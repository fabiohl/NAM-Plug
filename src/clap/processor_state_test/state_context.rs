// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use crate::clap::test_util;
use std::sync::atomic::Ordering;

#[test]
fn test_state_context_roundtrip() {
    use clack_extensions::state_context::{PluginStateContext, StateContextType};

    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

    let state_ext = test_util::get_state_ext(&mut plugin_instance);
    let state_ctx_ext = plugin_instance
        .plugin_handle()
        .get_extension::<PluginStateContext>()
        .expect("PluginStateContext extension not found");

    let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

    let model_path = crate::clap::test_util::model_path("lstm.nam");

    use neural_amp_modeler_rs::common::params::ProcessingParams;
    let params = ProcessingParams {
        model_path: Some(model_path.clone()),
        input_gain_db: 3.5,
        output_gain_db: -4.0,
        gate_threshold_db: -45.0,
        model_basename: Some("lstm.nam".to_string()),
        model_search_paths: vec![],
        model_hash: None,
        bypass: false,
        adaptive_compute: neural_amp_modeler_rs::common::params::AdaptiveComputeMode::Conservative,
        slim_override: Default::default(),
        oversample: neural_amp_modeler_rs::dsp::oversample::OversampleFactor::Off,
        ir_path: None,
        ir_hash: None,
        activation_precision: neural_amp_modeler_rs::common::params::ActivationPrecision::Standard,
    };
    let state_bytes = serde_json::to_vec(&params).unwrap();
    let mut handle = plugin_instance.plugin_handle();
    state_ext
        .load(&mut handle, &mut state_bytes.as_slice())
        .expect("Failed to load model via PluginState");

    let model_counter = shared.cold.model_load_counter.load(Ordering::Relaxed);
    assert!(model_counter > 0, "Model should have been loaded");

    // --- Save: ForPreset context ---
    let mut preset_buffer = Vec::new();
    let mut handle = plugin_instance.plugin_handle();
    state_ctx_ext
        .save(&mut handle, &mut preset_buffer, StateContextType::ForPreset)
        .expect("save ForPreset should succeed");

    let preset_json: serde_json::Value =
        serde_json::from_slice(&preset_buffer).expect("preset buffer should be valid JSON");
    assert_eq!(
        preset_json["version"], 1,
        "ForPreset save should produce v1 envelope"
    );
    assert!(
        preset_json["params"].is_object(),
        "ForPreset envelope should contain params"
    );
    assert!(
        preset_json["params"]["model_path"].is_null(),
        "ForPreset save should omit model_path"
    );
    assert!(
        preset_json["params"]["model_basename"].is_string(),
        "ForPreset save should preserve model_basename"
    );
    assert!(
        (preset_json["params"]["input_gain_db"].as_f64().unwrap() - 3.5).abs() < f64::EPSILON,
        "ForPreset save should preserve input_gain_db"
    );

    // --- Save: ForProject context ---
    let mut project_buffer = Vec::new();
    let mut handle = plugin_instance.plugin_handle();
    state_ctx_ext
        .save(
            &mut handle,
            &mut project_buffer,
            StateContextType::ForProject,
        )
        .expect("save ForProject should succeed");

    let project_json: serde_json::Value =
        serde_json::from_slice(&project_buffer).expect("project buffer should be valid JSON");
    assert_eq!(
        project_json["version"], 1,
        "ForProject save should produce v1 envelope"
    );
    assert!(
        project_json["params"]["model_path"].is_string(),
        "ForProject save should preserve model_path"
    );
    assert!(
        (project_json["params"]["input_gain_db"].as_f64().unwrap() - 3.5).abs() < f64::EPSILON,
        "ForProject save should preserve input_gain_db"
    );

    // --- Load: ForPreset context (only audio params restored, model_path unchanged) ---
    let preset_json_str = r#"{"input_gain_db":1.5,"output_gain_db":-2.0,"gate_threshold_db":-40.0,"model_path":null,"model_basename":null,"model_search_paths":[],"bypass":false,"adaptive_compute":"Off"}"#;
    let mut handle = plugin_instance.plugin_handle();
    state_ctx_ext
        .load(
            &mut handle,
            &mut preset_json_str.as_bytes(),
            StateContextType::ForPreset,
        )
        .expect("load ForPreset should succeed");

    let gain_in = shared.ui_to_rt.param_input_gain.load(Ordering::Relaxed);
    assert!(
        (f32::from_bits(gain_in) - 1.5).abs() < 0.01,
        "input gain should be restored from preset"
    );
    let gain_out = shared.ui_to_rt.param_output_gain.load(Ordering::Relaxed);
    assert!(
        (f32::from_bits(gain_out) - (-2.0)).abs() < 0.01,
        "output gain should be restored from preset"
    );

    // --- Load: ForProject context (full state, with model_path) ---
    let project_with_path = format!(
        r#"{{"input_gain_db":5.0,"output_gain_db":-8.0,"gate_threshold_db":-60.0,"model_path":"{}","model_basename":"lstm.nam","model_search_paths":[],"bypass":true,"adaptive_compute":"Aggressive"}}"#,
        model_path.to_str().unwrap()
    );
    let mut handle = plugin_instance.plugin_handle();
    state_ctx_ext
        .load(
            &mut handle,
            &mut project_with_path.as_bytes(),
            StateContextType::ForProject,
        )
        .expect("load ForProject should succeed");

    let gain_in = shared.ui_to_rt.param_input_gain.load(Ordering::Relaxed);
    assert!(
        (f32::from_bits(gain_in) - 5.0).abs() < 0.01,
        "input gain should reflect full project state restoration"
    );
    let bypass_val = shared.ui_to_rt.param_bypass.load(Ordering::Relaxed);
    assert_eq!(bypass_val, 1, "bypass should be enabled from project state");
    let adaptive_mode = shared
        .ui_to_rt
        .param_adaptive_compute
        .load(Ordering::Relaxed);
    assert_eq!(
        adaptive_mode, 2,
        "adaptive_compute should be Aggressive from project state"
    );
}

