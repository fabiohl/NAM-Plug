// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! CLAP End-to-End Parity against NAMcore across Multiple Sample Rates
//!
//! Dynamically loads the CLAP `.so` artifact, processes stress signals
//! with irregular buffers at 44.1 kHz, 48 kHz, and 96 kHz, and compares the output
//! against the C++ NAMcore oracle using ESR/SNR metrics.
//!
//! # Reference resampling pipeline (`host_sr ≠ model_sr`)
//!
//! The stress signal is generated at the model's native rate (48 kHz) and the
//! C++ oracle always renders at that native rate. The expected host-rate curve
//! is produced with the `NeuralAmpModeler-rs` high-fidelity reference resampler
//! (`NamResampler`, minimum-phase polyphase sinc FIR — the same filter family
//! the plugin's `StreamingResampleBuffer` embeds internally):
//!
//! 1. `stress` is generated at `model_sr` (48 kHz).
//! 2. The host-rate input fed to the CLAP plugin is the reference resample of
//!    `stress` (`model_sr → host_sr`).
//! 3. The oracle model input is the reference resample of that host input back
//!    to `model_sr` — the exact round-trip signal the plugin's input resampler
//!    presents to the model. The input-stage group delay is therefore embedded
//!    in the oracle input and cancels out of the comparison.
//! 4. The C++ oracle renders that model-rate signal; the expected host-rate
//!    curve is the reference resample of the oracle output (`model_sr → host_sr`).
//! 5. Group-delay compensation: the plugin stream zero-primes exactly
//!    `latency_samples()` host samples (its declared resampler latency), while
//!    the one-shot reference filter carries the same output-stage group delay,
//!    so both curves carry the same content timeline at equal host indices.
//!    ESR/SNR are computed on the steady-state window after that latency.
//!
//! // Measured: (2026-07-30) — cross-implementation floor against
//! // real C++ oracle + LUT-based gain (wavenet_a1_standard @ 48 kHz):
//! //   ESR ≈ 1.07e-9, SNR ≈ 89.7 dB (after loudness calibration compensation).
//! // Conservative gate: ESR < 1e-8, SNR > 80 dB.
//! // Re-measured 2026-09-03 with the reference pipeline at all three rates:
//! //   48.0 kHz → ESR ≈ 7.98e-12, SNR ≈ 111.0 dB (native, latency 0)
//! //   44.1 kHz → ESR ≈ 9.01e-12, SNR ≈ 110.5 dB (latency 11)
//! //   96.0 kHz → ESR ≈ 8.13e-12, SNR ≈ 110.9 dB (latency 19)
//! // The resampled rates sit on the same native floor: re-rendering the oracle
//! // over the round-trip model input cancels the sinc interpolation error, so
//! // the gate (ESR < 1e-8, SNR > 80 dB) is uniform across the rate matrix.
//!
//! # Artifact-level oversampling and CabSim coverage (E-D)
//!
//! The NAMCore render tool enforces `input.wav.sample_rate == model.sample_rate`,
//! so it cannot render a model at 2×/4×. The oversampling reference therefore
//! keeps the oracle at the model's native rate but feeds it the exact sequence
//! the plugin's `OversampleEngine` presents to the model: the half-band
//! upsampler output is rendered by the C++ model and then decimated by the same
//! half-band engine. Only the half-band filters are shared with the artifact; the
//! model remains C++ (oracle) vs Rust (artifact), so the comparison is still
//! cross-implementation. Oversampled parity is measured at 48 kHz on
//! `wavenet_a1_standard.nam`.
//!
//! CabSim parity takes the artifact's IR-free output as the pre-convolution
//! reference, convolves it with the same IR using a direct time-domain FIR, and
//! compares it against the artifact's IR-enabled output. Host blocks equal the
//! adapter partition, so steady-state delivery is in-call and needs no quantum
//! shift; the first and last partitions are trimmed as activation/tail
//! boundaries. The IR-free output is itself certified against NAMCore by the
//! matrix above, so the CabSim check isolates the convolution path of the final
//! binary without duplicating an oracle render.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::common::metrics::{compute_esr, compute_snr_db};
use clack_extensions::state::PluginState;
use clack_host::prelude::*;
use neural_amp_modeler_rs::common::params::ProcessingParams;
use neural_amp_modeler_rs::dsp::oversample::{OversampleEngine, OversampleFactor};
use neural_amp_modeler_rs::dsp::resampler::NamResampler;

/// Host `max_frames_count` used by every parity instance. It is also the
/// CabSim adapter's partition size; the CabSim parity test uses it as the host
/// block size so each partition is delivered in-call.
const PARITY_MAX_FRAMES: u32 = 1024;

/// Irregular host block pattern used by the native-rate and oversampling
/// matrices (exercises the streaming adapters across non-aligned boundaries).
const IRREGULAR_BLOCK_SIZES: &[usize] = &[127, 251, 64, 383, 192, 512];

