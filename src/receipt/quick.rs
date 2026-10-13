// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Fail-closed semantic parser for the quick-suite receipt
//! (`target/logs/quick-receipt.txt`, emitted by `utils/tests-quick.sh`).
//!
//! This is the single source of truth for the quick-receipt grammar, shared by
//! `src/bin/quick_receipt_check.rs` (the release gate invoked by
//! `utils/build-release.sh`) and the integration tests. It is deliberately
//! **not** a substring search: unknown line types, duplicated mandatory fields,
//! invalid values, a missing mandatory phase or a missing `OVERALL:` verdict are
//! all rejected, and the declared `OVERALL:` verdict is cross-checked against
//! the phase/gap evidence collected in the same receipt.
//!
//! The format (one line per entry):
//!
//! ```text
//! SUITE: tests-quick
//! STRICT: 0|1
//! PHASE1: PASS log=target/logs/quick-phase1.log
//! PHASE2: PASS log=... | PHASE2: GAP reason=...
//! PHASE3: PASS log=...
//! ORACLE_SHA256: <hex>          (optional, referenced render oracle)
//! FIXTURE_SHA256: <hex>         (optional, referenced input fixture)
//! ARTIFACT_SHA256: <hex>        (optional, referenced built artifact)
//! CLAP_CPP_PARITY: PASS         (optional evidence flag)
//! CABSIM_IR: PASS               (optional evidence flag)
//! RT_CODEGEN: PASS              (optional evidence flag)
//! HEAP_AUDIT=RAN                (optional evidence flag)
//! GAP: <typed reason>           (zero or more)
//! OVERALL: PASSED|COMPLETED_WITH_GAPS|FAIL [reason=...]
//! ```

use std::collections::{BTreeMap, BTreeSet};

/// Canonical `SUITE:` value of the quick receipt.
pub const QUICK_SUITE: &str = "tests-quick";

/// Status of one quick-suite phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuickPhaseStatus {
    /// Phase completed successfully.
    Pass,
    /// Phase bypassed itself and declared a typed gap.
    Gap,
}

/// One `PHASEn:` line of the quick receipt (a phase may emit the same verdict
/// twice — e.g. `PHASE2` declares two distinct gaps — so only a *mixed*
/// PASS/GAP verdict for one phase is inconsistent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickPhase {
    /// Phase id, e.g. `PHASE2`.
    pub id: String,
    /// Aggregated phase verdict (Gap wins over Pass; mixed is rejected).
    pub status: QuickPhaseStatus,
    /// `log=...` value when the PASS form was used.
    pub log: Option<String>,
    /// `reason=...` value(s) when the GAP form was used.
    pub reasons: Vec<String>,
}

/// Fail-closed parsed representation of `target/logs/quick-receipt.txt`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickReceipt {
    /// `SUITE:` value (must be `tests-quick`).
    pub suite: String,
    /// `STRICT:` flag.
    pub strict: bool,
    /// Parsed phase lines (`PHASE1..PHASE3` mandatory).
    pub phases: Vec<QuickPhase>,
    /// Referenced render-oracle hash.
    pub oracle_sha256: Option<String>,
    /// Referenced input-fixture hash.
    pub fixture_sha256: Option<String>,
    /// Referenced built-artifact hash.
    pub artifact_sha256: Option<String>,
    /// Evidence flags proven by the run (`CLAP_CPP_PARITY`, `CABSIM_IR`,
    /// `RT_CODEGEN`, `HEAP_AUDIT`).
    pub evidence: BTreeSet<String>,
    /// Typed `GAP:` reasons.
    pub gaps: Vec<String>,
    /// `OVERALL:` verdict token.
    pub overall: String,
}

/// Mandatory quick-suite phases.
pub const QUICK_PHASES: [&str; 3] = ["PHASE1", "PHASE2", "PHASE3"];

/// Canonical evidence flag identifiers.
const EVIDENCE_FLAGS: [&str; 3] = ["CLAP_CPP_PARITY", "CABSIM_IR", "RT_CODEGEN"];

/// Parses an optional `key=value` token from a detail string.
fn parse_kv<'a>(tokens: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("{key}=");
    tokens
        .split_whitespace()
        .find_map(|t| t.strip_prefix(&needle))
}

