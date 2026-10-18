// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! # Integration Tests for Slint View-Model
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

use nam_plug::clap::gui::slint_view_model::{StatusBarCache, StatusTextSink, amp_to_meter_frac};
use nam_plug::clap::gui::{MainWindow, SlintViewModel};
use nam_plug::clap::plugin::shared::{
    GESTURE_BEGIN_SHIFT, GESTURE_BITS_PER_PARAM, GESTURE_CHANGED_SHIFT, GESTURE_END_SHIFT,
};
use nam_plug::clap::plugin::{GuiSharedState, NamModelMetadata};
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
    let mut vm = SlintViewModel::new(window.clone_strong(), Arc::clone(&shared), None, "Floating");

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

    // Static-for-the-window badges (Finding F6): set once at construction,
    // never left at the design-time placeholder values.
    assert_eq!(window.get_backend_text(), "Floating");
    assert!(
        !window.get_simd_badge().is_empty(),
        "simd_badge must reflect the runtime-detected instruction set"
    );
    assert_eq!(
        window.get_model_arch(),
        "No model loaded",
        "model_arch must not show the design-time placeholder before any model loads"
    );
    assert_eq!(window.get_buffer_size_text(), "— spl");
    assert_eq!(window.get_channels_text(), "Mono");
    assert_eq!(window.get_sample_rate_text(), "44.1 kHz");

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

    // Active channel count update (Mono -> Stereo transition)
    shared
        .rt_to_ui
        .active_channel_count
        .store(2, Ordering::Relaxed);
    vm.tick_telemetry();
    assert_eq!(window.get_channels_text(), "Stereo");

    // ── Phase 4b: DSP Load & XRun Telemetry (T-R2.2) ─────────────────────
    assert_eq!(window.get_dsp_load_text(), "0% DSP");
    assert!(!window.get_xrun_active());

    // 1. Changing permille within the same integer percentage (e.g. 5‰ = 0.5% -> int 0%)
    shared
        .rt_to_ui
        .dsp_load_permille
        .store(5, Ordering::Relaxed);
    vm.tick_telemetry();
    assert_eq!(window.get_dsp_load_text(), "0% DSP");

    // Changing permille across integer boundary (12‰ = 1.2% -> 1%)
    shared
        .rt_to_ui
        .dsp_load_permille
        .store(12, Ordering::Relaxed);
    vm.tick_telemetry();
    assert_eq!(window.get_dsp_load_text(), "1% DSP");

    // Changing within 1% (18‰ = 1.8% -> 1%)
    shared
        .rt_to_ui
        .dsp_load_permille
        .store(18, Ordering::Relaxed);
    vm.tick_telemetry();
    assert_eq!(window.get_dsp_load_text(), "1% DSP");

    // Changing to 25% (250‰)
    shared
        .rt_to_ui
        .dsp_load_permille
        .store(250, Ordering::Relaxed);
    vm.tick_telemetry();
    assert_eq!(window.get_dsp_load_text(), "25% DSP");

    // 2. Increment in xrun_epoch triggers xrun_active for exactly 120 frames (~2 s @ 60 Hz)
    assert!(!window.get_xrun_active());
    shared.rt_to_ui.xrun_epoch.store(1, Ordering::Relaxed);

    // Frame 1
    vm.tick_telemetry();
    assert!(
        window.get_xrun_active(),
        "xrun_active must be true on first frame after xrun_epoch increment"
    );

    // Frames 2..=120 (total 120 frames active)
    for frame in 2..=120 {
        vm.tick_telemetry();
        assert!(
            window.get_xrun_active(),
            "xrun_active must stay true at frame {frame} of 120"
        );
    }

    // Frame 121: hold expires, xrun_active returns to false
    vm.tick_telemetry();
    assert!(
        !window.get_xrun_active(),
        "xrun_active must return to false after exactly 120 frames (2.0s hold)"
    );

    // ── Phase 5: File Dialog Actions & Portal Interfacing ────────────────
    window.set_has_model(true);
    window.set_model_name("Test Model.nam".into());
    assert!(window.get_has_model());

    window.invoke_clear_model_clicked();

    assert!(
        shared.cold.ui_clear_model.load(Ordering::Relaxed),
        "ui_clear_model must be set to true"
    );
    assert!(!window.get_has_model(), "has_model on window must be false");
    assert_eq!(window.get_model_name(), "No model loaded");

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

    // Select Model Clicked Trigger (F-UX-06): the dialog is open, but no load
    // has started — `ui_dialog_open` and `ui_loading` are separate states and
    // the spinner only follows the latter.
    window.invoke_load_model_clicked();
    assert!(
        shared.cold.ui_dialog_open.load(Ordering::Relaxed),
        "ui_dialog_open must be true while picker dialog is open"
    );
    assert!(
        !shared.cold.ui_loading.load(Ordering::Relaxed),
        "ui_loading must stay false while the dialog is open (spinner is for real loads)"
    );
    assert!(
        shared.is_model_dialog_active(),
        "dialog_state.active must be true"
    );

    // Spinner transitions (F-UX-06): `dialog_open → loading → idle`.
    vm.tick_telemetry();
    assert!(
        !window.get_model_loading(),
        "no spinner while the dialog is open"
    );
    shared.set_model_dialog_active(false);
    shared.cold.ui_dialog_open.store(false, Ordering::Relaxed);
    shared.cold.ui_loading.store(true, Ordering::Relaxed);
    vm.tick_telemetry();
    assert!(
        window.get_model_loading(),
        "the spinner follows ui_loading during the load"
    );
    shared.cold.ui_loading.store(false, Ordering::Relaxed);
    vm.tick_telemetry();
    assert!(!window.get_model_loading());

    // Clean up state
    shared.set_model_dialog_active(false);
    shared.cold.ui_dialog_open.store(false, Ordering::Relaxed);
    shared.cold.ui_loading.store(false, Ordering::Relaxed);

    // ── Phase 6: Model Architecture Badge Live Update (Finding F6) ───────
    // Mirrors the `ui_model_name` diff pattern: loading a model must update
    // `model_arch` on the next tick, and clearing it must revert to the
    // "No model loaded" placeholder — never a stale architecture string.
    if let Ok(mut guard) = shared.cold.ui_model_metadata.lock() {
        *guard = Some(NamModelMetadata {
            architecture: "WaveNet".to_string(),
            topology: "1x16".to_string(),
            ..Default::default()
        });
    }
    vm.tick_telemetry();
    assert_eq!(window.get_model_arch(), "WaveNet (1x16)");

    if let Ok(mut guard) = shared.cold.ui_model_metadata.lock() {
        *guard = None;
    }
    vm.tick_telemetry();
    assert_eq!(window.get_model_arch(), "No model loaded");
}

