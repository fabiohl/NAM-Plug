// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

#[test]
fn formats_known_instants_as_iso8601_utc() {
    assert_eq!(format_iso8601(0), "1970-01-01T00:00:00Z");
    assert_eq!(format_iso8601(951_782_400), "2000-02-29T00:00:00Z");
    assert_eq!(format_iso8601(1_790_000_000), "2026-09-21T14:13:20Z");
}

#[test]
fn parses_cpu_list_ranges_and_singletons() {
    assert_eq!(parse_cpu_list("0-3,8,10-11"), vec![0, 1, 2, 3, 8, 10, 11]);
    assert_eq!(parse_cpu_list("8"), vec![8]);
    assert_eq!(parse_cpu_list(""), Vec::<usize>::new());
    assert_eq!(parse_cpu_list("9-8"), Vec::<usize>::new());
}

fn isolated_host() -> HostEnvironment {
    HostEnvironment {
        bench_core: Some(8),
        cpus_allowed_list: "8".to_string(),
        sched_policy: "SCHED_FIFO".to_string(),
        governor: Some("performance".to_string()),
        boost: Some("0".to_string()),
        cpu_online: "0-15".to_string(),
        cpu_isolated: "8-9".to_string(),
        irq_isolation: "isolated".to_string(),
        irqs_sharing_bench_core: Vec::new(),
    }
}

#[test]
fn fully_isolated_host_has_no_degradation_reasons() {
    assert!(host_degradation_reasons(&isolated_host()).is_empty());
}

#[test]
fn records_isolation_gaps_without_treating_them_as_noise() {
    let mut host = isolated_host();
    host.cpu_isolated = String::new();
    host.irq_isolation = "shared".to_string();
    host.irqs_sharing_bench_core = vec!["25".to_string(), "42".to_string()];
    host.sched_policy = "SCHED_OTHER".to_string();
    host.governor = Some("powersave".to_string());
    host.boost = Some("1".to_string());

    let reasons = host_degradation_reasons(&host);
    for expected in [
        "bench_core_not_isolated:8",
        "irq_isolation_shared:8:2",
        "sched_policy_not_realtime:SCHED_OTHER",
        "governor_not_performance:powersave",
        "cpu_boost_enabled",
    ] {
        assert!(
            reasons.contains(&expected.to_string()),
            "missing {expected}"
        );
    }
}

#[test]
fn resolves_pinned_core_before_isolated_fallback() {
    assert_eq!(resolve_bench_core(&[8], &[8, 9], None), Some(8));
    assert_eq!(resolve_bench_core(&[0, 1, 2, 3], &[8, 9], None), None);
    assert_eq!(
        resolve_bench_core(&[0, 1, 2, 3, 8, 9], &[8, 9], None),
        Some(8)
    );
    assert_eq!(
        resolve_bench_core(&[0, 1, 2, 3, 10], &[10, 11], None),
        Some(10)
    );
    assert_eq!(resolve_bench_core(&[0, 1, 2, 3], &[8, 9], Some(3)), Some(3));
}

fn reference_report(reference_p50_ns: f64) -> ReferenceReport {
    ReferenceReport {
        scenarios: vec![
            ReferenceScenario {
                name: "WaveNet_A1_64_samp".to_string(),
                p50_ns: reference_p50_ns,
            },
            ReferenceScenario {
                name: "LSTM_64_samp".to_string(),
                p50_ns: reference_p50_ns,
            },
        ],
    }
}

#[test]
fn reference_check_passes_within_ratio_and_fails_above() {
    let measured = vec![
        ("WaveNet_A1_64_samp".to_string(), 45_000.0),
        ("LSTM_64_samp".to_string(), 5_000.0),
    ];
    let pass = evaluate_reference_series(
        &measured,
        &reference_report(30_000.0),
        1.5,
        "ref.json",
        "sha",
    )
    .expect("reference evaluation");
    assert!(pass.passed);
    assert!((pass.worst_ratio - 1.5).abs() < 1e-9);

    let fail = evaluate_reference_series(
        &measured,
        &reference_report(20_000.0),
        1.5,
        "ref.json",
        "sha",
    )
    .expect("reference evaluation");
    assert!(!fail.passed);
    assert!(fail.worst_ratio > 1.5);
}

#[test]
fn reference_check_rejects_missing_scenario_and_invalid_ratio() {
    let measured = vec![("WaveNet_A1_64_samp".to_string(), 45_000.0)];
    let incomplete = ReferenceReport {
        scenarios: vec![ReferenceScenario {
            name: "LSTM_64_samp".to_string(),
            p50_ns: 5_000.0,
        }],
    };
    assert!(evaluate_reference_series(&measured, &incomplete, 1.5, "ref.json", "sha").is_err());
    assert!(
        evaluate_reference_series(
            &measured,
            &reference_report(30_000.0),
            0.0,
            "ref.json",
            "sha"
        )
        .is_err()
    );
}
