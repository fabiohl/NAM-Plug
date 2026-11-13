// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! `PluginMainThreadParams` implementation for the Main Thread.

use super::{
    PARAM_ACTIVATION, PARAM_ACTIVE_MODEL, PARAM_ADAPTIVE_COMPUTE, PARAM_BYPASS,
    PARAM_CALIBRATE_INPUT, PARAM_COUNT, PARAM_GATE_MODE, PARAM_GATE_THRESH,
    PARAM_INPUT_CALIBRATION_LEVEL, PARAM_INPUT_GAIN, PARAM_OUTPUT_GAIN, PARAM_OUTPUT_MODE,
    PARAM_OVERSAMPLE, PARAM_SLIM_OVERRIDE, bypass_bool_to_u32, bypass_f32_to_bool,
    sanitize_param_value, sanitize_param_value_f64,
};
use crate::clap::extensions::calibration::{
    CalibrationConfig, INPUT_CALIBRATION_LEVEL_DEFAULT_DBU, INPUT_CALIBRATION_LEVEL_MAX_DBU,
    INPUT_CALIBRATION_LEVEL_MIN_DBU, OutputMode,
};
use crate::clap::extensions::gate_mode::GateMode;
use crate::clap::plugin::NamClapMainThread;
use crate::clap::plugin::lock::lock_cold;
use clack_extensions::params::{
    ParamDisplayWriter, ParamInfo, ParamInfoFlags, ParamInfoWriter, PluginMainThreadParams,
};
use clack_plugin::events::event_types::ParamValueEvent;
use clack_plugin::prelude::{ClapId, InputEvents, OutputEvents};
use neural_amp_modeler_rs::common::params::RtProcessingParams;
use neural_amp_modeler_rs::math::constants::{GAIN_MAX_DB, GAIN_MIN_DB};
use std::ffi::CStr;

