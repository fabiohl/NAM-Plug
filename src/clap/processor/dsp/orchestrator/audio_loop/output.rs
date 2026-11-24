// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Fused output sanitization, SIMD peak/RMS metrics, and host buffer transfers.

#[cfg(any(test, feature = "testing"))]
use neural_amp_modeler_rs::math::dsp::sanitize_copy_peak;

/// Reference two-pass sum of squares over a slice, retained for F10 benchmark/test comparison.
#[cfg(any(test, feature = "testing"))]
#[inline(always)]
fn compute_sum_sq(samples: &[f32]) -> f32 {
    let mut sum = 0.0f32;
    for &s in samples {
        sum += s * s;
    }
    sum
}

/// Computes absolute peak and sum of squared samples over an input slice.
#[inline(always)]
pub(crate) fn compute_peak_and_sum_sq(samples: &[f32]) -> (f32, f32) {
    let mut peak = 0.0f32;
    let mut sum_sq = 0.0f32;
    for &s in samples {
        let abs = s.abs();
        if abs.is_finite() {
            peak = peak.max(abs);
            sum_sq += abs * abs;
        }
    }
    (peak, sum_sq)
}

#[inline(always)]
pub(crate) fn copy_silence_to_output(
    out_l: &mut Option<&mut [f32]>,
    out_r: &mut Option<&mut [f32]>,
    output_offset: usize,
    n_samples: usize,
    _process_mono: bool,
) -> (f32, f32, f32, f32) {
    if let Some(o_l) = out_l {
        let end = (output_offset + n_samples).min(o_l.len());
        o_l[output_offset..end].fill(0.0);
    }
    if let Some(o_r) = out_r {
        let end = (output_offset + n_samples).min(o_r.len());
        o_r[output_offset..end].fill(0.0);
    }
    (0.0, 0.0, 0.0, 0.0)
}

/// Sanitizes non-finite values (NaN and ±inf -> `0.0`) while copying `src` to `dst`
/// in a single scalar pass, computing absolute peak and sum of squares simultaneously.
#[cfg(any(not(target_arch = "x86_64"), test, feature = "testing"))]
#[inline(always)]
pub(super) fn sanitize_copy_peak_sum_sq_scalar(src: &[f32], dst: &mut [f32]) -> (f32, f32) {
    let len = core::cmp::min(src.len(), dst.len());
    let mut peak = 0.0f32;
    let mut sum_sq = 0.0f32;
    for (&s, d) in src[..len].iter().zip(dst[..len].iter_mut()) {
        let clean = if s.is_finite() { s } else { 0.0 };
        *d = clean;
        peak = peak.max(clean.abs());
        sum_sq += clean * clean;
    }
    (peak, sum_sq)
}

