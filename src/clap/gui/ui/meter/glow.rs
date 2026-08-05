// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
//! OpenGL (glow) rendering path for the vertical VU meter.

use glow::HasContext;
use std::sync::Arc;
use std::sync::OnceLock;

use super::super::state::{VuMeterSharedState, VuUniforms};

/// Renders the VU meter bar using OpenGL via glow.
/// Returns `true` if the GL path was used; `false` if the caller should fall back to CPU.
#[expect(
    clippy::too_many_arguments,
    reason = "Meter glow render component with many OpenGL/visual configuration parameters for flexible rendering"
)]
pub(crate) fn render_glow(
    ui: &egui::Ui,
    vu_program: Option<glow::Program>,
    vu_vao: Option<glow::VertexArray>,
    vu_shared_state: &mut Option<Arc<VuMeterSharedState>>,
    vu_callback: &mut Option<Arc<egui_glow::CallbackFn>>,
    vu_uniforms: Arc<OnceLock<VuUniforms>>,
    meter_rect: egui::Rect,
    peak_frac: f32,
    hold_frac: f32,
    hold_color_type: i32,
) -> bool {
    let enabled = ui.is_enabled();
    if !enabled {
        return false;
    }
    let (Some(program), Some(vao)) = (vu_program, vu_vao) else {
        return false;
    };

    let shared_state =
        vu_shared_state.get_or_insert_with(|| Arc::new(VuMeterSharedState::default()));
    let callback = vu_callback.get_or_insert_with(|| {
        let shared_state_capture = Arc::clone(shared_state);
        let uniforms_capture = Arc::clone(&vu_uniforms);
        Arc::new(egui_glow::CallbackFn::new(move |info, painter| {
            let gl = painter.gl();
            let peak_frac = f32::from_bits(
                shared_state_capture
                    .peak_frac
                    .load(std::sync::atomic::Ordering::Relaxed),
            );
            let hold_frac = f32::from_bits(
                shared_state_capture
                    .hold_frac
                    .load(std::sync::atomic::Ordering::Relaxed),
            );
            let hold_color_type = shared_state_capture
                .hold_color_type
                .load(std::sync::atomic::Ordering::Relaxed);
            let rect_min_x = f32::from_bits(
                shared_state_capture
                    .rect_min_x
                    .load(std::sync::atomic::Ordering::Relaxed),
            );
            let rect_min_y = f32::from_bits(
                shared_state_capture
                    .rect_min_y
                    .load(std::sync::atomic::Ordering::Relaxed),
            );
            let rect_max_x = f32::from_bits(
                shared_state_capture
                    .rect_max_x
                    .load(std::sync::atomic::Ordering::Relaxed),
            );
            let rect_max_y = f32::from_bits(
                shared_state_capture
                    .rect_max_y
                    .load(std::sync::atomic::Ordering::Relaxed),
            );

            let ppp = info.pixels_per_point;
            let p_left = rect_min_x * ppp;
            let p_top = rect_min_y * ppp;
            let p_right = rect_max_x * ppp;
            let p_bottom = rect_max_y * ppp;

            let vp_left = info.viewport.min.x;
            let vp_top = info.viewport.min.y;
            let vp_right = info.viewport.max.x;
            let vp_bottom = info.viewport.max.y;

            // SAFETY: FFI call, host pointer transmute, or raw graphics context access with verified lifetimes.
            unsafe {
                gl.enable(glow::BLEND);
                gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);

                gl.use_program(Some(program));
                gl.bind_vertex_array(Some(vao));

                let uniforms = uniforms_capture.get_or_init(|| VuUniforms {
                    loc_viewport: gl.get_uniform_location(program, "u_viewport"),
                    loc_meter_rect: gl.get_uniform_location(program, "u_meter_rect"),
                    loc_peak_frac: gl.get_uniform_location(program, "u_peak_frac"),
                    loc_hold_frac: gl.get_uniform_location(program, "u_hold_frac"),
                    loc_hold_color_type: gl.get_uniform_location(program, "u_hold_color_type"),
                });

                if let Some(ref loc) = uniforms.loc_viewport {
                    gl.uniform_4_f32(Some(loc), vp_left, vp_top, vp_right, vp_bottom);
                }
                if let Some(ref loc) = uniforms.loc_meter_rect {
                    gl.uniform_4_f32(Some(loc), p_left, p_top, p_right, p_bottom);
                }
                if let Some(ref loc) = uniforms.loc_peak_frac {
                    gl.uniform_1_f32(Some(loc), peak_frac);
                }
                if let Some(ref loc) = uniforms.loc_hold_frac {
                    gl.uniform_1_f32(Some(loc), hold_frac);
                }
                if let Some(ref loc) = uniforms.loc_hold_color_type {
                    gl.uniform_1_i32(Some(loc), hold_color_type);
                }

                gl.draw_arrays(glow::TRIANGLES, 0, 6);

                gl.bind_vertex_array(None);
                gl.use_program(None);
                gl.disable(glow::BLEND);
            }
        }))
    });

    shared_state
        .peak_frac
        .store(peak_frac.to_bits(), std::sync::atomic::Ordering::Relaxed);
    shared_state
        .hold_frac
        .store(hold_frac.to_bits(), std::sync::atomic::Ordering::Relaxed);
    shared_state
        .hold_color_type
        .store(hold_color_type, std::sync::atomic::Ordering::Relaxed);
    shared_state.rect_min_x.store(
        meter_rect.min.x.to_bits(),
        std::sync::atomic::Ordering::Relaxed,
    );
    shared_state.rect_min_y.store(
        meter_rect.min.y.to_bits(),
        std::sync::atomic::Ordering::Relaxed,
    );
    shared_state.rect_max_x.store(
        meter_rect.max.x.to_bits(),
        std::sync::atomic::Ordering::Relaxed,
    );
    shared_state.rect_max_y.store(
        meter_rect.max.y.to_bits(),
        std::sync::atomic::Ordering::Relaxed,
    );

    let callback_fn = Arc::clone(callback);
    let cb = egui::PaintCallback {
        rect: meter_rect,
        callback: callback_fn,
    };
    ui.painter().add(egui::Shape::Callback(cb));
    true
}
