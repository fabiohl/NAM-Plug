// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Benchmarks of CLAP host integration — the `process` call path through the full
//! plugin stack (NamClapPlugin).
//!
//! Structured into two distinct layers (F-BENCH-013):
//! 1. `CLAP_Infrastructure`: Isolated host wrapper, buffer routing, event dispatch, and bypass overhead.
//! 2. `CLAP_Inference`: Real neural network inference through the complete CLAP plugin stack
//!    with authentic models (WaveNet A1 Standard, WaveNet A2 / Slimmable, LSTM), sample rate
//!    variations (44.1k, 48k, 96k), oversampling factors (Off, 2x, 4x), offline render mode, and CabSim IR.
//!
//! ## Running
//!
//! ```sh
//! cargo bench --features testing --bench clap_bench
//! ```

mod common;
use common::generate_sine_440hz;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

use clack_common::events::Pckn;
use clack_common::events::event_types::ParamValueEvent;
use clack_common::utils::{ClapId, Cookie};
use clack_extensions::render::{PluginRender, RenderMode};
use clack_extensions::state::PluginState;
use clack_host::prelude::*;
use nam_plug::clap::extensions::params::{PARAM_INPUT_GAIN, PARAM_OUTPUT_GAIN};
use nam_plug::clap::plugin::NamClapPlugin;
use nam_plug::clap::test_util::asset_hash;
use neural_amp_modeler_rs::common::params::ProcessingParams;
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;

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

