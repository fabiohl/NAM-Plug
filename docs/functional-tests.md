<!--
SPDX-License-Identifier: GPL-3.0-or-later
Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
-->

# Functional Testing Guide (Human QA) - NAM-Plug (CLAP Plugin)

**Audience:** Developers, QA Testers, and End Users.
**Target DAWs:** Bitwig Studio 6+ and Reaper / Fender Studio Pro 8+ (Linux, Flatpak / Native).
**Preparation:** `~/.clap/nam_plug.clap` installed (Release build via `utils/build-release.sh` or `target/release/libnam_plug.so`), ≥1 valid `.nam` model, 1 invalid file, 1 `.wav` IR file, Guitar DI or audio signal generator on track, and `pw-top` open to monitor XRUNs (Recommended: 128 samples @ 48 kHz).

> [!NOTE]
> This guide covers manual human QA workflows. For automated unit and integration tests (`cargo test`), property testing, allocation auditing, and Criterion performance benchmarks (`cargo bench`), refer to [`testing.md`](testing.md).

---

## Executive Summary & Testing Strategy

To ensure comprehensive testing without unnecessary friction, this guide is organized into **3 progressive tiers**:

| Tier       | Name                        | Target Duration | When to Run                            | Scope / Objective                                                     |
|:---------- |:--------------------------- |:--------------- |:-------------------------------------- |:--------------------------------------------------------------------- |
| **Tier 1** | ⚡ **2-Minute Smoke Test**  | ~2 min          | After every code commit or quick build | Catch 80% of critical regressions in 5 fast actions                   |
| **Tier 2** | 🎯 **Feature & Host Sweep** | ~10–15 min      | PR review / Feature branch completion  | Full functional verification across Audio, Controls, Host & Telemetry |
| **Tier 3** | 🛡️ **Stress & Release Gate**| ~20–30 min      | Pre-release / Nightly audit            | Heavy multithreaded stress, HiDPI, GL cleanup, offline determinism    |

---

## Tier 1: ⚡ 2-Minute Smoke Test (5 High-Yield Actions)

*Objective: Run these 5 actions immediately after building to verify core plugin stability.*

- [ ] **T1.1 Load Model & CabSim IR:** Click `[LOAD .NAM]` in the **AMP MODEL** card → select a valid `.nam` model → Click `[LOAD IR]` in the **CABINET IR** card → select a valid `.wav` IR.
  *Expected:* Both load asynchronously without host UI freeze or audio dropouts. Amp modeling and IR convolution become audible immediately. Active model name and IR filename update in Zone 1.
- [ ] **T1.2 Knobs & Double-Click Reset:** Drag **INPUT** knob to +6.0 dB, drag **GATE** to −50.0 dB → Double-click **INPUT**.
  *Expected:* Knobs move smoothly without zipper noise. Double-click instantly resets **INPUT** to default 0.0 dB (GATE remains at −50.0 dB). Double-clicking **GATE** resets it to default −90.0 dB (off by default); double-clicking **OUTPUT** resets to default −3.0 dB.
- [ ] **T1.3 Bypass & VU Meter Layout:** Insert plugin in DAW configured as Mono (Config 0, default: 1 centered meter bar), then instantiate on a Stereo track via `audio-ports-config` (Config 1: 2 meter bars L/R) → Toggle **BYPASS** (toggle switch).
  *Expected:* Meter layout matches the negotiated port configuration (1 centered bar for Mono vs. 2 bars L/R for Stereo; configuration negotiated strictly via CLAP `audio-ports-config`). The toggle switch transitions cleanly between `DSP ACTIVE` and `BYPASS` via a 64-sample click-free linear crossfade, yielding a bit-transparent dry signal without audible clicks.
- [ ] **T1.4 Host Parameter Automation (`[Bitwig]`):** Draw a quick automation ramp for `input_gain_db` or `output_gain_db` in DAW → start playback.
  *Expected:* Host automation smoothly drives parameter values and audio volume without zipper noise or audio dropouts.
