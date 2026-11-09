// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

#![cfg_attr(not(target_arch = "x86_64"), allow(dead_code, unused_imports))]

//! A/B optimization benchmark for NAM-Plug — measures the DSP hot-path envelope
//! of the CLAP plugin so `plain → PGO → PGO+BOLT` builds can be compared.

#![cfg(feature = "testing")]

use clack_host::prelude::*;
use nam_plug::clap::test_util::{self, asset_hash};
use neural_amp_modeler_rs::common::params::{ActivationPrecision, ProcessingParams};
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
use std::collections::BTreeMap;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

const DEFAULT_RECEIPT_JSON: &str = "target/logs/ab-opt-receipt.json";
const DEFAULT_RATE: f64 = 48_000.0;
const DEFAULT_QUANTUM: usize = 256;
const DEFAULT_BLOCKS: u64 = 20_000;
const DEFAULT_WARMUP: u64 = 2_000;
const DEFAULT_RUNS: usize = 3;

const TOPOLOGY_WAVENET_A1: &str = "wavenet_a1";
const TOPOLOGY_WAVENET_A2: &str = "wavenet_a2";
const TOPOLOGY_LSTM: &str = "lstm";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Topology {
    WavenetA1,
    WavenetA2,
    Lstm,
}

impl Topology {
    fn label(self) -> &'static str {
        match self {
            Self::WavenetA1 => TOPOLOGY_WAVENET_A1,
            Self::WavenetA2 => TOPOLOGY_WAVENET_A2,
            Self::Lstm => TOPOLOGY_LSTM,
        }
    }
}

fn resolve_models() -> Vec<(PathBuf, Topology)> {
    let search_dirs = {
        let mut dirs = Vec::new();
        if let Ok(dir) = std::env::var("NAM_FIXTURES_DIR") {
            dirs.push(PathBuf::from(dir));
        }
        dirs.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/models"));
        dirs.push(PathBuf::from("tests/fixtures/models"));
        dirs
    };
    let mut resolved: Vec<(PathBuf, Topology)> = Vec::new();
    for (name, topo) in [
        ("wavenet_a1_standard.nam", Topology::WavenetA1),
        ("a2_example.nam", Topology::WavenetA2),
        ("lstm.nam", Topology::Lstm),
    ] {
        let mut found = false;
        for dir in &search_dirs {
            let path = dir.join(name);
            if path.is_file() {
                resolved.push((path, topo));
                found = true;
                break;
            }
        }
        if !found {
            eprintln!("ab_opt_bench: FATAL: mandatory fixture {name} not found in {search_dirs:?}");
            std::process::exit(1);
        }
    }
    resolved
}

// ── Perf-event counters ─────────────────────────────────────────────────────

const PERF_EVENT_IOC_ENABLE: libc::c_ulong = 0x2400;
const PERF_EVENT_IOC_DISABLE: libc::c_ulong = 0x2401;
const PERF_EVENT_IOC_RESET: libc::c_ulong = 0x2403;
const PERF_FLAG_FD_CLOEXEC: u64 = 0x8;
const PERF_TYPE_HARDWARE: u32 = 0;
const PERF_TYPE_HW_CACHE: u32 = 3;
const PERF_COUNT_HW_CPU_CYCLES: u64 = 0;
const PERF_COUNT_HW_INSTRUCTIONS: u64 = 1;
const PERF_COUNT_HW_CACHE_ITLB: u64 = 8;
const PERF_COUNT_HW_CACHE_ICACHE: u64 = 5;
const PERF_COUNT_HW_CACHE_OP_READ: u64 = 0;
const PERF_COUNT_HW_CACHE_RESULT_MISS: u64 = 1;
const NR_PERF_EVENT_OPEN: libc::c_long = 298;

#[repr(C)]
struct PerfEventAttr {
    type_: u32,
    size: u32,
    config: u64,
    sample_period: u64,
    sample_type: u64,
    read_format: u64,
    flags: u64,
    wakeup_events: u32,
    bp_type: u32,
    config1: u64,
    config2: u64,
    branch_sample_type: u64,
    sample_regs_user: u64,
    sample_stack_user: u32,
    clockid: i32,
}

