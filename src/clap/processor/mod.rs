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
mod dsp;
mod events;
mod ftz;
mod gc;
#[cfg(feature = "heap-audit")]
mod heap_audit;
mod params;
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

/// Test-only hook to inject a panic inside the `process()` audio callback.
#[cfg(any(test, feature = "testing"))]
pub(crate) static TEST_PANIC_INJECTION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Test-only hook to inject a synthetic stalled block into telemetry
/// (T-P1.3.2): when non-zero, `process_telemetry` clamps the measured
/// `elapsed_nanos` up to at least this value (`elapsed.max(stall)`) and then
/// resets the hook to zero (one-shot). Lets tests inject exactly one
/// over-budget block deterministically through the real `process()` path
/// without sleeping on the audio thread. `#[cfg]`-gated: absent from every
/// non-test build, zero cost in production.
#[cfg(any(test, feature = "testing"))]
pub(crate) static TEST_TELEMETRY_STALL_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Test-only hook to bind `TEST_TELEMETRY_STALL_NS` to a specific instance ID.
/// When non-zero, only the matching plugin instance will consume the stall.
#[cfg(any(test, feature = "testing"))]
pub(crate) static TEST_TELEMETRY_STALL_TARGET_INSTANCE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Test-only hook to inject a panic inside the `reset()` audio processor method.
#[cfg(any(test, feature = "testing"))]
pub(crate) static TEST_RESET_PANIC_INJECTION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Maximum plausible duration for an audio callback measurement or prewarm step (1 second).
///
/// Defense-in-depth against clock jumps, uncalibrated-to-calibrated flips, or timer anomalies.
/// Measurements exceeding this threshold are discarded: they are not recorded in histograms,
/// do not increment overload counters, and do not update prewarm cost or adaptive compute.
pub const MAX_PLAUSIBLE_ELAPSED_NS: u64 = 1_000_000_000;

/// Test-only hook to inject a prewarm step elapsed duration.
#[cfg(any(test, feature = "testing"))]
pub(crate) static TEST_PREWARM_ELAPSED_NS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

use crate::clap::plugin::errors;
use crate::clap::plugin::{NamClapMainThread, NamClapShared};
use clack_plugin::prelude::*;
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
use neural_amp_modeler_rs::common::tsc::rdtsc_nanos;
use neural_amp_modeler_rs::dsp::gate::GateParams;
use neural_amp_modeler_rs::models::NamModel;
use std::sync::atomic::Ordering;

