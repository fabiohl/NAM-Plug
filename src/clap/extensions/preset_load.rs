// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! CLAP `clap.preset-load` extension implementation.
//!
//! Enables hosts to load NAM model files as presets via the host's preset browser.
//!
//! S4-E4-T05: The extension now stores the `location` and `load_key` for deferred
//! notification. The actual model load is asynchronous (queued via `ui_pending_model`
//! and `host.request_callback()`). When `housekeeping()` completes the load, it
//! calls `HostPresetLoad::loaded()` on success or `HostPresetLoad::on_error()` on
//! failure, satisfying the CLAP spec contract.

use crate::clap::plugin::shared::PendingPresetLoad;
use clack_extensions::preset_discovery::prelude::*;
use clack_plugin::prelude::*;
use std::ffi::{CStr, CString};
use std::path::PathBuf;

use crate::clap::plugin::NamClapMainThread;
use crate::clap::plugin::debug_assert_main_thread;

impl PluginPresetLoadImpl for NamClapMainThread<'_> {
    fn load_from_location(
        &mut self,
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

        let preset_name = path_buf
            .file_stem()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        // S4-E4-T05: Store the location and load_key for deferred host
        // notification when the async load completes (housekeeping.rs).
        {
            let mut pending_guard =
                self.shared
                    .cold
                    .pending_preset_load
                    .lock()
                    .unwrap_or_else(|e| {
                        log::error!("PoisonError in pending_preset_load lock: {e:?}");
                        e.into_inner()
                    });
            *pending_guard = Some(PendingPresetLoad {
                location_path: CString::new(path.to_bytes())
                    .unwrap_or_else(|_| CString::new("").unwrap()),
                load_key: load_key.map(|k| {
                    CString::new(k.to_bytes()).unwrap_or_else(|_| CString::new("").unwrap())
                }),
            });
        }

        // Enqueue the model for loading via the existing pipeline.
        let mut pending_guard = self
            .shared
            .cold
            .ui_pending_model
            .lock()
            .unwrap_or_else(|e| {
                log::error!("PoisonError in ui_pending_model lock: {e:?}");
                e.into_inner()
            });
        *pending_guard = Some(path_buf);
        self.shared
            .cold
            .ui_loading
            .store(true, std::sync::atomic::Ordering::Relaxed);

        self.host.shared().request_callback();

        log::info!("Loading preset \"{preset_name}\" from {path_str:?}");

        Ok(())
    }
}
