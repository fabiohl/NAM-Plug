#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Long QA Suite for NAM-Plug — nightly/pre-release certification.
#
# Executes the heavy `#[ignore]` stress tests that `tests-quick.sh` deliberately
# leaves out (GC/SPSC cascade, teardown drain, multi-instance RT priority).
# Each phase runs in isolation, persists its log to target/logs/long-phaseN.log,
# and appends a structured JSONL line to target/logs/long-audit-receipt.jsonl.
#
# Phases:
#   1. GC Stress — 1000 model swaps + drain-on-destroy leak check
#   2. Teardown  — 25 swaps without housekeeping, off-RT drain verification
#   3. Multi-instance RT priority — 10 instances, SCHED_FIFO detection
#
# Usage:
#   ./utils/tests-long.sh [--strict-pre-release] [--nocapture] [--dry-run]
#   chrt -f 80 ./utils/tests-long.sh --strict-pre-release  # official pre-release ceremony

set -euo pipefail

STRICT_PRE_RELEASE=0
NOCAPTURE=0
DRY_RUN=0

for arg in "$@"; do
    case "$arg" in
        --strict-pre-release)
            STRICT_PRE_RELEASE=1
            ;;
        --nocapture)
            NOCAPTURE=1
            ;;
        --dry-run)
            DRY_RUN=1
            ;;
        --help|-h)
            echo "Usage: $(basename "$0") [--strict-pre-release] [--nocapture] [--dry-run]"
            echo ""
            echo "Long audit suite for NAM-Plug — runs heavy #[ignore] stress tests."
            echo ""
            echo "Options:"
            echo "  --strict-pre-release  Fail closed on any gap or inconclusive phase"
            echo "  --nocapture           Pass --nocapture to cargo test for verbose output"
            echo "  --dry-run             Print planned commands without executing"
            echo "  -h, --help            Show this help and exit"
            echo ""
            echo "Receipt: target/logs/long-audit-receipt.jsonl"
            exit 0
            ;;
        *)
            echo "Unknown option: $arg" >&2
            echo "Usage: $(basename "$0") [--strict-pre-release] [--nocapture] [--dry-run]" >&2
            exit 1
            ;;
    esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LIB_DIR="$SCRIPT_DIR/lib"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

if [ -f "$LIB_DIR/_lib.sh" ]; then
    # shellcheck source=utils/lib/_lib.sh
    PHASE_TOTAL=3
    export PHASE_TOTAL
    source "$LIB_DIR/_lib.sh"