// ═══════════════════════════════════════════════════════════════════════════
// NAMCore C++ oracle helpers
// ═══════════════════════════════════════════════════════════════════════════

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn render_bin() -> PathBuf {
    // Self-contained sibling resolution. Search order:
    //   1. Formal environment contract NAM_CORE_RENDER_BIN.
    //   2. This repo's own `build/namcore_render` (self-contained clone).
    //   3. The `neural_amp_modeler_rs` testing fixture helper, which resolves
    //      the sibling build dir when the dependency is linked as a local path.
    //   4. Sibling `../NeuralAmpModeler-rs/build/namcore_render` — pure
    //      development convenience for the local monorepo workspace. This is
    //      NOT a structural assumption: standalone clones MUST use
    //      NAM_CORE_RENDER_BIN or their own build/namcore_render; when the
    //      sibling is absent this probe is skipped silently.
    if let Ok(val) = std::env::var("NAM_CORE_RENDER_BIN") {
        let p = PathBuf::from(val);
        if p.exists() {
            return p;
        }
        eprintln!("WARN: NAM_CORE_RENDER_BIN is set but path does not exist: {p:?}");
    }

    let root = project_root();
    for candidate in &[
        "build/namcore_render/tools/render",
        "build/namcore_render/Release/render",
        "build/namcore_render/Debug/render",
    ] {
        let p = root.join(candidate);
        if p.exists() {
            return p;
        }
    }
    let build = root.join("build/namcore_render");
    if build.exists() {
        for entry in std::fs::read_dir(&build).into_iter().flatten().flatten() {
            let p = entry.path();
            if p.is_dir() {
                let c = p.join("render");
                if c.exists() {
                    return c;
                }
            }
        }
    }

    // Local path dependency helper: resolves the sibling repo's build dir when
    // neural_amp_modeler_rs is linked as a path dependency (monorepo workspace).
    let core_path = neural_amp_modeler_rs::testing::fixtures::render_bin_path();
    if core_path.exists() {
        return core_path;
    }

    // Sibling-repo fallback — development convenience only. When the dependency
    // is the crates.io registry copy (no build dir shipped) but the sibling repo
    // is present as a local checkout, the helper above cannot find the binary;
    // this last-resort probe recovers it without any structural assumption.
    let sibling_build = root.join("../NeuralAmpModeler-rs/build/namcore_render");
    if sibling_build.exists() {
        for candidate in &["tools/render", "Release/render", "Debug/render"] {
            let p = sibling_build.join(candidate);
            if p.exists() {
                return p;
            }
        }
        for entry in std::fs::read_dir(&sibling_build)
            .into_iter()
            .flatten()
            .flatten()
        {
            let p = entry.path();
            if p.is_dir() {
                let c = p.join("render");
                if c.exists() {
                    return c;
                }
            }
        }
    }

    root.join("build/namcore_render/tools/render")
}