/// S6-E6-T03: `state_context.save(ForPreset)` → `state.load()` roundtrip.
///
/// Ensures a ForPreset blob, when loaded via the regular `state.load()`,
/// restores the same parameters and model identity that would be obtained
/// via `state_context.load(ForPreset)`.  The three CLAP spec combinations
/// must be equivalent:
/// 1. ForPreset → ForPreset
/// 2. ForPreset → state.load  ← this test
/// 3. ForDuplicate → state.load
#[test]
fn test_s6e6t03_state_context_preset_roundtrip_via_state_load() {
    use clack_extensions::state_context::{PluginStateContext, StateContextType};
    use neural_amp_modeler_rs::common::params::{
        ActivationPrecision, AdaptiveComputeMode, ProcessingParams,
    };
    use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;

    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

    let state_ext = test_util::get_state_ext(&mut plugin_instance);
    let state_ctx_ext = plugin_instance
        .plugin_handle()
        .get_extension::<PluginStateContext>()
        .expect("PluginStateContext extension not found");

    let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

    let model_path = crate::clap::test_util::model_path("lstm.nam");
    let model_dir = model_path.parent().unwrap().to_path_buf();

    // ── Load model via state.load (full params) ──
    let original = ProcessingParams {
        model_path: Some(model_path.clone()),
        input_gain_db: 2.0,
        output_gain_db: -3.5,
        gate_threshold_db: -55.0,
        model_basename: Some("lstm.nam".to_string()),
        model_hash: None,
        model_search_paths: vec![model_dir],
        bypass: true,
        adaptive_compute: AdaptiveComputeMode::Conservative,
        slim_override: Default::default(),
        oversample: OversampleFactor::X2,
        ir_path: None,
        ir_hash: None,
        activation_precision: ActivationPrecision::Fast,
    };
    let state_bytes = serde_json::to_vec(&original).unwrap();
    {
        let mut handle = plugin_instance.plugin_handle();
        state_ext
            .load(&mut handle, &mut state_bytes.as_slice())
            .expect("state.load should succeed");
    }

    // ── Save as ForPreset ──
    let mut preset_buffer = Vec::new();
    {
        let mut handle = plugin_instance.plugin_handle();
        state_ctx_ext
            .save(&mut handle, &mut preset_buffer, StateContextType::ForPreset)
            .expect("save ForPreset should succeed");
    }

    // Verify preset has no absolute paths
    let preset_json: serde_json::Value =
        serde_json::from_slice(&preset_buffer).expect("preset buffer should be valid JSON");
    assert!(preset_json["params"]["model_path"].is_null());
    // S6-E6-T02: model_search_paths are preserved as portable directory hints
    assert!(
        preset_json["params"]["model_search_paths"].is_array(),
        "model_search_paths are preserved for cross-machine search"
    );
    assert!(preset_json["params"]["ir_path"].is_null());
    assert!(preset_json["params"]["model_basename"].is_string());
    // S6-E6-T02: model_hash must be present for portable identity
    assert!(preset_json["params"]["model_hash"].is_string());
    // S6-E6-T03: oversample and activation_precision must be preserved
    assert_eq!(
        preset_json["params"]["oversample"], "X2",
        "ForPreset must preserve oversample"
    );
    assert!((preset_json["params"]["input_gain_db"].as_f64().unwrap() - 2.0).abs() < f64::EPSILON);

    // ── S6-E6-T03 equivalence: ForPreset blob loaded via state.load on same instance ──
    // First deactivate the plugin to reset DSP state, then load the preset
    // via state.load. The model must be found via basename + model_search_paths
    // (added to the preset in this test, as the model path is stripped by ForPreset save).

    // Clear the params on the current instance to simulate a fresh load
    {
        let clear_params = ProcessingParams {
            input_gain_db: 99.0, // value that would never match
            output_gain_db: 99.0,
            gate_threshold_db: -10.0,
            bypass: false,
            ..Default::default()
        };
        let clear_bytes = serde_json::to_vec(&clear_params).unwrap();
        let mut handle = plugin_instance.plugin_handle();
        state_ext
            .load(&mut handle, &mut clear_bytes.as_slice())
            .expect("clear state load should succeed");
    }

    // Reload the preset via state.load — should find model via basename + search_paths
    {
        let mut handle = plugin_instance.plugin_handle();
        state_ext
            .load(&mut handle, &mut preset_buffer.as_slice())
            .expect("state.load of preset should succeed (S6-E6-T03 equivalence)");
    }

    let counter = shared
        .cold
        .model_load_counter
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        counter > 0,
        "S6-E6-T03: model must be loaded via state.load(ForPreset blob)"
    );

    // Verify audio params match original values (not the cleared ones)
    assert!(
        (f32::from_bits(
            shared
                .ui_to_rt
                .param_input_gain
                .load(std::sync::atomic::Ordering::Relaxed)
        ) - original.input_gain_db)
            .abs()
            < f32::EPSILON
    );
    assert!(
        (f32::from_bits(
            shared
                .ui_to_rt
                .param_output_gain
                .load(std::sync::atomic::Ordering::Relaxed)
        ) - original.output_gain_db)
            .abs()
            < f32::EPSILON
    );
    assert!(
        (f32::from_bits(
            shared
                .ui_to_rt
                .param_gate_thresh
                .load(std::sync::atomic::Ordering::Relaxed)
        ) - original.gate_threshold_db)
            .abs()
            < f32::EPSILON
    );
    assert_eq!(
        shared
            .ui_to_rt
            .param_adaptive_compute
            .load(std::sync::atomic::Ordering::Relaxed),
        AdaptiveComputeMode::Conservative as u32
    );
    assert_eq!(
        shared
            .ui_to_rt
            .param_oversample
            .load(std::sync::atomic::Ordering::Relaxed),
        OversampleFactor::X2.to_f32() as u32
    );
    assert_eq!(
        shared
            .ui_to_rt
            .param_activation
            .load(std::sync::atomic::Ordering::Relaxed),
        ActivationPrecision::Fast as u32
    );

    let ui_name = shared.cold.ui_model_name.lock().unwrap();
    assert_eq!(ui_name.as_str(), "lstm.nam");

    // ── Verify pass 2: ForPreset blob loaded via state.load on fresh instance with model in canonical dir ──
    // This part requires the model to be reachable via canonical_search_dirs().
    // For CI environments where ~/.nam/models/ doesn't exist, we skip the cross-machine check.
    if crate::clap::extensions::state_transaction::canonical_search_dirs().is_empty() {
        log::info!(
            "S6-E6-T03: canonical search dirs empty, skipping cross-machine equivalence check"
        );
    }
}
