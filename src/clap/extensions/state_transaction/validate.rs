// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! State restoration validation phase: off-RT asset checks, model loading, and IR construction.

use super::RestoreMode;
use crate::clap::plugin::NamModelMetadata;
use crate::clap::plugin::errors::{self, static_plugin_error};
use clack_plugin::prelude::*;
use neural_amp_modeler_rs::common::diagnostics::{ModelInfo, NamDiagnostic, NamErrorCode};
use neural_amp_modeler_rs::common::params::ProcessingParams;
use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimPair;
use neural_amp_modeler_rs::dsp::cabsim::loader::CabSimIr;
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer;
use neural_amp_modeler_rs::loader::load_and_build_model;
use neural_amp_modeler_rs::models::NamModel;
use neural_amp_modeler_rs::models::StaticModel;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Validated model resources ready for transfer to the real-time processing thread.
///
/// Holds all heap-allocated DSP components and metadata constructed off-RT
/// during validation. These assets are passed across threads via lock-free SPSC
/// commands or staged for host restart cycles.
pub(crate) struct ModelResources {
    /// Pre-warmed model instance, boxed for heap transfer across the SPSC ring.
    pub(crate) model_l: Option<Box<StaticModel>>,
    /// Pre-warmed model instance for right channel in dual-mono mode.
    pub(crate) model_r: Option<Box<StaticModel>>,
    /// Multi-rate resampler matching host sample rate to model native rate.
    pub(crate) new_resampler: Box<NamResampler>,
    /// Streaming circular buffer for pitch/sample-rate conversion history.
    pub(crate) new_stream: Box<StreamingResampleBuffer>,
    /// Pre-computed input level multiplier adjustment (calibration).
    pub(crate) input_mult_adj: f32,
    /// Pre-computed output level multiplier adjustment (calibration).
    pub(crate) output_mult_adj: f32,
    /// Negotiated host block size (frames) at validation time. Carried so the
    /// restore path can size the off-RT prewarm-cost calibration block without
    /// exceeding the model's `max_buffer_size`.
    pub(crate) buffer_size: u32,
    /// Native sample rate expected by the model architecture.
    pub(crate) model_rate: u32,
    /// Model metadata extracted from the file header (name, author, architecture).
    pub(crate) model_metadata: NamModelMetadata,
    /// Static model diagnostics snapshot for error reporting and telemetry.
    pub(crate) model_info: ModelInfo,
    /// Streaming SHA-256 digest of the model file for cache validation and state tracking.
    pub(crate) model_hash: String,
}

/// Validated impulse response (IR) resources ready for convolution engine integration.
pub(crate) struct IrResources {
    /// Pre-configured convolution pair ready for atomic swap into the audio pipeline.
    pub(crate) pair: Option<Box<CabSimPair>>,
    /// Raw floating-point IR samples preserved for UI visualization or state archiving.
    pub(crate) samples: Vec<f32>,
    /// Sampling rate corresponding to the loaded IR waveform.
    pub(crate) sample_rate: u32,
}

/// Holds all resources that passed validation, ready for atomic commit.
///
/// Encapsulates the entire off-RT validation payload: parameter set, pre-warmed
/// model pipeline, impulse response adapter, disk paths, and cryptographic digests.
/// No further allocation or disk I/O is required once this structure is constructed.
pub(crate) struct ValidatedRestore {
    /// Plugin processing parameters extracted and validated from state.
    pub(crate) params: ProcessingParams,
    /// Validated model DSP resources, if a model was specified and verified.
    pub(crate) model: Option<ModelResources>,
    /// Canonical filesystem path to the model file on disk.
    pub(crate) model_path_on_disk: Option<PathBuf>,
    /// Human-readable file basename for UI display.
    pub(crate) model_basename: Option<String>,
    /// Directory to prepend to search paths if model was resolved dynamically.
    pub(crate) model_search_path_to_add: Option<PathBuf>,
    /// Cryptographic SHA-256 digest of the model file.
    pub(crate) model_hash: Option<String>,
    /// Validated IR convolution resources, if an IR was specified and verified.
    pub(crate) ir: Option<IrResources>,
    /// Canonical filesystem path to the impulse response file.
    pub(crate) ir_path_on_disk: Option<String>,
    /// Cryptographic SHA-256 digest of the impulse response file.
    pub(crate) ir_hash: Option<String>,
    /// Restored IR level policy.
    pub(crate) ir_level_policy: crate::clap::extensions::ir_level_policy::IrLevelPolicy,
}

