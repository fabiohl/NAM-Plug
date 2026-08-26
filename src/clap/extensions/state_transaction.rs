// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Centralised transactional pipeline for state and state-context restoration.
//!
//! Implements a 3-phase pipeline (Prepare → Validate → Commit) that guarantees:
//! - Any asset validation failure returns an error **without altering active DSP state**
//! - Successful restores publish all resources atomically
//!
//! # Phases
//! 1. **Prepare** — deserialise the state blob into `ProcessingParams`
//! 2. **Validate** — off-RT validation: check paths, load/build model, IR, resamplers
//! 3. **Commit** — publish params + payloads via atomics and SPSC (only after full validation)

use crate::clap::plugin::ClapParamPayload;
use crate::clap::plugin::LoadModelPayload;
use crate::clap::plugin::NamClapMainThread;
use crate::clap::plugin::NamModelMetadata;
use crate::clap::plugin::PendingModel;
use crate::clap::plugin::PendingRestartOs;
use crate::clap::plugin::PendingRestore;
use crate::clap::plugin::RestoreModelPublish;
use crate::clap::plugin::RestorePublish;
use crate::clap::plugin::RestoreTxn;
use crate::clap::plugin::StagedRestore;
use crate::clap::plugin::command_scheduler::PushError;
use crate::clap::plugin::debug_assert_main_thread;
use clack_plugin::prelude::*;
use neural_amp_modeler_rs::common::diagnostics::{ModelInfo, NamDiagnostic, NamErrorCode};
use neural_amp_modeler_rs::common::params::ProcessingParams;
use neural_amp_modeler_rs::common::params::RtProcessingParams;
use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter;
use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;
use neural_amp_modeler_rs::dsp::cabsim::loader::CabSimIr;
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer;
use neural_amp_modeler_rs::loader::load_and_build_model;
use neural_amp_modeler_rs::models::NamModel;
use neural_amp_modeler_rs::models::StaticModel;
use neural_amp_modeler_rs::models::slimmable::clone_wavenet_for_slimmable_storage;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

struct ModelResources {
    model_l: Option<Box<StaticModel>>,
    new_resampler: Box<NamResampler>,
    new_stream: Box<StreamingResampleBuffer>,
    input_mult_adj: f32,
    output_mult_adj: f32,
    model_rate: u32,
    model_metadata: NamModelMetadata,
    model_info: ModelInfo,
    model_hash: String,
}

struct IrResources {
    adapter: Option<Box<CabSimAdapter>>,
    samples: Vec<f32>,
    sample_rate: u32,
}

/// Holds all resources that passed validation, ready for atomic commit.
struct ValidatedRestore {
    params: ProcessingParams,
    model: Option<ModelResources>,
    model_path_on_disk: Option<PathBuf>,
    model_basename: Option<String>,
    model_search_path_to_add: Option<PathBuf>,
    model_hash: Option<String>,
    ir: Option<IrResources>,
    ir_path_on_disk: Option<String>,
    ir_hash: Option<String>,
}

/// Restore mode: `Full` for project/duplicate, `ForPreset` for preset bank loads.
pub(crate) enum RestoreMode {
    Full,
    ForPreset,
}

/// Entry-point for the 3-phase transactional state restore.
pub(crate) fn restore_state_transactional(
    buffer: &[u8],
    main_thread: &mut NamClapMainThread,
    mode: RestoreMode,
) -> Result<(), PluginError> {
    debug_assert_main_thread(&main_thread.host);

    if buffer.is_empty() {
        log::debug!("Empty state buffer, returning error");
        return Err(PluginError::Message("Empty state buffer"));
    }

    // ══════ Phase 1: PREPARE — deserialise ══════
    let loaded_params = super::state::load_state(buffer)?;

    let host_rate = {
        let rate = main_thread.shared.cold.sample_rate.load(Ordering::Relaxed);
        if rate == 0 { 48000 } else { rate }
    };
    let buffer_size = main_thread.shared.cold.buffer_size.load(Ordering::Relaxed);

    // ══════ Phase 2: VALIDATE — build resources off-RT ══════
    let validated = validate_and_build(
        &loaded_params,
        host_rate,
        buffer_size,
        &main_thread.sys,
        &mode,
    )?;

    // ══════ Phase 3: COMMIT — publish atomically ══════
    commit(validated, main_thread, &mode)?;

    Ok(())
}

type ModelValidationResult = (
    Option<ModelResources>,
    Option<PathBuf>,
    Option<String>,
    Option<PathBuf>,
    Option<String>,
);

type IrValidationResult = (Option<IrResources>, Option<String>, Option<String>);

fn validate_and_build(
    loaded_params: &ProcessingParams,
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
        validate_ir(loaded_params, host_rate, buffer_size, sys)?;

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
    })
}

/// Maximum allowed file size for hashing (256 MiB).
pub(crate) const MAX_FILE_HASH_SIZE: u64 = 256 * 1024 * 1024;

