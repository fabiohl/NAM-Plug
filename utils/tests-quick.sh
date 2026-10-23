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
#   2. Release verification — CLAP .so artifact build + static RT codegen guard (0 alloc,
#      0 illegal divs, 0 illegal TLS) + CLAP × NAMCore float parity oracle (release-only scope).
#   3. RT-Safety heap-audit — zero-alloc process() gate (--features heap-audit).
#
# Each phase persists its output to target/logs/quick-phaseN.log (phase 3:
# quick-heap-audit.log) and the run closes with a typed receipt at
# target/logs/quick-receipt.txt.
#
# Environment:
#   NAM_QUICK_STRICT           — Abort on warnings/gaps (strict mode; default: 0).
#   NAM_LOW_PRIORITY           — Re-exec with nice/ionice (default unless NAM_NO_LOW_PRIORITY=1).
#   NAM_NO_LOW_PRIORITY        — Prevent nice/ionice priority lowering.
#   CARGO_PROFILE_RELEASE_STRIP — Release profile strip mode (default: false, unifies build & test).
#   CARGO_PROFILE_RELEASE_LTO  — Release profile LTO mode (default: false, matches tests-long & tests-gui).
#   CARGO_PROFILE_RELEASE_CODEGEN_UNITS — Release codegen units (default: 16, parallel codegen).
#   NAM_QUICK_SELFTEST         — Diagnostic selftest hook: 'timeout' simulates execution timeout in ~1s.

set -euo pipefail

# Host memory guard: cap parallel rustc jobs to 4 to prevent OOM
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT_PATH="$SCRIPT_DIR/$(basename "${BASH_SOURCE[0]}")"

# ── Memory guard (OOM containment) ──────────────────────────────────────────
# Re-exec inside a transient, memory-capped systemd scope so a debug/release
# build peak is reclaimed inside its own cgroup instead of triggering a global
# OOM that kills the IDE. Fail-open when the user manager/cgroup delegation is
# unavailable. NAM_MEM_GUARD=1 marks the inner invocation.
if [ "${NAM_MEM_GUARD:-0}" != "1" ] && [ "${NAM_NO_MEM_GUARD:-0}" != "1" ]; then
    export NAM_MEM_GUARD=1
    exec "$SCRIPT_DIR/mem-guard.sh" "$SCRIPT_PATH" "$@"
fi

PHASE_TOTAL=3
source "$SCRIPT_DIR/lib/_lib.sh"

# One release profile for BOTH the artifact pre-build and `cargo test --release`:
# Cargo hashes `strip` into the profile, so building with strip=false and testing
# with the default strip=true recompiled the whole fat-LTO dependency graph.
export CARGO_PROFILE_RELEASE_STRIP="${CARGO_PROFILE_RELEASE_STRIP:-false}"

# Release test artifacts never need the shipped fat-LTO profile: disabling LTO
# and restoring parallel codegen keeps the build's peak memory bounded and ensures
# fast execution of the quick QA suite without affecting runtime semantics.
export CARGO_PROFILE_RELEASE_LTO="${CARGO_PROFILE_RELEASE_LTO:-false}"
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS="${CARGO_PROFILE_RELEASE_CODEGEN_UNITS:-16}"

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

set -o errtrace
trap 'status=$?
if [ "$status" -eq 124 ]; then
    echo -e "\n${RED}${BOLD}❌ TIMEOUT: command \"$BASH_COMMAND\" timed out at line $LINENO (phase ${PHASE_NUM:-?}/${PHASE_TOTAL:-?}). Aborting test suite.${NC}"
    emit "PHASE${PHASE_NUM:-?}: TIMEOUT line=$LINENO" 2>/dev/null || true
    emit "OVERALL: FAIL reason=timeout phase=${PHASE_NUM:-?}" 2>/dev/null || true
else
    echo -e "\n${RED}${BOLD}❌ Unexpected error: command \"$BASH_COMMAND\" failed at line $LINENO with status $status (phase ${PHASE_NUM:-?}/${PHASE_TOTAL:-?}). Aborting test suite.${NC}"
    emit "PHASE${PHASE_NUM:-?}: FAIL status=$status line=$LINENO" 2>/dev/null || true
    emit "OVERALL: FAIL reason=error phase=${PHASE_NUM:-?}" 2>/dev/null || true
