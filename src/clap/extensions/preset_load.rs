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

use crate::clap::factory::preset_discovery::{
    is_supported_model_extension, resolve_home_model_dir,
};
use crate::clap::plugin::shared::{
    MAX_PENDING_MODEL_REQUESTS, ModelRequestOrigin, PendingModelRequest, PresetLocationOrigin,
};
use clack_extensions::preset_discovery::prelude::*;
use clack_plugin::prelude::*;
use std::ffi::CStr;
use std::path::{Component, PathBuf};

use crate::clap::plugin::NamClapMainThread;
use crate::clap::plugin::debug_assert_main_thread;

impl PluginPresetLoadImpl for NamClapMainThread<'_> {
    fn load_from_location(
        &self,
        location: Location,
        load_key: Option<&CStr>,
    ) -> Result<(), PluginError> {
        debug_assert_main_thread(&self.host);

        let (path_buf, location_origin) = match location {
            Location::File { path } => {
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

                (path_buf, PresetLocationOrigin::File(path.to_owned()))
            }
            Location::Plugin => {
                let Some(key) = load_key.filter(|k| !k.to_bytes().is_empty()) else {
                    log::error!("NAM-Plug: Missing or empty load_key for plugin preset location");
                    return Err(PluginError::Message(
                        "Missing or empty load_key for plugin preset location",
                    ));
                };

                let key_str = key
                    .to_str()
                    .map_err(|_| PluginError::Message("Invalid UTF-8 in preset load_key"))?;
                let candidate = PathBuf::from(key_str);
                let path_buf = if candidate.is_relative() {
                    if candidate
                        .components()
                        .any(|c| matches!(c, Component::ParentDir))
                    {
                        log::error!(
                            "NAM-Plug: Rejected relative preset load_key containing parent traversal: {key_str:?}"
                        );
                        return Err(PluginError::Message(
                            "Invalid relative load_key for plugin preset location",
                        ));
                    }
                    match resolve_home_model_dir() {
                        Some(home_dir) => home_dir.join(candidate),
                        None => candidate,
                    }
                } else {
                    candidate
                };

                if !path_buf.exists() || !path_buf.is_file() {
                    log::error!(
                        "NAM-Plug: Preset model file does not exist or is not a regular file: {path_buf:?}"
                    );
                    return Err(PluginError::Message(
                        "Model file does not exist or is not a regular file",
                    ));
                }

                if !is_supported_model_extension(&path_buf) {
                    log::error!(
                        "NAM-Plug: Unsupported model extension in preset load_key: {path_buf:?}"
                    );
                    return Err(PluginError::Message(
                        "Unsupported preset model file extension",
                    ));
                }

                (path_buf, PresetLocationOrigin::Plugin)
            }
        };

        let preset_name = path_buf
            .file_stem()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();
        let path_display = path_buf.display().to_string();

        let origin = ModelRequestOrigin::Preset {
            location: location_origin,
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

        // F-UX-06: `ui_loading` is raised only while `housekeeping()` is inside
        // `load_model`, not at enqueue time.
        self.host.shared().request_callback();

        log::info!("Loading preset \"{preset_name}\" from {path_display:?}");

        Ok(())
    }
}