fn oracle_required() -> bool {
    std::env::var("NAM_REQUIRE_CPP_ORACLE")
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn oracle_fail_loud(bin: &Path) {
    if !bin.exists() {
        if oracle_required() {
            panic!(
                "NAM_REQUIRE_CPP_ORACLE=1: NAMCore C++ render binary not found at {bin:?}. \
                 Set NAM_CORE_RENDER_BIN or build via golden_gen_build.sh."
            );
        } else {
            eprintln!(
                "SKIP: NAMCore render binary not found at {bin:?}. \
                 Set NAM_CORE_RENDER_BIN or set NAM_REQUIRE_CPP_ORACLE=1 to fail loud."
            );
        }
    }
}

/// Generates deterministic stress signal for a given sample rate.
fn generate_stress_signal(sample_rate: f64, duration_secs: f64) -> Vec<f32> {
    let n = (sample_rate * duration_secs) as usize;
    let mut signal = Vec::with_capacity(n);
    // Multi-component signal: sin sweep, harmonics, impulse
    let mut phase = 0.0f64;
    let two_pi = std::f64::consts::TAU;
    for i in 0..n {
        let t = i as f64 / sample_rate;
        // Frequency sweep 20 Hz → 2 kHz over duration
        let freq = 20.0 + 1980.0 * (t / duration_secs);
        phase += two_pi * freq / sample_rate;
        // Mix: fundamental sweep + 3rd harmonic + transient at t=0.1s
        let sweep = (phase.sin() * 0.4) as f32;
        let h3 = ((phase * 3.0).sin() * 0.15) as f32;
        let impulse = if (t - 0.1).abs() < 1.0 / sample_rate {
            0.8f32
        } else {
            0.0
        };
        let envelope = (1.0 - (t / duration_secs) * 0.7) as f32;
        let sample = (sweep + h3 + impulse) * envelope;
        signal.push(sample.clamp(-0.95, 0.95));
    }
    signal
}

/// Runs NAMCore C++ render on `wav_in` using `model_path`, writes to `wav_out`.
/// Returns `Err` with the renderer's stderr on non-zero exit (used for oracle
/// capability probing, so an unsupported topology becomes a typed gap instead
/// of a hard panic).
fn try_run_cpp_render(model_path: &Path, wav_in: &Path, wav_out: &Path) -> Result<(), String> {
    std::fs::create_dir_all(wav_out.parent().unwrap()).ok();
    let bin = render_bin();
    if !bin.exists() {
        return Err(format!("render binary not found at {bin:?}"));
    }
    let output = Command::new(&bin)
        .arg(model_path)
        .arg(wav_in)
        .arg(wav_out)
        .output()
        .map_err(|e| format!("failed to execute NAMCore render: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(())
}

fn run_cpp_render(model_path: &Path, wav_in: &Path, wav_out: &Path) {
    if let Err(stderr) = try_run_cpp_render(model_path, wav_in, wav_out) {
        panic!("NAMCore render failed: {stderr}");
    }
}

/// Reads mono WAV f32 samples.
fn read_wav_mono(path: &Path) -> (Vec<f32>, u32) {
    neural_amp_modeler_rs::testing::wav::read_wav_f32(path).expect("Failed to read WAV")
}

/// Writes mono WAV f32 samples.
fn write_wav_mono(path: &Path, samples: &[f32], sample_rate: u32) {
    neural_amp_modeler_rs::testing::wav::write_wav_f32(path, samples, sample_rate)
        .expect("Failed to write WAV")
}

// ═══════════════════════════════════════════════════════════════════════════
// CLAP plugin processing helpers
// ═══════════════════════════════════════════════════════════════════════════

struct ParityHostShared;
impl SharedHandler<'_> for ParityHostShared {
    fn request_restart(&self) {}
    fn request_process(&self) {}
    fn request_callback(&self) {}
}

struct ParityHost;
impl HostHandlers for ParityHost {
    type Shared<'a> = ParityHostShared;
    type MainThread<'a> = ();
    type AudioProcessor<'a> = ();
}

/// Processes `input` through CLAP plugin at `sample_rate` Hz using the given
/// host block sizes, returns output samples.
///
/// `oversample` and `ir_path` exercise the artifact's production DSP branches
/// (half-band oversampling and CabSim convolution); the default matrix uses
/// `Off`/`None`, matching the NAMCore native-rate oracle.
fn process_through_clap(
    model_path: &Path,
    input: &[f32],
    sample_rate: f64,
    oversample: OversampleFactor,
    ir_path: Option<&Path>,
    block_sizes: &[usize],
) -> Vec<f32> {
    let artifact = super::artifact_validator::TestedArtifact::resolve_and_hash();
    // SAFETY: Dynamic loading of .so artifact.
    let entry = unsafe { PluginEntry::load(&artifact.path).expect("Failed to load CLAP plugin") };

    let host_info = HostInfo::new(
        "CLAP-Parity",
        "nam-plug",
        "https://github.com/fabiohl/NAM-Plug",
        "0.1.0",
    )
    .unwrap();

    let mut instance = PluginInstance::<ParityHost>::new(
        |_| ParityHostShared,
        |_| (),
        &entry,
        c"br.eti.fabiolima.nam-plug",
        &host_info,
    )
    .expect("Failed to create CLAP instance");

    // Load model via state
    {
        let mut params = ProcessingParams::default();
        params.model_path = Some(model_path.to_path_buf());
        params.model_hash = nam_plug::clap::test_util::asset_hash(model_path);
        params.input_gain_db = 0.0;
        params.output_gain_db = 0.0;
        params.gate_threshold_db = -90.0; // effectively disabled gate
        params.bypass = false;
        params.oversample = oversample;
        if let Some(ir) = ir_path {
            params.ir_path = Some(ir.to_path_buf());
            params.ir_hash = nam_plug::clap::test_util::asset_hash(ir);
        }
        let state_ext = instance
            .plugin_handle()
            .get_extension::<PluginState>()
            .expect("PluginState extension not found");
        let state_bytes = serde_json::to_vec(&params).expect("Failed to serialize params");
        let handle = instance.plugin_handle();
        state_ext
            .load(&handle, &mut state_bytes.as_slice())
            .expect("Failed to load model state");
    }

    // Activate
    let audio_config = PluginAudioConfiguration {
        sample_rate,
        min_frames_count: 32,
        max_frames_count: PARITY_MAX_FRAMES,
    };

    let stopped = instance
        .activate(|_, _| (), audio_config)
        .expect("Failed to activate CLAP plugin");
    let mut started = stopped
        .start_processing()
        .expect("Failed to start processing");

    // Process in the caller-provided host block pattern
    let mut output = vec![0.0f32; input.len()];
    let mut pos = 0;
    assert!(!block_sizes.is_empty(), "block_sizes must not be empty");
    let mut block_idx = 0;
    let mut event_buffer = EventBuffer::with_capacity(10);

    while pos < input.len() {
        let block = block_sizes[block_idx % block_sizes.len()];
        let end = (pos + block).min(input.len());
        let n = end - pos;

        let mut in_l = vec![0.0f32; n];
        let mut in_r = vec![0.0f32; n];
        in_l.copy_from_slice(&input[pos..end]);
        in_r.copy_from_slice(&input[pos..end]);
        let mut out_l = vec![0.0f32; n];
        let mut out_r = vec![0.0f32; n];

        let mut input_ports = AudioPorts::with_capacity(2, 1);
        let mut output_ports = AudioPorts::with_capacity(2, 1);
        let mut in_ch = [in_l.as_mut_slice(), in_r.as_mut_slice()];
        let out_ch = [out_l.as_mut_slice(), out_r.as_mut_slice()];

        let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(
                in_ch.iter_mut().map(InputChannel::constant),
            ),
        }]);
        let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(out_ch.into_iter()),
        }]);
        let mut out_ev = OutputEvents::from_buffer(&mut event_buffer);

        started
            .process(
                &input_audio,
                &mut output_audio,
                &InputEvents::empty(),
                &mut out_ev,
                None,
                None,
            )
            .expect("process() failed");

        output[pos..end].copy_from_slice(&out_l[..n]);

        pos = end;
        block_idx += 1;
    }

    let stopped = started.stop_processing();
    instance.deactivate(stopped);

    output
}

// ═══════════════════════════════════════════════════════════════════════════
// Helper: get expected sample rate from NAM model metadata
// ═══════════════════════════════════════════════════════════════════════════

fn get_model_sample_rate(model_path: &Path) -> u32 {
    let file = std::fs::File::open(model_path).expect("Failed to open model");
    let reader = std::io::BufReader::new(file);
    let model_data: serde_json::Value =
        serde_json::from_reader(reader).expect("Failed to parse NAM JSON");
    model_data["metadata"]["sample_rate"]
        .as_f64()
        .map(|v| v as u32)
        .or_else(|| {
            model_data["config"]["sample_rate"]
                .as_f64()
                .map(|v| v as u32)
        })
        .or_else(|| model_data["sample_rate"].as_f64().map(|v| v as u32))
        .unwrap_or(48000)
}

// ═══════════════════════════════════════════════════════════════════════════
// Reference resampling oracle helpers
// ═══════════════════════════════════════════════════════════════════════════

