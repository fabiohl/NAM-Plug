// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use crate::clap::test_util;
use clack_extensions::audio_ports::{AudioPortInfoBuffer, PluginAudioPorts};
use clack_extensions::audio_ports_activation::{PluginAudioPortsActivationSetImpl, SampleSize};
use clack_extensions::audio_ports_config::{
    AudioPortsConfigBuffer, PluginAudioPortsConfig, PluginAudioPortsConfigInfo,
};

#[test]
fn test_audio_ports_default_is_mono() {
    let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
    let handle = instance.plugin_handle();
    let ports_ext = handle
        .get_extension::<PluginAudioPorts>()
        .expect("PluginAudioPorts must be declared");

    // Only 1 main audio port is exposed for input and output.
    assert_eq!(ports_ext.count(&handle, true), 1);
    assert_eq!(ports_ext.count(&handle, false), 1);

    // Default configuration must be Mono (1 channel, MONO port type).
    let mut info_buf = AudioPortInfoBuffer::new();
    let port_in = ports_ext
        .get(&handle, 0, true, &mut info_buf)
        .expect("Input port 0 must exist");
    assert_eq!(port_in.id, ClapId::new(0));
    assert_eq!(port_in.name, b"Main Input");
    assert_eq!(port_in.channel_count, 1);
    assert_eq!(port_in.port_type, Some(AudioPortType::MONO));
    assert!(port_in.flags.contains(AudioPortFlags::IS_MAIN));
    assert_eq!(port_in.in_place_pair, Some(ClapId::new(0)));

    let mut info_buf_out = AudioPortInfoBuffer::new();
    let port_out = ports_ext
        .get(&handle, 0, false, &mut info_buf_out)
        .expect("Output port 0 must exist");
    assert_eq!(port_out.id, ClapId::new(0));
    assert_eq!(port_out.name, b"Main Output");
    assert_eq!(port_out.channel_count, 1);
    assert_eq!(port_out.port_type, Some(AudioPortType::MONO));
}

#[test]
fn test_audio_ports_config_declarations_and_switching() {
    let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
    let handle = instance.plugin_handle();
    let ports_ext = handle
        .get_extension::<PluginAudioPorts>()
        .expect("PluginAudioPorts must be declared");
    let config_ext = handle
        .get_extension::<PluginAudioPortsConfig>()
        .expect("PluginAudioPortsConfig must be declared");
    let info_ext = handle
        .get_extension::<PluginAudioPortsConfigInfo>()
        .expect("PluginAudioPortsConfigInfo must be declared");

    // Configurations declared: Mono (and Stereo only when dual-mono is enabled).
    #[cfg(feature = "dual-mono")]
    assert_eq!(config_ext.count(&handle), 2);
    #[cfg(not(feature = "dual-mono"))]
    assert_eq!(config_ext.count(&handle), 1);

    let mut cfg_buf = AudioPortsConfigBuffer::new();
    let cfg0 = config_ext
        .get(&handle, 0, &mut cfg_buf)
        .expect("Configuration 0 must exist");
    assert_eq!(cfg0.id, ClapId::new(CONFIG_ID_MONO));
    assert_eq!(cfg0.name, b"Mono");
    assert_eq!(cfg0.input_port_count, 1);
    assert_eq!(cfg0.output_port_count, 1);
    let main_in0 = cfg0.main_input.expect("Mono main_input");
    assert_eq!(main_in0.channel_count, 1);
    assert_eq!(main_in0.port_type, Some(AudioPortType::MONO));

    #[cfg(feature = "dual-mono")]
    {
        let cfg1 = config_ext
            .get(&handle, 1, &mut cfg_buf)
            .expect("Configuration 1 must exist");
        assert_eq!(cfg1.id, ClapId::new(CONFIG_ID_STEREO));
        assert_eq!(cfg1.name, b"Stereo");
        assert_eq!(cfg1.input_port_count, 1);
        assert_eq!(cfg1.output_port_count, 1);
        let main_in1 = cfg1.main_input.expect("Stereo main_input");
        assert_eq!(main_in1.channel_count, 2);
        assert_eq!(main_in1.port_type, Some(AudioPortType::STEREO));
    }

    // Initially active configuration is Mono.
    assert_eq!(
        info_ext.current_config(&handle),
        Some(ClapId::new(CONFIG_ID_MONO))
    );

    #[cfg(feature = "dual-mono")]
    {
        // Switch to Stereo configuration.
        config_ext
            .select(&handle, ClapId::new(CONFIG_ID_STEREO))
            .expect("Switch to Stereo must succeed while deactivated");

        assert_eq!(
            info_ext.current_config(&handle),
            Some(ClapId::new(CONFIG_ID_STEREO))
        );

        // After switching, PluginAudioPorts must report 2 channels and STEREO type.
        let mut info_buf = AudioPortInfoBuffer::new();
        let port_in_stereo = ports_ext
            .get(&handle, 0, true, &mut info_buf)
            .expect("Input port 0 in stereo mode");
        assert_eq!(port_in_stereo.channel_count, 2);
        assert_eq!(port_in_stereo.port_type, Some(AudioPortType::STEREO));

        // Switch back to Mono.
        config_ext
            .select(&handle, ClapId::new(CONFIG_ID_MONO))
            .expect("Switch back to Mono must succeed");

        assert_eq!(
            info_ext.current_config(&handle),
            Some(ClapId::new(CONFIG_ID_MONO))
        );
        let port_in_mono = ports_ext
            .get(&handle, 0, true, &mut info_buf)
            .expect("Input port 0 back in mono mode");
        assert_eq!(port_in_mono.channel_count, 1);
        assert_eq!(port_in_mono.port_type, Some(AudioPortType::MONO));
    }
    #[cfg(not(feature = "dual-mono"))]
    {
        assert!(
            config_ext
                .select(&handle, ClapId::new(CONFIG_ID_STEREO))
                .is_err()
        );
    }

    // Unknown configuration ID must be rejected.
    assert!(config_ext.select(&handle, ClapId::new(999)).is_err());
}

