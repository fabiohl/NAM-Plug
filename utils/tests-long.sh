#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Long QA Suite for NAM-Plug — nightly/pre-release certification.
#
# Executes the heavy `#[ignore]` stress tests that `tests-quick.sh` deliberately
# leaves out (GC/SPSC cascade, teardown drain, multi-instance RT priority, GUI/X11).
# Each phase runs in isolation, persists its log to target/logs/long-phaseN.log,
# and appends a structured JSONL line to target/logs/long-audit-receipt.jsonl.
#
# Evidence hygiene: a --dry-run rehearsal must never destroy prior evidence.
# It writes exclusively to target/logs/long-audit-receipt.dryrun.jsonl and
# leaves any previous phase logs and receipts untouched; truncation of the
# evidence paths happens solely in a real run.
#
# Build optimisation:
#   Before any timed phase begins, a single `cargo test --no-run` pass pre-compiles
#   all test binaries in release mode, ensuring 100 % artifact reuse by subsequent
#   phases. This eliminates the cold-cache compilation overhead (~7 min) from the
#   phase timing windows so that reported durations reflect pure test execution time.
#
# Thermal cooldown:
#   After pre-compilation, NAM_THERMAL_COOLDOWN_S seconds of idle time (default: 0,
#   i.e. disabled) allow the CPU to return to base frequency and temperature before
#   SCHED_FIFO / taskset-pinned tests run, preserving measurement determinism on
#   isolated cores.
#
# Phases:
#   0. Pre-build    — `--no-run` warm-up; not timed individually
#   1. GC Stress    — 1000 model swaps + drain-on-destroy leak check
#   2. Teardown     — 25 swaps without housekeeping, off-RT drain verification
#   3. Multi-instance RT priority — 10 instances, SCHED_FIFO detection
#   4. GUI/X11      — headless Xvfb GUI lifecycle, XEmbed cycles, clipboard
#                     (conditional: skipped with a gap warning if xvfb-run absent)
#
# Usage:
#   ./utils/tests-long.sh [--artifact PATH] [--strict-pre-release] [--nocapture] [--dry-run] [--gui]
#   chrt -f 80 ./utils/tests-long.sh --strict-pre-release  # official pre-release ceremony
#
# Every phase runs against ONE resolved artifact (`CLAP_PLUGIN_UNDER_TEST`),
# defaulting to the installed `~/.clap/nam_plug.clap`; the script records its
# path and SHA-256 in every receipt line. In `--strict-pre-release` mode it also
# exports `CLAP_STRICT_ARTIFACT=1` so the artifact validator fails closed.
#
# Environment variables:
#   CLAP_PLUGIN_UNDER_TEST      Artifact under test (overrides the default install;
#                               `--artifact PATH` takes precedence over it)
#   NAM_BENCH_CORE              CPU core for affinity pinning (default: isolated core or nproc/2)
#   NAM_THERMAL_COOLDOWN_S      Seconds to idle after pre-compilation (default: 0)
#   NAM_GUI_PHASE_AUTO          Set to 0 to suppress auto-trigger of GUI phase (default: 1)

set -euo pipefail

STRICT_PRE_RELEASE=0
NOCAPTURE=0
DRY_RUN=0
RUN_GUI=0
CLI_ARTIFACT=""

while [ "$#" -gt 0 ]; do
    case "$1" in
        --strict-pre-release)
            STRICT_PRE_RELEASE=1
            ;;
        --nocapture)
            NOCAPTURE=1
            ;;
        --dry-run)
            DRY_RUN=1
            ;;
        --gui)
            RUN_GUI=1
            ;;
        --artifact)
            if [ "$#" -lt 2 ] || [ -z "$2" ]; then
                echo "--artifact requires a path" >&2
                exit 1
            fi
            CLI_ARTIFACT="$2"
            shift
            ;;
        --artifact=*)
            CLI_ARTIFACT="${1#--artifact=}"
            ;;
        --help|-h)
            echo "Usage: $(basename "$0") [--artifact PATH] [--strict-pre-release] [--nocapture] [--dry-run] [--gui]"
            echo ""
            echo "Long audit suite for NAM-Plug — runs heavy #[ignore] stress tests."
            echo ""
            echo "Options:"
            echo "  --artifact PATH       Artifact under test (default: ~/.clap/nam_plug.clap)"
            echo "  --strict-pre-release  Fail closed on any gap or inconclusive phase (rejects --dry-run)"
            echo "  --nocapture           Pass --nocapture to cargo test for verbose output (phases 1-3 always do)"
            echo "  --dry-run             Print planned commands and write SIMULATED receipts to target/logs/long-audit-receipt.dryrun.jsonl"
            echo "  --gui                 Run the GUI/Xvfb phase (Phase 4) explicitly"
            echo "  -h, --help            Show this help and exit"
            echo ""
            echo "Environment variables:"
            echo "  CLAP_PLUGIN_UNDER_TEST    Artifact under test (overrides the default install)"
            echo "  NAM_BENCH_CORE            CPU core for affinity pinning (default: isolated core or nproc/2)"
            echo "  NAM_THERMAL_COOLDOWN_S    Idle seconds after pre-compilation (default: 0)"
            echo "  NAM_GUI_PHASE_AUTO        Set 0 to suppress auto GUI trigger (default: 1)"
            echo ""
            echo "Receipt: target/logs/long-audit-receipt.jsonl (or target/logs/long-audit-receipt.dryrun.jsonl in --dry-run)"
            exit 0
            ;;
        *)
            echo "Unknown option: $1" >&2
            echo "Usage: $(basename "$0") [--artifact PATH] [--strict-pre-release] [--nocapture] [--dry-run] [--gui]" >&2
            exit 1
            ;;
    esac
    shift
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT_PATH="$SCRIPT_DIR/$(basename "${BASH_SOURCE[0]}")"
LIB_DIR="$SCRIPT_DIR/lib"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

