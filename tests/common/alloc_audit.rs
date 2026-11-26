// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Shared CountingAllocator infrastructure for heap-audit integration tests.
//!
//! Provides a local `CountingAllocator` (used when `clap-plugin` is disabled),
//! `TrackingGuard` (RAII gate that starts/stops allocation counting), and
//! `get_alloc_count()`. When `clap-plugin` is enabled, the guard and counter
//! delegate to [`neural_amp_modeler_rs::common::alloc_audit`].
//!
//! Each test binary registers its own `#[global_allocator]` referencing
//! [`CountingAllocator`]; this module only provides the shared type.

#![allow(dead_code)]

#[cfg(not(feature = "heap-audit"))]
use std::alloc::{GlobalAlloc, Layout, System};
#[cfg(not(feature = "heap-audit"))]
use std::cell::Cell;

#[cfg(not(feature = "heap-audit"))]
thread_local! {
    static TRACKING_ACTIVE: Cell<bool> = const { Cell::new(false) };
    static ALLOC_COUNT_TLS: Cell<usize> = const { Cell::new(0) };
    static DEALLOC_COUNT_TLS: Cell<usize> = const { Cell::new(0) };
    static REALLOC_COUNT_TLS: Cell<usize> = const { Cell::new(0) };
}

#[cfg(not(feature = "heap-audit"))]
fn is_tracking_active() -> bool {
    TRACKING_ACTIVE
        .try_with(|active| active.get())
        .unwrap_or(false)
}

#[cfg(not(feature = "heap-audit"))]
fn set_tracking_active(active: bool) {
    let _ = TRACKING_ACTIVE.try_with(|a| a.set(active));
}

#[cfg(not(feature = "heap-audit"))]
fn get_local_alloc_count() -> usize {
    ALLOC_COUNT_TLS.try_with(|count| count.get()).unwrap_or(0)
}

#[cfg(not(feature = "heap-audit"))]
fn set_local_alloc_count(val: usize) {
    let _ = ALLOC_COUNT_TLS.try_with(|count| count.set(val));
}

#[cfg(not(feature = "heap-audit"))]
pub struct CountingAllocator;

#[cfg(feature = "heap-audit")]
pub use neural_amp_modeler_rs::common::alloc_audit::CountingAllocator;

#[cfg(not(feature = "heap-audit"))]
// SAFETY: `CountingAllocator` forwards every operation verbatim to `System`
// with the caller-supplied layout/pointer and only bumps per-thread counters;
// it owns no memory and imposes no additional invariants, so it upholds the
// `GlobalAlloc` contract exactly as `System` does.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if is_tracking_active() {
            let _ = ALLOC_COUNT_TLS.try_with(|count| {
                count.set(count.get() + 1);
            });
        }
        // SAFETY: this trait method forwards the exact pointer/layout/new-size supplied by Rust's allocator contract to `System`; the wrapper only increments per-thread counters and never dereferences the pointer.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if is_tracking_active() {
            let _ = DEALLOC_COUNT_TLS.try_with(|count| {
                count.set(count.get() + 1);
            });
        }
        // SAFETY: this trait method forwards the exact pointer/layout/new-size supplied by Rust's allocator contract to `System`; the wrapper only increments per-thread counters and never dereferences the pointer.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if is_tracking_active() {
            let _ = REALLOC_COUNT_TLS.try_with(|count| {
                count.set(count.get() + 1);
            });
            let _ = ALLOC_COUNT_TLS.try_with(|count| {
                count.set(count.get() + 1);
            });
        }
        // SAFETY: this trait method forwards the exact pointer/layout/new-size supplied by Rust's allocator contract to `System`; the wrapper only increments per-thread counters and never dereferences the pointer.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

pub struct TrackingGuard {
    #[cfg(feature = "heap-audit")]
    _inner: neural_amp_modeler_rs::common::alloc_audit::TrackingGuard,
}

impl TrackingGuard {
    pub fn new() -> Self {
        #[cfg(feature = "heap-audit")]
        {
            Self {
                _inner: neural_amp_modeler_rs::common::alloc_audit::TrackingGuard::new(),
            }
        }
        #[cfg(not(feature = "heap-audit"))]
        {
            set_tracking_active(true);
            set_local_alloc_count(0);
            let _ = DEALLOC_COUNT_TLS.try_with(|c| c.set(0));
            let _ = REALLOC_COUNT_TLS.try_with(|c| c.set(0));
            Self {}
        }
    }
}

impl Drop for TrackingGuard {
    fn drop(&mut self) {
        #[cfg(not(feature = "heap-audit"))]
        {
            set_tracking_active(false);
        }
    }
}

pub fn get_alloc_count() -> usize {
    #[cfg(feature = "heap-audit")]
    {
        neural_amp_modeler_rs::common::alloc_audit::get_alloc_count()
    }
    #[cfg(not(feature = "heap-audit"))]
    {
        get_local_alloc_count()
    }
}

pub fn get_dealloc_count() -> usize {
    #[cfg(feature = "heap-audit")]
    {
        neural_amp_modeler_rs::common::alloc_audit::get_dealloc_count()
    }
    #[cfg(not(feature = "heap-audit"))]
    {
        DEALLOC_COUNT_TLS.try_with(|c| c.get()).unwrap_or(0)
    }
}

pub fn get_realloc_count() -> usize {
    #[cfg(feature = "heap-audit")]
    {
        neural_amp_modeler_rs::common::alloc_audit::get_realloc_count()
    }
    #[cfg(not(feature = "heap-audit"))]
    {
        REALLOC_COUNT_TLS.try_with(|c| c.get()).unwrap_or(0)
    }
}
