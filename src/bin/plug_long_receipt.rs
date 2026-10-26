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
//! The phase roster of this suite is `phase1`..`phase5` + terminal `overall`.
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
use nam_plug::receipt::build::{
    BuildArtifacts, BuildOptimizations, BuildProvenance, BuildReceipt, EngineProvenance,
    PackageInfo, compare_codegen, hash_file, parse_ab_opt_ceremony_receipt,
    parse_pgo_workload_receipt,
};
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
pub const PLUG_LONG_ROSTER: [&str; 6] = LONG_ROSTER;

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
        "build-receipt" => cmd_build_receipt(&args[1..]),
        "smoke-load" => cmd_smoke_load(&args[1..]),
        "certify" => cmd_certify(&args[1..]),
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
    println!("NAM-Plug Long-Audit & Build Receipt Tools (unified schema)");
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
    println!("  plug_long_receipt build-receipt [--out <file>] [--artifact <path>] [options...]");
    println!("  plug_long_receipt smoke-load --artifact <path>");
    println!(
        "  plug_long_receipt certify [--artifact <path>] [--build-receipt <path>] [--long-receipt <path>] [--out <file>] [--strict] [--run-id <id>]"
    );
    println!();
    println!(
        "Status values: PASSED, FAILED, SKIPPED, INCONCLUSIVE, SKIP_CAPABILITY, NOT_RUN, SIMULATED"
    );
    println!("Canonical roster: {}", PLUG_LONG_ROSTER.join(", "));
    println!("Default output file: {DEFAULT_OUT}");
}

const VALUE_LESS_FLAGS: &[&str] = &["strict", "package", "install", "certify"];

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

    let run_id = flags
        .get("run-id")
        .cloned()
        .or_else(|| env::var("NAM_RUN_ID").ok());

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
    let line = render_with_plug_fields(
        &receipt,
        artifact_path.as_deref(),
        artifact_exercised,
        run_id.as_deref(),
    );
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
    let run_id = flags
        .get("run-id")
        .cloned()
        .or_else(|| env::var("NAM_RUN_ID").ok())
        .or_else(|| {
            raw_entries.iter().find_map(|e| {
                e.get("run_id")
                    .and_then(|r| r.as_str())
                    .map(|s| s.to_string())
            })
        });
    if let Some(rid) = run_id {
        summary_value["run_id"] = serde_json::Value::String(rid);
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
    run_id: Option<&str>,
) -> String {
    match serde_json::to_value(receipt) {
        Ok(mut value) => {
            if let Some(path) = artifact_path {
                value[ARTIFACT_PATH_KEY] = serde_json::Value::String(path.to_string());
                value[ARTIFACT_EXERCISED_KEY] = serde_json::Value::Bool(artifact_exercised);
            }
            if let Some(rid) = run_id {
                value["run_id"] = serde_json::Value::String(rid.to_string());
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

fn extract_engine_provenance(cargo_lock_path: &Path) -> EngineProvenance {
    if let Ok(content) = fs::read_to_string(cargo_lock_path) {
        let mut in_pkg = false;
        let mut version = "unknown".to_string();
        let mut checksum = None;
        let mut dep_type = "workspace_or_path".to_string();

        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed == "[[package]]" {
                in_pkg = false;
            } else if trimmed == "name = \"NeuralAmpModeler-rs\"" {
                in_pkg = true;
            } else if in_pkg {
                if let Some(v) = trimmed.strip_prefix("version = \"") {
                    version = v.trim_end_matches('"').to_string();
                } else if let Some(s) = trimmed.strip_prefix("source = \"") {
                    let src = s.trim_end_matches('"');
                    if src.starts_with("registry+") {
                        dep_type = "crates.io".to_string();
                    } else if src.starts_with("git+") {
                        dep_type = "git".to_string();
                    } else {
                        dep_type = src.to_string();
                    }
                } else if let Some(c) = trimmed.strip_prefix("checksum = \"") {
                    checksum = Some(c.trim_end_matches('"').to_string());
                } else if trimmed.starts_with("[[") {
                    break;
                }
            }
        }
        EngineProvenance {
            dependency_type: dep_type,
            version,
            checksum,
        }
    } else {
        EngineProvenance {
            dependency_type: "missing_lockfile".to_string(),
            version: "unknown".to_string(),
            checksum: None,
        }
    }
}

fn cmd_smoke_load(args: &[String]) {
    let flags = match parse_flags(args) {
        Ok(f) => f,
        Err(e) => die_usage("smoke-load", &e),
    };
    let artifact = match required(&flags, "artifact") {
        Ok(v) => PathBuf::from(v),
        Err(e) => die_usage("smoke-load", &e),
    };

    if !artifact.exists() {
        eprintln!("smoke-load: artifact not found: {}", artifact.display());
        exit(1);
    }

    let c_path = match std::ffi::CString::new(artifact.to_string_lossy().as_bytes()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("smoke-load: invalid path string: {e}");
            exit(1);
        }
    };

    // SAFETY: dlopen is standard POSIX dynamic library loading with RTLD_NOW | RTLD_LOCAL.
    let handle = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if handle.is_null() {
        // SAFETY: dlerror retrieves the dynamic linker error string.
        let err_str = unsafe {
            let err = libc::dlerror();
            if err.is_null() {
                "unknown dlopen error".to_string()
            } else {
                std::ffi::CStr::from_ptr(err).to_string_lossy().into_owned()
            }
        };
        eprintln!(
            "smoke-load: dlopen failed for {}: {err_str}",
            artifact.display()
        );
        exit(1);
    }

    let symbol = std::ffi::CString::new("clap_entry").expect("valid CString");
    // SAFETY: dlsym looks up the exported CLAP entry point symbol in the loaded library.
    let sym = unsafe { libc::dlsym(handle, symbol.as_ptr()) };
    if sym.is_null() {
        eprintln!(
            "smoke-load: symbol 'clap_entry' not found in {}",
            artifact.display()
        );
        // SAFETY: close handle before exiting
        unsafe {
            libc::dlclose(handle);
        }
        exit(1);
    }

    // SAFETY: dlclose releases the library handle.
    unsafe {
        libc::dlclose(handle);
    }

    println!("SMOKE_LOAD: PASS (dlopen OK, clap_entry resolved)");
    exit(0);
}

fn cmd_certify(args: &[String]) {
    let flags = match parse_flags(args) {
        Ok(f) => f,
        Err(e) => die_usage("certify", &e),
    };
    let artifact = flags
        .get("artifact")
        .cloned()
        .unwrap_or_else(|| "target/dist/nam_plug.clap".to_string());
    let build_receipt_path = flags
        .get("build-receipt")
        .cloned()
        .unwrap_or_else(|| "target/dist/build-receipt.json".to_string());
    let long_receipt_path = flags
        .get("long-receipt")
        .cloned()
        .unwrap_or_else(|| "target/logs/long-audit-receipt.jsonl".to_string());
    let out = flags
        .get("out")
        .cloned()
        .unwrap_or_else(|| "target/dist/release-receipt.json".to_string());
    let strict = flags.contains_key("strict");
    let run_id = flags
        .get("run-id")
        .cloned()
        .or_else(|| env::var("NAM_RUN_ID").ok());

    let release_receipt = nam_plug::receipt::release::evaluate_certification(
        Path::new(&artifact),
        Path::new(&build_receipt_path),
        Path::new(&long_receipt_path),
        strict,
        run_id.as_deref(),
    );

    let json = match serde_json::to_string_pretty(&release_receipt) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("certify: failed to serialize release receipt: {e}");
            exit(1);
        }
    };

    if let Err(e) = write_file(&out, &json) {
        eprintln!("certify: failed to write {out}: {e}");
        exit(1);
    }

    if release_receipt.status == "CERTIFIED" {
        println!(
            "CERTIFIED: PASS log={out} sha256={}",
            release_receipt.artifacts.artifact_sha256
        );
        exit(0);
    } else {
        eprintln!(
            "REJECTED: FAIL log={out} reasons={}",
            release_receipt.reasons.join("; ")
        );
        exit(1);
    }
}

