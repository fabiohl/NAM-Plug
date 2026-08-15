// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

pub mod audio_loop;
pub mod worker;

pub(crate) use worker::ScheduledEvent;

use super::super::NamClapProcessor;
use crate::clap::processor::dsp::{channels, peaks};
use clack_plugin::events::event_types::{ParamModEvent, ParamValueEvent};
use clack_plugin::prelude::*;
use neural_amp_modeler_rs::common::spsc::RT_STATUS_HOST_CONTRACT_VIOLATION;
use neural_amp_modeler_rs::dsp::gate::GateState;
use neural_amp_modeler_rs::dsp::gate_flags;
use neural_amp_modeler_rs::dsp::pipeline::DspPipelineContext;
use std::sync::atomic::Ordering;

const MAX_SCHEDULED_EVENTS: usize = 4096;

impl<'a> NamClapProcessor<'a> {
    #[inline(always)]
    pub(crate) fn process_dsp_audio(
        &mut self,
        audio: &mut Audio,
        input_events: &InputEvents,
        start_nanos: u64,
    ) -> Result<ProcessStatus, PluginError> {
        // S4-E4-T02: track pending restart factor for latency-policy enforcement.
        let pending_before = self
            .shared
            .cold
            .pending_restart_os_factor
            .load(Ordering::Relaxed);
        {
            let events = &mut self.scheduled_events;
            events.clear();

            for event in input_events {
                if events.len() >= MAX_SCHEDULED_EVENTS {
                    debug_assert!(
                        false,
                        "CLAP-F007: event flood > {MAX_SCHEDULED_EVENTS} in one block; truncating"
                    );
                    break;
                }
                let time = event.header().time() as usize;
                if let Some(param_event) = event.as_event::<ParamValueEvent>() {
                    let Some(clap_id) = param_event.param_id() else {
                        continue;
                    };
                    events.push(ScheduledEvent {
                        time,
                        param_id: clap_id.get(),
                        value: param_event.value() as f32,
                        is_mod: false,
                    });
                } else if let Some(mod_event) = event.as_event::<ParamModEvent>() {
                    let Some(clap_id) = mod_event.param_id() else {
                        continue;
                    };
                    events.push(ScheduledEvent {
                        time,
                        param_id: clap_id.get(),
                        value: mod_event.amount() as f32,
                        is_mod: true,
                    });
                }
            }
        }

        let event_count = self.scheduled_events.len();
        let mut event_idx = 0;

        for mut port_pair in audio {
            let n_samples_raw = port_pair.frames_count() as usize;
            if n_samples_raw > self.max_frames_count {
                debug_assert!(
                    false,
                    "Host contract violation: n_samples={n_samples_raw} > max_frames_count={}",
                    self.max_frames_count
                );
                self.rt_status.set_flag(RT_STATUS_HOST_CONTRACT_VIOLATION);
            }
            let n_samples = n_samples_raw.min(self.max_frames_count);
            if n_samples == 0 {
                continue;
            }
            let n = n_samples as u32;
            if self.rt_status.last_n_samples.load(Ordering::Relaxed) != n {
                self.rt_status.last_n_samples.store(n, Ordering::Relaxed);
            }

            let Some((mut out_l, mut out_r)) = channels::extract_channels(
                &mut port_pair,
                &mut self.buf_host_l,
                &mut self.buf_host_r,
                &self.shared.rt_to_ui.active_channel_count,
                &mut self.process_mono,
                n_samples,
            )?
            else {
                continue;
            };

            if self
                .shared
                .ui_to_rt
                .host_r_deactivated
                .load(Ordering::Acquire)
            {
                self.process_mono = true;
            }

            if self.gate_dirty {
                let modulated_gate_db = self.params.gate_threshold_db + self.mod_gate_thresh;
                let close_db = modulated_gate_db - 6.0;
                self.cached_threshold_open_sq =
                    self.gain_lut.db_to_linear(modulated_gate_db).powi(2);
                self.cached_threshold_close_sq = self.gain_lut.db_to_linear(close_db).powi(2);
                self.cached_gate_params.threshold_open_db = modulated_gate_db;
                self.cached_gate_params.threshold_close_db = close_db;
                self.gate_dirty = false;
            }

            let model_load_fail = self.model_l.is_none() && !self.params.bypass;
            let current_fail_flag = self
                .rt_status
                .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_MODEL_LOAD_FAILED);
            if model_load_fail != current_fail_flag {
                if model_load_fail {
                    self.rt_status
                        .set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_MODEL_LOAD_FAILED);
                } else {
                    self.rt_status.clear_flag(
                        neural_amp_modeler_rs::common::spsc::RT_STATUS_MODEL_LOAD_FAILED,
                    );
                }
            }

