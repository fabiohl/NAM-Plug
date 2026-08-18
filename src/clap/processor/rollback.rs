// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! RAII rollback guard for `activate()` resource extraction.
//! CLAP-F026: If any allocation stage in `activate()` fails
//! (resampler, ConvEngine, oversampling engines, buffer pre-allocation),
//! the guard restores ownership of SPSC channel ends and `DeactivatedDspState`
//! back into `ColdShared`, leaving the plugin in a clean deactivated state
//! ready for the next `activate()` attempt.

use crate::clap::plugin::{ClapParamPayload, NamClapShared};
use clack_plugin::plugin::PluginError;
use neural_amp_modeler_rs::common::spsc::GcItem;
use neural_amp_modeler_rs::models::StaticModel;
use rtrb::{Consumer, Producer};

use super::deactivated::DeactivatedDspState;

/// RAII guard that restores extracted shared resources on drop.
///
/// Must be created immediately after the first SPSC channel extraction
/// in `activate()`. Each subsequent resource extraction is stored in the
/// guard. On early return (`?`), Drop fires and puts everything back into
/// `ColdShared`. On success, `defuse()` takes ownership of the resources
/// (transferring them into `NamClapProcessor`) and disarms the destructor.
pub(crate) struct ActivateRollbackGuard<'a> {
    shared: &'a NamClapShared,
    pub(crate) param_rx: Option<Consumer<ClapParamPayload>>,
    pub(crate) gc_tx: Option<Producer<GcItem>>,
    pub(crate) slimmable_rx: Option<Consumer<Option<Box<StaticModel>>>>,
    pub(crate) deactivated: Option<DeactivatedDspState>,
}

/// Resources extracted from `ColdShared` during `activate()`.
/// Returned by `ActivateRollbackGuard::defuse()` on success.
pub(crate) struct ActivatedResources {
    pub(crate) param_rx: Consumer<ClapParamPayload>,
    pub(crate) gc_tx: Producer<GcItem>,
    pub(crate) slimmable_rx: Consumer<Option<Box<StaticModel>>>,
}

impl<'a> ActivateRollbackGuard<'a> {
    pub(crate) fn new(shared: &'a NamClapShared) -> Self {
        Self {
            shared,
            param_rx: None,
            gc_tx: None,
            slimmable_rx: None,
            deactivated: None,
        }
    }

    /// Consumes the guard, transferring SPSC channel ownership to the caller
    /// (for the `NamClapProcessor` constructor). Dropping the guard after
    /// defuse is a no-op — resources have been moved out.
    pub(crate) fn defuse(mut self) -> Result<ActivatedResources, PluginError> {
        let rx = self
            .param_rx
            .take()
            .ok_or_else(|| PluginError::Message("param_rx must be set before defuse"))?;
        let tx = self
            .gc_tx
            .take()
            .ok_or_else(|| PluginError::Message("gc_tx must be set before defuse"))?;
        let slimmable = self
            .slimmable_rx
            .take()
            .ok_or_else(|| PluginError::Message("slimmable_rx must be set before defuse"))?;
        Ok(ActivatedResources {
            param_rx: rx,
            gc_tx: tx,
            slimmable_rx: slimmable,
        })
    }
}

impl Drop for ActivateRollbackGuard<'_> {
    fn drop(&mut self) {
        // Restore SPSC channels and DeactivatedDspState in reverse
        // extraction order. Each `.take()` moves the resource out of the
        // guard so the Drop is idempotent (restore-once). Mutex poisoning
        // is recovered via `into_inner()` — the resource must be restored
        // even if a previous lock attempt panicked.
        if let Some(rx) = self.slimmable_rx.take() {
            *self
                .shared
                .cold
                .slimmable_rx
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(rx);
        }
        if let Some(tx) = self.gc_tx.take() {
            *self
                .shared
                .cold
                .gc_tx
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(tx);
        }
        if let Some(rx) = self.param_rx.take() {
            *self
                .shared
                .cold
                .param_rx
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(rx);
        }
        if let Some(state) = self.deactivated.take() {
            *self
                .shared
                .cold
                .deactivated_dsp
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clap::plugin::make_test_shared;

    #[test]
    fn test_defuse_success() {
        let shared = make_test_shared();
        let mut guard = ActivateRollbackGuard::new(&shared);
        let (_param_tx, param_rx) = rtrb::RingBuffer::new(8);
        let (gc_tx, _gc_rx) = rtrb::RingBuffer::new(8);
        let (_slimmable_tx, slimmable_rx) = rtrb::RingBuffer::new(8);

        guard.param_rx = Some(param_rx);
        guard.gc_tx = Some(gc_tx);
        guard.slimmable_rx = Some(slimmable_rx);

        let resources = guard.defuse();
        assert!(resources.is_ok());
    }

    #[test]
    fn test_defuse_missing_resource_returns_error() {
        let shared = make_test_shared();
        let guard = ActivateRollbackGuard::new(&shared);
        let result = guard.defuse();
        assert!(result.is_err());
    }
}
