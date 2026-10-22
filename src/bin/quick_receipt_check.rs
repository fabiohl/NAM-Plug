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
//!   `quick_receipt_check <path> [--require-passed] [--require-heap-audit]
//!    [--require-strict] [--expect-artifact-sha256 <hex>]`
//!
//! `--require-strict` and `--expect-artifact-sha256` let the release gate prove
//! that an existing receipt may be reused: it must come from a strict run and
//! must describe exactly the artifact about to be released.
//!
//! Exit codes:
//!   * `0` — receipt parsed, consistent (and, when required, green).
//!   * `1` — receipt missing, unparseable or inconsistent.
//!   * `2` — usage error.

use std::process::exit;

use nam_plug::receipt::quick::parse_quick_receipt;

const USAGE: &str = "usage: quick_receipt_check <path> [--require-passed] [--require-heap-audit] [--require-strict] [--expect-artifact-sha256 <hex>]";

fn main() {
    let mut args = std::env::args().skip(1);
    let mut path: Option<String> = None;
    let mut require_passed = false;
    let mut require_heap_audit = false;
    let mut require_strict = false;
    let mut expect_artifact_sha256: Option<String> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--require-passed" => require_passed = true,
            "--require-heap-audit" => require_heap_audit = true,
            "--require-strict" => require_strict = true,
            "--expect-artifact-sha256" => match args.next() {
                Some(v) if !v.starts_with("--") => expect_artifact_sha256 = Some(v),
                _ => {
                    eprintln!("quick_receipt_check: --expect-artifact-sha256 requires a value");
                    exit(2);
                }
            },
            "-h" | "--help" => {
                println!("{USAGE}");
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
        eprintln!("{USAGE}");
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
    if require_strict && !receipt.strict {
        eprintln!("quick_receipt_check: {path} was not produced by a strict run (STRICT: 0)");
        exit(1);
    }
    if let Some(expected) = expect_artifact_sha256 {
        match receipt.artifact_sha256.as_deref() {
            Some(declared) if declared.eq_ignore_ascii_case(&expected) => {}
            Some(declared) => {
                eprintln!(
                    "quick_receipt_check: {path} declares artifact {declared} but {expected} was expected"
                );
                exit(1);
            }
            None => {
                eprintln!("quick_receipt_check: {path} declares no ARTIFACT_SHA256");
                exit(1);
            }
        }
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
