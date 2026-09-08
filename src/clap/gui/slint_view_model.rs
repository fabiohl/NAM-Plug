// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Slint View-Model: connects Slint UI callbacks and telemetry to `GuiSharedState`
//! and CLAP host parameter flush.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use clack_extensions::params::HostParams;
use slint::ComponentHandle;

use crate::clap::extensions::params::{
    PARAM_ACTIVATION, PARAM_BYPASS, PARAM_GATE_THRESH, PARAM_INPUT_GAIN, PARAM_OUTPUT_GAIN,
    PARAM_OVERSAMPLE,
};
use crate::clap::gui::GuiHostBridge;
use crate::clap::gui::file_dialogs::{spawn_file_dialog, spawn_ir_file_dialog};
use crate::clap::plugin::GuiSharedState;

slint::include_modules!();

/// Ballistics decay multiplier per 60 Hz frame (~20 dB drop in 1.7 s: 0.10^(1/102) ≈ 0.978,
/// normalized to ~0.962 for responsive analog VU needle release).
pub const BALLISTICS_DECAY_RATE: f32 = 0.962;

/// Peak hold duration in 60 Hz frames (120 frames = 2.0 seconds).
pub const PEAK_HOLD_FRAMES: u32 = 120;

/// Lower dB floor for meter display.
pub const METER_MIN_DB: f32 = -60.0;

/// Upper dB headroom ceiling for meter display.
pub const METER_MAX_DB: f32 = 6.0;

/// Maps a linear amplitude value to a normalized [0.0, 1.0] fraction using logarithmic dB scaling.
#[inline]
pub fn amp_to_meter_frac(amp: f32) -> f32 {
    let db = if amp > 1e-5 {
        20.0 * amp.log10()
    } else {
        METER_MIN_DB
    };
    ((db - METER_MIN_DB) / (METER_MAX_DB - METER_MIN_DB)).clamp(0.0, 1.0)
}

#[inline]
fn format_gain_text(val: f32) -> String {
    let rounded = (val * 10.0).round() / 10.0;
    if val > 0.0 {
        format!("+{rounded:.1} dB")
    } else {
        format!("{rounded:.1} dB")
    }
}

#[inline]
fn format_gate_text(val: f32) -> String {
    if val <= -90.0 {
        "OFF".to_string()
    } else {
        format!("{} dB", val.round() as i32)
    }
}

#[inline]
fn flush_params(host: Option<&GuiHostBridge>) {
    if let Some(bridge) = host {
        let host_static = bridge.as_static();
        if let Some(params_ext) = host_static.get_extension::<HostParams>() {
            params_ext.request_flush(&host_static);
        }
    }
}

/// Internal mutable state for the Slint view model.
pub(crate) struct SlintViewModelInner {
    window: MainWindow,
    shared: Arc<GuiSharedState>,
    _host: Option<GuiHostBridge>,

    // Parameter caches to prevent feedback loops and redundant UI sets
    last_input_gain: f32,
    last_output_gain: f32,
    last_gate_thresh: f32,
    last_bypass: bool,
    last_oversampling: u32,
    last_activation: u32,
    last_model_name: String,
    last_ir_path: Option<String>,

    // Ballistics state
    current_peak_l: f32,
    current_peak_r: f32,
    hold_peak_l: f32,
    hold_peak_r: f32,
    hold_timer_l: u32,
    hold_timer_r: u32,
}

