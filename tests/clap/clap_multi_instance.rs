// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! S8-E8-T02: Multi-instance tests now load the freshly built `.so`
//! instead of using the static `PluginEntry::load_from_clack()` path.
//! SHA256 of the tested binary is recorded in the test log.

use clack_host::prelude::*;
#[cfg(feature = "heap-audit")]
use neural_amp_modeler_rs::common::alloc_audit::{TrackingGuard, get_alloc_count};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

struct MultiHostShared {
    _restart_was_called: Arc<AtomicBool>,
}
impl<'a> SharedHandler<'a> for MultiHostShared {
    fn request_restart(&self) {}
    fn request_process(&self) {}
    fn request_callback(&self) {}
}

struct MultiHost;
impl HostHandlers for MultiHost {
    type Shared<'a> = MultiHostShared;
    type MainThread<'a> = ();
    type AudioProcessor<'a> = ();
}

const INSTANCE_COUNT: usize = 10;
const BLOCK_SIZE: usize = 256;

fn process_block(
    started_processor: &mut StartedPluginAudioProcessor<MultiHost>,
    input_audio_buf: &mut [[f32; BLOCK_SIZE]; 2],
    output_audio_buf: &mut [[f32; BLOCK_SIZE]; 2],
    events_buf: &mut EventBuffer,
) {
    let mut input_ports = AudioPorts::with_capacity(2, 1);
    let mut output_ports = AudioPorts::with_capacity(2, 1);

    let input_events = InputEvents::empty();
    let mut output_events = OutputEvents::from_buffer(events_buf);

    let mut input_audio_buffers = [input_audio_buf[0], input_audio_buf[1]];
    let mut output_audio_buffers = [output_audio_buf[0], output_audio_buf[1]];

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
        .expect("process() failed");

    assert!(
        status == ProcessStatus::Continue || status == ProcessStatus::Sleep,
        "Unexpected process status: {:?}",
        status
    );
}

#[test]
#[ignore = "long-running stress test"]
fn test_multi_instance_rt_priority() {
    let artifact = super::artifact_validator::TestedArtifact::resolve_and_hash();

    let host_info = HostInfo::new(
        "NAM-rs Multi-Instance Stress Test",
        "NAM-rs",
        "https://github.com/fabiohl/nam-rs",
        "0.1.0",
    )
    .expect("Failed to create HostInfo");

    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: BLOCK_SIZE as u32,
        max_frames_count: BLOCK_SIZE as u32,
    };

    let mut instances: Vec<PluginInstance<MultiHost>> = Vec::with_capacity(INSTANCE_COUNT);
    let mut shared_refs: Vec<*const nam_plug::clap::plugin::NamClapShared> =
        Vec::with_capacity(INSTANCE_COUNT);

    for _i in 0..INSTANCE_COUNT {
        // SAFETY: Dynamic plugin loading.
        let entry =
            unsafe { PluginEntry::load(&artifact.path).expect("Failed to load plugin entry") };

        let mut plugin_instance = PluginInstance::<MultiHost>::new(
            |_| MultiHostShared {
                _restart_was_called: Arc::new(AtomicBool::new(false)),
            },
            |_| (),
            &entry,
            c"br.eti.fabiolima.nam-plug",
            &host_info,
        )
        .expect("Failed to create plugin instance");

        let raw_plugin_ptr = plugin_instance.plugin_handle().as_raw_ptr();
        let shared_ptr = unsafe {
            clack_plugin::extensions::wrapper::PluginWrapper::<
                nam_plug::clap::plugin::NamClapPlugin,
            >::handle(
                raw_plugin_ptr,
                |wrapper| Ok(wrapper.shared() as *const nam_plug::clap::plugin::NamClapShared),
            )
            .expect("Failed to obtain plugin wrapper")
        };
        shared_refs.push(shared_ptr);

        let stopped_processor = plugin_instance
            .activate(|_, _| (), audio_config)
            .expect("Failed to activate plugin");

        let mut started_processor = stopped_processor
            .start_processing()
            .expect("Failed to start processing");

        let mut input_buf = [[0.1f32; BLOCK_SIZE]; 2];
        let mut output_buf = [[0.0f32; BLOCK_SIZE]; 2];
        let mut events_buf = EventBuffer::with_capacity(10);

        for _block in 0..3 {
            process_block(
                &mut started_processor,
                &mut input_buf,
                &mut output_buf,
                &mut events_buf,
            );
        }

        let stopped_processor = started_processor.stop_processing();
        plugin_instance.deactivate(stopped_processor);

        instances.push(plugin_instance);
    }

    for (i, shared_ptr) in shared_refs.iter().enumerate() {
        let shared = unsafe { &**shared_ptr };
        let priority = shared.cold.rt_status.rt_priority.load(Ordering::Relaxed);
        assert_ne!(
            priority, -1,
            "Instance {}: rt_priority is still at sentinel value (-1). \
             Each instance must independently detect its thread priority.",
            i
        );
    }

    drop(instances);
}

