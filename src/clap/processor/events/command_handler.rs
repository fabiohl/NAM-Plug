// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! [`RtSwapHandler`] binding and cold swap handlers for [`ClapParamPayload`].

use crate::clap::extensions::cabinet::CabSlotDsp;
use crate::clap::extensions::calibration::{
    CalibrationConfig, ModelCalibrationMeta, compute_calibration_multipliers,
};
use crate::clap::extensions::gate_mode::GateMode;
use crate::clap::extensions::params::bypass_bool_to_u32;
use crate::clap::plugin::retired_box::{RetiredBox, RetiredBoxSink};
use crate::clap::plugin::shared::{LoadModelPayload, RestoreTxn};
use crate::clap::plugin::{ClapParamPayload, NamClapShared, StructuralKind};
use crate::clap::processor::dsp::dry_delay::DryDelayLine;
use crate::clap::processor::dsp::musical_gate::MusicalNoiseGate;
use crate::clap::processor::dsp::orchestrator::worker::apply_gate_mode_change;
use crate::clap::processor::state::BypassCrossfader;
use clack_extensions::tail::HostTail;
use clack_plugin::host::HostAudioProcessorHandle;
use neural_amp_modeler_rs::common::params::RtProcessingParams;
use neural_amp_modeler_rs::common::spsc::{GcItem, GcSink, RtStatusFlags, RtSwapHandler};
use neural_amp_modeler_rs::dsp::adaptive::AdaptiveCompute;
use neural_amp_modeler_rs::dsp::oversample::{OversampleEngine, OversampleFactor};
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer;
use neural_amp_modeler_rs::dsp::smoother::ParamSmoother;
use neural_amp_modeler_rs::math::dsp::gain_lut::GainLUT;
use neural_amp_modeler_rs::models::StaticModel;
use std::sync::atomic::Ordering;

/// [`RtSwapHandler`] binding the engine's structural-swap scheduler to the
/// CLAP command payload family ([`ClapParamPayload`]).
pub(crate) struct ClapCommandSwapHandler<'p, 'a> {
    pub(in crate::clap::processor) model_l: &'p mut Option<Box<StaticModel>>,
    pub(in crate::clap::processor) model_r: &'p mut Option<Box<StaticModel>>,
    pub(in crate::clap::processor) model_generation: &'p mut u64,
    pub(in crate::clap::processor) resampler: &'p mut Box<NamResampler>,
    pub(in crate::clap::processor) stream: &'p mut Box<StreamingResampleBuffer>,
    pub(in crate::clap::processor) os_l: &'p mut Box<OversampleEngine>,
    pub(in crate::clap::processor) os_r: &'p mut Box<OversampleEngine>,
    pub(in crate::clap::processor) cab_slot: &'p mut Option<CabSlotDsp>,
    pub(in crate::clap::processor) cab_bypass: &'p mut bool,
    pub(in crate::clap::processor) cab_needs_reset: &'p mut bool,
    pub(in crate::clap::processor) cab_nam_dirty: &'p mut bool,
    pub(in crate::clap::processor) cab_nam_generation: &'p mut u64,
    pub(in crate::clap::processor) cab_nam_rebuild_in_flight: &'p mut bool,
    pub(in crate::clap::processor) cab_bypass_xfade: &'p mut BypassCrossfader,
    pub(in crate::clap::processor) cab_dry_delay: &'p mut DryDelayLine,
    pub(in crate::clap::processor) max_frames_count: usize,
    pub(in crate::clap::processor) params: &'p mut RtProcessingParams,
    pub(in crate::clap::processor) realtime_activation:
        &'p mut neural_amp_modeler_rs::common::params::ActivationPrecision,
    pub(in crate::clap::processor) applied_os_factor: &'p mut OversampleFactor,
    pub(in crate::clap::processor) cal_config: &'p mut CalibrationConfig,
    pub(in crate::clap::processor) model_cal_meta: &'p mut ModelCalibrationMeta,
    pub(in crate::clap::processor) model_input_mult_adj: &'p mut f32,
    pub(in crate::clap::processor) model_output_mult_adj: &'p mut f32,
    pub(in crate::clap::processor) adaptive_compute: &'p mut AdaptiveCompute,
    pub(in crate::clap::processor) cached_effective_latency: &'p mut u32,
    pub(in crate::clap::processor) dry_delay: &'p mut DryDelayLine,
    pub(in crate::clap::processor) smoother_in: &'p mut ParamSmoother,
    pub(in crate::clap::processor) smoother_out: &'p mut ParamSmoother,
    pub(in crate::clap::processor) smoother_cal_in: &'p mut ParamSmoother,
    pub(in crate::clap::processor) smoother_cal_out: &'p mut ParamSmoother,
    pub(in crate::clap::processor) mod_input_gain: &'p mut f32,
    pub(in crate::clap::processor) mod_output_gain: &'p mut f32,
    pub(in crate::clap::processor) gate_dirty: &'p mut bool,
    pub(in crate::clap::processor) gate_mode: &'p mut GateMode,
    pub(in crate::clap::processor) musical_gate: &'p mut MusicalNoiseGate,
    pub(in crate::clap::processor) silence_hyst:
        &'p mut neural_amp_modeler_rs::dsp::gate::DynamicHysteresis,
    pub(in crate::clap::processor) prewarm_pending: &'p mut bool,
    pub(in crate::clap::processor) prewarm_cost_ns_per_sample: &'p mut Option<f32>,
    pub(in crate::clap::processor) prewarm_zero_budget_count: &'p mut u32,
    pub(in crate::clap::processor) shared: &'a NamClapShared,
    pub(in crate::clap::processor) rt_status: &'p RtStatusFlags,
    pub(in crate::clap::processor) gain_lut: &'static GainLUT,
    pub(in crate::clap::processor) host: &'p mut HostAudioProcessorHandle<'a>,
    pub(in crate::clap::processor) host_tail: Option<HostTail>,
    pub(in crate::clap::processor) box_gc: RetiredBoxSink<'p>,
    /// Commands installed or discarded by this handler instance (one per
    /// `process` callback). Starts at `0`, so an idle callback proves
    /// "nothing drained" and skips the ack atomics entirely.
    pub(in crate::clap::processor) resolved: u64,
}

