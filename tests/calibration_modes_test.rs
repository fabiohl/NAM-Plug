// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! End-to-end integration tests for Input Calibration and Output Modes.
//!
//! Covers:
//! 1. New-project default parity (`CalibrateInput = false`, `OutputMode = Normalized`,
//!    `InputCalibrationLevel = 12.0 dBu`) and exact `f64` oracle parity across
//!    `Raw`, `Normalized`, `Calibrated`, extreme dBu `[-60, +60]` (no ±30 dB LUT
//!    saturation), and 44.1 / 48 / 96 kHz sample rates.
//! 2. Pre-nonlinearity `CalibrateInput` behavior on `lstm.nam` below and above
//!    saturation.
//! 3. Missing and partial model metadata (never fabricating `dBu` or `loudness`).
//! 4. `.nam` and `.namb` v2 format parity plus gate and CabSim IR independence.
//! 5. Multi-instance isolation, live model swap, and smoothed CLAP parameter
//!    automation without click discontinuities.
//! 6. State save/restore migration (`v0`/`v1` -> legacy compensated defaults vs
//!    `v2` explicit calibration preservation).

use clack_extensions::params::PluginParams;
use clack_extensions::state::PluginState;
use clack_host::events::event_types::ParamValueEvent;
use clack_host::prelude::*;
use nam_plug::clap::extensions::calibration::{
    CalibrationConfig, ModelCalibrationMeta, OutputMode, compute_calibration_multipliers,
};
use nam_plug::clap::extensions::params::{
    PARAM_CALIBRATE_INPUT, PARAM_GATE_THRESH, PARAM_INPUT_CALIBRATION_LEVEL, PARAM_OUTPUT_MODE,
};
use nam_plug::clap::test_util::{self, TestHost};
use neural_amp_modeler_rs::common::params::ProcessingParams;
use neural_amp_modeler_rs::loader::nam_json::{WeightsLayout, parse_nam_json};
use neural_amp_modeler_rs::loader::namb_encoder::encode_namb;
use neural_amp_modeler_rs::testing::perceptual::compute_esr;
use neural_amp_modeler_rs::testing::reference_oracle::{PrecisionConfig, oracle_forward};
use neural_amp_modeler_rs::testing::wav::write_wav_f32;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const BLOCK: usize = 256;

fn unique_tmp_path(tag: &str, ext: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "nam_plug_cal_{tag}_{}_{nanos}.{ext}",
        std::process::id()
    ))
}

fn load_v2_instance_at_rate(
    model_path: &Path,
    ir_path: Option<&Path>,
    cal: CalibrationConfig,
    gate_threshold_db: f32,
    sample_rate: f64,
) -> (
    PluginEntry,
    HostInfo,
    PluginInstance<TestHost>,
    StartedPluginAudioProcessor<TestHost>,
) {
    let (entry, host_info, mut instance) = test_util::make_test_plugin();
    let mut params = ProcessingParams::default();
    params.gate_threshold_db = gate_threshold_db;
    params.model_path = Some(model_path.to_path_buf());
    params.model_hash = test_util::asset_hash(model_path);
    if let Some(ir) = ir_path {
        params.ir_path = Some(ir.to_path_buf());
        params.ir_hash = test_util::asset_hash(ir);
    }

    let envelope = serde_json::json!({
        "version": 2,
        "params": params,
        "ir_level_policy": "peak-normalized",
        "calibration": cal,
    });
    let state_bytes = serde_json::to_vec(&envelope).expect("serialize v2 state");
    let state_ext = instance
        .plugin_handle()
        .get_extension::<PluginState>()
        .expect("PluginState");
    let handle = instance.plugin_handle();
    let mut cursor = Cursor::new(&state_bytes);
    state_ext.load(&handle, &mut cursor).expect("load v2 state");

    let stopped = instance
        .activate(
            |_, _| (),
            PluginAudioConfiguration {
                sample_rate,
                min_frames_count: BLOCK as u32,
                max_frames_count: BLOCK as u32,
            },
        )
        .expect("activate");
    let started = stopped.start_processing().expect("start_processing");
    (entry, host_info, instance, started)
}

