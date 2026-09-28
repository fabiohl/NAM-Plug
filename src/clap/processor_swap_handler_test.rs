// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Stress tests for the [`RtSwapHandler`](neural_amp_modeler_rs::common::spsc::RtSwapHandler)
//! binding of the CLAP command payloads ([`ClapCommandSwapHandler`]), driven
//! through the engine's generic structural-swap scheduler
//! ([`RtSwapDrain`](neural_amp_modeler_rs::common::spsc::RtSwapDrain) +
//! [`SwapBudget`](neural_amp_modeler_rs::common::spsc::SwapBudget) +
//! [`GcSink`](neural_amp_modeler_rs::common::spsc::GcSink)) with real
//! `ClapParamPayload` traffic against a live processor.
//!
//! Validated contract:
//! - classification (`is_structural`, `coalesce_key`) matches the command
//!   budgeting policy (`Params` light; model/IR/oversample structural and
//!   coalescible; restore transactions structural and atomic);
//! - at most one structural install per callback under the shared budget,
//!   excess parked in the drain's deferred slot (FIFO intact);
//! - same-kind bursts coalesce latest-wins, superseded resources retired
//!   through the GC cascade (never dropped on the audio thread);
//! - the acknowledgment published by `after_drain` stays monotonic and
//!   gapless across deferrals (seed + installs + discards reproduces the
//!   consumer's `processed_seq` exactly);
//! - the whole drain cycle is allocation-free on the simulated audio thread
//!   (heap-audit gate).

#[cfg(test)]
mod tests {
    use crate::clap::plugin::{ClapParamPayload, RestoreTxn};
    use crate::clap::processor::events::command_swap_parts;
    use crate::clap::test_util::{self, TestHost};
    use clack_host::prelude::*;
    use neural_amp_modeler_rs::common::params::RtProcessingParams;
    use neural_amp_modeler_rs::common::spsc::{
        GcItem, RT_STATUS_STRUCTURAL_DEFERRED, RT_STATUS_STRUCTURAL_SUPERSEDED, RtSwapHandler,
        SwapBudget, SwapTunables,
    };
    use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter;
    use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;
    use neural_amp_modeler_rs::dsp::oversample::{OversampleEngine, OversampleFactor};
    use neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF;
    use neural_amp_modeler_rs::dsp::resampler::NamResampler;
    use neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer;
    use std::sync::atomic::Ordering;

    const N: usize = 512;

    /// Engine tuning mirroring the processor's command consumer: a 64-pop
    /// coalescing window, one structural apply per callback, no backlog flag.
    const TUNABLES: SwapTunables = SwapTunables {
        pops_per_callback: 64,
        swaps_per_callback: 1,
        backlog_flag: false,
    };