/// Tuple result of off-RT model validation:
/// `(resources, path_on_disk, basename, search_path_to_add, sha256_hash)`.
pub(crate) type ModelValidationResult = (
    Option<ModelResources>,
    Option<PathBuf>,
    Option<String>,
    Option<PathBuf>,
    Option<String>,
);

/// Tuple result of off-RT impulse response validation:
/// `(resources, path_on_disk, sha256_hash)`.
pub(crate) type IrValidationResult = (Option<IrResources>, Option<String>, Option<String>);

/// Validates state parameters and off-RT assets, constructing all pre-warmed DSP structures.
///
/// This function runs strictly on the main/off-RT thread:
/// - Verifies file integrity, permissions, and cryptographic checksums on disk.
/// - Loads and parses neural net weights into heap-allocated static models.
/// - Configures multi-rate resamplers and streaming buffers.
/// - Decodes impulse response audio and sets up the FFT convolution engine.
///
/// Returns a [`ValidatedRestore`] containing all pre-warmed resources, or a descriptive
/// [`PluginError`] if any asset is missing, corrupted, or incompatible.
pub(crate) fn validate_and_build(
    loaded_params: &ProcessingParams,
    ir_level_policy: crate::clap::extensions::ir_level_policy::IrLevelPolicy,
    host_rate: u32,
    buffer_size: u32,
    sys: &neural_amp_modeler_rs::common::diagnostics::SystemSnapshot,
    mode: &RestoreMode,
) -> Result<ValidatedRestore, PluginError> {
    let maybe_model = match mode {
        RestoreMode::Full => validate_model_full(loaded_params, host_rate, buffer_size, sys)?,
        RestoreMode::ForPreset => {
            validate_model_preset(loaded_params, host_rate, buffer_size, sys)?
        }
    };

    let params = loaded_params.clone();

    let (maybe_ir, ir_path_on_disk, ir_hash) =
        validate_ir(loaded_params, ir_level_policy, host_rate, buffer_size, sys)?;

    Ok(ValidatedRestore {
        params,
        model: maybe_model.0,
        model_path_on_disk: maybe_model.1,
        model_basename: maybe_model.2,
        model_search_path_to_add: maybe_model.3,
        model_hash: maybe_model.4,
        ir: maybe_ir,
        ir_path_on_disk,
        ir_hash,
        ir_level_policy,
    })
}

/// Maximum allowed file size for hashing (256 MiB).
pub(crate) const MAX_FILE_HASH_SIZE: u64 = 256 * 1024 * 1024;

/// Computes the SHA-256 hex digest of a file's raw bytes in streaming chunks.
pub(crate) fn compute_file_hash(path: &Path) -> Result<String, PluginError> {
    use std::io::Read;

    let metadata = std::fs::metadata(path).map_err(|e| {
        static_plugin_error(
            errors::assets::HASH_METADATA_FAILED,
            format_args!("{path:?}: {e}"),
        )
    })?;

    if !metadata.file_type().is_file() {
        return Err(static_plugin_error(
            errors::assets::HASH_TARGET_NOT_REGULAR_FILE,
            format_args!("{path:?}"),
        ));
    }

    if metadata.len() > MAX_FILE_HASH_SIZE {
        return Err(static_plugin_error(
            errors::assets::HASH_FILE_TOO_LARGE,
            format_args!(
                "{} bytes > {MAX_FILE_HASH_SIZE} bytes: {path:?}",
                metadata.len()
            ),
        ));
    }

    let file = std::fs::File::open(path).map_err(|e| {
        static_plugin_error(
            errors::assets::HASH_OPEN_FAILED,
            format_args!("{path:?}: {e}"),
        )
    })?;

    let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut total_read: u64 = 0;

    loop {
        let bytes_read = reader.read(&mut buffer).map_err(|e| {
            static_plugin_error(
                errors::assets::HASH_READ_FAILED,
                format_args!("{path:?}: {e}"),
            )
        })?;

        if bytes_read == 0 {
            break;
        }

        total_read += bytes_read as u64;
        if total_read > MAX_FILE_HASH_SIZE {
            return Err(static_plugin_error(
                errors::assets::HASH_STREAM_TOO_LARGE,
                format_args!("{MAX_FILE_HASH_SIZE} bytes: {path:?}"),
            ));
        }

        hasher.update(&buffer[..bytes_read]);
    }

    let hash = hasher.finalize();
    // Lowercase hex via a 16-entry table instead of `format!` per byte: the
    // digest is on the hot state-restore path and per-byte formatting would
    // allocate 32 throwaway strings. Output is byte-identical to `{b:02x}`.
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut digest = String::with_capacity(hash.len() * 2);
    for &b in hash.iter() {
        digest.push(HEX[(b >> 4) as usize] as char);
        digest.push(HEX[(b & 0x0f) as usize] as char);
    }
    Ok(digest)
}

