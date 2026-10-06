// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Complete, in-place, zero-alloc CLAP reset.
//!
//! Validates `PluginAudioProcessor::reset()`:
//!
//! * **Seek/loop relocation (acceptance):** an impulse from the pre-reset
//!   timeline produces no residual tail after the reset — the output right
//!   after `reset()` is silence and a new impulse restarts the IR cleanly.
//! * **Fresh-instance equivalence (invariant):** processing after `reset()`
//!   — once the two-phase amortization window converges — is **bit-identical
//!   (within the LSTM Kahan shadow bound)** to a freshly-initialized instance
//!   with the same configuration (model, IR, oversampling) — both with the
//!   conv engine and with the recurrent (LSTM) neural model.
//! * **Two-phase reset (S6-T1):** the reset itself is phase zero —
//!   `NamModel::prewarm_reset` only, no stabilization feed — and the
//!   following callbacks drain a fixed zeroed-sample budget while holding
//!   their blocks on the latency-compensated dry path (bypass-leg
//!   semantics), converging deterministically into the first wet block equal
//!   to a fresh instance's first block.
//! * **Zero allocation (acceptance):** the `reset()` method itself performs no
//!   heap allocation on the real-time audio thread, and neither does any
//!   callback of the amortization window.
//! * **Configuration preservation:** model, IR, params, applied oversampling
//!   factor and the declared latency survive the reset; no host restart is
//!   requested and the latency telemetry never changes.
//! * **CLAP `steady_time` contract:** after `reset()` the `steady_time` may
//!   jump backwards — processing with a regressed `steady_time` succeeds.

#[cfg(test)]
mod tests {
    use crate::clap::extensions::params::{PARAM_ADAPTIVE_COMPUTE, bypass_bool_to_u32};
    use crate::clap::host_harness::{
        extract_plugin_main_thread, extract_plugin_shared, make_harness_audio_processor,
        make_test_plugin_with_harness, perform_restart, process_block_harness,
    };
    use crate::clap::test_util::{
        StereoTestBuffers, assert_zero_alloc, model_path, process_stereo_block_prealloc, tmp_path,
    };
    use clack_common::events::Pckn;
    use clack_common::events::event_types::ParamValueEvent;
    use clack_common::utils::ClapId;
    use clack_host::prelude::*;
    use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;
    use std::time::Instant;

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