# Detect whether xvfb-run is available for GUI phase auto-trigger.
HAVE_XVFB_RUN=0
if command -v xvfb-run >/dev/null 2>&1; then
    HAVE_XVFB_RUN=1
    # Auto-enable GUI phase when xvfb-run is present, unless overridden to 0.
    if [ "${RUN_GUI}" = "0" ] && [ "${NAM_GUI_PHASE_AUTO:-1}" = "1" ]; then
        RUN_GUI=1
    fi
fi

if [ -f "$LIB_DIR/_lib.sh" ]; then
    # shellcheck source=utils/lib/_lib.sh
    PHASE_TOTAL=4
    export PHASE_TOTAL
    source "$LIB_DIR/_lib.sh"
else
    RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
    BLUE='\033[0;34m'; CYAN='\033[0;36m'; BOLD='\033[1m'; NC='\033[0m'
    PHASE_NUM=0
    PHASE_TOTAL=4
    phase() {
        PHASE_NUM=$((PHASE_NUM + 1))
        echo -e "\n${BLUE}${BOLD}[${PHASE_NUM}/${PHASE_TOTAL}]${NC} $*"
    }
    die() {
        echo -e "${RED}${BOLD}[FATAL]${NC} $*" >&2
        exit 1
    }
    ok() { echo -e "  ${GREEN}OK${NC} $*"; }
    warn() { echo -e "  ${YELLOW}ⓘ${NC} $*"; }
    PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
    cd "$PROJECT_DIR" || exit 1
fi

if [ "${NAM_LIB_NO_CD:-0}" != "1" ]; then
    cd "$PROJECT_DIR" || exit 1
fi

# ── Artifact under test (E-A: provenance chain) ─────────────────────────────
# Every phase must test the SAME binary that the operator installed. Resolve it
# once, fail closed when it is absent, and export it to every cargo invocation.
RESOLVED_ARTIFACT="${CLI_ARTIFACT:-${CLAP_PLUGIN_UNDER_TEST:-$HOME/.clap/nam_plug.clap}}"
if [ ! -f "$RESOLVED_ARTIFACT" ]; then
    echo -e "${RED}${BOLD}❌ CLAP artifact not found: $RESOLVED_ARTIFACT${NC}" >&2
    echo "  Build and install it first (utils/build-release.sh --install) or pass --artifact PATH." >&2
    exit 1
fi
ARTIFACT_PATH="$(readlink -f -- "$RESOLVED_ARTIFACT" 2>/dev/null || printf '%s' "$RESOLVED_ARTIFACT")"
ARTIFACT_SHA256="$(sha256sum "$ARTIFACT_PATH" | cut -d' ' -f1)"
export CLAP_PLUGIN_UNDER_TEST="$ARTIFACT_PATH"
export ARTIFACT_PATH ARTIFACT_SHA256
if [ "$STRICT_PRE_RELEASE" = "1" ]; then
    export CLAP_STRICT_ARTIFACT=1
fi

# ── Memory guard (OOM containment) ──────────────────────────────────────────
# Phase 0 pre-compiles every test binary in release (fat LTO). Re-exec inside a
# transient, memory-capped systemd scope so that peak is contained in its own
# cgroup instead of triggering a global OOM that kills the IDE. Fail-open when
# unavailable; tune with NAM_MEM_GUARD_MAX on larger hosts.
if [ "${NAM_MEM_GUARD:-0}" != "1" ] && [ "${NAM_NO_MEM_GUARD:-0}" != "1" ] && [ "$DRY_RUN" = "0" ]; then
    export NAM_MEM_GUARD=1
    export NAM_MEM_GUARD_HIGH="${NAM_MEM_GUARD_HIGH:-6G}"
    export NAM_MEM_GUARD_MAX="${NAM_MEM_GUARD_MAX:-8G}"
    export NAM_MEM_GUARD_SWAP_MAX="${NAM_MEM_GUARD_SWAP_MAX:-8G}"
    exec "$SCRIPT_DIR/mem-guard.sh" "$SCRIPT_PATH" "$@"
fi

if [ "$STRICT_PRE_RELEASE" = "1" ] && [ "$DRY_RUN" = "1" ]; then
    echo -e "${RED}${BOLD}❌ --strict-pre-release rejects simulated receipts (--dry-run is active).${NC}" >&2
    exit 1
fi

ORIG_PARANOID="$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo "2")"
PARANOID_MODIFIED=false
if [ "$DRY_RUN" = "1" ]; then
    RECEIPT_FILE="target/logs/long-audit-receipt.dryrun.jsonl"
else
    RECEIPT_FILE="target/logs/long-audit-receipt.jsonl"
fi

cleanup() {
    local rc=$?
    if [ "$PARANOID_MODIFIED" = true ]; then
        echo -e "\nRestoring kernel.perf_event_paranoid to $ORIG_PARANOID..."
        sudo sysctl -q -w kernel.perf_event_paranoid="$ORIG_PARANOID" 2>/dev/null || true
    fi
    return $rc
}

trap 'rc=$?; cleanup; if [ $rc -eq 124 ]; then echo -e "\n${RED}${BOLD}❌ TIMEOUT at line $LINENO (phase ${PHASE_NUM:-?}/${PHASE_TOTAL})${NC}"; else echo -e "\n${RED}${BOLD}❌ Interrupted (SIGINT/SIGTERM) at line $LINENO (phase ${PHASE_NUM:-?}/${PHASE_TOTAL})${NC}"; fi; exit $rc' INT TERM
trap 'rc=$?; if [ $rc -ne 0 ]; then echo -e "\n${RED}${BOLD}❌ Unexpected error: \"$BASH_COMMAND\" failed at line $LINENO with status $rc (phase ${PHASE_NUM:-?}/${PHASE_TOTAL})${NC}"; fi; cleanup; exit $rc' ERR
trap cleanup EXIT

BENCH_CORE="$(pick_bench_core)"
NUM_CORES="$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 1)"
HAS_TASKSET=0
if command -v taskset >/dev/null 2>&1; then
    HAS_TASKSET=1
