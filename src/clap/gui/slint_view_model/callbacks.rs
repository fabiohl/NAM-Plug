// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Slint UI callback bindings and host parameter flush helpers.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use clack_extensions::params::HostParams;
use slint::ComponentHandle;

use super::MainWindow;
use super::formatting::{format_cal_dbu_text, format_oversample_text};
use crate::clap::extensions::cabinet::CabAssetKind;
use crate::clap::extensions::calibration::{OutputMode, sanitize_calibration_level_dbu};
use crate::clap::extensions::gate_mode::GateMode;
use crate::clap::extensions::params::{
    PARAM_ACTIVATION, PARAM_BYPASS, PARAM_CAB_BYPASS, PARAM_CALIBRATE_INPUT, PARAM_GATE_MODE,
    PARAM_GATE_THRESH, PARAM_INPUT_CALIBRATION_LEVEL, PARAM_INPUT_GAIN, PARAM_OUTPUT_GAIN,
    PARAM_OUTPUT_MODE, PARAM_OVERSAMPLE,
};
use crate::clap::gui::GuiHostBridge;
use crate::clap::gui::file_dialogs::{spawn_file_dialog, spawn_ir_file_dialog};
use crate::clap::plugin::GuiSharedState;
use crate::clap::plugin::lock::lock_cold;

#[inline]
fn commit_gui_param(host: Option<&GuiHostBridge>, shared: &GuiSharedState, param_id: u32) {
    shared.note_ui_interaction();
    shared.begin_gesture(param_id);
    shared.mark_param_changed(param_id);
    shared.end_gesture(param_id);
    shared.bump_generation();
    flush_params(host, shared);
}

#[inline]
fn flush_params(host: Option<&GuiHostBridge>, shared: &GuiSharedState) {
    if let Some(bridge) = host {
        shared.cold.host_call_quiescence.host_call(|| {
            // SAFETY: the handle is created and used only inside `host_call`,
            // which refuses entry once `alive_fence` is lowered at teardown.
            let host_static = unsafe { bridge.as_static() };
            if let Some(params_ext) = host_static.get_extension::<HostParams>() {
                params_ext.request_flush(&host_static);
            }
        });
    }
}

#[inline]
fn request_host_callback(host: Option<GuiHostBridge>, shared: &GuiSharedState) {
    if let Some(bridge) = host {
        shared.cold.host_call_quiescence.host_call(|| {
            // SAFETY: created inside `host_call`; see `flush_params`.
            unsafe { bridge.as_static() }.request_callback();
        });
    }
}

pub(super) fn bind_all_callbacks(
    window: &MainWindow,
    shared: &Arc<GuiSharedState>,
    host: Option<GuiHostBridge>,
) {
    bind_gain_and_gate_callbacks(window, shared, host);
    bind_mode_and_switch_callbacks(window, shared, host);
    bind_model_and_ir_callbacks(window, shared, host);
    bind_calibration_callbacks(window, shared, host);
}

