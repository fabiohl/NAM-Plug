// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

impl<'a> NamClapProcessor<'a> {
    /// Containment leg of the two-phase reset amortization window: renders
    /// one callback while the split-stabilization pass armed by the
    /// phase-zero `reset()` is still outstanding, so the non-converged model
    /// can never leak wet audio into the mix.
    ///
    /// The block is held out of the DSP pipeline with bypass-leg semantics:
    /// the staged host input feeds the latency-compensated dry delay
    /// (preserving the ring history the wet path rejoins on the first
    /// post-convergence block) and the delayed dry is published as the
    /// output, while the bypass crossfader trigger, the gate/adaptive FSMs
    /// and the smoothers stay untouched — host audio neither reaches the
    /// model nor advances any temporal state, so the first wet block after
    /// convergence equals a fresh instance's first block.
    ///
    /// Host parameter events keep applying (zero event loss): every
    /// scheduled event is applied in delivery order as the callback's
    /// end-of-block parameter state, mirroring the wet pipeline's
    /// defensive-fallback semantics. Intra-block sample accuracy is not
    /// observable because the block's audio bypasses the pipeline. The
    /// bypass crossfader trigger is deliberately not fired: a crossfade
    /// ramping mid-window would blend non-converged wet audio; the first
    /// wet block re-triggers from the held crossfader state instead.
    ///
    /// Telemetry covers the window: the callback is measured like any other
    /// block, so the drain cost lands in the deadline histogram and the
    /// adaptive FSM.
    #[cold]
    // Standalone audit unit for `verify_rt_codegen.sh` (the gate watches the
    // symbol; inlining into `process()` would hide the leg's .text).
    #[inline(never)]
    pub(crate) fn process_dry_contained_block(
        &mut self,
        audio: &mut Audio,
        input_events: &InputEvents,
        start_nanos: u64,
    ) -> Result<ProcessStatus, PluginError> {
        self.schedule_input_events(input_events);

        // Apply every scheduled event in delivery order (the wet path's
        // defensive fallback applies leftovers the same way, in ring order
        // with clamped times). The bypass crossfader trigger is held (see
        // the doc comment above).
        for index in 0..self.scheduled_events.len() {
            let (param_id, value, is_mod) = {
                let event = &self.scheduled_events[index];
                (event.param_id, event.value, event.is_mod)
            };
            self.param_target().apply(param_id, value, is_mod, true);
        }

        // Parameter applications can invalidate the gate coefficients; keep
        // the cached gate parameters current so the first wet block starts
        // with the same gate math the wet pipeline would have.
        self.refresh_gate_config();

        let mut metrics = BlockMetricsAccumulator::default();
        self.for_each_port_slice(
            audio,
            |proc, _slice_start, slice_len, _is_last, mut out_l, mut out_r| {
                proc.run_dry_containment_slice(&mut out_l, &mut out_r, slice_len, &mut metrics);
            },
        )?;

        // Bypass-leg telemetry projection: gate reported open, no FSM
        // advance, peaks from the delayed dry.
        if self.gate_mode == GateMode::Musical {
            self.musical_gate.reset();
        }
        self.shared
            .rt_to_ui
            .ui_gate_active
            .store(false, Ordering::Relaxed);
        gate_flags::report_gate_flags(&self.rt_status, GateState::Open);
        self.update_rms_and_telemetry(
            &metrics,
            self.shared.cold.sample_rate.load(Ordering::Relaxed),
        );

        self.maybe_request_restart();

        self.process_telemetry(start_nanos);

        Ok(ProcessStatus::Continue)
    }

    #[cold]
    #[inline(always)]
    pub(super) fn run_dry_containment_slice(
        &mut self,
        out_l: &mut Option<&mut [f32]>,
        out_r: &mut Option<&mut [f32]>,
        n_samples: usize,
        metrics: &mut BlockMetricsAccumulator,
    ) {
        self.dry_delay.process_block(
            &self.buf_host_l[..n_samples],
            &self.buf_host_r[..n_samples],
            &mut self.buf_xfade_dry_l[..n_samples],
            &mut self.buf_xfade_dry_r[..n_samples],
            n_samples,
            self.process_mono,
        );
        let (in_peak_l, in_sum_sq_l) =
            audio_loop::compute_peak_and_sum_sq(&self.buf_host_l[..n_samples]);
        let (in_peak_r, in_sum_sq_r) = if self.process_mono {
            (in_peak_l, in_sum_sq_l)
        } else {
            audio_loop::compute_peak_and_sum_sq(&self.buf_host_r[..n_samples])
        };
        let (sub_peak_l, sub_peak_r, sub_sum_sq_l, sub_sum_sq_r) =
            audio_loop::copy_delayed_dry_to_output(
                out_l,
                out_r,
                &mut self.buf_xfade_dry_l[..n_samples],
                &mut self.buf_xfade_dry_r[..n_samples],
                0,
                self.process_mono,
            );
        metrics.in_peak_l = metrics.in_peak_l.max(in_peak_l);
        metrics.in_peak_r = metrics.in_peak_r.max(in_peak_r);
        metrics.in_sum_sq_l += in_sum_sq_l;
        metrics.in_sum_sq_r += in_sum_sq_r;
        metrics.out_peak_l = metrics.out_peak_l.max(sub_peak_l);
        metrics.out_peak_r = metrics.out_peak_r.max(sub_peak_r);
        metrics.out_sum_sq_l += sub_sum_sq_l;
        metrics.out_sum_sq_r += sub_sum_sq_r;
        metrics.total_samples += n_samples;
    }
}