impl PerfEventAttr {
    fn hardware(config: u64) -> Self {
        Self {
            type_: PERF_TYPE_HARDWARE,
            size: std::mem::size_of::<Self>() as u32,
            config,
            sample_period: 0,
            sample_type: 0,
            read_format: 0,
            flags: 1 | (1 << 5) | (1 << 6),
            wakeup_events: 0,
            bp_type: 0,
            config1: 0,
            config2: 0,
            branch_sample_type: 0,
            sample_regs_user: 0,
            sample_stack_user: 0,
            clockid: 0,
        }
    }

    fn cache(config: u64) -> Self {
        Self {
            type_: PERF_TYPE_HW_CACHE,
            ..Self::hardware(config)
        }
    }
}

struct Pmc {
    name: &'static str,
    fd: Option<i32>,
    reason: Option<String>,
}

impl Pmc {
    fn open(name: &'static str, attr: PerfEventAttr) -> Self {
        let fd = unsafe {
            libc::syscall(
                NR_PERF_EVENT_OPEN,
                &raw const attr,
                0,
                -1,
                -1,
                PERF_FLAG_FD_CLOEXEC,
            )
        };
        if fd >= 0 {
            Self {
                name,
                fd: Some(fd as i32),
                reason: None,
            }
        } else {
            Self {
                name,
                fd: None,
                reason: Some(std::io::Error::last_os_error().to_string()),
            }
        }
    }

    fn reset(&mut self) {
        if let Some(fd) = self.fd {
            unsafe {
                libc::ioctl(fd, PERF_EVENT_IOC_RESET, 0);
            }
        }
    }

    fn enable(&mut self) {
        if let Some(fd) = self.fd {
            unsafe {
                libc::ioctl(fd, PERF_EVENT_IOC_ENABLE, 0);
            }
        }
    }

    fn disable(&mut self) {
        if let Some(fd) = self.fd {
            unsafe {
                libc::ioctl(fd, PERF_EVENT_IOC_DISABLE, 0);
            }
        }
    }

    fn read_count(&self) -> Option<u64> {
        let fd = self.fd?;
        let mut count = 0u64;
        let n = unsafe {
            libc::read(
                fd,
                &raw mut count as *mut libc::c_void,
                std::mem::size_of::<u64>(),
            )
        };
        if n == std::mem::size_of::<u64>() as isize {
            Some(count)
        } else {
            None
        }
    }
}

impl Drop for Pmc {
    fn drop(&mut self) {
        if let Some(fd) = self.fd.take() {
            unsafe {
                libc::close(fd);
            }
        }
    }
}

struct PmcSet {
    cycles: Pmc,
    instructions: Pmc,
    itlb_misses: Pmc,
    icache_misses: Pmc,
}

impl PmcSet {
    fn open() -> Self {
        let itlb_config = PERF_COUNT_HW_CACHE_ITLB
            | (PERF_COUNT_HW_CACHE_OP_READ << 8)
            | (PERF_COUNT_HW_CACHE_RESULT_MISS << 16);
        let icache_config = PERF_COUNT_HW_CACHE_ICACHE
            | (PERF_COUNT_HW_CACHE_OP_READ << 8)
            | (PERF_COUNT_HW_CACHE_RESULT_MISS << 16);
        Self {
            cycles: Pmc::open("cycles", PerfEventAttr::hardware(PERF_COUNT_HW_CPU_CYCLES)),
            instructions: Pmc::open(
                "instructions",
                PerfEventAttr::hardware(PERF_COUNT_HW_INSTRUCTIONS),
            ),
            itlb_misses: Pmc::open("itlb_misses", PerfEventAttr::cache(itlb_config)),
            icache_misses: Pmc::open("icache_misses", PerfEventAttr::cache(icache_config)),
        }
    }

    fn reset(&mut self) {
        self.cycles.reset();
        self.instructions.reset();
        self.itlb_misses.reset();
        self.icache_misses.reset();
    }

    fn enable(&mut self) {
        self.cycles.enable();
        self.instructions.enable();
        self.itlb_misses.enable();
        self.icache_misses.enable();
    }

    fn disable(&mut self) {
        self.cycles.disable();
        self.instructions.disable();
        self.itlb_misses.disable();
        self.icache_misses.disable();
    }

