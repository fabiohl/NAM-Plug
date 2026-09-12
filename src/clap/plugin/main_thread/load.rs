// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Model loading (main thread only).

use super::super::shared::StagedSwap;
use super::super::shared::{ClapParamPayload, LoadModelPayload, NamModelMetadata, PendingModel};
use super::NamClapMainThread;
use crate::clap::plugin::command_scheduler::PushError;
use neural_amp_modeler_rs::common::diagnostics::{NamDiagnostic, NamErrorCode};
use neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF;
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::loader::load_and_build_model;
use neural_amp_modeler_rs::models::slimmable::clone_wavenet_for_slimmable_storage;
use neural_amp_modeler_rs::models::{NamModel, StaticModel};
use std::path::Path;
use std::sync::atomic::Ordering;

use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter;
use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;
use neural_amp_modeler_rs::dsp::cabsim::loader::CabSimIr;

impl<'a> NamClapMainThread<'a> {
    /// Loads a new neural model from the specified path.
    ///
    /// This method performs I/O and memory allocations, being safe to execute
    /// only on the main thread. The loaded model is sent to the RT thread
    /// via a lock-free channel.
    pub fn load_model(&mut self, path: &Path) -> Result<(), Box<NamDiagnostic>> {
        let _scope = neural_amp_modeler_rs::common::diagnostics::scope_instance(
            self.shared.cold.instance_id,
        );

        let model_pair = load_and_build_model(
            path,
            &self.sys,
            false,
            neural_amp_modeler_rs::loader::LoadOptions::default(),
        )
        .map_err(|e| {
            Box::new(
                NamDiagnostic::new(NamErrorCode::ModelBuildFailed, &self.sys)
                    .message(format!("Failed to load model: {:?}", path))
                    .param("error", e.to_string()),
            )
        })?;

        if model_pair.model_l.is_none() {
            return Err(Box::new(
                NamDiagnostic::new(NamErrorCode::ModelBuildFailed, &self.sys)
                    .message(format!("Failed to build model: {:?}", path)),
            ));
        }

        let host_rate = self.shared.cold.sample_rate.load(Ordering::Relaxed);
        let host_rate = if host_rate == 0 { 48000 } else { host_rate };
        let model_rate = model_pair.sample_rate;
        if host_rate != model_rate {
            log::warn!(
                "Model sample rate ({model_rate} Hz) differs from host sample rate ({host_rate} Hz); \
                 resampler will convert internally"
            );
        }
        let _new_resampler =
            Box::new(NamResampler::new(host_rate, model_rate, 0).map_err(|e| {
                Box::new(
                    NamDiagnostic::new(NamErrorCode::ModelBuildFailed, &self.sys)
                        .message("Failed to build resampler")
                        .param("error", e.to_string()),
                )
            })?);

        self.params.model_path = Some(path.to_path_buf());
        self.params.model_basename = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string());
        if let Some(parent) = path.parent() {
            let parent_buf = parent.to_path_buf();
            if !self.params.model_search_paths.contains(&parent_buf) {
                self.params.model_search_paths.push(parent_buf);
            }
        }

        // Compute content hash for portable asset identity. The GUI load
        // is the explicit user override path — but the asset is only adopted
        // when its digest can actually be computed, so persisted state always
        // carries a valid `model_hash`.
        let model_hash = crate::clap::extensions::state_transaction::compute_file_hash(path)
            .map_err(|e| {
                Box::new(
                    NamDiagnostic::new(NamErrorCode::ModelBuildFailed, &self.sys)
                        .message(format!("Failed to compute model SHA-256 ({path:?}): {e}"))
                        .hint("Assets are adopted only with a verified SHA-256 digest."),
                )
            })?;
        self.params.model_hash = Some(model_hash);

        let metadata = model_pair.metadata.clone();
        let architecture = model_pair.architecture.clone();
        let topology = model_pair.topology.clone();
        if let Ok(mut meta_guard) = self.shared.cold.ui_model_metadata.lock() {
            *meta_guard = Some(NamModelMetadata {
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
            });
        }

