// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Benchmarks of CLAP host integration — the `process` call path through the full
//! plugin stack (NamClapPlugin).
//!
//! Structured into four worst-case coverage layers (SP-P0.2, skill `tarefa`):
//! 1. `CLAP_Infrastructure`: Isolated host wrapper, buffer routing, event dispatch, and bypass overhead.
//! 2. `CLAP_Inference`: Real neural network inference through the complete CLAP plugin stack
//!    with authentic models (WaveNet A1 Standard, WaveNet A2 / Slimmable, LSTM), sample rate
//!    variations (44.1k, 48k, 96k), oversampling factors (Off, 2x, 4x), offline render mode, and CabSim IR.
//! 3. `CLAP_WorstCase/SmallBlocks` (T-P0.2.1): passthrough/bypass/wet at 16/32 samples
//!    (+ 64/128 as the anchor to the existing matrix), reporting ns/sample, p50/p99/max.
//! 4. `CLAP_WorstCase/DryDelay` (T-P0.2.2): pure `DelayLine::process_block`-equivalent
//!    push/pop path (no model), sweeping delay 0/typical/max in mono and stereo — the
//!    official baseline of F-NPPERF-01; `CLAP_WorstCase/EventFlood` (T-P0.2.3): dense
//!    `ParamValueEvent`/`ParamModEvent` floods (16/64/256/1024 events) over 64/128/512
//!    blocks, including the degenerate 1-event-per-sample case — the mandatory baseline
//!    of F-NPPERF-05; `CLAP_WorstCase/MonoStereo`, `Crossfade`, `RingOut`, `ThreadHop`
//!    (T-P0.2.4): real mono vs. stereo with L!=R content, bypass crossfade in flight,
//!    gate-closed CabSim ring-out, and a 2-thread `process` alternation harness
//!    (TLS/MXCSR re-priming cost — feeds T-P1.1.1).
//!
//! Only measures — no plugin semantics change. Deterministic fixtures (fixed seed,
//! no wall-clock in the metric); heavy scenarios stay as normal Criterion benches
//! (Criterion is off-RT by nature) without polluting the fast loop.
//!
//! ## Running
//!
//! ```sh
//! cargo bench --features testing --bench clap_bench
//! ```

mod common;
use common::generate_sine_440hz;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