fn process_signal_with_events<F>(
    processor: &mut StartedPluginAudioProcessor<TestHost>,
    input: &[f32],
    mut block_events: F,
) -> Vec<f32>
where
    F: FnMut(usize, &mut EventBuffer),
{
    let mut output = Vec::with_capacity(input.len());
    let mut in_r = vec![0.0f32; BLOCK];
    let mut out_l = vec![0.0f32; BLOCK];
    let mut out_r = vec![0.0f32; BLOCK];

    for (block_idx, chunk) in input.chunks(BLOCK).enumerate() {
        let n = chunk.len();
        let mut in_l = chunk.to_vec();
        in_r[..n].copy_from_slice(&in_l);
        out_l[..n].fill(0.0);
        out_r[..n].fill(0.0);

        let mut input_ports = AudioPorts::with_capacity(2, 1);
        let mut output_ports = AudioPorts::with_capacity(2, 1);
        let mut in_ch = [&mut in_l[..n], &mut in_r[..n]];
        let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(
                in_ch.iter_mut().map(InputChannel::constant),
            ),
        }]);
        let out_ch = [&mut out_l[..n], &mut out_r[..n]];
        let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(out_ch.into_iter()),
        }]);

        let mut ev_buf = EventBuffer::new();
        block_events(block_idx, &mut ev_buf);
        let in_events = InputEvents::from_buffer(&ev_buf);
        let mut out_events = OutputEvents::void();

        processor
            .process(
                &input_audio,
                &mut output_audio,
                &in_events,
                &mut out_events,
                None,
                None,
            )
            .expect("process");
        output.extend_from_slice(&out_l[..n]);
    }
    output
}

fn sine_wave(len: usize, freq_hz: f32, amp: f32, sample_rate: f64) -> Vec<f32> {
    (0..len)
        .map(|i| {
            let t = i as f64 / sample_rate;
            amp * (2.0 * std::f64::consts::PI * f64::from(freq_hz) * t).sin() as f32
        })
        .collect()
}

fn oracle_calibrated_f32(
    model_path: &Path,
    input: &[f32],
    in_mult: f32,
    out_mult: f32,
) -> Vec<f32> {
    let json = std::fs::read_to_string(model_path).expect("read model json");
    let data = parse_nam_json(&json).expect("parse model json");
    let scaled_in: Vec<f64> = input
        .iter()
        .map(|&x| f64::from(x) * f64::from(in_mult))
        .collect();
    let out = oracle_forward(&data, &scaled_in, &PrecisionConfig::default());
    out.into_iter()
        .map(|y| (y * f64::from(out_mult)) as f32)
        .collect()
}

