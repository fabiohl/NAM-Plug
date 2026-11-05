// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

#[test]
fn test_quiescence_clean_when_idle() {
    let coordinator = HostLogQuiescence::new();
    assert!(coordinator.is_active());
    assert_eq!(coordinator.in_flight_count(), 0);

    let outcome = coordinator.quiesce(Duration::from_millis(50));
    assert_eq!(outcome, QuiescenceOutcome::Clean);
    assert!(!coordinator.is_active());
    assert_eq!(coordinator.in_flight_count(), 0);

    // Subsequent dispatches are rejected
    assert!(coordinator.enter().is_none());
}

#[test]
fn test_quiescence_rejects_new_dispatches_once_closed() {
    let coord = Arc::new(HostLogQuiescence::new());
    let guard = coord.enter().expect("should enter when active");
    assert_eq!(coord.in_flight_count(), 1);

    let coord_clone = Arc::clone(&coord);

    let handle = thread::spawn(move || coord_clone.quiesce(Duration::from_millis(50)));

    // Give the quiesce thread a moment to set active = false
    thread::sleep(Duration::from_millis(5));
    assert!(!coord.is_active());
    // New dispatch must be rejected immediately
    assert!(coord.enter().is_none());

    // Drop the held guard
    drop(guard);

    let outcome = handle.join().expect("thread join failed");
    assert_eq!(outcome, QuiescenceOutcome::Clean);
    assert_eq!(coord.in_flight_count(), 0);
}

#[test]
fn test_quiescence_waits_for_in_flight() {
    let coord = Arc::new(HostLogQuiescence::new());
    let coord_clone = Arc::clone(&coord);

    let guard = coord.enter().expect("should enter");

    let handle = thread::spawn(move || coord_clone.quiesce(Duration::from_millis(100)));

    // Hold guard for 20ms (well within the 100ms grace period)
    thread::sleep(Duration::from_millis(20));
    drop(guard);

    let outcome = handle.join().expect("join failed");
    assert_eq!(outcome, QuiescenceOutcome::Clean);
    assert_eq!(coord.in_flight_count(), 0);
}

#[test]
fn test_quiescence_stalled_dispatch() {
    // Reproduce a retained host callback that exceeds the grace period (D-16 / S07.3)
    let coord = Arc::new(HostLogQuiescence::new());
    let coord_clone = Arc::clone(&coord);

    let guard = coord.enter().expect("should enter");

    let handle = thread::spawn(move || {
        // Short grace period of 15ms
        coord_clone.quiesce(Duration::from_millis(15))
    });

    // Retain the callback for 50ms (longer than 15ms grace period)
    thread::sleep(Duration::from_millis(50));
    drop(guard);

    let outcome = handle.join().expect("join failed");
    match outcome {
        QuiescenceOutcome::StalledWait(dur) => {
            assert!(dur >= Duration::from_millis(40), "elapsed was {dur:?}");
        }
        QuiescenceOutcome::Clean => {
            panic!("Expected StalledWait since the guard was held longer than the grace period");
        }
    }
    assert_eq!(coord.in_flight_count(), 0);
}

#[test]
fn test_quiescence_concurrency() {
    let coord = Arc::new(HostLogQuiescence::new());
    let mut workers = Vec::new();

    for _ in 0..8 {
        let c = Arc::clone(&coord);
        workers.push(thread::spawn(move || {
            for _ in 0..500 {
                if let Some(_guard) = c.enter() {
                    thread::yield_now();
                }
            }
        }));
    }

    // Concurrently run quiesce
    thread::sleep(Duration::from_millis(2));
    let outcome = coord.quiesce(Duration::from_millis(50));
    assert!(matches!(
        outcome,
        QuiescenceOutcome::Clean | QuiescenceOutcome::StalledWait(_)
    ));

    for w in workers {
        w.join().expect("worker failed");
    }

    assert_eq!(coord.in_flight_count(), 0);
    assert!(!coord.is_active());
    assert!(coord.enter().is_none());
}

// ── HostCallQuiescence Tests (SP-06 / F-PLUG-02) ──────────────────────────

#[test]
fn test_host_call_clean_when_idle() {
    let fence = Arc::new(AtomicBool::new(true));
    let coordinator = HostCallQuiescence::new(Arc::clone(&fence));
    assert!(coordinator.is_active());
    assert_eq!(coordinator.in_flight_count(), 0);

    let res = coordinator.host_call(|| 42);
    assert_eq!(res, Some(42));
    assert_eq!(coordinator.in_flight_count(), 0);

    let outcome = coordinator.quiesce(Duration::from_millis(50));
    assert_eq!(outcome, QuiescenceOutcome::Clean);
    assert!(!coordinator.is_active());
    assert_eq!(coordinator.in_flight_count(), 0);

    // Subsequent calls rejected
    assert_eq!(coordinator.host_call(|| 100), None);
}

