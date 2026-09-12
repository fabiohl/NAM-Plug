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
#      parity oracle (release-only scope) when the
#      C++ render binary, release .so and model fixture exist; otherwise
#      reported as an explicit GAP (or FAIL in NAM_QUICK_STRICT=1).
#   3. RT-Safety heap-audit — zero-alloc process() gate (--features heap-audit).
#
# Each phase persists its output to target/logs/quick-phaseN.log (phase 3:
# quick-heap-audit.log) and the run closes with a typed receipt at
# target/logs/quick-receipt.txt.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT_PATH="$SCRIPT_DIR/$(basename "${BASH_SOURCE[0]}")"

PHASE_TOTAL=3
source "$SCRIPT_DIR/lib/_lib.sh"

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
SUITE_START=$(date +%s%N)

# find_stale_artifact_input <artifact>
#   Prints the first source input that is strictly newer than <artifact>, or
#   nothing. Inputs: Cargo.toml, Cargo.lock, .cargo/config.toml, build.rs and
#   src/** of this crate, plus the patched sibling NeuralAmpModeler-rs tree
#   (Cargo.toml + src/**) when present ([patch.crates-io] in Cargo.toml).
find_stale_artifact_input() {
    local artifact="$1"
    local f hit
    for f in Cargo.toml Cargo.lock build.rs .cargo/config.toml; do
        if [ -f "$f" ] && [ "$f" -nt "$artifact" ]; then
            printf '%s\n' "$f"
            return 0
        fi
    done
    if [ -d src ]; then
        hit=$(find src -type f -newer "$artifact" -print -quit 2>/dev/null || true)
        if [ -n "$hit" ]; then
            printf '%s\n' "$hit"
            return 0
        fi
    fi
    if [ -d "../NeuralAmpModeler-rs" ]; then
        if [ -f "../NeuralAmpModeler-rs/Cargo.toml" ] && [ "../NeuralAmpModeler-rs/Cargo.toml" -nt "$artifact" ]; then
            printf '%s\n' "../NeuralAmpModeler-rs/Cargo.toml"
            return 0
        fi
        if [ -d "../NeuralAmpModeler-rs/src" ]; then
            hit=$(find "../NeuralAmpModeler-rs/src" -type f -newer "$artifact" -print -quit 2>/dev/null || true)
            if [ -n "$hit" ]; then
                printf '%s\n' "$hit"
                return 0
            fi
        fi
    fi
    return 1
}

# verify_artifact_fresh <artifact> <profile> <cargo_flag>
#   Fail-closed staleness gate: the artifact must be newer than every
#   source input it is compiled from. In strict mode a stale artifact aborts —
#   there is NO silent rebuild, because validating a stale .so is exactly the
#   defect being closed; the operator must build explicitly first.
verify_artifact_fresh() {
    local artifact="$1"
    local profile="$2"
    local flag="$3"
    local stale sha
    stale=$(find_stale_artifact_input "$artifact") || true
    if [ -n "$stale" ]; then
        die "FATAL: CLAP artifact ($profile) is STALE — '$stale' is newer than $artifact. Run 'cargo build --locked${flag:+ $flag}' first; strict mode never validates a stale artifact (fail-closed staleness gate)."
    fi
    sha=$(sha256sum "$artifact" | cut -d' ' -f1)
    echo -e "  ${GREEN}✓ CLAP artifact ($profile, fresh):${NC} $artifact (sha256: ${sha:0:16}...)"
}

