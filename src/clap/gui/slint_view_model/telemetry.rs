// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Telemetry polling, IEC 60268-10 ballistics, echo-loop parameter sync, and
//! adaptive cadence control for [`super::SlintViewModel`].

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::MainWindow;
use super::formatting::{
    StatusBarCache, amp_to_meter_frac, extract_cal_flags, format_cal_dbu_text, format_gain_text,
    format_gate_text, format_model_arch, format_oversample_text,
};
use crate::clap::extensions::cabinet::{CabAssetKind, disambiguated_cab_label};
use crate::clap::extensions::calibration::{OutputMode, sanitize_calibration_level_dbu};
use crate::clap::gui::GuiHostBridge;
use crate::clap::plugin::GuiSharedState;
use crate::clap::plugin::shared::{UI_NO_INTERACTION, ui_monotonic_ms};

/// Ballistics decay multiplier per 60 Hz frame (~20 dB drop in 1.7 s: 0.10^(1/102) ≈ 0.978,
/// normalized to ~0.962 for responsive analog VU needle release).
pub const BALLISTICS_DECAY_RATE: f32 = 0.962;

/// Peak hold duration in 60 Hz frames (120 frames = 2.0 seconds).
pub const PEAK_HOLD_FRAMES: u32 = 120;

/// Full-rate telemetry period (60 Hz) — the rate at which the IEC 60268-10
/// ballistics ([`BALLISTICS_DECAY_RATE`], [`PEAK_HOLD_FRAMES`]) are calibrated.
pub const TELEMETRY_PERIOD_FULL: std::time::Duration = std::time::Duration::from_millis(16);

/// Reduced telemetry period (~30 Hz), used only in the silence state.
pub const TELEMETRY_PERIOD_LOW: std::time::Duration = std::time::Duration::from_millis(33);

/// Minimum time without GUI interaction before the telemetry cadence may drop
/// to [`TELEMETRY_PERIOD_LOW`].
pub const TELEMETRY_LOW_CADENCE_IDLE: std::time::Duration = std::time::Duration::from_secs(2);

/// Silence threshold for the adaptive cadence: peaks below −90 dBFS map to a
/// 0.0 meter fraction.
const SILENCE_PEAK_AMP: f32 = 3.162_277_7e-5; // 10^(−90/20)

#[inline]
fn update_peak_ballistics(raw: f32, current: &mut f32, hold: &mut f32, timer: &mut u32) {
    if raw > *current {
        *current = raw;
    } else {
        *current = (*current * BALLISTICS_DECAY_RATE).max(raw);
    }
    if raw >= *hold {
        *hold = raw;
        *timer = PEAK_HOLD_FRAMES;
    } else if *timer > 0 {
        *timer -= 1;
    } else {
        *hold = (*hold * BALLISTICS_DECAY_RATE).max(raw);
    }
}

/// Drives the telemetry timer at the current cadence.
pub(super) fn arm_telemetry_timer(
    inner: &Rc<RefCell<SlintViewModelInner>>,
    period: std::time::Duration,
) {
    let weak = Rc::downgrade(inner);
    inner
        .borrow()
        .timer
        .start(slint::TimerMode::Repeated, period, move || {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let desired = inner.borrow_mut().tick_telemetry();
            if let Some(next_period) = desired
                && inner.borrow().telemetry_active
            {
                arm_telemetry_timer(&inner, next_period);
            }
        });
}

/// Suspends/resumes the telemetry timer.
pub(super) fn apply_telemetry_active(inner: &Rc<RefCell<SlintViewModelInner>>, active: bool) {
    let period = {
        let mut guard = inner.borrow_mut();
        guard.telemetry_active = active;
        if !active {
            guard.timer.stop();
            return;
        }
        guard.shared.note_ui_interaction();
        guard.cadence
    };
    arm_telemetry_timer(inner, period);
}

