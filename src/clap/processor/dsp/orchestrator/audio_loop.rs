// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Sub-block processing loop and streaming neural audio execution for NAM-Plug.

use crate::clap::processor::dsp::dry_delay::DryDelayLine;
use crate::clap::processor::state::BypassCrossfader;
use neural_amp_modeler_rs::dsp::gate::{DynamicHysteresis, GateState};
use neural_amp_modeler_rs::dsp::pipeline::{
    DspPipelineContext, apply_input_stage, apply_input_stage_mono, apply_output_stage,
    run_inference_streaming,
};
use neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer;
use neural_amp_modeler_rs::dsp::smoother::ParamSmoother;
use neural_amp_modeler_rs::math::dsp::sanitize_copy_peak;

/// Disjoint scratch-buffer borrows for one sub-block of the DSP pipeline.
///
/// Groups the ~17 mutable slice borrows that [`process_sub_block`] and its
/// cold callees thread through every branch, so the hot-path signatures carry
/// one aggregate instead of ~30 scalars. The borrows are provably disjoint by
/// construction: each field aliases a distinct backing buffer drained from the
/// processor state in the orchestrator, and the struct carries no `unsafe`
/// (the borrow checker enforces the disjunction at each instantiation site).
///
/// The buffers the engine no longer needs (`buf_mid_*`, retired from the
/// former resampler hand-off; the `GainLUT` reference the loop never reads)
/// are absent from the struct rather than forwarded as `_`-prefixed dead
/// parameters.
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

/// Processes an audio sub-block (chunked up to MAX_RESAMP_BUF) through input staging,
/// streaming neural inference, gate hysteresis, output gain, and latency-compensated dry delay blending.
///
/// The crossfade and tail-drain transitions are `#[cold]` callees: they are rare
/// (bypass toggles, gate ring-out) and must not inflate the steady-state body of
/// `process` (L1i/uop-cache pressure, register allocation of the hot path).
#[inline]
/// `process_sub_block` and `process_sub_block_chunked` intentionally keep the
/// explicit scalar tail (`out_*`, `ctx`, `stream`, `crossfader`, `dry_delay`,
/// smoothers, scalars) alongside the grouped `scratch`: moving `ctx` (engine
/// context) into the plugin-owned struct would tangle the engine/plugin
/// ownership domains (R2), and the remaining scalars are not slice borrows
/// the struct could absorb. The lint is allowed with justification — the
/// struct still removed ~13 slice parameters per signature plus the two dead
/// ones (`buf_mid_*`, `GainLUT`).
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
    input_clipped: &mut bool,
    smoother_in: &mut ParamSmoother,
    smoother_out: &mut ParamSmoother,
    model_output_mult_adj: f32,
    shared_sample_rate: u32,
) -> (usize, GateState, f32, f32) {
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
            input_clipped,
            smoother_in,
            smoother_out,
            model_output_mult_adj,
            shared_sample_rate,
        );
    }

    // Delay the raw dry input by the applied wet latency and stage it into
    // buf_xfade_dry — the single source for both the bypass output and the
    // crossfade blend. Fed every sub-block (even wet-only) so the ring always
    // holds the full latency history when a bypass/crossfade transition starts.
    // `n_samples <= buf_xfade_dry.len()` by construction (both are bounded by
    // max_frames_count / MAX_RESAMP_BUF chunking).
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
            input_clipped,
            smoother_in,
            smoother_out,
            model_output_mult_adj,
            shared_sample_rate,
        );
    }

    if bypass {
        let (peak_l, peak_r) = copy_delayed_dry_to_output(
            out_l,
            out_r,
            &mut buf_xfade_dry_l[..n_samples],
            &mut buf_xfade_dry_r[..n_samples],
            output_offset,
            process_mono,
        );
        return (n_samples, GateState::Open, peak_l, peak_r);
    }

    apply_iir_gain_ramp_sub_block(
        smoother_in,
        buf_host_l,
        buf_host_r,
        offset,
        n_samples,
        true,
        input_clipped,
        process_mono,
    );

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

    if gate_state == GateState::Closed {
        core::hint::cold_path();
        return process_tail_drain(
            n_samples,
            out_l,
            out_r,
            output_offset,
            ctx,
            process_mono,
            smoother_out,
            scratch,
            model_output_mult_adj,
            shared_sample_rate,
        );
    }

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
        // The IR operates directly on the final output scratch: `process_block`
        // consumes the model output and writes the convolution result back into
        // the same buffer, so no intermediate model buffer (and no transfer copy)
        // is needed on the audio callback. The driver is block-agnostic — it
        // chunks arbitrary host blocks against the fixed partition internally —
        // so the output stream is invariant to host block-size variations
        // (including sizes that exceed the partition) and the partition-cap
        // clamp of the legacy call can never flag a contract violation here.
        conv.process_block(&mut buf_out_l[..n_out], Some(ctx.rt_status));
        // The tail budget is re-armed to the full IR duration whenever active
        // audio is effectively fed into the convolution module. Without this,
        // the first gate close consumes the budget to zero and every later note
        // is truncated to immediate silence on the next close. Rearming in the
        // drain paths is deliberately avoided (no signal reaches the conv there)
        // so the drain always terminates.
        conv.rearm_tail();
        if !process_mono {
            buf_out_r[..n_out].copy_from_slice(&buf_out_l[..n_out]);
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
        process_mono,
    );

    let (peak_l, peak_r) = copy_output_from_sub_block(
        out_l,
        out_r,
        buf_out_l,
        buf_out_r,
        n_out,
        output_offset,
        process_mono,
    );

    (n_out, gate_state, peak_l, peak_r)
}

