// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! `PluginAudioProcessorParams` implementation for the Audio Thread.

use super::{
    PARAM_ACTIVATION, PARAM_ADAPTIVE_COMPUTE, PARAM_BYPASS, PARAM_CAB_BYPASS,
    PARAM_CALIBRATE_INPUT, PARAM_GATE_MODE, PARAM_GATE_THRESH, PARAM_INPUT_CALIBRATION_LEVEL,
    PARAM_INPUT_GAIN, PARAM_OUTPUT_GAIN, PARAM_OUTPUT_MODE, PARAM_OVERSAMPLE, PARAM_SLIM_OVERRIDE,
};
use crate::clap::processor::NamClapProcessor;
use clack_extensions::params::PluginAudioProcessorParams;
use clack_plugin::events::event_types::{ParamModEvent, ParamValueEvent};
use clack_plugin::prelude::{InputEvents, OutputEvents};

/// Implementation of `PluginAudioProcessorParams` for the Audio Thread.
///
/// # Design: Intentional Duplication
///
/// The parameter event parsing here is structurally identical to the one in
/// `PluginMainThreadParams::flush()` above. This duplication is **intentional**:
///
/// - **Main Thread flush()**: Updates `self.params` + atomics + sends snapshot via SPSC
///   (necessary because the Audio Thread may not be active).
/// - **Audio Thread flush()**: Updates `self.params` + atomics + syncs parameters
///   coming from the GUI via direct atomic reads (no SPSC, since we're already on the Audio Thread).
///
/// Extracting a common helper would be possible but would add unnecessary indirection
/// and complicate the `self` mutable lifetimes in each context.
impl PluginAudioProcessorParams for NamClapProcessor<'_> {
    /// Processes parameter events on the Audio Thread when `process()` is not being called.
    ///
    /// In addition to applying host events, this method also syncs parameters
    /// that were changed by the GUI directly in the shared atomics.
    fn flush(&mut self, input: &InputEvents, output: &mut OutputEvents) {
        // Isolate panics from this instance to protect the host.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.flush_inner(input, output);
        }));
        if let Err(err) = result {
            drop(err);
        }
    }
}

impl NamClapProcessor<'_> {
    #[inline(always)]
    fn flush_inner(&mut self, input: &InputEvents, output: &mut OutputEvents) {
        self.shared.write_gui_events(output);

        let mut param_changed = false;
        for event in input {
            if let Some(param_event) = event.as_event::<ParamValueEvent>() {
                let Some(clap_id) = param_event.param_id() else {
                    continue;
                };
                let id = clap_id.get();
                let val = param_event.value() as f32;

                match id {
                    PARAM_INPUT_GAIN => self.set_input_gain(val),
                    PARAM_OUTPUT_GAIN => self.set_output_gain(val),
                    PARAM_GATE_THRESH => self.set_gate_threshold(val),
                    PARAM_BYPASS => self.set_bypass(val),
                    PARAM_ADAPTIVE_COMPUTE => self.set_adaptive_compute(val),
                    PARAM_SLIM_OVERRIDE => self.set_slim_override(val),
                    PARAM_OVERSAMPLE => self.set_oversample(val),
                    PARAM_ACTIVATION => self.set_activation(val),
                    PARAM_CALIBRATE_INPUT => self.set_calibrate_input(val),
                    PARAM_INPUT_CALIBRATION_LEVEL => self.set_input_calibration_level(val),
                    PARAM_OUTPUT_MODE => self.set_output_mode(val),
                    PARAM_GATE_MODE => self.set_gate_mode(val),
                    PARAM_CAB_BYPASS => self.set_cab_bypass(val),
                    _ => continue,
                }
                param_changed = true;
            } else if let Some(mod_event) = event.as_event::<ParamModEvent>() {
                let Some(clap_id) = mod_event.param_id() else {
                    continue;
                };
                let id = clap_id.get();
                let val = mod_event.amount() as f32;
                let amount = if val.is_finite() { val } else { 0.0 };

                match id {
                    PARAM_INPUT_GAIN => {
                        self.mod_input_gain = amount;
                        self.smoother_in.set_target(
                            self.gain_lut
                                .db_to_linear(self.params.input_gain_db + amount),
                        );
                        param_changed = true;
                    }
                    PARAM_OUTPUT_GAIN => {
                        self.mod_output_gain = amount;
                        self.smoother_out.set_target(
                            self.gain_lut
                                .db_to_linear(self.params.output_gain_db + amount),
                        );
                        param_changed = true;
                    }
                    PARAM_GATE_THRESH => {
                        self.mod_gate_thresh = amount;
                        self.gate_dirty = true;
                        param_changed = true;
                    }
                    _ => continue,
                }
            }
        }

        if param_changed {
            self.shared.bump_generation();
        }

        let generation = self
            .shared
            .ui_to_rt
            .gui_param_generation
            .load(std::sync::atomic::Ordering::Acquire); // pairs with Release fetch_add em plugin/shared.rs:313, gui/ui/bypass.rs:62, gui/ui/knob.rs:281
        if generation != self.last_seen_generation {
            self.sync_params_from_gui();
            self.last_seen_generation = generation;
        }

        self.check_tail_changed();
    }
}