/// Returns true when `digest` is exactly 64 hex characters — the canonical
/// SHA-256 hex form produced by [`compute_file_hash`].
pub(crate) fn is_valid_sha256_hex(digest: &str) -> bool {
    digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Returns canonical search directories for portable model/IR lookup.
///
/// These are well-known directories where users store their NAM models, shared
/// across the NAM ecosystem. The order is:
/// 1. `~/.nam/models/` — NAM ecosystem convention
/// 2. `~/NAM Models/` — alternative common location
pub(crate) fn canonical_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let home = match std::env::var("HOME") {
        Ok(h) => PathBuf::from(h),
        Err(_) => return dirs,
    };

    let candidate = home.join(".nam").join("models");
    if candidate.is_dir() {
        dirs.push(candidate);
    }

    let candidate = home.join("NAM Models");
    if candidate.is_dir() {
        dirs.push(candidate);
    }

    dirs
}

/// Builds a model pair + resampler from a filesystem path, **without** any
/// side-effects on the main-thread state.
pub(crate) fn build_model_resources(
    path: &Path,
    host_rate: u32,
    buffer_size: u32,
    sys: &neural_amp_modeler_rs::common::diagnostics::SystemSnapshot,
) -> Result<ModelResources, Box<NamDiagnostic>> {
    let meta_before = std::fs::metadata(path).map_err(|e| {
        Box::new(
            NamDiagnostic::new(NamErrorCode::ModelBuildFailed, sys)
                .message(format!(
                    "Failed to read metadata before loading model: {:?}",
                    path
                ))
                .param("error", e.to_string()),
        )
    })?;
    let len_before = meta_before.len();
    let mtime_before = meta_before.modified().ok();

    let model_hash = compute_file_hash(path).map_err(|e| {
        Box::new(
            NamDiagnostic::new(NamErrorCode::ModelBuildFailed, sys)
                .message(format!("Failed to hash model file: {:?}", path))
                .param("error", e.to_string()),
        )
    })?;

    let model_pair = load_and_build_model(
        path,
        sys,
        true,
        neural_amp_modeler_rs::loader::LoadOptions::default(),
    )
    .map_err(|e| {
        Box::new(
            NamDiagnostic::new(NamErrorCode::ModelBuildFailed, sys)
                .message(format!("Failed to load model: {:?}", path))
                .param("error", e.to_string()),
        )
    })?;

    let meta_after = std::fs::metadata(path).map_err(|e| {
        Box::new(
            NamDiagnostic::new(NamErrorCode::ModelBuildFailed, sys)
                .message(format!(
                    "Failed to read metadata after loading model: {:?}",
                    path
                ))
                .param("error", e.to_string()),
        )
    })?;
    if meta_after.len() != len_before || meta_after.modified().ok() != mtime_before {
        return Err(Box::new(
            NamDiagnostic::new(NamErrorCode::ModelBuildFailed, sys)
                .message(format!("Model file was modified during load: {:?}", path))
                .hint("arquivo modificado durante a carga"),
        ));
    }

    if model_pair.model_l.is_none() {
        return Err(Box::new(
            NamDiagnostic::new(NamErrorCode::ModelBuildFailed, sys)
                .message(format!("Failed to build model: {:?}", path)),
        ));
    }

    let model_rate = model_pair.sample_rate;
    if host_rate != model_rate {
        log::warn!(
            "Model sample rate ({model_rate} Hz) differs from host sample rate ({host_rate} Hz); \
             resampler will convert internally"
        );
    }

    let model_info = model_pair.model_info(path);

    let mut model_l = model_pair.model_l;
    let mut model_r = model_pair.model_r;
    let input_mult_adj = model_pair.input_mult_adj;
    let output_mult_adj = model_pair.output_mult_adj;

    // Pre-size model if buffer size is known (borrow ends before model_l is moved)
    if buffer_size > 0
        && let Some(ref mut model) = model_l
        && let Err(e) = model.set_max_buffer_size(buffer_size as usize)
    {
        return Err(Box::new(
            NamDiagnostic::new(NamErrorCode::ModelBuildFailed, sys)
                .message("Failed to resize model buffers for host buffer size")
                .param("buffer_size", buffer_size.to_string())
                .param("error", e.to_string()),
        ));
    }

    if buffer_size > 0
        && let Some(ref mut model) = model_r
        && let Err(e) = model.set_max_buffer_size(buffer_size as usize)
    {
        return Err(Box::new(
            NamDiagnostic::new(NamErrorCode::ModelBuildFailed, sys)
                .message("Failed to resize model buffers for host buffer size (R)")
                .param("buffer_size", buffer_size.to_string())
                .param("error", e.to_string()),
        ));
    }

    let new_resampler = Box::new(
        NamResampler::new_simple(host_rate, model_rate).map_err(|e| {
            Box::new(
                NamDiagnostic::new(NamErrorCode::ModelBuildFailed, sys)
                    .message("Failed to build resampler")
                    .param("error", e.to_string()),
            )
        })?,
    );

    // Streaming resample adapter, sized for the host buffer.
    // When `buffer_size` is 0 (pre-activation restore), `flush_pending_model()`
    // builds it at activate() time from the deferred `PendingModel`.
    let new_stream = crate::clap::plugin::build_stream_adapter(
        host_rate,
        model_rate,
        buffer_size.max(1) as usize,
    )
    .map_err(|e| {
        Box::new(
            NamDiagnostic::new(NamErrorCode::ModelBuildFailed, sys)
                .message("Failed to build streaming resample buffer")
                .param("error", e.to_string()),
        )
    })?;

    let metadata = model_pair.metadata.clone();
    let architecture = model_pair.architecture.clone();
    let topology = model_pair.topology.clone();

    let model_metadata = NamModelMetadata {
        architecture,
        topology,
        sample_rate: model_rate,
        modeled_by: metadata.as_ref().and_then(|m| m.modeled_by.clone()),
        gear_make: metadata.as_ref().and_then(|m| m.gear_make.clone()),
        gear_model: metadata.as_ref().and_then(|m| m.gear_model.clone()),
        gear_type: metadata.as_ref().and_then(|m| m.gear_type.clone()),
        tone_type: metadata.as_ref().and_then(|m| m.tone_type.clone()),
        date: metadata
            .as_ref()
            .and_then(|m| m.date.as_ref())
            .map(|d| match (d.year, d.month, d.day) {
                (Some(y), Some(m), Some(d)) => format!("{:04}-{:02}-{:02}", y, m, d),
                (Some(y), Some(m), None) => format!("{:04}-{:02}", y, m),
                (Some(y), None, None) => format!("{:04}", y),
                _ => String::new(),
            })
            .filter(|s| !s.is_empty()),
    };
    Ok(ModelResources {
        model_l,
        model_r,
        new_resampler,
        new_stream,
        input_mult_adj,
        output_mult_adj,
        buffer_size,
        model_rate,
        model_metadata,
        model_info,
        model_hash,
    })
}

