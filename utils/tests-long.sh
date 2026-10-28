#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
# tests-long.sh — suíte longa de auditoria, certificação e provas de artefato. Classe B.
# Uso: utils/tests-long.sh [--strict] [--only <fase>[,<fase>]] [--artifact] [-h|--help]
# Orçamento: 300
set -x; PS4='+ ${BASH_SOURCE##*/}:${LINENO}: '
set -o pipefail
cd "$(dirname "$0")/.."
. utils/_lib.sh

SCRIPT_NAME="tests-long"
LOG_DIR=target/logs
mkdir -p "$LOG_DIR"
RECEIPT="$LOG_DIR/long-audit-receipt.jsonl"

CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target/long}"
export CARGO_TARGET_DIR

USE_ARTIFACT=0
ONLY_PHASES=""
STRICT=0
# long: fases dedicadas/pinadas (gui: gui_x11_ sob xvfb; heap-audit: micro-latência com --test-threads=1)
SKIP=(gui_x11_ processor_drain_latency_test test_reset_wall_clock_time_within_contract test_structural_burst_p99_within_contract)

usage() {
    cat <<'EOF'
Uso: utils/tests-long.sh [OPÇÕES]
  --strict               GAP converte-se em falha; rejeita recibos com lacunas
  --only <fases>         Executa apenas as fases nomeadas (vírgula separadas)
  --artifact             Avalia o artefato canônico em target/dist/nam_plug.clap
  -h, --help             Exibe esta mensagem de ajuda

Fases:
  catch-all     cargo test --release --include-ignored, exceto gui_x11_ e micro-latência
  gui           testes gui_x11_ ignorados sob xvfb-run, um a um
  artifact      paridade CLAP/CabSim e clap-validator contra o artefato
  perf          nam_perf_guard certify no artefato (--artifact)
  heap-audit    GC, teardown, RT priority/latency e soak de heap
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --strict)
            STRICT=1
            shift
            ;;
        --only)
            [ $# -ge 2 ] || { echo "ERRO: --only requer argumento" >&2; exit 2; }
            ONLY_PHASES=",$2,"
            shift 2
            ;;
        --artifact)
            USE_ARTIFACT=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "ERRO: flag não reconhecida: $1" >&2
            usage
            exit 2
            ;;
    esac
done

PHASES=(catch-all gui artifact perf heap-audit)
if [ -n "$ONLY_PHASES" ]; then
    IFS=',' read -ra requested <<< "$ONLY_PHASES"
    for p in "${requested[@]}"; do
        [ -n "$p" ] || continue
        [[ " ${PHASES[*]} " == *" $p "* ]] || { echo "ERRO: fase desconhecida: $p" >&2; usage; exit 2; }
    done
fi

# target/long is removed by default unless --only is specified to accelerate iterative reruns
if [ -z "$ONLY_PHASES" ]; then
    rm -rf "$CARGO_TARGET_DIR"
fi
rm -f "$RECEIPT"

ARTIFACT_PATH=""
ARTIFACT_SHA256=""
if [ "$USE_ARTIFACT" = "1" ]; then
    ARTIFACT_PATH="target/dist/nam_plug.clap"
    if [ ! -f "$ARTIFACT_PATH" ]; then
        echo "ERRO: --artifact especificado mas $ARTIFACT_PATH não existe." >&2
        exit 2
    fi
    ARTIFACT_SHA256=$(sha256sum "$ARTIFACT_PATH" | cut -d' ' -f1)
fi

should_run() {
    [ -z "$ONLY_PHASES" ] && return 0
    [[ "$ONLY_PHASES" == *",$1,"* ]]
}

PLUG_LONG_RECEIPT_BIN="$CARGO_TARGET_DIR/release/plug_long_receipt"
if [ ! -x "$PLUG_LONG_RECEIPT_BIN" ]; then
    cargo build --release --features testing --bin plug_long_receipt >> "$LOG_DIR/tests-long-receipt.log" 2>&1
fi

