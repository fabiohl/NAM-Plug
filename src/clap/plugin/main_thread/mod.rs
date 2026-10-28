// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Main thread exclusive state (model loading, state save/load).
//!
//! Split into concern-specific sub-modules:
//! - `housekeeping` — GC drain, status flags, hugepage sync, pending model load, latency
//! - `logging` — RT→main transient event logging via atomic flags
//! - `load` — model loading + error_code mapping

mod housekeeping;
pub(crate) mod load;
mod logging;
pub(crate) mod model_loader;

use super::command_scheduler::{CommandProducer, PushError};
use super::errors::{self, static_plugin_error};
use super::lock::lock_cold;
use super::shared::{
    ClapParamPayload, NamClapShared, PendingModel, PendingRestore, SlimmableRebuild, StagedRestore,
    StagedSwap,
};
use crate::clap::gui::lifecycle::{GuiEvent, GuiLifecycle};
use clack_plugin::prelude::*;
use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
use neural_amp_modeler_rs::common::params::ProcessingParams;
use neural_amp_modeler_rs::common::spsc::{self, GcItem};
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::models::NamModel;
use rtrb::{Consumer, Producer};
use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Nominal main-thread watchdog period (ms): RT poison recovery, latency/PDC
/// sync and the Tier-1 GC ring drain.
pub(crate) const WATCHDOG_PERIOD_MS: u32 = 250;
/// Accelerated watchdog period (ms) armed while a structural rebuild
/// flag (`RT_STATUS_NEEDS_SLIMMABLE_REBUILD` / `RT_STATUS_NEEDS_OS_REBUILD`) is
/// still pending, so the retry cadence shortens on hosts where the worker's
/// `request_callback` is ineffective. Restored to [`WATCHDOG_PERIOD_MS`] once the
/// flag clears (no idle-CPU regression).
pub(crate) const WATCHDOG_FAST_PERIOD_MS: u32 = 50;

