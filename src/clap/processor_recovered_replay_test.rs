// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Recovered-command replay: lifecycle, budget and FIFO contract (F-12).
//!
//! Commands drained from the ring by a failed `activate()` are staged in
//! `ColdShared::recovered_commands` and replayed by the next successful
//! activation. The replay must:
//! - survive a deactivate that happens before the first callback (no silent
//!   loss of payloads whose ring sequence was already acknowledged);
//! - respect the structural budget of one apply per callback, exactly as the
//!   ring drain does (`MAX_STRUCTURAL_COMMANDS_PER_CALLBACK`);
//! - keep causal FIFO order: earlier staged commands precede every command
//!   staged later, and ring commands never overtake staged ones.

use crate::clap::plugin::{ClapParamPayload, RestoreTxn, make_test_shared};
use crate::clap::processor::rollback::ActivateRollbackGuard;
use crate::clap::test_util::{self, StereoTestBuffers, TestHost};
use clack_host::prelude::*;
use neural_amp_modeler_rs::common::params::RtProcessingParams;
use std::sync::atomic::Ordering;

const N: usize = 256;

fn audio_config(max_frames: u32) -> PluginAudioConfiguration {
    PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 1,
        max_frames_count: max_frames,
    }
}

fn restore(generation: u64) -> Box<ClapParamPayload> {
    Box::new(ClapParamPayload::RestoreTxn(RestoreTxn {
        generation,
        model: None,
        ir: None,
        params: RtProcessingParams::default(),
        calibration: Default::default(),
        gate_mode: Default::default(),
        cab_bypass: false,
    }))
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
        .unwrap();
}

fn main_thread_ptr(
    instance: &mut PluginInstance<TestHost>,
) -> *const crate::clap::plugin::NamClapMainThread<'static> {
    let raw_ptr = instance.plugin_handle().as_raw_ptr();
    // SAFETY: `raw_ptr` is the live plugin handle owned by this test's instance; `PluginWrapper::handle` runs the closure synchronously while that instance is alive, and the returned raw pointer is only dereferenced while the instance remains in scope.
    unsafe {
        clack_plugin::extensions::wrapper::PluginWrapper::<crate::clap::NamClapPlugin>::handle(
            raw_ptr,
            |w| Ok(w.main_thread() as *const crate::clap::plugin::NamClapMainThread<'static>),
        )
        .unwrap()
    }
}

/// Stages restores as a failed activation would leave them, then activates.
fn activate_with_staged(
    plugin_instance: &mut PluginInstance<TestHost>,
    generations: &[u64],
) -> (
    StartedPluginAudioProcessor<TestHost>,
    *const crate::clap::plugin::NamClapShared,
) {
    let shared_ptr = test_util::extract_shared(plugin_instance);
    {
        // SAFETY: `extract_plugin_shared`/`extract_shared` returns a pointer into the live plugin instance owned by this test; the instance is kept alive and unmoved for the borrow, access goes through atomics/interior-mutable fields, and no `&mut` to the same shared state is alive.
        let shared = unsafe { &*shared_ptr };
        let mut staged = shared.cold.recovered_commands.lock().unwrap();
        for &generation in generations {
            staged.push_back(restore(generation));
        }
    }
    let stopped = plugin_instance
        .activate(|_, _| (), audio_config(N as u32))
        .unwrap();
    let started = stopped.start_processing().unwrap();
    let shared_ptr = test_util::extract_shared(plugin_instance);
    (started, shared_ptr)
}

/// Staged commands must not vanish when the host deactivates before the first
/// callback: their ring sequence was already resolved by the rollback, so a
/// lost payload would never be re-issued and never be applied.
#[test]
fn test_recovered_replay_survives_deactivate_before_first_callback() {
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let (started, shared_ptr) = activate_with_staged(&mut plugin_instance, &[1, 2]);
    // SAFETY: `extract_plugin_shared`/`extract_shared` returns a pointer into the live plugin instance owned by this test; the instance is kept alive and unmoved for the borrow, access goes through atomics/interior-mutable fields, and no `&mut` to the same shared state is alive.
    let shared = unsafe { &*shared_ptr };

    let stopped = started.stop_processing();
    plugin_instance.deactivate(stopped);

    assert_eq!(
        shared.cold.recovered_commands.lock().unwrap().len(),
        2,
        "recovered commands must be returned to cold storage on deactivate, not dropped"
    );

    let (mut started, shared_ptr) = activate_with_staged(&mut plugin_instance, &[]);
    // SAFETY: `extract_plugin_shared`/`extract_shared` returns a pointer into the live plugin instance owned by this test; the instance is kept alive and unmoved for the borrow, access goes through atomics/interior-mutable fields, and no `&mut` to the same shared state is alive.
    let shared = unsafe { &*shared_ptr };
    let mut bufs = StereoTestBuffers::new(N, 0.0, 0.0);
    process_block(&mut started, &mut bufs);
    process_block(&mut started, &mut bufs);
    assert_eq!(
        shared.cold.last_applied_generation.load(Ordering::Relaxed),
        2,
        "the replayed restores must apply after reactivation, in order"
    );

    let stopped = started.stop_processing();
    plugin_instance.deactivate(stopped);
}