    fn counts(&self) -> BTreeMap<&'static str, Result<u64, String>> {
        let mut m = BTreeMap::new();
        let record = |pmc: &Pmc, map: &mut BTreeMap<&'static str, Result<u64, String>>| {
            if let Some(val) = pmc.read_count() {
                map.insert(pmc.name, Ok(val));
            } else if let Some(ref r) = pmc.reason {
                map.insert(pmc.name, Err(r.clone()));
            } else {
                map.insert(pmc.name, Err("read failed".into()));
            }
        };
        record(&self.cycles, &mut m);
        record(&self.instructions, &mut m);
        record(&self.itlb_misses, &mut m);
        record(&self.icache_misses, &mut m);
        m
    }
}

// ── Affinity / Thread Pinning ───────────────────────────────────────────────

fn apply_bench_affinity(core: usize) -> Result<usize, String> {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
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
            return Err(format!(
                "sched_getcpu:{core}:{}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(observed as usize)
    }
}

fn select_optimal_cpu() -> Option<usize> {
    if let Some(core) = std::env::var("NAM_BENCH_CORE")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        return Some(core);
    }
    std::fs::read_to_string("/sys/devices/system/cpu/isolated")
        .ok()
        .and_then(|content| {
            content
                .trim()
                .split(',')
                .next()
                .and_then(|s| s.split('-').next())
                .filter(|s| !s.is_empty())
                .and_then(|s| s.parse::<usize>().ok())
        })
}

fn pin_thread(cpu: Option<usize>) -> Result<(), String> {
    if let Some(c) = cpu {
        match apply_bench_affinity(c) {
            Ok(_) => {}
            Err(e) => eprintln!("  warn: could not pin thread to cpu {c}: {e}"),
        }
    }
    Ok(())
}

// ── Timing ──────────────────────────────────────────────────────────────────

#[inline(always)]
fn rdtsc_serialized() -> u64 {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::x86_64::_mm_lfence();
        let tsc = std::arch::x86_64::_rdtsc();
        std::arch::x86_64::_mm_lfence();
        tsc
    }
    #[cfg(not(target_arch = "x86_64"))]
    0
}

fn measure_freq_ghz_x1000() -> u64 {
    let t0 = Instant::now();
    let c0 = rdtsc_serialized();
    std::thread::sleep(Duration::from_millis(50));
    let c1 = rdtsc_serialized();
    let dt = t0.elapsed().as_nanos();
    if dt == 0 {
        return 3_000;
    }
    let cyc = c1.saturating_sub(c0);
    let freq = (cyc as u128 * 1_000_000_000 / dt) as u64;
    (freq / 1_000_000).max(500)
}

struct Stats {
    min: u64,
    mean: f64,
    p50: u64,
    p95: u64,
    p99: u64,
    p999: u64,
    max: u64,
}

impl Stats {
    fn of(mut v: Vec<u64>) -> Self {
        if v.is_empty() {
            return Self {
                min: 0,
                mean: 0.0,
                p50: 0,
                p95: 0,
                p99: 0,
                p999: 0,
                max: 0,
            };
        }
        v.sort_unstable();
        let n = v.len();
        let min = v[0];
        let max = v[n - 1];
        let sum: u64 = v.iter().copied().sum();
        let mean = sum as f64 / n as f64;
        let pct = |p: f64| -> u64 {
            let idx = ((p * (n as f64 - 1.0)).round() as usize).min(n - 1);
            v[idx]
        };
        Self {
            min,
            mean,
            p50: pct(0.50),
            p95: pct(0.95),
            p99: pct(0.99),
            p999: pct(0.999),
            max,
        }
    }
}

// ── Workload runner ─────────────────────────────────────────────────────────

