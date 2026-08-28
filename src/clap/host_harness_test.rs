// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use crate::clap::extensions::params::PARAM_OVERSAMPLE;
use crate::clap::plugin::PendingRestartOs;
use crate::clap::test_util::{model_path, write_model_with_rate};
use clack_common::events::Pckn;
use clack_common::events::event_types::ParamValueEvent;
use clack_common::utils::{ClapId, Cookie};
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
use std::sync::atomic::Ordering;

fn default_audio_config() -> PluginAudioConfiguration {
    PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: 256,
        max_frames_count: 256,
    }
}

#[test]
fn test_thread_check_main_thread() {
    let (_entry, _host_info, _instance, state) = make_test_plugin_with_harness();
    assert!(
        state
            .main_thread_id
            .lock()
            .unwrap()
            .is_none_or(|id| id == std::thread::current().id()),
        "Harness must report is_main_thread=true for the test thread"
    );
}

#[test]
fn test_thread_check_audio_thread() {
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
    state.set_audio_thread();
    let _stopped = instance
        .activate(
            |_, _| make_harness_audio_processor(&state),
            default_audio_config(),
        )
        .expect("activate failed");
    assert!(
        state
            .audio_thread_id
            .lock()
            .unwrap()
            .is_none_or(|id| id == std::thread::current().id()),
        "Harness must report is_audio_thread=true after set_audio_thread()"
    );
}

#[test]
fn test_oversample_change_triggers_restart_protocol() {
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();

    let n = 256;
    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: n as u32,
        max_frames_count: n as u32,
    };

    let stopped = instance
        .activate(|_, _| make_harness_audio_processor(&state), audio_config)
        .expect("activate failed");
    let mut started = stopped.start_processing().expect("start_processing failed");

    let event = ParamValueEvent::new(
        0u32,
        ClapId::new(PARAM_OVERSAMPLE),
        Pckn::match_all(),
        1.0,
        Cookie::empty(),
    );
    let mut event_buffer = EventBuffer::new();
    event_buffer.push(&event);
    let input_events = InputEvents::from_buffer(&event_buffer);

    let mut il = vec![0.3f32; n];
    let mut ir = vec![0.3f32; n];
    let mut ol = vec![0.0f32; n];
    let mut or = vec![0.0f32; n];
    let _ = process_block_harness(
        &mut started,
        &mut il,
        &mut ir,
        &mut ol,
        &mut or,
        Some(&input_events),
    );

    assert!(
        state.restart_requested.load(Ordering::SeqCst),
        "Plugin must call request_restart() when oversampling changes during active processing"
    );

    state.assert_event_occurred("RestartRequested", |e| {
        matches!(e, HostEvent::RestartRequested)
    });

    drop(started);
}

#[test]
fn test_restart_cycle_clean() {
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
    let audio_config = default_audio_config();

    let stopped = instance
        .activate(|_, _| make_harness_audio_processor(&state), audio_config)
        .expect("activate failed");
    let started = stopped.start_processing().expect("start_processing failed");

    let mut started_after = perform_restart(&mut instance, started, &state, audio_config);

    let n = 256;
    let mut il = vec![0.3f32; n];
    let mut ir = vec![0.3f32; n];
    let mut ol = vec![0.0f32; n];
    let mut or = vec![0.0f32; n];
    let _ = process_block_harness(&mut started_after, &mut il, &mut ir, &mut ol, &mut or, None);

    assert!(
        !state.restart_requested.load(Ordering::SeqCst),
        "restart_requested should be cleared after perform_restart()"
    );
}

/// Oversample engine group-delay contribution (host-rate samples): Off = 0,
/// X2 = 12 (one half-band stage), X4 = 24 (two cascaded stages).
const OS_LATENCY_2X: u32 = 12;
const OS_LATENCY_4X: u32 = 24;

/// Reads the latency the host would receive via `PluginLatency::get()`.
fn plugin_latency_get(instance: &mut PluginInstance<CompleteHost>) -> u32 {
    let mut handle = instance.plugin_handle();
    let ext = handle
        .get_extension::<clack_extensions::latency::PluginLatency>()
        .expect("PluginLatency extension must be registered");
    ext.get(&mut handle)
}

