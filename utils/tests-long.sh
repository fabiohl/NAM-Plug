#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
# tests-long.sh — suíte longa de auditoria, certificação e provas de artefato. Classe B.
# Uso: utils/tests-long.sh [--strict] [--only <fase>[,<fase>]] [--artifact] [-h|--help]
# Orçamento: 400         # +20: recibo por rodada em .partial com mv atômico e publicação condicional (S3-T2)
set -x; PS4='+ ${BASH_SOURCE##*/}:${LINENO}: '
set -o pipefail
cd "$(dirname "$0")/.."
. utils/_lib.sh

SCRIPT_NAME="tests-long"
LOG_DIR=target/logs
DEFAULT_TARGET_DIR="target/long"
RECEIPT_FINAL="$LOG_DIR/long-audit-receipt.jsonl"

CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$DEFAULT_TARGET_DIR}"
export CARGO_TARGET_DIR
USE_ARTIFACT=0; ONLY_PHASES=""; STRICT=0

usage() {
    cat <<'EOF'
Uso: utils/tests-long.sh [OPÇÕES]
  --strict               GAP converte-se em falha; rejeita recibos com lacunas
  --only <fases>         Executa apenas as fases nomeadas (vírgula separadas)
  --artifact             Avalia o artefato canônico em target/dist/nam_plug.clap
  -h, --help             Exibe esta mensagem de ajuda

Fases:
  catch-all     cargo test release não-ignorado (build separado do timeout de execução)
  gui           testes gui_x11_ ignorados sob xvfb-run, um a um
  artifact      paridade CLAP/CabSim e clap-validator contra o artefato
  perf          nam_perf_guard certify no artefato (--artifact)
  heap-audit    GC, teardown, prioridade RT e soaks de heap (sem tempo)
  rt-heap       limiares de tempo RT (operador-only; nunca no CI hospedado)
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --strict) STRICT=1; shift ;;
        --only) [ $# -ge 2 ] || { echo "ERRO: --only requer argumento" >&2; exit 2; }; ONLY_PHASES=",$2,"; shift 2 ;;
        --artifact) USE_ARTIFACT=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "ERRO: flag não reconhecida: $1" >&2; usage; exit 2 ;;
    esac
done

# Reuse the build run id so the independent build/long/certify processes
# reconcile instead of each generating a different id.
if [ -z "${NAM_RUN_ID:-}" ] && [ -s target/logs/build-run-id ]; then
    IFS= read -r NAM_RUN_ID < target/logs/build-run-id || true
    export NAM_RUN_ID
fi
lib_init

PHASES=(catch-all gui artifact perf heap-audit rt-heap)
if [ -n "$ONLY_PHASES" ]; then
    IFS=',' read -ra requested <<< "$ONLY_PHASES"
    for p in "${requested[@]}"; do
        [ -n "$p" ] || continue
        [[ " ${PHASES[*]} " == *" $p "* ]] || { echo "ERRO: fase desconhecida: $p" >&2; usage; exit 2; }
    done
fi

# Only an unrestricted run may replace the canonical receipt. A partial --only
# run writes a per-run sibling receipt and preserves the previous full evidence.
RECEIPT="$LOG_DIR/long-audit-receipt.${NAM_RUN_ID}.partial.jsonl"

# target/long is removed by default unless --only is specified to accelerate
# iterative reruns. A custom CARGO_TARGET_DIR is operator data and is never
# removed: the script has no authority to delete an arbitrary directory.
if [ -z "$ONLY_PHASES" ]; then
    if [ "$CARGO_TARGET_DIR" = "$DEFAULT_TARGET_DIR" ]; then
        rm -rf "$DEFAULT_TARGET_DIR"
    else
        emit "PHASE: long-cleanup SKIPPED custom_cargo_target_dir=$CARGO_TARGET_DIR"
    fi
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
    cargo build --locked --release --features testing --bin plug_long_receipt >> "$LOG_DIR/tests-long-receipt.log" 2>&1
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

