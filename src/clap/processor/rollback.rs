// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! RAII rollback guard for `activate()` resource extraction.
//!
//! If any allocation stage in `activate()` fails
//! (resampler, ConvEngine, oversampling engines, buffer pre-allocation),
//! the guard restores ownership of SPSC channel ends and `DeactivatedDspState`
//! back into `ColdShared`, leaving the plugin in a clean deactivated state
//! ready for the next `activate()` attempt.

use crate::clap::plugin::lock::lock_cold;
use crate::clap::plugin::shared::PendingModel;
use crate::clap::plugin::{ClapParamPayload, NamClapShared, PendingRestartOs, SlimmableRebuild};
use clack_plugin::plugin::PluginError;
use neural_amp_modeler_rs::common::spsc::GcItem;
use rtrb::{Consumer, Producer};

use std::sync::atomic::Ordering;

use super::deactivated::DeactivatedDspState;

/// RAII guard that restores extracted shared resources on drop.
///
/// Must be created immediately after the first SPSC channel extraction
/// in `activate()`. Each subsequent resource extraction is stored in the
/// guard. On early return (`?`), Drop fires and puts everything back into
/// `ColdShared`. On success, `defuse()` takes ownership of the resources
/// (transferring them into `NamClapProcessor`) and disarms the destructor.
pub(crate) struct ActivateRollbackGuard<'a> {
    shared: &'a NamClapShared,
    pub(crate) param_rx: Option<Consumer<Box<ClapParamPayload>>>,
    pub(crate) gc_tx: Option<Producer<GcItem>>,
    pub(crate) slimmable_rx: Option<Consumer<Box<SlimmableRebuild>>>,
    pub(crate) deactivated: Option<DeactivatedDspState>,
    pub(crate) pending_restart_os_factor: Option<PendingRestartOs>,
}

/// Resources extracted from `ColdShared` during `activate()`.
/// Returned by `ActivateRollbackGuard::defuse()` on success.
pub(crate) struct ActivatedResources {
    pub(crate) param_rx: Consumer<Box<ClapParamPayload>>,
    pub(crate) gc_tx: Producer<GcItem>,
    pub(crate) slimmable_rx: Consumer<Box<SlimmableRebuild>>,
}

impl<'a> ActivateRollbackGuard<'a> {
    pub(crate) fn new(shared: &'a NamClapShared) -> Self {
        Self {
            shared,
            param_rx: None,
            gc_tx: None,
            slimmable_rx: None,
            deactivated: None,
            pending_restart_os_factor: None,
        }
    }

    /// Consumes the guard, transferring SPSC channel ownership to the caller
    /// (for the `NamClapProcessor` constructor). Dropping the guard after
    /// defuse is a no-op — resources have been moved out.
    pub(crate) fn defuse(mut self) -> Result<ActivatedResources, PluginError> {
        let rx = self
            .param_rx
            .take()
            .ok_or_else(|| PluginError::Message("param_rx must be set before defuse"))?;
        let tx = self
            .gc_tx
            .take()
            .ok_or_else(|| PluginError::Message("gc_tx must be set before defuse"))?;
        let slimmable = self
            .slimmable_rx
            .take()
            .ok_or_else(|| PluginError::Message("slimmable_rx must be set before defuse"))?;
        self.deactivated = None;
        self.pending_restart_os_factor = None;
        Ok(ActivatedResources {
            param_rx: rx,
            gc_tx: tx,
            slimmable_rx: slimmable,
        })
    }
}