/// Helper to create and activate a `NamClapPlugin` instance configured with the specified
/// sample rate, block size, processing parameters, and optional render mode.
fn create_and_activate_bench_plugin(
    sample_rate: f64,
    block_size: usize,
    params: &ProcessingParams,
    render_mode: Option<RenderMode>,
) -> (
    PluginInstance<BenchHost>,
    StoppedPluginAudioProcessor<BenchHost>,
) {
    let entry =
        PluginEntry::load_from_clack::<clack_plugin::entry::SinglePluginEntry<NamClapPlugin>>(
            c"/bench",
        )
        .expect("Failed to load PluginEntry");

    let host_info = HostInfo::new(
        "NAM-Plug Bench Host",
        "Fabio Lima",
        "https://github.com/fabiohl/NAM-Plug",
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

    // Load state
    let state_ext = plugin_instance
        .plugin_handle()
        .get_extension::<PluginState>()
        .expect("State extension not found");
    let state_bytes = serde_json::to_vec(params).expect("Failed to serialize ProcessingParams");
    let mut handle = plugin_instance.plugin_handle();
    state_ext
        .load(&mut handle, &mut state_bytes.as_slice())
        .expect("Failed to load plugin state");

    // Configure render mode if requested
    if let Some(mode) = render_mode {
        let ext = plugin_instance
            .plugin_handle()
            .get_extension::<PluginRender>();
        if let Some(render_ext) = ext {
            let mut handle = plugin_instance.plugin_handle();
            render_ext
                .set(&mut handle, mode)
                .expect("Failed to set render mode");
        }
    }

    let audio_config = PluginAudioConfiguration {
        sample_rate,
        min_frames_count: block_size as u32,
        max_frames_count: block_size as u32,
    };

    let stopped_processor = plugin_instance
        .activate(|_, _| (), audio_config)
        .expect("Failed to activate plugin");

    (plugin_instance, stopped_processor)
}

// ─────────────────────────────────────────────────────────────────────────────
// 1. CLAP Infrastructure Microbenchmarks
// ─────────────────────────────────────────────────────────────────────────────

fn bench_clap_infrastructure(c: &mut Criterion) {
    const BLOCK_SIZES: &[usize] = &[32, 64, 128, 256, 512, 1024];

    // ── 1.1 Passthrough (Empty state, no model) ──
    let mut group_passthrough = c.benchmark_group("CLAP_Infrastructure/Passthrough");
    for &block_size in BLOCK_SIZES {
        let params = ProcessingParams::default();
        let (mut plugin_instance, stopped_processor) =
            create_and_activate_bench_plugin(48000.0, block_size, &params, None);
        let mut started_processor = stopped_processor
            .start_processing()
            .expect("Failed to start processing");

        let sine = generate_sine_440hz(block_size);
        let mut in_l = sine.clone();
        let mut in_r = sine.clone();
        let mut out_l = vec![0.0f32; block_size];
        let mut out_r = vec![0.0f32; block_size];

        let mut input_ports = AudioPorts::with_capacity(2, 1);
        let mut output_ports = AudioPorts::with_capacity(2, 1);
        let mut output_events_buffer = EventBuffer::with_capacity(10);

        group_passthrough.bench_with_input(
            BenchmarkId::from_parameter(format!("{block_size}_samp")),
            &block_size,
            |b, _| {
                b.iter(|| {
                    let input_events = InputEvents::empty();
                    let mut output_events = OutputEvents::from_buffer(&mut output_events_buffer);

                    let mut input_channels = [in_l.as_mut_slice(), in_r.as_mut_slice()];
                    let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
                        latency: 0,
                        channels: AudioPortBufferType::f32_input_only(
                            input_channels.iter_mut().map(InputChannel::constant),
                        ),
                    }]);

                    let output_channels = [out_l.as_mut_slice(), out_r.as_mut_slice()];
                    let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
                        latency: 0,
                        channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
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
            },
        );

        let stopped_processor = started_processor.stop_processing();
        plugin_instance.deactivate(stopped_processor);
    }
    group_passthrough.finish();

    // ── 1.2 Parameter Modulation (Continuous ParamValueEvents) ──
    let mut group_params = c.benchmark_group("CLAP_Infrastructure/ParamModulation");
    for &block_size in BLOCK_SIZES {
        let params = ProcessingParams::default();
        let (mut plugin_instance, stopped_processor) =
            create_and_activate_bench_plugin(48000.0, block_size, &params, None);
        let mut started_processor = stopped_processor
            .start_processing()
            .expect("Failed to start processing");

        let sine = generate_sine_440hz(block_size);
        let mut in_l = sine.clone();
        let mut in_r = sine.clone();
        let mut out_l = vec![0.0f32; block_size];
        let mut out_r = vec![0.0f32; block_size];

        let mut input_ports = AudioPorts::with_capacity(2, 1);
        let mut output_ports = AudioPorts::with_capacity(2, 1);
        let mut output_events_buffer = EventBuffer::with_capacity(10);

        let mut input_events_buffer = EventBuffer::new();
        let event_in = ParamValueEvent::new(
            0,
            ClapId::new(PARAM_INPUT_GAIN),
            Pckn::match_all(),
            1.5,
            Cookie::empty(),
        );
        let event_out = ParamValueEvent::new(
            (block_size / 2) as u32,
            ClapId::new(PARAM_OUTPUT_GAIN),
            Pckn::match_all(),
            -1.5,
            Cookie::empty(),
        );
        input_events_buffer.push(&event_in);
        input_events_buffer.push(&event_out);

        group_params.bench_with_input(
            BenchmarkId::from_parameter(format!("{block_size}_samp")),
            &block_size,
            |b, _| {
                b.iter(|| {
                    let input_events = InputEvents::from_buffer(&input_events_buffer);
                    let mut output_events = OutputEvents::from_buffer(&mut output_events_buffer);

                    let mut input_channels = [in_l.as_mut_slice(), in_r.as_mut_slice()];
                    let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
                        latency: 0,
                        channels: AudioPortBufferType::f32_input_only(
                            input_channels.iter_mut().map(InputChannel::constant),
                        ),
                    }]);

                    let output_channels = [out_l.as_mut_slice(), out_r.as_mut_slice()];
                    let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
                        latency: 0,
                        channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
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
            },
        );

        let stopped_processor = started_processor.stop_processing();
        plugin_instance.deactivate(stopped_processor);
    }
    group_params.finish();

    // ── 1.3 Bypass Path ──
    let mut group_bypass = c.benchmark_group("CLAP_Infrastructure/Bypass");
    {
        let block_size = 64;
        let params = ProcessingParams {
            bypass: true,
            ..Default::default()
        };
        let (mut plugin_instance, stopped_processor) =
            create_and_activate_bench_plugin(48000.0, block_size, &params, None);
        let mut started_processor = stopped_processor
            .start_processing()
            .expect("Failed to start processing");

        let sine = generate_sine_440hz(block_size);
        let mut in_l = sine.clone();
        let mut in_r = sine.clone();
        let mut out_l = vec![0.0f32; block_size];
        let mut out_r = vec![0.0f32; block_size];

        let mut input_ports = AudioPorts::with_capacity(2, 1);
        let mut output_ports = AudioPorts::with_capacity(2, 1);
        let mut output_events_buffer = EventBuffer::with_capacity(10);

        group_bypass.bench_function("Block_64samp", |b| {
            b.iter(|| {
                let input_events = InputEvents::empty();
                let mut output_events = OutputEvents::from_buffer(&mut output_events_buffer);

                let mut input_channels = [in_l.as_mut_slice(), in_r.as_mut_slice()];
                let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
                    latency: 0,
                    channels: AudioPortBufferType::f32_input_only(
                        input_channels.iter_mut().map(InputChannel::constant),
                    ),
                }]);

                let output_channels = [out_l.as_mut_slice(), out_r.as_mut_slice()];
                let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
                    latency: 0,
                    channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
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

        let stopped_processor = started_processor.stop_processing();
        plugin_instance.deactivate(stopped_processor);
    }
    group_bypass.finish();
}