/// Main thread exclusive state (model loading, state save/load).
///
/// # Interior mutability model ("Embrace Reentrancy")
///
/// Every mutable field is exposed through granular `Cell`/`RefCell` interior
/// mutability, so the whole type can be reached through `&self`. The granular
/// domains guarantee that a reentrant host callback (e.g. `PluginParams::get_value`
/// or `PluginLatencyImpl::get` while a model is being loaded) never collides
/// with the mutation of the SPSC producers or the GUI window/worker slots.
///
/// Borrow discipline: `RefCell` borrows are strictly scoped and always dropped
/// **before** any external host call (`request_callback`, `request_restart`,
/// extension calls). No borrow is ever held across a host call, which rules out
/// reentrant `BorrowMutError` panics by construction.
///
/// None of these fields are ever touched from the audio thread; the RT-safety
/// and `#[repr(align(128))]` cache isolation of `NamClapShared` are unaffected.
pub struct NamClapMainThread<'a> {
    pub(crate) shared: &'a NamClapShared,
    /// Current parameters known by the main thread (mirror of the audio thread params).
    pub params: RefCell<ProcessingParams>,
    /// Host handle for notifications (latency_changed, state, etc.).
    ///
    /// `HostMainThreadHandle` is `Copy` in clack 0.2.0; plugin-to-host calls
    /// take `&self.host` directly (or a local copy when the callee still
    /// wants `&mut`, e.g. `HostTrackInfo::get`).
    pub host: HostMainThreadHandle<'a>,
    /// System snapshot for emitting diagnostics.
    pub sys: SystemSnapshot,
    /// Producer to send updates to the audio thread with coalescing and ack.
    pub cmd_producer: RefCell<CommandProducer<'a>>,
    /// Consumer to collect garbage (obsolete models) from the audio thread.
    pub gc_rx: RefCell<Consumer<GcItem>>,
    /// Producer to send slimmable-rebuilt models to the audio thread.
    /// Boxed end-to-end so the engine `SwapRing` surface applies with zero
    /// allocation on the audio thread (boxes created only on the main thread).
    pub slimmable_tx: RefCell<Producer<Box<SlimmableRebuild>>>,
    /// Cached last latency reported to the host to avoid redundant notifications.
    pub last_reported_latency: Cell<u32>,
    /// Cached last CabSim tail length reported to the host to avoid redundant notifications.
    pub last_reported_cabsim_tail: Cell<u32>,
    /// Last-seen value of `ColdShared::slimmable_stale_discarded_total`, used to
    /// log stale-slimmable-rebuild discards exactly once.
    pub last_seen_slimmable_stale: Cell<u32>,
    /// Last-seen count of `rt_status.dsp_overloads` logged to host/warnings.
    pub last_seen_overloads: Cell<u32>,
    /// Instant of the last rate-limited overload warning log emission.
    pub last_overload_log_instant: Cell<Option<std::time::Instant>>,
    /// Last-seen `RtToUi::non_finite_epoch` to detect new non-finite input events.
    pub(crate) last_seen_non_finite_epoch: Cell<u32>,
    /// Instant until which the non-finite input alert should be held active (3s hold).
    pub(crate) non_finite_hold_until: Cell<Option<std::time::Instant>>,
    /// Last-seen alert bits to emit host logs only on 0 -> 1 transitions.
    pub(crate) last_seen_alert_bits: Cell<u32>,
    /// Slint window handle for GUI lifecycle control.
    pub slint_window: RefCell<Option<slint::Weak<crate::clap::gui::MainWindow>>>,
    /// Persistent per-instance GUI worker thread (owns the Slint platform and
    /// event loop for the instance lifetime; survives `gui.destroy()` so
    /// repeated create/destroy cycles keep working — Slint 1.18 pins the
    /// platform to the thread that first initialized it).
    pub(crate) gui_worker: RefCell<Option<crate::clap::gui::worker::GuiWorker>>,
    /// Handle for the model file-dialog background thread (if active). Joined during teardown.
    #[expect(dead_code, reason = "held for join on teardown, never read directly")]
    pub(crate) dialog_handle: RefCell<Option<std::thread::JoinHandle<()>>>,
    /// Shared state synchronized with the model file-dialog background thread.
    #[expect(dead_code, reason = "held for Arc lifecycle, never read directly")]
    pub(crate) dialog_state: Option<Arc<crate::clap::gui::dialog_state::DialogSharedState>>,
    /// Handle for the IR file-dialog background thread (if active). Joined during teardown.
    #[expect(dead_code, reason = "held for join on teardown, never read directly")]
    pub(crate) ir_dialog_handle: RefCell<Option<std::thread::JoinHandle<()>>>,
    /// Shared state synchronized with the IR file-dialog background thread.
    #[expect(dead_code, reason = "held for Arc lifecycle, never read directly")]
    pub(crate) ir_dialog_state: Option<Arc<crate::clap::gui::dialog_state::IrDialogSharedState>>,
    /// Finite state machine tracking the GUI window lifecycle
    /// (Hidden → ShowRequested → Active → HideRequested → Destroyed).
    pub(crate) gui_lifecycle: Cell<GuiLifecycle>,
    /// Flag indicating whether hugepage status has been synced for this instance.
    pub(crate) hugepage_synced: Cell<bool>,
    /// A validated restore staged on the main thread awaiting atomic delivery
    /// to the audio thread and ack-gated publication. Private slot —
    /// never shared with the audio thread or GUI.
    pub(crate) pending_restore: RefCell<Option<PendingRestore>>,
    /// Latency-affecting full state restore staged to land only on the next
    /// host restart cycle (Strict Restart Policy).
    ///
    /// Set by `atomic_commit()` when the restore package changes physical
    /// stream latency, physical cabsim latency, or oversampling factor.
    /// Parked here until `activate()` installs the entire package atomically.
    pub(crate) staged_restore: RefCell<Option<StagedRestore>>,
    /// Latency-affecting model/IR swap staged to land only on the next host
    /// restart cycle (Strict Restart Policy).
    ///
    /// Set by `load_model()`/`load_cabsim()`/IR-clear when the swap would
    /// change the physical latency: the resources are fully built off-RT here,
    /// `host.request_restart()` is issued, and `activate()` consumes this slot
    /// during the restart cycle — the DSP keeps the old, still-reported
    /// latency until then. Same-latency swaps bypass this slot and apply
    /// continuously through the SPSC. Latest-wins coalescing per component.
    pub(crate) staged_swap: RefCell<Option<StagedSwap>>,
    /// Flag indicating whether a restart request has already been dispatched to
    /// the host following a fatal processor poisoning event in the RT audio loop.
    pub(crate) poison_restart_requested: Cell<bool>,
    /// Watchdog timer ID registered with the host for periodic housekeeping ticks (if supported).
    pub watchdog_timer: Cell<Option<clack_extensions::timer::TimerId>>,
    /// Currently armed watchdog period in milliseconds. Tracks the cadence so the
    /// adaptive-rebuild fallback (F-UX-02) only reprograms the host timer
    /// on an actual change.
    pub(crate) watchdog_period_ms: Cell<u32>,
    /// Persistent per-instance asynchronous model loader (F-UX-01).
    /// Spawned lazily on the first asynchronous load; `None` in the synchronous
    /// fallback mode or before the first load.
    pub(crate) model_loader: RefCell<Option<model_loader::ModelLoader>>,
    /// Shared delivery slot written by the loader worker and drained by the
    /// main thread on each `housekeeping()` tick. Off-RT only.
    pub(crate) model_result_slot: model_loader::ModelResultSlot,
    /// Whether the loader worker currently has an outstanding (dispatched,
    /// not yet consumed) job. Gates dispatching a new job.
    pub(crate) model_worker_busy: Cell<bool>,
    /// Generation of the job currently dispatched to the worker (`0` = none /
    /// invalidated). A worker result is committed only when its generation
    /// matches; mismatches were superseded and are discarded.
    pub(crate) dispatched_model_generation: Cell<u64>,
    /// Whether the dispatched job originated from a GUI load. Only a newer GUI
    /// request invalidates it (last-wins); presets are always honoured FIFO.
    pub(crate) dispatched_model_is_gui: Cell<bool>,
    /// Adaptive-rebuild delivery slots written by the loader worker and drained
    /// by the main thread on each `housekeeping()` tick (F-UX-02).
    /// Off-RT only.
    pub(crate) rebuild_slots: model_loader::RebuildSlots,
    /// Asynchronous loads accepted but not yet dispatched (worker busy).
    /// A newer GUI request supersedes pending GUI entries (last-wins), so at
    /// most one GUI load is ever pending at a time.
    pub(crate) pending_async_loads:
        RefCell<std::collections::VecDeque<model_loader::ModelLoaderJob>>,
}

