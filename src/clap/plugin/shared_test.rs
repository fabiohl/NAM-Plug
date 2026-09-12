// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use crate::clap::plugin::shared::{
    ColdShared, GuiSharedState, NamClapShared, RENDER_MODE_REALTIME, RtToUi, UiToRt,
};
use std::sync::atomic::Ordering;

pub(crate) fn make_test_shared() -> NamClapShared {
    use neural_amp_modeler_rs::common::spsc::{GcOverflowBuffer, RtStatusFlags};
    use rtrb::RingBuffer;
    use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32};
    use std::sync::{Arc, Mutex};

    let (param_tx, param_rx) = RingBuffer::new(8);
    let (gc_tx, gc_rx) = RingBuffer::new(32);
    let (slimmable_tx, slimmable_rx) = RingBuffer::new(4);

    let gui_state = Arc::new(GuiSharedState {
        rt_to_ui: RtToUi {
            ui_peak_l: AtomicU32::new(0.0f32.to_bits()),
            ui_peak_r: AtomicU32::new(0.0f32.to_bits()),
            ui_clipped: AtomicBool::new(false),
            ui_clip_indicator: AtomicBool::new(false),
            ui_gate_active: AtomicBool::new(false),
            current_latency: AtomicU32::new(0),
            cabsim_tail_samples: AtomicU32::new(0),
            active_channel_count: AtomicU32::new(1),
        },
        ui_to_rt: UiToRt {
            param_input_gain: AtomicU32::new(0.0f32.to_bits()),
            param_output_gain: AtomicU32::new(0.0f32.to_bits()),
            param_gate_thresh: AtomicU32::new((-90.0f32).to_bits()),
            param_bypass: AtomicU32::new(0),
            param_adaptive_compute: AtomicU32::new(1),
            param_slim_override: AtomicU32::new(0),
            param_oversample: AtomicU32::new(0),
            param_activation: AtomicU32::new(1), // Standard (exact-grade) by default
            gesture_flags: AtomicU32::new(0),
            gui_param_generation: AtomicU32::new(0),
            host_r_deactivated: AtomicBool::new(false),
        },
        cold: ColdShared {
            instance_id: 1,
            param_tx: Mutex::new(Some(param_tx)),
            param_rx: Mutex::new(Some(param_rx)),
            gc_tx: Mutex::new(Some(gc_tx)),
            gc_rx: Mutex::new(Some(gc_rx)),
            gc_overflow: Arc::new(GcOverflowBuffer::new(
                neural_amp_modeler_rs::common::spsc::SPSC_CAPACITY,
            )),
            rt_status: Arc::new(RtStatusFlags::new()),
            model_sample_rate: AtomicU32::new(48000),
            sample_rate: AtomicU32::new(44100),
            buffer_size: AtomicU32::new(0),
            current_stream_latency: AtomicU32::new(0),
            current_cabsim_latency: AtomicU32::new(0),
            track_accent_color: AtomicU32::new(0),
            param_indication: [
                AtomicU8::new(0),
                AtomicU8::new(0),
                AtomicU8::new(0),
                AtomicU8::new(0),
                AtomicU8::new(0),
                AtomicU8::new(0),
                AtomicU8::new(0),
                AtomicU8::new(0),
                AtomicU8::new(0),
            ],
            param_indication_color: [
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
            ],
            model_load_counter: AtomicU32::new(0),
            model_generation: std::sync::atomic::AtomicU64::new(0),
            ui_model_name: Mutex::new(String::new()),
            ui_model_metadata: Mutex::new(None),
            ui_pending_model: Mutex::new(None),
            ui_loading: AtomicBool::new(false),
            ui_load_error: AtomicBool::new(false),
            ui_load_error_msg: Mutex::new(String::new()),
            ui_model_info: Mutex::new(None),
            alive_fence: Arc::new(AtomicBool::new(true)),
            gui_user_closed: AtomicBool::new(false),
            render_mode: AtomicU32::new(RENDER_MODE_REALTIME),
            gui_scale_factor: AtomicU32::new(0),
            ir_path: Mutex::new(None),
            ir_hash: Mutex::new(None),
            ui_pending_ir: Mutex::new(None),
            ui_ir_loading: AtomicBool::new(false),
            ui_ir_load_error: AtomicBool::new(false),
            ui_ir_load_error_msg: Mutex::new(String::new()),
            ui_clear_ir: AtomicBool::new(false),
            ui_clear_model: AtomicBool::new(false),
            ir_raw_samples: Mutex::new(None),
            ir_raw_sample_rate: AtomicU32::new(0),
            slimmable_tx: Mutex::new(Some(slimmable_tx)),
            slimmable_rx: Mutex::new(Some(slimmable_rx)),
            requested_slimmable_generation: std::sync::atomic::AtomicU64::new(0),
            slimmable_stale_discarded_total: AtomicU32::new(0),
            full_wavenet_model: Mutex::new(None),
            cmd_next_seq: std::sync::atomic::AtomicU64::new(0),
            cmd_last_ack: std::sync::atomic::AtomicU64::new(0),
            last_applied_generation: std::sync::atomic::AtomicU64::new(0),
            pending_restart_os_factor: AtomicU32::new(0),
            in_flight_params: Mutex::new(None),
            pending_preset_load: Mutex::new(std::collections::VecDeque::new()),
            pending_model: Mutex::new(None),
            deactivated_dsp: Mutex::new(None),
            dialog_state: None,
            ir_dialog_state: None,
            dialog_handle_sink: Mutex::new(None),
            ir_dialog_handle_sink: Mutex::new(None),
            host_log_sink: Mutex::new(None),
        },
    });

    NamClapShared { gui: gui_state }
}

