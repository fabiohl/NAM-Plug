// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Implementation of the `clap_plugin_latency` extension for NAM-Plug.

use crate::clap::plugin::NamClapMainThread;
use clack_extensions::latency::{PluginLatency, PluginLatencyImpl};

/// Implementation of the `PluginLatencyImpl` trait for the NAM-Plug plugin.
/// The trait is implemented on `MainThread`.
impl<'a> PluginLatencyImpl for NamClapMainThread<'a> {
    /// Returns the current plugin latency in samples.
    ///
    /// The value is synchronized inside `NamClapProcessor::activate()` alongside
    /// `HostLatency::changed()` (per the CLAP 1.2.2 requirement that latency only
    /// changes during `clap_plugin::activate()`). Reading the reported snapshot
    /// (instead of the raw cross-thread atomic) keeps `get()` consistent with the
    /// change notifications the host has actually received, and stays valid under
    /// the shared-reference (`&self`) main-thread contract.
    fn get(&self) -> u32 {
        self.last_reported_latency.get()
    }
}

/// Marker type for extension registration.
pub type NamPluginLatency = PluginLatency;