fn cmd_build_receipt(args: &[String]) {
    let flags = match parse_flags(args) {
        Ok(f) => f,
        Err(e) => die_usage("build-receipt", &e),
    };

    let out = flags
        .get("out")
        .cloned()
        .unwrap_or_else(|| "target/dist/build-receipt.json".to_string());
    let artifact_path_str = flags
        .get("artifact")
        .cloned()
        .unwrap_or_else(|| "target/dist/nam_plug.clap".to_string());
    let pkg_name = flags
        .get("package-name")
        .cloned()
        .unwrap_or_else(|| "nam-plug".to_string());
    let pkg_version = flags
        .get("package-version")
        .cloned()
        .unwrap_or_else(|| "1.0.0".to_string());
    let git_commit = flags
        .get("git-commit")
        .cloned()
        .unwrap_or_else(|| "unknown".to_string());
    let git_branch = flags
        .get("git-branch")
        .cloned()
        .unwrap_or_else(|| "unknown".to_string());
    let git_dirty = flags
        .get("git-dirty")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);
    let cargo_lock = flags
        .get("cargo-lock")
        .cloned()
        .unwrap_or_else(|| "Cargo.lock".to_string());
    let rustc_version = flags
        .get("rustc-version")
        .cloned()
        .unwrap_or_else(|| "unknown".to_string());
    let rustflags = flags.get("rustflags").cloned().unwrap_or_default();
    let pgo_status = flags
        .get("pgo-status")
        .cloned()
        .unwrap_or_else(|| "skipped".to_string());
    let bolt_status = flags
        .get("bolt-status")
        .cloned()
        .unwrap_or_else(|| "skipped".to_string());
    let bolt_version = flags.get("bolt-version").cloned();
    let bolt_profile = flags.get("bolt-profile").cloned();
    let ceremony_receipt = flags.get("ceremony-receipt").cloned();
    let pgo_workload_receipt = flags.get("pgo-workload-receipt").cloned();
    let codegen_csv_pre = flags.get("codegen-csv-pre").cloned();
    let codegen_csv_post = flags.get("codegen-csv-post").cloned();
    let tarball_path = flags.get("tarball-path").cloned();
    let flatpak_path = flags.get("flatpak-path").cloned();
    let installed_path = flags.get("installed-path").cloned();
    let strict = flags.contains_key("strict");
    let run_id = flags
        .get("run-id")
        .cloned()
        .or_else(|| env::var("NAM_RUN_ID").ok());

    let mut gaps: Vec<String> = Vec::new();
    if let Some(g) = flags.get("gaps") {
        for item in g.split(',') {
            let item = item.trim();
            if !item.is_empty() {
                gaps.push(item.to_string());
            }
        }
    }

    let art_path = Path::new(&artifact_path_str);
    if !art_path.exists() {
        eprintln!("build-receipt: artifact not found: {artifact_path_str}");
        exit(1);
    }

    let artifact_sha256 = match hash_file(art_path) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("build-receipt: failed to hash artifact {artifact_path_str}: {e}");
            exit(1);
        }
    };

    let cargo_lock_path = Path::new(&cargo_lock);
    let cargo_lock_sha256 = hash_file(cargo_lock_path).unwrap_or_else(|_| "absent".to_string());
    let engine_provenance = extract_engine_provenance(cargo_lock_path);

    let tarball_sha256 = tarball_path
        .as_ref()
        .and_then(|p| hash_file(Path::new(p)).ok());
    let flatpak_sha256 = flatpak_path
        .as_ref()
        .and_then(|p| hash_file(Path::new(p)).ok());
    let installed_sha256 = installed_path
        .as_ref()
        .and_then(|p| hash_file(Path::new(p)).ok());

    let (bolt_proof, bolt_lbr_samples, bolt_lbr_fdata_sha256) =
        if let Some(cr) = ceremony_receipt.as_deref() {
            match parse_ab_opt_ceremony_receipt(cr) {
                Some((proof_id, samples, fdata)) => (proof_id, samples, fdata),
                None => {
                    if bolt_status == "applied" {
                        gaps.push("bolt_proof_unverified".to_string());
                        ("unverified".to_string(), None, None)
                    } else {
                        ("skipped".to_string(), None, None)
                    }
                }
            }
        } else if bolt_status == "applied" {
            ("unverified".to_string(), None, None)
        } else {
            ("skipped".to_string(), None, None)
        };

    let bolt_profile_sha256 = bolt_profile
        .as_ref()
        .and_then(|p| hash_file(Path::new(p)).ok());

    let (codegen_comparison, pre_bolt_symbols, post_bolt_symbols) = compare_codegen(
        codegen_csv_pre.as_deref().map(Path::new),
        codegen_csv_post.as_deref().map(Path::new),
    );

    let pgo_workload = pgo_workload_receipt
        .as_deref()
        .and_then(parse_pgo_workload_receipt);

    let pgo_applied = pgo_status == "applied";
    let bolt_applied = bolt_status == "applied";

    if !pgo_applied {
        gaps.push("pgo_not_applied".to_string());
    }
    if !bolt_applied {
        gaps.push("bolt_not_applied".to_string());
    }

    gaps.sort();
    gaps.dedup();

    let status = if !gaps.is_empty() && strict {
        "FAILED".to_string()
    } else if !gaps.is_empty() {
        "PASS_WITH_GAPS".to_string()
    } else {
        "PASSED".to_string()
    };

    let receipt = BuildReceipt {
        schema_version: "1.0.0".to_string(),
        timestamp_utc: now_iso8601(),
        status: status.clone(),
        strict_mode: strict,
        gaps,
        package: PackageInfo {
            name: pkg_name,
            version: pkg_version,
        },
        provenance: BuildProvenance {
            git_commit,
            git_branch,
            git_dirty,
            cargo_lock_sha256,
            rustc_version,
            rustflags,
            run_id,
        },
        engine_provenance,
        optimizations: BuildOptimizations {
            pgo_applied,
            bolt_applied,
            bolt_proof,
            bolt_profile_sha256,
            bolt_version,
            bolt_lbr_samples,
            bolt_lbr_fdata_sha256,
            codegen_comparison,
            pre_bolt_symbols,
            post_bolt_symbols,
        },
        pgo_workload,
        artifacts: BuildArtifacts {
            artifact_path: artifact_path_str,
            artifact_sha256: artifact_sha256.clone(),
            pre_strip_artifact_sha256: None,
            tarball_path,
            tarball_sha256,
            flatpak_path,
            flatpak_sha256,
            installed_path,
            installed_sha256,
        },
    };

    let json = match serde_json::to_string_pretty(&receipt) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("build-receipt: failed to serialize json: {e}");
            exit(1);
        }
    };

    if let Err(e) = write_file(&out, &json) {
        eprintln!("build-receipt: failed to write {out}: {e}");
        exit(1);
    }

    if status == "FAILED" {
        eprintln!("BUILD_RECEIPT: FAIL rc=1 log={out}");
        exit(1);
    } else {
        println!("BUILD_RECEIPT: PASS log={out} sha256={artifact_sha256}");
        exit(0);
    }
}
