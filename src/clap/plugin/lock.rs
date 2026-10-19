// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Cold (off-RT) mutex acquisition helper.
//!
//! Every lock reached through this module is taken off the audio thread, so
//! poisoning only ever means a prior main/GUI/dialog-thread panic while the
//! guard was held. The historical fallible `if let Ok(..)` acquisition shape
//! silently dropped the update on poison (returning early / writing nothing),
//! which is the same class of silent failure as F-RB-01. [`lock_cold`] instead
//! recovers the guard via `PoisonError::into_inner` — the canonical pattern
//! already used elsewhere in the crate — logs the recovery once, and clears the
//! poison so the warning cannot fire again on every later acquisition (F-RB-03).

use std::sync::{Mutex, MutexGuard};

/// Acquires `m`, recovering the guard when the mutex was poisoned.
///
/// `what` is a short static label (e.g. `"pending_model"`) naming the mutex in
/// the poison log line. Must never be called on the audio thread or any other
/// RT-context; GUI polling that must stay non-blocking keeps using `try_lock`.
///
/// Poison is sticky in `std::sync::Mutex`, so recovering without
/// [`Mutex::clear_poison`] would re-enter this branch — and re-log — on every
/// subsequent acquisition (e.g. the 250 ms housekeeping tick). The poison is
/// cleared right after recovery so the warning fires exactly once.
pub(crate) fn lock_cold<'a, T>(m: &'a Mutex<T>, what: &'static str) -> MutexGuard<'a, T> {
    m.lock().unwrap_or_else(|e| {
        log::error!("NAM-Plug: poisoned mutex ({what}); recovering");
        let guard = e.into_inner();
        m.clear_poison();
        guard
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_poisoned_mutex_and_preserves_value() {
        let m = Mutex::new(41u32);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut guard = m.lock().unwrap();
            *guard = 42;
            panic!("poison the mutex");
        }));
        assert!(m.is_poisoned(), "the mutex must be poisoned by the panic");
        // `lock_cold` must transparently recover the guard and expose the value
        // written before the panic instead of returning a "nothing happened" path.
        let guard = lock_cold(&m, "test");
        assert_eq!(*guard, 42);
    }
}
