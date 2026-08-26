// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

#[cfg(test)]
mod tests {
    use crate::clap::extensions::params::PARAM_OVERSAMPLE;
    use crate::clap::plugin::PendingRestartOs;
    use crate::clap::test_util::{self, TestHost};
    use clack_common::events::Pckn;
    use clack_common::events::event_types::ParamValueEvent;
    use clack_common::utils::{ClapId, Cookie};
    use clack_host::prelude::*;
    use neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_OS_REBUILD;
    use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
    use std::sync::atomic::Ordering;

    /// Oversample engine group delay contribution, host-rate samples:
    /// `OversampleEngine::latency_samples()` = HB_DELAY (12) per 2× half-band
    /// stage. Off = 0, X2 = 12, X4 = 24.
    const OS_LATENCY_2X: u32 = 12;
    const OS_LATENCY_4X: u32 = 24;

    fn oversample_event(value: f32) -> ParamValueEvent {
        ParamValueEvent::new(
            0u32,
            ClapId::new(PARAM_OVERSAMPLE),
            Pckn::match_all(),
            value as f64,
            Cookie::empty(),
        )
    }

    fn process_block_with_oversample(
        started: &mut StartedPluginAudioProcessor<TestHost>,
        value: f32,
    ) {
        let n = 256;
        let event = oversample_event(value);
        let mut event_buffer = EventBuffer::new();
        event_buffer.push(&event);
        let input_events = InputEvents::from_buffer(&event_buffer);
        let mut bufs = test_util::StereoTestBuffers::new(n, 0.0, 0.0);
        let mut input_channels = [bufs.in_l.as_mut_slice(), bufs.in_r.as_mut_slice()];
        let input_audio = bufs.input_ports.with_input_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(
                input_channels.iter_mut().map(InputChannel::constant),
            ),
        }]);
        let output_channels = [bufs.out_l.as_mut_slice(), bufs.out_r.as_mut_slice()];
        let mut output_audio = bufs.output_ports.with_output_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
        }]);
        let mut output_events = OutputEvents::from_buffer(&mut bufs.output_events_buffer);

        started
            .process(
                &input_audio,
                &mut output_audio,
                &input_events,
                &mut output_events,
                None,
                None,
            )
            .expect("process should succeed");
    }

    fn audio_config() -> PluginAudioConfiguration {
        PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 256,
            max_frames_count: 256,
        }
    }

    /// Verify that changing oversampling during active processing
    /// stores the pending restart factor and does NOT set RT_STATUS_NEEDS_OS_REBUILD.
    #[test]
    fn test_oversample_change_stores_pending_factor_when_active() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let stopped = plugin_instance
            .activate(|_, _| (), audio_config())
            .expect("Failed to activate");
        let mut started = stopped
            .start_processing()
            .expect("Failed to start processing");

        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

        // Verify plugin is active
        let buffer_size = shared.cold.buffer_size.load(Ordering::Relaxed);
        assert!(
            buffer_size > 0,
            "plugin should be active after start_processing"
        );

        // Verify no pending restart initially
        assert_eq!(
            PendingRestartOs::load(&shared.cold.pending_restart_os_factor, Ordering::Relaxed,),
            PendingRestartOs::None
        );

        // Process a block with an oversampling change (2x)
        process_block_with_oversample(&mut started, OversampleFactor::X2.to_f32());

        // After processing, the oversample change should have been detected
        // by process_dsp_audio -> set_oversample -> apply_oversample.
        // Since buffer_size > 0, it should store the pending factor
        // and NOT set RT_STATUS_NEEDS_OS_REBUILD.
        let pending =
            PendingRestartOs::load(&shared.cold.pending_restart_os_factor, Ordering::Relaxed);
        assert_eq!(
            pending,
            PendingRestartOs::Pending(OversampleFactor::X2),
            "pending restart factor should be stored for the requested oversampling"
        );

        let needs_rebuild = shared.cold.rt_status.check_flag(RT_STATUS_NEEDS_OS_REBUILD);
        assert!(
            !needs_rebuild,
            "RT_STATUS_NEEDS_OS_REBUILD should NOT be set for active oversample changes"
        );
    }

    /// Verify that `activate()` consumes the pending restart factor.
    #[test]
    fn test_activate_clears_pending_restart_factor() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

        // Set a pending restart factor as if the audio thread requested it
        PendingRestartOs::Pending(OversampleFactor::X2)
            .store(&shared.cold.pending_restart_os_factor, Ordering::Release);

        assert_ne!(
            PendingRestartOs::load(&shared.cold.pending_restart_os_factor, Ordering::Relaxed,),
            PendingRestartOs::None
        );

        let _stopped = plugin_instance
            .activate(|_, _| (), audio_config())
            .expect("Failed to activate");

        // After activate(), the pending factor should be consumed
        assert_eq!(
            PendingRestartOs::load(&shared.cold.pending_restart_os_factor, Ordering::Relaxed,),
            PendingRestartOs::None,
            "pending restart factor should be cleared after activate() consumes it"
        );
    }

    /// F-LAT-004/T3.1 acceptance matrix over the full host restart cycle:
    /// requested vs applied must be distinct while a restart is pending,
    /// `deactivate()` must persist the **applied** factor (never the eagerly
    /// updated requested one), and after the restart the engines must match
    /// the requested factor (reported latency == active filter latency).
    #[test]
    fn test_oversample_requested_vs_applied_across_restart_cycle() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

        let latency_off = {
            let stopped = plugin_instance
                .activate(|_, _| (), audio_config())
                .expect("Failed to activate");
            let started = stopped
                .start_processing()
                .expect("Failed to start processing");
            let latency = shared.rt_to_ui.current_latency.load(Ordering::Relaxed);
            let stopped = started.stop_processing();
            plugin_instance.deactivate(stopped);
            latency
        };

        let deactivated_factor = || {
            let guard = shared.cold.deactivated_dsp.lock().unwrap();
            guard.as_ref().expect("deactivated state missing").os_factor
        };

        // ── Off → X2 (restart pending while active) ─────────────────
        let stopped = plugin_instance
            .activate(|_, _| (), audio_config())
            .expect("Failed to activate");
        let mut started = stopped
            .start_processing()
            .expect("Failed to start processing");

        // Request X2 while active: the requested factor updates eagerly, the
        // applied engines (and reported latency) must NOT change yet.
        process_block_with_oversample(&mut started, OversampleFactor::X2.to_f32());
        assert_eq!(
            PendingRestartOs::load(&shared.cold.pending_restart_os_factor, Ordering::Relaxed,),
            PendingRestartOs::Pending(OversampleFactor::X2),
            "requested X2 must be recorded as pending"
        );
        assert_eq!(
            OversampleFactor::from_f32(
                shared.ui_to_rt.param_oversample.load(Ordering::Relaxed) as f32
            ),
            OversampleFactor::X2,
            "requested factor must be visible to the UI/state"
        );
        assert_eq!(
            shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
            latency_off,
            "latency must NOT change while the restart is pending (engines still Off)"
        );

        let stopped = started.stop_processing();
        plugin_instance.deactivate(stopped);

        // F-LAT-004 regression: deactivate() must persist the APPLIED factor
        // (Off engines), NOT the requested X2.
        assert_eq!(
            deactivated_factor(),
            OversampleFactor::Off,
            "deactivated engines run at Off — the persisted factor must be the APPLIED one"
        );

        // ── Host restart: activate consumes pending X2 ──────────────
        let stopped = plugin_instance
            .activate(|_, _| (), audio_config())
            .expect("Failed to activate");
        let mut started = stopped
            .start_processing()
            .expect("Failed to start processing");

        assert_eq!(
            PendingRestartOs::load(&shared.cold.pending_restart_os_factor, Ordering::Relaxed,),
            PendingRestartOs::None,
            "activate() must consume the pending restart"
        );
        assert_eq!(
            shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
            latency_off + OS_LATENCY_2X,
            "after restart the reported latency must include the X2 engine delay"
        );

        // ── X2 → X4 (restart pending while active) ──────────────────
        process_block_with_oversample(&mut started, OversampleFactor::X4.to_f32());
        assert_eq!(
            PendingRestartOs::load(&shared.cold.pending_restart_os_factor, Ordering::Relaxed,),
            PendingRestartOs::Pending(OversampleFactor::X4)
        );
        assert_eq!(
            shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
            latency_off + OS_LATENCY_2X,
            "latency must NOT change while X4 is pending (engines still X2)"
        );

        let stopped = started.stop_processing();
        plugin_instance.deactivate(stopped);

        assert_eq!(
            deactivated_factor(),
            OversampleFactor::X2,
            "deactivated engines run at X2 — the persisted factor must be the APPLIED one"
        );

        // ── Host restart: activate consumes pending X4 ──────────────
        let stopped = plugin_instance
            .activate(|_, _| (), audio_config())
            .expect("Failed to activate");
        let mut started = stopped
            .start_processing()
            .expect("Failed to start processing");

        assert_eq!(
            PendingRestartOs::load(&shared.cold.pending_restart_os_factor, Ordering::Relaxed,),
            PendingRestartOs::None
        );
        assert_eq!(
            shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
            latency_off + OS_LATENCY_4X,
            "after restart the reported latency must include the X4 engine delay"
        );

        // ── X4 → Off (restart pending while active; Off is representable) ──
        process_block_with_oversample(&mut started, OversampleFactor::Off.to_f32());
        assert_eq!(
            PendingRestartOs::load(&shared.cold.pending_restart_os_factor, Ordering::Relaxed,),
            PendingRestartOs::Pending(OversampleFactor::Off),
            "a pending transition to Off must be representable (T3.1/F-LAT-004)"
        );
        assert_eq!(
            shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
            latency_off + OS_LATENCY_4X,
            "latency must NOT change while Off is pending (engines still X4)"
        );

        let stopped = started.stop_processing();
        plugin_instance.deactivate(stopped);

        assert_eq!(
            deactivated_factor(),
            OversampleFactor::X4,
            "deactivated engines run at X4 — the persisted factor must be the APPLIED one"
        );

        // ── Host restart: activate consumes pending Off ─────────────
        let stopped = plugin_instance
            .activate(|_, _| (), audio_config())
            .expect("Failed to activate");
        let started = stopped
            .start_processing()
            .expect("Failed to start processing");

        assert_eq!(
            PendingRestartOs::load(&shared.cold.pending_restart_os_factor, Ordering::Relaxed,),
            PendingRestartOs::None
        );
        assert_eq!(
            shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
            latency_off,
            "after restart back to Off the reported latency must drop to baseline"
        );

        let stopped = started.stop_processing();
        plugin_instance.deactivate(stopped);

        assert_eq!(
            deactivated_factor(),
            OversampleFactor::Off,
            "final cycle: deactivated engines run at Off"
        );
    }

    /// T3.1/F-LAT-004 coalescence: multiple user clicks before the host
    /// responds to the restart must be latest-wins — only the final requested
    /// factor survives into the next `activate()`, and a round-trip
    /// Off→X4→Off resolves to unchanged Off engines (no spurious latency).
    #[test]
    fn test_oversample_coalescing_latest_wins_before_restart() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

        let stopped = plugin_instance
            .activate(|_, _| (), audio_config())
            .expect("Failed to activate");
        let mut started = stopped
            .start_processing()
            .expect("Failed to start processing");

        let latency_off = shared.rt_to_ui.current_latency.load(Ordering::Relaxed);

        // Burst of clicks before the host restarts: X4 then back to Off.
        process_block_with_oversample(&mut started, OversampleFactor::X4.to_f32());
        process_block_with_oversample(&mut started, OversampleFactor::Off.to_f32());

        // Latest requested factor wins — the pending state must be Off,
        // not X4, and no latency change occurred (engines untouched).
        assert_eq!(
            PendingRestartOs::load(&shared.cold.pending_restart_os_factor, Ordering::Relaxed,),
            PendingRestartOs::Pending(OversampleFactor::Off),
            "last requested factor must prevail while the restart is pending"
        );
        assert_eq!(
            shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
            latency_off,
            "no latency change before the restart lands"
        );

        let stopped = started.stop_processing();
        plugin_instance.deactivate(stopped);

        // The engines were Off all along — deactivate persists the applied Off.
        let guard = shared.cold.deactivated_dsp.lock().unwrap();
        assert_eq!(
            guard.as_ref().expect("deactivated state missing").os_factor,
            OversampleFactor::Off
        );
        drop(guard);

        // Host restart: activate consumes the pending Off — engines remain Off,
        // latency unchanged (no spurious X4 round-trip).
        let stopped = plugin_instance
            .activate(|_, _| (), audio_config())
            .expect("Failed to activate");
        let started = stopped
            .start_processing()
            .expect("Failed to start processing");

        assert_eq!(
            PendingRestartOs::load(&shared.cold.pending_restart_os_factor, Ordering::Relaxed,),
            PendingRestartOs::None,
            "activate() must consume the coalesced (Off) pending request"
        );
        assert_eq!(
            shared.rt_to_ui.current_latency.load(Ordering::Relaxed),
            latency_off,
            "coalesced Off→X4→Off must not change the applied latency"
        );

        let stopped = started.stop_processing();
        plugin_instance.deactivate(stopped);
    }
}
