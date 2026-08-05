// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
//! Persistent GUI state: `UiState`, `VuMeterSharedState`, `VuUniforms`.

use crate::clap::plugin::NamModelMetadata;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Instant;

/// Cache of uniform variable locations in the VU meter GLSL shader.
#[derive(Debug, Clone)]
pub struct VuUniforms {
    /// Location of the viewport uniform.
    pub loc_viewport: Option<glow::UniformLocation>,
    /// Location of the meter rectangle uniform.
    pub loc_meter_rect: Option<glow::UniformLocation>,
    /// Location of the peak value uniform.
    pub loc_peak_frac: Option<glow::UniformLocation>,
    /// Location of the hold value uniform.
    pub loc_hold_frac: Option<glow::UniformLocation>,
    /// Location of the hold color uniform.
    pub loc_hold_color_type: Option<glow::UniformLocation>,
}

/// Shared VU meter state for rendering in the egui callback without heap allocations per frame.
#[derive(Debug)]
pub struct VuMeterSharedState {
    /// Current peak level fraction of the meter (0.0 to 1.0, encoded as u32 bits).
    pub peak_frac: std::sync::atomic::AtomicU32,
    /// Current level fraction of the peak hold indicator (0.0 to 1.0, encoded as u32 bits).
    pub hold_frac: std::sync::atomic::AtomicU32,
    /// Color type to apply to the peak hold indicator (representing green, yellow, or red).
    pub hold_color_type: std::sync::atomic::AtomicI32,
    /// Minimum X coordinate of the meter's logical drawing rectangle (encoded as u32 bits).
    pub rect_min_x: std::sync::atomic::AtomicU32,
    /// Minimum Y coordinate of the meter's logical drawing rectangle (encoded as u32 bits).
    pub rect_min_y: std::sync::atomic::AtomicU32,
    /// Maximum X coordinate of the meter's logical drawing rectangle (encoded as u32 bits).
    pub rect_max_x: std::sync::atomic::AtomicU32,
    /// Maximum Y coordinate of the meter's logical drawing rectangle (encoded as u32 bits).
    pub rect_max_y: std::sync::atomic::AtomicU32,
}

impl Default for VuMeterSharedState {
    fn default() -> Self {
        Self {
            peak_frac: std::sync::atomic::AtomicU32::new(0.0f32.to_bits()),
            hold_frac: std::sync::atomic::AtomicU32::new(0.0f32.to_bits()),
            hold_color_type: std::sync::atomic::AtomicI32::new(0),
            rect_min_x: std::sync::atomic::AtomicU32::new(0.0f32.to_bits()),
            rect_min_y: std::sync::atomic::AtomicU32::new(0.0f32.to_bits()),
            rect_max_x: std::sync::atomic::AtomicU32::new(0.0f32.to_bits()),
            rect_max_y: std::sync::atomic::AtomicU32::new(0.0f32.to_bits()),
        }
    }
}

