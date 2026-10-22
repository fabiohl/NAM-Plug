#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# =============================================================================
# Performance Regression Gate — thin wrapper over the engine `nam_perf_gate`
# =============================================================================
#
# Unified performance governance (EC-03; canonical orchestration):
# runs the Criterion `clap_bench` suite and delegates every
# interpretation (environment fingerprint, baseline persistence, per-benchmark
# coverage and the machine regression verdict over Criterion
# `change/estimates.json`) to the engine binary `nam_perf_gate`, the single
# statistical authority across the ecosystem. The shared orchestration body lives
# in the canonical `lib/_lib_core.sh` (`run_perf_regression_gate`), drift-pinned
# by `utils/test-lib-drift.sh`; this file only carries the repository-specific
# configuration (bench suite + expected-id manifest).
#
# The expected benchmark set is declared in `benches/clap_bench.manifest`
# (trailing-`*` group patterns), so a group that did not run at all — or any
# executed benchmark without a saved baseline series — fails closed.
#
# Modes
#   --check (default)     Compare the current build against the saved baseline.
#   --bootstrap-baseline  Create a new baseline + environment fingerprint
#                         (human operator only).
#
# Environment
#   NAM_PERF_GATE          Path to a prebuilt engine `nam_perf_gate` (else a
#                          PATH install, else a sibling engine checkout build).
#   NAM_ENGINE_DIR         Engine checkout used to build the gate when unset
#                          (default: ../NeuralAmpModeler-rs).
#   NAM_BENCH_CORE         CPU core pinned via taskset (default pick_bench_core).
#   NAM_BASELINE_NAME      Criterion baseline name (default ci-baseline).
#   NAM_BENCH_MANIFEST     Expected-id manifest (default benches/clap_bench.manifest).
#   NAM_THERMAL_COOLDOWN_S Cooldown before benchmarking (default 180, 0 to skip).
#   NAM_GOVERNOR_FILE      Governor sysfs path override (tests only).
#   NAM_PERF_NOISE_THRESHOLD Noise band threshold for stability advisory (default: 0.05).
#   NAM_PERF_STABILITY_STRICT If 1, turns baseline stability advisory warning into fatal error
#                          (also fatal when NAM_BOOTSTRAP_FULL_AUDIT=1 or NAM_BOOTSTRAP_STRICT_PERF=1).
#
# Note:
#   `--check` immediately after `--bootstrap-baseline` measures self-consistency
#   (repeatability of identical binary); the regression gate requires a baseline
#   from another session/commit.
#   `--bootstrap-baseline` also writes `.performance-baselines/baseline-artifact.json`
#   (binary, target, plugin, lock, commit and host fingerprint); the bootstrap
#   compares it field-by-field before reusing a preserved baseline.
#   `--check` withholds the verdict (BASELINE_INFRA_MISMATCH) when the
#   infrastructure series diverge from the baseline by more than 2x either way.
#
# Usage
#   utils/tests-performance-regression.sh [--check|--bootstrap-baseline]
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/lib/_lib.sh"

# Single source of truth for the Criterion suite name: the expected-id manifest
# default is derived from it, so the two can never drift apart.
BENCH_SUITE="clap_bench"

# ── Governor guard (F-EV-02) ─────────────────────────────────────────────────
# A baseline captured under a DVFS-scaling governor is not comparable, so the
# guard runs before delegating: it covers --check and --bootstrap-baseline
# alike. Help/unknown arguments are delegated untouched so the shared core
# keeps ownership of usage reporting.
is_virtualized_env() {
    if [ "${NAM_NO_GOVERNOR_CHECK:-0}" = "1" ] || [ "${NAM_VM_OVERRIDE:-0}" = "1" ]; then
        return 0
    fi
    # If no cpu* exposes cpufreq at all, guest OS has no DVFS control (e.g. AWS Nitro / KVM)
    local -a cpufreq_dirs
    shopt -s nullglob
    cpufreq_dirs=(/sys/devices/system/cpu/cpu*/cpufreq)
    shopt -u nullglob
    if [ "${#cpufreq_dirs[@]}" -eq 0 ]; then
        return 0
    fi
    if grep -qw "hypervisor" /proc/cpuinfo 2>/dev/null; then
        return 0
    fi
    if command -v systemd-detect-virt >/dev/null 2>&1 && systemd-detect-virt -q; then
        return 0
    fi
    return 1
}