fn bind_gain_and_gate_callbacks(
    window: &MainWindow,
    shared: &Arc<GuiSharedState>,
    host: Option<GuiHostBridge>,
) {
    {
        let shared = Arc::clone(shared);
        window.on_input_gain_gesture_begin(move || {
            shared.begin_gesture(PARAM_INPUT_GAIN);
            flush_params(host.as_ref(), &shared);
        });
    }
    {
        let shared = Arc::clone(shared);
        window.on_input_gain_changed(move |val| {
            shared
                .ui_to_rt
                .param_input_gain
                .store(val.to_bits(), Ordering::Relaxed);
            shared.bump_generation();
            shared.mark_param_changed(PARAM_INPUT_GAIN);
            flush_params(host.as_ref(), &shared);
        });
    }
    {
        let shared = Arc::clone(shared);
        window.on_input_gain_gesture_end(move || {
            shared.end_gesture(PARAM_INPUT_GAIN);
            flush_params(host.as_ref(), &shared);
        });
    }

    {
        let shared = Arc::clone(shared);
        window.on_output_gain_gesture_begin(move || {
            shared.begin_gesture(PARAM_OUTPUT_GAIN);
            flush_params(host.as_ref(), &shared);
        });
    }
    {
        let shared = Arc::clone(shared);
        window.on_output_gain_changed(move |val| {
            shared
                .ui_to_rt
                .param_output_gain
                .store(val.to_bits(), Ordering::Relaxed);
            shared.bump_generation();
            shared.mark_param_changed(PARAM_OUTPUT_GAIN);
            flush_params(host.as_ref(), &shared);
        });
    }
    {
        let shared = Arc::clone(shared);
        window.on_output_gain_gesture_end(move || {
            shared.end_gesture(PARAM_OUTPUT_GAIN);
            flush_params(host.as_ref(), &shared);
        });
    }

    {
        let shared = Arc::clone(shared);
        window.on_gate_thresh_gesture_begin(move || {
            shared.begin_gesture(PARAM_GATE_THRESH);
            flush_params(host.as_ref(), &shared);
        });
    }
    {
        let shared = Arc::clone(shared);
        window.on_gate_thresh_changed(move |val| {
            shared
                .ui_to_rt
                .param_gate_thresh
                .store(val.to_bits(), Ordering::Relaxed);
            shared.bump_generation();
            shared.mark_param_changed(PARAM_GATE_THRESH);
            flush_params(host.as_ref(), &shared);
        });
    }
    {
        let shared = Arc::clone(shared);
        window.on_gate_thresh_gesture_end(move || {
            shared.end_gesture(PARAM_GATE_THRESH);
            flush_params(host.as_ref(), &shared);
        });
    }

    {
        let shared = Arc::clone(shared);
        window.on_gate_mode_changed(move |val| {
            let mode = GateMode::from_u32(val.max(0) as u32);
            shared.store_gate_mode(mode);
            commit_gui_param(host.as_ref(), &shared, PARAM_GATE_MODE);
        });
    }
}

fn bind_mode_and_switch_callbacks(
    window: &MainWindow,
    shared: &Arc<GuiSharedState>,
    host: Option<GuiHostBridge>,
) {
    {
        let shared = Arc::clone(shared);
        let window_weak = window.as_weak();
        window.on_toggle_bypass(move || {
            let prev = shared.ui_to_rt.param_bypass.load(Ordering::Relaxed) != 0;
            let next = !prev;
            shared
                .ui_to_rt
                .param_bypass
                .store(u32::from(next), Ordering::Relaxed);
            commit_gui_param(host.as_ref(), &shared, PARAM_BYPASS);
            if let Some(w) = window_weak.upgrade() {
                w.set_bypass_active(next);
            }
        });
    }

    {
        let shared = Arc::clone(shared);
        window.on_oversample_changed(move |val| {
            let mode = (val as u32).min(2);
            shared
                .ui_to_rt
                .param_oversample
                .store(mode, Ordering::Relaxed);
            commit_gui_param(host.as_ref(), &shared, PARAM_OVERSAMPLE);
        });
    }
    {
        let shared = Arc::clone(shared);
        let window_weak = window.as_weak();
        window.on_cycle_oversample(move || {
            let cur = shared.ui_to_rt.param_oversample.load(Ordering::Relaxed);
            let next = (cur + 1) % 3;
            shared
                .ui_to_rt
                .param_oversample
                .store(next, Ordering::Relaxed);
            commit_gui_param(host.as_ref(), &shared, PARAM_OVERSAMPLE);
            if let Some(w) = window_weak.upgrade() {
                w.set_oversample_mode(next as i32);
                w.set_oversample_text(format_oversample_text(next).into());
            }
        });
    }

    {
        let shared = Arc::clone(shared);
        window.on_activation_changed(move |val| {
            let mode = (val as u32).min(1);
            shared
                .ui_to_rt
                .param_activation
                .store(mode, Ordering::Relaxed);
            commit_gui_param(host.as_ref(), &shared, PARAM_ACTIVATION);
        });
    }

    {
        let shared = Arc::clone(shared);
        let window_weak = window.as_weak();
        window.on_reset_clip_clicked(move || {
            shared.note_ui_interaction();
            shared
                .rt_to_ui
                .ui_clip_indicator
                .store(false, Ordering::Relaxed);
            shared.rt_to_ui.ui_clipped.store(false, Ordering::Relaxed);
            if let Some(w) = window_weak.upgrade() {
                w.set_clip_active(false);
            }
        });
    }

    {
        let shared = Arc::clone(shared);
        let window_weak = window.as_weak();
        window.on_reset_in_clip_clicked(move || {
            shared.note_ui_interaction();
            shared
                .rt_to_ui
                .ui_in_clip_indicator
                .store(false, Ordering::Relaxed);
            shared
                .rt_to_ui
                .ui_in_clipped
                .store(false, Ordering::Relaxed);
            if let Some(w) = window_weak.upgrade() {
                w.set_in_clip_active(false);
            }
        });
    }
}

