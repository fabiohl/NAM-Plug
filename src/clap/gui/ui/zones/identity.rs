// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
//! Zone 1 (left): Identity — logo, version, SIMD badge, model load button, and Cab IR controls.

use crate::clap::gui::GuiHostBridge;
use crate::clap::plugin::NamClapShared;
use clack_plugin::host::HostSharedHandle;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::clap::gui::ui::{
    colors::{COL_BG, COL_BORDER, COL_MUTED, COL_PANEL, COL_TEXT, COL_VU_RED},
    simd::get_simd_badge,
    state::UiState,
};

use super::file_dialogs::{spawn_file_dialog, spawn_ir_file_dialog};

pub(crate) fn draw_zone1_identity(
    ui: &mut egui::Ui,
    shared: &NamClapShared,
    host: &HostSharedHandle,
    state: &mut UiState,
    accent_color: egui::Color32,
) -> (Option<egui::Id>, Option<egui::Id>, Option<egui::Id>) {
    let mut load_btn_id = None;
    let mut load_ir_btn_id = None;
    let mut clear_ir_btn_id = None;

    ui.allocate_ui(egui::vec2(135.0, 230.0), |ui| {
        ui.vertical(|ui| {
            render_brand_header(ui, accent_color);
            load_btn_id = render_model_section(ui, shared, host, state, accent_color);
            let (ir_id, clear_id) = render_ir_section(ui, shared, host, state, accent_color);
            load_ir_btn_id = ir_id;
            clear_ir_btn_id = clear_id;
        });
    });

    (load_btn_id, load_ir_btn_id, clear_ir_btn_id)
}

fn render_brand_header(ui: &mut egui::Ui, accent_color: egui::Color32) {
    ui.add_space(8.0);

    ui.label(
        egui::RichText::new("NAM-rs⚡")
            .font(egui::FontId::proportional(24.0))
            .strong()
            .color(accent_color),
    );

    ui.label(
        egui::RichText::new("Neural Amp Modeler")
            .font(egui::FontId::proportional(9.5))
            .color(COL_MUTED),
    );

    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                .font(egui::FontId::proportional(9.0))
                .color(COL_MUTED),
        );
        ui.add_space(4.0);
        let simd = get_simd_badge();
        let badge_color = if simd.contains("AVX") {
            accent_color
        } else {
            COL_MUTED
        };
        let response = ui.label(
            egui::RichText::new(simd)
                .font(egui::FontId::monospace(8.0))
                .strong()
                .color(badge_color),
        );
        response.on_hover_text(format!(
            "Active runtime SIMD engine backend: {}.",
            neural_amp_modeler_rs::math::common::SIMD_MATH.name
        ));
    });

    ui.add_space(8.0);
}