impl Drop for ActivateRollbackGuard<'_> {
    fn drop(&mut self) {
        // Restore SPSC channels, DeactivatedDspState, and pending restart factor in reverse
        // extraction order. Each `.take()` moves the resource out of the
        // guard so the Drop is idempotent (restore-once). Mutex poisoning
        // is recovered via `into_inner()` — the resource must be restored
        // even if a previous lock attempt panicked.
        if let Some(restart) = self.pending_restart_os_factor.take() {
            self.shared
                .cold
                .pending_restart_os_factor
                .store(restart.encode(), Ordering::Release);
        }
        if let Some(rx) = self.slimmable_rx.take() {
            *self
                .shared
                .cold
                .slimmable_rx
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(rx);
        }
        if let Some(tx) = self.gc_tx.take() {
            *self
                .shared
                .cold
                .gc_tx
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(tx);
        }
        if let Some(mut rx) = self.param_rx.take() {
            // Recover the ring without losing commands: non-model payloads are
            // staged in their original order for replay by the next activation
            // (the ring consumer cannot be re-seeded), while the newest
            // LoadModel is preserved in `pending_model` (with its payload rate
            // and generation) for `flush_pending_model()`.
            let mut newest_model: Option<PendingModel> = None;
            let mut drained: u64 = 0;
            let mut staged = self
                .shared
                .cold
                .recovered_commands
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            // Recovered commands are older than anything already staged.
            let newer = std::mem::take(&mut *staged);
            while let Ok(boxed_payload) = rx.pop() {
                drained += 1;
                if !matches!(*boxed_payload, ClapParamPayload::LoadModel { .. }) {
                    staged.push_back(boxed_payload);
                    continue;
                }
                if let ClapParamPayload::LoadModel {
                    generation,
                    model_l,
                    model_r,
                    new_resampler: _,
                    new_stream: _,
                    model_rate,
                    input_mult_adj,
                    output_mult_adj,
                    cal_meta,
                    prewarm_cost_ns: _,
                } = *boxed_payload
                {
                    let candidate = PendingModel {
                        generation,
                        model: model_l,
                        model_r,
                        model_rate,
                        input_mult_adj,
                        output_mult_adj,
                        cal_meta,
                    };
                    // Concurrent generations: keep the newest; a superseded
                    // model payload drops here on the main thread.
                    if newest_model
                        .as_ref()
                        .is_none_or(|p| candidate.generation > p.generation)
                    {
                        newest_model = Some(candidate);
                    }
                }
            }
            staged.extend(newer);
            drop(staged);

            if let Some(candidate) = newest_model {
                let mut slot = lock_cold(&self.shared.cold.pending_model, "pending_model_rollback");
                // Never let an older recovered generation overwrite a newer
                // model that a subsequent load already deferred.
                if slot
                    .as_ref()
                    .is_none_or(|p| candidate.generation > p.generation)
                {
                    *slot = Some(candidate);
                }
            }

            // The drained commands were consumed from the ring. Account for
            // them so a main-thread `wait_for_ack` does not stall on sequence
            // numbers that this rollback has already resolved.
            self.shared
                .cold
                .cmd_last_ack
                .fetch_add(drained, Ordering::Release);

            *self
                .shared
                .cold
                .param_rx
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(rx);
        }
        if let Some(state) = self.deactivated.take() {
            *self
                .shared
                .cold
                .deactivated_dsp
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clap::plugin::make_test_shared;

    #[test]
    fn test_defuse_success() {
        let shared = make_test_shared();
        let mut guard = ActivateRollbackGuard::new(&shared);
        let (_param_tx, param_rx) = rtrb::RingBuffer::new(8);
        let (gc_tx, _gc_rx) = rtrb::RingBuffer::new(8);
        let (_slimmable_tx, slimmable_rx) = rtrb::RingBuffer::new(8);

        guard.param_rx = Some(param_rx);
        guard.gc_tx = Some(gc_tx);
        guard.slimmable_rx = Some(slimmable_rx);

        let resources = guard.defuse();
        assert!(resources.is_ok());
    }

    #[test]
    fn test_defuse_missing_resource_returns_error() {
        let shared = make_test_shared();
        let guard = ActivateRollbackGuard::new(&shared);
        let result = guard.defuse();
        assert!(result.is_err());
    }

    #[test]
    fn test_drop_rollback_restores_all_resources() {
        let shared = make_test_shared();
        {
            let mut guard = ActivateRollbackGuard::new(&shared);
            let (_param_tx, param_rx) = rtrb::RingBuffer::new(8);
            let (gc_tx, _gc_rx) = rtrb::RingBuffer::new(8);
            let (_slimmable_tx, slimmable_rx) = rtrb::RingBuffer::new(8);

            guard.param_rx = Some(param_rx);
            guard.gc_tx = Some(gc_tx);
            guard.slimmable_rx = Some(slimmable_rx);
            guard.pending_restart_os_factor = Some(PendingRestartOs::Pending(
                neural_amp_modeler_rs::dsp::oversample::OversampleFactor::X4,
            ));

            // Simulating an error before defuse: guard dropped
        }

        // Verify that pending_restart_os_factor was restored (encoded).
        assert_eq!(
            PendingRestartOs::load(&shared.cold.pending_restart_os_factor, Ordering::Acquire,),
            PendingRestartOs::Pending(neural_amp_modeler_rs::dsp::oversample::OversampleFactor::X4)
        );
        // Verify channels are back in ColdShared
        assert!(shared.cold.param_rx.lock().unwrap().is_some());
        assert!(shared.cold.gc_tx.lock().unwrap().is_some());
        assert!(shared.cold.slimmable_rx.lock().unwrap().is_some());
    }

    #[test]
    fn test_rollback_replays_mixed_commands_and_keeps_newest_generation() {
        use crate::clap::plugin::shared::build_stream_adapter;
        use neural_amp_modeler_rs::common::params::RtProcessingParams;
        use neural_amp_modeler_rs::dsp::resampler::NamResampler;

        let shared = make_test_shared();
        let (mut param_tx, param_rx) = rtrb::RingBuffer::new(8);
        let (gc_tx, _gc_rx) = rtrb::RingBuffer::new(8);
        let (_slimmable_tx, slimmable_rx) = rtrb::RingBuffer::new(8);

        // A newer model is already deferred; the recovered ring carries an
        // older model, a params snapshot and an IR command in causal order.
        *shared.cold.pending_model.lock().unwrap() = Some(PendingModel {
            generation: 11,
            model: None,
            model_r: None,
            model_rate: 48000,
            input_mult_adj: 1.0,
            output_mult_adj: 1.0,
            cal_meta: Default::default(),
        });

        let model = |generation: u64, rate: u32| ClapParamPayload::LoadModel {
            generation,
            model_l: None,
            model_r: None,
            new_resampler: Box::new(NamResampler::new_simple(48000, rate).unwrap()),
            new_stream: build_stream_adapter(48000, rate, 256).unwrap(),
            model_rate: rate,
            input_mult_adj: 1.0,
            output_mult_adj: 1.0,
            cal_meta: Default::default(),
            prewarm_cost_ns: None,
        };
        param_tx
            .push(Box::new(ClapParamPayload::Params(
                RtProcessingParams::default(),
            )))
            .unwrap();
        param_tx.push(Box::new(model(10, 44100))).unwrap();
        param_tx
            .push(Box::new(ClapParamPayload::LoadCabIr { pair: None }))
            .unwrap();
        param_tx.push(Box::new(model(12, 44100))).unwrap();

        {
            let mut guard = ActivateRollbackGuard::new(&shared);
            guard.param_rx = Some(param_rx);
            guard.gc_tx = Some(gc_tx);
            guard.slimmable_rx = Some(slimmable_rx);
        }

        // Newest recovered generation wins and keeps the payload rate.
        let pending = shared.cold.pending_model.lock().unwrap();
        let pending = pending.as_ref().unwrap();
        assert_eq!(pending.generation, 12);
        assert_eq!(pending.model_rate, 44100);

        // Non-model commands are preserved in their original order for replay.
        let recovered = shared.cold.recovered_commands.lock().unwrap();
        assert_eq!(recovered.len(), 2);
        assert!(matches!(recovered[0].as_ref(), ClapParamPayload::Params(_)));
        assert!(matches!(
            recovered[1].as_ref(),
            ClapParamPayload::LoadCabIr { .. }
        ));

        // Every drained sequence is resolved so a main-thread ack wait cannot
        // stall on a command the rollback consumed.
        assert_eq!(shared.cold.cmd_last_ack.load(Ordering::Acquire), 4);
    }

    #[test]
    fn test_rollback_never_overwrites_newer_pending_generation() {
        use neural_amp_modeler_rs::dsp::resampler::NamResampler;

        let shared = make_test_shared();
        let (mut param_tx, param_rx) = rtrb::RingBuffer::new(8);
        let (gc_tx, _gc_rx) = rtrb::RingBuffer::new(8);
        let (_slimmable_tx, slimmable_rx) = rtrb::RingBuffer::new(8);

        *shared.cold.pending_model.lock().unwrap() = Some(PendingModel {
            generation: 99,
            model: None,
            model_r: None,
            model_rate: 48000,
            input_mult_adj: 1.0,
            output_mult_adj: 1.0,
            cal_meta: Default::default(),
        });
        param_tx
            .push(Box::new(ClapParamPayload::LoadModel {
                generation: 5,
                model_l: None,
                model_r: None,
                new_resampler: Box::new(NamResampler::new_simple(48000, 48000).unwrap()),
                new_stream: crate::clap::plugin::shared::build_stream_adapter(48000, 48000, 256)
                    .unwrap(),
                model_rate: 48000,
                input_mult_adj: 1.0,
                output_mult_adj: 1.0,
                cal_meta: Default::default(),
                prewarm_cost_ns: None,
            }))
            .unwrap();

        {
            let mut guard = ActivateRollbackGuard::new(&shared);
            guard.param_rx = Some(param_rx);
            guard.gc_tx = Some(gc_tx);
            guard.slimmable_rx = Some(slimmable_rx);
        }

        assert_eq!(
            shared
                .cold
                .pending_model
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .generation,
            99
        );
    }

    #[test]
    fn test_drop_rollback_drains_and_restores_pending_model_payload() {
        use crate::clap::plugin::shared::build_stream_adapter;
        use neural_amp_modeler_rs::dsp::resampler::NamResampler;

        let shared = make_test_shared();
        let (mut param_tx, param_rx) = rtrb::RingBuffer::new(8);
        let (gc_tx, _gc_rx) = rtrb::RingBuffer::new(8);
        let (_slimmable_tx, slimmable_rx) = rtrb::RingBuffer::new(8);

        let resampler = Box::new(NamResampler::new_linear_simple(48000, 48000).unwrap());
        let stream = build_stream_adapter(48000, 48000, 256).unwrap();

        let payload = ClapParamPayload::LoadModel {
            generation: 42,
            model_l: None,
            model_r: None,
            new_resampler: resampler,
            new_stream: stream,
            model_rate: 48000,
            input_mult_adj: 1.0,
            output_mult_adj: 1.0,
            cal_meta: Default::default(),
            prewarm_cost_ns: None,
        };
        let _ = param_tx.push(Box::new(payload));

        {
            let mut guard = ActivateRollbackGuard::new(&shared);
            guard.param_rx = Some(param_rx);
            guard.gc_tx = Some(gc_tx);
            guard.slimmable_rx = Some(slimmable_rx);
        }

        assert!(shared.cold.param_rx.lock().unwrap().is_some());
        let restored = shared.cold.pending_model.lock().unwrap().take();
        assert!(restored.is_some());
        assert_eq!(restored.unwrap().generation, 42);
    }
}
