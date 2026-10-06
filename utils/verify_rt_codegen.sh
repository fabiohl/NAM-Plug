#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Static RT-codegen guard for NAM-Plug.
#
# Statically inspects the compiled CLAP plugin shared library (.so / .clap)
# using `nm` and `objdump` to verify machine code invariants in hot-path
# audio routines (`process`, `process_sub_block`, `process_sub_block_chunked`,
# `process_crossfade_sub_block`, `process_tail_drain`, `drain_tail_into`):
#
# Invariants enforced:
#   1. Zero heap allocations/deallocations (`malloc`, `calloc`, `realloc`, `free`).
#   2. Zero integer division (`div`/`idiv`) in the core DSP loop (`process_sub_block`
#      and callees). The only permitted exception in `process()` is the 1x/block
#      telemetry budget calculation (`last_n_samples * 1e9 / sample_rate`, up to 2 divs).
#   3. Zero thread-local storage accesses (`call __tls_get_addr`) in the core DSP loop.
#      In `process()`, accesses are restricted to cold lifecycle priming / mode-switch
#      branches (up to 5 occurrences).
#
# Fail-closed policy:
#   - Missing binary -> exit 1
#   - Missing tools (`nm`, `objdump`, `python3`) -> exit 1
#   - Empty symbol table / stripped binary -> exit 1
#   - Missing required hot-path symbols (`process`, `process_sub_block`) -> exit 1
#   - Empty disassembly output -> exit 1
#   - Any code invariant violation -> exit 1
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
NM_TOOL="${NM_TOOL:-$(command -v llvm-nm || command -v nm || true)}"
OBJDUMP_TOOL="${OBJDUMP_TOOL:-$(command -v llvm-objdump || command -v objdump || true)}"
PYTHON_TOOL="${PYTHON_BIN:-$(command -v python3 || true)}"

if [ -z "$NM_TOOL" ] || [ ! -x "$NM_TOOL" ]; then
    die "Required symbol inspection tool 'nm' not found or not executable (fail-closed)."
fi
if [ -z "$OBJDUMP_TOOL" ] || [ ! -x "$OBJDUMP_TOOL" ]; then
    die "Required disassembly tool 'objdump' not found or not executable (fail-closed)."
fi
if [ -z "$PYTHON_TOOL" ] || [ ! -x "$PYTHON_TOOL" ]; then
    die "Required script runner 'python3' not found or not executable (fail-closed)."
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
echo -e "  ${BLUE}Tools:${NC} nm=$NM_TOOL, objdump=$OBJDUMP_TOOL, python=$PYTHON_TOOL"

# Execute fail-closed symbol extraction and disassembly scan
export TARGET_BIN NM_TOOL OBJDUMP_TOOL TEST_SYNTHETIC
"$PYTHON_TOOL" - << 'PYEOF'
import sys, os, re, subprocess

bin_path = os.environ["TARGET_BIN"]
nm_tool = os.environ["NM_TOOL"]
objdump_tool = os.environ["OBJDUMP_TOOL"]
test_synthetic = os.environ.get("TEST_SYNTHETIC", "0") == "1"

# 1. Run nm to extract demangled text symbols
try:
    proc_nm = subprocess.run(
        [nm_tool, "--demangle", bin_path],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        check=True
    )
except subprocess.CalledProcessError as e:
    print(f"FATAL: nm failed on '{bin_path}': {e.stderr}", file=sys.stderr)
    sys.exit(1)
except Exception as e:
    print(f"FATAL: failed to execute nm tool '{nm_tool}': {e}", file=sys.stderr)
    sys.exit(1)

text_syms = set()
for line in proc_nm.stdout.splitlines():
    parts = line.strip().split()
    if len(parts) >= 3 and parts[1] in ("t", "T"):
        text_syms.add(" ".join(parts[2:]))

if not text_syms:
    print(f"FATAL: 0 demangled text symbols found in '{bin_path}' (stripped binary?) (fail-closed).", file=sys.stderr)
    sys.exit(1)

# 2. Target definitions and invariant budgets
TARGET_SPECS = [
    {
        "id": "process",
        "pattern": r"^<.*NamClapProcessor as clack_plugin::plugin::PluginAudioProcessor<.*>>::process$",
        "required": True,
        "max_div": 2, # Telemetry block budget: last_n_samples * 1e9 / sample_rate (64/32-bit pair)
        "max_tls": 5, # Cold lifecycle fallback + mode change event branches
        "max_alloc": 0,
    },
    {
        "id": "process_sub_block",
        "pattern": r"^nam_plug::clap::processor::dsp::orchestrator::audio_loop::process_sub_block$",
        "required": True,
        "max_div": 0,
        "max_tls": 0,
        "max_alloc": 0,
    },
    {
        "id": "process_sub_block_chunked",
        "pattern": r"^nam_plug::clap::processor::dsp::orchestrator::audio_loop::process_sub_block_chunked$",
        "required": False,
        "max_div": 0,
        "max_tls": 0,
        "max_alloc": 0,
    },
    {
        "id": "process_crossfade_sub_block",
        "pattern": r"^nam_plug::clap::processor::dsp::orchestrator::audio_loop::process_crossfade_sub_block$",
        "required": False,
        "max_div": 0,
        "max_tls": 0,
        "max_alloc": 0,
    },
    {
        "id": "process_tail_drain",
        "pattern": r"^nam_plug::clap::processor::dsp::orchestrator::audio_loop::process_tail_drain$",
        "required": False,
        "max_div": 0,
        "max_tls": 0,
        "max_alloc": 0,
    },
    {
        "id": "drain_tail_into",
        "pattern": r"^nam_plug::clap::processor::dsp::orchestrator::audio_loop::drain_tail_into$",
        "required": False,
        "max_div": 0,
        "max_tls": 0,
        "max_alloc": 0,
    },
    {
        "id": "process_dry_contained_block",
        # Two-phase reset containment leg (S6-T1): the drained callback's
        # bypass-leg dry passthrough + scheduled-event application, part of
        # the audio callback path — same zero-alloc/TLS/div contract.
        "pattern": r"^<nam_plug::clap::processor::state::NamClapProcessor>::process_dry_contained_block$",
        "required": True,
        "max_div": 2, # Same allowance as `process`: telemetry block budget
                      # last_n_samples * 1e9 / sample_rate (64/32-bit pair),
                      # inlined from `process_telemetry`.
        "max_tls": 0,
        "max_alloc": 0,
    },
]

