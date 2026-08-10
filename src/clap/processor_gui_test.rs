// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

#[cfg(test)]
mod tests {
    use crate::clap::test_util::{self, StereoTestBuffers};
    use clack_host::prelude::*;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;

    #[test]
    fn test_gui_extension_x11() {
        use clack_extensions::gui::{GuiApiType, GuiConfiguration, GuiSize, PluginGui};

        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let gui_ext = plugin_instance
            .plugin_handle()
            .get_extension::<PluginGui>()
            .expect("PluginGui extension not found");

        let mut handle = plugin_instance.plugin_handle();

        // 1. Test is_api_supported
        assert!(gui_ext.is_api_supported(
            &mut handle,
            GuiConfiguration {
                api_type: GuiApiType::X11,
                is_floating: false,
            }
        ));
        assert!(!gui_ext.is_api_supported(
            &mut handle,
            GuiConfiguration {
                api_type: GuiApiType::WAYLAND,
                is_floating: false,
            }
        ));
        assert!(gui_ext.is_api_supported(
            &mut handle,
            GuiConfiguration {
                api_type: GuiApiType::X11,
                is_floating: true,
            }
        ));

        // 2. Test get_preferred_api
        let pref = gui_ext
            .get_preferred_api(&mut handle)
            .expect("Preferred API not found");
        assert_eq!(pref.api_type, GuiApiType::X11);
        assert!(!pref.is_floating);

        // 3. Test get_size
        let size = gui_ext.get_size(&mut handle).expect("Failed to get size");
        assert_eq!(size.width, 600);
        assert_eq!(size.height, 275);

        // 4. Test can_resize
        assert!(!gui_ext.can_resize(&mut handle));

        // 5. Test set_size
        assert!(
            gui_ext
                .set_size(
                    &mut handle,
                    GuiSize {
                        width: 600,
                        height: 275
                    }
                )
                .is_ok()
        );
        // Note: clack-extensions 0.1.0's FFI wrapper for set_size contains a bug where it calls
        // .is_some() on the Option<Result<(), PluginError>>, returning true (Ok) to the host even
        // when the plugin returns an Err. Thus we cannot assert gui_ext.set_size returns Err
        // from the host-side wrapper here.
    }

    #[test]
    fn test_ui_telemetry() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let audio_config = PluginAudioConfiguration {
            sample_rate: 48000.0,
            min_frames_count: 512,
            max_frames_count: 512,
        };

        let model_dir = crate::clap::test_util::model_path("lstm.nam");

        let params = test_util::make_default_params(Some(model_dir));
        test_util::load_plugin_state(&mut plugin_instance, &params);

        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

        // Check model name basename was updated
        {
            let name_guard = shared.cold.ui_model_name.lock().unwrap();
            assert_eq!(*name_guard, "lstm.nam");
        }

        let stopped_processor = plugin_instance.activate(|_, _| (), audio_config).unwrap();
        let mut started_processor = stopped_processor.start_processing().unwrap();

        shared
            .rt_to_ui
            .ui_peak_l
            .store(0.0f32.to_bits(), Ordering::Relaxed);
        shared
            .rt_to_ui
            .ui_peak_r
            .store(0.0f32.to_bits(), Ordering::Relaxed);
        shared.rt_to_ui.ui_clipped.store(false, Ordering::Relaxed);

        let n = 512;
        let mut bufs = StereoTestBuffers::new(n, 1.5, 0.5);

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

        let input_events = InputEvents::empty();
        let mut output_events = OutputEvents::from_buffer(&mut bufs.output_events_buffer);

        started_processor
            .process(
                &input_audio,
                &mut output_audio,
                &input_events,
                &mut output_events,
                None,
                None,
            )
            .unwrap();

        // Verify peaks are greater than 0.0
        let peak_l = f32::from_bits(shared.rt_to_ui.ui_peak_l.load(Ordering::Relaxed));
        let peak_r = f32::from_bits(shared.rt_to_ui.ui_peak_r.load(Ordering::Relaxed));
        assert!(peak_l > 0.0, "peak_l was: {}", peak_l);
        assert!(peak_r > 0.0, "peak_r was: {}", peak_r);