/// Cold overflow chunker for host blocks larger than `MAX_RESAMP_BUF`.
///
/// Splits the oversized sub-block into `MAX_RESAMP_BUF`-bounded chunks and
/// feeds each through [`process_sub_block`] (whose `n_samples > MAX_RESAMP_BUF`
/// guard is never re-entered for chunks). Iterative by construction — the
/// self-recursive inline body of the block-overflow branch is gone, and the
/// loop never executes on the steady-state path
/// (`max_frames_count <= MAX_RESAMP_BUF`).
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
    input_clipped: &mut bool,
    smoother_in: &mut ParamSmoother,
    smoother_out: &mut ParamSmoother,
    model_output_mult_adj: f32,
    shared_sample_rate: u32,
) -> (usize, GateState, f32, f32) {
    let mut total_out = 0;
    let mut last_gate = GateState::Open;
    let mut curr_offset = offset;
    let mut curr_out_offset = output_offset;
    let mut remaining = n_samples;
    let mut peak_l = 0.0f32;
    let mut peak_r = 0.0f32;

    while remaining > 0 {
        let chunk = remaining.min(neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF);
        let (c_out, c_gate, c_pl, c_pr) = process_sub_block(
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
            input_clipped,
            smoother_in,
            smoother_out,
            model_output_mult_adj,
            shared_sample_rate,
        );
        total_out += c_out;
        last_gate = c_gate;
        peak_l = peak_l.max(c_pl);
        peak_r = peak_r.max(c_pr);
        curr_offset += chunk;
        curr_out_offset += c_out;
        remaining -= chunk;
    }
    (total_out, last_gate, peak_l, peak_r)
}

/// Shared kernel of both gate-closed paths: drains up to `n_samples` tail
/// samples of the cab-sim IR ring-out into `buf_out_*` and applies the
/// unity-gate output stage plus the output gain ramp to the drained head.
///
/// Returns the number of tail samples actually drained. On return,
/// `buf_out_l[..n_samples]`/`buf_out_r[..n_samples]` are fully defined:
/// the drained head followed by a zero-fill suffix (strict cardinality —
/// every sub-block must deliver exactly `n_samples` host samples, and no
/// stale/sentinel residue may reach the host). When nothing was drained
/// (no active adapter, passthrough, or budget spent) the smoother is left
/// untouched and the buffers are pure zeros; callers then switch to true
/// silence.
///
/// The ring-out is intentional signal, not noise floor — the output stage
/// must not multiply it by the (now closed) noise-gate multiplier of 0. A
/// fresh unity gate (Open, multiplier 1.0, steady) yields exactly the wet
/// output gain + smoothing while letting the IR ring to completion; the real
/// gate FSM keeps tracking the input independently.
///
/// Cold path: entered only when the noise gate closes while a cab-sim IR is
/// still ringing — never on the steady-state audio path.
#[cold]
#[inline(never)]
fn drain_tail_into(
    n_samples: usize,
    ctx: &mut DspPipelineContext<'_>,
    process_mono: bool,
    smoother_out: &mut ParamSmoother,
    scratch: &mut SubBlockScratch<'_>,
    model_output_mult_adj: f32,
    shared_sample_rate: u32,
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
    let drain = n_samples.min(ctx.conv.as_ref().map_or(0, |conv| {
        if conv.is_passthrough() {
            0
        } else {
            conv.remaining_tail_samples()
        }
    }));

    if drain > 0 {
        if let Some(ref mut conv) = ctx.conv {
            conv.drain_tail(&mut buf_out_l[..drain], Some(ctx.rt_status));
        }
        if !process_mono {
            buf_out_r[..drain].copy_from_slice(&buf_out_l[..drain]);
        }
        apply_output_stage(
            &mut buf_out_l[..drain],
            &mut buf_out_r[..drain],
            drain,
            model_output_mult_adj,
            &mut DynamicHysteresis::new(),
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
            process_mono,
        );
    }

    // Strict cardinality (covers the `drain == 0` case with a full zero-fill):
    // the ring-out only covers `drain` samples; zero-fill the suffix so no
    // stale/sentinel residue reaches the host and `output_offset` advances by
    // the full sub-block size.
    if drain < n_samples {
        buf_out_l[drain..n_samples].fill(0.0);
        buf_out_r[drain..n_samples].fill(0.0);
    }

    drain
}

