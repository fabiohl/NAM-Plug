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
use neural_amp_modeler_rs::loader::load_and_build_model;
use neural_amp_modeler_rs::models::slimmable::clone_wavenet_for_slimmable_storage;
use neural_amp_modeler_rs::models::{NamModel, StaticModel};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

#[cfg(test)]
type TestMidLoadHook = Box<dyn Fn(&Path) + Send + 'static>;

#[cfg(test)]
static TEST_MID_LOAD_HOOK: std::sync::Mutex<Option<TestMidLoadHook>> = std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn set_test_mid_load_hook<F: Fn(&Path) + Send + 'static>(f: Option<F>) {
    *TEST_MID_LOAD_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = f.map(|func| Box::new(func) as _);
}

#[cfg(test)]
fn run_test_mid_load_hook(path: &Path) {
    let mut guard = TEST_MID_LOAD_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(hook) = guard.take() {
        drop(guard);
        let hook_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            hook(path);
        }));
        *TEST_MID_LOAD_HOOK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(hook);
        if let Err(payload) = hook_res {
            std::panic::resume_unwind(payload);
        }
    }
}

/// Serialises tests that install the process-global `TEST_MID_LOAD_HOOK`, so a
/// test that stalls the loader cannot have its hook replaced by another test
/// running in parallel (the hook is a single global slot).
#[cfg(test)]
pub(crate) fn test_mid_load_hook_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

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
    /// Input gain calibration multiplier.
    input_mult_adj: f32,
    /// Output gain calibration multiplier.
    output_mult_adj: f32,
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

