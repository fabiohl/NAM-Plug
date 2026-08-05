// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
//! Bypass Rocker Switch toggle with LED, glow and automation halo.

use crate::clap::extensions::params::{bypass_bool_to_u32, bypass_u32_to_bool};
use clack_plugin::host::HostSharedHandle;
use std::sync::atomic::Ordering;

use super::colors::{COL_BORDER, COL_BYPASS_OFF, COL_MUTED, COL_PANEL};

/// Draws the bypass toggle with LED and "BYPASS" / "ACTIVE" labels.
/// `indication` encodes the bits of `param_indication`: bit 0=IS_MAPPED, bit 1=IS_AUTOMATING, bit 2=IS_OVERRIDING.
#[expect(
    clippy::too_many_arguments,
    reason = "GUI bypass button widget with many visual and layout parameters (position, size, colors, label) for flexible UI layout"
)]
pub fn handle_bypass(
    ui: &mut egui::Ui,
    id: egui::Id,
    atomic_val: &std::sync::atomic::AtomicU32,
    gesture_flags: &std::sync::atomic::AtomicU32,
    gui_param_generation: &std::sync::atomic::AtomicU32,
    param_index: usize,
    accent_color: egui::Color32,
    host: &HostSharedHandle,
    indication: u8,
    indication_color: egui::Color32,
) {
    const GESTURE_BITS_PER_PARAM: u32 = 3;
    const GESTURE_CHANGED_SHIFT: u32 = 0;
    const GESTURE_BEGIN_SHIFT: u32 = 1;
    const GESTURE_END_SHIFT: u32 = 2;

    let offset = param_index as u32 * GESTURE_BITS_PER_PARAM;
    let current_bypass = bypass_u32_to_bool(atomic_val.load(Ordering::Relaxed));
    let button_size = egui::vec2(32.0, 56.0);

    let response = ui
        .horizontal(|ui| {
            let available_width = ui.available_width();
            let space = (available_width - button_size.x) / 2.0;
            if space > 0.0 {
                ui.add_space(space);
            }
            let (rect, _) = ui.allocate_exact_size(button_size, egui::Sense::hover());
            ui.interact(rect, id, egui::Sense::click())
        })
        .inner;

    ui.memory_mut(|mem| mem.interested_in_focus(id, ui.layer_id()));

    let mut toggle_bypass = response.clicked();
    if response.has_focus() {
        ui.input(|i| {
            if i.key_pressed(egui::Key::Space) || i.key_pressed(egui::Key::Enter) {
                toggle_bypass = true;
            }
        });
    }

    // Accessibility: register widget info for screen readers
    let bypass_label = if current_bypass {
        "Bypass: On"
    } else {
        "Bypass: Off"
    };
    response.clone().widget_info(move || egui::WidgetInfo {
        typ: egui::WidgetType::Checkbox,
        enabled: true,
        label: Some(bypass_label.to_string()),
        current_text_value: None,
        prev_text_value: None,
        hint_text: None,
        text_selection: None,
        selected: Some(current_bypass),
        value: None,
    });

    if toggle_bypass {
        let new_bypass = !current_bypass;
        gesture_flags.fetch_or(1 << (offset + GESTURE_BEGIN_SHIFT), Ordering::Relaxed);
        atomic_val.store(bypass_bool_to_u32(new_bypass), Ordering::Relaxed);
        gui_param_generation.fetch_add(1, Ordering::Release); // pairs with Acquire loads em processor/events.rs:94, extensions/params/audio.rs:70
        gesture_flags.fetch_or(1 << (offset + GESTURE_CHANGED_SHIFT), Ordering::Relaxed);
        gesture_flags.fetch_or(1 << (offset + GESTURE_END_SHIFT), Ordering::Relaxed);
        if let Some(params_ext) = host.get_extension::<clack_extensions::params::HostParams>() {
            params_ext.request_flush(host);
        }
    }

    let painter = ui.painter();
    let rect = response.rect;

    painter.rect_filled(rect, 5.0, COL_PANEL);
    painter.rect_stroke(
        rect,
        5.0,
        egui::Stroke::new(1.5_f32, COL_BORDER),
        egui::StrokeKind::Inside,
    );

    if response.has_focus() {
        painter.rect_stroke(
            rect,
            5.0,
            egui::Stroke::new(2.0_f32, accent_color),
            egui::StrokeKind::Outside,
        );
    }

    let mut led_color = if current_bypass {
        COL_BYPASS_OFF
    } else {
        accent_color
    };

    let is_automating = indication & 2 != 0;
    if is_automating {
        let time = ui.input(|i| i.time) as f32;
        let pulse = (time * std::f32::consts::PI).sin().abs();
        let alpha = 0.3 + 0.7 * pulse;
        led_color = egui::Color32::from_rgba_unmultiplied(
            led_color.r(),
            led_color.g(),
            led_color.b(),
            (255.0 * alpha) as u8,
        );
        ui.ctx().request_repaint();
    }

    let led_rect = egui::Rect::from_center_size(rect.center(), egui::vec2(14.0, 8.0));
    painter.rect_filled(led_rect, 2.0, led_color);

    if !current_bypass {
        let glow = egui::Color32::from_rgba_unmultiplied(
            accent_color.r(),
            accent_color.g(),
            accent_color.b(),
            40,
        );
        painter.rect_filled(led_rect.expand(3.0), 4.0, glow);
    }

    if indication & 1 != 0 {
        let dot_color = indication_color;
        let corners = [
            rect.left_top() + egui::vec2(4.0, 4.0),
            rect.right_top() + egui::vec2(-4.0, 4.0),
            rect.left_bottom() + egui::vec2(4.0, -4.0),
            rect.right_bottom() + egui::vec2(-4.0, -4.0),
        ];
        for pos in &corners {
            painter.circle_filled(*pos, 1.5, dot_color);
        }
    }

    ui.add_space(14.0);
    ui.vertical_centered(|ui| {
        let status_text = if current_bypass { "BYPASSED" } else { "ACTIVE" };
        let text_color = if current_bypass { COL_MUTED } else { led_color };
        ui.label(
            egui::RichText::new(status_text)
                .font(egui::FontId::proportional(11.0))
                .strong()
                .color(text_color),
        );
    });
}
