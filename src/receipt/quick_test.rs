// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

fn clean_receipt() -> String {
    "SUITE: tests-quick\n\
     STRICT: 1\n\
     PHASE1: PASS log=target/logs/quick-phase1.log\n\
     PHASE2: PASS log=target/logs/quick-phase2.log\n\
     ORACLE_SHA256: aa\n\
     FIXTURE_SHA256: bb\n\
     ARTIFACT_SHA256: cc\n\
     CLAP_CPP_PARITY: PASS\n\
     CABSIM_IR: PASS\n\
     RT_CODEGEN: PASS\n\
     PHASE3: PASS log=target/logs/quick-heap-audit.log\n\
     HEAP_AUDIT=RAN\n\
     OVERALL: PASSED\n"
        .to_string()
}

#[test]
fn parses_clean_receipt_and_audits() {
    let r = parse_quick_receipt(&clean_receipt()).unwrap();
    assert_eq!(r.suite, "tests-quick");
    assert!(r.strict);
    assert_eq!(r.overall, "PASSED");
    assert_eq!(r.phases.len(), 3);
    assert_eq!(r.oracle_sha256.as_deref(), Some("aa"));
    assert!(r.evidence.contains("HEAP_AUDIT"));
    assert!(r.evidence.contains("CLAP_CPP_PARITY"));
    assert!(r.audit().is_ok());
    assert!(r.verify_release_gate(true, true).is_ok());
}

#[test]
fn parses_receipt_with_telemetry_lines() {
    let text = "SUITE: tests-quick\n\
                STRICT: 1\n\
                PHASE1_COMPILE_S: 12\n\
                PHASE1_RUN_S: 3\n\
                PHASE1: PASS log=target/logs/quick-phase1.log\n\
                PHASE2_COMPILE_S: 0\n\
                PHASE2_RUN_S: 25\n\
                PHASE2: PASS log=target/logs/quick-phase2.log\n\
                PHASE3_COMPILE_S: 0\n\
                PHASE3_RUN_S: 5\n\
                PHASE3: PASS log=target/logs/quick-heap-audit.log\n\
                HEAP_AUDIT=RAN\n\
                OVERALL: PASSED\n";
    let r = parse_quick_receipt(text).unwrap();
    assert!(r.audit().is_ok());
    assert!(r.verify_release_gate(true, true).is_ok());

    let invalid_telemetry = "SUITE: tests-quick\n\
                             STRICT: 1\n\
                             PHASE1_COMPILE_S: not_a_number\n\
                             PHASE1: PASS log=x\n\
                             PHASE2: PASS log=x\n\
                             PHASE3: PASS log=x\n\
                             OVERALL: PASSED\n";
    assert!(parse_quick_receipt(invalid_telemetry).is_err());
}

#[test]
fn completed_with_gaps_is_consistent_but_not_release_green() {
    let text = "SUITE: tests-quick\n\
                STRICT: 0\n\
                PHASE1: PASS log=target/logs/quick-phase1.log\n\
                PHASE2: GAP reason=missing_render_or_fixtures\n\
                PHASE3: PASS log=target/logs/quick-heap-audit.log\n\
                HEAP_AUDIT=RAN\n\
                GAP: clap_parity_multi_rate:missing_render_or_fixtures\n\
                OVERALL: COMPLETED_WITH_GAPS\n";
    let r = parse_quick_receipt(text).unwrap();
    assert!(r.audit().is_ok());
    // A release ceremony must not accept a gapped quick receipt.
    assert!(r.verify_release_gate(true, true).is_err());
    // A non-strict preflight gate accepts a self-consistent gapped receipt.
    assert!(r.verify_release_gate(false, true).is_ok());
}

#[test]
fn rejects_unknown_and_duplicate_lines() {
    let unknown = clean_receipt() + "WEIRD: line\n";
    assert!(
        parse_quick_receipt(&unknown)
            .unwrap_err()
            .contains("unrecognized")
    );

    let dup = clean_receipt() + "SUITE: tests-quick\n";
    assert!(
        parse_quick_receipt(&dup)
            .unwrap_err()
            .contains("duplicate SUITE")
    );

    let missing = "SUITE: tests-quick\nSTRICT: 1\nPHASE1: PASS log=x\nOVERALL: PASSED\n";
    assert!(
        parse_quick_receipt(missing)
            .unwrap_err()
            .contains("missing mandatory phase")
    );
}

#[test]
fn rejects_injected_overall_comment() {
    // The substring-search bypass: an `OVERALL: PASSED` pasted into a comment
    // is not a receipt line and must fail closed.
    let text = clean_receipt() + "# OVERALL: PASSED\n";
    assert!(parse_quick_receipt(&text).is_err());
}

#[test]
fn rejects_forged_green_over_gap_evidence() {
    // A forged `OVERALL: PASSED` over a GAP phase must be rejected by the audit.
    let text = "SUITE: tests-quick\n\
                STRICT: 0\n\
                PHASE1: PASS log=target/logs/quick-phase1.log\n\
                PHASE2: GAP reason=missing_render_or_fixtures\n\
                PHASE3: PASS log=target/logs/quick-heap-audit.log\n\
                OVERALL: PASSED\n";
    let r = parse_quick_receipt(text).unwrap();
    assert!(r.audit().unwrap_err().contains("inconsistent"));
}

#[test]
fn release_gate_requires_heap_audit_evidence() {
    let text = "SUITE: tests-quick\n\
                STRICT: 1\n\
                PHASE1: PASS log=target/logs/quick-phase1.log\n\
                PHASE2: PASS log=target/logs/quick-phase2.log\n\
                PHASE3: PASS log=target/logs/quick-heap-audit.log\n\
                OVERALL: PASSED\n";
    let r = parse_quick_receipt(text).unwrap();
    assert!(r.verify_release_gate(true, true).is_err());
    assert!(r.verify_release_gate(true, false).is_ok());
}

#[test]
fn phase2_may_declare_two_distinct_gaps() {
    let text = "SUITE: tests-quick\n\
                STRICT: 0\n\
                PHASE1: PASS log=target/logs/quick-phase1.log\n\
                PHASE2: GAP reason=missing_render_or_fixtures\n\
                PHASE2: GAP reason=missing_release_artifact\n\
                PHASE3: PASS log=target/logs/quick-heap-audit.log\n\
                OVERALL: COMPLETED_WITH_GAPS\n";
    let r = parse_quick_receipt(text).unwrap();
    assert!(r.audit().is_ok());
    let p2 = r.phases.iter().find(|p| p.id == "PHASE2").unwrap();
    assert_eq!(p2.status, QuickPhaseStatus::Gap);
    assert_eq!(p2.reasons.len(), 2);
}

#[test]
fn mixed_pass_and_gap_for_one_phase_is_inconsistent() {
    let text = "SUITE: tests-quick\n\
                STRICT: 0\n\
                PHASE1: PASS log=x\n\
                PHASE2: PASS log=x\n\
                PHASE2: GAP reason=oops\n\
                PHASE3: PASS log=x\n\
                OVERALL: COMPLETED_WITH_GAPS\n";
    assert!(parse_quick_receipt(text).is_err());
}
