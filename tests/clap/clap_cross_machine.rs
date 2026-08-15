// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Cross-Machine Simulation and Corrupted Asset Test Suite.
//!
//! Simulates real-world error scenarios: truncated files, malformed payloads,
//! missing assets, cross-machine portable search, and corrupted model weights.
//! All error paths must result in graceful rejection without altering active
//! DSP state or producing panics / buffer overflows.

use clack_host::prelude::*;
use nam_plug::clap::test_util;
use neural_amp_modeler_rs::common::params::ProcessingParams;
use neural_amp_modeler_rs::common::spsc::RT_STATUS_MODEL_LOAD_FAILED;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

fn model_fixture(name: &str) -> PathBuf {
    crate::common::fixtures::model_path(name)
}

/// Minimal audio process helper — verifies DSP is alive and produces finite output.
fn process_one_block(started: &mut StartedPluginAudioProcessor<test_util::TestHost>, n: usize) {
    let mut il = vec![0.3f32; n];
    let mut ir = vec![0.3f32; n];
    let mut ol = vec![0.0f32; n];
    let mut or = vec![0.0f32; n];

    let mut input_ports = AudioPorts::with_capacity(2, 1);
    let mut output_ports = AudioPorts::with_capacity(2, 1);

    let in_ch = [il.as_mut_slice(), ir.as_mut_slice()];
    let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_input_only(
            in_ch.into_iter().map(InputChannel::constant),
        ),
    }]);
    let out_ch = [ol.as_mut_slice(), or.as_mut_slice()];
    let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_output_only(out_ch.into_iter()),
    }]);
    let mut output_events_buffer = EventBuffer::new();
    let mut out_ev = OutputEvents::from_buffer(&mut output_events_buffer);

    started
        .process(
            &input_audio,
            &mut output_audio,
            &InputEvents::empty(),
            &mut out_ev,
            None,
            None,
        )
        .expect("process should not panic after failed state restore");

    for &sample in &ol {
        assert!(
            sample.is_finite(),
            "output must be finite after failed restore"
        );
    }
    for &sample in &or {
        assert!(
            sample.is_finite(),
            "output must be finite after failed restore"
        );
    }
}