impl RtSwapHandler for ClapCommandSwapHandler<'_, '_> {
    type Payload = ClapParamPayload;

    #[inline(always)]
    fn is_structural(&self, payload: &ClapParamPayload) -> bool {
        payload.is_structural()
    }

    #[inline(always)]
    fn coalesce_key(&self, payload: &ClapParamPayload) -> Option<u64> {
        payload
            .structural_kind()
            .filter(|kind| kind.is_coalescible())
            .map(|kind| kind as u64)
    }

    #[cold]
    fn install(&mut self, mut payload: Box<Self::Payload>, gc: &mut GcSink<'_>) {
        self.resolved = self.resolved.wrapping_add(1);
        let taken = std::mem::replace(
            payload.as_mut(),
            ClapParamPayload::Params(RtProcessingParams::default()),
        );
        self.box_gc.retire(RetiredBox::Command(payload));
        self.apply_structural(taken, gc);
    }

    #[cold]
    fn discard(&mut self, mut payload: Box<Self::Payload>, gc: &mut GcSink<'_>) {
        self.resolved = self.resolved.wrapping_add(1);
        let taken = std::mem::replace(
            payload.as_mut(),
            ClapParamPayload::Params(RtProcessingParams::default()),
        );
        self.box_gc.retire(RetiredBox::Command(payload));
        self.discard_structural_payload(taken, gc);
    }

    fn after_drain(&mut self, _gc: &mut GcSink<'_>) {
        // Idle callback: zero atomic load and zero `fetch_max` RMW.
        if self.resolved == 0 {
            return;
        }
        let seq = self
            .shared
            .cold
            .cmd_last_ack
            .load(Ordering::Acquire)
            .wrapping_add(self.resolved);
        crate::clap::plugin::command_scheduler::ack_processed(&self.shared.cold.cmd_last_ack, seq);
    }
}

