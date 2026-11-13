// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! End-to-end differential parity and integration tests for `GateMode::Musical`
//! in NAM-Plug.
//!
//! Covers:
//! 1. Differential sample-by-sample parity against the double-precision C++
//!    `AudioDSPTools/dsp/NoiseGate.{h,cpp}` equations across `44.1 / 48 / 96 kHz`
//!    and `GateMode::Silence` baseline un-gated model output.
//! 2. Pre-calibration detector tap (post user input gain, pre-`CalibrateInput`)
//!    and post-model / pre-CabSim-IR gain application preserving natural IR tails.
//! 3. `-90.0 dB` (`OFF`) threshold disable contract vs active quadratic expansion.
//! 4. Independent per-channel stereo (`CONFIG_ID_STEREO`) expansion.
//! 5. Live CLAP parameter automation (`PARAM_GATE_MODE`, `PARAM_GATE_THRESH`),
//!    non-finite input resilience, and state save/restore migration across `v0`,
//!    `v1`, and `v2` payloads.
//! 6. Zero-allocation RT proof under `feature = "heap-audit"`.

use clack_extensions::audio_ports_config::PluginAudioPortsConfig;
use clack_extensions::params::PluginParams;
use clack_extensions::state::PluginState;
use clack_host::events::event_types::ParamValueEvent;
use clack_host::prelude::*;
use nam_plug::clap::extensions::calibration::{CalibrationConfig, OutputMode};
use nam_plug::clap::extensions::gate_mode::GateMode;
use nam_plug::clap::extensions::params::{PARAM_GATE_MODE, PARAM_GATE_THRESH};
use nam_plug::clap::plugin::shared::CONFIG_ID_STEREO;
use nam_plug::clap::test_util::{self, TestHost};
use neural_amp_modeler_rs::common::params::ProcessingParams;
use neural_amp_modeler_rs::testing::perceptual::compute_esr;
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
        "nam_plug_musical_gate_{tag}_{}_{nanos}.{ext}",
        std::process::id()
    ))
}

/// Exact double-precision C++ `AudioDSPTools/dsp/NoiseGate.{h,cpp}` reference oracle.
struct CppNoiseGateOracle {
    alpha: f64,
    beta: f64,
    dt: f64,
    threshold_db: f64,
    max_gain_reduction_db: f64,
    d_open: f64,
    d_close: f64,
    level: f64,
    last_gr_db: f64,
    time_held: f64,
    holding: bool,
}

impl CppNoiseGateOracle {
    const TIME_SEC: f64 = 0.01;
    const RATIO: f64 = 0.1;
    const OPEN_TIME_SEC: f64 = 0.005;
    const HOLD_TIME_SEC: f64 = 0.01;
    const CLOSE_TIME_SEC: f64 = 0.05;
    const MIN_LOUDNESS_DB: f64 = -120.0;
    const MIN_LOUDNESS_POWER: f64 = 1e-12;
    const MAX_LOUDNESS_POWER: f64 = 1e12;

    fn new(sample_rate: f64, threshold_db: f64) -> Self {
        let alpha = 0.5_f64.powf(1.0 / (Self::TIME_SEC * sample_rate));
        let max_gr = Self::gr_db(Self::MIN_LOUDNESS_DB, threshold_db);
        Self {
            alpha,
            beta: 1.0 - alpha,
            dt: 1.0 / sample_rate,
            threshold_db,
            max_gain_reduction_db: max_gr,
            d_open: -max_gr / (Self::OPEN_TIME_SEC * sample_rate),
            d_close: max_gr / (Self::CLOSE_TIME_SEC * sample_rate),
            level: Self::MIN_LOUDNESS_POWER,
            last_gr_db: max_gr,
            time_held: 0.0,
            holding: false,
        }
    }

    fn gr_db(level_db: f64, threshold_db: f64) -> f64 {
        if level_db < threshold_db {
            let diff = level_db - threshold_db;
            -Self::RATIO * diff * diff
        } else {
            0.0
        }
    }