/// Sanitizes a file basename, ensuring it contains no directory separators,
/// traversal components (`..`), or control characters, and is strictly a single filename.
pub(crate) fn sanitize_basename(raw_name: &str) -> Option<&str> {
    if raw_name.is_empty()
        || raw_name.contains('/')
        || raw_name.contains('\\')
        || raw_name.contains("..")
        || raw_name.contains('\0')
    {
        return None;
    }
    let p = Path::new(raw_name);
    let file_name = p.file_name()?.to_str()?;
    if file_name == raw_name {
        Some(file_name)
    } else {
        None
    }
}

/// Resolves a candidate asset file inside a search directory, canonicalizing both
/// and verifying that the candidate resides strictly within the directory hierarchy.
pub(crate) fn resolve_confined_candidate(dir: &Path, clean_basename: &str) -> Option<PathBuf> {
    let dir_canon = dir.canonicalize().ok()?;
    let candidate = dir_canon.join(clean_basename);
    if !candidate.exists() {
        return None;
    }
    let cand_canon = candidate.canonicalize().ok()?;
    if cand_canon.starts_with(&dir_canon) {
        Some(cand_canon)
    } else {
        log::warn!(
            "NAM-Plug: Asset {:?} resolved outside search directory {:?}; access denied",
            candidate,
            dir_canon
        );
        None
    }
}

