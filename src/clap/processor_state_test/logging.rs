// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use crate::clap::test_util;
use log::LevelFilter;
use std::sync::atomic::{AtomicU64, Ordering};

static LOG_BUFFER_TEST_NONCE: AtomicU64 = AtomicU64::new(0);

struct LogLevelGuard {
    logger: &'static neural_amp_modeler_rs::common::diagnostics::logger::NamLogger,
    previous_level: LevelFilter,
}

impl LogLevelGuard {
    fn new(
        logger: &'static neural_amp_modeler_rs::common::diagnostics::logger::NamLogger,
        level: LevelFilter,
    ) -> Self {
        let previous_level = log::max_level();
        logger.set_max_level(level);
        Self {
            logger,
            previous_level,
        }
    }
}

impl Drop for LogLevelGuard {
    fn drop(&mut self) {
        self.logger.set_max_level(self.previous_level);
    }
}

#[test]
fn test_nam_logger_initialized_on_plugin_construction() {
    let (_entry, _host_info, _plugin_instance) = test_util::make_test_plugin();

    assert!(
        neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::global().is_some(),
        "NamLogger::global() should be Some after plugin construction"
    );
    assert!(
        neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::log_buffer().is_some(),
        "LogBuffer should be accessible after plugin construction"
    );
}

#[test]
fn test_log_info_reaches_log_buffer_during_plugin_lifecycle() {
    let (_entry, _host_info, _plugin_instance) = test_util::make_test_plugin();
    let logger = neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::global()
        .expect("NamLogger should be initialized");
    let _level_guard = LogLevelGuard::new(logger, LevelFilter::Info);
    let nonce = LOG_BUFFER_TEST_NONCE.fetch_add(1, Ordering::Relaxed);
    let expected = format!("CLAP integration log test: reaching LogBuffer nonce={nonce}");

    log::info!("{expected}");

    let snapshot = neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::log_buffer()
        .expect("LogBuffer should be accessible")
        .snapshot();
    assert!(
        snapshot.iter().any(|record| record.message == expected),
        "LogBuffer should contain the unique test message: {expected}"
    );
}

#[test]
fn test_log_info_reaches_host_log_sink() {
    let (_entry, _host_info, _plugin_instance) = test_util::make_test_plugin();

    let (captured, _sink_arc) = test_util::register_test_sink();

    log::info!("CLAP integration log test: reaching HostLog sink");

    let captured_msgs = captured.lock().unwrap();
    let found = captured_msgs
        .iter()
        .any(|(severity, msg)| *severity == "INFO" && msg.contains("HostLog sink"));
    assert!(
        found,
        "HostLog sink should have received the log message.\nCaptured: {captured_msgs:#?}"
    );
}

#[test]
fn test_log_error_levels_reach_both_sinks() {
    let (_entry, _host_info, _plugin_instance) = test_util::make_test_plugin();
    let (captured, _sink_arc) = test_util::register_test_sink();

    log::error!("CLAP integration: error level test");
    log::warn!("CLAP integration: warn level test");

    test_util::assert_log_buffer_contains("CLAP integration: error level test");
    test_util::assert_log_buffer_contains("CLAP integration: warn level test");

    let captured_msgs = captured.lock().unwrap();
    let has_error = captured_msgs
        .iter()
        .any(|(s, m)| *s == "ERROR" && m.contains("error level test"));
    let has_warn = captured_msgs
        .iter()
        .any(|(s, m)| *s == "WARN" && m.contains("warn level test"));
    assert!(has_error, "HostLog sink should receive ERROR messages");
    assert!(has_warn, "HostLog sink should receive WARN messages");
}

#[test]
fn test_instance_sink_unregistered_on_drop() {
    let (_entry, _host_info, plugin_instance) = test_util::make_test_plugin();
    let _logger = neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::global()
        .expect("NamLogger should be initialized");

    // The plugin instance was created and its sink registered.
    // Drop the plugin instance; this triggers NamClapMainThread::drop which
    // unregisters the instance sink from NamLogger and quiesces the bridge.
    drop(plugin_instance);

    // After drop, logging should execute cleanly without panic, UAF, or deadlock.
    log::info!("Log dispatched after plugin instance drop");
    log::warn!("Warning dispatched after plugin instance drop");
    log::error!("Error dispatched after plugin instance drop");
}

#[test]
fn test_concurrent_logging_during_repeated_instance_lifecycle() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    let running = Arc::new(AtomicBool::new(true));
    let mut workers = Vec::new();

    // Spawn 4 concurrent worker threads spamming the log facade
    for thread_idx in 0..4 {
        let is_running = Arc::clone(&running);
        workers.push(thread::spawn(move || {
            let mut count = 0;
            while is_running.load(Ordering::Relaxed) {
                log::info!("Worker {} iteration {}", thread_idx, count);
                log::warn!("Worker {} warning {}", thread_idx, count);
                count += 1;
                std::thread::yield_now();
            }
            count
        }));
    }

    // Repeatedly instantiate and drop plugin instances while workers log concurrently
    // 1000 cycles stress test per S3-T1 acceptance criteria.
    for _ in 0..1000 {
        let (_entry, _host_info, plugin_instance) = test_util::make_test_plugin();
        log::info!("Plugin lifecycle in-flight log");
        drop(plugin_instance);
    }

    running.store(false, Ordering::Release);

    for handle in workers {
        let log_count = handle.join().expect("Worker thread panicked");
        assert!(log_count > 0, "Worker thread should have logged");
    }
}