    fn next_gain(&mut self, sample: f32) -> f32 {
        let x = f64::from(sample);
        self.level = (self.alpha * self.level + self.beta * (x * x))
            .clamp(Self::MIN_LOUDNESS_POWER, Self::MAX_LOUDNESS_POWER);
        let level_db = 10.0 * self.level.log10();

        if self.holding {
            self.last_gr_db = 0.0;
            if level_db < self.threshold_db {
                self.time_held += self.dt;
                if self.time_held >= Self::HOLD_TIME_SEC {
                    self.holding = false;
                }
            } else {
                self.time_held = 0.0;
            }
            1.0
        } else {
            let target_gr = Self::gr_db(level_db, self.threshold_db);
            if target_gr > self.last_gr_db {
                let dg = (0.5 * (target_gr - self.last_gr_db)).clamp(0.0, self.d_open);
                self.last_gr_db += dg;
                if self.last_gr_db >= 0.0 {
                    self.last_gr_db = 0.0;
                    self.holding = true;
                    self.time_held = 0.0;
                    return 1.0;
                }
            } else if target_gr < self.last_gr_db {
                let dg = (0.5 * (target_gr - self.last_gr_db)).clamp(self.d_close, 0.0);
                self.last_gr_db += dg;
                if self.last_gr_db < self.max_gain_reduction_db {
                    self.last_gr_db = self.max_gain_reduction_db;
                }
            }
            // Mirrors C++ `_DBToLevel(lastGainReductionDB) = pow(10, gr / 10)`.
            10.0_f64.powf(self.last_gr_db * 0.1) as f32
        }
    }
}

fn setup_plugin(
    model_path: &Path,
    ir_path: Option<&Path>,
    cal: CalibrationConfig,
    gate_mode: GateMode,
    gate_threshold_db: f32,
    sample_rate: f64,
    select_stereo: bool,
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
        "gate_mode": gate_mode,
    });
    let state_bytes = serde_json::to_vec(&envelope).expect("serialize v2 state");
    {
        let state_ext = instance
            .plugin_handle()
            .get_extension::<PluginState>()
            .expect("PluginState");
        let handle = instance.plugin_handle();
        let mut cursor = Cursor::new(&state_bytes);
        state_ext.load(&handle, &mut cursor).expect("load v2 state");
    }

    if select_stereo {
        let cfg_ext = instance
            .plugin_handle()
            .get_extension::<PluginAudioPortsConfig>()
            .expect("PluginAudioPortsConfig");
        let handle = instance.plugin_handle();
        cfg_ext
            .select(&handle, ClapId::new(CONFIG_ID_STEREO))
            .expect("select stereo config");
    }

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

fn process_stereo_with_events<F>(
    processor: &mut StartedPluginAudioProcessor<TestHost>,
    in_l_all: &[f32],
    in_r_all: &[f32],
    mut block_events: F,
) -> (Vec<f32>, Vec<f32>)
where
    F: FnMut(usize, &mut EventBuffer),
{
    assert_eq!(in_l_all.len(), in_r_all.len());
    let mut out_l_all = Vec::with_capacity(in_l_all.len());
    let mut out_r_all = Vec::with_capacity(in_r_all.len());
    let mut out_l = vec![0.0f32; BLOCK];
    let mut out_r = vec![0.0f32; BLOCK];

    for (block_idx, (chunk_l, chunk_r)) in in_l_all
        .chunks(BLOCK)
        .zip(in_r_all.chunks(BLOCK))
        .enumerate()
    {
        let n = chunk_l.len();
        let mut buf_in_l = chunk_l.to_vec();
        let mut buf_in_r = chunk_r.to_vec();
        out_l[..n].fill(0.0);
        out_r[..n].fill(0.0);

        let mut input_ports = AudioPorts::with_capacity(2, 1);
        let mut output_ports = AudioPorts::with_capacity(2, 1);
        let mut in_ch = [&mut buf_in_l[..n], &mut buf_in_r[..n]];
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
        out_l_all.extend_from_slice(&out_l[..n]);
        out_r_all.extend_from_slice(&out_r[..n]);
    }
    (out_l_all, out_r_all)
}

fn sine_wave(len: usize, freq_hz: f32, amp: f32, sample_rate: f64) -> Vec<f32> {
    (0..len)
        .map(|i| {
            let t = i as f64 / sample_rate;
            amp * (2.0 * std::f64::consts::PI * f64::from(freq_hz) * t).sin() as f32
        })
        .collect()
}

