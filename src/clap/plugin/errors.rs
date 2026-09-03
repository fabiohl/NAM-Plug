// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! # CLAP Static Error Catalog (Épico 4 / SA-04)
//!
//! `clack_plugin` requires `PluginError::Message` to carry a `&'static str`.
//! Historically, dynamic error text (formatted paths, causes, sizes) was leaked
//! onto the heap via `Box::leak(format!(...))`, producing monotonic RAM growth
//! whenever a host cyclically submitted corrupted presets/states (Finding SA-04).
//!
//! This module centralises every DAW-facing plugin error as a catalog of static
//! `&'static str` constants, grouped by error category:
//!
//! * [`activation`] — `activate()`/`deactivate()` lifecycle failures.
//! * [`dsp_resources`] — sample-rate resampler, streaming adapter, oversampler
//!   and cab-sim convolution engine construction failures.
//! * [`state_txn`] — state JSON transaction validation failures.
//! * [`assets`] — file hashing / asset identity failures.
//!
//! # Invariants (SA-04)
//!
//! * No `Box::leak` is used to format runtime error strings. The single
//!   remaining `Box::leak` in the plugin error paths is the emergency panic
//!   capture (`processor::panic_to_error`), where the process is already
//!   recovering from a critical failure and the leak is trivial and one-shot.
//! * Dynamic detail is emitted exclusively through the `log` facade (via
//!   [`static_plugin_error`]); the DAW always receives the static catalog
//!   message, which keeps error dialogs legible and bounded in memory.
//! * Zero monotonic heap growth when a host submits corrupted states in a loop.

use clack_plugin::plugin::PluginError;

/// Activation and audio-configuration lifecycle errors (CLAP `activate()`).
///
/// These fire while the plugin instantiates its DSP resources, before the audio
/// thread starts. They are rare, but a hostile/buggy host must never be able to
/// grow the plugin heap by repeatedly failing activation with dynamic detail.
pub mod activation {
    /// A single-owner channel consumer was already extracted from `ColdShared`.
    pub const PARAM_RX_ALREADY_EXTRACTED: &str = "param_rx consumer has already been extracted";
    /// A single-owner GC producer was already extracted from `ColdShared`.
    pub const GC_TX_ALREADY_EXTRACTED: &str = "gc_tx producer has already been extracted";
    /// A single-owner channel consumer was already extracted from `ColdShared`.
    pub const SLIMMABLE_RX_ALREADY_EXTRACTED: &str =
        "slimmable_rx consumer has already been extracted";
    /// A DSP intermediate buffer could not be pre-allocated during `activate()`.
    pub const BUFFER_PREALLOC_FAILED: &str =
        "Failed to pre-allocate DSP processing buffers during plugin activation";
    /// The fresh resampler expected on the staged-restore path is unavailable.
    pub const RESAMPLER_UNAVAILABLE_STAGED: &str =
        "Fresh resampler unavailable during activate (staged restore)";
    /// The fresh streaming adapter expected on the staged-restore path is unavailable.
    pub const STREAM_UNAVAILABLE_STAGED: &str =
        "Fresh streaming buffer unavailable during activate (staged restore)";
    /// The fresh CabSim adapter expected on the staged-restore path is unavailable.
    pub const CABSIM_UNAVAILABLE_STAGED: &str =
        "Fresh CabSim adapter unavailable during activate (staged restore)";
    /// The fresh oversamplers expected on the staged-restore path are unavailable.
    pub const OVERSAMPLER_UNAVAILABLE_STAGED: &str =
        "Fresh oversamplers unavailable during activate (staged restore)";
    /// The fresh resampler expected on the fresh activation path is unavailable.
    pub const RESAMPLER_UNAVAILABLE_FRESH: &str =
        "Fresh resampler unavailable during activate (fresh path)";
    /// The fresh streaming adapter expected on the fresh activation path is unavailable.
    pub const STREAM_UNAVAILABLE_FRESH: &str =
        "Fresh streaming buffer unavailable during activate (fresh path)";
    /// The fresh CabSim adapter expected on the fresh activation path is unavailable.
    pub const CABSIM_UNAVAILABLE_FRESH: &str =
        "Fresh CabSim adapter unavailable during activate (fresh path)";
    /// The fresh oversamplers expected on the fresh activation path are unavailable.
    pub const OVERSAMPLER_UNAVAILABLE_FRESH: &str =
        "Fresh oversamplers unavailable during activate (fresh path)";
}

