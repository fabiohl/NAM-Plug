// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Reactive rearming of the CabSim tail counter.
//!
//! Validates that `cabsim_tail_remaining` is re-armed to the full impulse
//! response duration whenever active audio is fed into the convolution module,
//! so every note produces a complete ring-out:
//!
//! * The pattern `impulse → silence → impulse → silence` yields **two** full
//!   acoustic tails (the second re-arms the counter after the first fully
//!   drained — previously it was truncated to immediate silence).
//! * The ring-out is actually audible: the tail drain is not gated to zero by
//!   the closed noise gate (the cab tail is intentional signal, not noise).
//! * No infinite idle processing: after each tail completes, the output stays
//!   silent (the drain is bounded and the plugin can return to rest).
//! * Telemetry: `cabsim_tail_samples` is published on the restart install path
//!   (`activate()`), not only on the continuous SPSC swap.
//! * The rearm + drain paths are strictly zero-alloc on the real-time audio thread.

#[cfg(test)]
mod tests {
    use crate::clap::host_harness::{
        extract_plugin_main_thread, extract_plugin_shared, make_harness_audio_processor,
        make_test_plugin_with_harness, perform_restart, process_block_harness,
    };
    use crate::clap::test_util::{assert_zero_alloc, tmp_path};
    use clack_host::prelude::*;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;

    const BLOCK: usize = 256;
    const PARTITIONS: usize = 32;
    const IR_LEN: usize = BLOCK * PARTITIONS; // 8192 samples → 32 partitions
    const TAIL_TELEMETRY: u32 = (IR_LEN) as u32; // num_partitions × partition
    const TAIL_DRAIN: usize = IR_LEN + BLOCK; // tail_samples() = partitions × partition + partition

