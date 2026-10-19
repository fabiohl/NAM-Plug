// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Slint View-Model: connects Slint UI callbacks and telemetry to `GuiSharedState`
//! and CLAP host parameter flush.

// `slint::include_modules!` materializes machine-generated UI code whose own
// `unwrap()` calls are out of scope for the hand-written production unwrap
// hygiene enforced crate-wide (F-PR-13). Hand-written `expect()` in this file
// remains subject to the `expect_used` lint.
#![expect(clippy::unwrap_used)]

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use clack_extensions::params::HostParams;
use neural_amp_modeler_rs::math::common::{InstructionSet, SIMD_MATH};
use slint::ComponentHandle;

use crate::clap::extensions::params::{
    PARAM_ACTIVATION, PARAM_BYPASS, PARAM_GATE_THRESH, PARAM_INPUT_GAIN, PARAM_OUTPUT_GAIN,
    PARAM_OVERSAMPLE,
};
use crate::clap::gui::GuiHostBridge;
use crate::clap::gui::file_dialogs::{spawn_file_dialog, spawn_ir_file_dialog};
use crate::clap::plugin::GuiSharedState;
use crate::clap::plugin::lock::lock_cold;
use crate::clap::plugin::shared::{UI_NO_INTERACTION, ui_monotonic_ms};

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

/// Formats the loaded model's architecture/topology for the status bar
/// (Finding F6): reads real `NamModelMetadata`, never a hard-coded placeholder.
#[inline]
fn format_model_arch(meta: &crate::clap::plugin::NamModelMetadata) -> String {
    if meta.topology.is_empty() {
        meta.architecture.clone()
    } else {
        format!("{} ({})", meta.architecture, meta.topology)
    }
}

/// Full-rate telemetry period (60 Hz) — the rate at which the IEC 60268-10
/// ballistics ([`BALLISTICS_DECAY_RATE`], [`PEAK_HOLD_FRAMES`]) are calibrated.
pub const TELEMETRY_PERIOD_FULL: std::time::Duration = std::time::Duration::from_millis(16);

/// Reduced telemetry period (~30 Hz), used only in the silence state (F-UX-08).
pub const TELEMETRY_PERIOD_LOW: std::time::Duration = std::time::Duration::from_millis(33);

/// Minimum time without GUI interaction before the telemetry cadence may drop
/// to [`TELEMETRY_PERIOD_LOW`] (F-UX-08). A recorded interaction restarts the
/// window; an unrecorded one ([`UI_NO_INTERACTION`]) counts as idle.
pub const TELEMETRY_LOW_CADENCE_IDLE: std::time::Duration = std::time::Duration::from_secs(2);
/// Silence threshold for the adaptive cadence: peaks below −90 dBFS map to a
/// 0.0 meter fraction (the display floors at −60 dBFS), so polling them slower
/// cannot change the ballistics behavior.
const SILENCE_PEAK_AMP: f32 = 3.162_277_7e-5; // 10^(−90/20)

/// Formats the status-bar sample-rate text (e.g. `"48 kHz"`, `"44.1 kHz"`).
/// Only called when the sample rate changes (F-UX-08); `sr` must be > 0.
fn format_sample_rate_text(sr: u32) -> String {
    if sr.is_multiple_of(1000) {
        format!("{} kHz", sr / 1000)
    } else {
        format!("{:.1} kHz", sr as f32 / 1000.0)
    }
}

/// Formats the status-bar buffer-size text (e.g. `"64 spl"`, `"— spl"`).
fn format_buffer_size_text(buf: u32) -> String {
    if buf > 0 {
        format!("{buf} spl")
    } else {
        "— spl".to_string()
    }
}

/// Formats the status-bar latency text (e.g. `"128 spl (2.7 ms)"`).
///
/// The millisecond figure depends on both the total latency and the sample
/// rate, so the cache republishes it when either changes.
fn format_latency_text(total_lat: u32, sr: u32) -> String {
    let ms = if sr > 0 {
        (total_lat as f32 * 1000.0) / sr as f32
    } else {
        0.0
    };
    format!("{total_lat} spl ({ms:.1} ms)")
}