fi
exit 1' ERR

mkdir -p target/logs target/test_crashes
export NAM_CRASH_DIR="${PROJECT_DIR}/target/test_crashes"
rm -f target/logs/quick-phase1.log \
      target/logs/quick-phase2.log \
      target/logs/quick-heap-audit.log \
      target/logs/quick-receipt.txt \
      target/logs/quick-receipt-check.log \
      target/logs/codegen-quick.json

echo -e "${BLUE}${BOLD}========================================${NC}"
echo -e "${BLUE}${BOLD}        NAM-Plug Quick QA Suite         ${NC}"
echo -e "${BLUE}${BOLD}========================================${NC}"

emit "SUITE: tests-quick"
emit "STRICT: ${NAM_QUICK_STRICT:-0}"
SUITE_START=$(date +%s%N)

# cargo_test_budgeted <timeout_s> <log> [cargo test args...] [-- runtime args...]
#   Compiles the test binaries WITHOUT a time budget (`--no-run`; a cold
#   fat-LTO release build can legitimately take >10 min) and applies the
#   `timeout` budget ONLY to test execution, so the budget measures the
#   test's behaviour and not the compiler. Output is appended to <log>.
#
#   Budget Calibration Policy (T3.2, E2):
#     - Formula: timeout = ceil(2 * p95(execution_s)) rounded to multiple of 30s.
#     - Floor invariant: Never reduce below established conservative floors:
#         * Phase 1 (debug unit/integration tests): 300s
#         * Phase 2 (release CLAP × NAMCore parity oracle): 600s
#         * Phase 2 (release cabsim IR dlopen artifact): 300s
#         * Phase 3 (debug zero-alloc RT heap-audit): 120s
#     - Empirical rationale: test execution is measured separately from build
#       time; budgets must never be increased without verified empirical data.
cargo_test_budgeted() {
    local secs="$1" log="$2"
    shift 2
    if [ "${NAM_QUICK_SELFTEST:-}" = "timeout" ]; then
        timeout 1 sleep 3 2>&1 | tee -a "$log"
        return
    fi
    local -a build_args=()
    local a
    for a in "$@"; do
        [ "$a" = "--" ] && break
        build_args+=("$a")
    done
    local t0 t1 compile_dur_s run_dur_s
    t0=$(date +%s%N)
    cargo test "${build_args[@]}" --no-run 2>&1 | tee -a "$log"
    t1=$(date +%s%N)
    compile_dur_s=$(( (t1 - t0) / 1000000000 ))
    emit "PHASE${PHASE_NUM:-?}_COMPILE_S: $compile_dur_s"

    t0=$(date +%s%N)
    local rc=0
    timeout "$secs" cargo test "$@" 2>&1 | tee -a "$log" || rc=$?
    t1=$(date +%s%N)
    run_dur_s=$(( (t1 - t0) / 1000000000 ))
    if [ "$rc" -eq 0 ]; then
        emit "PHASE${PHASE_NUM:-?}_RUN_S: $run_dur_s"
    fi
    return "$rc"
}

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
    if [ -d "../NeuralAmpModeler-rs" ] && grep -q 'NeuralAmpModeler-rs.*path.*=.*"\.\./NeuralAmpModeler-rs"' Cargo.toml 2>/dev/null; then
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
_ensure_clap_artifact_impl() {
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
        CARGO_PROFILE_RELEASE_STRIP="${CARGO_PROFILE_RELEASE_STRIP:-false}" cargo build --locked $flag
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
            CARGO_PROFILE_RELEASE_STRIP="${CARGO_PROFILE_RELEASE_STRIP:-false}" cargo build --locked $flag
        fi
    fi

    if [ ! -f "$artifact_path" ]; then
        die "FATAL: CLAP plugin artifact was not created at expected path: $artifact_path"
    fi

    local sha
    sha=$(sha256sum "$artifact_path" | cut -d' ' -f1)
    echo -e "  ${GREEN}✓ CLAP artifact ($profile):${NC} $artifact_path (sha256: ${sha:0:16}...)"
}