pub(crate) fn validate_model_full(
    loaded_params: &ProcessingParams,
    host_rate: u32,
    buffer_size: u32,
    sys: &neural_amp_modeler_rs::common::diagnostics::SystemSnapshot,
) -> Result<ModelValidationResult, PluginError> {
    let Some(ref path) = loaded_params.model_path else {
        // ForPreset blobs carry model_path=None but model_basename + model_hash.
        // Fall through to portable basename search so state.load() restores presets equivalently
        // to state_context.load(ForPreset).
        return validate_model_from_basename(loaded_params, host_rate, buffer_size, sys);
    };

    if path.exists() {
        // Mandatory asset identity: an asset is only adopted with a valid
        // SHA-256 digest that is verified in this same restore cycle. A missing,
        // malformed or divergent hash rejects this path — never a silent accept.
        let hash_valid = match loaded_params.model_hash.as_deref() {
            Some(expected) if is_valid_sha256_hex(expected) => match compute_file_hash(path) {
                Ok(actual) => {
                    if actual.eq_ignore_ascii_case(expected) {
                        true
                    } else {
                        log::warn!(
                            "NAM-Plug: Model file at original path {:?} has hash mismatch (expected {}, got {}); attempting fallback",
                            path,
                            expected,
                            actual
                        );
                        false
                    }
                }
                Err(e) => {
                    log::warn!("NAM-Plug: Failed to hash model file at {:?}: {}", path, e);
                    false
                }
            },
            Some(malformed) => {
                log::error!(
                    "NAM-Plug: State restore rejected — model_hash is malformed ({} chars, expected 64 hex): {malformed:?}",
                    malformed.len()
                );
                false
            }
            None => {
                log::error!(
                    "NAM-Plug: State restore rejected — model at {:?} has no saved SHA-256 hash. \
                     Re-load the model explicitly via the GUI to migrate this project.",
                    path
                );
                false
            }
        };

        if hash_valid {
            let resources = build_model_resources(path, host_rate, buffer_size, sys)
                .map_err(|e| PluginError::Error(e))?;
            let basename = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string());
            let search_path = path.parent().map(|p| p.to_path_buf());
            let hash = Some(resources.model_hash.clone());
            return Ok((
                Some(resources),
                Some(path.clone()),
                basename,
                search_path,
                hash,
            ));
        }
    }

    // Try portable basename fallback if direct path did not exist or failed hash verification
    if loaded_params.model_basename.is_some() {
        return validate_model_from_basename(loaded_params, host_rate, buffer_size, sys);
    }

    log::error!(
        "NAM-Plug: State restore failed — model not found or failed validation at {path:?}"
    );
    Err(PluginError::Message(
        errors::state_txn::MODEL_NOT_FOUND_OR_INVALID,
    ))
}

pub(crate) fn validate_model_preset(
    loaded_params: &ProcessingParams,
    host_rate: u32,
    buffer_size: u32,
    sys: &neural_amp_modeler_rs::common::diagnostics::SystemSnapshot,
) -> Result<ModelValidationResult, PluginError> {
    validate_model_from_basename(loaded_params, host_rate, buffer_size, sys)
}

