// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

#[test]
fn test_bypass_crossfader_initial_state() {
    let xfader_dry = BypassCrossfader::new(true);
    assert!(xfader_dry.target);
    assert_eq!(xfader_dry.mix, 0.0);
    assert!(!xfader_dry.active);
    assert_eq!(xfader_dry.remaining, 0);

    let xfader_wet = BypassCrossfader::new(false);
    assert!(!xfader_wet.target);
    assert_eq!(xfader_wet.mix, 1.0);
    assert!(!xfader_wet.active);
    assert_eq!(xfader_wet.remaining, 0);
}

#[test]
fn test_bypass_crossfader_single_transition() {
    let mut xfader = BypassCrossfader::new(false);
    xfader.trigger(true);

    assert!(xfader.target);
    assert!(xfader.active);
    assert_eq!(xfader.remaining, BYPASS_XFADE_SAMPLES);
    assert_eq!(xfader.step, -BYPASS_XFADE_INV);
    assert_eq!(xfader.mix, 1.0);
}

#[test]
fn test_bypass_crossfader_rapid_toggle_under_64_samples() {
    let mut xfader = BypassCrossfader::new(false);

    // First trigger: bypass ON (towards dry, mix = 0.0)
    xfader.trigger(true);
    assert_eq!(xfader.remaining, 64);

    // Advance 20 samples along the ramp towards 0.0
    for _ in 0..20 {
        xfader.mix += xfader.step;
        xfader.remaining -= 1;
    }
    let mix_at_sample_20 = xfader.mix;
    let expected_mix_20 = 1.0 - 20.0 * (1.0 / 64.0);
    assert!((mix_at_sample_20 - expected_mix_20).abs() < 1e-6);
    assert_eq!(xfader.remaining, 44);

    // Rapid toggle (< 64 samples elapsed): trigger bypass OFF (towards wet, mix = 1.0)
    xfader.trigger(false);

    // Verify state reset and continuity:
    assert!(!xfader.target);
    assert!(xfader.active);
    assert_eq!(xfader.remaining, BYPASS_XFADE_SAMPLES);
    assert_eq!(xfader.step, BYPASS_XFADE_INV);
    assert_eq!(xfader.mix, mix_at_sample_20);

    // Advance 10 samples along the reversed ramp towards 1.0
    for _ in 0..10 {
        xfader.mix += xfader.step;
        xfader.remaining -= 1;
    }
    let mix_at_sample_30 = xfader.mix;
    let expected_mix_30 = mix_at_sample_20 + 10.0 * (1.0 / 64.0);
    assert!((mix_at_sample_30 - expected_mix_30).abs() < 1e-6);
    assert_eq!(xfader.remaining, 54);

    // Rapid toggle again (< 64 samples elapsed since previous toggle): trigger bypass ON
    xfader.trigger(true);

    assert!(xfader.target);
    assert_eq!(xfader.remaining, BYPASS_XFADE_SAMPLES);
    assert_eq!(xfader.step, -BYPASS_XFADE_INV);
    assert_eq!(xfader.mix, mix_at_sample_30);
}

#[test]
fn test_bypass_crossfader_duplicate_trigger_ignored() {
    let mut xfader = BypassCrossfader::new(false);
    xfader.trigger(true);
    assert_eq!(xfader.remaining, 64);

    for _ in 0..10 {
        xfader.mix += xfader.step;
        xfader.remaining -= 1;
    }
    let remaining_before = xfader.remaining;
    let mix_before = xfader.mix;

    xfader.trigger(true);
    assert_eq!(xfader.remaining, remaining_before);
    assert_eq!(xfader.mix, mix_before);
}
