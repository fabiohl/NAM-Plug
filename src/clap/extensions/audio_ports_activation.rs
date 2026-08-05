// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Implementation of the audio-ports-activation CLAP extension for NAM-rs.
//!
//! Allows the host to deactivate the R channel on mono tracks, eliminating
//! mono-detection heuristics and saving half of the gain/copy processing.

use crate::clap::plugin::NamClapMainThread;
use crate::clap::processor::NamClapProcessor;
use clack_extensions::audio_ports_activation::{
    PluginAudioPortsActivation, PluginAudioPortsActivationImpl, PluginAudioPortsActivationSetImpl,
    SampleSize,
};
use std::sync::atomic::Ordering;

/// Marker type for extension registration.
pub type NamPluginAudioPortsActivation = PluginAudioPortsActivation;

// ---------------------------------------------------------------------------
// Main Thread implementation (plugin inactive)
// ---------------------------------------------------------------------------

impl PluginAudioPortsActivationImpl for NamClapMainThread<'_> {
    fn can_activate_while_processing(&mut self) -> bool {
        true
    }
}

impl PluginAudioPortsActivationSetImpl for NamClapMainThread<'_> {
    fn set_active(
        &mut self,
        is_input: bool,
        port_index: u32,
        is_active: bool,
        _sample_size: SampleSize,
    ) -> bool {
        if port_index != 0 {
            return false;
        }
        if is_input {
            self.shared
                .ui_to_rt
                .host_r_deactivated
                .store(!is_active, Ordering::Release);
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Audio Thread implementation (plugin active)
// ---------------------------------------------------------------------------

impl PluginAudioPortsActivationSetImpl for NamClapProcessor<'_> {
    fn set_active(
        &mut self,
        is_input: bool,
        port_index: u32,
        is_active: bool,
        _sample_size: SampleSize,
    ) -> bool {
        if port_index != 0 {
            return false;
        }
        if is_input {
            self.shared
                .ui_to_rt
                .host_r_deactivated
                .store(!is_active, Ordering::Release);
        }
        true
    }
}
