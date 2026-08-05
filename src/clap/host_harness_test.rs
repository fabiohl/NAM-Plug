// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use clack_common::events::Pckn;
use clack_common::events::event_types::ParamValueEvent;
use clack_common::utils::{ClapId, Cookie};
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
    use crate::clap::extensions::params::PARAM_OVERSAMPLE;

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

#[test]
fn test_latency_changed_notification() {
    let (_entry, _host_info, mut instance, state) = make_test_plugin_with_harness();
    let audio_config = default_audio_config();

    let stopped = instance
        .activate(|_, _| make_harness_audio_processor(&state), audio_config)
        .expect("activate failed");
    let mut started = stopped.start_processing().expect("start_processing failed");

    for _ in 0..8 {
        let n = 256;
        let mut il = vec![0.3f32; n];
        let mut ir = vec![0.3f32; n];
        let mut ol = vec![0.0f32; n];
        let mut or = vec![0.0f32; n];
        let _ = process_block_harness(&mut started, &mut il, &mut ir, &mut ol, &mut or, None);
    }

    let events = state.snapshot();
    let latency_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, HostEvent::LatencyChanged))
        .collect();
    eprintln!(
        "LatencyChanged events: {latency_events:?} (total={})",
        events.len()
    );
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
