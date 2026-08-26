// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! T4.1 / F-DSP-008 — Dry delay line: bypass/crossfade time-alignment tests.
//!
//! Validates the invariant that the delayed dry signal and the wet signal
//! represent the same temporal instant of the input audio at every output
//! sample, preventing comb filtering, transient cancellation and PDC
//! inconsistency in the bypass state.
//!
//! * Impulse alignment: an impulse fed at time `t` produces a peak at exactly
//!   the same output index in both the bypass (delayed dry) and wet paths,
//!   for each latency component (oversampling half-band delay, cab-sim
//!   partition with pre-delay).
//! * Single-peak crossfade: toggling bypass with a live impulse produces a
//!   single coherent peak instead of swallowing or splitting the transient.
//! * Steady-state gain: the delay line preserves DC/gain (rollback condition).
//! * Zero allocation: bypass entry/exit and crossfade remain zero-alloc.

#[cfg(test)]
mod tests {
    use crate::clap::extensions::params::{PARAM_BYPASS, bypass_bool_to_u32};
    use crate::clap::host_harness::{
        extract_plugin_main_thread, extract_plugin_shared, make_harness_audio_processor,
        make_test_plugin_with_harness, perform_restart, process_block_harness,
    };
    use crate::clap::test_util::tmp_path;
    use clack_common::events::Pckn;
    use clack_common::events::event_types::ParamValueEvent;
    use clack_common::utils::{ClapId, Cookie};
    use clack_host::prelude::*;
    use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;

