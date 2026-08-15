// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use crate::clap::processor::state::BypassCrossfader;
use neural_amp_modeler_rs::dsp::gate::GateState;
use neural_amp_modeler_rs::dsp::pipeline::{
    DspPipelineContext, apply_input_stage, apply_output_stage, run_inference,
};
use neural_amp_modeler_rs::dsp::smoother::ParamSmoother;
use neural_amp_modeler_rs::math::dsp::gain_lut::GainLUT;

#[inline(always)]
#[expect(clippy::too_many_arguments)]
pub(crate) fn process_sub_block(
    offset: usize,
    n_samples: usize,
    out_l: &mut Option<&mut [f32]>,
    out_r: &mut Option<&mut [f32]>,
    output_offset: usize,
    ctx: &mut DspPipelineContext<'_>,
    bypass: bool,
    process_mono: bool,
    crossfader: &mut BypassCrossfader,
    buf_xfade_dry_l: &mut [f32],
    buf_xfade_dry_r: &mut [f32],
    buf_xfd_scratch_l: &mut [f32],
    buf_xfd_scratch_r: &mut [f32],
    input_clipped: &mut bool,
    smoother_in: &mut ParamSmoother,
    smoother_out: &mut ParamSmoother,
    buf_host_l: &mut [f32],
    buf_host_r: &mut [f32],
    buf_mid_l: &mut [f32],
    buf_mid_r: &mut [f32],
    buf_out_l: &mut [f32],
    buf_out_r: &mut [f32],
    buf_model_l: &mut [f32],
    buf_model_r: &mut [f32],
    buf_os_in_l: &mut [f32],
    buf_os_in_r: &mut [f32],
    buf_os_model_l: &mut [f32],
    buf_os_model_r: &mut [f32],
    model_output_mult_adj: f32,
    shared_sample_rate: u32,
    gain_lut: &GainLUT,
    cabsim_tail_remaining: &mut usize,
) -> (usize, GateState) {
    if n_samples > neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF {
        let mut total_out = 0;
        let mut last_gate = GateState::Open;
        let mut curr_offset = offset;
        let mut curr_out_offset = output_offset;
        let mut remaining = n_samples;

        while remaining > 0 {
            let chunk = remaining.min(neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF);
            let (c_out, c_gate) = process_sub_block(
                curr_offset,
                chunk,
                out_l,
                out_r,
                curr_out_offset,
                ctx,
                bypass,
                process_mono,
                crossfader,
                buf_xfade_dry_l,
                buf_xfade_dry_r,
                buf_xfd_scratch_l,
                buf_xfd_scratch_r,
                input_clipped,
                smoother_in,
                smoother_out,
                buf_host_l,
                buf_host_r,
                buf_mid_l,
                buf_mid_r,
                buf_out_l,
                buf_out_r,
                buf_model_l,
                buf_model_r,
                buf_os_in_l,
                buf_os_in_r,
                buf_os_model_l,
                buf_os_model_r,
                model_output_mult_adj,
                shared_sample_rate,
                gain_lut,
                cabsim_tail_remaining,
            );
            total_out += c_out;
            last_gate = c_gate;
            curr_offset += chunk;
            curr_out_offset += c_out;
            remaining -= chunk;
        }
        return (total_out, last_gate);
    }

    if crossfader.active {
        return process_crossfade_sub_block(
            offset,
            n_samples,
            out_l,
            out_r,
            output_offset,
            ctx,
            process_mono,
            crossfader,
            buf_xfade_dry_l,
            buf_xfade_dry_r,
            buf_xfd_scratch_l,
            buf_xfd_scratch_r,
            input_clipped,
            smoother_in,
            smoother_out,
            buf_host_l,
            buf_host_r,
            buf_mid_l,
            buf_mid_r,
            buf_out_l,
            buf_out_r,
            buf_model_l,
            buf_model_r,
            buf_os_in_l,
            buf_os_in_r,
            buf_os_model_l,
            buf_os_model_r,
            model_output_mult_adj,
            shared_sample_rate,
            gain_lut,
            cabsim_tail_remaining,
        );
    }

    if bypass {
        copy_bypass_to_output(
            out_l,
            out_r,
            &buf_host_l[offset..offset + n_samples],
            &buf_host_r[offset..offset + n_samples],
            output_offset,
            process_mono,
        );
        return (n_samples, GateState::Open);
    }

    apply_iir_gain_ramp_sub_block(
        smoother_in,
        buf_host_l,
        buf_host_r,
        offset,
        n_samples,
        true,
        input_clipped,
    );

    let gate_state = apply_input_stage(
        &mut buf_host_l[offset..offset + n_samples],
        &mut buf_host_r[offset..offset + n_samples],
        n_samples,
        ctx,
    );

    if gate_state == GateState::Closed {
        if *cabsim_tail_remaining > 0 {
            return process_tail_drain(
                n_samples,
                out_l,
                out_r,
                output_offset,
                ctx,
                process_mono,
                smoother_out,
                buf_out_l,
                buf_out_r,
                buf_model_l,
                buf_model_r,
                model_output_mult_adj,
                shared_sample_rate,
                cabsim_tail_remaining,
            );
        }
        copy_silence_to_output(out_l, out_r, output_offset, n_samples, process_mono);
        return (n_samples, GateState::Closed);
    }

    let n_out = run_inference(
        &mut buf_host_l[offset..offset + n_samples],
        &mut buf_host_r[offset..offset + n_samples],
        n_samples,
        ctx,
        buf_mid_l,
        buf_mid_r,
        buf_out_l,
        buf_out_r,
        buf_model_l,
        buf_model_r,
        buf_os_in_l,
        buf_os_in_r,
        buf_os_model_l,
        buf_os_model_r,
        buf_xfd_scratch_l,
        buf_xfd_scratch_r,
    );

    if let Some(ref mut conv) = ctx.conv
        && !conv.is_passthrough()
    {
        conv.process_variable(
            &buf_out_l[..n_out],
            &mut buf_model_l[..n_out],
            Some(ctx.rt_status),
        );
        unsafe {
            core::ptr::copy_nonoverlapping(buf_model_l.as_ptr(), buf_out_l.as_mut_ptr(), n_out);
        }
        unsafe {
            core::ptr::copy_nonoverlapping(buf_out_l.as_ptr(), buf_out_r.as_mut_ptr(), n_out);
        }
    }

    apply_output_stage(
        &mut buf_out_l[..n_out],
        &mut buf_out_r[..n_out],
        n_out,
        model_output_mult_adj,
        ctx.silence_hysteresis,
        ctx.rt_status,
        *ctx.process_mono,
        ctx.adaptive,
        shared_sample_rate,
    );

    apply_iir_gain_ramp_sub_block(
        smoother_out,
        buf_out_l,
        buf_out_r,
        0,
        n_out,
        false,
        &mut false,
    );

    copy_output_from_sub_block(
        out_l,
        out_r,
        buf_out_l,
        buf_out_r,
        n_out,
        output_offset,
        process_mono,
    );

    (n_out, gate_state)
}

