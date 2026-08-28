// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Behavioral Containment and State Fidelity Integration Tests.
//!
//! Regression test suite verifying crucial edge behaviors and invariant containment:
//! - CabSim IR participation in audio path and latency reporting.
//! - Asset failure transactional containment (fail without side effects).
//! - Offline rendering activation precision preservation across mode cycles.
//! - Active bypass event automation and single-quantum responsiveness.
//! - Atypical large audio blocks (>8192 frames) processing without buffer truncation.
//! - Diagnostic log fidelity (oversampling state truthfulness).

use clack_extensions::render::{PluginRender, RenderMode};
use clack_host::prelude::*;
use nam_plug::clap::test_util;
use neural_amp_modeler_rs::common::params::ProcessingParams;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

// ── Helpers ────────────────────────────────────────────────────────────────

fn model_fixture(name: &str) -> PathBuf {
    test_util::model_path(name)
}

fn make_synthetic_ir(len: usize) -> Vec<f32> {
    let mut ir = vec![0.0f32; len];
    ir[0] = 0.8;
    ir[1] = 0.5;
    ir[2] = 0.3;
    ir[3] = 0.1;
    ir
}

fn max_abs_diff(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (x - y).abs())
        .fold(0.0f32, f32::max)
}

fn process_block(
    started: &mut StartedPluginAudioProcessor<test_util::TestHost>,
    in_l: &mut [f32],
    in_r: &mut [f32],
    out_l: &mut [f32],
    out_r: &mut [f32],
    events: &InputEvents<'_>,
) -> EventBuffer {
    let mut input_ports = AudioPorts::with_capacity(2, 1);
    let mut output_ports = AudioPorts::with_capacity(2, 1);

    let mut in_ch = [in_l, in_r];
    let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_input_only(in_ch.iter_mut().map(InputChannel::constant)),
    }]);
    let out_ch = [out_l, out_r];
    let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_output_only(out_ch.into_iter()),
    }]);
    let mut output_events_buffer = EventBuffer::new();
    let mut out_ev = OutputEvents::from_buffer(&mut output_events_buffer);

    started
        .process(
            &input_audio,
            &mut output_audio,
            events,
            &mut out_ev,
            None,
            None,
        )
        .expect("process() failed");

    output_events_buffer
}

// ── Test: CabSim IR Audio Participation and Latency Reporting ─────────────
//
// The GUI and state loader build a ConvEngine and send it via SPSC.
// The orchestrator injects `conv` into DspPipelineContext, and run_inference()
// applies convolution. The reported latency must reflect the CabSim delay.

#[test]
fn test_cabsim_loaded_and_applied_to_audio() {
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

    let partition_size: usize = 256;

    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: partition_size as u32,
        max_frames_count: partition_size as u32,
    };

    // ── Pass 1: activate without IR, process, capture baseline ──
    let stopped1 = plugin_instance.activate(|_, _| (), audio_config).unwrap();
    let mut started1 = stopped1.start_processing().unwrap();

    let n = partition_size;
    let mut baseline_l = vec![0.0f32; n];
    let mut baseline_r = vec![0.0f32; n];
    let in_l: Vec<f32> = vec![0.3f32; n];
    let in_r: Vec<f32> = vec![0.3f32; n];
    {
        let mut il = in_l.clone();
        let mut ir = in_r.clone();
        let _ = process_block(
            &mut started1,
            &mut il,
            &mut ir,
            &mut baseline_l,
            &mut baseline_r,
            &InputEvents::empty(),
        );
    }
    let baseline_latency = shared.rt_to_ui.current_latency.load(Ordering::Relaxed);

    plugin_instance.deactivate(started1.stop_processing());

    // ── Pass 2: activate WITH IR, process, capture output ──
    let ir = make_synthetic_ir(128);
    {
        let mut raw = shared.cold.ir_raw_samples.lock().unwrap();
        *raw = Some(ir);
    }

    let stopped2 = plugin_instance.activate(|_, _| (), audio_config).unwrap();
    let mut started2 = stopped2.start_processing().unwrap();

    let ir_latency = shared.rt_to_ui.current_latency.load(Ordering::Relaxed);

    let mut ir_out_l = vec![0.0f32; n];
    let mut ir_out_r = vec![0.0f32; n];
    {
        let mut il = in_l.clone();
        let mut ir = in_r.clone();
        let _ = process_block(
            &mut started2,
            &mut il,
            &mut ir,
            &mut ir_out_l,
            &mut ir_out_r,
            &InputEvents::empty(),
        );
    }

    plugin_instance.deactivate(started2.stop_processing());

    // Clean IR state for subsequent tests
    if let Ok(mut raw) = shared.cold.ir_raw_samples.lock() {
        *raw = None;
    }

    // ── Assertions ──

    // Latency must include CabSim delay when convolution is applied.
    assert_eq!(
        ir_latency,
        baseline_latency + 256,
        "current_latency must include CabSim latency ({ir_latency} vs expected {})",
        baseline_latency + 256
    );

    // Audio verification: IR-loaded output must differ from no-IR baseline output.
    let diff = max_abs_diff(&ir_out_l, &baseline_l);
    assert!(
        diff > 1e-4,
        "IR-loaded output must differ from no-IR output (diff={diff}). Convolution must be actively applied to the audio buffer."
    );
}

