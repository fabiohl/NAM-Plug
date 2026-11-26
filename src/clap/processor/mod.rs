// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! # CLAP Real-Time Audio Processor Subsystem
//!
//! Handles real-time audio sample processing inside DAW audio threads in compliance with CLAP specifications.
//!
//! ## Key Architectural Invariants
//! - **RT-Safety**: Zero heap allocations, zero dynamic drops, zero blocking I/O, zero mutexes on the audio thread.
//! - **Submodules Breakdown**:
//!   - **`activate`**: Off-RT buffer pre-allocation, resource extraction, DSP pipeline assembly, and initial latency publishing.
//!   - **`deactivated`**: `DeactivatedDspState` struct and resource preservation across deactivate/activate cycles.
//!   - **`dsp`**: Real-time DSP engine (channel extraction, dry delay line, input stage, neural inference, oversampling resampler, peak metering).
//!   - **`events`**: SPSC event drainage (Main Thread → Audio Thread) and CLAP input event parameter parsing.
//!   - **`ftz`**: RAII guard (`FtzDazGuard`) asserting DAZ/FTZ denormal flushing during audio processing with zero leakage.
//!   - **`gc`**: Garbage collection queue for offloading obsolete engine models/IRs off the audio thread.
//!   - **`heap_audit`**: Allocation auditing interceptors and tracking guards (active under `heap-audit` feature).
//!   - **`params`**: Parameter smoothing (`ParamSmoother`) and atomic parameter synchronization.
//!   - **`prewarm_policy`**: Proportional stabilization budget policy and execution step calculation for two-phase model resets.
//!   - **`rollback`**: Panic-resilient resource recovery guards (`ActivateRollbackGuard`).
//!   - **`state`**: `NamClapProcessor` struct declaration and bypass crossfader state machine.

mod activate;
mod deactivated;

/// Resolves the activation precision actually in effect for a render mode:
/// offline rendering always uses `Standard`, while live mode preserves the
/// user-selected `realtime` precision.
#[inline]
pub(crate) fn effective_activation(
    render_mode: u32,
    realtime: neural_amp_modeler_rs::common::params::ActivationPrecision,
) -> neural_amp_modeler_rs::common::params::ActivationPrecision {
    if render_mode == crate::clap::plugin::RENDER_MODE_OFFLINE {
        neural_amp_modeler_rs::common::params::ActivationPrecision::Standard
    } else {
        realtime
    }
}
mod dsp;
#[cfg(any(test, feature = "testing"))]
pub(crate) use dsp::orchestrator::audio_loop;
mod events;
mod ftz;
mod gc;
#[cfg(feature = "heap-audit")]
pub(crate) mod heap_audit;
mod params;
mod prewarm;
mod reset;
pub use prewarm::MAX_PLAUSIBLE_ELAPSED_NS;
#[cfg(test)]
pub(crate) use prewarm::PREWARM_STEP_SAMPLES_PER_CALLBACK;
#[cfg(any(test, feature = "testing"))]
pub(crate) use prewarm::TEST_PREWARM_ELAPSED_NS;

use crate::clap::plugin::errors;
use crate::clap::plugin::{NamClapMainThread, NamClapShared};
use clack_plugin::prelude::*;
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
use neural_amp_modeler_rs::common::tsc::rdtsc_nanos;
use neural_amp_modeler_rs::dsp::gate::GateParams;
use neural_amp_modeler_rs::models::NamModel;
use std::sync::atomic::Ordering;

pub(crate) mod prewarm_policy;
mod rollback;
mod state;

pub(crate) use deactivated::DeactivatedDspState;
pub(crate) use ftz::FtzDazGuard;
pub(crate) use prewarm_policy::{ModelFamily, compute_prewarm_step};
pub use state::BYPASS_XFADE_SAMPLES;
pub use state::BypassCrossfader;
pub(crate) use state::NamClapProcessor;
pub(crate) use state::{CMD_SWAP_TUNABLES, SLIMMABLE_SWAP_TUNABLES};

