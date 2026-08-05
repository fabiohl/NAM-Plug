// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Implementation of the audio ports CLAP extension for NAM-rs.

use crate::clap::plugin::NamClapMainThread;
use clack_extensions::audio_ports::{
    AudioPortFlags, AudioPortInfo, AudioPortInfoWriter, AudioPortType, PluginAudioPortsImpl,
};
use clack_plugin::prelude::ClapId;

impl PluginAudioPortsImpl for NamClapMainThread<'_> {
    /// Returns the number of audio ports (input or output).
    ///
    /// NAM-rs is a native mono plugin by definition with exactly 1 input and 1 output port.
    fn count(&mut self, _is_input: bool) -> u32 {
        1
    }

    /// Fills in the audio port info at the specified index.
    ///
    /// Configures a **mono** port (1 channel) with in-place pair enabled
    /// (allows the host to use the same buffer for input and output).
    ///
    /// The port is declared as mono because NAM is native mono by definition.
    /// To respect traditional DAW workflows, the plugin works strictly as mono.
    /// The DAW decides how to connect the plugin (e.g., managing stereo channel routing externally),
    /// while stereo support is provided exclusively as a convenience in standalone mode.
    fn get(&mut self, index: u32, is_input: bool, writer: &mut AudioPortInfoWriter) {
        if index == 0 {
            writer.set(&AudioPortInfo {
                id: ClapId::new(0),
                name: if is_input {
                    b"Main Input"
                } else {
                    b"Main Output"
                },
                channel_count: 1,
                flags: AudioPortFlags::IS_MAIN,
                port_type: Some(AudioPortType::MONO),
                in_place_pair: Some(ClapId::new(0)),
            });
        }
    }
}
