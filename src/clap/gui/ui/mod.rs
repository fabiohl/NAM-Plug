// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
//! Graphical interface of the NAM-rs plugin rendered via `egui` + `egui_glow`.
//!
//! This module implements the entire GUI rendering of the CLAP plugin,
//! organized into 5 visual zones:
//!
//! - **Zone 1** (left): Identity — logo, version, SIMD badge, model load button
//! - **Zone 2** (center): Controls — Input Gain, Output Gain and Gate Threshold knobs
//! - **Zone 3** (right): Adaptive VU meter(s) — mono (1 centered bar) or stereo (L/R bars) based on host track config
//! - **Zone 4** (far right): Bypass toggle with status LED
//! - **Zone 5** (footer): Status bar with sample rate, latency, DSP load and telemetry
//!
//! Communication with the plugin state is done entirely via atomics
//! in `NamClapShared`, with no locks in the render path.

mod bypass;
pub mod colors;
mod focus;
mod knob;
mod meter;
mod simd;
pub mod state;
mod status_bar;
mod vsep;
pub(crate) mod zones;

pub use colors::{
    COL_ACCENT, COL_AMBER, COL_BG, COL_BORDER, COL_BYPASS_OFF, COL_MUTED, COL_PANEL, COL_TEXT,
    COL_VU_GREEN, COL_VU_RED, COL_VU_YELLOW,
};
pub use colors::{resolve_accent, resolve_color};
pub use state::{UiState, VuMeterSharedState, VuUniforms};

use self::focus::handle_focus_navigation;
use self::status_bar::draw_zone5_status_bar;
use self::vsep::styled_vsep;
use self::zones::{draw_zone1_identity, draw_zone2_controls, draw_zone3_meters, draw_zone4_bypass};
use crate::clap::extensions::params::bypass_u32_to_bool;
use crate::clap::plugin::NamClapShared;
use clack_plugin::host::HostSharedHandle;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// Draws the components and 5-zone layout of the NAM-rs graphical interface.
///
/// This is the main GUI rendering function, called every frame by
/// `NamPluginWindow::on_frame()`. Reads shared state via atomics and
/// writes parameter changes back to atomics + requests host flush.
///
/// # Layout
///
/// ```text
/// ┌─────────┬──────────────────┬──────────┬────────┐
/// │  Zone 1 │    Zone 2        │ Zone 3   │ Zone 4 │
/// │  Logo   │  Input  Out Gate │  VU mono │ Bypass │
/// │  Model  │  Knobs           │  or L/R  │ Switch │
/// ├─────────┴──────────────────┴──────────┴────────┤
/// │               Zone 5: Status Bar               │
/// └────────────────────────────────────────────────┘
/// ```
pub fn draw_ui(
    ui: &mut egui::Ui,
    shared: &NamClapShared,
    host: &HostSharedHandle,
    state: &mut UiState,
) {
    if shared.cold.ui_load_error.swap(false, Ordering::Relaxed) {
        state.error_expiration = Some(Instant::now() + Duration::from_secs(3));
        if let Ok(msg_guard) = shared.cold.ui_load_error_msg.lock() {
            state.error_msg = msg_guard.clone();
        } else {
            state.error_msg = "Load failed".to_string();
        }
    }

    if shared.cold.ui_ir_load_error.swap(false, Ordering::Relaxed) {
        state.ir_error_expiration = Some(Instant::now() + Duration::from_secs(3));
        if let Ok(msg_guard) = shared.cold.ui_ir_load_error_msg.lock() {
            state.ir_error_msg = msg_guard.clone();
        } else {
            state.ir_error_msg = "IR load failed".to_string();
        }
    }

    let current_bypass = bypass_u32_to_bool(shared.ui_to_rt.param_bypass.load(Ordering::Relaxed));
    let accent_color = resolve_accent(shared);
    let mut load_btn_id = None;
    let mut load_ir_btn_id = None;
    let mut clear_ir_btn_id = None;
    let mut oversample_id = None;
    let mut activation_id = None;
    ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);

    let available_w = ui.available_width();

    // Main layout: horizontal with Zones 1-4 centered
    ui.horizontal(|ui| {
        let total_content_width =
            135.0 + 240.0 + 80.0 + 60.0 + 3.0 * 13.0 + 6.0 * ui.spacing().item_spacing.x;
        let extra_width = (available_w - total_content_width).max(0.0);
        let left_space = (extra_width / 2.0) - ui.spacing().item_spacing.x;
        if left_space > 0.0 {
            ui.add_space(left_space);
        }

        // ── Zone 1: Identity (left) ───────────────────────────
        let (model_btn, ir_btn, clear_ir_btn) =
            draw_zone1_identity(ui, shared, host, state, accent_color);
        load_btn_id = model_btn;
        load_ir_btn_id = ir_btn;
        clear_ir_btn_id = clear_ir_btn;

        styled_vsep(ui);

        // ── Zone 2: Controls (center) ────────────────────
        let (os_id, act_id) = draw_zone2_controls(ui, shared, host, current_bypass, accent_color);
        oversample_id = os_id;
        activation_id = act_id;

        styled_vsep(ui);

        // ── Zone 3: VU Meters (right) ─────────────────────
        draw_zone3_meters(ui, shared, state, current_bypass);

        styled_vsep(ui);

        // ── Zone 4: Bypass (far right) ────────────────────
        draw_zone4_bypass(ui, shared, host, accent_color);
    });

    // ── Zone 5: Status Bar / Footer ──────────────────────────────────────────
    draw_zone5_status_bar(ui, shared, state, accent_color);

    if state.drag_active {
        egui::Area::new(egui::Id::new("drop_overlay"))
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .order(egui::Order::Foreground)
            .interactable(false)
            .show(ui.ctx(), |ui| {
                let screen_rect = ui.ctx().content_rect();
                let painter = ui.painter();
                painter.rect_filled(
                    screen_rect,
                    0.0,
                    egui::Color32::from_rgba_unmultiplied(26, 29, 35, 217),
                );
                ui.vertical_centered(|ui| {
                    ui.add_space(screen_rect.height() / 2.0 - 20.0);
                    ui.label(
                        egui::RichText::new("Drop NAM Model Here ⬇️")
                            .font(egui::FontId::proportional(20.0))
                            .strong()
                            .color(accent_color),
                    );
                });
            });
    }

    handle_focus_navigation(
        ui,
        load_btn_id,
        load_ir_btn_id,
        clear_ir_btn_id,
        oversample_id,
        activation_id,
    );

    // Repaint driver (CLAP-F022):
    // Request repaints only when something is actively changing. The baseview
    // event loop drives frames at ~15 ms. The idle early-exit in on_frame()
    // returns immediately when nothing changed, dropping per-frame cost to
    // near-zero. We do NOT set a long idle repaint here because it would
    // override shorter requests from widgets (spinner, VU decay, etc.).
    let needs_repaint = state.has_active_animations()  // error/toast banners
        || !state.peak_hold_is_stable()                // VU meters active
        || state.show_telemetry; // telemetry visible

    if needs_repaint {
        ui.ctx().request_repaint_after(Duration::from_millis(33));
    }
}

#[cfg(test)]
mod ui_test;
