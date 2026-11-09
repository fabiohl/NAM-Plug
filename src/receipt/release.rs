// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Release receipt generation and certification for NAM-Plug.
//!
//! Under the simplified pipeline, `build-release.sh` produces `target/dist/build-receipt.json`,
//! `tests-long.sh` produces `target/logs/long-audit-receipt.jsonl`, and `build-release.sh --certify`
//! (via `plug_long_receipt certify`) reconciles both receipts and writes the authoritative
//! `target/dist/release-receipt.json` with `status: CERTIFIED` or `status: REJECTED`.
//!
//! Certification enforces:
//! - Exact matching SHA-256 for the canonical artifact across disk, build receipt, and long audit.
//! - Strict mode in both receipts (zero unapproved deviations).
//! - Zero FAIL and zero GAP across all phases.
//! - Clean schema validation (`serde(deny_unknown_fields)`).
//! - Matching run identifier (`NAM_RUN_ID`) across receipts.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::receipt::LONG_ROSTER;
use crate::receipt::build::{
    BuildOptimizations, BuildProvenance, BuildReceipt, EngineProvenance, PackageInfo,
    PgoWorkloadInfo, hash_file,
};
use crate::receipt::long_artifact::{
    ARTIFACT_EXERCISED_KEY, ARTIFACT_SHA256_KEY, ArtifactConsistency, verify_receipt_artifact,
};
fn now_iso8601() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };

    format!("{year:04}-{m:02}-{d:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

/// Authoritative release certification receipt (`target/dist/release-receipt.json`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReleaseReceipt {
    pub schema_version: String,
    pub timestamp_utc: String,
    pub status: String,
    pub strict_mode: bool,
    #[serde(default)]
    pub fails: Vec<String>,
    #[serde(default)]
    pub gaps: Vec<String>,
    #[serde(default)]
    pub reasons: Vec<String>,
    pub package: PackageInfo,
    pub provenance: BuildProvenance,
    pub engine_provenance: EngineProvenance,
    pub optimizations: BuildOptimizations,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pgo_workload: Option<PgoWorkloadInfo>,
    pub artifacts: ReleaseArtifacts,
    pub long_audit: ReleaseLongAudit,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
}

/// Release artifact details and provenance status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReleaseArtifacts {
    pub artifact_path: String,
    pub artifact_sha256: String,
    pub artifact_consistency: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tarball_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tarball_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flatpak_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flatpak_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_sha256: Option<String>,
}

/// Long-audit suite summary for release certification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReleaseLongAudit {
    pub receipt_path: String,
    pub status: String,
    pub tests_executed: u64,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub phases: Vec<ReleaseLongPhaseSummary>,
}

/// Summary of a single long-audit phase.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReleaseLongPhaseSummary {
    pub phase_id: String,
    pub name: String,
    pub status: String,
    pub duration_ms: u64,
    pub tests_executed: u64,
    #[serde(default)]
    pub gaps: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_exercised: Option<bool>,
}