/// Accumulator for a phase line observed in the receipt (a phase may emit its
/// verdict once, or twice for distinct gaps).
#[derive(Default)]
struct PhaseAcc {
    saw_pass: bool,
    saw_gap: bool,
    log: Option<String>,
    reasons: Vec<String>,
}

/// Parses a phase line (`PHASEn: <status> ...`). Returns the phase, or `None`
/// when the line is a phase line but structurally malformed.
fn parse_quick_phase(
    line: &str,
) -> Option<(String, QuickPhaseStatus, Option<String>, Option<String>)> {
    let rest = line.strip_prefix("PHASE")?;
    let (num, tail) = rest.split_once(':')?;
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let id = format!("PHASE{num}");
    let tail = tail.trim();
    if let Some(log) = tail.strip_prefix("PASS") {
        return Some((
            id,
            QuickPhaseStatus::Pass,
            parse_kv(log, "log").map(str::to_string),
            None,
        ));
    }
    if let Some(detail) = tail.strip_prefix("GAP") {
        return Some((
            id,
            QuickPhaseStatus::Gap,
            None,
            Some(parse_kv(detail, "reason").unwrap_or_default().to_string()),
        ));
    }
    None
}

/// Fail-closed parser for the quick-suite receipt format.
pub fn parse_quick_receipt(text: &str) -> Result<QuickReceipt, String> {
    let mut suite: Option<String> = None;
    let mut strict: Option<bool> = None;
    // phase id -> accumulator
    let mut phases: BTreeMap<String, PhaseAcc> = BTreeMap::new();
    let mut oracle_sha256: Option<String> = None;
    let mut fixture_sha256: Option<String> = None;
    let mut artifact_sha256: Option<String> = None;
    let mut evidence: BTreeSet<String> = BTreeSet::new();
    let mut gaps: Vec<String> = Vec::new();
    let mut overall: Option<String> = None;

    for (idx, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let lineno = idx + 1;
        if let Some(v) = line.strip_prefix("SUITE:") {
            if suite.replace(v.trim().to_string()).is_some() {
                return Err(format!("line {lineno}: duplicate SUITE: line"));
            }
        } else if let Some(v) = line.strip_prefix("STRICT:") {
            let parsed = match v.trim() {
                "0" => false,
                "1" => true,
                other => {
                    return Err(format!(
                        "line {lineno}: invalid STRICT: value {other:?} (expected 0 or 1)"
                    ));
                }
            };
            if strict.replace(parsed).is_some() {
                return Err(format!("line {lineno}: duplicate STRICT: line"));
            }
        } else if let Some(v) = line.strip_prefix("OVERALL:") {
            let token = v.split_whitespace().next().unwrap_or("").to_string();
            if !matches!(token.as_str(), "PASSED" | "COMPLETED_WITH_GAPS" | "FAIL") {
                return Err(format!("line {lineno}: invalid OVERALL: verdict {token:?}"));
            }
            if overall.replace(token).is_some() {
                return Err(format!("line {lineno}: duplicate OVERALL: line"));
            }
        } else if let Some(v) = line.strip_prefix("GAP:") {
            gaps.push(v.trim().to_string());
        } else if line.starts_with("PHASE") {
            let (id, status, log, reason) = parse_quick_phase(line)
                .ok_or_else(|| format!("line {lineno}: malformed phase line: {line:?}"))?;
            let entry = phases.entry(id).or_default();
            match status {
                QuickPhaseStatus::Pass => {
                    entry.saw_pass = true;
                    if let Some(log) = log {
                        entry.log = Some(log);
                    }
                }
                QuickPhaseStatus::Gap => {
                    entry.saw_gap = true;
                    if let Some(r) = reason {
                        entry.reasons.push(r);
                    }
                }
            }
        } else if let Some(v) = line.strip_prefix("ORACLE_SHA256:") {
            if oracle_sha256.replace(v.trim().to_string()).is_some() {
                return Err(format!("line {lineno}: duplicate ORACLE_SHA256: line"));
            }
        } else if let Some(v) = line.strip_prefix("FIXTURE_SHA256:") {
            if fixture_sha256.replace(v.trim().to_string()).is_some() {
                return Err(format!("line {lineno}: duplicate FIXTURE_SHA256: line"));
            }
        } else if let Some(v) = line.strip_prefix("ARTIFACT_SHA256:") {
            if artifact_sha256.replace(v.trim().to_string()).is_some() {
                return Err(format!("line {lineno}: duplicate ARTIFACT_SHA256: line"));
            }
        } else if let Some(v) = line.strip_prefix("HEAP_AUDIT=") {
            if v.trim() != "RAN" {
                return Err(format!(
                    "line {lineno}: invalid HEAP_AUDIT value {:?} (expected RAN)",
                    v.trim()
                ));
            }
            if !evidence.insert("HEAP_AUDIT".to_string()) {
                return Err(format!("line {lineno}: duplicate HEAP_AUDIT line"));
            }
        } else if let Some((key, v)) = line.split_once(':') {
            let key = key.trim();
            if EVIDENCE_FLAGS.contains(&key) {
                if v.trim() != "PASS" {
                    return Err(format!(
                        "line {lineno}: invalid {key} value {:?} (expected PASS)",
                        v.trim()
                    ));
                }
                if !evidence.insert(key.to_string()) {
                    return Err(format!("line {lineno}: duplicate {key}: line"));
                }
            } else {
                return Err(format!(
                    "line {lineno}: unrecognized receipt line: {line:?}"
                ));
            }
        } else {
            return Err(format!(
                "line {lineno}: unrecognized receipt line: {line:?}"
            ));
        }
    }

    let suite = suite.ok_or("missing SUITE: line")?;
    let strict = strict.ok_or("missing STRICT: line")?;
    let overall = overall.ok_or("missing OVERALL: line")?;

    let mut out_phases = Vec::new();
    for id in QUICK_PHASES {
        if !phases.contains_key(id) {
            return Err(format!("missing mandatory phase {id} in receipt"));
        }
    }
    for (id, acc) in phases {
        if acc.saw_pass && acc.saw_gap {
            return Err(format!(
                "inconsistent receipt: phase {id} declares both PASS and GAP"
            ));
        }
        out_phases.push(QuickPhase {
            id,
            status: if acc.saw_gap {
                QuickPhaseStatus::Gap
            } else {
                QuickPhaseStatus::Pass
            },
            log: acc.log,
            reasons: acc.reasons,
        });
    }

    Ok(QuickReceipt {
        suite,
        strict,
        phases: out_phases,
        oracle_sha256,
        fixture_sha256,
        artifact_sha256,
        evidence,
        gaps,
        overall,
    })
}