else
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[1;33m'
    BLUE='\033[0;34m'
    CYAN='\033[0;36m'
    BOLD='\033[1m'
    NC='\033[0m'
    PHASE_NUM=0
    PHASE_TOTAL=3
    phase() {
        PHASE_NUM=$((PHASE_NUM + 1))
        echo -e "\n${BLUE}${BOLD}[${PHASE_NUM}/${PHASE_TOTAL}]${NC} $*"
    }
    die() {
        echo -e "${RED}${BOLD}[FATAL]${NC} $*" >&2
        exit 1
    }
    ok() {
        echo -e "  ${GREEN}OK${NC} $*"
    }
    warn() {
        echo -e "  ${YELLOW}ⓘ${NC} $*"
    }
    PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
    cd "$PROJECT_DIR" || exit 1
fi

if [ "${NAM_LIB_NO_CD:-0}" != "1" ]; then
    cd "$PROJECT_DIR" || exit 1
fi

ORIG_PARANOID="$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo "2")"
PARANOID_MODIFIED=false
RECEIPT_FILE="target/logs/long-audit-receipt.jsonl"

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

NUM_CORES="$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 1)"
DEFAULT_CORE=$(( (${NUM_CORES:-1} / 2) ))
BENCH_CORE="${NAM_BENCH_CORE:-$DEFAULT_CORE}"
HAS_TASKSET=0
if command -v taskset >/dev/null 2>&1; then
    HAS_TASKSET=1
fi

NOCAPTURE_FLAG=""
if [ "$NOCAPTURE" = "1" ]; then
    NOCAPTURE_FLAG="--nocapture"
fi

mkdir -p target/logs
rm -f target/logs/long-phase1.log \
      target/logs/long-phase2.log \
      target/logs/long-phase3.log \
      "$RECEIPT_FILE"

: > "$RECEIPT_FILE"

emit_receipt() {
    local phase_id="$1"
    local name="$2"
    local status="$3"
    local duration_ms="$4"
    local log_file="${5:-}"
    local timestamp
    timestamp="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
    python3 -c "
import json
rec = {
    'phase_id': '''$phase_id''',
    'name': '''$name''',
    'status': '''$status''',
    'duration_ms': $duration_ms,
    'timestamp_utc': '''$timestamp''',
    'strict_pre_release': bool($STRICT_PRE_RELEASE),
    'bench_core': '''$BENCH_CORE''',
    'log': '''$log_file''' or None
}
with open('''$RECEIPT_FILE''', 'a') as f:
    json.dump(rec, f)
    f.write('\n')
"
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
    local cmd_prefix
    cmd_prefix="$(maybe_taskset)"
    local full_cmd
    if [ -n "$cmd_prefix" ]; then
        full_cmd="$cmd_prefix cargo test --features testing $*"
    else
        full_cmd="cargo test --features testing $*"
    fi
    if [ "$DRY_RUN" = "1" ]; then
        echo -e "  ${YELLOW}[dry-run]${NC} $full_cmd 2>&1 | tee $log_file"
        return 0
    fi
    echo -e "  ${BLUE}→${NC} $full_cmd"
    # shellcheck disable=SC2086
    if [ -n "$cmd_prefix" ]; then
        eval $cmd_prefix cargo test --features testing "$@" 2>&1 | tee "$log_file"
    else
        cargo test --features testing "$@" 2>&1 | tee "$log_file"
    fi
}

echo -e "${BLUE}${BOLD}=============================================================${NC}"
echo -e "${BLUE}${BOLD}     NAM-Plug Long-Duration Stress & Audit Suite             ${NC}"
echo -e "${BLUE}${BOLD}=============================================================${NC}"
echo -e "  Strict pre-release: ${BOLD}$STRICT_PRE_RELEASE${NC}  Nocapture: ${BOLD}$NOCAPTURE${NC}  Dry-run: ${BOLD}$DRY_RUN${NC}"
echo -e "  Bench core: ${BOLD}$BENCH_CORE${NC} (of $NUM_CORES, HAS_TASKSET=$HAS_TASKSET)  Receipt: ${CYAN}$RECEIPT_FILE${NC}"
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

# ── Phase 1: GC Stress ────────────────────────────────────────────────────────
phase "GC Stress — SPSC cascade & drain-on-destroy (Phase 1/3)"
PHASE1_START=$(date +%s%N)
PHASE1_STATUS="PASSED"
PHASE1_LOG="target/logs/long-phase1.log"
: > "$PHASE1_LOG" 2>/dev/null || true

if [ "$DRY_RUN" = "1" ]; then
    echo -e "  ${YELLOW}[dry-run] Phase 1 would execute:${NC}"
    echo -e "    cargo test --features testing --lib test_gc_stress_1000_swaps -- --ignored $NOCAPTURE_FLAG"
    echo -e "    cargo test --features testing --lib test_gc_drain_on_destroy_no_leak -- --ignored $NOCAPTURE_FLAG"
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
        if ! grep -q "test result: ok" "$PHASE1_LOG" 2>/dev/null; then
            :
        fi
        if grep -q "test_gc_drain_on_destroy_no_leak.*ok" "$PHASE1_LOG" 2>/dev/null || [ $RC2 -eq 0 ]; then
            echo -e "  ${GREEN}✓ test_gc_drain_on_destroy_no_leak passed${NC}"
        fi
    fi
    set -e
    if [ "$PHASE1_STATUS" = "FAILED" ]; then
        echo -e "  ${RED}${BOLD}Phase 1 FAILED${NC}"
    else
        echo -e "  ${GREEN}${BOLD}Phase 1 PASSED${NC}"
    fi
fi

PHASE1_END=$(date +%s%N)
PHASE1_DUR_MS=$(( (PHASE1_END - PHASE1_START) / 1000000 ))
if [ "$DRY_RUN" = "1" ]; then
    PHASE1_DUR_MS=0
    PHASE1_STATUS="PASSED"
fi
emit_receipt "phase1" "GC Stress — 1000 swaps + drain-on-destroy" "$PHASE1_STATUS" "$PHASE1_DUR_MS" "$PHASE1_LOG"
PHASE_STATUS+=("$PHASE1_STATUS")
PHASE_DURATIONS+=("$PHASE1_DUR_MS")

# ── Phase 2: Teardown Drain ─────────────────────────────────────────────────
phase "Teardown — RT parking lot drain off-RT (Phase 2/3)"
PHASE2_START=$(date +%s%N)
PHASE2_STATUS="PASSED"
PHASE2_LOG="target/logs/long-phase2.log"
: > "$PHASE2_LOG" 2>/dev/null || true

if [ "$DRY_RUN" = "1" ]; then
    echo -e "  ${YELLOW}[dry-run] Phase 2 would execute:${NC}"
    echo -e "    cargo test --features testing --lib test_teardown_drains_rt_parking_lot_off_rt -- --ignored $NOCAPTURE_FLAG"
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
    if [ $RC -ne 0 ]; then
        PHASE2_STATUS="FAILED"
        OVERALL_FAILED=1
        echo -e "  ${RED}${BOLD}Phase 2 FAILED (rc=$RC)${NC}"
    else
        if ! grep -q "test result: ok" "$PHASE2_LOG" 2>/dev/null; then
            echo -e "  ${YELLOW}⚠ Phase 2: no test result summary found — treating as failure${NC}"
            PHASE2_STATUS="FAILED"
            OVERALL_FAILED=1
        else
            echo -e "  ${GREEN}${BOLD}Phase 2 PASSED${NC}"
        fi
    fi
fi

PHASE2_END=$(date +%s%N)
PHASE2_DUR_MS=$(( (PHASE2_END - PHASE2_START) / 1000000 ))
if [ "$DRY_RUN" = "1" ]; then
    PHASE2_DUR_MS=0
fi
emit_receipt "phase2" "Teardown — RT parking lot drain off-RT" "$PHASE2_STATUS" "$PHASE2_DUR_MS" "$PHASE2_LOG"
PHASE_STATUS+=("$PHASE2_STATUS")
PHASE_DURATIONS+=("$PHASE2_DUR_MS")

# ── Phase 3: Multi-Instance RT Priority ─────────────────────────────────────
phase "Multi-Instance — RT priority under CPU affinity (Phase 3/3)"
PHASE3_START=$(date +%s%N)
PHASE3_STATUS="PASSED"
PHASE3_LOG="target/logs/long-phase3.log"
: > "$PHASE3_LOG" 2>/dev/null || true

if [ "$DRY_RUN" = "1" ]; then
    echo -e "  ${YELLOW}[dry-run] Phase 3 would execute:${NC}"
    if [ "$HAS_TASKSET" = "1" ]; then
        echo -e "    taskset -c $BENCH_CORE cargo test --features testing --test clap test_multi_instance_rt_priority -- --ignored $NOCAPTURE_FLAG"
    else
        echo -e "    cargo test --features testing --test clap test_multi_instance_rt_priority -- --ignored $NOCAPTURE_FLAG"
    fi
else
    set +e
    echo -e "  ${BLUE}→ Running test_multi_instance_rt_priority under affinity core $BENCH_CORE...${NC}"
    if [ -n "$NOCAPTURE_FLAG" ]; then
        run_cargo_test "$PHASE3_LOG" --test clap test_multi_instance_rt_priority -- --ignored --nocapture
    else
        run_cargo_test "$PHASE3_LOG" --test clap test_multi_instance_rt_priority -- --ignored
    fi
    RC=$?
    set -e
    if [ $RC -ne 0 ]; then
        PHASE3_STATUS="FAILED"
        OVERALL_FAILED=1
        echo -e "  ${RED}${BOLD}Phase 3 FAILED (rc=$RC)${NC}"
    else
        if ! grep -q "test result: ok" "$PHASE3_LOG" 2>/dev/null; then
            echo -e "  ${YELLOW}⚠ Phase 3: no test result summary — treating as failure${NC}"
            PHASE3_STATUS="FAILED"
            OVERALL_FAILED=1
        else
            echo -e "  ${GREEN}${BOLD}Phase 3 PASSED${NC}"
        fi
    fi
fi

PHASE3_END=$(date +%s%N)
PHASE3_DUR_MS=$(( (PHASE3_END - PHASE3_START) / 1000000 ))
if [ "$DRY_RUN" = "1" ]; then
    PHASE3_DUR_MS=0
fi
emit_receipt "phase3" "Multi-Instance — RT priority under CPU affinity" "$PHASE3_STATUS" "$PHASE3_DUR_MS" "$PHASE3_LOG"
PHASE_STATUS+=("$PHASE3_STATUS")
PHASE_DURATIONS+=("$PHASE3_DUR_MS")

# ── Overall receipt ─────────────────────────────────────────────────────────
OVERALL_STATUS="PASSED"
if [ "$OVERALL_FAILED" -ne 0 ]; then
    OVERALL_STATUS="FAILED"
fi
if [ "$DRY_RUN" = "1" ]; then
    OVERALL_STATUS="PASSED"
fi
TOTAL_DUR_MS=$(( PHASE1_DUR_MS + PHASE2_DUR_MS + PHASE3_DUR_MS ))
TIMESTAMP="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
python3 -c "
import json
rec = {
    'phase_id': 'overall',
    'name': 'Overall long audit',
    'status': '''$OVERALL_STATUS''',
    'duration_ms': $TOTAL_DUR_MS,
    'timestamp_utc': '''$TIMESTAMP''',
    'strict_pre_release': bool($STRICT_PRE_RELEASE),
    'phases': [
        {'id': 'phase1', 'status': '''${PHASE_STATUS[0]}''', 'duration_ms': ${PHASE_DURATIONS[0]}},
        {'id': 'phase2', 'status': '''${PHASE_STATUS[1]}''', 'duration_ms': ${PHASE_DURATIONS[1]}},
        {'id': 'phase3', 'status': '''${PHASE_STATUS[2]}''', 'duration_ms': ${PHASE_DURATIONS[2]}}
    ]
}
with open('''$RECEIPT_FILE''', 'a') as f:
    json.dump(rec, f)
    f.write('\n')
"

echo -e "\n${BLUE}${BOLD}================ AUDIT SUMMARY ================${NC}"
for i in 0 1 2; do
    s="${PHASE_STATUS[$i]}"
    d="${PHASE_DURATIONS[$i]}"
    name=""
    case $i in
        0) name="Phase 1 — GC Stress" ;;
        1) name="Phase 2 — Teardown Drain" ;;
        2) name="Phase 3 — Multi-Instance RT" ;;
    esac
    if [ "$s" = "PASSED" ]; then
        echo -e "  ${GREEN}✓ $name: $s (${d} ms)${NC}"
    else
        echo -e "  ${RED}✗ $name: $s (${d} ms)${NC}"
    fi
done
echo -e "  Receipt: ${CYAN}$RECEIPT_FILE${NC}  Total: ${BOLD}${TOTAL_DUR_MS} ms${NC}  Overall: ${BOLD}$OVERALL_STATUS${NC}"
echo -e "${BLUE}${BOLD}================================================${NC}\n"

if [ "$DRY_RUN" = "1" ]; then
    echo -e "${GREEN}${BOLD}✓ Dry-run completed — no tests executed.${NC}"
    exit 0
fi

if [ "$OVERALL_STATUS" = "FAILED" ]; then
    echo -e "${RED}${BOLD}❌ Long audit FAILED — see $RECEIPT_FILE and target/logs/long-phase*.log${NC}"
    exit 1
fi

echo -e "${GREEN}${BOLD}✓ Long audit PASSED — all heavy stress tests passed.${NC}"
echo -e "  Artifacts: ${CYAN}target/logs/long-phase*.log${NC}  Receipt: ${CYAN}$RECEIPT_FILE${NC}"
exit 0
