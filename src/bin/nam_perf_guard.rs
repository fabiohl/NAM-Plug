// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! CLI entry point for the Distributed Artifact Performance Certification Gate.
//!
//! Dynamically loads the final distributed CLAP artifact (post-strip and post-BOLT),
//! executes authentic neural inference benchmarks across core topologies (WaveNet A1 Standard,
//! WaveNet A2 Slimmable, LSTM) and block sizes (64 and 128 samples), computes statistical latency
//! distributions (p50, p95, p99, max, mean, ns/sample), evaluates real-time deadline margins
//! against the 85%-of-budget certification gate (the same threshold the audio callback
//! telemetry uses to flag a DSP overload), and detects host environmental noise / thermal
//! anomalies.
//!
//! The report also records the host measurement context (pinned core, scheduling
//! policy, governor, boost, online/isolated CPUs and IRQ isolation) so a number is
//! always attributable to the environment that produced it, and can optionally be
//! compared against a previous certification report (`--reference`) with a relative
//! p99 tail ceiling and mandatory artifact identity.
//!
//! Exit codes:
//!   0 — Performance certification PASSED.
//!   1 — Performance certification FAILED (deadline overrun, reference ratio breach, or fatal error).
//!   2 — Performance certification INCONCLUSIVE (excessive host CPU jitter / noise).

#![cfg(feature = "testing")]

use clack_common::events::Pckn;
use clack_extensions::audio_ports_config::PluginAudioPortsConfig;
use clack_extensions::state::PluginState;
use clack_host::events::event_types::ParamValueEvent;
use clack_host::prelude::*;
use neural_amp_modeler_rs::common::diagnostics::logger::{LoggerConfig, NamLogger};
use neural_amp_modeler_rs::common::params::{ActivationPrecision, ProcessingParams};
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

/// Expected input amplitude for the non-timed activation probe block.
const PROBE_INPUT_SAMPLE: f32 = 0.1;

/// Maximum deviation from `PROBE_INPUT_SAMPLE` allowed on the no-model
/// (`Passthrough_Overhead_64_samp`) scenario (accounts for `-220 dBFS` / `1e-11`
/// denormal dither and f32 rounding).
// Measured: Passthrough_Overhead_64_samp max_abs_diff=2.831e-7, rms_diff=2.238e-7
const PASSTHROUGH_PROBE_MAX_DIFF: f64 = 1.0e-5;

/// Minimum deviation from the no-model reference (`PROBE_INPUT_SAMPLE`)
/// required to prove a neural model actually processed the probe block.
// Measured: WaveNet_A1_64/128 max_abs_diff=1.150e-1, WaveNet_A2_64 max_abs_diff=1.037e-1, LSTM_64 max_abs_diff=7.786e-2 (vs passthrough 2.831e-7)
const MODEL_PROBE_MIN_DIFF: f64 = 1.0e-2;

/// Minimum deviation between cabinet-wet and cabinet-bypassed probe outputs
/// required to prove a configured cabinet (WAV IR or NAM) actively processed audio.
// Measured: WaveNet_A1_Stereo_CabWAV_64/128 max_abs_diff=2.433e-2, WaveNet_A1_Stereo_CabNAM_64/128 max_abs_diff=1.308e-2
const CAB_PROBE_MIN_DIFF: f64 = 1.0e-3;

const CONFIG_ID_STEREO: u32 = 1;
const PARAM_INPUT_GAIN: u32 = 0;
const PARAM_BYPASS: u32 = 3;
const PARAM_CAB_BYPASS: u32 = 13;

/// Certification gate fraction of the real-time block deadline budget.
///
/// A scenario is certified only when p99 latency stays below 85% of the block
/// budget — the same threshold the audio callback telemetry uses to count a
/// DSP overload (`processor/dsp/telemetry.rs`) — so the certification gate
/// fails strictly before the runtime would report an overload. This reserves
/// headroom for the scheduling jitter a live host adds on top of the isolated
/// measurement and is strictly stricter than the nominal 100% deadline: an
/// artifact that passes here also passes the raw deadline check.
const DEADLINE_GATE_FRACTION: f64 = 0.85;

struct PerfHostShared;
impl clack_host::prelude::SharedHandler<'_> for PerfHostShared {
    fn request_restart(&self) {}
    fn request_process(&self) {}
    fn request_callback(&self) {}
}

struct PerfHost;
impl clack_host::prelude::HostHandlers for PerfHost {
    type Shared<'a> = PerfHostShared;
    type MainThread<'a> = ();
    type AudioProcessor<'a> = ();
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PerfCertificationReport {
    pub schema_version: String,
    pub timestamp_utc: String,
    /// Runner identifier, when the caller/CI exported `NAM_RUN_ID`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub clap_artifact_path: String,
    pub clap_artifact_sha256: String,
    pub overall_status: String,
    /// Whether the measurement series itself was stable (low coefficient of
    /// variation). This is a noise statistic, not an isolation claim.
    pub environment_stable: bool,
    /// Whether the host was less isolated than the certification policy expects
    /// (no `isolcpus`/`nohz_full`, shared IRQ affinity, non-RT scheduling,
    /// non-performance governor, or an unpinned affinity mask).
    ///
    /// A degraded host does not by itself fail the deadline gate: the measured
    /// p99 is still valid evidence for *this* host. It is recorded separately so
    /// the number is never read as if it came from a fully isolated machine.
    #[serde(default)]
    pub environment_degraded: bool,
    /// Typed, machine-readable reasons behind `environment_degraded`.
    #[serde(default)]
    pub environment_degraded_reasons: Vec<String>,
    pub total_scenarios: usize,
    pub scenarios: Vec<ScenarioPerfMetrics>,
    /// Measurement environment that produced the numbers above.
    ///
    /// `serde(default)` keeps pre-1.2 reports (and reference reports produced by
    /// them) readable: an old report simply has no host block.
    #[serde(default)]
    pub host: HostEnvironment,
    /// Relative p50 comparison against a `--reference` report, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_check: Option<ReferenceCheck>,
}

/// Host context captured at measurement time.
///
/// The certification gate is only meaningful when it runs under the same
/// core/priority policy as the regression gate; recording the policy in the
/// report makes a pipeline number comparable (or explicitly known not to be)
/// with `.performance-baselines` and with a manual pinned run.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct HostEnvironment {
    /// CPU selected by the affinity mask (or `NAM_BENCH_CORE`), if resolvable.
    pub bench_core: Option<usize>,
    /// `/proc/self/status:Cpus_allowed_list` of the measuring process.
    pub cpus_allowed_list: String,
    /// Kernel scheduling policy of the measuring thread (`SCHED_FIFO`, ...).
    pub sched_policy: String,
    /// `scaling_governor` of the benchmark core, when exposed by cpufreq.
    pub governor: Option<String>,
    /// CPU boost state (`1` enabled, `0` disabled), when exposed.
    pub boost: Option<String>,
    /// `/sys/devices/system/cpu/online`.
    pub cpu_online: String,
    /// `/sys/devices/system/cpu/isolated`.
    pub cpu_isolated: String,
    /// `isolated` when no IRQ affinity mask includes the benchmark core.
    pub irq_isolation: String,
    /// IRQ numbers whose affinity currently includes the benchmark core.
    pub irqs_sharing_bench_core: Vec<String>,
    /// Whether the process affinity was successfully narrowed to `bench_core`.
    #[serde(default)]
    pub affinity_applied: bool,
    /// Core reported by `sched_getcpu()` after applying the affinity.
    #[serde(default)]
    pub affinity_observed_core: Option<usize>,
    /// Typed reason the affinity could not be applied or verified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity_error: Option<String>,
}

const DEFAULT_NOISE_FLOOR_NS: f64 = 25_000.0;

fn default_noise_floor_ns() -> f64 {
    DEFAULT_NOISE_FLOOR_NS
}

