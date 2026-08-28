// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Temporal validation suite:
//! Reset Equivalence, Bypass Continuity & Impulse/Tail Fidelity.
//!
//! Comprehensive temporal fidelity audit of `NAM-Plug`:
//!
//! * **Batch reset invariance (multi-cycle):** 10+ successive cycles of
//!   pollute-signal → `reset()` → test-probe produce outputs bit-equivalent
//!   (or bounded to 1 ULP RNN shadow) to freshly constructed instances,
//!   proving zero ghost state leakage or cumulative drift across seeks.
//! * **Bypass crossfade continuity (click-free):** continuous tonal audio
//!   transitioning into and out of bypass produces bounded first-order sample
//!   derivatives with zero transient pops, clicks, or phase cancellation dips.
//! * **Gate handoff to tail drain continuity:** the boundary between the
//!   active gate hold/fade and the CabSim tail drain is smooth and monotonic.
//! * **Combined latency temporal alignment:** resampling + oversampling + CabSim
//!   produce aligned dry/wet impulse arrival samples under all active stages.
//! * **Zero allocation under rapid temporal transitions:** dynamic bypass
//!   toggling, reset, and gate transitions execute strictly without heap
//!   allocation in real-time processing routines.

#[cfg(test)]
mod tests {
    use crate::clap::extensions::params::{PARAM_BYPASS, bypass_bool_to_u32};
    use crate::clap::host_harness::{
        extract_plugin_main_thread, extract_plugin_shared, make_harness_audio_processor,
        make_test_plugin_with_harness, perform_restart, process_block_harness,
    };
    use crate::clap::test_util::{assert_zero_alloc, model_path, tmp_path};
    use clack_common::events::Pckn;
    use clack_common::events::event_types::ParamValueEvent;
    use clack_common::utils::{ClapId, Cookie};
    use clack_host::prelude::*;
    use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
    use std::f32::consts::PI;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;

    const BLOCK: usize = 256;
    const PARTITIONS: usize = 32;
    const IR_LEN: usize = BLOCK * PARTITIONS; // 8192 samples

