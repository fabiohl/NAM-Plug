#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
# utils/_lib.sh — Helpers locais de NAM-Plug. Sem cópia pinada, sem hash.
# Orçamento: 120

LOG_DIR=target/logs; mkdir -p "$LOG_DIR"
export NAM_RUN_ID="${NAM_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)-$$}"
[ "$(cat "$LOG_DIR/.summary-run" 2>/dev/null)" = "$NAM_RUN_ID" ] || { : > "$LOG_DIR/summary.txt"; printf '%s\n' "$NAM_RUN_ID" > "$LOG_DIR/.summary-run"; }
export PROJECT_DIR="${PROJECT_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
T0=$SECONDS; FAILS=(); GAPS=(); STRICT=0

warn() { echo "WARN: $*" >&2; }
die()  { echo "ERROR: $*" >&2; exit 1; }
ok()   { echo "OK: $*"; }
emit() { echo "$*"; printf '%s\n' "$*" >> "$LOG_DIR/summary.txt"; }

phase() { emit "PHASE: $1"; }
gap()   { GAPS+=("$1"); emit "PHASE: $1 GAP"; }

# run_phase <nome> <timeout_s> <comando…> — executa sem eval, com log; NUNCA aborta o script (classe B).
run_phase() {
  local name=$1 to=${NAM_PHASE_TIMEOUT_S:-$2} log="$LOG_DIR/${SCRIPT_NAME:-run}-$1.log"; shift 2
  phase "$name"
  timeout --kill-after=30 "$to" "$@" 2>&1 | tee "$log"
  local rc=${PIPESTATUS[0]}
  if [ "$rc" -eq 0 ] && { [ "${REQUIRE_TESTS:-1}" = 0 ] || ran_tests "$log"; }; then
    emit "PHASE: $name PASS log=$log"
  else
    FAILS+=("$name"); emit "PHASE: $name FAIL rc=$rc log=$log"
  fi
}

# Zero testes executados = falha (um filtro errado não pode "passar").
# Higieniza escapes ANSI de CI (CARGO_TERM_COLOR: always) e evita armadilha do grep -c || echo 0.
ran_tests() {
  local log="$1" clean n_run n_start n_res n_fail n_pass
  clean=$(sed -E 's/\x1b\[[0-9;]*[a-zA-Z]//g' "$log" 2>/dev/null || true)
  n_run=$(grep -c -E '^\s*(Running|Doc-tests)\b' <<<"$clean" || true)
  n_start=$(grep -c -E '^\s*running [0-9]+ tests?' <<<"$clean" || true)
  n_res=$(grep -c -E '^test result: (ok|FAILED)\.' <<<"$clean" || true)
  n_fail=$(grep -c 'test result: FAILED' <<<"$clean" || true)
  n_pass=$(grep -c -E 'test result: ok\. [1-9][0-9]* passed' <<<"$clean" || true)
  [ "$n_run" -gt 0 ] && [ "$n_res" -ge "$n_run" ] && [ "$n_start" -eq "$n_res" ] \
    && [ "$n_fail" -eq 0 ] && [ "$n_pass" -gt 0 ]
}

finish() {
  local s=PASS
  [ ${#GAPS[@]} -gt 0 ] && s=PASS_WITH_GAPS
  { [ ${#FAILS[@]} -gt 0 ] || { [ "$STRICT" = 1 ] && [ ${#GAPS[@]} -gt 0 ]; }; } && s=FAIL
  emit "RESULT: $s ($((SECONDS - T0))s) fails=${FAILS[*]:-none} gaps=${GAPS[*]:-none}"
  [ "$s" != FAIL ]
}
