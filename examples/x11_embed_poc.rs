// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! X11 embedding proof-of-concept (E4 / Sprint 7 — Opção A).
//!
//! Proves that a Slint 1.17 window can be created as a **native X11 child**
//! (XEmbed) of an arbitrary host window, **without forking Slint**, via the
//! `unstable-winit-030` feature: `BackendSelector::with_winit_window_attributes_hook`
//! + `winit::platform::x11::WindowAttributesExtX11::with_embed_parent_window`.
//!
//! Run against any live X server (a real session, `Xvfb :99`, or `Xephyr`):
//!
//! ```text
//! DISPLAY=:0 cargo run --example x11_embed_poc
//! ```
//!
//! The process creates a small host parent window with `x11rb`, embeds the real
//! `MainWindow` into it, verifies the parent/child relationship via
//! `XCB query_tree`, runs the Slint event loop briefly, and exits cleanly.

use slint::winit_030::{SlintEvent, WinitWindowAccessor, winit};
use winit::platform::x11::{EventLoopBuilderExtX11, WindowAttributesExtX11};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, CreateWindowAux, EventMask, WindowClass};

slint::include_modules!();

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Create a minimal "host" top-level X11 window to act as the embed parent.
    let (conn, screen_num) = x11rb::connect(None)?;
    let screen = &conn.setup().roots[screen_num];
    let parent: x11rb::protocol::xproto::Window = conn.generate_id()?;
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
    )?;
    conn.map_window(parent)?;
    conn.flush()?;
    println!("POC: host parent X11 window = {parent:#x}");

    // 2. Select the winit backend with (a) X11 forced — matching what the plugin
    //    will do when the host negotiates `CLAP_WINDOW_API_X11` — and (b) an embed
    //    hook applied to every window creation. The host window id is captured by
    //    value (a `u32` XID), so no raw pointer escapes to the GUI thread.
    let parent_id: winit::platform::x11::XWindow = parent;
    let mut event_loop_builder = winit::event_loop::EventLoop::<SlintEvent>::with_user_event();
    event_loop_builder.with_x11();
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .with_winit_event_loop_builder(event_loop_builder)
        .with_winit_window_attributes_hook(move |attrs| attrs.with_embed_parent_window(parent_id))
        .select()?;

    // 3. Create the real Slint window — winit creates it already parented and
    //    sets the `_XEMBED` property (version 0, mapped).
    let window = MainWindow::new()?;
    window.show()?;

    // 4. Self-verify from inside the event loop (the winit window only exists
    //    once the loop is active): resolve the Slint window's X11 id and confirm
    //    the X server reports the host window as its parent.
    let weak = window.as_weak();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::SingleShot,
        std::time::Duration::from_millis(500),
        move || {
            let child = weak.upgrade().and_then(|w| {
                w.window()
                    .with_winit_window(|win| {
                        win.window_handle().ok().and_then(|h| match h.as_raw() {
                            RawWindowHandle::Xlib(x) => Some(x.window),
                            _ => None,
                        })
                    })
                    .flatten()
            });
            match child {
                Some(child) => {
                    let child_xid = child as x11rb::protocol::xproto::Window;
                    let tree = conn
                        .query_tree(child_xid)
                        .ok()
                        .and_then(|cookie| cookie.reply().ok());
                    match tree {
                        Some(tree) => {
                            let ok = tree.parent == parent;
                            println!(
                                "POC: Slint window = {child_xid:#x}, parent = {:#x} \
                                 (expected {parent:#x}) -> {}",
                                tree.parent,
                                if ok {
                                    "EMBEDDED OK"
                                } else {
                                    "EMBEDDING FAILED"
                                }
                            );
                        }
                        None => println!("POC: query_tree failed"),
                    }
                }
                None => println!("POC: could not resolve Slint window X11 id (backend mismatch)"),
            }
            let _ = slint::quit_event_loop();
        },
    );

    // 5. Run the event loop, then exit.
    let _ = slint::run_event_loop();
    println!("POC: Slint event loop exited cleanly");
    Ok(())
}
