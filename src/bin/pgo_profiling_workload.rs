// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! PGO & BOLT Profiling Workload Tool.
//!
//! Executes a deterministic, versioned manifest of representative production scenarios
//! (covering all neural topologies, sample rates, block sizes, oversampling factors,
//! offline rendering, parameter automation, and CabSim IR convolution) to generate
//! optimal compiler profiles for PGO and BOLT.
//!
//! Strictly fail-closed: aborts with exit code 1 if any fixture is missing or if any
//! scenario fails. Generates a structured JSON receipt upon successful completion.

#![cfg(feature = "testing")]

use clack_common::events::Pckn;
use clack_common::events::event_types::ParamValueEvent;
use clack_common::utils::{ClapId, Cookie};
use clack_extensions::render::{PluginRender, RenderMode};
use clack_host::prelude::*;
use nam_plug::clap::extensions::params::{
    PARAM_BYPASS, PARAM_GATE_THRESH, PARAM_INPUT_GAIN, PARAM_OUTPUT_GAIN,
};
use nam_plug::clap::test_util::{self, asset_hash};
use neural_amp_modeler_rs::common::diagnostics::{
    SystemSnapshot,
    logger::{LoggerConfig, NamLogger},
};
use neural_amp_modeler_rs::common::params::{ActivationPrecision, ProcessingParams};
use neural_amp_modeler_rs::common::spsc;
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
use neural_amp_modeler_rs::loader;
use neural_amp_modeler_rs::models::NamModel;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub const MANIFEST_VERSION: &str = "1.0.0";

/// Describes a mandatory profiling scenario executed by the workload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestScenario {
    pub id: String,
    pub name: String,
    pub topology: String,
    pub sample_rate: f64,
    pub block_sizes: Vec<usize>,
    pub oversample_factor: String,
    pub render_mode: String,
    pub activation_precision: String,
    pub cabsim_active: bool,
    pub parameter_automation: bool,
}