            let mut block_offset = 0usize;
            let mut output_offset = 0usize;
            let mut peak_l = 0.0f32;
            let mut peak_r = 0.0f32;
            let mut last_gate_state = GateState::Open;
            let mut any_active = false;
            let model_output_mult_adj = self.model_output_mult_adj;
            let shared_sample_rate = self.shared.cold.sample_rate.load(Ordering::Relaxed);

            let mut input_clipped = false;

            // Sync crossfader with current bypass state. If bypass was changed
            // by SPSC event sync (process_events) before this block, trigger
            // the crossfade to avoid click artifacts.
            self.bypass_xfade.trigger(self.params.bypass);

            while block_offset < n_samples {
                while event_idx < event_count
                    && self.scheduled_events[event_idx].time < block_offset
                {
                    event_idx += 1;
                }

                let sub_end = if event_idx < event_count {
                    let et = self.scheduled_events[event_idx].time;
                    if et < n_samples { et } else { n_samples }
                } else {
                    n_samples
                };

                let sub_n = sub_end - block_offset;
                if sub_n > 0 {
                    let bypass = self.params.bypass;
                    let process_mono = self.process_mono;

                    let (n_out, gate_state) = {
                        let mut ctx = DspPipelineContext {
                            resampler: &mut self.resampler,
                            os_l: &mut self.os_l,
                            os_r: &mut self.os_r,
                            active_model_l: &mut self.model_l,
                            active_model_r: &mut None,
                            input_gain_mult: self.model_input_mult_adj,
                            output_gain_mult: model_output_mult_adj,
                            gate_params: &self.cached_gate_params,
                            silence_hysteresis: &mut self.silence_hyst,
                            mono_hysteresis: &mut self.mono_hyst,
                            threshold_open_sq: self.cached_threshold_open_sq,
                            threshold_close_sq: self.cached_threshold_close_sq,
                            process_mono: &mut self.process_mono,
                            rt_status: &self.rt_status,
                            adaptive: &mut self.adaptive_compute,
                            bridge_writer: None,
                            conv: self.cabsim_adapter.as_mut(),
                        };

                        audio_loop::process_sub_block(
                            block_offset,
                            sub_n,
                            &mut out_l,
                            &mut out_r,
                            output_offset,
                            &mut ctx,
                            bypass,
                            process_mono,
                            &mut self.bypass_xfade,
                            &mut self.buf_xfade_dry_l,
                            &mut self.buf_xfade_dry_r,
                            &mut self.buf_xfd_scratch_l,
                            &mut self.buf_xfd_scratch_r,
                            &mut input_clipped,
                            &mut self.smoother_in,
                            &mut self.smoother_out,
                            &mut self.buf_host_l,
                            &mut self.buf_host_r,
                            &mut self.buf_mid_l,
                            &mut self.buf_mid_r,
                            &mut self.buf_out_l,
                            &mut self.buf_out_r,
                            &mut self.buf_model_l,
                            &mut self.buf_model_r,
                            &mut self.buf_os_in_l,
                            &mut self.buf_os_in_r,
                            &mut self.buf_os_model_l,
                            &mut self.buf_os_model_r,
                            model_output_mult_adj,
                            shared_sample_rate,
                            self.gain_lut,
                            &mut self.cabsim_tail_remaining,
                        )
                    };

                    if input_clipped {
                        self.shared
                            .rt_to_ui
                            .ui_clipped
                            .store(true, Ordering::Relaxed);
                    }

                    output_offset += n_out;

                    if let (Some(o_l), Some(o_r)) = (&out_l, &out_r) {
                        let o_start = output_offset - n_out;
                        let o_end = o_start + n_out;
                        let avail_l = o_l.len().min(o_end).saturating_sub(o_start);
                        let avail_r = o_r.len().min(o_end).saturating_sub(o_start);
                        let n = avail_l.min(avail_r);
                        if n > 0 {
                            let (pl, pr) = unsafe {
                                neural_amp_modeler_rs::math::dsp::stereo::compute_peak_abs_stereo(
                                    &o_l[o_start..o_start + n],
                                    &o_r[o_start..o_start + n],
                                )
                            };
                            peak_l = peak_l.max(pl);
                            peak_r = peak_r.max(pr);
                        }
                    } else if let Some(o_l) = &out_l {
                        let o_start = output_offset - n_out;
                        let o_end = o_start + n_out;
                        let avail = o_l.len().min(o_end).saturating_sub(o_start);
                        if avail > 0 {
                            let (pl, _) = unsafe {
                                neural_amp_modeler_rs::math::dsp::stereo::compute_peak_abs_stereo(
                                    &o_l[o_start..o_start + avail],
                                    &o_l[o_start..o_start + avail],
                                )
                            };
                            peak_l = peak_l.max(pl);
                            peak_r = peak_r.max(pl);
                        }
                    }

                    if gate_state != GateState::Closed {
                        last_gate_state = gate_state;
                        any_active = true;
                    }
                }

                while event_idx < event_count && self.scheduled_events[event_idx].time == sub_end {
                    let evt = &self.scheduled_events[event_idx];
                    worker::apply_scheduled_event(
                        evt.param_id,
                        evt.value,
                        evt.is_mod,
                        &mut self.params,
                        &mut self.smoother_in,
                        &mut self.smoother_out,
                        &mut self.gate_dirty,
                        &mut self.mod_input_gain,
                        &mut self.mod_output_gain,
                        &mut self.mod_gate_thresh,
                        &mut self.adaptive_compute,
                        &self.rt_status,
                        &self.shared.ui_to_rt,
                        self.gain_lut,
                        self.shared.cold.buffer_size.load(Ordering::Relaxed),
                        &self.shared.cold.pending_restart_os_factor,
                    );
                    event_idx += 1;
                }

                if self.gate_dirty {
                    let modulated_gate_db = self.params.gate_threshold_db + self.mod_gate_thresh;
                    let close_db = modulated_gate_db - 6.0;
                    self.cached_threshold_open_sq =
                        self.gain_lut.db_to_linear(modulated_gate_db).powi(2);
                    self.cached_threshold_close_sq = self.gain_lut.db_to_linear(close_db).powi(2);
                    self.cached_gate_params.threshold_open_db = modulated_gate_db;
                    self.cached_gate_params.threshold_close_db = close_db;
                    self.gate_dirty = false;
                }

                // Trigger bypass crossfade if the bypass state changed via
                // a host event at this sub-block boundary.
                self.bypass_xfade.trigger(self.params.bypass);

                block_offset = sub_end;
            }

            if any_active {
                gate_flags::report_gate_flags(&self.rt_status, last_gate_state);
            } else {
                gate_flags::report_gate_flags(&self.rt_status, GateState::Closed);
            }

            peaks::store_peaks(self.shared, peak_l, peak_r);
        }

        // S4-E4-T02: if an oversampling change was detected during active
        // processing, request host restart so latency can be updated
        // legally during the next activate().
        let pending_after = self
            .shared
            .cold
            .pending_restart_os_factor
            .load(Ordering::Relaxed);
        if pending_after != pending_before && pending_after != 0 {
            self.host.request_restart();
        }

        self.process_telemetry(start_nanos);

        #[cfg(feature = "heap-audit")]
        if neural_amp_modeler_rs::common::alloc_audit::AUDIT_ENABLED.load(Ordering::Relaxed) {
            let allocs = neural_amp_modeler_rs::common::alloc_audit::get_alloc_count();
            if allocs > 0 {
                self.rt_status
                    .set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HEAP_ALLOC);
                return Ok(ProcessStatus::Sleep);
            }
        }

        Ok(ProcessStatus::Continue)
    }
}