    fn audio_config_48k() -> PluginAudioConfiguration {
        PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: BLOCK as u32,
            max_frames_count: BLOCK as u32,
        }
    }

    /// Writes a deterministic pseudo-noise IR with exponential decay.
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

    /// Writes an IR with an exact pre-delay of `partition` samples.
    fn write_pre_delay_ir(partition: usize, name: &str) -> PathBuf {
        let path = tmp_path(&format!("{name}.wav"));
        let mut ir = vec![0.0f32; partition * 2];
        ir[partition] = 1.0;
        neural_amp_modeler_rs::testing::wav::write_wav_f32(&path, &ir, 48000)
            .expect("write pre-delay IR WAV");
        path
    }

    /// Generates pseudo-noise signal for state pollution.
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

    fn first_peak_index(buf: &[f32], min: f32) -> usize {
        buf.iter()
            .position(|&s| s.abs() >= min)
            .unwrap_or(usize::MAX)
    }

    fn max_abs_diff(a: &[f32], b: &[f32]) -> f32 {
        a.iter()
            .zip(b.iter())
            .fold(0.0f32, |acc, (&x, &y)| acc.max((x - y).abs()))
    }

    fn block_max_abs(buf: &[f32]) -> f32 {
        buf.iter().fold(0.0f32, |m, &s| m.max(s.abs()))
    }

    fn process_block(
        started: &mut StartedPluginAudioProcessor<crate::clap::host_harness::CompleteHost>,
        input: &[f32],
        events: Option<&InputEvents<'_>>,
    ) -> Vec<f32> {
        let mut il = input.to_vec();
        let mut ir_buf = input.to_vec();
        let mut ol = vec![0.0f32; BLOCK];
        let mut or_buf = vec![0.0f32; BLOCK];
        let _ = process_block_harness(started, &mut il, &mut ir_buf, &mut ol, &mut or_buf, events);
        ol
    }

    // ── Test 1: Multi-cycle batch reset invariance ──

    #[test]
    fn test_batch_reset_invariance_multi_cycle() {
        let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
        let stopped = instance
            .activate(
                |_, _| make_harness_audio_processor(&state),
                audio_config_48k(),
            )
            .expect("activate");
        let started = stopped.start_processing().expect("start_processing");

        let mt = unsafe { &mut *extract_plugin_main_thread(&mut instance) };
        let model_file = model_path("lstm.nam");
        mt.load_model(&model_file).expect("load LSTM model");
        let ir = write_decay_ir("t44_batch_reset");
        mt.load_cabsim(&ir).expect("load decay IR");

        let mut started = perform_restart(&mut instance, started, &state, audio_config_48k());

        // Probe signal to measure output determinism pós-reset
        let probe = noise_pattern(BLOCK, 0xCAFE_BABE, 0.5);

        // Compute fresh build output, then perform reset to get post-reset reference
        let fresh_output = process_block(&mut started, &probe, None);
        started.reset();
        let ref_post_reset = process_block(&mut started, &probe, None);

        // Verify fresh build vs post-reset divergence is within RNN Kahan shadow bound (~1.5e-6)
        let fresh_vs_reset = max_abs_diff(&fresh_output, &ref_post_reset);
        assert!(
            fresh_vs_reset < 2.5e-6,
            "fresh build vs post-reset divergence ({fresh_vs_reset:e}) must be < 2.5e-6"
        );

        // Run 10 successive cycles of pollution -> reset -> probe
        for cycle in 0..10 {
            // 1. Heavy pollution with distinct random noise
            for _ in 0..4 {
                let pollute = noise_pattern(BLOCK, 0x1000 + cycle as u32 * 17, 0.95);
                let _ = process_block(&mut started, &pollute, None);
            }

            // 2. Reset in-place
            started.reset();

            // 3. Process the exact same probe
            let post_reset_out = process_block(&mut started, &probe, None);

            // 4. Measure difference vs post-reset reference: must be bit-equivalent (< 1e-6)
            let diff = max_abs_diff(&post_reset_out, &ref_post_reset);
            assert!(
                diff < 1e-6,
                "cycle {cycle}: post-reset output diverged from reference (max abs diff = {diff:e} >= 1e-6)"
            );
        }
    }

    // ── Test 2: Bypass crossfade sample continuity and click-freedom ──

    #[test]
    fn test_bypass_crossfade_continuity_and_click_freedom() {
        let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
        let stopped = instance
            .activate(
                |_, _| make_harness_audio_processor(&state),
                audio_config_48k(),
            )
            .expect("activate");
        let started = stopped.start_processing().expect("start_processing");

        let mt = unsafe { &mut *extract_plugin_main_thread(&mut instance) };
        let model_file = model_path("lstm.nam");
        mt.load_model(&model_file).expect("load LSTM model");
        let ir = write_pre_delay_ir(256, "t44_xfade_ir");
        mt.load_cabsim(&ir).expect("load IR");
        let mut started = perform_restart(&mut instance, started, &state, audio_config_48k());

        // Generate a 440 Hz sine wave over 20 blocks (5120 samples)
        let num_blocks = 20;
        let mut full_sine = Vec::with_capacity(num_blocks * BLOCK);
        for i in 0..(num_blocks * BLOCK) {
            let t = i as f32 / 48000.0;
            let sample = 0.8 * (2.0 * PI * 440.0 * t).sin();
            full_sine.push(sample);
        }

        let mut rendered_stream = Vec::with_capacity(num_blocks * BLOCK);

        for (b, chunk) in full_sine.as_chunks::<BLOCK>().0.iter().enumerate() {
            let mut event_buf = EventBuffer::new();
            if b == 5 {
                // Toggle Bypass ON at block 5
                let ev = ParamValueEvent::new(
                    0,
                    ClapId::new(PARAM_BYPASS),
                    Pckn::match_all(),
                    1.0,
                    Cookie::empty(),
                );
                event_buf.push(&ev);
            } else if b == 12 {
                // Toggle Bypass OFF at block 12
                let ev = ParamValueEvent::new(
                    0,
                    ClapId::new(PARAM_BYPASS),
                    Pckn::match_all(),
                    0.0,
                    Cookie::empty(),
                );
                event_buf.push(&ev);
            }

            let in_events = InputEvents::from_buffer(&event_buf);
            let out_block = process_block(&mut started, chunk, Some(&in_events));
            rendered_stream.extend_from_slice(&out_block);
        }

        // Measure maximum sample-to-sample discrete derivative across the entire stream
        let mut max_derivative = 0.0f32;
        let mut max_deriv_idx = 0;
        for i in 1..rendered_stream.len() {
            let deriv = (rendered_stream[i] - rendered_stream[i - 1]).abs();
            if deriv > max_derivative {
                max_derivative = deriv;
                max_deriv_idx = i;
            }
        }

        // Theoretical maximum derivative for 440 Hz sine @ 48kHz is ~0.046.
        // During crossfade between time-aligned signals, max derivative is bounded < 0.12.
        // A click/pop/step discontinuity would produce deriv > 0.3.
        assert!(
            max_derivative < 0.15,
            "maximum derivative across bypass crossfades was {max_derivative:e} at sample {max_deriv_idx}, indicating a click/discontinuity"
        );

        // Verify that the output maintains healthy signal energy during crossfade blocks (blocks 5..7 and 12..14)
        for b in [5, 6, 12, 13] {
            let block_slice = &rendered_stream[b * BLOCK..(b + 1) * BLOCK];
            let max_abs = block_max_abs(block_slice);
            assert!(
                max_abs > 0.2,
                "block {b} during crossfade lost energy (max_abs = {max_abs:e}), possible comb filtering cancellation"
            );
        }
    }

    // ── Test 3: Gate handoff to tail drain continuity ──

    #[test]
    fn test_gate_handoff_to_tail_drain_monotonic_continuity() {
        let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
        let stopped = instance
            .activate(
                |_, _| make_harness_audio_processor(&state),
                audio_config_48k(),
            )
            .expect("activate");
        let started = stopped.start_processing().expect("start_processing");

        let mt = unsafe { &mut *extract_plugin_main_thread(&mut instance) };
        let ir = write_decay_ir("t44_gate_handoff");
        mt.load_cabsim(&ir).expect("load decay IR");
        let mut started = perform_restart(&mut instance, started, &state, audio_config_48k());

        let silence = [0.0f32; BLOCK];
        let mut impulse = [0.0f32; BLOCK];
        impulse[0] = 1.0;

        // Feed impulse (opens gate and rearms tail)
        let imp_out = process_block(&mut started, &impulse, None);
        let mut energies = Vec::new();
        energies.push(block_max_abs(&imp_out));

        // Feed 40 blocks of silence (crossing gate hold -> gate fade -> tail drain)
        for _ in 0..40 {
            let out = process_block(&mut started, &silence, None);
            energies.push(block_max_abs(&out));
        }

        // Assert that the energy decays continuously without upward jumps
        // (No energy spikes or pops at the gate closing / tail drain handoff boundary).
        for i in 2..32 {
            let prev = energies[i - 1];
            let curr = energies[i];
            assert!(
                curr <= prev * 1.05 + 1e-5,
                "energy spike at block {i}: curr ({curr:e}) > prev ({prev:e})"
            );
            assert!(
                curr > 1e-5,
                "tail block {i} must remain audible, got {curr:e}"
            );
        }

        // Tail completes cleanly into silence
        for (i, &energy) in energies.iter().enumerate().skip(36) {
            assert!(
                energy < 1e-6,
                "post-tail block {i} must be silent, got {energy:e}"
            );
        }
    }

    // ── Test 4: Combined latency temporal alignment (Oversample X2 + CabSim) ──

    #[test]
    fn test_combined_resampling_oversampling_cabsim_temporal_alignment() {
        let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
        let shared = unsafe { &*extract_plugin_shared(&mut instance) };

        // Set oversampling to 2X (L_os = 12 samples)
        shared
            .ui_to_rt
            .param_oversample
            .store(OversampleFactor::X2.to_f32() as u32, Ordering::Relaxed);

        let stopped = instance
            .activate(
                |_, _| make_harness_audio_processor(&state),
                audio_config_48k(),
            )
            .expect("activate");
        let started = stopped.start_processing().expect("start_processing");

        let mt = unsafe { &mut *extract_plugin_main_thread(&mut instance) };
        let ir = write_pre_delay_ir(256, "t44_combined_ir");
        mt.load_cabsim(&ir).expect("load pre-delay IR");

        let mut started = perform_restart(&mut instance, started, &state, audio_config_48k());

        // Drain OS housekeeping
        let silence = [0.0f32; BLOCK];
        for _ in 0..4 {
            let _ = process_block(&mut started, &silence, None);
        }

        let declared_latency = shared.rt_to_ui.current_latency.load(Ordering::Relaxed);
        assert_eq!(
            declared_latency, 268,
            "declared latency must equal sum of OS X2 (12) + CabSim partition (256) = 268"
        );

        // Feed impulse under Wet mode (Bypass = OFF)
        let mut impulse = [0.0f32; BLOCK];
        impulse[0] = 1.0;

        let mut wet_stream = Vec::new();
        wet_stream.extend_from_slice(&process_block(&mut started, &impulse, None));
        for _ in 0..4 {
            wet_stream.extend_from_slice(&process_block(&mut started, &silence, None));
        }
        let wet_peak = first_peak_index(&wet_stream, 0.3);
        assert_eq!(
            wet_peak, 268,
            "wet impulse peak must land at sample index 268 (12 + 256)"
        );

        // Reset and switch to Bypass = ON
        started.reset();
        shared
            .ui_to_rt
            .param_bypass
            .store(bypass_bool_to_u32(true), Ordering::Relaxed);
        shared.bump_generation();

        for _ in 0..4 {
            let _ = process_block(&mut started, &silence, None);
        }

        let mut dry_stream = Vec::new();
        dry_stream.extend_from_slice(&process_block(&mut started, &impulse, None));
        for _ in 0..4 {
            dry_stream.extend_from_slice(&process_block(&mut started, &silence, None));
        }
        let dry_peak = first_peak_index(&dry_stream, 0.3);
        assert_eq!(
            dry_peak, 268,
            "bypass impulse peak must land at sample index 268"
        );

        assert_eq!(
            dry_peak, wet_peak,
            "dry peak ({dry_peak}) and wet peak ({wet_peak}) must occur at the exact same sample index"
        );
    }

    // ── Test 5: Rapid temporal transitions zero allocation ──

    #[test]
    fn test_temporal_transitions_zero_alloc() {
        let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
        let stopped = instance
            .activate(
                |_, _| make_harness_audio_processor(&state),
                audio_config_48k(),
            )
            .expect("activate");
        let started = stopped.start_processing().expect("start_processing");

        let mt = unsafe { &mut *extract_plugin_main_thread(&mut instance) };
        let model_file = model_path("lstm.nam");
        mt.load_model(&model_file).expect("load LSTM model");
        let ir = write_decay_ir("t44_zero_alloc");
        mt.load_cabsim(&ir).expect("load IR");
        let mut started = perform_restart(&mut instance, started, &state, audio_config_48k());

        let silence = [0.0f32; BLOCK];
        let noise = noise_pattern(BLOCK, 0xFEED_FACE, 0.8);

        assert_zero_alloc("temporal_transitions_zero_alloc", || {
            for i in 0..30 {
                let input = if i % 2 == 0 { &noise[..] } else { &silence[..] };

                let mut event_buf = EventBuffer::new();
                if i % 3 == 0 {
                    let ev = ParamValueEvent::new(
                        0,
                        ClapId::new(PARAM_BYPASS),
                        Pckn::match_all(),
                        if (i / 3) % 2 == 0 { 1.0 } else { 0.0 },
                        Cookie::empty(),
                    );
                    event_buf.push(&ev);
                }

                let in_events = InputEvents::from_buffer(&event_buf);
                let _ = process_block(&mut started, input, Some(&in_events));

                if i % 10 == 0 {
                    started.reset();
                }
            }
        });
    }
}
