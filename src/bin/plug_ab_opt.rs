// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Typed comparison stage of the A/B optimization ceremony for NAM-Plug.
//!
//! Reads the per-variant JSONL receipts produced by `ab_opt_bench`
//! (`target/logs/ab-opt-{plain,pgo,pgo+bolt}.json`), derives the median p99/max/
//! mean cycle envelope, applies the promotion rule (an optimization is
//! announced only when it preserves or improves the measured envelope) and
//! writes `target/logs/ab-opt-ceremony-receipt.json` for the release chain.
//!
//! Usage:
//! ```text
//!   plug_ab_opt compare --dir <dir> --out <file> --runs <N> [--bolt-applied 0|1]
//!                       [--bolt-cause <text>]
//! ```
//!
//! Exit codes: 0 valid ceremony verdict (PGO+BOLT, PGO-ONLY, or UNOPTIMIZED
//! fallback); 1 missing/invalid/incomplete receipts; 2 usage error.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::exit;

use nam_plug::receipt::ab_opt::{
    CeremonyBoltLbr, CeremonyProvenance, derive_bolt_lbr, derive_provenance, evaluate_ceremony,
    fmt, fmt_num, median_num, read_runs,
};
use serde_json::{Map, Value};

const USAGE: &str = "usage: plug_ab_opt compare --dir <dir> --out <file> --runs <N> \
    [--bolt-applied 0|1] [--bolt-cause <text>]\n       \
    plug_ab_opt write-meta --dir <dir> [--cpu-bench <core>] [--paranoid-before <N>] \
    [--paranoid-after <N>] [--engine-commit <sha>] [--bolt-applied 0|1] \
    [--bolt-samples <N>] [--bolt-fdata-sha256 <hex>] [--bolt-mode <lbr|instrumented>]\n       \
    plug_ab_opt pick-core [--bench-core <N>]";

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let Some(action) = args.first().map(String::as_str) else {
        eprintln!("{USAGE}");
        exit(2);
    };
    match action {
        "compare" => compare(&args[1..]),
        "write-meta" => write_meta(&args[1..]),
        "pick-core" => pick_core(&args[1..]),
        "-h" | "--help" => {
            println!("{USAGE}");
            exit(0);
        }
        _ => {
            eprintln!("{USAGE}");
            exit(2);
        }
    }
}

fn parse_cpu_list(spec: &str) -> Vec<usize> {
    let mut cpus = Vec::new();
    for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        if let Some((lo, hi)) = part.split_once('-') {
            if let (Ok(a), Ok(b)) = (lo.trim().parse::<usize>(), hi.trim().parse::<usize>())
                && a <= b
            {
                cpus.extend(a..=b);
            }
        } else if let Ok(c) = part.parse::<usize>() {
            cpus.push(c);
        }
    }
    cpus
}

fn is_cpu_online(cpu: usize) -> bool {
    let online = std::path::Path::new("/sys/devices/system/cpu").join(format!("cpu{cpu}/online"));
    match fs::read_to_string(online) {
        Ok(value) => value.trim() == "1",
        Err(_) => std::path::Path::new("/sys/devices/system/cpu")
            .join(format!("cpu{cpu}"))
            .is_dir(),
    }
}

fn permitted_cpus() -> Vec<usize> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `cpu_set_t` is zero-initialized before being passed to `sched_getaffinity`.
        let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        // SAFETY: `pid = 0` queries the calling thread into the stack-allocated `cpu_set_t`.
        let rc =
            unsafe { libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) };
        if rc != 0 {
            return Vec::new();
        }
        let mut cpus = Vec::new();
        for cpu in 0..libc::CPU_SETSIZE as usize {
            // SAFETY: `cpu < CPU_SETSIZE`.
            if unsafe { libc::CPU_ISSET(cpu, &set) } {
                cpus.push(cpu);
            }
        }
        cpus
    }
    #[cfg(not(target_os = "linux"))]
    Vec::new()
}

fn pick_core(args: &[String]) {
    let explicit = args
        .windows(2)
        .find(|w| w[0] == "--bench-core")
        .map(|w| w[1].clone())
        .or_else(|| env::var("NAM_BENCH_CORE").ok());

    let resolved = if let Some(raw) = explicit
        .as_deref()
        .map(str::trim)
        .filter(|raw| !raw.is_empty() && *raw != "auto")
    {
        raw.parse::<u32>()
            .ok()
            .filter(|&cpu| is_cpu_online(cpu as usize))
    } else {
        let isolated = fs::read_to_string("/sys/devices/system/cpu/isolated")
            .map(|list| parse_cpu_list(&list))
            .unwrap_or_default();
        isolated
            .into_iter()
            .find(|&cpu| is_cpu_online(cpu))
            .or_else(|| permitted_cpus().into_iter().find(|&cpu| is_cpu_online(cpu)))
            .map(|cpu| cpu as u32)
    };

    match resolved {
        Some(core) => {
            println!("{core}");
            exit(0);
        }
        None => {
            eprintln!("GAP: no isolated or permitted bench core");
            exit(2);
        }
    }
}