fn render_model_section(
    ui: &mut egui::Ui,
    shared: &NamClapShared,
    host: &HostSharedHandle,
    state: &mut UiState,
    accent_color: egui::Color32,
) -> Option<egui::Id> {
    ui.label(
        egui::RichText::new("MODEL")
            .font(egui::FontId::proportional(9.0))
            .strong()
            .color(COL_MUTED),
    );
    ui.add_space(2.0);

    // "Load Model" button
    let load_btn = ui.add(
        egui::Button::new(
            egui::RichText::new("📂 Load Model")
                .font(egui::FontId::proportional(11.5))
                .strong()
                .color(COL_TEXT),
        )
        .fill(COL_PANEL)
        .stroke(egui::Stroke::new(1.0_f32, COL_BORDER)),
    );
    let btn_id = load_btn.id;
    load_btn.widget_info(|| egui::WidgetInfo {
        typ: egui::WidgetType::Button,
        enabled: true,
        label: Some("Load NAM model file".into()),
        current_text_value: None,
        prev_text_value: None,
        hint_text: None,
        text_selection: None,
        selected: None,
        value: None,
    });
    ui.memory_mut(|mem| mem.interested_in_focus(btn_id, ui.layer_id()));

    if load_btn.has_focus() {
        ui.painter().rect_stroke(
            load_btn.rect,
            2.0,
            egui::Stroke::new(2.0_f32, accent_color),
            egui::StrokeKind::Outside,
        );
    }

    let mut load_clicked = load_btn.clicked();
    if load_btn.has_focus() {
        ui.input(|i| {
            if i.key_pressed(egui::Key::Space) || i.key_pressed(egui::Key::Enter) {
                load_clicked = true;
            }
        });
    }

    if load_clicked
        && !shared.cold.ui_loading.load(Ordering::Relaxed)
        && let Some(dialog_state_arc) = shared.cold.dialog_state.as_ref()
        && !dialog_state_arc.active.load(Ordering::Relaxed)
    {
        shared.cold.ui_loading.store(true, Ordering::Relaxed);
        dialog_state_arc.active.store(true, Ordering::Relaxed);
        let host_static = GuiHostBridge::new(host).as_static();
        let handle = spawn_file_dialog(
            Arc::clone(dialog_state_arc),
            host_static,
            Arc::clone(&shared.cold.alive_fence),
        );
        if let Ok(mut guard) = shared.cold.dialog_handle_sink.lock() {
            *guard = Some(handle);
        }
    }

    ui.add_space(6.0);

    let is_error_active = if let Some(expiration) = state.error_expiration {
        if Instant::now() < expiration {
            ui.ctx().request_repaint();
            true
        } else {
            state.error_expiration = None;
            false
        }
    } else {
        false
    };

    let model_name: &str = if is_error_active {
        if state.model_display_name != "⚠ Load failed" {
            state.model_display_name.clear();
            state.model_display_name.push_str("⚠ Load failed");
        }
        &state.model_display_name
    } else if shared.cold.ui_loading.load(Ordering::Relaxed) {
        ui.ctx().request_repaint_after(Duration::from_millis(100));
        let elapsed = ui.input(|i| i.time);
        let frames = ["Loading", "Loading.", "Loading..", "Loading..."];
        let idx = (elapsed * 4.0) as usize % frames.len();
        state.model_display_name.clear();
        state.model_display_name.push_str(frames[idx]);
        &state.model_display_name
    } else {
        let name_guard = shared
            .cold
            .ui_model_name
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if name_guard.is_empty() {
            if state.model_display_name != "No model loaded" {
                state.model_display_name.clear();
                state.model_display_name.push_str("No model loaded");
            }
            &state.model_display_name
        } else {
            if *name_guard != state.model_display_name {
                state.model_display_name.clear();
                state.model_display_name.push_str(&name_guard);
            }
            &state.model_display_name
        }
    };

    let text_color = if is_error_active {
        COL_VU_RED
    } else {
        COL_MUTED
    };

    let frame_res = egui::Frame::new()
        .fill(COL_BG)
        .stroke(egui::Stroke::new(
            1.0_f32,
            if is_error_active {
                COL_VU_RED
            } else {
                COL_BORDER
            },
        ))
        .corner_radius(egui::CornerRadius::same(3))
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.set_min_width(120.0);
            ui.set_max_width(120.0);
            ui.add(
                egui::Label::new(
                    egui::RichText::new(model_name)
                        .font(egui::FontId::proportional(9.5))
                        .color(text_color)
                        .italics(),
                )
                .wrap(),
            );
        });

    if is_error_active {
        frame_res.response.on_hover_text(&state.error_msg);
    } else {
        frame_res.response.on_hover_text(model_name);
    }

    ui.add_space(6.0);
    Some(btn_id)
}