/// Assert that the old state is fully preserved after a failed restore.
fn assert_old_state_preserved(
    shared: &nam_plug::clap::plugin::NamClapShared,
    expected_name: &str,
    expected_counter: u32,
) {
    let ui_name = shared.cold.ui_model_name.lock().unwrap();
    assert!(
        !ui_name.is_empty(),
        "ui_model_name must be preserved after failed restore (was '{}')",
        ui_name.as_str()
    );
    assert_eq!(
        ui_name.as_str(),
        expected_name,
        "ui_model_name should be original model name after failed restore"
    );
    assert!(
        !shared
            .cold
            .rt_status
            .check_flag(RT_STATUS_MODEL_LOAD_FAILED),
        "RT_STATUS_MODEL_LOAD_FAILED must NOT be set — DSP was never touched"
    );
    assert_eq!(
        shared.cold.model_load_counter.load(Ordering::Relaxed),
        expected_counter,
        "model_load_counter must not change after failed restore"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_zero_byte_state_payload_is_rejected() {
    let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
    let state_ext = test_util::get_state_ext(&mut instance);
    let empty: &[u8] = &[];
    let mut handle = instance.plugin_handle();
    let result = state_ext.load(&mut handle, &mut std::io::Cursor::new(empty));
    assert!(result.is_err(), "Empty state payload must be rejected");
}

#[test]
fn test_malformed_json_state_is_rejected() {
    let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
    let state_ext = test_util::get_state_ext(&mut instance);

    // Garbage bytes
    let garbage = b"not-valid-json-at-all-!!!!";
    let mut handle = instance.plugin_handle();
    let result = state_ext.load(&mut handle, &mut std::io::Cursor::new(garbage));
    assert!(result.is_err(), "Garbage bytes must be rejected");

    // Valid JSON envelope with corrupted params
    let corrupted = br#"{"version":1,"params":{"input_gain_db":"not_a_number","model_path":null}}"#;
    let mut handle = instance.plugin_handle();
    let result = state_ext.load(&mut handle, &mut std::io::Cursor::new(corrupted));
    assert!(result.is_err(), "Corrupted JSON params must be rejected");
}

#[test]
fn test_missing_model_path_is_rejected_and_keeps_old_dsp() {
    let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
    let shared_ptr = test_util::extract_shared(&mut instance);
    let shared = unsafe { &*shared_ptr };

    // Load valid model first
    let model_a = model_fixture("wavenet_a1_standard.nam");
    let params_a = ProcessingParams {
        model_path: Some(model_a),
        model_basename: Some("wavenet_a1_standard.nam".to_string()),
        input_gain_db: 0.0,
        output_gain_db: 0.0,
        gate_threshold_db: -90.0,
        bypass: false,
        ..Default::default()
    };

    let state_ext = test_util::get_state_ext(&mut instance);
    let state_bytes = serde_json::to_vec(&params_a).unwrap();
    {
        let mut handle = instance.plugin_handle();
        state_ext
            .load(&mut handle, &mut state_bytes.as_slice())
            .expect("load model A should succeed");
    }

    // Activate and process
    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 256,
        max_frames_count: 256,
    };
    let stopped = instance.activate(|_, _| (), audio_config).unwrap();
    let mut started = stopped.start_processing().unwrap();
    let n = 256;

    for _ in 0..4 {
        process_one_block(&mut started, n);
    }

    let model_a_name = shared.cold.ui_model_name.lock().unwrap().clone();
    let model_a_counter = shared.cold.model_load_counter.load(Ordering::Relaxed);

    // Attempt bad load
    let bad_params = ProcessingParams {
        model_path: Some(PathBuf::from("/nonexistent/model_x.nam")),
        model_basename: Some("model_x.nam".to_string()),
        model_search_paths: vec![],
        input_gain_db: 99.0,
        output_gain_db: 99.0,
        gate_threshold_db: -10.0,
        bypass: true,
        ..Default::default()
    };

    let state_bad = serde_json::to_vec(&bad_params).unwrap();
    {
        let state_ext = test_util::get_state_ext(&mut instance);
        let mut handle = instance.plugin_handle();
        let result = state_ext.load(&mut handle, &mut state_bad.as_slice());
        assert!(result.is_err(), "Missing model path must return Err");
    }

    process_one_block(&mut started, n);
    assert_old_state_preserved(shared, &model_a_name, model_a_counter);
    instance.deactivate(started.stop_processing());
}

#[test]
fn test_corrupted_model_weights_rejected_gracefully() {
    let original = model_fixture("wavenet_a1_standard.nam");
    let bytes = std::fs::read(&original).expect("failed to read model fixture");
    let mut corrupted = bytes.clone();
    let start = (bytes.len() / 3).max(1);
    let end = (bytes.len() * 2 / 3).max(start + 1);
    corrupted[start..end].fill(0xFF);
    let corrupted_path = std::env::temp_dir().join("nam_s6e6t05_corrupted.nam");
    std::fs::write(&corrupted_path, &corrupted).expect("write corrupted model");

    let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
    let shared_ptr = test_util::extract_shared(&mut instance);
    let shared = unsafe { &*shared_ptr };

    let model_a = model_fixture("lstm.nam");
    let params_a = ProcessingParams {
        model_path: Some(model_a),
        model_basename: Some("lstm.nam".to_string()),
        ..Default::default()
    };

    let state_ext = test_util::get_state_ext(&mut instance);
    let state_bytes = serde_json::to_vec(&params_a).unwrap();
    {
        let mut handle = instance.plugin_handle();
        state_ext
            .load(&mut handle, &mut state_bytes.as_slice())
            .expect("load model A");
    }

    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 256,
        max_frames_count: 256,
    };
    let stopped = instance.activate(|_, _| (), audio_config).unwrap();
    let mut started = stopped.start_processing().unwrap();
    let n = 256;
    for _ in 0..4 {
        process_one_block(&mut started, n);
    }

    let model_a_name = shared.cold.ui_model_name.lock().unwrap().clone();
    let model_a_counter = shared.cold.model_load_counter.load(Ordering::Relaxed);

    let bad_params = ProcessingParams {
        model_path: Some(corrupted_path.clone()),
        ..Default::default()
    };
    let bad_bytes = serde_json::to_vec(&bad_params).unwrap();
    {
        let state_ext = test_util::get_state_ext(&mut instance);
        let mut handle = instance.plugin_handle();
        let result = state_ext.load(&mut handle, &mut bad_bytes.as_slice());
        assert!(result.is_err(), "Corrupted model weights must return Err");
    }

    process_one_block(&mut started, n);
    assert_old_state_preserved(shared, &model_a_name, model_a_counter);
    instance.deactivate(started.stop_processing());
    let _ = std::fs::remove_file(&corrupted_path);
}

