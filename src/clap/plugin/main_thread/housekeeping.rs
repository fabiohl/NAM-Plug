// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Main thread housekeeping: GC drain, status-flags sync, pending model load, latency.

use super::NamClapMainThread;
use crate::clap::gui::dialog_state;
use crate::clap::plugin::shared::{
    ModelRequestOrigin, PendingModelRequest, SlimmableRebuild, StagedSwap,
};
use clack_extensions::preset_discovery::prelude::*;
use clack_plugin::host::HostMainThreadHandle;
use neural_amp_modeler_rs::common::spsc::{self, GcItem, drain_gc_channels};
use neural_amp_modeler_rs::models::slimmable::slice_wavenet_model;
use neural_amp_modeler_rs::models::{NamModel, StaticModel};
use std::sync::atomic::Ordering;

impl<'a> NamClapMainThread<'a> {
    /// GC drain, status flag mirroring, hugepage sync, pending model load, latency notification.
    ///
    /// Runs under `&self`: every mutation goes through the granular
    /// `Cell`/`RefCell` interior mutability, and every `RefCell` borrow is
    /// scoped to end **before** any external host call (`request_callback`,
    /// `request_restart`, extension methods) — no borrow survives a host call,
    /// so a reentrant host callback can never hit a `BorrowMutError`.
    pub(crate) fn housekeeping(&self) {
        let _scope = neural_amp_modeler_rs::common::diagnostics::scope_instance(
            self.shared.cold.instance_id,
        );

        // Backend GUI feedback: apply the "user closed the window" signal raised
        // by the Slint close callback on the GUI thread.
        self.reconcile_pending_gui_close();

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

        self.drain_gc_and_sync_flags();
        self.handle_slimmable_rebuild();
        self.handle_oversample_rebuild();
        self.handle_pending_model_load();
        self.handle_pending_ir_and_clear();
        self.sync_latency_and_telemetry();
    }

    /// Drain obsolete models to free memory outside RT and sync status flags.
    fn drain_gc_and_sync_flags(&self) {
        // During normal operation the RT parking lot is owned by the
        // processor and flushed back to this SPSC every audio cycle
        // (gc.rs::drain_parking_lot), so a main-thread-side empty lot is
        // correct here. The teardown handoff (`deactivate()` →
        // `drain_gc_final(&mut processor.parking_lot)`) covers the 16 slots
        // after the audio thread stops.
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
        if !self.hugepage_synced.get() {
            neural_amp_modeler_rs::dsp::mirror_buf::sync_huge_page_flag(
                &self.shared.cold.rt_status,
            );
            self.hugepage_synced.set(true);
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

        // Processor poisoning containment (S2-T1 / F-NP-02):
        // If the RT audio thread panicked, it transitioned to a poisoned silent state
        // and set RT_STATUS_PROCESSOR_POISONED. The main thread detects this flag,
        // logs an error, and requests a restart from the host (once per occurrence).
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
            // Processor has been reactivated cleanly; reset the latch
            self.poison_restart_requested.set(false);
        }
    }

