// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Parameter synchronization helpers for `NamClapProcessor`.
//!
//! Extracts the triplicated logic of updating `self.params.*`,
//! `self.shared.ui_to_rt.*`, `smoother`, and `gate_dirty` from the
//! three event-processing paths (SPSC, Host Events, GUI sync).
//!
//! The full-snapshot apply (`apply_params_from_spsc`) and the oversample
//! latency policy (`apply_oversample`) are defined once on
//! [`ClapCommandSwapHandler`] — the command-swap handler — so the ring drain
//! and the host-event/GUI paths share a single definition; the processor
//! methods here are thin split-view adapters.

use super::NamClapProcessor;
use crate::clap::extensions::gate_mode::GateMode;
use crate::clap::extensions::params::{
    PARAM_ACTIVATION, PARAM_ADAPTIVE_COMPUTE, PARAM_BYPASS, PARAM_CAB_BYPASS,
    PARAM_CALIBRATE_INPUT, PARAM_GATE_MODE, PARAM_GATE_THRESH, PARAM_INPUT_CALIBRATION_LEVEL,
    PARAM_INPUT_GAIN, PARAM_OUTPUT_GAIN, PARAM_OUTPUT_MODE, PARAM_OVERSAMPLE, PARAM_SLIM_OVERRIDE,
    bypass_u32_to_bool, sanitize_param_value,
};
use crate::clap::plugin::PendingRestartOs;
use crate::clap::processor::dsp::orchestrator::worker::ParamTarget;
use crate::clap::processor::events::ClapCommandSwapHandler;
use neural_amp_modeler_rs::common::params::RtProcessingParams;
use std::sync::atomic::Ordering;