/// Persistent GUI state between frames.
#[derive(Clone)]
pub struct UiState {
    /// Last retained peak value for the left channel.
    pub peak_l_hold: f32,
    /// Last retained peak value for the right channel.
    pub peak_r_hold: f32,
    /// Instant when the left peak value was retained.
    pub peak_l_hold_time: Instant,
    /// Instant when the right peak value was retained.
    pub peak_r_hold_time: Instant,
    /// Persistent L clipping LED — cleared by clicking on the meter.
    pub clip_l: bool,
    /// Persistent R clipping LED — cleared by clicking on the meter.
    pub clip_r: bool,
    /// Indicates whether the expanded telemetry panel should be displayed.
    pub show_telemetry: bool,
    /// Last instant when telemetry information was updated in the GUI.
    pub last_telem_update: Instant,
    /// Damped copy of the DSP cycle time for display.
    pub telem_cycles: u64,
    /// Damped copy of the last block size for display.
    pub telem_last_n: u32,
    /// Damped copy of the DSP thread RT priority.
    pub telem_prio: i32,
    /// Damped copy of the overload count.
    pub telem_overloads: u32,
    /// Compiled OpenGL shader program for drawing the VU meter.
    pub vu_program: Option<glow::Program>,
    /// Empty VAO for bufferless draw calls.
    pub vu_vao: Option<glow::VertexArray>,
    /// Cache of uniform locations in the VU meter GLSL shader.
    pub vu_uniforms: Arc<OnceLock<VuUniforms>>,
    /// Shared state for the left meter.
    pub vu_l_state: Option<Arc<VuMeterSharedState>>,
    /// Shared state for the right meter.
    pub vu_r_state: Option<Arc<VuMeterSharedState>>,
    /// OpenGL drawing callback for the left meter.
    pub vu_l_callback: Option<Arc<egui_glow::CallbackFn>>,
    /// OpenGL drawing callback for the right meter.
    pub vu_r_callback: Option<Arc<egui_glow::CallbackFn>>,
    /// Indicates whether a valid model file is being dragged over the window.
    pub drag_active: bool,
    /// Damped CPU/DSP load in percentage.
    pub telem_load_pct: f64,
    /// Damped cycle time in nanoseconds.
    pub telem_cycle_ns: u64,
    /// Maximum time budget in nanoseconds.
    pub telem_budget_ns: u64,
    /// Expiration of the visual error loading banner (None if not active).
    pub error_expiration: Option<Instant>,
    /// Short/summary model error message.
    pub error_msg: String,
    /// Expiration of the visual IR error loading banner (None if not active).
    /// Separated from `error_expiration` so model and IR errors don't
    /// overwrite each other (CLAP-F021, S0-E0-T05).
    pub ir_error_expiration: Option<Instant>,
    /// Short/summary IR error message.
    pub ir_error_msg: String,
    /// Expiration of the visual toast notification banner (None if not active).
    pub toast_expiration: Option<Instant>,
    /// Cached status bar strings (SR, Lat, DSP%, Cycles, Last N, RT Prio, Overloads, Flags).
    pub status_strings: [String; 8],
    /// Corresponding tooltips for each status bar item.
    pub status_tooltips: [&'static str; 8],
    /// Cache of complete model metadata for change detection.
    pub cached_metadata: Option<NamModelMetadata>,
    /// Cached metadata display strings (text, tooltip).
    pub metadata_display: Vec<(String, String)>,
    /// Cached model name for display.
    pub model_display_name: String,
    /// Cached IR file name for display.
    pub ir_display_name: String,
    /// Memoized font size for the telemetry status bar (recomputed on content/width change).
    pub telem_cached_font_size: Option<f32>,
    /// Last available width used to compute telem_cached_font_size.
    pub telem_cached_width: f32,
    /// Instant when the telemetry font size was last computed.
    pub telem_last_font_instant: Instant,
    /// Memoized font size for the metadata bar (recomputed on content/width change).
    pub metadata_cached_font_size: Option<f32>,
    /// Last available width used to compute metadata_cached_font_size.
    pub metadata_cached_width: f32,
    /// Version counter incremented when metadata_display is rebuilt.
    pub metadata_display_version: u64,
    /// The metadata_display_version used for the last font size computation.
    pub metadata_last_font_version: u64,
    /// Cached peak-L value for idle-frame detection (updated by on_frame before ui run).
    pub cached_peak_l: f32,
    /// Cached peak-R value for idle-frame detection (updated by on_frame before ui run).
    pub cached_peak_r: f32,
}

impl std::fmt::Debug for UiState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UiState")
            .field("peak_l_hold", &self.peak_l_hold)
            .field("peak_r_hold", &self.peak_r_hold)
            .field("peak_l_hold_time", &self.peak_l_hold_time)
            .field("peak_r_hold_time", &self.peak_r_hold_time)
            .field("clip_l", &self.clip_l)
            .field("clip_r", &self.clip_r)
            .field("show_telemetry", &self.show_telemetry)
            .field("last_telem_update", &self.last_telem_update)
            .field("telem_cycles", &self.telem_cycles)
            .field("telem_last_n", &self.telem_last_n)
            .field("telem_prio", &self.telem_prio)
            .field("telem_overloads", &self.telem_overloads)
            .field("vu_program", &self.vu_program)
            .field("vu_vao", &self.vu_vao)
            .field("vu_uniforms", &self.vu_uniforms)
            .field("vu_l_state", &self.vu_l_state)
            .field("vu_r_state", &self.vu_r_state)
            .field(
                "vu_l_callback",
                &self.vu_l_callback.as_ref().map(|_| "<callback>"),
            )
            .field(
                "vu_r_callback",
                &self.vu_r_callback.as_ref().map(|_| "<callback>"),
            )
            .field("telem_load_pct", &self.telem_load_pct)
            .field("telem_cycle_ns", &self.telem_cycle_ns)
            .field("telem_budget_ns", &self.telem_budget_ns)
            .field("error_expiration", &self.error_expiration)
            .field("ir_error_expiration", &self.ir_error_expiration)
            .field("cached_metadata", &self.cached_metadata)
            .field("model_display_name", &self.model_display_name)
            .field("ir_display_name", &self.ir_display_name)
            .finish()
    }
}

