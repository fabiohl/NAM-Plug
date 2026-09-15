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
use clack_extensions::gui::{GuiApiType, GuiConfiguration, GuiSize, PluginGui, Window};
use clack_host::plugin::PluginInstance;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt as _;

/// Returns a shared reference to the main thread struct of a test plugin.
///
/// The reference is only valid while `instance` is alive; callers must not use
/// it after the instance is dropped.
///
/// Shared (not mutable): clack 0.2.0 exposes the main thread through `&self`
/// ("Embrace Reentrancy"); interior mutability (`Cell`/`RefCell`) keeps the
/// staged slots reachable without `&mut`.
fn main_thread_mut(instance: &mut PluginInstance<test_util::TestHost>) -> &NamClapMainThread<'_> {
    let raw_ptr = instance.plugin_handle().as_raw_ptr();
    let ptr = unsafe {
        clack_plugin::extensions::wrapper::PluginWrapper::<crate::clap::plugin::NamClapPlugin>::handle(
            raw_ptr,
            |wrapper| Ok(wrapper.main_thread() as *const NamClapMainThread<'_>),
        )
    }
    .expect("Failed to get plugin wrapper");
    // SAFETY: the plugin instance outlives the returned reference, and the
    // wrapper guarantees main-thread exclusivity while it is alive.
    unsafe { &*ptr }
}

/// Fixed X11 floating configuration used as the canonical host request.
fn x11_floating() -> GuiConfiguration<'static> {
    GuiConfiguration {
        api_type: GuiApiType::X11,
        is_floating: true,
    }
}

/// Fixed X11 embedded configuration (E4/Sprint 8).
fn x11_embedded() -> GuiConfiguration<'static> {
    GuiConfiguration {
        api_type: GuiApiType::X11,
        is_floating: false,
    }
}

/// Creates a real top-level X11 window to act as the CLAP host embed parent.
///
/// Returns `None` when no X11 display server is reachable (headless CI) — the
/// caller then exercises the graceful-failure path instead.
fn create_host_parent_window() -> Option<(x11rb::rust_connection::RustConnection, u32)> {
    use x11rb::protocol::xproto::{CreateWindowAux, EventMask, WindowClass};

    let (conn, screen_num) = x11rb::connect(None).ok()?;
    let screen = &conn.setup().roots[screen_num];
    let parent: u32 = conn.generate_id().ok()?;
    let aux = CreateWindowAux::new()
        .background_pixel(screen.white_pixel)
        .event_mask(EventMask::EXPOSURE | EventMask::STRUCTURE_NOTIFY);
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        parent,
        screen.root,
        0,
        0,
        640,
        320,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &aux,
    )
    .ok()?;
    conn.map_window(parent).ok()?;
    conn.flush().ok()?;
    Some((conn, parent))
}

