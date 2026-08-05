// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
//! CPU (egui painter) fallback rendering for the vertical VU meter.

use super::super::colors::{
    COL_BG, COL_BYPASS_OFF, COL_MUTED, COL_VU_GREEN, COL_VU_RED, COL_VU_YELLOW,
};

/// Renders the VU meter bar using egui's CPU painter as a fallback when OpenGL is not available.
#[expect(
    clippy::too_many_arguments,
    reason = "CPU meter widget with many visual parameters (position, size, colors, ranges) for flexible UI layout"
)]
pub(crate) fn render_cpu(
    ui: &egui::Ui,
    meter_rect: egui::Rect,
    meter_h: f32,
    peak_frac: f32,
    hold_frac: f32,
    hold_db: f32,
    green_frac: f32,
    yellow_frac: f32,
) {
    let enabled = ui.is_enabled();
    let painter = ui.painter();
    painter.rect_filled(meter_rect, 1.5, COL_BG);

    let draw_segment = |from_frac: f32, to_frac: f32, color: egui::Color32| {
        if to_frac > from_frac {
            let y_bottom = meter_rect.bottom() - from_frac * meter_h;
            let y_top = meter_rect.bottom() - to_frac * meter_h;
            let seg = egui::Rect::from_min_max(
                egui::pos2(meter_rect.left(), y_top),
                egui::pos2(meter_rect.right(), y_bottom),
            );
            painter.rect_filled(seg, 0.0, color);
        }
    };

    let (col_g, col_y, col_r) = if enabled {
        (COL_VU_GREEN, COL_VU_YELLOW, COL_VU_RED)
    } else {
        (COL_BYPASS_OFF, COL_BYPASS_OFF, COL_BYPASS_OFF)
    };

    draw_segment(0.0, peak_frac.min(green_frac), col_g);
    if peak_frac > green_frac {
        draw_segment(green_frac, peak_frac.min(yellow_frac), col_y);
    }
    if peak_frac > yellow_frac {
        draw_segment(yellow_frac, peak_frac, col_r);
    }

    if hold_frac > 0.0 {
        let y_hold = meter_rect.bottom() - hold_frac * meter_h;
        let hold_color = if enabled {
            if hold_db >= -3.0 {
                COL_VU_RED
            } else if hold_db >= -12.0 {
                COL_VU_YELLOW
            } else {
                COL_VU_GREEN
            }
        } else {
            COL_MUTED
        };
        painter.line(
            vec![
                egui::pos2(meter_rect.left() - 2.0, y_hold),
                egui::pos2(meter_rect.right() + 2.0, y_hold),
            ],
            egui::Stroke::new(1.5_f32, hold_color),
        );
    }
}