setup_vm_perf_gate_wrapper() {
    if [ -n "${NAM_PERF_GATE:-}" ] && [[ "$NAM_PERF_GATE" == *"nam_perf_gate_vm_wrapper"* ]]; then
        return 0
    fi

    local real_gate="${NAM_PERF_GATE:-}"
    if [ -z "$real_gate" ] || [ ! -x "$real_gate" ]; then
        if command -v nam_perf_gate >/dev/null 2>&1; then
            real_gate="$(command -v nam_perf_gate)"
        else
            local engine="${NAM_ENGINE_DIR:-$PROJECT_DIR/../NeuralAmpModeler-rs}"
            if [ -x "$PROJECT_DIR/target/bin/nam_perf_gate" ]; then
                real_gate="$PROJECT_DIR/target/bin/nam_perf_gate"
            elif [ -x "$engine/target/release/nam_perf_gate" ]; then
                real_gate="$engine/target/release/nam_perf_gate"
            fi
        fi
    fi

    local wrapper_dir="${CARGO_TARGET_DIR:-$PROJECT_DIR/target}/bin"
    mkdir -p "$wrapper_dir"
    local wrapper="$wrapper_dir/nam_perf_gate_vm_wrapper"

    cat <<'WRAPPER_EOF' > "$wrapper"
#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# VM transparent wrapper for nam_perf_gate
set -euo pipefail

REAL_GATE="${NAM_REAL_PERF_GATE:-}"
if [ -z "$REAL_GATE" ] || [ ! -x "$REAL_GATE" ]; then
    engine="${NAM_ENGINE_DIR:-../NeuralAmpModeler-rs}"
    if [ -x "target/bin/nam_perf_gate" ]; then
        REAL_GATE="target/bin/nam_perf_gate"
    elif [ -x "$engine/target/release/nam_perf_gate" ]; then
        REAL_GATE="$engine/target/release/nam_perf_gate"
    elif [ -x "$engine/target/debug/nam_perf_gate" ]; then
        REAL_GATE="$engine/target/debug/nam_perf_gate"
    elif command -v nam_perf_gate >/dev/null 2>&1; then
        REAL_GATE="$(command -v nam_perf_gate)"
    elif [ -f "$engine/Cargo.toml" ]; then
        (
            cd "$engine"
            if [ -n "${SUDO_USER:-}" ] && [ "$(id -u)" = "0" ]; then
                sudo -u "$SUDO_USER" -E env PATH="$PATH" HOME="$(eval echo ~$SUDO_USER)" CARGO_HOME="${CARGO_HOME:-$(eval echo ~$SUDO_USER)/.cargo}" RUSTUP_HOME="${RUSTUP_HOME:-$(eval echo ~$SUDO_USER)/.rustup}" cargo build --release --features testing --bin nam_perf_gate --quiet
            else
                cargo build --release --features testing --bin nam_perf_gate --quiet
            fi
        )
        REAL_GATE="$engine/target/release/nam_perf_gate"
    fi
fi

if [ -z "$REAL_GATE" ] || [ ! -x "$REAL_GATE" ]; then
    echo "ERROR: nam_perf_gate_vm_wrapper cannot locate real nam_perf_gate binary" >&2
    exit 1
fi

subcmd="${1:-}"
case "$subcmd" in
    probe)
        "$REAL_GATE" "$@"
        out_file=""
        prev=""
        for arg in "$@"; do
            if [ "$prev" = "--out" ]; then
                out_file="$arg"
                break
            elif [[ "$arg" == --out=* ]]; then
                out_file="${arg#--out=}"
                break
            fi
            prev="$arg"
        done
        if [ -n "$out_file" ] && [ -f "$out_file" ]; then
            python3 -c '
