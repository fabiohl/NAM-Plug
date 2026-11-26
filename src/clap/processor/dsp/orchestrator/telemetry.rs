// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

impl<'a> NamClapProcessor<'a> {
    #[inline(always)]
    pub(super) fn configure_channel_gate_and_model_flag(&mut self) {
        let is_stereo_config = self
            .shared
            .cold
            .selected_port_config
            .load(Ordering::Relaxed)
            == crate::clap::plugin::CONFIG_ID_STEREO;
        if self
            .shared
            .ui_to_rt
            .host_r_deactivated
            .load(Ordering::Acquire)
        {
            self.process_mono = true;
        }
        self.cached_gate_params.mono_epsilon = if !self.process_mono && is_stereo_config {
            -1.0
        } else {
            1e-4
        };
        self.refresh_gate_config();

        if (self.model_l.is_some() || self.params.bypass)
            && self
                .rt_status
                .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_MODEL_LOAD_FAILED)
        {
            self.rt_status
                .clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_MODEL_LOAD_FAILED);
        }
    }

    #[cold]
    #[inline(never)]
    pub(super) fn update_rms_alpha_cold(&mut self, n_samples: u32, sample_rate: u32) {
        self.cached_rms_samples = n_samples;
        self.cached_rms_sample_rate = sample_rate;
        self.cached_rms_ewma_alpha = compute_rms_ewma_alpha(n_samples, sample_rate);
    }

    #[inline(always)]
    pub(super) fn update_rms_and_telemetry(
        &mut self,
        metrics: &BlockMetricsAccumulator,
        sample_rate: u32,
    ) {
        let effective_sr = if sample_rate > 0 { sample_rate } else { 48000 };
        let n = metrics.total_samples;
        if n > 0 {
            let n_u32 = n.min(u32::MAX as usize) as u32;
            if self.cached_rms_samples != n_u32 || self.cached_rms_sample_rate != effective_sr {
                self.update_rms_alpha_cold(n_u32, effective_sr);
            }
            let n_f = n as f32;
            let block_in_rms_l = (metrics.in_sum_sq_l / n_f).sqrt();
            let block_in_rms_r = (metrics.in_sum_sq_r / n_f).sqrt();
            let block_out_rms_l = (metrics.out_sum_sq_l / n_f).sqrt();
            let block_out_rms_r = (metrics.out_sum_sq_r / n_f).sqrt();

            let alpha = self.cached_rms_ewma_alpha;
            self.in_rms_ewma_l += alpha * (block_in_rms_l - self.in_rms_ewma_l);
            self.in_rms_ewma_r += alpha * (block_in_rms_r - self.in_rms_ewma_r);
            self.out_rms_ewma_l += alpha * (block_out_rms_l - self.out_rms_ewma_l);
            self.out_rms_ewma_r += alpha * (block_out_rms_r - self.out_rms_ewma_r);
        }

        peaks::store_peaks(self.shared, metrics.out_peak_l, metrics.out_peak_r);
        peaks::store_input_peaks(self.shared, metrics.in_peak_l, metrics.in_peak_r);
        peaks::store_rms(
            self.shared,
            self.in_rms_ewma_l,
            self.in_rms_ewma_r,
            self.out_rms_ewma_l,
            self.out_rms_ewma_r,
        );
    }

    #[inline]
    pub(super) fn report_gate_telemetry(&self, any_active: bool, last_gate_state: GateState) {
        match self.gate_mode {
            GateMode::Silence => {
                let is_gate_active = any_active && last_gate_state == GateState::Closed;
                self.shared
                    .rt_to_ui
                    .ui_gate_active
                    .store(is_gate_active, Ordering::Relaxed);

                if any_active {
                    gate_flags::report_gate_flags(&self.rt_status, last_gate_state);
                } else {
                    gate_flags::report_gate_flags(&self.rt_status, GateState::Closed);
                }
            }
            GateMode::Musical => {
                let attenuating =
                    !self.params.bypass && self.musical_gate.is_attenuating(self.process_mono);
                self.shared
                    .rt_to_ui
                    .ui_gate_active
                    .store(attenuating, Ordering::Relaxed);
                gate_flags::report_gate_flags(
                    &self.rt_status,
                    if attenuating {
                        GateState::Closed
                    } else {
                        GateState::Open
                    },
                );
            }
        }
    }
}

#[inline]
fn compute_rms_ewma_alpha(n_samples: u32, sample_rate: u32) -> f32 {
    if n_samples == 0 || sample_rate == 0 {
        return 0.0;
    }
    let n_f = n_samples as f32;
    let sr = sample_rate as f32;
    (1.0 - (-n_f / (sr * 0.300)).exp()).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::compute_rms_ewma_alpha;

    #[test]
    fn rms_ewma_alpha_matches_exponential_decay_contract() {
        let a64 = compute_rms_ewma_alpha(64, 48_000);
        let expected_64 = 1.0 - (-64.0f32 / (48_000.0 * 0.300)).exp();
        assert!((a64 - expected_64).abs() <= f32::EPSILON);
        assert!(a64 > 0.0 && a64 < 1.0);

        assert_eq!(compute_rms_ewma_alpha(0, 48_000), 0.0);
        assert_eq!(compute_rms_ewma_alpha(64, 0), 0.0);
    }
}
