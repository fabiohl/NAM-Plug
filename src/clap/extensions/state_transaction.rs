// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Centralised transactional pipeline for state and state-context restoration (S6-E6-T01).
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
use crate::clap::plugin::NamClapMainThread;
use crate::clap::plugin::NamModelMetadata;
use crate::clap::plugin::PendingModel;
use crate::clap::plugin::command_scheduler::PushError;
use crate::clap::plugin::debug_assert_main_thread;
use clack_plugin::prelude::*;
use neural_amp_modeler_rs::common::diagnostics::{ModelInfo, NamDiagnostic, NamErrorCode};
use neural_amp_modeler_rs::common::params::ProcessingParams;
use neural_amp_modeler_rs::common::params::RtProcessingParams;
use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter;
use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;
use neural_amp_modeler_rs::dsp::cabsim::loader::CabSimIr;
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
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
    input_mult_adj: f32,
    output_mult_adj: f32,
    model_rate: u32,
    model_metadata: NamModelMetadata,
    model_info: ModelInfo,
    model_hash: String,
}

struct IrResources {
    adapter: Option<CabSimAdapter>,
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
}

/// Restore mode: `Full` for project/duplicate, `ForPreset` for preset bank loads.
pub(crate) enum RestoreMode {
    Full,
    ForPreset,
}

/// Entry-point for the 3-phase transactional state restore (S6-E6-T01).
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
    commit(validated, main_thread, &mode);

    Ok(())
}

type ModelValidationResult = (
    Option<ModelResources>,
    Option<PathBuf>,
    Option<String>,
    Option<PathBuf>,
    Option<String>,
);

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

    let (maybe_ir, ir_path_on_disk) = validate_ir(loaded_params, host_rate, buffer_size, sys)?;

    Ok(ValidatedRestore {
        params,
        model: maybe_model.0,
        model_path_on_disk: maybe_model.1,
        model_basename: maybe_model.2,
        model_search_path_to_add: maybe_model.3,
        model_hash: maybe_model.4,
        ir: maybe_ir,
        ir_path_on_disk,
    })
}

/// Computes the SHA-256 hex digest of a file's raw bytes (S6-E6-T02).
fn compute_file_hash(path: &Path) -> Result<String, PluginError> {
    let bytes = std::fs::read(path).map_err(|e| {
        PluginError::Message(Box::leak(
            format!("Failed to read file for hashing ({path:?}): {e}").into_boxed_str(),
        ))
    })?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let hash = hasher.finalize();
    Ok(hash.iter().map(|b| format!("{b:02x}")).collect())
}

/// Returns canonical search directories for portable model/IR lookup (S6-E6-T02).
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
        input_mult_adj,
        output_mult_adj,
        model_rate,
        model_metadata,
        model_info,
        model_hash,
    })
}