/// Sends an oversampling parameter event through one audio block (host-event
/// path), mirroring `test_oversample_change_triggers_restart_protocol`.
fn send_oversample_request(
    started: &mut StartedPluginAudioProcessor<CompleteHost>,
    factor: OversampleFactor,
) {
    let event = ParamValueEvent::new(
        0u32,
        ClapId::new(PARAM_OVERSAMPLE),
        Pckn::match_all(),
        factor.to_f32() as f64,
        Cookie::empty(),
    );
    let mut event_buffer = EventBuffer::new();
    event_buffer.push(&event);
    let input_events = InputEvents::from_buffer(&event_buffer);
    let mut il = vec![0.3f32; 256];
    let mut ir = vec![0.3f32; 256];
    let mut ol = vec![0.0f32; 256];
    let mut or = vec![0.0f32; 256];
    let _ = process_block_harness(
        started,
        &mut il,
        &mut ir,
        &mut ol,
        &mut or,
        Some(&input_events),
    );
}

/// Processes one audio block with no input events (used to drain the SPSC).
fn process_block_silent(started: &mut StartedPluginAudioProcessor<CompleteHost>) {
    let mut il = vec![0.3f32; 256];
    let mut ir = vec![0.3f32; 256];
    let mut ol = vec![0.0f32; 256];
    let mut or = vec![0.0f32; 256];
    let _ = process_block_harness(started, &mut il, &mut ir, &mut ol, &mut or, None);
}

/// Asserts the value a host would read (`PluginLatency::get()`) equals the
/// physical latency of the active filter chain: stream + cabsim + OS delay.
/// A removed notification or a diverging announced value fails the test.
fn assert_reported_latency_matches_physical(
    shared: &crate::clap::plugin::NamClapShared,
    instance: &mut PluginInstance<CompleteHost>,
    expected_os_latency: u32,
) {
    let reported = plugin_latency_get(instance);
    let stream = shared.cold.current_stream_latency.load(Ordering::Relaxed);
    let cabsim = shared.cold.current_cabsim_latency.load(Ordering::Relaxed);
    assert_eq!(
        reported,
        stream + cabsim + expected_os_latency,
        "announced latency must equal the physical filter-chain latency \
         (stream={stream} + cabsim={cabsim} + os={expected_os_latency})"
    );
    assert_eq!(
        shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
        reported,
        "PluginLatency::get() must match the published current_latency"
    );
}

