// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! GUI interaction, parameter gesture tracking, atomic snapshots, and host
//! output event flushing on [`GuiSharedState`].

use super::GuiSharedState;
use super::state_blocks::{ColdShared, RtToUi, UiToRt};
use crate::clap::extensions::calibration::{CalibrationConfig, OutputMode};
use crate::clap::extensions::gate_mode::GateMode;
use crate::clap::extensions::params::{
    PARAM_ACTIVATION, PARAM_ADAPTIVE_COMPUTE, PARAM_BYPASS, PARAM_CAB_BYPASS,
    PARAM_CALIBRATE_INPUT, PARAM_GATE_MODE, PARAM_GATE_THRESH, PARAM_INPUT_CALIBRATION_LEVEL,
    PARAM_INPUT_GAIN, PARAM_OUTPUT_GAIN, PARAM_OUTPUT_MODE, PARAM_OVERSAMPLE, PARAM_SLIM_OVERRIDE,
};
use clack_plugin::events::event_types::{
    ParamGestureBeginEvent, ParamGestureEndEvent, ParamValueEvent,
};
use clack_plugin::prelude::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

/// Bit shift for the "parameter value changed" flag within a parameter's 3-bit slot in `gesture_flags`.
pub const GESTURE_CHANGED_SHIFT: u32 = 0;
/// Bit shift for the "gesture begin" flag within a parameter's 3-bit slot in `gesture_flags`.
pub const GESTURE_BEGIN_SHIFT: u32 = 1;
/// Bit shift for the "gesture end" flag within a parameter's 3-bit slot in `gesture_flags`.
pub const GESTURE_END_SHIFT: u32 = 2;
/// Number of bits reserved per parameter in `gesture_flags` (`changed | begin | end`).
pub const GESTURE_BITS_PER_PARAM: u32 = 3;

/// Sentinel for [`ColdShared::ui_last_interaction_ms`](super::ColdShared::ui_last_interaction_ms):
/// no GUI interaction has been recorded yet, which the telemetry tick treats as
/// idle (nothing has interacted within the last cadence-downgrade window).
pub const UI_NO_INTERACTION: u64 = u64::MAX;

/// Process-start anchor for the GUI interaction clock ([`ui_monotonic_ms`]).
static UI_EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Monotonic milliseconds since this process' first GUI interaction timestamp,
/// the clock behind [`ColdShared::ui_last_interaction_ms`].
pub(crate) fn ui_monotonic_ms() -> u64 {
    UI_EPOCH
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
}

impl GuiSharedState {
    /// Bit shift for the "parameter value changed" flag in `gesture_flags`.
    pub const GESTURE_CHANGED_SHIFT: u32 = GESTURE_CHANGED_SHIFT;
    /// Bit shift for the "gesture begin" flag in `gesture_flags`.
    pub const GESTURE_BEGIN_SHIFT: u32 = GESTURE_BEGIN_SHIFT;
    /// Bit shift for the "gesture end" flag in `gesture_flags`.
    pub const GESTURE_END_SHIFT: u32 = GESTURE_END_SHIFT;
    /// Number of bits reserved per parameter in `gesture_flags`.
    pub const GESTURE_BITS_PER_PARAM: u32 = GESTURE_BITS_PER_PARAM;

    /// Maps a CLAP `param_id` (`0..PARAM_COUNT`) to its internal slot index.
    pub const fn param_index(param_id: u32) -> usize {
        param_id as usize
    }

    /// Records a user interaction with the GUI.
    pub fn note_ui_interaction(&self) {
        self.cold
            .ui_last_interaction_ms
            .store(ui_monotonic_ms(), Ordering::Relaxed);
    }

    /// Sets a begin gesture flag for the given CLAP param_id.
    pub fn begin_gesture(&self, param_id: u32) {
        self.note_ui_interaction();
        self.set_gesture(Self::param_index(param_id), Self::GESTURE_BEGIN_SHIFT);
    }

    /// Sets an end gesture flag for the given CLAP param_id.
    pub fn end_gesture(&self, param_id: u32) {
        self.note_ui_interaction();
        self.set_gesture(Self::param_index(param_id), Self::GESTURE_END_SHIFT);
    }

    /// Sets a changed flag for the given CLAP param_id.
    pub fn mark_param_changed(&self, param_id: u32) {
        self.note_ui_interaction();
        self.set_gesture(Self::param_index(param_id), Self::GESTURE_CHANGED_SHIFT);
    }