/// Formats the status-bar channel-count text (`"Mono"` / `"Stereo"`).
fn format_channels_text(channels: u32) -> &'static str {
    match channels {
        1 => "Mono",
        _ => "Stereo",
    }
}

/// Destination for the four status-bar texts (F-UX-08 test seam).
///
/// [`MainWindow`] publishes them to the Slint window; tests implement a
/// counting wrapper to assert that [`StatusBarCache`] publishes only when the
/// underlying integers change.
pub trait StatusTextSink {
    fn publish_sample_rate_text(&self, text: &str);
    fn publish_buffer_size_text(&self, text: &str);
    fn publish_latency_text(&self, text: &str);
    fn publish_channels_text(&self, text: &str);
}

impl StatusTextSink for MainWindow {
    fn publish_sample_rate_text(&self, text: &str) {
        self.set_sample_rate_text(text.into());
    }
    fn publish_buffer_size_text(&self, text: &str) {
        self.set_buffer_size_text(text.into());
    }
    fn publish_latency_text(&self, text: &str) {
        self.set_latency_text(text.into());
    }
    fn publish_channels_text(&self, text: &str) {
        self.set_channels_text(text.into());
    }
}

/// Integer cache behind the status-bar texts (F-UX-08).
///
/// The telemetry tick used to `format!` four `String`s every 16 ms and compare
/// them *after* allocating. This cache stores the last published integers and
/// formats/publishes only when one of them changes, so a steady state performs
/// zero allocations and zero `set_*_text` calls.
///
/// The cache mirrors what is already on screen: [`StatusBarCache::new`] seeds
/// it with the values `SlintViewModel::new` published at construction, so the
/// first tick with unchanged inputs publishes nothing.
pub struct StatusBarCache {
    last_sr: u32,
    last_buf: u32,
    last_total_lat: u32,
    last_channels: u32,
}

impl StatusBarCache {
    /// Seeds the cache with the values already published to the window.
    pub fn new(sample_rate: u32, buffer_size: u32, total_lat: u32, channels: u32) -> Self {
        Self {
            last_sr: sample_rate,
            last_buf: buffer_size,
            last_total_lat: total_lat,
            last_channels: channels,
        }
    }

    /// Publishes the status-bar texts whose backing integers changed since the
    /// last call. Nothing is formatted and no `set_*_text` is invoked for
    /// unchanged values.
    ///
    /// The latency text depends on the sample rate as well (its millisecond
    /// figure), so a sample-rate change republishes it even when the sample
    /// count is unchanged.
    pub fn update(
        &mut self,
        sample_rate: u32,
        buffer_size: u32,
        total_lat: u32,
        channels: u32,
        sink: &impl StatusTextSink,
    ) {
        let sr_changed = sample_rate != self.last_sr;
        if sr_changed {
            self.last_sr = sample_rate;
            if sample_rate > 0 {
                sink.publish_sample_rate_text(&format_sample_rate_text(sample_rate));
            }
        }
        if buffer_size != self.last_buf {
            self.last_buf = buffer_size;
            sink.publish_buffer_size_text(&format_buffer_size_text(buffer_size));
        }
        if sr_changed || total_lat != self.last_total_lat {
            self.last_total_lat = total_lat;
            sink.publish_latency_text(&format_latency_text(total_lat, sample_rate));
        }
        if channels != self.last_channels {
            self.last_channels = channels;
            sink.publish_channels_text(format_channels_text(channels));
        }
    }
}

/// Drives the telemetry timer at the current cadence (F-UX-08).
///
/// The `slint::Timer` is bound to the GUI/event-loop thread, so arming always
/// happens through this helper (also called from within the timer callback
/// itself to change cadence — Slint supports restarting a timer from its own
/// callback). The callback holds a [`Weak`] reference so the timer never keeps
/// the view model alive.
fn arm_telemetry_timer(inner: &Rc<RefCell<SlintViewModelInner>>, period: std::time::Duration) {
    let weak = Rc::downgrade(inner);
    inner
        .borrow()
        .timer
        .start(slint::TimerMode::Repeated, period, move || {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let desired = inner.borrow_mut().tick_telemetry();
            if let Some(next_period) = desired
                && inner.borrow().telemetry_active
            {
                arm_telemetry_timer(&inner, next_period);
            }
        });
}

