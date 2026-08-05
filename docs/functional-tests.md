<!--
SPDX-License-Identifier: GPL-3.0-or-later
Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
-->

# Functional Testing Guide (Human QA) — NAM-Plug (CLAP Plugin)

**Audience:** Developers, QA Testers, and End Users.
**Target DAWs:** Bitwig Studio 6+ and Fender Studio Pro 8+ (Linux, Flatpak / Native).
**Preparation:** `~/.clap/nam_plug.clap` installed (Release build via `utils/build-release.sh` or `target/release/libnam_plug.so`), ≥1 valid `.nam` model, 1 invalid file, 1 `.wav` IR file, Guitar DI / signal generator on track, `pw-top` open to monitor XRUNs (Recommended: 128 samples @ 48 kHz).

> [!NOTE]
> This guide covers manual human QA workflows. For automated unit/integration tests (`cargo test`), property testing, allocation auditing, and Criterion performance benchmarks (`cargo bench`), refer to [`testing.md`](testing.md).

---

## Executive Summary & Testing Strategy

To prevent manual QA from being skipped due to friction ("philosophy of zero laziness"), this testing guide is structured into **3 progressive tiers**:

| Tier       | Name                         | Target Duration | When to Run                            | Scope / Objective                                                     |
|:---------- |:---------------------------- |:--------------- |:-------------------------------------- |:--------------------------------------------------------------------- |
| **Tier 1** | ⚡ **2-Minute Smoke Test**   | ~2 min          | After every code commit or quick build | Catch 80% of critical regressions in 5 fast actions                   |
| **Tier 2** | 🎯 **Feature & Host Sweep**  | ~10–15 min      | PR review / Feature branch completion  | Full functional verification across Audio, Controls, Host & Telemetry |
| **Tier 3** | 🛡️ **Stress & Release Gate** | ~20–30 min      | Pre-release / Nightly audit            | Heavy multithreaded stress, HiDPI, GL cleanup, offline determinism    |

---

## Tier 1: ⚡ 2-Minute Smoke Test (5 High-Yield Actions)

*Objective: Run these 5 actions immediately after building to verify core plugin stability.*

- [ ] **T1.1 Load Model & CabSim IR:** Click `[📂 Load Model]` → select a valid `.nam` model → Click `[📂 Load IR]` → select a valid `.wav` IR.
  *Expected:* Both load without host UI freeze or audio dropouts. Audible amp modeling and IR convolution active immediately.
- [ ] **T1.2 Knobs & Double-Click Reset:** Drag **INPUT** knob to +6.0 dB, drag **GATE** to −50.0 dB → Double-click **INPUT**.
  *Expected:* Knobs move smoothly without zipper noise. Double-click instantly resets **INPUT** to 0.0 dB (GATE remains at −50.0 dB).
- [ ] **T1.3 Bypass & Adaptive VU Meter:** Insert plugin on a mono track (1 bar), then move to a stereo track (2 bars L/R) → Toggle **BYPASS**.
  *Expected:* VU layout adapts dynamically (1 centered bar mono vs 2 bars L/R). Bypass toggle instantly silences DSP processing, yielding bit-transparent dry signal without clicks.
- [ ] **T1.4 Host Parameter Automation (`[Bitwig]`):** Draw a quick automation ramp for `input_gain_db` or `output_gain_db` in DAW → start playback.
  *Expected:* GUI knob arc pulses/animates smoothly in sync with host automation; audio adjusts without zipper noise.
- [ ] **T1.5 Telemetry & Diagnostic Export:** Hover over status bar `"ℹ"` icon → click `"ℹ"` → paste (Ctrl+V) into a text editor.
  *Expected:* Status bar shows RT telemetry (sample rate, latency, DSP load). Visual toast confirmation appears; pasted text contains complete diagnostic dump.

---

## Tier 2: 🎯 Feature & Host Integration Sweep (~10–15 min)