/// The replay is bounded by the same structural budget as the ring: one
/// structural apply per callback, FIFO across the staged prefix and the ring.
#[test]
fn test_recovered_replay_respects_structural_budget_and_fifo() {
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let (mut started, shared_ptr) = activate_with_staged(&mut plugin_instance, &[1, 2, 3]);
    // SAFETY: `extract_plugin_shared`/`extract_shared` returns a pointer into the live plugin instance owned by this test; the instance is kept alive and unmoved for the borrow, access goes through atomics/interior-mutable fields, and no `&mut` to the same shared state is alive.
    let shared = unsafe { &*shared_ptr };

    {
        let main_thread_ptr = main_thread_ptr(&mut plugin_instance);
        // SAFETY: the main-thread pointer is obtained from the live plugin instance's wrapper; the instance outlives this borrow, the main thread is accessed through `&self` with interior mutability, and no `&mut` to it is alive.
        let mt = unsafe { &*main_thread_ptr };
        mt.cmd_producer
            .borrow_mut()
            .push_command(*restore(4))
            .expect("ring push must succeed");
    }

    let mut bufs = StereoTestBuffers::new(N, 0.0, 0.0);
    for expected in 1..=4u64 {
        process_block(&mut started, &mut bufs);
        assert_eq!(
            shared.cold.last_applied_generation.load(Ordering::Relaxed),
            expected,
            "exactly one structural command per callback, staged prefix before ring"
        );
    }
}

/// A rollback that runs while earlier commands are still staged must keep the
/// causal order: staged (older) commands first, then the commands drained now.
#[test]
fn test_rollback_keeps_staged_prefix_ahead_of_drained_ring() {
    let shared = make_test_shared();
    let (mut param_tx, param_rx) = rtrb::RingBuffer::new(8);
    let (gc_tx, _gc_rx) = rtrb::RingBuffer::new(8);
    let (box_gc_tx, _box_gc_rx) = rtrb::RingBuffer::new(8);
    let (_slimmable_tx, slimmable_rx) = rtrb::RingBuffer::new(8);

    shared
        .cold
        .recovered_commands
        .lock()
        .unwrap()
        .push_back(restore(1));
    param_tx.push(restore(2)).unwrap();

    {
        let mut guard = ActivateRollbackGuard::new(&shared);
        guard.param_rx = Some(param_rx);
        guard.gc_tx = Some(gc_tx);
        guard.box_gc_tx = Some(box_gc_tx);
        guard.slimmable_rx = Some(slimmable_rx);
    }

    let recovered = shared.cold.recovered_commands.lock().unwrap();
    let generations: Vec<u64> = recovered
        .iter()
        .map(|p| match p.as_ref() {
            ClapParamPayload::RestoreTxn(txn) => txn.generation,
            _ => u64::MAX,
        })
        .collect();
    assert_eq!(
        generations,
        vec![1, 2],
        "staged commands must precede commands drained by a later rollback"
    );
    assert_eq!(shared.cold.cmd_last_ack.load(Ordering::Acquire), 1);
}

/// Light replayed commands are bounded by the same per-callback pop window as
/// the ring drain: a rollback cannot accumulate an unbounded light backlog and
/// apply it all in one callback. The staged prefix keeps draining on later
/// callbacks with the ring closed, so no command is lost or reordered.
#[test]
fn test_recovered_replay_light_backlog_is_bounded_per_callback() {
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let shared_ptr = test_util::extract_shared(&mut plugin_instance);
    {
        // SAFETY: `extract_plugin_shared`/`extract_shared` returns a pointer into the live plugin instance owned by this test; the instance is kept alive and unmoved for the borrow, access goes through atomics/interior-mutable fields, and no `&mut` to the same shared state is alive.
        let shared = unsafe { &*shared_ptr };
        let mut staged = shared.cold.recovered_commands.lock().unwrap();
        for _ in 0..100 {
            staged.push_back(Box::new(ClapParamPayload::Params(
                RtProcessingParams::default(),
            )));
        }
    }
    let stopped = plugin_instance
        .activate(|_, _| (), audio_config(N as u32))
        .unwrap();
    let mut started = stopped.start_processing().unwrap();
    // SAFETY: `extract_plugin_shared`/`extract_shared` returns a pointer into the live plugin instance owned by this test; the instance is kept alive and unmoved for the borrow, access goes through atomics/interior-mutable fields, and no `&mut` to the same shared state is alive.
    let shared = unsafe { &*shared_ptr };
    let mut bufs = StereoTestBuffers::new(N, 0.0, 0.0);

    process_block(&mut started, &mut bufs);
    let stopped = started.stop_processing();
    plugin_instance.deactivate(stopped);
    assert_eq!(
        shared.cold.recovered_commands.lock().unwrap().len(),
        100 - crate::clap::processor::CMD_SWAP_TUNABLES.pops_per_callback,
        "one callback must replay at most the ring pop window; the rest stays staged"
    );

    let stopped = plugin_instance
        .activate(|_, _| (), audio_config(N as u32))
        .unwrap();
    let mut started = stopped.start_processing().unwrap();
    for _ in 0..2 {
        process_block(&mut started, &mut bufs);
    }
    let stopped = started.stop_processing();
    plugin_instance.deactivate(stopped);
    assert_eq!(
        shared.cold.recovered_commands.lock().unwrap().len(),
        0,
        "the staged prefix must fully drain without loss"
    );
}
