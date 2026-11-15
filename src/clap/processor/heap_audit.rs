// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! CLAP-specific global allocator registration for heap audit.
//!
//! The shared `CountingAllocator` and tracking infrastructure lives in
//! [`neural_amp_modeler_rs::common::alloc_audit`]. The arming switch is owned
//! by this crate and is thread-local: each audited window is armed by, and
//! observed on, its own thread, so concurrent tests cannot disarm or mask one
//! another.

use std::cell::Cell;

#[cfg(feature = "heap-audit")]
#[global_allocator]
static GLOBAL: neural_amp_modeler_rs::common::alloc_audit::CountingAllocator =
    neural_amp_modeler_rs::common::alloc_audit::CountingAllocator;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn is_armed() -> bool {
    ARMED.with(Cell::get)
}

pub(crate) fn window_violations() -> usize {
    neural_amp_modeler_rs::common::alloc_audit::get_alloc_count()
        + neural_amp_modeler_rs::common::alloc_audit::get_dealloc_count()
        + neural_amp_modeler_rs::common::alloc_audit::get_realloc_count()
}

pub(crate) struct ArmGuard {
    previous: bool,
}

impl ArmGuard {
    pub(crate) fn set(armed: bool) -> Self {
        let previous = ARMED.with(|a| a.replace(armed));
        Self { previous }
    }
}

impl Drop for ArmGuard {
    fn drop(&mut self) {
        ARMED.with(|a| a.set(self.previous));
    }
}