fn validate_model_full(
    loaded_params: &ProcessingParams,
    host_rate: u32,
    buffer_size: u32,
    sys: &neural_amp_modeler_rs::common::diagnostics::SystemSnapshot,
) -> Result<ModelValidationResult, PluginError> {
    let Some(ref path) = loaded_params.model_path else {
        // S6-E6-T03: ForPreset blobs carry model_path=None but model_basename + model_hash.
        // Fall through to portable basename search so state.load() restores presets equivalently
        // to state_context.load(ForPreset).
        return validate_model_from_basename(loaded_params, host_rate, buffer_size, sys);
    };

    if path.exists() {
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

    if let Some(ref basename) = loaded_params.model_basename {
        let found = loaded_params
            .model_search_paths
            .clone()
            .into_iter()
            .find_map(|dir| {
                let candidate = dir.join(basename);
                if candidate.exists() {
                    Some(candidate)
                } else {
                    None
                }
            });
        if let Some(new_path) = found {
            log::info!(
                "Model not found at original path ({path:?}), using portable fallback: {new_path:?}"
            );
            let resources = build_model_resources(&new_path, host_rate, buffer_size, sys)
                .map_err(|e| PluginError::Error(e))?;
            let basename = new_path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string());
            let search_path = new_path.parent().map(|p| p.to_path_buf());
            let hash = Some(resources.model_hash.clone());
            return Ok((Some(resources), Some(new_path), basename, search_path, hash));
        }
    }

    log::error!(
        "NAM-rs: State restore failed — model not found at {path:?}{}",
        loaded_params
            .model_basename
            .as_ref()
            .map(|b| format!(" (basename: {b})"))
            .unwrap_or_default()
    );
    Err(PluginError::Message(Box::leak(
        format!("Saved model not found: {:?}", path).into_boxed_str(),
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

/// Shared basename-based portable model resolution with canonical search and
/// content-hash verification (S6-E6-T02, S6-E6-T03).  Used by both
/// `validate_model_preset()` and `validate_model_full()` when `model_path` is None.
fn validate_model_from_basename(
    loaded_params: &ProcessingParams,
    host_rate: u32,
    buffer_size: u32,
    sys: &neural_amp_modeler_rs::common::diagnostics::SystemSnapshot,
) -> Result<ModelValidationResult, PluginError> {
    let Some(ref basename) = loaded_params.model_basename else {
        return Ok((None, None, None, None, None));
    };

    // S6-E6-T02: search chain — loaded search paths first, then canonical dirs.
    // Hash verification ensures content identity when multiple files share a basename.
    let search_dirs: Vec<PathBuf> = loaded_params
        .model_search_paths
        .iter()
        .cloned()
        .chain(canonical_search_dirs())
        .collect();

    let expected_hash = loaded_params.model_hash.as_deref();

    for dir in &search_dirs {
        let candidate = dir.join(basename);
        if !candidate.exists() {
            continue;
        }
        if let Some(ref expected) = expected_hash
            && let Ok(actual) = compute_file_hash(&candidate)
            && actual != *expected
        {
            log::debug!(
                "NAM-rs: Skipping {candidate:?} — hash mismatch (expected {expected}, got {actual})"
            );
            continue;
        }
        let resources = build_model_resources(&candidate, host_rate, buffer_size, sys)
            .map_err(|e| PluginError::Error(e))?;
        let basename_from_path = candidate
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string());
        let search_path = candidate.parent().map(|p| p.to_path_buf());
        let hash = Some(resources.model_hash.clone());
        log::info!("NAM-rs: Resolved model via basename search: {candidate:?}");
        return Ok((
            Some(resources),
            Some(candidate),
            basename_from_path,
            search_path,
            hash,
        ));
    }

    let searched = search_dirs
        .iter()
        .map(|d| format!("{d:?}/"))
        .collect::<Vec<_>>()
        .join(", ");
    log::error!(
        "NAM-rs: ForPreset restore failed — model basename {basename:?} not found in: [{searched}]"
    );
    Err(PluginError::Message(Box::leak(
        format!("Preset model not found: {basename} (searched canonical dirs)").into_boxed_str(),
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
        adapter: Some(CabSimAdapter::new(Box::new(engine)).map_err(|e| {
            Box::new(
                NamDiagnostic::new(e, sys)
                    .message("Failed to build cab-sim convolution adapter")
                    .hint("The IR samples require more memory than available."),
            )
        })?),
        samples: cabsim.samples,
        sample_rate: cabsim.sample_rate,
    })
}

fn validate_ir(
    loaded_params: &ProcessingParams,
    host_rate: u32,
    buffer_size: u32,
    sys: &neural_amp_modeler_rs::common::diagnostics::SystemSnapshot,
) -> Result<(Option<IrResources>, Option<String>), PluginError> {
    let Some(ref ir_path) = loaded_params.ir_path else {
        return Ok((None, None));
    };

    if !ir_path.exists() {
        log::error!("NAM-rs: State restore failed — IR not found at {ir_path:?}");
        return Err(PluginError::Message(Box::leak(
            format!("Saved IR not found: {:?}", ir_path).into_boxed_str(),
        )));
    }

    let resources = build_ir_resources(ir_path, host_rate, buffer_size, sys)
        .map_err(|e| PluginError::Error(e))?;
    Ok((Some(resources), Some(ir_path.to_string_lossy().to_string())))
}

/// Publishes the validated restore atomically.  Only reached after all validation passes.
fn commit(validated: ValidatedRestore, main_thread: &mut NamClapMainThread, mode: &RestoreMode) {
    let buffer_size = main_thread.shared.cold.buffer_size.load(Ordering::Relaxed);

    // ── Commit model ──
    if let Some(resources) = validated.model {
        let ModelResources {
            model_l,
            new_resampler,
            input_mult_adj,
            output_mult_adj,
            model_rate,
            model_metadata,
            model_info,
            model_hash: _,
        } = resources;

        // Store full WaveNet weights for slimmable rebuild
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

        // Deliver the model to the audio thread, retaining it on saturation
        // instead of dropping it (R-10). The UI is only advanced once the
        // model is delivered (or deferred pre-activation, where no audio path
        // is active to desync from).
        let delivered = if buffer_size > 0 {
            match main_thread
                .cmd_producer
                .try_push_command(ClapParamPayload::LoadModel {
                    model_l,
                    new_resampler,
                    input_mult_adj,
                    output_mult_adj,
                }) {
                Ok(_seq) => true,
                Err((PushError::Full, payload)) => {
                    if let ClapParamPayload::LoadModel {
                        model_l,
                        new_resampler,
                        input_mult_adj,
                        output_mult_adj,
                    } = payload
                    {
                        drop(new_resampler); // rebuilt by flush_pending_model()
                        if let Ok(mut pending_guard) = main_thread.shared.cold.pending_model.lock()
                        {
                            *pending_guard = Some(PendingModel {
                                model: model_l,
                                model_rate,
                                input_mult_adj,
                                output_mult_adj,
                            });
                        }
                    }
                    main_thread.host.request_callback();
                    false
                }
            }
        } else {
            if let Ok(mut pending_guard) = main_thread.shared.cold.pending_model.lock() {
                *pending_guard = Some(PendingModel {
                    model: model_l,
                    model_rate,
                    input_mult_adj,
                    output_mult_adj,
                });
            }
            true
        };

        if delivered {
            main_thread
                .shared
                .cold
                .model_load_counter
                .fetch_add(1, Ordering::Relaxed);

            if let Some(ref basename) = validated.model_basename {
                if let Ok(mut name_guard) = main_thread.shared.cold.ui_model_name.lock() {
                    *name_guard = basename.clone();
                }
                log::info!(
                    "Model restored: {:?}",
                    validated
                        .model_path_on_disk
                        .as_deref()
                        .unwrap_or(Path::new(""))
                );
            }
        }

        if let Some(mut state_ext) = main_thread
            .host
            .get_extension::<clack_extensions::state::HostState>()
        {
            state_ext.mark_dirty(&main_thread.host);
        }
    }

    // ── Commit IR ──
    if let Some(ir) = validated.ir {
        if let Some(ref ir_path_str) = validated.ir_path_on_disk
            && let Ok(mut ir_guard) = main_thread.shared.cold.ir_path.lock()
        {
            *ir_guard = Some(ir_path_str.clone());
        }
        if let Ok(mut raw_guard) = main_thread.shared.cold.ir_raw_samples.lock() {
            *raw_guard = Some(ir.samples);
        }
        main_thread
            .shared
            .cold
            .ir_raw_sample_rate
            .store(ir.sample_rate, Ordering::Relaxed);

        // When already active, deliver the adapter immediately. When not yet
        // active (buffer_size == 0), skip the push: `activate()` rebuilds the
        // adapter from `ir_raw_samples` with the correct host partition size.
        // Pushing the pre-activation adapter (built with a 256-sample fallback
        // partition) would overwrite the correctly-sized activate-built one.
        if buffer_size > 0 {
            match main_thread
                .cmd_producer
                .try_push_command(ClapParamPayload::LoadCabIr {
                    adapter: ir.adapter,
                }) {
                Ok(_) => {}
                Err((_, payload)) => {
                    // R-10: retain the restored adapter for retry via
                    // ui_pending_ir / housekeeping. ir_path and
                    // ir_raw_samples are already stored so activate()
                    // can rebuild if the plugin is not yet processing.
                    log::warn!("NAM-rs: restored IR channel full — will retry on next callback");
                    if let Some(ref ir_path_str) = validated.ir_path_on_disk
                        && let Ok(mut pending) = main_thread.shared.cold.ui_pending_ir.lock()
                    {
                        *pending = Some(PathBuf::from(ir_path_str));
                    }
                    let _ = payload;
                    main_thread.host.request_callback();
                }
            }
        }
    } else if buffer_size > 0
        && main_thread
            .cmd_producer
            .try_push_command(ClapParamPayload::LoadCabIr { adapter: None })
            .is_err()
    {
        main_thread
            .shared
            .cold
            .ui_clear_ir
            .store(true, Ordering::Relaxed);
        main_thread.host.request_callback();
    }

    // ── Commit params ──
    match mode {
        RestoreMode::Full => {
            main_thread.params = validated.params;
        }
        RestoreMode::ForPreset => {
            main_thread.params.input_gain_db = validated.params.input_gain_db;
            main_thread.params.output_gain_db = validated.params.output_gain_db;
            main_thread.params.gate_threshold_db = validated.params.gate_threshold_db;
            main_thread.params.bypass = validated.params.bypass;
            main_thread.params.adaptive_compute = validated.params.adaptive_compute;
            main_thread.params.slim_override = validated.params.slim_override;
            // S6-E6-T03: oversample and activation_precision are part of the preset identity
            main_thread.params.oversample = validated.params.oversample;
            main_thread.params.activation_precision = validated.params.activation_precision;
        }
    }

    if let RestoreMode::Full = mode {
        main_thread.params.model_path = validated.model_path_on_disk;
        main_thread.params.model_basename = validated.model_basename;
        main_thread.params.model_hash = validated.model_hash;
        if let Some(search_path) = validated.model_search_path_to_add
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

    main_thread
        .cmd_producer
        .push_params(RtProcessingParams::from_processing_params(
            &main_thread.params,
        ));
    let _ = main_thread.cmd_producer.force_flush();

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

// ═══════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
#[path = "state_transaction_test.rs"]
mod tests;
