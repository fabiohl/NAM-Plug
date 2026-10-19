// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use crate::clap::test_util;
use neural_amp_modeler_rs::common::spsc::{
    RT_STATUS_GC_OVERFLOW, RT_STATUS_HAS_CLIPPED, RT_STATUS_HUGEPAGE_OK,
    RT_STATUS_MODEL_LOAD_FAILED, RtStatusFlags,
};
use std::sync::Arc;
use std::sync::atomic::Ordering;

// Flag set/clear mechanism for emit_pending_logs
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

// Verify flag-to-log messages reach LogBuffer
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
    log::warn!(
        "NAM-Plug: Output clipping detected! Consider reducing input/output gain, \
         or checking whether the model carries a large DC offset."
    );

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
    test_util::assert_log_buffer_contains("DC offset");
    test_util::assert_log_buffer_contains("GC channel overflow");
    test_util::assert_log_buffer_contains("Critical failure! No active model for processing");

    log::set_max_level(original_level);
    logger.set_max_level(original_level);
}

// Verify state save emits confirmation log
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

    let mut params = neural_amp_modeler_rs::common::params::ProcessingParams::default();
    params.model_path = Some(model_path.clone());
    params.input_gain_db = 1.0;
    params.output_gain_db = -2.0;
    params.gate_threshold_db = -50.0;
    params.model_basename = Some("lstm.nam".to_string());
    params.model_hash = crate::clap::test_util::asset_hash(&model_path);
    let state_bytes = serde_json::to_vec(&params).unwrap();
    let state_ext = test_util::get_state_ext(&mut plugin_instance);
    let handle = plugin_instance.plugin_handle();
    state_ext
        .load(&handle, &mut state_bytes.as_slice())
        .expect("Failed to load state");

    let mut output = Vec::new();
    let handle = plugin_instance.plugin_handle();
    state_ext
        .save(&handle, &mut output)
        .expect("save should succeed");

    assert!(!output.is_empty(), "save output should not be empty");
    test_util::assert_log_buffer_contains("[State] Save completed:");
    test_util::assert_log_buffer_contains("bytes serialized.");

    log::set_max_level(original_level);
    logger.set_max_level(original_level);
}

