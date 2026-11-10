<!--
SPDX-License-Identifier: GPL-3.0-or-later
Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
-->

# Changelog

All notable changes to the **NAM-Plug** project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Control begins formally with version 1.0.0.

---

## [Unreleased]

## [1.1.0] - 2026-11-10

### Added [1.1.0]

- **Release Receipts & Certified Artifact Identity Chain:** Rust-based long-phase receipts (`plug_long_receipt`), golden quick-receipt validation, and fail-closed contract tests enforcing cryptographic hash parity across the distribution artifact chain (`F-CER-01`).
- **Adaptive Worker Polling & Alert Standardization:** Integrated adaptive polling and standardized host log alert transitions in main-thread housekeeping.
- **Proportional Reset Stabilization:** Proportional zero-budget stabilization policy for model resets, preserving real-time execution deadlines and numerical parity.
- **CI Provenance Tracking:** Independent lint and quick workflows with deterministic engine provenance logging (`Cargo.lock` and `variables.env`) and always-uploaded gate evidence.

### Changed [1.1.0]

- **Processor Lifecycle Decomposition (`src/clap/processor/`):**
  - Decomposed monolithic processor lifecycle into dedicated submodules (`activate.rs` for off-RT buffer pre-allocation and DSP component assembly; `deactivated.rs` for `DeactivatedDspState` resource preservation and zero-allocation re-activation; `ftz.rs` for RAII-scoped `FtzDazGuard` MXCSR management).
  - Fully isolated off-RT allocation and lifecycle stages from the per-block audio processing callback in `mod.rs`.
- **Asynchronous Model Loader Integration:** Integrated asynchronous background model loading (`nam-model-loader`) and offloaded heavy WaveNet slimmable slice and oversampler rebuilds from the audio thread.
- **Host Compatibility & Windowing Documentation:**
  - Updated documentation across `README.md`, `docs/architecture.md`, and `docs/functional-tests.md` reflecting empirical Linux host compatibility.
  - Documented native X11 XEmbed integration (verified in Bitwig Studio and REAPER) and generic host controls behavior under native Wayland hosts requesting unsupported embedded configurations (such as Fender Studio Pro 8.2).
  - Classified Wayland floating window support as experimental due to absence of verified test hosts negotiating floating windows under Wayland.
  - Added dedicated operational guidance on running without a graphical interface (parameter access, state persistence via `~/.nam/models`, and Flatpak sandbox boundaries).
- **Cleaned Main Thread APIs:** Streamlined model data builders, removed obsolete experimental asynchronous state-restore flags, and updated lint and quick disciplines with typed status carriers (`_status.tsv`).

### Fixed [1.1.0]

- **Performance Certification Harness (`nam_perf_guard` & `tests-long.sh`):**
  - `utils/tests-long.sh` automatically bootstraps the paired baseline series (`target/logs/perf-reference.json`) for the current `nam_plug.clap` SHA-256 when absent or stale, resolving certification bottlenecks on clean release builds.
  - `nam_perf_guard` applies in-process real-time hardening (`mlockall`, 64 KiB stack prefaulting, and `SCHED_FIFO` priority 80 promotion), pre-allocates and prefaults latency buffers before timing, checks kernel `effective_affinity_list` (with fallback to `smp_affinity_list`) when detecting shared IRQs on `isolcpus=managed_irq` hosts, and records `cpufreq/boost` as diagnostic provenance.
  - Pre-allocated and prefaulted timing vectors in `processor_command_budget_test` and `processor_drain_latency_test` to eliminate page-fault jitter during `rt-heap` micro-latency certification.
- **Long Audit & Ceremony Script Hardening:**
  - `utils/tests-long.sh` conforms to the Class B decoupled audit contract (`set -o pipefail`), derives `tests=` counts automatically for `catch-all`, and executes `catch-all`, `artifact`, and `perf` consecutively before `gui` (`--features testing,gui-x11`) to eliminate redundant ~3 min release recompilations.
  - `utils/ab-opt-ceremony.sh` sources `variables.env` before emitting provenance and ceremony receipts, ensuring `engine_commit` records the pinned engine SHA.