/// Suspends/resumes the telemetry timer (F-UX-08).
///
/// This is the single entry point behind `MainWindow`'s `set_telemetry_active`
/// callback (dispatched by the CLAP `gui.show()`/`gui.hide()` paths) and the
/// public [`SlintViewModel::set_telemetry_active`]. Resuming also restarts the
/// interaction clock, so the cadence stays at 60 Hz for a full idle window
/// after the window is shown.
fn apply_telemetry_active(inner: &Rc<RefCell<SlintViewModelInner>>, active: bool) {
    let period = {
        let mut guard = inner.borrow_mut();
        guard.telemetry_active = active;
        if !active {
            guard.timer.stop();
            return;
        }
        guard.shared.note_ui_interaction();
        guard.cadence
    };
    arm_telemetry_timer(inner, period);
}

/// Human-readable label for the runtime-detected SIMD backend actually
/// dispatching the neural inference math (Finding F6). `Avx2` is the sole
/// production backend unless the crate's opt-in `avx512` feature is enabled.
#[inline]
#[expect(
    deprecated,
    reason = "Avx512VnniBf16 must stay handled for match exhaustiveness"
)]
fn simd_badge_text(isa: InstructionSet) -> &'static str {
    match isa {
        InstructionSet::Avx2 => "AVX2+FMA",
        InstructionSet::Avx512 => "AVX-512",
        InstructionSet::Avx512VnniBf16 => "AVX-512 VNNI+BF16",
        _ => "UNKNOWN",
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
    last_model_arch: String,
    last_ir_path: Option<String>,
    last_dsp_load_pct: Option<u32>,
    last_seen_xrun_epoch: u32,
    xrun_hold_frames: u32,

    // Ballistics state
    current_peak_l: f32,
    current_peak_r: f32,
    hold_peak_l: f32,
    hold_peak_r: f32,
    hold_timer_l: u32,
    hold_timer_r: u32,

    // Status-bar text cache (F-UX-08): formats/publishes only on change.
    status_bar: StatusBarCache,

    // Adaptive telemetry timer (F-UX-08): suspended while the window is hidden.
    timer: slint::Timer,
    telemetry_active: bool,
    /// Period the timer runs at while [`Self::telemetry_active`] — 60 Hz, or
    /// 30 Hz in the silence state. Updated by [`Self::evaluate_cadence`].
    cadence: std::time::Duration,
}

