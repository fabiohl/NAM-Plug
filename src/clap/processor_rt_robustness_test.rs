// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! RT robustness, containment and panic-latch regressions (S3).
//!
//! Covers mono engagement inside a host block (`process_mono` freshness),
//! poisoning from the audio-thread flush, the non-finite containment reset of
//! the musical gate, the absence of RT-thread re-arming on `reset()`, and the
//! cached `HostTail` extension used by the RT tail notification.

#[cfg(test)]
mod tests {
    use crate::clap::extensions::gate_mode::GateMode;
    use crate::clap::host_harness::{
        extract_plugin_main_thread, extract_plugin_shared, make_harness_audio_processor,
        make_test_plugin_with_harness,
    };
    use crate::clap::plugin::shared::CONFIG_ID_STEREO;
    use crate::clap::processor::NamClapProcessor;
    use crate::clap::processor::processor_poisoning_test::ensure_isolated_crash_dir;
    use crate::clap::test_util::{self, StereoTestBuffers, TestHost};
    use clack_common::utils::ClapId;
    use clack_extensions::audio_ports_config::PluginAudioPortsConfig;
    use clack_extensions::params::PluginAudioProcessorParams;
    use clack_host::prelude::*;
    use neural_amp_modeler_rs::common::spsc::RT_STATUS_PROCESSOR_POISONED;
    use std::f32::consts::PI;
    use std::sync::atomic::Ordering;

    const FRAMES: usize = 256;
    const SAMPLE_RATE: f32 = 48000.0;

    fn audio_config() -> PluginAudioConfiguration {
        PluginAudioConfiguration {
            sample_rate: SAMPLE_RATE as f64,
            min_frames_count: FRAMES as u32,
            max_frames_count: FRAMES as u32,
        }
    }

