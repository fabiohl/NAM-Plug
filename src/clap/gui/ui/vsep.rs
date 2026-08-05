// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
//! Styled vertical separator.

use super::colors::COL_BORDER;

/// Renders a styled vertical separator (m5) — thin #2E3440 line instead of egui's default Separator.
pub fn styled_vsep(ui: &mut egui::Ui) {
    let space = 6.0;
    ui.add_space(space);
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(1.0, ui.available_height()), egui::Sense::hover());
    ui.painter().line(
        vec![rect.center_top(), rect.center_bottom()],
        egui::Stroke::new(0.5_f32, COL_BORDER),
    );
    ui.add_space(space);
}
