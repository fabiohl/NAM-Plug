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
use super::shared::{ClapParamPayload, NamClapShared, PendingModel};
use crate::clap::gui::lifecycle::GuiLifecycle;
use clack_plugin::prelude::*;
use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
use neural_amp_modeler_rs::common::params::ProcessingParams;
use neural_amp_modeler_rs::common::spsc::{self, GcItem};
use neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF;
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::models::{NamModel, StaticModel};
use rtrb::{Consumer, Producer};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Main thread exclusive state (model loading, state save/load).
pub struct NamClapMainThread<'a> {
    pub(crate) shared: &'a NamClapShared,
    /// Current parameters known by the main thread (mirror of the audio thread params).
    pub params: ProcessingParams,
    /// Host handle for notifications (latency_changed, state, etc.).
    pub host: HostMainThreadHandle<'a>,
    /// System snapshot for emitting diagnostics.
    pub sys: SystemSnapshot,
    /// Producer to send updates to the audio thread with coalescing and ack.
    pub cmd_producer: CommandProducer<'a>,
    /// Consumer to collect garbage (obsolete models) from the audio thread.
    pub gc_rx: Consumer<GcItem>,
    /// Producer to send slimmable-rebuilt models to the audio thread.
    pub slimmable_tx: Producer<Option<Box<StaticModel>>>,
    /// Cached last latency reported to the host to avoid redundant notifications.
    pub last_reported_latency: u32,
    /// Cached last CabSim tail length reported to the host to avoid redundant notifications.
    pub last_reported_cabsim_tail: u32,
    /// Baseview window handle for GUI lifecycle control (embedded mode).
    pub window_handle: Option<baseview::WindowHandle>,
    /// Thread handle for the floating window event loop.
    pub floating_thread_handle: Option<std::thread::JoinHandle<()>>,
    /// Close signal for the floating window.
    pub floating_close_signal: Option<Arc<AtomicBool>>,
    /// Handle da thread de file-dialog de modelo (se ativa). Joinado no teardown.
    #[expect(dead_code, reason = "held for join on teardown, never read directly")]
    pub(crate) dialog_handle: Option<std::thread::JoinHandle<()>>,
    /// Estado compartilhado com a thread de file-dialog de modelo.
    #[expect(dead_code, reason = "held for Arc lifecycle, never read directly")]
    pub(crate) dialog_state:
        Option<Arc<crate::clap::gui::ui::zones::dialog_state::DialogSharedState>>,
    /// Handle da thread de file-dialog de IR (se ativa). Joinado no teardown.
    #[expect(dead_code, reason = "held for join on teardown, never read directly")]
    pub(crate) ir_dialog_handle: Option<std::thread::JoinHandle<()>>,
    /// Estado compartilhado com a thread de file-dialog de IR.
    #[expect(dead_code, reason = "held for Arc lifecycle, never read directly")]
    pub(crate) ir_dialog_state:
        Option<Arc<crate::clap::gui::ui::zones::dialog_state::IrDialogSharedState>>,
    /// Finite state machine tracking the GUI window lifecycle
    /// (Hidden → ShowRequested → Active → HideRequested → Destroyed).
    pub(crate) gui_lifecycle: GuiLifecycle,
    /// Flag indicating whether hugepage status has been synced for this instance.
    pub(crate) hugepage_synced: bool,
}

