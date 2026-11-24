// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Gain smoothing, calibration scaling, and piecewise IIR ramp kernels for `audio_loop`.

use super::super::worker::GainStep;
use neural_amp_modeler_rs::dsp::gate::DynamicHysteresis;
use neural_amp_modeler_rs::dsp::pipeline::apply_output_stage;
use neural_amp_modeler_rs::dsp::smoother::ParamSmoother;

#[inline(always)]
pub(super) fn advance_smoother_if_active(smoother: &mut ParamSmoother, n_samples: usize) {
    if (smoother.peek() - smoother.target_value()).abs() >= 1e-9 {
        for _ in 0..n_samples {
            smoother.tick();
        }
    }
}

#[inline(always)]
pub(super) fn resolve_input_cal_mult(
    smoother_cal_in: &mut ParamSmoother,
    buf_l: &mut [f32],
    buf_r: &mut [f32],
    offset: usize,
    n_samples: usize,
    process_mono: bool,
) -> f32 {
    if (smoother_cal_in.peek() - smoother_cal_in.target_value()).abs() >= 1e-9 {
        let mut dummy = false;
        apply_iir_gain_ramp_sub_block(
            smoother_cal_in,
            buf_l,
            buf_r,
            offset,
            n_samples,
            false,
            &mut dummy,
            process_mono,
        );
        1.0
    } else {
        smoother_cal_in.target_value()
    }
}

#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "wrapper around apply_output_stage plus optional calibration smoother ramp"
)]
pub(super) fn apply_output_stage_calibrated(
    buf_out_l: &mut [f32],
    buf_out_r: &mut [f32],
    n_out: usize,
    smoother_cal_out: &mut ParamSmoother,
    silence_hysteresis: &mut DynamicHysteresis,
    rt_status: &neural_amp_modeler_rs::common::spsc::RtStatusFlags,
    process_mono: bool,
    adaptive: &mut neural_amp_modeler_rs::dsp::adaptive::AdaptiveCompute,
    shared_sample_rate: u32,
) {
    let cal_out_smoothing =
        (smoother_cal_out.peek() - smoother_cal_out.target_value()).abs() >= 1e-9;
    let scalar_cal_out = if cal_out_smoothing {
        1.0
    } else {
        smoother_cal_out.target_value()
    };
    apply_output_stage(
        buf_out_l,
        buf_out_r,
        n_out,
        scalar_cal_out,
        silence_hysteresis,
        rt_status,
        process_mono,
        adaptive,
        shared_sample_rate,
    );
    if cal_out_smoothing && n_out > 0 {
        let mut dummy = false;
        apply_iir_gain_ramp_sub_block(
            smoother_cal_out,
            buf_out_l,
            buf_out_r,
            0,
            n_out,
            false,
            &mut dummy,
            process_mono,
        );
    }
}

#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors apply_iir_gain_ramp_sub_block plus the fused step list"
)]
pub(super) fn apply_gain_ramp_with_steps(
    smoother: &mut ParamSmoother,
    buf_l: &mut [f32],
    buf_r: &mut [f32],
    offset: usize,
    n: usize,
    steps: &[GainStep],
    want_input: bool,
    detect_clip: bool,
    input_clipped: &mut bool,
    process_mono: bool,
) {
    let end = offset + n;
    let mut pos = offset;
    for step in steps {
        if step.is_input != want_input {
            continue;
        }
        let step_off = offset + (step.offset as usize).min(n);
        if step_off > pos {
            apply_iir_gain_ramp_sub_block(
                smoother,
                buf_l,
                buf_r,
                pos,
                step_off - pos,
                detect_clip,
                input_clipped,
                process_mono,
            );
            pos = step_off;
        }
        // The target for the next piece is applied at this exact offset, so
        // the concatenated ramps equal the legacy per-event sub-block split.
        smoother.set_target(step.target);
    }
    if pos < end {
        apply_iir_gain_ramp_sub_block(
            smoother,
            buf_l,
            buf_r,
            pos,
            end - pos,
            detect_clip,
            input_clipped,
            process_mono,
        );
    }
}

#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "FFI design or complex DSP kernel signature required by construction"
)]
pub(crate) fn apply_iir_gain_ramp_sub_block(
    smoother: &mut ParamSmoother,
    buf_l: &mut [f32],
    buf_r: &mut [f32],
    offset: usize,
    n: usize,
    detect_clip: bool,
    input_clipped: &mut bool,
    process_mono: bool,
) {
    let start = smoother.peek();
    let target = smoother.target_value();

    #[cfg(feature = "dual-mono")]
    let stereo = !process_mono;
    #[cfg(not(feature = "dual-mono"))]
    let _stereo = false;
    #[cfg(not(feature = "dual-mono"))]
    let _ = (_stereo, process_mono);
    // Fast path: gain is stable — single SIMD multiply.
    if (start - target).abs() < 1e-9 {
        #[cfg(feature = "dual-mono")]
        {
            if stereo {
                if detect_clip {
                    // SAFETY: buf_l and buf_r slices are valid for length n.
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
                    // SAFETY: buf_l and buf_r slices are valid for length n.
                    unsafe {
                        neural_amp_modeler_rs::math::dsp::gain::apply_gain_stereo(
                            &mut buf_l[offset..offset + n],
                            &mut buf_r[offset..offset + n],
                            start,
                        );
                    }
                }
            } else if detect_clip {
                // SAFETY: buf_l slice is valid for length n.
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
        #[cfg(not(feature = "dual-mono"))]
        {
            let _ = buf_r;
            if detect_clip {
                // SAFETY: buf_l slice is valid for length n.
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

    // IIR exponential ramp: y[i] = target + (1-α)^(i+1) * (start - target).
    let alpha = smoother.alpha();
    let beta = 1.0 - alpha;
    let diff = start - target;
    let mut bp = beta;

    let slice_l = &mut buf_l[offset..offset + n];
    #[cfg(feature = "dual-mono")]
    let slice_r = &mut buf_r[offset..offset + n];
    #[cfg(not(feature = "dual-mono"))]
    let _ = buf_r;

    #[cfg(feature = "dual-mono")]
    if stereo {
        for (p_l, p_r) in slice_l.iter_mut().zip(slice_r.iter_mut()) {
            let gain = target + bp * diff;
            *p_l *= gain;
            *p_r *= gain;
            bp *= beta;
        }
    } else {
        for p_l in slice_l.iter_mut() {
            let gain = target + bp * diff;
            *p_l *= gain;
            bp *= beta;
        }
    }
    #[cfg(not(feature = "dual-mono"))]
    for p_l in slice_l.iter_mut() {
        let gain = target + bp * diff;
        *p_l *= gain;
        bp *= beta;
    }

    if detect_clip {
        let max_l = slice_l.iter().fold(0.0f32, |acc, &s| acc.max(s.abs()));
        #[cfg(feature = "dual-mono")]
        let clipped = if stereo {
            let max_r = slice_r.iter().fold(0.0f32, |acc, &s| acc.max(s.abs()));
            max_l > 1.0 || max_r > 1.0
        } else {
            max_l > 1.0
        };
        #[cfg(not(feature = "dual-mono"))]
        let clipped = max_l > 1.0;

        if clipped {
            *input_clipped = true;
        }
    }

    // If beta <= 0.0 (instant jump / alpha >= 1.0), avoid 0/0 NaN division (F-NP-R3).
    let final_val = if beta <= 0.0 {
        target
    } else {
        target + (bp / beta) * diff
    };
    smoother.set(final_val);
}
