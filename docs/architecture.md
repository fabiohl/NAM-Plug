<!--
SPDX-License-Identifier: GPL-3.0-or-later
Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
-->

# NAM-Plug Architecture: CLAP Plugin

This document is the primary architecture reference and source of truth for **NAM-Plug**, a Neural Amp Modeler (NAM) audio plugin built on the CLAP (CLever Audio Plug-in) specification for Linux.

NAM-Plug wraps the low-latency DSP neural inference engine provided by the [`NeuralAmpModeler-rs`](https://github.com/fabiohl/NeuralAmpModeler-rs) library crate into a production-grade CLAP plugin. It manages plugin lifecycle, DAW parameter automation, lock-free real-time audio processing, state persistence, host extension compliance, and an immediate-mode graphical user interface.

> **Scope Boundary:** For microarchitectural details on neural model execution (WaveNet, LSTM, ConvNet, Linear), SIMD kernels (AVX2 `x86-64-v3`), polyphase resamplers, half-band FIR filters, or model loading formats (`.nam`/`.namb`), see the core engine documentation in [`NeuralAmpModeler-rs`](https://github.com/fabiohl/NeuralAmpModeler-rs) (`docs/architecture.md`).

---

## 1. System Topology & Thread Model

NAM-Plug enforces strict thread segregation to guarantee Real-Time (RT) safety during audio processing while providing responsive UI rendering and host DAW coordination.

```text
┌─────────────────────────────────────────────────────────────────────────────┐
│                            DAW Host Environment                             │
└──────┬──────────────────────────────────┬───────────────────────────┬───────┘
       │ (C-ABI Extensions)               │ (process() Callback)      │ (Host Window)
       ▼                                  ▼                           ▼
┌─────────────────────────────┐  ┌─────────────────────────┐  ┌────────────────────────┐
│        Main Thread          │  │    Audio Thread (RT)    │  │       GUI Thread       │
│  - Plugin Lifecycle         │  │  - Hard RT Contract     │  │  - Slint Event Loop    │
│  - Parameter Scanning       │  │  - Zero Heap Alloc      │  │  - "nam-slint-gui"     │
│  - State Save / Load        │  │  - Zero Mutex Locks     │  │  - Slint v1.18 FemtoVG │
│  - nam-model-loader Offload │  │  - Zero Blocking I/O    │  │  - Wayland & X11       │
│  - GC Tier 1 Disposal       │  │  - DSP Signal Chain     │  │  - Async rfd FileDialog│
│                             │  │                         │  │  - 5-Zone UI Layout    │
└──────────────┬──────────────┘  └────────────▲────────────┘  └───────────┬────────────┘
               │                              │                           │
               │ SPSC Command Queues          │ Atomics & Peak Telemetry  │
               └──────────────────────────────┴───────────────────────────┘
```

### 1.1 Thread Roles & Hard Contracts

- **Main Thread (Host):**

  - **Lifecycle & State:** Handles plugin lifecycle (`init`, `activate`, `deactivate`, `destroy`), parameter scanning, and DAW project state serialization/restoration (`clap_plugin_state`).
  - **Worker Offloading:** Dispatches asynchronous `.nam`/`.namb` model loading to `"nam-model-loader"` and CabSim IR reading via `cabsim::loader`.
  - **Garbage Collection:** Drains the Tier 1 SPSC deallocation queue (`gc_tx`).
  - **Autonomous Housekeeping:** When `clap_plugin_timer_support` is available, registers a periodic 250 ms watchdog timer with the host. This timer executes `housekeeping()` (recovering poisoned RT processors via `request_restart()`, announcing dynamic PDC latency changes via `HostLatency::changed()`, and draining GC queues) independently of GUI events. Falls back to event-driven housekeeping on hosts without timer support.

- **Audio Thread (RT):**

  - Driven exclusively by the host `process()` callback (`PluginAudioProcessor::process` in `src/clap/processor/mod.rs`).

  > [!IMPORTANT]
  > **Hard Real-Time Contract:** Zero heap allocations, zero mutex locks, zero blocking I/O, zero panics. Operates directly on host audio buffers using host-driven single-callback processing.

- **GUI Thread:**

  - Dedicated `"nam-slint-gui"` event loop thread running `slint::run_event_loop()` with Slint 1.18 (FemtoVG / winit backend).
  - Fully isolated from the audio thread, synchronizing peak telemetry, DSP load, and parameter changes via `SlintViewModel` at 60 Hz (relaxed to 30 Hz while the signal is silent and the UI idle; suspended while the window is hidden).

### 1.2 Host vs. Plugin Real-Time Boundary & `rt-hardening` Delegation

The underlying DSP engine (`NeuralAmpModeler-rs`) provides an optional `rt-hardening` feature with Linux system utilities for memory locking (`mlockall`), thread scheduling (`SCHED_FIFO`), CPU core affinity, and PM-QoS (`/dev/cpu_dma_latency`).

**Architectural Decision:** `NAM-Plug` **strictly disables** `rt-hardening`. Process-level real-time hardening is the exclusive responsibility of the DAW host:

1. **Address Space Isolation (`mlockall`):** As a dynamic shared library (`cdylib`), calling `mlockall(MCL_CURRENT | MCL_FUTURE)` from inside a plugin would lock all virtual pages belonging to the host and sibling plugins, risking immediate `ENOMEM` in production sessions with large sample libraries.
2. **Thread Scheduling & Affinity (`sched_setscheduler`, `pthread_setaffinity_np`):** Audio threads are created and scheduled by the DAW host. Forcing `SCHED_FIFO` or thread pinning from within a plugin disrupts host multicore load-balancing and violates the CLAP specification.
3. **Global System Resources (`PM-QoS`):** `/dev/cpu_dma_latency` is a system-wide resource; individual plugins must not contend for global DMA latency locks.

**Local RT Safety Invariants:** While delegating host-level thread policy to the DAW, `NAM-Plug` enforces strict real-time discipline internally:

- **Thread-Local FTZ / DAZ:** Configured on the audio thread upon first execution (`apply_rt_thread_setup()` in `start_processing()`) and defended on every callback via RAII guard (`FtzDazGuard` in `src/clap/processor/ftz.rs`). The guard asserts FTZ (bit 15) and DAZ (bit 6) in MXCSR and restores the host's exact MXCSR on scope exit, preventing denormal performance stalls while ensuring zero host state leakage.
- **Strict Audio Callback Contract:** Zero heap allocations (enforced via `heap-audit`), zero mutex locks, zero blocking I/O, and zero panics.
- **Passive RT Telemetry:** Thread scheduling, CPU affinity, and processing health are passively reported through lock-free atomic bitmasks (`RtStatusFlags`).

---

## 2. Compilation Strategy & Feature Flags

`NAM-Plug` compiles into a dynamic shared library (`libnam_plug.so`, installed as `nam_plug.clap`) plus auxiliary binaries under `src/bin/`. Standalone PipeWire hosting is handled separately by `NAM-Audio-Pipe`.

| Binary                   | Required Feature | Role                                                                                        |
|:------------------------ |:---------------- |:------------------------------------------------------------------------------------------- |
| `ui_preview`             | —                | Standalone GUI preview without a DAW host.                                                  |
| `plug_ab_opt`            | —                | A/B optimization ceremony comparator (`plug_ab_opt compare`), see §9.7.1.                   |
| `pgo_profiling_workload` | `testing`        | Representative DSP workload used to collect PGO profiles in `utils/build-release.sh`.       |
| `nam_perf_guard`         | `testing`        | Performance certification gate for the shipped `.clap` artifact (`nam_perf_guard certify`). |
| `ab_opt_bench`           | `testing`        | Per-variant benchmark driver consumed by the A/B ceremony.                                  |
| `plug_long_receipt`      | `testing`        | Typed receipt emitter/verifier and release certifier (`plug_long_receipt certify`).         |

| Feature Flag | Default | Description                                                                                                                                                                                                                                                   |
|:------------ |:------- |:------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `dual-mono`  | Yes     | Enables two independent, identical L/R dual-mono inference and CabSim IR convolution chains with zero channel crosstalk, plus dynamic adaptive stereo VU metering and port announcement. Without this feature, only the Mono port configuration is announced. |
| `testing`    | No      | Enables internal test harnesses, fixture resolution, and the testing-only binaries listed above.                                                                                                                                                              |
| `heap-audit` | No      | Activates the global allocation counting allocator (`CountingAllocator`) for RT-safety verification.                                                                                                                                                          |
| `gui-x11`    | No      | Enables display-server GUI lifecycle tests; exercised only by the dedicated Xvfb `gui` phase of `utils/tests-long.sh`.                                                                                                                                        |

```bash
# Standard release build (produces target/release/libnam_plug.so)
cargo build --release

# Development build with testing harness
cargo build --features testing
```

### 2.1 Link-Time Symbol Hardening & ELF Interposition Defense

To prevent fatal runtime CPU spin loops in Linux release builds caused by dynamic linker (`ld.so`) symbol interposition:

- **The ELF Interposition Hazard:** When third-party dependencies define bare C-ABI fallback routines for standard math functions (`log10f`, `atan2f`, `acosf`, etc.) with global visibility, `ld.so` resolves dynamic `JUMP_SLOT` relocations back to the local plugin binary instead of system `libm.so.6`. This creates a self-referential `trampoline -> PLT -> GOT -> trampoline` infinite loop (100% CPU spin, zero syscalls).
- **Hardening Guard (`build.rs`):** the anonymous version script ([`.cargo/hide-libm-shadow.map`](../.cargo/hide-libm-shadow.map)) is applied via `-Wl,--version-script=...` and `-Wl,--undefined-version` to every executable target (`bins`, `tests`, `benches`, `examples`), demoting shadowed `libm` symbols to `local` binding so dynamic calls resolve strictly to the system's `libm.so.6`. The CLAP `cdylib` relies on rustc's own anonymous export list (`global: clap_entry + Slint vtables; local: *`), which already localizes every other symbol: GNU `ld.bfd` rejects combining that mandatory list with a second version script, whereas `rust-lld` tolerates it. Both linkers are verified with `nm -D` (`clap_entry` exported; no `libm` function defined as a global dynamic symbol).
- **Reference:** For the in-depth disassembly trace and relocation analysis, see [`NeuralAmpModeler-rs/docs/postmortem-libm-symbol-interposition.md`](https://github.com/fabiohl/NeuralAmpModeler-rs/blob/main/docs/postmortem-libm-symbol-interposition.md).

---

## 3. Plugin Descriptor & Parameter Surface

### 3.1 Plugin Descriptor

Returned by `nam_descriptor()` (`src/clap/descriptor.rs`) during host scan without heap allocation:

| Field        | Value                                                              |
|:------------ |:------------------------------------------------------------------ |
| **ID**       | `br.eti.fabiolima.nam-plug`                                        |
| **Name**     | `NAM-Plug`                                                         |
| **Vendor**   | `Fabio Lima`                                                       |
| **URL**      | `https://github.com/fabiohl/NAM-Plug`                              |
| **Features** | `["audio-effect", "distortion", "gate", "mono"]` (CLAP 1.2.2 spec) |

Core DSP neural inference is mono by definition. Buffer extraction and VU metering adapt dynamically to mono or stereo host track configurations.

### 3.2 Parameter Surface Catalog

Exposed via `src/clap/extensions/params/` and synchronized via `src/clap/processor/params.rs` (using engine parameters from `RtProcessingParams`). Parameter IDs are `u32` constants (`PARAM_*`, 0–13):

| Parameter                    | ID                            | Type    | Range / Options                                                 | Description                                                                     |
|:---------------------------- |:----------------------------- |:------- |:--------------------------------------------------------------- |:------------------------------------------------------------------------------- |
| **Input Gain**               | `input_gain_db` (0)           | dB      | `-96.0` to `+30.0` (default `0.0`)                              | Pre-inference gain, sample-accurate smoothed.                                   |
| **Output Gain**              | `output_gain_db` (1)          | dB      | `-96.0` to `+30.0` (default `0.0`)                              | Post-inference gain, sample-accurate smoothed.                                  |
| **Gate Threshold**           | `gate_threshold_db` (2)       | dB      | `-90.0` to `-40.0` (default `-90.0`, off by default)            | Noise-gate opening threshold.                                                   |
| **Bypass**                   | `bypass` (3)                  | Binary  | `0.0` (false / default) / `1.0` (true)                          | Disables full plugin processing (64-sample click-free linear crossfade passthrough). |
| **Active Model**             | `active_model` (4)            | String  | `0.0` to `1000.0` (Read-only, default `0.0`)                    | Filename / identification of currently loaded model.                            |
| **Adaptive Compute**         | `adaptive_compute` (5)        | Stepped | `0` (`Off`), `1` (`Conservative` / default), `2` (`Aggressive`) | CPU-based dynamic degradation FSM.                                              |
| **Slim Override**            | `slim_override` (6)           | Stepped | `0` (`Auto` / default), `1` (`ForceFull`), `2` (`ForceLite`)    | Slimmable A2 container submodel selection.                                      |
| **Oversampling**             | `oversample` (7)              | Stepped | `0` (`Off` / default), `1` (`2x`), `2` (`4x`)                   | Activation oversampling factor.                                                 |
| **Activation Precision**     | `activation_precision` (8)    | Stepped | `0` (`Fast`), `1` (`Standard` / universal default)              | Math mode (`Standard` exact / `Fast` Padé-minimax).                             |
| **Calibrate Input**          | `calibrate_input` (9)         | Binary  | `0` (`Off` / default) / `1` (`On`)                              | Enables pre-nonlinearity input calibration from model `input_level` dBu.         |
| **Input Calibration Level**  | `input_calibration_level` (10)| dBu     | `-60.0` to `+60.0` dBu (default `12.0`)                         | Reference 0 dBFS input level in dBu used for calibrated gain equations.         |
| **Output Mode**              | `output_mode` (11)            | Stepped | `0` (`Raw`), `1` (`Normalized` / default), `2` (`Calibrated`)   | Output level normalization mode (`loudness`, `output_level`, or unity).         |
| **Gate Mode**                | `gate_mode` (12)              | Stepped | `0` (`Silence` / default), `1` (`Musical`)                      | Gate topology (`Silence` post-IR Schmitt gate vs `Musical` pre-IR expander).     |
| **Cab Bypass**               | `cab_bypass` (13)             | Binary  | `0.0` (false / default) / `1.0` (true)                          | Independently bypasses the cabinet slot with constant PDC latency and 64-sample cosine crossfade. |

The table lists the host-facing (authoritative) ranges. The GUI knobs expose a narrower, ergonomic subset (`INPUT` −20 to +20 dB, `OUTPUT` −40 to +20 dB); values written by the GUI are clamped to the host range by the parameter layer (`src/clap/extensions/params/mod.rs`).

Model file paths (`.nam`/`.namb`) and Cabinet slot file paths (`.wav`, `.nam`, or `.namb`) are managed as **DAW State Properties** (`clap_plugin_state`), enabling project-level serialization and restoration.

### 3.3 Mandatory Asset Identity (SHA-256)

Every asset reference persisted in state **must** carry its SHA-256 hex digest (`model_hash` / `ir_hash`, exactly 64 characters) — representing the content-based portable identity of the file.

> **Asset Verification Invariant:** No asset is adopted without a valid digest verified in the same restore cycle, preventing silent corruption across different machines.

| Path Context                           | Verification Policy                                                                                                                                                                                                                                                                                                                                                 |
|:-------------------------------------- |:------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Full Restore** (Project / Duplicate) | If `model_path` exists, the digest is mandatory and must match the file at restore time. A missing, malformed, or divergent digest rejects that path. The basename fallback executes only when the expected digest exists and matches a candidate. Full state without an expected digest results in an explicit failure — leaving the previously active DSP intact. |
| **Basename Search** (Cross-Machine)    | A candidate file is adopted only when its computed digest equals the saved `model_hash`. A reference without a saved hash is rejected outright.                                                                                                                                                                                                                     |
| **CabSim IR**                          | The same rule applies: an `ir_path` without a matching `ir_hash` never loads the WAV file.                                                                                                                                                                                                                                                                          |
| **Legacy Presets (No Hash)**           | Rejected during automatic restore. Users migrate presets via the explicit GUI action — loading the model/IR file via the dialog recomputes the digest and saves an updated preset.                                                                                                                                                                                  |
| **State Save**                         | `state.save` and `state-context.save` refuse to persist any asset reference lacking a well-formed digest.                                                                                                                                                                                                                                                           |

All adoption paths compute the digest up-front and **fail closed** if it cannot be computed (e.g., file exceeding the 256 MiB streaming-hash limit). An asset without a digest is never adopted and never persisted.

> **GAP-IR-INMEMORY-DECODE:** IR bytes are read once, bounded by 256 MiB, hashed, and staged to an exclusively-created private temp file (`create_new`, removed on every exit path) before `CabSimIr::load` decodes them. The pinned engine (`NeuralAmpModeler-rs` 2.1.0) exposes no `load_from_bytes` API, so a true in-memory decode cannot be implemented without expanding engine scope. The temp round-trip is content-safe (the verified buffer is written verbatim, never re-read from the source path), but it remains a documented residual GAP; removing it requires a new engine API.

---

## 4. CLAP Extensions Framework

Extensions are registered in `declare_extensions()` (`src/clap/plugin/mod.rs`) via `clack-extensions`:

| Extension                             | Reference File                                  | Purpose                                                                                     |
|:------------------------------------- |:----------------------------------------------- |:------------------------------------------------------------------------------------------- |
| `clap_plugin_audio_ports`             | `src/clap/extensions/audio_ports.rs`            | Single main input/output audio port (Mono 1 ch or Stereo 2 ch per negotiated config).       |
| `clap_plugin_audio_ports_config`      | `src/clap/extensions/audio_ports.rs`            | Layout negotiation: Config 0 ("Mono", default) and Config 1 ("Stereo"). Selection when off. |
| `clap_plugin_audio_ports_config_info` | `src/clap/extensions/audio_ports.rs`            | Introspection of active configuration and port characteristics per configuration ID.        |
| `clap_plugin_audio_ports_activation`  | `src/clap/extensions/audio_ports_activation.rs` | Port activation gating; rejects port > 0; strictly rejects Float64 (`f32` only).            |
| `clap_plugin_params`                  | `src/clap/extensions/params/`                   | Parameter mapping, DAW automation, gesture tracking, and `flush()`.                         |
| `clap_plugin_state`                   | `src/clap/extensions/state.rs`                  | DAW project state serialization (parameters + model path + IR path).                        |
| `clap_plugin_state_context`           | `src/clap/extensions/state_context.rs`          | Context-aware state restore (distinguishes portable presets vs. project duplicates).        |
| `clap_plugin_latency`                 | `src/clap/extensions/latency.rs`                | Dynamic latency reporting (resampler + oversample + CabSim total delay).                    |
| `clap_plugin_track_info`              | `src/clap/extensions/track_info.rs`             | Host track color synchronization to GUI accent theme.                                       |
| `clap_plugin_remote_controls`         | `src/clap/extensions/remote_controls.rs`        | "Main" and "Gate" control pages for hardware controllers / DAW device panels.               |
| `clap_plugin_param_indication`        | `src/clap/extensions/param_indication.rs`       | GUI visual cues for mapped/automated/overridden parameter status.                           |
| `clap_plugin_preset_load`             | `src/clap/extensions/preset_load.rs`            | Direct model loading (`.nam`/`.namb`) from host preset browser.                             |
| `clap_plugin_render`                  | `src/clap/extensions/render.rs`                 | Offline render detection. Forces `AdaptiveCompute::Off` + `Standard` activation precision.  |
| `clap_plugin_tail`                    | `src/clap/extensions/tail.rs`                   | Host tail query reporting remaining CabSim IR ring-out frames.                              |
| `clap_plugin_timer_support`           | `src/clap/extensions/timer.rs`                  | Periodic 250 ms watchdog timer driving autonomous housekeeping and RT recovery.             |
| `clap_plugin_gui`                     | `src/clap/extensions/gui.rs`                    | Hardware-accelerated Slint GUI with native X11 XEmbed embedding and floating fallback.      |

> [!NOTE]
> `clap_plugin_track_info` and `clap_plugin_param_indication` are fully negotiated and their data is stored in shared state (e.g., `track_accent_color`), but the current Slint GUI does not yet render track-color accents or mapping/automation indications.

Besides the plugin factory, `NamEntry` (`src/clap/entry.rs`) registers a **preset discovery factory** (`NamPresetDiscoveryFactory`, `src/clap/factory/preset_discovery.rs`) that serves `.nam`/`.namb` model metadata from `~/.nam/models` to host browsers. Using `IndexerInfo` at `create_provider()` time, it declares `CLAP_PRESET_DISCOVERY_LOCATION_PLUGIN` (with per-file `load_key` metadata) for PreSonus/Fender indexers and `CLAP_PRESET_DISCOVERY_LOCATION_FILE` for Bitwig Studio and other standard indexers, while `PluginPresetLoadImpl::load_from_location` (`src/clap/extensions/preset_load.rs`) accepts both location kinds symmetrically and preserves `PresetLocationOrigin` across asynchronous model load notifications.

### 4.1 Windowing Support Matrix

| Negotiation (`is_floating`)         | Wayland Session (`WAYLAND_DISPLAY`)                                                                | X11 Session / XWayland                                       |
|:----------------------------------- |:-------------------------------------------------------------------------------------------------- |:------------------------------------------------------------ |
| `CLAP_WINDOW_API_X11`, **embedded** | `create` rejected unless `DISPLAY` reachable (XWayland); forces X11 loop → **Native XEmbed child** | **Native XEmbed child** of the host `Window` (preferred API) |
| `CLAP_WINDOW_API_X11`, floating     | X11 floating window via XWayland                                                                   | Plugin-owned top-level X11 window                            |
| `CLAP_WINDOW_API_WAYLAND`, floating | Native Wayland top-level window (preferred API)                                                    | Wayland window if compositor is present; else rejected       |
| `CLAP_WINDOW_API_WAYLAND`, embedded | **Rejected** (no XEmbed equivalent on Wayland per CLAP spec)                                       | **Rejected**                                                 |

**Guarantees:**

- An embedded request is honored **only** when true embedding succeeds (create-time viability gate + strict `set_parent` error path). The plugin never silently delivers a floating window for an embedded request and never creates two windows.
- Repeated `create`/`destroy` cycles reuse the persistent per-instance GUI worker.

#### 4.1.1 Host Integration Compatibility Matrix

| Host / Session                             | GUI Presentation                      | Operational Behavior                                                                                                                                                                                                                |
|:------------------------------------------ |:------------------------------------- |:----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Bitwig Studio** (X11)                    | Embedded (XEmbed)                     | Native XEmbed child window integration; fully verified.                                                                                                                                                                             |
| **REAPER** (Native Linux, X11)             | Embedded (XEmbed)                     | Native XEmbed child window integration; fully verified.                                                                                                                                                                             |
| **X11 Hosts in General**                   | Embedded / Floating                   | Supported as negotiated by host (`is_floating`).                                                                                                                                                                                    |
| **Fender Studio Pro 8.2** (Linux, Wayland) | Generic Host Controls (No Plugin GUI) | Host requests only embedded Wayland (`CLAP_WINDOW_API_WAYLAND` with `is_floating = false`) and does not negotiate floating by host design, falling back to its native generic parameter controls. Preset browsing/loading from `~/.nam/models` via `CLAP_PRESET_DISCOVERY_LOCATION_PLUGIN` and `.song` state persistence are verified. |
| **Wayland Hosts Requesting Floating**      | Floating Window (*Experimental*)      | Plugin code supports `CLAP_WINDOW_API_WAYLAND` with `is_floating = true`; marked **experimental** due to absence of an available test host that negotiates floating windows under Wayland.                                          |

### 4.2 Audio Port Layout Negotiation Protocol

NAM-Plug negotiates channel configurations using standard CLAP `audio-ports-config`:

- **Design Comparison:** Declaring two independent mono ports (`Port 0` and `Port 1`) violates standard DAW track semantics; DAWs (such as Bitwig, Reaper) treat `Port 1` as an auxiliary sidechain or secondary bus rather than the Right channel of a stereo track. In contrast, `audio-ports-config` allows declaring full layout sets over a single main port:
  - **Config 0 (`Mono`, default):** 1 main port with 1 channel. Maintains backward compatibility with mono-only sessions and provides zero-overhead mono processing. In mono mode, only the active channel (L) is processed; the inactive side remains in full bypass with zero DSP CPU consumption.
  - **Config 1 (`Stereo` / Dual-Mono):** 1 main port with 2 channels (announced strictly when compiled with the `dual-mono` feature). Enables two independent, identical L/R dual-mono inference chains and independent CabSim IR convolution pairs (`CabSimPair`) with zero channel crosstalk.
- **Strict Host Negotiation (No Automatic Track Adaptation):** If the host does not invoke `PluginAudioPortsConfig::select()`, Config 0 (`Mono`) remains active. The plugin does NOT attempt automatic, heuristic content-based or track-type sniffing; audio routing and port width are negotiated strictly through the CLAP extension contract.
- **Deactivation Invariant:** Reconfiguration via `PluginAudioPortsConfig::select()` is permitted only while the plugin is deactivated (`is_processing == false`).
- **Activation & Precision Safety:** `PluginAudioPortsActivation` governs port 0 activation. Requests with `SampleSize::Float64` are strictly rejected (`return false`) because NAM-Plug's DSP kernels run exclusively in IEEE-754 32-bit float (`f32`).
- **Validation Status & GAP-HOST-MONO-STEREO:** DSP parity, zero crosstalk, anti-collapse hysteresis, and port gating are fully validated in unit/integration harnesses (`src/clap/processor_dual_mono_test.rs`). However, live validation inside real DAW hosts (Bitwig, REAPER) for dynamic configuration selection, session save/reload migration, and exact CPU/RSS scaling is formally classified as an explicit GAP (GAP-HOST-MONO-STEREO) pending physical host testing.

---

## 5. Lock-Free Communication & RT Safety Architecture

Shared cross-thread state is anchored in `NamClapShared` (`src/clap/plugin/shared.rs`). The audio thread hot path uses **zero mutexes, zero locks, and zero allocations**, relying on atomic primitives and lock-free SPSC channels (`rtrb`).

```text
┌─────────────────────────────────────────────────────────────────────────────┐
│                          NamClapShared Allocation                           │
├─────────────────────────────────────────────────────────────────────────────┤
│  #[repr(align(128))] RtToUi      (RT Thread -> UI Reads)                    │
│    - ui_peak_l / ui_peak_r, ui_clipped, active_channel_count, latency       │
├─────────────────────────────────────────────────────────────────────────────┤
│  #[repr(align(128))] UiToRt      (UI Thread -> RT Reads)                    │
│    - 8 Parameter Atomics, gesture_flags, gui_param_generation               │
├─────────────────────────────────────────────────────────────────────────────┤
│  #[repr(align(128))] ColdShared  (Main / UI / RT Setup)                     │
│    - SPSC Command Queues, pending_model payload, IR state, alive_fence      │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 5.1 Cache-Line Isolation (`#[repr(align(128))]`)

To eliminate CPU cache bouncing (False Sharing) between the high-frequency audio thread and the UI/Main threads:

- **`RtToUi`** — Dedicated 128-byte aligned cache line for telemetry communicated from RT to UI (`ui_peak_l`, `ui_peak_r`, `ui_clipped`, `ui_clip_indicator`, `ui_gate_active`, `current_latency`, `cabsim_tail_samples`, `active_channel_count`).
  - Peak levels are accumulated across audio blocks by RT via `AtomicU32::fetch_max` and read/cleared by UI via `swap(0.0f32.to_bits())` without TOCTOU races.
  - The `ui_clipped` sticky flag is set by RT and cleared by UI upon acknowledgement.
- **`UiToRt`** — Dedicated 128-byte aligned cache line for parameters written by UI (`input_gain_db`, `output_gain_db`, etc.) and the `gui_param_generation` counter.
- **`ColdShared`** — Dedicated 128-byte aligned structure for low-frequency channels and lifecycle flags (`alive_fence`, pending payloads).

### 5.2 Generation-Counter Parameter Synchronization

To prevent loading 8 atomic floats on every audio block when parameters are stationary:

1. UI/Main updates parameter atomics and executes `fetch_add(1, Release)` on `gui_param_generation`.
2. Audio thread performs a single `Acquire` load of `gui_param_generation`. If unchanged, per-parameter reads are skipped. If incremented, it reads parameter targets with `Relaxed` ordering and passes them to `ParamSmoother`.

### 5.3 Three-Tier Real-Time Garbage Collection Cascade

Deallocating complex DSP structures (`Box<StaticModel>`, `Box<NamResampler>`, `ConvEngine`, `OversampleEngine`) on the audio thread causes allocator locks and priority inversion. Disposal cascades through three lock-free tiers (`gc_cascade` in `neural_amp_modeler_rs::common::spsc::gc`):

```text
RT Thread (Replaced Asset)
       │
       ▼
[Tier 1: SPSC gc_tx (32 slots)] ──────► Drained by Main Thread (housekeeping)
       │ (if full)
       ▼
[Tier 2: Processor Parking Lot (16 slots)] ──► Flushed back to SPSC next block;
       │                                          single-owner handoff on deactivate
       │ (if full)
       ▼
[Tier 3: GcOverflowBuffer Atomic Ring] ────► Controlled Leak + Sets RT_STATUS_GC_OVERFLOW
```

1. **Tier 1 (SPSC GC Queue):** Push to `gc_tx` (32 slots). The main thread drains and drops objects during periodic `housekeeping()`.
2. **Tier 2 (Processor Parking Lot):** Fixed array `[Option<GcItem>; 16]` inside `PluginAudioProcessor`. Flushed back to SPSC whenever capacity frees. At plugin teardown, `deactivate()` passes `&mut parking_lot` to `drain_gc_final()` for single-owner disposal on the main thread.
3. **Tier 3 (`GcOverflowBuffer`):** Atomic ring buffer. Overwrites the oldest slot if completely saturated, setting `RT_STATUS_GC_OVERFLOW` to preserve hard real-time deadlines.

### 5.4 Poison-Resilient Activation Rollback Guard (`ActivateRollbackGuard` & `StagedItemsRollbackGuard`)

During plugin `activate()` (`src/clap/processor/activate.rs`), channel receivers (`param_rx`, `slimmable_rx`), `gc_tx`, and `deactivated_dsp` are transferred to the audio processor under `ActivateRollbackGuard` (`src/clap/processor/rollback.rs`). If activation panics or fails midway, `Drop` recovers poisoned mutexes and restores all resources back into `ColdShared` without dropping them on the audio thread.

Furthermore, `StagedItemsRollbackGuard` guarantees that any staged state restore or hot-swap payloads (`StagedRestore`, `StagedSwap`) are disarmed only upon successful processor construction, restoring staged items back to `NamClapMainThread` if activation encounters an error or early returns.

### 5.5 Cold-Path Latency Caching (`cached_effective_latency`)

Recomputing effective latency (resampler latency + oversample latency + CabSim IR latency) requires arithmetic and multiple structure queries. Effective latency is cached in `cached_effective_latency` (`src/clap/processor/state.rs`) and recomputed strictly during cold swap routines (`cold_load_model`, `cold_load_cabsim`, `apply_oversample`), eliminating latency math from the hot `process_events` path while driving instant PDC updates via `clap_plugin_latency`.

### 5.6 FFI Lifetime Safety, Panic Guard & Multi-Instance Isolation

- **Refcounted Shared State (`Arc<GuiSharedState>`):** `NamClapShared` owns plugin shared state through `Arc<GuiSharedState>` (`src/clap/plugin/shared.rs`). Windows, background file pickers, and teardown reaper threads all hold refcounted references, preventing use-after-free conditions.
- **Logical Validity Gate (`alive_fence`):** An `Arc<AtomicBool>` lowered in `NamClapShared::drop`. During teardown, the main thread lowers the fence and joins background threads before releasing state. Callbacks gated by the fence become no-ops, preventing callbacks to the host after `destroy()`.
- **Safe Host Bridge (`GuiHostBridge`):** Stores the raw host pointer as `NonNull<()>` and reconstructs `HostSharedHandle<'static>` on demand, reflecting the CLAP guarantee that the host outlives the plugin.
- **C-ABI Panic Guard & Processor Poisoning Latch:** All CLAP entry points wrap execution in `catch_unwind(AssertUnwindSafe(..))`. If a panic occurs on the audio thread:
  1. The panic payload is dropped without allocation (`panic_to_error` uses static catalog messages).
  2. The processor latches `self.poisoned = true`, zeroes output audio ports, and asserts `RT_STATUS_PROCESSOR_POISONED`.
  3. Subsequent audio blocks execute an $O(1)$ containment branch, maintaining silence and returning `Ok(ProcessStatus::Continue)` without allocations or I/O, preventing panic cascades and acoustic spikes.
  4. Off-RT, the main thread detects the poisoned bit during `housekeeping()` and triggers `host.request_restart()`.
  5. Upon `deactivate()`, corrupted DSP state is discarded (`deactivated_dsp = None`). Subsequent activation cleanly resumes processing.
- **Multi-Instance Isolation:** Every instance receives a unique `instance_id` (`next_instance_id()`). Diagnostics and logging sinks are scoped per instance, preventing crosstalk in multi-track DAW sessions.

### 5.7 Deferred Model Load (`pending_model`)

When DAW state is restored prior to `activate()`, the host buffer size (`max_frames_count`) is unknown. The model payload is cached in `ColdShared::pending_model` and flushed via `flush_pending_model()` during `activate()` or initial `housekeeping()`.

**UI Synchronization Invariant:**

- `commit_prepared_model` publishes identity (`params.model_path/hash`) up-front, but display telemetry (`ui_model_metadata`, `ui_model_name`, `model_load_counter`, and error clearing) lands together in `finish_commit_ui`. The UI advances only after the model is retained.
- `flush_pending_model` uses an RAII restore guard: any error re-inserts the model into `pending_model` for retry instead of dropping it, setting `RT_STATUS_MODEL_LOAD_FAILED` and populating `ui_load_error_msg`.

### 5.8 Channel Preservation and DSP Resource Reuse on `deactivate()`

Calling `deactivate()` (`src/clap/processor/deactivated.rs`) returns SPSC channel consumers (`param_rx`, `gc_tx`, `slimmable_rx`) back into `ColdShared`, allowing hosts to stop and restart audio processing without reconstructing channels or reallocating memory.

Additionally, `DeactivatedDspState` preserves heavy DSP resources (active neural model weights, CabSim convolution pair, polyphase resamplers, streaming resample buffers, and oversampling engines) across deactivate/activate cycles:

- **Model weights:** Reusable unconditionally across activations, as weights are independent of host sample rate and buffer size.
- **CabSim convolution pair:** Reusable across buffer size (quantum) renegotiations because the underlying driver partitions blocks internally; only an effective host sample rate change forces a rebuild.
- **Resampler & Streaming buffers:** Reused if the newly negotiated host sample rate and maximum block size match the preserved configuration.
- **Poisoning Invariant:** If the processor was poisoned by a runtime panic, `deactivated_dsp` is discarded (`None`) so that the subsequent activation rebuilds a clean DSP pipeline from scratch.

### 5.9 Transactional State Restore (Stage / Commit / Ack)

State restoration (`clap_plugin_state`, `clap_plugin_state_context`, and preset loading) follows a 3-phase **Stage → Commit → Ack** protocol ensuring atomic transitions where no observer ever sees a hybrid of two restore generations:

1. **Stage:** `build_restore_package()` validates asset digests, resampler feasibility, and parameter subsets. Heavy resources (model, IR adapter) are moved **by value** into the transaction without modifying active DSP state.
2. **Commit (RT):** The package is pushed as a single `ClapParamPayload::RestoreTxn` item into the SPSC command ring. The audio thread applies model, IR, and parameters atomically in a single audio block without heap allocation.
3. **Ack:** Once the audio thread acknowledges the sequence number (`is_acked(seq)`), `publish_restore()` updates UI metadata, parameter atomics, and host state flags. UI and disk observe the new state only after the audio thread has committed it.

### 5.10 Host Log & Host Call Quiescence and Teardown Protocol

To prevent Use-After-Free (UAF) of the host handle when tearing down plugin instances:

- **Strict Memory Safety over Bounded Teardown:** Under the CLAP specification, the DAW host owns the `clap_host` pointer and can deallocate it immediately once `destroy()` returns. If a background worker (`ModelLoader`, `GuiWorker`, `file_dialogs`) or logger thread is executing a host callback (`host_log.log()`, `request_callback()`, `request_flush()`, `gui_host.closed()`), returning before that callback finishes causes an immediate UAF crash.
- **Coordination Mechanisms:**
  - **`HostLogQuiescence`:** Coordinates logger sinks, ensuring `host_log.log()` dispatches quiesce cleanly before the host sink is cleared.
  - **`HostCallQuiescence`:** Unified barrier coordinating all external host calls from detachable threads (`request_callback`, `request_flush`, `closed`).
  1. **Immediate Rejection:** As soon as instance teardown begins (`alive_fence` lowered), all subsequent calls from detachable threads return `None` immediately without contacting the host.
  2. **Zero-CPU Wait:** In-flight calls are waited on via `Condvar` rather than busy-spinning.
  3. **Grace Period & Telemetry:** If an in-flight foreign host callback exceeds 50 ms, diagnostic `[E2130 | HOST_LOG_QUIESCENCE_STALLED]` or `[E2131 | HOST_CALL_QUIESCENCE_STALLED]` is emitted once, outside the coordinator mutex.
  4. **Quarantine Teardown Policy (no free while the host may be active):** Memory safety wins over resource liberation. After the deadline the diagnostic is emitted, but the wait remains until `in_flight == 0`; plugin resources (host handle, sinks, shared state) are **not** freed, dropped or reused while a host call may still be executing. If the host never returns, the resources stay quarantined with the blocked thread instead of being released under a possible Use-After-Free. A poisoned coordinator mutex is recovered via `into_inner`, `in_flight` is still decremented by the RAII guard, and the poisoning is reported — it can neither leave the count stuck nor spin forever without a diagnostic.
- **Teardown Execution Sequence (`NamClapMainThread::drop`):**
  1. Emit remaining telemetry logs.
  2. Unregister instance sink from `NamLogger`.
  3. Quiesce `HostLogQuiescence` (50 ms grace period, wait for `in_flight == 0`).
  4. Clear host log sink slot.
  5. Unregister periodic watchdog timer.
  6. Lower `alive_fence` (`store(false, Ordering::Release)`).
  7. Quiesce `HostCallQuiescence` (50 ms grace period, wait for in-flight calls to complete).
  8. Synchronous GUI teardown (`teardown_gui_resources`): close window, join workers or hand off to reaper.
  9. Final GC drain of SPSC and overflow rings.
- **Validation Status & GAP-HOST-LOG-TEARDOWN-BOUND:** The coordinators' thread-safety, immediate closure, clean completion and the blocked-host path are tested in unit tests (`src/clap/plugin/quiescence.rs`) and product integration tests (`tests/clap_quiescence_test.rs`), which prove with explicit synchronization that a real in-flight host call finished before `destroy()` returned and that subsequent calls are rejected. Because memory safety strictly precludes returning — and precludes freeing — while a foreign host callback is executing, a buggy or deadlocked host callback can delay teardown indefinitely and leaves the plugin resources quarantined. This trade-off is formally documented as an explicit GAP (GAP-HOST-LOG-TEARDOWN-BOUND).

---

## 6. DAW Audio Processing & DSP Pipeline

The audio thread entry point `PluginAudioProcessor::process()` (`src/clap/processor/mod.rs`) executes the signal chain:

```mermaid
graph TD
    HostInput[/"Host Input Buffer"/] --> DryDelay["DryDelayLine\n(circular ring, L/R)"]
    DryDelay --> BypassCheck{"Bypass Active?"}
    BypassCheck -->|"Yes"| RunBypass["copy_delayed_dry_to_output()\n(Latency-compensated passthrough)"]
    BypassCheck -->|"No"| ChanExt["extract_channels()\n(Mono / Adaptive Stereo)"]

    ChanExt --> InGain["Input Gain\n(SIMD + ParamSmoother)"]
    InGain --> InStage["apply_input_stage()\n(Dither & Gate FSM)"]

    InStage --> GateCheck{"Gate Open?"}
    GateCheck -->|"No"| ZeroBuf["Fill Scratch Buffer with 0.0"]
    GateCheck -->|"Yes"| InferEngine["run_inference()\n- NamResampler (Up to 48kHz)\n- NamModel::process()\n- NamResampler (Down to Host SR)"]

    ZeroBuf --> OutStage
    InferEngine --> OutStage["apply_output_stage()\n(Dither Comp & Gate Fade\n& Adaptive Compute Check)"]

    OutStage --> CabCheck{"Cabsim IR Loaded?"}
    CabCheck -->|"Yes"| CabConv["UPOLS Cabsim Convolution\n(ConvEngine::process)"]
    CabCheck -->|"No"| OutGain
    CabConv --> OutGain["Output Gain + Hard Clipping\n(SIMD + ParamSmoother)"]

    OutGain --> PeakStore["store_peaks()\n(Write to RtToUi Atomics)"]
    PeakStore --> HostOutput[/"Host Output Buffer"/]
```

### 6.0 Processor Subsystem Architecture (`src/clap/processor/`)

To ensure strict separation of concerns, the real-time audio processor lifecycle is decomposed into dedicated, cohesive submodules:

| Submodule               | Role & Responsibilities                                                                                                                                                                   |
|:----------------------- |:----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **`activate.rs`**       | Off-RT buffer pre-allocation (`AlignedVec`), SPSC channel extraction, staged restore/swap adaptation (`StagedItemsRollbackGuard`), DSP component assembly, and initial latency reporting. |
| **`deactivated.rs`**    | `DeactivatedDspState` definition and `deactivate()` lifecycle implementation, preserving reusable heavy DSP assets off the audio thread.                                                  |
| **`ftz.rs`**            | RAII `FtzDazGuard` asserting hardware FTZ/DAZ denormal flushing during block rendering and restoring host MXCSR on drop with zero leakage.                                                |
| **`mod.rs`**            | CLAP `PluginAudioProcessor` trait implementation for `NamClapProcessor`: `start_processing()` thread setup, top-level callback (`process()`), reset logic, and panic containment.         |
| **`dsp/`**              | Audio signal chain (channel extraction, latency-compensated dry delay, noise gate FSM, neural inference, peak telemetry) and the `orchestrator/` event scheduler with sub-block slicing.  |
| **`events.rs`**         | SPSC command ring drainage (`RtSwapHandler` binding), host event handling, GUI parameter sync, and latency monitoring.                                                                    |
| **`gc.rs`**             | Allocation- and lock-free disposal of replaced resources (parking lot + overflow buffer), see §5.3.                                                                                       |
| **`heap_audit.rs`**     | `CountingAllocator` registration under the `heap-audit` feature.                                                                                                                          |
| **`params.rs`**         | Atomic parameter snapshotting and `ParamSmoother` integration.                                                                                                                            |
| **`prewarm_policy.rs`** | Proportional zero-budget stabilization policy and execution step sizing for two-phase model resets.                                                                                       |
| **`rollback.rs`**       | Panic-resilient resource recovery guard (`ActivateRollbackGuard`).                                                                                                                        |
| **`state.rs`**          | `NamClapProcessor` struct declaration and click-free `BypassCrossfader` state machine.                                                                                                    |

### 6.1 Execution Sequence

1. **Subnormal & Denormal Setup:** First block applies one-shot thread setup (`apply_rt_thread_setup()`), and every callback asserts Flush-To-Zero (FTZ) and Denormals-Are-Zero (DAZ) via RAII `FtzDazGuard` (`src/clap/processor/ftz.rs`), restoring host MXCSR upon exit.
2. **Event & Command Draining (`process_events()`):** Drains command and slimmable rings via `RtSwapDrain` schedulers (at most one structural swap per callback), then updates local parameter targets from GUI atomics.
3. **Channel Extraction (`extract_channels()`):** Maps host input buffers into contiguous working scratch buffers and detects active channel count (`1` or `2`) for adaptive metering.
4. **Input Gain & Stage:** Applies user input gain (SIMD + `ParamSmoother`), injects anti-subnormal dither (`-220 dBFS`), and evaluates the Noise Gate FSM.
5. **Neural Inference (`run_inference()`):**
   - Upsamples host sample rate to native 48 kHz via minimum-phase polyphase `NamResampler` (bypassed if host rate is 48 kHz).
   - Runs `NamModel::process()` (with optional 2x/4x oversampling around neural activations).
   - Downsamples 48 kHz back to host sample rate.
6. **Output Stage & Degradation Check:** Compensates dither, applies linear gate fade ramps, and checks block deadlines for `AdaptiveCompute` CPU fallback.
7. **Cabinet Stage (Optional Single Exclusive Slot):** Executes either Uniform-Partitioned Overlap-Save (UPOLS) frequency-domain convolution (`WavIr` via `ConvEngine::process()`) for `.wav` IRs **or** a dedicated 1-in/1-out neural cabinet model (`NamModel` dual-mono `model_l`/`model_r` with dedicated `StreamingResampleBuffer`, strict unity gain isolated from amp calibration and `IrLevelPolicy`, and fail-closed WaveNet post-stack `head` rejection scoped strictly to the cabinet slot). Independent `PARAM_CAB_BYPASS` (13) routes the cabinet stage through a latency-aligned `DryDelayLine`, 64-sample cosine crossfade, lazy `reset()` on re-engagement, and steady-state DSP skip while preserving constant host PDC.
8. **Output Gain & Clipping:** Applies output gain (SIMD + `ParamSmoother`), enforces hard clipping at `+0 dBFS` if configured, and updates peak telemetry atomics.

### 6.2 Branchless FMA Bypass Crossfader (`process_crossfade_sub_block`)

To prevent clicks, pops, or phase artifacts when toggling plugin bypass:

- **Branchless Scalar FMA Loop:** The inner blend loop executes across `n_xfade` samples without internal branching (`wet[i] = dry[i] + (wet[i] - dry[i]) * mix`) using scalar FMA instructions. Under strict IEEE-754 semantics (avoiding non-compliant fast-math), scalar recurrence guarantees deterministic transitions.
- **Mix Clamping:** Mix parameters are clamped to `[0.0, 1.0]` at each step, preventing saturation artifacts.

#### 6.2.1 Latency-Compensated Dry Path (`DryDelayLine`)

The wet DSP chain introduces algorithmic latency (`cached_effective_latency` = resampler delay + oversampling filter delay + CabSim partition delay). The dry path is latency-compensated to match this delay exactly:

- **Pre-allocated Circular Ring:** `DryDelayLine` (`src/clap/processor/dsp/dry_delay.rs`) wraps two engine `DelayLine<f32>` rings allocated once in `activate()`.
- **Single Dry Source:** `process_sub_block` feeds raw host input into the ring every sub-block. Both the bypass output (`copy_delayed_dry_to_output`) and the crossfade blend consume this compensated dry signal, preventing phase cancellation and comb filtering.
- **Seamless Host PDC:** In the bypassed state, the output is delayed by the exact declared latency, ensuring host Plugin Delay Compensation (PDC) remains aligned without temporal jumps.

### 6.3 Model Gain Calibration Isolation

NAM models may supply embedded metadata (`input_level_dbu`, `output_level_dbu`, `loudness`). Linear calibration multipliers are computed in `f64::powf` (avoiding `GainLUT` ±30 dB saturation across the full `[-60, +60]` dBu calibration range) and driven through dedicated per-processor `smoother_cal_in` and `smoother_cal_out` exponential smoothers into `DspPipelineContext` rather than multiplying user gain smoothers:

- `input_mult_adj`: when `calibrate_input` is enabled and `input_level_dbu` exists, `10^((input_calibration_level_dbu - input_level_dbu) / 20)` applied post-gate / pre-nonlinearity; otherwise `1.0` (the noise gate continues evaluating raw pre-calibration input).
- `output_mult_adj`: `Raw` -> `1.0`; `Normalized` -> `10^((-18.0 - loudness) / 20)` if `loudness` exists else `1.0`; `Calibrated` -> `10^((output_level_dbu - input_calibration_level_dbu) / 20)` if `output_level_dbu` exists else `1.0`.
- **State Migration (`StateEnvelope` v2):** Sessions persist `CalibrationState` (`calibrate_input`, `input_calibration_level_dbu`, `output_mode`), optional `gate_mode` (`Silence` / `Musical`), and optional `cab_bypass` (`bool`) alongside cabinet slot `.wav`/`.nam`/`.namb` identity (`ir_path` + `ir_hash`). Restoring `v0`/`v1` envelopes lacking `"calibration"` preserves upstream legacy playback semantics (`calibrate_input = true`, `12.0` dBu, `Normalized`), while missing `gate_mode` / `cab_bypass` default cleanly to `Silence` / `false`. In `Musical` mode, `MusicalNoiseGate` triggers post-user-trim / pre-calibration and applies gain post-model / pre-cabinet while keeping neural inference continuous without hard cutouts.

### 6.4 Host Event Scheduling, In-Block Slicing & Anomaly Containment

Host parameter automation and modulation events are processed in `src/clap/processor/dsp/orchestrator.rs`:

1. **Bounded Event Buffer (`MAX_SCHEDULED_EVENTS`):** Input events are parsed into a pre-allocated buffer of 4096 entries. Floods exceeding capacity are truncated, asserting `RT_STATUS_SPSC_DRAIN_TRUNCATED` for off-RT logging.
2. **Sample-Accurate Sub-Block Slicing:** The block is partitioned into consecutive slices bounded by distinct event timestamps (`sub_end = scheduled_events[event_idx].time`). Sliced audio is processed through the DSP chain, events are applied at slice boundaries, and smoothers resynchronize.
3. **Temporal Anomaly Policy:**
   - **Jittered Events (`time < block_offset`):** Clamped to `block_offset` and applied immediately before subsequent DSP processing.
   - **Out-of-Bounds Events (`time >= n_samples`):** Clamped to `n_samples` and applied at the block close boundary, guaranteeing parameter values are never lost.
   - **Telemetry:** Timing anomalies assert `RT_STATUS_EVENT_TIMING_ANOMALY` for off-RT diagnostic reporting via `log::warn!`.

### 6.5 Two-Phase Non-Finite Containment

Non-finite (NaN/Inf) host input samples poison recurrent neural network states. To contain poisoning without synchronous multi-millisecond resets:

1. **Detection:** Input stage detects non-finite samples and asserts `RT_STATUS_NON_FINITE_INPUT_DETECTED`.
2. **Fast Containment (O(1)):** At the start of the audio block, `NamModel::prewarm_reset()` restores model state without synchronous stabilization loops. Delay lines and parameter smoothers snap to target, clearing pre-fault history.
3. **Held Dry Block:** The faulted callback outputs latency-compensated dry audio (`process_dry_contained_block`) while the model reconverges over an amortized window, avoiding heavy blocking resets (1.8–4.2 ms) on the audio thread.

### 6.6 Transparent Oversize Block Slicing

When DAW hosts deliver audio buffers exceeding internal design limits (`n_samples > MAX_RESAMP_BUF`, where `MAX_RESAMP_BUF = 2048` frames):

1. **Cold Path Chunking:** `process_sub_block` branches via `cold_path()` to `process_sub_block_chunked`.
2. **Zero-Allocation Slicing:** Partitions the oversize block into successive slices of at most 2048 frames using pre-allocated scratch buffers (`SubBlockScratch`).
3. **DSP Continuity:** All stateful components (delay rings, crossfaders, smoothers, resamplers, CabSim partitions) advance continuously across slices without boundary clicks or state resets.

### 6.7 Dual-Mono & Stereo Processing Architecture

To support true dual-mono processing for stereo guitar tracks:

1. **Dual Independent Chains (`model_l` & `model_r`):** In Stereo mode (`CONFIG_ID_STEREO`), the loader builds two distinct model instances with identical weights and architectures. Both models maintain isolated recurrent/FIR histories, ensuring zero crosstalk between channels.
2. **Independent Dual-Mono CabSim (`CabSimPair`):** In Stereo mode with an active IR, convolution is processed via a stereo-decoupled `CabSimPair` (`l` and `r` adapters). Channel R is processed strictly along its own time axis and is never duplicated from Channel L.
3. **Zero-Cost Mono Mode:** In Mono mode (`CONFIG_ID_MONO`, default), only `model_l` and `cabsim_pair.l` are evaluated; the right channel remains in full bypass with zero DSP CPU consumption. No implicit downmixing or channel mixing is performed on incoming tracks.
4. **Anti-Collapse Invariant (`mono_epsilon`):** In standard mono processing, `apply_input_stage` computes channel divergence against `mono_epsilon` (`1e-4`) to collapse identical channels into a mono fast-path. In stereo dual-mono mode, `mono_epsilon` is clamped to `-1.0`. Because absolute difference `|L - R| >= 0.0 > -1.0`, the hysteresis detector never triggers, preventing independent guitar tracks from collapsing to mono during silent passages or identical musical intervals.
5. **Port Activation Gating:** Inactive input ports (`port_input_active == false`) zero the input buffers; inactive output ports (`port_output_active == false`) skip output slice writes entirely.

### 6.8 Two-Phase Reset & Prewarm Liveness Architecture

To reconcile CLAP synchronous reset requirements with hard real-time deadlines:

1. **Phase Zero (Synchronous `reset()`):** Calls `NamModel::prewarm_reset()` in-place for active models (`model_l` and `model_r`), clearing filter/delay-line histories, phases, FIFOs, and smoothers without heap allocations. Rate/buffer bookkeeping is preserved. For LSTM and WaveNet A1, phase-zero runtime is < 50 µs, fully satisfying the universal real-time callback budget (p99 ≤ 1.33 ms @ 48 kHz). For WaveNet A2 cascade, the frozen engine structure requires ~2.1 ms p50 for phase zero (~3.2 ms total mono / ~6.4 ms total stereo dual-mono), which inherently breaches the universal 1.33 ms RT budget. This is formally classified as an architectural GAP (GAP-RESET-A2-ENVELOPE), with documented risk of xruns during seek/reset on quantum ≤ 64 frames @ 48 kHz (or ≤ 128 @ 96 kHz). Relaxing the budget to manufacture a PASS is prohibited.
2. **Phase One (Proportional Prewarm Amortization):** The stabilization zero feed is amortized across subsequent audio callbacks using `compute_prewarm_step()`. The step scales dynamically with callback frame count and is strictly bounded by the callback period safety margin (75% budget minus overhead). In dual-mono mode (`chains = 2`), the sample step budget is scaled by active chains so that stepping both models never exceeds the real-time deadline.
3. **Liveness & Zero-Budget Starvation Fallback:** When host callbacks are too small to fit even a single sample without breaching RT deadlines (`available_ns < step_cost_ns`), prewarm work is deferred (`step = 0`) to preserve the audio deadline and output remains strictly on the latency-compensated dry path (`process_dry_contained_block`). If zero-budget callbacks persist, upon reaching `PREWARM_ZERO_BUDGET_STARVATION_LIMIT` (64 callbacks), the transition triggers a single host callback request (`host.request_callback()`) and increments `prewarm_starvation_epoch` telemetry. Subsequent zero-budget callbacks do NOT re-request host callbacks, preventing callback storms. Any elevated EWMA sample cost is cleared to retry against conservative table costs. When the host resumes standard quantum, prewarm converges deterministically and wet processing commences with bit-exact parity matching a fresh instance.

---

## 7. Graphical User Interface (Slint GUI Architecture)

The graphical interface is built using Slint (v1.18) under `src/clap/gui/`.

```text
┌─────────────────────────────────────────────────────────────────────────────┐
│                       NAM-Plug Slint GUI Architecture                       │
├─────────────────────────────────────────────────────────────────────────────┤
│  5-Zone UI Layout (main.slint)                                              │
│    Zone 1: Identity Bar (Brand, SIMD badge, ModelCard, IrCard)              │
│    Zone 2: Gain & Gate Stages (INPUT/OUTPUT/GATE knobs, Sil/Mus, Gate LED)  │
│            + Oversample (Off/2×/4×) and Precision (Std/Fast) selectors      │
│    Zone 3: Meters Section (Adaptive VuMeter: Mono / Stereo, clip LED)       │
│    Zone 4: Master Bypass (ToggleSwitch, "DSP ACTIVE" / "BYPASS")            │
│    Zone 5: Status Bar (SR, BUF, LAT, CH, DSP load, alert pill, backend)     │
├─────────────────────────────────────────────────────────────────────────────┤
│  Rendering Pipeline: Slint v1.18 ──► FemtoVG / OpenGL backend via winit     │
├─────────────────────────────────────────────────────────────────────────────┤
│  Windowing: X11 embedded (native XEmbed) + X11/Wayland floating fallback    │
│             Persistent per-instance GUI worker + bounded teardown reaper    │
├─────────────────────────────────────────────────────────────────────────────┤
│  Async File Picker: rfd (Background Worker File Dialog, Non-Blocking)       │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 7.1 Module Organization

- `src/clap/gui/mod.rs` — GUI entry point, default window dimensions (`600x275`), and `GuiHostBridge`.
- `src/clap/gui/worker.rs` — Persistent per-instance GUI worker thread (`GuiWorker`) owning the Slint platform and event loop.
- `src/clap/gui/x11_embed.rs` — X11 XEmbed embed-at-creation engine and backend selection hooks.
- `src/clap/gui/lifecycle.rs` — `GuiLifecycle` finite state machine for window visibility.
- `src/clap/gui/slint/main.slint` — Declarative 5-Zone UI layout (`MainWindow`).
- `src/clap/gui/slint/` — Modular Slint components (`RotaryKnob`, `VuMeter`, `ToggleSwitch`, `SelectorButton`, `LedIndicator`, `ModelCard`, `IrCard`).
- `src/clap/gui/slint_view_model.rs` — `SlintViewModel`: bridges Slint properties with plugin atomics, drives adaptive 60/30 Hz polling, and applies peak-meter ballistics.
- `src/clap/gui/file_dialogs.rs` — Asynchronous background file pickers using `rfd::AsyncFileDialog`.
- `src/clap/gui/dialog_state.rs` — Lock-free state synchronization for file dialogs.
- `src/bin/ui_preview.rs` — Standalone binary for rapid visual inspection without a DAW host.

### 7.2 Telemetry Synchronization & Peak-Meter Ballistics

- A Slint timer polls atomic telemetry from `NamClapShared::rt_to_ui` at 60 Hz (`TELEMETRY_PERIOD_FULL`), relaxing to 30 Hz (`TELEMETRY_PERIOD_LOW`) while the signal is below −90 dBFS and the UI is idle; the timer is suspended with the window.
- Peak levels follow IEC 60268-10-inspired peak-programme ballistics (constants in `slint_view_model.rs`):
  - **Attack:** Immediate (single-block peak capture).
  - **Decay:** Exponential release with a per-frame multiplier of `0.962` (`BALLISTICS_DECAY_RATE`, ≈ 20 dB in 1 s at 60 Hz).
  - **Peak-Hold Indicator:** 2.0-second retention (`PEAK_HOLD_FRAMES = 120`) before release.
  - **Clip LED:** Sticky until the user clicks the meter.
- **Adaptive Track Configuration:** Adapts automatically between a centered mono bar and independent stereo L/R bars based on `active_channel_count`.

#### 7.2.1 Identity and Status-Bar Readouts

- **`model_arch` (Live):** Displays loaded model architecture (e.g., WaveNet A1, LSTM) from `ui_model_metadata` in the `ModelCard`. Shows `"No model loaded"` when inactive.
- **`simd_badge` (Static, Zone 1):** Displays the runtime-detected instruction set (`"AVX2+FMA"`), set once per window creation.
- **`SR` / `BUF` / `LAT` / `CH` (Live, Zone 5):** Host sample rate, last block size, total latency as `"<N> spl (<ms> ms)"`, and active channel layout.
- **`dsp_load_text` (Live, Zone 5):** Formatted as `"{pct}% DSP"` from `rt_to_ui.dsp_load_permille`, with an LED that holds for 2 seconds after an overload (XRUN) and a hover hint.
- **`backend_text` (Static, Zone 5):** Displays the negotiated window backend (`"X11 (Embedded)"`, `"X11 (Floating)"`, `"Wayland (Floating)"`).

#### 7.2.2 Status Alert Banner

Operational warnings are derived off-RT from `ColdShared::ui_alert_bits` and displayed as status bar pills with tooltips:

- Captures processor poisoning, host contract violations, CabSim errors, resampler rebuild failures, dynamic CPU degradation, and non-finite input events (3-second hold).

### 7.3 Persistent GUI Worker & Bounded Teardown (`nam-gui-reaper`)

**Thread-Pinning Rationale:** Slint 1.18 binds its platform, event loop, and window hooks to the thread that first initializes it. Creating a new thread per window open/close cycle causes `SetPlatformError`. NAM-Plug therefore runs **one** persistent `"nam-slint-gui"` thread per plugin instance.

**Teardown Protocol:**

1. The host invokes `gui.destroy()` or the user closes the window.
2. The plugin closes the window (`slint::quit_event_loop()`), while the worker thread remains alive for subsequent `create()` calls.
3. The worker releases UI components and executes one drained loop iteration to ensure winit's queued `XDestroyWindow` reaches the X server, preventing orphan windows.
4. Upon **instance destruction**, the worker is joined with a bounded 2000 ms timeout. If the window backend delays release, the handle is handed off to a detached background thread (`"nam-gui-reaper"`), ensuring the DAW main thread is never blocked.

### 7.4 Asynchronous Model Loader (`nam-model-loader`)

Model loading is divided into two phases:

1. **Prepare Phase (Off-RT):** File reading, JSON parsing, SHA-256 calculation, and model/resampler instantiation execute on a dedicated background thread (`"nam-model-loader"`).
2. **Commit Phase (Main Thread):** The completed result is swapped atomically on the main thread and pushed to the audio thread via SPSC command queues.

- **Adaptive Structural Rebuild Offload:** The model loader also offloads allocation-heavy tasks for the audio thread, including WaveNet slimmable slices and oversampling engine rebuilds. The main thread's housekeeping watchdog accelerates from 250 ms to 50 ms while a rebuild is pending.

### 7.5 GUI Lifecycle State Machine

Window visibility is governed by `GuiLifecycle` (`Hidden → ShowRequested → Active → HideRequested → Hidden`):

- **Host-driven:** `create()`, `show()`, and `hide()` initiate state transitions on the main thread.
- **Backend-driven:** The GUI worker notifies the main thread upon window readiness (`WindowReady`) or user closure (`gui_user_closed`), advancing the state machine off-RT.

### 7.6 X11 Embedding Architecture

NAM-Plug supports native X11 window embedding (XEmbed) under Linux using Slint's `with_winit_window_attributes_hook` (`unstable-winit-030` feature).

**Architectural Decision: Embed-at-Creation:**

- **Mechanism:** Before creating the window, `BackendSelector` registers a hook that applies `.with_embed_parent_window(parent_id)`. Winit creates the window as a direct child of the host window and sets the `_XEMBED` property at creation time.
- **Alternative Rationale:**
  - *Reparent-after-creation (Rejected):* Reparenting after mapping causes race conditions with Window Managers and requires an external X11 connection.
  - *Custom WindowAdapter (Rejected):* Re-implements complex window management already handled by Slint.
- **Parent ID Passing:** The host window ID is passed by value (`u32`) through an atomic variable (`EMBED_TARGET`), preventing raw pointers from escaping to the GUI thread.
- **Viability Gate & Honest Fallback:** The plugin checks display reachability at `create()`. If X11 embedding is unavailable (e.g., pure Wayland without XWayland), `create()` rejects the embedded request cleanly (`CLAP_WINDOW_API_WAYLAND` with `is_floating = false` is rejected per CLAP specification). Conforming hosts may negotiate a floating window (`is_floating = true`), or fall back to their native generic parameter editor if floating is not supported or requested by the host (as observed in Fender Studio Pro 8.2).

---

## 8. Error Catalog Summary (`NamErrorCode`)

NAM-Plug utilizes typed diagnostic codes (`NamErrorCode` in `neural_amp_modeler_rs::common::diagnostics`) for structured logging and UI error toasts:

| Range   | Category            | Representative Examples                                                                                                   |
|:------- |:------------------- |:------------------------------------------------------------------------------------------------------------------------- |
| `E1xxx` | Model Loading & I/O | `E1100` FILE_NOT_FOUND, `E1200` NAM_JSON_PARSE_ERROR, `E1201` NAMB_CRC32_MISMATCH, `E1300` UNSUPPORTED_ARCHITECTURE       |
| `E2xxx` | Audio & Real-Time   | `E2001` DEADLINE_EXCEEDED, `E2130` HOST_LOG_QUIESCENCE_STALLED, `E2200` RESAMPLER_BUILD_FAILED, `E2300` SCHED_FIFO_DENIED |
| `E3xxx` | SPSC / Lock-Free GC | `E3100` PARAM_CHANNEL_FULL, `E3101` GC_OVERFLOW, `E3102` GC_CORRUPTED                                                     |
| `E4xxx` | Runtime & CLI       | `E4100` INVALID_GAIN_VALUE, `E4103` IR_LOAD_FAILED                                                                        |
| `E5xxx` | System Resources    | `E5000` OUT_OF_MEMORY                                                                                                     |

---

## 9. Test Infrastructure & Contract Validation

Validation is implemented across dedicated testing and certification layers:

### 9.1 Host Harness (`src/clap/host_harness.rs`)

Simulated DAW host environment within library unit tests:

- Implements the host-side CLAP extensions the plugin queries (`thread-check`, `log`, `latency`, `tail`, `params`, `preset-load`, and optionally `timer`).
- Provides helpers (`make_test_plugin_with_harness()`, `process_block_harness()`) to test activation cycles, parameter automation, and state migration.

### 9.2 Dynamic Artifact Validator (`tests/clap/artifact_validator.rs`)

Dynamic-links against the compiled `.so` binary using `PluginEntry::load(&artifact.path)` to assert ABI symbol compliance and verify release binary hashes.

### 9.3 SIMD Segregation & Hardware Baseline (x86-64-v3)

Adherence to the baseline `x86-64-v3` (AVX2/FMA/BMI2) architecture is contractual:

- Compiles without enabling the engine's opt-in `avx512` feature, ensuring zero EVEX instructions in `.text`.
- Verified across compile-time feature matrices in `utils/lints.sh`.

#### 9.3.1 Entry-Time CPU Feature Validation Gate

To protect legacy CPUs from crashing with `SIGILL` upon plugin load:

- `NamEntry::new()` calls `check_cpu_requirements()` during `clap_entry.init` before allocating plugin resources.
- Verifies `avx2`, `fma`, and `bmi2` via CPUID (`is_x86_feature_detected!`).
- If features are missing, logs an error to `stderr` and returns `false` cleanly, allowing the host to handle hardware incompatibility gracefully.
- Dynamic initialization sections (`.init_array`) contain only baseline `x86-64-v1` instructions, ensuring execution reaches the validation gate safely.

### 9.4 Real-Time Heap Allocation Audit

Enforces zero heap allocation on the audio thread:

- Global memory interceptor (`CountingAllocator`) active under `--features "testing,heap-audit"`.
- Static AST scanner (`utils/verify_no_rt_alloc.sh`) asserting zero `Box`, `Vec`, or `format!` calls in `src/clap/processor/`.
- Codegen verifier (`utils/verify_rt_codegen.sh`) disassembling the `.so` with `objdump` to verify zero `malloc`/`free`, zero `div`/`idiv` in hot loops, and zero `__tls_get_addr` calls.

### 9.5 Headless GUI Testing (Xvfb)

Floating window lifecycles and clipboard integration are validated under a virtual X11 display (`Xvfb :99`) with Mesa software rendering (`llvmpipe`).

### 9.6 E2E CLAP vs. NAMCore Parity & CabSim IR Test

- **NAMCore Parity (`tests/clap/clap_parity_multi_sr.rs`):** Loads `.so`, loads models via CLAP state, and compares processed output against the reference C++ NAMCore oracle (`ESR < 1e-8`, `SNR > 80 dB`).
- **CabSim IR Test (`tests/clap/clap_cabsim_ir.rs`):** Dynamically loads release `.so` to verify that impulse responses transform audio output accurately.

### 9.7 Performance Certification Gate (`src/bin/nam_perf_guard.rs`)

`nam_perf_guard certify --clap <artifact>` measures statistical latency distributions (p50, p95, p99, ns/sample) of the shipped `.clap` across core topologies (WaveNet A1, WaveNet A2 Slimmable, LSTM) and block sizes (64 and 128 samples), validating real-time deadline margins (p99 < 85% of the block budget) and regressions against a paired reference series. See [`docs/testing.md` §4.3](./testing.md#43-performance-certification-gate).

#### 9.7.1 A/B Optimization Ceremony & Promotion Fallback Policy

`NAM-Plug` requires empirical proof via `utils/ab-opt-ceremony.sh` (`plug_ab_opt compare`) before promoting PGO or LLVM-BOLT in `utils/build-release.sh`. When hardware LBR is unavailable (e.g., AMD Zen `bolt_lbr_unsupported`) or when PGO/BOLT does not beat the unoptimized release baseline by $\ge 1.0\%$ across all three canonical topologies (`wavenet_a1`, `wavenet_a2`, `lstm` — e.g., hand-tuned AVX2/FMA kernels already saturating execution ports with `verdict: "UNOPTIMIZED"`, `verdict_reason: "pgo_margin_not_met"`), `build-release.sh` automatically promotes the deterministic plain release artifact (`bolt_status: "not_proven"`, `bolt_proof: "fallback_plain"`, `pgo_status: "skipped_unproven"`), recording `"unproven_claims": ["pgo_win", "bolt_win"]` in `build-receipt.json` and `release-receipt.json`. See [`docs/testing.md` §5.4.2](./testing.md#542-ab-optimization-ceremony--llvm-bolt-promotion-policy).

---

## 10. Flatpak Packaging & CLAP Discovery Architecture

NAM-Plug supports distribution as a Flatpak audio plugin extension, enabling integration with containerized DAWs (Bitwig Studio, REAPER, Ardour Flatpaks) without host filesystem sandbox holes (`--filesystem=host`):

### 10.1 Freedesktop LinuxAudio Extension Topology

Freedesktop Flatpak audio applications utilize a standardized extension point (`org.freedesktop.LinuxAudio.Plugins`):

```text
┌─────────────────────────────────────────────────────────────────────────────┐
│                           Flatpak DAW Container                             │
│                  (e.g., com.bitwig.BitwigStudio)                            │
├─────────────────────────────────────────────────────────────────────────────┤
│  Mount Point: /app/extensions/Plugins/                                      │
│    └── clap/                                                                │
│         └── nam_plug.clap ◄── Mounted dynamically from host runtime         │
├─────────────────────────────────────────────────────────────────────────────┤
│  AppStream Catalog: /app/extensions/Plugins/share/metainfo/                 │
│    └── org.freedesktop.LinuxAudio.Plugins.NAMPlug.metainfo.xml              │
└─────────────────────────────────────────────────────────────────────────────┘
                               ▲
                               │ (Flatpak runtime extension bind mount)
┌──────────────────────────────┴──────────────────────────────────────────────┐
│                    Host User Extension Storage (~/.local/share/flatpak)     │
│  runtime/org.freedesktop.LinuxAudio.Plugins.NAMPlug/x86_64/26.08/active/    │
│    └── files/clap/nam_plug.clap                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

- **Runtime Extension Ref:** `runtime/org.freedesktop.LinuxAudio.Plugins.NAMPlug/x86_64/26.08`
- **Base Extension:** `org.freedesktop.LinuxAudio.BaseExtension`
- **Container Mount Path:** `/app/extensions/Plugins/clap/nam_plug.clap`
- **Extension Priority:** `--extension-priority=100` during `flatpak build-finish`.

### 10.2 AppStream Addon Metadata

The metadata descriptor (`packaging/flatpak/org.freedesktop.LinuxAudio.Plugins.NAMPlug.metainfo.xml`) declares `<component type="addon">` and establishes discovery links with supported DAWs:

```xml
<component type="addon">
  <id>org.freedesktop.LinuxAudio.Plugins.NAMPlug</id>
  <extends>org.freedesktop.LinuxAudio.BaseExtension</extends>
  <extends>com.bitwig.BitwigStudio</extends>
  <extends>fm.reaper.Reaper</extends>
  <extends>org.ardour.Ardour</extends>
  <extends>com.fender.studioapp8</extends>
  <name>NAM Plug</name>
  <summary>Neural Amp Modeler CLAP audio plugin</summary>
</component>
```

### 10.3 Manifest Specification

The Flatpak manifest (`packaging/flatpak/org.freedesktop.LinuxAudio.Plugins.NAMPlug.yml`) targets the `26.08` runtime branch:

```yaml
id: org.freedesktop.LinuxAudio.Plugins.NAMPlug
branch: "26.08"
runtime: org.freedesktop.LinuxAudio.BaseExtension
runtime-version: stable
sdk: org.freedesktop.Sdk//26.08
build-extension: true

build-options:
  prefix: /app/extensions/Plugins/NAMPlug

modules:
  - name: nam-plug
    buildsystem: simple
    build-commands:
      - install -Dm755 nam_plug.clap ${FLATPAK_DEST}/clap/nam_plug.clap
      - install -Dm644 org.freedesktop.LinuxAudio.Plugins.NAMPlug.metainfo.xml -t ${FLATPAK_DEST}/share/metainfo/
```

### 10.4 Integrated Release Pipeline (`build-release.sh`)

Flatpak bundle creation is integrated into Phase 7 of `utils/build-release.sh` with zero-recompile **multi-stack** bundling across `26.08` and `25.08`:

1. **Initialize:** Runs `flatpak build-init --type=extension --extension-tag=<branch>` for each branch.
2. **Install:** Reuses the single optimized host binary `nam_plug.clap` into `files/clap/` and AppStream XML into `files/share/metainfo/`.
3. **Finish:** Runs `flatpak build-finish --extension-priority=100`.
4. **Export OSTree:** Executes `flatpak build-export --update-appstream`.
5. **Create Bundles:** Generates the canonical bundle `target/dist/nam-plug-<VERSION>-linux-x86_64.flatpak` (branch 26.08) and the compatibility bundle `target/dist/nam-plug-<VERSION>-linux-x86_64-25.08.flatpak`.
6. **Deploy:** Automatically deploys both branches when `--install` is supplied.

### 10.5 AppStream Catalog Dynamics & Bundle Semantics

1. **Local Bundles vs. Remote Repositories (Flathub):**
   - Standalone `.flatpak` bundles encapsulate only the commit objects for the plugin ref. They do not contain repository-wide AppStream catalog branches (`appstream/x86_64`).
   - Consequently, local bundle installations show generic icons and fallback names in graphical software centers (e.g., Warehouse).
   - In production (Flathub), `flatpak build-update-repo` indexes the metainfo XML into the centralized AppStream branch, displaying full titles, release notes, and icons.
2. **Semantic Versioning in `flatpak list`:**
   - The `Versão` (Version) column in `flatpak list` is populated from the cached AppStream catalog. When installed from a standalone bundle without catalog indexing, this column remains blank while the `Ramo` (Branch) column displays `26.08`.
3. **Sandbox Permissions:**
   - Plugin extensions are shared libraries mounted into host containers, not standalone sandboxed applications. They declare zero independent sandbox permissions (`--filesystem`, `--device`) and inherit the security boundary of the host DAW. Running `flatpak info --show-permissions` legitimately displays zero lines.

### 10.6 Developer Commands

```bash
# 1. Build canonical release plugin and Flatpak bundle, then audit and certify/install
./utils/build-release.sh --strict --package
./utils/tests-long.sh --strict --artifact
./utils/build-release.sh --strict --certify --install

# 2. Validate AppStream metadata
appstreamcli validate packaging/flatpak/org.freedesktop.LinuxAudio.Plugins.NAMPlug.metainfo.xml

# 3. Query installed extension metadata
flatpak info org.freedesktop.LinuxAudio.Plugins.NAMPlug

# 4. Verify mounting inside host Flatpak DAW (e.g., Bitwig Studio)
flatpak run --command=ls com.bitwig.BitwigStudio -la /app/extensions/Plugins/clap/

# 5. Remove installed extension
flatpak uninstall --user org.freedesktop.LinuxAudio.Plugins.NAMPlug
```

---

## 11. References

- [`NeuralAmpModeler-rs`](https://github.com/fabiohl/NeuralAmpModeler-rs) — Neural Amp Modeler DSP engine library.
- [CLAP (CLever Audio Plug-in) Specification](https://cleveraudio.org/) — Official CLAP plugin specification.
- [Clack Framework](https://github.com/prokopyl/clack) — Safe Rust bindings for CLAP plugins and hosts.
- [NeuralAmpModelerCore](https://github.com/sdatkinson/NeuralAmpModelerCore) — Reference C++ implementation of NAM.
