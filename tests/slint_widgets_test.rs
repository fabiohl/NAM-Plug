// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! # Integration Tests for Slint Audio Widgets (Sprint 2)
//!
//! Validates `MainWindow` instantiation, initial properties, and interactive callbacks
//! for `RotaryKnob`, `VuMeter`, `ToggleSwitch`, `LedIndicator`, and `SelectorButton`.

slint::include_modules!();

#[test]
fn test_slint_widgets_and_five_zones_lifecycle() {
    let window =
        MainWindow::new().expect("Failed to initialize MainWindow with Slint audio widgets");

    // ── Phase 1: Verify Initial Default Properties ───────────────────
    assert_eq!(window.get_input_gain_db(), 0.0);
    assert_eq!(window.get_output_gain_db(), -3.0);
    assert_eq!(window.get_gate_thresh_db(), -90.0);
    assert!(!window.get_bypass_active());
    assert!(!window.get_clip_active());
    assert!(window.get_is_stereo());

    assert_eq!(window.get_peak_l(), 0.72);
    assert_eq!(window.get_peak_r(), 0.68);
    assert_eq!(window.get_peak_hold_l(), 0.78);
    assert_eq!(window.get_peak_hold_r(), 0.74);

    assert_eq!(window.get_model_name(), "No model loaded");
    assert_eq!(window.get_model_arch(), "No model loaded");
    assert_eq!(window.get_model_sample_rate(), "—");
    assert!(!window.get_has_model());
    assert!(!window.get_model_loading());
    assert!(!window.get_model_error());

    assert_eq!(window.get_ir_name(), "No IR loaded");
    assert_eq!(window.get_ir_status(), "—");
    assert!(!window.get_has_ir());
    assert!(!window.get_ir_loading());
    assert!(!window.get_ir_error());

    assert_eq!(window.get_oversample_mode(), 0);
    assert_eq!(window.get_activation_mode(), 1);
    assert_eq!(window.get_sample_rate_text(), "48000 Hz");
    assert_eq!(window.get_buffer_size_text(), "— spl");
    assert_eq!(window.get_channels_text(), "Stereo");
    assert_eq!(window.get_latency_text(), "0 spl (0.0 ms)");
    // Finding F6: the design-time default is an honest "no data yet"
    // placeholder, not a hard-coded fake percentage — `SlintViewModel`
    // never overwrites this property (no real per-block metric exists yet).
    assert_eq!(window.get_dsp_load_text(), "—");
    assert_eq!(window.get_simd_badge(), "AVX2+FMA");
    assert_eq!(window.get_backend_text(), "Wayland Native");

    // ── Phase 2: Mutate Controls and Verify Reactivity ───────────────
    window.set_input_gain_db(5.5);
    assert_eq!(window.get_input_gain_db(), 5.5);

    window.set_output_gain_db(-6.0);
    assert_eq!(window.get_output_gain_db(), -6.0);

    window.set_gate_thresh_db(-45.0);
    assert_eq!(window.get_gate_thresh_db(), -45.0);

    window.set_peak_l(0.95);
    assert_eq!(window.get_peak_l(), 0.95);
    window.set_clip_active(true);
    assert!(window.get_clip_active());

    window.set_bypass_active(true);
    assert!(window.get_bypass_active());
    window.set_bypass_active(false);
    assert!(!window.get_bypass_active());

    window.set_oversample_mode(1);
    assert_eq!(window.get_oversample_mode(), 1);
    window.set_oversample_mode(2);
    assert_eq!(window.get_oversample_mode(), 2);

    window.set_activation_mode(1);
    assert_eq!(window.get_activation_mode(), 1);

    window.set_is_stereo(false);
    assert!(!window.get_is_stereo());
    window.set_is_stereo(true);
    assert!(window.get_is_stereo());

    // ── Phase 3: Ultra-Long String Stress Test (Graceful Truncation) ───
    let ultra_long_model = "1965_Fender_Super_Reverb_Blackface_Vibrato_Channel_Bright_On_6L6_Matched_Pair_Ultra_High_Gain_Lead_Profile_v2_Final_Author_John_Doe_Recorded_At_Sound_Studios_Nashville_Tennessee_With_Vintage_Neve_1073_Preamp_High_Resolution_Capture.nam";
    window.set_model_name(ultra_long_model.into());
    assert_eq!(window.get_model_name(), ultra_long_model);

    let ultra_long_ir = "Celestion_Vintage_30_Closed_Back_4x12_Cab_Shure_SM57_CapEdge_Royer_R121_Cone_Distance_4in_Phase_Aligned_48k_24b_Captured_With_Millennia_HV3D_Preamps_Reference_IR_Archive_Collection_2026.wav";
    window.set_ir_name(ultra_long_ir.into());
    assert_eq!(window.get_ir_name(), ultra_long_ir);

    // ── Phase 4: Loading & Error States ──────────────────────────────
    window.set_model_loading(true);
    assert!(window.get_model_loading());
    window.set_model_loading(false);

    window.set_model_error(true);
    window.set_model_error_msg("Invalid NAM header".into());
    assert!(window.get_model_error());
    assert_eq!(window.get_model_error_msg(), "Invalid NAM header");
    window.set_model_error(false);

    window.set_ir_loading(true);
    assert!(window.get_ir_loading());
    window.set_ir_loading(false);

    window.set_ir_error(true);
    window.set_ir_error_msg("WAV corrupt".into());
    assert!(window.get_ir_error());
    assert_eq!(window.get_ir_error_msg(), "WAV corrupt");
    window.set_ir_error(false);

    // ── Phase 5: Test Callback Invocations ───────────────────────────
    let bypass_called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let bypass_clone = bypass_called.clone();
    window.on_toggle_bypass(move || {
        bypass_clone.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    window.invoke_toggle_bypass();
    assert!(bypass_called.load(std::sync::atomic::Ordering::SeqCst));

    let clip_cleared = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let clip_clone = clip_cleared.clone();
    window.on_reset_clip_clicked(move || {
        clip_clone.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    window.invoke_reset_clip_clicked();
    assert!(clip_cleared.load(std::sync::atomic::Ordering::SeqCst));

    let gain_received = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let gain_clone = gain_received.clone();
    window.on_input_gain_changed(move |v| {
        gain_clone.store(v.to_bits(), std::sync::atomic::Ordering::SeqCst);
    });
    window.invoke_input_gain_changed(3.5);
    let received_f32 = f32::from_bits(gain_received.load(std::sync::atomic::Ordering::SeqCst));
    assert!((received_f32 - 3.5).abs() < 1e-6);

    let model_load_triggered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let model_load_clone = model_load_triggered.clone();
    window.on_load_model_clicked(move || {
        model_load_clone.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    window.invoke_load_model_clicked();
    assert!(model_load_triggered.load(std::sync::atomic::Ordering::SeqCst));

    let ir_load_triggered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ir_load_clone = ir_load_triggered.clone();
    window.on_load_ir_clicked(move || {
        ir_load_clone.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    window.invoke_load_ir_clicked();
    assert!(ir_load_triggered.load(std::sync::atomic::Ordering::SeqCst));

    let model_clear_triggered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let model_clear_clone = model_clear_triggered.clone();
    window.on_clear_model_clicked(move || {
        model_clear_clone.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    window.invoke_clear_model_clicked();
    assert!(model_clear_triggered.load(std::sync::atomic::Ordering::SeqCst));

    let ir_clear_triggered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ir_clear_clone = ir_clear_triggered.clone();
    window.on_clear_ir_clicked(move || {
        ir_clear_clone.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    window.invoke_clear_ir_clicked();
    assert!(ir_clear_triggered.load(std::sync::atomic::Ordering::SeqCst));

    let os_mode_received = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(-1));
    let os_mode_clone = os_mode_received.clone();
    window.on_oversample_changed(move |mode| {
        os_mode_clone.store(mode, std::sync::atomic::Ordering::SeqCst);
    });
    window.invoke_oversample_changed(2);
    assert_eq!(
        os_mode_received.load(std::sync::atomic::Ordering::SeqCst),
        2
    );

    let act_mode_received = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(-1));
    let act_mode_clone = act_mode_received.clone();
    window.on_activation_changed(move |mode| {
        act_mode_clone.store(mode, std::sync::atomic::Ordering::SeqCst);
    });
    window.invoke_activation_changed(1);
    assert_eq!(
        act_mode_received.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}
