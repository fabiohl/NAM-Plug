// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Integration tests for Preset Discovery Factory and Preset Load extension.
#[cfg(test)]
mod tests {
    use crate::clap::entry::NamEntry;
    use crate::clap::test_util::{TestHost, TestHostShared};
    use clack_extensions::preset_discovery::factory::PresetDiscoveryFactory;
    use clack_host::prelude::*;

    #[test]
    fn test_entry_exposes_plugin_factory() {
        let entry =
            PluginEntry::load_from_clack::<NamEntry>(c"/test").expect("Failed to load PluginEntry");

        let plugin_factory = entry.get_plugin_factory();
        assert!(
            plugin_factory.is_some(),
            "Entry should expose PluginFactory"
        );

        let pf = plugin_factory.unwrap();
        assert_eq!(pf.plugin_count(), 1, "Should have exactly 1 plugin");
        let desc = pf
            .plugin_descriptor(0)
            .expect("Should have plugin descriptor at index 0");
        assert_eq!(
            desc.id().unwrap().to_str().unwrap(),
            "br.eti.fabiolima.nam-plug",
            "Plugin ID should match"
        );
    }

    #[test]
    fn test_entry_exposes_preset_discovery_factory() {
        let entry =
            PluginEntry::load_from_clack::<NamEntry>(c"/test").expect("Failed to load PluginEntry");

        let preset_factory = entry.get_factory::<PresetDiscoveryFactory>();
        assert!(
            preset_factory.is_some(),
            "Entry should expose PresetDiscoveryFactory"
        );

        let factory = preset_factory.unwrap();
        assert_eq!(
            factory.provider_count(),
            1,
            "Should have exactly 1 provider"
        );

        let desc = factory
            .get_provider_descriptor(0)
            .expect("Should have provider descriptor at index 0");
        assert!(desc.id().is_some(), "Provider descriptor should have an ID");
        assert!(
            desc.name().is_some(),
            "Provider descriptor should have a name"
        );
    }

    #[test]
    fn test_entry_exposes_both_factories() {
        let entry =
            PluginEntry::load_from_clack::<NamEntry>(c"/test").expect("Failed to load PluginEntry");

        assert!(
            entry.get_plugin_factory().is_some(),
            "Plugin factory must be available"
        );
        assert!(
            entry.get_factory::<PresetDiscoveryFactory>().is_some(),
            "Preset discovery factory must be available"
        );
    }

    #[test]
    fn test_plugin_instance_works_with_custom_entry() {
        let entry =
            PluginEntry::load_from_clack::<NamEntry>(c"/test").expect("Failed to load PluginEntry");

        let host_info = HostInfo::new(
            "NAM-rs-Test",
            "NAM",
            "https://github.com/fabiohl/nam-rs",
            "0.1.0",
        )
        .expect("Failed to create HostInfo");

        let mut plugin_instance = PluginInstance::<TestHost>::new(
            |_| TestHostShared {
                restart_was_called: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
            |_| (),
            &entry,
            c"br.eti.fabiolima.nam-plug",
            &host_info,
        )
        .expect("Failed to instantiate plugin");

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 512,
            max_frames_count: 512,
        };

        let stopped_processor = plugin_instance
            .activate(|_, _| (), audio_config)
            .expect("Failed to activate plugin");
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let mut input = [0.1f32; 512];
        let mut output = [0.0f32; 512];
        let mut input_ports = AudioPorts::with_capacity(1, 1);
        let mut output_ports = AudioPorts::with_capacity(1, 1);
        let mut input_channels = [input.as_mut_slice()];
        let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(
                input_channels.iter_mut().map(InputChannel::constant),
            ),
        }]);
        let output_channels = [output.as_mut_slice()];
        let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
        }]);
        let input_events =
            InputEvents::from_buffer::<[clack_host::events::event_types::NoteOnEvent; 0]>(&[]);
        let mut output_events_buffer = EventBuffer::new();
        let mut output_events = OutputEvents::from_buffer(&mut output_events_buffer);

        started_processor
            .process(
                &input_audio,
                &mut output_audio,
                &input_events,
                &mut output_events,
                None,
                None,
            )
            .expect("Process should succeed");

        assert!(
            output.iter().any(|s| *s != 0.0),
            "Output should not be silent (bypass is off by default)"
        );
    }
}