# Helper: Ensure the CLAP plugin shared library artifact for the requested
# profile is present before running integration tests that dlopen it.
#
# Accepts profile "debug" or "release". Checks for the exact profile artifact
# first; does NOT silently fall back to the other profile, since that could
# mask build-time failures specific to debug assertions or release codegen.
# In NAM_QUICK_STRICT=1 the artifact is also freshness-checked: a
# stale .so aborts the suite instead of being validated.
ensure_clap_artifact() {
    local profile="${1:-debug}"
    local flag=""
    if [ "$profile" = "release" ]; then
        flag="--release"
    fi

    # Explicit override: honour CLAP_PLUGIN_UNDER_TEST or CLAP_PLUGIN_PATH
    local explicit="${CLAP_PLUGIN_UNDER_TEST:-${CLAP_PLUGIN_PATH:-}}"
    if [ -n "$explicit" ]; then
        if [ -f "$explicit" ]; then
            if [ "${NAM_QUICK_STRICT:-0}" = "1" ]; then
                verify_artifact_fresh "$explicit" "$profile" "$flag"
            else
                local stale_input
                stale_input=$(find_stale_artifact_input "$explicit") || true
                if [ -n "$stale_input" ]; then
                    warn "Explicit CLAP artifact is stale relative to '$stale_input'; testing it as requested (non-strict)."
                fi
                local sha
                sha=$(sha256sum "$explicit" | cut -d' ' -f1)
                echo -e "  ${GREEN}✓ CLAP artifact ($profile explicit):${NC} $explicit (sha256: ${sha:0:16}...)"
            fi
            return 0
        else
            if [ "${NAM_QUICK_STRICT:-0}" = "1" ]; then
                die "FATAL: Explicit CLAP plugin artifact does not exist: $explicit"
            fi
            warn "Explicit CLAP artifact not found: $explicit. Rebuilding..."
        fi
    fi

    local target_dir="${CARGO_TARGET_DIR:-target}"
    local artifact_path="$target_dir/$profile/libnam_plug.so"

    if [ ! -f "$artifact_path" ]; then
        warn "CLAP plugin ($profile) artifact not found. Pre-building..."
        # shellcheck disable=SC2086
        cargo build --locked $flag
    elif [ "${NAM_QUICK_STRICT:-0}" = "1" ]; then
        verify_artifact_fresh "$artifact_path" "$profile" "$flag"
        return 0
    else
        # Non-strict development flow: stale artifacts are rebuilt (never
        # validated), preserving the permissive PASSED_WITH_GAPS behavior.
        local stale_input
        stale_input=$(find_stale_artifact_input "$artifact_path") || true
        if [ -n "$stale_input" ]; then
            warn "CLAP plugin ($profile) artifact is STALE ('$stale_input' newer). Rebuilding with --locked..."
            # shellcheck disable=SC2086
            cargo build --locked $flag
        fi
    fi

    if [ ! -f "$artifact_path" ]; then
        die "FATAL: CLAP plugin artifact was not created at expected path: $artifact_path"
    fi

    local sha
    sha=$(sha256sum "$artifact_path" | cut -d' ' -f1)
    echo -e "  ${GREEN}✓ CLAP artifact ($profile):${NC} $artifact_path (sha256: ${sha:0:16}...)"
}

# ── Phase 1: Structural unit & integration tests (debug) ─────────────────────
P1_START=$(date +%s%N)
phase "Structural: unit & integration tests (debug)..."
ensure_clap_artifact debug
timeout 300 cargo test --features testing --lib \
    --test clap \
    --test clap_e0_containment_test \
    --test clap_e2_proptest \
    --test processor_bypass_test \
    2>&1 | tee target/logs/quick-phase1.log
assert_ran_tests target/logs/quick-phase1.log 1
P1_DUR_MS=$(( ($(date +%s%N) - P1_START) / 1000000 ))
P1_DUR_STR=$(format_duration_ms "$P1_DUR_MS")
ok "Phase 1 passed (${P1_DUR_STR})"
emit "PHASE1: PASS log=target/logs/quick-phase1.log"

# ── Phase 2: Release verification (release) ─────────────────────────────────
declare -a GAPS=()

P2_START=$(date +%s%N)
phase "Release verification: CLAP .so artifact + float parity oracle (release)..."
ensure_clap_artifact release

release_artifact="${CLAP_PLUGIN_UNDER_TEST:-${CLAP_PLUGIN_PATH:-${CARGO_TARGET_DIR:-target}/release/libnam_plug.so}}"
export CLAP_PLUGIN_UNDER_TEST="$release_artifact"

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
    local local_bin="build/namcore_render/namcore_render"
    if [ -f "$local_bin" ]; then
        echo "$local_bin"
        return 0
    fi
    local sibling_bin="../NeuralAmpModeler-rs/build/namcore_render/namcore_render"
    if [ -f "$sibling_bin" ]; then
        echo "$sibling_bin"
        return 0
    fi
    return 1
}