#[test]
fn test_host_call_rejected_when_fence_is_down() {
    let fence = Arc::new(AtomicBool::new(true));
    let coordinator = HostCallQuiescence::new(Arc::clone(&fence));

    // Initially true
    assert_eq!(coordinator.host_call(|| 1), Some(1));

    // Lower fence
    fence.store(false, Ordering::Release);

    let called = Arc::new(AtomicBool::new(false));
    let called_clone = Arc::clone(&called);
    let res = coordinator.host_call(move || {
        called_clone.store(true, Ordering::SeqCst);
    });

    assert_eq!(res, None);
    assert!(
        !called.load(Ordering::SeqCst),
        "Closure must not be executed when alive_fence is false"
    );
}

#[test]
fn test_host_call_rejected_after_quiesce() {
    let fence = Arc::new(AtomicBool::new(true));
    let coordinator = HostCallQuiescence::new(Arc::clone(&fence));

    let outcome = coordinator.quiesce(Duration::from_millis(50));
    assert_eq!(outcome, QuiescenceOutcome::Clean);

    let called = Arc::new(AtomicBool::new(false));
    let called_clone = Arc::clone(&called);
    let res = host_call(&coordinator, move || {
        called_clone.store(true, Ordering::SeqCst);
    });

    assert_eq!(res, None);
    assert!(
        !called.load(Ordering::SeqCst),
        "Closure must not be executed after quiesce has run"
    );
}

#[test]
fn test_host_call_quiesce_waits_for_in_flight() {
    let fence = Arc::new(AtomicBool::new(true));
    let coord = Arc::new(HostCallQuiescence::new(Arc::clone(&fence)));
    let coord_clone = Arc::clone(&coord);

    let call_started = Arc::new(AtomicBool::new(false));
    let call_started_clone = Arc::clone(&call_started);
    let allow_exit = Arc::new(AtomicBool::new(false));
    let allow_exit_clone = Arc::clone(&allow_exit);

    let call_thread = thread::spawn(move || {
        coord_clone.host_call(|| {
            call_started_clone.store(true, Ordering::Release);
            while !allow_exit_clone.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(2));
            }
            123
        })
    });

    // Wait until call enters
    while !call_started.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(coord.in_flight_count(), 1);

    // Concurrently invoke quiesce in another thread
    let coord_for_quiesce = Arc::clone(&coord);
    let quiesce_done = Arc::new(AtomicBool::new(false));
    let quiesce_done_clone = Arc::clone(&quiesce_done);
    let quiesce_thread = thread::spawn(move || {
        let outcome = coord_for_quiesce.quiesce(Duration::from_millis(100));
        quiesce_done_clone.store(true, Ordering::Release);
        outcome
    });

    // Ensure quiesce is waiting and has not finished while call is active
    thread::sleep(Duration::from_millis(20));
    assert!(
        !quiesce_done.load(Ordering::Acquire),
        "quiesce must not complete while in-flight call is held"
    );

    // Release the call
    allow_exit.store(true, Ordering::Release);

    let call_res = call_thread.join().expect("call thread join");
    assert_eq!(call_res, Some(123));

    let outcome = quiesce_thread.join().expect("quiesce thread join");
    assert_eq!(outcome, QuiescenceOutcome::Clean);
    assert_eq!(coord.in_flight_count(), 0);
}

#[test]
fn test_host_call_guard_decrements_on_panic() {
    let fence = Arc::new(AtomicBool::new(true));
    let coordinator = Arc::new(HostCallQuiescence::new(Arc::clone(&fence)));

    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        coordinator.host_call(|| {
            panic!("deliberate panic inside host_call");
        });
    }));

    // In-flight count must be 0 even after panic
    assert_eq!(coordinator.in_flight_count(), 0);

    // Subsequent host call can still enter
    let res = coordinator.host_call(|| 99);
    assert_eq!(res, Some(99));
}

#[test]
fn test_host_call_quiescence_concurrency() {
    let fence = Arc::new(AtomicBool::new(true));
    let coord = Arc::new(HostCallQuiescence::new(Arc::clone(&fence)));
    let counter = Arc::new(AtomicUsize::new(0));
    let mut workers = Vec::new();

    for _ in 0..6 {
        let c = Arc::clone(&coord);
        let cnt = Arc::clone(&counter);
        workers.push(thread::spawn(move || {
            for _ in 0..400 {
                if let Some(()) = c.host_call(|| {
                    cnt.fetch_add(1, Ordering::Relaxed);
                }) {
                    thread::yield_now();
                }
            }
        }));
    }

    thread::sleep(Duration::from_millis(3));
    fence.store(false, Ordering::Release);
    let outcome = coord.quiesce(Duration::from_millis(50));
    assert!(matches!(
        outcome,
        QuiescenceOutcome::Clean | QuiescenceOutcome::StalledWait(_)
    ));

    for w in workers {
        w.join().expect("worker failed");
    }

    assert_eq!(coord.in_flight_count(), 0);
    assert!(!coord.is_active());
    assert_eq!(coord.host_call(|| 1), None);
}
