// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Engine-metric re-exports for NAM-Plug integration test binaries.
//!
//! The plugin no longer carries private copies of `compute_esr`/`compute_snr_db`
//! (perceptual metrics); all test binaries share the engine's definitions so a
//! fix lands exactly once. Generic helpers (`max_abs_diff`, exact percentiles)
//! live in `neural_amp_modeler_rs::testing::metrics` and are imported where used.

pub use neural_amp_modeler_rs::testing::perceptual::{compute_esr, compute_snr_db};
