#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
# ab-opt-ceremony.sh — A/B optimization ceremony (operator-only: plain → PGO → PGO+BOLT). Classe D.
# Uso: utils/ab-opt-ceremony.sh [--runs N] [--blocks N] [--no-pgo] [--no-bolt] [--bench-extra 'ARGS'] [-h|--help]
# AI agents must not execute this script; it belongs to the operator release ceremony.
# Orçamento: 350
set -x; PS4='+ ${BASH_SOURCE##*/}:${LINENO}: '
set -euo pipefail
cd "$(dirname "$0")/.."
. utils/_lib.sh
SCRIPT_NAME=ab-opt-ceremony

RUNS=3
BLOCKS=20000
USE_PGO=1
USE_BOLT=1
VARIANT_TIMEOUT_S=1800
COOLDOWN_S="${NAM_THERMAL_COOLDOWN_S:-0}"
AB_DIR="$PWD/target/logs"
AB_BUILD_DIR="$PWD/target/ab-opt-build"
RECEIPT="$AB_DIR/ab-opt-ceremony-receipt.json"
AB_BENCH_FLAGS=()

usage() {
    cat <<'EOF'
Uso: utils/ab-opt-ceremony.sh [OPÇÕES]
  --runs N            Execuções medidas por variante (padrão 3, mínimo 3)
  --blocks N          Blocos DSP por execução medida (padrão 20000)
  --no-pgo            Compara apenas PLAIN (diagnóstico, sem veredito)
  --no-bolt           Compila PGO mas não BOLT (compara plain → PGO)
  --bench-extra 'ARGS'  Flags extras para ab_opt_bench
  -h, --help          Exibe esta ajuda
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --runs) RUNS="$2"; shift 2 ;;
        --blocks) BLOCKS="$2"; shift 2 ;;
        --no-pgo) USE_PGO=0; USE_BOLT=0; shift ;;
        --no-bolt) USE_BOLT=0; shift ;;
        --bench-extra)
            IFS=' ' read -r -a EXTRA <<< "$2"
            AB_BENCH_FLAGS+=(${EXTRA[@]+"${EXTRA[@]}"})
            shift 2
            ;;
        -h|--help) usage; exit 0 ;;
        *) usage_fail "ab-opt-ceremony.sh: opção desconhecida '$1'" ;;
    esac
done

[ "$RUNS" -ge 3 ] 2>/dev/null || usage_fail "--runs exige inteiro >= 3 (recebido: $RUNS)"
[ "$BLOCKS" -ge 1 ] 2>/dev/null || usage_fail "--blocks exige inteiro >= 1 (recebido: $BLOCKS)"
AB_BENCH_FLAGS=(--blocks "$BLOCKS" ${AB_BENCH_FLAGS[@]+"${AB_BENCH_FLAGS[@]}"})
[[ "$COOLDOWN_S" =~ ^[0-9]+$ ]] || usage_fail "NAM_THERMAL_COOLDOWN_S inválido: $COOLDOWN_S"

need() { command -v "$1" >/dev/null 2>&1 || die "'$1' não encontrado no PATH."; }
need rustc; need cargo; need timeout
lib_init
mkdir -p "$AB_DIR"
PARANOID_BEFORE=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo 2)
restore_paranoid() {
    local cur
    cur=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo 2)
    if [ -n "${PARANOID_BEFORE:-}" ] && [ "$cur" != "$PARANOID_BEFORE" ]; then
        if ! sudo -n sysctl -q -w kernel.perf_event_paranoid="$PARANOID_BEFORE" 2>/dev/null; then
            gap perf_paranoid_not_restored
        fi
    fi
}
trap restore_paranoid EXIT
trap 'restore_paranoid; trap_fail $?' ERR
rm -f "$AB_DIR"/ab-opt-plain.json "$AB_DIR"/ab-opt-pgo.json "$AB_DIR"/ab-opt-pgo+bolt.json
rm -f "$AB_DIR"/ab-opt-pgo-gen.json "$AB_DIR"/ab-opt-bolt-probe.json "$AB_DIR"/ab-opt-ceremony-receipt.json
rm -rf "$AB_BUILD_DIR"
mkdir -p "$AB_BUILD_DIR"
export CARGO_TARGET_DIR="$AB_BUILD_DIR"
export CARGO_PROFILE_DIST_STRIP="false"
cargo build --quiet --locked --release --bin plug_ab_opt
COMPARE_BIN="$AB_BUILD_DIR/release/plug_ab_opt"

