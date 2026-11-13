// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Single Cabinet Slot domain contracts, off-RT WAV/NAM/NAMB builders, and RT wrapper.
//!
//! The cabinet slot hosts strictly **one** cabinet asset at a time:
//! - `.wav` impulse response ([`CabSimPair`], partitioned UPOLS convolution)
//! - `.nam` / `.namb` 1-in/1-out neural cabinet model ([`CabNamSlot`])
//!
//! Architecture metadata (`gear_type`, `gear_make`, `gear_model`) is treated strictly as
//! display/hint metadata — compatible 1-in/1-out models are never rejected based on tags.
//! Conversely, WaveNet models carrying an unadjudicated post-stack `head` (`config.head`
//! non-null object) and multi-channel (`in_channels != 1` or `num_output_channels != 1`)
//! models are rejected fail-closed in the cabinet slot without affecting the amp slot.
//!
//! Cabinet neural models run at unity gain without applying amp-slot calibration
//! (`input_level_dbu`, `output_level_dbu`, `loudness`) or WAV `IrLevelPolicy`.

use neural_amp_modeler_rs::common::diagnostics::{NamDiagnostic, NamErrorCode, SystemSnapshot};
use neural_amp_modeler_rs::common::spsc::GcItem;
use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimPair;
use neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF;
use neural_amp_modeler_rs::dsp::resampling::{MAX_STREAM_BLOCK, StreamingResampleBuffer};
use neural_amp_modeler_rs::loader::nam_json::{NamConfig, NamModelData, parse_nam_json};
use neural_amp_modeler_rs::loader::namb::parse_namb_typed;
use neural_amp_modeler_rs::loader::{LoadOptions, load_and_build_model_from_bytes_named};
use neural_amp_modeler_rs::models::{NamModel, StaticModel};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

/// Supported file formats for the single cabinet slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CabAssetKind {
    /// Standard PCM/IEEE-float WAV cabinet impulse response.
    #[default]
    WavIr,
    /// Upstream NAM JSON neural cabinet model (`.nam`).
    NamModel,
    /// Rust-native binary NAM cabinet model (`.namb`).
    NambModel,
}

impl CabAssetKind {
    /// Detects the cabinet asset format from its path extension (case-insensitive).
    #[must_use]
    pub fn detect_from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?;
        if ext.eq_ignore_ascii_case("wav") {
            Some(Self::WavIr)
        } else if ext.eq_ignore_ascii_case("nam") {
            Some(Self::NamModel)
        } else if ext.eq_ignore_ascii_case("namb") {
            Some(Self::NambModel)
        } else {
            None
        }
    }

    /// Short uppercase format badge for UI display and diagnostics.
    #[must_use]
    pub const fn badge_label(self) -> &'static str {
        match self {
            Self::WavIr => "WAV",
            Self::NamModel => "NAM",
            Self::NambModel => "NAMB",
        }
    }

    /// Whether this cabinet asset runs neural inference rather than WAV convolution.
    #[must_use]
    pub const fn is_neural(self) -> bool {
        !matches!(self, Self::WavIr)
    }

    /// Encodes as `u8` for lock-free atomic UI publication (`0 = none`).
    #[must_use]
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::WavIr => 1,
            Self::NamModel => 2,
            Self::NambModel => 3,
        }
    }

    /// Decodes from atomic `u8` (`0` or unknown -> `None`).
    #[must_use]
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::WavIr),
            2 => Some(Self::NamModel),
            3 => Some(Self::NambModel),
            _ => None,
        }
    }
}

/// Optional display metadata extracted from a neural cabinet model (hint/UI only).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CabNamMeta {
    /// Asset kind (`NamModel` or `NambModel`).
    pub kind: CabAssetKind,
    /// Primary model architecture string (`Linear`, `LSTM`, `WaveNet`, `Sequential`, etc.).
    pub architecture: String,
    /// Native sample rate declared by the model in Hz.
    pub sample_rate: u32,
    /// Model receptive field in native samples.
    pub receptive_field: usize,
    /// Optional declared `gear_type` tag (informational only; never gates loading).
    pub gear_type: Option<String>,
    /// Optional declared `gear_make` tag.
    pub gear_make: Option<String>,
    /// Optional declared `gear_model` tag.
    pub gear_model: Option<String>,
    /// Estimated weight footprint in bytes (`model_l` + `model_r`).
    pub memory_bytes: usize,
}

