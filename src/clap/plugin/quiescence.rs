// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Host log and host call quiescence coordinators ensuring strict memory safety (zero UAF)
//! (D-16 / S08.3 / E-11 / SP-06 / F-PLUG-02).
//!
//! Under the CLAP specification, the DAW host owns the `clap_host` instance. If `destroy`
//! finishes and the host deallocates its host handle or instance data while a foreign thread
//! is mid-call in `host_log.log()` or `request_callback()`, an immediate Use-After-Free (UAF) occurs.
//!
//! # Architecture and Safety Trade-Off (S08.3 / GAP-HOST-LOG-TEARDOWN-BOUND)
//!
//! Memory safety strictly takes precedence over bounded teardown:
//! 1. Immediate rejection: As soon as quiescence begins, any subsequent dispatch attempts
//!    are rejected immediately without blocking.
//! 2. Zero-CPU condition variable waiting: In-flight calls are waited on using a condition
//!    variable instead of a busy-spin loop.
//! 3. Mitigated wait with unbounded safety fallback: Teardown waits up to a 50 ms grace period.
//!    If a host callback remains stalled beyond this deadline, diagnostic
//!    `[E2130 | HOST_LOG_QUIESCENCE_STALLED]` or `[E2131 | HOST_CALL_QUIESCENCE_STALLED]` is emitted once,
//!    and the coordinator enters an unbounded wait until `in_flight == 0` to eliminate UAF.
//!    This architectural limitation is formally classified as GAP-HOST-LOG-TEARDOWN-BOUND:
//!    teardown cannot be strictly bounded in time without risking UAF under a deadlocked host.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Result of a quiescence operation.
#[derive(Debug, PartialEq, Eq)]
pub enum QuiescenceOutcome {
    /// Quiescence completed within the grace period (no host stalling).
    Clean,
    /// Quiescence exceeded the grace period because a foreign host callback was stalled,
    /// but eventually completed safely.
    StalledWait(Duration),
}

#[derive(Debug)]
struct QuiescenceState {
    active: bool,
    in_flight: usize,
}

/// Locks the quiescence state, recovering the guard from a poisoned mutex.
///
/// Returns the guard plus a flag telling whether the mutex was poisoned. The
/// caller must log after releasing the guard: no diagnostic is emitted here so
/// no logging happens while the state mutex is held.
fn lock_state(state: &Mutex<QuiescenceState>) -> (MutexGuard<'_, QuiescenceState>, bool) {
    match state.lock() {
        Ok(guard) => (guard, false),
        Err(poisoned) => (poisoned.into_inner(), true),
    }
}

/// Shared quiesce implementation for both coordinators.
///
/// Memory safety wins over bounded teardown (D1c): after the grace period the
/// stalled host is reported, but the wait continues until `in_flight == 0`.
/// Resources are therefore never freed while the host may still be executing a
/// call. A poisoned state mutex is recovered via `into_inner` and reported, so
/// it can neither leave `in_flight` stuck nor spin forever without a diagnostic.
fn quiesce_state(
    state: &Mutex<QuiescenceState>,
    cvar: &Condvar,
    grace_period: Duration,
    stall_tag: &str,
    poison_tag: &str,
    what: &str,
) -> QuiescenceOutcome {
    let start = Instant::now();
    let mut poisoned = false;
    let stalled;

    {
        let (mut guard, was_poisoned) = lock_state(state);
        poisoned |= was_poisoned;
        if was_poisoned {
            state.clear_poison();
        }
        guard.active = false;

        if guard.in_flight > 0 {
            while guard.in_flight > 0 {
                let elapsed = start.elapsed();
                if elapsed >= grace_period {
                    break;
                }
                let remaining = grace_period - elapsed;
                guard = match cvar.wait_timeout(guard, remaining) {
                    Ok((guard, _)) => guard,
                    Err(err) => {
                        poisoned = true;
                        let (guard, _) = err.into_inner();
                        state.clear_poison();
                        guard
                    }
                };
            }
        }

        if guard.in_flight == 0 {
            drop(guard);
            if poisoned {
                log::warn!(
                    "[{poison_tag}] {what} quiescence hit a poisoned state mutex; \
                     recovered the guard and completed cleanly."
                );
            }
            return QuiescenceOutcome::Clean;
        }
        stalled = guard.in_flight;
    }

    if poisoned {
        log::warn!(
            "[{poison_tag}] {what} quiescence hit a poisoned state mutex; \
             recovered the guard and is waiting for in-flight calls."
        );
    }
    log::warn!(
        "[{stall_tag}] {what} did not complete within {grace_period:?} during plugin teardown \
         (in_flight: {stalled}). Waiting for the call to complete to prevent Use-After-Free; \
         plugin resources are NOT freed while the host may still access them (D1c)."
    );

    {
        let (mut guard, was_poisoned) = lock_state(state);
        if was_poisoned {
            poisoned = true;
            state.clear_poison();
        }
        while guard.in_flight > 0 {
            guard = match cvar.wait(guard) {
                Ok(guard) => guard,
                Err(err) => {
                    poisoned = true;
                    let guard = err.into_inner();
                    state.clear_poison();
                    guard
                }
            };
        }
    }

    let total_wait = start.elapsed();
    if poisoned {
        log::warn!(
            "[{poison_tag}] {what} quiescence recovered from a poisoned state mutex before \
             completion."
        );
    }
    log::info!("{what} completed after {total_wait:?}; quiescence reached safely.");
    QuiescenceOutcome::StalledWait(total_wait)
}

