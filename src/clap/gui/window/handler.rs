// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use baseview::{
    DropEffect, Event, EventStatus, MouseEvent, ScrollDelta, Window, WindowEvent, WindowHandler,
};
use clack_extensions::gui::HostGui;
use std::sync::atomic::Ordering;

use super::NamPluginWindow;
use super::drag_drop::get_valid_model_file;
use super::input_map::{
    SCROLL_LINES_TO_POINTS, map_keyboard_event, map_modifiers, map_mouse_button,
};

impl WindowHandler for NamPluginWindow {
    fn on_frame(&mut self, window: &mut Window) {
        // Unlike `new()`, `on_frame()` is called repeatedly by the baseview
        // rendering loop (C ABI). A panic here would cross the FFI boundary and
        // cause UB in C++ hosts. We use a silent early-return as a safe fallback.
        //
        // Exit conditions:
        // - the close signal was set (window closed by the user or teardown);
        // - the alive fence is down (plugin destruction in progress — R-09:
        //   the event loop must become a no-op and stop dereferencing shared
        //   state and the host handle);
        // - the window is a degraded stub (GL initialization failed — R-11).
        if self.close_signal.load(Ordering::Relaxed)
            || !self.alive_fence.load(Ordering::Acquire)
            || self.painter.is_none()
        {
            if let Some(gl_ctx) = window.gl_context() {
                // SAFETY: FFI call, host pointer transmute, or raw graphics context access with verified lifetimes.
                unsafe {
                    gl_ctx.make_current();
                }
                self.destroy_gl_resources();
                // SAFETY: FFI call, host pointer transmute, or raw graphics context access with verified lifetimes.
                unsafe {
                    gl_ctx.make_not_current();
                }
            }
            window.close();
            return;
        }

        // Idle-skip (CLAP-F022): when the window is truly static — no input
        // events, VU meters stable, no animations active — skip the entire
        // GL context acquisition, egui run, and paint cycle. This drops CPU
        // usage to ~0% when the window is open but idle.
        //
        // We need to check peak values BEFORE run_ui() (otherwise we never
        // detect the transition from silent→audio because hold timestamps
        // are only updated inside run_ui). Cached peak comparison against
        // shared atomics gives us a cheap change detector.
        let mut peaks_changed = false;
        if let Some(shared) = self.safe_shared() {
            let peak_l = f32::from_bits(shared.rt_to_ui.ui_peak_l.load(Ordering::Relaxed));
            let peak_r = f32::from_bits(shared.rt_to_ui.ui_peak_r.load(Ordering::Relaxed));
            peaks_changed =
                peak_l != self.state.cached_peak_l || peak_r != self.state.cached_peak_r;
        }

        if !self.dirty && !self.state.has_active_animations() && !peaks_changed {
            return;
        }

        let Some(gl_ctx) = window.gl_context() else {
            return;
        };
        // SAFETY: FFI call, host pointer transmute, or raw graphics context access with verified lifetimes.
        unsafe {
            gl_ctx.make_current();
        }

        if let Some(shared) = self.safe_shared() {
            // Cache peak values for idle detection on the next frame.
            self.state.cached_peak_l =
                f32::from_bits(shared.rt_to_ui.ui_peak_l.load(Ordering::Relaxed));
            self.state.cached_peak_r =
                f32::from_bits(shared.rt_to_ui.ui_peak_r.load(Ordering::Relaxed));

            let mut raw_input = self.raw_input.take();
            raw_input.time = Some(self.start_time.elapsed().as_secs_f64());

            let logical_width = self.width as f32 / self.scale;
            let logical_height = self.height as f32 / self.scale;
            raw_input.screen_rect = Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(logical_width, logical_height),
            ));

            if let Some(info) = raw_input.viewports.get_mut(&egui::ViewportId::ROOT) {
                info.native_pixels_per_point = Some(self.scale);
            }

            // Snapshot peak-hold values before run_ui to detect active decay.
            let hold_l_before = self.state.peak_l_hold;
            let hold_r_before = self.state.peak_r_hold;

