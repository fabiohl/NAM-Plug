#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
# verify_rt_codegen.sh — Validação de codegen e desmontagem SIMD do hot-path. Classe D.
# Uso: utils/verify_rt_codegen.sh [binário] [--profile <path>] [--json-out <path>]
# Orçamento: 250
set -x; PS4='+ ${BASH_SOURCE##*/}:${LINENO}: '
set -euo pipefail
cd "$(dirname "$0")/.."
. utils/_lib.sh

FILTER="utils/dsp-hotpath-filter.py"
REPORT="utils/codegen-report.py"
PROFILE="utils/codegen-profile.json"
JSON_OUT=""
PRE_ASM_FULL=""
PHASE_NAME="codegen"
BUILD_IF_MISSING=0
TEST_SYNTHETIC=0
TARGET_BIN=""

while [ "$#" -gt 0 ]; do
    case "$1" in
        --build-if-missing) BUILD_IF_MISSING=1 ;;
        --test-synthetic-violation) TEST_SYNTHETIC=1 ;;
        --phase-name) shift; [ "$#" -gt 0 ] || usage_fail "--phase-name requires a value"; PHASE_NAME="$1" ;;
        --full-dump) shift; [ "$#" -gt 0 ] || usage_fail "--full-dump requires a path"; PRE_ASM_FULL="$1" ;;
        --profile) shift; [ "$#" -gt 0 ] || usage_fail "--profile requires a path"; PROFILE="$1" ;;
        --json-out) shift; [ "$#" -gt 0 ] || usage_fail "--json-out requires a path"; JSON_OUT="$1" ;;
        -h|--help)
            echo "Uso: utils/verify_rt_codegen.sh [binário] [--profile <path>] [--json-out <path>] [--phase-name <name>] [--full-dump <path>] [--build-if-missing] [--test-synthetic-violation] [-h|--help]"
            exit 0
            ;;
        -*) usage_fail "Unknown option: $1" ;;
        *)
            if [ -z "$TARGET_BIN" ]; then TARGET_BIN="$1"; else usage_fail "Multiple target binaries: '$TARGET_BIN' and '$1'"; fi
            ;;
    esac
    shift
done

lib_init
trap 'trap_fail $?' ERR

CUR_LOG="$LOG_DIR/${SCRIPT_NAME}-${PHASE_NAME}.log"
phase "$PHASE_NAME"

OBJDUMP_TOOL="${OBJDUMP_TOOL:-$(command -v llvm-objdump || command -v objdump || true)}"

if [ -z "$OBJDUMP_TOOL" ] || [ ! -x "$OBJDUMP_TOOL" ]; then
    FAILS+=("objdump_missing")
    emit "PHASE: codegen FAIL rc=2 log=none (objdump not found, fail-closed)"
    finish || true
    exit 2
fi

if [ ! -x "$FILTER" ]; then
    FAILS+=("filter_missing")
    emit "PHASE: codegen FAIL rc=2 log=none (Missing or non-executable codegen filter: $FILTER)"
    finish || true
    exit 2
fi

if [ ! -x "$REPORT" ]; then
    FAILS+=("report_missing")
    emit "PHASE: codegen FAIL rc=2 log=none (Missing or non-executable codegen report: $REPORT)"
    finish || true
    exit 2
fi

if [ ! -f "$PROFILE" ]; then
    FAILS+=("profile_missing")
    emit "PHASE: codegen FAIL rc=2 log=none (Missing codegen profile: $PROFILE)"
    finish || true
    exit 2
fi