fn bind_model_and_ir_callbacks(
    window: &MainWindow,
    shared: &Arc<GuiSharedState>,
    host: Option<GuiHostBridge>,
) {
    {
        let shared = Arc::clone(shared);
        window.on_load_model_clicked(move || {
            if let Some(dialog_state_arc) = shared.cold.dialog_state.as_ref()
                && !shared.cold.ui_loading.load(Ordering::Relaxed)
                && !shared.cold.ui_dialog_open.load(Ordering::Relaxed)
                && !dialog_state_arc.active.load(Ordering::Relaxed)
            {
                log::info!("SlintViewModel: Triggering Model file picker (rfd / XDG Portal)");
                shared.note_ui_interaction();
                shared.cold.ui_dialog_open.store(true, Ordering::Relaxed);
                dialog_state_arc.active.store(true, Ordering::Relaxed);
                if let Some(bridge) = host {
                    let handle = spawn_file_dialog(
                        Arc::clone(dialog_state_arc),
                        bridge,
                        Arc::clone(&shared.cold.alive_fence),
                        Arc::clone(&shared.cold.host_call_quiescence),
                    );
                    *lock_cold(&shared.cold.dialog_handle_sink, "dialog_handle_sink") =
                        Some(handle);
                }
            }
        });
    }

    {
        let shared = Arc::clone(shared);
        let window_weak = window.as_weak();
        window.on_clear_model_clicked(move || {
            log::info!("SlintViewModel: Requesting NAM Amp Model clear");
            shared.note_ui_interaction();
            shared.cold.ui_clear_model.store(true, Ordering::Relaxed);
            request_host_callback(host, &shared);
            if let Some(w) = window_weak.upgrade() {
                w.set_has_model(false);
                w.set_model_name("No model loaded".into());
                w.set_model_arch("No model loaded".into());
                w.set_has_input_cal_meta(false);
                w.set_has_output_cal_meta(false);
                w.set_has_loudness_meta(false);
            }
        });
    }

    {
        let shared = Arc::clone(shared);
        let window_weak = window.as_weak();
        window.on_load_ir_clicked(move || {
            if let Some(ir_dialog_state_arc) = shared.cold.ir_dialog_state.as_ref()
                && !shared.cold.ui_ir_loading.load(Ordering::Relaxed)
                && !ir_dialog_state_arc.active.load(Ordering::Relaxed)
            {
                log::info!("SlintViewModel: Triggering IR file picker (rfd / XDG Portal)");
                shared.note_ui_interaction();
                shared.cold.ui_ir_loading.store(true, Ordering::Relaxed);
                ir_dialog_state_arc.active.store(true, Ordering::Relaxed);
                if let Some(w) = window_weak.upgrade() {
                    w.set_ir_loading(true);
                }
                if let Some(bridge) = host {
                    let handle = spawn_ir_file_dialog(
                        Arc::clone(ir_dialog_state_arc),
                        bridge,
                        Arc::clone(&shared.cold.alive_fence),
                        Arc::clone(&shared.cold.host_call_quiescence),
                    );
                    *lock_cold(&shared.cold.ir_dialog_handle_sink, "ir_dialog_handle_sink") =
                        Some(handle);
                }
            }
        });
    }

    {
        let shared = Arc::clone(shared);
        let window_weak = window.as_weak();
        window.on_clear_ir_clicked(move || {
            log::info!("SlintViewModel: Requesting IR CabSim clear");
            shared.note_ui_interaction();
            shared.cold.ui_clear_ir.store(true, Ordering::Relaxed);
            request_host_callback(host, &shared);
            if let Some(w) = window_weak.upgrade() {
                w.set_has_ir(false);
                w.set_ir_name("No cabinet loaded".into());
                w.set_ir_status("—".into());
                w.set_cab_detail("".into());
                w.set_cab_kind_label("CAB".into());
                w.set_cab_is_neural(false);
            }
        });
    }

    {
        let shared = Arc::clone(shared);
        let window_weak = window.as_weak();
        window.on_toggle_cab_bypass_clicked(move || {
            let prev = shared.ui_to_rt.param_cab_bypass.load(Ordering::Relaxed) != 0;
            let next = !prev;
            shared
                .ui_to_rt
                .param_cab_bypass
                .store(u32::from(next), Ordering::Relaxed);
            commit_gui_param(host.as_ref(), &shared, PARAM_CAB_BYPASS);
            if let Some(w) = window_weak.upgrade() {
                w.set_cab_bypassed(next);
            }
        });
    }

    {
        let shared = Arc::clone(shared);
        let window_weak = window.as_weak();
        window.on_cycle_ir_policy_clicked(move || {
            use crate::clap::extensions::ir_level_policy::IrLevelPolicy;
            if let Some(kind) = CabAssetKind::from_u8(shared.cold.cab_kind.load(Ordering::Relaxed))
                && kind.is_neural()
            {
                return;
            }
            shared.note_ui_interaction();
            let current =
                IrLevelPolicy::from_u8(shared.cold.ir_level_policy.load(Ordering::Relaxed));
            let next = current.next();
            shared
                .cold
                .ir_level_policy
                .store(next.to_u8(), Ordering::Relaxed);
            shared
                .cold
                .ui_ir_policy_rebuild_pending
                .store(true, Ordering::Release);
            request_host_callback(host, &shared);
            if let Some(w) = window_weak.upgrade() {
                w.set_ir_policy_label(next.display_label().into());
            }
            log::info!(
                "SlintViewModel: Cycled IR level policy to '{}'",
                next.as_str()
            );
        });
    }
}

