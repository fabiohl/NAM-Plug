#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
# tests-long.sh — suíte longa de auditoria, certificação e provas de artefato. Classe B.
# Uso: utils/tests-long.sh [--strict] [--only <fase>[,<fase>]] [--artifact] [-h|--help]
set -x; PS4='+ ${BASH_SOURCE##*/}:${LINENO}: '
set -o pipefail
cd "$(dirname "$0")/.."
. utils/_lib.sh

SCRIPT_NAME="tests-long"
LOG_DIR=target/logs
mkdir -p "$LOG_DIR"
RECEIPT="$LOG_DIR/long-audit-receipt.jsonl"
rm -f "$RECEIPT"

CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target/long}"
export CARGO_TARGET_DIR

USE_ARTIFACT=0
ONLY_PHASES=""
STRICT=0

usage() {
    cat <<'EOF'
Uso: utils/tests-long.sh [OPÇÕES]
  --strict               GAP converte-se em falha; rejeita recibos com lacunas
  --only <fases>         Executa apenas as fases especificadas (vírgula separadas)
  --artifact             Avalia o artefato canônico em target/dist/nam_plug.clap
  -h, --help             Exibe esta mensagem de ajuda
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

# target/long is removed by default unless --only is specified to accelerate iterative reruns
if [ -z "$ONLY_PHASES" ]; then
    rm -rf "$CARGO_TARGET_DIR"
fi

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
    local id=$1 name=$2
    if [ -z "$ONLY_PHASES" ]; then return 0; fi
    [[ "$ONLY_PHASES" == *",$id,"* ]] || [[ "$ONLY_PHASES" == *",$name,"* ]]
}

PLUG_LONG_RECEIPT_BIN="$CARGO_TARGET_DIR/release/plug_long_receipt"
if [ ! -x "$PLUG_LONG_RECEIPT_BIN" ]; then
    cargo build --release --features testing --bin plug_long_receipt >> "$LOG_DIR/tests-long-receipt.log" 2>&1
fi

append_receipt() {
    local pid=$1 pname=$2 pstatus=$3 plog=$4 ptime_ms=$5 ptests=$6 pexercised=$7 pgaps=${8:-}
    local cmd=(
        "$PLUG_LONG_RECEIPT_BIN" append
        --out "$RECEIPT"
        --phase-id "$pid"
        --name "$pname"
        --status "$pstatus"
        --duration-ms "$ptime_ms"
        --log "$plog"
    )
    if [ -n "$ptests" ]; then
        cmd+=(--tests-executed "$ptests")
    fi
    if [ -n "$pgaps" ]; then
        cmd+=(--gaps "$pgaps")
    fi
    if [ -n "$ARTIFACT_PATH" ] && [ -n "$ARTIFACT_SHA256" ]; then
        cmd+=(
            --artifact-path "$ARTIFACT_PATH"
            --artifact-sha256 "$ARTIFACT_SHA256"
            --artifact-exercised "$pexercised"
        )
    fi
    "${cmd[@]}" >> "$LOG_DIR/tests-long-receipt.log" 2>&1 || true
}

GUI_TESTS=(
    gui_embedded_x11_open_close_cycles_no_leak
    gui_lifecycle_hide_after_show_with_real_window
    gui_lifecycle_x11_embedded_set_parent_honest
    test_headless_clipboard_works
    test_headless_gui_floating_window_lifecycle
)

# ── Phase 1: catch-all ───────────────────────────────────────────────────────
if should_run "phase1" "catch-all"; then
    t_start=$SECONDS
    gui_skips=()
    for gtest in "${GUI_TESTS[@]}" gui_lifecycle_integration_test processor_gui_test; do
        gui_skips+=(--skip "$gtest")
    done
    run_phase "phase1" 600 cargo test --release --features testing --no-fail-fast \
        -- --include-ignored "${gui_skips[@]}"
    dur_ms=$(( (SECONDS - t_start) * 1000 ))
    plog="$LOG_DIR/tests-long-phase1.log"
    pstatus="PASSED"
    if [[ " ${FAILS[*]} " =~ " phase1 " ]]; then
        pstatus="FAILED"
    fi
    append_receipt "phase1" "catch-all" "$pstatus" "$plog" "$dur_ms" "" "false"
else
    append_receipt "phase1" "catch-all" "SKIPPED" "/dev/null" 0 0 "false"
fi