fi

# Phases 1-3 always run with `--nocapture`: the artifact validator prints the
# `=== CLAP Artifact ===` provenance block through `eprintln!`, which libtest
# would otherwise swallow. Keeping it in `long-phase*.log` is the evidence that
# the phase tested the resolved artifact.
NOCAPTURE_FLAG="--nocapture"

mkdir -p target/logs target/test_crashes
export NAM_CRASH_DIR="${PROJECT_DIR}/target/test_crashes"
if [ "$DRY_RUN" != "1" ]; then
    rm -f target/logs/long-phase1.log \
          target/logs/long-phase2.log \
          target/logs/long-phase3.log \
          target/logs/long-phase4.log \
          "$RECEIPT_FILE"
else
    rm -f "$RECEIPT_FILE"
fi

: > "$RECEIPT_FILE"

# ── Unified engine receipt schema (testing::receipt) ─────────────────────────
# The long receipt is emitted through the Rust `plug_long_receipt` binary — the
# same schema the engine and NAM-Audio-Pipe emit — so a single semantic verifier
# (plug_long_receipt validate) can certify it without a substring search. The
# shell never hand-serializes JSON.
LONG_RECEIPT_FAILED=0
PLUG_RECEIPT_BIN=""
ensure_plug_receipt_bin() {
    if [ -n "$PLUG_RECEIPT_BIN" ] && [ -x "$PLUG_RECEIPT_BIN" ]; then
        return 0
    fi
    if ! cargo build --quiet --features testing --bin plug_long_receipt >/dev/null 2>&1; then
        return 1
    fi
    PLUG_RECEIPT_BIN="target/debug/plug_long_receipt"
    [ -x "$PLUG_RECEIPT_BIN" ]
}

emit_receipt() {
    local phase_id="$1"
    local name="$2"
    local status="$3"
    local duration_ms="$4"
    local log_file="${5:-}"
    # Declared vs exercised: phases 1/2/4 run only `--lib` tests that never load
    # the shared object; phase 3 includes `cargo test --test clap`, whose
    # multi-instance test `dlopen`s the installed artifact.
    local artifact_exercised="${6:-false}"
    local mapped="$status"
    local gaps_arg=""
    # The engine vocabulary has no GAP phase status: a declared gap is a
    # SKIPPED phase carrying a typed gap reason.
    if [ "$status" = "GAP" ]; then
        mapped="SKIPPED"
        gaps_arg="$phase_id:declared_gap"
    fi
    if ! ensure_plug_receipt_bin; then
        LONG_RECEIPT_FAILED=1
        return 0
    fi
    local -a args=(append --phase-id "$phase_id" --name "$name" --status "$mapped"
                   --duration-ms "$duration_ms" --out "$RECEIPT_FILE"
                   --artifact-path "$ARTIFACT_PATH" --artifact-sha256 "$ARTIFACT_SHA256"
                   --artifact-exercised "$artifact_exercised")
    [ -n "$log_file" ] && [ -f "$log_file" ] && args+=(--log "$log_file")
    [ -n "$gaps_arg" ] && args+=(--gaps "$gaps_arg")
    if ! "$PLUG_RECEIPT_BIN" "${args[@]}"; then
        LONG_RECEIPT_FAILED=1
    fi
}

maybe_taskset() {
    if [ "$HAS_TASKSET" = "1" ] && [ -n "${BENCH_CORE:-}" ]; then
        echo "taskset -c $BENCH_CORE"
    else
        echo ""
    fi
}

run_cargo_test() {
    local log_file="$1"
    shift
    local cmd_prefix=""
    if [ "${1:-}" = "--affinity" ]; then
        cmd_prefix="$(maybe_taskset)"
        shift
    fi
    local full_cmd
    if [ -n "$cmd_prefix" ]; then
        full_cmd="$cmd_prefix cargo test --features testing --release $*"
    else
        full_cmd="cargo test --features testing --release $*"
    fi
    if [ "$DRY_RUN" = "1" ]; then
        echo -e "  ${YELLOW}[dry-run]${NC} $full_cmd 2>&1 | tee $log_file"
        return 0
    fi
    echo -e "  ${BLUE}→${NC} $full_cmd"
    # shellcheck disable=SC2086
    if [ -n "$cmd_prefix" ]; then
        eval $cmd_prefix cargo test --features testing --release "$@" 2>&1 | tee "$log_file"
    else
        cargo test --features testing --release "$@" 2>&1 | tee "$log_file"
    fi
}

echo -e "${BLUE}${BOLD}=============================================================${NC}"
echo -e "${BLUE}${BOLD}     NAM-Plug Long-Duration Stress & Audit Suite             ${NC}"
echo -e "${BLUE}${BOLD}=============================================================${NC}"
echo -e "  Strict pre-release: ${BOLD}$STRICT_PRE_RELEASE${NC}  Nocapture (phases 1-3): ${BOLD}1${NC}  Dry-run: ${BOLD}$DRY_RUN${NC}  GUI: ${BOLD}$RUN_GUI${NC}"
echo -e "  Bench core: ${BOLD}$BENCH_CORE${NC} (of $NUM_CORES, HAS_TASKSET=$HAS_TASKSET)  Receipt: ${CYAN}$RECEIPT_FILE${NC}"
echo -e "  Artifact under test: ${BOLD}$ARTIFACT_PATH${NC}"
echo -e "  Artifact SHA-256: ${BOLD}$ARTIFACT_SHA256${NC}  Strict artifact: ${BOLD}${CLAP_STRICT_ARTIFACT:-0}${NC}"
echo -e "  xvfb-run available: ${BOLD}$HAVE_XVFB_RUN${NC}  Thermal cooldown: ${BOLD}${NAM_THERMAL_COOLDOWN_S:-0}s${NC}"
if [ "$ORIG_PARANOID" != "2" ] || [ "$STRICT_PRE_RELEASE" = "1" ]; then
    echo -e "  kernel.perf_event_paranoid: ${BOLD}$ORIG_PARANOID${NC}"
