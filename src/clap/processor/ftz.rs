// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! RAII guard asserting DAZ/FTZ denormal flushing for real-time audio scopes.

/// RAII guard that asserts DAZ/FTZ denormal flushing for the scope and
/// restores the host's MXCSR on drop.
///
/// On construction reads the calling thread's MXCSR (`stmxcsr`), sets the
/// DAZ (bit 6, `0x0040`) and FTZ (bit 15, `0x8000`) control bits and writes
/// the result back (`ldmxcsr`); on drop restores the saved value verbatim,
/// so no FTZ/DAZ state ever leaks to the host. Stack-only (`u32`), no
/// allocation, no log, no lock — safe on the audio thread. `Drop` never
/// panics (`ldmxcsr` only), hence `Drop`-safe on the `catch_unwind` panic
/// path as well.
///
/// The guard owns the steady-state FTZ/DAZ invariant per `process()` block,
/// replacing the former periodic reassertion every 1024 blocks
/// (`cycles_since_telemetry & 0x3FF`). The one-shot
/// [`NamClapProcessor::apply_rt_thread_setup`](crate::clap::processor::NamClapProcessor::apply_rt_thread_setup)
/// still applies DAZ/FTZ in `start_processing()` (thread-sticky setup outside any block);
/// the guard reasserts it on every block and guarantees restoration on return.
///
/// On non-`x86_64` targets this is a zero-sized no-op: DAZ/FTZ is an MXCSR
/// (SSE2) mechanism, while AArch64 flushes denormals via FPCR or hardware.
#[must_use]
pub(crate) struct FtzDazGuard {
    #[cfg(target_arch = "x86_64")]
    saved: u32,
}

impl FtzDazGuard {
    /// Asserts DAZ/FTZ on the calling thread, saving the previous MXCSR.
    #[inline(always)]
    pub(crate) fn new() -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            // SAFETY: `stmxcsr`/`ldmxcsr` on an aligned stack-local `u32` is
            // unconditionally safe on x86-64 (SSE2 is part of the x86-64-v3
            // baseline); `0x8040` (DAZ | FTZ) are valid MXCSR control flags.
            unsafe {
                let mut mxcsr: u32 = 0;
                core::arch::asm!("stmxcsr [{0}]", in(reg) &mut mxcsr);
                let saved = mxcsr;
                mxcsr |= 0x8040;
                core::arch::asm!("ldmxcsr [{0}]", in(reg) &mxcsr);
                Self { saved }
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            Self {}
        }
    }
}

impl Drop for FtzDazGuard {
    /// Restores the MXCSR saved at construction. Never panics.
    #[inline(always)]
    fn drop(&mut self) {
        #[cfg(target_arch = "x86_64")]
        {
            // SAFETY: same rationale as `new` — restoring a previously read
            // MXCSR value via `ldmxcsr` is always safe on x86-64.
            unsafe {
                core::arch::asm!("ldmxcsr [{0}]", in(reg) &self.saved);
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            let _ = self;
        }
    }
}