- [ ] **T1.5 Real-Time Telemetry Bar Verification:** Observe the status bar (Zone 5) during playback.
  *Expected:* Status bar displays real-time telemetry readouts: host sample rate (`SR: {sr}`), buffer quantum (`BUF: {buf} spl`), latency (`LAT: {lat} spl ({ms} ms)`), channel configuration (`CH: Mono` or `Stereo`), DSP CPU load percentage (`{pct}% DSP` with overload/xrun alert LED), contextual alert pill (when active), and active window backend (`backend_text`, e.g. `X11 (Embedded)`).

---

## Tier 2: 🎯 Feature & Host Integration Sweep (~10–15 min)

---

### Domain 2A: Audio & DSP Engine

- [ ] **2A.1 SPSC Non-Blocking Load:** Load a different `.nam` model while playing an audio track.
  *Expected:* DAW UI remains 100% responsive. Audio engine swaps models seamlessly without stopping playback or causing XRUNs in `pw-top`. Dropped model allocations are safely collected off-RT via SPSC GC.
- [ ] **2A.2 Invalid File Robustness:** Select a corrupted or 0-byte `.nam` / `.wav` file in the file picker.
  *Expected:* Red error state (`"Load failed"` / `"IR load failed"`) displays on the respective card in Zone 1. The previous valid model/IR remains active; plugin never crashes or silences audio.
- [ ] **2A.3 CabSim IR Management:** With an IR loaded, click the `[✕]` clear button on the CABINET IR card.
  *Expected:* IR display reverts to `"No IR loaded"`, audio transitions cleanly back to post-model output without cab simulation, and the `[✕]` clear button disappears.
- [ ] **2A.4 Oversampling & Activation Engine Modes:** Switch Oversampling from `"Off"` to `"2×"` to `"4×"`, and Activation Precision from `"Standard"` to `"Fast"`.
  *Expected:* Resampler adjusts seamlessly; reported latency in status bar updates dynamically to reflect filter delay (`0 spl` @ Off, `32 spl` @ 2×, `96 spl` @ 4×); zero audio dropouts.
- [ ] **2A.5 Slimmable A2 Container & Adaptive Compute:** Load a slimmable container model → change host parameter `"Slim Override"` from `"Auto"` to `"Force Lite"` to `"Force Full"` (or adjust `"Adaptive Compute"` mode).
  *Expected:* Submodel swaps cleanly without audible clicks. Under high CPU load in `"Auto"`, telemetry flags reflect adaptive degradation safely.
- [ ] **2A.6 Multi-Instance Isolation:** Insert 3 NAM-Plug instances across different tracks → load different models & IRs on each.
  *Expected:* All 3 instances process audio independently with zero cross-talk, state leakage, or audio dropouts.

---

### Domain 2B: UI Controls & Dynamic Feedback

- [ ] **2B.1 Knob Tooltips & Fine-Tune:** Drag knobs or hover and scroll with mouse wheel → hold **Shift** while dragging/scrolling.
  *Expected:* Numerical text in knob center updates smoothly (e.g. `"+3.5 dB"`). Holding **Shift** scales adjustments by 0.1× for precision fine-tuning.
- [ ] **2B.2 Interactive Knob Glow & Double-Click:** Drag any knob dial, then double-click it.
  *Expected:* The active value arc tracks the cursor. Double-clicking instantly returns the parameter to its defined default (`0.0 dB` for INPUT, `-3.0 dB` for OUTPUT, `-90.0 dB` for GATE).
- [ ] **2B.3 Discrete Selectors (Oversampling & Precision):** Click `"Off"`, `"2×"`, `"4×"` under Oversample, and `"Std"`, `"Fast"` under Precision.
  *Expected:* Active option highlights crisply with distinct background color. Changes dispatch discrete CLAP parameter events to host and update DSP state instantly without audio glitches.
- [ ] **2B.4 VU Peak Hold & Clipping:** Feed a high-gain signal to induce digital clipping (>0 dBFS) → stop audio → click the red clipped indicator bar.
  *Expected:* Peak hold indicator pauses ~2.0s before decaying smoothly. Red clip indicator at the top persists until clicked to reset.