/// Internal mutable state for the Slint view model.
pub(crate) struct SlintViewModelInner {
    pub(super) window: MainWindow,
    pub(super) shared: Arc<GuiSharedState>,
    pub(super) _host: Option<GuiHostBridge>,

    // Parameter caches to prevent feedback loops and redundant UI sets
    pub(super) last_input_gain: f32,
    pub(super) last_output_gain: f32,
    pub(super) last_gate_thresh: f32,
    pub(super) last_gate_mode: u32,
    pub(super) last_bypass: bool,
    pub(super) last_cab_bypass: bool,
    pub(super) last_oversampling: u32,
    pub(super) last_activation: u32,
    pub(super) last_calibrate_input: bool,
    pub(super) last_input_cal_level: f32,
    pub(super) last_output_mode: u32,
    pub(super) last_cal_flags: (bool, bool, bool),
    pub(super) last_model_name: String,
    pub(super) last_model_arch: String,
    pub(super) last_ir_path: Option<String>,
    pub(super) last_cab_kind: u8,
    pub(super) last_cab_status: String,
    pub(super) last_cab_detail: String,
    pub(super) last_ir_policy: crate::clap::extensions::ir_level_policy::IrLevelPolicy,
    pub(super) last_dsp_load_pct: Option<u32>,
    pub(super) last_seen_xrun_epoch: u32,
    pub(super) xrun_hold_frames: u32,

    // Ballistics state
    pub(super) current_in_peak_l: f32,
    pub(super) current_in_peak_r: f32,
    pub(super) hold_in_peak_l: f32,
    pub(super) hold_in_peak_r: f32,
    pub(super) hold_in_timer_l: u32,
    pub(super) hold_in_timer_r: u32,

    pub(super) current_peak_l: f32,
    pub(super) current_peak_r: f32,
    pub(super) hold_peak_l: f32,
    pub(super) hold_peak_r: f32,
    pub(super) hold_timer_l: u32,
    pub(super) hold_timer_r: u32,

    // Status-bar text cache (F-UX-08): formats/publishes only on change.
    pub(super) status_bar: StatusBarCache,

    // Adaptive telemetry timer (F-UX-08): suspended while the window is hidden.
    pub(super) timer: slint::Timer,
    pub(super) telemetry_active: bool,
    pub(super) cadence: std::time::Duration,
}

impl SlintViewModelInner {
    /// Polls RT telemetry, updates IEC 60268-10 ballistics, and synchronizes external
    /// parameter/model/IR updates without echo loops.
    pub fn tick_telemetry(&mut self) -> Option<std::time::Duration> {
        let (raw_l, raw_r) = self.update_meters();
        let channels = self.update_indicators();
        self.sync_params_to_ui();
        self.sync_model_and_cab_status();

        // Status Bar Telemetry (cached — F-UX-08)
        let sr = self.shared.cold.sample_rate.load(Ordering::Relaxed);
        let buf = self.shared.cold.buffer_size.load(Ordering::Relaxed);
        let stream_lat = self
            .shared
            .cold
            .current_stream_latency
            .load(Ordering::Relaxed);
        let cab_lat = self
            .shared
            .cold
            .current_cabsim_latency
            .load(Ordering::Relaxed);
        self.status_bar
            .update(sr, buf, stream_lat + cab_lat, channels, &self.window);

        self.evaluate_alert_pill();
        self.evaluate_cadence(raw_l, raw_r)
    }

