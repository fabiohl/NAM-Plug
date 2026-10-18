// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Unit and equivalence tests for `audio_loop` gain ramp kernel (F-NP-R3).

use super::*;
use neural_amp_modeler_rs::dsp::smoother::ParamSmoother;

/// Reference implementation matching the original gain ramp loop arithmetic exactly,
/// used to enforce 0 ULP bit-identity invariant across refactored safe iterators.
fn reference_ramp_apply(
    start: f32,
    target: f32,
    alpha: f32,
    buf_l: &mut [f32],
    buf_r: &mut [f32],
    stereo: bool,
    detect_clip: bool,
) -> (bool, f32) {
    let beta = 1.0 - alpha;
    let diff = start - target;
    let mut bp = beta;
    let mut input_clipped = false;
    let n = buf_l.len();
    for i in 0..n {
        let gain = target + bp * diff;
        buf_l[i] *= gain;
        if stereo {
            buf_r[i] *= gain;
            if detect_clip && (buf_l[i].abs() > 1.0 || buf_r[i].abs() > 1.0) {
                input_clipped = true;
            }
        } else if detect_clip && buf_l[i].abs() > 1.0 {
            input_clipped = true;
        }
        bp *= beta;
    }
    let final_val = if beta <= 0.0 {
        target
    } else {
        target + (bp / beta) * diff
    };
    (input_clipped, final_val)
}

#[test]
fn test_apply_iir_gain_ramp_instant_jump_beta_zero() {
    // S3-T1: ParamSmoother constructed with sample_rate <= 0 forces alpha = 1.0.
    // In this case, beta = 1 - alpha = 0.0.
    let mut smoother = ParamSmoother::new(1.0, 0.0, 20.0);
    assert_eq!(
        smoother.alpha(),
        1.0,
        "Smoother with sample_rate <= 0 must have alpha = 1.0"
    );

    let target = 0.42f32;
    smoother.set_target(target);

    const N: usize = 64;
    let mut buf_l = [0.8f32; N];
    let mut buf_r = [0.8f32; N];
    let mut clipped = false;

    // Execute ramp sub-block
    apply_iir_gain_ramp_sub_block(
        &mut smoother,
        &mut buf_l,
        &mut buf_r,
        0,
        N,
        true,
        &mut clipped,
        false,
    );

    // Verify smoother final value is finite and equals exact target (no NaN from 0/0 division)
    assert!(
        smoother.current_value().is_finite(),
        "Smoother final value must be finite, got {}",
        smoother.current_value()
    );
    assert_eq!(
        smoother.current_value(),
        target,
        "Smoother must snap to exact target on instant jump"
    );

    // Verify all buffer samples are finite and scaled by target
    for (i, &s) in buf_l.iter().enumerate() {
        assert!(s.is_finite(), "buf_l[{i}] must be finite");
        assert_eq!(s, 0.8 * target, "buf_l[{i}] must equal initial * target");
    }
    #[cfg(feature = "dual-mono")]
    for (i, &s) in buf_r.iter().enumerate() {
        assert!(s.is_finite(), "buf_r[{i}] must be finite");
        assert_eq!(s, 0.8 * target, "buf_r[{i}] must equal initial * target");
    }

    // Verify subsequent sub-block: smoother is already at target, fast-path executes cleanly
    let mut buf_l2 = [1.0f32; N];
    let mut buf_r2 = [1.0f32; N];
    apply_iir_gain_ramp_sub_block(
        &mut smoother,
        &mut buf_l2,
        &mut buf_r2,
        0,
        N,
        false,
        &mut clipped,
        false,
    );
    assert!(smoother.current_value().is_finite());
    assert_eq!(smoother.current_value(), target);
    for (i, &s) in buf_l2.iter().enumerate() {
        assert_eq!(s, target, "buf_l2[{i}] must equal target");
    }
}

#[test]
fn test_apply_iir_gain_ramp_instant_jump_mono_path() {
    let mut smoother = ParamSmoother::new(2.0, -1.0, 20.0);
    smoother.set_target(0.1);

    const N: usize = 32;
    let mut buf_l = [0.5f32; N];
    let mut buf_r = [0.5f32; N];
    let mut clipped = false;

    apply_iir_gain_ramp_sub_block(
        &mut smoother,
        &mut buf_l,
        &mut buf_r,
        0,
        N,
        true,
        &mut clipped,
        true, // process_mono = true
    );

    assert!(smoother.current_value().is_finite());
    assert_eq!(smoother.current_value(), 0.1);
    for &s in buf_l.iter() {
        assert!(s.is_finite());
        assert_eq!(s, 0.5 * 0.1);
    }
}

