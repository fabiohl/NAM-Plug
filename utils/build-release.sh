#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Unified compiler-grade release build & packaging script for NAM-Plug (PGO + BOLT + Flatpak).
# Compiles the CLAP plugin with Profile-Guided Optimization (PGO),
# post-link BOLT binary reordering, and generates release distribution archives
# and standalone Flatpak plugin extensions.
#
# Deliverables:
#   - ~/.clap/nam_plug.clap                      (PGO + BOLT optimized CLAP plugin)
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
  -h, --help             Show this help message and exit.

Deliverables:
  - ~/.clap/nam_plug.clap                      (Installed CLAP plugin)
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
# NAM_LIB_NO_CD=1 prevents _lib.sh from cding — we manage our own working
# directory below after computing PROJECT_DIR from SCRIPT_DIR.
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
NAM_LIB_NO_CD=1 source "$SCRIPT_DIR/_lib.sh"

echo -e "${BLUE}${BOLD}========================================================================${NC}"
echo -e "${BLUE}${BOLD}   NAM-Plug Unified Release Build & Optimization Pipeline               ${NC}"
echo -e "${BLUE}${BOLD}========================================================================${NC}"

# Ensure execution from the subproject root directory
cd "$PROJECT_DIR"

# State tracking for signal safety and cleanup
ORIG_PARANOID=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo "2")
PARANOID_MODIFIED=false
WORKLOAD_PID=""

