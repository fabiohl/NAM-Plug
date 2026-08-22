#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Quick QA Suite for NAM-Plug — agile first line of defense.
#
# Division of responsibility among QA scripts:
#   * utils/lints.sh       — Static quality gate (fmt, SPDX, cargo check, clippy).
#   * utils/tests-quick.sh — THIS script. Agile green test suite (cargo test).
#
# NAM-Plug is a CLAP plugin crate with unit tests in src/ and integration tests in tests/.
#
# Phases:
#   1. Structural (debug)   — unit + integration tests with debug assertions ON.
#   2. Release verification — CLAP .so artifact build + CLAP × NAMCore float
#      parity oracle (S1-T06; release-only scope per S6-T04 / RES-04) when the
#      C++ render binary, release .so and model fixture exist; otherwise
#      reported as an explicit GAP.
#   3. RT-Safety heap-audit — zero-alloc process() gate (--features heap-audit).
#
# Each phase persists its output to target/logs/quick-phaseN.log (phase 3:
# quick-heap-audit.log) and the run closes with a typed receipt at
# target/logs/quick-receipt.txt.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT_PATH="$SCRIPT_DIR/$(basename "${BASH_SOURCE[0]}")"

PHASE_TOTAL=3
source "$SCRIPT_DIR/_lib.sh"

# Re-execute with low CPU and I/O priority (nice and ionice) to prevent overloading the system.
if [ "${NAM_LOW_PRIORITY:-0}" != "1" ] && [ "${NAM_NO_LOW_PRIORITY:-0}" != "1" ]; then
    export NAM_LOW_PRIORITY=1
    CMD_PREFIX=""
    if command -v nice > /dev/null 2>&1; then
        CMD_PREFIX="nice -n 19"
    fi
    if command -v ionice > /dev/null 2>&1; then
        CMD_PREFIX="$CMD_PREFIX ionice -c 3"
    fi
    if [ -n "$CMD_PREFIX" ]; then
        warn "Restarting script with low priority (CPU/IO) to prevent system overload..."
        exec $CMD_PREFIX "$SCRIPT_PATH" "$@"
    fi
fi

trap 'status=$?
if [ "$status" -eq 124 ]; then
    echo -e "\n${RED}${BOLD}❌ TIMEOUT: command \"$BASH_COMMAND\" timed out at line $LINENO (phase ${PHASE_NUM:-?}/${PHASE_TOTAL:-?}). Aborting test suite.${NC}"
else
    echo -e "\n${RED}${BOLD}❌ Unexpected error: command \"$BASH_COMMAND\" failed at line $LINENO with status $status (phase ${PHASE_NUM:-?}/${PHASE_TOTAL:-?}). Aborting test suite.${NC}"
fi
exit 1' ERR

mkdir -p target/logs
rm -f target/logs/quick-phase1.log \
      target/logs/quick-phase2.log \
      target/logs/quick-heap-audit.log \
      target/logs/quick-receipt.txt

echo -e "${BLUE}${BOLD}========================================${NC}"
echo -e "${BLUE}${BOLD}        NAM-Plug Quick QA Suite         ${NC}"
echo -e "${BLUE}${BOLD}========================================${NC}"

emit "SUITE: tests-quick"
emit "STRICT: ${NAM_QUICK_STRICT:-0}"

# Helper: Ensure the CLAP plugin shared library artifact for the requested
# profile is present before running integration tests that dlopen it.
#
# Accepts profile "debug" or "release". Checks for the exact profile artifact
# first; does NOT silently fall back to the other profile, since that could
# mask build-time failures specific to debug assertions or release codegen.
ensure_clap_artifact() {
    local profile="${1:-debug}"
    local flag=""
    if [ "$profile" = "release" ]; then
        flag="--release"
    fi

    # If the caller explicitly pointed to a pre-built artifact, honour it only
    # when the file actually exists — never silently accept a stale path.
    if [ -n "${CLAP_PLUGIN_PATH:-}" ]; then
        if [ -f "$CLAP_PLUGIN_PATH" ]; then
            return 0
        else
            warn "CLAP_PLUGIN_PATH set but file not found: $CLAP_PLUGIN_PATH. Rebuilding..."
        fi
    fi

    local target_dir="${CARGO_TARGET_DIR:-target}"
    local artifact_path="$target_dir/$profile/libnam_plug.so"

    if [ ! -f "$artifact_path" ]; then
        warn "CLAP plugin ($profile) artifact not found. Pre-building..."
        # shellcheck disable=SC2086
        cargo build $flag
    fi

    # Fail-closed (S1-T07): a successful cargo build must have produced the
    # shared library. Never proceed to dlopen-based tests with a missing or
    # partial artifact — that would mask build-time failures.
    if [ ! -f "$artifact_path" ]; then
        die "FATAL: CLAP plugin artifact was not created at expected path: $artifact_path"
    fi

    local sha
    sha=$(sha256sum "$artifact_path" | cut -d' ' -f1)
    echo -e "  ${GREEN}✓ CLAP artifact ($profile):${NC} $artifact_path (sha256: ${sha:0:16}...)"
}

