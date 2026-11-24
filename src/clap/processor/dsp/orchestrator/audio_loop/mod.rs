// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Sub-block processing loop and streaming neural audio execution for NAM-Plug.

mod cabinet;
mod gain;
mod output;

pub(crate) use cabinet::CabinetStage;
pub(crate) use gain::apply_iir_gain_ramp_sub_block;
pub(crate) use output::{
    compute_peak_and_sum_sq, copy_delayed_dry_to_output, copy_output_from_sub_block,
    copy_silence_to_output,
};
#[cfg(any(test, feature = "testing"))]
pub(crate) use output::{
    copy_output_from_sub_block_fused_scalar, copy_output_from_sub_block_two_pass_ref,
};

use cabinet::{blend_cabinet_crossfade, process_cabinet_stage};
use gain::{
    advance_smoother_if_active, apply_gain_ramp_with_steps, apply_output_stage_calibrated,
    resolve_input_cal_mult,
};

use crate::clap::extensions::cabinet::CabSlotDsp;
use crate::clap::extensions::gate_mode::GateMode;
use crate::clap::processor::dsp::dry_delay::DryDelayLine;
use crate::clap::processor::dsp::musical_gate::MusicalNoiseGate;
use crate::clap::processor::state::BypassCrossfader;
use neural_amp_modeler_rs::dsp::gate::{DynamicHysteresis, GateState};
use neural_amp_modeler_rs::dsp::pipeline::{
    DspPipelineContext, apply_input_stage, apply_input_stage_mono, run_inference_streaming,
};
use neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer;
use neural_amp_modeler_rs::dsp::smoother::ParamSmoother;

use super::worker::GainStep;

/// Disjoint scratch-buffer borrows for one sub-block of the DSP pipeline.
pub(crate) struct SubBlockScratch<'a> {
    /// Host input copies staged for DSP (indexed by `offset..offset + n`).
    pub buf_host_l: &'a mut [f32],
    /// Host input copies staged for DSP (indexed by `offset..offset + n`).
    pub buf_host_r: &'a mut [f32],
    /// Wet model output scratch (indexed `0..n_out`).
    pub buf_out_l: &'a mut [f32],
    /// Wet model output scratch (indexed `0..n_out`).
    pub buf_out_r: &'a mut [f32],
    /// Streaming-resampler oversample input scratch.
    pub buf_os_in_l: &'a mut [f32],
    /// Streaming-resampler oversample input scratch.
    pub buf_os_in_r: &'a mut [f32],
    /// Streaming-resampler oversample model scratch.
    pub buf_os_model_l: &'a mut [f32],
    /// Streaming-resampler oversample model scratch.
    pub buf_os_model_r: &'a mut [f32],
    /// Latency-compensated dry capture (bypass source, crossfade blend).
    pub buf_xfade_dry_l: &'a mut [f32],
    /// Latency-compensated dry capture (bypass source, crossfade blend).
    pub buf_xfade_dry_r: &'a mut [f32],
    /// Inference scratch shared with the crossfade wet leg.
    pub buf_xfd_scratch_l: &'a mut [f32],
    /// Inference scratch shared with the crossfade wet leg.
    pub buf_xfd_scratch_r: &'a mut [f32],
}

/// Telemetry and processing metrics returned by [`process_sub_block`].
pub(crate) struct SubBlockMetrics {
    /// Number of output samples rendered into host buffer.
    pub n_out: usize,
    /// Noise gate state at the end of this sub-block.
    pub gate_state: GateState,
    /// Post-gain input absolute peak on Left channel.
    pub in_peak_l: f32,
    /// Post-gain input absolute peak on Right channel.
    pub in_peak_r: f32,
    /// Sum of squared post-gain input samples on Left channel (for block RMS).
    pub in_sum_sq_l: f32,
    /// Sum of squared post-gain input samples on Right channel (for block RMS).
    pub in_sum_sq_r: f32,
    /// Output absolute peak on Left channel.
    pub out_peak_l: f32,
    /// Output absolute peak on Right channel.
    pub out_peak_r: f32,
    /// Sum of squared output samples on Left channel (for block RMS).
    pub out_sum_sq_l: f32,
    /// Sum of squared output samples on Right channel (for block RMS).
    pub out_sum_sq_r: f32,
}

