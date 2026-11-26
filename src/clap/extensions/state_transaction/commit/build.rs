// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

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
        ir_level_policy,
        calibration,
        gate_mode,
        cab_bypass,
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
        memory_bytes: r
            .model_l
            .as_ref()
            .map(|m| m.memory_footprint())
            .unwrap_or(0),
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
                mut model_l,
                mut model_r,
                new_resampler,
                new_stream,
                input_mult_adj,
                output_mult_adj,
                cal_meta,
                buffer_size,
                model_rate,
                model_metadata: _,
                model_info: _,
                model_hash: _,
            } = resources;
            // Off-RT cost calibration for the restored model.
            // Cap the block at the negotiated quantum so it never exceeds the
            // model's `max_buffer_size` (composed models truncate the block).
            let prewarm_cost_ns = if crate::clap::processor::prewarm_policy::ADAPTIVE_COST_ENABLED {
                let measure_frames = crate::clap::processor::prewarm_policy::PREWARM_MEASURE_FRAMES
                    .min((buffer_size as usize).max(1));
                model_l
                    .as_mut()
                    .map(|model| model.measure_cost_ns_per_sample(measure_frames))
            } else {
                None
            };
            crate::clap::plugin::shared::preinject_rt_status(
                &cold.rt_status,
                &mut model_l,
                &mut model_r,
            );
            Some(LoadModelPayload {
                generation: cold.allocate_model_generation(),
                model_l,
                model_r,
                new_resampler,
                new_stream,
                model_rate,
                input_mult_adj,
                output_mult_adj,
                cal_meta,
                prewarm_cost_ns,
            })
        }
        None => match mode {
            RestoreMode::Full => {
                // Explicitly clear the model on the RT thread.
                // Building the passthrough resampler is part of the transaction:
                // a failure aborts the entire commit — nothing is published.
                let new_resampler =
                    NamResampler::new_simple_typed(host_rate, 48000).map_err(|e| {
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
                    model_r: None,
                    new_resampler: Box::new(new_resampler),
                    new_stream,
                    model_rate: 48000,
                    input_mult_adj: 1.0,
                    output_mult_adj: 1.0,
                    cal_meta: crate::clap::extensions::calibration::ModelCalibrationMeta::empty(),
                    prewarm_cost_ns: None,
                })
            }
            RestoreMode::ForPreset => None,
        },
    };

    let (ir_publish, cab_kind_pub, cab_nam_raw_pub, cab_ui_meta_pub, ir_component) = match ir {
        Some(resources) => {
            let IrResources {
                slot,
                kind,
                samples,
                sample_rate,
                nam_raw,
                ui_meta,
            } = resources;
            let wav_pub = if kind == CabAssetKind::WavIr {
                Some((samples, sample_rate))
            } else {
                None
            };
            (wav_pub, Some(kind), nam_raw, Some(ui_meta), Some(slot))
        }
        None => match mode {
            RestoreMode::Full => (None, None, None, None, Some(None)),
            RestoreMode::ForPreset => (None, None, None, None, None),
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
        ir_level_policy,
        calibration,
        gate_mode,
        cab_bypass,
        cab_kind: cab_kind_pub,
        cab_nam_raw_bytes: cab_nam_raw_pub,
        cab_ui_meta: cab_ui_meta_pub,
    };

    let txn = RestoreTxn {
        generation: next_restore_generation(),
        model: model_component,
        ir: ir_component,
        params: RtProcessingParams::from_processing_params(&effective_params),
        calibration,
        gate_mode,
        cab_bypass,
    };

    Ok((publish, txn))
}

/// Attempts the first `try_push_command` of the transaction. On `Full` the whole
/// package (txn + publish payload) is retained in `pending_restore` for retry.
///
/// Runs under `&NamClapMainThread`: the `pending_restore` and `cmd_producer`
/// borrows are scoped and released before the host `request_callback()`.
pub(crate) fn deliver_pending_restore(
    main_thread: &NamClapMainThread,
    mut pending: PendingRestore,
) {
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
            Err(err) => {
                let (PushError::Full, payload) = *err;
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
