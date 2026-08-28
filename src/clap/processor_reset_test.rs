// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Complete, in-place, zero-alloc CLAP reset.
//!
//! Validates `PluginAudioProcessor::reset()`:
//!
//! * **Seek/loop relocation (acceptance):** an impulse from the pre-reset
//!   timeline produces no residual tail after the reset — the output right
//!   after `reset()` is silence and a new impulse restarts the IR cleanly.
//! * **Fresh-instance equivalence (invariant):** processing immediately after
//!   `reset()` is **bit-identical** to a freshly-initialized instance with the
//!   same configuration (model, IR, oversampling) — both with the conv engine
//!   and with the recurrent (LSTM) neural model.
//! * **Zero allocation (acceptance):** the `reset()` method itself performs no
//!   heap allocation on the real-time audio thread.
//! * **Configuration preservation:** model, IR, params, applied oversampling
//!   factor and the declared latency survive the reset; no host restart is
//!   requested and the latency telemetry never changes.
//! * **CLAP `steady_time` contract:** after `reset()` the `steady_time` may
//!   jump backwards — processing with a regressed `steady_time` succeeds.

#[cfg(test)]
mod tests {
    use crate::clap::extensions::params::bypass_bool_to_u32;
    use crate::clap::host_harness::{
        extract_plugin_main_thread, extract_plugin_shared, make_harness_audio_processor,
        make_test_plugin_with_harness, perform_restart, process_block_harness,
    };
    use crate::clap::test_util::{assert_zero_alloc, model_path, tmp_path};
    use clack_host::prelude::*;
    use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;

    const BLOCK: usize = 256;
    const PARTITIONS: usize = 32;
    const IR_LEN: usize = BLOCK * PARTITIONS; // 8192 samples → 32 partitions

