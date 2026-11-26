// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Test-only loader hooks.

use std::path::Path;

#[cfg(test)]
type TestMidLoadHook = Box<dyn Fn(&Path) + Send + 'static>;

#[cfg(test)]
static TEST_MID_LOAD_HOOK: std::sync::Mutex<Option<TestMidLoadHook>> = std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn set_test_mid_load_hook<F: Fn(&Path) + Send + 'static>(f: Option<F>) {
    *TEST_MID_LOAD_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = f.map(|func| Box::new(func) as _);
}

#[cfg(test)]
pub(crate) fn run_test_mid_load_hook(path: &Path) {
    let mut guard = TEST_MID_LOAD_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(hook) = guard.take() {
        drop(guard);
        let hook_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            hook(path);
        }));
        *TEST_MID_LOAD_HOOK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(hook);
        if let Err(payload) = hook_res {
            std::panic::resume_unwind(payload);
        }
    }
}

/// Serialises tests that install the process-global `TEST_MID_LOAD_HOOK`, so a
/// test that stalls the loader cannot have its hook replaced by another test
/// running in parallel (the hook is a single global slot).
#[cfg(test)]
pub(crate) fn test_mid_load_hook_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