---

### Domain 2A: Audio & DSP Engine

- [ ] **2A.1 SPSC Non-Blocking Load:** Load a different `.nam` model while playing an audio track.
  *Expected:* DAW UI remains 100% responsive. Audio engine swaps models seamlessly without stopping playback or causing XRUNs in `pw-top`.
- [ ] **2A.2 Invalid File Robustness:** Select a corrupted or 0-byte `.nam` / `.wav` file in the file picker.
  *Expected:* Red error toast (`"⚠ Load failed"`) displays for ~3s. Previous valid model/IR remains active; plugin never crashes or silences audio.
- [ ] **2A.3 CabSim IR Management:** With an IR loaded, click `[🗑 Clear IR]`.
  *Expected:* IR display reverts to `"No IR loaded"`, audio transitions cleanly back to post-model output without cab simulation. Clear button disappears.
- [ ] **2A.4 Slimmable A2 Container (`[FSM]`):** Load a slimmable container model → change host parameter `"Slim Override"` from `"Auto"` to `"Force Lite"` to `"Force Full"`.
  *Expected:* Submodel swaps instantly with a 32 ms crossfade without audible clicks. In `"Auto"`, high CPU load triggers `DEGRADE` status in telemetry.
- [ ] **2A.5 Multi-Instance Isolation:** Insert 3 NAM-Plug instances across different tracks → load different models & IRs on each.
  *Expected:* All 3 instances process audio independently with zero cross-talk, state leakage, or audio dropouts.

---

### Domain 2B: UI Controls & Dynamic Feedback

- [ ] **2B.1 Knob Tooltips & Fine-Tune:** Hover over knobs → hold **Ctrl + Drag** (or Ctrl + Scroll).
  *Expected:* Tooltips display exact values with 2 decimals (e.g. `"3.50 dB"`). Ctrl key modifies parameter ~10× slower for precision tuning.
- [ ] **2B.2 Interactive Knob Glow:** Drag any knob.
  *Expected:* Semi-transparent halo glow appears on the active arc while dragging and fades immediately upon release.
- [ ] **2B.3 VU Peak Hold & Clipping:** Feed a high-gain signal to induce clipping (>0 dBFS) → stop audio → click clipped meter bar.
  *Expected:* Peak hold bar pauses ~2s before decaying smoothly. Red clip LED at the top of the meter persists until manually clicked to reset.
- [ ] **2B.4 VU Meter L/R Channel Independence (`[Stereo Track]`):** On a stereo track, test 4 signal scenarios:
  - (a) Signal **only on L** (hard-pan L or L-only generator) → only the L bar moves; R remains at minimum.
  - (b) Signal **only on R** → only the R bar moves; L remains at minimum.
  - (c) **Symmetric signal** (equal on L and R) → L and R bars move to equal levels.
  - (d) **Asymmetric signal** (different levels) → L and R bars move independently.
  *Failure:* If L-only signal causes both bars to move equally, the test fails — `vu_l_state` and `vu_r_state` are not isolated.
- [ ] **2B.5 Keyboard Navigation & Accessibility:** Press **Tab** / **Shift+Tab** to navigate controls → use **Up/Down** arrows on knobs → **Space/Enter** on buttons.
  *Expected:* Clear focus ring cycles through interactive controls (`INPUT → OUTPUT → GATE → BYPASS → Load Model → Load IR`). Arrow keys increment/decrement values.

---

### Domain 2C: Host Integration & State Persistence

- [ ] **2C.1 Bitwig Track Color Sync (`[Bitwig]`):** Change DAW track color (e.g., Red, Blue, Green).
  *Expected:* Knob arcs and active LEDs update to match track color in <100ms (VU meters maintain standard tricolor gradient).
