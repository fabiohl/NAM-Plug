// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Implementation of the `clap_plugin_tail` extension for NAM-Plug.
//!
//! Reports the plugin's tail length (CabSim IR + oversampling/resampling latencies)
//! so DAWs can properly handle offline bounces and silence-timeout processing.

use crate::clap::processor::NamClapProcessor;
use clack_extensions::tail::{PluginTail, PluginTailImpl, TailLength};
use std::sync::atomic::Ordering;

/// Computes the tail length reported to the host as the saturating sum of the
/// fixed pipeline latency and the CabSim ring-out duration.
///
/// Both inputs are `u32` sample counts read from shared atomics; saturating
/// arithmetic guarantees the report never wraps around or panics in debug
/// builds, even for pathological combinations.
#[inline]
fn compute_tail_length(base: u32, cabsim_tail: u32) -> TailLength {
    TailLength::Finite(base.saturating_add(cabsim_tail))
}

impl PluginTailImpl for NamClapProcessor<'_> {
    /// Returns the current tail length in samples.
    ///
    /// The value is the sum of `current_latency` (fixed processing latency:
    /// resampler + oversampling) and `cabsim_tail_samples` (IR ring-out
    /// duration = num_partitions × partition_size, zero when no IR is loaded).
    /// Per the CLAP specification, the tail extension reports the *additional*
    /// time the host must process after input silence to capture the full
    /// ring-out — including both the fixed pipeline latency and the CabSim tail.
    fn get(&self) -> TailLength {
        let base = self.shared.rt_to_ui.current_latency.load(Ordering::Relaxed);
        let cabsim_tail = self
            .shared
            .rt_to_ui
            .cabsim_tail_samples
            .load(Ordering::Relaxed);
        compute_tail_length(base, cabsim_tail)
    }
}

/// Marker type for extension registration.
pub type NamPluginTail = PluginTail;

#[cfg(test)]
mod tests {
    use super::compute_tail_length;
    use clack_extensions::tail::TailLength;

    #[test]
    fn tail_length_saturates_at_u32_max() {
        assert_eq!(
            compute_tail_length(u32::MAX - 10, 100),
            TailLength::Finite(u32::MAX)
        );
    }

    #[test]
    fn tail_length_normal_case_is_the_plain_sum() {
        assert_eq!(
            compute_tail_length(268, 4096),
            TailLength::Finite(268 + 4096)
        );
    }
}
