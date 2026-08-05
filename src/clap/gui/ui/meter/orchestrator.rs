// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
//! Orchestrator: dispatches vertical VU meter rendering to glow, CPU, or readout.

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use super::super::colors::{COL_BYPASS_OFF, COL_MUTED, COL_VU_RED};
use super::super::state::{VuMeterSharedState, VuUniforms};

/// Draws a vertical VU meter with tricolor gradient, analog peak hold fade (E2),
/// persistent clipping LED on top (E4) and L/R labels above and below (m4).
#[expect(
    clippy::too_many_arguments,
    reason = "Meter orchestrator with many GUI coordination parameters for multi-widget meter management"
)]
pub(crate) fn draw_vertical_meter(
    ui: &mut egui::Ui,
    peak_val: f32,
    hold_val: &mut f32,
    hold_time: &mut Instant,
    clipped: &mut bool,
    label: &str,
    vu_program: Option<::glow::Program>,
    vu_vao: Option<::glow::VertexArray>,
    vu_shared_state: &mut Option<Arc<VuMeterSharedState>>,
    vu_callback: &mut Option<Arc<egui_glow::CallbackFn>>,
    vu_uniforms: Arc<OnceLock<VuUniforms>>,
) {
    let meter_h = 130.0f32;
    let meter_w = 16.0f32;

    ui.vertical(|ui| {
        if !label.is_empty() {
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new(label)
                        .font(egui::FontId::monospace(9.0))
                        .color(COL_MUTED),
                );
            });
            ui.add_space(2.0);
        }

        let desired_led = egui::vec2(meter_w, 6.0);
        let led_rect = ui
            .horizontal(|ui| {
                let avail = ui.available_width();
                let space = (avail - meter_w) / 2.0;
                if space > 0.0 {
                    ui.add_space(space);
                }
                let (r, _) = ui.allocate_exact_size(desired_led, egui::Sense::hover());
                r
            })
            .inner;
        {
            let enabled = ui.is_enabled();
            let painter = ui.painter();
            let clip_color = if *clipped {
                if enabled { COL_VU_RED } else { COL_BYPASS_OFF }
            } else {
                egui::Color32::from_rgb(50, 30, 30)
            };
            painter.rect_filled(led_rect, 1.0, clip_color);
        }
        let led_response = ui.interact(
            led_rect,
            ui.id().with((label, "clip_led")),
            egui::Sense::click(),
        );
        if led_response.clicked() {
            *clipped = false;
        }

        ui.add_space(2.0);

        let desired_size = egui::vec2(meter_w, meter_h);
        let meter_rect = ui
            .horizontal(|ui| {
                let avail = ui.available_width();
                let space = (avail - meter_w) / 2.0;
                if space > 0.0 {
                    ui.add_space(space);
                }
                let (r, _) = ui.allocate_exact_size(desired_size, egui::Sense::hover());
                r
            })
            .inner;

        let meter_response = ui.interact(
            meter_rect,
            ui.id().with((label, "meter_bar")),
            egui::Sense::click(),
        );
        if meter_response.clicked() {
            *clipped = false;
        }

        let now = Instant::now();
        if peak_val >= *hold_val {
            *hold_val = peak_val;
            *hold_time = now;
        } else if now.duration_since(*hold_time) > Duration::from_secs(2) {
            *hold_val = (*hold_val * 0.95_f32).max(peak_val).max(0.0);
        }

        if peak_val >= 1.0 {
            *clipped = true;
        }

        let peak_db = if peak_val > 1e-5 {
            20.0 * peak_val.log10()
        } else {
            -60.0
        };
        let hold_db = if *hold_val > 1e-5 {
            20.0 * (*hold_val).log10()
        } else {
            -60.0
        };

        let min_db = -60.0f32;
        let max_db = 6.0f32;
        let db_to_frac = |db: f32| ((db - min_db) / (max_db - min_db)).clamp(0.0, 1.0);

        let peak_frac = db_to_frac(peak_db);
        let hold_frac = db_to_frac(hold_db);
        let green_frac = db_to_frac(-12.0);
        let yellow_frac = db_to_frac(-3.0);

        let hold_color_type = if hold_db >= -3.0 {
            2
        } else if hold_db >= -12.0 {
            1
        } else {
            0
        };

        let drawn_with_glow = super::glow::render_glow(
            ui,
            vu_program,
            vu_vao,
            vu_shared_state,
            vu_callback,
            vu_uniforms,
            meter_rect,
            peak_frac,
            hold_frac,
            hold_color_type,
        );

        if !drawn_with_glow {
            super::cpu::render_cpu(
                ui,
                meter_rect,
                meter_h,
                peak_frac,
                hold_frac,
                hold_db,
                green_frac,
                yellow_frac,
            );
        }

        super::readout::render_readout(ui, peak_db, label);
    });
}
