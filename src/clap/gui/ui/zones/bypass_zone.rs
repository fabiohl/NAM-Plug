// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
//! Zone 4 (far right): Bypass toggle with status LED.

use crate::clap::plugin::NamClapShared;
use clack_plugin::host::HostSharedHandle;
use std::sync::atomic::Ordering;

use crate::clap::gui::ui::{bypass::handle_bypass, colors::resolve_color};

pub(crate) fn draw_zone4_bypass(
    ui: &mut egui::Ui,
    shared: &NamClapShared,
    host: &HostSharedHandle,
    accent_color: egui::Color32,
) {
    ui.allocate_ui(egui::vec2(60.0, 205.0), |ui| {
        ui.vertical(|ui| {
            ui.add_space(18.0);
            let ind_bypass = shared.cold.param_indication
                [crate::clap::extensions::params::PARAM_BYPASS as usize]
                .load(Ordering::Relaxed);
            let ind_bypass_color = resolve_color(
                shared.cold.param_indication_color
                    [crate::clap::extensions::params::PARAM_BYPASS as usize]
                    .load(Ordering::Relaxed),
                egui::Color32::from_rgb(94, 129, 172),
            );
            let bypass_id = ui.make_persistent_id("bypass_switch");
            handle_bypass(
                ui,
                bypass_id,
                &shared.ui_to_rt.param_bypass,
                &shared.ui_to_rt.gesture_flags,
                &shared.ui_to_rt.gui_param_generation,
                crate::clap::extensions::params::PARAM_BYPASS as usize,
                accent_color,
                host,
                ind_bypass,
                ind_bypass_color,
            );
        });
    });
}