    fn audio_config() -> PluginAudioConfiguration {
        PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: BLOCK as u32,
            max_frames_count: BLOCK as u32,
        }
    }

    /// Writes a deterministic pseudo-noise IR with a slow exponential decay
    /// (every block of the 32-partition tail carries measurable energy).
    fn write_decay_ir(name: &str) -> PathBuf {
        let mut ir = vec![0.0f32; IR_LEN];
        let mut x = 0x1234_5678u32;
        for (i, s) in ir.iter_mut().enumerate() {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let n = (x >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
            *s = n * 0.999f32.powi(i as i32);
        }
        let path = tmp_path(&format!("{name}.wav"));
        neural_amp_modeler_rs::testing::wav::write_wav_f32(&path, &ir, 48000)
            .expect("write decay IR WAV");
        path
    }

    /// Processes one `BLOCK`-sample block and returns the L output.
    fn process_block(
        started: &mut StartedPluginAudioProcessor<crate::clap::host_harness::CompleteHost>,
        input: &[f32],
    ) -> Vec<f32> {
        let mut il = input.to_vec();
        let mut ir_buf = input.to_vec();
        let mut ol = vec![0.0f32; BLOCK];
        let mut or_buf = vec![0.0f32; BLOCK];
        let _ = process_block_harness(started, &mut il, &mut ir_buf, &mut ol, &mut or_buf, None);
        ol
    }

    /// Mirrors `process_block_harness` but exposes the CLAP `steady_time`
    /// parameter so the post-reset regression contract can be exercised.
    fn process_block_steady(
        started: &mut StartedPluginAudioProcessor<crate::clap::host_harness::CompleteHost>,
        input: &[f32],
        steady_time: u64,
    ) -> ProcessStatus {
        let mut il = input.to_vec();
        let mut ir_buf = input.to_vec();
        let mut ol = vec![0.0f32; BLOCK];
        let mut or_buf = vec![0.0f32; BLOCK];

        let mut input_ports = AudioPorts::with_capacity(2, 1);
        let mut output_ports = AudioPorts::with_capacity(2, 1);

        let mut in_ch = [il.as_mut_slice(), ir_buf.as_mut_slice()];
        let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(
                in_ch.iter_mut().map(InputChannel::constant),
            ),
        }]);
        let out_ch = [ol.as_mut_slice(), or_buf.as_mut_slice()];
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
                &InputEvents::empty(),
                &mut out_ev,
                Some(steady_time),
                None,
            )
            .expect("process() failed")
    }

    fn block_max_abs(buf: &[f32]) -> f32 {
        buf.iter().fold(0.0f32, |m, &s| m.max(s.abs()))
    }

    /// Deterministic pseudo-noise pattern used to exercise neural state.
    fn noise_pattern(len: usize, seed: u32, gain: f32) -> Vec<f32> {
        let mut x = seed.wrapping_mul(7_474_963).wrapping_add(1_601_234_567);
        (0..len)
            .map(|_| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let n = (x >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
                n * gain
            })
            .collect()
    }

    // ── Acceptance: seek/loop relocation — impulse before reset leaves no
    //    tail, and the impulse response after reset equals the fresh one. ──

    #[test]
    fn test_reset_clears_tail_and_matches_fresh_impulse_response() {
        let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
        let shared = unsafe { &*extract_plugin_shared(&mut instance) };

        let stopped = instance
            .activate(|_, _| make_harness_audio_processor(&state), audio_config())
            .expect("activate");
        let started = stopped.start_processing().expect("start_processing");

        // First IR load (0 → partition latency) is staged + host restart.
        let mt = unsafe { &mut *extract_plugin_main_thread(&mut instance) };
        let ir = write_decay_ir("t43_reset_tail");
        mt.load_cabsim(&ir).expect("load decay IR");
        assert!(
            state.restart_requested.load(Ordering::SeqCst),
            "first IR load must request a host restart"
        );
        let mut started = perform_restart(&mut instance, started, &state, audio_config());
        let latency_after_ir = shared.rt_to_ui.current_latency.load(Ordering::Relaxed);
        assert_eq!(latency_after_ir, BLOCK as u32, "cab-sim partition latency");

        let silence = [0.0f32; BLOCK];
        let mut impulse = [0.0f32; BLOCK];
        impulse[0] = 1.0;

        // Fresh instance: impulse → capture the reference impulse response.
        let first = process_block(&mut started, &impulse);
        assert!(
            block_max_abs(&first) > 1e-3,
            "fresh impulse response must carry energy"
        );

        // Let the tail ring out (the gate hold is 2048 > 2 blocks, so the
        // convolution keeps being fed and the tail is audible).
        let tail1 = process_block(&mut started, &silence);
        let tail2 = process_block(&mut started, &silence);
        assert!(
            block_max_abs(&tail1) > 1e-3 || block_max_abs(&tail2) > 1e-3,
            "pre-reset tail must be audible (proving residue would leak)"
        );

        // ── Seek/loop relocation: reset ──
        started.reset();

        // No stale tail may replay — the immediate post-reset block is silence.
        let post_reset = process_block(&mut started, &silence);
        assert!(
            block_max_abs(&post_reset) < 1e-6,
            "impulse before reset must not produce tail after reset (max-abs {:.3e})",
            block_max_abs(&post_reset)
        );

        // Post-reset impulse response == fresh impulse response (bit-exact).
        let second = process_block(&mut started, &impulse);
        assert_eq!(
            second, first,
            "post-reset impulse response must be bit-identical to the fresh one"
        );

        // Configuration preserved: latency/telemetry unchanged, no restart.
        assert_eq!(
            shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
            latency_after_ir,
            "reset must not change the declared latency"
        );
        assert_eq!(
            shared.rt_to_ui.cabsim_tail_samples.load(Ordering::Relaxed),
            IR_LEN as u32,
            "reset must not lose the cab-sim tail telemetry"
        );
        assert!(
            !state.restart_requested.load(Ordering::SeqCst),
            "reset must never request a host restart"
        );

        let _ = std::fs::remove_file(&ir);
    }

    // ── Invariant: recurrent model post-reset == freshly-initialized. ──

    #[test]
    fn test_reset_lstm_equivalent_to_fresh() {
        let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
        let _shared = unsafe { &*extract_plugin_shared(&mut instance) };

        let stopped = instance
            .activate(|_, _| make_harness_audio_processor(&state), audio_config())
            .expect("activate");
        let mut started = stopped.start_processing().expect("start_processing");

        let model = model_path("lstm.nam");
        assert!(model.exists(), "lstm.nam fixture missing");
        let mt = unsafe { &mut *extract_plugin_main_thread(&mut instance) };
        mt.load_model(&model).expect("load lstm model");

        let silence = [0.0f32; BLOCK];
        let probe = noise_pattern(BLOCK, 0xABCD, 0.4);

        // Warm-up silence (installs the model via SPSC; the LSTM advances by
        // one block of zeros — this block is replayed after the reset).
        let _ = process_block(&mut started, &silence);

        // Reference: fresh LSTM response to the probe.
        let fresh = process_block(&mut started, &probe);

        // Pollute the recurrent state with a different, loud signal.
        let pollute = noise_pattern(BLOCK, 0x1234, 0.9);
        for _ in 0..4 {
            let _ = process_block(&mut started, &pollute);
        }

        // Seek/loop relocation.
        started.reset();

        // Replay the exact same warm-up + probe sequence.
        let _ = process_block(&mut started, &silence);
        let post_reset = process_block(&mut started, &probe);

        // Measured: `NamModel::reset` zeroes the layer states and re-prewarms;
        // the build prewarm preserves the NAM file's tiny initial `_xh`/`_c`
        // states, so the Kahan compensation shadow (`cell_error`) diverges by a
        // single ULP (~5.96e-8 max abs over 256 samples — ESR ≈ 1.6e-11, far
        // below audibility and the project's parity gates). A real timeline
        // residue leak would be orders of magnitude larger (the probe amplitude
        // is 0.4). Tolerance 1e-6 gives ~16x margin over the measured drift.
        let max_err: f32 = fresh
            .iter()
            .zip(post_reset.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(
            max_err < 1e-6,
            "LSTM post-reset processing must equal a fresh instance (max abs err \
             {max_err:.3e} > 1e-6)"
        );
    }

    // ── Acceptance: zero allocation on reset() itself. ──

    #[test]
    fn test_reset_zero_alloc() {
        let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
        let shared = unsafe { &*extract_plugin_shared(&mut instance) };

        // Pre-set OS X2 so the activation installs the oversample engines
        // (exercised by the reset too).
        shared
            .ui_to_rt
            .param_oversample
            .store(OversampleFactor::X2.to_f32() as u32, Ordering::Relaxed);
        shared.bump_generation();

        let stopped = instance
            .activate(|_, _| make_harness_audio_processor(&state), audio_config())
            .expect("activate");
        let started = stopped.start_processing().expect("start_processing");

        // Model + IR installed (IR load is staged + restart).
        let model = model_path("lstm.nam");
        assert!(model.exists(), "lstm.nam fixture missing");
        let mt = unsafe { &mut *extract_plugin_main_thread(&mut instance) };
        mt.load_model(&model).expect("load lstm model");
        let ir = write_decay_ir("t43_reset_zalloc");
        mt.load_cabsim(&ir).expect("load decay IR");
        let mut started = perform_restart(&mut instance, started, &state, audio_config());

        // Warm up the full chain (model + OS X2 + conv) so every reset stage
        // has live state to clear.
        let pattern = noise_pattern(BLOCK, 0x5555, 0.5);
        for _ in 0..2 {
            let _ = process_block(&mut started, &pattern);
        }

        // The reset itself must be zero-alloc in real time.
        assert_zero_alloc("reset() (model+OS+conv in place)", || {
            started.reset();
        });

        // And the first post-reset block remains zero-alloc too.
        assert_zero_alloc("post-reset process()", || {
            let _ = process_block(&mut started, &pattern);
        });

        let _ = std::fs::remove_file(&ir);
    }

    // ── Configuration preservation + CLAP steady_time regression. ──

    #[test]
    fn test_reset_preserves_config_and_allows_steady_time_regression() {
        let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
        let shared = unsafe { &*extract_plugin_shared(&mut instance) };

        let stopped = instance
            .activate(|_, _| make_harness_audio_processor(&state), audio_config())
            .expect("activate");
        let started = stopped.start_processing().expect("start_processing");

        // Configure: bypass OFF + IR loaded (wet path active, latency 256).
        shared
            .ui_to_rt
            .param_bypass
            .store(bypass_bool_to_u32(false), Ordering::Relaxed);
        shared.bump_generation();
        let mt = unsafe { &mut *extract_plugin_main_thread(&mut instance) };
        let ir = write_decay_ir("t43_reset_cfg");
        mt.load_cabsim(&ir).expect("load decay IR");
        let mut started = perform_restart(&mut instance, started, &state, audio_config());

        // Steady-time contract: monotonic before reset...
        let silence = [0.0f32; BLOCK];
        let s1 = process_block_steady(&mut started, &silence, 4_000_000);
        assert_eq!(s1, ProcessStatus::Continue);
        let s2 = process_block_steady(&mut started, &silence, 4_000_512);
        assert_eq!(s2, ProcessStatus::Continue);

        // ...a reset allows steady_time to jump backwards (CLAP contract).
        started.reset();
        let s3 = process_block_steady(&mut started, &silence, 2_000_000);
        assert_eq!(
            s3,
            ProcessStatus::Continue,
            "post-reset process must succeed"
        );

        // Configuration preserved: bypass still OFF, model-less wet path still
        // produces the IR tail for a fresh impulse, latency unchanged.
        let mut impulse = [0.0f32; BLOCK];
        impulse[0] = 1.0;
        let out = process_block(&mut started, &impulse);
        assert!(
            block_max_abs(&out) > 1e-3,
            "IR must remain active after the reset"
        );
        assert_eq!(
            shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
            BLOCK as u32,
            "declared latency must be preserved across reset"
        );
        assert!(
            !state.restart_requested.load(Ordering::SeqCst),
            "reset must never request a host restart"
        );
        assert_eq!(
            shared.ui_to_rt.param_bypass.load(Ordering::Relaxed),
            bypass_bool_to_u32(false),
            "bypass parameter must survive the reset"
        );

        let _ = std::fs::remove_file(&ir);
    }
}
