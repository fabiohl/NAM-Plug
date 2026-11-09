// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::PathBuf;

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "nam_plug_rel_test_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::create_dir_all(&path);
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn create_mock_release_env(
    tag: &str,
    pgo_applied: bool,
    bolt_applied: bool,
    bolt_proof: &str,
    build_status: &str,
    git_dirty: bool,
    gaps: Vec<String>,
) -> (TempDir, PathBuf, PathBuf, PathBuf) {
    let dir = TempDir::new(tag);

    // 1. Artifact
    let artifact_path = dir.path.join("libnam_plug.so");
    let artifact_bytes = b"MOCK_ARTIFACT_CLAP_SO";
    fs::write(&artifact_path, artifact_bytes).expect("write artifact");

    let mut hasher = Sha256::new();
    hasher.update(artifact_bytes);
    let artifact_sha256 = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();

    // 2. Build receipt
    let build_receipt_path = dir.path.join("build-receipt.json");
    let build_receipt = json!({
        "schema_version": "1.0.0",
        "timestamp_utc": "2026-11-08T00:00:00Z",
        "status": build_status,
        "strict_mode": true,
        "gaps": gaps,
        "package": {
            "name": "nam-plug",
            "version": "1.1.0"
        },
        "provenance": {
            "git_commit": "0123456789abcdef0123456789abcdef01234567",
            "git_branch": "dev",
            "git_dirty": git_dirty,
            "cargo_lock_sha256": "4e724b172a15f019688461bfa2a74c43f11e9f2df486be77fb238385d03e5c77",
            "rustc_version": "rustc 1.99.0",
            "rustflags": "-C target-cpu=x86-64-v3",
            "run_id": "test-run-123"
        },
        "engine_provenance": {
            "dependency_type": "workspace_or_path",
            "version": "2.0.0",
            "checksum": null
        },
        "optimizations": {
            "pgo_applied": pgo_applied,
            "bolt_applied": bolt_applied,
            "bolt_proof": bolt_proof,
            "bolt_profile_sha256": if bolt_applied { Some("bolt_profile_sha".to_string()) } else { None },
            "bolt_version": if bolt_applied { Some("llvm-bolt 19".to_string()) } else { None },
            "bolt_lbr_samples": null,
            "bolt_lbr_fdata_sha256": null,
            "codegen_comparison": "match",
            "pre_bolt_symbols": null,
            "post_bolt_symbols": null
        },
        "artifacts": {
            "artifact_path": artifact_path.display().to_string(),
            "artifact_sha256": artifact_sha256,
            "tarball_path": null,
            "tarball_sha256": null,
            "flatpak_path": null,
            "flatpak_sha256": null,
            "installed_path": null,
            "installed_sha256": null
        }
    });
    fs::write(
        &build_receipt_path,
        serde_json::to_string_pretty(&build_receipt).unwrap(),
    )
    .expect("write build receipt");

    // 3. Long JSONL receipt
    let long_receipt_path = dir.path.join("long-audit-receipt.jsonl");
    let mut jsonl_file = fs::File::create(&long_receipt_path).expect("create jsonl");
    for phase in LONG_ROSTER {
        let entry = json!({
            "phase_id": phase,
            "name": format!("phase_{phase}"),
            "status": "PASSED",
            "artifact_sha256": artifact_sha256,
            "artifact_exercised": true,
            "run_id": "test-run-123",
            "strict_mode": true,
            "tests_executed": 10,
            "duration_ms": 100,
            "gaps": [],
            "timestamp": "2026-11-08T00:00:00Z"
        });
        writeln!(jsonl_file, "{entry}").expect("write jsonl line");
    }

    (dir, artifact_path, build_receipt_path, long_receipt_path)
}

#[test]
fn test_certify_pgo_bolt_clean_head_certified() {
    let (_dir, art, bld, lng) = create_mock_release_env(
        "pgo_bolt",
        true,
        true,
        "ab-opt-ceremony:proof-abc-123",
        "PASSED",
        false,
        vec![],
    );

    let receipt = evaluate_certification(&art, &bld, &lng, true, Some("test-run-123"));
    assert_eq!(receipt.status, "CERTIFIED");
    assert_eq!(receipt.verdict, Some("PGO+BOLT".to_string()));
    assert!(receipt.fails.is_empty(), "fails: {:?}", receipt.fails);
}

#[test]
fn test_certify_pgo_only_fallback_certified() {
    let (_dir, art, bld, lng) = create_mock_release_env(
        "pgo_only",
        true,
        false,
        "fallback_pgo_only",
        "PASS_WITH_GAPS",
        false,
        vec!["bolt_not_proven".to_string()],
    );

    let receipt = evaluate_certification(&art, &bld, &lng, true, Some("test-run-123"));
    assert_eq!(receipt.status, "CERTIFIED");
    assert_eq!(receipt.verdict, Some("PGO-ONLY".to_string()));
    assert!(receipt.fails.is_empty(), "fails: {:?}", receipt.fails);
}

#[test]
fn test_certify_plain_only_fallback_certified() {
    let (_dir, art, bld, lng) = create_mock_release_env(
        "plain_only",
        false,
        false,
        "fallback_plain",
        "PASS_WITH_GAPS",
        false,
        vec![
            "bolt_not_proven".to_string(),
            "pgo_not_applied".to_string(),
            "bolt_not_applied".to_string(),
        ],
    );

    let receipt = evaluate_certification(&art, &bld, &lng, true, Some("test-run-123"));
    assert_eq!(receipt.status, "CERTIFIED");
    assert_eq!(receipt.verdict, Some("PLAIN-ONLY".to_string()));
    assert!(receipt.fails.is_empty(), "fails: {:?}", receipt.fails);
}

#[test]
fn test_certify_unoptimized_rejected() {
    let (_dir, art, bld, lng) =
        create_mock_release_env("unopt", false, false, "none", "PASSED", false, vec![]);

    let receipt = evaluate_certification(&art, &bld, &lng, true, Some("test-run-123"));
    assert_eq!(receipt.status, "REJECTED");
    assert!(
        receipt.fails.contains(&"unoptimized_verdict".to_string()),
        "fails: {:?}",
        receipt.fails
    );
}

#[test]
fn test_certify_dirty_worktree_rejected() {
    let (_dir, art, bld, lng) = create_mock_release_env(
        "dirty",
        true,
        true,
        "ab-opt-ceremony:proof-abc-123",
        "PASSED",
        true, // dirty worktree
        vec![],
    );

    let receipt = evaluate_certification(&art, &bld, &lng, true, Some("test-run-123"));
    assert_eq!(receipt.status, "REJECTED");
    assert!(
        receipt.fails.contains(&"build_worktree_dirty".to_string()),
        "fails: {:?}",
        receipt.fails
    );
}

#[test]
fn test_certify_verdict_mismatch_rejected() {
    let (_dir, art, bld, lng) = create_mock_release_env(
        "mismatch",
        true,
        true,
        "fallback_pgo_only", // mismatch: bolt_applied is true but proof is fallback_pgo_only
        "PASSED",
        false,
        vec![],
    );

    let receipt = evaluate_certification(&art, &bld, &lng, true, Some("test-run-123"));
    assert_eq!(receipt.status, "REJECTED");
    assert!(
        receipt
            .fails
            .contains(&"build_verdict_mismatch".to_string()),
        "fails: {:?}",
        receipt.fails
    );
}