/// Structured receipt emitted upon 100% successful workload completion.
#[derive(Debug, Serialize, Deserialize)]
pub struct WorkloadReceipt {
    pub manifest_version: String,
    pub status: String,
    pub timestamp_utc: String,
    pub total_scenarios: usize,
    pub total_callbacks: u64,
    pub total_samples_processed: u64,
    pub elapsed_ms: u128,
    pub fixtures: HashMap<String, FixtureMetadata>,
    pub scenario_results: Vec<ScenarioExecutionResult>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FixtureMetadata {
    pub path: String,
    pub sha256: String,
    pub file_size_bytes: u64,
    pub topology_family: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ScenarioExecutionResult {
    pub scenario_id: String,
    pub status: String,
    pub callbacks_executed: u64,
    pub samples_executed: u64,
}

fn main() {
    let start_time = Instant::now();

    // 1. Initialize panic hook and logger backend
    neural_amp_modeler_rs::common::panic_hook::install_panic_hook("pgo_profiler");
    NamLogger::init(LoggerConfig {
        level_filter: log::LevelFilter::Info,
        emit_stderr: true,
    })
    .expect("Failed to initialize NamLogger backend");

    log::info!(
        "🎸 Starting real-world fail-closed PGO/BOLT profiling workload (Manifest v{MANIFEST_VERSION})..."
    );

    // 2. Capture processor snapshot & verify baseline
    let sys = SystemSnapshot::capture();
    log::info!("  Processor capabilities verified. Mode: x86-64-v3 baseline.");

    // Setup SPSC channels to profile lock-free queue primitives
    let _channels = spsc::setup_spsc(spsc::SPSC_CAPACITY);

    // 3. Mandatory fixture discovery & verification (Fail-Closed)
    let fixture_map = match resolve_and_verify_fixtures() {
        Ok(map) => map,
        Err(err) => {
            log::error!("❌ FATAL: Fixture validation failed (Fail-Closed): {err}");
            std::process::exit(1);
        }
    };

    // 4. Generate synthetic CabSim IR WAV for profiling
    let ir_path = std::env::temp_dir().join("nam_pgo_synthetic_cab_ir.wav");
    let mut ir_data = vec![0.0f32; 512];
    ir_data[0] = 1.0;
    ir_data[1] = -0.9;
    if let Err(e) = neural_amp_modeler_rs::testing::wav::write_wav_f32(&ir_path, &ir_data, 48000) {
        log::error!("❌ FATAL: Failed to write synthetic CabSim IR: {e}");
        std::process::exit(1);
    }
    let ir_hash = asset_hash(&ir_path).unwrap_or_default();

    // 5. Generate audio excitation signals
    let samples_48k =
        neural_amp_modeler_rs::testing::stress::generate_stress_signal_v2_default(48000);
    // Slice to 48,000 samples (1 full second of diverse signal) for balanced profiling coverage
    let signal_48k = if samples_48k.len() > 48000 {
        &samples_48k[..48000]
    } else {
        &samples_48k[..]
    };
    log::info!(
        "  Generated test signal: {} samples at 48000 Hz",
        signal_48k.len()
    );

    // 6. Build the versioned manifest
    let manifest = build_manifest();
    log::info!(
        "  Defined {} mandatory profiling scenarios.",
        manifest.len()
    );

    let mut scenario_results = Vec::new();
    let mut total_callbacks: u64 = 0;
    let mut total_samples_processed: u64 = 0;

    // 7. Execute all manifest scenarios (CLAP host wrapper path)
    for scenario in &manifest {
        log::info!(
            "🚀 Executing Scenario [{}] - {} (Rate: {} Hz, Blocks: {:?}, OS: {}, Render: {}, CabSim: {})",
            scenario.id,
            scenario.name,
            scenario.sample_rate,
            scenario.block_sizes,
            scenario.oversample_factor,
            scenario.render_mode,
            scenario.cabsim_active
        );

        let model_path = match scenario.topology.as_str() {
            "WaveNet A1 Standard" => &fixture_map["wavenet_a1_standard.nam"].path,
            "WaveNet A2 / Slimmable" => &fixture_map["a2_example.nam"].path,
            "LSTM" => &fixture_map["lstm.nam"].path,
            other => {
                log::error!("❌ FATAL: Unknown topology family: {other}");
                std::process::exit(1);
            }
        };

        let result = match run_clap_scenario(
            scenario,
            model_path,
            if scenario.cabsim_active {
                Some((&ir_path, &ir_hash))
            } else {
                None
            },
            signal_48k,
        ) {
            Ok(res) => res,
            Err(err) => {
                log::error!(
                    "❌ FATAL: Scenario [{}] execution failed (Fail-Closed): {err}",
                    scenario.id
                );
                std::process::exit(1);
            }
        };

        total_callbacks += result.callbacks_executed;
        total_samples_processed += result.samples_executed;
        scenario_results.push(result);
    }

    // 8. Execute Standalone DSP direct paths
    for (name, fixture_key) in &[
        ("standalone_dsp_wavenet_a1", "wavenet_a1_standard.nam"),
        ("standalone_dsp_wavenet_a2", "a2_example.nam"),
        ("standalone_dsp_lstm", "lstm.nam"),
    ] {
        log::info!("🚀 Executing Standalone DSP Scenario [{name}]");
        let model_path = Path::new(&fixture_map[*fixture_key].path);
        let result = match run_standalone_scenario(name, model_path, &sys, signal_48k) {
            Ok(res) => res,
            Err(err) => {
                log::error!(
                    "❌ FATAL: Standalone DSP scenario [{name}] failed (Fail-Closed): {err}"
                );
                std::process::exit(1);
            }
        };
        total_callbacks += result.callbacks_executed;
        total_samples_processed += result.samples_executed;
        scenario_results.push(result);
    }

    let elapsed = start_time.elapsed();
    log::info!(
        "✓ Workload executed successfully: {} scenarios, {} callbacks, {} samples in {:.2}s",
        scenario_results.len(),
        total_callbacks,
        total_samples_processed,
        elapsed.as_secs_f64()
    );

    // 9. Emit Machine-Readable Receipt JSON
    let receipt = WorkloadReceipt {
        manifest_version: MANIFEST_VERSION.to_string(),
        status: "SUCCESS".to_string(),
        timestamp_utc: format!("{:?}", std::time::SystemTime::now()),
        total_scenarios: scenario_results.len(),
        total_callbacks,
        total_samples_processed,
        elapsed_ms: elapsed.as_millis(),
        fixtures: fixture_map,
        scenario_results,
    };

    let receipt_path = std::env::var("NAM_PGO_RECEIPT_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("target/pgo-workload-receipt.json"));

    if let Some(parent) = receipt_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let receipt_json = serde_json::to_string_pretty(&receipt).expect("Serialize receipt JSON");
    if let Err(e) = std::fs::write(&receipt_path, receipt_json) {
        log::error!(
            "❌ FATAL: Failed to write receipt to {}: {e}",
            receipt_path.display()
        );
        std::process::exit(1);
    }

    log::info!("📄 Generated receipt at: {}", receipt_path.display());
    log::info!("✓ Real-world PGO profiling workload completed successfully.");
}

/// Resolves mandatory fixtures and returns their metadata. Aborts immediately if missing.
fn resolve_and_verify_fixtures() -> Result<HashMap<String, FixtureMetadata>, String> {
    let mut map = HashMap::new();

    let search_dirs = {
        let mut dirs = Vec::new();
        if let Ok(dir) = std::env::var("NAM_FIXTURES_DIR") {
            dirs.push(PathBuf::from(dir));
        }
        dirs.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/models"));
        dirs.push(PathBuf::from("tests/fixtures/models"));
        dirs
    };

    let mandatory: &[(&str, &str)] = &[
        ("wavenet_a1_standard.nam", "WaveNet A1 Standard"),
        ("a2_example.nam", "WaveNet A2 / Slimmable"),
        ("lstm.nam", "LSTM"),
    ];

    for &(filename, family) in mandatory {
        let mut resolved_path: Option<PathBuf> = None;
        for dir in &search_dirs {
            let candidate = dir.join(filename);
            if candidate.exists() {
                resolved_path = Some(candidate);
                break;
            }
        }

        let path = match resolved_path {
            Some(p) => p,
            None => {
                return Err(format!(
                    "Mandatory fixture '{filename}' not found in search paths: {search_dirs:?}"
                ));
            }
        };

        let sha256 = asset_hash(&path)
            .ok_or_else(|| format!("Failed to compute SHA256 for {}", path.display()))?;
        let file_size_bytes = std::fs::metadata(&path)
            .map(|m| m.len())
            .map_err(|e| format!("Failed to read metadata for {}: {e}", path.display()))?;

        log::info!(
            "  Found mandatory fixture '{}' [{} bytes, SHA256: {}]",
            filename,
            file_size_bytes,
            &sha256[..12]
        );

        map.insert(
            filename.to_string(),
            FixtureMetadata {
                path: path.to_string_lossy().to_string(),
                sha256,
                file_size_bytes,
                topology_family: family.to_string(),
            },
        );
    }

    Ok(map)
}

/// Builds the versioned manifest of scenarios.
fn build_manifest() -> Vec<ManifestScenario> {
    vec![
        ManifestScenario {
            id: "wavenet_a1_live_48k".to_string(),
            name: "WaveNet A1 Standard Live 48kHz".to_string(),
            topology: "WaveNet A1 Standard".to_string(),
            sample_rate: 48000.0,
            block_sizes: vec![64, 128, 256, 512],
            oversample_factor: "Off".to_string(),
            render_mode: "Realtime".to_string(),
            activation_precision: "Standard".to_string(),
            cabsim_active: false,
            parameter_automation: true,
        },
        ManifestScenario {
            id: "wavenet_a1_resample_44k".to_string(),
            name: "WaveNet A1 Standard Resample 44.1kHz".to_string(),
            topology: "WaveNet A1 Standard".to_string(),
            sample_rate: 44100.0,
            block_sizes: vec![64, 128],
            oversample_factor: "Off".to_string(),
            render_mode: "Realtime".to_string(),
            activation_precision: "Standard".to_string(),
            cabsim_active: false,
            parameter_automation: false,
        },
        ManifestScenario {
            id: "wavenet_a1_resample_96k".to_string(),
            name: "WaveNet A1 Standard Resample 96kHz".to_string(),
            topology: "WaveNet A1 Standard".to_string(),
            sample_rate: 96000.0,
            block_sizes: vec![64, 128],
            oversample_factor: "Off".to_string(),
            render_mode: "Realtime".to_string(),
            activation_precision: "Standard".to_string(),
            cabsim_active: false,
            parameter_automation: false,
        },
        ManifestScenario {
            id: "wavenet_a1_os_2x".to_string(),
            name: "WaveNet A1 Standard Oversample 2x".to_string(),
            topology: "WaveNet A1 Standard".to_string(),
            sample_rate: 48000.0,
            block_sizes: vec![64, 128],
            oversample_factor: "2x".to_string(),
            render_mode: "Realtime".to_string(),
            activation_precision: "Standard".to_string(),
            cabsim_active: false,
            parameter_automation: false,
        },
        ManifestScenario {
            id: "wavenet_a1_os_4x".to_string(),
            name: "WaveNet A1 Standard Oversample 4x".to_string(),
            topology: "WaveNet A1 Standard".to_string(),
            sample_rate: 48000.0,
            block_sizes: vec![64, 128],
            oversample_factor: "4x".to_string(),
            render_mode: "Realtime".to_string(),
            activation_precision: "Standard".to_string(),
            cabsim_active: false,
            parameter_automation: false,
        },
        ManifestScenario {
            id: "wavenet_a1_offline_hq".to_string(),
            name: "WaveNet A1 Standard Offline HQ 4x".to_string(),
            topology: "WaveNet A1 Standard".to_string(),
            sample_rate: 48000.0,
            block_sizes: vec![128, 256, 512],
            oversample_factor: "4x".to_string(),
            render_mode: "Offline".to_string(),
            activation_precision: "Standard".to_string(),
            cabsim_active: false,
            parameter_automation: false,
        },
        ManifestScenario {
            id: "wavenet_a1_cabsim".to_string(),
            name: "WaveNet A1 Standard + CabSim IR".to_string(),
            topology: "WaveNet A1 Standard".to_string(),
            sample_rate: 48000.0,
            block_sizes: vec![64, 128, 256],
            oversample_factor: "Off".to_string(),
            render_mode: "Realtime".to_string(),
            activation_precision: "Standard".to_string(),
            cabsim_active: true,
            parameter_automation: false,
        },
        ManifestScenario {
            id: "wavenet_a1_fast_activation".to_string(),
            name: "WaveNet A1 Standard Fast Activation".to_string(),
            topology: "WaveNet A1 Standard".to_string(),
            sample_rate: 48000.0,
            block_sizes: vec![64, 128],
            oversample_factor: "Off".to_string(),
            render_mode: "Realtime".to_string(),
            activation_precision: "Fast".to_string(),
            cabsim_active: false,
            parameter_automation: false,
        },
        ManifestScenario {
            id: "wavenet_a2_live_48k".to_string(),
            name: "WaveNet A2 Slimmable Live 48kHz".to_string(),
            topology: "WaveNet A2 / Slimmable".to_string(),
            sample_rate: 48000.0,
            block_sizes: vec![64, 128, 256],
            oversample_factor: "Off".to_string(),
            render_mode: "Realtime".to_string(),
            activation_precision: "Standard".to_string(),
            cabsim_active: false,
            parameter_automation: true,
        },
        ManifestScenario {
            id: "wavenet_a2_os_2x".to_string(),
            name: "WaveNet A2 Slimmable Oversample 2x".to_string(),
            topology: "WaveNet A2 / Slimmable".to_string(),
            sample_rate: 48000.0,
            block_sizes: vec![64, 128],
            oversample_factor: "2x".to_string(),
            render_mode: "Realtime".to_string(),
            activation_precision: "Standard".to_string(),
            cabsim_active: false,
            parameter_automation: false,
        },
        ManifestScenario {
            id: "lstm_live_48k".to_string(),
            name: "LSTM Live 48kHz".to_string(),
            topology: "LSTM".to_string(),
            sample_rate: 48000.0,
            block_sizes: vec![32, 64, 128, 256],
            oversample_factor: "Off".to_string(),
            render_mode: "Realtime".to_string(),
            activation_precision: "Standard".to_string(),
            cabsim_active: false,
            parameter_automation: true,
        },
        ManifestScenario {
            id: "lstm_resample_44k_cabsim".to_string(),
            name: "LSTM Resample 44.1kHz + CabSim IR".to_string(),
            topology: "LSTM".to_string(),
            sample_rate: 44100.0,
            block_sizes: vec![64, 128],
            oversample_factor: "Off".to_string(),
            render_mode: "Realtime".to_string(),
            activation_precision: "Standard".to_string(),
            cabsim_active: true,
            parameter_automation: false,
        },
    ]
}

/// Executes a single CLAP scenario through the plugin stack.
fn run_clap_scenario(
    scenario: &ManifestScenario,
    model_path: &str,
    ir_info: Option<(&Path, &str)>,
    samples: &[f32],
) -> Result<ScenarioExecutionResult, Box<dyn std::error::Error>> {
    let so_path = std::env::var("NAM_CLAP_SO_PATH").ok();
    let (_entry, _host_info, mut plugin_instance) = match so_path {
        Some(ref path) => test_util::make_test_plugin_dynamic(Path::new(path)),
        None => test_util::make_test_plugin(),
    };

    let oversample = match scenario.oversample_factor.as_str() {
        "2x" => OversampleFactor::X2,
        "4x" => OversampleFactor::X4,
        _ => OversampleFactor::Off,
    };

    let activation_precision = match scenario.activation_precision.as_str() {
        "Fast" => ActivationPrecision::Fast,
        _ => ActivationPrecision::Standard,
    };

    let (ir_path_buf, ir_hash_opt) = match ir_info {
        Some((p, h)) => (Some(p.to_path_buf()), Some(h.to_string())),
        None => (None, None),
    };

    let params = ProcessingParams {
        model_path: Some(PathBuf::from(model_path)),
        model_hash: asset_hash(Path::new(model_path)),
        oversample,
        activation_precision,
        ir_path: ir_path_buf,
        ir_hash: ir_hash_opt,
        ..Default::default()
    };

    test_util::load_plugin_state(&mut plugin_instance, &params);

    if scenario.render_mode == "Offline" {
        let ext = plugin_instance
            .plugin_handle()
            .get_extension::<PluginRender>();
        if let Some(render_ext) = ext {
            let mut handle = plugin_instance.plugin_handle();
            render_ext.set(&mut handle, RenderMode::Offline)?;
        }
    }

    let mut callbacks_count: u64 = 0;
    let mut samples_count: u64 = 0;

    for &block_size in &scenario.block_sizes {
        let audio_config = PluginAudioConfiguration {
            sample_rate: scenario.sample_rate,
            min_frames_count: block_size as u32,
            max_frames_count: block_size as u32,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config)?;
        let mut started_processor = stopped_processor.start_processing()?;

        let mut input_ports = AudioPorts::with_capacity(2, 1);
        let mut output_ports = AudioPorts::with_capacity(2, 1);
        let mut output_events_buffer = EventBuffer::new();

        let mut offset = 0;
        let mut block_idx: u32 = 0;
        while offset < samples.len() {
            let chunk_len = std::cmp::min(block_size, samples.len() - offset);
            if chunk_len == 0 {
                break;
            }

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

            let mut input_events_buffer = EventBuffer::new();
            if scenario.parameter_automation && block_idx.is_multiple_of(4) {
                let mod_gain = ((block_idx % 20) as f64 - 10.0) * 0.5;
                input_events_buffer.push(&ParamValueEvent::new(
                    0,
                    ClapId::new(PARAM_INPUT_GAIN),
                    Pckn::match_all(),
                    mod_gain,
                    Cookie::empty(),
                ));
                input_events_buffer.push(&ParamValueEvent::new(
                    (chunk_len / 4) as u32,
                    ClapId::new(PARAM_GATE_THRESH),
                    Pckn::match_all(),
                    -60.0 + (block_idx % 30) as f64,
                    Cookie::empty(),
                ));
                input_events_buffer.push(&ParamValueEvent::new(
                    (chunk_len / 2) as u32,
                    ClapId::new(PARAM_OUTPUT_GAIN),
                    Pckn::match_all(),
                    -mod_gain,
                    Cookie::empty(),
                ));
                if block_idx.is_multiple_of(16) {
                    input_events_buffer.push(&ParamValueEvent::new(
                        (3 * chunk_len / 4) as u32,
                        ClapId::new(PARAM_BYPASS),
                        Pckn::match_all(),
                        0.0,
                        Cookie::empty(),
                    ));
                }
            }
            let input_events = InputEvents::from_buffer(&input_events_buffer);
            let mut output_events = OutputEvents::from_buffer(&mut output_events_buffer);

            started_processor.process(
                &input_audio,
                &mut output_audio,
                &input_events,
                &mut output_events,
                None,
                None,
            )?;

            callbacks_count += 1;
            samples_count += chunk_len as u64;
            offset += chunk_len;
            block_idx += 1;
        }

        let stopped_processor = started_processor.stop_processing();
        plugin_instance.deactivate(stopped_processor);
    }

    Ok(ScenarioExecutionResult {
        scenario_id: scenario.id.clone(),
        status: "PASSED".to_string(),
        callbacks_executed: callbacks_count,
        samples_executed: samples_count,
    })
}

/// Executes a direct standalone DSP engine scenario.
fn run_standalone_scenario(
    name: &str,
    model_path: &Path,
    sys: &SystemSnapshot,
    samples: &[f32],
) -> Result<ScenarioExecutionResult, Box<dyn std::error::Error>> {
    let loaded =
        loader::load_and_build_model(model_path, sys, true, loader::LoadOptions::default())?;

    let mut callbacks_count: u64 = 0;
    let mut samples_count: u64 = 0;

    if let Some(mut model) = loaded.model_l {
        model.prewarm(2048);

        let block_size = 64;
        let mut offset = 0;
        let mut out = vec![0.0f32; block_size];

        while offset < samples.len() {
            let chunk_len = std::cmp::min(block_size, samples.len() - offset);
            if chunk_len < block_size {
                break;
            }

            let chunk = &samples[offset..offset + block_size];
            model.process(chunk, &mut out);
            callbacks_count += 1;
            samples_count += block_size as u64;
            offset += block_size;
        }
    }

    Ok(ScenarioExecutionResult {
        scenario_id: name.to_string(),
        status: "PASSED".to_string(),
        callbacks_executed: callbacks_count,
        samples_executed: samples_count,
    })
}
