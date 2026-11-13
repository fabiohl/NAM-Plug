// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Processor activation subsystem (`activate`).
//!
//! Handles pre-allocation of RT-safe buffers and initialization of all DSP
//! sub-components (models, resamplers, cab-sim IRs, oversamplers, and smoothers).

use crate::clap::extensions::cabinet::{CabAssetKind, CabSlotDsp, build_cab_nam_from_shared};
use crate::clap::extensions::params::bypass_u32_to_bool;
use crate::clap::plugin::errors::{self, static_plugin_error};
use crate::clap::plugin::shared::{PendingRestartOs, StagedRestore, StagedSwap};
use crate::clap::plugin::{CommandConsumer, NamClapMainThread, NamClapShared};
use crate::clap::processor::dsp::dry_delay::{DRY_DELAY_MAX_EXTRA, DryDelayLine};
use crate::clap::processor::panic_to_error;
use crate::clap::processor::prewarm_policy;
use crate::clap::processor::rollback;
use crate::clap::processor::state::{
    BypassCrossfader, CMD_SWAP_TUNABLES, NamClapProcessor, SLIMMABLE_SWAP_TUNABLES,
};
use clack_common::process::PluginAudioConfiguration;
use clack_plugin::host::HostAudioProcessorHandle;
use clack_plugin::prelude::PluginError;
use neural_amp_modeler_rs::common::params::RtProcessingParams;
use neural_amp_modeler_rs::common::spsc::RtSwapDrain;
use neural_amp_modeler_rs::dsp::adaptive::AdaptiveCompute;
use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimPair;
use neural_amp_modeler_rs::dsp::gate::{DynamicHysteresis, GateParams};
use neural_amp_modeler_rs::dsp::oversample::{OversampleEngine, OversampleFactor};
use neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF;
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::dsp::smoother::ParamSmoother;
use neural_amp_modeler_rs::math::common::AlignedVec;
use neural_amp_modeler_rs::math::dsp::gain_lut::get_gain_lut;
use neural_amp_modeler_rs::models::NamModel;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Standardized pre-allocation error constructor for intermediate audio scratch buffers.
fn buffer_prealloc_error(stage: &str, error: impl std::fmt::Debug) -> PluginError {
    static_plugin_error(
        errors::activation::BUFFER_PREALLOC_FAILED,
        format_args!("Failed to allocate intermediate {stage} buffers: {error:?}"),
    )
}

/// Rollback guard that restores staged restore and staged swap items back to
/// `NamClapMainThread` if `activate()` fails or early-returns with an error.
struct StagedItemsRollbackGuard<'m, 'a> {
    main_thread: &'m NamClapMainThread<'a>,
    staged_restore: Option<StagedRestore>,
    staged_swap: Option<StagedSwap>,
    disarmed: bool,
}

impl Drop for StagedItemsRollbackGuard<'_, '_> {
    fn drop(&mut self) {
        if !self.disarmed {
            if let Some(r) = self.staged_restore.take() {
                *self.main_thread.staged_restore.borrow_mut() = Some(r);
            }
            if let Some(s) = self.staged_swap.take() {
                *self.main_thread.staged_swap.borrow_mut() = Some(s);
            }
        }
    }
}

/// Constructs a new CabSimPair adapter directly from raw IR samples or fallback storage.
fn build_cab_sim_from_raw_samples(
    shared: &NamClapShared,
    override_samples: Option<(&[f32], u32)>,
    partition_size: usize,
    host_rate: u32,
) -> Result<Option<Box<CabSimPair>>, PluginError> {
    use crate::clap::extensions::ir_level_policy::{IrLevelPolicy, apply_ir_level_policy};
    use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter;
    use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;
    use neural_amp_modeler_rs::dsp::cabsim::loader::CabSimIr;

    if partition_size == 0 {
        return Ok(None);
    }

    // Step 1: Acquire raw IR samples from caller override or ColdShared fallback storage.
    let (samples_cow, stored_rate) = match override_samples {
        Some((samples, rate)) => (std::borrow::Cow::Borrowed(samples), rate),
        None => {
            let raw_guard = shared.cold.ir_raw_samples.lock().map_err(|e| {
                static_plugin_error(
                    errors::dsp_resources::CABSIM_STORAGE_LOCK_POISONED,
                    format_args!("{e}"),
                )
            })?;
            let Some(ref samples) = *raw_guard else {
                return Ok(None);
            };
            (
                std::borrow::Cow::Owned(samples.clone()),
                shared.cold.ir_raw_sample_rate.load(Ordering::Relaxed),
            )
        }
    };

    if samples_cow.is_empty() {
        return Ok(None);
    }

    // Step 2: High-fidelity sample rate conversion if stored IR sample rate differs from the host DAW rate.
    let mut resolved_samples: Vec<f32> = if stored_rate > 0 && stored_rate != host_rate {
        CabSimIr::resample(&samples_cow, stored_rate, host_rate).map_err(|e| {
            static_plugin_error(
                errors::dsp_resources::CABSIM_IR_RESAMPLE_FAILED,
                format_args!("{stored_rate} Hz → {host_rate} Hz: {e}"),
            )
        })?
    } else {
        samples_cow.into_owned()
    };

    // Step 2b: Apply active IR level policy non-destructively to the convolution kernel.
    let policy = IrLevelPolicy::from_u8(shared.cold.ir_level_policy.load(Ordering::Relaxed));
    let _ = apply_ir_level_policy(&mut resolved_samples, policy, host_rate);

    // Step 3: Instantiate Uniform-Partitioned Overlap-Save (UPOLS) frequency-domain convolution engines for L/R channels.
    let build_adapter = || -> Result<CabSimAdapter, PluginError> {
        let engine = ConvEngine::new(&resolved_samples, partition_size).map_err(|e| {
            static_plugin_error(
                errors::dsp_resources::CABSIM_BUILD_FAILED,
                format_args!("ConvEngine::new failed: {e:?}"),
            )
        })?;
        CabSimAdapter::new(Box::new(engine)).map_err(|e| {
            static_plugin_error(
                errors::dsp_resources::CABSIM_BUILD_FAILED,
                format_args!("CabSimAdapter::new failed: {e:?}"),
            )
        })
    };

    let l = build_adapter()?;
    let r = build_adapter()?;

    Ok(Some(Box::new(CabSimPair {
        l: Box::new(l),
        r: Box::new(r),
        sample_rate: host_rate,
    })))
}

