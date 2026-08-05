// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::bypass::handle_bypass;
use super::knob::knob_widget;
use super::*;
use crate::clap::plugin::make_test_shared;
use std::sync::atomic::Ordering;

struct SafeClapHost(clap_sys::host::clap_host);
// SAFETY: SafeClapHost is a read-only static dummy host used exclusively in tests.
// The pointers are never mutated or dereferenced, making it safe to share across threads.
unsafe impl Sync for SafeClapHost {}

static DUMMY_CLAP_HOST: SafeClapHost = SafeClapHost(clap_sys::host::clap_host {
    clap_version: clap_sys::version::clap_version {
        major: 0,
        minor: 0,
        revision: 0,
    },
    host_data: std::ptr::null_mut(),
    name: std::ptr::null(),
    vendor: std::ptr::null(),
    url: std::ptr::null(),
    version: std::ptr::null(),
    get_extension: None,
    request_restart: None,
    request_process: None,
    request_callback: None,
});

/// Returns a zero-initialized `HostSharedHandle` for GUI tests that don't need
/// real host interaction. Calls to `get_extension` return `None` safely (all
/// function pointers are null).
fn make_dummy_host() -> HostSharedHandle<'static> {
    // SAFETY: DUMMY_CLAP_HOST is static and lives for 'static.
    unsafe { HostSharedHandle::from_raw(std::ptr::NonNull::from(&DUMMY_CLAP_HOST.0)) }
}

#[test]
fn test_track_color_conversion_and_fallback() {
    // 1. Fallback (alpha == 0)
    let shared_fallback = make_test_shared();
    shared_fallback
        .cold
        .track_accent_color
        .store(0, Ordering::Relaxed);
    let resolved = resolve_accent(&shared_fallback);
    assert_eq!(resolved, COL_ACCENT);

    // 2. White (0xFFFFFFFF)
    let shared_white = make_test_shared();
    shared_white
        .cold
        .track_accent_color
        .store(0xFFFFFFFF, Ordering::Relaxed);
    assert_eq!(
        resolve_accent(&shared_white),
        egui::Color32::from_rgb(255, 255, 255)
    );

    // 3. Black (0xFF000000)
    let shared_black = make_test_shared();
    shared_black
        .cold
        .track_accent_color
        .store(0xFF000000, Ordering::Relaxed);
    assert_eq!(
        resolve_accent(&shared_black),
        egui::Color32::from_rgb(0, 0, 0)
    );

    // 4. Pure Red (0xFFFF0000)
    let shared_red = make_test_shared();
    shared_red
        .cold
        .track_accent_color
        .store(0xFFFF0000, Ordering::Relaxed);
    assert_eq!(
        resolve_accent(&shared_red),
        egui::Color32::from_rgb(255, 0, 0)
    );

    // 5. Pure Green (0xFF00FF00)
    let shared_green = make_test_shared();
    shared_green
        .cold
        .track_accent_color
        .store(0xFF00FF00, Ordering::Relaxed);
    assert_eq!(
        resolve_accent(&shared_green),
        egui::Color32::from_rgb(0, 255, 0)
    );

    // 6. Bitwig Blue #5e81ac (0xFF5E81AC)
    let shared_bitwig = make_test_shared();
    shared_bitwig
        .cold
        .track_accent_color
        .store(0xFF5E81AC, Ordering::Relaxed);
    assert_eq!(
        resolve_accent(&shared_bitwig),
        egui::Color32::from_rgb(0x5E, 0x81, 0xAC)
    );
}

#[test]
fn test_resolve_color() {
    let fallback = egui::Color32::BLUE;

    // 1. Fallback (alpha == 0)
    assert_eq!(resolve_color(0, fallback), fallback);

    // 2. Parsed color (alpha != 0)
    let packed = crate::clap::extensions::track_info::pack_argb(0xFF, 0xFF, 0x00, 0x00); // Red
    assert_eq!(
        resolve_color(packed, fallback),
        egui::Color32::from_rgb(255, 0, 0)
    );
}

#[test]
fn test_ui_load_error_visual_feedback() {
    use std::time::{Duration, Instant};

    let shared = make_test_shared();
    shared.cold.track_accent_color.store(0, Ordering::Relaxed);
    shared.cold.ui_load_error.store(true, Ordering::Relaxed);
    *shared.cold.ui_load_error_msg.lock().unwrap() = "Invalid JSON format".to_string();

    let mut state = UiState::default();
    assert!(state.error_expiration.is_none());
    assert!(state.error_msg.is_empty());

    let ctx = egui::Context::default();
    let host = make_dummy_host();

    let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
        egui::CentralPanel::default().show_inside(ui, |ui| {
            draw_ui(ui, &shared, &host, &mut state);
        });
    });

    // 1. After draw_ui, state.error_expiration should be set.
    assert!(state.error_expiration.is_some());
    assert_eq!(state.error_msg, "Invalid JSON format");

    // 2. The flag ui_load_error should have been swapped to false.
    assert!(!shared.cold.ui_load_error.load(Ordering::Relaxed));

    // 3. If we set error_expiration to the past, the next draw_ui should reset/clear it.
    state.error_expiration = Some(Instant::now() - Duration::from_secs(1));
    let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
        egui::CentralPanel::default().show_inside(ui, |ui| {
            draw_ui(ui, &shared, &host, &mut state);
        });
    });

    assert!(state.error_expiration.is_none());
}