/// Drains the cab-sim IR tail ring-out after the noise gate closes.
///
/// The [`CabSimAdapter`] owns the ring-out budget (armed by `rearm_tail` on the
/// active-audio path): when a budget remains, `drain_tail` feeds its own
/// zero-input partitions through the engine and renders the decaying IR
/// response directly into the output scratch. The drain never re-arms, so the
/// ring-out terminates; once the budget is spent (or there is no active
/// adapter) the output switches to true silence.
///
/// Cold path: entered only when the noise gate closes while a cab-sim IR is
/// still ringing — never on the steady-state audio path.
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
    scratch: &mut SubBlockScratch<'_>,
    model_output_mult_adj: f32,
    shared_sample_rate: u32,
) -> (usize, GateState, f32, f32) {
    let drained = drain_tail_into(
        n_samples,
        ctx,
        process_mono,
        smoother_out,
        scratch,
        model_output_mult_adj,
        shared_sample_rate,
    );

    if drained == 0 {
        let (peak_l, peak_r) =
            copy_silence_to_output(out_l, out_r, output_offset, n_samples, process_mono);
        return (n_samples, GateState::Closed, peak_l, peak_r);
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
    let (peak_l, peak_r) = copy_output_from_sub_block(
        out_l,
        out_r,
        buf_out_l,
        buf_out_r,
        n_samples,
        output_offset,
        process_mono,
    );

    (n_samples, GateState::Closed, peak_l, peak_r)
}

/// Wet pipeline for sub-blocks under an active bypass crossfade.
///
/// Runs the full wet pipeline and blends it against the latency-compensated
/// dry capture in `buf_xfade_dry_*`.
///
/// Cold path: bypass transitions are rare host actions, so this body must not
/// be fused into the steady-state `process` code (see `process_sub_block`).
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
    input_clipped: &mut bool,
    smoother_in: &mut ParamSmoother,
    smoother_out: &mut ParamSmoother,
    model_output_mult_adj: f32,
    shared_sample_rate: u32,
) -> (usize, GateState, f32, f32) {
    // The scratch borrows are re-taken per phase (rather than destructured once
    // at the top) because the gate-closed phase passes `scratch` whole to the
    // shared `drain_tail_into` kernel: holding the field borrows across that
    // call would alias `*scratch` twice. Re-destructuring is zero-cost — the
    // struct itself never materializes in the binary.
    let dry_n = n_samples.min(scratch.buf_xfade_dry_l.len());
    debug_assert_eq!(
        dry_n, n_samples,
        "dry delay output must cover the full sub-block (n_samples={n_samples})"
    );

    // 2. Run full wet pipeline
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
        // Shared gate-closed kernel (unity-gate ring-out + strict-cardinality
        // zero-fill). The crossfade blends the drained/zeroed wet scratch
        // against the dry capture below, so the full wet head must be defined
        // even when the tail budget is already spent (`drained == 0`).
        let _drained = drain_tail_into(
            n_samples,
            ctx,
            process_mono,
            smoother_out,
            scratch,
            model_output_mult_adj,
            shared_sample_rate,
        );
        n_samples
    } else {
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

        if let Some(ref mut conv) = ctx.conv
            && !conv.is_passthrough()
        {
            // The IR operates directly on the final output scratch (see the
            // main-path comment in `process_sub_block`): the block-agnostic
            // driver keeps the crossfade wet stream invariant to host
            // block-size variations as well.
            conv.process_block(&mut scratch.buf_out_l[..n_o], Some(ctx.rt_status));
            // Re-arm the tail budget for active audio (see the main-path
            // comment in `process_sub_block`).
            conv.rearm_tail();
            if !process_mono {
                scratch.buf_out_r[..n_o].copy_from_slice(&scratch.buf_out_l[..n_o]);
            }
        }

        apply_output_stage(
            &mut scratch.buf_out_l[..n_o],
            &mut scratch.buf_out_r[..n_o],
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

    // 3. Crossfade blend: output = dry * (1 - mix_i) + wet * mix_i
    // With the strict-cardinality streaming adapter, `n_out == n_samples ==
    // dry_n` always, so the wet count never exceeds the dry capture. The clamps
    // below remain as cheap defensive guards.
    let n_xfade_raw = n_out.min(crossfader.remaining);
    let n_xfade = n_xfade_raw.min(dry_n);
    debug_assert!(
        n_xfade <= dry_n,
        "n_xfade ({n_xfade}) exceeded dry_n ({dry_n}) in bypass crossfade"
    );
    let step = crossfader.step;
    let mut mix = crossfader.mix;

    // Blended portion (first n_xfade samples): ramp from current mix towards target.
    // Bounds pre-validated above — slices are disjoint.
    // Note on SIMD vectorization: under strict IEEE-754 semantics (prohibiting
    // non-compliant `-ffast-math`, R1), the sequential scalar recurrence `mix += step`
    // precludes compiler auto-vectorization across SIMD lanes. The loop executes as
    // scalar branchless FMA instructions (`vfmadd213ss`). Because `process_crossfade_sub_block`
    // is a `#[cold]` path running only during rare bypass transitions (typically <= 64 samples,
    // ~20-30 ns, < 1% of non-model block overhead), scalar execution avoids complexity and
    // ULP drift while remaining well below the SP-P3.4 relevance threshold.
    //
    // The four slices are reborrowed here (not held from the wet leg above)
    // so the mutable blend borrows provably end before the pure-portion loop
    // and the final host copy below. Re-slicing from `scratch` is zero-cost.
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

    // Overflow region (n_xfade..n_xfade_raw): wet count exceeded the dry
    // capture. Legacy guard semantics: dry = 0.0 → wet scaled by the ramp mix.
    for i in n_xfade..n_xfade_raw {
        scratch.buf_out_l[i] *= mix;
        scratch.buf_out_r[i] *= mix;
        mix += step;
    }

    // Pure portion (remaining n_out - n_xfade_raw samples): final mix value
    let final_mix = if crossfader.target { 0.0 } else { 1.0 };
    if (final_mix - 1.0f32).abs() > f32::EPSILON {
        // final_mix is 0.0 (dry target): copy dry to output, zero-fill any excess beyond dry_n
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
    // If final_mix is 1.0 (wet target): buf_out already has wet, nothing to do

    crossfader.mix = mix;
    crossfader.mix = crossfader.mix.clamp(0.0, 1.0);
    crossfader.remaining = crossfader.remaining.saturating_sub(n_xfade_raw);
    if crossfader.remaining == 0 {
        crossfader.active = false;
        crossfader.mix = final_mix;
    }

    // 4. Copy blended result to output
    let (peak_l, peak_r) = copy_output_from_sub_block(
        out_l,
        out_r,
        scratch.buf_out_l,
        scratch.buf_out_r,
        n_out,
        output_offset,
        process_mono,
    );

    (n_out, gate_state, peak_l, peak_r)
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

    // F-PERF-15 fast path: for mono (`process_mono`), the R channel
    // duplicates L everywhere downstream (see `extract_channels`) and the
    // stereo engine stages short-circuit on `ctx.process_mono` — an extra R
    // pass would touch a second cache line and a second dispatch frame for
    // identical samples. Mono therefore touches L only and mirrors L→R once
    // at the end (the DSP stages already do this via `ctx.process_mono`; the
    // mirror below extends it to the plugin-level IIR ramps, pure gain, and
    // delay-line dry feeds).
    #[cfg(feature = "dual-mono")]
    let stereo = !process_mono;
    #[cfg(not(feature = "dual-mono"))]
    let _stereo = false;
    #[cfg(not(feature = "dual-mono"))]
    let _ = process_mono;
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

    #[cfg(feature = "dual-mono")]
    {
        for i in 0..n {
            let gain = target + bp * diff;
            // SAFETY: i is bounded by 0..n where slice_l and slice_r have length n.
            unsafe {
                let p_l = slice_l.get_unchecked_mut(i);
                *p_l *= gain;
                if stereo {
                    let p_r = slice_r.get_unchecked_mut(i);
                    *p_r *= gain;
                    if detect_clip && ((*p_l).abs() > 1.0 || (*p_r).abs() > 1.0) {
                        *input_clipped = true;
                    }
                } else if detect_clip && (*p_l).abs() > 1.0 {
                    *input_clipped = true;
                }
            }
            bp *= beta;
        }
    }
    #[cfg(not(feature = "dual-mono"))]
    {
        let _ = &slice_r;
        let _ = buf_r;
        let _ = _stereo;
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
) -> (f32, f32) {
    if let Some(o_l) = out_l {
        let end = (output_offset + n_samples).min(o_l.len());
        o_l[output_offset..end].fill(0.0);
    }
    if let Some(o_r) = out_r {
        let end = (output_offset + n_samples).min(o_r.len());
        o_r[output_offset..end].fill(0.0);
    }
    (0.0, 0.0)
}

/// Copies the wet model output to the host buffers while sanitizing non-finites
/// and computing absolute peak in a single fused pass (SP-P2.4 / F-PERF-16).
///
/// Containment and peak detection happen directly during the transfer to host:
/// `sanitize_copy_peak` reads the scratch buffer, sanitizes non-finites to 0.0,
/// writes directly to the host output, and calculates the peak absolute value.
/// This eliminates redundant passes over memory.
///
/// When `process_mono` is active, `buf_out_l` is lazily mirrored to `out_r`
/// directly at host transfer time, keeping `buf_out_r` untouched.
#[inline(always)]
pub(crate) fn copy_output_from_sub_block(
    out_l: &mut Option<&mut [f32]>,
    out_r: &mut Option<&mut [f32]>,
    buf_out_l: &mut [f32],
    buf_out_r: &mut [f32],
    n_out: usize,
    output_offset: usize,
    process_mono: bool,
) -> (f32, f32) {
    let mut peak_l = 0.0f32;
    if let Some(o_l) = out_l {
        let n = n_out.min(o_l.len().saturating_sub(output_offset));
        peak_l = sanitize_copy_peak(&buf_out_l[..n], &mut o_l[output_offset..output_offset + n]);
    }
    let mut peak_r = 0.0f32;
    if let Some(o_r) = out_r {
        let src: &[f32] = if process_mono { buf_out_l } else { buf_out_r };
        let n = n_out.min(o_r.len().saturating_sub(output_offset));
        peak_r = sanitize_copy_peak(&src[..n], &mut o_r[output_offset..output_offset + n]);
    }
    (peak_l, peak_r)
}

/// Copies the latency-compensated dry signal to the output in the
/// fully-bypassed state while sanitizing non-finites and computing peak
/// in a single fused pass (SP-P2.4).
#[inline(always)]
pub(crate) fn copy_delayed_dry_to_output(
    out_l: &mut Option<&mut [f32]>,
    out_r: &mut Option<&mut [f32]>,
    dry_l: &mut [f32],
    dry_r: &mut [f32],
    output_offset: usize,
    process_mono: bool,
) -> (f32, f32) {
    let mut peak_l = 0.0f32;
    if let Some(o_l) = out_l {
        let n = dry_l.len().min(o_l.len().saturating_sub(output_offset));
        peak_l = sanitize_copy_peak(&dry_l[..n], &mut o_l[output_offset..output_offset + n]);
    }
    let mut peak_r = 0.0f32;
    if let Some(o_r) = out_r {
        let src: &[f32] = if process_mono { dry_l } else { dry_r };
        let n = src.len().min(o_r.len().saturating_sub(output_offset));
        peak_r = sanitize_copy_peak(&src[..n], &mut o_r[output_offset..output_offset + n]);
    }
    (peak_l, peak_r)
}