/// Processes an audio sub-block (chunked up to MAX_RESAMP_BUF) through input staging,
/// streaming neural inference, gate hysteresis, output gain, and latency-compensated dry delay blending.
#[inline(never)]
#[expect(
    clippy::too_many_arguments,
    reason = "explicit scalar tail beside the grouped SubBlockScratch: ctx/engine borrows and control scalars stay outside the plugin-owned scratch struct by design"
)]
pub(crate) fn process_sub_block(
    offset: usize,
    n_samples: usize,
    out_l: &mut Option<&mut [f32]>,
    out_r: &mut Option<&mut [f32]>,
    output_offset: usize,
    ctx: &mut DspPipelineContext<'_>,
    stream: &mut StreamingResampleBuffer,
    bypass: bool,
    process_mono: bool,
    crossfader: &mut BypassCrossfader,
    dry_delay: &mut DryDelayLine,
    scratch: &mut SubBlockScratch<'_>,
    cab: &mut CabinetStage<'_>,
    input_clipped: &mut bool,
    smoother_in: &mut ParamSmoother,
    smoother_out: &mut ParamSmoother,
    smoother_cal_in: &mut ParamSmoother,
    smoother_cal_out: &mut ParamSmoother,
    shared_sample_rate: u32,
    gain_steps: &[GainStep],
    gate_mode: GateMode,
    musical_gate: &mut MusicalNoiseGate,
) -> SubBlockMetrics {
    // 1. Oversize-block guard: split buffers exceeding MAX_RESAMP_BUF into bounded chunks.
    if n_samples > neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF {
        core::hint::cold_path();
        return process_sub_block_chunked(
            offset,
            n_samples,
            out_l,
            out_r,
            output_offset,
            ctx,
            stream,
            bypass,
            process_mono,
            crossfader,
            dry_delay,
            scratch,
            cab,
            input_clipped,
            smoother_in,
            smoother_out,
            smoother_cal_in,
            smoother_cal_out,
            shared_sample_rate,
            gate_mode,
            musical_gate,
        );
    }

    // 2. Feed the latency-compensated dry delay ring every sub-block so bypass
    //    and crossfade blends always stay phase-aligned with the wet chain.
    let SubBlockScratch {
        buf_host_l,
        buf_host_r,
        buf_out_l: _,
        buf_out_r: _,
        buf_os_in_l: _,
        buf_os_in_r: _,
        buf_os_model_l: _,
        buf_os_model_r: _,
        buf_xfade_dry_l,
        buf_xfade_dry_r,
        buf_xfd_scratch_l: _,
        buf_xfd_scratch_r: _,
    } = scratch;
    dry_delay.process_block(
        &buf_host_l[offset..offset + n_samples],
        &buf_host_r[offset..offset + n_samples],
        &mut buf_xfade_dry_l[..n_samples],
        &mut buf_xfade_dry_r[..n_samples],
        n_samples,
        process_mono,
    );

    // 3. Click-free master bypass transitions: render both wet and dry legs and blend.
    if crossfader.active {
        core::hint::cold_path();
        return process_crossfade_sub_block(
            offset,
            n_samples,
            out_l,
            out_r,
            output_offset,
            ctx,
            stream,
            process_mono,
            crossfader,
            scratch,
            cab,
            input_clipped,
            smoother_in,
            smoother_out,
            smoother_cal_in,
            smoother_cal_out,
            shared_sample_rate,
            gate_mode,
            musical_gate,
        );
    }

    // 4. Steady-state master bypass: advance calibration smoothers, meter raw input,
    //    and copy the latency-compensated dry buffer directly to the host output.
    if bypass {
        if gate_mode == GateMode::Musical {
            musical_gate.reset();
        }
        advance_smoother_if_active(smoother_cal_in, n_samples);
        advance_smoother_if_active(smoother_cal_out, n_samples);
        let (in_peak_l, in_sum_sq_l) =
            compute_peak_and_sum_sq(&buf_host_l[offset..offset + n_samples]);
        let (in_peak_r, in_sum_sq_r) = if process_mono {
            (in_peak_l, in_sum_sq_l)
        } else {
            compute_peak_and_sum_sq(&buf_host_r[offset..offset + n_samples])
        };
        let (out_peak_l, out_peak_r, out_sum_sq_l, out_sum_sq_r) = copy_delayed_dry_to_output(
            out_l,
            out_r,
            &mut buf_xfade_dry_l[..n_samples],
            &mut buf_xfade_dry_r[..n_samples],
            output_offset,
            process_mono,
        );
        return SubBlockMetrics {
            n_out: n_samples,
            gate_state: GateState::Open,
            in_peak_l,
            in_peak_r,
            in_sum_sq_l,
            in_sum_sq_r,
            out_peak_l,
            out_peak_r,
            out_sum_sq_l,
            out_sum_sq_r,
        };
    }

    // 5. Input trim & pre-model metering (post-trim, pre-gate/pre-calibration).
    apply_gain_ramp_with_steps(
        smoother_in,
        buf_host_l,
        buf_host_r,
        offset,
        n_samples,
        gain_steps,
        true,
        true,
        input_clipped,
        process_mono,
    );

    let (in_peak_l, in_sum_sq_l) = compute_peak_and_sum_sq(&buf_host_l[offset..offset + n_samples]);
    let (in_peak_r, in_sum_sq_r) = if process_mono {
        (in_peak_l, in_sum_sq_l)
    } else {
        compute_peak_and_sum_sq(&buf_host_r[offset..offset + n_samples])
    };

    // In Musical mode, the downward expander's envelope detector tracks the
    // pre-calibration signal while its gain reduction applies post-model/pre-cab.
    if gate_mode == GateMode::Musical {
        musical_gate.process_trigger(
            &buf_host_l[offset..offset + n_samples],
            &buf_host_r[offset..offset + n_samples],
            n_samples,
            process_mono,
        );
    }

    // 6. Silence-gate evaluation & denormal dither injection.
    let gate_state = if process_mono {
        apply_input_stage_mono(&mut buf_host_l[offset..offset + n_samples], n_samples, ctx)
    } else {
        apply_input_stage(
            &mut buf_host_l[offset..offset + n_samples],
            &mut buf_host_r[offset..offset + n_samples],
            n_samples,
            ctx,
        )
    };

    // Fast path when Silence gate is closed: skip neural inference and drain CabSim tail.
    if gate_state == GateState::Closed {
        core::hint::cold_path();
        advance_smoother_if_active(smoother_cal_in, n_samples);
        let (out_peak_l, out_peak_r, out_sum_sq_l, out_sum_sq_r) = process_tail_drain(
            n_samples,
            out_l,
            out_r,
            output_offset,
            ctx,
            process_mono,
            smoother_out,
            smoother_cal_out,
            scratch,
            cab,
            shared_sample_rate,
            gain_steps,
        );
        return SubBlockMetrics {
            n_out: n_samples,
            gate_state: GateState::Closed,
            in_peak_l,
            in_peak_r,
            in_sum_sq_l,
            in_sum_sq_r,
            out_peak_l,
            out_peak_r,
            out_sum_sq_l,
            out_sum_sq_r,
        };
    }

    // 7. Streaming neural inference (resampling + oversampling + model execution).
    let SubBlockScratch {
        buf_host_l,
        buf_host_r,
        buf_out_l,
        buf_out_r,
        buf_os_in_l,
        buf_os_in_r,
        buf_os_model_l,
        buf_os_model_r,
        buf_xfade_dry_l: _,
        buf_xfade_dry_r: _,
        buf_xfd_scratch_l,
        buf_xfd_scratch_r,
    } = scratch;
    ctx.input_gain_mult = resolve_input_cal_mult(
        smoother_cal_in,
        buf_host_l,
        buf_host_r,
        offset,
        n_samples,
        process_mono,
    );
    let n_out = run_inference_streaming(
        &buf_host_l[offset..offset + n_samples],
        &buf_host_r[offset..offset + n_samples],
        &mut buf_out_l[..n_samples],
        &mut buf_out_r[..n_samples],
        n_samples,
        ctx,
        stream,
        buf_os_in_l,
        buf_os_in_r,
        buf_os_model_l,
        buf_os_model_r,
        buf_xfd_scratch_l,
        buf_xfd_scratch_r,
    );

    // 8. Post-model stages: musical gate attenuation, cabinet slot (WAV IR or 1x1 NAM),
    //    output calibration/soft-degrade check, user output gain, and host copy.
    if gate_mode == GateMode::Musical {
        musical_gate.apply_gain(
            &mut buf_out_l[..n_out],
            &mut buf_out_r[..n_out],
            n_out,
            process_mono,
        );
    }

    process_cabinet_stage(
        cab,
        &mut buf_out_l[..n_out],
        &mut buf_out_r[..n_out],
        n_out,
        process_mono,
        ctx.rt_status,
    );

    apply_output_stage_calibrated(
        &mut buf_out_l[..n_out],
        &mut buf_out_r[..n_out],
        n_out,
        smoother_cal_out,
        ctx.silence_hysteresis,
        ctx.rt_status,
        process_mono,
        ctx.adaptive,
        shared_sample_rate,
    );

    apply_gain_ramp_with_steps(
        smoother_out,
        buf_out_l,
        buf_out_r,
        0,
        n_out,
        gain_steps,
        false,
        false,
        &mut false,
        *ctx.process_mono,
    );

    let (out_peak_l, out_peak_r, out_sum_sq_l, out_sum_sq_r) = copy_output_from_sub_block(
        out_l,
        out_r,
        buf_out_l,
        buf_out_r,
        n_out,
        output_offset,
        process_mono,
    );

    SubBlockMetrics {
        n_out,
        gate_state,
        in_peak_l,
        in_peak_r,
        in_sum_sq_l,
        in_sum_sq_r,
        out_peak_l,
        out_peak_r,
        out_sum_sq_l,
        out_sum_sq_r,
    }
}

