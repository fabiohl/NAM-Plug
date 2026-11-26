// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

impl<'a> NamClapProcessor<'a> {
    #[expect(clippy::too_many_arguments)]
    pub(super) fn process_wet_sub_blocks(
        &mut self,
        slice_base: usize,
        n_samples: usize,
        mut out_l: Option<&mut [f32]>,
        mut out_r: Option<&mut [f32]>,
        event_idx: &mut usize,
        metrics: &mut BlockMetricsAccumulator,
        last_gate_state: &mut GateState,
        any_active: &mut bool,
        input_clipped: &mut bool,
        is_last_slice: bool,
    ) {
        let mut gain_steps = [worker::GainStep {
            offset: 0,
            target: 0.0,
            is_input: true,
        }; MAX_GAIN_STEPS];

        let mut block_offset = 0usize;
        let mut output_offset = 0usize;
        let shared_sample_rate = self.shared.cold.sample_rate.load(Ordering::Relaxed);
        let event_count = self.scheduled_events.len();

        while block_offset < n_samples {
            let current_abs = slice_base + block_offset;
            let mut out_of_order_applied = false;
            while *event_idx < event_count && self.scheduled_events[*event_idx].time < current_abs {
                // Out-of-order or late timestamp (arrived with time < current_abs).
                // Defensive containment: clamp time to current_abs and apply immediately
                // so no parameter update is silently discarded (zero event loss).
                core::hint::cold_path();
                self.rt_status.set_flag(RT_STATUS_EVENT_TIMING_ANOMALY);
                self.scheduled_events[*event_idx].time = current_abs;
                let evt = self.scheduled_events[*event_idx];
                self.param_target()
                    .apply(evt.param_id, evt.value, evt.is_mod, true);
                *event_idx += 1;
                out_of_order_applied = true;
            }

            if out_of_order_applied {
                self.refresh_gate_config();
                self.bypass_xfade.trigger(self.params.bypass);
                self.cab_bypass_xfade.trigger(self.cab_bypass);
            }

            // Resolve the next sub-block boundary: the first event that
            // requires an exact split (any non-gain param), or the
            // gain-step cap. Gain events in `[current_abs, slice_base + sub_end)` are
            // fused into this sub-block and applied at their exact sample
            // offsets through the piecewise IIR ramp, so the inference
            // call is not fragmented by automation of smoothed gains.
            let fuse = MIN_SPLIT_SAMPLES > 1
                && self.scheduled_events_sorted
                && n_samples <= neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF
                && !self.bypass_xfade.active
                && !self.params.bypass;
            let mut sub_end = n_samples;
            {
                let mut i = *event_idx;
                let mut gains = 0usize;
                while i < event_count {
                    let t = self.scheduled_events[i].time;
                    if t < current_abs {
                        i += 1;
                        continue;
                    }
                    if t >= slice_base + n_samples {
                        break;
                    }
                    let rel_t = t - slice_base;
                    if !fuse || event_requires_boundary(self.scheduled_events[i].param_id) {
                        sub_end = rel_t;
                        break;
                    }
                    gains += 1;
                    if gains > MAX_GAIN_STEPS {
                        sub_end = rel_t;
                        break;
                    }
                    i += 1;
                }
            }

            let mut n_steps = 0usize;
            let sub_n = sub_end - block_offset;

            // Stage the fused gain events without jumping the smoother:
            // the target for each offset is replayed inside the ramp.
            if fuse && sub_n > 0 {
                while *event_idx < event_count {
                    let t = self.scheduled_events[*event_idx].time;
                    if t >= slice_base + sub_end {
                        break;
                    }
                    let param_id = self.scheduled_events[*event_idx].param_id;
                    if event_requires_boundary(param_id) {
                        break;
                    }
                    let value = self.scheduled_events[*event_idx].value;
                    let is_mod = self.scheduled_events[*event_idx].is_mod;
                    let gain_target = self.param_target().apply(param_id, value, is_mod, false);
                    // `is_fusable_gain_param` is the single source of truth
                    // for both this loop and the `Some(GainTarget)` arms of
                    // `ParamTarget::apply`; a `None` here means the two
                    // drifted and an event would be silently dropped.
                    debug_assert!(
                        gain_target.is_some(),
                        "fusable gain param returned no target (classification drift)"
                    );
                    if let Some(gt) = gain_target
                        && n_steps < MAX_GAIN_STEPS
                    {
                        gain_steps[n_steps] = worker::GainStep {
                            offset: (t - current_abs) as u32,
                            target: gt.target,
                            is_input: gt.is_input,
                        };
                        n_steps += 1;
                    }
                    *event_idx += 1;
                }
            }

            if sub_n > 0 {
                let bypass = self.params.bypass;
                let process_mono = self.process_mono;
                let is_stereo_config = self
                    .shared
                    .cold
                    .selected_port_config
                    .load(Ordering::Relaxed)
                    == crate::clap::plugin::CONFIG_ID_STEREO;
                let mut dummy_none_model: Option<Box<neural_amp_modeler_rs::models::StaticModel>> =
                    None;
                let active_model_r: &mut Option<Box<neural_amp_modeler_rs::models::StaticModel>> =
                    if is_stereo_config {
                        &mut self.model_r
                    } else {
                        &mut dummy_none_model
                    };

                let sub_m = {
                    let mut ctx = DspPipelineContext {
                        resampler: &mut self.resampler,
                        os_l: &mut self.os_l,
                        os_r: &mut self.os_r,
                        active_model_l: &mut self.model_l,
                        active_model_r,
                        input_gain_mult: self.model_input_mult_adj,
                        output_gain_mult: self.model_output_mult_adj,
                        gate_params: &self.cached_gate_params,
                        silence_hysteresis: &mut self.silence_hyst,
                        mono_hysteresis: &mut self.mono_hyst,
                        threshold_open_sq: self.cached_threshold_open_sq,
                        threshold_close_sq: self.cached_threshold_close_sq,
                        process_mono: &mut self.process_mono,
                        rt_status: &self.rt_status,
                        adaptive: &mut self.adaptive_compute,
                        bridge_writer: None,
                        conv: None,
                        conv_pair: None,
                    };

                    let mut scratch = audio_loop::SubBlockScratch {
                        buf_host_l: &mut self.buf_host_l,
                        buf_host_r: &mut self.buf_host_r,
                        buf_out_l: &mut self.buf_out_l,
                        buf_out_r: &mut self.buf_out_r,
                        buf_os_in_l: &mut self.buf_os_in_l,
                        buf_os_in_r: &mut self.buf_os_in_r,
                        buf_os_model_l: &mut self.buf_os_model_l,
                        buf_os_model_r: &mut self.buf_os_model_r,
                        buf_xfade_dry_l: &mut self.buf_xfade_dry_l,
                        buf_xfade_dry_r: &mut self.buf_xfade_dry_r,
                        buf_xfd_scratch_l: &mut self.buf_xfd_scratch_l,
                        buf_xfd_scratch_r: &mut self.buf_xfd_scratch_r,
                    };
                    let mut cab_stage = audio_loop::CabinetStage {
                        cab_slot: &mut self.cab_slot,
                        cab_bypass: self.cab_bypass,
                        cab_needs_reset: &mut self.cab_needs_reset,
                        cab_nam_dirty: &mut self.cab_nam_dirty,
                        cab_nam_generation: &mut self.cab_nam_generation,
                        cab_nam_rebuild_in_flight: &mut self.cab_nam_rebuild_in_flight,
                        cab_nam_rebuild_req: &self.shared.cold.cab_nam_rebuild_req,
                        cab_bypass_xfade: &mut self.cab_bypass_xfade,
                        cab_dry_delay: &mut self.cab_dry_delay,
                        buf_cab_dry_l: &mut self.buf_cab_dry_l,
                        buf_cab_dry_r: &mut self.buf_cab_dry_r,
                        buf_cab_nam_in_l: &mut self.buf_cab_nam_in_l,
                        buf_cab_nam_in_r: &mut self.buf_cab_nam_in_r,
                        buf_cab_zero_l: &self.buf_cab_zero_l,
                        buf_cab_zero_r: &self.buf_cab_zero_r,
                    };

                    audio_loop::process_sub_block(
                        block_offset,
                        sub_n,
                        &mut out_l,
                        &mut out_r,
                        output_offset,
                        &mut ctx,
                        &mut self.stream,
                        bypass,
                        process_mono,
                        &mut self.bypass_xfade,
                        &mut self.dry_delay,
                        &mut scratch,
                        &mut cab_stage,
                        input_clipped,
                        &mut self.smoother_in,
                        &mut self.smoother_out,
                        &mut self.smoother_cal_in,
                        &mut self.smoother_cal_out,
                        shared_sample_rate,
                        &gain_steps[..n_steps],
                        self.gate_mode,
                        &mut self.musical_gate,
                    )
                };

                if *input_clipped {
                    self.shared
                        .rt_to_ui
                        .ui_in_clipped
                        .store(true, Ordering::Relaxed);
                    self.shared
                        .rt_to_ui
                        .ui_in_clip_indicator
                        .store(true, Ordering::Relaxed);
                    self.shared
                        .rt_to_ui
                        .ui_clipped
                        .store(true, Ordering::Relaxed);
                    self.shared
                        .rt_to_ui
                        .ui_clip_indicator
                        .store(true, Ordering::Relaxed);
                }

                output_offset += sub_m.n_out;

                metrics.in_peak_l = metrics.in_peak_l.max(sub_m.in_peak_l);
                metrics.in_peak_r = metrics.in_peak_r.max(sub_m.in_peak_r);
                metrics.in_sum_sq_l += sub_m.in_sum_sq_l;
                metrics.in_sum_sq_r += sub_m.in_sum_sq_r;
                metrics.out_peak_l = metrics.out_peak_l.max(sub_m.out_peak_l);
                metrics.out_peak_r = metrics.out_peak_r.max(sub_m.out_peak_r);
                metrics.out_sum_sq_l += sub_m.out_sum_sq_l;
                metrics.out_sum_sq_r += sub_m.out_sum_sq_r;
                metrics.total_samples += sub_m.n_out;

                if sub_m.gate_state != GateState::Closed {
                    *last_gate_state = sub_m.gate_state;
                    *any_active = true;
                }
            }

            while *event_idx < event_count
                && self.scheduled_events[*event_idx].time == slice_base + sub_end
                && sub_end < n_samples
            {
                let evt = self.scheduled_events[*event_idx];
                self.param_target()
                    .apply(evt.param_id, evt.value, evt.is_mod, true);
                *event_idx += 1;
            }

            self.refresh_gate_config();

            // Trigger bypass crossfade if the bypass state changed via
            // a host event at this sub-block boundary.
            self.bypass_xfade.trigger(self.params.bypass);
            self.cab_bypass_xfade.trigger(self.cab_bypass);

            block_offset = sub_end;
        }

        // Process remaining events with time >= slice_base + n_samples at the block close boundary.
        // Clamped defensively to slice_base + n_samples to guarantee zero silent event loss,
        // asserting RT_STATUS_EVENT_TIMING_ANOMALY.
        if is_last_slice {
            let mut remaining_applied = false;
            let end_abs = slice_base + n_samples;
            while *event_idx < event_count {
                core::hint::cold_path();
                self.rt_status.set_flag(RT_STATUS_EVENT_TIMING_ANOMALY);
                self.scheduled_events[*event_idx].time = end_abs;
                let evt = self.scheduled_events[*event_idx];
                self.param_target()
                    .apply(evt.param_id, evt.value, evt.is_mod, true);
                *event_idx += 1;
                remaining_applied = true;
            }

            if remaining_applied {
                self.refresh_gate_config();
                self.bypass_xfade.trigger(self.params.bypass);
                self.cab_bypass_xfade.trigger(self.cab_bypass);
            }
        }
    }
}