/// Offline full-signal resample with the engine's high-fidelity reference
/// resampler (`NamResampler`, minimum-phase polyphase sinc FIR).
///
/// Converts a mono signal from `from_sr` to `to_sr` in one shot. A fresh
/// resampler is built per call so the filter always starts from the same zero
/// state the plugin's adapters have at activation. `from_sr == to_sr` maps to
/// the engine bypass (plain copy, zero added latency). The output buffer is
/// sized with a 512-sample headroom and truncated to the written count,
/// mirroring the engine's own C++-parity fixtures.
fn reference_resample(signal: &[f32], from_sr: u32, to_sr: u32) -> Vec<f32> {
    if from_sr == to_sr {
        return signal.to_vec();
    }
    let mut rs = NamResampler::new_simple(to_sr, from_sr)
        .unwrap_or_else(|e| panic!("reference_resample {from_sr} Hz → {to_sr} Hz failed: {e}"));
    let est = (signal.len() as f64 * to_sr as f64 / from_sr as f64).ceil() as usize + 512;
    let mut out_l = vec![0.0f32; est];
    let mut out_r = vec![0.0f32; est];
    let n = rs
        .process_output_mono(signal, &mut out_l, &mut out_r)
        .samples_written;
    out_l.truncate(n);
    out_l
}

/// Runs the C++ NAMCore oracle over `model_rate_input` (already scaled by the
/// loudness input multiplier) at the model's native `model_rate`, returning the
/// raw model-rate output samples. Temp files are tagged per rate to keep the
/// oracle artifacts of concurrent rates isolated.
fn render_oracle(
    model_path: &Path,
    model_rate: u32,
    model_rate_input: &[f32],
    tmp_dir: &Path,
    tag: &str,
) -> Vec<f32> {
    let stem = model_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("model");
    let stress_wav = tmp_dir.join(format!("oracle_in_{stem}_{tag}.wav"));
    let ref_wav = tmp_dir.join(format!("oracle_out_{stem}_{tag}.wav"));
    write_wav_mono(&stress_wav, model_rate_input, model_rate);
    run_cpp_render(model_path, &stress_wav, &ref_wav);
    let (samples, _sr) = read_wav_mono(&ref_wav);
    samples
}

/// Declared latency (host-rate samples) that the plugin's streaming resample
/// adapter zero-primes when `host_sr != model_sr` — the minimum host index at
/// which the delivered waveform carries real content. Zero when the rates
/// match (the engine resampler fully bypasses).
fn resampler_latency_samples(host_sr: u32, model_sr: u32) -> usize {
    if host_sr == model_sr {
        return 0;
    }
    NamResampler::new_simple(host_sr, model_sr)
        .map(|r| r.latency_samples(host_sr) as usize)
        .unwrap_or(0)
}

/// Per-rate parity gates: `(esr_max, snr_min_db, measured_comment)`.
///
/// All rates share the native-floor gates because the reference pipeline
/// re-renders the oracle over the exact round-trip model input — the sinc
/// interpolation error cancels and the resampled rates measure at the same
/// cross-implementation float floor as 48 kHz native.
fn parity_gates(model_name: &str, host_sr: f64) -> (f64, f64, &'static str) {
    let host_u32 = host_sr.round() as u32;
    // Empirically measured floor against NAMCore C++ oracle across certified topologies:
    // - wavenet_a1_standard: ESR 7.98e-12 @ 48k, 9.01e-12 @ 44.1k, 8.13e-12 @ 96k
    // - a2_example:         ESR 2.75e-12 @ 48k, 2.77e-12 @ 44.1k, 2.76e-12 @ 96k
    // - lstm:               ESR 2.05e-11 @ 48k, 1.91e-11 @ 44.1k, 2.03e-11 @ 96k
    let comment = match (model_name, host_u32) {
        ("wavenet_a1_standard.nam", 48_000) => "ESR ≈ 7.98e-12, SNR ≈ 111.0 dB",
        ("wavenet_a1_standard.nam", 44_100) => "ESR ≈ 9.01e-12, SNR ≈ 110.5 dB",
        ("wavenet_a1_standard.nam", 96_000) => "ESR ≈ 8.13e-12, SNR ≈ 110.9 dB",
        ("a2_example.nam", 48_000) => "ESR ≈ 2.75e-12, SNR ≈ 115.6 dB",
        ("a2_example.nam", 44_100) => "ESR ≈ 2.77e-12, SNR ≈ 115.6 dB",
        ("a2_example.nam", 96_000) => "ESR ≈ 2.76e-12, SNR ≈ 115.6 dB",
        ("lstm.nam", 48_000) => "ESR ≈ 2.05e-11, SNR ≈ 106.9 dB",
        ("lstm.nam", 44_100) => "ESR ≈ 1.91e-11, SNR ≈ 107.2 dB",
        ("lstm.nam", 96_000) => "ESR ≈ 2.03e-11, SNR ≈ 106.9 dB",
        _ => "default parity gate (native floor)",
    };
    (1e-8, 80.0, comment)
}

// ═══════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════

/// Outcome of a parity matrix cell group. `OracleUnsupported` is a typed gap,
/// not a silent skip: the caller aggregates it and fails the suite unless the
/// fixture is explicitly known to be unsupported by the render tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParityOutcome {
    Passed,
    OracleUnsupported,
}

