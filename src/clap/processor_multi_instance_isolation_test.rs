// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use crate::clap::test_util;
use neural_amp_modeler_rs::common::diagnostics::logger::{HostLogFn, NamLogger, scope_instance};
use neural_amp_modeler_rs::common::diagnostics::{AudioMetadata, DiagnosticBundle};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::thread;

static TEST_MUTEX: Mutex<()> = Mutex::new(());

#[test]
fn test_sixteen_concurrent_plugin_instances_have_unique_ids() {
    let _guard = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    const CONCURRENT_COUNT: usize = 16;
    let mut handles = Vec::with_capacity(CONCURRENT_COUNT);

    for _ in 0..CONCURRENT_COUNT {
        let handle = thread::spawn(|| {
            let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
            let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };
            shared.cold.instance_id
        });
        handles.push(handle);
    }

    let mut instance_ids = Vec::with_capacity(CONCURRENT_COUNT);
    for handle in handles {
        let id = handle.join().expect("Thread should not panic");
        assert!(id > 0, "instance_id should be strictly positive");
        instance_ids.push(id);
    }

    let unique_ids: HashSet<u64> = instance_ids.iter().copied().collect();
    assert_eq!(
        unique_ids.len(),
        CONCURRENT_COUNT,
        "All 16 concurrent plugin instances must have unique instance_id values"
    );
}

#[test]
fn test_sixteen_concurrent_instances_log_isolation_and_sink_routing() {
    let _guard = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    const INSTANCES: usize = 16;
    const LOGS_PER_INSTANCE: usize = 20;

    let logger = NamLogger::global().expect("NamLogger must be initialized");

    // Pre-create 16 instances and register per-instance capturing sinks
    let mut instances = Vec::with_capacity(INSTANCES);
    let mut received_logs: Vec<Arc<Mutex<Vec<String>>>> = Vec::with_capacity(INSTANCES);
    let mut registered_sinks: Vec<Arc<HostLogFn>> = Vec::with_capacity(INSTANCES);

    for _ in 0..INSTANCES {
        let (_entry, host_info, mut instance) = test_util::make_test_plugin();
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

        instances.push((host_info, instance, inst_id));
        received_logs.push(logs);
        registered_sinks.push(sink);
    }

    // Spawn 16 threads, each logging scoped messages
    let mut thread_handles = Vec::with_capacity(INSTANCES);
    for (_, _, inst_id) in instances.iter() {
        let id = *inst_id;
        let handle = thread::spawn(move || {
            let _scope = scope_instance(id);
            for j in 0..LOGS_PER_INSTANCE {
                log::info!("[INST_{}] message iteration {}", id, j);
            }
        });
        thread_handles.push(handle);
    }

    for handle in thread_handles {
        handle.join().expect("Worker thread should succeed");
    }

    // Verify isolation: each sink should ONLY contain messages for its own instance
    for (i, (_, _, inst_id)) in instances.iter().enumerate() {
        let id = *inst_id;
        let logs = received_logs[i].lock().unwrap();

        assert!(
            !logs.is_empty(),
            "Sink for instance {} should have received logs",
            id
        );

        let own_prefix = format!("[INST_{}]", id);
        for line in logs.iter() {
            if line.contains("[INST_") {
                assert!(
                    line.contains(&own_prefix),
                    "Instance {} received log from another instance: {}",
                    id,
                    line
                );
            }
        }
    }
}

#[test]
fn test_sixteen_concurrent_instances_diagnostic_bundle_isolation() {
    let _guard = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    const INSTANCES: usize = 16;
    let mut handles = Vec::with_capacity(INSTANCES);

    for _ in 0..INSTANCES {
        let handle = thread::spawn(move || {
            let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
            let shared = unsafe { &*test_util::extract_shared(&mut instance) };
            let inst_id = shared.cold.instance_id;

            // Set instance-specific model info
            let model_name = format!("Custom_Amp_Profile_{}.nam", inst_id);
            if let Ok(mut info_guard) = shared.cold.ui_model_info.lock() {
                *info_guard = Some(neural_amp_modeler_rs::common::diagnostics::ModelInfo {
                    arch_label: "WaveNet".to_string(),
                    topology: "Standard".to_string(),
                    channels: 16,
                    receptive_field: 2048,
                    model_sample_rate: 48000,
                    weights_layout: "Interleaved4WaveNet".to_string(),
                    path_basename: model_name.clone(),
                });
            }

            // Emit instance-scoped log
            {
                let _scope = scope_instance(inst_id);
                log::info!("Instance {} unique diagnostic telemetry trace", inst_id);
            }

            let meta = AudioMetadata {
                channel_count: 2,
                host_name: format!("DAW_Host_{}", inst_id),
            };

            let bundle =
                DiagnosticBundle::capture_for_instance_with_runtime(inst_id, shared, &meta);
            let rendered = bundle.render();

            (inst_id, model_name, rendered)
        });
        handles.push(handle);
    }

    for handle in handles {
        let (inst_id, model_name, rendered) = handle.join().expect("Thread should not panic");

        assert!(
            rendered.contains(&format!("instance_id={}", inst_id)),
            "Rendered bundle must contain instance_id={}: {}",
            inst_id,
            rendered
        );
        assert!(
            rendered.contains(&model_name),
            "Rendered bundle must contain instance model name: {}",
            model_name
        );
        assert!(
            rendered.contains(&format!(
                "Instance {} unique diagnostic telemetry trace",
                inst_id
            )),
            "Rendered bundle trace must contain own instance logs"
        );
    }
}

#[test]
fn test_rapid_multi_instance_lifecycle_contention_free() {
    let _guard = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    const THREADS: usize = 8;
    const CYCLES_PER_THREAD: usize = 10;

    let mut handles = Vec::with_capacity(THREADS);
    for _ in 0..THREADS {
        let handle = thread::spawn(|| {
            for _ in 0..CYCLES_PER_THREAD {
                let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
                let shared = unsafe { &*test_util::extract_shared(&mut instance) };
                let _scope = scope_instance(shared.cold.instance_id);
                log::info!("Rapid cycle log instance {}", shared.cold.instance_id);
                drop(instance);
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle
            .join()
            .expect("Rapid lifecycle thread should not panic or deadlock");
    }
}