/// Deterministic CLAP latency notification harness.
///
/// Induces real structural transitions (OS Off→2x→4x, a same-latency model
/// swap, a latency-changing 44.1k model over a 48k host) and asserts:
///   - `HostLatency::changed()` fires exactly once per applied latency change;
///   - no notification fires while a restart is pending, for a same-latency
///     swap, for a coalesced round-trip, or for repeated housekeeping;
///   - the announced value (`PluginLatency::get()`) equals the physical
///     filter-chain latency after every transition.
#[test]
fn test_latency_changed_notification() {
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
    let audio_config = default_audio_config();
    let shared = unsafe { &*extract_plugin_shared(&mut instance) };

    let stopped = instance
        .activate(|_, _| make_harness_audio_processor(&state), audio_config)
        .expect("activate failed");
    let mut started = stopped.start_processing().expect("start_processing failed");

    // 48 kHz host, no model/IR, OS Off → 0 latency, no notification.
    instance.call_on_main_thread_callback();
    assert_eq!(
        state.latency_changed_count.load(Ordering::SeqCst),
        0,
        "initial 0-sample latency must not notify"
    );
    assert_eq!(plugin_latency_get(&mut instance), 0);

    // ── Off → X2 ──────────────────────────────────────────────────────────
    send_oversample_request(&mut started, OversampleFactor::X2);
    assert!(
        state.restart_requested.load(Ordering::SeqCst),
        "OS change must request a host restart"
    );
    assert_eq!(
        PendingRestartOs::load(&shared.cold.pending_restart_os_factor, Ordering::Relaxed),
        PendingRestartOs::Pending(OversampleFactor::X2)
    );
    assert_eq!(
        shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
        0,
        "latency must not change while the restart is pending"
    );
    assert_eq!(
        state.latency_changed_count.load(Ordering::SeqCst),
        0,
        "no notification while the restart is pending"
    );

    let mut started = perform_restart(&mut instance, started, &state, audio_config);
    instance.call_on_main_thread_callback();
    assert_eq!(
        shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
        OS_LATENCY_2X
    );
    assert_reported_latency_matches_physical(shared, &mut instance, OS_LATENCY_2X);
    assert_eq!(
        state.latency_changed_count.load(Ordering::SeqCst),
        1,
        "exactly one LatencyChanged after the X2 restart"
    );

    // ── X2 → X4 ───────────────────────────────────────────────────────────
    send_oversample_request(&mut started, OversampleFactor::X4);
    assert!(state.restart_requested.load(Ordering::SeqCst));
    assert_eq!(
        PendingRestartOs::load(&shared.cold.pending_restart_os_factor, Ordering::Relaxed),
        PendingRestartOs::Pending(OversampleFactor::X4)
    );
    assert_eq!(
        shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
        OS_LATENCY_2X,
        "latency must stay at X2 while X4 is pending"
    );
    assert_eq!(state.latency_changed_count.load(Ordering::SeqCst), 1);

    let mut started = perform_restart(&mut instance, started, &state, audio_config);
    instance.call_on_main_thread_callback();
    assert_eq!(
        shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
        OS_LATENCY_4X
    );
    assert_reported_latency_matches_physical(shared, &mut instance, OS_LATENCY_4X);
    assert_eq!(
        state.latency_changed_count.load(Ordering::SeqCst),
        2,
        "exactly two LatencyChanged after the X4 restart"
    );

    // ── Coalescing: a burst that ends at the current factor must not notify ──
    send_oversample_request(&mut started, OversampleFactor::X2);
    send_oversample_request(&mut started, OversampleFactor::X4);
    send_oversample_request(&mut started, OversampleFactor::X2);
    send_oversample_request(&mut started, OversampleFactor::X4);
    assert_eq!(
        PendingRestartOs::load(&shared.cold.pending_restart_os_factor, Ordering::Relaxed),
        PendingRestartOs::Pending(OversampleFactor::X4),
        "latest-wins coalescing must leave the X4 factor pending"
    );
    let mut started = perform_restart(&mut instance, started, &state, audio_config);
    instance.call_on_main_thread_callback();
    assert_eq!(
        state.latency_changed_count.load(Ordering::SeqCst),
        2,
        "a coalesced round-trip landing on the current factor must not notify"
    );

    // ── Same-latency model swap (48k over 48k): continuous, silent ─────────
    let mt = unsafe { &mut *extract_plugin_main_thread(&mut instance) };
    let base = model_path("lstm.nam");
    assert!(base.exists(), "lstm.nam fixture missing");
    mt.load_model(&base).expect("load same-rate model");
    assert!(
        !state.restart_requested.load(Ordering::SeqCst),
        "same-latency model swap must not request a restart"
    );
    assert!(
        mt.staged_swap.is_none(),
        "same-latency swap must not be staged"
    );
    process_block_silent(&mut started);
    instance.call_on_main_thread_callback();
    assert_eq!(
        state.latency_changed_count.load(Ordering::SeqCst),
        2,
        "a model swap that keeps the latency unchanged must not notify"
    );
    assert_reported_latency_matches_physical(shared, &mut instance, OS_LATENCY_4X);

    // Prove the continuous swap landed on the audio thread: the installed
    // model generation must be 1 (the 48k model), with zero latency change.
    let stopped = started.stop_processing();
    instance.deactivate(stopped);
    let guard = shared.cold.deactivated_dsp.lock().unwrap();
    let deact = guard.as_ref().expect("deactivated state missing");
    assert_eq!(
        deact.model_generation, 1,
        "the same-latency 48k model must be installed on the audio thread"
    );
    drop(guard);
    let stopped = instance
        .activate(|_, _| make_harness_audio_processor(&state), audio_config)
        .expect("reactivate after same-latency swap failed");
    started = stopped
        .start_processing()
        .expect("restart processing failed");

    // ── 44.1k model over 48k host: latency-changing → staged + one notify ──
    let model_44k = write_model_with_rate(&base, 44100);
    let stream_44k_latency = crate::clap::plugin::build_stream_adapter(48000, 44100, 256)
        .expect("build 44.1k stream adapter")
        .latency_samples();
    assert!(
        stream_44k_latency > 0,
        "44.1k over 48k must add stream latency"
    );

    mt.load_model(&model_44k).expect("load 44.1k model");
    assert!(
        state.restart_requested.load(Ordering::SeqCst),
        "rate-changing model load must request a host restart"
    );
    assert!(
        mt.staged_swap
            .as_ref()
            .and_then(|s| s.model.as_ref())
            .is_some(),
        "44.1k model must be staged for the restart"
    );
    assert_eq!(
        shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
        OS_LATENCY_4X,
        "latency must stay at X4 while the model restart is pending"
    );
    assert_eq!(state.latency_changed_count.load(Ordering::SeqCst), 2);
    instance.call_on_main_thread_callback();
    assert_eq!(
        state.latency_changed_count.load(Ordering::SeqCst),
        2,
        "no notification while the latency-changing swap is pending"
    );

    let expected_latency = OS_LATENCY_4X + stream_44k_latency;
    let _started = perform_restart(&mut instance, started, &state, audio_config);
    instance.call_on_main_thread_callback();
    assert_eq!(
        shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
        expected_latency
    );
    assert_reported_latency_matches_physical(shared, &mut instance, OS_LATENCY_4X);
    assert_eq!(
        state.latency_changed_count.load(Ordering::SeqCst),
        3,
        "exactly three LatencyChanged across the three latency transitions"
    );

    // ── No spam: repeated housekeeping without a latency change ────────────
    for _ in 0..4 {
        instance.call_on_main_thread_callback();
    }
    assert_eq!(
        state.latency_changed_count.load(Ordering::SeqCst),
        3,
        "housekeeping without a latency change must not spam notifications"
    );

    // ── Ordering invariant: every LatencyChanged is preceded by a restart ──
    let mut restarts = 0u32;
    let mut changes = 0u32;
    for event in state.snapshot() {
        match event {
            HostEvent::RestartRequested => restarts += 1,
            HostEvent::LatencyChanged => {
                changes += 1;
                assert!(
                    restarts >= changes,
                    "LatencyChanged must be preceded by a RestartRequested"
                );
            }
            _ => {}
        }
    }
    assert_eq!(
        changes, 3,
        "exactly one LatencyChanged per applied latency transition"
    );

    let _ = std::fs::remove_file(&model_44k);
}