fi
if [ "$DRY_RUN" = "1" ]; then
    echo -e "  ${YELLOW}Dry-run mode — no tests will be executed.${NC}"
fi

if [ "$STRICT_PRE_RELEASE" = "1" ] && [ "$ORIG_PARANOID" -gt 1 ] 2>/dev/null; then
    if command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null; then
        echo -e "  Attempting to set kernel.perf_event_paranoid to 1 for stable RT audit..."
        if sudo sysctl -q -w kernel.perf_event_paranoid=1 2>/dev/null; then
            PARANOID_MODIFIED=true
            echo -e "  ${GREEN}✓${NC} kernel.perf_event_paranoid set to 1 (will restore on exit)"
        fi
    fi
fi

PHASE_STATUS=()
PHASE_DURATIONS=()
OVERALL_FAILED=0

# ── Phase 0: Pre-compilation warm-up (--no-run) ───────────────────────────────
# Compile all test binaries in a single upfront pass, identical flags to those
# used by Phases 1-4, so that every subsequent `cargo test` hits a warm cache
# and pays only linkage + runner overhead (< 1 s per invocation instead of
# several minutes on a cold cache). This pass is NOT counted in any phase timer.
PREBUILD_LOG="target/logs/long-prebuild.log"
if [ "$DRY_RUN" != "1" ]; then
    : > "$PREBUILD_LOG" 2>/dev/null || true
fi

# Release test artifacts never need the shipped fat-LTO profile: disabling LTO
# and restoring parallel codegen keeps the build's peak memory bounded and ensures
# consistent cache reuse across all phases (including Phase 4 tests-gui.sh).
export CARGO_PROFILE_RELEASE_LTO="${CARGO_PROFILE_RELEASE_LTO:-false}"
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS="${CARGO_PROFILE_RELEASE_CODEGEN_UNITS:-16}"

echo -e "\n${BLUE}${BOLD}[0/4] Pre-build — compiling all test artefacts (--no-run)...${NC}"
TARGET_PREBUILD_ARGS=(--lib --test heap_audit --test clap)
COMPILE_CMD=(cargo test --features testing --release --no-run --jobs "${CARGO_BUILD_JOBS:-4}" "${TARGET_PREBUILD_ARGS[@]}")
if [ "$DRY_RUN" = "1" ]; then
    echo -e "  ${YELLOW}[dry-run]${NC} ${COMPILE_CMD[*]} 2>&1 | tee $PREBUILD_LOG"
else
    PREBUILD_START=$(date +%s%N)
    echo -e "  ${BLUE}→${NC} ${COMPILE_CMD[*]}"
    NORM_PREFIX=()
    if command -v chrt >/dev/null 2>&1 && [ "$(chrt -p $$ 2>/dev/null | grep -o 'SCHED_[A-Z]*' || true)" = "SCHED_FIFO" ]; then
        NORM_PREFIX=(chrt -o 0)
    fi
    if "${NORM_PREFIX[@]}" "${COMPILE_CMD[@]}" 2>&1 | tee "$PREBUILD_LOG"; then
        PREBUILD_END=$(date +%s%N)
        PREBUILD_DUR_MS=$(( (PREBUILD_END - PREBUILD_START) / 1000000 ))
        prebuild_str=$(format_duration_ms "$PREBUILD_DUR_MS")
        ok "Pre-build completed in ${prebuild_str} — artefacts cached for Phases 1-4."
    else
        PREBUILD_END=$(date +%s%N)
        PREBUILD_DUR_MS=$(( (PREBUILD_END - PREBUILD_START) / 1000000 ))
        prebuild_str=$(format_duration_ms "$PREBUILD_DUR_MS")
        # A pre-build failure is fatal: subsequent phases would recompile and give
        # misleading timings, or fail for the same underlying reason.
        echo -e "  ${RED}${BOLD}✗ Pre-build failed after ${prebuild_str} — aborting audit.${NC}" >&2
        exit 1
    fi
fi

# ── Thermal cooldown (NAM_THERMAL_COOLDOWN_S) ────────────────────────────────
# After compilation the CPU may be at elevated frequency/temperature due to the
# parallel codegen workload. Allow it to return to base state before timed RT
# phases run under SCHED_FIFO / taskset affinity, ensuring measurement
# determinism. Default is 0 (disabled) — enable for pre-release ceremonies.
COOLDOWN_S="${NAM_THERMAL_COOLDOWN_S:-0}"
if [ "$COOLDOWN_S" -gt 0 ] 2>/dev/null && [ "$DRY_RUN" != "1" ]; then
    echo -e "  ${CYAN}[INFO] Resfriamento térmico: aguardando ${COOLDOWN_S}s para estabilização da CPU antes dos testes RT...${NC}"
    sleep "$COOLDOWN_S"
    ok "Cooldown concluído — CPU pronta para medição determinística."
elif [ "$DRY_RUN" = "1" ] && [ "${NAM_THERMAL_COOLDOWN_S:-0}" -gt 0 ] 2>/dev/null; then
    echo -e "  ${YELLOW}[dry-run]${NC} sleep ${COOLDOWN_S}  # NAM_THERMAL_COOLDOWN_S"
fi

# ── Phase 1: GC Stress ────────────────────────────────────────────────────────
phase "GC Stress — SPSC cascade, drain-on-destroy & property soak (Phase 1/3)"
PHASE1_START=$(date +%s%N)
PHASE1_STATUS="PASSED"
PHASE1_LOG="target/logs/long-phase1.log"
if [ "$DRY_RUN" != "1" ]; then
    : > "$PHASE1_LOG" 2>/dev/null || true
fi