impl SlintViewModelInner {
    /// Polls RT telemetry, updates IEC 60268-10 ballistics, and synchronizes external
    /// parameter/model/IR updates without echo loops.
    ///
    /// Returns `Some(new_period)` when the adaptive telemetry cadence (F-UX-08)
    /// changed and the timer must be re-armed; `None` to keep it running.
    /// The ballistics above run identically on every call regardless of cadence.
    pub fn tick_telemetry(&mut self) -> Option<std::time::Duration> {
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

        // ── 2b. DSP Load & XRun Indicator (T-R2.2) ───────────────────────────
        let permille = self
            .shared
            .rt_to_ui
            .dsp_load_permille
            .load(Ordering::Relaxed);
        let pct = permille as u32 / 10;
        if self.last_dsp_load_pct != Some(pct) {
            self.last_dsp_load_pct = Some(pct);
            self.window.set_dsp_load_text(format!("{pct}% DSP").into());
        }

        let xrun_epoch = self.shared.rt_to_ui.xrun_epoch.load(Ordering::Relaxed);
        if xrun_epoch != self.last_seen_xrun_epoch {
            self.last_seen_xrun_epoch = xrun_epoch;
            self.xrun_hold_frames = PEAK_HOLD_FRAMES; // 120 frames = 2.0 s @ 60 Hz
        }

        let xrun_active = self.xrun_hold_frames > 0;
        if xrun_active {
            self.xrun_hold_frames -= 1;
        }
        if xrun_active != self.window.get_xrun_active() {
            self.window.set_xrun_active(xrun_active);
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
            let has_model = !name_guard.is_empty();
            let current_name = if has_model {
                name_guard.as_str()
            } else {
                "No model loaded"
            };
            if self.window.get_has_model() != has_model {
                self.window.set_has_model(has_model);
            }
            if self.last_model_name != current_name {
                self.last_model_name = current_name.to_string();
                self.window.set_model_name(current_name.into());
            }
        }

        // Model architecture/topology badge (Finding F6): mirrors the
        // `ui_model_name` diff pattern above so the status card never shows a
        // stale architecture after a model swap or clear.
        if let Ok(meta_guard) = self.shared.cold.ui_model_metadata.try_lock() {
            let current_arch = match meta_guard.as_ref() {
                Some(meta) => format_model_arch(meta),
                None => "No model loaded".to_string(),
            };
            if self.last_model_arch != current_arch {
                self.last_model_arch = current_arch.clone();
                self.window.set_model_arch(current_arch.into());
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

        // ── 5. Status Bar Telemetry (cached — F-UX-08) ────────────────────────
        // Formats and publishes the four status-bar texts only when their
        // backing integers change: a steady state performs zero allocations and
        // zero `set_*_text` calls (see `StatusBarCache`).
        let sr = self.shared.cold.sample_rate.load(Ordering::Relaxed);
        let buf = self.shared.cold.buffer_size.load(Ordering::Relaxed);
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
        self.status_bar
            .update(sr, buf, stream_lat + cab_lat, channels, &self.window);

        // ── 5b. GUI Alert Pill Evaluation (T-R3.2) ──────────────────────────
        let rt = &self.shared.cold.rt_status;
        let mut effective_bits = self.shared.cold.ui_alert_bits.load(Ordering::Relaxed);
        if rt.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_PROCESSOR_POISONED) {
            effective_bits |= crate::clap::plugin::shared::UI_ALERT_POISONED;
        }
        if rt.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HOST_CONTRACT_VIOLATION) {
            effective_bits |= crate::clap::plugin::shared::UI_ALERT_HOST_CONTRACT;
        }
        if rt.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RESAMPLER_REBUILD_FAILED) {
            effective_bits |= crate::clap::plugin::shared::UI_ALERT_RESAMPLER_FAILED;
        }
        if rt.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_CABSIM_CONTRACT_VIOLATION) {
            effective_bits |= crate::clap::plugin::shared::UI_ALERT_CABSIM_CONTRACT;
        }
        if rt.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_DEGRADE_MINIMAL) {
            effective_bits |= crate::clap::plugin::shared::UI_ALERT_DEGRADE_MINIMAL;
        }
        if rt.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_DEGRADE_REDUCED) {
            effective_bits |= crate::clap::plugin::shared::UI_ALERT_DEGRADE_REDUCED;
        }
        if rt.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_NON_FINITE_INPUT_DETECTED) {
            effective_bits |= crate::clap::plugin::shared::UI_ALERT_NON_FINITE_INPUT;
        }

        let alert: Option<(&'static str, slint::Color)> = if (effective_bits
            & crate::clap::plugin::shared::UI_ALERT_POISONED)
            != 0
        {
            Some((
                "Plugin parado — reiniciando…",
                slint::Color::from_argb_encoded(0xFF_F7_4E_4E),
            ))
        } else if (effective_bits & crate::clap::plugin::shared::UI_ALERT_HOST_CONTRACT) != 0 {
            Some((
                "Bloco do host acima do limite",
                slint::Color::from_argb_encoded(0xFF_F7_4E_4E),
            ))
        } else if (effective_bits & crate::clap::plugin::shared::UI_ALERT_RESAMPLER_FAILED) != 0 {
            Some((
                "Falha ao reconstruir resampler",
                slint::Color::from_argb_encoded(0xFF_F7_4E_4E),
            ))
        } else if (effective_bits & crate::clap::plugin::shared::UI_ALERT_CABSIM_CONTRACT) != 0 {
            Some((
                "Violação de partição do CabSim",
                slint::Color::from_argb_encoded(0xFF_F7_4E_4E),
            ))
        } else if (effective_bits & crate::clap::plugin::shared::UI_ALERT_DEGRADE_MINIMAL) != 0 {
            Some((
                "Qualidade mínima: CPU alta",
                slint::Color::from_argb_encoded(0xFF_EB_CB_8B),
            ))
        } else if (effective_bits & crate::clap::plugin::shared::UI_ALERT_DEGRADE_REDUCED) != 0 {
            Some((
                "Qualidade reduzida: CPU alta",
                slint::Color::from_argb_encoded(0xFF_EB_CB_8B),
            ))
        } else if (effective_bits & crate::clap::plugin::shared::UI_ALERT_NON_FINITE_INPUT) != 0 {
            Some((
                "Entrada inválida (NaN) — reconvergindo",
                slint::Color::from_argb_encoded(0xFF_EB_CB_8B),
            ))
        } else {
            None
        };

        match alert {
            Some((text, color)) => {
                if !self.window.get_alert_active() {
                    self.window.set_alert_active(true);
                }
                self.window.set_alert_text(text.into());
                self.window.set_alert_color(color);
            }
            None => {
                if self.window.get_alert_active() {
                    self.window.set_alert_active(false);
                    self.window.set_alert_text("".into());
                }
            }
        }

        // ── 6. Adaptive Telemetry Cadence (F-UX-08) ──────────────────────────
        self.evaluate_cadence(raw_l, raw_r)
    }

    /// Chooses the telemetry cadence for the next interval (F-UX-08).
    ///
    /// Drops to [`TELEMETRY_PERIOD_LOW`] only in the silence state — every
    /// metered value at/below −90 dBFS (invisible on the −60 dBFS-floored
    /// display; a hold at/below the threshold stays visually zero regardless of
    /// its hold timer) — and only after
    /// [`TELEMETRY_LOW_CADENCE_IDLE`] without a GUI interaction. Any signal or
    /// interaction returns to [`TELEMETRY_PERIOD_FULL`] immediately, so the
    /// IEC 60268-10 ballistics always run at their calibrated 60 Hz frame rate
    /// while anything is visible.
    ///
    /// Returns `Some(new_period)` when the armed period must change.
    fn evaluate_cadence(&mut self, raw_l: f32, raw_r: f32) -> Option<std::time::Duration> {
        let last_interaction = self
            .shared
            .cold
            .ui_last_interaction_ms
            .load(Ordering::Relaxed);
        let idle = last_interaction == UI_NO_INTERACTION
            || ui_monotonic_ms().saturating_sub(last_interaction)
                >= TELEMETRY_LOW_CADENCE_IDLE.as_millis() as u64;
        let silent = raw_l < SILENCE_PEAK_AMP
            && raw_r < SILENCE_PEAK_AMP
            && self.current_peak_l < SILENCE_PEAK_AMP
            && self.current_peak_r < SILENCE_PEAK_AMP
            && self.hold_peak_l < SILENCE_PEAK_AMP
            && self.hold_peak_r < SILENCE_PEAK_AMP;
        let target = if idle && silent {
            TELEMETRY_PERIOD_LOW
        } else {
            TELEMETRY_PERIOD_FULL
        };
        if target != self.cadence {
            self.cadence = target;
            return Some(target);
        }
        None
    }
}

