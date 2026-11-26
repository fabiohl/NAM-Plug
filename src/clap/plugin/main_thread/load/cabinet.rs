// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

impl<'a> NamClapMainThread<'a> {
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

        let kind = crate::clap::extensions::cabinet::CabAssetKind::detect_from_path(path)
            .ok_or_else(|| {
                Box::new(
                    NamDiagnostic::new(NamErrorCode::IrLoadFailed, &self.sys)
                        .message(format!(
                            "Unsupported cabinet file extension ({})",
                            path.display()
                        ))
                        .hint(
                            "Supported cabinet formats: .wav IR or 1-in/1-out .nam / .namb model.",
                        ),
                )
            })?;

        let host_rate = self.shared.cold.sample_rate.load(Ordering::Relaxed);
        let host_rate = if host_rate == 0 { 48000 } else { host_rate };
        let buffer_size = self.shared.cold.buffer_size.load(Ordering::Relaxed) as usize;
        let partition_size = if buffer_size > 0 { buffer_size } else { 256 };

        // Single read: the digest and the decoded cabinet asset are derived from
        // the exact same byte buffer, so a file replaced between hash and
        // adoption cannot desynchronise `ir_hash` from the installed cabinet.
        let cab_bytes = crate::clap::extensions::state_transaction::read_ir_file_bytes(path)
            .map_err(|e| {
                Box::new(
                    NamDiagnostic::new(NamErrorCode::IrLoadFailed, &self.sys)
                        .message(format!("Failed to read cabinet file ({path:?}): {e}"))
                        .hint("Assets are adopted only with a verified SHA-256 digest."),
                )
            })?;
        let cab_hash = crate::clap::extensions::state_transaction::compute_bytes_hash(&cab_bytes);
        let ir_policy = crate::clap::extensions::ir_level_policy::IrLevelPolicy::from_u8(
            self.shared.cold.ir_level_policy.load(Ordering::Relaxed),
        );

        let mut resources =
            crate::clap::extensions::state_transaction::validate::build_cab_resources_from_bytes(
                &cab_bytes,
                kind,
                path,
                ir_policy,
                host_rate,
                partition_size as u32,
                &self.sys,
            )?;

        // When the plugin is already active, deliver the validated WAV IR or
        // neural cabinet slot directly or via staged restart according to the
        // Strict Restart Policy. While inactive (`!processor_active ||
        // buffer_size == 0`), keep raw bytes/samples in `ColdShared` so
        // `activate()` builds for the real host sample rate and block size.
        let processor_active = self
            .shared
            .cold
            .param_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none();
        let delivered = if processor_active && buffer_size > 0 {
            *self.staged_restore.borrow_mut() = None;
            let slot = resources.slot.take();
            let current_cabsim_latency = self
                .shared
                .cold
                .current_cabsim_latency
                .load(Ordering::Relaxed);
            let new_cabsim_latency = slot.as_ref().map_or(0, |s| s.latency_samples());
            if new_cabsim_latency == current_cabsim_latency {
                {
                    let mut staged_swap = self.staged_swap.borrow_mut();
                    if let Some(staged) = staged_swap.as_mut() {
                        staged.clear_ir();
                        if staged.is_empty() {
                            *staged_swap = None;
                        }
                    }
                }
                match self
                    .cmd_producer
                    .borrow_mut()
                    .try_push_command(ClapParamPayload::LoadCabIr { pair: slot })
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
                {
                    let mut staged_swap = self.staged_swap.borrow_mut();
                    let staged = staged_swap.get_or_insert_with(StagedSwap::default);
                    staged.ir = Some(slot);
                }
                self.host.request_restart();
                true
            }
        } else {
            // Inactive processor: drop the built DSP handle off-RT and let
            // `activate()` construct from ColdShared at the true host rate.
            drop(resources.slot.take());
            true
        };

        if delivered {
            *lock_cold(&self.shared.cold.ir_path, "ir_path") =
                Some(path.to_string_lossy().to_string());
            *lock_cold(&self.shared.cold.ir_hash, "ir_hash") = Some(cab_hash.clone());
            self.shared
                .cold
                .cab_kind
                .store(kind.to_u8(), Ordering::Relaxed);
            *lock_cold(&self.shared.cold.cab_ui_meta, "cab_ui_meta") = Some(resources.ui_meta);

            match kind {
                crate::clap::extensions::cabinet::CabAssetKind::WavIr => {
                    *lock_cold(&self.shared.cold.ir_raw_samples, "ir_raw_samples") =
                        Some(resources.samples);
                    self.shared
                        .cold
                        .ir_raw_sample_rate
                        .store(resources.sample_rate, Ordering::Relaxed);
                    *lock_cold(&self.shared.cold.cab_nam_raw_bytes, "cab_nam_raw_bytes") = None;
                }
                crate::clap::extensions::cabinet::CabAssetKind::NamModel
                | crate::clap::extensions::cabinet::CabAssetKind::NambModel => {
                    *lock_cold(&self.shared.cold.ir_raw_samples, "ir_raw_samples") = None;
                    self.shared
                        .cold
                        .ir_raw_sample_rate
                        .store(0, Ordering::Relaxed);
                    *lock_cold(&self.shared.cold.cab_nam_raw_bytes, "cab_nam_raw_bytes") =
                        resources.nam_raw;
                }
            }

            {
                let mut params = self.params.borrow_mut();
                params.ir_path = Some(path.to_path_buf());
                params.ir_hash = Some(cab_hash);
            }
        }

        Ok(())
    }
}