        // Verify that clipping occurred because input left was 1.5
        let clipped = shared.rt_to_ui.ui_clipped.load(Ordering::Relaxed);
        assert!(
            clipped,
            "ui_clipped should be true since input left was 1.5"
        );
    }

    #[test]
    fn test_gui_load_error_null_byte_safety() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

        let path_with_null = PathBuf::from("invalid_model\0name.nam");
        if let Ok(mut pending_guard) = shared.cold.ui_pending_model.lock() {
            *pending_guard = Some(path_with_null);
        }
        shared.cold.ui_loading.store(true, Ordering::Relaxed);

        plugin_instance.call_on_main_thread_callback();

        assert!(!shared.cold.ui_loading.load(Ordering::Relaxed));
        assert!(shared.cold.ui_load_error.load(Ordering::Relaxed));
    }

    #[test]
    fn test_c_string_null_byte_sanitization() {
        use std::ffi::CString;
        let err_msg = "Model load failed: file\0name.nam not found";
        let err_str = format!("NAM-rs: Failed to load model from GUI: {}", err_msg);
        let sanitized_err = err_str.replace('\0', " ");
        let msg = CString::new(sanitized_err);
        assert!(msg.is_ok(), "Sanitized CString creation should succeed");
    }

    /// Loading a .nam file that fails to build should:
    /// 1. Show an error to the user (ui_load_error = true)
    /// 2. NOT swap the active model (model_load_counter unchanged)
    #[test]
    fn test_gui_load_model_build_failed() {
        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();
        let shared = unsafe { &*test_util::extract_shared(&mut plugin_instance) };

        let invalid_path = std::env::temp_dir().join("nam_plug_gui_invalid.nam");
        test_util::write_invalid_model_fixture(&invalid_path);

        if let Ok(mut pending_guard) = shared.cold.ui_pending_model.lock() {
            *pending_guard = Some(invalid_path);
        }
        shared.cold.ui_loading.store(true, Ordering::Relaxed);

        plugin_instance.call_on_main_thread_callback();

        assert!(!shared.cold.ui_loading.load(Ordering::Relaxed));
        assert!(
            shared.cold.ui_load_error.load(Ordering::Relaxed),
            "Expected ui_load_error to be set when model build fails"
        );
        assert_eq!(
            shared.cold.model_load_counter.load(Ordering::Relaxed),
            0,
            "model_load_counter should not increment for failed model build"
        );
    }

    // ══════════════════════════════════════════════════════════════════════
    // S8-E8-T05: Headless GUI lifecycle + clipboard tests
    // ══════════════════════════════════════════════════════════════════════

    /// Verifies the full floating window lifecycle in a headless X11
    /// environment (Xvfb): create → set_transient → destroy.
    ///
    /// Requires `DISPLAY` to be set to a running X11 display server
    /// (e.g., `Xvfb :99 -screen 0 1024x768x24 &` and `export DISPLAY=:99`).
    ///
    /// The floating window path (`is_floating: true`) is used because it does
    /// not require a parent window handle from the host — it creates its own
    /// top-level window via `baseview::Window::open_blocking` on a background
    /// thread.
    ///
    /// Note: `show()` and `hide()` are NOT called because the floating
    /// window is immediately visible after `set_transient()`; the GUI
    /// lifecycle FSM's `WindowReady` transition is not triggered in this
    /// code path (it's a documented limitation — the FSM awaits a GUI-thread
    /// callback that does not exist in the current implementation).
    /// DAWs use `destroy()` for window teardown, which is what we test here.
    #[test]
    #[ignore = "requires dedicated X11 display server and mutates process environment"]
    fn test_headless_gui_floating_window_lifecycle() {
        assert!(
            std::env::var("DISPLAY").is_ok(),
            "DISPLAY environment variable not set. \
             Start Xvfb first: Xvfb :99 -screen 0 1024x768x24 &; export DISPLAY=:99"
        );

        // SAFETY: Setting environment variables in a single-threaded test is safe.
        unsafe {
            std::env::set_var("LIBGL_ALWAYS_SOFTWARE", "1");
            std::env::set_var("GALLIUM_DRIVER", "llvmpipe");
        }

        use clack_extensions::gui::{GuiApiType, GuiConfiguration, PluginGui, Window};

        let (_entry, _host_info, mut plugin_instance) = test_util::make_test_plugin();

        let gui_ext = plugin_instance
            .plugin_handle()
            .get_extension::<PluginGui>()
            .expect("PluginGui extension not found");

        let mut handle = plugin_instance.plugin_handle();

        let float_config = GuiConfiguration {
            api_type: GuiApiType::X11,
            is_floating: true,
        };
        assert!(
            gui_ext.is_api_supported(&mut handle, float_config),
            "X11 floating must be supported"
        );

        // 1. Create GUI resources
        assert!(
            gui_ext.create(&mut handle, float_config).is_ok(),
            "GUI create() must succeed"
        );

        // 2. Open floating window — spawns background thread with
        //    baseview::Window::open_blocking, waits for initialization
        //    (up to 2 seconds). The `_window` parameter is unused in
        //    the plugin's set_transient implementation (baseview lacks
        //    transient window support), so we pass a dummy X11 window.
        // SAFETY: The dummy window handle is not dereferenced by the plugin.
        let result = unsafe {
            gui_ext.set_transient(
                &mut handle,
                Window::from_generic_ptr(GuiApiType::X11, std::ptr::null_mut()),
            )
        };
        assert!(
            result.is_ok(),
            "set_transient() must succeed — floating window creation; got: {result:?}"
        );

        // Window is now open and visible on a background thread.
        // Let it render a frame.
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Destroy — tears down resources, joins window thread via reaper
        gui_ext.destroy(&mut handle);

        eprintln!("  ✓ GUI floating window lifecycle: create → set_transient → destroy");
    }

    /// Verifies that `arboard` clipboard operations succeed under Xvfb.
    ///
    /// Tests `Clipboard::new()` (X11 connection via `$DISPLAY`) and
    /// `set_text()` (atom-level text storage).  This ensures the clipboard
    /// integration — used by the status-bar diagnostic-copy button — is
    /// compatible with headless CI environments.
    #[test]
    #[ignore = "requires dedicated X11 display server connection"]
    fn test_headless_clipboard_works() {
        assert!(
            std::env::var("DISPLAY").is_ok(),
            "DISPLAY environment variable not set. \
             Start Xvfb first: Xvfb :99 -screen 0 1024x768x24 &; export DISPLAY=:99"
        );

        use arboard::Clipboard;

        let mut clipboard =
            Clipboard::new().expect("arboard::Clipboard::new() must succeed with X11 display");

        let test_text = "nam-rs diagnostic test — headless clipboard";
        clipboard
            .set_text(test_text)
            .expect("set_text() must succeed");

        // Verify we can re-open and get the text we just set
        let mut verify = Clipboard::new().expect("Re-opening clipboard must succeed");
        let retrieved = verify.get_text().expect("get_text() must succeed");
        assert_eq!(
            retrieved, test_text,
            "Clipboard get_text must match set_text"
        );

        // Cleanup — clear clipboard
        clipboard
            .set_text("")
            .expect("Clear clipboard must succeed");

        eprintln!("  ✓ Clipboard: X11 connection + set/get round-trip succeeded");
    }
}