/// DSP resource construction errors (resampler, streaming adapter, oversampler,
/// cab-sim convolution engine, clear-model rebuild).
pub mod dsp_resources {
    /// The sample-rate resampler could not be constructed.
    pub const RESAMPLER_BUILD_FAILED: &str = "Failed to build the sample-rate resampler";
    /// The streaming resample adapter could not be constructed.
    pub const STREAM_BUILD_FAILED: &str = "Failed to build the streaming resample adapter";
    /// An oversampling engine could not be constructed.
    pub const OVERSAMPLER_BUILD_FAILED: &str = "Failed to build the oversampling engine";
    /// The cab-sim convolution engine/adapter could not be constructed.
    pub const CABSIM_BUILD_FAILED: &str = "Failed to build the cab-sim convolution engine";
    /// The stored cab-sim IR could not be resampled to the host sample rate.
    pub const CABSIM_IR_RESAMPLE_FAILED: &str =
        "Failed to resample the stored cab-sim IR to the host sample rate";
    /// The cab-sim IR raw-sample storage mutex is poisoned.
    pub const CABSIM_STORAGE_LOCK_POISONED: &str =
        "Cab-sim IR sample storage lock unavailable during plugin activation";
    /// The clear-model (Full restore without a model) resampler could not be built.
    pub const CLEAR_MODEL_RESAMPLER_FAILED: &str = "Failed to build the clear-model resampler";
    /// The clear-model (Full restore without a model) streaming adapter could not be built.
    pub const CLEAR_MODEL_STREAM_FAILED: &str = "Failed to build the clear-model streaming adapter";
}

/// State JSON transaction validation errors.
///
/// These are the messages a DAW surfaces when a project/preset state blob is
/// corrupt, stale, or references assets that no longer exist. They are the
/// primary target of the SA-04 heap-stability invariant: a host replaying a
/// broken preset in a loop must not grow the plugin heap.
pub mod state_txn {
    /// The state stream handed to the plugin was empty.
    pub const EMPTY_BUFFER: &str = "Empty state buffer";
    /// A Full-restore model path does not exist and no portable fallback matched.
    pub const MODEL_NOT_FOUND_OR_INVALID: &str = "Saved model not found or invalid";
    /// The portable model basename failed the security sanitisation.
    pub const MODEL_BASENAME_INVALID: &str = "Invalid model basename";
    /// A portable model reference carries no SHA-256 digest.
    pub const MODEL_HASH_MISSING: &str =
        "Model has no saved hash (re-load it explicitly via the GUI to migrate)";
    /// The saved model SHA-256 digest is malformed.
    pub const MODEL_HASH_MALFORMED: &str = "Saved model hash is malformed";
    /// The portable model could not be resolved in any canonical search directory.
    pub const MODEL_NOT_FOUND_PORTABLE: &str =
        "Preset/state model not found (searched canonical dirs)";
    /// The saved IR file does not exist.
    pub const IR_NOT_FOUND: &str = "Saved IR not found";
    /// The saved IR reference carries no SHA-256 digest.
    pub const IR_HASH_MISSING: &str =
        "Saved IR has no SHA-256 hash (re-load it explicitly via the GUI to migrate)";
    /// The saved IR SHA-256 digest is malformed.
    pub const IR_HASH_MALFORMED: &str = "Saved IR hash is malformed";
    /// The saved IR file exists but its SHA-256 digest diverges from the state.
    pub const IR_HASH_MISMATCH: &str = "Saved IR file hash mismatch";
    /// Hashing the saved IR file failed.
    pub const IR_HASH_FAILED: &str = "Failed to hash saved IR file";
}

/// Asset file hashing and identity failures (SHA-256 streaming digest).
pub mod assets {
    /// Reading the metadata of the asset to be hashed failed.
    pub const HASH_METADATA_FAILED: &str =
        "Failed to read file metadata while computing the asset SHA-256";
    /// The path to be hashed is not a regular file.
    pub const HASH_TARGET_NOT_REGULAR_FILE: &str = "Asset is not a regular file";
    /// The asset file exceeds the maximum size allowed for hashing.
    pub const HASH_FILE_TOO_LARGE: &str =
        "Asset file exceeds the maximum size allowed for SHA-256 hashing";
    /// Opening the asset file for hashing failed.
    pub const HASH_OPEN_FAILED: &str = "Failed to open asset file for SHA-256 hashing";
    /// Reading a chunk of the asset file while hashing failed.
    pub const HASH_READ_FAILED: &str = "Failed to read asset file while computing the SHA-256";
    /// The streamed asset exceeded the maximum size allowed for hashing.
    pub const HASH_STREAM_TOO_LARGE: &str =
        "Asset stream exceeded the maximum size allowed for SHA-256 hashing";
}

/// Builds a `PluginError::Message` without allocating or leaking.
///
/// `code` must be one of the static catalog constants above — it is the exact
/// message the DAW receives. The dynamic `detail` (path, cause, sizes) is
/// emitted exclusively through the logger and never reaches the host error
/// dialog, keeping both the dialog legible and the heap footprint constant.
///
/// # SA-04 invariant
///
/// Calling this repeatedly with the same `code` performs zero heap
/// allocations, so a host replaying corrupted states cannot grow plugin RAM.
#[inline]
pub fn static_plugin_error(code: &'static str, detail: impl std::fmt::Display) -> PluginError {
    log::error!("NAM-Plug [{}]: {}", code, detail);
    PluginError::Message(code)
}
