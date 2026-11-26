// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

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

        // Single in-memory read (bounded, regular file) reused for both hashing
        // and model construction — no second full-file I/O.
        let (bytes, model_hash) =
            crate::clap::extensions::state_transaction::read_model_file_bytes(path).map_err(
                |e| {
                    Box::new(
                        NamDiagnostic::new(NamErrorCode::ModelBuildFailed, sys)
                            .message(format!("Failed to read model file ({path:?}): {e}"))
                            .hint("Assets are adopted only with a verified SHA-256 digest."),
                    )
                },
            )?;

        let path_label = path.to_string_lossy();
        let model_pair = neural_amp_modeler_rs::loader::load_and_build_model_from_bytes_named(
            &bytes,
            path_label.as_ref(),
            sys,
            true,
            neural_amp_modeler_rs::loader::LoadOptions::default(),
        )
        .map_err(|e| {
            Box::new(
                NamDiagnostic::new(e.code(), sys)
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

        // TOCTOU mitigation (test-only mutation hook): verify the path was not
        // modified while the verified bytes were being built.
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

        let metadata = model_pair.metadata.clone();
        let cal_meta =
            crate::clap::extensions::calibration::ModelCalibrationMeta::from_loaded_model(
                &model_pair,
            );
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
            input_level_dbu: cal_meta.input_level_dbu,
            output_level_dbu: cal_meta.output_level_dbu,
            loudness: cal_meta.loudness,
        };

        let model_info = model_pair.model_info(path);
        let model_bytes = model_pair.model_memory_bytes().unwrap_or(0);

        let mut model_l = model_pair.model_l;
        let mut model_r = model_pair.model_r;

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
                NamResampler::new_simple_typed(host_rate, model_rate).map_err(|e| {
                    Box::new(
                        NamDiagnostic::new(e, sys)
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

            // Off-RT cost calibration: measure the freshly loaded, stabilized
            // model so the audio thread sizes its first prewarm window from real
            // hardware. `None` (no model) falls back to the per-family table.
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
            cal_meta,
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
}