fn run_measured_clap(
    model_path: &Path,
    sample_rate: f64,
    block_size: usize,
    warmup_blocks: u64,
    measured_blocks: u64,
    clap_so_path: Option<&Path>,
    pmcs: Option<&mut PmcSet>,
) -> Result<(Vec<u64>, u64), String> {
    let (_entry, _host, mut plugin_instance) = match clap_so_path {
        Some(path) => test_util::make_test_plugin_dynamic(path),
        None => test_util::make_test_plugin(),
    };

    let mut params = ProcessingParams::default();
    params.model_path = Some(model_path.to_path_buf());
    params.model_hash = asset_hash(model_path);
    params.oversample = OversampleFactor::Off;
    params.activation_precision = ActivationPrecision::Standard;

    test_util::load_plugin_state(&mut plugin_instance, &params);

    let audio_config = PluginAudioConfiguration {
        sample_rate,
        min_frames_count: block_size as u32,
        max_frames_count: block_size as u32,
    };

    let stopped_processor = plugin_instance
        .activate(|_, _| (), audio_config)
        .map_err(|e| format!("Plugin activation failed: {e:?}"))?;

    let mut started_processor = stopped_processor
        .start_processing()
        .map_err(|e| format!("Start processing failed: {e:?}"))?;

    let n = block_size;
    let mut in_l = vec![0.1f32; n];
    let mut in_r = vec![0.1f32; n];
    let mut out_l = vec![0.0f32; n];
    let mut out_r = vec![0.0f32; n];

    let mut input_ports = AudioPorts::with_capacity(2, 1);
    let mut output_ports = AudioPorts::with_capacity(2, 1);
    let mut output_events_buffer = EventBuffer::with_capacity(10);

    for _ in 0..warmup_blocks {
        let input_events = InputEvents::empty();
        let mut output_events = OutputEvents::from_buffer(&mut output_events_buffer);
        let mut input_channels = [in_l.as_mut_slice(), in_r.as_mut_slice()];
        let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(
                input_channels.iter_mut().map(InputChannel::constant),
            ),
        }]);
        let output_channels = [out_l.as_mut_slice(), out_r.as_mut_slice()];
        let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
        }]);

        let _ = started_processor.process(
            &input_audio,
            &mut output_audio,
            &input_events,
            &mut output_events,
            None,
            None,
        );
    }

    if let Some(p) = pmcs {
        p.enable();
    }

    let mut cycles = Vec::with_capacity(measured_blocks as usize);
    let mut total_frames = 0u64;

    for _ in 0..measured_blocks {
        let input_events = InputEvents::empty();
        let mut output_events = OutputEvents::from_buffer(&mut output_events_buffer);
        let mut input_channels = [in_l.as_mut_slice(), in_r.as_mut_slice()];
        let input_audio = input_ports.with_input_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(
                input_channels.iter_mut().map(InputChannel::constant),
            ),
        }]);
        let output_channels = [out_l.as_mut_slice(), out_r.as_mut_slice()];
        let mut output_audio = output_ports.with_output_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
        }]);

        let t0 = rdtsc_serialized();
        let _ = started_processor.process(
            &input_audio,
            &mut output_audio,
            &input_events,
            &mut output_events,
            None,
            None,
        );
        let t1 = rdtsc_serialized();

        black_box(out_l[0]);
        cycles.push(t1.saturating_sub(t0));
        total_frames += n as u64;
    }

    Ok((cycles, total_frames))
}

// ── JSON formatting ─────────────────────────────────────────────────────────

enum JsonValue {
    Str(String),
    Int(u64),
    Float(f64),
    Obj(Vec<(String, JsonValue)>),
}

impl JsonValue {
    fn to_json(&self) -> String {
        match self {
            Self::Str(s) => format!("\"{}\"", s.replace('"', "\\\"")),
            Self::Int(i) => i.to_string(),
            Self::Float(f) => {
                if f.is_nan() || f.is_infinite() {
                    "null".to_string()
                } else {
                    format!("{f:.3}")
                }
            }
            Self::Obj(pairs) => {
                let inner = pairs
                    .iter()
                    .map(|(k, v)| format!("\"{}\":{}", k, v.to_json()))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("{{{inner}}}")
            }
        }
    }
}

fn kv(k: &str, v: JsonValue) -> (String, JsonValue) {
    (k.to_string(), v)
}

// ── CLI Configuration ───────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
enum TopoFilter {
    All,
    A1,
    A2,
    Lstm,
}

struct Cfg {
    variant: String,
    runs: usize,
    blocks: u64,
    warmup: u64,
    quantum: usize,
    rate: f64,
    topology: TopoFilter,
    cpu: Option<usize>,
    receipt_json: PathBuf,
    clap_path: Option<PathBuf>,
}

const USAGE: &str = "Uso: ab_opt_bench --variant <name> [--runs N] [--blocks N] [--warmup N] \
    [--quantum N] [--rate N] [--topology all|a1|a2|lstm] [--cpu N] [--clap <so>] [--receipt <path>]";