/// Relative p99 tail ceiling against a previous certification report.
#[derive(Debug, Serialize, Deserialize)]
pub struct ReferenceCheck {
    pub reference_path: String,
    pub reference_sha256: String,
    pub status: String,
    pub p99_ratio_max: f64,
    pub p50_ratio_max: f64,
    #[serde(default = "default_noise_floor_ns")]
    pub p99_noise_floor_ns: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worst_p99_ratio: Option<f64>,
    pub worst_p50_ratio: f64,
    pub passed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inconclusive_reason: Option<String>,
    pub comparisons: Vec<ReferenceComparison>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ReferenceComparison {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_p50_ns: Option<f64>,
    pub measured_p50_ns: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50_ratio: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_p99_ns: Option<f64>,
    pub measured_p99_ns: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p99_ratio: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_max_ns: Option<f64>,
    pub measured_max_ns: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ratio: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p99_passed: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ReferenceReport {
    #[serde(default)]
    clap_artifact_sha256: Option<String>,
    scenarios: Vec<ReferenceScenario>,
}

#[derive(Debug, Deserialize)]
struct ReferenceScenario {
    name: String,
    #[serde(default)]
    p50_ns: Option<f64>,
    #[serde(default)]
    p99_ns: Option<f64>,
    #[serde(default)]
    max_ns: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ScenarioPerfMetrics {
    pub name: String,
    pub topology: String,
    #[serde(default)]
    pub channels: String,
    pub sample_rate: f64,
    pub block_size: usize,
    #[serde(default)]
    pub oversample: String,
    #[serde(default)]
    pub cabinet: String,
    #[serde(default)]
    pub events: String,
    pub iterations: usize,
    pub p50_ns: f64,
    pub p95_ns: f64,
    pub p99_ns: f64,
    pub max_ns: f64,
    pub mean_ns: f64,
    pub stddev_ns: f64,
    pub ns_per_sample: f64,
    pub deadline_budget_ns: f64,
    pub gate_threshold_ns: f64,
    pub deadline_margin_percent: f64,
    pub gate_headroom_percent: f64,
    pub deadline_passed: bool,
    #[serde(default)]
    pub process_errors: usize,
    #[serde(default)]
    pub status_rejected: usize,
    #[serde(default)]
    pub probe_passed: bool,
    #[serde(default)]
    pub probe_max_abs_diff: f64,
    #[serde(default)]
    pub probe_rms_diff: f64,
    #[serde(default)]
    pub cab_probe_passed: bool,
    #[serde(default)]
    pub cab_probe_max_abs_diff: f64,
}

fn sha256_of(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    Ok(Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn read_trim(path: &str) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Expands the kernel CPU-list syntax (`0-3,8-9`) into individual CPU ids.
fn parse_cpu_list(raw: &str) -> Vec<usize> {
    let mut cpus = Vec::new();
    for part in raw.trim().split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.split_once('-') {
            Some((start, end)) => {
                if let (Ok(start), Ok(end)) =
                    (start.trim().parse::<usize>(), end.trim().parse::<usize>())
                    && start <= end
                {
                    cpus.extend(start..=end);
                }
            }
            None => {
                if let Ok(cpu) = part.parse::<usize>() {
                    cpus.push(cpu);
                }
            }
        }
    }
    cpus.sort_unstable();
    cpus.dedup();
    cpus
}

fn read_proc_status_field(field: &str) -> Option<String> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key.trim() == field).then(|| value.trim().to_string())
    })
}

fn read_sched_policy() -> String {
    let Some(sched) = fs::read_to_string("/proc/self/sched").ok() else {
        return "unknown".to_string();
    };
    for line in sched.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim() != "policy" {
            continue;
        }
        let Ok(policy) = value.trim().parse::<u32>() else {
            return "unknown".to_string();
        };
        return match policy {
            0 => "SCHED_OTHER",
            1 => "SCHED_FIFO",
            2 => "SCHED_RR",
            3 => "SCHED_BATCH",
            5 => "SCHED_IDLE",
            6 => "SCHED_DEADLINE",
            other => return format!("SCHED_UNKNOWN({other})"),
        }
        .to_string();
    }
    "unknown".to_string()
}

/// Resolves the core the measurement runs on, in the canonical order of the
/// constitution §4.2: explicit `NAM_BENCH_CORE` → first isolated CPU allowed by
/// the affinity mask → first permitted CPU → `None` (GAP).
fn resolve_bench_core(
    allowed: &[usize],
    isolated: &[usize],
    override_core: Option<usize>,
) -> Option<usize> {
    override_core
        .or_else(|| isolated.iter().copied().find(|core| allowed.contains(core)))
        .or_else(|| allowed.first().copied())
}

/// Narrows the current process to `core` and verifies where it actually ran.
/// The affinity is exercised in-process (no shell `taskset`) so the report can
/// distinguish the requested, applied and observed core.
fn apply_bench_affinity(core: usize) -> Result<usize, String> {
    if core >= libc::CPU_SETSIZE as usize {
        return Err(format!("cpu_out_of_range:{core}"));
    }
    // SAFETY: `cpu_set_t` is a plain bitset; zeroing it and setting one valid
    // CPU index is the documented libc API. `sched_setaffinity` is called for
    // pid 0 (current thread) with the exact size of the set.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: `set` is an initialized bitset, `core` is bounds-checked above,
    // and both calls target pid 0 (current thread) with the set's exact size.
    unsafe {
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(core, &mut set);
        let rc = libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
        if rc != 0 {
            return Err(format!(
                "sched_setaffinity:{core}:{}",
                std::io::Error::last_os_error()
            ));
        }
        let observed = libc::sched_getcpu();
        if observed < 0 {
            return Err(format!("sched_getcpu:{}", std::io::Error::last_os_error()));
        }
        Ok(observed as usize)
    }
}

/// Applies host-side real-time hardening (memory locking, 64 KiB stack
/// prefaulting, and best-effort `SCHED_FIFO` priority 80 promotion) before
/// running the certification harness, emulating a calibrated DAW audio thread.
fn apply_bench_rt_hardening() {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `mlockall` locks current and future pages of this standalone
        // certification binary into RAM so page faults cannot skew p99 latency.
        // Failure is non-fatal (best-effort under unprivileged RLIMIT_MEMLOCK).
        unsafe {
            let _ = libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE);
        }
        let mut stack_probe = [0u8; 64 * 1024];
        for page in stack_probe.chunks_mut(4096) {
            // SAFETY: `page[0]` is a valid in-bounds byte of the local stack array.
            unsafe {
                std::ptr::write_volatile(&mut page[0], 1);
            }
        }
        std::hint::black_box(&stack_probe);

        // SAFETY: `sched_getscheduler(0)` queries the calling thread's policy;
        // `sched_setscheduler(0, SCHED_FIFO, &param)` promotes the calling thread
        // when `RLIMIT_RTPRIO` / `CAP_SYS_NICE` permits it.
        unsafe {
            let current = libc::sched_getscheduler(0);
            if current != libc::SCHED_FIFO && current != libc::SCHED_RR {
                let param = libc::sched_param { sched_priority: 80 };
                let _ = libc::sched_setscheduler(0, libc::SCHED_FIFO, &param);
            }
        }
    }
}

fn irqs_sharing_core(core: usize) -> Vec<String> {
    let Ok(entries) = fs::read_dir("/proc/irq") else {
        return Vec::new();
    };
    let mut sharing = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let dir = entry.path();
        let affinity = read_trim(&dir.join("effective_affinity_list").to_string_lossy())
            .filter(|s| !s.is_empty())
            .or_else(|| read_trim(&dir.join("smp_affinity_list").to_string_lossy()));
        let Some(affinity) = affinity else {
            continue;
        };
        if parse_cpu_list(&affinity).contains(&core) {
            sharing.push(name.to_string());
        }
    }
    sharing.sort_by_key(|irq| irq.parse::<usize>().unwrap_or(usize::MAX));
    sharing
}

fn probe_host() -> HostEnvironment {
    let cpus_allowed_list =
        read_proc_status_field("Cpus_allowed_list").unwrap_or_else(|| "unknown".to_string());
    let allowed = parse_cpu_list(&cpus_allowed_list);
    let cpu_isolated = read_trim("/sys/devices/system/cpu/isolated").unwrap_or_default();
    let isolated = parse_cpu_list(&cpu_isolated);
    let override_core = env::var("NAM_BENCH_CORE")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok());
    let bench_core = resolve_bench_core(&allowed, &isolated, override_core);

    let governor = bench_core
        .and_then(|core| {
            read_trim(&format!(
                "/sys/devices/system/cpu/cpu{core}/cpufreq/scaling_governor"
            ))
        })
        .or_else(|| read_trim("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"));
    let boost = read_trim("/sys/devices/system/cpu/cpufreq/boost").or_else(|| {
        read_trim("/sys/devices/system/cpu/intel_pstate/no_turbo")
            .map(|no_turbo| if no_turbo == "1" { "0" } else { "1" }.to_string())
    });

    let (irq_isolation, irqs_sharing_bench_core) = match bench_core {
        Some(core) => {
            let sharing = irqs_sharing_core(core);
            let status = if sharing.is_empty() {
                "isolated"
            } else {
                "shared"
            };
            (status.to_string(), sharing)
        }
        None => ("unknown".to_string(), Vec::new()),
    };

    HostEnvironment {
        bench_core,
        cpus_allowed_list,
        sched_policy: read_sched_policy(),
        governor,
        boost,
        cpu_online: read_trim("/sys/devices/system/cpu/online").unwrap_or_default(),
        cpu_isolated,
        irq_isolation,
        irqs_sharing_bench_core,
        ..HostEnvironment::default()
    }
}