#[cfg(test)]
mod layout_tests {
    use crate::clap::plugin::shared::GuiSharedState;

    #[test]
    fn rt_to_ui_and_ui_to_rt_in_separate_cache_lines() {
        let off_rt = std::mem::offset_of!(GuiSharedState, rt_to_ui);
        let off_ui = std::mem::offset_of!(GuiSharedState, ui_to_rt);
        let distance = off_ui.wrapping_sub(off_rt);
        assert!(
            distance >= 128,
            "RtToUi and UiToRt must not share a 128-byte cache line: offset(RtToUi)={off_rt}, offset(UiToRt)={off_ui}, distance={distance}"
        );
    }

    #[test]
    fn ui_to_rt_and_cold_in_separate_cache_lines() {
        let off_ui = std::mem::offset_of!(GuiSharedState, ui_to_rt);
        let off_cold = std::mem::offset_of!(GuiSharedState, cold);
        let distance = off_cold.wrapping_sub(off_ui);
        assert!(
            distance >= 128,
            "UiToRt and Cold must not share a 128-byte cache line: offset(UiToRt)={off_ui}, offset(Cold)={off_cold}, distance={distance}"
        );
    }
}

// ---------------------------------------------------------------------------
// T6.3 — GUI events fail-closed (no silent `try_push` discard)
// ---------------------------------------------------------------------------

use clack_host::prelude::EventBuffer;
use clack_plugin::events::UnknownEvent;
use clack_plugin::events::event_types::{
    ParamGestureBeginEvent, ParamGestureEndEvent, ParamValueEvent,
};
use clack_plugin::events::io::{OutputEventBuffer, TryPushError};
use clack_plugin::prelude::{InputEvents, OutputEvents};

const CHANGED_SHIFT: u32 = 0;
const BEGIN_SHIFT: u32 = 1;
const END_SHIFT: u32 = 2;

/// CLAP ids of the 8 writable parameters (PARAM_ACTIVE_MODEL = 4 is read-only
/// and is not part of the GUI gesture set).
const WRITABLE_PARAM_IDS: [u32; 8] = [0, 1, 2, 3, 5, 6, 7, 8];

/// Host-side output queue with a hard capacity limit, simulating a real host
/// whose output event buffer is full. `capacity == 0` rejects every push.
struct LimitedOutput {
    inner: EventBuffer,
    capacity: usize,
}

impl LimitedOutput {
    fn new(capacity: usize) -> Self {
        Self {
            inner: EventBuffer::new(),
            capacity,
        }
    }
}

