// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! DeactivatedDspState — ownership of heavy DSP resources across activate cycles.
//!
//! When the host deactivates a plugin instance (mute track, bypass, UI close),
//! expensive resources — model weights, resampler polyphase coefficient banks,
//! oversampling half-band filter states, convolution FFT partitions — are moved
//! here instead of being dropped. On the next `activate()`, they are reinstalled
//! deterministically, avoiding I/O, filter-bank pre-compute, and FFT setup overhead.
//!
//! Validation on restore: `sample_rate` and `buffer_size` must match the current
//! audio configuration. If the host changed either, the affected resources are
//! discarded and rebuilt from scratch. Model weights are always reusable.

use crate::clap::plugin::NamClapMainThread;
use crate::clap::processor::NamClapProcessor;
use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimPair;
use neural_amp_modeler_rs::dsp::oversample::{OversampleEngine, OversampleFactor};
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::models::StaticModel;
use std::sync::atomic::Ordering;

/// Heavy DSP resources preserved across deactivate/activate cycles.
pub(crate) struct DeactivatedDspState {
    /// Active neural model (L channel). Always reusable — model weights
    /// are independent of host sample rate and buffer size.
    pub(crate) model_l: Option<Box<StaticModel>>,
    /// Active neural model (R channel) in dual-mono mode.
    pub(crate) model_r: Option<Box<StaticModel>>,
    /// Monotonic generation of the active model identity,
    /// persisted with the model so a reactivation keeps the same generation.
    pub(crate) model_generation: u64,
    /// Cab-sim convolution pair. The underlying `process_block` driver is
    /// block-agnostic — host blocks of any size are chunked against the fixed
    /// partition internally — so the pair is reusable across buffer-size
    /// changes and only a host sample-rate change (the IR is resampled to the
    /// host rate at build time) forces a rebuild. Kept `Box`ed to preserve the
    /// end-to-end real-time safe ownership contract.
    pub(crate) cabsim_pair: Option<Box<CabSimPair>>,
    /// Polyphase sinc resampler. Reusable only if `host_rate` matches the
    /// current host sample rate (phase interpolation banks are rate-dependent).
    pub(crate) resampler: Box<NamResampler>,
    /// Strict-cardinality streaming resample adapter.
    /// Reusable only if both `host_rate` and `buffer_size` match (FIFO
    /// capacities are sized by the worst-case host block).
    pub(crate) stream: Box<neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer>,
    /// Half-band oversampling engine L. Reusable unconditionally (fixed at
    /// MAX_RESAMP_BUF input, state is reset per activate).
    pub(crate) os_l: Box<OversampleEngine>,
    /// Half-band oversampling engine R.
    pub(crate) os_r: Box<OversampleEngine>,
    /// Oversampling factor these engines were *applied* with.
    /// Persisted from `NamClapProcessor::applied_os_factor` at deactivate —
    /// never the requested `params.oversample`, which may lag ahead of the
    /// engines while a host restart is pending. Used to detect real engine
    /// compatibility on restore.
    pub(crate) os_factor: OversampleFactor,
    /// Host sample rate when deactivated (for restore validation).
    pub(crate) sample_rate: u32,
    /// Host buffer size when deactivated (for `conv_engine` validation).
    pub(crate) buffer_size: u32,
    /// Model input gain calibration multiplier (from `input_level_dbu` metadata).
    pub(crate) model_input_mult_adj: f32,
    /// Model output gain calibration multiplier (from loudness metadata).
    pub(crate) model_output_mult_adj: f32,
    /// Model calibration metadata preserved across deactivate/activate.
    pub(crate) model_cal_meta: crate::clap::extensions::calibration::ModelCalibrationMeta,
}

/// Deactivates the audio processor instance, returning channel resources to shared storage
/// and preserving reusable DSP states off the audio thread.
pub(crate) fn deactivate<'a>(
    mut processor: NamClapProcessor<'a>,
    main_thread: &NamClapMainThread<'a>,
) {
    processor
        .shared
        .cold
        .is_processing
        .store(false, Ordering::Relaxed);
    // Isolate panics during cleanup.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // A structural command deferred by the final callback is resolved
        // before teardown. The parked payload's heap resources drop here on
        // the main thread (safe — never on the audio thread), and its
        // rolled-back sequence slot is consumed so the ack stays gapless
        // across a subsequent deactivate/activate cycle (the ring may still
        // hold commands for the next activate).
        let (mut cmd_consumer, parked) = processor.cmd_drain.into_parts();
        if parked.is_some() {
            cmd_consumer.advance_pending();
            cmd_consumer.ack_processed();
        }
        drop(parked);

        let mut param_rx_guard = processor
            .shared
            .cold
            .param_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *param_rx_guard = Some(cmd_consumer.into_inner());

        let mut gc_tx_guard = processor
            .shared
            .cold
            .gc_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *gc_tx_guard = Some(processor.gc_tx);

        let (slimmable_rx, parked_slimmable) = processor.slimmable_drain.into_parts();
        // A slimmable rebuild parked by the final callback drops here on
        // the main thread (safe — never on the audio thread). The plain
        // ring carries no ack, so no sequence bookkeeping is needed.
        drop(parked_slimmable);

        let mut slimmable_rx_guard = processor
            .shared
            .cold
            .slimmable_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *slimmable_rx_guard = Some(slimmable_rx);

        // Preserve heavy DSP resources across deactivate/activate cycles
        // to avoid I/O, filter-bank recompute, and FFT setup on the next
        // activate(). Resources are validated on restore against the current
        // audio configuration.
        //
        // Persist the *applied* oversampling factor — the one the preserved
        // engines were actually built for — NEVER the user/DAW-requested
        // `params.oversample`, which may still be ahead of the engines while
        // a host restart is pending. Otherwise the next activate() would
        // consider the old engines compatible with the new requested factor
        // and reuse them incorrectly.
        // If the processor was poisoned by a runtime panic, discard the corrupted
        // DSP state so that the subsequent activate() rebuilds a fresh pipeline.
        if !processor.poisoned {
            let deactivated = DeactivatedDspState {
                model_l: processor.model_l,
                model_r: processor.model_r,
                model_generation: processor.model_generation,
                cabsim_pair: processor.cabsim_pair,
                resampler: processor.resampler,
                stream: processor.stream,
                os_l: processor.os_l,
                os_r: processor.os_r,
                os_factor: processor.applied_os_factor,
                sample_rate: processor.shared.cold.sample_rate.load(Ordering::Relaxed),
                buffer_size: processor.shared.cold.buffer_size.load(Ordering::Relaxed),
                model_input_mult_adj: processor.model_input_mult_adj,
                model_output_mult_adj: processor.model_output_mult_adj,
                model_cal_meta: processor.model_cal_meta,
            };
            *processor
                .shared
                .cold
                .deactivated_dsp
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(deactivated);
        } else {
            *processor
                .shared
                .cold
                .deactivated_dsp
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
        }

        // Hand off single-owner parking-lot items to the final off-RT GC
        // drain. The audio thread has already stopped (the host calls
        // deactivate() after stop_processing) and the processor has not been
        // dropped yet — a single drain_gc_final call releases the SPSC +
        // overflow + the 16 parking-lot slots on the main thread.
        main_thread.drain_gc_final(&mut processor.parking_lot);
    }));
    if let Err(err) = result {
        // Deactivate panicked — resources may leak but crash report
        // was already written. Drop the payload silently.
        drop(err);
    }
}
