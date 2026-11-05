#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
# tests-quick.sh — Execução da disciplina rápida de testes. Classe A.
# Uso: utils/tests-quick.sh [-h|--help]
# Orçamento: 40
set -x; PS4='+ ${BASH_SOURCE##*/}:${LINENO}: '
set -euo pipefail
cd "$(dirname "$0")/.."
. utils/_lib.sh

case "${1:-}" in "") ;; -h|--help) echo "Uso: utils/tests-quick.sh"; exit 0 ;; *) usage_fail "tests-quick.sh aceita apenas -h/--help" ;; esac
lib_init
trap 'trap_fail $?' ERR

PHASE_NAME=tests; CUR_LOG="$LOG_DIR/quick-tests.log"; phase tests
rc_tests=0
cargo test --locked --features testing --no-fail-fast > "$CUR_LOG" 2>&1 || rc_tests=$?
if [ "$rc_tests" -ne 0 ]; then
  grep -C 3 -E '(panicked at|FAILED)' "$CUR_LOG" | head -n 50 || true
  grep -q -E '(panicked at|FAILED)' "$CUR_LOG" || { echo "--- Cargo/build failure (tail of $CUR_LOG) ---" >&2; tail -n 25 "$CUR_LOG" >&2; }
fi
awk '/test result: / { p+=$4; f+=$6; i+=$8 } END { printf "Tests summary: %d passed, %d failed, %d ignored\n", p, f, i }' "$CUR_LOG"
# Measured: x86=453 passed, aarch64=441 passed (run 42075116266-1); threshold=400
[ "$rc_tests" -eq 0 ] && assert_ran_tests "$CUR_LOG" 400 || { FAILS+=(tests); emit "PHASE: tests FAIL rc=$rc_tests log=$CUR_LOG"; finish || exit 1; exit 1; }
emit "PHASE: tests PASS log=$CUR_LOG"

# S9-T05: mantido no quick (custo medido 12.4% <= 20% do quick total; zero-alloc RT gate)
PHASE_NAME=heap-audit; CUR_LOG="$LOG_DIR/quick-heap-audit.log"; phase heap-audit
rc_heap=0
cargo test --locked --features testing,heap-audit --lib processor_heap_audit_test > "$CUR_LOG" 2>&1 || rc_heap=$?
if [ "$rc_heap" -ne 0 ]; then
  grep -C 3 -E '(panicked at|FAILED)' "$CUR_LOG" | head -n 50 || true
  grep -q -E '(panicked at|FAILED)' "$CUR_LOG" || { echo "--- Cargo/build failure (tail of $CUR_LOG) ---" >&2; tail -n 25 "$CUR_LOG" >&2; }
fi
# Measured: 1 passed (processor_heap_audit_test); threshold=1
[ "$rc_heap" -eq 0 ] && assert_ran_tests "$CUR_LOG" 1 || { FAILS+=(heap-audit); emit "PHASE: heap-audit FAIL rc=$rc_heap log=$CUR_LOG"; finish || exit 1; exit 1; }
emit "PHASE: heap-audit PASS log=$CUR_LOG"

finish || exit 1