#[test]
fn test_knob_tooltip_suffixes() {
    let ctx = egui::Context::default();
    let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
        egui::CentralPanel::default().show_inside(ui, |ui| {
            let value = 0.0;
            let range = -10.0..=10.0;
            let size = egui::vec2(50.0, 50.0);
            let color = egui::Color32::RED;
            let indication = 0;

            // Render with " dB" suffix
            let (_response_db, new_val_db) = knob_widget(
                ui,
                ui.make_persistent_id("test_knob_db"),
                value,
                range.clone(),
                size,
                color,
                color,
                indication,
                egui::Color32::from_rgb(94, 129, 172),
                " dB",
            );
            assert_eq!(new_val_db, value);

            // Render with " dB (Threshold)" suffix
            let (_response_threshold, new_val_threshold) = knob_widget(
                ui,
                ui.make_persistent_id("test_knob_threshold"),
                value,
                range,
                size,
                color,
                color,
                indication,
                egui::Color32::from_rgb(94, 129, 172),
                " dB (Threshold)",
            );
            assert_eq!(new_val_threshold, value);
        });
    });
}

fn relative_luminance(color: egui::Color32) -> f32 {
    let r = color.r() as f32 / 255.0;
    let g = color.g() as f32 / 255.0;
    let b = color.b() as f32 / 255.0;

    let r_lin = if r <= 0.03928 {
        r / 12.92
    } else {
        ((r + 0.055) / 1.055).powf(2.4)
    };
    let g_lin = if g <= 0.03928 {
        g / 12.92
    } else {
        ((g + 0.055) / 1.055).powf(2.4)
    };
    let b_lin = if b <= 0.03928 {
        b / 12.92
    } else {
        ((b + 0.055) / 1.055).powf(2.4)
    };

    0.2126 * r_lin + 0.7152 * g_lin + 0.0722 * b_lin
}

fn contrast_ratio(c1: egui::Color32, c2: egui::Color32) -> f32 {
    let l1 = relative_luminance(c1);
    let l2 = relative_luminance(c2);
    if l1 > l2 {
        (l1 + 0.05) / (l2 + 0.05)
    } else {
        (l2 + 0.05) / (l1 + 0.05)
    }
}

#[test]
fn test_contrast_ratios() {
    let ratio_muted_panel = contrast_ratio(COL_MUTED, COL_PANEL);
    let ratio_vured_bg = contrast_ratio(COL_VU_RED, COL_BG);
    let ratio_muted_bg = contrast_ratio(COL_MUTED, COL_BG);

    // Muted on Panel should be >= 4.5 (typically ~4.88)
    assert!(
        ratio_muted_panel >= 4.5,
        "Muted on Panel: {}",
        ratio_muted_panel
    );
    // VU Red on BG should be >= 4.5 (typically ~4.99)
    assert!(ratio_vured_bg >= 4.5, "VU Red on BG: {}", ratio_vured_bg);
    // Muted on BG (Bypassed status text) should be >= 4.5 (typically ~5.63)
    assert!(ratio_muted_bg >= 4.5, "Muted on BG: {}", ratio_muted_bg);
}

#[test]
fn test_knob_keyboard_navigation() {
    let ctx = egui::Context::default();
    let id = egui::Id::new("test_knob");
    let mut state = 0.0f32;
    let range = -10.0..=10.0;
    let size = egui::vec2(50.0, 50.0);
    let color = egui::Color32::RED;
    let accent_color = egui::Color32::GREEN;

    // Frame 1: Render and request focus
    let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
        egui::CentralPanel::default().show_inside(ui, |ui| {
            let (_, val) = knob_widget(
                ui,
                id,
                state,
                range.clone(),
                size,
                color,
                accent_color,
                0,
                egui::Color32::from_rgb(94, 129, 172),
                " dB",
            );
            state = val;
            ui.memory_mut(|mem| mem.request_focus(id));
        });
    });

    // Frame 2: ArrowUp should increment value by 1.0
    let mut input_up = egui::RawInput::default();
    input_up.events.push(egui::Event::Key {
        key: egui::Key::ArrowUp,
        physical_key: None,
        pressed: true,
        modifiers: egui::Modifiers::default(),
        repeat: false,
    });
    let _ = ctx.run_ui(input_up, |ui| {
        egui::CentralPanel::default().show_inside(ui, |ui| {
            let (_, val) = knob_widget(
                ui,
                id,
                state,
                range.clone(),
                size,
                color,
                accent_color,
                0,
                egui::Color32::from_rgb(94, 129, 172),
                " dB",
            );
            state = val;
        });
    });
    assert_eq!(state, 1.0);

    // Frame 3: Ctrl + ArrowDown should decrement value by 0.1
    let mut input_down_ctrl = egui::RawInput::default();
    input_down_ctrl.modifiers.ctrl = true;
    input_down_ctrl.events.push(egui::Event::Key {
        key: egui::Key::ArrowDown,
        physical_key: None,
        pressed: true,
        modifiers: egui::Modifiers {
            ctrl: true,
            ..Default::default()
        },
        repeat: false,
    });
    let _ = ctx.run_ui(input_down_ctrl, |ui| {
        egui::CentralPanel::default().show_inside(ui, |ui| {
            let (_, val) = knob_widget(
                ui,
                id,
                state,
                range.clone(),
                size,
                color,
                accent_color,
                0,
                egui::Color32::from_rgb(94, 129, 172),
                " dB",
            );
            state = val;
        });
    });
    assert!((state - 0.9).abs() < 1e-5, "Expected 0.9, got {}", state);
}

