// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Centralised transactional pipeline for state and state-context restoration.
//!
//! Implements a 3-phase pipeline (Prepare → Validate → Commit) that guarantees:
//! - Any asset validation failure returns an error **without altering active DSP state**
//! - Successful restores publish all resources atomically
//!
//! # Phases
//! 1. **Prepare** — deserialise the state blob into `ProcessingParams`
//! 2. **Validate** — off-RT validation: check paths, load/build model, IR, resamplers (`validate.rs`)
//! 3. **Commit** — publish params + payloads via atomics and SPSC (only after full validation) (`commit.rs`)

mod commit;
mod validate;

pub(crate) use commit::publish_restore;
pub(crate) use validate::{compute_file_hash, is_valid_sha256_hex};

#[cfg(test)]
pub(crate) use commit::*;
#[cfg(test)]
pub(crate) use validate::*;

use crate::clap::plugin::NamClapMainThread;
use crate::clap::plugin::debug_assert_main_thread;
use crate::clap::plugin::errors::{self};
use clack_plugin::prelude::*;
use std::sync::atomic::Ordering;

/// Restore mode: `Full` for project/duplicate, `ForPreset` for preset bank loads.
pub(crate) enum RestoreMode {
    Full,
    ForPreset,
}

/// Entry-point for the 3-phase transactional state restore.
pub(crate) fn restore_state_transactional(
    buffer: &[u8],
    main_thread: &NamClapMainThread,
    mode: RestoreMode,
) -> Result<(), PluginError> {
    debug_assert_main_thread(&main_thread.host);

    if buffer.is_empty() {
        log::debug!("Empty state buffer, returning error");
        return Err(PluginError::Message(errors::state_txn::EMPTY_BUFFER));
    }

    // ══════ Phase 1: PREPARE — deserialise ══════
    let loaded_params = super::state::load_state(buffer)?;

    let host_rate = {
        let rate = main_thread.shared.cold.sample_rate.load(Ordering::Relaxed);
        if rate == 0 { 48000 } else { rate }
    };
    let buffer_size = main_thread.shared.cold.buffer_size.load(Ordering::Relaxed);

    // ══════ Phase 2: VALIDATE — build resources off-RT ══════
    let validated = validate::validate_and_build(
        &loaded_params,
        host_rate,
        buffer_size,
        &main_thread.sys,
        &mode,
    )?;

    // ══════ Phase 3: COMMIT — publish atomically ══════
    commit::commit(validated, main_thread, &mode)?;

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
#[path = "state_transaction_test.rs"]
mod tests;
