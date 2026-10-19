#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Deterministic harness test for `cargo_test_budgeted`, trap ERR/errtrace receipt,
# and assert_ran_target tolerance to `Executable` lines (D1, D3).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"

# ANSI style helpers
RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m'

TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

cd "$PROJECT_DIR"

echo -e "${BLUE}${BOLD}===============================================================${NC}"
echo -e "${BLUE}${BOLD}    test_tests_quick_harness: cargo_test_budgeted & trap (D1, D3)${NC}"
echo -e "${BLUE}${BOLD}===============================================================${NC}"

# Source lib helpers for assert_ran_target
# shellcheck source=/dev/null
source "$PROJECT_DIR/utils/lib/_lib_core.sh"
# shellcheck source=/dev/null
source "$PROJECT_DIR/utils/lib/_lib.sh"

# ── Test 1: Compilation (--no-run) is NEVER wrapped by timeout ───────────────
echo -e "\n${BLUE}[1/4] Verifying compilation (--no-run) is never wrapped by timeout & emits telemetry...${NC}"
(
    CALL_LOG="$TMP_DIR/call_order.log"
    EMIT_LOG="$TMP_DIR/test1_emit.log"
    rm -f "$CALL_LOG" "$EMIT_LOG"

    timeout() {
        echo "timeout: $*" >> "$CALL_LOG"
        shift 1
        "$@"
    }

    cargo() {
        echo "cargo: $*" >> "$CALL_LOG"
        return 0
    }

    emit() {
        echo "$*" >> "$EMIT_LOG"
    }

    eval "$(sed -n '/^cargo_test_budgeted()/,/^}/p' "$PROJECT_DIR/utils/tests-quick.sh")"

    PHASE_NUM=1
    cargo_test_budgeted 300 "$TMP_DIR/test1.log" --features testing --lib --tests

    # 1. timeout must NEVER have been called with --no-run
    if grep -F "timeout:" "$CALL_LOG" | grep -F -- "--no-run"; then
        echo -e "${RED}FAIL: timeout wrapped --no-run compilation call!${NC}" >&2
        exit 1
    fi

    # 2. First call must be cargo with --no-run
    first_call=$(head -n 1 "$CALL_LOG")
    if [[ "$first_call" != "cargo: test --features testing --lib --tests --no-run" ]]; then
        echo -e "${RED}FAIL: first call was not unbudgeted cargo --no-run: $first_call${NC}" >&2
        exit 1
    fi

    # 3. Second call must be timeout wrapping cargo test execution without --no-run
    second_call=$(sed -n '2p' "$CALL_LOG")
    if [[ "$second_call" != "timeout: 300 cargo test --features testing --lib --tests" ]]; then
        echo -e "${RED}FAIL: second call was not timeout wrapping test execution: $second_call${NC}" >&2
        exit 1
    fi

    # 4. Verify telemetry lines emitted for compile and run durations
    if ! grep -qE '^PHASE1_COMPILE_S: [0-9]+' "$EMIT_LOG"; then
        echo -e "${RED}FAIL: emit did not record PHASE1_COMPILE_S: (emit log:)${NC}" >&2
        cat "$EMIT_LOG" >&2
        exit 1
    fi
    if ! grep -qE '^PHASE1_RUN_S: [0-9]+' "$EMIT_LOG"; then
        echo -e "${RED}FAIL: emit did not record PHASE1_RUN_S: (emit log:)${NC}" >&2
        cat "$EMIT_LOG" >&2
        exit 1
    fi
)
echo -e "  ${GREEN}✓ Compilation was unbudgeted; timeout applied strictly to test execution; telemetry emitted.${NC}"

# ── Test 2: Execution returning 124 propagates status 124 under pipefail ──────
echo -e "\n${BLUE}[2/4] Verifying execution timeout (124) propagates through pipeline...${NC}"
(
    set -o pipefail

    timeout() {
        return 124
    }

    cargo() {
        return 0
    }

    eval "$(sed -n '/^cargo_test_budgeted()/,/^}/p' "$PROJECT_DIR/utils/tests-quick.sh")"

    set +e
    cargo_test_budgeted 10 "$TMP_DIR/test2.log" --lib
    rc=$?
    set -e

    if [ "$rc" -ne 124 ]; then
        echo -e "${RED}FAIL: cargo_test_budgeted did not propagate exit code 124 (got $rc)${NC}" >&2
        exit 1
    fi
)
echo -e "  ${GREEN}✓ Status 124 successfully propagated under pipefail.${NC}"

