// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! # Integration Tests for the Adaptive Telemetry Timer (F-UX-08)
//!
//! Covers the telemetry-timer hygiene invariants:
//!
//! 1. Suspension: after `hide` (dispatched through the `set_telemetry_active`
//!    bridge) no telemetry tick is scheduled at all; `show` re-arms the timer.
//! 2. IEC 60268-10 ballistics are unchanged on a synthetic peak sequence —
//!    the calibrated `BALLISTICS_DECAY_RATE`/`PEAK_HOLD_FRAMES` behavior is
//!    identical before and after the adaptive cadence.
//! 3. The 30 Hz cadence reduction is only allowed in the silence state after
//!    an idle window; any signal or interaction returns to 60 Hz at once.
//!
//! Slint pins its platform (and the winit event loop) to the first thread that
//! initializes it — the exact constraint documented in `src/clap/gui/worker.rs` —
//! and libtest runs every `#[test]` on its own thread. All window-creating
//! tests in a binary therefore share this single phased test, matching the
//! one-window-per-binary convention of `slint_view_model_test.rs` and
//! `slint_widgets_test.rs`.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use nam_plug::clap::gui::slint_view_model::{
    BALLISTICS_DECAY_RATE, PEAK_HOLD_FRAMES, TELEMETRY_PERIOD_FULL, TELEMETRY_PERIOD_LOW,
    amp_to_meter_frac,
};
use nam_plug::clap::gui::{MainWindow, SlintViewModel};
use nam_plug::clap::plugin::GuiSharedState;
use nam_plug::clap::plugin::shared::UI_NO_INTERACTION;
use slint::ComponentHandle;

#[test]
fn test_telemetry_timer_cadence_and_ballistics() {
    let shared = GuiSharedState::new_test();
    let window = MainWindow::new().expect("Failed to create MainWindow");
    let mut vm = SlintViewModel::new(window.clone_strong(), Arc::clone(&shared), None, "Test");

    // ── Phase 1: Suspension — after hide, no tick is scheduled ───────────
    assert!(
        vm.is_telemetry_running(),
        "the telemetry timer is scheduled while the window is shown"
    );

    // `gui.hide()` dispatches this callback into the event loop.
    window.invoke_set_telemetry_active(false);
    assert!(
        !vm.is_telemetry_running(),
        "after hide no telemetry tick may be scheduled"
    );
    vm.tick_telemetry();
    assert!(
        !vm.is_telemetry_running(),
        "a manual tick must not re-arm a suspended timer"
    );

    // `gui.show()` dispatches the resume.
    window.invoke_set_telemetry_active(true);
    assert!(
        vm.is_telemetry_running(),
        "show must re-arm the telemetry timer"
    );

    // The direct entry point behaves identically.
    vm.set_telemetry_active(false);
    assert!(!vm.is_telemetry_running());
    vm.set_telemetry_active(true);
    assert!(vm.is_telemetry_running());

    // ── Phase 2: Adaptive cadence (30 Hz only in silence + idle) ─────────
    // Freshly noted interaction keeps the full rate.
    shared.note_ui_interaction();
    vm.tick_telemetry();
    assert_eq!(vm.telemetry_period(), TELEMETRY_PERIOD_FULL);

    // Silence (no peaks, no holds) plus no interaction in the idle window
    // (re-forcing the never-interacted idle state): cadence drops to 30 Hz,
    // but the timer keeps being scheduled.
    shared
        .cold
        .ui_last_interaction_ms
        .store(UI_NO_INTERACTION, Ordering::Relaxed);
    vm.tick_telemetry();
    assert_eq!(vm.telemetry_period(), TELEMETRY_PERIOD_LOW);
    assert!(vm.is_telemetry_running());

    // Any interaction returns to 60 Hz immediately.
    window.invoke_toggle_bypass();
    vm.tick_telemetry();
    assert_eq!(vm.telemetry_period(), TELEMETRY_PERIOD_FULL);

    // And so does any signal, without interaction.
    shared
        .cold
        .ui_last_interaction_ms
        .store(UI_NO_INTERACTION, Ordering::Relaxed);
    vm.tick_telemetry();
    assert_eq!(vm.telemetry_period(), TELEMETRY_PERIOD_LOW);
    shared
        .rt_to_ui
        .ui_peak_l
        .store(0.5f32.to_bits(), Ordering::Relaxed);
    vm.tick_telemetry();
    assert_eq!(vm.telemetry_period(), TELEMETRY_PERIOD_FULL);

    // ── Phase 3: Ballistics unchanged on a synthetic peak sequence ───────
    let peak_0db = amp_to_meter_frac(1.0);

    // Attack frame: raw 1.0 on both channels (resets the hold window).
    shared
        .rt_to_ui
        .ui_peak_l
        .store(1.0f32.to_bits(), Ordering::Relaxed);
    shared
        .rt_to_ui
        .ui_peak_r
        .store(1.0f32.to_bits(), Ordering::Relaxed);
    vm.tick_telemetry();
    assert!((window.get_peak_l() - peak_0db).abs() < 1e-4);
    assert!((window.get_peak_r() - peak_0db).abs() < 1e-4);
    assert!((window.get_peak_hold_l() - peak_0db).abs() < 1e-4);
    assert!((window.get_peak_hold_r() - peak_0db).abs() < 1e-4);

    // 10 silent frames: current decays by one factor per frame, the hold
    // stays pinned at the peak.
    for _ in 0..10 {
        vm.tick_telemetry();
    }
    let decayed_10 = amp_to_meter_frac(BALLISTICS_DECAY_RATE.powi(10));
    assert!(
        (window.get_peak_l() - decayed_10).abs() < 1e-4,
        "peak after 10 silent frames: got {}, want {}",
        window.get_peak_l(),
        decayed_10
    );
    assert!((window.get_peak_hold_l() - peak_0db).abs() < 1e-4);

    // Hold expiry: PEAK_HOLD_FRAMES silent frames after the attack leave the
    // hold untouched; the next frame releases it by one decay factor.
    for _ in 0..(PEAK_HOLD_FRAMES - 10) {
        vm.tick_telemetry();
    }
    assert!(
        (window.get_peak_hold_l() - peak_0db).abs() < 1e-4,
        "hold must stay pinned for exactly {PEAK_HOLD_FRAMES} frames"
    );
    vm.tick_telemetry();
    let hold_released = amp_to_meter_frac(BALLISTICS_DECAY_RATE);
    assert!(
        (window.get_peak_hold_l() - hold_released).abs() < 1e-4,
        "hold after release: got {}, want {}",
        window.get_peak_hold_l(),
        hold_released
    );

    // The synthetic sequence keeps the cadence at the calibrated 60 Hz rate.
    assert_eq!(vm.telemetry_period(), TELEMETRY_PERIOD_FULL);
}