impl<'a> NamClapProcessor<'a> {
    #[inline]
    pub(crate) fn param_target(&mut self) -> ParamTarget<'_> {
        ParamTarget {
            params: &mut self.params,
            cab_bypass: &mut self.cab_bypass,
            cal_config: &mut self.cal_config,
            model_cal_meta: &self.model_cal_meta,
            model_input_mult_adj: &mut self.model_input_mult_adj,
            model_output_mult_adj: &mut self.model_output_mult_adj,
            smoother_in: &mut self.smoother_in,
            smoother_out: &mut self.smoother_out,
            smoother_cal_in: &mut self.smoother_cal_in,
            smoother_cal_out: &mut self.smoother_cal_out,
            gate_dirty: &mut self.gate_dirty,
            gate_mode: &mut self.gate_mode,
            musical_gate: &mut self.musical_gate,
            mod_input_gain: &mut self.mod_input_gain,
            mod_output_gain: &mut self.mod_output_gain,
            mod_gate_thresh: &mut self.mod_gate_thresh,
            adaptive_compute: &mut self.adaptive_compute,
            rt_status: &self.rt_status,
            ui_to_rt: &self.shared.ui_to_rt,
            gain_lut: self.gain_lut,
            buffer_size: self.shared.cold.buffer_size.load(Ordering::Relaxed),
            pending_restart_os_factor: &self.shared.cold.pending_restart_os_factor,
            restart_wanted: &self.shared.cold.restart_wanted,
            render_mode: &self.shared.cold.render_mode,
            realtime_activation: &mut self.realtime_activation,
            silence_hyst: &mut self.silence_hyst,
        }
    }

    /// Issues at most one `request_restart()` for an oversampling change
    /// latched during this callback (or already pending before it began).
    ///
    /// The latch is published by every writer of
    /// `pending_restart_os_factor` while active and consumed here with a
    /// single `swap`, so GUI sync and the SPSC drain — both of which run
    /// before the DSP block — can no longer be masked by a before/after
    /// comparison inside the same callback.
    #[inline]
    pub(crate) fn maybe_request_restart(&self) {
        // Fast path: no restart latched means zero RMW on the hot path. Only
        // when a writer published `restart_wanted` do we pay for the swap.
        if !self.shared.cold.restart_wanted.load(Ordering::Relaxed) {
            return;
        }
        if !self
            .shared
            .cold
            .restart_wanted
            .swap(false, Ordering::AcqRel)
        {
            return;
        }
        // Pair with the `Release` that latched the factor: consume the request
        // first, then read the factor it refers to with `Acquire`.
        let pending = PendingRestartOs::load(
            &self.shared.cold.pending_restart_os_factor,
            Ordering::Acquire,
        );
        if pending != PendingRestartOs::None {
            self.host.request_restart();
        }
    }

    // ── Write helpers (Host Events path) ──────────────────────────

    /// Sets the input gain in decibels.
    ///
    /// Sanitizes the value against `PARAM_INPUT_GAIN` range ([-20.0, +20.0] dB),
    /// stores the updated value in processor params, publishes the raw float bits
    /// to `ui_to_rt.param_input_gain` using `Ordering::Relaxed` for UI telemetry,
    /// and recalculates the linear input smoother target via `gain_lut` including
    /// any active host parameter modulation.
    pub(crate) fn set_input_gain(&mut self, db: f32) {
        let _ = self.param_target().apply(PARAM_INPUT_GAIN, db, false, true);
    }

    /// Sets the output gain in decibels.
    ///
    /// Sanitizes the value against `PARAM_OUTPUT_GAIN` range ([-40.0, +40.0] dB),
    /// stores the updated value in processor params, publishes the raw float bits
    /// to `ui_to_rt.param_output_gain` using `Ordering::Relaxed` for UI telemetry,
    /// and recalculates the linear output smoother target via `gain_lut` including
    /// any active host parameter modulation.
    pub(crate) fn set_output_gain(&mut self, db: f32) {
        let _ = self
            .param_target()
            .apply(PARAM_OUTPUT_GAIN, db, false, true);
    }

    /// Sets the noise gate threshold in decibels.
    ///
    /// Sanitizes the value against `PARAM_GATE_THRESH` range ([-100.0, 0.0] dB),
    /// stores the updated value, publishes to `ui_to_rt.param_gate_thresh` via
    /// `Ordering::Relaxed`, and marks `gate_dirty = true` to trigger lazy
    /// recalculation of gate envelope coefficients and linear threshold.
    pub(crate) fn set_gate_threshold(&mut self, db: f32) {
        let _ = self
            .param_target()
            .apply(PARAM_GATE_THRESH, db, false, true);
    }

    /// Sets the plugin processing bypass state.
    ///
    /// Sanitizes the float representation (0.0 = active, 1.0 = bypassed), converts
    /// to boolean, updates processor params, and stores the atomic state in
    /// `ui_to_rt.param_bypass` with `Ordering::Relaxed`.
    pub(crate) fn set_bypass(&mut self, val: f32) {
        let _ = self.param_target().apply(PARAM_BYPASS, val, false, true);
    }

    /// Sets the independent cabinet bypass state (`PARAM_CAB_BYPASS`).
    pub(crate) fn set_cab_bypass(&mut self, val: f32) {
        let _ = self
            .param_target()
            .apply(PARAM_CAB_BYPASS, val, false, true);
    }

    /// Sets the adaptive compute mode.
    ///
    /// Sanitizes the float representation, parses the `AdaptiveComputeMode` enum,
    /// publishes the enum integer to `ui_to_rt.param_adaptive_compute` with
    /// `Ordering::Relaxed`, and updates the adaptive compute controller while
    /// binding real-time status reporting.
    pub(crate) fn set_adaptive_compute(&mut self, val: f32) {
        let _ = self
            .param_target()
            .apply(PARAM_ADAPTIVE_COMPUTE, val, false, true);
    }

    /// Sets the slimmable neural model layer override.
    ///
    /// Sanitizes the float value, parses the `SlimOverride` enum (`Auto`, `Full`,
    /// `Half`, `Quarter`), publishes to `ui_to_rt.param_slim_override` with
    /// `Ordering::Relaxed`, and updates the controller.
    pub(crate) fn set_slim_override(&mut self, val: f32) {
        let _ = self
            .param_target()
            .apply(PARAM_SLIM_OVERRIDE, val, false, true);
    }

    /// Sets the DSP oversampling factor (`Off`, `2x`, `4x`).
    pub(crate) fn set_oversample(&mut self, val: f32) {
        let _ = self
            .param_target()
            .apply(PARAM_OVERSAMPLE, val, false, true);
    }

    /// Sets the neural network activation function precision (`Fast` or `Accurate`).
    ///
    /// Sanitizes the float value and converts to `ActivationPrecision`. The
    /// user choice is stored in `realtime_activation`; the effective precision
    /// applied to the DSP is `Standard` while rendering offline, so automation
    /// can never change an offline render's arithmetic.
    pub(crate) fn set_activation(&mut self, val: f32) {
        let _ = self
            .param_target()
            .apply(PARAM_ACTIVATION, val, false, true);
    }

    /// Enables or disables input level calibration prior to the neural model.
    pub(crate) fn set_calibrate_input(&mut self, val: f32) {
        let _ = self
            .param_target()
            .apply(PARAM_CALIBRATE_INPUT, val, false, true);
    }

    /// Sets the converter 0 dBFS input calibration reference level in dBu (`[-60, 60]`).
    pub(crate) fn set_input_calibration_level(&mut self, val: f32) {
        let _ = self
            .param_target()
            .apply(PARAM_INPUT_CALIBRATION_LEVEL, val, false, true);
    }

    /// Sets the neural amp model output compensation mode (`Raw`, `Normalized`, `Calibrated`).
    pub(crate) fn set_output_mode(&mut self, val: f32) {
        let _ = self
            .param_target()
            .apply(PARAM_OUTPUT_MODE, val, false, true);
    }

    /// Sets the noise gate operating mode (`Silence` legacy vs `Musical` expander).
    pub(crate) fn set_gate_mode(&mut self, val: f32) {
        let _ = self.param_target().apply(PARAM_GATE_MODE, val, false, true);
    }

    // ── GUI sync helpers ──────────────────────────────────────────

    /// Synchronizes input gain parameter from the GUI/UI thread.
    ///
    /// Atomically loads the shared float bits via `Ordering::Relaxed`, sanitizes the
    /// value against `PARAM_INPUT_GAIN`, and if changed, updates local params and
    /// retargets the input smoother including active modulation.
    pub(super) fn sync_input_gain_from_gui(&mut self) {
        let raw_db = f32::from_bits(
            self.shared
                .ui_to_rt
                .param_input_gain
                .load(Ordering::Relaxed),
        );
        let shared_db = sanitize_param_value(PARAM_INPUT_GAIN, raw_db);
        let target = self.gain_lut.db_to_linear(shared_db + self.mod_input_gain);
        if shared_db != self.params.input_gain_db
            || (self.smoother_in.target_value() - target).abs() > 1e-6
        {
            let _ = self
                .param_target()
                .apply_synced(PARAM_INPUT_GAIN, shared_db, false, true);
        }
    }

    /// Synchronizes output gain parameter from the GUI/UI thread.
    ///
    /// Atomically loads the shared float bits via `Ordering::Relaxed`, sanitizes the
    /// value against `PARAM_OUTPUT_GAIN`, and if changed, updates local params and
    /// retargets the output smoother including active modulation.
    pub(super) fn sync_output_gain_from_gui(&mut self) {
        let raw_db = f32::from_bits(
            self.shared
                .ui_to_rt
                .param_output_gain
                .load(Ordering::Relaxed),
        );
        let shared_db = sanitize_param_value(PARAM_OUTPUT_GAIN, raw_db);
        let target = self.gain_lut.db_to_linear(shared_db + self.mod_output_gain);
        if shared_db != self.params.output_gain_db
            || (self.smoother_out.target_value() - target).abs() > 1e-6
        {
            let _ = self
                .param_target()
                .apply_synced(PARAM_OUTPUT_GAIN, shared_db, false, true);
        }
    }

    /// Synchronizes noise gate threshold from the GUI/UI thread.
    ///
    /// Atomically loads the shared float bits via `Ordering::Relaxed`, sanitizes the
    /// value against `PARAM_GATE_THRESH`, and if changed, updates local params and
    /// flags `gate_dirty = true` for lazy coefficient recalculation.
    pub(super) fn sync_gate_thresh_from_gui(&mut self) {
        let raw_db = f32::from_bits(
            self.shared
                .ui_to_rt
                .param_gate_thresh
                .load(Ordering::Relaxed),
        );
        let shared_db = sanitize_param_value(PARAM_GATE_THRESH, raw_db);
        if shared_db != self.params.gate_threshold_db {
            let _ = self
                .param_target()
                .apply_synced(PARAM_GATE_THRESH, shared_db, false, true);
        }
    }

    /// Synchronizes bypass state from the GUI/UI thread.
    ///
    /// Atomically loads the bypass flag via `Ordering::Relaxed` and updates local
    /// params if changed.
    pub(super) fn sync_bypass_from_gui(&mut self) {
        let shared_bypass =
            bypass_u32_to_bool(self.shared.ui_to_rt.param_bypass.load(Ordering::Relaxed));
        if shared_bypass != self.params.bypass {
            let _ = self.param_target().apply_synced(
                PARAM_BYPASS,
                if shared_bypass { 1.0 } else { 0.0 },
                false,
                true,
            );
        }
    }

    /// Synchronizes independent cabinet bypass state from the GUI/UI thread.
    pub(super) fn sync_cab_bypass_from_gui(&mut self) {
        let shared_bypass = bypass_u32_to_bool(
            self.shared
                .ui_to_rt
                .param_cab_bypass
                .load(Ordering::Relaxed),
        );
        if shared_bypass != self.cab_bypass {
            let _ = self.param_target().apply_synced(
                PARAM_CAB_BYPASS,
                if shared_bypass { 1.0 } else { 0.0 },
                false,
                true,
            );
        }
    }

    /// Synchronizes adaptive compute mode from the GUI/UI thread.
    ///
    /// Atomically loads the mode integer via `Ordering::Relaxed`, sanitizes the value,
    /// and if changed, updates local params and configures the controller mode.
    pub(super) fn sync_adaptive_compute_from_gui(&mut self) {
        let raw = self
            .shared
            .ui_to_rt
            .param_adaptive_compute
            .load(Ordering::Relaxed) as f32;
        let sanitized = sanitize_param_value(PARAM_ADAPTIVE_COMPUTE, raw);
        let shared_adaptive =
            neural_amp_modeler_rs::common::params::AdaptiveComputeMode::from_f32(sanitized);
        if shared_adaptive != self.params.adaptive_compute
            || shared_adaptive != self.adaptive_compute.mode()
        {
            let _ =
                self.param_target()
                    .apply_synced(PARAM_ADAPTIVE_COMPUTE, sanitized, false, true);
        }
    }

    /// Synchronizes slimmable neural model override from the GUI/UI thread.
    ///
    /// Atomically loads the override integer via `Ordering::Relaxed`, sanitizes the value,
    /// and if changed, updates local params and the controller override.
    pub(super) fn sync_slim_override_from_gui(&mut self) {
        let raw = self
            .shared
            .ui_to_rt
            .param_slim_override
            .load(Ordering::Relaxed) as f32;
        let sanitized = sanitize_param_value(PARAM_SLIM_OVERRIDE, raw);
        let shared_override =
            neural_amp_modeler_rs::dsp::adaptive::SlimOverride::from_f32(sanitized);
        if shared_override != self.params.slim_override
            || shared_override != self.adaptive_compute.slim_override()
        {
            let _ = self
                .param_target()
                .apply_synced(PARAM_SLIM_OVERRIDE, sanitized, false, true);
        }
    }

    /// Synchronizes oversampling factor from the GUI/UI thread.
    ///
    /// Atomically loads the oversample factor integer via `Ordering::Relaxed`, sanitizes
    /// the value, and if changed, applies the factor under the CLAP latency restart policy.
    pub(super) fn sync_oversample_from_gui(&mut self) {
        let raw = self
            .shared
            .ui_to_rt
            .param_oversample
            .load(Ordering::Relaxed) as f32;
        let sanitized = sanitize_param_value(PARAM_OVERSAMPLE, raw);
        let shared_factor =
            neural_amp_modeler_rs::dsp::oversample::OversampleFactor::from_f32(sanitized);
        if shared_factor != self.params.oversample {
            let _ = self
                .param_target()
                .apply_synced(PARAM_OVERSAMPLE, sanitized, false, true);
        }
    }

    /// Synchronizes activation function precision from the GUI/UI thread.
    ///
    /// Atomically loads the activation mode integer via `Ordering::Relaxed`, sanitizes
    /// the value, and if changed, updates local params and TLS activation mode.
    pub(super) fn sync_activation_from_gui(&mut self) {
        let raw = self
            .shared
            .ui_to_rt
            .param_activation
            .load(Ordering::Relaxed) as f32;
        let sanitized = sanitize_param_value(PARAM_ACTIVATION, raw);
        let shared_mode =
            neural_amp_modeler_rs::common::params::ActivationPrecision::from_f32(sanitized);
        if shared_mode != self.realtime_activation {
            let _ = self
                .param_target()
                .apply_synced(PARAM_ACTIVATION, sanitized, false, true);
        }
    }

    /// Synchronizes input calibration and output mode configuration from `UiToRt`.
    pub(super) fn sync_calibration_from_gui(&mut self) {
        let shared_cal = self.shared.calibration_config();
        if shared_cal.calibrate_input != self.cal_config.calibrate_input {
            let _ = self.param_target().apply_synced(
                PARAM_CALIBRATE_INPUT,
                if shared_cal.calibrate_input { 1.0 } else { 0.0 },
                false,
                true,
            );
        }
        if shared_cal.input_calibration_level_dbu != self.cal_config.input_calibration_level_dbu {
            let _ = self.param_target().apply_synced(
                PARAM_INPUT_CALIBRATION_LEVEL,
                shared_cal.input_calibration_level_dbu,
                false,
                true,
            );
        }
        if shared_cal.output_mode != self.cal_config.output_mode {
            let _ = self.param_target().apply_synced(
                PARAM_OUTPUT_MODE,
                shared_cal.output_mode.to_f32(),
                false,
                true,
            );
        }
    }

    /// Synchronizes noise gate mode from `UiToRt`.
    pub(super) fn sync_gate_mode_from_gui(&mut self) {
        let shared_mode = self.shared.gate_mode();
        if shared_mode != self.gate_mode {
            let _ = self.param_target().apply_synced(
                PARAM_GATE_MODE,
                shared_mode.to_f32(),
                false,
                true,
            );
        }
    }

    /// Synchronizes all parameters changed via GUI from shared UI-to-RT atomics,
    /// retargeting smoothers, flagging gate recalculation, and updating controllers.
    ///
    /// This method is idempotent and called by both `process_events` and `flush_inner`.
    pub(crate) fn sync_params_from_gui(&mut self) {
        self.sync_input_gain_from_gui();
        self.sync_output_gain_from_gui();
        self.sync_gate_thresh_from_gui();
        self.sync_gate_mode_from_gui();
        self.sync_bypass_from_gui();
        self.sync_cab_bypass_from_gui();
        self.sync_adaptive_compute_from_gui();
        self.sync_slim_override_from_gui();
        self.sync_oversample_from_gui();
        self.sync_activation_from_gui();
        self.sync_calibration_from_gui();
    }

    /// Recomputes cached thresholds and musical gate state whenever gate settings change.
    #[inline]
    pub(crate) fn refresh_gate_config(&mut self) {
        if !self.gate_dirty {
            return;
        }
        let modulated_gate_db = self.params.gate_threshold_db + self.mod_gate_thresh;
        match self.gate_mode {
            GateMode::Silence => {
                let close_db = modulated_gate_db - 6.0;
                let open_linear = self.gain_lut.db_to_linear(modulated_gate_db);
                self.cached_threshold_open_sq = open_linear * open_linear;
                let close_linear = self.gain_lut.db_to_linear(close_db);
                self.cached_threshold_close_sq = close_linear * close_linear;
                self.cached_gate_params.threshold_open_db = modulated_gate_db;
                self.cached_gate_params.threshold_close_db = close_db;
            }
            GateMode::Musical => {
                // Keep Silence gate FSM permanently open so neural inference runs continuously
                // without block-energy dropouts; MusicalNoiseGate handles per-sample expansion.
                self.cached_threshold_open_sq = 0.0;
                self.cached_threshold_close_sq = 0.0;
                self.cached_gate_params.threshold_open_db = -90.0;
                self.cached_gate_params.threshold_close_db = -96.0;
                self.silence_hyst.reset();
                self.musical_gate
                    .set_threshold(modulated_gate_db as f64, modulated_gate_db > -90.0);
            }
        }
        self.gate_dirty = false;
    }
}

