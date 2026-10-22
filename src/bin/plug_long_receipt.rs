// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Structured JSONL receipt emitter/verifier for the NAM-Plug long audit suite.
//!
//! `utils/tests-long.sh` appends one machine-readable line per completed phase
//! to `target/logs/long-audit-receipt.jsonl` using the **unified engine receipt
//! schema** ([`neural_amp_modeler_rs::testing::receipt`]) — the same schema the
//! engine and NAM-Audio-Pipe emit, so a single semantic verifier can validate
//! every producer. The suite-level `overall` line is derived in Rust by
//! `summary`, never re-derived from logs by the shell.
//!
//! The phase roster of this suite is `phase1`..`phase4` + terminal `overall`.
//!
//! Subcommands:
//! - `append`:  validate one phase outcome and append its JSONL line. Plug-local
//!   keys `artifact_path` and `artifact_exercised` record provenance and whether
//!   the phase actually loaded the shared object.
//! - `summary`: read the receipt, derive and append the terminal `overall` line.
//! - `validate`: fail-closed semantic verification against the Plug roster
//!   (canonical roster + declared/derived `OVERALL` consistency); `--strict`
//!   additionally rejects any declared gap or failure.
//! - `verify-artifact`: verify that every receipt line declares the installed
//!   CLAP artifact (`match`/`mismatch`/`absent`/`incomplete`).
//! - `artifact-status`: reconcile a receipt's declared hash against the hash
//!   recorded by the build receipt (no file re-hashing).
//!
//! Exit codes: 0 success; 1 I/O or verification error; 2 usage error.
//! `verify-artifact` additionally exits 3 when the receipt declares no artifact
//! (`absent`) and 4 when only some lines declare one (`incomplete`), so the build
//! receipt can record the typed gap without dying in non-strict mode.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::str::FromStr;

use nam_plug::receipt::LONG_ROSTER;
use nam_plug::receipt::long_artifact::{
    ARTIFACT_EXERCISED_KEY, ARTIFACT_PATH_KEY, ARTIFACT_SHA256_KEY, ArtifactConsistency,
    declared_artifact, verify_receipt_artifact, verify_receipt_declared_sha,
};
use neural_amp_modeler_rs::testing::receipt::{
    LongAuditReceipt, LongPhaseReceipt, LongPhaseStatus, count_tests_executed_from_log,
    detect_gap_markers, now_iso8601,
};

const DEFAULT_OUT: &str = "target/logs/long-audit-receipt.jsonl";

/// Canonical phase roster of the NAM-Plug long audit suite.
pub const PLUG_LONG_ROSTER: [&str; 5] = LONG_ROSTER;

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        print_help();
        exit(2);
    }
    match args[0].as_str() {
        "append" => cmd_append(&args[1..]),
        "summary" => cmd_summary(&args[1..]),
        "validate" => cmd_validate(&args[1..]),
        "verify-artifact" => cmd_verify_artifact(&args[1..]),
        "artifact-status" => cmd_artifact_status(&args[1..]),
        "-h" | "--help" => {
            print_help();
            exit(0);
        }
        other => {
            eprintln!("unknown subcommand: {other}");
            print_help();
            exit(2);
        }
    }
}

fn print_help() {
    println!("NAM-Plug Long-Audit Receipt (unified engine schema)");
    println!("Usage:");
    println!("  plug_long_receipt append --phase-id <id> --name <name> --status <STATUS>");
    println!("                        --duration-ms <ms> [--tests-executed <n>] [--log <path>]");
    println!("                        [--gaps <a,b,...>] [--out <file>]");
    println!("                        [--artifact-path <path>] [--artifact-sha256 <hex>]");
    println!("                        [--artifact-exercised true|false]");
    println!("  plug_long_receipt summary [--out <file>]");
    println!("  plug_long_receipt validate [--out <file>] [--strict]");
    println!("  plug_long_receipt verify-artifact --out <file> --artifact <path>");
    println!("  plug_long_receipt artifact-status --out <file> --expected-sha256 <hex>");
    println!();
    println!(
        "Status values: PASSED, FAILED, SKIPPED, INCONCLUSIVE, SKIP_CAPABILITY, NOT_RUN, SIMULATED"
    );
    println!("Canonical roster: {}", PLUG_LONG_ROSTER.join(", "));
    println!("Default output file: {DEFAULT_OUT}");
}

