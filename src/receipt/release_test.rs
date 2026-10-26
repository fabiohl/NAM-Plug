// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use std::fs;
use std::path::Path;

use super::*;
use crate::receipt::build::{
    BuildArtifacts, BuildOptimizations, BuildProvenance, BuildReceipt, EngineProvenance,
    PackageInfo, hash_file,
};

#[test]
fn test_serde_deny_unknown_fields() {
    let receipt = ReleaseReceipt {
        schema_version: "1.0.0".to_string(),
        timestamp_utc: "2026-10-26T20:00:00Z".to_string(),
        status: "CERTIFIED".to_string(),
        strict_mode: true,
        fails: Vec::new(),
        gaps: Vec::new(),
        reasons: Vec::new(),
        package: PackageInfo {
            name: "nam-plug".to_string(),
            version: "1.0.0".to_string(),
        },
        provenance: BuildProvenance {
            git_commit: "0123456789abcdef".to_string(),
            git_branch: "dev".to_string(),
            git_dirty: false,
            cargo_lock_sha256: "abc".to_string(),
            rustc_version: "rustc 1.99".to_string(),
            rustflags: "-Ctarget-cpu=x86-64-v3".to_string(),
            run_id: Some("run-test-1".to_string()),
        },
        engine_provenance: EngineProvenance {
            dependency_type: "crates.io".to_string(),
            version: "1.0.1".to_string(),
            checksum: None,
        },
        optimizations: BuildOptimizations {
            pgo_applied: true,
            bolt_applied: true,
            bolt_proof: "test".to_string(),
            bolt_profile_sha256: None,
            bolt_version: None,
            bolt_lbr_samples: None,
            bolt_lbr_fdata_sha256: None,
            codegen_comparison: "match".to_string(),
            pre_bolt_symbols: None,
            post_bolt_symbols: None,
        },
        pgo_workload: None,
        artifacts: ReleaseArtifacts {
            artifact_path: "/tmp/nam_plug.clap".to_string(),
            artifact_sha256: "0123".to_string(),
            artifact_consistency: "match".to_string(),
            tarball_path: None,
            tarball_sha256: None,
            flatpak_path: None,
            flatpak_sha256: None,
            installed_path: None,
            installed_sha256: None,
        },
        long_audit: ReleaseLongAudit {
            receipt_path: "/tmp/long.jsonl".to_string(),
            status: "PASSED".to_string(),
            tests_executed: 100,
            duration_ms: 5000,
            run_id: Some("run-test-1".to_string()),
            phases: Vec::new(),
        },
    };

    let serialized = serde_json::to_string(&receipt).expect("serialize release receipt");
    let deserialized: ReleaseReceipt =
        serde_json::from_str(&serialized).expect("deserialize release receipt");
    assert_eq!(receipt, deserialized);

    // Injected unknown field must fail under deny_unknown_fields
    let mut val: serde_json::Value = serde_json::from_str(&serialized).unwrap();
    val["unknown_extra_field"] = serde_json::Value::Bool(true);
    let tampered = serde_json::to_string(&val).unwrap();
    assert!(serde_json::from_str::<ReleaseReceipt>(&tampered).is_err());
}

