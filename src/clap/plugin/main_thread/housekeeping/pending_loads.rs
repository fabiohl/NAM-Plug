// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Main-thread pending model and cabinet IR load/clear handling, asynchronous
//! model loader dispatch, and preset-load host notifications.

use super::super::NamClapMainThread;
use super::super::load::AudioEpoch;
use super::super::model_loader::{ModelLoader, ModelLoaderJob};
use crate::clap::extensions::cabinet::{CabAssetKind, CabSlotDsp};
use crate::clap::extensions::ir_level_policy::{IrLevelPolicy, build_cabsim_pair_from_raw};
use crate::clap::gui::dialog_state;
use crate::clap::plugin::ClapParamPayload;
use crate::clap::plugin::lock::lock_cold;
use crate::clap::plugin::shared::{
    MAX_PENDING_MODEL_REQUESTS, ModelRequestOrigin, PendingModelRequest, PresetLocationOrigin,
    StagedSwap,
};
use clack_extensions::preset_discovery::prelude::*;
use clack_plugin::host::HostMainThreadHandle;
use std::sync::Arc;
use std::sync::atomic::Ordering;

impl<'a> NamClapMainThread<'a> {
    /// Propagates file dialog results and processes pending model loads.
    pub(super) fn handle_pending_model_load(&self) {
        if let Some(dialog_state) = self.shared.cold.dialog_state.as_ref() {
            let mut dialog_guard =
                lock_cold(&dialog_state.pending_model, "dialog_state.pending_model");
            if let Some(path) = dialog_guard.take() {
                self.shared
                    .cold
                    .ui_dialog_open
                    .store(false, Ordering::Relaxed);
                let mut queue_guard = lock_cold(
                    &self.shared.cold.pending_model_requests,
                    "pending_model_requests",
                );
                if queue_guard.len() < MAX_PENDING_MODEL_REQUESTS {
                    queue_guard.push_back(PendingModelRequest {
                        path,
                        origin: ModelRequestOrigin::Gui,
                    });
                } else {
                    log::warn!(
                        "NAM-Plug: pending_model_requests queue full, dropping dialog result"
                    );
                }
            }
        }

        self.accept_queued_model_request();
        self.poll_model_loader_result();
        self.dispatch_pending_model_load();
    }

    fn accept_queued_model_request(&self) {
        let (request, remaining_count) = {
            let mut queue_guard = lock_cold(
                &self.shared.cold.pending_model_requests,
                "pending_model_requests",
            );
            let popped = queue_guard.pop_front();
            let count = queue_guard.len();
            (popped, count)
        };

        if let Some(req) = request {
            self.accept_model_request(req);
            if remaining_count > 0 {
                self.host.request_callback();
            }
        }
    }

    fn accept_model_request(&self, req: PendingModelRequest) {
        let cancelled_sentinel = dialog_state::dialog_cancelled_sentinel();
        let timedout_sentinel = dialog_state::dialog_timedout_sentinel();

        if req.path == cancelled_sentinel {
            log::info!("NAM-Plug: model file dialog cancelled by user");
            self.shared
                .cold
                .ui_dialog_open
                .store(false, Ordering::Relaxed);
            self.maybe_finish_loading_ui();
            return;
        }
        if req.path == timedout_sentinel {
            log::info!("NAM-Plug: model file dialog timed out");
            self.shared
                .cold
                .ui_dialog_open
                .store(false, Ordering::Relaxed);
            self.maybe_finish_loading_ui();
            return;
        }

        self.shared
            .cold
            .ui_dialog_open
            .store(false, Ordering::Relaxed);

        let generation = self.shared.cold.allocate_model_generation();
        self.shared.cold.ui_loading.store(true, Ordering::Relaxed);

        let is_gui = matches!(req.origin, ModelRequestOrigin::Gui);
        let job = ModelLoaderJob {
            path: req.path,
            origin: req.origin,
            generation,
            audio_epoch: AudioEpoch {
                sample_rate: 0,
                buffer_size: 0,
            },
        };

        let mut pending = self.pending_async_loads.borrow_mut();
        if is_gui {
            pending.retain(|j| !matches!(j.origin, ModelRequestOrigin::Gui));
            if self.model_worker_busy.get() && self.dispatched_model_is_gui.get() {
                self.dispatched_model_generation.set(0);
            }
        }
        let evicted = if pending.len() >= MAX_PENDING_MODEL_REQUESTS {
            pending.pop_front()
        } else {
            None
        };
        pending.push_back(job);
        drop(pending);

        if let Some(evicted) = evicted
            && let ModelRequestOrigin::Preset { location, load_key } = evicted.origin
        {
            log::warn!(
                "NAM-Plug: superseded a queued preset model load (queue full at {MAX_PENDING_MODEL_REQUESTS})"
            );
            notify_preset_error(
                &self.host,
                &location,
                load_key.as_deref(),
                0,
                "Preset model load superseded before it could start",
            );
        }
    }