// ── Test: Asset failure during state restore keeps previous DSP ──────────
//
// State is validated before committing to parameters and atomics.
// If the path does not exist, loading fails with an error and maintains the previous state
// intact — without altering DSP, parameters, or UI.
// Transactional pipeline ensures "fail without side-effects".

#[test]
fn test_state_restore_with_missing_model_fails_and_keeps_old_dsp() {
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

    let model_a = model_fixture("lstm.nam");

    // ── Step 1: Load valid model A via state ──
    let params_a = ProcessingParams {
        model_path: Some(model_a.clone()),
        model_basename: Some("lstm.nam".into()),
        model_hash: test_util::asset_hash(&model_a),
        input_gain_db: 0.0,
        output_gain_db: 0.0,
        gate_threshold_db: -90.0,
        bypass: false,
        ..Default::default()
    };

    {
        let state_ext = test_util::get_state_ext(&mut plugin_instance);
        let state_a = serde_json::to_vec(&params_a).unwrap();
        let mut handle = plugin_instance.plugin_handle();
        state_ext
            .load(&mut handle, &mut state_a.as_slice())
            .expect("failed to load model A via state");
    }

    // ── Step 2: Activate, process blocks so DSP materialises ──
    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 256,
        max_frames_count: 256,
    };

    let stopped = plugin_instance.activate(|_, _| (), audio_config).unwrap();
    let mut started = stopped.start_processing().unwrap();

    let n: usize = 256;
    for _ in 0..4 {
        let mut il = vec![0.3f32; n];
        let mut ir = vec![0.3f32; n];
        let mut ol = vec![0.0f32; n];
        let mut or = vec![0.0f32; n];
        let _ = process_block(
            &mut started,
            &mut il,
            &mut ir,
            &mut ol,
            &mut or,
            &InputEvents::empty(),
        );
    }

    // Save model A's name for later comparison
    let model_a_name = shared.cold.ui_model_name.lock().unwrap().clone();

    // ── Step 3: Attempt to load nonexistent model B via state ──
    let missing_path = PathBuf::from("/nonexistent/model_b.nam");
    let params_b = ProcessingParams {
        model_path: Some(missing_path),
        model_basename: Some("model_b.nam".into()),
        model_search_paths: vec![],
        input_gain_db: params_a.input_gain_db,
        output_gain_db: params_a.output_gain_db,
        gate_threshold_db: params_a.gate_threshold_db,
        bypass: false,
        adaptive_compute: params_a.adaptive_compute,
        slim_override: params_a.slim_override,
        oversample: params_a.oversample,
        activation_precision: params_a.activation_precision,
        ir_path: None,
        ir_hash: None,
        model_hash: None,
    };

    {
        let state_ext = test_util::get_state_ext(&mut plugin_instance);
        let state_b = serde_json::to_vec(&params_b).unwrap();
        let mut handle = plugin_instance.plugin_handle();
        // Load returns Err when model is not found — no DSP change
        let result = state_ext.load(&mut handle, &mut state_b.as_slice());
        assert!(
            result.is_err(),
            "state load with missing model must return Err (transactional pipeline)"
        );
    }

    // Process one more block so the RT thread processes any queued events
    {
        let mut il = vec![0.3f32; n];
        let mut ir = vec![0.3f32; n];
        let mut ol = vec![0.0f32; n];
        let mut or = vec![0.0f32; n];
        let _ = process_block(
            &mut started,
            &mut il,
            &mut ir,
            &mut ol,
            &mut or,
            &InputEvents::empty(),
        );
    }

    plugin_instance.deactivate(started.stop_processing());

    // ── Assertions ──
    // Failed restore preserves old state completely.

    // Model name in GUI must still be the original model name
    let ui_name = shared.cold.ui_model_name.lock().unwrap();
    assert!(
        !ui_name.is_empty(),
        "ui_model_name must NOT be empty — old model should be preserved. Currently: '{ui_name}'"
    );
    assert_eq!(
        ui_name.as_str(),
        model_a_name,
        "ui_model_name should still be the old model name after failed restore"
    );

    // RT status must NOT have MODEL_LOAD_FAILED — no change to DSP
    assert!(
        !shared
            .cold
            .rt_status
            .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_MODEL_LOAD_FAILED),
        "RT_STATUS_MODEL_LOAD_FAILED should NOT be set — old DSP was never touched"
    );
}