/// Lists every way the host is less isolated than the certification policy in
/// `docs/testing.md` §5.4 assumes. An empty list means the measurement ran on a
/// dedicated core with real-time scheduling and no IRQ contention.
fn host_degradation_reasons(host: &HostEnvironment) -> Vec<String> {
    let mut reasons = Vec::new();

    match host.bench_core {
        Some(core) => {
            let isolated = parse_cpu_list(&host.cpu_isolated);
            if !isolated.contains(&core) {
                reasons.push(format!("bench_core_not_isolated:{core}"));
            }
            if host.irq_isolation == "shared" {
                reasons.push(format!(
                    "irq_isolation_shared:{}:{}",
                    core,
                    host.irqs_sharing_bench_core.len()
                ));
            }
        }
        None => reasons.push("bench_core_unresolved".to_string()),
    }

    if let Some(error) = &host.affinity_error {
        reasons.push(format!("affinity_apply_failed:{error}"));
    }
    if let (Some(requested), Some(observed)) = (host.bench_core, host.affinity_observed_core)
        && requested != observed
    {
        reasons.push(format!("affinity_observed_mismatch:{requested}:{observed}"));
    }

    if host.sched_policy != "SCHED_FIFO" && host.sched_policy != "SCHED_RR" {
        reasons.push(format!("sched_policy_not_realtime:{}", host.sched_policy));
    }
    match host.governor.as_deref() {
        Some("performance") => {}
        Some(other) => reasons.push(format!("governor_not_performance:{other}")),
        None => reasons.push("governor_unknown".to_string()),
    }
    let allowed = parse_cpu_list(&host.cpus_allowed_list);
    if !host.affinity_applied && allowed.len() > 1 {
        reasons.push(format!(
            "affinity_not_single_core:{}",
            host.cpus_allowed_list
        ));
    }

    reasons
}

fn evaluate_reference_series(
    measured: &[ScenarioPerfMetrics],
    reference: &ReferenceReport,
    p99_ratio_max: f64,
    p50_ratio_max: f64,
    p99_noise_floor_ns: f64,
    reference_path: &str,
    reference_sha256: &str,
) -> Result<ReferenceCheck, String> {
    if !p99_ratio_max.is_finite() || p99_ratio_max <= 0.0 {
        return Err(format!(
            "invalid --p99-ratio-max '{p99_ratio_max}': must be a positive finite number"
        ));
    }
    if !p50_ratio_max.is_finite() || p50_ratio_max <= 0.0 {
        return Err(format!(
            "invalid --p50-ratio-max '{p50_ratio_max}': must be a positive finite number"
        ));
    }
    if !p99_noise_floor_ns.is_finite() || p99_noise_floor_ns < 0.0 {
        return Err(format!(
            "invalid --p99-noise-floor-us '{p99_noise_floor_ns}': must be a non-negative finite number"
        ));
    }
    let reference_scenarios: BTreeMap<&str, &ReferenceScenario> = reference
        .scenarios
        .iter()
        .map(|scenario| (scenario.name.as_str(), scenario))
        .collect();

    let mut comparisons = Vec::with_capacity(measured.len());
    let mut worst_p99_ratio: Option<f64> = None;
    let mut worst_p50_ratio = 0.0_f64;
    let mut missing_p99_scenarios = Vec::new();
    let mut p99_failures = Vec::new();

    for m in measured {
        let Some(ref_sc) = reference_scenarios.get(m.name.as_str()) else {
            return Err(format!(
                "reference report '{reference_path}' has no scenario '{}'",
                m.name
            ));
        };

        // p50 comparison (informative)
        let ref_p50 = ref_sc.p50_ns.filter(|v| v.is_finite() && *v > 0.0);
        let p50_ratio = ref_p50.map(|rp| {
            let r = m.p50_ns / rp;
            worst_p50_ratio = worst_p50_ratio.max(r);
            r
        });

        // max comparison (informative)
        let ref_max = ref_sc.max_ns.filter(|v| v.is_finite() && *v > 0.0);
        let max_ratio = ref_max.map(|rm| m.max_ns / rm);

        // p99 comparison (tail certification gate)
        let ref_p99 = ref_sc.p99_ns.filter(|v| v.is_finite() && *v > 0.0);
        let (p99_ratio, p99_passed) = match ref_p99 {
            Some(rp) => {
                let r = m.p99_ns / rp;
                let delta_ns = m.p99_ns - rp;
                let passed = r <= p99_ratio_max || delta_ns <= p99_noise_floor_ns;
                if delta_ns > p99_noise_floor_ns {
                    worst_p99_ratio = Some(worst_p99_ratio.map_or(r, |w| w.max(r)));
                }
                if !passed {
                    p99_failures.push((m.name.clone(), r));
                }
                (Some(r), Some(passed))
            }
            None => {
                missing_p99_scenarios.push(m.name.clone());
                (None, None)
            }
        };

        comparisons.push(ReferenceComparison {
            name: m.name.clone(),
            reference_p50_ns: ref_p50,
            measured_p50_ns: m.p50_ns,
            p50_ratio,
            reference_p99_ns: ref_p99,
            measured_p99_ns: m.p99_ns,
            p99_ratio,
            reference_max_ns: ref_max,
            measured_max_ns: m.max_ns,
            max_ratio,
            p99_passed,
        });
    }

    if worst_p99_ratio.is_none() {
        worst_p99_ratio = comparisons
            .iter()
            .filter_map(|c| c.p99_ratio)
            .reduce(f64::max);
    }

    let (status, passed, inconclusive_reason) = if !missing_p99_scenarios.is_empty() {
        (
            "INCONCLUSIVE".to_string(),
            false,
            Some(format!(
                "reference report '{reference_path}' lacks p99 tail statistics for scenarios: {}",
                missing_p99_scenarios.join(", ")
            )),
        )
    } else if !p99_failures.is_empty() {
        ("FAIL".to_string(), false, None)
    } else {
        ("PASS".to_string(), true, None)
    };

    Ok(ReferenceCheck {
        reference_path: reference_path.to_string(),
        reference_sha256: reference_sha256.to_string(),
        status,
        p99_ratio_max,
        p50_ratio_max,
        p99_noise_floor_ns,
        worst_p99_ratio,
        worst_p50_ratio,
        passed,
        inconclusive_reason,
        comparisons,
    })
}

fn load_reference_check(
    reference_path: &Path,
    measured: &[ScenarioPerfMetrics],
    p99_ratio_max: f64,
    p50_ratio_max: f64,
    p99_noise_floor_ns: f64,
    artifact_sha256: &str,
) -> Result<ReferenceCheck, String> {
    let bytes = fs::read(reference_path).map_err(|e| {
        format!(
            "cannot read reference report {}: {e}",
            reference_path.display()
        )
    })?;
    let reference: ReferenceReport = serde_json::from_slice(&bytes)
        .map_err(|e| format!("invalid reference report {}: {e}", reference_path.display()))?;
    let reference_sha = reference
        .clap_artifact_sha256
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            format!(
                "reference_identity_missing: reference report {} has no clap_artifact_sha256",
                reference_path.display()
            )
        })?;
    if reference_sha != artifact_sha256 {
        return Err(format!(
            "reference_identity_mismatch: reference artifact SHA-256 ({reference_sha}) != current artifact ({artifact_sha256})"
        ));
    }
    let reference_sha256: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    evaluate_reference_series(
        measured,
        &reference,
        p99_ratio_max,
        p50_ratio_max,
        p99_noise_floor_ns,
        &reference_path.to_string_lossy(),
        &reference_sha256,
    )
}

