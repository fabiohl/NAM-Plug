// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::{CabAssetKind, Path, build_cab_nam_slot_for_host, minimal_linear_json};
use clack_common::events::{Pckn, event_types::ParamValueEvent};
use clack_common::utils::ClapId;
use clack_host::prelude::{
    AudioPortBuffer, AudioPortBufferType, AudioPorts, EventBuffer, HostInfo, InputChannel,
    InputEvents, OutputEvents, PluginAudioConfiguration, PluginEntry, PluginInstance,
    StartedPluginAudioProcessor,
};
use std::sync::atomic::Ordering;

fn load_cab_and_activate_48k(
    cab_path: &Path,
) -> (
    PluginEntry,
    HostInfo,
    PluginInstance<crate::clap::host_harness::CompleteHost>,
    crate::clap::host_harness::CompleteHostState,
    StartedPluginAudioProcessor<crate::clap::host_harness::CompleteHost>,
) {
    let (entry, host_info, mut inst, state) =
        crate::clap::host_harness::make_test_plugin_with_harness();
    let cfg = PluginAudioConfiguration {
        sample_rate: 48_000.0,
        min_frames_count: 256,
        max_frames_count: 256,
    };
    let mt = unsafe { &*crate::clap::host_harness::extract_plugin_main_thread(&mut inst) };
    mt.load_cabsim(cab_path).expect("load cabsim");
    let stopped = inst
        .activate(
            |_, _| crate::clap::host_harness::make_harness_audio_processor(&state),
            cfg,
        )
        .expect("activate");
    let started = stopped.start_processing().expect("start_processing");
    (entry, host_info, inst, state, started)
}

fn run_cab_block(
    started: &mut StartedPluginAudioProcessor<crate::clap::host_harness::CompleteHost>,
    inp: &[f32],
    bypass_toggle: Option<bool>,
) -> Vec<f32> {
    let mut ev_buf = EventBuffer::new();
    if let Some(b) = bypass_toggle {
        ev_buf.push(&ParamValueEvent::new(
            0,
            ClapId::new(crate::clap::extensions::params::PARAM_CAB_BYPASS),
            Pckn::match_all(),
            if b { 1.0 } else { 0.0 },
        ));
    }
    let in_ev = InputEvents::from_buffer(&ev_buf);
    let (mut il, mut ir, mut ol, mut or) = (
        inp.to_vec(),
        inp.to_vec(),
        vec![0.0f32; inp.len()],
        vec![0.0f32; inp.len()],
    );
    let mut in_ports = AudioPorts::with_capacity(2, 1);
    let mut out_ports = AudioPorts::with_capacity(2, 1);
    let mut in_ch = [il.as_mut_slice(), ir.as_mut_slice()];
    let in_audio = in_ports.with_input_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_input_only(in_ch.iter_mut().map(InputChannel::constant)),
    }]);
    let out_ch = [ol.as_mut_slice(), or.as_mut_slice()];
    let mut out_audio = out_ports.with_output_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_output_only(out_ch.into_iter()),
    }]);
    let mut out_ev = OutputEvents::void();
    #[cfg(feature = "heap-audit")]
    let _g = neural_amp_modeler_rs::common::alloc_audit::TrackingGuard::new();
    started
        .process(&in_audio, &mut out_audio, &in_ev, &mut out_ev, None, None)
        .expect("process");
    #[cfg(feature = "heap-audit")]
    {
        use neural_amp_modeler_rs::common::alloc_audit::{
            get_alloc_count, get_dealloc_count, get_realloc_count,
        };
        assert_eq!(
            get_alloc_count() + get_dealloc_count() + get_realloc_count(),
            0,
            "RT cab callback allocated heap memory"
        );
    }
    ol
}