impl OutputEventBuffer for LimitedOutput {
    fn try_push(&mut self, event: &UnknownEvent) -> Result<(), TryPushError> {
        if self.inner.len() as usize >= self.capacity {
            return Err(TryPushError::new());
        }
        self.inner.push(event);
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum EvKind {
    Begin,
    Value,
    End,
}

#[derive(Debug, Clone)]
struct EvRec {
    kind: EvKind,
    param: u32,
    value: Option<f64>,
}

fn record_events(buf: &EventBuffer) -> Vec<EvRec> {
    let mut out = Vec::new();
    for event in &InputEvents::from_buffer(buf) {
        if let Some(begin) = event.as_event::<ParamGestureBeginEvent>()
            && let Some(id) = begin.param_id()
        {
            out.push(EvRec {
                kind: EvKind::Begin,
                param: id.get(),
                value: None,
            });
        } else if let Some(val) = event.as_event::<ParamValueEvent>()
            && let Some(id) = val.param_id()
        {
            out.push(EvRec {
                kind: EvKind::Value,
                param: id.get(),
                value: Some(val.value()),
            });
        } else if let Some(end) = event.as_event::<ParamGestureEndEvent>()
            && let Some(id) = end.param_id()
        {
            out.push(EvRec {
                kind: EvKind::End,
                param: id.get(),
                value: None,
            });
        }
    }
    out
}

/// Validates the CLAP gesture protocol per parameter across the whole
/// delivered sequence: `begin → value* → end`, no orphan `end`, no `begin`
/// while a gesture is already active, and no unterminated gesture.
fn assert_legal_gesture_order(records: &[EvRec]) {
    // Indices 0..=8 = the 9 CLAP params; index 4 (ACTIVE_MODEL) is never used.
    let mut state = [0u8; 9];
    for rec in records {
        let p = rec.param as usize;
        assert!(p <= 8, "unexpected param_id {}", rec.param);
        match rec.kind {
            EvKind::Begin => {
                assert_eq!(
                    state[p], 0,
                    "begin for param {p} while a gesture is already active"
                );
                state[p] = 1;
            }
            EvKind::Value => {
                assert_eq!(
                    state[p], 1,
                    "value for param {p} without an active gesture (begin not delivered)"
                );
            }
            EvKind::End => {
                assert_eq!(
                    state[p], 1,
                    "orphan end for param {p}: end delivered without a preceding begin"
                );
                state[p] = 0;
            }
        }
    }
    for (p, s) in state.iter().enumerate() {
        assert_eq!(
            *s, 0,
            "param {p} left an unterminated gesture (end never delivered)"
        );
    }
}

fn arms_gesture(shared: &NamClapShared, param_id: u32, value: f32) {
    shared.set_gesture(param_id as usize, BEGIN_SHIFT);
    shared.set_gesture(param_id as usize, CHANGED_SHIFT);
    shared.set_gesture(param_id as usize, END_SHIFT);
    let atomic = match param_id {
        0 => &shared.ui_to_rt.param_input_gain,
        1 => &shared.ui_to_rt.param_output_gain,
        2 => &shared.ui_to_rt.param_gate_thresh,
        3 => &shared.ui_to_rt.param_bypass,
        5 => &shared.ui_to_rt.param_adaptive_compute,
        6 => &shared.ui_to_rt.param_slim_override,
        7 => &shared.ui_to_rt.param_oversample,
        8 => &shared.ui_to_rt.param_activation,
        _ => unreachable!("unexpected param id {param_id}"),
    };
    atomic.store(value.to_bits(), Ordering::Relaxed);
}

fn gesture_flags_clear(shared: &NamClapShared) -> bool {
    shared.ui_to_rt.gesture_flags.load(Ordering::Relaxed) == 0
}

fn flush_until_drained(
    shared: &NamClapShared,
    mock: &mut LimitedOutput,
    max_flushes: usize,
) -> Vec<EvRec> {
    let mut all = Vec::new();
    for _ in 0..max_flushes {
        if gesture_flags_clear(shared) {
            break;
        }
        {
            let mut out = OutputEvents::from_buffer(mock);
            shared.write_gui_events(&mut out);
        }
        all.extend(record_events(&mock.inner));
        mock.inner.clear();
    }
    assert!(
        gesture_flags_clear(shared),
        "gesture flags not drained within {max_flushes} flushes"
    );
    all
}

#[test]
fn test_gui_events_capacity_0_retains_every_bit_and_recovers() {
    let shared = make_test_shared();

    for &pid in &WRITABLE_PARAM_IDS {
        arms_gesture(&shared, pid, 1.0 + pid as f32);
    }

    // Capacity 0: every push fails, nothing may be consumed.
    let mut mock = LimitedOutput::new(0);
    {
        let mut out = OutputEvents::from_buffer(&mut mock);
        shared.write_gui_events(&mut out);
    }
    assert!(
        mock.inner.is_empty(),
        "capacity-0 buffer must reject every event"
    );
    assert!(
        !gesture_flags_clear(&shared),
        "no gesture bit may be consumed while the host queue is full"
    );
    assert!(
        shared
            .cold
            .rt_status
            .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_GUI_EVENT_BACKPRESSURE),
        "backpressure flag must be raised when the host queue is full"
    );

    // Once the host provides room, one unbounded flush delivers everything
    // in legal order and clears every bit.
    let mut roomy = LimitedOutput::new(64);
    let records = flush_until_drained(&shared, &mut roomy, 10);
    assert_eq!(
        records.len(),
        24,
        "8 params x (begin, value, end) = 24 events"
    );
    assert_legal_gesture_order(&records);
    for rec in &records {
        if rec.kind == EvKind::Value {
            let expected = 1.0 + rec.param as f32;
            assert_eq!(
                rec.value,
                Some(expected as f64),
                "param {} value",
                rec.param
            );
        }
    }
}

#[test]
fn test_gui_events_capacity_1_delivers_all_in_legal_order() {
    let shared = make_test_shared();

    // Three sequential full gesture cycles over all 8 writable params,
    // each fully drained before the next cycle is armed (capacity 1).
    let mut mock = LimitedOutput::new(1);
    let mut all_records = Vec::new();
    for cycle in 0..3 {
        for &pid in &WRITABLE_PARAM_IDS {
            arms_gesture(&shared, pid, 10.0 * cycle as f32 + pid as f32);
        }
        let recs = flush_until_drained(&shared, &mut mock, 100);
        all_records.extend(recs);
    }

    assert_eq!(all_records.len(), 72, "8 params x 3 gestures x 3 events");
    assert_legal_gesture_order(&all_records);

    // Each cycle must deliver exactly one value per param, equal to the
    // value armed for that cycle (values read at delivery time).
    let values: Vec<f64> = all_records
        .iter()
        .filter(|r| r.kind == EvKind::Value)
        .map(|r| r.value.unwrap())
        .collect();
    assert_eq!(values.len(), 24);
    for cycle in 0..3 {
        for (i, &pid) in WRITABLE_PARAM_IDS.iter().enumerate() {
            let expected = (10.0 * cycle as f32 + pid as f32) as f64;
            assert_eq!(values[cycle * 8 + i], expected, "cycle {cycle} param {pid}");
        }
    }
}

#[test]
fn test_gui_events_value_coalesces_while_host_queue_full() {
    let shared = make_test_shared();

    arms_gesture(&shared, 0, 0.5);

    // Capacity 1: begin goes out, value and end are retained.
    let mut mock = LimitedOutput::new(1);
    {
        let mut out = OutputEvents::from_buffer(&mut mock);
        shared.write_gui_events(&mut out);
    }
    let phase1 = record_events(&mock.inner);
    mock.inner.clear();
    assert_eq!(phase1.len(), 1);
    assert_eq!(phase1[0].kind, EvKind::Begin);

    // While the queue is still saturated, the GUI writes several new values.
    for v in [1.0f32, 2.0, 3.0] {
        shared
            .ui_to_rt
            .param_input_gain
            .store(v.to_bits(), Ordering::Relaxed);
        shared.set_gesture(0, CHANGED_SHIFT);
    }

    {
        let mut out = OutputEvents::from_buffer(&mut mock);
        shared.write_gui_events(&mut out);
    }
    let phase2 = record_events(&mock.inner);
    mock.inner.clear();
    // Only the LATEST value may be delivered — earlier writes coalesced.
    assert_eq!(phase2.len(), 1);
    assert_eq!(phase2[0].kind, EvKind::Value);
    assert_eq!(phase2[0].value, Some(3.0));
    assert!(
        shared
            .cold
            .rt_status
            .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_GUI_EVENT_BACKPRESSURE),
        "value push failure must raise the backpressure flag"
    );

