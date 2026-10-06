// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

#[cfg(test)]
mod tests {
    #[cfg(feature = "heap-audit")]
    use crate::clap::test_util::TestHost;
    #[cfg(feature = "heap-audit")]
    use crate::clap::test_util::{self, StereoTestBuffers};
    #[cfg(feature = "heap-audit")]
    use clack_host::prelude::*;

    #[cfg(feature = "heap-audit")]
    use std::sync::atomic::Ordering;

    #[cfg(feature = "heap-audit")]
    struct AuditEnabledGuard;

    #[cfg(feature = "heap-audit")]
    impl AuditEnabledGuard {
        fn new() -> Self {
            neural_amp_modeler_rs::common::alloc_audit::AUDIT_ENABLED
                .store(true, Ordering::Relaxed);
            Self
        }
    }

    #[cfg(feature = "heap-audit")]
    impl Drop for AuditEnabledGuard {
        fn drop(&mut self) {
            neural_amp_modeler_rs::common::alloc_audit::AUDIT_ENABLED
                .store(false, Ordering::Relaxed);
        }
    }

    /// Serializes the evidence-gate tests across this audit lane: the audit
    /// flips a global `AUDIT_ENABLED` switch, so concurrent audited tests
    /// could mask each other's guard drop. Lock here keeps determinism.
    #[cfg(feature = "heap-audit")]
    static AUDIT_LANE_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Runs a single stereo block through the processor, returning the
    /// `ProcessStatus`. The audio and event buffers are re-created per block
    /// so the borrows stay local to each call.
    #[cfg(feature = "heap-audit")]
    fn process_block(
        started: &mut StartedPluginAudioProcessor<TestHost>,
        bufs: &mut StereoTestBuffers,
    ) -> ProcessStatus {
        process_block_with_events(started, bufs, &InputEvents::empty())
    }