/// Computes the SHA-256 hex digest of a file's raw bytes in streaming chunks.
pub(crate) fn compute_file_hash(path: &Path) -> Result<String, PluginError> {
    use std::io::Read;

    let metadata = std::fs::metadata(path).map_err(|e| {
        PluginError::Message(Box::leak(
            format!("Failed to read metadata for hashing ({path:?}): {e}").into_boxed_str(),
        ))
    })?;

    if !metadata.file_type().is_file() {
        return Err(PluginError::Message(Box::leak(
            format!("Target path for hashing is not a regular file: {path:?}").into_boxed_str(),
        )));
    }

    if metadata.len() > MAX_FILE_HASH_SIZE {
        return Err(PluginError::Message(Box::leak(
            format!(
                "File size ({} bytes) exceeds maximum hash size ({} bytes): {path:?}",
                metadata.len(),
                MAX_FILE_HASH_SIZE
            )
            .into_boxed_str(),
        )));
    }

    let file = std::fs::File::open(path).map_err(|e| {
        PluginError::Message(Box::leak(
            format!("Failed to open file for hashing ({path:?}): {e}").into_boxed_str(),
        ))
    })?;

    let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut total_read: u64 = 0;

    loop {
        let bytes_read = reader.read(&mut buffer).map_err(|e| {
            PluginError::Message(Box::leak(
                format!("Failed to read file chunk during hashing ({path:?}): {e}")
                    .into_boxed_str(),
            ))
        })?;

        if bytes_read == 0 {
            break;
        }

        total_read += bytes_read as u64;
        if total_read > MAX_FILE_HASH_SIZE {
            return Err(PluginError::Message(Box::leak(
                format!(
                    "Stream exceeded maximum hash size ({} bytes): {path:?}",
                    MAX_FILE_HASH_SIZE
                )
                .into_boxed_str(),
            )));
        }

        hasher.update(&buffer[..bytes_read]);
    }

    let hash = hasher.finalize();
    Ok(hash.iter().map(|b| format!("{b:02x}")).collect())
}

/// Returns true when `digest` is exactly 64 hex characters — the canonical
/// SHA-256 hex form produced by [`compute_file_hash`] (T6.2).
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
fn build_model_resources(
    path: &Path,
    host_rate: u32,
    buffer_size: u32,
    sys: &neural_amp_modeler_rs::common::diagnostics::SystemSnapshot,
) -> Result<ModelResources, Box<NamDiagnostic>> {
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
        false,
        neural_amp_modeler_rs::loader::LoadOptions::default(),
    )
    .map_err(|e| {
        Box::new(
            NamDiagnostic::new(NamErrorCode::ModelBuildFailed, sys)
                .message(format!("Failed to load model: {:?}", path))
                .param("error", e.to_string()),
        )
    })?;

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

    let new_resampler = Box::new(NamResampler::new(host_rate, model_rate, 0).map_err(|e| {
        Box::new(
            NamDiagnostic::new(NamErrorCode::ModelBuildFailed, sys)
                .message("Failed to build resampler")
                .param("error", e.to_string()),
        )
    })?);

    // Streaming resample adapter (T1.2/F-PERF-002), sized for the host buffer.
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
        new_resampler,
        new_stream,
        input_mult_adj,
        output_mult_adj,
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

fn validate_model_full(
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
        // T6.2 (F-ROB-PLUG-08 residual): an asset is only adopted with a valid
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
    Err(PluginError::Message(Box::leak(
        format!("Saved model not found or invalid: {:?}", path).into_boxed_str(),
    )))
}

fn validate_model_preset(
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
fn validate_model_from_basename(
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
        PluginError::Message(Box::leak(
            format!("Invalid model basename: {:?}", raw_basename).into_boxed_str(),
        ))
    })?;

    // T6.2: a model reference without a valid expected digest is never adopted
    // silently. Missing/malformed hash ⇒ explicit rejection in automatic
    // restore; the only migration path is an explicit GUI re-load (which
    // recomputes the digest from the file the user picks).
    let expected_hash = loaded_params.model_hash.as_deref().ok_or_else(|| {
        log::error!(
            "NAM-Plug: State restore rejected — model basename {raw_basename:?} carries no SHA-256 \
             hash. Re-load the model explicitly via the GUI to migrate this preset/project."
        );
        PluginError::Message(Box::leak(
            format!(
                "Model has no saved hash: {clean_basename} (re-load it explicitly via the GUI to migrate)"
            )
            .into_boxed_str(),
        ))
    })?;

    if !is_valid_sha256_hex(expected_hash) {
        log::error!(
            "NAM-Plug: State restore rejected — model_hash is malformed ({} chars, expected 64 \
             hex) for basename {raw_basename:?}",
            expected_hash.len()
        );
        return Err(PluginError::Message(Box::leak(
            format!(
                "Saved model hash is malformed: {clean_basename} ({} chars, expected 64 hex)",
                expected_hash.len()
            )
            .into_boxed_str(),
        )));
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
    Err(PluginError::Message(Box::leak(
        format!(
            "Preset/state model not found: {} (searched canonical dirs)",
            clean_basename
        )
        .into_boxed_str(),
    )))
}

