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
    /// The value is the one last reported to the host: `housekeeping()` samples
    /// the RT-published combined latency (`rt_to_ui.current_latency`) into
    /// `last_reported_latency` and immediately emits `HostLatency::changed()`
    /// when it advances. Reading the reported snapshot (instead of the raw
    /// cross-thread atomic) keeps `get()` consistent with the change
    /// notifications the host has actually received, and stays valid under the
    /// shared-reference (`&self`) main-thread contract.
    fn get(&self) -> u32 {
        self.last_reported_latency.get()
    }
}

/// Marker type for extension registration.
pub type NamPluginLatency = PluginLatency;