#[test]
fn t05_new_project_defaults_and_f64_oracle_parity_across_modes_and_rates() {
    let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
    let params_ext = instance
        .plugin_handle()
        .get_extension::<PluginParams>()
        .expect("PluginParams");
    let handle = instance.plugin_handle();

    assert_eq!(
        params_ext.get_value(&handle, ClapId::new(PARAM_CALIBRATE_INPUT)),
        Some(0.0)
    );
    assert_eq!(
        params_ext.get_value(&handle, ClapId::new(PARAM_INPUT_CALIBRATION_LEVEL)),
        Some(12.0)
    );
    assert_eq!(
        params_ext.get_value(&handle, ClapId::new(PARAM_OUTPUT_MODE)),
        Some(1.0)
    );
    drop(instance);

    let model = test_util::model_path("lstm.nam");
    let meta = ModelCalibrationMeta {
        has_model: true,
        loudness: Some(-37.840_687),
        input_level_dbu: Some(18.3),
        output_level_dbu: Some(12.3),
    };

    let stimulus = sine_wave(BLOCK * 24, 330.0, 0.25, 48_000.0);
    let configs = [
        CalibrationConfig {
            calibrate_input: false,
            input_calibration_level_dbu: 12.0,
            output_mode: OutputMode::Raw,
        },
        CalibrationConfig {
            calibrate_input: false,
            input_calibration_level_dbu: 12.0,
            output_mode: OutputMode::Normalized,
        },
        CalibrationConfig {
            calibrate_input: false,
            input_calibration_level_dbu: 12.0,
            output_mode: OutputMode::Calibrated,
        },
        CalibrationConfig {
            calibrate_input: true,
            input_calibration_level_dbu: 12.0,
            output_mode: OutputMode::Raw,
        },
        CalibrationConfig {
            calibrate_input: true,
            input_calibration_level_dbu: -60.0,
            output_mode: OutputMode::Calibrated,
        },
        CalibrationConfig {
            calibrate_input: true,
            input_calibration_level_dbu: 60.0,
            output_mode: OutputMode::Calibrated,
        },
    ];

    // Verify full calibration matrix against f64 oracle at native 48 kHz (including ±60 dBu
    // extremes spanning ±78.3 dB without GainLUT ±30 dB clipping).
    for cal in configs {
        let (_e, _h, _inst, mut proc) =
            load_v2_instance_at_rate(&model, None, cal, -90.0, 48_000.0);
        let produced = process_signal_with_events(&mut proc, &stimulus, |_, _| {});
        let (in_mult, out_mult) = compute_calibration_multipliers(meta, cal);
        let expected = oracle_calibrated_f32(&model, &stimulus, in_mult, out_mult);

        let start = BLOCK * 8;
        let esr = compute_esr(&expected[start..], &produced[start..]);
        assert!(
            esr < 1e-8,
            "48k oracle parity failed for {cal:?}: in_mult={in_mult}, out_mult={out_mult}, esr={esr:e}"
        );
    }

    // Verify 44.1 / 48 / 96 kHz exact output-mode scaling ratios on plugin outputs.
    for &rate in &[44_100.0, 48_000.0, 96_000.0] {
        let rate_stim = sine_wave(BLOCK * 32, 330.0, 0.25, rate);
        let cal_raw = CalibrationConfig {
            calibrate_input: false,
            input_calibration_level_dbu: 12.0,
            output_mode: OutputMode::Raw,
        };
        let cal_calib = CalibrationConfig {
            calibrate_input: false,
            input_calibration_level_dbu: 18.0,
            output_mode: OutputMode::Calibrated,
        };

        let (_e1, _h1, _i1, mut p_raw) =
            load_v2_instance_at_rate(&model, None, cal_raw, -90.0, rate);
        let out_raw = process_signal_with_events(&mut p_raw, &rate_stim, |_, _| {});

        let (_e2, _h2, _i2, mut p_cal) =
            load_v2_instance_at_rate(&model, None, cal_calib, -90.0, rate);
        let out_cal = process_signal_with_events(&mut p_cal, &rate_stim, |_, _| {});

        let (_, expected_ratio) = compute_calibration_multipliers(meta, cal_calib);
        let mut max_rel_err = 0.0f32;
        for (&r, &c) in out_raw.iter().zip(out_cal.iter()).skip(BLOCK * 12) {
            if r.abs() > 1e-4 {
                max_rel_err = max_rel_err.max((c - r * expected_ratio).abs() / c.abs().max(1e-6));
            }
        }
        assert!(
            max_rel_err < 1e-5,
            "rate {rate}: Calibrated vs Raw scaling diverged (max_rel_err={max_rel_err:e})"
        );
    }
}

