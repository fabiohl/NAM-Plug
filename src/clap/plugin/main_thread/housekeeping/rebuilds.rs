// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Main-thread adaptive rebuild handlers (slimmable WaveNet, oversampling, and
//! neural cabinet rebuilds).

use super::super::NamClapMainThread;
use super::super::model_loader::{
    self, CabNamRebuildOutcome, OversampleRebuildOutcome, SlimmableRebuildOutcome,
};
use crate::clap::plugin::ClapParamPayload;
use crate::clap::plugin::command_scheduler::PushError;
use neural_amp_modeler_rs::common::diagnostics::{NamDiagnostic, NamErrorCode};
use neural_amp_modeler_rs::common::spsc;
use neural_amp_modeler_rs::dsp::oversample::{OversampleEngine, OversampleFactor};
use neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF;
use std::sync::atomic::Ordering;

impl<'a> NamClapMainThread<'a> {
    /// WaveNet slimmable rebuild.
    pub(super) fn handle_slimmable_rebuild(&self) {
        if self.shared.cold.pending_slimmable_request() == 0 {
            return;
        }

        if self.ensure_rebuild_worker() {
            self.drain_slimmable_rebuild();
            return;
        }

        self.handle_slimmable_rebuild_inline();
    }

    /// Consumes a worker-built slimmable payload (when ready), pushes it onto the
    /// SPSC and acknowledges the exact request it was built for.
    pub(super) fn drain_slimmable_rebuild(&self) {
        let outcome = {
            let mut slot = self
                .rebuild_slots
                .slimmable
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            slot.take()
        };
        let Some(outcome) = outcome else {
            return;
        };

        self.apply_slimmable_outcome(outcome, true);
    }

    /// WaveNet slimmable rebuild inline fallback when the loader worker is unavailable.
    fn handle_slimmable_rebuild_inline(&self) {
        let Some((request_id, target_ch, generation, dual)) =
            self.shared.cold.snapshot_slimmable_request()
        else {
            return;
        };

        let outcome =
            model_loader::build_slimmable(self.shared, request_id, target_ch, generation, dual);
        self.apply_slimmable_outcome(outcome, false);
    }

    fn apply_slimmable_outcome(&self, outcome: SlimmableRebuildOutcome, retain_in_slot: bool) {
        match outcome {
            SlimmableRebuildOutcome::Ready(request_id, boxed) => {
                match self.slimmable_tx.borrow_mut().push(boxed) {
                    Ok(()) => {
                        self.shared.cold.complete_slimmable_request(request_id);
                    }
                    Err(rtrb::PushError::Full(retained)) => {
                        if retain_in_slot {
                            *self
                                .rebuild_slots
                                .slimmable
                                .lock()
                                .unwrap_or_else(|e| e.into_inner()) =
                                Some(SlimmableRebuildOutcome::Ready(request_id, retained));
                        }
                        log::warn!("NAM-Plug: slimmable channel full — rebuild will retry");
                        self.host.request_callback();
                    }
                }
            }
            SlimmableRebuildOutcome::Failed(request_id) => {
                if self.shared.cold.complete_slimmable_request(request_id) {
                    self.shared
                        .cold
                        .rt_status
                        .set_flag(spsc::RT_STATUS_SLIMMABLE_SLICE_FAILED);
                }
            }
            SlimmableRebuildOutcome::Skipped(request_id) => {
                self.shared.cold.complete_slimmable_request(request_id);
            }
        }
    }

    /// Oversampling engine rebuild.
    pub(super) fn handle_oversample_rebuild(&self) {
        if !self
            .shared
            .cold
            .rt_status
            .check_flag_acquire(spsc::RT_STATUS_NEEDS_OS_REBUILD)
        {
            return;
        }

        if self.ensure_rebuild_worker() {
            self.drain_oversample_rebuild();
            return;
        }

        self.handle_oversample_rebuild_inline();
    }

    /// Consumes a worker-built oversampling payload (when ready), pushes it onto
    /// the command ring and clears the flag on success.
    fn drain_oversample_rebuild(&self) {
        let outcome = {
            let mut slot = self
                .rebuild_slots
                .oversample
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            slot.take()
        };
        let Some(outcome) = outcome else {
            return;
        };

        match outcome {
            OversampleRebuildOutcome::Ready(os_l, os_r) => {
                let push = self
                    .cmd_producer
                    .borrow_mut()
                    .try_push_command(ClapParamPayload::SetOversample { os_l, os_r });
                match push {
                    Ok(_) => self
                        .shared
                        .cold
                        .rt_status
                        .clear_flag_release(spsc::RT_STATUS_NEEDS_OS_REBUILD),
                    Err(err) => {
                        let (PushError::Full, payload) = *err;
                        if let ClapParamPayload::SetOversample { os_l, os_r } = payload {
                            *self
                                .rebuild_slots
                                .oversample
                                .lock()
                                .unwrap_or_else(|e| e.into_inner()) =
                                Some(OversampleRebuildOutcome::Ready(os_l, os_r));
                        }
                        log::warn!(
                            "NAM-Plug: Command ring full during SetOversample; retaining flag for retry"
                        );
                        self.host.request_callback();
                    }
                }
            }
            OversampleRebuildOutcome::Failed => self.emit_oversample_oom_diagnostic(),
        }
    }