        let model_info = model_pair.model_info(path);
        if let Ok(mut info_guard) = self.shared.cold.ui_model_info.lock() {
            *info_guard = Some(model_info);
        }

        self.shared
            .cold
            .model_sample_rate
            .store(model_rate, Ordering::Relaxed);

        let mut model_l = model_pair.model_l;
        let input_mult_adj = model_pair.input_mult_adj;
        let output_mult_adj = model_pair.output_mult_adj;

        // Allocate a fresh monotonic model generation for this
        // identity now (before any deferral/retry) so the model keeps a stable
        // generation tag through `PendingModel` → `flush_pending_model()`.
        let generation = self.shared.cold.allocate_model_generation();

        // Derive and store full WaveNet weights for main-thread slimmable rebuild.
        // LoadedModelPair no longer carries a pre-computed `full_wavenet` field;
        // we clone it from `model_l` using the same helper used in state_transaction.rs.
        {
            let mut storage = self
                .shared
                .cold
                .full_wavenet_model
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *storage = model_l.as_ref().and_then(|m| {
                if let StaticModel::WavenetDyn(w) = m.as_ref() {
                    clone_wavenet_for_slimmable_storage(w).ok()
                } else {
                    None
                }
            });
        }

        let buffer_size = self.shared.cold.buffer_size.load(Ordering::Relaxed) as usize;
        if buffer_size > 0 {
            // Buffer size is known: create resampler with real host sample rate and capacity
            let buf_capacity = buffer_size.max(MAX_RESAMP_BUF);
            let new_resampler = Box::new(
                NamResampler::new(host_rate, model_rate, buf_capacity).map_err(|e| {
                    Box::new(
                        NamDiagnostic::new(NamErrorCode::ModelBuildFailed, &self.sys)
                            .message("Failed to build polyphase resampler for model")
                            .param("error", e.to_string()),
                    )
                })?,
            );

            if let Some(ref mut model) = model_l
                && let Err(e) = model.set_max_buffer_size(buffer_size)
            {
                return Err(Box::new(
                    NamDiagnostic::new(NamErrorCode::ModelBuildFailed, &self.sys)
                        .message("Failed to set model max buffer size")
                        .param("error", e.to_string()),
                ));
            }

            // Streaming resample adapter, sized for the
            // worst-case host block — built off-RT like the resampler.
            let new_stream =
                crate::clap::plugin::build_stream_adapter(host_rate, model_rate, buffer_size)
                    .map_err(|e| {
                        Box::new(
                            NamDiagnostic::new(NamErrorCode::ModelBuildFailed, &self.sys)
                                .message("Failed to build streaming resample buffer")
                                .param("error", e.to_string()),
                        )
                    })?;

            // Explicit user load supersedes any pending staged restore (TR.1 #5).
            self.staged_restore = None;

            // Strict Restart Policy: a model swap only changes the
            // physical latency when the streaming adapter's latency changes
            // (i.e. the model rate differs). The audio thread publishes the
            // latency contribution of the *installed* stream in
            // `current_stream_latency`, so comparing it against the new
            // stream's latency is the authoritative same-latency check.
            let current_stream_latency = self
                .shared
                .cold
                .current_stream_latency
                .load(Ordering::Relaxed);
            let new_stream_latency = new_stream.latency_samples();
            if new_stream_latency == current_stream_latency {
                // Same exact latency ⇒ continuous hot swap:
                // deliver through the SPSC and supersede any
                // previously staged model (latest user intent already landed).
                if let Some(staged) = self.staged_swap.as_mut() {
                    staged.clear_model();
                    if staged.is_empty() {
                        self.staged_swap = None;
                    }
                }
                match self
                    .cmd_producer
                    .try_push_command(ClapParamPayload::LoadModel {
                        generation,
                        model_l,
                        new_resampler,
                        new_stream,
                        input_mult_adj,
                        output_mult_adj,
                    }) {
                    Ok(_seq) => {}
                    Err((PushError::Full, payload)) => {
                        // Fail-closed — retain the model for retry instead of
                        // dropping it. The UI is not advanced because the model is
                        // not yet installed on the audio thread; `flush_pending_model`
                        // will retry on the next housekeeping cycle.
                        if let ClapParamPayload::LoadModel {
                            generation,
                            model_l,
                            new_resampler,
                            new_stream,
                            input_mult_adj,
                            output_mult_adj,
                        } = payload
                        {
                            drop(new_resampler); // rebuilt by flush_pending_model()
                            drop(new_stream); // rebuilt by flush_pending_model()
                            if let Ok(mut pending_guard) = self.shared.cold.pending_model.lock() {
                                *pending_guard = Some(PendingModel {
                                    generation,
                                    model: model_l,
                                    model_rate,
                                    input_mult_adj,
                                    output_mult_adj,
                                });
                            }
                        }
                        self.host.request_callback();
                        return Ok(());
                    }
                }
            } else {
                // Different latency ⇒ strict Política A: the resources are
                // staged off-RT and land only on the next host restart cycle
                // (`activate()` consumes `staged_swap`). The DSP keeps the
                // old, still-reported latency until then — no sample ever
                // diverges from `PluginLatency::get()`.
                let staged = self.staged_swap.get_or_insert_with(StagedSwap::default);
                staged.model = Some(LoadModelPayload {
                    generation,
                    model_l,
                    new_resampler,
                    new_stream,
                    input_mult_adj,
                    output_mult_adj,
                });
                self.host.request_restart();
            }
        } else {
            // Defer sending until `buffer_size` and `sample_rate`
            // become known in `activate()`. The resampler is NOT constructed here
            // because the host sample rate is unknown during pre-activation state
            // restore (defaults to 48000). `flush_pending_model()` builds the
            // resampler with the correct rates determined at activate() time.
            if let Ok(mut pending_guard) = self.shared.cold.pending_model.lock() {
                *pending_guard = Some(PendingModel {
                    generation,
                    model: model_l,
                    model_rate,
                    input_mult_adj,
                    output_mult_adj,
                });
            }
        }
        self.shared
            .cold
            .model_load_counter
            .fetch_add(1, Ordering::Relaxed);