    fn maybe_finish_loading_ui(&self) {
        if !self.model_worker_busy.get() && self.pending_async_loads.borrow().is_empty() {
            self.shared.cold.ui_loading.store(false, Ordering::Relaxed);
        }
    }

    fn run_sync_model_load(&self, req: PendingModelRequest) {
        self.shared.cold.ui_loading.store(true, Ordering::Relaxed);
        let res = self.load_model(&req.path);
        self.shared.cold.ui_loading.store(false, Ordering::Relaxed);

        match res {
            Ok(_) => {
                if let ModelRequestOrigin::Preset { location, load_key } = req.origin {
                    notify_preset_loaded(&self.host, &location, load_key.as_deref());
                }
            }
            Err(e) => self.report_model_load_error(&req.origin, &e),
        }
    }

    fn report_model_load_error(
        &self,
        origin: &ModelRequestOrigin,
        e: &neural_amp_modeler_rs::common::diagnostics::NamDiagnostic,
    ) {
        let err_msg = if !e.user_hint().is_empty() {
            format!("{}: {}", e.user_message(), e.user_hint())
        } else if !e.user_message().is_empty() {
            e.user_message().to_string()
        } else {
            e.error_code().message().to_string()
        };
        self.shared
            .cold
            .rt_status
            .set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_MODEL_LOAD_FAILED);
        *lock_cold(&self.shared.cold.ui_load_error_msg, "ui_load_error_msg") = err_msg.clone();
        self.shared
            .cold
            .ui_load_error
            .store(true, Ordering::Relaxed);

        if let ModelRequestOrigin::Preset { location, load_key } = origin {
            notify_preset_error(
                &self.host,
                location,
                load_key.as_deref(),
                e.error_code() as i32,
                &err_msg,
            );
        }