/// Fused AVX2/FMA pass: sanitizes non-finite samples (`0.0`), stores to `dst`,
/// and accumulates both peak magnitude and sum of squares in 8-lane vectors.
///
/// # Safety
///
/// `src` and `dst` must be valid slices. AVX2 and FMA must be available
/// (guaranteed unconditionally on `x86_64` by the crate's `x86-64-v3` baseline).
#[cfg(target_arch = "x86_64")]
#[inline]
#[target_feature(enable = "avx2", enable = "fma")]
unsafe fn sanitize_copy_peak_sum_sq_avx2(src: &[f32], dst: &mut [f32]) -> (f32, f32) {
    use core::arch::x86_64::*;

    let len = core::cmp::min(src.len(), dst.len());
    if len == 0 {
        return (0.0, 0.0);
    }

    let mut i = 0;
    let mut peak = 0.0f32;
    let mut sum_sq = 0.0f32;

    // SAFETY: the loop guard keeps every 8-lane load/store within `src` and
    // `dst` (`i + 8 <= len`). AVX2 and FMA are enabled by `#[target_feature]`.
    unsafe {
        let max = _mm256_set1_ps(f32::MAX);
        let abs_mask = _mm256_set1_ps(f32::from_bits(0x7FFF_FFFF));
        let zero = _mm256_setzero_ps();
        let mut max_v = _mm256_setzero_ps();
        let mut sum_v = _mm256_setzero_ps();

        while i + 8 <= len {
            let src_ptr = src.as_ptr().add(i);
            let dst_ptr = dst.as_mut_ptr().add(i);
            let v = _mm256_loadu_ps(src_ptr);

            let ord = _mm256_cmp_ps(v, v, _CMP_ORD_Q);
            let abs = _mm256_and_ps(v, abs_mask);
            let le = _mm256_cmp_ps(abs, max, _CMP_LE_OQ);

            let finite = _mm256_and_ps(ord, le);
            let clean = _mm256_blendv_ps(zero, v, finite);
            _mm256_storeu_ps(dst_ptr, clean);

            let abs_clean = _mm256_and_ps(clean, abs_mask);
            max_v = _mm256_max_ps(max_v, abs_clean);
            sum_v = _mm256_fmadd_ps(clean, clean, sum_v);

            i += 8;
        }

        let hi_m = _mm256_extractf128_ps(max_v, 1);
        let lo_m = _mm256_castps256_ps128(max_v);
        let m128 = _mm_max_ps(lo_m, hi_m);
        let shuf_m = _mm_shuffle_ps(m128, m128, 0xEE);
        let m64 = _mm_max_ps(m128, shuf_m);
        let shuf2_m = _mm_shuffle_ps(m64, m64, 0x55);
        let m32 = _mm_max_ps(m64, shuf2_m);
        _mm_store_ss(&mut peak, m32);

        let hi_s = _mm256_extractf128_ps(sum_v, 1);
        let lo_s = _mm256_castps256_ps128(sum_v);
        let s128 = _mm_add_ps(lo_s, hi_s);
        let shuf_s = _mm_movehl_ps(s128, s128);
        let s64 = _mm_add_ps(s128, shuf_s);
        let shuf2_s = _mm_shuffle_ps(s64, s64, 0x55);
        let s32 = _mm_add_ss(s64, shuf2_s);
        _mm_store_ss(&mut sum_sq, s32);
    }

    while i < len {
        let s = src[i];
        let clean = if s.is_finite() { s } else { 0.0 };
        dst[i] = clean;
        let a = clean.abs();
        if a > peak {
            peak = a;
        }
        sum_sq += clean * clean;
        i += 1;
    }

    (peak, sum_sq)
}

#[inline(always)]
fn sanitize_copy_peak_sum_sq(src: &[f32], dst: &mut [f32]) -> (f32, f32) {
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: `x86-64-v3` baseline guarantees AVX2 and FMA unconditionally.
        unsafe { sanitize_copy_peak_sum_sq_avx2(src, dst) }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        sanitize_copy_peak_sum_sq_scalar(src, dst)
    }
}

/// Shared fused transfer kernel for wet output and delayed dry output.
#[inline(always)]
fn copy_channels_fused(
    out_l: &mut Option<&mut [f32]>,
    out_r: &mut Option<&mut [f32]>,
    src_l: &[f32],
    src_r: &[f32],
    max_len: usize,
    output_offset: usize,
    process_mono: bool,
) -> (f32, f32, f32, f32) {
    let mut peak_l = 0.0f32;
    let mut sum_sq_l = 0.0f32;
    let mut n_l = 0usize;
    if let Some(o_l) = out_l {
        let n = max_len
            .min(src_l.len())
            .min(o_l.len().saturating_sub(output_offset));
        n_l = n;
        (peak_l, sum_sq_l) =
            sanitize_copy_peak_sum_sq(&src_l[..n], &mut o_l[output_offset..output_offset + n]);
    }
    let mut peak_r = 0.0f32;
    let mut sum_sq_r = 0.0f32;
    if let Some(o_r) = out_r {
        let src = if process_mono { src_l } else { src_r };
        let n_r = max_len
            .min(src.len())
            .min(o_r.len().saturating_sub(output_offset));
        if process_mono
            && let Some(o_l) = out_l
            && n_l == n_r
        {
            o_r[output_offset..output_offset + n_r]
                .copy_from_slice(&o_l[output_offset..output_offset + n_l]);
            peak_r = peak_l;
            sum_sq_r = sum_sq_l;
        } else {
            (peak_r, sum_sq_r) = sanitize_copy_peak_sum_sq(
                &src[..n_r],
                &mut o_r[output_offset..output_offset + n_r],
            );
        }
    }
    (peak_l, peak_r, sum_sq_l, sum_sq_r)
}

