// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::super::MAX_PLAUSIBLE_ELAPSED_NS;
use super::super::NamClapProcessor;
use std::sync::atomic::Ordering;

#[cfg(target_arch = "x86_64")]
use neural_amp_modeler_rs::common::tsc::rdtsc_nanos;

impl<'a> NamClapProcessor<'a> {
    #[cold]
    #[inline(never)]
    pub(super) fn update_telemetry_cadence_cold(&mut self, n_samples: u32, sample_rate: u32) {
        self.cached_n_samples = n_samples;
        self.cached_sample_rate = sample_rate;
        if sample_rate > 0 && n_samples > 0 {
            let budget_ns = (n_samples as u64 * 1_000_000_000) / sample_rate as u64;
            self.cached_threshold_ns = (budget_ns * 85) / 100;
            self.cached_budget_us = budget_ns / 1000;
            // Pre-calculate inverse budget in permille per nanosecond: 1000.0 / budget_ns
            self.cached_inv_budget_permille = 1000.0 / budget_ns as f32;
            // Discrete EWMA alpha for continuous tau ≈ 250ms (0.25s)
            let dt = n_samples as f32 / sample_rate as f32;
            const TAU_S: f32 = 0.250;
            self.cached_ewma_alpha = 1.0 - (-dt / TAU_S).exp();
        } else {
            self.cached_threshold_ns = u64::MAX;
            self.cached_budget_us = 0;
            self.cached_inv_budget_permille = 0.0;
            self.cached_ewma_alpha = 0.0;
        }
    }

    #[inline(always)]
    pub(super) fn process_telemetry(&mut self, start_nanos: u64) {
        if start_nanos > 0 {
            #[cfg(target_arch = "x86_64")]
            let raw_elapsed = rdtsc_nanos().wrapping_sub(start_nanos);
            #[cfg(not(target_arch = "x86_64"))]
            let raw_elapsed = 0;

            // Test-only synthetic stall injection (T-P1.3.2, one-shot):
            // clamps the measurement up to at least the injected value so a
            // test can force exactly one over-budget block through the real
            // `process()` path without sleeping on the audio thread. `#[cfg]`
            // out of every non-test build; zero-cost in production.
            #[cfg(any(test, feature = "testing"))]
            let elapsed_nanos = {
                let target = crate::clap::processor::TEST_TELEMETRY_STALL_TARGET_INSTANCE
                    .load(Ordering::Relaxed);
                let stall = if target != 0 && target == self.shared.cold.instance_id {
                    crate::clap::processor::TEST_TELEMETRY_STALL_NS.swap(0, Ordering::Relaxed)
                } else {
                    0
                };
                raw_elapsed.max(stall)
            };
            #[cfg(not(any(test, feature = "testing")))]
            let elapsed_nanos = raw_elapsed;

            // Defense-in-depth (T-R1.2): discard anomalous elapsed times (> 1s)
            // to avoid corrupting latency histograms, overload counters, or adaptive compute.
            if elapsed_nanos > MAX_PLAUSIBLE_ELAPSED_NS {
                return;
            }

            self.rt_status
                .dsp_cycle_time
                .store(elapsed_nanos, Ordering::Relaxed);
            self.rt_status.latency_hist.record(elapsed_nanos);

            let sample_rate = self.shared.cold.sample_rate.load(Ordering::Relaxed);
            let last_n_samples = self.rt_status.last_n_samples.load(Ordering::Relaxed);
            if sample_rate > 0 && last_n_samples > 0 {
                if self.cached_n_samples != last_n_samples || self.cached_sample_rate != sample_rate
                {
                    self.update_telemetry_cadence_cold(last_n_samples, sample_rate);
                }

                // EWMA tau ≈ 250ms of elapsed/budget (T-R2.1) without hot-path division.
                let instant_permille =
                    (elapsed_nanos as f32 * self.cached_inv_budget_permille).min(1000.0);
                self.dsp_load_ewma +=
                    self.cached_ewma_alpha * (instant_permille - self.dsp_load_ewma);
                let permille = (self.dsp_load_ewma.round() as u32).min(1000) as u16;
                self.shared
                    .rt_to_ui
                    .dsp_load_permille
                    .store(permille, Ordering::Relaxed);

                if elapsed_nanos > self.cached_threshold_ns {
                    let overloads =
                        self.rt_status.dsp_overloads.fetch_add(1, Ordering::Relaxed) + 1;
                    self.shared
                        .rt_to_ui
                        .xrun_epoch
                        .store(overloads, Ordering::Relaxed);
                }

                let latency_us = elapsed_nanos / 1000;
                let budget_us = self.cached_budget_us;
                self.adaptive_compute
                    .update(latency_us, budget_us, sample_rate, &self.rt_status);
            }
        }
    }
}