#[test]
fn test_truncated_model_file_rejected_gracefully() {
    let original = model_fixture("wavenet_a1_standard.nam");
    let bytes = std::fs::read(&original).unwrap();
    let truncated = &bytes[..100.min(bytes.len())];
    let truncated_path = std::env::temp_dir().join("nam_s6e6t05_truncated.nam");
    std::fs::write(&truncated_path, truncated).unwrap();

    let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
    let state_ext = test_util::get_state_ext(&mut instance);

    let bad_params = ProcessingParams {
        model_path: Some(truncated_path.clone()),
        ..Default::default()
    };
    let bad_bytes = serde_json::to_vec(&bad_params).unwrap();
    let mut handle = instance.plugin_handle();
    let result = state_ext.load(&mut handle, &mut bad_bytes.as_slice());
    assert!(result.is_err(), "Truncated model file must return Err");

    let _ = std::fs::remove_file(&truncated_path);
}

#[test]
fn test_zero_byte_model_file_rejected_gracefully() {
    let zero_path = std::env::temp_dir().join("nam_s6e6t05_zero.nam");
    std::fs::write(&zero_path, []).unwrap();

    let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
    let state_ext = test_util::get_state_ext(&mut instance);

    let bad_params = ProcessingParams {
        model_path: Some(zero_path.clone()),
        ..Default::default()
    };
    let bad_bytes = serde_json::to_vec(&bad_params).unwrap();
    let mut handle = instance.plugin_handle();
    let result = state_ext.load(&mut handle, &mut bad_bytes.as_slice());
    assert!(result.is_err(), "Zero-byte model file must return Err");

    let _ = std::fs::remove_file(&zero_path);
}

#[test]
fn test_cross_machine_restore_via_basename_search_succeeds() {
    let model_dir = model_fixture("wavenet_a1_standard.nam")
        .parent()
        .unwrap()
        .to_path_buf();

    let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
    let shared_ptr = test_util::extract_shared(&mut instance);

    let cross_params = ProcessingParams {
        model_path: Some(PathBuf::from(
            "/home/otheruser/models/wavenet_a1_standard.nam",
        )),
        model_basename: Some("wavenet_a1_standard.nam".to_string()),
        model_search_paths: vec![model_dir],
        input_gain_db: 0.0,
        output_gain_db: 0.0,
        gate_threshold_db: -90.0,
        bypass: false,
        ..Default::default()
    };

    let state_ext = test_util::get_state_ext(&mut instance);
    let state_bytes = serde_json::to_vec(&cross_params).unwrap();
    {
        let mut handle = instance.plugin_handle();
        let result = state_ext.load(&mut handle, &mut state_bytes.as_slice());
        assert!(result.is_ok(), "Cross-machine restore must succeed");
    }

    let shared = unsafe { &*shared_ptr };
    assert!(shared.cold.model_load_counter.load(Ordering::Relaxed) > 0);
    let ui_name = shared.cold.ui_model_name.lock().unwrap();
    assert_eq!(ui_name.as_str(), "wavenet_a1_standard.nam");
}

#[test]
fn test_cross_machine_basename_not_found_is_rejected() {
    let non_existent_dir = PathBuf::from("/tmp/nam_s6e6t05_empty");
    let _ = std::fs::create_dir_all(&non_existent_dir);

    let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
    let state_ext = test_util::get_state_ext(&mut instance);

    let cross_params = ProcessingParams {
        model_path: Some(PathBuf::from("/home/otheruser/models/unknown.nam")),
        model_basename: Some("unknown.nam".to_string()),
        model_search_paths: vec![non_existent_dir.clone()],
        input_gain_db: 0.0,
        output_gain_db: 0.0,
        gate_threshold_db: -90.0,
        bypass: false,
        ..Default::default()
    };

    let state_bytes = serde_json::to_vec(&cross_params).unwrap();
    let mut handle = instance.plugin_handle();
    let result = state_ext.load(&mut handle, &mut state_bytes.as_slice());
    assert!(result.is_err(), "Basename not found must return Err");

    let _ = std::fs::remove_dir_all(&non_existent_dir);
}

#[test]
fn test_bad_model_hash_is_rejected() {
    let model_dir = model_fixture("wavenet_a1_standard.nam")
        .parent()
        .unwrap()
        .to_path_buf();

    let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
    let state_ext = test_util::get_state_ext(&mut instance);

    let bad_hash_params = ProcessingParams {
        model_path: None,
        model_basename: Some("wavenet_a1_standard.nam".to_string()),
        model_hash: Some(
            "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
        ),
        model_search_paths: vec![model_dir],
        ..Default::default()
    };

    let state_bytes = serde_json::to_vec(&bad_hash_params).unwrap();
    let mut handle = instance.plugin_handle();
    let result = state_ext.load(&mut handle, &mut state_bytes.as_slice());
    assert!(
        result.is_err(),
        "Bad model_hash must be rejected even with valid basename+search_path"
    );
}