# close_phase <pid> <nome> <rc> <log> <dur_ms> <tests> <exercised> [gaps] [requires_tests]
# `requires_tests=0` is used by phases that certify an artifact instead of
# running a Cargo test binary (perf), so `ran_tests` does not apply.
close_phase() {
    local pid=$1 pname=$2 rc=$3 plog=$4 dur=$5 tests=$6 ex=${7:-false} pgaps=${8:-} req_tests=${9:-1} st=PASSED
    if [ "$rc" -eq 0 ] && { [ "$req_tests" = "0" ] || ran_tests "$plog"; }; then
        emit "PHASE: $pname PASS log=$plog"
    else
        FAILS+=("$pid")
        emit "PHASE: $pname FAIL rc=$rc log=$plog"; tail -n 40 "$plog" >&2
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

# run_named <phase_tag> <test_name> <cmd...>
run_named() {
    local ptag=$1 sname="${2##*::}" sublog="$LOG_DIR/long-$1-${2##*::}.log" pst
    shift 2
    echo "--- Running $ptag test: $sname ---" >> "$plog"
    "$@" < /dev/null 2>&1 | tee "$sublog" >> "$plog"
    pst=("${PIPESTATUS[@]}")
    [ "${pst[0]}" -eq 0 ] && [ "${pst[1]}" -eq 0 ] && ran_tests "$sublog"
}
# Declared subcase cardinality for receipt counts: matrix/property tests execute
# more cases than the single Cargo invocation the log parser can observe.
subcase_count() {
    case "$1" in
        *test_clap_parity_multi_rate) echo 9 ;;   # 3 fixtures x 3 host rates
        *test_clap_parity_oversample) echo 2 ;;   # X2 + X4
        *prop_gc_swap_idempotence) echo 500 ;;    # proptest cases
        *) echo 1 ;;
    esac
}
# ── Fase catch-all ───────────────────────────────────────────────────────────
if should_run "catch-all"; then
    t_start=$SECONDS; plog="$LOG_DIR/long-catch-all.log"; phase "catch-all"
    # Build outside the execution timeout; every #[ignore] suite has a dedicated phase.
    timeout --kill-after=30 1200 cargo test --locked --release --features testing --no-run > "$plog" 2>&1; catch_rc=$?
    if [ "$catch_rc" -eq 0 ]; then
        timeout --kill-after=30 "${NAM_PHASE_TIMEOUT_S:-300}" cargo test --locked --release --features testing --no-fail-fast --lib --bins --tests 2>&1 | tee -a "$plog"
        pst=("${PIPESTATUS[@]}"); catch_rc=${pst[0]}
        [ "${pst[1]}" -eq 0 ] || catch_rc=1
    fi
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
        gui_to=${NAM_PHASE_TIMEOUT_S:-300}; gui_rc=0; gui_passed=0
        GUI_TESTS=(
            clap::extensions::gui::gui_lifecycle_integration_test::gui_x11_embedded_open_close_cycles_no_leak
            clap::extensions::gui::gui_lifecycle_integration_test::gui_x11_lifecycle_embedded_set_parent_honest
            clap::extensions::gui::gui_lifecycle_integration_test::gui_x11_lifecycle_hide_after_show_with_real_window
            clap::processor::processor_gui_test::tests::gui_x11_headless_clipboard_works
            clap::processor::processor_gui_test::tests::gui_x11_headless_floating_window_lifecycle
        )
        for gt in "${GUI_TESTS[@]}"; do
            LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe NAM_GUI_DEDICATED_X11=1 NAM_GUI_TIMEOUT_MS="${NAM_GUI_TIMEOUT_MS:-15000}" \
                run_named "gui" "$gt" timeout --kill-after=30 "$gui_to" xvfb-run -a --server-args="-screen 0 1280x800x24" \
                cargo test --locked --features testing,gui-x11 --release --lib "$gt" -- --ignored --exact \
                && gui_passed=$((gui_passed + 1)) || gui_rc=1
        done
        close_phase "phase2" "gui" "$gui_rc" "$plog" $(( (SECONDS - t_start) * 1000 )) "$gui_passed" false
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
            timeout --kill-after=30 "$art_to" cargo build --locked --release --features testing 2>&1 | tee -a "$plog"; build_rc=${PIPESTATUS[0]}
        if [ "$build_rc" -ne 0 ]; then echo "ERROR: Neutral artifact build failed (rc=$build_rc)" >> "$plog"
        elif [ -f "$candidate" ]; then plugin_under_test="$candidate"
        else echo "ERROR: Neutral artifact $candidate not found" >> "$plog"; fi
    fi

    art_rc=0
    art_cases=0
    if [ -n "$plugin_under_test" ] && [ -f "$plugin_under_test" ]; then
        export CLAP_PLUGIN_UNDER_TEST="$plugin_under_test"
        render_bin="${NAM_CORE_RENDER_BIN:-}"
        if [ -z "$render_bin" ]; then
            for cand in build/namcore_render/tools/render build/namcore_render/Release/render build/namcore_render/Debug/render build/namcore_render/render; do
                if [ -f "$cand" ] && [ -x "$cand" ]; then
                    render_bin="$cand"
                    break
                fi
            done
        fi
        oracle_ok=0; [ -n "$render_bin" ] && [ -f "$render_bin" ] && [ -x "$render_bin" ] && oracle_ok=1
        [ "$STRICT" = "1" ] && export NAM_RT_STRICT=1
        if [ "$oracle_ok" = 1 ]; then
            export NAM_CORE_RENDER_BIN="$render_bin"
            [ "$STRICT" = "1" ] && export NAM_REQUIRE_CPP_ORACLE=1
        else
            gap "clap_parity:missing_cpp_oracle"; art_gaps="${art_gaps:+$art_gaps,}clap_parity:missing_cpp_oracle"
        fi

        # Oracle-free tests always run; oracle-backed parity only with a render binary.
        ARTIFACT_TESTS=(
            clap_cabsim_ir::test_cabsim_ir_changes_audio_release_artifact
            clap_parity_multi_sr::test_clap_parity_cabsim
        )
        [ "$oracle_ok" = 1 ] && ARTIFACT_TESTS+=(
            clap_parity_multi_sr::test_clap_parity_multi_rate
            clap_parity_multi_sr::test_clap_parity_oversample
        )
        for atest in "${ARTIFACT_TESTS[@]}"; do
            art_cases=$((art_cases + $(subcase_count "$atest")))
            run_named "artifact" "$atest" timeout --kill-after=30 "$art_to" \
                cargo test --locked --features testing --release --test clap "$atest" \
                -- --ignored --exact --nocapture \
                || art_rc=1
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
        "$art_cases" "$exercised" "$art_gaps"