#[test]
fn test_apply_iir_gain_ramp_bit_identity_equivalence() {
    // S3-T2: Invariant: bit-identidade with reference algorithm over random gain trajectories
    // Fixed-seed LCG deterministic PRNG for 100% reproducible tests without external dependencies.
    let mut rng_state = 0x853c49e6748fea9b_u64;
    let mut next_f32 = |min: f32, max: f32| -> f32 {
        rng_state = rng_state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let unit = ((rng_state >> 32) as u32 as f32) / (u32::MAX as f32);
        min + unit * (max - min)
    };

    let block_sizes = [1, 7, 8, 16, 31, 32, 64, 128, 256];
    let sample_rates = [44100.0, 48000.0, 96000.0, 192000.0];

    for &sr in &sample_rates {
        for &n in &block_sizes {
            for trajectory in 0..15 {
                let start_gain = next_f32(0.0, 2.5);
                let target_gain = next_f32(0.0, 2.5);
                let detect_clip = (trajectory % 2) == 0;
                let process_mono = (trajectory % 3) == 0;

                let mut smoother = ParamSmoother::new(start_gain, sr, 20.0);
                smoother.set_target(target_gain);
                let alpha = smoother.alpha();

                // Allocate test buffers and copy them for reference comparison
                let mut test_l: Vec<f32> = (0..n).map(|_| next_f32(-1.5, 1.5)).collect();
                let mut test_r: Vec<f32> = (0..n).map(|_| next_f32(-1.5, 1.5)).collect();
                let mut ref_l = test_l.clone();
                let mut ref_r = test_r.clone();

                let mut test_clipped = false;

                // Run refactored kernel
                apply_iir_gain_ramp_sub_block(
                    &mut smoother,
                    &mut test_l,
                    &mut test_r,
                    0,
                    n,
                    detect_clip,
                    &mut test_clipped,
                    process_mono,
                );

                #[cfg(feature = "dual-mono")]
                let stereo = !process_mono;
                #[cfg(not(feature = "dual-mono"))]
                let stereo = false;

                // Run reference loop
                let (ref_clipped, ref_final) = reference_ramp_apply(
                    start_gain,
                    target_gain,
                    alpha,
                    &mut ref_l,
                    &mut ref_r,
                    stereo,
                    detect_clip,
                );

                // Assert 0 ULP bit-identity on all output samples
                for i in 0..n {
                    assert_eq!(
                        test_l[i].to_bits(),
                        ref_l[i].to_bits(),
                        "Bit-identity mismatch in Left channel at sample {i}, sr={sr}, n={n}: test={}, ref={}",
                        test_l[i],
                        ref_l[i]
                    );
                    if stereo {
                        assert_eq!(
                            test_r[i].to_bits(),
                            ref_r[i].to_bits(),
                            "Bit-identity mismatch in Right channel at sample {i}, sr={sr}, n={n}: test={}, ref={}",
                            test_r[i],
                            ref_r[i]
                        );
                    }
                }

                // Assert clipping detection equivalence
                assert_eq!(
                    test_clipped, ref_clipped,
                    "Clipping detection mismatch at sr={sr}, n={n}, detect_clip={detect_clip}, stereo={stereo}"
                );

                // Assert smoother final value 0 ULP bit-identity
                assert_eq!(
                    smoother.current_value().to_bits(),
                    ref_final.to_bits(),
                    "Smoother final value mismatch: test={}, ref={}",
                    smoother.current_value(),
                    ref_final
                );
            }
        }
    }
}

// ── T7.1 (S7 / F-UX-03): piecewise fused ramp ≡ legacy per-event split ──────

use proptest::prelude::*;

