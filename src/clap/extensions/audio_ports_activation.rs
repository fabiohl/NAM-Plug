// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Implementation of the audio-ports-activation CLAP extension for NAM-Plug.
//!
//! Allows the host to activate and deactivate audio ports (port 0) and signals
//! buffer sample size capability. NAM-Plug exclusively processes 32-bit float (`f32`)
//! audio buffers, rejecting `SampleSize::Float64` per CLAP specification (D-13).

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
    fn can_activate_while_processing(&self) -> bool {
        true
    }
}

impl PluginAudioPortsActivationSetImpl for NamClapMainThread<'_> {
    fn set_active(
        &self,
        is_input: bool,
        port_index: u32,
        is_active: bool,
        sample_size: SampleSize,
    ) -> bool {
        if port_index != 0 {
            return false;
        }
        if sample_size == SampleSize::Float64 {
            // NAM-Plug only processes 32-bit float buffers; reject Float64 (D-13).
            return false;
        }
        if is_input {
            self.shared
                .cold
                .port_input_active
                .store(is_active, Ordering::Release);
        } else {
            self.shared
                .cold
                .port_output_active
                .store(is_active, Ordering::Release);
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Audio Thread implementation (plugin active)
// ---------------------------------------------------------------------------

impl PluginAudioPortsActivationSetImpl for NamClapProcessor<'_> {
    fn set_active(
        &self,
        is_input: bool,
        port_index: u32,
        is_active: bool,
        sample_size: SampleSize,
    ) -> bool {
        if port_index != 0 {
            return false;
        }
        if sample_size == SampleSize::Float64 {
            // NAM-Plug only processes 32-bit float buffers; reject Float64 (D-13).
            return false;
        }
        if is_input {
            self.shared
                .cold
                .port_input_active
                .store(is_active, Ordering::Release);
        } else {
            self.shared
                .cold
                .port_output_active
                .store(is_active, Ordering::Release);
        }
        true
    }
}