// ── Test: Offline rendering preserves and restores realtime state ─────────
//
// When exiting offline mode, the previous activation precision must be restored
// to its pre-offline configuration.

#[test]
fn test_offline_realtime_restores_activation_precision() {
    use neural_amp_modeler_rs::common::params::ActivationPrecision;

    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

    let render_ext = plugin_instance
        .plugin_handle()
        .get_extension::<PluginRender>()
        .expect("PluginRender extension not found");

    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 256,
        max_frames_count: 256,
    };

    let stopped = plugin_instance.activate(|_, _| (), audio_config).unwrap();
    let mut started = stopped.start_processing().unwrap();

    // ── Step 1: Set activation precision to Fast ──
    shared
        .ui_to_rt
        .param_activation
        .store(ActivationPrecision::Fast as u32, Ordering::Relaxed);
    shared.bump_generation();

    // Process a block to sync the change
    {
        let n: usize = 256;
        let mut il = vec![0.3f32; n];
        let mut ir = vec![0.3f32; n];
        let mut ol = vec![0.0f32; n];
        let mut or = vec![0.0f32; n];
        let _ = process_block(
            &mut started,
            &mut il,
            &mut ir,
            &mut ol,
            &mut or,
            &InputEvents::empty(),
        );
    }

    // ── Step 2: Enter Offline mode ──
    {
        let mut handle = plugin_instance.plugin_handle();
        render_ext
            .set(&mut handle, RenderMode::Offline)
            .expect("set Offline should succeed");
    }

    // Process a block in offline mode
    {
        let n: usize = 256;
        let mut il = vec![0.3f32; n];
        let mut ir = vec![0.3f32; n];
        let mut ol = vec![0.0f32; n];
        let mut or = vec![0.0f32; n];
        let _ = process_block(
            &mut started,
            &mut il,
            &mut ir,
            &mut ol,
            &mut or,
            &InputEvents::empty(),
        );
    }

    // ── Step 3: Return to Realtime mode ──
    {
        let mut handle = plugin_instance.plugin_handle();
        render_ext
            .set(&mut handle, RenderMode::Realtime)
            .expect("set Realtime should succeed");
    }

    // Process a block to sync the render mode transition
    {
        let n: usize = 256;
        let mut il = vec![0.3f32; n];
        let mut ir = vec![0.3f32; n];
        let mut ol = vec![0.0f32; n];
        let mut or = vec![0.0f32; n];
        let _ = process_block(
            &mut started,
            &mut il,
            &mut ir,
            &mut ol,
            &mut or,
            &InputEvents::empty(),
        );
    }

    plugin_instance.deactivate(started.stop_processing());

    // Assertion: the internal activation precision at the DSP
    // level (TLS) must be restored to Fast after offline->realtime transition.
    let actual_mode = neural_amp_modeler_rs::math::activations::activation_precision();
    assert_eq!(
        actual_mode,
        ActivationPrecision::Fast,
        "activation precision must be Fast after offline->realtime cycle (was set to Fast before offline). Got {actual_mode:?}"
    );
}

// ── Test: Active bypass automation responds to host events ────────────────
//
// When bypass is active, host parameter events within the same block
// must be processed and applied to clear the bypass state cleanly.

