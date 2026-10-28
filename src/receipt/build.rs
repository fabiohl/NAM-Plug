// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Typed build provenance receipt schema for NAM-Plug canonical artifacts (`target/dist/build-receipt.json`).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BuildReceipt {
    pub schema_version: String,
    pub timestamp_utc: String,
    pub status: String,
    pub strict_mode: bool,
    #[serde(default)]
    pub gaps: Vec<String>,
    pub package: PackageInfo,
    pub provenance: BuildProvenance,
    pub engine_provenance: EngineProvenance,
    pub optimizations: BuildOptimizations,
    #[serde(default)]
    pub pgo_workload: Option<PgoWorkloadInfo>,
    pub artifacts: BuildArtifacts,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PackageInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BuildProvenance {
    pub git_commit: String,
    pub git_branch: String,
    pub git_dirty: bool,
    pub cargo_lock_sha256: String,
    pub rustc_version: String,
    pub rustflags: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EngineProvenance {
    pub dependency_type: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BuildOptimizations {
    pub pgo_applied: bool,
    pub bolt_applied: bool,
    pub bolt_proof: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bolt_profile_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bolt_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bolt_lbr_samples: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bolt_lbr_fdata_sha256: Option<String>,
    pub codegen_comparison: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_bolt_symbols: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_bolt_symbols: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PgoWorkloadInfo {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excitation_gain: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_scenarios: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_callbacks: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clipping_count_phase1: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clipping_count_bolt: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BuildArtifacts {
    pub artifact_path: String,
    pub artifact_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_strip_artifact_sha256: Option<String>,
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

fn digest_to_hex(digest: impl AsRef<[u8]>) -> String {
    use std::fmt::Write;
    let bytes = digest.as_ref();
    let mut hex = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(&mut hex, "{b:02x}");
    }
    hex
}

/// Compute hex SHA-256 digest of a file.
pub fn hash_file<P: AsRef<Path>>(path: P) -> Result<String, std::io::Error> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    Ok(digest_to_hex(hasher.finalize()))
}

/// Parse ab-opt-ceremony-receipt.json to extract `(proof_id, lbr_samples, lbr_fdata_sha256)`.
pub fn parse_ab_opt_ceremony_receipt<P: AsRef<Path>>(
    path: P,
) -> Option<(String, Option<String>, Option<String>)> {
    let content = fs::read_to_string(path).ok()?;
    let doc: serde_json::Value = serde_json::from_str(&content).ok()?;
    if doc.get("verdict").and_then(|v| v.as_str()) != Some("PGO+BOLT") {
        return None;
    }
    let runs = doc
        .get("runs")
        .and_then(|r| r.as_u64().or_else(|| r.as_str()?.parse().ok()))
        .unwrap_or(0);
    if runs < 3 {
        return None;
    }
    // Check positive gain
    let gain_ok = if let Some(gain_ci) = doc.get("gain_ci_low").and_then(|g| g.as_f64()) {
        gain_ci > 0.0
    } else if let Some(mean) = doc
        .get("bolt_gain_pct")
        .and_then(|b| b.get("mean"))
        .and_then(|m| m.as_f64())
    {
        mean < 0.0
    } else {
        false
    };
    if !gain_ok {
        return None;
    }

    let lbr = doc.get("bolt_lbr");
    let samples = lbr
        .and_then(|l| l.get("samples"))
        .and_then(|s| s.as_str().or_else(|| s.as_u64().map(|_| "")))
        .map(|s| s.to_string());
    let fdata_sha = lbr
        .and_then(|l| l.get("fdata_sha256"))
        .and_then(|s| s.as_str())
        .map(|s| s.to_string());

    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let hex = digest_to_hex(hasher.finalize());
    let proof_id = format!("ab-opt-ceremony:{}", &hex[..16.min(hex.len())]);

    Some((proof_id, samples, fdata_sha))
}

/// Parse `pgo-workload-receipt.json` if present.
pub fn parse_pgo_workload_receipt<P: AsRef<Path>>(path: P) -> Option<PgoWorkloadInfo> {
    let content = fs::read_to_string(path).ok()?;
    let doc: serde_json::Value = serde_json::from_str(&content).ok()?;
    let status = doc
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("UNKNOWN")
        .to_string();
    let gain = doc.get("excitation_gain").and_then(|g| g.as_f64());
    let scenarios = doc.get("total_scenarios").and_then(|s| s.as_u64());
    let callbacks = doc.get("total_callbacks").and_then(|c| c.as_u64());
    Some(PgoWorkloadInfo {
        status,
        excitation_gain: gain,
        total_scenarios: scenarios,
        total_callbacks: callbacks,
        clipping_count_phase1: None,
        clipping_count_bolt: None,
        log_sha256: None,
        receipt_sha256: None,
    })
}

/// Parse codegen metrics json file: returns `(symbols, targets_matched, targets_total)`.
pub fn parse_codegen_metrics<P: AsRef<Path>>(path: P) -> Option<(u64, u64, u64)> {
    let content = fs::read_to_string(path).ok()?;
    let doc: serde_json::Value = serde_json::from_str(&content).ok()?;
    let symbols = doc.get("symbols").and_then(|s| s.as_u64())?;
    let matched = doc.get("targets_matched").and_then(|m| m.as_u64())?;
    let total = doc.get("targets_total").and_then(|t| t.as_u64())?;
    Some((symbols, matched, total))
}

/// Compare pre and post codegen metrics. Returns `(verdict, pre_symbols, post_symbols)`.
pub fn compare_codegen(
    pre_path: Option<&Path>,
    post_path: Option<&Path>,
) -> (String, Option<u64>, Option<u64>) {
    let pre = pre_path.and_then(parse_codegen_metrics);
    let post = post_path.and_then(parse_codegen_metrics);
    let pre_syms = pre.map(|(s, _, _)| s);
    let post_syms = post.map(|(s, _, _)| s);

    let verdict = match (pre, post) {
        (None, None) => "absent".to_string(),
        (None, Some(_)) => "post_only".to_string(),
        (Some(_), None) => "pre_only".to_string(),
        (Some((pre_s, pre_m, _)), Some((post_s, post_m, _))) => {
            let fewer_targets = post_m < pre_m;
            let fewer_symbols = post_s < pre_s;
            if fewer_targets && fewer_symbols {
                "post_has_fewer_symbols_and_targets".to_string()
            } else if fewer_targets {
                "post_has_fewer_targets".to_string()
            } else if fewer_symbols {
                "post_has_fewer_symbols".to_string()
            } else {
                "match".to_string()
            }
        }
    };

    (verdict, pre_syms, post_syms)
}