/// Test-only drain-latency probe: latest duration (ns) of the canonical
/// per-callback drain window (`SwapBudget` construction + both
/// `RtSwapDrain::drain` calls), recorded by `events::process_events` on x86_64
/// test builds via calibrated TSC reads. Consumed by the drain-latency
/// certification gate; absent from every non-test build.
#[cfg(all(test, target_arch = "x86_64", feature = "heap-audit"))]
pub(crate) static DRAIN_PROBE_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Test-only hook to inject a synthetic stalled block into telemetry:
/// when non-zero, `process_telemetry` clamps the measured `elapsed_nanos` up
/// to at least this value (`elapsed.max(stall)`) and then resets the hook to
/// zero (one-shot). Lets tests inject exactly one over-budget block
/// deterministically through the real `process()` path without sleeping on the
/// audio thread. `#[cfg]`-gated: absent from every non-test build, zero cost in
/// production.
#[cfg(any(test, feature = "testing"))]
pub(crate) static TEST_TELEMETRY_STALL_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Test-only hook to bind `TEST_TELEMETRY_STALL_NS` to a specific instance ID.
/// When non-zero, only the matching plugin instance will consume the stall.
#[cfg(any(test, feature = "testing"))]
pub(crate) static TEST_TELEMETRY_STALL_TARGET_INSTANCE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Serializes test modules that share the process-wide telemetry stall hooks:
/// a stall armed for one instance must never be re-targeted or consumed by a
/// concurrently running test.
#[cfg(test)]
pub(crate) static TEST_TELEMETRY_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Converts a panic payload into `PluginError` for `catch_unwind` guards.
///
/// In accordance with the zero memory leak contract in plugin binaries, the panic payload
/// is dropped without leaking heap memory. Detailed panic information and backtrace
/// were already safely written by the panic hook (`common/diagnostics/panic_hook.rs`)
/// to `~/.cache/neural-amp-modeler-rs/crash-*.txt`.
#[cold]
pub(crate) fn panic_to_error(panic_info: Box<dyn std::any::Any + Send>) -> PluginError {
    drop(panic_info);
    PluginError::Message(errors::processor::AUDIO_CALLBACK_PANICKED)
}

/// Helper to zero all output channels in the provided CLAP audio ports.
///
/// Used for deterministic O(1) silence on processor failure / poisoning without allocations.
/// Directly targets output ports to eliminate buffer-pairing sample type mismatches
/// and handles both f32 and f64 host audio ports defensively in depth.
#[inline(always)]
fn silence_audio_ports(audio: &mut Audio) {
    for mut port_pair in audio {
        if let Some(mut output) = port_pair.output()
            && let Ok(channels) = output.channels()
        {
            match channels {
                clack_plugin::process::audio::SampleType::F32(channels) => {
                    for ch in channels {
                        ch.fill(0.0f32);
                    }
                }
                clack_plugin::process::audio::SampleType::F64(channels) => {
                    for ch in channels {
                        ch.fill(0.0f64);
                    }
                }
                clack_plugin::process::audio::SampleType::Both(c32, c64) => {
                    for ch in c32 {
                        ch.fill(0.0f32);
                    }
                    for ch in c64 {
                        ch.fill(0.0f64);
                    }
                }
            }
        }
    }
}