/// View-Model coordinating Slint GUI events, parameter gestures, and telemetry synchronization.
pub struct SlintViewModel {
    inner: Rc<RefCell<SlintViewModelInner>>,
}

impl SlintViewModel {
    /// Creates a new `SlintViewModel`, binding all Slint callbacks to `GuiSharedState`
    /// and starting the 60 Hz telemetry polling timer (F-UX-08: suspended with
    /// the window, adaptive to 30 Hz in the silence state).
    pub fn new(
        window: MainWindow,
        shared: Arc<GuiSharedState>,
        host: Option<GuiHostBridge>,
        backend_text: &str,
    ) -> Self {
        log::info!("SlintViewModel: Initializing Slint UI bindings and telemetry loop");

        // Opening the window is a UI event (F-UX-08): keep the telemetry
        // cadence at 60 Hz for a full idle window from here.
        shared.note_ui_interaction();

        // Static-for-the-window-lifetime badges (Finding F6): the negotiated
        // windowing backend and the runtime-detected SIMD instruction set
        // never change while this window is alive, so they are set once here
        // instead of every 60 Hz tick.
        window.set_backend_text(backend_text.into());
        window.set_simd_badge(simd_badge_text(SIMD_MATH.instruction_set).into());

        let initial_model_name = shared
            .cold
            .ui_model_name
            .try_lock()
            .ok()
            .and_then(|guard| {
                if guard.is_empty() {
                    None
                } else {
                    Some(guard.clone())
                }
            })
            .unwrap_or_else(|| "No model loaded".to_string());
        let initial_has_model = initial_model_name != "No model loaded";
        window.set_has_model(initial_has_model);
        window.set_model_name(initial_model_name.as_str().into());

        let initial_model_arch = shared
            .cold
            .ui_model_metadata
            .try_lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(format_model_arch))
            .unwrap_or_else(|| "No model loaded".to_string());
        window.set_model_arch(initial_model_arch.clone().into());

