// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! # CLAP Real-Time Audio Processor Subsystem
//!
//! Handles real-time audio sample processing inside DAW audio threads in compliance with CLAP specifications.
//!
//! ## Key Architectural Invariants
//! - **RT-Safety**: Zero heap allocations, zero dynamic drops, zero blocking I/O, zero mutexes on the audio thread.
//! - **Submodules Breakdown**:
//!   - **`events`**: SPSC event drainage (Main Thread → Audio Thread) and CLAP input event parameter parsing.
//!   - **`dsp`**: Real-time DSP engine (noise gate, neural inference, oversampling resampler, peak metering).
//!   - **`state`**: `NamClapProcessor` struct declaration and bypass crossfader state machine.
//!   - **`gc`**: Garbage collection queue for offloading obsolete engine models/IRs off the audio thread.
//!   - **`params`**: Parameter smoothing and atomic parameter sync.
//!   - **`rollback`**: Safe state rollback handling on processing panics or host resets.

mod deactivated;
mod dsp;
mod events;
mod gc;
#[cfg(feature = "heap-audit")]
mod heap_audit;
mod params;
mod rollback;
mod state;

pub(crate) use deactivated::DeactivatedDspState;
pub use state::BYPASS_XFADE_SAMPLES;
pub use state::BypassCrossfader;
pub(crate) use state::NamClapProcessor;

use crate::clap::plugin::{CommandConsumer, NamClapMainThread, NamClapShared};
use clack_plugin::prelude::*;
use neural_amp_modeler_rs::common::params::RtProcessingParams;
#[cfg(target_arch = "x86_64")]
use neural_amp_modeler_rs::common::tsc::rdtsc_nanos;
use neural_amp_modeler_rs::dsp::adaptive::AdaptiveCompute;
use neural_amp_modeler_rs::dsp::gate::{DynamicHysteresis, GateParams};
use neural_amp_modeler_rs::dsp::oversample::{OversampleEngine, OversampleFactor};
use neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF;
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::dsp::smoother::ParamSmoother;
use neural_amp_modeler_rs::math::common::AlignedVec;
use neural_amp_modeler_rs::math::dsp::gain_lut::get_gain_lut;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Helper to construct a `PluginError::Message` with a leaked string slice.
///
/// NOTE: Intentional leak — `PluginError::Message` requires `'static` lifetime.
/// Since initialization errors and panic handling occur rarely during plugin setup
/// or emergency unwinding (and crash details are captured to disk), leaking a small
/// error string slice is an intentional design trade-off to satisfy `clack_plugin`
/// API signatures (Finding F01).
#[inline]
fn leak_error_msg(msg: impl Into<String>) -> PluginError {
    // NOTE: Intentional leak — PluginError requires 'static lifetime
    PluginError::Message(Box::leak(msg.into().into_boxed_str()))
}

/// Converts a panic payload into `PluginError` for `catch_unwind` guards (S5-E5-T03).
///
/// The panic hook has already written the full crash report to
/// `~/.cache/nam-rs/crash-*.txt`. This function extracts a human-readable
/// message from the payload so the host can display it.
///
/// NOTE: Intentional leak — PluginError requires 'static lifetime. Dynamic panic
/// payload strings are converted and intentionally leaked via [`leak_error_msg`] to
/// satisfy the `'static` requirement of `PluginError::Message` (Finding F01).
#[cold]
fn panic_to_error(panic_info: Box<dyn std::any::Any + Send>) -> PluginError {
    // NOTE: Intentional leak — PluginError requires 'static lifetime
    if let Some(s) = panic_info.downcast_ref::<String>() {
        leak_error_msg(s.clone())
    } else if let Some(s) = panic_info.downcast_ref::<&str>() {
        leak_error_msg(s.to_string())
    } else {
        PluginError::Message("Plugin panicked — crash report saved to ~/.cache/nam-rs/")
    }
}

