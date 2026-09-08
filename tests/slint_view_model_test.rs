// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! # Integration Tests for Slint View-Model (Sprint 4)
//!
//! Validates:
//! 1. Initial property synchronization between `GuiSharedState` and Slint `MainWindow`.
//! 2. UI-to-RT atomic updates (`param_input_gain`, `param_output_gain`, `param_gate_thresh`,
//!    `param_bypass`, `param_oversample`, `param_activation`), gesture bitmask updates,
//!    and generation counter increment.
//! 3. External parameter sync without echo/feedback loops.
//! 4. RT-to-UI telemetry polling, IEC 60268-10 Type I ballistics decay, peak hold,
//!    gate LED status, and clip indicator / reset callback.
//! 5. XDG Desktop Portal file dialog triggers and IR clear actions.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use nam_plug::clap::gui::slint_view_model::amp_to_meter_frac;
use nam_plug::clap::gui::{MainWindow, SlintViewModel};
use nam_plug::clap::plugin::GuiSharedState;
use nam_plug::clap::plugin::shared::{
    GESTURE_BEGIN_SHIFT, GESTURE_BITS_PER_PARAM, GESTURE_CHANGED_SHIFT, GESTURE_END_SHIFT,
};
use slint::ComponentHandle;

#[test]
fn test_slint_view_model_full_lifecycle() {
    let shared = GuiSharedState::new_test();

    // ── Phase 1: Initial Sync from GuiSharedState to Slint Properties ───
    shared
        .ui_to_rt
        .param_input_gain
        .store((-4.5f32).to_bits(), Ordering::Relaxed);
    shared
        .ui_to_rt
        .param_output_gain
        .store(2.0f32.to_bits(), Ordering::Relaxed);
    shared
        .ui_to_rt
        .param_gate_thresh
        .store((-35.0f32).to_bits(), Ordering::Relaxed);
    shared.ui_to_rt.param_bypass.store(1, Ordering::Relaxed);
    shared.ui_to_rt.param_oversample.store(2, Ordering::Relaxed); // 4x (Ultra)
    shared.ui_to_rt.param_activation.store(1, Ordering::Relaxed); // Standard

    let window = MainWindow::new().expect("Failed to create MainWindow");
    let mut vm = SlintViewModel::new(window.clone_strong(), Arc::clone(&shared), None);

    assert_eq!(window.get_input_gain_db(), -4.5);
    assert_eq!(window.get_input_gain_text(), "-4.5 dB");
    assert_eq!(window.get_output_gain_db(), 2.0);
    assert_eq!(window.get_output_gain_text(), "+2.0 dB");
    assert_eq!(window.get_gate_thresh_db(), -35.0);
    assert_eq!(window.get_gate_thresh_text(), "-35 dB");
    assert!(window.get_bypass_active());
    assert_eq!(window.get_oversample_mode(), 2);
    assert_eq!(window.get_oversample_text(), "4x (Ultra)");
    assert_eq!(window.get_activation_mode(), 1);

    // ── Phase 2: UI-to-RT Parameter Changes & Gestures ───────────────────
    let initial_gen = shared.ui_to_rt.gui_param_generation.load(Ordering::Acquire);

    // Input gain gesture & write
    window.invoke_input_gain_gesture_begin();
    let param_index = 0u32;
    let shift_in = param_index * GESTURE_BITS_PER_PARAM;
    let begin_mask_in = 1 << (shift_in + GESTURE_BEGIN_SHIFT);
    assert_ne!(
        shared.ui_to_rt.gesture_flags.load(Ordering::Relaxed) & begin_mask_in,
        0,
        "Input gain begin gesture bit must be set"
    );

    window.invoke_input_gain_changed(3.5);
    let val_in = f32::from_bits(shared.ui_to_rt.param_input_gain.load(Ordering::Relaxed));
    assert_eq!(val_in, 3.5);
    let changed_mask_in = 1 << (shift_in + GESTURE_CHANGED_SHIFT);
    assert_ne!(
        shared.ui_to_rt.gesture_flags.load(Ordering::Relaxed) & changed_mask_in,
        0,
        "Input gain changed bit must be set"
    );
    let current_gen = shared.ui_to_rt.gui_param_generation.load(Ordering::Acquire);
    assert!(
        current_gen > initial_gen,
        "Generation counter must increment on param write"
    );

    window.invoke_input_gain_gesture_end();
    let end_mask_in = 1 << (shift_in + GESTURE_END_SHIFT);
    assert_ne!(
        shared.ui_to_rt.gesture_flags.load(Ordering::Relaxed) & end_mask_in,
        0,
        "Input gain end gesture bit must be set"
    );

    // Output gain & gate threshold
    window.invoke_output_gain_changed(-6.0);
    let val_out = f32::from_bits(shared.ui_to_rt.param_output_gain.load(Ordering::Relaxed));
    assert_eq!(val_out, -6.0);

    window.invoke_gate_thresh_changed(-55.0);
    let val_gate = f32::from_bits(shared.ui_to_rt.param_gate_thresh.load(Ordering::Relaxed));
    assert_eq!(val_gate, -55.0);

    // Bypass toggle
    window.invoke_toggle_bypass(); // currently 1 -> toggles to 0
    assert_eq!(shared.ui_to_rt.param_bypass.load(Ordering::Relaxed), 0);
    assert!(!window.get_bypass_active());

    window.invoke_toggle_bypass(); // toggles back to 1
    assert_eq!(shared.ui_to_rt.param_bypass.load(Ordering::Relaxed), 1);
    assert!(window.get_bypass_active());

    // Oversampling cycling
    window.invoke_cycle_oversample(); // 2 -> 0 (1x Off)
    assert_eq!(shared.ui_to_rt.param_oversample.load(Ordering::Relaxed), 0);
    assert_eq!(window.get_oversample_mode(), 0);
    assert_eq!(window.get_oversample_text(), "1x (Off)");

    window.invoke_cycle_oversample(); // 0 -> 1 (2x HQ)
    assert_eq!(shared.ui_to_rt.param_oversample.load(Ordering::Relaxed), 1);
    assert_eq!(window.get_oversample_mode(), 1);
    assert_eq!(window.get_oversample_text(), "2x (HQ)");

    // Activation precision
    window.invoke_activation_changed(0); // Fast
    assert_eq!(shared.ui_to_rt.param_activation.load(Ordering::Relaxed), 0);

    // ── Phase 3: External Parameter Sync Without Echo ────────────────────
    shared.clear_gestures();

    // Simulate DAW host automation writing to shared UiToRt
    shared
        .ui_to_rt
        .param_input_gain
        .store(5.0f32.to_bits(), Ordering::Relaxed);
    shared
        .ui_to_rt
        .param_output_gain
        .store((-2.5f32).to_bits(), Ordering::Relaxed);
    shared
        .ui_to_rt
        .param_gate_thresh
        .store((-60.0f32).to_bits(), Ordering::Relaxed);
    shared.ui_to_rt.param_bypass.store(0, Ordering::Relaxed);
    shared.ui_to_rt.param_oversample.store(2, Ordering::Relaxed);
    shared.ui_to_rt.param_activation.store(1, Ordering::Relaxed);

    // Explicit telemetry tick
    vm.tick_telemetry();

    assert_eq!(window.get_input_gain_db(), 5.0);
    assert_eq!(window.get_input_gain_text(), "+5.0 dB");
    assert_eq!(window.get_output_gain_db(), -2.5);
    assert_eq!(window.get_output_gain_text(), "-2.5 dB");
    assert_eq!(window.get_gate_thresh_db(), -60.0);
    assert_eq!(window.get_gate_thresh_text(), "-60 dB");
    assert!(!window.get_bypass_active());
    assert_eq!(window.get_oversample_mode(), 2);
    assert_eq!(window.get_oversample_text(), "4x (Ultra)");
    assert_eq!(window.get_activation_mode(), 1);

    // Ensure NO gestures were triggered by external host sync (echo prevention)
    assert_eq!(
        shared.ui_to_rt.gesture_flags.load(Ordering::Relaxed),
        0,
        "External parameter sync must not generate gesture events (no feedback loop)"
    );

    // ── Phase 4: Telemetry Ballistics and Clip Reset ─────────────────────
    shared
        .rt_to_ui
        .ui_peak_l
        .store(1.0f32.to_bits(), Ordering::Relaxed); // 0 dB
    shared
        .rt_to_ui
        .ui_peak_r
        .store(0.5f32.to_bits(), Ordering::Relaxed); // ~ -6 dB
    shared
        .rt_to_ui
        .ui_clip_indicator
        .store(true, Ordering::Relaxed);
    shared
        .rt_to_ui
        .ui_gate_active
        .store(true, Ordering::Relaxed);

    vm.tick_telemetry();

    assert!(window.get_clip_active(), "Clip LED must be active");
    assert!(window.get_gate_active(), "Gate LED must be active");

    let frac_0db = amp_to_meter_frac(1.0);
    let frac_neg6db = amp_to_meter_frac(0.5);

    assert!(
        (window.get_peak_l() - frac_0db).abs() < 1e-3,
        "Left peak must match 0 dB fraction: got {}, expected {}",
        window.get_peak_l(),
        frac_0db
    );
    assert!(
        (window.get_peak_r() - frac_neg6db).abs() < 1e-3,
        "Right peak must match -6 dB fraction: got {}, expected {}",
        window.get_peak_r(),
        frac_neg6db
    );
    assert_eq!(window.get_peak_hold_l(), frac_0db);

    // Ballistics decay tick (simulate silence on RT thread)
    vm.tick_telemetry();

    assert!(
        window.get_peak_l() < frac_0db,
        "Meter needle must decay: got {}, peak was {}",
        window.get_peak_l(),
        frac_0db
    );
    assert_eq!(
        window.get_peak_hold_l(),
        frac_0db,
        "Peak hold must remain at maximum during hold window"
    );

    // Clip Reset Callback
    window.invoke_reset_clip_clicked();
    assert!(
        !window.get_clip_active(),
        "Clip flag on window must be cleared"
    );
    assert!(
        !shared.rt_to_ui.ui_clip_indicator.load(Ordering::Relaxed),
        "ui_clip_indicator atomic must be cleared"
    );

    // ── Phase 5: File Dialog Actions & Portal Interfacing ────────────────
    window.set_has_ir(true);
    window.set_ir_name("Test Cab IR.wav".into());
    assert!(window.get_has_ir());

    window.invoke_clear_ir_clicked();

    assert!(
        shared.cold.ui_clear_ir.load(Ordering::Relaxed),
        "ui_clear_ir must be set to true"
    );
    assert!(!window.get_has_ir(), "has_ir on window must be false");
    assert_eq!(window.get_ir_name(), "No IR loaded");

    // Select Model Clicked Trigger
    window.invoke_load_model_clicked();
    assert!(
        shared.cold.ui_loading.load(Ordering::Relaxed),
        "ui_loading must be true while picker dialog is open"
    );
    assert!(
        shared.is_model_dialog_active(),
        "dialog_state.active must be true"
    );

    // Clean up state
    shared.set_model_dialog_active(false);
    shared.cold.ui_loading.store(false, Ordering::Relaxed);
}
