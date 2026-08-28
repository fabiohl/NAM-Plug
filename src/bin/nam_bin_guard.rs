// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! CLI entry point for the fail-closed AVX-512 absence certificate.
//!
//! Reuses the authoritative EVEX prefix (`0x62`) binary decoder and defensive
//! symbol scanner from `neural_amp_modeler_rs::testing::bin_guard`.
//! It scans an ELF shared library/executable or GNU `ar` archive for EVEX-prefixed
//! instructions and forbidden AVX-512 symbols, failing closed on any error.
//!
//! Usage:
//!   nam_bin_guard scan <artifact> [--objdump PATH] [--nm PATH]
//!
//! Exit codes:
//!   0 — certificate passed (zero EVEX, zero forbidden symbols).
//!   1 — EVEX or forbidden symbol found, or any fail-closed tool/format error.
//!   2 — usage error.

use neural_amp_modeler_rs::testing::bin_guard::{
    EvexScanReport, ToolKind, resolve_llvm_tool, scan_for_evex, scan_symbols,
};
use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// AVX-512 kernel/symbol names that must never appear in the default build.
/// Defense-in-depth only; the authoritative proof is the EVEX opcode scan.
const FORBIDDEN_SYMBOLS: &[&str] = &[
    "gemv_4gate_avx512",
    "dot_product_4x_f32_avx512",
    "Avx512Math",
    "process_sample_avx512",
    "process_avx512",
    "hard_swish_slice_avx512",
    "leaky_hard_tanh_slice_avx512",
    "simd_relu_avx512",
    "relu_slice_avx512",
    "simd_silu_avx512",
    "silu_slice_avx512",
    "simd_silu_poly_avx512",
    "silu_poly_slice_avx512",
    "simd_tanh_sigmoid_dual_avx512",
];

/// SHA-256 of the artifact bytes, logged before inspection so a human can
/// audit that the certificate refers to the freshly-built artifact.
fn sha256_of(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    Ok(Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn usage() -> ! {
    eprintln!("Usage: nam_bin_guard scan <artifact> [--objdump PATH] [--nm PATH]");
    std::process::exit(2);
}

fn print_report(report: &EvexScanReport, sha: &str, symbol_matches: &[String]) {
    println!("artifact: {}", report.artifact.display());
    println!("sha256: {sha}");
    println!("sections_scanned: {}", report.sections_scanned);
    println!("instructions_scanned: {}", report.instructions_scanned);
    println!("members_scanned: {}", report.members_scanned);
    println!("bitcode_members: {}", report.bitcode_members);
    println!("metadata_members: {}", report.metadata_members);
    println!("evex_violations: {}", report.evex_violations.len());
    println!("forbidden_symbols: {}", symbol_matches.len());
    for violation in &report.evex_violations {
        let where_ = violation
            .member
            .as_deref()
            .map(|m| format!("{m}:"))
            .unwrap_or_default();
        println!("EVEX {where_}{}: {}", violation.section, violation.line);
    }
    for line in symbol_matches {
        println!("SYMBOL {line}");
    }
    println!(
        "result: {}",
        if report.is_clean() && symbol_matches.is_empty() {
            "PASS"
        } else {
            "FAIL"
        }
    );
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some("scan") {
        usage();
    }

    let artifact = args.get(1).map(PathBuf::from).unwrap_or_else(|| usage());
    let mut custom_objdump: Option<PathBuf> = None;
    let mut custom_nm: Option<PathBuf> = None;

    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--objdump" => {
                i += 1;
                custom_objdump = Some(PathBuf::from(args.get(i).unwrap_or_else(|| usage())));
            }
            "--nm" => {
                i += 1;
                custom_nm = Some(PathBuf::from(args.get(i).unwrap_or_else(|| usage())));
            }
            _ => usage(),
        }
        i += 1;
    }

    let objdump = match custom_objdump {
        Some(p) => p,
        None => match resolve_llvm_tool(ToolKind::LlvmObjdump) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("nam_bin_guard: fail-closed tool resolution error: {e}");
                return ExitCode::from(1);
            }
        },
    };

    let nm = match custom_nm {
        Some(p) => p,
        None => match resolve_llvm_tool(ToolKind::LlvmNm) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("nam_bin_guard: fail-closed tool resolution error: {e}");
                return ExitCode::from(1);
            }
        },
    };

    let sha = match sha256_of(&artifact) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("nam_bin_guard: fail-closed artifact error: {e}");
            return ExitCode::from(1);
        }
    };

    let report = match scan_for_evex(&objdump, &artifact) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("nam_bin_guard: fail-closed scan error: {e}");
            return ExitCode::from(1);
        }
    };

    let symbol_matches = match scan_symbols(&nm, &artifact, FORBIDDEN_SYMBOLS) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("nam_bin_guard: fail-closed symbol error: {e}");
            return ExitCode::from(1);
        }
    };

    print_report(&report, &sha, &symbol_matches);

    if report.is_clean() && symbol_matches.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
