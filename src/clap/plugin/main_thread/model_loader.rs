// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Per-instance asynchronous model loader worker (F-UX-01) and
//! adaptive-rebuild offload (F-UX-02).
//!
//! The heavy, side-effect-free half of a model load
//! ([`NmClapMainThread::prepare_model`](super::NamClapMainThread::prepare_model))
//! runs on a dedicated **non-RT** thread (normal scheduling priority, never
//! `SCHED_FIFO`) so the CLAP main thread — and therefore `housekeeping()` — is
//! never blocked by file I/O, hashing or model construction. The short commit
//! ([`commit_prepared_model`](super::NamClapMainThread::commit_prepared_model))
//! stays on the main thread.
//!
//! # Adaptive rebuild offload (F-UX-02)
//!
//! The same worker also builds the two allocation-heavy structural refreshes
//! requested by the audio thread — the WaveNet *slimmable* slice (`slice` +
//! `prewarm` + `set_max_buffer_size`) and the *oversampling* engine pair — so
//! the latency of a flag→swap response no longer depends on the 250 ms
//! housekeeping watchdog. The worker polls the `RT_STATUS_NEEDS_*` flags with a
//! short `park_timeout` window, builds the payload off the main thread, and
//! publishes it into a per-kind slot; it then issues a host callback so the main
//! thread runs one `housekeeping()` tick that pushes the payload onto the SPSC
//! and clears the flag.
//!
//! The SPSC *push* and the *flag clear* stay on the main thread (the producers
//! are main-thread owned), so the existing fail-closed retry logic is preserved
//! verbatim: a full ring retains the flag and the built payload and retries on
//! the next tick. The worker never clears a flag and never rebuilds while a
//! payload it already published is still waiting to be consumed.
//!
//! # Lifetime protocol
//!
//! - [`ModelLoader::spawn`] starts the `nam-model-loader` thread lazily on the
//!   first asynchronous load (or the first adaptive rebuild request).
//! - [`ModelLoader::dispatch`] queues one [`ModelLoaderJob`]; the worker
//!   publishes a [`PreparedModelResult`] into the shared slot and calls
//!   `HostSharedHandle::request_callback()` so the main thread polls it on the
//!   next `housekeeping()` tick.
//! - [`ModelLoader::shutdown`] drops the job channel and returns the join
//!   handle for a bounded join / reaper fallback (same protocol as
//!   `GuiWorker::shutdown`).
//!
//! The worker owns an `Arc<GuiSharedState>`, so no raw pointer can dangle: the
//! state outlives every load even if the instance is destroyed mid-`prepare`.
//! It is also fence-gated — once `alive_fence` is lowered no result is
//! published and no host callback is issued.

use super::load::PreparedModel;
use crate::clap::plugin::GuiSharedState;
use crate::clap::plugin::shared::{ModelRequestOrigin, SlimmableRebuild};
use clack_plugin::host::HostSharedHandle;
use neural_amp_modeler_rs::common::diagnostics::{NamDiagnostic, SystemSnapshot};
use neural_amp_modeler_rs::common::spsc::{
    RT_STATUS_NEEDS_OS_REBUILD, RT_STATUS_NEEDS_SLIMMABLE_REBUILD,
};
use neural_amp_modeler_rs::dsp::oversample::{OversampleEngine, OversampleFactor};
use neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF;
use neural_amp_modeler_rs::models::slimmable::slice_wavenet_model;
use neural_amp_modeler_rs::models::{NamModel, StaticModel};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Poll window while the worker is idle and the processor is in processing state (10 ms):
/// armed in `start_processing()` and `apply_rt_thread_setup()`, preserving the flag→swap latency (< 60 ms).
pub(crate) const REBUILD_POLL_INTERVAL_ACTIVE: Duration = Duration::from_millis(10);