        let initial_has_ir = shared
            .cold
            .ir_path
            .try_lock()
            .ok()
            .map(|guard| guard.is_some())
            .unwrap_or(false);
        let initial_ir_name = shared
            .cold
            .ir_path
            .try_lock()
            .ok()
            .and_then(|guard| {
                guard.as_ref().map(|path| {
                    std::path::Path::new(path)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(path.as_str())
                        .to_string()
                })
            })
            .unwrap_or_else(|| "No IR loaded".to_string());
        window.set_has_ir(initial_has_ir);
        window.set_ir_name(initial_ir_name.into());

        // Status bar: published once here and then mirrored by `StatusBarCache`
        // (F-UX-08) — the tick formats/publishes these texts only on change.
        let initial_sr = shared.cold.sample_rate.load(Ordering::Relaxed);
        let initial_sr_text = if initial_sr > 0 {
            format_sample_rate_text(initial_sr)
        } else {
            "48 kHz".to_string()
        };
        window.set_sample_rate_text(initial_sr_text.into());

        let initial_buf = shared.cold.buffer_size.load(Ordering::Relaxed);
        window.set_buffer_size_text(format_buffer_size_text(initial_buf).into());

        let initial_total_lat = shared.cold.current_stream_latency.load(Ordering::Relaxed)
            + shared.cold.current_cabsim_latency.load(Ordering::Relaxed);
        window.set_latency_text(format_latency_text(initial_total_lat, initial_sr).into());