# Timed wrapper: emits PHASE<n>_BUILD_S so the artifact build/freshness cost is
# attributed in the receipt instead of vanishing between compile and run times.
ensure_clap_artifact() {
    local t0 rc=0
    t0=$(date +%s%N)
    _ensure_clap_artifact_impl "$@" || rc=$?
    emit "PHASE${PHASE_NUM:-?}_BUILD_S: $(( ($(date +%s%N) - t0) / 1000000000 ))"
    return "$rc"
}

# Fail-fast receipt verification: runs the same semantic verifier the release
# gate uses, right after the receipt is sealed. A producer/parser contract
# mismatch therefore surfaces here (seconds) rather than after the whole
# release pipeline. Usage: verify_receipt_fail_fast [extra quick_receipt_check args]
verify_receipt_fail_fast() {
    if [ "${NAM_QUICK_NO_RECEIPT_CHECK:-0}" = "1" ] || [ -n "${NAM_QUICK_SELFTEST:-}" ]; then
        return 0
    fi
    local out rc=0
    out=$(cargo run --quiet --locked --bin quick_receipt_check -- \
        target/logs/quick-receipt.txt "$@" 2>&1) || rc=$?
    if [ "$rc" -ne 0 ]; then
        echo -e "${RED}${BOLD}FAIL: quick receipt failed its own semantic verification:${NC}\n$out" >&2
        exit 1
    fi
    ok "Quick receipt verified (fail-fast)"
}

# ── Phase 1: Structural unit & integration tests (debug) ─────────────────────
P1_START=$(date +%s%N)
phase "Structural: unit & integration tests (debug)..."
ensure_clap_artifact debug
cargo_test_budgeted 300 target/logs/quick-phase1.log \
    --features testing --lib --tests
assert_ran_target target/logs/quick-phase1.log "unittests src/lib.rs" 300
assert_ran_target target/logs/quick-phase1.log "tests/clap.rs" 20
assert_ran_target target/logs/quick-phase1.log "tests/fixtures_freshness.rs" 2
assert_ran_target target/logs/quick-phase1.log "tests/receipt_semantics_test.rs" 5
assert_ran_target target/logs/quick-phase1.log "tests/clap_e0_containment_test.rs" 5
assert_ran_target target/logs/quick-phase1.log "tests/clap_e2_proptest.rs" 2
assert_ran_target target/logs/quick-phase1.log "tests/processor_bypass_test.rs" 1
assert_ran_target target/logs/quick-phase1.log "tests/heap_audit.rs" 1
assert_ran_target target/logs/quick-phase1.log "tests/slint_view_model_test.rs" 1
assert_ran_target target/logs/quick-phase1.log "tests/slint_widgets_test.rs" 1
assert_ran_tests target/logs/quick-phase1.log 350
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
    local cand
    for cand in \
        "build/namcore_render/tools/render" \
        "build/namcore_render/Release/render" \
        "build/namcore_render/Debug/render" \
        "build/namcore_render/render" \
        "build/namcore_render/namcore_render" \
        "../NeuralAmpModeler-rs/build/namcore_render/tools/render" \
        "../NeuralAmpModeler-rs/build/namcore_render/Release/render" \
        "../NeuralAmpModeler-rs/build/namcore_render/Debug/render" \
        "../NeuralAmpModeler-rs/build/namcore_render/render" \
        "../NeuralAmpModeler-rs/build/namcore_render/namcore_render"; do
        if [ -x "$cand" ]; then
            echo "$cand"
            return 0
        fi
    done
    local base hit
    for base in "build/namcore_render" "../NeuralAmpModeler-rs/build/namcore_render"; do
        hit=$(find "$base" -type f \( -name render -o -name namcore_render \) -executable -print -quit 2>/dev/null || true)
        if [ -n "$hit" ]; then
            echo "$hit"
            return 0
        fi
    done
    return 1
}