impl QuickReceipt {
    /// Semantic consistency audit (fail-closed): the `OVERALL:` verdict must
    /// match the phase/gap evidence in the same receipt.
    pub fn audit(&self) -> Result<(), String> {
        if self.suite != QUICK_SUITE {
            return Err(format!(
                "expected SUITE: {QUICK_SUITE}, got {:?}",
                self.suite
            ));
        }
        let has_gap = self
            .phases
            .iter()
            .any(|p| p.status == QuickPhaseStatus::Gap)
            || !self.gaps.is_empty();
        match self.overall.as_str() {
            "PASSED" if !has_gap => Ok(()),
            "COMPLETED_WITH_GAPS" if has_gap => Ok(()),
            "FAIL" if self.strict => Ok(()),
            other => Err(format!(
                "inconsistent receipt: OVERALL={other} (strict={}, gap_evidence={has_gap})",
                self.strict
            )),
        }
    }

    /// Release-gate verdict: the receipt must audit cleanly and, when
    /// `require_passed` is set, must additionally close `OVERALL: PASSED` with
    /// all mandatory phases passed and the zero-allocation heap audit proven.
    pub fn verify_release_gate(
        &self,
        require_passed: bool,
        require_heap_audit: bool,
    ) -> Result<(), String> {
        self.audit()?;
        if require_heap_audit && !self.evidence.contains("HEAP_AUDIT") {
            return Err("heap audit evidence missing (HEAP_AUDIT=RAN not present)".into());
        }
        if require_passed {
            if self.overall != "PASSED" {
                return Err(format!(
                    "release gate requires OVERALL: PASSED (got {:?})",
                    self.overall
                ));
            }
            if let Some(p) = self
                .phases
                .iter()
                .find(|p| p.status != QuickPhaseStatus::Pass)
            {
                return Err(format!("release gate requires phase {} to PASS", p.id));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "quick_test.rs"]
mod tests;
