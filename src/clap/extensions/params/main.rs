// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! `PluginMainThreadParams` implementation for the Main Thread.

use super::{
    PARAM_ACTIVATION, PARAM_ACTIVE_MODEL, PARAM_ADAPTIVE_COMPUTE, PARAM_BYPASS, PARAM_GATE_THRESH,
    PARAM_INPUT_GAIN, PARAM_OUTPUT_GAIN, PARAM_OVERSAMPLE, PARAM_SLIM_OVERRIDE, bypass_bool_to_u32,
    bypass_f32_to_bool,
};
use crate::clap::plugin::NamClapMainThread;
use clack_extensions::params::{
    ParamDisplayWriter, ParamInfo, ParamInfoFlags, ParamInfoWriter, PluginMainThreadParams,
};
use clack_plugin::events::event_types::ParamValueEvent;
use clack_plugin::prelude::{ClapId, InputEvents, OutputEvents};
use neural_amp_modeler_rs::common::params::RtProcessingParams;
use neural_amp_modeler_rs::math::constants::{GAIN_MAX_DB, GAIN_MIN_DB};
use std::ffi::CStr;

impl PluginMainThreadParams for NamClapMainThread<'_> {
    fn count(&mut self) -> u32 {
        9
    }

    fn get_info(&mut self, param_index: u32, info: &mut ParamInfoWriter) {
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
                    max_value: -40.0,
                    default_value: -70.0,
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
                    flags: ParamInfoFlags::IS_AUTOMATABLE | ParamInfoFlags::IS_STEPPED,
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
                    flags: ParamInfoFlags::IS_AUTOMATABLE | ParamInfoFlags::IS_STEPPED,
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
                    flags: ParamInfoFlags::IS_AUTOMATABLE | ParamInfoFlags::IS_STEPPED,
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
                    flags: ParamInfoFlags::IS_AUTOMATABLE | ParamInfoFlags::IS_STEPPED,
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
            _ => {}
        }
    }

    fn get_value(&mut self, id: ClapId) -> Option<f64> {
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
            PARAM_ACTIVE_MODEL => Some(
                self.shared
                    .cold
                    .model_load_counter
                    .load(std::sync::atomic::Ordering::Relaxed) as f64,
            ),
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
            _ => None,
        }
    }

    fn value_to_text(
        &mut self,
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
                let guard = self.shared.cold.ui_model_name.lock().unwrap_or_else(|e| {
                    log::error!("PoisonError in ui_model_name lock: {e:?}");
                    e.into_inner()
                });
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
            _ => Ok(()),
        }
    }

    fn text_to_value(&mut self, id: ClapId, text: &CStr) -> Option<f64> {
        let text_str = text.to_str().ok()?;
        match id.get() {
            PARAM_INPUT_GAIN | PARAM_OUTPUT_GAIN | PARAM_GATE_THRESH => {
                let clean_text = text_str.trim_end_matches(" dB").trim();
                clean_text.parse::<f64>().ok()
            }
            PARAM_BYPASS => match text_str.to_lowercase().as_str() {
                "active" | "0" | "false" | "off" => Some(0.0),
                "bypassed" | "1" | "true" | "on" => Some(1.0),
                _ => None,
            },
            PARAM_ACTIVE_MODEL => {
                let guard = self.shared.cold.ui_model_name.lock().unwrap_or_else(|e| {
                    log::error!("PoisonError in ui_model_name lock: {e:?}");
                    e.into_inner()
                });
                let current_name = if guard.is_empty() {
                    "None".to_string()
                } else {
                    guard.clone()
                };

                if text_str == current_name {
                    Some(
                        self.shared
                            .cold
                            .model_load_counter
                            .load(std::sync::atomic::Ordering::Relaxed)
                            as f64,
                    )
                } else if let Ok(val) = text_str.parse::<f64>() {
                    Some(val)
                } else {
                    Some(0.0)
                }
            }
            PARAM_ADAPTIVE_COMPUTE => match text_str.to_lowercase().as_str() {
                "off" | "0" => Some(0.0),
                "conservative" | "1" => Some(1.0),
                "aggressive" | "2" => Some(2.0),
                _ => text_str.parse::<f64>().ok(),
            },
            PARAM_SLIM_OVERRIDE => match text_str.to_lowercase().as_str() {
                "auto" | "0" => Some(0.0),
                "force full" | "full" | "1" => Some(1.0),
                "force lite" | "lite" | "2" => Some(2.0),
                _ => text_str.parse::<f64>().ok(),
            },
            PARAM_OVERSAMPLE => match text_str.to_lowercase().as_str() {
                "off" | "0" => Some(0.0),
                "2x" | "2" => Some(1.0),
                "4x" | "4" => Some(2.0),
                _ => text_str.parse::<f64>().ok(),
            },
            PARAM_ACTIVATION => match text_str.to_lowercase().as_str() {
                "fast" | "0" => Some(0.0),
                "standard" | "1" => Some(1.0),
                _ => text_str.parse::<f64>().ok(),
            },
            _ => None,
        }
    }

    fn flush(&mut self, input: &InputEvents, output: &mut OutputEvents) {
        self.shared.write_gui_events(output);

        let mut param_changed = false;

        for event in input {
            let Some(param_event) = event.as_event::<ParamValueEvent>() else {
                continue;
            };
            let Some(clap_id) = param_event.param_id() else {
                continue;
            };
            let id = clap_id.get();
            let val = param_event.value() as f32;

            match id {
                PARAM_INPUT_GAIN => {
                    self.params.input_gain_db = val;
                    self.shared
                        .ui_to_rt
                        .param_input_gain
                        .store(val.to_bits(), std::sync::atomic::Ordering::Relaxed);
                }
                PARAM_OUTPUT_GAIN => {
                    self.params.output_gain_db = val;
                    self.shared
                        .ui_to_rt
                        .param_output_gain
                        .store(val.to_bits(), std::sync::atomic::Ordering::Relaxed);
                }
                PARAM_GATE_THRESH => {
                    self.params.gate_threshold_db = val;
                    self.shared
                        .ui_to_rt
                        .param_gate_thresh
                        .store(val.to_bits(), std::sync::atomic::Ordering::Relaxed);
                }
                PARAM_BYPASS => {
                    self.params.bypass = bypass_f32_to_bool(val);
                    self.shared.ui_to_rt.param_bypass.store(
                        bypass_bool_to_u32(self.params.bypass),
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
                PARAM_ADAPTIVE_COMPUTE => {
                    let mode =
                        neural_amp_modeler_rs::common::params::AdaptiveComputeMode::from_f32(val);
                    self.params.adaptive_compute = mode;
                    self.shared
                        .ui_to_rt
                        .param_adaptive_compute
                        .store(mode as u32, std::sync::atomic::Ordering::Relaxed);
                }
                PARAM_SLIM_OVERRIDE => {
                    let ov = neural_amp_modeler_rs::dsp::adaptive::SlimOverride::from_f32(val);
                    self.params.slim_override = ov;
                    self.shared
                        .ui_to_rt
                        .param_slim_override
                        .store(ov as u32, std::sync::atomic::Ordering::Relaxed);
                }
                PARAM_OVERSAMPLE => {
                    let factor =
                        neural_amp_modeler_rs::dsp::oversample::OversampleFactor::from_f32(val);
                    self.params.oversample = factor;
                    self.shared
                        .ui_to_rt
                        .param_oversample
                        .store(factor.to_f32() as u32, std::sync::atomic::Ordering::Relaxed);
                }
                PARAM_ACTIVATION => {
                    let mode =
                        neural_amp_modeler_rs::common::params::ActivationPrecision::from_f32(val);
                    self.params.activation_precision = mode;
                    self.shared
                        .ui_to_rt
                        .param_activation
                        .store(mode as u32, std::sync::atomic::Ordering::Relaxed);
                }
                _ => continue,
            }
            param_changed = true;
        }

        if param_changed {
            let snapshot = RtProcessingParams::from_processing_params(&self.params);
            self.cmd_producer.push_params(snapshot);
            if self.cmd_producer.force_flush().is_err() {
                let mut guard = self
                    .shared
                    .cold
                    .in_flight_params
                    .lock()
                    .unwrap_or_else(|e| {
                        log::error!(
                            "in_flight_params mutex poisoned during param flush, recovering"
                        );
                        e.into_inner()
                    });
                *guard = Some(snapshot);
                self.shared.bump_generation();
                self.host.request_callback();
            }
        }
    }
}