fn build_cab_slot_from_cold_storage(
    shared: &NamClapShared,
    override_wav: Option<(&[f32], u32)>,
    override_nam: Option<(CabAssetKind, &[u8])>,
    partition_size: usize,
    host_rate: u32,
) -> Result<Option<CabSlotDsp>, PluginError> {
    if let Some(slot) = build_cab_nam_from_shared(shared, override_nam, partition_size, host_rate)?
    {
        return Ok(Some(slot));
    }
    Ok(
        build_cab_sim_from_raw_samples(shared, override_wav, partition_size, host_rate)?
            .map(CabSlotDsp::WavIr),
    )
}

impl<'a> NamClapProcessor<'a> {
    /// Activates the real-time audio processor.
    ///
    /// `activate` is the ONLY allocation site — kept out of `process`.
    pub(crate) fn activate(
        host: HostAudioProcessorHandle<'a>,
        main_thread: &NamClapMainThread<'a>,
        shared: &'a NamClapShared,
        audio_config: PluginAudioConfiguration,
    ) -> Result<Self, PluginError> {
        // Catch panics from this instance so they don't
        // crash the host or other active instances.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // 1. SPSC channel extraction from Shared (ownership transfer)
            // Extracted resources are held in a rollback guard.
            // If any later allocation fails, Drop restores everything into ColdShared.
            let param_rx = shared
                .cold
                .param_rx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .ok_or_else(|| {
                    PluginError::Message(errors::activation::PARAM_RX_ALREADY_EXTRACTED)
                })?;

            let mut rollback = rollback::ActivateRollbackGuard::new(shared);
            rollback.param_rx = Some(param_rx);

            let gc_tx = shared
                .cold
                .gc_tx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .ok_or_else(|| PluginError::Message(errors::activation::GC_TX_ALREADY_EXTRACTED))?;
            rollback.gc_tx = Some(gc_tx);

            let slimmable_rx = shared
                .cold
                .slimmable_rx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .ok_or_else(|| {
                    PluginError::Message(errors::activation::SLIMMABLE_RX_ALREADY_EXTRACTED)
                })?;
            rollback.slimmable_rx = Some(slimmable_rx);

            // 2. Intermediate buffer pre-allocation (Disjoint Stages)
            let buf_capacity = (audio_config.max_frames_count as usize)
                .max(MAX_RESAMP_BUF)
                .max(1024)
                * 2;
            let buf_host_l = AlignedVec::new(buf_capacity, 0.0f32)
                .map_err(|e| buffer_prealloc_error("host", e))?;
            let buf_host_r = AlignedVec::new(buf_capacity, 0.0f32)
                .map_err(|e| buffer_prealloc_error("host", e))?;
            let buf_mid_l = AlignedVec::new(buf_capacity, 0.0f32)
                .map_err(|e| buffer_prealloc_error("mid", e))?;
            let buf_mid_r = AlignedVec::new(buf_capacity, 0.0f32)
                .map_err(|e| buffer_prealloc_error("mid", e))?;
            let buf_out_l = AlignedVec::new(buf_capacity, 0.0f32)
                .map_err(|e| buffer_prealloc_error("output", e))?;
            let buf_out_r = AlignedVec::new(buf_capacity, 0.0f32)
                .map_err(|e| buffer_prealloc_error("output", e))?;

            // 2b. Oversample buffer pre-allocation (MAX_RESAMP_BUF * 4 for X4)
            let os_capacity = MAX_RESAMP_BUF * 4;
            let buf_os_in_l = AlignedVec::new(os_capacity, 0.0f32)
                .map_err(|e| buffer_prealloc_error("oversample input", e))?;
            let buf_os_in_r = AlignedVec::new(os_capacity, 0.0f32)
                .map_err(|e| buffer_prealloc_error("oversample input", e))?;
            let buf_os_model_l = AlignedVec::new(os_capacity, 0.0f32)
                .map_err(|e| buffer_prealloc_error("oversample model", e))?;
            let buf_os_model_r = AlignedVec::new(os_capacity, 0.0f32)
                .map_err(|e| buffer_prealloc_error("oversample model", e))?;

            // 2c. Bypass crossfade dry storage (one sub-block of input samples, max_frames_count).
            let xfade_capacity = audio_config.max_frames_count as usize;
            let buf_xfade_dry_l = AlignedVec::new(xfade_capacity, 0.0f32)
                .map_err(|e| buffer_prealloc_error("bypass xfade dry", e))?;
            let buf_xfade_dry_r = AlignedVec::new(xfade_capacity, 0.0f32)
                .map_err(|e| buffer_prealloc_error("bypass xfade dry", e))?;

            // 2d. WaveNet crossfade scratch buffers (0.5.0 run_inference): used
            // as the second-pass output when processing is chunked, so it must
            // not alias any accumulated output buffer. MAX_RESAMP_BUF each.
            let buf_xfd_scratch_l = AlignedVec::new(MAX_RESAMP_BUF, 0.0f32)
                .map_err(|e| buffer_prealloc_error("crossfade scratch", e))?;
            let buf_xfd_scratch_r = AlignedVec::new(MAX_RESAMP_BUF, 0.0f32)
                .map_err(|e| buffer_prealloc_error("crossfade scratch", e))?;

            // 2e. Cabinet bypass & neural cabinet scratch buffers (zero RT alloc).
            let buf_cab_dry_l = AlignedVec::new(buf_capacity, 0.0f32)
                .map_err(|e| buffer_prealloc_error("cab dry delay", e))?;
            let buf_cab_dry_r = AlignedVec::new(buf_capacity, 0.0f32)
                .map_err(|e| buffer_prealloc_error("cab dry delay", e))?;
            let buf_cab_nam_in_l = AlignedVec::new(buf_capacity, 0.0f32)
                .map_err(|e| buffer_prealloc_error("cab nam input", e))?;
            let buf_cab_nam_in_r = AlignedVec::new(buf_capacity, 0.0f32)
                .map_err(|e| buffer_prealloc_error("cab nam input", e))?;
            let buf_cab_zero_l = AlignedVec::new(buf_capacity, 0.0f32)
                .map_err(|e| buffer_prealloc_error("cab zero input", e))?;
            let buf_cab_zero_r = AlignedVec::new(buf_capacity, 0.0f32)
                .map_err(|e| buffer_prealloc_error("cab zero input", e))?;

            // 3. DSP component initialization
            let model_rate = shared.cold.model_sample_rate.load(Ordering::Relaxed);
            let model_rate = if model_rate == 0 { 48000 } else { model_rate };
            let host_rate = (audio_config.sample_rate as u32).clamp(4_000, 384_000);
            let host_buffer = audio_config.max_frames_count;

            // Resolve the oversampling factor for this activation.
            // Priority: pending restart > UiToRt atomic > Off (fresh).
            // The pending restart is consumed atomically here; if any later
            // fallible stage fails, the rollback guard restores it so the
            // request survives into the next activate() attempt.
            let pending_restart =
                PendingRestartOs::take(&shared.cold.pending_restart_os_factor, Ordering::Acquire);
            if pending_restart != PendingRestartOs::None {
                rollback.pending_restart_os_factor = Some(pending_restart);
            }
            let os_factor = match pending_restart {
                PendingRestartOs::Pending(f) => f,
                PendingRestartOs::None => OversampleFactor::from_f32(
                    shared.ui_to_rt.param_oversample.load(Ordering::Relaxed) as f32,
                ),
            };

            // Restore heavy DSP resources from DeactivatedDspState if
            // available, validating sample rate and buffer size invariants. Model
            // weights are always reusable; resampler and conv-engine require
            // matching audio configuration.
            //
            // DeactivatedDspState is extracted into the rollback guard
            // immediately after `.take()`. The guard retains it intact during all
            // fallible allocations (resamplers, oversamplers, CabSim).
            rollback.deactivated = shared
                .cold
                .deactivated_dsp
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();

            let (new_resampler, new_stream, new_cabsim, new_os) =
                if let Some(ref deact) = rollback.deactivated {
                    let rate_matches = deact.sample_rate == host_rate;
                    let buf_matches = deact.buffer_size == host_buffer;
                    let os_matches = deact.os_factor == os_factor;
                    // The cab-sim adapter is block-agnostic (`process_block`
                    // chunks arbitrary host blocks against the fixed partition
                    // internally), so a buffer-size renegotiation no longer
                    // invalidates it: only the sample rate does (the IR was
                    // resampled to the host rate at build time). Quantum
                    // changes reuse the adapter — keeping its partition (and
                    // the latency the host already received) instead of paying
                    // a full FFT rebuild per buffer-size change.
                    let cab_matches = deact.cab_slot.as_ref().is_some_and(|c| {
                        c.is_compatible_with_host(host_rate, host_buffer as usize)
                    });
                    // The streaming adapter's FIFO capacities are sized by the
                    // worst-case host block, so it also needs `buf_matches`.
                    let stream_matches = rate_matches && buf_matches;

                    let res = if rate_matches {
                        None
                    } else {
                        Some(Box::new(
                            NamResampler::new_simple(host_rate, model_rate).map_err(|e| {
                                static_plugin_error(
                                    errors::dsp_resources::RESAMPLER_BUILD_FAILED,
                                    format_args!("{e:?}"),
                                )
                            })?,
                        ))
                    };

                    let stream = if stream_matches {
                        None
                    } else {
                        Some(
                            crate::clap::plugin::build_stream_adapter(
                                host_rate,
                                model_rate,
                                host_buffer as usize,
                            )
                            .map_err(|e| {
                                static_plugin_error(
                                    errors::dsp_resources::STREAM_BUILD_FAILED,
                                    format_args!("{e:?}"),
                                )
                            })?,
                        )
                    };

                    let cab = if cab_matches {
                        None
                    } else {
                        Some(build_cab_slot_from_cold_storage(
                            shared,
                            None,
                            None,
                            audio_config.max_frames_count as usize,
                            host_rate,
                        )?)
                    };

                    let os = if os_matches {
                        None
                    } else {
                        let os_l = Box::new(
                            OversampleEngine::new(os_factor, MAX_RESAMP_BUF).map_err(|e| {
                                static_plugin_error(
                                    errors::dsp_resources::OVERSAMPLER_BUILD_FAILED,
                                    format_args!("(L) engine: {e:?}"),
                                )
                            })?,
                        );
                        let os_r = Box::new(
                            OversampleEngine::new(os_factor, MAX_RESAMP_BUF).map_err(|e| {
                                static_plugin_error(
                                    errors::dsp_resources::OVERSAMPLER_BUILD_FAILED,
                                    format_args!("(R) engine: {e:?}"),
                                )
                            })?,
                        );
                        Some((os_l, os_r))
                    };

                    (res, stream, cab, os)
                } else {
                    let res = Some(Box::new(
                        NamResampler::new_simple(host_rate, model_rate).map_err(|e| {
                            static_plugin_error(
                                errors::dsp_resources::RESAMPLER_BUILD_FAILED,
                                format_args!("{e:?}"),
                            )
                        })?,
                    ));

                    let stream = Some(
                        crate::clap::plugin::build_stream_adapter(
                            host_rate,
                            model_rate,
                            host_buffer as usize,
                        )
                        .map_err(|e| {
                            static_plugin_error(
                                errors::dsp_resources::STREAM_BUILD_FAILED,
                                format_args!("{e:?}"),
                            )
                        })?,
                    );

                    let cab = Some(build_cab_slot_from_cold_storage(
                        shared,
                        None,
                        None,
                        audio_config.max_frames_count as usize,
                        host_rate,
                    )?);

                    let os_l = Box::new(OversampleEngine::new(os_factor, MAX_RESAMP_BUF).map_err(
                        |e| {
                            static_plugin_error(
                                errors::dsp_resources::OVERSAMPLER_BUILD_FAILED,
                                format_args!("(L) engine: {e:?}"),
                            )
                        },
                    )?);
                    let os_r = Box::new(OversampleEngine::new(os_factor, MAX_RESAMP_BUF).map_err(
                        |e| {
                            static_plugin_error(
                                errors::dsp_resources::OVERSAMPLER_BUILD_FAILED,
                                format_args!("(R) engine: {e:?}"),
                            )
                        },
                    )?);

                    (res, stream, cab, Some((os_l, os_r)))
                };

            // Consume any latency-affecting restore or swap staged for this
            // restart cycle. All fallible stages above already succeeded, so
            // staged items are never stranded by a failure between here and
            // `Self` construction. The staged resources were fully built off-RT
            // by the main thread; they take precedence over preserved
            // (`DeactivatedDspState`) ones, and superseded preserved resources
            // drop here on the main thread.
            let mut staged_guard = StagedItemsRollbackGuard {
                main_thread,
                staged_restore: main_thread.staged_restore.borrow_mut().take(),
                staged_swap: main_thread.staged_swap.borrow_mut().take(),
                disarmed: false,
            };

            // Adapt staged_restore for any host audio configuration changes
            // (e.g. sample rate or buffer size renegotiated upon restart).
            if let Some(ref mut r) = staged_guard.staged_restore {
                if let Some(ref mut m) = r.txn.model
                    && (m.new_stream.host_rate() != host_rate
                        || m.new_stream.max_block() != host_buffer as usize)
                {
                    log::info!(
                        "NAM-Plug: adapting staged restore model for new audio config \
                     (staged: {} Hz/{} frames -> host: {} Hz/{} frames)",
                        m.new_stream.host_rate(),
                        m.new_stream.max_block(),
                        host_rate,
                        host_buffer
                    );
                    let model_rate = m.new_stream.model_rate();
                    let new_res = NamResampler::new_simple(host_rate, model_rate).map_err(|e| {
                        static_plugin_error(
                            errors::dsp_resources::RESAMPLER_BUILD_FAILED,
                            format_args!("{e:?}"),
                        )
                    })?;
                    let new_stream = crate::clap::plugin::build_stream_adapter(
                        host_rate,
                        model_rate,
                        host_buffer as usize,
                    )
                    .map_err(|e| {
                        static_plugin_error(
                            errors::dsp_resources::STREAM_BUILD_FAILED,
                            format_args!("{e:?}"),
                        )
                    })?;
                    if let Some(ref mut model) = m.model_l {
                        model
                            .set_max_buffer_size(host_buffer as usize)
                            .map_err(|e| {
                                static_plugin_error(
                                    errors::activation::MODEL_RESIZE_FAILED,
                                    format_args!("{e:?}"),
                                )
                            })?;
                        let prewarm_cost_ns =
                            if crate::clap::processor::prewarm_policy::ADAPTIVE_COST_ENABLED {
                                let measure_frames =
                                    crate::clap::processor::prewarm_policy::PREWARM_MEASURE_FRAMES
                                        .min((host_buffer as usize).max(1));
                                Some(model.measure_cost_ns_per_sample(measure_frames))
                            } else {
                                None
                            };
                        m.prewarm_cost_ns = prewarm_cost_ns;
                    }
                    if let Some(ref mut model) = m.model_r {
                        model
                            .set_max_buffer_size(host_buffer as usize)
                            .map_err(|e| {
                                static_plugin_error(
                                    errors::activation::MODEL_RESIZE_FAILED,
                                    format_args!("{e:?}"),
                                )
                            })?;
                    }
                    *m.new_resampler = new_res;
                    m.new_stream = new_stream;
                }

                if let Some(Some(ref mut cab_slot)) = r.txn.ir
                    && !cab_slot.is_exact_host_config(host_rate, host_buffer as usize)
                {
                    let override_wav = r
                        .publish
                        .ir_raw_samples
                        .as_deref()
                        .map(|s| (s, r.publish.ir_raw_sample_rate));
                    let override_nam = r
                        .publish
                        .cab_nam_raw_bytes
                        .as_ref()
                        .map(|(k, b)| (*k, b.as_slice()));
                    if let Some(new_slot) = build_cab_slot_from_cold_storage(
                        shared,
                        override_wav,
                        override_nam,
                        host_buffer as usize,
                        host_rate,
                    )? {
                        *cab_slot = new_slot;
                    }
                }
            }

            // Adapt staged_swap for any host configuration changes
            if let Some(ref mut s) = staged_guard.staged_swap {
                if let Some(ref mut m) = s.model
                    && (m.new_stream.host_rate() != host_rate
                        || m.new_stream.max_block() != host_buffer as usize)
                {
                    log::info!(
                        "NAM-Plug: adapting staged model swap for new audio config \
                     (staged: {} Hz/{} frames -> host: {} Hz/{} frames)",
                        m.new_stream.host_rate(),
                        m.new_stream.max_block(),
                        host_rate,
                        host_buffer
                    );
                    let model_rate = m.new_stream.model_rate();
                    let new_res = NamResampler::new_simple(host_rate, model_rate).map_err(|e| {
                        static_plugin_error(
                            errors::dsp_resources::RESAMPLER_BUILD_FAILED,
                            format_args!("{e:?}"),
                        )
                    })?;
                    let new_stream = crate::clap::plugin::build_stream_adapter(
                        host_rate,
                        model_rate,
                        host_buffer as usize,
                    )
                    .map_err(|e| {
                        static_plugin_error(
                            errors::dsp_resources::STREAM_BUILD_FAILED,
                            format_args!("{e:?}"),
                        )
                    })?;
                    if let Some(ref mut model) = m.model_l {
                        model
                            .set_max_buffer_size(host_buffer as usize)
                            .map_err(|e| {
                                static_plugin_error(
                                    errors::activation::MODEL_RESIZE_FAILED,
                                    format_args!("{e:?}"),
                                )
                            })?;
                        let prewarm_cost_ns =
                            if crate::clap::processor::prewarm_policy::ADAPTIVE_COST_ENABLED {
                                let measure_frames =
                                    crate::clap::processor::prewarm_policy::PREWARM_MEASURE_FRAMES
                                        .min((host_buffer as usize).max(1));
                                Some(model.measure_cost_ns_per_sample(measure_frames))
                            } else {
                                None
                            };
                        m.prewarm_cost_ns = prewarm_cost_ns;
                    }
                    if let Some(ref mut model) = m.model_r {
                        model
                            .set_max_buffer_size(host_buffer as usize)
                            .map_err(|e| {
                                static_plugin_error(
                                    errors::activation::MODEL_RESIZE_FAILED,
                                    format_args!("{e:?}"),
                                )
                            })?;
                    }
                    *m.new_resampler = new_res;
                    m.new_stream = new_stream;
                }

                if let Some(Some(ref mut cab_slot)) = s.ir
                    && !cab_slot.is_exact_host_config(host_rate, host_buffer as usize)
                    && let Some(new_slot) = build_cab_slot_from_cold_storage(
                        shared,
                        None,
                        None,
                        host_buffer as usize,
                        host_rate,
                    )?
                {
                    *cab_slot = new_slot;
                }
            }

            let staged_restore = staged_guard.staged_restore.take();
            let (staged_model, staged_ir) = match staged_guard.staged_swap.take() {
                Some(s) => (s.model, s.ir),
                None => (None, None),
            };
            staged_guard.disarmed = true;

            let staged_restore_valid = staged_restore.is_some();
            let staged_model_ok = staged_model.is_some();

            // All fallible stages succeeded: now assemble the active DSP resources
            // by taking ownership from staged items or the rollback guard.
            let (
                mut model_l,
                mut model_r,
                model_generation,
                resampler,
                stream,
                os_l,
                os_r,
                cabsim_pair,
                model_input_mult_adj,
                model_output_mult_adj,
                model_cal_meta,
            ) = if staged_restore_valid {
                // `staged_restore_valid` is `staged_restore.as_ref().is_some_and(..)`,
                // so the flag implies a staged value; a `None` here is an internal
                // invariant violation and must not panic on the activation path.
                let Some(StagedRestore { txn, publish }) = staged_restore else {
                    debug_assert!(false, "staged_restore_valid implies a staged restore");
                    return Err(PluginError::Message(
                        errors::activation::STAGED_RESTORE_UNAVAILABLE,
                    ));
                };
                crate::clap::extensions::state_transaction::publish_restore(publish, main_thread);
                shared
                    .cold
                    .last_applied_generation
                    .store(txn.generation, Ordering::Relaxed);

                if let Some(deact) = rollback.deactivated.take() {
                    let (
                        model_l,
                        model_r,
                        model_generation,
                        resampler,
                        stream,
                        model_input_mult_adj,
                        model_output_mult_adj,
                        model_cal_meta,
                    ) = if let Some(m) = txn.model {
                        (
                            m.model_l,
                            m.model_r,
                            m.generation,
                            m.new_resampler,
                            m.new_stream,
                            m.input_mult_adj,
                            m.output_mult_adj,
                            m.cal_meta,
                        )
                    } else {
                        (
                            deact.model_l,
                            deact.model_r,
                            deact.model_generation,
                            new_resampler.unwrap_or(deact.resampler),
                            new_stream.unwrap_or(deact.stream),
                            deact.model_input_mult_adj,
                            deact.model_output_mult_adj,
                            deact.model_cal_meta,
                        )
                    };

                    let cab_slot = if let Some(ir) = txn.ir {
                        ir
                    } else if let Some(new_c) = new_cabsim {
                        new_c
                    } else {
                        deact.cab_slot
                    };

                    let (os_l, os_r) = if let Some((l, r)) = new_os {
                        (l, r)
                    } else {
                        (deact.os_l, deact.os_r)
                    };

                    (
                        model_l,
                        model_r,
                        model_generation,
                        resampler,
                        stream,
                        os_l,
                        os_r,
                        cab_slot,
                        model_input_mult_adj,
                        model_output_mult_adj,
                        model_cal_meta,
                    )
                } else {
                    let (
                        model_l,
                        model_r,
                        model_generation,
                        resampler,
                        stream,
                        model_input_mult_adj,
                        model_output_mult_adj,
                        model_cal_meta,
                    ) = if let Some(m) = txn.model {
                        (
                            m.model_l,
                            m.model_r,
                            m.generation,
                            m.new_resampler,
                            m.new_stream,
                            m.input_mult_adj,
                            m.output_mult_adj,
                            m.cal_meta,
                        )
                    } else {
                        (
                            None,
                            None,
                            0,
                            new_resampler.ok_or_else(|| {
                                PluginError::Message(
                                    errors::activation::RESAMPLER_UNAVAILABLE_STAGED,
                                )
                            })?,
                            new_stream.ok_or_else(|| {
                                PluginError::Message(errors::activation::STREAM_UNAVAILABLE_STAGED)
                            })?,
                            1.0,
                            1.0,
                            crate::clap::extensions::calibration::ModelCalibrationMeta::empty(),
                        )
                    };

                    let cab_slot = if let Some(ir) = txn.ir {
                        ir
                    } else {
                        new_cabsim.ok_or_else(|| {
                            PluginError::Message(errors::activation::CABSIM_UNAVAILABLE_STAGED)
                        })?
                    };

                    let (os_l, os_r) = new_os.ok_or_else(|| {
                        PluginError::Message(errors::activation::OVERSAMPLER_UNAVAILABLE_STAGED)
                    })?;

                    (
                        model_l,
                        model_r,
                        model_generation,
                        resampler,
                        stream,
                        os_l,
                        os_r,
                        cab_slot,
                        model_input_mult_adj,
                        model_output_mult_adj,
                        model_cal_meta,
                    )
                }
            } else if let Some(deact) = rollback.deactivated.take() {
                let (
                    model_l,
                    model_r,
                    model_generation,
                    resampler,
                    stream,
                    model_input_mult_adj,
                    model_output_mult_adj,
                    model_cal_meta,
                ) = if staged_model_ok {
                    // `staged_model_ok` is derived from
                    // `staged_swap.model.as_ref().is_some_and(..)`, so the flag
                    // implies a staged model; a `None` here is an internal
                    // invariant violation and must not panic on the activation path.
                    let Some(m) = staged_model else {
                        debug_assert!(false, "staged_model_ok implies a staged model");
                        return Err(PluginError::Message(
                            errors::activation::STAGED_MODEL_UNAVAILABLE,
                        ));
                    };
                    (
                        m.model_l,
                        m.model_r,
                        m.generation,
                        m.new_resampler,
                        m.new_stream,
                        m.input_mult_adj,
                        m.output_mult_adj,
                        m.cal_meta,
                    )
                } else {
                    (
                        deact.model_l,
                        deact.model_r,
                        deact.model_generation,
                        new_resampler.unwrap_or(deact.resampler),
                        new_stream.unwrap_or(deact.stream),
                        deact.model_input_mult_adj,
                        deact.model_output_mult_adj,
                        deact.model_cal_meta,
                    )
                };
                let cab_slot = if let Some(ir) = staged_ir {
                    ir
                } else if let Some(new_c) = new_cabsim {
                    new_c
                } else {
                    deact.cab_slot
                };
                let (os_l, os_r) = if let Some((l, r)) = new_os {
                    (l, r)
                } else {
                    (deact.os_l, deact.os_r)
                };
                (
                    model_l,
                    model_r,
                    model_generation,
                    resampler,
                    stream,
                    os_l,
                    os_r,
                    cab_slot,
                    model_input_mult_adj,
                    model_output_mult_adj,
                    model_cal_meta,
                )
            } else {
                let (
                    model_l,
                    model_r,
                    model_generation,
                    resampler,
                    stream,
                    model_input_mult_adj,
                    model_output_mult_adj,
                    model_cal_meta,
                ) = if staged_model_ok {
                    // `staged_model_ok` is derived from
                    // `staged_swap.model.as_ref().is_some_and(..)`, so the flag
                    // implies a staged model; a `None` here is an internal
                    // invariant violation and must not panic on the activation path.
                    let Some(m) = staged_model else {
                        debug_assert!(false, "staged_model_ok implies a staged model");
                        return Err(PluginError::Message(
                            errors::activation::STAGED_MODEL_UNAVAILABLE,
                        ));
                    };
                    (
                        m.model_l,
                        m.model_r,
                        m.generation,
                        m.new_resampler,
                        m.new_stream,
                        m.input_mult_adj,
                        m.output_mult_adj,
                        m.cal_meta,
                    )
                } else {
                    (
                        None,
                        None,
                        0,
                        new_resampler.ok_or_else(|| {
                            PluginError::Message(errors::activation::RESAMPLER_UNAVAILABLE_FRESH)
                        })?,
                        new_stream.ok_or_else(|| {
                            PluginError::Message(errors::activation::STREAM_UNAVAILABLE_FRESH)
                        })?,
                        1.0,
                        1.0,
                        crate::clap::extensions::calibration::ModelCalibrationMeta::empty(),
                    )
                };
                let cabsim_pair = if let Some(ir) = staged_ir {
                    ir
                } else {
                    new_cabsim.ok_or_else(|| {
                        PluginError::Message(errors::activation::CABSIM_UNAVAILABLE_FRESH)
                    })?
                };
                let (os_l, os_r) = new_os.ok_or_else(|| {
                    PluginError::Message(errors::activation::OVERSAMPLER_UNAVAILABLE_FRESH)
                })?;
                (
                    model_l,
                    model_r,
                    model_generation,
                    resampler,
                    stream,
                    os_l,
                    os_r,
                    cabsim_pair,
                    model_input_mult_adj,
                    model_output_mult_adj,
                    model_cal_meta,
                )
            };

            // A reinstalled cab-sim adapter must not bleed the previous host
            // session's convolution tail into the restarted timeline: the
            // deactivate/activate boundary is a timeline discontinuity (the
            // stream adapter is rebuilt fresh and the smoothers re-sync), so
            // clear the FDL and FIFO history in place. Zero-alloc — the IR and
            // the RFFT plan are preserved, so a quantum renegotiation still
            // avoids the full rebuild.
            let cab_slot = {
                let mut cab_slot = cabsim_pair;
                if let Some(ref mut cab) = cab_slot {
                    cab.reset();
                }
                cab_slot
            };

            // Complete any outstanding split-stabilization pass of the
            // activated model, eagerly and off the audio thread: `activate()`
            // runs on the main thread (no RT deadline) and `prewarm_step` is
            // allocation-free, so one full-budget call finishes every family
            // (LSTM `usize`, WaveNet/ConvNet one-shot, A2/Linear RF). The
            // two-phase reset latch is a callback-window microstate — never
            // persisted in `DeactivatedDspState` — so a deactivate() that
            // interrupted the amortization window leaves the preserved model
            // internally pending; draining here restores the fresh-instance
            // invariant (bit-exact post-stabilization state) before the
            // first block runs. A converged model (fresh loader install,
            // slimmable rebuild) reports complete immediately: the check
            // alone is the whole cost. The bounded round cap only guards a
            // hypothetical non-converging family against an unbounded loop.
            if let Some(model) = &mut model_l {
                for _ in 0..64 {
                    if model.prewarm_complete() {
                        break;
                    }
                    model.prewarm_step(usize::MAX);
                }
                debug_assert!(
                    model.prewarm_complete(),
                    "model stabilization pass must complete in activate()"
                );
                if !model.prewarm_complete() {
                    log::warn!(
                        "NAM-Plug: model stabilization pass did not complete during activate()"
                    );
                }
            }
            if let Some(model) = &mut model_r {
                for _ in 0..64 {
                    if model.prewarm_complete() {
                        break;
                    }
                    model.prewarm_step(usize::MAX);
                }
                debug_assert!(
                    model.prewarm_complete(),
                    "model_r stabilization pass must complete in activate()"
                );
                if !model.prewarm_complete() {
                    log::warn!(
                        "NAM-Plug: model_r stabilization pass did not complete during activate()"
                    );
                }
            }
            if let Some(ref mut model) = model_l {
                model.inject_rt_status(Arc::clone(&shared.cold.rt_status));
            }
            if let Some(ref mut model) = model_r {
                model.inject_rt_status(Arc::clone(&shared.cold.rt_status));
            }

            let silence_hyst = DynamicHysteresis::new();
            let mono_hyst = DynamicHysteresis::new();

            // Adaptive prewarm seed (U-02 / F-PR-10): measure the installed
            // model's per-sample inference cost here, off the audio thread
            // (allocations allowed) so the first amortization window after
            // activation is sized from real hardware instead of the conservative
            // family table. The frozen-table latch is set in debug builds (the
            // measured cost there is not release-representative). Continuous model
            // swaps update this scalar from the load payload.
            let prewarm_static = !prewarm_policy::ADAPTIVE_COST_ENABLED;
            let prewarm_cost_ns_per_sample = if prewarm_static {
                None
            } else {
                // Cap the calibration block at the negotiated host quantum so
                // it never exceeds the model's `max_buffer_size` (a smaller
                // block is otherwise silently truncated by composed models and
                // under-measures the per-sample cost).
                let measure_frames =
                    prewarm_policy::PREWARM_MEASURE_FRAMES.min((host_buffer as usize).max(1));
                model_l
                    .as_mut()
                    .map(|m| m.measure_cost_ns_per_sample(measure_frames))
            };

            // 4. Smoother initialization (Sample-Accurate)
            // Warm reset from shared atomics to avoid transient jump on reactivation
            // when gain differs from 0 dB (1.0).
            let gain_lut = get_gain_lut();
            let input_db = f32::from_bits(shared.ui_to_rt.param_input_gain.load(Ordering::Relaxed));
            let output_db =
                f32::from_bits(shared.ui_to_rt.param_output_gain.load(Ordering::Relaxed));
            let smoother_in = ParamSmoother::new(
                gain_lut.db_to_linear(input_db),
                audio_config.sample_rate as f32,
                20.0,
            );
            let smoother_out = ParamSmoother::new(
                gain_lut.db_to_linear(output_db),
                audio_config.sample_rate as f32,
                20.0,
            );
            let cal_config = shared.calibration_config();
            let cab_bypass =
                bypass_u32_to_bool(shared.ui_to_rt.param_cab_bypass.load(Ordering::Relaxed));
            let gate_mode = shared.gate_mode();
            let musical_gate = crate::clap::processor::dsp::musical_gate::MusicalNoiseGate::new(
                audio_config.sample_rate.round().clamp(1.0, u32::MAX as f64) as u32,
            );
            let has_cal_meta = model_cal_meta.input_level_dbu.is_some()
                || model_cal_meta.output_level_dbu.is_some()
                || model_cal_meta.loudness.is_some();
            let (model_input_mult_adj, model_output_mult_adj) = if has_cal_meta {
                crate::clap::extensions::calibration::compute_calibration_multipliers(
                    model_cal_meta,
                    cal_config,
                )
            } else {
                (model_input_mult_adj, model_output_mult_adj)
            };
            let smoother_cal_in =
                ParamSmoother::new(model_input_mult_adj, audio_config.sample_rate as f32, 20.0);
            let smoother_cal_out =
                ParamSmoother::new(model_output_mult_adj, audio_config.sample_rate as f32, 20.0);

            // Build an atomic snapshot for params that drive smoothers
            // from UiToRt atomics BEFORE constructing the RT processor. This
            // guarantees that self.params starts in sync with the smoother state
            // (both read from the same gain atomics) — no one-block window where
            // params.input_gain_db lags behind smoother_in.target.
            //
            // Non-smoother params (gate, bypass, adaptive_compute, etc.) are left
            // at their defaults and will be synced on the first process events
            // call (SPSC drain or GUI generation guard). Full-param snapshot
            // would trigger AdaptiveCompute::set_mode log on audio thread when
            // values differ from SPSC-delivered state.
            let mut params = RtProcessingParams::default();
            params.input_gain_db = input_db;
            params.output_gain_db = output_db;
            params.oversample = os_factor;
            debug_assert!(
                (smoother_in.current_value() - gain_lut.db_to_linear(params.input_gain_db)).abs()
                    < f32::EPSILON * 10.0,
                "Invariant: smoother_in must start from the same input_gain_db atomics"
            );
            debug_assert!(
                (smoother_out.current_value() - gain_lut.db_to_linear(params.output_gain_db)).abs()
                    < f32::EPSILON * 10.0,
                "Invariant: smoother_out must start from the same output_gain_db atomics"
            );

            // 5. Report initial latency to shared state.
            // The streaming adapter owns the resampler and zero-primes exactly
            // `latency_samples()` host samples, so its declared latency is the
            // authoritative PDC value.
            let mut initial_latency = stream.latency_samples();
            initial_latency += os_l.latency_samples() as u32;
            let cab_latency = cab_slot.as_ref().map_or(0, CabSlotDsp::latency_samples);
            initial_latency += cab_latency;
            // Publish the installed latency contributions so the main thread
            // can decide continuous-vs-staged swaps (same latency ⇒ continuous,
            // different ⇒ staged + restart) for future model/IR loads. Written
            // at the exact instant the resources land.
            shared
                .cold
                .current_stream_latency
                .store(stream.latency_samples(), Ordering::Relaxed);
            shared
                .cold
                .current_cabsim_latency
                .store(cab_latency, Ordering::Relaxed);
            // Publish the CabSim tail telemetry on the restart install path
            // too — `cold_load_cabsim` only covers the continuous SPSC swap,
            // leaving the atomic at 0 (and the host tail extension wrong) after
            // a staged IR lands via `activate()`.
            shared.rt_to_ui.cabsim_tail_samples.store(
                cab_slot.as_ref().map_or(0, CabSlotDsp::tail_samples),
                Ordering::Relaxed,
            );
            shared
                .rt_to_ui
                .current_latency
                .store(initial_latency, Ordering::Relaxed);
            shared.cold.sample_rate.store(host_rate, Ordering::Relaxed);
            shared
                .cold
                .buffer_size
                .store(audio_config.max_frames_count, Ordering::Relaxed);

            // Flush any model deferred by load_model() (state-restore-before-activate).
            // Now that sample_rate and buffer_size are published, build resampler
            // and streaming buffer for the exact newly negotiated audio config.
            main_thread.flush_pending_model()?;

            // Defuse the rollback guard — transfers SPSC channel
            // ownership back for processor construction. Guard Drop is now a no-op.
            let channels = rollback.defuse()?;

            // The engine structural-swap scheduler owns the command consumer
            // end-to-end: the 3-phase drain (deferred resolution, bounded
            // coalescing, budgeted apply-or-park) runs on this single ring
            // with the consumer's historical tuning (CMD_SWAP_TUNABLES).
            let cmd_drain = RtSwapDrain::new(
                CommandConsumer::new(channels.param_rx, &shared.cold.cmd_last_ack),
                CMD_SWAP_TUNABLES,
            );
            let slimmable_drain = RtSwapDrain::new(channels.slimmable_rx, SLIMMABLE_SWAP_TUNABLES);

            // Pre-allocate the circular dry delay line sized for the maximum
            // possible DSP latency (cab-sim partition == host block size, plus
            // the resampler/oversampler group-delay headroom). Its delay tracks
            // `initial_latency` and is kept in sync whenever a
            // latency-affecting resource is swapped (recompute_effective_latency).
            // The two engine `DelayLine<f32>` rings are allocated here in
            // `activate()` — the plugin's single documented allocation site.
            let dry_delay_capacity = (audio_config.max_frames_count as usize)
                .max(MAX_RESAMP_BUF)
                .saturating_add(DRY_DELAY_MAX_EXTRA);
            let dry_delay = DryDelayLine::new(dry_delay_capacity, initial_latency as usize);
            let cab_dry_delay = DryDelayLine::new(dry_delay_capacity, cab_latency as usize);

            let recovered_commands = std::mem::take(
                &mut *shared
                    .cold
                    .recovered_commands
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()),
            );

