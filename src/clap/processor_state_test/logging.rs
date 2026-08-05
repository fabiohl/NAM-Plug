// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use crate::clap::test_util;

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

    let snapshot_before =
        neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::log_buffer()
            .expect("LogBuffer should be accessible")
            .len();

    log::info!("CLAP integration log test: reaching LogBuffer");

    let snapshot_after =
        neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::log_buffer()
            .expect("LogBuffer should be accessible")
            .len();
    assert!(
        snapshot_after > snapshot_before,
        "LogBuffer should have new entries after log::info! call"
    );

    test_util::assert_log_buffer_contains("CLAP integration log test: reaching LogBuffer");
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