# ── Phase 1: Structural unit & integration tests (debug) ─────────────────────
phase "Structural: unit & integration tests (debug)..."
ensure_clap_artifact debug
timeout 300 cargo test --features testing --lib \
    --test clap \
    --test clap_e0_containment_test \
    --test clap_e2_proptest \
    --test processor_bypass_test \
    2>&1 | tee target/logs/quick-phase1.log
assert_ran_tests target/logs/quick-phase1.log 1
emit "PHASE1: PASS log=target/logs/quick-phase1.log"

# ── Phase 2: Release verification (release, S6-T04 / RES-04) ─────────────────
# Gaps collected during the run (e.g. missing oracle prerequisites); surfaced
# as WARN GAP lines and as OVERALL: PASSED_WITH_GAPS in the final receipt.
declare -a GAPS=()

# Phase 2 focuses on what actually changes with `--release`: compiling the CLAP
# .so artifact and running the CLAP × NAMCore float parity oracle against it
# (test_clap_parity_multi_rate, tests/clap/clap_parity_multi_sr.rs, #[ignore])
# with ESR < 1e-8 / SNR > 80 dB gates. The logical unit/integration re-run of
# the Phase 1 targets (--lib, --test clap, --test clap_e0_containment_test,
# --test clap_e2_proptest, --test processor_bypass_test) is dropped: debug
# assertions ON in Phase 1 already validate that logic, and the release codegen
# of the .so is exactly what the oracle measures (RES-04 / SIB-03).
#
# The oracle runs only when the C++ render binary, the release artifact and the
# model fixture are all present; otherwise the gate is reported as an explicit
# GAP instead of silently passing.
phase "Release verification: CLAP .so artifact + float parity oracle (release)..."
ensure_clap_artifact release
"$SCRIPT_DIR/verify_no_avx512_release.sh" "${CARGO_TARGET_DIR:-target}/release/libnam_plug.so"

# Mirrors the Rust-side discovery order: NAM_CORE_RENDER_BIN first, then
# build/namcore_render in this repo, then the sibling NeuralAmpModeler-rs
# build dir (the crates.io dependency copy ships no build artifacts).
find_namcore_render() {
    if [ -n "${NAM_CORE_RENDER_BIN:-}" ]; then
        if [ -f "$NAM_CORE_RENDER_BIN" ]; then
            echo "$NAM_CORE_RENDER_BIN"
            return 0
        fi
        warn "NAM_CORE_RENDER_BIN set but path not found: $NAM_CORE_RENDER_BIN"
    fi
    local base hit
    for base in "build/namcore_render" "../NeuralAmpModeler-rs/build/namcore_render"; do
        hit=$(find "$base" -type f -name render -print -quit 2>/dev/null || true)
        if [ -n "$hit" ]; then
            echo "$hit"
            return 0
        fi
    done
    return 1
}

model_fixture=""
if [ -n "${NAM_FIXTURES_DIR:-}" ] && [ -f "$NAM_FIXTURES_DIR/wavenet_a1_standard.nam" ]; then
    model_fixture="$NAM_FIXTURES_DIR/wavenet_a1_standard.nam"
elif [ -f "tests/fixtures/models/wavenet_a1_standard.nam" ]; then
    model_fixture="tests/fixtures/models/wavenet_a1_standard.nam"
fi

if render_bin=$(find_namcore_render); then
    artifact_path="${CARGO_TARGET_DIR:-target}/release/libnam_plug.so"
    if [ -f "$artifact_path" ] && [ -n "$model_fixture" ]; then
        echo -e "  ${BLUE}→ Executing CLAP vs NAMCore float parity oracle...${NC}"
        warn "oracle=$render_bin artifact=$artifact_path fixture=$model_fixture"
        # NAM_REQUIRE_CPP_ORACLE=1 turns a discovery mismatch inside the test
        # into a loud panic instead of a masked SKIP-pass.
        NAM_REQUIRE_CPP_ORACLE=1 timeout 600 cargo test --features testing --release --test clap \
            test_clap_parity_multi_rate -- --ignored --nocapture \
            2>&1 | tee -a target/logs/quick-phase2.log
        if grep -q "test_clap_parity_multi_rate .* ok" target/logs/quick-phase2.log \
           && grep -q "ESR  =" target/logs/quick-phase2.log; then
            emit "PARITY: PASS"
            ok "CLAP vs NAMCore float parity: PASS"
            assert_ran_tests target/logs/quick-phase2.log 1
            emit "PHASE2: PASS log=target/logs/quick-phase2.log"
        else
            die "PARITY: FAIL test_clap_parity_multi_rate did not complete successfully"
        fi
    else
        GAPS+=("clap_parity_multi_rate:missing_render_or_fixtures")
        echo -e "${YELLOW}${BOLD}WARN GAP: clap_parity_multi_rate:missing_render_or_fixtures${NC}"
        warn "Actionable: ensure release artifact at $artifact_path ('cargo build --release') and model fixture tests/fixtures/models/wavenet_a1_standard.nam (or set NAM_FIXTURES_DIR to a fixture directory in isolated CI/CD). Oracle found at $render_bin."
        emit "PHASE2: GAP reason=missing_render_or_fixtures"
    fi