        log::error!("Failed to load model: {e:?}");
    }

    fn poll_model_loader_result(&self) {
        let result = {
            let mut slot = self
                .model_result_slot
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            slot.take()
        };
        let Some(result) = result else {
            return;
        };
        self.model_worker_busy.set(false);

        if result.generation == 0 || result.generation != self.dispatched_model_generation.get() {
            log::debug!(
                "NAM-Plug: discarding superseded async model load (generation {})",
                result.generation
            );
            self.dispatched_model_generation.set(0);
            self.dispatched_model_is_gui.set(false);
            return;
        }
        self.dispatched_model_generation.set(0);
        self.dispatched_model_is_gui.set(false);

        let still_loading = !self.pending_async_loads.borrow().is_empty();
        self.shared
            .cold
            .ui_loading
            .store(still_loading, Ordering::Relaxed);
        match result.outcome {
            Ok(prepared) => {
                self.commit_prepared_model_with_generation(prepared, result.generation);
                if let ModelRequestOrigin::Preset { location, load_key } = result.origin {
                    notify_preset_loaded(&self.host, &location, load_key.as_deref());
                }
            }
            Err(e) => self.report_model_load_error(&result.origin, &e),
        }
    }

    fn dispatch_pending_model_load(&self) {
        if self.model_worker_busy.get() {
            return;
        }
        let Some(mut job) = self.pending_async_loads.borrow_mut().pop_front() else {
            return;
        };

        job.audio_epoch = AudioEpoch {
            sample_rate: self.shared.cold.sample_rate.load(Ordering::Relaxed),
            buffer_size: self.shared.cold.buffer_size.load(Ordering::Relaxed) as usize,
        };

        let have_loader = self
            .model_loader
            .borrow()
            .as_ref()
            .is_some_and(|loader| !loader.is_finished());
        if !have_loader && let Err(msg) = self.spawn_model_loader() {
            log::warn!(
                "NAM-Plug: model loader worker unavailable ({msg}); \
                 falling back to synchronous load"
            );
            self.run_sync_model_load(PendingModelRequest {
                path: job.path,
                origin: job.origin,
            });
            return;
        }

        let job_generation = job.generation;
        let job_is_gui = matches!(job.origin, ModelRequestOrigin::Gui);

        let dispatch_result = {
            let loader = self.model_loader.borrow();
            loader.as_ref().map(|loader| loader.dispatch(job))
        };

        match dispatch_result {
            Some(Ok(())) => {
                self.model_worker_busy.set(true);
                self.dispatched_model_generation.set(job_generation);
                self.dispatched_model_is_gui.set(job_is_gui);
            }
            Some(Err(job)) => {
                log::warn!(
                    "NAM-Plug: model loader dispatch failed; falling back to synchronous load"
                );
                *self.model_loader.borrow_mut() = None;
                self.run_sync_model_load(PendingModelRequest {
                    path: job.path,
                    origin: job.origin,
                });
            }
            None => {
                log::error!("NAM-Plug: model loader missing after spawn");
            }
        }
    }

    pub(super) fn spawn_model_loader(&self) -> Result<(), &'static str> {
        let (host, shared_arc) = self.host_bridge_and_shared();
        let rebuilds = Some(self.rebuild_slots.clone());
        let loader = ModelLoader::spawn(
            self.shared.cold.instance_id,
            self.sys.clone(),
            shared_arc,
            host,
            Arc::clone(&self.model_result_slot),
            rebuilds,
        )?;
        *self.model_loader.borrow_mut() = Some(loader);
        Ok(())
    }

    /// Propagates IR dialog results and processes pending IR loads / clears.
    pub(super) fn handle_pending_ir_and_clear(&self) {
        // 1. Drain any completed file-picker result into `ui_pending_ir`.
        if let Some(ir_dialog_state) = self.shared.cold.ir_dialog_state.as_ref() {
            let mut dialog_guard =
                lock_cold(&ir_dialog_state.pending_ir, "ir_dialog_state.pending_ir");
            if let Some(path) = dialog_guard.take() {
                *lock_cold(&self.shared.cold.ui_pending_ir, "ui_pending_ir") = Some(path);
            }
        }

        // 2. Process pending cabinet asset load (or cancel/timeout sentinels).
        let pending_ir = lock_cold(&self.shared.cold.ui_pending_ir, "ui_pending_ir").take();
        if let Some(path) = pending_ir {
            let cancelled_sentinel = dialog_state::dialog_cancelled_sentinel();
            let timedout_sentinel = dialog_state::dialog_timedout_sentinel();

            if path == cancelled_sentinel {
                log::info!("NAM-Plug: IR file dialog cancelled by user");
                self.shared
                    .cold
                    .ui_ir_loading
                    .store(false, Ordering::Relaxed);
            } else if path == timedout_sentinel {
                log::info!("NAM-Plug: IR file dialog timed out");
                self.shared
                    .cold
                    .ui_ir_loading
                    .store(false, Ordering::Relaxed);
            } else {
                let res = self.load_cabsim(&path);
                self.shared
                    .cold
                    .ui_ir_loading
                    .store(false, Ordering::Relaxed);
                match res {
                    Ok(_) => {
                        self.shared
                            .cold
                            .ui_ir_load_error
                            .store(false, Ordering::Relaxed);
                        lock_cold(
                            &self.shared.cold.ui_ir_load_error_msg,
                            "ui_ir_load_error_msg",
                        )
                        .clear();
                    }
                    Err(e) => {
                        let detail = e.to_string();
                        let err_msg = if detail.trim().is_empty() {
                            e.error_code().message().to_string()
                        } else {
                            detail
                        };
                        *lock_cold(
                            &self.shared.cold.ui_ir_load_error_msg,
                            "ui_ir_load_error_msg",
                        ) = err_msg;
                        self.shared
                            .cold
                            .ui_ir_load_error
                            .store(true, Ordering::Relaxed);

                        log::error!("Failed to load cabinet asset from GUI: {e:?}");
                    }
                }
            }
        }

        // 3. Process pending cabinet clear request from GUI.
        if self.shared.cold.ui_clear_ir.load(Ordering::Relaxed) {
            let current_cabsim_latency = self
                .shared
                .cold
                .current_cabsim_latency
                .load(Ordering::Relaxed);
            let has_cab_path = lock_cold(&self.shared.cold.ir_path, "ir_path").is_some();
            if current_cabsim_latency == 0 && !has_cab_path {
                self.shared.cold.ui_clear_ir.store(false, Ordering::Relaxed);
            } else {
                // Zero-latency cabinet clears swap immediately via SPSC when active;
                // latency-bearing cabinets require a host restart to update PDC cleanly.
                let cleared = if current_cabsim_latency == 0 {
                    let processor_active = self
                        .shared
                        .cold
                        .param_rx
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_none();
                    let buffer_size = self.shared.cold.buffer_size.load(Ordering::Relaxed);
                    if processor_active && buffer_size > 0 {
                        match self
                            .cmd_producer
                            .borrow_mut()
                            .try_push_command(ClapParamPayload::LoadCabIr { pair: None })
                        {
                            Ok(_) => true,
                            Err(_) => {
                                self.host.request_callback();
                                false
                            }
                        }
                    } else {
                        true
                    }
                } else {
                    {
                        let mut staged_swap = self.staged_swap.borrow_mut();
                        let staged = staged_swap.get_or_insert_with(StagedSwap::default);
                        staged.ir = Some(None);
                    }
                    self.host.request_restart();
                    true
                };

                if cleared {
                    self.shared.cold.ui_clear_ir.store(false, Ordering::Relaxed);
                    self.shared.cold.cab_kind.store(0, Ordering::Relaxed);
                    self.shared
                        .cold
                        .ui_ir_load_error
                        .store(false, Ordering::Relaxed);
                    *lock_cold(
                        &self.shared.cold.ui_ir_load_error_msg,
                        "ui_ir_load_error_msg",
                    ) = String::new();
                    *lock_cold(&self.shared.cold.ir_path, "ir_path") = None;
                    *lock_cold(&self.shared.cold.ir_hash, "ir_hash") = None;
                    *lock_cold(&self.shared.cold.ir_raw_samples, "ir_raw_samples") = None;
                    self.shared
                        .cold
                        .ir_raw_sample_rate
                        .store(0, Ordering::Relaxed);
                    *lock_cold(&self.shared.cold.cab_nam_raw_bytes, "cab_nam_raw_bytes") = None;
                    *lock_cold(&self.shared.cold.cab_ui_meta, "cab_ui_meta") = None;
                    {
                        let mut params = self.params.borrow_mut();
                        params.ir_path = None;
                        params.ir_hash = None;
                    }
                    log::info!("NAM-Plug: cabinet asset cleared via GUI");
                }
            }
        }

        // 4. Process pending amp model clear request from GUI.
        if self.shared.cold.ui_clear_model.load(Ordering::Relaxed) {
            self.shared
                .cold
                .ui_clear_model
                .store(false, Ordering::Relaxed);
            if let Err(e) = self.clear_model() {
                log::error!("Failed to clear model from GUI: {e:?}");
            }
        }

        // 5. Rebuild active WAV cabinet convolution when the user cycles IrLevelPolicy.
        if self
            .shared
            .cold
            .ui_ir_policy_rebuild_pending
            .swap(false, Ordering::AcqRel)
        {
            let active_kind =
                CabAssetKind::from_u8(self.shared.cold.cab_kind.load(Ordering::Relaxed));
            let is_wav = active_kind == Some(CabAssetKind::WavIr) || active_kind.is_none();
            let policy =
                IrLevelPolicy::from_u8(self.shared.cold.ir_level_policy.load(Ordering::Relaxed));
            let raw_opt = if is_wav {
                lock_cold(&self.shared.cold.ir_raw_samples, "ir_raw_samples").clone()
            } else {
                None
            };
            if let Some(raw_samples) = raw_opt {
                let buffer_size = self.shared.cold.buffer_size.load(Ordering::Relaxed);
                let processor_active = self
                    .shared
                    .cold
                    .param_rx
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_none();
                if processor_active && buffer_size > 0 {
                    let host_rate = self.shared.cold.sample_rate.load(Ordering::Relaxed);
                    let host_rate = if host_rate == 0 { 48_000 } else { host_rate };
                    let original_rate = self.shared.cold.ir_raw_sample_rate.load(Ordering::Relaxed);
                    match build_cabsim_pair_from_raw(
                        &raw_samples,
                        original_rate,
                        host_rate,
                        buffer_size as usize,
                        policy,
                        &self.sys,
                    ) {
                        Ok(pair) => {
                            if self
                                .cmd_producer
                                .borrow_mut()
                                .try_push_command(ClapParamPayload::LoadCabIr {
                                    pair: Some(CabSlotDsp::WavIr(Box::new(pair))),
                                })
                                .is_err()
                            {
                                self.shared
                                    .cold
                                    .ui_ir_policy_rebuild_pending
                                    .store(true, Ordering::Release);
                                self.host.request_callback();
                            } else {
                                log::info!(
                                    "NAM-Plug: rebuilt cab-sim IR with level policy '{}'",
                                    policy.as_str()
                                );
                                if let Some(state_ext) = self
                                    .host
                                    .get_extension::<clack_extensions::state::HostState>()
                                {
                                    state_ext.mark_dirty(&self.host);
                                }
                            }
                        }
                        Err(e) => {
                            log::error!(
                                "NAM-Plug: failed to rebuild cab-sim IR for policy '{}': {:?}",
                                policy.as_str(),
                                e
                            );
                        }
                    }
                }
            }
        }
    }
}

fn notify_preset_loaded(
    host: &HostMainThreadHandle,
    location: &PresetLocationOrigin,
    load_key: Option<&std::ffi::CStr>,
) {
    if let Some(preset_load) = host.get_extension::<HostPresetLoad>() {
        preset_load.loaded(host, location.as_clap_location(), load_key);
        log::info!("Host notified: preset loaded successfully");
    }
}

fn notify_preset_error(
    host: &HostMainThreadHandle,
    location: &PresetLocationOrigin,
    load_key: Option<&std::ffi::CStr>,
    os_error: i32,
    message: &str,
) {
    if let Some(preset_load) = host.get_extension::<HostPresetLoad>() {
        let msg_cstr = std::ffi::CString::new(message);
        let msg_ref = msg_cstr.as_deref().ok();
        preset_load.on_error(
            host,
            location.as_clap_location(),
            load_key,
            os_error,
            msg_ref,
        );
        log::error!("Host notified: preset load failed (os_error={os_error})");
    }
}
