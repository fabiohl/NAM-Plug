#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
# miri-audit.sh — optional Miri UB/aliasing audit (RetiredBoxOverflow + GuiHostBridge + quiescence). Classe D.
# Uso: utils/miri-audit.sh [-h|--help]
# Orçamento: 80
set -x; PS4='+ ${BASH_SOURCE##*/}:${LINENO}: '
set -euo pipefail
cd "$(dirname "$0")/.."
. utils/_lib.sh

case "${1:-}" in "") ;; -h|--help) echo "Uso: utils/miri-audit.sh"; exit 0 ;; *) usage_fail "miri-audit.sh aceita apenas -h/--help" ;; esac
lib_init
trap 'trap_fail $?' ERR

# Post-init preconditions close the discipline (RESULT + carrier) before exit 2.
precondition_fail() {
    FAILS+=(precondition)
    emit "PHASE: precondition FAIL rc=2 log=none ($1)"
    finish || true
    exit 2
}

echo "Miri concurrency & UB audit gate"
echo "  TARGET: clap::plugin::retired_box::tests (RetiredBoxOverflow raw-pointer ring)"
echo "  TARGET: clap::gui::tests::test_gui_host_bridge_roundtrip (GuiHostBridge)"
echo "  TARGET: clap::plugin::quiescence::quiescence_test (teardown fence protocol)"

# Fail-closed prerequisites: an unavailable toolchain is a typed usage
# precondition (exit 2), never a silent success.
command -v rustup >/dev/null 2>&1 || {
    echo "MIRI: rustup not found — install a nightly toolchain with the miri component." >&2
    precondition_fail "rustup not found"
}
rustup toolchain list 2>/dev/null | grep -q "nightly" || {
    echo "MIRI: no nightly toolchain — rustup toolchain install nightly --profile minimal" >&2
    precondition_fail "no nightly toolchain"
}
cargo +nightly miri --version >/dev/null 2>&1 || {
    echo "MIRI: component missing — rustup component add miri --toolchain nightly" >&2
    precondition_fail "miri component missing"
}

PHASE_NAME=preflight; CUR_LOG="$LOG_DIR/miri-audit-preflight.log"; phase preflight
# A failed sysroot build invalidates the whole audit: never ignore it.
CARGO_BUILD_WARNINGS=allow cargo +nightly miri setup >"$CUR_LOG" 2>&1
emit "PHASE: preflight PASS log=$CUR_LOG"

# -Zmiri-disable-isolation: allow host clock / pseudo-sleeps.
# -Zmiri-ignore-leaks: `RetiredBoxOverflow` intentionally leaks overwritten
# shells by design (fail-closed overflow policy), so leak reports are expected.
export MIRIFLAGS="${MIRIFLAGS:--Zmiri-disable-isolation -Zmiri-ignore-leaks}"

PHASE_NAME=retired_box; CUR_LOG="$LOG_DIR/miri-audit-retired-box.log"; phase retired_box
cargo +nightly miri test --lib clap::plugin::retired_box::tests 2>&1 | tee "$CUR_LOG"
assert_ran_tests "$CUR_LOG" 2
emit "PHASE: retired_box PASS log=$CUR_LOG"

PHASE_NAME=gui_bridge; CUR_LOG="$LOG_DIR/miri-audit-gui-bridge.log"; phase gui_bridge
cargo +nightly miri test --lib clap::gui::tests::test_gui_host_bridge_roundtrip 2>&1 | tee "$CUR_LOG"
assert_ran_tests "$CUR_LOG" 1
emit "PHASE: gui_bridge PASS log=$CUR_LOG"

PHASE_NAME=quiescence; CUR_LOG="$LOG_DIR/miri-audit-quiescence.log"; phase quiescence
cargo +nightly miri test --lib clap::plugin::quiescence::quiescence_test 2>&1 | tee "$CUR_LOG"
assert_ran_tests "$CUR_LOG" 11
emit "PHASE: quiescence PASS log=$CUR_LOG"

ok "Miri aliasing/UB audit completed successfully (retired_box 2/2 + gui bridge 1/1 + quiescence 11/11)!"
finish || exit 1