    fn update_meters(&mut self) -> (f32, f32) {
        let raw_in_l = f32::from_bits(
            self.shared
                .rt_to_ui
                .ui_in_peak_l
                .swap(0.0f32.to_bits(), Ordering::Relaxed),
        );
        let raw_in_r = f32::from_bits(
            self.shared
                .rt_to_ui
                .ui_in_peak_r
                .swap(0.0f32.to_bits(), Ordering::Relaxed),
        );
        update_peak_ballistics(
            raw_in_l,
            &mut self.current_in_peak_l,
            &mut self.hold_in_peak_l,
            &mut self.hold_in_timer_l,
        );
        update_peak_ballistics(
            raw_in_r,
            &mut self.current_in_peak_r,
            &mut self.hold_in_peak_r,
            &mut self.hold_in_timer_r,
        );

        let in_rms_l = f32::from_bits(self.shared.rt_to_ui.ui_in_rms_l.load(Ordering::Relaxed));
        let in_rms_r = f32::from_bits(self.shared.rt_to_ui.ui_in_rms_r.load(Ordering::Relaxed));
        let out_rms_l = f32::from_bits(self.shared.rt_to_ui.ui_out_rms_l.load(Ordering::Relaxed));
        let out_rms_r = f32::from_bits(self.shared.rt_to_ui.ui_out_rms_r.load(Ordering::Relaxed));

        self.window
            .set_in_peak_l(amp_to_meter_frac(self.current_in_peak_l));
        self.window
            .set_in_peak_r(amp_to_meter_frac(self.current_in_peak_r));
        self.window
            .set_in_peak_hold_l(amp_to_meter_frac(self.hold_in_peak_l));
        self.window
            .set_in_peak_hold_r(amp_to_meter_frac(self.hold_in_peak_r));
        self.window.set_in_rms_l(amp_to_meter_frac(in_rms_l));
        self.window.set_in_rms_r(amp_to_meter_frac(in_rms_r));

        let raw_l = f32::from_bits(
            self.shared
                .rt_to_ui
                .ui_peak_l
                .swap(0.0f32.to_bits(), Ordering::Relaxed),
        );
        let raw_r = f32::from_bits(
            self.shared
                .rt_to_ui
                .ui_peak_r
                .swap(0.0f32.to_bits(), Ordering::Relaxed),
        );
        update_peak_ballistics(
            raw_l,
            &mut self.current_peak_l,
            &mut self.hold_peak_l,
            &mut self.hold_timer_l,
        );
        update_peak_ballistics(
            raw_r,
            &mut self.current_peak_r,
            &mut self.hold_peak_r,
            &mut self.hold_timer_r,
        );

        self.window
            .set_peak_l(amp_to_meter_frac(self.current_peak_l));
        self.window
            .set_peak_r(amp_to_meter_frac(self.current_peak_r));
        self.window
            .set_peak_hold_l(amp_to_meter_frac(self.hold_peak_l));
        self.window
            .set_peak_hold_r(amp_to_meter_frac(self.hold_peak_r));
        self.window.set_rms_l(amp_to_meter_frac(out_rms_l));
        self.window.set_rms_r(amp_to_meter_frac(out_rms_r));
        (raw_l, raw_r)
    }

    fn update_indicators(&mut self) -> u32 {
        let in_clip = self
            .shared
            .rt_to_ui
            .ui_in_clip_indicator
            .load(Ordering::Relaxed)
            || self.shared.rt_to_ui.ui_in_clipped.load(Ordering::Relaxed);
        if in_clip != self.window.get_in_clip_active() {
            self.window.set_in_clip_active(in_clip);
        }

        let clip = self
            .shared
            .rt_to_ui
            .ui_clip_indicator
            .load(Ordering::Relaxed)
            || self.shared.rt_to_ui.ui_clipped.load(Ordering::Relaxed);
        if clip != self.window.get_clip_active() {
            self.window.set_clip_active(clip);
        }

        let gate = self.shared.rt_to_ui.ui_gate_active.load(Ordering::Relaxed);
        if gate != self.window.get_gate_active() {
            self.window.set_gate_active(gate);
        }

        let channels = self
            .shared
            .rt_to_ui
            .active_channel_count
            .load(Ordering::Relaxed);
        let is_stereo = channels >= 2;
        if is_stereo != self.window.get_is_stereo() {
            self.window.set_is_stereo(is_stereo);
        }

        let permille = self
            .shared
            .rt_to_ui
            .dsp_load_permille
            .load(Ordering::Relaxed);
        let pct = permille as u32 / 10;
        if self.last_dsp_load_pct != Some(pct) {
            self.last_dsp_load_pct = Some(pct);
            self.window.set_dsp_load_text(format!("{pct}% DSP").into());
        }

        let xrun_epoch = self.shared.rt_to_ui.xrun_epoch.load(Ordering::Relaxed);
        if xrun_epoch != self.last_seen_xrun_epoch {
            self.last_seen_xrun_epoch = xrun_epoch;
            self.xrun_hold_frames = PEAK_HOLD_FRAMES;
        }

        let xrun_active = self.xrun_hold_frames > 0;
        if xrun_active {
            self.xrun_hold_frames -= 1;
        }
        if xrun_active != self.window.get_xrun_active() {
            self.window.set_xrun_active(xrun_active);
        }
        channels
    }