    fn audio_config(max_frames: u32) -> PluginAudioConfiguration {
        PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: max_frames,
            max_frames_count: max_frames,
        }
    }

    /// Raw pointer to the live RT processor. The caller must keep the plugin
    /// instance (and its started processor) alive while dereferencing it.
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

    /// Activates a fresh test plugin and returns the started processor (kept
    /// alive by the binding), the shared state and the main-thread view.
    fn activate_plugin(
        plugin_instance: &mut PluginInstance<TestHost>,
    ) -> (
        StartedPluginAudioProcessor<TestHost>,
        &'static crate::clap::plugin::NamClapShared,
        &'static crate::clap::plugin::NamClapMainThread<'static>,
    ) {
        let stopped = plugin_instance
            .activate(|_, _| (), audio_config(N as u32))
            .expect("activation");
        let started = stopped.start_processing().expect("start processing");
        let shared = unsafe { &*test_util::extract_shared(plugin_instance) };
        let mt = unsafe { &*main_thread_ptr(plugin_instance) };
        (started, shared, mt)
    }

    /// Runs one callback drain on the processor's engine drain
    /// (`cmd_drain`, which owns the command consumer and the deferred slot):
    /// splits the processor into the canonical handler + GC sink (exactly the
    /// split the production drain uses) and executes the engine's 3-phase
    /// protocol under the callback budget.
    fn run_drain(proc: &mut crate::clap::processor::NamClapProcessor<'_>) {
        let (mut handler, mut gc) = command_swap_parts!(proc);
        let mut budget = SwapBudget::new(TUNABLES.swaps_per_callback);
        proc.cmd_drain.drain(&mut handler, &mut budget, &mut gc);
    }

    fn make_adapter(ir_len: usize) -> Box<CabSimAdapter> {
        let ir: Vec<f32> = (0..ir_len)
            .map(|i| {
                let t = i as f32;
                (t * 0.05).sin() * (-t * 0.02).exp()
            })
            .collect();
        Box::new(
            CabSimAdapter::new(Box::new(
                ConvEngine::new(&ir, N).expect("ConvEngine must build"),
            ))
            .expect("CabSimAdapter must build"),
        )
    }

    /// Model envelope with no weights: exercises the resampler/stream swap
    /// and GC retirement without requiring a model fixture.
    fn make_clear_model_payload(generation: u64) -> ClapParamPayload {
        ClapParamPayload::LoadModel {
            generation,
            model_l: None,
            new_resampler: Box::new(
                NamResampler::new_simple(48_000, 48_000).expect("bypass resampler"),
            ),
            new_stream: Box::new(
                StreamingResampleBuffer::new(48_000, 48_000, N).expect("stream adapter"),
            ),
            input_mult_adj: 1.0,
            output_mult_adj: 1.0,
        }
    }

    fn params_with_gate(db: f32) -> RtProcessingParams {
        // `RtProcessingParams` is `#[non_exhaustive]`: mutate through Default.
        let mut params = RtProcessingParams::default();
        params.gate_threshold_db = db;
        params
    }

    fn make_restore(generation: u64) -> ClapParamPayload {
        ClapParamPayload::RestoreTxn(RestoreTxn {
            generation,
            model: None,
            ir: None,
            params: RtProcessingParams::default(),
        })
    }

    /// Classification policy: `Params` is the only light kind; model/IR/
    /// oversample are structural and coalescible on distinct kind ids; a
    /// restore transaction is structural but never coalescible (atomic).
    #[test]
    fn test_handler_classification_is_structural_and_coalesce_keys() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let (_started, _shared, _mt) = activate_plugin(&mut plugin_instance);
        let proc_ptr = audio_processor_ptr(&mut plugin_instance);
        let proc = unsafe { &mut *proc_ptr };

        let (handler, _gc) = command_swap_parts!(proc);

        let params = ClapParamPayload::Params(RtProcessingParams::default());
        let clear_model = make_clear_model_payload(0);
        let clear_ir = ClapParamPayload::LoadCabIr { adapter: None };
        let oversample = ClapParamPayload::SetOversample {
            os_l: Box::new(
                OversampleEngine::new(OversampleFactor::Off, MAX_RESAMP_BUF)
                    .expect("oversample engine (L)"),
            ),
            os_r: Box::new(
                OversampleEngine::new(OversampleFactor::Off, MAX_RESAMP_BUF)
                    .expect("oversample engine (R)"),
            ),
        };
        let restore = make_restore(0);

        let cases: [(&str, &ClapParamPayload, bool, Option<u64>); 5] = [
            ("Params", &params, false, None),
            ("LoadModel", &clear_model, true, Some(0)),
            ("LoadCabIr", &clear_ir, true, Some(1)),
            ("SetOversample", &oversample, true, Some(2)),
            ("RestoreTxn", &restore, true, None),
        ];

        for (name, payload, structural, key) in cases {
            assert_eq!(
                handler.is_structural(payload),
                structural,
                "{name}: structural classification"
            );
            assert_eq!(handler.coalesce_key(payload), key, "{name}: coalesce key");
        }
    }

    /// Command budgeting: with one structural slot per callback, a burst of
    /// three restore transactions installs exactly one per drain; the excess
    /// parks in the deferred slot (FIFO intact) and resolves before new ring
    /// payloads on the next callback. The ack tracks installs 1 → 2 → 3 with
    /// no gaps.
    #[test]
    fn test_drain_installs_one_restore_per_callback_and_parks_excess() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let (_started, shared, mt) = activate_plugin(&mut plugin_instance);
        let proc_ptr = audio_processor_ptr(&mut plugin_instance);
        let proc = unsafe { &mut *proc_ptr };

        for restore_gen in 1..=3u64 {
            let seq = mt
                .cmd_producer
                .borrow_mut()
                .push_command(make_restore(restore_gen))
                .expect("command push");
            assert_eq!(seq, restore_gen, "sequence numbers are assigned 1-based");
        }

        run_drain(proc);
        assert_eq!(
            shared.cold.last_applied_generation.load(Ordering::Relaxed),
            1,
            "only the first restore applies in callback 1"
        );
        assert!(
            proc.cmd_drain.has_deferred(),
            "second restore parks in the slot"
        );
        assert!(
            shared
                .cold
                .rt_status
                .check_flag(RT_STATUS_STRUCTURAL_DEFERRED),
            "deferral must raise the RT status flag"
        );
        assert_eq!(shared.cold.cmd_last_ack.load(Ordering::Acquire), 1);
        assert_eq!(
            mt.cmd_producer.borrow().last_acked_seq(),
            1,
            "producer view of the ack must match the shared atomic"
        );

        run_drain(proc);
        assert_eq!(
            shared.cold.last_applied_generation.load(Ordering::Relaxed),
            2,
            "the parked restore resolves before new ring payloads"
        );
        assert!(
            !proc.cmd_drain.has_deferred(),
            "slot freed after the resolve"
        );
        assert_eq!(shared.cold.cmd_last_ack.load(Ordering::Acquire), 2);

        run_drain(proc);
        assert_eq!(
            shared.cold.last_applied_generation.load(Ordering::Relaxed),
            3
        );
        assert!(!proc.cmd_drain.has_deferred());
        assert_eq!(proc.cmd_drain.ring_occupied(), 0, "all commands consumed");
        assert_eq!(
            shared.cold.cmd_last_ack.load(Ordering::Acquire),
            3,
            "ack is gapless: one per consumed command"
        );
    }

    /// Latest-wins coalescing: three IR swaps inside one callback window
    /// collapse into the newest; the two superseded adapters are retired
    /// through the GC cascade (never dropped on the audio thread) and the
    /// ack covers all three popped slots.
    #[test]
    fn test_drain_coalesces_same_kind_burst_and_supersedes_via_gc() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let (_started, shared, mt) = activate_plugin(&mut plugin_instance);
        let proc_ptr = audio_processor_ptr(&mut plugin_instance);
        let proc = unsafe { &mut *proc_ptr };

        for ir_len in [512usize, 1024, 2048] {
            mt.cmd_producer
                .borrow_mut()
                .push_command(ClapParamPayload::LoadCabIr {
                    adapter: Some(make_adapter(ir_len)),
                })
                .expect("command push");
        }

        run_drain(proc);

        assert!(
            shared
                .cold
                .rt_status
                .check_flag(RT_STATUS_STRUCTURAL_SUPERSEDED),
            "coalesced-away commands must raise the superseded flag"
        );
        assert_eq!(
            shared
                .cold
                .rt_status
                .structural_superseded_total
                .load(Ordering::Relaxed),
            2,
            "two IR swaps are superseded by the newest"
        );
        assert_eq!(
            shared.rt_to_ui.cabsim_tail_samples.load(Ordering::Relaxed),
            2048,
            "the newest IR (2048 samples, one partition per 512 block) wins"
        );
        assert!(!proc.cmd_drain.has_deferred());
        assert_eq!(proc.cmd_drain.ring_occupied(), 0);
        assert_eq!(
            shared.cold.cmd_last_ack.load(Ordering::Acquire),
            3,
            "superseded discards consume their ring slots too"
        );
        assert_eq!(mt.cmd_producer.borrow().last_acked_seq(), 3);

        // The retired resources reach the GC cascade: the main thread's GC
        // consumer observes both superseded adapters.
        let mut gc_rx = mt.gc_rx.borrow_mut();
        for _ in 0..2 {
            assert!(
                matches!(gc_rx.pop(), Ok(GcItem::CabConvAdapter(_))),
                "both superseded adapters must reach the GC cascade"
            );
        }
    }

    /// Acknowledgment gaplessness under deferral with mixed traffic: light
    /// scalars install inline (consuming their slots), the restore parks
    /// (rolling back its slot until resolved), and the ack stays monotonic —
    /// `3` after the first drain (the parked transaction's slot is excluded)
    /// and `4` once it resolves, never skipping a sequence number.
    #[test]
    fn test_drain_ack_gapless_under_deferral() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let (_started, shared, mt) = activate_plugin(&mut plugin_instance);
        let proc_ptr = audio_processor_ptr(&mut plugin_instance);
        let proc = unsafe { &mut *proc_ptr };

        mt.cmd_producer
            .borrow_mut()
            .push_command(ClapParamPayload::Params(params_with_gate(-70.0)))
            .expect("push params #1");
        mt.cmd_producer
            .borrow_mut()
            .push_command(ClapParamPayload::LoadCabIr {
                adapter: Some(make_adapter(512)),
            })
            .expect("push IR");
        mt.cmd_producer
            .borrow_mut()
            .push_command(make_restore(77))
            .expect("push restore");
        mt.cmd_producer
            .borrow_mut()
            .push_command(ClapParamPayload::Params(params_with_gate(-50.0)))
            .expect("push params #2");

        proc.gate_dirty = false;
        run_drain(proc);

        // Params #1 + IR (installed) + Params #2 (inline) = 3 slots consumed;
        // the parked restore's slot is rolled back, so the ack excludes it.
        assert_eq!(shared.cold.cmd_last_ack.load(Ordering::Acquire), 3);
        assert!(
            proc.cmd_drain.has_deferred(),
            "the restore parks behind the IR"
        );
        assert_eq!(
            proc.params.gate_threshold_db, -50.0,
            "the last Params snapshot (inline install) wins"
        );
        assert!(
            proc.gate_dirty,
            "gate coefficient change must invalidate cached gate params"
        );
        assert_ne!(
            shared.cold.last_applied_generation.load(Ordering::Relaxed),
            77,
            "the restore must not apply while parked"
        );

        run_drain(proc);

        assert_eq!(
            shared.cold.last_applied_generation.load(Ordering::Relaxed),
            77,
            "the parked restore applies on the next callback"
        );
        assert_eq!(
            shared.cold.cmd_last_ack.load(Ordering::Acquire),
            4,
            "ack catches up gaplessly once the parked slot resolves"
        );
        assert_eq!(proc.cmd_drain.ring_occupied(), 0);
        assert_eq!(mt.cmd_producer.borrow().last_acked_seq(), 4);
    }

    /// Light payloads (`Params`) never consume the structural budget and
    /// never park: a lone snapshot installs in one drain and its slot is
    /// acknowledged.
    #[test]
    fn test_drain_params_light_payload_applies_inline() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let (_started, shared, mt) = activate_plugin(&mut plugin_instance);
        let proc_ptr = audio_processor_ptr(&mut plugin_instance);
        let proc = unsafe { &mut *proc_ptr };

        mt.cmd_producer
            .borrow_mut()
            .push_command(ClapParamPayload::Params(params_with_gate(-60.0)))
            .expect("push params");

        proc.gate_dirty = false;
        run_drain(proc);

        assert!(!proc.cmd_drain.has_deferred());
        assert_eq!(proc.cmd_drain.ring_occupied(), 0);
        assert_eq!(shared.cold.cmd_last_ack.load(Ordering::Acquire), 1);
        assert_eq!(proc.params.gate_threshold_db, -60.0);
        assert!(proc.gate_dirty);
    }

    /// Heap-audit gate: the full drain cycle — pops, a coalesced discard,
    /// structural installs (IR, restore transaction, model envelope with
    /// resampler/stream swap) and the ack publication — performs zero heap
    /// allocations on the simulated audio thread. All payloads are pushed
    /// (allocated, boxed) before the counted window opens.
    #[cfg(feature = "heap-audit")]
    #[test]
    fn test_swap_handler_drain_zero_alloc() {
        use neural_amp_modeler_rs::common::spsc::RT_STATUS_SPSC_DRAIN_TRUNCATED;

        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let (_started, shared, mt) = activate_plugin(&mut plugin_instance);
        let proc_ptr = audio_processor_ptr(&mut plugin_instance);
        let proc = unsafe { &mut *proc_ptr };

        mt.cmd_producer
            .borrow_mut()
            .push_command(ClapParamPayload::LoadCabIr {
                adapter: Some(make_adapter(512)),
            })
            .expect("push IR A");
        mt.cmd_producer
            .borrow_mut()
            .push_command(ClapParamPayload::LoadCabIr {
                adapter: Some(make_adapter(1024)),
            })
            .expect("push IR B");
        mt.cmd_producer
            .borrow_mut()
            .push_command(make_restore(5))
            .expect("push restore");
        mt.cmd_producer
            .borrow_mut()
            .push_command(ClapParamPayload::Params(params_with_gate(-80.0)))
            .expect("push params");
        mt.cmd_producer
            .borrow_mut()
            .push_command(make_clear_model_payload(9))
            .expect("push model envelope");

        test_util::assert_zero_alloc("swap-handler drain burst", || {
            for _ in 0..3 {
                run_drain(proc);
            }
        });

        // Every pushed payload was consumed across the three drains: IR A
        // superseded, IR B + restore + params + model envelope installed.
        assert_eq!(proc.cmd_drain.ring_occupied(), 0);
        assert!(!proc.cmd_drain.has_deferred());
        assert!(
            !shared
                .cold
                .rt_status
                .check_flag(RT_STATUS_SPSC_DRAIN_TRUNCATED)
        );
        assert_eq!(shared.cold.cmd_last_ack.load(Ordering::Acquire), 5);
        assert_eq!(
            shared.cold.last_applied_generation.load(Ordering::Relaxed),
            5
        );
        // Engine protocol: light scalars install inline and never wait for a
        // parked structural head, so the `Params(-80)` snapshot (seq 4) was
        // consumed in drain 1 while the restore transaction (seq 3) parked;
        // when the parked transaction resolves in drain 2 its own (older)
        // full snapshot lands last and wins — an accepted scalar-ahead
        // inversion of the scheduler, not an ack/sequencing defect.
        assert_eq!(proc.params.gate_threshold_db, -70.0);
        assert_eq!(
            shared.rt_to_ui.cabsim_tail_samples.load(Ordering::Relaxed),
            1024,
            "IR B (1024 samples) is the surviving cab-sim"
        );
        assert!(
            shared
                .cold
                .rt_status
                .structural_superseded_total
                .load(Ordering::Relaxed)
                >= 1,
            "IR A is coalesced away into the GC cascade"
        );
    }
}