# E-D parity matrix requires all three certified topologies (WaveNet A1,
# WaveNet A2/Slimmable, LSTM). Resolve one complete fixture directory so a
# partial set becomes a typed gap instead of silently shrinking the matrix.
parity_fixture_dir=""
if [ -n "${NAM_FIXTURES_DIR:-}" ] && [ -f "$NAM_FIXTURES_DIR/wavenet_a1_standard.nam" ]; then
    parity_fixture_dir="$NAM_FIXTURES_DIR"
elif [ -f "tests/fixtures/models/wavenet_a1_standard.nam" ]; then
    parity_fixture_dir="tests/fixtures/models"
fi
if [ -n "$parity_fixture_dir" ]; then
    for required_fixture in wavenet_a1_standard.nam a2_example.nam lstm.nam; do
        if [ ! -f "$parity_fixture_dir/$required_fixture" ]; then
            warn "Parity fixture missing: $parity_fixture_dir/$required_fixture"
            parity_fixture_dir=""
            break
        fi
    done
fi
model_fixture=""
if [ -n "$parity_fixture_dir" ]; then
    model_fixture="$parity_fixture_dir/wavenet_a1_standard.nam"
fi

# verify_parity_log <logfile>
#   Validates that the CLAP × NAMCore float parity test executed and completed
#   successfully (T-P4.1.1, resolves H4). Under --nocapture, cargo test prints
#   the test name, then output diagnostics, and then "ok" or summary, causing
#   name and "ok" to fall across separate lines. The verification predicate checks:
#     1. Executed test name present in log
#     2. Cargo overall test run succeeded ("test result: ok.")
#     3. Absence of failure, panic, or failures section
verify_parity_log() {
    local logfile="$1"
    [ -f "$logfile" ] || return 1
    grep -qE "(test[[:space:]]+.*test_clap_parity_multi_rate|test_clap_parity_multi_rate)" "$logfile" || return 1
    grep -qE "^test result: ok\." "$logfile" || return 1
    if grep -qE "(FAILED|panicked at|failures:)" "$logfile"; then
        return 1
    fi
    return 0
}

# verify_parity_harness_fixtures
#   Validates parity gate robustness against synthetic fixtures (T-P4.1.1):
#     - Wrapped multi-line nocapture output must PASS
#     - Missing test execution must FAIL
verify_parity_harness_fixtures() {
    local fixture_pass="tests/fixtures/logs/parity_wrapped_nocapture_pass.log"
    local fixture_fail="tests/fixtures/logs/parity_missing_fail.log"
    if [ -f "$fixture_pass" ] && ! verify_parity_log "$fixture_pass"; then
        die "HARNESS BUG: verify_parity_log rejected valid wrapped nocapture fixture: $fixture_pass"
    fi
    if [ -f "$fixture_fail" ] && verify_parity_log "$fixture_fail"; then
        die "HARNESS BUG: verify_parity_log falsely accepted missing test fixture: $fixture_fail"
    fi
}

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

        verify_parity_harness_fixtures

        NAM_CORE_RENDER_BIN="$render_bin" cargo_test_budgeted 600 target/logs/quick-phase2.log \
            --features testing \
            --release \
            --test clap \
            test_clap_parity_multi_rate \
            -- --ignored --nocapture
        if verify_parity_log target/logs/quick-phase2.log; then
            emit "CLAP_CPP_PARITY: PASS"
            ok "Multi-rate parity oracle: PASS"
            assert_ran_tests target/logs/quick-phase2.log 1
            emit "PHASE2: PASS log=target/logs/quick-phase2.log"
        else
            die "PARITY: FAIL test_clap_parity_multi_rate did not complete successfully"
        fi

        echo -e "  ${BLUE}→ Executing CLAP × NAMCore oversampling parity (2x/4x)...${NC}"
        NAM_CORE_RENDER_BIN="$render_bin" cargo_test_budgeted 300 target/logs/quick-phase2.log \
            --features testing \
            --release \
            --test clap \
            test_clap_parity_oversample \
            -- --ignored --nocapture
        if grep -q "test_clap_parity_oversample .* ok" target/logs/quick-phase2.log; then
            emit "CLAP_OS_PARITY: PASS"
            ok "Oversampling parity (2x/4x): PASS"
        else
            die "OS_PARITY: FAIL test_clap_parity_oversample did not complete successfully"
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
    cargo_test_budgeted 300 target/logs/quick-phase2.log \
        --features testing --release --test clap \
        test_cabsim_ir_changes_audio_release_artifact -- --ignored --nocapture
    if grep -q "test_cabsim_ir_changes_audio_release_artifact .* ok" target/logs/quick-phase2.log; then
        emit "CABSIM_IR: PASS"
        ok "Cab-sim IR artifact test: PASS"
    else
        die "CABSIM_IR: FAIL test_cabsim_ir_changes_audio_release_artifact did not complete successfully"
    fi

    echo -e "  ${BLUE}→ Executing CabSim parity vs direct convolution (artifact)...${NC}"
    cargo_test_budgeted 300 target/logs/quick-phase2.log \
        --features testing --release --test clap \
        test_clap_parity_cabsim -- --ignored --nocapture
    if grep -q "test_clap_parity_cabsim .* ok" target/logs/quick-phase2.log; then
        emit "CABSIM_PARITY: PASS"
        ok "Cab-sim parity vs direct convolution: PASS"
    else
        die "CABSIM_PARITY: FAIL test_clap_parity_cabsim did not complete successfully"
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

