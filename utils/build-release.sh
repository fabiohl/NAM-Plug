#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Unified compiler-grade release build & packaging script for NAM-Plug (PGO + BOLT + Flatpak).
# Compiles the CLAP plugin with Profile-Guided Optimization (PGO),
# post-link BOLT binary reordering, and generates release distribution archives
# and standalone Flatpak plugin extensions with strict cryptographic receipts.
#
# Deliverables:
#   - ~/.clap/nam_plug.clap                      (PGO + BOLT optimized CLAP plugin)
#   - target/release-receipt.json                (Cryptographic build provenance receipt)
#   - target/dsp_hotpath.asm                     (Disassembly hotspot report)
#   - ~/nam-plug-v<ver>-linux-x86_64-v3.tar.zst  (Release distribution tarball)
#   - ~/nam-plug-v<ver>-linux-x86_64-v3.flatpak  (Flatpak plugin extension bundle)

set -euo pipefail

# Parse command line options
DO_INSTALL_FLATPAK=false
BUILD_FLATPAK=true
BUILD_TARBALL=true
USE_PGO=true
USE_BOLT=true
RELEASE_CEREMONY=false

show_help() {
    cat <<EOF
Usage: $(basename "$0") [OPTIONS]

Unified compiler-grade release build and packaging pipeline for NAM-Plug.

Options:
  --install              Automatically install the Flatpak bundle locally (flatpak install --user)
                         in addition to installing ~/.clap/nam_plug.clap.
  --no-flatpak           Skip Phase 7 (Flatpak bundle creation).
  --no-tarball           Skip Phase 6 (.tar.zst archive creation).
  --no-pgo               Skip Profile-Guided Optimization and compile directly with dist profile.
  --no-bolt              Skip Phase 4 (LLVM BOLT post-link optimization).
  --no-strict            Disable strict fail-closed mode on optional tool absence.
  --strict               Enforce strict fail-closed mode (default: 1).
  --release-ceremony     Official release ceremony mode: requires a pristine worktree & strict tests.
  -h, --help             Show this help message and exit.

Deliverables:
  - ~/.clap/nam_plug.clap                      (Installed CLAP plugin)
  - target/release-receipt.json                (Cryptographic build receipt)
  - target/dsp_hotpath.asm                     (Disassembly hotspot report)
  - ~/nam-plug-v<ver>-linux-x86_64-v3.tar.zst  (Distribution tarball)
  - ~/nam-plug-v<ver>-linux-x86_64-v3.flatpak  (Flatpak plugin extension bundle)
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --install)
            DO_INSTALL_FLATPAK=true
            shift
            ;;
        --no-flatpak)
            BUILD_FLATPAK=false
            shift
            ;;
        --no-tarball)
            BUILD_TARBALL=false
            shift
            ;;
        --no-pgo)
            USE_PGO=false
            shift
            ;;
        --no-bolt)
            USE_BOLT=false
            shift
            ;;
        --strict)
            STRICT_MODE=1
            shift
            ;;
        --no-strict)
            STRICT_MODE=0
            shift
            ;;
        --release-ceremony)
            RELEASE_CEREMONY=true
            STRICT_MODE=1
            shift
            ;;
        -h|--help)
            show_help
            exit 0
            ;;
        *)
            echo "Unknown option: $1" >&2
            show_help
            exit 1
            ;;
    esac
done

# Import shared style helpers and utilities from _lib.sh.
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
NAM_LIB_NO_CD=1 source "$SCRIPT_DIR/lib/_lib.sh"

echo -e "${BLUE}${BOLD}========================================================================${NC}"
echo -e "${BLUE}${BOLD}   NAM-Plug Unified Release Build & Optimization Pipeline               ${NC}"
echo -e "${BLUE}${BOLD}========================================================================${NC}"

# Ensure execution from the subproject root directory
cd "$PROJECT_DIR"

# Engine provenance variables
CORE_PATH=""
CORE_VERSION="unknown"
CORE_GIT_COMMIT="unknown"
CORE_GIT_BRANCH="unknown"
CORE_GIT_DIRTY=false
CORE_TREE_SHA=""

# Provenance check: Inspect DSP engine crate NeuralAmpModeler-rs
check_engine_provenance() {
    if [ -d "../NeuralAmpModeler-rs" ] && grep -q 'NeuralAmpModeler-rs.*path.*=.*"\.\./NeuralAmpModeler-rs"' Cargo.toml 2>/dev/null; then
        CORE_PATH="../NeuralAmpModeler-rs"
        if [ -f "$CORE_PATH/Cargo.toml" ]; then
            CORE_VERSION=$(grep '^version\s*=' "$CORE_PATH/Cargo.toml" | head -n 1 | cut -d'"' -f2 || echo "unknown")
        fi
        if git -C "$CORE_PATH" rev-parse --is-inside-work-tree &>/dev/null; then
            CORE_GIT_COMMIT=$(git -C "$CORE_PATH" rev-parse HEAD 2>/dev/null || echo "unknown")
            CORE_GIT_BRANCH=$(git -C "$CORE_PATH" rev-parse --abbrev-ref HEAD 2>/dev/null || echo "unknown")
            if [ -n "$(git -C "$CORE_PATH" status --porcelain 2>/dev/null || true)" ]; then
                CORE_GIT_DIRTY=true
            fi
            CORE_TREE_SHA=$(git -C "$CORE_PATH" rev-parse HEAD^{tree} 2>/dev/null || echo "")
        fi

        if [ "$CORE_GIT_DIRTY" = "true" ]; then
            if [ "$STRICT_MODE" = "1" ]; then
                die "Engine repository '$CORE_PATH' has uncommitted changes in strict release mode (provenance fail-closed). Commit, stash or clean before generating a certified release."
            else
                warn "Engine repository '$CORE_PATH' is dirty (non-strict mode)."
            fi
        else
            echo -e "  ${GREEN}✓${NC} Engine ($CORE_PATH) provenance verified clean (commit: ${CORE_GIT_COMMIT:0:12}, branch: $CORE_GIT_BRANCH, version: $CORE_VERSION)."
        fi
    fi
}

# Provenance fail-closed: a certified release can never be produced from a
# dirty work tree. In strict mode this aborts before any heavy work is started
# and is re-verified immediately before receipt generation (Phase 8), so no
# receipt is ever written for a dirty tree.
check_git_clean_strict() {
    if [ "$STRICT_MODE" != "1" ]; then
        return 0
    fi
    if ! git rev-parse --is-inside-work-tree &>/dev/null; then
        die "Not inside a git work tree in strict release mode; provenance cannot be certified. Run the release from a clean git checkout."
    fi
    if [ -n "$(git status --porcelain 2>/dev/null || true)" ]; then
        die "Git working tree is dirty in strict release mode (provenance fail-closed). Commit, stash or clean before generating a certified release; no receipt will be written."
    fi
    check_engine_provenance
}
check_git_clean_strict

# State tracking for signal safety and cleanup
ORIG_PARANOID=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo "2")
PARANOID_MODIFIED=false
WORKLOAD_PID=""
declare -a GATE_SKIPS=()