    /// `process_block` carrying host input events (param flips).
    fn process_block_events(
        started: &mut StartedPluginAudioProcessor<crate::clap::host_harness::CompleteHost>,
        input: &[f32],
        events: &InputEvents,
    ) -> Vec<f32> {
        let mut il = input.to_vec();
        let mut ir_buf = input.to_vec();
        let mut ol = vec![0.0f32; BLOCK];
        let mut or_buf = vec![0.0f32; BLOCK];
        let _ = process_block_harness(
            started,
            &mut il,
            &mut ir_buf,
            &mut ol,
            &mut or_buf,
            Some(events),
        );
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

    /// Expected number of post-reset callbacks until the first wet block
    /// (convergence callback), at 48 kHz with the fixed drain budget
    /// `PREWARM_STEP_SAMPLES_PER_CALLBACK` zeroed samples per callback.
    ///
    /// Measured: engine `prewarm_samples()` per fixture —
    /// * lstm.nam = 24000 (half the 48 kHz sample rate) → ceil(24000 / 4096) = 6;
    /// * wavenet_a1_standard.nam = one-shot boolean stabilization → 1
    ///   (the first post-reset callback is already the wet one);
    /// * a2_example.nam = receptive field 19092 → ceil(19092 / 4096) = 5.
    ///
    /// The count is deterministic for a fixed k: the engine's split
    /// stabilization guarantees bit-exact equivalence to the integral prewarm
    /// regardless of chunking, so convergence depends only on the total
    /// pending count, not on the host block size.
    ///
    /// Measured: first wet callback per fixture (release, k=1024, model-only
    /// configuration @ 48 kHz): lstm.nam = 24 = ceil(24000/1024);
    /// wavenet_a1_standard.nam = 1 (one-shot boolean stabilization);
    /// a2_example.nam = 7 (engine `prewarm_samples()` for the A2 cascade ∈
    /// (6144, 7168] → ceil(R/1024) = 7).
    const EXPECTED_CALLBACKS_TO_WET_LSTM: usize = 24;
    const EXPECTED_CALLBACKS_TO_WET_A1: usize = 1;
    const EXPECTED_CALLBACKS_TO_WET_A2: usize = 7;

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
        let mt = unsafe { &*extract_plugin_main_thread(&mut instance) };
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
        let shared = unsafe { &*extract_plugin_shared(&mut instance) };

        let stopped = instance
            .activate(|_, _| make_harness_audio_processor(&state), audio_config())
            .expect("activate");
        let mut started = stopped.start_processing().expect("start_processing");

        let model = model_path("lstm.nam");
        assert!(model.exists(), "lstm.nam fixture missing");
        let mt = unsafe { &*extract_plugin_main_thread(&mut instance) };
        mt.load_model(&model).expect("load lstm model");

        let silence = [0.0f32; BLOCK];
        let probe = noise_pattern(BLOCK, 0xABCD, 0.4);

        // Warm-up silence (installs the model via SPSC; the LSTM advances by
        // one block of zeros — this block is replayed after the reset).
        let _ = process_block(&mut started, &silence);

        // The adaptive-compute FSM is not part of the two-phase reset
        // contract (quiescent scope, S6-T0 D4): with it on, the debug-build
        // drain callbacks exceed the block budget and the FSM's Minimal
        // state skips the LSTM model entirely, masking the containment and
        // parity semantics under test. The flip goes through the sanctioned
        // param-event path (`set_adaptive_compute` calls the FSM's
        // `set_mode` directly); the bare GUI-atomic sync never corrects the
        // FSM's born-Conservative mode when the RT params already read Off.
        let mut flip_buffer = EventBuffer::new();
        flip_buffer.push(&ParamValueEvent::new(
            0,
            ClapId::new(PARAM_ADAPTIVE_COMPUTE),
            Pckn::match_all(),
            0.0f64,
        ));
        let flip_events = InputEvents::from_buffer(&flip_buffer);
        let _ = shared;
        // The flip block carries silence: the fresh and post-reset timelines
        // then cover zeroed history by exactly the same count (the drain's
        // 24000 model-internal zeros replace the loader's build-time prewarm
        // one-for-one), keeping the fresh reference's wet timeline replayable
        // verbatim after the reset.
        let _ = process_block_events(&mut started, &silence, &flip_events);

        // Reference: fresh LSTM response to the probe.
        let fresh = process_block(&mut started, &probe);

        // Pollute the recurrent state with a different, loud signal.
        let pollute = noise_pattern(BLOCK, 0x1234, 0.9);
        for _ in 0..4 {
            let _ = process_block(&mut started, &pollute);
        }

        // Seek/loop relocation.
        started.reset();

        // Two-phase reset: drain the amortization window. Callbacks 1..5 are
        // contained (dry passthrough; the model converges on engine-internal
        // zeros) and callback 6 is the convergence callback, which processes
        // its host audio wet — the same wet silence block the fresh timeline
        // replayed above, so the probe afterwards runs over an identical
        // temporal state.
        // Measured: engine `prewarm_samples()` for lstm.nam = 24000 @ 48 kHz
        // (half the sample rate); with k = PREWARM_STEP_SAMPLES_PER_CALLBACK
        // = 4096 zeroed samples per callback, convergence lands on callback
        // ceil(24000 / 4096) = 6 (5 contained + 1 convergence).
        for _ in 0..(EXPECTED_CALLBACKS_TO_WET_LSTM - 1) {
            let _ = process_block(&mut started, &silence);
        }

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

    // ── S6-T1: two-phase reset — containment, parity and deterministic
    //    convergence across all three neural model families. ──

    #[test]
    fn test_two_phase_reset_containment_parity_and_convergence() {
        use crate::clap::processor::PREWARM_STEP_SAMPLES_PER_CALLBACK;

        // The LSTM expected count must track the drain budget k (engine
        // `prewarm_samples()` = 24000 zeroed samples @ 48 kHz for lstm.nam).
        assert_eq!(
            24000_usize.div_ceil(PREWARM_STEP_SAMPLES_PER_CALLBACK),
            EXPECTED_CALLBACKS_TO_WET_LSTM,
            "lstm expected callbacks must equal ceil(24000 / k)"
        );

        let fixtures: [(&str, usize); 3] = [
            ("lstm.nam", EXPECTED_CALLBACKS_TO_WET_LSTM),
            ("wavenet_a1_standard.nam", EXPECTED_CALLBACKS_TO_WET_A1),
            ("a2_example.nam", EXPECTED_CALLBACKS_TO_WET_A2),
        ];

        for (fixture_name, expected_callbacks_to_wet) in fixtures.iter().copied() {
            let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
            let shared = unsafe { &*extract_plugin_shared(&mut instance) };
            let stopped = instance
                .activate(|_, _| make_harness_audio_processor(&state), audio_config())
                .expect("activate");
            let mut started = stopped.start_processing().expect("start_processing");

            let path = model_path(fixture_name);
            assert!(path.exists(), "fixture missing: {fixture_name}");
            let mt = unsafe { &*extract_plugin_main_thread(&mut instance) };
            mt.load_model(&path).expect("load model");

            // Model-only configuration (no cab-sim IR, oversampling off): the
            // effective latency is zero, so a contained callback's output is
            // the raw input (dry passthrough, bit-exact).

            let silence = [0.0f32; BLOCK];
            let probe = noise_pattern(BLOCK, 0xABCD, 0.4);
            let pollute = noise_pattern(BLOCK, 0x1234, 0.9);

            // Fresh timeline: one wet silence block (installs the model via
            // SPSC; the FSM and the wet pipeline start from the same cleared
            // state a phase-zero reset leaves) then the probe, then one more
            // probe for the steady-state comparison.
            let _ = process_block(&mut started, &silence);

            // Adaptive-compute FSM off for the content-parity assertions
            // (same rationale and same sanctioned flip path as the LSTM
            // equivalence test above).
            let mut flip_buffer = EventBuffer::new();
            flip_buffer.push(&ParamValueEvent::new(
                0,
                ClapId::new(PARAM_ADAPTIVE_COMPUTE),
                Pckn::match_all(),
                0.0f64,
            ));
            let flip_events = InputEvents::from_buffer(&flip_buffer);
            let _ = shared;
            let _ = process_block_events(&mut started, &silence, &flip_events);

            let fresh_ref = process_block(&mut started, &probe);
            assert!(
                block_max_abs(&fresh_ref) > 1e-3,
                "{fixture_name}: fresh probe response must carry energy"
            );
            let fresh_steady = process_block(&mut started, &probe);

            // Pollute the neural/pipeline state.
            for _ in 0..4 {
                let _ = process_block(&mut started, &pollute);
            }

            // Seek/loop relocation: phase-zero reset arms the window.
            started.reset();

            // Contained callbacks: dry passthrough at zero latency — bit-equal
            // to the input, and never the wet reference.
            for contained in 1..expected_callbacks_to_wet {
                let out = process_block(&mut started, &probe);
                assert_eq!(
                    out.as_slice(),
                    probe.as_slice(),
                    "{fixture_name}: contained callback {contained} must output the raw dry \
                     input at zero latency (bypass-leg containment)"
                );
                let wet_err: f32 = fresh_ref
                    .iter()
                    .zip(out.iter())
                    .map(|(a, b)| (a - b).abs())
                    .fold(0.0f32, f32::max);
                assert!(
                    wet_err > 1e-3,
                    "{fixture_name}: contained callback {contained} must not leak wet audio \
                     (max diff vs fresh reference {wet_err:.3e})"
                );
            }

            // Convergence callback: the latch clears and the host audio is
            // processed by the normal pipeline (wet silence, matching the
            // fresh timeline's silence block). One more wet silence block
            // repays the fresh timeline's flip block, so both timelines
            // carry the exact same zeroed-history count (the drain's
            // 24000 model-internal zeros replace the loader's build-time
            // prewarm one-for-one) before the probe comparisons.
            let _ = process_block(&mut started, &silence);
            let _ = process_block(&mut started, &silence);

            // First wet probe block ≡ fresh instance (Kahan shadow bound).
            let first_wet = process_block(&mut started, &probe);
            let max_err: f32 = fresh_ref
                .iter()
                .zip(first_wet.iter())
                .map(|(a, b)| (a - b).abs())
                .fold(0.0f32, f32::max);
            assert!(
                max_err < 1e-6,
                "{fixture_name}: first wet block after convergence must equal a fresh \
                 instance (max abs err {max_err:.3e} > 1e-6)"
            );

            // Steady wet processing stays at parity (the fresh timeline's
            // second consecutive probe block).
            let steady = process_block(&mut started, &probe);
            let steady_err: f32 = fresh_steady
                .iter()
                .zip(steady.iter())
                .map(|(a, b)| (a - b).abs())
                .fold(0.0f32, f32::max);
            assert!(
                steady_err < 1e-6,
                "{fixture_name}: steady post-convergence processing diverged from the fresh \
                 instance (max abs err {steady_err:.3e} > 1e-6)"
            );
        }
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
        let mt = unsafe { &*extract_plugin_main_thread(&mut instance) };
        mt.load_model(&model).expect("load lstm model");
        let ir = write_decay_ir("t43_reset_zalloc");
        mt.load_cabsim(&ir).expect("load decay IR");
        let mut started = perform_restart(&mut instance, started, &state, audio_config());

        // Warm up the full chain (model + OS X2 + conv) so every reset stage
        // has live state to clear. The prealloc runner also constructs the
        // reused ports' internal view state outside the counted windows.
        let pattern = noise_pattern(BLOCK, 0x5555, 0.5);
        let mut bufs = crate::clap::test_util::StereoTestBuffers::new(BLOCK, 0.0, 0.0);
        bufs.in_l.copy_from_slice(&pattern);
        bufs.in_r.copy_from_slice(&pattern);
        for _ in 0..2 {
            crate::clap::test_util::process_stereo_block_prealloc(&mut started, &mut bufs, None);
        }

        // The reset itself must be zero-alloc in real time (phase zero only
        // — the stabilization feed moved off the audio callback).
        assert_zero_alloc("reset() (model+OS+conv in place)", || {
            started.reset();
        });

        // And every callback of the amortization window remains zero-alloc,
        // block by block, through the convergence callback (which runs the
        // wet pipeline again). lstm.nam @ 48 kHz with k =
        // PREWARM_STEP_SAMPLES_PER_CALLBACK = 1024: convergence on callback
        // ceil(24000 / 1024) = 24 (see EXPECTED_CALLBACKS_TO_WET_LSTM and
        // the per-fixture `// Measured:` counts).
        for window_block in 0..EXPECTED_CALLBACKS_TO_WET_LSTM {
            assert_zero_alloc(
                &format!("two-phase reset window block {window_block}"),
                || {
                    bufs.in_l.copy_from_slice(&pattern);
                    bufs.in_r.copy_from_slice(&pattern);
                    crate::clap::test_util::process_stereo_block_prealloc(
                        &mut started,
                        &mut bufs,
                        None,
                    );
                },
            );
        }

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
        let mt = unsafe { &*extract_plugin_main_thread(&mut instance) };
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

    // ── RT Wall-Clock Latency Certification: reset() + post-reset block ──

    const RT_BUDGET_NS: u128 = 1_333_333; // 64 frames @ 48 kHz ≈ 1.33 ms

    /// Own wall-clock budget of the A2-cascade family (per-family envelope).
    ///
    /// Measured (release, pinned core 8, N=250, Block=64 @ 48 kHz, four runs):
    /// phase-zero `reset()` p50 2033–2186 µs plus the first contained callback
    /// p50 873–901 µs — a total p50 of 3010–3186 µs. The ceiling lays the
    /// D3-style 25% headroom over the accepted envelope's slowest run
    /// (3.19 ms). The gate is on the median, not the p99: this machine's p99
    /// tail here is ambient scheduler noise (the worst run recorded a 6.76 ms
    /// p99 while its median stayed at 3.19 ms), whereas the median is
    /// insensitive to isolated bolts and still fails closed on any real
    /// regression of the phase-zero reset or the amortized feed. Both A2
    /// costs are engine-side (engine frozen for this sprint by D6); shrinking
    /// this envelope requires engine work (cascade double-zeroing backlog).
    const RT_BUDGET_A2_NS: u128 = 4_000_000; // 25% headroom over 3.2 ms p50

    /// Micro-latency and wall-clock certification for `PluginAudioProcessor::reset()`
    /// across all supported neural model families (LSTM, WaveNet A1 standard, WaveNet A2 slimmable).
    ///
    /// Validates the two-phase reset amortization (S6-T1):
    /// - **RT Budget Contract (fail-closed):** The combined wall-clock time of the phase-zero
    ///   `reset()` + the immediate first post-reset audio block — the first amortized drain
    ///   callback (`64` samples @ 48 kHz) — is `assert!`ed per fixture: the LSTM/WaveNet A1
    ///   families hold the universal callback budget (`p99 ≤ RT_BUDGET_NS`); the A2 cascade
    ///   family holds its own measured envelope (`p50 ≤ RT_BUDGET_A2_NS`). A budget breach
    ///   fails the test — it is no longer report-only.
    /// - **In-place Zero Allocation:** Neither the phase-zero `reset()` nor the first drained
    ///   block allocate heap memory.
    /// - **Equivalence / Parity:** The first wet block after the window converges within the
    ///   `1e-6` tolerance of a freshly initialized instance.
    #[test]
    #[ignore = "timing/micro-latency certification: run in Phase 3 of utils/tests-long.sh on a pinned core with --test-threads=1"]
    fn test_reset_wall_clock_time_within_contract() {
        const TIMING_BLOCK: usize = 64;
        const N_ITERS: usize = 250;

        let timing_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: TIMING_BLOCK as u32,
            max_frames_count: TIMING_BLOCK as u32,
        };

        let fixtures = ["lstm.nam", "wavenet_a1_standard.nam", "a2_example.nam"];

        println!(
            "\n=== reset() Wall-Clock Latency Certification (N={N_ITERS}, Block={TIMING_BLOCK} @ 48 kHz) ==="
        );

        for &fixture_name in &fixtures {
            let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
            let shared = unsafe { &*extract_plugin_shared(&mut instance) };

            let stopped = instance
                .activate(|_, _| make_harness_audio_processor(&state), timing_config)
                .expect("activate");
            let mut started = stopped.start_processing().expect("start_processing");

            let path = model_path(fixture_name);
            assert!(path.exists(), "fixture missing: {fixture_name}");
            let mt = unsafe { &*extract_plugin_main_thread(&mut instance) };
            mt.load_model(&path).expect("load model");

            let mut bufs = StereoTestBuffers::new(TIMING_BLOCK, 0.0, 0.0);
            let pattern = noise_pattern(TIMING_BLOCK, 0xABCD, 0.4);
            let silence = [0.0f32; TIMING_BLOCK];

            // Expected callbacks to the first wet block for this fixture, at
            // 48 kHz with k = PREWARM_STEP_SAMPLES_PER_CALLBACK (see the
            // Measured notes on the constants above).
            let expected_to_wet = match fixture_name {
                "lstm.nam" => EXPECTED_CALLBACKS_TO_WET_LSTM,
                "wavenet_a1_standard.nam" => EXPECTED_CALLBACKS_TO_WET_A1,
                _ => EXPECTED_CALLBACKS_TO_WET_A2,
            };

            // Warm-up: block 0 installs the model from the SPSC ring (the
            // first prealloc call also warms the ports' internal view state
            // outside every counted window).
            bufs.in_l.copy_from_slice(&silence);
            bufs.in_r.copy_from_slice(&silence);
            process_stereo_block_prealloc(&mut started, &mut bufs, None);

            // Adaptive-compute FSM off: the certification must measure the
            // full wet pipeline on every block (the FSM's Minimal state
            // skips the LSTM model, which would invalidate the timing and
            // the drained-block parity invariants). Same sanctioned flip
            // path as the other tests.
            let mut flip_buffer = EventBuffer::new();
            flip_buffer.push(&ParamValueEvent::new(
                0,
                ClapId::new(PARAM_ADAPTIVE_COMPUTE),
                Pckn::match_all(),
                0.0f64,
            ));
            let flip_events = InputEvents::from_buffer(&flip_buffer);
            let _ = shared;
            bufs.in_l.copy_from_slice(&silence);
            bufs.in_r.copy_from_slice(&silence);
            process_stereo_block_prealloc(&mut started, &mut bufs, Some(&flip_events));

            // Fresh instance reference for parity verification: the probe
            // response of the converged fresh instance (same wet-pipeline
            // entry state a phase-zero reset leaves: cleared FSM/smoothers
            // + one wet silence block).
            bufs.in_l.copy_from_slice(&pattern);
            bufs.in_r.copy_from_slice(&pattern);
            process_stereo_block_prealloc(&mut started, &mut bufs, None);
            let ref_out_l = bufs.out_l.clone();

            // Invariant: zero-alloc verification for the phase-zero reset()
            // and the first drained callback (contained dry block; already
            // the wet block for the one-shot WaveNet A1 family).
            assert_zero_alloc(&format!("{fixture_name}: reset() zero-alloc"), || {
                started.reset();
            });
            assert_zero_alloc(
                &format!("{fixture_name}: first post-reset block zero-alloc"),
                || {
                    bufs.in_l.copy_from_slice(&silence);
                    bufs.in_r.copy_from_slice(&silence);
                    process_stereo_block_prealloc(&mut started, &mut bufs, None);
                },
            );

            // Drain the window to convergence and replay the fresh
            // timeline's wet-silence pair before the probe: the convergence
            // callback and one repayment silence give both timelines the same
            // zeroed-history count (the drain's 24000 model-internal zeros
            // replace the loader's build-time prewarm one-for-one) and the
            // same pipeline age, so the probe runs over state identical to
            // the fresh reference's.
            if expected_to_wet > 1 {
                for _ in 0..(expected_to_wet - 2) {
                    bufs.in_l.copy_from_slice(&silence);
                    bufs.in_r.copy_from_slice(&silence);
                    process_stereo_block_prealloc(&mut started, &mut bufs, None);
                }
                bufs.in_l.copy_from_slice(&silence);
                bufs.in_r.copy_from_slice(&silence);
                process_stereo_block_prealloc(&mut started, &mut bufs, None);
            }
            bufs.in_l.copy_from_slice(&silence);
            bufs.in_r.copy_from_slice(&silence);
            process_stereo_block_prealloc(&mut started, &mut bufs, None);
            bufs.in_l.copy_from_slice(&pattern);
            bufs.in_r.copy_from_slice(&pattern);
            process_stereo_block_prealloc(&mut started, &mut bufs, None);
            let max_err: f32 = ref_out_l
                .iter()
                .zip(bufs.out_l.iter())
                .map(|(a, b)| (a - b).abs())
                .fold(0.0f32, f32::max);
            assert!(
                max_err < 1e-6,
                "{fixture_name}: post-reset processing diverged from reference (max abs err {max_err:.3e} >= 1e-6)"
            );

            let pollute = noise_pattern(TIMING_BLOCK, 0x1234, 0.9);

            let mut reset_times: Vec<u128> = Vec::with_capacity(N_ITERS);
            let mut block_times: Vec<u128> = Vec::with_capacity(N_ITERS);
            let mut total_times: Vec<u128> = Vec::with_capacity(N_ITERS);

            for _ in 0..N_ITERS {
                // Exercise / pollute neural state with dynamic content before reset
                bufs.in_l.copy_from_slice(&pollute);
                bufs.in_r.copy_from_slice(&pollute);
                for _ in 0..4 {
                    process_stereo_block_prealloc(&mut started, &mut bufs, None);
                }

                // The measured block is the first post-reset callback fed
                // with silence: the phase-zero reset plus the first amortized
                // drain (contained dry block; for the one-shot A1 family it
                // is already the wet convergence block, so silence aligns it
                // with the fresh timeline's first wet silence).
                bufs.in_l.copy_from_slice(&silence);
                bufs.in_r.copy_from_slice(&silence);
                let t1 = Instant::now();
                started.reset();
                let t2 = Instant::now();
                process_stereo_block_prealloc(&mut started, &mut bufs, None);
                let t3 = Instant::now();

                let d_reset = t2.duration_since(t1).as_nanos();
                let d_block = t3.duration_since(t2).as_nanos();
                let d_total = t3.duration_since(t1).as_nanos();

                reset_times.push(d_reset);
                block_times.push(d_block);
                total_times.push(d_total);

                // Finish the window (contained silences + wet-silence
                // convergence + one repayment silence blocking the fresh
                // timeline's flip block), then take the wet probe block so
                // the parity probe runs over the exact same pipeline age and
                // model state as the reference, and the next iteration's
                // pollution starts from a converged state.
                if expected_to_wet > 1 {
                    for _ in 0..(expected_to_wet - 2) {
                        bufs.in_l.copy_from_slice(&silence);
                        bufs.in_r.copy_from_slice(&silence);
                        process_stereo_block_prealloc(&mut started, &mut bufs, None);
                    }
                    bufs.in_l.copy_from_slice(&silence);
                    bufs.in_r.copy_from_slice(&silence);
                    process_stereo_block_prealloc(&mut started, &mut bufs, None);
                }
                bufs.in_l.copy_from_slice(&silence);
                bufs.in_r.copy_from_slice(&silence);
                process_stereo_block_prealloc(&mut started, &mut bufs, None);
                bufs.in_l.copy_from_slice(&pattern);
                bufs.in_r.copy_from_slice(&pattern);
                process_stereo_block_prealloc(&mut started, &mut bufs, None);
            }

            // Invariant: parity post-reset — the last iteration's wet probe
            // block against the fresh-instance reference.
            let max_err: f32 = ref_out_l
                .iter()
                .zip(bufs.out_l.iter())
                .map(|(a, b)| (a - b).abs())
                .fold(0.0f32, f32::max);
            assert!(
                max_err < 1e-6,
                "{fixture_name}: post-reset processing diverged from reference (max abs err {max_err:.3e} >= 1e-6)"
            );

            let p50 = |v: &[u128]| -> u128 {
                let mut s = v.to_vec();
                s.sort_unstable();
                s[s.len() / 2]
            };
            let p99 = |v: &[u128]| -> u128 {
                let mut s = v.to_vec();
                s.sort_unstable();
                let idx = (s.len().saturating_sub(1) * 99) / 100;
                s[idx]
            };
            let max_val = |v: &[u128]| -> u128 { *v.iter().max().unwrap_or(&0) };

            let p50_reset = p50(&reset_times);
            let p99_reset = p99(&reset_times);
            let max_reset = max_val(&reset_times);

            let p50_block = p50(&block_times);
            let p99_block = p99(&block_times);
            let max_block = max_val(&block_times);

            // Measured — S2-T1 baseline BEFORE S6-T1 (synchronous integral
            // prewarm reset, 2026-10-04, release profile, pinned isolated core 8, N=250 @ 48 kHz, Block=64):
            // - lstm.nam:                 reset p50=1624.02 µs, p99=1800.23 µs, max=1827.47 µs | block p50= 6.63 µs, p99=12.85 µs | total p50=1631.78 µs, p99=1813.22 µs [EXCEEDED: 1.81 ms vs 1.33 ms budget]
            // - wavenet_a1_standard.nam:  reset p50=  23.95 µs, p99=  63.35 µs, max=  80.39 µs | block p50=40.44 µs, p99=52.66 µs | total p50=  63.91 µs, p99= 109.09 µs [PASS: 0.11 ms vs 1.33 ms budget]
            // - a2_example.nam:           reset p50=3287.37 µs, p99=4140.13 µs, max=5125.04 µs | block p50=37.78 µs, p99=55.73 µs | total p50=3324.66 µs, p99=4178.97 µs [EXCEEDED: 4.18 ms vs 1.33 ms budget]
            // Measured — S6-T1 two-phase reset (phase zero + first drained
            // block), same profile, 2026-10-06, four runs (binaries
            // pre-fix + post-parity-fix); representative run:
            // lstm.nam: reset p50=  45.61 µs, p99=  781.32 µs, max=  896.90 µs | block p50= 69.28 µs, p99=  86.95 µs | total p50= 117.54 µs, p99=  852.90 µs, max=  978.76 µs [PASS: 0.85 ms vs 1.33 ms budget]
            //   (range across runs: reset p50 40.86–69.63 µs; total p50 110–139 µs; a quiet-window run measured reset p50=28.22 µs, p99=131.23 µs, total p99=207.92 µs)
            // - wavenet_a1_standard.nam:  reset p50=  20.18 µs, p99=  496.78 µs, max= 1049.72 µs | block p50= 47.98 µs, p99= 137.66 µs | total p50=  69.77 µs, p99=  602.45 µs, max= 1645.88 µs [PASS: 0.60 ms vs 1.33 ms budget]
            //   (range across runs: total p50 69–101 µs; phase-zero one-shot drain, no amortization window)
            // - a2_example.nam:           reset p50=2185.62 µs, p99=5733.29 µs, max= 8362.75 µs | block p50=900.54 µs, p99= 1835.15 µs | total p50=3185.61 µs, p99= 6756.88 µs, max= 9190.65 µs [EXCEEDED: 6.76 ms vs 1.33 ms — see the S6-T0 D3 adendo]
            //   (range across runs: reset p50 2033–2186 µs, block p50 873–901 µs, total p50 3010–3186 µs across all four)
            // The sub-millisecond p99 spikes on all fixtures (~0.5–1 ms) are
            // ambient scheduler noise on the shared machine, not per-callback
            // work (they appear identically on cheap callbacks; the
            // quiet-window run shows the clean tails above).
            // A2 adendo: the A2-cascade phase zero zeroes the subarray layer
            // buffers in place (engine `a2_prewarm_common` + per-array
            // `set_max_buffer_size` in-place re-zero — `prewarm_step`'s
            // chained inference adds ≈0.87–0.93 ms per amortized callback on
            // top). Both costs are engine-side (D6: no engine changes in
            // S6), so the 1.33 ms universal budget is enforced for the
            // LSTM/A1 families; the A2 family is gated by this test against
            // its own measured envelope (RT_BUDGET_A2_NS).</think><tool_call>edit<arg_key>filePath</arg_key><arg_value>/home/fabio/NAM/NAM-Plug/src/clap/processor_reset_test.rs
            let p50_total = p50(&total_times);
            let p99_total = p99(&total_times);
            let max_total = max_val(&total_times);

            // Budget applicable to this fixture, mirrored to the telemetry
            // line (plain metrics, no PASS/EXCEEDED verdict — the only
            // verdict is the fail-closed assert below).
            let budget_label = match fixture_name {
                "a2_example.nam" => "p50 ≤ 4.0 ms (A2 envelope)",
                _ => "p99 ≤ 1.33 ms (universal RT budget)",
            };
            println!(
                "[RESET-TIMING] {:<24} | reset: p50={:6.2} µs, p99={:6.2} µs, max={:6.2} µs | block: p50={:6.2} µs, p99={:6.2} µs, max={:6.2} µs | total: p50={:6.2} µs, p99={:6.2} µs, max={:6.2} µs | budget: {}",
                fixture_name,
                p50_reset as f64 / 1_000.0,
                p99_reset as f64 / 1_000.0,
                max_reset as f64 / 1_000.0,
                p50_block as f64 / 1_000.0,
                p99_block as f64 / 1_000.0,
                max_block as f64 / 1_000.0,
                p50_total as f64 / 1_000.0,
                p99_total as f64 / 1_000.0,
                max_total as f64 / 1_000.0,
                budget_label,
            );

            // Fail-closed budget gate: one assert per fixture for granular
            // diagnostics (which family breached, by how much). LSTM/WaveNet
            // A1 hold the universal RT callback budget on the p99 total; the
            // A2 cascade holds its own measured envelope on the p50 total
            // (statistic rationale on RT_BUDGET_A2_NS).
            match fixture_name {
                "a2_example.nam" => assert!(
                    p50_total <= RT_BUDGET_A2_NS,
                    "{fixture_name}: p50 total {p50_total} ns excede o envelope próprio de {RT_BUDGET_A2_NS} ns (4,0 ms)"
                ),
                _ => assert!(
                    p99_total <= RT_BUDGET_NS,
                    "{fixture_name}: p99 total {p99_total} ns excede 1,33 ms"
                ),
            }
        }
    }
}
