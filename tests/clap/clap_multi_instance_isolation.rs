// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Multi-instance isolation integration test for NAM-Plug.
//!
//! Validates:
//! 1. 16 concurrent CLAP plugin instances receiving unique monotonic `instance_id` values.
//! 2. Complete log message isolation across concurrent instances.
//! 3. Zero sink contamination and accurate per-instance `DiagnosticBundle` rendering.

use nam_plug::clap::test_util;
use neural_amp_modeler_rs::common::diagnostics::logger::{
    HostLogFn, LoggerConfig, NamLogger, scope_instance,
};
use neural_amp_modeler_rs::common::diagnostics::{AudioMetadata, DiagnosticBundle, ModelInfo};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::thread;

#[test]
fn test_integration_16_instances_log_and_diagnostic_isolation() {
    const CONCURRENT_INSTANCES: usize = 16;
    const LOGS_PER_THREAD: usize = 15;

    let logger = NamLogger::init(LoggerConfig {
        level_filter: log::LevelFilter::Debug,
        emit_stderr: false,
    })
    .unwrap_or_else(|_| NamLogger::global().expect("NamLogger must be initialized"));

    let mut instances = Vec::with_capacity(CONCURRENT_INSTANCES);
    let mut sink_logs: Vec<Arc<Mutex<Vec<String>>>> = Vec::with_capacity(CONCURRENT_INSTANCES);
    let mut sinks: Vec<Arc<HostLogFn>> = Vec::with_capacity(CONCURRENT_INSTANCES);

    for _ in 0..CONCURRENT_INSTANCES {
        let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
        let shared = unsafe { &*test_util::extract_shared(&mut instance) };
        let inst_id = shared.cold.instance_id;

        let logs = Arc::new(Mutex::new(Vec::new()));
        let logs_clone = Arc::clone(&logs);
        let sink: Arc<HostLogFn> = Arc::new(move |_severity, msg| {
            if let Ok(mut guard) = logs_clone.lock() {
                guard.push(msg.to_string());
            }
        });

        logger.register_instance_sink(inst_id, &sink);

        instances.push((instance, inst_id));
        sink_logs.push(logs);
        sinks.push(sink);
    }

    // Verify all 16 instance_ids are strictly unique
    let id_set: HashSet<u64> = instances.iter().map(|(_, id)| *id).collect();
    assert_eq!(
        id_set.len(),
        CONCURRENT_INSTANCES,
        "Every plugin instance must have a strictly unique instance_id"
    );

    // Spawn 16 threads executing concurrent scoped work and generating telemetry
    let mut thread_handles = Vec::with_capacity(CONCURRENT_INSTANCES);
    for (idx, (_, inst_id)) in instances.iter().enumerate() {
        let id = *inst_id;
        let thread_tag = format!("WORKER_INST_{:02}", idx);
        let handle = thread::spawn(move || {
            let _scope = scope_instance(id);
            for j in 0..LOGS_PER_THREAD {
                log::info!("[{}] Iteration {} processing model chunk", thread_tag, j);
            }
            thread_tag
        });
        thread_handles.push(handle);
    }

    let mut thread_tags = Vec::with_capacity(CONCURRENT_INSTANCES);
    for handle in thread_handles {
        let tag = handle.join().expect("Worker thread must succeed");
        thread_tags.push(tag);
    }

    // Assert sink log isolation
    for (idx, (_, inst_id)) in instances.iter().enumerate() {
        let id = *inst_id;
        let expected_tag = &thread_tags[idx];
        let captured = sink_logs[idx].lock().unwrap();

        assert!(
            !captured.is_empty(),
            "Sink for instance {} ({}) should have received logs",
            id,
            expected_tag
        );

        for log_line in captured.iter() {
            if log_line.contains("WORKER_INST_") {
                assert!(
                    log_line.contains(expected_tag),
                    "Instance {} received log from another instance thread: {}",
                    id,
                    log_line
                );
            }
        }
    }

    // Assert diagnostic bundle isolation
    for (idx, (mut instance, inst_id)) in instances.into_iter().enumerate() {
        let shared = unsafe { &*nam_plug::clap::test_util::extract_shared(&mut instance) };
        let model_basename = format!("Test_Profile_{:02}.nam", idx);

        if let Ok(mut info_guard) = shared.cold.ui_model_info.lock() {
            *info_guard = Some(ModelInfo {
                arch_label: "WaveNet".to_string(),
                topology: "Standard".to_string(),
                channels: 16,
                receptive_field: 2048,
                model_sample_rate: 48000,
                weights_layout: "Interleaved4WaveNet".to_string(),
                path_basename: model_basename.clone(),
            });
        }

        let meta = AudioMetadata {
            channel_count: 2,
            host_name: format!("Integration_Host_{}", inst_id),
        };

        let bundle = DiagnosticBundle::capture_for_instance_with_runtime(inst_id, shared, &meta);
        let rendered = bundle.render();

        assert!(
            rendered.contains(&format!("instance_id={}", inst_id)),
            "Bundle must include instance_id={}",
            inst_id
        );
        assert!(
            rendered.contains(&model_basename),
            "Bundle must include model {}",
            model_basename
        );
        assert!(
            rendered.contains(&thread_tags[idx]),
            "Bundle log trace must include own thread tag {}",
            thread_tags[idx]
        );
    }
}
