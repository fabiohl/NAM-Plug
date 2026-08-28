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
/// Maximum allowed size for a CLAP state stream (32 MiB).
pub(crate) const MAX_STATE_STREAM_SIZE: usize = 32 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub(crate) enum StateError {
    #[error("Failed to serialize state: {0}")]
    Serialize(#[source] serde_json::Error),

    #[error("Failed to write to state stream: {0}")]
    WriteStream(#[source] std::io::Error),

    #[error("Failed to read from state stream: {0}")]
    ReadStream(#[source] std::io::Error),

    #[error("State stream exceeds maximum allowed size ({size} bytes > {limit} bytes)")]
    StreamTooLarge { size: usize, limit: usize },

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

/// Mandatory asset identity: no asset reference may be persisted without a valid SHA-256 digest.
/// Defensive fail-closed guard on the save path — every adoption path already
/// computes the digest, so a violation here means a programming error. The only
/// migration path for legacy hashless state is an explicit GUI re-load.
pub(crate) fn ensure_asset_hashes(
    params: &ProcessingParams,
    portable: bool,
) -> Result<(), PluginError> {
    if !portable {
        if let Some(ref path) = params.model_path
            && params.model_hash.is_none()
        {
            log::error!(
                "NAM-Plug: Refusing to persist Full state — model {path:?} has no SHA-256 hash"
            );
            return Err(PluginError::Message(Box::leak(
                "Cannot persist state: model has no SHA-256 hash (re-load it via the GUI to migrate)"
                    .to_string()
                    .into_boxed_str(),
            )));
        }
        if let Some(ref path) = params.ir_path
            && params.ir_hash.is_none()
        {
            log::error!(
                "NAM-Plug: Refusing to persist Full state — IR {path:?} has no SHA-256 hash"
            );
            return Err(PluginError::Message(Box::leak(
                "Cannot persist state: IR has no SHA-256 hash (re-load it via the GUI to migrate)"
                    .to_string()
                    .into_boxed_str(),
            )));
        }
    }
    if let Some(ref basename) = params.model_basename
        && params.model_hash.is_none()
    {
        log::error!(
            "NAM-Plug: Refusing to persist state — model reference {basename:?} has no SHA-256 hash"
        );
        return Err(PluginError::Message(Box::leak(
            "Cannot persist state: model has no SHA-256 hash (re-load it via the GUI to migrate)"
                .to_string()
                .into_boxed_str(),
        )));
    }
    if let Some(ref hash) = params.model_hash
        && !state_transaction::is_valid_sha256_hex(hash)
    {
        log::error!(
            "NAM-Plug: Refusing to persist state — malformed model_hash ({} chars): {hash:?}",
            hash.len()
        );
        return Err(PluginError::Message(Box::leak(
            format!(
                "Cannot persist state: malformed model hash ({} chars, expected 64 hex)",
                hash.len()
            )
            .into_boxed_str(),
        )));
    }
    if let Some(ref hash) = params.ir_hash
        && !state_transaction::is_valid_sha256_hex(hash)
    {
        log::error!(
            "NAM-Plug: Refusing to persist state — malformed ir_hash ({} chars): {hash:?}",
            hash.len()
        );
        return Err(PluginError::Message(Box::leak(
            format!(
                "Cannot persist state: malformed IR hash ({} chars, expected 64 hex)",
                hash.len()
            )
            .into_boxed_str(),
        )));
    }
    Ok(())
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
        ensure_asset_hashes(&self.params, false)?;

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
            .take((MAX_STATE_STREAM_SIZE + 1) as u64)
            .read_to_end(&mut buffer)
            .map_err(|e| PluginError::Error(Box::new(StateError::ReadStream(e))))?;

        if buffer.len() > MAX_STATE_STREAM_SIZE {
            log::error!(
                "[State] State stream exceeds maximum size limit of {} bytes (read {} bytes)",
                MAX_STATE_STREAM_SIZE,
                buffer.len()
            );
            return Err(PluginError::Error(Box::new(StateError::StreamTooLarge {
                size: buffer.len(),
                limit: MAX_STATE_STREAM_SIZE,
            })));
        }

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