if [ "$DRY_RUN" = "1" ]; then
    PHASE1_DUR_MS=0
    PHASE1_STATUS="SIMULATED"
    echo -e "  ${YELLOW}[dry-run] Phase 1 would execute:${NC}"
    echo -e "    cargo test --features testing --release --lib test_gc_stress_1000_swaps -- --ignored $NOCAPTURE_FLAG"
    echo -e "    cargo test --features testing --release --lib test_gc_drain_on_destroy_no_leak -- --ignored $NOCAPTURE_FLAG"
    echo -e "    cargo test --features testing --release --test heap_audit prop_gc_swap_idempotence -- --ignored $NOCAPTURE_FLAG"
    echo -e "    cargo test --features testing --release --lib test_corrupted_states_10000_heap_stable -- --ignored --test-threads=1 $NOCAPTURE_FLAG"
else
    set +e
    echo -e "  ${BLUE}→ Running test_gc_stress_1000_swaps...${NC}"
    if [ -n "$NOCAPTURE_FLAG" ]; then
        run_cargo_test "$PHASE1_LOG" --lib test_gc_stress_1000_swaps -- --ignored --nocapture
    else
        run_cargo_test "$PHASE1_LOG" --lib test_gc_stress_1000_swaps -- --ignored
    fi
    RC1=$?
    if [ $RC1 -ne 0 ]; then
        PHASE1_STATUS="FAILED"
        OVERALL_FAILED=1
        echo -e "  ${RED}✗ test_gc_stress_1000_swaps failed (rc=$RC1)${NC}"
    else
        if ! grep -q "test result: ok" "$PHASE1_LOG" 2>/dev/null; then
            echo -e "  ${RED}✗ test_gc_stress_1000_swaps: no passing test result found${NC}"
            PHASE1_STATUS="FAILED"
            OVERALL_FAILED=1
        else
            echo -e "  ${GREEN}✓ test_gc_stress_1000_swaps passed${NC}"
        fi
    fi

    echo -e "  ${BLUE}→ Running test_gc_drain_on_destroy_no_leak...${NC}"
    TMP_LOG="target/logs/long-phase1b.log"
    if [ -n "$NOCAPTURE_FLAG" ]; then
        run_cargo_test "$TMP_LOG" --lib test_gc_drain_on_destroy_no_leak -- --ignored --nocapture
    else
        run_cargo_test "$TMP_LOG" --lib test_gc_drain_on_destroy_no_leak -- --ignored
    fi
    RC2=$?
    cat "$TMP_LOG" >> "$PHASE1_LOG" 2>/dev/null || true
    rm -f "$TMP_LOG"
    if [ $RC2 -ne 0 ]; then
        PHASE1_STATUS="FAILED"
        OVERALL_FAILED=1
        echo -e "  ${RED}✗ test_gc_drain_on_destroy_no_leak failed (rc=$RC2)${NC}"
    else
        if grep -q "test_gc_drain_on_destroy_no_leak.*ok" "$PHASE1_LOG" 2>/dev/null || [ $RC2 -eq 0 ]; then
            echo -e "  ${GREEN}✓ test_gc_drain_on_destroy_no_leak passed${NC}"
        fi
    fi

    echo -e "  ${BLUE}→ Running prop_gc_swap_idempotence (property soak)...${NC}"
    TMP_LOG="target/logs/long-phase1c.log"
    if [ -n "$NOCAPTURE_FLAG" ]; then
        run_cargo_test "$TMP_LOG" --test heap_audit prop_gc_swap_idempotence -- --ignored --nocapture
    else
        run_cargo_test "$TMP_LOG" --test heap_audit prop_gc_swap_idempotence -- --ignored
    fi
    RC3=$?
    cat "$TMP_LOG" >> "$PHASE1_LOG" 2>/dev/null || true
    rm -f "$TMP_LOG"
    if [ $RC3 -ne 0 ]; then
        PHASE1_STATUS="FAILED"
        OVERALL_FAILED=1
        echo -e "  ${RED}✗ prop_gc_swap_idempotence failed (rc=$RC3)${NC}"
    else
        if grep -q "prop_gc_swap_idempotence.*ok" "$PHASE1_LOG" 2>/dev/null || [ $RC3 -eq 0 ]; then
            echo -e "  ${GREEN}✓ prop_gc_swap_idempotence passed${NC}"
        fi
    fi

    # Heap-stability soak: its allocation accounting is TLS-scoped, so it must
    # run alone on a single test thread (it is #[ignore]d in the default suite).
    echo -e "  ${BLUE}→ Running test_corrupted_states_10000_heap_stable (heap soak)...${NC}"
    TMP_LOG="target/logs/long-phase1d.log"
    if [ -n "$NOCAPTURE_FLAG" ]; then
        run_cargo_test "$TMP_LOG" --lib test_corrupted_states_10000_heap_stable -- --ignored --test-threads=1 --nocapture
    else
        run_cargo_test "$TMP_LOG" --lib test_corrupted_states_10000_heap_stable -- --ignored --test-threads=1
    fi
    RC4=$?
    cat "$TMP_LOG" >> "$PHASE1_LOG" 2>/dev/null || true
    rm -f "$TMP_LOG"
    if [ $RC4 -ne 0 ] || ! grep -q "test_corrupted_states_10000_heap_stable.*ok" "$PHASE1_LOG" 2>/dev/null; then
        PHASE1_STATUS="FAILED"
        OVERALL_FAILED=1
        echo -e "  ${RED}✗ test_corrupted_states_10000_heap_stable failed or did not run (rc=$RC4)${NC}"
    else
        echo -e "  ${GREEN}✓ test_corrupted_states_10000_heap_stable passed${NC}"
    fi

    set -e
    PHASE1_END=$(date +%s%N)
    PHASE1_DUR_MS=$(( (PHASE1_END - PHASE1_START) / 1000000 ))
    dur1_str=$(format_duration_ms "$PHASE1_DUR_MS")
    if [ "$PHASE1_STATUS" = "FAILED" ]; then
        echo -e "  ${RED}${BOLD}Phase 1 FAILED (${dur1_str})${NC}"
    else
        echo -e "  ${GREEN}${BOLD}Phase 1 PASSED (${dur1_str})${NC}"
    fi
