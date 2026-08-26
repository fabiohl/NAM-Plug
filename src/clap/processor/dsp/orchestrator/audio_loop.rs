// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use crate::clap::processor::dsp::dry_delay::DryDelayLine;
use crate::clap::processor::state::BypassCrossfader;
use neural_amp_modeler_rs::dsp::gate::{DynamicHysteresis, GateState};
use neural_amp_modeler_rs::dsp::pipeline::{
    DspPipelineContext, apply_input_stage, apply_output_stage, run_inference_streaming,
};
use neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer;
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
    stream: &mut StreamingResampleBuffer,
    bypass: bool,
    process_mono: bool,
    crossfader: &mut BypassCrossfader,
    dry_delay: &mut DryDelayLine,
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
                stream,
                bypass,
                process_mono,
                crossfader,
                dry_delay,
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

    // T4.1/F-DSP-008: delay the raw dry input by the applied wet latency and
    // stage it into buf_xfade_dry — the single source for both the bypass
    // output and the crossfade blend. Fed every sub-block (even wet-only) so
    // the ring always holds the full latency history when a bypass/crossfade
    // transition starts. `n_samples <= buf_xfade_dry.len()` by construction
    // (both are bounded by max_frames_count / MAX_RESAMP_BUF chunking).
    dry_delay.process_block(
        &buf_host_l[offset..offset + n_samples],
        &buf_host_r[offset..offset + n_samples],
        &mut buf_xfade_dry_l[..n_samples],
        &mut buf_xfade_dry_r[..n_samples],
        n_samples,
    );

    if crossfader.active {
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
        copy_delayed_dry_to_output(
            out_l,
            out_r,
            &buf_xfade_dry_l[..n_samples],
            &buf_xfade_dry_r[..n_samples],
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

    if let Some(ref mut conv) = ctx.conv
        && !conv.is_passthrough()
    {
        conv.process_variable(
            &buf_out_l[..n_out],
            &mut buf_model_l[..n_out],
            Some(ctx.rt_status),
        );
        // F-DSP-009/T4.2: the tail counter is re-armed to the full IR duration
        // whenever active audio is effectively fed into the convolution module.
        // Without this, the first gate close consumes the counter to zero and
        // every later note is truncated to immediate silence on the next close.
        // Rearming in the drain paths is deliberately avoided (no signal reaches
        // the conv there) so the drain always terminates.
        *cabsim_tail_remaining = conv.tail_samples();
        unsafe {
            core::ptr::copy_nonoverlapping(buf_model_l.as_ptr(), buf_out_l.as_mut_ptr(), n_out);
        }
        if !process_mono {
            unsafe {
                core::ptr::copy_nonoverlapping(buf_out_l.as_ptr(), buf_out_r.as_mut_ptr(), n_out);
            }
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
        }
        if !process_mono {
            unsafe {
                core::ptr::copy_nonoverlapping(buf_out_l.as_ptr(), buf_out_r.as_mut_ptr(), drain);
            }
        }
    }

    // F-DSP-009/T4.2: the ring-out is intentional signal, not noise floor —
    // the output stage must not multiply it by the (now closed) noise-gate
    // multiplier of 0. A fresh unity gate (Open, multiplier 1.0, steady) yields
    // exactly the wet output gain + smoothing while letting the IR ring to
    // completion; the real gate FSM keeps tracking the input independently.
    let mut tail_gate = DynamicHysteresis::new();
    apply_output_stage(
        &mut buf_out_l[..drain],
        &mut buf_out_r[..drain],
        drain,
        model_output_mult_adj,
        &mut tail_gate,
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

    // Strict cardinality (F-PERF-002 / T1.3): every sub-block must deliver
    // exactly `n_samples` host samples. The ring-out only covers `drain`
    // samples; zero-fill the suffix so no stale/sentinel residue reaches the
    // host and `output_offset` advances by the full sub-block size.
    if drain < n_samples {
        buf_out_l[drain..n_samples].fill(0.0);
        buf_out_r[drain..n_samples].fill(0.0);
    }

    copy_output_from_sub_block(
        out_l,
        out_r,
        buf_out_l,
        buf_out_r,
        n_samples,
        output_offset,
        process_mono,
    );

    *cabsim_tail_remaining -= drain;
    (n_samples, GateState::Closed)
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
    stream: &mut StreamingResampleBuffer,
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
    _buf_mid_l: &mut [f32],
    _buf_mid_r: &mut [f32],
    buf_out_l: &mut [f32],
    buf_out_r: &mut [f32],
    buf_model_l: &mut [f32],
    _buf_model_r: &mut [f32],
    buf_os_in_l: &mut [f32],
    buf_os_in_r: &mut [f32],
    buf_os_model_l: &mut [f32],
    buf_os_model_r: &mut [f32],
    model_output_mult_adj: f32,
    shared_sample_rate: u32,
    _gain_lut: &GainLUT,
    cabsim_tail_remaining: &mut usize,
) -> (usize, GateState) {
    // 1. Dry source: `buf_xfade_dry` already holds the latency-compensated dry
    // for this sub-block (fed by `process_sub_block` via the DryDelayLine, so
    // the dry and the wet below represent the same input instant — T4.1/
    // F-DSP-008). The wet pipeline below modifies `buf_host` in place; the
    // dry was captured before that happened.
    let dry_n = n_samples.min(buf_xfade_dry_l.len());
    debug_assert_eq!(
        dry_n, n_samples,
        "dry delay output must cover the full sub-block (n_samples={n_samples})"
    );

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
                }
                if !process_mono {
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            buf_out_l.as_ptr(),
                            buf_out_r.as_mut_ptr(),
                            drain,
                        );
                    }
                }
            }
            // F-DSP-009/T4.2: same unity-gate semantics as `process_tail_drain`
            // — the cab ring-out must not be gated to zero by the closed gate.
            let mut tail_gate = DynamicHysteresis::new();
            apply_output_stage(
                &mut buf_out_l[..drain],
                &mut buf_out_r[..drain],
                drain,
                model_output_mult_adj,
                &mut tail_gate,
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
            // Strict cardinality: the sub-block must deliver exactly
            // `n_samples` host samples. Zero-fill the suffix after the
            // ring-out so no stale/sentinel residue reaches the host.
            if drain < n_samples {
                buf_out_l[drain..n_samples].fill(0.0);
                buf_out_r[drain..n_samples].fill(0.0);
            }
            n_samples
        } else {
            buf_out_l[..n_samples].fill(0.0);
            buf_out_r[..n_samples].fill(0.0);
            n_samples
        }
    } else {
        let n_o = run_inference_streaming(
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

        if let Some(ref mut conv) = ctx.conv
            && !conv.is_passthrough()
        {
            conv.process_variable(
                &buf_out_l[..n_o],
                &mut buf_model_l[..n_o],
                Some(ctx.rt_status),
            );
            // F-DSP-009/T4.2: re-arm the tail counter for active audio (see
            // the main-path comment in `process_sub_block`).
            *cabsim_tail_remaining = conv.tail_samples();
            unsafe {
                core::ptr::copy_nonoverlapping(buf_model_l.as_ptr(), buf_out_l.as_mut_ptr(), n_o);
            }
            if !process_mono {
                unsafe {
                    core::ptr::copy_nonoverlapping(buf_out_l.as_ptr(), buf_out_r.as_mut_ptr(), n_o);
                }
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
    // With the strict-cardinality streaming adapter (T1.2/F-PERF-002),
    // `n_out == n_samples == dry_n` always, so the wet count never exceeds the
    // dry capture. The clamps below remain as cheap defensive guards.
    let n_xfade_raw = n_out.min(crossfader.remaining);
    let n_xfade = n_xfade_raw.min(dry_n);
    debug_assert!(
        n_xfade <= dry_n,
        "n_xfade ({n_xfade}) exceeded dry_n ({dry_n}) in bypass crossfade"
    );
    let step = crossfader.step;
    let mut mix = crossfader.mix;

    // Blended portion (first n_xfade samples): ramp from current mix towards target.
    // Bounds pre-validated above — slices are disjoint, loop is auto-vectorizable (FMA).
    let dry_l = &buf_xfade_dry_l[..n_xfade];
    let dry_r = &buf_xfade_dry_r[..n_xfade];
    let wet_l = &mut buf_out_l[..n_xfade];
    let wet_r = &mut buf_out_r[..n_xfade];
    for i in 0..n_xfade {
        let d_l = dry_l[i];
        let d_r = dry_r[i];
        wet_l[i] = d_l + (wet_l[i] - d_l) * mix;
        wet_r[i] = d_r + (wet_r[i] - d_r) * mix;
        mix += step;
    }

    // Overflow region (n_xfade..n_xfade_raw): wet count exceeded the dry
    // capture. Legacy guard semantics: dry = 0.0 → wet scaled by the ramp mix.
    for i in n_xfade..n_xfade_raw {
        buf_out_l[i] *= mix;
        buf_out_r[i] *= mix;
        mix += step;
    }

    // Pure portion (remaining n_out - n_xfade_raw samples): final mix value
    let final_mix = if crossfader.target { 0.0 } else { 1.0 };
    if (final_mix - 1.0f32).abs() > f32::EPSILON {
        // final_mix is 0.0 (dry target): copy dry to output, zero-fill any excess beyond dry_n
        for i in n_xfade_raw..n_out {
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
    crossfader.mix = crossfader.mix.clamp(0.0, 1.0);
    crossfader.remaining = crossfader.remaining.saturating_sub(n_xfade_raw);
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
        for i in 0..n {
            let s = buf_out_l[i];
            o_l[output_offset + i] = if s.is_finite() { s } else { 0.0 };
        }
    }
    if let Some(o_r) = out_r {
        let n = n_out.min(o_r.len().saturating_sub(output_offset));
        let src = if process_mono { buf_out_l } else { buf_out_r };
        for i in 0..n {
            let s = src[i];
            o_r[output_offset + i] = if s.is_finite() { s } else { 0.0 };
        }
    }
}

/// Copies the latency-compensated dry signal (T4.1/F-DSP-008) to the output
/// in the fully-bypassed state. `dry_l`/`dry_r` are the `DryDelayLine` output
/// staged into `buf_xfade_dry`, delayed by exactly the applied wet latency —
/// the bypass path therefore keeps the physical latency declared to the host
/// instead of snapping back to zero.
#[inline(always)]
pub(crate) fn copy_delayed_dry_to_output(
    out_l: &mut Option<&mut [f32]>,
    out_r: &mut Option<&mut [f32]>,
    dry_l: &[f32],
    dry_r: &[f32],
    output_offset: usize,
    process_mono: bool,
) {
    if let Some(o_l) = out_l {
        let n = dry_l.len().min(o_l.len().saturating_sub(output_offset));
        for i in 0..n {
            let s = dry_l[i];
            o_l[output_offset + i] = if s.is_finite() { s } else { 0.0 };
        }
    }
    if let Some(o_r) = out_r {
        let src = if process_mono { dry_l } else { dry_r };
        let n = src.len().min(o_r.len().saturating_sub(output_offset));
        for i in 0..n {
            let s = src[i];
            o_r[output_offset + i] = if s.is_finite() { s } else { 0.0 };
        }
    }
}
