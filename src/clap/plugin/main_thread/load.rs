// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Model loading (main thread only).

use super::super::lock::lock_cold;
use super::super::shared::StagedSwap;
use super::super::shared::{ClapParamPayload, LoadModelPayload, NamModelMetadata, PendingModel};
use super::NamClapMainThread;
use crate::clap::plugin::command_scheduler::PushError;
use neural_amp_modeler_rs::common::diagnostics::{
    ModelInfo, NamDiagnostic, NamErrorCode, SystemSnapshot,
};
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer;
use neural_amp_modeler_rs::models::slimmable::clone_wavenet_for_slimmable_storage;
use neural_amp_modeler_rs::models::{NamModel, StaticModel};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

#[cfg(test)]
mod test_hooks;
#[cfg(test)]
pub(crate) use test_hooks::{
    run_test_mid_load_hook, set_test_mid_load_hook, test_mid_load_hook_lock,
};
mod offthread;
pub(crate) use offthread::prepare_model_offthread;

/// Audio configuration epoch captured at dispatch/prepare time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AudioEpoch {
    /// Host sample rate (Hz).
    pub(crate) sample_rate: u32,
    /// Host buffer size (frames). 0 indicates pre-activation deferral.
    pub(crate) buffer_size: usize,
}

/// Off-RT, side-effect-free model resources produced by
/// [`NamClapMainThread::prepare_model`] and installed by
/// [`NamClapMainThread::commit_prepared_model`].
///
/// The split isolates the expensive loading work (file I/O, hashing, model
/// build, buffer sizing, cost calibration) from every mutation of main-thread
/// state, so the loading half can run on a non-RT worker thread and hand this
/// `Send` package back for a short, bounded commit.
pub(crate) struct PreparedModel {
    /// Validated model file path (drives identity/search-path publication).
    path: PathBuf,
    /// Encapsulated model for neural inference (Left Channel).
    model_l: Option<Box<StaticModel>>,
    /// Encapsulated model for neural inference (Right Channel in dual-mono mode).
    model_r: Option<Box<StaticModel>>,
    /// Model native sample rate (Hz).
    model_rate: u32,
    /// Model calibration metadata extracted at load time.
    cal_meta: crate::clap::extensions::calibration::ModelCalibrationMeta,
    /// Content SHA-256 hex digest adopted as the portable asset identity.
    model_hash: String,
    /// Metadata for GUI display.
    metadata: NamModelMetadata,
    /// Dynamic model info for diagnostics.
    model_info: ModelInfo,
    /// Dynamic model memory footprint in bytes.
    model_bytes: usize,
    /// Full WaveNet weights clone for main-thread slimmable rebuild.
    full_wavenet: Option<Box<StaticModel>>,
    /// Polyphase resampler matching host↔model rate. Built only when
    /// `buffer_size > 0`; a deferred load builds it in `flush_pending_model()`.
    new_resampler: Option<Box<NamResampler>>,
    /// Streaming resample adapter sized for the host block. Same build
    /// condition as `new_resampler`.
    new_stream: Option<Box<StreamingResampleBuffer>>,
    /// Off-RT measured per-sample inference cost (ns) when adaptive cost is on.
    prewarm_cost_ns: Option<f32>,
    /// Audio configuration epoch captured at prepare time.
    pub(crate) audio_epoch: AudioEpoch,
}

// `PreparedModel` crosses the main/worker thread boundary, so its
// `Send`-ness is pinned at compile time.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<PreparedModel>();
};
mod cabinet;
mod commit;
mod model;
mod prepare;

#[cfg(test)]
#[path = "load_test.rs"]
mod load_error_code_tests;