/// Shared basename-based portable model resolution with canonical search,
/// directory confinement, and content-hash verification. Used by both
/// `validate_model_preset()` and `validate_model_full()` when `model_path` is None or fails.
pub(crate) fn validate_model_from_basename(
    loaded_params: &ProcessingParams,
    host_rate: u32,
    buffer_size: u32,
    sys: &neural_amp_modeler_rs::common::diagnostics::SystemSnapshot,
) -> Result<ModelValidationResult, PluginError> {
    let Some(ref raw_basename) = loaded_params.model_basename else {
        return Ok((None, None, None, None, None));
    };

    let clean_basename = sanitize_basename(raw_basename).ok_or_else(|| {
        log::error!("NAM-Plug: Insecure or invalid model basename: {raw_basename:?}");
        PluginError::Message(errors::state_txn::MODEL_BASENAME_INVALID)
    })?;

    // Mandatory asset identity: a model reference without a valid expected digest is never adopted
    // silently. Missing/malformed hash ⇒ explicit rejection in automatic
    // restore; the only migration path is an explicit GUI re-load (which
    // recomputes the digest from the file the user picks).
    let expected_hash = loaded_params.model_hash.as_deref().ok_or_else(|| {
        log::error!(
            "NAM-Plug: State restore rejected — model basename {raw_basename:?} carries no SHA-256 \
             hash. Re-load the model explicitly via the GUI to migrate this preset/project."
        );
        PluginError::Message(errors::state_txn::MODEL_HASH_MISSING)
    })?;

    if !is_valid_sha256_hex(expected_hash) {
        log::error!(
            "NAM-Plug: State restore rejected — model_hash is malformed ({} chars, expected 64 \
             hex) for basename {raw_basename:?}",
            expected_hash.len()
        );
        return Err(PluginError::Message(
            errors::state_txn::MODEL_HASH_MALFORMED,
        ));
    }

    // Search chain: loaded search paths first, then canonical dirs.
    // Hash verification ensures content identity when multiple files share a basename.
    let search_dirs: Vec<PathBuf> = loaded_params
        .model_search_paths
        .iter()
        .cloned()
        .chain(canonical_search_dirs())
        .collect();

    for dir in &search_dirs {
        let Some(candidate) = resolve_confined_candidate(dir, clean_basename) else {
            continue;
        };

        match compute_file_hash(&candidate) {
            Ok(actual) if actual.eq_ignore_ascii_case(expected_hash) => {
                let resources = build_model_resources(&candidate, host_rate, buffer_size, sys)
                    .map_err(|e| PluginError::Error(e))?;
                let basename_from_path = candidate
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|s| s.to_string());
                let search_path = candidate.parent().map(|p| p.to_path_buf());
                let hash = Some(resources.model_hash.clone());
                log::info!("NAM-Plug: Resolved model via basename search: {candidate:?}");
                return Ok((
                    Some(resources),
                    Some(candidate),
                    basename_from_path,
                    search_path,
                    hash,
                ));
            }
            Ok(actual) => {
                log::debug!(
                    "NAM-Plug: Skipping {candidate:?} — hash mismatch (expected {expected_hash}, got {actual})"
                );
                continue;
            }
            Err(e) => {
                log::warn!(
                    "NAM-Plug: Skipping {candidate:?} — failed to hash candidate ({e}); refusing unverified adoption"
                );
                continue;
            }
        }
    }

    let searched = search_dirs
        .iter()
        .map(|d| format!("{d:?}/"))
        .collect::<Vec<_>>()
        .join(", ");
    log::error!(
        "NAM-Plug: State restore failed — model basename {clean_basename:?} not found in: [{searched}]"
    );
    Err(PluginError::Message(
        errors::state_txn::MODEL_NOT_FOUND_PORTABLE,
    ))
}

/// Computes the SHA-256 hex digest of a byte slice in memory.
pub(crate) fn compute_bytes_hash(bytes: &[u8]) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(bytes);
    let hash = hasher.finalize();
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut digest = String::with_capacity(hash.len() * 2);
    for &b in hash.iter() {
        digest.push(HEX[(b >> 4) as usize] as char);
        digest.push(HEX[(b & 0x0f) as usize] as char);
    }
    digest
}

