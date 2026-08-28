// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! DeactivatedDspState — ownership of heavy DSP resources across activate cycles.
//!
//! When the host deactivates a plugin instance (mute track, bypass, UI close),
//! expensive resources — model weights, resampler polyphase coefficient banks,
//! oversampling half-band filter states, convolution FFT partitions — are moved
//! here instead of being dropped. On the next `activate()`, they are reinstalled
//! deterministically, avoiding I/O, filter-bank pre-compute, and FFT setup overhead.
//!
//! Validation on restore: `sample_rate` and `buffer_size` must match the current
//! audio configuration. If the host changed either, the affected resources are
//! discarded and rebuilt from scratch. Model weights are always reusable.

use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter;
use neural_amp_modeler_rs::dsp::oversample::{OversampleEngine, OversampleFactor};
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::models::StaticModel;

/// Heavy DSP resources preserved across deactivate/activate cycles.
pub(crate) struct DeactivatedDspState {
    /// Active neural model (L channel). Always reusable — model weights
    /// are independent of host sample rate and buffer size.
    pub(crate) model_l: Option<Box<StaticModel>>,
    /// Monotonic generation of the active model identity,
    /// persisted with the model so a reactivation keeps the same generation.
    pub(crate) model_generation: u64,
    /// Cab-sim convolution adapter. Reusable only if `partition_size` matches
    /// the current `max_frames_count` (all FFT plans and FDL are sized by
    /// partition size at construction time). Kept `Box`ed to preserve the
    /// end-to-end real-time safe ownership contract.
    pub(crate) cabsim_adapter: Option<Box<CabSimAdapter>>,
    /// Polyphase sinc resampler. Reusable only if `host_rate` matches the
    /// current host sample rate (phase interpolation banks are rate-dependent).
    pub(crate) resampler: Box<NamResampler>,
    /// Strict-cardinality streaming resample adapter.
    /// Reusable only if both `host_rate` and `buffer_size` match (FIFO
    /// capacities are sized by the worst-case host block).
    pub(crate) stream: Box<neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer>,
    /// Half-band oversampling engine L. Reusable unconditionally (fixed at
    /// MAX_RESAMP_BUF input, state is reset per activate).
    pub(crate) os_l: Box<OversampleEngine>,
    /// Half-band oversampling engine R.
    pub(crate) os_r: Box<OversampleEngine>,
    /// Oversampling factor these engines were *applied* with.
    /// Persisted from `NamClapProcessor::applied_os_factor` at deactivate —
    /// never the requested `params.oversample`, which may lag ahead of the
    /// engines while a host restart is pending. Used to detect real engine
    /// compatibility on restore.
    pub(crate) os_factor: OversampleFactor,
    /// Host sample rate when deactivated (for restore validation).
    pub(crate) sample_rate: u32,
    /// Host buffer size when deactivated (for `conv_engine` validation).
    pub(crate) buffer_size: u32,
    /// Model input gain calibration multiplier (from `input_level_dbu` metadata).
    pub(crate) model_input_mult_adj: f32,
    /// Model output gain calibration multiplier (from loudness metadata).
    pub(crate) model_output_mult_adj: f32,
}