- [ ] **2B.5 VU Meter L/R Channel Independence (`[Stereo Track]`):** On a stereo track (Config 1), test 4 signal scenarios:
  - (a) Signal **only on L** (hard-pan L) → only the L bar moves; R remains at minimum.
  - (b) Signal **only on R** → only the R bar moves; L remains at minimum.
  - (c) **Symmetric signal** (equal L and R) → both bars move to equal levels.
  - (d) **Asymmetric signal** (different levels) → bars move independently.
  *Expected:* L and R channels operate independently with zero channel crosstalk.
- [ ] **2B.6 Noise Gate LED Indicator:** Feed a low-level signal below the set gate threshold → then play above threshold.
  *Expected:* LED displays `"GATE ACTIVE"` in green when closed (silencing background noise) and transitions to `"GATE OPEN"` when audio passes through.

---

### Domain 2C: Host Integration & State Persistence

- [ ] **2C.1 Host Track Info & Param Indication Extensions:** Verify plugin instantiation on colored tracks or with host MIDI mapping in Bitwig Studio.
  *Expected:* Plugin queries host track info (`clap_plugin_track_info`) and parameter indication (`clap_plugin_param_indication`) without errors or crashes. Shared state records track color and mapping metadata safely off the audio thread.
- [ ] **2C.2 Automation Tracking & Playback:** Play back active automation on `output_gain_db` or `input_gain_db` in DAW while monitoring audio.
  *Expected:* Audio gain ramps smoothly without zipper noise or audio dropouts, and UI display synchronizes with current parameter values.
- [ ] **2C.4 Full Project State Reload:** Set custom gain, gate, oversampling, activation, model, and IR → save DAW project → close DAW → reopen project.
  *Expected:* All parameters, model path, and IR path restore perfectly with identical audio output. Missing model files degrade cleanly to `"No model loaded"`.
- [ ] **2C.5 Preset Discovery Browser:** Place `.nam`/`.namb` models in `~/.nam/models` → open the host's preset browser / FX browser for NAM-Plug → inspect entries → load one.
  *Expected:* Under standard file-location hosts (e.g., Bitwig Studio, `CLAP_PRESET_DISCOVERY_LOCATION_FILE`) and plugin-location hosts (e.g., Fender Studio Pro 8+, `CLAP_PRESET_DISCOVERY_LOCATION_PLUGIN`), entries display model name, creator, and gear metadata; selecting a preset loads the model cleanly without XRUNs, and reopening the host or rescanning plugins detects newly added files in `~/.nam/models`.
- [ ] **2C.6 Preset Portability (`clap.state-context`):** Move `.nam` model file to a different directory → load saved DAW device preset.
  *Expected:* Plugin locates model via fallback search paths or basename and restores full state.
- [ ] **2C.7 GUI Window Negotiation (Embedded & Floating):**
  - **Embedded mode (default):** GUI renders inside the host plugin window as an X11 child (via XEmbed; Wayland embedded is rejected per CLAP specification) without orphan windows or memory growth.
  - **Floating mode:** GUI opens as a clean standalone window when requested by the host (`is_floating = true`; verified on X11; experimental on Wayland due to lack of a test host negotiating floating).
  - **Honest fallback:** An unsupported embedded request (such as Wayland embedded) fails gracefully at `create()` rather than silently opening an orphan floating window; conforming hosts either negotiate a floating window or display their generic parameter editor.
  - **Reopen cycles:** Closing and reopening the GUI window 5+ times on the same instance re-embeds cleanly with zero crashes or platform re-initialization errors.
- [ ] **2C.8 Bitwig Device Panel Pages (`[Bitwig]`):** Open Bitwig Device Panel → navigate between pages.
  *Expected:* 2 pages present: **"Main"** (INPUT, OUTPUT, BYPASS) and **"Gate"** (GATE). Moving a slider in the Device Panel updates the corresponding knob in the GUI, and vice versa.
- [ ] **2C.9 Category & Search Indexing:** Search host plugin browser for `"distortion"`, `"gate"`, or `"mono"`.
  *Expected:* NAM-Plug correctly indexed under **Audio FX → Distortion**.

