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

fn receipt_with_evidence(line: &str) -> String {
    format!(
        "SUITE: tests-quick\n\
         STRICT: 1\n\
         PHASE1: PASS log=x\n\
         PHASE2: PASS log=x\n\
         {line}\n\
         PHASE3: PASS log=x\n\
         HEAP_AUDIT=RAN\n\
         OVERALL: PASSED\n"
    )
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
fn accepts_producer_evidence_with_numeric_metrics() {
    // Exact line emitted by utils/tests-quick.sh for the static codegen guard
    // (seen in target/logs/quick-receipt.txt).
    let text = receipt_with_evidence(
        "RT_CODEGEN: PASS symbols=7 instructions=14115 vfmadd=14 targets_matched=7 targets_total=7 required_matched=3 required_total=3",
    );
    let r = parse_quick_receipt(&text).unwrap();
    let metrics = r.evidence_metrics.get("RT_CODEGEN").expect("metrics");
    assert_eq!(metrics.get("symbols"), Some(&7));
    assert_eq!(metrics.get("instructions"), Some(&14115));
    assert_eq!(metrics.get("required_total"), Some(&3));
    assert!(r.verify_release_gate(true, true).is_ok());
}

#[test]
fn rejects_malformed_evidence_metrics() {
    let cases = [
        ("RT_CODEGEN: PASS foo", "foo"),
        ("RT_CODEGEN: PASS symbols=", "symbols="),
        ("RT_CODEGEN: PASS symbols=7 junk", "junk"),
        ("RT_CODEGEN: FAIL symbols=1", "FAIL"),
        ("RT_CODEGEN: PASS symbols=7 symbols=8", "symbols=8"),
    ];
    for (line, offender) in cases {
        let err = parse_quick_receipt(&receipt_with_evidence(line)).unwrap_err();
        assert!(
            err.contains("line 5"),
            "missing line number for {line:?}: {err}"
        );
        assert!(
            err.contains(offender),
            "missing offending token {offender:?} for {line:?}: {err}"
        );
    }
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

#[test]
fn parses_receipt_with_bolt_symbols_provenance() {
    let text = "SUITE: tests-quick\n\
                STRICT: 1\n\
                PHASE1: PASS log=x\n\
                PHASE2: PASS log=x\n\
                RT_CODEGEN: PASS\n\
                PRE_BOLT_SYMBOLS: 7\n\
                POST_BOLT_SYMBOLS: n/a\n\
                PHASE3: PASS log=x\n\
                HEAP_AUDIT=RAN\n\
                OVERALL: PASSED\n";
    let r = parse_quick_receipt(text).expect("parsed");
    assert_eq!(r.pre_bolt_symbols.as_deref(), Some("7"));
    assert_eq!(r.post_bolt_symbols.as_deref(), Some("n/a"));
    assert!(r.verify_release_gate(true, true).is_ok());

    let text_both_numeric = "SUITE: tests-quick\n\
                             STRICT: 1\n\
                             PHASE1: PASS log=x\n\
                             PHASE2: PASS log=x\n\
                             RT_CODEGEN: PASS\n\
                             PRE_BOLT_SYMBOLS: 7\n\
                             POST_BOLT_SYMBOLS: 7\n\
                             PHASE3: PASS log=x\n\
                             HEAP_AUDIT=RAN\n\
                             OVERALL: PASSED\n";
    let r2 = parse_quick_receipt(text_both_numeric).expect("parsed");
    assert_eq!(r2.pre_bolt_symbols.as_deref(), Some("7"));
    assert_eq!(r2.post_bolt_symbols.as_deref(), Some("7"));
}

#[test]
fn rejects_invalid_and_duplicate_bolt_symbols() {
    let invalid_pre = "SUITE: tests-quick\n\
                       STRICT: 1\n\
                       PHASE1: PASS log=x\n\
                       PHASE2: PASS log=x\n\
                       PRE_BOLT_SYMBOLS: invalid\n\
                       PHASE3: PASS log=x\n\
                       HEAP_AUDIT=RAN\n\
                       OVERALL: PASSED\n";
    let err = parse_quick_receipt(invalid_pre).unwrap_err();
    assert!(err.contains("invalid PRE_BOLT_SYMBOLS value"));

    let invalid_post = "SUITE: tests-quick\n\
                        STRICT: 1\n\
                        PHASE1: PASS log=x\n\
                        PHASE2: PASS log=x\n\
                        POST_BOLT_SYMBOLS: -1\n\
                        PHASE3: PASS log=x\n\
                        HEAP_AUDIT=RAN\n\
                        OVERALL: PASSED\n";
    let err = parse_quick_receipt(invalid_post).unwrap_err();
    assert!(err.contains("invalid POST_BOLT_SYMBOLS value"));

    let duplicate_pre = "SUITE: tests-quick\n\
                         STRICT: 1\n\
                         PHASE1: PASS log=x\n\
                         PHASE2: PASS log=x\n\
                         PRE_BOLT_SYMBOLS: 7\n\
                         PRE_BOLT_SYMBOLS: 8\n\
                         PHASE3: PASS log=x\n\
                         HEAP_AUDIT=RAN\n\
                         OVERALL: PASSED\n";
    let err = parse_quick_receipt(duplicate_pre).unwrap_err();
    assert!(err.contains("duplicate PRE_BOLT_SYMBOLS: line"));
}

#[test]
fn verifies_producer_contract_with_tests_quick_script() {
    // Contract test: every line key emitted via `emit "KEY: ..."` or `emit "KEY=..."`
    // in `utils/tests-quick.sh` must be recognized by `parse_quick_receipt` so that
    // producer and consumer never drift apart silently.
    let script_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("utils/tests-quick.sh");
    let script = std::fs::read_to_string(&script_path).expect("read tests-quick.sh");

    let mut emitted_keys = std::collections::BTreeSet::new();
    for line in script.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("emit \"") {
            let token = rest
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_end_matches('"');
            if let Some((k, _)) = token.split_once(':') {
                emitted_keys.insert(k.to_string());
            } else if let Some((k, _)) = token.split_once('=') {
                emitted_keys.insert(k.to_string());
            }
        }
    }

    // Known dynamic prefixes handled by pattern matching in the parser:
    // e.g. PHASE${PHASE_NUM:-?}, PHASE${PHASE_NUM:-?}_COMPILE_S, PHASE${PHASE_NUM:-?}_RUN_S
    for key in &emitted_keys {
        let is_known = match key.as_str() {
            "SUITE" | "STRICT" | "PHASE1" | "PHASE2" | "PHASE3" | "ORACLE_SHA256"
            | "FIXTURE_SHA256" | "ARTIFACT_SHA256" | "CLAP_CPP_PARITY" | "CLAP_OS_PARITY"
            | "CABSIM_IR" | "CABSIM_PARITY" | "RT_CODEGEN" | "PRE_BOLT_SYMBOLS"
            | "POST_BOLT_SYMBOLS" | "HEAP_AUDIT" | "GAP" | "OVERALL" => true,
            k if k.starts_with("PHASE") => true,
            _ => false,
        };
        assert!(
            is_known,
            "Key {key:?} is emitted by utils/tests-quick.sh but not recognized in the parser contract!"
        );
    }
}

#[test]
fn accepts_build_and_codegen_telemetry_lines() {
    let text = "SUITE: tests-quick\n\
                STRICT: 1\n\
                PHASE1_BUILD_S: 190\n\
                PHASE1: PASS log=x\n\
                PHASE2_BUILD_S: 550\n\
                PHASE2_CODEGEN_S: 3\n\
                PHASE2: PASS log=x\n\
                PHASE3: PASS log=x\n\
                HEAP_AUDIT=RAN\n\
                OVERALL: PASSED\n";
    let r = parse_quick_receipt(text).expect("parsed");
    assert!(r.verify_release_gate(true, true).is_ok());

    let bad = text.replace("PHASE2_CODEGEN_S: 3", "PHASE2_CODEGEN_S: soon");
    assert!(
        parse_quick_receipt(&bad)
            .unwrap_err()
            .contains("invalid telemetry duration")
    );
}
