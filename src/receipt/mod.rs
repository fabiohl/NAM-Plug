// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Receipt verification for the NAM-Plug QA suites.
//!
//! - [`quick`]: fail-closed semantic parser/verifier for the quick-suite
//!   receipt (`target/logs/quick-receipt.txt`). It replaces the historical
//!   substring search (`grep -q -E "OVERALL: ..."`) in `utils/build-release.sh`
//!   — a text that merely *contains* a green token must never certify a release.

pub mod quick;

/// Canonical phase roster of the NAM-Plug long audit suite: `phase1`..`phase4`
/// plus the terminal `overall` line. Shared by the `plug_long_receipt` binary
/// and the integration tests so validation and emission can never drift.
pub const LONG_ROSTER: [&str; 5] = ["phase1", "phase2", "phase3", "phase4", "overall"];