/// Builds IR resources from a filesystem path without side-effects.
fn build_ir_resources(
    path: &Path,
    host_rate: u32,
    buffer_size: u32,
    sys: &neural_amp_modeler_rs::common::diagnostics::SystemSnapshot,
) -> Result<IrResources, Box<NamDiagnostic>> {
    let partition_size = if buffer_size > 0 {
        buffer_size as usize
    } else {
        256
    };

    let cabsim = CabSimIr::load(path, host_rate, true).map_err(|e| {
        Box::new(
            NamDiagnostic::new(NamErrorCode::IrLoadFailed, sys)
                .message(format!("Failed to load cab-sim IR: {:?}", path))
                .param("error", e.to_string()),
        )
    })?;

    let engine = ConvEngine::new(&cabsim.samples, partition_size).map_err(|e| {
        Box::new(
            NamDiagnostic::new(e, sys)
                .message("Failed to build convolution engine for cab-sim IR")
                .hint("The IR samples require more memory than available."),
        )
    })?;

    Ok(IrResources {
        adapter: Some(Box::new(CabSimAdapter::new(Box::new(engine)).map_err(
            |e| {
                Box::new(
                    NamDiagnostic::new(e, sys)
                        .message("Failed to build cab-sim convolution adapter")
                        .hint("The IR samples require more memory than available."),
                )
            },
        )?)),
        samples: cabsim.samples,
        sample_rate: cabsim.sample_rate,
    })
}

fn validate_ir(
    loaded_params: &ProcessingParams,
    host_rate: u32,
    buffer_size: u32,
    sys: &neural_amp_modeler_rs::common::diagnostics::SystemSnapshot,
) -> Result<IrValidationResult, PluginError> {
    let Some(ref ir_path) = loaded_params.ir_path else {
        return Ok((None, None, None));
    };

    if !ir_path.exists() {
        log::error!("NAM-Plug: State restore failed — IR not found at {ir_path:?}");
        return Err(PluginError::Message(Box::leak(
            format!("Saved IR not found: {:?}", ir_path).into_boxed_str(),
        )));
    }

    // T6.2: the same mandatory-digest rule applies to the IR. A missing or
    // malformed `ir_hash` never loads the WAV; the migration path is an
    // explicit GUI re-load of the IR file.
    let expected_hash = loaded_params.ir_hash.as_deref().ok_or_else(|| {
        log::error!(
            "NAM-Plug: State restore rejected — IR at {ir_path:?} carries no SHA-256 hash. \
             Re-load the IR explicitly via the GUI to migrate this project."
        );
        PluginError::Message(
            "Saved IR has no SHA-256 hash (re-load it explicitly via the GUI to migrate)",
        )
    })?;

    if !is_valid_sha256_hex(expected_hash) {
        log::error!(
            "NAM-Plug: State restore rejected — ir_hash is malformed ({} chars, expected 64 hex) for {ir_path:?}",
            expected_hash.len()
        );
        return Err(PluginError::Message(Box::leak(
            format!(
                "Saved IR hash is malformed ({} chars, expected 64 hex)",
                expected_hash.len()
            )
            .into_boxed_str(),
        )));
    }

    let actual_hash = compute_file_hash(ir_path).map_err(|e| {
        log::error!("NAM-Plug: Failed to hash IR file at {:?}: {}", ir_path, e);
        PluginError::Message("Failed to hash saved IR file")
    })?;
    if !actual_hash.eq_ignore_ascii_case(expected_hash) {
        log::error!(
            "NAM-Plug: IR file at {:?} hash mismatch (expected {}, got {})",
            ir_path,
            expected_hash,
            actual_hash
        );
        return Err(PluginError::Message("Saved IR file hash mismatch"));
    }

    let resources = build_ir_resources(ir_path, host_rate, buffer_size, sys)
        .map_err(|e| PluginError::Error(e))?;
    Ok((
        Some(resources),
        Some(ir_path.to_string_lossy().to_string()),
        Some(actual_hash),
    ))
}

/// Monotonic generation tag for restore transactions (T6.1).
static NEXT_RESTORE_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn next_restore_generation() -> u64 {
    NEXT_RESTORE_GENERATION.fetch_add(1, Ordering::Relaxed) + 1
}

/// Publishes the validated restore transactionally. Only reached after all
/// validation passes (T6.1 / F-ROB-PLUG-07).
///
/// - Active (`buffer_size > 0`): the whole package (model, IR, params) is pushed
///   as a single [`RestoreTxn`] command. UI/paths/hashes are published only after
///   the audio thread acks its sequence number (`flush_pending_restore`).
/// - Pre-activate (`buffer_size == 0`): local commit — no audio thread exists to
///   desync from, so everything is published immediately and the model is
///   retained in `pending_model` for `activate()`.
fn commit(
    validated: ValidatedRestore,
    main_thread: &mut NamClapMainThread,
    mode: &RestoreMode,
) -> Result<(), PluginError> {
    let buffer_size = main_thread.shared.cold.buffer_size.load(Ordering::Relaxed);
    if buffer_size == 0 {
        return local_commit(validated, main_thread, mode);
    }
    atomic_commit(validated, main_thread, mode)
}