/// Fixture root for the model assets the gate needs.
///
/// Anchored to this crate's manifest directory so resolution is independent of
/// the process working directory (a plain `tests/...` relative path silently
/// depended on being launched from the crate root). `NAM_FIXTURES_DIR` overrides
/// the root for operators running the gate from an out-of-tree checkout; no
/// sibling-repository path is ever consulted, keeping the gate self-contained.
fn fixtures_dir() -> PathBuf {
    if let Ok(dir) = env::var("NAM_FIXTURES_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/models")
}

fn resolve_model_fixture(filename: &str) -> Option<PathBuf> {
    let path = fixtures_dir().join(filename);
    path.exists().then_some(path)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScenarioEventMode {
    None,
    GainAutomation,
    BypassToggle,
    ModelSwap,
}

impl ScenarioEventMode {
    fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::GainAutomation => "automation",
            Self::BypassToggle => "bypass",
            Self::ModelSwap => "model_swap",
        }
    }
}

fn resolve_fixture_asset(
    fixture: Option<&str>,
    kind: &str,
) -> Result<(Option<std::path::PathBuf>, Option<String>), String> {
    let Some(fixture) = fixture else {
        return Ok((None, None));
    };
    let fixture_path = resolve_model_fixture(fixture)
        .ok_or_else(|| format!("fixture_missing: Required {kind} '{fixture}' not found"))?;
    let abs_path = std::fs::canonicalize(&fixture_path).map_err(|e| e.to_string())?;
    let hash = sha256_of(&abs_path).map_err(|e| format!("fixture_hash_failed:{fixture}:{e}"))?;
    Ok((Some(abs_path), Some(hash)))
}

fn build_state_payload(
    fixture_file: Option<&str>,
    cab_fixture_file: Option<&str>,
    oversample: OversampleFactor,
) -> Result<Vec<u8>, String> {
    let (model_path, model_hash) = resolve_fixture_asset(fixture_file, "fixture")?;
    let (ir_path, ir_hash) = resolve_fixture_asset(cab_fixture_file, "cabinet fixture")?;

    let mut params = ProcessingParams::default();
    params.oversample = oversample;
    params.activation_precision = ActivationPrecision::Standard;
    params.model_path = model_path;
    params.model_hash = model_hash;
    params.ir_path = ir_path;
    params.ir_hash = ir_hash;
    serde_json::to_vec(&params).map_err(|e| e.to_string())
}

fn oversample_label(factor: OversampleFactor) -> &'static str {
    match factor {
        OversampleFactor::Off => "Off",
        OversampleFactor::X2 => "2x",
        OversampleFactor::X4 => "4x",
        _ => "unknown",
    }
}

fn cabinet_label(cab_fixture_file: Option<&str>) -> &'static str {
    match cab_fixture_file {
        None => "none",
        Some(f) if f.ends_with(".wav") => "WAV",
        Some(_) => "NAM",
    }
}

struct ScenarioConfig {
    name: &'static str,
    topology: &'static str,
    fixture_file: Option<&'static str>,
    cab_fixture_file: Option<&'static str>,
    stereo: bool,
    oversample: OversampleFactor,
    event_mode: ScenarioEventMode,
    block_size: usize,
    sample_rate: f64,
}

fn validate_process_result(
    phase: &str,
    result: Result<ProcessStatus, PluginInstanceError>,
    process_errors: &mut usize,
    status_rejected: &mut usize,
) -> Result<(), String> {
    match result {
        Ok(ProcessStatus::Continue) => Ok(()),
        Ok(other) => {
            *status_rejected += 1;
            Err(format!("status_rejected:{phase}:{other:?}"))
        }
        Err(err) => {
            *process_errors += 1;
            Err(format!("process_error:{phase}:{err:?}"))
        }
    }
}

fn evaluate_probe_output(
    out_l: &[f32],
    out_r: &[f32],
    expects_model: bool,
) -> Result<(f64, f64), String> {
    let mut max_abs_diff = 0.0_f64;
    let mut sum_sq_diff = 0.0_f64;
    let mut sample_count = 0usize;

    for (ch_name, ch) in [("L", out_l), ("R", out_r)] {
        for (idx, &sample) in ch.iter().enumerate() {
            if !sample.is_finite() {
                return Err(format!(
                    "probe_non_finite_output: channel={ch_name} sample={idx} value={sample}"
                ));
            }
            let diff = (sample as f64 - PROBE_INPUT_SAMPLE as f64).abs();
            if diff > max_abs_diff {
                max_abs_diff = diff;
            }
            sum_sq_diff += diff * diff;
            sample_count += 1;
        }
    }

    let rms_diff = if sample_count > 0 {
        (sum_sq_diff / sample_count as f64).sqrt()
    } else {
        0.0
    };

    if expects_model {
        if max_abs_diff <= MODEL_PROBE_MIN_DIFF {
            return Err(format!(
                "probe_activation_failed: model output matched no-model reference \
                 (max_abs_diff={max_abs_diff:.6e}, rms_diff={rms_diff:.6e}, threshold={MODEL_PROBE_MIN_DIFF:.6e})"
            ));
        }
    } else if max_abs_diff > PASSTHROUGH_PROBE_MAX_DIFF {
        return Err(format!(
            "probe_passthrough_mismatch: no-model output deviated from reference \
             (max_abs_diff={max_abs_diff:.6e}, rms_diff={rms_diff:.6e}, max_allowed={PASSTHROUGH_PROBE_MAX_DIFF:.6e})"
        ));
    }

    Ok((max_abs_diff, rms_diff))
}

fn evaluate_cab_probe_diff(
    wet_l: &[f32],
    wet_r: &[f32],
    bypassed_l: &[f32],
    bypassed_r: &[f32],
) -> Result<f64, String> {
    let mut max_abs_diff = 0.0_f64;
    for (ch_name, wet, byp) in [("L", wet_l, bypassed_l), ("R", wet_r, bypassed_r)] {
        for (idx, (&w, &b)) in wet.iter().zip(byp.iter()).enumerate() {
            if !b.is_finite() {
                return Err(format!(
                    "cab_probe_non_finite_output: channel={ch_name} sample={idx} value={b}"
                ));
            }
            let diff = (w as f64 - b as f64).abs();
            if diff > max_abs_diff {
                max_abs_diff = diff;
            }
        }
    }
    if max_abs_diff <= CAB_PROBE_MIN_DIFF {
        return Err(format!(
            "cab_probe_activation_failed: cabinet-active output matched cabinet-bypassed output \
             (max_abs_diff={max_abs_diff:.6e}, threshold={CAB_PROBE_MIN_DIFF:.6e})"
        ));
    }
    Ok(max_abs_diff)
}

#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "bundles pre-allocated CLAP host audio/event buffers across warmup, probe, and timed loops"
)]
fn run_process_block(
    phase: &str,
    processor: &mut clack_host::process::StartedPluginAudioProcessor<PerfHost>,
    in_l: &mut [f32],
    in_r: &mut [f32],
    out_l: &mut [f32],
    out_r: &mut [f32],
    input_ports: &mut AudioPorts,
    output_ports: &mut AudioPorts,
    input_events_buffer: Option<&EventBuffer>,
    output_events_buffer: &mut EventBuffer,
    process_errors: &mut usize,
    status_rejected: &mut usize,
) -> Result<std::time::Duration, String> {
    let input_events = match input_events_buffer {
        Some(buf) => InputEvents::from_buffer(buf),
        None => InputEvents::empty(),
    };
    let mut output_events = OutputEvents::from_buffer(output_events_buffer);

    let mut input_channels = [in_l, in_r];
    let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_input_only(
            input_channels.iter_mut().map(InputChannel::constant),
        ),
    }]);

    let output_channels = [out_l, out_r];
    let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
    }]);

    let start = Instant::now();
    let result = processor.process(
        &input_audio,
        &mut output_audio,
        &input_events,
        &mut output_events,
        None,
        None,
    );
    let elapsed = start.elapsed();
    validate_process_result(phase, result, process_errors, status_rejected)?;
    Ok(elapsed)
}