/// Cold overflow chunker for host blocks larger than `MAX_RESAMP_BUF`.
#[cold]
#[inline(never)]
#[expect(
    clippy::too_many_arguments,
    reason = "cold overflow chunker mirrors the process_sub_block signature by construction (iterative splitter)"
)]
fn process_sub_block_chunked(
    offset: usize,
    n_samples: usize,
    out_l: &mut Option<&mut [f32]>,
    out_r: &mut Option<&mut [f32]>,
    output_offset: usize,
    ctx: &mut DspPipelineContext<'_>,
    stream: &mut StreamingResampleBuffer,
    bypass: bool,
    process_mono: bool,
    crossfader: &mut BypassCrossfader,
    dry_delay: &mut DryDelayLine,
    scratch: &mut SubBlockScratch<'_>,
    cab: &mut CabinetStage<'_>,
    input_clipped: &mut bool,
    smoother_in: &mut ParamSmoother,
    smoother_out: &mut ParamSmoother,
    smoother_cal_in: &mut ParamSmoother,
    smoother_cal_out: &mut ParamSmoother,
    shared_sample_rate: u32,
    gate_mode: GateMode,
    musical_gate: &mut MusicalNoiseGate,
) -> SubBlockMetrics {
    let mut total_out = 0;
    let mut last_gate = GateState::Open;
    let mut curr_offset = offset;
    let mut curr_out_offset = output_offset;
    let mut remaining = n_samples;
    let mut in_peak_l = 0.0f32;
    let mut in_peak_r = 0.0f32;
    let mut in_sum_sq_l = 0.0f32;
    let mut in_sum_sq_r = 0.0f32;
    let mut out_peak_l = 0.0f32;
    let mut out_peak_r = 0.0f32;
    let mut out_sum_sq_l = 0.0f32;
    let mut out_sum_sq_r = 0.0f32;

    while remaining > 0 {
        let chunk = remaining.min(neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF);
        let m = process_sub_block(
            curr_offset,
            chunk,
            out_l,
            out_r,
            curr_out_offset,
            ctx,
            stream,
            bypass,
            process_mono,
            crossfader,
            dry_delay,
            scratch,
            cab,
            input_clipped,
            smoother_in,
            smoother_out,
            smoother_cal_in,
            smoother_cal_out,
            shared_sample_rate,
            &[],
            gate_mode,
            musical_gate,
        );
        total_out += m.n_out;
        last_gate = m.gate_state;
        in_peak_l = in_peak_l.max(m.in_peak_l);
        in_peak_r = in_peak_r.max(m.in_peak_r);
        in_sum_sq_l += m.in_sum_sq_l;
        in_sum_sq_r += m.in_sum_sq_r;
        out_peak_l = out_peak_l.max(m.out_peak_l);
        out_peak_r = out_peak_r.max(m.out_peak_r);
        out_sum_sq_l += m.out_sum_sq_l;
        out_sum_sq_r += m.out_sum_sq_r;
        curr_offset += chunk;
        curr_out_offset += m.n_out;
        remaining -= chunk;
    }
    SubBlockMetrics {
        n_out: total_out,
        gate_state: last_gate,
        in_peak_l,
        in_peak_r,
        in_sum_sq_l,
        in_sum_sq_r,
        out_peak_l,
        out_peak_r,
        out_sum_sq_l,
        out_sum_sq_r,
    }
}