/// Note: the entire `PluginAudioProcessor` impl must live in a single block
/// (Rust E0119 — trait impls cannot be split across modules).
impl<'a> PluginAudioProcessor<'a, NamClapShared, NamClapMainThread<'a>> for NamClapProcessor<'a> {
    /// `activate` is the ONLY allocation site — kept out of `process`.
    fn activate(
        host: HostAudioProcessorHandle<'a>,
        main_thread: &mut NamClapMainThread<'a>,
        shared: &'a NamClapShared,
        audio_config: PluginAudioConfiguration,
    ) -> Result<Self, PluginError> {
        // S5-E5-T03: catch panics from this instance so they don't
        // crash the host or other active instances.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            #[cfg(feature = "heap-audit")]
            {
                if std::env::var("NAM_HEAP_AUDIT").is_ok() {
                    neural_amp_modeler_rs::common::alloc_audit::AUDIT_ENABLED
                        .store(true, Ordering::Relaxed);
                }
            }
            // 1. SPSC channel extraction from Shared (ownership transfer)
            // S1-E1-T04: extracted resources are held in a rollback guard.
            // If any later allocation fails, Drop restores everything into ColdShared.
            let param_rx = shared
                .cold
                .param_rx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .ok_or_else(|| {
                    PluginError::Message("param_rx consumer has already been extracted")
                })?;

            let mut rollback = rollback::ActivateRollbackGuard::new(shared);
            rollback.param_rx = Some(param_rx);

            let gc_tx = shared
                .cold
                .gc_tx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .ok_or_else(|| PluginError::Message("gc_tx producer has already been extracted"))?;
            rollback.gc_tx = Some(gc_tx);

            let slimmable_rx = shared
                .cold
                .slimmable_rx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .ok_or_else(|| {
                    PluginError::Message("slimmable_rx consumer has already been extracted")
                })?;
            rollback.slimmable_rx = Some(slimmable_rx);

            // 2. Intermediate buffer pre-allocation (Disjoint Stages)
            let buf_capacity = (audio_config.max_frames_count as usize)
                .max(MAX_RESAMP_BUF)
                .max(1024)
                * 2;
            let buf_host_l = AlignedVec::new(buf_capacity, 0.0f32).map_err(|e| {
                leak_error_msg(format!("pre-allocation of host buffer failed: {e:?}"))
            })?;
            let buf_host_r = AlignedVec::new(buf_capacity, 0.0f32).map_err(|e| {
                leak_error_msg(format!("pre-allocation of host buffer failed: {e:?}"))
            })?;
            let buf_mid_l = AlignedVec::new(buf_capacity, 0.0f32).map_err(|e| {
                leak_error_msg(format!("pre-allocation of mid buffer failed: {e:?}"))
            })?;
            let buf_mid_r = AlignedVec::new(buf_capacity, 0.0f32).map_err(|e| {
                leak_error_msg(format!("pre-allocation of mid buffer failed: {e:?}"))
            })?;
            let buf_model_l = AlignedVec::new(buf_capacity, 0.0f32).map_err(|e| {
                leak_error_msg(format!("pre-allocation of model buffer failed: {e:?}"))
            })?;
            let buf_model_r = AlignedVec::new(buf_capacity, 0.0f32).map_err(|e| {
                leak_error_msg(format!("pre-allocation of model buffer failed: {e:?}"))
            })?;
            let buf_out_l = AlignedVec::new(buf_capacity, 0.0f32).map_err(|e| {
                leak_error_msg(format!("pre-allocation of output buffer failed: {e:?}"))
            })?;
            let buf_out_r = AlignedVec::new(buf_capacity, 0.0f32).map_err(|e| {
                leak_error_msg(format!("pre-allocation of output buffer failed: {e:?}"))
            })?;

            // 2b. Oversample buffer pre-allocation (MAX_RESAMP_BUF * 4 for X4)
            let os_capacity = MAX_RESAMP_BUF * 4;
            let buf_os_in_l = AlignedVec::new(os_capacity, 0.0f32).map_err(|e| {
                leak_error_msg(format!(
                    "pre-allocation of oversample input buffer failed: {e:?}"
                ))
            })?;
            let buf_os_in_r = AlignedVec::new(os_capacity, 0.0f32).map_err(|e| {
                leak_error_msg(format!(
                    "pre-allocation of oversample input buffer failed: {e:?}"
                ))
            })?;
            let buf_os_model_l = AlignedVec::new(os_capacity, 0.0f32).map_err(|e| {
                leak_error_msg(format!(
                    "pre-allocation of oversample model buffer failed: {e:?}"
                ))
            })?;
            let buf_os_model_r = AlignedVec::new(os_capacity, 0.0f32).map_err(|e| {
                leak_error_msg(format!(
                    "pre-allocation of oversample model buffer failed: {e:?}"
                ))
            })?;

            // 2c. Bypass crossfade dry storage (one sub-block of input samples, max_frames_count).
            let xfade_capacity = audio_config.max_frames_count as usize;
            let buf_xfade_dry_l = AlignedVec::new(xfade_capacity, 0.0f32).map_err(|e| {
                leak_error_msg(format!(
                    "pre-allocation of bypass xfade dry buffer failed: {e:?}"
                ))
            })?;
            let buf_xfade_dry_r = AlignedVec::new(xfade_capacity, 0.0f32).map_err(|e| {
                leak_error_msg(format!(
                    "pre-allocation of bypass xfade dry buffer failed: {e:?}"
                ))
            })?;

            // 2d. WaveNet crossfade scratch buffers (0.5.0 run_inference): used
            // as the second-pass output when processing is chunked, so it must
            // not alias any accumulated output buffer. MAX_RESAMP_BUF each.
            let buf_xfd_scratch_l = AlignedVec::new(MAX_RESAMP_BUF, 0.0f32).map_err(|e| {
                leak_error_msg(format!(
                    "pre-allocation of crossfade scratch buffer failed: {e:?}"
                ))
            })?;
            let buf_xfd_scratch_r = AlignedVec::new(MAX_RESAMP_BUF, 0.0f32).map_err(|e| {
                leak_error_msg(format!(
                    "pre-allocation of crossfade scratch buffer failed: {e:?}"
                ))
            })?;

            // 3. DSP component initialization
            let model_rate = shared.cold.model_sample_rate.load(Ordering::Relaxed);
            let model_rate = if model_rate == 0 { 48000 } else { model_rate };
            let host_rate = audio_config.sample_rate as u32;
            let host_buffer = audio_config.max_frames_count;

            // S1-E1-T01: Restore heavy DSP resources from DeactivatedDspState if
            // available, validating sample rate and buffer size invariants. Model
            // weights are always reusable; resampler and conv-engine require
            // matching audio configuration.
            //
            // S1-E1-T04: DeactivatedDspState is extracted into the rollback guard
            // immediately after `.take()`. If any later allocation fails, the
            // guard restores it — avoiding loss of expensive model/engine state.
            rollback.deactivated = shared
                .cold
                .deactivated_dsp
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            let deactivated = rollback.deactivated.take();

            // S4-E4-T02: Resolve the oversampling factor for this activation.
            // Priority: pending restart > UiToRt atomic > Off (fresh).
            let os_factor = {
                let pending = shared
                    .cold
                    .pending_restart_os_factor
                    .swap(0, Ordering::Acquire);
                if pending != 0 {
                    OversampleFactor::from_f32(pending as f32)
                } else {
                    OversampleFactor::from_f32(
                        shared.ui_to_rt.param_oversample.load(Ordering::Relaxed) as f32,
                    )
                }
            };

            let (
                model_l,
                resampler,
                os_l,
                os_r,
                cabsim_adapter,
                model_input_mult_adj,
                model_output_mult_adj,
            ) = if let Some(deact) = deactivated {
                let rate_matches = deact.sample_rate == host_rate;
                let buf_matches = deact.buffer_size == host_buffer;

                // Resampler: reuse only if host sample rate matches the preserved rate.
                let resampler = if rate_matches {
                    deact.resampler
                } else {
                    Box::new(
                        NamResampler::new(host_rate, model_rate, buf_capacity).map_err(|e| {
                            leak_error_msg(format!("Failed to create NamResampler: {:?}", e))
                        })?,
                    )
                };

                // CabSimAdapter: rebuild if buffer size OR sample rate changed, or if not yet built.
                // Rate changes require resampling ir_raw_samples to the new host rate.
                let cabsim_adapter =
                    if deact.cabsim_adapter.is_some() && buf_matches && rate_matches {
                        deact.cabsim_adapter
                    } else {
                        build_cab_sim_from_raw_samples(
                            shared,
                            audio_config.max_frames_count as usize,
                            host_rate,
                        )?
                    };

                // Oversample engines: reuse only if the factor hasn't changed
                // (structural change → rebuild). Otherwise rebuild for the resolved
                // factor (S4-E4-T02).
                let os_l = if deact.os_factor == os_factor {
                    deact.os_l
                } else {
                    Box::new(
                        OversampleEngine::new(os_factor, MAX_RESAMP_BUF).map_err(|e| {
                            leak_error_msg(format!(
                                "Failed to create oversample engine (L): {:?}",
                                e
                            ))
                        })?,
                    )
                };
                let os_r = if deact.os_factor == os_factor {
                    deact.os_r
                } else {
                    Box::new(
                        OversampleEngine::new(os_factor, MAX_RESAMP_BUF).map_err(|e| {
                            leak_error_msg(format!(
                                "Failed to create oversample engine (R): {:?}",
                                e
                            ))
                        })?,
                    )
                };

                // Model weights: always reusable (independent of rates/buffers).
                (
                    deact.model_l,
                    resampler,
                    os_l,
                    os_r,
                    cabsim_adapter,
                    deact.model_input_mult_adj,
                    deact.model_output_mult_adj,
                )
            } else {
                // Fresh build: construct all DSP resources from scratch.
                let resampler = Box::new(
                    NamResampler::new(host_rate, model_rate, buf_capacity).map_err(|e| {
                        leak_error_msg(format!("Failed to create NamResampler: {:?}", e))
                    })?,
                );

                let cabsim_adapter = {
                    build_cab_sim_from_raw_samples(
                        shared,
                        audio_config.max_frames_count as usize,
                        host_rate,
                    )?
                };
                let os_l = Box::new(OversampleEngine::new(os_factor, MAX_RESAMP_BUF).map_err(
                    |e| leak_error_msg(format!("Failed to create oversample engine (L): {:?}", e)),
                )?);
                let os_r = Box::new(OversampleEngine::new(os_factor, MAX_RESAMP_BUF).map_err(
                    |e| leak_error_msg(format!("Failed to create oversample engine (R): {:?}", e)),
                )?);

                (None, resampler, os_l, os_r, cabsim_adapter, 1.0, 1.0)
            };

            let silence_hyst = DynamicHysteresis::new();
            let mono_hyst = DynamicHysteresis::new();

            // 4. Smoother initialization (Sample-Accurate)
            // Warm reset from shared atomics to avoid transient jump on reactivation
            // when gain differs from 0 dB (1.0).
            let gain_lut = get_gain_lut();
            let input_db = f32::from_bits(shared.ui_to_rt.param_input_gain.load(Ordering::Relaxed));
            let output_db =
                f32::from_bits(shared.ui_to_rt.param_output_gain.load(Ordering::Relaxed));
            let smoother_in = ParamSmoother::new(
                gain_lut.db_to_linear(input_db),
                audio_config.sample_rate as f32,
                20.0,
            );
            let smoother_out = ParamSmoother::new(
                gain_lut.db_to_linear(output_db),
                audio_config.sample_rate as f32,
                20.0,
            );

            // S1-E1-T03: Build an atomic snapshot for params that drive smoothers
            // from UiToRt atomics BEFORE constructing the RT processor. This
            // guarantees that self.params starts in sync with the smoother state
            // (both read from the same gain atomics) — no one-block window where
            // params.input_gain_db lags behind smoother_in.target.
            //
            // Non-smoother params (gate, bypass, adaptive_compute, etc.) are left
            // at their defaults and will be synced on the first process events
            // call (SPSC drain or GUI generation guard). Full-param snapshot
            // would trigger AdaptiveCompute::set_mode log on audio thread when
            // values differ from SPSC-delivered state — a pre-existing log-on-RT
            // violation tracked as S5-E5-T02.
            let params = RtProcessingParams {
                input_gain_db: input_db,
                output_gain_db: output_db,
                oversample: os_factor,
                ..RtProcessingParams::default()
            };
            debug_assert!(
                (smoother_in.current_value() - gain_lut.db_to_linear(params.input_gain_db)).abs()
                    < f32::EPSILON * 10.0,
                "S1-E1-T03 invariant: smoother_in must start from the same input_gain_db atomics"
            );
            debug_assert!(
                (smoother_out.current_value() - gain_lut.db_to_linear(params.output_gain_db)).abs()
                    < f32::EPSILON * 10.0,
                "S1-E1-T03 invariant: smoother_out must start from the same output_gain_db atomics"
            );

            // 5. Report initial latency to shared state
            let mut initial_latency = resampler.latency_samples(audio_config.sample_rate as u32);
            initial_latency += os_l.latency_samples() as u32;
            if let Some(ref adapter) = cabsim_adapter {
                initial_latency += adapter.latency_samples() as u32;
            }
            shared
                .rt_to_ui
                .current_latency
                .store(initial_latency, Ordering::Relaxed);
            shared
                .cold
                .sample_rate
                .store(audio_config.sample_rate as u32, Ordering::Relaxed);
            shared
                .cold
                .buffer_size
                .store(audio_config.max_frames_count, Ordering::Relaxed);

            // F3: flush any model deferred by load_model() (state-restore-before-activate).
            // This calls set_max_buffer_size on the main thread before process() starts.
            main_thread.flush_pending_model()?;

            // S1-E1-T04: defuse the rollback guard — transfers SPSC channel
            // ownership back for processor construction. Guard Drop is now a no-op.
            let channels = rollback.defuse()?;

            let cmd_consumer = CommandConsumer::new(channels.param_rx, &shared.cold.cmd_last_ack);

            let cabsim_tail_initial = cabsim_adapter.as_ref().map_or(0, |a| a.tail_samples());

            Ok(Self {
                model_l,
                cabsim_adapter,
                resampler,
                os_l,
                os_r,
                params,
                buf_host_l,
                buf_host_r,
                buf_mid_l,
                buf_mid_r,
                buf_model_l,
                buf_model_r,
                buf_out_l,
                buf_out_r,
                buf_os_in_l,
                buf_os_in_r,
                buf_os_model_l,
                buf_os_model_r,
                buf_xfade_dry_l,
                buf_xfade_dry_r,
                buf_xfd_scratch_l,
                buf_xfd_scratch_r,
                silence_hyst,
                mono_hyst,
                process_mono: true,
                scheduled_events: Vec::with_capacity(4096),
                bypass_xfade: state::BypassCrossfader::new(params.bypass),
                rt_status: Arc::clone(&shared.cold.rt_status),
                adaptive_compute: AdaptiveCompute::new(
                    neural_amp_modeler_rs::common::params::AdaptiveComputeMode::Conservative,
                ),
                shared,
                smoother_in,
                smoother_out,
                model_input_mult_adj,
                model_output_mult_adj,
                cmd_consumer,
                gc_tx: channels.gc_tx,
                slimmable_rx: channels.slimmable_rx,
                gc_overflow: Arc::clone(&shared.cold.gc_overflow),
                parking_lot: Default::default(),
                mod_input_gain: 0.0,
                mod_output_gain: 0.0,
                mod_gate_thresh: 0.0,
                cached_threshold_open_sq: 0.0,
                cached_threshold_close_sq: 0.0,
                cached_gate_params: GateParams::default(),
                gate_dirty: true,
                cycles_since_telemetry: 0,
                prio_checked: false,
                last_seen_generation: 0,
                max_frames_count: audio_config.max_frames_count as usize,
                last_render_mode: 0,
                realtime_activation:
                    neural_amp_modeler_rs::common::params::ActivationPrecision::Standard,
                gain_lut: get_gain_lut(),
                cabsim_tail_remaining: cabsim_tail_initial,
                host,
            })
        }));
        match result {
            Ok(r) => r,
            Err(err) => Err(panic_to_error(err)),
        }
    }

    fn deactivate(mut self, _main_thread: &mut NamClapMainThread<'a>) {
        // S5-E5-T03: isolate panics during cleanup.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut param_rx_guard = self
                .shared
                .cold
                .param_rx
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *param_rx_guard = Some(self.cmd_consumer.into_inner());

            let mut gc_tx_guard = self
                .shared
                .cold
                .gc_tx
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *gc_tx_guard = Some(self.gc_tx);

            let mut slimmable_rx_guard = self
                .shared
                .cold
                .slimmable_rx
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *slimmable_rx_guard = Some(self.slimmable_rx);

            // S1-E1-T01: Preserve heavy DSP resources across deactivate/activate
            // cycles to avoid I/O, filter-bank recompute, and FFT setup on the
            // next activate(). Resources are validated on restore against the
            // current audio configuration.
            let deactivated = DeactivatedDspState {
                model_l: self.model_l,
                cabsim_adapter: self.cabsim_adapter,
                resampler: self.resampler,
                os_l: self.os_l,
                os_r: self.os_r,
                os_factor: self.params.oversample,
                sample_rate: self.shared.cold.sample_rate.load(Ordering::Relaxed),
                buffer_size: self.shared.cold.buffer_size.load(Ordering::Relaxed),
                model_input_mult_adj: self.model_input_mult_adj,
                model_output_mult_adj: self.model_output_mult_adj,
            };
            *self
                .shared
                .cold
                .deactivated_dsp
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(deactivated);

            // R-04: handoff single-owner do parking lot RT para o drain final
            // off-RT. A thread de áudio já parou (o host chama deactivate()
            // depois do stop_processing) e o processador ainda não foi
            // dropado — uma única chamada a drain_gc_channels libera SPSC +
            // overflow + os 16 slots no main thread, nunca no RT.
            _main_thread.drain_gc_final(&mut self.parking_lot);
        }));
        if let Err(err) = result {
            // Deactivate panicked — resources may leak but crash report
            // was already written. Drop the payload silently.
            drop(err);
        }
    }

    fn process(
        &mut self,
        _process: Process,
        mut audio: Audio,
        events: Events,
    ) -> Result<ProcessStatus, PluginError> {
        // S5-E5-T03: isolate panics in this instance's audio callback.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            #[cfg(feature = "heap-audit")]
            let _guard = if neural_amp_modeler_rs::common::alloc_audit::AUDIT_ENABLED
                .load(Ordering::Relaxed)
            {
                Some(neural_amp_modeler_rs::common::alloc_audit::TrackingGuard::new())
            } else {
                None
            };

            // S5-E5-T01: Per-instance activation precision via TLS.
            // Activation updates within process() (host events, SPSC, GUI sync, offline↔realtime)
            // call set_activation_tls() to reflect the new value.
            neural_amp_modeler_rs::math::activations::set_activation_tls(
                self.params.activation_precision,
            );

            let should_measure = self.cycles_since_telemetry & 0xF == 0;
            self.cycles_since_telemetry = self.cycles_since_telemetry.wrapping_add(1);

            // NOTE (Architecture Limitation / F11): rdtsc_nanos() relies on hardware TSC reading (x86_64).
            // While x86-64-v3 is the mandatory baseline target architecture for this project,
            // conditionally guarding the call with target_arch ensures transparent compilation and fallback
            // (returning 0) on non-x86_64 targets (e.g. ARM64 / AArch64).
            let start_nanos = if should_measure {
                #[cfg(target_arch = "x86_64")]
                {
                    rdtsc_nanos()
                }
                #[cfg(not(target_arch = "x86_64"))]
                {
                    0
                }
            } else {
                0
            };

            // One-time thread priority query on the first processed block
            if !self.prio_checked {
                self.prio_checked = true;
                // SAFETY: `pthread_self()` returns a valid thread handle for the
                // calling thread. `pthread_getschedparam()` reads scheduling
                // attributes into stack-local variables using FFI defined by POSIX.
                unsafe {
                    let thread_id = libc::pthread_self();
                    let mut policy = 0i32;
                    let mut param: libc::sched_param = std::mem::zeroed();
                    if libc::pthread_getschedparam(thread_id, &mut policy, &mut param) == 0 {
                        self.rt_status
                            .rt_priority
                            .store(param.sched_priority, Ordering::Relaxed);
                        self.rt_status
                            .confirmed_priority
                            .store(param.sched_priority, Ordering::Relaxed);
                        self.rt_status.rt_policy.store(policy, Ordering::Relaxed);
                        if policy == libc::SCHED_FIFO || policy == libc::SCHED_RR {
                            self.rt_status.set_flag(
                                neural_amp_modeler_rs::common::spsc::RT_STATUS_RT_IS_FIFO,
                            );
                        }
                    }
                    let cpu = libc::sched_getcpu();
                    self.rt_status.rt_cpu.store(cpu, Ordering::Relaxed);
                    neural_amp_modeler_rs::math::common::set_daz_ftz();
                }
            }

            // Periodic DAZ/FTZ reapplication: hosts may reset MXCSR after callbacks
            // (e.g. during GUI repaints or parameter flushes from another thread).
            // Reassert DAZ+FTZ every 1024 blocks using the existing telemetry counter
            // — the conditional is a single bit-test (1 cycle; cold branch).
            // SAFETY: DAZ+FTZ are SSE2 control bits on x86-64 — unconditionally safe.
            if self.cycles_since_telemetry & 0x3FF == 0 {
                unsafe {
                    neural_amp_modeler_rs::math::common::set_daz_ftz();
                }
            }

            // Event drainage (SPSC + Host + GUI sync + Latency)
            self.process_events(events.output);

            // DSP block (gate, inference, resampling, output, telemetry)
            // Host parameter events are handled sample-accurately via block-splitting.
            self.process_dsp_audio(&mut audio, events.input, start_nanos)
        }));
        match result {
            Ok(r) => r,
            Err(err) => Err(panic_to_error(err)),
        }
    }
}