impl SlintViewModelInner {
    /// Polls RT telemetry, updates IEC 60268-10 ballistics, and synchronizes external
    /// parameter/model/IR updates without echo loops.
    pub fn tick_telemetry(&mut self) {
        // ── 1. Peak Telemetry & IEC 60268-10 Type I Ballistics ────────────────
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

        // Left channel ballistics
        if raw_l > self.current_peak_l {
            self.current_peak_l = raw_l;
        } else {
            self.current_peak_l = (self.current_peak_l * BALLISTICS_DECAY_RATE).max(raw_l);
        }
        if raw_l >= self.hold_peak_l {
            self.hold_peak_l = raw_l;
            self.hold_timer_l = PEAK_HOLD_FRAMES;
        } else if self.hold_timer_l > 0 {
            self.hold_timer_l -= 1;
        } else {
            self.hold_peak_l = (self.hold_peak_l * BALLISTICS_DECAY_RATE).max(raw_l);
        }

        // Right channel ballistics
        if raw_r > self.current_peak_r {
            self.current_peak_r = raw_r;
        } else {
            self.current_peak_r = (self.current_peak_r * BALLISTICS_DECAY_RATE).max(raw_r);
        }
        if raw_r >= self.hold_peak_r {
            self.hold_peak_r = raw_r;
            self.hold_timer_r = PEAK_HOLD_FRAMES;
        } else if self.hold_timer_r > 0 {
            self.hold_timer_r -= 1;
        } else {
            self.hold_peak_r = (self.hold_peak_r * BALLISTICS_DECAY_RATE).max(raw_r);
        }

        self.window
            .set_peak_l(amp_to_meter_frac(self.current_peak_l));
        self.window
            .set_peak_r(amp_to_meter_frac(self.current_peak_r));
        self.window
            .set_peak_hold_l(amp_to_meter_frac(self.hold_peak_l));
        self.window
            .set_peak_hold_r(amp_to_meter_frac(self.hold_peak_r));

        // ── 2. Clipping & Gate Indicators ─────────────────────────────────────
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

        // ── 3. Parameter Echo-Loop Prevention (Sync from RT/Host -> UI) ───────
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

        let bypass = self.shared.ui_to_rt.param_bypass.load(Ordering::Relaxed) != 0;
        if bypass != self.window.get_bypass_active() {
            self.last_bypass = bypass;
            self.window.set_bypass_active(bypass);
        }

        let os = self
            .shared
            .ui_to_rt
            .param_oversample
            .load(Ordering::Relaxed);
        if os as i32 != self.window.get_oversample_mode() {
            self.last_oversampling = os;
            self.window.set_oversample_mode(os as i32);
            self.window.set_oversample_text(match os {
                1 => "2x (HQ)".into(),
                2 => "4x (Ultra)".into(),
                _ => "1x (Off)".into(),
            });
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

        // ── 4. Model Card & IR Card Status ────────────────────────────────────
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
            let current_name = if name_guard.is_empty() {
                "No model loaded"
            } else {
                name_guard.as_str()
            };
            if self.last_model_name != current_name {
                self.last_model_name = current_name.to_string();
                self.window.set_model_name(current_name.into());
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

        if let Ok(ir_guard) = self.shared.cold.ir_path.try_lock() {
            let has_ir = ir_guard.is_some();
            if has_ir != self.window.get_has_ir() {
                self.window.set_has_ir(has_ir);
            }
            if let Some(ref path_str) = *ir_guard {
                let name = std::path::Path::new(path_str)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(path_str.as_str());
                if self.last_ir_path.as_deref() != Some(name) {
                    self.last_ir_path = Some(name.to_string());
                    self.window.set_ir_name(name.into());
                }
            } else if self.last_ir_path.is_some() {
                self.last_ir_path = None;
                self.window.set_ir_name("No IR loaded".into());
            }
        }

        // ── 5. Status Bar Telemetry ───────────────────────────────────────────
        let sr = self.shared.cold.sample_rate.load(Ordering::Relaxed);
        if sr > 0 {
            let sr_text = format!("{sr} Hz");
            if self.window.get_sample_rate_text() != sr_text.as_str() {
                self.window.set_sample_rate_text(sr_text.into());
            }
        }

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
        let total_lat = stream_lat + cab_lat;
        let ms = if sr > 0 {
            (total_lat as f32 * 1000.0) / sr as f32
        } else {
            0.0
        };
        let lat_text = format!("{total_lat} spl ({ms:.1} ms)");
        if self.window.get_latency_text() != lat_text.as_str() {
            self.window.set_latency_text(lat_text.into());
        }
    }
}

/// View-Model coordinating Slint GUI events, parameter gestures, and telemetry synchronization.
pub struct SlintViewModel {
    inner: Rc<RefCell<SlintViewModelInner>>,
    _timer: slint::Timer,
}

impl SlintViewModel {
    /// Creates a new `SlintViewModel`, binding all Slint callbacks to `GuiSharedState`
    /// and starting the 60 Hz telemetry polling timer.
    pub fn new(
        window: MainWindow,
        shared: Arc<GuiSharedState>,
        host: Option<GuiHostBridge>,
    ) -> Self {
        log::info!("SlintViewModel: Initializing Slint UI bindings and telemetry loop");

        let initial_in_gain =
            f32::from_bits(shared.ui_to_rt.param_input_gain.load(Ordering::Relaxed));
        let initial_out_gain =
            f32::from_bits(shared.ui_to_rt.param_output_gain.load(Ordering::Relaxed));
        let initial_gate_th =
            f32::from_bits(shared.ui_to_rt.param_gate_thresh.load(Ordering::Relaxed));
        let initial_bypass = shared.ui_to_rt.param_bypass.load(Ordering::Relaxed) != 0;
        let initial_os = shared.ui_to_rt.param_oversample.load(Ordering::Relaxed);
        let initial_act = shared.ui_to_rt.param_activation.load(Ordering::Relaxed);

        window.set_input_gain_db(initial_in_gain);
        window.set_input_gain_text(format_gain_text(initial_in_gain).into());
        window.set_output_gain_db(initial_out_gain);
        window.set_output_gain_text(format_gain_text(initial_out_gain).into());
        window.set_gate_thresh_db(initial_gate_th);
        window.set_gate_thresh_text(format_gate_text(initial_gate_th).into());
        window.set_bypass_active(initial_bypass);
        window.set_oversample_mode(initial_os as i32);
        window.set_oversample_text(match initial_os {
            1 => "2x (HQ)".into(),
            2 => "4x (Ultra)".into(),
            _ => "1x (Off)".into(),
        });
        window.set_activation_mode(initial_act as i32);

        // ── Bind Input Gain Callbacks ─────────────────────────────────────────
        {
            let shared = Arc::clone(&shared);
            window.on_input_gain_gesture_begin(move || {
                shared.begin_gesture(PARAM_INPUT_GAIN);
                flush_params(host.as_ref());
            });
        }
        {
            let shared = Arc::clone(&shared);
            window.on_input_gain_changed(move |val| {
                shared
                    .ui_to_rt
                    .param_input_gain
                    .store(val.to_bits(), Ordering::Relaxed);
                shared.bump_generation();
                shared.mark_param_changed(PARAM_INPUT_GAIN);
                flush_params(host.as_ref());
            });
        }
        {
            let shared = Arc::clone(&shared);
            window.on_input_gain_gesture_end(move || {
                shared.end_gesture(PARAM_INPUT_GAIN);
                flush_params(host.as_ref());
            });
        }

        // ── Bind Output Gain Callbacks ────────────────────────────────────────
        {
            let shared = Arc::clone(&shared);
            window.on_output_gain_gesture_begin(move || {
                shared.begin_gesture(PARAM_OUTPUT_GAIN);
                flush_params(host.as_ref());
            });
        }
        {
            let shared = Arc::clone(&shared);
            window.on_output_gain_changed(move |val| {
                shared
                    .ui_to_rt
                    .param_output_gain
                    .store(val.to_bits(), Ordering::Relaxed);
                shared.bump_generation();
                shared.mark_param_changed(PARAM_OUTPUT_GAIN);
                flush_params(host.as_ref());
            });
        }
        {
            let shared = Arc::clone(&shared);
            window.on_output_gain_gesture_end(move || {
                shared.end_gesture(PARAM_OUTPUT_GAIN);
                flush_params(host.as_ref());
            });
        }

        // ── Bind Gate Threshold Callbacks ─────────────────────────────────────
        {
            let shared = Arc::clone(&shared);
            window.on_gate_thresh_gesture_begin(move || {
                shared.begin_gesture(PARAM_GATE_THRESH);
                flush_params(host.as_ref());
            });
        }
        {
            let shared = Arc::clone(&shared);
            window.on_gate_thresh_changed(move |val| {
                shared
                    .ui_to_rt
                    .param_gate_thresh
                    .store(val.to_bits(), Ordering::Relaxed);
                shared.bump_generation();
                shared.mark_param_changed(PARAM_GATE_THRESH);
                flush_params(host.as_ref());
            });
        }
        {
            let shared = Arc::clone(&shared);
            window.on_gate_thresh_gesture_end(move || {
                shared.end_gesture(PARAM_GATE_THRESH);
                flush_params(host.as_ref());
            });
        }

        // ── Bind Master Bypass Callback ───────────────────────────────────────
        {
            let shared = Arc::clone(&shared);
            let window_weak = window.as_weak();
            window.on_toggle_bypass(move || {
                let prev = shared.ui_to_rt.param_bypass.load(Ordering::Relaxed) != 0;
                let next = !prev;
                shared
                    .ui_to_rt
                    .param_bypass
                    .store(if next { 1 } else { 0 }, Ordering::Relaxed);
                shared.begin_gesture(PARAM_BYPASS);
                shared.mark_param_changed(PARAM_BYPASS);
                shared.end_gesture(PARAM_BYPASS);
                shared.bump_generation();
                flush_params(host.as_ref());
                if let Some(w) = window_weak.upgrade() {
                    w.set_bypass_active(next);
                }
            });
        }

        // ── Bind Oversampling Controls ────────────────────────────────────────
        {
            let shared = Arc::clone(&shared);
            window.on_oversample_changed(move |val| {
                let mode = (val as u32).min(2);
                shared
                    .ui_to_rt
                    .param_oversample
                    .store(mode, Ordering::Relaxed);
                shared.begin_gesture(PARAM_OVERSAMPLE);
                shared.mark_param_changed(PARAM_OVERSAMPLE);
                shared.end_gesture(PARAM_OVERSAMPLE);
                shared.bump_generation();
                flush_params(host.as_ref());
            });
        }
        {
            let shared = Arc::clone(&shared);
            let window_weak = window.as_weak();
            window.on_cycle_oversample(move || {
                let cur = shared.ui_to_rt.param_oversample.load(Ordering::Relaxed);
                let next = (cur + 1) % 3;
                shared
                    .ui_to_rt
                    .param_oversample
                    .store(next, Ordering::Relaxed);
                shared.begin_gesture(PARAM_OVERSAMPLE);
                shared.mark_param_changed(PARAM_OVERSAMPLE);
                shared.end_gesture(PARAM_OVERSAMPLE);
                shared.bump_generation();
                flush_params(host.as_ref());
                if let Some(w) = window_weak.upgrade() {
                    w.set_oversample_mode(next as i32);
                    w.set_oversample_text(match next {
                        1 => "2x (HQ)".into(),
                        2 => "4x (Ultra)".into(),
                        _ => "1x (Off)".into(),
                    });
                }
            });
        }

        // ── Bind Activation Precision Control ─────────────────────────────────
        {
            let shared = Arc::clone(&shared);
            window.on_activation_changed(move |val| {
                let mode = (val as u32).min(1);
                shared
                    .ui_to_rt
                    .param_activation
                    .store(mode, Ordering::Relaxed);
                shared.begin_gesture(PARAM_ACTIVATION);
                shared.mark_param_changed(PARAM_ACTIVATION);
                shared.end_gesture(PARAM_ACTIVATION);
                shared.bump_generation();
                flush_params(host.as_ref());
            });
        }

        // ── Bind Clip Reset Callback ──────────────────────────────────────────
        {
            let shared = Arc::clone(&shared);
            let window_weak = window.as_weak();
            window.on_reset_clip_clicked(move || {
                shared
                    .rt_to_ui
                    .ui_clip_indicator
                    .store(false, Ordering::Relaxed);
                shared.rt_to_ui.ui_clipped.store(false, Ordering::Relaxed);
                if let Some(w) = window_weak.upgrade() {
                    w.set_clip_active(false);
                }
            });
        }

        // ── Bind Model File Dialog (XDG Portal via rfd) ───────────────────────
        {
            let shared = Arc::clone(&shared);
            let window_weak = window.as_weak();
            window.on_load_model_clicked(move || {
                if let Some(dialog_state_arc) = shared.cold.dialog_state.as_ref()
                    && !shared.cold.ui_loading.load(Ordering::Relaxed)
                    && !dialog_state_arc.active.load(Ordering::Relaxed)
                {
                    log::info!("SlintViewModel: Triggering Model file picker (rfd / XDG Portal)");
                    shared.cold.ui_loading.store(true, Ordering::Relaxed);
                    dialog_state_arc.active.store(true, Ordering::Relaxed);
                    if let Some(w) = window_weak.upgrade() {
                        w.set_model_loading(true);
                    }
                    if let Some(bridge) = host {
                        let host_static = bridge.as_static();
                        let handle = spawn_file_dialog(
                            Arc::clone(dialog_state_arc),
                            host_static,
                            Arc::clone(&shared.cold.alive_fence),
                        );
                        if let Ok(mut guard) = shared.cold.dialog_handle_sink.lock() {
                            *guard = Some(handle);
                        }
                    }
                }
            });
        }

        // ── Bind IR File Dialog (XDG Portal via rfd) ──────────────────────────
        {
            let shared = Arc::clone(&shared);
            let window_weak = window.as_weak();
            window.on_load_ir_clicked(move || {
                if let Some(ir_dialog_state_arc) = shared.cold.ir_dialog_state.as_ref()
                    && !shared.cold.ui_ir_loading.load(Ordering::Relaxed)
                    && !ir_dialog_state_arc.active.load(Ordering::Relaxed)
                {
                    log::info!("SlintViewModel: Triggering IR file picker (rfd / XDG Portal)");
                    shared.cold.ui_ir_loading.store(true, Ordering::Relaxed);
                    ir_dialog_state_arc.active.store(true, Ordering::Relaxed);
                    if let Some(w) = window_weak.upgrade() {
                        w.set_ir_loading(true);
                    }
                    if let Some(bridge) = host {
                        let host_static = bridge.as_static();
                        let handle = spawn_ir_file_dialog(
                            Arc::clone(ir_dialog_state_arc),
                            host_static,
                            Arc::clone(&shared.cold.alive_fence),
                        );
                        if let Ok(mut guard) = shared.cold.ir_dialog_handle_sink.lock() {
                            *guard = Some(handle);
                        }
                    }
                }
            });
        }

        // ── Bind Clear IR Callback ────────────────────────────────────────────
        {
            let shared = Arc::clone(&shared);
            let window_weak = window.as_weak();
            window.on_clear_ir_clicked(move || {
                log::info!("SlintViewModel: Requesting IR CabSim clear");
                shared.cold.ui_clear_ir.store(true, Ordering::Relaxed);
                if let Some(bridge) = host
                    && shared.cold.alive_fence.load(Ordering::Acquire)
                {
                    let host_static = bridge.as_static();
                    host_static.request_callback();
                }
                if let Some(w) = window_weak.upgrade() {
                    w.set_has_ir(false);
                    w.set_ir_name("No IR loaded".into());
                }
            });
        }

        let inner = Rc::new(RefCell::new(SlintViewModelInner {
            window,
            shared,
            _host: host,
            last_input_gain: initial_in_gain,
            last_output_gain: initial_out_gain,
            last_gate_thresh: initial_gate_th,
            last_bypass: initial_bypass,
            last_oversampling: initial_os,
            last_activation: initial_act,
            last_model_name: String::new(),
            last_ir_path: None,
            current_peak_l: 0.0,
            current_peak_r: 0.0,
            hold_peak_l: 0.0,
            hold_peak_r: 0.0,
            hold_timer_l: 0,
            hold_timer_r: 0,
        }));

        // Start 60 Hz Telemetry Timer (16 ms)
        let timer = slint::Timer::default();
        let inner_clone = Rc::clone(&inner);
        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(16),
            move || {
                inner_clone.borrow_mut().tick_telemetry();
            },
        );

        Self {
            inner,
            _timer: timer,
        }
    }

    /// Explicitly ticks telemetry once (useful for tests and manual render loops).
    pub fn tick_telemetry(&mut self) {
        self.inner.borrow_mut().tick_telemetry();
    }

    /// Returns a strong clone of the Slint window handle.
    pub fn window(&self) -> MainWindow {
        self.inner.borrow().window.clone_strong()
    }
}
