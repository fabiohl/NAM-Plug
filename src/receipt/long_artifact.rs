// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Artifact provenance for the long-audit receipt.
//!
//! `utils/tests-long.sh` runs every phase against the **same** CLAP artifact and
//! stamps its path and SHA-256 into each `long-audit-receipt.jsonl` line. This
//! module is the single semantic verifier for that invariant: it collects the
//! declared hashes, detects missing/incomplete/conflicting declarations, and
//! compares them with the artifact actually installed.
//!
//! It is used by `plug_long_receipt verify-artifact` (the release-time gate);
//! the shell never re-implements the comparison.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use serde_json::Value;
use sha2::{Digest, Sha256};

/// JSONL key carrying the tested artifact path.
pub const ARTIFACT_PATH_KEY: &str = "artifact_path";

/// JSONL key carrying the tested artifact SHA-256.
pub const ARTIFACT_SHA256_KEY: &str = "artifact_sha256";

/// JSONL key declaring whether the phase actually loaded the artifact (`dlopen`).
///
/// A phase may *declare* the artifact hash while running only `--lib` tests that
/// never load the shared object; `artifact_exercised=false` makes that explicit.
pub const ARTIFACT_EXERCISED_KEY: &str = "artifact_exercised";

/// Artifact declared by a long-audit receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredArtifact {
    /// Artifact path, when every line agrees on one.
    pub path: Option<String>,
    /// Artifact SHA-256, when every line agrees on one.
    pub sha256: String,
}

/// Why a receipt does not declare a single artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclaredArtifactError {
    /// No entry carried `artifact_sha256`.
    Missing,
    /// Some entries carried `artifact_sha256` and others did not.
    Incomplete,
    /// Entries declared different artifact hashes (or paths).
    Conflicting,
}

impl std::fmt::Display for DeclaredArtifactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Missing => "no artifact_sha256 declared by the receipt",
            Self::Incomplete => "only some receipt lines declare artifact_sha256",
            Self::Conflicting => "receipt lines declare conflicting artifacts",
        })
    }
}

/// Verdict of comparing a receipt's declared artifact with an installed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactConsistency {
    /// The receipt declares exactly the installed artifact.
    Match {
        /// Hash declared by the receipt (and verified against the file).
        declared: String,
    },
    /// The receipt exists but does not describe the installed artifact.
    Mismatch {
        /// Hash declared by the receipt, when it could be read.
        declared: Option<String>,
        /// Hash of the artifact handed to the verifier, when it could be read.
        actual: Option<String>,
        /// Human-readable reason.
        reason: String,
    },
    /// No usable artifact declaration exists (missing receipt/fields).
    Absent {
        /// Human-readable reason.
        reason: String,
    },
    /// The receipt exists but only some lines declare an artifact.
    Incomplete {
        /// Hash declared by the lines that carry one, when available.
        declared: Option<String>,
        /// Human-readable reason.
        reason: String,
    },
}

impl ArtifactConsistency {
    /// Stable lowercase token (`match`, `mismatch`, `absent`, `incomplete`) for
    /// receipts/logs.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Match { .. } => "match",
            Self::Mismatch { .. } => "mismatch",
            Self::Absent { .. } => "absent",
            Self::Incomplete { .. } => "incomplete",
        }
    }

    /// Declared hash, when available.
    #[must_use]
    pub fn declared_sha256(&self) -> Option<&str> {
        match self {
            Self::Match { declared } => Some(declared.as_str()),
            Self::Mismatch { declared, .. } | Self::Incomplete { declared, .. } => {
                declared.as_deref()
            }
            Self::Absent { .. } => None,
        }
    }

    /// Human-readable reason for non-match verdicts.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Match { .. } => None,
            Self::Mismatch { reason, .. }
            | Self::Absent { reason }
            | Self::Incomplete { reason, .. } => Some(reason),
        }
    }
}