/// Copies the wet model output to the host buffers while sanitizing non-finites
/// (`0.0`) and computing absolute peak and sum of squares in a single fused pass.
#[inline(always)]
pub(crate) fn copy_output_from_sub_block(
    out_l: &mut Option<&mut [f32]>,
    out_r: &mut Option<&mut [f32]>,
    buf_out_l: &mut [f32],
    buf_out_r: &mut [f32],
    n_out: usize,
    output_offset: usize,
    process_mono: bool,
) -> (f32, f32, f32, f32) {
    copy_channels_fused(
        out_l,
        out_r,
        buf_out_l,
        buf_out_r,
        n_out,
        output_offset,
        process_mono,
    )
}

/// Copies the latency-compensated dry signal to the output in the
/// fully-bypassed state while sanitizing non-finites (`0.0`) and computing
/// absolute peak and sum of squares in a single fused pass.
#[inline(always)]
pub(crate) fn copy_delayed_dry_to_output(
    out_l: &mut Option<&mut [f32]>,
    out_r: &mut Option<&mut [f32]>,
    dry_l: &mut [f32],
    dry_r: &mut [f32],
    output_offset: usize,
    process_mono: bool,
) -> (f32, f32, f32, f32) {
    copy_channels_fused(
        out_l,
        out_r,
        dry_l,
        dry_r,
        usize::MAX,
        output_offset,
        process_mono,
    )
}

#[cfg(any(test, feature = "testing"))]
#[inline(always)]
pub(crate) fn copy_output_from_sub_block_two_pass_ref(
    out_l: &mut Option<&mut [f32]>,
    out_r: &mut Option<&mut [f32]>,
    buf_out_l: &[f32],
    buf_out_r: &[f32],
    n_out: usize,
    output_offset: usize,
    process_mono: bool,
) -> (f32, f32, f32, f32) {
    let mut peak_l = 0.0f32;
    let mut sum_sq_l = 0.0f32;
    if let Some(o_l) = out_l {
        let n = n_out.min(o_l.len().saturating_sub(output_offset));
        peak_l = sanitize_copy_peak(&buf_out_l[..n], &mut o_l[output_offset..output_offset + n]);
        sum_sq_l = compute_sum_sq(&o_l[output_offset..output_offset + n]);
    }
    let mut peak_r = 0.0f32;
    let mut sum_sq_r = 0.0f32;
    if let Some(o_r) = out_r {
        let src: &[f32] = if process_mono { buf_out_l } else { buf_out_r };
        let n = n_out.min(o_r.len().saturating_sub(output_offset));
        peak_r = sanitize_copy_peak(&src[..n], &mut o_r[output_offset..output_offset + n]);
        sum_sq_r = compute_sum_sq(&o_r[output_offset..output_offset + n]);
    }
    (peak_l, peak_r, sum_sq_l, sum_sq_r)
}

#[cfg(any(test, feature = "testing"))]
#[inline(always)]
pub(crate) fn copy_output_from_sub_block_fused_scalar(
    out_l: &mut Option<&mut [f32]>,
    out_r: &mut Option<&mut [f32]>,
    buf_out_l: &[f32],
    buf_out_r: &[f32],
    n_out: usize,
    output_offset: usize,
    process_mono: bool,
) -> (f32, f32, f32, f32) {
    let mut peak_l = 0.0f32;
    let mut sum_sq_l = 0.0f32;
    if let Some(o_l) = out_l {
        let n = n_out.min(o_l.len().saturating_sub(output_offset));
        (peak_l, sum_sq_l) = sanitize_copy_peak_sum_sq_scalar(
            &buf_out_l[..n],
            &mut o_l[output_offset..output_offset + n],
        );
    }
    let mut peak_r = 0.0f32;
    let mut sum_sq_r = 0.0f32;
    if let Some(o_r) = out_r {
        let src: &[f32] = if process_mono { buf_out_l } else { buf_out_r };
        let n = n_out.min(o_r.len().saturating_sub(output_offset));
        (peak_r, sum_sq_r) =
            sanitize_copy_peak_sum_sq_scalar(&src[..n], &mut o_r[output_offset..output_offset + n]);
    }
    (peak_l, peak_r, sum_sq_l, sum_sq_r)
}