# ── Phase 2: gui ─────────────────────────────────────────────────────────────
if should_run "phase2" "gui"; then
    t_start=$SECONDS
    phase "phase2"
    plog="$LOG_DIR/tests-long-phase2.log"
    : > "$plog"

    if ! command -v xvfb-run >/dev/null 2>&1; then
        gap "gui:xvfb_not_found"
        dur_ms=$(( (SECONDS - t_start) * 1000 ))
        append_receipt "phase2" "gui" "SKIPPED" "$plog" "$dur_ms" 0 "false" "gui:xvfb_not_found"
    else
        gui_rc=0
        gui_passed=0
        for gui_test in "${GUI_TESTS[@]}"; do
            echo "--- GUI test: $gui_test (${GUI_TESTS[*]}) ---" >> "$plog"
            LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe \
            NAM_GUI_DEDICATED_X11=1 NAM_GUI_TIMEOUT_MS="${NAM_GUI_TIMEOUT_MS:-15000}" \
                xvfb-run -a --server-args="-screen 0 1280x800x24" \
                cargo test --features testing --release --lib "$gui_test" \
                    -- --ignored --test-threads=1 2>&1 | tee -a "$plog"
            test_rc=${PIPESTATUS[0]}
            if [ "$test_rc" -ne 0 ]; then
                gui_rc=1
            else
                ((gui_passed++)) || true
            fi
        done

        dur_ms=$(( (SECONDS - t_start) * 1000 ))
        if [ "$gui_rc" -eq 0 ] && ran_tests "$plog"; then
            echo "PHASE: phase2 PASS log=$plog"
            pstatus="PASSED"
        else
            FAILS+=("phase2")
            echo "PHASE: phase2 FAIL rc=$gui_rc log=$plog"
            pstatus="FAILED"
        fi
        append_receipt "phase2" "gui" "$pstatus" "$plog" "$dur_ms" "$gui_passed" "false"
    fi
else
    append_receipt "phase2" "gui" "SKIPPED" "/dev/null" 0 0 "false"
fi

# ── Phase 3: artifact ────────────────────────────────────────────────────────
if should_run "phase3" "artifact"; then
    t_start=$SECONDS
    phase "phase3"
    plog="$LOG_DIR/tests-long-phase3.log"
    : > "$plog"

    plugin_under_test=""
    exercised="false"
    p3_gaps=""

    if [ "$USE_ARTIFACT" = "1" ]; then
        plugin_under_test="$ARTIFACT_PATH"
        exercised="true"
    else
        echo "Building neutral release artifact..." >> "$plog"
        CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
            cargo build --release --features testing 2>&1 | tee -a "$plog"
        candidate="$CARGO_TARGET_DIR/release/libnam_plug.so"
        if [ -f "$candidate" ]; then
            plugin_under_test="$candidate"
        else
            echo "ERROR: Neutral artifact $candidate not found" >> "$plog"
        fi
    fi

    art_rc=0
    if [ -n "$plugin_under_test" ] && [ -f "$plugin_under_test" ]; then
        export CLAP_PLUGIN_UNDER_TEST="$plugin_under_test"
        [ "$STRICT" = "1" ] && { export NAM_RT_STRICT=1; export NAM_REQUIRE_CPP_ORACLE=1; }

        # Locate NAMCore C++ render binary if available
        render_bin="${NAM_CORE_RENDER_BIN:-}"
        if [ -z "$render_bin" ] && [ -x "build/namcore_render" ]; then
            render_bin="build/namcore_render"
        fi

        if [ -n "$render_bin" ] && [ -x "$render_bin" ]; then
            export NAM_CORE_RENDER_BIN="$render_bin"
        else
            gap "clap_parity:missing_cpp_oracle"; p3_gaps="${p3_gaps:+$p3_gaps,}clap_parity:missing_cpp_oracle"
        fi

        # Run parity and CabSim tests against the plugin artifact
        ARTIFACT_TESTS=(
            test_clap_parity_multi_rate
            test_clap_parity_oversample
            test_cabsim_ir_changes_audio_release_artifact
            test_clap_parity_cabsim
        )
        art_rc=0
        art_passed=0
        for atest in "${ARTIFACT_TESTS[@]}"; do
            echo "--- Running artifact test: $atest ---" >> "$plog"
            cargo test --features testing --release --test clap "$atest" \
                -- --ignored --nocapture 2>&1 | tee -a "$plog"
            sub_rc=${PIPESTATUS[0]}
            if [ "$sub_rc" -ne 0 ]; then
                art_rc=1
            else
                ((art_passed++)) || true
            fi
        done

        # clap-validator if installed
        if [ "$art_rc" -eq 0 ]; then
            if command -v clap-validator >/dev/null 2>&1; then
                clap-validator validate "$plugin_under_test" 2>&1 | tee -a "$plog" || art_rc=$?
            else
                gap "artifact:clap_validator_missing"; p3_gaps="${p3_gaps:+$p3_gaps,}artifact:clap_validator_missing"
            fi
        fi
    else
        art_rc=1
    fi

    dur_ms=$(( (SECONDS - t_start) * 1000 ))
    if [ "$art_rc" -eq 0 ] && ran_tests "$plog"; then
        echo "PHASE: phase3 PASS log=$plog"
        pstatus="PASSED"
    else
        FAILS+=("phase3")
        echo "PHASE: phase3 FAIL rc=$art_rc log=$plog"
        pstatus="FAILED"
    fi
    append_receipt "phase3" "artifact" "$pstatus" "$plog" "$dur_ms" "$art_passed" "$exercised" "$p3_gaps"