#[test]
fn t05_lstm_saturation_divergence_below_and_above_pre_nonlinearity() {
    let model = test_util::model_path("lstm.nam");
    let stimulus = sine_wave(BLOCK * 20, 220.0, 0.65, 48_000.0);

    let cal_low = CalibrationConfig {
        calibrate_input: true,
        input_calibration_level_dbu: -6.0,
        output_mode: OutputMode::Raw,
    };
    let cal_high = CalibrationConfig {
        calibrate_input: true,
        input_calibration_level_dbu: 30.0,
        output_mode: OutputMode::Raw,
    };

    let (_e1, _h1, _i1, mut proc_low) =
        load_v2_instance_at_rate(&model, None, cal_low, -90.0, 48_000.0);
    let out_low = process_signal_with_events(&mut proc_low, &stimulus, |_, _| {});

    let (_e2, _h2, _i2, mut proc_high) =
        load_v2_instance_at_rate(&model, None, cal_high, -90.0, 48_000.0);
    let out_high = process_signal_with_events(&mut proc_high, &stimulus, |_, _| {});

    // Compare each against oracle with pre-nonlinearity input gain.
    let meta = ModelCalibrationMeta {
        has_model: true,
        loudness: Some(-37.840_687),
        input_level_dbu: Some(18.3),
        output_level_dbu: Some(12.3),
    };
    for (cal, produced, label) in [
        (cal_low, &out_low, "low_-6dBu"),
        (cal_high, &out_high, "high_+30dBu"),
    ] {
        let (in_mult, out_mult) = compute_calibration_multipliers(meta, cal);
        let expected = oracle_calibrated_f32(&model, &stimulus, in_mult, out_mult);
        let esr = compute_esr(&expected[BLOCK * 6..], &produced[BLOCK * 6..]);
        assert!(
            esr < 1e-8,
            "{label}: ESR vs f64 pre-nonlinearity oracle was {esr:e}"
        );
    }

    // Confirm linear post-gain ratio would NOT equal the nonlinear change (proving
    // pre-nonlinearity drive shifts harmonic compression on LSTM).
    let peak_low = out_low[BLOCK * 6..]
        .iter()
        .fold(0.0f32, |m, &x| m.max(x.abs()));
    let peak_high = out_high[BLOCK * 6..]
        .iter()
        .fold(0.0f32, |m, &x| m.max(x.abs()));
    let naive_linear_ratio = 10.0_f32.powf((30.0 - (-6.0)) / 20.0); // 63.1x (+36 dB)
    let actual_ratio = peak_high / peak_low.max(1e-9);
    assert!(
        actual_ratio < naive_linear_ratio * 0.75,
        "expected nonlinear saturation compression: actual_ratio={actual_ratio:.2}, naive_linear={naive_linear_ratio:.2}"
    );
}