impl<'a> NamClapMainThread<'a> {
    /// Applies a pending backend "user closed" signal to the lifecycle FSM.
    ///
    /// The Slint close callback cannot mutate the main-thread FSM directly, so
    /// it publishes `ColdShared::gui_user_closed`; this reconciles that signal
    /// before a host-driven transition is validated. Best-effort and
    /// idempotent — an event arriving in a state that no longer permits it is
    /// logged by the FSM and discarded, never propagated as an error.
    pub(crate) fn reconcile_pending_gui_close(&self) {
        if self
            .shared
            .cold
            .gui_user_closed
            .swap(false, Ordering::AcqRel)
        {
            let mut lifecycle = self.gui_lifecycle.get();
            let _ = lifecycle.transition(GuiEvent::UserClosed);
            self.gui_lifecycle.set(lifecycle);
        }
    }

    /// Discards a pending backend "user closed" signal without applying it.
    ///
    /// Used by `hide()`: the host's own hide request already drives the FSM to
    /// `Hidden`, so applying `UserClosed` afterwards would be redundant and
    /// would be rejected as an illegal transition from `Hidden`.
    pub(crate) fn discard_pending_gui_close(&self) {
        self.shared
            .cold
            .gui_user_closed
            .store(false, Ordering::Release);
    }

