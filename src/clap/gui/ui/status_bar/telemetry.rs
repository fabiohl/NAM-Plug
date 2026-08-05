// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
//! Telemetry display: DSP load, sample rate, latency, RT priority, overloads.

use crate::clap::gui::ui::colors::{COL_AMBER, COL_BORDER, COL_MUTED, COL_VU_GREEN, COL_VU_RED};
use crate::clap::gui::ui::state::UiState;
use crate::clap::plugin::NamClapShared;
use std::fmt::Write;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

pub(crate) fn update_telemetry_state(state: &mut UiState, shared: &NamClapShared) {
    let now = Instant::now();
    if now.duration_since(state.last_telem_update) < Duration::from_secs(1)
        && (state.telem_cycles != 0 || state.telem_last_n != 0)
    {
        return;
    }

    state.telem_cycles = shared.cold.rt_status.dsp_cycle_time.load(Ordering::Relaxed);
    state.telem_last_n = shared.cold.rt_status.last_n_samples.load(Ordering::Relaxed);
    state.telem_prio = shared.cold.rt_status.rt_priority.load(Ordering::Relaxed);
    state.telem_overloads = shared.cold.rt_status.dsp_overloads.load(Ordering::Relaxed);

    let sr = shared.cold.sample_rate.load(Ordering::Relaxed);
    let lat = shared.rt_to_ui.current_latency.load(Ordering::Relaxed);

    if sr > 0 && state.telem_last_n > 0 {
        let budget_ns = (state.telem_last_n as u64 * 1_000_000_000) / sr as u64;
        state.telem_budget_ns = budget_ns;
        state.telem_cycle_ns = state.telem_cycles;
        state.telem_load_pct = ((state.telem_cycles as f64 / budget_ns as f64) * 100.0).min(999.0);
    } else {
        state.telem_budget_ns = 0;
        state.telem_cycle_ns = 0;
        state.telem_load_pct = 0.0;
    }

    state.status_strings[0].clear();
    if sr == 0 {
        state.status_strings[0].push_str("SR: Off");
    } else {
        let _ = write!(state.status_strings[0], "SR: {:.1} kHz", sr as f64 / 1000.0);
    }

    state.status_strings[1].clear();
    if lat == 0 {
        state.status_strings[1].push_str("Lat: 0 ms");
    } else {
        let lat_ms = (lat as f64 * 1000.0) / sr.max(1) as f64;
        let _ = write!(
            state.status_strings[1],
            "Lat: {:.1} ms ({} spl)",
            lat_ms, lat
        );
    }

    state.status_strings[2].clear();
    let _ = write!(state.status_strings[2], "DSP: {:.1}%", state.telem_load_pct);

    state.status_strings[3].clear();
    let _ = write!(state.status_strings[3], "Cycles: {}", state.telem_cycles);

    state.status_strings[4].clear();
    let _ = write!(state.status_strings[4], "Last N: {}", state.telem_last_n);

    state.status_strings[5].clear();
    let _ = write!(state.status_strings[5], "RT Prio: {}", state.telem_prio);

    state.status_strings[6].clear();
    let _ = write!(
        state.status_strings[6],
        "Overloads: {}",
        state.telem_overloads
    );

    let status_bits = shared.cold.rt_status.status_bits.load(Ordering::Relaxed);
    state.status_strings[7].clear();
    let _ = write!(state.status_strings[7], "Flags: {:#X}", status_bits);

    state.last_telem_update = now;
}