/// Shared kernel of both gate-closed paths: drains up to `n_samples` tail
/// samples of the cab-sim IR ring-out into `buf_out_*` and applies the
/// unity-gate output stage plus the output gain ramp to the drained head.
#[cold]
#[inline(never)]
#[expect(
    clippy::too_many_arguments,
    reason = "gate-closed drain kernel fans out to ctx/smoother/scratch/steps with no further grouping available without unsafe"
)]
fn drain_tail_into(
    n_samples: usize,
    ctx: &mut DspPipelineContext<'_>,
    process_mono: bool,
    smoother_out: &mut ParamSmoother,
    smoother_cal_out: &mut ParamSmoother,
    scratch: &mut SubBlockScratch<'_>,
    cab: &mut CabinetStage<'_>,
    shared_sample_rate: u32,
    gain_steps: &[GainStep],
) -> usize {
    let SubBlockScratch {
        buf_host_l: _,
        buf_host_r: _,
        buf_out_l,
        buf_out_r,
        buf_os_in_l: _,
        buf_os_in_r: _,
        buf_os_model_l: _,
        buf_os_model_r: _,
        buf_xfade_dry_l: _,
        buf_xfade_dry_r: _,
        buf_xfd_scratch_l: _,
        buf_xfd_scratch_r: _,
    } = scratch;
    let Some(ref mut slot) = *cab.cab_slot else {
        cab.cab_bypass_xfade.active = false;
        return 0;
    };
    cab.cab_dry_delay.process_block(
        &cab.buf_cab_zero_l[..n_samples],
        &cab.buf_cab_zero_r[..n_samples],
        &mut cab.buf_cab_dry_l[..n_samples],
        &mut cab.buf_cab_dry_r[..n_samples],
        n_samples,
        process_mono,
    );
    if cab.cab_bypass && !cab.cab_bypass_xfade.active {
        *cab.cab_needs_reset = true;
        return 0;
    }
    let drain = n_samples.min(slot.remaining_tail_samples(process_mono));

    if drain > 0 {
        match slot {
            CabSlotDsp::WavIr(pair) => {
                if !process_mono {
                    pair.drain_tail_stereo(
                        &mut buf_out_l[..drain],
                        &mut buf_out_r[..drain],
                        Some(ctx.rt_status),
                    );
                } else {
                    pair.l
                        .drain_tail(&mut buf_out_l[..drain], Some(ctx.rt_status));
                }
            }
            CabSlotDsp::NamModel(nam) => {
                let _ = nam.drain_tail(
                    &cab.buf_cab_zero_l[..drain],
                    &cab.buf_cab_zero_r[..drain],
                    &mut buf_out_l[..drain],
                    &mut buf_out_r[..drain],
                    drain,
                    process_mono,
                );
            }
        }
        if cab.cab_bypass_xfade.active {
            blend_cabinet_crossfade(
                cab.cab_bypass_xfade,
                &cab.buf_cab_dry_l[..drain],
                &cab.buf_cab_dry_r[..drain],
                &mut buf_out_l[..drain],
                &mut buf_out_r[..drain],
                drain,
                process_mono,
            );
            if !cab.cab_bypass_xfade.active && cab.cab_bypass {
                *cab.cab_needs_reset = true;
            }
        }
        apply_output_stage_calibrated(
            &mut buf_out_l[..drain],
            &mut buf_out_r[..drain],
            drain,
            smoother_cal_out,
            &mut DynamicHysteresis::new(),
            ctx.rt_status,
            *ctx.process_mono,
            ctx.adaptive,
            shared_sample_rate,
        );
        if drain < n_samples {
            advance_smoother_if_active(smoother_cal_out, n_samples - drain);
        }
        apply_gain_ramp_with_steps(
            smoother_out,
            buf_out_l,
            buf_out_r,
            0,
            drain,
            gain_steps,
            false,
            false,
            &mut false,
            process_mono,
        );
    } else {
        advance_smoother_if_active(smoother_cal_out, n_samples);
        apply_gain_ramp_with_steps(
            smoother_out,
            buf_out_l,
            buf_out_r,
            0,
            0,
            gain_steps,
            false,
            false,
            &mut false,
            process_mono,
        );
    }

    if drain < n_samples {
        buf_out_l[drain..n_samples].fill(0.0);
        buf_out_r[drain..n_samples].fill(0.0);
    }

    drain
}