    /// Advances `ShowRequested → Active` once the backend window is ready.
    ///
    /// Centralizes the single `WindowReady` transition so both backend triggers
    /// (window creation in `spawn_gui` and the host's `show()` when the window
    /// already exists) share one owner. No-op when the host has not requested
    /// visibility.
    pub(crate) fn promote_window_ready(&self) {
        let mut lifecycle = self.gui_lifecycle.get();
        if lifecycle == GuiLifecycle::ShowRequested {
            let _ = lifecycle.transition(GuiEvent::WindowReady);
            self.gui_lifecycle.set(lifecycle);
        }
    }

    /// Flushes any model deferred by `load_model()` when `buffer_size == 0`
    /// (state-restore-before-activate scenario). Pre-sizes the model on the
    /// main thread and sends it to the audio thread via SPSC.
    ///
    /// F3 fix: this ensures heap alloc + mmap/munmap/memfd_create + drop
    /// never happen on the audio thread.
    pub fn flush_pending_model(&self) -> Result<(), PluginError> {
        let pending = lock_cold(&self.shared.cold.pending_model, "pending_model").take();
        let Some(p) = pending else { return Ok(()) };

        // RAII guard: any `Err`/early-return below restores the `PendingModel`
        // for retry instead of silently dropping it (F-RB-01). Disarmed only
        // once the payload is pushed to the SPSC.
        struct RestoreOnDrop<'a> {
            slot: &'a std::sync::Mutex<Option<PendingModel>>,
            pending: Option<PendingModel>,
        }
        impl Drop for RestoreOnDrop<'_> {
            fn drop(&mut self) {
                if let Some(p) = self.pending.take() {
                    *lock_cold(self.slot, "pending_model_restore") = Some(p);
                }
            }
        }
        let mut restore = RestoreOnDrop {
            slot: &self.shared.cold.pending_model,
            pending: Some(p),
        };

        let buffer_size = self.shared.cold.buffer_size.load(Ordering::Relaxed) as usize;
        if buffer_size == 0 {
            // Still no buffer_size — keep the model for a later retry.
            return Ok(());
        }

        let Some(PendingModel {
            model: mut model_l,
            generation,
            model_rate,
            input_mult_adj,
            output_mult_adj,
        }) = restore.pending.take()
        else {
            return Ok(());
        };

        let sample_rate = self.shared.cold.sample_rate.load(Ordering::Relaxed);

        // Construct the resampler HERE with the real host sample rate
        // now that activate() has been called.
        let new_resampler = match NamResampler::new_simple(sample_rate, model_rate) {
            Ok(r) => Box::new(r),
            Err(e) => {
                restore.pending = Some(PendingModel {
                    generation,
                    model: model_l,
                    model_rate,
                    input_mult_adj,
                    output_mult_adj,
                });
                self.note_flush_failure(&format!(
                    "Failed to create deferred resampler (host {sample_rate} Hz, model {model_rate} Hz): {e:?}. Check the host sample rate."
                ));
                return Err(static_plugin_error(
                    errors::dsp_resources::RESAMPLER_BUILD_FAILED,
                    format_args!("Failed to create deferred resampler: {e:?}"),
                ));
            }
        };

        // Streaming resample adapter (T1.2/F-PERF-002), sized for the worst-case
        // host block now that `buffer_size` is known.
        let new_stream =
            match crate::clap::plugin::build_stream_adapter(sample_rate, model_rate, buffer_size) {
                Ok(s) => s,
                Err(e) => {
                    restore.pending = Some(PendingModel {
                        generation,
                        model: model_l,
                        model_rate,
                        input_mult_adj,
                        output_mult_adj,
                    });
                    self.note_flush_failure(&format!(
                    "Failed to create deferred streaming buffer (buffer size {buffer_size}): {e:?}."
                ));
                    return Err(static_plugin_error(
                        errors::dsp_resources::STREAM_BUILD_FAILED,
                        format_args!("Failed to create deferred streaming buffer: {e:?}"),
                    ));
                }
            };

        if let Some(ref mut model) = model_l
            && let Err(e) = model.set_max_buffer_size(buffer_size)
        {
            restore.pending = Some(PendingModel {
                generation,
                model: model_l,
                model_rate,
                input_mult_adj,
                output_mult_adj,
            });
            self.note_flush_failure(&format!(
                "Failed to resize deferred model buffers for host buffer size ({buffer_size}): {e}. Try a smaller host buffer size."
            ));
            return Err(static_plugin_error(
                errors::activation::MODEL_RESIZE_FAILED,
                format_args!(
                    "Failed to resize deferred model buffers for host buffer size ({buffer_size}): {e}"
                ),
            ));
        }

        // Off-RT cost calibration (U-02 / F-PR-10), mirroring the direct load
        // path: size the first prewarm window from the real model cost.
        let prewarm_cost_ns = if crate::clap::processor::prewarm_policy::ADAPTIVE_COST_ENABLED {
            // Cap at the negotiated quantum so the calibration block never
            // exceeds the model's `max_buffer_size` (composed models truncate).
            let measure_frames = crate::clap::processor::prewarm_policy::PREWARM_MEASURE_FRAMES
                .min(buffer_size.max(1));
            model_l
                .as_mut()
                .map(|model| model.measure_cost_ns_per_sample(measure_frames))
        } else {
            None
        };

        let push_result =
            self.cmd_producer
                .borrow_mut()
                .try_push_command(ClapParamPayload::LoadModel {
                    generation,
                    model_l,
                    new_resampler,
                    new_stream,
                    input_mult_adj,
                    output_mult_adj,
                    prewarm_cost_ns,
                });
        match push_result {
            Ok(seq) => {
                // Delivered: disarm the restore guard so the drop below is a
                // no-op — the model now belongs to the audio thread.
                restore.pending = None;
                log::trace!("Deferred model sent to audio thread (seq={seq})");
                Ok(())
            }
            Err((PushError::Full, payload)) => {
                // Fail-closed — retain the model for retry instead of
                // dropping it. The resampler and streaming buffer are rebuilt on the next flush.
                if let ClapParamPayload::LoadModel {
                    generation,
                    model_l,
                    new_resampler,
                    new_stream,
                    input_mult_adj,
                    output_mult_adj,
                    prewarm_cost_ns: _,
                } = payload
                {
                    drop(new_resampler);
                    drop(new_stream);
                    *lock_cold(&self.shared.cold.pending_model, "pending_model") =
                        Some(PendingModel {
                            generation,
                            model: model_l,
                            model_rate,
                            input_mult_adj,
                            output_mult_adj,
                        });
                }
                self.host.request_callback();
                Ok(())
            }
        }
    }
    /// Records a deferred-load failure visibly: sets
    /// `RT_STATUS_MODEL_LOAD_FAILED` and fills `ui_load_error` /
    /// `ui_load_error_msg` with an actionable message (F-RB-01).
    ///
    /// Anti-spam: housekeeping retries every 250 ms while the model is
    /// retained, so the `log::error!` fires only on the flag transition; a
    /// flag that is already set logs at `debug` level.
    fn note_flush_failure(&self, msg: &str) {
        let already = self
            .shared
            .cold
            .rt_status
            .check_flag(spsc::RT_STATUS_MODEL_LOAD_FAILED);
        self.shared
            .cold
            .rt_status
            .set_flag(spsc::RT_STATUS_MODEL_LOAD_FAILED);
        self.shared
            .cold
            .ui_load_error
            .store(true, Ordering::Relaxed);
        *lock_cold(&self.shared.cold.ui_load_error_msg, "ui_load_error_msg") = msg.to_string();
        if already {
            log::debug!("flush_pending_model still failing: {msg}");
        } else {
            log::error!("flush_pending_model failed: {msg}");
        }
    }

    /// Synchronises atomic values from the audio thread and cold state into `self.params`
    /// before serialisation. Shared by `state.rs` and `state_context.rs`.
    pub(crate) fn snapshot_params(&self) {
        let mut params = self.params.borrow_mut();
        params.input_gain_db = f32::from_bits(
            self.shared
                .ui_to_rt
                .param_input_gain
                .load(std::sync::atomic::Ordering::Relaxed),
        );
        params.output_gain_db = f32::from_bits(
            self.shared
                .ui_to_rt
                .param_output_gain
                .load(std::sync::atomic::Ordering::Relaxed),
        );
        params.gate_threshold_db = f32::from_bits(
            self.shared
                .ui_to_rt
                .param_gate_thresh
                .load(std::sync::atomic::Ordering::Relaxed),
        );
        params.bypass = super::super::extensions::params::bypass_u32_to_bool(
            self.shared
                .ui_to_rt
                .param_bypass
                .load(std::sync::atomic::Ordering::Relaxed),
        );
        params.adaptive_compute =
            neural_amp_modeler_rs::common::params::AdaptiveComputeMode::from_f32(
                self.shared
                    .ui_to_rt
                    .param_adaptive_compute
                    .load(std::sync::atomic::Ordering::Relaxed) as f32,
            );
        params.slim_override = neural_amp_modeler_rs::dsp::adaptive::SlimOverride::from_f32(
            self.shared
                .ui_to_rt
                .param_slim_override
                .load(std::sync::atomic::Ordering::Relaxed) as f32,
        );
        params.oversample = neural_amp_modeler_rs::dsp::oversample::OversampleFactor::from_f32(
            self.shared
                .ui_to_rt
                .param_oversample
                .load(std::sync::atomic::Ordering::Relaxed) as f32,
        );
        params.activation_precision =
            neural_amp_modeler_rs::common::params::ActivationPrecision::from_f32(
                self.shared
                    .ui_to_rt
                    .param_activation
                    .load(std::sync::atomic::Ordering::Relaxed) as f32,
            );
        params.ir_path = lock_cold(&self.shared.cold.ir_path, "ir_path")
            .as_ref()
            .map(std::path::PathBuf::from);
        // Mandatory asset identity: keep the IR digest in lockstep with the path — no persisted IR
        // reference without its SHA-256 digest, and no stale digest when the
        // IR was cleared.
        params.ir_hash = lock_cold(&self.shared.cold.ir_hash, "ir_hash").clone();
        if params.ir_path.is_none() {
            params.ir_hash = None;
        }
    }

    /// Drains the GC-cascade completely before the plugin is destroyed.
    ///
    /// Called during the final `on_main_thread` callback or in final `deactivate()`.
    /// Ensures that no `Box<StaticModel>` or similar heap items remain alive in
    /// `GcOverflowBuffer` after the plugin terminates.
    ///
    /// `parking_lot` is the single-owner handoff from the RT state — in
    /// `deactivate()` the processor hands over `&mut self.parking_lot` here,
    /// after the audio thread has stopped and prior to `Drop` of RT state.
    /// A single call drops SPSC + overflow + the 16 off-RT slots.
    pub(crate) fn drain_gc_final(&self, parking_lot: &mut [Option<GcItem>; 16]) {
        use neural_amp_modeler_rs::common::spsc::drain_gc_channels;
        // Drain primary SPSC channel, overflow buffer, and RT parking lot
        let drained = {
            let mut gc_rx = self.gc_rx.borrow_mut();
            drain_gc_channels(
                &mut gc_rx,
                &self.shared.cold.gc_overflow,
                parking_lot,
                &self.shared.cold.rt_status,
            )
        };
        self.shared
            .cold
            .rt_status
            .drains
            .fetch_add(drained as u32, Ordering::Relaxed);
        if drained > 0 {
            log::debug!(
                "NAM-Plug: GC drain final — {} item(s) dropped on destroy",
                drained
            );
        }
        // Second pass: overflow may have been filled by RT between the first
        // drain and now (benign race — the second pass closes the window)
        let second = {
            let mut gc_rx = self.gc_rx.borrow_mut();
            drain_gc_channels(
                &mut gc_rx,
                &self.shared.cold.gc_overflow,
                parking_lot,
                &self.shared.cold.rt_status,
            )
        };
        self.shared
            .cold
            .rt_status
            .drains
            .fetch_add(second as u32, Ordering::Relaxed);
    }
}

