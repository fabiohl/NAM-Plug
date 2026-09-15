// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Main thread exclusive state (model loading, state save/load).
//!
//! Split into concern-specific sub-modules:
//! - `housekeeping` — GC drain, status flags, hugepage sync, pending model load, latency
//! - `logging` — RT→main transient event logging via atomic flags
//! - `load` — model loading + error_code mapping

mod housekeeping;
mod load;
mod logging;

use super::command_scheduler::{CommandProducer, PushError};
use super::shared::{
    ClapParamPayload, NamClapShared, PendingModel, PendingRestore, SlimmableRebuild, StagedRestore,
    StagedSwap,
};
use crate::clap::gui::lifecycle::{GuiEvent, GuiLifecycle};
use clack_plugin::prelude::*;
use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
use neural_amp_modeler_rs::common::params::ProcessingParams;
use neural_amp_modeler_rs::common::spsc::{self, GcItem};
use neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF;
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::models::NamModel;
use rtrb::{Consumer, Producer};
use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::sync::atomic::Ordering;

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
    pub slimmable_tx: RefCell<Producer<SlimmableRebuild>>,
    /// Cached last latency reported to the host to avoid redundant notifications.
    pub last_reported_latency: Cell<u32>,
    /// Cached last CabSim tail length reported to the host to avoid redundant notifications.
    pub last_reported_cabsim_tail: Cell<u32>,
    /// Last-seen value of `ColdShared::slimmable_stale_discarded_total`, used to
    /// log stale-slimmable-rebuild discards exactly once.
    pub last_seen_slimmable_stale: Cell<u32>,
    /// Slint window handle for GUI lifecycle control.
    pub slint_window: RefCell<Option<slint::Weak<crate::clap::gui::MainWindow>>>,
    /// Persistent per-instance GUI worker thread (owns the Slint platform and
    /// event loop for the instance lifetime; survives `gui.destroy()` so
    /// repeated create/destroy cycles keep working — Slint 1.17 pins the
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
    /// host restart cycle (Strict Restart Policy / TR.1).
    ///
    /// Set by `atomic_commit()` when the restore package changes physical
    /// stream latency, physical cabsim latency, or oversampling factor.
    /// Parked here until `activate()` installs the entire package atomically.
    pub(crate) staged_restore: RefCell<Option<StagedRestore>>,
    /// Latency-affecting model/IR swap staged to land only on the next host
    /// restart cycle (Strict Restart Policy / TR.1).
    ///
    /// Set by `load_model()`/`load_cabsim()`/IR-clear when the swap would
    /// change the physical latency: the resources are fully built off-RT here,
    /// `host.request_restart()` is issued, and `activate()` consumes this slot
    /// during the restart cycle — the DSP keeps the old, still-reported
    /// latency until then. Same-latency swaps bypass this slot and apply
    /// continuously through the SPSC. Latest-wins coalescing per component.
    pub(crate) staged_swap: RefCell<Option<StagedSwap>>,
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
        let pending = if let Ok(mut guard) = self.shared.cold.pending_model.lock() {
            guard.take()
        } else {
            return Ok(());
        };
        let Some(p) = pending else { return Ok(()) };

        let buffer_size = self.shared.cold.buffer_size.load(Ordering::Relaxed) as usize;
        if buffer_size == 0 {
            // Still no buffer_size — store model back for later retry.
            if let Ok(mut guard) = self.shared.cold.pending_model.lock() {
                *guard = Some(p);
            }
            return Ok(());
        }

        let PendingModel {
            model: mut model_l,
            generation,
            model_rate,
            input_mult_adj,
            output_mult_adj,
        } = p;

        let sample_rate = self.shared.cold.sample_rate.load(Ordering::Relaxed);

        // Construct the resampler HERE with the real host sample rate
        // and buffer capacity — both are known now that activate() has been called.
        let buf_capacity = buffer_size.max(MAX_RESAMP_BUF).max(1024) * 2;
        let new_resampler = Box::new(
            NamResampler::new(sample_rate, model_rate, buf_capacity).map_err(|e| {
                PluginError::Message(Box::leak(
                    format!("Failed to create deferred resampler: {:?}", e).into_boxed_str(),
                ))
            })?,
        );

        // Streaming resample adapter (T1.2/F-PERF-002), sized for the worst-case
        // host block now that `buffer_size` is known.
        let new_stream =
            crate::clap::plugin::build_stream_adapter(sample_rate, model_rate, buffer_size)
                .map_err(|e| {
                    PluginError::Message(Box::leak(
                        format!("Failed to create deferred streaming buffer: {e:?}")
                            .into_boxed_str(),
                    ))
                })?;

        if let Some(ref mut model) = model_l
            && let Err(e) = model.set_max_buffer_size(buffer_size)
        {
            self.shared
                .cold
                .rt_status
                .set_flag(spsc::RT_STATUS_MODEL_LOAD_FAILED);
            return Err(PluginError::Message(Box::leak(
                format!(
                    "Failed to resize deferred model buffers for host buffer size ({}): {}",
                    buffer_size, e
                )
                .into_boxed_str(),
            )));
        }

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
                });
        match push_result {
            Ok(seq) => {
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
                } = payload
                {
                    drop(new_resampler);
                    drop(new_stream);
                    if let Ok(mut guard) = self.shared.cold.pending_model.lock() {
                        *guard = Some(PendingModel {
                            generation,
                            model: model_l,
                            model_rate,
                            input_mult_adj,
                            output_mult_adj,
                        });
                    }
                }
                self.host.request_callback();
                Ok(())
            }
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
        if let Ok(ir_guard) = self.shared.cold.ir_path.lock() {
            params.ir_path = ir_guard.as_ref().map(std::path::PathBuf::from);
        }
        // Mandatory asset identity: keep the IR digest in lockstep with the path — no persisted IR
        // reference without its SHA-256 digest, and no stale digest when the
        // IR was cleared.
        if let Ok(hash_guard) = self.shared.cold.ir_hash.lock() {
            params.ir_hash = hash_guard.clone();
        }
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
        log::info!("NAM-Plug: Plugin instance destroying — GUI fence down, teardown + GC drain.");
        // Lower the alive fence BEFORE releasing any shared state, so
        // GUI/dialog threads stop dereferencing `NamClapShared` and the host
        // handle immediately. Their event loops are no-ops from this point on.
        self.shared.cold.alive_fence.store(false, Ordering::Release);
        // Synchronous GUI teardown: close windows and bounded-join their
        // threads before `NamClapShared` is dropped (the wrapper drops the
        // main thread before the shared state). A reaper is spawned only as a
        // last resort, after the fence is already down.
        self.teardown_gui_resources();
        // On destroy, `deactivate()` has already transferred the RT parking lot
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
