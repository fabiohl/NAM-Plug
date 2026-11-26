// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use crate::clap::extensions::calibration::{
    CalibrationConfig, ModelCalibrationMeta, OutputMode, compute_calibration_multipliers,
};
use crate::clap::extensions::gate_mode::GateMode;
use crate::clap::extensions::params::{
    PARAM_ACTIVATION, PARAM_ADAPTIVE_COMPUTE, PARAM_BYPASS, PARAM_CAB_BYPASS,
    PARAM_CALIBRATE_INPUT, PARAM_GATE_MODE, PARAM_GATE_THRESH, PARAM_INPUT_CALIBRATION_LEVEL,
    PARAM_INPUT_GAIN, PARAM_OUTPUT_GAIN, PARAM_OUTPUT_MODE, PARAM_OVERSAMPLE, PARAM_SLIM_OVERRIDE,
};
use crate::clap::plugin::{PendingRestartOs, UiToRt};
use crate::clap::processor::dsp::musical_gate::MusicalNoiseGate;
use neural_amp_modeler_rs::common::params::RtProcessingParams;
use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
use neural_amp_modeler_rs::dsp::adaptive::AdaptiveCompute;
use neural_amp_modeler_rs::dsp::smoother::ParamSmoother;
use neural_amp_modeler_rs::math::dsp::gain_lut::GainLUT;
use std::sync::atomic::{AtomicBool, AtomicU32};

#[derive(Clone, Copy)]
pub(crate) struct ScheduledEvent {
    pub(crate) time: usize,
    pub(crate) param_id: u32,
    pub(crate) value: f32,
    pub(crate) is_mod: bool,
}

/// One intra-sub-block gain targeting step produced while fusing a run of
/// gain events into a single inference sub-block.
///
/// `offset` is relative to the sub-block start. Applying the smoother's IIR
/// ramp piecewise at these exact offsets reproduces, bit-for-bit, the ramp the
/// legacy per-event sub-block split produced — because the ramp is evaluated
/// analytically from `(start, target, n)` and the smoother state converges to
/// the same value. The neural model itself is block-invariant (see
/// `clap_e2_proptest`), so fusing the inference call over the span is
/// numerically transparent.
#[derive(Clone, Copy)]
pub(crate) struct GainStep {
    pub(crate) offset: u32,
    pub(crate) target: f32,
    pub(crate) is_input: bool,
}

/// Gain target resolved from one gain event, returned by
/// [`ParamTarget::apply`] so callers can stage a piecewise ramp instead of
/// jumping the smoother immediately.
#[derive(Clone, Copy)]
pub(crate) struct GainTarget {
    pub(crate) is_input: bool,
    pub(crate) target: f32,
}

/// Single source of truth for "smoothed gain" parameters whose events may be
/// fused into one inference sub-block: they are continuously smoothed, so they
/// do not require an exact sample boundary. [`ParamTarget::apply`] returns
/// `Some(GainTarget)` for exactly these parameters, and the scheduler's
/// boundary classification delegates here, so the two can never drift.
#[inline(always)]
pub(crate) fn is_fusable_gain_param(param_id: u32) -> bool {
    param_id == PARAM_INPUT_GAIN || param_id == PARAM_OUTPUT_GAIN
}

