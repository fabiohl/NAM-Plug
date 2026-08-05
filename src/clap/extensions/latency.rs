// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Implementation of the `clap_plugin_latency` extension for NAM-rs.

use crate::clap::plugin::NamClapMainThread;
use clack_extensions::latency::{PluginLatency, PluginLatencyImpl};
use std::sync::atomic::Ordering;

/// Implementation of the `PluginLatencyImpl` trait for the NAM-rs plugin.
/// The trait is implemented on `MainThread`.
impl<'a> PluginLatencyImpl for NamClapMainThread<'a> {
    /// Returns the current plugin latency in samples.
    ///
    /// The value is read from the shared state, which is updated by `NamClapProcessor`
    /// whenever the algorithmic latency changes (e.g., model activation or swap).
    fn get(&mut self) -> u32 {
        self.shared.rt_to_ui.current_latency.load(Ordering::Relaxed)
    }
}

/// Marker type for extension registration.
pub type NamPluginLatency = PluginLatency;
