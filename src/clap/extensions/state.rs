// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Implementation of the CLAP state extension.
//! Allows the plugin to save and load its current configuration (parameters and model).
//!
//! Payload versioning:
//! - v0 (legacy, CLAP v1.5.x): plain JSON `ProcessingParams`, without a `version` field.
//! - v1 (current): envelope `StateEnvelope { version: 1, params: {...} }`.

use crate::clap::extensions::state_transaction::{self, RestoreMode};
use crate::clap::plugin::NamClapMainThread;
use crate::clap::plugin::debug_assert_main_thread;
use clack_common::stream::{InputStream, OutputStream};
use clack_extensions::state::PluginStateImpl;
use clack_plugin::prelude::*;
use neural_amp_modeler_rs::common::params::ProcessingParams;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

pub(crate) const CURRENT_STATE_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub(crate) enum StateError {
    #[error("Failed to serialize state: {0}")]
    Serialize(#[source] serde_json::Error),

    #[error("Failed to write to state stream: {0}")]
    WriteStream(#[source] std::io::Error),

    #[error("Failed to read from state stream: {0}")]
    ReadStream(#[source] std::io::Error),

    #[error("Failed to deserialize state (corrupted v1+ envelope)")]
    CorruptedEnvelope,

    #[error("Failed to deserialize state (v0 legacy): {0}")]
    Deserialize(#[source] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StateEnvelope {
    pub(crate) version: u32,
    pub(crate) params: ProcessingParams,
}

/// Shared helper: serialises `ProcessingParams` wrapped in a v1 `StateEnvelope`.
pub(crate) fn serialize_envelope(params: &ProcessingParams) -> Result<Vec<u8>, PluginError> {
    let envelope = StateEnvelope {
        version: CURRENT_STATE_VERSION,
        params: params.clone(),
    };
    serde_json::to_vec(&envelope)
        .map_err(|e| PluginError::Error(Box::new(StateError::Serialize(e))))
}

#[expect(
    clippy::single_match,
    reason = "Match kept for exhaustiveness — future extensions expected at this dispatch site"
)]
fn migrate(version: u32, params: ProcessingParams) -> ProcessingParams {
    match version {
        0 => {
            // v0 → v1: common fields copied, new fields use Default
            // (ProcessingParams already has #[serde(default)] on all fields)
        }
        _ => {}
    }
    params
}

impl<'a> PluginStateImpl for NamClapMainThread<'a> {
    fn save(&mut self, output: &mut OutputStream) -> Result<(), PluginError> {
        debug_assert_main_thread(&self.host);
        self.snapshot_params();

        let serialized = serialize_envelope(&self.params)?;
        let blob_len = serialized.len();

        output
            .write_all(&serialized)
            .map_err(|e| PluginError::Error(Box::new(StateError::WriteStream(e))))?;

        log::info!("[State] Save completed: {} bytes serialized.", blob_len);

        Ok(())
    }

    fn load(&mut self, input: &mut InputStream) -> Result<(), PluginError> {
        debug_assert_main_thread(&self.host);
        let mut buffer = Vec::new();
        input
            .read_to_end(&mut buffer)
            .map_err(|e| PluginError::Error(Box::new(StateError::ReadStream(e))))?;

        state_transaction::restore_state_transactional(&buffer, self, RestoreMode::Full)
    }
}

pub(crate) fn load_state(buffer: &[u8]) -> Result<ProcessingParams, PluginError> {
    if let Ok(envelope) = serde_json::from_slice::<StateEnvelope>(buffer) {
        return Ok(migrate(envelope.version, envelope.params));
    }

    // If the buffer is a v1+ envelope (contains a "version" key) that failed to parse,
    // we don't fall back to v0 — we propagate the error as corrupted data
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(buffer)
        && value.get("version").is_some()
    {
        return Err(PluginError::Error(Box::new(StateError::CorruptedEnvelope)));
    }

    // Fallback: v0 legacy — ProcessingParams directly, without a version field
    let params: ProcessingParams = serde_json::from_slice(buffer)
        .map_err(|e| PluginError::Error(Box::new(StateError::Deserialize(e))))?;

    Ok(migrate(0, params))
}

#[cfg(test)]
#[path = "state_test.rs"]
mod state_test;
