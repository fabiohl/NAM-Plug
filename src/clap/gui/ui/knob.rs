// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
//! Custom knob widget with arc, glow, tooltip and automation halo.

use clack_plugin::host::HostSharedHandle;
use std::sync::atomic::Ordering;

use super::colors::{COL_AMBER, COL_BG, COL_BORDER, COL_BYPASS_OFF, COL_MUTED, COL_TEXT};

/// Renders a custom knob with:
/// - 48 segments (smooth arc)
/// - Arc glow during drag (E1)
/// - Tooltip with exact value on hover (E3)
/// - Ctrl+Drag for fine-tune ÷10 (E5)
/// - Mapping halo, automation pulsation and override color (A.3)
///
/// Returns `(response, new_value)`.
#[expect(
    clippy::too_many_arguments,
    reason = "GUI knob widget with many visual configuration parameters (position, size, colors, ranges, label) for flexible UI layout"
)]
pub fn knob_widget(
    ui: &mut egui::Ui,
    id: egui::Id,
    value: f32,
    range: std::ops::RangeInclusive<f32>,
    size: egui::Vec2,
    color: egui::Color32,
    accent_color: egui::Color32,
    indication: u8,
    indication_color: egui::Color32,
    tooltip_suffix: &str,
) -> (egui::Response, f32) {
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let response = ui.interact(rect, id, egui::Sense::click_and_drag());
    ui.memory_mut(|mem| mem.interested_in_focus(id, ui.layer_id()));

    let mut current_value = value;
    let range_len = range.end() - range.start();

    if response.dragged() {
        let delta_y = response.drag_delta().y;
        let ctrl_held = ui.input(|i| i.modifiers.ctrl);
        let sensitivity = if ctrl_held {
            range_len / 2000.0
        } else {
            range_len / 200.0
        };
        current_value = (current_value - delta_y * sensitivity).clamp(*range.start(), *range.end());
    }

    if response.hovered() {
        let scroll_y = ui.input(|i| {
            let mut sum_y = 0.0;
            for event in &i.raw.events {
                if let egui::Event::MouseWheel { delta, .. } = event {
                    sum_y += delta.y;
                }
            }
            sum_y
        });
        if scroll_y != 0.0 {
            let ctrl_held = ui.input(|i| i.modifiers.ctrl);
            let sensitivity = if ctrl_held {
                range_len / 10000.0
            } else {
                range_len / 1000.0
            };
            current_value =
                (current_value + scroll_y * sensitivity).clamp(*range.start(), *range.end());
        }
    }

    if response.has_focus() {
        let mut step = 0.0;
        ui.input(|i| {
            let ctrl_held = i.modifiers.ctrl;
            let step_size = if ctrl_held { 0.1 } else { 1.0 };
            if i.key_pressed(egui::Key::ArrowUp) || i.key_pressed(egui::Key::ArrowRight) {
                step = step_size;
            } else if i.key_pressed(egui::Key::ArrowDown) || i.key_pressed(egui::Key::ArrowLeft) {
                step = -step_size;
            }
        });
        if step != 0.0 {
            current_value = (current_value + step).clamp(*range.start(), *range.end());
        }
    }

    let response = response.on_hover_text(format!("{:.2}{}", current_value, tooltip_suffix));

    // Accessibility: register widget info for screen readers
    let label = format!("{:.2}{}", current_value, tooltip_suffix);
    let knob_enabled = ui.is_enabled();
    response.clone().widget_info(move || egui::WidgetInfo {
        typ: egui::WidgetType::DragValue,
        enabled: knob_enabled,
        label: Some(label.clone()),
        current_text_value: None,
        prev_text_value: None,
        hint_text: None,
        text_selection: None,
        selected: None,
        value: Some(current_value as f64),
    });

    let painter = ui.painter();
    let center = rect.center();
    let radius = rect.width() / 2.0 - 2.0;
    let frac = (current_value - range.start()) / range_len;
    let angle_start = -135.0f32.to_radians();
    let angle_end = 135.0f32.to_radians();
    let angle = angle_start + frac * (angle_end - angle_start);

    if indication & 1 != 0 {
        let dot_color = indication_color;
        let halo_radius = radius + 5.0;
        for i in 0..6 {
            let a = (i as f32) * (std::f32::consts::TAU / 6.0);
            let pos = center + egui::vec2(a.cos() * halo_radius, a.sin() * halo_radius);
            painter.circle_filled(pos, 1.5, dot_color);
        }
    }

    let enabled = ui.is_enabled();

    painter.circle_stroke(center, radius, egui::Stroke::new(3.0_f32, COL_BG));

    let mut active_arc_color = if !enabled {
        COL_BYPASS_OFF
    } else if indication & 4 != 0 {
        COL_AMBER
    } else if indication & 2 != 0 {
        indication_color
    } else {
        color
    };

    if indication & 2 != 0 {
        let time = ui.input(|i| i.time) as f32;
        let pulse = (time * std::f32::consts::PI).sin().abs();
        let alpha = 0.3 + 0.7 * pulse;
        active_arc_color = egui::Color32::from_rgba_unmultiplied(
            active_arc_color.r(),
            active_arc_color.g(),
            active_arc_color.b(),
            (active_arc_color.a() as f32 * alpha) as u8,
        );
        ui.ctx().request_repaint();
    }

    let num_segments = 48;
    let arc_radius = radius;
    const MAX_ARC_POINTS: usize = 49;
    let mut points_buf = [egui::pos2(0.0, 0.0); MAX_ARC_POINTS];
    let mut point_count = 0;
    for i in 0..=num_segments {
        let f = i as f32 / num_segments as f32;
        if f <= frac {
            let a = angle_start + f * (angle_end - angle_start) - std::f32::consts::FRAC_PI_2;
            points_buf[point_count] =
                center + egui::vec2(a.cos() * arc_radius, a.sin() * arc_radius);
            point_count += 1;
        }
    }
    if point_count > 1 {
        let arc_stroke = egui::Stroke::new(3.5_f32, active_arc_color);
        for w in points_buf[..point_count].windows(2) {
            painter.line_segment([w[0], w[1]], arc_stroke);
        }
        if response.dragged() {
            let glow_color = egui::Color32::from_rgba_unmultiplied(
                active_arc_color.r(),
                active_arc_color.g(),
                active_arc_color.b(),
                60,
            );
            let glow_stroke = egui::Stroke::new(7.0_f32, glow_color);
            for w in points_buf[..point_count].windows(2) {
                painter.line_segment([w[0], w[1]], glow_stroke);
            }
        }
    }

    let body_radius = radius - 3.0;
    let body_color = if enabled {
        COL_BORDER
    } else {
        egui::Color32::from_rgb(32, 36, 44)
    };
    painter.circle_filled(center, body_radius, body_color);
    painter.circle_stroke(center, body_radius, egui::Stroke::new(1.0_f32, COL_BORDER));

    if response.has_focus() && enabled {
        painter.circle_stroke(
            center,
            body_radius + 1.0,
            egui::Stroke::new(2.0_f32, accent_color),
        );
    }

    let pointer_angle = angle - std::f32::consts::FRAC_PI_2;
    let pointer_len_start = body_radius * 0.4;
    let pointer_len_end = body_radius * 0.92;
    let p_start = center
        + egui::vec2(
            pointer_angle.cos() * pointer_len_start,
            pointer_angle.sin() * pointer_len_start,
        );
    let p_end = center
        + egui::vec2(
            pointer_angle.cos() * pointer_len_end,
            pointer_angle.sin() * pointer_len_end,
        );
    let pointer_color = if enabled { COL_TEXT } else { COL_MUTED };
    painter.line(
        vec![p_start, p_end],
        egui::Stroke::new(rect.width() * 0.05, pointer_color),
    );

    (response, current_value)
}