/// Snapshot metadata published to ColdShared for disambiguated GUI display.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CabUiMeta {
    /// Asset format in the slot.
    pub kind: CabAssetKind,
    /// Basename for primary display.
    pub file_name: String,
    /// Compact parent-qualified path label (e.g., `dirA/cab.wav`) for collision clarity.
    pub disambiguated_name: String,
    /// Native asset sample rate (Hz).
    pub source_rate: u32,
    /// Status summary (`128 spl • 48k` for WAV or `WaveNet • 48k` for NAM).
    pub status_summary: String,
    /// Optional `gear_type` / model subtitle.
    pub gear_summary: Option<String>,
}

impl CabUiMeta {
    /// Builds display metadata for a WAV cabinet IR without double-applying amp tags.
    #[must_use]
    pub fn from_wav(path: &Path, num_samples: usize, source_rate: u32) -> Self {
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        let rate_k = (source_rate as f32) / 1000.0;
        Self {
            kind: CabAssetKind::WavIr,
            file_name,
            disambiguated_name: disambiguated_cab_label(path),
            source_rate,
            status_summary: format!("{num_samples} spl • {rate_k:.1}k"),
            gear_summary: None,
        }
    }

    /// Builds display metadata for a `.nam`/`.namb` neural cabinet model.
    #[must_use]
    pub fn from_nam(path: &Path, meta: &CabNamMeta) -> Self {
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        let rate_k = (meta.sample_rate as f32) / 1000.0;
        let mut gear_parts = Vec::new();
        if let Some(ref gt) = meta.gear_type
            && !gt.trim().is_empty()
        {
            gear_parts.push(gt.trim().to_owned());
        }
        if let Some(ref gm) = meta.gear_model
            && !gm.trim().is_empty()
        {
            gear_parts.push(gm.trim().to_owned());
        } else if let Some(ref mk) = meta.gear_make
            && !mk.trim().is_empty()
        {
            gear_parts.push(mk.trim().to_owned());
        }
        Self {
            kind: meta.kind,
            file_name,
            disambiguated_name: disambiguated_cab_label(path),
            source_rate: meta.sample_rate,
            status_summary: format!("{} • {rate_k:.1}k [1x1]", meta.architecture),
            gear_summary: if gear_parts.is_empty() {
                None
            } else {
                Some(gear_parts.join(" • "))
            },
        }
    }
}

/// Neural cabinet processor state stored inside [`CabSlotDsp::NamModel`].
///
/// Kept unboxed intentionally so RT GC retirement decomposes its heap-allocated
/// members (`model_l`, `model_r`, `stream`) directly into [`GcItem::Model`] and
/// [`GcItem::Streaming`] with **zero** audio-thread deallocations and no engine changes.
pub struct CabNamSlot {
    /// Left-channel (or mono) neural cabinet model (`1 in -> 1 out`).
    pub model_l: Box<StaticModel>,
    /// Independent right-channel clone for true dual-mono stereo (`None` in mono builds).
    pub model_r: Option<Box<StaticModel>>,
    /// Dedicated bidirectional host<->model streaming resampler (strict block cardinality).
    pub stream: Box<StreamingResampleBuffer>,
    /// Native sample rate of the neural cabinet model (Hz).
    pub model_rate: u32,
    /// Host sample rate this slot's resampler was built for (Hz).
    pub host_rate: u32,
    /// Host-rate group delay introduced by `stream` (`0` when `host_rate == model_rate`).
    pub latency_samples: u32,
    /// Receptive field in native samples (reported for diagnostics/tail estimate).
    pub receptive_field: usize,
    /// Maximum tail drain budget in host-rate samples after input gate closure.
    pub tail_budget: usize,
    /// Remaining tail samples eligible for draining when the gate is closed.
    pub remaining_tail: usize,
}

impl CabNamSlot {
    /// Host-rate algorithmic latency contributed by this neural cabinet slot.
    #[inline]
    #[must_use]
    pub fn latency_samples(&self) -> u32 {
        self.latency_samples
    }