---

### Domain 2D: Telemetry, Diagnostics & Dynamic Latency

- [ ] **2D.1 Status Bar Cadence & Metrics:** Observe the footer status bar for ≥20s during steady playback.
  *Expected:* Telemetry readouts (`SR`, `BUF`, `LAT`, `CH`, `{pct}% DSP`, backend) update smoothly. The polling cadence operates at 60 Hz under active interaction and relaxes to 30 Hz when signal is silent and the UI is untouched.
- [ ] **2D.2 Real-Time DSP Load & Overload Detection:** Monitor the `{pct}% DSP` readout during high processing loads (e.g., 4× oversampling on high sample rates).
  *Expected:* DSP load percentage tracks audio computation time accurately. If an audio buffer overload occurs, the red indicator lights up with a 2-second hold, and hovering displays an actionable recommendation tooltip (`"Reduza oversampling ou habilite computação adaptativa"`).
- [ ] **2D.3 Contextual Alert Banner:** Induce a runtime condition (e.g. non-finite audio or file load issue).
  *Expected:* An alert pill appears in Zone 5 displaying the diagnostic alert text and status indicator for ~3 seconds before fading cleanly.
- [ ] **2D.4 Dynamic Sample Rate & PDC Recalculation:** With NAM-Plug active and audio playing, change the project sample rate (e.g., 44.1 kHz → 96 kHz).
  *Expected:* Status bar updates to the new sample rate. Reported latency updates immediately. Host recalculates Plugin Delay Compensation (PDC) without audio desync or XRUNs.
- [ ] **2D.5 Bypass & Resampling PDC Update:** Toggle bypass or switch oversampling (e.g. Off → 4×) while audio is playing.
  *Expected:* Reported latency in status bar updates to reflect the new resampling state. Host PDC updates immediately with zero audio dropouts.

---

## Tier 3: 🛡️ Stress & Release Gate (Pre-Release Audit)

---

### 3.1 Spam & Rapid Load Stress

- [ ] **3.1.1 GUI Open/Close Spam:** Rapidly open and close GUI window 20+ times in <30s during active playback.
  *Expected:* Zero audio glitches, zero XRUNs in `pw-top`, no lingering window threads.
- [ ] **3.1.2 Model & IR Reload Flood:** Load 10 different `.nam` models and 10 `.wav` IRs in <1 minute while audio is running.
  *Expected:* Memory RSS remains stable (growth < 2 MB after 10 reloads). SPSC GC safely frees dropped allocations without memory leaks.
- [ ] **3.1.3 Bypass Spam:** Toggle **BYPASS** 20+ consecutive times via GUI and host automation.
  *Expected:* Zero audio artifacts, no clicks, zero DAW crashes.

---

### 3.2 Concurrent Modulation & Multi-Instance

- [ ] **3.2.1 High-Frequency LFO Sweep:** Modulate `input_gain_db` and `output_gain_db` with dual independent LFOs (5–13 Hz for ≥2 min, then 20–100 Hz for ≥5 min with 128-sample buffer).
  *Expected:* Smooth gain interpolation, zero zipper noise, zero XRUNs in `pw-top`.
- [ ] **3.2.2 Gate FSM Silence Test:** Feed signal → stop playback for 10s → resume signal.
  *Expected:* Output enters clean silence (no denormals or residual noise). Resuming signal produces no transient clicks.
- [ ] **3.2.3 1-Hour Endurance Audit:** Run 4 instances (2× WaveNet, 2× LSTM) with continuous LFO modulation for **60 minutes**. Monitor RSS, file descriptors, thread count, and XRUNs every 30 seconds.
  *Expected:* Zero crashes, RSS variation < 5 MB after initial warmup, zero FD/thread leaks, zero XRUNs across the entire session.

---

### 3.3 Audio Integrity & Determinism

- [ ] **3.3.1 Bypass Null Test:** Route clean signal through bypassed NAM-Plug in parallel with phase-inverted original signal.
  *Expected:* Absolute null silence (<−120 dBFS). Bypass is 100% bit-transparent regardless of cab sim state.