#[test]
fn test_audio_ports_config_rejected_while_active() {
    let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
    let shared = unsafe { &*test_util::extract_shared(&mut instance) };

    // Simulate active processing state
    shared.cold.is_processing.store(true, Ordering::Release);

    let handle = instance.plugin_handle();
    let config_ext = handle
        .get_extension::<PluginAudioPortsConfig>()
        .expect("PluginAudioPortsConfig must be declared");

    #[cfg(feature = "dual-mono")]
    {
        // Selecting configuration while active must fail.
        assert!(
            config_ext
                .select(&handle, ClapId::new(CONFIG_ID_STEREO))
                .is_err(),
            "Audio ports configuration change must be rejected while processing is active"
        );

        // Reset processing state and verify selection succeeds
        shared.cold.is_processing.store(false, Ordering::Release);
        assert!(
            config_ext
                .select(&handle, ClapId::new(CONFIG_ID_STEREO))
                .is_ok(),
            "Audio ports configuration change must succeed when deactivated"
        );
    }
    #[cfg(not(feature = "dual-mono"))]
    {
        assert!(
            config_ext
                .select(&handle, ClapId::new(CONFIG_ID_STEREO))
                .is_err()
        );
    }
}

#[test]
fn test_audio_ports_activation_and_f64_rejection() {
    let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
    let shared = unsafe { &*test_util::extract_shared(&mut instance) };

    // Initially both input and output are active.
    assert!(shared.cold.port_input_active.load(Ordering::Relaxed));
    assert!(shared.cold.port_output_active.load(Ordering::Relaxed));

    let raw_ptr = instance.plugin_handle().as_raw_ptr();
    let main_thread = unsafe {
        clack_plugin::extensions::wrapper::PluginWrapper::<crate::clap::NamClapPlugin>::handle(
            raw_ptr,
            |w| Ok(w.main_thread() as *const crate::clap::plugin::NamClapMainThread<'static>),
        )
        .expect("main thread pointer")
    };
    let mt = unsafe { &*main_thread };

    // 1. Deactivating port 0 input via Float32 succeeds and updates atomic.
    let ok = mt.set_active(true, 0, false, SampleSize::Float32);
    assert!(ok);
    assert!(!shared.cold.port_input_active.load(Ordering::Relaxed));

    // Re-activating input.
    let ok = mt.set_active(true, 0, true, SampleSize::Float32);
    assert!(ok);
    assert!(shared.cold.port_input_active.load(Ordering::Relaxed));

    // 2. Deactivating port 0 output via Unspecified succeeds and updates atomic.
    let ok = mt.set_active(false, 0, false, SampleSize::Unspecified);
    assert!(ok);
    assert!(!shared.cold.port_output_active.load(Ordering::Relaxed));

    // Re-activating output.
    let ok = mt.set_active(false, 0, true, SampleSize::Float32);
    assert!(ok);
    assert!(shared.cold.port_output_active.load(Ordering::Relaxed));

    // 3. Port index != 0 must be rejected (returns false).
    assert!(!mt.set_active(true, 1, true, SampleSize::Float32));
    assert!(!mt.set_active(false, 1, true, SampleSize::Float32));

    // 4. SampleSize::Float64 must be strictly rejected (returns false) per D-13.
    assert!(
        !mt.set_active(true, 0, true, SampleSize::Float64),
        "Float64 buffer sample size must be rejected"
    );
    assert!(
        !mt.set_active(false, 0, true, SampleSize::Float64),
        "Float64 buffer sample size must be rejected"
    );
}

#[test]
fn test_audio_ports_config_vs_two_mono_ports_protocol_comparison() {
    // Protocol demonstration (D-12 / E-10):
    // In CLAP, declaring two separate mono ports (Port 0 [1 ch] and Port 1 [1 ch]) causes DAWs
    // (such as Bitwig, Reaper) to classify Port 1 as an auxiliary/sidechain input or secondary bus,
    // rather than the Right channel of an authentic stereo pair.
    //
    // By providing audio-ports-config, the plugin explicitly allows host layout negotiation:
    // - Config 0 (Mono): 1 input port (1 channel), 1 output port (1 channel).
    //   Zero DSP waste, single model inference chain active.
    // - Config 1 (Stereo): 1 input port (2 channels), 1 output port (2 channels).
    //   True dual-mono inference with zero downmixing and zero crosstalk.
    let (_entry, _host_info, mut instance) = test_util::make_test_plugin();
    let handle = instance.plugin_handle();
    let ports_ext = handle
        .get_extension::<PluginAudioPorts>()
        .expect("PluginAudioPorts");
    let config_ext = handle
        .get_extension::<PluginAudioPortsConfig>()
        .expect("PluginAudioPortsConfig");

    // The plugin must never expose 2 independent ports to simulate stereo.
    assert_eq!(ports_ext.count(&handle, true), 1);
    assert_eq!(ports_ext.count(&handle, false), 1);

    // The configuration count is 2 with dual-mono, or 1 without.
    #[cfg(feature = "dual-mono")]
    assert_eq!(config_ext.count(&handle), 2);
    #[cfg(not(feature = "dual-mono"))]
    assert_eq!(config_ext.count(&handle), 1);
}
