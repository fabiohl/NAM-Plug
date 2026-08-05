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
    /// Cab-sim convolution adapter. Reusable only if `partition_size` matches
    /// the current `max_frames_count` (all FFT plans and FDL are sized by
    /// partition size at construction time).
    pub(crate) cabsim_adapter: Option<CabSimAdapter>,
    /// Polyphase sinc resampler. Reusable only if `host_rate` matches the
    /// current host sample rate (phase interpolation banks are rate-dependent).
    pub(crate) resampler: Box<NamResampler>,
    /// Half-band oversampling engine L. Reusable unconditionally (fixed at
    /// MAX_RESAMP_BUF input, state is reset per activate).
    pub(crate) os_l: Box<OversampleEngine>,
    /// Half-band oversampling engine R.
    pub(crate) os_r: Box<OversampleEngine>,
    /// Oversampling factor these engines were built for.
    /// Used to detect factor changes on restart (S4-E4-T02).
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
