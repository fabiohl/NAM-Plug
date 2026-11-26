// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

impl<'a> NamClapMainThread<'a> {
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
        let basename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        // Publish the process-global crash telemetry (F-DIAG-01) using
        // `try_write` so this main-thread path never blocks on a panic report.
        if let Ok(mut active_name) =
            neural_amp_modeler_rs::common::diagnostics::ACTIVE_MODEL_NAME.try_write()
        {
            active_name.clear();
            active_name.push_str(&basename);
        }
        if let Ok(mut active_info) =
            neural_amp_modeler_rs::common::diagnostics::ACTIVE_MODEL_INFO.try_write()
        {
            *active_info = Some(model_info.clone());
        }
        neural_amp_modeler_rs::common::diagnostics::ACTIVE_SAMPLE_RATE
            .store(metadata.sample_rate, Ordering::Relaxed);

        *lock_cold(&self.shared.cold.ui_model_metadata, "ui_model_metadata") = Some(metadata);
        *lock_cold(&self.shared.cold.ui_model_info, "ui_model_info") = Some(model_info);

        self.shared
            .cold
            .model_load_counter
            .fetch_add(1, Ordering::Relaxed);

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

        // A successful adoption clears any stale load error: a later
        // success must never be shadowed by an earlier failure.
        self.shared
            .cold
            .rt_status
            .clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_MODEL_LOAD_FAILED);
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
            cal_meta,
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
        let (input_mult_adj, output_mult_adj) =
            crate::clap::extensions::calibration::compute_calibration_multipliers(
                cal_meta,
                self.shared.calibration_config(),
            );

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
                cal_meta,
            });
            self.publish_model_identity(&path, model_hash, model_rate, model_bytes, full_wavenet);
            self.finish_commit_ui(&path, metadata, model_info);
            return;
        }

        // Audio epoch revalidation: if the host audio configuration renegotiated
        // while the asynchronous prepare was in flight (or if buffer_size was
        // zero at prepare time), rebuild the resampler and streaming adapter
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

            let adapted_resampler =
                match NamResampler::new_simple_typed(current_host_rate, model_rate) {
                    Ok(r) => Box::new(r),
                    Err(e) => {
                        self.note_commit_failure(
                            &path,
                            &format!(
                                "failed to build polyphase resampler for the active epoch: {e:?}"
                            ),
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
            // Attach the canonical RT status instance off-RT so the audio-thread
            // install only performs a pointer-identity no-op.
            crate::clap::plugin::shared::preinject_rt_status(
                &self.shared.cold.rt_status,
                &mut model_l,
                &mut model_r,
            );

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
                            cal_meta,
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
                    Err(err) => {
                        let (PushError::Full, payload) = *err;
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
                            cal_meta,
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
                                    cal_meta,
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
                        cal_meta,
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
}
