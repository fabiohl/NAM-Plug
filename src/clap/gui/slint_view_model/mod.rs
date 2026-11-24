// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Slint View-Model: connects Slint UI callbacks and telemetry to `GuiSharedState`
//! and CLAP host parameter flush.

// `slint::include_modules!` materializes machine-generated UI code whose own
// `unwrap()` calls are out of scope for the hand-written production unwrap
// hygiene enforced crate-wide. Hand-written `expect()` in this file
// remains subject to the `expect_used` lint.
#![expect(clippy::unwrap_used)]

mod callbacks;
mod formatting;
mod telemetry;

pub use formatting::{
    METER_MAX_DB, METER_MIN_DB, StatusBarCache, StatusTextSink, amp_to_meter_frac,
};
pub use telemetry::{
    BALLISTICS_DECAY_RATE, PEAK_HOLD_FRAMES, TELEMETRY_LOW_CADENCE_IDLE, TELEMETRY_PERIOD_FULL,
    TELEMETRY_PERIOD_LOW,
};

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use neural_amp_modeler_rs::math::common::SIMD_MATH;
use slint::ComponentHandle;

use callbacks::bind_all_callbacks;
use formatting::{
    extract_cal_flags, format_buffer_size_text, format_cal_dbu_text, format_channels_text,
    format_gain_text, format_gate_text, format_latency_text, format_model_arch,
    format_oversample_text, format_sample_rate_text, simd_badge_text,
};
use telemetry::{SlintViewModelInner, apply_telemetry_active, arm_telemetry_timer};

use crate::clap::extensions::cabinet::{CabAssetKind, disambiguated_cab_label};
use crate::clap::extensions::calibration::{OutputMode, sanitize_calibration_level_dbu};
use crate::clap::gui::GuiHostBridge;
use crate::clap::plugin::GuiSharedState;

slint::include_modules!();

/// View-Model coordinating Slint GUI events, parameter gestures, and telemetry synchronization.
pub struct SlintViewModel {
    inner: Rc<RefCell<SlintViewModelInner>>,
}

