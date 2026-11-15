// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
// SAFETY: FFI call, host pointer transmute, or raw graphics context access with verified lifetimes.
#![warn(clippy::undocumented_unsafe_blocks)]

//! Implementation of the main graphical user interface window.

/// Dialog state for asynchronous file dialogs.
pub(crate) mod dialog_state;
/// File dialog handling for NAM models and IR files.
pub(crate) mod file_dialogs;
/// GUI lifecycle finite state machine.
pub mod lifecycle;
/// Slint view-model and telemetry/event bridge.
pub mod slint_view_model;
/// Persistent per-instance GUI worker thread.
pub(crate) mod worker;
/// Process-global X11 XEmbed embed-at-creation engine.
pub(crate) mod x11_embed;
pub use slint_view_model::{MainWindow, SlintViewModel};

/// Default width of the plugin window.
pub const GUI_WIDTH: u32 = 600;
/// Default height of the plugin window.
pub const GUI_HEIGHT: u32 = 312;

/// Safe bridge for passing the CLAP host handle to the GUI thread.
///
/// `HostSharedHandle<'a>` carries a lifetime tied to the plugin instance,
/// but the CLAP spec guarantees the host outlives the plugin: the factory is
/// deactivated only after all plugin instances are destroyed. This struct
/// wraps the raw host pointer and documents the invariant that the host
/// remains valid for the plugin's entire lifetime (including any spawned
/// GUI threads).
///
/// # Teardown safety fence protocol
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
///
/// # Embedded X11 windows
///
/// An embedded child window is created *inside* the host's window tree via
/// the XEmbed hook (`x11_embed`): the hook captures the host X11 parent id by
/// value (`u32` XID), so no additional raw pointer crosses to the GUI thread.
/// If the host destroys its panel without calling `gui.destroy()`, X11
/// destroys the descendant window automatically — the child never outlives
/// the host `Window`, and the fence protocol below is unchanged.
#[derive(Clone, Copy)]
pub struct GuiHostBridge {
    raw: std::ptr::NonNull<clap_sys::host::clap_host>,
}

// SAFETY: HostSharedHandle is Send and Sync in clack-plugin, wrapping a thread-safe host pointer.
unsafe impl Send for GuiHostBridge {}
// SAFETY: HostSharedHandle is Send and Sync in clack-plugin, wrapping a thread-safe host pointer.
unsafe impl Sync for GuiHostBridge {}

// Compile-time layout assertions ensuring GuiHostBridge and HostSharedHandle have identical representation.
const _: () = assert!(
    std::mem::size_of::<clack_plugin::host::HostSharedHandle<'static>>()
        == std::mem::size_of::<std::ptr::NonNull<clap_sys::host::clap_host>>()
);
const _: () = assert!(
    std::mem::align_of::<clack_plugin::host::HostSharedHandle<'static>>()
        == std::mem::align_of::<std::ptr::NonNull<clap_sys::host::clap_host>>()
);
const _: () = assert!(
    std::mem::size_of::<GuiHostBridge>()
        == std::mem::size_of::<std::ptr::NonNull<clap_sys::host::clap_host>>()
);
const _: () = assert!(
    std::mem::align_of::<GuiHostBridge>()
        == std::mem::align_of::<std::ptr::NonNull<clap_sys::host::clap_host>>()
);

impl GuiHostBridge {
    /// Creates a `GuiHostBridge` from a live `HostSharedHandle`.
    ///
    /// # Safety invariants
    ///
    /// The host pointer encapsulated here is valid for the lifetime of the
    /// plugin. The caller must ensure:
    /// 1. The host outlives the plugin (CLAP spec guarantee).
    /// 2. Any thread holding this bridge dereferences it only while the
    ///    `alive_fence` or quiescence lock is active.
    #[inline]
    pub fn new(host: &clack_plugin::host::HostSharedHandle<'_>) -> Self {
        let raw = std::ptr::NonNull::from(host.as_raw());
        Self { raw }
    }

    /// Reconstructs the `HostSharedHandle` with `'static` lifetime.
    ///
    /// The `'static` lifetime is a fiction: the real lifetime is the plugin
    /// instance's, enforced at runtime by the teardown fence, not by the type
    /// system. Returning it from a safe function would let a safe caller keep
    /// the handle after the host is gone, so the boundary is `unsafe`.
    ///
    /// # Safety
    ///
    /// The returned handle must not be stored, moved to another thread, or
    /// dereferenced except within a window that the plugin's teardown protocol
    /// fences. Acceptable windows are:
    /// 1. a `HostCallQuiescence::host_call` (or `quiescence::host_call`)
    ///    closure, which refuses entry once `alive_fence` is lowered; or
    /// 2. a `HostLogQuiescence` guard, which `NamClapMainThread::drop` quiesces
    ///    before the host is released.
    ///
    /// The handle must be obtained inside that window, not before it.
    #[inline]
    pub unsafe fn as_static(&self) -> clack_plugin::host::HostSharedHandle<'static> {
        // SAFETY: `self.raw` was obtained from a valid HostSharedHandle during
        // initialization. The caller upholds the fencing contract above, which
        // keeps every dereference inside the host's lifetime.
        unsafe { clack_plugin::host::HostAudioProcessorHandle::from_raw(self.raw).shared() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gui_host_bridge_roundtrip() {
        let mut mock_host = clap_sys::host::clap_host {
            clap_version: clap_sys::version::CLAP_VERSION,
            host_data: std::ptr::null_mut(),
            name: c"Test Host".as_ptr(),
            vendor: std::ptr::null(),
            url: std::ptr::null(),
            version: std::ptr::null(),
            get_extension: None,
            request_restart: None,
            request_process: None,
            request_callback: None,
        };

        let raw_ptr = std::ptr::NonNull::new(&mut mock_host).unwrap();
        // SAFETY: `raw_ptr` points to a valid mock `clap_host` allocated on the test stack.
        let shared_handle =
            unsafe { clack_plugin::host::HostAudioProcessorHandle::from_raw(raw_ptr).shared() };

        let bridge = GuiHostBridge::new(&shared_handle);
        // SAFETY: `mock_host` outlives `static_handle`, and the handle is only compared, never dereferenced.
        let static_handle = unsafe { bridge.as_static() };

        assert_eq!(
            static_handle.as_raw() as *const _,
            &raw const mock_host as *const _
        );
    }
}