#[test]
fn test_tail_changed_on_cabsim_load() {
    let state = CompleteHostState::new();
    let mut ap = CompleteHostAudioProcessor::new(&state);

    // Verify HostTailImpl records events and increments counter
    ap.changed();
    ap.changed();

    state.assert_event_occurred("TailChanged", |e| matches!(e, HostEvent::TailChanged));
    assert_eq!(state.tail_changed_count.load(Ordering::SeqCst), 2);

    // Note: Full integration test (IR load via SPSC → HostTail::changed())
    // requires a model fixture and the LoadCabIr command path through the
    // SPSC channel. The harness infrastructure itself is validated here.
}

#[test]
fn test_command_queue_no_overflow_under_automation_burst() {
    use crate::clap::extensions::params::{PARAM_GATE_THRESH, PARAM_INPUT_GAIN, PARAM_OUTPUT_GAIN};

    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
    let n = 64;
    let audio_config = PluginAudioConfiguration {
        sample_rate: 48000.0,
        min_frames_count: n as u32,
        max_frames_count: n as u32,
    };

    let stopped = instance
        .activate(|_, _| make_harness_audio_processor(&state), audio_config)
        .expect("activate failed");
    let mut started = stopped.start_processing().expect("start_processing failed");

    let mut event_buffer = EventBuffer::new();
    for i in 0..50 {
        let param_id = [PARAM_INPUT_GAIN, PARAM_OUTPUT_GAIN, PARAM_GATE_THRESH][i % 3];
        let value = (i as f64) / 50.0;
        event_buffer.push(&ParamValueEvent::new(
            i as u32,
            ClapId::new(param_id),
            Pckn::match_all(),
            value,
            Cookie::empty(),
        ));
    }
    let input_events = InputEvents::from_buffer(&event_buffer);

    let mut il = vec![0.3f32; n];
    let mut ir = vec![0.3f32; n];
    let mut ol = vec![0.0f32; n];
    let mut or = vec![0.0f32; n];
    let _ = process_block_harness(
        &mut started,
        &mut il,
        &mut ir,
        &mut ol,
        &mut or,
        Some(&input_events),
    );
}