            Ok(NamClapProcessor {
                model_l,
                model_r,
                model_generation,
                cab_slot,
                cab_bypass,
                cab_needs_reset: false,
                cab_bypass_xfade: BypassCrossfader::new(cab_bypass),
                cab_dry_delay,
                recovered_commands,
                resampler,
                stream,
                os_l,
                os_r,
                params,
                applied_os_factor: os_factor,
                buf_host_l,
                buf_host_r,
                buf_mid_l,
                buf_mid_r,
                buf_out_l,
                buf_out_r,
                buf_os_in_l,
                buf_os_in_r,
                buf_os_model_l,
                buf_os_model_r,
                buf_xfade_dry_l,
                buf_xfade_dry_r,
                buf_cab_dry_l,
                buf_cab_dry_r,
                buf_cab_nam_in_l,
                buf_cab_nam_in_r,
                buf_cab_zero_l,
                buf_cab_zero_r,
                buf_xfd_scratch_l,
                buf_xfd_scratch_r,
                silence_hyst,
                mono_hyst,
                process_mono: true,
                scheduled_events: Vec::with_capacity(4096),
                scheduled_events_sorted: true,
                bypass_xfade: BypassCrossfader::new(params.bypass),
                dry_delay,
                rt_status: Arc::clone(&shared.cold.rt_status),
                adaptive_compute: AdaptiveCompute::new(
                    neural_amp_modeler_rs::common::params::AdaptiveComputeMode::Conservative,
                ),
                shared,
                smoother_in,
                smoother_out,
                smoother_cal_in,
                smoother_cal_out,
                cal_config,
                model_cal_meta,
                model_input_mult_adj,
                model_output_mult_adj,
                in_rms_ewma_l: 0.0,
                in_rms_ewma_r: 0.0,
                out_rms_ewma_l: 0.0,
                out_rms_ewma_r: 0.0,
                cmd_drain,
                gc_tx: channels.gc_tx,
                slimmable_drain,
                gc_overflow: Arc::clone(&shared.cold.gc_overflow),
                parking_lot: Default::default(),
                parking_lot_dirty: std::sync::atomic::AtomicBool::new(false),
                mod_input_gain: 0.0,
                mod_output_gain: 0.0,
                mod_gate_thresh: 0.0,
                cached_threshold_open_sq: 0.0,
                cached_threshold_close_sq: 0.0,
                cached_gate_params: GateParams::default(),
                gate_dirty: true,
                gate_mode,
                musical_gate,
                rt_thread_setup_done: false,
                // Fresh models arrive converged from the off-RT loader and the
                // eager drain above finishes any deactivate-interrupted pass,
                // so activation never arms the amortization window.
                prewarm_pending: false,
                prewarm_cost_ns_per_sample,
                prewarm_static,
                prewarm_zero_budget_count: 0,
                last_seen_generation: 0,
                max_frames_count: audio_config.max_frames_count as usize,
                min_frames_count: audio_config.min_frames_count as usize,
                sample_rate: audio_config.sample_rate,
                last_render_mode: 0,
                realtime_activation:
                    neural_amp_modeler_rs::common::params::ActivationPrecision::Standard,
                gain_lut: get_gain_lut(),
                cached_effective_latency: initial_latency,
                host,
                poisoned: false,
                dsp_load_ewma: 0.0,
                cached_n_samples: 0,
                cached_sample_rate: 0,
                cached_inv_budget_permille: 0.0,
                cached_ewma_alpha: 0.0,
                cached_threshold_ns: u64::MAX,
                cached_budget_us: 0,
            })
        }));
        shared
            .cold
            .rt_status
            .clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_PROCESSOR_POISONED);
        match result {
            Ok(r) => r,
            Err(err) => Err(panic_to_error(err)),
        }
    }
}