fn measure_scenario(
    clap_path: &Path,
    config: &ScenarioConfig,
) -> Result<ScenarioPerfMetrics, String> {
    // 1. Dynamic plugin loading from the target distributed .clap
    // SAFETY: Loading dynamic CLAP plugin from certified artifact path.
    let entry = unsafe {
        PluginEntry::load(clap_path).map_err(|e| {
            format!(
                "Failed to load plugin entry from {}: {e:?}",
                clap_path.display()
            )
        })?
    };

    let host_info = HostInfo::new(
        "NAM-Plug Perf Guard Host",
        "Fabio Lima",
        "https://github.com/fabiohl/NAM-Plug",
        "0.1.0",
    )
    .map_err(|e| format!("Failed to create HostInfo: {e:?}"))?;

    let mut plugin_instance = PluginInstance::<PerfHost>::new(
        |_| PerfHostShared,
        |_| (),
        &entry,
        c"br.eti.fabiolima.nam-plug",
        &host_info,
    )
    .map_err(|e| format!("Failed to create plugin instance: {e:?}"))?;

    if config.stereo {
        let ports_cfg = plugin_instance
            .plugin_handle()
            .get_extension::<PluginAudioPortsConfig>()
            .ok_or_else(|| {
                "audio_ports_config_missing: PluginAudioPortsConfig extension unavailable"
                    .to_string()
            })?;
        let handle = plugin_instance.plugin_handle();
        ports_cfg
            .select(&handle, ClapId::new(CONFIG_ID_STEREO))
            .map_err(|e| format!("audio_ports_config_select_failed: {e:?}"))?;
    }

    let expects_model = config.fixture_file.is_some();
    let expects_cab = config.cab_fixture_file.is_some();
    let state_bytes = build_state_payload(
        config.fixture_file,
        config.cab_fixture_file,
        config.oversample,
    )?;
    let swap_state_bytes = if config.event_mode == ScenarioEventMode::ModelSwap {
        Some(build_state_payload(
            Some("a2_example.nam"),
            config.cab_fixture_file,
            config.oversample,
        )?)
    } else {
        None
    };

    match plugin_instance
        .plugin_handle()
        .get_extension::<PluginState>()
    {
        Some(state_ext) => {
            let handle = plugin_instance.plugin_handle();
            state_ext
                .load(&handle, &mut state_bytes.as_slice())
                .map_err(|e| format!("state_load_failed: {e:?}"))?;
        }
        None if expects_model || expects_cab => {
            return Err("state_extension_missing: PluginState extension unavailable".to_string());
        }
        None => {}
    }

    // 3. Activate audio processor
    let audio_config = PluginAudioConfiguration {
        sample_rate: config.sample_rate,
        min_frames_count: config.block_size as u32,
        max_frames_count: config.block_size as u32,
    };

    let stopped_processor = plugin_instance
        .activate(|_, _| (), audio_config)
        .map_err(|e| format!("Failed to activate plugin: {e:?}"))?;

    let mut started_processor = stopped_processor
        .start_processing()
        .map_err(|e| format!("Failed to start processing: {e:?}"))?;

    let n = config.block_size;
    let mut in_l = vec![PROBE_INPUT_SAMPLE; n];
    let mut in_r = vec![PROBE_INPUT_SAMPLE; n];
    let mut out_l = vec![0.0f32; n];
    let mut out_r = vec![0.0f32; n];

    let mut input_ports = AudioPorts::with_capacity(2, 1);
    let mut output_ports = AudioPorts::with_capacity(2, 1);
    let mut output_events_buffer = EventBuffer::with_capacity(10);

    let mut process_errors = 0usize;
    let mut status_rejected = 0usize;

    // Warmup cycles (400 iterations to settle branch predictors, caches, and P-state)
    for _ in 0..400 {
        run_process_block(
            "warmup",
            &mut started_processor,
            &mut in_l,
            &mut in_r,
            &mut out_l,
            &mut out_r,
            &mut input_ports,
            &mut output_ports,
            None,
            &mut output_events_buffer,
            &mut process_errors,
            &mut status_rejected,
        )?;
    }

    // Activation probe block (outside timed interval): proves the loaded model
    // actually transformed audio (or passthrough stayed bit-faithful) and that
    // all output channels remain finite.
    let (probe_max_abs_diff, probe_rms_diff) = {
        out_l.fill(f32::NAN);
        out_r.fill(f32::NAN);
        run_process_block(
            "probe",
            &mut started_processor,
            &mut in_l,
            &mut in_r,
            &mut out_l,
            &mut out_r,
            &mut input_ports,
            &mut output_ports,
            None,
            &mut output_events_buffer,
            &mut process_errors,
            &mut status_rejected,
        )?;
        evaluate_probe_output(&out_l, &out_r, expects_model)?
    };
    let probe_passed = true;
    let cab_wet_l = expects_cab.then(|| out_l.clone());
    let cab_wet_r = expects_cab.then(|| out_r.clone());

    let (ev_a, ev_b) = build_timed_event_buffers(config.event_mode, n);

    // Benchmark measurement (1000 iterations, pre-allocated and prefaulted)
    const MEASUREMENT_CYCLES: usize = 1000;
    let mut latencies_ns = vec![0.0_f64; MEASUREMENT_CYCLES];
    let swap_state_ext = (config.event_mode == ScenarioEventMode::ModelSwap)
        .then(|| {
            plugin_instance
                .plugin_handle()
                .get_extension::<PluginState>()
        })
        .flatten();
    if config.event_mode == ScenarioEventMode::ModelSwap && swap_state_ext.is_none() {
        return Err(
            "state_extension_missing: PluginState extension unavailable for ModelSwap".to_string(),
        );
    }
    let mut swap_count = 0usize;

    for (idx, slot) in latencies_ns.iter_mut().enumerate() {
        let do_swap =
            config.event_mode == ScenarioEventMode::ModelSwap && idx > 0 && idx.is_multiple_of(200);
        if do_swap {
            let mut next_slice: &[u8] = if swap_count.is_multiple_of(2) {
                swap_state_bytes.as_deref().unwrap_or(&state_bytes)
            } else {
                &state_bytes
            };
            let handle = plugin_instance.plugin_handle();
            swap_state_ext
                .as_ref()
                .expect("checked above")
                .load(&handle, &mut next_slice)
                .map_err(|e| format!("model_swap_load_failed_at_{idx}: {e:?}"))?;
            swap_count += 1;
        }

        let ev_buf = match config.event_mode {
            ScenarioEventMode::None | ScenarioEventMode::ModelSwap => None,
            ScenarioEventMode::GainAutomation | ScenarioEventMode::BypassToggle => {
                if idx % 2 == 0 {
                    Some(&ev_a)
                } else {
                    Some(&ev_b)
                }
            }
        };
        let elapsed = run_process_block(
            "measurement",
            &mut started_processor,
            &mut in_l,
            &mut in_r,
            &mut out_l,
            &mut out_r,
            &mut input_ports,
            &mut output_ports,
            ev_buf,
            &mut output_events_buffer,
            &mut process_errors,
            &mut status_rejected,
        )?;
        *slot = elapsed.as_nanos() as f64;

        if do_swap {
            plugin_instance.call_on_main_thread_callback();
        }
    }

    let (cab_probe_passed, cab_probe_max_abs_diff) =
        if let (Some(wet_l), Some(wet_r)) = (cab_wet_l, cab_wet_r) {
            let mut cab_byp_ev = EventBuffer::with_capacity(2);
            let ev = ParamValueEvent::new(0, ClapId::new(PARAM_CAB_BYPASS), Pckn::match_all(), 1.0);
            cab_byp_ev.push(&ev);
            let settle_blocks = 128usize.div_ceil(n).max(4);
            for block_idx in 0..settle_blocks {
                let ev_ref = (block_idx == 0).then_some(&cab_byp_ev);
                run_process_block(
                    "cab_probe_settle",
                    &mut started_processor,
                    &mut in_l,
                    &mut in_r,
                    &mut out_l,
                    &mut out_r,
                    &mut input_ports,
                    &mut output_ports,
                    ev_ref,
                    &mut output_events_buffer,
                    &mut process_errors,
                    &mut status_rejected,
                )?;
            }
            out_l.fill(f32::NAN);
            out_r.fill(f32::NAN);
            run_process_block(
                "cab_probe",
                &mut started_processor,
                &mut in_l,
                &mut in_r,
                &mut out_l,
                &mut out_r,
                &mut input_ports,
                &mut output_ports,
                None,
                &mut output_events_buffer,
                &mut process_errors,
                &mut status_rejected,
            )?;
            let diff = evaluate_cab_probe_diff(&wet_l, &wet_r, &out_l, &out_r)?;
            (true, diff)
        } else {
            (true, 0.0)
        };

    let stopped_processor = started_processor.stop_processing();
    plugin_instance.deactivate(stopped_processor);

    if config.event_mode == ScenarioEventMode::ModelSwap && swap_count != 4 {
        return Err(format!(
            "model_swap_count_mismatch: expected 4 swaps across 1000 blocks, observed {swap_count}"
        ));
    }

    // Compute statistics
    latencies_ns.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let count = latencies_ns.len();
    let sum: f64 = latencies_ns.iter().sum();
    let mean_ns = sum / count as f64;

    let p50_idx = ((count as f64) * 0.50).min((count - 1) as f64) as usize;
    let p95_idx = ((count as f64) * 0.95).min((count - 1) as f64) as usize;
    let p99_idx = ((count as f64) * 0.99).min((count - 1) as f64) as usize;

    let p50_ns = latencies_ns[p50_idx];
    let p95_ns = latencies_ns[p95_idx];
    let p99_ns = latencies_ns[p99_idx];
    let max_ns = *latencies_ns.last().unwrap_or(&0.0);

    let variance: f64 = latencies_ns
        .iter()
        .map(|v| (v - mean_ns).powi(2))
        .sum::<f64>()
        / count as f64;
    let stddev_ns = variance.sqrt();
    let ns_per_sample = mean_ns / n as f64;

    // Real-time deadline budget (e.g. at 48kHz, 64 samples = 1,333,333 ns) and
    // the certification gate derived from it: p99 must stay below 85% of the
    // budget, matching the audio callback's DSP overload threshold.
    let deadline_budget_ns = (n as f64 / config.sample_rate) * 1_000_000_000.0;
    let gate_threshold_ns = deadline_budget_ns * DEADLINE_GATE_FRACTION;
    let deadline_margin_percent = ((deadline_budget_ns - p99_ns) / deadline_budget_ns) * 100.0;
    let gate_headroom_percent = ((gate_threshold_ns - p99_ns) / gate_threshold_ns) * 100.0;
    let deadline_passed = p99_ns < gate_threshold_ns
        && process_errors == 0
        && status_rejected == 0
        && probe_passed
        && cab_probe_passed;

    Ok(ScenarioPerfMetrics {
        name: config.name.to_string(),
        topology: config.topology.to_string(),
        channels: if config.stereo {
            "stereo".to_string()
        } else {
            "mono".to_string()
        },
        sample_rate: config.sample_rate,
        block_size: config.block_size,
        oversample: oversample_label(config.oversample).to_string(),
        cabinet: cabinet_label(config.cab_fixture_file).to_string(),
        events: config.event_mode.label().to_string(),
        iterations: count,
        p50_ns,
        p95_ns,
        p99_ns,
        max_ns,
        mean_ns,
        stddev_ns,
        ns_per_sample,
        deadline_budget_ns,
        gate_threshold_ns,
        deadline_margin_percent,
        gate_headroom_percent,
        deadline_passed,
        process_errors,
        status_rejected,
        probe_passed,
        probe_max_abs_diff,
        probe_rms_diff,
        cab_probe_passed,
        cab_probe_max_abs_diff,
    })
}

