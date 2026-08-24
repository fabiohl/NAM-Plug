#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Binary artifact inspection guard: verifies zero AVX-512 symbols and zero
# EVEX/ZMM instructions in default (non-avx512) release compilation artifacts
# using the fail-closed nam_bin_guard scanner (Sprint 4 / F-ROB-PLUG-10).
#
# Usage:
#   utils/verify_no_avx512_release.sh [path/to/artifact.so|.rlib|binary]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/_lib.sh"

TARGET="${1:-$PROJECT_DIR/target/release/libnam_plug.so}"

if [ ! -f "$TARGET" ]; then
    warn "Target artifact not found: $TARGET. Building default release..."
    ( cd "$PROJECT_DIR" && cargo build --release --locked )
fi

if [ ! -f "$TARGET" ]; then
    die "Artifact not found after build: $TARGET"
fi

HASH="$(sha256sum "$TARGET" | cut -d' ' -f1)"
if [ -z "$HASH" ]; then
    die "failed to compute SHA-256 of $TARGET"
fi
echo -e "  ${CYAN}Scanning artifact:${NC} $TARGET (sha256: ${HASH:0:16}...)"

GUARD_BIN="$PROJECT_DIR/target/debug/nam_bin_guard"
if [ ! -x "$GUARD_BIN" ]; then
    warn "Building QA scanner (nam_bin_guard)..."
    ( cd "$PROJECT_DIR" && cargo build --quiet --locked --features testing --bin nam_bin_guard )
fi

if [ ! -x "$GUARD_BIN" ]; then
    die "nam_bin_guard was not produced at $GUARD_BIN (fail-closed)"
fi

if ! "$GUARD_BIN" scan "$TARGET"; then
    die "Binary certification FAILED for $TARGET (sha256=$HASH)"
fi

ok "Binary scan passed: clean x86-64-v3 baseline without AVX-512 leaks (sha256=$HASH)."
exit 0
