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

# Host memory guard: cap parallel rustc jobs to 4 to prevent OOM
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"

# Preserve the original CLI for the memory-guard re-exec below (the option
# parser consumes "$@" with shift).
ORIGINAL_ARGS=("$@")

# Parse command line options
DO_INSTALL_FLATPAK=false
BUILD_FLATPAK=true
BUILD_TARBALL=true
USE_PGO=true
USE_BOLT=true
RELEASE_CEREMONY=false
STRICT_MODE=1

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
  --strict, --strict-release
                         Enforce strict fail-closed mode (default: 1).
  --release-ceremony     Official release ceremony mode: requires a pristine worktree & strict tests.
  -h, --help             Show this help message and exit.

Environment:
  NAM_PGO_REUSE_TARGET=1 Reuse target/pgo-build and target/pgo-clap for local iteration
                         (ignored with a warning in strict/ceremony mode; certified builds
                         always start from an empty target).

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
        --strict|--strict-release)
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
SCRIPT_PATH="$SCRIPT_DIR/$(basename "$0")"
NAM_LIB_NO_CD=1 source "$SCRIPT_DIR/lib/_lib.sh"

# Ensure execution from the subproject root directory
cd "$PROJECT_DIR"

# ── Memory guard (OOM containment) ──────────────────────────────────────────
# PGO/BOLT + fat LTO is the heaviest build in the repository. Re-exec inside a
# transient, memory-capped systemd scope so a compiler peak is contained in its
# own cgroup instead of triggering a global OOM that kills the IDE. Fail-open
# when unavailable. The PGO/BOLT pipeline legitimately needs more headroom than
# the test suites, so it defaults to a 10G cap; override with NAM_MEM_GUARD_MAX.
export NAM_MEM_GUARD_MAX="${NAM_MEM_GUARD_MAX:-10G}"
export NAM_MEM_GUARD_HIGH="${NAM_MEM_GUARD_HIGH:-8G}"
if [ "${NAM_MEM_GUARD:-0}" != "1" ] && [ "${NAM_MEM_GUARD_ACTIVE:-0}" != "1" ] && [ "${NAM_NO_MEM_GUARD:-0}" != "1" ]; then
    export NAM_MEM_GUARD=1
    exec "$SCRIPT_DIR/mem-guard.sh" "$SCRIPT_PATH" "${ORIGINAL_ARGS[@]}"
fi

# Printed after the memory-guard re-exec: `exec` restarts the script and would
# otherwise print the banner twice.
echo -e "${BLUE}${BOLD}========================================================================${NC}"
echo -e "${BLUE}${BOLD}   NAM-Plug Unified Release Build & Optimization Pipeline               ${NC}"
echo -e "${BLUE}${BOLD}========================================================================${NC}"

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
            if [ "$RELEASE_CEREMONY" = "true" ]; then
                die "Engine repository '$CORE_PATH' has uncommitted changes in release ceremony mode (provenance fail-closed). Commit, stash or clean before generating a certified release."
            else
                warn "Engine repository '$CORE_PATH' is dirty (non-ceremony build)."
            fi
        else
            echo -e "  ${GREEN}✓${NC} Engine ($CORE_PATH) provenance verified clean (commit: ${CORE_GIT_COMMIT:0:12}, branch: $CORE_GIT_BRANCH, version: $CORE_VERSION)."
        fi
    else
        # Default: public crate from crates.io (host-agnostic, zero sibling checkout dependency)
        CORE_PATH=""
        CORE_GIT_COMMIT=""
        CORE_GIT_BRANCH=""
        CORE_GIT_DIRTY=false
        local lock_info
        lock_info=$(python3 -c '
import tomllib
try:
    with open("Cargo.lock", "rb") as f:
        data = tomllib.load(f)
    for p in data.get("package", []):
        if p.get("name") == "NeuralAmpModeler-rs":
            v = p.get("version", "unknown")
            c = p.get("checksum", "")
            print(f"{v}|{c}")
            break
except Exception:
    pass
' 2>/dev/null || true)
        if [ -n "$lock_info" ]; then
            CORE_VERSION="${lock_info%%|*}"
            CORE_TREE_SHA="${lock_info##*|}"
        else
            CORE_VERSION=$(grep -A 2 'name = "NeuralAmpModeler-rs"' Cargo.lock 2>/dev/null | grep 'version =' | head -n 1 | cut -d'"' -f2 || echo "unknown")
            CORE_TREE_SHA=$(grep -A 4 'name = "NeuralAmpModeler-rs"' Cargo.lock 2>/dev/null | grep 'checksum =' | head -n 1 | cut -d'"' -f2 || echo "")
        fi

        if [ "$CORE_VERSION" = "unknown" ] && [ "$RELEASE_CEREMONY" = "true" ]; then
            die "Failed to resolve NeuralAmpModeler-rs engine version from Cargo.lock in release ceremony mode (provenance fail-closed)."
        fi

        if [ -n "$CORE_TREE_SHA" ]; then
            echo -e "  ${GREEN}✓${NC} Engine (NeuralAmpModeler-rs) crates.io provenance verified (version: $CORE_VERSION, checksum: ${CORE_TREE_SHA:0:12}...)."
        else
            echo -e "  ${GREEN}✓${NC} Engine (NeuralAmpModeler-rs) crates.io provenance verified (version: $CORE_VERSION)."
        fi
    fi
}

# Provenance fail-closed: an official certified release can never be produced
# from a dirty work tree. In release ceremony mode this aborts before any heavy
# work is started and is re-verified before packaging/receipt generation.
check_git_clean_strict() {
    if [ "$RELEASE_CEREMONY" != "true" ]; then
        return 0
    fi
    if ! git rev-parse --is-inside-work-tree &>/dev/null; then
        die "Not inside a git work tree in release ceremony mode; provenance cannot be certified. Run the release from a clean git checkout."
    fi
    if [ -n "$(git status --porcelain 2>/dev/null || true)" ]; then
        die "Git working tree is dirty in release ceremony mode (provenance fail-closed). Commit, stash or clean before generating a certified release; no receipt will be written."
    fi
}
check_git_clean_strict
check_engine_provenance

# State tracking for signal safety and cleanup
ORIG_PARANOID=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo "2")
PARANOID_MODIFIED=false
WORKLOAD_PID=""
declare -a GATE_SKIPS=()

# PGO workload evidence recorded in the release receipt (S3.2–S3.4).
PGO_WORKLOAD_LOG="$PROJECT_DIR/target/logs/pgo-workload.log"
PGO_WORKLOAD_RECEIPT="$PROJECT_DIR/target/pgo-workload-receipt.json"
PGO_WORKLOAD_RAN_PHASE1=false
PGO_WORKLOAD_RAN_BOLT=false
PGO_CLIPPING_PHASE1=0
PGO_CLIPPING_BOLT=0
PGO_WORKLOAD_GAIN=""
PGO_WORKLOAD_SCENARIOS=""
PGO_WORKLOAD_CALLBACKS=""

# Artifact parity gates (S3.1).
PARITY_OVERSAMPLE_STATUS="SKIP"
PARITY_OVERSAMPLE_REASON=""
PARITY_CABSIM_STATUS="SKIP"
PARITY_CABSIM_REASON=""

# External clap-validator outcome. Skipped tests are recorded explicitly: a
# green validator run with N skips is not the same evidence as N=0.
CLAP_VALIDATOR_STATUS="UNKNOWN"
CLAP_VALIDATOR_TESTS_RUN=""
CLAP_VALIDATOR_PASSED=""
CLAP_VALIDATOR_FAILED=""
CLAP_VALIDATOR_SKIPPED=""
CLAP_VALIDATOR_SKIP_REASONS=""
CLAP_VALIDATOR_LOG=""

# Performance report fields surfaced in the receipt (isolation, not noise).
PERF_ENVIRONMENT_DEGRADED="false"
PERF_ENVIRONMENT_DEGRADED_REASONS=""

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

# Hermetic build targets: every certified run starts from an empty PGO/BOLT
# target. For local iteration the operator may opt into reusing the previous
# target with NAM_PGO_REUSE_TARGET=1; that is deliberately refused in strict and
# ceremony modes, because a stale object could otherwise be certified as freshly
# built bytes.
PGO_REUSE_TARGET=false
if [ "${NAM_PGO_REUSE_TARGET:-0}" = "1" ]; then
    if [ "$STRICT_MODE" = "1" ] || [ "$RELEASE_CEREMONY" = "true" ]; then
        warn "NAM_PGO_REUSE_TARGET=1 ignored in strict/ceremony mode: certified builds are hermetic."
    else
        PGO_REUSE_TARGET=true
        warn "NAM_PGO_REUSE_TARGET=1: reusing $PGO_BUILD_TARGET_DIR (uncertified local iteration)."
    fi
fi
if [ "$PGO_REUSE_TARGET" = "true" ] && [ -d "$PGO_BUILD_TARGET_DIR" ]; then
    mkdir -p "$PGO_CLAP_TARGET_DIR" "$PROFRAW_DIR" "$BOLT_DIR" "$PROJECT_DIR/target"
else
    rm -rf "$PGO_BUILD_TARGET_DIR" "$PGO_CLAP_TARGET_DIR"
    mkdir -p "$PROFRAW_DIR" "$BOLT_DIR" "$PROJECT_DIR/target"
fi

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

# Artifact installed by the previous session, captured before this pipeline
# overwrites it. The long-audit receipt is bound to this hash: a receipt that
# describes a different binary is not evidence for this build.
INSTALLED_AT_START_SHA=""
if [ -f "$CLAP_TARGET" ]; then
    INSTALLED_AT_START_SHA=$(sha256sum "$CLAP_TARGET" | cut -d' ' -f1)
fi

# Certified-artifact identity (S3 / F-CER-01). The preliminary invocation
# captures the bytes it installs; the ceremony reuses exactly those bytes and
# never recompiles after the long audit.
CERTIFIED_DIR="$PROJECT_DIR/target/certified"
CERTIFIED_CLAP="$CERTIFIED_DIR/nam_plug.clap"
CERTIFIED_CLAP_SHA_FILE="$CERTIFIED_CLAP.sha256"
CERTIFIED_COPY_SHA=""
REUSED_CERTIFIED_BINARY=false
PRE_STRIP_ARTIFACT_SHA=""
REUSED_BOLT_APPLIED=false
REUSED_BOLT_PROOF=""
REUSED_BOLT_PROFILE_SHA=""
REUSED_BOLT_VERSION=""
REUSED_BOLT_LBR_SAMPLES=""
REUSED_BOLT_LBR_FDATA_SHA256=""
CODEGEN_PRE_JSON="$PROJECT_DIR/target/logs/codegen-pre-bolt.json"
CODEGEN_POST_JSON="$PROJECT_DIR/target/logs/codegen-post-bolt.json"
LONG_RECEIPT_PATH="$PROJECT_DIR/target/logs/long-audit-receipt.jsonl"

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
QUICK_RECEIPT_CHECK_LOG="$PROJECT_DIR/target/logs/quick-receipt-check.log"

# Discovery helper for NAMCore render oracle
find_namcore_render() {
    if [ -n "${NAM_CORE_RENDER_BIN:-}" ]; then
        if [ -f "$NAM_CORE_RENDER_BIN" ]; then
            echo "$NAM_CORE_RENDER_BIN"
            return 0
        fi
    fi
    local cand
    for cand in \
        "build/namcore_render/tools/render" \
        "build/namcore_render/Release/render" \
        "build/namcore_render/Debug/render" \
        "build/namcore_render/render" \
        "build/namcore_render/namcore_render" \
        "../NeuralAmpModeler-rs/build/namcore_render/tools/render" \
        "../NeuralAmpModeler-rs/build/namcore_render/Release/render" \
        "../NeuralAmpModeler-rs/build/namcore_render/Debug/render" \
        "../NeuralAmpModeler-rs/build/namcore_render/render" \
        "../NeuralAmpModeler-rs/build/namcore_render/namcore_render"; do
        if [ -x "$cand" ]; then
            echo "$cand"
            return 0
        fi
    done
    local base hit
    for base in "build/namcore_render" "../NeuralAmpModeler-rs/build/namcore_render"; do
        hit=$(find "$base" -type f \( -name render -o -name namcore_render \) -executable -print -quit 2>/dev/null || true)
        if [ -n "$hit" ]; then
            echo "$hit"
            return 0
        fi
    done
    return 1
}

if [ -z "${NAM_CORE_RENDER_BIN:-}" ]; then
    if _candidate_oracle=$(find_namcore_render 2>/dev/null); then
        export NAM_CORE_RENDER_BIN="$_candidate_oracle"
    fi
fi

# Semantic verification of the quick receipt — never a substring search. The
# declared OVERALL verdict must be consistent with the phase/gap evidence in the
# same receipt; a green token pasted into a comment is rejected (fail-closed).
# The same failure is reported once even though the gate is queried several times.
_QUICK_RECEIPT_LAST_WARN=""
quick_receipt_ok() {
    local status=0 err="" last=""
    mkdir -p "$(dirname "$QUICK_RECEIPT_CHECK_LOG")"
    err=$(cargo run --quiet --locked --bin quick_receipt_check -- "$@" 2>&1 >/dev/null) || status=$?
    printf '%s\n' "$err" > "$QUICK_RECEIPT_CHECK_LOG"
    if [ "$status" -ne 0 ]; then
        last=$(printf '%s\n' "$err" | grep -v '^[[:space:]]*$' | tail -n 1 || true)
        if [ -n "$last" ] && [ "$last" != "$_QUICK_RECEIPT_LAST_WARN" ]; then
            warn "quick receipt verification failed: $last (see target/logs/quick-receipt-check.log)"
            _QUICK_RECEIPT_LAST_WARN="$last"
        fi
    fi
    return "$status"
}