impl UiState {
    /// Returns `true` if any UI animation is currently active (error banner,
    /// toast notification) and requires continuous repaints.
    pub fn has_active_animations(&self) -> bool {
        let now = Instant::now();
        self.error_expiration.is_some_and(|t| t > now)
            || self.ir_error_expiration.is_some_and(|t| t > now)
            || self.toast_expiration.is_some_and(|t| t > now)
            || self.drag_active
    }

    /// Returns `true` if peak-hold values are stable (not being updated by
    /// audio — hold timestamps haven't changed). Stable holds mean the audio
    /// stream is silent and we can skip repaints.
    pub fn peak_hold_is_stable(&self) -> bool {
        // Peak holds are considered stable when:
        // - Values < 1e-6 (no audio at all), OR
        // - Hold timestamps haven't changed in >2s (audio stopped, holds decayed)
        let tiny = self.peak_l_hold < 1e-6 && self.peak_r_hold < 1e-6;
        let old = self.peak_l_hold_time.elapsed() > std::time::Duration::from_secs(2)
            && self.peak_r_hold_time.elapsed() > std::time::Duration::from_secs(2);
        tiny || old
    }
}

impl Default for UiState {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            peak_l_hold: 0.0,
            peak_r_hold: 0.0,
            peak_l_hold_time: now,
            peak_r_hold_time: now,
            clip_l: false,
            clip_r: false,
            show_telemetry: false,
            last_telem_update: now - std::time::Duration::from_secs(2),
            telem_cycles: 0,
            telem_last_n: 0,
            telem_prio: 0,
            telem_overloads: 0,
            vu_program: None,
            vu_vao: None,
            vu_uniforms: Arc::new(OnceLock::new()),
            vu_l_state: None,
            vu_r_state: None,
            vu_l_callback: None,
            vu_r_callback: None,
            drag_active: false,
            telem_load_pct: 0.0,
            telem_cycle_ns: 0,
            telem_budget_ns: 0,
            error_expiration: None,
            error_msg: String::new(),
            ir_error_expiration: None,
            ir_error_msg: String::new(),
            toast_expiration: None,
            status_strings: Default::default(),
            status_tooltips: [
                "Host DAW Sample Rate.\nThe neural model runs internally at 48 kHz. High quality resampling is automatically active if rates differ.",
                "Latency introduced by the internal resampler to align host sample rate with neural model's native 48 kHz.\nBypassed (0 ms) when host sample rate is 48 kHz.",
                "DSP Thread Load: portion of real-time audio time budget used.",
                "Average CPU clock cycles consumed per real-time processing block",
                "Number of audio samples processed in the last block",
                "Real-time thread scheduling priority. Values > 0 indicate active RT thread scheduling",
                "Number of real-time buffer deadline overruns (XRUNs) detected since start",
                "Real-time engine diagnostic status flags bitmask",
            ],
            cached_metadata: None,
            metadata_display: Vec::new(),
            model_display_name: String::new(),
            ir_display_name: String::new(),
            telem_cached_font_size: None,
            telem_cached_width: 0.0,
            telem_last_font_instant: now,
            metadata_cached_font_size: None,
            metadata_cached_width: 0.0,
            metadata_display_version: 0,
            metadata_last_font_version: 0,
            cached_peak_l: 0.0,
            cached_peak_r: 0.0,
        }
    }
}