bench_core=""
if bench_core=$("$COMPARE_BIN" pick-core 2>/dev/null); then
    echo "bench core: $bench_core"
    AB_BENCH_FLAGS=(--cpu "$bench_core" ${AB_BENCH_FLAGS[@]+"${AB_BENCH_FLAGS[@]}"})
else
    gap bench_core_unavailable; echo "  reason: no explicit/isolated/permitted core (running unpinned)"
fi

cooldown() {
    [ "$COOLDOWN_S" -eq 0 ] && return 0
    echo "cooldown: ${COOLDOWN_S}s"
    sleep "$COOLDOWN_S"
}
run_timed() { local t="$1"; shift; timeout --kill-after=30 "$t" "$@"; }

CONFIG_RUSTFLAGS=$(awk '
    /^[[:space:]]*rustflags[[:space:]]*=/ { active = 1 }
    active {
        line = $0; sub(/#.*/, "", line)
        while (match(line, /"[^"]*"/)) {
            printf "%s ", substr(line, RSTART + 1, RLENGTH - 2)
            line = substr(line, RSTART + RLENGTH)
        }
        if (index(line, "]")) active = 0
    }
' .cargo/config.toml)
CONFIG_RUSTFLAGS="${CONFIG_RUSTFLAGS% }"
[ -n "$CONFIG_RUSTFLAGS" ] || die "Não foi possível extrair rustflags de .cargo/config.toml."
CONFIG_RUSTFLAGS="$CONFIG_RUSTFLAGS -C strip=none -Clink-arg=-Wl,-q"

LLVM_PROFDATA=""
if [ "$USE_PGO" = 1 ]; then
    SYSROOT="$(rustc --print sysroot)"
    HOST="$(rustc -vV | sed -n 's/^host: //p')"
    LLVM_PROFDATA="$SYSROOT/lib/rustlib/$HOST/bin/llvm-profdata"
    [ -x "$LLVM_PROFDATA" ] || die "llvm-profdata ausente em $LLVM_PROFDATA (rustup component add llvm-tools-preview)."
fi

LLVM_BOLT=""
MERGE_FDATA=""
HAS_PERF=0
if [ "$USE_BOLT" = 1 ]; then
    for candidate in /usr/lib/llvm-{22,21,20,19,18}/bin/llvm-bolt \
        /usr/bin/llvm-bolt-{22,21} /usr/bin/llvm-bolt; do
        [ -x "$candidate" ] && { LLVM_BOLT="$candidate"; break; }
    done
    if [ -n "$LLVM_BOLT" ]; then
        PERF2BOLT="$(dirname "$LLVM_BOLT")/perf2bolt"
        [ -x "$PERF2BOLT" ] || PERF2BOLT="perf2bolt"
        MERGE_FDATA="$(dirname "$LLVM_BOLT")/merge-fdata"
        [ -x "$MERGE_FDATA" ] || MERF=$(command -v merge-fdata 2>/dev/null || true)
        [ -x "$MERGE_FDATA" ] || MERGE_FDATA="${MERF:-}"
    else
        gap llvm_bolt_unavailable; echo "  reason: llvm-bolt not found (comparing plain → PGO only)"
    fi
    if command -v perf >/dev/null 2>&1; then
        PARANOID=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo 2)
        if [ "$PARANOID" -gt 1 ] && sudo -n sysctl -q -w kernel.perf_event_paranoid=1 2>/dev/null; then
            PARANOID=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo 2)
        fi
        if [ "$PARANOID" -le 1 ]; then
            HAS_PERF=1
        else
            gap perf_event_paranoid; echo "  reason: kernel.perf_event_paranoid=$PARANOID (sudo sysctl -w kernel.perf_event_paranoid=1)"
        fi
    else
        gap perf_unavailable; echo "  reason: perf not found"
    fi