# Opt-in reuse (NAM_REUSE_QUICK_RECEIPT=1): skips the ~25 min quick suite only
# when the existing receipt is provably still valid — strict run, OVERALL PASSED,
# heap audit proven, ARTIFACT_SHA256 equal to the artifact being released and no
# build input newer than the receipt. Any doubt falls back to a full run.
quick_receipt_reusable() {
    [ "${NAM_REUSE_QUICK_RECEIPT:-0}" = "1" ] || return 1
    [ -f "$QUICK_RECEIPT" ] || return 1
    local art sha newer
    art="${CLAP_PLUGIN_UNDER_TEST:-${CLAP_PLUGIN_PATH:-${CARGO_TARGET_DIR:-target}/release/libnam_plug.so}}"
    [ -f "$art" ] || return 1
    sha=$(sha256sum "$art" | cut -d' ' -f1)
    newer=$(find src tests Cargo.toml Cargo.lock .cargo build.rs \
        utils/tests-quick.sh utils/verify_rt_codegen.sh \
        -type f -newer "$QUICK_RECEIPT" -print -quit 2>/dev/null || true)
    if [ -n "$newer" ]; then
        warn "Quick receipt not reusable: '$newer' is newer than the receipt."
        return 1
    fi
    quick_receipt_ok --require-passed --require-heap-audit --require-strict \
        --expect-artifact-sha256 "$sha" "$QUICK_RECEIPT"
}

if [ "${NAM_SKIP_QUICK_QA:-0}" != "1" ] && quick_receipt_reusable; then
    echo -e "  ${GREEN}✓${NC} Reusing strict quick receipt (artifact hash unchanged, no newer inputs); quick suite skipped."
