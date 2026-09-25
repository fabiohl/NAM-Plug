// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use std::path::PathBuf;

#[test]
fn test_v0_legacy_load() {
    let v0_json = r#"{"input_gain_db": 3.0,"output_gain_db": -6.0,"gate_threshold_db": -50.0,"model_path": null,"bypass": false}"#;
    let params = load_state(v0_json.as_bytes()).expect("v0 payload should load");
    assert!((params.input_gain_db - 3.0).abs() < f32::EPSILON);
    assert!((params.output_gain_db - (-6.0)).abs() < f32::EPSILON);
    assert!((params.gate_threshold_db - (-50.0)).abs() < f32::EPSILON);
    assert_eq!(params.model_path, None);
    assert!(!params.bypass);
    assert_eq!(params.ir_path, None);
}

#[test]
fn test_v0_legacy_load_with_missing_fields() {
    // Simulates old v0 payload that could have missing fields
    let v0_json = r#"{"input_gain_db": 1.5}"#;
    let params = load_state(v0_json.as_bytes()).expect("v0 with missing fields should load");
    assert!((params.input_gain_db - 1.5).abs() < f32::EPSILON);
    assert_eq!(params.output_gain_db, 0.0);
    assert_eq!(params.gate_threshold_db, -70.0);
    assert_eq!(params.model_path, None);
    assert!(!params.bypass);
    assert_eq!(params.ir_path, None);
}

#[test]
fn test_v1_round_trip() {
    let mut original = ProcessingParams::default();
    original.input_gain_db = 2.5;
    original.output_gain_db = -3.0;
    original.gate_threshold_db = -40.0;
    original.model_path = Some(PathBuf::from("/tmp/test.nam"));
    original.bypass = true;

    let envelope = StateEnvelope {
        version: CURRENT_STATE_VERSION,
        params: original.clone(),
    };
    let json = serde_json::to_vec(&envelope).unwrap();

    let restored = load_state(&json).expect("v1 payload should load");
    assert_eq!(restored, original, "v1 round-trip should be idempotent");
}

#[test]
fn test_v1_save_format() {
    let params = ProcessingParams::default();
    let envelope = StateEnvelope {
        version: CURRENT_STATE_VERSION,
        params,
    };
    let json = serde_json::to_vec(&envelope).unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(parsed["version"], 1, "envelope should contain version: 1");
    assert!(
        parsed["params"].is_object(),
        "envelope should contain params"
    );
    assert_eq!(parsed["params"]["ir_path"], serde_json::Value::Null);
}

#[test]
fn test_v0_legacy_load_new_fields_default() {
    let v0_json = r#"{"input_gain_db": 3.0,"output_gain_db": -6.0,"gate_threshold_db": -50.0,"model_path": null,"bypass": false}"#;
    let params = load_state(v0_json.as_bytes()).expect("v0 payload should load");
    assert_eq!(params.model_basename, None);
    assert!(params.model_search_paths.is_empty());
    assert_eq!(params.ir_path, None);
}

#[test]
fn test_v1_round_trip_with_search_fields() {
    let search_paths = vec![
        std::path::PathBuf::from("/usr/share/nam-models"),
        std::path::PathBuf::from("/home/user/models"),
    ];
    let mut original = ProcessingParams::default();
    original.input_gain_db = 2.5;
    original.output_gain_db = -3.0;
    original.gate_threshold_db = -40.0;
    original.model_path = Some(PathBuf::from("/tmp/test.nam"));
    original.model_basename = Some("test.nam".to_string());
    original.model_search_paths = search_paths.clone();
    original.bypass = true;

    let envelope = StateEnvelope {
        version: CURRENT_STATE_VERSION,
        params: original.clone(),
    };
    let json = serde_json::to_vec(&envelope).unwrap();

    let restored = load_state(&json).expect("v1 payload should load");
    assert_eq!(
        restored, original,
        "v1 round-trip with search fields should be idempotent"
    );
}

#[test]
fn test_v1_search_fields_serialization_format() {
    let mut params = ProcessingParams::default();
    params.model_basename = Some("tone.nam".to_string());
    params.model_search_paths = vec![PathBuf::from("/models")];
    let envelope = StateEnvelope {
        version: CURRENT_STATE_VERSION,
        params,
    };
    let json = serde_json::to_vec(&envelope).unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(parsed["params"]["model_basename"], "tone.nam");
    assert_eq!(parsed["params"]["model_search_paths"][0], "/models");
}

#[test]
fn test_v1_round_trip_with_ir_path() {
    let mut original = ProcessingParams::default();
    original.gate_threshold_db = -70.0;
    original.ir_path = Some(PathBuf::from("/tmp/cab.wav"));

    let envelope = StateEnvelope {
        version: CURRENT_STATE_VERSION,
        params: original.clone(),
    };
    let json = serde_json::to_vec(&envelope).unwrap();

    let restored = load_state(&json).expect("v1 payload with ir_path should load");
    assert_eq!(
        restored, original,
        "v1 round-trip with ir_path should be idempotent"
    );
}

#[test]
fn test_v1_ir_path_serialization_format() {
    let mut params = ProcessingParams::default();
    params.ir_path = Some(PathBuf::from("/path/to/cab.wav"));
    let envelope = StateEnvelope {
        version: CURRENT_STATE_VERSION,
        params,
    };
    let json = serde_json::to_vec(&envelope).unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(parsed["params"]["ir_path"], "/path/to/cab.wav");
}

#[test]
fn test_state_stream_size_limit_constant() {
    assert_eq!(MAX_STATE_STREAM_SIZE, 32 * 1024 * 1024);
}

#[test]
fn test_file_hash_size_limit_and_incremental_computation() {
    use crate::clap::extensions::state_transaction::{MAX_FILE_HASH_SIZE, compute_file_hash};
    assert_eq!(MAX_FILE_HASH_SIZE, 256 * 1024 * 1024);

    let temp_dir = std::env::temp_dir();
    let test_file = temp_dir.join("nam_test_hash_stream.tmp");
    std::fs::write(&test_file, b"test nam model bytes for streaming hash").unwrap();

    let hash_result = compute_file_hash(&test_file);
    assert!(hash_result.is_ok());
    let hash = hash_result.unwrap();
    assert!(!hash.is_empty());

    let _ = std::fs::remove_file(&test_file);
}