fn render_ir_section(
    ui: &mut egui::Ui,
    shared: &NamClapShared,
    host: &HostSharedHandle,
    state: &mut UiState,
    accent_color: egui::Color32,
) -> (Option<egui::Id>, Option<egui::Id>) {
    let mut clear_ir_btn_id = None;

    ui.label(
        egui::RichText::new("CAB SIM IR")
            .font(egui::FontId::proportional(9.0))
            .strong()
            .color(COL_MUTED),
    );
    ui.add_space(2.0);

    let has_ir = shared
        .cold
        .ir_path
        .lock()
        .map(|g| g.is_some())
        .unwrap_or(false);

    let ir_loading = shared.cold.ui_ir_loading.load(Ordering::Relaxed);

    // "Load IR" button
    let load_ir_btn = ui.add(
        egui::Button::new(
            egui::RichText::new("📂 Load IR")
                .font(egui::FontId::proportional(11.5))
                .strong()
                .color(COL_TEXT),
        )
        .fill(COL_PANEL)
        .stroke(egui::Stroke::new(1.0_f32, COL_BORDER)),
    );
    let ir_btn_id = load_ir_btn.id;
    let load_ir_btn_id = Some(ir_btn_id);
    load_ir_btn.widget_info(|| egui::WidgetInfo {
        typ: egui::WidgetType::Button,
        enabled: true,
        label: Some("Load cabinet simulator impulse response".into()),
        current_text_value: None,
        prev_text_value: None,
        hint_text: None,
        text_selection: None,
        selected: None,
        value: None,
    });
    ui.memory_mut(|mem| mem.interested_in_focus(ir_btn_id, ui.layer_id()));

    if load_ir_btn.has_focus() {
        ui.painter().rect_stroke(
            load_ir_btn.rect,
            2.0,
            egui::Stroke::new(2.0_f32, accent_color),
            egui::StrokeKind::Outside,
        );
    }

    let mut load_ir_clicked = load_ir_btn.clicked();
    if load_ir_btn.has_focus() {
        ui.input(|i| {
            if i.key_pressed(egui::Key::Space) || i.key_pressed(egui::Key::Enter) {
                load_ir_clicked = true;
            }
        });
    }

    if load_ir_clicked
        && !ir_loading
        && let Some(ir_dialog_state_arc) = shared.cold.ir_dialog_state.as_ref()
        && !ir_dialog_state_arc.active.load(Ordering::Relaxed)
    {
        shared.cold.ui_ir_loading.store(true, Ordering::Relaxed);
        ir_dialog_state_arc.active.store(true, Ordering::Relaxed);
        let host_static = GuiHostBridge::new(host).as_static();
        let handle = spawn_ir_file_dialog(
            Arc::clone(ir_dialog_state_arc),
            host_static,
            Arc::clone(&shared.cold.alive_fence),
        );
        if let Ok(mut guard) = shared.cold.ir_dialog_handle_sink.lock() {
            *guard = Some(handle);
        }
    }

    ui.add_space(4.0);

    // "Clear IR" button (only show if IR is loaded)
    if has_ir && !ir_loading {
        let clear_ir_btn = ui.add(
            egui::Button::new(
                egui::RichText::new("🗑 Clear IR")
                    .font(egui::FontId::proportional(10.5))
                    .color(COL_MUTED),
            )
            .fill(COL_PANEL)
            .stroke(egui::Stroke::new(1.0_f32, COL_BORDER)),
        );
        let clear_ir_id = clear_ir_btn.id;
        clear_ir_btn_id = Some(clear_ir_id);
        ui.memory_mut(|mem| mem.interested_in_focus(clear_ir_id, ui.layer_id()));
        clear_ir_btn.widget_info(|| egui::WidgetInfo {
            typ: egui::WidgetType::Button,
            enabled: true,
            label: Some("Clear cabinet simulator impulse response".into()),
            current_text_value: None,
            prev_text_value: None,
            hint_text: None,
            text_selection: None,
            selected: None,
            value: None,
        });

        if clear_ir_btn.has_focus() {
            ui.painter().rect_stroke(
                clear_ir_btn.rect,
                2.0,
                egui::Stroke::new(2.0_f32, accent_color),
                egui::StrokeKind::Outside,
            );
        }

        let mut clear_ir_clicked = clear_ir_btn.clicked();
        if clear_ir_btn.has_focus() {
            ui.input(|i| {
                if i.key_pressed(egui::Key::Space) || i.key_pressed(egui::Key::Enter) {
                    clear_ir_clicked = true;
                }
            });
        }

        if clear_ir_clicked {
            shared.cold.ui_clear_ir.store(true, Ordering::Relaxed);
            // R-09: fence-gate the host notification — a stale
            // request_callback after destroy would dispatch to a
            // freed clap_plugin_t.
            if shared.cold.alive_fence.load(Ordering::Acquire) {
                let host_static = GuiHostBridge::new(host).as_static();
                host_static.request_callback();
            }
        }
    }

    ui.add_space(4.0);

    // IR file name display
    let ir_is_error = if let Some(expiration) = state.ir_error_expiration {
        if Instant::now() < expiration {
            ui.ctx().request_repaint();
            state.ir_display_name == "⚠ IR load failed"
        } else {
            state.ir_error_expiration = None;
            false
        }
    } else {
        false
    };

    let ir_name: &str = if ir_is_error {
        if state.ir_display_name != "⚠ IR load failed" {
            state.ir_display_name.clear();
            state.ir_display_name.push_str("⚠ IR load failed");
        }
        &state.ir_display_name
    } else if ir_loading {
        ui.ctx().request_repaint_after(Duration::from_millis(100));
        let elapsed = ui.input(|i| i.time);
        let frames = ["Loading", "Loading.", "Loading..", "Loading..."];
        let idx = (elapsed * 4.0) as usize % frames.len();
        state.ir_display_name.clear();
        state.ir_display_name.push_str(frames[idx]);
        &state.ir_display_name
    } else if let Ok(ir_guard) = shared.cold.ir_path.lock() {
        if let Some(ref path) = *ir_guard {
            let basename = std::path::Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(path);
            if state.ir_display_name != basename {
                state.ir_display_name.clear();
                state.ir_display_name.push_str(basename);
            }
            &state.ir_display_name
        } else {
            if state.ir_display_name != "No IR loaded" {
                state.ir_display_name.clear();
                state.ir_display_name.push_str("No IR loaded");
            }
            &state.ir_display_name
        }
    } else {
        if state.ir_display_name != "No IR loaded" {
            state.ir_display_name.clear();
            state.ir_display_name.push_str("No IR loaded");
        }
        &state.ir_display_name
    };

    let ir_text_color = if ir_is_error { COL_VU_RED } else { COL_MUTED };

    let ir_frame_res = egui::Frame::new()
        .fill(COL_BG)
        .stroke(egui::Stroke::new(
            1.0_f32,
            if ir_is_error { COL_VU_RED } else { COL_BORDER },
        ))
        .corner_radius(egui::CornerRadius::same(3))
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.set_min_width(120.0);
            ui.set_max_width(120.0);
            ui.add(
                egui::Label::new(
                    egui::RichText::new(ir_name)
                        .font(egui::FontId::proportional(9.5))
                        .color(ir_text_color)
                        .italics(),
                )
                .wrap(),
            );
        });

    if ir_is_error {
        ir_frame_res.response.on_hover_text(&state.ir_error_msg);
    } else {
        ir_frame_res.response.on_hover_text(ir_name);
    }

    (load_ir_btn_id, clear_ir_btn_id)
}
