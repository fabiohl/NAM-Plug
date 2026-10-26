// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Receipt verification for the NAM-Plug QA suites.

pub mod build;
pub mod long_artifact;
pub mod release;

/// Canonical phase roster of the NAM-Plug long audit suite: `phase1`..`phase5`
/// plus the terminal `overall` line. Shared by the `plug_long_receipt` binary
/// and the integration tests so validation and emission can never drift.
pub const LONG_ROSTER: [&str; 6] = ["phase1", "phase2", "phase3", "phase4", "phase5", "overall"];