model_fixture=""
if [ -n "${NAM_FIXTURES_DIR:-}" ] && [ -f "$NAM_FIXTURES_DIR/wavenet_a1_standard.nam" ]; then
    model_fixture="$NAM_FIXTURES_DIR/wavenet_a1_standard.nam"
elif [ -f "tests/fixtures/models/wavenet_a1_standard.nam" ]; then
    model_fixture="tests/fixtures/models/wavenet_a1_standard.nam"
fi

render_bin=$(find_namcore_render || true)
if [ -n "$render_bin" ] && [ -x "$render_bin" ]; then
    echo -e "  ${GREEN}✓ NAMCore render binary:${NC} $render_bin"
    # Multi-rate parity oracle test requires:
    #   1. Release CLAP plugin artifact ($release_artifact)
    #   2. C++ NAMCore render binary ($render_bin)
    #   3. Model fixture: tests/fixtures/models/wavenet_a1_standard.nam
    if [ -f "$release_artifact" ] && [ -f "$model_fixture" ]; then
        oracle_sha=$(sha256sum "$render_bin" | cut -d' ' -f1)
        fixture_sha=$(sha256sum "$model_fixture" | cut -d' ' -f1)
        artifact_sha=$(sha256sum "$release_artifact" | cut -d' ' -f1)

        echo -e "  ${BLUE}→ Executing CLAP × NAMCore float parity oracle (multi-rate)...${NC}"
        warn "oracle=$render_bin (sha256:${oracle_sha:0:16}) artifact=$release_artifact fixture=$model_fixture (sha256:${fixture_sha:0:16})"
        emit "ORACLE_SHA256: $oracle_sha"
        emit "FIXTURE_SHA256: $fixture_sha"
        emit "ARTIFACT_SHA256: $artifact_sha"

        NAM_CORE_RENDER_BIN="$render_bin" timeout 600 cargo test \
            --features testing \
            --release \
            --test clap \
            test_clap_multi_rate_parity_with_cpp_namcore \
            -- --ignored --nocapture \
            2>&1 | tee -a target/logs/quick-phase2.log
        if grep -q "test_clap_multi_rate_parity_with_cpp_namcore .* ok" target/logs/quick-phase2.log; then
            emit "CLAP_CPP_PARITY: PASS"
            ok "Multi-rate parity oracle: PASS"
            assert_ran_tests target/logs/quick-phase2.log 1
            emit "PHASE2: PASS log=target/logs/quick-phase2.log"
        else
            die "PARITY: FAIL test_clap_multi_rate_parity_with_cpp_namcore did not complete successfully"
        fi
    else
        if [ "${NAM_QUICK_STRICT:-0}" = "1" ]; then
            die "PARITY: FAIL missing release artifact or fixture in strict mode"
        fi
        GAPS+=("clap_parity_multi_rate:missing_render_or_fixtures")
        echo -e "${YELLOW}${BOLD}WARN GAP: clap_parity_multi_rate:missing_render_or_fixtures${NC}"
        warn "Actionable: ensure release artifact at $release_artifact and model fixture tests/fixtures/models/wavenet_a1_standard.nam. Oracle found at $render_bin."
        emit "PHASE2: GAP reason=missing_render_or_fixtures"
    fi
else
    if [ "${NAM_QUICK_STRICT:-0}" = "1" ]; then
        die "PARITY: FAIL missing NAMCore render binary in strict mode"
    fi
    GAPS+=("clap_parity_multi_rate:missing_render_or_fixtures")
    echo -e "${YELLOW}${BOLD}WARN GAP: clap_parity_multi_rate:missing_render_or_fixtures${NC}"
    warn "Actionable: set NAM_CORE_RENDER_BIN (path to the NAMCore C++ render binary) or build it locally under build/namcore_render to enable the CLAP parity oracle."
    emit "PHASE2: GAP reason=missing_render_or_fixtures"