elif [ "${NAM_SKIP_QUICK_QA:-0}" != "1" ]; then
    local_quick_strict=0
    if [ "$RELEASE_CEREMONY" = "true" ] || [ "$STRICT_MODE" = "1" ]; then
        local_quick_strict=1
    fi
    NAM_QUICK_STRICT=$local_quick_strict "$SCRIPT_DIR/tests-quick.sh" || {
        if [ "$RELEASE_CEREMONY" = "true" ]; then
            die "Quick QA preflight suite failed in release ceremony mode!"
        elif [ "$STRICT_MODE" = "1" ]; then
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

if [ -f "$QUICK_RECEIPT" ] && quick_receipt_ok --require-passed "$QUICK_RECEIPT"; then
    QUICK_QA_PASSED=true
    QUICK_RECEIPT_SHA=$(sha256sum "$QUICK_RECEIPT" | cut -d' ' -f1)
fi

if [ -f "$HEAP_LOG" ] && [ -f "$QUICK_RECEIPT" ] && quick_receipt_ok --require-heap-audit "$QUICK_RECEIPT"; then
    HEAP_AUDIT_PASSED=true
    HEAP_AUDIT_LOG_SHA=$(sha256sum "$HEAP_LOG" | cut -d' ' -f1)
fi

# The quick suite tests the release `.so` (pre-PGO/BOLT), not the installed
# binary, and its strictness is independent of this build. Record both facts
# explicitly instead of letting a green OVERALL imply "strict, installed artifact".
QUICK_RECEIPT_ARTIFACT_SHA=""
QUICK_RECEIPT_STRICT=""
if [ -f "$QUICK_RECEIPT" ]; then
    QUICK_RECEIPT_ARTIFACT_SHA=$(awk '/^ARTIFACT_SHA256: /{print $2; exit}' "$QUICK_RECEIPT" || true)
    QUICK_RECEIPT_STRICT=$(awk '/^STRICT: /{print $2; exit}' "$QUICK_RECEIPT" || true)
fi

# Gaps declared by the quick suite are never silently accepted: propagate each
# one as a typed gate skip, and fail closed in strict/ceremony builds.
QUICK_GAPS=()
if [ -f "$QUICK_RECEIPT" ]; then
    while IFS= read -r quick_gap; do
        [ -n "$quick_gap" ] || continue
        QUICK_GAPS+=("$quick_gap")
        GATE_SKIPS+=("quick_gap:$quick_gap")
    done < <(sed -n 's/^GAP:[[:space:]]*//p' "$QUICK_RECEIPT")
fi
if [ ${#QUICK_GAPS[@]} -gt 0 ]; then
    if [ "$STRICT_MODE" = "1" ] || [ "$RELEASE_CEREMONY" = "true" ]; then
        die "Quick QA receipt declares gaps in strict release mode: ${QUICK_GAPS[*]}"
    fi
    warn "Quick QA receipt gaps propagated to the release receipt: ${QUICK_GAPS[*]}"
fi

if [ "$QUICK_QA_PASSED" = true ] && [ "$HEAP_AUDIT_PASSED" = true ]; then
    echo -e "  ${GREEN}✓${NC} Quick QA preflight and zero-alloc heap audit verified."
else
    if [ "$RELEASE_CEREMONY" = "true" ]; then
        die "Quick QA preflight did not pass or heap audit was not verified in release ceremony mode!"
    elif [ "$STRICT_MODE" = "1" ]; then
        die "Quick QA preflight did not pass or heap audit was not verified in strict release mode!"
    fi
    warn "Quick QA preflight or heap audit not fully verified."
    GATE_SKIPS+=("quick_qa:unverified")
fi

# -----------------------------------------------------------------------------
# GUI/X11 evidence: the real-window tests are #[ignore]d (they need a dedicated
# Xvfb) and only run through utils/tests-gui.sh. A release must not silently
# omit them, so a missing/failed/simulated GUI receipt becomes a typed gap.
# -----------------------------------------------------------------------------
GUI_RECEIPT_PATH="$PROJECT_DIR/target/logs/gui-receipt.txt"
if [ -s "$GUI_RECEIPT_PATH" ] && grep -q '^gui_test_suite status=PASS' "$GUI_RECEIPT_PATH"; then
    echo -e "  ${GREEN}✓${NC} GUI/X11 test receipt present (status=PASS)."
else
    warn "No passing GUI/X11 receipt (target/logs/gui-receipt.txt): run utils/tests-gui.sh — typed gap gui_tests:unverified."
    GATE_SKIPS+=("gui_tests:unverified")
fi

# -----------------------------------------------------------------------------
# Release-ceremony gate: a REAL, strict, consistent LONG receipt is mandatory.
# The historical gate read only the quick receipt via a substring grep — not
# proof of a full audit. A simulated/gapped/failed/forged long receipt is
# rejected by the unified-schema semantic verifier (never a substring search).
# -----------------------------------------------------------------------------
if [ "$RELEASE_CEREMONY" = "true" ]; then
    if [ ! -f "$LONG_RECEIPT_PATH" ]; then
        die "Release ceremony requires a real long-audit receipt on disk (target/logs/long-audit-receipt.jsonl is missing).\nAsk the human operator to execute:\n  ./utils/tests-long.sh --strict-pre-release\n(AI agents MUST NEVER execute the long suite)."
    fi
    if ! cargo run --quiet --locked --features testing --bin plug_long_receipt -- validate --strict --out "$LONG_RECEIPT_PATH"; then
        die "Release ceremony rejected the long-audit receipt (target/logs/long-audit-receipt.jsonl): strict semantic verification failed — SIMULATED, declared gaps, failures or a forged OVERALL verdict are rejected fail-closed."
    fi
    echo -e "  ${GREEN}✓${NC} Real strict long-audit receipt verified (unified engine schema, semantic verification)."

    # -------------------------------------------------------------------------
    # F-CER-01: the ceremony never recompiles. It reuses the exact bytes that
    # the long suite certified and rejects any divergent link in the chain:
    # long-receipt SHA == installed SHA == target/certified copy SHA.
    # -------------------------------------------------------------------------
    set +e
    CERTIFIED_LONG_OUT=$(cargo run --quiet --locked --features testing --bin plug_long_receipt -- \
        verify-artifact --out "$LONG_RECEIPT_PATH" --artifact "$CLAP_TARGET" 2>/dev/null)
    CERTIFIED_LONG_RC=$?
    set -e
    CERTIFIED_LONG_SHA=$(printf '%s\n' "$CERTIFIED_LONG_OUT" | awk '/^ARTIFACT_SHA256: /{print $2; exit}')
    [ "$CERTIFIED_LONG_SHA" = "-" ] && CERTIFIED_LONG_SHA=""
    if [ "$CERTIFIED_LONG_RC" -ne 0 ]; then
        die "F-CER-01: the long-audit receipt does not certify the installed artifact (verify-artifact rc=$CERTIFIED_LONG_RC, sha=${CERTIFIED_LONG_SHA:-none}). Run the preliminary --strict-release --install build first; the ceremony refuses to rebuild."
    fi

    REUSE_VERDICT=$(ceremony_certified_verdict "$CERTIFIED_DIR" "$CLAP_TARGET" "$CERTIFIED_LONG_SHA")
    if [ "$REUSE_VERDICT" != "ok" ]; then
        die "F-CER-01: certified artifact chain rejected ($REUSE_VERDICT). Expected sha256(long receipt) == sha256(~/.clap/nam_plug.clap) == sha256(target/certified/nam_plug.clap). Re-run the preliminary --strict-release --install build; never rebuild silently in ceremony mode."
    fi
    if [ "$USE_PGO" != "true" ] || [ "$USE_BOLT" != "true" ]; then
        die "F-CER-01: ceremony reuse requires the PGO+BOLT flags (got --no-pgo/--no-bolt); the certified artifact was produced with both and the receipt must describe the bytes it ships."
    fi
    CERTIFIED_COPY_SHA=$(sha256sum "$CERTIFIED_CLAP" | cut -d' ' -f1)

    CODEGEN_VERDICT=$(ceremony_codegen_verdict "$CODEGEN_PRE_JSON" "$CODEGEN_POST_JSON")
    if [ "$CODEGEN_VERDICT" != "ok" ]; then
        die "F-CER-01: reused codegen evidence rejected ($CODEGEN_VERDICT). The ceremony requires the codegen-pre/post receipts produced by the preliminary PGO+BOLT build (target/logs/codegen-*-bolt.json)."
    fi
    PRE_STRIP_ARTIFACT_SHA=$(codegen_post_sha "$CODEGEN_POST_JSON")

    # Chain of custody for the optimization evidence: the preliminary receipt
    # must describe this exact installed binary and the same codegen/PGO logs.
    PRELIM_RECEIPT="$PROJECT_DIR/target/release-receipt.json"
    if [ ! -f "$PRELIM_RECEIPT" ]; then
        die "F-CER-01: preliminary receipt target/release-receipt.json is missing; the ceremony cannot prove which build produced the certified artifact. Run the preliminary --strict-release --install build first."
    fi
    PRELIM_VALUES=$(python3 - "$PRELIM_RECEIPT" <<'PY'
import json
import sys

try:
    with open(sys.argv[1], "r", encoding="utf-8") as handle:
        doc = json.load(handle)
except Exception:
    print("|||||||||||||||||||||||||||")
    raise SystemExit(0)

artifacts = doc.get("artifacts") or {}
optimizations = doc.get("optimizations") or {}
pgo = doc.get("pgo_workload") or {}
provenance = doc.get("provenance") or {}
engine = doc.get("engine_provenance") or {}
print("|".join(str(v if v is not None else "") for v in (
    artifacts.get("clap_installed_sha256") or "",
    "true" if optimizations.get("pgo_applied") else "false",
    "true" if optimizations.get("bolt_applied") else "false",
    optimizations.get("bolt_proof") or "",
    optimizations.get("bolt_profile_sha256") or "",
    optimizations.get("bolt_version") or "",
    optimizations.get("bolt_lbr_samples") or "",
    optimizations.get("bolt_lbr_fdata_sha256") or "",
    ((optimizations.get("codegen") or {}).get("post_bolt") or {}).get("sha256") or "",
    pgo.get("status") or "",
    pgo.get("log_sha256") or "",
    pgo.get("receipt_sha256") or "",
    pgo.get("gain") if pgo.get("gain") is not None else "",
    pgo.get("scenarios") if pgo.get("scenarios") is not None else "",
    pgo.get("callbacks") if pgo.get("callbacks") is not None else "",
    pgo.get("clipping_count_phase1") if pgo.get("clipping_count_phase1") is not None else "",
    pgo.get("clipping_count_bolt") if pgo.get("clipping_count_bolt") is not None else "",
    provenance.get("rustflags") or "",
    provenance.get("git_commit") or "",
    "true" if provenance.get("git_dirty") is True else ("false" if provenance.get("git_dirty") is False else "missing"),
    provenance.get("cargo_lock_sha256") or "",
    provenance.get("rustc_version") or "",
    engine.get("dependency_type") or "",
    engine.get("version") or "",
    engine.get("git_commit") or "",
    engine.get("source_tree_sha256") or "",
    "true" if engine.get("git_dirty") is True else ("false" if engine.get("git_dirty") is False else "missing"),
)))
PY
)
    IFS='|' read -r PRELIM_INSTALLED PRELIM_PGO PRELIM_BOLT PRELIM_PROOF PRELIM_PROFILE PRELIM_VERSION PRELIM_LBR PRELIM_LBR_SHA PRELIM_CODEGEN_SHA PRELIM_PGO_STATUS PRELIM_PGO_LOG_SHA PRELIM_PGO_RECEIPT_SHA PRELIM_PGO_GAIN PRELIM_PGO_SCENARIOS PRELIM_PGO_CALLBACKS PRELIM_PGO_CLIP1 PRELIM_PGO_CLIPBOLT PRELIM_RUSTFLAGS PRELIM_GIT_COMMIT PRELIM_GIT_DIRTY PRELIM_CARGO_LOCK_SHA PRELIM_RUSTC_VER PRELIM_ENGINE_DEP PRELIM_ENGINE_VERSION PRELIM_ENGINE_COMMIT PRELIM_ENGINE_TREE PRELIM_ENGINE_DIRTY <<<"$PRELIM_VALUES"

    if [ "$PRELIM_INSTALLED" != "$CERTIFIED_COPY_SHA" ]; then
        die "F-CER-01: preliminary receipt installed sha (${PRELIM_INSTALLED:-none}) != certified copy sha ($CERTIFIED_COPY_SHA). The artifact changed after the preliminary build."
    fi
    if [ "$PRELIM_PGO" != "true" ] || [ "$PRELIM_BOLT" != "true" ]; then
        die "F-CER-01: preliminary receipt reports pgo_applied=$PRELIM_PGO bolt_applied=$PRELIM_BOLT; a ceremony requires the PGO+BOLT artifact certified by the long suite."
    fi
    if [ -z "$PRELIM_CODEGEN_SHA" ] || [ "$PRELIM_CODEGEN_SHA" != "$PRE_STRIP_ARTIFACT_SHA" ]; then
        die "F-CER-01: codegen-post-bolt sha (${PRE_STRIP_ARTIFACT_SHA:-none}) != preliminary receipt post_bolt sha (${PRELIM_CODEGEN_SHA:-none}). The reused codegen evidence does not belong to the certified build."
    fi
    if [ "$PRELIM_PGO_STATUS" != "SUCCESS" ]; then
        die "F-CER-01: preliminary receipt pgo_workload.status=${PRELIM_PGO_STATUS:-none} (expected SUCCESS); the ceremony requires the PGO evidence produced by the preliminary build."
    fi
    if [ ! -f "$PGO_WORKLOAD_RECEIPT" ] || [ ! -f "$PGO_WORKLOAD_LOG" ]; then
        die "F-CER-01: reused PGO evidence is missing ($PGO_WORKLOAD_RECEIPT / $PGO_WORKLOAD_LOG). Re-run the preliminary build."
    fi
    CURRENT_PGO_LOG_SHA=$(sha256sum "$PGO_WORKLOAD_LOG" | cut -d' ' -f1)
    CURRENT_PGO_RECEIPT_SHA=$(sha256sum "$PGO_WORKLOAD_RECEIPT" | cut -d' ' -f1)
    if [ -z "$PRELIM_PGO_LOG_SHA" ] || [ "$PRELIM_PGO_LOG_SHA" != "$CURRENT_PGO_LOG_SHA" ]; then
        die "F-CER-01: PGO workload log sha (${CURRENT_PGO_LOG_SHA:-none}) != preliminary receipt log sha (${PRELIM_PGO_LOG_SHA:-none}). The reused PGO evidence was modified or is unbound."
    fi
    if [ -z "$PRELIM_PGO_RECEIPT_SHA" ] || [ "$PRELIM_PGO_RECEIPT_SHA" != "$CURRENT_PGO_RECEIPT_SHA" ]; then
        die "F-CER-01: PGO workload receipt sha (${CURRENT_PGO_RECEIPT_SHA:-none}) != preliminary receipt receipt_sha256 (${PRELIM_PGO_RECEIPT_SHA:-none}). The reused PGO receipt is modified or unbound."
    fi

    # Source provenance: the bytes must have been built from the checkout that
    # is being certified. A clean tree alone is not enough (commit A artifact +
    # clean checkout B would otherwise be certified as B).
    CURRENT_HEAD=$(git rev-parse HEAD 2>/dev/null || echo "")
    CURRENT_LOCK_SHA=$(sha256sum Cargo.lock 2>/dev/null | cut -d' ' -f1 || echo "")
    CURRENT_RUSTC_VER=$(rustc --version 2>/dev/null || echo "")
    EXPECTED_ENGINE_DEP="crates_io"
    EXPECTED_ENGINE_COMMIT=""
    if [ -n "$CORE_PATH" ]; then
        EXPECTED_ENGINE_DEP="path_patch"
        [ "$CORE_GIT_COMMIT" != "unknown" ] && EXPECTED_ENGINE_COMMIT="$CORE_GIT_COMMIT"
    fi
    if [ "$PRELIM_GIT_COMMIT" != "$CURRENT_HEAD" ] || [ "$PRELIM_GIT_DIRTY" != "false" ]; then
        die "F-CER-01: preliminary build source (commit=${PRELIM_GIT_COMMIT:-none}, dirty=$PRELIM_GIT_DIRTY) does not match the certified checkout (commit=${CURRENT_HEAD:-none}, dirty=false). Rebuild the preliminary artifact from the checkout being certified."
    fi
    if [ "$PRELIM_CARGO_LOCK_SHA" != "$CURRENT_LOCK_SHA" ] || [ "$PRELIM_RUSTC_VER" != "$CURRENT_RUSTC_VER" ]; then
        die "F-CER-01: preliminary build toolchain/lock differs from the current checkout (cargo.lock or rustc version changed). Rebuild the preliminary artifact."
    fi
    if [ "$PRELIM_ENGINE_DEP" != "$EXPECTED_ENGINE_DEP" ] \
        || [ "$PRELIM_ENGINE_VERSION" != "$CORE_VERSION" ] \
        || [ "$PRELIM_ENGINE_COMMIT" != "$EXPECTED_ENGINE_COMMIT" ] \
        || [ "$PRELIM_ENGINE_TREE" != "$CORE_TREE_SHA" ] \
        || [ "$PRELIM_ENGINE_DIRTY" != "${CORE_GIT_DIRTY:-false}" ]; then
        die "F-CER-01: preliminary engine provenance does not match the current dependency. Rebuild the preliminary artifact."
    fi

    REUSED_CERTIFIED_BINARY=true
    REUSED_BOLT_APPLIED="$PRELIM_BOLT"
    REUSED_BOLT_PROOF="$PRELIM_PROOF"
    REUSED_BOLT_PROFILE_SHA="$PRELIM_PROFILE"
    REUSED_BOLT_VERSION="$PRELIM_VERSION"
    REUSED_BOLT_LBR_SAMPLES="$PRELIM_LBR"
    REUSED_BOLT_LBR_FDATA_SHA256="$PRELIM_LBR_SHA"
    REUSED_PGO_STATUS="$PRELIM_PGO_STATUS"
    REUSED_PGO_GAIN="$PRELIM_PGO_GAIN"
    REUSED_PGO_SCENARIOS="$PRELIM_PGO_SCENARIOS"
    REUSED_PGO_CALLBACKS="$PRELIM_PGO_CALLBACKS"
    REUSED_PGO_LOG_SHA="$PRELIM_PGO_LOG_SHA"
    REUSED_PGO_RECEIPT_SHA="$PRELIM_PGO_RECEIPT_SHA"
    REUSED_PGO_CLIPPING_PHASE1="${PRELIM_PGO_CLIP1:-0}"
    REUSED_PGO_CLIPPING_BOLT="${PRELIM_PGO_CLIPBOLT:-0}"
    PGO_WORKLOAD_RAN_PHASE1=true
    PGO_WORKLOAD_RAN_BOLT=true
    [ -n "$PRELIM_RUSTFLAGS" ] && CLAP_RUSTFLAGS="$PRELIM_RUSTFLAGS"
    echo -e "  ${GREEN}✓${NC} Certified artifact chain verified: installed == certified copy == long receipt (sha256: ${CERTIFIED_COPY_SHA:0:16}...)."
    echo -e "  ${GREEN}✓${NC} Reusing preliminary PGO/BOLT + codegen evidence from commit ${PRELIM_GIT_COMMIT:0:12}; Phases 2-4 will not rebuild."
fi

# -----------------------------------------------------------------------------
# PHASE 2: Profile-Guided Optimization (PGO) - Profiling Workload
# -----------------------------------------------------------------------------
if [ "$REUSED_CERTIFIED_BINARY" = "true" ]; then
    echo -e "\n${BLUE}${BOLD}[Phase 2/7] Reusing certified artifact — PGO profiling skipped (F-CER-01).${NC}"
elif [ "$USE_PGO" = true ]; then
    echo -e "\n${BLUE}${BOLD}[Phase 2/7] Generating PGO profiles via workload runner...${NC}"

    # Allocate sufficient static value profile counters (LLVM default is 2, which exhausts on polymorphic DSP topologies).
    # Prevents "LLVM Profile Warning: Unable to track new values: Running out of static counters" and preserves indirect call profile fidelity.
    PGO_GEN_RUSTFLAGS="$CONFIG_RUSTFLAGS $ORIG_RUSTFLAGS -Cprofile-generate=$PROFRAW_DIR -Cllvm-args=-vp-counters-per-site=8"
    echo -e "  Using RUSTFLAGS: ${BOLD}$PGO_GEN_RUSTFLAGS${NC}"

    echo -e "  Compiling real-world PGO profiling workload (pgo_profiling_workload)..."
    RUSTFLAGS="$PGO_GEN_RUSTFLAGS" cargo build --locked --profile dist --features testing --bin pgo_profiling_workload || {
        echo -e "${RED}Error: Failed to build pgo_profiling_workload for PGO profiling.${NC}"
        exit 1
    }

    echo -e "  Executing PGO profiling workload..."
    mkdir -p "$PROJECT_DIR/target/logs"
    LLVM_PROFILE_FILE="$PROFRAW_DIR/default_%m_%p.profraw" timeout 60 "$PGO_BUILD_TARGET_DIR/dist/pgo_profiling_workload" 2>&1 | tee "$PGO_WORKLOAD_LOG" || {
        echo -e "${RED}Error: pgo_profiling_workload failed (or timed out after 60s). Cannot generate PGO profiles.${NC}"
        exit 1
    }
    PGO_WORKLOAD_RAN_PHASE1=true
    PGO_CLIPPING_PHASE1=$(grep -c "Output clipping" "$PGO_WORKLOAD_LOG" || true)
    if [ "${PGO_CLIPPING_PHASE1:-0}" -gt 0 ]; then
        # A profile trained on clipped output is not representative of the
        # shipped signal path: fail closed in strict/ceremony mode and keep a
        # typed gap otherwise (never a silent WARN-only pass).
        if [ "$STRICT_MODE" = "1" ] || [ "$RELEASE_CEREMONY" = "true" ]; then
            die "PGO workload reported $PGO_CLIPPING_PHASE1 output-clipping warning(s); refusing to train/certify in a clipping regime (see $PGO_WORKLOAD_LOG)."
        fi
        GATE_SKIPS+=("pgo_workload:clipping")
        warn "PGO workload reported $PGO_CLIPPING_PHASE1 output-clipping warning(s); the profile trains in a clipping regime (typed gap pgo_workload:clipping; see $PGO_WORKLOAD_LOG)."
    else
        echo -e "  ${GREEN}✓${NC} PGO workload profile is clipping-free (grep -c \"Output clipping\" = 0)."
    fi

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

if [ "$REUSED_CERTIFIED_BINARY" = "true" ]; then
    echo -e "  ${YELLOW}Reusing certified artifact — CLAP compilation skipped (F-CER-01).${NC}"
    # Provenance is taken from the preliminary receipt below this block; keep a
    # syntactically valid flag string even if that receipt is incomplete.
    [ -n "${CLAP_RUSTFLAGS:-}" ] || CLAP_RUSTFLAGS="$CONFIG_RUSTFLAGS $ORIG_RUSTFLAGS -Clink-arg=-Wl,-q -Clink-arg=-Wl,-soname,nam_plug.clap"
else
    CLAP_BASE_RUSTFLAGS="$CONFIG_RUSTFLAGS $ORIG_RUSTFLAGS"
    if [ "$USE_PGO" = true ] && [ -f "$MERGED_PROFILE" ]; then
        CLAP_BASE_RUSTFLAGS="$CLAP_BASE_RUSTFLAGS -Cprofile-use=$MERGED_PROFILE"
    else
        echo -e "  ${YELLOW}Compiling without PGO profile.${NC}"
    fi

    CLAP_RUSTFLAGS="$CLAP_BASE_RUSTFLAGS -Clink-arg=-Wl,-q -Clink-arg=-Wl,-soname,nam_plug.clap"
    echo -e "  Using RUSTFLAGS (CLAP): ${BOLD}$CLAP_RUSTFLAGS${NC}"
    RUSTFLAGS="$CLAP_RUSTFLAGS" cargo build --locked --profile dist --target-dir "$PGO_CLAP_TARGET_DIR" --lib

    # Confirm binary compiled
    if [ ! -f "$PGO_CLAP_TARGET_DIR/dist/libnam_plug.so" ]; then
        echo -e "${RED}Error: Failed to find compiled CLAP plugin library at $PGO_CLAP_TARGET_DIR/dist/libnam_plug.so${NC}"
        exit 1
    fi
fi
echo -e "  ${GREEN}✓${NC} Compilation step satisfied (compiled or certified artifact reused)."

# Ensure RUSTFLAGS and LLVM_PROFILE_FILE do not leak into downstream phases or verification gates
unset RUSTFLAGS LLVM_PROFILE_FILE 2>/dev/null || true

# -----------------------------------------------------------------------------
# PHASE 4: BOLT Instrumentation & Post-Link Optimization
# -----------------------------------------------------------------------------
CLAP_BOLT_APPLIED=false
BOLT_PROFILE_SHA=""
BOLT_VERSION=""

if [ "$REUSED_CERTIFIED_BINARY" = "true" ]; then
    echo -e "\n${BLUE}${BOLD}[Phase 4/7] Reusing certified artifact — BOLT optimization skipped (F-CER-01).${NC}"
    CLAP_BOLT_APPLIED="$REUSED_BOLT_APPLIED"
elif [ "$USE_BOLT" = true ] && [ -n "$LLVM_BOLT" ]; then
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
        echo -e "  Recompiling workload runner without PGO instrumentation (linking with LTO; please wait)..."

        # Recompile workload runner & performance guard without PGO instrumentation for clean profiling/gates
        RUSTFLAGS="$CONFIG_RUSTFLAGS $ORIG_RUSTFLAGS" \
            cargo build --locked --profile dist --features testing --bin pgo_profiling_workload --bin nam_perf_guard
        unset RUSTFLAGS 2>/dev/null || true

        # Clean any stale fdata files and previous workload receipt before profiling
        rm -f "$PGO_CLAP_TARGET_DIR"/libnam_plug.fdata.* "$BOLT_DIR/libnam_plug.merged.fdata" "$PROJECT_DIR/target/pgo-workload-receipt.json"

        # Append the BOLT run to the same evidence log so a single SHA covers
        # both training phases. The run is checked for clipping exactly like
        # the PGO phase (fail-closed in strict/ceremony mode).
        mkdir -p "$(dirname "$PGO_WORKLOAD_LOG")"
        # Without the PGO phase, discard any stale log so the BOLT clipping
        # count cannot be inflated by a previous run.
        if [ "$PGO_WORKLOAD_RAN_PHASE1" != true ]; then
            : > "$PGO_WORKLOAD_LOG"
        fi
        set +e
        NAM_CLAP_SO_PATH="$PGO_CLAP_TARGET_DIR/dist/libnam_plug.instrumented.so" \
            "$PGO_BUILD_TARGET_DIR/dist/pgo_profiling_workload" 2>&1 | tee -a "$PGO_WORKLOAD_LOG"
        BOLT_WORKLOAD_RC=${PIPESTATUS[0]}
        set -e
        PGO_WORKLOAD_RAN_BOLT=true
        PGO_CLIPPING_TOTAL=$(grep -c "Output clipping" "$PGO_WORKLOAD_LOG" 2>/dev/null || true)
        PGO_CLIPPING_BOLT=$(( ${PGO_CLIPPING_TOTAL:-0} - ${PGO_CLIPPING_PHASE1:-0} ))
        if [ "${PGO_CLIPPING_BOLT:-0}" -lt 0 ]; then
            PGO_CLIPPING_BOLT=0
        fi

        if [ "$BOLT_WORKLOAD_RC" -eq 0 ]; then

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

        if [ "${PGO_CLIPPING_BOLT:-0}" -gt 0 ]; then
            if [ "$STRICT_MODE" = "1" ] || [ "$RELEASE_CEREMONY" = "true" ]; then
                die "BOLT workload reported $PGO_CLIPPING_BOLT output-clipping warning(s); refusing to certify in a clipping regime (see $PGO_WORKLOAD_LOG)."
            fi
            GATE_SKIPS+=("pgo_workload:clipping")
            warn "BOLT workload reported $PGO_CLIPPING_BOLT output-clipping warning(s); typed gap pgo_workload:clipping (see $PGO_WORKLOAD_LOG)."
        else
            echo -e "  ${GREEN}✓${NC} BOLT workload profile is clipping-free (grep -c \"Output clipping\" delta = 0)."
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

# A claimed BOLT application must be backed by the optimized artifact. Losing
# the file between phases would otherwise silently ship the pre-BOLT `.so`
# while the receipt says `bolt_applied: true`.
if [ "$REUSED_CERTIFIED_BINARY" != "true" ] && [ "${CLAP_BOLT_APPLIED:-false}" = true ] && [ ! -s "$PGO_CLAP_TARGET_DIR/dist/libnam_plug.bolt.so" ]; then
    die "CLAP_BOLT_APPLIED=true but the optimized artifact is missing: $PGO_CLAP_TARGET_DIR/dist/libnam_plug.bolt.so (fail-closed)."
fi

# -----------------------------------------------------------------------------
# PHASE 4.1: BOLT ceremony proof
# -----------------------------------------------------------------------------
# A `PGO+BOLT` *win* must be independently proven. Only the canonical
# `ab-opt-ceremony-receipt.json` (verdict `PGO+BOLT`, >= 3 runs, positive
# measured gain) is accepted as `ab-opt-ceremony:<id>`. This build's measured
# performance gate is recorded as `gate-pass-only`: it proves the shipped binary
# meets the deadline envelope, not that BOLT beat PGO.
# `bolt_profile_sha256` alone only proves a profile existed, not that the
# transform won. Per-variant or stale `ab-opt-*.json` files are never accepted.
BOLT_PROOF="MISSING"
BOLT_LBR_SAMPLES=""
BOLT_LBR_FDATA_SHA256=""
CEREMONY_RECEIPT="$PROJECT_DIR/target/logs/ab-opt-ceremony-receipt.json"
if [ "$REUSED_CERTIFIED_BINARY" = "true" ]; then
    BOLT_PROOF="${REUSED_BOLT_PROOF:-MISSING}"
    BOLT_LBR_SAMPLES="$REUSED_BOLT_LBR_SAMPLES"
    BOLT_LBR_FDATA_SHA256="$REUSED_BOLT_LBR_FDATA_SHA256"
    BOLT_PROFILE_SHA="$REUSED_BOLT_PROFILE_SHA"
    BOLT_VERSION="$REUSED_BOLT_VERSION"
    if [ "$BOLT_PROOF" = "MISSING" ]; then
        warn "Preliminary receipt carries no BOLT proof; the ceremony cannot certify (typed gap bolt_proof:missing_ceremony)."
    else
        echo -e "  ${GREEN}✓${NC} BOLT proof reused from preliminary receipt: $BOLT_PROOF"
    fi
elif [ -f "$CEREMONY_RECEIPT" ]; then
    read -r proof_id ceremony_lbr ceremony_sha <<<"$(python3 -c '
import hashlib, json, sys
path = sys.argv[1]
try:
    with open(path, "r", encoding="utf-8") as fh:
        doc = json.load(fh)
except Exception:
    sys.exit(0)
if doc.get("verdict") != "PGO+BOLT":
    sys.exit(0)
try:
    runs = int(doc.get("runs") or 0)
except (TypeError, ValueError):
    runs = 0
if runs < 3:
    sys.exit(0)
# A ceremony may report a positive-gain confidence interval (`gain_ci_low > 0`)
# or the canonical `bolt_gain_pct.mean` (negative percent = improvement).
gain_ci = doc.get("gain_ci_low")
try:
    if gain_ci is not None:
        if float(gain_ci) <= 0.0:
            sys.exit(0)
    else:
        gain = (doc.get("bolt_gain_pct") or {}).get("mean")
        if gain is None or float(gain) >= 0.0:
            sys.exit(0)
except (TypeError, ValueError):
    sys.exit(0)
with open(path, "rb") as fh:
    digest = hashlib.sha256(fh.read()).hexdigest()
lbr = doc.get("bolt_lbr") or {}
print(digest[:16], lbr.get("samples") or "", lbr.get("fdata_sha256") or "")
' "$CEREMONY_RECEIPT" 2>/dev/null || true)"
    if [ -n "$proof_id" ]; then
        BOLT_PROOF="ab-opt-ceremony:$proof_id"
        BOLT_LBR_SAMPLES="$ceremony_lbr"
        BOLT_LBR_FDATA_SHA256="$ceremony_sha"
        echo -e "  ${GREEN}✓${NC} BOLT ceremony proof: $BOLT_PROOF (verdict PGO+BOLT, >=3 runs, positive measured gain)"
    else
        warn "Ceremony receipt present but does not prove a BOLT win; BOLT proof MISSING."
    fi
else
    warn "No canonical ceremony receipt at $CEREMONY_RECEIPT; BOLT win unproven (gate-pass label may apply)."
fi

# -----------------------------------------------------------------------------
# PHASE 4.5: Assembly Hotspot Disassembly Report
# -----------------------------------------------------------------------------
echo -e "\n${BLUE}${BOLD}[Phase 4.5/7] Generating AI-ready assembly hotspot report...${NC}"

ASM_TARGET="$PROJECT_DIR/target/dsp_hotpath.asm"
mkdir -p "$PROJECT_DIR/target"
mkdir -p "$PROJECT_DIR/target/logs"
CODEGEN_COMPARISON="not_run"
if [ "$REUSED_CERTIFIED_BINARY" = "true" ]; then
    CODEGEN_COMPARISON="$(codegen_compare_verdict "$CODEGEN_PRE_JSON" "$CODEGEN_POST_JSON")"
else
    rm -f "$CODEGEN_PRE_JSON" "$CODEGEN_POST_JSON"
fi

if [ "${CLAP_BOLT_APPLIED:-false}" = true ] && [ -f "$PGO_CLAP_TARGET_DIR/dist/libnam_plug.bolt.so" ]; then
    ASM_BIN="$PGO_CLAP_TARGET_DIR/dist/libnam_plug.bolt.so"
elif [ -f "$PGO_CLAP_TARGET_DIR/dist/libnam_plug.so" ]; then
    ASM_BIN="$PGO_CLAP_TARGET_DIR/dist/libnam_plug.so"
fi

if [ "$REUSED_CERTIFIED_BINARY" = "true" ]; then
    ok "Reused pre/post-BOLT codegen evidence (comparison: $CODEGEN_COMPARISON; pre-strip sha256: ${PRE_STRIP_ARTIFACT_SHA:0:16}...)."
elif [ -n "${ASM_BIN:-}" ]; then
    OBJDUMP_TOOL=""
    if command -v llvm-objdump &>/dev/null; then
        OBJDUMP_TOOL="llvm-objdump"
    elif command -v objdump &>/dev/null; then
        OBJDUMP_TOOL="objdump"
    fi

    if [ -n "$OBJDUMP_TOOL" ]; then
        python3 - "$ASM_BIN" "$ASM_TARGET" "$OBJDUMP_TOOL" << 'PYEOF'
import sys, re, subprocess, os, hashlib

asm_bin = sys.argv[1]
asm_target = sys.argv[2]
objdump_tool = sys.argv[3]

bin_sha = "unknown"
try:
    with open(asm_bin, "rb") as bf:
        bin_sha = hashlib.sha256(bf.read()).hexdigest()
except Exception:
    pass

# Disassemble the active .text section (bypassing redundant pre-BOLT backup sections)
cmd = [objdump_tool, "-d", "--demangle", "--no-show-raw-insn", "-j", ".text", asm_bin]
try:
    proc = subprocess.Popen(
        cmd,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        env=dict(os.environ, LC_ALL="C")
    )
except Exception:
    cmd = [objdump_tool, "-d", "--demangle", "--no-show-raw-insn", asm_bin]
    proc = subprocess.Popen(
        cmd,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        env=dict(os.environ, LC_ALL="C")
    )

func_re = re.compile(r"^[0-9a-fA-F]+\s+<(.*)>:\s*$")
include_re = re.compile(r"(neural_amp_modeler_rs::(models|math|dsp)|nam_plug::clap::processor)")
exclude_re = re.compile(r"(drop_glue|fmt::|serde|deserialize|serialize|Display|Debug)")

captured_funcs = 0
captured_lines = 0
body_lines = []
is_capturing = False

for line in proc.stdout:
    m = func_re.match(line)
    if m:
        sym = m.group(1)
        if include_re.search(sym) and not exclude_re.search(sym):
            is_capturing = True
            captured_funcs += 1
        else:
            is_capturing = False
    if is_capturing:
        body_lines.append(line)
        captured_lines += 1

proc.wait()

# Fail-safe: if symbol filtering matched zero lines, fallback to raw disassembly
if captured_lines == 0:
    cmd_fallback = [objdump_tool, "-d", "--demangle", "--no-show-raw-insn", "-j", ".text", asm_bin]
    try:
        raw_out = subprocess.check_output(cmd_fallback, stderr=subprocess.DEVNULL, text=True, env=dict(os.environ, LC_ALL="C"))
        body_lines = [raw_out]
    except Exception:
        pass

with open(asm_target, "w") as out:
    out.write("# =========================================================================\n")
    out.write("# NAM-Plug AI-Ready DSP Hotpath Disassembly Report\n")
    out.write(f"# Binary:           {os.path.basename(asm_bin)}\n")
    out.write(f"# SHA-256:          {bin_sha}\n")
    out.write("# Architecture:     x86-64-v3 (AVX2, FMA, BMI2)\n")
    out.write("# Captured Scope:   neural_amp_modeler_rs::{models, math, dsp}, nam_plug::clap::processor\n")
    out.write("# Excluded Noise:   cold runtime & formatting (drop_glue, serde, fmt)\n")
    out.write(f"# Hot Functions:    {captured_funcs}\n")
    out.write(f"# Captured Lines:   {captured_lines}\n")
    out.write("# =========================================================================\n\n")
    out.writelines(body_lines)
PYEOF
    fi

    if [ -s "$ASM_TARGET" ]; then
        ASM_LINES=$(wc -l < "$ASM_TARGET")
        ASM_SIZE=$(du -h "$ASM_TARGET" | cut -f1)
        echo -e "  ${GREEN}✓${NC} Assembly hotspot report generated at target/dsp_hotpath.asm (${ASM_LINES} lines, ${ASM_SIZE})"
    else
        echo -e "  ${YELLOW}Warning: Assembly disassembly failed or produced empty output.${NC}"
    fi

    if [ -x "$SCRIPT_DIR/verify_rt_codegen.sh" ]; then
        if [ "${CLAP_BOLT_APPLIED:-false}" = true ] && [ -f "$PGO_CLAP_TARGET_DIR/dist/libnam_plug.so" ]; then
            echo -e "  ${BLUE}→ Enforcing real-time codegen static invariants on pre-BOLT libnam_plug.so...${NC}"
            if ! "$SCRIPT_DIR/verify_rt_codegen.sh" "$PGO_CLAP_TARGET_DIR/dist/libnam_plug.so" \
                --json-out "$CODEGEN_PRE_JSON"; then
                if [ "$STRICT_MODE" = "1" ] || [ "$RELEASE_CEREMONY" = "true" ]; then
                    die "RT codegen static gate failed on pre-BOLT artifact!"
                else
                    warn "RT codegen static gate reported violations on pre-BOLT artifact (non-strict mode)."
                fi
            fi
        fi
        echo -e "  ${BLUE}→ Enforcing real-time codegen static invariants on $(basename "$ASM_BIN")...${NC}"
        if ! "$SCRIPT_DIR/verify_rt_codegen.sh" "$ASM_BIN" --json-out "$CODEGEN_POST_JSON"; then
            if [ "$STRICT_MODE" = "1" ] || [ "$RELEASE_CEREMONY" = "true" ]; then
                die "RT codegen static gate failed on release artifact ($ASM_BIN)!"
            else
                warn "RT codegen static gate reported violations (non-strict mode)."
            fi
        fi

        # Post-BOLT coverage must not shrink silently: BOLT reorders functions and
        # keeps cold/unmoved bodies in `.bolt.org.text`, so accepting suffixes and
        # disassembling per symbol is what makes the post-link check meaningful.
        CODEGEN_COMPARISON="$(codegen_compare_verdict "$CODEGEN_PRE_JSON" "$CODEGEN_POST_JSON")"
        case "$CODEGEN_COMPARISON" in
            match)
                PRE_SUMMARY="$(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print("{} symbols/{} instr, {}/{} targets".format(d["symbols"], d["instructions"], d["targets_matched"], d["targets_total"]))' "$CODEGEN_PRE_JSON")"
                POST_SUMMARY="$(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print("{} symbols/{} instr, {}/{} targets".format(d["symbols"], d["instructions"], d["targets_matched"], d["targets_total"]))' "$CODEGEN_POST_JSON")"
                ok "Post-BOLT codegen coverage matches pre-BOLT (pre: ${PRE_SUMMARY}; post: ${POST_SUMMARY})."
                ;;
            post_has_fewer_*)
                if [ "$STRICT_MODE" = "1" ] || [ "$RELEASE_CEREMONY" = "true" ]; then
                    die "Post-BOLT codegen coverage shrank ($CODEGEN_COMPARISON): the shipped binary lost hot-path symbols/targets verified before BOLT."
                fi
                GATE_SKIPS+=("codegen:$CODEGEN_COMPARISON")
                warn "Post-BOLT codegen coverage shrank ($CODEGEN_COMPARISON) — typed gap recorded (non-strict mode)."
                ;;
            pre_only)
                warn "No post-BOLT codegen metrics (BOLT not applied); nothing to compare against the pre-BOLT artifact."
                ;;
            post_only)
                warn "No pre-BOLT codegen metrics; post-BOLT coverage cannot be compared (typed provenance gap)."
                GATE_SKIPS+=("codegen:pre_metrics_absent")
                ;;
            absent)
                warn "Codegen metrics absent for both artifacts; post-BOLT coverage comparison skipped."
                GATE_SKIPS+=("codegen:metrics_absent")
                ;;
            *)
                warn "Unexpected codegen comparison verdict '$CODEGEN_COMPARISON'; recording typed gap."
                GATE_SKIPS+=("codegen:comparison_unknown")
                ;;
        esac

        # S3.5: the post-BOLT regression script must be reachable from the
        # codegen driver, not only from a manual invocation. It re-verifies the
        # real pre/post pair and the synthetic fail-closed case.
        if [ "${CLAP_BOLT_APPLIED:-false}" = true ]; then
            echo -e "  ${BLUE}→ Running post-BOLT codegen regression (real pre/post pair + synthetic removal)...${NC}"
            if ! "$SCRIPT_DIR/verify_rt_codegen.sh" "$ASM_BIN" --post-bolt-regression \
                > "$PROJECT_DIR/target/logs/post-bolt-codegen-regression.log" 2>&1; then
                if [ "$STRICT_MODE" = "1" ] || [ "$RELEASE_CEREMONY" = "true" ]; then
                    die "Post-BOLT codegen regression failed (see target/logs/post-bolt-codegen-regression.log)."
                fi
                GATE_SKIPS+=("codegen:post_bolt_regression")
                warn "Post-BOLT codegen regression failed; typed gap codegen:post_bolt_regression."
            else
                ok "Post-BOLT codegen regression passed (real pre/post pair + synthetic removal)."
            fi
        fi
    fi
