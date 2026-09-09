// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! CLI entry point for the fail-closed AVX-512 absence certificate.
//!
//! Reuses the authoritative EVEX prefix (`0x62`) binary decoder and defensive
//! symbol scanner from `neural_amp_modeler_rs::testing::bin_guard`.
//! It scans an ELF shared library/executable or GNU `ar` archive for EVEX-prefixed
//! instructions and forbidden AVX-512 symbols, failing closed on any error.
//!
//! # Verification Policy
//!
//! 1. **Engine Invariant (Strict Absence of Hand-Written AVX-512):**
//!    Hand-written AVX-512 code in `NeuralAmpModeler-rs` or `NAM-Plug` (e.g.
//!    `gemv_4gate_avx512`, `dot_product_4x_f32_avx512`, `Avx512Math`, `process_sample_avx512`,
//!    and float SIMD kernels) did not prove value and must never appear in default builds.
//!    Any `FORBIDDEN_SYMBOLS` or non-supply-chain EVEX instructions cause immediate failure.
//!    When scanning crate archives (`.rlib`), zero EVEX instructions are tolerated.
//!
//! 2. **Supply-Chain Invariant (Permitted with Dynamic CPU Detection):**
//!    Across the third-party supply chain (such as `crc32fast` 1.5+ utilizing `VPCLMULQDQ` /
//!    `VPTERNLOGQ` polynomial reduction routines guarded by runtime CPU detection
//!    `is_x86_feature_detected!`), AVX-512 is beneficial and permitted in linked dynamic libraries
//!    (`.so`, `.clap`) unless strict mode (`--strict`) is requested.
//!
//! Usage:
//!   nam_bin_guard scan <artifact> [--objdump PATH] [--nm PATH] [--strict]
//!
//! Exit codes:
//!   0 — certificate passed (zero fatal EVEX, zero forbidden symbols).
//!   1 — fatal EVEX or forbidden symbol found, or any fail-closed tool/format error.
//!   2 — usage error.

use neural_amp_modeler_rs::testing::bin_guard::{
    EvexScanReport, EvexViolation, ToolKind, resolve_llvm_tool, scan_for_evex, scan_symbols,
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

/// Mnemonics permitted in third-party supply-chain dependencies with dynamic CPU detection
/// (e.g. crc32fast 1.5+ VPCLMULQDQ / VPTERNLOGQ polynomial reduction).
/// Hand-written engine kernels (GEMV, activations, resampler convolution) use float SIMD
/// (vfmadd*, vmulps, vaddps, vmaxps, vminps, vcmpps, etc.) and are strictly prohibited.
const ALLOWED_SUPPLY_CHAIN_MNEMONICS: &[&str] = &[
    "vpclmulqdq",
    "vpternlogq",
    "vextracti32x4",
    "vpbroadcastq",
    "vmovdqu64",
    "vpxorq",
];

/// Extracts the instruction mnemonic from a violation line formatted as
/// `"0x<addr>: <mnemonic> <operands> (<hex bytes>)"`.
fn extract_mnemonic(line: &str) -> Option<&str> {
    let colon_pos = line.find(':')?;
    let after_colon = line[colon_pos + 1..].trim_start();
    after_colon.split_whitespace().next()
}

/// Checks if an EVEX instruction is an allowed supply-chain instruction with runtime CPU detection.
fn is_allowed_supply_chain_instruction(line: &str) -> bool {
    if let Some(mnemonic) = extract_mnemonic(line) {
        ALLOWED_SUPPLY_CHAIN_MNEMONICS.contains(&mnemonic)
    } else {
        false
    }
}

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
    eprintln!("Usage: nam_bin_guard scan <artifact> [--objdump PATH] [--nm PATH] [--strict]");
    std::process::exit(2);
}

