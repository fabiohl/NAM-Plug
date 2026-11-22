// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Concurrency Safety — Monotonic `model_generation` rejects stale slimmable rebuilds.
//!
//! Deterministic concurrency test: a slimmable rebuild delivered after its
//! source model was swapped out (stale generation) must be discarded straight
//! to the GC without touching the active DSP state, while a rebuild matching
//! the active generation installs normally and retires the prior model.

#[cfg(test)]
mod tests {
    use crate::clap::plugin::SlimmableRebuild;
    use crate::clap::test_util::{self, MonoTestBuffers, TestHost};
    use clack_host::prelude::*;
    use neural_amp_modeler_rs::common::spsc::GcItem;
    use neural_amp_modeler_rs::loader::nam_json::model::LinearImplementation;
    use neural_amp_modeler_rs::models::StaticModel;
    use neural_amp_modeler_rs::models::linear::LinearModel;
    use std::sync::atomic::Ordering;

    const N: usize = 512;

    fn audio_config() -> PluginAudioConfiguration {
        PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 1,
            max_frames_count: N as u32,
        }
    }

    fn main_thread_ptr(
        instance: &mut PluginInstance<TestHost>,
    ) -> *const crate::clap::plugin::NamClapMainThread<'static> {
        let raw_ptr = instance.plugin_handle().as_raw_ptr();
        unsafe {
            clack_plugin::extensions::wrapper::PluginWrapper::<crate::clap::NamClapPlugin>::handle(
                raw_ptr,
                |w| Ok(w.main_thread() as *const crate::clap::plugin::NamClapMainThread<'static>),
            )
            .unwrap()
        }
    }

    fn activate_plugin(
        plugin_instance: &mut PluginInstance<TestHost>,
    ) -> (
        StartedPluginAudioProcessor<TestHost>,
        *const crate::clap::plugin::NamClapShared,
        *const crate::clap::plugin::NamClapMainThread<'static>,
    ) {
        let stopped = plugin_instance.activate(|_, _| (), audio_config()).unwrap();
        let started = stopped.start_processing().unwrap();
        let shared_ptr = test_util::extract_shared(plugin_instance);
        let main_thread_ptr = main_thread_ptr(plugin_instance);
        (started, shared_ptr, main_thread_ptr)
    }

    fn process_block(
        started: &mut StartedPluginAudioProcessor<TestHost>,
        bufs: &mut MonoTestBuffers,
    ) -> ProcessStatus {
        let mut input_channels = [bufs.in_buf.as_mut_slice()];
        let input_audio = bufs.input_ports.with_input_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(
                input_channels.iter_mut().map(InputChannel::constant),
            ),
        }]);
        let output_channels = [bufs.out_buf.as_mut_slice()];
        let mut output_audio = bufs.output_ports.with_output_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
        }]);
        let input_events = InputEvents::empty();
        let mut output_events = OutputEvents::from_buffer(&mut bufs.output_events_buffer);
        started
            .process(
                &input_audio,
                &mut output_audio,
                &input_events,
                &mut output_events,
                None,
                None,
            )
            .unwrap()
    }

    /// Builds a trivial `StaticModel::Linear` delivery payload, distinguishable
    /// from the active WaveNet model so the GC contents identify what was
    /// discarded/retired.
    fn make_linear_model(bias: f32) -> Box<StaticModel> {
        let model = LinearModel::new(vec![bias, 0.1, 0.05], 0.0, LinearImplementation::Direct)
            .expect("trivial linear model must build");
        Box::new(StaticModel::Linear(Box::new(model)))
    }

    /// Linear delivery distinguishable by its bias.
    fn make_linear_bias(bias: f32) -> Box<StaticModel> {
        let model = LinearModel::new(vec![0.1, 0.1, 0.1], bias, LinearImplementation::Direct)
            .expect("trivial linear model must build");
        Box::new(StaticModel::Linear(Box::new(model)))
    }

    #[test]
    fn test_stale_rebuild_discarded_and_valid_applied() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        // Load a real WaveNet model through the pre-activate restore path: this
        // allocates model generation 1 and stages the model for install at
        // activate() → flush_pending_model().
        let model_path = test_util::model_path("wavenet_a1_standard.nam");
        assert!(model_path.exists(), "wavenet fixture must exist");
        let params = test_util::make_default_params(Some(model_path));
        test_util::load_plugin_state(&mut plugin_instance, &params);
        plugin_instance.call_on_main_thread_callback();

        let (mut started, shared_ptr, main_thread_ptr) = activate_plugin(&mut plugin_instance);
        let shared = unsafe { &*shared_ptr };

        let mut bufs = MonoTestBuffers::new(N, 0.0);
        // Warm-up: drain the deferred LoadModel (gen 1) so the processor's
        // active model generation converges to the allocated generation.
        for _ in 0..8 {
            process_block(&mut started, &mut bufs);
        }

        let active_gen = shared.cold.model_generation.load(Ordering::Relaxed);
        assert!(
            active_gen > 0,
            "a model generation must have been allocated"
        );
        assert_eq!(
            shared
                .cold
                .slimmable_stale_discarded_total
                .load(Ordering::Relaxed),
            0,
            "no rebuild should have been discarded yet"
        );

        // ── Phase 1: stale delivery (older generation) must be discarded ──
        let stale_gen = active_gen.wrapping_sub(1); // guaranteed != active_gen
        {
            let mt = unsafe { &*main_thread_ptr };
            mt.slimmable_tx
                .borrow_mut()
                .push(Box::new(SlimmableRebuild {
                    request_id: 0,
                    model_r: None,
                    generation: stale_gen,
                    model: Some(make_linear_model(0.5)),
                }))
                .expect("slimmable push must succeed");
        }
        process_block(&mut started, &mut bufs);
        assert_eq!(
            shared
                .cold
                .slimmable_stale_discarded_total
                .load(Ordering::Relaxed),
            1,
            "the stale rebuild must be counted as discarded"
        );

        // The discarded item is the stale Linear delivery. If the fix regressed,
        // the stale Linear would have replaced the active WaveNet and the
        // WaveNet would be the one retired to GC instead.
        {
            let mt = unsafe { &*main_thread_ptr };
            let mut saw_linear = false;
            let mut saw_other = false;
            while let Ok(item) = mt.gc_rx.borrow_mut().pop() {
                if let GcItem::Model(m) = item {
                    match m.as_ref() {
                        StaticModel::Linear(_) => saw_linear = true,
                        _ => saw_other = true,
                    }
                }
            }
            assert!(
                saw_linear && !saw_other,
                "stale delivery must be discarded (Linear in GC) and the active model retained (no WaveNet/LSTM in GC)"
            );
        }

        // ── Phase 2: matching delivery must install and retire the old model ──
        {
            let mt = unsafe { &*main_thread_ptr };
            mt.slimmable_tx
                .borrow_mut()
                .push(Box::new(SlimmableRebuild {
                    request_id: 0,
                    model_r: None,
                    generation: active_gen,
                    model: Some(make_linear_model(0.9)),
                }))
                .expect("slimmable push must succeed");
        }
        process_block(&mut started, &mut bufs);
        assert_eq!(
            shared
                .cold
                .slimmable_stale_discarded_total
                .load(Ordering::Relaxed),
            1,
            "a valid rebuild must not be counted as stale"
        );

        // The retired model (the previously-active WaveNet) reaches the GC, and
        // nothing is discarded — proving the valid path still installs.
        {
            let mt = unsafe { &*main_thread_ptr };
            let mut saw_linear = false;
            let mut saw_other = false;
            while let Ok(item) = mt.gc_rx.borrow_mut().pop() {
                if let GcItem::Model(m) = item {
                    match m.as_ref() {
                        StaticModel::Linear(_) => saw_linear = true,
                        _ => saw_other = true,
                    }
                }
            }
            assert!(
                saw_other && !saw_linear,
                "valid delivery must install and retire the prior model (WaveNet in GC), not be discarded (no Linear in GC)"
            );
        }
    }

    /// FSM parity: the slimmable request published by the audio thread must
    /// carry the channel target derived from the real model geometry and the
    /// engine FSM state, plus the active model generation — never a fixed
    /// constant or a stale generation.
    ///
    /// The expected channel count is derived from the fixture's `ch()` and
    /// from an engine `AdaptiveCompute` mirror driven with the same overload
    /// sequence, so no numeric target is hard-coded here.
    #[test]
    fn test_slimmable_request_matches_model_geometry_and_fsm_state() {
        use crate::clap::processor::{
            TEST_TELEMETRY_MUTEX, TEST_TELEMETRY_STALL_NS, TEST_TELEMETRY_STALL_TARGET_INSTANCE,
        };
        use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
        use neural_amp_modeler_rs::common::params::AdaptiveComputeMode;
        use neural_amp_modeler_rs::common::spsc::{
            RT_STATUS_DEGRADE_MINIMAL, RT_STATUS_NEEDS_SLIMMABLE_REBUILD, RtStatusFlags,
        };
        use neural_amp_modeler_rs::dsp::adaptive::AdaptiveCompute;
        use neural_amp_modeler_rs::loader::LoadOptions;
        use neural_amp_modeler_rs::loader::build::load_and_build_model;

        let _hook_guard = crate::clap::processor::processor_poisoning_test::TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _mutex_guard = TEST_TELEMETRY_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        TEST_TELEMETRY_STALL_NS.store(0, Ordering::Relaxed);
        TEST_TELEMETRY_STALL_TARGET_INSTANCE.store(0, Ordering::Relaxed);

        let model_path = test_util::model_path("slimmable_wavenet.nam");
        assert!(
            model_path.exists(),
            "slimmable_wavenet.nam fixture missing — the FSM parity test requires it"
        );

        // Full-quality channel count straight from the model geometry.
        let full_ch = {
            let sys = SystemSnapshot::capture();
            let pair = load_and_build_model(&model_path, &sys, false, LoadOptions::default())
                .expect("slimmable_wavenet.nam must load");
            let model = pair.model_l.expect("fixture must build a left model");
            match model.as_ref() {
                StaticModel::WavenetDyn(w) => w.ch(),
                _ => panic!("slimmable_wavenet.nam must resolve to StaticModel::WavenetDyn"),
            }
        };

        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let mut params = test_util::make_default_params(Some(model_path));
        // The restore carries the effective processing params; the adaptive
        // engine must be Conservative (test default) for degradation to arm.
        params.adaptive_compute =
            neural_amp_modeler_rs::common::params::AdaptiveComputeMode::Conservative;
        test_util::load_plugin_state(&mut plugin_instance, &params);
        plugin_instance.call_on_main_thread_callback();

        let (mut started, shared_ptr, _) = activate_plugin(&mut plugin_instance);
        let shared = unsafe { &*shared_ptr };
        TEST_TELEMETRY_STALL_TARGET_INSTANCE.store(shared.cold.instance_id, Ordering::Relaxed);
        let rt_status = &shared.cold.rt_status;

        let mut bufs = MonoTestBuffers::new(N, 0.01);
        for _ in 0..8 {
            let _ = process_block(&mut started, &mut bufs);
        }
        let active_generation = shared.cold.model_generation.load(Ordering::Relaxed);
        assert!(
            active_generation > 0,
            "a model generation must have been allocated"
        );

        // Drive sustained overload through the real callback path until the
        // FSM reaches its floor (Minimal). The floor is independent of how
        // slowly the warm-up blocks ran on a loaded host, so the assertion
        // does not depend on a pristine Full start.
        const MAX_STALL_BLOCKS: usize = 16;
        let budget_ns = (N as u64 * 1_000_000_000) / 48_000;
        let budget_us = budget_ns / 1_000;
        let stall_ns = budget_ns * 2;
        for _ in 0..MAX_STALL_BLOCKS {
            if rt_status.check_flag(RT_STATUS_DEGRADE_MINIMAL) {
                break;
            }
            TEST_TELEMETRY_STALL_NS.store(stall_ns, Ordering::Relaxed);
            let _ = process_block(&mut started, &mut bufs);
        }
        assert_eq!(
            TEST_TELEMETRY_STALL_NS.load(Ordering::Relaxed),
            0,
            "the stall hook is one-shot"
        );
        assert!(
            rt_status.check_flag(RT_STATUS_DEGRADE_MINIMAL),
            "the FSM must reach Minimal under sustained overload within {MAX_STALL_BLOCKS} blocks"
        );

        // The request is published by the next callback after the transition,
        // because telemetry (which drives the FSM) runs after event draining.
        let _ = process_block(&mut started, &mut bufs);

        assert!(
            rt_status.check_flag(RT_STATUS_NEEDS_SLIMMABLE_REBUILD),
            "a slimmable rebuild must be requested once the FSM leaves Full"
        );
        assert_eq!(
            shared
                .cold
                .requested_slimmable_generation
                .load(Ordering::Relaxed),
            active_generation,
            "the request must carry the active model generation"
        );

        // Engine FSM mirror: same mode and registration, driven to the same
        // floor state, so its ratio is the oracle for the expected target.
        let mut mirror = AdaptiveCompute::new(AdaptiveComputeMode::Conservative);
        mirror.set_wavenet_full_ch_rt(full_ch, true);
        let mirror_flags = RtStatusFlags::new();
        for _ in 0..MAX_STALL_BLOCKS {
            if mirror.state() == neural_amp_modeler_rs::dsp::adaptive::AdaptiveState::Minimal {
                break;
            }
            mirror.update(budget_us * 2, budget_us, 48_000, &mirror_flags);
        }
        assert_eq!(
            mirror.state(),
            neural_amp_modeler_rs::dsp::adaptive::AdaptiveState::Minimal,
            "the mirror FSM must reproduce the Minimal state"
        );
        let expected_ch = (((full_ch as f32) * mirror.wavenet_slimmable_ratio()).round() as usize)
            .clamp(1, full_ch);
        let requested_ch = rt_status.requested_slimmable_ch.load(Ordering::Relaxed) as usize;
        assert_eq!(
            requested_ch, expected_ch,
            "requested channels must equal round(full_ch * fsm ratio) derived from the model"
        );

        TEST_TELEMETRY_STALL_TARGET_INSTANCE.store(0, Ordering::Relaxed);
        TEST_TELEMETRY_STALL_NS.store(0, Ordering::Relaxed);
    }

    /// Raw pointer to the live RT processor (kept alive by the started handle).
    fn audio_processor_ptr(
        plugin_instance: &mut PluginInstance<TestHost>,
    ) -> *mut crate::clap::processor::NamClapProcessor<'static> {
        let raw_ptr = plugin_instance.plugin_handle().as_raw_ptr();
        unsafe {
            clack_plugin::extensions::wrapper::PluginWrapper::<crate::clap::NamClapPlugin>::handle(
                raw_ptr,
                |wrapper| {
                    Ok(wrapper
                        .audio_processor()
                        .expect("processor must be active")
                        .as_ptr())
                },
            )
            .expect("plugin wrapper handle")
        }
    }

    /// Builds an atomic restore carrying (or clearing) the neural model.
    fn make_model_restore(
        shared: &crate::clap::plugin::NamClapShared,
        model_name: Option<&str>,
        generation: u64,
    ) -> crate::clap::plugin::RestoreTxn {
        use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
        use neural_amp_modeler_rs::loader::LoadOptions;
        use neural_amp_modeler_rs::loader::build::load_and_build_model;
        use neural_amp_modeler_rs::models::NamModel;

        let host_rate = shared.cold.sample_rate.load(Ordering::Relaxed).max(48_000);
        let model_rate = shared
            .cold
            .model_sample_rate
            .load(Ordering::Relaxed)
            .max(48_000);

        let (model_l, input_mult_adj, output_mult_adj) = match model_name {
            Some(name) => {
                let path = test_util::model_path(name);
                let sys = SystemSnapshot::capture();
                let pair = load_and_build_model(&path, &sys, false, LoadOptions::default())
                    .unwrap_or_else(|e| panic!("failed to load {name}: {e:?}"));
                let mut model_l = pair.model_l;
                if let Some(model) = model_l.as_mut() {
                    model
                        .set_max_buffer_size(N)
                        .expect("model pre-size must succeed");
                }
                (model_l, pair.input_mult_adj, pair.output_mult_adj)
            }
            None => (None, 1.0, 1.0),
        };

        crate::clap::plugin::RestoreTxn {
            generation,
            model: Some(crate::clap::plugin::LoadModelPayload {
                generation,
                model_l,
                model_r: None,
                new_resampler: Box::new(
                    neural_amp_modeler_rs::dsp::resampler::NamResampler::new_simple_typed(
                        host_rate, model_rate,
                    )
                    .expect("resampler"),
                ),
                new_stream: crate::clap::plugin::build_stream_adapter(host_rate, model_rate, N)
                    .expect("streaming adapter"),
                model_rate,
                input_mult_adj,
                output_mult_adj,
                cal_meta: crate::clap::extensions::calibration::ModelCalibrationMeta::empty(),
                prewarm_cost_ns: None,
            }),
            ir: None,
            params: neural_amp_modeler_rs::common::params::RtProcessingParams::default()
                .with_adaptive_compute(
                    neural_amp_modeler_rs::common::params::AdaptiveComputeMode::Conservative,
                ),
            calibration:
                crate::clap::extensions::calibration::CalibrationConfig::new_project_default(),
            gate_mode: Default::default(),
            cab_bypass: false,
        }
    }

    /// F-07/D2: installing a non-slimmable model forgets the previous WaveNet
    /// topology, cancels any pending slimmable request, and stops the FSM from
    /// publishing channel requests derived from the old model.
    #[test]
    fn test_model_swap_clears_slimmable_registration_and_pending_request() {
        use crate::clap::plugin::ClapParamPayload;
        use crate::clap::processor::{
            TEST_TELEMETRY_MUTEX, TEST_TELEMETRY_STALL_NS, TEST_TELEMETRY_STALL_TARGET_INSTANCE,
        };
        use neural_amp_modeler_rs::common::spsc::{
            RT_STATUS_DEGRADE_MINIMAL, RT_STATUS_NEEDS_SLIMMABLE_REBUILD,
        };

        let _hook_guard = crate::clap::processor::processor_poisoning_test::TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _mutex_guard = TEST_TELEMETRY_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        TEST_TELEMETRY_STALL_NS.store(0, Ordering::Relaxed);
        TEST_TELEMETRY_STALL_TARGET_INSTANCE.store(0, Ordering::Relaxed);

        let dyn_path = test_util::model_path("slimmable_wavenet.nam");
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let mut params = test_util::make_default_params(Some(dyn_path));
        params.adaptive_compute =
            neural_amp_modeler_rs::common::params::AdaptiveComputeMode::Conservative;
        test_util::load_plugin_state(&mut plugin_instance, &params);
        plugin_instance.call_on_main_thread_callback();

        let (mut started, shared_ptr, main_thread_ptr) = activate_plugin(&mut plugin_instance);
        let shared = unsafe { &*shared_ptr };
        let mt = unsafe { &*main_thread_ptr };
        TEST_TELEMETRY_STALL_TARGET_INSTANCE.store(shared.cold.instance_id, Ordering::Relaxed);

        let mut bufs = MonoTestBuffers::new(N, 0.0);
        for _ in 0..8 {
            let _ = process_block(&mut started, &mut bufs);
        }

        let full_ch = {
            let proc = unsafe { &mut *audio_processor_ptr(&mut plugin_instance) };
            proc.adaptive_compute
                .wavenet_current_ch()
                .expect("the loaded slimmable WaveNet must be registered")
        };

        // A request is pending across the model swap.
        let generation = shared.cold.model_generation.load(Ordering::Relaxed);
        let pending = shared.cold.publish_slimmable_request(2, generation, false);
        assert_eq!(shared.cold.pending_slimmable_request(), pending);

        // Swap to an LSTM while the request is pending.
        let next_generation = generation.wrapping_add(1);
        mt.cmd_producer
            .borrow_mut()
            .push_command(ClapParamPayload::RestoreTxn(make_model_restore(
                shared,
                Some("lstm.nam"),
                next_generation,
            )))
            .expect("LSTM restore push must succeed");
        let _ = process_block(&mut started, &mut bufs);

        {
            let proc = unsafe { &*audio_processor_ptr(&mut plugin_instance) };
            assert_eq!(
                proc.adaptive_compute.wavenet_current_ch(),
                None,
                "an LSTM install must forget the previous WaveNet registration"
            );
            assert_eq!(proc.adaptive_compute.wavenet_slimmable_ch_target(), None);
        }
        assert_eq!(
            shared.cold.pending_slimmable_request(),
            0,
            "the model swap must cancel the pending request"
        );
        assert!(
            !shared
                .cold
                .rt_status
                .check_flag(RT_STATUS_NEEDS_SLIMMABLE_REBUILD)
        );

        // Overload must not resurrect a request from the stale topology.
        let budget_ns = (N as u64 * 1_000_000_000) / 48_000;
        let budget_us = budget_ns / 1_000;
        for _ in 0..16 {
            if shared.cold.rt_status.check_flag(RT_STATUS_DEGRADE_MINIMAL) {
                break;
            }
            TEST_TELEMETRY_STALL_NS.store(budget_ns * 2, Ordering::Relaxed);
            let _ = process_block(&mut started, &mut bufs);
        }
        let _ = budget_us;
        let _ = process_block(&mut started, &mut bufs);
        assert!(
            shared.cold.rt_status.check_flag(RT_STATUS_DEGRADE_MINIMAL),
            "the FSM must still degrade under overload after the model swap"
        );
        assert!(
            !shared
                .cold
                .rt_status
                .check_flag(RT_STATUS_NEEDS_SLIMMABLE_REBUILD),
            "no slimmable request may be published for an LSTM registration"
        );
        assert_eq!(shared.cold.pending_slimmable_request(), 0);

        // Restoring a slimmable WaveNet re-arms the registration at full width.
        let next_generation = next_generation.wrapping_add(1);
        mt.cmd_producer
            .borrow_mut()
            .push_command(ClapParamPayload::RestoreTxn(make_model_restore(
                shared,
                Some("slimmable_wavenet.nam"),
                next_generation,
            )))
            .expect("WaveNet restore push must succeed");
        let _ = process_block(&mut started, &mut bufs);
        {
            let proc = unsafe { &*audio_processor_ptr(&mut plugin_instance) };
            assert!(
                proc.adaptive_compute
                    .wavenet_slimmable_ch_target()
                    .is_some(),
                "a new slimmable WaveNet must be registered again"
            );
            // The FSM is still Minimal after the overload, so the next block may
            // immediately publish a fresh request for the new model at its
            // derived width (never the cleared old registration).
            assert!(
                shared.cold.pending_slimmable_request() != 0
                    || proc.adaptive_compute.wavenet_current_ch().is_some()
            );
        }
        let _ = full_ch;

        // Clearing the model also clears the registration.
        let next_generation = next_generation.wrapping_add(1);
        mt.cmd_producer
            .borrow_mut()
            .push_command(ClapParamPayload::RestoreTxn(make_model_restore(
                shared,
                None,
                next_generation,
            )))
            .expect("model clear push must succeed");
        let _ = process_block(&mut started, &mut bufs);
        let proc = unsafe { &*audio_processor_ptr(&mut plugin_instance) };
        assert_eq!(proc.adaptive_compute.wavenet_current_ch(), None);

        TEST_TELEMETRY_STALL_TARGET_INSTANCE.store(0, Ordering::Relaxed);
        TEST_TELEMETRY_STALL_NS.store(0, Ordering::Relaxed);
    }
    /// T7.2: a same-generation delivery from an older request must not install
    /// over a newer request that was already published (and whose result may
    /// still be queued). The older payload is retired to the GC; the newest
    /// request installs and is acknowledged.
    #[test]
    fn test_older_request_never_installs_over_newer_request() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let model_path = test_util::model_path("wavenet_a1_standard.nam");
        assert!(model_path.exists(), "wavenet fixture must exist");
        let params = test_util::make_default_params(Some(model_path));
        test_util::load_plugin_state(&mut plugin_instance, &params);
        plugin_instance.call_on_main_thread_callback();

        let (mut started, shared_ptr, main_thread_ptr) = activate_plugin(&mut plugin_instance);
        let shared = unsafe { &*shared_ptr };
        let mut bufs = MonoTestBuffers::new(N, 0.0);
        for _ in 0..8 {
            process_block(&mut started, &mut bufs);
        }
        let active_gen = shared.cold.model_generation.load(Ordering::Relaxed);
        assert!(active_gen > 0);

        let a = shared.cold.publish_slimmable_request(4, active_gen, false);
        let b = shared.cold.publish_slimmable_request(8, active_gen, false);
        assert_ne!(a, b);
        assert_eq!(shared.cold.latest_slimmable_request(), b);

        // Older request A is delivered while B is the newest request.
        {
            let mt = unsafe { &*main_thread_ptr };
            mt.slimmable_tx
                .borrow_mut()
                .push(Box::new(SlimmableRebuild {
                    request_id: a,
                    model_r: None,
                    generation: active_gen,
                    model: Some(make_linear_bias(0.2)),
                }))
                .expect("slimmable push must succeed");
        }
        process_block(&mut started, &mut bufs);
        assert_eq!(
            shared
                .cold
                .slimmable_stale_discarded_total
                .load(Ordering::Relaxed),
            1,
            "the older request must be discarded, not installed"
        );
        {
            let mt = unsafe { &*main_thread_ptr };
            let mut saw_old_linear = false;
            while let Ok(item) = mt.gc_rx.borrow_mut().pop() {
                if let GcItem::Model(m) = item
                    && let StaticModel::Linear(l) = m.as_ref()
                    && (l.bias - 0.2).abs() < 1e-6
                {
                    saw_old_linear = true;
                }
            }
            assert!(saw_old_linear, "older request payload must reach the GC");
        }

        // Newest request B installs and resolves.
        {
            let mt = unsafe { &*main_thread_ptr };
            mt.slimmable_tx
                .borrow_mut()
                .push(Box::new(SlimmableRebuild {
                    request_id: b,
                    model_r: None,
                    generation: active_gen,
                    model: Some(make_linear_model(0.9)),
                }))
                .expect("slimmable push must succeed");
        }
        process_block(&mut started, &mut bufs);
        assert_eq!(
            shared
                .cold
                .slimmable_stale_discarded_total
                .load(Ordering::Relaxed),
            1,
            "the newest request must install without a stale discard"
        );
        assert_eq!(shared.cold.pending_slimmable_request(), 0);
        assert_eq!(shared.cold.latest_slimmable_request(), b);
    }

    /// T7.3: reactivation reuses the preserved model without a fresh install.
    /// The adaptive FSM must be re-registered from that model, otherwise the
    /// preserved slimmable WaveNet can never request another rebuild.
    #[test]
    fn test_reactivation_reregisters_preserved_slimmable_wavenet() {
        let dyn_path = test_util::model_path("slimmable_wavenet.nam");
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let mut params = test_util::make_default_params(Some(dyn_path));
        params.adaptive_compute =
            neural_amp_modeler_rs::common::params::AdaptiveComputeMode::Conservative;
        test_util::load_plugin_state(&mut plugin_instance, &params);
        plugin_instance.call_on_main_thread_callback();

        let (mut started, _shared_ptr, _main_thread_ptr) = activate_plugin(&mut plugin_instance);
        let mut bufs = MonoTestBuffers::new(N, 0.0);
        for _ in 0..8 {
            let _ = process_block(&mut started, &mut bufs);
        }
        {
            let proc = unsafe { &*audio_processor_ptr(&mut plugin_instance) };
            assert!(
                proc.adaptive_compute.wavenet_current_ch().is_some(),
                "the loaded slimmable WaveNet must be registered on first activation"
            );
        }

        let stopped = started.stop_processing();
        plugin_instance.deactivate(stopped);
        let (mut started, _shared_ptr, _main_thread_ptr) = activate_plugin(&mut plugin_instance);
        for _ in 0..8 {
            let _ = process_block(&mut started, &mut bufs);
        }
        let proc = unsafe { &*audio_processor_ptr(&mut plugin_instance) };
        assert!(
            proc.adaptive_compute.wavenet_current_ch().is_some(),
            "reactivation must re-register the preserved slimmable WaveNet"
        );

        let stopped = started.stop_processing();
        plugin_instance.deactivate(stopped);
    }

    /// T7.3: a slimmable rebuild in dual-mono must update BOTH channel models.
    /// The R slice travels in the payload and replaces `model_r`, retiring the
    /// previous R model through the GC exactly like the L channel.
    #[test]
    fn test_dual_mono_rebuild_replaces_both_channel_models() {
        use crate::clap::plugin::ClapParamPayload;

        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let model_path = test_util::model_path("wavenet_a1_standard.nam");
        assert!(model_path.exists());
        let params = test_util::make_default_params(Some(model_path));
        test_util::load_plugin_state(&mut plugin_instance, &params);
        plugin_instance.call_on_main_thread_callback();
        let (mut started, shared_ptr, main_thread_ptr) = activate_plugin(&mut plugin_instance);
        let shared = unsafe { &*shared_ptr };
        let mut bufs = MonoTestBuffers::new(N, 0.0);
        for _ in 0..8 {
            process_block(&mut started, &mut bufs);
        }
        let generation = shared.cold.model_generation.load(Ordering::Relaxed);

        // Install a dual-mono pair: L is the real model, R is an identifiable
        // linear stand-in.
        let mt = unsafe { &*main_thread_ptr };
        let mut restore = make_model_restore(
            shared,
            Some("wavenet_a1_standard.nam"),
            generation.wrapping_add(1),
        );
        restore
            .model
            .as_mut()
            .expect("restore must carry a model")
            .model_r = Some(make_linear_bias(0.5));
        mt.cmd_producer
            .borrow_mut()
            .push_command(ClapParamPayload::RestoreTxn(restore))
            .expect("dual-mono restore push must succeed");
        process_block(&mut started, &mut bufs);
        {
            let proc = unsafe { &*audio_processor_ptr(&mut plugin_instance) };
            assert!(
                matches!(
                    proc.model_r.as_deref(),
                    Some(StaticModel::Linear(l)) if (l.bias - 0.5).abs() < 1e-6
                ),
                "the restore must install the R model"
            );
        }
        let generation = unsafe { &*audio_processor_ptr(&mut plugin_instance) }.model_generation;

        // A rebuild for both channels replaces R as well.
        let request = shared.cold.publish_slimmable_request(4, generation, true);
        mt.slimmable_tx
            .borrow_mut()
            .push(Box::new(SlimmableRebuild {
                request_id: request,
                model_r: Some(make_linear_bias(0.7)),
                generation,
                model: Some(make_linear_bias(0.1)),
            }))
            .expect("slimmable push must succeed");
        process_block(&mut started, &mut bufs);
        {
            let proc = unsafe { &*audio_processor_ptr(&mut plugin_instance) };
            assert!(
                matches!(
                    proc.model_r.as_deref(),
                    Some(StaticModel::Linear(l)) if (l.bias - 0.7).abs() < 1e-6
                ),
                "the R channel must be swapped by the rebuild"
            );
        }
        {
            let mut retired_old_r = false;
            while let Ok(item) = mt.gc_rx.borrow_mut().pop() {
                if let GcItem::Model(m) = item
                    && let StaticModel::Linear(l) = m.as_ref()
                    && (l.bias - 0.5).abs() < 1e-6
                {
                    retired_old_r = true;
                }
            }
            assert!(
                retired_old_r,
                "the previous R model must be retired to the GC"
            );
        }
        let stopped = started.stop_processing();
        plugin_instance.deactivate(stopped);
    }
}
