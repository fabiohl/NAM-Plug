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
//! - **GUI Infrastructure**: Integrated `egui`-based control panel with direct atomic telemetry and non-blocking model/IR loading.

pub mod clap;

use clack_plugin::clack_export_entry;
clack_export_entry!(crate::clap::entry::NamEntry);