// ═══════════════════════════════════════════════════════════════════════════
// T2.1 — Status-bar string cache (F-UX-08)
// ═══════════════════════════════════════════════════════════════════════════

/// Test wrapper around `StatusTextSink` (F-UX-08): records every `set_*_text`
/// publication so the cache can be held to "no publication without a change".
#[derive(Default)]
struct CountingStatusSink {
    calls: std::cell::RefCell<Vec<(&'static str, String)>>,
}

impl CountingStatusSink {
    fn take(&self) -> Vec<(&'static str, String)> {
        self.calls.borrow_mut().drain(..).collect()
    }
}

impl StatusTextSink for CountingStatusSink {
    fn publish_sample_rate_text(&self, text: &str) {
        self.calls
            .borrow_mut()
            .push(("sample_rate", text.to_string()));
    }
    fn publish_buffer_size_text(&self, text: &str) {
        self.calls
            .borrow_mut()
            .push(("buffer_size", text.to_string()));
    }
    fn publish_latency_text(&self, text: &str) {
        self.calls.borrow_mut().push(("latency", text.to_string()));
    }
    fn publish_channels_text(&self, text: &str) {
        self.calls.borrow_mut().push(("channels", text.to_string()));
    }
}

/// T2.1: with unchanged inputs the cache must publish nothing (zero `format!`,
/// zero `set_*_text`); on change it must publish exactly the legacy formats.
#[test]
fn test_status_bar_cache_publishes_only_on_change() {
    let sink = CountingStatusSink::default();
    // Mirrors a window already showing the values published at construction.
    let mut cache = StatusBarCache::new(44100, 64, 128, 2);

    // No change → no publication.
    cache.update(44100, 64, 128, 2, &sink);
    assert!(
        sink.take().is_empty(),
        "no set_*_text may be invoked without a value change"
    );

    // Buffer-size change only — formats unchanged from the legacy strings.
    cache.update(44100, 128, 128, 2, &sink);
    assert_eq!(sink.take(), vec![("buffer_size", "128 spl".to_string())]);

    // Sample-rate change also republishes the latency text (ms depends on sr).
    cache.update(48000, 128, 128, 2, &sink);
    assert_eq!(
        sink.take(),
        vec![
            ("sample_rate", "48 kHz".to_string()),
            ("latency", "128 spl (2.7 ms)".to_string()),
        ]
    );

    // Latency-only change.
    cache.update(48000, 128, 0, 2, &sink);
    assert_eq!(sink.take(), vec![("latency", "0 spl (0.0 ms)".to_string())]);

    // Channel-count change.
    cache.update(48000, 128, 0, 1, &sink);
    assert_eq!(sink.take(), vec![("channels", "Mono".to_string())]);

    // Still no change → nothing.
    cache.update(48000, 128, 0, 1, &sink);
    assert!(sink.take().is_empty());

    // Full-format sweep from an empty cache (locks the legacy formats).
    let mut cache = StatusBarCache::new(0, 0, 0, 0);
    cache.update(44100, 64, 128, 3, &sink);
    assert_eq!(
        sink.take(),
        vec![
            ("sample_rate", "44.1 kHz".to_string()),
            ("buffer_size", "64 spl".to_string()),
            ("latency", "128 spl (2.9 ms)".to_string()),
            ("channels", "Stereo".to_string()),
        ]
    );
    cache.update(48001, 0, 64, 0, &sink);
    assert_eq!(
        sink.take(),
        vec![
            ("sample_rate", "48.0 kHz".to_string()),
            ("buffer_size", "— spl".to_string()),
            ("latency", "64 spl (1.3 ms)".to_string()),
            ("channels", "Stereo".to_string()),
        ]
    );
}