fn parse_args_from<I: Iterator<Item = String>>(mut args: I) -> Result<Cfg, String> {
    let mut cfg = Cfg {
        variant: String::new(),
        runs: DEFAULT_RUNS,
        blocks: DEFAULT_BLOCKS,
        warmup: DEFAULT_WARMUP,
        quantum: DEFAULT_QUANTUM,
        rate: DEFAULT_RATE,
        topology: TopoFilter::All,
        cpu: None,
        receipt_json: PathBuf::from(DEFAULT_RECEIPT_JSON),
        clap_path: std::env::var("NAM_CLAP_SO_PATH").ok().map(PathBuf::from),
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--variant" => {
                cfg.variant = args
                    .next()
                    .ok_or_else(|| "--variant <name> requires a value".to_string())?;
            }
            "--runs" => {
                cfg.runs = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| "--runs requires an integer".to_string())?;
            }
            "--blocks" => {
                cfg.blocks = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| "--blocks requires an integer".to_string())?;
            }
            "--warmup" => {
                cfg.warmup = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| "--warmup requires an integer".to_string())?;
            }
            "--quantum" => {
                cfg.quantum = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| "--quantum requires an integer".to_string())?;
            }
            "--rate" => {
                cfg.rate = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| "--rate requires a float".to_string())?;
            }
            "--topology" => {
                cfg.topology = match args.next().as_deref() {
                    Some("all") => TopoFilter::All,
                    Some("a1") => TopoFilter::A1,
                    Some("a2") => TopoFilter::A2,
                    Some("lstm") => TopoFilter::Lstm,
                    other => {
                        return Err(format!("--topology must be all|a1|a2|lstm, got {other:?}"));
                    }
                };
            }
            "--cpu" => {
                cfg.cpu = args.next().and_then(|v| v.parse().ok());
            }
            "--clap" => {
                cfg.clap_path = args.next().map(PathBuf::from);
            }
            "--receipt" => {
                cfg.receipt_json = PathBuf::from(
                    args.next()
                        .ok_or_else(|| "--receipt <path> requires a value".to_string())?,
                );
            }
            "--help" | "-h" => {
                eprintln!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if cfg.variant.is_empty() {
        return Err("--variant <name> is required".to_string());
    }
    if cfg.runs < 1 {
        return Err("--runs must be >= 1".to_string());
    }
    if cfg.blocks < 1000 {
        return Err("--blocks must be >= 1000".to_string());
    }
    if cfg.cpu.is_none() {
        cfg.cpu = select_optimal_cpu();
    }
    Ok(cfg)
}

