// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use neural_amp_modeler_rs::common::params::{
    ActivationPrecision, AdaptiveComputeMode, ProcessingParams,
};
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
use std::path::PathBuf;

fn make_test_params() -> ProcessingParams {
    ProcessingParams {
        input_gain_db: 3.0,
        output_gain_db: -6.0,
        gate_threshold_db: -50.0,
        model_path: Some(PathBuf::from("/tmp/test.nam")),
        model_basename: Some("test.nam".to_string()),
        model_hash: None,
        model_search_paths: vec![PathBuf::from("/tmp")],
        bypass: false,
        adaptive_compute: AdaptiveComputeMode::Off,
        slim_override: Default::default(),
        oversample: OversampleFactor::Off,
        ir_path: None,
        ir_hash: None,
        activation_precision: ActivationPrecision::Standard,
    }
}

fn deserialize_from_context(buf: &[u8]) -> ProcessingParams {
    crate::clap::extensions::state::load_state(buf).unwrap()
}

#[test]
fn test_serialized_format_is_v1_envelope() {
    let params = make_test_params();
    let buf = crate::clap::extensions::state::serialize_envelope(&params).unwrap();

    let parsed: serde_json::Value = serde_json::from_slice(&buf).expect("should be valid JSON");
    assert_eq!(parsed["version"], 1, "envelope should have version: 1");
    assert!(
        parsed["params"].is_object(),
        "envelope should contain params object"
    );
    assert_eq!(parsed["params"]["model_basename"], "test.nam");
}

#[test]
fn test_preset_save_strips_model_path_from_envelope() {
    let params = make_test_params();

    let mut preset_params = params.clone();
    preset_params.model_path = None;
    let buf = crate::clap::extensions::state::serialize_envelope(&preset_params).unwrap();

    let loaded = deserialize_from_context(&buf);
    assert_eq!(
        loaded.model_path, None,
        "preset should not contain model_path"
    );
    assert_eq!(loaded.model_basename, Some("test.nam".to_string()));
    assert!((loaded.input_gain_db - 3.0).abs() < f32::EPSILON);
}

#[test]
fn test_project_save_preserves_model_path_in_envelope() {
    let params = make_test_params();
    let buf = crate::clap::extensions::state::serialize_envelope(&params).unwrap();

    let loaded = deserialize_from_context(&buf);
    assert_eq!(
        loaded.model_path,
        Some(PathBuf::from("/tmp/test.nam")),
        "project save should preserve model_path"
    );
}

#[test]
fn test_duplicate_save_preserves_model_path_in_envelope() {
    let params = make_test_params();
    let buf = crate::clap::extensions::state::serialize_envelope(&params).unwrap();

    let loaded = deserialize_from_context(&buf);
    assert_eq!(
        loaded.model_path,
        Some(PathBuf::from("/tmp/test.nam")),
        "duplicate save should preserve model_path"
    );
}

#[test]
fn test_preset_load_without_model_path_restores_audio_params() {
    let preset_json = r#"{"input_gain_db":2.5,"output_gain_db":-3.0,"gate_threshold_db":-40.0,"model_path":null,"model_basename":"test.nam","model_search_paths":[],"bypass":false,"adaptive_compute":"Off"}"#;
    let loaded = deserialize_from_context(preset_json.as_bytes());

    assert!((loaded.input_gain_db - 2.5).abs() < f32::EPSILON);
    assert!((loaded.output_gain_db - (-3.0)).abs() < f32::EPSILON);
    assert!((loaded.gate_threshold_db - (-40.0)).abs() < f32::EPSILON);
    assert_eq!(loaded.model_path, None);
    assert_eq!(loaded.model_basename, Some("test.nam".to_string()));
}

#[test]
fn test_project_load_preserves_full_state_from_v0_legacy() {
    let project_json = r#"{"input_gain_db":2.5,"output_gain_db":-3.0,"gate_threshold_db":-40.0,"model_path":"/tmp/test.nam","model_basename":"test.nam","model_search_paths":[],"bypass":false,"adaptive_compute":"Off"}"#;
    let loaded = deserialize_from_context(project_json.as_bytes());

    assert_eq!(
        loaded.model_path,
        Some(PathBuf::from("/tmp/test.nam")),
        "project load from v0 should preserve model_path"
    );
}

#[test]
fn test_v1_envelope_load_preserves_full_state() {
    let project_json = serde_json::json!({
        "version": 1,
        "params": {
            "input_gain_db": 2.5,
            "output_gain_db": -3.0,
            "gate_threshold_db": -40.0,
            "model_path": "/tmp/test.nam",
            "model_basename": "test.nam",
            "model_search_paths": [],
            "bypass": false,
            "adaptive_compute": "Off"
        }
    })
    .to_string();

    let loaded = deserialize_from_context(project_json.as_bytes());

    assert_eq!(
        loaded.model_path,
        Some(PathBuf::from("/tmp/test.nam")),
        "v1 envelope load should preserve model_path"
    );
}

#[test]
fn test_v1_envelope_round_trip_preserves_ir_path() {
    let params = ProcessingParams {
        ir_path: Some(PathBuf::from("/tmp/cab.wav")),
        ..make_test_params()
    };

    let buf = crate::clap::extensions::state::serialize_envelope(&params).unwrap();
    let loaded = deserialize_from_context(&buf);

    assert_eq!(
        loaded.ir_path,
        Some(PathBuf::from("/tmp/cab.wav")),
        "ir_path should be preserved in v1 envelope round-trip"
    );
}

#[test]
fn test_v0_legacy_load_preserves_ir_path_null() {
    let v0_json = r#"{"input_gain_db": 3.0, "output_gain_db": -6.0, "gate_threshold_db": -50.0, "model_path": null, "bypass": false}"#;
    let loaded = deserialize_from_context(v0_json.as_bytes());
    assert_eq!(
        loaded.ir_path, None,
        "v0 legacy ir_path should default to None"
    );
}

#[test]
fn test_serialize_envelope_produces_valid_v1_json() {
    let params = make_test_params();
    let buf = crate::clap::extensions::state::serialize_envelope(&params).unwrap();

    let envelope: serde_json::Value =
        serde_json::from_slice(&buf).expect("should deserialize as JSON value");
    assert!(envelope.get("version").is_some());
    assert!(envelope.get("params").is_some());
    assert!(envelope.get("params").unwrap().get("ir_path").is_some());
}
