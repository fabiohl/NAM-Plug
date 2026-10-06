// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Implementation of the `clap_plugin_timer_support` extension for NAM-Plug.
//!
//! Provides a periodic main-thread watchdog timer driven by the CLAP host,
//! ensuring that `housekeeping()` runs regularly even when GUI or user events are absent.

use crate::clap::plugin::NamClapMainThread;
use clack_extensions::timer::{PluginTimer, PluginTimerImpl, TimerId};
use std::sync::atomic::Ordering;

/// Implementation of the `PluginTimerImpl` trait for the NAM-Plug plugin.
/// The trait is implemented on `NamClapMainThread`.
impl<'a> PluginTimerImpl for NamClapMainThread<'a> {
    /// Host-driven periodic timer callback executing on the CLAP main thread.
    ///
    /// Guarded by `alive_fence` to prevent executing housekeeping during or after teardown.
    fn on_timer(&self, _timer_id: TimerId) {
        if self.shared.cold.alive_fence.load(Ordering::Relaxed) {
            self.housekeeping();
            self.emit_pending_logs();
        }
    }
}

/// Marker type for extension registration.
pub type NamPluginTimer = PluginTimer;