/// Poll window while the worker is idle and the plugin is NOT processing audio (250 ms):
/// disarmed in `stop_processing()`, reduces background thread wakeups by 25x while idle/deactivated (F-RA-06).
pub(crate) const REBUILD_POLL_INTERVAL_IDLE: Duration = Duration::from_millis(250);

/// One model load dispatched to the loader worker.
///
/// Carries everything the pure `prepare_model` phase needs plus the monotonic
/// `generation` the main thread allocated for this request — the tag used to
/// discard a result that a newer request already superseded.
pub(crate) struct ModelLoaderJob {
    /// Validated model file path.
    pub(crate) path: PathBuf,
    /// Request origin (GUI vs. CLAP preset load) — drives the preset ack.
    pub(crate) origin: ModelRequestOrigin,
    /// Monotonic generation allocated for this request by the main thread.
    pub(crate) generation: u64,
    /// Host sample rate captured at dispatch time.
    pub(crate) host_rate: u32,
    /// Host block size captured at dispatch time (0 = pre-activation).
    pub(crate) buffer_size: usize,
}

/// Result of an asynchronous load, published by the worker and consumed by the
/// main thread.
pub(crate) struct PreparedModelResult {
    /// Generation echoed from the [`ModelLoaderJob`].
    pub(crate) generation: u64,
    /// Request origin, propagated to the preset ack.
    pub(crate) origin: ModelRequestOrigin,
    /// Pure prepare outcome (still uncommitted).
    pub(crate) outcome: Result<PreparedModel, Box<NamDiagnostic>>,
}

/// Shared delivery slot for [`PreparedModelResult`]s.
///
/// The mutex is only ever touched off-RT (worker publishes, main thread
/// consumes) — the audio thread never sees it.
pub(crate) type ModelResultSlot = Arc<Mutex<Option<PreparedModelResult>>>;

/// Outcome of an off-thread WaveNet slimmable rebuild.
pub(crate) enum SlimmableRebuildOutcome {
    /// Built and ready to be pushed onto the slimmable SPSC.
    Ready(Box<SlimmableRebuild>),
    /// `slice_wavenet_model` failed (`RT_STATUS_SLIMMABLE_SLICE_FAILED`).
    Failed,
    /// Nothing to rebuild (target `< 4` channels, or no WaveNet installed):
    /// the request flag is simply cleared.
    Skipped,
}

/// Outcome of an off-thread oversampling engine rebuild.
pub(crate) enum OversampleRebuildOutcome {
    /// Built and ready to be pushed onto the command ring.
    Ready(Box<OversampleEngine>, Box<OversampleEngine>),
    /// `OversampleEngine::new` failed (OOM): the request flag is cleared and a
    /// diagnostic is emitted by the main thread.
    Failed,
}

/// Per-kind adaptive-rebuild delivery slots shared between the worker and the
/// main thread. Off-RT only; the audio thread never touches them.
#[derive(Clone)]
pub(crate) struct RebuildSlots {
    /// WaveNet slimmable rebuild payload.
    pub(crate) slimmable: Arc<Mutex<Option<SlimmableRebuildOutcome>>>,
    /// Oversampling engine rebuild payload.
    pub(crate) oversample: Arc<Mutex<Option<OversampleRebuildOutcome>>>,
}

impl RebuildSlots {
    /// Creates the empty rebuild delivery slots.
    pub(crate) fn new() -> Self {
        Self {
            slimmable: Arc::new(Mutex::new(None)),
            oversample: Arc::new(Mutex::new(None)),
        }
    }
}

/// Creates the empty delivery slot shared between the main thread and the worker.
pub(crate) fn new_result_slot() -> ModelResultSlot {
    Arc::new(Mutex::new(None))
}