/// S5-E5-T04: Multi-instance parallel stress test with heap audit.
///
/// S8-E8-T02: Loads the freshly built `.so` in each parallel thread
/// instead of the static `load_from_clack()` path.
#[cfg(feature = "heap-audit")]
#[test]
fn test_multi_instance_heap_audit_stress() {
    let artifact = super::artifact_validator::TestedArtifact::resolve_and_hash();
    let artifact_path = artifact.path.clone();

    const INSTANCE_COUNT: usize = 16;
    const BLOCK_COUNT: usize = 10;
    const BLOCK_SIZE: usize = 256;

    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: BLOCK_SIZE as u32,
        max_frames_count: BLOCK_SIZE as u32,
    };

    let handles: Vec<_> = (0..INSTANCE_COUNT)
        .map(|i| {
            let path = artifact_path.clone();
            std::thread::spawn(move || {
                // SAFETY: Dynamic plugin loading.
                let entry =
                    unsafe { PluginEntry::load(&path).expect("Failed to load plugin entry") };

                let host_info = HostInfo::new(
                    "NAM-rs Stress",
                    "NAM-rs",
                    "https://github.com/fabiohl/nam-rs",
                    "0.1.0",
                )
                .expect("Failed to create HostInfo");

                let mut plugin_instance = PluginInstance::<MultiHost>::new(
                    |_| MultiHostShared {
                        _restart_was_called: Arc::new(AtomicBool::new(false)),
                    },
                    |_| (),
                    &entry,
                    c"br.eti.fabiolima.nam-plug",
                    &host_info,
                )
                .expect("Failed to create plugin instance");

                let stopped = plugin_instance
                    .activate(|_, _| (), audio_config)
                    .expect("Failed to activate plugin");

                let mut started = stopped
                    .start_processing()
                    .expect("Failed to start processing");

                let input_buf = [[0.1f32; BLOCK_SIZE]; 2];
                let output_buf = [[0.0f32; BLOCK_SIZE]; 2];

                let mut input_ports = AudioPorts::with_capacity(2, 1);
                let mut output_ports = AudioPorts::with_capacity(2, 1);
                let mut events_buf = EventBuffer::with_capacity(10);
                let input_events = InputEvents::empty();

                let mut input_audio_buf1 = input_buf[0];
                let mut input_audio_buf2 = input_buf[1];
                let mut output_audio_buf1 = output_buf[0];
                let mut output_audio_buf2 = output_buf[1];

                let mut total_allocs: usize = 0;
                for _block in 0..BLOCK_COUNT {
                    let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
                        latency: 0,
                        channels: AudioPortBufferType::f32_input_only(
                            [&mut input_audio_buf1, &mut input_audio_buf2]
                                .into_iter()
                                .map(InputChannel::constant),
                        ),
                    }]);

                    let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
                        latency: 0,
                        channels: AudioPortBufferType::f32_output_only(
                            [&mut output_audio_buf1, &mut output_audio_buf2]
                                .into_iter()
                                .map(|b| b.as_mut_slice()),
                        ),
                    }]);

                    let mut output_events = OutputEvents::from_buffer(&mut events_buf);

                    let _guard = TrackingGuard::new();
                    let status = started
                        .process(
                            &input_audio,
                            &mut output_audio,
                            &input_events,
                            &mut output_events,
                            None,
                            None,
                        )
                        .expect("process() failed");
                    let allocs = get_alloc_count();
                    total_allocs += allocs;

                    assert_eq!(
                        allocs, 0,
                        "[instance {}] heap allocation ({}) detected during process() block {}",
                        i, allocs, _block,
                    );

                    assert!(
                        status == ProcessStatus::Continue || status == ProcessStatus::Sleep,
                        "[instance {}] unexpected process status: {:?}",
                        i,
                        status
                    );
                }

                let stopped = started.stop_processing();
                plugin_instance.deactivate(stopped);

                assert_eq!(
                    total_allocs, 0,
                    "[instance {}] total heap allocations across {} blocks: {} — expected 0",
                    i, BLOCK_COUNT, total_allocs
                );

                drop(plugin_instance);
            })
        })
        .collect();

    for (i, h) in handles.into_iter().enumerate() {
        h.join()
            .unwrap_or_else(|_| panic!("worker thread {} panicked", i));
    }
}
