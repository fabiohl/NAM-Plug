// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Model loading (main thread only).

use super::super::shared::{ClapParamPayload, NamModelMetadata, PendingModel};
use super::NamClapMainThread;
use neural_amp_modeler_rs::common::diagnostics::{NamDiagnostic, NamErrorCode};
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::loader::load_and_build_model;
use neural_amp_modeler_rs::models::NamModel;
use neural_amp_modeler_rs::models::StaticModel;
use neural_amp_modeler_rs::models::slimmable::clone_wavenet_for_slimmable_storage;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::atomic::Ordering;

#[cfg(test)]
use neural_amp_modeler_rs::dsp::cabsim::loader::CabSimIr;

#[cfg(test)]
use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter;

#[cfg(test)]
use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;

impl<'a> NamClapMainThread<'a> {
    /// Loads a new neural model from the specified path.
    ///
    /// This method performs I/O and memory allocations, being safe to execute
    /// only on the main thread. The loaded model is sent to the RT thread
    /// via a lock-free channel.
    pub fn load_model(&mut self, path: &Path) -> Result<(), Box<NamDiagnostic>> {
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
        let new_resampler = Box::new(NamResampler::new(host_rate, model_rate, 0).map_err(|e| {
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

        // S6-E6-T02: compute content hash for portable asset identity
        self.params.model_hash = match std::fs::read(path) {
            Ok(bytes) => {
                let mut hasher = Sha256::new();
                hasher.update(&bytes);
                Some(format!("{:x}", hasher.finalize()))
            }
            Err(e) => {
                log::warn!("NAM-rs: Failed to compute model hash ({path:?}): {e}");
                None
            }
        };

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

        // Store full WaveNet weights for main-thread slimmable rebuild
        {
            let mut storage = self
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

        let buffer_size = self.shared.cold.buffer_size.load(Ordering::Relaxed) as usize;
        if buffer_size > 0 {
            // Main path: buffer_size known, pre-size and send immediately.
            if let Some(ref mut model) = model_l
                && let Err(e) = model.set_max_buffer_size(buffer_size)
            {
                return Err(Box::new(
                    NamDiagnostic::new(NamErrorCode::ModelBuildFailed, &self.sys)
                        .message("Failed to resize model buffers for host buffer size")
                        .param("buffer_size", buffer_size.to_string())
                        .param("error", e.to_string()),
                ));
            }

            self.cmd_producer
                .push_command(ClapParamPayload::LoadModel {
                    model_l,
                    new_resampler,
                    input_mult_adj,
                    output_mult_adj,
                })
                .map_err(|_| {
                    Box::new(
                        NamDiagnostic::new(NamErrorCode::ParamChannelFull, &self.sys)
                            .message("The communication channel with the audio thread is full.")
                            .hint("Please try loading the model again in a few moments."),
                    )
                })?;
        } else {
            // F3 / S1-E1-T02: defer sending until `buffer_size` and `sample_rate`
            // become known in `activate()`. The resampler is NOT constructed here
            // because the host sample rate is unknown during pre-activation state
            // restore (defaults to 48000). `flush_pending_model()` builds the
            // resampler with the correct rates determined at activate() time.
            if let Ok(mut pending_guard) = self.shared.cold.pending_model.lock() {
                *pending_guard = Some(PendingModel {
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
    #[cfg(test)]
    pub fn load_cabsim(&mut self, path: &Path) -> Result<(), Box<NamDiagnostic>> {
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

        let engine = ConvEngine::new(&cabsim.samples, partition_size).map_err(|e| {
            Box::new(
                NamDiagnostic::new(e, &self.sys)
                    .message("Failed to build convolution engine for cab-sim IR")
                    .hint("The IR samples require more memory than available."),
            )
        })?;

        // Store ir_path for state save/load and GUI display
        if let Ok(mut ir_guard) = self.shared.cold.ir_path.lock() {
            *ir_guard = Some(path.to_string_lossy().to_string());
        }

        // Store raw IR samples and their sample rate for adaptive rebuild
        if let Ok(mut raw_guard) = self.shared.cold.ir_raw_samples.lock() {
            *raw_guard = Some(cabsim.samples.clone());
        }
        self.shared
            .cold
            .ir_raw_sample_rate
            .store(cabsim.sample_rate, Ordering::Relaxed);

        let adapter = Some(CabSimAdapter::new(Box::new(engine)).map_err(|e| {
            Box::new(
                NamDiagnostic::new(e, &self.sys)
                    .message("Failed to build cab-sim convolution adapter")
                    .hint("The IR samples require more memory than available."),
            )
        })?);

        self.cmd_producer
            .push_command(ClapParamPayload::LoadCabIr { adapter })
            .map_err(|_| {
                Box::new(
                    NamDiagnostic::new(NamErrorCode::ParamChannelFull, &self.sys)
                        .message("The communication channel with the audio thread is full.")
                        .hint("Please try loading the IR again in a few moments."),
                )
            })?;

        Ok(())
    }
}
