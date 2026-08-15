// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::dialog_state;
use super::dialog_state::{DialogSharedState, IrDialogSharedState};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[test]
fn test_dialog_state_sentinels_are_distinct() {
    let cancelled = dialog_state::dialog_cancelled_sentinel();
    let timedout = dialog_state::dialog_timedout_sentinel();
    assert_ne!(
        cancelled, timedout,
        "cancel and timeout sentinels must differ"
    );
    assert!(cancelled.to_string_lossy().contains("CANCELLED"));
    assert!(timedout.to_string_lossy().contains("TIMEDOUT"));
}

#[test]
fn test_dialog_state_active_flag() {
    let state = DialogSharedState::new();
    assert!(!state.active.load(Ordering::Relaxed));
    state.active.store(true, Ordering::Relaxed);
    assert!(state.active.load(Ordering::Relaxed));
    state.active.store(false, Ordering::Relaxed);
    assert!(!state.active.load(Ordering::Relaxed));
}

#[test]
fn test_ir_dialog_state_active_flag() {
    let state = IrDialogSharedState::new();
    assert!(!state.active.load(Ordering::Relaxed));
    state.active.store(true, Ordering::Relaxed);
    assert!(state.active.load(Ordering::Relaxed));
}

#[test]
fn test_dialog_fence_down_discards_outcome_and_notification() {
    // R-09: the fence drops while the picker is open (plugin destroyed).
    // The completed dialog must NOT write the path and must NOT notify
    // the host (request_callback would dispatch to a destroyed instance).
    let state = Arc::new(DialogSharedState::new());
    state.active.store(true, Ordering::Relaxed);
    let fence = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let notify_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let picker = move || {
        let _ = started_tx.send(());
        let _ = release_rx.recv();
        Some(PathBuf::from("/tmp/fake-model.nam"))
    };
    let notify = {
        let count = Arc::clone(&notify_count);
        move || {
            count.fetch_add(1, Ordering::Relaxed);
        }
    };

    let handle =
        super::spawn_file_dialog_inner(Arc::clone(&state), Arc::clone(&fence), picker, notify);

    // Wait until the picker is running, then destroy the plugin (fence
    // down) while the dialog is still open, and let it complete.
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    fence.store(false, Ordering::Release);
    release_tx.send(()).unwrap();
    handle.join().unwrap();

    assert!(
        state.pending_model.lock().unwrap().is_none(),
        "fence down ⇒ the picked path must NOT be written to pending_model"
    );
    assert_eq!(
        notify_count.load(Ordering::Relaxed),
        0,
        "fence down ⇒ zero request_callback must be observed"
    );
    assert!(!state.active.load(Ordering::Relaxed));
}

#[test]
fn test_dialog_fence_up_writes_path_and_notifies() {
    // Happy path: fence up — the outcome is written and the host is
    // notified exactly once.
    let state = Arc::new(DialogSharedState::new());
    state.active.store(true, Ordering::Relaxed);
    let fence = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let notify_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let picker = move || Some(PathBuf::from("/tmp/real-model.nam"));
    let notify = {
        let count = Arc::clone(&notify_count);
        move || {
            count.fetch_add(1, Ordering::Relaxed);
        }
    };

    let handle =
        super::spawn_file_dialog_inner(Arc::clone(&state), Arc::clone(&fence), picker, notify);
    handle.join().unwrap();

    assert_eq!(
        state.pending_model.lock().unwrap().as_deref(),
        Some(std::path::Path::new("/tmp/real-model.nam"))
    );
    assert_eq!(notify_count.load(Ordering::Relaxed), 1);
    assert!(!state.active.load(Ordering::Relaxed));
}

#[test]
fn test_ir_dialog_fence_down_discards_outcome_and_notification() {
    let state = Arc::new(IrDialogSharedState::new());
    state.active.store(true, Ordering::Relaxed);
    let fence = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let notify_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let picker = move || {
        let _ = started_tx.send(());
        let _ = release_rx.recv();
        Some(PathBuf::from("/tmp/fake-ir.wav"))
    };
    let notify = {
        let count = Arc::clone(&notify_count);
        move || {
            count.fetch_add(1, Ordering::Relaxed);
        }
    };

    let handle =
        super::spawn_ir_file_dialog_inner(Arc::clone(&state), Arc::clone(&fence), picker, notify);

    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    fence.store(false, Ordering::Release);
    release_tx.send(()).unwrap();
    handle.join().unwrap();

    assert!(
        state.pending_ir.lock().unwrap().is_none(),
        "fence down ⇒ the picked IR path must NOT be written to pending_ir"
    );
    assert_eq!(notify_count.load(Ordering::Relaxed), 0);
    assert!(!state.active.load(Ordering::Relaxed));
}