if [ -z "$TARGET_BIN" ]; then
    if [ -n "${NAM_CLAP_SO_PATH:-}" ]; then
        TARGET_BIN="$NAM_CLAP_SO_PATH"
    elif [ -n "${CLAP_PLUGIN_UNDER_TEST:-}" ]; then
        TARGET_BIN="$CLAP_PLUGIN_UNDER_TEST"
    else
        for cand in \
            "$PROJECT_DIR/target/release/libnam_plug.so" \
            "$PROJECT_DIR/target/pgo-clap/dist/libnam_plug.bolt.so" \
            "$PROJECT_DIR/target/pgo-clap/dist/libnam_plug.so" \
            "$PROJECT_DIR/target/dist/libnam_plug.so"; do
            if [ -f "$cand" ] && [ -s "$cand" ]; then
                TARGET_BIN="$cand"; break
            fi
        done
    fi
fi

if [ -z "$TARGET_BIN" ] || [ ! -f "$TARGET_BIN" ]; then
    if [ "$BUILD_IF_MISSING" -eq 1 ]; then
        warn "Target binary missing. Building release library with symbols..."
        CARGO_PROFILE_RELEASE_STRIP="false" cargo build --release --lib
        TARGET_BIN="$PROJECT_DIR/target/release/libnam_plug.so"
    else
        FAILS+=("target_bin_missing")
        emit "PHASE: codegen FAIL rc=1 log=none (Binary not found: '${TARGET_BIN:-<none found in target/>}')"
        finish || true
        exit 1
    fi
fi

if [ ! -f "$TARGET_BIN" ] || [ ! -s "$TARGET_BIN" ]; then
    FAILS+=("target_bin_invalid")
    emit "PHASE: $PHASE_NAME FAIL rc=1 log=none (Target binary invalid or empty: $TARGET_BIN)"
    finish || true
    exit 1
fi

ASM_FULL=""
ASM_BOUNDED="$(mktemp -t nam-plug-rt-codegen-bounded.XXXXXX.asm)"
HOT_SYMBOLS="$(mktemp -t nam-plug-rt-codegen-symbols.XXXXXX.txt)"
ASM_FILTERED="$(mktemp -t nam-plug-rt-codegen-filtered.XXXXXX.asm)"
ASM_SUMMARY="$(mktemp -t nam-plug-rt-codegen-summary.XXXXXX.txt)"
TMP_ERR="$(mktemp -t nam-plug-rt-codegen-err.XXXXXX.log)"

if [ -n "$PRE_ASM_FULL" ] && [ -s "$PRE_ASM_FULL" ]; then
    ASM_FULL="$PRE_ASM_FULL"
    trap 'rm -f "$ASM_BOUNDED" "$HOT_SYMBOLS" "$ASM_FILTERED" "$ASM_SUMMARY" "$TMP_ERR"' EXIT
else
    ASM_FULL="$(mktemp -t nam-plug-rt-codegen.XXXXXX.asm)"
    trap 'rm -f "$ASM_FULL" "$ASM_BOUNDED" "$HOT_SYMBOLS" "$ASM_FILTERED" "$ASM_SUMMARY" "$TMP_ERR"' EXIT

    if ! LC_ALL=C "$OBJDUMP_TOOL" -d --demangle --no-show-raw-insn "$TARGET_BIN" > "$ASM_FULL" 2> "$TMP_ERR"; then
        cat "$TMP_ERR" >&2
        cat "$TMP_ERR" >> "$CUR_LOG"
        FAILS+=("objdump_full")
        emit "PHASE: $PHASE_NAME FAIL rc=1 log=$CUR_LOG (objdump full disassembly failed)"
        finish || true
        exit 1
    fi
fi

if [ ! -s "$ASM_FULL" ]; then
    echo "Disassembly produced empty output for $TARGET_BIN" >> "$CUR_LOG"
    FAILS+=("objdump_empty")
    emit "PHASE: $PHASE_NAME FAIL rc=1 log=$CUR_LOG (Disassembly produced empty output for $TARGET_BIN)"
    finish || true
    exit 1
fi

if ! "$REPORT" hot-symbols "$PROFILE" "$ASM_FULL" > "$HOT_SYMBOLS" 2>> "$CUR_LOG"; then
    FAILS+=("hot_symbols")
    emit "PHASE: $PHASE_NAME FAIL rc=1 log=$CUR_LOG (hot-symbols extraction failed)"
    finish || true
    exit 1