    /// WaveNet slimmable rebuild: main thread performs all allocation,
    /// prewarm, and mmap outside the audio-thread callback.
    fn handle_slimmable_rebuild(&self) {
        if !self
            .shared
            .cold
            .rt_status
            .check_flag_acquire(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD)
        {
            return;
        }

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
                // The producer borrow ends before `request_callback()` below
                // (Err arm), so no borrow is held across the host call.
                match self
                    .slimmable_tx
                    .borrow_mut()
                    .push(Box::new(SlimmableRebuild { generation, model }))
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

    /// Oversampling engine rebuild: main thread performs all allocation
    /// (OversampleEngine::new creates filters and buffers) off the audio-thread.
    fn handle_oversample_rebuild(&self) {
        if !self
            .shared
            .cold
            .rt_status
            .check_flag_acquire(spsc::RT_STATUS_NEEDS_OS_REBUILD)
        {
            return;
        }

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
            // Producer borrow ends with the match scrutinee, before the
            // host calls in the arms (clear_flag / request_callback).
            match self.cmd_producer.borrow_mut().try_push_command(
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

    /// Propagates file dialog results and processes pending model loads.
    fn handle_pending_model_load(&self) {
        // Propagate dialog_state → pending_model_requests (R2: Arc-backed, UAF-safe)
        // Always propagates: Selected(path), Cancelled, or TimedOut sentinels.
        if let Some(dialog_state) = self.shared.cold.dialog_state.as_ref() {
            let mut dialog_guard = dialog_state.pending_model.lock().unwrap_or_else(|e| {
                log::error!("PoisonError in dialog_state.pending_model lock: {e:?}");
                e.into_inner()
            });
            if let Some(path) = dialog_guard.take() {
                let mut queue_guard = self
                    .shared
                    .cold
                    .pending_model_requests
                    .lock()
                    .unwrap_or_else(|e| {
                        log::error!("PoisonError in pending_model_requests lock: {e:?}");
                        e.into_inner()
                    });
                if queue_guard.len() < crate::clap::plugin::shared::MAX_PENDING_MODEL_REQUESTS {
                    queue_guard.push_back(PendingModelRequest {
                        path,
                        origin: ModelRequestOrigin::Gui,
                    });
                } else {
                    log::warn!(
                        "NAM-Plug: pending_model_requests queue full, dropping dialog result"
                    );
                }
            }
        }

        // Pop at most one request per housekeeping cycle.
        let (request, remaining_count) = {
            let mut queue_guard = self
                .shared
                .cold
                .pending_model_requests
                .lock()
                .unwrap_or_else(|e| {
                    log::error!("PoisonError in pending_model_requests lock: {e:?}");
                    e.into_inner()
                });
            let popped = queue_guard.pop_front();
            let count = queue_guard.len();
            (popped, count)
        };

        if let Some(req) = request {
            let cancelled_sentinel = dialog_state::dialog_cancelled_sentinel();
            let timedout_sentinel = dialog_state::dialog_timedout_sentinel();

            if req.path == cancelled_sentinel {
                log::info!("NAM-Plug: model file dialog cancelled by user");
                self.shared.cold.ui_loading.store(false, Ordering::Relaxed);
            } else if req.path == timedout_sentinel {
                log::info!("NAM-Plug: model file dialog timed out");
                self.shared.cold.ui_loading.store(false, Ordering::Relaxed);
            } else {
                let res = self.load_model(&req.path);
                self.shared.cold.ui_loading.store(false, Ordering::Relaxed);

                match res {
                    Ok(_) => {
                        if let ModelRequestOrigin::Preset { location, load_key } = req.origin {
                            notify_preset_loaded(&self.host, &location, load_key.as_deref());
                        }
                    }
                    Err(e) => {
                        let err_msg = if !e.user_hint().is_empty() {
                            format!("{}: {}", e.user_message(), e.user_hint())
                        } else if !e.user_message().is_empty() {
                            e.user_message().to_string()
                        } else {
                            e.error_code().message().to_string()
                        };
                        let mut msg_guard = self
                            .shared
                            .cold
                            .ui_load_error_msg
                            .lock()
                            .unwrap_or_else(|e| {
                                log::error!("PoisonError in ui_load_error_msg lock: {e:?}");
                                e.into_inner()
                            });
                        *msg_guard = err_msg.clone();
                        self.shared
                            .cold
                            .ui_load_error
                            .store(true, Ordering::Relaxed);

                        if let ModelRequestOrigin::Preset { location, load_key } = req.origin {
                            notify_preset_error(
                                &self.host,
                                &location,
                                load_key.as_deref(),
                                e.error_code() as i32,
                                &err_msg,
                            );
                        }

                        log::error!("Failed to load model: {e:?}");
                    }
                }
            }

            // If there are more requests pending in the queue, schedule the next
            // housekeeping cycle to drain sequentially across main thread ticks.
            if remaining_count > 0 {
                self.host.request_callback();
            }
        }
    }

    /// Propagates IR dialog results and processes pending IR loads / clears.
    fn handle_pending_ir_and_clear(&self) {
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
                // Scoped borrow: `staged_swap` is released before the host
                // `request_restart()` call below.
                {
                    let mut staged_swap = self.staged_swap.borrow_mut();
                    let staged = staged_swap.get_or_insert_with(StagedSwap::default);
                    staged.ir = Some(None);
                }
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

        if self.shared.cold.ui_clear_model.load(Ordering::Relaxed) {
            self.shared
                .cold
                .ui_clear_model
                .store(false, Ordering::Relaxed);
            if let Err(e) = self.clear_model() {
                log::error!("Failed to clear model from GUI: {e:?}");
            }
        }
    }

    /// Syncs latency changes with the CLAP host and records slimmable stale telemetry.
    fn sync_latency_and_telemetry(&self) {
        // Check if latency changed (stream resampler, oversample, or cabsim).
        // `rt_to_ui.current_latency` is the authoritative combined total (stream +
        // oversampling + cabsim) maintained by the RT processor in `activate()` and
        // updated by `recompute_effective_latency` on every resource swap.
        let current_latency = self.shared.rt_to_ui.current_latency.load(Ordering::Relaxed);

        if current_latency != self.last_reported_latency.get() {
            self.last_reported_latency.set(current_latency);
            log::info!(
                "[Housekeeping] Latency changed: {} samples reported to host.",
                current_latency
            );
            if let Some(latency_ext) = self
                .host
                .get_extension::<clack_extensions::latency::HostLatency>()
            {
                latency_ext.changed(&self.host);
            }
        }

        // Check if cabsim tail changed (for host tail reporting)
        let _cabsim_tail = self
            .shared
            .rt_to_ui
            .cabsim_tail_samples
            .load(Ordering::Relaxed);
        if _cabsim_tail != self.last_reported_cabsim_tail.get() {
            self.last_reported_cabsim_tail.set(_cabsim_tail);
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
    /// Called from `housekeeping()` (triggered periodically by the 250 ms
    /// CLAP watchdog timer or upon `host.request_callback()`).
    fn flush_in_flight_params(&self) {
        let snapshot = self
            .shared
            .cold
            .in_flight_params
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(params) = snapshot {
            // Scoped producer borrow: released before the poisoned-lock
            // recovery below (no host call happens here, but keeping the
            // borrow window minimal is the house rule).
            let flush_result = {
                let mut producer = self.cmd_producer.borrow_mut();
                producer.push_params(params);
                producer.force_flush()
            };
            match flush_result {
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
/// Reconstructs the `Location` and calls `HostPresetLoad::loaded()`.
fn notify_preset_loaded(
    host: &HostMainThreadHandle,
    location_path: &std::ffi::CStr,
    load_key: Option<&std::ffi::CStr>,
) {
    if let Some(preset_load) = host.get_extension::<HostPresetLoad>() {
        let location = Location::File {
            path: location_path,
        };
        preset_load.loaded(host, location, load_key);
        log::info!("Host notified: preset loaded successfully");
    }
}

/// Notify the host that a preset load failed.
fn notify_preset_error(
    host: &HostMainThreadHandle,
    location_path: &std::ffi::CStr,
    load_key: Option<&std::ffi::CStr>,
    os_error: i32,
    message: &str,
) {
    if let Some(preset_load) = host.get_extension::<HostPresetLoad>() {
        let location = Location::File {
            path: location_path,
        };
        let msg_cstr = std::ffi::CString::new(message);
        let msg_ref = msg_cstr.as_deref().ok();
        preset_load.on_error(host, location, load_key, os_error, msg_ref);
        log::error!("Host notified: preset load failed (os_error={os_error})");
    }
}