    fn processor_ptr<H: HostHandlers>(
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

    fn stereo_instance() -> (PluginEntry, HostInfo, PluginInstance<TestHost>) {
        let (entry, host_info, mut instance) = test_util::make_test_plugin();
        {
            let handle = instance.plugin_handle();
            let config_ext = handle
                .get_extension::<PluginAudioPortsConfig>()
                .expect("PluginAudioPortsConfig");
            config_ext
                .select(&handle, ClapId::new(CONFIG_ID_STEREO))
                .expect("select stereo");
        }
        let model_path = test_util::model_path("wavenet_a1_standard.nam");
        let params = test_util::make_default_params(Some(model_path));
        test_util::load_plugin_state(&mut instance, &params);
        instance.call_on_main_thread_callback();
        (entry, host_info, instance)
    }

    fn process_block(
        started: &mut StartedPluginAudioProcessor<TestHost>,
        bufs: &mut StereoTestBuffers,
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
        let mut output_events = OutputEvents::from_buffer(&mut bufs.output_events_buffer);
        started
            .process(
                &input_audio,
                &mut output_audio,
                &InputEvents::empty(),
                &mut output_events,
                None,
                None,
            )
            .expect("process failed in RT robustness test");
    }

    fn fill_tone_pair(bufs: &mut StereoTestBuffers, first_sample: usize, identical: bool) {
        for i in 0..FRAMES {
            let n = (first_sample + i) as f32;
            let left = (n * 2.0 * PI * 440.0 / SAMPLE_RATE).sin() * 0.5;
            let right = if identical {
                left
            } else {
                (n * 2.0 * PI * 1000.0 / SAMPLE_RATE).sin() * 0.5
            };
            bufs.in_l[i] = left;
            bufs.in_r[i] = right;
        }
    }

    /// T3.1 / P-DEF-04: the input stage's mono detector writes `process_mono`
    /// after the orchestrator sampled it at block start. The downstream stages
    /// now consume the effective value, which is only a behavioural change if
    /// the detector can engage inside a stereo-port block. This pins the
    /// invariant that makes the re-read sufficient: with the R port active,
    /// `mono_epsilon` is `-1.0`, so identical L/R input never collapses to mono
    /// and the stereo chains stay independent across every block.
    #[test]
    fn stereo_port_identical_input_never_engages_mono_mid_block() {
        let (_entry, _host_info, mut instance) = stereo_instance();
        let stopped = instance
            .activate(|_, _| (), audio_config())
            .expect("activate");
        let mut started = stopped.start_processing().expect("start_processing");
        let mut bufs = StereoTestBuffers::new(FRAMES, 0.0, 0.0);

        let mut sample = 0usize;
        for _ in 0..8 {
            fill_tone_pair(&mut bufs, sample, false);
            sample += FRAMES;
            process_block(&mut started, &mut bufs);
        }

        for block in 0..200 {
            fill_tone_pair(&mut bufs, sample, true);
            sample += FRAMES;
            process_block(&mut started, &mut bufs);

            // SAFETY: the processor pointer comes from the active wrapper of this live plugin instance; the processor is stopped and driven exclusively by this test, so the mutable borrow is unique and cannot outlive the instance.
            let processor = unsafe { &*processor_ptr(&mut instance) };
            assert!(
                !processor.process_mono,
                "block {block}: stereo port must keep process_mono false"
            );
            assert_eq!(
                processor.cached_gate_params.mono_epsilon, -1.0,
                "block {block}: stereo port must keep the mono detector disabled"
            );
        }
    }

    /// T3.2 / P-DEF-05: a panic inside the audio-thread `flush()` must latch the
    /// poisoned state exactly like `process()` so the main-thread watchdog
    /// requests the host restart instead of letting the instance keep running.
    #[test]
    fn flush_panic_latches_poisoned_and_requests_restart() {
        let _lock = crate::clap::processor::processor_poisoning_test::TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        ensure_isolated_crash_dir();
        let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
        let state_ap = state.clone();
        let _stopped = instance
            .activate(
                move |_, _| make_harness_audio_processor(&state_ap),
                audio_config(),
            )
            .expect("activate");
        // SAFETY: `extract_plugin_shared`/`extract_shared` returns a pointer into the live plugin instance owned by this test; the instance is kept alive and unmoved for the borrow, access goes through atomics/interior-mutable fields, and no `&mut` to the same shared state is alive.
        let shared = unsafe { &*extract_plugin_shared(&mut instance) };
        // SAFETY: the main-thread pointer is obtained from the live plugin instance's wrapper; the instance outlives this borrow, the main thread is accessed through `&self` with interior mutability, and no `&mut` to it is alive.
        let main_thread = unsafe { &*extract_plugin_main_thread(&mut instance) };
        // SAFETY: the processor pointer comes from the active wrapper of this live plugin instance; the processor is stopped and driven exclusively by this test, so the mutable borrow is unique and cannot outlive the instance.
        let processor = unsafe { &mut *processor_ptr(&mut instance) };

        shared
            .cold
            .test_flush_panic_injection
            .store(true, Ordering::Relaxed);
        let event_buffer = EventBuffer::new();
        let input_events = InputEvents::from_buffer(&event_buffer);
        let mut out_buffer = EventBuffer::new();
        let mut out_events = OutputEvents::from_buffer(&mut out_buffer);
        processor.flush(&input_events, &mut out_events);

        assert!(processor.poisoned, "a flush panic must latch poisoned");
        assert!(
            shared
                .cold
                .rt_status
                .check_flag(RT_STATUS_PROCESSOR_POISONED),
            "a flush panic must raise RT_STATUS_PROCESSOR_POISONED"
        );
        assert!(!state.restart_requested.load(Ordering::SeqCst));
        main_thread.housekeeping();
        assert!(
            state.restart_requested.load(Ordering::SeqCst),
            "the watchdog must request a restart after a flush panic"
        );
    }

    /// T3.3 / P-DEF-07: the containment of a non-finite block must restore the
    /// musical gate's initial channel state. The gate envelope ingests the faulty
    /// block before the input stage sanitizes it, so without the reset the
    /// attenuation state stays poisoned after reconvergence.
    #[test]
    fn non_finite_containment_restores_musical_gate_state() {
        let (_entry, _host_info, mut instance) = stereo_instance();
        let stopped = instance
            .activate(|_, _| (), audio_config())
            .expect("activate");
        // SAFETY: `extract_plugin_shared`/`extract_shared` returns a pointer into the live plugin instance owned by this test; the instance is kept alive and unmoved for the borrow, access goes through atomics/interior-mutable fields, and no `&mut` to the same shared state is alive.
        let shared = unsafe { &*test_util::extract_shared(&mut instance) };
        shared
            .ui_to_rt
            .param_gate_mode
            .store(GateMode::Musical.to_u32(), Ordering::Relaxed);
        shared.bump_generation();
        let mut started = stopped.start_processing().expect("start_processing");
        let mut bufs = StereoTestBuffers::new(FRAMES, 0.0, 0.0);
        let mut sample = 0usize;
        for _ in 0..4 {
            fill_tone_pair(&mut bufs, sample, false);
            sample += FRAMES;
            process_block(&mut started, &mut bufs);
        }

        fill_tone_pair(&mut bufs, sample, false);
        sample += FRAMES;
        bufs.in_l[FRAMES / 2] = f32::NAN;
        process_block(&mut started, &mut bufs);

        fill_tone_pair(&mut bufs, sample, false);
        process_block(&mut started, &mut bufs);

        // SAFETY: the processor pointer comes from the active wrapper of this live plugin instance; the processor is stopped and driven exclusively by this test, so the mutable borrow is unique and cannot outlive the instance.
        let processor = unsafe { &*processor_ptr(&mut instance) };
        assert!(
            processor.musical_gate.is_attenuating(true),
            "containment must restore the musical gate's initial (attenuating) channel state"
        );
        assert!(
            bufs.out_l
                .iter()
                .chain(bufs.out_r.iter())
                .all(|x| x.is_finite()),
            "output after containment must be finite"
        );
    }

    /// T3.4 / P-RT-01: `reset()` (seek / loop) must not re-arm the one-shot RT
    /// thread setup, which would run scheduler and CPU queries on the audio
    /// thread. `stop_processing()`/`start_processing()` still re-arm it.
    #[test]
    fn reset_does_not_rearm_rt_thread_setup() {
        let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
        let stopped = instance
            .activate(|_, _| (), audio_config())
            .expect("activate");
        let mut started = stopped.start_processing().expect("start_processing");
        // SAFETY: the processor pointer comes from the active wrapper of this live plugin instance; the processor is stopped and driven exclusively by this test, so the mutable borrow is unique and cannot outlive the instance.
        assert!(unsafe { &*processor_ptr(&mut instance) }.rt_thread_setup_done);

        started.reset();
        assert!(
            // SAFETY: the processor pointer comes from the active wrapper of this live plugin instance; the processor is stopped and driven exclusively by this test, so the mutable borrow is unique and cannot outlive the instance.
            unsafe { &*processor_ptr(&mut instance) }.rt_thread_setup_done,
            "reset() must not re-arm the RT thread setup"
        );

        let stopped = started.stop_processing();
        assert!(
            // SAFETY: the processor pointer comes from the active wrapper of this live plugin instance; the processor is stopped and driven exclusively by this test, so the mutable borrow is unique and cannot outlive the instance.
            !unsafe { &*processor_ptr(&mut instance) }.rt_thread_setup_done,
            "stop_processing() must re-arm the RT thread setup"
        );
        let _started = stopped.start_processing().expect("start_processing");
        // SAFETY: the processor pointer comes from the active wrapper of this live plugin instance; the processor is stopped and driven exclusively by this test, so the mutable borrow is unique and cannot outlive the instance.
        assert!(unsafe { &*processor_ptr(&mut instance) }.rt_thread_setup_done);
    }

    /// T3.5 / P-RT-02: the host tail extension is resolved once at activation so
    /// the RT tail notification never performs an extension lookup.
    #[test]
    fn host_tail_extension_is_cached_at_activation() {
        let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
        let state_ap = state.clone();
        let _stopped = instance
            .activate(
                move |_, _| make_harness_audio_processor(&state_ap),
                audio_config(),
            )
            .expect("activate");
        // SAFETY: the processor pointer comes from the active wrapper of this live plugin instance; the processor is stopped and driven exclusively by this test, so the mutable borrow is unique and cannot outlive the instance.
        let processor = unsafe { &*processor_ptr(&mut instance) };
        assert!(
            processor.host_tail.is_some(),
            "the harness host implements the tail extension; it must be cached"
        );
    }
}