    /// Runs a single stereo block through the processor with caller-supplied
    /// host input events, returning the `ProcessStatus`. The audio buffer is
    /// re-created per call so the borrows stay local; the event buffer is
    /// owned by the caller and must be built off-RT (outside the audited
    /// window), exactly like the DAW main thread staging automation.
    #[cfg(feature = "heap-audit")]
    fn process_block_with_events(
        started: &mut StartedPluginAudioProcessor<TestHost>,
        bufs: &mut StereoTestBuffers,
        events: &InputEvents<'_>,
    ) -> ProcessStatus {
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
                events,
                &mut output_events,
                None,
                None,
            )
            .expect("process failed in heap audit test")
    }

    /// Writes a short decaying-sine impulse response as a mono IEEE-float32
    /// WAV so the cabsim hot path is exercised during the audit.
    #[cfg(feature = "heap-audit")]
    fn write_synthetic_ir(path: &std::path::Path, sample_rate: u32) {
        let samples: Vec<f32> = (0..512)
            .map(|i| {
                let t = i as f32;
                (t * 0.1).sin() * (-t * 0.02).exp()
            })
            .collect();
        neural_amp_modeler_rs::testing::wav::write_wav_f32(path, &samples, sample_rate)
            .expect("failed to write synthetic IR WAV");
    }

    /// Architectural gate ensuring continuous real-time inference remains allocation-free.
    ///
    /// This test exercises the full DSP signal chain, including oversampling and IR
    /// convolution. It asserts that no dynamic memory allocations occur on the audio
    /// thread while processing audio buffers, effectively preventing regressions that
    /// could introduce stuttering or dropouts in host applications.
    #[cfg(feature = "heap-audit")]
    #[test]
    fn test_heap_audit_real_inference_zero_alloc() {
        let model_path = crate::clap::test_util::model_path("wavenet_a1_standard.nam");
        assert!(
            model_path.exists(),
            "wavenet_a1_standard.nam fixture missing — heap-audit gate requires a real model"
        );

        let ir_path = std::env::temp_dir().join("nam_plug_heap_audit_ir.wav");
        write_synthetic_ir(&ir_path, 48000);

        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let mut params = test_util::make_default_params(Some(model_path));
        params.oversample = neural_amp_modeler_rs::dsp::oversample::OversampleFactor::X2;
        params.ir_path = Some(ir_path.clone());
        params.ir_hash = crate::clap::test_util::asset_hash(&ir_path);
        test_util::load_plugin_state(&mut plugin_instance, &params);

        plugin_instance.call_on_main_thread_callback();

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 512,
            max_frames_count: 512,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let mut bufs = StereoTestBuffers::new(512, 0.0, 0.0);
        for i in 0..512 {
            bufs.in_l[i] = (i as f32 * 0.05).sin() * 0.5;
            bufs.in_r[i] = bufs.in_l[i];
        }

        // Process a few blocks to reach steady state.
        for _ in 0..5 {
            process_block(&mut started_processor, &mut bufs);
        }

        // Verify the model actually loaded and processed audio: output must
        // not be identically zero or pure passthrough (proves we're auditing
        // active inference, not the zeroed bypass path).
        let max_abs = bufs.out_l.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
        assert!(
            max_abs > 0.01,
            "output must carry processed signal (got max_abs={max_abs:.6})"
        );

        // Run audited blocks.
        let shared_ptr = test_util::extract_shared(&mut plugin_instance);
        let shared = unsafe { &*shared_ptr };

        for _ in 0..20 {
            let status = process_block(&mut started_processor, &mut bufs);
            assert!(
                matches!(status, ProcessStatus::Continue),
                "expected ProcessStatus::Continue, got {status:?}"
            );
            assert!(
                !shared
                    .cold
                    .rt_status
                    .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HEAP_ALLOC),
                "RT_STATUS_HEAP_ALLOC set — allocation detected in inference hot path"
            );
            assert_eq!(
                neural_amp_modeler_rs::common::alloc_audit::get_alloc_count(),
                0,
                "zero heap allocations expected in active inference path"
            );
        }
    }

    /// Architectural gate for zero-alloc impulse response lifecycle management.
    ///
    /// This test verifies that the real-time thread correctly handles IR lifecycle events
    /// (load, swap, clear) by offloading deallocation to the main thread via a garbage
    /// collection cascade. This ensures that changing cab simulations does not cause
    /// audio glitches due to non-deterministic memory management.
    #[cfg(feature = "heap-audit")]
    #[test]
    fn test_heap_audit_cabsim_swap_cycle_zero_alloc() {
        use crate::clap::plugin::ClapParamPayload;
        use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter;
        use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;

        let model_path = crate::clap::test_util::model_path("wavenet_a1_standard.nam");
        assert!(
            model_path.exists(),
            "wavenet_a1_standard.nam fixture missing — heap-audit gate requires a real model"
        );

        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let mut params = test_util::make_default_params(Some(model_path));
        params.oversample = neural_amp_modeler_rs::dsp::oversample::OversampleFactor::X2;
        test_util::load_plugin_state(&mut plugin_instance, &params);
        plugin_instance.call_on_main_thread_callback();

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 512,
            max_frames_count: 512,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let shared_ptr = test_util::extract_shared(&mut plugin_instance);
        let main_thread_ptr = main_thread_ptr(&mut plugin_instance);
        let shared = unsafe { &*shared_ptr };

        // Warm-up: drain LoadModel / Params commands before the audit starts.
        let n = 512;
        let mut bufs = StereoTestBuffers::new(n, 0.2, 0.2);
        for _ in 0..8 {
            process_block(&mut started_processor, &mut bufs);
        }

        // Off-RT adapter construction (untracked — happens between audited
        // callbacks). Two distinct IRs: A = 512 samples (1 partition), B =
        // 1024 samples (2 partitions) at the 512-sample partition size.
        let ir_a: Vec<f32> = (0..512)
            .map(|i| {
                let t = i as f32;
                (t * 0.1).sin() * (-t * 0.02).exp()
            })
            .collect();
        let ir_b: Vec<f32> = (0..1024)
            .map(|i| {
                let t = i as f32;
                (t * 0.05 + 1.3).sin() * (-t * 0.01).exp()
            })
            .collect();
        let adapter_a = CabSimAdapter::new(Box::new(
            ConvEngine::new(&ir_a, n).expect("ConvEngine A must build"),
        ))
        .expect("CabSimAdapter A must build");
        let adapter_b = CabSimAdapter::new(Box::new(
            ConvEngine::new(&ir_b, n).expect("ConvEngine B must build"),
        ))
        .expect("CabSimAdapter B must build");

        let _audit_guard = AuditEnabledGuard::new();
        shared
            .cold
            .rt_status
            .clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HEAP_ALLOC);

        // Step 1 — Load IR 1: the drained `LoadCabIr` swap must be zero-alloc.
        {
            let mt = unsafe { &*main_thread_ptr };
            mt.cmd_producer
                .borrow_mut()
                .push_command(ClapParamPayload::LoadCabIr {
                    adapter: Some(Box::new(adapter_a)),
                })
                .expect("Load IR 1 push must succeed");
        }
        let status = process_block(&mut started_processor, &mut bufs);
        assert!(
            matches!(status, ProcessStatus::Continue),
            "expected ProcessStatus::Continue after IR 1 load, got {status:?}"
        );
        assert!(
            !shared
                .cold
                .rt_status
                .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HEAP_ALLOC),
            "RT_STATUS_HEAP_ALLOC set — allocation detected during IR 1 install"
        );
        assert_eq!(
            neural_amp_modeler_rs::common::alloc_audit::get_alloc_count(),
            0,
            "zero heap allocations expected during IR 1 install"
        );
        let tail_after_a = shared.rt_to_ui.cabsim_tail_samples.load(Ordering::Relaxed);
        assert!(
            tail_after_a > 0,
            "IR 1 must be installed (cabsim tail must be non-zero)"
        );

        // Step 2 — Swap IR 2 (A → B): zero-alloc, and the tail must change
        // (proving a real adapter swap, not a no-op).
        {
            let mt = unsafe { &*main_thread_ptr };
            mt.cmd_producer
                .borrow_mut()
                .push_command(ClapParamPayload::LoadCabIr {
                    adapter: Some(Box::new(adapter_b)),
                })
                .expect("Swap IR 2 push must succeed");
        }
        let status = process_block(&mut started_processor, &mut bufs);
        assert!(
            matches!(status, ProcessStatus::Continue),
            "expected ProcessStatus::Continue after IR 2 swap, got {status:?}"
        );
        assert!(
            !shared
                .cold
                .rt_status
                .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HEAP_ALLOC),
            "RT_STATUS_HEAP_ALLOC set — allocation detected during IR swap"
        );
        assert_eq!(
            neural_amp_modeler_rs::common::alloc_audit::get_alloc_count(),
            0,
            "zero heap allocations expected during IR swap"
        );
        let tail_after_b = shared.rt_to_ui.cabsim_tail_samples.load(Ordering::Relaxed);
        assert!(
            tail_after_b > tail_after_a,
            "IR 2 (longer) must be installed — tail must grow ({tail_after_a} → {tail_after_b})"
        );

        // Step 3 — Clear IR (B → None): zero-alloc, tail must drop to zero.
        {
            let mt = unsafe { &*main_thread_ptr };
            mt.cmd_producer
                .borrow_mut()
                .push_command(ClapParamPayload::LoadCabIr { adapter: None })
                .expect("Clear IR push must succeed");
        }
        let status = process_block(&mut started_processor, &mut bufs);
        assert!(
            matches!(status, ProcessStatus::Continue),
            "expected ProcessStatus::Continue after IR clear, got {status:?}"
        );
        assert!(
            !shared
                .cold
                .rt_status
                .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HEAP_ALLOC),
            "RT_STATUS_HEAP_ALLOC set — allocation detected during IR clear"
        );
        assert_eq!(
            neural_amp_modeler_rs::common::alloc_audit::get_alloc_count(),
            0,
            "zero heap allocations expected during IR clear"
        );
        assert_eq!(
            shared.rt_to_ui.cabsim_tail_samples.load(Ordering::Relaxed),
            0,
            "IR must be cleared (cabsim tail must be zero)"
        );

        // Drain the GC channel off-RT: the replaced adapters must be dropped
        // on the main thread, never leaked.
        {
            let mt = unsafe { &*main_thread_ptr };
            mt.housekeeping();
        }

        // Step 4 — Stop: tear-down must not drop any GcItem on the audio thread.
        let stopped = started_processor.stop_processing();
        plugin_instance.deactivate(stopped);
    }

    /// Runs one audited audio block and asserts the zero-alloc contract
    /// (`Continue` status, no `RT_STATUS_HEAP_ALLOC`, zero TLS alloc count).
    /// Returns the block's max output RMS so the caller can prove real audio
    /// flowed through the stress run.
    #[cfg(feature = "heap-audit")]
    fn audited_block(
        started: &mut StartedPluginAudioProcessor<TestHost>,
        bufs: &mut StereoTestBuffers,
        shared: &crate::clap::plugin::NamClapShared,
        label: &str,
    ) -> f32 {
        audited_block_with_events(started, bufs, shared, label, &InputEvents::empty())
    }

    /// Same zero-alloc audit contract as [`audited_block`], but drives the
    /// block with caller-supplied host input events (e.g. bypass automation
    /// for the crossfade transition).
    #[cfg(feature = "heap-audit")]
    fn audited_block_with_events(
        started: &mut StartedPluginAudioProcessor<TestHost>,
        bufs: &mut StereoTestBuffers,
        shared: &crate::clap::plugin::NamClapShared,
        label: &str,
        events: &InputEvents<'_>,
    ) -> f32 {
        let status = process_block_with_events(started, bufs, events);
        assert!(
            matches!(status, ProcessStatus::Continue),
            "{label}: expected ProcessStatus::Continue (zero-alloc), got {status:?}"
        );
        assert!(
            !shared
                .cold
                .rt_status
                .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HEAP_ALLOC),
            "{label}: RT_STATUS_HEAP_ALLOC set — allocation detected in burst stress block"
        );
        assert_eq!(
            neural_amp_modeler_rs::common::alloc_audit::get_alloc_count(),
            0,
            "{label}: zero heap allocations expected in burst stress block"
        );
        let l_rms =
            (bufs.out_l.iter().map(|x| x * x).sum::<f32>() / bufs.out_l.len() as f32).sqrt();
        let r_rms =
            (bufs.out_r.iter().map(|x| x * x).sum::<f32>() / bufs.out_r.len() as f32).sqrt();
        l_rms.max(r_rms)
    }

    /// Sustained mixed bursts of IR, model and oversample swaps plus atomic
    /// restores, all while real audio flows, must keep the audio thread at
    /// exactly zero heap allocations on every callback.
    ///
    /// Extends the swap-cycle template to a burst workload: every round
    /// pushes a same-kind IR burst (3 → 2 callbacks, 1 supersede), a same-kind
    /// model burst (3 → 2 callbacks, 1 supersede), a mixed structural burst
    /// (`SetOversample` + a full atomic `RestoreTxn` carrying model+IR+params,
    /// 2 → 2 callbacks) and a non-coalescible restore burst (3 → 3 callbacks).
    /// The Command Budgeting layer applies at most one structural command
    /// per callback and defers the excess; superseded resources are discarded
    /// off-RT through the GC cascade — never dropped on the audio thread.
    ///
    /// The resource payloads (models, IR adapters, oversample engines) are built
    /// by the test between audited callbacks, exactly like the DAW main thread
    /// does — the per-thread tracking guard is only active inside the audio
    /// callback, so this off-RT allocation is invisible to the audit lane while
    /// the callback itself must remain provably zero-alloc.
    ///
    /// Per round the burst defers exactly 5 structural commands (IR:1, model:1,
    /// mixed:1, restore-burst:2) and supersedes exactly 2 (IR:1, model:1); every
    /// `RestoreTxn` (4 per round) is applied atomically in FIFO order.
    #[cfg(feature = "heap-audit")]
    #[test]
    fn test_heap_audit_ir_model_swap_burst_zero_alloc() {
        use crate::clap::plugin::{ClapParamPayload, LoadModelPayload, RestoreTxn};
        use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
        use neural_amp_modeler_rs::common::params::RtProcessingParams;
        use neural_amp_modeler_rs::common::spsc::{
            RT_STATUS_GC_OVERFLOW, RT_STATUS_HEAP_ALLOC, RT_STATUS_STRUCTURAL_DEFERRED,
            RT_STATUS_STRUCTURAL_SUPERSEDED,
        };
        use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter;
        use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;
        use neural_amp_modeler_rs::dsp::oversample::{OversampleEngine, OversampleFactor};
        use neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF;
        use neural_amp_modeler_rs::dsp::resampler::NamResampler;
        use neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer;
        use neural_amp_modeler_rs::loader::build::load_and_build_model;
        use neural_amp_modeler_rs::models::{NamModel, StaticModel};

        const BLOCK: usize = 512;
        const HOST_RATE: u32 = 48000;
        const ROUNDS: usize = 8;
        // Engine protocol, event-level counting per round: IR + model bursts
        // coalesce latest-wins inside their first callback (0 defers); the
        // mixed burst parks the restore behind the oversample install (1);
        // the 3-restore burst yields park + stop (2), stop (1), idle (0).
        // Total 4 defers. Supersedes: 2 per coalescing burst = 4.
        const DEFERS_PER_ROUND: u32 = 4;
        const SUPERSEDES_PER_ROUND: u32 = 4;

        let model_path = crate::clap::test_util::model_path("wavenet_a1_standard.nam");
        assert!(
            model_path.exists(),
            "wavenet_a1_standard.nam fixture missing — heap-audit gate requires a real model"
        );

        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let mut params = test_util::make_default_params(Some(model_path));
        params.oversample = OversampleFactor::X2;
        test_util::load_plugin_state(&mut plugin_instance, &params);
        plugin_instance.call_on_main_thread_callback();

        let audio_config = PluginAudioConfiguration {
            sample_rate: HOST_RATE as f64,
            min_frames_count: BLOCK as u32,
            max_frames_count: BLOCK as u32,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let shared_ptr = test_util::extract_shared(&mut plugin_instance);
        let main_thread_ptr = main_thread_ptr(&mut plugin_instance);
        let shared = unsafe { &*shared_ptr };

        // The model MUST have loaded — a zero counter means the audit only
        // exercised the bypass path.
        assert!(
            shared.cold.model_load_counter.load(Ordering::Relaxed) > 0,
            "model_load_counter must be > 0 — the audit must run real inference"
        );

        let n = BLOCK;
        let mut bufs = StereoTestBuffers::new(n, 0.2, 0.2);

        // Warm-up: drain the LoadModel / SetOversample / Params commands and let
        // the gate/hysteresis/smoothers converge before auditing.
        for _ in 0..8 {
            process_block(&mut started_processor, &mut bufs);
        }

        // ── Off-RT resource builders (main-thread role) ──
        // The payloads are constructed between audited callbacks — the per-thread
        // tracking guard is only active inside `process()`, so this allocation is
        // invisible to the audit lane (exactly like the DAW main thread building
        // models/IRs asynchronously while the audio thread keeps processing).
        let sys = SystemSnapshot::capture();
        let build_ir = |ir_len: usize| -> Box<CabSimAdapter> {
            let ir: Vec<f32> = (0..ir_len)
                .map(|i| {
                    let t = i as f32;
                    (t * 0.05).sin() * (-t * 0.02).exp()
                })
                .collect();
            Box::new(
                CabSimAdapter::new(Box::new(
                    ConvEngine::new(&ir, BLOCK).expect("ConvEngine must build"),
                ))
                .expect("CabSimAdapter must build"),
            )
        };
        let build_model = |name: &str| -> (
            Option<Box<StaticModel>>,
            Box<NamResampler>,
            Box<StreamingResampleBuffer>,
            f32,
            f32,
        ) {
            let path = crate::clap::test_util::model_path(name);
            let pair = load_and_build_model(
                &path,
                &sys,
                false,
                neural_amp_modeler_rs::loader::LoadOptions::default(),
            )
            .unwrap_or_else(|e| panic!("failed to load fixture model {name}: {e:?}"));
            let mut model_l = pair.model_l;
            if let Some(ref mut m) = model_l {
                m.set_max_buffer_size(BLOCK).expect("model pre-size failed");
            }
            let new_resampler =
                Box::new(NamResampler::new_simple(HOST_RATE, pair.sample_rate).expect("resampler"));
            let new_stream =
                crate::clap::plugin::build_stream_adapter(HOST_RATE, pair.sample_rate, BLOCK)
                    .expect("streaming adapter");
            (
                model_l,
                new_resampler,
                new_stream,
                pair.input_mult_adj,
                pair.output_mult_adj,
            )
        };
        let build_os = |factor: OversampleFactor| -> ClapParamPayload {
            ClapParamPayload::SetOversample {
                os_l: Box::new(OversampleEngine::new(factor, MAX_RESAMP_BUF).expect("os L")),
                os_r: Box::new(OversampleEngine::new(factor, MAX_RESAMP_BUF).expect("os R")),
            }
        };

        // ── Arm the audit lane for the entire stress run. ──
        let _audit_guard = AuditEnabledGuard::new();
        shared.cold.rt_status.clear_flag(RT_STATUS_HEAP_ALLOC);

        let mut max_rms = 0.0f32;
        let mut next_gen: u64 = 0;

        for round in 0..ROUNDS {
            // Burst 1 — same-kind IR coalescing burst (3 payloads → 1
            // callback: the engine collapses the burst latest-wins and
            // applies C directly; A and B are superseded to the GC).
            {
                let mt = unsafe { &*main_thread_ptr };
                for len in [512usize, 1024, 2048] {
                    mt.cmd_producer
                        .borrow_mut()
                        .push_command(ClapParamPayload::LoadCabIr {
                            adapter: Some(build_ir(len)),
                        })
                        .expect("IR burst push must succeed");
                }
            }
            max_rms = max_rms.max(audited_block(
                &mut started_processor,
                &mut bufs,
                shared,
                "IR burst: apply first",
            ));
            max_rms = max_rms.max(audited_block(
                &mut started_processor,
                &mut bufs,
                shared,
                "IR burst: supersede + apply",
            ));

            // Burst 2 — same-kind model coalescing burst (3 payloads → 1
            // callback: only the last model installs; the two intermediates
            // are superseded to the GC).
            {
                let mt = unsafe { &*main_thread_ptr };
                for name in [
                    if round % 2 == 0 {
                        "wavenet_a1_standard.nam"
                    } else {
                        "lstm.nam"
                    },
                    if round % 2 == 0 {
                        "lstm.nam"
                    } else {
                        "wavenet_a1_standard.nam"
                    },
                    if round % 2 == 0 {
                        "wavenet_a1_standard.nam"
                    } else {
                        "lstm.nam"
                    },
                ] {
                    let (model_l, new_resampler, new_stream, input_mult_adj, output_mult_adj) =
                        build_model(name);
                    mt.cmd_producer
                        .borrow_mut()
                        .push_command(ClapParamPayload::LoadModel {
                            generation: 0,
                            model_l,
                            new_resampler,
                            new_stream,
                            input_mult_adj,
                            output_mult_adj,
                        })
                        .expect("model burst push must succeed");
                }
            }
            max_rms = max_rms.max(audited_block(
                &mut started_processor,
                &mut bufs,
                shared,
                "model burst: apply first",
            ));
            max_rms = max_rms.max(audited_block(
                &mut started_processor,
                &mut bufs,
                shared,
                "model burst: supersede + apply",
            ));

            // Burst 3 — mixed structural burst: `SetOversample` engine rebuild
            // followed by a full atomic `RestoreTxn` (model + IR + params),
            // 2 → 2 callbacks (the restore is deferred one callback).
            next_gen += 1;
            {
                let mt = unsafe { &*main_thread_ptr };
                mt.cmd_producer
                    .borrow_mut()
                    .push_command(build_os(if round % 2 == 0 {
                        OversampleFactor::X4
                    } else {
                        OversampleFactor::X2
                    }))
                    .expect("SetOversample push must succeed");
                let (model_l, new_resampler, new_stream, input_mult_adj, output_mult_adj) =
                    build_model(if round % 2 == 0 {
                        "lstm.nam"
                    } else {
                        "wavenet_a1_standard.nam"
                    });
                mt.cmd_producer
                    .borrow_mut()
                    .push_command(ClapParamPayload::RestoreTxn(RestoreTxn {
                        generation: next_gen,
                        model: Some(LoadModelPayload {
                            generation: 0,
                            model_l,
                            new_resampler,
                            new_stream,
                            input_mult_adj,
                            output_mult_adj,
                        }),
                        ir: Some(Some(build_ir(4096))),
                        params: RtProcessingParams::default().with_oversample(OversampleFactor::X2),
                    }))
                    .expect("mixed RestoreTxn push must succeed");
            }
            max_rms = max_rms.max(audited_block(
                &mut started_processor,
                &mut bufs,
                shared,
                "mixed burst: SetOversample",
            ));
            max_rms = max_rms.max(audited_block(
                &mut started_processor,
                &mut bufs,
                shared,
                "mixed burst: atomic RestoreTxn",
            ));

            // Burst 4 — non-coalescible restore burst (3 → 3 callbacks; a
            // RestoreTxn is ack-gated and never superseded, so each callback
            // applies exactly one generation in FIFO order).
            {
                let mt = unsafe { &*main_thread_ptr };
                for _ in 0..3 {
                    next_gen += 1;
                    mt.cmd_producer
                        .borrow_mut()
                        .push_command(ClapParamPayload::RestoreTxn(RestoreTxn {
                            generation: next_gen,
                            model: None,
                            ir: Some(None),
                            params: RtProcessingParams::default()
                                .with_oversample(OversampleFactor::X2),
                        }))
                        .expect("restore burst push must succeed");
                }
            }
            for _ in 0..3 {
                max_rms = max_rms.max(audited_block(
                    &mut started_processor,
                    &mut bufs,
                    shared,
                    "restore burst",
                ));
            }

            // Off-RT GC drain between rounds (main-thread role). The replaced
            // models/resamplers/streams/adapters/engines are dropped here, never
            // on the audio thread; allocations in housekeeping are invisible to
            // the audit lane because the tracking guard is not active outside
            // the audio callback.
            {
                let mt = unsafe { &*main_thread_ptr };
                mt.housekeeping();
            }
        }

        // ── Final assertions ──
        assert!(
            !shared.cold.rt_status.check_flag(RT_STATUS_HEAP_ALLOC),
            "RT_STATUS_HEAP_ALLOC set during the burst stress run"
        );
        assert_eq!(
            shared.cold.last_applied_generation.load(Ordering::Relaxed),
            next_gen,
            "every atomic RestoreTxn must be applied (none superseded, none lost)"
        );
        assert_eq!(
            shared
                .cold
                .rt_status
                .structural_deferred_total
                .load(Ordering::Relaxed),
            ROUNDS as u32 * DEFERS_PER_ROUND,
            "each round must defer exactly the predicted excess structural commands"
        );
        assert_eq!(
            shared
                .cold
                .rt_status
                .structural_superseded_total
                .load(Ordering::Relaxed),
            ROUNDS as u32 * SUPERSEDES_PER_ROUND,
            "each round must supersede exactly the predicted same-kind deferred commands"
        );
        assert!(
            shared
                .cold
                .rt_status
                .check_flag(RT_STATUS_STRUCTURAL_DEFERRED),
            "RT_STATUS_STRUCTURAL_DEFERRED must have been raised during the burst stress"
        );
        assert!(
            shared
                .cold
                .rt_status
                .check_flag(RT_STATUS_STRUCTURAL_SUPERSEDED),
            "RT_STATUS_STRUCTURAL_SUPERSEDED must have been raised during the burst stress"
        );
        assert!(
            !shared.cold.rt_status.check_flag(RT_STATUS_GC_OVERFLOW),
            "GC overflow must not occur — housekeeping keeps the cascade drained"
        );
        assert!(
            max_rms > 1e-6,
            "the burst stress must keep real audio flowing (max output RMS {max_rms:.6})"
        );

        // Tear-down: stop and deactivate — the final off-RT drain covers any
        // GcItem still in flight, never dropping on the audio thread.
        let stopped = started_processor.stop_processing();
        plugin_instance.deactivate(stopped);
    }

    /// Returns a raw pointer to the plugin main-thread state so tests can push
    /// SPSC commands exactly like the DAW's main thread would.
    #[cfg(feature = "heap-audit")]
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

    /// The stereo wet path must be zero-alloc as well.
    /// Asymmetric L/R inputs keep the engine's mono detector open, so the
    /// audited steady state exercises independent-channel inference (R dry
    /// passthrough, `process_mono == false`).
    #[cfg(feature = "heap-audit")]
    #[test]
    fn test_heap_audit_stereo_wet_path_zero_alloc() {
        let model_path = crate::clap::test_util::model_path("wavenet_a1_standard.nam");
        assert!(
            model_path.exists(),
            "wavenet_a1_standard.nam fixture missing — heap-audit gate requires a real model"
        );

        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let params = test_util::make_default_params(Some(model_path));
        test_util::load_plugin_state(&mut plugin_instance, &params);

        plugin_instance.call_on_main_thread_callback();

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 512,
            max_frames_count: 512,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };
        assert!(
            shared.cold.model_load_counter.load(Ordering::Relaxed) > 0,
            "model_load_counter must be > 0 — the audit must run real inference"
        );

        let n = 512;
        let mut bufs = StereoTestBuffers::new(n, 0.2, 0.3);

        // Warm-up: drain the LoadModel command and let gate/smoothers converge
        // while the engine settles on the stereo path (L != R keeps the mono
        // detector open).
        for _ in 0..8 {
            process_block(&mut started_processor, &mut bufs);
        }

        let _audit_guard = AuditEnabledGuard::new();
        shared
            .cold
            .rt_status
            .clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HEAP_ALLOC);

        for _ in 0..8 {
            let status = process_block(&mut started_processor, &mut bufs);
            assert!(
                matches!(status, ProcessStatus::Continue),
                "expected ProcessStatus::Continue (zero-alloc), got {status:?}"
            );
            assert!(
                !shared
                    .cold
                    .rt_status
                    .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HEAP_ALLOC),
                "RT_STATUS_HEAP_ALLOC set — allocation detected in stereo inference hot path"
            );
            assert_eq!(
                neural_amp_modeler_rs::common::alloc_audit::get_alloc_count(),
                0,
                "zero heap allocations expected in stereo inference path"
            );
        }
    }

    /// Secondary gate: an invalid model must fail to load gracefully (no
    /// panic, `RT_STATUS_MODEL_LOAD_FAILED` set, counter stays at 0). Kept
    /// as a distinct test so graceful-degradation behaviour is still covered.
    #[cfg(feature = "heap-audit")]
    #[test]
    fn test_heap_audit_invalid_model_graceful() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let invalid_path = std::env::temp_dir().join("nam_plug_heap_audit_invalid.nam");
        test_util::write_invalid_model_fixture(&invalid_path);

        let params = test_util::make_default_params(Some(invalid_path));
        let state_bytes = serde_json::to_vec(&params).unwrap();
        let state_ext = test_util::get_state_ext(&mut plugin_instance);
        let handle = plugin_instance.plugin_handle();
        let _ = state_ext.load(&handle, &mut state_bytes.as_slice());

        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

        // Invalid fixture fails to build (model_l = None post-build).
        // load_model now rejects this — counter stays at 0.
        assert_eq!(
            shared.cold.model_load_counter.load(Ordering::Relaxed),
            0,
            "model_load_counter should not increment when model build fails"
        );

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 512,
            max_frames_count: 512,
        };

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let n = 512;
        let mut bufs = StereoTestBuffers::new(n, 0.1, 0.2);

        // Activates heap audit globally using RAII guard
        let _audit_guard = AuditEnabledGuard::new();

        // Resets the status flag to ensure it is clean before
        shared
            .cold
            .rt_status
            .clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HEAP_ALLOC);

        // Runs process(), which should return Continue (zero heap allocations in the hot path)
        let status = process_block(&mut started_processor, &mut bufs);

        // Verifies that the returned status is Continue (zero-alloc path)
        assert!(
            matches!(status, ProcessStatus::Continue),
            "Expected ProcessStatus::Continue (zero-alloc), got {status:?}"
        );

        // Verifies the RT_STATUS_HEAP_ALLOC flag was NOT set (no allocations detected)
        assert!(
            !shared
                .cold
                .rt_status
                .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HEAP_ALLOC)
        );

        // Verifies the RT_STATUS_MODEL_LOAD_FAILED flag was set (invalid fixture fails to build)
        assert!(
            shared
                .cold
                .rt_status
                .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_MODEL_LOAD_FAILED),
            "Expected RT_STATUS_MODEL_LOAD_FAILED to be set because invalid fixture fails to build"
        );
    }

    /// Architectural gate: the bypass-crossfade cold path
    /// (`process_crossfade_sub_block`) must remain allocation-free.
    ///
    /// The crossover is pinned by signal content, not probes: a bypass-flip
    /// event schedules the 64-sample ramp that blends the live wet inference
    /// against the latency-compensated dry capture. The audited ramp head
    /// must therefore deviate from the steady-bypass dry reference wherever
    /// the blend is active (left channel, the inference chain), and a
    /// completed ramp-to-dry block must resume the pure delayed-dry
    /// passthrough bit-exactly in its tail.
    #[cfg(feature = "heap-audit")]
    #[test]
    fn test_heap_audit_bypass_crossfade_zero_alloc() {
        use crate::clap::extensions::params::PARAM_BYPASS;
        use crate::clap::processor::state::BYPASS_XFADE_SAMPLES;
        use clack_common::events::Pckn;
        use clack_common::events::event_types::ParamValueEvent;
        use clack_common::utils::ClapId;

        let _lane = AUDIT_LANE_MUTEX.lock().expect("audit lane mutex poisoned");

        // Host block must cover the whole ramp so the crossover lives inside
        // one audited callback.
        const BLOCK: usize = 128;
        const { assert!(BLOCK >= BYPASS_XFADE_SAMPLES) };

        let model_path = crate::clap::test_util::model_path("wavenet_a1_standard.nam");
        assert!(
            model_path.exists(),
            "wavenet_a1_standard.nam fixture missing — heap-audit gate requires a real model"
        );

        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let params = test_util::make_default_params(Some(model_path));
        test_util::load_plugin_state(&mut plugin_instance, &params);
        plugin_instance.call_on_main_thread_callback();

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: BLOCK as u32,
            max_frames_count: BLOCK as u32,
        };
        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };
        assert!(
            shared.cold.model_load_counter.load(Ordering::Relaxed) > 0,
            "model_load_counter must be > 0 — the audit must run real inference"
        );

        // Distinct per-channel tones keep the mono detector out of the way:
        // the crossfade blends the left inference leg against dry, while the
        // right channel must remain the dry passthrough (dual-mono: the
        // active inference chain is left-only).
        let mut bufs = StereoTestBuffers::new(BLOCK, 0.0, 0.0);
        for i in 0..BLOCK {
            bufs.in_l[i] = (i as f32 * 0.05).sin() * 0.5;
            bufs.in_r[i] = ((i as f32 * 0.07) + 1.13).sin() * 0.3;
        }

        // Wet warm-up: settle gate, smoothers and the inference steady state.
        for _ in 0..8 {
            let status = process_block(&mut started_processor, &mut bufs);
            assert!(
                matches!(status, ProcessStatus::Continue),
                "wet warm-up must continue, got {status:?}"
            );
        }

        // Bypass automation buffers (built off-RT, outside the audited
        // window): 1.0 = bypass on, 0.0 = pipeline.
        let mut ev_bypass = EventBuffer::new();
        let mut ev_wet = EventBuffer::new();
        let ever = ParamValueEvent::new(0u32, ClapId::new(PARAM_BYPASS), Pckn::match_all(), 1.0);
        ev_bypass.push(&ever);
        let evwet = ParamValueEvent::new(0u32, ClapId::new(PARAM_BYPASS), Pckn::match_all(), 0.0);
        ev_wet.push(&evwet);

        // Dry reference: identical input blocks copied through the
        // latency-compensated dry ring are per-block constant, so the last
        // bypass block is the reusable delayed-dry passthrough reference.
        let bypass_events = InputEvents::from_buffer(&ev_bypass);
        let mut dry_l_ref = vec![0.0f32; BLOCK];
        let mut dry_r_ref = vec![0.0f32; BLOCK];
        for _ in 0..4 {
            let status =
                process_block_with_events(&mut started_processor, &mut bufs, &bypass_events);
            assert!(
                matches!(status, ProcessStatus::Continue),
                "dry reference phase must continue, got {status:?}"
            );
            dry_l_ref.copy_from_slice(&bufs.out_l);
            dry_r_ref.copy_from_slice(&bufs.out_r);
        }
        let dry_peak = dry_l_ref
            .iter()
            .chain(dry_r_ref.iter())
            .fold(0.0f32, |m, &x| m.max(x.abs()));
        assert!(
            dry_peak > 0.01,
            "dry reference must carry passthrough signal (peak {dry_peak:.6})"
        );

        // ── Arm the audit lane for the crossfade transitions ──
        let _audit_guard = AuditEnabledGuard::new();
        shared
            .cold
            .rt_status
            .clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HEAP_ALLOC);

        let wet_events = InputEvents::from_buffer(&ev_wet);

        // Ramp-to-wet: the event flips bypass off and the crossover blends the
        // live wet leg against dry; both the ramp head and the pure-wet tail
        // must deviate from the dry reference.
        let rms = audited_block_with_events(
            &mut started_processor,
            &mut bufs,
            shared,
            "crossfade ramp to wet",
            &wet_events,
        );
        assert!(
            rms > 0.01,
            "crossfade-to-wet must carry audio (rms {rms:.6})"
        );
        assert_deviation_from(&bufs.out_l, &dry_l_ref, "crossfade-to-wet left channel");
        // The right channel is dry passthrough by architecture (the
        // dual-mono inference chain is left-only): continuity, no wet blend.
        assert_dry_continuity(&bufs.out_r, &dry_r_ref, "crossfade-to-wet right channel");

        // Ramp-to-dry: the ramp head blends wet (deviates from dry); once the
        // 64-sample ramp completes, the pure portion resumes the dry
        // passthrough — the tail must equal the reference bit-exactly.
        let rms = audited_block_with_events(
            &mut started_processor,
            &mut bufs,
            shared,
            "crossfade ramp to bypass",
            &bypass_events,
        );
        assert!(
            rms > 0.01,
            "crossfade-to-dry must carry audio (rms {rms:.6})"
        );
        assert_eq!(
            bufs.out_l[BYPASS_XFADE_SAMPLES..],
            dry_l_ref[BYPASS_XFADE_SAMPLES..],
            "post-ramp tail must equal the dry reference bit-exactly"
        );
        assert_eq!(
            bufs.out_r[BYPASS_XFADE_SAMPLES..],
            dry_r_ref[BYPASS_XFADE_SAMPLES..],
            "post-ramp tail must equal the dry reference bit-exactly"
        );
        assert_deviation_from(
            &bufs.out_l[..BYPASS_XFADE_SAMPLES],
            &dry_l_ref[..BYPASS_XFADE_SAMPLES],
            "crossfade-to-dry left ramp head",
        );
        assert_dry_continuity(
            &bufs.out_r[..BYPASS_XFADE_SAMPLES],
            &dry_r_ref[..BYPASS_XFADE_SAMPLES],
            "crossfade-to-dry right ramp head",
        );

        // One more round-trip: a second flip pair must reproduce the same
        // signatures while staying zero-alloc.
        let rms = audited_block_with_events(
            &mut started_processor,
            &mut bufs,
            shared,
            "crossfade ramp back to wet",
            &wet_events,
        );
        assert!(
            rms > 0.01,
            "second crossfade-to-wet must carry audio (rms {rms:.6})"
        );
        assert_deviation_from(
            &bufs.out_l,
            &dry_l_ref,
            "second crossfade-to-wet left channel",
        );
        assert_dry_continuity(
            &bufs.out_r,
            &dry_r_ref,
            "second crossfade-to-wet right channel",
        );
        let rms = audited_block_with_events(
            &mut started_processor,
            &mut bufs,
            shared,
            "crossfade ramp back to bypass",
            &bypass_events,
        );
        assert!(
            rms > 0.01,
            "second crossfade-to-dry must carry audio (rms {rms:.6})"
        );
        assert_eq!(
            bufs.out_l[BYPASS_XFADE_SAMPLES..],
            dry_l_ref[BYPASS_XFADE_SAMPLES..],
            "post-ramp tail must equal the dry reference bit-exactly"
        );
        assert_eq!(
            bufs.out_r[BYPASS_XFADE_SAMPLES..],
            dry_r_ref[BYPASS_XFADE_SAMPLES..],
            "post-ramp tail must equal the dry reference bit-exactly"
        );
        assert_deviation_from(
            &bufs.out_l[..BYPASS_XFADE_SAMPLES],
            &dry_l_ref[..BYPASS_XFADE_SAMPLES],
            "second crossfade-to-dry left ramp head",
        );
        assert_dry_continuity(
            &bufs.out_r[..BYPASS_XFADE_SAMPLES],
            &dry_r_ref[..BYPASS_XFADE_SAMPLES],
            "second crossfade-to-dry right ramp head",
        );
    }

    /// Architectural gate: the noise-gate tail-drain cold path
    /// (`process_tail_drain`) must remain allocation-free while a cabsim
    /// ring-out drains under a closed gate, and must terminate into pure
    /// silencing.
    ///
    /// Signal pinning: a wet chain with a loaded IR is excited, then
    /// silenced. After the default hold/fade hysteresis elapses the gate FSM
    /// flips to Closed (`RT_STATUS_IS_SILENT` set); the closed blocks then
    /// carry the convolution ring-out until the tail budget is spent, after
    /// which the output is exactly zero. Both phases must stay zero-alloc.
    #[cfg(feature = "heap-audit")]
    #[test]
    fn test_heap_audit_gate_tail_drain_zero_alloc() {
        use neural_amp_modeler_rs::common::spsc::{RT_STATUS_HEAP_ALLOC, RT_STATUS_IS_SILENT};

        let _lane = AUDIT_LANE_MUTEX.lock().expect("audit lane mutex poisoned");

        let model_path = crate::clap::test_util::model_path("wavenet_a1_standard.nam");
        assert!(
            model_path.exists(),
            "wavenet_a1_standard.nam fixture missing — heap-audit gate requires a real model"
        );

        let ir_path = std::env::temp_dir().join("nam_plug_heap_audit_tail_drain_ir.wav");
        write_synthetic_ir(&ir_path, 48000);

        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let mut params = test_util::make_default_params(Some(model_path));
        params.ir_path = Some(ir_path.clone());
        params.ir_hash = crate::clap::test_util::asset_hash(&ir_path);
        test_util::load_plugin_state(&mut plugin_instance, &params);
        plugin_instance.call_on_main_thread_callback();

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 512,
            max_frames_count: 512,
        };
        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };
        assert!(
            shared.cold.model_load_counter.load(Ordering::Relaxed) > 0,
            "model_load_counter must be > 0 — the audit must run real inference"
        );
        assert!(
            shared.rt_to_ui.cabsim_tail_samples.load(Ordering::Relaxed) > 0,
            "cabsim tail must be armed — the tail-drain audit requires a loaded IR"
        );

        let mut bufs = StereoTestBuffers::new(512, 0.0, 0.0);
        for i in 0..512 {
            bufs.in_l[i] = (i as f32 * 0.05).sin() * 0.2;
            bufs.in_r[i] = bufs.in_l[i];
        }

        // Wet warm-up: the conv tail budget re-arms on every active-audio
        // block, so the last warm block leaves a full IR tail behind.
        for _ in 0..8 {
            let status = process_block(&mut started_processor, &mut bufs);
            assert!(
                matches!(status, ProcessStatus::Continue),
                "wet warm-up must continue, got {status:?}"
            );
        }

        // ── Arm the audit lane for the silence/draining sequence ──
        let _audit_guard = AuditEnabledGuard::new();
        shared.cold.rt_status.clear_flag(RT_STATUS_HEAP_ALLOC);

        // Silence the input. The first held-open blocks still run inference
        // (hold phase), then the fade completes and the closed blocks drain
        // the ring-out. The witness is a gate-closed block with ring-out
        // energy — the wet leg cannot produce it while the input is silent
        // and the gate is closed.
        let mut drained_witness = false;
        let mut trailing_silent_blocks = 0usize;
        const SILENCE_BLOCKS: usize = 12;
        for _ in 0..SILENCE_BLOCKS {
            bufs.in_l.fill(0.0);
            bufs.in_r.fill(0.0);
            let rms = audited_block(&mut started_processor, &mut bufs, shared, "gate tail drain");
            let gate_closed = shared.cold.rt_status.check_flag(RT_STATUS_IS_SILENT);
            if gate_closed && rms > 1e-6 {
                drained_witness = true;
                trailing_silent_blocks = 0;
            } else if gate_closed {
                trailing_silent_blocks += 1;
            }
        }
        assert!(
            drained_witness,
            "no gate-closed block carried the convolution ring-out — the tail-drain path was not audited"
        );
        assert!(
            trailing_silent_blocks >= 2,
            "the tail drain must terminate into pure silencing (trailing silent blocks {trailing_silent_blocks})"
        );
    }

    /// Architectural gate: oversampling engine rebuilds (Off → 2× → 4×) must
    /// stay allocation-free on the audio thread, both at the instant the
    /// pre-built engines install (SPSC structural apply inside the audited
    /// callback) and in the steady-state inference blocks running under each
    /// factor.
    ///
    /// Real-path pinning: the install is observed through the published
    /// effective latency, which strictly increases with the half-band latency
    /// table (Off: 0, 2×: 12, 4×: 24 host samples at 48 kHz), and every
    /// steady-state block must carry processed signal under the new factor.
    #[cfg(feature = "heap-audit")]
    #[test]
    fn test_heap_audit_oversample_swap_zero_alloc() {
        use crate::clap::plugin::ClapParamPayload;
        use neural_amp_modeler_rs::common::spsc::RT_STATUS_HEAP_ALLOC;
        use neural_amp_modeler_rs::dsp::oversample::{OversampleEngine, OversampleFactor};
        use neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF;

        let _lane = AUDIT_LANE_MUTEX.lock().expect("audit lane mutex poisoned");

        const BLOCK: usize = 128;

        let model_path = crate::clap::test_util::model_path("wavenet_a1_standard.nam");
        assert!(
            model_path.exists(),
            "wavenet_a1_standard.nam fixture missing — heap-audit gate requires a real model"
        );

        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let params = test_util::make_default_params(Some(model_path));
        test_util::load_plugin_state(&mut plugin_instance, &params);
        plugin_instance.call_on_main_thread_callback();

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: BLOCK as u32,
            max_frames_count: BLOCK as u32,
        };
        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let shared_ptr = test_util::extract_shared(&mut plugin_instance);
        let main_thread_ptr = main_thread_ptr(&mut plugin_instance);
        let shared = unsafe { &*shared_ptr };
        assert!(
            shared.cold.model_load_counter.load(Ordering::Relaxed) > 0,
            "model_load_counter must be > 0 — the audit must run real inference"
        );

        let mut bufs = StereoTestBuffers::new(BLOCK, 0.2, 0.2);

        // Off-RT engine builders, before the audit lane arms — exactly like
        // the DAW main thread pre-building swap payloads.
        let build_os = |factor: OversampleFactor| -> ClapParamPayload {
            ClapParamPayload::SetOversample {
                os_l: Box::new(OversampleEngine::new(factor, MAX_RESAMP_BUF).expect("os L")),
                os_r: Box::new(OversampleEngine::new(factor, MAX_RESAMP_BUF).expect("os R")),
            }
        };
        let x2_payload = build_os(OversampleFactor::X2);
        let x4_payload = build_os(OversampleFactor::X4);

        // Warm-up under Off so the audited window starts from a pipeline the
        // user could actually configure.
        for _ in 0..8 {
            let status = process_block(&mut started_processor, &mut bufs);
            assert!(
                matches!(status, ProcessStatus::Continue),
                "warm-up must continue, got {status:?}"
            );
        }

        // ── Arm the audit lane for the whole Off → 2× → 4× progression ──
        let _audit_guard = AuditEnabledGuard::new();
        shared.cold.rt_status.clear_flag(RT_STATUS_HEAP_ALLOC);

        let latency_off = shared.rt_to_ui.current_latency.load(Ordering::Relaxed);
        let rms = audited_block(
            &mut started_processor,
            &mut bufs,
            shared,
            "oversample Off steady",
        );
        assert!(
            rms > 0.01,
            "Off steady state must carry audio (rms {rms:.6})"
        );

        // Install 2×: the structural apply lands inside this audited callback
        // and the publish path reports the higher effective latency.
        {
            let mt = unsafe { &*main_thread_ptr };
            mt.cmd_producer
                .borrow_mut()
                .push_command(x2_payload)
                .expect("2× oversample push must succeed");
        }
        let rms = audited_block(
            &mut started_processor,
            &mut bufs,
            shared,
            "oversample 2× install block",
        );
        assert!(
            rms > 0.01,
            "2× install block must carry audio (rms {rms:.6})"
        );
        let latency_x2 = shared.rt_to_ui.current_latency.load(Ordering::Relaxed);
        assert!(
            latency_x2 > latency_off,
            "2× install must publish a higher effective latency (Off {latency_off} → X2 {latency_x2})"
        );
        let rms = audited_block(
            &mut started_processor,
            &mut bufs,
            shared,
            "oversample 2× steady",
        );
        assert!(
            rms > 0.01,
            "2× steady state must carry audio (rms {rms:.6})"
        );

        // Install 4×: same contract under the deeper oversampling stage.
        {
            let mt = unsafe { &*main_thread_ptr };
            mt.cmd_producer
                .borrow_mut()
                .push_command(x4_payload)
                .expect("4× oversample push must succeed");
        }
        let rms = audited_block(
            &mut started_processor,
            &mut bufs,
            shared,
            "oversample 4× install block",
        );
        assert!(
            rms > 0.01,
            "4× install block must carry audio (rms {rms:.6})"
        );
        let latency_x4 = shared.rt_to_ui.current_latency.load(Ordering::Relaxed);
        assert!(
            latency_x4 > latency_x2,
            "4× install must publish a higher effective latency (X2 {latency_x2} → X4 {latency_x4})"
        );
        let rms = audited_block(
            &mut started_processor,
            &mut bufs,
            shared,
            "oversample 4× steady",
        );
        assert!(
            rms > 0.01,
            "4× steady state must carry audio (rms {rms:.6})"
        );
    }

    /// Architectural gate: poisoning containment must remain allocation-free
    /// after the poison latch engages. An injected panic in `process()`
    /// surfaces as `Err` with silenced outputs, and every subsequent
    /// silenced block until the host restarts the plugin runs at exactly zero
    /// heap allocations.
    #[cfg(feature = "heap-audit")]
    #[test]
    fn test_heap_audit_poison_containment_zero_alloc() {
        use neural_amp_modeler_rs::common::spsc::{
            RT_STATUS_HEAP_ALLOC, RT_STATUS_PROCESSOR_POISONED,
        };

        // The injection latch is a global one-shot: serialize against the
        // panic-containment suite that shares it.
        let _lock = super::super::processor_poisoning_test::TEST_MUTEX
            .lock()
            .expect("poisoning test mutex poisoned");
        super::super::processor_poisoning_test::ensure_isolated_crash_dir();
        let _lane = AUDIT_LANE_MUTEX.lock().expect("audit lane mutex poisoned");

        const BLOCK: usize = 64;

        let model_path = crate::clap::test_util::model_path("wavenet_a1_standard.nam");
        assert!(
            model_path.exists(),
            "wavenet_a1_standard.nam fixture missing — heap-audit gate requires a real model"
        );

        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let params = test_util::make_default_params(Some(model_path));
        test_util::load_plugin_state(&mut plugin_instance, &params);
        plugin_instance.call_on_main_thread_callback();

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: BLOCK as u32,
            max_frames_count: BLOCK as u32,
        };
        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };
        assert!(
            shared.cold.model_load_counter.load(Ordering::Relaxed) > 0,
            "model_load_counter must be > 0 — the audit must run real inference"
        );

        let mut bufs = StereoTestBuffers::new(BLOCK, 0.2, 0.2);
        for _ in 0..8 {
            let status = process_block(&mut started_processor, &mut bufs);
            assert!(
                matches!(status, ProcessStatus::Continue),
                "healthy warm-up must continue, got {status:?}"
            );
        }
        let healthy_peak = bufs
            .out_l
            .iter()
            .chain(bufs.out_r.iter())
            .fold(0.0f32, |m, &x| m.max(x.abs()));
        assert!(
            healthy_peak > 0.01,
            "the pipeline must be healthy before poisoning (peak {healthy_peak:.6})"
        );

        // Inject the one-shot panic. The panicking callback itself is NOT the
        // audited contract (panic unwinding is not zero-alloc); the audited
        // contract is the silenced containment afterwards.
        super::super::TEST_PANIC_INJECTION.store(true, Ordering::Relaxed);
        let poisoned = {
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
            let events = InputEvents::empty();
            let mut output_events = OutputEvents::from_buffer(&mut bufs.output_events_buffer);
            started_processor.process(
                &input_audio,
                &mut output_audio,
                &events,
                &mut output_events,
                None,
                None,
            )
        };
        assert!(
            poisoned.is_err(),
            "the injected panic must surface as a processing error"
        );
        assert!(
            shared
                .cold
                .rt_status
                .check_flag(RT_STATUS_PROCESSOR_POISONED),
            "RT_STATUS_PROCESSOR_POISONED must be set after the injected panic"
        );
        assert!(
            bufs.out_l.iter().all(|&x| x == 0.0) && bufs.out_r.iter().all(|&x| x == 0.0),
            "the panicking callback must silence the outputs"
        );

        // The unwinding telemetry may have touched the audit flag; the
        // silenced containment path afterwards starts from a clean slate.
        shared.cold.rt_status.clear_flag(RT_STATUS_HEAP_ALLOC);

        // ── Arm the audit lane for the silenced containment path ──
        let _audit_guard = AuditEnabledGuard::new();

        for i in 0..10 {
            let rms = audited_block(
                &mut started_processor,
                &mut bufs,
                shared,
                "poisoned silenced block",
            );
            assert_eq!(rms, 0.0, "block {i} after poisoning must stay silenced");
            assert!(
                shared
                    .cold
                    .rt_status
                    .check_flag(RT_STATUS_PROCESSOR_POISONED),
                "the poison latch must stay engaged while the host has not restarted the plugin"
            );
        }
    }

    /// Asserts that `got` deviates from `reference` on 4 or more samples with
    /// an absolute error above 5e-4 — the observable signature that the wet
    /// (model) leg mixed into the crossfade output instead of a pure copy of
    /// the dry reference.
    #[cfg(feature = "heap-audit")]
    fn assert_deviation_from(got: &[f32], reference: &[f32], label: &str) {
        assert_eq!(got.len(), reference.len(), "{label}: length mismatch");
        let deviating = got
            .iter()
            .zip(reference.iter())
            .filter(|(g, r)| (**g - **r).abs() > 5e-4)
            .count();
        assert!(
            deviating >= 4,
            "{label}: no wet blend signature (deviating samples {deviating} of {})",
            reference.len()
        );
    }

    /// Asserts that `got` keeps the dry reference intact (no significant
    /// deviation). The right channel is dry passthrough by architecture —
    /// the dual-mono inference chain is left-only — so during a crossfade its
    /// signal must remain the latency-compensated dry capture.
    #[cfg(feature = "heap-audit")]
    fn assert_dry_continuity(got: &[f32], reference: &[f32], label: &str) {
        assert_eq!(got.len(), reference.len(), "{label}: length mismatch");
        let deviating = got
            .iter()
            .zip(reference.iter())
            .filter(|(g, r)| (**g - **r).abs() > 5e-4)
            .count();
        assert_eq!(
            deviating, 0,
            "{label}: dry passthrough must remain intact ({deviating} deviating samples)"
        );
    }
}