/// Drains the cab-sim IR tail ring-out after the noise gate closes.
#[cold]
#[inline(never)]
#[expect(
    clippy::too_many_arguments,
    reason = "gate-closed drain fans out to out_*/ctx/smoother/scratch with no further grouping available without unsafe"
)]
fn process_tail_drain(
    n_samples: usize,
    out_l: &mut Option<&mut [f32]>,
    out_r: &mut Option<&mut [f32]>,
    output_offset: usize,
    ctx: &mut DspPipelineContext<'_>,
    process_mono: bool,
    smoother_out: &mut ParamSmoother,
    smoother_cal_out: &mut ParamSmoother,
    scratch: &mut SubBlockScratch<'_>,
    cab: &mut CabinetStage<'_>,
    shared_sample_rate: u32,
    gain_steps: &[GainStep],
) -> (f32, f32, f32, f32) {
    let drained = drain_tail_into(
        n_samples,
        ctx,
        process_mono,
        smoother_out,
        smoother_cal_out,
        scratch,
        cab,
        shared_sample_rate,
        gain_steps,
    );

    if drained == 0 {
        return copy_silence_to_output(out_l, out_r, output_offset, n_samples, process_mono);
    }

    let SubBlockScratch {
        buf_host_l: _,
        buf_host_r: _,
        buf_out_l,
        buf_out_r,
        buf_os_in_l: _,
        buf_os_in_r: _,
        buf_os_model_l: _,
        buf_os_model_r: _,
        buf_xfade_dry_l: _,
        buf_xfade_dry_r: _,
        buf_xfd_scratch_l: _,
        buf_xfd_scratch_r: _,
    } = scratch;
    copy_output_from_sub_block(
        out_l,
        out_r,
        buf_out_l,
        buf_out_r,
        n_samples,
        output_offset,
        process_mono,
    )
}

