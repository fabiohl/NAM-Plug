// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Integration regression tests for the CLAP GUI lifecycle FSM.
//!
//! These tests drive the **public** `PluginGuiImpl` surface (`create` → `show` →
//! `hide`) on a real plugin instance backed by the lightweight test host, proving
//! that `hide()` must be accepted after a successful `show()`.
//!
//! The defect they guard against is purely a state-machine wiring issue: the
//! backend never reports `WindowReady`, so the lifecycle parks in `ShowRequested`
//! and `hide()` is rejected before it can hide the window. Because the wiring is
//! independent of any window server, the gate test runs headless (no Xvfb); an
//! additional `#[ignore]`d variant exercises the real window backend when a
//! display is available.

use crate::clap::gui::lifecycle::{GuiEvent, GuiLifecycle};
use crate::clap::plugin::NamClapMainThread;
use crate::clap::test_util::{self};
use clack_extensions::gui::{GuiApiType, GuiConfiguration, PluginGui};
use clack_host::plugin::PluginInstance;

/// Returns a mutable reference to the main thread struct of a test plugin.
///
/// The reference is only valid while `instance` is alive; callers must not use
/// it after the instance is dropped.
fn main_thread_mut(
    instance: &mut PluginInstance<test_util::TestHost>,
) -> &mut NamClapMainThread<'_> {
    let raw_ptr = instance.plugin_handle().as_raw_ptr();
    let mut nn = unsafe {
        clack_plugin::extensions::wrapper::PluginWrapper::<crate::clap::plugin::NamClapPlugin>::handle(
            raw_ptr,
            |wrapper| Ok(wrapper.main_thread()),
        )
    }
    .expect("Failed to get plugin wrapper");
    // SAFETY: the plugin instance is uniquely borrowed for the duration of the
    // caller's use; the wrapper guarantees main-thread exclusivity and nothing
    // else aliases this struct while the caller holds the reference.
    unsafe { nn.as_mut() }
}

/// Fixed X11 floating configuration used as the canonical host request.
fn x11_floating() -> GuiConfiguration<'static> {
    GuiConfiguration {
        api_type: GuiApiType::X11,
        is_floating: true,
    }
}

/// Headless F1 regression: after `create()` and a successful `show()`, the host
/// `hide()` **must** return `Ok` so the window can actually be hidden.
#[test]
fn gui_lifecycle_hide_after_show_returns_ok() {
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let gui_ext = plugin_instance
        .plugin_handle()
        .get_extension::<PluginGui>()
        .expect("PluginGui extension not found");

    {
        let mut handle = plugin_instance.plugin_handle();
        gui_ext
            .create(&mut handle, x11_floating())
            .expect("create(X11 floating) must succeed");
        gui_ext
            .show(&mut handle)
            .expect("show() from Hidden must succeed");
    }

    {
        let mut handle = plugin_instance.plugin_handle();
        let hide = gui_ext.hide(&mut handle);
        assert!(
            hide.is_ok(),
            "F1 defect: hide() after show() must return Ok so the window is hidden, got {hide:?}"
        );
    }

    let mt = main_thread_mut(&mut plugin_instance);
    assert_eq!(
        mt.gui_lifecycle,
        GuiLifecycle::Hidden,
        "hide() must leave the FSM in Hidden once the dispatch is accepted"
    );
}

/// Headless wiring regression: the backend "user closed" signal must reach the
/// FSM through the shared flag drained by `housekeeping()`.
#[test]
fn gui_lifecycle_user_closed_flag_drained_by_housekeeping() {
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let gui_ext = plugin_instance
        .plugin_handle()
        .get_extension::<PluginGui>()
        .expect("PluginGui extension not found");

    {
        let mut handle = plugin_instance.plugin_handle();
        gui_ext
            .create(&mut handle, x11_floating())
            .expect("create(X11 floating) must succeed");
        gui_ext
            .show(&mut handle)
            .expect("show() from Hidden must succeed");
    }

    // The Slint close callback publishes this flag and wakes the host; raising
    // it directly proves the main-thread drain path feeds the lifecycle FSM.
    let shared_ptr = test_util::extract_shared(&mut plugin_instance);
    // SAFETY: the plugin instance outlives this reference for the whole test.
    let shared = unsafe { &*shared_ptr };
    shared
        .cold
        .gui_user_closed
        .store(true, std::sync::atomic::Ordering::Release);

    {
        let mt = main_thread_mut(&mut plugin_instance);
        mt.housekeeping();
        assert_eq!(
            mt.gui_lifecycle,
            GuiLifecycle::Hidden,
            "draining gui_user_closed must drive UserClosed into the FSM"
        );
    }
    assert!(
        !shared
            .cold
            .gui_user_closed
            .load(std::sync::atomic::Ordering::Acquire),
        "the drained flag must be cleared so each close fires exactly once"
    );

    let mut handle = plugin_instance.plugin_handle();
    gui_ext
        .show(&mut handle)
        .expect("show() after a user close must succeed from the post-close Hidden state");
}