/// Drains the cab-sim IR tail ring-out after the noise gate closes.
///
/// Feeds zero-input blocks through the convolution adapter and output stage.
/// The tail counter (`cabsim_tail_remaining`) is decremented until zero, after
/// which the caller switches to true silence.
#[inline(always)]
#[expect(clippy::too_many_arguments)]
fn process_tail_drain(
    n_samples: usize,
    out_l: &mut Option<&mut [f32]>,
    out_r: &mut Option<&mut [f32]>,
    output_offset: usize,
    ctx: &mut DspPipelineContext<'_>,
    process_mono: bool,
    smoother_out: &mut ParamSmoother,
    buf_out_l: &mut [f32],
    buf_out_r: &mut [f32],
    buf_model_l: &mut [f32],
    _buf_model_r: &mut [f32],
    model_output_mult_adj: f32,
    shared_sample_rate: u32,
    cabsim_tail_remaining: &mut usize,
) -> (usize, GateState) {
    let drain = n_samples.min(*cabsim_tail_remaining);

    buf_out_l[..drain].fill(0.0);
    buf_out_r[..drain].fill(0.0);

    if let Some(ref mut conv) = ctx.conv
        && !conv.is_passthrough()
    {
        conv.process_variable(
            &buf_out_l[..drain],
            &mut buf_model_l[..drain],
            Some(ctx.rt_status),
        );
        unsafe {
            core::ptr::copy_nonoverlapping(buf_model_l.as_ptr(), buf_out_l.as_mut_ptr(), drain);
            core::ptr::copy_nonoverlapping(buf_out_l.as_ptr(), buf_out_r.as_mut_ptr(), drain);
        }
    }

    apply_output_stage(
        &mut buf_out_l[..drain],
        &mut buf_out_r[..drain],
        drain,
        model_output_mult_adj,
        ctx.silence_hysteresis,
        ctx.rt_status,
        *ctx.process_mono,
        ctx.adaptive,
        shared_sample_rate,
    );

    apply_iir_gain_ramp_sub_block(
        smoother_out,
        buf_out_l,
        buf_out_r,
        0,
        drain,
        false,
        &mut false,
    );

    copy_output_from_sub_block(
        out_l,
        out_r,
        buf_out_l,
        buf_out_r,
        drain,
        output_offset,
        process_mono,
    );

    *cabsim_tail_remaining -= drain;
    (drain, GateState::Closed)
}

