// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

fn calibrated_host() -> HostEnvironment {
    HostEnvironment {
        bench_core: Some(8),
        affinity_applied: true,
        affinity_observed_core: Some(8),
        affinity_error: None,
        cpus_allowed_list: "8".to_string(),
        sched_policy: "SCHED_FIFO".to_string(),
        governor: Some("performance".to_string()),
        boost: Some("1".to_string()),
        cpu_online: "0-15".to_string(),
        cpu_isolated: "8-9".to_string(),
        irq_isolation: "isolated".to_string(),
        irqs_sharing_bench_core: Vec::new(),
    }
}

#[test]
fn calibrated_host_is_not_degraded_even_with_cpufreq_boost_enabled() {
    let host = calibrated_host();
    assert!(
        host_degradation_reasons(&host).is_empty(),
        "calibrated isolated host with SCHED_FIFO and performance governor must not degrade on boost metadata"
    );
}

#[test]
fn host_degradation_flags_non_rt_policy_and_shared_irq() {
    let mut host = calibrated_host();
    host.sched_policy = "SCHED_OTHER".to_string();
    host.irq_isolation = "shared".to_string();
    host.irqs_sharing_bench_core = vec!["62".to_string()];
    let reasons = host_degradation_reasons(&host);
    assert!(
        reasons
            .iter()
            .any(|r| r == "sched_policy_not_realtime:SCHED_OTHER"),
        "expected sched_policy_not_realtime, got {reasons:?}"
    );
    assert!(
        reasons.iter().any(|r| r == "irq_isolation_shared:8:1"),
        "expected irq_isolation_shared:8:1, got {reasons:?}"
    );
}

#[test]
fn reference_series_passes_within_noise_floor_even_if_ratio_exceeds_threshold() {
    let measured = vec![ScenarioPerfMetrics {
        name: "LSTM_64_samp".to_string(),
        topology: "LSTM 1x3".to_string(),
        channels: "mono".to_string(),
        sample_rate: 48000.0,
        block_size: 64,
        oversample: "Off".to_string(),
        cabinet: "none".to_string(),
        events: "none".to_string(),
        iterations: 300,
        p50_ns: 8102.0,
        p95_ns: 8521.0,
        p99_ns: 13759.0, // 4.19 µs delta over reference
        max_ns: 13759.0,
        mean_ns: 8158.4,
        stddev_ns: 612.0,
        ns_per_sample: 127.5,
        deadline_budget_ns: 1333333.3,
        gate_threshold_ns: 1133333.3,
        deadline_margin_percent: 98.9,
        gate_headroom_percent: 98.7,
        deadline_passed: true,
        process_errors: 0,
        status_rejected: 0,
        probe_passed: true,
        probe_max_abs_diff: 0.1,
        probe_rms_diff: 0.1,
        cab_probe_passed: true,
        cab_probe_max_abs_diff: 0.0,
    }];
    let reference = ReferenceReport {
        clap_artifact_sha256: Some("dummy".to_string()),
        scenarios: vec![ReferenceScenario {
            name: "LSTM_64_samp".to_string(),
            p50_ns: Some(8102.0),
            p99_ns: Some(9568.0),
            max_ns: Some(14248.0),
        }],
    };

    let check = evaluate_reference_series(
        &measured, &reference, 1.25, 1.5, 25_000.0, // 25 µs noise floor
        "ref.json", "sha",
    )
    .expect("evaluation should succeed");

    assert_eq!(check.status, "PASS");
    assert!(check.passed);
    assert_eq!(check.comparisons[0].p99_passed, Some(true));
}

#[test]
fn reference_series_fails_when_both_ratio_and_noise_floor_exceeded() {
    let measured = vec![ScenarioPerfMetrics {
        name: "LSTM_64_samp".to_string(),
        topology: "LSTM 1x3".to_string(),
        channels: "mono".to_string(),
        sample_rate: 48000.0,
        block_size: 64,
        oversample: "Off".to_string(),
        cabinet: "none".to_string(),
        events: "none".to_string(),
        iterations: 300,
        p50_ns: 8102.0,
        p95_ns: 8521.0,
        p99_ns: 60000.0, // 50.4 µs delta over reference (> 25 µs noise floor)
        max_ns: 60000.0,
        mean_ns: 15000.0,
        stddev_ns: 1000.0,
        ns_per_sample: 250.0,
        deadline_budget_ns: 1333333.3,
        gate_threshold_ns: 1133333.3,
        deadline_margin_percent: 95.0,
        gate_headroom_percent: 94.0,
        deadline_passed: true,
        process_errors: 0,
        status_rejected: 0,
        probe_passed: true,
        probe_max_abs_diff: 0.1,
        probe_rms_diff: 0.1,
        cab_probe_passed: true,
        cab_probe_max_abs_diff: 0.0,
    }];
    let reference = ReferenceReport {
        clap_artifact_sha256: Some("dummy".to_string()),
        scenarios: vec![ReferenceScenario {
            name: "LSTM_64_samp".to_string(),
            p50_ns: Some(8102.0),
            p99_ns: Some(9568.0),
            max_ns: Some(14248.0),
        }],
    };

    let check = evaluate_reference_series(
        &measured, &reference, 1.25, 1.5, 25_000.0, "ref.json", "sha",
    )
    .expect("evaluation should succeed");

    assert_eq!(check.status, "FAIL");
    assert!(!check.passed);
    assert_eq!(check.comparisons[0].p99_passed, Some(false));
}
