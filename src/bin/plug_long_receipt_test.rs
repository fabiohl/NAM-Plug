// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

#[test]
fn test_compute_build_status_strict_fatal_gaps() {
    let gaps = vec!["pgo_not_applied".to_string()];
    let (status, fatal) = compute_build_status(&gaps, "skipped", true);
    assert_eq!(status, "FAILED");
    assert_eq!(fatal, vec!["pgo_not_applied"]);

    let gaps = vec!["bolt_not_applied".to_string()];
    let (status, fatal) = compute_build_status(&gaps, "unverified:missing", true);
    assert_eq!(status, "FAILED");
    assert_eq!(fatal, vec!["bolt_not_applied"]);
}

#[test]
fn test_compute_build_status_fallback_plain() {
    let gaps = vec![
        "bolt_not_applied".to_string(),
        "bolt_not_proven".to_string(),
        "pgo_not_applied".to_string(),
    ];
    let (status, fatal) = compute_build_status(&gaps, "fallback_plain", true);
    assert_eq!(status, "PASS_WITH_GAPS");
    assert!(fatal.is_empty());

    let gaps = vec!["bolt_not_applied".to_string(), "tool_missing".to_string()];
    let (status, fatal) = compute_build_status(&gaps, "fallback_plain", true);
    assert_eq!(status, "FAILED");
    assert_eq!(fatal, vec!["tool_missing"]);
}

#[test]
fn test_compute_build_status_fallback_pgo_only() {
    let gaps = vec![
        "bolt_not_applied".to_string(),
        "bolt_not_proven".to_string(),
    ];
    let (status, fatal) = compute_build_status(&gaps, "fallback_pgo_only", true);
    assert_eq!(status, "PASS_WITH_GAPS");
    assert!(fatal.is_empty());

    let gaps = vec!["bolt_not_proven".to_string(), "pgo_not_applied".to_string()];
    let (status, fatal) = compute_build_status(&gaps, "fallback_pgo_only", true);
    assert_eq!(status, "FAILED");
    assert_eq!(fatal, vec!["pgo_not_applied"]);
}

#[test]
fn test_compute_build_status_non_strict() {
    let gaps = vec!["pgo_not_applied".to_string()];
    let (status, fatal) = compute_build_status(&gaps, "skipped", false);
    assert_eq!(status, "PASS_WITH_GAPS");
    assert_eq!(fatal, vec!["pgo_not_applied"]);
}

#[test]
fn test_compute_build_status_clean_pass() {
    let gaps = Vec::new();
    let (status, fatal) = compute_build_status(&gaps, "applied", true);
    assert_eq!(status, "PASSED");
    assert!(fatal.is_empty());
}

#[test]
fn test_ceremony_proof_missing_file_rejected() {
    let res = validate_ab_opt_ceremony_receipt(
        std::path::Path::new("target/logs/non_existent_ceremony.json"),
        None,
        None,
    );
    assert!(res.is_err());
    assert_eq!(res.unwrap_err().reason_code(), "ceremony_missing_receipt");
}

#[test]
fn test_push_optimization_gaps_clean_fallbacks_have_no_gaps() {
    let mut gaps_pgo_only = Vec::new();
    push_optimization_gaps(
        &mut gaps_pgo_only,
        true,
        false,
        "not_proven",
        "fallback_pgo_only",
    );
    assert!(
        gaps_pgo_only.is_empty(),
        "fallback_pgo_only should not emit optimization gaps: {gaps_pgo_only:?}"
    );

    let mut gaps_plain = Vec::new();
    push_optimization_gaps(&mut gaps_plain, false, false, "skipped", "fallback_plain");
    assert!(
        gaps_plain.is_empty(),
        "fallback_plain should not emit optimization gaps: {gaps_plain:?}"
    );
}

#[test]
fn test_push_optimization_gaps_flags_unproven_without_clean_fallback() {
    let mut gaps = Vec::new();
    push_optimization_gaps(&mut gaps, true, false, "not_proven", "real-proof-id");
    assert_eq!(gaps, vec!["bolt_not_proven"]);

    let mut missing_pgo = Vec::new();
    push_optimization_gaps(
        &mut missing_pgo,
        false,
        false,
        "not_proven",
        "fallback_pgo_only",
    );
    assert_eq!(missing_pgo, vec!["pgo_not_applied"]);
}