/// Reads a file into a byte buffer bounded by [`MAX_FILE_HASH_SIZE`].
pub(crate) fn read_ir_file_bytes(path: &Path) -> Result<Vec<u8>, PluginError> {
    use std::io::Read;

    let metadata = std::fs::metadata(path).map_err(|e| {
        static_plugin_error(
            errors::assets::HASH_METADATA_FAILED,
            format_args!("{path:?}: {e}"),
        )
    })?;

    if !metadata.file_type().is_file() {
        return Err(static_plugin_error(
            errors::assets::HASH_TARGET_NOT_REGULAR_FILE,
            format_args!("{path:?}"),
        ));
    }

    let file_size = metadata.len();
    if file_size > MAX_FILE_HASH_SIZE {
        return Err(static_plugin_error(
            errors::assets::HASH_FILE_TOO_LARGE,
            format_args!("{path:?} ({file_size} > {MAX_FILE_HASH_SIZE})"),
        ));
    }

    let mut file = std::fs::File::open(path).map_err(|e| {
        static_plugin_error(
            errors::assets::HASH_OPEN_FAILED,
            format_args!("{path:?}: {e}"),
        )
    })?;

    // Cap the allocation at `MAX_FILE_HASH_SIZE + 1`: metadata can be stale
    // (the file may grow between `metadata()` and the read), and an unbounded
    // `read_to_end` would let a hostile asset exhaust memory.
    let mut data = Vec::with_capacity(file_size as usize);
    file.by_ref()
        .take(MAX_FILE_HASH_SIZE + 1)
        .read_to_end(&mut data)
        .map_err(|e| {
            static_plugin_error(
                errors::assets::HASH_READ_FAILED,
                format_args!("{path:?}: {e}"),
            )
        })?;
    if data.len() as u64 > MAX_FILE_HASH_SIZE {
        return Err(static_plugin_error(
            errors::assets::HASH_FILE_TOO_LARGE,
            format_args!("{path:?} grew past {MAX_FILE_HASH_SIZE} bytes while reading"),
        ));
    }

    Ok(data)
}

/// Loads [`CabSimIr`] from verified in-memory bytes, eliminating TOCTOU races
/// with external filesystem replacement.
pub(crate) fn load_cabsim_from_verified_bytes(
    data: &[u8],
    _host_rate: u32,
    sys: &neural_amp_modeler_rs::common::diagnostics::SystemSnapshot,
    path_for_log: &Path,
) -> Result<Box<CabSimIr>, Box<NamDiagnostic>> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    // GAP(A-01, D7a): the frozen published engine exposes only
    // `CabSimIr::load(&Path, ..)`; it cannot decode a WAV from an in-memory
    // slice. The verified bytes therefore have to be staged to a private temp
    // file and re-opened by the engine. Identity is still coherent because the
    // exact verified buffer is written (never the source path again) and the
    // file is created exclusively (`create_new`), so no symlink or pre-existing
    // file can substitute different content. Escalated to the PO: removing the
    // temp round-trip requires a new engine API (`CabSimIr::load_from_bytes`).
    static IR_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();

    let (tmp_path, mut file) = loop {
        let count = IR_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp_path = std::env::temp_dir().join(format!("nam_ir_{pid}_{count}.wav"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
        {
            Ok(file) => break (tmp_path, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(Box::new(
                    NamDiagnostic::new(NamErrorCode::IrLoadFailed, sys)
                        .message(format!(
                            "Failed to stage verified IR bytes: {:?}",
                            path_for_log
                        ))
                        .param("error", e.to_string()),
                ));
            }
        }
    };

    struct TempFileGuard<'a>(&'a Path);
    impl Drop for TempFileGuard<'_> {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.0);
        }
    }
    let _guard = TempFileGuard(&tmp_path);

    file.write_all(data).map_err(|e| {
        Box::new(
            NamDiagnostic::new(NamErrorCode::IrLoadFailed, sys)
                .message(format!(
                    "Failed to stage verified IR bytes: {:?}",
                    path_for_log
                ))
                .param("error", e.to_string()),
        )
    })?;
    drop(file);

    CabSimIr::load(&tmp_path, 0, false).map_err(|e| {
        Box::new(
            NamDiagnostic::new(NamErrorCode::IrLoadFailed, sys)
                .message(format!("Failed to load cab-sim IR: {:?}", path_for_log))
                .param("error", e.to_string()),
        )
    })
}