    fn sync_params_to_ui(&mut self) {
        let in_gain = f32::from_bits(
            self.shared
                .ui_to_rt
                .param_input_gain
                .load(Ordering::Relaxed),
        );
        if (in_gain - self.window.get_input_gain_db()).abs() > 1e-4 {
            self.last_input_gain = in_gain;
            self.window.set_input_gain_db(in_gain);
            self.window
                .set_input_gain_text(format_gain_text(in_gain).into());
        }

        let out_gain = f32::from_bits(
            self.shared
                .ui_to_rt
                .param_output_gain
                .load(Ordering::Relaxed),
        );
        if (out_gain - self.window.get_output_gain_db()).abs() > 1e-4 {
            self.last_output_gain = out_gain;
            self.window.set_output_gain_db(out_gain);
            self.window
                .set_output_gain_text(format_gain_text(out_gain).into());
        }

        let gate_th = f32::from_bits(
            self.shared
                .ui_to_rt
                .param_gate_thresh
                .load(Ordering::Relaxed),
        );
        if (gate_th - self.window.get_gate_thresh_db()).abs() > 1e-4 {
            self.last_gate_thresh = gate_th;
            self.window.set_gate_thresh_db(gate_th);
            self.window
                .set_gate_thresh_text(format_gate_text(gate_th).into());
        }

        let gate_mode = self.shared.gate_mode().to_u32();
        if gate_mode != self.last_gate_mode || gate_mode as i32 != self.window.get_gate_mode() {
            self.last_gate_mode = gate_mode;
            self.window.set_gate_mode(gate_mode as i32);
        }

        let bypass = self.shared.ui_to_rt.param_bypass.load(Ordering::Relaxed) != 0;
        if bypass != self.window.get_bypass_active() {
            self.last_bypass = bypass;
            self.window.set_bypass_active(bypass);
        }

        let cab_bypass = self
            .shared
            .ui_to_rt
            .param_cab_bypass
            .load(Ordering::Relaxed)
            != 0;
        if cab_bypass != self.last_cab_bypass || cab_bypass != self.window.get_cab_bypassed() {
            self.last_cab_bypass = cab_bypass;
            self.window.set_cab_bypassed(cab_bypass);
        }

        let os = self
            .shared
            .ui_to_rt
            .param_oversample
            .load(Ordering::Relaxed);
        if os as i32 != self.window.get_oversample_mode() {
            self.last_oversampling = os;
            self.window.set_oversample_mode(os as i32);
            self.window
                .set_oversample_text(format_oversample_text(os).into());
        }

        let act = self
            .shared
            .ui_to_rt
            .param_activation
            .load(Ordering::Relaxed);
        if act as i32 != self.window.get_activation_mode() {
            self.last_activation = act;
            self.window.set_activation_mode(act as i32);
        }

        let cal_in = self
            .shared
            .ui_to_rt
            .param_calibrate_input
            .load(Ordering::Relaxed)
            != 0;
        if cal_in != self.last_calibrate_input || cal_in != self.window.get_calibrate_input() {
            self.last_calibrate_input = cal_in;
            self.window.set_calibrate_input(cal_in);
        }

        let cal_level = sanitize_calibration_level_dbu(f32::from_bits(
            self.shared
                .ui_to_rt
                .param_input_calibration_level
                .load(Ordering::Relaxed),
        ));
        if (cal_level - self.last_input_cal_level).abs() > f32::EPSILON
            || (cal_level - self.window.get_input_calibration_level_dbu()).abs() > f32::EPSILON
        {
            self.last_input_cal_level = cal_level;
            self.window.set_input_calibration_level_dbu(cal_level);
            self.window
                .set_input_calibration_text(format_cal_dbu_text(cal_level).into());
        }

        let out_mode = OutputMode::from_u32(
            self.shared
                .ui_to_rt
                .param_output_mode
                .load(Ordering::Relaxed),
        )
        .to_u32();
        if out_mode != self.last_output_mode || out_mode as i32 != self.window.get_output_mode() {
            self.last_output_mode = out_mode;
            self.window.set_output_mode(out_mode as i32);
        }
    }

