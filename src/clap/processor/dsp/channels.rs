// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use clack_plugin::{prelude::*, process::audio::PortPair};
use std::sync::atomic::{AtomicU32, Ordering};

type ChannelResult<'a> = (Option<&'a mut [f32]>, Option<&'a mut [f32]>);

/// Maps the host port channels into the contiguous working scratch buffers.
///
/// Channel semantics (PO: mono is the priority default; authentic stereo only
/// when the host track is stereo):
///
/// * **Mono port** (host provides a single input channel, or channel R has no
///   input at all): `buf_host_l` receives the mono source, `buf_host_r`
///   mirrors it so every downstream stereo-aware stage sees coherent buffers,
///   `process_mono` is forced `true` and the output carries the mono signal.
/// * **Stereo port** (host provides two distinct input channels): `buf_host_l`
///   receives channel 0 (L) and `buf_host_r` receives channel 1 (R) verbatim —
///   it is **forbidden** to duplicate L into R when a real R input exists.
///   `process_mono` starts `false`; the engine's mono detector
///   (`apply_input_stage`) reverts to the mono fast path as soon as L == R, so
///   a mono signal never pays the stereo processing cost.
/// * **Aliasing:** `InPlace` reads are copied into the scratch before any
///   output write, so read/write always target disjoint buffers.
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

    // A port is stereo-capable only when channel R actually carries an input.
    // `OutputOnly` R (mono-in/stereo-out) stays on the prioritized mono path.
    let has_r_input = matches!(
        pair_r,
        Some(ChannelPair::InputOutput(..) | ChannelPair::InPlace(_) | ChannelPair::InputOnly(_))
    );

    let channel_count = if pair_r.is_some() { 2 } else { 1 };
    if active_channel_count.load(Ordering::Relaxed) != channel_count {
        active_channel_count.store(channel_count, Ordering::Relaxed);
    }

    #[cfg(feature = "stereo")]
    {
        *process_mono = !has_r_input;
    }
    #[cfg(not(feature = "stereo"))]
    {
        let _ = has_r_input;
        *process_mono = true;
    }

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

    if let Some(pair) = pair_r {
        match pair {
            ChannelPair::InputOutput(i, o) => {
                buf_host_r[..n_samples].copy_from_slice(&i[..n_samples]);
                out_r = Some(o);
            }
            ChannelPair::InPlace(io) => {
                buf_host_r[..n_samples].copy_from_slice(&io[..n_samples]);
                out_r = Some(io);
            }
            ChannelPair::InputOnly(i) => {
                buf_host_r[..n_samples].copy_from_slice(&i[..n_samples]);
            }
            ChannelPair::OutputOnly(o) => {
                // No R input: mono broadcast, R mirrors the L source.
                buf_host_r[..n_samples].copy_from_slice(&buf_host_l[..n_samples]);
                out_r = Some(o);
            }
        }
    } else {
        #[cfg(feature = "stereo")]
        buf_host_r[..n_samples].copy_from_slice(&buf_host_l[..n_samples]);
        #[cfg(not(feature = "stereo"))]
        let _ = buf_host_r;
    }

    Ok(Some((out_l, out_r)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clack_host::prelude::*;

    const N: usize = 8;

    fn extract(
        ins: &mut [&mut [f32]],
        outs: &mut [&mut [f32]],
    ) -> (Vec<f32>, Vec<f32>, Option<usize>, Option<usize>, bool, u32) {
        let mut input_ports = AudioPorts::with_capacity(ins.len(), 1);
        let mut output_ports = AudioPorts::with_capacity(outs.len(), 1);
        let mut buf_host_l = vec![f32::NAN; N];
        let mut buf_host_r = vec![f32::NAN; N];
        let mut process_mono = false;
        let active_channel_count = AtomicU32::new(0);

        let (out_l_idx, out_r_idx) = {
            let input_buffers = input_ports.with_input_buffers([AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_input_only(
                    ins.iter_mut().map(InputChannel::constant),
                ),
            }]);
            let output_buffers = output_ports.with_output_buffers([AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_output_only(outs.iter_mut().map(|s| &mut **s)),
            }]);
            let frames_count = input_buffers.min_available_frames_with(&output_buffers);
            let raw_inputs = input_buffers.as_raw_buffers();
            let raw_outputs = output_buffers.into_raw_buffers();
            // SAFETY: `raw_inputs`/`raw_outputs` hold the `clap_audio_buffer`
            // structs whose channel pointers reference the caller-owned `ins`/
            // `outs` buffers (alive for the whole scope), and `frames_count` is
            // bounded by every channel length (validated by the buffers above).
            let mut audio = unsafe {
                clack_plugin::process::Audio::from_raw_buffers(
                    raw_inputs,
                    raw_outputs,
                    frames_count,
                )
            };
            let mut port_pair = audio.port_pair(0).unwrap();

            let result = extract_channels(
                &mut port_pair,
                &mut buf_host_l,
                &mut buf_host_r,
                &active_channel_count,
                &mut process_mono,
                N,
            )
            .expect("extract_channels failed");

            let (out_l, out_r) = result.unwrap();
            (
                out_l.map(|o| o.as_ptr() as usize),
                out_r.map(|o| o.as_ptr() as usize),
            )
        };

        (
            buf_host_l,
            buf_host_r,
            out_l_idx,
            out_r_idx,
            process_mono,
            active_channel_count.load(Ordering::Relaxed),
        )
    }

    #[test]
    fn stereo_input_output_reads_each_channel_independently() {
        let mut in_l = [0.1f32; N];
        let mut in_r = [0.2f32; N];
        let mut out_l = [0.0f32; N];
        let mut out_r = [0.0f32; N];
        let out_l_ptr = out_l.as_ptr() as usize;
        let out_r_ptr = out_r.as_ptr() as usize;
        let mut ins = [in_l.as_mut_slice(), in_r.as_mut_slice()];
        let mut outs = [out_l.as_mut_slice(), out_r.as_mut_slice()];

        let (buf_l, buf_r, ol, or, process_mono, count) = extract(&mut ins, &mut outs);

        assert_eq!(buf_l, vec![0.1; N]);
        assert_eq!(buf_r, vec![0.2; N]);
        assert_eq!(ol, Some(out_l_ptr));
        assert_eq!(or, Some(out_r_ptr));
        assert!(!process_mono, "stereo port must start in stereo mode");
        assert_eq!(count, 2);
    }

    #[test]
    fn mono_port_mirrors_l_into_r_and_stays_mono() {
        let mut in_l = [0.5f32; N];
        let mut out_l = [0.0f32; N];
        let out_l_ptr = out_l.as_ptr() as usize;
        let mut ins = [in_l.as_mut_slice()];
        let mut outs = [out_l.as_mut_slice()];

        let (buf_l, buf_r, ol, or, process_mono, count) = extract(&mut ins, &mut outs);

        assert_eq!(buf_l, vec![0.5; N]);
        assert_eq!(buf_r, vec![0.5; N]);
        assert_eq!(ol, Some(out_l_ptr));
        assert_eq!(or, None);
        assert!(process_mono, "mono port must stay on the mono path");
        assert_eq!(count, 1);
    }

    #[test]
    fn mono_in_stereo_out_output_only_r_mirrors_l() {
        let mut in_l = [0.3f32; N];
        let mut out_l = [0.0f32; N];
        let mut out_r = [0.0f32; N];
        let out_l_ptr = out_l.as_ptr() as usize;
        let out_r_ptr = out_r.as_ptr() as usize;
        let mut ins = [in_l.as_mut_slice()];
        let mut outs = [out_l.as_mut_slice(), out_r.as_mut_slice()];

        let (buf_l, buf_r, ol, or, process_mono, count) = extract(&mut ins, &mut outs);

        assert_eq!(buf_l, vec![0.3; N]);
        assert_eq!(buf_r, vec![0.3; N]);
        assert_eq!(ol, Some(out_l_ptr));
        assert_eq!(or, Some(out_r_ptr));
        assert!(process_mono, "no R input => mono broadcast to R output");
        assert_eq!(count, 2);
    }

    #[test]
    fn stereo_in_place_reads_before_writing_and_detects_aliasing() {
        let mut shared_l = [0.4f32; N];
        let mut shared_r = [0.7f32; N];
        let l_ptr = shared_l.as_ptr() as usize;
        let r_ptr = shared_r.as_ptr() as usize;
        let in_l = unsafe { std::slice::from_raw_parts_mut(shared_l.as_mut_ptr(), N) };
        let out_l = unsafe { std::slice::from_raw_parts_mut(shared_l.as_mut_ptr(), N) };
        let in_r = unsafe { std::slice::from_raw_parts_mut(shared_r.as_mut_ptr(), N) };
        let out_r = unsafe { std::slice::from_raw_parts_mut(shared_r.as_mut_ptr(), N) };
        let mut ins = [in_l, in_r];
        let mut outs = [out_l, out_r];

        let (buf_l, buf_r, ol, or, process_mono, count) = extract(&mut ins, &mut outs);

        assert_eq!(buf_l, vec![0.4; N]);
        assert_eq!(buf_r, vec![0.7; N]);
        assert_eq!(ol, Some(l_ptr));
        assert_eq!(or, Some(r_ptr));
        assert!(!process_mono);
        assert_eq!(count, 2);
    }

    #[test]
    fn stereo_input_only_r_reads_input_without_output() {
        let mut in_l = [0.1f32; N];
        let mut in_r = [0.9f32; N];
        let mut out_l = [0.0f32; N];
        let out_l_ptr = out_l.as_ptr() as usize;
        let mut ins = [in_l.as_mut_slice(), in_r.as_mut_slice()];
        let mut outs = [out_l.as_mut_slice()];

        let (buf_l, buf_r, ol, or, process_mono, count) = extract(&mut ins, &mut outs);

        assert_eq!(buf_l, vec![0.1; N]);
        assert_eq!(buf_r, vec![0.9; N]);
        assert_eq!(ol, Some(out_l_ptr));
        assert_eq!(or, None);
        assert!(!process_mono, "a real R input marks the port stereo");
        assert_eq!(count, 2);
    }

    #[test]
    fn stereo_output_only_uses_l_as_input() {
        let mut out_l = [0.0f32; N];
        let mut out_r = [0.0f32; N];
        let out_l_ptr = out_l.as_ptr() as usize;
        let out_r_ptr = out_r.as_ptr() as usize;
        let mut ins: [&mut [f32]; 0] = [];
        let mut outs = [out_l.as_mut_slice(), out_r.as_mut_slice()];

        let (buf_l, buf_r, ol, or, process_mono, count) = extract(&mut ins, &mut outs);

        assert_eq!(buf_l, vec![0.0; N]);
        assert_eq!(buf_r, vec![0.0; N]);
        assert_eq!(ol, Some(out_l_ptr));
        assert_eq!(or, Some(out_r_ptr));
        assert!(process_mono);
        assert_eq!(count, 2);
    }
}