fi

emit_receipt "phase1" "GC Stress — 1000 swaps, drain-on-destroy & property soak" "$PHASE1_STATUS" "$PHASE1_DUR_MS" "$PHASE1_LOG" false
PHASE_STATUS+=("$PHASE1_STATUS")
PHASE_DURATIONS+=("$PHASE1_DUR_MS")

# ── Phase 2: Teardown Drain ─────────────────────────────────────────────────
phase "Teardown — RT parking lot drain off-RT (Phase 2/3)"
PHASE2_START=$(date +%s%N)
PHASE2_STATUS="PASSED"
PHASE2_LOG="target/logs/long-phase2.log"
if [ "$DRY_RUN" != "1" ]; then
    : > "$PHASE2_LOG" 2>/dev/null || true
fi

if [ "$DRY_RUN" = "1" ]; then
    PHASE2_DUR_MS=0
    PHASE2_STATUS="SIMULATED"
    echo -e "  ${YELLOW}[dry-run] Phase 2 would execute:${NC}"
    echo -e "    cargo test --features testing --release --lib test_teardown_drains_rt_parking_lot_off_rt -- --ignored $NOCAPTURE_FLAG"
else
    set +e
    echo -e "  ${BLUE}→ Running test_teardown_drains_rt_parking_lot_off_rt...${NC}"
    if [ -n "$NOCAPTURE_FLAG" ]; then
        run_cargo_test "$PHASE2_LOG" --lib test_teardown_drains_rt_parking_lot_off_rt -- --ignored --nocapture
    else
        run_cargo_test "$PHASE2_LOG" --lib test_teardown_drains_rt_parking_lot_off_rt -- --ignored
    fi
    RC=$?
    set -e
    PHASE2_END=$(date +%s%N)
    PHASE2_DUR_MS=$(( (PHASE2_END - PHASE2_START) / 1000000 ))
    dur2_str=$(format_duration_ms "$PHASE2_DUR_MS")
    if [ $RC -ne 0 ]; then
        PHASE2_STATUS="FAILED"
        OVERALL_FAILED=1
        echo -e "  ${RED}${BOLD}Phase 2 FAILED (rc=$RC, ${dur2_str})${NC}"
    else
        if ! grep -q "test result: ok" "$PHASE2_LOG" 2>/dev/null; then
            echo -e "  ${YELLOW}⚠ Phase 2: no test result summary found — treating as failure (${dur2_str})${NC}"
            PHASE2_STATUS="FAILED"
            OVERALL_FAILED=1
        else
            echo -e "  ${GREEN}${BOLD}Phase 2 PASSED (${dur2_str})${NC}"
        fi
    fi
fi

emit_receipt "phase2" "Teardown — RT parking lot drain off-RT" "$PHASE2_STATUS" "$PHASE2_DUR_MS" "$PHASE2_LOG" false
PHASE_STATUS+=("$PHASE2_STATUS")
PHASE_DURATIONS+=("$PHASE2_DUR_MS")

# ── Phase 3: RT Priority & Micro-Latency Certification ──────────────────────
phase "RT Priority & Micro-Latency Certification — Pinned Core (Phase 3/3)"
PHASE3_START=$(date +%s%N)
PHASE3_STATUS="PASSED"
PHASE3_LOG="target/logs/long-phase3.log"
if [ "$DRY_RUN" != "1" ]; then
    : > "$PHASE3_LOG" 2>/dev/null || true
fi

if [ "$DRY_RUN" = "1" ]; then
    PHASE3_DUR_MS=0
    PHASE3_STATUS="SIMULATED"
    echo -e "  ${YELLOW}[dry-run] Phase 3 would execute:${NC}"
    if [ "$HAS_TASKSET" = "1" ]; then
        echo -e "    taskset -c $BENCH_CORE cargo test --features testing --release --test clap test_multi_instance_rt_priority -- --ignored --test-threads=1 $NOCAPTURE_FLAG"
        echo -e "    taskset -c $BENCH_CORE cargo test --features testing --release --lib test_structural_burst_p99_within_contract -- --ignored --test-threads=1 $NOCAPTURE_FLAG"
        echo -e "    taskset -c $BENCH_CORE cargo test --features testing --release --lib processor_drain_latency_test -- --ignored --test-threads=1 $NOCAPTURE_FLAG"
        echo -e "    taskset -c $BENCH_CORE cargo test --features testing --release --lib test_reset_wall_clock_time_within_contract -- --ignored --test-threads=1 $NOCAPTURE_FLAG"
    else
        echo -e "    cargo test --features testing --release --test clap test_multi_instance_rt_priority -- --ignored --test-threads=1 $NOCAPTURE_FLAG"
        echo -e "    cargo test --features testing --release --lib test_structural_burst_p99_within_contract -- --ignored --test-threads=1 $NOCAPTURE_FLAG"
        echo -e "    cargo test --features testing --release --lib processor_drain_latency_test -- --ignored --test-threads=1 $NOCAPTURE_FLAG"
        echo -e "    cargo test --features testing --release --lib test_reset_wall_clock_time_within_contract -- --ignored --test-threads=1 $NOCAPTURE_FLAG"
    fi
