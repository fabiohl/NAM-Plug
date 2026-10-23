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
#   --profile PATH              Use an alternate codegen profile
#   --json-out PATH             Write machine-readable per-target metrics
#   --post-bolt-regression      Run tests/scripts/test_rt_codegen_post_bolt.sh
#                               after a successful verification (never recursive:
#                               the regression script invokes this driver without
#                               the flag).
#
# Post-BOLT artifacts keep cold/unmoved functions in `.bolt.org.text` and leave
# dead bytes from moved functions in that section. Disassembling the symbol table
# naively therefore attributes dead code to the preceding live symbol. The driver
# builds a bounded dump (one `objdump --disassemble=<symbol>` per profile match)
# and normalizes `.bolt.org.text*` to `.text*` so the shared filter only gates the
# live function bodies in both the pre-BOLT and post-BOLT binaries.

set -euo pipefail

# Host memory guard: cap parallel rustc jobs to 4 to prevent OOM
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/lib/_lib.sh"

FILTER="$SCRIPT_DIR/lib/dsp-hotpath-filter.py"
PROFILE="$SCRIPT_DIR/codegen-profile.json"
PROFILE_OVERRIDE=""
JSON_OUT=""

BUILD_IF_MISSING=0
TEST_SYNTHETIC=0
POST_BOLT_REGRESSION=0
TARGET_BIN=""

while [ "$#" -gt 0 ]; do
    arg="$1"
    case "$arg" in
        --build-if-missing)
            BUILD_IF_MISSING=1
            ;;
        --test-synthetic-violation)
            TEST_SYNTHETIC=1
            ;;
        --post-bolt-regression)
            POST_BOLT_REGRESSION=1
            ;;
        --profile)
            shift
            if [ "$#" -eq 0 ]; then
                die "--profile requires a path"
            fi
            PROFILE_OVERRIDE="$1"
            ;;
        --json-out)
            shift
            if [ "$#" -eq 0 ]; then
                die "--json-out requires a path"
            fi
            JSON_OUT="$1"
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
    shift
done

if [ -n "$PROFILE_OVERRIDE" ]; then
    PROFILE="$PROFILE_OVERRIDE"
fi

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
ASM_BOUNDED="$(mktemp -t nam-plug-rt-codegen-bounded.XXXXXX.asm)"
HOT_SYMBOLS="$(mktemp -t nam-plug-rt-codegen-symbols.XXXXXX.txt)"
ASM_FILTERED="$(mktemp -t nam-plug-rt-codegen-filtered.XXXXXX.asm)"
ASM_SUMMARY="$(mktemp -t nam-plug-rt-codegen-summary.XXXXXX.txt)"
trap 'rm -f "$ASM_FULL" "$ASM_BOUNDED" "$HOT_SYMBOLS" "$ASM_FILTERED" "$ASM_SUMMARY"' EXIT

LC_ALL=C "$OBJDUMP_TOOL" -d --demangle --no-show-raw-insn "$TARGET_BIN" > "$ASM_FULL" 2>/dev/null || true
if [ ! -s "$ASM_FULL" ]; then
    die "Disassembly produced empty output for $TARGET_BIN (stripped binary?) (fail-closed)."
fi

# Enumerate every symbol whose (demangled) name matches a profile target. The
# shared filter uses the same regex search, so the bounded dump covers exactly
# the symbols the gate will evaluate.
"$PYTHON_TOOL" - "$PROFILE" "$ASM_FULL" > "$HOT_SYMBOLS" <<'PY'
import json
import re
import sys

profile_path, dump_path = sys.argv[1:3]
with open(profile_path, "r", encoding="utf-8") as handle:
    profile = json.load(handle)
patterns = []
for target in profile.get("targets") or []:
    patterns.append(re.compile(str(target["pattern"])))
if profile.get("symbols"):
    patterns.append(re.compile(str(profile["symbols"])))
symbol_re = re.compile(r"^([0-9a-fA-F]+)\s+<(.+)>:\s*$")
seen = set()
with open(dump_path, "r", encoding="utf-8", errors="replace") as handle:
    for line in handle:
        match = symbol_re.match(line.rstrip("\n"))
        if not match:
            continue
        name = match.group(2)
        if name in seen:
            continue
        if any(pattern.search(name) for pattern in patterns):
            seen.add(name)
            print(name)
PY