- **Activation Sample Rate Clamping:** Host sample rate in `activate()` is bounded to `[4_000, 384_000]` Hz so ultra-high-frequency test harnesses (such as `clap-validator` at 768 kHz) activate cleanly without panic.
- **Dual-Mono Teardown Accounting:** GC stress verification accounts for all 98 in-flight items of the dual-mono layout (32 SPSC + 16 parking lot + 50 overflow) drained off the audio thread.
- **State Transaction Quantum:** Passed buffer size through state transactions and aligned CabSim tests with the engine's constant partition-sized priming latency.
- **Release Packaging Integrity:** Included `LICENSE.txt` and engine `NOTICE` in tarball and Flatpak distributions, installed verified artifacts directly to `~/.clap`, and ensured Flatpak installation errors are caught fail-closed.

---

## [1.0.0] - 2026-10-22

### Added [1.0.0]

#### CLAP 1.2+ Audio Plugin Integration

- **Native CLAP Specification**: Full implementation of the CLAP (CLever Audio Plug-in) standard built on the `clack` framework (`clack-plugin`, `clack-extensions`), eliminating bridging overhead and running natively inside Linux DAWs.
- **CLAP Extensions Compliance**: Supported host extensions include `audio-ports`, `audio-ports-activation`, `params`, `state`, `state-context`, `latency`, `gui`, `track-info`, `remote-controls`, `param-indication`, `preset-discovery`, `render`, `tail`, `timer`, and `log`.
- **Sample-Accurate Parameter Automation**: Continuous parameter automation with sub-block slicing and smoothing (`ParamSmoother`), maintaining sample-accurate response for gain staging, noise gate, and model parameters.
- **Dynamic Plugin Delay Compensation (PDC)**: Cold-path cached effective latency reporting via `clap_plugin_latency`, notifying the host (`HostLatency::changed()`) immediately upon oversampling factor or CabSim changes without per-block audio thread overhead.
- **Bit-Transparent Bypass**: Latency-compensated bypass mode featuring a 64-sample click-free linear crossfader with phase compensation.
- **State Persistence & Asset Identity**: Full project state save/load serialization with mandatory cryptographic SHA-256 asset hashing (`model_hash`, `ir_hash`), ensuring reliable cross-machine project restoration and preset portability.

#### Core Neural DSP Engine (`NeuralAmpModeler-rs`) [1.0.0]

- **Extensive Topology Support**: Native execution of WaveNet (A1 and A2 standard and slimmable profiles), LSTM (1-layer and 2-layer configurations, multi-I/O), ConvNet, Linear FIR/FFT, and Sequential compositions.
- **Dual-Oracle Parity Validation**: Formally certified against canonical C++ NAMCore ($f32$) and double-precision ($f64$) oracles across 52 model baselines (WaveNet Standard ESR $< 2.31 \times 10^{-14}$, SNR $> 136.4\text{ dB}$; ConvNet ESR $< 9.33 \times 10^{-16}$, SNR $> 150.3\text{ dB}$).
- **x86-64-v3 SIMD Acceleration**: Mandatory AVX2/FMA/BMI2 baseline vectorization using hand-crafted SIMD kernels, tap-major memory layouts, and static `dispatch_simd!` monomorphization (zero vtables, zero function pointers).
- **Const-Generic Optimization**: 24 model variants (16 static compile-time monomorphized channel profiles for canonical geometries $CH=16, 12, 8, 4, 3$ + dynamic zero-allocation fallbacks).
- **Native Minimum-Phase Polyphase Sinc Resampler**: Built-in 256 phases × 64 taps Kaiser sinc resampler (Kaiser $\beta=12$, $>105\text{ dB}$ stopband attenuation, zero pre-ringing, $0.7\text{–}1.3\text{ µs}$) adapting 48 kHz models to 44.1 kHz, 96 kHz, and 192 kHz host sample rates.
- **Cabinet IR Convolution (UPOLS)**: Uniform-Partitioned Overlap-Save FFT convolution engine processing speaker cabinet impulse responses (`.wav`) in $1.3\text{ µs}$ for 512-sample IRs.
- **Half-Band Anti-Aliasing Oversampling**: Multi-stage polyphase FIR oversampling (`Off`, `2x`, `4x`, $>100\text{ dB}$ stopband attenuation) centered around the neural inference stage.
- **Activation Math Modes**: Selectable `Standard` (exact Taylor minimax, +89.5 dB SNR) and `Fast` (Padé polynomial approximations) modes.
- **Binary `.namb` v2 Container**: Support for pre-transposed binary model containers (Gate-Major LSTM, Interleaved-4 WaveNet) with IEEE 802.3 CRC32 integrity validation (<1 ms hot-swap time).