impl<'a> NamClapMainThread<'a> {
    /// Pure (side-effect-free) phase of a model load.
    ///
    /// Performs all file I/O, hashing, model construction, buffer sizing and
    /// cost calibration **without** touching `self.params`, any `RefCell`, the
    /// host handle or `shared.cold`: every shared value is an explicit
    /// parameter. Diagnostics are emitted against the provided `sys` snapshot.
    /// The returned [`PreparedModel`] is `Send`, so this half can be moved to a
    /// non-RT worker while the main thread stays responsive.
    pub(crate) fn prepare_model(
        path: &Path,
        host_rate: u32,
        buffer_size: usize,
        sys: &SystemSnapshot,
    ) -> Result<PreparedModel, Box<NamDiagnostic>> {
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

        #[cfg(test)]
        run_test_mid_load_hook(path);

        if model_pair.model_l.is_none() {
            return Err(Box::new(
                NamDiagnostic::new(NamErrorCode::ModelBuildFailed, sys)
                    .message(format!("Failed to build model: {:?}", path)),
            ));
        }

        let host_rate = if host_rate == 0 { 48000 } else { host_rate };
        let model_rate = model_pair.sample_rate;
        if host_rate != model_rate {
            log::warn!(
                "Model sample rate ({model_rate} Hz) differs from host sample rate ({host_rate} Hz); \
                 resampler will convert internally"
            );
        }

        // Compute content hash for portable asset identity. The GUI load is the
        // explicit user override path — but the asset is only adopted when its
        // digest can actually be computed, so persisted state always carries a
        // valid `model_hash`.
        let model_hash = crate::clap::extensions::state_transaction::compute_file_hash(path)
            .map_err(|e| {
                Box::new(
                    NamDiagnostic::new(NamErrorCode::ModelBuildFailed, sys)
                        .message(format!("Failed to compute model SHA-256 ({path:?}): {e}"))
                        .hint("Assets are adopted only with a verified SHA-256 digest."),
                )
            })?;

        // TOCTOU mitigation: verify the file was not modified while loading or hashing.
        let meta_after = std::fs::metadata(path).map_err(|e| {
            Box::new(
                NamDiagnostic::new(NamErrorCode::ModelBuildFailed, sys)
                    .message(format!(
                        "Failed to read metadata after hashing model: {:?}",
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

        let metadata = model_pair.metadata.clone();
        let ui_metadata = NamModelMetadata {
            architecture: model_pair.architecture.clone(),
            topology: model_pair.topology.clone(),
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

        let model_info = model_pair.model_info(path);
        let model_bytes = model_pair.model_memory_bytes().unwrap_or(0);

        let mut model_l = model_pair.model_l;
        let mut model_r = model_pair.model_r;
        let input_mult_adj = model_pair.input_mult_adj;
        let output_mult_adj = model_pair.output_mult_adj;

        // Derive full WaveNet weights for main-thread slimmable rebuild. This is
        // the same helper used in state_transaction.rs.
        let full_wavenet = model_l.as_ref().and_then(|m| {
            if let StaticModel::WavenetDyn(w) = m.as_ref() {
                clone_wavenet_for_slimmable_storage(w).ok()
            } else {
                None
            }
        });

        let mut new_resampler = None;
        let mut new_stream = None;
        let mut prewarm_cost_ns = None;

        if buffer_size > 0 {
            new_resampler = Some(Box::new(
                NamResampler::new_simple(host_rate, model_rate).map_err(|e| {
                    Box::new(
                        NamDiagnostic::new(NamErrorCode::ModelBuildFailed, sys)
                            .message("Failed to build polyphase resampler for model")
                            .param("error", e.to_string()),
                    )
                })?,
            ));

            if let Some(ref mut model) = model_l
                && let Err(e) = model.set_max_buffer_size(buffer_size)
            {
                return Err(Box::new(
                    NamDiagnostic::new(NamErrorCode::ModelBuildFailed, sys)
                        .message("Failed to set model max buffer size")
                        .param("error", e.to_string()),
                ));
            }
            if let Some(ref mut model) = model_r
                && let Err(e) = model.set_max_buffer_size(buffer_size)
            {
                return Err(Box::new(
                    NamDiagnostic::new(NamErrorCode::ModelBuildFailed, sys)
                        .message("Failed to set model max buffer size (R)")
                        .param("error", e.to_string()),
                ));
            }

            // Off-RT cost calibration (U-02 / F-PR-10): measure the freshly
            // loaded, stabilized model so the audio thread sizes its first
            // prewarm window from real hardware. `None` (no model) falls back
            // to the per-family table.
            prewarm_cost_ns = if crate::clap::processor::prewarm_policy::ADAPTIVE_COST_ENABLED {
                // Cap the calibration block at the negotiated host quantum so
                // it never exceeds the model's `max_buffer_size` (a smaller
                // block would otherwise be silently truncated by composed
                // models and under-measure the cost).
                let measure_frames =
                    crate::clap::processor::prewarm_policy::PREWARM_MEASURE_FRAMES.min(buffer_size);
                model_l
                    .as_mut()
                    .map(|model| model.measure_cost_ns_per_sample(measure_frames))
            } else {
                None
            };

            // Streaming resample adapter, sized for the worst-case host block —
            // built off-RT like the resampler.
            new_stream = Some(
                crate::clap::plugin::build_stream_adapter(host_rate, model_rate, buffer_size)
                    .map_err(|e| {
                        Box::new(
                            NamDiagnostic::new(NamErrorCode::ModelBuildFailed, sys)
                                .message("Failed to build streaming resample buffer")
                                .param("error", e.to_string()),
                        )
                    })?,
            );
        }

        Ok(PreparedModel {
            path: path.to_path_buf(),
            model_l,
            model_r,
            model_rate,
            input_mult_adj,
            output_mult_adj,
            model_hash,
            metadata: ui_metadata,
            model_info,
            model_bytes,
            full_wavenet,
            new_resampler,
            new_stream,
            prewarm_cost_ns,
            audio_epoch: AudioEpoch {
                sample_rate: host_rate,
                buffer_size,
            },
        })
    }

    /// Effectful phase of a model load: installs a [`PreparedModel`] produced by
    /// [`Self::prepare_model`] into the plugin state.
    ///
    /// Publishes params/`ui_*`, stores the slimmable WaveNet clone, allocates the
    /// model generation and delivers the resources to the audio thread (SPSC
    /// swap, staged swap or `pending_model` deferral), then rescans params and
    /// marks the state dirty. Mirrors the historical ordering of `load_model`
    /// one-to-one, so an explicit same-latency swap keeps its exact semantics.
    pub(crate) fn commit_prepared_model(&self, prepared: PreparedModel) {
        let generation = self.shared.cold.allocate_model_generation();
        self.commit_prepared_model_with_generation(prepared, generation);
    }

    /// Publishes the UI/host commit for a retained model: identity metadata,
    /// name, load counter, param rescan, stale-error clear and host dirty bit.
    ///
    /// Called in **every** outcome that retains the model for the audio thread
    /// (SPSC-ok, staged-restart, pending/`Full`, deferred): the UI may advance
    /// because the model is retained, never ahead of it (F-RB-02). All
    /// `RefCell` borrows are released before the host calls below.
    fn finish_commit_ui(&self, path: &Path, metadata: NamModelMetadata, model_info: ModelInfo) {
        *lock_cold(&self.shared.cold.ui_model_metadata, "ui_model_metadata") = Some(metadata);
        *lock_cold(&self.shared.cold.ui_model_info, "ui_model_info") = Some(model_info);

        self.shared
            .cold
            .model_load_counter
            .fetch_add(1, Ordering::Relaxed);

        let basename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        *lock_cold(&self.shared.cold.ui_model_name, "ui_model_name") = basename;

        if let Some(params_ext) = self
            .host
            .get_extension::<clack_extensions::params::HostParams>()
        {
            params_ext.rescan(
                &self.host,
                clack_extensions::params::ParamRescanFlags::VALUES
                    | clack_extensions::params::ParamRescanFlags::TEXT,
            );
        }

        log::info!("Model loaded: {path:?}");

        // A successful adoption clears any stale load error (F-UX-06): a later
        // success must never be shadowed by an earlier failure.
        self.shared
            .cold
            .ui_load_error
            .store(false, Ordering::Relaxed);
        lock_cold(&self.shared.cold.ui_load_error_msg, "ui_load_error_msg").clear();

        if let Some(state_ext) = self
            .host
            .get_extension::<clack_extensions::state::HostState>()
        {
            state_ext.mark_dirty(&self.host);
        }
    }

    /// Generation-explicit variant of [`Self::commit_prepared_model`].
    ///
    /// The asynchronous loader allocates the generation on the main thread
    /// *before* dispatching the job, so the same generation can be echoed by the
    /// worker and used to discard results superseded by a newer request.
    pub(crate) fn commit_prepared_model_with_generation(
        &self,
        prepared: PreparedModel,
        generation: u64,
    ) {
        let PreparedModel {
            path,
            mut model_l,
            mut model_r,
            model_rate,
            input_mult_adj,
            output_mult_adj,
            model_hash,
            metadata,
            model_info,
            model_bytes,
            full_wavenet,
            mut new_resampler,
            mut new_stream,
            mut prewarm_cost_ns,
            audio_epoch,
        } = prepared;

        // Identity (path/hash/search-path), DSP epoch and display metadata are
        // published only after the resources have been committed (or
        // retained) successfully. A failed revalidation leaves the previous
        // identity and DSP untouched and surfaces a visible error.
        let current_host_rate = self.shared.cold.sample_rate.load(Ordering::Relaxed);
        let current_buffer_size = self.shared.cold.buffer_size.load(Ordering::Relaxed) as usize;

        if current_buffer_size == 0 {
            // Plugin not yet activated — defer installation to activate() via pending_model.
            *lock_cold(&self.shared.cold.pending_model, "pending_model") = Some(PendingModel {
                generation,
                model: model_l,
                model_r,
                model_rate,
                input_mult_adj,
                output_mult_adj,
            });
            self.publish_model_identity(&path, model_hash, model_rate, model_bytes, full_wavenet);
            self.finish_commit_ui(&path, metadata, model_info);
            return;
        }

        // F-PLUG-03: Audio epoch revalidation. If the host audio configuration
        // renegotiated while the asynchronous prepare was in flight (or if buffer_size
        // was zero at prepare time), rebuild the resampler and streaming adapter
        // off-RT for the actual active configuration before committing.
        let divergent = audio_epoch.sample_rate != current_host_rate
            || audio_epoch.buffer_size != current_buffer_size
            || new_stream.as_ref().is_none_or(|s| {
                s.host_rate() != current_host_rate || s.max_block() != current_buffer_size
            })
            || new_resampler.is_none();

        if divergent {
            log::info!(
                "NAM-Plug: audio configuration divergence detected during model commit \
                 (epoch: {} Hz/{} frames -> host: {} Hz/{} frames); adapting off-RT",
                audio_epoch.sample_rate,
                audio_epoch.buffer_size,
                current_host_rate,
                current_buffer_size,
            );

            let adapted_resampler = match NamResampler::new_simple(current_host_rate, model_rate) {
                Ok(r) => Box::new(r),
                Err(e) => {
                    self.note_commit_failure(
                        &path,
                        &format!("failed to build polyphase resampler for the active epoch: {e:?}"),
                    );
                    return;
                }
            };

            let adapted_stream = match crate::clap::plugin::build_stream_adapter(
                current_host_rate,
                model_rate,
                current_buffer_size,
            ) {
                Ok(s) => s,
                Err(e) => {
                    self.note_commit_failure(
                        &path,
                        &format!("failed to build stream adapter for the active epoch: {e:?}"),
                    );
                    return;
                }
            };

            if let Some(ref mut model) = model_l
                && let Err(e) = model.set_max_buffer_size(current_buffer_size)
            {
                self.note_commit_failure(
                    &path,
                    &format!("failed to resize left model to {current_buffer_size}: {e:?}"),
                );
                return;
            }
            if let Some(ref mut model) = model_r
                && let Err(e) = model.set_max_buffer_size(current_buffer_size)
            {
                self.note_commit_failure(
                    &path,
                    &format!("failed to resize right model to {current_buffer_size}: {e:?}"),
                );
                return;
            }

            prewarm_cost_ns = if crate::clap::processor::prewarm_policy::ADAPTIVE_COST_ENABLED {
                let measure_frames = crate::clap::processor::prewarm_policy::PREWARM_MEASURE_FRAMES
                    .min(current_buffer_size);
                model_l
                    .as_mut()
                    .map(|model| model.measure_cost_ns_per_sample(measure_frames))
            } else {
                None
            };

            new_resampler = Some(adapted_resampler);
            new_stream = Some(adapted_stream);
        }

        if let (Some(new_resampler), Some(new_stream)) = (new_resampler, new_stream) {
            // Explicit user load supersedes any pending staged restore.
            *self.staged_restore.borrow_mut() = None;

            // Strict Restart Policy: a model swap only changes the physical
            // latency when the streaming adapter's latency changes (i.e. the
            // model rate differs). The audio thread publishes the latency
            // contribution of the *installed* stream in
            // `current_stream_latency`, so comparing it against the new
            // stream's latency is the authoritative same-latency check.
            let current_stream_latency = self
                .shared
                .cold
                .current_stream_latency
                .load(Ordering::Relaxed);
            let new_stream_latency = new_stream.latency_samples();
            if new_stream_latency == current_stream_latency {
                // Same exact latency ⇒ continuous hot swap: deliver through
                // the SPSC and supersede any previously staged model (latest
                // user intent already landed).
                {
                    let mut staged_swap = self.staged_swap.borrow_mut();
                    if let Some(staged) = staged_swap.as_mut() {
                        staged.clear_model();
                        if staged.is_empty() {
                            *staged_swap = None;
                        }
                    }
                }
                let push_result =
                    self.cmd_producer
                        .borrow_mut()
                        .try_push_command(ClapParamPayload::LoadModel {
                            generation,
                            model_l,
                            model_r,
                            new_resampler,
                            new_stream,
                            model_rate,
                            input_mult_adj,
                            output_mult_adj,
                            prewarm_cost_ns,
                        });
                match push_result {
                    Ok(_seq) => {
                        self.publish_model_identity(
                            &path,
                            model_hash,
                            model_rate,
                            model_bytes,
                            full_wavenet,
                        );
                        self.finish_commit_ui(&path, metadata, model_info);
                    }
                    Err((PushError::Full, payload)) => {
                        // Fail-closed — retain the model for retry instead of
                        // dropping it. The model is retained, so the UI
                        // commit advances together with it (F-RB-02);
                        // `flush_pending_model` retries on the next cycle.
                        if let ClapParamPayload::LoadModel {
                            generation,
                            model_l,
                            model_r,
                            new_resampler,
                            new_stream,
                            model_rate: _,
                            input_mult_adj,
                            output_mult_adj,
                            prewarm_cost_ns: _,
                        } = payload
                        {
                            drop(new_resampler); // rebuilt by flush_pending_model()
                            drop(new_stream); // rebuilt by flush_pending_model()
                            *lock_cold(&self.shared.cold.pending_model, "pending_model") =
                                Some(PendingModel {
                                    generation,
                                    model: model_l,
                                    model_r,
                                    model_rate,
                                    input_mult_adj,
                                    output_mult_adj,
                                });
                        }
                        self.host.request_callback();
                        self.publish_model_identity(
                            &path,
                            model_hash,
                            model_rate,
                            model_bytes,
                            full_wavenet,
                        );
                        self.finish_commit_ui(&path, metadata, model_info);
                    }
                }
            } else {
                // Different latency ⇒ Strict Restart Policy: the resources
                // are staged off-RT and land only on the next host restart
                // cycle (`activate()` consumes `staged_swap`). The DSP keeps
                // the old, still-reported latency until then — no sample ever
                // diverges from `PluginLatency::get()`.
                {
                    let mut staged_swap = self.staged_swap.borrow_mut();
                    let staged = staged_swap.get_or_insert_with(StagedSwap::default);
                    staged.model = Some(LoadModelPayload {
                        generation,
                        model_l,
                        model_r,
                        new_resampler,
                        new_stream,
                        model_rate,
                        input_mult_adj,
                        output_mult_adj,
                        prewarm_cost_ns,
                    });
                }
                self.host.request_restart();
                self.publish_model_identity(
                    &path,
                    model_hash,
                    model_rate,
                    model_bytes,
                    full_wavenet,
                );
                self.finish_commit_ui(&path, metadata, model_info);
            }
        } else {
            self.note_commit_failure(
                &path,
                "model resources were not rebuilt for the active audio epoch",
            );
        }
    }

    /// Publishes the adopted model identity only after a valid commit.
    fn publish_model_identity(
        &self,
        path: &Path,
        model_hash: String,
        model_rate: u32,
        model_bytes: usize,
        full_wavenet: Option<Box<StaticModel>>,
    ) {
        {
            let mut params = self.params.borrow_mut();
            params.model_path = Some(path.to_path_buf());
            params.model_basename = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string());
            if let Some(parent) = path.parent() {
                let parent_buf = parent.to_path_buf();
                if !params.model_search_paths.contains(&parent_buf) {
                    params.model_search_paths.push(parent_buf);
                }
            }
            params.model_hash = Some(model_hash);
        }

        self.shared
            .cold
            .ui_model_memory_bytes
            .store(model_bytes, Ordering::Relaxed);
        self.shared
            .cold
            .model_sample_rate
            .store(model_rate, Ordering::Relaxed);
        *lock_cold(&self.shared.cold.full_wavenet_model, "full_wavenet_model") = full_wavenet;
    }

    /// Surfaces a commit failure without publishing any identity: the previous
    /// model identity and DSP stay live.
    fn note_commit_failure(&self, path: &Path, msg: &str) {
        self.shared
            .cold
            .rt_status
            .set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_MODEL_LOAD_FAILED);
        self.shared
            .cold
            .ui_load_error
            .store(true, Ordering::Relaxed);
        *lock_cold(&self.shared.cold.ui_load_error_msg, "ui_load_error_msg") =
            format!("Failed to commit model {:?}: {msg}", path);
        log::error!("NAM-Plug: failed to commit model {path:?}: {msg}");
    }

    /// Loads a new neural model from the specified path.
    ///
    /// Performs I/O and memory allocations, being safe to execute only on the
    /// main thread. The load is split into a pure
    /// `prepare_model` phase and an effectful
    /// `commit_prepared_model` phase; here they
    /// run back-to-back, preserving the fully synchronous behaviour.
    ///
    /// Runs under `&self`: mutations go through the granular interior
    /// mutability, with every `RefCell` borrow scoped so it is released
    /// before any host call.
    pub fn load_model(&self, path: &Path) -> Result<(), Box<NamDiagnostic>> {
        let _scope = neural_amp_modeler_rs::common::diagnostics::scope_instance(
            self.shared.cold.instance_id,
        );

        let host_rate = self.shared.cold.sample_rate.load(Ordering::Relaxed);
        let buffer_size = self.shared.cold.buffer_size.load(Ordering::Relaxed) as usize;

        let prepared = Self::prepare_model(path, host_rate, buffer_size, &self.sys)?;
        self.commit_prepared_model(prepared);
        Ok(())
    }

    /// Loads a new cab-sim impulse response from the specified path.
    ///
    /// This method performs I/O and memory allocations (WAV loading, resampling,
    /// FFT plan construction), being safe to execute only on the main thread.
    /// The constructed `ConvEngine` is sent to the RT thread via a lock-free
    /// SPSC channel following the same pattern as `load_model`.
    pub fn load_cabsim(&self, path: &Path) -> Result<(), Box<NamDiagnostic>> {
        let _scope = neural_amp_modeler_rs::common::diagnostics::scope_instance(
            self.shared.cold.instance_id,
        );

        let host_rate = self.shared.cold.sample_rate.load(Ordering::Relaxed);
        let host_rate = if host_rate == 0 { 48000 } else { host_rate };
        let buffer_size = self.shared.cold.buffer_size.load(Ordering::Relaxed) as usize;
        let partition_size = if buffer_size > 0 { buffer_size } else { 256 };

        // Single read: the digest and the decoded samples are derived from
        // the exact same byte buffer, so a file replaced between hash and
        // adoption cannot desynchronise `ir_hash` from the installed IR.
        let ir_bytes = crate::clap::extensions::state_transaction::read_ir_file_bytes(path)
            .map_err(|e| {
                Box::new(
                    NamDiagnostic::new(NamErrorCode::IrLoadFailed, &self.sys)
                        .message(format!("Failed to read IR file ({path:?}): {e}"))
                        .hint("Assets are adopted only with a verified SHA-256 digest."),
                )
            })?;
        let ir_hash = crate::clap::extensions::state_transaction::compute_bytes_hash(&ir_bytes);

        let cabsim = crate::clap::extensions::state_transaction::load_cabsim_from_verified_bytes(
            &ir_bytes, host_rate, &self.sys, path,
        )?;
        let ir_policy = crate::clap::extensions::ir_level_policy::IrLevelPolicy::from_u8(
            self.shared.cold.ir_level_policy.load(Ordering::Relaxed),
        );

        // When the plugin is already active, build and deliver the pair
        // with the host partition size. When not yet active (buffer_size == 0),
        // skip the push: `activate()` rebuilds the pair from `ir_raw_samples`
        // with the correct partition size, avoiding a stale fallback-sized
        // pair overwriting the activate-built one.
        //
        // Strict Restart Policy: the cabsim latency contribution is
        // exactly the partition size (0 without an IR). Loading the *first* IR
        // (0 → partition) or clearing the active IR (partition → 0) changes the
        // physical latency, so the swap is staged and a host restart is
        // requested; swapping one IR for another (same partition) keeps the
        // exact same latency and applies continuously through the SPSC.
        // Only deliver through the SPSC while a processor owns the command
        // consumer. After a deactivation the cached `sample_rate`/`buffer_size`
        // describe the previous epoch, so pushing a pair built from them would
        // install a stale-rate adapter on the next activation. Deferring leaves
        // the IR in `ir_raw_samples`, and `activate()` rebuilds it for the real
        // epoch.
        let processor_active = self
            .shared
            .cold
            .param_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none();
        let delivered = if processor_active && buffer_size > 0 {
            // Explicit user IR load supersedes any pending staged restore.
            *self.staged_restore.borrow_mut() = None;

            let built_pair = crate::clap::extensions::ir_level_policy::build_cabsim_pair_from_raw(
                &cabsim.samples,
                cabsim.original_rate,
                host_rate,
                partition_size,
                ir_policy,
                &self.sys,
            )?;
            let pair = Some(Box::new(built_pair));

            let current_cabsim_latency = self
                .shared
                .cold
                .current_cabsim_latency
                .load(Ordering::Relaxed);
            let new_cabsim_latency = partition_size as u32;
            if new_cabsim_latency == current_cabsim_latency {
                // Same exact latency ⇒ continuous IR swap (no restart needed).
                // Supersedes any previously staged IR (latest user intent
                // already landed).
                {
                    let mut staged_swap = self.staged_swap.borrow_mut();
                    if let Some(staged) = staged_swap.as_mut() {
                        staged.clear_ir();
                        if staged.is_empty() {
                            *staged_swap = None;
                        }
                    }
                }
                // Fail-closed — if the SPSC is full, put the path back in
                // `ui_pending_ir` and request a callback so housekeeping retries.
                // Do not commit ir_path / ir_raw_samples until the adapter is
                // actually delivered (UI/state would otherwise claim IR is loaded
                // while DSP stays dry).
                match self
                    .cmd_producer
                    .borrow_mut()
                    .try_push_command(ClapParamPayload::LoadCabIr { pair })
                {
                    Ok(_) => true,
                    Err(_) => {
                        *lock_cold(&self.shared.cold.ui_pending_ir, "ui_pending_ir") =
                            Some(path.to_path_buf());
                        self.host.request_callback();
                        false
                    }
                }
            } else {
                // Latency changes (first IR load) ⇒ Strict Restart Policy: stage
                // the adapter and request a host restart. The DSP keeps running
                // without the IR (and reporting the old latency) until the
                // restart cycle installs the staged IR in `activate()`.
                {
                    let mut staged_swap = self.staged_swap.borrow_mut();
                    let staged = staged_swap.get_or_insert_with(StagedSwap::default);
                    staged.ir = Some(pair);
                }
                self.host.request_restart();
                true
            }
        } else {
            true
        };

        if delivered {
            *lock_cold(&self.shared.cold.ir_path, "ir_path") =
                Some(path.to_string_lossy().to_string());
            *lock_cold(&self.shared.cold.ir_hash, "ir_hash") = Some(ir_hash.clone());
            *lock_cold(&self.shared.cold.ir_raw_samples, "ir_raw_samples") = Some(cabsim.samples);
            self.shared
                .cold
                .ir_raw_sample_rate
                .store(cabsim.original_rate, Ordering::Relaxed);
            {
                let mut params = self.params.borrow_mut();
                params.ir_path = Some(path.to_path_buf());
                params.ir_hash = Some(ir_hash);
            }
        }

        Ok(())
    }

    /// Clears the active neural model (unloads model).
    pub fn clear_model(&self) -> Result<(), Box<NamDiagnostic>> {
        let _scope = neural_amp_modeler_rs::common::diagnostics::scope_instance(
            self.shared.cold.instance_id,
        );

        let host_rate = self.shared.cold.sample_rate.load(Ordering::Relaxed);
        let host_rate = if host_rate == 0 { 48000 } else { host_rate };
        let buffer_size = self.shared.cold.buffer_size.load(Ordering::Relaxed) as usize;

        let generation = self.shared.cold.allocate_model_generation();

        *lock_cold(&self.shared.cold.full_wavenet_model, "full_wavenet_model") = None;

        {
            let mut params = self.params.borrow_mut();
            params.model_path = None;
            params.model_basename = None;
            params.model_hash = None;
        }

        *lock_cold(&self.shared.cold.ui_model_metadata, "ui_model_metadata") = None;
        *lock_cold(&self.shared.cold.ui_model_info, "ui_model_info") = None;
        self.shared
            .cold
            .ui_model_memory_bytes
            .store(0, Ordering::Relaxed);
        lock_cold(&self.shared.cold.ui_model_name, "ui_model_name").clear();

        self.shared
            .cold
            .model_sample_rate
            .store(0, Ordering::Relaxed);

        if buffer_size > 0 {
            let new_resampler =
                Box::new(NamResampler::new_simple(host_rate, host_rate).map_err(|e| {
                    Box::new(
                        NamDiagnostic::new(NamErrorCode::ModelBuildFailed, &self.sys)
                            .message("Failed to build dummy resampler for clear")
                            .param("error", e.to_string()),
                    )
                })?);

            let new_stream =
                crate::clap::plugin::build_stream_adapter(host_rate, host_rate, buffer_size)
                    .map_err(|e| {
                        Box::new(
                            NamDiagnostic::new(NamErrorCode::ModelBuildFailed, &self.sys)
                                .message("Failed to build dummy streaming buffer for clear")
                                .param("error", e.to_string()),
                        )
                    })?;

            *self.staged_restore.borrow_mut() = None;

            let current_stream_latency = self
                .shared
                .cold
                .current_stream_latency
                .load(Ordering::Relaxed);
            let new_stream_latency = new_stream.latency_samples();

            if new_stream_latency == current_stream_latency {
                {
                    let mut staged_swap = self.staged_swap.borrow_mut();
                    if let Some(staged) = staged_swap.as_mut() {
                        staged.clear_model();
                        if staged.is_empty() {
                            *staged_swap = None;
                        }
                    }
                }
                let push_result =
                    self.cmd_producer
                        .borrow_mut()
                        .try_push_command(ClapParamPayload::LoadModel {
                            generation,
                            model_l: None,
                            model_r: None,
                            new_resampler,
                            new_stream,
                            model_rate: host_rate,
                            input_mult_adj: 1.0,
                            output_mult_adj: 1.0,
                            prewarm_cost_ns: None,
                        });
                match push_result {
                    Ok(_seq) => {}
                    Err((PushError::Full, payload)) => {
                        if let ClapParamPayload::LoadModel {
                            generation,
                            model_l,
                            model_r,
                            input_mult_adj,
                            output_mult_adj,
                            ..
                        } = payload
                        {
                            *lock_cold(&self.shared.cold.pending_model, "pending_model") =
                                Some(PendingModel {
                                    generation,
                                    model: model_l,
                                    model_r,
                                    model_rate: host_rate,
                                    input_mult_adj,
                                    output_mult_adj,
                                });
                        }
                    }
                }
            } else {
                {
                    let mut staged_swap = self.staged_swap.borrow_mut();
                    let staged = staged_swap.get_or_insert_with(StagedSwap::default);
                    staged.model = Some(LoadModelPayload {
                        generation,
                        model_l: None,
                        model_r: None,
                        new_resampler,
                        new_stream,
                        model_rate: host_rate,
                        input_mult_adj: 1.0,
                        output_mult_adj: 1.0,
                        prewarm_cost_ns: None,
                    });
                }
                self.host.request_restart();
            }
        } else {
            *lock_cold(&self.shared.cold.pending_model, "pending_model") = Some(PendingModel {
                generation,
                model: None,
                model_r: None,
                model_rate: host_rate,
                input_mult_adj: 1.0,
                output_mult_adj: 1.0,
            });
        }

        self.shared
            .cold
            .model_load_counter
            .fetch_add(1, Ordering::Relaxed);

        log::info!("NAM-Plug: model cleared via GUI");
        Ok(())
    }
}

/// Off-thread entry point to the pure `prepare_model` phase.
///
/// A free function so the loader worker can call it without an instance of
/// [`NamClapMainThread`] — every input is an explicit parameter.
pub(crate) fn prepare_model_offthread(
    path: &Path,
    host_rate: u32,
    buffer_size: usize,
    sys: &SystemSnapshot,
) -> Result<PreparedModel, Box<NamDiagnostic>> {
    NamClapMainThread::prepare_model(path, host_rate, buffer_size, sys)
}