/// Note: the entire `PluginAudioProcessor` impl must live in a single block
/// (Rust E0119 — trait impls cannot be split across modules).
impl<'a> NamClapProcessor<'a> {
    /// Latches the poisoned containment state after a panic caught on the audio
    /// thread: later callbacks take the O(1) silence path, the status flag lets the
    /// main-thread watchdog request a restart, and the host is called back so the
    /// restart is serviced promptly. Every containment site routes through here so
    /// no panic path can silently continue processing.
    pub(crate) fn latch_poisoned(&mut self) {
        self.poisoned = true;
        self.rt_status
            .set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_PROCESSOR_POISONED);
        self.host.request_callback();
    }

    /// One-shot RT-thread setup for the *current* audio thread.
    ///
    /// Runs from [`PluginAudioProcessor::start_processing`] (the CLAP
    /// audio-thread callback) with a cheap `rt_thread_setup_done` re-check
    /// kept as a defensive fallback at the top of `process()`. It:
    /// 1. publishes the thread's confirmed scheduling parameters (POSIX
    ///    `pthread_getschedparam`) and pinned core (`sched_getcpu`) into the
    ///    RT telemetry;
    /// 2. primes the activation-precision TLS slot with the mode in effect —
    ///    the first TLS access of a `dlopen`ed module on a fresh thread may
    ///    allocate that thread's TLS block, so it is kept off the `process()`
    ///    hot path;
    /// 3. applies DAZ/FTZ denormal flushing for the thread.
    ///
    /// Idempotent and allocation-free: `stop_processing()` re-arms the latch so a
    /// host that migrates the audio thread re-runs this setup on the new thread
    /// before any further block is processed. `reset()` never re-arms it.
    pub(crate) fn apply_rt_thread_setup(&mut self) {
        self.rt_thread_setup_done = true;
        self.shared
            .cold
            .is_processing
            .store(true, Ordering::Relaxed);

        // Query RT thread scheduling parameters via POSIX FFI.
        // SAFETY: `pthread_self()` is POSIX FFI with no arguments; always succeeds
        // and returns the calling thread's ID safely.
        let thread_id = unsafe { libc::pthread_self() };

        let mut policy = 0i32;
        let mut param = libc::sched_param { sched_priority: 0 };
        // SAFETY: `pthread_getschedparam` takes valid thread ID and valid pointers
        // to stack-local variables with proper alignment and lifetime.
        let getsched_res =
            unsafe { libc::pthread_getschedparam(thread_id, &mut policy, &mut param) };
        if getsched_res == 0 {
            self.rt_status
                .rt_priority
                .store(param.sched_priority, Ordering::Relaxed);
            self.rt_status
                .confirmed_priority
                .store(param.sched_priority, Ordering::Relaxed);
            self.rt_status.rt_policy.store(policy, Ordering::Relaxed);
            if policy == libc::SCHED_FIFO || policy == libc::SCHED_RR {
                self.rt_status
                    .set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RT_IS_FIFO);
            }
        }

        // SAFETY: `sched_getcpu()` takes no arguments, is thread-safe (POSIX/vDSO),
        // and never accesses or dereferences user memory.
        let cpu = unsafe { libc::sched_getcpu() };
        self.rt_status.rt_cpu.store(cpu, Ordering::Relaxed);

        // Prime the activation-precision TLS with the mode in effect. Every
        // later precision change (host events, SPSC snapshots, render-mode
        // transitions) refreshes the same slot on the audio thread, so the
        // per-block unconditional set is no longer needed in `process()`.
        neural_amp_modeler_rs::math::activations::set_activation_tls(
            self.params.activation_precision,
        );

        // Configure denormals handling (DAZ/FTZ) at start of processing.
        // Architecture note: On x86_64, `set_daz_ftz()` manipulates MXCSR via SSE2 instructions.
        // On AArch64/ARM64, flush-to-zero is handled either at the FPCR register level or denormals
        // are flushed in hardware; `set_daz_ftz()` is currently x86_64-specific (baseline target).
        #[cfg(target_arch = "x86_64")]
        {
            // SAFETY: MXCSR modification via stmxcsr/ldmxcsr is safe on x86_64 with SSE2/AVX2.
            unsafe {
                neural_amp_modeler_rs::math::common::set_daz_ftz();
            }
        }
    }
}

impl<'a> PluginAudioProcessor<'a, NamClapShared, NamClapMainThread<'a>> for NamClapProcessor<'a> {
    /// `activate` is the ONLY allocation site — kept out of `process`.
    fn activate(
        host: HostAudioProcessorHandle<'a>,
        main_thread: &NamClapMainThread<'a>,
        shared: &'a NamClapShared,
        audio_config: PluginAudioConfiguration,
    ) -> Result<Self, PluginError> {
        Self::activate(host, main_thread, shared, audio_config)
    }

    fn deactivate(self, main_thread: &NamClapMainThread<'a>) {
        deactivated::deactivate(self, main_thread);
    }