# append_receipt <phase-id> <nome> <status> <log> <ms> <tests> <exercised> [gaps]
append_receipt() {
    local pid=$1 pname=$2 pstatus=$3 plog=$4 ptime_ms=$5 ptests=$6 pexercised=$7 pgaps=${8:-}
    local cmd=("$PLUG_LONG_RECEIPT_BIN" append --out "$RECEIPT" --phase-id "$pid" --name "$pname"
        --status "$pstatus" --duration-ms "$ptime_ms" --log "$plog")
    [ -n "$ptests" ] && cmd+=(--tests-executed "$ptests")
    [ -n "$pgaps" ] && cmd+=(--gaps "$pgaps")
    [ -n "$ARTIFACT_PATH" ] && [ -n "$ARTIFACT_SHA256" ] && cmd+=(
        --artifact-path "$ARTIFACT_PATH" --artifact-sha256 "$ARTIFACT_SHA256" --artifact-exercised "$pexercised")
    "${cmd[@]}" >> "$LOG_DIR/tests-long-receipt.log" 2>&1 || {
        FAILS+=("receipt-$pid")
        emit "PHASE: receipt-$pid FAIL"
    }
}

# close_phase <pid> <nome> <rc> <log> <dur_ms> <tests> <exercised> [gaps]
close_phase() {
    local pid=$1 pname=$2 rc=$3 plog=$4 dur=$5 tests=$6 ex=$7 pgaps=${8:-} st=PASSED
    if [ "$rc" -eq 0 ] && ran_tests "$plog"; then
        emit "PHASE: $pname PASS log=$plog"
    else
        FAILS+=("$pid")
        emit "PHASE: $pname FAIL rc=$rc log=$plog"
        st=FAILED
    fi
    append_receipt "$pid" "$pname" "$st" "$plog" "$dur" "$tests" "$ex" "$pgaps"
}

# skip_phase <pid> <nome> [gaps]
skip_phase() {
    local log="/dev/null" gaps=${3:-}
    [ -n "$gaps" ] && log="$LOG_DIR/long-$2.log"
    append_receipt "$1" "$2" SKIPPED "$log" 0 0 false "$gaps"
}

# ── Fase catch-all ───────────────────────────────────────────────────────────
if should_run "catch-all"; then
    t_start=$SECONDS; plog="$LOG_DIR/long-catch-all.log"; phase "catch-all"
    skips=(); for s in "${SKIP[@]}"; do skips+=(--skip "$s"); done
    # timeout: pior comando ~230s (build LTO + testes); 2x p100 ≈ 500s
    timeout --kill-after=30 "${NAM_PHASE_TIMEOUT_S:-500}" cargo test --release --features testing --no-fail-fast \
        -- --include-ignored "${skips[@]}" 2>&1 | tee "$plog"
    catch_rc=${PIPESTATUS[0]}
    close_phase "phase1" "catch-all" "$catch_rc" "$plog" $(( (SECONDS - t_start) * 1000 )) "" false
else
    skip_phase "phase1" "catch-all"
fi
# ── Fase gui ─────────────────────────────────────────────────────────────────
if should_run "gui"; then
    t_start=$SECONDS; phase "gui"
    plog="$LOG_DIR/long-gui.log"; : > "$plog"
    if ! command -v xvfb-run >/dev/null 2>&1; then
        gap "gui:xvfb_not_found"
        append_receipt "phase2" "gui" "SKIPPED" "$plog" $(( (SECONDS - t_start) * 1000 )) 0 false "gui:xvfb_not_found"
    else
        # timeout: pior teste ~1.2s (~120s frio); 2x p100 ≈ 300s
        gui_to=${NAM_PHASE_TIMEOUT_S:-300}; gui_rc=0
        for gt in gui_x11_embedded_open_close_cycles_no_leak gui_x11_lifecycle_embedded_set_parent_honest gui_x11_lifecycle_hide_after_show_with_real_window gui_x11_headless_clipboard_works gui_x11_headless_floating_window_lifecycle; do
            LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe NAM_GUI_DEDICATED_X11=1 NAM_GUI_TIMEOUT_MS="${NAM_GUI_TIMEOUT_MS:-15000}" \
                timeout --kill-after=30 "$gui_to" xvfb-run -a --server-args="-screen 0 1280x800x24" \
                cargo test --features testing --release --lib "$gt" -- --ignored < /dev/null 2>&1 | tee -a "$plog" || gui_rc=1
        done
        close_phase "phase2" "gui" "$gui_rc" "$plog" $(( (SECONDS - t_start) * 1000 )) "" false
    fi
