// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Fail-closed semantic verifier for the NAM-Plug quick-suite receipt.
//!
//! Invoked by `utils/build-release.sh` to verify `target/logs/quick-receipt.txt`
//! with the shared semantic parser ([`nam_plug::receipt::quick`]) — never a
//! substring search. The receipt must parse fail-closed and audit cleanly
//! (`OVERALL:` consistent with the phase/gap evidence).
//!
//! Usage:
//!   `quick_receipt_check <path> [--require-passed] [--require-heap-audit]`
//!
//! Exit codes:
//!   * `0` — receipt parsed, consistent (and, when required, green).
//!   * `1` — receipt missing, unparseable or inconsistent.
//!   * `2` — usage error.

use std::process::exit;

use nam_plug::receipt::quick::parse_quick_receipt;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut path: Option<String> = None;
    let mut require_passed = false;
    let mut require_heap_audit = false;

    for arg in &args {
        match arg.as_str() {
            "--require-passed" => require_passed = true,
            "--require-heap-audit" => require_heap_audit = true,
            "-h" | "--help" => {
                println!(
                    "usage: quick_receipt_check <path> [--require-passed] [--require-heap-audit]"
                );
                exit(0);
            }
            other if other.starts_with("--") => {
                eprintln!("quick_receipt_check: unknown flag {other}");
                exit(2);
            }
            other => {
                if path.replace(other.to_string()).is_some() {
                    eprintln!("quick_receipt_check: unexpected extra argument {other}");
                    exit(2);
                }
            }
        }
    }

    let Some(path) = path else {
        eprintln!("usage: quick_receipt_check <path> [--require-passed] [--require-heap-audit]");
        exit(2);
    };

    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("quick_receipt_check: cannot read quick receipt {path}: {e}");
            exit(1);
        }
    };
    let receipt = match parse_quick_receipt(&text) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("quick_receipt_check: {path} is not parseable: {e}");
            exit(1);
        }
    };
    if let Err(e) = receipt.verify_release_gate(require_passed, require_heap_audit) {
        eprintln!("quick_receipt_check: {path} failed semantic verification: {e}");
        exit(1);
    }

    println!(
        "quick_receipt_check: OK (SUITE={}, STRICT={}, {} phases, evidence={:?}, OVERALL={})",
        receipt.suite,
        u8::from(receipt.strict),
        receipt.phases.len(),
        receipt.evidence,
        receipt.overall
    );
    exit(0);
}