/// Wet pipeline for sub-blocks under an active bypass crossfade.
#[cold]
#[inline(never)]
#[expect(
    clippy::too_many_arguments,
    reason = "cold wet-leg fans out to out_*/ctx/stream/crossfader/scratch/smoothers/scalars; scalars stay scalar so ctx and engine-owned borrows keep their domain (R2)"
)]
fn process_crossfade_sub_block(
    offset: usize,
    n_samples: usize,
    out_l: &mut Option<&mut [f32]>,
    out_r: &mut Option<&mut [f32]>,
    output_offset: usize,
    ctx: &mut DspPipelineContext<'_>,
    stream: &mut StreamingResampleBuffer,
    process_mono: bool,
    crossfader: &mut BypassCrossfader,
    scratch: &mut SubBlockScratch<'_>,
    cab: &mut CabinetStage<'_>,
    input_clipped: &mut bool,
    smoother_in: &mut ParamSmoother,
    smoother_out: &mut ParamSmoother,
    smoother_cal_in: &mut ParamSmoother,
    smoother_cal_out: &mut ParamSmoother,
    shared_sample_rate: u32,
    gate_mode: GateMode,
    musical_gate: &mut MusicalNoiseGate,
) -> SubBlockMetrics {
    let dry_n = n_samples.min(scratch.buf_xfade_dry_l.len());
    debug_assert_eq!(
        dry_n, n_samples,
        "dry delay output must cover the full sub-block (n_samples={n_samples})"
    );

    apply_iir_gain_ramp_sub_block(
        smoother_in,
        scratch.buf_host_l,
        scratch.buf_host_r,
        offset,
        n_samples,
        true,
        input_clipped,
        process_mono,
    );

    let (in_peak_l, in_sum_sq_l) =
        compute_peak_and_sum_sq(&scratch.buf_host_l[offset..offset + n_samples]);
    let (in_peak_r, in_sum_sq_r) = if process_mono {
        (in_peak_l, in_sum_sq_l)
    } else {
        compute_peak_and_sum_sq(&scratch.buf_host_r[offset..offset + n_samples])
    };

    if gate_mode == GateMode::Musical {
        musical_gate.process_trigger(
            &scratch.buf_host_l[offset..offset + n_samples],
            &scratch.buf_host_r[offset..offset + n_samples],
            n_samples,
            process_mono,
        );
    }

    let gate_state = if process_mono {
        apply_input_stage_mono(
            &mut scratch.buf_host_l[offset..offset + n_samples],
            n_samples,
            ctx,
        )
    } else {
        apply_input_stage(
            &mut scratch.buf_host_l[offset..offset + n_samples],
            &mut scratch.buf_host_r[offset..offset + n_samples],
            n_samples,
            ctx,
        )
    };

    let n_out = if gate_state == GateState::Closed {
        advance_smoother_if_active(smoother_cal_in, n_samples);
        let _drained = drain_tail_into(
            n_samples,
            ctx,
            process_mono,
            smoother_out,
            smoother_cal_out,
            scratch,
            cab,
            shared_sample_rate,
            &[],
        );
        n_samples
    } else {
        ctx.input_gain_mult = resolve_input_cal_mult(
            smoother_cal_in,
            scratch.buf_host_l,
            scratch.buf_host_r,
            offset,
            n_samples,
            process_mono,
        );
        let n_o = run_inference_streaming(
            &scratch.buf_host_l[offset..offset + n_samples],
            &scratch.buf_host_r[offset..offset + n_samples],
            &mut scratch.buf_out_l[..n_samples],
            &mut scratch.buf_out_r[..n_samples],
            n_samples,
            ctx,
            stream,
            scratch.buf_os_in_l,
            scratch.buf_os_in_r,
            scratch.buf_os_model_l,
            scratch.buf_os_model_r,
            scratch.buf_xfd_scratch_l,
            scratch.buf_xfd_scratch_r,
        );

        if gate_mode == GateMode::Musical {
            musical_gate.apply_gain(
                &mut scratch.buf_out_l[..n_o],
                &mut scratch.buf_out_r[..n_o],
                n_o,
                process_mono,
            );
        }

        process_cabinet_stage(
            cab,
            &mut scratch.buf_out_l[..n_o],
            &mut scratch.buf_out_r[..n_o],
            n_o,
            process_mono,
            ctx.rt_status,
        );

        apply_output_stage_calibrated(
            &mut scratch.buf_out_l[..n_o],
            &mut scratch.buf_out_r[..n_o],
            n_o,
            smoother_cal_out,
            ctx.silence_hysteresis,
            ctx.rt_status,
            *ctx.process_mono,
            ctx.adaptive,
            shared_sample_rate,
        );

        apply_iir_gain_ramp_sub_block(
            smoother_out,
            scratch.buf_out_l,
            scratch.buf_out_r,
            0,
            n_o,
            false,
            &mut false,
            process_mono,
        );

        n_o
    };

    let n_xfade_raw = n_out.min(crossfader.remaining);
    let n_xfade = n_xfade_raw.min(dry_n);
    debug_assert!(
        n_xfade <= dry_n,
        "n_xfade ({n_xfade}) exceeded dry_n ({dry_n}) in bypass crossfade"
    );
    let step = crossfader.step;
    let mut mix = crossfader.mix;

    let (dry_l, dry_r, wet_l, wet_r) = {
        let s = &mut *scratch;
        (
            &s.buf_xfade_dry_l[..n_xfade],
            &s.buf_xfade_dry_r[..n_xfade],
            &mut s.buf_out_l[..n_xfade],
            &mut s.buf_out_r[..n_xfade],
        )
    };
    for i in 0..n_xfade {
        let d_l = dry_l[i];
        let d_r = dry_r[i];
        wet_l[i] = d_l + (wet_l[i] - d_l) * mix;
        wet_r[i] = d_r + (wet_r[i] - d_r) * mix;
        mix += step;
    }

    for i in n_xfade..n_xfade_raw {
        scratch.buf_out_l[i] *= mix;
        scratch.buf_out_r[i] *= mix;
        mix += step;
    }

    let final_mix = if crossfader.target { 0.0 } else { 1.0 };
    if (final_mix - 1.0f32).abs() > f32::EPSILON {
        for i in n_xfade_raw..n_out {
            if i < dry_n {
                scratch.buf_out_l[i] = scratch.buf_xfade_dry_l[i];
                scratch.buf_out_r[i] = scratch.buf_xfade_dry_r[i];
            } else {
                scratch.buf_out_l[i] = 0.0;
                scratch.buf_out_r[i] = 0.0;
            }
        }
    }

    crossfader.mix = mix.clamp(0.0, 1.0);
    crossfader.remaining = crossfader.remaining.saturating_sub(n_xfade_raw);
    if crossfader.remaining == 0 {
        crossfader.active = false;
        crossfader.mix = final_mix;
    }

    let (out_peak_l, out_peak_r, out_sum_sq_l, out_sum_sq_r) = copy_output_from_sub_block(
        out_l,
        out_r,
        scratch.buf_out_l,
        scratch.buf_out_r,
        n_out,
        output_offset,
        process_mono,
    );

    SubBlockMetrics {
        n_out,
        gate_state,
        in_peak_l,
        in_peak_r,
        in_sum_sq_l,
        in_sum_sq_r,
        out_peak_l,
        out_peak_r,
        out_sum_sq_l,
        out_sum_sq_r,
    }
}

#[cfg(test)]
#[path = "../audio_loop_test.rs"]
mod tests;