fi

HAS_SYM_DISASM=0
if [ -s "$HOT_SYMBOLS" ] && LC_ALL=C "$OBJDUMP_TOOL" --help 2>&1 | grep -q -- '--disassemble=<sym>'; then
    HAS_SYM_DISASM=1
    TMP_SYM_ASM="$(mktemp -t nam-plug-rt-sym.XXXXXX.asm)"
    TMP_SYM_ERR="$(mktemp -t nam-plug-rt-sym.XXXXXX.err)"
    while IFS= read -r symbol; do
        [ -n "$symbol" ] || continue
        if ! LC_ALL=C "$OBJDUMP_TOOL" -d --demangle --no-show-raw-insn \
            --disassemble="$symbol" "$TARGET_BIN" > "$TMP_SYM_ASM" 2> "$TMP_SYM_ERR"; then
            cat "$TMP_SYM_ERR" >&2
            cat "$TMP_SYM_ERR" >> "$CUR_LOG"
            rm -f "$TMP_SYM_ASM" "$TMP_SYM_ERR"
            FAILS+=("objdump:$symbol")
            emit "PHASE: $PHASE_NAME FAIL rc=1 log=$CUR_LOG (failed to disassemble $symbol)"
            finish || true
            exit 1
        fi
        if [ ! -s "$TMP_SYM_ASM" ]; then
            echo "Empty disassembly for symbol: $symbol" >> "$CUR_LOG"
            rm -f "$TMP_SYM_ASM" "$TMP_SYM_ERR"
            FAILS+=("objdump:$symbol")
            emit "PHASE: $PHASE_NAME FAIL rc=1 log=$CUR_LOG (empty disassembly for $symbol)"
            finish || true
            exit 1
        fi
        sed -E 's/^Disassembly of section \.bolt\.org\.(.*)$/Disassembly of section .\1/' "$TMP_SYM_ASM" >> "$ASM_BOUNDED"
    done < "$HOT_SYMBOLS"
    rm -f "$TMP_SYM_ASM" "$TMP_SYM_ERR"
fi

if [ -s "$ASM_BOUNDED" ]; then
    ASM_INPUT="$ASM_BOUNDED"
else
    ASM_INPUT="$ASM_FULL"
    if [ -s "$HOT_SYMBOLS" ] && [ "$HAS_SYM_DISASM" -eq 0 ]; then
        warn "Per-symbol disassembly unsupported by $OBJDUMP_TOOL; fallback to full dump."
    fi
fi

FILTER_ARGS=(--full "$ASM_INPUT" --filtered "$ASM_FILTERED" --summary "$ASM_SUMMARY" --profile "$PROFILE")
[ "$TEST_SYNTHETIC" -eq 1 ] && FILTER_ARGS+=(--test-synthetic-violation)

if ! "$FILTER" "${FILTER_ARGS[@]}" >> "$CUR_LOG" 2>&1; then
    cat "$ASM_SUMMARY" 2>/dev/null >> "$CUR_LOG" || true
    echo "RT codegen gate FAILED (see summary above)." >&2
    FAILS+=("filter")
    emit "PHASE: $PHASE_NAME FAIL rc=1 log=$CUR_LOG (RT codegen verification failed for $TARGET_BIN)"
    finish || true
    exit 1
fi

cat "$ASM_SUMMARY" 2>/dev/null >> "$CUR_LOG" || true
METRICS_LINE="$("$REPORT" metrics "$PROFILE" "$ASM_SUMMARY" "$TARGET_BIN" "$JSON_OUT")"
echo "RT_CODEGEN_METRICS: $METRICS_LINE"

emit "PHASE: $PHASE_NAME PASS log=$CUR_LOG"
ok "RT codegen verification passed cleanly."
finish