/// Active path: builds the atomic [`RestoreTxn`] and either delivers it
/// continuously through SPSC (same physical latency) or stages it for the next
/// host restart cycle (different physical latency, Política A / TR.1).
fn atomic_commit(
    validated: ValidatedRestore,
    main_thread: &mut NamClapMainThread,
    mode: &RestoreMode,
) -> Result<(), PluginError> {
    let host_rate = {
        let rate = main_thread.shared.cold.sample_rate.load(Ordering::Relaxed);
        if rate == 0 { 48000 } else { rate }
    };
    let buffer_size = main_thread.shared.cold.buffer_size.load(Ordering::Relaxed);
    let (publish, txn) = build_restore_package(
        validated,
        &main_thread.params,
        host_rate,
        buffer_size,
        mode,
        &main_thread.shared.cold,
    )?;

    // Check if the restore changes physical latency (T3.3 / F-LAT-005 / TR.1 Política A).
    let current_stream_latency = main_thread
        .shared
        .cold
        .current_stream_latency
        .load(Ordering::Relaxed);
    let current_cabsim_latency = main_thread
        .shared
        .cold
        .current_cabsim_latency
        .load(Ordering::Relaxed);

    let stream_latency_differs = match txn.model.as_ref() {
        Some(m) => m.new_stream.latency_samples() != current_stream_latency,
        None => false,
    };

    let cabsim_latency_differs = match txn.ir.as_ref() {
        Some(maybe_adapter) => {
            let new_lat = maybe_adapter
                .as_ref()
                .map_or(0, |a| a.latency_samples() as u32);
            new_lat != current_cabsim_latency
        }
        None => false,
    };

    let current_os = OversampleFactor::from_f32(
        main_thread
            .shared
            .ui_to_rt
            .param_oversample
            .load(Ordering::Relaxed) as f32,
    );
    let pending_os = PendingRestartOs::load(
        &main_thread.shared.cold.pending_restart_os_factor,
        Ordering::Relaxed,
    );
    let os_differs =
        publish.params.oversample != current_os || pending_os != PendingRestartOs::None;

    let latency_differs = stream_latency_differs || cabsim_latency_differs || os_differs;

    if !latency_differs {
        // Same latency ⇒ continuous atomic restore through SPSC (ack-gated).
        // Clear any superseded staged items on the main thread (TR.1 #5).
        main_thread.staged_restore = None;
        if let Some(staged) = main_thread.staged_swap.as_mut() {
            staged.clear_model();
            staged.clear_ir();
            main_thread.staged_swap = None;
        }

        let pending = PendingRestore {
            generation: txn.generation,
            txn: Some(txn),
            publish,
            seq: 0,
        };
        deliver_pending_restore(main_thread, pending);
    } else {
        // Different physical latency ⇒ strict Política A: stage entire package off-RT
        // and request host restart. Audio thread keeps running current state and reporting
        // current latency until activate() consumes this staged restore.
        if os_differs {
            PendingRestartOs::Pending(publish.params.oversample).store(
                &main_thread.shared.cold.pending_restart_os_factor,
                Ordering::Release,
            );
        }
        main_thread.staged_swap = None;
        main_thread.staged_restore = Some(StagedRestore { txn, publish });
        main_thread.host.request_restart();
    }

    Ok(())
}

