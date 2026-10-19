// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! RT-Safe logging via atomic flags — consumes transient events on the main thread.

use super::NamClapMainThread;
use crate::clap::plugin::shared::{
    UI_ALERT_CABSIM_CONTRACT, UI_ALERT_DEGRADE_MINIMAL, UI_ALERT_DEGRADE_REDUCED,
    UI_ALERT_HOST_CONTRACT, UI_ALERT_NON_FINITE_INPUT, UI_ALERT_POISONED,
    UI_ALERT_RESAMPLER_FAILED,
};
use clack_extensions::log::{HostLog, LogSeverity};
use std::ffi::CString;
use std::sync::atomic::Ordering;

impl<'a> NamClapMainThread<'a> {
    /// Emits host log messages for RT telemetry flags that were set.
    ///
    /// RT-Safe: all `CString::new(…).unwrap_or_default()` calls use static
    /// ASCII literals with no internal null bytes — guaranteed non-panicking.
    pub(crate) fn emit_pending_logs(&self) {
        self.update_alert_bits();

        let log_ext = self.host.get_extension::<HostLog>();
        let shared = self.host.shared();

        // Standardized alert transition logs to host (T-R3.3).
        // Emits only on 0 -> 1 transitions to prevent flooding.
        let current_alerts = self.shared.cold.ui_alert_bits.load(Ordering::Acquire);
        let last_alerts = self.last_seen_alert_bits.get();
        let new_alerts = current_alerts & !last_alerts;
        self.last_seen_alert_bits.set(current_alerts);

        if (new_alerts & UI_ALERT_POISONED) != 0 {
            let msg = CString::new(
                "NAM-Plug: Audio processor is poisoned due to a fatal panic in RT callback! DSP halted — restart requested.",
            )
            .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Error, &msg);
            }
            log::error!(
                "NAM-Plug: Audio processor is poisoned due to a fatal panic in RT callback! DSP halted — restart requested."
            );
        }

        if (new_alerts & UI_ALERT_HOST_CONTRACT) != 0 {
            let msg = CString::new(
                "NAM-Plug: Host audio buffer size exceeded negotiated maximum contract limit!",
            )
            .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Error, &msg);
            }
            log::error!(
                "NAM-Plug: Host audio buffer size exceeded negotiated maximum contract limit!"
            );
        }

        if (new_alerts & UI_ALERT_RESAMPLER_FAILED) != 0 {
            let msg =
                CString::new("NAM-Plug: Resampler rebuild failed for current host sample rate!")
                    .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Error, &msg);
            }
            log::error!("NAM-Plug: Resampler rebuild failed for current host sample rate!");
        }

        if (new_alerts & UI_ALERT_CABSIM_CONTRACT) != 0 {
            let msg = CString::new(
                "NAM-Plug: Host buffer size violates CabSim partition contract limit!",
            )
            .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Error, &msg);
            }
            log::error!("NAM-Plug: Host buffer size violates CabSim partition contract limit!");
        }

        if (new_alerts & UI_ALERT_DEGRADE_MINIMAL) != 0 {
            let msg = CString::new(
                "NAM-Plug: DSP degraded to minimal quality mode due to critical CPU pressure.",
            )
            .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Warning, &msg);
            }
            log::warn!(
                "NAM-Plug: DSP degraded to minimal quality mode due to critical CPU pressure."
            );
        }

        if (new_alerts & UI_ALERT_DEGRADE_REDUCED) != 0 {
            let msg = CString::new(
                "NAM-Plug: DSP degraded to reduced quality mode due to high CPU pressure.",
            )
            .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Warning, &msg);
            }
            log::warn!("NAM-Plug: DSP degraded to reduced quality mode due to high CPU pressure.");
        }

        if (new_alerts & UI_ALERT_NON_FINITE_INPUT) != 0 {
            let msg = CString::new(
                "NAM-Plug: Non-finite input samples (NaN/Inf) detected — recovering DSP state.",
            )
            .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Warning, &msg);
            }
            log::warn!(
                "NAM-Plug: Non-finite input samples (NaN/Inf) detected — recovering DSP state."
            );
        }

        if self
            .shared
            .cold
            .rt_status
            .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HAS_CLIPPED)
        {
            let msg = CString::new(
                "NAM-Plug: Output clipping detected! Consider reducing input/output gain, \
                 or checking whether the model carries a large DC offset.",
            )
            .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Warning, &msg);
            }
            log::warn!(
                "NAM-Plug: Output clipping detected! Consider reducing input/output gain, \
                 or checking whether the model carries a large DC offset."
            );
        }

        if self
            .shared
            .cold
            .rt_status
            .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_GC_OVERFLOW)
        {
            let msg = CString::new("NAM-Plug: GC channel overflow! Possible memory leak.")
                .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Error, &msg);
            }
            log::error!("NAM-Plug: GC channel overflow! Possible memory leak.");
        }

        if self
            .shared
            .cold
            .rt_status
            .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_GC_TIER3)
        {
            let msg = CString::new(
                "NAM-Plug: GC cascade reached Tier 3 (overflow buffer). Sustained GC pressure.",
            )
            .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Warning, &msg);
            }
            log::warn!(
                "NAM-Plug: GC cascade reached Tier 3 (overflow buffer). Sustained GC pressure."
            );
        }

        if self
            .shared
            .cold
            .rt_status
            .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_GC_CORRUPTED)
        {
            let msg =
                CString::new("NAM-Plug: GC overflow buffer corrupted! Forced leak to avoid UB.")
                    .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Error, &msg);
            }
            log::error!("NAM-Plug: GC overflow buffer corrupted! Forced leak to avoid UB.");
        }

        if self
            .shared
            .cold
            .rt_status
            .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_MODEL_LOAD_FAILED)
        {
            let msg = CString::new("NAM-Plug: Critical failure! No active model for processing.")
                .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Error, &msg);
            }
            log::error!("NAM-Plug: Critical failure! No active model for processing.");
        }

        if self
            .shared
            .cold
            .rt_status
            .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HEAP_ALLOC)
        {
            let msg = CString::new(
                "NAM-Plug: Heap allocation detected in audio thread during process()!",
            )
            .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Error, &msg);
            }
            log::error!("NAM-Plug: Heap allocation detected in audio thread during process()!");
        }

        if self
            .shared
            .cold
            .rt_status
            .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HUGEPAGE_OK)
        {
            let msg = CString::new(
                "NAM-Plug: HugeTLB explicit 2 MB pages active — reduced TLB pressure on DSP thread.",
            )
            .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Info, &msg);
            }
            log::info!(
                "NAM-Plug: HugeTLB explicit 2 MB pages active — reduced TLB pressure on DSP thread."
            );
        }

        if self
            .shared
            .cold
            .rt_status
            .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_THP_ACTIVE)
        {
            let msg = CString::new(
                "NAM-Plug: Transparent Huge Pages (THP) advice active — kernel may promote to 2 MB.",
            )
            .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Info, &msg);
            }
            log::info!(
                "NAM-Plug: Transparent Huge Pages (THP) advice active — kernel may promote to 2 MB."
            );
        }

        if self.shared.cold.rt_status.check_and_clear_flag(
            neural_amp_modeler_rs::common::spsc::RT_STATUS_SLIMMABLE_SLICE_FAILED,
        ) {
            let msg = CString::new("NAM-Plug: WaveNet slimmable slice_channels rebuild failed.")
                .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Error, &msg);
            }
            log::error!("NAM-Plug: WaveNet slimmable slice_channels rebuild failed.");
        }

        if self.shared.cold.rt_status.check_and_clear_flag(
            neural_amp_modeler_rs::common::spsc::RT_STATUS_SPSC_DRAIN_TRUNCATED,
        ) {
            let msg = CString::new(
                "Event queue saturation: SPSC drain limit (64) or input event budget (4096) exceeded - pending events deferred",
            )
            .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Warning, &msg);
            }
            log::warn!(
                "Event queue saturation: SPSC drain limit (64) or input event budget (4096) exceeded - pending events deferred"
            );
        }

        if self.shared.cold.rt_status.check_and_clear_flag(
            neural_amp_modeler_rs::common::spsc::RT_STATUS_GUI_EVENT_BACKPRESSURE,
        ) {
            let msg = CString::new(
                "NAM-Plug: GUI event queue backpressure - host output full; gesture events retained for retry",
            )
            .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Warning, &msg);
            }
            log::warn!(
                "NAM-Plug: GUI event queue backpressure - host output full; gesture events retained for retry"
            );
        }

        if self.shared.cold.rt_status.check_and_clear_flag(
            neural_amp_modeler_rs::common::spsc::RT_STATUS_STRUCTURAL_DEFERRED,
        ) {
            let msg = CString::new(
                "NAM-Plug: Structural command deferred to the next callback (command budget 1/callback) - FIFO order preserved",
            )
            .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Info, &msg);
            }
            log::info!(
                "NAM-Plug: Structural command deferred to the next callback (command budget 1/callback) - FIFO order preserved"
            );
        }

        if self.shared.cold.rt_status.check_and_clear_flag(
            neural_amp_modeler_rs::common::spsc::RT_STATUS_STRUCTURAL_SUPERSEDED,
        ) {
            let msg = CString::new(
                "NAM-Plug: Deferred structural command superseded by a newer same-kind command; obsolete resources discarded off-RT (coalescing)",
            )
            .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Info, &msg);
            }
            log::info!(
                "NAM-Plug: Deferred structural command superseded by a newer same-kind command; obsolete resources discarded off-RT (coalescing)"
            );
        }

        if self.shared.cold.rt_status.check_and_clear_flag(
            neural_amp_modeler_rs::common::spsc::RT_STATUS_EVENT_TIMING_ANOMALY,
        ) {
            let msg = CString::new(
                "NAM-Plug: Parameter event timing anomaly detected (out-of-order, late, or beyond block limits); clamped defensively with zero event loss",
            )
            .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Warning, &msg);
            }
            log::warn!(
                "NAM-Plug: Parameter event timing anomaly detected (out-of-order, late, or beyond block limits); clamped defensively with zero event loss"
            );
        }

        // Rate-limited DSP overload/xrun warning (T-R2.3) — at most 1 warning / 5 s.
        let overloads = self
            .shared
            .cold
            .rt_status
            .dsp_overloads
            .load(Ordering::Relaxed);
        let last_overloads = self.last_seen_overloads.get();
        if overloads > last_overloads {
            let now = std::time::Instant::now();
            let should_log = match self.last_overload_log_instant.get() {
                None => true,
                Some(last) => now.duration_since(last) >= std::time::Duration::from_secs(5),
            };
            if should_log {
                let delta = overloads - last_overloads;
                self.last_seen_overloads.set(overloads);
                self.last_overload_log_instant.set(Some(now));

                let permille = self
                    .shared
                    .rt_to_ui
                    .dsp_load_permille
                    .load(Ordering::Relaxed);
                let load_pct = (permille as u32 + 5) / 10;

                let msg_str = format!(
                    "NAM-Plug: {delta} blocks exceeded 85% of the callback budget (load {load_pct}%)"
                );
                let msg = CString::new(msg_str.as_bytes()).unwrap_or_default();
                if let Some(log) = log_ext {
                    log.log(&shared, LogSeverity::Warning, &msg);
                }
                log::warn!(
                    "NAM-Plug: {delta} blocks exceeded 85% of the callback budget (load {load_pct}%)"
                );
            }
        }
    }
}