impl ClapCommandSwapHandler<'_, '_> {
    /// Applies an oversampling factor change under the CLAP latency policy.
    ///
    /// While the plugin is active, structural changes that alter latency must
    /// request a host restart and defer the rebuild to the next `activate()`.
    ///
    /// The pending request uses the `PendingRestartOs` encoding, so a pending
    /// transition *to Off* is representable (the `None` variant no longer
    /// collides with "no restart pending"). Coalescing is latest-wins: if a
    /// restart for the *same* target factor is already pending, the redundant
    /// `request_restart()` is skipped (exactly one restart per coalesced
    /// relevant transition).
    pub(super) fn apply_oversample(
        &mut self,
        factor: neural_amp_modeler_rs::dsp::oversample::OversampleFactor,
    ) {
        crate::clap::processor::dsp::orchestrator::worker::latch_oversample_restart(
            factor,
            self.shared.cold.buffer_size.load(Ordering::Relaxed),
            &self.shared.cold.pending_restart_os_factor,
            &self.shared.cold.restart_wanted,
            self.rt_status,
        );
    }

    /// Applies a full parameter snapshot delivered over the command ring
    /// (light `Params` payload) or as part of a restore transaction.
    ///
    /// Values are sanitized against the declared CLAP ranges; smoothers
    /// retarget sample-accurately (including the current modulation), and
    /// only the fields that actually changed trigger their side effects
    /// (adaptive mode, slim override, oversample policy, activation TLS,
    /// gate-coefficient invalidation).
    pub(super) fn apply_params_from_spsc(&mut self, mut new_params: RtProcessingParams) {
        new_params.input_gain_db = sanitize_param_value(PARAM_INPUT_GAIN, new_params.input_gain_db);
        new_params.output_gain_db =
            sanitize_param_value(PARAM_OUTPUT_GAIN, new_params.output_gain_db);
        new_params.gate_threshold_db =
            sanitize_param_value(PARAM_GATE_THRESH, new_params.gate_threshold_db);

        let adaptive_changed = self.params.adaptive_compute != new_params.adaptive_compute;
        let slim_override_changed = self.params.slim_override != new_params.slim_override;
        let oversample_changed = self.params.oversample != new_params.oversample;
        let activation_changed = *self.realtime_activation != new_params.activation_precision;
        let gate_changed = self.params.gate_threshold_db != new_params.gate_threshold_db;
        let requested_activation = new_params.activation_precision;
        *self.params = new_params;
        if activation_changed {
            *self.realtime_activation = requested_activation;
        }
        let effective_activation = crate::clap::processor::effective_activation(
            self.shared.cold.render_mode.load(Ordering::Acquire),
            *self.realtime_activation,
        );
        self.params.activation_precision = effective_activation;
        self.smoother_in.set_target(
            self.gain_lut
                .db_to_linear(self.params.input_gain_db + *self.mod_input_gain),
        );
        self.smoother_out.set_target(
            self.gain_lut
                .db_to_linear(self.params.output_gain_db + *self.mod_output_gain),
        );
        if adaptive_changed {
            self.adaptive_compute
                .set_mode(self.params.adaptive_compute, self.rt_status);
        }
        if slim_override_changed {
            self.adaptive_compute
                .set_slim_override(self.params.slim_override);
        }
        if oversample_changed {
            self.apply_oversample(self.params.oversample);
        }
        if activation_changed {
            neural_amp_modeler_rs::math::activations::set_activation_tls(effective_activation);
        }
        if gate_changed {
            *self.gate_dirty = true;
        }
    }
}
