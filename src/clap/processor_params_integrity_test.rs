// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Parameter-integrity and lifecycle regressions (S2).
//!
//! Covers the restart latch (GUI/SPSC/flush paths), full parameter seeding on
//! reactivation, main-thread flush snapshots, scheduled gate-mode resets,
//! offline activation determinism and the slimmable install guard.

#[cfg(test)]
mod tests {
    use crate::clap::extensions::gate_mode::GateMode;
    use crate::clap::extensions::params::{
        PARAM_GATE_MODE, PARAM_GATE_THRESH, PARAM_INPUT_GAIN, PARAM_OVERSAMPLE,
    };
    use crate::clap::host_harness::{
        self, CompleteHost, CompleteHostState, make_harness_audio_processor,
        make_test_plugin_with_harness, process_block_harness,
    };
    use crate::clap::plugin::{
        ClapParamPayload, NamClapMainThread, RENDER_MODE_OFFLINE, RENDER_MODE_REALTIME,
        SlimmableRebuild,
    };
    use crate::clap::processor::NamClapProcessor;
    use crate::clap::test_util::{self, TestHost};
    use clack_common::events::Pckn;
    use clack_common::events::event_types::ParamValueEvent;
    use clack_common::utils::ClapId;
    use clack_extensions::params::PluginMainThreadParams;
    use clack_host::prelude::*;
    use neural_amp_modeler_rs::common::params::{
        ActivationPrecision, AdaptiveComputeMode, RtProcessingParams,
    };
    use neural_amp_modeler_rs::dsp::adaptive::SlimOverride;
    use neural_amp_modeler_rs::dsp::gate::GateParams;
    use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
    use neural_amp_modeler_rs::models::StaticModel;
    use std::sync::atomic::Ordering;

