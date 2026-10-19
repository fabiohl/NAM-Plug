// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! # NAM-Plug — CLAP Audio Plugin Crate
//!
//! `NAM-Plug` wraps the core [`neural_amp_modeler_rs`] inference engine into a high-performance
//! [CLAP (CLever Audio Plug-in)](https://cleveraudioplug.in/) plugin format.
//!
//! ## Key Architectural Subsystems
//!
//! - **[`clap`]**: Core plugin entry point, descriptor, extensions (Params, State, GUI, Latency, Preset Discovery), factory wrappers, host harness, and DSP processor implementations.
//! - **Real-Time Safety (RT-Safety)**: Processing loops in [`clap::processor`] isolate all heap allocations and dynamic drops from the audio thread, communicating with the DAW host and GUI thread via SPSC lock-free channels and atomic status bitmasks.
//! - **GUI Infrastructure**: Integrated Slint 1.18-based control panel with direct atomic telemetry and non-blocking model/IR loading.

// Non-test production code must not rely on `unwrap`/`expect`: every fallible
// path returns a typed error, and RT-adjacent invariants are handled with
// explicit `let ... else`/`debug_assert!` instead of panics (F-PR-13). Modules
// compiled only for the `testing` feature are test support, not production.
#![cfg_attr(
    not(any(test, feature = "testing")),
    warn(clippy::unwrap_used, clippy::expect_used)
)]

pub mod clap;
pub mod receipt;

use clack_plugin::clack_export_entry;
clack_export_entry!(crate::clap::entry::NamEntry);
