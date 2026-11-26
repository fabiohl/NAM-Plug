// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use neural_amp_modeler_rs::dsp::adaptive::AdaptiveCompute;
use neural_amp_modeler_rs::models::StaticModel;

#[inline]
pub(crate) fn on_model_installed(
    model_l: Option<&StaticModel>,
    adaptive_compute: &mut AdaptiveCompute,
) {
    match model_l {
        Some(model) if matches!(model, StaticModel::WavenetDyn(_)) => {
            if let StaticModel::WavenetDyn(w) = model {
                adaptive_compute.set_wavenet_full_ch_rt(w.ch(), model.is_slimmable_capable());
            }
        }
        _ => adaptive_compute.clear_wavenet_registration_rt(),
    }
}