# Dynamic isolated temporary directories for PGO & BOLT profiling and packaging
PGO_DIR="$(mktemp -d -t nam-plug-pgo.XXXXXX)"
BOLT_DIR="$(mktemp -d -t nam-plug-bolt.XXXXXX)"
PKG_DIR=""
FLATPAK_BUILD_DIR=""
FLATPAK_REPO_DIR=""
PROFRAW_DIR="$PGO_DIR/profraw"
MERGED_PROFILE="$PGO_DIR/merged.profdata"
ORIG_RUSTFLAGS="${RUSTFLAGS:-}"

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
        echo -e "${YELLOW}Warning: llvm-bolt was not found. The build will continue with PGO only.${NC}"
        echo -e "${YELLOW}To enable BOLT, install: sudo apt install llvm-22-tools${NC}"
    fi

    # Check perf_event_paranoid requirement for BOLT profiling
    if [ "$ORIG_PARANOID" -gt 1 ]; then
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
# PHASE 2: Profile-Guided Optimization (PGO) - Profiling Workload
# -----------------------------------------------------------------------------
if [ "$USE_PGO" = true ]; then
    echo -e "\n${BLUE}${BOLD}[Phase 2/7] Generating PGO profiles via workload runner...${NC}"

    export RUSTFLAGS="$CONFIG_RUSTFLAGS $ORIG_RUSTFLAGS -Cprofile-generate=$PROFRAW_DIR"
    export LLVM_PROFILE_FILE="$PROFRAW_DIR/default_%m_%p.profraw"
    echo -e "  Using RUSTFLAGS: ${BOLD}$RUSTFLAGS${NC}"

    echo -e "  Compiling real-world PGO profiling workload (pgo_profiling_workload)..."
    cargo build --profile dist --features testing --bin pgo_profiling_workload || {
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
RUSTFLAGS="$CLAP_RUSTFLAGS" cargo build --profile dist --target-dir "$PGO_CLAP_TARGET_DIR" --lib

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
    else
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
            cargo build --profile dist --features testing --bin pgo_profiling_workload

        NAM_CLAP_SO_PATH="$PGO_CLAP_TARGET_DIR/dist/libnam_plug.instrumented.so" \
            "$PGO_BUILD_TARGET_DIR/dist/pgo_profiling_workload" && \
            echo -e "  ${GREEN}✓${NC} CLAP profile collected" || \
            echo -e "${YELLOW}  Warning: CLAP profiling workload failed${NC}"

        # Merge fdata profiles
        if command -v "$MERGE_FDATA" &>/dev/null || [ -x "$MERGE_FDATA" ]; then
            echo -e "  Merging BOLT instrumentation profiles..."
            CLAP_FDATA_FILES=()
            while IFS= read -r -d '' f; do
                CLAP_FDATA_FILES+=("$f")
            done < <(find "$PGO_CLAP_TARGET_DIR" -maxdepth 1 -name "libnam_plug.fdata.*" -print0 2>/dev/null || true)

            if [ ${#CLAP_FDATA_FILES[@]} -gt 0 ]; then
                "$MERGE_FDATA" "${CLAP_FDATA_FILES[@]}" > "$BOLT_DIR/libnam_plug.merged.fdata" 2>"$BOLT_DIR/merge-fdata-clap.log"
                if [ -s "$BOLT_DIR/libnam_plug.merged.fdata" ]; then
                    echo -e "  ${GREEN}✓${NC} CLAP profiles merged (${#CLAP_FDATA_FILES[@]} files)"
                else
                    echo -e "${YELLOW}  Warning: CLAP profile merge produced empty output.${NC}"
                    if [ -f "$BOLT_DIR/merge-fdata-clap.log" ]; then
                        echo -e "${YELLOW}  --- merge-fdata log tail ---${NC}"
                        tail -n 10 "$BOLT_DIR/merge-fdata-clap.log"
                    fi
                fi
            else
                echo -e "${YELLOW}  Warning: No CLAP fdata profiles found.${NC}"
            fi
        else
            echo -e "${YELLOW}  Warning: merge-fdata tool not available. Skipping profile merge.${NC}"
        fi
    fi

    # Step 3: Apply BOLT Optimization
    if [ -f "$BOLT_DIR/libnam_plug.merged.fdata" ] && [ -s "$BOLT_DIR/libnam_plug.merged.fdata" ]; then
        echo -e "  [Step 3/3] Applying BOLT optimization using merged fdata..."
        # Shared libraries (cdylib/DSOs) require relocation-safe BOLT flags without -hugify or --split-all-cold
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
            echo -e "${YELLOW}  Warning: BOLT optimization failed for CLAP plugin. Falling back to PGO-only build.${NC}"
            if [ -f "$BOLT_DIR/llvm-bolt-clap.log" ]; then
                echo -e "${YELLOW}  --- llvm-bolt log tail ---${NC}"
                tail -n 10 "$BOLT_DIR/llvm-bolt-clap.log"
            fi
        fi
    else
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
# PHASE 5: Deliverables Installation & Verification
# -----------------------------------------------------------------------------
echo -e "\n${BLUE}${BOLD}[Phase 5/7] Installing and validating artifacts...${NC}"

# Target directories creation
mkdir -p "$CLAP_INSTALL_DIR"

# Deliver CLAP plugin
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

# Gate: validate the SHIPPED CLAP distribution artifact
echo -e "  Validating shipped CLAP artifact integrity..."
if ! nm -D "$CLAP_TARGET" | grep -w "clap_entry" > /dev/null; then
    echo -e "${RED}Error: Missing 'clap_entry' symbol in distributed CLAP artifact!${NC}"
    exit 1
fi
if ! readelf -d "$CLAP_TARGET" | grep SONAME >/dev/null; then
    echo -e "${RED}Error: Missing SONAME in distributed CLAP artifact!${NC}"
    exit 1
fi
if command -v clap-validator >/dev/null 2>&1; then
    echo -e "  Executing clap-validator..."
    clap-validator validate "$CLAP_TARGET" || {
        echo -e "${RED}Error: clap-validator rejected the distribution artifact!${NC}"
        exit 1
    }
else
    echo -e "${YELLOW}  Warning: clap-validator unavailable. Skipping external validation.${NC}"
fi
echo -e "  ${GREEN}✓${NC} CLAP artifact validation passed."

# Read version for archive naming
VERSION=$(cargo metadata --no-deps --format-version 1 | python3 -c "import sys, json; print(json.load(sys.stdin)['packages'][0]['version'])")
ARCHIVE_NAME="nam-plug-v${VERSION}-linux-x86_64-v3"
TARBALL="$HOME/${ARCHIVE_NAME}.tar.zst"
FLATPAK_BUNDLE="$HOME/${ARCHIVE_NAME}.flatpak"

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

    echo -e "  ${GREEN}✓${NC} Distribution package generated at: ${BOLD}$TARBALL${NC} ($(du -h "$TARBALL" | cut -f1))"
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

    echo -e "  ${GREEN}✓${NC} Flatpak bundle generated successfully: ${BOLD}$FLATPAK_BUNDLE${NC} ($(du -h "$FLATPAK_BUNDLE" | cut -f1))"

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

echo -e "\n${GREEN}${BOLD}================================================================================${NC}"
echo -e "${GREEN}${BOLD}   Pipeline completed! Artifacts ready for distribution:                ${NC}"
echo -e "  ${BOLD}Artifacts saved:${NC}"
echo -e "    - CLAP Plugin:    ${CYAN}$CLAP_TARGET${NC}"
if [ "$BUILD_TARBALL" = true ]; then
    echo -e "    - Tarball:        ${CYAN}$TARBALL${NC}"
fi
if [ "$BUILD_FLATPAK" = true ]; then
    echo -e "    - Flatpak Bundle: ${CYAN}$FLATPAK_BUNDLE${NC}"
fi
if [ -f "$PROJECT_DIR/target/dsp_hotpath.asm" ]; then
    echo -e "    - Assembly ASM:   ${CYAN}$PROJECT_DIR/target/dsp_hotpath.asm${NC}"
fi
echo -e "${GREEN}${BOLD}================================================================================${NC}\n"
