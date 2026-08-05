// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
//! Zone 3 (right): Adaptive VU meter(s) with peak hold and clipping LEDs.
//!
//! Renders 1 centered bar (mono, 76px) when `active_channel_count < 2`, or
//! 2 labeled bars L/R (36px each) when the host reports ≥2 channels.

use crate::clap::plugin::NamClapShared;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::clap::gui::ui::{meter::draw_vertical_meter, state::UiState};

pub(crate) fn draw_zone3_meters(
    ui: &mut egui::Ui,
    shared: &NamClapShared,
    state: &mut UiState,
    current_bypass: bool,
) {
    ui.allocate_ui(egui::vec2(80.0, 205.0), |ui| {
        if current_bypass {
            ui.disable();
        }
        ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
            ui.add_space(8.0);

            let peak_l = f32::from_bits(
                shared
                    .rt_to_ui
                    .ui_peak_l
                    .swap(0.0f32.to_bits(), Ordering::Relaxed),
            );
            let peak_r = f32::from_bits(
                shared
                    .rt_to_ui
                    .ui_peak_r
                    .swap(0.0f32.to_bits(), Ordering::Relaxed),
            );

            let is_stereo = shared.rt_to_ui.active_channel_count.load(Ordering::Relaxed) >= 2;

            ui.horizontal(|ui| {
                if is_stereo {
                    ui.allocate_ui(egui::vec2(36.0, 190.0), |ui| {
                        ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                            draw_vertical_meter(
                                ui,
                                peak_l,
                                &mut state.peak_l_hold,
                                &mut state.peak_l_hold_time,
                                &mut state.clip_l,
                                "L",
                                state.vu_program,
                                state.vu_vao,
                                &mut state.vu_l_state,
                                &mut state.vu_l_callback,
                                Arc::clone(&state.vu_uniforms),
                            );
                        });
                    });
                    ui.add_space(4.0);
                    ui.allocate_ui(egui::vec2(36.0, 190.0), |ui| {
                        ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                            draw_vertical_meter(
                                ui,
                                peak_r,
                                &mut state.peak_r_hold,
                                &mut state.peak_r_hold_time,
                                &mut state.clip_r,
                                "R",
                                state.vu_program,
                                state.vu_vao,
                                &mut state.vu_r_state,
                                &mut state.vu_r_callback,
                                Arc::clone(&state.vu_uniforms),
                            );
                        });
                    });
                } else {
                    ui.allocate_ui(egui::vec2(76.0, 190.0), |ui| {
                        ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                            draw_vertical_meter(
                                ui,
                                peak_l,
                                &mut state.peak_l_hold,
                                &mut state.peak_l_hold_time,
                                &mut state.clip_l,
                                "",
                                state.vu_program,
                                state.vu_vao,
                                &mut state.vu_l_state,
                                &mut state.vu_l_callback,
                                Arc::clone(&state.vu_uniforms),
                            );
                        });
                    });
                }
            });
        });
    });
}
