// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Sub-block processing loop and streaming neural audio execution for NAM-Plug.

mod cabinet;
mod chunked;
mod crossfade;
mod gain;
mod output;

use chunked::{process_sub_block_chunked, process_tail_drain};
use crossfade::process_crossfade_sub_block;

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

use cabinet::process_cabinet_stage;
use gain::{
    advance_smoother_if_active, apply_gain_ramp_with_steps, apply_output_stage_calibrated,
    resolve_input_cal_mult,
};

pub(crate) use crate::clap::extensions::cabinet::CabSlotDsp;
use crate::clap::extensions::gate_mode::GateMode;
use crate::clap::processor::dsp::dry_delay::DryDelayLine;
use crate::clap::processor::dsp::musical_gate::MusicalNoiseGate;
use crate::clap::processor::state::BypassCrossfader;
pub(crate) use neural_amp_modeler_rs::dsp::gate::DynamicHysteresis;
use neural_amp_modeler_rs::dsp::gate::GateState;
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
    // The input stage's mono detector writes `ctx.process_mono`; every stage below
    // must consume that effective value rather than the block-start snapshot.
    let process_mono = *ctx.process_mono;

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
        process_mono,
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

#[cfg(test)]
#[path = "../audio_loop_test.rs"]
mod tests;
