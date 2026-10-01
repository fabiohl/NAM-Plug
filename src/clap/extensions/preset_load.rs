// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! CLAP `clap.preset-load` extension implementation.
//!
//! Enables hosts to load NAM model files as presets via the host's preset browser.
//!
//! The extension enqueues a `PendingModelRequest` with `ModelRequestOrigin::Preset` into
//! `pending_model_requests` and triggers `host.request_callback()`. When `housekeeping()`
//! completes each load on the main thread, it calls `HostPresetLoad::loaded()` on success
//! or `HostPresetLoad::on_error()` on failure, passing the original `location` and `load_key`
//! of that specific request, satisfying the CLAP spec contract.

use crate::clap::plugin::shared::{
    MAX_PENDING_MODEL_REQUESTS, ModelRequestOrigin, PendingModelRequest,
};
use clack_extensions::preset_discovery::prelude::*;
use clack_plugin::prelude::*;
use std::ffi::CStr;
use std::path::PathBuf;

use crate::clap::plugin::NamClapMainThread;
use crate::clap::plugin::debug_assert_main_thread;

impl PluginPresetLoadImpl for NamClapMainThread<'_> {
    fn load_from_location(
        &self,
        location: Location,
        load_key: Option<&CStr>,
    ) -> Result<(), PluginError> {
        debug_assert_main_thread(&self.host);
        let path = match location {
            Location::File { path } => path,
            Location::Plugin => {
                return Err(PluginError::Message(
                    "Cannot load NAM model from plugin container",
                ));
            }
        };

        let path_str = path
            .to_str()
            .map_err(|_| PluginError::Message("Invalid UTF-8 in model path"))?;
        let path_buf = PathBuf::from(path_str);

        // Pre-enqueuing validation: verify that the file exists and is a regular file.
        // If not, return error immediately without modifying the queue or state.
        if !path_buf.exists() || !path_buf.is_file() {
            log::error!(
                "NAM-Plug: Model file does not exist or is not a regular file: {path_str:?}"
            );
            return Err(PluginError::Message(
                "Model file does not exist or is not a regular file",
            ));
        }

        let preset_name = path_buf
            .file_stem()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        let origin = ModelRequestOrigin::Preset {
            location: path.to_owned(),
            load_key: load_key.map(|k| k.to_owned()),
        };

        // Enqueue the model load request into the unified bounded FIFO queue.
        {
            let mut pending_guard = self
                .shared
                .cold
                .pending_model_requests
                .lock()
                .unwrap_or_else(|e| {
                    log::error!("PoisonError in pending_model_requests lock: {e:?}");
                    e.into_inner()
                });
            if pending_guard.len() >= MAX_PENDING_MODEL_REQUESTS {
                log::error!(
                    "NAM-Plug: Model request queue full (max {MAX_PENDING_MODEL_REQUESTS} pending requests)"
                );
                return Err(PluginError::Message("Preset load queue full"));
            }
            pending_guard.push_back(PendingModelRequest {
                path: path_buf,
                origin,
            });
        }

        self.shared
            .cold
            .ui_loading
            .store(true, std::sync::atomic::Ordering::Relaxed);

        self.host.shared().request_callback();

        log::info!("Loading preset \"{preset_name}\" from {path_str:?}");

        Ok(())
    }
}