/// Builds IR resources directly from an in-memory byte slice without re-reading the target file.
pub(crate) fn build_ir_resources_from_bytes(
    data: &[u8],
    path_for_diagnostic: &Path,
    ir_level_policy: crate::clap::extensions::ir_level_policy::IrLevelPolicy,
    host_rate: u32,
    buffer_size: u32,
    sys: &neural_amp_modeler_rs::common::diagnostics::SystemSnapshot,
) -> Result<IrResources, Box<NamDiagnostic>> {
    let partition_size = if buffer_size > 0 {
        buffer_size as usize
    } else {
        256
    };

    let cabsim = load_cabsim_from_verified_bytes(data, host_rate, sys, path_for_diagnostic)?;
    let pair = crate::clap::extensions::ir_level_policy::build_cabsim_pair_from_raw(
        &cabsim.samples,
        cabsim.original_rate,
        host_rate,
        partition_size,
        ir_level_policy,
        sys,
    )?;

    Ok(IrResources {
        pair: Some(Box::new(pair)),
        samples: cabsim.samples,
        sample_rate: cabsim.original_rate,
    })
}

/// Builds IR resources from a filesystem path without side-effects (reads bytes once).
#[expect(dead_code)]
pub(crate) fn build_ir_resources(
    path: &Path,
    host_rate: u32,
    buffer_size: u32,
    sys: &neural_amp_modeler_rs::common::diagnostics::SystemSnapshot,
) -> Result<IrResources, Box<NamDiagnostic>> {
    let bytes = read_ir_file_bytes(path).map_err(|e| {
        Box::new(
            NamDiagnostic::new(NamErrorCode::IrLoadFailed, sys)
                .message(format!("Failed to read IR bytes: {:?}", path))
                .param("error", e.to_string()),
        )
    })?;
    build_ir_resources_from_bytes(
        &bytes,
        path,
        crate::clap::extensions::ir_level_policy::IrLevelPolicy::default(),
        host_rate,
        buffer_size,
        sys,
    )
}

pub(crate) fn validate_ir(
    loaded_params: &ProcessingParams,
    ir_level_policy: crate::clap::extensions::ir_level_policy::IrLevelPolicy,
    host_rate: u32,
    buffer_size: u32,
    sys: &neural_amp_modeler_rs::common::diagnostics::SystemSnapshot,
) -> Result<IrValidationResult, PluginError> {
    let Some(ref ir_path) = loaded_params.ir_path else {
        return Ok((None, None, None));
    };

    if !ir_path.exists() {
        log::error!("NAM-Plug: State restore failed — IR not found at {ir_path:?}");
        return Err(PluginError::Message(errors::state_txn::IR_NOT_FOUND));
    }

    // Mandatory asset identity: the same mandatory-digest rule applies to the IR. A missing or
    // malformed `ir_hash` never loads the WAV; the migration path is an
    // explicit GUI re-load of the IR file.
    let expected_hash = loaded_params.ir_hash.as_deref().ok_or_else(|| {
        log::error!(
            "NAM-Plug: State restore rejected — IR at {ir_path:?} carries no SHA-256 hash. \
             Re-load the IR explicitly via the GUI to migrate this project."
        );
        PluginError::Message(errors::state_txn::IR_HASH_MISSING)
    })?;

    if !is_valid_sha256_hex(expected_hash) {
        log::error!(
            "NAM-Plug: State restore rejected — ir_hash is malformed ({} chars, expected 64 hex) for {ir_path:?}",
            expected_hash.len()
        );
        return Err(PluginError::Message(errors::state_txn::IR_HASH_MALFORMED));
    }

    // Read bytes once: hash computation and decoding operate on the exact same buffer in memory.
    let ir_bytes = read_ir_file_bytes(ir_path).map_err(|e| {
        log::error!("NAM-Plug: Failed to read IR file at {:?}: {}", ir_path, e);
        PluginError::Message(errors::state_txn::IR_HASH_FAILED)
    })?;

    let actual_hash = compute_bytes_hash(&ir_bytes);
    if !actual_hash.eq_ignore_ascii_case(expected_hash) {
        log::error!(
            "NAM-Plug: IR file at {:?} hash mismatch (expected {}, got {})",
            ir_path,
            expected_hash,
            actual_hash
        );
        return Err(PluginError::Message(errors::state_txn::IR_HASH_MISMATCH));
    }

    let resources = build_ir_resources_from_bytes(
        &ir_bytes,
        ir_path,
        ir_level_policy,
        host_rate,
        buffer_size,
        sys,
    )
    .map_err(|e| PluginError::Error(e))?;
    Ok((
        Some(resources),
        Some(ir_path.to_string_lossy().to_string()),
        Some(actual_hash),
    ))
}
