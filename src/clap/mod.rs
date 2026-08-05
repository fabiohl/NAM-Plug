// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! # CLAP Plugin Subsystem
//!
//! This module provides the complete CLAP (CLever Audio Plug-in) standard integration for `NAM-Plug`.
//!
//! ## Submodules Overview
//!
//! - **[`descriptor`]**: Immutable plugin identity metadata (`br.eti.fabiolima.nam-plug`), features, vendor info.
//! - **[`entry`]**: Dynamic library entry point (`NamEntry`) registering plugin and preset discovery factories with the host.
//! - **[`extensions`]**: Implementation of standard CLAP extensions: Parameters, State Serialization, Audio Ports, GUI, Latency, Preset Loading, Render Mode, Tail, Track Info, and Remote Controls.
//! - **[`factory`]**: Preset discovery factory for querying local `.nam` model collections and presets.
//! - **[`plugin`]**: Plugin instance context (`NamClapPlugin`), shared thread state, and SPSC command scheduler.
//! - **[`processor`]**: Real-time DSP audio processor (`NamClapProcessor`), sample rendering loop, and SIMD activation processing.
//! - **[`gui`]**: Cross-platform `egui`-based GUI lifecycle and window rendering pipeline.
//! - **`host_harness` / `test_util`**: CLAP host mock harness and test utilities for integration testing (active under `testing` feature).

pub mod descriptor;
pub mod entry;
pub mod extensions;
pub mod factory;

pub mod plugin;
pub mod processor;

pub mod gui;

pub use plugin::NamClapPlugin;

#[cfg(test)]
#[path = "preset_discovery_test.rs"]
mod preset_discovery_test;

#[cfg(any(test, feature = "testing"))]
pub mod test_util;

#[cfg(any(test, feature = "testing"))]
pub mod host_harness;
