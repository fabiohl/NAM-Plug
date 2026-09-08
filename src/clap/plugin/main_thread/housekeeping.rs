// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Main thread housekeeping: GC drain, status-flags sync, pending model load, latency.

use super::NamClapMainThread;
use crate::clap::gui::dialog_state;
use crate::clap::plugin::shared::{PendingPresetLoad, SlimmableRebuild, StagedSwap};
use clack_extensions::preset_discovery::prelude::*;
use clack_plugin::host::HostMainThreadHandle;
use neural_amp_modeler_rs::common::spsc::{self, GcItem, drain_gc_channels};
use neural_amp_modeler_rs::models::slimmable::slice_wavenet_model;
use neural_amp_modeler_rs::models::{NamModel, StaticModel};
use std::sync::atomic::Ordering;

impl<'a> NamClapMainThread<'a> {
    /// GC drain, status flag mirroring, hugepage sync, pending model load, latency notification.
    pub(crate) fn housekeeping(&mut self) {
        let _scope = neural_amp_modeler_rs::common::diagnostics::scope_instance(
            self.shared.cold.instance_id,
        );

        // Drain in-flight parameter snapshot queued by
        // PluginMainThreadParams::flush() when the SPSC was full.
        self.flush_in_flight_params();
        // Deliver/ack any pending restore transaction: pushes the atomic
        // RestoreTxn when the ring has room and publishes UI/paths/hashes only
        // once the audio thread confirms the generation.
        self.flush_pending_restore();
        // Flush any model deferred by load_model().
        // Primary mechanism is activate(), this is a fallback for hosts
        // that call state-load between activate() and the first process().
        // Errors set RT_STATUS_MODEL_LOAD_FAILED internally; housekeeping is void.
        let _ = self.flush_pending_model();

        // Drain obsolete models to free memory outside RT.
        // During normal operation the RT parking lot is owned by the
        // processor and flushed back to this SPSC every audio cycle
        // (gc.rs::drain_parking_lot), so a main-thread-side empty lot is
        // correct here. The teardown handoff (`deactivate()` →
        // `drain_gc_final(&mut processor.parking_lot)`) covers the 16 slots
        // after the audio thread stops.
        let mut rt_parking_lot: [Option<GcItem>; 16] = Default::default();
        let drained = drain_gc_channels(
            &mut self.gc_rx,
            &self.shared.cold.gc_overflow,
            &mut rt_parking_lot,
            &self.shared.cold.rt_status,
        );
        self.shared
            .cold
            .rt_status
            .drains
            .fetch_add(drained as u32, Ordering::Relaxed);

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

        // Sync huge page status from mirror buffer (one-shot per instance).
        if !self.hugepage_synced {
            neural_amp_modeler_rs::dsp::mirror_buf::sync_huge_page_flag(
                &self.shared.cold.rt_status,
            );
            self.hugepage_synced = true;
        }

        // Slimmable reset failure: RT thread sets flag, main thread emits log.
        if self
            .shared
            .cold
            .rt_status
            .check_and_clear_flag(spsc::RT_STATUS_SLIMMABLE_RESET_FAILED)
        {
            log::error!("ContainerModel submodel reset failed — model may run in previous state.");
        }

        // WaveNet slimmable rebuild: main thread performs all allocation,
        // prewarm, and mmap outside the audio-thread callback.
        if self
            .shared
            .cold
            .rt_status
            .check_flag_acquire(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD)
        {
            let target_ch = self
                .shared
                .cold
                .rt_status
                .requested_slimmable_ch
                .load(Ordering::Relaxed) as usize;
            let generation = self
                .shared
                .cold
                .requested_slimmable_generation
                .load(Ordering::Relaxed);
            let buffer_size = self.shared.cold.buffer_size.load(Ordering::Relaxed) as usize;

            if target_ch >= 4 {
                let new_model = {
                    let storage = self
                        .shared
                        .cold
                        .full_wavenet_model
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    storage.as_ref().and_then(|m| {
                        if let StaticModel::WavenetDyn(w) = m.as_ref() {
                            match slice_wavenet_model(w, target_ch) {
                                Ok(mut slimmed) => {
                                    slimmed.prewarm();
                                    if buffer_size > 0 {
                                        let _ = slimmed.set_max_buffer_size(buffer_size);
                                    }
                                    Some(Box::new(StaticModel::WavenetDyn(Box::new(slimmed))))
                                }
                                Err(_) => {
                                    self.shared
                                        .cold
                                        .rt_status
                                        .set_flag(spsc::RT_STATUS_SLIMMABLE_SLICE_FAILED);
                                    None
                                }
                            }
                        } else {
                            None
                        }
                    })
                };

                if let Some(model) = new_model {
                    match self
                        .slimmable_tx
                        .push(SlimmableRebuild { generation, model })
                    {
                        Ok(()) => {
                            self.shared
                                .cold
                                .rt_status
                                .clear_flag_release(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD);
                        }
                        Err(rtrb::PushError::Full(_)) => {
                            // Keep NEEDS_SLIMMABLE_REBUILD so the FSM
                            // retries on the next cycle instead of silently
                            // dropping the slimmed model and locking quality.
                            log::warn!("NAM-Plug: slimmable channel full — rebuild will retry");
                            self.host.request_callback();
                        }
                    }
                } else {
                    self.shared
                        .cold
                        .rt_status
                        .clear_flag_release(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD);
                }
            } else {
                // target_ch < 4: no slimmable rebuild needed — clear the flag
                // so the FSM does not retry a no-op.
                self.shared
                    .cold
                    .rt_status
                    .clear_flag_release(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD);
            }
        }

        // Oversampling engine rebuild: main thread performs all allocation
        // (OversampleEngine::new creates filters and buffers) off the audio-thread.
        if self
            .shared
            .cold
            .rt_status
            .check_flag_acquire(spsc::RT_STATUS_NEEDS_OS_REBUILD)
        {
            use neural_amp_modeler_rs::dsp::oversample::{OversampleEngine, OversampleFactor};
            use neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF;

            let factor_val = self
                .shared
                .cold
                .rt_status
                .requested_os_factor
                .load(Ordering::Relaxed);
            let factor = OversampleFactor::from_f32(factor_val as f32);
            if let (Ok(l), Ok(r)) = (
                OversampleEngine::new(factor, MAX_RESAMP_BUF),
                OversampleEngine::new(factor, MAX_RESAMP_BUF),
            ) {
                match self.cmd_producer.try_push_command(
                    crate::clap::plugin::ClapParamPayload::SetOversample {
                        os_l: Box::new(l),
                        os_r: Box::new(r),
                    },
                ) {
                    Ok(_) => {
                        self.shared
                            .cold
                            .rt_status
                            .clear_flag_release(spsc::RT_STATUS_NEEDS_OS_REBUILD);
                    }
                    Err((crate::clap::plugin::command_scheduler::PushError::Full, _payload)) => {
                        log::warn!(
                            "NAM-Plug: Command ring full during SetOversample; retaining flag for retry"
                        );
                        self.host.request_callback();
                    }
                }
            } else {
                neural_amp_modeler_rs::common::diagnostics::NamDiagnostic::new(
                    neural_amp_modeler_rs::common::diagnostics::NamErrorCode::OutOfMemory,
                    &self.sys,
                )
                .message("Failed to rebuild oversample engine (OOM).")
                .hint("The current oversampling state will be preserved.")
                .emit_warning();
                self.shared
                    .cold
                    .rt_status
                    .clear_flag_release(spsc::RT_STATUS_NEEDS_OS_REBUILD);
            }
        }

        // Propagate dialog_state → ui_pending_model (R2: Arc-backed, UAF-safe)
        // Always propagates: Selected(path), Cancelled, or TimedOut sentinels.
        if let Some(dialog_state) = self.shared.cold.dialog_state.as_ref() {
            let mut dialog_guard = dialog_state.pending_model.lock().unwrap_or_else(|e| {
                log::error!("PoisonError in dialog_state.pending_model lock: {e:?}");
                e.into_inner()
            });
            if let Some(path) = dialog_guard.take() {
                let mut ui_guard = self
                    .shared
                    .cold
                    .ui_pending_model
                    .lock()
                    .unwrap_or_else(|e| {
                        log::error!("PoisonError in ui_pending_model lock: {e:?}");
                        e.into_inner()
                    });
                *ui_guard = Some(path);
            }
        }

        // Check if there is a pending model sent by the UI (real path) or dialog (sentinel).
        let pending = {
            let mut pending_guard = self
                .shared
                .cold
                .ui_pending_model
                .lock()
                .unwrap_or_else(|e| {
                    log::error!("PoisonError in ui_pending_model lock: {e:?}");
                    e.into_inner()
                });
            pending_guard.take()
        };

        if let Some(path) = pending {
            let cancelled_sentinel = dialog_state::dialog_cancelled_sentinel();
            let timedout_sentinel = dialog_state::dialog_timedout_sentinel();

            if path == cancelled_sentinel {
                log::info!("NAM-Plug: model file dialog cancelled by user");
                self.shared.cold.ui_loading.store(false, Ordering::Relaxed);
            } else if path == timedout_sentinel {
                log::info!("NAM-Plug: model file dialog timed out");
                self.shared.cold.ui_loading.store(false, Ordering::Relaxed);
            } else {
                let res = self.load_model(&path);
                self.shared.cold.ui_loading.store(false, Ordering::Relaxed);

                // Notify host via HostPresetLoad if this model was
                // queued by the preset-load extension.
                let pending_load = self
                    .shared
                    .cold
                    .pending_preset_load
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .pop_front();

                match res {
                    Ok(_) => {
                        if let Some(pending) = pending_load {
                            notify_preset_loaded(&mut self.host, pending);
                        }
                    }
                    Err(e) => {
                        let err_msg = e.error_code().message();
                        let mut msg_guard = self
                            .shared
                            .cold
                            .ui_load_error_msg
                            .lock()
                            .unwrap_or_else(|e| {
                                log::error!("PoisonError in ui_load_error_msg lock: {e:?}");
                                e.into_inner()
                            });
                        *msg_guard = err_msg.to_string();
                        self.shared
                            .cold
                            .ui_load_error
                            .store(true, Ordering::Relaxed);

                        if let Some(pending) = pending_load {
                            notify_preset_error(
                                &mut self.host,
                                pending,
                                e.error_code() as i32,
                                err_msg,
                            );
                        }

                        log::error!("Failed to load model from GUI: {e:?}");
                    }
                }
            }
        }

        // Check if there is a pending IR sent by the UI
        {
            // Propagate ir_dialog_state → ui_pending_ir (R2: Arc-backed, UAF-safe)
            if let Some(ir_dialog_state) = self.shared.cold.ir_dialog_state.as_ref() {
                let mut dialog_guard = ir_dialog_state.pending_ir.lock().unwrap_or_else(|e| {
                    log::error!("PoisonError in ir_dialog_state.pending_ir lock: {e:?}");
                    e.into_inner()
                });
                if let Some(path) = dialog_guard.take() {
                    let mut ui_guard = self.shared.cold.ui_pending_ir.lock().unwrap_or_else(|e| {
                        log::error!("PoisonError in ui_pending_ir lock: {e:?}");
                        e.into_inner()
                    });
                    *ui_guard = Some(path);
                }
            }

            let pending_ir = self
                .shared
                .cold
                .ui_pending_ir
                .lock()
                .unwrap_or_else(|e| {
                    log::error!("PoisonError in ui_pending_ir lock: {e:?}");
                    e.into_inner()
                })
                .take();
            if let Some(path) = pending_ir {
                let cancelled_sentinel = dialog_state::dialog_cancelled_sentinel();
                let timedout_sentinel = dialog_state::dialog_timedout_sentinel();

                if path == cancelled_sentinel {
                    log::info!("NAM-Plug: IR file dialog cancelled by user");
                    self.shared
                        .cold
                        .ui_ir_loading
                        .store(false, Ordering::Relaxed);
                } else if path == timedout_sentinel {
                    log::info!("NAM-Plug: IR file dialog timed out");
                    self.shared
                        .cold
                        .ui_ir_loading
                        .store(false, Ordering::Relaxed);
                } else {
                    let res = self.load_cabsim(&path);
                    self.shared
                        .cold
                        .ui_ir_loading
                        .store(false, Ordering::Relaxed);
                    match res {
                        Ok(_) => {}
                        Err(e) => {
                            let err_msg = e.error_code().message();
                            let mut msg_guard = self
                                .shared
                                .cold
                                .ui_ir_load_error_msg
                                .lock()
                                .unwrap_or_else(|e| {
                                    log::error!("PoisonError in ui_ir_load_error_msg lock: {e:?}");
                                    e.into_inner()
                                });
                            *msg_guard = err_msg.to_string();
                            self.shared
                                .cold
                                .ui_ir_load_error
                                .store(true, Ordering::Relaxed);

                            log::error!("Failed to load cab-sim IR from GUI: {e:?}");
                        }
                    }
                }
            }

            if self.shared.cold.ui_clear_ir.load(Ordering::Relaxed) {
                // Strict Restart Policy: clearing the active IR changes
                // the physical latency (partition → 0), so it is staged and a
                // host restart is requested — the DSP keeps the IR (and the
                // still-reported latency) until `activate()` installs the
                // staged clear. A clear when no IR is installed is a no-op.
                let current_cabsim_latency = self
                    .shared
                    .cold
                    .current_cabsim_latency
                    .load(Ordering::Relaxed);
                if current_cabsim_latency == 0 {
                    self.shared.cold.ui_clear_ir.store(false, Ordering::Relaxed);
                } else {
                    let staged = self.staged_swap.get_or_insert_with(StagedSwap::default);
                    staged.ir = Some(None);
                    self.host.request_restart();
                    self.shared.cold.ui_clear_ir.store(false, Ordering::Relaxed);
                    {
                        let mut ir_guard = self.shared.cold.ir_path.lock().unwrap_or_else(|e| {
                            log::error!("PoisonError in ir_path lock: {e:?}");
                            e.into_inner()
                        });
                        *ir_guard = None;
                    }
                    {
                        let mut hash_guard = self.shared.cold.ir_hash.lock().unwrap_or_else(|e| {
                            log::error!("PoisonError in ir_hash lock: {e:?}");
                            e.into_inner()
                        });
                        *hash_guard = None;
                    }
                    {
                        let mut raw_guard =
                            self.shared.cold.ir_raw_samples.lock().unwrap_or_else(|e| {
                                log::error!("PoisonError in ir_raw_samples lock: {e:?}");
                                e.into_inner()
                            });
                        *raw_guard = None;
                        self.shared
                            .cold
                            .ir_raw_sample_rate
                            .store(0, Ordering::Relaxed);
                    }
                    log::info!("NAM-Plug: cab-sim IR cleared via GUI (staged for host restart)");
                }
            }
        }

        // Check if latency changed (stream resampler, oversample, or cabsim).
        // `rt_to_ui.current_latency` is the authoritative combined total (stream +
        // oversampling + cabsim) maintained by the RT processor in `activate()` and
        // updated by `recompute_effective_latency` on every resource swap.
        // The former per-component reads (including the now-removed `os_latency` field
        // from `RtToUi`) have been replaced by this single unified atomic read.
        let current_latency = self.shared.rt_to_ui.current_latency.load(Ordering::Relaxed);

        if current_latency != self.last_reported_latency {
            self.last_reported_latency = current_latency;
            log::info!(
                "[Housekeeping] Latency changed: {} samples reported to host.",
                current_latency
            );
            if let Some(latency_ext) = self
                .host
                .get_extension::<clack_extensions::latency::HostLatency>()
            {
                latency_ext.changed(&mut self.host);
            }
        }

        // Check if cabsim tail changed (for host tail reporting)
        let _cabsim_tail = self
            .shared
            .rt_to_ui
            .cabsim_tail_samples
            .load(Ordering::Relaxed);
        if _cabsim_tail != self.last_reported_cabsim_tail {
            self.last_reported_cabsim_tail = _cabsim_tail;
        }

        // Observability: surface stale slimmable-rebuild
        // discards exactly once when the RT counter advances. Without this, a
        // regression that discards legitimate rebuilds (adaptive-compute
        // starvation) would be invisible in field telemetry.
        let stale_discarded = self
            .shared
            .cold
            .slimmable_stale_discarded_total
            .load(Ordering::Relaxed);
        if stale_discarded != self.last_seen_slimmable_stale {
            self.last_seen_slimmable_stale = stale_discarded;
            log::warn!(
                "NAM-Plug: {} stale slimmable rebuild(s) discarded",
                stale_discarded
            );
        }
    }

