// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

impl<'a> NamClapProcessor<'a> {
    /// Activates the real-time audio processor.
    ///
    /// `activate` is the ONLY allocation site — kept out of `process`.
    pub(crate) fn activate(
        host: HostAudioProcessorHandle<'a>,
        main_thread: &NamClapMainThread<'a>,
        shared: &'a NamClapShared,
        audio_config: PluginAudioConfiguration,
    ) -> Result<Self, PluginError> {
        // Catch panics from this instance so they don't
        // crash the host or other active instances.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // 1. SPSC channel extraction from Shared (ownership transfer)
            // Extracted resources are held in a rollback guard.
            // If any later allocation fails, Drop restores everything into ColdShared.
            let param_rx = shared
                .cold
                .param_rx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .ok_or_else(|| {
                    PluginError::Message(errors::activation::PARAM_RX_ALREADY_EXTRACTED)
                })?;

            let mut rollback = rollback::ActivateRollbackGuard::new(shared);
            rollback.param_rx = Some(param_rx);

            let gc_tx = shared
                .cold
                .gc_tx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .ok_or_else(|| PluginError::Message(errors::activation::GC_TX_ALREADY_EXTRACTED))?;
            rollback.gc_tx = Some(gc_tx);

            let box_gc_tx = shared
                .cold
                .box_gc_tx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .ok_or_else(|| {
                    PluginError::Message(errors::activation::BOX_GC_TX_ALREADY_EXTRACTED)
                })?;
            rollback.box_gc_tx = Some(box_gc_tx);

            let slimmable_rx = shared
                .cold
                .slimmable_rx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .ok_or_else(|| {
                    PluginError::Message(errors::activation::SLIMMABLE_RX_ALREADY_EXTRACTED)
                })?;
            rollback.slimmable_rx = Some(slimmable_rx);

            // 2. Intermediate buffer pre-allocation (Disjoint Stages)
            let alloc_buf = |cap: usize, stage: &str| {
                AlignedVec::new(cap, 0.0f32).map_err(|e| buffer_prealloc_error(stage, e))
            };
            let buf_capacity = (audio_config.max_frames_count as usize)
                .max(MAX_RESAMP_BUF)
                .max(1024)
                * 2;
            let buf_host_l = alloc_buf(buf_capacity, "host")?;
            let buf_host_r = alloc_buf(buf_capacity, "host")?;
            let buf_out_l = alloc_buf(buf_capacity, "output")?;
            let buf_out_r = alloc_buf(buf_capacity, "output")?;

            // 2b. Oversample buffer pre-allocation (MAX_RESAMP_BUF * 4 for X4)
            let os_capacity = MAX_RESAMP_BUF * 4;
            let buf_os_in_l = alloc_buf(os_capacity, "oversample input")?;
            let buf_os_in_r = alloc_buf(os_capacity, "oversample input")?;
            let buf_os_model_l = alloc_buf(os_capacity, "oversample model")?;
            let buf_os_model_r = alloc_buf(os_capacity, "oversample model")?;

            // 2c. Bypass crossfade dry storage (one sub-block of input samples, max_frames_count).
            let xfade_capacity = audio_config.max_frames_count as usize;
            let buf_xfade_dry_l = alloc_buf(xfade_capacity, "bypass xfade dry")?;
            let buf_xfade_dry_r = alloc_buf(xfade_capacity, "bypass xfade dry")?;

            // 2d. WaveNet crossfade scratch buffers (MAX_RESAMP_BUF each).
            let buf_xfd_scratch_l = alloc_buf(MAX_RESAMP_BUF, "crossfade scratch")?;
            let buf_xfd_scratch_r = alloc_buf(MAX_RESAMP_BUF, "crossfade scratch")?;

            // 2e. Cabinet bypass & neural cabinet scratch buffers (zero RT alloc).
            let buf_cab_dry_l = alloc_buf(buf_capacity, "cab dry delay")?;
            let buf_cab_dry_r = alloc_buf(buf_capacity, "cab dry delay")?;
            let buf_cab_nam_in_l = alloc_buf(buf_capacity, "cab nam input")?;
            let buf_cab_nam_in_r = alloc_buf(buf_capacity, "cab nam input")?;
            let buf_cab_zero_l = alloc_buf(buf_capacity, "cab zero input")?;
            let buf_cab_zero_r = alloc_buf(buf_capacity, "cab zero input")?;

            // 3. DSP component initialization
            let model_rate = shared.cold.model_sample_rate.load(Ordering::Relaxed);
            let model_rate = if model_rate == 0 { 48000 } else { model_rate };
            let host_rate = (audio_config.sample_rate as u32).clamp(4_000, 384_000);
            let host_buffer = audio_config.max_frames_count;

            // Resolve the oversampling factor for this activation.
            // Priority: pending restart > UiToRt atomic > Off (fresh).
            // The pending restart is consumed atomically here; if any later
            // fallible stage fails, the rollback guard restores it so the
            // request survives into the next activate() attempt.
            let pending_restart =
                PendingRestartOs::take(&shared.cold.pending_restart_os_factor, Ordering::Acquire);
            // The host restart being serviced consumed the request: a stale
            // latch must never fire a second `request_restart()` on the next
            // callback.
            shared.cold.restart_wanted.store(false, Ordering::Release);
            if pending_restart != PendingRestartOs::None {
                rollback.pending_restart_os_factor = Some(pending_restart);
                // The request latch was cleared above; if this activation
                // fails the guard must re-raise it together with the factor.
                rollback.restart_wanted_restore = true;
            }
            let os_factor = match pending_restart {
                PendingRestartOs::Pending(f) => f,
                PendingRestartOs::None => OversampleFactor::from_f32(
                    shared.ui_to_rt.param_oversample.load(Ordering::Relaxed) as f32,
                ),
            };

            // Restore heavy DSP resources from DeactivatedDspState if
            // available, validating sample rate and buffer size invariants. Model
            // weights are always reusable; resampler and conv-engine require
            // matching audio configuration.
            //
            // DeactivatedDspState is extracted into the rollback guard
            // immediately after `.take()`. The guard retains it intact during all
            // fallible allocations (resamplers, oversamplers, CabSim).
            rollback.deactivated = shared
                .cold
                .deactivated_dsp
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();

            let (rate_matches, stream_matches, cab_matches, os_matches) =
                if let Some(ref deact) = rollback.deactivated {
                    let rate_matches = deact.sample_rate == host_rate;
                    let buf_matches = deact.buffer_size == host_buffer;
                    let os_matches = deact.os_factor == os_factor;
                    let cab_matches = deact.cab_slot.as_ref().is_some_and(|c| {
                        c.is_compatible_with_host(host_rate, host_buffer as usize)
                    });
                    (
                        rate_matches,
                        rate_matches && buf_matches,
                        cab_matches,
                        os_matches,
                    )
                } else {
                    (false, false, false, false)
                };

            let new_resampler = if rate_matches {
                None
            } else {
                Some(Box::new(
                    NamResampler::new_simple_typed(host_rate, model_rate).map_err(|e| {
                        static_plugin_error(
                            errors::dsp_resources::RESAMPLER_BUILD_FAILED,
                            format_args!("{e:?}"),
                        )
                    })?,
                ))
            };
            let new_stream = if stream_matches {
                None
            } else {
                Some(
                    crate::clap::plugin::build_stream_adapter(
                        host_rate,
                        model_rate,
                        host_buffer as usize,
                    )
                    .map_err(|e| {
                        static_plugin_error(
                            errors::dsp_resources::STREAM_BUILD_FAILED,
                            format_args!("{e:?}"),
                        )
                    })?,
                )
            };
            let new_cabsim = if cab_matches {
                None
            } else {
                Some(build_cab_slot_from_cold_storage(
                    shared,
                    None,
                    None,
                    audio_config.max_frames_count as usize,
                    host_rate,
                )?)
            };
            let new_os = if os_matches {
                None
            } else {
                let os_l = Box::new(OversampleEngine::new(os_factor, MAX_RESAMP_BUF).map_err(
                    |e| {
                        static_plugin_error(
                            errors::dsp_resources::OVERSAMPLER_BUILD_FAILED,
                            format_args!("(L) engine: {e:?}"),
                        )
                    },
                )?);
                let os_r = Box::new(OversampleEngine::new(os_factor, MAX_RESAMP_BUF).map_err(
                    |e| {
                        static_plugin_error(
                            errors::dsp_resources::OVERSAMPLER_BUILD_FAILED,
                            format_args!("(R) engine: {e:?}"),
                        )
                    },
                )?);
                Some((os_l, os_r))
            };

            // Consume any latency-affecting restore or swap staged for this
            // restart cycle. All fallible stages above already succeeded, so
            // staged items are never stranded by a failure between here and
            // `Self` construction.
            let mut staged_guard = StagedItemsRollbackGuard {
                main_thread,
                staged_restore: main_thread.staged_restore.borrow_mut().take(),
                staged_swap: main_thread.staged_swap.borrow_mut().take(),
                disarmed: false,
            };

            // Adapt staged_restore for any host audio configuration changes.
            if let Some(ref mut r) = staged_guard.staged_restore {
                if let Some(ref mut m) = r.txn.model {
                    adapt_staged_model_for_host_config(
                        m,
                        host_rate,
                        host_buffer,
                        "staged restore model",
                    )?;
                }

                if let Some(Some(ref mut cab_slot)) = r.txn.ir
                    && !cab_slot.is_exact_host_config(host_rate, host_buffer as usize)
                {
                    let override_wav = r
                        .publish
                        .ir_raw_samples
                        .as_deref()
                        .map(|s| (s, r.publish.ir_raw_sample_rate));
                    let override_nam = r
                        .publish
                        .cab_nam_raw_bytes
                        .as_ref()
                        .map(|(k, b)| (*k, b.as_slice()));
                    if let Some(new_slot) = build_cab_slot_from_cold_storage(
                        shared,
                        override_wav,
                        override_nam,
                        host_buffer as usize,
                        host_rate,
                    )? {
                        *cab_slot = new_slot;
                    }
                }
            }

            // Adapt staged_swap for any host configuration changes.
            if let Some(ref mut s) = staged_guard.staged_swap {
                if let Some(ref mut m) = s.model {
                    adapt_staged_model_for_host_config(
                        m,
                        host_rate,
                        host_buffer,
                        "staged model swap",
                    )?;
                }

                if let Some(Some(ref mut cab_slot)) = s.ir
                    && !cab_slot.is_exact_host_config(host_rate, host_buffer as usize)
                    && let Some(new_slot) = build_cab_slot_from_cold_storage(
                        shared,
                        None,
                        None,
                        host_buffer as usize,
                        host_rate,
                    )?
                {
                    *cab_slot = new_slot;
                }
            }

            let staged_restore = staged_guard.staged_restore.take();
            let (staged_model, staged_ir) = match staged_guard.staged_swap.take() {
                Some(s) => (s.model, s.ir),
                None => (None, None),
            };
            staged_guard.disarmed = true;

            let staged_restore_valid = staged_restore.is_some();
            let staged_model_ok = staged_model.is_some();

            // All fallible stages succeeded: now assemble the active DSP resources
            // by taking ownership from staged items or the rollback guard.
            let (
                mut model_l,
                mut model_r,
                model_generation,
                resampler,
                stream,
                os_l,
                os_r,
                cabsim_pair,
                model_input_mult_adj,
                model_output_mult_adj,
                model_cal_meta,
            ) = if staged_restore_valid {
                // `staged_restore_valid` is `staged_restore.as_ref().is_some_and(..)`,
                // so the flag implies a staged value; a `None` here is an internal
                // invariant violation and must not panic on the activation path.
                let Some(StagedRestore { txn, publish }) = staged_restore else {
                    debug_assert!(false, "staged_restore_valid implies a staged restore");
                    return Err(PluginError::Message(
                        errors::activation::STAGED_RESTORE_UNAVAILABLE,
                    ));
                };
                crate::clap::extensions::state_transaction::publish_restore(publish, main_thread);
                shared
                    .cold
                    .last_applied_generation
                    .store(txn.generation, Ordering::Relaxed);

                if let Some(deact) = rollback.deactivated.take() {
                    let (
                        model_l,
                        model_r,
                        model_generation,
                        resampler,
                        stream,
                        model_input_mult_adj,
                        model_output_mult_adj,
                        model_cal_meta,
                    ) = if let Some(m) = txn.model {
                        (
                            m.model_l,
                            m.model_r,
                            m.generation,
                            m.new_resampler,
                            m.new_stream,
                            m.input_mult_adj,
                            m.output_mult_adj,
                            m.cal_meta,
                        )
                    } else {
                        (
                            deact.model_l,
                            deact.model_r,
                            deact.model_generation,
                            new_resampler.unwrap_or(deact.resampler),
                            new_stream.unwrap_or(deact.stream),
                            deact.model_input_mult_adj,
                            deact.model_output_mult_adj,
                            deact.model_cal_meta,
                        )
                    };

                    let cab_slot = if let Some(ir) = txn.ir {
                        ir
                    } else if let Some(new_c) = new_cabsim {
                        new_c
                    } else {
                        deact.cab_slot
                    };

                    let (os_l, os_r) = if let Some((l, r)) = new_os {
                        (l, r)
                    } else {
                        (deact.os_l, deact.os_r)
                    };

                    (
                        model_l,
                        model_r,
                        model_generation,
                        resampler,
                        stream,
                        os_l,
                        os_r,
                        cab_slot,
                        model_input_mult_adj,
                        model_output_mult_adj,
                        model_cal_meta,
                    )
                } else {
                    let (
                        model_l,
                        model_r,
                        model_generation,
                        resampler,
                        stream,
                        model_input_mult_adj,
                        model_output_mult_adj,
                        model_cal_meta,
                    ) = if let Some(m) = txn.model {
                        (
                            m.model_l,
                            m.model_r,
                            m.generation,
                            m.new_resampler,
                            m.new_stream,
                            m.input_mult_adj,
                            m.output_mult_adj,
                            m.cal_meta,
                        )
                    } else {
                        (
                            None,
                            None,
                            0,
                            new_resampler.ok_or_else(|| {
                                PluginError::Message(
                                    errors::activation::RESAMPLER_UNAVAILABLE_STAGED,
                                )
                            })?,
                            new_stream.ok_or_else(|| {
                                PluginError::Message(errors::activation::STREAM_UNAVAILABLE_STAGED)
                            })?,
                            1.0,
                            1.0,
                            crate::clap::extensions::calibration::ModelCalibrationMeta::empty(),
                        )
                    };

                    let cab_slot = if let Some(ir) = txn.ir {
                        ir
                    } else {
                        new_cabsim.ok_or_else(|| {
                            PluginError::Message(errors::activation::CABSIM_UNAVAILABLE_STAGED)
                        })?
                    };

                    let (os_l, os_r) = new_os.ok_or_else(|| {
                        PluginError::Message(errors::activation::OVERSAMPLER_UNAVAILABLE_STAGED)
                    })?;

                    (
                        model_l,
                        model_r,
                        model_generation,
                        resampler,
                        stream,
                        os_l,
                        os_r,
                        cab_slot,
                        model_input_mult_adj,
                        model_output_mult_adj,
                        model_cal_meta,
                    )
                }
            } else if let Some(deact) = rollback.deactivated.take() {
                let (
                    model_l,
                    model_r,
                    model_generation,
                    resampler,
                    stream,
                    model_input_mult_adj,
                    model_output_mult_adj,
                    model_cal_meta,
                ) = if staged_model_ok {
                    // `staged_model_ok` is derived from
                    // `staged_swap.model.as_ref().is_some_and(..)`, so the flag
                    // implies a staged model; a `None` here is an internal
                    // invariant violation and must not panic on the activation path.
                    let Some(m) = staged_model else {
                        debug_assert!(false, "staged_model_ok implies a staged model");
                        return Err(PluginError::Message(
                            errors::activation::STAGED_MODEL_UNAVAILABLE,
                        ));
                    };
                    (
                        m.model_l,
                        m.model_r,
                        m.generation,
                        m.new_resampler,
                        m.new_stream,
                        m.input_mult_adj,
                        m.output_mult_adj,
                        m.cal_meta,
                    )
                } else {
                    (
                        deact.model_l,
                        deact.model_r,
                        deact.model_generation,
                        new_resampler.unwrap_or(deact.resampler),
                        new_stream.unwrap_or(deact.stream),
                        deact.model_input_mult_adj,
                        deact.model_output_mult_adj,
                        deact.model_cal_meta,
                    )
                };
                let cab_slot = if let Some(ir) = staged_ir {
                    ir
                } else if let Some(new_c) = new_cabsim {
                    new_c
                } else {
                    deact.cab_slot
                };
                let (os_l, os_r) = if let Some((l, r)) = new_os {
                    (l, r)
                } else {
                    (deact.os_l, deact.os_r)
                };
                (
                    model_l,
                    model_r,
                    model_generation,
                    resampler,
                    stream,
                    os_l,
                    os_r,
                    cab_slot,
                    model_input_mult_adj,
                    model_output_mult_adj,
                    model_cal_meta,
                )
            } else {
                let (
                    model_l,
                    model_r,
                    model_generation,
                    resampler,
                    stream,
                    model_input_mult_adj,
                    model_output_mult_adj,
                    model_cal_meta,
                ) = if staged_model_ok {
                    // `staged_model_ok` is derived from
                    // `staged_swap.model.as_ref().is_some_and(..)`, so the flag
                    // implies a staged model; a `None` here is an internal
                    // invariant violation and must not panic on the activation path.
                    let Some(m) = staged_model else {
                        debug_assert!(false, "staged_model_ok implies a staged model");
                        return Err(PluginError::Message(
                            errors::activation::STAGED_MODEL_UNAVAILABLE,
                        ));
                    };
                    (
                        m.model_l,
                        m.model_r,
                        m.generation,
                        m.new_resampler,
                        m.new_stream,
                        m.input_mult_adj,
                        m.output_mult_adj,
                        m.cal_meta,
                    )
                } else {
                    (
                        None,
                        None,
                        0,
                        new_resampler.ok_or_else(|| {
                            PluginError::Message(errors::activation::RESAMPLER_UNAVAILABLE_FRESH)
                        })?,
                        new_stream.ok_or_else(|| {
                            PluginError::Message(errors::activation::STREAM_UNAVAILABLE_FRESH)
                        })?,
                        1.0,
                        1.0,
                        crate::clap::extensions::calibration::ModelCalibrationMeta::empty(),
                    )
                };
                let cabsim_pair = if let Some(ir) = staged_ir {
                    ir
                } else {
                    new_cabsim.ok_or_else(|| {
                        PluginError::Message(errors::activation::CABSIM_UNAVAILABLE_FRESH)
                    })?
                };
                let (os_l, os_r) = new_os.ok_or_else(|| {
                    PluginError::Message(errors::activation::OVERSAMPLER_UNAVAILABLE_FRESH)
                })?;
                (
                    model_l,
                    model_r,
                    model_generation,
                    resampler,
                    stream,
                    os_l,
                    os_r,
                    cabsim_pair,
                    model_input_mult_adj,
                    model_output_mult_adj,
                    model_cal_meta,
                )
            };

            // A reinstalled cab-sim adapter must not bleed the previous host
            // session's convolution tail into the restarted timeline: the
            // deactivate/activate boundary is a timeline discontinuity (the
            // stream adapter is rebuilt fresh and the smoothers re-sync), so
            // clear the FDL and FIFO history in place. Zero-alloc — the IR and
            // the RFFT plan are preserved, so a quantum renegotiation still
            // avoids the full rebuild.
            let cab_slot = {
                let mut cab_slot = cabsim_pair;
                if let Some(ref mut cab) = cab_slot {
                    cab.reset_and_prewarm_off_rt();
                }
                cab_slot
            };
            let cab_nam_generation = (shared.cold.cab_nam_rebuild_req.load(Ordering::Acquire) >> 1)
                .wrapping_add(1)
                .max(1);
            shared
                .cold
                .cab_nam_rebuild_req
                .store(cab_nam_generation << 1, Ordering::Release);

            // Complete any outstanding split-stabilization pass of the
            // activated model, eagerly and off the audio thread: `activate()`
            // runs on the main thread (no RT deadline) and `prewarm_step` is
            // allocation-free, so one full-budget call finishes every family
            // (LSTM `usize`, WaveNet/ConvNet one-shot, A2/Linear RF). The
            // two-phase reset latch is a callback-window microstate — never
            // persisted in `DeactivatedDspState` — so a deactivate() that
            // interrupted the amortization window leaves the preserved model
            // internally pending; draining here restores the fresh-instance
            // invariant (bit-exact post-stabilization state) before the
            // first block runs. A converged model (fresh loader install,
            // slimmable rebuild) reports complete immediately: the check
            // alone is the whole cost. The bounded round cap only guards a
            // hypothetical non-converging family against an unbounded loop.
            drain_model_prewarm_off_rt(&mut model_l, "model");
            drain_model_prewarm_off_rt(&mut model_r, "model_r");
            if let Some(ref mut model) = model_l {
                model.inject_rt_status(Arc::clone(&shared.cold.rt_status));
            }
            if let Some(ref mut model) = model_r {
                model.inject_rt_status(Arc::clone(&shared.cold.rt_status));
            }

            let silence_hyst = DynamicHysteresis::new();
            let mono_hyst = DynamicHysteresis::new();

            // Adaptive prewarm seed: measure the installed model's per-sample
            // inference cost here, off the audio thread (allocations allowed),
            // so the first amortization window after activation is sized from
            // real hardware instead of the conservative family table. The
            // frozen-table latch is set in debug builds (the measured cost there
            // is not release-representative). Continuous model swaps update
            // this scalar from the load payload.
            let prewarm_static = !prewarm_policy::ADAPTIVE_COST_ENABLED;
            let prewarm_cost_ns_per_sample = if prewarm_static {
                None
            } else {
                // Cap the calibration block at the negotiated host quantum so
                // it never exceeds the model's `max_buffer_size` (a smaller
                // block is otherwise silently truncated by composed models and
                // under-measures the per-sample cost).
                let measure_frames =
                    prewarm_policy::PREWARM_MEASURE_FRAMES.min((host_buffer as usize).max(1));
                model_l
                    .as_mut()
                    .map(|m| m.measure_cost_ns_per_sample(measure_frames))
            };

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
            let cal_config = shared.calibration_config();
            let cab_bypass =
                bypass_u32_to_bool(shared.ui_to_rt.param_cab_bypass.load(Ordering::Relaxed));
            let gate_mode = shared.gate_mode();
            let musical_gate = crate::clap::processor::dsp::musical_gate::MusicalNoiseGate::new(
                audio_config.sample_rate.round().clamp(1.0, u32::MAX as f64) as u32,
            );
            let has_cal_meta = model_cal_meta.input_level_dbu.is_some()
                || model_cal_meta.output_level_dbu.is_some()
                || model_cal_meta.loudness.is_some();
            let (model_input_mult_adj, model_output_mult_adj) = if has_cal_meta {
                crate::clap::extensions::calibration::compute_calibration_multipliers(
                    model_cal_meta,
                    cal_config,
                )
            } else {
                (model_input_mult_adj, model_output_mult_adj)
            };
            let smoother_cal_in =
                ParamSmoother::new(model_input_mult_adj, audio_config.sample_rate as f32, 20.0);
            let smoother_cal_out =
                ParamSmoother::new(model_output_mult_adj, audio_config.sample_rate as f32, 20.0);

            // Build the full RT parameter snapshot from the UiToRt atomics
            // BEFORE constructing the RT processor. This guarantees that
            // `self.params` starts in sync with every published GUI value
            // (gains, gate, bypass, adaptive compute, slim override,
            // activation) instead of a partially defaulted state that would
            // only converge on the first `process_events` generation check.
            let mut params = shared.processing_params_from_atomics();
            params.oversample = os_factor;
            debug_assert!(
                (smoother_in.current_value() - gain_lut.db_to_linear(params.input_gain_db)).abs()
                    < f32::EPSILON * 10.0,
                "Invariant: smoother_in must start from the same input_gain_db atomics"
            );
            debug_assert!(
                (smoother_out.current_value() - gain_lut.db_to_linear(params.output_gain_db)).abs()
                    < f32::EPSILON * 10.0,
                "Invariant: smoother_out must start from the same output_gain_db atomics"
            );

            // 5. Report initial latency to shared state.
            // The streaming adapter owns the resampler and zero-primes exactly
            // `latency_samples()` host samples, so its declared latency is the
            // authoritative PDC value.
            let mut initial_latency = stream.latency_samples();
            initial_latency += os_l.latency_samples() as u32;
            let cab_latency = cab_slot.as_ref().map_or(0, CabSlotDsp::latency_samples);
            initial_latency += cab_latency;
            // Publish the installed latency contributions so the main thread
            // can decide continuous-vs-staged swaps (same latency ⇒ continuous,
            // different ⇒ staged + restart) for future model/IR loads. Written
            // at the exact instant the resources land.
            shared
                .cold
                .current_stream_latency
                .store(stream.latency_samples(), Ordering::Relaxed);
            shared
                .cold
                .current_cabsim_latency
                .store(cab_latency, Ordering::Relaxed);
            // Publish the CabSim tail telemetry on the restart install path
            // too — `cold_load_cabsim` only covers the continuous SPSC swap,
            // leaving the atomic at 0 (and the host tail extension wrong) after
            // a staged IR lands via `activate()`.
            shared.rt_to_ui.cabsim_tail_samples.store(
                cab_slot.as_ref().map_or(0, CabSlotDsp::tail_samples),
                Ordering::Relaxed,
            );
            shared
                .rt_to_ui
                .current_latency
                .store(initial_latency, Ordering::Relaxed);
            shared.cold.sample_rate.store(host_rate, Ordering::Relaxed);
            neural_amp_modeler_rs::common::diagnostics::ACTIVE_SAMPLE_RATE
                .store(host_rate, Ordering::Relaxed);
            shared
                .cold
                .buffer_size
                .store(audio_config.max_frames_count, Ordering::Relaxed);

            // Flush any model deferred by load_model() (state-restore-before-activate).
            // Now that sample_rate and buffer_size are published, build resampler
            // and streaming buffer for the exact newly negotiated audio config.
            main_thread.flush_pending_model()?;

            // Defuse the rollback guard — transfers SPSC channel
            // ownership back for processor construction. Guard Drop is now a no-op.
            let channels = rollback.defuse()?;

            // CLAP 1.2.2 C ABI (`clap/ext/latency.h`): `clap_host_latency::changed()`
            // is ONLY legal inside `clap_plugin::activate()`. Sync `last_reported_latency`
            // first so reentrant `PluginLatency::get()` already observes `initial_latency`,
            // then notify the host if the activated latency changed.
            main_thread.is_active.set(true);
            main_thread.latency_restart_requested_for.set(None);
            let prev_reported_latency = main_thread.last_reported_latency.replace(initial_latency);
            if initial_latency != prev_reported_latency {
                log::info!(
                    "[Activate] Latency updated: {} -> {} samples reported to host.",
                    prev_reported_latency,
                    initial_latency
                );
                if let Some(latency_ext) = main_thread
                    .host
                    .get_extension::<clack_extensions::latency::HostLatency>()
                {
                    latency_ext.changed(&main_thread.host);
                }
            }

            // The engine structural-swap scheduler owns the command consumer
            // end-to-end: the 3-phase drain (deferred resolution, bounded
            // coalescing, budgeted apply-or-park) runs on this single ring
            // with the consumer's historical tuning (CMD_SWAP_TUNABLES).
            let cmd_drain = RtSwapDrain::new(
                CommandConsumer::new(channels.param_rx, &shared.cold.cmd_last_ack),
                CMD_SWAP_TUNABLES,
            );
            let slimmable_drain = RtSwapDrain::new(channels.slimmable_rx, SLIMMABLE_SWAP_TUNABLES);

            // Pre-allocate the circular dry delay line sized for the maximum
            // possible DSP latency (cab-sim partition == host block size, plus
            // the resampler/oversampler group-delay headroom). Its delay tracks
            // `initial_latency` and is kept in sync whenever a
            // latency-affecting resource is swapped (recompute_effective_latency).
            // The two engine `DelayLine<f32>` rings are allocated here in
            // `activate()` — the plugin's single documented allocation site.
            let dry_delay_capacity = (audio_config.max_frames_count as usize)
                .max(MAX_RESAMP_BUF)
                .saturating_add(DRY_DELAY_MAX_EXTRA);
            let dry_delay = DryDelayLine::new(dry_delay_capacity, initial_latency as usize);
            let cab_dry_delay = DryDelayLine::new(dry_delay_capacity, cab_latency as usize);

            let recovered_commands = std::mem::take(
                &mut *shared
                    .cold
                    .recovered_commands
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()),
            );

