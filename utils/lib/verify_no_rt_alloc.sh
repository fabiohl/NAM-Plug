#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Static RT-allocation guard.
#
# Verifies that the audio-thread code under `src/clap/processor/` contains no
# `Box::new` or implicit heap allocations outside the documented off-RT sites
# (`activate()`, the panic helpers, and `#[cfg(test)]` test modules).
#
# This is the static half of the zero-alloc contract; the dynamic half is the
# heap-audit CI lane (`cargo test --features heap-audit`). Fail-closed: an
# empty scan scope or a scanner failure aborts with exit 1.
#
# Usage:
#   utils/lib/verify_no_rt_alloc.sh
#
# Exit codes:
#   0 — RT path is allocation-free by static inspection
#   1 — RT-hostile allocation pattern found (or the scan could not run)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/_lib.sh"

AWK_PROG="$SCRIPT_DIR/rt_alloc_scan.awk"
PROC_DIR="$PROJECT_DIR/src/clap/processor"

if [ ! -f "$AWK_PROG" ]; then
    die "missing scanner: $AWK_PROG"
fi
if [ ! -d "$PROC_DIR" ]; then
    die "scan scope missing: $PROC_DIR (fail-closed)"
fi

# Scope: every Rust source inside the RT processor directory. Sibling
# `*_test.rs` files are `#[cfg(test)]`-included test modules per repo
# convention (`#[cfg(test)] #[path = "..."] mod`) and are excluded.
RT_FILES="$(find "$PROC_DIR" -type f -name '*.rs' | sort)"
SCAN_FILES="$(printf '%s\n' "$RT_FILES" | grep -vE '(^|/)src/clap/processor/[A-Za-z0-9_]*_test\.rs$' || true)"

if [ -z "$SCAN_FILES" ]; then
    die "scan scope is empty after excluding test modules (fail-closed)"
fi

if ! matches=$(awk -f "$AWK_PROG" $SCAN_FILES 2>&1); then
    echo -e "  ${RED}${BOLD}RT-hostile allocation pattern found in src/clap/processor/:${NC}"
    printf '%s\n' "$matches" | sed 's/^/    /'
    die "RT static allocation scan FAILED"
fi

ok "RT allocation scan clean: no Box::new / implicit allocations on the audio-thread path (src/clap/processor/)."
exit 0