else
    skip_phase "phase3" "artifact"
fi
# ── Fase perf ────────────────────────────────────────────────────────────────
if should_run "perf"; then
    t_start=$SECONDS; phase "perf"
    plog="$LOG_DIR/long-perf.log"; : > "$plog"
    if [ "$USE_ARTIFACT" = "1" ]; then
        # Certification requires a paired reference series on the certified
        # host. Its absence is a typed gap, never a PASS.
        perf_reference="$LOG_DIR/perf-reference.json"
        if [ ! -f "$perf_reference" ] && [ -s "$LOG_DIR/perf-certification-report.json" ]; then
            # First run of a new baseline is a GAP; afterwards the previous
            # certification report is the paired series for a relative gate.
            perf_reference="$LOG_DIR/perf-certification-report.json"
            emit "PHASE: perf reference=previous_report"
        fi
        if [ ! -f "$perf_reference" ]; then
            gap "perf:reference_missing"; append_receipt "phase4" "perf" "SKIPPED" "$plog" 0 0 false "perf:reference_missing"
        else
            # timeout: pior comando ~30s (nam_perf_guard certify); 2x p100 ≈ 120s
            timeout --kill-after=30 "${NAM_PHASE_TIMEOUT_S:-120}" cargo run --locked --release --features testing \
                --bin nam_perf_guard -- certify --clap "$ARTIFACT_PATH" --reference "$perf_reference" \
                --out "$LOG_DIR/perf-certification-report.json" 2>&1 | tee -a "$plog"
            pst=("${PIPESTATUS[@]}"); perf_rc=${pst[0]}
            [ "${pst[1]}" -eq 0 ] || perf_rc=1
            # one gate: nam_perf_guard certify (artifact evidence, not a Cargo test binary)
            close_phase "phase4" "perf" "$perf_rc" "$plog" $(( (SECONDS - t_start) * 1000 )) 1 true "" 0
        fi
    else
        gap "perf:requires_artifact"
        append_receipt "phase4" "perf" "SKIPPED" "$plog" $(( (SECONDS - t_start) * 1000 )) 0 false "perf:requires_artifact"
    fi
else
    skip_phase "phase4" "perf"