fn print_report(
    report: &EvexScanReport,
    sha: &str,
    symbol_matches: &[String],
    supply_chain_allowed: usize,
    fatal_violations: &[&EvexViolation],
) {
    println!("artifact: {}", report.artifact.display());
    println!("sha256: {sha}");
    println!("sections_scanned: {}", report.sections_scanned);
    println!("instructions_scanned: {}", report.instructions_scanned);
    println!("members_scanned: {}", report.members_scanned);
    println!("bitcode_members: {}", report.bitcode_members);
    println!("metadata_members: {}", report.metadata_members);
    println!("evex_violations: {}", fatal_violations.len());
    println!("supply_chain_evex_allowed: {supply_chain_allowed}");
    println!("forbidden_symbols: {}", symbol_matches.len());
    for violation in fatal_violations {
        let where_ = violation
            .member
            .as_deref()
            .map(|m| format!("{m}:"))
            .unwrap_or_default();
        println!(
            "FATAL_EVEX {where_}{}: {}",
            violation.section, violation.line
        );
    }
    for line in symbol_matches {
        println!("SYMBOL {line}");
    }
    println!(
        "result: {}",
        if fatal_violations.is_empty() && symbol_matches.is_empty() {
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
    let mut strict = false;

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
            "--strict" => {
                strict = true;
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

    // An archive (.rlib) contains exclusively crate-local objects — zero tolerance for EVEX.
    // A linked binary (.so / .clap) may contain approved supply-chain instructions with
    // runtime feature detection unless --strict is requested.
    let is_archive =
        artifact.extension().and_then(|s| s.to_str()) == Some("rlib") || report.members_scanned > 0;
    let allow_supply_chain = !strict && !is_archive;

    let mut supply_chain_allowed = 0;
    let mut fatal_violations = Vec::new();

    for violation in &report.evex_violations {
        if allow_supply_chain && is_allowed_supply_chain_instruction(&violation.line) {
            supply_chain_allowed += 1;
        } else {
            fatal_violations.push(violation);
        }
    }

    print_report(
        &report,
        &sha,
        &symbol_matches,
        supply_chain_allowed,
        &fatal_violations,
    );

    if fatal_violations.is_empty() && symbol_matches.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_mnemonic() {
        let line = "0xe9d4de: vpclmulqdq $0x0, %zmm4, %zmm3, %zmm5 (62 f3 65 48 44 ec 00)";
        assert_eq!(extract_mnemonic(line), Some("vpclmulqdq"));

        let line_with_comment = "0xe9d4f6: vpternlogq $0x96, 0x100(%rsi), %zmm5, %zmm3 # zmm3 = zmm3 ^ zmm5 ^ mem (62 f3 d5 48 25 5e 04 96)";
        assert_eq!(extract_mnemonic(line_with_comment), Some("vpternlogq"));

        let invalid_line = "Disassembly of section .text:";
        assert_eq!(extract_mnemonic(invalid_line), None);
    }

    #[test]
    fn test_allowed_supply_chain_mnemonics() {
        assert!(is_allowed_supply_chain_instruction(
            "0x100: vpclmulqdq $0, %zmm1, %zmm2 (62 ...)"
        ));
        assert!(is_allowed_supply_chain_instruction(
            "0x104: vpternlogq $0x96, %zmm1, %zmm2 (62 ...)"
        ));
        assert!(is_allowed_supply_chain_instruction(
            "0x108: vextracti32x4 $2, %zmm1, %xmm2 (62 ...)"
        ));
        assert!(is_allowed_supply_chain_instruction(
            "0x10c: vpbroadcastq %rax, %zmm1 (62 ...)"
        ));
        assert!(is_allowed_supply_chain_instruction(
            "0x110: vmovdqu64 (%rax), %zmm1 (62 ...)"
        ));
        assert!(is_allowed_supply_chain_instruction(
            "0x114: vpxorq %zmm1, %zmm2, %zmm3 (62 ...)"
        ));

        // Float / DSP / neural net instructions are strictly forbidden
        assert!(!is_allowed_supply_chain_instruction(
            "0x200: vfmadd213ps %zmm1, %zmm2, %zmm3 (62 ...)"
        ));
        assert!(!is_allowed_supply_chain_instruction(
            "0x204: vmulps %zmm1, %zmm2, %zmm3 (62 ...)"
        ));
        assert!(!is_allowed_supply_chain_instruction(
            "0x208: vaddps %zmm1, %zmm2, %zmm3 (62 ...)"
        ));
        assert!(!is_allowed_supply_chain_instruction(
            "0x20c: vmaxps %zmm1, %zmm2, %zmm3 (62 ...)"
        ));
        assert!(!is_allowed_supply_chain_instruction(
            "0x210: vminps %zmm1, %zmm2, %zmm3 (62 ...)"
        ));
        assert!(!is_allowed_supply_chain_instruction(
            "0x214: vcmpps $0, %zmm1, %zmm2, %k1 (62 ...)"
        ));
    }
}