impl PluginMainThreadParams for NamClapMainThread<'_> {
    fn count(&self) -> u32 {
        PARAM_COUNT as u32
    }

    fn get_info(&self, param_index: u32, info: &mut ParamInfoWriter) {
        match param_index {
            PARAM_INPUT_GAIN => {
                info.set(&ParamInfo {
                    id: ClapId::new(PARAM_INPUT_GAIN),
                    flags: ParamInfoFlags::IS_AUTOMATABLE | ParamInfoFlags::IS_MODULATABLE,
                    cookie: clack_plugin::utils::Cookie::empty(),
                    name: b"Input Gain",
                    module: b"",
                    min_value: GAIN_MIN_DB as f64,
                    max_value: GAIN_MAX_DB as f64,
                    default_value: 0.0,
                });
            }
            PARAM_OUTPUT_GAIN => {
                info.set(&ParamInfo {
                    id: ClapId::new(PARAM_OUTPUT_GAIN),
                    flags: ParamInfoFlags::IS_AUTOMATABLE | ParamInfoFlags::IS_MODULATABLE,
                    cookie: clack_plugin::utils::Cookie::empty(),
                    name: b"Output Gain",
                    module: b"",
                    min_value: GAIN_MIN_DB as f64,
                    max_value: GAIN_MAX_DB as f64,
                    default_value: 0.0,
                });
            }
            PARAM_GATE_THRESH => {
                info.set(&ParamInfo {
                    id: ClapId::new(PARAM_GATE_THRESH),
                    flags: ParamInfoFlags::IS_AUTOMATABLE | ParamInfoFlags::IS_MODULATABLE,
                    cookie: clack_plugin::utils::Cookie::empty(),
                    name: b"Gate Threshold",
                    module: b"",
                    min_value: -90.0,
                    max_value: 0.0,
                    // Gate off by default: parked at the most permissive end of the
                    // range so the noise gate practically never closes out of the box
                    // (opt-in tightening via automation/GUI).
                    default_value: -90.0,
                });
            }
            PARAM_BYPASS => {
                info.set(&ParamInfo {
                    id: ClapId::new(PARAM_BYPASS),
                    flags: ParamInfoFlags::IS_AUTOMATABLE
                        | ParamInfoFlags::IS_STEPPED
                        | ParamInfoFlags::IS_BYPASS,
                    cookie: clack_plugin::utils::Cookie::empty(),
                    name: b"Bypass",
                    module: b"",
                    min_value: 0.0,
                    max_value: 1.0,
                    default_value: 0.0,
                });
            }
            PARAM_ACTIVE_MODEL => {
                info.set(&ParamInfo {
                    id: ClapId::new(PARAM_ACTIVE_MODEL),
                    flags: ParamInfoFlags::IS_READONLY,
                    cookie: clack_plugin::utils::Cookie::empty(),
                    name: b"Active Model",
                    module: b"",
                    min_value: 0.0,
                    max_value: 1000.0,
                    default_value: 0.0,
                });
            }
            PARAM_ADAPTIVE_COMPUTE => {
                info.set(&ParamInfo {
                    id: ClapId::new(PARAM_ADAPTIVE_COMPUTE),
                    flags: ParamInfoFlags::IS_AUTOMATABLE
                        | ParamInfoFlags::IS_STEPPED
                        | ParamInfoFlags::IS_ENUM,
                    cookie: clack_plugin::utils::Cookie::empty(),
                    name: b"Adaptive Compute",
                    module: b"",
                    min_value: 0.0,
                    max_value: 2.0,
                    default_value: 1.0,
                });
            }
            PARAM_SLIM_OVERRIDE => {
                info.set(&ParamInfo {
                    id: ClapId::new(PARAM_SLIM_OVERRIDE),
                    flags: ParamInfoFlags::IS_AUTOMATABLE
                        | ParamInfoFlags::IS_STEPPED
                        | ParamInfoFlags::IS_ENUM,
                    cookie: clack_plugin::utils::Cookie::empty(),
                    name: b"Slim Override",
                    module: b"",
                    min_value: 0.0,
                    max_value: 2.0,
                    default_value: 0.0,
                });
            }
            PARAM_OVERSAMPLE => {
                info.set(&ParamInfo {
                    id: ClapId::new(PARAM_OVERSAMPLE),
                    flags: ParamInfoFlags::IS_AUTOMATABLE
                        | ParamInfoFlags::IS_STEPPED
                        | ParamInfoFlags::IS_ENUM,
                    cookie: clack_plugin::utils::Cookie::empty(),
                    name: b"Oversampling",
                    module: b"",
                    min_value: 0.0,
                    max_value: 2.0,
                    default_value: 0.0,
                });
            }
            PARAM_ACTIVATION => {
                info.set(&ParamInfo {
                    id: ClapId::new(PARAM_ACTIVATION),
                    flags: ParamInfoFlags::IS_AUTOMATABLE
                        | ParamInfoFlags::IS_STEPPED
                        | ParamInfoFlags::IS_ENUM,
                    cookie: clack_plugin::utils::Cookie::empty(),
                    name: b"Activation Precision",
                    module: b"",
                    min_value: 0.0,
                    max_value: 1.0,
                    // Universal default is `Standard` (exact-grade, value 1.0),
                    // not `Fast` (value 0.0) — see ACTIVATION_STANDARD/ACTIVATION_FAST.
                    default_value: 1.0,
                });
            }
            PARAM_CALIBRATE_INPUT => {
                info.set(&ParamInfo {
                    id: ClapId::new(PARAM_CALIBRATE_INPUT),
                    flags: ParamInfoFlags::IS_AUTOMATABLE | ParamInfoFlags::IS_STEPPED,
                    cookie: clack_plugin::utils::Cookie::empty(),
                    name: b"Calibrate Input",
                    module: b"Calibration",
                    min_value: 0.0,
                    max_value: 1.0,
                    default_value: 0.0,
                });
            }
            PARAM_INPUT_CALIBRATION_LEVEL => {
                info.set(&ParamInfo {
                    id: ClapId::new(PARAM_INPUT_CALIBRATION_LEVEL),
                    flags: ParamInfoFlags::IS_AUTOMATABLE,
                    cookie: clack_plugin::utils::Cookie::empty(),
                    name: b"Input Cal Level",
                    module: b"Calibration",
                    min_value: INPUT_CALIBRATION_LEVEL_MIN_DBU as f64,
                    max_value: INPUT_CALIBRATION_LEVEL_MAX_DBU as f64,
                    default_value: INPUT_CALIBRATION_LEVEL_DEFAULT_DBU as f64,
                });
            }
            PARAM_OUTPUT_MODE => {
                info.set(&ParamInfo {
                    id: ClapId::new(PARAM_OUTPUT_MODE),
                    flags: ParamInfoFlags::IS_AUTOMATABLE
                        | ParamInfoFlags::IS_STEPPED
                        | ParamInfoFlags::IS_ENUM,
                    cookie: clack_plugin::utils::Cookie::empty(),
                    name: b"Output Mode",
                    module: b"Calibration",
                    min_value: 0.0,
                    max_value: 2.0,
                    default_value: OutputMode::Normalized.to_f32() as f64,
                });
            }
            PARAM_GATE_MODE => {
                info.set(&ParamInfo {
                    id: ClapId::new(PARAM_GATE_MODE),
                    flags: ParamInfoFlags::IS_AUTOMATABLE
                        | ParamInfoFlags::IS_STEPPED
                        | ParamInfoFlags::IS_ENUM,
                    cookie: clack_plugin::utils::Cookie::empty(),
                    name: b"Gate Mode",
                    module: b"",
                    min_value: 0.0,
                    max_value: 1.0,
                    default_value: GateMode::Silence.to_f32() as f64,
                });
            }
            _ => {}
        }
    }

    fn get_value(&self, id: ClapId) -> Option<f64> {
        match id.get() {
            PARAM_INPUT_GAIN => Some(f32::from_bits(
                self.shared
                    .ui_to_rt
                    .param_input_gain
                    .load(std::sync::atomic::Ordering::Relaxed),
            ) as f64),
            PARAM_OUTPUT_GAIN => Some(f32::from_bits(
                self.shared
                    .ui_to_rt
                    .param_output_gain
                    .load(std::sync::atomic::Ordering::Relaxed),
            ) as f64),
            PARAM_GATE_THRESH => Some(f32::from_bits(
                self.shared
                    .ui_to_rt
                    .param_gate_thresh
                    .load(std::sync::atomic::Ordering::Relaxed),
            ) as f64),
            PARAM_BYPASS => Some(
                self.shared
                    .ui_to_rt
                    .param_bypass
                    .load(std::sync::atomic::Ordering::Relaxed) as f64,
            ),
            PARAM_ACTIVE_MODEL => {
                let guard = lock_cold(&self.shared.cold.ui_model_name, "ui_model_name");
                if guard.is_empty() {
                    Some(0.0)
                } else {
                    Some(
                        self.shared
                            .cold
                            .model_load_counter
                            .load(std::sync::atomic::Ordering::Relaxed)
                            as f64,
                    )
                }
            }
            PARAM_ADAPTIVE_COMPUTE => Some(
                self.shared
                    .ui_to_rt
                    .param_adaptive_compute
                    .load(std::sync::atomic::Ordering::Relaxed) as f64,
            ),
            PARAM_SLIM_OVERRIDE => Some(
                self.shared
                    .ui_to_rt
                    .param_slim_override
                    .load(std::sync::atomic::Ordering::Relaxed) as f64,
            ),
            PARAM_OVERSAMPLE => Some(
                self.shared
                    .ui_to_rt
                    .param_oversample
                    .load(std::sync::atomic::Ordering::Relaxed) as f64,
            ),
            PARAM_ACTIVATION => Some(
                self.shared
                    .ui_to_rt
                    .param_activation
                    .load(std::sync::atomic::Ordering::Relaxed) as f64,
            ),
            PARAM_CALIBRATE_INPUT => Some(
                self.shared
                    .ui_to_rt
                    .param_calibrate_input
                    .load(std::sync::atomic::Ordering::Relaxed) as f64,
            ),
            PARAM_INPUT_CALIBRATION_LEVEL => Some(f32::from_bits(
                self.shared
                    .ui_to_rt
                    .param_input_calibration_level
                    .load(std::sync::atomic::Ordering::Relaxed),
            ) as f64),
            PARAM_OUTPUT_MODE => Some(
                self.shared
                    .ui_to_rt
                    .param_output_mode
                    .load(std::sync::atomic::Ordering::Relaxed) as f64,
            ),
            PARAM_GATE_MODE => Some(
                self.shared
                    .ui_to_rt
                    .param_gate_mode
                    .load(std::sync::atomic::Ordering::Relaxed) as f64,
            ),
            _ => None,
        }
    }

    fn value_to_text(
        &self,
        id: ClapId,
        value: f64,
        writer: &mut ParamDisplayWriter,
    ) -> core::fmt::Result {
        use core::fmt::Write;
        match id.get() {
            PARAM_INPUT_GAIN | PARAM_OUTPUT_GAIN | PARAM_GATE_THRESH => {
                writer.write_fmt(format_args!("{:.1} dB", value))
            }
            PARAM_BYPASS => {
                if bypass_f32_to_bool(value as f32) {
                    writer.write_str("Bypassed")
                } else {
                    writer.write_str("Active")
                }
            }
            PARAM_ACTIVE_MODEL => {
                let guard = lock_cold(&self.shared.cold.ui_model_name, "ui_model_name");
                let name = if guard.is_empty() {
                    "None".to_string()
                } else {
                    guard.clone()
                };
                writer.write_str(&name)
            }
            PARAM_ADAPTIVE_COMPUTE => match value.round() as i32 {
                0 => writer.write_str("Off"),
                1 => writer.write_str("Conservative"),
                2 => writer.write_str("Aggressive"),
                _ => writer.write_str("Off"),
            },
            PARAM_SLIM_OVERRIDE => match value.round() as i32 {
                1 => writer.write_str("Force Full"),
                2 => writer.write_str("Force Lite"),
                _ => writer.write_str("Auto"),
            },
            PARAM_OVERSAMPLE => match value.round() as i32 {
                1 => writer.write_str("2x"),
                2 => writer.write_str("4x"),
                _ => writer.write_str("Off"),
            },
            PARAM_ACTIVATION => match value.round() as i32 {
                0 => writer.write_str("Fast"),
                _ => writer.write_str("Standard"),
            },
            PARAM_CALIBRATE_INPUT => {
                if bypass_f32_to_bool(value as f32) {
                    writer.write_str("On")
                } else {
                    writer.write_str("Off")
                }
            }
            PARAM_INPUT_CALIBRATION_LEVEL => writer.write_fmt(format_args!("{:.1} dBu", value)),
            PARAM_OUTPUT_MODE => writer.write_str(OutputMode::from_f32(value as f32).as_str()),
            PARAM_GATE_MODE => writer.write_str(GateMode::from_f32(value as f32).as_str()),
            _ => Ok(()),
        }
    }

    fn text_to_value(&self, id: ClapId, text: &CStr) -> Option<f64> {
        let text_str = text.to_str().ok()?;
        let id_val = id.get();
        let parsed = match id_val {
            PARAM_INPUT_GAIN | PARAM_OUTPUT_GAIN | PARAM_GATE_THRESH => {
                let clean_text = text_str.trim_end_matches(" dB").trim();
                let lower = clean_text.to_lowercase();
                if lower.contains("nan") || lower.contains("inf") {
                    return None;
                }
                let val = clean_text.parse::<f64>().ok()?;
                if !val.is_finite() {
                    return None;
                }
                Some(val)
            }
            PARAM_BYPASS => match text_str.to_lowercase().as_str() {
                "active" | "0" | "false" | "off" => Some(0.0),
                "bypassed" | "1" | "true" | "on" => Some(1.0),
                _ => None,
            },
            PARAM_ACTIVE_MODEL => {
                let guard = lock_cold(&self.shared.cold.ui_model_name, "ui_model_name");
                let current_name = if guard.is_empty() {
                    "None".to_string()
                } else {
                    guard.clone()
                };

                if text_str == current_name {
                    if guard.is_empty() {
                        Some(0.0)
                    } else {
                        Some(
                            self.shared
                                .cold
                                .model_load_counter
                                .load(std::sync::atomic::Ordering::Relaxed)
                                as f64,
                        )
                    }
                } else if let Ok(val) = text_str.parse::<f64>() {
                    if val.is_finite() { Some(val) } else { None }
                } else {
                    Some(0.0)
                }
            }
            PARAM_ADAPTIVE_COMPUTE => match text_str.to_lowercase().as_str() {
                "off" | "0" => Some(0.0),
                "conservative" | "1" => Some(1.0),
                "aggressive" | "2" => Some(2.0),
                _ => {
                    let val = text_str.parse::<f64>().ok()?;
                    if val.is_finite() { Some(val) } else { None }
                }
            },
            PARAM_SLIM_OVERRIDE => match text_str.to_lowercase().as_str() {
                "auto" | "0" => Some(0.0),
                "force full" | "full" | "1" => Some(1.0),
                "force lite" | "lite" | "2" => Some(2.0),
                _ => {
                    let val = text_str.parse::<f64>().ok()?;
                    if val.is_finite() { Some(val) } else { None }
                }
            },
            PARAM_OVERSAMPLE => match text_str.to_lowercase().as_str() {
                "off" | "0" => Some(0.0),
                "2x" | "2" => Some(1.0),
                "4x" | "4" => Some(2.0),
                _ => {
                    let val = text_str.parse::<f64>().ok()?;
                    if val.is_finite() { Some(val) } else { None }
                }
            },
            PARAM_ACTIVATION => match text_str.to_lowercase().as_str() {
                "fast" | "0" => Some(0.0),
                "standard" | "1" => Some(1.0),
                _ => {
                    let val = text_str.parse::<f64>().ok()?;
                    if val.is_finite() { Some(val) } else { None }
                }
            },
            PARAM_CALIBRATE_INPUT => match text_str.trim().to_ascii_lowercase().as_str() {
                "off" | "0" | "false" => Some(0.0),
                "on" | "1" | "true" => Some(1.0),
                _ => None,
            },
            PARAM_INPUT_CALIBRATION_LEVEL => {
                let clean = text_str
                    .trim()
                    .trim_end_matches("dBu")
                    .trim_end_matches("dbu")
                    .trim_end_matches("dB")
                    .trim_end_matches("db")
                    .trim();
                let lower = clean.to_ascii_lowercase();
                if lower.contains("nan") || lower.contains("inf") {
                    return None;
                }
                let val = clean.parse::<f64>().ok()?;
                if val.is_finite() { Some(val) } else { None }
            }
            PARAM_OUTPUT_MODE => OutputMode::from_text(text_str).map(|m| m.to_f32() as f64),
            PARAM_GATE_MODE => GateMode::from_text(text_str).map(|m| m.to_f32() as f64),
            _ => None,
        };

        parsed.map(|v| sanitize_param_value_f64(id_val, v))
    }

    fn flush(&self, input: &InputEvents, output: &mut OutputEvents) {
        self.shared.write_gui_events(output);

        let mut param_changed = false;
        let mut cal_changed = false;
        let mut gate_mode_changed = false;

        for event in input {
            let Some(param_event) = event.as_event::<ParamValueEvent>() else {
                continue;
            };
            let Some(clap_id) = param_event.param_id() else {
                continue;
            };
            let id = clap_id.get();
            let raw_val = param_event.value() as f32;
            let val = sanitize_param_value(id, raw_val);

            match id {
                PARAM_INPUT_GAIN => {
                    self.params.borrow_mut().input_gain_db = val;
                    self.shared
                        .ui_to_rt
                        .param_input_gain
                        .store(val.to_bits(), std::sync::atomic::Ordering::Relaxed);
                }
                PARAM_OUTPUT_GAIN => {
                    self.params.borrow_mut().output_gain_db = val;
                    self.shared
                        .ui_to_rt
                        .param_output_gain
                        .store(val.to_bits(), std::sync::atomic::Ordering::Relaxed);
                }
                PARAM_GATE_THRESH => {
                    self.params.borrow_mut().gate_threshold_db = val;
                    self.shared
                        .ui_to_rt
                        .param_gate_thresh
                        .store(val.to_bits(), std::sync::atomic::Ordering::Relaxed);
                }
                PARAM_BYPASS => {
                    let bypass = bypass_f32_to_bool(val);
                    self.params.borrow_mut().bypass = bypass;
                    self.shared.ui_to_rt.param_bypass.store(
                        bypass_bool_to_u32(bypass),
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
                PARAM_ADAPTIVE_COMPUTE => {
                    let mode =
                        neural_amp_modeler_rs::common::params::AdaptiveComputeMode::from_f32(val);
                    self.params.borrow_mut().adaptive_compute = mode;
                    self.shared
                        .ui_to_rt
                        .param_adaptive_compute
                        .store(mode as u32, std::sync::atomic::Ordering::Relaxed);
                }
                PARAM_SLIM_OVERRIDE => {
                    let ov = neural_amp_modeler_rs::dsp::adaptive::SlimOverride::from_f32(val);
                    self.params.borrow_mut().slim_override = ov;
                    self.shared
                        .ui_to_rt
                        .param_slim_override
                        .store(ov as u32, std::sync::atomic::Ordering::Relaxed);
                }
                PARAM_OVERSAMPLE => {
                    let factor =
                        neural_amp_modeler_rs::dsp::oversample::OversampleFactor::from_f32(val);
                    self.params.borrow_mut().oversample = factor;
                    self.shared
                        .ui_to_rt
                        .param_oversample
                        .store(factor.to_f32() as u32, std::sync::atomic::Ordering::Relaxed);
                }
                PARAM_ACTIVATION => {
                    let mode =
                        neural_amp_modeler_rs::common::params::ActivationPrecision::from_f32(val);
                    self.params.borrow_mut().activation_precision = mode;
                    self.shared
                        .ui_to_rt
                        .param_activation
                        .store(mode as u32, std::sync::atomic::Ordering::Relaxed);
                }
                PARAM_CALIBRATE_INPUT => {
                    let on = bypass_f32_to_bool(val);
                    self.shared
                        .ui_to_rt
                        .param_calibrate_input
                        .store(u32::from(on), std::sync::atomic::Ordering::Relaxed);
                    cal_changed = true;
                    continue;
                }
                PARAM_INPUT_CALIBRATION_LEVEL => {
                    self.shared
                        .ui_to_rt
                        .param_input_calibration_level
                        .store(val.to_bits(), std::sync::atomic::Ordering::Relaxed);
                    cal_changed = true;
                    continue;
                }
                PARAM_OUTPUT_MODE => {
                    let mode = OutputMode::from_f32(val);
                    self.shared
                        .ui_to_rt
                        .param_output_mode
                        .store(mode.to_u32(), std::sync::atomic::Ordering::Relaxed);
                    cal_changed = true;
                    continue;
                }
                PARAM_GATE_MODE => {
                    let mode = GateMode::from_f32(val);
                    self.shared
                        .ui_to_rt
                        .param_gate_mode
                        .store(mode.to_u32(), std::sync::atomic::Ordering::Relaxed);
                    gate_mode_changed = true;
                    continue;
                }
                _ => continue,
            }
            param_changed = true;
        }

        if param_changed || cal_changed || gate_mode_changed {
            // Scoped params borrow: released before the SPSC producer borrow
            // below, and both are released before the host
            // `request_callback()` in the error path.
            let snapshot = param_changed
                .then(|| RtProcessingParams::from_processing_params(&self.params.borrow()));
            let cal_cfg: Option<CalibrationConfig> =
                cal_changed.then(|| self.shared.calibration_config());
            let gm: Option<GateMode> = gate_mode_changed.then(|| self.shared.gate_mode());
            let flush_result = {
                let mut producer = self.cmd_producer.borrow_mut();
                if let Some(s) = snapshot {
                    producer.push_params(s);
                }
                if let Some(c) = cal_cfg {
                    producer.push_calibration(c);
                }
                if let Some(m) = gm {
                    producer.push_gate_mode(m);
                }
                producer.force_flush()
            };
            if flush_result.is_err() {
                if let Some(s) = snapshot {
                    *lock_cold(&self.shared.cold.in_flight_params, "in_flight_params") = Some(s);
                }
                if let Some(c) = cal_cfg {
                    *lock_cold(
                        &self.shared.cold.in_flight_calibration,
                        "in_flight_calibration",
                    ) = Some(c);
                }
                if let Some(m) = gm {
                    *lock_cold(&self.shared.cold.in_flight_gate_mode, "in_flight_gate_mode") =
                        Some(m);
                }
                self.shared.bump_generation();
                self.host.request_callback();
            }
        }
    }
}
