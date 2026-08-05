// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use clack_plugin::{prelude::*, process::audio::PortPair};
use std::sync::atomic::{AtomicU32, Ordering};

type ChannelResult<'a> = (Option<&'a mut [f32]>, Option<&'a mut [f32]>);

#[inline(always)]
pub(super) fn extract_channels<'a>(
    port_pair: &mut PortPair<'a>,
    buf_host_l: &mut [f32],
    buf_host_r: &mut [f32],
    active_channel_count: &AtomicU32,
    process_mono: &mut bool,
    n_samples: usize,
) -> Result<Option<ChannelResult<'a>>, PluginError> {
    let Some(channel_pairs) = port_pair.channels()?.into_f32() else {
        return Ok(None);
    };

    let mut channel_iter = channel_pairs.into_iter();
    let pair_l = channel_iter.next();
    let pair_r = channel_iter.next();

    let channel_count = if pair_r.is_some() { 2 } else { 1 };
    if active_channel_count.load(Ordering::Relaxed) != channel_count {
        active_channel_count.store(channel_count, Ordering::Relaxed);
    }
    *process_mono = true;

    let mut out_l: Option<&mut [f32]> = None;
    let mut out_r: Option<&mut [f32]> = None;

    if let Some(pair) = pair_l {
        match pair {
            ChannelPair::InputOutput(i, o) => {
                buf_host_l[..n_samples].copy_from_slice(&i[..n_samples]);
                out_l = Some(o);
            }
            ChannelPair::InPlace(io) => {
                buf_host_l[..n_samples].copy_from_slice(&io[..n_samples]);
                out_l = Some(io);
            }
            ChannelPair::InputOnly(i) => {
                buf_host_l[..n_samples].copy_from_slice(&i[..n_samples]);
            }
            ChannelPair::OutputOnly(o) => {
                buf_host_l[..n_samples].fill(0.0);
                out_l = Some(o);
            }
        }
    } else {
        buf_host_l[..n_samples].fill(0.0);
    }

    #[cfg(feature = "stereo")]
    buf_host_r[..n_samples].copy_from_slice(&buf_host_l[..n_samples]);
    #[cfg(not(feature = "stereo"))]
    let _ = buf_host_r;

    if let Some(pair) = pair_r {
        match pair {
            ChannelPair::InputOutput(_, o) | ChannelPair::OutputOnly(o) => {
                out_r = Some(o);
            }
            ChannelPair::InPlace(io) => {
                out_r = Some(io);
            }
            ChannelPair::InputOnly(_) => {}
        }
    }

    Ok(Some((out_l, out_r)))
}