impl<'a> NamClapMainThread<'a> {
    /// Flushes any model deferred by `load_model()` when `buffer_size == 0`
    /// (state-restore-before-activate scenario). Pre-sizes the model on the
    /// main thread and sends it to the audio thread via SPSC.
    ///
    /// F3 fix: this ensures heap alloc + mmap/munmap/memfd_create + drop
    /// never happen on the audio thread.
    pub fn flush_pending_model(&mut self) -> Result<(), PluginError> {
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

        match self
            .cmd_producer
            .try_push_command(ClapParamPayload::LoadModel {
                model_l,
                new_resampler,
                input_mult_adj,
                output_mult_adj,
            }) {
            Ok(seq) => {
                log::trace!("Deferred model sent to audio thread (seq={seq})");
                Ok(())
            }
            Err((PushError::Full, payload)) => {
                // R-10: fail-closed — retain the model for retry instead of
                // dropping it. The resampler is rebuilt on the next flush.
                if let ClapParamPayload::LoadModel {
                    model_l,
                    new_resampler,
                    input_mult_adj,
                    output_mult_adj,
                } = payload
                {
                    drop(new_resampler);
                    if let Ok(mut guard) = self.shared.cold.pending_model.lock() {
                        *guard = Some(PendingModel {
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
    pub(crate) fn snapshot_params(&mut self) {
        self.params.input_gain_db = f32::from_bits(
            self.shared
                .ui_to_rt
                .param_input_gain
                .load(std::sync::atomic::Ordering::Relaxed),
        );
        self.params.output_gain_db = f32::from_bits(
            self.shared
                .ui_to_rt
                .param_output_gain
                .load(std::sync::atomic::Ordering::Relaxed),
        );
        self.params.gate_threshold_db = f32::from_bits(
            self.shared
                .ui_to_rt
                .param_gate_thresh
                .load(std::sync::atomic::Ordering::Relaxed),
        );
        self.params.bypass = super::super::extensions::params::bypass_u32_to_bool(
            self.shared
                .ui_to_rt
                .param_bypass
                .load(std::sync::atomic::Ordering::Relaxed),
        );
        self.params.adaptive_compute =
            neural_amp_modeler_rs::common::params::AdaptiveComputeMode::from_f32(
                self.shared
                    .ui_to_rt
                    .param_adaptive_compute
                    .load(std::sync::atomic::Ordering::Relaxed) as f32,
            );
        self.params.slim_override = neural_amp_modeler_rs::dsp::adaptive::SlimOverride::from_f32(
            self.shared
                .ui_to_rt
                .param_slim_override
                .load(std::sync::atomic::Ordering::Relaxed) as f32,
        );
        self.params.oversample = neural_amp_modeler_rs::dsp::oversample::OversampleFactor::from_f32(
            self.shared
                .ui_to_rt
                .param_oversample
                .load(std::sync::atomic::Ordering::Relaxed) as f32,
        );
        self.params.activation_precision =
            neural_amp_modeler_rs::common::params::ActivationPrecision::from_f32(
                self.shared
                    .ui_to_rt
                    .param_activation
                    .load(std::sync::atomic::Ordering::Relaxed) as f32,
            );
        if let Ok(ir_guard) = self.shared.cold.ir_path.lock() {
            self.params.ir_path = ir_guard.as_ref().map(std::path::PathBuf::from);
        }
    }

    /// Drains the GC-cascade completely before the plugin is destroyed.
    ///
    /// Called during the final `on_main_thread` callback or in final `deactivate()`.
    /// Ensures that no `Box<StaticModel>` or similar heap items remain alive in
    /// `GcOverflowBuffer` after the plugin terminates (R-11).
    ///
    /// R-04: `parking_lot` is the single-owner handoff from the RT state — in
    /// `deactivate()` the processor hands over `&mut self.parking_lot` here,
    /// after the audio thread has stopped and prior to `Drop` of RT state.
    /// A single call drops SPSC + overflow + the 16 off-RT slots.
    pub(crate) fn drain_gc_final(&mut self, parking_lot: &mut [Option<GcItem>; 16]) {
        use neural_amp_modeler_rs::common::spsc::drain_gc_channels;
        // Drain primary SPSC channel, overflow buffer, and RT parking lot
        let drained = drain_gc_channels(
            &mut self.gc_rx,
            &self.shared.cold.gc_overflow,
            parking_lot,
            &self.shared.cold.rt_status,
        );
        self.shared
            .cold
            .rt_status
            .drains
            .fetch_add(drained as u32, Ordering::Relaxed);
        if drained > 0 {
            log::debug!(
                "NAM-rs: GC drain final — {} item(s) liberados no destroy (R11)",
                drained
            );
        }
        // Segunda passagem: overflow pode ter sido preenchido pelo RT entre a primeira
        // drenagem e agora (race benigna — a segunda passagem fecha a janela)
        let second = drain_gc_channels(
            &mut self.gc_rx,
            &self.shared.cold.gc_overflow,
            parking_lot,
            &self.shared.cold.rt_status,
        );
        self.shared
            .cold
            .rt_status
            .drains
            .fetch_add(second as u32, Ordering::Relaxed);
    }
}

impl<'a> Drop for NamClapMainThread<'a> {
    fn drop(&mut self) {
        log::info!("NAM-rs: Plugin instance destroying — GUI fence down, teardown + GC drain.");
        // R-09: lower the alive fence BEFORE releasing any shared state, so
        // GUI/dialog threads stop dereferencing `NamClapShared` and the host
        // handle immediately. Their event loops are no-ops from this point on.
        self.shared.cold.alive_fence.store(false, Ordering::Release);
        // Synchronous GUI teardown: close windows and bounded-join their
        // threads before `NamClapShared` is dropped (the wrapper drops the
        // main thread before the shared state). A reaper is spawned only as a
        // last resort, after the fence is already down.
        self.teardown_gui_resources();
        // R-04: no destroy, `deactivate()` já transferiu o parking lot do RT
        // para o drain final (ele é single-owner e já veio drenado). Aqui o
        // lot é vazio — nenhum produtor RT está vivo — e o drain cobre SPSC +
        // overflow pela última vez.
        let mut empty_rt_parking_lot: [Option<GcItem>; 16] = Default::default();
        self.drain_gc_final(&mut empty_rt_parking_lot);
    }
}

impl<'a> PluginMainThread<'a, NamClapShared> for NamClapMainThread<'a> {
    /// Called periodically or in response to host events.
    /// Delegates to concern-specific sub-module methods.
    fn on_main_thread(&mut self) {
        if !self.shared.cold.alive_fence.load(Ordering::Relaxed) {
            // R-04: fence down ⇒ o processador RT já parou (deactivate) e o
            // lot veio drenado no handoff; drain final cobre SPSC + overflow.
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
