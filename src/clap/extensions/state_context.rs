// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Implementation of the CLAP state-context extension.
//!
//! Allows the plugin to distinguish between saving state for a preset
//! (portable, without absolute paths) vs for a project or duplication
//! (full state, with absolute model paths).
//!
//! See: `clap/ext/state-context.h`

use crate::clap::extensions::state_transaction::{self, RestoreMode};
use crate::clap::plugin::NamClapMainThread;
use crate::clap::plugin::debug_assert_main_thread;
use clack_common::stream::{InputStream, OutputStream};
use clack_extensions::state_context::{
    PluginStateContext, PluginStateContextImpl, StateContextType,
};
use clack_plugin::prelude::*;
use std::io::{Read, Write};

/// Type alias for the CLAP state-context extension registration.
pub type NamPluginStateContext = PluginStateContext;

impl<'a> PluginStateContextImpl for NamClapMainThread<'a> {
    fn save(
        &mut self,
        output: &mut OutputStream,
        context_type: StateContextType,
    ) -> Result<(), PluginError> {
        debug_assert_main_thread(&self.host);
        self.snapshot_params();

        let save_params = if context_type == StateContextType::ForPreset {
            let mut preset_params = self.params.clone();
            // S6-E6-T02: strip absolute file paths, keep portable identifiers + search hints
            preset_params.model_path = None;
            preset_params.ir_path = None;
            // model_search_paths are directory hints (not machine-specific file paths)
            // and are preserved for cross-machine canonical search fallback
            preset_params
        } else {
            self.params.clone()
        };

        let serialized = super::state::serialize_envelope(&save_params)?;

        output
            .write_all(&serialized)
            .map_err(|e| PluginError::Error(Box::new(e)))?;

        Ok(())
    }

    fn load(
        &mut self,
        input: &mut InputStream,
        context_type: StateContextType,
    ) -> Result<(), PluginError> {
        debug_assert_main_thread(&self.host);
        let mut buffer = Vec::new();
        input
            .read_to_end(&mut buffer)
            .map_err(|e| PluginError::Error(Box::new(e)))?;

        let mode = if context_type == StateContextType::ForPreset {
            RestoreMode::ForPreset
        } else {
            RestoreMode::Full
        };

        state_transaction::restore_state_transactional(&buffer, self, mode)
    }
}

#[cfg(test)]
#[path = "state_context_test.rs"]
mod tests;