/// Zeroed-sample stabilization budget drained per audio callback by the
/// two-phase reset amortization window.
///
/// Sized so the worst-case post-reset callback — `reset()` phase zero +
/// `NamModel::prewarm_step(k)` + a 64-frame block — stays under the audio
/// callback deadline (1.33 ms @ 48 kHz; design target < 1.0 ms with 25%
/// margin), measured per fixture on a pinned isolated core:
///
/// // Measured: 2026-10-06, release profile, pinned core 8, N=250 @ 48 kHz,
/// // Block=64 (`test_reset_wall_clock_time_within_contract`, 4 runs):
/// // k=1024 keeps the amortized window inside the 1.33 ms callback budget
/// // for every family as measured — window `prewarm_step(k)` + contained
/// // 64-frame dry block p50 ≈ 69 µs (LSTM), ≈ 48 µs (WaveNet A1 one-shot),
/// // ≈ 0.87–0.93 ms (A2: chained cascade inference ≈ 0.87 µs/sample,
/// // structural engine cost). The A2 *phase-zero* `prewarm_reset` zeroes the
/// // cascade layer buffers in place (measured ≈ 2.0–2.2 ms p50) and is covered
/// // by the budget scoped to LSTM/A1 (A2 carried by its own measured envelope —
/// // a synchronous integral reset was 3.29 ms p50 + 55.7 µs block, so the
/// // two-phase split still lowers the worst-case callback for A2 as well).
///
/// Multiple of 64 by contract (matches the engine's `WAVENET_MAX_NUM_FRAMES`
/// and LSTM 512-sample chunking with no straggler partial chunks).
#[cfg(test)]
pub(crate) const PREWARM_STEP_SAMPLES_PER_CALLBACK: usize = prewarm_policy::PREWARM_STEP_MAX;

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
/// and handles both f32 and f64 host audio ports defensively in depth (F-NP-R4).
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
    /// Idempotent and allocation-free: `stop_processing()` (and `reset()`)
    /// re-arm the latch so a host that migrates the audio thread re-runs this
    /// setup on the new thread before any further block is processed.
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
                // Processor poisoning containment (F-NP-02): latch poisoned state and signal to main
                // thread via atomic status flag.
                self.poisoned = true;
                self.rt_status
                    .set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_PROCESSOR_POISONED);
                self.host.request_callback();
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
        // Processor poisoning containment (F-NP-02): Fast O(1) containment path if processor is poisoned.
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
            if TEST_PANIC_INJECTION.swap(false, Ordering::Relaxed) {
                panic!("Test injected audio processing panic");
            }

            #[cfg(feature = "heap-audit")]
            let _guard = if neural_amp_modeler_rs::common::alloc_audit::AUDIT_ENABLED
                .load(Ordering::Relaxed)
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

            // Per-block telemetry (T-P1.3.2): every block is measured.
            // The T-P1.3.1 measurement (núcleo isolado, 2026-10-01) priced the
            // full cost (par rdtsc + stores Relaxed + histograma + orçamento +
            // FSM) at ~90-105 ciclos ≈ 0,004% de um bloco de 64 @48 kHz — mais
            // de 10× abaixo do limiar de 0,05%. Amostrar 1/16 perderia 15 de
            // cada 16 estouros de deadline e enviesaria o histograma p99 e o
            // FSM adaptativo; medir todo bloco não precisa de decimação.
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
                        // drain elapsed time (U-02 / F-PR-10). A step that
                        // overran half the callback period folds in the worst
                        // observed cost immediately so the next callback's
                        // budget shrinks before it can breach the deadline.
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
                        // Budget is zero for this callback (D-15 / S08.5).
                        // Track consecutive starvation; do NOT force a sample if that would break deadline.
                        self.prewarm_zero_budget_count =
                            self.prewarm_zero_budget_count.saturating_add(1);
                        // Emit request_callback() and update starvation telemetry strictly on the limit
                        // transition (== LIMIT) rather than on every subsequent callback, preventing a
                        // callback storm while zero budget persists. Do NOT assert HOST_CONTRACT_VIOLATION
                        // for an internal budget constraint (S08.5).
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
                // Processor poisoning containment (F-NP-02): Latch poisoned state and signal to main thread via atomic status flag.
                self.poisoned = true;
                self.rt_status
                    .set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_PROCESSOR_POISONED);
                self.host.request_callback();
                silence_audio_ports(&mut audio);
                Err(panic_to_error(err))
            }
        }
    }

    fn reset(&mut self) {
        // If already poisoned, avoid running DSP reset state transitions.
        if self.poisoned {
            return;
        }

        // Full in-place, zero-alloc DSP state reset on timeline discontinuity
        // (seek / loop relocation, `steady_time` regression).
        //
        // Reset target: "a freshly-initialized instance with the same
        // configuration". Every *temporal* state (filter/delay-line history,
        // phases, FIFOs, gate/adaptive FSM state, smoothers, scratch buffers)
        // is cleared in place; every *configuration* (model, IR, params,
        // applied oversampling factor, latency, preset) is preserved — nothing
        // is deallocated or recreated on the audio thread.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            #[cfg(any(test, feature = "testing"))]
            if TEST_RESET_PANIC_INJECTION.swap(false, Ordering::Relaxed) {
                panic!("Test injected reset panic");
            }

            // 1. Recurrent/FIR neural model state (WaveNet / LSTM / ConvNet /
            //    Linear / Container): two-phase reset, phase zero —
            //    `NamModel::prewarm_reset` clears the temporal state to the
            //    freshly-built condition WITHOUT the stabilization feed
            //    (ring/cepstral clearing only: in-place, zero-alloc, O(state);
            //    the integral prewarm's inline zero feed measured 1.81 ms p99
            //    for LSTM and 4.18 ms p99 for WaveNet A2 — moves
            //    off the audio callback into the per-callback amortization
            //    window below `process()`). The latch arms that window: every
            //    following callback drains a fixed zeroed-sample budget and
            //    holds its block on the latency-compensated dry path until
            //    the model reports convergence, then resumes normal wet
            //    processing with the first wet block equal to a fresh
            //    instance's first block (engine split-vs-integral bit-exact
            //    guarantee). Rate/buffer bookkeeping (`sample_rate`,
            //    `max_buffer_size`) is session-constant while active, so the
            //    integral reset's config re-statement is a no-op here.
            if let Some(model) = &mut self.model_l {
                model.prewarm_reset();
                self.prewarm_pending = true;
            }
            let is_stereo_config = self
                .shared
                .cold
                .selected_port_config
                .load(Ordering::Relaxed)
                == crate::clap::plugin::CONFIG_ID_STEREO;
            if is_stereo_config && let Some(model) = &mut self.model_r {
                model.prewarm_reset();
                self.prewarm_pending = true;
            }
            if self.prewarm_pending {
                self.shared
                    .rt_to_ui
                    .prewarm_pending
                    .store(true, Ordering::Relaxed);
            }
            self.prewarm_zero_budget_count = 0;

            // 2. Streaming resample adapter: clears its FIFOs, resets the
            //    inner resampler and re-arms the priming budget — post-reset
            //    cardinality matches a fresh activation.
            self.stream.reset();

            // 3. Polyphase resampler: clears phase accumulators and delay
            //    lines (keeps host/model rates — config).
            self.resampler.reset();

            // 4. Oversampling engines (L/R): clears every half-band delay line,
            //    phase counter and inter-stage scratch (keeps the applied
            //    factor — config).
            self.os_l.reset();
            self.os_r.reset();

            // 5. Cabinet slot (WAV IR convolution or neural NAM model): clears
            //    filter/recurrent memory and FIFOs in place while preserving the asset.
            if let Some(cab) = &mut self.cab_slot {
                cab.reset();
            }
            self.cab_needs_reset = false;
            self.cab_bypass_xfade = BypassCrossfader::new(self.cab_bypass);
            self.cab_dry_delay.reset();

            // 6. Noise-gate hysteresis FSMs back to the initial Open state
            //    (unity multiplier, no ramp).
            self.silence_hyst.reset();
            self.mono_hyst.reset();

            // 7. Adaptive compute FSM back to Full, aborting any in-flight
            //    crossfade; preserves the user mode/override/slimmable config.
            self.adaptive_compute.reset(&self.rt_status);

            // 8. Parameter smoothers snap to their current target — the
            //    current params are reapplied without a ramp that originates
            //    in pre-reset state.
            self.smoother_in.snap_to_target();
            self.smoother_out.snap_to_target();
            self.smoother_cal_in.snap_to_target();
            self.smoother_cal_out.snap_to_target();

            // 9. Bypass crossfader snaps to the current bypass state (no
            //    mid-ramp blend left over from the pre-reset timeline).
            self.bypass_xfade = BypassCrossfader::new(self.params.bypass);

            // 10. Dry delay ring: clears pre-reset history while keeping the
            //     current applied delay (`cached_effective_latency` did not
            //     change — no resource was swapped).
            self.dry_delay.reset();

            // 11. Intermediate/scratch buffers zeroed so no pre-reset sample
            //     survives in a stale slot.
            self.buf_host_l.fill(0.0);
            self.buf_host_r.fill(0.0);
            self.buf_mid_l.fill(0.0);
            self.buf_mid_r.fill(0.0);
            self.buf_out_l.fill(0.0);
            self.buf_out_r.fill(0.0);
            self.buf_os_in_l.fill(0.0);
            self.buf_os_in_r.fill(0.0);
            self.buf_os_model_l.fill(0.0);
            self.buf_os_model_r.fill(0.0);
            self.buf_xfade_dry_l.fill(0.0);
            self.buf_xfade_dry_r.fill(0.0);
            self.buf_xfd_scratch_l.fill(0.0);
            self.buf_xfd_scratch_r.fill(0.0);
            self.buf_cab_dry_l.fill(0.0);
            self.buf_cab_dry_r.fill(0.0);
            self.buf_cab_nam_in_l.fill(0.0);
            self.buf_cab_nam_in_r.fill(0.0);
            self.buf_cab_zero_l.fill(0.0);
            self.buf_cab_zero_r.fill(0.0);

            // 12. Per-block caches a fresh activation starts with: gate cache
            //     invalidated (recomputed from the preserved params on the
            //     next block), modulation offsets zeroed, mono flag at the
            //     default, telemetry cycle counter re-zeroed and the one-shot
            //     RT-thread setup re-armed (re-runs on the next block via the
            //     `process()` fallback, refreshing priority/CPU telemetry and
            //     re-applying FTZ/DAZ on this thread).
            self.scheduled_events.clear();
            self.process_mono = true;
            self.mod_input_gain = 0.0;
            self.mod_output_gain = 0.0;
            self.mod_gate_thresh = 0.0;
            self.cached_threshold_open_sq = 0.0;
            self.cached_threshold_close_sq = 0.0;
            self.cached_gate_params = GateParams::default();
            self.gate_dirty = true;
            self.musical_gate.reset();
            self.rt_thread_setup_done = false;
        }));
        if let Err(err) = result {
            // Latch poisoned state and signal via atomic status flag.
            self.poisoned = true;
            self.rt_status
                .set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_PROCESSOR_POISONED);
            self.host.request_callback();
            drop(err);
        }
    }
}

