// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! X11 XEmbed embedding engine (E4 / Sprint 8 — delivery of Opção A).
//!
//! Implements *embed-at-creation*: the Slint/winit window is created **already
//! parented** under the host's X11 `Window` (CLAP `set_parent`) via
//! `slint::BackendSelector::with_winit_window_attributes_hook` +
//! `winit::platform::x11::WindowAttributesExtX11::with_embed_parent_window`.
//! The design decision (option i) is documented in `docs/architecture.md` §7.5.
//!
//! # Process-global constraints
//!
//! Slint allows exactly one platform per process and winit exactly one event
//! loop, and both are fixed by the **first** window created in the process.
//! This module therefore keeps process-global, lock-free state so every plugin
//! instance in a shared host process agrees on the windowing backend:
//!
//! - [`BACKEND_KIND`]: which winit event-loop backend is pinned —
//!   `Undecided`, `X11` (forced via `with_x11()`), or `NonX11` (Wayland or a
//!   failed selection). The first GUI window of the process decides it.
//! - [`EMBED_TARGET`]: the current host X11 parent id (`u32` XID). Captured by
//!   **value** — no raw pointer ever escapes to the GUI thread, so the
//!   `GuiHostBridge`/`alive_fence` fence protocol remains the sole owner of
//!   host lifetime.
//! - [`BACKEND_ONCE`]: exactly-once backend selection across all plugin
//!   instances in the process.
//!
//! # Honesty invariants (E2/Sprint 4 + "never two windows")
//!
//! An embedded request is accepted only when embedding is actually possible —
//! the process event loop is X11, or still undecided and can be forced to
//! X11. Otherwise the embedded `set_parent` returns an error and the host
//! falls back to a floating negotiation; the plugin never silently delivers a
//! floating window for an embedded contract, and never creates two windows.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};

// winit is not a direct dependency: it is re-exported by Slint 1.17 behind the
// `unstable-winit-030` feature (pinned via Cargo.lock; see the stability
// caveat in docs/architecture.md §7.5).
use slint::winit_030::winit;
use winit::platform::x11::{EventLoopBuilderExtX11, WindowAttributesExtX11};

/// Backend pin states ([`BACKEND_KIND`]).
const KIND_UNDECIDED: u8 = 0;
/// The process event loop is X11 (forced via `with_x11()`) — embedded X11 works.
const KIND_X11: u8 = 1;
/// The process event loop is non-X11 (Wayland) or selection failed — embedded
/// X11 is impossible; only floating modes can be served.
const KIND_NON_X11: u8 = 2;

/// Which winit event-loop backend is (or will be) pinned for this process.
static BACKEND_KIND: AtomicU8 = AtomicU8::new(KIND_UNDECIDED);
/// Current host X11 parent id for the dynamic embed hook. `0` = floating mode
/// (the hook passes the window attributes through unchanged).
static EMBED_TARGET: AtomicU32 = AtomicU32::new(0);
/// Exactly-once backend selection. A failed selection is cached too, so every
/// subsequent embedded attempt fails fast instead of re-connecting to a
/// missing display.
static BACKEND_ONCE: OnceLock<Result<(), ()>> = OnceLock::new();

/// Returns whether an X11 embedded negotiation can be honored by this process.
///
/// This is a *capability* probe (build feature + current backend pin): it
/// returns `true` while the event loop is undecided or pinned to X11. The
/// definitive guarantee happens at `set_parent` time, where any failure in
/// [`prepare_embedded_x11`] surfaces as a hard error (the host then falls
/// back to a floating negotiation). A *failed* selection never flips this
/// flag, so headless runs keep reporting the capability deterministically.
#[inline]
pub(crate) fn is_embedded_supported() -> bool {
    BACKEND_KIND.load(Ordering::Acquire) != KIND_NON_X11
}

/// Prepares the process for a floating window of the given negotiated API.
///
/// Only clears the embed target (the dynamic hook becomes a no-op). The
/// backend pinning itself happens later on the GUI worker thread, where the
/// winit event loop must be created (Slint 1.17 pins the platform to the
/// initializing thread).
#[inline]
pub(crate) fn prepare_floating() {
    EMBED_TARGET.store(0, Ordering::Release);
}