fn build_cab_sim_from_raw_samples(
    shared: &NamClapShared,
    partition_size: usize,
    host_rate: u32,
) -> Result<Option<neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter>, PluginError> {
    use neural_amp_modeler_rs::dsp::cabsim::loader::CabSimIr;
    use std::sync::atomic::Ordering;

    let raw_guard = shared
        .cold
        .ir_raw_samples
        .lock()
        .map_err(|e| leak_error_msg(format!("ir_raw_samples lock poisoned: {e}")))?;
    let Some(ref samples) = *raw_guard else {
        return Ok(None);
    };

    let stored_rate = shared.cold.ir_raw_sample_rate.load(Ordering::Relaxed);

    let resolved_samples: std::borrow::Cow<'_, [f32]> =
        if stored_rate > 0 && stored_rate != host_rate {
            let resampled = CabSimIr::resample(samples, stored_rate, host_rate).map_err(|e| {
                leak_error_msg(format!(
                    "IR resample failed: {} Hz → {} Hz: {e}",
                    stored_rate, host_rate
                ))
            })?;
            std::borrow::Cow::Owned(resampled)
        } else {
            std::borrow::Cow::Borrowed(samples)
        };

    if partition_size == 0 {
        return Ok(None);
    }

    let engine = neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine::new(
        &resolved_samples,
        partition_size,
    )
    .map_err(|e| leak_error_msg(format!("ConvEngine allocation failed: {e:?}")))?;

    Ok(Some(
        neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter::new(Box::new(engine))
            .map_err(|e| leak_error_msg(format!("CabSimAdapter allocation failed: {e:?}")))?,
    ))
}