pub(crate) struct ParamTarget<'p> {
    pub(crate) params: &'p mut RtProcessingParams,
    pub(crate) cab_bypass: &'p mut bool,
    pub(crate) cal_config: &'p mut CalibrationConfig,
    pub(crate) model_cal_meta: &'p ModelCalibrationMeta,
    pub(crate) model_input_mult_adj: &'p mut f32,
    pub(crate) model_output_mult_adj: &'p mut f32,
    pub(crate) smoother_in: &'p mut ParamSmoother,
    pub(crate) smoother_out: &'p mut ParamSmoother,
    pub(crate) smoother_cal_in: &'p mut ParamSmoother,
    pub(crate) smoother_cal_out: &'p mut ParamSmoother,
    pub(crate) gate_dirty: &'p mut bool,
    pub(crate) gate_mode: &'p mut GateMode,
    pub(crate) musical_gate: &'p mut MusicalNoiseGate,
    pub(crate) mod_input_gain: &'p mut f32,
    pub(crate) mod_output_gain: &'p mut f32,
    pub(crate) mod_gate_thresh: &'p mut f32,
    pub(crate) adaptive_compute: &'p mut AdaptiveCompute,
    pub(crate) rt_status: &'p RtStatusFlags,
    pub(crate) ui_to_rt: &'p UiToRt,
    pub(crate) gain_lut: &'p GainLUT,
    pub(crate) buffer_size: u32,
    pub(crate) pending_restart_os_factor: &'p AtomicU32,
    pub(crate) restart_wanted: &'p AtomicBool,
    pub(crate) render_mode: &'p AtomicU32,
    pub(crate) realtime_activation:
        &'p mut neural_amp_modeler_rs::common::params::ActivationPrecision,
    pub(crate) silence_hyst: &'p mut neural_amp_modeler_rs::dsp::gate::DynamicHysteresis,
}

#[inline]
pub(crate) fn apply_gate_mode_change(
    mode: GateMode,
    gate_mode: &mut GateMode,
    musical_gate: &mut MusicalNoiseGate,
    silence_hyst: &mut neural_amp_modeler_rs::dsp::gate::DynamicHysteresis,
    gate_dirty: &mut bool,
) -> bool {
    if mode == *gate_mode {
        return false;
    }
    *gate_mode = mode;
    musical_gate.reset();
    silence_hyst.reset();
    *gate_dirty = true;
    true
}

#[inline]
pub(crate) fn latch_oversample_restart(
    factor: neural_amp_modeler_rs::dsp::oversample::OversampleFactor,
    buffer_size: u32,
    pending_restart_os_factor: &AtomicU32,
    restart_wanted: &AtomicBool,
    rt_status: &RtStatusFlags,
) {
    use std::sync::atomic::Ordering;
    if buffer_size > 0 {
        let prev = PendingRestartOs::load(pending_restart_os_factor, Ordering::Relaxed);
        if prev != PendingRestartOs::Pending(factor) {
            PendingRestartOs::Pending(factor).store(pending_restart_os_factor, Ordering::Release);
            restart_wanted.store(true, Ordering::Release);
        }
    } else {
        rt_status
            .requested_os_factor
            .store(factor.to_f32() as u32, Ordering::Relaxed);
        rt_status.set_flag_release(neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_OS_REBUILD);
    }
}

impl<'p> ParamTarget<'p> {
    pub(crate) fn apply(
        &mut self,
        param_id: u32,
        value: f32,
        is_mod: bool,
        smooth: bool,
    ) -> Option<GainTarget> {
        self.apply_inner(param_id, value, is_mod, smooth, true)
    }

    pub(crate) fn apply_synced(
        &mut self,
        param_id: u32,
        value: f32,
        is_mod: bool,
        smooth: bool,
    ) -> Option<GainTarget> {
        self.apply_inner(param_id, value, is_mod, smooth, false)
    }

