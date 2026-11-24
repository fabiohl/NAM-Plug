// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Main thread housekeeping: GC drain, status-flags sync, pending model load, latency.

mod pending_loads;
mod rebuilds;

use super::NamClapMainThread;
use crate::clap::plugin::lock::lock_cold;
use crate::clap::plugin::shared::{
    UI_ALERT_CABSIM_CONTRACT, UI_ALERT_DEGRADE_MINIMAL, UI_ALERT_DEGRADE_REDUCED,
    UI_ALERT_HOST_CONTRACT, UI_ALERT_NON_FINITE_INPUT, UI_ALERT_POISONED,
    UI_ALERT_RESAMPLER_FAILED,
};
use neural_amp_modeler_rs::common::spsc::{self, GcItem, drain_gc_channels};
use std::sync::atomic::Ordering;

impl<'a> NamClapMainThread<'a> {
    /// GC drain, status flag mirroring, hugepage sync, pending model load, latency notification.
    ///
    /// Runs under `&self`: every mutation goes through the granular
    /// `Cell`/`RefCell` interior mutability, and every `RefCell` borrow is
    /// scoped to end **before** any external host call.
    pub(crate) fn housekeeping(&self) {
        let _scope = neural_amp_modeler_rs::common::diagnostics::scope_instance(
            self.shared.cold.instance_id,
        );

        self.reconcile_pending_gui_close();
        self.flush_in_flight_params();
        self.flush_pending_restore();

        if let Err(e) = self.flush_pending_model() {
            log::debug!("housekeeping: deferred model flush failed: {e:?}");
        }

        self.drain_gc_and_sync_flags();
        self.handle_slimmable_rebuild();
        self.handle_oversample_rebuild();
        self.handle_cab_nam_rebuild();
        self.sync_watchdog_cadence();
        self.handle_pending_model_load();
        self.handle_pending_ir_and_clear();
        self.sync_latency_and_telemetry();
        self.update_alert_bits();
    }

    /// Drain obsolete models to free memory outside RT and sync status flags.
    fn drain_gc_and_sync_flags(&self) {
        let mut rt_parking_lot: [Option<GcItem>; 16] = Default::default();
        let drained = {
            let mut gc_rx = self.gc_rx.borrow_mut();
            drain_gc_channels(
                &mut gc_rx,
                &self.shared.cold.gc_overflow,
                &mut rt_parking_lot,
                &self.shared.cold.rt_status,
            )
        };
        self.shared
            .cold
            .rt_status
            .drains
            .fetch_add(drained as u32, Ordering::Relaxed);
        let mut empty_box_lot: [Option<crate::clap::plugin::retired_box::RetiredBox>; 16] =
            Default::default();
        self.drain_retired_boxes_final(&mut empty_box_lot);

        let current_bits = self
            .shared
            .cold
            .rt_status
            .status_bits
            .load(Ordering::Relaxed);
        self.shared
            .cold
            .rt_status
            .flags_seen
            .fetch_or(current_bits, Ordering::Relaxed);

        if !self.hugepage_synced.get() {
            neural_amp_modeler_rs::dsp::mirror_buf::sync_huge_page_flag(
                &self.shared.cold.rt_status,
            );
            self.hugepage_synced.set(true);
        }

        if self
            .shared
            .cold
            .rt_status
            .check_and_clear_flag(spsc::RT_STATUS_SLIMMABLE_RESET_FAILED)
        {
            log::error!("ContainerModel submodel reset failed — model may run in previous state.");
        }

        if self
            .shared
            .cold
            .rt_status
            .check_flag(spsc::RT_STATUS_PROCESSOR_POISONED)
        {
            if !self.poison_restart_requested.get() {
                self.poison_restart_requested.set(true);
                log::error!(
                    "NAM-Plug: Audio processor is poisoned due to a fatal panic in RT callback. Requesting host restart..."
                );
                self.host.request_restart();
            }
        } else if self.poison_restart_requested.get() {
            self.poison_restart_requested.set(false);
        }
    }