    /// Advertised host-rate tail length for CLAP `HostTail`.
    #[inline]
    #[must_use]
    pub fn tail_samples(&self) -> u32 {
        self.tail_budget.min(u32::MAX as usize) as u32
    }

    /// Remaining tail samples for gate-closed draining.
    #[inline]
    #[must_use]
    pub fn remaining_tail_samples(&self) -> usize {
        self.remaining_tail
    }

    /// Rearms the tail budget on active audio input.
    #[inline]
    pub fn rearm_tail(&mut self) {
        self.remaining_tail = self.tail_budget;
    }

    /// Resets resampler FIFOs and L/R neural states without allocating. RT-safe.
    #[inline]
    pub fn reset(&mut self) {
        self.stream.reset();
        self.model_l.prewarm_reset();
        if let Some(ref mut mr) = self.model_r {
            mr.prewarm_reset();
        }
        self.remaining_tail = 0;
    }

    /// Processes `n_samples` frames cleanly via `StreamingResampleBuffer` with zero RT allocs.
    ///
    /// `in_l`/`in_r` must NOT alias `out_l`/`out_r` and all slices must have length >= `n_samples`.
    #[inline]
    pub fn process_block(
        &mut self,
        in_l: &[f32],
        in_r: &[f32],
        out_l: &mut [f32],
        out_r: &mut [f32],
        n_samples: usize,
        process_mono: bool,
    ) {
        if n_samples == 0 {
            return;
        }
        let max_chunk = self.stream.max_block().max(1);
        let model_l = &mut self.model_l;
        let model_r = &mut self.model_r;
        let mut offset = 0usize;
        while offset < n_samples {
            let end = (offset + max_chunk).min(n_samples);
            self.stream.process(
                &in_l[offset..end],
                &in_r[offset..end],
                &mut out_l[offset..end],
                &mut out_r[offset..end],
                |ml_in, mr_in, ml_out, mr_out| {
                    model_l.process(ml_in, ml_out);
                    if !process_mono {
                        if let Some(mr) = model_r.as_mut() {
                            mr.process(mr_in, mr_out);
                        } else {
                            mr_out.copy_from_slice(ml_out);
                        }
                    }
                    ml_out.len()
                },
            );
            offset = end;
        }
    }

    /// Drains up to `n_samples` tail frames using zero input without re-arming tail budget.
    #[inline]
    pub fn drain_tail(
        &mut self,
        zero_l: &[f32],
        zero_r: &[f32],
        out_l: &mut [f32],
        out_r: &mut [f32],
        n_samples: usize,
        process_mono: bool,
    ) -> usize {
        let count = n_samples.min(self.remaining_tail);
        if count == 0 {
            return 0;
        }
        self.process_block(zero_l, zero_r, out_l, out_r, count, process_mono);
        self.remaining_tail -= count;
        count
    }

    /// Retires all owned heap allocations into `GcItem`s on the SPSC GC ring. RT-safe.
    #[inline]
    pub fn retire_to_gc(self, mut sink: impl FnMut(GcItem)) {
        sink(GcItem::Model(self.model_l));
        if let Some(mr) = self.model_r {
            sink(GcItem::Model(mr));
        }
        sink(GcItem::Streaming(self.stream));
    }
}

/// Exclusive occupant of the single Cabinet Slot (`WAV IR` OR `NAM/NAMB`).
pub enum CabSlotDsp {
    /// Partitioned UPOLS convolution engine pair from a `.wav` IR.
    WavIr(Box<CabSimPair>),
    /// 1-in/1-out neural cabinet model from a `.nam` or `.namb` file.
    NamModel(CabNamSlot),
}

impl CabSlotDsp {
    /// Host-rate latency contributed by the active cabinet in samples.
    #[inline]
    #[must_use]
    pub fn latency_samples(&self) -> u32 {
        match self {
            Self::WavIr(pair) => pair.partition_size() as u32,
            Self::NamModel(nam) => nam.latency_samples(),
        }
    }

    /// Host-rate tail length for CLAP `HostTail`.
    #[inline]
    #[must_use]
    pub fn tail_samples(&self) -> u32 {
        match self {
            Self::WavIr(pair) => (pair.l.num_partitions() * pair.l.latency_samples()) as u32,
            Self::NamModel(nam) => nam.tail_samples(),
        }
    }