#[test]
fn test_state_load_after_failed_restore_still_works() {
    let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
    let shared_ptr = test_util::extract_shared(&mut instance);

    let model_a = model_fixture("wavenet_a1_standard.nam");
    let params_a = ProcessingParams {
        model_path: Some(model_a),
        model_basename: Some("wavenet_a1_standard.nam".to_string()),
        ..Default::default()
    };

    let state_ext = test_util::get_state_ext(&mut instance);
    let state_bytes = serde_json::to_vec(&params_a).unwrap();
    {
        let mut handle = instance.plugin_handle();
        state_ext
            .load(&mut handle, &mut state_bytes.as_slice())
            .expect("load model A");
    }

    // Failed restore
    let bad_params = ProcessingParams {
        model_path: Some(PathBuf::from("/nonexistent/x.nam")),
        ..Default::default()
    };
    let bad_bytes = serde_json::to_vec(&bad_params).unwrap();
    {
        let state_ext = test_util::get_state_ext(&mut instance);
        let mut handle = instance.plugin_handle();
        let result = state_ext.load(&mut handle, &mut bad_bytes.as_slice());
        assert!(result.is_err());
    }

    // Valid restore afterward
    let valid_b = model_fixture("lstm.nam");
    let valid_params = ProcessingParams {
        model_path: Some(valid_b),
        model_basename: Some("lstm.nam".to_string()),
        input_gain_db: 2.5,
        ..Default::default()
    };
    let valid_bytes = serde_json::to_vec(&valid_params).unwrap();
    {
        let state_ext = test_util::get_state_ext(&mut instance);
        let mut handle = instance.plugin_handle();
        let result = state_ext.load(&mut handle, &mut valid_bytes.as_slice());
        assert!(
            result.is_ok(),
            "Valid state.load must work after failed restore"
        );
    }

    let shared = unsafe { &*shared_ptr };
    assert!(shared.cold.model_load_counter.load(Ordering::Relaxed) > 0);
    let ui_name = shared.cold.ui_model_name.lock().unwrap();
    assert_eq!(ui_name.as_str(), "lstm.nam");
}

#[test]
fn test_all_failure_modes_preserve_dsp_and_produce_finite_output() {
    let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
    let shared_ptr = test_util::extract_shared(&mut instance);
    let shared = unsafe { &*shared_ptr };

    let model_a = model_fixture("wavenet_a1_standard.nam");
    let params_a = ProcessingParams {
        model_path: Some(model_a),
        model_basename: Some("wavenet_a1_standard.nam".to_string()),
        ..Default::default()
    };

    let state_ext = test_util::get_state_ext(&mut instance);
    let state_bytes = serde_json::to_vec(&params_a).unwrap();
    {
        let mut handle = instance.plugin_handle();
        state_ext
            .load(&mut handle, &mut state_bytes.as_slice())
            .unwrap();
    }

    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 256,
        max_frames_count: 256,
    };
    let stopped = instance.activate(|_, _| (), audio_config).unwrap();
    let mut started = stopped.start_processing().unwrap();
    let n = 256;
    for _ in 0..4 {
        process_one_block(&mut started, n);
    }

    let model_a_name = shared.cold.ui_model_name.lock().unwrap().clone();
    let model_a_counter = shared.cold.model_load_counter.load(Ordering::Relaxed);

    let failure_params_list: Vec<ProcessingParams> = vec![
        ProcessingParams {
            model_path: Some(PathBuf::from("/nonexistent/a.nam")),
            model_basename: Some("a.nam".to_string()),
            model_search_paths: vec![],
            ..Default::default()
        },
        ProcessingParams {
            model_path: Some(PathBuf::from("/nonexistent/b.nam")),
            ..Default::default()
        },
        ProcessingParams {
            model_path: Some(std::env::temp_dir().join("doesnotexist.nam")),
            ..Default::default()
        },
    ];

    for (i, bad_params) in failure_params_list.iter().enumerate() {
        let bad_bytes = serde_json::to_vec(bad_params).unwrap();
        {
            let state_ext = test_util::get_state_ext(&mut instance);
            let mut handle = instance.plugin_handle();
            let result = state_ext.load(&mut handle, &mut bad_bytes.as_slice());
            assert!(result.is_err(), "Failure mode {i} must return Err");
        }

        process_one_block(&mut started, n);
        assert_old_state_preserved(shared, &model_a_name, model_a_counter);

        let in_gain = f32::from_bits(shared.ui_to_rt.param_input_gain.load(Ordering::Relaxed));
        let out_gain = f32::from_bits(shared.ui_to_rt.param_output_gain.load(Ordering::Relaxed));
        assert!((in_gain - 0.0).abs() < f32::EPSILON);
        assert!((out_gain - 0.0).abs() < f32::EPSILON);
    }

    instance.deactivate(started.stop_processing());
}