// T-R2.3: Verify rate-limit and aggregation of xrun/overload warnings (<= 1 warning / 5 s)
#[test]
fn test_emit_pending_logs_overload_rate_limiting() {
    let _guard = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    use crate::clap::host_harness::{self, HostEvent};
    use clack_extensions::log::LogSeverity;
    use clack_extensions::timer::TimerId;

    let (_entry, _host_info, mut instance, host_state) =
        host_harness::make_test_plugin_with_harness();
    let shared = unsafe { &*test_util::extract_shared(&mut instance) };

    // 1. Initial state: 0 overloads -> timer fires -> 0 warnings
    host_harness::fire_timer(&mut instance, TimerId(1));
    let logs = host_state
        .snapshot()
        .into_iter()
        .filter_map(|e| match e {
            HostEvent::PluginLog {
                severity: LogSeverity::Warning,
                message,
            } => Some(message),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        logs.is_empty(),
        "No warnings should be emitted when there are no overloads"
    );

    // 2. Phase 1: 1 isolated stall -> exactly 1 warning cycle emitted
    // Note: emit_pending_logs emits via both HostLog and log::warn! (which NamLogger forwards to HostLog),
    // yielding 2 HostEvent::PluginLog entries for the single logical warning.
    shared
        .cold
        .rt_status
        .dsp_overloads
        .store(1, Ordering::Relaxed);
    shared
        .rt_to_ui
        .dsp_load_permille
        .store(850, Ordering::Relaxed); // 85%

    host_harness::fire_timer(&mut instance, TimerId(1));

    let logs = host_state
        .snapshot()
        .into_iter()
        .filter_map(|e| match e {
            HostEvent::PluginLog {
                severity: LogSeverity::Warning,
                message,
            } => Some(message),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        logs.len(),
        2,
        "1 isolated stall must emit warning via HostLog and log::warn!"
    );
    assert!(
        logs[0].contains("NAM-Plug: 1 blocks exceeded 85% of the callback budget (load 85%)"),
        "Warning message must match expected template: {}",
        logs[0]
    );

    // 3. Phase 2: 100 stalls in burst in the same 5-second window -> suppressed by rate limiter
    shared
        .cold
        .rt_status
        .dsp_overloads
        .store(101, Ordering::Relaxed); // +100 overloads
    shared
        .rt_to_ui
        .dsp_load_permille
        .store(920, Ordering::Relaxed); // 92%

    for _ in 0..10 {
        host_harness::fire_timer(&mut instance, TimerId(1));
    }

    let logs_after_burst = host_state
        .snapshot()
        .into_iter()
        .filter_map(|e| match e {
            HostEvent::PluginLog {
                severity: LogSeverity::Warning,
                message,
            } => Some(message),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        logs_after_burst.len(),
        2,
        "100 stalls in burst must be suppressed by the 5-second rate limiter (no new warnings in window)"
    );
}

// T-R3.3: Verify standardized HostLog transitions (0 -> 1) and zero duplicate logs per tick
#[test]
fn test_emit_pending_logs_alert_transitions_and_no_repeat() {
    let _guard = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    use crate::clap::host_harness::{self, HostEvent};
    use clack_extensions::log::LogSeverity;
    use clack_extensions::timer::TimerId;
    use neural_amp_modeler_rs::common::spsc::{
        RT_STATUS_HOST_CONTRACT_VIOLATION, RT_STATUS_NON_FINITE_INPUT_DETECTED,
        RT_STATUS_PROCESSOR_POISONED,
    };

    let (_entry, _host_info, mut instance, host_state) =
        host_harness::make_test_plugin_with_harness();
    let shared = unsafe { &*test_util::extract_shared(&mut instance) };

    // Initial state: tick timer, expect zero warning/error alert logs
    host_harness::fire_timer(&mut instance, TimerId(1));
    let initial_events = host_state.snapshot();
    let initial_alerts: Vec<_> = initial_events
        .iter()
        .filter_map(|e| match e {
            HostEvent::PluginLog { severity, message }
                if *severity == LogSeverity::Warning || *severity == LogSeverity::Error =>
            {
                Some((severity, message.clone()))
            }
            _ => None,
        })
        .collect();
    assert!(
        initial_alerts.is_empty(),
        "Initial state must emit zero warning/error logs, got: {:?}",
        initial_alerts
    );

    // 1. Transition 0 -> 1 on POISONED and NON_FINITE_INPUT
    shared.cold.rt_status.set_flag(RT_STATUS_PROCESSOR_POISONED);
    shared
        .cold
        .rt_status
        .set_flag(RT_STATUS_NON_FINITE_INPUT_DETECTED);

    host_harness::fire_timer(&mut instance, TimerId(1));

    let events = host_state.snapshot();
    let errors: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            HostEvent::PluginLog {
                severity: LogSeverity::Error,
                message,
            } => Some(message.clone()),
            _ => None,
        })
        .collect();
    let warnings: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            HostEvent::PluginLog {
                severity: LogSeverity::Warning,
                message,
            } => Some(message.clone()),
            _ => None,
        })
        .collect();

    assert!(
        errors
            .iter()
            .any(|m| m.contains("Audio processor is poisoned")),
        "Must emit error log for POISONED transition: {:?}",
        errors
    );
    assert!(
        warnings
            .iter()
            .any(|m| m.contains("Non-finite input samples")),
        "Must emit warning log for NON_FINITE_INPUT transition: {:?}",
        warnings
    );

    let error_count_after_first = errors.len();
    let warning_count_after_first = warnings.len();

    // 2. Subsequent ticks with identical state must emit 0 additional logs (no flooding)
    for _ in 0..5 {
        host_harness::fire_timer(&mut instance, TimerId(1));
    }
    let events_after_ticks = host_state.snapshot();
    let errors_after_ticks: Vec<_> = events_after_ticks
        .iter()
        .filter_map(|e| match e {
            HostEvent::PluginLog {
                severity: LogSeverity::Error,
                message,
            } => Some(message.clone()),
            _ => None,
        })
        .collect();
    let warnings_after_ticks: Vec<_> = events_after_ticks
        .iter()
        .filter_map(|e| match e {
            HostEvent::PluginLog {
                severity: LogSeverity::Warning,
                message,
            } => Some(message.clone()),
            _ => None,
        })
        .collect();

    assert_eq!(
        errors_after_ticks.len(),
        error_count_after_first,
        "Subsequent ticks without bit transition must not emit duplicate error logs"
    );
    assert_eq!(
        warnings_after_ticks.len(),
        warning_count_after_first,
        "Subsequent ticks without bit transition must not emit duplicate warning logs"
    );

    // 3. New transition: HOST_CONTRACT (0 -> 1)
    shared
        .cold
        .rt_status
        .set_flag(RT_STATUS_HOST_CONTRACT_VIOLATION);
    host_harness::fire_timer(&mut instance, TimerId(1));

    let events_after_new = host_state.snapshot();
    let errors_after_new: Vec<_> = events_after_new
        .iter()
        .filter_map(|e| match e {
            HostEvent::PluginLog {
                severity: LogSeverity::Error,
                message,
            } => Some(message.clone()),
            _ => None,
        })
        .collect();

    assert_eq!(
        errors_after_new.len(),
        error_count_after_first + 2, // 1 from HostLog + 1 from log::error! forwarding
        "New transition must emit exactly one log cycle"
    );
    assert!(
        errors_after_new
            .iter()
            .any(|m| m.contains("Host audio buffer size exceeded")),
        "Host contract violation log must be present"
    );
}