#[cfg(test)]
#[path = "../processor_bypass_test.rs"]
mod processor_bypass_test;

#[cfg(test)]
#[path = "../processor_stress_test.rs"]
mod processor_stress_test;

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
mod diagnostics_logging_tests {
    use crate::clap::test_util;
    use neural_amp_modeler_rs::common::spsc::{
        RT_STATUS_GC_OVERFLOW, RT_STATUS_HAS_CLIPPED, RT_STATUS_HUGEPAGE_OK,
        RT_STATUS_MODEL_LOAD_FAILED, RtStatusFlags,
    };
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    // Task 4.3.1 — flag set/clear mechanism for emit_pending_logs
    #[test]
    fn test_flag_set_and_clear_mechanism() {
        let rt_status = Arc::new(RtStatusFlags::new());

        rt_status.set_flag(RT_STATUS_HAS_CLIPPED);
        rt_status.set_flag(RT_STATUS_GC_OVERFLOW);
        rt_status.set_flag(RT_STATUS_HUGEPAGE_OK);

        assert!(rt_status.check_flag(RT_STATUS_HAS_CLIPPED));
        assert!(rt_status.check_flag(RT_STATUS_GC_OVERFLOW));
        assert!(rt_status.check_flag(RT_STATUS_HUGEPAGE_OK));

        assert!(rt_status.check_and_clear_flag(RT_STATUS_HAS_CLIPPED));
        assert!(!rt_status.check_flag(RT_STATUS_HAS_CLIPPED));

        assert!(rt_status.check_and_clear_flag(RT_STATUS_GC_OVERFLOW));
        assert!(!rt_status.check_flag(RT_STATUS_GC_OVERFLOW));

        assert!(rt_status.check_and_clear_flag(RT_STATUS_HUGEPAGE_OK));
        assert!(!rt_status.check_flag(RT_STATUS_HUGEPAGE_OK));

        let flags_seen = rt_status.flags_seen.load(Ordering::Relaxed);
        assert_eq!(
            flags_seen,
            RT_STATUS_HAS_CLIPPED | RT_STATUS_GC_OVERFLOW | RT_STATUS_HUGEPAGE_OK,
            "flags_seen should accumulate all flags that were ever set"
        );
    }