    fn audio_config(frames: u32) -> PluginAudioConfiguration {
        PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: frames,
            max_frames_count: frames,
        }
    }

    fn value_event(id: u32, value: f64) -> ParamValueEvent {
        ParamValueEvent::new(0u32, ClapId::new(id), Pckn::match_all(), value)
    }

    fn process_silence(started: &mut StartedPluginAudioProcessor<CompleteHost>) {
        let n = 256;
        let mut il = vec![0.0f32; n];
        let mut ir = vec![0.0f32; n];
        let mut ol = vec![0.0f32; n];
        let mut or = vec![0.0f32; n];
        let _ = process_block_harness(started, &mut il, &mut ir, &mut ol, &mut or, None);
    }

    fn restart_count(state: &CompleteHostState) -> usize {
        state
            .snapshot()
            .iter()
            .filter(|e| matches!(e, host_harness::HostEvent::RestartRequested))
            .count()
    }

    fn audio_processor_ptr<H: HostHandlers>(
        instance: &mut PluginInstance<H>,
    ) -> *mut NamClapProcessor<'static> {
        let raw_ptr = instance.plugin_handle().as_raw_ptr();
        // SAFETY: `raw_ptr` is the live plugin handle owned by this test's instance; `PluginWrapper::handle` runs the closure synchronously while that instance is alive, and the returned raw pointer is only dereferenced while the instance remains in scope.
        unsafe {
            clack_plugin::extensions::wrapper::PluginWrapper::<crate::clap::NamClapPlugin>::handle(
                raw_ptr,
                |wrapper| Ok(wrapper.audio_processor().unwrap().as_ptr() as *mut _),
            )
            .unwrap()
        }
    }

    fn main_thread_ptr<H: HostHandlers>(
        instance: &mut PluginInstance<H>,
    ) -> *const NamClapMainThread<'static> {
        let raw_ptr = instance.plugin_handle().as_raw_ptr();
        // SAFETY: `raw_ptr` is the live plugin handle owned by this test's instance; `PluginWrapper::handle` runs the closure synchronously while that instance is alive, and the returned raw pointer is only dereferenced while the instance remains in scope.
        unsafe {
            clack_plugin::extensions::wrapper::PluginWrapper::<crate::clap::NamClapPlugin>::handle(
                raw_ptr,
                |wrapper| Ok(wrapper.main_thread() as *const _),
            )
            .unwrap()
        }
    }

    /// T2.1(a)/T2.2: an oversampling change published by the GUI generation
    /// guard (before the DSP block) must still request exactly one host
    /// restart at the callback boundary.
    #[test]
    fn gui_oversample_change_requests_restart_once() {
        let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
        let audio_config = audio_config(256);

        let stopped = instance
            .activate(|_, _| make_harness_audio_processor(&state), audio_config)
            .expect("activate");
        let mut started = stopped.start_processing().expect("start_processing");
        // SAFETY: `extract_plugin_shared`/`extract_shared` returns a pointer into the live plugin instance owned by this test; the instance is kept alive and unmoved for the borrow, access goes through atomics/interior-mutable fields, and no `&mut` to the same shared state is alive.
        let shared = unsafe { &*host_harness::extract_plugin_shared(&mut instance) };

        shared
            .ui_to_rt
            .param_oversample
            .store(OversampleFactor::X2.to_f32() as u32, Ordering::Relaxed);
        shared.bump_generation();

        process_silence(&mut started);
        assert_eq!(
            restart_count(&state),
            1,
            "GUI oversampling change must request exactly one restart"
        );

        process_silence(&mut started);
        assert_eq!(
            restart_count(&state),
            1,
            "the latch must not re-arm while the same request is pending"
        );

        drop(started);
    }

    /// T2.1(a)/T2.2: the SPSC full-parameter drain happens before the DSP
    /// block, so the request must not be masked by a same-callback comparison.
    #[test]
    fn spsc_oversample_change_requests_restart() {
        let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
        let cfg = audio_config(256);

        let stopped = instance
            .activate(|_, _| make_harness_audio_processor(&state), cfg)
            .expect("activate");
        let mut started = stopped.start_processing().expect("start_processing");
        // SAFETY: the main-thread pointer is obtained from the live plugin instance's wrapper; the instance outlives this borrow, the main thread is accessed through `&self` with interior mutability, and no `&mut` to it is alive.
        let mt = unsafe { &*main_thread_ptr(&mut instance) };

        let mut params = RtProcessingParams::default();
        params.oversample = OversampleFactor::X2;
        mt.cmd_producer.borrow_mut().push_params(params);
        mt.cmd_producer
            .borrow_mut()
            .force_flush()
            .expect("parameter push must succeed");

        process_silence(&mut started);
        assert_eq!(
            restart_count(&state),
            1,
            "SPSC oversampling change must request exactly one restart"
        );

        drop(started);
    }

    /// T2.1(a)/T2.2: `flush_inner` has no DSP block to reach the orchestrator,
    /// so it must consume the latch itself.
    #[test]
    fn audio_flush_oversample_change_requests_restart() {
        use clack_extensions::params::PluginAudioProcessorParams;

        let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
        let _stopped = instance
            .activate(
                |_, _| make_harness_audio_processor(&state),
                audio_config(256),
            )
            .expect("activate");
        // SAFETY: the processor pointer comes from the active wrapper of this live plugin instance; the processor is stopped and driven exclusively by this test, so the mutable borrow is unique and cannot outlive the instance.
        let processor = unsafe { &mut *audio_processor_ptr(&mut instance) };

        let event = value_event(PARAM_OVERSAMPLE, OversampleFactor::X2.to_f32() as f64);
        let mut event_buffer = EventBuffer::new();
        event_buffer.push(&event);
        let input_events = InputEvents::from_buffer(&event_buffer);
        let mut out_buffer = EventBuffer::new();
        let mut out_events = OutputEvents::from_buffer(&mut out_buffer);
        processor.flush(&input_events, &mut out_events);

        assert_eq!(
            restart_count(&state),
            1,
            "flush_inner must request the restart it latched"
        );
    }

    /// T2.2: after the host restart is serviced by `activate()`, the stale
    /// latch must not fire again on the next callback.
    #[test]
    fn restart_latch_does_not_resurrect_after_activate() {
        let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
        let cfg = audio_config(256);

        let stopped = instance
            .activate(|_, _| make_harness_audio_processor(&state), cfg)
            .expect("activate");
        let mut started = stopped.start_processing().expect("start_processing");
        // SAFETY: `extract_plugin_shared`/`extract_shared` returns a pointer into the live plugin instance owned by this test; the instance is kept alive and unmoved for the borrow, access goes through atomics/interior-mutable fields, and no `&mut` to the same shared state is alive.
        let shared = unsafe { &*host_harness::extract_plugin_shared(&mut instance) };

        shared
            .ui_to_rt
            .param_oversample
            .store(OversampleFactor::X2.to_f32() as u32, Ordering::Relaxed);
        shared.bump_generation();
        process_silence(&mut started);
        assert_eq!(restart_count(&state), 1);

        let mut started = host_harness::perform_restart(&mut instance, started, &state, cfg);
        state.events.lock().unwrap().clear();
        process_silence(&mut started);
        process_silence(&mut started);
        assert_eq!(
            restart_count(&state),
            0,
            "activate() must clear the latch; no restart after the host serviced it"
        );

        drop(started);
    }

    /// T2.1(b)/T2.4: reactivating without GUI interaction must preserve every
    /// parameter and seed `AdaptiveCompute` with the user mode.
    #[test]
    fn reactivation_seeds_all_parameters_from_atomics() {
        let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
        let cfg = audio_config(256);

        let stopped = instance.activate(|_, _| (), cfg).expect("first activate");
        // SAFETY: `extract_plugin_shared`/`extract_shared` returns a pointer into the live plugin instance owned by this test; the instance is kept alive and unmoved for the borrow, access goes through atomics/interior-mutable fields, and no `&mut` to the same shared state is alive.
        let shared = unsafe { &*test_util::extract_shared(&mut instance) };

        shared.ui_to_rt.param_bypass.store(1, Ordering::Relaxed);
        shared
            .ui_to_rt
            .param_gate_thresh
            .store((-50.0f32).to_bits(), Ordering::Relaxed);
        shared
            .ui_to_rt
            .param_activation
            .store(ActivationPrecision::Fast as u32, Ordering::Relaxed);
        shared
            .ui_to_rt
            .param_adaptive_compute
            .store(AdaptiveComputeMode::Aggressive as u32, Ordering::Relaxed);
        shared
            .ui_to_rt
            .param_slim_override
            .store(SlimOverride::ForceLite as u32, Ordering::Relaxed);
        shared.bump_generation();

        instance.deactivate(stopped);
        let stopped = instance.activate(|_, _| (), cfg).expect("second activate");
        // SAFETY: the processor pointer comes from the active wrapper of this live plugin instance; the processor is stopped and driven exclusively by this test, so the mutable borrow is unique and cannot outlive the instance.
        let processor = unsafe { &mut *audio_processor_ptr(&mut instance) };

        assert!(processor.params.bypass, "bypass must survive reactivation");
        assert_eq!(processor.params.gate_threshold_db, -50.0);
        assert_eq!(
            processor.params.adaptive_compute,
            AdaptiveComputeMode::Aggressive
        );
        assert_eq!(
            processor.adaptive_compute.mode(),
            processor.params.adaptive_compute,
            "AdaptiveCompute must be built with the parameter mode"
        );
        assert_eq!(processor.params.slim_override, SlimOverride::ForceLite);
        assert_eq!(
            processor.params.activation_precision,
            ActivationPrecision::Fast
        );

        drop(stopped);
    }

    /// T2.1(c)/T2.3: a main-thread flush triggered by an unrelated event must
    /// publish the GUI's newer gain, not the stale mirror.
    #[test]
    fn main_thread_flush_snapshot_contains_gui_gain() {
        let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
        // SAFETY: `extract_plugin_shared`/`extract_shared` returns a pointer into the live plugin instance owned by this test; the instance is kept alive and unmoved for the borrow, access goes through atomics/interior-mutable fields, and no `&mut` to the same shared state is alive.
        let shared = unsafe { &*test_util::extract_shared(&mut instance) };
        // SAFETY: the main-thread pointer is obtained from the live plugin instance's wrapper; the instance outlives this borrow, the main thread is accessed through `&self` with interior mutability, and no `&mut` to it is alive.
        let mt = unsafe { &*main_thread_ptr(&mut instance) };

        shared
            .ui_to_rt
            .param_input_gain
            .store(7.0f32.to_bits(), Ordering::Relaxed);

        let event = value_event(PARAM_GATE_THRESH, -40.0);
        let mut event_buffer = EventBuffer::new();
        event_buffer.push(&event);
        let input_events = InputEvents::from_buffer(&event_buffer);
        let mut out_buffer = EventBuffer::new();
        let mut out_events = OutputEvents::from_buffer(&mut out_buffer);
        mt.flush(&input_events, &mut out_events);

        let mut rx_guard = shared
            .cold
            .param_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let rx = rx_guard.as_mut().expect("param consumer must exist");
        let payload = rx.pop().expect("flush must publish one payload");
        match *payload {
            ClapParamPayload::Params(params) => assert_eq!(
                params.input_gain_db, 7.0,
                "the flushed snapshot must carry the GUI-published gain"
            ),
            _ => panic!("expected Params payload"),
        }
    }

    /// T2.1(d)/T2.6: a scheduled gate-mode event must reset `silence_hyst`
    /// exactly like the direct `set_gate_mode` path.
    #[test]
    fn scheduled_gate_mode_resets_silence_hysteresis() {
        let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
        let cfg = audio_config(256);

        let stopped = instance.activate(|_, _| (), cfg).expect("activate");
        // SAFETY: the processor pointer comes from the active wrapper of this live plugin instance; the processor is stopped and driven exclusively by this test, so the mutable borrow is unique and cannot outlive the instance.
        let processor = unsafe { &mut *audio_processor_ptr(&mut instance) };

        processor.gate_mode = GateMode::Musical;
        shared_gate_mode(&mut instance, GateMode::Musical.to_u32());

        let close_params = GateParams::new(-70.0, -80.0, 0, 0, 1e-4);
        processor
            .silence_hyst
            .update(0.0, 0.25, 0.25, &close_params, 64);
        assert_eq!(
            processor.silence_hyst.state(),
            neural_amp_modeler_rs::dsp::gate::GateState::Closed,
            "test setup must close the hysteresis"
        );
        processor.gate_dirty = false;

        let mut started = stopped.start_processing().expect("start_processing");
        send_event(
            &mut started,
            PARAM_GATE_MODE,
            GateMode::Silence.to_f32() as f64,
        );

        // SAFETY: the processor pointer comes from the active wrapper of this live plugin instance; the processor is stopped and driven exclusively by this test, so the mutable borrow is unique and cannot outlive the instance.
        let processor = unsafe { &mut *audio_processor_ptr(&mut instance) };
        assert_eq!(processor.gate_mode, GateMode::Silence);
        assert_eq!(
            processor.silence_hyst.state(),
            neural_amp_modeler_rs::dsp::gate::GateState::Open,
            "scheduled gate-mode change must reset the silence hysteresis"
        );
    }

    fn shared_gate_mode(instance: &mut PluginInstance<TestHost>, mode: u32) {
        // SAFETY: `extract_plugin_shared`/`extract_shared` returns a pointer into the live plugin instance owned by this test; the instance is kept alive and unmoved for the borrow, access goes through atomics/interior-mutable fields, and no `&mut` to the same shared state is alive.
        let shared = unsafe { &*test_util::extract_shared(instance) };
        shared
            .ui_to_rt
            .param_gate_mode
            .store(mode, Ordering::Relaxed);
    }

    fn send_event(started: &mut StartedPluginAudioProcessor<TestHost>, id: u32, value: f64) {
        let n = 256;
        let event = value_event(id, value);
        let mut event_buffer = EventBuffer::new();
        event_buffer.push(&event);
        let input_events = InputEvents::from_buffer(&event_buffer);
        let mut bufs = test_util::StereoTestBuffers::new(n, 0.0, 0.0);
        let mut input_channels = [bufs.in_l.as_mut_slice(), bufs.in_r.as_mut_slice()];
        let input_audio = bufs.input_ports.with_input_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(
                input_channels.iter_mut().map(InputChannel::constant),
            ),
        }]);
        let output_channels = [bufs.out_l.as_mut_slice(), bufs.out_r.as_mut_slice()];
        let mut output_audio = bufs.output_ports.with_output_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
        }]);
        let mut out_events = OutputEvents::from_buffer(&mut bufs.output_events_buffer);
        started
            .process(
                &input_audio,
                &mut output_audio,
                &input_events,
                &mut out_events,
                None,
                None,
            )
            .expect("process");
    }

    /// T2.1(b)/T2.5: automation during offline rendering updates only the user
    /// choice; the offline DSP stays Standard and the choice returns with RT.
    #[test]
    fn offline_automation_preserves_standard_and_restores_user_choice() {
        let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
        let cfg = audio_config(256);

        let stopped = instance.activate(|_, _| (), cfg).expect("activate");
        // SAFETY: `extract_plugin_shared`/`extract_shared` returns a pointer into the live plugin instance owned by this test; the instance is kept alive and unmoved for the borrow, access goes through atomics/interior-mutable fields, and no `&mut` to the same shared state is alive.
        let shared = unsafe { &*test_util::extract_shared(&mut instance) };

        shared
            .cold
            .render_mode
            .store(RENDER_MODE_OFFLINE, Ordering::Release);
        let mut started = stopped.start_processing().expect("start_processing");
        send_event(&mut started, PARAM_INPUT_GAIN, 0.0);

        // SAFETY: the processor pointer comes from the active wrapper of this live plugin instance; the processor is stopped and driven exclusively by this test, so the mutable borrow is unique and cannot outlive the instance.
        let processor = unsafe { &mut *audio_processor_ptr(&mut instance) };
        assert_eq!(
            processor.params.activation_precision,
            ActivationPrecision::Standard,
            "offline rendering must force Standard"
        );

        shared
            .ui_to_rt
            .param_activation
            .store(ActivationPrecision::Fast as u32, Ordering::Relaxed);
        shared.bump_generation();
        send_event(&mut started, PARAM_INPUT_GAIN, 0.0);

        // SAFETY: the processor pointer comes from the active wrapper of this live plugin instance; the processor is stopped and driven exclusively by this test, so the mutable borrow is unique and cannot outlive the instance.
        let processor = unsafe { &mut *audio_processor_ptr(&mut instance) };
        assert_eq!(
            processor.realtime_activation,
            ActivationPrecision::Fast,
            "automation must always update the user choice"
        );
        assert_eq!(
            processor.params.activation_precision,
            ActivationPrecision::Standard,
            "offline render must ignore the automation for its arithmetic"
        );

        shared
            .cold
            .render_mode
            .store(RENDER_MODE_REALTIME, Ordering::Release);
        send_event(&mut started, PARAM_INPUT_GAIN, 0.0);

        // SAFETY: the processor pointer comes from the active wrapper of this live plugin instance; the processor is stopped and driven exclusively by this test, so the mutable borrow is unique and cannot outlive the instance.
        let processor = unsafe { &mut *audio_processor_ptr(&mut instance) };
        assert_eq!(
            processor.params.activation_precision,
            ActivationPrecision::Fast,
            "returning to realtime must restore the user choice"
        );

        drop(started);
    }

    /// T2.7: a slimmable rebuild delivered while no primary model is installed
    /// must never populate `model_l`; the payload goes to GC instead.
    #[test]
    fn slimmable_rebuild_without_model_l_is_not_installed() {
        let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
        let cfg = audio_config(256);

        let stopped = instance.activate(|_, _| (), cfg).expect("activate");
        let mut started = stopped.start_processing().expect("start_processing");
        // SAFETY: the main-thread pointer is obtained from the live plugin instance's wrapper; the instance outlives this borrow, the main thread is accessed through `&self` with interior mutability, and no `&mut` to it is alive.
        let mt = unsafe { &*main_thread_ptr(&mut instance) };

        {
            let mut tx = mt.slimmable_tx.borrow_mut();
            tx.push(Box::new(SlimmableRebuild {
                request_id: 0,
                generation: 0,
                model: Some(make_linear_model()),
                model_r: None,
            }))
            .expect("slimmable push");
        }

        let mut bufs = test_util::StereoTestBuffers::new(256, 0.0, 0.0);
        send_block(&mut started, &mut bufs);

        // SAFETY: the processor pointer comes from the active wrapper of this live plugin instance; the processor is stopped and driven exclusively by this test, so the mutable borrow is unique and cannot outlive the instance.
        let processor = unsafe { &mut *audio_processor_ptr(&mut instance) };
        assert!(
            processor.model_l.is_none(),
            "a rebuild must not install without an active primary model"
        );

        drop(started);
    }

    fn make_linear_model() -> Box<StaticModel> {
        use neural_amp_modeler_rs::loader::nam_json::model::LinearImplementation;
        use neural_amp_modeler_rs::models::linear::LinearModel;
        let model = LinearModel::new(vec![0.1, 0.1, 0.1], 0.0, LinearImplementation::Direct)
            .expect("linear model");
        Box::new(StaticModel::Linear(Box::new(model)))
    }

    fn send_block(
        started: &mut StartedPluginAudioProcessor<TestHost>,
        bufs: &mut test_util::StereoTestBuffers,
    ) {
        let mut input_channels = [bufs.in_l.as_mut_slice(), bufs.in_r.as_mut_slice()];
        let input_audio = bufs.input_ports.with_input_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(
                input_channels.iter_mut().map(InputChannel::constant),
            ),
        }]);
        let output_channels = [bufs.out_l.as_mut_slice(), bufs.out_r.as_mut_slice()];
        let mut output_audio = bufs.output_ports.with_output_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
        }]);
        let input_events = InputEvents::empty();
        let mut out_events = OutputEvents::from_buffer(&mut bufs.output_events_buffer);
        started
            .process(
                &input_audio,
                &mut output_audio,
                &input_events,
                &mut out_events,
                None,
                None,
            )
            .expect("process");
    }
}