#[cfg(test)]
#[path = "../processor_bypass_test.rs"]
mod processor_bypass_test;

#[cfg(test)]
#[path = "../processor_dual_mono_test.rs"]
mod processor_dual_mono_test;

#[cfg(test)]
#[path = "../processor_stress_test.rs"]
mod processor_stress_test;

#[cfg(test)]
#[path = "../processor_streaming_cardinality_test.rs"]
mod processor_streaming_cardinality_test;

#[cfg(test)]
#[path = "../processor_gui_test.rs"]
mod processor_gui_test;

#[cfg(test)]
#[path = "../processor_state_test.rs"]
mod processor_state_test;

#[cfg(test)]
#[path = "../processor_heap_audit_test.rs"]
mod processor_heap_audit_test;

#[cfg(test)]
#[path = "../processor_clip_test.rs"]
mod processor_clip_test;

#[cfg(test)]
#[path = "../processor_gc_stress_test.rs"]
mod processor_gc_stress_test;

#[cfg(test)]
#[path = "../processor_calibration_test.rs"]
mod processor_calibration_test;

#[cfg(test)]
#[path = "../processor_automation_test.rs"]
mod processor_automation_test;

#[cfg(test)]
#[path = "../processor_deactivate_reactivate_test.rs"]
mod processor_deactivate_reactivate_test;