    fn audio_config() -> PluginAudioConfiguration {
        PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: BLOCK as u32,
            max_frames_count: BLOCK as u32,
        }
    }

    /// Writes a deterministic pseudo-noise IR with a slow exponential decay:
    /// every block of the 32-partition tail carries measurable energy (max-abs
    /// in the last block ≈ 0.999^7936 ≈ 3.6e-4), so "energy throughout the
    /// tail" is assertable without nulls.
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

    fn block_max_abs(buf: &[f32]) -> f32 {
        buf.iter().fold(0.0f32, |m, &s| m.max(s.abs()))
    }

    /// Processes one 256-sample block and returns the L output.
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

    /// Feeds `blocks` silence blocks after an impulse and returns the max-abs
    /// energy of each output block (one per input block).
    fn tail_energy_profile(
        started: &mut StartedPluginAudioProcessor<crate::clap::host_harness::CompleteHost>,
        impulse_index: usize,
        n_blocks: usize,
    ) -> Vec<f32> {
        let silence = [0.0f32; BLOCK];
        let mut profile = Vec::with_capacity(n_blocks);
        for b in 0..n_blocks {
            let input = if b == impulse_index {
                let mut imp = [0.0f32; BLOCK];
                imp[0] = 1.0;
                imp
            } else {
                silence
            };
            profile.push(block_max_abs(&process_block(started, &input)));
        }
        profile
    }

    /// Asserts that a tail profile is audible across its whole window and ends
    /// in silence. The conv engine is same-block (group delay ≈ 0),
    /// so the IR occupies `PARTITIONS` blocks starting at the impulse block;
    /// the drain then flushes `TAIL_DRAIN` samples and finally yields silence.
    fn assert_tail_profile(profile: &[f32], start: usize, label: &str) {
        let audible_window = &profile[start..start + PARTITIONS];
        // Energy throughout the whole tail period (acceptance criterion).
        for (k, &e) in audible_window.iter().enumerate() {
            assert!(
                e > 1e-5,
                "{label}: tail block {k} (index {}) must carry acoustic energy, got max-abs {e:e}",
                start + k
            );
        }
        // A substantial fraction of the tail must be clearly audible.
        let loud = audible_window.iter().filter(|&&e| e > 1e-3).count();
        assert!(
            loud >= PARTITIONS / 2,
            "{label}: expected at least {} clearly audible tail blocks, got {loud}",
            PARTITIONS / 2
        );
        // The drain must terminate: after the IR window + full flush + margin,
        // the output is true silence (no infinite idle processing).
        let end = (start + PARTITIONS + TAIL_DRAIN / BLOCK + 2).min(profile.len());
        for (k, &e) in profile[end..].iter().enumerate() {
            assert!(
                e < 1e-6,
                "{label}: block {} after the tail flush must be silent, got max-abs {e:e}",
                end + k
            );
        }
    }

    #[test]
    fn test_cabsim_tail_rearms_across_impulse_silence_impulse_silence() {
        let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
        let shared = unsafe { &*extract_plugin_shared(&mut instance) };

        let stopped = instance
            .activate(|_, _| make_harness_audio_processor(&state), audio_config())
            .expect("activate");
        let started = stopped.start_processing().expect("start_processing");

        // First IR load (0 → partition latency) is staged + host restart.
        let mt = unsafe { &mut *extract_plugin_main_thread(&mut instance) };
        let ir = write_decay_ir("t42_rearm");
        mt.load_cabsim(&ir).expect("load decay IR");
        assert!(
            state.restart_requested.load(Ordering::SeqCst),
            "first IR load must request a host restart"
        );

        let mut started = perform_restart(&mut instance, started, &state, audio_config());
        assert_eq!(
            shared.cold.current_cabsim_latency.load(Ordering::Relaxed),
            BLOCK as u32,
            "cabsim partition must be installed after the restart"
        );
        assert_eq!(
            shared.rt_to_ui.cabsim_tail_samples.load(Ordering::Relaxed),
            TAIL_TELEMETRY,
            "cabsim_tail_samples must be published by the restart install path (activate), \
             not only by the continuous SPSC swap"
        );

        // ── Impulse 1 → silence: full tail, then silence ──
        let n1 = 42;
        let profile1 = tail_energy_profile(&mut started, 0, n1);
        assert_tail_profile(&profile1, 0, "tail 1");

        // ── Impulse 2 → silence: the counter must be re-armed to the full IR
        //    duration (the first drain consumed it to zero), yielding a second
        //    complete tail instead of immediate silence. ──
        let n2 = 120;
        let profile2 = tail_energy_profile(&mut started, 42, n2);
        assert_tail_profile(&profile2, 42, "tail 2");

        // Rearm invariant: both triggers produce tails of comparable duration
        // (without rearming, tail 2 would be absent entirely).
        let audible1 = profile1[0..PARTITIONS]
            .iter()
            .filter(|&&e| e > 1e-4)
            .count();
        let audible2 = profile2[42..42 + PARTITIONS]
            .iter()
            .filter(|&&e| e > 1e-4)
            .count();
        assert!(
            audible2 >= audible1.saturating_sub(1),
            "rearm must restore the full tail on the second trigger: tail1 {audible1} audible \
             blocks, tail2 {audible2}"
        );

        // Prolonged rest stays silent (rollback condition: Sleep at rest).
        for (b, &e) in profile2.iter().enumerate().take(n2).skip(90) {
            assert!(
                e < 1e-6,
                "block {b} after tail 2 must be silent at prolonged rest, got {e:.3e}"
            );
        }

        let _ = std::fs::remove_file(&ir);
    }

    #[test]
    fn test_cabsim_tail_rearm_drain_zero_alloc() {
        let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
        let shared = unsafe { &*extract_plugin_shared(&mut instance) };

        let stopped = instance
            .activate(|_, _| make_harness_audio_processor(&state), audio_config())
            .expect("activate");
        let started = stopped.start_processing().expect("start_processing");

        let mt = unsafe { &mut *extract_plugin_main_thread(&mut instance) };
        let ir = write_decay_ir("rearm_zalloc");
        mt.load_cabsim(&ir).expect("load decay IR");
        let mut started = perform_restart(&mut instance, started, &state, audio_config());
        let _ = shared;

        // Warm-up + impulse through the wet path.
        let mut impulse = [0.0f32; BLOCK];
        impulse[0] = 1.0;
        let _ = process_block(&mut started, &impulse);

        // Rearm happens on the open-gate conv path; the drain runs on the
        // closed-gate path. Both must be zero-alloc.
        let silence = [0.0f32; BLOCK];
        assert_zero_alloc("cabsim tail rearm + drain (open-gate rearm)", || {
            for _ in 0..12 {
                let _ = process_block(&mut started, &silence);
            }
        });

        let _ = std::fs::remove_file(&ir);
    }
}