    /// Sets a gesture flag for the parameter (store = true).
    pub fn set_gesture(&self, param_index: usize, flag_shift: u32) {
        let bit = 1u64 << (param_index as u32 * Self::GESTURE_BITS_PER_PARAM + flag_shift);
        self.ui_to_rt.gesture_flags.fetch_or(bit, Ordering::Relaxed);
    }

    /// Reads and clears a gesture flag (swap to false), returns the previous value.
    pub fn take_gesture(&self, param_index: usize, flag_shift: u32) -> bool {
        let bit = 1u64 << (param_index as u32 * Self::GESTURE_BITS_PER_PARAM + flag_shift);
        (self
            .ui_to_rt
            .gesture_flags
            .fetch_and(!bit, Ordering::Relaxed)
            & bit)
            != 0
    }

    /// Reads the current [`CalibrationConfig`] snapshot from `ui_to_rt` atomics.
    pub fn calibration_config(&self) -> CalibrationConfig {
        let calibrate_input = self.ui_to_rt.param_calibrate_input.load(Ordering::Relaxed) != 0;
        let input_calibration_level_dbu = f32::from_bits(
            self.ui_to_rt
                .param_input_calibration_level
                .load(Ordering::Relaxed),
        );
        let output_mode =
            OutputMode::from_u32(self.ui_to_rt.param_output_mode.load(Ordering::Relaxed));
        CalibrationConfig {
            calibrate_input,
            input_calibration_level_dbu,
            output_mode,
        }
        .sanitized()
    }

    /// Stores `cfg` into `ui_to_rt` atomics.
    pub fn store_calibration_config(&self, cfg: CalibrationConfig) {
        let cfg = cfg.sanitized();
        self.ui_to_rt
            .param_calibrate_input
            .store(u32::from(cfg.calibrate_input), Ordering::Relaxed);
        self.ui_to_rt
            .param_input_calibration_level
            .store(cfg.input_calibration_level_dbu.to_bits(), Ordering::Relaxed);
        self.ui_to_rt
            .param_output_mode
            .store(cfg.output_mode.to_u32(), Ordering::Relaxed);
    }

    /// Reads the current [`GateMode`] snapshot from `ui_to_rt` atomics.
    pub fn gate_mode(&self) -> GateMode {
        GateMode::from_u32(self.ui_to_rt.param_gate_mode.load(Ordering::Relaxed))
    }

    /// Stores `mode` into `ui_to_rt` atomics.
    pub fn store_gate_mode(&self, mode: GateMode) {
        self.ui_to_rt
            .param_gate_mode
            .store(mode.to_u32(), Ordering::Relaxed);
    }

    /// Reads the current independent cabinet bypass state from `ui_to_rt` atomics.
    pub fn cab_bypass(&self) -> bool {
        self.ui_to_rt.param_cab_bypass.load(Ordering::Relaxed) != 0
    }

    /// Stores `bypass` into `ui_to_rt` atomics.
    pub fn store_cab_bypass(&self, bypass: bool) {
        self.ui_to_rt
            .param_cab_bypass
            .store(u32::from(bypass), Ordering::Relaxed);
    }

    /// Zeros out all gesture flags.
    pub fn clear_gestures(&self) {
        self.ui_to_rt.gesture_flags.store(0, Ordering::Relaxed);
    }

    /// Bumps the GUI parameter generation counter (Release ordering).
    /// Signals to the RT thread that GUI-initiated parameter values may have changed.
    pub fn bump_generation(&self) {
        self.ui_to_rt
            .gui_param_generation
            .fetch_add(1, Ordering::Release); // pairs with Acquire loads in processor/events, extensions/params/audio.rs
    }

    /// Returns whether the model file dialog is currently active.
    pub fn is_model_dialog_active(&self) -> bool {
        self.cold
            .dialog_state
            .as_ref()
            .map(|d| d.active.load(Ordering::Relaxed))
            .unwrap_or(false)
    }

    /// Sets whether the model file dialog is currently active.
    pub fn set_model_dialog_active(&self, active: bool) {
        if let Some(d) = self.cold.dialog_state.as_ref() {
            d.active.store(active, Ordering::Relaxed);
        }
    }

    /// Creates a new `GuiSharedState` instance with default production/test sub-structs.
    pub fn new(instance_id: u64) -> Self {
        Self {
            rt_to_ui: RtToUi::default(),
            ui_to_rt: UiToRt::default(),
            cold: ColdShared::new(instance_id),
        }
    }