/// A pending backend close must not reject the host's re-open: `show()`
/// reconciles the signal before validating the transition.
#[test]
fn gui_lifecycle_pending_close_reconciled_before_show() {
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let gui_ext = plugin_instance
        .plugin_handle()
        .get_extension::<PluginGui>()
        .expect("PluginGui extension not found");

    {
        let mut handle = plugin_instance.plugin_handle();
        gui_ext
            .create(&mut handle, x11_floating())
            .expect("create(X11 floating) must succeed");
        gui_ext
            .show(&mut handle)
            .expect("show() from Hidden must succeed");
    }

    let shared_ptr = test_util::extract_shared(&mut plugin_instance);
    // SAFETY: the plugin instance outlives this reference for the whole test.
    let shared = unsafe { &*shared_ptr };
    shared
        .cold
        .gui_user_closed
        .store(true, std::sync::atomic::Ordering::Release);

    {
        let mut handle = plugin_instance.plugin_handle();
        gui_ext
            .show(&mut handle)
            .expect("show() must reconcile a pending close instead of rejecting the re-open");
    }

    let mt = main_thread_mut(&mut plugin_instance);
    assert_eq!(mt.gui_lifecycle, GuiLifecycle::ShowRequested);
    assert!(
        !shared
            .cold
            .gui_user_closed
            .load(std::sync::atomic::Ordering::Acquire),
        "show() must consume the pending close signal"
    );
}

/// `destroy()` starts a fresh GUI generation: a close signal left over from the
/// previous window must not leak into it.
#[test]
fn gui_lifecycle_destroy_clears_stale_close() {
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let gui_ext = plugin_instance
        .plugin_handle()
        .get_extension::<PluginGui>()
        .expect("PluginGui extension not found");

    {
        let mut handle = plugin_instance.plugin_handle();
        gui_ext
            .create(&mut handle, x11_floating())
            .expect("create(X11 floating) must succeed");
        gui_ext
            .show(&mut handle)
            .expect("show() from Hidden must succeed");
    }

    let shared_ptr = test_util::extract_shared(&mut plugin_instance);
    // SAFETY: the plugin instance outlives this reference for the whole test.
    let shared = unsafe { &*shared_ptr };
    shared
        .cold
        .gui_user_closed
        .store(true, std::sync::atomic::Ordering::Release);

    {
        let mut handle = plugin_instance.plugin_handle();
        gui_ext.destroy(&mut handle);
    }
    assert!(
        !shared
            .cold
            .gui_user_closed
            .load(std::sync::atomic::Ordering::Acquire),
        "destroy() must discard a pending close so it cannot cross GUI generations"
    );
}

/// Headless recovery regression: after the user closes the window (`UserClosed`
/// reaches the FSM), a subsequent `show()` must be accepted.
///
/// The close is injected directly into the FSM here; the backend-to-FSM wiring
/// that delivers it is covered by `gui_lifecycle_user_closed_flag_drained_by_housekeeping`.
#[test]
fn gui_lifecycle_reopen_after_user_closed_returns_ok() {
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let gui_ext = plugin_instance
        .plugin_handle()
        .get_extension::<PluginGui>()
        .expect("PluginGui extension not found");

    {
        let mut handle = plugin_instance.plugin_handle();
        gui_ext
            .create(&mut handle, x11_floating())
            .expect("create(X11 floating) must succeed");
        gui_ext
            .show(&mut handle)
            .expect("show() from Hidden must succeed");
    }

    {
        let mt = main_thread_mut(&mut plugin_instance);
        mt.gui_lifecycle
            .transition(GuiEvent::UserClosed)
            .expect("UserClosed from ShowRequested must be a valid transition");
        assert_eq!(mt.gui_lifecycle, GuiLifecycle::Hidden);
    }

    let mut handle = plugin_instance.plugin_handle();
    let reopen = gui_ext.show(&mut handle);
    assert!(
        reopen.is_ok(),
        "show() after a user close must succeed, got {reopen:?}"
    );
}

/// Complementary backend variant: same `create → set_transient → show → hide →
/// destroy` sequence with a real Slint window. Requires a display server, hence
/// `#[ignore]`d; the headless tests above are the always-on gate.
#[test]
#[ignore = "requires a dedicated X11 display server (Xvfb) and mutates process env"]
fn gui_lifecycle_hide_after_show_with_real_window() {
    assert!(
        std::env::var("DISPLAY").is_ok(),
        "DISPLAY not set; start Xvfb before running this test"
    );
    // SAFETY: setting these process-wide variables is confined to this ignored,
    // display-only test and only affects software rendering.
    unsafe {
        std::env::set_var("LIBGL_ALWAYS_SOFTWARE", "1");
        std::env::set_var("GALLIUM_DRIVER", "llvmpipe");
    }

    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let gui_ext = plugin_instance
        .plugin_handle()
        .get_extension::<PluginGui>()
        .expect("PluginGui extension not found");
    let mut handle = plugin_instance.plugin_handle();

    let float_config = GuiConfiguration {
        api_type: GuiApiType::X11,
        is_floating: true,
    };
    gui_ext
        .create(&mut handle, float_config)
        .expect("create(X11 floating) must succeed");
    // SAFETY: the dummy window handle is never dereferenced by the plugin's
    // floating path, which creates its own top-level window.
    unsafe {
        gui_ext
            .set_transient(
                &mut handle,
                clack_extensions::gui::Window::from_generic_ptr(
                    GuiApiType::X11,
                    std::ptr::null_mut(),
                ),
            )
            .expect("set_transient() must succeed");
    }
    gui_ext.show(&mut handle).expect("show() must succeed");
    let hide = gui_ext.hide(&mut handle);
    assert!(
        hide.is_ok(),
        "F1 defect with a real window: hide() after show() must return Ok, got {hide:?}"
    );
    gui_ext.destroy(&mut handle);
}