else
    echo -e "  ${YELLOW}Warning: No optimized binary found for disassembly.${NC}"
fi

# Record the pre-strip BOLT identity alongside the post-strip installed bytes.
# The preliminary receipt must carry both; only the installed (post-strip) SHA
# is compared with the long-audit receipt.
if [ -f "$CODEGEN_POST_JSON" ]; then
    CODEGEN_POST_SHA=$(codegen_post_sha "$CODEGEN_POST_JSON")
    [ -n "$CODEGEN_POST_SHA" ] && PRE_STRIP_ARTIFACT_SHA="$CODEGEN_POST_SHA"
fi

# -----------------------------------------------------------------------------
# PHASE 5: Deliverables Installation & Strict Certification of Shipped Bytes
# -----------------------------------------------------------------------------
echo -e "\n${BLUE}${BOLD}[Phase 5/7] Installing and strictly certifying distributed artifact...${NC}"

if [ "$REUSED_CERTIFIED_BINARY" = "true" ]; then
    if [ ! -f "$CLAP_TARGET" ]; then
        die "F-CER-01: certified artifact is missing at $CLAP_TARGET; the ceremony refuses to rebuild."
    fi
    if ! cmp -s "$CLAP_TARGET" "$CERTIFIED_CLAP"; then
        die "F-CER-01: installed artifact diverged from the certified copy before certification gates."
    fi
    echo -e "  ${GREEN}✓${NC} Reusing installed certified artifact (no rebuild): $CLAP_TARGET"
