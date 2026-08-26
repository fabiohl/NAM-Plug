// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! T3.2 / F-CONC-006 — monotonic `model_generation` rejects stale slimmable rebuilds.
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

    fn activate_plugin(
        plugin_instance: &mut PluginInstance<TestHost>,
    ) -> (
        StartedPluginAudioProcessor<TestHost>,
        *const crate::clap::plugin::NamClapShared,
        *mut crate::clap::plugin::NamClapMainThread<'static>,
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
            let mt = unsafe { &mut *main_thread_ptr };
            mt.slimmable_tx
                .push(SlimmableRebuild {
                    generation: stale_gen,
                    model: make_linear_model(0.5),
                })
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
            let mt = unsafe { &mut *main_thread_ptr };
            let mut saw_linear = false;
            let mut saw_other = false;
            while let Ok(item) = mt.gc_rx.pop() {
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
            let mt = unsafe { &mut *main_thread_ptr };
            mt.slimmable_tx
                .push(SlimmableRebuild {
                    generation: active_gen,
                    model: make_linear_model(0.9),
                })
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
            let mt = unsafe { &mut *main_thread_ptr };
            let mut saw_linear = false;
            let mut saw_other = false;
            while let Ok(item) = mt.gc_rx.pop() {
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
}