            let mut full_output = self.egui_ctx.run_ui(raw_input, |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    crate::clap::gui::ui::draw_ui(ui, shared, &self.host, &mut self.state);
                });
            });

            // Handle CopyText platform command (egui's "Copy" button output).
            // Writes the copied text to the system clipboard via arboard.
            for cmd in &full_output.platform_output.commands {
                if let egui::OutputCommand::CopyText(text) = cmd {
                    use arboard::Clipboard;
                    match Clipboard::new() {
                        Ok(mut clipboard) => {
                            if let Err(e) = clipboard.set_text(text.clone()) {
                                log::warn!("NAM-rs: Clipboard set_text failed: {e}");
                            } else {
                                self.state.toast_expiration = Some(
                                    std::time::Instant::now() + std::time::Duration::from_secs(5),
                                );
                            }
                        }
                        Err(e) => {
                            log::warn!("NAM-rs: Failed to open clipboard: {e}");
                        }
                    }
                }
            }

            // Determine whether a full paint cycle is needed. Skip when all of:
            // (a) egui requested a long (or no) repaint delay — no pending animations,
            // (b) no input events arrived since last paint (`dirty` is false),
            // (c) peak-hold meters are not in active decay.
            let repaint_delay = full_output
                .viewport_output
                .get(&egui::ViewportId::ROOT)
                .map(|vo| vo.repaint_delay);

            let has_short_repaint = repaint_delay.is_some_and(|d| d.as_secs_f64() < 0.050);

            // Hold values change when audio is playing (hold tracks peak) or when
            // hold decays after >2 s of silence. Static hold means true idle.
            let hold_changed =
                self.state.peak_l_hold != hold_l_before || self.state.peak_r_hold != hold_r_before;

            // Throttle: minimum interval between paint cycles to honour the
            // ~30–45 fps target when active (original 30 ms intent).
            let time_since_paint = self.last_paint_time.elapsed();

            let should_skip = !self.dirty
                && !has_short_repaint
                && !hold_changed
                && time_since_paint < std::time::Duration::from_millis(22);

            if !should_skip {
                let clipped_primitives = self
                    .egui_ctx
                    .tessellate(full_output.shapes, full_output.pixels_per_point);

                let screen_size = [self.width, self.height];
                // Background color: #1A1D23 (approved dark mode palette)
                if let Some(painter) = self.painter.as_mut() {
                    painter.clear(screen_size, [0.102, 0.114, 0.137, 1.0]);
                    painter.paint_and_update_textures(
                        screen_size,
                        full_output.pixels_per_point,
                        &clipped_primitives,
                        &mut full_output.textures_delta,
                    );
                }

                gl_ctx.swap_buffers();
                self.last_paint_time = std::time::Instant::now();
            } else {
                full_output.textures_delta.clear();
            }

            self.dirty = false;
        }

        // SAFETY: FFI call, host pointer transmute, or raw graphics context access with verified lifetimes.
        unsafe {
            gl_ctx.make_not_current();
        }
    }

    /// Processes window events (mouse, keyboard, resize, drag-and-drop).
    ///
    /// Converts baseview events to the `egui::RawInput` format that will be consumed
    /// in the next `on_frame()`. For NAM model drag-and-drop, communicates directly
    /// with `NamClapShared` to enqueue loading via `ui_pending_model`.
    fn on_event(&mut self, _window: &mut Window, event: Event) -> EventStatus {
        self.dirty = true;

        match event {
            Event::Window(WindowEvent::WillClose) => {
                // User closed the window via the window manager (X button, Alt-F4, etc.).
                // Notify the host that the GUI was closed externally, then
                // clean up GL resources and stop the render loop.
                //
                // R-09: the host notification is a no-op when the alive fence
                // is down — during plugin destruction the host handle must not
                // be dereferenced after teardown began.
                if self.alive_fence.load(Ordering::Acquire)
                    && let Some(gui_host) = self.host.get_extension::<HostGui>()
                {
                    gui_host.closed(&self.host, false);
                }
                self.close_signal.store(true, Ordering::Release);
                EventStatus::Captured
            }
            Event::Window(WindowEvent::Resized(window_info)) => {
                self.scale = window_info.scale() as f32;
                let size = window_info.logical_size();
                self.width = window_info.physical_size().width;
                self.height = window_info.physical_size().height;
                self.raw_input.screen_rect = Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(size.width as f32, size.height as f32),
                ));
                if let Some(info) = self.raw_input.viewports.get_mut(&egui::ViewportId::ROOT) {
                    info.native_pixels_per_point = Some(self.scale);
                }
                EventStatus::Captured
            }
            Event::Mouse(mouse_event) => {
                match mouse_event {
                    MouseEvent::CursorMoved { position, .. } => {
                        let egui_pos = egui::pos2(position.x as f32, position.y as f32);
                        self.last_mouse_pos = egui_pos;
                        self.raw_input
                            .events
                            .push(egui::Event::PointerMoved(egui_pos));
                    }
                    MouseEvent::ButtonPressed { button, modifiers } => {
                        if let Some(egui_button) = map_mouse_button(button) {
                            self.raw_input.events.push(egui::Event::PointerButton {
                                pos: self.last_mouse_pos,
                                button: egui_button,
                                pressed: true,
                                modifiers: map_modifiers(modifiers),
                            });
                        }
                    }
                    MouseEvent::ButtonReleased { button, modifiers } => {
                        if let Some(egui_button) = map_mouse_button(button) {
                            self.raw_input.events.push(egui::Event::PointerButton {
                                pos: self.last_mouse_pos,
                                button: egui_button,
                                pressed: false,
                                modifiers: map_modifiers(modifiers),
                            });
                        }
                    }
                    MouseEvent::WheelScrolled { delta, modifiers } => {
                        let (unit, delta_vec) = match delta {
                            ScrollDelta::Lines { x, y } => (
                                egui::MouseWheelUnit::Line,
                                egui::vec2(x * SCROLL_LINES_TO_POINTS, y * SCROLL_LINES_TO_POINTS),
                            ),
                            ScrollDelta::Pixels { x, y } => {
                                (egui::MouseWheelUnit::Point, egui::vec2(x, y))
                            }
                        };
                        self.raw_input.events.push(egui::Event::MouseWheel {
                            unit,
                            delta: delta_vec,
                            modifiers: map_modifiers(modifiers),
                            phase: egui::TouchPhase::Move,
                        });
                    }
                    MouseEvent::CursorLeft => {
                        self.raw_input.events.push(egui::Event::PointerGone);
                    }
                    MouseEvent::DragEntered { data, .. } | MouseEvent::DragMoved { data, .. } => {
                        if get_valid_model_file(&data).is_some() {
                            self.state.drag_active = true;
                            return EventStatus::AcceptDrop(DropEffect::Copy);
                        } else {
                            self.state.drag_active = false;
                            return EventStatus::Ignored;
                        }
                    }
                    MouseEvent::DragLeft => {
                        self.state.drag_active = false;
                        return EventStatus::Captured;
                    }
                    MouseEvent::DragDropped { data, .. } => {
                        self.state.drag_active = false;
                        if let Some(path) = get_valid_model_file(&data) {
                            #[expect(
                                clippy::collapsible_if,
                                reason = "Nested if expresses two independent guard conditions — collapsing would reduce clarity"
                            )]
                            if let Some(shared) = self.safe_shared() {
                                if let Ok(mut pending_guard) = shared.cold.ui_pending_model.lock() {
                                    *pending_guard = Some(path);
                                    shared
                                        .cold
                                        .ui_loading
                                        .store(true, std::sync::atomic::Ordering::Relaxed);
                                    // R-09: fence re-check immediately before
                                    // the host call — the fence may have
                                    // dropped between safe_shared() and here.
                                    if self.alive_fence.load(Ordering::Acquire) {
                                        self.host.request_callback();
                                    }
                                }
                            }
                            return EventStatus::AcceptDrop(DropEffect::Copy);
                        } else {
                            return EventStatus::Ignored;
                        }
                    }
                    _ => {}
                }
                EventStatus::Captured
            }
            Event::Keyboard(key_event) => {
                let pressed = match key_event.state {
                    keyboard_types::KeyState::Down => true,
                    keyboard_types::KeyState::Up => false,
                };

                if let (true, keyboard_types::Key::Character(s)) = (pressed, &key_event.key) {
                    let has_modifiers = key_event
                        .modifiers
                        .contains(keyboard_types::Modifiers::CONTROL)
                        || key_event
                            .modifiers
                            .contains(keyboard_types::Modifiers::META);
                    if !has_modifiers {
                        self.raw_input.events.push(egui::Event::Text(s.clone()));
                    }
                }

                if let Some(egui_event) = map_keyboard_event(&key_event) {
                    self.raw_input.events.push(egui_event);
                    EventStatus::Captured
                } else {
                    EventStatus::Ignored
                }
            }
            _ => EventStatus::Ignored,
        }
    }
}
