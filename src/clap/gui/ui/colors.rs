// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
//! Approved color palette and color resolution helpers.

/// Main window background color (`#1A1D23`).
pub const COL_BG: egui::Color32 = egui::Color32::from_rgb(26, 29, 35);
/// Panel and button background color (`#232830`).
pub const COL_PANEL: egui::Color32 = egui::Color32::from_rgb(35, 40, 48);
/// Border and separator color (`#2E3440`).
pub const COL_BORDER: egui::Color32 = egui::Color32::from_rgb(46, 52, 64);
/// Primary text color (`#E5E9F0`).
pub const COL_TEXT: egui::Color32 = egui::Color32::from_rgb(229, 233, 240);
/// Secondary/muted text color (`#8B95A5`).
pub const COL_MUTED: egui::Color32 = egui::Color32::from_rgb(139, 149, 165);
/// Primary accent color — default turquoise (`#00D4AA`).
pub const COL_ACCENT: egui::Color32 = egui::Color32::from_rgb(0, 212, 170);
/// Amber color for warnings and secondary highlights (`#F5A623`).
pub const COL_AMBER: egui::Color32 = egui::Color32::from_rgb(245, 166, 35);
/// VU meter green color (`#43E97B`).
pub const COL_VU_GREEN: egui::Color32 = egui::Color32::from_rgb(67, 233, 123);
/// VU meter yellow color (`#F5CE62`).
pub const COL_VU_YELLOW: egui::Color32 = egui::Color32::from_rgb(245, 206, 98);
/// VU meter red and clipping LED color (`#F74E4E`).
pub const COL_VU_RED: egui::Color32 = egui::Color32::from_rgb(247, 78, 78);
/// Bypass button color when disabled (`#4A4F5A`).
pub const COL_BYPASS_OFF: egui::Color32 = egui::Color32::from_rgb(74, 79, 90);

use crate::clap::plugin::NamClapShared;
use std::sync::atomic::Ordering;

/// Resolves the plugin's dynamic accent color.
///
/// First tries to use the track color provided by the DAW host (stored in
/// `track_accent_color` as packed ARGB). If `alpha == 0` (sentinel for
/// "no color"), returns `COL_ACCENT` (default turquoise).
pub fn resolve_accent(shared: &NamClapShared) -> egui::Color32 {
    let packed = shared.cold.track_accent_color.load(Ordering::Relaxed);
    let alpha = (packed >> 24) as u8;
    if alpha == 0 {
        COL_ACCENT
    } else {
        let red = ((packed >> 16) & 0xFF) as u8;
        let green = ((packed >> 8) & 0xFF) as u8;
        let blue = (packed & 0xFF) as u8;
        egui::Color32::from_rgba_unmultiplied(red, green, blue, alpha)
    }
}

/// Resolves a packed ARGB color in u32 to `egui::Color32`, with fallback.
///
/// Follows the same convention as `resolve_accent`: `alpha == 0` indicates no
/// color and returns the provided fallback.
pub fn resolve_color(packed: u32, fallback: egui::Color32) -> egui::Color32 {
    let alpha = (packed >> 24) as u8;
    if alpha == 0 {
        fallback
    } else {
        let red = ((packed >> 16) & 0xFF) as u8;
        let green = ((packed >> 8) & 0xFF) as u8;
        let blue = (packed & 0xFF) as u8;
        egui::Color32::from_rgba_unmultiplied(red, green, blue, alpha)
    }
}
