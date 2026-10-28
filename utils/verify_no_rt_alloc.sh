#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
# verify_no_rt_alloc.sh — Varredura estática de alocações proibidas no hot-path RT. Classe D.
# Uso: utils/verify_no_rt_alloc.sh
# Orçamento: 53
set -x; PS4='+ ${BASH_SOURCE##*/}:${LINENO}: '
set -euo pipefail
cd "$(dirname "$0")/.."
. utils/_lib.sh

[ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ] && { echo "Uso: utils/verify_no_rt_alloc.sh"; exit 0; }

AWK_PROG="utils/rt_alloc_scan.awk"
PROC_DIR="src/clap/processor"

[ -f "$AWK_PROG" ] || die "missing scanner: $AWK_PROG"
[ -d "$PROC_DIR" ] || die "scan scope missing: $PROC_DIR (fail-closed)"

RT_FILES="$(find "$PROC_DIR" -type f -name '*.rs' | sort)"
SCAN_FILES="$(printf '%s\n' "$RT_FILES" | grep -vE '(^|/)[A-Za-z0-9_]*_test\.rs$' || true)"

[ -n "$SCAN_FILES" ] || die "scan scope is empty after excluding test modules (fail-closed)"

if ! matches=$(awk -f "$AWK_PROG" $SCAN_FILES 2>&1); then
    echo "RT-hostile allocation pattern found in src/clap/processor/:" >&2
    printf '%s\n' "$matches" | sed 's/^/    /' >&2
    die "RT static allocation scan FAILED"
fi

ok "RT allocation scan clean: no Box::new / implicit allocations on the audio-thread path (src/clap/processor/)."
exit 0
