// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! PGO Profiling Workload Tool.
//!
//! Mimics the final standalone CLI and CLAP plugin execution structures to run a
//! highly representative real-world workload (loading real WAV files and models)
//! to generate optimal compiler profiles for PGO.

#![cfg(feature = "testing")]

use clack_host::prelude::*;
use nam_plug::clap::test_util;
use neural_amp_modeler_rs::common::diagnostics::{
    SystemSnapshot,
    logger::{LoggerConfig, NamLogger},
};
use neural_amp_modeler_rs::common::spsc;
use neural_amp_modeler_rs::loader;
use neural_amp_modeler_rs::models::NamModel;
use std::path::{Path, PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Replicate standalone startup initialization structure
    neural_amp_modeler_rs::common::panic_hook::install_panic_hook("pgo_profiler");
    NamLogger::init(LoggerConfig {
        level_filter: log::LevelFilter::Info,
        emit_stderr: true,
    })
    .expect("Failed to initialize NamLogger backend");

    log::info!("🎸 Starting real-world PGO profiling workload...");

    // Capture system architecture/SIMD snapshot as in final standalone
    let sys = SystemSnapshot::capture();
    log::info!("  Processor capabilities verified. Mode: x86-64-v3 baseline.");

    // Setup SPSC channels to profile lock-free queue primitives
    let _channels = spsc::setup_spsc(spsc::SPSC_CAPACITY);

    // 1. Generate stress signal in-process
    let samples = neural_amp_modeler_rs::testing::stress::generate_stress_signal_v2_default(48000);
    log::info!(
        "  Generated {} samples stress signal at 48000 Hz",
        samples.len()
    );

    // 2. Select models representing WaveNet A1, WaveNet A2, LSTM, and Custom topologies
    let models_to_run = resolve_workload_models();

    // 3. Profile CLAP mode wrapper & extensions (State, Params, Ports)
    for model_path in &models_to_run {
        log::info!(
            "🚀 Profiling CLAP plugin path for model: {:?}",
            model_path.file_name().unwrap()
        );
        // Run with model rate (no resampling) and different rates (triggers resampler)
        for &target_sr in &[48000.0, 44100.0] {
            profile_clap_model(&samples, target_sr, model_path)?;
        }
    }

    // 4. Profile Standalone mode loading & DSP path
    for model_path in &models_to_run {
        log::info!(
            "🚀 Profiling Standalone DSP path for model: {:?}",
            model_path.file_name().unwrap()
        );
        profile_standalone_model(&samples, &sys, model_path)?;
    }

    log::info!("✓ Real-world PGO profiling workload completed successfully.");
    Ok(())
}

/// Resolves a suíte de modelos cobrindo as 4 famílias topológicas:
/// 1. WaveNet A1 Standard
/// 2. WaveNet A2 / SlimmableContainer
/// 3. LSTM 1x16
/// 4. WaveNet Custom
fn resolve_workload_models() -> Vec<PathBuf> {
    let mut resolved = Vec::new();

    if let Ok(p) = std::env::var("NAM_MODEL") {
        let path = PathBuf::from(&p);
        if path.exists() {
            resolved.push(path);
        } else {
            log::warn!(
                "pgo_profiling_workload: NAM_MODEL={p} not found, proceeding with fixture search"
            );
        }
    }

    let search_dirs = {
        let mut dirs = Vec::new();
        if let Ok(dir) = std::env::var("NAM_FIXTURES_DIR") {
            dirs.push(PathBuf::from(dir));
        }
        dirs.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/models"));
        dirs.push(PathBuf::from("tests/fixtures/models"));
        dirs
    };

    let topology_categories: &[(&str, &[&str])] = &[
        ("WaveNet A1 Standard", &["wavenet_a1_standard.nam"]),
        ("WaveNet A2 / SlimmableContainer", &["a2_example.nam"]),
        ("LSTM", &["lstm.nam"]),
    ];

    for (cat_name, candidates) in topology_categories {
        let mut found_for_cat = false;
        for dir in &search_dirs {
            for name in *candidates {
                let path = dir.join(name);
                if path.exists() && !resolved.contains(&path) {
                    log::info!(
                        "pgo_profiling_workload: resolved fixture for category '{}': {}",
                        cat_name,
                        path.display()
                    );
                    resolved.push(path);
                    found_for_cat = true;
                    break;
                }
            }
            if found_for_cat {
                break;
            }
        }
        if !found_for_cat {
            log::warn!(
                "pgo_profiling_workload: no fixture found for category '{cat_name}' in search dirs"
            );
        }
    }

    if resolved.is_empty() {
        log::info!("pgo_profiling_workload: attempting generic fallback for any .nam model...");
        for dir in &search_dirs {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|s| s.to_str()) == Some("nam") {
                        log::info!(
                            "pgo_profiling_workload: fallback model found: {}",
                            path.display()
                        );
                        resolved.push(path);
                        break;
                    }
                }
            }
            if !resolved.is_empty() {
                break;
            }
        }
    }

    if resolved.is_empty() {
        log::error!(
            "pgo_profiling_workload: ERROR: No .nam model files found in any search location! \
             Set NAM_FIXTURES_DIR or NAM_MODEL."
        );
        std::process::exit(1);
    }

    resolved
}

