// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Opt-in asynchronous state-restore loader (F-UX-01).
//!
//! When enabled (`NAM_ASYNC_RESTORE`) on an interactive host that is already
//! active, the expensive validation phase of a state restore — file hashing,
//! model/IR construction, resampler setup (`validate::validate_and_build`) —
//! runs on this dedicated **non-RT** worker instead of the host's `state.load`
//! callback. The atomic commit
//! ([`commit`](crate::clap::extensions::state_transaction::commit::commit)) still
//! runs on the main thread, so the audio thread never sees a partial restore and
//! the previous DSP state stays intact on failure.
//!
//! The default remains **synchronous**: an opted-in asynchronous restore
//! necessarily reports success to the host before validation has run, so it is
//! only enabled deliberately. Pre-activate restores, offline/render contexts,
//! and hosts without the switch keep the historical blocking path.
//!
//! Lifetime protocol mirrors [`ModelLoader`](super::model_loader::ModelLoader):
//! the worker owns an `Arc<GuiSharedState>`, is `alive_fence`-gated, and is shut
//! down with a bounded join during teardown.

use crate::clap::extensions::state_transaction::RestoreMode;
use crate::clap::extensions::state_transaction::validate::ValidatedRestore;
use crate::clap::plugin::GuiSharedState;
use clack_plugin::host::HostSharedHandle;
use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

/// One restore dispatched to the worker.
pub(crate) struct RestoreJob {
    /// Raw state blob to deserialize and validate.
    pub(crate) buffer: Vec<u8>,
    /// Full (project) or preset restore.
    pub(crate) mode: RestoreMode,
    /// Host sample rate captured at dispatch time.
    pub(crate) host_rate: u32,
    /// Host block size captured at dispatch time.
    pub(crate) buffer_size: u32,
}

/// Result of an asynchronous restore validation.
pub(crate) struct RestoreResult {
    /// Mode echoed from the [`RestoreJob`] (the commit needs it).
    pub(crate) mode: RestoreMode,
    /// Validated package ready for the atomic commit, or a rendered error.
    pub(crate) outcome: Result<ValidatedRestore, String>,
}

/// Shared delivery slot for [`RestoreResult`]s (off-RT only).
pub(crate) type RestoreResultSlot = Arc<Mutex<Option<RestoreResult>>>;

/// Creates the empty restore delivery slot.
pub(crate) fn new_result_slot() -> RestoreResultSlot {
    Arc::new(Mutex::new(None))
}

/// Persistent per-instance asynchronous restore loader.
pub(crate) struct RestoreLoader {
    tx: mpsc::Sender<RestoreJob>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl RestoreLoader {
    /// Spawns the `nam-restore-loader` thread.
    pub(crate) fn spawn(
        instance_id: u64,
        sys: SystemSnapshot,
        shared: Arc<GuiSharedState>,
        host_static: HostSharedHandle<'static>,
        slot: RestoreResultSlot,
    ) -> Result<Self, &'static str> {
        let (tx, rx) = mpsc::channel::<RestoreJob>();
        let handle = std::thread::Builder::new()
            .name("nam-restore-loader".to_string())
            .spawn(move || worker_main(rx, instance_id, sys, shared, host_static, slot))
            .map_err(|_| "failed to spawn the restore loader worker thread")?;
        Ok(Self {
            tx,
            handle: Some(handle),
        })
    }

    /// Queues one job; on failure the job is returned for a synchronous fallback.
    pub(crate) fn dispatch(&self, job: RestoreJob) -> Result<(), RestoreJob> {
        self.tx.send(job).map_err(|e| e.0)
    }

    /// Stops the worker: closes the channel and returns the join handle.
    pub(crate) fn shutdown(mut self) -> Option<std::thread::JoinHandle<()>> {
        drop(self.tx);
        self.handle.take()
    }
}

fn worker_main(
    rx: mpsc::Receiver<RestoreJob>,
    instance_id: u64,
    sys: SystemSnapshot,
    shared: Arc<GuiSharedState>,
    host_static: HostSharedHandle<'static>,
    slot: RestoreResultSlot,
) {
    let _scope = neural_amp_modeler_rs::common::diagnostics::scope_instance(instance_id);
    while let Ok(job) = rx.recv() {
        if !shared.cold.alive_fence.load(Ordering::Acquire) {
            break;
        }

        let outcome = build_validated(&job.buffer, job.host_rate, job.buffer_size, &job.mode, &sys);

        {
            let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some(RestoreResult {
                mode: job.mode,
                outcome,
            });
        }

        if shared.cold.alive_fence.load(Ordering::Acquire) {
            host_static.request_callback();
        }
    }
    log::debug!("NAM-Plug: restore loader worker exiting");
}

/// Runs the pure prepare+validate phases off-RT, rendering any error to a
/// `String` so it can cross the thread boundary (`PluginError` is not `Send`).
fn build_validated(
    buffer: &[u8],
    host_rate: u32,
    buffer_size: u32,
    mode: &RestoreMode,
    sys: &SystemSnapshot,
) -> Result<ValidatedRestore, String> {
    let params = crate::clap::extensions::state::load_state(buffer).map_err(|e| format!("{e}"))?;
    crate::clap::extensions::state_transaction::validate::validate_and_build(
        &params,
        host_rate,
        buffer_size,
        sys,
        mode,
    )
    .map_err(|e| format!("{e}"))
}
