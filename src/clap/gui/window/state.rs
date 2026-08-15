// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use baseview::Window;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use glow::HasContext;

use super::shaders::{FRAGMENT_SHADER_SRC, VERTEX_SHADER_SRC, compile_shader_program};

use crate::clap::plugin::NamClapSharedRef;

/// Main window representation of the NAM-rs plugin.
///
/// Manages the initialization and lifecycle of the `egui` context and the `egui_glow` painter
/// for accelerated drawing via OpenGL (glow).
pub struct NamPluginWindow {
    /// Egui Context.
    pub(crate) egui_ctx: egui::Context,
    /// Glow Painter for rendering via OpenGL.
    ///
    /// `None` when the window was created in degraded mode (GL initialization
    /// failed — R-11): the event loop closes the window on its first frame.
    pub(crate) painter: Option<egui_glow::Painter>,
    /// Accumulated raw input for Egui.
    pub(crate) raw_input: egui::RawInput,
    /// Start time for elapsed time calculations.
    pub(crate) start_time: Instant,
    /// Physical window width.
    pub(crate) width: u32,
    /// Physical window height.
    pub(crate) height: u32,
    /// Screen scale factor.
    pub(crate) scale: f32,
    /// Reference to the plugin's shared state.
    pub(crate) shared: NamClapSharedRef,
    /// Lifetime fence (shared Arc for destruction detection).
    pub(crate) alive_fence: Arc<std::sync::atomic::AtomicBool>,
    /// Static shared handle of the CLAP host.
    pub(crate) host: clack_plugin::host::HostSharedHandle<'static>,
    /// Local persistent state of the graphical interface.
    pub(crate) state: crate::clap::gui::ui::UiState,
    /// Last known mouse position.
    pub(crate) last_mouse_pos: egui::Pos2,
    /// Close signal for floating windows. When set to true, the window event
    /// loop will exit gracefully.
    pub(crate) close_signal: Arc<AtomicBool>,
    /// Instant of the last completed paint cycle (tessellate + paint + swap).
    /// Used for frame throttle and idle detection.
    pub(crate) last_paint_time: Instant,
    /// Set to `true` by `on_event` when any input event arrives; cleared after
    /// each paint cycle. Prevents frame-skipping when the user is interacting.
    pub(crate) dirty: bool,
}

impl NamPluginWindow {
    /// Initializes the main plugin window with baseview, creating the graphics context
    /// and setting up the custom dark theme.
    ///
    /// # FFI Robustness (R-11)
    ///
    /// This function is called by the GUI thread via baseview callback (C ABI). Panics that
    /// cross this boundary are UB in C++ hosts. OpenGL context or Painter initialization
    /// failures therefore return a structured error — never a panic.
    pub fn new(
        window: &mut Window,
        shared: NamClapSharedRef,
        host: clack_plugin::host::HostSharedHandle<'static>,
        close_signal: Arc<AtomicBool>,
        alive_fence: Arc<std::sync::atomic::AtomicBool>,
        scale_factor: f32,
    ) -> Result<Self, clack_plugin::plugin::PluginError> {
        if std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland") {
            log::info!("Wayland session detected, using XDG portal for file dialogs");
        }

        let (painter, vu_program, vu_vao) = Self::init_graphics(window.gl_context())?;

        let egui_ctx = egui::Context::default();