/// Prepares the process to embed the next Slint window under `parent_id`.
///
/// Only records the embed target and rejects an already-pinned non-X11
/// process loop; the actual backend selection happens on the GUI worker
/// thread via [`ensure_backend_on_gui_thread`].
///
/// # Errors
///
/// - The event loop is already pinned to a non-X11 backend (Wayland): there
///   is no way to honor embedded X11 for the rest of the process.
pub(crate) fn prepare_embedded_x11(parent_id: u32) -> Result<(), &'static str> {
    if BACKEND_KIND.load(Ordering::Acquire) == KIND_NON_X11 {
        return Err(
            "embedded X11 requires an X11 winit event loop, but this process is \
             already pinned to a non-X11 windowing backend",
        );
    }
    EMBED_TARGET.store(parent_id, Ordering::Release);
    Ok(())
}

/// Selects the winit backend on the GUI worker thread, exactly once per
/// process, forcing the X11 event loop when `force_x11` is set and installing
/// the dynamic embed hook.
///
/// Must be called from the thread that creates the Slint windows (the
/// persistent GUI worker), because Slint 1.17 pins the platform — and the
/// winit event loop — to the initializing thread.
///
/// # Errors
///
/// - The platform was already initialized (another window driver in this
///   process, or a failed selection cached from a display-less run): the
///   embedded/floating negotiation fails cleanly and the host falls back.
pub(crate) fn ensure_backend_on_gui_thread(force_x11: bool) -> Result<(), &'static str> {
    ensure_backend(force_x11)
}

/// Selects the winit backend exactly once, forcing the X11 event loop when
/// `force_x11` is set and installing the dynamic embed hook.
fn ensure_backend(force_x11: bool) -> Result<(), &'static str> {
    let outcome = BACKEND_ONCE.get_or_init(|| select_backend(force_x11));
    match outcome {
        Ok(()) => {
            // Pinning is decided by the *first* window of the process: only a
            // forced-X11 selection proves the loop is X11; everything else
            // (Wayland auto-selection or a failure) is conservatively non-X11.
            if BACKEND_KIND.load(Ordering::Acquire) == KIND_UNDECIDED {
                let kind = if force_x11 { KIND_X11 } else { KIND_NON_X11 };
                BACKEND_KIND.store(kind, Ordering::Release);
            }
            Ok(())
        }
        Err(_) => Err(
            "the winit/Slint backend could not be initialized for X11 embedding \
             (no X11 display server reachable?)",
        ),
    }
}

/// Runs the one-time backend selection with the dynamic embed hook installed.
///
/// The event loop is created here (see `Backend::build`), so a missing display
/// server surfaces at this point and is cached for the rest of the process.
fn select_backend(force_x11: bool) -> Result<(), ()> {
    let mut event_loop_builder =
        winit::event_loop::EventLoop::<slint::winit_030::SlintEvent>::with_user_event();
    if force_x11 {
        event_loop_builder.with_x11();
    }
    slint::BackendSelector::new()
        .backend_name("winit".to_string())
        .with_winit_event_loop_builder(event_loop_builder)
        .with_winit_window_attributes_hook(embed_hook)
        .select()
        .map_err(|_| ())
}

/// Dynamic embed hook, installed once for the whole process.
///
/// Reads the current embed target on every window creation: when a host parent
/// XID is set, winit creates the window **already parented** (XEmbed —
/// `_XEMBED` `[0,1]`), which is race-free; when the target is `0`, the window
/// is a regular top-level (floating) window. This single hook therefore serves
/// every GUI generation of every plugin instance without re-selection.
fn embed_hook(mut attributes: winit::window::WindowAttributes) -> winit::window::WindowAttributes {
    let parent = EMBED_TARGET.load(Ordering::Acquire);
    if parent != 0 {
        attributes = attributes.with_embed_parent_window(parent);
    }
    attributes
}