else
    GAPS+=("clap_parity_multi_rate:missing_render_or_fixtures")
    echo -e "${YELLOW}${BOLD}WARN GAP: clap_parity_multi_rate:missing_render_or_fixtures${NC}"
    warn "Actionable: set NAM_CORE_RENDER_BIN (path to the NAMCore C++ render binary) or build it locally under build/namcore_render (see golden_gen_build.sh) to enable the CLAP parity oracle. Isolated clones must use NAM_CORE_RENDER_BIN."
    emit "PHASE2: GAP reason=missing_render_or_fixtures"
fi

# S4-T1 (R-08): dlopen the production artifact and assert a cab-sim IR
# measurably changes the audio output (fail-closed evidence that the cabsim
# path is compiled into the default cdylib, not gated behind #[cfg(test)]).
release_artifact="${CARGO_TARGET_DIR:-target}/release/libnam_plug.so"
if [ -f "$release_artifact" ]; then
    echo -e "  ${BLUE}→ Executing cab-sim IR artifact test (dlopen)...${NC}"
    timeout 300 cargo test --features testing --release --test clap \
        test_cabsim_ir_changes_audio_release_artifact -- --ignored --nocapture \
        2>&1 | tee -a target/logs/quick-phase2.log
    if grep -q "test_cabsim_ir_changes_audio_release_artifact .* ok" target/logs/quick-phase2.log; then
        emit "CABSIM_IR: PASS"
        ok "Cab-sim IR artifact test: PASS"
    else
        die "CABSIM_IR: FAIL test_cabsim_ir_changes_audio_release_artifact did not complete successfully"
    fi
else
    GAPS+=("cabsim_ir:missing_release_artifact")
    echo -e "${YELLOW}${BOLD}WARN GAP: cabsim_ir:missing_release_artifact${NC}"
    warn "Actionable: build the release artifact ('cargo build --release') to enable the cab-sim IR artifact test."
    emit "PHASE2: GAP reason=missing_release_artifact"
fi

# ── Phase 3: RT-Safety & Heap Allocation Audit (debug, heap-audit) ───────────
phase "RT-Safety & Heap Allocation Audit..."
timeout 120 cargo test --features testing,heap-audit --lib \
    processor_heap_audit_test \
    2>&1 | tee target/logs/quick-heap-audit.log
assert_ran_tests target/logs/quick-heap-audit.log 1
emit "PHASE3: PASS log=target/logs/quick-heap-audit.log"
emit "HEAP_AUDIT=RAN"

# ── Receipt & summary ────────────────────────────────────────────────────────
if [ ${#GAPS[@]} -gt 0 ]; then
    for g in "${GAPS[@]}"; do
        emit "GAP: $g"
        echo -e "${YELLOW}${BOLD}WARN GAP: $g${NC}"
    done
    echo -e "\n${YELLOW}${BOLD}================================================================================${NC}"
    echo -e "  ${BOLD}Artifacts saved:${NC}"
    echo -e "    - Receipt:     ${CYAN}target/logs/quick-receipt.txt${NC}"
    echo -e "    - Phase 1 log: ${CYAN}target/logs/quick-phase1.log${NC}"
    echo -e "    - Phase 2 log: ${CYAN}target/logs/quick-phase2.log${NC}"
    echo -e "    - Heap log:    ${CYAN}target/logs/quick-heap-audit.log${NC}"
    echo -e "${YELLOW}${BOLD}================================================================================${NC}\n"
    if [ "${NAM_QUICK_STRICT:-0}" = "1" ]; then
        echo -e "${RED}${BOLD}OVERALL: FAIL reason=strict_gaps${NC}"
        emit "OVERALL: FAIL reason=strict_gaps"
        exit 1
    fi
    exit 0
fi

echo -e "\n${GREEN}${BOLD}================================================================================${NC}"
echo -e "${GREEN}${BOLD}    All quick tests passed! (CLAP)      ${NC}"
echo -e "  ${BOLD}Artifacts saved:${NC}"
echo -e "    - Receipt:     ${CYAN}target/logs/quick-receipt.txt${NC}"
echo -e "    - Phase 1 log: ${CYAN}target/logs/quick-phase1.log${NC}"
echo -e "    - Phase 2 log: ${CYAN}target/logs/quick-phase2.log${NC}"
echo -e "    - Heap log:    ${CYAN}target/logs/quick-heap-audit.log${NC}"
echo -e "${GREEN}${BOLD}================================================================================${NC}\n"
emit "OVERALL: PASSED"