/// Maps the validated resources onto a [`RestorePublish`] (publication payload,
/// applied only on ack) and the atomic [`RestoreTxn`] (audio-thread package).
fn build_restore_package(
    validated: ValidatedRestore,
    current_params: &ProcessingParams,
    host_rate: u32,
    buffer_size: u32,
    mode: &RestoreMode,
    cold: &crate::clap::plugin::shared::ColdShared,
) -> Result<(RestorePublish, RestoreTxn), PluginError> {
    let ValidatedRestore {
        params: validated_params,
        model,
        model_path_on_disk,
        model_basename,
        model_search_path_to_add,
        model_hash,
        ir,
        ir_path_on_disk,
        ir_hash,
    } = validated;

    // Effective params: Full replaces everything; ForPreset only the
    // preset-identity subset (oversample + activation_precision included).
    let effective_params = match mode {
        RestoreMode::Full => validated_params.clone(),
        RestoreMode::ForPreset => {
            let mut p = current_params.clone();
            p.input_gain_db = validated_params.input_gain_db;
            p.output_gain_db = validated_params.output_gain_db;
            p.gate_threshold_db = validated_params.gate_threshold_db;
            p.bypass = validated_params.bypass;
            p.adaptive_compute = validated_params.adaptive_compute;
            p.slim_override = validated_params.slim_override;
            p.oversample = validated_params.oversample;
            p.activation_precision = validated_params.activation_precision;
            p
        }
    };

    // Publication metadata is cloned before the heavy resources are moved into
    // the transaction payload.
    let model_publish = model.as_ref().map(|r| RestoreModelPublish {
        metadata: r.model_metadata.clone(),
        info: r.model_info.clone(),
        full_wavenet: r.model_l.as_ref().and_then(|m| {
            if let StaticModel::WavenetDyn(w) = m.as_ref() {
                clone_wavenet_for_slimmable_storage(w).ok()
            } else {
                None
            }
        }),
        model_rate: r.model_rate,
    });

    let model_component = match model {
        Some(resources) => {
            let ModelResources {
                model_l,
                new_resampler,
                new_stream,
                input_mult_adj,
                output_mult_adj,
                model_rate: _,
                model_metadata: _,
                model_info: _,
                model_hash: _,
            } = resources;
            Some(LoadModelPayload {
                generation: cold.allocate_model_generation(),
                model_l,
                new_resampler,
                new_stream,
                input_mult_adj,
                output_mult_adj,
            })
        }
        None => match mode {
            RestoreMode::Full => {
                // Explicitly clear the model on the RT thread (T6.1 defect #5).
                // Building the passthrough resampler is part of the transaction:
                // a failure aborts the entire commit — nothing is published.
                let new_resampler = NamResampler::new(host_rate, 48000, 0).map_err(|e| {
                    PluginError::Message(Box::leak(
                        format!("Failed to build clear-model resampler: {e:?}").into_boxed_str(),
                    ))
                })?;
                let new_stream = crate::clap::plugin::build_stream_adapter(
                    host_rate,
                    48000,
                    buffer_size.max(1) as usize,
                )
                .map_err(|e| {
                    PluginError::Message(Box::leak(
                        format!("Failed to build clear-model streaming buffer: {e:?}")
                            .into_boxed_str(),
                    ))
                })?;
                Some(LoadModelPayload {
                    generation: cold.allocate_model_generation(),
                    model_l: None,
                    new_resampler: Box::new(new_resampler),
                    new_stream,
                    input_mult_adj: 1.0,
                    output_mult_adj: 1.0,
                })
            }
            RestoreMode::ForPreset => None,
        },
    };

    let ir_publish = ir.as_ref().map(|r| (r.samples.clone(), r.sample_rate));
    let ir_component = match ir {
        Some(resources) => {
            let IrResources {
                adapter,
                samples: _,
                sample_rate: _,
            } = resources;
            // `adapter` is already `Option<Box<CabSimAdapter>>` — None clears
            // the IR. The box preserves the RT-safe end-to-end ownership
            // contract (F-RT-003/T2.1).
            Some(adapter)
        }
        None => match mode {
            RestoreMode::Full => Some(None),
            RestoreMode::ForPreset => None,
        },
    };

    let publish = RestorePublish {
        mode_full: matches!(mode, RestoreMode::Full),
        params: effective_params.clone(),
        model: model_publish,
        model_path_on_disk,
        model_basename,
        model_hash,
        model_search_path_to_add,
        ir_path_on_disk,
        ir_raw_samples: ir_publish.as_ref().map(|(s, _)| s.clone()),
        ir_raw_sample_rate: ir_publish.as_ref().map(|(_, r)| *r).unwrap_or(0),
        ir_hash,
    };

    let txn = RestoreTxn {
        generation: next_restore_generation(),
        model: model_component,
        ir: ir_component,
        params: RtProcessingParams::from_processing_params(&effective_params),
    };

    Ok((publish, txn))
}

/// Attempts the first `try_push_command` of the transaction. On `Full` the whole
/// package (txn + publish payload) is retained in `pending_restore` for retry.
fn deliver_pending_restore(main_thread: &mut NamClapMainThread, mut pending: PendingRestore) {
    if let Some(txn) = pending.txn.take() {
        match main_thread
            .cmd_producer
            .try_push_command(ClapParamPayload::RestoreTxn(txn))
        {
            Ok(seq) => {
                pending.txn = None;
                pending.seq = seq;
                note_model_delivered(main_thread, &pending.publish);
            }
            Err((PushError::Full, payload)) => {
                if let ClapParamPayload::RestoreTxn(txn) = payload {
                    pending.txn = Some(txn);
                }
                main_thread.pending_restore = Some(pending);
                main_thread.host.request_callback();
                return;
            }
        }
    }
    // Pushed (or already pushed earlier): retain until the audio thread acks.
    main_thread.pending_restore = Some(pending);
    main_thread.host.request_callback();
}