else
    set +e
    echo -e "  ${BLUE}→ Running test_multi_instance_rt_priority under affinity core $BENCH_CORE...${NC}"
    if [ -n "$NOCAPTURE_FLAG" ]; then
        run_cargo_test "$PHASE3_LOG" --affinity --test clap test_multi_instance_rt_priority -- --ignored --test-threads=1 --nocapture
    else
        run_cargo_test "$PHASE3_LOG" --affinity --test clap test_multi_instance_rt_priority -- --ignored --test-threads=1
    fi
    RC1=$?
    if [ $RC1 -ne 0 ]; then
        echo -e "  ${RED}✗ test_multi_instance_rt_priority failed (rc=$RC1)${NC}"
    else
        echo -e "  ${GREEN}✓ test_multi_instance_rt_priority passed${NC}"
    fi

    echo -e "  ${BLUE}→ Running test_structural_burst_p99_within_contract under affinity core $BENCH_CORE...${NC}"
    TMP_LOG="target/logs/long-phase3b.log"
    if [ -n "$NOCAPTURE_FLAG" ]; then
        run_cargo_test "$TMP_LOG" --affinity --lib test_structural_burst_p99_within_contract -- --ignored --test-threads=1 --nocapture
    else
        run_cargo_test "$TMP_LOG" --affinity --lib test_structural_burst_p99_within_contract -- --ignored --test-threads=1
    fi
    RC2=$?
    cat "$TMP_LOG" >> "$PHASE3_LOG" 2>/dev/null || true
    rm -f "$TMP_LOG"
    if [ $RC2 -ne 0 ]; then
        echo -e "  ${RED}✗ test_structural_burst_p99_within_contract failed (rc=$RC2)${NC}"
    else
        echo -e "  ${GREEN}✓ test_structural_burst_p99_within_contract passed${NC}"
    fi

    echo -e "  ${BLUE}→ Running processor_drain_latency_test under affinity core $BENCH_CORE...${NC}"
    TMP_LOG="target/logs/long-phase3c.log"
    if [ -n "$NOCAPTURE_FLAG" ]; then
        run_cargo_test "$TMP_LOG" --affinity --lib processor_drain_latency_test -- --ignored --test-threads=1 --nocapture
    else
        run_cargo_test "$TMP_LOG" --affinity --lib processor_drain_latency_test -- --ignored --test-threads=1
    fi
    RC3=$?
    cat "$TMP_LOG" >> "$PHASE3_LOG" 2>/dev/null || true
    rm -f "$TMP_LOG"
    if [ $RC3 -ne 0 ]; then
        echo -e "  ${RED}✗ processor_drain_latency_test failed (rc=$RC3)${NC}"
    else
        echo -e "  ${GREEN}✓ processor_drain_latency_test passed${NC}"
    fi

    echo -e "  ${BLUE}→ Running test_reset_wall_clock_time_within_contract under affinity core $BENCH_CORE...${NC}"
    TMP_LOG="target/logs/long-phase3d.log"
    if [ -n "$NOCAPTURE_FLAG" ]; then
        run_cargo_test "$TMP_LOG" --affinity --lib test_reset_wall_clock_time_within_contract -- --ignored --test-threads=1 --nocapture
    else
        run_cargo_test "$TMP_LOG" --affinity --lib test_reset_wall_clock_time_within_contract -- --ignored --test-threads=1
    fi
    RC4=$?
    cat "$TMP_LOG" >> "$PHASE3_LOG" 2>/dev/null || true
    rm -f "$TMP_LOG"
    if [ $RC4 -ne 0 ]; then
        echo -e "  ${RED}✗ test_reset_wall_clock_time_within_contract failed (rc=$RC4)${NC}"
    else
        echo -e "  ${GREEN}✓ test_reset_wall_clock_time_within_contract passed${NC}"
    fi

    RC=$(( RC1 | RC2 | RC3 | RC4 ))
    set -e
    PHASE3_END=$(date +%s%N)
    PHASE3_DUR_MS=$(( (PHASE3_END - PHASE3_START) / 1000000 ))
    dur3_str=$(format_duration_ms "$PHASE3_DUR_MS")
    if [ $RC -ne 0 ]; then
        PHASE3_STATUS="FAILED"
        OVERALL_FAILED=1
        echo -e "  ${RED}${BOLD}Phase 3 FAILED (rc=$RC, ${dur3_str})${NC}"
    else
        if ! grep -q "test result: ok" "$PHASE3_LOG" 2>/dev/null; then
            echo -e "  ${YELLOW}⚠ Phase 3: no test result summary — treating as failure (${dur3_str})${NC}"
            PHASE3_STATUS="FAILED"
            OVERALL_FAILED=1
        else
            echo -e "  ${GREEN}${BOLD}Phase 3 PASSED (${dur3_str})${NC}"
        fi
    fi
fi

emit_receipt "phase3" "RT Priority & Micro-Latency Certification" "$PHASE3_STATUS" "$PHASE3_DUR_MS" "$PHASE3_LOG" true
PHASE_STATUS+=("$PHASE3_STATUS")
PHASE_DURATIONS+=("$PHASE3_DUR_MS")

# ── Phase 4: GUI / Xvfb ─────────────────────────────────────────────────────
phase "GUI/X11 Headless — Xvfb lifecycle, XEmbed cycles, clipboard (Phase 4/4)"
PHASE4_START=$(date +%s%N)
PHASE4_STATUS="SKIPPED"
PHASE4_DUR_MS=0
PHASE4_LOG="target/logs/long-phase4.log"
if [ "$DRY_RUN" != "1" ]; then
    : > "$PHASE4_LOG" 2>/dev/null || true
fi

if [ "$RUN_GUI" = "0" ]; then
    warn "GUI phase skipped (xvfb-run not found and --gui not requested)."
    warn "Install 'xvfb' or pass --gui to enable this phase."
elif [ "$DRY_RUN" = "1" ]; then
    echo -e "  ${YELLOW}[dry-run] Phase 4 would execute:${NC}"
    echo -e "    ./utils/tests-gui.sh --dry-run"
    PHASE4_STATUS="SIMULATED"