    fn apply_inner(
        &mut self,
        param_id: u32,
        value: f32,
        is_mod: bool,
        smooth: bool,
        publish: bool,
    ) -> Option<GainTarget> {
        use crate::clap::extensions::params::{
            bypass_bool_to_u32, bypass_f32_to_bool, sanitize_param_value,
        };
        use std::sync::atomic::Ordering;

        if is_mod {
            let amount = if value.is_finite() { value } else { 0.0 };
            match param_id {
                PARAM_INPUT_GAIN => {
                    *self.mod_input_gain = amount;
                    let target = self
                        .gain_lut
                        .db_to_linear(self.params.input_gain_db + amount);
                    if smooth {
                        self.smoother_in.set_target(target);
                    }
                    return Some(GainTarget {
                        is_input: true,
                        target,
                    });
                }
                PARAM_OUTPUT_GAIN => {
                    *self.mod_output_gain = amount;
                    let target = self
                        .gain_lut
                        .db_to_linear(self.params.output_gain_db + amount);
                    if smooth {
                        self.smoother_out.set_target(target);
                    }
                    return Some(GainTarget {
                        is_input: false,
                        target,
                    });
                }
                PARAM_GATE_THRESH => {
                    *self.mod_gate_thresh = amount;
                    *self.gate_dirty = true;
                }
                _ => {}
            }
            return None;
        }

        let val = sanitize_param_value(param_id, value);
        match param_id {
            PARAM_INPUT_GAIN => {
                self.params.input_gain_db = val;
                if publish {
                    self.ui_to_rt
                        .param_input_gain
                        .store(val.to_bits(), Ordering::Relaxed);
                }
                let target = self.gain_lut.db_to_linear(val + *self.mod_input_gain);
                if smooth {
                    self.smoother_in.set_target(target);
                }
                Some(GainTarget {
                    is_input: true,
                    target,
                })
            }
            PARAM_OUTPUT_GAIN => {
                self.params.output_gain_db = val;
                if publish {
                    self.ui_to_rt
                        .param_output_gain
                        .store(val.to_bits(), Ordering::Relaxed);
                }
                let target = self.gain_lut.db_to_linear(val + *self.mod_output_gain);
                if smooth {
                    self.smoother_out.set_target(target);
                }
                Some(GainTarget {
                    is_input: false,
                    target,
                })
            }
            PARAM_GATE_THRESH => {
                self.params.gate_threshold_db = val;
                if publish {
                    self.ui_to_rt
                        .param_gate_thresh
                        .store(val.to_bits(), Ordering::Relaxed);
                }
                *self.gate_dirty = true;
                None
            }
            PARAM_GATE_MODE => {
                let mode = GateMode::from_f32(val);
                if apply_gate_mode_change(
                    mode,
                    self.gate_mode,
                    self.musical_gate,
                    self.silence_hyst,
                    self.gate_dirty,
                ) && publish
                {
                    self.ui_to_rt
                        .param_gate_mode
                        .store(mode.to_u32(), Ordering::Relaxed);
                }
                None
            }
            PARAM_BYPASS => {
                let bypass = bypass_f32_to_bool(val);
                self.params.bypass = bypass;
                if publish {
                    self.ui_to_rt
                        .param_bypass
                        .store(bypass_bool_to_u32(bypass), Ordering::Relaxed);
                }
                None
            }
            PARAM_CAB_BYPASS => {
                let bypass = bypass_f32_to_bool(val);
                *self.cab_bypass = bypass;
                if publish {
                    self.ui_to_rt
                        .param_cab_bypass
                        .store(bypass_bool_to_u32(bypass), Ordering::Relaxed);
                }
                None
            }
            PARAM_ADAPTIVE_COMPUTE => {
                let mode =
                    neural_amp_modeler_rs::common::params::AdaptiveComputeMode::from_f32(val);
                self.params.adaptive_compute = mode;
                if publish {
                    self.ui_to_rt
                        .param_adaptive_compute
                        .store(mode as u32, Ordering::Relaxed);
                }
                self.adaptive_compute.set_mode(mode, self.rt_status);
                None
            }
            PARAM_SLIM_OVERRIDE => {
                let ov = neural_amp_modeler_rs::dsp::adaptive::SlimOverride::from_f32(val);
                self.params.slim_override = ov;
                if publish {
                    self.ui_to_rt
                        .param_slim_override
                        .store(ov as u32, Ordering::Relaxed);
                }
                self.adaptive_compute.set_slim_override(ov);
                None
            }
            PARAM_OVERSAMPLE => {
                let factor =
                    neural_amp_modeler_rs::dsp::oversample::OversampleFactor::from_f32(val);
                if factor != self.params.oversample {
                    self.params.oversample = factor;
                    if publish {
                        self.ui_to_rt
                            .param_oversample
                            .store(factor.to_f32() as u32, Ordering::Relaxed);
                    }
                    latch_oversample_restart(
                        factor,
                        self.buffer_size,
                        self.pending_restart_os_factor,
                        self.restart_wanted,
                        self.rt_status,
                    );
                }
                None
            }
            PARAM_ACTIVATION => {
                let mode =
                    neural_amp_modeler_rs::common::params::ActivationPrecision::from_f32(val);
                if mode != *self.realtime_activation {
                    *self.realtime_activation = mode;
                    let effective = crate::clap::processor::effective_activation(
                        self.render_mode.load(Ordering::Acquire),
                        mode,
                    );
                    self.params.activation_precision = effective;
                    if publish {
                        self.ui_to_rt
                            .param_activation
                            .store(mode as u32, Ordering::Relaxed);
                    }
                    neural_amp_modeler_rs::math::activations::set_activation_tls(effective);
                }
                None
            }
            PARAM_CALIBRATE_INPUT => {
                let enabled = val >= 0.5;
                if enabled != self.cal_config.calibrate_input {
                    self.cal_config.calibrate_input = enabled;
                    if publish {
                        self.ui_to_rt
                            .param_calibrate_input
                            .store(enabled as u32, Ordering::Relaxed);
                    }
                    update_calibration_multipliers(
                        self.model_cal_meta,
                        *self.cal_config,
                        self.model_input_mult_adj,
                        self.model_output_mult_adj,
                        self.smoother_cal_in,
                        self.smoother_cal_out,
                        smooth,
                    );
                }
                None
            }
            PARAM_INPUT_CALIBRATION_LEVEL => {
                let level_changed = if publish {
                    (val - self.cal_config.input_calibration_level_dbu).abs() > f32::EPSILON
                } else {
                    val != self.cal_config.input_calibration_level_dbu
                };
                if level_changed {
                    self.cal_config.input_calibration_level_dbu = val;
                    if publish {
                        self.ui_to_rt
                            .param_input_calibration_level
                            .store(val.to_bits(), Ordering::Relaxed);
                    }
                    update_calibration_multipliers(
                        self.model_cal_meta,
                        *self.cal_config,
                        self.model_input_mult_adj,
                        self.model_output_mult_adj,
                        self.smoother_cal_in,
                        self.smoother_cal_out,
                        smooth,
                    );
                }
                None
            }
            PARAM_OUTPUT_MODE => {
                let mode = OutputMode::from_f32(val);
                if mode != self.cal_config.output_mode {
                    self.cal_config.output_mode = mode;
                    if publish {
                        self.ui_to_rt
                            .param_output_mode
                            .store(mode.to_u32(), Ordering::Relaxed);
                    }
                    update_calibration_multipliers(
                        self.model_cal_meta,
                        *self.cal_config,
                        self.model_input_mult_adj,
                        self.model_output_mult_adj,
                        self.smoother_cal_in,
                        self.smoother_cal_out,
                        smooth,
                    );
                }
                None
            }
            _ => None,
        }
    }
}

#[inline]
fn update_calibration_multipliers(
    model_cal_meta: &ModelCalibrationMeta,
    cal_config: CalibrationConfig,
    model_input_mult_adj: &mut f32,
    model_output_mult_adj: &mut f32,
    smoother_cal_in: &mut ParamSmoother,
    smoother_cal_out: &mut ParamSmoother,
    apply_smoother: bool,
) {
    let (in_mult, out_mult) = compute_calibration_multipliers(*model_cal_meta, cal_config);
    *model_input_mult_adj = in_mult;
    *model_output_mult_adj = out_mult;
    if apply_smoother {
        smoother_cal_in.set_target(in_mult);
        smoother_cal_out.set_target(out_mult);
    }
}