    /// Aggregates RT status flags into cold-path alert bitmask.
    pub(crate) fn update_alert_bits(&self) {
        let epoch = self
            .shared
            .rt_to_ui
            .non_finite_epoch
            .load(Ordering::Relaxed);
        let flag_non_finite = self
            .shared
            .cold
            .rt_status
            .check_flag(spsc::RT_STATUS_NON_FINITE_INPUT_DETECTED);
        let now = std::time::Instant::now();

        if epoch != self.last_seen_non_finite_epoch.get() || flag_non_finite {
            self.last_seen_non_finite_epoch.set(epoch);
            self.non_finite_hold_until
                .set(Some(now + std::time::Duration::from_secs(3)));
        }

        let non_finite_active = match self.non_finite_hold_until.get() {
            Some(deadline) => now < deadline,
            None => false,
        };

        let rt = &self.shared.cold.rt_status;
        let mut bits = 0u32;
        let flag_map = [
            (spsc::RT_STATUS_PROCESSOR_POISONED, UI_ALERT_POISONED),
            (
                spsc::RT_STATUS_HOST_CONTRACT_VIOLATION,
                UI_ALERT_HOST_CONTRACT,
            ),
            (
                spsc::RT_STATUS_RESAMPLER_REBUILD_FAILED,
                UI_ALERT_RESAMPLER_FAILED,
            ),
            (
                spsc::RT_STATUS_CABSIM_CONTRACT_VIOLATION,
                UI_ALERT_CABSIM_CONTRACT,
            ),
            (spsc::RT_STATUS_DEGRADE_MINIMAL, UI_ALERT_DEGRADE_MINIMAL),
            (spsc::RT_STATUS_DEGRADE_REDUCED, UI_ALERT_DEGRADE_REDUCED),
        ];
        for (rt_flag, ui_bit) in flag_map {
            if rt.check_flag(rt_flag) {
                bits |= ui_bit;
            }
        }
        if non_finite_active {
            bits |= UI_ALERT_NON_FINITE_INPUT;
        }

        self.shared
            .cold
            .ui_alert_bits
            .store(bits, Ordering::Release);
    }

    /// Adaptive-rebuild watchdog fallback.
    pub(crate) fn sync_watchdog_cadence(&self) {
        let rt = &self.shared.cold.rt_status;
        let pending = self.shared.cold.pending_slimmable_request() != 0
            || rt.check_flag(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD)
            || rt.check_flag(spsc::RT_STATUS_NEEDS_OS_REBUILD)
            || self.shared.cold.is_cab_nam_rebuild_pending();
        let want = if pending {
            super::WATCHDOG_FAST_PERIOD_MS
        } else {
            super::WATCHDOG_PERIOD_MS
        };
        if self.watchdog_period_ms.get() != want {
            self.reprogram_watchdog(want);
        }
    }

    fn reprogram_watchdog(&self, period_ms: u32) {
        let Some(timer_ext) = self
            .host
            .get_extension::<clack_extensions::timer::HostTimer>()
        else {
            self.watchdog_period_ms.set(period_ms);
            return;
        };
        match timer_ext.register_timer(&self.host, period_ms) {
            Ok(new_id) => {
                if let Some(old_id) = self.watchdog_timer.replace(Some(new_id)) {
                    let _ = timer_ext.unregister_timer(&self.host, old_id);
                }
                self.watchdog_period_ms.set(period_ms);
                log::debug!("NAM-Plug: watchdog timer re-armed at {period_ms} ms");
            }
            Err(err) => {
                log::warn!(
                    "NAM-Plug: failed to re-arm watchdog timer at {period_ms} ms ({err}); \
                     keeping the previous cadence"
                );
            }
        }
    }

