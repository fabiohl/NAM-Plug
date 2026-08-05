// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! `PluginAudioProcessorParams` implementation for the Audio Thread.

use super::{
    PARAM_ACTIVATION, PARAM_ADAPTIVE_COMPUTE, PARAM_BYPASS, PARAM_GATE_THRESH, PARAM_INPUT_GAIN,
    PARAM_OUTPUT_GAIN, PARAM_OVERSAMPLE, PARAM_SLIM_OVERRIDE, bypass_u32_to_bool,
};
use crate::clap::processor::NamClapProcessor;
use clack_extensions::params::PluginAudioProcessorParams;
use clack_plugin::events::event_types::ParamValueEvent;
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
        // S5-E5-T03: isolate panics from this instance.
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
            let Some(param_event) = event.as_event::<ParamValueEvent>() else {
                continue;
            };
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
                _ => continue,
            }
            param_changed = true;
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
            self.last_seen_generation = generation;

            let shared_in_db = f32::from_bits(
                self.shared
                    .ui_to_rt
                    .param_input_gain
                    .load(std::sync::atomic::Ordering::Relaxed),
            );
            if shared_in_db != self.params.input_gain_db {
                self.params.input_gain_db = shared_in_db;
            }

            let shared_out_db = f32::from_bits(
                self.shared
                    .ui_to_rt
                    .param_output_gain
                    .load(std::sync::atomic::Ordering::Relaxed),
            );
            if shared_out_db != self.params.output_gain_db {
                self.params.output_gain_db = shared_out_db;
            }

            let shared_gate_db = f32::from_bits(
                self.shared
                    .ui_to_rt
                    .param_gate_thresh
                    .load(std::sync::atomic::Ordering::Relaxed),
            );
            if shared_gate_db != self.params.gate_threshold_db {
                self.params.gate_threshold_db = shared_gate_db;
            }

            let shared_bypass = bypass_u32_to_bool(
                self.shared
                    .ui_to_rt
                    .param_bypass
                    .load(std::sync::atomic::Ordering::Relaxed),
            );
            if shared_bypass != self.params.bypass {
                self.params.bypass = shared_bypass;
            }

            let shared_adaptive =
                neural_amp_modeler_rs::common::params::AdaptiveComputeMode::from_f32(
                    self.shared
                        .ui_to_rt
                        .param_adaptive_compute
                        .load(std::sync::atomic::Ordering::Relaxed) as f32,
                );
            if shared_adaptive != self.params.adaptive_compute {
                self.params.adaptive_compute = shared_adaptive;
            }

            let shared_slim_override = neural_amp_modeler_rs::dsp::adaptive::SlimOverride::from_f32(
                self.shared
                    .ui_to_rt
                    .param_slim_override
                    .load(std::sync::atomic::Ordering::Relaxed) as f32,
            );
            if shared_slim_override != self.params.slim_override {
                self.params.slim_override = shared_slim_override;
            }

            let shared_oversample =
                neural_amp_modeler_rs::dsp::oversample::OversampleFactor::from_f32(
                    self.shared
                        .ui_to_rt
                        .param_oversample
                        .load(std::sync::atomic::Ordering::Relaxed) as f32,
                );
            if shared_oversample != self.params.oversample {
                self.params.oversample = shared_oversample;
                self.apply_oversample(shared_oversample);
            }

            let shared_activation =
                neural_amp_modeler_rs::common::params::ActivationPrecision::from_f32(
                    self.shared
                        .ui_to_rt
                        .param_activation
                        .load(std::sync::atomic::Ordering::Relaxed) as f32,
                );
            if shared_activation != self.params.activation_precision {
                self.params.activation_precision = shared_activation;
                neural_amp_modeler_rs::math::activations::set_activation_tls(shared_activation);
            }
        }
    }
}
