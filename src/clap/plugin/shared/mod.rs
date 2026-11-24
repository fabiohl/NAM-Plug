// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Lock-free shared state between the audio thread and the main thread.

mod gui_events;
mod payloads;
mod state_blocks;

pub(crate) use gui_events::ui_monotonic_ms;
pub use gui_events::{
    GESTURE_BEGIN_SHIFT, GESTURE_BITS_PER_PARAM, GESTURE_CHANGED_SHIFT, GESTURE_END_SHIFT,
    UI_NO_INTERACTION,
};
pub use payloads::{
    ClapParamPayload, LoadModelPayload, MAX_PENDING_MODEL_REQUESTS, ModelRequestOrigin,
    NamModelMetadata, PendingModel, PendingModelRequest, PendingRestore, PresetLocationOrigin,
    RecoveredCommands, RestoreModelPublish, RestorePublish, RestoreTxn, SlimmableRebuild,
    StagedRestore, StructuralKind,
};
pub(crate) use payloads::{PendingRestartOs, StagedSwap, preinject_rt_status};
pub use state_blocks::{
    CONFIG_ID_MONO, CONFIG_ID_STEREO, ColdShared, RENDER_MODE_OFFLINE, RENDER_MODE_REALTIME,
    RtToUi, UI_ALERT_CABSIM_CONTRACT, UI_ALERT_DEGRADE_MINIMAL, UI_ALERT_DEGRADE_REDUCED,
    UI_ALERT_HOST_CONTRACT, UI_ALERT_NON_FINITE_INPUT, UI_ALERT_POISONED,
    UI_ALERT_RESAMPLER_FAILED, UiToRt,
};

use crate::clap::plugin::lock::lock_cold;
use clack_plugin::prelude::*;
use neural_amp_modeler_rs::common::diagnostics::NamErrorCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Active plugin instance counter.
/// Incremented in `new_shared()`, decremented in `NamClapShared::drop()`.
/// `set_shutdown_in_progress()` is only called when this reaches zero,
/// so crash-reporting remains active as long as at least one instance exists.
static ACTIVE_INSTANCES: AtomicU32 = AtomicU32::new(0);

/// Monotonic instance identifier generator for per-instance logging and diagnostics isolation.
static NEXT_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

/// Increments the active instance counter.
#[inline]
pub(crate) fn bump_active_instances() {
    let prev = ACTIVE_INSTANCES.fetch_add(1, Ordering::Release);
    if prev == 0 {
        neural_amp_modeler_rs::common::panic_hook::clear_shutdown_in_progress();
    }
}

/// Allocates a unique monotonic instance ID.
#[inline]
pub(crate) fn next_instance_id() -> u64 {
    NEXT_INSTANCE_ID.fetch_add(1, Ordering::Relaxed)
}

/// Builds the strict-cardinality streaming resample adapter
/// on the main thread, sized for the worst-case host block.
#[inline]
pub(crate) fn build_stream_adapter(
    host_rate: u32,
    model_rate: u32,
    max_block: usize,
) -> Result<Box<neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer>, NamErrorCode> {
    neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer::new(
        host_rate,
        model_rate,
        max_block.max(1),
    )
    .map(Box::new)
}

/// Lock-free shared state between the audio thread, main thread, and GUI window.
///
/// Fields are segregated into cache-line-isolated sub-structs grouped by
/// access pattern to eliminate False Sharing.
///
/// - `rt_to_ui`: written every block by RT, read by UI.
/// - `ui_to_rt`: written by UI/Main, read every block by RT.
/// - `cold`: low-frequency access by both threads.
pub struct GuiSharedState {
    /// Cache-line-isolated sub-struct: written every block by RT, read by UI.
    pub rt_to_ui: RtToUi,
    /// Cache-line-isolated sub-struct: written by UI/Main, read every block by RT.
    pub ui_to_rt: UiToRt,
    /// Cache-line-isolated sub-struct: low-frequency access by both threads.
    pub cold: ColdShared,
}