#### Strict Real-Time Safety & Threading [1.0.0]

- **Hard RT Audio Contract**: Guaranteed zero heap allocations, zero heap deallocations, zero mutex locks, and zero blocking syscalls on the audio thread (`process()`), audited dynamically via `CountingAllocator`.
- **3-Tier Lock-Free GC Cascade**: Dropped models, IRs, and oversamplers traverse lock-free SPSC queues (32 slots) → processor parking lot (16 slots) → atomic overwrite ring for deterministic off-RT disposal.
- **Fault Containment & Poisoning Recovery**: Real-time exception and panic containment latching `poisoned = true`, immediately zeroing output buffers in O(1) without leaks, and signaling autonomous off-RT host restart.
- **Denormal & Subnormal Armor**: Per-block hardware MXCSR FTZ/DAZ reassertion combined with symmetric $-220\text{ dBFS}$ deterministic dither injection, preventing CPU microcode stalls on digital silence.

#### Hardware-Accelerated Declarative GUI (Slint 1.18) [1.0.0]

- **Native X11 XEmbed Embedding**: Seamlessly embeds the plugin GUI inside host plugin panels (`CLAP_WINDOW_API_X11`, embedded) in Bitwig Studio and REAPER.
- **Wayland & X11 Floating Fallbacks**: Top-level floating window fallbacks for hosts requesting floating GUI surfaces (`CLAP_WINDOW_API_WAYLAND` / `CLAP_WINDOW_API_X11`).
- **Hardware-Accelerated Rendering**: Slint 1.18 UI rendered via FemtoVG and OpenGL (`winit` backend) on a dedicated `"nam-slint-gui"` event loop thread.
- **5-Zone Layout**:
  - Zone 1: Asset management (Model and CabSim IR loaders, status display, clear IR).
  - Zone 2: Gain staging and noise gate (Input/Output gain knobs from $-20\text{ dB}$ to $+20\text{ dB}$, noise gate from $-90\text{ dB}$ to $-40\text{ dB}$, off by default at $-90\text{ dB}$).
  - Zone 3: Quality modes (Oversampling `Off`/`2x`/`4x` and Activation `Standard`/`Fast`).
  - Zone 4: Tricolor adaptive VU meters (dynamic Mono / Stereo L/R layout with IEC 60268-10 PPM ballistics, peak-hold, and clip LEDs).
  - Zone 5: Real-time telemetry footer reporting host sample rate, buffer latency, DSP CPU load %, cycle counts, quantum size, RT thread priority, and diagnostic status flags.
- **Accessibility & Interaction**: Full keyboard navigation (Tab/Shift-Tab focus, arrow key parameter adjustments), double-click reset to default values, Ctrl-drag fine tuning, and drag-and-drop `.nam` model file loading.

#### Distribution, Packaging & Tooling [1.0.0]

- **Flatpak Plugin Extension**: Standalone Flatpak bundle (`org.freedesktop.LinuxAudio.Plugins.NAMPlug`) targeting `org.freedesktop.LinuxAudio.BaseExtension` 25.08, providing native access to sandboxed Flatpak DAWs (Bitwig, REAPER) without filesystem sandbox holes.
- **Mega-Optimized Release Pipeline (`utils/build-release.sh`)**: 5-phase optimization workflow combining Profile-Guided Optimization (PGO) on neural workloads, LLVM BOLT machine code layout optimization, and real-time deadline certification (`nam_perf_guard`).
- **Linker-Level Symbol Isolation**: Scoped version script (`hide-libm-shadow.map`) ensuring libm symbols resolve dynamically to `glibc` without dangerous PLT/GOT self-referential loops in release builds.
- **Automated QA & Verification Suite**: Multi-tier test scripts (`lints.sh`, `tests-quick.sh`, `tests-long.sh`, `tests-gui.sh`, `verify_rt_codegen.sh`, `tests-performance-regression.sh`) validating static RT safety, machine code codegen, property-based tests, and multi-rate C++ oracle parity.

[Unreleased]: https://github.com/fabiohl/NAM-Plug/compare/v1.1.0...HEAD
[1.1.0]: https://github.com/fabiohl/NAM-Plug/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/fabiohl/NAM-Plug/releases/tag/v1.0.0
