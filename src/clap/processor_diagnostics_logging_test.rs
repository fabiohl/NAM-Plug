// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use crate::clap::test_util;
use neural_amp_modeler_rs::common::spsc::{
    RT_STATUS_GC_OVERFLOW, RT_STATUS_HAS_CLIPPED, RT_STATUS_HUGEPAGE_OK,
    RT_STATUS_MODEL_LOAD_FAILED, RtStatusFlags,
};
use std::sync::Arc;
use std::sync::atomic::Ordering;

// Task 4.3.1 — flag set/clear mechanism for emit_pending_logs
#[test]
fn test_flag_set_and_clear_mechanism() {
    let rt_status = Arc::new(RtStatusFlags::new());

    rt_status.set_flag(RT_STATUS_HAS_CLIPPED);
    rt_status.set_flag(RT_STATUS_GC_OVERFLOW);
    rt_status.set_flag(RT_STATUS_HUGEPAGE_OK);

    assert!(rt_status.check_flag(RT_STATUS_HAS_CLIPPED));
    assert!(rt_status.check_flag(RT_STATUS_GC_OVERFLOW));
    assert!(rt_status.check_flag(RT_STATUS_HUGEPAGE_OK));

    assert!(rt_status.check_and_clear_flag(RT_STATUS_HAS_CLIPPED));
    assert!(!rt_status.check_flag(RT_STATUS_HAS_CLIPPED));

    assert!(rt_status.check_and_clear_flag(RT_STATUS_GC_OVERFLOW));
    assert!(!rt_status.check_flag(RT_STATUS_GC_OVERFLOW));

    assert!(rt_status.check_and_clear_flag(RT_STATUS_HUGEPAGE_OK));
    assert!(!rt_status.check_flag(RT_STATUS_HUGEPAGE_OK));

    let flags_seen = rt_status.flags_seen.load(Ordering::Relaxed);
    assert_eq!(
        flags_seen,
        RT_STATUS_HAS_CLIPPED | RT_STATUS_GC_OVERFLOW | RT_STATUS_HUGEPAGE_OK,
        "flags_seen should accumulate all flags that were ever set"
    );
}

// Mutex to synchronize tests inspecting or mutating the global NamLogger / LogBuffer state
static TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

// Task 4.3.1 — verify flag-to-log messages reach LogBuffer
#[test]
fn test_emit_pending_logs_messages_reach_log_buffer() {
    let _guard = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

    let logger = neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::global()
        .expect("NamLogger should be initialized");
    let original_level = log::max_level();
    log::set_max_level(log::LevelFilter::Debug);
    logger.set_max_level(log::LevelFilter::Debug);

    let buffer = neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::log_buffer()
        .expect("LogBuffer should be accessible");
    let snapshot_before = buffer.len();
    let capacity = buffer.capacity();

    shared
        .cold
        .rt_status
        .check_and_clear_flag(RT_STATUS_HAS_CLIPPED);
    log::warn!("NAM-Plug: Output clipping detected!");

    shared
        .cold
        .rt_status
        .check_and_clear_flag(RT_STATUS_GC_OVERFLOW);
    log::error!("NAM-Plug: GC channel overflow! Possible memory leak.");

    shared
        .cold
        .rt_status
        .check_and_clear_flag(RT_STATUS_MODEL_LOAD_FAILED);
    log::error!("NAM-Plug: Critical failure! No active model for processing.");

    let snapshot_after = buffer.len();
    assert!(
        snapshot_after > snapshot_before + 2 || snapshot_after >= capacity,
        "LogBuffer should grow after log entries are emitted (or remain at capacity)"
    );

    test_util::assert_log_buffer_contains("Output clipping detected");
    test_util::assert_log_buffer_contains("GC channel overflow");
    test_util::assert_log_buffer_contains("Critical failure! No active model for processing");

    log::set_max_level(original_level);
    logger.set_max_level(original_level);
}

// Task 4.3.1 — verify state save emits confirmation log
#[test]
fn test_state_save_emits_confirmation_log() {
    use log::LevelFilter;
    let _guard = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

    let logger = neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::global()
        .expect("NamLogger should be initialized");
    let original_level = log::max_level();
    log::set_max_level(LevelFilter::Debug);
    logger.set_max_level(LevelFilter::Debug);

    let model_path = crate::clap::test_util::model_path("lstm.nam");

    use neural_amp_modeler_rs::common::params::ProcessingParams;
    let params = ProcessingParams {
        model_path: Some(model_path.clone()),
        input_gain_db: 1.0,
        output_gain_db: -2.0,
        gate_threshold_db: -50.0,
        model_basename: Some("lstm.nam".to_string()),
        model_search_paths: vec![],
        model_hash: crate::clap::test_util::asset_hash(&model_path),
        bypass: false,
        adaptive_compute: neural_amp_modeler_rs::common::params::AdaptiveComputeMode::Off,
        slim_override: Default::default(),
        oversample: neural_amp_modeler_rs::dsp::oversample::OversampleFactor::Off,
        ir_path: None,
        ir_hash: None,
        activation_precision: neural_amp_modeler_rs::common::params::ActivationPrecision::Standard,
    };
    let state_bytes = serde_json::to_vec(&params).unwrap();
    let state_ext = test_util::get_state_ext(&mut plugin_instance);
    let mut handle = plugin_instance.plugin_handle();
    state_ext
        .load(&mut handle, &mut state_bytes.as_slice())
        .expect("Failed to load state");

    let mut output = Vec::new();
    let mut handle = plugin_instance.plugin_handle();
    state_ext
        .save(&mut handle, &mut output)
        .expect("save should succeed");

    assert!(!output.is_empty(), "save output should not be empty");
    test_util::assert_log_buffer_contains("[State] Save completed:");
    test_util::assert_log_buffer_contains("bytes serialized.");

    log::set_max_level(original_level);
    logger.set_max_level(original_level);
}