fn create_mock_environment(
    tmp_dir: &Path,
) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let art_path = tmp_dir.join("nam_plug.clap");
    fs::write(&art_path, b"dummy clap binary content").unwrap();
    let actual_sha = hash_file(&art_path).unwrap();

    let br_path = tmp_dir.join("build-receipt.json");
    let build_receipt = BuildReceipt {
        schema_version: "1.0.0".to_string(),
        timestamp_utc: "2026-10-26T20:00:00Z".to_string(),
        status: "PASSED".to_string(),
        strict_mode: true,
        gaps: Vec::new(),
        package: PackageInfo {
            name: "nam-plug".to_string(),
            version: "1.0.0".to_string(),
        },
        provenance: BuildProvenance {
            git_commit: "abcdef".to_string(),
            git_branch: "dev".to_string(),
            git_dirty: false,
            cargo_lock_sha256: "cargo-lock-sha".to_string(),
            rustc_version: "rustc 1.99".to_string(),
            rustflags: "-Ctarget-cpu=x86-64-v3".to_string(),
            run_id: Some("test-run-123".to_string()),
        },
        engine_provenance: EngineProvenance {
            dependency_type: "crates.io".to_string(),
            version: "1.0.1".to_string(),
            checksum: None,
        },
        optimizations: BuildOptimizations {
            pgo_applied: true,
            bolt_applied: true,
            bolt_proof: "proof".to_string(),
            bolt_profile_sha256: None,
            bolt_version: None,
            bolt_lbr_samples: None,
            bolt_lbr_fdata_sha256: None,
            codegen_comparison: "match".to_string(),
            pre_bolt_symbols: None,
            post_bolt_symbols: None,
        },
        pgo_workload: None,
        artifacts: BuildArtifacts {
            artifact_path: art_path.display().to_string(),
            artifact_sha256: actual_sha.clone(),
            pre_strip_artifact_sha256: None,
            tarball_path: None,
            tarball_sha256: None,
            flatpak_path: None,
            flatpak_sha256: None,
            installed_path: None,
            installed_sha256: None,
        },
    };
    fs::write(
        &br_path,
        serde_json::to_string_pretty(&build_receipt).unwrap(),
    )
    .unwrap();

    let lr_path = tmp_dir.join("long-audit-receipt.jsonl");
    let phases = vec!["phase1", "phase2", "phase3", "phase4", "phase5", "overall"];
    let mut lines = Vec::new();
    for pid in phases {
        let entry = serde_json::json!({
            "phase_id": pid,
            "name": format!("Name {pid}"),
            "status": "PASSED",
            "duration_ms": 100,
            "tests_executed": 10,
            "gaps": [],
            "timestamp": "2026-10-26T20:00:00Z",
            "artifact_path": art_path.display().to_string(),
            "artifact_sha256": actual_sha,
            "artifact_exercised": true,
            "run_id": "test-run-123",
        });
        lines.push(serde_json::to_string(&entry).unwrap());
    }
    fs::write(&lr_path, lines.join("\n") + "\n").unwrap();

    (art_path, br_path, lr_path)
}

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

struct TestTempDir {
    path: PathBuf,
}

impl TestTempDir {
    fn new(prefix: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "nam_plug_test_{prefix}_{}_{}",
            std::process::id(),
            id
        ));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestTempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn test_evaluate_certification_clean_passes() {
    let tmp = TestTempDir::new("clean");
    let (art_path, br_path, lr_path) = create_mock_environment(tmp.path());

    let receipt = evaluate_certification(&art_path, &br_path, &lr_path, true, Some("test-run-123"));

    assert_eq!(receipt.status, "CERTIFIED");
    assert!(receipt.fails.is_empty(), "fails: {:?}", receipt.fails);
    assert!(receipt.gaps.is_empty(), "gaps: {:?}", receipt.gaps);
    assert!(receipt.reasons.is_empty(), "reasons: {:?}", receipt.reasons);
}

#[test]
fn test_evaluate_certification_rejected_on_tampered_byte() {
    let tmp = TestTempDir::new("tampered");
    let (art_path, br_path, lr_path) = create_mock_environment(tmp.path());

    // Tamper one byte of the artifact after long-audit
    fs::write(&art_path, b"modified clap binary content!").unwrap();

    let receipt = evaluate_certification(&art_path, &br_path, &lr_path, true, Some("test-run-123"));

    assert_eq!(receipt.status, "REJECTED");
    assert!(!receipt.fails.is_empty());
    assert!(
        receipt
            .reasons
            .iter()
            .any(|r| r.contains("SHA-256 mismatch") || r.contains("mismatch"))
    );
}

#[test]
fn test_evaluate_certification_rejected_on_gap() {
    let tmp = TestTempDir::new("gap");
    let (art_path, br_path, lr_path) = create_mock_environment(tmp.path());

    // Inject a gap in build receipt
    let mut br: BuildReceipt =
        serde_json::from_str(&fs::read_to_string(&br_path).unwrap()).unwrap();
    br.gaps.push("bolt_not_applied".to_string());
    fs::write(&br_path, serde_json::to_string_pretty(&br).unwrap()).unwrap();

    let receipt = evaluate_certification(&art_path, &br_path, &lr_path, true, Some("test-run-123"));

    assert_eq!(receipt.status, "REJECTED");
    assert!(!receipt.gaps.is_empty());
    assert!(receipt.reasons.iter().any(|r| r.contains("gaps")));
}

#[test]
fn test_evaluate_certification_rejected_on_run_id_mismatch() {
    let tmp = TestTempDir::new("run_id");
    let (art_path, br_path, lr_path) = create_mock_environment(tmp.path());

    let receipt = evaluate_certification(
        &art_path,
        &br_path,
        &lr_path,
        true,
        Some("different-session-999"),
    );

    assert_eq!(receipt.status, "REJECTED");
    assert!(
        receipt
            .reasons
            .iter()
            .any(|r| r.contains("different-session-999"))
    );
}
