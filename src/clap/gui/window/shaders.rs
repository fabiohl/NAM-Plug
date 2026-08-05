// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use glow::HasContext;

/// GLSL vertex shader for the VU meter.
///
/// Geometry: generates a quad (2 triangles, 6 vertices) from the `u_viewport` and
/// `u_meter_rect` uniforms. The meter rectangle coordinates are converted to NDC (Normalized
/// Device Coordinates) using the egui viewport. The `v_uv` attribute (0→1 on both axes)
/// is interpolated for use in the fragment shader.
pub(crate) const VERTEX_SHADER_SRC: &str = r#"#version 330 core
uniform vec4 u_viewport;
uniform vec4 u_meter_rect;
out vec2 v_uv;

void main() {
    int id = gl_VertexID;
    vec2 pos = vec2(0.0);
    vec2 uv = vec2(0.0);
    
    float left = u_meter_rect.x - 2.0;
    float right = u_meter_rect.z + 2.0;
    float top = u_meter_rect.y;
    float bottom = u_meter_rect.w;
    
    if (id == 0) {
        pos = vec2(left, bottom);
        uv = vec2(0.0, 0.0);
    } else if (id == 1) {
        pos = vec2(right, bottom);
        uv = vec2(1.0, 0.0);
    } else if (id == 2) {
        pos = vec2(left, top);
        uv = vec2(0.0, 1.0);
    } else if (id == 3) {
        pos = vec2(left, top);
        uv = vec2(0.0, 1.0);
    } else if (id == 4) {
        pos = vec2(right, bottom);
        uv = vec2(1.0, 0.0);
    } else if (id == 5) {
        pos = vec2(right, top);
        uv = vec2(1.0, 1.0);
    }
    
    float ndc_x = (pos.x - u_viewport.x) / (u_viewport.z - u_viewport.x) * 2.0 - 1.0;
    float ndc_y = 1.0 - (pos.y - u_viewport.y) / (u_viewport.w - u_viewport.y) * 2.0;
    
    gl_Position = vec4(ndc_x, ndc_y, 0.0, 1.0);
    v_uv = uv;
}
"#;

/// GLSL fragment shader for the VU meter.
///
/// Implements a vertical meter with a three-color gradient based on dB thresholds:
/// - **Green** (`COL_VU_GREEN`): up to `green_frac` (≈ -12 dBFS → 48/66 of range)
/// - **Yellow** (`COL_VU_YELLOW`): from `green_frac` up to `yellow_frac` (≈ -3 dBFS → 57/66)
/// - **Red** (`COL_VU_RED`): above `yellow_frac`
///
/// The peak hold indicator is rendered as a thin horizontal line (1.5px)
/// at position `u_hold_frac`, colored according to `u_hold_color_type` (0=green, 1=yellow, 2=red).
/// Rounded corners (radius 1.5px) are applied via distance field in texture space.
pub(crate) const FRAGMENT_SHADER_SRC: &str = r#"#version 330 core
precision mediump float;
in vec2 v_uv;
out vec4 f_color;

uniform float u_peak_frac;
uniform float u_hold_frac;
uniform int u_hold_color_type;

const vec4 COL_BG = vec4(0.10196, 0.11373, 0.13725, 1.0);
const vec4 COL_VU_GREEN = vec4(0.26275, 0.91373, 0.48235, 1.0);
const vec4 COL_VU_YELLOW = vec4(0.96078, 0.80784, 0.38431, 1.0);
const vec4 COL_VU_RED = vec4(0.96863, 0.30588, 0.30588, 1.0);

const float green_frac = 48.0 / 66.0;
const float yellow_frac = 57.0 / 66.0;

void main() {
    float line_half_h = 0.75 / 130.0;
    if (u_hold_frac > 0.0 && abs(v_uv.y - u_hold_frac) <= line_half_h) {
        vec4 hold_color = COL_VU_GREEN;
        if (u_hold_color_type == 1) {
            hold_color = COL_VU_YELLOW;
        } else if (u_hold_color_type == 2) {
            hold_color = COL_VU_RED;
        }
        f_color = hold_color;
        return;
    }
    
    if (v_uv.x >= 0.1 && v_uv.x <= 0.9) {
        vec2 size = vec2(16.0, 130.0);
        vec2 pos = vec2((v_uv.x - 0.1) / 0.8 * size.x, v_uv.y * size.y);
        float r = 1.5;
        
        bool in_corner = false;
        vec2 corner_center = vec2(0.0);
        if (pos.x < r && pos.y < r) {
            in_corner = true;
            corner_center = vec2(r, r);
        } else if (pos.x > size.x - r && pos.y < r) {
            in_corner = true;
            corner_center = vec2(size.x - r, r);
        } else if (pos.x < r && pos.y > size.y - r) {
            in_corner = true;
            corner_center = vec2(r, size.y - r);
        } else if (pos.x > size.x - r && pos.y > size.y - r) {
            in_corner = true;
            corner_center = vec2(size.x - r, size.y - r);
        }
        
        if (in_corner && distance(pos, corner_center) > r) {
            discard;
        }
        
        if (v_uv.y <= u_peak_frac) {
            if (v_uv.y <= green_frac) {
                f_color = COL_VU_GREEN;
            } else if (v_uv.y <= yellow_frac) {
                f_color = COL_VU_YELLOW;
            } else {
                f_color = COL_VU_RED;
            }
        } else {
            f_color = COL_BG;
        }
    } else {
        discard;
    }
}
"#;

/// Compiles and links an OpenGL program from GLSL vertex and fragment sources.
///
/// In case of compilation or linking errors, all partial resources (shaders, program)
/// are cleaned up before returning the error. Shaders are detached and deleted after
/// successful linking, following OpenGL best practices.
pub(crate) fn compile_shader_program(
    gl: &glow::Context,
    vertex_source: &str,
    fragment_source: &str,
) -> Result<glow::Program, String> {
    // SAFETY: FFI call, host pointer transmute, or raw graphics context access with verified lifetimes.
    unsafe {
        let program = gl.create_program()?;

        let shader_sources = [
            (glow::VERTEX_SHADER, vertex_source),
            (glow::FRAGMENT_SHADER, fragment_source),
        ];

        let mut shaders = Vec::new();
        for (shader_type, shader_source) in shader_sources {
            let shader = gl.create_shader(shader_type)?;
            gl.shader_source(shader, shader_source);
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                let log = gl.get_shader_info_log(shader);
                for s in &shaders {
                    gl.detach_shader(program, *s);
                    gl.delete_shader(*s);
                }
                gl.delete_program(program);
                return Err(format!("Shader compilation failed: {log}"));
            }
            gl.attach_shader(program, shader);
            shaders.push(shader);
        }

        gl.link_program(program);
        if !gl.get_program_link_status(program) {
            let log = gl.get_program_info_log(program);
            for s in shaders {
                gl.detach_shader(program, s);
                gl.delete_shader(s);
            }
            gl.delete_program(program);
            return Err(format!("Program link failed: {log}"));
        }

        for s in shaders {
            gl.detach_shader(program, s);
            gl.delete_shader(s);
        }

        Ok(program)
    }
}