/// Manages a knob with label, dB value, automation gestures and double-click to reset.
#[expect(
    clippy::too_many_arguments,
    reason = "GUI knob widget with many visual configuration parameters (position, size, colors, ranges, label) for flexible UI layout"
)]
pub fn handle_knob(
    ui: &mut egui::Ui,
    id: egui::Id,
    label: &str,
    range: std::ops::RangeInclusive<f32>,
    default_value: f32,
    atomic_val: &std::sync::atomic::AtomicU32,
    gesture_flags: &std::sync::atomic::AtomicU32,
    gui_param_generation: &std::sync::atomic::AtomicU32,
    param_index: usize,
    color: egui::Color32,
    accent_color: egui::Color32,
    host: &HostSharedHandle,
    knob_size: egui::Vec2,
    indication: u8,
    indication_color: egui::Color32,
    tooltip_suffix: &str,
) {
    const GESTURE_BITS_PER_PARAM: u32 = 3;
    const GESTURE_CHANGED_SHIFT: u32 = 0;
    const GESTURE_BEGIN_SHIFT: u32 = 1;
    const GESTURE_END_SHIFT: u32 = 2;

    let offset = param_index as u32 * GESTURE_BITS_PER_PARAM;

    ui.vertical(|ui| {
        let current_val = f32::from_bits(atomic_val.load(Ordering::Relaxed));

        let (response, new_val) = ui
            .vertical(|ui| {
                ui.horizontal(|ui| {
                    let available_width = ui.available_width();
                    let space = (available_width - knob_size.x) / 2.0;
                    if space > 0.0 {
                        ui.add_space(space);
                    }
                    knob_widget(
                        ui,
                        id,
                        current_val,
                        range,
                        knob_size,
                        color,
                        accent_color,
                        indication,
                        indication_color,
                        tooltip_suffix,
                    )
                })
                .inner
            })
            .inner;

        if response.drag_started() {
            gesture_flags.fetch_or(1 << (offset + GESTURE_BEGIN_SHIFT), Ordering::Relaxed);
            if let Some(params_ext) = host.get_extension::<clack_extensions::params::HostParams>() {
                params_ext.request_flush(host);
            }
        }

        let reset_clicked = response.double_clicked();
        let final_val = if reset_clicked {
            default_value
        } else {
            new_val
        };

        if final_val != current_val {
            let is_discrete = !response.dragged();
            if is_discrete {
                gesture_flags.fetch_or(1 << (offset + GESTURE_BEGIN_SHIFT), Ordering::Relaxed);
            }
            atomic_val.store(final_val.to_bits(), Ordering::Relaxed);
            gui_param_generation.fetch_add(1, Ordering::Release); // pairs with Acquire loads em processor/events.rs:94, extensions/params/audio.rs:70
            gesture_flags.fetch_or(1 << (offset + GESTURE_CHANGED_SHIFT), Ordering::Relaxed);
            if is_discrete {
                gesture_flags.fetch_or(1 << (offset + GESTURE_END_SHIFT), Ordering::Relaxed);
            }
            if let Some(params_ext) = host.get_extension::<clack_extensions::params::HostParams>() {
                params_ext.request_flush(host);
            }
        }

        if response.drag_stopped() {
            gesture_flags.fetch_or(1 << (offset + GESTURE_END_SHIFT), Ordering::Relaxed);
            if let Some(params_ext) = host.get_extension::<clack_extensions::params::HostParams>() {
                params_ext.request_flush(host);
            }
        }

        ui.add_space(5.0);
        ui.vertical_centered(|ui| {
            ui.label(
                egui::RichText::new(label)
                    .font(egui::FontId::proportional(11.0))
                    .strong()
                    .color(COL_MUTED),
            );
            let val_color = if ui.is_enabled() { COL_TEXT } else { COL_MUTED };
            ui.label(
                egui::RichText::new(format!("{:.1} dB", final_val))
                    .font(egui::FontId::monospace(10.0))
                    .color(val_color),
            );
        });
    });
}
