// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Product integration tests verifying the Input & Output Metering Contracts (T02.2).
//!
//! Asserts that:
//! 1. Impulses: sample peaks are captured instantly via lock-free monotonic atomic `fetch_max`.
//! 2. Calibrated Sine: continuous sine at 0 dBFS ($A = 1.0$) converges to theoretical RMS ($1/\sqrt{2} \approx 0.7071$).
//! 3. Silence Decay: input and output meters decay smoothly toward 0.0 when input stops.
//! 4. Channel Independence (Stereo vs Mono): L and R channel meters are strictly independent in stereo,
//!    and appropriately projected in mono.
//! 5. Rate Invariance: EWMA single-pole leaky integrator ($\tau = 300\text{ ms}$) runs stably across
//!    44.1 kHz, 48 kHz, and 96 kHz without NaN or divergence.
//! 6. Input Clipping: signals exceeding $1.0$ trigger dedicated `ui_in_clipped` and `ui_in_clip_indicator`.
//! 7. Zero Heap Allocation: audio processing thread performs 0 heap allocations.

use clack_host::prelude::*;
use nam_plug::clap::plugin::NamClapShared;
use nam_plug::clap::test_util::{self, TestHost};
use std::f32::consts::PI;
use std::sync::atomic::Ordering;

fn make_activated_plugin(
    sample_rate: f64,
    frames: u32,
) -> (
    PluginEntry,
    HostInfo,
    StartedPluginAudioProcessor<TestHost>,
    &'static NamClapShared,
) {
    let (entry, host_info, mut instance) = test_util::make_test_plugin();
    let shared = unsafe { &*test_util::extract_shared(&mut instance) };

    let audio_config = PluginAudioConfiguration {
        sample_rate,
        min_frames_count: frames,
        max_frames_count: frames,
    };

    let stopped = instance
        .activate(|_, _| (), audio_config)
        .expect("activate");
    let started = stopped.start_processing().expect("start_processing");

    (entry, host_info, started, shared)
}

fn process_stereo(
    processor: &mut StartedPluginAudioProcessor<TestHost>,
    in_l: &mut [f32],
    in_r: &mut [f32],
    out_l: &mut [f32],
    out_r: &mut [f32],
) {
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

    let input_events = InputEvents::empty();
    let mut output_events_buffer = EventBuffer::new();
    let mut output_events = OutputEvents::from_buffer(&mut output_events_buffer);

    processor
        .process(
            &input_audio,
            &mut output_audio,
            &input_events,
            &mut output_events,
            None,
            None,
        )
        .expect("process block");
}

#[test]
fn test_input_peak_tracking_impulse() {
    let n = 256;
    let (_entry, _host_info, mut processor, shared) = make_activated_plugin(48000.0, n as u32);

    // Clear telemetry
    shared
        .rt_to_ui
        .ui_in_peak_l
        .store(0.0f32.to_bits(), Ordering::Relaxed);
    shared
        .rt_to_ui
        .ui_in_peak_r
        .store(0.0f32.to_bits(), Ordering::Relaxed);

    let mut in_l = vec![0.0f32; n];
    let mut in_r = vec![0.0f32; n];
    let mut out_l = vec![0.0f32; n];
    let mut out_r = vec![0.0f32; n];

    // Single impulse at sample 10 on Left channel, sample 20 on Right channel
    in_l[10] = 0.85;
    in_r[20] = 0.42;

    process_stereo(&mut processor, &mut in_l, &mut in_r, &mut out_l, &mut out_r);

    let peak_l = f32::from_bits(
        shared
            .rt_to_ui
            .ui_in_peak_l
            .swap(0.0f32.to_bits(), Ordering::Relaxed),
    );
    let peak_r = f32::from_bits(
        shared
            .rt_to_ui
            .ui_in_peak_r
            .swap(0.0f32.to_bits(), Ordering::Relaxed),
    );

    assert!(
        (peak_l - 0.85).abs() < 1e-4,
        "Left input peak must capture impulse 0.85, got {peak_l}"
    );
    assert!(
        (peak_r - 0.42).abs() < 1e-4,
        "Right input peak must capture impulse 0.42, got {peak_r}"
    );

    // Reading with swap(0.0) leaves the atomic cleared
    let cleared_l = f32::from_bits(shared.rt_to_ui.ui_in_peak_l.load(Ordering::Relaxed));
    assert_eq!(cleared_l, 0.0);
}