    fn sync_model_and_cab_status(&mut self) {
        let model_loading = self.shared.cold.ui_loading.load(Ordering::Relaxed);
        if model_loading != self.window.get_model_loading() {
            self.window.set_model_loading(model_loading);
        }

        let model_error = self.shared.cold.ui_load_error.load(Ordering::Relaxed);
        if model_error != self.window.get_model_error() {
            self.window.set_model_error(model_error);
            if model_error {
                if let Ok(msg) = self.shared.cold.ui_load_error_msg.try_lock() {
                    self.window.set_model_error_msg(msg.as_str().into());
                }
            } else {
                self.window.set_model_error_msg("".into());
            }
        }

        if let Ok(name_guard) = self.shared.cold.ui_model_name.try_lock() {
            let has_model = !name_guard.is_empty();
            let current_name = if has_model {
                name_guard.as_str()
            } else {
                "No model loaded"
            };
            if self.window.get_has_model() != has_model {
                self.window.set_has_model(has_model);
            }
            if self.last_model_name != current_name {
                self.last_model_name = current_name.to_string();
                self.window.set_model_name(current_name.into());
            }
        }

        if let Ok(meta_guard) = self.shared.cold.ui_model_metadata.try_lock() {
            let current_arch = match meta_guard.as_ref() {
                Some(meta) => format_model_arch(meta),
                None => "No model loaded".to_string(),
            };
            if self.last_model_arch != current_arch {
                self.last_model_arch = current_arch.clone();
                self.window.set_model_arch(current_arch.into());
            }
            let cal_flags = extract_cal_flags(meta_guard.as_ref());
            if self.last_cal_flags != cal_flags {
                self.last_cal_flags = cal_flags;
                self.window.set_has_input_cal_meta(cal_flags.0);
                self.window.set_has_output_cal_meta(cal_flags.1);
                self.window.set_has_loudness_meta(cal_flags.2);
            }
        }

        let ir_loading = self.shared.cold.ui_ir_loading.load(Ordering::Relaxed);
        if ir_loading != self.window.get_ir_loading() {
            self.window.set_ir_loading(ir_loading);
        }

        let ir_error = self.shared.cold.ui_ir_load_error.load(Ordering::Relaxed);
        if ir_error != self.window.get_ir_error() {
            self.window.set_ir_error(ir_error);
            if ir_error {
                if let Ok(msg) = self.shared.cold.ui_ir_load_error_msg.try_lock() {
                    self.window.set_ir_error_msg(msg.as_str().into());
                }
            } else {
                self.window.set_ir_error_msg("".into());
            }
        }

        let cab_kind_raw = self.shared.cold.cab_kind.load(Ordering::Relaxed);
        if cab_kind_raw != self.last_cab_kind {
            self.last_cab_kind = cab_kind_raw;
            if let Some(kind) = CabAssetKind::from_u8(cab_kind_raw) {
                self.window.set_cab_kind_label(kind.badge_label().into());
                self.window.set_cab_is_neural(kind.is_neural());
            } else {
                self.window.set_cab_kind_label("CAB".into());
                self.window.set_cab_is_neural(false);
            }
        }

        if let Ok(meta_guard) = self.shared.cold.cab_ui_meta.try_lock() {
            if let Some(ref meta) = *meta_guard {
                if self.last_ir_path.as_deref() != Some(meta.disambiguated_name.as_str()) {
                    self.last_ir_path = Some(meta.disambiguated_name.clone());
                    self.window
                        .set_ir_name(meta.disambiguated_name.as_str().into());
                }
                if self.last_cab_status != meta.status_summary {
                    self.last_cab_status = meta.status_summary.clone();
                    self.window
                        .set_ir_status(meta.status_summary.as_str().into());
                }
                let detail = meta.gear_summary.as_deref().unwrap_or("");
                if self.last_cab_detail != detail {
                    self.last_cab_detail = detail.to_string();
                    self.window.set_cab_detail(detail.into());
                }
                if !self.window.get_has_ir() {
                    self.window.set_has_ir(true);
                }
            } else if let Ok(ir_guard) = self.shared.cold.ir_path.try_lock() {
                let has_ir = ir_guard.is_some();
                if has_ir != self.window.get_has_ir() {
                    self.window.set_has_ir(has_ir);
                }
                if let Some(ref path_str) = *ir_guard {
                    let label = disambiguated_cab_label(std::path::Path::new(path_str));
                    if self.last_ir_path.as_deref() != Some(label.as_str()) {
                        self.last_ir_path = Some(label.clone());
                        self.window.set_ir_name(label.as_str().into());
                    }
                    if let Some(kind) =
                        CabAssetKind::detect_from_path(std::path::Path::new(path_str))
                        && cab_kind_raw == 0
                    {
                        self.window.set_cab_kind_label(kind.badge_label().into());
                        self.window.set_cab_is_neural(kind.is_neural());
                    }
                } else {
                    if self.last_ir_path.is_some() {
                        self.last_ir_path = None;
                        self.window.set_ir_name("No cabinet loaded".into());
                    }
                    if !self.last_cab_status.is_empty() {
                        self.last_cab_status.clear();
                        self.window.set_ir_status("—".into());
                    }
                    if !self.last_cab_detail.is_empty() {
                        self.last_cab_detail.clear();
                        self.window.set_cab_detail("".into());
                    }
                }
            }
        }
        let ir_policy = crate::clap::extensions::ir_level_policy::IrLevelPolicy::from_u8(
            self.shared.cold.ir_level_policy.load(Ordering::Relaxed),
        );
        if ir_policy != self.last_ir_policy {
            self.last_ir_policy = ir_policy;
            self.window
                .set_ir_policy_label(ir_policy.display_label().into());
        }
    }