# Dynamic isolated temporary directories for PGO & BOLT profiling and packaging
PGO_DIR="$(mktemp -d -t nam-plug-pgo.XXXXXX)"
BOLT_DIR="$(mktemp -d -t nam-plug-bolt.XXXXXX)"
PKG_DIR=""
FLATPAK_BUILD_DIR=""
FLATPAK_REPO_DIR=""
PROFRAW_DIR="$PGO_DIR/profraw"
MERGED_PROFILE="$PGO_DIR/merged.profdata"

# Provenance fail-closed: external RUSTFLAGS is rejected wholesale in
# strict release mode. The certified pipeline uses exclusively the
# CONFIG_RUSTFLAGS extracted from .cargo/config.toml below; any environment
# flag (not just -Ctarget-cpu=) could otherwise alter the shipped bytes
# without being recorded as provenance.
RAW_RUSTFLAGS="${RUSTFLAGS:-}"
if [ -n "${CARGO_ENCODED_RUSTFLAGS:-}" ]; then
    RAW_RUSTFLAGS="${RAW_RUSTFLAGS:+$RAW_RUSTFLAGS }$CARGO_ENCODED_RUSTFLAGS"
fi
ORIG_RUSTFLAGS=""
if [ -n "$RAW_RUSTFLAGS" ]; then
    if [ "$STRICT_MODE" = "1" ]; then
        die "External RUSTFLAGS/CARGO_ENCODED_RUSTFLAGS detected in strict release mode: '$RAW_RUSTFLAGS'. Certified builds use exclusively CONFIG_RUSTFLAGS from .cargo/config.toml. Unset them or run with NAM_STRICT_RELEASE=0."
    fi
    warn "External RUSTFLAGS present in non-strict mode: '$RAW_RUSTFLAGS'"
    CLEAN_RUSTFLAGS=""
    for flag in $RAW_RUSTFLAGS; do
        if [[ "$flag" =~ ^-Ctarget-cpu= ]]; then
            warn "Ignoring unvetted environment flag: $flag (enforcing controlled x86-64-v3 baseline)"
        else
            warn "Allowing unvetted environment flag (non-strict): $flag"
            CLEAN_RUSTFLAGS="$CLEAN_RUSTFLAGS $flag"
        fi
    done
    ORIG_RUSTFLAGS="$(echo "$CLEAN_RUSTFLAGS" | xargs)"
fi

# Isolated target directories to avoid polluting standard compilations
PGO_BUILD_TARGET_DIR="$PROJECT_DIR/target/pgo-build"
PGO_CLAP_TARGET_DIR="$PROJECT_DIR/target/pgo-clap"

# Clean prior target artifacts and initialize temporary directories
rm -rf "$PGO_BUILD_TARGET_DIR" "$PGO_CLAP_TARGET_DIR"
mkdir -p "$PROFRAW_DIR" "$BOLT_DIR" "$PROJECT_DIR/target"

# Signal handling and process/temporary file cleanup
cleanup() {
    if [ -n "${WORKLOAD_PID:-}" ] && kill -0 "$WORKLOAD_PID" 2>/dev/null; then
        kill "$WORKLOAD_PID" 2>/dev/null || true
        wait "$WORKLOAD_PID" 2>/dev/null || true
    fi
    if [ "${PARANOID_MODIFIED:-false}" = "true" ]; then
        echo -e "\nRestoring kernel.perf_event_paranoid to $ORIG_PARANOID..."
        sudo sysctl -q -w kernel.perf_event_paranoid="$ORIG_PARANOID" 2>/dev/null || true
    fi
    if [ -n "${PGO_DIR:-}" ] && [ -d "$PGO_DIR" ]; then rm -rf "$PGO_DIR"; fi
    if [ -n "${BOLT_DIR:-}" ] && [ -d "$BOLT_DIR" ]; then rm -rf "$BOLT_DIR"; fi
    if [ -n "${PKG_DIR:-}" ] && [ -d "$PKG_DIR" ]; then rm -rf "$PKG_DIR"; fi
    if [ -n "${FLATPAK_BUILD_DIR:-}" ] && [ -d "$FLATPAK_BUILD_DIR" ]; then rm -rf "$FLATPAK_BUILD_DIR"; fi
    if [ -n "${FLATPAK_REPO_DIR:-}" ] && [ -d "$FLATPAK_REPO_DIR" ]; then rm -rf "$FLATPAK_REPO_DIR"; fi
    return 0
}
trap cleanup EXIT INT TERM HUP

export CARGO_TARGET_DIR="$PGO_BUILD_TARGET_DIR"

# Disable symbol stripping during release compilation so BOLT can reorder symbols
export CARGO_PROFILE_DIST_STRIP="false"
export CARGO_PROFILE_BENCH_STRIP="false"

