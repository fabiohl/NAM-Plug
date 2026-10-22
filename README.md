<!--
SPDX-License-Identifier: GPL-3.0-or-later
Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
-->

# NAM-Plug

![License](https://img.shields.io/badge/License-GPL--3.0--or--later-blue.svg) ![Rust](https://img.shields.io/badge/Rust-1.99%2B-orange.svg) ![Format](https://img.shields.io/badge/Format-CLAP%201.2%2B-brightgreen.svg) ![GUI](https://img.shields.io/badge/GUI-Slint%201.18%20%7C%20FemtoVG%20%7C%20X11%20Embedded%20%7C%20Wayland-blueviolet.svg) ![Latency](https://img.shields.io/badge/Latency-Zero--Added%20%2F%20Sub--ms-red.svg) ![RT-Safe](https://img.shields.io/badge/RT--Safe-Zero--Alloc%20%7C%20Zero--Locks-brightgreen.svg) ![SIMD](https://img.shields.io/badge/SIMD-AVX2%20x86--64--v3-blueviolet.svg) ![Models](https://img.shields.io/badge/Models-WaveNet%20A1%20A2%20%7C%20LSTM%20%7C%20ConvNet-success.svg) ![Parity](https://img.shields.io/badge/Parity-Dual--Oracle%20%3E136dB%20SNR-success.svg)

**NAM-Plug** is a high-performance, ultra-low latency CLAP (CLever Audio Plug-in) audio plugin for real-time [Neural Amp Modeler (NAM)](https://www.neuralampmodeler.com/) simulation on Linux DAWs.

It directly embeds [`NeuralAmpModeler-rs`](https://github.com/fabiohl/NeuralAmpModeler-rs) as its core neural DSP engine, inheriting all of its real-time guarantees: **zero heap allocations**, **zero locks**, and **zero blocking system calls** on the real-time audio thread, `x86-64-v3` (AVX2/FMA) baseline SIMD vectorization, and exact numerical parity against canonical C++ NAMCore and double-precision f64 reference oracles.

Designed for seamless integration into modern Linux digital audio workstations (DAWs) such as Bitwig Studio and REAPER, NAM-Plug offers a declarative, hardware-accelerated Slint GUI (with **native X11 XEmbed embedding** into the host panel, plus Wayland and X11 floating fallbacks) for loading `.nam` neural amp models and `.wav` impulse responses (IRs), gain staging, noise gating, oversampling, anti-aliasing filter configuration, and real-time DSP performance telemetry.

> **❤️‍🔥 NAM-Plug is in active development.** Feedback, bug reports, performance metrics, and DAW compatibility notes are very welcome!

---

## 🎨 Visual Overview & GUI Showcase

![NAM-Plug GUI](docs/gui_main.png)

*The declarative Slint graphical interface running inside a Linux host. The interface features model and cabinet IR selectors, rotary controls for Input/Output gain staging and Noise Gate threshold, toggle selectors for half-band anti-aliasing Oversampling (`Off`, `2x`, `4x`) and Activation precision math (`Standard` vs `Fast`), an active/bypass state indicator, high-resolution adaptive peak level meters with IEC 60268-10 ballistics, and a real-time DSP telemetry status bar.*

---

## ⚡ Key Strengths & Architectural Highlights

* **Native CLAP 1.2+ Standard Integration:** Built on top of the `clack` framework, exposing a clean, robust implementation of the CLever Audio Plug-in (CLAP) standard with zero translation overhead, sample-accurate parameter automation, and native host extension compliance (`audio-ports`, `audio-ports-activation`, `params`, `state`, `state-context`, `latency`, `gui`, `track-info`, `remote-controls`, `param-indication`, `preset-discovery`, `render`, `tail`, `log`).
* **Inherited Neural Engine Excellence & Dual-Oracle Numerical Parity:** Powered by [`NeuralAmpModeler-rs`](https://github.com/fabiohl/NeuralAmpModeler-rs), supporting WaveNet (A1/A2 standard & slimmable profiles), LSTM (1-layer and 2-layer topologies, multi-I/O), ConvNet, Linear FIR/FFT, and Sequential compositions. Strictly validated under a continuous dual-oracle regression contract against canonical C++ NAMCore ($f32$) and double-precision $f64$ mathematical oracles tracking 52 model baselines (WaveNet Standard ESR $< 2.31 \times 10^{-14}$, SNR $> 136.4\text{ dB}$, MR-STFT $< 6.5 \times 10^{-6}$; ConvNet ESR $< 9.33 \times 10^{-16}$, SNR $> 150.3\text{ dB}$).
* **SIMD Hardware Acceleration & Const-Generic Specialization:** Enforces `x86-64-v3` (AVX2/FMA) baseline vectorization with hand-written SIMD kernels and static `dispatch_simd!` monomorphization (zero vtables or indirect function pointer calls). Features 24 distinct model variants (16 static compile-time monomorphized profiles for canonical geometries $CH=16, 12, 8, 4, 3$ + dynamic zero-alloc fallbacks) utilizing tap-major layouts and `sum0..sum3` instruction-level unrolling.
* **Strict Zero-Allocation RT Safety & 3-Tier GC Cascade:** The audio callback thread runs with strict real-time determinism — no heap allocations, no mutex locks, and no blocking I/O on the hot path (audited via `CountingAllocator`). Dropped models, IRs, and oversamplers cascade through lock-free SPSC channels (32 slots) → processor parking lot (16 slots) → atomic overflow ring buffer with poison-resilient rollback guards (`ActivateRollbackGuard`).
* **Denormal & Subnormal Armor:** Injects symmetric $-220\text{ dBFS}$ deterministic dither ($1.0 \times 10^{-11}$) combined with hardware MXCSR FTZ/DAZ reassertion on every processing block, preventing 10–100× CPU microcode penalties during digital silence with zero net DC drift.
* **Native Minimum-Phase Polyphase Sinc Resampling:** Built-in 256 phases × 64 taps Kaiser sinc resampler (Kaiser $\beta=12$, $>105\text{ dB}$ stopband attenuation, $< 0.05\text{ dB}$ passband ripple, zero pre-ringing, $0.7\text{–}1.3\text{ µs}$ execution) flawlessly adapts 48 kHz models to any host sample rate (44.1 kHz, 96 kHz, 192 kHz).
* **Low-Latency Cabinet IR Convolution:** Uniform-Partitioned Overlap-Save (UPOLS) FFT convolution engine runs speaker cabinet impulse responses (`.wav`) in **$1.3\text{ µs}$** (for 512-sample IRs) with bounded, deterministic latency.
* **Binary `.namb` v2 Format Support:** Loads pre-transposed binary models (Gate-Major LSTM, Interleaved-4 WaveNet) with IEEE 802.3 CRC32 integrity validation, reducing model load and hot-swap times from $\sim 50\text{ ms}$ to $< 1\text{ ms}$.
* **Branchless FMA-Optimized Bypass Crossfader:** 32 ms equal-power crossfade blending during bypass transitions, executing branchlessly with FMA vectorization and adaptive handling of fractional phase discrepancies between dry capture and resampled wet streams.
* **Cold-Path Latency Caching & Dynamic PDC:** Effective latency (resampler + oversample + cab-sim) is cached on the audio thread and recomputed strictly during cold asset swaps, driving instant DAW Plugin Delay Compensation (`clap_plugin_latency`) without per-block audio thread overhead.
* **Decoupled Gain Staging & Model Calibration:** Embedded model loudness metadata calibration (`input_mult_adj`/`output_mult_adj`) is isolated from sample-accurate DAW user-gain automation (`ParamSmoother`), preventing automation sweeps from altering static model calibration multipliers.
* **Generation-Counter Fast Path (`gui_param_generation`):** Eliminates redundant atomic float loads when parameters are stationary, reading parameter targets only upon modification.
* **Hardware-Accelerated Slint GUI with Native X11 Embedding & Wayland:** Declarative UI built with Slint 1.18 (FemtoVG / OpenGL rendering via `winit`), with **native X11 XEmbed embedding** into the host's plugin panel (`CLAP_WINDOW_API_X11`, embedded) plus X11/Wayland floating fallbacks (`CLAP_WINDOW_API_X11`/`CLAP_WINDOW_API_WAYLAND`). Features 5-zone modular architecture, adaptive mono/stereo tricolor VU meters with IEC 60268-10 PPM ballistics, and dedicated 60 Hz lock-free telemetry polling via `SlintViewModel`.
* **Half-Band Anti-Aliasing Oversampling:** Optional `2x` and `4x` polyphase oversampling centered around the neural inference stage to eliminate high-frequency aliasing foldover in high-gain amp models (>100 dB stopband attenuation).
* **Selectable Activation Precision:** Supports both `Standard` (exact Taylor minimax, +89.5 dB SNR) and `Fast` (Padé polynomial minimax approximations) math modes to balance precision against CPU consumption on demanding setups.
* **Real-Time DSP Telemetry & Diagnostics:** Live footer display reporting sample rate (`SR`), buffer latency (`Lat`), DSP CPU load percentage (`DSP %`), CPU cycles per block, block size (`Last N`), real-time thread priority (`RT Prio`), overload xrun count, and diagnostic status flags (`Flags`).
* **5-Phase Advanced Optimization Pipeline (PGO + LLVM-BOLT):** Automated compilation suite (`build-release.sh`) leveraging synthetic neural DSP profiling, Profile-Guided Optimization (PGO), LLVM-BOLT machine code layout optimization, and 5 strict verification gates (SONAME/symbols, clap-validator, NAMCore float parity, CabSim IR, and `nam_perf_guard` performance certification).
* **Linker-Level Symbol Isolation:** Scoped version script (`hide-libm-shadow.map`) ensuring libm symbols resolve dynamically to `glibc` without dangerous PLT/GOT self-referential loops in release builds.

---

## 🥊 Feature Showcase ("Roofshoot")

| Feature / Attribute            | Technical Implementation                                                                     | Benefit & Impact                                                                              |
|:------------------------------ |:-------------------------------------------------------------------------------------------- |:--------------------------------------------------------------------------------------------- |
| **Inference Engine**           | Core `NeuralAmpModeler-rs` engine (WaveNet A1/A2, LSTM, ConvNet, Linear, Sequential)         | Dual-oracle verified fidelity ($>136.4\text{ dB}$ SNR WaveNet, $>150.3\text{ dB}$ ConvNet)    |
| **Const-Generic Profiles**     | 24 model variants (16 static compile-time profiles $CH=16, 12, 8, 4, 3$ + dynamic fallbacks) | Compile-time loop unrolling and optimal register allocation for canonical architectures       |
| **Plugin Standard**            | Native CLAP API wrapper (`clack-plugin` & `clack-extensions`)                                | Sub-millisecond buffer sizes, sample-accurate DAW automation, and dynamic PDC                 |
| **RT Determinism**             | Strict zero heap drop, zero mutex locks, 3-tier lock-free GC cascade                         | Guaranteed audio stability without buffer underruns (xruns) under strict deadlines            |
| **SIMD Hardware Acceleration** | Engine `x86-64-v3` (AVX2/FMA) baseline with static `dispatch_simd!` dispatch                 | WaveNet Std 41.8 µs / LSTM 6.8 µs per 64-sample block (AMD Ryzen 7 5700U)                     |
| **Denormal Armor**             | Symmetric −220 dBFS dither injection + hardware MXCSR FTZ/DAZ                                | Prevents 10–100× CPU microcode stalls on digital silence with zero net DC drift               |
| **Bypass Crossfader**          | 32 ms equal-power crossfade with branchless FMA loop & phase compensation                    | Smooth, pop-free bypass transitions with zero phase cancellation                              |
| **Dynamic Latency (PDC)**      | Cold-path cached effective latency with dynamic `clap_plugin_latency`                        | Instant host Plugin Delay Compensation with 0 per-block overhead                              |
| **Native Polyphase Resampler** | 256 phases × 64 taps Kaiser sinc resampler (>105 dB stopband, <0.05 dB ripple)               | Pristine multi-rate conversion (0.7–1.3 µs) with zero pre-ringing                             |
| **Cabinet IR Convolution**     | Uniform-Partitioned Overlap-Save (UPOLS) FFT convolution engine (.wav IRs)                   | Ultra-low-overhead speaker cabinet simulation ($1.3\text{ µs}$ for 512-sample IRs)            |
| **Binary `.namb` v2 Format**   | Pre-transposed memory layout (Gate-Major LSTM, Interleaved-4 WaveNet) with CRC32             | Reduces model loading and DAW track hot-swap time from ~50 ms to $< 1\text{ ms}$              |
| **Graphical User Interface**   | Slint 1.18 declarative UI (FemtoVG/OpenGL backend via `winit`)                               | Native X11 XEmbed embedding + Wayland/X11 floating, high-DPI scaling, IEC 60268-10 ballistics |
| **GUI Event Loop & Telemetry** | Dedicated `"nam-slint-gui"` event loop thread with 60 Hz `SlintViewModel`                    | 0% audio thread overhead, lock-free atomics, smooth 60 FPS metering                           |
| **Oversampling**               | Half-band polyphase FIR filters (`Off`, `2x`, `4x`, >100 dB stopband)                        | Eliminates aliasing distortion on high-gain amp models                                        |
| **Activation Precision**       | `Standard` (exact Taylor minimax, +89.5 dB SNR) vs `Fast` (Padé polynomial approximations)   | User-selectable trade-off between math precision and CPU latency                              |
| **CLAP State Persistence**     | Lock-free atomic synchronization & JSON serialization                                        | Full preset saving/loading and seamless DAW project restoration                               |
| **Diagnostics & Telemetry**    | Atomic telemetry bitmask & `LogBuffer` ring buffer integration                               | Real-time CPU, latency, overload, and flag telemetry in GUI footer                            |
| **Release Optimization**       | 5-phase PGO + LLVM-BOLT pipeline with demangled assembly report                              | Minimized I-Cache misses and maximum instruction throughput                                   |

---

## 🏆 Measured Audio Fidelity & Real-Time Performance

NAM-Plug inherits the complete, uncompromised DSP engine of [`NeuralAmpModeler-rs`](https://github.com/fabiohl/NeuralAmpModeler-rs), backed by formal regression envelopes and continuous quality contracts.

### Dual-Oracle Parity & Perceptual Validation

Accuracy is strictly enforced against two co-equal reference oracles across 52 model baselines (`NeuralAmpModeler-rs/docs/quality-contract.json`):

1. **Canonical C++ NAMCore Reference ($f32$):** Verifies bit-exact behavioral parity across WaveNet A1/A2, LSTM, ConvNet, and Linear architectures.
2. **Double-Precision Reference ($f64$):** Quantifies numerical ideality and eliminates floating-point rounding divergence.

| Architecture / Model | Error-to-Signal Ratio (ESR) | Signal-to-Noise Ratio (SNR) | Multi-Resolution STFT  |
|:-------------------- |:--------------------------- |:--------------------------- |:---------------------- |
| **WaveNet Standard** | $< 2.31 \times 10^{-14}$    | $> 136.4\text{ dB}$         | $< 6.5 \times 10^{-6}$ |
| **WaveNet A2**       | $< 2.10 \times 10^{-14}$    | $> 136.8\text{ dB}$         | $< 5.8 \times 10^{-6}$ |
| **ConvNet ReLU**     | $< 9.33 \times 10^{-16}$    | $> 150.3\text{ dB}$         | $< 1.2 \times 10^{-6}$ |
| **LSTM 1-Layer**     | $< 1.45 \times 10^{-14}$    | $> 138.2\text{ dB}$         | $< 4.1 \times 10^{-6}$ |

All models satisfy spectral convergence and LUFS loudness constancy invariants, ensuring identical frequency response and loudness perception compared to reference hardware captures.

### 1. Bare DSP Engine Core Microbenchmarks (`NeuralAmpModeler-rs`)

Execution times for isolated DSP kernels measured per **64-sample block at 48 kHz** ($1.33\text{ ms}$ real-time deadline budget) on an AMD Ryzen 7 5700U (`x86-64-v3` AVX2 baseline, `rustc 1.99.0`):

| Component / Profile                            | Execution Time | RT Deadline Budget Used | Theoretical DAW Track Capacity |
|:---------------------------------------------- |:-------------- |:----------------------- |:------------------------------ |
| **WaveNet Standard (CH=16)**                   | **41.8 µs**    | 3.1%                    | ~31 tracks per core            |
| **WaveNet Lite (CH=12)**                       | **57.2 µs**    | 4.3%                    | ~23 tracks per core            |
| **WaveNet Feather (CH=8)**                     | **20.1 µs**    | 1.5%                    | ~66 tracks per core            |
| **WaveNet Nano (CH=4)**                        | **18.3 µs**    | 1.4%                    | ~72 tracks per core            |
| **WaveNet A2 Full (CH=8)**                     | **19.8 µs**    | 1.5%                    | ~67 tracks per core            |
| **WaveNet A2 Lite (CH=3)**                     | **14.0 µs**    | 1.1%                    | ~95 tracks per core            |
| **LSTM 1-Layer (Hidden=16)**                   | **6.8 µs**     | 0.5%                    | ~195 tracks per core           |
| **LSTM 2-Layer (Hidden=8)**                    | **7.6 µs**     | 0.6%                    | ~175 tracks per core           |
| **ConvNet**                                    | **9.5 µs**     | 0.7%                    | ~140 tracks per core           |
| **Linear FIR (RF=4 Direct)**                   | **0.13 µs**    | 0.01%                   | >1,000 tracks per core         |
| **DSP Resampler (44.1 kHz → 48 kHz)**          | **1.3 µs**     | 0.10%                   | >1,000 tracks per core         |
| **Cabinet IR Simulation (UPOLS 512)**          | **1.3 µs**     | 0.10%                   | >1,000 tracks per core         |
| **Full Pipeline Base (Model + CabSim, No OS)** | **42.8 µs**    | 3.2%                    | ~31 tracks per core            |
| **Full Pipeline HQ (Model + CabSim + 4× OS)**  | **174.4 µs**   | 13.1%                   | ~7 tracks per core             |

### 2. End-to-End CLAP Plugin Binary Certification (`nam_plug.clap`)

In addition to bare engine microbenchmarks, the distributed release artifact (`~/.clap/nam_plug.clap`) undergoes automated real-time certification via `nam_perf_guard --certify` under realistic DAW host execution conditions (including CLAP event queue consumption, lock-free parameter sync, denormal armor, equal-power crossfader, and audio buffer routing) after PGO + LLVM-BOLT optimization:

| Host Scenario / Topology           | Block Size | p50 Latency | p99 Latency | Deadline Budget | Budget Used (p50 / p99) | Gate Headroom | Certification Status |
|:---------------------------------- |:---------- |:----------- |:----------- |:--------------- |:----------------------- |:------------- |:-------------------- |
| **WaveNet A1 Standard**            | 64 samples | **102.2 µs**| **111.8 µs**| 1.33 ms         | 7.7% / 8.4%             | **90.1%**     | ✅ PASS (Certified)  |
| **WaveNet A1 Standard**            | 128 samples| **201.4 µs**| **219.5 µs**| 2.67 ms         | 7.6% / 8.2%             | **90.3%**     | ✅ PASS (Certified)  |
| **WaveNet A2 Slimmable**           | 64 samples | **49.9 µs** | **60.1 µs** | 1.33 ms         | 3.7% / 4.5%             | **94.7%**     | ✅ PASS (Certified)  |
| **LSTM 1×3**                       | 64 samples | **12.4 µs** | **19.9 µs** | 1.33 ms         | 0.9% / 1.5%             | **98.2%**     | ✅ PASS (Certified)  |
| **CLAP Passthrough (No Model)**    | 64 samples | **2.3 µs**  | **2.4 µs**  | 1.33 ms         | 0.17% / 0.18%           | **99.8%**     | ✅ PASS (Certified)  |

> **Factual Highlights from Certification (`target/perf-certification-report.json`):**
> * **Pure Host Wrapper Overhead:** Total CLAP host abstraction, event dispatching, and lock-free telemetry consumes only **2.3 µs** per 64-sample block.
> * **C++ NAMCore Parity in Host:** Parity tests confirm SNR of **110.5 dB** at 44.1 kHz, **111.0 dB** at 48.0 kHz, and **110.9 dB** at 96.0 kHz.
> * **Zero Heap Allocation Invariant:** Certified **0 heap allocations** across 10 stress scenarios via `CountingAllocator` audit.
> * **Why This Matters in Your DAW (Bitwig / REAPER):** Even under worst-case p99 spikes with a full WaveNet A1 amplifier model, the complete CLAP plugin consumes less than **8.4%** of a 64-sample quantum ($1.33\text{ ms}$ at 48 kHz), leaving $>90\%$ of CPU budget for DAW mixing, instruments, and complex FX chains.

---

## 🛠️ System Prerequisites

| Dependency                | Minimum Version                               | Package / Command     |
|:------------------------- |:--------------------------------------------- |:--------------------- |
| **Linux Kernel**          | ≥ 5.10                                        | `uname -r`            |
| **Rust Toolchain**        | ≥ 1.99.0 (edition 2024)                       | `rustc --version`     |
| **CPU Architecture**      | `x86_64` with AVX2/FMA (`x86-64-v3` baseline) | `lscpu`               |
| **CLAP Host / DAW**       | Bitwig, REAPER, Studio Pro, etc.              | Host application      |
| **Development Libraries** | `build-essential`, `pkg-config`, `cmake`, GL  | See apt command below |

> **MSRV policy:** `rust-version = "1.99.0"` in `Cargo.toml` is the **public MSRV promise**.
> Development happens on `stable` (pinned by `rust-toolchain.toml`). The MSRV promise is
> verified as an **isolated local check** — never mixed with the dev toolchain:
> `cargo +1.99.0 check --locked` from the repository root.

### Installation of System Dependencies (Debian / Ubuntu / Pop!_OS)

```bash
sudo apt update && sudo apt install -y build-essential pkg-config cmake libgl1-mesa-dev libx11-dev libxcursor-dev libxcb1-dev libxkbcommon-dev
```

---

## 🚀 Building & Installation

> [!IMPORTANT]
> **Host Memory Protection (`CARGO_BUILD_JOBS=4`):**
> On systems with high core counts, Cargo can spawn numerous parallel `rustc` processes that may rapidly saturate available RAM and trigger system or IDE OOM crashes. Always limit parallelism by setting `export CARGO_BUILD_JOBS=4` before running `cargo build`, `cargo test`, `cargo clippy`, or test scripts. For heavy optimization pipelines, `utils/mem-guard.sh` wraps commands with systemd/cgroup resource limits.

### 1. Direct Shared Library Build (`cargo build`)

For standard plugin compilation:

```bash
export CARGO_BUILD_JOBS=4
cargo build --release
```

The resulting shared library will be placed at `target/release/libnam_plug.so`.

To install the plugin into your user CLAP directory:

```bash
mkdir -p ~/.clap
cp target/release/libnam_plug.so ~/.clap/nam_plug.clap
```

For development and host-harness testing support:

```bash
cargo build --features testing
```

To run the standalone GUI preview tool (live Slint interface without a DAW host):

```bash
cargo run --bin ui_preview
```

---

### 2. Mega-Optimized Compiler Build (`./utils/build-release.sh`)

For maximum performance in live and studio DAW environments, `NAM-Plug` includes a 5-phase optimization pipeline leveraging **Profile-Guided Optimization (PGO)** and **LLVM BOLT** (Binary Optimization and Layout Tool).

```bash
./utils/build-release.sh
```

#### What `build-release.sh` does under the hood

1. **Phase 1 — Environment Verification:** Validates toolchain prerequisites (`rustc`, `cargo`, `python3`, `tar`, `zstd`, `flatpak`, `llvm-profdata`, `llvm-bolt`, and `perf`) and verifies target CPU flags from `.cargo/config.toml`.
2. **Phase 2 — PGO Trace Generation:** Compiles `pgo_profiling_workload` with `-Cprofile-generate`, executing synthetic neural DSP workloads to collect realistic CPU performance profiles (`.profraw`), merging them into `merged.profdata`.
3. **Phase 3 — PGO-Optimized Compilation:** Recompiles `libnam_plug.so` using `-Cprofile-use=merged.profdata` and relocation symbols (`-Clink-arg=-Wl,-q`), allowing LLVM to optimize hot loops, inline activation functions, and unroll vector SIMD loops.
4. **Phase 4 — LLVM BOLT Machine Code Reordering:** Reorders machine code instructions via `llvm-bolt` to minimize Instruction Cache (I-Cache) misses and TLB pressure during real-time processing.
5. **Phase 4.5 — Assembly Hotspot Disassembly Report:** Outputs an AI-ready demangled disassembly report at `target/dsp_hotpath.asm`.
6. **Phase 5 — Automated Deployment & Strict Certification:** Strips and installs the finalized, hyper-optimized plugin directly to `~/.clap/nam_plug.clap`, executing 5 release certification gates:
   * **Gate 1:** Exported symbols and SONAME validation (`clap_entry`, SONAME presence).
   * **Gate 2:** External `clap-validator` test suite compliance.
   * **Gate 3:** NAMCore C++ float parity oracle (`test_clap_parity_multi_rate`).
   * **Gate 4:** CabSim IR artifact test against distributed `.so` (`test_cabsim_ir_changes_audio_release_artifact`).
   * **Gate 5:** Distributed artifact performance certification gate (`nam_perf_guard` latency distributions and real-time deadline margins).
7. **Phase 6 — Release Packaging (.tar.zst):** Generates a release distribution archive at `~/nam-plug-vx.y.z-linux-x86_64-v3.tar.zst` containing the plugin, documentation, license, and a 1-click installation script.
8. **Phase 7 — Release Packaging (.flatpak):** Builds and exports the standalone Flatpak plugin extension bundle (`~/nam-plug-vx.y.z-linux-x86_64-v3.flatpak`) with AppStream metadata for sandboxed DAWs (Bitwig, REAPER).

#### CLI Options

| Option         | Description                                                                                                |
|:-------------- |:---------------------------------------------------------------------------------------------------------- |
| `--install`    | Automatically installs the Flatpak extension locally (`flatpak install --user`) in addition to `~/.clap/`. |
| `--no-flatpak` | Skips Phase 7 (Flatpak bundle creation).                                                                   |
| `--no-tarball` | Skips Phase 6 (.tar.zst archive creation).                                                                 |
| `--no-pgo`     | Skips Phase 2/3 (Profile-Guided Optimization) and compiles directly with the `dist` release profile.       |
| `--no-bolt`    | Skips Phase 4 (LLVM BOLT post-link optimization).                                                          |
| `-h, --help`   | Displays command-line help screen and exits.                                                               |

---

### 3. Flatpak Plugin Extension Distribution (`.flatpak`)

In addition to traditional shared library installation, `NAM-Plug` is distributed as a standalone **Flatpak Audio Plugin Extension** (`org.freedesktop.LinuxAudio.Plugins.NAMPlug`), targeting the standard `org.freedesktop.LinuxAudio.BaseExtension` runtime point (`branch 25.08`).

This format enables sandboxed Flatpak DAWs (including Bitwig Studio `com.bitwig.BitwigStudio`, REAPER `fm.reaper.Reaper`, and Studio One `com.fender.studioapp8`) to seamlessly discover and load `NAM-Plug` without requiring insecure filesystem sandbox holes (`--filesystem=host` or `--filesystem=home`).

#### End-User Installation

Install the `.flatpak` bundle directly into your local user Flatpak repository:

```bash
flatpak install --user --reinstall ~/nam-plug-v0.8.0-linux-x86_64-v3.flatpak
```

#### How DAW Discovery Works in Flatpak

When installed, the plugin binary is mounted inside the DAW container at `/app/extensions/Plugins/clap/nam_plug.clap`. Compatible Flatpak DAWs configured with the `org.freedesktop.LinuxAudio.Plugins` extension point automatically scan this directory on startup and expose `NAM-Plug` directly in their native CLAP plugin browser.

To verify the installed extension files on your system:

```bash
ls -la ~/.local/share/flatpak/runtime/org.freedesktop.LinuxAudio.Plugins.NAMPlug/x86_64/25.08/active/files/clap/
```

> [!NOTE]
> **AppStream Metadata in Local Bundles vs. Flathub Repositories:**
> Single-file `.flatpak` bundles distribute exclusively the extension's runtime commit; they do not bundle repository-wide AppStream catalog branches (`appstream/x86_64`). Consequently, graphical Flatpak managers (such as Warehouse or GNOME Software) display bundles installed from a local origin (`namplug-origin`) with fallback labels (*"Sem metadados"* / *"No metadata"* and an empty `Versão` column in `flatpak list`). In production distributions via Flathub or remote OSTree repositories, the AppStream catalog is indexed automatically. See [`docs/architecture.md`](docs/architecture.md#105-appstream-catalog-dynamics-bundle-semantics--diagnostic-invariants) for architectural details.

#### Developer Workflow (Building & Testing Flatpak Locally)

You can build and package the Flatpak extension bundle locally using either the release pipeline or `flatpak-builder`:

1. **Automated Pipeline Build & Install:**

   ```bash
   ./utils/build-release.sh --install
   ```

2. **Standalone Manifest Compilation via `flatpak-builder`:**

   ```bash
   # Build the release CLAP library first
   cargo build --release

   # Compile and install the extension manifest locally
   flatpak-builder --user --install --force-clean \
     --state-dir=target/flatpak-builder \
     target/flatpak-build \
     packaging/flatpak/org.freedesktop.LinuxAudio.Plugins.NAMPlug.yml
   ```

#### Uninstallation

To remove the Flatpak plugin extension:

```bash
flatpak uninstall --user org.freedesktop.LinuxAudio.Plugins.NAMPlug
```

---

## 🎛️ DAW Usage & Workflow Guide

### 1. Host Scanning & Loading

1. Open your CLAP-compatible DAW.
2. Trigger a plugin rescan if required. `NAM-Plug` will appear under your CLAP plugin list as **NAM-Plug** (or **Neural Amp Modeler**).
3. Insert `NAM-Plug` into an audio or guitar track.

### 2. Loading Models & Cabinet IRs

1. **Neural Model:** Click **`Load Model`** in the left panel to select a `.nam` or `.namb` amplifier model file.
2. **Cabinet Impulse Response:** Click **`Load IR`** to load a speaker cabinet `.wav` impulse response.

### 3. Staging & Performance Tuning

1. **Gain Staging:** Use the **`INPUT`** and **`OUTPUT`** rotary knobs to balance signal levels (`-20.0 dB` to `+20.0 dB`, default `0.0 dB`).
2. **Noise Gate:** Adjust the **`GATE`** knob (`-90.0 dB` to `-40.0 dB`, default `-90.0 dB` — off by default, i.e. parked at the most permissive end of the range) to eliminate hum and background noise when not playing.
3. **Anti-Aliasing Oversampling:** Select **`2x`** or **`4x`** polyphase oversampling when running high-gain amplifier models to eliminate aliasing foldover distortion.
4. **Activation Math Mode:** Switch between **`Standard`** (exact precision, default) and **`Fast`** (Padé polynomial approximations) to optimize CPU usage on large sessions.
5. **Active / Bypass:** Toggle the **`ACTIVE`** button to bypass or re-engage processing seamlessly with 32 ms equal-power crossfading.

### 4. Telemetry Footer Monitoring

The status bar at the bottom of the plugin GUI provides real-time telemetry:

* **SR:** Host sample rate (e.g., `48.0 kHz`).
* **Lat:** Added latency in samples/ms (e.g., `0 ms` when oversampling is Off).
* **DSP:** Percentage of real-time audio block budget consumed (e.g., `DSP: 8.2%`).
* **Cycles:** Exact CPU cycle count for the last processed block.
* **Last N:** Quantum block size (e.g., `128` samples).
* **RT Prio:** Operating system real-time thread priority (e.g., `85`).
* **Overloads:** Count of detected audio buffer overruns/underruns (xruns).
* **Flags:** Atomic diagnostic bitmask status.

---

## ⚠️ Known Host Limitations & DAW Compatibility

| DAW / Host Environment                                  | Status                  | Compatibility Notes                                                                                                                                                                                                                                                           |
|:------------------------------------------------------- |:----------------------- |:----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Bitwig Studio** (Linux)                               | ✅ Full Support         | Native X11 embedded GUI (default) with X11/Wayland floating fallback, sample-accurate automation, state save/restore, and offline bounce.                                                                                                                                     |
| **REAPER** (Native Linux)                               | ✅ Full Support         | Native X11 embedded GUI (default) with X11/Wayland floating fallback, parameter automation, and ultra-low latency playback.                                                                                                                                                   |
| **PreSonus Studio One** / **Fender Studio Pro** (Linux) | ⚠️ Known GUI Limitation | **Known issue:** the host cannot initialize a natively embedded X11 GUI surface. NAM-Plug serves X11 floating and Wayland floating (`CLAP_WINDOW_API_X11`/`CLAP_WINDOW_API_WAYLAND` with `is_floating = true`), opening its own top-level window with bounded teardown joins. |

---

## 🧪 CI & QA Automation Suite (`./utils/`)

The `./utils/` directory contains maintainer tools and standard scripts for code quality, CLAP compliance, and continuous integration:

| Script                                                                           | Purpose & Execution Scope                                                                                                                                                                                                                      |
|:-------------------------------------------------------------------------------- |:---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [`utils/lints.sh`](utils/lints.sh)                                               | **Static Analysis Gate:** Runs `cargo fmt`, compilation checks (`cargo check`), strict `cargo clippy` across feature combinations, validates SPDX license headers, static RT allocations, and AppStream metadata.                              |
| [`utils/tests-quick.sh`](utils/tests-quick.sh)                                   | **Consolidated QA Suite:** 3-phase agile gate executing all unit and integration tests (`--lib --tests`), release float parity against NAMCore C++, RT codegen disassembly guard, and RT-safety heap-audit (`--features testing,heap-audit`).  |
| [`utils/tests-long.sh`](utils/tests-long.sh)                                     | **Nightly / Pre-Release Certification:** 4-phase heavy stress suite testing GC model swaps, off-RT teardown drain, multi-instance RT priority/SCHED_FIFO, and headless GUI lifecycle under isolated CPU core pinning.                          |
| [`utils/tests-gui.sh`](utils/tests-gui.sh)                                       | **Headless GUI & XEmbed Suite:** Headless Xvfb lifecycle, XEmbed embedding cycles, and system clipboard interoperability testing.                                                                                                              |
| [`utils/verify_rt_codegen.sh`](utils/verify_rt_codegen.sh)                       | **Static Machine-Code Codegen Guard:** Disassembles the CLAP `.so` and verifies zero `malloc`/`free`, zero `div`/`idiv` in core loops, and bounded TLS access via `dsp-hotpath-filter.py`.                                                     |
| [`utils/tests-performance-regression.sh`](utils/tests-performance-regression.sh) | **Criterion Regression Guard:** Enforces performance budgets and checks CPU governor/isolation against benchmark baselines.                                                                                                                    |
| [`utils/build-release.sh`](utils/build-release.sh)                               | **Compiler Optimization Pipeline:** Multi-stage release builder using PGO and LLVM BOLT, outputting assembly report `target/dsp_hotpath.asm`, binary `~/.clap/nam_plug.clap`, and release archive `~/nam-plug-vx.y.z-linux-x86_64-v3.tar.zst`. |
| [`utils/mod-update.sh`](utils/mod-update.sh)                                     | **Supply Chain Update Pipeline:** Updates active Rust toolchain via `rustup`, dependency specifications via `cargo-upgrade`, and resolved versions in `Cargo.lock`.                                                                            |
| [`utils/test-lib-drift.sh`](utils/test-lib-drift.sh)                             | **Shell QA Library Drift Guard:** Verifies cryptographic SHA-256 integrity of canonical `_lib_core.sh` and shared `dsp-hotpath-filter.py`, running `pick_bench_core` unit tests.                                                               |
| [`utils/test-pick-bench-core.sh`](utils/test-pick-bench-core.sh)                 | **Bench Core Selection Unit Tests:** Verifies CPU core resolution logic against simulated sysfs trees (isolated cores, offline CPUs, fallbacks).                                                                                               |

---

## 📚 Architectural & Technical Documentation

The following technical documents are maintained in the source repository:

| Document                                                                                                                         | Primary Focus & Topic Coverage                                                                                                          |
|:-------------------------------------------------------------------------------------------------------------------------------- |:--------------------------------------------------------------------------------------------------------------------------------------- |
| [`docs/architecture.md`](docs/architecture.md)                                                                                   | CLAP plugin architecture, SPSC GC thread model, Slint declarative GUI integration, lock-free state synchronization                      |
| [`docs/testing.md`](docs/testing.md)                                                                                             | Test suite layout, host-harness verification phases, CLAP test policies, and test coverage matrix                                       |
| [`docs/functional-tests.md`](docs/functional-tests.md)                                                                           | Plugin functional test checklist and verification matrices                                                                              |
| [`docs/postmortem-libm-symbol-interposition.md`](docs/postmortem-libm-symbol-interposition.md)                                   | Technical postmortem on libm symbol interposition resolution on Linux dynamic linkers                                                   |
| [`docs/rt-hardening-evaluation.md`](docs/rt-hardening-evaluation.md)                                                             | Formal architectural evaluation of `rt-hardening` for CLAP plugins — decision: not activated in plugin, delegated to host |
| [`NeuralAmpModeler-rs: Audio Fidelity Map`](https://github.com/fabiohl/NeuralAmpModeler-rs/blob/main/docs/audio_fidelity_map.md) | DSP decision quality trade-off matrix and frequency response analysis (NeuralAmpModeler-rs engine)                                      |

---

## 🙏 Credits & Acknowledgments

* **Steven Atkinson** — Creator of [Neural Amp Modeler (NAM)](https://github.com/sdatkinson/neural-amp-modeler) for pioneering deep learning guitar amplifier modeling.
* **Clack Framework & CLAP Community** — For providing the Rust `clack` library and creating the open, modern CLever Audio Plug-in standard.
* **Slint Team & Community** — For the declarative GUI framework and high-performance FemtoVG/OpenGL renderer.

---

## ⚖️ License & AI Transparency

### AI Transparency Note

The system architecture, real-time safety guarantees, CLAP state management, DSP pipeline design, and GUI implementation are intellectual work (and love) of the maintainer (**Fábio Lima**). Implementation was accelerated through pair programming (*Vibe Coding*) using artificial intelligence models (Gemini, Claude, Grok, DeepSeek and others) within Google Antigravity IDE. IA is just a tool that make wonder in wise hands.

### License

This project is licensed under the **GNU General Public License v3.0 or later** (**GPL-3.0-or-later**). See [LICENSE.txt](LICENSE.txt) for full license details.