else
    append_receipt "phase3" "artifact" "SKIPPED" "/dev/null" 0 0 "false"
fi

# ── Phase 4: perf ────────────────────────────────────────────────────────────
if should_run "phase4" "perf"; then
    t_start=$SECONDS
    phase "phase4"
    plog="$LOG_DIR/tests-long-phase4.log"
    : > "$plog"

    if [ "$USE_ARTIFACT" = "1" ]; then
        cargo run --release --features testing --bin nam_perf_guard -- \
            certify --clap "$ARTIFACT_PATH" 2>&1 | tee -a "$plog"
        perf_rc=${PIPESTATUS[0]}
        dur_ms=$(( (SECONDS - t_start) * 1000 ))
        if [ "$perf_rc" -eq 0 ]; then
            echo "PHASE: phase4 PASS log=$plog"
            pstatus="PASSED"
        else
            FAILS+=("phase4")
            echo "PHASE: phase4 FAIL rc=$perf_rc log=$plog"
            pstatus="FAILED"
        fi
        append_receipt "phase4" "perf" "$pstatus" "$plog" "$dur_ms" 1 "true"  # one gate: nam_perf_guard certify (validator rejects PASSED with 0 tests)
    else
        gap "perf:requires_artifact"
        dur_ms=$(( (SECONDS - t_start) * 1000 ))
        append_receipt "phase4" "perf" "SKIPPED" "$plog" "$dur_ms" 0 "false" "perf:requires_artifact"
    fi
else
    append_receipt "phase4" "perf" "SKIPPED" "/dev/null" 0 0 "false"
fi

# ── Phase 5: heap-audit ──────────────────────────────────────────────────────
if should_run "phase5" "heap-audit"; then
    t_start=$SECONDS
    phase "phase5"
    plog="$LOG_DIR/tests-long-phase5.log"
    : > "$plog"

    heap_rc=0
    heap_passed=0

    run_heap_test() {
        echo "--- Running heap/RT test: $* ---" >> "$plog"
        cargo test --features testing --release "$@" 2>&1 | tee -a "$plog"
        local rc=${PIPESTATUS[0]}
        if [ "$rc" -ne 0 ]; then
            heap_rc=1
        else
            ((heap_passed++)) || true
        fi
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
    cargo test --features testing,heap-audit --release --lib test_corrupted_states_10000_heap_stable \
        -- --ignored --test-threads=1 2>&1 | tee -a "$plog"
    tls_rc=${PIPESTATUS[0]}
    if [ "$tls_rc" -ne 0 ]; then
        heap_rc=1
    else
        ((heap_passed++)) || true
    fi

    dur_ms=$(( (SECONDS - t_start) * 1000 ))
    if [ "$heap_rc" -eq 0 ] && ran_tests "$plog"; then
        echo "PHASE: phase5 PASS log=$plog"
        pstatus="PASSED"
    else
        FAILS+=("phase5")
        echo "PHASE: phase5 FAIL rc=$heap_rc log=$plog"
        pstatus="FAILED"
    fi
    append_receipt "phase5" "heap-audit" "$pstatus" "$plog" "$dur_ms" "$heap_passed" "false"
else
    append_receipt "phase5" "heap-audit" "SKIPPED" "/dev/null" 0 0 "false"
fi

# ── Terminal receipt derivation & verification ───────────────────────────────
"$PLUG_LONG_RECEIPT_BIN" summary --out "$RECEIPT" >> "$LOG_DIR/tests-long-receipt.log" 2>&1 || true

val_cmd=("$PLUG_LONG_RECEIPT_BIN" validate --out "$RECEIPT")
[ "$STRICT" = "1" ] && val_cmd+=(--strict)
if ! "${val_cmd[@]}" >> "$LOG_DIR/tests-long-receipt.log" 2>&1; then
    FAILS+=("receipt_validation")
fi

finish