    /// Starts continuous audio processing (CLAP audio thread).
    ///
    /// Runs the one-shot RT-thread setup (confirmed scheduling telemetry,
    /// activation TLS priming, DAZ/FTZ) on this thread so `process()` never
    /// carries first-block setup costs. CLAP invokes this on the audio thread
    /// immediately before the first block; a host that stops and restarts
    /// processing (possibly on another thread) triggers it again after
    /// [`stop_processing`](Self::stop_processing) re-armed the latch.
    fn start_processing(&mut self) -> Result<(), PluginError> {
        self.shared
            .cold
            .is_processing
            .store(true, Ordering::Relaxed);
        // Containment mirrors process()/reset(): any panic in the one-shot
        // setup latches the poisoned state (silenced O(1) processing) instead
        // of unwinding through the C ABI into the host.
        if !self.poisoned {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.apply_rt_thread_setup();
            }));
            if let Err(err) = result {
                // Processor poisoning containment: latch poisoned state and signal to main
                // thread via atomic status flag.
                self.latch_poisoned();
                drop(err);
            }
        }
        Ok(())
    }

    /// Stops continuous audio processing (CLAP audio thread).
    ///
    /// Invalidates the one-shot RT-thread setup: the next
    /// [`start_processing`](Self::start_processing) re-runs it on the calling
    /// thread, keeping priority/CPU telemetry and FTZ/DAZ current even when a
    /// pool host migrates processing to another thread.
    fn stop_processing(&mut self) {
        self.shared
            .cold
            .is_processing
            .store(false, Ordering::Relaxed);
        self.rt_thread_setup_done = false;
    }

    fn process(
        &mut self,
        _process: Process,
        mut audio: Audio,
        events: Events,
    ) -> Result<ProcessStatus, PluginError> {
        // Processor poisoning containment: Fast O(1) containment path if processor is poisoned.
        // Zero allocations, zero I/O, zero log calls on the RT hot-path.
        if self.poisoned {
            silence_audio_ports(&mut audio);
            return Ok(ProcessStatus::Continue);
        }

        // Per-block DAZ/FTZ guard: asserts flush-to-zero/denormals-are-zero
        // for the DSP below and restores the host's MXCSR on scope exit —
        // including the `catch_unwind` panic path (`Drop` never panics, so
        // the restoration provably runs before the poisoning latch below).
        // Instantiated OUTSIDE `catch_unwind` (a `&mut self` borrow cannot
        // cross into the `AssertUnwindSafe` closure, and the guard owns no
        // processor state — only a stack `u32`).
        let _ftz_daz = FtzDazGuard::new();

        // Isolate panics in this instance's audio callback.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            #[cfg(any(test, feature = "testing"))]
            if self
                .shared
                .cold
                .test_panic_injection
                .swap(false, Ordering::Relaxed)
            {
                panic!("Test injected audio processing panic");
            }

            #[cfg(feature = "heap-audit")]
            let _guard = if heap_audit::is_armed()
                && !neural_amp_modeler_rs::common::alloc_audit::is_tracking_active()
            {
                Some(neural_amp_modeler_rs::common::alloc_audit::TrackingGuard::new())
            } else {
                None
            };

            // One-shot RT-thread setup fallback: CLAP-conformant hosts call
            // `start_processing()` (which already ran `apply_rt_thread_setup`
            // on this thread) before the first block. The flag re-check is the
            // cheap defensive path for hosts that skip it — the full setup
            // still runs here exactly once per start, never per block.
            if !self.rt_thread_setup_done {
                self.apply_rt_thread_setup();
            }

            // Per-block telemetry: every block is measured.
            // On an isolated core, the full cost (rdtsc pair + Relaxed stores +
            // histogram + budget + FSM) was measured at ~90-105 cycles (~0.004%
            // of a 64-sample @ 48 kHz block), well below 0.05%. Sampling 1/16
            // would miss 15 of every 16 deadline overruns and skew p99/adaptive
            // compute, so every block is measured without decimation.
            //
            // NOTE (Architecture): rdtsc_nanos() is backed by serialized cycle counters:
            // RDTSC on x86_64, and CNTVCT_EL0 generic timer on aarch64.
            // On other architectures, fallback returns 0u64.
            #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
            let start_nanos = rdtsc_nanos();
            #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
            let start_nanos = 0u64;

            // Event drainage (SPSC + Host + GUI sync + Latency)
            self.process_events(events.output);

            // Two-phase reset amortization window: exactly one
            // `model_l.prewarm_step(k)` drain per callback — after
            // `process_events` (a model swapped in mid-window is converged
            // and clears the latch, never trained stale) and before
            // `process_dsp_audio` (the input stage/gate). Drained callbacks
            // that have not converged hold their block on the
            // latency-compensated dry path (bypass-leg semantics: no
            // crossfade trigger, no gate/FSM advance, telemetry covered); the
            // callback where the model reports convergence clears the latch
            // and processes the host audio through the normal pipeline, so
            // the first wet block equals a fresh instance's first block.
            // `poisoned` never reaches here (O(1) silence above). The
            // latch-armed-but-model-less case is defensively degenerate: it
            // clears and takes the normal path.
            if self.prewarm_pending {
                if let Some(model) = self.model_l.as_mut() {
                    let frames_count = audio.frames_count() as usize;
                    let family = ModelFamily::from_model(model);
                    let is_stereo_config = self
                        .shared
                        .cold
                        .selected_port_config
                        .load(Ordering::Relaxed)
                        == crate::clap::plugin::CONFIG_ID_STEREO;
                    let chains = if is_stereo_config && self.model_r.is_some() {
                        2
                    } else {
                        1
                    };
                    // The frozen family table replaces the runtime EWMA when the latch is set.
                    let measured_cost = if self.prewarm_static {
                        None
                    } else {
                        self.prewarm_cost_ns_per_sample
                    };
                    let step = compute_prewarm_step(
                        frames_count,
                        self.min_frames_count,
                        self.sample_rate,
                        family,
                        measured_cost,
                        chains,
                    );
                    if step > 0 {
                        self.prewarm_zero_budget_count = 0;
                        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
                        let step_start = rdtsc_nanos();
                        let _ = model.prewarm_step(step);
                        if is_stereo_config && let Some(model_r) = self.model_r.as_mut() {
                            let _ = model_r.prewarm_step(step);
                        }
                        // Refine the per-sample cost estimate from the real
                        // drain elapsed time. A step that overran half the
                        // callback period folds in the worst observed cost
                        // immediately so the next callback's budget shrinks
                        // before it can breach the deadline.
                        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
                        {
                            if !self.prewarm_static {
                                #[cfg(any(test, feature = "testing"))]
                                let elapsed = {
                                    let injected =
                                        TEST_PREWARM_ELAPSED_NS.swap(0, Ordering::Relaxed);
                                    if injected > 0 {
                                        injected
                                    } else {
                                        rdtsc_nanos().wrapping_sub(step_start)
                                    }
                                };
                                #[cfg(not(any(test, feature = "testing")))]
                                let elapsed = rdtsc_nanos().wrapping_sub(step_start);

                                if elapsed <= MAX_PLAUSIBLE_ELAPSED_NS {
                                    let effective_frames = if frames_count > 0 {
                                        frames_count
                                    } else {
                                        self.min_frames_count.max(1)
                                    };
                                    let effective_rate = if self.sample_rate > 0.0 {
                                        self.sample_rate
                                    } else {
                                        48000.0
                                    };
                                    let period_ns =
                                        (effective_frames as f64 / effective_rate) * 1e9;
                                    let total_samples = (step * chains) as f64;
                                    let sample_cost = elapsed as f64 / total_samples;
                                    const EWMA_ALPHA: f64 = 0.25;
                                    let mut ewma = match self.prewarm_cost_ns_per_sample {
                                        Some(prev) if prev.is_finite() && prev > 0.0 => {
                                            f64::from(prev) * (1.0 - EWMA_ALPHA)
                                                + sample_cost * EWMA_ALPHA
                                        }
                                        _ => sample_cost,
                                    };
                                    if elapsed as f64 > 0.5 * period_ns {
                                        ewma = ewma.max(sample_cost);
                                    }
                                    self.prewarm_cost_ns_per_sample = Some(ewma as f32);
                                }
                            }
                        }
                        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
                        {
                            #[cfg(any(test, feature = "testing"))]
                            let _ = TEST_PREWARM_ELAPSED_NS.swap(0, Ordering::Relaxed);
                        }
                    } else {
                        // Budget is zero for this callback.
                        // Track consecutive starvation; do NOT force a sample if that would break deadline.
                        self.prewarm_zero_budget_count =
                            self.prewarm_zero_budget_count.saturating_add(1);
                        // Emit request_callback() and update starvation telemetry strictly on the limit
                        // transition (== LIMIT) rather than on every subsequent callback, preventing a
                        // callback storm while zero budget persists. Do NOT assert HOST_CONTRACT_VIOLATION
                        // for an internal budget constraint.
                        if self.prewarm_zero_budget_count
                            == prewarm_policy::PREWARM_ZERO_BUDGET_STARVATION_LIMIT
                        {
                            self.shared
                                .rt_to_ui
                                .prewarm_starvation_epoch
                                .fetch_add(1, Ordering::Relaxed);
                            // Clear potentially elevated EWMA to retry against conservative table cost
                            self.prewarm_cost_ns_per_sample = None;
                            self.host.request_callback();
                        }
                    }
                    let right_incomplete = is_stereo_config
                        && self.model_r.as_ref().is_some_and(|m| !m.prewarm_complete());
                    if !model.prewarm_complete() || right_incomplete {
                        return self.process_dry_contained_block(
                            &mut audio,
                            events.input,
                            start_nanos,
                        );
                    }
                }
                self.prewarm_pending = false;
                self.shared
                    .rt_to_ui
                    .prewarm_pending
                    .store(false, Ordering::Relaxed);
                self.prewarm_zero_budget_count = 0;
            }

            // DSP block (gate, inference, resampling, output, telemetry)
            // Host parameter events are handled sample-accurately via block-splitting.
            self.process_dsp_audio(&mut audio, events.input, start_nanos)
        }));
        match result {
            Ok(r) => r,
            Err(err) => {
                // Processor poisoning containment: Latch poisoned state and signal to main thread via atomic status flag.
                self.latch_poisoned();
                silence_audio_ports(&mut audio);
                Err(panic_to_error(err))
            }
        }
    }

    fn reset(&mut self) {
        self.reset_state();
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) use tests::processor_poisoning_test;
