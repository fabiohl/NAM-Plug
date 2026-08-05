// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Benchmarks of CLAP host integration — the `process` call path through the full
//! plugin stack (NamClapPlugin).
//!
//! Covers two paths:
//!
//! ## Running
//!
//! ```sh
//! cargo bench --features clap-plugin --bench clap_bench
//! ```

mod common;
use common::generate_sine_440hz;
use criterion::{Criterion, criterion_group, criterion_main};

struct BenchHostShared;
impl clack_host::prelude::SharedHandler<'_> for BenchHostShared {
    fn request_restart(&self) {}
    fn request_process(&self) {}
    fn request_callback(&self) {}
}

struct BenchHost;
impl clack_host::prelude::HostHandlers for BenchHost {
    type Shared<'a> = BenchHostShared;
    type MainThread<'a> = ();
    type AudioProcessor<'a> = ();
}

fn bench_clap_process_block_64samp(c: &mut Criterion) {
    use clack_common::events::Pckn;
    use clack_common::events::event_types::ParamValueEvent;
    use clack_common::utils::{ClapId, Cookie};
    use clack_host::prelude::*;
    use nam_plug::clap::extensions::params::{PARAM_INPUT_GAIN, PARAM_OUTPUT_GAIN};
    use nam_plug::clap::plugin::NamClapPlugin;

    let entry =
        PluginEntry::load_from_clack::<clack_plugin::entry::SinglePluginEntry<NamClapPlugin>>(
            c"/bench",
        )
        .expect("Failed to load PluginEntry");

    let host_info = HostInfo::new(
        "NAM-rs Bench Host",
        "Fabio Lima",
        "https://github.com/fabiohl/nam-rs",
        "0.1.0",
    )
    .unwrap();

    let mut plugin_instance = PluginInstance::<BenchHost>::new(
        |_| BenchHostShared,
        |_| (),
        &entry,
        c"br.eti.fabiolima.nam-plug",
        &host_info,
    )
    .expect("Failed to create plugin instance");

    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 64,
        max_frames_count: 64,
    };

    let stopped_processor = plugin_instance
        .activate(|_, _| (), audio_config)
        .expect("Failed to activate plugin");

    let mut started_processor = stopped_processor
        .start_processing()
        .expect("Failed to start processing");

    let sine = generate_sine_440hz(64);
    let mut input_audio_buffers = [sine.clone(), sine.clone()];
    let mut output_audio_buffers = [[0.0f32; 64], [0.0f32; 64]];

    let mut input_ports = AudioPorts::with_capacity(2, 1);
    let mut output_ports = AudioPorts::with_capacity(2, 1);

    let mut output_events_buffer = EventBuffer::with_capacity(10);

    let mut group = c.benchmark_group("CLAP_process_block_64samp");

    group.bench_function("SIMD_FastPath", |b| {
        b.iter(|| {
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
                    output_audio_buffers
                        .iter_mut()
                        .map(|buf| buf.as_mut_slice()),
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

            std::hint::black_box(status);
        });
    });

    let mut input_events_buffer = EventBuffer::new();
    let event_in = ParamValueEvent::new(
        0,
        ClapId::new(PARAM_INPUT_GAIN),
        Pckn::match_all(),
        0.0,
        Cookie::empty(),
    );
    let event_out = ParamValueEvent::new(
        0,
        ClapId::new(PARAM_OUTPUT_GAIN),
        Pckn::match_all(),
        0.0,
        Cookie::empty(),
    );
    input_events_buffer.push(&event_in);
    input_events_buffer.push(&event_out);
    let input_events = InputEvents::from_buffer(&input_events_buffer);

    group.bench_function("Scalar_SlowPath", |b| {
        b.iter(|| {
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
                    output_audio_buffers
                        .iter_mut()
                        .map(|buf| buf.as_mut_slice()),
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

            std::hint::black_box(status);
        });
    });

    group.finish();

    let stopped_processor = started_processor.stop_processing();
    plugin_instance.deactivate(stopped_processor);
}

criterion_group! {
    name = clap_benches;
    config = criterion::Criterion::default().sample_size(50);
    targets = bench_clap_process_block_64samp
}

criterion_main!(clap_benches);