#[test]
fn test_full_lifecycle_smoke() {
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
    let audio_config = default_audio_config();

    let stopped = instance
        .activate(|_, _| make_harness_audio_processor(&state), audio_config)
        .expect("activate failed");
    let mut started = stopped.start_processing().expect("start_processing failed");

    let n = 256;
    for _ in 0..4 {
        let mut il = vec![0.3f32; n];
        let mut ir = vec![0.3f32; n];
        let mut ol = vec![0.0f32; n];
        let mut or = vec![0.0f32; n];
        let _ = process_block_harness(&mut started, &mut il, &mut ir, &mut ol, &mut or, None);
    }

    let stopped = started.stop_processing();
    instance.deactivate(stopped);

    let events = state.snapshot();
    assert!(
        !events.is_empty(),
        "Harness should have recorded events during lifecycle.\nEvents: {events:#?}"
    );
}

#[test]
fn test_host_log_captures_plugin_messages() {
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
    let audio_config = default_audio_config();

    let stopped = instance
        .activate(|_, _| make_harness_audio_processor(&state), audio_config)
        .expect("activate failed");
    let mut started = stopped.start_processing().expect("start_processing failed");

    let n = 256;
    let mut il = vec![0.3f32; n];
    let mut ir = vec![0.3f32; n];
    let mut ol = vec![0.0f32; n];
    let mut or = vec![0.0f32; n];
    let _ = process_block_harness(&mut started, &mut il, &mut ir, &mut ol, &mut or, None);

    drop(started);

    state.assert_event_occurred("PluginLog", |e| matches!(e, HostEvent::PluginLog { .. }));
}

#[test]
fn test_dual_instance_harness() {
    let (_entry_a, _host_info_a, mut inst_a, state_a) = make_test_plugin_with_harness();
    let (_entry_b, _host_info_b, mut inst_b, state_b) = make_test_plugin_with_harness();

    let audio_config = default_audio_config();
    let stopped_a = inst_a
        .activate(|_, _| make_harness_audio_processor(&state_a), audio_config)
        .expect("activate A failed");
    let stopped_b = inst_b
        .activate(|_, _| make_harness_audio_processor(&state_b), audio_config)
        .expect("activate B failed");

    let mut started_a = stopped_a.start_processing().expect("start A failed");
    let mut started_b = stopped_b.start_processing().expect("start B failed");

    let n = 256;
    for _ in 0..2 {
        let mut il = vec![0.3f32; n];
        let mut ir = vec![0.3f32; n];
        let mut ol = vec![0.0f32; n];
        let mut or = vec![0.0f32; n];
        let _ = process_block_harness(&mut started_a, &mut il, &mut ir, &mut ol, &mut or, None);
    }
    for _ in 0..2 {
        let mut il = vec![0.5f32; n];
        let mut ir = vec![0.5f32; n];
        let mut ol = vec![0.0f32; n];
        let mut or = vec![0.0f32; n];
        let _ = process_block_harness(&mut started_b, &mut il, &mut ir, &mut ol, &mut or, None);
    }

    let stopped_a = started_a.stop_processing();
    let stopped_b = started_b.stop_processing();
    inst_a.deactivate(stopped_a);
    inst_b.deactivate(stopped_b);

    assert!(!state_a.snapshot().is_empty());
    assert!(!state_b.snapshot().is_empty());
}
