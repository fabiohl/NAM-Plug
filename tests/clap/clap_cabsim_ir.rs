// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! S4-T1 (R-08): the production `.so` must load and apply a cab-sim IR.
//!
//! Dynamically loads the freshly built CLAP artifact and asserts that loading
//! an IR measurably changes the audio output versus the dry (no-IR) path. This
//! is the artifact-level evidence that the cabsim path is no longer gated
//! behind `#[cfg(test)]` in the default `cdylib`.

use clack_extensions::state::PluginState;
use clack_host::prelude::*;
use neural_amp_modeler_rs::common::params::ProcessingParams;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

struct TestHostShared {
    _restart_was_called: Arc<AtomicBool>,
}
impl<'a> SharedHandler<'a> for TestHostShared {
    fn request_restart(&self) {}
    fn request_process(&self) {}
    fn request_callback(&self) {}
}

struct TestHost;
impl HostHandlers for TestHost {
    type Shared<'a> = TestHostShared;
    type MainThread<'a> = ();
    type AudioProcessor<'a> = ();
}

/// Creates a plugin instance by loading the freshly built `.so` (release
/// preferred, debug fallback) and recording its SHA256.
fn create_plugin_instance() -> PluginInstance<TestHost> {
    let artifact = super::artifact_validator::TestedArtifact::resolve_and_hash();

    // SAFETY: Dynamic plugin loading from the build artifact.
    let entry = unsafe {
        PluginEntry::load(&artifact.path).expect("Failed to load plugin entry from build artifact")
    };

    let host_info = HostInfo::new(
        "NAM-rs-Test",
        "NAM",
        "https://github.com/fabiohl/nam-rs",
        "0.1.0",
    )
    .expect("Failed to create HostInfo");

    PluginInstance::<TestHost>::new(
        |_| TestHostShared {
            _restart_was_called: Arc::new(AtomicBool::new(false)),
        },
        |_| (),
        &entry,
        c"br.eti.fabiolima.nam-plug",
        &host_info,
    )
    .expect("Failed to instantiate plugin")
}

/// Writes a two-tap FIR impulse response `[1.0, -0.9, 0, ...]`. Its peak is
/// already 1.0 so IR normalization is a no-op; convolution strongly attenuates
/// a DC input (out ≈ 0.1 × in), giving a clear wet-vs-dry difference.
fn write_synthetic_ir(path: &std::path::Path, sample_rate: u32) {
    let mut ir = vec![0.0f32; 512];
    ir[0] = 1.0;
    ir[1] = -0.9;
    neural_amp_modeler_rs::testing::wav::write_wav_f32(path, &ir, sample_rate)
        .expect("failed to write synthetic IR WAV");
}

/// Processes one stereo block and returns the left-channel output RMS.
fn process_block_rms(started: &mut StartedPluginAudioProcessor<TestHost>, n: usize) -> f64 {
    let mut il = vec![0.3f32; n];
    let mut ir = vec![0.3f32; n];
    let mut ol = vec![0.0f32; n];
    let mut or = vec![0.0f32; n];

    let mut input_ports = AudioPorts::with_capacity(2, 1);
    let mut output_ports = AudioPorts::with_capacity(2, 1);
    let mut output_events_buffer = EventBuffer::new();

    let in_ch = [il.as_mut_slice(), ir.as_mut_slice()];
    let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_input_only(
            in_ch.into_iter().map(InputChannel::constant),
        ),
    }]);
    let out_ch = [ol.as_mut_slice(), or.as_mut_slice()];
    let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_output_only(out_ch.into_iter()),
    }]);
    let mut out_ev = OutputEvents::from_buffer(&mut output_events_buffer);

    started
        .process(
            &input_audio,
            &mut output_audio,
            &InputEvents::empty(),
            &mut out_ev,
            None,
            None,
        )
        .expect("process failed");

    let sum_sq: f64 = ol.iter().map(|&x| (x as f64).powi(2)).sum();
    (sum_sq / n as f64).sqrt()
}

fn load_state(instance: &mut PluginInstance<TestHost>, params: &ProcessingParams) {
    let state_ext = instance
        .plugin_handle()
        .get_extension::<PluginState>()
        .expect("PluginState extension not found");
    let state_bytes = serde_json::to_vec(params).unwrap();
    let mut handle = instance.plugin_handle();
    state_ext
        .load(&mut handle, &mut state_bytes.as_slice())
        .expect("state load should succeed");
}

#[test]
#[ignore = "runs in tests-quick.sh Phase 2 against the freshly built release artifact"]
fn test_cabsim_ir_changes_audio_release_artifact() {
    let mut instance = create_plugin_instance();

    let model = crate::common::fixtures::model_path("lstm.nam");
    assert!(model.exists(), "lstm.nam fixture missing");

    let ir_path = std::env::temp_dir().join("nam_plug_cabsim_ir_release.wav");
    write_synthetic_ir(&ir_path, 48000);

    // ── 1. Load model + IR and measure wet RMS ──
    load_state(
        &mut instance,
        &ProcessingParams {
            model_path: Some(model.clone()),
            ir_path: Some(ir_path.clone()),
            ..Default::default()
        },
    );

    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 256,
        max_frames_count: 256,
    };
    let stopped = instance
        .activate(|_, _| (), audio_config)
        .expect("activate with IR should succeed");
    let mut started = stopped.start_processing().expect("start processing");

    // Warm-up: drain model + IR swaps and cabsim partition latency.
    for _ in 0..12 {
        let _ = process_block_rms(&mut started, 256);
    }
    let wet_rms = process_block_rms(&mut started, 256);
    assert!(
        wet_rms > 1e-4,
        "wet output must be non-silent (model + IR active), got {wet_rms:.6}"
    );
    // ── 2. Clear the IR (state without ir_path) and measure dry RMS ──
    load_state(
        &mut instance,
        &ProcessingParams {
            model_path: Some(model),
            ir_path: None,
            ..Default::default()
        },
    );

    for _ in 0..12 {
        let _ = process_block_rms(&mut started, 256);
    }
    let dry_rms = process_block_rms(&mut started, 256);

    // The two-tap IR [1.0, -0.9] attenuates the (near-DC) model output to
    // ~10% — the wet signal must be clearly quieter than the dry signal.
    assert!(
        wet_rms < dry_rms * 0.5,
        "IR must measurably change the audio: wet_rms={wet_rms:.6}, dry_rms={dry_rms:.6}"
    );

    let _ = std::fs::remove_file(&ir_path);
    instance.deactivate(started.stop_processing());
}
