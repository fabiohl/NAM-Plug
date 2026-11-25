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
    /// Emits an instance-scoped alert once to `LogBuffer` and the DAW `HostLog`.
    ///
    /// When `NamLogger` is active and has an instance sink registered for this
    /// plugin, `log::*!` inside `scope_instance` forwards to `HostLog`
    /// automatically; calling `HostLog::log` directly as well would duplicate
    /// every message on the host console. Only fall back to direct `HostLog`
    /// dispatch when `NamLogger` forwarding is unavailable.
    fn emit_alert_log(&self, severity: LogSeverity, msg: &str) {
        let level = match severity {
            LogSeverity::Error => log::Level::Error,
            LogSeverity::Warning => log::Level::Warn,
            LogSeverity::Info => log::Level::Info,
            LogSeverity::Debug => log::Level::Debug,
            _ => log::Level::Info,
        };
        let forwarded_by_logger =
            neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::global().is_some()
                && log::log_enabled!(level)
                && self
                    .shared
                    .cold
                    .host_log_sink
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_some();
        match severity {
            LogSeverity::Error => log::error!("{msg}"),
            LogSeverity::Warning => log::warn!("{msg}"),
            LogSeverity::Info => log::info!("{msg}"),
            LogSeverity::Debug => log::debug!("{msg}"),
            _ => log::info!("{msg}"),
        }
        if !forwarded_by_logger && let Some(log) = self.host.get_extension::<HostLog>() {
            let cmsg = CString::new(msg.as_bytes()).unwrap_or_default();
            log.log(&self.host.shared(), severity, &cmsg);
        }
    }

    /// Emits host log messages for RT telemetry flags that were set.
    ///
    /// RT-Safe: all `CString::new(…).unwrap_or_default()` calls use static
    /// ASCII literals or pre-formatted non-null strings — guaranteed non-panicking.
    pub(crate) fn emit_pending_logs(&self) {
        let _scope = neural_amp_modeler_rs::common::diagnostics::scope_instance(
            self.shared.cold.instance_id,
        );
        self.update_alert_bits();

        // Standardized alert transition logs to host.
        // Emits only on 0 -> 1 transitions to prevent flooding.
        let current_alerts = self.shared.cold.ui_alert_bits.load(Ordering::Acquire);
        let last_alerts = self.last_seen_alert_bits.get();
        let new_alerts = current_alerts & !last_alerts;
        self.last_seen_alert_bits.set(current_alerts);

        if (new_alerts & UI_ALERT_POISONED) != 0 {
            self.emit_alert_log(
                LogSeverity::Error,
                "NAM-Plug: Audio processor is poisoned due to a fatal panic in RT callback! DSP halted — restart requested.",
            );
        }

        if (new_alerts & UI_ALERT_HOST_CONTRACT) != 0 {
            self.emit_alert_log(
                LogSeverity::Error,
                "NAM-Plug: Host audio buffer size exceeded negotiated maximum contract limit!",
            );
        }

        if (new_alerts & UI_ALERT_RESAMPLER_FAILED) != 0 {
            self.emit_alert_log(
                LogSeverity::Error,
                "NAM-Plug: Resampler rebuild failed for current host sample rate!",
            );
        }

        if (new_alerts & UI_ALERT_CABSIM_CONTRACT) != 0 {
            self.emit_alert_log(
                LogSeverity::Error,
                "NAM-Plug: Host buffer size violates CabSim partition contract limit!",
            );
        }

        if (new_alerts & UI_ALERT_DEGRADE_MINIMAL) != 0 {
            self.emit_alert_log(
                LogSeverity::Warning,
                "NAM-Plug: DSP degraded to minimal quality mode due to critical CPU pressure.",
            );
        }

        if (new_alerts & UI_ALERT_DEGRADE_REDUCED) != 0 {
            self.emit_alert_log(
                LogSeverity::Warning,
                "NAM-Plug: DSP degraded to reduced quality mode due to high CPU pressure.",
            );
        }

        if (new_alerts & UI_ALERT_NON_FINITE_INPUT) != 0 {
            self.emit_alert_log(
                LogSeverity::Warning,
                "NAM-Plug: Non-finite input samples (NaN/Inf) detected — recovering DSP state.",
            );
        }

        if self
            .shared
            .cold
            .rt_status
            .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HAS_CLIPPED)
        {
            self.emit_alert_log(
                LogSeverity::Warning,
                "NAM-Plug: Output clipping detected! Consider reducing input/output gain, \
                 or checking whether the model carries a large DC offset.",
            );
        }

        if self
            .shared
            .cold
            .rt_status
            .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_GC_OVERFLOW)
        {
            self.emit_alert_log(
                LogSeverity::Error,
                "NAM-Plug: GC channel overflow! Possible memory leak.",
            );
        }

        if self
            .shared
            .cold
            .rt_status
            .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_GC_TIER3)
        {
            self.emit_alert_log(
                LogSeverity::Warning,
                "NAM-Plug: GC cascade reached Tier 3 (overflow buffer). Sustained GC pressure.",
            );
        }

        if self
            .shared
            .cold
            .rt_status
            .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_GC_CORRUPTED)
        {
            self.emit_alert_log(
                LogSeverity::Error,
                "NAM-Plug: GC overflow buffer corrupted! Forced leak to avoid UB.",
            );
        }

        // Rising-edge detection only: the RT thread keeps this flag set while
        // the failure persists, so consuming it here (check_and_clear) would let
        // a failure flood the log at every host tick.
        let model_load_failed = self
            .shared
            .cold
            .rt_status
            .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_MODEL_LOAD_FAILED);
        let was_failed = self.last_seen_model_load_failed.replace(model_load_failed);
        if model_load_failed && !was_failed {
            self.emit_alert_log(
                LogSeverity::Error,
                "NAM-Plug: Critical failure! No active model for processing.",
            );
        }

        if self
            .shared
            .cold
            .rt_status
            .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HEAP_ALLOC)
        {
            self.emit_alert_log(
                LogSeverity::Error,
                "NAM-Plug: Heap allocation detected in audio thread during process()!",
            );
        }

        if self
            .shared
            .cold
            .rt_status
            .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HUGEPAGE_OK)
        {
            self.emit_alert_log(
                LogSeverity::Info,
                "NAM-Plug: HugeTLB explicit 2 MB pages active — reduced TLB pressure on DSP thread.",
            );
        }

        if self
            .shared
            .cold
            .rt_status
            .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_THP_ACTIVE)
        {
            self.emit_alert_log(
                LogSeverity::Info,
                "NAM-Plug: Transparent Huge Pages (THP) advice active — kernel may promote to 2 MB.",
            );
        }

        if self.shared.cold.rt_status.check_and_clear_flag(
            neural_amp_modeler_rs::common::spsc::RT_STATUS_SLIMMABLE_SLICE_FAILED,
        ) {
            self.emit_alert_log(
                LogSeverity::Error,
                "NAM-Plug: WaveNet slimmable slice_channels rebuild failed.",
            );
        }

        if self.shared.cold.rt_status.check_and_clear_flag(
            neural_amp_modeler_rs::common::spsc::RT_STATUS_SPSC_DRAIN_TRUNCATED,
        ) {
            self.emit_alert_log(
                LogSeverity::Warning,
                "Event queue saturation: SPSC drain limit (64) or input event budget (4096) exceeded - pending events deferred",
            );
        }

        if self.shared.cold.rt_status.check_and_clear_flag(
            neural_amp_modeler_rs::common::spsc::RT_STATUS_GUI_EVENT_BACKPRESSURE,
        ) {
            self.emit_alert_log(
                LogSeverity::Warning,
                "NAM-Plug: GUI event queue backpressure - host output full; gesture events retained for retry",
            );
        }

        if self.shared.cold.rt_status.check_and_clear_flag(
            neural_amp_modeler_rs::common::spsc::RT_STATUS_STRUCTURAL_DEFERRED,
        ) {
            self.emit_alert_log(
                LogSeverity::Info,
                "NAM-Plug: Structural command deferred to the next callback (command budget 1/callback) - FIFO order preserved",
            );
        }

        if self.shared.cold.rt_status.check_and_clear_flag(
            neural_amp_modeler_rs::common::spsc::RT_STATUS_STRUCTURAL_SUPERSEDED,
        ) {
            self.emit_alert_log(
                LogSeverity::Info,
                "NAM-Plug: Deferred structural command superseded by a newer same-kind command; obsolete resources discarded off-RT (coalescing)",
            );
        }

        if self.shared.cold.rt_status.check_and_clear_flag(
            neural_amp_modeler_rs::common::spsc::RT_STATUS_EVENT_TIMING_ANOMALY,
        ) {
            self.emit_alert_log(
                LogSeverity::Warning,
                "NAM-Plug: Parameter event timing anomaly detected (out-of-order, late, or beyond block limits); clamped defensively with zero event loss",
            );
        }

        // Rate-limited DSP overload/xrun warning — at most 1 warning / 5 s.
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

                let peak_permille = self
                    .shared
                    .rt_to_ui
                    .dsp_overload_peak_permille
                    .swap(0, Ordering::Relaxed);
                let ewma_permille = self
                    .shared
                    .rt_to_ui
                    .dsp_load_permille
                    .load(Ordering::Relaxed);
                let permille = peak_permille.max(ewma_permille);
                let load_pct = (permille as u32 + 5) / 10;

                let msg_str = format!(
                    "NAM-Plug: {delta} blocks exceeded 85% of the callback budget (load {load_pct}%)"
                );
                self.emit_alert_log(LogSeverity::Warning, &msg_str);
            }
        }

        let box_overwritten = self.shared.cold.box_overflow.overwritten_total();
        let last_box_overwritten = self.last_seen_box_overwritten.get();
        if box_overwritten > last_box_overwritten {
            self.last_seen_box_overwritten.set(box_overwritten);
            let delta = box_overwritten - last_box_overwritten;
            let msg_str = format!(
                "NAM-Plug: RetiredBoxOverflow overwrote {delta} undrained box shell(s) (total={box_overwritten})."
            );
            self.emit_alert_log(LogSeverity::Error, &msg_str);
        }
    }
}