resolved_targets = {}
for spec in TARGET_SPECS:
    prog = re.compile(spec["pattern"])
    matches = [s for s in text_syms if prog.match(s)]
    if not matches and spec["required"]:
        print(f"FATAL: Required hot-path target '{spec['id']}' not found in binary symbol table (fail-closed).", file=sys.stderr)
        sys.exit(1)
    for m in matches:
        resolved_targets[m] = spec

if not resolved_targets:
    print(f"FATAL: No hot-path symbols resolved in '{bin_path}' (fail-closed).", file=sys.stderr)
    sys.exit(1)

# 3. Disassemble with objdump
try:
    cmd_obj = [objdump_tool, "-d", "--demangle", "--no-show-raw-insn", bin_path]
    proc_obj = subprocess.Popen(cmd_obj, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
except Exception as e:
    print(f"FATAL: failed to launch objdump tool '{objdump_tool}': {e}", file=sys.stderr)
    sys.exit(1)

func_re = re.compile(r"^[0-9a-fA-F]+\s+<(.*)>:\s*$")
current_sym = None
insn_lines = {sym: [] for sym in resolved_targets}

for line in proc_obj.stdout:
    m = func_re.match(line)
    if m:
        sym = m.group(1)
        if sym in resolved_targets:
            current_sym = sym
        else:
            current_sym = None
    elif current_sym:
        trimmed = line.strip()
        if trimmed and not trimmed.startswith("..."):
            insn_lines[current_sym].append(trimmed)

proc_obj.wait()
if proc_obj.returncode != 0:
    err_out = proc_obj.stderr.read()
    print(f"FATAL: objdump failed with exit code {proc_obj.returncode}: {err_out}", file=sys.stderr)
    sys.exit(1)

total_insns = sum(len(lines) for lines in insn_lines.values())
if total_insns == 0:
    print("FATAL: Disassembly produced 0 instructions for target symbols (fail-closed).", file=sys.stderr)
    sys.exit(1)

# 4. Check invariants
re_alloc = re.compile(r"\bcall[q]?\s+.*(malloc|calloc|realloc|free)(@plt|\b)")
re_tls = re.compile(r"\bcall[q]?\s+.*__tls_get_addr")
re_div = re.compile(r"\b(i?div[bwdlq]?)\b")

violations = []

if test_synthetic:
    violations.append("process_sub_block: synthetic violation injected for fail-closed validation")

for sym, lines in insn_lines.items():
    spec = resolved_targets[sym]
    if len(lines) == 0 and spec["required"]:
        violations.append(f"{spec['id']}: 0 instructions disassembled (fail-closed)")
        continue
        
    allocs = [l for l in lines if re_alloc.search(l)]
    tls_calls = [l for l in lines if re_tls.search(l)]
    divs = [l for l in lines if re_div.search(l)]
    
    print(f"    - {spec['id']}: {len(lines)} insns | allocs={len(allocs)} (max {spec['max_alloc']}) | tls={len(tls_calls)} (max {spec['max_tls']}) | divs={len(divs)} (max {spec['max_div']})")
    
    if len(allocs) > spec["max_alloc"]:
        violations.append(f"{spec['id']}: forbidden heap allocs ({len(allocs)} > {spec['max_alloc']}): {allocs[:5]}")
    if len(tls_calls) > spec["max_tls"]:
        violations.append(f"{spec['id']}: excess TLS calls ({len(tls_calls)} > {spec['max_tls']}): {tls_calls[:5]}")
    if len(divs) > spec["max_div"]:
        violations.append(f"{spec['id']}: excess div/idiv instructions ({len(divs)} > {spec['max_div']}): {divs[:5]}")

if violations:
    print(f"\nFATAL: Real-time codegen violations detected in {bin_path}:", file=sys.stderr)
    for v in violations:
        print(f"  ❌ {v}", file=sys.stderr)
    sys.exit(1)

print(f"\n  ✓ RT codegen verified: {len(resolved_targets)} functions, {total_insns} insns, 0 heap allocs, 0 illegal divs, 0 illegal TLS calls.")
PYEOF

ok "RT codegen verification passed cleanly."
exit 0