#[test]
fn test_bypass_keyboard_trigger() {
    let ctx = egui::Context::default();
    let id = egui::Id::new("test_bypass");
    let atomic_val = std::sync::atomic::AtomicU32::new(0); // initial: bypass off
    let gesture_flags = std::sync::atomic::AtomicU32::new(0);
    let gui_param_generation = std::sync::atomic::AtomicU32::new(0);
    let host = make_dummy_host();

    const BYPASS_INDEX: usize = 3; // PARAM_BYPASS = 3
    const BITS_PER_PARAM: u32 = 3;
    const CHANGED_SHIFT: u32 = 0;
    const BEGIN_SHIFT: u32 = 1;
    const END_SHIFT: u32 = 2;
    let offset = BYPASS_INDEX as u32 * BITS_PER_PARAM;

    // Frame 1: Render and request focus
    let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
        egui::CentralPanel::default().show_inside(ui, |ui| {
            handle_bypass(
                ui,
                id,
                &atomic_val,
                &gesture_flags,
                &gui_param_generation,
                BYPASS_INDEX,
                egui::Color32::GREEN,
                &host,
                0,
                egui::Color32::from_rgb(94, 129, 172),
            );
            ui.memory_mut(|mem| mem.request_focus(id));
        });
    });

    // Frame 2: Space key event to toggle bypass
    let mut input_space = egui::RawInput::default();
    input_space.events.push(egui::Event::Key {
        key: egui::Key::Space,
        physical_key: None,
        pressed: true,
        modifiers: egui::Modifiers::default(),
        repeat: false,
    });
    let _ = ctx.run_ui(input_space, |ui| {
        egui::CentralPanel::default().show_inside(ui, |ui| {
            handle_bypass(
                ui,
                id,
                &atomic_val,
                &gesture_flags,
                &gui_param_generation,
                BYPASS_INDEX,
                egui::Color32::GREEN,
                &host,
                0,
                egui::Color32::from_rgb(94, 129, 172),
            );
        });
    });
    assert_eq!(atomic_val.load(Ordering::Relaxed), 1); // Should be Bypassed (1)

    let flags = gesture_flags.load(Ordering::Relaxed);
    assert!(flags & (1 << (offset + CHANGED_SHIFT)) != 0);
    assert!(flags & (1 << (offset + BEGIN_SHIFT)) != 0);
    assert!(flags & (1 << (offset + END_SHIFT)) != 0);
}

#[test]
fn test_tab_order_navigation() {
    let ctx = egui::Context::default();
    let shared = make_test_shared();
    shared.cold.track_accent_color.store(0, Ordering::Relaxed);
    let mut state = UiState::default();
    let host = make_dummy_host();

    // Frame 1: Initial render, no focus
    let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
        egui::CentralPanel::default().show_inside(ui, |ui| {
            draw_ui(ui, &shared, &host, &mut state);
        });
    });
    assert!(ctx.memory(|mem| mem.focused()).is_none());

    // Frame 2: Send Tab key -> should focus INPUT knob (controls[0])
    let mut tab_input = egui::RawInput::default();
    tab_input.events.push(egui::Event::Key {
        key: egui::Key::Tab,
        physical_key: None,
        pressed: true,
        modifiers: egui::Modifiers::default(),
        repeat: false,
    });
    let _ = ctx.run_ui(tab_input, |ui| {
        egui::CentralPanel::default().show_inside(ui, |ui| {
            draw_ui(ui, &shared, &host, &mut state);
        });
    });
    let focused_1 = ctx.memory(|mem| mem.focused());
    assert!(focused_1.is_some());

    // Frame 3: Send Tab again -> should focus next widget (controls[1])
    let mut tab_input2 = egui::RawInput::default();
    tab_input2.events.push(egui::Event::Key {
        key: egui::Key::Tab,
        physical_key: None,
        pressed: true,
        modifiers: egui::Modifiers::default(),
        repeat: false,
    });
    let _ = ctx.run_ui(tab_input2, |ui| {
        egui::CentralPanel::default().show_inside(ui, |ui| {
            draw_ui(ui, &shared, &host, &mut state);
        });
    });
    let focused_2 = ctx.memory(|mem| mem.focused());
    assert!(focused_2.is_some());
    assert_ne!(focused_1, focused_2);
}