struct Cfg {
    dir: PathBuf,
    out: PathBuf,
    runs: u64,
    bolt_applied: Option<bool>,
    bolt_cause: Option<String>,
}

fn parse_args(args: &[String]) -> Result<Cfg, String> {
    let mut cfg = Cfg {
        dir: PathBuf::from("target/logs"),
        out: PathBuf::from("target/logs/ab-opt-ceremony-receipt.json"),
        runs: 3,
        bolt_applied: None,
        bolt_cause: None,
    };
    let mut i = 0;
    while i < args.len() {
        let value = |i: usize| {
            args.get(i + 1)
                .cloned()
                .ok_or_else(|| format!("{} requires a value", args[i]))
        };
        match args[i].as_str() {
            "--dir" => {
                cfg.dir = PathBuf::from(value(i)?);
                i += 2;
            }
            "--out" => {
                cfg.out = PathBuf::from(value(i)?);
                i += 2;
            }
            "--runs" => {
                cfg.runs = value(i)?
                    .parse()
                    .map_err(|_| "--runs requires an integer".to_string())?;
                i += 2;
            }
            "--bolt-applied" => {
                cfg.bolt_applied = Some(match value(i)?.as_str() {
                    "0" => false,
                    "1" => true,
                    other => return Err(format!("--bolt-applied expects 0 or 1, got {other:?}")),
                });
                i += 2;
            }
            "--bolt-cause" => {
                cfg.bolt_cause = Some(value(i)?);
                i += 2;
            }
            flag => return Err(format!("unknown flag: {flag}")),
        }
    }
    Ok(cfg)
}

fn fail(message: &str) -> ! {
    eprintln!("plug_ab_opt: {message}");
    exit(1);
}

fn compare(args: &[String]) {
    let cfg = match parse_args(args) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("plug_ab_opt: {e}");
            eprintln!("{USAGE}");
            exit(2);
        }
    };
    let plain = match read_runs(&cfg.dir, "plain") {
        Ok(runs) => runs,
        Err(e) => {
            eprintln!("plug_ab_opt: {e}");
            exit(1);
        }
    };
    let pgo = match read_runs(&cfg.dir, "pgo") {
        Ok(runs) => runs,
        Err(e) => {
            eprintln!("plug_ab_opt: {e}");
            exit(1);
        }
    };
    if plain.is_empty() || pgo.is_empty() {
        eprintln!("plug_ab_opt: plain/pgo receipts are empty; cannot compare");
        exit(1);
    }
    let bolt = match cfg.bolt_applied {
        Some(false) => Vec::new(),
        Some(true) => read_runs(&cfg.dir, "pgo+bolt").unwrap_or_else(|e| fail(&e)),
        None => {
            if cfg.dir.join("ab-opt-pgo+bolt.json").exists()
                || cfg.dir.join("ab-opt-pgo-bolt.json").exists()
            {
                read_runs(&cfg.dir, "pgo+bolt").unwrap_or_else(|e| fail(&e))
            } else {
                Vec::new()
            }
        }
    };
    if cfg.bolt_applied == Some(true) && bolt.is_empty() {
        fail("BOLT marked applied but ab-opt-pgo+bolt.json has no run events");
    }

    let provenance = Some(derive_provenance(&cfg.dir, None));
    let bolt_applied = cfg.bolt_applied.unwrap_or(!bolt.is_empty());
    let bolt_lbr = if bolt_applied {
        derive_bolt_lbr(&cfg.dir)
    } else {
        None
    };

    let decision = match evaluate_ceremony(
        &plain,
        &pgo,
        &bolt,
        cfg.runs,
        cfg.bolt_cause.as_deref(),
        provenance,
        bolt_lbr,
    ) {
        Ok(d) => d,
        Err(e) => fail(&e),
    };

    let pretty = serde_json::to_string_pretty(&Value::Object(decision.doc))
        .unwrap_or_else(|_| "{}".to_string());
    if let Err(e) = fs::write(&cfg.out, format!("{pretty}\n")) {
        eprintln!("plug_ab_opt: cannot write {}: {e}", cfg.out.display());
        exit(1);
    }

    println!();
    println!("VERDICT: {}", decision.verdict);
    if !decision.rollback.is_empty() {
        println!("  {}", decision.rollback);
    }
    println!();
    if let (Some(bolt_gain), Some(pmu_bolt_gain)) = (&decision.bolt_gain, &decision.pmu_bolt_gain) {
        let read = |map: &Map<String, Value>, key: &str| map.get(key).and_then(Value::as_f64);
        println!(
            "  // Measured: BOLT gain p99={} (PGO p99={} → BOLT p99={} cyc), max={}, mean={}",
            fmt(read(bolt_gain, "p99")),
            fmt_num(Some(decision.pgo_p99)),
            fmt_num(median_num(&bolt, "p99")),
            fmt(read(bolt_gain, "max")),
            fmt(read(bolt_gain, "mean"))
        );
        println!(
            "  // Measured: BOLT iTLB={}, I-cache={}",
            fmt(read(pmu_bolt_gain, "itlb_misses")),
            fmt(read(pmu_bolt_gain, "icache_misses"))
        );
    }
    let read = |map: &Map<String, Value>, key: &str| map.get(key).and_then(Value::as_f64);
    println!(
        "  // Measured: PGO gain p99={}, max={}, mean={}",
        fmt(read(&decision.pgo_gain, "p99")),
        fmt(read(&decision.pgo_gain, "max")),
        fmt(read(&decision.pgo_gain, "mean"))
    );
    println!();
    println!("  Receipt: {}", cfg.out.display());
}