    /// Oversampling engine rebuild inline fallback when the loader worker is unavailable.
    fn handle_oversample_rebuild_inline(&self) {
        if !self
            .shared
            .cold
            .rt_status
            .check_flag_acquire(spsc::RT_STATUS_NEEDS_OS_REBUILD)
        {
            return;
        }

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
            match self
                .cmd_producer
                .borrow_mut()
                .try_push_command(ClapParamPayload::SetOversample {
                    os_l: Box::new(l),
                    os_r: Box::new(r),
                }) {
                Ok(_) => {
                    self.shared
                        .cold
                        .rt_status
                        .clear_flag_release(spsc::RT_STATUS_NEEDS_OS_REBUILD);
                }
                Err(err) => {
                    let (PushError::Full, _payload) = *err;
                    log::warn!(
                        "NAM-Plug: Command ring full during SetOversample; retaining flag for retry"
                    );
                    self.host.request_callback();
                }
            }
        } else {
            self.emit_oversample_oom_diagnostic();
        }
    }

    fn emit_oversample_oom_diagnostic(&self) {
        NamDiagnostic::new(NamErrorCode::OutOfMemory, &self.sys)
            .message("Failed to rebuild oversample engine (OOM).")
            .hint("The current oversampling state will be preserved.")
            .emit_warning();
        self.shared
            .cold
            .rt_status
            .clear_flag_release(spsc::RT_STATUS_NEEDS_OS_REBUILD);
    }

    /// Off-RT neural cabinet (`CabNamSlot`) rebuild requested by RT when
    /// `cab_bypass` re-engages or `reset()` clears a dirty `NamModel`.
    pub(super) fn handle_cab_nam_rebuild(&self) {
        let Some(generation) = self.shared.cold.pending_cab_nam_generation() else {
            return;
        };

        if self.ensure_rebuild_worker() {
            self.drain_cab_nam_rebuild(generation);
            return;
        }

        let outcome = model_loader::build_cab_nam_rebuild(self.shared, generation);
        self.push_cab_nam_outcome(generation, outcome, false);
    }

    fn drain_cab_nam_rebuild(&self, pending_gen: u64) {
        let outcome = {
            let mut slot = self
                .rebuild_slots
                .cab_nam
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            slot.take()
        };
        let Some(outcome) = outcome else {
            return;
        };
        self.push_cab_nam_outcome(pending_gen, outcome, true);
    }

    fn push_cab_nam_outcome(
        &self,
        pending_gen: u64,
        outcome: CabNamRebuildOutcome,
        retain_in_slot_on_full: bool,
    ) {
        match outcome {
            CabNamRebuildOutcome::Ready(built_gen, slot) => {
                if built_gen != pending_gen {
                    self.host.request_callback();
                    return;
                }
                match self.cmd_producer.borrow_mut().try_push_command(
                    ClapParamPayload::RebuildCabNam {
                        generation: built_gen,
                        slot,
                    },
                ) {
                    Ok(_) => self.shared.cold.clear_cab_nam_rebuild_if_gen(built_gen),
                    Err(err) => {
                        let (PushError::Full, payload) = *err;
                        if retain_in_slot_on_full
                            && let ClapParamPayload::RebuildCabNam { generation, slot } = payload
                        {
                            *self
                                .rebuild_slots
                                .cab_nam
                                .lock()
                                .unwrap_or_else(|e| e.into_inner()) =
                                Some(CabNamRebuildOutcome::Ready(generation, slot));
                        }
                        log::warn!(
                            "NAM-Plug: Command ring full during RebuildCabNam; retaining request for retry"
                        );
                        self.host.request_callback();
                    }
                }
            }
            CabNamRebuildOutcome::Skipped(built_gen) => {
                self.shared.cold.clear_cab_nam_rebuild_if_gen(built_gen);
            }
            CabNamRebuildOutcome::Failed(err) => {
                log::warn!("NAM-Plug: off-RT neural cabinet rebuild failed: {err}");
                self.shared.cold.clear_cab_nam_rebuild_if_gen(pending_gen);
            }
        }
    }

    /// Ensures the asynchronous loader worker exists, spawning it lazily.
    pub(super) fn ensure_rebuild_worker(&self) -> bool {
        let dead = self
            .model_loader
            .borrow()
            .as_ref()
            .is_some_and(|loader| loader.is_finished());
        if dead {
            log::error!(
                "NAM-Plug: model loader worker terminated unexpectedly; dropping it and \
                 respawning on demand"
            );
            self.model_worker_busy.set(false);
            self.dispatched_model_generation.set(0);
            self.dispatched_model_is_gui.set(false);
            self.shared.cold.ui_loading.store(false, Ordering::Relaxed);
            *self.model_loader.borrow_mut() = None;
        }
        if self.model_loader.borrow().is_some() {
            return true;
        }
        if let Err(msg) = self.spawn_model_loader() {
            log::warn!(
                "NAM-Plug: adaptive rebuild worker unavailable ({msg}); \
                 rebuilding on the main thread"
            );
            return false;
        }
        true
    }
}
