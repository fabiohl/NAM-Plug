// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Integration test suite for BypassCrossfader rapid automation toggling (Finding F12, Task SP3-T03).

use nam_plug::clap::processor::{BYPASS_XFADE_SAMPLES, BypassCrossfader};

#[test]
fn test_rapid_bypass_toggle_consecutive_under_64_samples() {
    let mut xfader = BypassCrossfader::new(false); // start wet (mix = 1.0)
    assert_eq!(xfader.mix, 1.0);
    assert!(!xfader.target);

    // Toggle 1: bypass ON at t = 0
    xfader.trigger(true);
    assert!(xfader.target);
    assert_eq!(xfader.remaining, BYPASS_XFADE_SAMPLES);

    // Advance 15 samples (< 64 samples)
    for _ in 0..15 {
        xfader.mix = (xfader.mix + xfader.step).clamp(0.0, 1.0);
        xfader.remaining -= 1;
    }
    let mix_at_15 = xfader.mix;
    assert_eq!(xfader.remaining, 49);

    // Toggle 2: bypass OFF at t = 15 (< 64 samples elapsed)
    xfader.trigger(false);
    assert!(!xfader.target);
    // Verified: remaining resets back to 64
    assert_eq!(xfader.remaining, BYPASS_XFADE_SAMPLES);
    // Verified: mix level preserves continuity at mix_at_15 (no step jump)
    assert_eq!(xfader.mix, mix_at_15);

    // Advance 25 samples (< 64 samples)
    for _ in 0..25 {
        xfader.mix = (xfader.mix + xfader.step).clamp(0.0, 1.0);
        xfader.remaining -= 1;
    }
    let mix_at_40 = xfader.mix;
    assert_eq!(xfader.remaining, 39);

    // Toggle 3: bypass ON at t = 40 (< 64 samples elapsed since Toggle 2)
    xfader.trigger(true);
    assert!(xfader.target);
    // Verified: remaining resets back to 64
    assert_eq!(xfader.remaining, BYPASS_XFADE_SAMPLES);
    // Verified: mix level preserves continuity at mix_at_40 (no step jump)
    assert_eq!(xfader.mix, mix_at_40);

    // Let the crossfade run to completion (64 samples)
    for _ in 0..64 {
        xfader.mix = (xfader.mix + xfader.step).clamp(0.0, 1.0);
        xfader.remaining -= 1;
    }

    assert_eq!(xfader.remaining, 0);
    assert!((xfader.mix - 0.0).abs() < 1e-6);
}
