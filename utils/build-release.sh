#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
# build-release.sh — Compila e empacota artefato release canônico (nam_plug.clap). Classe C.
# Uso: utils/build-release.sh [--strict] [--package] [--install] [--certify] [-h|--help]
# Orçamento: 1000
set -x; PS4='+ ${BASH_SOURCE##*/}:${LINENO}: '
set -euo pipefail
cd "$(dirname "$0")/.."
. utils/_lib.sh

SCRIPT_NAME="build-release"
SCRIPT_DIR="utils"
PROJECT_DIR="$(pwd)"
DIST_DIR="$PROJECT_DIR/target/dist"
CLAP_TARGET="$DIST_DIR/nam_plug.clap"
CLAP_SHA_FILE="$DIST_DIR/nam_plug.clap.sha256"
BUILD_RECEIPT="$DIST_DIR/build-receipt.json"

# CLI flag parsing (Constitution §4.1 canonical flags only)
DO_STRICT=0
DO_PACKAGE=false
DO_INSTALL=false
DO_CERTIFY=false

show_help() {
    cat << 'EOF'
NAM-Plug Release Build Script (Canonical Class C)

Usage:
  utils/build-release.sh [flags]

Canonical Flags:
  --strict       Fail-closed mode: any optimization or verification gap aborts with exit code 1.
  --package      Generate distribution tarball (.tar.zst) and Flatpak extension (.flatpak) in target/dist/.
  --install      Install the compiled CLAP plugin into ~/.clap/ (and flatpak if packaged).
  --certify      Do not compile; strictly validate and certify canonical artifacts against receipts.
  -h, --help     Show this help message.
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --strict)
            DO_STRICT=1
            STRICT=1
            shift
            ;;
        --package)
            DO_PACKAGE=true
            shift
            ;;
        --install)
            DO_INSTALL=true
            shift
            ;;
        --certify)
            DO_CERTIFY=true
            shift
            ;;
        -h|--help)
            show_help
            exit 0
            ;;
        *)
            echo "build-release.sh: unknown flag '$1'" >&2
            echo "Allowed flags: --strict, --package, --install, --certify, -h, --help" >&2
            exit 2
            ;;
    esac
done

mkdir -p "$DIST_DIR" "$LOG_DIR" "target/logs"

# -----------------------------------------------------------------------------
# MODE: --certify (Pure receipt & artifact validation without compilation)
# -----------------------------------------------------------------------------
if [ "$DO_CERTIFY" = true ]; then
    phase "certify"
    CERT_ARGS=("--artifact" "$CLAP_TARGET" "--build-receipt" "$BUILD_RECEIPT")
    if [ "$DO_STRICT" -eq 1 ]; then
        CERT_ARGS+=("--strict")
    fi

    CERT_BIN=""
    if [ -x "$PROJECT_DIR/target/long/release/plug_long_receipt" ]; then
        CERT_BIN="$PROJECT_DIR/target/long/release/plug_long_receipt"
    elif [ -x "$PROJECT_DIR/target/release/plug_long_receipt" ]; then
        CERT_BIN="$PROJECT_DIR/target/release/plug_long_receipt"
    fi

    RC=0
    if [ -n "$CERT_BIN" ]; then
        "$CERT_BIN" certify "${CERT_ARGS[@]}" 2>&1 | tee "$LOG_DIR/build-release-certify.log" || RC=${PIPESTATUS[0]}
    else
        cargo run --release --quiet --locked --features testing --bin plug_long_receipt -- certify "${CERT_ARGS[@]}" 2>&1 | tee "$LOG_DIR/build-release-certify.log" || RC=${PIPESTATUS[0]}
    fi

    if [ "$RC" -eq 0 ]; then
        emit "PHASE: certify PASS log=$LOG_DIR/build-release-certify.log"
    else
        FAILS+=("certify")
        emit "PHASE: certify FAIL rc=$RC log=$LOG_DIR/build-release-certify.log"
    fi
    finish
    exit $?
fi

