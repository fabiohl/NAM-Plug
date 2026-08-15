// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use crate::clap::extensions::params::{
    PARAM_ACTIVATION, PARAM_ADAPTIVE_COMPUTE, PARAM_BYPASS, PARAM_GATE_THRESH, PARAM_INPUT_GAIN,
    PARAM_OUTPUT_GAIN, PARAM_OVERSAMPLE, PARAM_SLIM_OVERRIDE,
};
use crate::clap::plugin::UiToRt;
use neural_amp_modeler_rs::common::params::RtProcessingParams;
use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
use neural_amp_modeler_rs::dsp::adaptive::AdaptiveCompute;
use neural_amp_modeler_rs::dsp::smoother::ParamSmoother;
use neural_amp_modeler_rs::math::dsp::gain_lut::GainLUT;
use std::sync::atomic::AtomicU32;

#[derive(Clone, Copy)]
pub(crate) struct ScheduledEvent {
    pub(crate) time: usize,
    pub(crate) param_id: u32,
    pub(crate) value: f32,
    pub(crate) is_mod: bool,
}

#[expect(clippy::too_many_arguments)]
pub(crate) fn apply_scheduled_event(
    param_id: u32,
    value: f32,
    is_mod: bool,
    params: &mut RtProcessingParams,
    smoother_in: &mut ParamSmoother,
    smoother_out: &mut ParamSmoother,
    gate_dirty: &mut bool,
    mod_input_gain: &mut f32,
    mod_output_gain: &mut f32,
    mod_gate_thresh: &mut f32,
    adaptive_compute: &mut AdaptiveCompute,
    rt_status: &RtStatusFlags,
    ui_to_rt: &UiToRt,
    gain_lut: &GainLUT,
    buffer_size: u32,
    pending_restart_os_factor: &AtomicU32,
) {
    use crate::clap::extensions::params::{bypass_bool_to_u32, bypass_f32_to_bool};
    use std::sync::atomic::Ordering;

    if is_mod {
        let amount = value;
        match param_id {
            PARAM_INPUT_GAIN => {
                *mod_input_gain = amount;
                smoother_in.set_target(gain_lut.db_to_linear(params.input_gain_db + amount));
            }
            PARAM_OUTPUT_GAIN => {
                *mod_output_gain = amount;
                smoother_out.set_target(gain_lut.db_to_linear(params.output_gain_db + amount));
            }
            PARAM_GATE_THRESH => {
                *mod_gate_thresh = amount;
                *gate_dirty = true;
            }
            _ => {}
        }
    } else {
        let val = value;
        match param_id {
            PARAM_INPUT_GAIN => {
                params.input_gain_db = val;
                ui_to_rt
                    .param_input_gain
                    .store(val.to_bits(), Ordering::Relaxed);
                smoother_in.set_target(gain_lut.db_to_linear(val + *mod_input_gain));
            }
            PARAM_OUTPUT_GAIN => {
                params.output_gain_db = val;
                ui_to_rt
                    .param_output_gain
                    .store(val.to_bits(), Ordering::Relaxed);
                smoother_out.set_target(gain_lut.db_to_linear(val + *mod_output_gain));
            }
            PARAM_GATE_THRESH => {
                params.gate_threshold_db = val;
                ui_to_rt
                    .param_gate_thresh
                    .store(val.to_bits(), Ordering::Relaxed);
                *gate_dirty = true;
            }
            PARAM_BYPASS => {
                let bypass = bypass_f32_to_bool(val);
                params.bypass = bypass;
                ui_to_rt
                    .param_bypass
                    .store(bypass_bool_to_u32(bypass), Ordering::Relaxed);
            }
            PARAM_ADAPTIVE_COMPUTE => {
                let mode =
                    neural_amp_modeler_rs::common::params::AdaptiveComputeMode::from_f32(val);
                params.adaptive_compute = mode;
                ui_to_rt
                    .param_adaptive_compute
                    .store(mode as u32, Ordering::Relaxed);
                adaptive_compute.set_mode(mode, rt_status);
            }
            PARAM_SLIM_OVERRIDE => {
                let ov = neural_amp_modeler_rs::dsp::adaptive::SlimOverride::from_f32(val);
                params.slim_override = ov;
                ui_to_rt
                    .param_slim_override
                    .store(ov as u32, Ordering::Relaxed);
                adaptive_compute.set_slim_override(ov);
            }
            PARAM_OVERSAMPLE => {
                let factor =
                    neural_amp_modeler_rs::dsp::oversample::OversampleFactor::from_f32(val);
                if factor != params.oversample {
                    params.oversample = factor;
                    ui_to_rt
                        .param_oversample
                        .store(factor.to_f32() as u32, Ordering::Relaxed);
                    // If the plugin is active, defer the rebuild
                    // via host restart; otherwise flag the main thread.
                    if buffer_size > 0 {
                        pending_restart_os_factor.store(factor.to_f32() as u32, Ordering::Release);
                    } else {
                        rt_status
                            .requested_os_factor
                            .store(factor.to_f32() as u32, Ordering::Relaxed);
                        rt_status.set_flag_release(
                            neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_OS_REBUILD,
                        );
                    }
                }
            }
            PARAM_ACTIVATION => {
                let mode =
                    neural_amp_modeler_rs::common::params::ActivationPrecision::from_f32(val);
                if mode != params.activation_precision {
                    params.activation_precision = mode;
                    ui_to_rt
                        .param_activation
                        .store(mode as u32, Ordering::Relaxed);
                    neural_amp_modeler_rs::math::activations::set_activation_tls(mode);
                }
            }
            _ => {}
        }
    }
}
