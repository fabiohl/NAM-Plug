// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! State restoration commit phase: atomic publishing, SPSC delivery, and local staging.

use super::RestoreMode;
use super::validate::{IrResources, ModelResources, ValidatedRestore};
use crate::clap::extensions::cabinet::{CabAssetKind, CabSlotDsp};
use crate::clap::plugin::command_scheduler::PushError;
use crate::clap::plugin::errors::{self, static_plugin_error};
use crate::clap::plugin::lock::lock_cold;
pub(crate) use crate::clap::plugin::shared::StagedSwap;
use crate::clap::plugin::{
    ClapParamPayload, LoadModelPayload, NamClapMainThread, PendingModel, PendingRestartOs,
    PendingRestore, RestoreModelPublish, RestorePublish, RestoreTxn, StagedRestore,
};
use clack_plugin::prelude::*;
use neural_amp_modeler_rs::common::params::{ProcessingParams, RtProcessingParams};
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
pub(crate) use neural_amp_modeler_rs::models::NamModel;
use neural_amp_modeler_rs::models::StaticModel;
use neural_amp_modeler_rs::models::slimmable::clone_wavenet_for_slimmable_storage;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

mod local;
use local::local_commit;

/// Monotonic generation tag for restore transactions.
static NEXT_RESTORE_GENERATION: AtomicU64 = AtomicU64::new(0);

pub(crate) fn next_restore_generation() -> u64 {
    NEXT_RESTORE_GENERATION.fetch_add(1, Ordering::Relaxed) + 1
}

/// Publishes the validated restore transactionally. Only reached after all
/// validation passes.
///
/// - Active (`buffer_size > 0`): the whole package (model, IR, params) is pushed
///   as a single [`RestoreTxn`] command. UI/paths/hashes are published only after
///   the audio thread acks its sequence number (`flush_pending_restore`).
/// - Pre-activate (`buffer_size == 0`): local commit — no audio thread exists to
///   desync from, so everything is published immediately and the model is
///   retained in `pending_model` for `activate()`.
pub(crate) fn commit(
    validated: ValidatedRestore,
    main_thread: &NamClapMainThread,
    mode: &RestoreMode,
) -> Result<(), PluginError> {
    let buffer_size = main_thread.shared.cold.buffer_size.load(Ordering::Relaxed);
    if buffer_size == 0 {
        return local_commit(validated, main_thread, mode);
    }
    atomic_commit(validated, main_thread, mode)
}

/// Active path: builds the atomic [`RestoreTxn`] and either delivers it
/// continuously through SPSC (same physical latency) or stages it for the next
/// host restart cycle (different physical latency, Strict Restart Policy).
fn atomic_commit(
    validated: ValidatedRestore,
    main_thread: &NamClapMainThread,
    mode: &RestoreMode,
) -> Result<(), PluginError> {
    let host_rate = {
        let rate = main_thread.shared.cold.sample_rate.load(Ordering::Relaxed);
        if rate == 0 { 48000 } else { rate }
    };
    let buffer_size = main_thread.shared.cold.buffer_size.load(Ordering::Relaxed);
    let (publish, txn) = build_restore_package(
        validated,
        &main_thread.params.borrow(),
        host_rate,
        buffer_size,
        mode,
        &main_thread.shared.cold,
    )?;

    // Check if the restore changes physical latency (Strict Restart Policy).
    let current_stream_latency = main_thread
        .shared
        .cold
        .current_stream_latency
        .load(Ordering::Relaxed);
    let current_cabsim_latency = main_thread
        .shared
        .cold
        .current_cabsim_latency
        .load(Ordering::Relaxed);

    let stream_latency_differs = match txn.model.as_ref() {
        Some(m) => m.new_stream.latency_samples() != current_stream_latency,
        None => false,
    };

    let cabsim_latency_differs = match txn.ir.as_ref() {
        Some(maybe_slot) => {
            let new_lat = maybe_slot.as_ref().map_or(0, CabSlotDsp::latency_samples);
            new_lat != current_cabsim_latency
        }
        None => false,
    };

    let current_os = OversampleFactor::from_f32(
        main_thread
            .shared
            .ui_to_rt
            .param_oversample
            .load(Ordering::Relaxed) as f32,
    );
    let pending_os = PendingRestartOs::load(
        &main_thread.shared.cold.pending_restart_os_factor,
        Ordering::Relaxed,
    );
    let os_differs =
        publish.params.oversample != current_os || pending_os != PendingRestartOs::None;

    let latency_differs = stream_latency_differs || cabsim_latency_differs || os_differs;

    if !latency_differs {
        // Same latency ⇒ continuous atomic restore through SPSC (ack-gated).
        // Clear any superseded staged items on the main thread.
        *main_thread.staged_restore.borrow_mut() = None;
        if let Some(mut staged) = main_thread.staged_swap.borrow_mut().take() {
            staged.clear_model();
            staged.clear_ir();
        }

        let pending = PendingRestore {
            generation: txn.generation,
            txn: Some(txn),
            publish,
            seq: 0,
        };
        deliver_pending_restore(main_thread, pending);
    } else {
        // Different physical latency ⇒ Strict Restart Policy: stage entire package off-RT
        // and request host restart. Audio thread keeps running current state and reporting
        // current latency until activate() consumes this staged restore.
        if os_differs {
            PendingRestartOs::Pending(publish.params.oversample).store(
                &main_thread.shared.cold.pending_restart_os_factor,
                Ordering::Release,
            );
        }
        *main_thread.staged_swap.borrow_mut() = None;
        *main_thread.staged_restore.borrow_mut() = Some(StagedRestore { txn, publish });
        main_thread.host.request_restart();
    }

    Ok(())
}
mod build;
mod publish;

pub(crate) use build::{build_restore_package, deliver_pending_restore};
pub(crate) use publish::{note_model_delivered, publish_restore};

impl<'a> NamClapMainThread<'a> {
    /// Retries delivery of a pending restore transaction and, once the audio
    /// thread acks it, publishes the UI/paths/hashes (ack phase).
    ///
    /// Called from `housekeeping()`. Latest-wins: a newer restore replaces an
    /// older still-pending one; the older transaction already in the ring still
    /// applies atomically (a complete package), but its UI publication is
    /// superseded.
    ///
    /// Runs under `&self`; every `RefCell` borrow is released before the host
    /// `request_callback()` calls.
    pub(crate) fn flush_pending_restore(&self) {
        let Some(mut pending) = self.pending_restore.borrow_mut().take() else {
            return;
        };

        if let Some(txn) = pending.txn.take() {
            let push_result = self
                .cmd_producer
                .borrow_mut()
                .try_push_command(ClapParamPayload::RestoreTxn(txn));
            match push_result {
                Ok(seq) => {
                    pending.txn = None;
                    pending.seq = seq;
                    note_model_delivered(self, &pending.publish);
                }
                Err(err) => {
                    let (PushError::Full, payload) = *err;
                    if let ClapParamPayload::RestoreTxn(txn) = payload {
                        pending.txn = Some(txn);
                    }
                    *self.pending_restore.borrow_mut() = Some(pending);
                    self.host.request_callback();
                    return;
                }
            }
        }

        if pending.seq > 0 && self.cmd_producer.borrow().is_acked(pending.seq) {
            publish_restore(pending.publish, self);
        } else {
            *self.pending_restore.borrow_mut() = Some(pending);
            self.host.request_callback();
        }
    }
}