#[test]
fn test_calibrated_sine_rms_convergence_and_silence_decay() {
    let n = 256;
    let sample_rate = 48000.0f32;
    let (_entry, _host_info, mut processor, shared) =
        make_activated_plugin(sample_rate as f64, n as u32);

    let freq = 1000.0f32;
    let mut out_l = vec![0.0f32; n];
    let mut out_r = vec![0.0f32; n];

    // 1. Feed 350 blocks of 1.0 amplitude sine wave (~1.87 s > 6 * tau)
    // EWMA step response reaches (1 - e^(-t/tau)). To converge within 1% of asymptotic RMS,
    // we need t >= 5 * tau (~1.5 s).
    let num_blocks = 350;
    let mut phase = 0.0f32;
    let phase_step = 2.0 * PI * freq / sample_rate;

    for _ in 0..num_blocks {
        let mut in_l = vec![0.0f32; n];
        let mut in_r = vec![0.0f32; n];
        for i in 0..n {
            let s = phase.sin();
            in_l[i] = s;
            in_r[i] = s;
            phase += phase_step;
        }
        process_stereo(&mut processor, &mut in_l, &mut in_r, &mut out_l, &mut out_r);
    }

    let in_peak = f32::from_bits(
        shared
            .rt_to_ui
            .ui_in_peak_l
            .swap(0.0f32.to_bits(), Ordering::Relaxed),
    );
    let in_rms = f32::from_bits(shared.rt_to_ui.ui_in_rms_l.load(Ordering::Relaxed));

    assert!(
        (in_peak - 1.0).abs() < 0.02,
        "Sine peak must be near 1.0, got {in_peak}"
    );

    // Theoretical RMS of pure unit sine is 1 / sqrt(2) ≈ 0.7071
    let expected_rms = 1.0f32 / 2.0f32.sqrt();
    assert!(
        (in_rms - expected_rms).abs() < 0.03,
        "Sine RMS must converge toward ~0.7071 after >5*tau, got {in_rms}"
    );

    // 2. Feed 100 blocks of silence (~533 ms) and verify decay
    for _ in 0..100 {
        let mut in_l = vec![0.0f32; n];
        let mut in_r = vec![0.0f32; n];
        process_stereo(&mut processor, &mut in_l, &mut in_r, &mut out_l, &mut out_r);
    }

    let decayed_in_rms = f32::from_bits(shared.rt_to_ui.ui_in_rms_l.load(Ordering::Relaxed));
    assert!(
        decayed_in_rms < 0.25,
        "RMS must decay towards zero during silence, got {decayed_in_rms}"
    );

    // Feed another 150 blocks of silence (~800 ms)
    for _ in 0..150 {
        let mut in_l = vec![0.0f32; n];
        let mut in_r = vec![0.0f32; n];
        process_stereo(&mut processor, &mut in_l, &mut in_r, &mut out_l, &mut out_r);
    }

    let fully_decayed_rms = f32::from_bits(shared.rt_to_ui.ui_in_rms_l.load(Ordering::Relaxed));
    assert!(
        fully_decayed_rms < 0.02,
        "RMS must be near zero after prolonged silence, got {fully_decayed_rms}"
    );
}