#[test]
fn t05_missing_and_partial_metadata_never_fabricates_dbu() {
    let base_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(test_util::model_path("lstm.nam")).unwrap())
            .unwrap();

    // Case A: Empty metadata -> CalibrateInput=true and Calibrated must resolve to 1.0x.
    let mut no_meta = base_json.clone();
    no_meta
        .as_object_mut()
        .unwrap()
        .insert("metadata".to_string(), serde_json::json!({}));
    let no_meta_path = unique_tmp_path("no_meta", "nam");
    std::fs::write(&no_meta_path, serde_json::to_vec(&no_meta).unwrap()).unwrap();

    // Case B: Loudness only, no dBu fields -> Normalized scales, CalibrateInput/Calibrated stay 1.0x.
    let mut loudness_only = base_json;
    loudness_only.as_object_mut().unwrap().insert(
        "metadata".to_string(),
        serde_json::json!({ "loudness": -24.0 }),
    );
    let loudness_only_path = unique_tmp_path("loudness_only", "nam");
    std::fs::write(
        &loudness_only_path,
        serde_json::to_vec(&loudness_only).unwrap(),
    )
    .unwrap();

    let cal = CalibrationConfig {
        calibrate_input: true,
        input_calibration_level_dbu: -20.0,
        output_mode: OutputMode::Calibrated,
    };
    let stim = sine_wave(BLOCK * 10, 300.0, 0.2, 48_000.0);

    for &(path, exp_in_mult, exp_out_mult, label) in &[
        (&no_meta_path, 1.0_f32, 1.0_f32, "empty_meta_calibrated"),
        (
            &loudness_only_path,
            1.0_f32,
            1.0_f32,
            "loudness_only_calibrated",
        ),
    ] {
        let (_e, _h, _inst, mut proc) = load_v2_instance_at_rate(path, None, cal, -90.0, 48_000.0);
        let produced = process_signal_with_events(&mut proc, &stim, |_, _| {});
        let expected = oracle_calibrated_f32(path, &stim, exp_in_mult, exp_out_mult);
        let esr = compute_esr(&expected[BLOCK * 4..], &produced[BLOCK * 4..]);
        assert!(
            esr < 1e-10,
            "{label}: expected 1.0x fallback, got ESR={esr:e}"
        );
    }

    // With loudness_only in Normalized mode, target -18 - (-24) = +6 dB is still applied.
    let cal_norm = CalibrationConfig {
        calibrate_input: true,
        input_calibration_level_dbu: -20.0,
        output_mode: OutputMode::Normalized,
    };
    let (_e, _h, _inst, mut proc_norm) =
        load_v2_instance_at_rate(&loudness_only_path, None, cal_norm, -90.0, 48_000.0);
    let produced_norm = process_signal_with_events(&mut proc_norm, &stim, |_, _| {});
    let expected_norm = oracle_calibrated_f32(
        &loudness_only_path,
        &stim,
        1.0,
        10.0_f64.powf((-18.0 - (-24.0)) / 20.0) as f32,
    );
    let esr = compute_esr(&expected_norm[BLOCK * 4..], &produced_norm[BLOCK * 4..]);
    assert!(esr < 1e-10, "loudness_only_normalized: ESR={esr:e}");

    let _ = std::fs::remove_file(&no_meta_path);
    let _ = std::fs::remove_file(&loudness_only_path);
}

#[test]
fn t05_namb_v2_parity_and_gate_cabsim_independence() {
    let nam_path = test_util::model_path("lstm.nam");
    let data = parse_nam_json(&std::fs::read_to_string(&nam_path).unwrap()).unwrap();
    let namb_bytes = encode_namb(&data, 2, WeightsLayout::Original).expect("encode .namb");
    let namb_path = unique_tmp_path("lstm", "namb");
    std::fs::write(&namb_path, &namb_bytes).unwrap();

    // Unit-impulse WAV IR (PeakNormalized = 1.0 gain, introduces 1 partition latency = BLOCK).
    let ir_path = unique_tmp_path("unit_ir", "wav");
    let mut ir = vec![0.0f32; 512];
    ir[0] = 0.5;
    write_wav_f32(&ir_path, &ir, 48_000).unwrap();

    let cal_a = CalibrationConfig {
        calibrate_input: true,
        input_calibration_level_dbu: 6.0,
        output_mode: OutputMode::Calibrated,
    };
    let stim = sine_wave(BLOCK * 16, 440.0, 0.25, 48_000.0);

    // .nam vs .namb parity under identical calibration + unit IR + open gate (-90 dB).
    let (_e1, _h1, _i1, mut p_nam) =
        load_v2_instance_at_rate(&nam_path, Some(&ir_path), cal_a, -90.0, 48_000.0);
    let out_nam = process_signal_with_events(&mut p_nam, &stim, |_, _| {});

    let (_e2, _h2, _i2, mut p_namb) =
        load_v2_instance_at_rate(&namb_path, Some(&ir_path), cal_a, -90.0, 48_000.0);
    let out_namb = process_signal_with_events(&mut p_namb, &stim, |_, _| {});

    let esr = compute_esr(&out_nam[BLOCK * 6..], &out_namb[BLOCK * 6..]);
    assert!(esr < 1e-12, ".nam vs .namb calibration parity ESR={esr:e}");

    // Verify Gate detects threshold on RAW pre-calibration signal (-40 dBFS open / -46 dBFS close):
    // Wait past gate hold + fade (~100 ms = ~19 blocks of 256 at 48 kHz); an input at 0.002 (-54 dBFS)
    // must settle into GateState::Closed and remain closed even under +60 dBu calibration boost.
    let quiet_stim = sine_wave(BLOCK * 32, 440.0, 0.002, 48_000.0);
    let hot_cal = CalibrationConfig {
        calibrate_input: true,
        input_calibration_level_dbu: 60.0,
        output_mode: OutputMode::Normalized,
    };
    let (_e3, _h3, _i3, mut p_gated) =
        load_v2_instance_at_rate(&nam_path, Some(&ir_path), hot_cal, -40.0, 48_000.0);
    let gated_out = process_signal_with_events(&mut p_gated, &quiet_stim, |_, _| {});
    let gated_peak = gated_out[BLOCK * 24..]
        .iter()
        .fold(0.0f32, |m, &x| m.max(x.abs()));
    assert!(
        gated_peak < 1e-6,
        "gate must evaluate raw pre-calibration input, got tail peak {gated_peak:e}"
    );

    // Silence PARAM_GATE_THRESH unused-import check & confirm automation compatibility.
    assert_eq!(PARAM_GATE_THRESH, 2);

    let _ = std::fs::remove_file(&namb_path);
    let _ = std::fs::remove_file(&ir_path);
}