        let initial_channels = shared.rt_to_ui.active_channel_count.load(Ordering::Relaxed);
        window.set_channels_text(format_channels_text(initial_channels).into());
        let status_bar =
            StatusBarCache::new(initial_sr, initial_buf, initial_total_lat, initial_channels);

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
                shared.note_ui_interaction();
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
            window.on_load_model_clicked(move || {
                if let Some(dialog_state_arc) = shared.cold.dialog_state.as_ref()
                    && !shared.cold.ui_loading.load(Ordering::Relaxed)
                    && !shared.cold.ui_dialog_open.load(Ordering::Relaxed)
                    && !dialog_state_arc.active.load(Ordering::Relaxed)
                {
                    log::info!("SlintViewModel: Triggering Model file picker (rfd / XDG Portal)");
                    shared.note_ui_interaction();
                    // F-UX-06: opening the dialog is not a load in progress, so
                    // no spinner is raised here. `ui_loading` (and the spinner)
                    // only follow the actual `load_model` inside housekeeping.
                    shared.cold.ui_dialog_open.store(true, Ordering::Relaxed);
                    dialog_state_arc.active.store(true, Ordering::Relaxed);
                    if let Some(bridge) = host {
                        let host_static = bridge.as_static();
                        let handle = spawn_file_dialog(
                            Arc::clone(dialog_state_arc),
                            host_static,
                            Arc::clone(&shared.cold.alive_fence),
                        );
                        *lock_cold(&shared.cold.dialog_handle_sink, "dialog_handle_sink") =
                            Some(handle);
                    }
                }
            });
        }

        // ── Bind Clear Model Callback ─────────────────────────────────────────
        {
            let shared = Arc::clone(&shared);
            let window_weak = window.as_weak();
            window.on_clear_model_clicked(move || {
                log::info!("SlintViewModel: Requesting NAM Amp Model clear");
                shared.note_ui_interaction();
                shared.cold.ui_clear_model.store(true, Ordering::Relaxed);
                if let Some(bridge) = host
                    && shared.cold.alive_fence.load(Ordering::Acquire)
                {
                    let host_static = bridge.as_static();
                    host_static.request_callback();
                }
                if let Some(w) = window_weak.upgrade() {
                    w.set_has_model(false);
                    w.set_model_name("No model loaded".into());
                    w.set_model_arch("No model loaded".into());
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
                    shared.note_ui_interaction();
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
                        *lock_cold(&shared.cold.ir_dialog_handle_sink, "ir_dialog_handle_sink") =
                            Some(handle);
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
                shared.note_ui_interaction();
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
            last_model_name: initial_model_name,
            last_model_arch: initial_model_arch,
            last_ir_path: None,
            last_dsp_load_pct: None,
            last_seen_xrun_epoch: 0,
            xrun_hold_frames: 0,
            current_peak_l: 0.0,
            current_peak_r: 0.0,
            hold_peak_l: 0.0,
            hold_peak_r: 0.0,
            hold_timer_l: 0,
            hold_timer_r: 0,
            status_bar,
            timer: slint::Timer::default(),
            telemetry_active: true,
            cadence: TELEMETRY_PERIOD_FULL,
        }));

        // T2.2 (F-UX-08): suspend/resume the telemetry timer with the window.
        // The CLAP `gui.show()`/`gui.hide()` paths dispatch this callback via
        // `Weak<MainWindow>::upgrade_in_event_loop` on the GUI thread, so the
        // timer (thread-bound in Slint) is always touched from that thread. The
        // handler keeps a `Weak` reference — the window must not own the view
        // model.
        {
            let weak_inner = Rc::downgrade(&inner);
            inner
                .borrow()
                .window
                .on_set_telemetry_active(move |active: bool| {
                    if let Some(inner) = weak_inner.upgrade() {
                        apply_telemetry_active(&inner, active);
                    }
                });
        }
        arm_telemetry_timer(&inner, TELEMETRY_PERIOD_FULL);

        Self { inner }
    }

    /// Explicitly ticks telemetry once (useful for tests and manual render loops).
    ///
    /// Re-arms the timer when the adaptive cadence (F-UX-08) changes. While
    /// suspended ([`Self::set_telemetry_active`]) the timer stays stopped and a
    /// manual tick never re-schedules it.
    pub fn tick_telemetry(&mut self) {
        let desired = self.inner.borrow_mut().tick_telemetry();
        if let Some(period) = desired
            && self.inner.borrow().telemetry_active
        {
            arm_telemetry_timer(&self.inner, period);
        }
    }

    /// Suspends (hidden window) or resumes the telemetry timer (F-UX-08).
    ///
    /// This is the same entry point the CLAP `gui.hide()`/`gui.show()` paths
    /// reach through the window's `set_telemetry_active` callback. While
    /// suspended, no telemetry tick is scheduled at all.
    pub fn set_telemetry_active(&mut self, active: bool) {
        apply_telemetry_active(&self.inner, active);
    }

    /// Whether the telemetry timer is currently scheduled (F-UX-08).
    pub fn is_telemetry_running(&self) -> bool {
        self.inner.borrow().timer.running()
    }

    /// The period the telemetry timer runs at while active (F-UX-08):
    /// [`TELEMETRY_PERIOD_FULL`], or [`TELEMETRY_PERIOD_LOW`] in the silence
    /// state.
    pub fn telemetry_period(&self) -> std::time::Duration {
        self.inner.borrow().cadence
    }

    /// Returns a strong clone of the Slint window handle.
    pub fn window(&self) -> MainWindow {
        self.inner.borrow().window.clone_strong()
    }
}
