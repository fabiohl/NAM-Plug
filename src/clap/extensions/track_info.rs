// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Implementation of the `clap_plugin_track_info` extension for NAM-rs.

use crate::clap::plugin::NamClapMainThread;
use crate::clap::plugin::debug_assert_main_thread;
use clack_extensions::track_info::{PluginTrackInfo, PluginTrackInfoImpl};
use std::sync::atomic::Ordering;

impl<'a> PluginTrackInfoImpl for NamClapMainThread<'a> {
    fn changed(&self) {
        debug_assert_main_thread(&self.host);
        if let Some(track_info_ext) = self
            .host
            .get_extension::<clack_extensions::track_info::HostTrackInfo>()
        {
            let mut buffer = clack_extensions::track_info::TrackInfoBuffer::new();
            // SAFETY: The CLAP main thread executes this callback synchronously.
            // with_arbitrary_lifetime extends the handle's lifetime without bypassing
            // thread-safety checks — the caller guarantees we are on the main thread.
            let mut host_mut = unsafe { self.host.with_arbitrary_lifetime() };
            if let Some(info) = track_info_ext.get(&mut host_mut, &mut buffer) {
                if let Some(color) = info.color() {
                    let packed = pack_argb(color.alpha, color.red, color.green, color.blue);
                    self.shared
                        .cold
                        .track_accent_color
                        .store(packed, Ordering::Relaxed);
                } else {
                    self.shared
                        .cold
                        .track_accent_color
                        .store(0, Ordering::Relaxed);
                }
            }
        }
    }
}

/// Packs ARGB color components into a `u32` for atomic storage.
///
/// Format: `(alpha << 24) | (red << 16) | (green << 8) | blue`.
/// A value of zero (`pack_argb(0, 0, 0, 0)`) is treated as a "no color" sentinel.
/// The `resolve_accent()` function in the GUI uses `alpha == 0` as a fallback for `COL_ACCENT`.
pub fn pack_argb(alpha: u8, red: u8, green: u8, blue: u8) -> u32 {
    ((alpha as u32) << 24) | ((red as u32) << 16) | ((green as u32) << 8) | (blue as u32)
}

/// Marker type for extension registration.
pub type NamPluginTrackInfo = PluginTrackInfo;

#[cfg(test)]
mod tests {
    use super::*;

    /// Validates ARGB packing for the 5 representative colors required by spec A.1.
    #[test]
    fn test_pack_argb_representative_colors() {
        // Branco opaco: ARGB = (FF, FF, FF, FF)
        assert_eq!(pack_argb(0xFF, 0xFF, 0xFF, 0xFF), 0xFFFF_FFFF);

        // Preto opaco: ARGB = (FF, 00, 00, 00)
        assert_eq!(pack_argb(0xFF, 0x00, 0x00, 0x00), 0xFF00_0000);

        // Vermelho puro: ARGB = (FF, FF, 00, 00)
        assert_eq!(pack_argb(0xFF, 0xFF, 0x00, 0x00), 0xFFFF_0000);

        // Verde puro: ARGB = (FF, 00, FF, 00)
        assert_eq!(pack_argb(0xFF, 0x00, 0xFF, 0x00), 0xFF00_FF00);

        // Azul Bitwig #5e81ac: ARGB = (FF, 5E, 81, AC)
        assert_eq!(pack_argb(0xFF, 0x5E, 0x81, 0xAC), 0xFF5E_81AC);
    }

    /// Validates that alpha == 0 (sentinel) results in packed == 0 when RGB is also zero.
    #[test]
    fn test_pack_argb_zero_is_sentinel() {
        assert_eq!(pack_argb(0, 0, 0, 0), 0);
    }

    /// Validates bidirectional consistency: pack then unpack should recover the components.
    #[test]
    fn test_pack_argb_roundtrip() {
        let colors = [
            (0xFFu8, 0x00u8, 0xD4u8, 0xAAu8), // Turquesa #00D4AA (fallback)
            (0xFF, 0x5E, 0x81, 0xAC),         // Azul Bitwig
            (0xFF, 0xF5, 0xA6, 0x23),         // Amber
        ];
        for (alpha, red, green, blue) in colors {
            let packed = pack_argb(alpha, red, green, blue);
            assert_eq!((packed >> 24) as u8, alpha, "alpha mismatch");
            assert_eq!(((packed >> 16) & 0xFF) as u8, red, "red mismatch");
            assert_eq!(((packed >> 8) & 0xFF) as u8, green, "green mismatch");
            assert_eq!((packed & 0xFF) as u8, blue, "blue mismatch");
        }
    }
}
