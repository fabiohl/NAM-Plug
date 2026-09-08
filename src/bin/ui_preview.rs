// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! # NAM-Plug UI Preview Host
//!
//! Dedicated, lightweight desktop harness for testing and refining the
//! Slint-based GUI of NAM-Plug without requiring a full DAW host or CLAP loader.
//!
//! Provides synthetic data mocking `GuiSharedState` and animates meters at 60 Hz
//! with IEC 60268-10 Type I ballistic physics (5ms attack, 300ms return, 1.5s clip hold).

slint::include_modules!();

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let start_time = std::time::Instant::now();
    println!("NAM-Plug: Launching Slint UI Preview Harness...");

    let window = MainWindow::new()?;

    // Populate Initial Synthetic Mock State
    window.set_model_name("Clean Tweed Deluxe (Neural)".into());
    window.set_model_arch("WaveNet (Standard)".into());
    window.set_model_sample_rate("48 kHz".into());
    window.set_model_loading(false);
    window.set_model_error(false);

    window.set_ir_name("1x12 Greenback (CabSim)".into());
    window.set_ir_status("48 kHz".into());
    window.set_has_ir(true);
    window.set_ir_loading(false);
    window.set_ir_error(false);

    window.set_input_gain_db(0.0);
    window.set_output_gain_db(-3.0);
    window.set_gate_thresh_db(-90.0);
    window.set_input_gain_text("0.0 dB".into());
    window.set_output_gain_text("-3.0 dB".into());
    window.set_gate_thresh_text("-90 dB".into());
    window.set_oversample_mode(0);
    window.set_activation_mode(0);
    window.set_bypass_active(false);
    window.set_is_stereo(true);
    window.set_peak_l(0.65);
    window.set_peak_r(0.60);
    window.set_peak_hold_l(0.75);
    window.set_peak_hold_r(0.70);
    window.set_gate_active(false);
    window.set_clip_active(false);
    window.set_sample_rate_text("48000 Hz".into());
    window.set_latency_text("0 spl (0.0 ms)".into());
    window.set_dsp_load_text("1.4% DSP".into());
    window.set_oversample_text("1x (Off)".into());
    window.set_simd_badge("AVX2+FMA".into());
    window.set_backend_text("Wayland Native".into());

    // Connect Callbacks for interactive knob manipulation
    {
        let handle = window.as_weak();
        window.on_input_gain_changed(move |val| {
            if let Some(w) = handle.upgrade() {
                let formatted = format!("{:+0.1} dB", val);
                w.set_input_gain_text(formatted.into());
                println!("Preview: Input Gain changed -> {:+0.1} dB", val);
            }
        });
    }

    {
        let handle = window.as_weak();
        window.on_output_gain_changed(move |val| {
            if let Some(w) = handle.upgrade() {
                let formatted = format!("{:+0.1} dB", val);
                w.set_output_gain_text(formatted.into());
                println!("Preview: Output Gain changed -> {:+0.1} dB", val);
            }
        });
    }

    {
        let handle = window.as_weak();
        window.on_gate_thresh_changed(move |val| {
            if let Some(w) = handle.upgrade() {
                let formatted = format!("{:.0} dB", val);
                w.set_gate_thresh_text(formatted.into());
                println!("Preview: Gate Threshold changed -> {:.0} dB", val);
            }
        });
    }

    {
        let handle = window.as_weak();
        window.on_toggle_bypass(move || {
            if let Some(w) = handle.upgrade() {
                let current = w.get_bypass_active();
                println!("Preview: Toggle bypass -> {}", current);
            }
        });
    }

    // Model loader: cycles between short name, ultra-long name, loading state, and error state
    {
        let handle = window.as_weak();
        let models = [
            (
                "Clean Tweed Deluxe (Neural)",
                "WaveNet (Standard)",
                "48 kHz",
            ),
            (
                "1965_Fender_Super_Reverb_Blackface_Vibrato_Channel_Bright_On_6L6_Matched_Pair_Ultra_High_Gain_Lead_Profile_v2_Final",
                "LSTM-16 (Extended)",
                "96 kHz",
            ),
            ("No model loaded", "", ""),
        ];
        let mut idx = 0;
        window.on_load_model_clicked(move || {
            if let Some(w) = handle.upgrade() {
                idx = (idx + 1) % models.len();
                let (name, arch, sr) = models[idx];
                w.set_model_name(name.into());
                w.set_model_arch(arch.into());
                w.set_model_sample_rate(sr.into());
                println!("Preview: Active Model changed -> '{}' ({})", name, arch);
            }
        });
    }

    // IR loader: cycles between standard IR, ultra-long IR name, and loaded status
    {
        let handle = window.as_weak();
        let irs = [
            ("1x12 Greenback (CabSim)", "48 kHz"),
            (
                "Celestion_Vintage_30_Closed_Back_4x12_Cab_Shure_SM57_CapEdge_Royer_R121_Cone_Distance_4in_Phase_Aligned_48k_24b.wav",
                "96 kHz",
            ),
            (
                "Custom_Mesa_Recto_Oversized_4x12_SM57_Grill_Center.wav",
                "44.1 kHz",
            ),
        ];
        let mut idx = 0;
        window.on_load_ir_clicked(move || {
            if let Some(w) = handle.upgrade() {
                idx = (idx + 1) % irs.len();
                let (name, sr) = irs[idx];
                w.set_has_ir(true);
                w.set_ir_name(name.into());
                w.set_ir_status(sr.into());
                println!("Preview: CabSim IR changed -> '{}' ({})", name, sr);
            }
        });
    }

    // Contextual IR clear action
    {
        let handle = window.as_weak();
        window.on_clear_ir_clicked(move || {
            if let Some(w) = handle.upgrade() {
                w.set_has_ir(false);
                w.set_ir_name("No IR loaded".into());
                w.set_ir_status("Bypassed".into());
                println!("Preview: CabSim IR cleared -> Bypassed");
            }
        });
    }

    // Oversampling segmented control
    {
        let handle = window.as_weak();
        let modes = ["1x (Off)", "2x (HQ)", "4x (Ultra)"];
        window.on_oversample_changed(move |mode| {
            if let Some(w) = handle.upgrade() {
                let idx = (mode as usize).min(modes.len() - 1);
                w.set_oversample_text(modes[idx].into());
                println!("Preview: Oversampling mode selected -> {}", modes[idx]);
            }
        });
    }

    // Activation precision segmented control
    {
        window.on_activation_changed(move |mode| {
            let name = if mode == 0 { "Standard" } else { "Fast" };
            println!("Preview: Activation precision changed -> {}", name);
        });
    }

    {
        let handle = window.as_weak();
        window.on_reset_clip_clicked(move || {
            if let Some(w) = handle.upgrade() {
                w.set_clip_active(false);
                println!("Preview: Clip indicator manually cleared");
            }
        });
    }

    {
        let handle = window.as_weak();
        let modes = ["1x (Off)", "2x (HQ)", "4x (Ultra)"];
        let mut idx = 0;
        window.on_cycle_oversample(move || {
            if let Some(w) = handle.upgrade() {
                idx = (idx + 1) % modes.len();
                w.set_oversample_mode(idx as i32);
                w.set_oversample_text(modes[idx].into());
                println!("Preview: Oversample cycled -> {}", modes[idx]);
            }
        });
    }

    // Dynamic 60 Hz telemetry animation simulation with IEC 60268-10 Type I Ballistics
    // 5ms integration attack, 300ms return decay, 1.5s clip hold
    let timer = slint::Timer::default();
    let handle = window.as_weak();
    let mut phase: f32 = 0.0;
    let mut curr_l: f32 = 0.65;
    let mut curr_r: f32 = 0.60;
    let mut hold_l: f32 = 0.75;
    let mut hold_r: f32 = 0.70;
    let mut clip_until = std::time::Instant::now();

    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(16),
        move || {
            if let Some(w) = handle.upgrade() {
                phase += 0.04;
                if phase > std::f32::consts::TAU {
                    phase -= std::f32::consts::TAU;
                }

                // Synthetic musical signal dynamics with peaks and occasional transients
                let raw_target_l = 0.52 + 0.35 * phase.sin() + 0.15 * (phase * 3.7).sin();
                let raw_target_r = 0.48 + 0.32 * (phase * 1.2).cos() + 0.12 * (phase * 4.1).cos();

                let target_l = raw_target_l.clamp(0.02, 1.0);
                let target_r = raw_target_r.clamp(0.02, 1.0);

                // IEC 60268-10 Ballistics:
                // Fast attack (~5ms, essentially instant on 16ms frame)
                // Smooth release (300ms return: alpha ~ 0.947 per 16ms frame)
                let decay = 0.947f32;

                if target_l > curr_l {
                    curr_l = target_l;
                } else {
                    curr_l = (curr_l * decay).max(target_l);
                }

                if target_r > curr_r {
                    curr_r = target_r;
                } else {
                    curr_r = (curr_r * decay).max(target_r);
                }

                // Peak Hold with slow decay (after hold period)
                if curr_l > hold_l {
                    hold_l = curr_l;
                } else {
                    hold_l = (hold_l * 0.992).max(curr_l);
                }

                if curr_r > hold_r {
                    hold_r = curr_r;
                } else {
                    hold_r = (hold_r * 0.992).max(curr_r);
                }

                // Clip Detection with 1.5s retention
                let now = std::time::Instant::now();
                if curr_l >= 0.98 || curr_r >= 0.98 {
                    clip_until = now + std::time::Duration::from_millis(1500);
                    w.set_clip_active(true);
                } else if now >= clip_until && w.get_clip_active() {
                    w.set_clip_active(false);
                }

                // Gate activity detection (active when signal is low)
                let gate_thresh = w.get_gate_thresh_db();
                let signal_db = 20.0 * curr_l.max(0.0001).log10();
                w.set_gate_active(signal_db < (gate_thresh + 60.0));

                w.set_peak_l(curr_l);
                w.set_peak_r(curr_r);
                w.set_peak_hold_l(hold_l);
                w.set_peak_hold_r(hold_r);
            }
        },
    );

    let elapsed = start_time.elapsed();
    println!("NAM-Plug: Preview Window ready in {:?}", elapsed);

    window.run()?;
    Ok(())
}