else
    skip_phase "phase2" "gui"
fi

# ── Fase artifact ────────────────────────────────────────────────────────────
if should_run "artifact"; then
    t_start=$SECONDS; phase "artifact"
    plog="$LOG_DIR/long-artifact.log"; : > "$plog"
    # timeout: pior comando ~180s (build neutral .so); 2x p100 ≈ 400s
    plugin_under_test=""; exercised="false"; art_gaps=""; art_to=${NAM_PHASE_TIMEOUT_S:-400}
    if [ "$USE_ARTIFACT" = "1" ]; then
        plugin_under_test="$ARTIFACT_PATH"
        exercised="true"
    else
        candidate="$CARGO_TARGET_DIR/release/libnam_plug.so"; rm -f "$candidate"
        echo "Building neutral release artifact..." >> "$plog"
        CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
            timeout --kill-after=30 "$art_to" cargo build --release --features testing 2>&1 | tee -a "$plog"; build_rc=${PIPESTATUS[0]}
        if [ "$build_rc" -ne 0 ]; then echo "ERROR: Neutral artifact build failed (rc=$build_rc)" >> "$plog"
        elif [ -f "$candidate" ]; then plugin_under_test="$candidate"
        else echo "ERROR: Neutral artifact $candidate not found" >> "$plog"; fi
    fi

    art_rc=0
    art_passed=0
    if [ -n "$plugin_under_test" ] && [ -f "$plugin_under_test" ]; then
        export CLAP_PLUGIN_UNDER_TEST="$plugin_under_test"
        [ "$STRICT" = "1" ] && { export NAM_RT_STRICT=1; export NAM_REQUIRE_CPP_ORACLE=1; }

        # Locate NAMCore C++ render binary if available
        render_bin="${NAM_CORE_RENDER_BIN:-}"
        [ -z "$render_bin" ] && [ -x "build/namcore_render" ] && render_bin="build/namcore_render"
        if [ -n "$render_bin" ] && [ -x "$render_bin" ]; then
            export NAM_CORE_RENDER_BIN="$render_bin"
        else
            gap "clap_parity:missing_cpp_oracle"; art_gaps="${art_gaps:+$art_gaps,}clap_parity:missing_cpp_oracle"
        fi

        # Run parity and CabSim tests against the plugin artifact
        ARTIFACT_TESTS=(test_clap_parity_multi_rate test_clap_parity_oversample
            test_cabsim_ir_changes_audio_release_artifact test_clap_parity_cabsim)
        for atest in "${ARTIFACT_TESTS[@]}"; do
            echo "--- Running artifact test: $atest ---" >> "$plog"
            timeout --kill-after=30 "$art_to" cargo test --features testing --release --test clap "$atest" \
                -- --ignored --nocapture 2>&1 | tee -a "$plog"
            sub_rc=${PIPESTATUS[0]}
            [ "$sub_rc" -eq 0 ] && art_passed=$((art_passed + 1)) || art_rc=1
        done

        # clap-validator if installed
        if [ "$art_rc" -eq 0 ]; then
            if command -v clap-validator >/dev/null 2>&1; then
                timeout --kill-after=30 "$art_to" clap-validator validate "$plugin_under_test" 2>&1 | tee -a "$plog"
                val_rc=${PIPESTATUS[0]}
                [ "$val_rc" -eq 0 ] || art_rc=$val_rc
            else
                gap "artifact:clap_validator_missing"; art_gaps="${art_gaps:+$art_gaps,}artifact:clap_validator_missing"
            fi
        fi
    else
        art_rc=1
    fi

    close_phase "phase3" "artifact" "$art_rc" "$plog" $(( (SECONDS - t_start) * 1000 )) \
        "$art_passed" "$exercised" "$art_gaps"
else
    skip_phase "phase3" "artifact"
