// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::state::NamPluginWindow;

/// Best-effort GL resource cleanup — `destroy()` checks an internal
/// idempotency flag, so it is safe even when `destroy_gl_resources` was
/// already called in `on_frame`. The GL context may not be current here;
/// without a current context, OpenGL delete operations become no-ops or
/// silently fail (no panics). Degraded windows (no painter) are no-ops.
impl Drop for NamPluginWindow {
    fn drop(&mut self) {
        if let Some(painter) = &mut self.painter {
            painter.destroy();
        }
    }
}