/// Re-checks a `CERTIFIED` release receipt against the bytes about to be installed.
/// Returns the verified artifact SHA-256, or every reconciliation problem found.
pub fn verify_release_receipt(
    release_path: &Path,
    artifact_path: &Path,
) -> Result<String, Vec<String>> {
    let receipt: ReleaseReceipt = fs::read_to_string(release_path)
        .map_err(|e| e.to_string())
        .and_then(|c| serde_json::from_str(&c).map_err(|e| e.to_string()))
        .map_err(|e| {
            vec![format!(
                "release receipt {} unreadable or invalid: {e}",
                release_path.display()
            )]
        })?;
    let actual = hash_file(artifact_path).map_err(|e| {
        vec![format!(
            "cannot hash artifact {}: {e}",
            artifact_path.display()
        )]
    })?;

    let mut problems = Vec::new();
    if receipt.status != "CERTIFIED" {
        problems.push(format!(
            "release receipt status is '{}', expected 'CERTIFIED'",
            receipt.status
        ));
    }
    if !receipt.fails.is_empty() || !receipt.gaps.is_empty() {
        problems.push("release receipt declares fails or gaps".to_string());
    }
    if receipt.artifacts.artifact_consistency != "match" {
        problems.push(format!(
            "release receipt artifact consistency is '{}', expected 'match'",
            receipt.artifacts.artifact_consistency
        ));
    }
    if actual != receipt.artifacts.artifact_sha256 {
        problems.push(format!(
            "artifact SHA-256 {actual} differs from release receipt {}",
            receipt.artifacts.artifact_sha256
        ));
    }
    match verify_receipt_artifact(Path::new(&receipt.long_audit.receipt_path), artifact_path) {
        ArtifactConsistency::Match { declared } if declared == actual => {}
        ArtifactConsistency::Mismatch { reason, .. }
        | ArtifactConsistency::Absent { reason }
        | ArtifactConsistency::Incomplete { reason, .. } => {
            problems.push(format!("long audit artifact provenance: {reason}"));
        }
        ArtifactConsistency::Match { .. } => {
            problems.push("long audit declares a different artifact SHA".to_string());
        }
    }
    match (
        receipt.provenance.run_id.as_deref(),
        receipt.long_audit.run_id.as_deref(),
    ) {
        (Some(build), Some(long)) if !build.is_empty() && build == long => {}
        _ => problems.push(
            "build and long audit run_id do not reconcile with the release receipt".to_string(),
        ),
    }
    if let (Some(path), Some(expected)) = (
        receipt.artifacts.flatpak_path.as_deref(),
        receipt.artifacts.flatpak_sha256.as_deref(),
    ) {
        match hash_file(path) {
            Ok(sha) if sha == expected => {}
            Ok(sha) => problems.push(format!(
                "flatpak SHA-256 mismatch at {path}: disk ({sha}) != release receipt ({expected})"
            )),
            Err(e) => problems.push(format!("cannot hash flatpak at {path}: {e}")),
        }
    }

    if problems.is_empty() {
        Ok(actual)
    } else {
        Err(problems)
    }
}

/// Flatpak bundle path declared by a release receipt; `None` when no bundle was packaged.
pub fn declared_flatpak_path(release_path: &Path) -> Result<Option<String>, String> {
    let content = fs::read_to_string(release_path).map_err(|e| e.to_string())?;
    let receipt: ReleaseReceipt = serde_json::from_str(&content).map_err(|e| e.to_string())?;
    Ok(receipt.artifacts.flatpak_path)
}