    let phase3 = flush_until_drained(&shared, &mut mock, 10);
    assert_eq!(phase3.len(), 1);
    assert_eq!(phase3[0].kind, EvKind::End);
    assert_legal_gesture_order(&[phase1[0].clone(), phase2[0].clone(), phase3[0].clone()]);
}

#[test]
fn test_gui_events_no_orphan_end_on_retry() {
    let shared = make_test_shared();

    arms_gesture(&shared, 0, 0.25);

    // Capacity 1: begin is delivered; value and end stay pending.
    let mut mock = LimitedOutput::new(1);
    {
        let mut out = OutputEvents::from_buffer(&mut mock);
        shared.write_gui_events(&mut out);
    }
    let phase1 = record_events(&mock.inner);
    mock.inner.clear();
    assert_eq!(phase1.len(), 1);
    assert_eq!(phase1[0].kind, EvKind::Begin);

    // A saturated flush must never emit `end` while `value` is still pending.
    let mut saturated = LimitedOutput::new(0);
    {
        let mut out = OutputEvents::from_buffer(&mut saturated);
        shared.write_gui_events(&mut out);
    }
    assert!(
        saturated.inner.is_empty(),
        "end must not be emitted while value is undelivered"
    );
    assert!(
        shared
            .cold
            .rt_status
            .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_GUI_EVENT_BACKPRESSURE)
    );

    let rest = flush_until_drained(&shared, &mut mock, 10);
    assert_eq!(rest.len(), 2);
    assert_eq!(rest[0].kind, EvKind::Value);
    assert_eq!(rest[1].kind, EvKind::End);
}