impl ClapCommandSwapHandler<'_, '_> {
    #[cold]
    fn apply_structural(&mut self, payload: ClapParamPayload, gc: &mut GcSink<'_>) {
        match payload {
            ClapParamPayload::Params(new_params) => {
                self.apply_params_from_spsc(new_params);
            }
            ClapParamPayload::Calibration(cfg) => {
                let sanitized = cfg.sanitized();
                if sanitized != *self.cal_config {
                    *self.cal_config = sanitized;
                    self.retarget_calibration(false);
                }
            }
            ClapParamPayload::GateMode(mode) => {
                apply_gate_mode_change(
                    mode,
                    self.gate_mode,
                    self.musical_gate,
                    self.silence_hyst,
                    self.gate_dirty,
                );
            }
            ClapParamPayload::CabBypass(bypass) => {
                if bypass != *self.cab_bypass {
                    *self.cab_bypass = bypass;
                    self.cab_bypass_xfade.trigger(bypass);
                }
            }
            ClapParamPayload::LoadModel {
                generation,
                model_l,
                model_r,
                new_resampler,
                new_stream,
                model_rate,
                input_mult_adj,
                output_mult_adj,
                cal_meta,
                prewarm_cost_ns,
            } => self.cold_load_model(
                LoadModelPayload {
                    generation,
                    model_l,
                    model_r,
                    new_resampler,
                    new_stream,
                    model_rate,
                    input_mult_adj,
                    output_mult_adj,
                    cal_meta,
                    prewarm_cost_ns,
                },
                gc,
            ),
            ClapParamPayload::LoadCabIr { pair } => {
                self.cold_load_cabsim(pair, gc);
            }
            ClapParamPayload::RebuildCabNam { generation, slot } => {
                self.cold_rebuild_cab_nam(generation, slot, gc);
            }
            ClapParamPayload::SetOversample { os_l, os_r } => {
                self.cold_load_os(os_l, os_r, gc);
            }
            ClapParamPayload::RestoreTxn(txn) => {
                self.cold_apply_restore_txn(txn, gc);
            }
        }
    }

    #[cold]
    fn retarget_calibration(&mut self, snap: bool) {
        let (in_mult, out_mult) =
            compute_calibration_multipliers(*self.model_cal_meta, *self.cal_config);
        *self.model_input_mult_adj = in_mult;
        *self.model_output_mult_adj = out_mult;
        self.smoother_cal_in.set_target(in_mult);
        self.smoother_cal_out.set_target(out_mult);
        if snap {
            self.smoother_cal_in.snap_to_target();
            self.smoother_cal_out.snap_to_target();
        }
    }

    #[cold]
    fn discard_structural_payload(&mut self, payload: ClapParamPayload, gc: &mut GcSink<'_>) {
        match payload {
            ClapParamPayload::LoadModel {
                model_l,
                model_r,
                new_resampler,
                new_stream,
                ..
            } => {
                if let Some(old_l) = model_l {
                    gc.retire(GcItem::Model(old_l));
                }
                if let Some(old_r) = model_r {
                    gc.retire(GcItem::Model(old_r));
                }
                gc.retire(GcItem::Resampler(new_resampler));
                gc.retire(GcItem::Streaming(new_stream));
            }
            ClapParamPayload::LoadCabIr { pair } => {
                if let Some(old) = pair {
                    old.retire_to_gc(|item| gc.retire(item));
                }
            }
            ClapParamPayload::RebuildCabNam { slot, .. } => {
                slot.retire_to_gc(|item| gc.retire(item));
            }
            ClapParamPayload::SetOversample { os_l, os_r } => {
                gc.retire(GcItem::Oversample(os_l));
                gc.retire(GcItem::Oversample(os_r));
            }
            other => {
                debug_assert!(
                    !other.is_structural()
                        || other.structural_kind() == Some(StructuralKind::Restore),
                    "discard_structural_payload called for a non-coalescible command"
                );
            }
        }
    }