pub(crate) fn draw_telemetry_strings(
    ui: &mut egui::Ui,
    state: &mut UiState,
    shared: &NamClapShared,
    accent_color: egui::Color32,
) {
    ui.horizontal(|ui| {
        let dsp_color = if state.telem_load_pct < 50.0 {
            COL_VU_GREEN
        } else if state.telem_load_pct <= 80.0 {
            COL_AMBER
        } else {
            COL_VU_RED
        };
        let colors: [Option<egui::Color32>; 8] =
            [None, None, Some(dsp_color), None, None, None, None, None];

        let available_width = ui.available_width();
        let width_changed = (available_width - state.telem_cached_width).abs() > 1.0;
        let content_stale = state.telem_last_font_instant < state.last_telem_update;

        let calculated_font_size = match state.telem_cached_font_size {
            Some(sz) if !width_changed && !content_stale => sz,
            _ => {
                let baseline_s = 10.0;
                let baseline_font = egui::FontId::proportional(baseline_s);
                let mut sum_widths = 0.0;
                for text in &state.status_strings {
                    let galley = ui.painter().layout_no_wrap(
                        text.clone(),
                        baseline_font.clone(),
                        egui::Color32::WHITE,
                    );
                    sum_widths += galley.rect.width();
                }
                let separator_text = " | ".to_string();
                let sep_galley = ui.painter().layout_no_wrap(
                    separator_text,
                    baseline_font,
                    egui::Color32::WHITE,
                );
                sum_widths += sep_galley.rect.width() * (state.status_strings.len() - 1) as f32;

                let num_gaps = (state.status_strings.len() * 2 - 2) as f32;
                let total_gap_width = num_gaps * ui.spacing().item_spacing.x;

                let target_width = available_width - total_gap_width - 8.0 - 22.0;
                let sz = if sum_widths > 0.0 && target_width > 0.0 {
                    let scale = target_width / sum_widths;
                    (baseline_s * scale).clamp(6.0, 14.0)
                } else {
                    8.5
                };
                state.telem_cached_font_size = Some(sz);
                state.telem_cached_width = available_width;
                state.telem_last_font_instant = Instant::now();
                sz
            }
        };
        let font_size = egui::FontId::proportional(calculated_font_size);

        let mut first = true;
        for (i, text) in state.status_strings.iter().enumerate() {
            if !first {
                ui.label(
                    egui::RichText::new("|")
                        .font(font_size.clone())
                        .color(COL_BORDER),
                );
            }
            first = false;

            let color = colors[i].unwrap_or(COL_MUTED);
            let label = ui.label(
                egui::RichText::new(text.as_str())
                    .font(font_size.clone())
                    .color(color),
            );
            label.on_hover_text(state.status_tooltips[i]);
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let info_btn = ui.add(
                egui::Button::new(
                    egui::RichText::new("ℹ")
                        .font(egui::FontId::proportional(11.0))
                        .color(accent_color)
                        .strong(),
                )
                .frame(false),
            );
            let info_btn =
                info_btn.on_hover_text("Copy Diagnostic info to clipboard and ~/.cache/nam-rs/");

            if info_btn.clicked() {
                // Synchronize global active model name and active sample rate
                if let (Ok(name), Ok(mut active_name)) = (
                    shared.cold.ui_model_name.lock(),
                    neural_amp_modeler_rs::common::diagnostics::ACTIVE_MODEL_NAME.write(),
                ) {
                    *active_name = name.clone();
                }
                neural_amp_modeler_rs::common::diagnostics::ACTIVE_SAMPLE_RATE.store(
                    shared.cold.sample_rate.load(Ordering::Relaxed),
                    Ordering::Relaxed,
                );

                // Capture diagnostic bundle
                let bundle =
                    neural_amp_modeler_rs::common::diagnostics::DiagnosticBundle::capture();
                let diagnostic_content = bundle.render();

                // (a) Copy to clipboard
                ui.ctx().copy_text(diagnostic_content.clone());

                // (b) Save to file
                if let Some(home_dir) = std::env::var_os("HOME") {
                    let mut cache_dir = std::path::PathBuf::from(home_dir);
                    cache_dir.push(".cache/nam-rs");
                    if std::fs::create_dir_all(&cache_dir).is_ok() {
                        #[cfg(unix)]
                        {
                            use std::fs::Permissions;
                            use std::os::unix::fs::PermissionsExt;
                            let _ =
                                std::fs::set_permissions(&cache_dir, Permissions::from_mode(0o700));
                        }
                        let unix_ts = std::time::SystemTime::now()
                            .duration_since(std::time::SystemTime::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        let file_path = cache_dir.join(format!("diagnostic-{unix_ts}.txt"));

                        use std::fs::OpenOptions;
                        let mut options = OpenOptions::new();
                        options.write(true).create(true).truncate(true);
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::OpenOptionsExt;
                            options.mode(0o600);
                        }
                        if let Ok(mut file) = options.open(&file_path) {
                            use std::io::Write;
                            let _ = file.write_all(diagnostic_content.as_bytes());
                        }
                    }
                }

                // (c) Trigger toast notification
                state.toast_expiration = Some(Instant::now() + Duration::from_secs(3));
            }
        });
    });
}