fi
# ── Fase perf ────────────────────────────────────────────────────────────────
if should_run "perf"; then
    t_start=$SECONDS; phase "perf"
    plog="$LOG_DIR/long-perf.log"; : > "$plog"
    if [ "$USE_ARTIFACT" = "1" ]; then
        # timeout: pior comando ~30s (nam_perf_guard certify); 2x p100 ≈ 120s
        timeout --kill-after=30 "${NAM_PHASE_TIMEOUT_S:-120}" cargo run --release --features testing \
            --bin nam_perf_guard -- certify --clap "$ARTIFACT_PATH" 2>&1 | tee -a "$plog"
        perf_rc=${PIPESTATUS[0]}
        # one gate: nam_perf_guard certify (validator rejects PASSED with 0 tests)
        close_phase "phase4" "perf" "$perf_rc" "$plog" $(( (SECONDS - t_start) * 1000 )) 1 true
    else
        gap "perf:requires_artifact"
        append_receipt "phase4" "perf" "SKIPPED" "$plog" $(( (SECONDS - t_start) * 1000 )) 0 false "perf:requires_artifact"
    fi
else
    skip_phase "phase4" "perf"
fi
# ── Fase heap-audit ──────────────────────────────────────────────────────────
if should_run "heap-audit"; then
    t_start=$SECONDS; phase "heap-audit"
    plog="$LOG_DIR/long-heap-audit.log"; : > "$plog"
    # timeout: pior comando ~215s (build heap-audit + soak); 2x p100 ≈ 500s
    heap_rc=0; heap_passed=0; heap_to=${NAM_PHASE_TIMEOUT_S:-500}
    run_heap_test() {
        echo "--- Running heap/RT test: $* ---" >> "$plog"
        timeout --kill-after=30 "$heap_to" cargo test --features testing --release "$@" 2>&1 | tee -a "$plog"
        local rc=${PIPESTATUS[0]}
        [ "$rc" -eq 0 ] && heap_passed=$((heap_passed + 1)) || heap_rc=1
    }

    run_heap_test --lib test_gc_stress_1000_swaps -- --ignored
    run_heap_test --lib test_gc_drain_on_destroy_no_leak -- --ignored
    run_heap_test --lib test_teardown_drains_rt_parking_lot_off_rt -- --ignored
    run_heap_test --test clap test_multi_instance_rt_priority -- --ignored --test-threads=1
    run_heap_test --test heap_audit prop_gc_swap_idempotence -- --ignored
    run_heap_test --lib test_structural_burst_p99_within_contract -- --ignored --test-threads=1
    run_heap_test --lib processor_drain_latency_test -- --ignored --test-threads=1
    run_heap_test --lib test_reset_wall_clock_time_within_contract -- --ignored --test-threads=1

    echo "--- Running TLS heap-stability soak ---" >> "$plog"
    timeout --kill-after=30 "$heap_to" cargo test --features testing,heap-audit --release --lib \
        test_corrupted_states_10000_heap_stable -- --ignored --test-threads=1 2>&1 | tee -a "$plog"
    tls_rc=${PIPESTATUS[0]}
    [ "$tls_rc" -eq 0 ] && heap_passed=$((heap_passed + 1)) || heap_rc=1

    close_phase "phase5" "heap-audit" "$heap_rc" "$plog" $(( (SECONDS - t_start) * 1000 )) "$heap_passed"
else
    skip_phase "phase5" "heap-audit"
fi

# ── Terminal receipt derivation & verification ───────────────────────────────
"$PLUG_LONG_RECEIPT_BIN" summary --out "$RECEIPT" >> "$LOG_DIR/tests-long-receipt.log" 2>&1 \
    || { FAILS+=("receipt-summary"); emit "PHASE: receipt-summary FAIL"; }

val_cmd=("$PLUG_LONG_RECEIPT_BIN" validate --out "$RECEIPT")
[ "$STRICT" = "1" ] && val_cmd+=(--strict)
"${val_cmd[@]}" >> "$LOG_DIR/tests-long-receipt.log" 2>&1 || FAILS+=("receipt_validation")

finish