if [ -f "$release_artifact" ] && [ -x "$SCRIPT_DIR/verify_rt_codegen.sh" ]; then
    echo -e "  ${BLUE}→ Verifying real-time codegen invariants (static assembly scan)...${NC}"
    codegen_t0=$(date +%s%N)
    codegen_out=$("$SCRIPT_DIR/verify_rt_codegen.sh" "$release_artifact" \
        --json-out target/logs/codegen-quick.json 2>&1 | tee -a target/logs/quick-phase2.log)
    emit "PHASE2_CODEGEN_S: $(( ($(date +%s%N) - codegen_t0) / 1000000000 ))"
    codegen_metrics=$(printf '%s\n' "$codegen_out" | grep -m1 '^RT_CODEGEN_METRICS: ' || true)
    if [ -z "$codegen_metrics" ]; then
        die "RT_CODEGEN: FAIL verify_rt_codegen.sh did not emit metrics"
    fi
    emit "RT_CODEGEN: PASS ${codegen_metrics#RT_CODEGEN_METRICS: }"
    ok "RT codegen verification: PASS (${codegen_metrics#RT_CODEGEN_METRICS: })"

    # Coverage provenance: the quick artifact is the pre-PGO/BOLT release
    # `.so`. The post-BOLT count is read from the release pipeline's metrics
    # when present, so both receipts carry the same field names.
    pre_bolt_symbols=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["symbols"])' target/logs/codegen-quick.json 2>/dev/null || echo "n/a")
    post_bolt_symbols="n/a"
    if [ -f target/logs/codegen-post-bolt.json ]; then
        post_bolt_symbols=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["symbols"])' target/logs/codegen-post-bolt.json 2>/dev/null || echo "n/a")
    fi
    emit "PRE_BOLT_SYMBOLS: $pre_bolt_symbols"
    emit "POST_BOLT_SYMBOLS: $post_bolt_symbols"
fi

P2_DUR_MS=$(( ($(date +%s%N) - P2_START) / 1000000 ))
P2_DUR_STR=$(format_duration_ms "$P2_DUR_MS")
ok "Phase 2 passed (${P2_DUR_STR})"

# ── Phase 3: RT-Safety & Heap Allocation Audit (debug, heap-audit) ───────────
P3_START=$(date +%s%N)
phase "RT-Safety & Heap Allocation Audit..."
cargo_test_budgeted 120 target/logs/quick-heap-audit.log \
    --features testing,heap-audit --lib \
    processor_heap_audit_test
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
    verify_receipt_fail_fast
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
verify_receipt_fail_fast --require-passed --require-heap-audit