/// Evaluates release certification against canonical artifact, build receipt, and long audit receipt.
pub fn evaluate_certification(
    artifact_path: &Path,
    build_receipt_path: &Path,
    long_receipt_path: &Path,
    strict_requested: bool,
    expected_run_id: Option<&str>,
) -> ReleaseReceipt {
    let mut fails = Vec::new();
    let mut gaps = Vec::new();
    let mut reasons = Vec::new();

    // 1. Verify artifact on disk
    let (actual_sha, art_exists) = if artifact_path.exists() {
        match hash_file(artifact_path) {
            Ok(h) => (h, true),
            Err(e) => {
                fails.push("artifact_hash_failed".to_string());
                reasons.push(format!("failed to compute artifact SHA-256: {e}"));
                ("unknown".to_string(), true)
            }
        }
    } else {
        fails.push("artifact_missing".to_string());
        reasons.push(format!("artifact not found: {}", artifact_path.display()));
        ("absent".to_string(), false)
    };

    // 2. Read and validate build receipt
    let build_receipt_opt: Option<BuildReceipt> = if build_receipt_path.exists() {
        match fs::read_to_string(build_receipt_path) {
            Ok(content) => {
                match serde_json::from_str::<BuildReceipt>(&content) {
                    Ok(br) => {
                        // Check artifact SHA against build receipt
                        if art_exists && br.artifacts.artifact_sha256 != actual_sha {
                            fails.push("artifact_sha_mismatch_build".to_string());
                            reasons.push(format!(
                            "artifact SHA-256 mismatch: actual disk ({actual_sha}) != build receipt ({})",
                            br.artifacts.artifact_sha256
                        ));
                        }
                        // Check strict mode
                        if !br.strict_mode {
                            fails.push("build_not_strict".to_string());
                            reasons.push(
                                "build receipt strict_mode is false; release requires strict build"
                                    .to_string(),
                            );
                        }
                        let is_proven_pgo_only = br.optimizations.pgo_applied
                            && !br.optimizations.bolt_applied
                            && br.optimizations.bolt_proof == "fallback_pgo_only";
                        let is_proven_plain_only = !br.optimizations.pgo_applied
                            && !br.optimizations.bolt_applied
                            && br.optimizations.bolt_proof == "fallback_plain";

                        let valid_status = br.status == "PASSED"
                            || ((is_proven_pgo_only || is_proven_plain_only)
                                && br.status == "PASS_WITH_GAPS");

                        // Check status
                        if !valid_status {
                            fails.push("build_status_not_passed".to_string());
                            reasons.push(format!(
                                "build receipt status is '{}', expected 'PASSED'",
                                br.status
                            ));
                        }
                        if br.provenance.git_dirty {
                            fails.push("build_worktree_dirty".to_string());
                            reasons.push(
                                "build receipt provenance records git_dirty=true; a certified release must come from a clean worktree"
                                    .to_string(),
                            );
                        }
                        // Check gaps
                        for g in &br.gaps {
                            if g == "bolt_not_proven"
                                && (is_proven_pgo_only || is_proven_plain_only)
                            {
                                continue;
                            }
                            if (g == "pgo_not_applied" || g == "bolt_not_applied")
                                && is_proven_plain_only
                            {
                                continue;
                            }
                            gaps.push(format!("build:{g}"));
                            reasons.push(format!("build receipt declares gap: '{g}'"));
                        }
                        if br.optimizations.bolt_applied
                            && br.optimizations.bolt_proof == "fallback_pgo_only"
                        {
                            fails.push("build_verdict_mismatch".to_string());
                            reasons.push(
                                "build receipt has bolt_applied=true but bolt_proof=fallback_pgo_only"
                                    .to_string(),
                            );
                        }
                        if !br.optimizations.pgo_applied && !is_proven_plain_only {
                            fails.push("unoptimized_verdict".to_string());
                            reasons.push(
                                "build receipt optimizations reflect unoptimized verdict (pgo_applied=false)"
                                    .to_string(),
                            );
                        }
                        // Check run_id
                        if let Some(exp) = expected_run_id
                            && !exp.is_empty()
                            && let Some(br_run) = &br.provenance.run_id
                            && br_run != exp
                        {
                            fails.push("run_id_mismatch_build".to_string());
                            reasons.push(format!(
                                "build receipt run_id ({br_run}) != expected ({exp})"
                            ));
                        }
                        Some(br)
                    }
                    Err(e) => {
                        fails.push("build_receipt_invalid_schema".to_string());
                        reasons.push(format!("build receipt schema validation failed (serde deny_unknown_fields): {e}"));
                        None
                    }
                }
            }
            Err(e) => {
                fails.push("build_receipt_read_failed".to_string());
                reasons.push(format!("failed to read build receipt: {e}"));
                None
            }
        }
    } else {
        fails.push("build_receipt_missing".to_string());
        reasons.push(format!(
            "build receipt not found: {}",
            build_receipt_path.display()
        ));
        None
    };

    // 3. Read and validate long audit receipt
    let mut long_phases_summary = Vec::new();
    let mut long_audit_status = "UNKNOWN".to_string();
    let mut long_tests_total = 0u64;
    let mut long_dur_total = 0u64;
    let mut long_audit_run_id = None;
    let mut consistency_token = "absent".to_string();

    if long_receipt_path.exists() {
        let consistency = verify_receipt_artifact(long_receipt_path, artifact_path);
        consistency_token = consistency.as_str().to_string();
        match &consistency {
            ArtifactConsistency::Match { declared } => {
                if art_exists && declared != &actual_sha {
                    fails.push("artifact_sha_mismatch_long".to_string());
                    reasons.push(format!(
                        "long audit declared SHA ({declared}) != actual SHA ({actual_sha})"
                    ));
                }
            }
            ArtifactConsistency::Mismatch {
                declared,
                actual,
                reason,
            } => {
                fails.push("artifact_sha_mismatch_long".to_string());
                reasons.push(format!(
                    "long audit artifact mismatch (declared={declared:?}, actual={actual:?}): {reason}"
                ));
            }
            ArtifactConsistency::Absent { reason } => {
                fails.push("long_artifact_absent".to_string());
                reasons.push(format!("long audit artifact provenance absent: {reason}"));
            }
            ArtifactConsistency::Incomplete { reason, .. } => {
                fails.push("long_artifact_incomplete".to_string());
                reasons.push(format!(
                    "long audit artifact provenance incomplete: {reason}"
                ));
            }
        }

        match fs::read_to_string(long_receipt_path) {
            Ok(content) => {
                let mut seen_phases = std::collections::HashSet::new();
                for (idx, line) in content.lines().enumerate() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<serde_json::Value>(line) {
                        Ok(val) => {
                            let pid = val
                                .get("phase_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let name = val
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let status = val
                                .get("status")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let duration_ms =
                                val.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(0);
                            let tests_executed = val
                                .get("tests_executed")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            let mut phase_gaps = Vec::new();
                            if let Some(arr) = val.get("gaps").and_then(|v| v.as_array()) {
                                for g in arr {
                                    if let Some(s) = g.as_str()
                                        && !s.is_empty()
                                    {
                                        phase_gaps.push(s.to_string());
                                        gaps.push(format!("{pid}:{s}"));
                                        reasons.push(format!(
                                            "long audit phase '{pid}' declares gap: '{s}'"
                                        ));
                                    }
                                }
                            }
                            let art_sha = val
                                .get(ARTIFACT_SHA256_KEY)
                                .and_then(|v| v.as_str())
                                .map(ToString::to_string);
                            let art_exercised =
                                val.get(ARTIFACT_EXERCISED_KEY).and_then(|v| v.as_bool());

                            if let Some(rid) = val.get("run_id").and_then(|v| v.as_str())
                                && !rid.is_empty()
                            {
                                long_audit_run_id = Some(rid.to_string());
                                if let Some(exp) = expected_run_id
                                    && !exp.is_empty()
                                    && rid != exp
                                {
                                    fails.push("run_id_mismatch_long".to_string());
                                    reasons.push(format!(
                                        "long audit line {} run_id ({rid}) != expected ({exp})",
                                        idx + 1
                                    ));
                                }
                            }

                            if !pid.is_empty() {
                                seen_phases.insert(pid.clone());
                            }

                            if pid == "overall" {
                                long_audit_status = status.clone();
                                long_tests_total = tests_executed;
                                long_dur_total = duration_ms;
                            }

                            if status != "PASSED" {
                                fails.push(format!("long_phase_{pid}_status_{status}"));
                                reasons.push(format!("long audit phase '{pid}' status is '{status}', expected 'PASSED'"));
                            }

                            long_phases_summary.push(ReleaseLongPhaseSummary {
                                phase_id: pid,
                                name,
                                status,
                                duration_ms,
                                tests_executed,
                                gaps: phase_gaps,
                                artifact_sha256: art_sha,
                                artifact_exercised: art_exercised,
                            });
                        }
                        Err(e) => {
                            fails.push(format!("long_receipt_line_{}_invalid", idx + 1));
                            reasons.push(format!("long audit line {} invalid JSON: {e}", idx + 1));
                        }
                    }
                }

                // Verify that all canonical phases are present
                for req in &LONG_ROSTER {
                    if !seen_phases.contains(*req) {
                        fails.push(format!("long_phase_{req}_missing"));
                        reasons.push(format!(
                            "canonical phase '{req}' is missing from long audit receipt"
                        ));
                    }
                }
            }
            Err(e) => {
                fails.push("long_receipt_read_failed".to_string());
                reasons.push(format!("failed to read long audit receipt: {e}"));
            }
        }
    } else {
        fails.push("long_receipt_missing".to_string());
        reasons.push(format!(
            "long audit receipt not found: {}",
            long_receipt_path.display()
        ));
    }

    // Check cross-receipt run_id consistency
    if let (Some(br), Some(lr_rid)) = (&build_receipt_opt, &long_audit_run_id)
        && let Some(br_rid) = &br.provenance.run_id
        && !br_rid.is_empty()
        && !lr_rid.is_empty()
        && br_rid != lr_rid
    {
        fails.push("run_id_mismatch_cross_receipts".to_string());
        reasons.push(format!(
            "run_id mismatch between build receipt ({br_rid}) and long receipt ({lr_rid})"
        ));
    }

    fails.sort();
    fails.dedup();
    gaps.sort();
    gaps.dedup();
    reasons.sort();
    reasons.dedup();

    let is_certified = fails.is_empty() && gaps.is_empty();
    let status = if is_certified {
        "CERTIFIED".to_string()
    } else {
        "REJECTED".to_string()
    };

    let (package, provenance, engine_provenance, optimizations, pgo_workload, artifacts) =
        if let Some(br) = build_receipt_opt {
            let arts = ReleaseArtifacts {
                artifact_path: br.artifacts.artifact_path,
                artifact_sha256: br.artifacts.artifact_sha256,
                artifact_consistency: consistency_token,
                tarball_path: br.artifacts.tarball_path,
                tarball_sha256: br.artifacts.tarball_sha256,
                flatpak_path: br.artifacts.flatpak_path,
                flatpak_sha256: br.artifacts.flatpak_sha256,
                installed_path: br.artifacts.installed_path,
                installed_sha256: br.artifacts.installed_sha256,
            };
            (
                br.package,
                br.provenance,
                br.engine_provenance,
                br.optimizations,
                br.pgo_workload,
                arts,
            )
        } else {
            let arts = ReleaseArtifacts {
                artifact_path: artifact_path.display().to_string(),
                artifact_sha256: actual_sha,
                artifact_consistency: consistency_token,
                tarball_path: None,
                tarball_sha256: None,
                flatpak_path: None,
                flatpak_sha256: None,
                installed_path: None,
                installed_sha256: None,
            };
            (
                PackageInfo {
                    name: "nam-plug".to_string(),
                    version: "unknown".to_string(),
                },
                BuildProvenance {
                    git_commit: "unknown".to_string(),
                    git_branch: "unknown".to_string(),
                    git_dirty: true,
                    cargo_lock_sha256: "unknown".to_string(),
                    rustc_version: "unknown".to_string(),
                    rustflags: String::new(),
                    run_id: None,
                },
                EngineProvenance {
                    dependency_type: "unknown".to_string(),
                    version: "unknown".to_string(),
                    checksum: None,
                },
                BuildOptimizations {
                    pgo_applied: false,
                    bolt_applied: false,
                    bolt_proof: "none".to_string(),
                    bolt_profile_sha256: None,
                    bolt_version: None,
                    bolt_lbr_samples: None,
                    bolt_lbr_fdata_sha256: None,
                    codegen_comparison: "none".to_string(),
                    pre_bolt_symbols: None,
                    post_bolt_symbols: None,
                },
                None,
                arts,
            )
        };

    let long_audit = ReleaseLongAudit {
        receipt_path: long_receipt_path.display().to_string(),
        status: long_audit_status,
        tests_executed: long_tests_total,
        duration_ms: long_dur_total,
        run_id: long_audit_run_id,
        phases: long_phases_summary,
    };

    let verdict = if optimizations.bolt_applied {
        Some("PGO+BOLT".to_string())
    } else if optimizations.pgo_applied && optimizations.bolt_proof == "fallback_pgo_only" {
        Some("PGO-ONLY".to_string())
    } else if !optimizations.pgo_applied
        && !optimizations.bolt_applied
        && optimizations.bolt_proof == "fallback_plain"
    {
        Some("PLAIN-ONLY".to_string())
    } else {
        None
    };

    ReleaseReceipt {
        schema_version: "1.0.0".to_string(),
        timestamp_utc: now_iso8601(),
        status,
        strict_mode: strict_requested,
        fails,
        gaps,
        reasons,
        package,
        provenance,
        engine_provenance,
        optimizations,
        pgo_workload,
        artifacts,
        long_audit,
        verdict,
    }
}

#[cfg(test)]
#[path = "release_test.rs"]
mod tests;
