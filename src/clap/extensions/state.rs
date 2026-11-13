// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Implementation of the CLAP state extension.
//! Allows the plugin to save and load its current configuration (parameters and model).
//!
//! Payload versioning:
//! - v0 (legacy, CLAP v1.5.x): plain JSON `ProcessingParams`, without a `version` field
//!   (restored with `calibrate_input = true`, `12.0 dBu`, `OutputMode::Normalized`).
//! - v1: envelope `StateEnvelope { version: 1, params, ir_level_policy }` without calibration
//!   fields (restored with `calibrate_input = true`, `12.0 dBu`, `OutputMode::Normalized`).
//! - v2 (current): envelope `StateEnvelope { version: 2, params, ir_level_policy, calibration }`
//!   with explicit opt-in calibration fields (new-project default `calibrate_input = false`).

use crate::clap::extensions::cabinet::CabAssetKind;
use crate::clap::extensions::calibration::CalibrationConfig;
use crate::clap::extensions::gate_mode::GateMode;
use crate::clap::extensions::ir_level_policy::IrLevelPolicy;
use crate::clap::extensions::params::bypass_u32_to_bool;
use crate::clap::extensions::state_transaction::{self, RestoreMode};
use crate::clap::plugin::NamClapMainThread;
use crate::clap::plugin::debug_assert_main_thread;
use crate::clap::plugin::errors::{self, static_plugin_error};
use clack_common::stream::{InputStream, OutputStream};
use clack_extensions::state::PluginStateImpl;
use clack_plugin::prelude::*;
use neural_amp_modeler_rs::common::params::ProcessingParams;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::sync::atomic::Ordering;

pub(crate) const CURRENT_STATE_VERSION: u32 = 2;
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
    #[serde(default)]
    pub(crate) ir_level_policy: IrLevelPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) calibration: Option<CalibrationConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) gate_mode: Option<GateMode>,
    #[serde(default)]
    pub(crate) cab_bypass: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cab_kind: Option<CabAssetKind>,
}

/// Shared helper: serialises `ProcessingParams` wrapped in a v2 `StateEnvelope`
/// using the default [`IrLevelPolicy`], [`CalibrationConfig`], and [`GateMode`].
#[cfg_attr(not(test), expect(dead_code))]
pub(crate) fn serialize_envelope(params: &ProcessingParams) -> Result<Vec<u8>, PluginError> {
    serialize_envelope_v2_with_gate_mode(
        params,
        IrLevelPolicy::default(),
        CalibrationConfig::default(),
        GateMode::default(),
    )
}

/// Serialises `ProcessingParams`, `ir_level_policy`, and `calibration` wrapped in
/// a v2 `StateEnvelope` using the default [`GateMode`].
#[cfg_attr(not(test), expect(dead_code))]
pub(crate) fn serialize_envelope_v2(
    params: &ProcessingParams,
    ir_level_policy: IrLevelPolicy,
    calibration: CalibrationConfig,
) -> Result<Vec<u8>, PluginError> {
    serialize_envelope_v2_with_gate_mode(params, ir_level_policy, calibration, GateMode::default())
}

/// Serialises `ProcessingParams`, `ir_level_policy`, `calibration`, and `gate_mode`
/// wrapped in a v2 `StateEnvelope`.
pub(crate) fn serialize_envelope_v2_with_gate_mode(
    params: &ProcessingParams,
    ir_level_policy: IrLevelPolicy,
    calibration: CalibrationConfig,
    gate_mode: GateMode,
) -> Result<Vec<u8>, PluginError> {
    serialize_envelope_v2_with_cab(params, ir_level_policy, calibration, gate_mode, false, None)
}

