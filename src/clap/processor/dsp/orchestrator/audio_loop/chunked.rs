// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::cabinet::blend_cabinet_crossfade;
use super::*;

#[cold]
#[inline(never)]
#[expect(
    clippy::too_many_arguments,
    reason = "cold overflow chunker mirrors the process_sub_block signature by construction (iterative splitter)"
)]
pub(super) fn process_sub_block_chunked(
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

#[cold]
#[inline(never)]
#[expect(
    clippy::too_many_arguments,
    reason = "gate-closed drain kernel fans out to ctx/smoother/scratch/steps with no further grouping available without unsafe"
)]
pub(super) fn drain_tail_into(
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

#[cold]
#[inline(never)]
#[expect(
    clippy::too_many_arguments,
    reason = "gate-closed drain fans out to out_*/ctx/smoother/scratch with no further grouping available without unsafe"
)]
pub(super) fn process_tail_drain(
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