    // Task 4.3.1 — verify flag-to-log messages reach LogBuffer
    #[test]
    fn test_emit_pending_logs_messages_reach_log_buffer() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

        let snapshot_before =
            neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::log_buffer()
                .expect("LogBuffer should be accessible")
                .len();

        shared
            .cold
            .rt_status
            .check_and_clear_flag(RT_STATUS_HAS_CLIPPED);
        log::warn!("NAM-rs: Output clipping detected!");

        shared
            .cold
            .rt_status
            .check_and_clear_flag(RT_STATUS_GC_OVERFLOW);
        log::error!("NAM-rs: GC channel overflow! Possible memory leak.");

        shared
            .cold
            .rt_status
            .check_and_clear_flag(RT_STATUS_MODEL_LOAD_FAILED);
        log::error!("NAM-rs: Critical failure! No active model for processing.");

        let snapshot_after =
            neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::log_buffer()
                .expect("LogBuffer should be accessible")
                .len();
        assert!(
            snapshot_after > snapshot_before + 2,
            "LogBuffer should grow after log entries are emitted"
        );

        test_util::assert_log_buffer_contains("Output clipping detected");
        test_util::assert_log_buffer_contains("GC channel overflow");
        test_util::assert_log_buffer_contains("Critical failure! No active model for processing");
    }

    // Task 4.3.1 — verify state save emits confirmation log
    static TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_state_save_emits_confirmation_log() {
        use log::LevelFilter;
        let _guard = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let logger = neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::global()
            .expect("NamLogger should be initialized");
        let original_level = log::max_level();
        log::set_max_level(LevelFilter::Debug);
        logger.set_max_level(LevelFilter::Debug);

        let model_path = crate::clap::test_util::model_path("lstm.nam");

        use neural_amp_modeler_rs::common::params::ProcessingParams;
        let params = ProcessingParams {
            model_path: Some(model_path),
            input_gain_db: 1.0,
            output_gain_db: -2.0,
            gate_threshold_db: -50.0,
            model_basename: Some("lstm.nam".to_string()),
            model_search_paths: vec![],
            model_hash: None,
            bypass: false,
            adaptive_compute: neural_amp_modeler_rs::common::params::AdaptiveComputeMode::Off,
            slim_override: Default::default(),
            oversample: neural_amp_modeler_rs::dsp::oversample::OversampleFactor::Off,
            ir_path: None,
            ir_hash: None,
            activation_precision:
                neural_amp_modeler_rs::common::params::ActivationPrecision::Standard,
        };
        let state_bytes = serde_json::to_vec(&params).unwrap();
        let state_ext = test_util::get_state_ext(&mut plugin_instance);
        let mut handle = plugin_instance.plugin_handle();
        state_ext
            .load(&mut handle, &mut state_bytes.as_slice())
            .expect("Failed to load state");

        let mut output = Vec::new();
        let mut handle = plugin_instance.plugin_handle();
        state_ext
            .save(&mut handle, &mut output)
            .expect("save should succeed");

        assert!(!output.is_empty(), "save output should not be empty");
        test_util::assert_log_buffer_contains("[State] Save completed:");
        test_util::assert_log_buffer_contains("bytes serialized.");

        log::set_max_level(original_level);
        logger.set_max_level(original_level);
    }
}