- [ ] **3.3.2 Offline Render Determinism:** Perform 2 consecutive offline bounces of a track with active NAM-Plug processing.
  *Expected:* Binary audio identity (`cmp file1.wav file2.wav` passes). Diagnostic status confirms `DEGRADE` flag is inactive during offline render.

---

### 3.4 GUI Engine & Resource Hygiene

- [ ] **3.4.1 GUI Idle CPU Reduction:** Stop audio and leave GUI window open and untouched for 30s.
  *Expected:* CPU usage of DAW process drops noticeably. Moving mouse or playing audio instantly resumes smooth GUI rendering.
- [ ] **3.4.2 OpenGL / FemtoVG Resource Leak Audit:** Open and close GUI editor 30+ times in <60s while monitoring stderr / DAW log.
  *Expected:* No OpenGL or FemtoVG resource leak warnings. Textures, arcs, and text render cleanly on every reopening.
- [ ] **3.4.3 HiDPI & Scale Factor Correctness:** Open plugin on HiDPI display (scale factor 1.5× or 2.0×) and standard 1.0× display.
  *Expected:* First frame renders at crisp native resolution without late resizing jumps or blurriness.

---

### 3.5 Automated Parity & Release Gate Verification

Before signing off on manual QA, verify that the automated release gates and mathematical parity checks have passed:

- [ ] **3.5.1 Automated Quick Verification:** Run `./utils/tests-quick.sh` (or inspect `target/logs/quick-tests.log` and `target/logs/quick-heap-audit.log`).
  *Expected:* Output confirms non-ignored unit and integration tests and the real-time allocation heap audit pass. Overall result must be `RESULT: PASS`, with the typed carrier `target/logs/tests-quick-status.tsv` (`tests-quick\tPASS|PASS_WITH_GAPS|FAIL\t<gaps|none>\t<fails|none>\t<scope=full|scope=partial>`). Every failure path emits exactly one `RESULT:` line; a missing carrier or RESULT is never success. Heavy parity matrices and codegen checks run in the long suite.
- [ ] **3.5.2 Parity Standards:** Confirm that multi-rate C++ NAMCore parity, oversampling (2×/4×), and CabSim IR convolution satisfy `ESR < 1e-8` and `SNR > 80 dB` across certification rates (44.1, 48, and 96 kHz). For detailed oracle methodology and baseline figures, see [`testing.md`](testing.md#321-multi-rate-resampling-reference-oracle).
- [ ] **3.5.3 Release Receipt:** For production builds (`utils/build-release.sh`), inspect `target/dist/release-receipt.json` to verify `status: "CERTIFIED"` with zero unapproved skipped gates.

---

## Release Acceptance Criteria (RC)

- [ ] **RC.1** Zero crashes, panics, or memory leaks across all test tiers.
- [ ] **RC.2** Zero XRUNs recorded in `pw-top` during standard playback sessions (128 samples @ 48 kHz).
- [ ] **RC.3** Zero audible zipper noise on parameter changes or automation.
- [ ] **RC.4** Bit-transparent bypass confirmed (<−120 dBFS phase cancellation).
- [ ] **RC.5** Full state roundtrip verified: Instantiate → Load → Save → Reopen → Preserved State.

---

## Standardized Bug Report Template

```text
**Test ID:** <e.g.: T1.2, 2A.2, 3.2.1>
**Environment:** <OS / Kernel, e.g. Ubuntu 24.04 / Linux 6.8-lowlatency>
**DAW & Host Mode:** <e.g. Bitwig Studio 6.0.6 (Flatpak), Embedded Mode>
**Buffer / Sample Rate:** <e.g. 128 samples @ 48 kHz>
**Active Model & IR:** <e.g. jcm800.nam, vintage30.wav>
**Expected Behavior:** <Description from testing guide>
**Observed Behavior:** <What actually happened>
**Host / Terminal Diagnostic Log:** <Paste relevant messages from DAW log, stderr, or status bar readouts>
**Attachments:** <Screenshots, pw-top output, audio clips if applicable>
```