const VALUE_LESS_FLAGS: &[&str] = &["strict"];

fn parse_flags(args: &[String]) -> Result<std::collections::HashMap<String, String>, String> {
    let mut flags = std::collections::HashMap::new();
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        let name = flag.strip_prefix("--").unwrap_or(flag.as_str()).to_string();
        if name.is_empty() {
            return Err(format!("{flag}: expected a flag name"));
        }
        if VALUE_LESS_FLAGS.contains(&name.as_str()) {
            flags.insert(name, String::new());
            continue;
        }
        let value = iter
            .next()
            .ok_or_else(|| format!("{flag} requires a value"))?;
        flags.insert(name, value.clone());
    }
    Ok(flags)
}

fn required<'a>(
    flags: &'a std::collections::HashMap<String, String>,
    name: &str,
) -> Result<&'a String, String> {
    flags
        .get(name)
        .ok_or_else(|| format!("missing required flag --{name}"))
}

fn cmd_append(args: &[String]) {
    let flags = match parse_flags(args) {
        Ok(f) => f,
        Err(e) => die_usage("append", &e),
    };
    let phase_id = match required(&flags, "phase-id") {
        Ok(v) => v.clone(),
        Err(e) => die_usage("append", &e),
    };
    if !PLUG_LONG_ROSTER.contains(&phase_id.as_str()) {
        die_usage(
            "append",
            &format!(
                "unknown phase id '{phase_id}' (canonical: {})",
                PLUG_LONG_ROSTER.join(", ")
            ),
        );
    }
    let name = match required(&flags, "name") {
        Ok(v) => v.clone(),
        Err(e) => die_usage("append", &e),
    };
    let status = match required(&flags, "status").and_then(|s| LongPhaseStatus::from_str(s)) {
        Ok(s) => s,
        Err(e) => die_usage("append", &e),
    };
    let duration_ms: u64 = match required(&flags, "duration-ms") {
        Ok(v) => match v.parse() {
            Ok(n) => n,
            Err(_) => die_usage("append", "--duration-ms must be a non-negative integer"),
        },
        Err(e) => die_usage("append", &e),
    };
    let out = flags
        .get("out")
        .cloned()
        .unwrap_or_else(|| DEFAULT_OUT.to_string());

    let log_path = flags.get("log").map(PathBuf::from);
    let tests_executed = match flags.get("tests-executed") {
        Some(v) => match v.parse::<u64>() {
            Ok(n) => n,
            Err(_) => die_usage("append", "--tests-executed must be a non-negative integer"),
        },
        None => log_path
            .as_deref()
            .map(count_tests_executed_from_log)
            .unwrap_or(0),
    };

    let mut gaps: Vec<String> = Vec::new();
    if let Some(list) = flags.get("gaps") {
        for g in list.split(',') {
            let g = g.trim();
            if !g.is_empty() {
                gaps.push(g.to_string());
            }
        }
    }
    if let Some(id) = status.gap_id() {
        gaps.push(id.to_string());
    }
    if let Some(path) = log_path.as_deref() {
        for marker in detect_gap_markers(path) {
            if !gaps.contains(&marker) {
                gaps.push(marker);
            }
        }
    }

    let artifact_path = flags.get("artifact-path").cloned();
    let artifact_sha256 = flags.get("artifact-sha256").cloned();
    if artifact_path.is_some() != artifact_sha256.is_some() {
        die_usage(
            "append",
            "--artifact-path and --artifact-sha256 must be supplied together",
        );
    }
    let artifact_exercised = match flags.get("artifact-exercised") {
        None => false,
        Some(v) => match v.as_str() {
            "true" => true,
            "false" => false,
            _ => die_usage(
                "append",
                "--artifact-exercised must be exactly 'true' or 'false'",
            ),
        },
    };
    if artifact_exercised && artifact_sha256.is_none() {
        die_usage(
            "append",
            "--artifact-exercised true requires --artifact-path and --artifact-sha256",
        );
    }

    let mut receipt = LongPhaseReceipt {
        phase_id,
        name,
        status,
        duration_ms,
        tests_executed,
        gaps,
        timestamp: now_iso8601(),
        ..Default::default()
    };
    receipt.artifact_sha256 = artifact_sha256;
    let line = render_with_plug_fields(&receipt, artifact_path.as_deref(), artifact_exercised);
    if let Err(e) = append_line(&out, &line) {
        eprintln!("append: failed to write receipt to {out}: {e}");
        exit(1);
    }
    eprintln!(
        "✓ plug long-audit receipt: {} {} ({} tests, {} ms)",
        receipt.phase_id, receipt.status, receipt.tests_executed, receipt.duration_ms
    );
    exit(0);
}

