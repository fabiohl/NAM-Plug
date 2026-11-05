// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Implementation of the audio ports and audio ports configuration CLAP extensions for NAM-Plug.

use crate::clap::plugin::NamClapMainThread;
use crate::clap::plugin::shared::{CONFIG_ID_MONO, CONFIG_ID_STEREO};
use clack_extensions::audio_ports::{
    AudioPortFlags, AudioPortInfo, AudioPortInfoWriter, AudioPortType, PluginAudioPortsImpl,
};
use clack_extensions::audio_ports_config::{
    AudioPortConfigWriter, AudioPortsConfiguration, MainPortInfo, PluginAudioPortsConfigImpl,
    PluginAudioPortsConfigInfoImpl,
};
use clack_plugin::prelude::{ClapId, PluginError};
use std::sync::atomic::Ordering;

// ---------------------------------------------------------------------------
// Audio Ports extension
// ---------------------------------------------------------------------------

impl PluginAudioPortsImpl for NamClapMainThread<'_> {
    /// Returns the number of audio ports (input or output).
    ///
    /// NAM-Plug exposes exactly 1 main audio port (either Mono or Stereo).
    fn count(&self, _is_input: bool) -> u32 {
        1
    }

    /// Fills in the audio port info at the specified index for the active configuration.
    fn get(&self, index: u32, is_input: bool, writer: &mut AudioPortInfoWriter) {
        if index == 0 {
            let is_stereo = self
                .shared
                .cold
                .selected_port_config
                .load(Ordering::Acquire)
                == CONFIG_ID_STEREO;
            writer.set(&AudioPortInfo {
                id: ClapId::new(0),
                name: if is_input {
                    b"Main Input"
                } else {
                    b"Main Output"
                },
                channel_count: if is_stereo { 2 } else { 1 },
                flags: AudioPortFlags::IS_MAIN,
                port_type: Some(if is_stereo {
                    AudioPortType::STEREO
                } else {
                    AudioPortType::MONO
                }),
                in_place_pair: Some(ClapId::new(0)),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Audio Ports Config extension
// ---------------------------------------------------------------------------

impl PluginAudioPortsConfigImpl for NamClapMainThread<'_> {
    /// Returns the number of available audio port configurations:
    /// - 0: Mono (1 input [1 ch], 1 output [1 ch])
    /// - 1: Stereo / Dual-Mono (1 input [2 ch], 1 output [2 ch]) [only with dual-mono feature]
    fn count(&self) -> u32 {
        #[cfg(feature = "dual-mono")]
        {
            2
        }
        #[cfg(not(feature = "dual-mono"))]
        {
            1
        }
    }

    fn get(&self, index: u32, writer: &mut AudioPortConfigWriter) {
        match index {
            0 => {
                writer.write(&AudioPortsConfiguration {
                    id: ClapId::new(CONFIG_ID_MONO),
                    name: b"Mono",
                    input_port_count: 1,
                    output_port_count: 1,
                    main_input: Some(MainPortInfo {
                        channel_count: 1,
                        port_type: Some(AudioPortType::MONO),
                    }),
                    main_output: Some(MainPortInfo {
                        channel_count: 1,
                        port_type: Some(AudioPortType::MONO),
                    }),
                });
            }
            #[cfg(feature = "dual-mono")]
            1 => {
                writer.write(&AudioPortsConfiguration {
                    id: ClapId::new(CONFIG_ID_STEREO),
                    name: b"Stereo",
                    input_port_count: 1,
                    output_port_count: 1,
                    main_input: Some(MainPortInfo {
                        channel_count: 2,
                        port_type: Some(AudioPortType::STEREO),
                    }),
                    main_output: Some(MainPortInfo {
                        channel_count: 2,
                        port_type: Some(AudioPortType::STEREO),
                    }),
                });
            }
            _ => {}
        }
    }

    fn select(&self, config_id: ClapId) -> Result<(), PluginError> {
        if self.shared.cold.is_processing.load(Ordering::Acquire) {
            return Err(PluginError::Message(
                "Audio ports configuration can only be changed while plugin is deactivated",
            ));
        }

        match config_id.get() {
            CONFIG_ID_MONO => {
                self.shared
                    .cold
                    .selected_port_config
                    .store(CONFIG_ID_MONO, Ordering::Release);
                Ok(())
            }
            #[cfg(feature = "dual-mono")]
            CONFIG_ID_STEREO => {
                self.shared
                    .cold
                    .selected_port_config
                    .store(CONFIG_ID_STEREO, Ordering::Release);
                Ok(())
            }
            #[cfg(not(feature = "dual-mono"))]
            CONFIG_ID_STEREO => Err(PluginError::Message(
                "Stereo audio ports configuration requires the 'dual-mono' feature",
            )),
            _ => Err(PluginError::Message("Unknown audio ports configuration ID")),
        }
    }
}

// ---------------------------------------------------------------------------
// Audio Ports Config Info extension
// ---------------------------------------------------------------------------

impl PluginAudioPortsConfigInfoImpl for NamClapMainThread<'_> {
    fn current_config(&self) -> Option<ClapId> {
        Some(ClapId::new(
            self.shared
                .cold
                .selected_port_config
                .load(Ordering::Acquire),
        ))
    }

    fn get(&self, config_id: ClapId, index: u32, is_input: bool, writer: &mut AudioPortInfoWriter) {
        if index == 0 {
            let is_stereo = config_id.get() == CONFIG_ID_STEREO;
            writer.set(&AudioPortInfo {
                id: ClapId::new(0),
                name: if is_input {
                    b"Main Input"
                } else {
                    b"Main Output"
                },
                channel_count: if is_stereo { 2 } else { 1 },
                flags: AudioPortFlags::IS_MAIN,
                port_type: Some(if is_stereo {
                    AudioPortType::STEREO
                } else {
                    AudioPortType::MONO
                }),
                in_place_pair: Some(ClapId::new(0)),
            });
        }
    }
}

#[cfg(test)]
#[path = "audio_ports_test.rs"]
mod audio_ports_test;