# `objdump --disassemble=<symbol>` honors the symbol size, unlike a plain
# section dump that keeps dead bytes left behind by BOLT function reordering.
# If the tool cannot disassemble a single symbol, fall back to the full dump so
# the gate still runs (and required targets still fail closed if they moved).
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
    if [ -s "$HOT_SYMBOLS" ]; then
        warn "Per-symbol disassembly unsupported by $OBJDUMP_TOOL; falling back to full section dump."
    fi
fi

FILTER_ARGS=(--full "$ASM_INPUT" --filtered "$ASM_FILTERED" --summary "$ASM_SUMMARY" --profile "$PROFILE")
if [ "$TEST_SYNTHETIC" -eq 1 ]; then
    FILTER_ARGS+=(--test-synthetic-violation)
fi

if ! "$PYTHON_TOOL" "$FILTER" "${FILTER_ARGS[@]}"; then
    echo -e "  ${RED}${BOLD}RT codegen gate FAILED (see summary above).${NC}" >&2
    die "RT codegen verification failed for $TARGET_BIN"
fi

METRICS_JSON="$("$PYTHON_TOOL" - "$PROFILE" "$ASM_SUMMARY" "$TARGET_BIN" <<'PY'
import hashlib
import json
import sys

profile_path, summary_path, binary_path = sys.argv[1:4]
try:
    with open(profile_path, "r", encoding="utf-8") as handle:
        profile = json.load(handle)
except (OSError, json.JSONDecodeError) as exc:
    print(f"[FATAL] cannot read profile for metrics: {exc}", file=sys.stderr)
    raise SystemExit(1)

targets = profile.get("targets") or []
target_ids = [str(t.get("id") or t.get("pattern")) for t in targets]
required_ids = {str(t.get("id") or t.get("pattern")) for t in targets if t.get("required", True)}

counts_by_id: dict[str, int] = {}
totals = {"symbols": 0, "instructions": 0, "vfmadd": 0, "call": 0}
with open(summary_path, "r", encoding="utf-8", errors="replace") as handle:
    for line in handle:
        line = line.rstrip("\n")
        if line.startswith("symbols="):
            for field in line.split():
                key, _, value = field.partition("=")
                if key in totals:
                    try:
                        totals[key] = int(value)
                    except ValueError:
                        pass
            continue
        if line.startswith("symbol |") or " | " not in line:
            continue
        parts = line.split(" | ")
        if len(parts) < 2:
            continue
        target_id = parts[1]
        counts_by_id[target_id] = counts_by_id.get(target_id, 0) + 1

targets_matched = sum(1 for target_id in target_ids if counts_by_id.get(target_id, 0) > 0)
required_matched = sum(1 for target_id in required_ids if counts_by_id.get(target_id, 0) > 0)
with open(binary_path, "rb") as handle:
    digest = hashlib.sha256(handle.read()).hexdigest()

doc = {
    "binary": binary_path,
    "sha256": digest,
    "symbols": int(totals["symbols"]),
    "instructions": int(totals["instructions"]),
    "vfmadd": int(totals["vfmadd"]),
    "call": int(totals["call"]),
    "targets_total": len(target_ids),
    "targets_matched": targets_matched,
    "required_total": len(required_ids),
    "required_matched": required_matched,
    "targets": {target_id: counts_by_id.get(target_id, 0) for target_id in target_ids},
}
print(json.dumps(doc, sort_keys=True))
PY
)"

if [ -n "$JSON_OUT" ]; then
    mkdir -p "$(dirname "$JSON_OUT")"
    printf '%s\n' "$METRICS_JSON" > "$JSON_OUT.tmp"
    mv "$JSON_OUT.tmp" "$JSON_OUT"
fi

METRICS_LINE="$("$PYTHON_TOOL" -c '
import json, sys
doc = json.load(sys.stdin)
print(
    "symbols={symbols} instructions={instructions} vfmadd={vfmadd} "
    "targets_matched={targets_matched} targets_total={targets_total} "
    "required_matched={required_matched} required_total={required_total}".format(**doc)
)
' <<<"$METRICS_JSON")"
echo "RT_CODEGEN_METRICS: $METRICS_LINE"

ok "RT codegen verification passed cleanly."

if [ "$POST_BOLT_REGRESSION" -eq 1 ]; then
    POST_BOLT_SCRIPT="$SCRIPT_DIR/../tests/scripts/test_rt_codegen_post_bolt.sh"
    if [ ! -x "$POST_BOLT_SCRIPT" ]; then
        die "Post-BOLT regression script missing or not executable: $POST_BOLT_SCRIPT (fail-closed)."
    fi
    "$POST_BOLT_SCRIPT"
fi
exit 0
