// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Off-RT activation helpers.

use super::*;
use neural_amp_modeler_rs::models::NamModel;

/// Standardized pre-allocation error constructor for intermediate audio scratch buffers.
pub(super) fn buffer_prealloc_error(stage: &str, error: impl std::fmt::Debug) -> PluginError {
    static_plugin_error(
        errors::activation::BUFFER_PREALLOC_FAILED,
        format_args!("Failed to allocate intermediate {stage} buffers: {error:?}"),
    )
}

/// Rollback guard that restores staged restore and staged swap items back to
/// `NamClapMainThread` if `activate()` fails or early-returns with an error.
pub(super) struct StagedItemsRollbackGuard<'m, 'a> {
    pub(super) main_thread: &'m NamClapMainThread<'a>,
    pub(super) staged_restore: Option<StagedRestore>,
    pub(super) staged_swap: Option<StagedSwap>,
    pub(super) disarmed: bool,
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
pub(super) fn build_cab_sim_from_raw_samples(
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

pub(super) fn build_cab_slot_from_cold_storage(
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

pub(super) fn adapt_staged_model_for_host_config(
    m: &mut crate::clap::plugin::shared::LoadModelPayload,
    host_rate: u32,
    host_buffer: u32,
    kind_label: &str,
) -> Result<(), PluginError> {
    if m.new_stream.host_rate() == host_rate && m.new_stream.max_block() == host_buffer as usize {
        return Ok(());
    }
    log::info!(
        "NAM-Plug: adapting {kind_label} for new audio config \
         (staged: {} Hz/{} frames -> host: {} Hz/{} frames)",
        m.new_stream.host_rate(),
        m.new_stream.max_block(),
        host_rate,
        host_buffer
    );
    let model_rate = m.new_stream.model_rate();
    let new_res = NamResampler::new_simple_typed(host_rate, model_rate).map_err(|e| {
        static_plugin_error(
            errors::dsp_resources::RESAMPLER_BUILD_FAILED,
            format_args!("{e:?}"),
        )
    })?;
    let new_stream =
        crate::clap::plugin::build_stream_adapter(host_rate, model_rate, host_buffer as usize)
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
        m.prewarm_cost_ns = if prewarm_policy::ADAPTIVE_COST_ENABLED {
            let measure_frames =
                prewarm_policy::PREWARM_MEASURE_FRAMES.min((host_buffer as usize).max(1));
            Some(model.measure_cost_ns_per_sample(measure_frames))
        } else {
            None
        };
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
    Ok(())
}

pub(super) fn drain_model_prewarm_off_rt(
    model: &mut Option<Box<neural_amp_modeler_rs::models::StaticModel>>,
    label: &str,
) {
    let Some(model) = model else {
        return;
    };
    for _ in 0..64 {
        if model.prewarm_complete() {
            break;
        }
        model.prewarm_step(usize::MAX);
    }
    debug_assert!(
        model.prewarm_complete(),
        "{label} stabilization pass must complete in activate()"
    );
    if !model.prewarm_complete() {
        log::warn!("NAM-Plug: {label} stabilization pass did not complete during activate()");
    }
}