- [ ] **2C.2 MIDI Learn Mapping Halo (`[Bitwig]`):** Activate MIDI Learn on the **INPUT** or **OUTPUT** knob in the host.
  *Expected:* A ring of 6 small dots appears around the knob arc (color provided by host, typically `#5e81ac`). The halo disappears when MIDI Learn is deactivated. VU meters are unaffected. (Tests `INDICATION_MAPPED` bit in `param_indication.rs`.)
- [ ] **2C.3 Automation Arc Pulse & Override (`[Bitwig]`):** Play back active automation on `output_gain_db` → then manually drag the same knob while automation is playing.
  *Expected:* (a) Arc pulses smoothly (alpha 0.3→1.0, ~1s cycle) while automation is active. (b) Manual touch temporarily turns arc amber (`#F5A623`) until released. (Tests `INDICATION_AUTOMATING` and `INDICATION_OVERRIDING` bits.)
- [ ] **2C.4 Full Project State Reload:** Set custom gain, gate, model, and IR → save DAW project → close DAW → reopen project.
  *Expected:* All parameters, model path, and IR path restore perfectly with identical audio output. Missing model files degrade cleanly to `"No model loaded"`.
- [ ] **2C.5 Preset Discovery Browser:** Open the host's preset browser for NAM-Plug → inspect entries → load one.
  *Expected:* Each entry displays the model name, creator (`modeled_by`), and gear model (if present in the `.nam` metadata). Loading the preset changes the active model (audible model change) and the model name updates in Zone 1.
- [ ] **2C.6 Preset Portability (`clap.state-context`):** Move `.nam` model file to a different directory → load saved DAW device preset.
  *Expected:* Plugin locates model via search paths / basename and restores full state.
- [ ] **2C.7 Floating GUI Fallback Window:** Test plugin on a host requesting floating GUI mode → open and close the window 10+ times.
  *Expected:* GUI opens as a standalone top-level window. All 10+ cycles complete without crashes, zombie windows, or RSS growth (>5 MB would indicate a leak).
- [ ] **2C.8 Bitwig Device Panel Pages (`[Bitwig]`):** Open Bitwig Device Panel → navigate between pages.
  *Expected:* 2 pages present: **"Main"** (INPUT, OUTPUT, BYPASS) and **"Gate"** (GATE). Moving a slider in the Device Panel updates the corresponding knob in the GUI, and vice versa.
- [ ] **2C.9 Category & Search Indexing:** Search host plugin browser for `"distortion"`, `"gate"`, or `"mono"`.
  *Expected:* NAM-Plug correctly indexed under **Audio FX → Distortion**.

---

### Domain 2D: Telemetry, Diagnostics & Dynamic Latency

- [ ] **2D.1 Status Bar Cadence & Metrics:** Observe status bar for ≥20s during steady playback.
  *Expected:* Real-time telemetry (Sample Rate, Latency, DSP Load %) updates smoothly at ~1 Hz without UI flickering.
- [ ] **2D.2 Diagnostic Clipboard & File Export:** Click status bar `"ℹ"` icon.
  *Expected:* Toast displays `"Diagnostic copied · file in ~/.cache/nam-rs/"`. Diagnostic file created under `~/.cache/nam-rs/` with `0o600` permissions.
- [ ] **2D.3 Diagnostic Folder Open (`[xdg-open]`):** Click `"Open Folder"` next to toast → test fallback by removing `xdg-open` or unsetting `HOME`.
  *Expected:* Opens `~/.cache/nam-rs/` in system file manager; missing `xdg-open` or headless SSH server degrades gracefully without host crash.
- [ ] **2D.4 Dynamic Sample Rate & PDC Recalculation:** With NAM-Plug active and audio playing, change the project sample rate (e.g., 44.1 kHz → 96 kHz).
  *Expected:* Status bar updates to the new sample rate (e.g., `"96kHz"`). Reported latency updates immediately. The host (Bitwig) recalculates Plugin Delay Compensation without audio desync. No XRUNs in `pw-top`.