    #[cold]
    fn cold_load_model(&mut self, payload: LoadModelPayload, gc: &mut GcSink<'_>) {
        let LoadModelPayload {
            generation,
            model_l,
            model_r,
            new_resampler,
            new_stream,
            model_rate: _,
            input_mult_adj,
            output_mult_adj,
            cal_meta,
            prewarm_cost_ns,
        } = payload;
        *self.model_generation = generation;
        if let Some(old_l) = std::mem::replace(self.model_l, model_l) {
            gc.retire(GcItem::Model(old_l));
        }
        if let Some(old_r) = std::mem::replace(self.model_r, model_r) {
            gc.retire(GcItem::Model(old_r));
        }
        *self.prewarm_pending = false;
        self.shared
            .rt_to_ui
            .prewarm_pending
            .store(false, Ordering::Relaxed);
        *self.prewarm_zero_budget_count = 0;
        *self.prewarm_cost_ns_per_sample = prewarm_cost_ns;
        if let Some(model) = self.model_l.as_mut() {
            model.inject_rt_status_rt(&self.shared.cold.rt_status);
        }
        if let Some(model) = self.model_r.as_mut() {
            model.inject_rt_status_rt(&self.shared.cold.rt_status);
        }

        let old_resampler = std::mem::replace(self.resampler, new_resampler);
        gc.retire(GcItem::Resampler(old_resampler));

        let old_stream = std::mem::replace(self.stream, new_stream);
        gc.retire(GcItem::Streaming(old_stream));

        self.shared
            .cold
            .current_stream_latency
            .store(self.stream.latency_samples(), Ordering::Relaxed);

        *self.model_cal_meta = cal_meta;
        let has_meta = cal_meta.input_level_dbu.is_some()
            || cal_meta.output_level_dbu.is_some()
            || cal_meta.loudness.is_some();
        let (effective_in_adj, effective_out_adj) = if has_meta {
            compute_calibration_multipliers(*self.model_cal_meta, *self.cal_config)
        } else {
            (input_mult_adj, output_mult_adj)
        };
        *self.model_input_mult_adj = effective_in_adj;
        *self.model_output_mult_adj = effective_out_adj;
        self.smoother_cal_in.set_target(effective_in_adj);
        self.smoother_cal_in.snap_to_target();
        self.smoother_cal_out.set_target(effective_out_adj);
        self.smoother_cal_out.snap_to_target();

        self.shared.cold.cancel_slimmable_request();

        crate::clap::processor::dsp::install::on_model_installed(
            self.model_l.as_deref(),
            self.adaptive_compute,
        );

        self.recompute_effective_latency();
    }

    #[cold]
    fn recompute_effective_latency(&mut self) {
        let mut effective_latency = self.stream.latency_samples();
        effective_latency += self.os_l.latency_samples() as u32;
        let cab_latency = self
            .cab_slot
            .as_ref()
            .map_or(0, CabSlotDsp::latency_samples);
        effective_latency += cab_latency;
        *self.cached_effective_latency = effective_latency;
        self.dry_delay.set_delay(effective_latency as usize);
        self.cab_dry_delay.set_delay(cab_latency as usize);
    }

    #[cold]
    fn cold_load_cabsim(&mut self, slot: Option<CabSlotDsp>, gc: &mut GcSink<'_>) {
        if let Some(ref candidate) = slot {
            let host_rate = self.shared.cold.sample_rate.load(Ordering::Relaxed);
            if host_rate != 0
                && !candidate.is_compatible_with_host(host_rate, self.max_frames_count)
            {
                self.rt_status.set_flag(
                    neural_amp_modeler_rs::common::spsc::RT_STATUS_HOST_CONTRACT_VIOLATION,
                );
                if let Some(stale) = slot {
                    stale.retire_to_gc(|item| gc.retire(item));
                }
                return;
            }
        }
        if let Some(old_slot) = std::mem::replace(self.cab_slot, slot) {
            old_slot.retire_to_gc(|item| gc.retire(item));
        }
        *self.cab_nam_generation = self.cab_nam_generation.wrapping_add(1);
        self.shared
            .cold
            .cab_nam_rebuild_req
            .store(*self.cab_nam_generation << 1, Ordering::Release);
        *self.cab_needs_reset = false;
        *self.cab_nam_dirty = false;
        *self.cab_nam_rebuild_in_flight = false;
        self.cab_dry_delay.reset();
        self.shared.cold.current_cabsim_latency.store(
            self.cab_slot
                .as_ref()
                .map_or(0, CabSlotDsp::latency_samples),
            Ordering::Relaxed,
        );
        let cabsim_tail = self.cab_slot.as_ref().map_or(0, CabSlotDsp::tail_samples);
        self.shared
            .rt_to_ui
            .cabsim_tail_samples
            .store(cabsim_tail, Ordering::Relaxed);

        self.recompute_effective_latency();

        let base = *self.cached_effective_latency;
        let current_tail = base.saturating_add(cabsim_tail);
        let last_reported = self
            .shared
            .cold
            .last_reported_tail_samples
            .load(Ordering::Relaxed);
        if current_tail != last_reported {
            self.shared
                .cold
                .last_reported_tail_samples
                .store(current_tail, Ordering::Relaxed);
            if let Some(tail_ext) = self.host_tail {
                tail_ext.changed(self.host);
            }
        }
    }