fi
# ── Fase heap-audit (determinística; hospedada) ──────────────────────────────
if should_run "heap-audit"; then
    t_start=$SECONDS; phase "heap-audit"
    plog="$LOG_DIR/long-heap-audit.log"; : > "$plog"
    # timeout: pior comando ~215s (build heap-audit + soak); 2x p100 ≈ 500s
    heap_rc=0; heap_cases=0; heap_to=${NAM_PHASE_TIMEOUT_S:-500}
    run_heap_test() {
        local tname="$2" sublog pst
        [ "$1" = "--test" ] && tname="$2-$3"
        sublog="$LOG_DIR/long-heap-${tname##*::}.log"
        echo "--- Running heap/RT test: $* ---" >> "$plog"
        timeout --kill-after=30 "$heap_to" cargo test --locked --features testing,heap-audit --release "$@" < /dev/null 2>&1 | tee "$sublog" >> "$plog"
        pst=("${PIPESTATUS[@]}")
        if [ "${pst[0]}" -ne 0 ] || [ "${pst[1]}" -ne 0 ] || ! ran_tests "$sublog"; then
            heap_rc=1
        fi
    }

    run_heap_test --lib test_gc_stress_1000_swaps -- --ignored; heap_cases=$((heap_cases + 1))
    run_heap_test --lib test_gc_drain_on_destroy_no_leak -- --ignored; heap_cases=$((heap_cases + 1))
    run_heap_test --lib test_teardown_drains_rt_parking_lot_off_rt -- --ignored; heap_cases=$((heap_cases + 1))
    run_heap_test --test clap test_multi_instance_rt_priority -- --ignored --test-threads=1; heap_cases=$((heap_cases + 1))
    run_heap_test --test heap_audit prop_gc_swap_idempotence -- --ignored; heap_cases=$((heap_cases + 500))
    run_heap_test --lib test_corrupted_states_10000_heap_stable -- --ignored --test-threads=1; heap_cases=$((heap_cases + 1))

    close_phase "phase5" "heap-audit" "$heap_rc" "$plog" $(( (SECONDS - t_start) * 1000 )) "$heap_cases" false
else
    skip_phase "phase5" "heap-audit"
fi
# ── Fase rt-heap (limiares de tempo; operador-only, nunca no CI hospedado) ────
if should_run "rt-heap"; then
    t_start=$SECONDS; phase "rt-heap"
    plog="$LOG_DIR/long-rt-heap.log"; : > "$plog"
    rt_rc=0; rt_cases=0; rt_to=${NAM_PHASE_TIMEOUT_S:-500}
    run_rt_heap_test() {
        local tname="$2" sublog pst
        [ "$1" = "--test" ] && tname="$2-$3"
        sublog="$LOG_DIR/long-rt-heap-${tname##*::}.log"
        echo "--- Running RT timing test: $* ---" >> "$plog"
        timeout --kill-after=30 "$rt_to" cargo test --locked --features testing,heap-audit --release "$@" < /dev/null 2>&1 | tee "$sublog" >> "$plog"
        pst=("${PIPESTATUS[@]}")
        if [ "${pst[0]}" -ne 0 ] || [ "${pst[1]}" -ne 0 ] || ! ran_tests "$sublog"; then
            rt_rc=1
        fi
    }

    run_rt_heap_test --lib test_structural_burst_p99_within_contract -- --ignored --test-threads=1; rt_cases=$((rt_cases + 1))
    run_rt_heap_test --lib processor_drain_latency_test -- --ignored --test-threads=1; rt_cases=$((rt_cases + 2))
    run_rt_heap_test --lib test_reset_wall_clock_time_within_contract -- --ignored --test-threads=1; rt_cases=$((rt_cases + 1))

    close_phase "phase6" "rt-heap" "$rt_rc" "$plog" $(( (SECONDS - t_start) * 1000 )) "$rt_cases" false
else
    skip_phase "phase6" "rt-heap"
fi

if [ -n "$ONLY_PHASES" ]; then
    SCOPE=partial; subset_tag=${ONLY_PHASES#,}; subset_tag=${subset_tag%,}
    emit "PHASE: phase-selection PASS only-subset:${subset_tag//,/+}"
else
    # ── Terminal receipt derivation & verification (full runs only) ─────────────
    RECEIPT_READY=0
    if "$PLUG_LONG_RECEIPT_BIN" summary --out "$RECEIPT" >> "$LOG_DIR/tests-long-receipt.log" 2>&1; then
        val_cmd=("$PLUG_LONG_RECEIPT_BIN" validate --out "$RECEIPT")
        [ "$STRICT" = "1" ] && val_cmd+=(--strict)
        if "${val_cmd[@]}" >> "$LOG_DIR/tests-long-receipt.log" 2>&1; then
            RECEIPT_READY=1
        else
            FAILS+=("receipt_validation")
        fi
    else
        FAILS+=("receipt-summary"); emit "PHASE: receipt-summary FAIL"
    fi
    if [ "$RECEIPT_READY" = "1" ]; then
        if mv -f "$RECEIPT" "$RECEIPT_FINAL"; then
            emit "PHASE: receipt-publish PASS log=$RECEIPT_FINAL"
        else
            FAILS+=("receipt-publish"); emit "PHASE: receipt-publish FAIL"
        fi
    fi
fi

finish