fn bind_calibration_callbacks(
    window: &MainWindow,
    shared: &Arc<GuiSharedState>,
    host: Option<GuiHostBridge>,
) {
    {
        let shared = Arc::clone(shared);
        let window_weak = window.as_weak();
        window.on_calibrate_input_toggled(move || {
            let prev = shared
                .ui_to_rt
                .param_calibrate_input
                .load(Ordering::Relaxed)
                != 0;
            let active = !prev;
            shared
                .ui_to_rt
                .param_calibrate_input
                .store(u32::from(active), Ordering::Relaxed);
            commit_gui_param(host.as_ref(), &shared, PARAM_CALIBRATE_INPUT);
            if let Some(w) = window_weak.upgrade() {
                w.set_calibrate_input(active);
            }
        });
    }
    {
        let shared = Arc::clone(shared);
        let window_weak = window.as_weak();
        window.on_input_calibration_level_changed(move |val| {
            let sanitized = sanitize_calibration_level_dbu(val);
            shared
                .ui_to_rt
                .param_input_calibration_level
                .store(sanitized.to_bits(), Ordering::Relaxed);
            commit_gui_param(host.as_ref(), &shared, PARAM_INPUT_CALIBRATION_LEVEL);
            if let Some(w) = window_weak.upgrade() {
                w.set_input_calibration_level_dbu(sanitized);
                w.set_input_calibration_text(format_cal_dbu_text(sanitized).into());
            }
        });
    }
    {
        let shared = Arc::clone(shared);
        let window_weak = window.as_weak();
        window.on_output_mode_changed(move |val| {
            let mode = OutputMode::from_u32(val.max(0) as u32);
            shared
                .ui_to_rt
                .param_output_mode
                .store(mode.to_u32(), Ordering::Relaxed);
            commit_gui_param(host.as_ref(), &shared, PARAM_OUTPUT_MODE);
            if let Some(w) = window_weak.upgrade() {
                w.set_output_mode(mode.to_u32() as i32);
            }
        });
    }
}
