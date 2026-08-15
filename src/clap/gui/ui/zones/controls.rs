// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
//! Zone 2 (center): Controls — Input Gain, Output Gain, Gate Threshold knobs,
//! and Oversampling segmented control.

use crate::clap::plugin::NamClapShared;
use clack_extensions::params::HostParams;
use clack_plugin::host::HostSharedHandle;
use std::sync::atomic::Ordering;

use crate::clap::gui::ui::{
    colors::{COL_AMBER, resolve_color},
    knob::handle_knob,
};

fn dispatch_discrete_param_change(
    shared: &NamClapShared,
    host: &HostSharedHandle,
    param_storage: &std::sync::atomic::AtomicU32,
    param_idx: u32,
    new_val: u32,
) {
    param_storage.store(new_val, Ordering::Relaxed);
    shared.set_gesture(param_idx as usize, 1);
    shared.set_gesture(param_idx as usize, 0);
    shared.set_gesture(param_idx as usize, 2);
    shared.bump_generation();
    if let Some(params_ext) = host.get_extension::<HostParams>() {
        params_ext.request_flush(host);
    }
}

pub(crate) fn draw_zone2_controls(
    ui: &mut egui::Ui,
    shared: &NamClapShared,
    host: &HostSharedHandle,
    current_bypass: bool,
    accent_color: egui::Color32,
) -> (Option<egui::Id>, Option<egui::Id>) {
    let mut oversample_id: Option<egui::Id> = None;
    let mut activation_id: Option<egui::Id> = None;
    ui.allocate_ui(egui::vec2(240.0, 230.0), |ui| {
        if current_bypass {
            ui.disable();
        }
        ui.vertical(|ui| {
            ui.add_space(12.0);
            let ind_input = shared.cold.param_indication
                [crate::clap::extensions::params::PARAM_INPUT_GAIN as usize]
                .load(Ordering::Relaxed);
            let ind_input_color = resolve_color(
                shared.cold.param_indication_color
                    [crate::clap::extensions::params::PARAM_INPUT_GAIN as usize]
                    .load(Ordering::Relaxed),
                egui::Color32::from_rgb(94, 129, 172),
            );

            let ind_output = shared.cold.param_indication
                [crate::clap::extensions::params::PARAM_OUTPUT_GAIN as usize]
                .load(Ordering::Relaxed);
            let ind_output_color = resolve_color(
                shared.cold.param_indication_color
                    [crate::clap::extensions::params::PARAM_OUTPUT_GAIN as usize]
                    .load(Ordering::Relaxed),
                egui::Color32::from_rgb(94, 129, 172),
            );

            let ind_gate = shared.cold.param_indication
                [crate::clap::extensions::params::PARAM_GATE_THRESH as usize]
                .load(Ordering::Relaxed);
            let ind_gate_color = resolve_color(
                shared.cold.param_indication_color
                    [crate::clap::extensions::params::PARAM_GATE_THRESH as usize]
                    .load(Ordering::Relaxed),
                egui::Color32::from_rgb(94, 129, 172),
            );

            ui.horizontal(|ui| {
                ui.allocate_ui(egui::vec2(78.0, 125.0), |ui| {
                    handle_knob(
                        ui,
                        ui.make_persistent_id("input_gain_knob"),
                        "INPUT",
                        neural_amp_modeler_rs::math::constants::GAIN_MIN_DB
                            ..=neural_amp_modeler_rs::math::constants::GAIN_MAX_DB,
                        0.0,
                        &shared.ui_to_rt.param_input_gain,
                        &shared.ui_to_rt.gesture_flags,
                        &shared.ui_to_rt.gui_param_generation,
                        crate::clap::extensions::params::PARAM_INPUT_GAIN as usize,
                        accent_color,
                        accent_color,
                        host,
                        egui::vec2(70.0, 70.0),
                        ind_input,
                        ind_input_color,
                        " dB",
                    );
                });
                ui.add_space(2.0);
                ui.allocate_ui(egui::vec2(78.0, 125.0), |ui| {
                    handle_knob(
                        ui,
                        ui.make_persistent_id("output_gain_knob"),
                        "OUTPUT",
                        neural_amp_modeler_rs::math::constants::GAIN_MIN_DB
                            ..=neural_amp_modeler_rs::math::constants::GAIN_MAX_DB,
                        0.0,
                        &shared.ui_to_rt.param_output_gain,
                        &shared.ui_to_rt.gesture_flags,
                        &shared.ui_to_rt.gui_param_generation,
                        crate::clap::extensions::params::PARAM_OUTPUT_GAIN as usize,
                        accent_color,
                        accent_color,
                        host,
                        egui::vec2(70.0, 70.0),
                        ind_output,
                        ind_output_color,
                        " dB",
                    );
                });
                ui.add_space(2.0);
                ui.allocate_ui(egui::vec2(70.0, 125.0), |ui| {
                    handle_knob(
                        ui,
                        ui.make_persistent_id("gate_thresh_knob"),
                        "GATE",
                        -90.0..=-40.0,
                        -70.0,
                        &shared.ui_to_rt.param_gate_thresh,
                        &shared.ui_to_rt.gesture_flags,
                        &shared.ui_to_rt.gui_param_generation,
                        crate::clap::extensions::params::PARAM_GATE_THRESH as usize,
                        COL_AMBER,
                        accent_color,
                        host,
                        egui::vec2(42.0, 42.0),
                        ind_gate,
                        ind_gate_color,
                        " dB (Threshold)",
                    );
                });
            });

            // Oversampling segmented control
            ui.add_space(4.0);
            let ind_os = shared.cold.param_indication
                [crate::clap::extensions::params::PARAM_OVERSAMPLE as usize]
                .load(Ordering::Relaxed);
            let ind_os_color = resolve_color(
                shared.cold.param_indication_color
                    [crate::clap::extensions::params::PARAM_OVERSAMPLE as usize]
                    .load(Ordering::Relaxed),
                accent_color,
            );
            let os_val = shared.ui_to_rt.param_oversample.load(Ordering::Relaxed);
            let os_val_i32 = os_val as i32;

            ui.allocate_ui(egui::vec2(210.0, 26.0), |ui| {
                let os_id = ui.make_persistent_id("oversample_control");
                oversample_id = Some(os_id);
                ui.memory_mut(|mem| mem.interested_in_focus(os_id, ui.layer_id()));

                let os_label = format!(
                    "Oversampling: currently {}",
                    match os_val_i32 {
                        0 => "Off",
                        1 => "2×",
                        _ => "4×",
                    }
                );
                let os_label_clone = os_label.clone();
                ui.ctx()
                    .register_widget_info(os_id, move || egui::WidgetInfo {
                        typ: egui::WidgetType::RadioGroup,
                        enabled: true,
                        label: Some(os_label_clone.clone()),
                        current_text_value: None,
                        prev_text_value: None,
                        hint_text: None,
                        text_selection: None,
                        selected: None,
                        value: None,
                    });
                ui.horizontal_centered(|ui| {
                    ui.label(
                        egui::RichText::new("Oversampling")
                            .font(egui::FontId::proportional(10.0))
                            .color(egui::Color32::GRAY),
                    );
                    let os_options = [(0, "Off"), (1, "2×"), (2, "4×")];
                    for (val, label) in os_options {
                        let resp = ui.selectable_value(
                            &mut (os_val_i32 == val),
                            true,
                            egui::RichText::new(label).font(egui::FontId::proportional(11.0)),
                        );
                        if resp.clicked() && os_val_i32 != val {
                            dispatch_discrete_param_change(
                                shared,
                                host,
                                &shared.ui_to_rt.param_oversample,
                                crate::clap::extensions::params::PARAM_OVERSAMPLE,
                                val as u32,
                            );
                        }
                    }
                });
                if ind_os & 1 != 0 {
                    let painter = ui.painter();
                    let rect = ui.min_rect();
                    let dots = [
                        egui::pos2(rect.left() + 4.0, rect.top() + 4.0),
                        egui::pos2(rect.right() - 4.0, rect.top() + 4.0),
                        egui::pos2(rect.left() + 4.0, rect.bottom() - 4.0),
                        egui::pos2(rect.right() - 4.0, rect.bottom() - 4.0),
                    ];
                    for dot in dots {
                        painter.circle_filled(dot, 1.5, ind_os_color);
                    }
                }
            });

            // Activation Precision segmented control
            ui.add_space(4.0);
            let ind_act = shared.cold.param_indication
                [crate::clap::extensions::params::PARAM_ACTIVATION as usize]
                .load(Ordering::Relaxed);
            let ind_act_color = resolve_color(
                shared.cold.param_indication_color
                    [crate::clap::extensions::params::PARAM_ACTIVATION as usize]
                    .load(Ordering::Relaxed),
                accent_color,
            );
            let act_val = shared.ui_to_rt.param_activation.load(Ordering::Relaxed);
            let act_val_i32 = act_val as i32;

            ui.allocate_ui(egui::vec2(210.0, 26.0), |ui| {
                let act_id = ui.make_persistent_id("activation_control");
                activation_id = Some(act_id);
                ui.memory_mut(|mem| mem.interested_in_focus(act_id, ui.layer_id()));

                let act_label = format!(
                    "Activation precision: currently {}",
                    if act_val_i32 == 1 { "Standard" } else { "Fast" }
                );
                let act_label_clone = act_label.clone();
                ui.ctx()
                    .register_widget_info(act_id, move || egui::WidgetInfo {
                        typ: egui::WidgetType::RadioGroup,
                        enabled: true,
                        label: Some(act_label_clone.clone()),
                        current_text_value: None,
                        prev_text_value: None,
                        hint_text: None,
                        text_selection: None,
                        selected: None,
                        value: None,
                    });
                ui.horizontal_centered(|ui| {
                    ui.label(
                        egui::RichText::new("Activation")
                            .font(egui::FontId::proportional(10.0))
                            .color(egui::Color32::GRAY),
                    );
                    // Value 1 = Standard (exact-grade, universal default);
                    // Value 0 = Fast (Padé/minimax approximation, opt-in).
                    let act_options = [(1, "Standard"), (0, "Fast")];
                    for (val, label) in act_options {
                        let resp = ui.selectable_value(
                            &mut (act_val_i32 == val),
                            true,
                            egui::RichText::new(label).font(egui::FontId::proportional(11.0)),
                        );
                        if resp.clicked() && act_val_i32 != val {
                            dispatch_discrete_param_change(
                                shared,
                                host,
                                &shared.ui_to_rt.param_activation,
                                crate::clap::extensions::params::PARAM_ACTIVATION,
                                val as u32,
                            );
                        }
                    }
                });
                if ind_act & 1 != 0 {
                    let painter = ui.painter();
                    let rect = ui.min_rect();
                    let dots = [
                        egui::pos2(rect.left() + 4.0, rect.top() + 4.0),
                        egui::pos2(rect.right() - 4.0, rect.top() + 4.0),
                        egui::pos2(rect.left() + 4.0, rect.bottom() - 4.0),
                        egui::pos2(rect.right() - 4.0, rect.bottom() - 4.0),
                    ];
                    for dot in dots {
                        painter.circle_filled(dot, 1.5, ind_act_color);
                    }
                }
            });
        });
    });
    (oversample_id, activation_id)
}
