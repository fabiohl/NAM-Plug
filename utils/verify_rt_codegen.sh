#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Static RT-codegen guard for NAM-Plug.
#
# Disassembles the compiled CLAP plugin shared library (.so / .clap) and runs
# the unified NAM codegen filter (`utils/lib/dsp-hotpath-filter.py`) with the
# per-repo symbol profile (`utils/codegen-profile.json`, T5.2) over the hot-path
# audio routines (`process`, `process_sub_block`, `process_dry_contained_block`,
# ...):
#
# Invariants enforced per target (see the profile's per-target budgets):
#   1. Zero heap allocations/deallocations (`malloc`, `calloc`, `realloc`, `free`,
#      `__rust_alloc`, ...).
#   2. Zero integer division (`div`/`idiv`) in the core DSP loop; `process()`
#      and `process_dry_contained_block()` allow the 2 telemetry divisions.
#   3. Zero thread-local storage accesses (`call __tls_get_addr`) in the core
#      DSP loop; `process()` allows the cold lifecycle/mode-switch budget.
#
# The symbol/gate policy lives entirely in the profile and the filtering logic
# in the shared Python filter — this script is only the fail-closed driver.
#
# Fail-closed policy:
#   - Missing binary -> exit 1
#   - Missing tools (`objdump`, `python3`) -> exit 1
#   - Missing filter/profile -> exit 1
#   - Empty disassembly output -> exit 1
#   - Missing required hot-path symbols -> exit 1 (filter)
#   - Any code invariant violation -> exit 1 (filter)
#
# Usage:
#   utils/verify_rt_codegen.sh [path/to/libnam_plug.so] [options]
#
# Options:
#   --build-if-missing          Build release lib with symbols if candidate is missing
#   --test-synthetic-violation  Inject synthetic failure for fail-closed verification

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/lib/_lib.sh"

FILTER="$SCRIPT_DIR/lib/dsp-hotpath-filter.py"
PROFILE="$SCRIPT_DIR/codegen-profile.json"

BUILD_IF_MISSING=0
TEST_SYNTHETIC=0
TARGET_BIN=""

for arg in "$@"; do
    case "$arg" in
        --build-if-missing)
            BUILD_IF_MISSING=1
            ;;
        --test-synthetic-violation)
            TEST_SYNTHETIC=1
            ;;
        -*)
            die "Unknown option: $arg"
            ;;
        *)
            if [ -z "$TARGET_BIN" ]; then
                TARGET_BIN="$arg"
            else
                die "Multiple target binaries specified: '$TARGET_BIN' and '$arg'"
            fi
            ;;
    esac
done

# Resolve tools (fail-closed)
OBJDUMP_TOOL="${OBJDUMP_TOOL:-$(command -v llvm-objdump || command -v objdump || true)}"
PYTHON_TOOL="${PYTHON_BIN:-$(command -v python3 || true)}"

if [ -z "$OBJDUMP_TOOL" ] || [ ! -x "$OBJDUMP_TOOL" ]; then
    die "Required disassembly tool 'objdump' not found or not executable (fail-closed)."
fi
if [ -z "$PYTHON_TOOL" ] || [ ! -x "$PYTHON_TOOL" ]; then
    die "Required script runner 'python3' not found or not executable (fail-closed)."
fi
if [ ! -f "$FILTER" ]; then
    die "Missing shared codegen filter: $FILTER (fail-closed)."
fi
if [ ! -f "$PROFILE" ]; then
    die "Missing codegen symbol profile: $PROFILE (fail-closed)."
fi

# Locate candidate binary if not explicitly provided
if [ -z "$TARGET_BIN" ]; then
    if [ -n "${NAM_CLAP_SO_PATH:-}" ]; then
        TARGET_BIN="$NAM_CLAP_SO_PATH"
    elif [ -n "${CLAP_PLUGIN_UNDER_TEST:-}" ]; then
        TARGET_BIN="$CLAP_PLUGIN_UNDER_TEST"
    else
        # Candidate search order: release unstripped > PGO+BOLT > PGO > dist
        for cand in \
            "$PROJECT_DIR/target/release/libnam_plug.so" \
            "$PROJECT_DIR/target/pgo-clap/dist/libnam_plug.bolt.so" \
            "$PROJECT_DIR/target/pgo-clap/dist/libnam_plug.so" \
            "$PROJECT_DIR/target/dist/libnam_plug.so"; do
            if [ -f "$cand" ] && [ -s "$cand" ]; then
                TARGET_BIN="$cand"
                break
            fi
        done
    fi
fi

# Pre-build if requested and binary is missing
if [ -z "$TARGET_BIN" ] || [ ! -f "$TARGET_BIN" ]; then
    if [ "$BUILD_IF_MISSING" -eq 1 ]; then
        warn "Target binary missing. Building release library with symbols (--profile release, strip=false)..."
        CARGO_PROFILE_RELEASE_STRIP="false" cargo build --release --lib
        TARGET_BIN="$PROJECT_DIR/target/release/libnam_plug.so"
    else
        die "Binary not found: '${TARGET_BIN:-<none found in target/>}' (fail-closed)."
    fi
fi

if [ ! -f "$TARGET_BIN" ]; then
    die "Target binary does not exist: $TARGET_BIN (fail-closed)."
fi

if [ ! -s "$TARGET_BIN" ]; then
    die "Target binary is empty (0 bytes): $TARGET_BIN (fail-closed)."
fi

echo -e "  ${BLUE}Inspecting binary:${NC} $TARGET_BIN"
echo -e "  ${BLUE}Tools:${NC} objdump=$OBJDUMP_TOOL, python=$PYTHON_TOOL, profile=$PROFILE"

ASM_FULL="$(mktemp -t nam-plug-rt-codegen.XXXXXX.asm)"
ASM_FILTERED="$(mktemp -t nam-plug-rt-codegen-filtered.XXXXXX.asm)"
ASM_SUMMARY="$(mktemp -t nam-plug-rt-codegen-summary.XXXXXX.txt)"
trap 'rm -f "$ASM_FULL" "$ASM_FILTERED" "$ASM_SUMMARY"' EXIT

LC_ALL=C "$OBJDUMP_TOOL" -d --demangle --no-show-raw-insn "$TARGET_BIN" > "$ASM_FULL" 2>/dev/null || true
if [ ! -s "$ASM_FULL" ]; then
    die "Disassembly produced empty output for $TARGET_BIN (stripped binary?) (fail-closed)."
fi

FILTER_ARGS=(--full "$ASM_FULL" --filtered "$ASM_FILTERED" --summary "$ASM_SUMMARY" --profile "$PROFILE")
if [ "$TEST_SYNTHETIC" -eq 1 ]; then
    FILTER_ARGS+=(--test-synthetic-violation)
fi

if ! "$PYTHON_TOOL" "$FILTER" "${FILTER_ARGS[@]}"; then
    echo -e "  ${RED}${BOLD}RT codegen gate FAILED (see summary above).${NC}" >&2
    die "RT codegen verification failed for $TARGET_BIN"
fi

ok "RT codegen verification passed cleanly."
exit 0