fn cmd_summary(args: &[String]) {
    let flags = match parse_flags(args) {
        Ok(f) => f,
        Err(e) => die_usage("summary", &e),
    };
    let out = flags
        .get("out")
        .cloned()
        .unwrap_or_else(|| DEFAULT_OUT.to_string());
    let audit = match LongAuditReceipt::parse_jsonl_file(Path::new(&out)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("summary: cannot build summary from {out}: {e}");
            exit(1);
        }
    };
    if audit.phases.is_empty() {
        eprintln!("summary: no phase entries in {out} — nothing to summarize");
        exit(1);
    }

    // Preserve the raw phase lines verbatim: the engine schema is the single
    // source of truth for the verdict, while `artifact_path` is a Plug-local
    // provenance key that the engine type does not model. Re-rendering the
    // parsed structs would silently drop it.
    let raw = match fs::read_to_string(&out) {
        Ok(raw) => raw,
        Err(e) => {
            eprintln!("summary: cannot read {out}: {e}");
            exit(1);
        }
    };
    let mut kept_lines: Vec<String> = Vec::new();
    let mut raw_entries: Vec<serde_json::Value> = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(e) => {
                eprintln!("summary: invalid JSON line in {out}: {e}");
                exit(1);
            }
        };
        if value.get("phase_id").and_then(|v| v.as_str()) == Some("overall") {
            continue;
        }
        kept_lines.push(trimmed.to_string());
        raw_entries.push(value);
    }

    let summary = audit.summary_receipt();
    let declared = match declared_artifact(&raw_entries) {
        Ok(declared) => Some(declared),
        Err(nam_plug::receipt::long_artifact::DeclaredArtifactError::Conflicting) => {
            eprintln!("summary: receipt declares conflicting artifacts — refusing to summarize");
            exit(1);
        }
        Err(_) => None,
    };
    let mut summary_value = match serde_json::to_value(&summary) {
        Ok(value) => value,
        Err(e) => {
            eprintln!("summary: cannot serialize overall line: {e}");
            exit(1);
        }
    };
    if let Some(declared) = declared {
        summary_value[ARTIFACT_SHA256_KEY] = serde_json::Value::String(declared.sha256);
        if let Some(path) = declared.path {
            summary_value[ARTIFACT_PATH_KEY] = serde_json::Value::String(path);
        }
    }
    let summary_line = match serde_json::to_string(&summary_value) {
        Ok(line) => line,
        Err(e) => {
            eprintln!("summary: cannot render overall line: {e}");
            exit(1);
        }
    };
    kept_lines.push(summary_line);

    let mut content = kept_lines.join("\n");
    content.push('\n');
    if let Err(e) = write_file(&out, &content) {
        eprintln!("summary: failed to write overall line to {out}: {e}");
        exit(1);
    }
    eprintln!(
        "✓ plug long-audit receipt: overall {} ({} tests, {} ms, {} gaps)",
        summary.status,
        summary.tests_executed,
        summary.duration_ms,
        summary.gaps.len()
    );
    // Machine-readable verdict on stdout for the shell runner (single line).
    println!("OVERALL: {}", summary.status.as_str());
    exit(0);
}