fi

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
    if [ "${NAM_QUICK_STRICT:-0}" = "1" ]; then
        die "CABSIM_IR: FAIL missing release artifact in strict mode"
    fi
    GAPS+=("cabsim_ir:missing_release_artifact")
    echo -e "${YELLOW}${BOLD}WARN GAP: cabsim_ir:missing_release_artifact${NC}"
    warn "Actionable: build the release artifact ('cargo build --release') to enable the cab-sim IR artifact test."
    emit "PHASE2: GAP reason=missing_release_artifact"
fi
P2_DUR_MS=$(( ($(date +%s%N) - P2_START) / 1000000 ))
P2_DUR_STR=$(format_duration_ms "$P2_DUR_MS")
ok "Phase 2 passed (${P2_DUR_STR})"

# ── Phase 3: RT-Safety & Heap Allocation Audit (debug, heap-audit) ───────────
P3_START=$(date +%s%N)
phase "RT-Safety & Heap Allocation Audit..."
timeout 120 cargo test --features testing,heap-audit --lib \
    processor_heap_audit_test \
    2>&1 | tee target/logs/quick-heap-audit.log
assert_ran_tests target/logs/quick-heap-audit.log 1
P3_DUR_MS=$(( ($(date +%s%N) - P3_START) / 1000000 ))
P3_DUR_STR=$(format_duration_ms "$P3_DUR_MS")
ok "Phase 3 passed (${P3_DUR_STR})"
emit "PHASE3: PASS log=target/logs/quick-heap-audit.log"
emit "HEAP_AUDIT=RAN"

# ── Receipt & summary ────────────────────────────────────────────────────────
SUITE_END=$(date +%s%N)
TOTAL_DUR_MS=$(( (SUITE_END - SUITE_START) / 1000000 ))
TOTAL_DUR_STR=$(format_duration_ms "$TOTAL_DUR_MS")

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
    echo -e "  ${BOLD}Durations:${NC}"
    echo -e "    - Phase 1:     ${P1_DUR_STR:-N/A}"
    echo -e "    - Phase 2:     ${P2_DUR_STR:-N/A}"
    echo -e "    - Phase 3:     ${P3_DUR_STR:-N/A}"
    echo -e "    - Total:       ${TOTAL_DUR_STR}"
    echo -e "${YELLOW}${BOLD}================================================================================${NC}\n"
    if [ "${NAM_QUICK_STRICT:-0}" = "1" ]; then
        echo -e "${RED}${BOLD}OVERALL: FAIL reason=strict_gaps${NC}"
        emit "OVERALL: FAIL reason=strict_gaps"
        exit 1
    fi
    emit "OVERALL: COMPLETED_WITH_GAPS"
    echo -e "${YELLOW}${BOLD}OVERALL: COMPLETED_WITH_GAPS (${TOTAL_DUR_STR})${NC}"
    exit 0
fi

echo -e "\n${GREEN}${BOLD}================================================================================${NC}"
echo -e "${GREEN}${BOLD}    All quick tests passed! (CLAP)      ${NC}"
echo -e "  ${BOLD}Artifacts saved:${NC}"
echo -e "    - Receipt:     ${CYAN}target/logs/quick-receipt.txt${NC}"
echo -e "    - Phase 1 log: ${CYAN}target/logs/quick-phase1.log${NC}"
echo -e "    - Phase 2 log: ${CYAN}target/logs/quick-phase2.log${NC}"
echo -e "    - Heap log:    ${CYAN}target/logs/quick-heap-audit.log${NC}"
echo -e "  ${BOLD}Durations:${NC}"
echo -e "    - Phase 1:     ${P1_DUR_STR:-N/A}"
echo -e "    - Phase 2:     ${P2_DUR_STR:-N/A}"
echo -e "    - Phase 3:     ${P3_DUR_STR:-N/A}"
echo -e "    - Total:       ${TOTAL_DUR_STR}"
echo -e "${GREEN}${BOLD}================================================================================${NC}\n"
emit "OVERALL: PASSED"
