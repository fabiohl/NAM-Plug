// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Synthetic tamper-rejection tests for the NAM-Plug receipt stack (S4-T4.3).
//!
//! The release ceremonies must reject a tampered or simulated receipt. These
//! tests exercise the same semantic verifiers the gate uses — the quick text
//! parser ([`nam_plug::receipt::quick`]) and the unified-schema long verifier
//! (engine `testing::receipt` + the Plug roster) — with forged green verdicts,
//! injected verdict comments, missing phases and simulated runs.

use nam_plug::receipt::LONG_ROSTER;
use nam_plug::receipt::quick::{QuickPhaseStatus, parse_quick_receipt};
use neural_amp_modeler_rs::testing::receipt::{
    LongAuditReceipt, LongPhaseReceipt, LongPhaseStatus,
};

fn phase(id: &str, status: LongPhaseStatus, tests: u64) -> LongPhaseReceipt {
    LongPhaseReceipt {
        phase_id: id.to_string(),
        name: id.to_string(),
        status,
        duration_ms: 1000,
        tests_executed: tests,
        gaps: Vec::new(),
        timestamp: "2026-10-13T00:00:00Z".to_string(),
        ..Default::default()
    }
}

fn clean_long_jsonl() -> String {
    let phases = vec![
        phase("phase1", LongPhaseStatus::Passed, 10),
        phase("phase2", LongPhaseStatus::Passed, 10),
        phase("phase3", LongPhaseStatus::Passed, 10),
        phase("phase4", LongPhaseStatus::Passed, 10),
        phase("overall", LongPhaseStatus::Passed, 40),
    ];
    LongAuditReceipt { phases }.render_jsonl()
}

#[test]
fn clean_long_receipt_certifies_under_plug_roster() {
    let parsed = LongAuditReceipt::parse_jsonl(&clean_long_jsonl()).unwrap();
    assert!(parsed.semantic_verdict_with_roster(&LONG_ROSTER).is_ok());
    assert!(
        parsed
            .campaign_strict_verdict_with_roster(&LONG_ROSTER)
            .is_ok()
    );
}

#[test]
fn forged_green_over_gap_is_rejected() {
    let mut phases = vec![
        phase("phase1", LongPhaseStatus::Passed, 10),
        phase("phase2", LongPhaseStatus::Passed, 10),
        // A real gap, but the terminal verdict is still forged PASSED.
        phase("phase3", LongPhaseStatus::Skipped, 0),
        phase("phase4", LongPhaseStatus::Passed, 10),
        phase("overall", LongPhaseStatus::Passed, 30),
    ];
    phases[2].gaps.push("phase3:declared_gap".to_string());
    let jsonl = LongAuditReceipt { phases }.render_jsonl();
    let parsed = LongAuditReceipt::parse_jsonl(&jsonl).unwrap();
    let err = parsed
        .semantic_verdict_with_roster(&LONG_ROSTER)
        .unwrap_err();
    assert!(err.contains("disagrees"), "unexpected error: {err}");
    assert!(
        parsed
            .campaign_strict_verdict_with_roster(&LONG_ROSTER)
            .is_err()
    );
}

#[test]
fn injected_overall_comment_line_is_rejected_at_parse() {
    let tampered = format!("{}# OVERALL: PASSED\n", clean_long_jsonl());
    assert!(LongAuditReceipt::parse_jsonl(&tampered).is_err());
}

#[test]
fn missing_phase_is_rejected() {
    let phases = vec![
        phase("phase1", LongPhaseStatus::Passed, 10),
        phase("phase2", LongPhaseStatus::Passed, 10),
        phase("phase3", LongPhaseStatus::Passed, 10),
        phase("overall", LongPhaseStatus::Passed, 30),
    ];
    let parsed = LongAuditReceipt { phases };
    assert!(
        parsed
            .semantic_verdict_with_roster(&LONG_ROSTER)
            .unwrap_err()
            .contains("missing")
    );
}

#[test]
fn simulated_long_receipt_is_rejected_by_strict_gate() {
    let phases = vec![
        phase("phase1", LongPhaseStatus::Simulated, 0),
        phase("phase2", LongPhaseStatus::Simulated, 0),
        phase("phase3", LongPhaseStatus::Simulated, 0),
        phase("phase4", LongPhaseStatus::Simulated, 0),
        phase("overall", LongPhaseStatus::Simulated, 0),
    ];
    let parsed = LongAuditReceipt { phases };
    // Consistent (SIMULATED everywhere) → not a semantic-structure failure…
    assert!(parsed.semantic_verdict_with_roster(&LONG_ROSTER).is_ok());
    // …but a strict release ceremony rejects it fail-closed.
    assert!(
        parsed
            .campaign_strict_verdict_with_roster(&LONG_ROSTER)
            .is_err()
    );
}

#[test]
fn quick_receipt_rejects_forgery_and_injection() {
    let green_forgery = "SUITE: tests-quick\n\
                         STRICT: 0\n\
                         PHASE1: PASS log=x\n\
                         PHASE2: GAP reason=missing_render_or_fixtures\n\
                         PHASE3: PASS log=x\n\
                         OVERALL: PASSED\n";
    assert!(parse_quick_receipt(green_forgery).unwrap().audit().is_err());

    let injected = "SUITE: tests-quick\n\
                    STRICT: 0\n\
                    PHASE1: PASS log=x\n\
                    PHASE2: PASS log=x\n\
                    PHASE3: PASS log=x\n\
                    OVERALL: COMPLETED_WITH_GAPS\n\
                    # OVERALL: PASSED\n";
    assert!(parse_quick_receipt(injected).is_err());

    let ok = "SUITE: tests-quick\n\
              STRICT: 1\n\
              PHASE1: PASS log=x\n\
              PHASE2: PASS log=x\n\
              PHASE3: PASS log=x\n\
              HEAP_AUDIT=RAN\n\
              OVERALL: PASSED\n";
    let parsed = parse_quick_receipt(ok).unwrap();
    assert_eq!(parsed.phases[0].status, QuickPhaseStatus::Pass);
    assert!(parsed.verify_release_gate(true, true).is_ok());
}