fi

stage_begin() { CUR_STAGE="$1"; CUR_LOG="$AB_DIR/ab-opt-ceremony-$1.log"; : > "$CUR_LOG"; phase "$1"; }
stage_exec() {
    local pst
    set +e; "$@" 2>&1 | tee -a "$CUR_LOG"; pst=("${PIPESTATUS[@]}"); set -e
    [ "${pst[0]}" -eq 0 ] || return "${pst[0]}"
    [ "${pst[1]}" -eq 0 ] || return "${pst[1]}"
}
stage_pass() { emit "PHASE: $CUR_STAGE PASS log=$CUR_LOG"; }

build_variant() {
    local name="$1" flags="$2"
    echo "building variant '$name'"
    stage_exec run_timed "$VARIANT_TIMEOUT_S" env RUSTFLAGS="$flags" \
        cargo build --locked --profile dist --features testing --bin ab_opt_bench
}
run_variant() {
    local name="$1" bin="$2"
    [ -x "$bin" ] || die "binário da variante '$name' ausente: $bin"
    echo "running variant '$name' (runs=$RUNS, blocks=$BLOCKS)"
    stage_exec run_timed "$VARIANT_TIMEOUT_S" "$bin" \
        --variant "$name" --runs "$RUNS" "${AB_BENCH_FLAGS[@]}" \
        --receipt "$AB_DIR/ab-opt-$name.json"
}

echo "A/B optimization ceremony: runs=$RUNS blocks=$BLOCKS flags=${AB_BENCH_FLAGS[*]}"
stage_begin plain
build_variant plain "$CONFIG_RUSTFLAGS"
run_variant plain "$AB_BUILD_DIR/dist/ab_opt_bench"
stage_pass
cooldown

