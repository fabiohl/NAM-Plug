// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! CLI entry point for the Distributed Artifact Performance Certification Gate.
//!
//! Dynamically loads the final distributed CLAP artifact (post-strip and post-BOLT),
//! executes authentic neural inference benchmarks across core topologies (WaveNet A1 Standard,
//! WaveNet A2 Slimmable, LSTM) and block sizes (64 and 128 samples), computes statistical latency
//! distributions (p50, p95, p99, max, mean, ns/sample), evaluates real-time deadline margins,
//! and detects host environmental noise / thermal anomalies.
//!
//! Exit codes:
//!   0 — Performance certification PASSED.
//!   1 — Performance certification FAILED (deadline overrun or fatal error).
//!   2 — Performance certification INCONCLUSIVE (excessive host CPU jitter / noise).

#![cfg(feature = "testing")]

use clack_extensions::state::PluginState;
use clack_host::prelude::*;
use neural_amp_modeler_rs::common::diagnostics::logger::{LoggerConfig, NamLogger};
use neural_amp_modeler_rs::common::params::{ActivationPrecision, ProcessingParams};
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

struct PerfHostShared;
impl clack_host::prelude::SharedHandler<'_> for PerfHostShared {
    fn request_restart(&self) {}
    fn request_process(&self) {}
    fn request_callback(&self) {}
}