/// Serialises complete v2 `StateEnvelope` including independent cabinet bypass & format hint.
pub(crate) fn serialize_envelope_v2_with_cab(
    params: &ProcessingParams,
    ir_level_policy: IrLevelPolicy,
    calibration: CalibrationConfig,
    gate_mode: GateMode,
    cab_bypass: bool,
    cab_kind: Option<CabAssetKind>,
) -> Result<Vec<u8>, PluginError> {
    let envelope = StateEnvelope {
        version: CURRENT_STATE_VERSION,
        params: params.clone(),
        ir_level_policy,
        calibration: Some(calibration.sanitized()),
        gate_mode: Some(gate_mode),
        cab_bypass,
        cab_kind,
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
            return Err(static_plugin_error(
                errors::state_txn::MODEL_HASH_MISSING,
                format_args!("Refusing to persist Full state — model {path:?} has no SHA-256 hash"),
            ));
        }
        if let Some(ref path) = params.ir_path
            && params.ir_hash.is_none()
        {
            return Err(static_plugin_error(
                errors::state_txn::IR_HASH_MISSING,
                format_args!("Refusing to persist Full state — IR {path:?} has no SHA-256 hash"),
            ));
        }
    }
    if let Some(ref basename) = params.model_basename
        && params.model_hash.is_none()
    {
        return Err(static_plugin_error(
            errors::state_txn::MODEL_HASH_MISSING,
            format_args!(
                "Refusing to persist state — model reference {basename:?} has no SHA-256 hash"
            ),
        ));
    }
    if let Some(ref hash) = params.model_hash
        && !state_transaction::is_valid_sha256_hex(hash)
    {
        return Err(static_plugin_error(
            errors::state_txn::MODEL_HASH_MALFORMED,
            format_args!(
                "Refusing to persist state — malformed model_hash ({} chars): {hash:?}",
                hash.len()
            ),
        ));
    }
    if let Some(ref hash) = params.ir_hash
        && !state_transaction::is_valid_sha256_hex(hash)
    {
        return Err(static_plugin_error(
            errors::state_txn::IR_HASH_MALFORMED,
            format_args!(
                "Refusing to persist state — malformed ir_hash ({} chars): {hash:?}",
                hash.len()
            ),
        ));
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
    fn save(&self, output: &mut OutputStream) -> Result<(), PluginError> {
        debug_assert_main_thread(&self.host);
        self.snapshot_params();
        ensure_asset_hashes(&self.params.borrow(), false)?;

        let ir_policy =
            IrLevelPolicy::from_u8(self.shared.cold.ir_level_policy.load(Ordering::Relaxed));
        let calibration = self.shared.calibration_config();
        let gate_mode = self.shared.gate_mode();
        let cab_bypass = bypass_u32_to_bool(
            self.shared
                .ui_to_rt
                .param_cab_bypass
                .load(Ordering::Relaxed),
        );
        let cab_kind = CabAssetKind::from_u8(self.shared.cold.cab_kind.load(Ordering::Relaxed));
        let serialized = serialize_envelope_v2_with_cab(
            &self.params.borrow(),
            ir_policy,
            calibration,
            gate_mode,
            cab_bypass,
            cab_kind,
        )?;
        let blob_len = serialized.len();

        output
            .write_all(&serialized)
            .map_err(|e| PluginError::Error(Box::new(StateError::WriteStream(e))))?;

        log::info!("[State] Save completed: {} bytes serialized.", blob_len);

        Ok(())
    }

    fn load(&self, input: &mut InputStream) -> Result<(), PluginError> {
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

#[cfg_attr(not(test), expect(dead_code))]
pub(crate) fn load_state(buffer: &[u8]) -> Result<ProcessingParams, PluginError> {
    let (params, _policy, _calibration) = load_state_with_policy_and_calibration(buffer)?;
    Ok(params)
}

#[cfg_attr(not(test), expect(dead_code))]
pub(crate) fn load_state_with_policy_and_calibration(
    buffer: &[u8],
) -> Result<(ProcessingParams, IrLevelPolicy, CalibrationConfig), PluginError> {
    let (params, policy, calibration, _gate_mode) =
        load_state_with_policy_calibration_and_gate_mode(buffer)?;
    Ok((params, policy, calibration))
}

#[cfg_attr(not(test), expect(dead_code))]
pub(crate) fn load_state_with_policy_calibration_and_gate_mode(
    buffer: &[u8],
) -> Result<(ProcessingParams, IrLevelPolicy, CalibrationConfig, GateMode), PluginError> {
    let (params, policy, calibration, gate_mode, _cab_bypass, _cab_kind) =
        load_state_with_full_metadata(buffer)?;
    Ok((params, policy, calibration, gate_mode))
}

/// Unpacked metadata tuple returned by [`load_state_with_full_metadata`].
pub(crate) type LoadedFullMetadata = (
    ProcessingParams,
    IrLevelPolicy,
    CalibrationConfig,
    GateMode,
    bool,
    Option<CabAssetKind>,
);

pub(crate) fn load_state_with_full_metadata(
    buffer: &[u8],
) -> Result<LoadedFullMetadata, PluginError> {
    if let Ok(envelope) = serde_json::from_slice::<StateEnvelope>(buffer) {
        let calibration = match envelope.calibration {
            Some(cal) => {
                cal.validate()
                    .map_err(|_| PluginError::Error(Box::new(StateError::CorruptedEnvelope)))?;
                cal
            }
            None if envelope.version <= 1 => CalibrationConfig::legacy_restored_default(),
            None => CalibrationConfig::new_project_default(),
        };
        let gate_mode = envelope.gate_mode.unwrap_or(GateMode::Silence);
        return Ok((
            migrate(envelope.version, envelope.params),
            envelope.ir_level_policy,
            calibration,
            gate_mode,
            envelope.cab_bypass,
            envelope.cab_kind,
        ));
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

    Ok((
        migrate(0, params),
        IrLevelPolicy::PeakNormalized,
        CalibrationConfig::legacy_restored_default(),
        GateMode::Silence,
        false,
        None,
    ))
}

#[cfg(test)]
#[path = "state_test.rs"]
mod state_test;