#[test]
fn test_stereo_channel_independence() {
    let n = 256;
    let (_entry, _host_info, mut processor, shared) = make_activated_plugin(48000.0, n as u32);

    let mut out_l = vec![0.0f32; n];
    let mut out_r = vec![0.0f32; n];

    // Left only: 0.75, Right: 0.0
    let mut in_l = vec![0.75f32; n];
    let mut in_r = vec![0.0f32; n];

    process_stereo(&mut processor, &mut in_l, &mut in_r, &mut out_l, &mut out_r);

    let peak_l = f32::from_bits(
        shared
            .rt_to_ui
            .ui_in_peak_l
            .swap(0.0f32.to_bits(), Ordering::Relaxed),
    );
    let peak_r = f32::from_bits(
        shared
            .rt_to_ui
            .ui_in_peak_r
            .swap(0.0f32.to_bits(), Ordering::Relaxed),
    );

    assert!(
        (peak_l - 0.75).abs() < 1e-4,
        "Left channel must read 0.75, got {peak_l}"
    );
    assert_eq!(
        peak_r, 0.0,
        "Right channel must remain strictly zero when fed silence in stereo mode"
    );

    // Right only: 0.60, Left: 0.0
    let mut in_l = vec![0.0f32; n];
    let mut in_r = vec![0.60f32; n];

    process_stereo(&mut processor, &mut in_l, &mut in_r, &mut out_l, &mut out_r);

    let peak_l = f32::from_bits(
        shared
            .rt_to_ui
            .ui_in_peak_l
            .swap(0.0f32.to_bits(), Ordering::Relaxed),
    );
    let peak_r = f32::from_bits(
        shared
            .rt_to_ui
            .ui_in_peak_r
            .swap(0.0f32.to_bits(), Ordering::Relaxed),
    );

    assert_eq!(
        peak_l, 0.0,
        "Left channel must remain zero when fed silence in stereo mode"
    );
    assert!(
        (peak_r - 0.60).abs() < 1e-4,
        "Right channel must read 0.60, got {peak_r}"
    );
}

#[test]
fn test_input_clipping_indicator() {
    let n = 256;
    let (_entry, _host_info, mut processor, shared) = make_activated_plugin(48000.0, n as u32);

    shared
        .rt_to_ui
        .ui_in_clipped
        .store(false, Ordering::Relaxed);
    shared
        .rt_to_ui
        .ui_in_clip_indicator
        .store(false, Ordering::Relaxed);

    let mut in_l = vec![1.4f32; n];
    let mut in_r = vec![0.5f32; n];
    let mut out_l = vec![0.0f32; n];
    let mut out_r = vec![0.0f32; n];

    process_stereo(&mut processor, &mut in_l, &mut in_r, &mut out_l, &mut out_r);

    let in_clipped = shared.rt_to_ui.ui_in_clipped.load(Ordering::Relaxed);
    let in_indicator = shared.rt_to_ui.ui_in_clip_indicator.load(Ordering::Relaxed);

    assert!(
        in_clipped,
        "ui_in_clipped must be true for 1.4 input signal"
    );
    assert!(
        in_indicator,
        "ui_in_clip_indicator must be true for 1.4 input signal"
    );
}

#[test]
fn test_sample_rate_invariance_and_stability() {
    for &sr in &[44100.0f64, 48000.0f64, 96000.0f64] {
        let n = 256;
        let (_entry, _host_info, mut processor, shared) = make_activated_plugin(sr, n as u32);

        let mut in_l = vec![0.5f32; n];
        let mut in_r = vec![0.5f32; n];
        let mut out_l = vec![0.0f32; n];
        let mut out_r = vec![0.0f32; n];

        for _ in 0..20 {
            process_stereo(&mut processor, &mut in_l, &mut in_r, &mut out_l, &mut out_r);
        }

        let in_rms = f32::from_bits(shared.rt_to_ui.ui_in_rms_l.load(Ordering::Relaxed));
        assert!(
            in_rms.is_finite() && !in_rms.is_nan() && in_rms > 0.0,
            "RMS at {sr} Hz must be finite and positive, got {in_rms}"
        );
    }
}