/// Persistent per-instance asynchronous model loader.
pub(crate) struct ModelLoader {
    tx: mpsc::Sender<ModelLoaderJob>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ModelLoader {
    /// Spawns the `nam-model-loader` thread.
    ///
    /// `shared` keeps the plugin state alive for the worker's lifetime and
    /// provides the `alive_fence`; `host_static` is used only to request the
    /// main-thread callback that drains the result slots. `rebuilds` enables the
    /// adaptive-rebuild polling (`None` disables it for the synchronous rollback
    /// mode, where the main thread keeps the legacy inline path).
    pub(crate) fn spawn(
        instance_id: u64,
        sys: SystemSnapshot,
        shared: Arc<GuiSharedState>,
        host_static: HostSharedHandle<'static>,
        slot: ModelResultSlot,
        rebuilds: Option<RebuildSlots>,
    ) -> Result<Self, &'static str> {
        let (tx, rx) = mpsc::channel::<ModelLoaderJob>();
        let handle = std::thread::Builder::new()
            .name("nam-model-loader".to_string())
            .spawn(move || worker_main(rx, instance_id, sys, shared, host_static, slot, rebuilds))
            .map_err(|_| "failed to spawn the model loader worker thread")?;
        Ok(Self {
            tx,
            handle: Some(handle),
        })
    }

    /// Queues one job. On failure the job is returned so the caller can fall
    /// back to the synchronous path (fail-closed).
    pub(crate) fn dispatch(&self, job: ModelLoaderJob) -> Result<(), ModelLoaderJob> {
        self.tx.send(job).map_err(|e| e.0)
    }

    /// Stops the worker: the channel is closed so the thread exits as soon as
    /// its current work ends. Returns the join handle for a bounded join.
    pub(crate) fn shutdown(mut self) -> Option<std::thread::JoinHandle<()>> {
        drop(self.tx);
        self.handle.take()
    }
}

/// Builds a WaveNet slimmable rebuild payload off-thread.
///
/// Mirrors the historical main-thread `handle_slimmable_rebuild` body exactly:
/// `slice` → `prewarm` → `set_max_buffer_size`, boxed end-to-end. Returns
/// [`SlimmableRebuildOutcome::Skipped`] for the degenerate cases the inline path
/// resolves by merely clearing the flag.
fn build_slimmable(
    shared: &GuiSharedState,
    target_ch: u32,
    generation: u64,
) -> SlimmableRebuildOutcome {
    if target_ch < 4 {
        return SlimmableRebuildOutcome::Skipped;
    }
    let buffer_size = shared.cold.buffer_size.load(Ordering::Relaxed) as usize;
    let storage = shared
        .cold
        .full_wavenet_model
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(model) = storage.as_ref() else {
        return SlimmableRebuildOutcome::Skipped;
    };
    let StaticModel::WavenetDyn(w) = model.as_ref() else {
        return SlimmableRebuildOutcome::Skipped;
    };
    match slice_wavenet_model(w, target_ch as usize) {
        Ok(mut slimmed) => {
            slimmed.prewarm();
            if buffer_size > 0 {
                let _ = slimmed.set_max_buffer_size(buffer_size);
            }
            SlimmableRebuildOutcome::Ready(Box::new(SlimmableRebuild {
                generation,
                model: Box::new(StaticModel::WavenetDyn(Box::new(slimmed))),
            }))
        }
        Err(_) => SlimmableRebuildOutcome::Failed,
    }
}

/// Builds the two oversampling engines off-thread.
fn build_oversample(factor_val: u32) -> OversampleRebuildOutcome {
    let factor = OversampleFactor::from_f32(factor_val as f32);
    match (
        OversampleEngine::new(factor, MAX_RESAMP_BUF),
        OversampleEngine::new(factor, MAX_RESAMP_BUF),
    ) {
        (Ok(l), Ok(r)) => OversampleRebuildOutcome::Ready(Box::new(l), Box::new(r)),
        _ => OversampleRebuildOutcome::Failed,
    }
}