    /// Retries delivery of parameter snapshots queued by
    /// `PluginMainThreadParams::flush()` when the SPSC channel was full.
    /// Called from `housekeeping()` (triggered by `host.request_callback()`).
    fn flush_in_flight_params(&mut self) {
        let snapshot = self
            .shared
            .cold
            .in_flight_params
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(params) = snapshot {
            self.cmd_producer.push_params(params);
            match self.cmd_producer.force_flush() {
                Ok(_) => {
                    log::trace!("In-flight params delivered on retry");
                }
                Err(crate::clap::plugin::command_scheduler::PushError::Full) => {
                    let mut guard = self
                        .shared
                        .cold
                        .in_flight_params
                        .lock()
                        .unwrap_or_else(|e| {
                            log::error!("in_flight_params mutex poisoned during flush, recovering");
                            e.into_inner()
                        });
                    *guard = Some(params);
                    self.shared.bump_generation();
                }
            }
        }
    }
}

/// Notify the host that a preset was loaded successfully.
/// Reconstructs the `Location` from the stored `PendingPresetLoad` and
/// calls `HostPresetLoad::loaded()`.
fn notify_preset_loaded(host: &mut HostMainThreadHandle, pending: PendingPresetLoad) {
    let path_cstr = pending.location_path;
    let load_key_cstr = pending.load_key;
    if let Some(preset_load) = host.get_extension::<HostPresetLoad>() {
        let location = Location::File { path: &path_cstr };
        let load_key = load_key_cstr.as_deref();
        preset_load.loaded(host, location, load_key);
        log::info!("Host notified: preset loaded successfully");
    }
}

/// Notify the host that a preset load failed.
fn notify_preset_error(
    host: &mut HostMainThreadHandle,
    pending: PendingPresetLoad,
    os_error: i32,
    message: &str,
) {
    let path_cstr = pending.location_path;
    let load_key_cstr = pending.load_key;
    if let Some(preset_load) = host.get_extension::<HostPresetLoad>() {
        let location = Location::File { path: &path_cstr };
        let load_key = load_key_cstr.as_deref();
        let msg_cstr = std::ffi::CString::new(message);
        let msg_ref = msg_cstr.as_deref().ok();
        preset_load.on_error(host, location, load_key, os_error, msg_ref);
        log::error!("Host notified: preset load failed (os_error={os_error})");
    }
}
