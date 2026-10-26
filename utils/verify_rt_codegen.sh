#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
# verify_rt_codegen.sh — Validação de codegen e desmontagem SIMD do hot-path. Classe D.
# Uso: utils/verify_rt_codegen.sh [binário] [--profile <path>] [--json-out <path>]
set -x; PS4='+ ${BASH_SOURCE##*/}:${LINENO}: '
set -euo pipefail
cd "$(dirname "$0")/.."
. utils/_lib.sh

FILTER="utils/dsp-hotpath-filter.py"
PROFILE="utils/codegen-profile.json"
JSON_OUT=""
BUILD_IF_MISSING=0
TEST_SYNTHETIC=0
TARGET_BIN=""

while [ "$#" -gt 0 ]; do
    case "$1" in
        --build-if-missing) BUILD_IF_MISSING=1 ;;
        --test-synthetic-violation) TEST_SYNTHETIC=1 ;;
        --profile) shift; [ "$#" -gt 0 ] || die "--profile requires a path"; PROFILE="$1" ;;
        --json-out) shift; [ "$#" -gt 0 ] || die "--json-out requires a path"; JSON_OUT="$1" ;;
        -h|--help)
            echo "Uso: utils/verify_rt_codegen.sh [binário] [--profile <path>] [--json-out <path>] [--build-if-missing] [--test-synthetic-violation] [-h|--help]"
            exit 0
            ;;
        -*) die "Unknown option: $1" ;;
        *)
            if [ -z "$TARGET_BIN" ]; then TARGET_BIN="$1"; else die "Multiple target binaries: '$TARGET_BIN' and '$1'"; fi
            ;;
    esac
    shift
done

OBJDUMP_TOOL="${OBJDUMP_TOOL:-$(command -v llvm-objdump || command -v objdump || true)}"
PYTHON_TOOL="${PYTHON_BIN:-$(command -v python3 || true)}"

[ -n "$OBJDUMP_TOOL" ] && [ -x "$OBJDUMP_TOOL" ] || die "objdump not found (fail-closed)."
[ -n "$PYTHON_TOOL" ] && [ -x "$PYTHON_TOOL" ] || die "python3 not found (fail-closed)."
[ -f "$FILTER" ] || die "Missing codegen filter: $FILTER (fail-closed)."
[ -f "$PROFILE" ] || die "Missing codegen profile: $PROFILE (fail-closed)."

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
        die "Binary not found: '${TARGET_BIN:-<none found in target/>}' (fail-closed)."
    fi
fi

[ -f "$TARGET_BIN" ] && [ -s "$TARGET_BIN" ] || die "Target binary invalid or empty: $TARGET_BIN (fail-closed)."

ASM_FULL="$(mktemp -t nam-plug-rt-codegen.XXXXXX.asm)"
ASM_BOUNDED="$(mktemp -t nam-plug-rt-codegen-bounded.XXXXXX.asm)"
HOT_SYMBOLS="$(mktemp -t nam-plug-rt-codegen-symbols.XXXXXX.txt)"
ASM_FILTERED="$(mktemp -t nam-plug-rt-codegen-filtered.XXXXXX.asm)"
ASM_SUMMARY="$(mktemp -t nam-plug-rt-codegen-summary.XXXXXX.txt)"
trap 'rm -f "$ASM_FULL" "$ASM_BOUNDED" "$HOT_SYMBOLS" "$ASM_FILTERED" "$ASM_SUMMARY"' EXIT

LC_ALL=C "$OBJDUMP_TOOL" -d --demangle --no-show-raw-insn "$TARGET_BIN" > "$ASM_FULL" 2>/dev/null || true
[ -s "$ASM_FULL" ] || die "Disassembly produced empty output for $TARGET_BIN (fail-closed)."

"$PYTHON_TOOL" - "$PROFILE" "$ASM_FULL" > "$HOT_SYMBOLS" <<'PY'
import json, re, sys
profile_path, dump_path = sys.argv[1:3]
with open(profile_path, "r", encoding="utf-8") as h: profile = json.load(h)
patterns = [re.compile(str(t["pattern"])) for t in profile.get("targets") or []]
if profile.get("symbols"): patterns.append(re.compile(str(profile["symbols"])))
sym_re = re.compile(r"^([0-9a-fA-F]+)\s+<(.+)>:\s*$")
seen = set()
with open(dump_path, "r", encoding="utf-8", errors="replace") as h:
    for line in h:
        m = sym_re.match(line.rstrip("\n"))
        if m and m.group(2) not in seen and any(p.search(m.group(2)) for p in patterns):
            seen.add(m.group(2)); print(m.group(2))