/// Shared state handle for the CLAP plugin lifecycle (`clack_plugin::Plugin::Shared`).
///
/// Owns the underlying [`GuiSharedState`] via an [`Arc`], enabling safe reference-counted
/// ownership across GUI windows and background reaper threads without dangling pointers.
pub struct NamClapShared {
    /// Inner ref-counted GUI and plugin shared state.
    pub gui: Arc<GuiSharedState>,
}

impl<'a> PluginShared<'a> for NamClapShared {}

impl std::ops::Deref for NamClapShared {
    type Target = GuiSharedState;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.gui
    }
}

impl Drop for NamClapShared {
    fn drop(&mut self) {
        log::debug!("NAM-Plug: NamClapShared dropped.");
        self.gui.cold.alive_fence.store(false, Ordering::Release); // pairs with Acquire load
        // Only signal shutdown when the last instance is destroyed.
        let prev = ACTIVE_INSTANCES
            .try_update(Ordering::Release, Ordering::Relaxed, |val| {
                Some(val.saturating_sub(1))
            })
            .unwrap_or(0);
        let remaining = prev.saturating_sub(1);
        if prev > 0 && remaining == 0 {
            neural_amp_modeler_rs::common::panic_hook::set_shutdown_in_progress();
        }
    }
}

impl neural_amp_modeler_rs::common::diagnostics::HasRuntimeSnapshot for GuiSharedState {
    fn model_info(&self) -> Option<neural_amp_modeler_rs::common::diagnostics::ModelInfo> {
        lock_cold(&self.cold.ui_model_info, "ui_model_info").clone()
    }

    fn model_memory_bytes(&self) -> Option<usize> {
        let bytes = self.cold.ui_model_memory_bytes.load(Ordering::Relaxed);
        if bytes > 0 { Some(bytes) } else { None }
    }

    fn audio_info(
        &self,
        consumer: &neural_amp_modeler_rs::common::diagnostics::AudioMetadata,
    ) -> neural_amp_modeler_rs::common::diagnostics::AudioInfo {
        let sr = self.cold.sample_rate.load(Ordering::Relaxed);
        let buffer_size = self.cold.buffer_size.load(Ordering::Relaxed) as usize;
        neural_amp_modeler_rs::common::diagnostics::AudioInfo::new(
            sr,
            buffer_size,
            consumer.channel_count,
            consumer.host_name.clone(),
        )
    }

    fn rt_info(&self) -> neural_amp_modeler_rs::common::diagnostics::RtInfo {
        self.cold.rt_status.rt_info()
    }

    fn telemetry_snapshot(&self) -> neural_amp_modeler_rs::common::diagnostics::TelemetrySnapshot {
        self.cold.rt_status.telemetry_snapshot()
    }

    fn flags_seen(&self) -> u64 {
        self.cold.rt_status.flags_seen()
    }

    fn extra_telemetry(&self) -> Vec<(&'static str, u64)> {
        vec![(
            "retired_box_overwritten",
            self.cold.box_overflow.overwritten_total() as u64,
        )]
    }
}

impl neural_amp_modeler_rs::common::diagnostics::HasRuntimeSnapshot for NamClapShared {
    fn model_info(&self) -> Option<neural_amp_modeler_rs::common::diagnostics::ModelInfo> {
        self.gui.model_info()
    }

    fn model_memory_bytes(&self) -> Option<usize> {
        self.gui.model_memory_bytes()
    }

    fn audio_info(
        &self,
        consumer: &neural_amp_modeler_rs::common::diagnostics::AudioMetadata,
    ) -> neural_amp_modeler_rs::common::diagnostics::AudioInfo {
        self.gui.audio_info(consumer)
    }

    fn rt_info(&self) -> neural_amp_modeler_rs::common::diagnostics::RtInfo {
        self.gui.rt_info()
    }

    fn telemetry_snapshot(&self) -> neural_amp_modeler_rs::common::diagnostics::TelemetrySnapshot {
        self.gui.telemetry_snapshot()
    }

    fn flags_seen(&self) -> u64 {
        self.gui.flags_seen()
    }

    fn extra_telemetry(&self) -> Vec<(&'static str, u64)> {
        self.gui.extra_telemetry()
    }
}

#[cfg(test)]
#[path = "../shared_test.rs"]
mod shared_test;

#[cfg(test)]
pub(crate) use shared_test::make_test_shared;