/// Coordinator for host log sink quiescence.
#[derive(Debug)]
pub struct HostLogQuiescence {
    state: Mutex<QuiescenceState>,
    cvar: Condvar,
}

impl Default for HostLogQuiescence {
    fn default() -> Self {
        Self::new()
    }
}

impl HostLogQuiescence {
    /// Creates a new active quiescence coordinator.
    pub fn new() -> Self {
        Self {
            state: Mutex::new(QuiescenceState {
                active: true,
                in_flight: 0,
            }),
            cvar: Condvar::new(),
        }
    }

    /// Attempts to enter the host log dispatch critical section.
    ///
    /// Returns `Some(QuiescenceGuard)` if the coordinator is active, incrementing
    /// the in-flight reference counter.
    /// Returns `None` if quiescence has begun or the plugin is being destroyed,
    /// indicating that the host handle must not be accessed.
    pub fn enter(&self) -> Option<QuiescenceGuard<'_>> {
        let mut state = self.state.lock().ok()?;
        if !state.active {
            return None;
        }
        state.in_flight += 1;
        Some(QuiescenceGuard { coordinator: self })
    }

    /// Quiesces the coordinator, disallowing any future dispatches and waiting for
    /// all in-flight dispatches to finish (GAP-HOST-LOG-TEARDOWN-BOUND).
    pub fn quiesce(&self, grace_period: Duration) -> QuiescenceOutcome {
        quiesce_state(
            &self.state,
            &self.cvar,
            grace_period,
            "E2130 | HOST_LOG_QUIESCENCE_STALLED",
            "E2130 | HOST_LOG_QUIESCENCE_POISONED",
            "Host log callback",
        )
    }

    /// Returns `true` if the coordinator is still active and accepting dispatches.
    pub fn is_active(&self) -> bool {
        self.state.lock().map(|s| s.active).unwrap_or(false)
    }

    /// Returns the current number of in-flight dispatches.
    pub fn in_flight_count(&self) -> usize {
        self.state.lock().map(|s| s.in_flight).unwrap_or(0)
    }
}

/// RAII guard representing an active in-flight host log dispatch.
///
/// When dropped, decrements `in_flight` and notifies any thread waiting in `quiesce()`.
#[derive(Debug)]
pub struct QuiescenceGuard<'a> {
    coordinator: &'a HostLogQuiescence,
}

impl Drop for QuiescenceGuard<'_> {
    fn drop(&mut self) {
        let (mut state, poisoned) = lock_state(&self.coordinator.state);
        state.in_flight = state.in_flight.saturating_sub(1);
        let wake = state.in_flight == 0;
        drop(state);
        if wake {
            self.coordinator.cvar.notify_all();
        }
        if poisoned {
            self.coordinator.state.clear_poison();
            log::warn!(
                "[E2130 | HOST_LOG_QUIESCENCE_POISONED] Host log guard released a poisoned \
                 state mutex; in-flight count was decremented anyway."
            );
        }
    }
}