    /// Host sample rate this cabinet resource was constructed for.
    #[inline]
    #[must_use]
    pub fn host_sample_rate(&self) -> u32 {
        match self {
            Self::WavIr(pair) => pair.sample_rate,
            Self::NamModel(nam) => nam.host_rate,
        }
    }

    /// Remaining tail samples eligible for draining during noise-gate closure.
    #[inline]
    #[must_use]
    pub fn remaining_tail_samples(&self, process_mono: bool) -> usize {
        match self {
            Self::WavIr(pair) => {
                if pair.l.is_passthrough() && pair.r.is_passthrough() {
                    0
                } else if !process_mono {
                    pair.remaining_tail_samples()
                } else {
                    pair.l.remaining_tail_samples()
                }
            }
            Self::NamModel(nam) => nam.remaining_tail_samples(),
        }
    }

    /// Resets convolution/neural filter memory (used cleanly on cabinet bypass re-enable).
    #[inline]
    pub fn reset(&mut self) {
        match self {
            Self::WavIr(pair) => {
                pair.l.reset();
                pair.r.reset();
            }
            Self::NamModel(nam) => nam.reset(),
        }
    }

    /// Whether this cabinet slot matches the host audio configuration without rebuilding
    /// during deactivation -> reactivation quantum renegotiation (`WavIr` reuses existing partition).
    #[inline]
    #[must_use]
    pub fn is_compatible_with_host(&self, host_rate: u32, max_block: usize) -> bool {
        match self {
            Self::WavIr(pair) => pair.sample_rate == host_rate,
            Self::NamModel(nam) => {
                let required = max_block.clamp(1, MAX_STREAM_BLOCK.min(MAX_RESAMP_BUF));
                nam.host_rate == host_rate && nam.stream.max_block() >= required
            }
        }
    }

    /// Whether a pre-built staged cabinet matches the exact initial activation host rate
    /// and block partition size (used for `staged_restore` / `staged_swap` before first `activate()`).
    #[inline]
    #[must_use]
    pub fn is_exact_host_config(&self, host_rate: u32, max_block: usize) -> bool {
        match self {
            Self::WavIr(pair) => {
                pair.sample_rate == host_rate && pair.partition_size() == max_block
            }
            Self::NamModel(nam) => {
                let required = max_block.clamp(1, MAX_STREAM_BLOCK.min(MAX_RESAMP_BUF));
                nam.host_rate == host_rate && nam.stream.max_block() >= required
            }
        }
    }

    /// Retires all heap-allocated DSP objects off-RT without dropping anything on the audio thread.
    #[inline]
    pub fn retire_to_gc(self, mut sink: impl FnMut(GcItem)) {
        match self {
            Self::WavIr(pair) => sink(GcItem::CabSimPair(pair)),
            Self::NamModel(nam) => nam.retire_to_gc(sink),
        }
    }
}

/// Formats a compact collision-resistant label (`parent/file.ext` or `file.ext`) from `path`.
#[must_use]
pub fn disambiguated_cab_label(path: &Path) -> String {
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    if let Some(parent_name) = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        && !parent_name.is_empty()
    {
        return format!("{parent_name}/{file_name}");
    }
    file_name
}

fn value_has_wavenet_post_stack_head(arch: Option<&str>, cfg: Option<&Value>) -> bool {
    let Some(cfg) = cfg else {
        return false;
    };
    if arch == Some("WaveNet")
        && let Some(head) = cfg.get("head")
        && !head.is_null()
    {
        return true;
    }
    if let Some(cond) = cfg.get("condition_dsp")
        && !cond.is_null()
    {
        let target = cond.get("model").unwrap_or(cond);
        let sub_arch = target.get("architecture").and_then(Value::as_str);
        if value_has_wavenet_post_stack_head(sub_arch, target.get("config")) {
            return true;
        }
    }
    if arch == Some("Sequential")
        && let Some(models) = cfg.get("models").and_then(Value::as_array)
    {
        for entry in models {
            let target = entry.get("model").unwrap_or(entry);
            let sub_arch = target.get("architecture").and_then(Value::as_str);
            if value_has_wavenet_post_stack_head(sub_arch, target.get("config")) {
                return true;
            }
        }
    }
    if arch == Some("SlimmableContainer")
        && let Some(submodels) = cfg.get("submodels").and_then(Value::as_array)
    {
        for entry in submodels {
            let target = entry.get("model").unwrap_or(entry);
            let sub_arch = target.get("architecture").and_then(Value::as_str);
            if value_has_wavenet_post_stack_head(sub_arch, target.get("config")) {
                return true;
            }
        }
    }
    false
}