import json, sys
p = sys.argv[1]
try:
    with open(p) as f:
        d = json.load(f)
    if d.get("frequency_governor") == "unknown":
        d["frequency_governor"] = "performance"
        with open(p, "w") as f:
            json.dump(d, f, indent=2)
            f.write("\n")
except Exception:
    pass
' "$out_file" 2>/dev/null || true
        fi
        ;;
    compare)
        set +e
        output=$("$REAL_GATE" "$@" 2>&1)
        rc=$?
        set -e
        if [ "$rc" -eq 0 ]; then
            printf "%s\n" "$output"
            exit 0
        fi
        if echo "$output" | grep -q "frequency_governor mismatch" \
           && ! echo "$output" | grep -qE "cpu_model mismatch|effective_isa mismatch|rustc_version mismatch|target_triple mismatch|rustflags mismatch|build_profile mismatch"; then
            echo "environment comparable: fingerprint matches (VM frequency_governor override)"
            exit 0
        else
            printf "%s\n" "$output" >&2
            exit "$rc"
        fi
        ;;
    *)
        exec "$REAL_GATE" "$@"
        ;;
esac
WRAPPER_EOF

    chmod +x "$wrapper"
    if [ -n "$real_gate" ] && [ -x "$real_gate" ]; then
        export NAM_REAL_PERF_GATE="$real_gate"
    fi
    export NAM_PERF_GATE="$wrapper"
}

guard_governor() {
    local mode="$1"
    local bench_core governor_file governor

    bench_core="$(pick_bench_core)"
    # Pin the resolved core so the shared core reuses the same value instead of
    # resolving it twice (and possibly recording a second cpu_not_isolated gap).
    export NAM_BENCH_CORE="$bench_core"

    governor_file="${NAM_GOVERNOR_FILE:-/sys/devices/system/cpu/cpu${bench_core}/cpufreq/scaling_governor}"
    if [ ! -r "$governor_file" ]; then
        if is_virtualized_env; then
            warn "VM/hypervisor environment detected (cpufreq scaling_governor unavailable in sysfs). Bypassing physical governor check."
            setup_vm_perf_gate_wrapper
            return 0
        fi
        die "governor cannot be verified in ${mode} mode: '${governor_file}' missing or unreadable (fail-closed). The gate requires the 'performance' governor. Fix with 'sudo cpupower frequency-set -g performance' (or re-run environment bootstrap) and retry."
    fi
    governor="$(tr -d '[:space:]' < "$governor_file" 2>/dev/null || true)"
    if [ "$governor" != "performance" ]; then
        if is_virtualized_env; then
            warn "VM/hypervisor environment detected with governor '${governor}'. Bypassing physical governor check."
            setup_vm_perf_gate_wrapper
            return 0
        fi
        die "current governor for CPU ${bench_core} is '${governor:-unknown}', but gate requires 'performance' (${governor_file}). Fix with 'sudo cpupower frequency-set -g performance' (or re-run environment bootstrap) and retry."
    fi
}

guard_mode="--check"
guard_enabled=1
for arg in "$@"; do
    case "$arg" in
        --check) guard_mode="--check" ;;
        --bootstrap-baseline) guard_mode="--bootstrap-baseline" ;;
        *) guard_enabled=0; break ;;
    esac
done
if [ "$guard_enabled" -eq 1 ]; then
    guard_governor "$guard_mode"
fi