else
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

    # T3.1: capture the exact installed bytes for the later ceremony. The copy
    # lives outside target/logs and outside target/pgo-* so the ceremony can
    # prove identity instead of rebuilding (F-CER-01).
    mkdir -p "$CERTIFIED_DIR"
    cp "$CLAP_TARGET" "$CERTIFIED_CLAP"
    if ! cmp -s "$CLAP_TARGET" "$CERTIFIED_CLAP"; then
        die "F-CER-01: certified copy is not byte-identical to the installed artifact ($CERTIFIED_CLAP)."
    fi
    sha256sum "$CERTIFIED_CLAP" | cut -d' ' -f1 > "$CERTIFIED_CLAP_SHA_FILE"
    CERTIFIED_COPY_SHA=$(sha256sum "$CERTIFIED_CLAP" | cut -d' ' -f1)
    echo -e "  ${GREEN}✓${NC} Certified artifact captured: $CERTIFIED_CLAP (sha256: ${CERTIFIED_COPY_SHA:0:16}...)"
fi

FINAL_CLAP_SHA=$(sha256sum "$CLAP_TARGET" | cut -d' ' -f1)
if [ -n "$CERTIFIED_COPY_SHA" ] && [ "$FINAL_CLAP_SHA" != "$CERTIFIED_COPY_SHA" ]; then
    die "F-CER-01: installed sha256 ($FINAL_CLAP_SHA) != certified copy sha256 ($CERTIFIED_COPY_SHA)."
fi
echo -e "  ${CYAN}Distributed CLAP artifact SHA-256:${NC} ${BOLD}$FINAL_CLAP_SHA${NC}"

# Gate 1: Symbols and SONAME on distributed artifact
echo -e "  [Gate 1/5] Validating exported symbols and SONAME on distributed artifact..."
if ! nm -D "$CLAP_TARGET" | grep -w "clap_entry" > /dev/null; then
    die "Missing 'clap_entry' symbol in distributed CLAP artifact!"
fi
if ! readelf -d "$CLAP_TARGET" | grep SONAME >/dev/null; then
    die "Missing SONAME in distributed CLAP artifact!"
fi
ok "Symbol and SONAME validation passed."

# Gate 2: External clap-validator
echo -e "  [Gate 2/5] Running external clap-validator against distributed artifact..."
if command -v clap-validator >/dev/null 2>&1; then
    CLAP_VALIDATOR_LOG="$PROJECT_DIR/target/logs/clap-validator.log"
    mkdir -p "$(dirname "$CLAP_VALIDATOR_LOG")"
    set +e
    clap-validator validate "$CLAP_TARGET" 2>&1 | tee "$CLAP_VALIDATOR_LOG"
    CLAP_VALIDATOR_RC=${PIPESTATUS[0]}
    set -e
    [ "$CLAP_VALIDATOR_RC" -eq 0 ] || die "clap-validator rejected the distribution artifact!"

    # clap-validator prints "<N> tests run, <P> passed, <F> failed, <S> skipped".
    # Parse the last summary line so the receipt carries the exact counts
    # instead of only the process exit code.
    CLAP_VALIDATOR_SUMMARY=$(grep -E 'tests run, .* passed, .* failed, .* skipped' "$CLAP_VALIDATOR_LOG" | tail -n 1 || true)
    CLAP_VALIDATOR_TESTS_RUN=$(printf '%s\n' "$CLAP_VALIDATOR_SUMMARY" | sed -n 's/^[[:space:]]*\([0-9][0-9]*\) tests run.*/\1/p')
    CLAP_VALIDATOR_PASSED=$(printf '%s\n' "$CLAP_VALIDATOR_SUMMARY" | sed -n 's/.*tests run, \([0-9][0-9]*\) passed.*/\1/p')
    CLAP_VALIDATOR_FAILED=$(printf '%s\n' "$CLAP_VALIDATOR_SUMMARY" | sed -n 's/.* passed, \([0-9][0-9]*\) failed.*/\1/p')
    CLAP_VALIDATOR_SKIPPED=$(printf '%s\n' "$CLAP_VALIDATOR_SUMMARY" | sed -n 's/.* failed, \([0-9][0-9]*\) skipped.*/\1/p')
    CLAP_VALIDATOR_SKIP_REASONS=$(grep -oE 'SKIPPED: .*' "$CLAP_VALIDATOR_LOG" \
        | sed 's/^SKIPPED: //' | sort -u | paste -sd ';' - || true)
    CLAP_VALIDATOR_STATUS="PASSED"
    if [ -n "$CLAP_VALIDATOR_SKIPPED" ] && [ "$CLAP_VALIDATOR_SKIPPED" != "0" ]; then
        warn "clap-validator skipped $CLAP_VALIDATOR_SKIPPED test(s): $CLAP_VALIDATOR_SKIP_REASONS"
    fi
    ok "clap-validator passed${CLAP_VALIDATOR_SUMMARY:+ ($CLAP_VALIDATOR_SUMMARY)}."
else
    if [ "$STRICT_MODE" = "1" ]; then
        die "clap-validator unavailable in strict release mode!"
    fi
    warn "clap-validator unavailable. Skipping external validation."
    GATE_SKIPS+=("clap_validator:unavailable")
    CLAP_VALIDATOR_STATUS="UNAVAILABLE"
fi