- [ ] **2D.5 Bypass & Model-Switch PDC Update:** Toggle bypass or swap to a model with different resampling while audio is playing.
  *Expected:* Reported latency in the status bar changes to reflect the new resampling state. Host PDC updates immediately with no dropout.

---

## Tier 3: 🛡️ Stress & Release Gate (Pre-Release Audit)

---

### 3.1 Spam & Rapid Load Stress

- [ ] **3.1.1 GUI Open/Close Spam:** Rapidly open and close GUI window 20+ times in <30s during active playback.
  *Expected:* Zero audio glitches, zero XRUNs in `pw-top`, no lingering window threads.
- [ ] **3.1.2 Model & IR Reload Flood:** Load 10 different `.nam` models and 10 `.wav` IRs in <1 minute while audio is running.
  *Expected:* Memory RSS remains stable (growth < 2 MB after 10 reloads). GC cascade frees dropped allocations properly without memory leaks.
- [ ] **3.1.3 Bypass Spam:** Toggle **BYPASS** 20+ consecutive times via GUI and host automation.
  *Expected:* Zero audio artifacts, no clicks, zero DAW crashes.

---

### 3.2 Concurrent Modulation & Multi-Instance

- [ ] **3.2.1 High-Frequency LFO Sweep:** Modulate `input_gain_db` AND `output_gain_db` with dual independent LFOs — first at 5 Hz & 13 Hz for ≥2 min, then push to 20–100 Hz for ≥5 min. 128 sample buffer throughout.
  *Expected:* Smooth gain interpolation, zero zipper noise, zero XRUNs in `pw-top`.
- [ ] **3.2.2 Gate FSM Silence Test:** Feed signal → stop playback for 10s → resume signal.
  *Expected:* Output enters clean silence (no denormals / residual noise). Resuming signal produces no transient clicks.
- [ ] **3.2.3 1-Hour Endurance Audit:** Run 4 instances (2× WaveNet, 2× LSTM) with continuous LFO modulation for **60 minutes**. Monitor RSS, file descriptors, thread count, and XRUNs every 30 seconds.
  *Expected:* Zero crashes, RSS variation < 5 MB after initial warmup (first 2 min), zero FD/thread leaks, zero XRUNs for the entire session.

---

### 3.3 Audio Integrity & Determinism

- [ ] **3.3.1 Bypass Null Test:** Route clean signal through bypassed NAM-Plug in parallel with phase-inverted original signal.
  *Expected:* Absolute null silence (<−120 dBFS). Bypass is 100% bit-transparent regardless of cab sim state.
- [ ] **3.3.2 Offline Render Determinism:** Perform 2 consecutive offline bounces of a track with active NAM-Plug processing.
  *Expected:* Binary audio identity (`cmp file1.wav file2.wav` passes). Diagnostic status confirms `DEGRADE` flag is inactive during offline render.

---

### 3.4 GUI Engine & Resource Hygiene

- [ ] **3.4.1 GUI Idle CPU Reduction:** Stop audio and leave GUI window open & untouched for 30s.
  *Expected:* CPU usage of DAW process drops noticeably. Moving mouse or playing audio instantly resumes smooth GUI rendering.
- [ ] **3.4.2 OpenGL Resource Leak Audit:** Open and close GUI editor 30+ times in <60s while monitoring stderr / DAW log.
  *Expected:* No `egui_glow` warnings (`"Resources will be leaked!"`). Graphic textures, knob arcs, and text render perfectly on reopening.
- [ ] **3.4.3 HiDPI & Scale Factor Correctness:** Open plugin on HiDPI display (scale factor 1.5× or 2.0×) and 1.0× 1080p display.
  *Expected:* First-frame renders at crisp native resolution without late resizing jumps or blurriness.

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
**Telemetry Dump:** <Paste output from clicking the status bar "ℹ" diagnostic button>
**Attachments:** <Screenshots, pw-top output, audio clips if applicable>
```
