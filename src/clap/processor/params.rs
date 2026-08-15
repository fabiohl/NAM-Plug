// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Parameter synchronization helpers for `NamClapProcessor`.
//!
//! Extracts the triplicated logic of updating `self.params.*`,
//! `self.shared.ui_to_rt.*`, `smoother`, and `gate_dirty` from the
//! three event-processing paths (SPSC, Host Events, GUI sync).

use super::NamClapProcessor;
use crate::clap::extensions::params::{bypass_bool_to_u32, bypass_f32_to_bool, bypass_u32_to_bool};
use neural_amp_modeler_rs::common::params::RtProcessingParams;
use std::sync::atomic::Ordering;

impl<'a> NamClapProcessor<'a> {
    // ── Write helpers (Host Events path) ──────────────────────────

    pub(crate) fn set_input_gain(&mut self, db: f32) {
        self.params.input_gain_db = db;
        self.shared
            .ui_to_rt
            .param_input_gain
            .store(db.to_bits(), Ordering::Relaxed);
        self.smoother_in
            .set_target(self.gain_lut.db_to_linear(db + self.mod_input_gain));
    }

    pub(crate) fn set_output_gain(&mut self, db: f32) {
        self.params.output_gain_db = db;
        self.shared
            .ui_to_rt
            .param_output_gain
            .store(db.to_bits(), Ordering::Relaxed);
        self.smoother_out
            .set_target(self.gain_lut.db_to_linear(db + self.mod_output_gain));
    }

    pub(crate) fn set_gate_threshold(&mut self, db: f32) {
        self.params.gate_threshold_db = db;
        self.shared
            .ui_to_rt
            .param_gate_thresh
            .store(db.to_bits(), Ordering::Relaxed);
        self.gate_dirty = true;
    }

    pub(crate) fn set_bypass(&mut self, val: f32) {
        let bypass = bypass_f32_to_bool(val);
        self.params.bypass = bypass;
        self.shared
            .ui_to_rt
            .param_bypass
            .store(bypass_bool_to_u32(bypass), Ordering::Relaxed);
    }

    pub(crate) fn set_adaptive_compute(&mut self, val: f32) {
        let mode = neural_amp_modeler_rs::common::params::AdaptiveComputeMode::from_f32(val);
        self.params.adaptive_compute = mode;
        self.shared
            .ui_to_rt
            .param_adaptive_compute
            .store(mode as u32, Ordering::Relaxed);
        self.adaptive_compute.set_mode(mode, &self.rt_status);
    }

    pub(crate) fn set_slim_override(&mut self, val: f32) {
        let ov = neural_amp_modeler_rs::dsp::adaptive::SlimOverride::from_f32(val);
        self.params.slim_override = ov;
        self.shared
            .ui_to_rt
            .param_slim_override
            .store(ov as u32, Ordering::Relaxed);
        self.adaptive_compute.set_slim_override(ov);
    }

    pub(crate) fn set_oversample(&mut self, val: f32) {
        let factor = neural_amp_modeler_rs::dsp::oversample::OversampleFactor::from_f32(val);
        if factor != self.params.oversample {
            self.params.oversample = factor;
            self.shared
                .ui_to_rt
                .param_oversample
                .store(factor.to_f32() as u32, Ordering::Relaxed);
            self.apply_oversample(factor);
        }
    }

    pub(crate) fn set_activation(&mut self, val: f32) {
        let mode = neural_amp_modeler_rs::common::params::ActivationPrecision::from_f32(val);
        if mode != self.params.activation_precision {
            self.params.activation_precision = mode;
            self.shared
                .ui_to_rt
                .param_activation
                .store(mode as u32, Ordering::Relaxed);
            neural_amp_modeler_rs::math::activations::set_activation_tls(mode);
        }
    }

    // ── SPSC full-apply ───────────────────────────────────────────

    // ── SPSC full-apply ───────────────────────────────────────────

    pub(crate) fn apply_oversample(
        &mut self,
        factor: neural_amp_modeler_rs::dsp::oversample::OversampleFactor,
    ) {
        // CLAP latency policy — while the plugin is active,
        // structural changes that alter latency must request a host restart
        // and defer the rebuild to the next `activate()`.
        let buffer_size = self.shared.cold.buffer_size.load(Ordering::Relaxed);
        if buffer_size > 0 {
            self.shared
                .cold
                .pending_restart_os_factor
                .store(factor.to_f32() as u32, Ordering::Release);
            self.host.request_restart();
        } else {
            // Plugin is not active yet — safe to flag the main thread for
            // an immediate rebuild (allocate off-RT, deliver via SPSC).
            self.rt_status
                .requested_os_factor
                .store(factor.to_f32() as u32, Ordering::Relaxed);
            self.rt_status
                .set_flag_release(neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_OS_REBUILD);
        }
    }