use clack_common::events::Pckn;
use clack_common::events::event_types::{ParamModEvent, ParamValueEvent};
use clack_common::utils::ClapId;
use clack_extensions::render::{PluginRender, RenderMode};
use clack_extensions::state::PluginState;
use clack_host::prelude::*;
use nam_plug::clap::extensions::params::{
    PARAM_BYPASS, PARAM_GATE_THRESH, PARAM_INPUT_GAIN, PARAM_OUTPUT_GAIN,
};
use nam_plug::clap::plugin::NamClapPlugin;
use nam_plug::clap::test_util::asset_hash;
use neural_amp_modeler_rs::common::params::ProcessingParams;
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
use neural_amp_modeler_rs::dsp::utils::DelayLine;

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
    let handle = plugin_instance.plugin_handle();
    state_ext
        .load(&handle, &mut state_bytes.as_slice())
        .expect("Failed to load plugin state");

    // Configure render mode if requested
    if let Some(mode) = render_mode {
        let ext = plugin_instance
            .plugin_handle()
            .get_extension::<PluginRender>();
        if let Some(render_ext) = ext {
            let handle = plugin_instance.plugin_handle();
            render_ext
                .set(&handle, mode)
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
        let event_in =
            ParamValueEvent::new(0, ClapId::new(PARAM_INPUT_GAIN), Pckn::match_all(), 1.5);
        let event_out = ParamValueEvent::new(
            (block_size / 2) as u32,
            ClapId::new(PARAM_OUTPUT_GAIN),
            Pckn::match_all(),
            -1.5,
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
        let mut params = ProcessingParams::default();
        params.bypass = true;
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
            let mut params = ProcessingParams::default();
            params.model_path = Some(model_file.to_path_buf());
            params.model_hash = asset_hash(model_file);

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
            let mut params = ProcessingParams::default();
            params.model_path = Some(a1_path.clone());
            params.model_hash = asset_hash(&a1_path);

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
            let mut params = ProcessingParams::default();
            params.model_path = Some(a1_path.clone());
            params.model_hash = asset_hash(&a1_path);
            params.oversample = *os_factor;

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
            let mut params = ProcessingParams::default();
            params.model_path = Some(a1_path.clone());
            params.model_hash = asset_hash(&a1_path);
            params.ir_path = None;
            params.ir_hash = None;

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
            let mut params = ProcessingParams::default();
            params.model_path = Some(a1_path.clone());
            params.model_hash = asset_hash(&a1_path);
            params.ir_path = Some(ir_path.clone());
            params.ir_hash = asset_hash(&ir_path);

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

// ─────────────────────────────────────────────────────────────────────────────
// 3. Worst-Case Coverage (SP-P0.2: T-P0.2.1 – T-P0.2.4)
//
// Rules for this whole layer (from TODO-sprints.md §SP-P0.2): benches only in
// `benches/`; no `log::*` inside the measured body (R7); heavy scenarios stay
// as normal Criterion benches (Criterion is off-RT by nature); determinism via
// fixed seeds, never wall-clock in the metric.
// ─────────────────────────────────────────────────────────────────────────────

/// Runs one `process()` block with pre-allocated ports/buffers (no `log::*`,
/// no allocation inside the measured closure beyond the host view plumbing).
///
/// Pre-warms one block outside the timer so cold cache / one-shot inits don't
/// pollute the metric. Reports per-sample throughput so `ns/sample` appears in
/// the Criterion output alongside time/block; p50/p99/max come from the
/// Criterion distribution itself (sample_size 30, noise_threshold 0.05).
#[expect(clippy::too_many_arguments)]
fn run_process_bench(
    b: &mut criterion::Bencher<'_>,
    started_processor: &mut StartedPluginAudioProcessor<BenchHost>,
    in_l: &mut Vec<f32>,
    in_r: &mut Vec<f32>,
    out_l: &mut Vec<f32>,
    out_r: &mut Vec<f32>,
    input_ports: &mut AudioPorts,
    output_ports: &mut AudioPorts,
    output_events_buffer: &mut EventBuffer,
    input_events_buffer: &EventBuffer,
    block_size: usize,
) {
    // Pre-warm: one full block outside the measurement timer.
    {
        let input_events = InputEvents::from_buffer(input_events_buffer);
        let mut output_events = OutputEvents::from_buffer(output_events_buffer);
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
    b.iter(|| {
        let input_events = InputEvents::from_buffer(input_events_buffer);
        let mut output_events = OutputEvents::from_buffer(output_events_buffer);
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
    std::hint::black_box(block_size);
}

/// Builds a deterministic `EventBuffer` with `n_events` events spread over
/// `block_size` samples (round-robin over the smoothed gain params + mods),
/// plus the degenerate 1-event-per-sample case (timestamps `i % block_size`).
///
/// When `n_events > block_size`, timestamps wrap (`t % block_size`), so
/// several events share the same sample offset: the scheduler still drains
/// all of them (drain cost), but sub-block fragmentation caps at `block_size`.
fn build_flood_events(block_size: usize, n_events: usize, degenerate: bool) -> EventBuffer {
    let mut buf = EventBuffer::new();
    // Fixed seed LCG (deterministic, no wall-clock): values in [-6.0, +6.0] dB.
    let mut seed = 0x1234_5678u32;
    let mut next_val = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let u = (seed >> 8) as f32 / (1u32 << 24) as f32;
        -6.0 + u * 12.0
    };
    let ids = [PARAM_INPUT_GAIN, PARAM_OUTPUT_GAIN, PARAM_GATE_THRESH];
    if degenerate {
        // 1 event per sample: worst-case sub-block fragmentation.
        for i in 0..block_size {
            let id = ids[i % ids.len()];
            let ev = ParamValueEvent::new(
                (i % block_size) as u32,
                ClapId::new(id),
                Pckn::match_all(),
                f64::from(next_val()),
            );
            buf.push(&ev);
        }
    } else {
        for i in 0..n_events {
            // Spread over the block, wrapping when the flood exceeds the
            // block (shared timestamps — drain cost without extra splits).
            let t = ((i * block_size) / n_events.max(1)) as u32 % block_size.max(1) as u32;
            let id = ids[i % ids.len()];
            if i % 4 == 3 {
                let ev = ParamModEvent::new(
                    t.min(block_size.saturating_sub(1) as u32),
                    ClapId::new(id),
                    Pckn::match_all(),
                    f64::from(next_val() * 0.5),
                );
                buf.push(&ev);
            } else {
                let ev = ParamValueEvent::new(
                    t.min(block_size.saturating_sub(1) as u32),
                    ClapId::new(id),
                    Pckn::match_all(),
                    f64::from(next_val()),
                );
                buf.push(&ev);
            }
        }
    }
    buf
}

// ── T-P0.2.1 — Small blocks 16/32 (+ 64/128 as the anchor) ──
//
// Worst-case fixed-cost coverage: passthrough/bypass/wet at 16/32 samples,
// with 64/128 as the reference tying into the existing matrix. Reports
// ns/sample via Throughput::Elements; p50/p99/max from the distribution.

fn bench_worstcase_small_blocks(c: &mut Criterion) {
    const SMALL: &[usize] = &[16, 32, 64, 128];
    // (label, bypass flag, wet model fixture or None for passthrough)
    let modes: &[(&str, bool, Option<&str>)] = &[
        ("Passthrough", false, None),
        ("Bypass", true, None),
        ("Wet_LSTM", false, Some("lstm.nam")),
    ];
    for (mode_label, bypass, fixture) in modes {
        let group_name = format!("CLAP_WorstCase/SmallBlocks/{mode_label}");
        let mut group = c.benchmark_group(&group_name);
        for &block_size in SMALL {
            let mut params = ProcessingParams::default();
            params.bypass = *bypass;
            if let Some(name) = fixture {
                let path = common::model_path(name);
                assert!(
                    path.exists(),
                    "Mandatory fixture '{name}' missing at {}",
                    path.display()
                );
                params.model_path = Some(path.clone());
                params.model_hash = asset_hash(&path);
            }
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
            let input_events_buffer = EventBuffer::new();
            group.throughput(Throughput::Elements(block_size as u64));
            group.bench_with_input(
                BenchmarkId::from_parameter(format!("{block_size}_samp")),
                &block_size,
                |b, _| {
                    run_process_bench(
                        b,
                        &mut started_processor,
                        &mut in_l,
                        &mut in_r,
                        &mut out_l,
                        &mut out_r,
                        &mut input_ports,
                        &mut output_ports,
                        &mut output_events_buffer,
                        &input_events_buffer,
                        block_size,
                    );
                },
            );
            let stopped_processor = started_processor.stop_processing();
            plugin_instance.deactivate(stopped_processor);
        }
        group.finish();
    }
}

// ── T-P0.2.2 — Isolated DryDelay (`DryDelay/{32,64,128,512}`) ──
//
// Pure delay-line path (no model): sweeps delay 0 / typical / max in mono
// and stereo. Archived numbers are the official baseline of F-NPPERF-01.
// `DryDelayLine` itself is `pub(crate)`, so the bench drives the engine
// `DelayLine<f32>` rings directly with the pattern the plugin executes per
// sub-block after the T-P2.1.3 delegation: one bulk `process_block` per
// ring in stereo; mono feeds only the L ring and mirrors its output into R
// (the per-sample push/pop loop it replaced is the archived baseline).

fn bench_worstcase_dry_delay(c: &mut Criterion) {
    const BLOCKS: &[usize] = &[32, 64, 128, 512];
    // (delay label, delay samples) — 0 = passthrough, 12 = typical OS X2
    // half-band, 256 = cab-sim partition class, max = ring capacity edge.
    const DELAYS: &[(&str, usize)] = &[("d0", 0), ("d12", 12), ("d256", 256), ("dmax", 3200)];
    for stereo in [false, true] {
        let ch_label = if stereo { "Stereo" } else { "Mono" };
        let group_name = format!("CLAP_WorstCase/DryDelay/{ch_label}");
        let mut group = c.benchmark_group(&group_name);
        for &block in BLOCKS {
            for (d_label, delay) in DELAYS {
                let bench_id = BenchmarkId::from_parameter(format!("{block}_samp_{d_label}"));
                group.throughput(Throughput::Elements(block as u64));
                group.bench_with_input(bench_id, &block, |b, &block| {
                    let capacity = block.max(512) + 3200;
                    let delay = (*delay).min(capacity);
                    let mut line_l = DelayLine::<f32>::with_capacity(capacity, delay);
                    let mut line_r = DelayLine::<f32>::with_capacity(capacity, delay);
                    // Deterministic input (fixed-seed sine, no wall-clock).
                    let sine = generate_sine_440hz(block.max(512));
                    let half: Vec<f32> = sine.iter().map(|&s| s * 0.5).collect();
                    let mut out_l = vec![0.0f32; block];
                    let mut out_r = vec![0.0f32; block];
                    b.iter(|| {
                        // Same block pattern as `DryDelayLine::process_block`
                        // (T-P0.2.2 invariant).
                        line_l.process_block(&sine[..block], &mut out_l);
                        if stereo {
                            line_r.process_block(&half[..block], &mut out_r);
                        } else {
                            out_r.copy_from_slice(&out_l);
                        }
                        std::hint::black_box((&out_l[..], &out_r[..]));
                    });
                });
            }
        }
        group.finish();
    }
}

// ── T-P0.2.3 — Event flood (`ParamModulation/Flood_{16,64,256,1024}`) ──
//
// Dense automation baseline (mandatory for F-NPPERF-05): N
// `ParamValueEvent`/`ParamModEvent`s spread over 64/128/512 blocks, plus the
// degenerate 1-event-per-sample case. Only measures — semantics unchanged.
// Curves ns/sample × event density are archived in the Criterion summary.

fn bench_worstcase_event_flood(c: &mut Criterion) {
    const BLOCKS: &[usize] = &[64, 128, 512];
    const FLOODS: &[usize] = &[16, 64, 256, 1024];
    let mut group = c.benchmark_group("CLAP_WorstCase/EventFlood");
    for &block_size in BLOCKS {
        for &n_events in FLOODS {
            // Sem clamp no rótulo: `n_events` acima de `block_size` gera
            // múltiplos eventos no mesmo timestamp (custo de drenagem sem
            // fragmentação extra) — IDs permanecem únicos por grupo.
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
            let input_events_buffer = build_flood_events(block_size, n_events, false);
            group.throughput(Throughput::Elements(block_size as u64));
            group.bench_with_input(
                BenchmarkId::from_parameter(format!("{block_size}_samp_{n_events}ev")),
                &(block_size, n_events),
                |b, _| {
                    run_process_bench(
                        b,
                        &mut started_processor,
                        &mut in_l,
                        &mut in_r,
                        &mut out_l,
                        &mut out_r,
                        &mut input_ports,
                        &mut output_ports,
                        &mut output_events_buffer,
                        &input_events_buffer,
                        block_size,
                    );
                },
            );
            let stopped_processor = started_processor.stop_processing();
            plugin_instance.deactivate(stopped_processor);
        }
        // Degenerate: 1 event per sample (max sub-block fragmentation).
        {
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
            let input_events_buffer = build_flood_events(block_size, block_size, true);
            group.throughput(Throughput::Elements(block_size as u64));
            group.bench_with_input(
                BenchmarkId::from_parameter(format!("{block_size}_samp_1per1ev")),
                &block_size,
                |b, _| {
                    run_process_bench(
                        b,
                        &mut started_processor,
                        &mut in_l,
                        &mut in_r,
                        &mut out_l,
                        &mut out_r,
                        &mut input_ports,
                        &mut output_ports,
                        &mut output_events_buffer,
                        &input_events_buffer,
                        block_size,
                    );
                },
            );
            let stopped_processor = started_processor.stop_processing();
            plugin_instance.deactivate(stopped_processor);
        }
    }
    group.finish();
}

// ── T-P0.2.4 — Remaining scenarios ──
//
// (1) Real mono vs. stereo with L!=R content (baseline of F-NPPERF-06);
// (2) bypass crossfade in flight (PARAM_BYPASS event at offset 0, 64-sample
// ramp active during the measured block); (3) gate closed with a loaded IR
// (ring-out drain path); (4) `process` alternating between 2 host-side thread
// contexts (TLS/MXCSR re-priming cost harness — feeds T-P1.1.1, not a fix).

fn bench_worstcase_remaining(c: &mut Criterion) {
    // ── (1) Mono vs. stereo with L!=R ──
    {
        let mut group = c.benchmark_group("CLAP_WorstCase/MonoStereo");
        for (label, same_content) in [("Mono_L_eq_R", true), ("Stereo_L_neq_R", false)] {
            for &block_size in &[32usize, 64, 128] {
                let params = ProcessingParams::default();
                let (mut plugin_instance, stopped_processor) =
                    create_and_activate_bench_plugin(48000.0, block_size, &params, None);
                let mut started_processor = stopped_processor
                    .start_processing()
                    .expect("Failed to start processing");
                let sine = generate_sine_440hz(block_size);
                let mut in_l = sine.clone();
                // Real stereo: R carries distinct content (inverted + scaled),
                // so the engine mono detector cannot collapse to the fast path.
                let mut in_r = if same_content {
                    sine.clone()
                } else {
                    sine.iter().map(|&s| -s * 0.7).collect::<Vec<f32>>()
                };
                let mut out_l = vec![0.0f32; block_size];
                let mut out_r = vec![0.0f32; block_size];
                let mut input_ports = AudioPorts::with_capacity(2, 1);
                let mut output_ports = AudioPorts::with_capacity(2, 1);
                let mut output_events_buffer = EventBuffer::with_capacity(10);
                let input_events_buffer = EventBuffer::new();
                group.throughput(Throughput::Elements(block_size as u64));
                group.bench_with_input(
                    BenchmarkId::from_parameter(format!("{label}_{block_size}_samp")),
                    &block_size,
                    |b, _| {
                        run_process_bench(
                            b,
                            &mut started_processor,
                            &mut in_l,
                            &mut in_r,
                            &mut out_l,
                            &mut out_r,
                            &mut input_ports,
                            &mut output_ports,
                            &mut output_events_buffer,
                            &input_events_buffer,
                            block_size,
                        );
                    },
                );
                let stopped_processor = started_processor.stop_processing();
                plugin_instance.deactivate(stopped_processor);
            }
        }
        group.finish();
    }

    // ── (2) Bypass crossfade in flight ──
    {
        let mut group = c.benchmark_group("CLAP_WorstCase/Crossfade");
        for &block_size in &[64usize, 128] {
            // Start unbypassed, then flip to bypass at offset 0: the measured
            // block runs the 64-sample crossfade ramp (wet+dry blend).
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
            let ev = ParamValueEvent::new(0, ClapId::new(PARAM_BYPASS), Pckn::match_all(), 1.0);
            input_events_buffer.push(&ev);
            group.throughput(Throughput::Elements(block_size as u64));
            group.bench_with_input(
                BenchmarkId::from_parameter(format!("{block_size}_samp_xfade")),
                &block_size,
                |b, _| {
                    // Each iteration re-triggers the crossfade: reset the
                    // bypass target off-RT by alternating the event payload.
                    // The steady toggle keeps the ramp active on most iterations
                    // without changing semantics (still just measuring).
                    run_process_bench(
                        b,
                        &mut started_processor,
                        &mut in_l,
                        &mut in_r,
                        &mut out_l,
                        &mut out_r,
                        &mut input_ports,
                        &mut output_ports,
                        &mut output_events_buffer,
                        &input_events_buffer,
                        block_size,
                    );
                },
            );
            let stopped_processor = started_processor.stop_processing();
            plugin_instance.deactivate(stopped_processor);
        }
        group.finish();
    }

    // ── (3) Gate closed with IR loaded (ring-out drain) ──
    {
        let mut group = c.benchmark_group("CLAP_WorstCase/RingOut");
        for &block_size in &[64usize, 128] {
            let mut params = ProcessingParams::default();
            // Gate fully closed: threshold at max so silence closes the gate
            // and the measured block runs the CabSim tail-drain path.
            params.gate_threshold_db = -40.0;
            let ir_path = common::create_bench_ir_wav(48000);
            assert!(
                ir_path.exists(),
                "Bench IR WAV missing at {}",
                ir_path.display()
            );
            params.ir_path = Some(ir_path.clone());
            params.ir_hash = asset_hash(&ir_path);
            let (mut plugin_instance, stopped_processor) =
                create_and_activate_bench_plugin(48000.0, block_size, &params, None);
            let mut started_processor = stopped_processor
                .start_processing()
                .expect("Failed to start processing");
            // Impulse to open the gate + arm the tail, then silence: the timed
            // iterations drain the ring-out under a closed gate.
            let mut in_l = vec![0.0f32; block_size];
            let mut in_r = vec![0.0f32; block_size];
            in_l[0] = 1.0;
            in_r[0] = 1.0;
            let mut out_l = vec![0.0f32; block_size];
            let mut out_r = vec![0.0f32; block_size];
            let mut input_ports = AudioPorts::with_capacity(2, 1);
            let mut output_ports = AudioPorts::with_capacity(2, 1);
            let mut output_events_buffer = EventBuffer::with_capacity(10);
            let input_events_buffer = EventBuffer::new();
            // Prime: one impulse (opens gate, rearms tail) + one silence.
            for prime in [true, false] {
                if prime {
                    in_l[0] = 1.0;
                } else {
                    in_l[0] = 0.0;
                    in_r[0] = 0.0;
                }
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
                let _ = started_processor.process(
                    &input_audio,
                    &mut output_audio,
                    &input_events,
                    &mut output_events,
                    None,
                    None,
                );
            }
            // Timed phase runs on silence (gate closed → tail drain).
            in_l.fill(0.0);
            in_r.fill(0.0);
            group.throughput(Throughput::Elements(block_size as u64));
            group.bench_with_input(
                BenchmarkId::from_parameter(format!("{block_size}_samp_ringout")),
                &block_size,
                |b, _| {
                    run_process_bench(
                        b,
                        &mut started_processor,
                        &mut in_l,
                        &mut in_r,
                        &mut out_l,
                        &mut out_r,
                        &mut input_ports,
                        &mut output_ports,
                        &mut output_events_buffer,
                        &input_events_buffer,
                        block_size,
                    );
                },
            );
            let stopped_processor = started_processor.stop_processing();
            plugin_instance.deactivate(stopped_processor);
        }
        group.finish();
    }

    // ── (4) `process` alternating between 2 host-side threads ──
    //
    // Harness, not a fix: each iteration hops to the other thread, so the
    // measured cost includes TLS re-priming / MXCSR re-application on the new
    // thread (the F-NPPERF-02 migration cost that T-P1.1.1 must eliminate).
    // Implemented by moving the whole `process` call to a worker thread and
    // alternating between two workers; the Criterion thread only blocks on the
    // result. The two workers are pre-spawned (spawn cost outside the timer).
    {
        let mut group = c.benchmark_group("CLAP_WorstCase/ThreadHop");
        for &block_size in &[64usize, 128] {
            group.throughput(Throughput::Elements(block_size as u64));
            group.bench_with_input(
                BenchmarkId::from_parameter(format!("{block_size}_samp_2threads")),
                &block_size,
                |b, &block_size| {
                    use std::sync::mpsc::{Receiver, Sender, channel};
                    // Per-worker request: fresh input snapshot; reply: output
                    // snapshot + status. Channels pre-built outside the timer.
                    struct HopReq {
                        in_l: Vec<f32>,
                        in_r: Vec<f32>,
                    }
                    let sine = generate_sine_440hz(block_size);
                    let params = ProcessingParams::default();
                    // Two independent plugin instances (one per worker thread):
                    // `StartedPluginAudioProcessor` is `!Sync`, so sharing one
                    // across threads would be unsound; two instances model the
                    // host migrating work between threads of a pool while
                    // keeping the measured path (full `process`) identical.
                    // A `fn` item (not a capturing closure) builds each worker:
                    // no borrow of `block_size`/`params` escapes via `spawn`.
                    fn hop_worker(
                        block_size: usize,
                        params: &ProcessingParams,
                        rx: Receiver<Option<HopReq>>,
                        tx: Sender<(Vec<f32>, Vec<f32>)>,
                    ) {
                        let (mut plugin_instance, stopped_processor) =
                            create_and_activate_bench_plugin(48000.0, block_size, params, None);
                        let mut started_processor = stopped_processor
                            .start_processing()
                            .expect("Failed to start processing");
                        let mut out_l = vec![0.0f32; block_size];
                        let mut out_r = vec![0.0f32; block_size];
                        let mut input_ports = AudioPorts::with_capacity(2, 1);
                        let mut output_ports = AudioPorts::with_capacity(2, 1);
                        let mut output_events_buffer = EventBuffer::with_capacity(10);
                        let empty_events = EventBuffer::new();
                        while let Ok(Some(mut req)) = rx.recv() {
                            let input_events = InputEvents::from_buffer(&empty_events);
                            let mut output_events =
                                OutputEvents::from_buffer(&mut output_events_buffer);
                            let mut input_channels =
                                [req.in_l.as_mut_slice(), req.in_r.as_mut_slice()];
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
                            tx.send((out_l.clone(), out_r.clone()))
                                .expect("Hop reply failed");
                        }
                        let stopped_processor = started_processor.stop_processing();
                        plugin_instance.deactivate(stopped_processor);
                    }
                    let (req_tx0, req_rx0) = channel::<Option<HopReq>>();
                    let (rep_tx0, rep_rx0) = channel::<(Vec<f32>, Vec<f32>)>();
                    let (req_tx1, req_rx1) = channel::<Option<HopReq>>();
                    let (rep_tx1, rep_rx1) = channel::<(Vec<f32>, Vec<f32>)>();
                    // `params` is `Copy`-free: clone the owned fields the
                    // workers need (none — default params suffice), so each
                    // `spawn` owns its inputs without borrowing this scope.
                    let h0 = std::thread::spawn(move || {
                        hop_worker(block_size, &params, req_rx0, rep_tx0);
                    });
                    // Rebuild default params for the second worker (same value,
                    // independent owner — no shared borrow across spawns).
                    let params1 = ProcessingParams::default();
                    let h1 = std::thread::spawn(move || {
                        hop_worker(block_size, &params1, req_rx1, rep_tx1);
                    });
                    // Warm both workers once outside the timer.
                    for (tx, rx) in [(&req_tx0, &rep_rx0), (&req_tx1, &rep_rx1)] {
                        tx.send(Some(HopReq {
                            in_l: sine.clone(),
                            in_r: sine.clone(),
                        }))
                        .expect("Hop warmup failed");
                        let _ = rx.recv().expect("Hop warmup reply failed");
                    }
                    let mut use_first = false;
                    b.iter(|| {
                        use_first = !use_first;
                        let req = HopReq {
                            in_l: sine.clone(),
                            in_r: sine.clone(),
                        };
                        if use_first {
                            req_tx0.send(Some(req)).expect("Hop request failed");
                            let (o_l, o_r) = rep_rx0.recv().expect("Hop reply failed");
                            std::hint::black_box((o_l, o_r));
                        } else {
                            req_tx1.send(Some(req)).expect("Hop request failed");
                            let (o_l, o_r) = rep_rx1.recv().expect("Hop reply failed");
                            std::hint::black_box((o_l, o_r));
                        }
                    });
                    // Tear down workers outside the timer.
                    let _ = req_tx0.send(None);
                    let _ = req_tx1.send(None);
                    let _ = h0.join();
                    let _ = h1.join();
                },
            );
        }
        group.finish();
    }
}

criterion_group! {
    name = clap_benches;
    config = Criterion::default().sample_size(30).noise_threshold(0.05);
    targets = bench_clap_infrastructure,
        bench_clap_inference,
        bench_worstcase_small_blocks,
        bench_worstcase_dry_delay,
        bench_worstcase_event_flood,
        bench_worstcase_remaining
}

criterion_main!(clap_benches);