fn nam_config_has_wavenet_post_stack_head(arch: &str, cfg: &NamConfig) -> bool {
    if arch == "WaveNet" && cfg.head.as_ref().is_some_and(|head| !head.is_null()) {
        return true;
    }
    if let Some(ref cond) = cfg.condition_dsp
        && !cond.is_null()
    {
        let target = cond.get("model").unwrap_or(cond);
        let sub_arch = target.get("architecture").and_then(Value::as_str);
        if value_has_wavenet_post_stack_head(sub_arch, target.get("config")) {
            return true;
        }
    }
    if arch == "Sequential"
        && let Some(ref models) = cfg.models
    {
        for entry in models {
            let target = entry.get("model").unwrap_or(entry);
            let sub_arch = target.get("architecture").and_then(Value::as_str);
            if value_has_wavenet_post_stack_head(sub_arch, target.get("config")) {
                return true;
            }
        }
    }
    if arch == "SlimmableContainer"
        && let Some(ref submodels) = cfg.submodels
    {
        for entry in submodels {
            let target = entry.get("model").unwrap_or(entry);
            let sub_arch = target.get("architecture").and_then(Value::as_str);
            if value_has_wavenet_post_stack_head(sub_arch, target.get("config")) {
                return true;
            }
        }
    }
    false
}

/// Fail-closed guardrail for the cabinet slot: rejects WaveNet post-stack `head`
/// configurations (deferred from S08 to S09) anywhere in the model tree without
/// affecting amp-slot validation.
pub fn validate_cab_nam_data(
    data: &NamModelData,
    sys: &SystemSnapshot,
) -> Result<(), Box<NamDiagnostic>> {
    if nam_config_has_wavenet_post_stack_head(&data.architecture, &data.config) {
        return Err(Box::new(
            NamDiagnostic::new(NamErrorCode::NamJsonUnsupportedTopology, sys)
                .message("Cabinet NAM WaveNet post-stack head (`config.head`) is not supported")
                .hint("Use a standard 1-in/1-out WaveNet/LSTM/Linear/Sequential model without `config.head`."),
        ));
    }
    Ok(())
}

/// Checks recursively that `model` and all container submodels strictly satisfy `1 in -> 1 out`.
pub fn validate_single_channel_model(model: &StaticModel) -> Result<(), (usize, usize)> {
    let in_ch = model.in_channels();
    let out_ch = model.num_output_channels();
    if in_ch != 1 || out_ch != 1 {
        return Err((in_ch, out_ch));
    }
    if let StaticModel::Container(container) = model {
        for (_threshold, sub) in container.submodels() {
            validate_single_channel_model(sub)?;
        }
    }
    Ok(())
}

fn compute_nam_tail_budget(model: &StaticModel, model_rate: u32, host_rate: u32) -> usize {
    let scaled_rf = if model_rate > 0 && host_rate > 0 {
        ((model.receptive_field() as u64 * u64::from(host_rate)).div_ceil(u64::from(model_rate)))
            as usize
    } else {
        model.receptive_field()
    };
    if model.is_lstm() {
        // LSTM models have IIR memory; provide ~100 ms ring-out capped sensibly.
        let iir_tail = (host_rate as usize / 10).clamp(2048, 24_000);
        scaled_rf.max(iir_tail)
    } else {
        scaled_rf.max(128)
    }
}

