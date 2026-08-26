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

    /// Runs a single stereo block through the processor, returning the
    /// `ProcessStatus`. The audio and event buffers are re-created per block
    /// so the borrows stay local to each call.
    #[cfg(feature = "heap-audit")]
    fn process_block(
        started: &mut StartedPluginAudioProcessor<TestHost>,
        bufs: &mut StereoTestBuffers,
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

    /// R-12: the heap-audit gate must run continuous inference on a real
    /// model with oversampling and cabsim enabled, asserting zero heap
    /// allocations in the active hot path. A model that fails to load is
    /// a failure (the previous fixture was intentionally invalid and only
    /// exercised the bypass path).
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

        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

        // The model MUST have loaded — a zero counter means the audit only
        // exercised the bypass path (the previous fail-open behaviour).
        assert!(
            shared.cold.model_load_counter.load(Ordering::Relaxed) > 0,
            "model_load_counter must be > 0 — the audit must run real inference"
        );

        let n = 512;
        let mut bufs = StereoTestBuffers::new(n, 0.2, 0.2);

        // Warm-up: drain the LoadModel / LoadCabIr / Params commands and let
        // the gate/hysteresis/smoothers converge before auditing.
        for _ in 0..8 {
            process_block(&mut started_processor, &mut bufs);
        }

        // Audited steady-state blocks: continuous inference with oversampling
        // and cabsim must be zero-alloc.
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
                "RT_STATUS_HEAP_ALLOC set — allocation detected in inference hot path"
            );
            assert_eq!(
                neural_amp_modeler_rs::common::alloc_audit::get_alloc_count(),
                0,
                "zero heap allocations expected in active inference path"
            );
        }
    }

    /// T2.1 / F-RT-003: the full cab-sim lifecycle (Start → Load IR 1 →
    /// Swap IR 2 → Clear IR → Stop) must be zero-alloc on the audio thread.
    ///
    /// The adapters are boxed off-RT (main thread) and travel `Box`ed through
    /// the SPSC; the RT swap moves the old `Box` by value into the GC cascade
    /// with no `Box::new`/`malloc` on the callback. Every audited block that
    /// drains one of the three commands must report exactly zero allocations.
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
            let mt = unsafe { &mut *main_thread_ptr };
            mt.cmd_producer
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
            let mt = unsafe { &mut *main_thread_ptr };
            mt.cmd_producer
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
            let mt = unsafe { &mut *main_thread_ptr };
            mt.cmd_producer
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
        // on the main thread, never leaked (T2.1 rollback condition).
        {
            let mt = unsafe { &mut *main_thread_ptr };
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
        let status = process_block(started, bufs);
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

    /// T2.4 / F-RT-003 + F-RT-007: sustained mixed bursts of IR, model and
    /// oversample swaps plus atomic restores, all while real audio flows, must
    /// keep the audio thread at exactly zero heap allocations on every callback.
    ///
    /// Extends the T2.1 swap-cycle template to a burst workload: every round
    /// pushes a same-kind IR burst (3 → 2 callbacks, 1 supersede), a same-kind
    /// model burst (3 → 2 callbacks, 1 supersede), a mixed structural burst
    /// (`SetOversample` + a full atomic `RestoreTxn` carrying model+IR+params,
    /// 2 → 2 callbacks) and a non-coalescible restore burst (3 → 3 callbacks).
    /// The Command Budgeting layer (T2.3) applies at most one structural command
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
        const DEFERS_PER_ROUND: u32 = 5;
        const SUPERSEDES_PER_ROUND: u32 = 2;

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
                Box::new(NamResampler::new(HOST_RATE, pair.sample_rate, 0).expect("resampler"));
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
            // Burst 1 — same-kind IR coalescing burst (3 → 2 callbacks; the
            // deferred IR B is superseded by C on the second callback).
            {
                let mt = unsafe { &mut *main_thread_ptr };
                for len in [512usize, 1024, 2048] {
                    mt.cmd_producer
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

            // Burst 2 — same-kind model coalescing burst (3 → 2 callbacks; the
            // deferred middle model is superseded by the last one).
            {
                let mt = unsafe { &mut *main_thread_ptr };
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
                let mt = unsafe { &mut *main_thread_ptr };
                mt.cmd_producer
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
                        params: RtProcessingParams {
                            oversample: OversampleFactor::X2,
                            ..Default::default()
                        },
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
                let mt = unsafe { &mut *main_thread_ptr };
                for _ in 0..3 {
                    next_gen += 1;
                    mt.cmd_producer
                        .push_command(ClapParamPayload::RestoreTxn(RestoreTxn {
                            generation: next_gen,
                            model: None,
                            ir: Some(None),
                            params: RtProcessingParams {
                                oversample: OversampleFactor::X2,
                                ..Default::default()
                            },
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
                let mt = unsafe { &mut *main_thread_ptr };
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
        // GcItem still in flight (R-04), never dropping on the audio thread.
        let stopped = started_processor.stop_processing();
        plugin_instance.deactivate(stopped);
    }

    /// Returns a raw pointer to the plugin main-thread state so tests can push
    /// SPSC commands exactly like the DAW's main thread would.
    #[cfg(feature = "heap-audit")]
    fn main_thread_ptr(
        instance: &mut PluginInstance<TestHost>,
    ) -> *mut crate::clap::plugin::NamClapMainThread<'static> {
        let raw_ptr = instance.plugin_handle().as_raw_ptr();
        unsafe {
            clack_plugin::extensions::wrapper::PluginWrapper::<crate::clap::NamClapPlugin>::handle(
                raw_ptr,
                |w| Ok(w.main_thread().as_ptr()),
            )
            .unwrap()
        }
    }

    /// R-12 stereo variant (T1.1): the stereo wet path must be zero-alloc too.
    /// Asymmetric L/R inputs keep the engine's mono detector open, so the
    /// audited steady state exercises independent-channel inference (R dry
    /// passthrough, `process_mono == false`) — the path introduced by the
    /// F-PERF-001 channel-routing fix.
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
        let mut handle = plugin_instance.plugin_handle();
        let _ = state_ext.load(&mut handle, &mut state_bytes.as_slice());

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
}