else
    set +e
    echo -e "  ${BLUE}→ Delegating to utils/tests-gui.sh...${NC}"
    GUI_ARGS=""
    [ "$NOCAPTURE" = "1" ] && GUI_ARGS="--nocapture"
    bash "$SCRIPT_DIR/tests-gui.sh" $GUI_ARGS 2>&1 | tee "$PHASE4_LOG"
    PHASE4_RC=$?
    set -e
    PHASE4_END=$(date +%s%N)
    PHASE4_DUR_MS=$(( (PHASE4_END - PHASE4_START) / 1000000 ))
    dur4_str=$(format_duration_ms "$PHASE4_DUR_MS")
    if [ $PHASE4_RC -ne 0 ]; then
        PHASE4_STATUS="FAILED"
        OVERALL_FAILED=1
        echo -e "  ${RED}${BOLD}Phase 4 FAILED (rc=$PHASE4_RC, ${dur4_str})${NC}"
    else
        if grep -q "GAP\|not installed" "$PHASE4_LOG" 2>/dev/null; then
            PHASE4_STATUS="GAP"
            warn "Phase 4: xvfb-run absent — GUI coverage gap reported."
            if [ "$STRICT_PRE_RELEASE" = "1" ]; then
                OVERALL_FAILED=1
                echo -e "  ${RED}${BOLD}Phase 4 GAP treated as FAILED under --strict-pre-release.${NC}"
            fi
        else
            PHASE4_STATUS="PASSED"
            echo -e "  ${GREEN}${BOLD}Phase 4 PASSED (${dur4_str})${NC}"
        fi
    fi
fi

emit_receipt "phase4" "GUI/X11 Headless — Xvfb lifecycle, XEmbed cycles, clipboard" "$PHASE4_STATUS" "$PHASE4_DUR_MS" "$PHASE4_LOG" false
PHASE_STATUS+=("$PHASE4_STATUS")
PHASE_DURATIONS+=("$PHASE4_DUR_MS")

# ── Overall receipt (derived in Rust by plug_long_receipt) ──────────────────
TOTAL_DUR_MS=$(( PHASE1_DUR_MS + PHASE2_DUR_MS + PHASE3_DUR_MS + PHASE4_DUR_MS ))
OVERALL_STATUS="FAILED"
if ensure_plug_receipt_bin && SUMMARY_LINE="$("$PLUG_RECEIPT_BIN" summary --out "$RECEIPT_FILE" 2>/dev/null)"; then
    OVERALL_STATUS="${SUMMARY_LINE#OVERALL: }"
else
    LONG_RECEIPT_FAILED=1
fi

if [ "$LONG_RECEIPT_FAILED" -ne 0 ]; then
    echo -e "${RED}${BOLD}❌ Long-audit receipt emission failed — $RECEIPT_FILE is missing or incomplete.${NC}" >&2
    exit 1
fi

# Fail closed if the receipt and the resolved artifact disagree. The script
# emits the hash it computed at startup, so a mismatch means the artifact was
# replaced mid-audit; an absent declaration means a writer bug. Neither may be
# promoted to a PASSED audit.
if ! "$PLUG_RECEIPT_BIN" verify-artifact --out "$RECEIPT_FILE" --artifact "$ARTIFACT_PATH"; then
    echo -e "${RED}${BOLD}❌ Long-audit receipt does not certify $ARTIFACT_PATH (artifact provenance mismatch).${NC}" >&2
    exit 1
fi

echo -e "\n${BLUE}${BOLD}================ AUDIT SUMMARY ================${NC}"
for i in 0 1 2 3; do
    s="${PHASE_STATUS[$i]}"
    d="${PHASE_DURATIONS[$i]}"
    name=""
    case $i in
        0) name="Phase 1 — GC Stress" ;;
        1) name="Phase 2 — Teardown Drain" ;;
        2) name="Phase 3 — RT Priority & Micro-Latency" ;;
        3) name="Phase 4 — GUI/X11 Headless" ;;
    esac
    if [ "$s" = "PASSED" ]; then
        echo -e "  ${GREEN}✓ $name: $s (${d} ms)${NC}"
    elif [ "$s" = "SIMULATED" ]; then
        echo -e "  ${YELLOW}⚠ $name: $s (${d} ms) [SIMULATED]${NC}"
    elif [ "$s" = "SKIPPED" ] || [ "$s" = "GAP" ]; then
        echo -e "  ${YELLOW}⚠ $name: $s (${d} ms)${NC}"
    else
        echo -e "  ${RED}✗ $name: $s (${d} ms)${NC}"
    fi
done
echo -e "  Receipt: ${CYAN}$RECEIPT_FILE${NC}  Total: ${BOLD}${TOTAL_DUR_MS} ms${NC}  Overall: ${BOLD}$OVERALL_STATUS${NC}"
echo -e "${BLUE}${BOLD}================================================${NC}\n"

if [ "$DRY_RUN" = "1" ]; then
    echo -e "${YELLOW}${BOLD}⚠ Dry-run completed — simulated receipts recorded in $RECEIPT_FILE (no tests executed).${NC}"
    exit 0
fi

if [ "$OVERALL_STATUS" = "FAILED" ]; then
    echo -e "${RED}${BOLD}❌ Long audit FAILED — see $RECEIPT_FILE and target/logs/long-phase*.log${NC}"
    exit 1
fi

if [ "$OVERALL_STATUS" = "COMPLETED_WITH_GAPS" ]; then
    echo -e "${YELLOW}${BOLD}⚠ Long audit completed with declared gaps (skipped/inconclusive phases).${NC}"
    if [ "$STRICT_PRE_RELEASE" = "1" ]; then
        echo -e "${RED}${BOLD}❌ --strict-pre-release rejects declared gaps.${NC}"
        exit 1
    fi
    exit 0
fi

if [ "$OVERALL_STATUS" != "PASSED" ]; then
    echo -e "${RED}${BOLD}❌ Long audit has an unexpected verdict '$OVERALL_STATUS' — failing closed.${NC}"
    exit 1
fi

echo -e "${GREEN}${BOLD}✓ Long audit PASSED — all heavy stress tests passed.${NC}"
echo -e "  Artifacts: ${CYAN}target/logs/long-phase*.log${NC}  Receipt: ${CYAN}$RECEIPT_FILE${NC}"
exit 0