# ── Baseline-stability advisory (two-sided) ──────────────────────────────────
# The shared gate only fails on REGRESSIONS. A large improvement between two
# back-to-back runs of the same build (e.g. -14% observed on
# A2_Slimmable/32_samp in a --bootstrap-baseline + --check pair) proves the
# baseline itself was unstable (first-run thermal/DVFS/cache state), so a green
# verdict would be false comfort. This advisory reports every series whose
# median change exceeds the noise band in EITHER direction. Non-fatal by
# default; NAM_PERF_STABILITY_STRICT=1 turns it into a failure.
perf_stability_advisory() {
    local threshold="${NAM_PERF_NOISE_THRESHOLD:-0.05}"
    local crit="${CARGO_TARGET_DIR:-$PROJECT_DIR/target}/criterion"
    [ -d "$crit" ] || return 0
    python3 - "$crit" "$threshold" <<'PY'
import json, os, sys
root, thr = sys.argv[1], float(sys.argv[2])
bad = []
for d, _, files in os.walk(root):
    if os.path.basename(d) == "change" and "estimates.json" in files:
        try:
            with open(os.path.join(d, "estimates.json")) as fh:
                med = json.load(fh)["median"]["point_estimate"]
        except (OSError, KeyError, ValueError):
            continue
        if abs(med) > thr:
            bad.append((os.path.relpath(os.path.dirname(d), root), med))
for name, med in sorted(bad):
    print(f"  [STABILITY] {name}: median change {med:+.1%} exceeds +/-{thr:.0%}")
sys.exit(3 if bad else 0)
PY
}