        // Custom visuals for premium dark styling
        let mut visuals = egui::Visuals::dark();
        visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(18, 18, 24);
        visuals.widgets.noninteractive.weak_bg_fill = egui::Color32::from_rgb(25, 25, 35);
        visuals.widgets.noninteractive.bg_stroke =
            egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(45, 45, 60));
        visuals.widgets.noninteractive.fg_stroke =
            egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(200, 200, 210));
        visuals.selection.bg_fill = egui::Color32::from_rgb(120, 90, 230); // Vibrant purple accent
        egui_ctx.set_visuals(visuals);

        let width = crate::clap::gui::GUI_WIDTH;
        let height = crate::clap::gui::GUI_HEIGHT;
        let scale = scale_factor;

        let mut raw_input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width as f32, height as f32),
            )),
            ..Default::default()
        };

        if let Some(info) = raw_input.viewports.get_mut(&egui::ViewportId::ROOT) {
            info.native_pixels_per_point = Some(scale);
        }

        let state = crate::clap::gui::ui::UiState {
            vu_program,
            vu_vao,
            ..Default::default()
        };

        Ok(Self {
            egui_ctx,
            painter: Some(painter),
            raw_input,
            start_time: Instant::now(),
            width,
            height,
            scale,
            shared,
            alive_fence,
            host,
            state,
            last_mouse_pos: egui::Pos2::ZERO,
            close_signal,
            last_paint_time: Instant::now(),
            dirty: true,
        })
    }

    /// Initializes the glow painter and VU shader program from an optional
    /// OpenGL context (R-11).
    ///
    /// A missing context — headless sessions, SSH/X11 forwarding, broken GPU
    /// drivers — yields a structured error instead of panicking through the
    /// baseview/CLAP FFI boundary.
    fn init_graphics(
        gl_ctx: Option<&baseview::gl::GlContext>,
    ) -> Result<
        (
            egui_glow::Painter,
            Option<glow::Program>,
            Option<glow::VertexArray>,
        ),
        clack_plugin::plugin::PluginError,
    > {
        let gl_ctx = gl_ctx.ok_or(clack_plugin::plugin::PluginError::Message(
            "OpenGL context not available — GUI initialization failed",
        ))?;
        // SAFETY: FFI call, host pointer transmute, or raw graphics context access with verified lifetimes.
        unsafe {
            gl_ctx.make_current();
        }

        // SAFETY: FFI call, host pointer transmute, or raw graphics context access with verified lifetimes.
        let gl = unsafe { glow::Context::from_loader_function(|s| gl_ctx.get_proc_address(s)) };

        let painter = egui_glow::Painter::new(Arc::new(gl), "", None, true).map_err(|err| {
            clack_plugin::plugin::PluginError::Message(Box::leak(
                format!("Failed to create egui_glow Painter: {err}").into_boxed_str(),
            ))
        })?;

        // Robust shader compilation and VAO creation
        // SAFETY: FFI call, host pointer transmute, or raw graphics context access with verified lifetimes.
        let (vu_program, vu_vao) = unsafe {
            let gl = painter.gl();
            match compile_shader_program(gl, VERTEX_SHADER_SRC, FRAGMENT_SHADER_SRC) {
                Ok(prog) => {
                    let vao = gl.create_vertex_array().ok();
                    (Some(prog), vao)
                }
                Err(err) => {
                    log::error!("Failed to compile VU meter GLSL shaders: {}", err);
                    (None, None)
                }
            }
        };

        // SAFETY: FFI call, host pointer transmute, or raw graphics context access with verified lifetimes.
        unsafe {
            gl_ctx.make_not_current();
        }

        Ok((painter, vu_program, vu_vao))
    }

    /// Builds a degraded (stub) window handler used when GL initialization
    /// fails (R-11).
    ///
    /// The baseview build callback must always return a handler, so on
    /// failure the extension records the error, returns it to the host and
    /// installs this stub — its first `on_frame` closes the window without
    /// touching the GL context, the shared state or the host handle.
    pub fn degraded(
        shared: NamClapSharedRef,
        host: clack_plugin::host::HostSharedHandle<'static>,
        close_signal: Arc<AtomicBool>,
        alive_fence: Arc<std::sync::atomic::AtomicBool>,
        scale_factor: f32,
    ) -> Self {
        Self {
            egui_ctx: egui::Context::default(),
            painter: None,
            raw_input: egui::RawInput::default(),
            start_time: Instant::now(),
            width: crate::clap::gui::GUI_WIDTH,
            height: crate::clap::gui::GUI_HEIGHT,
            scale: scale_factor,
            shared,
            alive_fence,
            host,
            state: crate::clap::gui::ui::UiState::default(),
            last_mouse_pos: egui::Pos2::ZERO,
            close_signal,
            last_paint_time: Instant::now(),
            dirty: false,
        }
    }

    pub(crate) fn safe_shared(&self) -> Option<&'static crate::clap::plugin::NamClapShared> {
        if self.alive_fence.load(std::sync::atomic::Ordering::Acquire) {
            // pairs with Release store em plugin/shared.rs:262
            // SAFETY: If alive_fence is true, the plugin and its shared state
            // are still alive in memory, so the pointer is valid.
            unsafe { Some(self.shared.as_ref()) }
        } else {
            None
        }
    }

    /// Destroys all OpenGL resources created by this window.
    ///
    /// Must be called with the GL context *current*. Idempotent — safe to call
    /// multiple times. No-op for degraded windows (no GL resources were ever
    /// created — R-11).
    pub(crate) fn destroy_gl_resources(&mut self) {
        let Some(painter) = &mut self.painter else {
            return;
        };
        // SAFETY: FFI call, host pointer transmute, or raw graphics context access with verified lifetimes.
        // Clone the Arc so the glow context remains alive after painter.destroy().
        let gl = std::sync::Arc::clone(painter.gl());
        painter.destroy();

        // SAFETY: FFI call, host pointer transmute, or raw graphics context access with verified lifetimes.
        // take() ensures idempotency — second call is a no-op.
        unsafe {
            if let Some(vu_vao) = self.state.vu_vao.take() {
                gl.delete_vertex_array(vu_vao);
            }
            if let Some(vu_program) = self.state.vu_program.take() {
                gl.delete_program(vu_program);
            }
        }
    }
}

#[cfg(test)]
impl NamPluginWindow {
    pub(crate) fn test_init() {
        assert!(core::mem::size_of::<Self>() <= 4096);
        assert_eq!(core::mem::align_of::<Self>() % 8, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clap::plugin::make_test_shared;
    use clack_plugin::host::HostSharedHandle;
    use clack_plugin::plugin::PluginError;
    use std::sync::atomic::Ordering;

    #[test]
    fn test_init() {
        NamPluginWindow::test_init();
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
    // panics crossing the baseview/CLAP FFI boundary (S6.T2).
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
        let window =
            NamPluginWindow::degraded(shared_ref, host_static, close_signal, alive_fence, 1.0);
        assert!(
            window.painter.is_none(),
            "degraded window has no GL painter"
        );

        // Dropping the degraded window must not panic (painter is None).
        drop(window);
        // And the shared state survives independently of the GUI failure.
        assert!(shared.cold.alive_fence.load(Ordering::Relaxed));
    }
}
