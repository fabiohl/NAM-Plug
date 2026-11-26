// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::chunked::drain_tail_into;
use super::*;

#[cold]
#[inline(never)]
#[expect(
    clippy::too_many_arguments,
    reason = "cold wet-leg fans out to out_*/ctx/stream/crossfader/scratch/smoothers/scalars; scalars stay scalar so ctx and engine-owned borrows keep their domain (R2)"
)]
pub(super) fn process_crossfade_sub_block(
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