# Extract rustflags from .cargo/config.toml using tomllib (or regex fallback)
CONFIG_RUSTFLAGS=$(python3 -c '
import sys
try:
    import tomllib
    with open(".cargo/config.toml", "rb") as f:
        data = tomllib.load(f)
    flags = data.get("build", {}).get("rustflags", [])
    if isinstance(flags, list) and flags:
        print(" ".join(flags))
        sys.exit(0)
except Exception:
    pass

import re
try:
    with open(".cargo/config.toml", "r") as f:
        content = f.read()
    match = re.search(r"rustflags\s*=\s*\[(.*?)\n\]", content, re.DOTALL)
    if match:
        block = match.group(1)
        flags = []
        for line in block.splitlines():
            stripped = line.strip()
            if not stripped or stripped.startswith("#"):
                continue
            flag_match = re.search(r"\"([^\"]+)\"", stripped)
            if flag_match:
                flags.append(flag_match.group(1))
        print(" ".join(flags))
except Exception:
    pass
' 2>/dev/null || echo "")

# Deliverable Targets
CLAP_INSTALL_DIR="$HOME/.clap"
CLAP_TARGET="$CLAP_INSTALL_DIR/nam_plug.clap"

# -----------------------------------------------------------------------------
# PHASE 1: Environment & Dependency Verification
# -----------------------------------------------------------------------------
echo -e "\n${BLUE}${BOLD}[Phase 1/7] Verifying dependencies and environment...${NC}"

# Verify core dependencies
REQUIRED_CMDS=(rustc cargo python3 tar zstd)
if [ "$BUILD_FLATPAK" = true ]; then
    REQUIRED_CMDS+=(flatpak)
fi

for cmd in "${REQUIRED_CMDS[@]}"; do
    if ! command -v "$cmd" &>/dev/null; then
        echo -e "${RED}Error: '$cmd' is not installed or available in PATH.${NC}"
        exit 1
    fi
    echo -e "  ${GREEN}✓${NC} '$cmd' found."
done

# Ensure non-empty rustflags were extracted from .cargo/config.toml
if [ -z "${CONFIG_RUSTFLAGS:-}" ]; then
    echo -e "${RED}Error: Could not extract rustflags from .cargo/config.toml or they are empty!${NC}"
    echo -e "${YELLOW}The release build requires optimizations like '-Ctarget-cpu=x86-64-v3'.${NC}"
    exit 1
fi
echo -e "  ${GREEN}✓${NC} rustflags from config.toml verified: ${BOLD}$CONFIG_RUSTFLAGS${NC}"

# Locate llvm-profdata from Rustup toolchain (if PGO is active)
LLVM_PROFDATA=""
if [ "$USE_PGO" = true ]; then
    RUST_SYSROOT="$(rustc --print sysroot)"
    RUST_TARGET="$(rustc -vV | sed -n 's/^host: //p')"
    LLVM_PROFDATA="$RUST_SYSROOT/lib/rustlib/$RUST_TARGET/bin/llvm-profdata"
    if [ ! -x "$LLVM_PROFDATA" ]; then
        echo -e "${RED}Error: llvm-profdata not found at $LLVM_PROFDATA${NC}"
        echo -e "${YELLOW}Install LLVM tools via rustup:${NC}"
        echo -e "  rustup component add llvm-tools-preview"
        exit 1
    fi
    echo -e "  ${GREEN}✓${NC} llvm-profdata found: $LLVM_PROFDATA"
fi

# Locate LLVM BOLT binary and merge-fdata tool (if BOLT is active)
LLVM_BOLT=""
MERGE_FDATA=""
if [ "$USE_BOLT" = true ]; then
    for candidate in \
        /usr/lib/llvm-22/bin/llvm-bolt \
        /usr/lib/llvm-21/bin/llvm-bolt \
        /usr/lib/llvm-20/bin/llvm-bolt \
        /usr/lib/llvm-19/bin/llvm-bolt \
        /usr/lib/llvm-18/bin/llvm-bolt \
        /usr/bin/llvm-bolt-22 \
        /usr/bin/llvm-bolt-21 \
        /usr/bin/llvm-bolt; do
        if [ -x "$candidate" ]; then
            LLVM_BOLT="$candidate"
            break
        fi
    done

    if [ -n "$LLVM_BOLT" ]; then
        echo -e "  ${GREEN}✓${NC} llvm-bolt found: $LLVM_BOLT"
        MERGE_FDATA="$(dirname "$LLVM_BOLT")/merge-fdata"
        if [ ! -x "$MERGE_FDATA" ]; then
            MERGE_FDATA="merge-fdata"
        fi
    else
        if [ "$STRICT_MODE" = "1" ]; then
            echo -e "${RED}Error: llvm-bolt was not found in strict release mode.${NC}"
            echo -e "${YELLOW}To enable BOLT, install: sudo apt install llvm-22-tools (or run with --no-bolt / --no-strict).${NC}"
            exit 1
        fi
        echo -e "${YELLOW}Warning: llvm-bolt was not found. The build will continue with PGO only.${NC}"
    fi

    # Check perf_event_paranoid requirement for BOLT profiling
    if [ -n "$LLVM_BOLT" ] && [ "$ORIG_PARANOID" -gt 1 ]; then
        echo -e "  kernel.perf_event_paranoid is $ORIG_PARANOID. Attempting to set to 1..."
        if command -v sudo &>/dev/null; then
            if sudo -n sysctl -w kernel.perf_event_paranoid=1 &>/dev/null; then
                sudo sysctl -w kernel.perf_event_paranoid=1
                PARANOID_MODIFIED=true
                echo -e "  ${GREEN}✓${NC} paranoid level set to 1."
            else
                echo -e "${YELLOW}Warning: Passwordless sudo not available. Trying interactive sudo...${NC}"
                if [ -t 0 ]; then
                    if sudo sysctl -w kernel.perf_event_paranoid=1; then
                        PARANOID_MODIFIED=true
                        echo -e "  ${GREEN}✓${NC} paranoid level set to 1."
                    else
                        echo -e "${YELLOW}Warning: Failed to set paranoid level to 1. BOLT profiling might be skipped.${NC}"
                    fi
                else
                    echo -e "${YELLOW}Warning: Non-interactive shell, cannot prompt for sudo password. BOLT profiling might be skipped.${NC}"
                fi
            fi
        else
            echo -e "${YELLOW}Warning: 'sudo' command not found. BOLT profiling might be skipped.${NC}"
        fi
    fi
fi

# -----------------------------------------------------------------------------
# PHASE 1.5: Preflight Quick QA & RT Heap Allocation Audit
# -----------------------------------------------------------------------------
echo -e "\n${BLUE}${BOLD}[Phase 1.5/7] Executing strict Quick QA & RT Heap Allocation Audit preflight...${NC}"

QUICK_RECEIPT="$PROJECT_DIR/target/logs/quick-receipt.txt"
HEAP_LOG="$PROJECT_DIR/target/logs/quick-heap-audit.log"

if [ "${NAM_SKIP_QUICK_QA:-0}" != "1" ]; then
    NAM_QUICK_STRICT=1 "$SCRIPT_DIR/tests-quick.sh" || {
        if [ "$STRICT_MODE" = "1" ]; then
            die "Quick QA preflight suite failed in strict release mode!"
        fi
        warn "Quick QA preflight suite failed (non-strict mode)."
        GATE_SKIPS+=("quick_qa:failed")
    }
fi

QUICK_QA_PASSED=false
HEAP_AUDIT_PASSED=false
QUICK_RECEIPT_SHA=""
HEAP_AUDIT_LOG_SHA=""

if [ -f "$QUICK_RECEIPT" ] && grep -q "OVERALL: PASSED" "$QUICK_RECEIPT"; then
    QUICK_QA_PASSED=true
    QUICK_RECEIPT_SHA=$(sha256sum "$QUICK_RECEIPT" | cut -d' ' -f1)
fi

if [ -f "$HEAP_LOG" ] && [ -f "$QUICK_RECEIPT" ] && grep -q "HEAP_AUDIT=RAN" "$QUICK_RECEIPT"; then
    HEAP_AUDIT_PASSED=true
    HEAP_AUDIT_LOG_SHA=$(sha256sum "$HEAP_LOG" | cut -d' ' -f1)
fi

if [ "$QUICK_QA_PASSED" = true ] && [ "$HEAP_AUDIT_PASSED" = true ]; then
    echo -e "  ${GREEN}✓${NC} Quick QA preflight and zero-alloc heap audit verified."
else
    if [ "$STRICT_MODE" = "1" ]; then
        die "Quick QA preflight did not pass or heap audit was not verified in strict release mode!"
    fi
    warn "Quick QA preflight or heap audit not fully verified."
    GATE_SKIPS+=("quick_qa:unverified")
fi

# -----------------------------------------------------------------------------
# PHASE 2: Profile-Guided Optimization (PGO) - Profiling Workload
# -----------------------------------------------------------------------------
if [ "$USE_PGO" = true ]; then
    echo -e "\n${BLUE}${BOLD}[Phase 2/7] Generating PGO profiles via workload runner...${NC}"

    export RUSTFLAGS="$CONFIG_RUSTFLAGS $ORIG_RUSTFLAGS -Cprofile-generate=$PROFRAW_DIR"
    export LLVM_PROFILE_FILE="$PROFRAW_DIR/default_%m_%p.profraw"
    echo -e "  Using RUSTFLAGS: ${BOLD}$RUSTFLAGS${NC}"

    echo -e "  Compiling real-world PGO profiling workload (pgo_profiling_workload)..."
    cargo build --locked --profile dist --features testing --bin pgo_profiling_workload || {
        echo -e "${RED}Error: Failed to build pgo_profiling_workload for PGO profiling.${NC}"
        exit 1
    }

    echo -e "  Executing PGO profiling workload..."
    timeout 60 "$PGO_BUILD_TARGET_DIR/dist/pgo_profiling_workload" || {
        echo -e "${RED}Error: pgo_profiling_workload failed (or timed out after 60s). Cannot generate PGO profiles.${NC}"
        exit 1
    }

    PROFRAW_COUNT=$(find "$PROFRAW_DIR" -name "*.profraw" 2>/dev/null | wc -l)
    if [ "$PROFRAW_COUNT" -eq 0 ]; then
        echo -e "${RED}Error: No .profraw profile files were generated in $PROFRAW_DIR!${NC}"
        echo -e "${RED}PGO profiling failed — check that pgo_profiling_workload exercised the DSP pipeline.${NC}"
        exit 1
    fi

    echo -e "  ${GREEN}✓${NC} Collected $PROFRAW_COUNT .profraw profiles. Merging..."
    "$LLVM_PROFDATA" merge -sparse -o "$MERGED_PROFILE" "$PROFRAW_DIR"/*.profraw
    echo -e "  ${GREEN}✓${NC} Merged profile generated at: $MERGED_PROFILE ($(du -h "$MERGED_PROFILE" | cut -f1))"

    # Clean raw profiles after merging
    rm -rf "$PROFRAW_DIR"
else
    echo -e "\n${YELLOW}[Phase 2/7] Skipping PGO trace generation (--no-pgo).${NC}"
fi

# -----------------------------------------------------------------------------
# PHASE 3: Compile Optimized CLAP Plugin
# -----------------------------------------------------------------------------
echo -e "\n${BLUE}${BOLD}[Phase 3/7] Compiling optimized CLAP plugin...${NC}"

if [ "$USE_PGO" = true ] && [ -f "$MERGED_PROFILE" ]; then
    export RUSTFLAGS="$CONFIG_RUSTFLAGS $ORIG_RUSTFLAGS -Cprofile-use=$MERGED_PROFILE"
else
    export RUSTFLAGS="$CONFIG_RUSTFLAGS $ORIG_RUSTFLAGS"
    echo -e "  ${YELLOW}Compiling without PGO profile.${NC}"
fi

CLAP_RUSTFLAGS="$RUSTFLAGS -Clink-arg=-Wl,-q -Clink-arg=-Wl,-soname,nam_plug.clap"
echo -e "  Using RUSTFLAGS (CLAP): ${BOLD}$CLAP_RUSTFLAGS${NC}"
RUSTFLAGS="$CLAP_RUSTFLAGS" cargo build --locked --profile dist --target-dir "$PGO_CLAP_TARGET_DIR" --lib

# Confirm binary compiled
if [ ! -f "$PGO_CLAP_TARGET_DIR/dist/libnam_plug.so" ]; then
    echo -e "${RED}Error: Failed to find compiled CLAP plugin library at $PGO_CLAP_TARGET_DIR/dist/libnam_plug.so${NC}"
    exit 1
fi
echo -e "  ${GREEN}✓${NC} Compilation completed successfully."

# -----------------------------------------------------------------------------
# PHASE 4: BOLT Instrumentation & Post-Link Optimization
# -----------------------------------------------------------------------------
CLAP_BOLT_APPLIED=false
BOLT_PROFILE_SHA=""
BOLT_VERSION=""

if [ "$USE_BOLT" = true ] && [ -n "$LLVM_BOLT" ]; then
    echo -e "\n${BLUE}${BOLD}[Phase 4/7] Applying BOLT post-link optimization...${NC}"

    # Step 1: Instrument CLAP binary
    echo -e "  [Step 1/3] Instrumenting CLAP plugin library with llvm-bolt..."
    if "$LLVM_BOLT" \
        "$PGO_CLAP_TARGET_DIR/dist/libnam_plug.so" \
        -instrument \
        -o "$PGO_CLAP_TARGET_DIR/dist/libnam_plug.instrumented.so" \
        --instrumentation-file="$PGO_CLAP_TARGET_DIR/libnam_plug.fdata" \
        --instrumentation-file-append-pid > "$BOLT_DIR/bolt-instrument-clap.log" 2>&1; then
        echo -e "  ${GREEN}✓${NC} CLAP instrumented: $PGO_CLAP_TARGET_DIR/dist/libnam_plug.instrumented.so"
        BOLT_VERSION=$("$LLVM_BOLT" --version 2>&1 | head -n 1 || echo "unknown")
    else
        if [ "$STRICT_MODE" = "1" ]; then
            die "CLAP instrumentation failed in strict release mode."
        fi
        echo -e "${YELLOW}  Warning: CLAP instrumentation failed. Falling back to PGO-only build.${NC}"
        if [ -f "$BOLT_DIR/bolt-instrument-clap.log" ]; then
            echo -e "${YELLOW}  --- bolt-instrument log tail ---${NC}"
            tail -n 10 "$BOLT_DIR/bolt-instrument-clap.log"
        fi
    fi

    # Step 2: Collect Instrumentation Profiles
    if [ -f "$PGO_CLAP_TARGET_DIR/dist/libnam_plug.instrumented.so" ]; then
        echo -e "  [Step 2/3] Collecting BOLT instrumentation profiles via workload runner..."

        # Recompile pgo_profiling_workload without PGO instrumentation for clean BOLT profiling
        RUSTFLAGS="$CONFIG_RUSTFLAGS $ORIG_RUSTFLAGS" \
            cargo build --locked --profile dist --features testing --bin pgo_profiling_workload

        # Clean any stale fdata files and previous workload receipt before profiling
        rm -f "$PGO_CLAP_TARGET_DIR"/libnam_plug.fdata.* "$BOLT_DIR/libnam_plug.merged.fdata" "$PROJECT_DIR/target/pgo-workload-receipt.json"

        if NAM_CLAP_SO_PATH="$PGO_CLAP_TARGET_DIR/dist/libnam_plug.instrumented.so" \
            "$PGO_BUILD_TARGET_DIR/dist/pgo_profiling_workload"; then
            
            # Verify workload completion receipt
            if [ -f "$PROJECT_DIR/target/pgo-workload-receipt.json" ] && grep -q '"status": "SUCCESS"' "$PROJECT_DIR/target/pgo-workload-receipt.json"; then
                echo -e "  ${GREEN}✓${NC} CLAP profile collected and workload receipt verified."
            else
                echo -e "${RED}  Error: CLAP profiling workload did not emit valid SUCCESS receipt.${NC}"
                rm -f "$PGO_CLAP_TARGET_DIR"/libnam_plug.fdata.*
                if [ "$STRICT_MODE" = "1" ]; then
                    die "CLAP BOLT profiling workload receipt verification failed in strict release mode."
                fi
            fi
        else
            echo -e "${RED}  Error: CLAP profiling workload failed during BOLT profile generation.${NC}"
            # Clean partial fdata to prevent invalid partial optimization
            rm -f "$PGO_CLAP_TARGET_DIR"/libnam_plug.fdata.*
            if [ "$STRICT_MODE" = "1" ]; then
                die "CLAP BOLT profiling workload failed in strict release mode."
            fi
            echo -e "${YELLOW}  Warning: Proceeding without BOLT optimization (partial fdata purged).${NC}"
        fi

        # Merge fdata profiles
        if command -v "$MERGE_FDATA" &>/dev/null || [ -x "$MERGE_FDATA" ]; then
            echo -e "  Merging BOLT instrumentation profiles..."
            CLAP_FDATA_FILES=()
            while IFS= read -r -d '' f; do
                if [ -s "$f" ]; then
                    CLAP_FDATA_FILES+=("$f")
                fi
            done < <(find "$PGO_CLAP_TARGET_DIR" -maxdepth 1 -name "libnam_plug.fdata.*" -print0 2>/dev/null || true)

            if [ ${#CLAP_FDATA_FILES[@]} -gt 0 ]; then
                "$MERGE_FDATA" "${CLAP_FDATA_FILES[@]}" > "$BOLT_DIR/libnam_plug.merged.fdata" 2>"$BOLT_DIR/merge-fdata-clap.log"
                if [ -s "$BOLT_DIR/libnam_plug.merged.fdata" ]; then
                    BOLT_PROFILE_SHA=$(sha256sum "$BOLT_DIR/libnam_plug.merged.fdata" | cut -d' ' -f1)
                    echo -e "  ${GREEN}✓${NC} CLAP profiles merged (${#CLAP_FDATA_FILES[@]} files, sha256: ${BOLT_PROFILE_SHA:0:16}...)"
                else
                    echo -e "${YELLOW}  Warning: CLAP profile merge produced empty output.${NC}"
                    rm -f "$BOLT_DIR/libnam_plug.merged.fdata"
                    if [ "$STRICT_MODE" = "1" ]; then
                        die "CLAP profile merge produced empty output in strict release mode."
                    fi
                fi
            else
                echo -e "${YELLOW}  Warning: No non-empty CLAP fdata profiles found.${NC}"
                if [ "$STRICT_MODE" = "1" ]; then
                    die "No CLAP fdata profiles generated by workload in strict release mode."
                fi
            fi
        else
            echo -e "${YELLOW}  Warning: merge-fdata tool not available. Skipping profile merge.${NC}"
            if [ "$STRICT_MODE" = "1" ]; then
                die "merge-fdata tool not available in strict release mode."
            fi
        fi
    fi

    # Step 3: Apply BOLT Optimization
    if [ -f "$BOLT_DIR/libnam_plug.merged.fdata" ] && [ -s "$BOLT_DIR/libnam_plug.merged.fdata" ]; then
        echo -e "  [Step 3/3] Applying BOLT optimization using merged fdata..."
        if "$LLVM_BOLT" "$PGO_CLAP_TARGET_DIR/dist/libnam_plug.so" \
            -o "$PGO_CLAP_TARGET_DIR/dist/libnam_plug.bolt.so" \
            -data "$BOLT_DIR/libnam_plug.merged.fdata" \
            --reorder-blocks=ext-tsp \
            --reorder-functions=hfsort \
            --relocs \
            --lite > "$BOLT_DIR/llvm-bolt-clap.log" 2>&1; then
            CLAP_BOLT_APPLIED=true
            echo -e "  ${GREEN}✓${NC} BOLT optimization applied successfully to CLAP plugin."
        else
            if [ "$STRICT_MODE" = "1" ]; then
                die "BOLT optimization failed in strict release mode."
            fi
            echo -e "${YELLOW}  Warning: BOLT optimization failed for CLAP plugin. Falling back to PGO-only build.${NC}"
            if [ -f "$BOLT_DIR/llvm-bolt-clap.log" ]; then
                echo -e "${YELLOW}  --- llvm-bolt log tail ---${NC}"
                tail -n 10 "$BOLT_DIR/llvm-bolt-clap.log"
            fi
        fi
    else
        if [ "$STRICT_MODE" = "1" ]; then
            die "No merged fdata profile available for CLAP in strict release mode."
        fi
        echo -e "${YELLOW}  Warning: No merged fdata profile available for CLAP. Skipping BOLT optimization.${NC}"
    fi
else
    echo -e "\n${YELLOW}[Phase 4/7] Skipping BOLT optimization.${NC}"
fi

# -----------------------------------------------------------------------------
# PHASE 4.5: Assembly Hotspot Disassembly Report
# -----------------------------------------------------------------------------
echo -e "\n${BLUE}${BOLD}[Phase 4.5/7] Generating AI-ready assembly hotspot report...${NC}"

ASM_TARGET="$PROJECT_DIR/target/dsp_hotpath.asm"
mkdir -p "$PROJECT_DIR/target"

if [ "${CLAP_BOLT_APPLIED:-false}" = true ] && [ -f "$PGO_CLAP_TARGET_DIR/dist/libnam_plug.bolt.so" ]; then
    ASM_BIN="$PGO_CLAP_TARGET_DIR/dist/libnam_plug.bolt.so"
elif [ -f "$PGO_CLAP_TARGET_DIR/dist/libnam_plug.so" ]; then
    ASM_BIN="$PGO_CLAP_TARGET_DIR/dist/libnam_plug.so"
fi

if [ -n "${ASM_BIN:-}" ]; then
    if command -v llvm-objdump &>/dev/null; then
        llvm-objdump -d --demangle --no-show-raw-insn "$ASM_BIN" > "$ASM_TARGET" 2>/dev/null || true
    elif command -v objdump &>/dev/null; then
        objdump -d --demangle --no-show-raw-insn "$ASM_BIN" > "$ASM_TARGET" 2>/dev/null || true
    fi

    if [ -s "$ASM_TARGET" ]; then
        echo -e "  ${GREEN}✓${NC} Assembly report generated at target/dsp_hotpath.asm ($(wc -l < "$ASM_TARGET") lines)"
    else
        echo -e "  ${YELLOW}Warning: Assembly disassembly failed or produced empty output.${NC}"
    fi
else
    echo -e "  ${YELLOW}Warning: No optimized binary found for disassembly.${NC}"
fi

# -----------------------------------------------------------------------------
# PHASE 5: Deliverables Installation & Strict Certification of Shipped Bytes
# -----------------------------------------------------------------------------
echo -e "\n${BLUE}${BOLD}[Phase 5/7] Installing and strictly certifying distributed artifact...${NC}"

mkdir -p "$CLAP_INSTALL_DIR"
rm -f "$CLAP_TARGET"

if [ "${CLAP_BOLT_APPLIED:-false}" = true ] && [ -f "$PGO_CLAP_TARGET_DIR/dist/libnam_plug.bolt.so" ]; then
    cp "$PGO_CLAP_TARGET_DIR/dist/libnam_plug.bolt.so" "$CLAP_TARGET"
    strip --strip-unneeded "$CLAP_TARGET"
    echo -e "  Installed CLAP plugin (PGO + BOLT): $CLAP_TARGET"
else
    cp "$PGO_CLAP_TARGET_DIR/dist/libnam_plug.so" "$CLAP_TARGET"
    strip --strip-unneeded "$CLAP_TARGET"
    echo -e "  Installed CLAP plugin (PGO): $CLAP_TARGET"
fi

FINAL_CLAP_SHA=$(sha256sum "$CLAP_TARGET" | cut -d' ' -f1)
echo -e "  ${CYAN}Distributed CLAP artifact SHA-256:${NC} ${BOLD}$FINAL_CLAP_SHA${NC}"

# Gate 1: Symbols and SONAME on distributed artifact
echo -e "  [Gate 1/6] Validating exported symbols and SONAME on distributed artifact..."
if ! nm -D "$CLAP_TARGET" | grep -w "clap_entry" > /dev/null; then
    die "Missing 'clap_entry' symbol in distributed CLAP artifact!"
fi
if ! readelf -d "$CLAP_TARGET" | grep SONAME >/dev/null; then
    die "Missing SONAME in distributed CLAP artifact!"
fi
ok "Symbol and SONAME validation passed."

# Gate 2: External clap-validator
echo -e "  [Gate 2/6] Running external clap-validator against distributed artifact..."
if command -v clap-validator >/dev/null 2>&1; then
    clap-validator validate "$CLAP_TARGET" || die "clap-validator rejected the distribution artifact!"
    ok "clap-validator passed."
else
    if [ "$STRICT_MODE" = "1" ]; then
        die "clap-validator unavailable in strict release mode!"
    fi
    warn "clap-validator unavailable. Skipping external validation."
    GATE_SKIPS+=("clap_validator:unavailable")
fi

# Gate 3: Fail-closed AVX-512 absence certificate (EVEX byte decoding)
echo -e "  [Gate 3/6] Running fail-closed AVX-512 absence scan on distributed artifact..."
"$SCRIPT_DIR/lib/verify_no_avx512_release.sh" "$CLAP_TARGET"
ok "AVX-512 absence certificate passed on distributed artifact."

# Discovery helper for NAMCore render oracle
find_namcore_render() {
    if [ -n "${NAM_CORE_RENDER_BIN:-}" ]; then
        if [ -f "$NAM_CORE_RENDER_BIN" ]; then
            echo "$NAM_CORE_RENDER_BIN"
            return 0
        fi
    fi
    local base hit
    for base in "build/namcore_render" "../NeuralAmpModeler-rs/build/namcore_render"; do
        hit=$(find "$base" -type f -name render -print -quit 2>/dev/null || true)
        if [ -n "$hit" ]; then
            echo "$hit"
            return 0
        fi
    done
    return 1
}

model_fixture=""
if [ -n "${NAM_FIXTURES_DIR:-}" ] && [ -f "$NAM_FIXTURES_DIR/wavenet_a1_standard.nam" ]; then
    model_fixture="$NAM_FIXTURES_DIR/wavenet_a1_standard.nam"
elif [ -f "tests/fixtures/models/wavenet_a1_standard.nam" ]; then
    model_fixture="tests/fixtures/models/wavenet_a1_standard.nam"
fi

ORACLE_BIN=""
ORACLE_SHA=""
FIXTURE_SHA=""

# Gate 4: NAMCore float parity test against distributed artifact
echo -e "  [Gate 4/6] Running NAMCore float parity test against distributed artifact..."
if ORACLE_BIN=$(find_namcore_render); then
    if [ -n "$model_fixture" ]; then
        ORACLE_SHA=$(sha256sum "$ORACLE_BIN" | cut -d' ' -f1)
        FIXTURE_SHA=$(sha256sum "$model_fixture" | cut -d' ' -f1)
        echo -e "  ${BLUE}→ Parity oracle found:${NC} $ORACLE_BIN (sha256: ${ORACLE_SHA:0:16}...)"
        NAM_REQUIRE_CPP_ORACLE=1 CLAP_PLUGIN_UNDER_TEST="$CLAP_TARGET" \
            timeout 600 cargo test --features testing --release --test clap \
            test_clap_parity_multi_rate -- --ignored --nocapture
        ok "NAMCore float parity certified on distributed artifact."
    else
        if [ "$STRICT_MODE" = "1" ]; then
            die "Model fixture tests/fixtures/models/wavenet_a1_standard.nam missing in strict release mode!"
        fi
        warn "Model fixture missing. Skipping parity test."
        GATE_SKIPS+=("namcore_parity:missing_model_fixture")
    fi
else
    if [ "$STRICT_MODE" = "1" ]; then
        die "NAMCore render binary missing in strict release mode! Set NAM_CORE_RENDER_BIN or build it locally."
    fi
    warn "NAMCore render binary missing. Skipping parity test."
    GATE_SKIPS+=("namcore_parity:missing_oracle_bin")
fi

# Gate 5: CabSim IR test against distributed artifact
echo -e "  [Gate 5/6] Running CabSim IR test against distributed artifact..."
CLAP_PLUGIN_UNDER_TEST="$CLAP_TARGET" \
    timeout 300 cargo test --features testing --release --test clap \
    test_cabsim_ir_changes_audio_release_artifact -- --ignored --nocapture
ok "CabSim IR test passed on distributed artifact."

# Gate 6: Distributed Artifact Performance Certification Gate
echo -e "  [Gate 6/6] Running performance certification gate on distributed artifact..."
PERF_REPORT_PATH="$PROJECT_DIR/target/perf-certification-report.json"
PERF_REPORT_SHA=""
PERF_GATE_STATUS="SKIPPED"

set +e
cargo run --locked --profile dist --features testing --bin nam_perf_guard -- certify --clap "$CLAP_TARGET" --out "$PERF_REPORT_PATH"
PERF_EXIT_CODE=$?
set -e

if [ -f "$PERF_REPORT_PATH" ]; then
    PERF_REPORT_SHA=$(sha256sum "$PERF_REPORT_PATH" | cut -d' ' -f1)
fi

if [ "$PERF_EXIT_CODE" -eq 0 ]; then
    PERF_GATE_STATUS="PASSED"
    ok "Performance certification gate passed on distributed artifact."
elif [ "$PERF_EXIT_CODE" -eq 2 ]; then
    PERF_GATE_STATUS="INCONCLUSIVE"
    warn "Performance certification gate inconclusive due to host environmental noise/jitter."
else
    PERF_GATE_STATUS="FAILED"
    if [ "$STRICT_MODE" = "1" ]; then
        die "Performance certification gate failed on distributed artifact (deadline violation or fatal error)!"
    fi
    warn "Performance certification gate failed (non-strict mode)."
    GATE_SKIPS+=("performance_gate:failed")
fi

# Read version for archive naming
VERSION=$(cargo metadata --no-deps --format-version 1 | python3 -c "import sys, json; print(json.load(sys.stdin)['packages'][0]['version'])")
ARCHIVE_NAME="nam-plug-v${VERSION}-linux-x86_64-v3"
TARBALL="$HOME/${ARCHIVE_NAME}.tar.zst"
FLATPAK_BUNDLE="$HOME/${ARCHIVE_NAME}.flatpak"
TARBALL_SHA=""
FLATPAK_SHA=""

# -----------------------------------------------------------------------------
# PHASE 6: Release Packaging (.tar.zst)
# -----------------------------------------------------------------------------
if [ "$BUILD_TARBALL" = true ]; then
    echo -e "\n${BLUE}${BOLD}[Phase 6/7] Generating distribution tarball...${NC}"

    PKG_DIR="$(mktemp -d -t nam-plug-pkg.XXXXXX)"
    mkdir -p "$PKG_DIR/$ARCHIVE_NAME"

    cp "$CLAP_TARGET" "$PKG_DIR/$ARCHIVE_NAME/nam_plug.clap"
    cp README.md LICENSE.txt "$PKG_DIR/$ARCHIVE_NAME/" 2>/dev/null || true

    # Generate 1-click install script for end-users
    cat << 'EOF' > "$PKG_DIR/$ARCHIVE_NAME/install.sh"
#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
set -e
CLAP_DIR="$HOME/.clap"
mkdir -p "$CLAP_DIR"
cp nam_plug.clap "$CLAP_DIR/"
echo "✅ Installed nam_plug.clap to $CLAP_DIR/nam_plug.clap"
EOF
    chmod +x "$PKG_DIR/$ARCHIVE_NAME/install.sh"

    tar -C "$PKG_DIR" -I "zstd -6 -T0" -cf "$TARBALL" "$ARCHIVE_NAME"
    rm -rf "$PKG_DIR"
    PKG_DIR=""

    TARBALL_SHA=$(sha256sum "$TARBALL" | cut -d' ' -f1)
    echo -e "  ${GREEN}✓${NC} Distribution package generated at: ${BOLD}$TARBALL${NC} (sha256: ${TARBALL_SHA:0:16}...)"
else
    echo -e "\n${YELLOW}[Phase 6/7] Skipping tarball packaging (--no-tarball).${NC}"
fi

# -----------------------------------------------------------------------------
# PHASE 7: Release Packaging (.flatpak)
# -----------------------------------------------------------------------------
if [ "$BUILD_FLATPAK" = true ]; then
    echo -e "\n${BLUE}${BOLD}[Phase 7/7] Generating Flatpak Plugin Extension Bundle (.flatpak)...${NC}"

    FLATPAK_BUILD_DIR="$(mktemp -d -t nam-plug-flatpak-build.XXXXXX)"
    FLATPAK_REPO_DIR="$(mktemp -d -t nam-plug-flatpak-repo.XXXXXX)"

    SDK_NAME="org.freedesktop.Sdk"
    if ! flatpak info org.freedesktop.Sdk//25.08 &>/dev/null; then
        SDK_NAME="org.freedesktop.Platform"
    fi

    echo -e "  Initializing Flatpak extension environment (25.08 using $SDK_NAME)..."
    flatpak build-init --type=extension --extension-tag=25.08 \
        "$FLATPAK_BUILD_DIR" \
        org.freedesktop.LinuxAudio.Plugins.NAMPlug \
        "$SDK_NAME" \
        org.freedesktop.Platform \
        25.08

    mkdir -p "$FLATPAK_BUILD_DIR/files/clap"
    mkdir -p "$FLATPAK_BUILD_DIR/files/share/metainfo"
    mkdir -p "$FLATPAK_BUILD_DIR/files/share/licenses/org.freedesktop.LinuxAudio.Plugins.NAMPlug"

    cp "$CLAP_TARGET" "$FLATPAK_BUILD_DIR/files/clap/nam_plug.clap"
    echo -e "  ${GREEN}✓${NC} Installed nam_plug.clap -> extension directory"

    METAINFO_SRC="packaging/flatpak/org.freedesktop.LinuxAudio.Plugins.NAMPlug.metainfo.xml"
    if [ -f "$METAINFO_SRC" ]; then
        cp "$METAINFO_SRC" "$FLATPAK_BUILD_DIR/files/share/metainfo/"
        echo -e "  ${GREEN}✓${NC} Installed AppStream metainfo XML"
    else
        echo -e "  ${YELLOW}Warning: AppStream metainfo not found at $METAINFO_SRC${NC}"
    fi

    if [ -f "LICENSE.txt" ]; then
        cp "LICENSE.txt" "$FLATPAK_BUILD_DIR/files/share/licenses/org.freedesktop.LinuxAudio.Plugins.NAMPlug/"
    elif [ -f "LICENSE" ]; then
        cp "LICENSE" "$FLATPAK_BUILD_DIR/files/share/licenses/org.freedesktop.LinuxAudio.Plugins.NAMPlug/"
    fi

    echo -e "  Finalizing Flatpak extension configuration..."
    flatpak build-finish "$FLATPAK_BUILD_DIR" --extension-priority=100

    echo -e "  Exporting extension to temporary OSTree repository..."
    flatpak build-export --update-appstream "$FLATPAK_REPO_DIR" "$FLATPAK_BUILD_DIR" 25.08

    echo -e "  Building Flatpak bundle: $FLATPAK_BUNDLE..."
    mkdir -p "$(dirname "$FLATPAK_BUNDLE")"
    flatpak build-bundle --runtime "$FLATPAK_REPO_DIR" "$FLATPAK_BUNDLE" org.freedesktop.LinuxAudio.Plugins.NAMPlug 25.08

    FLATPAK_SHA=$(sha256sum "$FLATPAK_BUNDLE" | cut -d' ' -f1)
    echo -e "  ${GREEN}✓${NC} Flatpak bundle generated successfully: ${BOLD}$FLATPAK_BUNDLE${NC} (sha256: ${FLATPAK_SHA:0:16}...)"

    if [ "$DO_INSTALL_FLATPAK" = true ]; then
        echo -e "  Installing Flatpak extension locally for current user..."
        flatpak install --user --reinstall -y "$FLATPAK_BUNDLE"
        echo -e "  ${GREEN}✓${NC} Flatpak plugin extension installed successfully."
    fi

    rm -rf "$FLATPAK_BUILD_DIR" "$FLATPAK_REPO_DIR"
    FLATPAK_BUILD_DIR=""
    FLATPAK_REPO_DIR=""
else
    echo -e "\n${YELLOW}[Phase 7/7] Skipping Flatpak packaging (--no-flatpak).${NC}"
fi

# -----------------------------------------------------------------------------
# PHASE 8: Atomic Cryptographic Build Receipt Generation
# -----------------------------------------------------------------------------
echo -e "\n${BLUE}${BOLD}[Receipt] Generating cryptographic build provenance receipt...${NC}"

# Re-verify clean work tree before receipt writing (fail-closed)
check_git_clean_strict

GIT_COMMIT=$(git rev-parse HEAD 2>/dev/null || echo "unknown")
GIT_BRANCH=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "unknown")
GIT_DIRTY=false
if [ -n "$(git status --porcelain 2>/dev/null || true)" ]; then
    GIT_DIRTY=true
fi

# Determine typed certification status (CERTIFIED, FUNCTIONALLY_CERTIFIED, INCOMPLETE)
RECEIPT_STATUS="CERTIFIED"
if [ "$GIT_DIRTY" = "true" ] || [ "${CORE_GIT_DIRTY:-false}" = "true" ]; then
    RECEIPT_STATUS="INCOMPLETE"
fi

if [ ${#GATE_SKIPS[@]} -gt 0 ] || [ "$PERF_GATE_STATUS" = "FAILED" ] || [ "$QUICK_QA_PASSED" != true ] || [ "$HEAP_AUDIT_PASSED" != true ]; then
    RECEIPT_STATUS="INCOMPLETE"
elif [ "$PERF_GATE_STATUS" = "INCONCLUSIVE" ] || [ "$USE_PGO" = false ] || [ "${CLAP_BOLT_APPLIED:-false}" = false ]; then
    if [ "$RECEIPT_STATUS" = "CERTIFIED" ]; then
        RECEIPT_STATUS="FUNCTIONALLY_CERTIFIED"
    fi
fi

GATE_SKIPS_JOINED="${GATE_SKIPS[*]:-}"
if [ "$RECEIPT_STATUS" = "INCOMPLETE" ]; then
    warn "Receipt status INCOMPLETE: git_dirty=$GIT_DIRTY core_git_dirty=$CORE_GIT_DIRTY skipped_gates=[${GATE_SKIPS_JOINED:-(none)}]"
fi

CARGO_LOCK_SHA=$(sha256sum Cargo.lock 2>/dev/null | cut -d' ' -f1 || echo "")
RUSTC_VER=$(rustc --version 2>/dev/null || echo "unknown")
TIMESTAMP=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

RECEIPT_TMP="$(mktemp -t nam-plug-receipt.XXXXXX)"
python3 -c "
import json, sys

skipped_gates = [g for g in sys.argv[1].split('|') if g]

receipt = {
    'schema_version': '1.1',
    'timestamp_utc': '$TIMESTAMP',
    'status': '$RECEIPT_STATUS',
    'strict_mode': bool($STRICT_MODE),
    'skipped_gates': skipped_gates,
    'package': {
        'name': 'nam-plug',
        'version': '$VERSION'
    },
    'provenance': {
        'git_commit': '$GIT_COMMIT',
        'git_branch': '$GIT_BRANCH',
        'git_dirty': bool('$GIT_DIRTY' == 'true'),
        'cargo_lock_sha256': '$CARGO_LOCK_SHA',
        'rustc_version': '$RUSTC_VER',
        'rustflags': '$CLAP_RUSTFLAGS'
    },
    'engine_provenance': {
        'dependency_type': 'path_patch' if '$CORE_PATH' else 'crates_io',
        'path': '$CORE_PATH' or None,
        'version': '$CORE_VERSION',
        'git_commit': '$CORE_GIT_COMMIT',
        'git_branch': '$CORE_GIT_BRANCH',
        'git_dirty': bool('$CORE_GIT_DIRTY' == 'true'),
        'source_tree_sha256': '$CORE_TREE_SHA' or None
    },
    'quick_qa_audit': {
        'status': 'PASSED' if bool('$QUICK_QA_PASSED' == 'true') else 'FAILED',
        'heap_audit_passed': bool('$HEAP_AUDIT_PASSED' == 'true'),
        'receipt_sha256': '$QUICK_RECEIPT_SHA' or None,
        'heap_audit_log_sha256': '$HEAP_AUDIT_LOG_SHA' or None
    },
    'optimizations': {
        'pgo_applied': bool('$USE_PGO' == 'true'),
        'bolt_applied': bool('$CLAP_BOLT_APPLIED' == 'true'),
        'bolt_profile_sha256': '$BOLT_PROFILE_SHA' or None,
        'bolt_version': '$BOLT_VERSION' or None
    },
    'performance_certification': {
        'gate_status': '$PERF_GATE_STATUS',
        'report_path': '$PERF_REPORT_PATH' if '$PERF_REPORT_SHA' else None,
        'report_sha256': '$PERF_REPORT_SHA' or None
    },
    'artifacts': {
        'clap_installed_path': '$CLAP_TARGET',
        'clap_installed_sha256': '$FINAL_CLAP_SHA',
        'tarball_path': '$TARBALL' if '$BUILD_TARBALL' == 'true' else None,
        'tarball_sha256': '$TARBALL_SHA' if '$BUILD_TARBALL' == 'true' else None,
        'flatpak_path': '$FLATPAK_BUNDLE' if '$BUILD_FLATPAK' == 'true' else None,
        'flatpak_sha256': '$FLATPAK_SHA' if '$BUILD_FLATPAK' == 'true' else None
    },
    'oracles_and_fixtures': {
        'oracle_render_bin': '$ORACLE_BIN' or None,
        'oracle_render_sha256': '$ORACLE_SHA' or None,
        'fixture_model_path': '$model_fixture' or None,
        'fixture_model_sha256': '$FIXTURE_SHA' or None
    }
}

with open('$RECEIPT_TMP', 'w') as f:
    json.dump(receipt, f, indent=2)
" "$GATE_SKIPS_JOINED"

RECEIPT_TARGET="$PROJECT_DIR/target/release-receipt.json"
mv -f "$RECEIPT_TMP" "$RECEIPT_TARGET"
echo -e "  ${GREEN}✓${NC} Atomic build receipt generated at: ${BOLD}$RECEIPT_TARGET${NC} (status: $RECEIPT_STATUS)"

echo -e "\n${GREEN}${BOLD}================================================================================${NC}"
if [ "$RECEIPT_STATUS" = "CERTIFIED" ]; then
    echo -e "${GREEN}${BOLD}   Pipeline completed! Artifacts certified and ready for distribution:   ${NC}"
elif [ "$RECEIPT_STATUS" = "FUNCTIONALLY_CERTIFIED" ]; then
    echo -e "${YELLOW}${BOLD}   Pipeline completed! Functionally certified (PGO/BOLT omitted or perf inconclusive): ${NC}"
else
    echo -e "${RED}${BOLD}   Pipeline completed with gaps — receipt status INCOMPLETE (not certified): ${NC}"
fi
echo -e "  ${BOLD}Artifacts saved:${NC}"
echo -e "    - CLAP Plugin:    ${CYAN}$CLAP_TARGET${NC} (sha256: ${FINAL_CLAP_SHA:0:16}...)"
echo -e "    - Build Receipt:  ${CYAN}$RECEIPT_TARGET${NC}"
if [ "$BUILD_TARBALL" = true ]; then
    echo -e "    - Tarball:        ${CYAN}$TARBALL${NC} (sha256: ${TARBALL_SHA:0:16}...)"
fi
if [ "$BUILD_FLATPAK" = true ]; then
    echo -e "    - Flatpak Bundle: ${CYAN}$FLATPAK_BUNDLE${NC} (sha256: ${FLATPAK_SHA:0:16}...)"
fi
if [ -f "$PROJECT_DIR/target/dsp_hotpath.asm" ]; then
    echo -e "    - Assembly ASM:   ${CYAN}$PROJECT_DIR/target/dsp_hotpath.asm${NC}"
fi
echo -e "${GREEN}${BOLD}================================================================================${NC}\n"