/// Computes the SHA-256 hex digest of a file, streaming it in chunks.
pub fn sha256_file(path: &Path) -> Result<String, String> {
    let file =
        File::open(path).map_err(|e| format!("cannot open artifact {}: {e}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buf)
            .map_err(|e| format!("cannot read artifact {}: {e}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    use std::fmt::Write as _;
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

/// Reads all non-empty JSONL entries from a receipt.
pub fn read_receipt_entries(path: &Path) -> Result<Vec<Value>, String> {
    let file =
        File::open(path).map_err(|e| format!("cannot open receipt {}: {e}", path.display()))?;
    let mut entries = Vec::new();
    for (line_no, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|e| format!("cannot read receipt {}: {e}", path.display()))?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line).map_err(|e| {
            format!(
                "receipt {} line {} is not valid JSON: {e}",
                path.display(),
                line_no + 1
            )
        })?;
        entries.push(value);
    }
    if entries.is_empty() {
        return Err(format!("receipt {} is empty", path.display()));
    }
    Ok(entries)
}

/// Collects the artifact declared across all receipt entries.
///
/// The invariant is all-or-nothing: every line must declare the same artifact.
/// A legacy receipt with no artifact fields is [`DeclaredArtifactError::Missing`]
/// (reported as `absent` by the verifier), never a silent pass.
pub fn declared_artifact(entries: &[Value]) -> Result<DeclaredArtifact, DeclaredArtifactError> {
    let mut shas = BTreeSet::new();
    let mut paths = BTreeSet::new();
    let mut declared_count = 0usize;

    for entry in entries {
        if let Some(sha) = entry
            .get(ARTIFACT_SHA256_KEY)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            declared_count += 1;
            shas.insert(sha.to_string());
        }
        if let Some(path) = entry
            .get(ARTIFACT_PATH_KEY)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            paths.insert(path.to_string());
        }
    }

    if declared_count == 0 {
        return Err(DeclaredArtifactError::Missing);
    }
    if declared_count != entries.len() {
        return Err(DeclaredArtifactError::Incomplete);
    }
    if shas.len() != 1 {
        return Err(DeclaredArtifactError::Conflicting);
    }
    if paths.len() > 1 {
        return Err(DeclaredArtifactError::Conflicting);
    }

    let sha256 = shas
        .into_iter()
        .next()
        .ok_or(DeclaredArtifactError::Missing)?;
    Ok(DeclaredArtifact {
        path: paths.into_iter().next(),
        sha256,
    })
}

/// Verifies that `receipt_path` declares exactly `expected_sha256`.
///
/// This is the release-receipt reconciliation entry point: it compares the
/// receipt's declared artifact with the hash the build pipeline recorded for the
/// installed artifact, without re-hashing the file. Missing receipt/provenance
/// is `absent`, a partially stamped receipt is `incomplete`, and any other
/// divergence is `mismatch` (fail-closed).
pub fn verify_receipt_declared_sha(
    receipt_path: &Path,
    expected_sha256: &str,
) -> ArtifactConsistency {
    if !receipt_path.is_file() {
        return ArtifactConsistency::Absent {
            reason: format!("receipt {} does not exist", receipt_path.display()),
        };
    }
    let entries = match read_receipt_entries(receipt_path) {
        Ok(entries) => entries,
        Err(e) => {
            return ArtifactConsistency::Mismatch {
                declared: None,
                actual: Some(expected_sha256.to_string()),
                reason: e,
            };
        }
    };
    let declared = match declared_artifact(&entries) {
        Ok(declared) => declared,
        Err(DeclaredArtifactError::Missing) => {
            return ArtifactConsistency::Absent {
                reason: DeclaredArtifactError::Missing.to_string(),
            };
        }
        Err(DeclaredArtifactError::Incomplete) => {
            let mut declared_shas = BTreeSet::new();
            for entry in &entries {
                if let Some(sha) = entry
                    .get(ARTIFACT_SHA256_KEY)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    declared_shas.insert(sha.to_string());
                }
            }
            return ArtifactConsistency::Incomplete {
                declared: declared_shas.into_iter().next(),
                reason: DeclaredArtifactError::Incomplete.to_string(),
            };
        }
        Err(kind) => {
            return ArtifactConsistency::Mismatch {
                declared: None,
                actual: Some(expected_sha256.to_string()),
                reason: kind.to_string(),
            };
        }
    };
    if declared.sha256 == expected_sha256 {
        ArtifactConsistency::Match {
            declared: declared.sha256,
        }
    } else {
        ArtifactConsistency::Mismatch {
            declared: Some(declared.sha256.clone()),
            actual: Some(expected_sha256.to_string()),
            reason: format!(
                "receipt declares {} but the release receipt recorded {}",
                declared.sha256, expected_sha256
            ),
        }
    }
}

/// Verifies that `receipt_path` describes `artifact_path`.
///
/// A missing receipt or a receipt without provenance is `absent`; a receipt
/// whose lines only partially declare an artifact is `incomplete` (distinct
/// from `absent`); a malformed receipt, a conflicting declaration or a hash
/// difference is `mismatch` (fail-closed).
pub fn verify_receipt_artifact(receipt_path: &Path, artifact_path: &Path) -> ArtifactConsistency {
    if !receipt_path.is_file() {
        return ArtifactConsistency::Absent {
            reason: format!("receipt {} does not exist", receipt_path.display()),
        };
    }
    let entries = match read_receipt_entries(receipt_path) {
        Ok(entries) => entries,
        Err(e) => {
            return ArtifactConsistency::Mismatch {
                declared: None,
                actual: None,
                reason: e,
            };
        }
    };
    let declared = match declared_artifact(&entries) {
        Ok(declared) => declared,
        Err(DeclaredArtifactError::Missing) => {
            return ArtifactConsistency::Absent {
                reason: DeclaredArtifactError::Missing.to_string(),
            };
        }
        Err(DeclaredArtifactError::Incomplete) => {
            let mut declared_shas = BTreeSet::new();
            for entry in &entries {
                if let Some(sha) = entry
                    .get(ARTIFACT_SHA256_KEY)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    declared_shas.insert(sha.to_string());
                }
            }
            return ArtifactConsistency::Incomplete {
                declared: declared_shas.into_iter().next(),
                reason: DeclaredArtifactError::Incomplete.to_string(),
            };
        }
        Err(kind) => {
            return ArtifactConsistency::Mismatch {
                declared: None,
                actual: None,
                reason: kind.to_string(),
            };
        }
    };
    let actual = match sha256_file(artifact_path) {
        Ok(sha) => sha,
        Err(e) => {
            return ArtifactConsistency::Mismatch {
                declared: Some(declared.sha256.clone()),
                actual: None,
                reason: e,
            };
        }
    };
    if declared.sha256 == actual {
        ArtifactConsistency::Match {
            declared: declared.sha256,
        }
    } else {
        ArtifactConsistency::Mismatch {
            declared: Some(declared.sha256.clone()),
            actual: Some(actual.clone()),
            reason: format!(
                "receipt declares {} but {} is {}",
                declared.sha256,
                artifact_path.display(),
                actual
            ),
        }
    }
}