# -----------------------------------------------------------------------------
# PHASE 1: Environment & Tool Verification
# -----------------------------------------------------------------------------
phase "env_verify"
ENV_LOG="$LOG_DIR/build-release-env_verify.log"
{
    command -v rustc >/dev/null || { echo "rustc missing"; exit 1; }
    command -v cargo >/dev/null || { echo "cargo missing"; exit 1; }
    command -v nm >/dev/null || { echo "nm missing"; exit 1; }
    command -v readelf >/dev/null || { echo "readelf missing"; exit 1; }
    command -v strip >/dev/null || { echo "strip missing"; exit 1; }

    rustc --version
    cargo --version
} 2>&1 | tee "$ENV_LOG"
emit "PHASE: env_verify PASS log=$ENV_LOG"

VERSION=$(awk -F'"' '/^version = / {print $2; exit}' Cargo.toml 2>/dev/null || echo "1.0.0")
ARCHIVE_NAME="nam-plug-${VERSION}-linux-x86_64"
TARBALL="$DIST_DIR/${ARCHIVE_NAME}.tar.zst"
FLATPAK_BUNDLE="$DIST_DIR/org.freedesktop.LinuxAudio.Plugins.NAMPlug.flatpak"

# Git provenance
GIT_COMMIT="unknown"
GIT_BRANCH="unknown"
GIT_DIRTY=false
if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    GIT_COMMIT=$(git rev-parse HEAD 2>/dev/null || echo "unknown")
    GIT_BRANCH=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "unknown")
    if [ -n "$(git status --porcelain 2>/dev/null || true)" ]; then
        GIT_DIRTY=true
        if [ "$DO_STRICT" -eq 1 ]; then
            echo "build-release: git tree dirty in strict mode (fail-closed)" >&2
            exit 1
        fi
    fi
fi

# Disable symbol stripping during release compilation so BOLT can reorder symbols.
# Final artifact is stripped explicitly in deliver_artifact.
export CARGO_PROFILE_DIST_STRIP="false"
export CARGO_PROFILE_BENCH_STRIP="false"

# Parse .cargo/config.toml rustflags using POSIX awk
CONFIG_RUSTFLAGS=$(awk -F'"' '/^  "-C/ || /^  "-Wl/ || /^  "-/ {if ($2 != "") printf "%s ", $2}' .cargo/config.toml 2>/dev/null || true)
if [ -z "${CONFIG_RUSTFLAGS:-}" ]; then
    echo "build-release: failed to extract rustflags from .cargo/config.toml" >&2
    exit 1
fi
ORIG_RUSTFLAGS="${RUSTFLAGS:-}"
BASE_RUSTFLAGS="$CONFIG_RUSTFLAGS $ORIG_RUSTFLAGS"

# Toolchain detection for PGO & BOLT
RUST_SYSROOT="$(rustc --print sysroot 2>/dev/null || echo "")"
LLVM_PROFDATA=""
if [ -n "$RUST_SYSROOT" ] && [ -x "$RUST_SYSROOT/lib/rustlib/x86_64-unknown-linux-gnu/bin/llvm-profdata" ]; then
    LLVM_PROFDATA="$RUST_SYSROOT/lib/rustlib/x86_64-unknown-linux-gnu/bin/llvm-profdata"
elif command -v llvm-profdata >/dev/null 2>&1; then
    LLVM_PROFDATA="llvm-profdata"
fi

LLVM_BOLT=""
for candidate in llvm-bolt llvm-bolt-19 llvm-bolt-18; do
    if command -v "$candidate" >/dev/null 2>&1; then
        LLVM_BOLT="$candidate"
        break
    fi
done

MERGE_FDATA=""
for candidate in merge-fdata merge-fdata-19 merge-fdata-18; do
    if command -v "$candidate" >/dev/null 2>&1; then
        MERGE_FDATA="$candidate"
        break
    fi
done

OBJDUMP_TOOL=""
for candidate in llvm-objdump objdump; do
    if command -v "$candidate" >/dev/null 2>&1; then
        OBJDUMP_TOOL="$candidate"
        break
    fi
done

# Pre-build plug_long_receipt tool
cargo build --locked --features testing --bin plug_long_receipt >/dev/null 2>&1 || true

# -----------------------------------------------------------------------------
# PHASE 2: PGO Workload Profiling
# -----------------------------------------------------------------------------
PGO_TARGET_DIR="$PROJECT_DIR/target/pgo-profile"
PROFRAW_DIR="$PGO_TARGET_DIR/profraw"
MERGED_PROFILE="$PGO_TARGET_DIR/merged.profdata"
PGO_WORKLOAD_LOG="$PROJECT_DIR/target/logs/pgo-workload.log"
PGO_WORKLOAD_RECEIPT="$PROJECT_DIR/target/pgo-workload-receipt.json"
PGO_STATUS="skipped"