    fn evaluate_alert_pill(&self) {
        use crate::clap::plugin::shared::{
            UI_ALERT_CABSIM_CONTRACT, UI_ALERT_DEGRADE_MINIMAL, UI_ALERT_DEGRADE_REDUCED,
            UI_ALERT_HOST_CONTRACT, UI_ALERT_NON_FINITE_INPUT, UI_ALERT_POISONED,
            UI_ALERT_RESAMPLER_FAILED,
        };
        use neural_amp_modeler_rs::common::spsc::{
            RT_STATUS_CABSIM_CONTRACT_VIOLATION, RT_STATUS_DEGRADE_MINIMAL,
            RT_STATUS_DEGRADE_REDUCED, RT_STATUS_HOST_CONTRACT_VIOLATION,
            RT_STATUS_NON_FINITE_INPUT_DETECTED, RT_STATUS_PROCESSOR_POISONED,
            RT_STATUS_RESAMPLER_REBUILD_FAILED,
        };

        let rt = &self.shared.cold.rt_status;
        let mut effective_bits = self.shared.cold.ui_alert_bits.load(Ordering::Relaxed);
        let flag_map = [
            (RT_STATUS_PROCESSOR_POISONED, UI_ALERT_POISONED),
            (RT_STATUS_HOST_CONTRACT_VIOLATION, UI_ALERT_HOST_CONTRACT),
            (
                RT_STATUS_RESAMPLER_REBUILD_FAILED,
                UI_ALERT_RESAMPLER_FAILED,
            ),
            (
                RT_STATUS_CABSIM_CONTRACT_VIOLATION,
                UI_ALERT_CABSIM_CONTRACT,
            ),
            (RT_STATUS_DEGRADE_MINIMAL, UI_ALERT_DEGRADE_MINIMAL),
            (RT_STATUS_DEGRADE_REDUCED, UI_ALERT_DEGRADE_REDUCED),
            (
                RT_STATUS_NON_FINITE_INPUT_DETECTED,
                UI_ALERT_NON_FINITE_INPUT,
            ),
        ];
        for (rt_flag, ui_bit) in flag_map {
            if rt.check_flag(rt_flag) {
                effective_bits |= ui_bit;
            }
        }

        const RED: u32 = 0xFF_F7_4E_4E;
        const AMBER: u32 = 0xFF_EB_CB_8B;
        let alerts: [(u32, &'static str, u32); 7] = [
            (UI_ALERT_POISONED, "Plugin parado — reiniciando…", RED),
            (UI_ALERT_HOST_CONTRACT, "Bloco do host acima do limite", RED),
            (
                UI_ALERT_RESAMPLER_FAILED,
                "Falha ao reconstruir resampler",
                RED,
            ),
            (
                UI_ALERT_CABSIM_CONTRACT,
                "Violação de partição do CabSim",
                RED,
            ),
            (
                UI_ALERT_DEGRADE_MINIMAL,
                "Qualidade mínima: CPU alta",
                AMBER,
            ),
            (
                UI_ALERT_DEGRADE_REDUCED,
                "Qualidade reduzida: CPU alta",
                AMBER,
            ),
            (
                UI_ALERT_NON_FINITE_INPUT,
                "Entrada inválida (NaN) — reconvergindo",
                AMBER,
            ),
        ];
        let alert = alerts
            .iter()
            .find(|(bit, _, _)| (effective_bits & *bit) != 0)
            .map(|(_, text, argb)| (*text, slint::Color::from_argb_encoded(*argb)));

        match alert {
            Some((text, color)) => {
                if !self.window.get_alert_active() {
                    self.window.set_alert_active(true);
                }
                self.window.set_alert_text(text.into());
                self.window.set_alert_color(color);
            }
            None => {
                if self.window.get_alert_active() {
                    self.window.set_alert_active(false);
                    self.window.set_alert_text("".into());
                }
            }
        }
    }

    /// Chooses the telemetry cadence for the next interval (F-UX-08).
    fn evaluate_cadence(&mut self, raw_l: f32, raw_r: f32) -> Option<std::time::Duration> {
        let last_interaction = self
            .shared
            .cold
            .ui_last_interaction_ms
            .load(Ordering::Relaxed);
        let idle = last_interaction == UI_NO_INTERACTION
            || ui_monotonic_ms().saturating_sub(last_interaction)
                >= TELEMETRY_LOW_CADENCE_IDLE.as_millis() as u64;
        let silent = raw_l < SILENCE_PEAK_AMP
            && raw_r < SILENCE_PEAK_AMP
            && self.current_peak_l < SILENCE_PEAK_AMP
            && self.current_peak_r < SILENCE_PEAK_AMP
            && self.hold_peak_l < SILENCE_PEAK_AMP
            && self.hold_peak_r < SILENCE_PEAK_AMP;
        let target = if idle && silent {
            TELEMETRY_PERIOD_LOW
        } else {
            TELEMETRY_PERIOD_FULL
        };
        if target != self.cadence {
            self.cadence = target;
            return Some(target);
        }
        None
    }
}