/// Coordinator for external host call quiescence from detachable threads (SP-06 / F-PLUG-02).
///
/// Under the CLAP specification, the DAW host owns the `clap_host` pointer and can deallocate
/// it or free plugin resources once `destroy()` returns. Any background thread
/// (`file_dialogs`, `GuiWorker`, `ModelLoader`, `SlintViewModel`) attempting to call the host
/// (`request_callback`, `request_flush`, `closed`) must enter this barrier.
///
/// Invariant: after teardown begins, all future calls are rejected immediately, and `destroy()`
/// waits until any currently in-flight host call completes before returning.
#[derive(Debug)]
pub struct HostCallQuiescence {
    state: Mutex<QuiescenceState>,
    cvar: Condvar,
    alive_fence: Arc<AtomicBool>,
}

impl HostCallQuiescence {
    /// Creates a new active host call quiescence coordinator bound to `alive_fence`.
    pub fn new(alive_fence: Arc<AtomicBool>) -> Self {
        Self {
            state: Mutex::new(QuiescenceState {
                active: true,
                in_flight: 0,
            }),
            cvar: Condvar::new(),
            alive_fence,
        }
    }

    /// Attempts to enter the host call critical section.
    ///
    /// Returns `Some(HostCallGuard)` if the coordinator is active AND `alive_fence` is true,
    /// incrementing the in-flight reference counter.
    /// Returns `None` if quiescence has begun or `alive_fence` is down.
    pub fn enter(&self) -> Option<HostCallGuard<'_>> {
        let mut state = self.state.lock().ok()?;
        if !state.active || !self.alive_fence.load(Ordering::Acquire) {
            return None;
        }
        state.in_flight += 1;
        Some(HostCallGuard { coordinator: self })
    }

    /// Executes closure `f` within the host call quiescence critical section.
    ///
    /// Returns `Some(R)` if entered and executed, or `None` if rejected due to teardown.
    pub fn host_call<R>(&self, f: impl FnOnce() -> R) -> Option<R> {
        let guard = self.enter()?;
        let res = f();
        drop(guard);
        Some(res)
    }

    /// Quiesces the coordinator, rejecting future calls and waiting for in-flight calls
    /// to finish (SP-06 / F-PLUG-02 / GAP-HOST-LOG-TEARDOWN-BOUND).
    pub fn quiesce(&self, grace_period: Duration) -> QuiescenceOutcome {
        quiesce_state(
            &self.state,
            &self.cvar,
            grace_period,
            "E2131 | HOST_CALL_QUIESCENCE_STALLED",
            "E2131 | HOST_CALL_QUIESCENCE_POISONED",
            "Host call",
        )
    }

    /// Returns `true` if the coordinator is still active and accepting host calls.
    pub fn is_active(&self) -> bool {
        self.state.lock().map(|s| s.active).unwrap_or(false)
    }

    /// Returns the current number of in-flight host calls.
    pub fn in_flight_count(&self) -> usize {
        self.state.lock().map(|s| s.in_flight).unwrap_or(0)
    }
}

/// RAII guard representing an active in-flight host call.
#[derive(Debug)]
pub struct HostCallGuard<'a> {
    coordinator: &'a HostCallQuiescence,
}

impl Drop for HostCallGuard<'_> {
    fn drop(&mut self) {
        let (mut state, poisoned) = lock_state(&self.coordinator.state);
        state.in_flight = state.in_flight.saturating_sub(1);
        let wake = state.in_flight == 0;
        drop(state);
        if wake {
            self.coordinator.cvar.notify_all();
        }
        if poisoned {
            self.coordinator.state.clear_poison();
            log::warn!(
                "[E2131 | HOST_CALL_QUIESCENCE_POISONED] Host call guard released a poisoned \
                 state mutex; in-flight count was decremented anyway."
            );
        }
    }
}

/// Convenience function executing `f` within the host call quiescence guard (T-06.2).
#[inline]
pub(crate) fn host_call<R>(q: &HostCallQuiescence, f: impl FnOnce() -> R) -> Option<R> {
    q.host_call(f)
}

#[cfg(test)]
#[path = "quiescence_test.rs"]
mod quiescence_test;