fn wait_off_rt_cab_nam_rebuild(
    inst: &mut PluginInstance<crate::clap::host_harness::CompleteHost>,
    shared: &crate::clap::plugin::GuiSharedState,
) {
    for _ in 0..1000 {
        inst.call_on_main_thread_callback();
        if !shared.cold.is_cab_nam_rebuild_pending() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("off-RT cab NAM rebuild timed out after 5 s");
}

#[test]
fn t4_4_cab_nam_reengage_and_reset_off_rt_parity_and_zero_rt_prewarm() {
    let lstm_cab = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/models/lstm.nam");
    assert!(lstm_cab.exists(), "missing lstm.nam fixture");
    let (_e_d, _h_d, mut dut_inst, _s_d, mut dut_proc) = load_cab_and_activate_48k(&lstm_cab);
    let shared = unsafe { &*crate::clap::host_harness::extract_plugin_shared(&mut dut_inst) };

    let sine: Vec<f32> = (0..512)
        .map(|i| 0.35 * (2.0 * std::f32::consts::PI * 220.0 * (i as f32) / 48_000.0).sin())
        .collect();
    let _ = run_cab_block(&mut dut_proc, &sine[..256], None);
    let _ = run_cab_block(&mut dut_proc, &sine[..256], Some(true));
    let _ = run_cab_block(&mut dut_proc, &sine[..256], None);
    let dry_hold = run_cab_block(&mut dut_proc, &sine[..256], Some(false));
    assert!(shared.cold.is_cab_nam_rebuild_pending());
    for (i, (&got, &exp)) in dry_hold.iter().zip(&sine[..256]).enumerate() {
        assert!((got - exp).abs() < 1e-5, "dry hold @{i}: {got} vs {exp}");
    }

    wait_off_rt_cab_nam_rebuild(&mut dut_inst, shared);
    let _ = run_cab_block(&mut dut_proc, &sine[..256], None);
    let blk5 = run_cab_block(&mut dut_proc, &sine[256..], None);

    let (_e_r, _h_r, mut ref_inst, _s_r, mut ref_proc) = load_cab_and_activate_48k(&lstm_cab);
    let _ = run_cab_block(&mut ref_proc, &sine[..256], None);
    let ref_blk5 = run_cab_block(&mut ref_proc, &sine[256..], None);
    let err = blk5
        .iter()
        .zip(&ref_blk5)
        .fold(0.0f32, |m, (a, b)| m.max((a - b).abs()));
    assert!(err < 1e-5, "post-reengage parity err={err}");
    ref_inst.deactivate(ref_proc.stop_processing());

    #[cfg(feature = "heap-audit")]
    {
        use neural_amp_modeler_rs::common::alloc_audit::{
            TrackingGuard, get_alloc_count, get_dealloc_count, get_realloc_count,
        };
        let _g = TrackingGuard::new();
        dut_proc.reset();
        assert_eq!(
            get_alloc_count() + get_dealloc_count() + get_realloc_count(),
            0,
            "reset() with active NamModel cab allocated heap memory"
        );
    }
    #[cfg(not(feature = "heap-audit"))]
    dut_proc.reset();

    assert!(shared.cold.is_cab_nam_rebuild_pending());
    let reset_hold = run_cab_block(&mut dut_proc, &sine[..256], None);
    for (i, (&got, &exp)) in reset_hold.iter().zip(&sine[..256]).enumerate() {
        assert!(
            (got - exp).abs() < 1e-5,
            "post-reset hold @{i}: {got} vs {exp}"
        );
    }
    wait_off_rt_cab_nam_rebuild(&mut dut_inst, shared);
    let _ = run_cab_block(&mut dut_proc, &sine[..256], None);
    let r2 = run_cab_block(&mut dut_proc, &sine[256..], None);
    let err_rst = r2
        .iter()
        .zip(&ref_blk5)
        .fold(0.0f32, |m, (a, b)| m.max((a - b).abs()));
    assert!(err_rst < 1e-5, "post-reset parity err={err_rst}");

    let raw = minimal_linear_json(48_000, Some("Cabinet"));
    let (stale_slot, _) = build_cab_nam_slot_for_host(
        CabAssetKind::NamModel,
        &raw,
        Path::new("stale.nam"),
        48_000,
        256,
    )
    .expect("build stale slot");
    let before = shared
        .cold
        .cab_nam_stale_discarded_total
        .load(Ordering::Relaxed);
    {
        let mt = unsafe { &*crate::clap::host_harness::extract_plugin_main_thread(&mut dut_inst) };
        if let Err(err) = mt.cmd_producer.borrow_mut().try_push_command(
            crate::clap::plugin::ClapParamPayload::RebuildCabNam {
                generation: 999_999,
                slot: stale_slot,
            },
        ) {
            panic!("push stale RebuildCabNam failed: {:?}", err.0);
        }
    }
    let post_stale = run_cab_block(&mut dut_proc, &sine[..256], None);
    assert_eq!(
        shared
            .cold
            .cab_nam_stale_discarded_total
            .load(Ordering::Relaxed),
        before + 1
    );
    assert!((post_stale[16] - 0.5 * sine[16]).abs() > 1e-4);
    dut_inst.call_on_main_thread_callback();
    dut_inst.deactivate(dut_proc.stop_processing());
}