    fn audio_config() -> PluginAudioConfiguration {
        PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 256,
            max_frames_count: 256,
        }
    }

    /// Writes an IR WAV of `2 * partition` samples whose impulse sits exactly
    /// `partition` samples in — a pure `partition`-sample delay. The wet
    /// convolution of an input impulse then lands exactly `partition` samples
    /// later, matching the declared cab-sim latency.
    fn write_pre_delay_ir(partition: usize, name: &str) -> PathBuf {
        let path = tmp_path(&format!("{name}.wav"));
        let mut ir = vec![0.0f32; partition * 2];
        ir[partition] = 1.0;
        neural_amp_modeler_rs::testing::wav::write_wav_f32(&path, &ir, 48000)
            .expect("write pre-delay IR WAV");
        path
    }

    /// Returns the index of the first sample whose absolute value is >= `min`.
    /// Returns `usize::MAX` if no such sample exists.
    fn first_peak_index(buf: &[f32], min: f32) -> usize {
        buf.iter()
            .position(|&s| s.abs() >= min)
            .unwrap_or(usize::MAX)
    }

    /// Sets the bypass parameter via the UI atomic + generation bump so the
    /// first `process()` call syncs bypass=true before the audio block.
    fn set_bypass_on(shared: &crate::clap::plugin::NamClapShared) {
        shared
            .ui_to_rt
            .param_bypass
            .store(bypass_bool_to_u32(true), Ordering::Relaxed);
        shared.bump_generation();
    }

    /// Sets the oversampling factor via the UI atomic + generation bump.
    fn set_oversample(shared: &crate::clap::plugin::NamClapShared, factor: OversampleFactor) {
        shared
            .ui_to_rt
            .param_oversample
            .store(factor.to_f32() as u32, Ordering::Relaxed);
        shared.bump_generation();
    }

    /// Processes one 256-sample block, returning the L output.
    fn process_block(
        started: &mut StartedPluginAudioProcessor<crate::clap::host_harness::CompleteHost>,
        input: &[f32],
        events: Option<&InputEvents<'_>>,
    ) -> Vec<f32> {
        let mut il = input.to_vec();
        let mut ir_buf = input.to_vec();
        let mut ol = vec![0.0f32; 256];
        let mut or_buf = vec![0.0f32; 256];
        let _ = process_block_harness(started, &mut il, &mut ir_buf, &mut ol, &mut or_buf, events);
        ol
    }

    // ── Test 1: bypass (delayed dry) and wet produce the impulse at the same
    //    output index when the only latency source is the cab-sim partition. ──

    #[test]
    fn test_bypass_dry_aligned_with_wet_cabsim() {
        let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
        let shared = unsafe { &*extract_plugin_shared(&mut instance) };

        // Activate and load a pre-delay IR (partition = 256 → L = 256).
        let stopped = instance
            .activate(|_, _| make_harness_audio_processor(&state), audio_config())
            .expect("activate");
        let started = stopped.start_processing().expect("start_processing");

        let mt = unsafe { &mut *extract_plugin_main_thread(&mut instance) };
        let ir = write_pre_delay_ir(256, "t41_cabsim");
        mt.load_cabsim(&ir).expect("load pre-delay IR");
        assert!(
            state.restart_requested.load(Ordering::SeqCst),
            "first IR load must request a host restart"
        );

        let mut started = perform_restart(&mut instance, started, &state, audio_config());
        assert_eq!(
            shared.cold.current_cabsim_latency.load(Ordering::Relaxed),
            256,
            "cabsim partition must be 256 after the restart"
        );
        assert_eq!(
            shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
            256,
            "declared latency must equal the cabsim partition"
        );

        // ── Wet run (bypass OFF): impulse at block 1 index 0 → wet peak at block 2 index 0 ──
        let silence = [0.0f32; 256];
        let mut impulse = [0.0f32; 256];
        impulse[0] = 1.0;

        let _ = process_block(&mut started, &silence, None);
        let _ = process_block(&mut started, &impulse, None);
        let wet_out = process_block(&mut started, &silence, None);

        let wet_peak = first_peak_index(&wet_out, 0.5);
        assert_eq!(
            wet_peak, 0,
            "wet impulse peak must land at block 2 index 0 (L=256 samples after the input impulse)"
        );
        assert!(
            (wet_out[0] - 1.0).abs() < 0.1,
            "wet peak value must be ~1.0, got {}",
            wet_out[0]
        );

        // ── Dry run (bypass ON): same block sequence — delayed dry peak at block 2 index 0 ──
        let stopped = started.stop_processing();
        instance.deactivate(stopped);

        let stopped = instance
            .activate(|_, _| make_harness_audio_processor(&state), audio_config())
            .expect("reactivate");
        set_bypass_on(shared);
        let mut started = stopped.start_processing().expect("start_processing");

        // Block 0: silence (primes the dry delay ring with zeros + drains the
        // bypass crossfade), then the impulse, then silence.
        let _ = process_block(&mut started, &silence, None);
        let _ = process_block(&mut started, &impulse, None);
        let dry_out = process_block(&mut started, &silence, None);

        let dry_peak = first_peak_index(&dry_out, 0.5);
        assert_eq!(
            dry_peak, 0,
            "bypass (delayed dry) impulse peak must land at block 2 index 0 — same index as the wet peak"
        );
        assert!(
            (dry_out[0] - 1.0).abs() < 1e-4,
            "bypass peak value must be exactly 1.0 (pure delay, no filtering), got {}",
            dry_out[0]
        );

        // ── Invariant: dry and wet peaks coincide at the same output sample ──
        assert_eq!(
            wet_peak, dry_peak,
            "dry and wet impulse peaks must coincide at the same output index (T4.1 invariant)"
        );

        let _ = std::fs::remove_file(&ir);
    }

    // ── Test 2: oversampling alone (L=12) — dry and wet peaks coincide. ──

    #[test]
    fn test_bypass_dry_aligned_with_wet_oversample() {
        let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
        let shared = unsafe { &*extract_plugin_shared(&mut instance) };

        // Activate with OS X2 pre-set.
        set_oversample(shared, OversampleFactor::X2);
        let stopped = instance
            .activate(|_, _| make_harness_audio_processor(&state), audio_config())
            .expect("activate");
        let mut started = stopped.start_processing().expect("start_processing");

        // Drain the OS rebuild (main-thread housekeeping delivers SetOversample).
        let silence = [0.0f32; 256];
        for _ in 0..4 {
            let _ = process_block(&mut started, &silence, None);
        }
        assert_eq!(
            shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
            12,
            "declared latency must equal the OS X2 half-band delay (12 samples)"
        );

        // ── Wet run (bypass OFF): impulse at block 0 index 0 → wet peak at index 12 ──
        let mut impulse = [0.0f32; 256];
        impulse[0] = 1.0;
        let wet_out = process_block(&mut started, &impulse, None);
        let wet_peak = first_peak_index(&wet_out, 0.3);
        assert_eq!(
            wet_peak, 12,
            "wet impulse peak must land at index 12 (OS X2 half-band delay)"
        );

        // ── Dry run (bypass ON): same block — delayed dry peak at index 12 ──
        let stopped = started.stop_processing();
        instance.deactivate(stopped);

        set_oversample(shared, OversampleFactor::X2);
        let stopped = instance
            .activate(|_, _| make_harness_audio_processor(&state), audio_config())
            .expect("reactivate");
        set_bypass_on(shared);
        let mut started = stopped.start_processing().expect("start_processing");

        for _ in 0..4 {
            let _ = process_block(&mut started, &silence, None);
        }

        let dry_out = process_block(&mut started, &impulse, None);
        let dry_peak = first_peak_index(&dry_out, 0.3);
        assert_eq!(
            dry_peak, 12,
            "bypass (delayed dry) impulse peak must land at index 12 — same index as the wet peak"
        );
        assert!(
            (dry_out[12] - 1.0).abs() < 1e-4,
            "bypass peak value must be exactly 1.0 (pure delay), got {}",
            dry_out[12]
        );

        assert_eq!(
            wet_peak, dry_peak,
            "dry and wet impulse peaks must coincide (T4.1 invariant)"
        );
    }

    // ── Test 3: bypass toggled with a live impulse — the transient must
    //    survive as a single coherent peak (dry/wet aligned before the sum). ──

    #[test]
    fn test_bypass_crossfade_single_peak_alignment() {
        let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
        let _shared = unsafe { &*extract_plugin_shared(&mut instance) };

        let stopped = instance
            .activate(|_, _| make_harness_audio_processor(&state), audio_config())
            .expect("activate");
        let started = stopped.start_processing().expect("start_processing");

        let mt = unsafe { &mut *extract_plugin_main_thread(&mut instance) };
        let ir = write_pre_delay_ir(256, "t41_xfade");
        mt.load_cabsim(&ir).expect("load pre-delay IR");
        let mut started = perform_restart(&mut instance, started, &state, audio_config());

        // Block 0: silence (wet, fills the cab-sim pipeline).
        let silence = [0.0f32; 256];
        let _ = process_block(&mut started, &silence, None);

        // Block 1: bypass=ON at offset 0 (crossfade over samples 0..63) with an
        // impulse at index 0. The delayed dry of this impulse must land at block
        // 2 index 0 — the same index where the (now paused) wet would have
        // placed it. No transient may be swallowed or split.
        let mut event_buffer = EventBuffer::new();
        let bypass_event = ParamValueEvent::new(
            0u32,
            ClapId::new(PARAM_BYPASS),
            Pckn::match_all(),
            1.0f64,
            Cookie::empty(),
        );
        event_buffer.push(&bypass_event);
        let input_events = InputEvents::from_buffer(&event_buffer);

        let mut impulse = [0.0f32; 256];
        impulse[0] = 1.0;
        let xfade_out = process_block(&mut started, &impulse, Some(&input_events));

        // The crossfade ramp must not lose the transient: no peak before it.
        assert_eq!(
            first_peak_index(&xfade_out, 0.5),
            usize::MAX,
            "no peak must appear in the bypass-transition block (the impulse is in flight)"
        );

        // Block 2: silence, fully bypassed — the delayed dry delivers the impulse.
        let dry_out = process_block(&mut started, &silence, None);
        let dry_peak = first_peak_index(&dry_out, 0.5);
        assert_eq!(
            dry_peak, 0,
            "the in-flight impulse must land at block 2 index 0 as a single peak"
        );
        assert!(
            (dry_out[0] - 1.0).abs() < 1e-4,
            "single peak value must be ~1.0, got {}",
            dry_out[0]
        );

        // Single peak invariant: no second peak anywhere else in block 2.
        let peak_count = dry_out.iter().filter(|&&s| s.abs() >= 0.5).count();
        assert_eq!(
            peak_count, 1,
            "exactly one impulse peak expected in block 2"
        );

        let _ = std::fs::remove_file(&ir);
    }

    // ── Test 4: steady-state gain preservation (rollback condition). ──

    #[test]
    fn test_bypass_steady_state_gain_preserved() {
        let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
        let shared = unsafe { &*extract_plugin_shared(&mut instance) };

        let stopped = instance
            .activate(|_, _| make_harness_audio_processor(&state), audio_config())
            .expect("activate");
        let started = stopped.start_processing().expect("start_processing");

        let mt = unsafe { &mut *extract_plugin_main_thread(&mut instance) };
        let ir = write_pre_delay_ir(256, "t41_ss");
        mt.load_cabsim(&ir).expect("load pre-delay IR");
        let mut started = perform_restart(&mut instance, started, &state, audio_config());

        set_bypass_on(shared);
        // Drain the bypass crossfade (64 samples) + ring priming (256 samples).
        let constant = [0.25f32; 256];
        for _ in 0..3 {
            let _ = process_block(&mut started, &constant, None);
        }

        // After crossfade + priming, the output must be a constant 0.25 —
        // no gain change and no DC shift introduced by the delay line.
        for block in 0..4 {
            let out = process_block(&mut started, &constant, None);
            for (i, &s) in out.iter().enumerate() {
                assert!(
                    (s - 0.25).abs() < 1e-5,
                    "block {block} sample {i}: steady-state gain altered ({s}, expected 0.25)"
                );
            }
        }

        let _ = std::fs::remove_file(&ir);
    }

    // ── Test 5: zero allocation during bypass entry/exit and crossfade. ──

    #[test]
    fn test_bypass_crossfade_zero_alloc() {
        use crate::clap::test_util::assert_zero_alloc;

        let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
        let shared = unsafe { &*extract_plugin_shared(&mut instance) };

        let stopped = instance
            .activate(|_, _| make_harness_audio_processor(&state), audio_config())
            .expect("activate");
        let started = stopped.start_processing().expect("start_processing");

        let mt = unsafe { &mut *extract_plugin_main_thread(&mut instance) };
        let ir = write_pre_delay_ir(256, "t41_zalloc");
        mt.load_cabsim(&ir).expect("load pre-delay IR");
        let mut started = perform_restart(&mut instance, started, &state, audio_config());

        // Warm up (wet path).
        let input = [0.2f32; 256];
        for _ in 0..4 {
            let _ = process_block(&mut started, &input, None);
        }

        // Bypass ON at offset 0 → crossfade entry.
        let mut event_buffer = EventBuffer::new();
        event_buffer.push(&ParamValueEvent::new(
            0u32,
            ClapId::new(PARAM_BYPASS),
            Pckn::match_all(),
            1.0f64,
            Cookie::empty(),
        ));
        let input_events = InputEvents::from_buffer(&event_buffer);
        let mut il = input.to_vec();
        let mut ir_buf = input.to_vec();
        let mut ol = vec![0.0f32; 256];
        let mut or_buf = vec![0.0f32; 256];

        assert_zero_alloc("bypass crossfade entry (T4.1)", || {
            let _ = process_block_harness(
                &mut started,
                &mut il,
                &mut ir_buf,
                &mut ol,
                &mut or_buf,
                Some(&input_events),
            );
        });

        // Bypass steady state (delayed dry).
        for _ in 0..4 {
            let mut il = input.to_vec();
            let mut ir_buf = input.to_vec();
            let mut ol = vec![0.0f32; 256];
            let mut or_buf = vec![0.0f32; 256];
            assert_zero_alloc("bypass steady-state (T4.1)", || {
                let _ = process_block_harness(
                    &mut started,
                    &mut il,
                    &mut ir_buf,
                    &mut ol,
                    &mut or_buf,
                    None,
                );
            });
        }

        // Bypass OFF at offset 0 → crossfade exit.
        let mut event_buffer = EventBuffer::new();
        event_buffer.push(&ParamValueEvent::new(
            0u32,
            ClapId::new(PARAM_BYPASS),
            Pckn::match_all(),
            0.0f64,
            Cookie::empty(),
        ));
        let input_events = InputEvents::from_buffer(&event_buffer);
        let mut il = input.to_vec();
        let mut ir_buf = input.to_vec();
        let mut ol = vec![0.0f32; 256];
        let mut or_buf = vec![0.0f32; 256];

        assert_zero_alloc("bypass crossfade exit (T4.1)", || {
            let _ = process_block_harness(
                &mut started,
                &mut il,
                &mut ir_buf,
                &mut ol,
                &mut or_buf,
                Some(&input_events),
            );
        });

        let _ = shared;
        let _ = std::fs::remove_file(&ir);
    }
}