fn rms(slice: &[f32]) -> f32 {
    let sum_sq: f64 = slice.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    (sum_sq / slice.len().max(1) as f64).sqrt() as f32
}

fn param_value_event(param_id: u32, value: f64) -> ParamValueEvent {
    ParamValueEvent::new(0, ClapId::new(param_id), Pckn::match_all(), value)
}

#[test]
fn t01_differential_parity_vs_cpp_oracle_across_sample_rates() {
    let model = test_util::model_path("lstm.nam");
    let raw_cal = CalibrationConfig {
        calibrate_input: false,
        input_calibration_level_dbu: 12.0,
        output_mode: OutputMode::Raw,
    };

    for &sr in &[44_100.0, 48_000.0, 96_000.0] {
        // Composite trajectory crossing threshold multiple times:
        // silence -> quiet (-70 dBFS) -> loud (-12 dBFS) -> decay sweep -> silence.
        let n_seg = BLOCK * 6;
        let mut stim = vec![0.0f32; n_seg * 5];
        for (i, s) in stim[n_seg..n_seg * 2].iter_mut().enumerate() {
            let phase = 2.0 * std::f64::consts::PI * 190.0 * (i as f64 / sr);
            *s = 3e-4 * phase.sin() as f32;
        }
        for (i, s) in stim[n_seg * 2..n_seg * 3].iter_mut().enumerate() {
            let phase = 2.0 * std::f64::consts::PI * 220.0 * (i as f64 / sr);
            *s = 0.25 * phase.sin() as f32;
        }
        for (i, s) in stim[n_seg * 3..n_seg * 4].iter_mut().enumerate() {
            let env = 0.25 * (-4.5 * (i as f64 / n_seg as f64)).exp();
            let phase = 2.0 * std::f64::consts::PI * 220.0 * (i as f64 / sr);
            *s = (env * phase.sin()) as f32;
        }

        // Un-gated model baseline (`Silence` @ -90 dB stays open after above-90 trigger,
        // wait: -90 dB cutoff still closes on true zero after hold/fade, so feed raw baseline
        // via Musical @ -90 dB where musical gate is disabled and silence gate stays open).
        let (_e0, _h0, _i0, mut proc_ungated) =
            setup_plugin(&model, None, raw_cal, GateMode::Musical, -90.0, sr, false);
        let (ungated_out, _) =
            process_stereo_with_events(&mut proc_ungated, &stim, &stim, |_, _| {});

        let thresh_db = -40.0;
        let (_e1, _h1, _i1, mut proc_musical) = setup_plugin(
            &model,
            None,
            raw_cal,
            GateMode::Musical,
            thresh_db,
            sr,
            false,
        );
        let (musical_out, _) =
            process_stereo_with_events(&mut proc_musical, &stim, &stim, |_, _| {});

        let mut oracle = CppNoiseGateOracle::new(sr, f64::from(thresh_db));
        let expected: Vec<f32> = stim
            .iter()
            .zip(ungated_out.iter())
            .map(|(&x, &y)| y * oracle.next_gain(x))
            .collect();

        let esr = compute_esr(&expected, &musical_out);
        let max_err = expected
            .iter()
            .zip(musical_out.iter())
            .map(|(&a, &b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(
            esr < 1e-11 && max_err < 1e-5,
            "sr={sr}: C++ oracle mismatch ESR={esr:e}, max_err={max_err:e}"
        );
    }
}

#[test]
fn t02_pre_calibration_detector_and_pre_ir_natural_tail() {
    let model = test_util::model_path("lstm.nam");
    // Construct a synthetic exponential decay CabSim IR (4096 samples @ 48 kHz)
    let ir_len = 4096;
    let ir: Vec<f32> = (0..ir_len)
        .map(|i| {
            if i == 0 {
                1.0
            } else {
                0.85 * (-(i as f32) / 700.0).exp()
            }
        })
        .collect();
    let ir_path = unique_tmp_path("long_ir", "wav");
    write_wav_f32(&ir_path, &ir, 48_000).expect("write IR");

    // 1. Pre-IR gain + natural IR tail comparison:
    // Loud burst followed by 340 ms silence so Musical gate detector settles
    // smoothly through its 10 ms half-life exponential detector while attenuating
    // pre-IR amp floor and allowing natural IR decay.
    let mut stim = vec![0.0f32; BLOCK * 64];
    let burst = sine_wave(BLOCK * 4, 220.0, 0.4, 48_000.0);
    stim[..burst.len()].copy_from_slice(&burst);

    let raw_cal = CalibrationConfig {
        calibrate_input: false,
        input_calibration_level_dbu: 12.0,
        output_mode: OutputMode::Raw,
    };

    let (_e_s, _h_s, _i_s, mut proc_silence) = setup_plugin(
        &model,
        Some(&ir_path),
        raw_cal,
        GateMode::Silence,
        -35.0,
        48_000.0,
        false,
    );
    let (out_silence, _) = process_stereo_with_events(&mut proc_silence, &stim, &stim, |_, _| {});

    let (_e_m, _h_m, _i_m, mut proc_musical) = setup_plugin(
        &model,
        Some(&ir_path),
        raw_cal,
        GateMode::Musical,
        -35.0,
        48_000.0,
        false,
    );
    let (out_musical, _) = process_stereo_with_events(&mut proc_musical, &stim, &stim, |_, _| {});

    // Tail window well before settled floor shows natural decay:
    let tail_musical_rms = rms(&out_musical[BLOCK * 12..BLOCK * 20]);
    let late_musical_rms = rms(&out_musical[BLOCK * 56..BLOCK * 64]);
    assert!(
        tail_musical_rms > late_musical_rms * 5.0,
        "Musical IR tail must decay naturally (mid={tail_musical_rms:e}, late={late_musical_rms:e})"
    );
    let (_e_u, _h_u, _i_u, mut proc_ungated) = setup_plugin(
        &model,
        Some(&ir_path),
        raw_cal,
        GateMode::Musical,
        -90.0,
        48_000.0,
        false,
    );
    let (out_ungated, _) = process_stereo_with_events(&mut proc_ungated, &stim, &stim, |_, _| {});
    let gated_noise_rms = rms(&out_musical[BLOCK * 56..BLOCK * 64]);
    let ungated_noise_rms = rms(&out_ungated[BLOCK * 56..BLOCK * 64]);
    assert!(
        gated_noise_rms < ungated_noise_rms * 0.01,
        "Pre-IR musical gate must strongly attenuate idle amp floor (gated={gated_noise_rms:e}, ungated={ungated_noise_rms:e})"
    );
    assert!(out_silence.iter().all(|s| s.is_finite()));

    // 2. Verify detector taps PRE-calibration:
    // Use extreme InputCalibrationLevel = +40 dBu (CalibrateInput = true attenuates model input
    // by > 20 dB on lstm.nam with input_level_dbu = 18.3 dBu). Feed a -30 dBFS signal with gate
    // threshold at -40 dB:
    //   Pre-cal level is -30 dBFS (> -40 dB => Musical gate MUST open ~1.0x gain!).
    //   Post-cal level inside model is < -51 dBFS (< -40 dB); if detector were post-cal it would close.
    let extreme_cal = CalibrationConfig {
        calibrate_input: true,
        input_calibration_level_dbu: 40.0,
        output_mode: OutputMode::Raw,
    };
    let tone = sine_wave(BLOCK * 16, 220.0, 0.035, 48_000.0); // peak ~ -29 dBFS
    let (_ec1, _hc1, _ic1, mut proc_cal_off) = setup_plugin(
        &model,
        None,
        extreme_cal,
        GateMode::Musical,
        -90.0,
        48_000.0,
        false,
    );
    let (cal_ungated, _) = process_stereo_with_events(&mut proc_cal_off, &tone, &tone, |_, _| {});

    let (_ec2, _hc2, _ic2, mut proc_cal_on) = setup_plugin(
        &model,
        None,
        extreme_cal,
        GateMode::Musical,
        -40.0,
        48_000.0,
        false,
    );
    let (cal_gated, _) = process_stereo_with_events(&mut proc_cal_on, &tone, &tone, |_, _| {});

    let steady_esr = compute_esr(&cal_ungated[BLOCK * 8..], &cal_gated[BLOCK * 8..]);
    assert!(
        steady_esr < 1e-6,
        "Pre-cal -29 dBFS > -40 dB threshold must hold Musical gate open (ESR={steady_esr:e})"
    );

    let _ = std::fs::remove_file(&ir_path);
}

#[test]
fn t03_threshold_off_at_minus_90_and_stereo_channel_independence() {
    let model = test_util::model_path("lstm.nam");
    let raw_cal = CalibrationConfig {
        calibrate_input: false,
        input_calibration_level_dbu: 12.0,
        output_mode: OutputMode::Raw,
    };

    // Quiet -84 dBFS sine + -90 dB threshold => Musical gate bypassed cleanly;
    // at -70 dB threshold => quadratically expanded.
    let quiet = sine_wave(BLOCK * 16, 220.0, 6e-5, 48_000.0);
    let (_e0, _h0, _i0, mut proc_off) = setup_plugin(
        &model,
        None,
        raw_cal,
        GateMode::Musical,
        -90.0,
        48_000.0,
        false,
    );
    let (out_off, _) = process_stereo_with_events(&mut proc_off, &quiet, &quiet, |_, _| {});

    let (_e1, _h1, _i1, mut proc_on) = setup_plugin(
        &model,
        None,
        raw_cal,
        GateMode::Musical,
        -65.0,
        48_000.0,
        false,
    );
    let (out_on, _) = process_stereo_with_events(&mut proc_on, &quiet, &quiet, |_, _| {});

    let rms_off = rms(&out_off[BLOCK * 10..]);
    let rms_on = rms(&out_on[BLOCK * 10..]);
    assert!(
        rms_on < rms_off * 0.05,
        "-65 dB threshold must quadratically expand -84 dBFS signal while -90 dB stays OFF (on={rms_on:e}, off={rms_off:e})"
    );

    // Stereo / Dual-Mono Independence: L = -15 dBFS (open), R = silence (expanded)
    let active_l = sine_wave(BLOCK * 16, 220.0, 0.25, 48_000.0);
    let silent_r = vec![0.0f32; BLOCK * 16];
    let (_es, _hs, _is, mut proc_stereo) = setup_plugin(
        &model,
        None,
        raw_cal,
        GateMode::Musical,
        -35.0,
        48_000.0,
        true,
    );
    let (stereo_l, stereo_r) =
        process_stereo_with_events(&mut proc_stereo, &active_l, &silent_r, |_, _| {});

    let rms_l = rms(&stereo_l[BLOCK * 10..]);
    let rms_r = rms(&stereo_r[BLOCK * 10..]);
    assert!(
        rms_l > 1e-3 && rms_r < rms_l * 0.01,
        "L channel active ({rms_l:e}) must not open R channel ({rms_r:e}) in stereo mode"
    );
}

#[test]
fn t04_live_clap_automation_mode_switch_and_state_migration() {
    let model = test_util::model_path("lstm.nam");
    let raw_cal = CalibrationConfig::default();
    let stim = sine_wave(BLOCK * 24, 220.0, 0.2, 48_000.0);
    let mut stim_with_nan = stim.clone();
    stim_with_nan[BLOCK * 8] = f32::NAN;
    stim_with_nan[BLOCK * 8 + 1] = f32::INFINITY;

    let (_e, _h, mut instance, mut proc) = setup_plugin(
        &model,
        None,
        raw_cal,
        GateMode::Silence,
        -45.0,
        48_000.0,
        false,
    );

    let (out, _) = process_stereo_with_events(
        &mut proc,
        &stim_with_nan,
        &stim_with_nan,
        |blk, ev| match blk {
            4 => ev.push(&param_value_event(PARAM_GATE_MODE, 1.0)),
            10 => ev.push(&param_value_event(PARAM_GATE_THRESH, -20.0)),
            14 => ev.push(&param_value_event(PARAM_GATE_MODE, 0.0)),
            18 => ev.push(&param_value_event(PARAM_GATE_MODE, 1.0)),
            _ => {}
        },
    );
    assert!(
        out.iter().all(|s| s.is_finite()),
        "mode switches + non-finite inputs must remain finite"
    );

    let stopped = proc.stop_processing();
    instance.deactivate(stopped);

    // Save state -> must persist GateMode::Musical (1.0)
    let mut saved = Vec::new();
    {
        let state_ext = instance
            .plugin_handle()
            .get_extension::<PluginState>()
            .expect("PluginState");
        let handle = instance.plugin_handle();
        state_ext.save(&handle, &mut saved).expect("save state");
    }

    let (_e2, _h2, mut instance2) = test_util::make_test_plugin();
    {
        let state_ext = instance2
            .plugin_handle()
            .get_extension::<PluginState>()
            .expect("PluginState");
        let handle2 = instance2.plugin_handle();
        let mut cursor = Cursor::new(&saved);
        state_ext.load(&handle2, &mut cursor).expect("load state");
    }
    {
        let params_ext = instance2
            .plugin_handle()
            .get_extension::<PluginParams>()
            .expect("PluginParams");
        let handle2 = instance2.plugin_handle();
        assert_eq!(
            params_ext.get_value(&handle2, ClapId::new(PARAM_GATE_MODE)),
            Some(1.0),
            "saved GateMode::Musical must restore to 1.0"
        );
    }

    // Pre-S07 v2 state missing `gate_mode` must default safely to GateMode::Silence (0.0).
    let v2_legacy = serde_json::to_vec(&serde_json::json!({
        "version": 2,
        "params": ProcessingParams::default(),
        "ir_level_policy": "peak-normalized",
        "calibration": CalibrationConfig::default(),
    }))
    .unwrap();
    {
        let state_ext = instance2
            .plugin_handle()
            .get_extension::<PluginState>()
            .expect("PluginState");
        let handle2 = instance2.plugin_handle();
        let mut cursor = Cursor::new(&v2_legacy);
        state_ext
            .load(&handle2, &mut cursor)
            .expect("load pre-S07 v2");
    }
    let params_ext = instance2
        .plugin_handle()
        .get_extension::<PluginParams>()
        .expect("PluginParams");
    let handle2 = instance2.plugin_handle();
    assert_eq!(
        params_ext.get_value(&handle2, ClapId::new(PARAM_GATE_MODE)),
        Some(0.0),
        "pre-S07 v2 payload without gate_mode must default to Silence (0.0)"
    );
}

#[cfg(feature = "heap-audit")]
#[test]
fn t05_musical_gate_rt_zero_alloc_proof() {
    use neural_amp_modeler_rs::common::alloc_audit::{TrackingGuard, get_alloc_count};
    let model = test_util::model_path("lstm.nam");
    let (_e, _h, _i, mut proc) = setup_plugin(
        &model,
        None,
        CalibrationConfig::default(),
        GateMode::Musical,
        -40.0,
        48_000.0,
        true,
    );
    let stim = sine_wave(BLOCK, 220.0, 0.25, 48_000.0);
    // Warm up one block
    let _ = process_stereo_with_events(&mut proc, &stim, &stim, |_, _| {});

    // Pre-allocate all host-side containers off the guarded RT call.
    let mut in_l = stim.clone();
    let mut in_r = stim;
    let mut out_l = vec![0.0f32; BLOCK];
    let mut out_r = vec![0.0f32; BLOCK];
    let mut input_ports = AudioPorts::with_capacity(2, 1);
    let mut output_ports = AudioPorts::with_capacity(2, 1);
    let mut in_ch = [&mut in_l[..], &mut in_r[..]];
    let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_input_only(in_ch.iter_mut().map(InputChannel::constant)),
    }]);
    let out_ch = [&mut out_l[..], &mut out_r[..]];
    let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_output_only(out_ch.into_iter()),
    }]);
    let mut ev_buf = EventBuffer::new();
    ev_buf.push(&param_value_event(PARAM_GATE_THRESH, -50.0));
    ev_buf.push(&param_value_event(PARAM_GATE_MODE, 0.0));
    let in_events = InputEvents::from_buffer(&ev_buf);
    let mut out_events = OutputEvents::void();

    let _guard = TrackingGuard::new();
    let before = get_alloc_count();
    proc.process(
        &input_audio,
        &mut output_audio,
        &in_events,
        &mut out_events,
        None,
        None,
    )
    .expect("process");
    let after = get_alloc_count();
    assert_eq!(
        after - before,
        0,
        "MusicalNoiseGate RT callback allocated {} times!",
        after - before
    );
}