/// Parses, validates, prewarms, and builds a stereo/dual-mono [`CabNamSlot`] from file bytes off-RT.
pub fn build_cab_nam_from_bytes(
    bytes: &[u8],
    kind: CabAssetKind,
    path_for_diag: &Path,
    host_rate: u32,
    max_block: usize,
    dual_mono: bool,
    sys: &SystemSnapshot,
) -> Result<(CabNamSlot, CabNamMeta), Box<NamDiagnostic>> {
    let parsed_data = match kind {
        CabAssetKind::NamModel => {
            let text = std::str::from_utf8(bytes).map_err(|e| {
                Box::new(
                    NamDiagnostic::new(NamErrorCode::NamJsonParseError, sys)
                        .message(format!(
                            "Cabinet .nam file is not valid UTF-8: {path_for_diag:?}"
                        ))
                        .param("error", e.to_string()),
                )
            })?;
            parse_nam_json(text).map_err(|e| {
                Box::new(
                    NamDiagnostic::new(NamErrorCode::NamJsonParseError, sys)
                        .message(format!(
                            "Failed to parse cabinet .nam JSON: {path_for_diag:?}"
                        ))
                        .param("error", e.to_string()),
                )
            })?
        }
        CabAssetKind::NambModel => parse_namb_typed(bytes).map_err(|e| {
            Box::new(
                NamDiagnostic::new(NamErrorCode::NambInvalidMagic, sys)
                    .message(format!(
                        "Failed to parse cabinet .namb binary: {path_for_diag:?}"
                    ))
                    .param("error", e.to_string()),
            )
        })?,
        CabAssetKind::WavIr => {
            return Err(Box::new(
                NamDiagnostic::new(NamErrorCode::IrLoadFailed, sys)
                    .message("Internal error: build_cab_nam_from_bytes called for WAV IR"),
            ));
        }
    };

    validate_cab_nam_data(&parsed_data, sys)?;

    let source_label = path_for_diag.to_string_lossy();
    let loaded = load_and_build_model_from_bytes_named(
        bytes,
        source_label.as_ref(),
        sys,
        dual_mono,
        LoadOptions::default(),
    )
    .map_err(|e| Box::new(NamDiagnostic::new(e.code(), sys).message(e.to_string())))?;

    let mut model_l = loaded.model_l.ok_or_else(|| {
        Box::new(
            NamDiagnostic::new(NamErrorCode::ModelBuildFailed, sys)
                .message("Cabinet NAM build returned empty left model"),
        )
    })?;
    let mut model_r = loaded.model_r;

    if let Err((in_ch, out_ch)) = validate_single_channel_model(&model_l) {
        return Err(Box::new(
            NamDiagnostic::new(NamErrorCode::InvalidModelTopology, sys)
                .message(format!(
                    "Cabinet NAM model must be strictly 1-in/1-out (got {in_ch}-in/{out_ch}-out)"
                ))
                .hint("Multi-channel or stereo-coupled models are not supported in the single cabinet slot."),
        ));
    }
    if let Some(ref mr) = model_r
        && let Err((in_ch, out_ch)) = validate_single_channel_model(mr)
    {
        return Err(Box::new(
            NamDiagnostic::new(NamErrorCode::InvalidModelTopology, sys).message(format!(
                "Cabinet NAM right-channel clone must be strictly 1-in/1-out (got {in_ch}-in/{out_ch}-out)"
            )),
        ));
    }

    let safe_host_rate = host_rate.clamp(4_000, 384_000);
    let model_rate = loaded.sample_rate.max(1);
    let clamped_block = max_block.clamp(1, MAX_STREAM_BLOCK.min(MAX_RESAMP_BUF));

    let stream = StreamingResampleBuffer::new(safe_host_rate, model_rate, clamped_block)
        .map(Box::new)
        .map_err(|e| {
            Box::new(
                NamDiagnostic::new(e, sys)
                    .message("Failed to build streaming resampler for cabinet NAM model"),
            )
        })?;

    // Compute worst-case resampled inner block size so model_l/model_r buffers never overflow.
    let max_model_block = (((clamped_block as u64 * u64::from(model_rate))
        .div_ceil(u64::from(safe_host_rate))) as usize
        + 64)
        .clamp(64, MAX_STREAM_BLOCK);

    model_l.set_max_buffer_size(max_model_block).map_err(|e| {
        Box::new(
            NamDiagnostic::new(NamErrorCode::ModelBuildFailed, sys)
                .message("Failed to resize cabinet left NAM model buffer")
                .param("error", e.to_string()),
        )
    })?;
    model_l.prewarm_reset();
    for _ in 0..64 {
        if model_l.prewarm_complete() {
            break;
        }
        let _ = model_l.prewarm_step(usize::MAX);
    }

    if let Some(ref mut mr) = model_r {
        mr.set_max_buffer_size(max_model_block).map_err(|e| {
            Box::new(
                NamDiagnostic::new(NamErrorCode::ModelBuildFailed, sys)
                    .message("Failed to resize cabinet right NAM model buffer")
                    .param("error", e.to_string()),
            )
        })?;
        mr.prewarm_reset();
        for _ in 0..64 {
            if mr.prewarm_complete() {
                break;
            }
            let _ = mr.prewarm_step(usize::MAX);
        }
    }

    let latency_samples = stream.latency_samples();
    let receptive_field = model_l.receptive_field();
    let tail_budget = compute_nam_tail_budget(&model_l, model_rate, safe_host_rate);

    let mem_bytes =
        model_l.memory_footprint() + model_r.as_ref().map_or(0, |m| m.memory_footprint());

    let meta_opt = loaded.metadata.as_ref();
    let meta = CabNamMeta {
        kind,
        architecture: loaded.architecture,
        sample_rate: model_rate,
        receptive_field,
        gear_type: meta_opt.and_then(|m| m.gear_type.clone()),
        gear_make: meta_opt.and_then(|m| m.gear_make.clone()),
        gear_model: meta_opt.and_then(|m| m.gear_model.clone()),
        memory_bytes: mem_bytes,
    };

    let slot = CabNamSlot {
        model_l,
        model_r,
        stream,
        model_rate,
        host_rate: safe_host_rate,
        latency_samples,
        receptive_field,
        tail_budget,
        remaining_tail: 0,
    };

    Ok((slot, meta))
}

