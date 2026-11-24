// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Cabinet stage processing and bypass crossfade blending for `audio_loop`.

use crate::clap::extensions::cabinet::CabSlotDsp;
use crate::clap::processor::dsp::dry_delay::DryDelayLine;
use crate::clap::processor::state::BypassCrossfader;
use neural_amp_modeler_rs::common::spsc::RtStatusFlags;

/// Disjoint single-cabinet slot state and pre-allocated scratch buffers for one sub-block.
pub(crate) struct CabinetStage<'a> {
    pub cab_slot: &'a mut Option<CabSlotDsp>,
    pub cab_bypass: bool,
    pub cab_needs_reset: &'a mut bool,
    pub cab_nam_dirty: &'a mut bool,
    pub cab_nam_generation: &'a mut u64,
    pub cab_nam_rebuild_in_flight: &'a mut bool,
    pub cab_nam_rebuild_req: &'a std::sync::atomic::AtomicU64,
    pub cab_bypass_xfade: &'a mut BypassCrossfader,
    pub cab_dry_delay: &'a mut DryDelayLine,
    pub buf_cab_dry_l: &'a mut [f32],
    pub buf_cab_dry_r: &'a mut [f32],
    pub buf_cab_nam_in_l: &'a mut [f32],
    pub buf_cab_nam_in_r: &'a mut [f32],
    pub buf_cab_zero_l: &'a [f32],
    pub buf_cab_zero_r: &'a [f32],
}

#[inline]
pub(super) fn process_cabinet_stage(
    cab: &mut CabinetStage<'_>,
    buf_out_l: &mut [f32],
    buf_out_r: &mut [f32],
    n_out: usize,
    process_mono: bool,
    rt_status: &RtStatusFlags,
) {
    if n_out == 0 {
        return;
    }
    let Some(ref mut slot) = *cab.cab_slot else {
        cab.cab_bypass_xfade.active = false;
        return;
    };
    cab.cab_dry_delay.process_block(
        &buf_out_l[..n_out],
        &buf_out_r[..n_out],
        &mut cab.buf_cab_dry_l[..n_out],
        &mut cab.buf_cab_dry_r[..n_out],
        n_out,
        process_mono,
    );
    if cab.cab_bypass && !cab.cab_bypass_xfade.active {
        if matches!(slot, CabSlotDsp::WavIr(_)) || *cab.cab_nam_dirty {
            *cab.cab_needs_reset = true;
        }
        buf_out_l[..n_out].copy_from_slice(&cab.buf_cab_dry_l[..n_out]);
        if !process_mono {
            buf_out_r[..n_out].copy_from_slice(&cab.buf_cab_dry_r[..n_out]);
        }
        return;
    }
    if *cab.cab_needs_reset && (!cab.cab_bypass || !cab.cab_bypass_xfade.target) {
        match slot {
            CabSlotDsp::WavIr(_) => {
                slot.reset_wav_ir();
                *cab.cab_needs_reset = false;
            }
            CabSlotDsp::NamModel(_) => {
                // Off-RT policy (D2): never invoke `prewarm_reset`/`prewarm_step`
                // on the audio thread. Keep outputting delayed dry until the
                // off-RT worker delivers a freshly prewarmed `RebuildCabNam` slot.
                *cab.cab_bypass_xfade = BypassCrossfader::new(true);
                if !*cab.cab_nam_rebuild_in_flight {
                    *cab.cab_nam_generation = cab.cab_nam_generation.wrapping_add(1);
                    cab.cab_nam_rebuild_req.store(
                        (*cab.cab_nam_generation << 1) | 1,
                        std::sync::atomic::Ordering::Release,
                    );
                    *cab.cab_nam_rebuild_in_flight = true;
                }
                buf_out_l[..n_out].copy_from_slice(&cab.buf_cab_dry_l[..n_out]);
                if !process_mono {
                    buf_out_r[..n_out].copy_from_slice(&cab.buf_cab_dry_r[..n_out]);
                }
                return;
            }
        }
    }
    match slot {
        CabSlotDsp::WavIr(pair) => {
            if !pair.l.is_passthrough() {
                if !process_mono {
                    pair.process_block_stereo(
                        &mut buf_out_l[..n_out],
                        &mut buf_out_r[..n_out],
                        Some(rt_status),
                    );
                    pair.rearm_tail();
                } else {
                    pair.l
                        .process_block(&mut buf_out_l[..n_out], Some(rt_status));
                    pair.l.rearm_tail();
                }
            }
        }
        CabSlotDsp::NamModel(nam) => {
            cab.buf_cab_nam_in_l[..n_out].copy_from_slice(&buf_out_l[..n_out]);
            if !process_mono {
                cab.buf_cab_nam_in_r[..n_out].copy_from_slice(&buf_out_r[..n_out]);
            }
            nam.process_block(
                &cab.buf_cab_nam_in_l[..n_out],
                &cab.buf_cab_nam_in_r[..n_out],
                &mut buf_out_l[..n_out],
                &mut buf_out_r[..n_out],
                n_out,
                process_mono,
            );
            nam.rearm_tail();
            *cab.cab_nam_dirty = true;
        }
    }
    if cab.cab_bypass_xfade.active {
        blend_cabinet_crossfade(
            cab.cab_bypass_xfade,
            &cab.buf_cab_dry_l[..n_out],
            &cab.buf_cab_dry_r[..n_out],
            &mut buf_out_l[..n_out],
            &mut buf_out_r[..n_out],
            n_out,
            process_mono,
        );
        if !cab.cab_bypass_xfade.active
            && cab.cab_bypass
            && (matches!(*cab.cab_slot, Some(CabSlotDsp::WavIr(_))) || *cab.cab_nam_dirty)
        {
            *cab.cab_needs_reset = true;
        }
    }
}

#[cold]
#[inline(never)]
pub(super) fn blend_cabinet_crossfade(
    crossfader: &mut BypassCrossfader,
    dry_l: &[f32],
    dry_r: &[f32],
    wet_l: &mut [f32],
    wet_r: &mut [f32],
    n_out: usize,
    process_mono: bool,
) {
    let n_xfade = n_out.min(crossfader.remaining);
    let step = crossfader.step;
    let mut mix = crossfader.mix;
    for i in 0..n_xfade {
        let d_l = dry_l[i];
        wet_l[i] = d_l + (wet_l[i] - d_l) * mix;
        if !process_mono {
            let d_r = dry_r[i];
            wet_r[i] = d_r + (wet_r[i] - d_r) * mix;
        }
        mix += step;
    }
    let final_mix = if crossfader.target { 0.0 } else { 1.0 };
    if (final_mix - 1.0f32).abs() > f32::EPSILON {
        for i in n_xfade..n_out {
            wet_l[i] = dry_l[i];
            if !process_mono {
                wet_r[i] = dry_r[i];
            }
        }
    }
    crossfader.mix = mix.clamp(0.0, 1.0);
    crossfader.remaining = crossfader.remaining.saturating_sub(n_xfade);
    if crossfader.remaining == 0 {
        crossfader.active = false;
        crossfader.mix = final_mix;
    }
}