fn build_timed_event_buffers(mode: ScenarioEventMode, n: usize) -> (EventBuffer, EventBuffer) {
    let mut ev_a = EventBuffer::with_capacity(4);
    let mut ev_b = EventBuffer::with_capacity(4);
    match mode {
        ScenarioEventMode::None | ScenarioEventMode::ModelSwap => {}
        ScenarioEventMode::GainAutomation => {
            let mid = (n / 2) as u32;
            ev_a.push(&ParamValueEvent::new(
                0,
                ClapId::new(PARAM_INPUT_GAIN),
                Pckn::match_all(),
                -3.0,
            ));
            ev_a.push(&ParamValueEvent::new(
                mid,
                ClapId::new(PARAM_INPUT_GAIN),
                Pckn::match_all(),
                3.0,
            ));
            ev_b.push(&ParamValueEvent::new(
                0,
                ClapId::new(PARAM_INPUT_GAIN),
                Pckn::match_all(),
                0.0,
            ));
            ev_b.push(&ParamValueEvent::new(
                mid,
                ClapId::new(PARAM_INPUT_GAIN),
                Pckn::match_all(),
                -1.5,
            ));
        }
        ScenarioEventMode::BypassToggle => {
            ev_a.push(&ParamValueEvent::new(
                0,
                ClapId::new(PARAM_BYPASS),
                Pckn::match_all(),
                1.0,
            ));
            ev_b.push(&ParamValueEvent::new(
                0,
                ClapId::new(PARAM_BYPASS),
                Pckn::match_all(),
                0.0,
            ));
        }
    }
    (ev_a, ev_b)
}

fn usage() -> ! {
    eprintln!(
        "Usage: nam_perf_guard certify --clap <path-to-.clap> [--out <output.json>] \
         [--reference <previous-report.json>] [--p99-ratio-max <ratio>] [--p50-ratio-max <ratio>] \
         [--p99-noise-floor-us <microseconds>]"
    );
    std::process::exit(1);
}