#[test]
fn test_bypass_responds_to_host_events() {
    use clack_common::events::Pckn;
    use clack_common::events::event_types::ParamValueEvent;
    use clack_common::utils::{ClapId, Cookie};
    use nam_plug::clap::extensions::params::{PARAM_BYPASS, bypass_bool_to_u32};

    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 256,
        max_frames_count: 256,
    };

    // Sync bypass ON via UI to activate with bypass state
    shared
        .ui_to_rt
        .param_bypass
        .store(bypass_bool_to_u32(true), Ordering::Relaxed);
    shared.bump_generation();

    let stopped = plugin_instance.activate(|_, _| (), audio_config).unwrap();
    let mut started = stopped.start_processing().unwrap();

    let n: usize = 256;

    // Process blocks to settle bypass state
    for _ in 0..2 {
        let mut il = vec![0.3f32; n];
        let mut ir = vec![0.3f32; n];
        let mut ol = vec![0.0f32; n];
        let mut or = vec![0.0f32; n];
        let _ = process_block(
            &mut started,
            &mut il,
            &mut ir,
            &mut ol,
            &mut or,
            &InputEvents::empty(),
        );
    }

    // ── Send a bypass OFF event at offset 0 in the same block ──
    let mut input_events_buffer = EventBuffer::new();
    let bypass_off_event = ParamValueEvent::new(
        0u32,
        ClapId::new(PARAM_BYPASS),
        Pckn::match_all(),
        0.0f64,
        Cookie::empty(),
    );
    input_events_buffer.push(&bypass_off_event);

    let input_events = InputEvents::from_buffer(&input_events_buffer);

    let mut in_l = vec![0.5f32; n];
    let mut in_r = vec![0.5f32; n];
    let mut out_l = vec![0.0f32; n];
    let mut out_r = vec![0.0f32; n];
    let _ = process_block(
        &mut started,
        &mut in_l,
        &mut in_r,
        &mut out_l,
        &mut out_r,
        &input_events,
    );

    // Bypass OFF at offset 0 must deactivate bypass in the same block.
    // apply_scheduled_event() writes the new bypass state to ui_to_rt.param_bypass.
    let bypass_after = shared.ui_to_rt.param_bypass.load(Ordering::Relaxed);

    plugin_instance.deactivate(started.stop_processing());

    assert_eq!(
        bypass_after,
        bypass_bool_to_u32(false),
        "ui_to_rt.param_bypass must be OFF (0) after processing \
         bypass=OFF event at offset 0, but got {bypass_after}"
    );
}

// ── Test: Large audio blocks process completely without truncation ────────
//
// Audio blocks larger than default chunk partitions (e.g. 8193 frames) must be
// processed fully without buffer truncation.

#[test]
fn test_atypical_large_block_processes_completely() {
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 8193,
        max_frames_count: 8193,
    };

    let stopped = plugin_instance.activate(|_, _| (), audio_config).unwrap();
    let mut started = stopped.start_processing().unwrap();

    let n: usize = 8193;
    let mut in_l = vec![0.0f32; n];
    let mut in_r = vec![0.0f32; n];
    for i in 0..n {
        let v = (i as f32 / n as f32) * 0.1;
        in_l[i] = v;
        in_r[i] = v;
    }
    let mut out_l = vec![0.0f32; n];
    let mut out_r = vec![0.0f32; n];

    let _ = process_block(
        &mut started,
        &mut in_l,
        &mut in_r,
        &mut out_l,
        &mut out_r,
        &InputEvents::empty(),
    );

    plugin_instance.deactivate(started.stop_processing());

    // All outputs must be finite
    assert!(
        out_l[n - 1].is_finite(),
        "output sample at index {} is not finite: {}",
        n - 1,
        out_l[n - 1]
    );

    // Samples beyond 8192 must be non-zero (proving they were processed).
    let beyond_8192 = &out_l[8192..];
    let all_zero = beyond_8192.iter().all(|&s| s == 0.0);
    assert!(
        !all_zero,
        "output samples beyond index 8191 are all zero ({n} block total). Buffer tail must not be truncated."
    );

    let first_8192_nonzero = out_l[..8192].iter().any(|&s| s.abs() > 0.0);
    assert!(
        first_8192_nonzero,
        "First 8192 samples should contain non-zero output"
    );
}

// ── Test: Diagnostic log oversampling state truthfulness ───────────────────
//
// The log emitted upon entering offline mode must not falsely claim
// active oversampling if oversampling is set to Off.

#[test]
fn test_offline_log_does_not_claim_max_quality_without_4x() {
    use clack_extensions::render::{PluginRender, RenderMode};
    use neural_amp_modeler_rs::common::diagnostics::logger::NamLogger;

    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

    let render_ext = plugin_instance
        .plugin_handle()
        .get_extension::<PluginRender>()
        .expect("PluginRender extension not found");

    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 512,
        max_frames_count: 512,
    };

    let stopped = plugin_instance.activate(|_, _| (), audio_config).unwrap();
    let _started = stopped.start_processing().unwrap();

    // Enter Offline mode
    {
        let mut handle = plugin_instance.plugin_handle();
        render_ext
            .set(&mut handle, RenderMode::Offline)
            .expect("set Offline should succeed");
    }

    // Assertion: the log must NOT claim "max quality"
    // when no oversampling engine is active (factor is Off by default).
    if let Some(buffer) = NamLogger::log_buffer() {
        let snapshot = buffer.snapshot();
        for record in &snapshot {
            if record.message.contains("oversample=max quality") {
                panic!(
                    "Log claims 'oversample=max quality' but oversample is Off by default. Log line: {}",
                    record.message
                );
            }
        }
    }
}
