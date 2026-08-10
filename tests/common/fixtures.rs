// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Local fixture resolution for NAM-Plug integration tests.
//!
//! Models live under `tests/fixtures/models/` (plugin-owned, license-cleared).
//! Do not call `neural_amp_modeler_rs::testing::fixtures::model_path` — that
//! resolves against the crates.io package tree, which ships no `.nam` assets.

use std::path::PathBuf;

/// Resolves a test model under NAM-Plug's own fixture tree.
///
/// Search order:
/// 1. `NAM_FIXTURES_DIR/{name}` when set
/// 2. `{CARGO_MANIFEST_DIR}/tests/fixtures/models/{name}`
pub fn model_path(name: &str) -> PathBuf {
    if let Ok(dir) = std::env::var("NAM_FIXTURES_DIR") {
        let p = PathBuf::from(dir).join(name);
        if p.exists() {
            return p;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/models")
        .join(name)
}