    pub(super) fn apply_params_from_spsc(&mut self, new_params: RtProcessingParams) {
        let adaptive_changed = self.params.adaptive_compute != new_params.adaptive_compute;
        let slim_override_changed = self.params.slim_override != new_params.slim_override;
        let oversample_changed = self.params.oversample != new_params.oversample;
        let activation_changed =
            self.params.activation_precision != new_params.activation_precision;
        let gate_changed = self.params.gate_threshold_db != new_params.gate_threshold_db;
        self.params = new_params;
        self.smoother_in.set_target(
            self.gain_lut
                .db_to_linear(self.params.input_gain_db + self.mod_input_gain),
        );
        self.smoother_out.set_target(
            self.gain_lut
                .db_to_linear(self.params.output_gain_db + self.mod_output_gain),
        );
        if adaptive_changed {
            self.adaptive_compute
                .set_mode(self.params.adaptive_compute, &self.rt_status);
        }
        if slim_override_changed {
            self.adaptive_compute
                .set_slim_override(self.params.slim_override);
        }
        if oversample_changed {
            self.apply_oversample(self.params.oversample);
        }
        if activation_changed {
            neural_amp_modeler_rs::math::activations::set_activation_tls(
                self.params.activation_precision,
            );
        }
        if gate_changed {
            self.gate_dirty = true;
        }
    }

    // ── GUI sync helpers ──────────────────────────────────────────

    pub(super) fn sync_input_gain_from_gui(&mut self) {
        let shared_db = f32::from_bits(
            self.shared
                .ui_to_rt
                .param_input_gain
                .load(Ordering::Relaxed),
        );
        if shared_db != self.params.input_gain_db {
            self.params.input_gain_db = shared_db;
            self.smoother_in
                .set_target(self.gain_lut.db_to_linear(shared_db + self.mod_input_gain));
        }
    }

    pub(super) fn sync_output_gain_from_gui(&mut self) {
        let shared_db = f32::from_bits(
            self.shared
                .ui_to_rt
                .param_output_gain
                .load(Ordering::Relaxed),
        );
        if shared_db != self.params.output_gain_db {
            self.params.output_gain_db = shared_db;
            self.smoother_out
                .set_target(self.gain_lut.db_to_linear(shared_db + self.mod_output_gain));
        }
    }

    pub(super) fn sync_gate_thresh_from_gui(&mut self) {
        let shared_db = f32::from_bits(
            self.shared
                .ui_to_rt
                .param_gate_thresh
                .load(Ordering::Relaxed),
        );
        if shared_db != self.params.gate_threshold_db {
            self.params.gate_threshold_db = shared_db;
            self.gate_dirty = true;
        }
    }

    pub(super) fn sync_bypass_from_gui(&mut self) {
        let shared_bypass =
            bypass_u32_to_bool(self.shared.ui_to_rt.param_bypass.load(Ordering::Relaxed));
        if shared_bypass != self.params.bypass {
            self.params.bypass = shared_bypass;
        }
    }

    pub(super) fn sync_adaptive_compute_from_gui(&mut self) {
        let shared_adaptive = neural_amp_modeler_rs::common::params::AdaptiveComputeMode::from_f32(
            self.shared
                .ui_to_rt
                .param_adaptive_compute
                .load(Ordering::Relaxed) as f32,
        );
        if shared_adaptive != self.params.adaptive_compute {
            self.params.adaptive_compute = shared_adaptive;
            self.adaptive_compute
                .set_mode(shared_adaptive, &self.rt_status);
        }
    }

    pub(super) fn sync_slim_override_from_gui(&mut self) {
        let shared_override = neural_amp_modeler_rs::dsp::adaptive::SlimOverride::from_f32(
            self.shared
                .ui_to_rt
                .param_slim_override
                .load(Ordering::Relaxed) as f32,
        );
        if shared_override != self.params.slim_override {
            self.params.slim_override = shared_override;
            self.adaptive_compute.set_slim_override(shared_override);
        }
    }

    pub(super) fn sync_oversample_from_gui(&mut self) {
        let shared_factor = neural_amp_modeler_rs::dsp::oversample::OversampleFactor::from_f32(
            self.shared
                .ui_to_rt
                .param_oversample
                .load(Ordering::Relaxed) as f32,
        );
        if shared_factor != self.params.oversample {
            self.params.oversample = shared_factor;
            self.apply_oversample(shared_factor);
        }
    }

    pub(super) fn sync_activation_from_gui(&mut self) {
        let shared_mode = neural_amp_modeler_rs::common::params::ActivationPrecision::from_f32(
            self.shared
                .ui_to_rt
                .param_activation
                .load(Ordering::Relaxed) as f32,
        );
        if shared_mode != self.params.activation_precision {
            self.params.activation_precision = shared_mode;
            neural_amp_modeler_rs::math::activations::set_activation_tls(shared_mode);
        }
    }
}