/// Polls `query_tree(host)` until the host window has an X11 child (the
/// embedded Slint window) or `timeout` elapses.
fn wait_for_embedded_child(
    conn: &x11rb::rust_connection::RustConnection,
    host: u32,
    timeout: std::time::Duration,
) -> Option<u32> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(cookie) = conn.query_tree(host)
            && let Ok(reply) = cookie.reply()
            && !reply.children.is_empty()
        {
            return reply.children.first().copied();
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// Polls until `child` no longer exists on the X server (the embedded window
/// was destroyed by teardown) or `timeout` elapses.
fn wait_until_child_destroyed(
    conn: &x11rb::rust_connection::RustConnection,
    child: u32,
    timeout: std::time::Duration,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let gone = match conn.query_tree(child) {
            Ok(cookie) => cookie.reply().is_err(),
            Err(_) => true,
        };
        if gone {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// Headless E4 gate: the X11 embedded negotiation is accepted, and either
/// embeds for real (when a display server is reachable) or fails **at
/// `create()`** (headless CI) — it never silently spawns a floating window for
/// an embedded contract and never creates two windows.
///
/// The graceful branch is deterministic and doubly honest: `create(X11
/// embedded)` is rejected when no X11 display is reachable (clack 0.1.1
/// cannot propagate a `set_parent` error to the host, so the negotiation must
/// fail where it can be seen), and the FSM stays untouched.
#[test]
fn gui_lifecycle_x11_embedded_set_parent_honest() {
    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
    let gui_ext = plugin_instance
        .plugin_handle()
        .get_extension::<PluginGui>()
        .expect("PluginGui extension not found");

    // Negotiation: embedded X11 is advertised (E4/Sprint 8).
    {
        let handle = plugin_instance.plugin_handle();
        assert!(
            gui_ext.is_api_supported(&handle, x11_embedded()),
            "X11 embedded must be supported by negotiation"
        );
    }

    let display_available = std::env::var_os("DISPLAY").is_some();
    let handle = plugin_instance.plugin_handle();
    if display_available {
        // Real display: create + full embedded window cycle against the fake host.
        let (conn, parent) = create_host_parent_window()
            .expect("a display is available, so a host window must be creatable");
        gui_ext
            .create(&handle, x11_embedded())
            .expect("create(X11 embedded) with a display must succeed");
        // SAFETY: the x11rb host window outlives the plugin instance.
        unsafe {
            gui_ext
                .set_parent(
                    &handle,
                    Window::from_x11_handle(parent as std::os::raw::c_ulong),
                )
                .expect("set_parent(real X11 host window) must embed");
        }
        gui_ext.show(&handle).expect("show() must succeed");

        let child = wait_for_embedded_child(&conn, parent, std::time::Duration::from_secs(4))
            .expect("the host window must acquire the embedded child within the timeout");
        let tree = conn
            .query_tree(child)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .expect("embedded child must still exist");
        let host_tree = conn
            .query_tree(parent)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .expect("host window must still exist");
        assert_eq!(
            host_tree.children,
            vec![child],
            "host must have exactly the embedded child"
        );
        assert_eq!(
            tree.parent, parent,
            "the embedded Slint window must be a direct child of the host window (XEmbed)"
        );

        // T4.8.2: fixed size/scale contract holds on the child.
        assert_eq!(
            gui_ext.get_size(&handle),
            Some(GuiSize {
                width: 600,
                height: 275
            }),
            "embedded GUI keeps the fixed 600x275 logical size"
        );
        assert!(
            !gui_ext.can_resize(&handle),
            "embedded GUI is not resizable"
        );
        assert!(
            gui_ext
                .set_size(
                    &handle,
                    GuiSize {
                        width: 600,
                        height: 275
                    }
                )
                .is_ok(),
            "set_size(600x275) must be accepted"
        );
        assert!(
            gui_ext.set_scale(&handle, 1.5).is_ok(),
            "set_scale(positive) must be accepted"
        );

        gui_ext.hide(&handle).expect("hide() must succeed");
        gui_ext.show(&handle).expect("re-show() must succeed");
        gui_ext.destroy(&handle);
        assert!(
            wait_until_child_destroyed(&conn, child, std::time::Duration::from_secs(4)),
            "teardown must destroy the embedded X11 child (no orphan windows)"
        );
    } else {
        // Headless: the embedded negotiation must fail at create() — visible to
        // the host (create propagates; set_parent does not) — with the FSM
        // untouched and no window ever created.
        let result = gui_ext.create(&handle, x11_embedded());
        assert!(
            result.is_err(),
            "create(X11 embedded) without a display must return an error, got {result:?}"
        );
        let mt = main_thread_mut(&mut plugin_instance);
        assert_eq!(
            mt.gui_lifecycle.get(),
            GuiLifecycle::Hidden,
            "a failed embedded create() must leave the lifecycle FSM untouched"
        );
        assert!(
            mt.slint_window.borrow().is_none(),
            "a failed embedded create() must never create a window"
        );
        let handle = plugin_instance.plugin_handle();
        gui_ext.destroy(&handle);
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
        let handle = plugin_instance.plugin_handle();
        gui_ext
            .create(&handle, x11_floating())
            .expect("create(X11 floating) must succeed");
        gui_ext
            .show(&handle)
            .expect("show() from Hidden must succeed");
    }

    {
        let handle = plugin_instance.plugin_handle();
        let hide = gui_ext.hide(&handle);
        assert!(
            hide.is_ok(),
            "F1 defect: hide() after show() must return Ok so the window is hidden, got {hide:?}"
        );
    }

    let mt = main_thread_mut(&mut plugin_instance);
    assert_eq!(
        mt.gui_lifecycle.get(),
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
        let handle = plugin_instance.plugin_handle();
        gui_ext
            .create(&handle, x11_floating())
            .expect("create(X11 floating) must succeed");
        gui_ext
            .show(&handle)
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
            mt.gui_lifecycle.get(),
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

    let handle = plugin_instance.plugin_handle();
    gui_ext
        .show(&handle)
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
        let handle = plugin_instance.plugin_handle();
        gui_ext
            .create(&handle, x11_floating())
            .expect("create(X11 floating) must succeed");
        gui_ext
            .show(&handle)
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
        let handle = plugin_instance.plugin_handle();
        gui_ext
            .show(&handle)
            .expect("show() must reconcile a pending close instead of rejecting the re-open");
    }

    let mt = main_thread_mut(&mut plugin_instance);
    assert_eq!(mt.gui_lifecycle.get(), GuiLifecycle::ShowRequested);
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
        let handle = plugin_instance.plugin_handle();
        gui_ext
            .create(&handle, x11_floating())
            .expect("create(X11 floating) must succeed");
        gui_ext
            .show(&handle)
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
        let handle = plugin_instance.plugin_handle();
        gui_ext.destroy(&handle);
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
        let handle = plugin_instance.plugin_handle();
        gui_ext
            .create(&handle, x11_floating())
            .expect("create(X11 floating) must succeed");
        gui_ext
            .show(&handle)
            .expect("show() from Hidden must succeed");
    }

    {
        let mt = main_thread_mut(&mut plugin_instance);
        let mut lifecycle = mt.gui_lifecycle.get();
        lifecycle
            .transition(GuiEvent::UserClosed)
            .expect("UserClosed from ShowRequested must be a valid transition");
        mt.gui_lifecycle.set(lifecycle);
        assert_eq!(mt.gui_lifecycle.get(), GuiLifecycle::Hidden);
    }

    let handle = plugin_instance.plugin_handle();
    let reopen = gui_ext.show(&handle);
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
    let handle = plugin_instance.plugin_handle();

    let float_config = GuiConfiguration {
        api_type: GuiApiType::X11,
        is_floating: true,
    };
    gui_ext
        .create(&handle, float_config)
        .expect("create(X11 floating) must succeed");
    // SAFETY: the dummy window handle is never dereferenced by the plugin's
    // floating path, which creates its own top-level window.
    unsafe {
        gui_ext
            .set_transient(
                &handle,
                clack_extensions::gui::Window::from_generic_ptr(
                    GuiApiType::X11,
                    std::ptr::null_mut(),
                ),
            )
            .expect("set_transient() must succeed");
    }
    gui_ext.show(&handle).expect("show() must succeed");
    let hide = gui_ext.hide(&handle);
    assert!(
        hide.is_ok(),
        "F1 defect with a real window: hide() after show() must return Ok, got {hide:?}"
    );
    gui_ext.destroy(&handle);
}

/// Xvfb/X11 display test: 10 full open/close embedded cycles against the same
/// fake host window **on one plugin instance** (the real-host toggling
/// pattern), verifying on every cycle:
///
/// - the Slint window is a native X11 child of the host (XEmbed ancestry);
/// - teardown destroys the child (no orphans — verified via `query_tree`);
/// - the T4.8.2 contract (fixed 600×275, no resize, scale accepted) holds.
///
/// Run against a dedicated display server:
///
/// ```text
/// Xvfb :99 -screen 0 1280x800x24 &; DISPLAY=:99 cargo test --features testing -- --ignored gui_embedded
/// ```
///
/// Keep this the only test matching `gui_embedded`: Slint 1.17 allows one
/// platform per process, so a second display test in the same process could
/// not create windows (see `docs/architecture.md` §7.5).
#[test]
#[ignore = "requires a dedicated X11 display server (Xvfb/Xephyr) and mutates process env"]
fn gui_embedded_x11_open_close_cycles_no_leak() {
    use x11rb::protocol::xproto::ConnectionExt as _;
    assert!(
        std::env::var("DISPLAY").is_ok(),
        "DISPLAY not set; start Xvfb before running this test"
    );
    // SAFETY: settings confined to this ignored display-only test.
    unsafe {
        std::env::set_var("LIBGL_ALWAYS_SOFTWARE", "1");
        std::env::set_var("GALLIUM_DRIVER", "llvmpipe");
    }

    let (conn, parent) =
        create_host_parent_window().expect("a display server must be reachable (DISPLAY is set)");

    let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

    for round in 0..10 {
        let gui_ext = plugin_instance
            .plugin_handle()
            .get_extension::<PluginGui>()
            .expect("PluginGui extension not found");
        let handle = plugin_instance.plugin_handle();

        assert!(
            gui_ext.is_api_supported(&handle, x11_embedded()),
            "X11 embedded must be supported"
        );
        gui_ext
            .create(&handle, x11_embedded())
            .expect("create(X11 embedded) must succeed");
        // SAFETY: the x11rb host window outlives the plugin instance.
        unsafe {
            gui_ext
                .set_parent(
                    &handle,
                    Window::from_x11_handle(parent as std::os::raw::c_ulong),
                )
                .expect("set_parent(real X11 host window) must embed");
        }
        gui_ext.show(&handle).expect("show() must succeed");

        let child = wait_for_embedded_child(&conn, parent, std::time::Duration::from_secs(4))
            .unwrap_or_else(|| {
                panic!("round {round}: host window must acquire the embedded child")
            });
        let tree = conn
            .query_tree(child)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .unwrap_or_else(|| panic!("round {round}: embedded child must exist"));
        assert_eq!(
            tree.parent, parent,
            "round {round}: the Slint window must be a native X11 child of the host window"
        );

        if round == 0 {
            // T4.8.2 contract, asserted once (fixed per generation).
            assert_eq!(
                gui_ext.get_size(&handle),
                Some(GuiSize {
                    width: 600,
                    height: 275
                }),
                "embedded GUI keeps the fixed 600x275 logical size"
            );
            assert!(
                !gui_ext.can_resize(&handle),
                "embedded GUI is not resizable"
            );
            assert!(
                gui_ext
                    .set_size(
                        &handle,
                        GuiSize {
                            width: 600,
                            height: 275
                        }
                    )
                    .is_ok()
            );
            assert!(gui_ext.set_scale(&handle, 2.0).is_ok());
        }

        std::thread::sleep(std::time::Duration::from_millis(50));
        gui_ext.hide(&handle).expect("hide() must succeed");
        gui_ext.destroy(&handle);
        assert!(
            wait_until_child_destroyed(&conn, child, std::time::Duration::from_secs(4)),
            "round {round}: teardown must destroy the embedded child"
        );
    }
    drop(plugin_instance);

    let tree = conn
        .query_tree(parent)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .expect("host window must still exist");
    assert!(
        tree.children.is_empty(),
        "after 10 open/close cycles the host window must have zero children \
         (no orphaned embedded windows), found: {:?}",
        tree.children
    );
    eprintln!("  ✓ GUI embedded X11: 10 open/close cycles with zero leaked children");
}