# Gate 3: NAMCore float parity test against distributed artifact
model_fixture=""
if [ -n "${NAM_FIXTURES_DIR:-}" ] && [ -f "$NAM_FIXTURES_DIR/wavenet_a1_standard.nam" ]; then
    model_fixture="$NAM_FIXTURES_DIR/wavenet_a1_standard.nam"
elif [ -f "tests/fixtures/models/wavenet_a1_standard.nam" ]; then
    model_fixture="tests/fixtures/models/wavenet_a1_standard.nam"
fi

ORACLE_BIN=""
ORACLE_SHA=""
FIXTURE_SHA=""
if [ -n "$model_fixture" ] && [ -f "$model_fixture" ]; then
    FIXTURE_SHA=$(sha256sum "$model_fixture" | cut -d' ' -f1)
fi

# Pre-build release integration test binaries without runtime timeout.
# Cold release compilation (fat-LTO / 600+ targets) can take several minutes;
# pre-building with --no-run ensures the 600s timeout in Gate 3 and Gate 4
# guards only the test execution, measuring test behavior rather than the compiler.
echo -e "  Pre-building release integration tests (clap) before certification gates..."
cargo test --features testing --release --test clap --no-run

# Gate 3: NAMCore float parity test against distributed artifact
echo -e "  [Gate 3/5] Running NAMCore float parity test against distributed artifact..."
if ORACLE_BIN=$(find_namcore_render); then
    if [ -n "$model_fixture" ]; then
        ORACLE_SHA=$(sha256sum "$ORACLE_BIN" | cut -d' ' -f1)
        echo -e "  ${BLUE}→ Parity oracle found:${NC} $ORACLE_BIN (sha256: ${ORACLE_SHA:0:16}...)"
        NAM_REQUIRE_CPP_ORACLE=1 CLAP_PLUGIN_UNDER_TEST="$CLAP_TARGET" \
            timeout 600 cargo test --features testing --release --test clap \
            test_clap_parity_multi_rate -- --ignored --nocapture
        ok "NAMCore float parity certified on distributed artifact."

        # Gate 3b: artifact-level oversampling parity (2x/4x) on the exact
        # installed bytes. Same oracle/artifact wiring as the matrix above.
        echo -e "  [Gate 3b] Running artifact oversampling parity (2x/4x) against distributed artifact..."
        if NAM_REQUIRE_CPP_ORACLE=1 CLAP_PLUGIN_UNDER_TEST="$CLAP_TARGET" \
            timeout 600 cargo test --features testing --release --test clap \
            test_clap_parity_oversample -- --ignored --nocapture; then
            PARITY_OVERSAMPLE_STATUS="PASS"
            ok "Artifact oversampling parity (2x/4x) certified on distributed artifact."
        else
            PARITY_OVERSAMPLE_STATUS="FAIL"
            PARITY_OVERSAMPLE_REASON="divergence"
            if [ "$STRICT_MODE" = "1" ] || [ "$RELEASE_CEREMONY" = "true" ]; then
                die "Artifact oversampling parity failed on the distributed artifact (NAMCore divergence)."
            fi
            GATE_SKIPS+=("parity_oversample:divergence")
            warn "Artifact oversampling parity diverged from NAMCore; typed gap parity_oversample:divergence."
        fi
    else
        if [ "$RELEASE_CEREMONY" = "true" ]; then
            die "Model fixture tests/fixtures/models/wavenet_a1_standard.nam missing in release ceremony mode!"
        fi
        warn "Model fixture missing. Skipping parity test."
        GATE_SKIPS+=("namcore_parity:missing_model_fixture")
        PARITY_OVERSAMPLE_STATUS="SKIP"
        PARITY_OVERSAMPLE_REASON="missing_model_fixture"
    fi
else
    if [ "$RELEASE_CEREMONY" = "true" ]; then
        die "NAMCore render binary missing in release ceremony mode! Set NAM_CORE_RENDER_BIN or build it locally."
    fi
    warn "NAMCore render binary missing. Skipping parity test."
    GATE_SKIPS+=("namcore_parity:missing_oracle_bin")
    PARITY_OVERSAMPLE_STATUS="SKIP"
    PARITY_OVERSAMPLE_REASON="missing_oracle_bin"
    GATE_SKIPS+=("parity_oversample:missing_oracle_bin")
fi

# Gate 3c: artifact-level CabSim parity. The reference is an independent
# direct convolution of the artifact's certified IR-free output, so no C++
# oracle is required; it still targets the installed bytes.
echo -e "  [Gate 3c] Running artifact CabSim parity against distributed artifact..."
if [ -f "$model_fixture" ] && CLAP_PLUGIN_UNDER_TEST="$CLAP_TARGET" \
    timeout 600 cargo test --features testing --release --test clap \
    test_clap_parity_cabsim -- --ignored --nocapture; then
    PARITY_CABSIM_STATUS="PASS"
    ok "Artifact CabSim parity certified on distributed artifact."
else
    PARITY_CABSIM_STATUS="FAIL"
    if [ ! -f "$model_fixture" ]; then
        PARITY_CABSIM_REASON="missing_model_fixture"
    else
        PARITY_CABSIM_REASON="divergence"
    fi
    if [ "$STRICT_MODE" = "1" ] || [ "$RELEASE_CEREMONY" = "true" ]; then
        die "Artifact CabSim parity failed on the distributed artifact ($PARITY_CABSIM_REASON)."
    fi
    GATE_SKIPS+=("parity_cabsim:$PARITY_CABSIM_REASON")
    warn "Artifact CabSim parity failed ($PARITY_CABSIM_REASON); typed gap parity_cabsim:$PARITY_CABSIM_REASON."
fi

# Gate 4: CabSim IR test against distributed artifact
echo -e "  [Gate 4/5] Running CabSim IR test against distributed artifact..."
CLAP_PLUGIN_UNDER_TEST="$CLAP_TARGET" \
    timeout 600 cargo test --features testing --release --test clap \
    test_cabsim_ir_changes_audio_release_artifact -- --ignored --nocapture
ok "CabSim IR test passed on distributed artifact."

# Gate 5: Distributed Artifact Performance Certification Gate
echo -e "  [Gate 5/5] Running performance certification gate on distributed artifact..."
PERF_REPORT_PATH="$PROJECT_DIR/target/perf-certification-report.json"
PERF_REPORT_SHA=""
PERF_GATE_STATUS="SKIPPED"

# The certification gate must measure under the same core/priority policy as the
# regression gate (utils/tests-performance-regression.sh); otherwise its p50 is
# not comparable with Criterion or .performance-baselines. pick_bench_core comes
# from the shared _lib_core.sh already sourced above (NAM_BENCH_CORE → isolated
# core → nproc/2).
PERF_BENCH_CORE="$(pick_bench_core)"
export NAM_BENCH_CORE="$PERF_BENCH_CORE"
PERF_PIN_CMD=()
if command -v taskset >/dev/null 2>&1; then
    PERF_PIN_CMD=(taskset -c "$PERF_BENCH_CORE")
else
    warn "taskset not found — performance certification gate will run without core pinning."
fi
PERF_SCHED_CMD=()
PERF_SCHED_LABEL="SCHED_OTHER"
if command -v chrt >/dev/null 2>&1 && chrt -f 80 true 2>/dev/null; then
    PERF_SCHED_CMD=(chrt -f 80)
    PERF_SCHED_LABEL="SCHED_FIFO:80"
else
    warn "SCHED_FIFO unavailable — performance certification gate will run under the default scheduling policy."
fi
PERF_REFERENCE_ARGS=()
if [ -n "${NAM_PERF_REFERENCE_REPORT:-}" ]; then
    if [ ! -f "$NAM_PERF_REFERENCE_REPORT" ]; then
        die "NAM_PERF_REFERENCE_REPORT does not exist: $NAM_PERF_REFERENCE_REPORT"
    fi
    # Relative ceiling activation is gated on the operator's 3-run pinned noise
    # calibration; once calibrated, point this at the previous report to make the
    # gate fail on relative p50 regression instead of only on the absolute deadline.
    PERF_REFERENCE_ARGS=(--reference "$NAM_PERF_REFERENCE_REPORT" \
        --p50-ratio-max "${NAM_PERF_REFERENCE_P50_RATIO_MAX:-1.5}")
    echo -e "  ${BLUE}→ Relative p50 ceiling: ${NAM_PERF_REFERENCE_P50_RATIO_MAX:-1.5}× vs ${NAM_PERF_REFERENCE_REPORT}${NC}"
fi
echo -e "  ${BLUE}→ Benchmark core: ${PERF_BENCH_CORE}; scheduling: ${PERF_SCHED_LABEL}${NC}"

set +e
"${PERF_PIN_CMD[@]}" "${PERF_SCHED_CMD[@]}" \
    cargo run --locked --profile dist --features testing --bin nam_perf_guard -- \
    certify --clap "$CLAP_TARGET" --out "$PERF_REPORT_PATH" "${PERF_REFERENCE_ARGS[@]}"
PERF_EXIT_CODE=$?
set -e

if [ -f "$PERF_REPORT_PATH" ]; then
    PERF_REPORT_SHA=$(sha256sum "$PERF_REPORT_PATH" | cut -d' ' -f1)
    # Surface host isolation separately from measurement noise. The guard keeps
    # `environment_stable` for the coefficient-of-variation statistic and records
    # isolation gaps in dedicated fields; the receipt must carry both so a PASS
    # obtained on a non-isolated host is never read as a fully isolated run.
    PERF_ENVIRONMENT_VALUES=$(python3 - "$PERF_REPORT_PATH" <<'PY'
import json
import sys

try:
    with open(sys.argv[1], encoding="utf-8") as handle:
        report = json.load(handle)
except (OSError, json.JSONDecodeError):
    print("false")
    print("")
    raise SystemExit(0)

degraded = report.get("environment_degraded") is True
reasons = report.get("environment_degraded_reasons") or []
if not isinstance(reasons, list):
    reasons = [str(reasons)]
print("true" if degraded else "false")
print(";".join(str(reason) for reason in reasons))
PY
)
    PERF_ENVIRONMENT_DEGRADED=$(printf '%s\n' "$PERF_ENVIRONMENT_VALUES" | sed -n '1p')
    PERF_ENVIRONMENT_DEGRADED_REASONS=$(printf '%s\n' "$PERF_ENVIRONMENT_VALUES" | sed -n '2p')
    if [ "$PERF_ENVIRONMENT_DEGRADED" = "true" ]; then
        warn "Performance gate ran on a degraded host (isolation, not noise): ${PERF_ENVIRONMENT_DEGRADED_REASONS:-unknown}."
    fi
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
# Certification status (derived before packaging)
# -----------------------------------------------------------------------------
# A failed ceremony must never overwrite the last-known-good distribution files
# in $HOME. The status is therefore derived here, before Phases 6–7, and those
# phases are skipped when a ceremony cannot certify the artifact.
# Re-verify clean work tree before packaging and receipt writing (fail-closed).
check_git_clean_strict

GIT_COMMIT=$(git rev-parse HEAD 2>/dev/null || echo "unknown")
GIT_BRANCH=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "unknown")
GIT_DIRTY=false
if [ -n "$(git status --porcelain 2>/dev/null || true)" ]; then
    GIT_DIRTY=true
    # A rewritten lockfile is a deliberate dependency change, not an anonymous
    # dirty tree: name the cause so the operator knows the next step.
    if [ -n "$(git status --porcelain -- Cargo.lock 2>/dev/null || true)" ]; then
        GATE_SKIPS+=("git_dirty:Cargo.lock")
        warn "Cargo.lock differs from the committed lockfile — receipt cannot certify until it is committed (typed gap git_dirty:Cargo.lock)."
    else
        GATE_SKIPS+=("git_dirty:worktree")
        warn "Working tree is dirty — receipt cannot certify (typed gap git_dirty:worktree)."
    fi
fi

# -----------------------------------------------------------------------------
# PHASE 4.2: BOLT label fallback via the measured performance gate
# -----------------------------------------------------------------------------
# The Plug ships no ab-opt ceremony producer (docs/testing.md §5.4.1). A PASSED
# performance gate proves the deadline envelope, not a BOLT win, so it is
# recorded as `gate-pass-only`; it never promotes the claim to a proof. An
# applied BOLT without ceremony proof and without a passing gate remains a typed
# gap in EVERY mode, so it can never yield a CERTIFIED receipt.
if [ "${CLAP_BOLT_APPLIED:-false}" = true ] && [ "$BOLT_PROOF" = "MISSING" ] && [ "$PERF_GATE_STATUS" = "PASSED" ]; then
    BOLT_PROOF="gate-pass-only"
    warn "BOLT win unproven; recording explicit label '$BOLT_PROOF' (performance gate PASSED)."
fi
if [ "${CLAP_BOLT_APPLIED:-false}" = true ] && [ "$BOLT_PROOF" = "MISSING" ]; then
    GATE_SKIPS+=("bolt_proof:missing_ceremony")
    warn "BOLT applied but unproven (no ab-opt-ceremony receipt; perf gate not PASSED): typed gap bolt_proof:missing_ceremony."
fi