# ── Test 3: Trap ERR + errtrace emits typed receipt on timeout ────────────────
echo -e "\n${BLUE}[3/4] Verifying trap ERR + errtrace emits PHASE2: TIMEOUT and OVERALL: FAIL...${NC}"
EMIT_LOG="$TMP_DIR/emit.log"
rm -f "$EMIT_LOG"
set +e
(
    emit() {
        echo "$*" >> "$EMIT_LOG"
    }

    cargo() {
        return 0
    }

    timeout() {
        return 124
    }

    eval "$(sed -n '/^cargo_test_budgeted()/,/^}/p' "$PROJECT_DIR/utils/tests-quick.sh")"

    PHASE_NUM=2
    PHASE_TOTAL=3

    set -e -o pipefail -o errtrace
    # Trap structure mirrored directly from tests-quick.sh
    trap 'status=$?
    if [ "$status" -eq 124 ]; then
        emit "PHASE${PHASE_NUM:-?}: TIMEOUT line=$LINENO"
        emit "OVERALL: FAIL reason=timeout phase=${PHASE_NUM:-?}"
    else
        emit "PHASE${PHASE_NUM:-?}: FAIL status=$status line=$LINENO"
        emit "OVERALL: FAIL reason=error phase=${PHASE_NUM:-?}"
    fi
    exit 1' ERR

    cargo_test_budgeted 600 "$TMP_DIR/test3.log" --release
)
trap_rc=$?
set -e

if ! grep -q "^PHASE2: TIMEOUT" "$EMIT_LOG"; then
    echo -e "${RED}FAIL: emit did not record PHASE2: TIMEOUT (emit log:)${NC}" >&2
    cat "$EMIT_LOG" >&2
    exit 1
fi
if ! grep -q "^OVERALL: FAIL reason=timeout phase=2" "$EMIT_LOG"; then
    echo -e "${RED}FAIL: emit did not record OVERALL: FAIL reason=timeout phase=2${NC}" >&2
    cat "$EMIT_LOG" >&2
    exit 1
fi
echo -e "  ${GREEN}✓ Trap ERR + errtrace emitted terminal timeout receipt entries as expected.${NC}"

# ── Test 4: assert_ran_target handles Executable lines from --no-run ──────────
echo -e "\n${BLUE}[4/4] Verifying assert_ran_target matches Running <target> with Executable lines...${NC}"
MOCK_LOG="$TMP_DIR/quick-phase1-mock.log"
cat <<'EOF' > "$MOCK_LOG"
   Compiling nam-plug v1.0.0 (/home/fabio/NAM/NAM-Plug)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 3.12s
  Executable unittests src/lib.rs (target/debug/deps/nam_plug-74f4b2386bbd7619)
  Executable tests/clap.rs (target/debug/deps/clap-2b507efcf09f18b3)
     Running unittests src/lib.rs (target/debug/deps/nam_plug-74f4b2386bbd7619)

running 419 tests
test config::tests::test_defaults ... ok
test dsp::tests::test_dsp_init ... ok
test result: ok. 419 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.45s

     Running tests/clap.rs (target/debug/deps/clap-2b507efcf09f18b3)

running 25 tests
test tests::test_clap_entry ... ok
test result: ok. 25 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.12s
EOF

# Must succeed on both targets with minimum counts
assert_ran_target "$MOCK_LOG" "unittests src/lib.rs" 300
assert_ran_target "$MOCK_LOG" "tests/clap.rs" 20

# Fail-closed checks:
# a) Target only has 'Executable' line without 'Running': must fail
MOCK_FAIL_NO_RUN="$TMP_DIR/quick-fail-no-run.log"
cat <<'EOF' > "$MOCK_FAIL_NO_RUN"
  Executable unittests src/lib.rs (target/debug/deps/nam_plug-74f4b2386bbd7619)
EOF
if assert_ran_target "$MOCK_FAIL_NO_RUN" "unittests src/lib.rs" 1 2>/dev/null; then
    echo -e "${RED}FAIL: assert_ran_target succeeded when only Executable existed (no Running)!${NC}" >&2
    exit 1
fi

# b) Minimum test count not reached: must fail
if assert_ran_target "$MOCK_LOG" "unittests src/lib.rs" 500 2>/dev/null; then
    echo -e "${RED}FAIL: assert_ran_target succeeded when executed count (419) < min_count (500)!${NC}" >&2
    exit 1
fi
echo -e "  ${GREEN}✓ assert_ran_target accurately parsed Running sections despite Executable headers.${NC}"

echo -e "\n${GREEN}${BOLD}All 4 test_tests_quick_harness verification scenarios passed!${NC}"