#[inline(always)]
#[expect(clippy::too_many_arguments)]
fn process_crossfade_sub_block(
    offset: usize,
    n_samples: usize,
    out_l: &mut Option<&mut [f32]>,
    out_r: &mut Option<&mut [f32]>,
    output_offset: usize,
    ctx: &mut DspPipelineContext<'_>,
    process_mono: bool,
    crossfader: &mut BypassCrossfader,
    buf_xfade_dry_l: &mut [f32],
    buf_xfade_dry_r: &mut [f32],
    buf_xfd_scratch_l: &mut [f32],
    buf_xfd_scratch_r: &mut [f32],
    input_clipped: &mut bool,
    smoother_in: &mut ParamSmoother,
    smoother_out: &mut ParamSmoother,
    buf_host_l: &mut [f32],
    buf_host_r: &mut [f32],
    buf_mid_l: &mut [f32],
    buf_mid_r: &mut [f32],
    buf_out_l: &mut [f32],
    buf_out_r: &mut [f32],
    buf_model_l: &mut [f32],
    buf_model_r: &mut [f32],
    buf_os_in_l: &mut [f32],
    buf_os_in_r: &mut [f32],
    buf_os_model_l: &mut [f32],
    buf_os_model_r: &mut [f32],
    model_output_mult_adj: f32,
    shared_sample_rate: u32,
    _gain_lut: &GainLUT,
    cabsim_tail_remaining: &mut usize,
) -> (usize, GateState) {
    // 1. Save dry input before pipeline modifies buf_host in place
    let dry_n = n_samples.min(buf_xfade_dry_l.len());
    buf_xfade_dry_l[..dry_n].copy_from_slice(&buf_host_l[offset..offset + dry_n]);
    #[cfg(feature = "stereo")]
    buf_xfade_dry_r[..dry_n].copy_from_slice(&buf_host_r[offset..offset + dry_n]);
    #[cfg(not(feature = "stereo"))]
    buf_xfade_dry_r[..dry_n].copy_from_slice(&buf_xfade_dry_l[..dry_n]);

    // 2. Run full wet pipeline
    apply_iir_gain_ramp_sub_block(
        smoother_in,
        buf_host_l,
        buf_host_r,
        offset,
        n_samples,
        true,
        input_clipped,
    );

    let gate_state = apply_input_stage(
        &mut buf_host_l[offset..offset + n_samples],
        &mut buf_host_r[offset..offset + n_samples],
        n_samples,
        ctx,
    );

    let n_out = if gate_state == GateState::Closed {
        if *cabsim_tail_remaining > 0 {
            let drain = n_samples.min(*cabsim_tail_remaining);
            if let Some(ref mut conv) = ctx.conv
                && !conv.is_passthrough()
            {
                buf_out_l[..drain].fill(0.0);
                conv.process_variable(
                    &buf_out_l[..drain],
                    &mut buf_model_l[..drain],
                    Some(ctx.rt_status),
                );
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        buf_model_l.as_ptr(),
                        buf_out_l.as_mut_ptr(),
                        drain,
                    );
                    core::ptr::copy_nonoverlapping(
                        buf_out_l.as_ptr(),
                        buf_out_r.as_mut_ptr(),
                        drain,
                    );
                }
            }
            apply_output_stage(
                &mut buf_out_l[..drain],
                &mut buf_out_r[..drain],
                drain,
                model_output_mult_adj,
                ctx.silence_hysteresis,
                ctx.rt_status,
                *ctx.process_mono,
                ctx.adaptive,
                shared_sample_rate,
            );
            apply_iir_gain_ramp_sub_block(
                smoother_out,
                buf_out_l,
                buf_out_r,
                0,
                drain,
                false,
                &mut false,
            );
            *cabsim_tail_remaining -= drain;
            drain
        } else {
            buf_out_l[..n_samples].fill(0.0);
            buf_out_r[..n_samples].fill(0.0);
            n_samples
        }
    } else {
        let n_o = run_inference(
            &mut buf_host_l[offset..offset + n_samples],
            &mut buf_host_r[offset..offset + n_samples],
            n_samples,
            ctx,
            buf_mid_l,
            buf_mid_r,
            buf_out_l,
            buf_out_r,
            buf_model_l,
            buf_model_r,
            buf_os_in_l,
            buf_os_in_r,
            buf_os_model_l,
            buf_os_model_r,
            buf_xfd_scratch_l,
            buf_xfd_scratch_r,
        );

        if let Some(ref mut conv) = ctx.conv
            && !conv.is_passthrough()
        {
            conv.process_variable(
                &buf_out_l[..n_o],
                &mut buf_model_l[..n_o],
                Some(ctx.rt_status),
            );
            unsafe {
                core::ptr::copy_nonoverlapping(buf_model_l.as_ptr(), buf_out_l.as_mut_ptr(), n_o);
            }
            unsafe {
                core::ptr::copy_nonoverlapping(buf_out_l.as_ptr(), buf_out_r.as_mut_ptr(), n_o);
            }
        }

        apply_output_stage(
            &mut buf_out_l[..n_o],
            &mut buf_out_r[..n_o],
            n_o,
            model_output_mult_adj,
            ctx.silence_hysteresis,
            ctx.rt_status,
            *ctx.process_mono,
            ctx.adaptive,
            shared_sample_rate,
        );

        apply_iir_gain_ramp_sub_block(
            smoother_out,
            buf_out_l,
            buf_out_r,
            0,
            n_o,
            false,
            &mut false,
        );

        n_o
    };

    // 3. Crossfade blend: output = dry * (1 - mix_i) + wet * mix_i
    debug_assert!(
        n_out <= dry_n,
        "n_out ({n_out}) exceeded dry_n ({dry_n}) in bypass crossfade"
    );
    let n_xfade = n_out.min(crossfader.remaining);
    let step = crossfader.step;
    let mut mix = crossfader.mix;

    // Blended portion (first n_xfade samples): ramp from current mix towards target
    for i in 0..n_xfade {
        let dry_l = if i < dry_n { buf_xfade_dry_l[i] } else { 0.0 };
        let dry_r = if i < dry_n { buf_xfade_dry_r[i] } else { 0.0 };
        buf_out_l[i] = dry_l + (buf_out_l[i] - dry_l) * mix;
        buf_out_r[i] = dry_r + (buf_out_r[i] - dry_r) * mix;
        mix += step;
    }

    // Pure portion (remaining n_out - n_xfade samples): final mix value
    let final_mix = if crossfader.target { 0.0 } else { 1.0 };
    if (final_mix - 1.0f32).abs() > f32::EPSILON {
        // final_mix is 0.0 (dry target): copy dry to output, zero-fill any excess beyond dry_n
        for i in n_xfade..n_out {
            if i < dry_n {
                buf_out_l[i] = buf_xfade_dry_l[i];
                buf_out_r[i] = buf_xfade_dry_r[i];
            } else {
                buf_out_l[i] = 0.0;
                buf_out_r[i] = 0.0;
            }
        }
    }
    // If final_mix is 1.0 (wet target): buf_out already has wet, nothing to do

    crossfader.mix = mix;
    crossfader.remaining = crossfader.remaining.saturating_sub(n_xfade);
    if crossfader.remaining == 0 {
        crossfader.active = false;
        crossfader.mix = final_mix;
    }

    // 4. Copy blended result to output
    copy_output_from_sub_block(
        out_l,
        out_r,
        buf_out_l,
        buf_out_r,
        n_out,
        output_offset,
        process_mono,
    );

    (n_out, gate_state)
}

