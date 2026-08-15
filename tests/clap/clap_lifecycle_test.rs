// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Lifecycle test loads the freshly built `.so` via
//! `artifact_validator::TestedArtifact::resolve_and_hash()`, recording
//! the SHA256 of the tested binary in the test log.
//!
//! This eliminates the false confidence from loading a stale install
//! at `~/.clap/nam-rs.clap` when no `CLAP_PLUGIN_PATH` is set.

use clack_host::prelude::*;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

struct MyHostShared {
    _restart_was_called: Arc<AtomicBool>,
}
impl SharedHandler<'_> for MyHostShared {
    fn request_restart(&self) {}
    fn request_process(&self) {}
    fn request_callback(&self) {}
}

struct MyHost;
impl HostHandlers for MyHost {
    type Shared<'a> = MyHostShared;
    type MainThread<'a> = ();
    type AudioProcessor<'a> = ();
}

#[test]
fn test_clap_lifecycle() {
    let artifact = super::artifact_validator::TestedArtifact::resolve_and_hash();

    // SAFETY: Loading a CLAP plugin is inherently unsafe because it executes code from a dynamic library.
    let entry = unsafe { PluginEntry::load(&artifact.path).expect("Failed to load plugin entry") };
    let host_info = HostInfo::new(
        "NAM-rs Test Host",
        "Fabio Lima",
        "https://github.com/fabiohl/nam-rs",
        "0.1.0",
    )
    .unwrap();

    let mut plugin_instance = PluginInstance::<MyHost>::new(
        |_| MyHostShared {
            _restart_was_called: Arc::new(AtomicBool::new(false)),
        },
        |_| (),
        &entry,
        c"br.eti.fabiolima.nam-plug",
        &host_info,
    )
    .expect("Failed to create plugin instance");

    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 512,
        max_frames_count: 512,
    };

    let stopped_processor = plugin_instance
        .activate(|_, _| (), audio_config)
        .expect("Failed to activate plugin");

    let mut started_processor = stopped_processor
        .start_processing()
        .expect("Failed to start processing");

    let mut input_audio_buffers = [[0.0f32; 512]; 2];
    let mut output_audio_buffers = [[0.0f32; 512]; 2];

    let mut input_ports = AudioPorts::with_capacity(2, 1);
    let mut output_ports = AudioPorts::with_capacity(2, 1);

    let mut output_events_buffer = EventBuffer::with_capacity(10);

    for _ in 0..4 {
        let input_events = InputEvents::empty();
        let mut output_events = OutputEvents::from_buffer(&mut output_events_buffer);

        let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(
                input_audio_buffers.iter_mut().map(InputChannel::constant),
            ),
        }]);

        let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(
                output_audio_buffers.iter_mut().map(|b| b.as_mut_slice()),
            ),
        }]);

        let status = started_processor
            .process(
                &input_audio,
                &mut output_audio,
                &input_events,
                &mut output_events,
                None,
                None,
            )
            .expect("Process failed");

        assert!(
            status == ProcessStatus::Continue || status == ProcessStatus::Sleep,
            "Unexpected process status: {:?}",
            status
        );
    }

    let stopped_processor = started_processor.stop_processing();
    plugin_instance.deactivate(stopped_processor);
}
