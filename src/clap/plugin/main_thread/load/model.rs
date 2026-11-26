// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

impl<'a> NamClapMainThread<'a> {
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

        let prepared = match Self::prepare_model(path, host_rate, buffer_size, &self.sys) {
            Ok(p) => p,
            Err(e) => {
                self.shared
                    .cold
                    .rt_status
                    .set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_MODEL_LOAD_FAILED);
                return Err(e);
            }
        };
        self.commit_prepared_model(prepared);
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

        // Clear the process-global crash telemetry for the now-empty slot.
        if let Ok(mut active_name) =
            neural_amp_modeler_rs::common::diagnostics::ACTIVE_MODEL_NAME.try_write()
        {
            active_name.clear();
        }
        if let Ok(mut active_info) =
            neural_amp_modeler_rs::common::diagnostics::ACTIVE_MODEL_INFO.try_write()
        {
            *active_info = None;
        }
        neural_amp_modeler_rs::common::diagnostics::ACTIVE_SAMPLE_RATE.store(0, Ordering::Relaxed);

        if buffer_size > 0 {
            let new_resampler = Box::new(
                NamResampler::new_simple_typed(host_rate, host_rate).map_err(|e| {
                    Box::new(
                        NamDiagnostic::new(e, &self.sys)
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
                            cal_meta:
                                crate::clap::extensions::calibration::ModelCalibrationMeta::empty(),
                            prewarm_cost_ns: None,
                        });
                match push_result {
                    Ok(_seq) => {}
                    Err(err) => {
                        let (PushError::Full, payload) = *err;
                        if let ClapParamPayload::LoadModel {
                            generation,
                            model_l,
                            model_r,
                            input_mult_adj,
                            output_mult_adj,
                            cal_meta,
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
                                    cal_meta,
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
                        cal_meta: crate::clap::extensions::calibration::ModelCalibrationMeta::empty(
                        ),
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
                cal_meta: crate::clap::extensions::calibration::ModelCalibrationMeta::empty(),
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