fn main() -> ExitCode {
    NamLogger::init(LoggerConfig::new(log::LevelFilter::Info)).ok();

    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some("certify") {
        usage();
    }

    let mut clap_path: Option<PathBuf> = None;
    let mut out_path: Option<PathBuf> = None;
    let mut reference_path: Option<PathBuf> = None;
    let mut p99_ratio_max = 1.25_f64;
    let mut p50_ratio_max = 1.5_f64;
    let mut p99_noise_floor_ns = DEFAULT_NOISE_FLOOR_NS;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--clap" => {
                i += 1;
                if i >= args.len() {
                    usage();
                }
                clap_path = Some(PathBuf::from(&args[i]));
            }
            "--out" => {
                i += 1;
                if i >= args.len() {
                    usage();
                }
                out_path = Some(PathBuf::from(&args[i]));
            }
            "--reference" => {
                i += 1;
                if i >= args.len() {
                    usage();
                }
                reference_path = Some(PathBuf::from(&args[i]));
            }
            "--p99-ratio-max" => {
                i += 1;
                if i >= args.len() {
                    usage();
                }
                p99_ratio_max = match args[i].parse::<f64>() {
                    Ok(value) => value,
                    Err(_) => {
                        eprintln!(
                            "❌ FATAL: invalid --p99-ratio-max '{}': expected a number",
                            args[i]
                        );
                        return ExitCode::from(1);
                    }
                };
            }
            "--p50-ratio-max" => {
                i += 1;
                if i >= args.len() {
                    usage();
                }
                p50_ratio_max = match args[i].parse::<f64>() {
                    Ok(value) => value,
                    Err(_) => {
                        eprintln!(
                            "❌ FATAL: invalid --p50-ratio-max '{}': expected a number",
                            args[i]
                        );
                        return ExitCode::from(1);
                    }
                };
            }
            "--p99-noise-floor-us" => {
                i += 1;
                if i >= args.len() {
                    usage();
                }
                let us = match args[i].parse::<f64>() {
                    Ok(value) if value.is_finite() && value >= 0.0 => value,
                    _ => {
                        eprintln!(
                            "❌ FATAL: invalid --p99-noise-floor-us '{}': expected a non-negative number",
                            args[i]
                        );
                        return ExitCode::from(1);
                    }
                };
                p99_noise_floor_ns = us * 1000.0;
            }
            _ => usage(),
        }
        i += 1;
    }

    let clap_target = clap_path.unwrap_or_else(|| usage());
    if !clap_target.exists() {
        eprintln!(
            "❌ FATAL: Distributed CLAP artifact not found: {}",
            clap_target.display()
        );
        return ExitCode::from(1);
    }

    let clap_sha = match sha256_of(&clap_target) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("❌ FATAL: Failed to hash CLAP artifact: {err}");
            return ExitCode::from(1);
        }
    };

    println!("========================================================================");
    println!("   NAM-Plug Distributed Artifact Performance Certification Gate       ");
    println!("========================================================================");
    println!("CLAP Artifact: {}", clap_target.display());
    println!("SHA-256:       {}", clap_sha);
    println!("------------------------------------------------------------------------");

    let scenarios = [
        ScenarioConfig {
            name: "WaveNet_A1_64_samp",
            topology: "WaveNet A1 Standard",
            fixture_file: Some("wavenet_a1_standard.nam"),
            cab_fixture_file: None,
            stereo: false,
            oversample: OversampleFactor::Off,
            event_mode: ScenarioEventMode::None,
            block_size: 64,
            sample_rate: 48000.0,
        },
        ScenarioConfig {
            name: "WaveNet_A1_128_samp",
            topology: "WaveNet A1 Standard",
            fixture_file: Some("wavenet_a1_standard.nam"),
            cab_fixture_file: None,
            stereo: false,
            oversample: OversampleFactor::Off,
            event_mode: ScenarioEventMode::None,
            block_size: 128,
            sample_rate: 48000.0,
        },
        ScenarioConfig {
            name: "WaveNet_A2_64_samp",
            topology: "WaveNet A2 / Slimmable",
            fixture_file: Some("a2_example.nam"),
            cab_fixture_file: None,
            stereo: false,
            oversample: OversampleFactor::Off,
            event_mode: ScenarioEventMode::None,
            block_size: 64,
            sample_rate: 48000.0,
        },
        ScenarioConfig {
            name: "LSTM_64_samp",
            topology: "LSTM 1x3",
            fixture_file: Some("lstm.nam"),
            cab_fixture_file: None,
            stereo: false,
            oversample: OversampleFactor::Off,
            event_mode: ScenarioEventMode::None,
            block_size: 64,
            sample_rate: 48000.0,
        },
        ScenarioConfig {
            name: "Passthrough_Overhead_64_samp",
            topology: "CLAP Overhead (No Model)",
            fixture_file: None,
            cab_fixture_file: None,
            stereo: false,
            oversample: OversampleFactor::Off,
            event_mode: ScenarioEventMode::None,
            block_size: 64,
            sample_rate: 48000.0,
        },
        ScenarioConfig {
            name: "Passthrough_Overhead_128_samp",
            topology: "CLAP Overhead (No Model)",
            fixture_file: None,
            cab_fixture_file: None,
            stereo: false,
            oversample: OversampleFactor::Off,
            event_mode: ScenarioEventMode::None,
            block_size: 128,
            sample_rate: 48000.0,
        },
        ScenarioConfig {
            name: "WaveNet_A1_Stereo_64_samp",
            topology: "WaveNet A1 Standard",
            fixture_file: Some("wavenet_a1_standard.nam"),
            cab_fixture_file: None,
            stereo: true,
            oversample: OversampleFactor::Off,
            event_mode: ScenarioEventMode::None,
            block_size: 64,
            sample_rate: 48000.0,
        },
        ScenarioConfig {
            name: "WaveNet_A1_Stereo_32_samp",
            topology: "WaveNet A1 Standard",
            fixture_file: Some("wavenet_a1_standard.nam"),
            cab_fixture_file: None,
            stereo: true,
            oversample: OversampleFactor::Off,
            event_mode: ScenarioEventMode::None,
            block_size: 32,
            sample_rate: 48000.0,
        },
        ScenarioConfig {
            name: "WaveNet_A1_44100_64_samp",
            topology: "WaveNet A1 Standard",
            fixture_file: Some("wavenet_a1_standard.nam"),
            cab_fixture_file: None,
            stereo: false,
            oversample: OversampleFactor::Off,
            event_mode: ScenarioEventMode::None,
            block_size: 64,
            sample_rate: 44100.0,
        },
        ScenarioConfig {
            name: "WaveNet_A1_96000_64_samp",
            topology: "WaveNet A1 Standard",
            fixture_file: Some("wavenet_a1_standard.nam"),
            cab_fixture_file: None,
            stereo: false,
            oversample: OversampleFactor::Off,
            event_mode: ScenarioEventMode::None,
            block_size: 64,
            sample_rate: 96000.0,
        },
        ScenarioConfig {
            name: "WaveNet_A1_Stereo_OS2x_64_samp",
            topology: "WaveNet A1 Standard",
            fixture_file: Some("wavenet_a1_standard.nam"),
            cab_fixture_file: None,
            stereo: true,
            oversample: OversampleFactor::X2,
            event_mode: ScenarioEventMode::None,
            block_size: 64,
            sample_rate: 48000.0,
        },
        ScenarioConfig {
            name: "WaveNet_A1_Stereo_OS4x_64_samp",
            topology: "WaveNet A1 Standard",
            fixture_file: Some("wavenet_a1_standard.nam"),
            cab_fixture_file: None,
            stereo: true,
            oversample: OversampleFactor::X4,
            event_mode: ScenarioEventMode::None,
            block_size: 64,
            sample_rate: 48000.0,
        },
        ScenarioConfig {
            name: "WaveNet_A1_Stereo_CabWAV_64_samp",
            topology: "WaveNet A1 + CabSim WAV",
            fixture_file: Some("wavenet_a1_standard.nam"),
            cab_fixture_file: Some("cabsim_ir_pgo.wav"),
            stereo: true,
            oversample: OversampleFactor::Off,
            event_mode: ScenarioEventMode::None,
            block_size: 64,
            sample_rate: 48000.0,
        },
        ScenarioConfig {
            name: "WaveNet_A1_Stereo_CabWAV_128_samp",
            topology: "WaveNet A1 + CabSim WAV",
            fixture_file: Some("wavenet_a1_standard.nam"),
            cab_fixture_file: Some("cabsim_ir_pgo.wav"),
            stereo: true,
            oversample: OversampleFactor::Off,
            event_mode: ScenarioEventMode::None,
            block_size: 128,
            sample_rate: 48000.0,
        },
        ScenarioConfig {
            name: "WaveNet_A1_Stereo_CabNAM_64_samp",
            topology: "WaveNet A1 + Cab NAM (LSTM 1x3)",
            fixture_file: Some("wavenet_a1_standard.nam"),
            cab_fixture_file: Some("lstm.nam"),
            stereo: true,
            oversample: OversampleFactor::Off,
            event_mode: ScenarioEventMode::None,
            block_size: 64,
            sample_rate: 48000.0,
        },
        ScenarioConfig {
            name: "WaveNet_A1_Stereo_CabNAM_128_samp",
            topology: "WaveNet A1 + Cab NAM (LSTM 1x3)",
            fixture_file: Some("wavenet_a1_standard.nam"),
            cab_fixture_file: Some("lstm.nam"),
            stereo: true,
            oversample: OversampleFactor::Off,
            event_mode: ScenarioEventMode::None,
            block_size: 128,
            sample_rate: 48000.0,
        },
        ScenarioConfig {
            name: "WaveNet_A1_Stereo_Automation_64_samp",
            topology: "WaveNet A1 Standard",
            fixture_file: Some("wavenet_a1_standard.nam"),
            cab_fixture_file: None,
            stereo: true,
            oversample: OversampleFactor::Off,
            event_mode: ScenarioEventMode::GainAutomation,
            block_size: 64,
            sample_rate: 48000.0,
        },
        ScenarioConfig {
            name: "WaveNet_A1_Stereo_BypassTransition_64_samp",
            topology: "WaveNet A1 Standard",
            fixture_file: Some("wavenet_a1_standard.nam"),
            cab_fixture_file: None,
            stereo: true,
            oversample: OversampleFactor::Off,
            event_mode: ScenarioEventMode::BypassToggle,
            block_size: 64,
            sample_rate: 48000.0,
        },
        ScenarioConfig {
            name: "WaveNet_A1_Stereo_ModelSwap_64_samp",
            topology: "WaveNet A1 Standard",
            fixture_file: Some("wavenet_a1_standard.nam"),
            cab_fixture_file: None,
            stereo: true,
            oversample: OversampleFactor::Off,
            event_mode: ScenarioEventMode::ModelSwap,
            block_size: 64,
            sample_rate: 48000.0,
        },
    ];

    let mut results = Vec::new();
    let mut all_deadlines_passed = true;
    let mut environment_stable = true;

    // Real-time hardening and CPU affinity are applied before the first
    // measurement, not merely recorded: the report must distinguish the
    // requested from the observed core and reflect the active RT policy.
    apply_bench_rt_hardening();
    let mut host = probe_host();
    if let Some(core) = host.bench_core {
        match apply_bench_affinity(core) {
            Ok(observed) => {
                host.affinity_applied = true;
                host.affinity_observed_core = Some(observed);
                if let Some(allowed) = read_proc_status_field("Cpus_allowed_list") {
                    host.cpus_allowed_list = allowed;
                }
            }
            Err(error) => host.affinity_error = Some(error),
        }
    }
    host.sched_policy = read_sched_policy();

    for sc in &scenarios {
        match measure_scenario(&clap_target, sc) {
            Ok(metrics) => {
                let cv = metrics.stddev_ns / metrics.mean_ns;
                if cv > 1.2 {
                    environment_stable = false;
                }
                if !metrics.deadline_passed
                    || metrics.process_errors > 0
                    || metrics.status_rejected > 0
                    || !metrics.probe_passed
                    || !metrics.cab_probe_passed
                {
                    all_deadlines_passed = false;
                }

                println!(
                    "{} [{}] ({}, {:.0}Hz, os={}, cab={}, ev={}) p50={:.1}µs, p95={:.1}µs, p99={:.1}µs, max={:.1}µs | {:.1} ns/samp | margin={:.1}% (budget={:.1}µs) | gate(85%): {} (p99={:.1}% of budget, headroom={:.1}%) | process_errors={} status_rejected={} probe={} (max_diff={:.3e}, rms_diff={:.3e}) cab_probe={} (cab_diff={:.3e})",
                    if metrics.deadline_passed {
                        "✓"
                    } else {
                        "✗"
                    },
                    metrics.name,
                    metrics.channels,
                    metrics.sample_rate,
                    metrics.oversample,
                    metrics.cabinet,
                    metrics.events,
                    metrics.p50_ns / 1000.0,
                    metrics.p95_ns / 1000.0,
                    metrics.p99_ns / 1000.0,
                    metrics.max_ns / 1000.0,
                    metrics.ns_per_sample,
                    metrics.deadline_margin_percent,
                    metrics.deadline_budget_ns / 1000.0,
                    if metrics.deadline_passed {
                        "PASS"
                    } else {
                        "FAIL"
                    },
                    (metrics.p99_ns / metrics.deadline_budget_ns) * 100.0,
                    metrics.gate_headroom_percent,
                    metrics.process_errors,
                    metrics.status_rejected,
                    if metrics.probe_passed { "PASS" } else { "FAIL" },
                    metrics.probe_max_abs_diff,
                    metrics.probe_rms_diff,
                    if metrics.cab_probe_passed {
                        "PASS"
                    } else {
                        "FAIL"
                    },
                    metrics.cab_probe_max_abs_diff,
                );
                results.push(metrics);
            }
            Err(err) => {
                eprintln!("❌ FATAL: Scenario '{}' failed: {err}", sc.name);
                return ExitCode::from(1);
            }
        }
    }

    let degradation_reasons = host_degradation_reasons(&host);
    let environment_degraded = !degradation_reasons.is_empty();
    let reference_check = match reference_path.as_deref() {
        Some(path) => {
            match load_reference_check(
                path,
                &results,
                p99_ratio_max,
                p50_ratio_max,
                p99_noise_floor_ns,
                &clap_sha,
            ) {
                Ok(check) => Some(check),
                Err(err) => {
                    eprintln!("❌ FATAL: {err}");
                    return ExitCode::from(1);
                }
            }
        }
        None => None,
    };
    let reference_failed = reference_check
        .as_ref()
        .is_some_and(|check| check.status == "FAIL");
    let reference_inconclusive = reference_check
        .as_ref()
        .is_some_and(|check| check.status == "INCONCLUSIVE");
    let reference_missing = reference_check.is_none();

    // A degraded host, an inconclusive reference comparison, or a missing paired series
    // cannot certify a release: the numbers may describe a valid run, but not the reference environment.
    let overall_status = if !all_deadlines_passed || reference_failed {
        "FAIL"
    } else if !environment_stable
        || environment_degraded
        || reference_missing
        || reference_inconclusive
    {
        "INCONCLUSIVE"
    } else {
        "PASS"
    };

    println!("------------------------------------------------------------------------");
    println!("Performance Gate Result:    {overall_status}");
    println!(
        "Deadline Gate Threshold:    p99 < {:.0}% of block budget (RT overload threshold)",
        DEADLINE_GATE_FRACTION * 100.0
    );
    println!("Host Measurement Stable:    {environment_stable}");
    println!(
        "Host Isolation:             {}",
        if environment_degraded {
            "degraded"
        } else {
            "dedicated"
        }
    );
    for reason in &degradation_reasons {
        println!("  degraded: {reason}");
    }
    if reference_missing {
        println!("Reference Series Missing:   true (certification requires a paired --reference)");
    }
    println!("All Scenarios Passed Gate:  {all_deadlines_passed}");
    println!(
        "Host: core={:?} policy={} governor={:?} boost={:?} irq_isolation={}",
        host.bench_core, host.sched_policy, host.governor, host.boost, host.irq_isolation
    );
    if let Some(check) = &reference_check {
        println!(
            "Reference Check Status:     {} (worst p99 ratio: {}, max allowed: {:.3}, noise floor: {:.1}µs, ref={})",
            check.status,
            check
                .worst_p99_ratio
                .map_or_else(|| "n/a".to_string(), |w| format!("{w:.3}")),
            check.p99_ratio_max,
            check.p99_noise_floor_ns / 1000.0,
            check.reference_path
        );
        if let Some(reason) = &check.inconclusive_reason {
            println!("  inconclusive reason:      {reason}");
        }
        println!("  worst p50 ratio (info):   {:.3}", check.worst_p50_ratio);
        for comp in &check.comparisons {
            let ref_p99_str = comp
                .reference_p99_ns
                .map_or_else(|| "n/a".to_string(), |v| format!("{:.1}µs", v / 1000.0));
            let r99_str = comp
                .p99_ratio
                .map_or_else(|| "n/a".to_string(), |v| format!("{v:.3}"));
            let status_str = match comp.p99_passed {
                Some(true) => "PASS",
                Some(false) => "FAIL",
                None => "INCONCLUSIVE",
            };
            println!(
                "  {} p99={:.1}µs ref_p99={} ratio={} [{}] | p50={:.1}µs (info)",
                comp.name,
                comp.measured_p99_ns / 1000.0,
                ref_p99_str,
                r99_str,
                status_str,
                comp.measured_p50_ns / 1000.0,
            );
        }
    }
    println!("========================================================================");

    let report = PerfCertificationReport {
        schema_version: "1.4".to_string(),
        timestamp_utc: now_iso8601(),
        run_id: env::var("NAM_RUN_ID").ok().filter(|v| !v.is_empty()),
        clap_artifact_path: clap_target.to_string_lossy().to_string(),
        clap_artifact_sha256: clap_sha,
        overall_status: overall_status.to_string(),
        environment_stable,
        environment_degraded,
        environment_degraded_reasons: degradation_reasons,
        total_scenarios: results.len(),
        scenarios: results,
        host,
        reference_check,
    };

    let target_out =
        out_path.unwrap_or_else(|| PathBuf::from("target/perf-certification-report.json"));
    if let Some(parent) = target_out.parent()
        && let Err(err) = fs::create_dir_all(parent)
    {
        eprintln!(
            "❌ FATAL: cannot create report directory {}: {err}",
            parent.display()
        );
        return ExitCode::from(1);
    }
    let json = match serde_json::to_string_pretty(&report) {
        Ok(json) => json,
        Err(err) => {
            eprintln!("❌ FATAL: cannot serialize performance report: {err}");
            return ExitCode::from(1);
        }
    };
    if let Err(err) = fs::write(&target_out, json) {
        eprintln!(
            "❌ FATAL: cannot write performance report {}: {err}",
            target_out.display()
        );
        return ExitCode::from(1);
    }
    println!("Report saved to: {}", target_out.display());

    match overall_status {
        "PASS" => ExitCode::SUCCESS,
        "INCONCLUSIVE" => ExitCode::from(2),
        _ => ExitCode::from(1),
    }
}

/// UTC timestamp in ISO-8601 (`YYYY-MM-DDTHH:MM:SSZ`), the same shape the rest
/// of the release artifacts use, so reports can be compared and sorted as text.
fn now_iso8601() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format_iso8601(secs)
}

fn format_iso8601(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

/// Converts days since 1970-01-01 to a proleptic Gregorian date.
///
/// Howard Hinnant's `civil_from_days` algorithm. Kept in-tree (instead of a
/// date crate) because the guard only needs one UTC timestamp and the release
/// binary must not grow a new dependency.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
#[path = "nam_perf_guard_test.rs"]
mod tests;