# -----------------------------------------------------------------------------
# Long-audit artifact consistency (E-A)
# -----------------------------------------------------------------------------
# The long receipt is the only phase evidence that exercises the installed
# binary. Compare the artifact it declares with the binary this pipeline just
# installed; a receipt from another session (different artifact) is `absent`,
# never silently credited. In ceremony mode the preflight already ran this
# verifier, so reuse its result instead of spawning it a second time.
ARTIFACT_CONSISTENCY="absent"
LONG_RECEIPT_ARTIFACT_SHA=""
LONG_RECEIPT_SESSION="none"
if [ "$REUSED_CERTIFIED_BINARY" = "true" ] && [ -n "${CERTIFIED_LONG_SHA:-}" ]; then
    ARTIFACT_CONSISTENCY="match"
    LONG_RECEIPT_ARTIFACT_SHA="$CERTIFIED_LONG_SHA"
    LONG_RECEIPT_SESSION="current"
elif [ -f "$LONG_RECEIPT_PATH" ]; then
    set +e
    ARTIFACT_VERDICT_OUT=$(cargo run --quiet --locked --features testing --bin plug_long_receipt -- \
        verify-artifact --out "$LONG_RECEIPT_PATH" --artifact "$CLAP_TARGET" 2>/dev/null)
    ARTIFACT_VERDICT_RC=$?
    set -e
    LONG_RECEIPT_ARTIFACT_SHA=$(printf '%s\n' "$ARTIFACT_VERDICT_OUT" | awk '/^ARTIFACT_SHA256: /{print $2; exit}')
    [ "$LONG_RECEIPT_ARTIFACT_SHA" = "-" ] && LONG_RECEIPT_ARTIFACT_SHA=""
    if [ "$ARTIFACT_VERDICT_RC" = "1" ]; then
        ARTIFACT_CONSISTENCY="mismatch"
    elif [ "$ARTIFACT_VERDICT_RC" = "0" ]; then
        ARTIFACT_CONSISTENCY="match"
    else
        ARTIFACT_CONSISTENCY="absent"
    fi
    if [ -n "$INSTALLED_AT_START_SHA" ] && [ -n "$LONG_RECEIPT_ARTIFACT_SHA" ]; then
        if [ "$LONG_RECEIPT_ARTIFACT_SHA" = "$INSTALLED_AT_START_SHA" ]; then
            LONG_RECEIPT_SESSION="current"
        else
            LONG_RECEIPT_SESSION="stale"
            ARTIFACT_CONSISTENCY="absent"
        fi
    elif [ -n "$LONG_RECEIPT_ARTIFACT_SHA" ]; then
        LONG_RECEIPT_SESSION="current"
    fi
    if [ "$ARTIFACT_CONSISTENCY" = "match" ]; then
        echo -e "  ${GREEN}✓${NC} Long-audit receipt certifies the installed artifact (sha256: ${LONG_RECEIPT_ARTIFACT_SHA:0:16}...)."
    else
        warn "Long-audit artifact consistency: $ARTIFACT_CONSISTENCY (session=$LONG_RECEIPT_SESSION, receipt_sha=${LONG_RECEIPT_ARTIFACT_SHA:-none}, installed_sha=${FINAL_CLAP_SHA:-unknown})."
    fi
fi
ARTIFACT_GAP=$(receipt_artifact_gap "$ARTIFACT_CONSISTENCY")
if [ -n "$ARTIFACT_GAP" ]; then
    GATE_SKIPS+=("$ARTIFACT_GAP")
    warn "Long-audit provenance is '$ARTIFACT_CONSISTENCY' — typed gap $ARTIFACT_GAP (finalized by utils/finalize-release-receipt.sh after the long suite)."
fi

# Determine typed certification status (CERTIFIED, FUNCTIONALLY_CERTIFIED, INCOMPLETE).
# The rule lives in lib/_lib.sh so the post-long-audit finalizer never drifts.
NON_ARTIFACT_SKIP_COUNT=0
for skip in "${GATE_SKIPS[@]}"; do
    case "$skip" in
        artifact_consistency:*) ;;
        *) NON_ARTIFACT_SKIP_COUNT=$((NON_ARTIFACT_SKIP_COUNT + 1)) ;;
    esac
done
RECEIPT_STATUS=$(receipt_derive_status \
    "$GIT_DIRTY" "${CORE_GIT_DIRTY:-false}" "$PERF_GATE_STATUS" \
    "$QUICK_QA_PASSED" "$HEAP_AUDIT_PASSED" "$USE_PGO" \
    "${CLAP_BOLT_APPLIED:-false}" "$NON_ARTIFACT_SKIP_COUNT")
RECEIPT_STATUS=$(receipt_apply_artifact_cap \
    "$RECEIPT_STATUS" "$ARTIFACT_CONSISTENCY" "$STRICT_MODE")