    /// Test helper creating an `Arc<GuiSharedState>` initialized with valid channels,
    /// dialog states, and a pre-configured sample rate (44100 Hz).
    pub fn new_test() -> Arc<Self> {
        let state = Self::new(1);
        state.cold.sample_rate.store(44100, Ordering::Relaxed);
        Arc::new(state)
    }

    /// Flushes gestures and parameter updates initiated by the GUI
    /// into the host's output event queue (fail-closed).
    ///
    /// # Fail-closed contract
    ///
    /// If `output.try_push` fails (host output queue full), the corresponding
    /// gesture bit is **retained** for retry on the next `flush`/`process`
    /// instead of being silently consumed. Draining stops at the first failure
    /// so the legal order `begin → value → end` is preserved for every
    /// parameter. Every failure raises `RT_STATUS_GUI_EVENT_BACKPRESSURE` so
    /// saturation is observable off-RT.
    pub fn write_gui_events(&self, output: &mut OutputEvents) {
        // Fast path for the common headless/steady case — a single Relaxed load
        // avoids locked RMW `fetch_and` operations per callback when no GUI
        // gesture occurred.
        if self.ui_to_rt.gesture_flags.load(Ordering::Relaxed) == 0 {
            return;
        }

        let params: [(u32, &AtomicU32, bool); 13] = [
            (PARAM_INPUT_GAIN, &self.ui_to_rt.param_input_gain, true),
            (PARAM_OUTPUT_GAIN, &self.ui_to_rt.param_output_gain, true),
            (PARAM_GATE_THRESH, &self.ui_to_rt.param_gate_thresh, true),
            (PARAM_BYPASS, &self.ui_to_rt.param_bypass, false),
            (
                PARAM_ADAPTIVE_COMPUTE,
                &self.ui_to_rt.param_adaptive_compute,
                false,
            ),
            (
                PARAM_SLIM_OVERRIDE,
                &self.ui_to_rt.param_slim_override,
                false,
            ),
            (PARAM_OVERSAMPLE, &self.ui_to_rt.param_oversample, false),
            (PARAM_ACTIVATION, &self.ui_to_rt.param_activation, false),
            (
                PARAM_CALIBRATE_INPUT,
                &self.ui_to_rt.param_calibrate_input,
                false,
            ),
            (
                PARAM_INPUT_CALIBRATION_LEVEL,
                &self.ui_to_rt.param_input_calibration_level,
                true,
            ),
            (PARAM_OUTPUT_MODE, &self.ui_to_rt.param_output_mode, false),
            (PARAM_GATE_MODE, &self.ui_to_rt.param_gate_mode, false),
            (PARAM_CAB_BYPASS, &self.ui_to_rt.param_cab_bypass, false),
        ];

        for &(param_id, value_atomic, is_f32_bits) in &params {
            let pi = Self::param_index(param_id);

            if self.take_gesture(pi, Self::GESTURE_BEGIN_SHIFT) {
                let ev = ParamGestureBeginEvent::new(0, ClapId::new(param_id));
                if output.try_push(ev).is_err() {
                    self.set_gesture(pi, Self::GESTURE_BEGIN_SHIFT);
                    self.note_gui_backpressure();
                    return;
                }
            }
            if self.take_gesture(pi, Self::GESTURE_CHANGED_SHIFT) {
                let raw = value_atomic.load(Ordering::Relaxed);
                let val = if is_f32_bits {
                    f32::from_bits(raw) as f64
                } else {
                    raw as f64
                };
                let ev = ParamValueEvent::new(
                    0,
                    ClapId::new(param_id),
                    clack_plugin::events::Pckn::new(0u8, 0u8, 0u8, 0u8),
                    val,
                );
                if output.try_push(ev).is_err() {
                    self.set_gesture(pi, Self::GESTURE_CHANGED_SHIFT);
                    self.note_gui_backpressure();
                    return;
                }
            }
            if self.take_gesture(pi, Self::GESTURE_END_SHIFT) {
                let ev = ParamGestureEndEvent::new(0, ClapId::new(param_id));
                if output.try_push(ev).is_err() {
                    self.set_gesture(pi, Self::GESTURE_END_SHIFT);
                    self.note_gui_backpressure();
                    return;
                }
            }
        }
    }

    /// Records GUI→host output backpressure as an RT-safe telemetry flag.
    /// Consumed and logged off-RT by `emit_pending_logs()` (housekeeping).
    #[inline(always)]
    fn note_gui_backpressure(&self) {
        self.cold
            .rt_status
            .set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_GUI_EVENT_BACKPRESSURE);
    }
}