// ─────────────────────────────────────────────────────────────────────────────
// 2. Real Neural Inference Benchmarks (Authentic Models & Configurations)
// ─────────────────────────────────────────────────────────────────────────────

fn bench_clap_inference(c: &mut Criterion) {
    const BLOCK_SIZES: &[usize] = &[32, 64, 128, 256, 512, 1024];

    // Mandatory fixture resolution with strict fail-closed assertion
    let a1_path = common::model_path("wavenet_a1_standard.nam");
    assert!(
        a1_path.exists(),
        "Mandatory fixture 'wavenet_a1_standard.nam' missing at {}",
        a1_path.display()
    );
    let a2_path = common::model_path("a2_example.nam");
    assert!(
        a2_path.exists(),
        "Mandatory fixture 'a2_example.nam' missing at {}",
        a2_path.display()
    );
    let lstm_path = common::model_path("lstm.nam");
    assert!(
        lstm_path.exists(),
        "Mandatory fixture 'lstm.nam' missing at {}",
        lstm_path.display()
    );

    let models: &[(&str, &std::path::Path)] = &[
        ("WaveNet_A1_Standard", &a1_path),
        ("WaveNet_A2_Slimmable", &a2_path),
        ("LSTM", &lstm_path),
    ];

    // ── 2.1 Topology & Block Size Sweeps (48 kHz Live Mode) ──
    for (model_name, model_file) in models {
        let group_name = format!("CLAP_Inference/{model_name}/BlockSize");
        let mut group = c.benchmark_group(&group_name);

        for &block_size in BLOCK_SIZES {
            let params = ProcessingParams {
                model_path: Some(model_file.to_path_buf()),
                model_hash: asset_hash(model_file),
                ..Default::default()
            };

            let (mut plugin_instance, stopped_processor) =
                create_and_activate_bench_plugin(48000.0, block_size, &params, None);
            let mut started_processor = stopped_processor
                .start_processing()
                .expect("Failed to start processing");

            let sine = generate_sine_440hz(block_size);
            let mut in_l = sine.clone();
            let mut in_r = sine.clone();
            let mut out_l = vec![0.0f32; block_size];
            let mut out_r = vec![0.0f32; block_size];

            let mut input_ports = AudioPorts::with_capacity(2, 1);
            let mut output_ports = AudioPorts::with_capacity(2, 1);
            let mut output_events_buffer = EventBuffer::with_capacity(10);

            // Pre-warm: execute one initial block outside the measurement timer
            {
                let input_events = InputEvents::empty();
                let mut output_events = OutputEvents::from_buffer(&mut output_events_buffer);
                let mut input_channels = [in_l.as_mut_slice(), in_r.as_mut_slice()];
                let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
                    latency: 0,
                    channels: AudioPortBufferType::f32_input_only(
                        input_channels.iter_mut().map(InputChannel::constant),
                    ),
                }]);
                let output_channels = [out_l.as_mut_slice(), out_r.as_mut_slice()];
                let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
                    latency: 0,
                    channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
                }]);
                let _ = started_processor.process(
                    &input_audio,
                    &mut output_audio,
                    &input_events,
                    &mut output_events,
                    None,
                    None,
                );
            }

            group.bench_with_input(
                BenchmarkId::from_parameter(format!("{block_size}_samp")),
                &block_size,
                |b, _| {
                    b.iter(|| {
                        let input_events = InputEvents::empty();
                        let mut output_events =
                            OutputEvents::from_buffer(&mut output_events_buffer);

                        let mut input_channels = [in_l.as_mut_slice(), in_r.as_mut_slice()];
                        let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
                            latency: 0,
                            channels: AudioPortBufferType::f32_input_only(
                                input_channels.iter_mut().map(InputChannel::constant),
                            ),
                        }]);

                        let output_channels = [out_l.as_mut_slice(), out_r.as_mut_slice()];
                        let mut output_audio =
                            output_ports.with_output_buffers([AudioPortBuffer {
                                latency: 0,
                                channels: AudioPortBufferType::f32_output_only(
                                    output_channels.into_iter(),
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
                },
            );

            let stopped_processor = started_processor.stop_processing();
            plugin_instance.deactivate(stopped_processor);
        }
        group.finish();
    }

    // ── 2.2 Sample Rate Variations (Resampling Overhead on WaveNet A1) ──
    {
        let mut group_rates = c.benchmark_group("CLAP_Inference/WaveNet_A1/SampleRates");
        let sample_rates: &[(&str, f64)] = &[
            ("44100Hz", 44100.0),
            ("48000Hz", 48000.0),
            ("96000Hz", 96000.0),
        ];

        for (label, sr) in sample_rates {
            let block_size = 64;
            let params = ProcessingParams {
                model_path: Some(a1_path.clone()),
                model_hash: asset_hash(&a1_path),
                ..Default::default()
            };

            let (mut plugin_instance, stopped_processor) =
                create_and_activate_bench_plugin(*sr, block_size, &params, None);
            let mut started_processor = stopped_processor
                .start_processing()
                .expect("Failed to start processing");

            let sine = generate_sine_440hz(block_size);
            let mut in_l = sine.clone();
            let mut in_r = sine.clone();
            let mut out_l = vec![0.0f32; block_size];
            let mut out_r = vec![0.0f32; block_size];

            let mut input_ports = AudioPorts::with_capacity(2, 1);
            let mut output_ports = AudioPorts::with_capacity(2, 1);
            let mut output_events_buffer = EventBuffer::with_capacity(10);

            group_rates.bench_function(*label, |b| {
                b.iter(|| {
                    let input_events = InputEvents::empty();
                    let mut output_events = OutputEvents::from_buffer(&mut output_events_buffer);

                    let mut input_channels = [in_l.as_mut_slice(), in_r.as_mut_slice()];
                    let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
                        latency: 0,
                        channels: AudioPortBufferType::f32_input_only(
                            input_channels.iter_mut().map(InputChannel::constant),
                        ),
                    }]);

                    let output_channels = [out_l.as_mut_slice(), out_r.as_mut_slice()];
                    let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
                        latency: 0,
                        channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
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

            let stopped_processor = started_processor.stop_processing();
            plugin_instance.deactivate(stopped_processor);
        }
        group_rates.finish();
    }

    // ── 2.3 Oversampling & RenderMode Matrix (WaveNet A1 Standard @ 48 kHz, Block 64) ──
    {
        let mut group_os = c.benchmark_group("CLAP_Inference/WaveNet_A1/QualityModes");
        let modes: &[(&str, OversampleFactor, Option<RenderMode>)] = &[
            ("Oversample_Off", OversampleFactor::Off, None),
            ("Oversample_2x", OversampleFactor::X2, None),
            ("Oversample_4x", OversampleFactor::X4, None),
            (
                "RenderMode_Offline_HQ",
                OversampleFactor::X4,
                Some(RenderMode::Offline),
            ),
        ];

        for (label, os_factor, render_mode) in modes {
            let block_size = 64;
            let params = ProcessingParams {
                model_path: Some(a1_path.clone()),
                model_hash: asset_hash(&a1_path),
                oversample: *os_factor,
                ..Default::default()
            };

            let (mut plugin_instance, stopped_processor) =
                create_and_activate_bench_plugin(48000.0, block_size, &params, *render_mode);
            let mut started_processor = stopped_processor
                .start_processing()
                .expect("Failed to start processing");

            let sine = generate_sine_440hz(block_size);
            let mut in_l = sine.clone();
            let mut in_r = sine.clone();
            let mut out_l = vec![0.0f32; block_size];
            let mut out_r = vec![0.0f32; block_size];

            let mut input_ports = AudioPorts::with_capacity(2, 1);
            let mut output_ports = AudioPorts::with_capacity(2, 1);
            let mut output_events_buffer = EventBuffer::with_capacity(10);

            group_os.bench_function(*label, |b| {
                b.iter(|| {
                    let input_events = InputEvents::empty();
                    let mut output_events = OutputEvents::from_buffer(&mut output_events_buffer);

                    let mut input_channels = [in_l.as_mut_slice(), in_r.as_mut_slice()];
                    let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
                        latency: 0,
                        channels: AudioPortBufferType::f32_input_only(
                            input_channels.iter_mut().map(InputChannel::constant),
                        ),
                    }]);

                    let output_channels = [out_l.as_mut_slice(), out_r.as_mut_slice()];
                    let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
                        latency: 0,
                        channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
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

            let stopped_processor = started_processor.stop_processing();
            plugin_instance.deactivate(stopped_processor);
        }
        group_os.finish();
    }

    // ── 2.4 CabSim IR Matrix (WaveNet A1 Standard @ 48 kHz, Block 64) ──
    {
        let ir_path = common::create_bench_ir_wav(48000);
        let mut group_cab = c.benchmark_group("CLAP_Inference/WaveNet_A1/CabSim");

        // CabSim Off
        {
            let block_size = 64;
            let params = ProcessingParams {
                model_path: Some(a1_path.clone()),
                model_hash: asset_hash(&a1_path),
                ir_path: None,
                ir_hash: None,
                ..Default::default()
            };

            let (mut plugin_instance, stopped_processor) =
                create_and_activate_bench_plugin(48000.0, block_size, &params, None);
            let mut started_processor = stopped_processor
                .start_processing()
                .expect("Failed to start processing");

            let sine = generate_sine_440hz(block_size);
            let mut in_l = sine.clone();
            let mut in_r = sine.clone();
            let mut out_l = vec![0.0f32; block_size];
            let mut out_r = vec![0.0f32; block_size];

            let mut input_ports = AudioPorts::with_capacity(2, 1);
            let mut output_ports = AudioPorts::with_capacity(2, 1);
            let mut output_events_buffer = EventBuffer::with_capacity(10);

            group_cab.bench_function("CabSim_Off", |b| {
                b.iter(|| {
                    let input_events = InputEvents::empty();
                    let mut output_events = OutputEvents::from_buffer(&mut output_events_buffer);

                    let mut input_channels = [in_l.as_mut_slice(), in_r.as_mut_slice()];
                    let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
                        latency: 0,
                        channels: AudioPortBufferType::f32_input_only(
                            input_channels.iter_mut().map(InputChannel::constant),
                        ),
                    }]);

                    let output_channels = [out_l.as_mut_slice(), out_r.as_mut_slice()];
                    let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
                        latency: 0,
                        channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
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

            let stopped_processor = started_processor.stop_processing();
            plugin_instance.deactivate(stopped_processor);
        }

        // CabSim On
        {
            let block_size = 64;
            let params = ProcessingParams {
                model_path: Some(a1_path.clone()),
                model_hash: asset_hash(&a1_path),
                ir_path: Some(ir_path.clone()),
                ir_hash: asset_hash(&ir_path),
                ..Default::default()
            };

            let (mut plugin_instance, stopped_processor) =
                create_and_activate_bench_plugin(48000.0, block_size, &params, None);
            let mut started_processor = stopped_processor
                .start_processing()
                .expect("Failed to start processing");

            let sine = generate_sine_440hz(block_size);
            let mut in_l = sine.clone();
            let mut in_r = sine.clone();
            let mut out_l = vec![0.0f32; block_size];
            let mut out_r = vec![0.0f32; block_size];

            let mut input_ports = AudioPorts::with_capacity(2, 1);
            let mut output_ports = AudioPorts::with_capacity(2, 1);
            let mut output_events_buffer = EventBuffer::with_capacity(10);

            group_cab.bench_function("CabSim_On", |b| {
                b.iter(|| {
                    let input_events = InputEvents::empty();
                    let mut output_events = OutputEvents::from_buffer(&mut output_events_buffer);

                    let mut input_channels = [in_l.as_mut_slice(), in_r.as_mut_slice()];
                    let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
                        latency: 0,
                        channels: AudioPortBufferType::f32_input_only(
                            input_channels.iter_mut().map(InputChannel::constant),
                        ),
                    }]);

                    let output_channels = [out_l.as_mut_slice(), out_r.as_mut_slice()];
                    let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
                        latency: 0,
                        channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
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

            let stopped_processor = started_processor.stop_processing();
            plugin_instance.deactivate(stopped_processor);
        }

        group_cab.finish();
    }
}

criterion_group! {
    name = clap_benches;
    config = Criterion::default().sample_size(30).noise_threshold(0.05);
    targets = bench_clap_infrastructure, bench_clap_inference
}

criterion_main!(clap_benches);
