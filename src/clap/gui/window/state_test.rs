// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use crate::clap::plugin::make_test_shared;
use clack_plugin::host::HostSharedHandle;
use clack_plugin::plugin::PluginError;
use std::sync::atomic::Ordering;

#[test]
fn test_init() {
    assert!(core::mem::size_of::<NamPluginWindow>() <= 4096);
    assert_eq!(core::mem::align_of::<NamPluginWindow>() % 8, 0);
}

#[test]
fn test_window_safe_shared_boundary() {
    use crate::clap::plugin::NamClapShared;
    use crate::clap::plugin::make_test_shared;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    let shared = Arc::new(make_test_shared());
    // SAFETY: `&*shared` is a valid, non-null pointer into the Arc.
    // The Arc is kept alive for the duration of the test.
    let shared_ref = unsafe { NamClapSharedRef::new(&*shared) };
    let alive_fence = Arc::clone(&shared.cold.alive_fence);

    // Emulates the accessor logic of safe_shared()
    let safe_access =
        |fence: &Arc<AtomicBool>, sref: NamClapSharedRef| -> Option<&'static NamClapShared> {
            if fence.load(Ordering::Acquire) {
                // SAFETY: fence Acquire ensures the shared state is still alive
                unsafe { Some(sref.as_ref()) }
            } else {
                None
            }
        };

    // Fence active: access is permitted
    assert!(alive_fence.load(Ordering::Relaxed));
    assert!(safe_access(&alive_fence, shared_ref).is_some());

    // Fence disabled: access is denied (prevents UAF)
    alive_fence.store(false, Ordering::Release);
    assert!(safe_access(&alive_fence, shared_ref).is_none());

    // Re-enable and confirm access restored
    alive_fence.store(true, Ordering::Release);
    assert!(safe_access(&alive_fence, shared_ref).is_some());
}

// -----------------------------------------------------------------------
// R-11: GL initialization failures must be structured errors, never
// panics crossing the baseview/CLAP FFI boundary.
// -----------------------------------------------------------------------

#[test]
fn test_gl_init_missing_context_returns_error() {
    // Simulates a headless/remote session where baseview could not set up
    // an OpenGL context: the old code panicked here, aborting the DAW.
    let err = match NamPluginWindow::init_graphics(None) {
        Err(e) => e,
        Ok(_) => panic!("expected Err for missing GL context"),
    };
    let PluginError::Message(msg) = err else {
        panic!("expected a friendly PluginError::Message");
    };
    assert!(!msg.is_empty());
    assert!(msg.contains("OpenGL context not available"));
}

#[test]
fn test_degraded_window_builds_without_gl() {
    // A degraded window is the baseview fallback handler after a failed
    // GL init: it must construct with no GL resources, and its drop must
    // be a safe no-op (no painter destroy).
    let shared = Arc::new(make_test_shared());
    // SAFETY: `&*shared` is a valid, non-null pointer into the Arc.
    // The Arc is kept alive for the duration of the test.
    let shared_ref = unsafe { NamClapSharedRef::new(&*shared) };
    let alive_fence = Arc::clone(&shared.cold.alive_fence);
    // SAFETY (test-only): the dangling host handle is never dereferenced —
    // a degraded window has no painter, so its event loop closes the
    // window before touching the host or the shared state.
    let host_static: HostSharedHandle<'static> =
        // SAFETY: transmute between repr(transparent) pointer wrappers of
        // equal size; the resulting handle is only stored, never used.
        unsafe { std::mem::transmute(std::ptr::NonNull::<()>::dangling()) };

    let close_signal = Arc::new(AtomicBool::new(false));
    let window = NamPluginWindow::degraded(shared_ref, host_static, close_signal, alive_fence, 1.0);
    assert!(
        window.painter.is_none(),
        "degraded window has no GL painter"
    );

    // Dropping the degraded window must not panic (painter is None).
    drop(window);
    // And the shared state survives independently of the GUI failure.
    assert!(shared.cold.alive_fence.load(Ordering::Relaxed));
}