PGO_DIR="$AB_BUILD_DIR/pgo"
BOLT_APPLIED=0
BOLT_CAUSE=""
LBR=0
if [ "$USE_PGO" = 1 ]; then
    stage_begin pgo
    mkdir -p "$PGO_DIR/profraw"
    echo "building variant 'pgo-gen'"
    stage_exec run_timed "$VARIANT_TIMEOUT_S" env \
        RUSTFLAGS="$CONFIG_RUSTFLAGS -Cprofile-generate=$PGO_DIR/profraw -Cllvm-args=-vp-counters-per-site=8" \
        cargo build --locked --profile dist --features testing --bin ab_opt_bench
    echo "running variant 'pgo-gen' (profile capture)"
    stage_exec run_timed "$VARIANT_TIMEOUT_S" env \
        LLVM_PROFILE_FILE="$PGO_DIR/profraw/default_%m_%p.profraw" \
        "$AB_BUILD_DIR/dist/ab_opt_bench" --variant pgo-gen --runs 1 \
        "${AB_BENCH_FLAGS[@]}" --receipt "$AB_DIR/ab-opt-pgo-gen.json"
    PROFRAW_COUNT=$(find "$PGO_DIR/profraw" -name '*.profraw' | wc -l)
    [ "$PROFRAW_COUNT" -gt 0 ] || die "PGO: nenhum .profraw gerado."
    stage_exec run_timed "$VARIANT_TIMEOUT_S" "$LLVM_PROFDATA" merge -sparse \
        -o "$PGO_DIR/merged.profdata" "$PGO_DIR"/profraw/*.profraw
    build_variant pgo "$CONFIG_RUSTFLAGS -Cprofile-use=$PGO_DIR/merged.profdata"
    run_variant pgo "$AB_BUILD_DIR/dist/ab_opt_bench"
    stage_pass
    cooldown

    if [ "$USE_BOLT" = 1 ] && [ -n "$LLVM_BOLT" ] && { [ "$HAS_PERF" = 1 ] || [ -n "$MERGE_FDATA" ]; }; then
        stage_begin bolt
        echo "building & running variant 'pgo+bolt'"
        rm -f "$PGO_DIR/perf.data" "$PGO_DIR"/bolt.fdata.*
        if [ "$HAS_PERF" = 1 ] && run_timed 60 perf record -e cycles:u -j any,u -F 99 -o /dev/null -- true >/dev/null 2>&1; then
            LBR=1
        fi
        if [ "$LBR" = 1 ]; then
            if ! stage_exec run_timed "$VARIANT_TIMEOUT_S" perf record -F 99 -e cycles:u -j any,u -o "$PGO_DIR/perf.data" -- \
                "$AB_BUILD_DIR/dist/ab_opt_bench" --variant pgo-gen --runs 1 \
                "${AB_BENCH_FLAGS[@]}" --receipt "$AB_DIR/ab-opt-bolt-probe.json"; then
                BOLT_CAUSE="BOLT_FAILED: perf record session failed."
            elif [ ! -s "$PGO_DIR/perf.data" ]; then
                BOLT_CAUSE="BOLT_FAILED: perf.data empty or missing."
            elif ! chmod 644 "$PGO_DIR/perf.data" 2>/dev/null || ! run_timed "$VARIANT_TIMEOUT_S" "$PERF2BOLT" \
                -p "$PGO_DIR/perf.data" "$AB_BUILD_DIR/dist/ab_opt_bench" \
                -o "$PGO_DIR/perf.fdata" >"$PGO_DIR/perf2bolt.log" 2>&1; then
                cat "$PGO_DIR/perf2bolt.log" >>"$CUR_LOG" 2>/dev/null || true
                BOLT_CAUSE="BOLT_FAILED: perf2bolt conversion failed: $(tail -n1 "$PGO_DIR/perf2bolt.log" 2>/dev/null || true)"
            fi
        elif [ -n "$MERGE_FDATA" ]; then
            echo "  hardware LBR unavailable; using llvm-bolt -instrument fallback" | tee -a "$CUR_LOG"
            if ! run_timed "$VARIANT_TIMEOUT_S" "$LLVM_BOLT" "$AB_BUILD_DIR/dist/ab_opt_bench" \
                -instrument -o "$AB_BUILD_DIR/dist/ab_opt_bench.instrumented" \
                --instrumentation-file="$PGO_DIR/bolt.fdata" --instrumentation-file-append-pid \
                >"$PGO_DIR/bolt-instrument.log" 2>&1; then
                cat "$PGO_DIR/bolt-instrument.log" >>"$CUR_LOG" 2>/dev/null || true
                BOLT_CAUSE="BOLT_FAILED: llvm-bolt instrumentation failed: $(tail -n1 "$PGO_DIR/bolt-instrument.log" 2>/dev/null || true)"
            elif ! stage_exec run_timed "$VARIANT_TIMEOUT_S" "$AB_BUILD_DIR/dist/ab_opt_bench.instrumented" \
                --variant pgo-gen --runs 1 "${AB_BENCH_FLAGS[@]}" \
                --receipt "$AB_DIR/ab-opt-bolt-probe.json"; then
                BOLT_CAUSE="BOLT_FAILED: instrumented workload failed."
            else
                FDATA_FILES=()
                while IFS= read -r -d '' f; do [ -s "$f" ] && FDATA_FILES+=("$f"); done < <(find "$PGO_DIR" -maxdepth 1 -name 'bolt.fdata.*' -print0 2>/dev/null || true)
                if [ ${#FDATA_FILES[@]} -eq 0 ] || ! run_timed "$VARIANT_TIMEOUT_S" "$MERGE_FDATA" \
                    "${FDATA_FILES[@]}" >"$PGO_DIR/perf.fdata" 2>"$PGO_DIR/merge-fdata.log"; then
                    cat "$PGO_DIR/merge-fdata.log" >>"$CUR_LOG" 2>/dev/null || true
                    BOLT_CAUSE="BOLT_FAILED: merge-fdata produced no representative profile."
                fi
            fi
        else
            BOLT_CAUSE="BOLT_SKIPPED: hardware LBR unavailable and merge-fdata missing"
            gap bolt_lbr_unsupported
            echo "  reason: $BOLT_CAUSE" | tee -a "$CUR_LOG"
        fi
        if [ -z "$BOLT_CAUSE" ] && run_timed "$VARIANT_TIMEOUT_S" "$LLVM_BOLT" \
            "$AB_BUILD_DIR/dist/ab_opt_bench" -o "$AB_BUILD_DIR/dist/ab_opt_bench.bolt" \
            -data "$PGO_DIR/perf.fdata" --reorder-blocks=ext-tsp --reorder-functions=hfsort \
            --relocs --lite >"$PGO_DIR/llvm-bolt.log" 2>&1; then
            cat "$PGO_DIR/llvm-bolt.log" >>"$CUR_LOG" 2>/dev/null || true
            BOLT_APPLIED=1
        elif [ -z "$BOLT_CAUSE" ]; then
            cat "$PGO_DIR/llvm-bolt.log" >>"$CUR_LOG" 2>/dev/null || true
            BOLT_CAUSE="BOLT_FAILED: llvm-bolt failed: $(tail -n1 "$PGO_DIR/llvm-bolt.log" 2>/dev/null || true)"
        fi
        if [ "$BOLT_APPLIED" = 1 ]; then
            run_variant pgo+bolt "$AB_BUILD_DIR/dist/ab_opt_bench.bolt"
            cooldown
        else
            gap bolt_not_applied; echo "  reason: $BOLT_CAUSE" | tee -a "$CUR_LOG"
        fi
        stage_pass
    elif [ "$USE_BOLT" = 1 ]; then
        stage_begin bolt
        BOLT_CAUSE="BOLT_UNAVAILABLE: ${LLVM_BOLT:-llvm-bolt-not-found}/perf=$HAS_PERF"
        gap bolt_skipped; echo "  reason: $BOLT_CAUSE" | tee -a "$CUR_LOG"
        stage_pass
    fi
fi

restore_paranoid
PARANOID_AFTER=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo 2)
META_ARGS=(--dir "$AB_DIR" --paranoid-before "$PARANOID_BEFORE" --paranoid-after "$PARANOID_AFTER" --bolt-applied "$BOLT_APPLIED")
[ -n "$bench_core" ] && META_ARGS+=(--cpu-bench "$bench_core")
if [ "$BOLT_APPLIED" = 1 ]; then
    FDATA_SHA=$(sha256sum "$PGO_DIR/perf.fdata" 2>/dev/null | awk '{print $1}' || echo "unknown")
    if [ "$LBR" = 1 ]; then
        LBR_SAMPLES=$(perf script -i "$PGO_DIR/perf.data" -F brstack 2>/dev/null | grep -cE '0x[0-9a-fA-F]+/0x[0-9a-fA-F]+' || true)
        LBR_MODE="lbr"
    else
        LBR_SAMPLES=$(awk 'NF {n++} END {print n+0}' "$PGO_DIR/perf.fdata" 2>/dev/null || echo 0)
        LBR_MODE="instrumented"
    fi
    [ -n "$LBR_SAMPLES" ] || LBR_SAMPLES=0
    META_ARGS+=(--bolt-samples "$LBR_SAMPLES" --bolt-fdata-sha256 "$FDATA_SHA" --bolt-mode "$LBR_MODE")
fi
"$COMPARE_BIN" write-meta "${META_ARGS[@]}"

if [ "$USE_PGO" = 1 ]; then
    stage_begin verdict
    CAUSE=()
    [ "$BOLT_APPLIED" = 1 ] || CAUSE=(--bolt-cause "${BOLT_CAUSE:-BOLT not requested}")
    RC=0
    stage_exec run_timed 120 "$COMPARE_BIN" compare --dir "$AB_DIR" --out "$RECEIPT" --runs "$RUNS" \
        --bolt-applied "$BOLT_APPLIED" ${CAUSE[@]+"${CAUSE[@]}"} || RC=$?
    if [ "$RC" -eq 0 ]; then
        stage_pass
    else
        FAILS+=(verdict)
        emit "PHASE: verdict FAIL rc=$RC log=$CUR_LOG"
    fi
else
    echo "PLAIN-only diagnostic run; no PGO/BOLT verdict."
fi

restore_paranoid
finish

