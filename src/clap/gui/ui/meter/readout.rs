// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
//! Peak-dB readout text and label for the VU meter.

use super::super::colors::{COL_MUTED, COL_VU_GREEN, COL_VU_RED, COL_VU_YELLOW};

/// Renders the peak-dB numeric readout and label below the meter bar.
pub(crate) fn render_readout(ui: &mut egui::Ui, peak_db: f32, label: &str) {
    let enabled = ui.is_enabled();
    ui.add_space(2.0);
    ui.vertical_centered(|ui| {
        let peak_text = if !enabled || peak_db <= -59.9 {
            "-inf".to_string()
        } else {
            format!("{:.1}", peak_db)
        };

        let peak_color = if !enabled {
            COL_MUTED
        } else if peak_db >= -3.0 {
            COL_VU_RED
        } else if peak_db >= -12.0 {
            COL_VU_YELLOW
        } else {
            COL_VU_GREEN
        };

        ui.label(
            egui::RichText::new(peak_text)
                .font(egui::FontId::monospace(9.5))
                .strong()
                .color(peak_color),
        );
    });

    ui.add_space(2.0);
    ui.vertical_centered(|ui| {
        ui.label(
            egui::RichText::new(label)
                .font(egui::FontId::monospace(9.0))
                .color(COL_MUTED),
        );
    });
}