fn read_engine_commit_from_vars() -> Option<String> {
    let content = fs::read_to_string("variables.env").ok()?;
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("ENGINE_COMMIT=") {
            let val = rest.trim().trim_matches('"').trim_matches('\'');
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

fn write_meta(args: &[String]) {
    let mut dir = PathBuf::from("target/logs");
    let mut cpu_bench: Option<String> = None;
    let mut paranoid_before: Option<i32> = None;
    let mut paranoid_after: Option<i32> = None;
    let mut engine_commit: Option<String> = None;
    let mut bolt_applied = false;
    let mut bolt_samples: Option<u64> = None;
    let mut bolt_fdata_sha256: Option<String> = None;
    let mut bolt_mode: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let val = |idx: usize| -> String {
            args.get(idx + 1).cloned().unwrap_or_else(|| {
                eprintln!("plug_ab_opt: {} requires a value", args[idx]);
                exit(2);
            })
        };
        match args[i].as_str() {
            "--dir" => {
                dir = PathBuf::from(val(i));
                i += 2;
            }
            "--cpu-bench" => {
                cpu_bench = Some(val(i));
                i += 2;
            }
            "--paranoid-before" => {
                paranoid_before = val(i).parse().ok();
                i += 2;
            }
            "--paranoid-after" => {
                paranoid_after = val(i).parse().ok();
                i += 2;
            }
            "--engine-commit" => {
                engine_commit = Some(val(i));
                i += 2;
            }
            "--bolt-applied" => {
                bolt_applied = val(i) == "1";
                i += 2;
            }
            "--bolt-samples" => {
                bolt_samples = val(i).parse().ok();
                i += 2;
            }
            "--bolt-fdata-sha256" => {
                bolt_fdata_sha256 = Some(val(i));
                i += 2;
            }
            "--bolt-mode" => {
                bolt_mode = Some(val(i));
                i += 2;
            }
            other => {
                eprintln!("plug_ab_opt: unknown flag in write-meta: {other}");
                exit(2);
            }
        }
    }

    if let Err(e) = fs::create_dir_all(&dir) {
        fail(&format!("cannot create {}: {e}", dir.display()));
    }

    let prov_path = dir.join("ab-opt-provenance.json");
    let _ = fs::remove_file(&prov_path);
    let mut prov: CeremonyProvenance = derive_provenance(&dir, cpu_bench.as_deref());
    if let Some(ec) = engine_commit
        .filter(|s| !s.is_empty())
        .or_else(read_engine_commit_from_vars)
    {
        prov.engine_commit = ec;
    }
    if let Some(pb) = paranoid_before {
        prov.perf_event_paranoid_before = pb;
    }
    if let Some(pa) = paranoid_after {
        prov.perf_event_paranoid_after = pa;
    }

    let prov_json = serde_json::to_string_pretty(&prov.to_json())
        .unwrap_or_else(|e| fail(&format!("cannot serialize provenance: {e}")));
    if let Err(e) = fs::write(&prov_path, format!("{prov_json}\n")) {
        fail(&format!("cannot write {}: {e}", prov_path.display()));
    }

    let lbr_path = dir.join("ab-opt-bolt-lbr.json");
    if bolt_applied {
        let lbr = CeremonyBoltLbr {
            samples: bolt_samples.unwrap_or(0),
            fdata_sha256: bolt_fdata_sha256.unwrap_or_else(|| "unknown".to_string()),
            mode: bolt_mode.unwrap_or_else(|| "instrumented".to_string()),
        };
        let lbr_json = serde_json::to_string_pretty(&lbr.to_json())
            .unwrap_or_else(|e| fail(&format!("cannot serialize bolt lbr: {e}")));
        if let Err(e) = fs::write(&lbr_path, format!("{lbr_json}\n")) {
            fail(&format!("cannot write {}: {e}", lbr_path.display()));
        }
    } else {
        let _ = fs::remove_file(&lbr_path);
    }
}