impl<'a> Drop for NamClapMainThread<'a> {
    fn drop(&mut self) {
        log::info!(
            "NAM-Plug: Plugin instance destroying — sink unregister, quiescence, GUI fence down, teardown + GC drain."
        );

        // Flush any remaining telemetry flags before unregistering
        self.emit_pending_logs();

        // 1. Unregister instance sink from global NamLogger so no future logs are dispatched to this instance
        if let Some(logger) =
            neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::global()
        {
            logger.unregister_instance_sink(self.shared.cold.instance_id);
        }

        // 2. Quiesce the host log sink: acquire write lock to guarantee all in-flight try_read
        // dispatches have completed, then disable future dispatches (*lock = false).
        // Bounded spin/yield timeout of 50ms to avoid blocking DAW indefinitely if host stalls.
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_millis(50);
        let mut acquired = false;
        while start.elapsed() < timeout {
            if let Ok(mut lock) = self.shared.cold.host_log_quiescence.try_write() {
                *lock = false;
                acquired = true;
                break;
            }
            std::thread::yield_now();
        }
        if !acquired {
            log::warn!(
                "NAM-Plug: Timeout waiting for host_log_quiescence write lock during destroy; forcing write lock"
            );
            if let Ok(mut lock) = self.shared.cold.host_log_quiescence.write() {
                *lock = false;
            }
        }

        // 3. Clear host log sink arc in cold shared
        *lock_cold(&self.shared.cold.host_log_sink, "host_log_sink") = None;

        // 3b. Unregister periodic watchdog timer before lowering alive_fence
        if let Some(timer_id) = self.watchdog_timer.take()
            && let Some(timer_ext) = self
                .host
                .get_extension::<clack_extensions::timer::HostTimer>()
        {
            let unregister_res = timer_ext.unregister_timer(&self.host, timer_id);
            if let Err(err) = unregister_res {
                log::warn!(
                    "NAM-Plug: Failed to unregister watchdog timer {timer_id} during destroy: {err}"
                );
            }
        }

        // 4. Lower the alive fence BEFORE releasing any shared state, so
        // GUI/dialog threads stop dereferencing `NamClapShared` and the host
        // handle immediately. Their event loops are no-ops from this point on.
        self.shared.cold.alive_fence.store(false, Ordering::Release);

        // 5. Synchronous GUI teardown: close windows and bounded-join their
        // threads before `NamClapShared` is dropped (the wrapper drops the
        // main thread before the shared state). A reaper is spawned only as a
        // last resort, after the fence is already down.
        self.teardown_gui_resources();

        // 6. On destroy, `deactivate()` has already transferred the RT parking lot
        // for final drain (single-owner handoff). Here the lot is empty — no
        // RT producer is active — and the drain covers SPSC + overflow one final time.
        let mut empty_rt_parking_lot: [Option<GcItem>; 16] = Default::default();
        self.drain_gc_final(&mut empty_rt_parking_lot);
    }
}

