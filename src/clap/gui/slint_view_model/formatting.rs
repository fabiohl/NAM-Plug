// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Value formatting helpers, SIMD badge labels, and cached status-bar text sink
//! for [`super::SlintViewModel`].

use super::MainWindow;
use crate::clap::extensions::calibration::sanitize_calibration_level_dbu;
use neural_amp_modeler_rs::math::common::InstructionSet;

/// Lower dB floor for meter display.
pub const METER_MIN_DB: f32 = -60.0;

/// Upper dB headroom ceiling for meter display.
pub const METER_MAX_DB: f32 = 6.0;

/// Maps a linear amplitude value to a normalized `[0.0, 1.0]` fraction using logarithmic dB scaling.
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
pub(super) fn format_gain_text(val: f32) -> String {
    let rounded = (val * 10.0).round() / 10.0;
    if val > 0.0 {
        format!("+{rounded:.1} dB")
    } else {
        format!("{rounded:.1} dB")
    }
}

#[inline]
pub(super) fn format_gate_text(val: f32) -> String {
    if val <= -90.0 {
        "OFF".to_string()
    } else {
        format!("{} dB", val.round() as i32)
    }
}

/// Formats the loaded model's architecture/topology for the status bar:
/// reads real `NamModelMetadata`, never a hard-coded placeholder.
#[inline]
pub(super) fn format_model_arch(meta: &crate::clap::plugin::NamModelMetadata) -> String {
    if meta.topology.is_empty() {
        meta.architecture.clone()
    } else {
        format!("{} ({})", meta.architecture, meta.topology)
    }
}

#[inline]
pub(super) fn format_cal_dbu_text(val: f32) -> String {
    let clean = sanitize_calibration_level_dbu(val);
    let rounded = (clean * 10.0).round() / 10.0;
    let normalized = if rounded == 0.0 { 0.0 } else { rounded };
    if normalized > 0.0 {
        format!("+{normalized:.1} dBu")
    } else {
        format!("{normalized:.1} dBu")
    }
}

#[inline]
pub(super) fn extract_cal_flags(
    meta: Option<&crate::clap::plugin::NamModelMetadata>,
) -> (bool, bool, bool) {
    match meta {
        Some(m) => (
            m.input_level_dbu.is_some(),
            m.output_level_dbu.is_some(),
            m.loudness.is_some(),
        ),
        None => (false, false, false),
    }
}

#[inline]
pub(super) fn format_oversample_text(os: u32) -> &'static str {
    match os {
        1 => "2x (HQ)",
        2 => "4x (Ultra)",
        _ => "1x (Off)",
    }
}

/// Formats the status-bar sample-rate text (e.g. `"48 kHz"`, `"44.1 kHz"`).
/// Only called when the sample rate changes; `sr` must be > 0.
pub(super) fn format_sample_rate_text(sr: u32) -> String {
    if sr.is_multiple_of(1000) {
        format!("{} kHz", sr / 1000)
    } else {
        format!("{:.1} kHz", sr as f32 / 1000.0)
    }
}

/// Formats the status-bar buffer-size text (e.g. `"64 spl"`, `"— spl"`).
pub(super) fn format_buffer_size_text(buf: u32) -> String {
    if buf > 0 {
        format!("{buf} spl")
    } else {
        "— spl".to_string()
    }
}

/// Formats the status-bar latency text (e.g. `"128 spl (2.7 ms)"`).
pub(super) fn format_latency_text(total_lat: u32, sr: u32) -> String {
    let ms = if sr > 0 {
        (total_lat as f32 * 1000.0) / sr as f32
    } else {
        0.0
    };
    format!("{total_lat} spl ({ms:.1} ms)")
}

/// Formats the status-bar channel-count text (`"Mono"` / `"Stereo"`).
pub(super) fn format_channels_text(channels: u32) -> &'static str {
    match channels {
        1 => "Mono",
        _ => "Stereo",
    }
}

/// Destination for the four status-bar texts (F-UX-08 test seam).
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
/// Formats/publishes only when one of the backing integers changes, so steady
/// state performs zero allocations and zero `set_*_text` calls.
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
    /// last call.
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

/// Human-readable label for the runtime-detected SIMD backend actually
/// dispatching the neural inference math (Finding F6).
#[inline]
#[expect(
    deprecated,
    reason = "Avx512VnniBf16 must stay handled for match exhaustiveness"
)]
pub(super) fn simd_badge_text(isa: InstructionSet) -> &'static str {
    match isa {
        InstructionSet::Avx2 => "AVX2+FMA",
        InstructionSet::Avx512 => "AVX-512",
        InstructionSet::Avx512VnniBf16 => "AVX-512 VNNI+BF16",
        _ => "UNKNOWN",
    }
}