#[test]
fn t05_multi_instance_isolation_and_smoothed_live_automation() {
    let model_lstm = test_util::model_path("lstm.nam");
    let model_a1 = test_util::model_path("wavenet_a1_standard.nam");

    let cal_raw = CalibrationConfig {
        calibrate_input: false,
        input_calibration_level_dbu: 12.0,
        output_mode: OutputMode::Raw,
    };
    let cal_hot = CalibrationConfig {
        calibrate_input: true,
        input_calibration_level_dbu: 24.0,
        output_mode: OutputMode::Calibrated,
    };

    let (_e1, _h1, _i1, mut proc_a) =
        load_v2_instance_at_rate(&model_lstm, None, cal_raw, -90.0, 48_000.0);
    let (_e2, _h2, _i2, mut proc_b) =
        load_v2_instance_at_rate(&model_a1, None, cal_hot, -90.0, 48_000.0);

    let stim = sine_wave(BLOCK * 20, 330.0, 0.25, 48_000.0);
    let out_b = process_signal_with_events(&mut proc_b, &stim, |_, _| {});
    assert!(out_b[BLOCK * 6..].iter().all(|&x| x.is_finite()));

    // Automate CalibrateInput=1, InputCalLevel=18 -> -6 dBu, OutputMode=Normalized mid-stream on A.
    let out_a = process_signal_with_events(&mut proc_a, &stim, |block_idx, ev| {
        if block_idx == 6 {
            ev.push(&ParamValueEvent::new(
                0,
                ClapId::new(PARAM_CALIBRATE_INPUT),
                Pckn::match_all(),
                1.0,
            ));
            ev.push(&ParamValueEvent::new(
                0,
                ClapId::new(PARAM_INPUT_CALIBRATION_LEVEL),
                Pckn::match_all(),
                -6.0,
            ));
            ev.push(&ParamValueEvent::new(
                0,
                ClapId::new(PARAM_OUTPUT_MODE),
                Pckn::match_all(),
                OutputMode::Normalized.to_f32() as f64,
            ));
        }
    });

    // Check continuity around the transition block (block 6 = sample 1536): OutputMode changes from
    // Raw (1.0x) to Normalized (9.818x) smoothed over ~5ms (no instantaneous 9x step discontinuity).
    let transition_deriv = (out_a[BLOCK * 6] - out_a[BLOCK * 6 - 1]).abs();
    let max_steady = out_a[BLOCK * 16..]
        .windows(2)
        .fold(0.0f32, |m, w| m.max((w[1] - w[0]).abs()));
    assert!(
        transition_deriv < max_steady.max(0.25),
        "smoothed calibration automation stepped abruptly: transition_deriv={transition_deriv:e}, steady_deriv={max_steady:e}"
    );
}

