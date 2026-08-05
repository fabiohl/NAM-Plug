// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use crate::clap::plugin::NamClapShared;
use std::sync::atomic::Ordering;

#[inline(always)]
pub(super) fn store_peaks(shared: &NamClapShared, peak_l: f32, peak_r: f32) {
    let current_peak_l = f32::from_bits(shared.rt_to_ui.ui_peak_l.load(Ordering::Relaxed));
    if peak_l > current_peak_l {
        shared
            .rt_to_ui
            .ui_peak_l
            .store(peak_l.to_bits(), Ordering::Relaxed);
    }
    let current_peak_r = f32::from_bits(shared.rt_to_ui.ui_peak_r.load(Ordering::Relaxed));
    if peak_r > current_peak_r {
        shared
            .rt_to_ui
            .ui_peak_r
            .store(peak_r.to_bits(), Ordering::Relaxed);
    }
    if peak_l > 1.0 || peak_r > 1.0 {
        shared.rt_to_ui.ui_clipped.store(true, Ordering::Relaxed);
    }
}