#[cfg(test)]
#[path = "../processor_restart_test.rs"]
mod processor_restart_test;

#[cfg(test)]
#[path = "../processor_events_test.rs"]
mod processor_events_test;

#[cfg(test)]
#[path = "../processor_slimmable_generation_test.rs"]
mod processor_slimmable_generation_test;

#[cfg(test)]
#[path = "../processor_command_budget_test.rs"]
mod processor_command_budget_test;

#[cfg(all(test, feature = "heap-audit"))]
#[path = "../processor_drain_latency_test.rs"]
mod processor_drain_latency_test;

#[cfg(test)]
#[path = "../processor_swap_handler_test.rs"]
mod processor_swap_handler_test;

#[cfg(test)]
#[path = "../processor_latency_policy_test.rs"]
mod processor_latency_policy_test;

#[cfg(test)]
#[path = "../processor_dry_delay_test.rs"]
mod processor_dry_delay_test;

#[cfg(test)]
#[path = "../processor_tail_rearm_test.rs"]
mod processor_tail_rearm_test;

#[cfg(test)]
#[path = "../processor_cabsim_block_test.rs"]
mod processor_cabsim_block_test;

#[cfg(test)]
#[path = "../processor_reset_test.rs"]
mod processor_reset_test;

#[cfg(test)]
#[path = "../processor_diagnostics_logging_test.rs"]
mod processor_diagnostics_logging_test;

#[cfg(test)]
#[path = "../processor_multi_instance_isolation_test.rs"]
mod processor_multi_instance_isolation_test;

#[cfg(test)]
#[path = "../processor_temporal_validation_test.rs"]
mod processor_temporal_validation_test;

#[cfg(test)]
#[path = "../processor_rt_thread_test.rs"]
mod processor_rt_thread_test;

#[cfg(test)]
#[path = "../processor_ftz_guard_test.rs"]
mod processor_ftz_guard_test;

#[cfg(test)]
#[path = "../processor_telemetry_test.rs"]
mod processor_telemetry_test;

#[cfg(test)]
#[path = "../processor_poisoning_test.rs"]
mod processor_poisoning_test;

#[cfg(test)]
#[path = "../processor_oversize_block_test.rs"]
mod processor_oversize_block_test;