impl SlintViewModel {
    /// Creates a new `SlintViewModel`, binding all Slint callbacks to `GuiSharedState`
    /// and starting the 60 Hz telemetry polling timer (suspended when the
    /// window is hidden, adaptive to 30 Hz in the silence state).
    pub fn new(
        window: MainWindow,
        shared: Arc<GuiSharedState>,
        host: Option<GuiHostBridge>,
        backend_text: &str,
    ) -> Self {
        log::info!("SlintViewModel: Initializing Slint UI bindings and telemetry loop");

        shared.note_ui_interaction();

        // 1. Static window-lifetime identity badges (backend and host SIMD ISA).
        window.set_backend_text(backend_text.into());
        window.set_simd_badge(simd_badge_text(SIMD_MATH.instruction_set).into());

        // 2. Initial amp model metadata and calibration availability flags.
        let initial_model_name = shared
            .cold
            .ui_model_name
            .try_lock()
            .ok()
            .and_then(|guard| {
                if guard.is_empty() {
                    None
                } else {
                    Some(guard.clone())
                }
            })
            .unwrap_or_else(|| "No model loaded".to_string());
        let initial_has_model = initial_model_name != "No model loaded";
        window.set_has_model(initial_has_model);
        window.set_model_name(initial_model_name.as_str().into());

        let (initial_model_arch, initial_cal_flags) = shared
            .cold
            .ui_model_metadata
            .try_lock()
            .ok()
            .map(|guard| {
                let arch = guard
                    .as_ref()
                    .map(format_model_arch)
                    .unwrap_or_else(|| "No model loaded".to_string());
                let flags = extract_cal_flags(guard.as_ref());
                (arch, flags)
            })
            .unwrap_or_else(|| ("No model loaded".to_string(), (false, false, false)));
        window.set_model_arch(initial_model_arch.clone().into());
        window.set_has_input_cal_meta(initial_cal_flags.0);
        window.set_has_output_cal_meta(initial_cal_flags.1);
        window.set_has_loudness_meta(initial_cal_flags.2);

        // 3. Initial cabinet slot identity (WAV IR or 1x1 neural model), bypass, and IR policy.
        let initial_has_ir = shared
            .cold
            .ir_path
            .try_lock()
            .ok()
            .map(|guard| guard.is_some())
            .unwrap_or(false);
        let initial_cab_kind = shared.cold.cab_kind.load(Ordering::Relaxed);
        let initial_cab_meta = shared
            .cold
            .cab_ui_meta
            .try_lock()
            .ok()
            .and_then(|guard| guard.clone());
        let initial_ir_name = if let Some(ref meta) = initial_cab_meta {
            meta.disambiguated_name.clone()
        } else {
            shared
                .cold
                .ir_path
                .try_lock()
                .ok()
                .and_then(|guard| {
                    guard
                        .as_ref()
                        .map(|path| disambiguated_cab_label(std::path::Path::new(path)))
                })
                .unwrap_or_else(|| "No cabinet loaded".to_string())
        };
        let initial_cab_status = initial_cab_meta
            .as_ref()
            .map(|m| m.status_summary.clone())
            .unwrap_or_else(|| "—".to_string());
        let initial_cab_detail = initial_cab_meta
            .as_ref()
            .and_then(|m| m.gear_summary.clone())
            .unwrap_or_default();
        if let Some(kind) = CabAssetKind::from_u8(initial_cab_kind) {
            window.set_cab_kind_label(kind.badge_label().into());
            window.set_cab_is_neural(kind.is_neural());
        } else {
            window.set_cab_kind_label("CAB".into());
            window.set_cab_is_neural(false);
        }
        window.set_has_ir(initial_has_ir || initial_cab_meta.is_some());
        window.set_ir_name(initial_ir_name.as_str().into());
        window.set_ir_status(initial_cab_status.as_str().into());
        window.set_cab_detail(initial_cab_detail.as_str().into());
        let initial_cab_bypass = shared.ui_to_rt.param_cab_bypass.load(Ordering::Relaxed) != 0;
        window.set_cab_bypassed(initial_cab_bypass);
        let initial_ir_policy = crate::clap::extensions::ir_level_policy::IrLevelPolicy::from_u8(
            shared.cold.ir_level_policy.load(Ordering::Relaxed),
        );
        window.set_ir_policy_label(initial_ir_policy.display_label().into());

        // 4. Initial status-bar telemetry snapshot (sample rate, buffer, latency, channels).
        let initial_sr = shared.cold.sample_rate.load(Ordering::Relaxed);
        let initial_sr_text = if initial_sr > 0 {
            format_sample_rate_text(initial_sr)
        } else {
            "48 kHz".to_string()
        };
        window.set_sample_rate_text(initial_sr_text.into());

        let initial_buf = shared.cold.buffer_size.load(Ordering::Relaxed);
        window.set_buffer_size_text(format_buffer_size_text(initial_buf).into());

        let initial_total_lat = shared.cold.current_stream_latency.load(Ordering::Relaxed)
            + shared.cold.current_cabsim_latency.load(Ordering::Relaxed);
        window.set_latency_text(format_latency_text(initial_total_lat, initial_sr).into());

        let initial_channels = shared.rt_to_ui.active_channel_count.load(Ordering::Relaxed);
        window.set_channels_text(format_channels_text(initial_channels).into());
        let status_bar =
            StatusBarCache::new(initial_sr, initial_buf, initial_total_lat, initial_channels);

        // 5. Seed rotary knobs, selectors, and calibration toggles from shared atomics.
        let initial_in_gain =
            f32::from_bits(shared.ui_to_rt.param_input_gain.load(Ordering::Relaxed));
        let initial_out_gain =
            f32::from_bits(shared.ui_to_rt.param_output_gain.load(Ordering::Relaxed));
        let initial_gate_th =
            f32::from_bits(shared.ui_to_rt.param_gate_thresh.load(Ordering::Relaxed));
        let initial_gate_mode = shared.gate_mode().to_u32();
        let initial_bypass = shared.ui_to_rt.param_bypass.load(Ordering::Relaxed) != 0;
        let initial_os = shared.ui_to_rt.param_oversample.load(Ordering::Relaxed);
        let initial_act = shared.ui_to_rt.param_activation.load(Ordering::Relaxed);

        window.set_input_gain_db(initial_in_gain);
        window.set_input_gain_text(format_gain_text(initial_in_gain).into());
        window.set_output_gain_db(initial_out_gain);
        window.set_output_gain_text(format_gain_text(initial_out_gain).into());
        window.set_gate_thresh_db(initial_gate_th);
        window.set_gate_thresh_text(format_gate_text(initial_gate_th).into());
        window.set_gate_mode(initial_gate_mode as i32);
        window.set_bypass_active(initial_bypass);
        window.set_oversample_mode(initial_os as i32);
        window.set_oversample_text(format_oversample_text(initial_os).into());
        window.set_activation_mode(initial_act as i32);

        let initial_cal_in = shared
            .ui_to_rt
            .param_calibrate_input
            .load(Ordering::Relaxed)
            != 0;
        let initial_cal_level = sanitize_calibration_level_dbu(f32::from_bits(
            shared
                .ui_to_rt
                .param_input_calibration_level
                .load(Ordering::Relaxed),
        ));
        let initial_out_mode =
            OutputMode::from_u32(shared.ui_to_rt.param_output_mode.load(Ordering::Relaxed))
                .to_u32();
        window.set_calibrate_input(initial_cal_in);
        window.set_input_calibration_level_dbu(initial_cal_level);
        window.set_input_calibration_text(format_cal_dbu_text(initial_cal_level).into());
        window.set_output_mode(initial_out_mode as i32);

        // 6. Wire UI callbacks and start the 60 Hz adaptive telemetry polling loop.
        bind_all_callbacks(&window, &shared, host);

        let inner = Rc::new(RefCell::new(SlintViewModelInner {
            window,
            shared,
            _host: host,
            last_input_gain: initial_in_gain,
            last_output_gain: initial_out_gain,
            last_gate_thresh: initial_gate_th,
            last_gate_mode: initial_gate_mode,
            last_bypass: initial_bypass,
            last_cab_bypass: initial_cab_bypass,
            last_oversampling: initial_os,
            last_activation: initial_act,
            last_calibrate_input: initial_cal_in,
            last_input_cal_level: initial_cal_level,
            last_output_mode: initial_out_mode,
            last_cal_flags: initial_cal_flags,
            last_model_name: initial_model_name,
            last_model_arch: initial_model_arch,
            last_ir_path: if initial_has_ir || initial_cab_meta.is_some() {
                Some(initial_ir_name)
            } else {
                None
            },
            last_cab_kind: initial_cab_kind,
            last_cab_status: if initial_has_ir || initial_cab_meta.is_some() {
                initial_cab_status
            } else {
                String::new()
            },
            last_cab_detail: initial_cab_detail,
            last_ir_policy: initial_ir_policy,
            last_dsp_load_pct: None,
            last_seen_xrun_epoch: 0,
            xrun_hold_frames: 0,
            current_in_peak_l: 0.0,
            current_in_peak_r: 0.0,
            hold_in_peak_l: 0.0,
            hold_in_peak_r: 0.0,
            hold_in_timer_l: 0,
            hold_in_timer_r: 0,
            current_peak_l: 0.0,
            current_peak_r: 0.0,
            hold_peak_l: 0.0,
            hold_peak_r: 0.0,
            hold_timer_l: 0,
            hold_timer_r: 0,
            status_bar,
            timer: slint::Timer::default(),
            telemetry_active: true,
            cadence: TELEMETRY_PERIOD_FULL,
        }));

        {
            let weak_inner = Rc::downgrade(&inner);
            inner
                .borrow()
                .window
                .on_set_telemetry_active(move |active: bool| {
                    if let Some(inner) = weak_inner.upgrade() {
                        apply_telemetry_active(&inner, active);
                    }
                });
        }
        arm_telemetry_timer(&inner, TELEMETRY_PERIOD_FULL);

        Self { inner }
    }

    /// Explicitly ticks telemetry once (useful for tests and manual render loops).
    pub fn tick_telemetry(&mut self) {
        let desired = self.inner.borrow_mut().tick_telemetry();
        if let Some(period) = desired
            && self.inner.borrow().telemetry_active
        {
            arm_telemetry_timer(&self.inner, period);
        }
    }

    /// Suspends (hidden window) or resumes the telemetry timer.
    pub fn set_telemetry_active(&mut self, active: bool) {
        apply_telemetry_active(&self.inner, active);
    }

    /// Whether the telemetry timer is currently scheduled.
    pub fn is_telemetry_running(&self) -> bool {
        self.inner.borrow().timer.running()
    }

    /// The period the telemetry timer runs at while active.
    pub fn telemetry_period(&self) -> std::time::Duration {
        self.inner.borrow().cadence
    }

    /// Returns a strong clone of the Slint window handle.
    pub fn window(&self) -> MainWindow {
        self.inner.borrow().window.clone_strong()
    }
}