    #[cold]
    fn cold_rebuild_cab_nam(&mut self, generation: u64, slot: CabSlotDsp, gc: &mut GcSink<'_>) {
        if generation != *self.cab_nam_generation
            || !matches!(self.cab_slot, Some(CabSlotDsp::NamModel(_)))
        {
            self.shared
                .cold
                .cab_nam_stale_discarded_total
                .fetch_add(1, Ordering::Relaxed);
            slot.retire_to_gc(|item| gc.retire(item));
            return;
        }
        let host_rate = self.shared.cold.sample_rate.load(Ordering::Relaxed);
        if host_rate != 0 && !slot.is_compatible_with_host(host_rate, self.max_frames_count) {
            self.rt_status
                .set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HOST_CONTRACT_VIOLATION);
            slot.retire_to_gc(|item| gc.retire(item));
            *self.cab_nam_rebuild_in_flight = false;
            return;
        }
        if let Some(old_slot) = self.cab_slot.replace(slot) {
            old_slot.retire_to_gc(|item| gc.retire(item));
        }
        *self.cab_needs_reset = false;
        *self.cab_nam_dirty = false;
        *self.cab_nam_rebuild_in_flight = false;
        *self.cab_bypass_xfade = BypassCrossfader::new(true);
        if !*self.cab_bypass {
            self.cab_bypass_xfade.trigger(false);
        }
    }

    #[cold]
    fn cold_load_os(
        &mut self,
        os_l: Box<OversampleEngine>,
        os_r: Box<OversampleEngine>,
        gc: &mut GcSink<'_>,
    ) {
        *self.applied_os_factor = os_l.factor();
        let old_l = std::mem::replace(self.os_l, os_l);
        let old_r = std::mem::replace(self.os_r, os_r);
        gc.retire(GcItem::Oversample(old_l));
        gc.retire(GcItem::Oversample(old_r));

        self.recompute_effective_latency();
    }

    #[cold]
    fn cold_apply_restore_txn(&mut self, txn: RestoreTxn, gc: &mut GcSink<'_>) {
        *self.cal_config = txn.calibration.sanitized();
        let mode_changed = apply_gate_mode_change(
            txn.gate_mode,
            self.gate_mode,
            self.musical_gate,
            self.silence_hyst,
            self.gate_dirty,
        );
        if !mode_changed && txn.model.is_some() {
            self.musical_gate.reset();
        }
        if let Some(model) = txn.model {
            self.cold_load_model(model, gc);
        } else {
            self.retarget_calibration(true);
        }
        let preserve_cab_reset = txn.ir.is_none() && *self.cab_needs_reset && !txn.cab_bypass;
        if let Some(ir) = txn.ir {
            self.cold_load_cabsim(ir, gc);
        }
        *self.cab_bypass = txn.cab_bypass;
        *self.cab_needs_reset = preserve_cab_reset;
        *self.cab_bypass_xfade = BypassCrossfader::new(txn.cab_bypass);
        self.shared
            .ui_to_rt
            .param_cab_bypass
            .store(bypass_bool_to_u32(txn.cab_bypass), Ordering::Relaxed);
        self.apply_params_from_spsc(txn.params);
        self.shared
            .cold
            .last_applied_generation
            .store(txn.generation, Ordering::Relaxed);
    }
}