/// Rebuilds a [`CabSlotDsp::NamModel`] off-RT from raw bytes for the negotiated host config.
pub fn build_cab_nam_slot_for_host(
    kind: CabAssetKind,
    bytes: &[u8],
    path_for_diag: &Path,
    host_rate: u32,
    max_block: usize,
) -> Result<(CabSlotDsp, CabNamMeta), String> {
    let sys = SystemSnapshot::capture();
    build_cab_nam_from_bytes(
        bytes,
        kind,
        path_for_diag,
        host_rate,
        max_block,
        cfg!(feature = "dual-mono"),
        &sys,
    )
    .map(|(slot, meta)| (CabSlotDsp::NamModel(slot), meta))
    .map_err(|e| e.to_string())
}

/// Rebuilds a neural [`CabSlotDsp`] from override bytes or `ColdShared::cab_nam_raw_bytes`.
pub(crate) fn build_cab_nam_from_shared(
    shared: &crate::clap::plugin::NamClapShared,
    override_nam: Option<(CabAssetKind, &[u8])>,
    partition_size: usize,
    host_rate: u32,
) -> Result<Option<CabSlotDsp>, clack_plugin::prelude::PluginError> {
    use crate::clap::plugin::errors::{self, static_plugin_error};
    use std::sync::atomic::Ordering;

    let diag_path = shared
        .cold
        .ir_path
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_else(|| "cabinet.nam".to_string());
    if let Some((kind, bytes)) = override_nam {
        let (slot, _meta) = build_cab_nam_slot_for_host(
            kind,
            bytes,
            Path::new(&diag_path),
            host_rate,
            partition_size,
        )
        .map_err(|e| static_plugin_error(errors::dsp_resources::CAB_NAM_BUILD_FAILED, e))?;
        return Ok(Some(slot));
    }
    let kind = CabAssetKind::from_u8(shared.cold.cab_kind.load(Ordering::Relaxed));
    if let Some(k) = kind
        && k.is_neural()
    {
        let raw_guard = shared.cold.cab_nam_raw_bytes.lock().map_err(|e| {
            static_plugin_error(
                errors::dsp_resources::CAB_NAM_STORAGE_LOCK_POISONED,
                format_args!("{e}"),
            )
        })?;
        let Some((stored_kind, ref bytes)) = *raw_guard else {
            return Ok(None);
        };
        let (slot, _meta) = build_cab_nam_slot_for_host(
            stored_kind,
            bytes,
            Path::new(&diag_path),
            host_rate,
            partition_size,
        )
        .map_err(|e| static_plugin_error(errors::dsp_resources::CAB_NAM_BUILD_FAILED, e))?;
        return Ok(Some(slot));
    }
    Ok(None)
}

#[cfg(test)]
#[path = "cabinet_test.rs"]
mod tests;
