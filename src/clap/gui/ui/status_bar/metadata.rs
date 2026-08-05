// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
//! Model metadata display: architecture, topology, author, gear, tone, date.

use crate::clap::gui::ui::colors::{COL_BORDER, COL_MUTED};
use crate::clap::gui::ui::state::UiState;
use crate::clap::plugin::NamModelMetadata;
use std::fmt::Write;

pub(crate) fn update_metadata_cache(state: &mut UiState, meta: &NamModelMetadata) {
    let metadata_changed = state.cached_metadata.as_ref().is_none_or(|cached| {
        cached.architecture != meta.architecture
            || cached.topology != meta.topology
            || cached.modeled_by != meta.modeled_by
            || cached.gear_make != meta.gear_make
            || cached.gear_model != meta.gear_model
            || cached.gear_type != meta.gear_type
            || cached.tone_type != meta.tone_type
            || cached.date != meta.date
            || cached.sample_rate != meta.sample_rate
    });

    if metadata_changed {
        state.metadata_display.clear();

        let mut text_buf = String::new();
        let _ = write!(text_buf, "Model: {} ({})", meta.architecture, meta.topology);
        state.metadata_display.push((
            text_buf,
            "Neural network architecture type and topology geometry".to_string(),
        ));

        if meta.sample_rate > 0 {
            let mut buf = String::new();
            let _ = write!(buf, "SR: {} Hz", meta.sample_rate);
            state
                .metadata_display
                .push((buf, "Native sample rate expected by the model".to_string()));
        }

        if let Some(ref author) = meta.modeled_by {
            let trimmed = author.trim();
            if !trimmed.is_empty() {
                let mut buf = String::new();
                let _ = write!(buf, "Author: {}", trimmed);
                state
                    .metadata_display
                    .push((buf, "Creator / trainer of the model".to_string()));
            }
        }

        let mut gear_parts: Vec<&str> = Vec::new();
        if let Some(ref make) = meta.gear_make {
            let trimmed = make.trim();
            if !trimmed.is_empty() {
                gear_parts.push(trimmed);
            }
        }
        if let Some(ref model) = meta.gear_model {
            let trimmed = model.trim();
            if !trimmed.is_empty() {
                gear_parts.push(trimmed);
            }
        }
        if !gear_parts.is_empty() {
            let mut buf = String::new();
            let _ = write!(buf, "Gear: {}", gear_parts.join(" "));
            if let Some(ref gtype) = meta.gear_type {
                let trimmed = gtype.trim();
                if !trimmed.is_empty() {
                    let _ = write!(buf, " ({})", trimmed);
                }
            }
            state.metadata_display.push((
                buf,
                "Original hardware equipment modeled by this neural network".to_string(),
            ));
        }

        if let Some(ref tone) = meta.tone_type {
            let trimmed = tone.trim();
            if !trimmed.is_empty() {
                let mut buf = String::new();
                let _ = write!(buf, "Tone: {}", trimmed);
                state
                    .metadata_display
                    .push((buf, "Classification style of the tone".to_string()));
            }
        }

        if let Some(ref date) = meta.date {
            let trimmed = date.trim();
            if !trimmed.is_empty() {
                let mut buf = String::new();
                let _ = write!(buf, "Date: {}", trimmed);
                state
                    .metadata_display
                    .push((buf, "Model creation / export date".to_string()));
            }
        }

        state.cached_metadata = Some(meta.clone());
        state.metadata_display_version = state.metadata_display_version.wrapping_add(1);
    }
}

pub(crate) fn draw_metadata_strings(
    ui: &mut egui::Ui,
    state: &mut UiState,
    accent_color: egui::Color32,
) {
    if state.metadata_display.is_empty() {
        return;
    }
    let available_width = ui.available_width();
    let width_changed = (available_width - state.metadata_cached_width).abs() > 1.0;
    let content_stale = state.metadata_last_font_version != state.metadata_display_version;

    let calculated_font_size = match state.metadata_cached_font_size {
        Some(sz) if !width_changed && !content_stale => sz,
        _ => {
            let baseline_s = 10.0;
            let baseline_font = egui::FontId::proportional(baseline_s);
            let mut sum_widths = 0.0;
            for (text, _) in &state.metadata_display {
                let galley = ui.painter().layout_no_wrap(
                    text.clone(),
                    baseline_font.clone(),
                    egui::Color32::WHITE,
                );
                sum_widths += galley.rect.width();
            }
            let separator_text = " | ".to_string();
            let sep_galley =
                ui.painter()
                    .layout_no_wrap(separator_text, baseline_font, egui::Color32::WHITE);
            sum_widths += sep_galley.rect.width() * (state.metadata_display.len() - 1) as f32;

            let num_gaps = (state.metadata_display.len() * 2 - 2) as f32;
            let total_gap_width = num_gaps * ui.spacing().item_spacing.x;

            let target_width = available_width - total_gap_width - 8.0;
            let sz = if sum_widths > 0.0 && target_width > 0.0 {
                let scale = target_width / sum_widths;
                (baseline_s * scale).clamp(9.0, 10.0)
            } else {
                9.0
            };
            state.metadata_cached_font_size = Some(sz);
            state.metadata_cached_width = available_width;
            state.metadata_last_font_version = state.metadata_display_version;
            sz
        }
    };
    let font_size = egui::FontId::proportional(calculated_font_size);

    ui.horizontal(|ui| {
        let mut first = true;
        for (i, (text, tooltip)) in state.metadata_display.iter().enumerate() {
            if !first {
                ui.label(
                    egui::RichText::new("|")
                        .font(font_size.clone())
                        .color(COL_BORDER),
                );
            }
            first = false;

            let color = if i == 0 { accent_color } else { COL_MUTED };
            let label = ui.label(
                egui::RichText::new(text.as_str())
                    .font(font_size.clone())
                    .color(color),
            );
            label.on_hover_text(tooltip.as_str());
        }
    });
}