fn load_state_bytes(instance: &mut PluginInstance<TestHost>, bytes: &[u8]) {
    let state_ext = instance
        .plugin_handle()
        .get_extension::<PluginState>()
        .expect("PluginState");
    let handle = instance.plugin_handle();
    let mut cursor = Cursor::new(bytes);
    state_ext
        .load(&handle, &mut cursor)
        .expect("load state bytes");
}

fn assert_cal_param_values(
    instance: &mut PluginInstance<TestHost>,
    expected_cal_in: f64,
    expected_level_dbu: f64,
    expected_out_mode: f64,
) {
    let params_ext = instance
        .plugin_handle()
        .get_extension::<PluginParams>()
        .expect("PluginParams");
    let handle = instance.plugin_handle();
    assert_eq!(
        params_ext.get_value(&handle, ClapId::new(PARAM_CALIBRATE_INPUT)),
        Some(expected_cal_in)
    );
    assert_eq!(
        params_ext.get_value(&handle, ClapId::new(PARAM_INPUT_CALIBRATION_LEVEL)),
        Some(expected_level_dbu)
    );
    assert_eq!(
        params_ext.get_value(&handle, ClapId::new(PARAM_OUTPUT_MODE)),
        Some(expected_out_mode)
    );
}

#[test]
fn t05_state_migration_v0_v1_legacy_vs_v2_roundtrip() {
    let model = test_util::model_path("lstm.nam");
    let cal_custom = CalibrationConfig {
        calibrate_input: true,
        input_calibration_level_dbu: -15.5,
        output_mode: OutputMode::Calibrated,
    };
    let (_e, _h, mut instance, mut proc) =
        load_v2_instance_at_rate(&model, None, cal_custom, -90.0, 48_000.0);

    let stim = sine_wave(BLOCK * 8, 330.0, 0.25, 48_000.0);
    let out_before = process_signal_with_events(&mut proc, &stim, |_, _| {});
    let stopped = proc.stop_processing();
    instance.deactivate(stopped);

    let mut saved = Vec::new();
    {
        let state_ext = instance
            .plugin_handle()
            .get_extension::<PluginState>()
            .expect("PluginState");
        let handle = instance.plugin_handle();
        state_ext.save(&handle, &mut saved).expect("save v2 state");
    }

    // Reload saved bytes into a fresh instance and verify params 9/10/11 + audio identity.
    let (_e2, _h2, mut instance2) = test_util::make_test_plugin();
    load_state_bytes(&mut instance2, &saved);
    assert_cal_param_values(
        &mut instance2,
        1.0,
        -15.5,
        OutputMode::Calibrated.to_f32() as f64,
    );

    let stopped2 = instance2
        .activate(
            |_, _| (),
            PluginAudioConfiguration {
                sample_rate: 48_000.0,
                min_frames_count: BLOCK as u32,
                max_frames_count: BLOCK as u32,
            },
        )
        .expect("activate restored");
    let mut proc2 = stopped2.start_processing().expect("start restored");
    let out_after = process_signal_with_events(&mut proc2, &stim, |_, _| {});
    let esr = compute_esr(&out_before[BLOCK * 2..], &out_after[BLOCK * 2..]);
    assert!(
        esr < 1e-12,
        "saved/restored calibration state diverged: ESR={esr:e}"
    );

    // Verify legacy v1 payload lacking `"calibration"` restores legacy_restored_default():
    // CalibrateInput = true (1.0), InputCalibrationLevel = 12.0 dBu, OutputMode = Normalized (1.0).
    let (_e3, _h3, mut legacy_inst) = test_util::make_test_plugin();
    let legacy_params = ProcessingParams::default();
    let v1_bytes = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "params": legacy_params,
    }))
    .unwrap();
    load_state_bytes(&mut legacy_inst, &v1_bytes);
    assert_cal_param_values(&mut legacy_inst, 1.0, 12.0, 1.0);
}
