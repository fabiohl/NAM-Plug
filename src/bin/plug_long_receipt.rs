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
//! - `append`:  validate one phase outcome and append its JSONL line.
//! - `summary`: read the receipt, derive and append the terminal `overall` line.
//! - `validate`: fail-closed semantic verification against the Plug roster
//!   (canonical roster + declared/derived `OVERALL` consistency); `--strict`
//!   additionally rejects any declared gap or failure.
//!
//! Exit codes: 0 success; 1 I/O or verification error; 2 usage error.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::str::FromStr;

use nam_plug::receipt::LONG_ROSTER;
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
    println!("  plug_long_receipt summary [--out <file>]");
    println!("  plug_long_receipt validate [--out <file>] [--strict]");
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

    let receipt = LongPhaseReceipt {
        phase_id,
        name,
        status,
        duration_ms,
        tests_executed,
        gaps,
        timestamp: now_iso8601(),
        ..Default::default()
    };
    if let Err(e) = append_line(&out, &receipt.render_jsonl_line()) {
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
    let mut audit = match LongAuditReceipt::parse_jsonl_file(Path::new(&out)) {
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
    audit.push_summary();
    let overall = audit
        .phases
        .last()
        .expect("push_summary always appends the overall line");
    if let Err(e) = write_file(&out, &audit.render_jsonl()) {
        eprintln!("summary: failed to write overall line to {out}: {e}");
        exit(1);
    }
    eprintln!(
        "✓ plug long-audit receipt: overall {} ({} tests, {} ms, {} gaps)",
        overall.status,
        overall.tests_executed,
        overall.duration_ms,
        overall.gaps.len()
    );
    // Machine-readable verdict on stdout for the shell runner (single line).
    println!("OVERALL: {}", overall.status.as_str());
    exit(0);
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