#[inline(always)]
pub(crate) fn apply_iir_gain_ramp_sub_block(
    smoother: &mut ParamSmoother,
    buf_l: &mut [f32],
    buf_r: &mut [f32],
    offset: usize,
    n: usize,
    detect_clip: bool,
    input_clipped: &mut bool,
) {
    let start = smoother.peek();
    let target = smoother.target_value();

    // Fast path: gain is stable — single SIMD multiply.
    if (start - target).abs() < 1e-9 {
        #[cfg(feature = "stereo")]
        {
            if detect_clip {
                let clipped = unsafe {
                    neural_amp_modeler_rs::math::dsp::gain::apply_gain_and_detect_clipping_stereo(
                        &mut buf_l[offset..offset + n],
                        &mut buf_r[offset..offset + n],
                        start,
                    )
                };
                if clipped {
                    *input_clipped = true;
                }
            } else {
                unsafe {
                    neural_amp_modeler_rs::math::dsp::gain::apply_gain_stereo(
                        &mut buf_l[offset..offset + n],
                        &mut buf_r[offset..offset + n],
                        start,
                    );
                }
            }
        }
        #[cfg(not(feature = "stereo"))]
        {
            let _ = buf_r;
            if detect_clip {
                let clipped = unsafe {
                    neural_amp_modeler_rs::math::dsp::gain::apply_gain_and_detect_clipping_mono(
                        &mut buf_l[offset..offset + n],
                        start,
                    )
                };
                if clipped {
                    *input_clipped = true;
                }
            } else {
                neural_amp_modeler_rs::math::dsp::gain::apply_gain_simd(
                    &mut buf_l[offset..offset + n],
                    start,
                );
            }
        }
        return;
    }

    // IIR exponential ramp: exactly matches tick() output for all block sizes.
    // y[i] = target + (1-α)^(i+1) * (start - target)
    // Single branchless loop replaces the old small-block (< 8 tick path)
    // and large-block (linear ramp + snap) paths.
    let alpha = smoother.alpha();
    let beta = 1.0 - alpha;
    let diff = start - target;
    let mut bp = beta;

    let slice_l = &mut buf_l[offset..offset + n];
    let slice_r = &mut buf_r[offset..offset + n];

    #[cfg(feature = "stereo")]
    {
        for i in 0..n {
            let gain = target + bp * diff;
            unsafe {
                let p_l = slice_l.get_unchecked_mut(i);
                let p_r = slice_r.get_unchecked_mut(i);
                *p_l *= gain;
                *p_r *= gain;
                if detect_clip && ((*p_l).abs() > 1.0 || (*p_r).abs() > 1.0) {
                    *input_clipped = true;
                }
            }
            bp *= beta;
        }
    }
    #[cfg(not(feature = "stereo"))]
    {
        let _ = &slice_r;
        let _ = buf_r;
        for i in 0..n {
            let gain = target + bp * diff;
            unsafe {
                let p_l = slice_l.get_unchecked_mut(i);
                *p_l *= gain;
                if detect_clip && (*p_l).abs() > 1.0 {
                    *input_clipped = true;
                }
            }
            bp *= beta;
        }
    }

    // After n iterations, bp = beta^(n+1).
    // The last smoother state is y[n-1] = target + beta^n * diff = target + (bp / beta) * diff.
    let final_val = target + (bp / beta) * diff;
    smoother.set(final_val);
}