/// Verifies CLAP plugin parity against the NAMCore C++ oracle at every host
/// rate in `host_rates`, with irregular buffer sizes.
///
/// For `host_sr == model_sr` the oracle renders the native-rate stress
/// directly. For `host_sr != model_sr` the reference-resampling pipeline
/// described in the module docs is used: the plugin input and the oracle model
/// input both derive from the native stress through the `NamResampler`
/// high-fidelity reference resampler, so the input-stage group delay cancels
/// and both curves are content-aligned at equal host indices after the
/// plugin's declared resampler latency is skipped.
fn run_multi_rate_parity(
    model_name: &str,
    host_rates: &[f64],
    stress_duration: f64,
) -> ParityOutcome {
    let bin = render_bin();
    if !bin.exists() {
        oracle_fail_loud(&bin);
        return ParityOutcome::OracleUnsupported;
    }

    let model_path = crate::common::fixtures::model_path(model_name);

    let (input_mult_adj, output_mult_adj) =
        neural_amp_modeler_rs::testing::fixtures::calibration_multipliers_from_model_json(
            &model_path,
        );
    eprintln!(
        "  Calibration: input_mult_adj={input_mult_adj:.6}, output_mult_adj={output_mult_adj:.6}"
    );

    let model_sr = get_model_sample_rate(&model_path);
    let tmp_dir = std::env::temp_dir().join("nam_plug_clap_parity");
    std::fs::create_dir_all(&tmp_dir).ok();

    // Oracle capability probe (E-D.1): the render tool rejects unsupported
    // topologies before any real signal is rendered. Probe with silence so a
    // genuine DSP failure on the real signal still fails hard below.
    let probe_in = tmp_dir.join(format!("oracle_probe_{model_name}_in.wav"));
    let probe_out = tmp_dir.join(format!("oracle_probe_{model_name}_out.wav"));
    write_wav_mono(&probe_in, &[0.0f32; 64], model_sr);
    if let Err(err) = try_run_cpp_render(&model_path, &probe_in, &probe_out) {
        eprintln!("GAP: oracle_unsupported:{model_name} ({err})");
        return ParityOutcome::OracleUnsupported;
    }

    let mut all_passed = true;

    // Stress is generated once at the model NATIVE rate: every host rate is
    // derived from this same signal, keeping the model operating point
    // comparable across the whole rate matrix.
    let stress = generate_stress_signal(model_sr as f64, stress_duration);
    eprintln!(
        "  Fixture {model_name}: native rate {model_sr} Hz, {} stress samples",
        stress.len()
    );

    for &host_sr in host_rates {
        eprintln!("\n=== CLAP Parity: {model_name} @ {host_sr:.0} Hz ===");

        let host_u32 = host_sr.round() as u32;
        let resampled = (host_sr - model_sr as f64).abs() > 0.1;

        // Host-rate signal fed to the CLAP plugin (identity at the native rate).
        let plugin_input = if resampled {
            reference_resample(&stress, model_sr, host_u32)
        } else {
            stress.clone()
        };
        eprintln!(
            "  Plugin input: {} samples @ {host_sr:.0} Hz",
            plugin_input.len()
        );

        // The exact model-rate signal the plugin's input resampler presents to
        // the model. The oracle re-renders THIS signal (not the raw native
        // stress) so the input-stage group delay + interpolation artifacts are
        // embedded in the reference and cancel out of the comparison.
        let oracle_model_input = if resampled {
            reference_resample(&plugin_input, host_u32, model_sr)
        } else {
            plugin_input.clone()
        };

        // Mirror the plugin's internal DSP pipeline:
        //   - Oracle receives model input × input_mult_adj (the plugin applies
        //     the loudness input multiplier before the model).
        //   - Oracle output × output_mult_adj ≈ plugin output (loudness-
        //     normalized); the constant output multiplier commutes with the
        //     (linear) reference resampler.
        let scaled_oracle_input: Vec<f32> = oracle_model_input
            .iter()
            .map(|s| s * input_mult_adj)
            .collect();
        let tag = format!("{model_sr}_{host_u32}");
        let raw_cpp = render_oracle(&model_path, model_sr, &scaled_oracle_input, &tmp_dir, &tag);
        eprintln!(
            "  Oracle model input: {} samples @ {model_sr} Hz → raw output: {} samples",
            oracle_model_input.len(),
            raw_cpp.len()
        );

        let calibrated_cpp: Vec<f32> = raw_cpp.iter().map(|s| s * output_mult_adj).collect();

        // Expected host-rate curve: the reference resampler of the model-rate
        // oracle output (identity at the native rate).
        let reference = reference_resample(&calibrated_cpp, model_sr, host_u32);
        eprintln!("  Reference: {} samples @ {host_sr:.0} Hz", reference.len());

        let clap_output = process_through_clap(
            &model_path,
            &plugin_input,
            host_sr,
            OversampleFactor::Off,
            None,
            IRREGULAR_BLOCK_SIZES,
        );

        // Group-delay compensation: the streaming adapter zero-primes exactly
        // `latency_samples()` host samples, and the one-shot reference filter
        // (identical output-stage design) has its warm-up inside those same
        // samples. Compare the steady-state window only, with a small edge
        // guard on both ends for the resampler's fractional-phase rounding.
        let latency = resampler_latency_samples(host_u32, model_sr);
        const EDGE_GUARD: usize = 16;
        let skip = latency + EDGE_GUARD;
        let common = reference
            .len()
            .min(clap_output.len())
            .saturating_sub(skip + EDGE_GUARD);
        eprintln!(
            "  Resampler latency: {latency} host samples (skip {skip}, compare window {common})"
        );
        assert!(
            common > 64,
            "Steady-state comparison window too small ({common} samples) at {host_sr:.0} Hz — \
             oracle output may have diverged in length"
        );
        assert!(
            reference.len().abs_diff(clap_output.len()) <= 8,
            "Reference ({}) vs CLAP output ({}) length mismatch at {host_sr:.0} Hz",
            reference.len(),
            clap_output.len()
        );

        let ref_win = &reference[skip..skip + common];
        let clap_win = &clap_output[skip..skip + common];
        let esr = compute_esr(ref_win, clap_win);
        let snr = compute_snr_db(ref_win, clap_win);
        let esr_db = if esr > 0.0 {
            10.0 * (1.0 / esr).log10()
        } else {
            f64::INFINITY
        };

        let (esr_gate, snr_gate, gate_comment) = parity_gates(model_name, host_sr);
        eprintln!(
            "  ESR  = {esr:.2e}  ({:.1} dB)  [threshold < {esr_gate:.0e}]",
            esr_db
        );
        eprintln!("  SNR  = {snr:.1} dB                   [threshold > {snr_gate} dB]");
        eprintln!("  Gate basis: {gate_comment}");

        let esr_pass = esr < esr_gate;
        let snr_pass = snr > snr_gate;
        let pass = esr_pass && snr_pass;

        if !pass {
            all_passed = false;
        }

        eprintln!(
            "  {} ESR={} SNR={}",
            if pass { "PASS" } else { "FAIL" },
            if esr_pass { "✓" } else { "✗" },
            if snr_pass { "✓" } else { "✗" },
        );
    }

    assert!(all_passed, "CLAP parity failed for {model_name}");
    ParityOutcome::Passed
}

