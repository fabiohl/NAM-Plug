// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
//! Keyboard focus navigation: Tab/Shift+Tab cycling through UI controls.

pub(crate) fn handle_focus_navigation(
    ui: &mut egui::Ui,
    load_btn_id: Option<egui::Id>,
    load_ir_btn_id: Option<egui::Id>,
    clear_ir_btn_id: Option<egui::Id>,
    oversample_id: Option<egui::Id>,
    activation_id: Option<egui::Id>,
) {
    let load_btn_id = load_btn_id.unwrap_or_else(|| ui.make_persistent_id("load_model_button"));
    let load_ir_btn_id = load_ir_btn_id.unwrap_or_else(|| ui.make_persistent_id("load_ir_button"));
    let clear_ir_btn_id =
        clear_ir_btn_id.unwrap_or_else(|| ui.make_persistent_id("clear_ir_button"));
    let os_id = oversample_id.unwrap_or_else(|| ui.make_persistent_id("oversample_control"));
    let act_id = activation_id.unwrap_or_else(|| ui.make_persistent_id("activation_control"));

    let controls = [
        ui.make_persistent_id("input_gain_knob"),
        ui.make_persistent_id("output_gain_knob"),
        ui.make_persistent_id("gate_thresh_knob"),
        os_id,
        act_id,
        ui.make_persistent_id("bypass_switch"),
        load_btn_id,
        load_ir_btn_id,
        clear_ir_btn_id,
    ];
    let num_controls = controls.len();

    let mut tab_pressed = false;
    let mut shift_pressed = false;
    ui.input_mut(|i| {
        i.events.retain(|e| {
            if let egui::Event::Key {
                key: egui::Key::Tab,
                pressed: true,
                modifiers,
                ..
            } = e
            {
                tab_pressed = true;
                shift_pressed = modifiers.shift;
                false
            } else {
                true
            }
        });
    });

    if tab_pressed {
        let focused = ui.memory(|mem| mem.focused());
        let next_focus = if let Some(curr) = focused {
            if let Some(idx) = controls.iter().position(|&id| id == curr) {
                if shift_pressed {
                    if idx == 0 {
                        controls[num_controls - 1]
                    } else {
                        controls[idx - 1]
                    }
                } else {
                    if idx == num_controls - 1 {
                        controls[0]
                    } else {
                        controls[idx + 1]
                    }
                }
            } else if shift_pressed {
                controls[num_controls - 1]
            } else {
                controls[0]
            }
        } else if shift_pressed {
            controls[num_controls - 1]
        } else {
            controls[0]
        };
        ui.memory_mut(|mem| mem.request_focus(next_focus));
    }
}