GATE_SKIPS_JOINED=""
if [ ${#GATE_SKIPS[@]} -gt 0 ]; then
    GATE_SKIPS_JOINED=$(IFS='|'; printf '%s' "${GATE_SKIPS[*]}")
fi
if [ "$RECEIPT_STATUS" = "INCOMPLETE" ]; then
    warn "Receipt status INCOMPLETE: git_dirty=$GIT_DIRTY core_git_dirty=$CORE_GIT_DIRTY skipped_gates=[${GATE_SKIPS_JOINED:-(none)}]"
fi

PACKAGING_ALLOWED=true
if [ "$RELEASE_CEREMONY" = "true" ] && [ "$RECEIPT_STATUS" != "CERTIFIED" ]; then
    PACKAGING_ALLOWED=false
    warn "Ceremony status $RECEIPT_STATUS: distribution packaging is skipped; receipt and artifacts remain in target/ for diagnosis."
fi

# -----------------------------------------------------------------------------
# PHASE 6: Release Packaging (.tar.zst)
# -----------------------------------------------------------------------------
if [ "$PACKAGING_ALLOWED" != "true" ]; then
    echo -e "\n${YELLOW}[Phase 6/7] Skipping tarball packaging (ceremony status $RECEIPT_STATUS).${NC}"
elif [ "$BUILD_TARBALL" = true ]; then
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
if [ "$PACKAGING_ALLOWED" != "true" ]; then
    echo -e "\n${YELLOW}[Phase 7/7] Skipping Flatpak packaging (ceremony status $RECEIPT_STATUS).${NC}"
elif [ "$BUILD_FLATPAK" = true ]; then
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


CARGO_LOCK_SHA=$(sha256sum Cargo.lock 2>/dev/null | cut -d' ' -f1 || echo "")

# -----------------------------------------------------------------------------
# PGO workload evidence (S3.2–S3.4)
# -----------------------------------------------------------------------------
# The receipt records what actually trained the profile: the excitation gain,
# both clipping counts, the scenario/callback coverage and the SHA of the log
# that backs them. Missing evidence is explicit (`not_run`/null), never a
# silently green block.
if [ "$REUSED_CERTIFIED_BINARY" = "true" ]; then
    # Reuse only the values that were validated against the preliminary receipt
    # and its hashed proof files; never re-read the mutable workload JSON here.
    PGO_CLIPPING_PHASE1="${REUSED_PGO_CLIPPING_PHASE1:-0}"
    PGO_CLIPPING_BOLT="${REUSED_PGO_CLIPPING_BOLT:-0}"
    PGO_WORKLOAD_STATUS="$REUSED_PGO_STATUS"
    PGO_WORKLOAD_GAIN="$REUSED_PGO_GAIN"
    PGO_WORKLOAD_SCENARIOS="$REUSED_PGO_SCENARIOS"
    PGO_WORKLOAD_CALLBACKS="$REUSED_PGO_CALLBACKS"
    PGO_WORKLOAD_LOG_SHA="$REUSED_PGO_LOG_SHA"
    PGO_WORKLOAD_RECEIPT_SHA="$REUSED_PGO_RECEIPT_SHA"
else
    PGO_CLIPPING_TOTAL_ALL=$(( ${PGO_CLIPPING_PHASE1:-0} + ${PGO_CLIPPING_BOLT:-0} ))
    PGO_WORKLOAD_STATUS="not_run"
    if [ "$PGO_WORKLOAD_RAN_PHASE1" = true ] || [ "$PGO_WORKLOAD_RAN_BOLT" = true ]; then
        if [ "$PGO_CLIPPING_TOTAL_ALL" -gt 0 ]; then
            PGO_WORKLOAD_STATUS="CLIPPED"
        elif [ -f "$PGO_WORKLOAD_RECEIPT" ] && grep -q '"status": "SUCCESS"' "$PGO_WORKLOAD_RECEIPT"; then
            PGO_WORKLOAD_STATUS="SUCCESS"
        else
            PGO_WORKLOAD_STATUS="FAILED"
        fi
    fi
    if [ -f "$PGO_WORKLOAD_RECEIPT" ]; then
        PGO_WORKLOAD_VALUES=$(python3 - "$PGO_WORKLOAD_RECEIPT" <<'PY'
import json
import sys

try:
    with open(sys.argv[1], "r", encoding="utf-8") as handle:
        doc = json.load(handle)
except (OSError, json.JSONDecodeError):
    print("- - -")
    raise SystemExit(0)
gain = doc.get("excitation_gain")
print(
    "-" if gain is None else repr(float(gain)),
    doc.get("total_scenarios", "-"),
    doc.get("total_callbacks", "-"),
)
PY
)
        read -r PGO_WORKLOAD_GAIN PGO_WORKLOAD_SCENARIOS PGO_WORKLOAD_CALLBACKS <<<"$PGO_WORKLOAD_VALUES"
        [ "$PGO_WORKLOAD_GAIN" = "-" ] && PGO_WORKLOAD_GAIN=""
        [ "$PGO_WORKLOAD_SCENARIOS" = "-" ] && PGO_WORKLOAD_SCENARIOS=""
        [ "$PGO_WORKLOAD_CALLBACKS" = "-" ] && PGO_WORKLOAD_CALLBACKS=""
    fi
    PGO_WORKLOAD_LOG_SHA=""
    if [ -f "$PGO_WORKLOAD_LOG" ]; then
        PGO_WORKLOAD_LOG_SHA=$(sha256sum "$PGO_WORKLOAD_LOG" | cut -d' ' -f1)
    fi
    PGO_WORKLOAD_RECEIPT_SHA=""
    if [ -f "$PGO_WORKLOAD_RECEIPT" ]; then
        PGO_WORKLOAD_RECEIPT_SHA=$(sha256sum "$PGO_WORKLOAD_RECEIPT" | cut -d' ' -f1)
    fi
fi

RUSTC_VER=$(rustc --version 2>/dev/null || echo "unknown")
TIMESTAMP=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

RECEIPT_TMP="$(mktemp -t nam-plug-receipt.XXXXXX)"
# Pass the proof fields through the environment (not shell-interpolated
# into the Python literal) so a filename/JSON-derived value can never break the
# single-quoted literal or inject Python.
export BOLT_PROOF BOLT_LBR_SAMPLES BOLT_LBR_FDATA_SHA256
export CODEGEN_PRE_JSON CODEGEN_POST_JSON CODEGEN_COMPARISON
export PGO_WORKLOAD_STATUS PGO_WORKLOAD_GAIN PGO_WORKLOAD_SCENARIOS PGO_WORKLOAD_CALLBACKS
export PGO_CLIPPING_PHASE1 PGO_CLIPPING_BOLT PGO_WORKLOAD_LOG PGO_WORKLOAD_LOG_SHA
export PGO_WORKLOAD_RECEIPT_SHA CLAP_RUSTFLAGS
export PRE_STRIP_ARTIFACT_SHA CERTIFIED_CLAP CERTIFIED_COPY_SHA
export PARITY_OVERSAMPLE_STATUS PARITY_OVERSAMPLE_REASON
export PARITY_CABSIM_STATUS PARITY_CABSIM_REASON
export CLAP_VALIDATOR_STATUS CLAP_VALIDATOR_TESTS_RUN CLAP_VALIDATOR_PASSED
export CLAP_VALIDATOR_FAILED CLAP_VALIDATOR_SKIPPED CLAP_VALIDATOR_SKIP_REASONS
export CLAP_VALIDATOR_LOG
export PERF_ENVIRONMENT_DEGRADED PERF_ENVIRONMENT_DEGRADED_REASONS
python3 -c "
import json, os, re, sys

skipped_gates = [g for g in sys.argv[1].split('|') if g]

def _load_codegen(path):
    if not path:
        return None
    try:
        with open(path, 'r', encoding='utf-8') as handle:
            return json.load(handle)
    except (OSError, json.JSONDecodeError):
        return None


def _int_or_none(value):
    try:
        return None if value in (None, '') else int(value)
    except (TypeError, ValueError):
        return None


def _float_or_none(value):
    try:
        return None if value in (None, '') else float(value)
    except (TypeError, ValueError):
        return None


def _sha256_or_none(path):
    if not path or not os.path.isfile(path):
        return None
    import hashlib
    with open(path, 'rb') as handle:
        return hashlib.sha256(handle.read()).hexdigest()


def _ceremony_provenance():
    script_sha = os.environ.get('NAM_CEREMONY_SCRIPT_SHA256', '')
    if not re.fullmatch(r'[0-9a-f]{64}', script_sha):
        return None
    phases = [p for p in os.environ.get('NAM_CEREMONY_PHASES', '').split(',') if p]
    return {'script_sha256': script_sha, 'phases': phases}


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
        'rustflags': os.environ.get('CLAP_RUSTFLAGS', ''),
        'ceremony': _ceremony_provenance()
    },
    'engine_provenance': {
        'dependency_type': 'path_patch' if '$CORE_PATH' else 'crates_io',
        'path': '$CORE_PATH' or None,
        'version': '$CORE_VERSION',
        'git_commit': '$CORE_GIT_COMMIT' if '$CORE_PATH' and '$CORE_GIT_COMMIT' != 'unknown' else None,
        'git_branch': '$CORE_GIT_BRANCH' if '$CORE_PATH' and '$CORE_GIT_BRANCH' != 'unknown' else None,
        'git_dirty': bool('$CORE_GIT_DIRTY' == 'true'),
        'source_tree_sha256': '$CORE_TREE_SHA' or None
    },
    'quick_qa_audit': {
        'status': 'PASSED' if bool('$QUICK_QA_PASSED' == 'true') else 'FAILED',
        'heap_audit_passed': bool('$HEAP_AUDIT_PASSED' == 'true'),
        'receipt_sha256': '$QUICK_RECEIPT_SHA' or None,
        'heap_audit_log_sha256': '$HEAP_AUDIT_LOG_SHA' or None,
        # The quick suite exercises the pre-PGO/BOLT release .so; its observed
        # hash and strictness must be explicit so a green OVERALL is never read
        # as 'strict run of the installed artifact'.
        'artifact_sha256': '$QUICK_RECEIPT_ARTIFACT_SHA' or None,
        'strict': bool('$QUICK_RECEIPT_STRICT' == '1')
    },
    # External protocol conformance, including deliberate skips. A green run
    # with skips is not the same evidence as a green run with none.
    'clap_validator': {
        'status': os.environ.get('CLAP_VALIDATOR_STATUS', 'UNKNOWN') or 'UNKNOWN',
        'tests_run': _int_or_none(os.environ.get('CLAP_VALIDATOR_TESTS_RUN')),
        'passed': _int_or_none(os.environ.get('CLAP_VALIDATOR_PASSED')),
        'failed': _int_or_none(os.environ.get('CLAP_VALIDATOR_FAILED')),
        'skipped': _int_or_none(os.environ.get('CLAP_VALIDATOR_SKIPPED')),
        'skip_reasons': [
            reason
            for reason in (os.environ.get('CLAP_VALIDATOR_SKIP_REASONS', '') or '').split(';')
            if reason
        ],
        'log_sha256': _sha256_or_none(os.environ.get('CLAP_VALIDATOR_LOG')),
    },
    # Artifact-level parity gates (S3.1). Each entry names the exact installed
    # bytes it exercised; a skipped oracle is explicit, never a PASS.
    'gates': {
        'parity_oversample': {
            'status': os.environ.get('PARITY_OVERSAMPLE_STATUS', 'SKIP'),
            'reason': os.environ.get('PARITY_OVERSAMPLE_REASON') or None,
            'artifact_sha256': '$FINAL_CLAP_SHA'
        },
        'parity_cabsim': {
            'status': os.environ.get('PARITY_CABSIM_STATUS', 'SKIP'),
            'reason': os.environ.get('PARITY_CABSIM_REASON') or None,
            'artifact_sha256': '$FINAL_CLAP_SHA'
        }
    },
    'optimizations': {
        'pgo_applied': bool('$USE_PGO' == 'true'),
        'bolt_applied': bool('$CLAP_BOLT_APPLIED' == 'true'),
        'bolt_profile_sha256': '$BOLT_PROFILE_SHA' or None,
        'bolt_version': '$BOLT_VERSION' or None,
        # Independent win proof: only a valid 'ab-opt-ceremony:<id>' receipt may
        # claim a measured BOLT gain. A PASSED performance gate is labelled
        # 'gate-pass-only' and never promoted to a proof of a BOLT win.
        'bolt_proof': os.environ.get('BOLT_PROOF', 'MISSING') or 'MISSING',
        # Claims the receipt deliberately does NOT make. 'gate-pass-only' proves
        # the deadline envelope, not that BOLT made the artifact faster, so the
        # unproven claim is named instead of implied.
        'unproven_claims': (
            ['bolt_win']
            if (os.environ.get('BOLT_PROOF', 'MISSING') or 'MISSING') == 'gate-pass-only'
            else []
        ),
        'bolt_lbr_samples': os.environ.get('BOLT_LBR_SAMPLES') or None,
        'bolt_lbr_fdata_sha256': os.environ.get('BOLT_LBR_FDATA_SHA256') or None,
        # Pre/post-link codegen coverage, so a post-BOLT verifier that silently
        # inspects fewer targets is visible in the cryptographic receipt.
        'codegen': {
            'comparison': os.environ.get('CODEGEN_COMPARISON', 'not_run') or 'not_run',
            'pre_bolt': _load_codegen(os.environ.get('CODEGEN_PRE_JSON')),
            'post_bolt': _load_codegen(os.environ.get('CODEGEN_POST_JSON'))
        },
        # Flat convenience fields for the post-link coverage contract (S3.6).
        'pre_bolt_symbols': (_load_codegen(os.environ.get('CODEGEN_PRE_JSON')) or {}).get('symbols'),
        'post_bolt_symbols': (_load_codegen(os.environ.get('CODEGEN_POST_JSON')) or {}).get('symbols')
    },
    'pgo_workload': {
        'status': os.environ.get('PGO_WORKLOAD_STATUS', 'not_run') or 'not_run',
        'gain': _float_or_none(os.environ.get('PGO_WORKLOAD_GAIN')),
        'scenarios': _int_or_none(os.environ.get('PGO_WORKLOAD_SCENARIOS')),
        'callbacks': _int_or_none(os.environ.get('PGO_WORKLOAD_CALLBACKS')),
        'clipping_count_phase1': _int_or_none(os.environ.get('PGO_CLIPPING_PHASE1')),
        'clipping_count_bolt': _int_or_none(os.environ.get('PGO_CLIPPING_BOLT')),
        'log_path': os.environ.get('PGO_WORKLOAD_LOG') or None,
        'log_sha256': os.environ.get('PGO_WORKLOAD_LOG_SHA') or None,
        'receipt_sha256': os.environ.get('PGO_WORKLOAD_RECEIPT_SHA') or None
    },
    'performance_certification': {
        'gate_status': '$PERF_GATE_STATUS',
        'report_path': '$PERF_REPORT_PATH' if '$PERF_REPORT_SHA' else None,
        'report_sha256': '$PERF_REPORT_SHA' or None,
        # Isolation is separate from environment_stable, which is a noise statistic.
        # A degraded host does not fail the deadline gate, but the number it
        # produced must not be read as a fully isolated measurement.
        'environment_degraded': bool(os.environ.get('PERF_ENVIRONMENT_DEGRADED') == 'true'),
        'environment_degraded_reasons': [
            reason
            for reason in (os.environ.get('PERF_ENVIRONMENT_DEGRADED_REASONS', '') or '').split(';')
            if reason
        ]
    },
    'artifacts': {
        'clap_installed_path': '$CLAP_TARGET',
        'clap_installed_sha256': '$FINAL_CLAP_SHA',
        'certified_copy_path': os.environ.get('CERTIFIED_CLAP') or None,
        'certified_copy_sha256': os.environ.get('CERTIFIED_COPY_SHA') or None,
        'reused_certified_binary': bool('${REUSED_CERTIFIED_BINARY:-false}' == 'true'),
        # S1 finding: the installed artifact is the post-strip form of the
        # BOLT output, while codegen receipts measure the pre-strip .bolt.so.
        # Record that auxiliary hash so the two identities are never confused.
        'pre_strip_artifact_sha256': os.environ.get('PRE_STRIP_ARTIFACT_SHA') or None,
        'long_receipt_path': '$LONG_RECEIPT_PATH',
        'long_receipt_artifact_sha256': '$LONG_RECEIPT_ARTIFACT_SHA' or None,
        'long_receipt_session': '$LONG_RECEIPT_SESSION',
        'artifact_consistency': '$ARTIFACT_CONSISTENCY',
        'installed_sha256_at_build_start': '$INSTALLED_AT_START_SHA' or None,
        'tarball_path': '$TARBALL' if '$BUILD_TARBALL' == 'true' and '${PACKAGING_ALLOWED:-true}' == 'true' else None,
        'tarball_sha256': '$TARBALL_SHA' if '$BUILD_TARBALL' == 'true' and '${PACKAGING_ALLOWED:-true}' == 'true' else None,
        'flatpak_path': '$FLATPAK_BUNDLE' if '$BUILD_FLATPAK' == 'true' and '${PACKAGING_ALLOWED:-true}' == 'true' else None,
        'flatpak_sha256': '$FLATPAK_SHA' if '$BUILD_FLATPAK' == 'true' and '${PACKAGING_ALLOWED:-true}' == 'true' else None
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

# State the limits of what the receipt proves while the operator is still
# looking at the console: a CERTIFIED status is not a claim of a measured BOLT
# speedup, nor of a fully isolated measurement host.
if [ "${BOLT_PROOF:-}" = "gate-pass-only" ]; then
    echo -e "  ${YELLOW}[UNPROVEN]${NC} BOLT applied with bolt_proof=gate-pass-only: the performance gate proves the deadline envelope, not a BOLT speedup (optimizations.unproven_claims=[bolt_win])."
fi
if [ "$PERF_ENVIRONMENT_DEGRADED" = "true" ]; then
    echo -e "  ${YELLOW}[DEGRADED HOST]${NC} performance gate ran outside the isolated-core policy: ${PERF_ENVIRONMENT_DEGRADED_REASONS:-unknown}."
fi
if [ -n "$CLAP_VALIDATOR_SKIPPED" ]; then
    echo -e "  ${CYAN}clap-validator:${NC} ${CLAP_VALIDATOR_SUMMARY:-status=$CLAP_VALIDATOR_STATUS} (reasons: ${CLAP_VALIDATOR_SKIP_REASONS:-none})"
fi

BANNER_COLOR="${GREEN}"
if [ "$RECEIPT_STATUS" = "CERTIFIED" ]; then
    BANNER_COLOR="${GREEN}"
    echo -e "\n${BANNER_COLOR}${BOLD}================================================================================${NC}"
    echo -e "${GREEN}${BOLD}   Pipeline completed! Artifacts certified and ready for distribution:   ${NC}"
elif [ "$RECEIPT_STATUS" = "FUNCTIONALLY_CERTIFIED" ]; then
    BANNER_COLOR="${YELLOW}"
    echo -e "\n${BANNER_COLOR}${BOLD}================================================================================${NC}"
    echo -e "${YELLOW}${BOLD}   Pipeline completed! Functionally certified (PGO/BOLT omitted or perf inconclusive): ${NC}"
else
    BANNER_COLOR="${YELLOW}"
    echo -e "\n${BANNER_COLOR}${BOLD}================================================================================${NC}"
    echo -e "${YELLOW}${BOLD}   Pipeline completed with gaps — receipt status INCOMPLETE (not certified): ${NC}"
fi
echo -e "  ${BOLD}Artifacts saved:${NC}"
echo -e "    - CLAP Plugin:    ${CYAN}$CLAP_TARGET${NC} (sha256: ${FINAL_CLAP_SHA:0:16}...)"
echo -e "    - Build Receipt:  ${CYAN}$RECEIPT_TARGET${NC}"
if [ "$BUILD_TARBALL" = true ] && [ "$PACKAGING_ALLOWED" = true ]; then
    echo -e "    - Tarball:        ${CYAN}$TARBALL${NC} (sha256: ${TARBALL_SHA:0:16}...)"
fi
if [ "$BUILD_FLATPAK" = true ] && [ "$PACKAGING_ALLOWED" = true ]; then
    echo -e "    - Flatpak Bundle: ${CYAN}$FLATPAK_BUNDLE${NC} (sha256: ${FLATPAK_SHA:0:16}...)"
fi
if [ -f "$PROJECT_DIR/target/dsp_hotpath.asm" ]; then
    echo -e "    - Assembly ASM:   ${CYAN}$PROJECT_DIR/target/dsp_hotpath.asm${NC}"
fi
echo -e "${BANNER_COLOR}${BOLD}================================================================================${NC}\n"

# Release ceremony is fail-closed: the receipt and artifacts above stay on disk
# for diagnosis, but the run must not report success unless status is CERTIFIED.
CEREMONY_EXIT=$(ceremony_exit_code "$RECEIPT_STATUS" "$RELEASE_CEREMONY")
if [ "$CEREMONY_EXIT" -ne 0 ]; then
    echo -e "${RED}${BOLD}================================================================================${NC}" >&2
    echo -e "${RED}${BOLD}[CEREMONY-FAIL] receipt status=$RECEIPT_STATUS gaps=[${GATE_SKIPS_JOINED:-(none)}]${NC}" >&2
    echo -e "${RED}${BOLD}Release ceremony requires CERTIFIED. Receipt kept at: $RECEIPT_TARGET (exit $CEREMONY_EXIT)${NC}" >&2
    echo -e "${RED}${BOLD}================================================================================${NC}" >&2
    exit "$CEREMONY_EXIT"
fi