struct PerfHost;
impl clack_host::prelude::HostHandlers for PerfHost {
    type Shared<'a> = PerfHostShared;
    type MainThread<'a> = ();
    type AudioProcessor<'a> = ();
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PerfCertificationReport {
    pub schema_version: String,
    pub timestamp_utc: String,
    pub clap_artifact_path: String,
    pub clap_artifact_sha256: String,
    pub overall_status: String,
    pub environment_stable: bool,
    pub total_scenarios: usize,
    pub scenarios: Vec<ScenarioPerfMetrics>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ScenarioPerfMetrics {
    pub name: String,
    pub topology: String,
    pub sample_rate: f64,
    pub block_size: usize,
    pub iterations: usize,
    pub p50_ns: f64,
    pub p95_ns: f64,
    pub p99_ns: f64,
    pub max_ns: f64,
    pub mean_ns: f64,
    pub stddev_ns: f64,
    pub ns_per_sample: f64,
    pub deadline_budget_ns: f64,
    pub deadline_margin_percent: f64,
    pub deadline_passed: bool,
}

fn sha256_of(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    Ok(Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn resolve_model_fixture(filename: &str) -> Option<PathBuf> {
    let candidate_paths = [
        format!("tests/fixtures/models/{filename}"),
        format!("../NeuralAmpModeler-rs/tests/fixtures/models/{filename}"),
        format!("NeuralAmpModeler-rs/tests/fixtures/models/{filename}"),
    ];
    for p in &candidate_paths {
        let path = PathBuf::from(p);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

struct ScenarioConfig {
    name: &'static str,
    topology: &'static str,
    fixture_file: Option<&'static str>,
    block_size: usize,
    sample_rate: f64,
}

fn measure_scenario(
    clap_path: &Path,
    config: &ScenarioConfig,
) -> Result<ScenarioPerfMetrics, String> {
    // 1. Dynamic plugin loading from the target distributed .clap
    // SAFETY: Loading dynamic CLAP plugin from certified artifact path.
    let entry = unsafe {
        PluginEntry::load(clap_path).map_err(|e| {
            format!(
                "Failed to load plugin entry from {}: {e:?}",
                clap_path.display()
            )
        })?
    };

    let host_info = HostInfo::new(
        "NAM-Plug Perf Guard Host",
        "Fabio Lima",
        "https://github.com/fabiohl/NAM-Plug",
        "0.1.0",
    )
    .map_err(|e| format!("Failed to create HostInfo: {e:?}"))?;

    let mut plugin_instance = PluginInstance::<PerfHost>::new(
        |_| PerfHostShared,
        |_| (),
        &entry,
        c"br.eti.fabiolima.nam-plug",
        &host_info,
    )
    .map_err(|e| format!("Failed to create plugin instance: {e:?}"))?;

    let (model_path, model_hash) = if let Some(fixture) = config.fixture_file {
        let fixture_path = resolve_model_fixture(fixture)
            .ok_or_else(|| format!("Required fixture '{fixture}' not found"))?;
        let abs_path = std::fs::canonicalize(&fixture_path).map_err(|e| e.to_string())?;
        let hash = sha256_of(&abs_path).ok();
        (Some(abs_path), hash)
    } else {
        (None, None)
    };

    let params = ProcessingParams {
        oversample: OversampleFactor::Off,
        activation_precision: ActivationPrecision::Standard,
        model_path,
        model_hash,
        ..Default::default()
    };

    let state_bytes = serde_json::to_vec(&params).map_err(|e| e.to_string())?;
    if let Some(state_ext) = plugin_instance
        .plugin_handle()
        .get_extension::<PluginState>()
    {
        let mut handle = plugin_instance.plugin_handle();
        state_ext
            .load(&mut handle, &mut state_bytes.as_slice())
            .map_err(|e| format!("Failed to load state: {e:?}"))?;
    }

    // 3. Activate audio processor
    let audio_config = PluginAudioConfiguration {
        sample_rate: config.sample_rate,
        min_frames_count: config.block_size as u32,
        max_frames_count: config.block_size as u32,
    };

    let stopped_processor = plugin_instance
        .activate(|_, _| (), audio_config)
        .map_err(|e| format!("Failed to activate plugin: {e:?}"))?;

    let mut started_processor = stopped_processor
        .start_processing()
        .map_err(|e| format!("Failed to start processing: {e:?}"))?;

    let n = config.block_size;
    let mut in_l = vec![0.1f32; n];
    let mut in_r = vec![0.1f32; n];
    let mut out_l = vec![0.0f32; n];
    let mut out_r = vec![0.0f32; n];

    let mut input_ports = AudioPorts::with_capacity(2, 1);
    let mut output_ports = AudioPorts::with_capacity(2, 1);
    let mut output_events_buffer = EventBuffer::with_capacity(10);

    // Warmup cycles (100 iterations)
    for _ in 0..100 {
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

    // Benchmark measurement (300 iterations)
    const MEASUREMENT_CYCLES: usize = 300;
    let mut latencies_ns = Vec::with_capacity(MEASUREMENT_CYCLES);

    for _ in 0..MEASUREMENT_CYCLES {
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

        let start = Instant::now();
        let status = started_processor.process(
            &input_audio,
            &mut output_audio,
            &input_events,
            &mut output_events,
            None,
            None,
        );
        let elapsed = start.elapsed();
        let _ = std::hint::black_box(status);
        latencies_ns.push(elapsed.as_nanos() as f64);
    }

    let stopped_processor = started_processor.stop_processing();
    plugin_instance.deactivate(stopped_processor);

    // Compute statistics
    latencies_ns.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let count = latencies_ns.len();
    let sum: f64 = latencies_ns.iter().sum();
    let mean_ns = sum / count as f64;

    let p50_idx = ((count as f64) * 0.50).min((count - 1) as f64) as usize;
    let p95_idx = ((count as f64) * 0.95).min((count - 1) as f64) as usize;
    let p99_idx = ((count as f64) * 0.99).min((count - 1) as f64) as usize;

    let p50_ns = latencies_ns[p50_idx];
    let p95_ns = latencies_ns[p95_idx];
    let p99_ns = latencies_ns[p99_idx];
    let max_ns = *latencies_ns.last().unwrap_or(&0.0);

    let variance: f64 = latencies_ns
        .iter()
        .map(|v| (v - mean_ns).powi(2))
        .sum::<f64>()
        / count as f64;
    let stddev_ns = variance.sqrt();
    let ns_per_sample = mean_ns / n as f64;

    // Real-time deadline budget (e.g. at 48kHz, 64 samples = 1,333,333 ns)
    let deadline_budget_ns = (n as f64 / config.sample_rate) * 1_000_000_000.0;
    let deadline_margin_percent = ((deadline_budget_ns - p99_ns) / deadline_budget_ns) * 100.0;
    let deadline_passed = p99_ns < deadline_budget_ns;

    Ok(ScenarioPerfMetrics {
        name: config.name.to_string(),
        topology: config.topology.to_string(),
        sample_rate: config.sample_rate,
        block_size: config.block_size,
        iterations: count,
        p50_ns,
        p95_ns,
        p99_ns,
        max_ns,
        mean_ns,
        stddev_ns,
        ns_per_sample,
        deadline_budget_ns,
        deadline_margin_percent,
        deadline_passed,
    })
}

fn usage() -> ! {
    eprintln!("Usage: nam_perf_guard certify --clap <path-to-.clap> [--out <output.json>]");
    std::process::exit(1);
}

fn main() -> ExitCode {
    NamLogger::init(LoggerConfig {
        level_filter: log::LevelFilter::Info,
        emit_stderr: false,
    })
    .ok();

    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some("certify") {
        usage();
    }

    let mut clap_path: Option<PathBuf> = None;
    let mut out_path: Option<PathBuf> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--clap" => {
                i += 1;
                if i >= args.len() {
                    usage();
                }
                clap_path = Some(PathBuf::from(&args[i]));
            }
            "--out" => {
                i += 1;
                if i >= args.len() {
                    usage();
                }
                out_path = Some(PathBuf::from(&args[i]));
            }
            _ => usage(),
        }
        i += 1;
    }

    let clap_target = clap_path.unwrap_or_else(|| usage());
    if !clap_target.exists() {
        eprintln!(
            "❌ FATAL: Distributed CLAP artifact not found: {}",
            clap_target.display()
        );
        return ExitCode::from(1);
    }

    let clap_sha = match sha256_of(&clap_target) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("❌ FATAL: Failed to hash CLAP artifact: {err}");
            return ExitCode::from(1);
        }
    };

    println!("========================================================================");
    println!("   NAM-Plug Distributed Artifact Performance Certification Gate       ");
    println!("========================================================================");
    println!("CLAP Artifact: {}", clap_target.display());
    println!("SHA-256:       {}", clap_sha);
    println!("------------------------------------------------------------------------");

    let scenarios = [
        ScenarioConfig {
            name: "WaveNet_A1_64_samp",
            topology: "WaveNet A1 Standard",
            fixture_file: Some("wavenet_a1_standard.nam"),
            block_size: 64,
            sample_rate: 48000.0,
        },
        ScenarioConfig {
            name: "WaveNet_A1_128_samp",
            topology: "WaveNet A1 Standard",
            fixture_file: Some("wavenet_a1_standard.nam"),
            block_size: 128,
            sample_rate: 48000.0,
        },
        ScenarioConfig {
            name: "WaveNet_A2_64_samp",
            topology: "WaveNet A2 / Slimmable",
            fixture_file: Some("a2_example.nam"),
            block_size: 64,
            sample_rate: 48000.0,
        },
        ScenarioConfig {
            name: "LSTM_64_samp",
            topology: "LSTM 1x3",
            fixture_file: Some("lstm.nam"),
            block_size: 64,
            sample_rate: 48000.0,
        },
        ScenarioConfig {
            name: "Passthrough_Overhead_64_samp",
            topology: "CLAP Overhead (No Model)",
            fixture_file: None,
            block_size: 64,
            sample_rate: 48000.0,
        },
    ];

    let mut results = Vec::new();
    let mut all_deadlines_passed = true;
    let mut environment_stable = true;

    for sc in &scenarios {
        match measure_scenario(&clap_target, sc) {
            Ok(metrics) => {
                let cv = metrics.stddev_ns / metrics.mean_ns;
                if cv > 1.2 {
                    environment_stable = false;
                }
                if !metrics.deadline_passed {
                    all_deadlines_passed = false;
                }

                println!(
                    "✓ [{}] p50={:.1}µs, p95={:.1}µs, p99={:.1}µs, max={:.1}µs | {:.1} ns/samp | margin={:.1}% (budget={:.1}µs)",
                    metrics.name,
                    metrics.p50_ns / 1000.0,
                    metrics.p95_ns / 1000.0,
                    metrics.p99_ns / 1000.0,
                    metrics.max_ns / 1000.0,
                    metrics.ns_per_sample,
                    metrics.deadline_margin_percent,
                    metrics.deadline_budget_ns / 1000.0,
                );
                results.push(metrics);
            }
            Err(err) => {
                eprintln!("❌ FATAL: Scenario '{}' failed: {err}", sc.name);
                return ExitCode::from(1);
            }
        }
    }

    let overall_status = if !all_deadlines_passed {
        "FAIL"
    } else if !environment_stable {
        "INCONCLUSIVE"
    } else {
        "PASS"
    };

    println!("------------------------------------------------------------------------");
    println!("Performance Gate Result:    {overall_status}");
    println!("Host Environment Stable:    {environment_stable}");
    println!("All Deadlines Respected:    {all_deadlines_passed}");
    println!("========================================================================");

    let report = PerfCertificationReport {
        schema_version: "1.0".to_string(),
        timestamp_utc: chrono_free_timestamp(),
        clap_artifact_path: clap_target.to_string_lossy().to_string(),
        clap_artifact_sha256: clap_sha,
        overall_status: overall_status.to_string(),
        environment_stable,
        total_scenarios: results.len(),
        scenarios: results,
    };

    let target_out =
        out_path.unwrap_or_else(|| PathBuf::from("target/perf-certification-report.json"));
    if let Some(parent) = target_out.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(&report) {
        let _ = fs::write(&target_out, json);
        println!("Report saved to: {}", target_out.display());
    }

    match overall_status {
        "PASS" => ExitCode::SUCCESS,
        "INCONCLUSIVE" => ExitCode::from(2),
        _ => ExitCode::from(1),
    }
}

fn chrono_free_timestamp() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{now}")
}
