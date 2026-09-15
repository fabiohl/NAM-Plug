// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! State restoration commit phase: atomic publishing, SPSC delivery, and local staging.

use super::RestoreMode;
use super::validate::{IrResources, ModelResources, ValidatedRestore};
use crate::clap::plugin::command_scheduler::PushError;
use crate::clap::plugin::errors::{self, static_plugin_error};
use crate::clap::plugin::shared::StagedSwap;
use crate::clap::plugin::{
    ClapParamPayload, LoadModelPayload, NamClapMainThread, PendingModel, PendingRestartOs,
    PendingRestore, RestoreModelPublish, RestorePublish, RestoreTxn, StagedRestore,
};
use clack_plugin::prelude::*;
use neural_amp_modeler_rs::common::params::{ProcessingParams, RtProcessingParams};
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::models::StaticModel;
use neural_amp_modeler_rs::models::slimmable::clone_wavenet_for_slimmable_storage;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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
        Some(maybe_adapter) => {
            let new_lat = maybe_adapter
                .as_ref()
                .map_or(0, |a| a.latency_samples() as u32);
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

/// Maps the validated resources onto a [`RestorePublish`] (publication payload,
/// applied only on ack) and the atomic [`RestoreTxn`] (audio-thread package).
pub(crate) fn build_restore_package(
    validated: ValidatedRestore,
    current_params: &ProcessingParams,
    host_rate: u32,
    buffer_size: u32,
    mode: &RestoreMode,
    cold: &crate::clap::plugin::shared::ColdShared,
) -> Result<(RestorePublish, RestoreTxn), PluginError> {
    let ValidatedRestore {
        params: validated_params,
        model,
        model_path_on_disk,
        model_basename,
        model_search_path_to_add,
        model_hash,
        ir,
        ir_path_on_disk,
        ir_hash,
    } = validated;

    // Effective params: Full replaces everything; ForPreset only the
    // preset-identity subset (oversample + activation_precision included).
    let effective_params = match mode {
        RestoreMode::Full => validated_params.clone(),
        RestoreMode::ForPreset => {
            let mut p = current_params.clone();
            p.input_gain_db = validated_params.input_gain_db;
            p.output_gain_db = validated_params.output_gain_db;
            p.gate_threshold_db = validated_params.gate_threshold_db;
            p.bypass = validated_params.bypass;
            p.adaptive_compute = validated_params.adaptive_compute;
            p.slim_override = validated_params.slim_override;
            p.oversample = validated_params.oversample;
            p.activation_precision = validated_params.activation_precision;
            p
        }
    };

    // Publication metadata is cloned before the heavy resources are moved into
    // the transaction payload.
    let model_publish = model.as_ref().map(|r| RestoreModelPublish {
        metadata: r.model_metadata.clone(),
        info: r.model_info.clone(),
        full_wavenet: r.model_l.as_ref().and_then(|m| {
            if let StaticModel::WavenetDyn(w) = m.as_ref() {
                clone_wavenet_for_slimmable_storage(w).ok()
            } else {
                None
            }
        }),
        model_rate: r.model_rate,
    });

    let model_component = match model {
        Some(resources) => {
            let ModelResources {
                model_l,
                new_resampler,
                new_stream,
                input_mult_adj,
                output_mult_adj,
                model_rate: _,
                model_metadata: _,
                model_info: _,
                model_hash: _,
            } = resources;
            Some(LoadModelPayload {
                generation: cold.allocate_model_generation(),
                model_l,
                new_resampler,
                new_stream,
                input_mult_adj,
                output_mult_adj,
            })
        }
        None => match mode {
            RestoreMode::Full => {
                // Explicitly clear the model on the RT thread.
                // Building the passthrough resampler is part of the transaction:
                // a failure aborts the entire commit — nothing is published.
                let new_resampler = NamResampler::new(host_rate, 48000, 0).map_err(|e| {
                    static_plugin_error(
                        errors::dsp_resources::CLEAR_MODEL_RESAMPLER_FAILED,
                        format_args!("{e:?}"),
                    )
                })?;
                let new_stream = crate::clap::plugin::build_stream_adapter(
                    host_rate,
                    48000,
                    buffer_size.max(1) as usize,
                )
                .map_err(|e| {
                    static_plugin_error(
                        errors::dsp_resources::CLEAR_MODEL_STREAM_FAILED,
                        format_args!("{e:?}"),
                    )
                })?;
                Some(LoadModelPayload {
                    generation: cold.allocate_model_generation(),
                    model_l: None,
                    new_resampler: Box::new(new_resampler),
                    new_stream,
                    input_mult_adj: 1.0,
                    output_mult_adj: 1.0,
                })
            }
            RestoreMode::ForPreset => None,
        },
    };

    let ir_publish = ir.as_ref().map(|r| (r.samples.clone(), r.sample_rate));
    let ir_component = match ir {
        Some(resources) => {
            let IrResources {
                adapter,
                samples: _,
                sample_rate: _,
            } = resources;
            // `adapter` is already `Option<Box<CabSimAdapter>>` — None clears
            // the IR. The box preserves the RT-safe end-to-end ownership
            // contract.
            Some(adapter)
        }
        None => match mode {
            RestoreMode::Full => Some(None),
            RestoreMode::ForPreset => None,
        },
    };

    let publish = RestorePublish {
        mode_full: matches!(mode, RestoreMode::Full),
        params: effective_params.clone(),
        model: model_publish,
        model_path_on_disk,
        model_basename,
        model_hash,
        model_search_path_to_add,
        ir_path_on_disk,
        ir_raw_samples: ir_publish.as_ref().map(|(s, _)| s.clone()),
        ir_raw_sample_rate: ir_publish.as_ref().map(|(_, r)| *r).unwrap_or(0),
        ir_hash,
    };

    let txn = RestoreTxn {
        generation: next_restore_generation(),
        model: model_component,
        ir: ir_component,
        params: RtProcessingParams::from_processing_params(&effective_params),
    };

    Ok((publish, txn))
}

/// Attempts the first `try_push_command` of the transaction. On `Full` the whole
/// package (txn + publish payload) is retained in `pending_restore` for retry.
///
/// Runs under `&NamClapMainThread`: the `pending_restore` and `cmd_producer`
/// borrows are scoped and released before the host `request_callback()`.
fn deliver_pending_restore(main_thread: &NamClapMainThread, mut pending: PendingRestore) {
    if let Some(txn) = pending.txn.take() {
        let push_result = main_thread
            .cmd_producer
            .borrow_mut()
            .try_push_command(ClapParamPayload::RestoreTxn(txn));
        match push_result {
            Ok(seq) => {
                pending.txn = None;
                pending.seq = seq;
                note_model_delivered(main_thread, &pending.publish);
            }
            Err((PushError::Full, payload)) => {
                if let ClapParamPayload::RestoreTxn(txn) = payload {
                    pending.txn = Some(txn);
                }
                *main_thread.pending_restore.borrow_mut() = Some(pending);
                main_thread.host.request_callback();
                return;
            }
        }
    }
    // Pushed (or already pushed earlier): retain until the audio thread acks.
    *main_thread.pending_restore.borrow_mut() = Some(pending);
    main_thread.host.request_callback();
}

/// Advances the telemetry load counter once a restore carrying a model has been
/// delivered to the audio thread. Kept at delivery (push) time
/// so synchronous callers observe the counter advance without
/// waiting for the ack; UI/path/hash publication still waits for the ack.
pub(crate) fn note_model_delivered(main_thread: &NamClapMainThread, publish: &RestorePublish) {
    if publish.model.is_some() {
        main_thread
            .shared
            .cold
            .model_load_counter
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// Publishes UI/paths/hashes for a restore transaction once the audio thread has
/// applied it (ack phase), or immediately in a pre-activate local commit.
///
/// Runs under `&NamClapMainThread`; every `RefCell` borrow is scoped and never
/// held across a host call (`mark_dirty` / `rescan`).
pub(crate) fn publish_restore(publish: RestorePublish, main_thread: &NamClapMainThread) {
    let RestorePublish {
        mode_full,
        params,
        model,
        model_path_on_disk,
        model_basename,
        model_hash,
        model_search_path_to_add,
        ir_path_on_disk,
        ir_raw_samples,
        ir_raw_sample_rate,
        ir_hash,
    } = publish;
    let mode = if mode_full {
        RestoreMode::Full
    } else {
        RestoreMode::ForPreset
    };

    // ── Model publication (only after the audio thread applied the package) ──
    if let Some(model) = model {
        if let Ok(mut storage) = main_thread.shared.cold.full_wavenet_model.lock() {
            *storage = model.full_wavenet;
        }
        if let Ok(mut meta_guard) = main_thread.shared.cold.ui_model_metadata.lock() {
            *meta_guard = Some(model.metadata);
        }
        if let Ok(mut info_guard) = main_thread.shared.cold.ui_model_info.lock() {
            *info_guard = Some(model.info);
        }
        main_thread
            .shared
            .cold
            .model_sample_rate
            .store(model.model_rate, Ordering::Relaxed);
        if let Some(ref basename) = model_basename
            && let Ok(mut name_guard) = main_thread.shared.cold.ui_model_name.lock()
        {
            *name_guard = basename.clone();
        }
        log::info!(
            "Model restored (ack): {:?}",
            model_path_on_disk.as_deref().unwrap_or(Path::new(""))
        );
    } else if mode_full {
        // Explicitly clear model UI/ColdShared.
        if let Ok(mut storage) = main_thread.shared.cold.full_wavenet_model.lock() {
            *storage = None;
        }
        if let Ok(mut meta_guard) = main_thread.shared.cold.ui_model_metadata.lock() {
            *meta_guard = None;
        }
        if let Ok(mut info_guard) = main_thread.shared.cold.ui_model_info.lock() {
            *info_guard = None;
        }
        if let Ok(mut name_guard) = main_thread.shared.cold.ui_model_name.lock() {
            name_guard.clear();
        }
        main_thread
            .shared
            .cold
            .model_sample_rate
            .store(48000, Ordering::Relaxed);
    }

    if let Some(state_ext) = main_thread
        .host
        .get_extension::<clack_extensions::state::HostState>()
    {
        state_ext.mark_dirty(&main_thread.host);
    }

    // ── IR publication (only after the audio thread applied the package) ──
    if let Some(ref ir_path_str) = ir_path_on_disk {
        if let Ok(mut ir_guard) = main_thread.shared.cold.ir_path.lock() {
            *ir_guard = Some(ir_path_str.clone());
        }
        if let Ok(mut hash_guard) = main_thread.shared.cold.ir_hash.lock() {
            *hash_guard = ir_hash.clone();
        }
        if let Ok(mut raw_guard) = main_thread.shared.cold.ir_raw_samples.lock() {
            *raw_guard = ir_raw_samples;
        }
        main_thread
            .shared
            .cold
            .ir_raw_sample_rate
            .store(ir_raw_sample_rate, Ordering::Relaxed);
        {
            let mut params = main_thread.params.borrow_mut();
            params.ir_path = Some(PathBuf::from(ir_path_str.clone()));
            params.ir_hash = ir_hash;
        }
    } else if mode_full {
        if let Ok(mut ir_guard) = main_thread.shared.cold.ir_path.lock() {
            *ir_guard = None;
        }
        if let Ok(mut hash_guard) = main_thread.shared.cold.ir_hash.lock() {
            *hash_guard = None;
        }
        if let Ok(mut raw_guard) = main_thread.shared.cold.ir_raw_samples.lock() {
            *raw_guard = None;
        }
        main_thread
            .shared
            .cold
            .ir_raw_sample_rate
            .store(0, Ordering::Relaxed);
        {
            let mut params = main_thread.params.borrow_mut();
            params.ir_path = None;
            params.ir_hash = None;
        }
    }

    // ── Params publication ──
    // Single scoped borrow: no host call happens while it is held.
    {
        let mut mt_params = main_thread.params.borrow_mut();
        match mode {
            RestoreMode::Full => {
                *mt_params = params;
            }
            RestoreMode::ForPreset => {
                mt_params.input_gain_db = params.input_gain_db;
                mt_params.output_gain_db = params.output_gain_db;
                mt_params.gate_threshold_db = params.gate_threshold_db;
                mt_params.bypass = params.bypass;
                mt_params.adaptive_compute = params.adaptive_compute;
                mt_params.slim_override = params.slim_override;
                mt_params.oversample = params.oversample;
                mt_params.activation_precision = params.activation_precision;
            }
        }

        if mode_full {
            mt_params.model_path = model_path_on_disk;
            mt_params.model_basename = model_basename;
            mt_params.model_hash = model_hash;
            if let Some(search_path) = model_search_path_to_add
                && !mt_params.model_search_paths.contains(&search_path)
            {
                mt_params.model_search_paths.push(search_path);
            }
        }

        // ── Publish params to RT atomics ──
        use crate::clap::extensions::params::bypass_bool_to_u32;
        main_thread
            .shared
            .ui_to_rt
            .param_input_gain
            .store(mt_params.input_gain_db.to_bits(), Ordering::Relaxed);
        main_thread
            .shared
            .ui_to_rt
            .param_output_gain
            .store(mt_params.output_gain_db.to_bits(), Ordering::Relaxed);
        main_thread
            .shared
            .ui_to_rt
            .param_gate_thresh
            .store(mt_params.gate_threshold_db.to_bits(), Ordering::Relaxed);
        main_thread
            .shared
            .ui_to_rt
            .param_bypass
            .store(bypass_bool_to_u32(mt_params.bypass), Ordering::Relaxed);
        main_thread
            .shared
            .ui_to_rt
            .param_adaptive_compute
            .store(mt_params.adaptive_compute as u32, Ordering::Relaxed);
        main_thread
            .shared
            .ui_to_rt
            .param_slim_override
            .store(mt_params.slim_override as u32, Ordering::Relaxed);
        main_thread
            .shared
            .ui_to_rt
            .param_oversample
            .store(mt_params.oversample.to_f32() as u32, Ordering::Relaxed);
        main_thread
            .shared
            .ui_to_rt
            .param_activation
            .store(mt_params.activation_precision as u32, Ordering::Relaxed);
    }
    main_thread.shared.bump_generation();

    if let Some(params_ext) = main_thread
        .host
        .get_extension::<clack_extensions::params::HostParams>()
    {
        params_ext.rescan(
            &main_thread.host,
            clack_extensions::params::ParamRescanFlags::VALUES,
        );
    }
}

/// Pre-activate path (`buffer_size == 0`): there is no audio thread, so the
/// restore commits locally and atomically on the main thread. The model is
/// retained in `pending_model` for `flush_pending_model()` on `activate()`.
fn local_commit(
    validated: ValidatedRestore,
    main_thread: &NamClapMainThread,
    mode: &RestoreMode,
) -> Result<(), PluginError> {
    let ValidatedRestore {
        params: validated_params,
        model,
        model_path_on_disk,
        model_basename,
        model_search_path_to_add,
        model_hash,
        ir,
        ir_path_on_disk,
        ir_hash,
    } = validated;

    // ── Commit model (local) ──
    if let Some(resources) = model {
        let ModelResources {
            model_l,
            new_resampler: _,
            new_stream: _,
            input_mult_adj,
            output_mult_adj,
            model_rate,
            model_metadata,
            model_info,
            model_hash: _,
        } = resources;

        // Store full WaveNet weights for slimmable rebuild.
        {
            let mut storage = main_thread
                .shared
                .cold
                .full_wavenet_model
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *storage = model_l.as_ref().and_then(|m| {
                if let StaticModel::WavenetDyn(w) = m.as_ref() {
                    clone_wavenet_for_slimmable_storage(w).ok()
                } else {
                    None
                }
            });
        }

        if let Ok(mut meta_guard) = main_thread.shared.cold.ui_model_metadata.lock() {
            *meta_guard = Some(model_metadata);
        }
        if let Ok(mut info_guard) = main_thread.shared.cold.ui_model_info.lock() {
            *info_guard = Some(model_info);
        }

        if let Ok(mut pending_guard) = main_thread.shared.cold.pending_model.lock() {
            *pending_guard = Some(PendingModel {
                generation: main_thread.shared.cold.allocate_model_generation(),
                model: model_l,
                model_rate,
                input_mult_adj,
                output_mult_adj,
            });
        }
        main_thread
            .shared
            .cold
            .model_load_counter
            .fetch_add(1, Ordering::Relaxed);
        main_thread
            .shared
            .cold
            .model_sample_rate
            .store(model_rate, Ordering::Relaxed);

        if let Some(ref basename) = model_basename {
            if let Ok(mut name_guard) = main_thread.shared.cold.ui_model_name.lock() {
                *name_guard = basename.clone();
            }
            log::info!(
                "Model restored (local): {:?}",
                model_path_on_disk.as_deref().unwrap_or(Path::new(""))
            );
        }
    } else if let RestoreMode::Full = mode {
        // Explicitly clear model UI/ColdShared.
        if let Ok(mut storage) = main_thread.shared.cold.full_wavenet_model.lock() {
            *storage = None;
        }
        if let Ok(mut meta_guard) = main_thread.shared.cold.ui_model_metadata.lock() {
            *meta_guard = None;
        }
        if let Ok(mut info_guard) = main_thread.shared.cold.ui_model_info.lock() {
            *info_guard = None;
        }
        if let Ok(mut name_guard) = main_thread.shared.cold.ui_model_name.lock() {
            name_guard.clear();
        }
        if let Ok(mut pending_guard) = main_thread.shared.cold.pending_model.lock() {
            *pending_guard = Some(PendingModel {
                generation: main_thread.shared.cold.allocate_model_generation(),
                model: None,
                model_rate: 48000,
                input_mult_adj: 1.0,
                output_mult_adj: 1.0,
            });
        }
        main_thread
            .shared
            .cold
            .model_load_counter
            .fetch_add(1, Ordering::Relaxed);
        main_thread
            .shared
            .cold
            .model_sample_rate
            .store(48000, Ordering::Relaxed);
    }

    if let Some(state_ext) = main_thread
        .host
        .get_extension::<clack_extensions::state::HostState>()
    {
        state_ext.mark_dirty(&main_thread.host);
    }

    // ── Commit IR (local) ──
    if let Some(resources) = ir {
        let IrResources {
            adapter,
            samples,
            sample_rate,
        } = resources;
        if let Some(adapter) = adapter {
            let mut staged_swap = main_thread.staged_swap.borrow_mut();
            let staged = staged_swap.get_or_insert_with(StagedSwap::default);
            staged.ir = Some(Some(adapter));
        }
        if let Ok(mut ir_guard) = main_thread.shared.cold.ir_path.lock() {
            *ir_guard = ir_path_on_disk.clone();
        }
        if let Ok(mut hash_guard) = main_thread.shared.cold.ir_hash.lock() {
            *hash_guard = ir_hash.clone();
        }
        if let Ok(mut raw_guard) = main_thread.shared.cold.ir_raw_samples.lock() {
            *raw_guard = Some(samples);
        }
        main_thread
            .shared
            .cold
            .ir_raw_sample_rate
            .store(sample_rate, Ordering::Relaxed);
        {
            let mut params = main_thread.params.borrow_mut();
            params.ir_path = ir_path_on_disk.map(PathBuf::from);
            params.ir_hash = ir_hash;
        }
    } else if let RestoreMode::Full = mode {
        let mut staged_swap = main_thread.staged_swap.borrow_mut();
        let staged = staged_swap.get_or_insert_with(StagedSwap::default);
        staged.ir = Some(None);
        if let Ok(mut ir_guard) = main_thread.shared.cold.ir_path.lock() {
            *ir_guard = None;
        }
        if let Ok(mut hash_guard) = main_thread.shared.cold.ir_hash.lock() {
            *hash_guard = None;
        }
        if let Ok(mut raw_guard) = main_thread.shared.cold.ir_raw_samples.lock() {
            *raw_guard = None;
        }
        main_thread
            .shared
            .cold
            .ir_raw_sample_rate
            .store(0, Ordering::Relaxed);
        {
            let mut params = main_thread.params.borrow_mut();
            params.ir_path = None;
            params.ir_hash = None;
        }
    }

    // ── Commit parameters (local) ──
    {
        let mut params = main_thread.params.borrow_mut();
        match mode {
            RestoreMode::Full => {
                *params = validated_params.clone();
            }
            RestoreMode::ForPreset => {
                params.input_gain_db = validated_params.input_gain_db;
                params.output_gain_db = validated_params.output_gain_db;
                params.gate_threshold_db = validated_params.gate_threshold_db;
                params.bypass = validated_params.bypass;
                params.adaptive_compute = validated_params.adaptive_compute;
                params.slim_override = validated_params.slim_override;
                // Oversample and activation_precision are part of the preset identity
                params.oversample = validated_params.oversample;
                params.activation_precision = validated_params.activation_precision;
            }
        }

        if let RestoreMode::Full = mode {
            params.model_path = model_path_on_disk;
            params.model_basename = model_basename;
            params.model_hash = model_hash;
            if let Some(search_path) = model_search_path_to_add
                && !params.model_search_paths.contains(&search_path)
            {
                params.model_search_paths.push(search_path);
            }
        }

        // ── Publish params to RT atomics (no SPSC push: no audio thread yet) ──
        use crate::clap::extensions::params::bypass_bool_to_u32;
        main_thread
            .shared
            .ui_to_rt
            .param_input_gain
            .store(params.input_gain_db.to_bits(), Ordering::Relaxed);
        main_thread
            .shared
            .ui_to_rt
            .param_output_gain
            .store(params.output_gain_db.to_bits(), Ordering::Relaxed);
        main_thread
            .shared
            .ui_to_rt
            .param_gate_thresh
            .store(params.gate_threshold_db.to_bits(), Ordering::Relaxed);
        main_thread
            .shared
            .ui_to_rt
            .param_bypass
            .store(bypass_bool_to_u32(params.bypass), Ordering::Relaxed);
        main_thread
            .shared
            .ui_to_rt
            .param_adaptive_compute
            .store(params.adaptive_compute as u32, Ordering::Relaxed);
        main_thread
            .shared
            .ui_to_rt
            .param_slim_override
            .store(params.slim_override as u32, Ordering::Relaxed);
        main_thread
            .shared
            .ui_to_rt
            .param_oversample
            .store(params.oversample.to_f32() as u32, Ordering::Relaxed);
        main_thread
            .shared
            .ui_to_rt
            .param_activation
            .store(params.activation_precision as u32, Ordering::Relaxed);
    }
    main_thread.shared.bump_generation();

    if let Some(params_ext) = main_thread
        .host
        .get_extension::<clack_extensions::params::HostParams>()
    {
        params_ext.rescan(
            &main_thread.host,
            clack_extensions::params::ParamRescanFlags::VALUES,
        );
    }

    Ok(())
}

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
                Err((PushError::Full, payload)) => {
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
