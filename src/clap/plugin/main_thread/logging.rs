// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! RT-Safe logging via atomic flags — consumes transient events on the main thread.

use super::NamClapMainThread;
use clack_extensions::log::{HostLog, LogSeverity};
use std::ffi::CString;

impl<'a> NamClapMainThread<'a> {
    /// Emits host log messages for RT telemetry flags that were set.
    ///
    /// RT-Safe: all `CString::new(…).unwrap_or_default()` calls use static
    /// ASCII literals with no internal null bytes — guaranteed non-panicking.
    pub(crate) fn emit_pending_logs(&mut self) {
        let log_ext = self.host.get_extension::<HostLog>();
        let shared = self.host.shared();

        if self
            .shared
            .cold
            .rt_status
            .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HAS_CLIPPED)
        {
            let msg = CString::new("NAM-rs: Output clipping detected!").unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Warning, &msg);
            }
            log::warn!("NAM-rs: Output clipping detected!");
        }

        if self
            .shared
            .cold
            .rt_status
            .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_GC_OVERFLOW)
        {
            let msg = CString::new("NAM-rs: GC channel overflow! Possible memory leak.")
                .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Error, &msg);
            }
            log::error!("NAM-rs: GC channel overflow! Possible memory leak.");
        }

        if self
            .shared
            .cold
            .rt_status
            .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_GC_TIER3)
        {
            let msg = CString::new(
                "NAM-rs: GC cascade reached Tier 3 (overflow buffer). Sustained GC pressure.",
            )
            .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Warning, &msg);
            }
            log::warn!(
                "NAM-rs: GC cascade reached Tier 3 (overflow buffer). Sustained GC pressure."
            );
        }

        if self
            .shared
            .cold
            .rt_status
            .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_GC_CORRUPTED)
        {
            let msg =
                CString::new("NAM-rs: GC overflow buffer corrupted! Forced leak to avoid UB.")
                    .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Error, &msg);
            }
            log::error!("NAM-rs: GC overflow buffer corrupted! Forced leak to avoid UB.");
        }

        if self
            .shared
            .cold
            .rt_status
            .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_MODEL_LOAD_FAILED)
        {
            let msg = CString::new("NAM-rs: Critical failure! No active model for processing.")
                .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Error, &msg);
            }
            log::error!("NAM-rs: Critical failure! No active model for processing.");
        }

        if self
            .shared
            .cold
            .rt_status
            .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HEAP_ALLOC)
        {
            let msg =
                CString::new("NAM-rs: Heap allocation detected in audio thread during process()!")
                    .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Error, &msg);
            }
            log::error!("NAM-rs: Heap allocation detected in audio thread during process()!");
        }

        if self
            .shared
            .cold
            .rt_status
            .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HUGEPAGE_OK)
        {
            let msg = CString::new(
                "NAM-rs: HugeTLB explicit 2 MB pages active — reduced TLB pressure on DSP thread.",
            )
            .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Info, &msg);
            }
            log::info!(
                "NAM-rs: HugeTLB explicit 2 MB pages active — reduced TLB pressure on DSP thread."
            );
        }

        if self
            .shared
            .cold
            .rt_status
            .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_THP_ACTIVE)
        {
            let msg = CString::new(
                "NAM-rs: Transparent Huge Pages (THP) advice active — kernel may promote to 2 MB.",
            )
            .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Info, &msg);
            }
            log::info!(
                "NAM-rs: Transparent Huge Pages (THP) advice active — kernel may promote to 2 MB."
            );
        }

        if self.shared.cold.rt_status.check_and_clear_flag(
            neural_amp_modeler_rs::common::spsc::RT_STATUS_SLIMMABLE_SLICE_FAILED,
        ) {
            let msg = CString::new("NAM-rs: WaveNet slimmable slice_channels rebuild failed.")
                .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Error, &msg);
            }
            log::error!("NAM-rs: WaveNet slimmable slice_channels rebuild failed.");
        }

        if self.shared.cold.rt_status.check_and_clear_flag(
            neural_amp_modeler_rs::common::spsc::RT_STATUS_SPSC_DRAIN_TRUNCATED,
        ) {
            let msg = CString::new(
                "SPSC command queue drain limit reached (64 events) - pending events deferred",
            )
            .unwrap_or_default();
            if let Some(log) = log_ext {
                log.log(&shared, LogSeverity::Warning, &msg);
            }
            log::warn!(
                "SPSC command queue drain limit reached (64 events) - pending events deferred"
            );
        }
    }
}
