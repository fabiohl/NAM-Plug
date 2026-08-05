// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Implementation of the `clap_plugin_param_indication` extension for NAM-rs.

use crate::clap::plugin::NamClapMainThread;
use clack_common::utils::{ClapId, Color};
use clack_extensions::param_indication::{
    ParamIndicationAutomation, PluginParamIndication, PluginParamIndicationImpl,
};
use std::ffi::CStr;
use std::sync::atomic::Ordering;

/// Indicates that the parameter has active mapping.
pub const INDICATION_MAPPED: u8 = 1 << 0;
/// Indicates that the parameter is being automated.
pub const INDICATION_AUTOMATING: u8 = 1 << 1;
/// Indicates that the parameter has an active automation override.
pub const INDICATION_OVERRIDING: u8 = 1 << 2;

impl<'a> PluginParamIndicationImpl for NamClapMainThread<'a> {
    /// Notifies the plugin that a parameter has (or does not have) active mapping in the host.
    ///
    /// Updates the `INDICATION_MAPPED` bit and stores the packed ARGB color
    /// in shared atomics for reading by the GUI thread.
    fn set_mapping(
        &mut self,
        param_id: ClapId,
        has_mapping: bool,
        color: Option<Color>,
        _label: Option<&CStr>,
        _description: Option<&CStr>,
    ) {
        let idx = param_id.get() as usize;
        if idx < self.shared.cold.param_indication.len() {
            let atomic_val = &self.shared.cold.param_indication[idx];
            if has_mapping {
                atomic_val.fetch_or(INDICATION_MAPPED, Ordering::Relaxed);
                if let Some(c) = color {
                    let packed = crate::clap::extensions::track_info::pack_argb(
                        c.alpha, c.red, c.green, c.blue,
                    );
                    self.shared.cold.param_indication_color[idx].store(packed, Ordering::Relaxed);
                } else {
                    self.shared.cold.param_indication_color[idx].store(0, Ordering::Relaxed);
                }
            } else {
                atomic_val.fetch_and(!INDICATION_MAPPED, Ordering::Relaxed);
                self.shared.cold.param_indication_color[idx].store(0, Ordering::Relaxed);
            }
        }
    }

    /// Notifies the plugin of the automation state of a parameter (None/Present/Playing/Recording/Overriding).
    ///
    /// Updates the `INDICATION_AUTOMATING` and `INDICATION_OVERRIDING` bits,
    /// ensuring mutual exclusion between both states. The color provided by the host
    /// is stored for use by the GUI when rendering automation halos.
    fn set_automation(
        &mut self,
        param_id: ClapId,
        automation_state: ParamIndicationAutomation,
        color: Option<Color>,
    ) {
        let idx = param_id.get() as usize;
        if idx < self.shared.cold.param_indication.len() {
            let atomic_val = &self.shared.cold.param_indication[idx];
            match automation_state {
                ParamIndicationAutomation::None | ParamIndicationAutomation::Present => {
                    atomic_val.fetch_and(
                        !(INDICATION_AUTOMATING | INDICATION_OVERRIDING),
                        Ordering::Relaxed,
                    );
                    self.shared.cold.param_indication_color[idx].store(0, Ordering::Relaxed);
                }
                ParamIndicationAutomation::Playing | ParamIndicationAutomation::Recording => {
                    atomic_val.fetch_or(INDICATION_AUTOMATING, Ordering::Relaxed);
                    atomic_val.fetch_and(!INDICATION_OVERRIDING, Ordering::Relaxed);
                    if let Some(c) = color {
                        let packed = crate::clap::extensions::track_info::pack_argb(
                            c.alpha, c.red, c.green, c.blue,
                        );
                        self.shared.cold.param_indication_color[idx]
                            .store(packed, Ordering::Relaxed);
                    }
                }
                ParamIndicationAutomation::Overriding => {
                    atomic_val.fetch_or(INDICATION_OVERRIDING, Ordering::Relaxed);
                    atomic_val.fetch_and(!INDICATION_AUTOMATING, Ordering::Relaxed);
                    if let Some(c) = color {
                        let packed = crate::clap::extensions::track_info::pack_argb(
                            c.alpha, c.red, c.green, c.blue,
                        );
                        self.shared.cold.param_indication_color[idx]
                            .store(packed, Ordering::Relaxed);
                    }
                }
            }
        }
    }
}

/// Marker type for extension registration.
pub type NamPluginParamIndication = PluginParamIndication;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU8;

    #[test]
    fn test_param_indication_bits() {
        let val = AtomicU8::new(0);

        // Test mappings
        val.fetch_or(INDICATION_MAPPED, Ordering::Relaxed);
        assert_eq!(
            val.load(Ordering::Relaxed) & INDICATION_MAPPED,
            INDICATION_MAPPED
        );

        val.fetch_and(!INDICATION_MAPPED, Ordering::Relaxed);
        assert_eq!(val.load(Ordering::Relaxed) & INDICATION_MAPPED, 0);

        // Test automation
        val.fetch_or(INDICATION_AUTOMATING, Ordering::Relaxed);
        assert_eq!(
            val.load(Ordering::Relaxed) & INDICATION_AUTOMATING,
            INDICATION_AUTOMATING
        );
        assert_eq!(val.load(Ordering::Relaxed) & INDICATION_OVERRIDING, 0);

        // Overriding overrides automating
        val.fetch_or(INDICATION_OVERRIDING, Ordering::Relaxed);
        val.fetch_and(!INDICATION_AUTOMATING, Ordering::Relaxed);
        assert_eq!(
            val.load(Ordering::Relaxed) & INDICATION_OVERRIDING,
            INDICATION_OVERRIDING
        );
        assert_eq!(val.load(Ordering::Relaxed) & INDICATION_AUTOMATING, 0);
    }

    /// Verify that when both AUTOMATING and OVERRIDING are set,
    /// the `None` or `Present` state correctly clears both.
    #[test]
    fn test_param_indication_none_clears_both_bits() {
        let val = AtomicU8::new(0);

        // Set both bits simultaneously
        val.fetch_or(
            INDICATION_AUTOMATING | INDICATION_OVERRIDING,
            Ordering::Relaxed,
        );
        assert_eq!(
            val.load(Ordering::Relaxed) & (INDICATION_AUTOMATING | INDICATION_OVERRIDING),
            INDICATION_AUTOMATING | INDICATION_OVERRIDING,
            "both bits should be set before the test"
        );

        // Simulate the effect of ParamIndicationAutomation::None (clears both)
        val.fetch_and(
            !(INDICATION_AUTOMATING | INDICATION_OVERRIDING),
            Ordering::Relaxed,
        );

        // Both should be zeroed
        assert_eq!(
            val.load(Ordering::Relaxed) & INDICATION_AUTOMATING,
            0,
            "AUTOMATING should be zeroed"
        );
        assert_eq!(
            val.load(Ordering::Relaxed) & INDICATION_OVERRIDING,
            0,
            "OVERRIDING should be zeroed"
        );

        // MAPPED should not be affected
        val.fetch_or(INDICATION_MAPPED, Ordering::Relaxed);
        val.fetch_and(
            !(INDICATION_AUTOMATING | INDICATION_OVERRIDING),
            Ordering::Relaxed,
        );
        assert_eq!(
            val.load(Ordering::Relaxed) & INDICATION_MAPPED,
            INDICATION_MAPPED,
            "MAPPED should not be affected by automation clearing"
        );
    }
}