#[inline(always)]
pub(crate) fn copy_silence_to_output(
    out_l: &mut Option<&mut [f32]>,
    out_r: &mut Option<&mut [f32]>,
    output_offset: usize,
    n_samples: usize,
    _process_mono: bool,
) {
    if let Some(o_l) = out_l {
        let end = (output_offset + n_samples).min(o_l.len());
        o_l[output_offset..end].fill(0.0);
    }
    if let Some(o_r) = out_r {
        let end = (output_offset + n_samples).min(o_r.len());
        o_r[output_offset..end].fill(0.0);
    }
}

#[inline(always)]
pub(crate) fn copy_output_from_sub_block(
    out_l: &mut Option<&mut [f32]>,
    out_r: &mut Option<&mut [f32]>,
    buf_out_l: &[f32],
    buf_out_r: &[f32],
    n_out: usize,
    output_offset: usize,
    process_mono: bool,
) {
    if let Some(o_l) = out_l {
        let n = n_out.min(o_l.len().saturating_sub(output_offset));
        o_l[output_offset..output_offset + n].copy_from_slice(&buf_out_l[..n]);
    }
    if let Some(o_r) = out_r {
        let n = n_out.min(o_r.len().saturating_sub(output_offset));
        if process_mono {
            o_r[output_offset..output_offset + n].copy_from_slice(&buf_out_l[..n]);
        } else {
            o_r[output_offset..output_offset + n].copy_from_slice(&buf_out_r[..n]);
        }
    }
}

#[inline(always)]
pub(crate) fn copy_bypass_to_output(
    out_l: &mut Option<&mut [f32]>,
    out_r: &mut Option<&mut [f32]>,
    buf_host_l: &[f32],
    buf_host_r: &[f32],
    output_offset: usize,
    process_mono: bool,
) {
    if let Some(o_l) = out_l {
        let n = buf_host_l
            .len()
            .min(o_l.len().saturating_sub(output_offset));
        o_l[output_offset..output_offset + n].copy_from_slice(&buf_host_l[..n]);
    }
    if let Some(o_r) = out_r {
        let src = if process_mono { buf_host_l } else { buf_host_r };
        let n = src.len().min(o_r.len().saturating_sub(output_offset));
        o_r[output_offset..output_offset + n].copy_from_slice(&src[..n]);
    }
}
