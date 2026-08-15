// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
// SAFETY: FFI call, host pointer transmute, or raw graphics context access with verified lifetimes.
#![warn(clippy::undocumented_unsafe_blocks)]

//! Implementation of the main graphical user interface window.

/// GUI lifecycle finite state machine.
pub mod lifecycle;
/// egui rendering function and UI components.
pub mod ui;
/// Main window and event/draw manager with baseview + egui.
pub mod window;

/// Default width of the plugin window.
pub const GUI_WIDTH: u32 = 600;
/// Default height of the plugin window.
pub const GUI_HEIGHT: u32 = 275;

/// Safe bridge for passing the CLAP host handle to the GUI thread.
///
/// `HostSharedHandle<'a>` carries a lifetime tied to the plugin instance,
/// but the CLAP spec guarantees the host outlives the plugin: the factory is
/// deactivated only after all plugin instances are destroyed. This struct
/// wraps the raw host pointer and documents the invariant that the host
/// remains valid for the plugin's entire lifetime (including any spawned
/// GUI threads).
///
/// # R-09 fence protocol
///
/// GUI threads only dereference this bridge (or `NamClapShared`) while the
/// `alive_fence` is up: `NamClapMainThread::drop` lowers the fence and
/// bounded-joins every GUI/dialog thread before the plugin instance's shared
/// state is released. Window event loops are fence-gated no-ops during
/// destruction, so the `'static` handle is never dereferenced after the
/// plugin is destroyed.
///
/// Unlike the previous `extend_host_lifetime()` unsafe transmute, this
/// struct encapsulates the pointer with explicit safety documentation at
/// the creation site — no transmute necessary.
#[derive(Clone, Copy)]
pub(crate) struct GuiHostBridge {
    raw: std::ptr::NonNull<()>,
}

impl GuiHostBridge {
    /// Creates a `GuiHostBridge` from a live `HostSharedHandle`.
    ///
    /// # Safety
    ///
    /// The host pointer encapsulated here is valid for the lifetime of the
    /// plugin. The caller must ensure:
    /// 1. The host outlives the plugin (CLAP spec guarantee).
    /// 2. Any GUI thread holding this bridge dereferences it only while the
    ///    `alive_fence` is up (fence-gated event loops, R-09).
    #[inline]
    pub fn new(host: &clack_plugin::host::HostSharedHandle<'_>) -> Self {
        // HostSharedHandle is repr(transparent) over NonNull<clap_host>.
        // SAFETY: transmute_copy of a repr(transparent) type to extract the
        // inner pointer. The pointer remains valid as long as the host lives,
        // which the CLAP spec guarantees is longer than the plugin lifetime.
        let nn: std::ptr::NonNull<()> = unsafe { std::mem::transmute_copy(host) };
        Self { raw: nn }
    }

    /// Reconstructs the `HostSharedHandle` with `'static` lifetime.
    ///
    /// # Safety
    ///
    /// The returned handle is only valid while the host is alive — which the
    /// CLAP spec guarantees is longer than the plugin's lifetime. Callers must
    /// not cache this handle beyond the plugin's `destroy()` call, and every
    /// dereference from a GUI thread must be fenced by `alive_fence` (R-09).
    #[inline]
    pub fn as_static(&self) -> clack_plugin::host::HostSharedHandle<'static> {
        // SAFETY: HostSharedHandle is repr(transparent) over NonNull<()>.
        // The pointer was obtained from a live HostSharedHandle at construct
        // time and the host outlives the plugin (CLAP spec guarantee).
        // The 'static lifetime is justified because the host's actual lifetime
        // encompasses all plugin instance lifetimes.
        unsafe {
            std::mem::transmute::<
                std::ptr::NonNull<()>,
                clack_plugin::host::HostSharedHandle<'static>,
            >(self.raw)
        }
    }
}