fn cmd_verify_artifact(args: &[String]) {
    let flags = match parse_flags(args) {
        Ok(f) => f,
        Err(e) => die_usage("verify-artifact", &e),
    };
    let out = match required(&flags, "out") {
        Ok(v) => v.clone(),
        Err(e) => die_usage("verify-artifact", &e),
    };
    let artifact = match required(&flags, "artifact") {
        Ok(v) => PathBuf::from(v),
        Err(e) => die_usage("verify-artifact", &e),
    };

    let verdict = verify_receipt_artifact(Path::new(&out), &artifact);
    println!("ARTIFACT_CONSISTENCY: {}", verdict.as_str());
    println!(
        "ARTIFACT_SHA256: {}",
        verdict.declared_sha256().unwrap_or("-")
    );
    if let Some(reason) = verdict.reason() {
        eprintln!("verify-artifact: {reason}");
    }
    match verdict {
        ArtifactConsistency::Match { .. } => exit(0),
        ArtifactConsistency::Mismatch { .. } => exit(1),
        ArtifactConsistency::Absent { .. } => exit(3),
        ArtifactConsistency::Incomplete { .. } => exit(4),
    }
}

fn cmd_artifact_status(args: &[String]) {
    let flags = match parse_flags(args) {
        Ok(f) => f,
        Err(e) => die_usage("artifact-status", &e),
    };
    let out = match required(&flags, "out") {
        Ok(v) => v.clone(),
        Err(e) => die_usage("artifact-status", &e),
    };
    let expected = match required(&flags, "expected-sha256") {
        Ok(v) => v.clone(),
        Err(e) => die_usage("artifact-status", &e),
    };

    let verdict = verify_receipt_declared_sha(Path::new(&out), &expected);
    println!("ARTIFACT_CONSISTENCY: {}", verdict.as_str());
    println!(
        "ARTIFACT_SHA256: {}",
        verdict.declared_sha256().unwrap_or("-")
    );
    if let Some(reason) = verdict.reason() {
        eprintln!("artifact-status: {reason}");
    }
    match verdict {
        ArtifactConsistency::Match { .. } => exit(0),
        ArtifactConsistency::Mismatch { .. } => exit(1),
        ArtifactConsistency::Absent { .. } => exit(3),
        ArtifactConsistency::Incomplete { .. } => exit(4),
    }
}

fn cmd_validate(args: &[String]) {
    let flags = match parse_flags(args) {
        Ok(f) => f,
        Err(e) => die_usage("validate", &e),
    };
    let out = flags
        .get("out")
        .cloned()
        .unwrap_or_else(|| DEFAULT_OUT.to_string());
    let strict = flags.contains_key("strict");

    if !Path::new(&out).exists() {
        eprintln!("validate: receipt file not found: {out}");
        exit(1);
    }
    let audit = match LongAuditReceipt::parse_jsonl_file(Path::new(&out)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("validate: invalid receipt at {out}: {e}");
            exit(1);
        }
    };
    if let Err(e) = audit.semantic_verdict_with_roster(&PLUG_LONG_ROSTER) {
        eprintln!("validate: {e}");
        exit(1);
    }
    let overall = audit.summary_receipt();
    if strict {
        match audit.campaign_strict_verdict_with_roster(&PLUG_LONG_ROSTER) {
            Ok(()) => {
                println!("STRICT: PASSED (no gaps, no failures)");
                exit(0);
            }
            Err(reason) => {
                eprintln!("STRICT: FAIL — {reason}");
                exit(1);
            }
        }
    }
    println!(
        "VALID: {} receipt line(s) in {out} ({} tests, {} ms) | OVERALL: {}",
        audit.phases.len(),
        audit.tests_executed_total(),
        audit.duration_ms_total(),
        overall.status.as_str()
    );
    exit(0);
}

fn render_with_plug_fields(
    receipt: &LongPhaseReceipt,
    artifact_path: Option<&str>,
    artifact_exercised: bool,
) -> String {
    match serde_json::to_value(receipt) {
        Ok(mut value) => {
            if let Some(path) = artifact_path {
                value[ARTIFACT_PATH_KEY] = serde_json::Value::String(path.to_string());
                value[ARTIFACT_EXERCISED_KEY] = serde_json::Value::Bool(artifact_exercised);
            }
            serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
        }
        Err(_) => "{}".to_string(),
    }
}

fn append_line(out: &str, line: &str) -> std::io::Result<()> {
    if let Some(parent) = Path::new(out).parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(out)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")
}

fn write_file(out: &str, content: &str) -> std::io::Result<()> {
    if let Some(parent) = Path::new(out).parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(out, content)
}

fn die_usage(subcommand: &str, message: &str) -> ! {
    eprintln!("{subcommand}: {message}");
    eprintln!("Run `plug_long_receipt --help` for usage.");
    exit(2);
}