/// Fixtures certified by the multi-rate matrix (E-D.1). They cover the three
/// production topologies: WaveNet A1, WaveNet A2/Slimmable container, and LSTM.
const PARITY_FIXTURES: &[&str] = &["wavenet_a1_standard.nam", "a2_example.nam", "lstm.nam"];

/// Multi-rate CLAP vs NAMCore parity matrix with irregular buffers.
///
/// Runs every fixture in [`PARITY_FIXTURES`] at the three Gate-4 certification
/// rates: 44.1 kHz, 48 kHz (native), and 96 kHz. At the native rate the
/// plugin's resampler fully bypasses; at 44.1/96 kHz the reference-resampling
/// oracle pipeline described in the module docs is used. Applies loudness
/// calibration compensation (input/output_mult_adj via gain LUT) mirroring the
/// plugin DSP chain, so residuals reflect only actual DSP divergence.
///
/// The renderer is probed once per fixture; an unsupported topology becomes
/// the typed gap `oracle_unsupported:<fixture>` instead of an ambiguous crash.
///
/// // Measured: (2026-07-30) — cross-implementation floor
/// //   ESR ≈ 1.07e-9, SNR ≈ 89.7 dB (48 kHz native) → conservative gates:
/// //   ESR < 1e-8, SNR > 80 dB. Resampled-rate gates calibrated on
/// //   2026-09-03 (see `parity_gates()`).
///
/// This test is `#[ignore]` by default because it requires:
/// - NAMCore C++ render binary (build via golden_gen_build.sh)
/// - A release-build CLAP `.so` artifact
#[test]
#[ignore = "requires NAMCore C++ render + release CLAP .so"]
fn test_clap_parity_multi_rate() {
    let bin = render_bin();
    if !bin.exists() {
        oracle_fail_loud(&bin);
        return;
    }

    let mut gaps = Vec::new();
    for &fixture in PARITY_FIXTURES {
        eprintln!("\n════════ Fixture parity matrix: {fixture} ════════");
        if run_multi_rate_parity(fixture, &[44100.0, 48000.0, 96000.0], 0.5)
            == ParityOutcome::OracleUnsupported
        {
            gaps.push(fixture);
        }
    }

    assert!(
        gaps.is_empty(),
        "typed oracle gap(s) — render tool does not support: {gaps:?}. \
         Install/provide a render build with the required topology support, or \
         record the gap explicitly in docs/testing.md before certifying."
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Oversampling reference (E-D.2)
// ═══════════════════════════════════════════════════════════════════════════

/// Native block size used by the reference half-band engine. The engine is
/// streaming, so this only bounds scratch buffers, not the result.
const REFERENCE_OS_BLOCK: usize = 8192;

/// Reference half-band upsampling of a whole native-rate signal.
fn os_upsample_all(
    signal: &[f32],
    factor: OversampleFactor,
    engine: &mut OversampleEngine,
) -> Vec<f32> {
    let mult = factor.multiplier();
    let mut up = Vec::with_capacity(signal.len() * mult);
    let mut scratch = vec![0.0f32; REFERENCE_OS_BLOCK * mult];
    for chunk in signal.chunks(REFERENCE_OS_BLOCK) {
        let n = engine.upsample(chunk, &mut scratch[..chunk.len() * mult], None);
        up.extend_from_slice(&scratch[..n]);
    }
    up
}

/// Reference half-band decimation of a whole oversampled signal.
fn os_downsample_all(
    signal: &[f32],
    factor: OversampleFactor,
    engine: &mut OversampleEngine,
) -> Vec<f32> {
    let mult = factor.multiplier();
    let aligned = signal.len() - (signal.len() % mult);
    let mut down = Vec::with_capacity(aligned / mult + 1);
    let mut scratch = vec![0.0f32; REFERENCE_OS_BLOCK];
    for chunk in signal[..aligned].chunks(REFERENCE_OS_BLOCK * mult) {
        let n = engine.downsample(chunk, &mut scratch[..chunk.len() / mult], None);
        down.extend_from_slice(&scratch[..n]);
    }
    down
}

/// Artifact-level oversampling parity at the model native rate.
///
/// The C++ renderer cannot run at 2×/4×: it rejects an input whose sample rate
/// differs from the model's expected rate. The reference therefore presents to
/// the oracle the exact oversampled sequence the artifact feeds its model and
/// decimates the oracle output with the same half-band engine the artifact
/// embeds. The neural model remains cross-implementation (C++ oracle vs Rust),
/// so this certifies the artifact's production oversampling branch, not just
/// the engine unit tests.
///
/// // Measured: (2026-10-22, wavenet_a1_standard @ 48 kHz) — see the test log.
/// // Gate: ESR < 1e-8, SNR > 80 dB (same cross-implementation floor as native).
fn run_oversample_parity(model_name: &str, factor: OversampleFactor) {
    let bin = render_bin();
    if !bin.exists() {
        oracle_fail_loud(&bin);
        return;
    }

    let model_path = crate::common::fixtures::model_path(model_name);
    let model_sr = get_model_sample_rate(&model_path);
    let (input_mult_adj, output_mult_adj) =
        neural_amp_modeler_rs::testing::fixtures::calibration_multipliers_from_model_json(
            &model_path,
        );
    let stress = generate_stress_signal(model_sr as f64, 0.5);
    let tmp_dir = std::env::temp_dir().join("nam_plug_clap_parity");
    std::fs::create_dir_all(&tmp_dir).ok();

    let scaled_input: Vec<f32> = stress.iter().map(|s| s * input_mult_adj).collect();
    let mut os = OversampleEngine::new(factor, REFERENCE_OS_BLOCK)
        .unwrap_or_else(|e| panic!("reference OversampleEngine::new({factor:?}) failed: {e}"));
    let upsampled = os_upsample_all(&scaled_input, factor, &mut os);

    let tag = format!("os{}_{model_sr}", factor.multiplier());
    let raw_cpp = render_oracle(&model_path, model_sr, &upsampled, &tmp_dir, &tag);
    let calibrated_cpp: Vec<f32> = raw_cpp.iter().map(|s| s * output_mult_adj).collect();
    let reference = os_downsample_all(&calibrated_cpp, factor, &mut os);

    let clap_output = process_through_clap(
        &model_path,
        &stress,
        model_sr as f64,
        factor,
        None,
        IRREGULAR_BLOCK_SIZES,
    );

    let latency = os.latency_samples();
    const EDGE_GUARD: usize = 16;
    let skip = latency + EDGE_GUARD;
    let common = reference
        .len()
        .min(clap_output.len())
        .saturating_sub(skip + EDGE_GUARD);
    assert!(
        common > 64,
        "Oversample {factor:?} comparison window too small ({common} samples)"
    );
    assert!(
        reference.len().abs_diff(clap_output.len()) <= 8,
        "Oversample {factor:?} length mismatch: reference {} vs artifact {}",
        reference.len(),
        clap_output.len()
    );

    let esr = compute_esr(
        &reference[skip..skip + common],
        &clap_output[skip..skip + common],
    );
    let snr = compute_snr_db(
        &reference[skip..skip + common],
        &clap_output[skip..skip + common],
    );
    let esr_db = if esr > 0.0 {
        10.0 * (1.0 / esr).log10()
    } else {
        f64::INFINITY
    };
    const ESR_MAX: f64 = 1e-8;
    const SNR_MIN_DB: f64 = 80.0;
    eprintln!(
        "=== CLAP Parity: {model_name} @ {model_sr} Hz, oversample {factor:?} ===\n  \
         Reference OS latency: {latency} native samples (skip {skip}, window {common})\n  \
         ESR = {esr:.2e} ({esr_db:.1} dB)  [threshold < {ESR_MAX:.0e}]\n  \
         SNR = {snr:.1} dB                 [threshold > {SNR_MIN_DB} dB]"
    );
    assert!(
        esr < ESR_MAX && snr > SNR_MIN_DB,
        "Oversample {factor:?} parity failed: ESR={esr:.2e} ({esr_db:.1} dB), \
         SNR={snr:.1} dB"
    );
    eprintln!("  PASS oversample {factor:?} ESR={esr:.2e} SNR={snr:.1}");
}

/// Artifact-level oversampling parity (2× and 4×) on the final `.so`.
#[test]
#[ignore = "requires NAMCore C++ render + release CLAP .so"]
fn test_clap_parity_oversample() {
    run_oversample_parity("wavenet_a1_standard.nam", OversampleFactor::X2);
    run_oversample_parity("wavenet_a1_standard.nam", OversampleFactor::X4);
}

// ═══════════════════════════════════════════════════════════════════════════
// CabSim reference (E-D.2)
// ═══════════════════════════════════════════════════════════════════════════

/// Direct time-domain FIR convolution used as an independent CabSim reference.
fn direct_convolution(input: &[f32], ir: &[f32]) -> Vec<f32> {
    let mut out = vec![0.0f32; input.len() + ir.len() - 1];
    for (i, &x) in input.iter().enumerate() {
        if x == 0.0 {
            continue;
        }
        for (k, &h) in ir.iter().enumerate() {
            out[i + k] += x * h;
        }
    }
    out
}

/// Deterministic, strictly causal synthetic cab IR (decaying multi-tap).
fn synthetic_cab_ir() -> Vec<f32> {
    let taps = 256usize;
    let mut ir = vec![0.0f32; taps];
    ir[0] = 0.6;
    ir[1] = -0.35;
    ir[2] = 0.22;
    for (k, h) in ir.iter_mut().enumerate().skip(3) {
        let t = k as f32;
        *h = 0.35 * (-t / 40.0).exp() * (t * 0.7).sin();
    }
    let peak = ir.iter().fold(0.0f32, |acc, v| acc.max(v.abs()));
    if peak > 0.0 {
        for h in &mut ir {
            *h /= peak;
        }
    }
    ir
}

/// Artifact-level CabSim parity: the IR-enabled output must equal the direct
/// convolution of the IR-free artifact output with the same IR, after the
/// adapter's documented one-partition latency.
///
/// This isolates the final binary's convolution path; the IR-free curve is
/// certified separately against NAMCore by `test_clap_parity_multi_rate`.
///
/// // Measured: (2026-10-22, wavenet_a1_standard @ 48 kHz) — see the test log.
/// // Gate: ESR < 1e-8, SNR > 80 dB.
#[test]
#[ignore = "requires release CLAP .so"]
fn test_clap_parity_cabsim() {
    let model_name = "wavenet_a1_standard.nam";
    let model_path = crate::common::fixtures::model_path(model_name);
    let model_sr = get_model_sample_rate(&model_path);
    assert_eq!(
        model_sr, 48000,
        "CabSim parity fixture must be native 48 kHz"
    );

    let ir = synthetic_cab_ir();
    let ir_path = std::env::temp_dir().join("nam_plug_cabsim_parity_ir.wav");
    write_wav_mono(&ir_path, &ir, model_sr);

    // Warm-up prefix: gives the asynchronous IR load time to land before the
    // measured signal starts. Host blocks equal the adapter partition, so once
    // the IR is active the adapter renders each partition in-call and the
    // output is aligned sample-for-sample with a direct convolution of the
    // dry stream (no steady-state quantum latency). The prefix is zeros, so the
    // FIR state at the measurement boundary is zero regardless of when the IR
    // load landed.
    let warmup = 32 * PARITY_MAX_FRAMES as usize;
    let mut padded = vec![0.0f32; warmup];
    padded.extend_from_slice(&generate_stress_signal(model_sr as f64, 0.5));
    let partition_blocks = [PARITY_MAX_FRAMES as usize];

    let dry = process_through_clap(
        &model_path,
        &padded,
        model_sr as f64,
        OversampleFactor::Off,
        None,
        &partition_blocks,
    );
    let wet = process_through_clap(
        &model_path,
        &padded,
        model_sr as f64,
        OversampleFactor::Off,
        Some(&ir_path),
        &partition_blocks,
    );
    let dry_sig = &dry[warmup..];
    let wet_sig = &wet[warmup..];
    let reference = direct_convolution(dry_sig, &ir);
    let rms = |x: &[f32]| {
        (x.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>() / x.len() as f64).sqrt()
    };
    eprintln!(
        "  Reference RMS: dry={:.6} wet={:.6} direct-conv={:.6}",
        rms(dry_sig),
        rms(wet_sig),
        rms(&reference)
    );

    // With host blocks equal to the adapter partition, delivery is in-call:
    // no steady-state quantum latency remains. The edge guard trims the
    // boundary where the last warm-up zeros leave the FIR.
    const EDGE_GUARD: usize = 16;
    let partition = PARITY_MAX_FRAMES as usize;
    // The final partial host block has no queued convolution output (the
    // adapter delivered only complete partitions), and the first measured
    // partition is excluded conservatively as an activation boundary.
    let measurable = wet_sig.len() - (wet_sig.len() % partition);
    let skip = partition + EDGE_GUARD;
    let common = reference
        .len()
        .min(measurable)
        .saturating_sub(skip + EDGE_GUARD);
    assert!(
        common > 64,
        "CabSim comparison window too small ({common} samples)"
    );

    let esr = compute_esr(
        &reference[skip..skip + common],
        &wet_sig[skip..skip + common],
    );
    let snr = compute_snr_db(
        &reference[skip..skip + common],
        &wet_sig[skip..skip + common],
    );
    let esr_db = if esr > 0.0 {
        10.0 * (1.0 / esr).log10()
    } else {
        f64::INFINITY
    };
    const ESR_MAX: f64 = 1e-8;
    const SNR_MIN_DB: f64 = 80.0;
    eprintln!(
        "=== CLAP Parity: {model_name} @ {model_sr} Hz, CabSim ===\n  \
         Partition blocks: {PARITY_MAX_FRAMES} samples (warmup {warmup}, window {common})\n  \
         ESR = {esr:.2e} ({esr_db:.1} dB)  [threshold < {ESR_MAX:.0e}]\n  \
         SNR = {snr:.1} dB                 [threshold > {SNR_MIN_DB} dB]"
    );
    if !(esr < ESR_MAX && snr > SNR_MIN_DB) {
        // Alignment diagnostics only on failure: report the best integer lag
        // so a quantum/partition regression is immediately actionable.
        let seg = 4096usize.min(reference.len()).min(wet_sig.len());
        let mut best = (0usize, f64::INFINITY);
        for lag in 0..=PARITY_MAX_FRAMES as usize {
            if lag + seg > wet_sig.len() {
                break;
            }
            let e: f64 = (0..seg)
                .map(|i| {
                    let d = reference[i] as f64 - wet_sig[lag + i] as f64;
                    d * d
                })
                .sum();
            if e < best.1 {
                best = (lag, e);
            }
        }
        panic!(
            "CabSim parity failed: ESR={esr:.2e} ({esr_db:.1} dB), SNR={snr:.1} dB; \
             best lag={} (mse {:.3e})",
            best.0, best.1
        );
    }
    eprintln!("  PASS cabsim ESR={esr:.2e} SNR={snr:.1}");

    let _ = std::fs::remove_file(&ir_path);
}

/// Quick smoke test: processes a tiny signal through the CLAP plugin
/// and verifies the output is finite and non-trivial. Runs without
/// NAMCore C++ dependency.
#[test]
fn test_clap_parity_smoke() {
    let model_path = crate::common::fixtures::model_path("wavenet_a1_standard.nam");

    let stress = generate_stress_signal(48000.0, 0.1);
    assert!(!stress.is_empty());
    assert!(stress.iter().any(|&s| s.abs() > 0.01));

    let output = process_through_clap(
        &model_path,
        &stress,
        48000.0f64,
        OversampleFactor::Off,
        None,
        IRREGULAR_BLOCK_SIZES,
    );

    assert_eq!(output.len(), stress.len());
    assert!(
        output.iter().all(|s| s.is_finite()),
        "Output contains non-finite samples"
    );
    assert!(
        output.iter().any(|&s| s.abs() > 1e-8),
        "Output is effectively silent — model may not be processing"
    );

    eprintln!(
        "  ✓ CLAP smoke: {} samples, output is finite and non-trivial.",
        output.len()
    );
}