PY

if [ -s "$HOT_SYMBOLS" ] && LC_ALL=C "$OBJDUMP_TOOL" --help 2>&1 | grep -q -- '--disassemble=<sym>'; then
    while IFS= read -r symbol; do
        LC_ALL=C "$OBJDUMP_TOOL" -d --demangle --no-show-raw-insn \
            --disassemble="$symbol" "$TARGET_BIN" 2>/dev/null \
            | sed -E 's/^Disassembly of section \.bolt\.org\.(.*)$/Disassembly of section .\1/' \
            >> "$ASM_BOUNDED" || true
    done < "$HOT_SYMBOLS"
fi

if [ -s "$ASM_BOUNDED" ]; then
    ASM_INPUT="$ASM_BOUNDED"
else
    ASM_INPUT="$ASM_FULL"
    [ -s "$HOT_SYMBOLS" ] && warn "Per-symbol disassembly unsupported by $OBJDUMP_TOOL; fallback to full dump."
fi

FILTER_ARGS=(--full "$ASM_INPUT" --filtered "$ASM_FILTERED" --summary "$ASM_SUMMARY" --profile "$PROFILE")
[ "$TEST_SYNTHETIC" -eq 1 ] && FILTER_ARGS+=(--test-synthetic-violation)

if ! "$PYTHON_TOOL" "$FILTER" "${FILTER_ARGS[@]}"; then
    echo "RT codegen gate FAILED (see summary above)." >&2
    die "RT codegen verification failed for $TARGET_BIN"
fi

METRICS_LINE="$("$PYTHON_TOOL" - "$PROFILE" "$ASM_SUMMARY" "$TARGET_BIN" "$JSON_OUT" <<'PY'
import hashlib, json, os, sys

profile_path, summary_path, binary_path, json_out = sys.argv[1:5]
with open(profile_path, "r", encoding="utf-8") as h: profile = json.load(h)

targets = profile.get("targets") or []
target_ids = [str(t.get("id") or t.get("pattern")) for t in targets]
required_ids = {str(t.get("id") or t.get("pattern")) for t in targets if t.get("required", True)}

counts: dict[str, int] = {}
totals = {"symbols": 0, "instructions": 0, "vfmadd": 0, "call": 0}
with open(summary_path, "r", encoding="utf-8", errors="replace") as h:
    for line in h:
        line = line.rstrip("\n")
        if line.startswith("symbols="):
            for field in line.split():
                k, _, v = field.partition("=")
                if k in totals:
                    try: totals[k] = int(v)
                    except ValueError: pass
            continue
        if not line.startswith("symbol |") and " | " in line:
            parts = line.split(" | ")
            if len(parts) >= 2:
                tid = parts[1]
                counts[tid] = counts.get(tid, 0) + 1

matched = sum(1 for tid in target_ids if counts.get(tid, 0) > 0)
req_matched = sum(1 for tid in required_ids if counts.get(tid, 0) > 0)

with open(binary_path, "rb") as h: digest = hashlib.sha256(h.read()).hexdigest()

doc = {
    "binary": binary_path,
    "sha256": digest,
    "symbols": int(totals["symbols"]),
    "instructions": int(totals["instructions"]),
    "vfmadd": int(totals["vfmadd"]),
    "call": int(totals["call"]),
    "targets_total": len(target_ids),
    "targets_matched": matched,
    "required_total": len(required_ids),
    "required_matched": req_matched,
    "targets": {tid: counts.get(tid, 0) for tid in target_ids},
}

if json_out:
    out_dir = os.path.dirname(json_out)
    if out_dir: os.makedirs(out_dir, exist_ok=True)
    with open(json_out + ".tmp", "w", encoding="utf-8") as f:
        json.dump(doc, f, sort_keys=True)
    os.replace(json_out + ".tmp", json_out)

print(
    f"symbols={doc['symbols']} instructions={doc['instructions']} vfmadd={doc['vfmadd']} "
    f"targets_matched={doc['targets_matched']} targets_total={doc['targets_total']} "
    f"required_matched={doc['required_matched']} required_total={doc['required_total']}"
)
PY
)"
echo "RT_CODEGEN_METRICS: $METRICS_LINE"

ok "RT codegen verification passed cleanly."
exit 0
