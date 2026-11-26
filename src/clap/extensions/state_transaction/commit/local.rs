// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Pre-activate local commit path.

use super::*;

/// Pre-activate path (`buffer_size == 0`): there is no audio thread, so the
/// restore commits locally and atomically on the main thread. The model is
/// retained in `pending_model` for `flush_pending_model()` on `activate()`.
pub(super) fn local_commit(
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
        ir_level_policy,
        calibration,
        gate_mode,
        cab_bypass,
    } = validated;

    // ── Commit model (local) ──
    if let Some(resources) = model {
        let ModelResources {
            model_l,
            model_r,
            new_resampler: _,
            new_stream: _,
            input_mult_adj,
            output_mult_adj,
            cal_meta,
            buffer_size: _,
            model_rate,
            model_metadata,
            model_info,
            model_hash: _,
        } = resources;

        // Store full WaveNet weights for slimmable rebuild.
        *lock_cold(
            &main_thread.shared.cold.full_wavenet_model,
            "full_wavenet_model",
        ) = model_l.as_ref().and_then(|m| {
            if let StaticModel::WavenetDyn(w) = m.as_ref() {
                clone_wavenet_for_slimmable_storage(w).ok()
            } else {
                None
            }
        });

        *lock_cold(
            &main_thread.shared.cold.ui_model_metadata,
            "ui_model_metadata",
        ) = Some(model_metadata);
        *lock_cold(&main_thread.shared.cold.ui_model_info, "ui_model_info") = Some(model_info);
        let mem_bytes = model_l.as_ref().map(|m| m.memory_footprint()).unwrap_or(0);
        main_thread
            .shared
            .cold
            .ui_model_memory_bytes
            .store(mem_bytes, Ordering::Relaxed);

        *lock_cold(&main_thread.shared.cold.pending_model, "pending_model") = Some(PendingModel {
            generation: main_thread.shared.cold.allocate_model_generation(),
            model: model_l,
            model_r,
            model_rate,
            input_mult_adj,
            output_mult_adj,
            cal_meta,
        });
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
            *lock_cold(&main_thread.shared.cold.ui_model_name, "ui_model_name") = basename.clone();
            log::info!(
                "Model restored (local): {:?}",
                model_path_on_disk.as_deref().unwrap_or(Path::new(""))
            );
        }
    } else if let RestoreMode::Full = mode {
        // Explicitly clear model UI/ColdShared.
        *lock_cold(
            &main_thread.shared.cold.full_wavenet_model,
            "full_wavenet_model",
        ) = None;
        *lock_cold(
            &main_thread.shared.cold.ui_model_metadata,
            "ui_model_metadata",
        ) = None;
        *lock_cold(&main_thread.shared.cold.ui_model_info, "ui_model_info") = None;
        main_thread
            .shared
            .cold
            .ui_model_memory_bytes
            .store(0, Ordering::Relaxed);
        lock_cold(&main_thread.shared.cold.ui_model_name, "ui_model_name").clear();
        *lock_cold(&main_thread.shared.cold.pending_model, "pending_model") = Some(PendingModel {
            generation: main_thread.shared.cold.allocate_model_generation(),
            model: None,
            model_r: None,
            model_rate: 48000,
            input_mult_adj: 1.0,
            output_mult_adj: 1.0,
            cal_meta: crate::clap::extensions::calibration::ModelCalibrationMeta::empty(),
        });
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
    main_thread
        .shared
        .cold
        .ir_level_policy
        .store(ir_level_policy.to_u8(), Ordering::Relaxed);
    if let Some(resources) = ir {
        let IrResources {
            slot,
            kind,
            samples,
            sample_rate,
            nam_raw,
            ui_meta,
        } = resources;
        if let Some(slot) = slot {
            let mut staged_swap = main_thread.staged_swap.borrow_mut();
            let staged = staged_swap.get_or_insert_with(StagedSwap::default);
            staged.ir = Some(Some(slot));
        }
        *lock_cold(&main_thread.shared.cold.ir_path, "ir_path") = ir_path_on_disk.clone();
        *lock_cold(&main_thread.shared.cold.ir_hash, "ir_hash") = ir_hash.clone();
        main_thread
            .shared
            .cold
            .cab_kind
            .store(kind.to_u8(), Ordering::Relaxed);
        *lock_cold(
            &main_thread.shared.cold.cab_nam_raw_bytes,
            "cab_nam_raw_bytes",
        ) = nam_raw;
        *lock_cold(&main_thread.shared.cold.cab_ui_meta, "cab_ui_meta") = Some(ui_meta);
        if kind == CabAssetKind::WavIr {
            *lock_cold(&main_thread.shared.cold.ir_raw_samples, "ir_raw_samples") = Some(samples);
            main_thread
                .shared
                .cold
                .ir_raw_sample_rate
                .store(sample_rate, Ordering::Relaxed);
        } else {
            *lock_cold(&main_thread.shared.cold.ir_raw_samples, "ir_raw_samples") = None;
            main_thread
                .shared
                .cold
                .ir_raw_sample_rate
                .store(0, Ordering::Relaxed);
        }
        {
            let mut params = main_thread.params.borrow_mut();
            params.ir_path = ir_path_on_disk.map(PathBuf::from);
            params.ir_hash = ir_hash;
        }
    } else if let RestoreMode::Full = mode {
        let mut staged_swap = main_thread.staged_swap.borrow_mut();
        let staged = staged_swap.get_or_insert_with(StagedSwap::default);
        staged.ir = Some(None);
        *lock_cold(&main_thread.shared.cold.ir_path, "ir_path") = None;
        *lock_cold(&main_thread.shared.cold.ir_hash, "ir_hash") = None;
        main_thread.shared.cold.cab_kind.store(0, Ordering::Relaxed);
        *lock_cold(
            &main_thread.shared.cold.cab_nam_raw_bytes,
            "cab_nam_raw_bytes",
        ) = None;
        *lock_cold(&main_thread.shared.cold.cab_ui_meta, "cab_ui_meta") = None;
        *lock_cold(&main_thread.shared.cold.ir_raw_samples, "ir_raw_samples") = None;
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
    main_thread.shared.store_calibration_config(calibration);
    *lock_cold(
        &main_thread.shared.cold.in_flight_calibration,
        "in_flight_calibration",
    ) = None;
    main_thread.shared.store_gate_mode(gate_mode);
    *lock_cold(
        &main_thread.shared.cold.in_flight_gate_mode,
        "in_flight_gate_mode",
    ) = None;
    main_thread.shared.store_cab_bypass(cab_bypass);
    *lock_cold(
        &main_thread.shared.cold.in_flight_cab_bypass,
        "in_flight_cab_bypass",
    ) = None;
    main_thread.shared.bump_generation();

    if let Some(params_ext) = main_thread
        .host
        .get_extension::<clack_extensions::params::HostParams>()
    {
        params_ext.rescan(
            &main_thread.host,
            clack_extensions::params::ParamRescanFlags::VALUES
                | clack_extensions::params::ParamRescanFlags::TEXT,
        );
    }

    Ok(())
}
