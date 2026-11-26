// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

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
        ir_level_policy,
        calibration,
        gate_mode,
        cab_bypass,
        cab_kind,
        cab_nam_raw_bytes,
        cab_ui_meta,
    } = publish;
    let mode = if mode_full {
        RestoreMode::Full
    } else {
        RestoreMode::ForPreset
    };

    // ── Model publication (only after the audio thread applied the package) ──
    if let Some(model) = model {
        *lock_cold(
            &main_thread.shared.cold.full_wavenet_model,
            "full_wavenet_model",
        ) = model.full_wavenet;
        *lock_cold(
            &main_thread.shared.cold.ui_model_metadata,
            "ui_model_metadata",
        ) = Some(model.metadata);
        *lock_cold(&main_thread.shared.cold.ui_model_info, "ui_model_info") = Some(model.info);
        main_thread
            .shared
            .cold
            .ui_model_memory_bytes
            .store(model.memory_bytes, Ordering::Relaxed);
        main_thread
            .shared
            .cold
            .model_sample_rate
            .store(model.model_rate, Ordering::Relaxed);
        if let Some(ref basename) = model_basename {
            *lock_cold(&main_thread.shared.cold.ui_model_name, "ui_model_name") = basename.clone();
        }
        log::info!(
            "Model restored (ack): {:?}",
            model_path_on_disk.as_deref().unwrap_or(Path::new(""))
        );
    } else if mode_full {
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
    main_thread
        .shared
        .cold
        .ir_level_policy
        .store(ir_level_policy.to_u8(), Ordering::Relaxed);
    if let Some(ref ir_path_str) = ir_path_on_disk {
        *lock_cold(&main_thread.shared.cold.ir_path, "ir_path") = Some(ir_path_str.clone());
        *lock_cold(&main_thread.shared.cold.ir_hash, "ir_hash") = ir_hash.clone();
        main_thread
            .shared
            .cold
            .cab_kind
            .store(cab_kind.map_or(0, CabAssetKind::to_u8), Ordering::Relaxed);
        *lock_cold(
            &main_thread.shared.cold.cab_nam_raw_bytes,
            "cab_nam_raw_bytes",
        ) = cab_nam_raw_bytes;
        *lock_cold(&main_thread.shared.cold.cab_ui_meta, "cab_ui_meta") = cab_ui_meta;
        if cab_kind == Some(CabAssetKind::WavIr) {
            *lock_cold(&main_thread.shared.cold.ir_raw_samples, "ir_raw_samples") = ir_raw_samples;
            main_thread
                .shared
                .cold
                .ir_raw_sample_rate
                .store(ir_raw_sample_rate, Ordering::Relaxed);
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
            params.ir_path = Some(PathBuf::from(ir_path_str.clone()));
            params.ir_hash = ir_hash;
        }
    } else if mode_full {
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
}