        let basename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        if let Ok(mut name_guard) = self.shared.cold.ui_model_name.lock() {
            *name_guard = basename;
        }

        if let Some(params_ext) = self
            .host
            .get_extension::<clack_extensions::params::HostParams>()
        {
            params_ext.rescan(
                &mut self.host,
                clack_extensions::params::ParamRescanFlags::VALUES,
            );
        }

        log::info!("Model loaded: {path:?}");

        if let Some(mut state_ext) = self
            .host
            .get_extension::<clack_extensions::state::HostState>()
        {
            state_ext.mark_dirty(&self.host);
        }

        Ok(())
    }

    /// Loads a new cab-sim impulse response from the specified path.
    ///
    /// This method performs I/O and memory allocations (WAV loading, resampling,
    /// FFT plan construction), being safe to execute only on the main thread.
    /// The constructed `ConvEngine` is sent to the RT thread via a lock-free
    /// SPSC channel following the same pattern as `load_model`.
    pub fn load_cabsim(&mut self, path: &Path) -> Result<(), Box<NamDiagnostic>> {
        let _scope = neural_amp_modeler_rs::common::diagnostics::scope_instance(
            self.shared.cold.instance_id,
        );

        let host_rate = self.shared.cold.sample_rate.load(Ordering::Relaxed);
        let host_rate = if host_rate == 0 { 48000 } else { host_rate };
        let buffer_size = self.shared.cold.buffer_size.load(Ordering::Relaxed) as usize;
        let partition_size = if buffer_size > 0 { buffer_size } else { 256 };

        let cabsim = CabSimIr::load(path, host_rate, true).map_err(|e| {
            Box::new(
                NamDiagnostic::new(NamErrorCode::IrLoadFailed, &self.sys)
                    .message(format!("Failed to load cab-sim IR: {:?}", path))
                    .param("error", e.to_string()),
            )
        })?;

        // The IR digest is computed up-front so persisted state always
        // carries a valid `ir_hash`. A file whose digest cannot be computed is
        // never adopted — even on this explicit user override path.
        let ir_hash =
            crate::clap::extensions::state_transaction::compute_file_hash(path).map_err(|e| {
                Box::new(
                    NamDiagnostic::new(NamErrorCode::IrLoadFailed, &self.sys)
                        .message(format!("Failed to compute IR SHA-256 ({path:?}): {e}"))
                        .hint("Assets are adopted only with a verified SHA-256 digest."),
                )
            })?;

        let engine = ConvEngine::new(&cabsim.samples, partition_size).map_err(|e| {
            Box::new(
                NamDiagnostic::new(e, &self.sys)
                    .message("Failed to build convolution engine for cab-sim IR")
                    .hint("The IR samples require more memory than available."),
            )
        })?;

        // When the plugin is already active, build and deliver the adapter
        // with the host partition size. When not yet active (buffer_size == 0),
        // skip the push: `activate()` rebuilds the adapter from `ir_raw_samples`
        // with the correct partition size, avoiding a stale fallback-sized
        // adapter overwriting the activate-built one.
        //
        // Strict Restart Policy: the cabsim latency contribution is
        // exactly the partition size (0 without an IR). Loading the *first* IR
        // (0 → partition) or clearing the active IR (partition → 0) changes the
        // physical latency, so the swap is staged and a host restart is
        // requested; swapping one IR for another (same partition) keeps the
        // exact same latency and applies continuously through the SPSC.
        let delivered = if buffer_size > 0 {
            // Explicit user IR load supersedes any pending staged restore (TR.1 #5).
            self.staged_restore = None;

            // Box here on the main thread so the SPSC payload
            // carries `Box<CabSimAdapter>` and the audio-thread swap moves the
            // old `Box` by value to the GC with zero allocations.
            let adapter = Some(Box::new(CabSimAdapter::new(Box::new(engine)).map_err(
                |e| {
                    Box::new(
                        NamDiagnostic::new(e, &self.sys)
                            .message("Failed to build cab-sim convolution adapter")
                            .hint("The IR samples require more memory than available."),
                    )
                },
            )?));

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
                if let Some(staged) = self.staged_swap.as_mut() {
                    staged.clear_ir();
                    if staged.is_empty() {
                        self.staged_swap = None;
                    }
                }
                // Fail-closed — if the SPSC is full, put the path back in
                // `ui_pending_ir` and request a callback so housekeeping retries.
                // Do not commit ir_path / ir_raw_samples until the adapter is
                // actually delivered (UI/state would otherwise claim IR is loaded
                // while DSP stays dry).
                match self
                    .cmd_producer
                    .try_push_command(ClapParamPayload::LoadCabIr { adapter })
                {
                    Ok(_) => true,
                    Err(_) => {
                        if let Ok(mut pending) = self.shared.cold.ui_pending_ir.lock() {
                            *pending = Some(path.to_path_buf());
                        }
                        self.host.request_callback();
                        false
                    }
                }
            } else {
                // Latency changes (first IR load) ⇒ strict Política A: stage
                // the adapter and request a host restart. The DSP keeps running
                // without the IR (and reporting the old latency) until the
                // restart cycle installs the staged IR in `activate()`.
                let staged = self.staged_swap.get_or_insert_with(StagedSwap::default);
                staged.ir = Some(adapter);
                self.host.request_restart();
                true
            }
        } else {
            true
        };

        if delivered {
            if let Ok(mut ir_guard) = self.shared.cold.ir_path.lock() {
                *ir_guard = Some(path.to_string_lossy().to_string());
            }
            if let Ok(mut hash_guard) = self.shared.cold.ir_hash.lock() {
                *hash_guard = Some(ir_hash.clone());
            }
            if let Ok(mut raw_guard) = self.shared.cold.ir_raw_samples.lock() {
                *raw_guard = Some(cabsim.samples);
            }
            self.shared
                .cold
                .ir_raw_sample_rate
                .store(cabsim.sample_rate, Ordering::Relaxed);
            self.params.ir_path = Some(path.to_path_buf());
            self.params.ir_hash = Some(ir_hash);
        }

        Ok(())
    }

    /// Clears the active neural model (unloads model).
    pub fn clear_model(&mut self) -> Result<(), Box<NamDiagnostic>> {
        let _scope = neural_amp_modeler_rs::common::diagnostics::scope_instance(
            self.shared.cold.instance_id,
        );

        let host_rate = self.shared.cold.sample_rate.load(Ordering::Relaxed);
        let host_rate = if host_rate == 0 { 48000 } else { host_rate };
        let buffer_size = self.shared.cold.buffer_size.load(Ordering::Relaxed) as usize;

        let generation = self.shared.cold.allocate_model_generation();

        if let Ok(mut storage) = self.shared.cold.full_wavenet_model.lock() {
            *storage = None;
        }

        self.params.model_path = None;
        self.params.model_basename = None;
        self.params.model_hash = None;

        if let Ok(mut meta_guard) = self.shared.cold.ui_model_metadata.lock() {
            *meta_guard = None;
        }
        if let Ok(mut info_guard) = self.shared.cold.ui_model_info.lock() {
            *info_guard = None;
        }
        if let Ok(mut name_guard) = self.shared.cold.ui_model_name.lock() {
            name_guard.clear();
        }

        self.shared
            .cold
            .model_sample_rate
            .store(0, Ordering::Relaxed);

        if buffer_size > 0 {
            let buf_capacity = buffer_size.max(MAX_RESAMP_BUF);
            let new_resampler = Box::new(
                NamResampler::new(host_rate, host_rate, buf_capacity).map_err(|e| {
                    Box::new(
                        NamDiagnostic::new(NamErrorCode::ModelBuildFailed, &self.sys)
                            .message("Failed to build dummy resampler for clear")
                            .param("error", e.to_string()),
                    )
                })?,
            );

            let new_stream =
                crate::clap::plugin::build_stream_adapter(host_rate, host_rate, buffer_size)
                    .map_err(|e| {
                        Box::new(
                            NamDiagnostic::new(NamErrorCode::ModelBuildFailed, &self.sys)
                                .message("Failed to build dummy streaming buffer for clear")
                                .param("error", e.to_string()),
                        )
                    })?;

            self.staged_restore = None;

            let current_stream_latency = self
                .shared
                .cold
                .current_stream_latency
                .load(Ordering::Relaxed);
            let new_stream_latency = new_stream.latency_samples();

            if new_stream_latency == current_stream_latency {
                if let Some(staged) = self.staged_swap.as_mut() {
                    staged.clear_model();
                    if staged.is_empty() {
                        self.staged_swap = None;
                    }
                }
                match self
                    .cmd_producer
                    .try_push_command(ClapParamPayload::LoadModel {
                        generation,
                        model_l: None,
                        new_resampler,
                        new_stream,
                        input_mult_adj: 1.0,
                        output_mult_adj: 1.0,
                    }) {
                    Ok(_seq) => {}
                    Err((PushError::Full, payload)) => {
                        if let ClapParamPayload::LoadModel {
                            generation,
                            model_l,
                            input_mult_adj,
                            output_mult_adj,
                            ..
                        } = payload
                            && let Ok(mut pending_guard) = self.shared.cold.pending_model.lock()
                        {
                            *pending_guard = Some(PendingModel {
                                generation,
                                model: model_l,
                                model_rate: host_rate,
                                input_mult_adj,
                                output_mult_adj,
                            });
                        }
                    }
                }
            } else {
                let staged = self.staged_swap.get_or_insert_with(StagedSwap::default);
                staged.model = Some(LoadModelPayload {
                    generation,
                    model_l: None,
                    new_resampler,
                    new_stream,
                    input_mult_adj: 1.0,
                    output_mult_adj: 1.0,
                });
                self.host.request_restart();
            }
        } else {
            if let Ok(mut pending_guard) = self.shared.cold.pending_model.lock() {
                *pending_guard = Some(PendingModel {
                    generation,
                    model: None,
                    model_rate: host_rate,
                    input_mult_adj: 1.0,
                    output_mult_adj: 1.0,
                });
            }
        }

        self.shared
            .cold
            .model_load_counter
            .fetch_add(1, Ordering::Relaxed);

        log::info!("NAM-Plug: model cleared via GUI");
        Ok(())
    }
}