            // Reactivation can reuse a model preserved from the previous
            // activation without emitting a fresh model install, so the
            // adaptive FSM must be re-registered here as well; otherwise the
            // preserved slimmable WaveNet would never publish rebuild requests
            // (T7.3). The user-selected adaptive mode (not the conservative
            // default) seeds the controller on the same snapshot the processor
            // receives.
            let mut adaptive_compute = AdaptiveCompute::new(params.adaptive_compute);
            crate::clap::processor::dsp::install::on_model_installed(
                model_l.as_deref(),
                &mut adaptive_compute,
            );

            let realtime_activation = params.activation_precision;
            let effective_activation = crate::clap::processor::effective_activation(
                shared.cold.render_mode.load(Ordering::Acquire),
                realtime_activation,
            );
            params.activation_precision = effective_activation;
            neural_amp_modeler_rs::math::activations::set_activation_tls(effective_activation);

            // Resolved once on the main thread: the RT tail notification reuses this
            // handle instead of querying the host's extension table on the audio thread.
            let host_tail = host.get_extension::<HostTail>();

            Ok(NamClapProcessor {
                model_l,
                model_r,
                model_generation,
                cab_slot,
                cab_bypass,
                cab_needs_reset: false,
                cab_nam_dirty: false,
                cab_nam_generation,
                cab_nam_rebuild_in_flight: false,
                cab_bypass_xfade: BypassCrossfader::new(cab_bypass),
                cab_dry_delay,
                recovered_commands,
                resampler,
                stream,
                os_l,
                os_r,
                params,
                applied_os_factor: os_factor,
                buf_host_l,
                buf_host_r,
                buf_out_l,
                buf_out_r,
                buf_os_in_l,
                buf_os_in_r,
                buf_os_model_l,
                buf_os_model_r,
                buf_xfade_dry_l,
                buf_xfade_dry_r,
                buf_cab_dry_l,
                buf_cab_dry_r,
                buf_cab_nam_in_l,
                buf_cab_nam_in_r,
                buf_cab_zero_l,
                buf_cab_zero_r,
                buf_xfd_scratch_l,
                buf_xfd_scratch_r,
                silence_hyst,
                mono_hyst,
                process_mono: true,
                scheduled_events: Vec::with_capacity(4096),
                scheduled_events_sorted: true,
                bypass_xfade: BypassCrossfader::new(params.bypass),
                dry_delay,
                rt_status: Arc::clone(&shared.cold.rt_status),
                adaptive_compute,
                shared,
                smoother_in,
                smoother_out,
                smoother_cal_in,
                smoother_cal_out,
                cal_config,
                model_cal_meta,
                model_input_mult_adj,
                model_output_mult_adj,
                in_rms_ewma_l: 0.0,
                in_rms_ewma_r: 0.0,
                out_rms_ewma_l: 0.0,
                out_rms_ewma_r: 0.0,
                cmd_drain,
                gc_tx: channels.gc_tx,
                box_gc_tx: channels.box_gc_tx,
                slimmable_drain,
                gc_overflow: Arc::clone(&shared.cold.gc_overflow),
                parking_lot: Default::default(),
                parking_lot_dirty: std::sync::atomic::AtomicBool::new(false),
                box_parking_lot: Default::default(),
                box_parking_lot_dirty: std::sync::atomic::AtomicBool::new(false),
                mod_input_gain: 0.0,
                mod_output_gain: 0.0,
                mod_gate_thresh: 0.0,
                cached_threshold_open_sq: 0.0,
                cached_threshold_close_sq: 0.0,
                cached_gate_params: GateParams::default(),
                gate_dirty: true,
                gate_mode,
                musical_gate,
                rt_thread_setup_done: false,
                // Fresh models arrive converged from the off-RT loader and the
                // eager drain above finishes any deactivate-interrupted pass,
                // so activation never arms the amortization window.
                prewarm_pending: false,
                prewarm_cost_ns_per_sample,
                prewarm_static,
                prewarm_zero_budget_count: 0,
                last_seen_generation: 0,
                max_frames_count: audio_config.max_frames_count as usize,
                min_frames_count: audio_config.min_frames_count as usize,
                sample_rate: audio_config.sample_rate,
                last_render_mode: 0,
                realtime_activation,
                gain_lut: get_gain_lut(),
                cached_effective_latency: initial_latency,
                host,
                host_tail,
                poisoned: false,
                dsp_load_ewma: 0.0,
                cached_n_samples: 0,
                cached_sample_rate: 0,
                cached_inv_budget_permille: 0.0,
                cached_ewma_alpha: 0.0,
                cached_rms_samples: 0,
                cached_rms_sample_rate: 0,
                cached_rms_ewma_alpha: 0.0,
                cached_threshold_ns: u64::MAX,
                cached_budget_us: 0,
            })
        }));
        shared
            .cold
            .rt_status
            .clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_PROCESSOR_POISONED);
        match result {
            Ok(r) => r,
            Err(err) => Err(panic_to_error(err)),
        }
    }
}