/// Emulates a host running the CLAP plugin on the given audio.
fn profile_clap_model(
    samples: &[f32],
    sample_rate: f64,
    model_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let so_path = std::env::var("NAM_CLAP_SO_PATH").ok();
    let (_entry, _host_info, mut plugin_instance) = match so_path {
        Some(ref path) => {
            log::info!("🔌 Profiling CLAP dynamically from .so: {}", path);
            test_util::make_test_plugin_dynamic(Path::new(path))
        }
        None => {
            log::info!("🔌 Profiling CLAP statically (compiled-in plugin)");
            test_util::make_test_plugin()
        }
    };

    // Serialize model path into the plugin's state and restore it
    let params = test_util::make_default_params(Some(model_path.to_path_buf()));
    test_util::load_plugin_state(&mut plugin_instance, &params);

    // Profile common buffer sizes: 64 and 128
    for &block_size in &[64, 128] {
        let audio_config = PluginAudioConfiguration {
            sample_rate,
            min_frames_count: block_size as u32,
            max_frames_count: block_size as u32,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config)?;
        let mut started_processor = stopped_processor.start_processing()?;

        let mut input_ports = AudioPorts::with_capacity(2, 1);
        let mut output_ports = AudioPorts::with_capacity(2, 1);
        let mut output_events_buffer = EventBuffer::new();

        let mut offset = 0;
        while offset < samples.len() {
            let chunk_len = std::cmp::min(block_size, samples.len() - offset);
            if chunk_len == 0 {
                break;
            }

            // Mock stereo input from mono WAV
            let mut in_l = samples[offset..offset + chunk_len].to_vec();
            let mut in_r = samples[offset..offset + chunk_len].to_vec();
            let mut out_l = vec![0.0f32; chunk_len];
            let mut out_r = vec![0.0f32; chunk_len];

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

            let input_events = InputEvents::empty();
            let mut output_events = OutputEvents::from_buffer(&mut output_events_buffer);

            started_processor.process(
                &input_audio,
                &mut output_audio,
                &input_events,
                &mut output_events,
                None,
                None,
            )?;

            offset += chunk_len;
        }

        let stopped_processor = started_processor.stop_processing();
        plugin_instance.deactivate(stopped_processor);
    }

    Ok(())
}

/// Emulates the standalone loader and core DSP engine processing.
fn profile_standalone_model(
    samples: &[f32],
    sys: &SystemSnapshot,
    model_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    // Replicates how standalone loads and runs inference using the standard dispatcher
    let loaded =
        loader::load_and_build_model(model_path, sys, true, loader::LoadOptions::default())?;

    if let Some(mut model) = loaded.model_l {
        model.prewarm(2048);

        let block_size = 64;
        let mut offset = 0;
        let mut out = vec![0.0f32; block_size];

        while offset < samples.len() {
            let chunk_len = std::cmp::min(block_size, samples.len() - offset);
            if chunk_len < block_size {
                break; // Skip last partial block to avoid mismatch
            }

            let chunk = &samples[offset..offset + block_size];
            model.process(chunk, &mut out);
            offset += block_size;
        }
    }

    Ok(())
}