/// Advances the telemetry load counter once a restore carrying a model has been
/// delivered to the audio thread. Kept at delivery (push) time — like the
/// pre-T6.1 commit — so synchronous callers observe the counter advance without
/// waiting for the ack; UI/path/hash publication still waits for the ack.
fn note_model_delivered(main_thread: &mut NamClapMainThread, publish: &RestorePublish) {
    if publish.model.is_some() {
        main_thread
            .shared
            .cold
            .model_load_counter
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// Publishes UI/paths/hashes for a restore transaction once the audio thread has
/// applied it (ack phase), or immediately in a pre-activate local commit.
pub(crate) fn publish_restore(publish: RestorePublish, main_thread: &mut NamClapMainThread) {
    let RestorePublish {
        mode_full,
        params,
        model,
        model_path_on_disk,
        model_basename,
        model_hash,
        model_search_path_to_add,
        ir_path_on_disk,
        ir_raw_samples,
        ir_raw_sample_rate,
        ir_hash,
    } = publish;
    let mode = if mode_full {
        RestoreMode::Full
    } else {
        RestoreMode::ForPreset
    };

    // ── Model publication (only after the audio thread applied the package) ──
    if let Some(model) = model {
        if let Ok(mut storage) = main_thread.shared.cold.full_wavenet_model.lock() {
            *storage = model.full_wavenet;
        }
        if let Ok(mut meta_guard) = main_thread.shared.cold.ui_model_metadata.lock() {
            *meta_guard = Some(model.metadata);
        }
        if let Ok(mut info_guard) = main_thread.shared.cold.ui_model_info.lock() {
            *info_guard = Some(model.info);
        }
        main_thread
            .shared
            .cold
            .model_sample_rate
            .store(model.model_rate, Ordering::Relaxed);
        if let Some(ref basename) = model_basename
            && let Ok(mut name_guard) = main_thread.shared.cold.ui_model_name.lock()
        {
            *name_guard = basename.clone();
        }
        log::info!(
            "Model restored (ack): {:?}",
            model_path_on_disk.as_deref().unwrap_or(Path::new(""))
        );
    } else if mode_full {
        // Explicitly clear model UI/ColdShared.
        if let Ok(mut storage) = main_thread.shared.cold.full_wavenet_model.lock() {
            *storage = None;
        }
        if let Ok(mut meta_guard) = main_thread.shared.cold.ui_model_metadata.lock() {
            *meta_guard = None;
        }
        if let Ok(mut info_guard) = main_thread.shared.cold.ui_model_info.lock() {
            *info_guard = None;
        }
        if let Ok(mut name_guard) = main_thread.shared.cold.ui_model_name.lock() {
            name_guard.clear();
        }
        main_thread
            .shared
            .cold
            .model_sample_rate
            .store(48000, Ordering::Relaxed);
    }

    if let Some(mut state_ext) = main_thread
        .host
        .get_extension::<clack_extensions::state::HostState>()
    {
        state_ext.mark_dirty(&main_thread.host);
    }

    // ── IR publication (only after the audio thread applied the package) ──
    if let Some(ref ir_path_str) = ir_path_on_disk {
        if let Ok(mut ir_guard) = main_thread.shared.cold.ir_path.lock() {
            *ir_guard = Some(ir_path_str.clone());
        }
        if let Ok(mut hash_guard) = main_thread.shared.cold.ir_hash.lock() {
            *hash_guard = ir_hash.clone();
        }
        if let Ok(mut raw_guard) = main_thread.shared.cold.ir_raw_samples.lock() {
            *raw_guard = ir_raw_samples;
        }
        main_thread
            .shared
            .cold
            .ir_raw_sample_rate
            .store(ir_raw_sample_rate, Ordering::Relaxed);
        main_thread.params.ir_path = Some(PathBuf::from(ir_path_str.clone()));
        main_thread.params.ir_hash = ir_hash;
    } else if mode_full {
        if let Ok(mut ir_guard) = main_thread.shared.cold.ir_path.lock() {
            *ir_guard = None;
        }
        if let Ok(mut hash_guard) = main_thread.shared.cold.ir_hash.lock() {
            *hash_guard = None;
        }
        if let Ok(mut raw_guard) = main_thread.shared.cold.ir_raw_samples.lock() {
            *raw_guard = None;
        }
        main_thread
            .shared
            .cold
            .ir_raw_sample_rate
            .store(0, Ordering::Relaxed);
        main_thread.params.ir_path = None;
        main_thread.params.ir_hash = None;
    }

    // ── Params publication ──
    match mode {
        RestoreMode::Full => {
            main_thread.params = params;
        }
        RestoreMode::ForPreset => {
            main_thread.params.input_gain_db = params.input_gain_db;
            main_thread.params.output_gain_db = params.output_gain_db;
            main_thread.params.gate_threshold_db = params.gate_threshold_db;
            main_thread.params.bypass = params.bypass;
            main_thread.params.adaptive_compute = params.adaptive_compute;
            main_thread.params.slim_override = params.slim_override;
            main_thread.params.oversample = params.oversample;
            main_thread.params.activation_precision = params.activation_precision;
        }
    }

    if mode_full {
        main_thread.params.model_path = model_path_on_disk;
        main_thread.params.model_basename = model_basename;
        main_thread.params.model_hash = model_hash;
        if let Some(search_path) = model_search_path_to_add
            && !main_thread.params.model_search_paths.contains(&search_path)
        {
            main_thread.params.model_search_paths.push(search_path);
        }
    }

    // ── Publish params to RT atomics ──
    use crate::clap::extensions::params::bypass_bool_to_u32;
    main_thread.shared.ui_to_rt.param_input_gain.store(
        main_thread.params.input_gain_db.to_bits(),
        Ordering::Relaxed,
    );
    main_thread.shared.ui_to_rt.param_output_gain.store(
        main_thread.params.output_gain_db.to_bits(),
        Ordering::Relaxed,
    );
    main_thread.shared.ui_to_rt.param_gate_thresh.store(
        main_thread.params.gate_threshold_db.to_bits(),
        Ordering::Relaxed,
    );
    main_thread.shared.ui_to_rt.param_bypass.store(
        bypass_bool_to_u32(main_thread.params.bypass),
        Ordering::Relaxed,
    );
    main_thread.shared.ui_to_rt.param_adaptive_compute.store(
        main_thread.params.adaptive_compute as u32,
        Ordering::Relaxed,
    );
    main_thread
        .shared
        .ui_to_rt
        .param_slim_override
        .store(main_thread.params.slim_override as u32, Ordering::Relaxed);
    main_thread.shared.ui_to_rt.param_oversample.store(
        main_thread.params.oversample.to_f32() as u32,
        Ordering::Relaxed,
    );
    main_thread.shared.ui_to_rt.param_activation.store(
        main_thread.params.activation_precision as u32,
        Ordering::Relaxed,
    );
    main_thread.shared.bump_generation();

    if let Some(params_ext) = main_thread
        .host
        .get_extension::<clack_extensions::params::HostParams>()
    {
        params_ext.rescan(
            &mut main_thread.host,
            clack_extensions::params::ParamRescanFlags::VALUES,
        );
    }
}

/// Pre-activate path (`buffer_size == 0`): there is no audio thread, so the
/// restore commits locally and atomically on the main thread. The model is
/// retained in `pending_model` for `flush_pending_model()` on `activate()`.
fn local_commit(
    validated: ValidatedRestore,
    main_thread: &mut NamClapMainThread,
    mode: &RestoreMode,
) -> Result<(), PluginError> {
    let ValidatedRestore {
        params: validated_params,
        model,
        model_path_on_disk,
        model_basename,
        model_search_path_to_add,
        model_hash,
        ir,
        ir_path_on_disk,
        ir_hash,
    } = validated;

    // ── Commit model (local) ──
    if let Some(resources) = model {
        let ModelResources {
            model_l,
            new_resampler: _,
            new_stream: _,
            input_mult_adj,
            output_mult_adj,
            model_rate,
            model_metadata,
            model_info,
            model_hash: _,
        } = resources;

        // Store full WaveNet weights for slimmable rebuild.
        {
            let mut storage = main_thread
                .shared
                .cold
                .full_wavenet_model
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *storage = model_l.as_ref().and_then(|m| {
                if let StaticModel::WavenetDyn(w) = m.as_ref() {
                    clone_wavenet_for_slimmable_storage(w).ok()
                } else {
                    None
                }
            });
        }

        if let Ok(mut meta_guard) = main_thread.shared.cold.ui_model_metadata.lock() {
            *meta_guard = Some(model_metadata);
        }
        if let Ok(mut info_guard) = main_thread.shared.cold.ui_model_info.lock() {
            *info_guard = Some(model_info);
        }

        if let Ok(mut pending_guard) = main_thread.shared.cold.pending_model.lock() {
            *pending_guard = Some(PendingModel {
                generation: main_thread.shared.cold.allocate_model_generation(),
                model: model_l,
                model_rate,
                input_mult_adj,
                output_mult_adj,
            });
        }
        main_thread
            .shared
            .cold
            .model_load_counter
            .fetch_add(1, Ordering::Relaxed);
        main_thread
            .shared
            .cold
            .model_sample_rate
            .store(model_rate, Ordering::Relaxed);

        if let Some(ref basename) = model_basename {
            if let Ok(mut name_guard) = main_thread.shared.cold.ui_model_name.lock() {
                *name_guard = basename.clone();
            }
            log::info!(
                "Model restored (local): {:?}",
                model_path_on_disk.as_deref().unwrap_or(Path::new(""))
            );
        }

        if let Some(mut state_ext) = main_thread
            .host
            .get_extension::<clack_extensions::state::HostState>()
        {
            state_ext.mark_dirty(&main_thread.host);
        }
    } else if let RestoreMode::Full = mode {
        // Explicitly clear the model. There is no RT thread to push to yet, so
        // the clear intent is retained in `pending_model` (model_l = None) and
        // `flush_pending_model()` on `activate()` pushes `LoadModel { None }`,
        // ensuring the previous model does not survive a reactivation.
        if let Ok(mut pending_guard) = main_thread.shared.cold.pending_model.lock() {
            *pending_guard = Some(PendingModel {
                generation: main_thread.shared.cold.allocate_model_generation(),
                model: None,
                model_rate: 48000,
                input_mult_adj: 1.0,
                output_mult_adj: 1.0,
            });
        }
        if let Ok(mut storage) = main_thread.shared.cold.full_wavenet_model.lock() {
            *storage = None;
        }
        if let Ok(mut meta_guard) = main_thread.shared.cold.ui_model_metadata.lock() {
            *meta_guard = None;
        }
        if let Ok(mut info_guard) = main_thread.shared.cold.ui_model_info.lock() {
            *info_guard = None;
        }
        if let Ok(mut name_guard) = main_thread.shared.cold.ui_model_name.lock() {
            name_guard.clear();
        }
        main_thread
            .shared
            .cold
            .model_sample_rate
            .store(48000, Ordering::Relaxed);
    }

    // ── Commit IR (local) ──
    if let Some(ir) = ir {
        if let Some(ref ir_path_str) = ir_path_on_disk
            && let Ok(mut ir_guard) = main_thread.shared.cold.ir_path.lock()
        {
            *ir_guard = Some(ir_path_str.clone());
        }
        if let Ok(mut hash_guard) = main_thread.shared.cold.ir_hash.lock() {
            *hash_guard = ir_hash.clone();
        }
        if let Ok(mut raw_guard) = main_thread.shared.cold.ir_raw_samples.lock() {
            *raw_guard = Some(ir.samples);
        }
        main_thread
            .shared
            .cold
            .ir_raw_sample_rate
            .store(ir.sample_rate, Ordering::Relaxed);
        if let Some(ref ir_path_str) = ir_path_on_disk {
            main_thread.params.ir_path = Some(PathBuf::from(ir_path_str.clone()));
        }
        main_thread.params.ir_hash = ir_hash;
    } else if let RestoreMode::Full = mode {
        if let Ok(mut ir_guard) = main_thread.shared.cold.ir_path.lock() {
            *ir_guard = None;
        }
        if let Ok(mut hash_guard) = main_thread.shared.cold.ir_hash.lock() {
            *hash_guard = None;
        }
        if let Ok(mut raw_guard) = main_thread.shared.cold.ir_raw_samples.lock() {
            *raw_guard = None;
        }
        main_thread
            .shared
            .cold
            .ir_raw_sample_rate
            .store(0, Ordering::Relaxed);
        main_thread.params.ir_path = None;
        main_thread.params.ir_hash = None;
    }

    // ── Commit params (local) ──
    match mode {
        RestoreMode::Full => {
            main_thread.params = validated_params;
        }
        RestoreMode::ForPreset => {
            main_thread.params.input_gain_db = validated_params.input_gain_db;
            main_thread.params.output_gain_db = validated_params.output_gain_db;
            main_thread.params.gate_threshold_db = validated_params.gate_threshold_db;
            main_thread.params.bypass = validated_params.bypass;
            main_thread.params.adaptive_compute = validated_params.adaptive_compute;
            main_thread.params.slim_override = validated_params.slim_override;
            // Oversample and activation_precision are part of the preset identity
            main_thread.params.oversample = validated_params.oversample;
            main_thread.params.activation_precision = validated_params.activation_precision;
        }
    }

    if let RestoreMode::Full = mode {
        main_thread.params.model_path = model_path_on_disk;
        main_thread.params.model_basename = model_basename;
        main_thread.params.model_hash = model_hash;
        if let Some(search_path) = model_search_path_to_add
            && !main_thread.params.model_search_paths.contains(&search_path)
        {
            main_thread.params.model_search_paths.push(search_path);
        }
    }

    // ── Publish params to RT atomics (no SPSC push: no audio thread yet) ──
    use crate::clap::extensions::params::bypass_bool_to_u32;
    main_thread.shared.ui_to_rt.param_input_gain.store(
        main_thread.params.input_gain_db.to_bits(),
        Ordering::Relaxed,
    );
    main_thread.shared.ui_to_rt.param_output_gain.store(
        main_thread.params.output_gain_db.to_bits(),
        Ordering::Relaxed,
    );
    main_thread.shared.ui_to_rt.param_gate_thresh.store(
        main_thread.params.gate_threshold_db.to_bits(),
        Ordering::Relaxed,
    );
    main_thread.shared.ui_to_rt.param_bypass.store(
        bypass_bool_to_u32(main_thread.params.bypass),
        Ordering::Relaxed,
    );
    main_thread.shared.ui_to_rt.param_adaptive_compute.store(
        main_thread.params.adaptive_compute as u32,
        Ordering::Relaxed,
    );
    main_thread
        .shared
        .ui_to_rt
        .param_slim_override
        .store(main_thread.params.slim_override as u32, Ordering::Relaxed);
    main_thread.shared.ui_to_rt.param_oversample.store(
        main_thread.params.oversample.to_f32() as u32,
        Ordering::Relaxed,
    );
    main_thread.shared.ui_to_rt.param_activation.store(
        main_thread.params.activation_precision as u32,
        Ordering::Relaxed,
    );
    main_thread.shared.bump_generation();

    if let Some(params_ext) = main_thread
        .host
        .get_extension::<clack_extensions::params::HostParams>()
    {
        params_ext.rescan(
            &mut main_thread.host,
            clack_extensions::params::ParamRescanFlags::VALUES,
        );
    }

    Ok(())
}

impl<'a> NamClapMainThread<'a> {
    /// Retries delivery of a pending restore transaction and, once the audio
    /// thread acks it, publishes the UI/paths/hashes (T6.1 ack phase).
    ///
    /// Called from `housekeeping()`. Latest-wins: a newer restore replaces an
    /// older still-pending one; the older transaction already in the ring still
    /// applies atomically (a complete package), but its UI publication is
    /// superseded.
    pub(crate) fn flush_pending_restore(&mut self) {
        let Some(mut pending) = self.pending_restore.take() else {
            return;
        };

        if let Some(txn) = pending.txn.take() {
            match self
                .cmd_producer
                .try_push_command(ClapParamPayload::RestoreTxn(txn))
            {
                Ok(seq) => {
                    pending.txn = None;
                    pending.seq = seq;
                    note_model_delivered(self, &pending.publish);
                }
                Err((PushError::Full, payload)) => {
                    if let ClapParamPayload::RestoreTxn(txn) = payload {
                        pending.txn = Some(txn);
                    }
                    self.pending_restore = Some(pending);
                    self.host.request_callback();
                    return;
                }
            }
        }

        if pending.seq > 0 && self.cmd_producer.is_acked(pending.seq) {
            publish_restore(pending.publish, self);
        } else {
            self.pending_restore = Some(pending);
            self.host.request_callback();
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
#[path = "state_transaction_test.rs"]
mod tests;
