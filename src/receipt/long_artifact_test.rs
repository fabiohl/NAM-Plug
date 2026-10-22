// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Tests for [`super`] (long-audit artifact provenance).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

use super::{
    ARTIFACT_PATH_KEY, ARTIFACT_SHA256_KEY, ArtifactConsistency, DeclaredArtifactError,
    declared_artifact, sha256_file, verify_receipt_artifact, verify_receipt_declared_sha,
};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nam-plug-long-artifact-{}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn entry(phase: &str, sha: Option<&str>, path: Option<&str>) -> Value {
    let mut value = json!({ "phase_id": phase });
    if let Some(sha) = sha {
        value[ARTIFACT_SHA256_KEY] = json!(sha);
    }
    if let Some(path) = path {
        value[ARTIFACT_PATH_KEY] = json!(path);
    }
    value
}

#[test]
fn declared_artifact_is_unique_when_all_lines_agree() {
    let entries = vec![
        entry("phase1", Some("aaaa"), Some("/tmp/a.clap")),
        entry("phase2", Some("aaaa"), Some("/tmp/a.clap")),
        entry("overall", Some("aaaa"), Some("/tmp/a.clap")),
    ];
    let declared = declared_artifact(&entries).expect("consistent receipt");
    assert_eq!(declared.sha256, "aaaa");
    assert_eq!(declared.path.as_deref(), Some("/tmp/a.clap"));
}

#[test]
fn declared_artifact_detects_missing_incomplete_and_conflicting() {
    assert_eq!(
        declared_artifact(&[entry("phase1", None, None)]).unwrap_err(),
        DeclaredArtifactError::Missing
    );
    assert_eq!(
        declared_artifact(&[
            entry("phase1", Some("aaaa"), None),
            entry("phase2", None, None)
        ])
        .unwrap_err(),
        DeclaredArtifactError::Incomplete
    );
    assert_eq!(
        declared_artifact(&[
            entry("phase1", Some("aaaa"), None),
            entry("phase2", Some("bbbb"), None)
        ])
        .unwrap_err(),
        DeclaredArtifactError::Conflicting
    );
}

#[test]
fn verify_reports_match_mismatch_and_absent() {
    let dir = TempDir::new();
    let artifact = dir.path().join("nam_plug.clap");
    fs::write(&artifact, b"artifact-bytes").expect("write artifact");
    let actual = sha256_file(&artifact).expect("hash artifact");

    let matching = dir.path().join("matching.jsonl");
    fs::write(
        &matching,
        format!(
            "{}\n{}\n",
            json!({ "phase_id": "phase1", ARTIFACT_SHA256_KEY: actual }),
            json!({ "phase_id": "overall", ARTIFACT_SHA256_KEY: actual })
        ),
    )
    .expect("write matching receipt");
    assert!(matches!(
        verify_receipt_artifact(&matching, &artifact),
        ArtifactConsistency::Match { .. }
    ));

    let divergent = dir.path().join("divergent.jsonl");
    fs::write(
        &divergent,
        format!(
            "{}\n",
            json!({ "phase_id": "phase1", ARTIFACT_SHA256_KEY: "deadbeef" })
        ),
    )
    .expect("write divergent receipt");
    assert!(matches!(
        verify_receipt_artifact(&divergent, &artifact),
        ArtifactConsistency::Mismatch { .. }
    ));

    let legacy = dir.path().join("legacy.jsonl");
    fs::write(&legacy, format!("{}\n", json!({ "phase_id": "phase1" })))
        .expect("write legacy receipt");
    assert!(matches!(
        verify_receipt_artifact(&legacy, &artifact),
        ArtifactConsistency::Absent { .. }
    ));

    let partial = dir.path().join("partial.jsonl");
    fs::write(
        &partial,
        format!(
            "{}\n{}\n",
            json!({ "phase_id": "phase1", ARTIFACT_SHA256_KEY: actual }),
            json!({ "phase_id": "phase2" })
        ),
    )
    .expect("write partial receipt");
    let partial_verdict = verify_receipt_artifact(&partial, &artifact);
    assert!(matches!(
        partial_verdict,
        ArtifactConsistency::Incomplete { .. }
    ));
    assert_eq!(partial_verdict.as_str(), "incomplete");
    assert_eq!(partial_verdict.declared_sha256(), Some(actual.as_str()));

    assert!(matches!(
        verify_receipt_artifact(&dir.path().join("missing.jsonl"), &artifact),
        ArtifactConsistency::Absent { .. }
    ));

    assert!(matches!(
        verify_receipt_declared_sha(&matching, &actual),
        ArtifactConsistency::Match { .. }
    ));
    assert!(matches!(
        verify_receipt_declared_sha(&divergent, &actual),
        ArtifactConsistency::Mismatch { .. }
    ));
    assert!(matches!(
        verify_receipt_declared_sha(&partial, &actual),
        ArtifactConsistency::Incomplete { .. }
    ));
    assert!(matches!(
        verify_receipt_declared_sha(&legacy, &actual),
        ArtifactConsistency::Absent { .. }
    ));
    assert!(matches!(
        verify_receipt_declared_sha(&dir.path().join("missing.jsonl"), &actual),
        ArtifactConsistency::Absent { .. }
    ));
}