impl<'a> PluginMainThread<'a, NamClapShared> for NamClapMainThread<'a> {
    /// Called periodically or in response to host events.
    /// Delegates to concern-specific sub-module methods.
    fn on_main_thread(&self) {
        if !self.shared.cold.alive_fence.load(Ordering::Relaxed) {
            // Fence down implies RT processor has already stopped (deactivate) and
            // the lot arrived drained during handoff; final drain covers SPSC + overflow.
            let mut empty_rt_parking_lot: [Option<GcItem>; 16] = Default::default();
            self.drain_gc_final(&mut empty_rt_parking_lot);
            return;
        }
        self.housekeeping();
        self.emit_pending_logs();
    }
}

/// Runtime thread-check assertion for debug builds.
///
/// Queries the host's `thread-check` extension and panics (debug-only) if
/// the current thread is **not** the CLAP main thread.  When the host does
/// not provide the extension the check is skipped silently.
pub fn debug_assert_main_thread(host: &HostMainThreadHandle) {
    if let Some(check) = host
        .shared()
        .get_extension::<clack_extensions::thread_check::HostThreadCheck>()
    {
        debug_assert!(
            check.is_main_thread(&host.shared()).unwrap_or(true),
            "CLAP method called from a thread other than the main thread"
        );
    }
}

#[cfg(test)]
#[path = "model_loader_test.rs"]
mod model_loader_test;