if [ -n "$LLVM_PROFDATA" ]; then
    phase "pgo_profile"
    PGO_LOG="$LOG_DIR/build-release-pgo_profile.log"
    rm -rf "$PROFRAW_DIR" "$MERGED_PROFILE"
    mkdir -p "$PROFRAW_DIR"

    PGO_GEN_RUSTFLAGS="$BASE_RUSTFLAGS -Cprofile-generate=$PROFRAW_DIR -Cllvm-args=-vp-counters-per-site=8"
    {
        echo "Compiling PGO profiling workload..."
        RUSTFLAGS="$PGO_GEN_RUSTFLAGS" cargo build --locked --profile dist --features testing --bin pgo_profiling_workload
        echo "Executing PGO profiling workload..."
        LLVM_PROFILE_FILE="$PROFRAW_DIR/default_%m_%p.profraw" timeout 60 "$PROJECT_DIR/target/dist/pgo_profiling_workload" 2>&1 | tee "$PGO_WORKLOAD_LOG"

        PROFRAW_COUNT=$(find "$PROFRAW_DIR" -name "*.profraw" 2>/dev/null | wc -l)
        if [ "$PROFRAW_COUNT" -gt 0 ]; then
            "$LLVM_PROFDATA" merge -sparse -o "$MERGED_PROFILE" "$PROFRAW_DIR"/*.profraw
            echo "Merged $PROFRAW_COUNT .profraw profiles into $MERGED_PROFILE"
        else
            echo "No .profraw profiles generated."
            exit 1
        fi
    } 2>&1 | tee "$PGO_LOG"
    RC=${PIPESTATUS[0]}
    rm -rf "$PROFRAW_DIR"

    if [ "$RC" -eq 0 ] && [ -s "$MERGED_PROFILE" ]; then
        PGO_STATUS="applied"
        emit "PHASE: pgo_profile PASS log=$PGO_LOG"
    else
        if [ "$DO_STRICT" -eq 1 ]; then
            FAILS+=("pgo_profile")
            emit "PHASE: pgo_profile FAIL rc=$RC log=$PGO_LOG"
        else
            gap "pgo_profile"
            PGO_STATUS="failed"
        fi
    fi
else
    gap "pgo_profdata_unavailable"
    if [ "$DO_STRICT" -eq 1 ]; then
        echo "build-release: llvm-profdata unavailable in strict mode" >&2
        exit 1
    fi
fi

# -----------------------------------------------------------------------------
# PHASE 3: Compile Optimized CLAP Plugin
# -----------------------------------------------------------------------------
phase "compile_clap"
COMPILE_LOG="$LOG_DIR/build-release-compile_clap.log"
PGO_CLAP_TARGET_DIR="$PGO_TARGET_DIR"

CLAP_BUILD_RUSTFLAGS="$BASE_RUSTFLAGS -Clink-arg=-Wl,-q -Clink-arg=-Wl,-soname,nam_plug.clap"
if [ "$PGO_STATUS" = "applied" ] && [ -s "$MERGED_PROFILE" ]; then
    CLAP_BUILD_RUSTFLAGS="$CLAP_BUILD_RUSTFLAGS -Cprofile-use=$MERGED_PROFILE"
fi

{
    echo "Compiling CLAP library..."
    RUSTFLAGS="$CLAP_BUILD_RUSTFLAGS" cargo build --locked --profile dist --target-dir "$PGO_CLAP_TARGET_DIR" --lib
} 2>&1 | tee "$COMPILE_LOG"
RC=${PIPESTATUS[0]}

COMPILED_SO="$PGO_CLAP_TARGET_DIR/dist/libnam_plug.so"
if [ "$RC" -eq 0 ] && [ -f "$COMPILED_SO" ]; then
    emit "PHASE: compile_clap PASS log=$COMPILE_LOG"
else
    FAILS+=("compile_clap")
    emit "PHASE: compile_clap FAIL rc=$RC log=$COMPILE_LOG"
    exit 1
fi

# -----------------------------------------------------------------------------
# PHASE 4: BOLT Post-Link Optimization
# -----------------------------------------------------------------------------
BOLT_STATUS="skipped"
BOLT_DIR="$PROJECT_DIR/target/bolt"
BOLT_PROFILE="$BOLT_DIR/libnam_plug.merged.fdata"
BOLT_VERSION=""
mkdir -p "$BOLT_DIR"

if [ -n "$LLVM_BOLT" ] && [ -n "$MERGE_FDATA" ]; then
    phase "bolt_optimize"
    BOLT_LOG="$LOG_DIR/build-release-bolt_optimize.log"
    {
        BOLT_VERSION=$("$LLVM_BOLT" --version 2>&1 | head -n 1 || echo "unknown")
        echo "BOLT version: $BOLT_VERSION"

        echo "[Step 1/3] Instrumenting CLAP plugin..."
        "$LLVM_BOLT" "$COMPILED_SO" -instrument -o "$PGO_CLAP_TARGET_DIR/dist/libnam_plug.instrumented.so" \
            --instrumentation-file="$PGO_CLAP_TARGET_DIR/libnam_plug.fdata" \
            --instrumentation-file-append-pid > "$BOLT_DIR/bolt-instrument.log" 2>&1

        echo "[Step 2/3] Collecting BOLT workload profiles..."
        RUSTFLAGS="$BASE_RUSTFLAGS" cargo build --locked --profile dist --features testing --bin pgo_profiling_workload >/dev/null 2>&1
        rm -f "$PGO_CLAP_TARGET_DIR"/libnam_plug.fdata.* "$BOLT_PROFILE"

        NAM_CLAP_SO_PATH="$PGO_CLAP_TARGET_DIR/dist/libnam_plug.instrumented.so" \
            "$PROJECT_DIR/target/dist/pgo_profiling_workload" >> "$PGO_WORKLOAD_LOG" 2>&1 || true

        CLAP_FDATA_FILES=()
        while IFS= read -r -d '' f; do
            [ -s "$f" ] && CLAP_FDATA_FILES+=("$f")
        done < <(find "$PGO_CLAP_TARGET_DIR" -maxdepth 1 -name "libnam_plug.fdata.*" -print0 2>/dev/null || true)

        if [ ${#CLAP_FDATA_FILES[@]} -gt 0 ]; then
            "$MERGE_FDATA" "${CLAP_FDATA_FILES[@]}" > "$BOLT_PROFILE" 2>"$BOLT_DIR/merge-fdata.log"
        fi

        if [ -s "$BOLT_PROFILE" ]; then
            echo "[Step 3/3] Applying BOLT optimization..."
            "$LLVM_BOLT" "$COMPILED_SO" -o "$PGO_CLAP_TARGET_DIR/dist/libnam_plug.bolt.so" \
                -data "$BOLT_PROFILE" \
                --reorder-blocks=ext-tsp \
                --reorder-functions=hfsort \
                --relocs \
                --lite > "$BOLT_DIR/llvm-bolt.log" 2>&1
            echo "BOLT optimization completed successfully."
        else
            echo "Failed to produce merged fdata profile for BOLT."
            exit 1
        fi
    } 2>&1 | tee "$BOLT_LOG"
    RC=${PIPESTATUS[0]}

    if [ "$RC" -eq 0 ] && [ -s "$PGO_CLAP_TARGET_DIR/dist/libnam_plug.bolt.so" ]; then
        BOLT_STATUS="applied"
        emit "PHASE: bolt_optimize PASS log=$BOLT_LOG"
    else
        if [ "$DO_STRICT" -eq 1 ]; then
            FAILS+=("bolt_optimize")
            emit "PHASE: bolt_optimize FAIL rc=$RC log=$BOLT_LOG"
            exit 1
        else
            gap "bolt_optimize"
            BOLT_STATUS="failed"
        fi
    fi
else
    gap "bolt_tools_unavailable"
    if [ "$DO_STRICT" -eq 1 ]; then
        echo "build-release: llvm-bolt or merge-fdata unavailable in strict mode" >&2
        exit 1
    fi
fi

# Select target unstripped binary
if [ "$BOLT_STATUS" = "applied" ] && [ -f "$PGO_CLAP_TARGET_DIR/dist/libnam_plug.bolt.so" ]; then
    ACTIVE_SO="$PGO_CLAP_TARGET_DIR/dist/libnam_plug.bolt.so"
else
    ACTIVE_SO="$COMPILED_SO"
fi

# -----------------------------------------------------------------------------
# PHASE 4.5: Assembly Hotpath Disassembly & Codegen Static Checks
# -----------------------------------------------------------------------------
phase "codegen_report"
CODEGEN_LOG="$LOG_DIR/build-release-codegen_report.log"
ASM_TARGET="$PROJECT_DIR/target/dsp_hotpath.asm"
CODEGEN_PRE_JSON="$PROJECT_DIR/target/logs/codegen-pre-bolt.json"
CODEGEN_POST_JSON="$PROJECT_DIR/target/logs/codegen-post-bolt.json"
rm -f "$CODEGEN_PRE_JSON" "$CODEGEN_POST_JSON"

{
    if [ -n "$OBJDUMP_TOOL" ]; then
        echo "Generating assembly hotspot report with $OBJDUMP_TOOL..."
        "$OBJDUMP_TOOL" -d --demangle --no-show-raw-insn -j .text "$ACTIVE_SO" 2>/dev/null | awk '
          BEGIN { in_hot=0 }
          /^[0-9a-fA-F]+ <.*>:$/ {
            sym = $0
            if (sym ~ /neural_amp_modeler_rs::(models|math|dsp)/ || sym ~ /nam_plug::clap::processor/) {
              if (sym !~ /drop_glue|serde|fmt/) {
                in_hot = 1
                print ""
                print sym
                next
              }
            }
            in_hot = 0
          }
          in_hot && /^[[:space:]]+[0-9a-fA-F]+:/ {
            print $0
          }
        ' > "$ASM_TARGET" || true
    fi
} 2>&1 | tee "$CODEGEN_LOG"

CODEGEN_RC=0
if [ -z "$OBJDUMP_TOOL" ]; then
    gap "codegen_objdump_unavailable"
    if [ "$DO_STRICT" -eq 1 ]; then CODEGEN_RC=1; fi
else
    if [ "$BOLT_STATUS" = "applied" ] && [ -f "$COMPILED_SO" ]; then
        "$SCRIPT_DIR/verify_rt_codegen.sh" "$COMPILED_SO" --json-out "$CODEGEN_PRE_JSON" >> "$CODEGEN_LOG" 2>&1 || CODEGEN_RC=1
    fi
    "$SCRIPT_DIR/verify_rt_codegen.sh" "$ACTIVE_SO" --json-out "$CODEGEN_POST_JSON" >> "$CODEGEN_LOG" 2>&1 || CODEGEN_RC=1
fi
if [ "$CODEGEN_RC" -eq 0 ]; then
    emit "PHASE: codegen_report PASS log=$CODEGEN_LOG"
else
    FAILS+=("codegen_report")
    emit "PHASE: codegen_report FAIL rc=$CODEGEN_RC log=$CODEGEN_LOG"
    exit 1
fi

# -----------------------------------------------------------------------------
# PHASE 5: Canonical Artifact Delivery & Link Smoke Test
# -----------------------------------------------------------------------------
phase "deliver_artifact"
DELIVER_LOG="$LOG_DIR/build-release-deliver_artifact.log"
{
    echo "Installing canonical artifact to $CLAP_TARGET..."
    cp "$ACTIVE_SO" "$CLAP_TARGET"
    strip --strip-unneeded "$CLAP_TARGET"

    # Compute SHA-256
    sha256sum "$CLAP_TARGET" | cut -d' ' -f1 > "$CLAP_SHA_FILE"
    CLAP_SHA=$(cat "$CLAP_SHA_FILE")
    echo "Canonical artifact SHA-256: $CLAP_SHA"

    echo "Verifying symbols (clap_entry) and SONAME..."
    nm -D "$CLAP_TARGET" | grep -w "clap_entry" >/dev/null || { echo "Missing clap_entry symbol!"; exit 1; }
    readelf -d "$CLAP_TARGET" | grep SONAME >/dev/null || { echo "Missing SONAME!"; exit 1; }

    echo "Running dynamic linker smoke test (smoke-load)..."
    cargo run --quiet --locked --features testing --bin plug_long_receipt -- smoke-load --artifact "$CLAP_TARGET"
} 2>&1 | tee "$DELIVER_LOG"
RC=${PIPESTATUS[0]}

if [ "$RC" -eq 0 ]; then
    emit "PHASE: deliver_artifact PASS log=$DELIVER_LOG"
else
    FAILS+=("deliver_artifact")
    emit "PHASE: deliver_artifact FAIL rc=$RC log=$DELIVER_LOG"
    exit 1
fi

# -----------------------------------------------------------------------------
# PHASE 6: Packaging (Tarball & Flatpak)
# -----------------------------------------------------------------------------
TARBALL_PATH=""
FLATPAK_PATH=""

if [ "$DO_PACKAGE" = true ]; then
    phase "packaging"
    PKG_LOG="$LOG_DIR/build-release-packaging.log"
    {
        echo "Creating distribution tarball (.tar.zst)..."
        TMP_PKG_DIR=$(mktemp -d -t nam-plug-pkg.XXXXXX)
        mkdir -p "$TMP_PKG_DIR/$ARCHIVE_NAME"
        cp "$CLAP_TARGET" "$TMP_PKG_DIR/$ARCHIVE_NAME/"
        [ -f "README.md" ] && cp "README.md" "$TMP_PKG_DIR/$ARCHIVE_NAME/"
        [ -f "LICENSE" ] && cp "LICENSE" "$TMP_PKG_DIR/$ARCHIVE_NAME/"

        cat << 'EOF' > "$TMP_PKG_DIR/$ARCHIVE_NAME/install.sh"
#!/usr/bin/env bash
set -e
CLAP_DIR="${HOME}/.clap"
mkdir -p "$CLAP_DIR"
cp nam_plug.clap "$CLAP_DIR/"
echo "Installed nam_plug.clap to $CLAP_DIR/nam_plug.clap"
EOF
        chmod +x "$TMP_PKG_DIR/$ARCHIVE_NAME/install.sh"
        tar -C "$TMP_PKG_DIR" -I "zstd -6 -T0" -cf "$TARBALL" "$ARCHIVE_NAME"
        rm -rf "$TMP_PKG_DIR"
        TARBALL_PATH="$TARBALL"
        echo "Generated tarball: $TARBALL"

        if command -v flatpak >/dev/null 2>&1; then
            echo "Building Flatpak bundle..."
            FLATPAK_BUILD_DIR="$(mktemp -d -t nam-plug-flatpak-build.XXXXXX)"
            FLATPAK_REPO_DIR="$(mktemp -d -t nam-plug-flatpak-repo.XXXXXX)"
            SDK_NAME="org.freedesktop.Sdk"
            if ! flatpak info org.freedesktop.Sdk//25.08 >/dev/null 2>&1; then
                SDK_NAME="org.freedesktop.Platform"
            fi
            flatpak build-init --type=extension --extension-tag=25.08 \
                "$FLATPAK_BUILD_DIR" org.freedesktop.LinuxAudio.Plugins.NAMPlug "$SDK_NAME" org.freedesktop.Platform 25.08
            mkdir -p "$FLATPAK_BUILD_DIR/files/clap"
            cp "$CLAP_TARGET" "$FLATPAK_BUILD_DIR/files/clap/nam_plug.clap"
            flatpak build-finish "$FLATPAK_BUILD_DIR" --extension-priority=100
            flatpak build-export --update-appstream "$FLATPAK_REPO_DIR" "$FLATPAK_BUILD_DIR" 25.08
            flatpak build-bundle --runtime "$FLATPAK_REPO_DIR" "$FLATPAK_BUNDLE" org.freedesktop.LinuxAudio.Plugins.NAMPlug 25.08
            rm -rf "$FLATPAK_BUILD_DIR" "$FLATPAK_REPO_DIR"
            FLATPAK_PATH="$FLATPAK_BUNDLE"
            echo "Generated flatpak bundle: $FLATPAK_BUNDLE"
        else
            echo "flatpak command not found; skipping flatpak bundle."
        fi
    } 2>&1 | tee "$PKG_LOG"
    RC=${PIPESTATUS[0]}
    if [ "$RC" -eq 0 ]; then
        emit "PHASE: packaging PASS log=$PKG_LOG"
    else
        FAILS+=("packaging")
        emit "PHASE: packaging FAIL rc=$RC log=$PKG_LOG"
    fi
fi

# -----------------------------------------------------------------------------
# PHASE 7: Local Installation
# -----------------------------------------------------------------------------
INSTALLED_PATH=""
if [ "$DO_INSTALL" = true ]; then
    phase "install"
    INST_LOG="$LOG_DIR/build-release-install.log"
    {
        mkdir -p "$CLAP_INSTALL_DIR"
        cp "$CLAP_TARGET" "$CLAP_INSTALL_DIR/nam_plug.clap"
        INSTALLED_PATH="$CLAP_INSTALL_DIR/nam_plug.clap"
        echo "Installed to $INSTALLED_PATH"
        if [ -n "$FLATPAK_PATH" ] && [ -f "$FLATPAK_PATH" ]; then
            flatpak install --user --reinstall -y "$FLATPAK_PATH" || true
        fi
    } 2>&1 | tee "$INST_LOG"
    RC=${PIPESTATUS[0]}
    if [ "$RC" -eq 0 ]; then
        emit "PHASE: install PASS log=$INST_LOG"
    else
        FAILS+=("install")
        emit "PHASE: install FAIL rc=$RC log=$INST_LOG"
    fi
fi

# -----------------------------------------------------------------------------
# PHASE 8: Build Provenance Receipt Emission (via Rust)
# -----------------------------------------------------------------------------
phase "build_receipt"
RECEIPT_LOG="$LOG_DIR/build-release-build_receipt.log"
RECEIPT_ARGS=(
    "--out" "$BUILD_RECEIPT"
    "--artifact" "$CLAP_TARGET"
    "--package-name" "nam-plug"
    "--package-version" "$VERSION"
    "--git-commit" "$GIT_COMMIT"
    "--git-branch" "$GIT_BRANCH"
    "--git-dirty" "$GIT_DIRTY"
    "--cargo-lock" "Cargo.lock"
    "--rustc-version" "$(rustc --version 2>/dev/null || echo 'unknown')"
    "--rustflags" "$CLAP_BUILD_RUSTFLAGS"
    "--pgo-status" "$PGO_STATUS"
    "--bolt-status" "$BOLT_STATUS"
)

[ -n "$BOLT_VERSION" ] && RECEIPT_ARGS+=("--bolt-version" "$BOLT_VERSION")
[ -f "$BOLT_PROFILE" ] && RECEIPT_ARGS+=("--bolt-profile" "$BOLT_PROFILE")
[ -f "target/logs/ab-opt-ceremony-receipt.json" ] && RECEIPT_ARGS+=("--ceremony-receipt" "target/logs/ab-opt-ceremony-receipt.json")
[ -f "$PGO_WORKLOAD_RECEIPT" ] && RECEIPT_ARGS+=("--pgo-workload-receipt" "$PGO_WORKLOAD_RECEIPT")
[ -f "$CODEGEN_PRE_JSON" ] && RECEIPT_ARGS+=("--codegen-csv-pre" "$CODEGEN_PRE_JSON")
[ -f "$CODEGEN_POST_JSON" ] && RECEIPT_ARGS+=("--codegen-csv-post" "$CODEGEN_POST_JSON")
[ -n "$TARBALL_PATH" ] && RECEIPT_ARGS+=("--tarball-path" "$TARBALL_PATH")
[ -n "$FLATPAK_PATH" ] && RECEIPT_ARGS+=("--flatpak-path" "$FLATPAK_PATH")
[ -n "$INSTALLED_PATH" ] && RECEIPT_ARGS+=("--installed-path" "$INSTALLED_PATH")

if [ ${#GAPS[@]} -gt 0 ]; then
    GAPS_JOINED=$(IFS=,; echo "${GAPS[*]}")
    RECEIPT_ARGS+=("--gaps" "$GAPS_JOINED")
fi

if [ "$DO_STRICT" -eq 1 ]; then
    RECEIPT_ARGS+=("--strict")
fi

cargo run --quiet --locked --features testing --bin plug_long_receipt -- build-receipt "${RECEIPT_ARGS[@]}" 2>&1 | tee "$RECEIPT_LOG"
RC=${PIPESTATUS[0]}

if [ "$RC" -eq 0 ]; then
    emit "PHASE: build_receipt PASS log=$RECEIPT_LOG"
else
    FAILS+=("build_receipt")
    emit "PHASE: build_receipt FAIL rc=$RC log=$RECEIPT_LOG"
fi

finish