/// Polls the `RT_STATUS_NEEDS_*` flags, building each pending payload at most
/// once until the corresponding flag is lowered by the main thread.
///
/// `slimmable_built`/`os_built` remember the request identity already published
/// (channel+generation / factor), so a payload still awaiting consumption is not
/// rebuilt every poll; a *distinct* request supersedes it immediately. Once the
/// flag is observed clear, the memory is reset so the next request rebuilds.
fn poll_rebuilds(
    shared: &GuiSharedState,
    host_static: &HostSharedHandle<'static>,
    rebuilds: &RebuildSlots,
    slimmable_built: &mut Option<(u32, u64)>,
    os_built: &mut Option<u32>,
) {
    let rt = &shared.cold.rt_status;

    if rt.check_flag_acquire(RT_STATUS_NEEDS_SLIMMABLE_REBUILD) {
        let target_ch = rt.requested_slimmable_ch.load(Ordering::Relaxed);
        let generation = shared
            .cold
            .requested_slimmable_generation
            .load(Ordering::Relaxed);
        let identity = (target_ch, generation);
        if *slimmable_built != Some(identity) {
            let outcome = build_slimmable(shared, target_ch, generation);
            *rebuilds.slimmable.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
            *slimmable_built = Some(identity);
            if shared.cold.alive_fence.load(Ordering::Acquire) {
                host_static.request_callback();
            }
        }
    } else {
        *slimmable_built = None;
    }

    if rt.check_flag_acquire(RT_STATUS_NEEDS_OS_REBUILD) {
        let factor = rt.requested_os_factor.load(Ordering::Relaxed);
        if *os_built != Some(factor) {
            let outcome = build_oversample(factor);
            *rebuilds
                .oversample
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(outcome);
            *os_built = Some(factor);
            if shared.cold.alive_fence.load(Ordering::Acquire) {
                host_static.request_callback();
            }
        }
    } else {
        *os_built = None;
    }
}

/// The worker loop: one serial `prepare_model` at a time, plus adaptive-rebuild
/// polling between jobs.
fn worker_main(
    rx: mpsc::Receiver<ModelLoaderJob>,
    instance_id: u64,
    sys: SystemSnapshot,
    shared: Arc<GuiSharedState>,
    host_static: HostSharedHandle<'static>,
    slot: ModelResultSlot,
    rebuilds: Option<RebuildSlots>,
) {
    let _scope = neural_amp_modeler_rs::common::diagnostics::scope_instance(instance_id);
    let mut slimmable_built: Option<(u32, u64)> = None;
    let mut os_built: Option<u32> = None;

    loop {
        let is_processing = shared.cold.is_processing.load(Ordering::Relaxed);
        let poll_interval = if is_processing {
            REBUILD_POLL_INTERVAL_ACTIVE
        } else {
            REBUILD_POLL_INTERVAL_IDLE
        };

        // Park for an adaptive window so the rebuild flags are polled even when
        // no load job arrives; `Disconnected` (channel dropped) ends the worker.
        let recv_res = rx.recv_timeout(poll_interval);
        shared
            .cold
            .loader_worker_wakeups
            .fetch_add(1, Ordering::Relaxed);

        match recv_res {
            Ok(job) => {
                // Fence down: the instance is being destroyed; abandon the queue.
                if !shared.cold.alive_fence.load(Ordering::Acquire) {
                    break;
                }

                let outcome = super::load::prepare_model_offthread(
                    &job.path,
                    job.host_rate,
                    job.buffer_size,
                    &sys,
                );

                {
                    let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
                    *guard = Some(PreparedModelResult {
                        generation: job.generation,
                        origin: job.origin,
                        outcome,
                    });
                }

                // Publish the wake-up only while the instance is still alive;
                // after the fence drops, dispatching into a freed instance is
                // forbidden.
                if shared.cold.alive_fence.load(Ordering::Acquire) {
                    host_static.request_callback();
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        if !shared.cold.alive_fence.load(Ordering::Acquire) {
            break;
        }

        if let Some(rebuilds) = &rebuilds {
            poll_rebuilds(
                &shared,
                &host_static,
                rebuilds,
                &mut slimmable_built,
                &mut os_built,
            );
        }
    }
    log::debug!("NAM-Plug: model loader worker exiting");
}