/// Reference "legacy" path: the pre-S7 scheduler ended the sub-block at every
/// gain event and applied `apply_iir_gain_ramp_sub_block` once per piece with
/// the target updated at each event boundary. Reproduced here verbatim so the
/// fused helper can be proven numerically identical to it.
fn legacy_split_ramp(
    smoother: &mut ParamSmoother,
    buf_l: &mut [f32],
    buf_r: &mut [f32],
    sorted_steps: &[(usize, f32)],
    process_mono: bool,
) {
    let n = buf_l.len();
    let mut pos = 0usize;
    for &(off, target) in sorted_steps {
        let off = off.min(n);
        if off > pos {
            apply_iir_gain_ramp_sub_block(
                smoother,
                buf_l,
                buf_r,
                pos,
                off - pos,
                false,
                &mut false,
                process_mono,
            );
            pos = off;
        }
        smoother.set_target(target);
    }
    if pos < n {
        apply_iir_gain_ramp_sub_block(
            smoother,
            buf_l,
            buf_r,
            pos,
            n - pos,
            false,
            &mut false,
            process_mono,
        );
    }
}

fn smoother_for_alpha(alpha: f32) -> ParamSmoother {
    // ParamSmoother derives alpha = 1 - exp(-2π fc / fs); invert to hit `alpha`.
    // (called offline in tests, so a libm-free f32 computation is fine)
    let fs = 48000.0f32;
    let fc = -(fs / (2.0 * std::f32::consts::PI)) * (1.0 - alpha).ln();
    ParamSmoother::new(1.0, fs, fc)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn prop_fused_ramp_bit_identical_to_legacy_split(
        n in 1usize..=512,
        alpha in 0.0002f32..0.4,
        initial in 0.01f32..4.0,
        mut steps in prop::collection::vec((0usize..1024, 0.001f32..8.0f32), 0..=8),
        process_mono in any::<bool>(),
    ) {
        steps.iter_mut().for_each(|s| s.0 = s.0.min(n.saturating_sub(1)));
        steps.sort_by_key(|s| s.0);

        let mut ref_l: Vec<f32> = (0..n).map(|i| ((i as f32) * 0.013).sin()).collect();
        let mut ref_r: Vec<f32> = (0..n).map(|i| ((i as f32) * 0.017).cos()).collect();
        let mut cand_l = ref_l.clone();
        let mut cand_r = ref_r.clone();

        let mut s_ref = smoother_for_alpha(alpha);
        s_ref.set(initial);
        s_ref.set_target(initial);

        let mut s_cand = smoother_for_alpha(alpha);
        s_cand.set(initial);
        s_cand.set_target(initial);

        legacy_split_ramp(&mut s_ref, &mut ref_l, &mut ref_r, &steps, process_mono);

        let gain_steps: Vec<GainStep> = steps
            .iter()
            .map(|&(off, target)| GainStep {
                offset: off as u32,
                target,
                is_input: true,
            })
            .collect();
        apply_gain_ramp_with_steps(
            &mut s_cand,
            &mut cand_l,
            &mut cand_r,
            0,
            n,
            &gain_steps,
            true,
            false,
            &mut false,
            process_mono,
        );

        prop_assert_eq!(s_cand.current_value().to_bits(), s_ref.current_value().to_bits());
        for i in 0..n {
            prop_assert_eq!(cand_l[i].to_bits(), ref_l[i].to_bits());
            if !process_mono {
                prop_assert_eq!(cand_r[i].to_bits(), ref_r[i].to_bits());
            }
        }
    }

    #[test]
    fn prop_empty_steps_equals_single_ramp(
        n in 1usize..=512,
        alpha in 0.0002f32..0.4,
        initial in 0.01f32..4.0,
        target in 0.01f32..8.0f32,
    ) {
        let mut a: Vec<f32> = (0..n).map(|i| ((i as f32) * 0.011).sin()).collect();
        let mut ar: Vec<f32> = vec![0.5; n];
        let mut b = a.clone();
        let mut br: Vec<f32> = vec![0.5; n];

        let mut s_a = smoother_for_alpha(alpha);
        s_a.set(initial);
        s_a.set_target(target);
        apply_iir_gain_ramp_sub_block(&mut s_a, &mut a, &mut ar, 0, n, false, &mut false, true);

        let mut s_b = smoother_for_alpha(alpha);
        s_b.set(initial);
        s_b.set_target(target);
        apply_gain_ramp_with_steps(
            &mut s_b, &mut b, &mut br, 0, n, &[], true, false, &mut false, true,
        );

        prop_assert_eq!(s_b.current_value().to_bits(), s_a.current_value().to_bits());
        for i in 0..n {
            prop_assert_eq!(b[i].to_bits(), a[i].to_bits());
        }
    }
}