    /// Syncs latency changes with the CLAP host (strictly restart-gated) and
    /// records slimmable stale telemetry.
    fn sync_latency_and_telemetry(&self) {
        let current_latency = self.shared.rt_to_ui.current_latency.load(Ordering::Relaxed);

        if current_latency != self.last_reported_latency.get()
            && self.is_active.get()
            && self.latency_restart_requested_for.get() != Some(current_latency)
        {
            self.latency_restart_requested_for
                .set(Some(current_latency));
            log::info!(
                "[Housekeeping] Active latency diverged ({} -> {} samples); requesting host restart per CLAP contract.",
                self.last_reported_latency.get(),
                current_latency
            );
            self.host.request_restart();
        }

        let cabsim_tail = self
            .shared
            .rt_to_ui
            .cabsim_tail_samples
            .load(Ordering::Relaxed);
        if cabsim_tail != self.last_reported_cabsim_tail.get() {
            self.last_reported_cabsim_tail.set(cabsim_tail);
        }

        let stale_discarded = self
            .shared
            .cold
            .slimmable_stale_discarded_total
            .load(Ordering::Relaxed);
        if stale_discarded != self.last_seen_slimmable_stale.get() {
            self.last_seen_slimmable_stale.set(stale_discarded);
            log::warn!(
                "NAM-Plug: {} stale slimmable rebuild(s) discarded",
                stale_discarded
            );
        }
    }

    /// Retries delivery of parameter snapshots queued by
    /// `PluginMainThreadParams::flush()` when the SPSC channel was full.
    fn flush_in_flight_params(&self) {
        let snapshot = lock_cold(&self.shared.cold.in_flight_params, "in_flight_params").take();
        let cal_snapshot = lock_cold(
            &self.shared.cold.in_flight_calibration,
            "in_flight_calibration",
        )
        .take();
        let gate_mode_snapshot =
            lock_cold(&self.shared.cold.in_flight_gate_mode, "in_flight_gate_mode").take();
        let cab_bypass_snapshot = lock_cold(
            &self.shared.cold.in_flight_cab_bypass,
            "in_flight_cab_bypass",
        )
        .take();
        if snapshot.is_some()
            || cal_snapshot.is_some()
            || gate_mode_snapshot.is_some()
            || cab_bypass_snapshot.is_some()
        {
            let flush_result = {
                let mut producer = self.cmd_producer.borrow_mut();
                if let Some(params) = snapshot {
                    producer.push_params(params);
                }
                if let Some(cal) = cal_snapshot {
                    producer.push_calibration(cal);
                }
                if let Some(mode) = gate_mode_snapshot {
                    producer.push_gate_mode(mode);
                }
                if let Some(bypass) = cab_bypass_snapshot {
                    producer.push_cab_bypass(bypass);
                }
                producer.force_flush()
            };
            match flush_result {
                Ok(_) => {
                    log::trace!(
                        "In-flight params/calibration/gate_mode/cab_bypass delivered on retry"
                    );
                }
                Err(crate::clap::plugin::command_scheduler::PushError::Full) => {
                    if let Some(params) = snapshot {
                        *lock_cold(&self.shared.cold.in_flight_params, "in_flight_params") =
                            Some(params);
                    }
                    if let Some(cal) = cal_snapshot {
                        *lock_cold(
                            &self.shared.cold.in_flight_calibration,
                            "in_flight_calibration",
                        ) = Some(cal);
                    }
                    if let Some(mode) = gate_mode_snapshot {
                        *lock_cold(&self.shared.cold.in_flight_gate_mode, "in_flight_gate_mode") =
                            Some(mode);
                    }
                    if let Some(bypass) = cab_bypass_snapshot {
                        *lock_cold(
                            &self.shared.cold.in_flight_cab_bypass,
                            "in_flight_cab_bypass",
                        ) = Some(bypass);
                    }
                    self.shared.bump_generation();
                }
            }
        }
    }
}

#[cfg(test)]
impl<'a> NamClapMainThread<'a> {
    /// Runs exactly one main-thread slimmable drain on the calling thread.
    pub(crate) fn test_drain_slimmable_once(&self) {
        self.drain_slimmable_rebuild();
    }
}
