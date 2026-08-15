// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Deactivate/Reactivate lifecycle test matrix.
//!
//! Validates audio output parity, resource preservation, and robustness
//! across deactivate→activate transitions varying sample rate, buffer
//! size, model presence, and cycle count.

#[cfg(test)]
mod tests {
    use crate::clap::test_util::TestHost;
    use crate::clap::test_util::{self, StereoTestBuffers};
    use clack_host::prelude::*;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;

    const N: usize = 256;
    const SR_48K: f64 = 48000.0;
    const SR_44K1: f64 = 44100.0;

    fn audio_config(sample_rate: f64, max_frames: u32) -> PluginAudioConfiguration {
        PluginAudioConfiguration {
            sample_rate,
            min_frames_count: 1,
            max_frames_count: max_frames,
        }
    }

    fn model_path(name: &str) -> PathBuf {
        crate::clap::test_util::model_path(name)
    }

    fn process_block(
        started: &mut StartedPluginAudioProcessor<TestHost>,
        bufs: &mut StereoTestBuffers,
    ) {
        let mut input_channels = [bufs.in_l.as_mut_slice(), bufs.in_r.as_mut_slice()];
        let input_audio = bufs.input_ports.with_input_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(
                input_channels.iter_mut().map(InputChannel::constant),
            ),
        }]);

        let output_channels = [bufs.out_l.as_mut_slice(), bufs.out_r.as_mut_slice()];
        let mut output_audio = bufs.output_ports.with_output_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
        }]);

        let input_events = InputEvents::empty();
        let mut output_events = OutputEvents::from_buffer(&mut bufs.output_events_buffer);

        started
            .process(
                &input_audio,
                &mut output_audio,
                &input_events,
                &mut output_events,
                None,
                None,
            )
            .unwrap();
    }

    fn audio_is_valid(samples: &[f32], label: &str) {
        for (i, &s) in samples.iter().enumerate() {
            assert!(s.is_finite(), "{label}[{i}] = {s} is non-finite");
        }
    }

    fn rms(samples: &[f32]) -> f64 {
        let sum_sq: f64 = samples.iter().map(|&x| (x as f64).powi(2)).sum();
        (sum_sq / samples.len() as f64).sqrt()
    }

    fn max_diff(a: &[f32], b: &[f32]) -> f64 {
        a.iter()
            .zip(b.iter())
            .map(|(x, y)| (x - y).abs() as f64)
            .fold(0.0f64, f64::max)
    }

    // ── T01: Audio parity — same sample rate and buffer size ──────

    #[test]
    fn test_audio_parity_same_config() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        // Load a small model before activate so both cycles start with it.
        let params = test_util::make_default_params(Some(model_path("lstm.nam")));
        test_util::load_plugin_state(&mut plugin_instance, &params);

        let config = audio_config(SR_48K, N as u32);

        // ── First cycle ──
        let stopped = plugin_instance.activate(|_, _| (), config).unwrap();
        let mut started = stopped.start_processing().unwrap();

        let mut bufs1 = StereoTestBuffers::new(N, 0.3, 0.25);

        // Warm-up: let gate/hysteresis/smoothers converge, model arrive via SPSC.
        for _ in 0..8 {
            process_block(&mut started, &mut bufs1);
        }
        let baseline_l = bufs1.out_l.clone();
        let baseline_r = bufs1.out_r.clone();
        audio_is_valid(&baseline_l, "baseline_l");
        audio_is_valid(&baseline_r, "baseline_r");

        let stopped = started.stop_processing();
        plugin_instance.deactivate(stopped);

        // ── Second cycle (same config) ──
        let stopped = plugin_instance.activate(|_, _| (), config).unwrap();
        let mut started = stopped.start_processing().unwrap();

        let mut bufs2 = StereoTestBuffers::new(N, 0.3, 0.25);
        for _ in 0..8 {
            process_block(&mut started, &mut bufs2);
        }

        audio_is_valid(&bufs2.out_l, "cycle2_l");
        audio_is_valid(&bufs2.out_r, "cycle2_r");

        let diff_l = max_diff(&baseline_l, &bufs2.out_l);
        let diff_r = max_diff(&baseline_r, &bufs2.out_r);
        let tol = 2.0e-5f64;
        assert!(
            diff_l <= tol,
            "L audio parity violated: max_diff={diff_l:.2e} > {tol:.1e}"
        );
        assert!(
            diff_r <= tol,
            "R audio parity violated: max_diff={diff_r:.2e} > {tol:.1e}"
        );
    }

    // ── T02: No model — bypass pass-through parity ────────────────

    #[test]
    fn test_no_model_bypass_parity() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let config = audio_config(SR_48K, N as u32);

        // Cycle 1
        let stopped = plugin_instance.activate(|_, _| (), config).unwrap();
        let mut started = stopped.start_processing().unwrap();
        let mut bufs1 = StereoTestBuffers::new(N, 0.4, 0.35);
        for _ in 0..4 {
            process_block(&mut started, &mut bufs1);
        }
        let rms1_l = rms(&bufs1.out_l);
        let rms1_r = rms(&bufs1.out_r);
        let stopped = started.stop_processing();
        plugin_instance.deactivate(stopped);

        // Cycle 2
        let stopped = plugin_instance.activate(|_, _| (), config).unwrap();
        let mut started = stopped.start_processing().unwrap();
        let mut bufs2 = StereoTestBuffers::new(N, 0.4, 0.35);
        for _ in 0..4 {
            process_block(&mut started, &mut bufs2);
        }
        let stopped = started.stop_processing();
        plugin_instance.deactivate(stopped);

        // Bypass: output ≈ input signal passing through untouched gate.
        // Gate defaults to -70 dB, so -8 dB signal passes through.
        assert!((rms1_l - rms(&bufs2.out_l)).abs() < 1e-6);
        assert!((rms1_r - rms(&bufs2.out_r)).abs() < 1e-6);
        assert!(rms1_l > 0.1, "bypass signal should pass through gate");
    }

    // ── T03: Sample rate change — no crash, valid audio ───────────

    #[test]
    fn test_sample_rate_change_no_crash() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let params = test_util::make_default_params(Some(model_path("lstm.nam")));
        test_util::load_plugin_state(&mut plugin_instance, &params);

        // Cycle 1: 48 kHz
        let config48 = audio_config(SR_48K, N as u32);
        let stopped = plugin_instance.activate(|_, _| (), config48).unwrap();
        let mut started = stopped.start_processing().unwrap();
        let mut bufs = StereoTestBuffers::new(N, 0.3, 0.25);
        for _ in 0..8 {
            process_block(&mut started, &mut bufs);
        }
        let stopped = started.stop_processing();
        plugin_instance.deactivate(stopped);

        // Cycle 2: 44.1 kHz — resampler must be rebuilt.
        let config44 = audio_config(SR_44K1, N as u32);
        let stopped = plugin_instance.activate(|_, _| (), config44).unwrap();
        let mut started = stopped.start_processing().unwrap();
        let mut bufs2 = StereoTestBuffers::new(N, 0.3, 0.25);
        for _ in 0..8 {
            process_block(&mut started, &mut bufs2);
        }

        audio_is_valid(&bufs2.out_l, "44k1_out_l");
        audio_is_valid(&bufs2.out_r, "44k1_out_r");
        assert!(rms(&bufs2.out_l) > 0.001, "44k1 L signal too quiet");
        assert!(rms(&bufs2.out_r) > 0.001, "44k1 R signal too quiet");
    }

    // ── T04: Buffer size change — no crash, valid audio ───────────

    #[test]
    fn test_buffer_size_change_no_crash() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let params = test_util::make_default_params(Some(model_path("lstm.nam")));
        test_util::load_plugin_state(&mut plugin_instance, &params);

        // Cycle 1: buffer 256
        let config256 = audio_config(SR_48K, 256);
        let stopped = plugin_instance.activate(|_, _| (), config256).unwrap();
        let mut started = stopped.start_processing().unwrap();
        let mut bufs = StereoTestBuffers::new(256, 0.3, 0.25);
        for _ in 0..8 {
            process_block(&mut started, &mut bufs);
        }
        let stopped = started.stop_processing();
        plugin_instance.deactivate(stopped);

        // Cycle 2: buffer 512
        let config512 = audio_config(SR_48K, 512);
        let stopped = plugin_instance.activate(|_, _| (), config512).unwrap();
        let mut started = stopped.start_processing().unwrap();
        let mut bufs2 = StereoTestBuffers::new(512, 0.3, 0.25);
        for _ in 0..8 {
            process_block(&mut started, &mut bufs2);
        }

        audio_is_valid(&bufs2.out_l, "buf512_out_l");
        audio_is_valid(&bufs2.out_r, "buf512_out_r");
        assert!(rms(&bufs2.out_l) > 0.001, "buf512 L signal too quiet");
        assert!(rms(&bufs2.out_r) > 0.001, "buf512 R signal too quiet");
    }

    // ── T05: Multiple cycles — 3-cycle stability ──────────────────

    #[test]
    fn test_multiple_deactivate_reactivate_cycles() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let params = test_util::make_default_params(Some(model_path("lstm.nam")));
        test_util::load_plugin_state(&mut plugin_instance, &params);

        let config = audio_config(SR_48K, N as u32);
        let mut reference_rms = None;

        for cycle in 0..3 {
            let stopped = plugin_instance.activate(|_, _| (), config).unwrap();
            let mut started = stopped.start_processing().unwrap();
            let mut bufs = StereoTestBuffers::new(N, 0.3, 0.25);
            for _ in 0..8 {
                process_block(&mut started, &mut bufs);
            }

            audio_is_valid(&bufs.out_l, &format!("cycle{cycle}_l"));
            audio_is_valid(&bufs.out_r, &format!("cycle{cycle}_r"));

            let rms_l = rms(&bufs.out_l);
            if cycle == 0 {
                reference_rms = Some(rms_l);
            } else {
                let ref_rms = reference_rms.unwrap();
                assert!(
                    (rms_l - ref_rms).abs() < 1e-4,
                    "Cycle {cycle} RMS drifted: {rms_l:.6} vs reference {ref_rms:.6}"
                );
            }

            let stopped = started.stop_processing();
            plugin_instance.deactivate(stopped);
        }
    }

    // ── T06: Model preserved — no reload from disk ────────────────

    #[test]
    fn test_model_preserved_across_cycle() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let params = test_util::make_default_params(Some(model_path("lstm.nam")));
        test_util::load_plugin_state(&mut plugin_instance, &params);

        let config = audio_config(SR_48K, N as u32);
        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

        // Cycle 1
        let stopped = plugin_instance.activate(|_, _| (), config).unwrap();
        let mut started = stopped.start_processing().unwrap();
        let mut bufs = StereoTestBuffers::new(N, 0.3, 0.25);
        for _ in 0..8 {
            process_block(&mut started, &mut bufs);
        }

        let counter_after_first = shared.cold.model_load_counter.load(Ordering::Relaxed);
        assert!(
            counter_after_first > 0,
            "model should be loaded in first cycle"
        );

        let stopped = started.stop_processing();
        plugin_instance.deactivate(stopped);

        // Cycle 2 — model preserved, no reload
        let stopped = plugin_instance.activate(|_, _| (), config).unwrap();
        let mut started = stopped.start_processing().unwrap();
        let mut bufs2 = StereoTestBuffers::new(N, 0.3, 0.25);
        for _ in 0..8 {
            process_block(&mut started, &mut bufs2);
        }

        let counter_after_second = shared.cold.model_load_counter.load(Ordering::Relaxed);
        assert_eq!(
            counter_after_second, counter_after_first,
            "model_load_counter should NOT increment on reactivate — model preserved via DeactivatedDspState"
        );

        assert!(
            rms(&bufs2.out_l) > 0.001,
            "model should still produce audio in cycle 2"
        );
    }
}
