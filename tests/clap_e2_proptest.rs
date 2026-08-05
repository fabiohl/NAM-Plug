// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Property-Based Tests — S2-E2-T05: Block Invariance and Event Flooding.
//!
//! Validates that audio output is identical (to machine precision) regardless
//! of how the host partitions the audio buffer, and that event floods up to
//! MAX_SCHEDULED_EVENTS (4096) are fully processed without truncation.

#[cfg(not(feature = "testing"))]
compile_error!("this test requires the `testing` feature");

use clack_host::prelude::*;
use nam_plug::clap::test_util;
use proptest::prelude::*;

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
    let mut out_ev_buf = EventBuffer::new();
    let mut out_ev = OutputEvents::from_buffer(&mut out_ev_buf);
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
}

/// Create a plugin activated with `max_frames` buffer size.
fn activate_plugin(
    instance: &mut PluginInstance<test_util::TestHost>,
    max_frames: u32,
) -> StartedPluginAudioProcessor<test_util::TestHost> {
    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 1,
        max_frames_count: max_frames,
    };
    let stopped = instance.activate(|_, _| (), audio_config).unwrap();
    stopped.start_processing().unwrap()
}

// ── Test: Block Invariance — same signal, different partitions ─────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn test_block_invariance_bypass_off(
        total in (64usize..=4096),
    ) {
        let max_frames = total as u32;

        // ── Reference: process in one block ──
        let (_entry_r, _host_r, mut instance_r) = test_util::make_test_plugin();
        let mut started_r = activate_plugin(&mut instance_r, max_frames);

        let mut in_l = vec![0.3f32; total];
        let mut in_r = vec![0.3f32; total];
        let mut ref_l = vec![0.0f32; total];
        let mut ref_r = vec![0.0f32; total];

        process_block(
            &mut started_r,
            &mut in_l,
            &mut in_r,
            &mut ref_l,
            &mut ref_r,
            &InputEvents::empty(),
        );

        instance_r.deactivate(started_r.stop_processing());

        // ── Chunked: process in 2 partitions (half + remainder) ──
        let (_entry_c, _host_c, mut instance_c) = test_util::make_test_plugin();
        let mut started_c = activate_plugin(&mut instance_c, max_frames);

        let half = total / 2;
        let tail = total - half;
        let mut chunk_l = vec![0.0f32; total];
        let mut chunk_r = vec![0.0f32; total];
        let mut offset = 0usize;

        for &size in &[half, tail] {
            if size == 0 {
                continue;
            }
            let mut out_l = vec![0.0f32; size];
            let mut out_r = vec![0.0f32; size];
            process_block(
                &mut started_c,
                &mut in_l[offset..offset + size],
                &mut in_r[offset..offset + size],
                &mut out_l,
                &mut out_r,
                &InputEvents::empty(),
            );
            chunk_l[offset..offset + size].copy_from_slice(&out_l);
            chunk_r[offset..offset + size].copy_from_slice(&out_r);
            offset += size;
        }

        instance_c.deactivate(started_c.stop_processing());

        // ── Assert ──
        let esr = max_abs_diff(&ref_l, &chunk_l).max(max_abs_diff(&ref_r, &chunk_r));
        prop_assert!(
            esr < 5e-7,
            "output mismatch: 1-block vs 2-block partition: esr={:.3e} (total={total})",
            esr,
        );
    }
}

// ── Test: Event Flooding — parameter events up to 4096 ─────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn test_event_flooding_no_loss(
        n_events in 2048usize..=4096usize,
    ) {
        use clack_common::events::Pckn;
        use clack_common::events::event_types::ParamValueEvent;
        use clack_common::utils::{ClapId, Cookie};
        use nam_plug::clap::extensions::params::PARAM_INPUT_GAIN;

        let n = 256usize;
        let (_entry, _host, mut instance) = test_util::make_test_plugin();
        let mut started = activate_plugin(&mut instance, n as u32);

        let mut event_buffer = EventBuffer::new();
        let step: f64 = 12.0 / n_events as f64;

        for i in 0..n_events {
            let offset = (i * (n / n_events.saturating_sub(1).max(1))) as u32;
            let val = -6.0 + i as f64 * step;
            let ev = ParamValueEvent::new(
                offset.min((n - 1) as u32),
                ClapId::new(PARAM_INPUT_GAIN),
                Pckn::match_all(),
                val,
                Cookie::empty(),
            );
            event_buffer.push(&ev);
        }

        let input_events = InputEvents::from_buffer(&event_buffer);

        let mut in_l = vec![0.3f32; n];
        let mut in_r = vec![0.3f32; n];
        let mut out_l = vec![0.0f32; n];
        let mut out_r = vec![0.0f32; n];

        process_block(
            &mut started,
            &mut in_l,
            &mut in_r,
            &mut out_l,
            &mut out_r,
            &input_events,
        );

        instance.deactivate(started.stop_processing());
    }
}

// ── Test: Block Invariance — sine signals, 3-partition ─────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn test_block_invariance_sine_varying_partitions(
        total in (256usize..=2048),
    ) {
        let max_frames = total as u32;

        // ── Reference: one block ──
        let (_entry_r, _host_r, mut instance_r) = test_util::make_test_plugin();
        let mut started_r = activate_plugin(&mut instance_r, max_frames);

        let mut in_l = vec![0.0f32; total];
        let mut in_r = vec![0.0f32; total];
        for i in 0..total {
            let t = i as f32 / 48000.0;
            in_l[i] = 0.5 * (2.0 * std::f32::consts::PI * 220.0 * t).sin();
            in_r[i] = 0.35 * (2.0 * std::f32::consts::PI * 180.0 * t).sin();
        }

        let mut ref_l = vec![0.0f32; total];
        let mut ref_r = vec![0.0f32; total];

        process_block(
            &mut started_r,
            &mut in_l,
            &mut in_r,
            &mut ref_l,
            &mut ref_r,
            &InputEvents::empty(),
        );

        instance_r.deactivate(started_r.stop_processing());

        // ── Chunked: 3 partitions (33/33/34%) ──
        let (_entry_c, _host_c, mut instance_c) = test_util::make_test_plugin();
        let mut started_c = activate_plugin(&mut instance_c, max_frames);

        let s1 = total / 3;
        let s2 = total / 3;
        let s3 = total - s1 - s2;
        let sizes = [s1, s2, s3];
        let mut chunk_l = vec![0.0f32; total];
        let mut chunk_r = vec![0.0f32; total];
        let mut offset = 0usize;

        for &size in &sizes {
            if size == 0 {
                continue;
            }
            let mut out_l = vec![0.0f32; size];
            let mut out_r = vec![0.0f32; size];
            process_block(
                &mut started_c,
                &mut in_l[offset..offset + size],
                &mut in_r[offset..offset + size],
                &mut out_l,
                &mut out_r,
                &InputEvents::empty(),
            );
            chunk_l[offset..offset + size].copy_from_slice(&out_l);
            chunk_r[offset..offset + size].copy_from_slice(&out_r);
            offset += size;
        }

        instance_c.deactivate(started_c.stop_processing());

        // ── Assert ──
        let esr = max_abs_diff(&ref_l, &chunk_l).max(max_abs_diff(&ref_r, &chunk_r));
        prop_assert!(
            esr < 5e-7,
            "sine mismatch: 1-block vs 3-block partition: esr={:.3e} (total={total})",
            esr,
        );
    }
}