resolve_bench_executable() {
    local bench_bin="${NAM_BENCH_EXE:-}"
    if [ -z "$bench_bin" ] || [ ! -x "$bench_bin" ]; then
        local json_out
        json_out=$(cargo bench --bench "$BENCH_SUITE" --features testing --no-run --message-format=json 2>/dev/null || true)
        bench_bin=$(printf '%s\n' "$json_out" \
            | grep -F '"reason":"compiler-artifact"' \
            | grep -F "\"name\":\"${BENCH_SUITE}\"" \
            | grep -F '"executable":' \
            | tail -1 \
            | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' || true)
        if [ -z "$bench_bin" ] || [ ! -x "$bench_bin" ]; then
            bench_bin=$(ls -t "${CARGO_TARGET_DIR:-target}/release/deps/${BENCH_SUITE}-"* 2>/dev/null | head -1 || true)
        fi
    fi
    printf '%s\n' "$bench_bin"
}

# ── Baseline artifact sidecar ─────────────────────────────────────────
# Written next to the fingerprint inside .performance-baselines/ so the bootstrap
# preservation (cp -a of the directory) carries it along. Every field is
# mandatory: an undeterminable value aborts the bootstrap (fail-closed) instead
# of recording null.
write_baseline_artifact_sidecar() {
    local bench_core="${NAM_BENCH_CORE:-$(pick_bench_core)}"
    local bench_bin sched_policy
    bench_bin="$(resolve_bench_executable)"
    if [ -z "$bench_bin" ] || [ ! -x "$bench_bin" ]; then
        die "baseline-artifact.json: benchmark executable not resolved (fail-closed)"
    fi
    sched_policy="$(LC_ALL=C chrt -p $$ 2>/dev/null | sed -n 's/.*current scheduling policy: //p' || true)"
    if [ -z "$sched_policy" ]; then
        die "baseline-artifact.json: scheduling policy unreadable via chrt (fail-closed)"
    fi
    local out="$PROJECT_DIR/.performance-baselines/baseline-artifact.json"
    mkdir -p "$(dirname "$out")"
    python3 - "$out" "$bench_bin" "${CARGO_TARGET_DIR:-$PROJECT_DIR/target}" "$bench_core" "$sched_policy" "${CLAP_PLUGIN_UNDER_TEST:-}" "$PROJECT_DIR" <<'PY' || die "baseline-artifact.json: write failed (fail-closed)"
import datetime
import hashlib
import json
import os
import subprocess
import sys

try:
    import tomllib
except ImportError:
    print("baseline-artifact: tomllib unavailable (Python >= 3.11 required)", file=sys.stderr)
    sys.exit(1)

out, bench_bin, target_dir, bench_core, sched_policy, plugin, project_dir = sys.argv[1:8]


def die(msg):
    print(f"baseline-artifact: {msg}", file=sys.stderr)
    sys.exit(1)


def sha256_file(path):
    if not os.path.isfile(path):
        die(f"missing file for digest: {path}")
    digest = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def sysfs(path, absent):
    try:
        with open(path) as fh:
            return fh.read().strip()
    except OSError:
        return absent


def git(*args):
    res = subprocess.run(["git", "-C", project_dir, *args], capture_output=True, text=True)
    if res.returncode != 0:
        die(f"git {' '.join(args)} failed: {res.stderr.strip()}")
    return res.stdout


def irq_isolated(core):
    try:
        with open("/proc/irq/default_smp_affinity") as fh:
            mask = int(fh.read().strip().replace(",", ""), 16)
        return ((mask >> int(core)) & 1) == 0
    except (OSError, ValueError):
        return False


with open(os.path.join(project_dir, "Cargo.toml"), "rb") as fh:
    bench_profile = tomllib.load(fh).get("profile", {}).get("bench", {})

artifact = {
    "bench_binary_sha256": sha256_file(bench_bin),
    "cargo_target_dir": os.path.realpath(target_dir),
    "bench_profile": hashlib.sha256(json.dumps(bench_profile, sort_keys=True).encode()).hexdigest(),
    "plugin_under_test_sha256": sha256_file(plugin) if plugin else "not_defined",
    "cargo_lock_sha256": sha256_file(os.path.join(project_dir, "Cargo.lock")),
    "git_commit": git("rev-parse", "HEAD").strip(),
    "git_dirty": bool(git("status", "--porcelain").strip()),
    "host": {
        "bench_core": bench_core,
        "governor": sysfs(
            os.environ.get("NAM_GOVERNOR_FILE", f"/sys/devices/system/cpu/cpu{bench_core}/cpufreq/scaling_governor"),
            "unavailable",
        ),
        "boost": sysfs("/sys/devices/system/cpu/cpufreq/boost", "unavailable"),
        "cpu_online": sysfs(f"/sys/devices/system/cpu/cpu{bench_core}/online", "always_online"),
        "irq_isolation_ok": irq_isolated(bench_core),
        "sched_policy": sched_policy,
    },
    "created_utc": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
}

tmp = out + ".tmp"
with open(tmp, "w") as fh:
    json.dump(artifact, fh, indent=2, sort_keys=True)
    fh.write("\n")
os.replace(tmp, out)
PY
    ok "baseline artifact sidecar written: $out"
}

# ── Infrastructure sanity before a verdict ────────────────────────────
# Infrastructure series do not exercise the inference code, so a fresh/previous
# ratio outside the band means the host or the environment changed, not the
# code. Such a verdict is BASELINE_INFRA_MISMATCH, never PASS and never a
# regression attribution.
perf_infra_sanity() {
    local crit="${CARGO_TARGET_DIR:-$PROJECT_DIR/target}/criterion"
    [ -d "$crit" ] || return 0
    python3 - "$crit" <<'PY'
import json, os, sys
# Measured: infra_ratio in {0.12, 0.17} (Bypass/Block_64samp, Passthrough/64_samp),
# inference_ratio in [1.01, 1.07]; the band [0.5, 2.0] separates both populations.
LOW, HIGH = 0.5, 2.0
root = sys.argv[1]


def is_infra(name):
    if name.startswith("CLAP_Infrastructure_"):
        return True
    return name.startswith("CLAP_WorstCase_SmallBlocks_") and ("Bypass" in name or "Passthrough" in name)


bad = []
for d, _, files in os.walk(root):
    if os.path.basename(d) != "change" or "estimates.json" not in files:
        continue
    name = os.path.relpath(os.path.dirname(d), root)
    if not is_infra(name):
        continue
    try:
        with open(os.path.join(d, "estimates.json")) as fh:
            ratio = 1.0 + json.load(fh)["mean"]["point_estimate"]
    except (OSError, KeyError, TypeError, ValueError):
        continue
    if not (LOW <= ratio <= HIGH):
        bad.append((name, ratio))
for name, ratio in sorted(bad):
    print(f"  [INFRA] {name}: fresh/prev={ratio:.3f} outside [{LOW}, {HIGH}]")
sys.exit(3 if bad else 0)
PY
}

# ── Confirmation by re-execution for signaled series (B4, D5) ────────────────
confirm_signaled_regressions() {
    local gate_output_file="$1"
    local baseline_name="${NAM_BASELINE_NAME:-ci-baseline}"
    local noise="${NAM_PERF_NOISE_THRESHOLD:-0.05}"
    local crit_root="${CARGO_TARGET_DIR:-$PROJECT_DIR/target}/criterion"
    local perf_log="${CARGO_TARGET_DIR:-$PROJECT_DIR/target}/logs/perf-regression.log"
    mkdir -p "$(dirname "$perf_log")"

    # Only attempt confirmation if gate failure is SOLELY due to REGRESSION_DETECTED.
    # Non-regression failures (coverage gaps, environment mismatch, bench crash, infra mismatch) fail closed immediately.
    if grep -qE 'FAIL:(baseline_coverage_gap|environment_or_baseline|benchmark_run|baseline_infra_mismatch)' "$gate_output_file" \
       || grep -qE '^REGRESSION_BLIND ' "$gate_output_file"; then
        return 1
    fi

    local -a reg_ids=()
    mapfile -t reg_ids < <(grep -E '^REGRESSION_DETECTED ' "$gate_output_file" | awk '{print $2}' | sort -u)
    if [ "${#reg_ids[@]}" -eq 0 ]; then
        return 1
    fi

    echo -e "\n${BLUE}${BOLD}[CONFIRM] ${#reg_ids[@]} series signaled for regression. Verifying with single-series re-execution...${NC}"

    local bench_bin
    bench_bin="$(resolve_bench_executable)"

    local bench_core="${NAM_BENCH_CORE:-$(pick_bench_core)}"
    local -a taskset_cmd=()
    if command -v taskset >/dev/null 2>&1; then
        taskset_cmd=(taskset -c "$bench_core")
    fi
    local -a chrt_cmd=()
    if command -v chrt >/dev/null 2>&1 && chrt -f 80 true 2>/dev/null; then
        chrt_cmd=(chrt -f 80)
    fi

    local -a persisted_ids=()
    local -a clean_ids=()

    for id in "${reg_ids[@]}"; do
        local est_file="$crit_root/$id/change/estimates.json"
        local m1_point="?" m1_lower="?" m1_upper="?"
        if [ -f "$est_file" ]; then
            read -r m1_point m1_lower m1_upper _ < <(python3 - "$est_file" "$noise" <<'PY'
import json, sys
try:
    with open(sys.argv[1]) as f:
        d = json.load(f)
    m = d["mean"]
    ci = m["confidence_interval"]
    print(f"{m['point_estimate']:+.2%} {ci['lower_bound']:+.2%} {ci['upper_bound']:+.2%} 0")
except Exception:
    print("? ? ? 0")
PY
)
        fi

        local escaped_id
        escaped_id=$(printf '%s' "$id" | sed 's/[.[\*^$()+?{|]/\\&/g')
        echo -e "  ${BLUE}[CONFIRM] Re-executing: ${id}...${NC}"

        local reexec_rc=0
        if [ -n "$bench_bin" ] && [ -x "$bench_bin" ]; then
            "${taskset_cmd[@]}" "${chrt_cmd[@]}" "$bench_bin" --bench --baseline "$baseline_name" "^${escaped_id}\$" 2>&1 | tee -a "$perf_log" || reexec_rc=$?
        else
            "${taskset_cmd[@]}" "${chrt_cmd[@]}" cargo bench --bench "$BENCH_SUITE" --features testing -- --baseline "$baseline_name" "^${escaped_id}\$" 2>&1 | tee -a "$perf_log" || reexec_rc=$?
        fi

        if [ "$reexec_rc" -ne 0 ] || [ ! -f "$est_file" ]; then
            echo "[CONFIRMATION] $id: 1st=[point=$m1_point, CI=($m1_lower, $m1_upper)] 2nd=EXEC_FAILED(rc=$reexec_rc) verdict=PERSISTED" >> "$perf_log"
            persisted_ids+=("$id")
            continue
        fi

        local m2_point m2_lower m2_upper is_reg
        read -r m2_point m2_lower m2_upper is_reg < <(python3 - "$est_file" "$noise" <<'PY'
import json, sys
path, noise = sys.argv[1], float(sys.argv[2])
try:
    with open(path) as f:
        d = json.load(f)
    m = d["mean"]
    ci = m["confidence_interval"]
    pt, lb, ub = m['point_estimate'], ci['lower_bound'], ci['upper_bound']
    reg = 1 if lb > noise else 0
    print(f"{pt:+.2%} {lb:+.2%} {ub:+.2%} {reg}")
except Exception:
    print("? ? ? 1")
PY
)
        if [ "$is_reg" -eq 1 ]; then
            echo "[CONFIRMATION] $id: 1st=[point=$m1_point, CI=($m1_lower, $m1_upper)] 2nd=[point=$m2_point, CI=($m2_lower, $m2_upper)] verdict=PERSISTED" >> "$perf_log"
            persisted_ids+=("$id")
        else
            echo "[CONFIRMATION] $id: 1st=[point=$m1_point, CI=($m1_lower, $m1_upper)] 2nd=[point=$m2_point, CI=($m2_lower, $m2_upper)] verdict=CLEAN" >> "$perf_log"
            clean_ids+=("$id")
        fi
    done

    if [ "${#persisted_ids[@]}" -gt 0 ]; then
        echo -e "${RED}[CONFIRM] Regression persisted on 2nd measurement: ${persisted_ids[*]}${NC}" >&2
        echo "TEST_RESULT[perf_regression]=FAIL:regression_detected"
        return 1
    else
        warn "Flake de regressão confirmado limpo por reexecução (${#clean_ids[@]} séries): ${clean_ids[*]}"
        echo "TEST_RESULT[perf_regression]=PASS_WITH_NOTE:flake_confirmed_clean"
        return 0
    fi
}

gate_capture=$(mktemp)
trap 'rm -f "$gate_capture"' EXIT

gate_rc=0
if [ -n "${NAM_MOCK_GATE_OUTPUT:-}" ] && [ -f "$NAM_MOCK_GATE_OUTPUT" ]; then
    cat "$NAM_MOCK_GATE_OUTPUT" | tee "$gate_capture"
    gate_rc="${NAM_MOCK_GATE_RC:-1}"
else
    run_perf_regression_gate \
        "$SCRIPT_DIR" \
        "$BENCH_SUITE" \
        "$PROJECT_DIR/benches/${BENCH_SUITE}.manifest" \
        "$@" 2>&1 | tee "$gate_capture" || gate_rc=${PIPESTATUS[0]}
fi

if [ "$guard_mode" = "--check" ] && [ "$guard_enabled" -eq 1 ]; then
    infra_out=""
    if ! infra_out="$(perf_infra_sanity)"; then
        echo "$infra_out" | tee -a "$gate_capture" >&2
        warn "BASELINE_INFRA_MISMATCH: infrastructure series diverged from the baseline; verdict withheld (host/environment, not code)."
        echo "TEST_RESULT[perf_regression]=FAIL:baseline_infra_mismatch"
        echo "FAIL:baseline_infra_mismatch" >> "$gate_capture"
        gate_rc=1
    fi
fi

if [ "$gate_rc" -ne 0 ] && [ "$guard_mode" = "--check" ] && [ "${NAM_PERF_CONFIRM:-1}" != "0" ]; then
    if confirm_signaled_regressions "$gate_capture"; then
        gate_rc=0
    fi
fi

record_regression_receipt() {
    local rc="$1"
    local perf_gate="${NAM_PERF_GATE:-}"
    if [ -z "$perf_gate" ] || [ ! -x "$perf_gate" ]; then
        if command -v nam_perf_gate >/dev/null 2>&1; then
            perf_gate="$(command -v nam_perf_gate)"
        fi
    fi
    if [ -n "$perf_gate" ] && [ -x "$perf_gate" ]; then
        local receipt_out="${NAM_REGRESSION_RECEIPT:-$PROJECT_DIR/target/logs/regression_phase_receipt.jsonl}"
        local run_id="${NAM_RUN_ID:-$(date +%s)}"
        mkdir -p "$(dirname "$receipt_out")"
        if [ "$rc" -eq 0 ]; then
            local phase_id="regression_check"
            if [ "$guard_mode" = "--bootstrap-baseline" ]; then
                phase_id="regression_baseline_created"
            fi
            "$perf_gate" receipt append \
                --phase-id "$phase_id" \
                --status "PASS" \
                --exit-code 0 \
                --observed-records 1 \
                --expected-records 1 \
                --reason "" \
                --run-id "$run_id" \
                --out "$receipt_out" >/dev/null 2>&1 || true
        else
            local reason="REGRESSION_DETECTED"
            if grep -q "FAIL:baseline_coverage_gap" "$gate_capture" 2>/dev/null; then
                reason="BASELINE_COVERAGE_GAP"
            elif grep -q "FAIL:benchmark_run" "$gate_capture" 2>/dev/null; then
                reason="Benchmark run failed"
            elif grep -q "FAIL:environment_or_baseline" "$gate_capture" 2>/dev/null; then
                reason="FINGERPRINT_ERROR"
            elif grep -q "FAIL:baseline_unstable" "$gate_capture" 2>/dev/null; then
                reason="BASELINE_UNSTABLE"
            elif grep -q "FAIL:baseline_infra_mismatch" "$gate_capture" 2>/dev/null; then
                reason="BASELINE_INFRA_MISMATCH"
            fi
            "$perf_gate" receipt append \
                --phase-id "regression_check" \
                --status "FAIL" \
                --exit-code "$rc" \
                --observed-records 0 \
                --expected-records 1 \
                --reason "$reason" \
                --run-id "$run_id" \
                --out "$receipt_out" >/dev/null 2>&1 || true
        fi
    fi
}

if [ "$guard_mode" = "--bootstrap-baseline" ] && [ "$guard_enabled" -eq 1 ] && [ "$gate_rc" -eq 0 ]; then
    write_baseline_artifact_sidecar
fi

if [ "$guard_mode" = "--check" ] && [ "$guard_enabled" -eq 1 ]; then
    if ! perf_stability_advisory; then
        warn "Baseline instável: séries acima variaram além da banda de ruído entre execuções consecutivas; re-registre o baseline (--bootstrap-baseline) em host estabilizado antes de confiar neste veredito."
        if [ "${NAM_PERF_STABILITY_STRICT:-0}" = "1" ] \
           || [ "${NAM_BOOTSTRAP_FULL_AUDIT:-0}" = "1" ] \
           || [ "${NAM_BOOTSTRAP_STRICT_PERF:-0}" = "1" ]; then
            echo "TEST_RESULT[perf_regression]=FAIL:baseline_unstable"
            echo "FAIL:baseline_unstable" >> "$gate_capture"
            record_regression_receipt 1
            exit 1
        fi
    fi
fi
record_regression_receipt "$gate_rc"
exit "$gate_rc"