fn parse_args() -> Cfg {
    match parse_args_from(std::env::args().skip(1)) {
        Ok(cfg) => cfg,
        Err(msg) => {
            eprintln!("ab_opt_bench: {msg}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}

// ── Main ────────────────────────────────────────────────────────────────────

#[cfg(target_arch = "x86_64")]
fn main() -> ExitCode {
    let cfg = parse_args();
    if let Err(e) = pin_thread(cfg.cpu) {
        eprintln!("ab_opt_bench: FATAL: {e}");
        return ExitCode::FAILURE;
    }
    let freq = measure_freq_ghz_x1000();
    let models = resolve_models();

    let scope: Vec<&(PathBuf, Topology)> = models
        .iter()
        .filter(|(_, t)| match cfg.topology {
            TopoFilter::All => true,
            TopoFilter::A1 => *t == Topology::WavenetA1,
            TopoFilter::A2 => *t == Topology::WavenetA2,
            TopoFilter::Lstm => *t == Topology::Lstm,
        })
        .collect();
    if scope.is_empty() {
        eprintln!("ab_opt_bench: FATAL: no topology in scope");
        return ExitCode::FAILURE;
    }

    eprintln!(
        "NAM-Plug A/B opt bench — variant={} runs={} blocks={} quantum={} rate={} freq={:.3} GHz",
        cfg.variant,
        cfg.runs,
        cfg.blocks,
        cfg.quantum,
        cfg.rate,
        freq as f64 / 1000.0
    );
    if let Some(c) = cfg.cpu {
        eprintln!("  cpu = {c}");
    }
    if let Some(ref p) = cfg.clap_path {
        eprintln!("  clap = {}", p.display());
    }

    let mut pmcs = PmcSet::open();
    let mut jsonl_lines: Vec<JsonValue> = Vec::new();
    let mut txt = String::new();
    txt.push_str(&format!(
        "NAM-Plug A/B optimization bench\n\
         ===============================\n\
         variant={} runs={} blocks={} quantum={} rate={}\n",
        cfg.variant, cfg.runs, cfg.blocks, cfg.quantum, cfg.rate
    ));

    for (path, topo) in &scope {
        for run in 0..cfg.runs {
            pmcs.reset();
            let (cycles, total_frames) = match run_measured_clap(
                path,
                cfg.rate,
                cfg.quantum,
                cfg.warmup,
                cfg.blocks,
                cfg.clap_path.as_deref(),
                Some(&mut pmcs),
            ) {
                Ok(res) => res,
                Err(err) => {
                    eprintln!("ab_opt_bench: FATAL: {err}");
                    return ExitCode::FAILURE;
                }
            };
            pmcs.disable();
            let counts = pmcs.counts();
            let st = Stats::of(cycles);
            let to_ns = |c: u64| c * 1000 / freq;

            let mut run_obj = vec![
                kv("event", JsonValue::Str("run".into())),
                kv("variant", JsonValue::Str(cfg.variant.clone())),
                kv("topology", JsonValue::Str(topo.label().into())),
                kv("run", JsonValue::Int(run as u64 + 1)),
                kv("blocks", JsonValue::Int(cfg.blocks)),
                kv("frames_processed", JsonValue::Int(total_frames)),
                kv(
                    "cycles",
                    JsonValue::Obj(vec![
                        kv("min", JsonValue::Int(st.min)),
                        kv("mean", JsonValue::Float(st.mean)),
                        kv("p50", JsonValue::Int(st.p50)),
                        kv("p95", JsonValue::Int(st.p95)),
                        kv("p99", JsonValue::Int(st.p99)),
                        kv("p999", JsonValue::Int(st.p999)),
                        kv("max", JsonValue::Int(st.max)),
                    ]),
                ),
                kv(
                    "nanoseconds",
                    JsonValue::Obj(vec![
                        kv("min", JsonValue::Int(to_ns(st.min))),
                        kv("mean", JsonValue::Float(st.mean * 1000.0 / freq as f64)),
                        kv("p50", JsonValue::Int(to_ns(st.p50))),
                        kv("p95", JsonValue::Int(to_ns(st.p95))),
                        kv("p99", JsonValue::Int(to_ns(st.p99))),
                        kv("p999", JsonValue::Int(to_ns(st.p999))),
                        kv("max", JsonValue::Int(to_ns(st.max))),
                    ]),
                ),
            ];

            let mut pmc_obj = Vec::new();
            for (name, res) in counts {
                match res {
                    Ok(val) => pmc_obj.push(kv(name, JsonValue::Int(val))),
                    Err(cause) => {
                        pmc_obj.push(kv(
                            name,
                            JsonValue::Obj(vec![
                                kv("status", JsonValue::Str("unavailable".into())),
                                kv("cause", JsonValue::Str(cause)),
                            ]),
                        ));
                    }
                }
            }
            run_obj.push(kv("pmc", JsonValue::Obj(pmc_obj)));
            jsonl_lines.push(JsonValue::Obj(run_obj));

            let line = format!(
                "  [{}] run {}/{}: p50={:.1}us p99={:.1}us max={:.1}us (p99={} cyc)\n",
                topo.label(),
                run + 1,
                cfg.runs,
                to_ns(st.p50) as f64 / 1000.0,
                to_ns(st.p99) as f64 / 1000.0,
                to_ns(st.max) as f64 / 1000.0,
                st.p99
            );
            eprint!("{line}");
            txt.push_str(&line);
        }
    }

    if let Some(parent) = cfg.receipt_json.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut json_out = String::new();
    for l in jsonl_lines {
        json_out.push_str(&l.to_json());
        json_out.push('\n');
    }
    if let Err(e) = std::fs::write(&cfg.receipt_json, json_out) {
        eprintln!(
            "ab_opt_bench: FATAL: cannot write receipt {}: {e}",
            cfg.receipt_json.display()
        );
        return ExitCode::FAILURE;
    }
    let txt_path = cfg.receipt_json.with_extension("txt");
    let _ = std::fs::write(&txt_path, txt);

    eprintln!(
        "ab_opt_bench: receipt written to {}",
        cfg.receipt_json.display()
    );
    ExitCode::SUCCESS
}

#[cfg(not(target_arch = "x86_64"))]
fn main() -> ExitCode {
    eprintln!("ab_opt_bench: x86_64 target required for RDTSC/perf_event_open benchmark");
    ExitCode::FAILURE
}
