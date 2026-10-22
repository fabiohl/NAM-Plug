#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Deterministic regression test for baseline integrity (sidecar, field compatibility, infra sanity, unstable escalation).
#   (a) --bootstrap-baseline writes baseline-artifact.json with every field non-null
#   (b) field-level compatibility: identical -> compatible; tampered binary/host/missing -> BASELINE_INCOMPATIBLE
#   (c) infrastructure series outside [0.5, 2.0] withhold the verdict (BASELINE_INFRA_MISMATCH)
#   (d) BASELINE_UNSTABLE is fatal only when full audit / strict perf is requested
#
# Runs against an isolated copy of the wrapper project so the real
# .performance-baselines/ directory is never read or written.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"

BOOTSTRAP_SCRIPT="${NAM_BOOTSTRAP_SCRIPT:-$(command -v bootstrap-repo.sh || true)}"
if [ -z "$BOOTSTRAP_SCRIPT" ] || [ ! -f "$BOOTSTRAP_SCRIPT" ]; then
    echo "NOTICE: bootstrap-repo.sh not found (set NAM_BOOTSTRAP_SCRIPT to run); skipping."
    exit 0
fi

RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m'

TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

fail() {
    echo -e "${RED}FAIL: $*${NC}" >&2
    exit 1
}

echo -e "${BLUE}${BOLD}===============================================================${NC}"
echo -e "${BLUE}${BOLD}    test_perf_baseline_integrity: sidecar, compat, infra${NC}"
echo -e "${BLUE}${BOLD}===============================================================${NC}"

unset NAM_BOOTSTRAP_FULL_AUDIT NAM_BOOTSTRAP_STRICT_PERF NAM_PERF_STABILITY_STRICT CLAP_PLUGIN_UNDER_TEST

PROJ="$TMP_DIR/proj"
mkdir -p "$PROJ/benches"
cp -a "$PROJECT_DIR/utils" "$PROJ/utils"
cp "$PROJECT_DIR/Cargo.toml" "$PROJECT_DIR/Cargo.lock" "$PROJ/"
cp "$PROJECT_DIR/benches/clap_bench.manifest" "$PROJ/benches/"
(
    cd "$PROJ"
    git init -q
    git add -A
    git -c user.email=test@example.invalid -c user.name=test commit -qm "isolated perf integrity fixture"
)

STUB_BENCH="$TMP_DIR/stub_clap_bench"
printf '#!/bin/bash\nexit 0\n' > "$STUB_BENCH"
chmod +x "$STUB_BENCH"

STUB_PLUGIN="$TMP_DIR/stub_plugin.clap"
printf 'plugin-under-test-fixture\n' > "$STUB_PLUGIN"

MOCK_GOVERNOR="$TMP_DIR/scaling_governor"
echo "performance" > "$MOCK_GOVERNOR"

MOCK_GATE_OUT="$TMP_DIR/gate_out.txt"
echo "TEST_RESULT[perf_regression]=PASS" > "$MOCK_GATE_OUT"

CRIT_ROOT="$PROJ/target/criterion"
SIDECAR="$PROJ/.performance-baselines/baseline-artifact.json"

# Writes one Criterion change estimate: name, mean point_estimate, median point_estimate.
write_change() {
    local name="$1" mean="$2" median="$3"
    local dir="$CRIT_ROOT/$name/change"
    mkdir -p "$dir"
    cat <<JSON > "$dir/estimates.json"
{"mean":{"confidence_interval":{"confidence_level":0.95,"lower_bound":$mean,"upper_bound":$mean},"point_estimate":$mean,"standard_error":0.001},"median":{"confidence_interval":{"confidence_level":0.95,"lower_bound":$median,"upper_bound":$median},"point_estimate":$median,"standard_error":0.001}}
JSON
}

# run_wrapper <args...>: runs the wrapper inside the isolated project with mocks.
run_wrapper() {
    (
        cd "$PROJ"
        export NAM_GOVERNOR_FILE="$MOCK_GOVERNOR"
        export NAM_BENCH_EXE="$STUB_BENCH"
        export NAM_BENCH_CORE=0
        export CARGO_TARGET_DIR="$PROJ/target"
        export NAM_MOCK_GATE_OUTPUT="$MOCK_GATE_OUT"
        export NAM_MOCK_GATE_RC=0
        export NAM_PERF_CONFIRM=0
        "$PROJ/utils/tests-performance-regression.sh" "$@"
    )
}

# ── [1/4] Sidecar written by --bootstrap-baseline ────────────────────────────
echo -e "\n${BLUE}[1/4] Testing --bootstrap-baseline writes a complete sidecar...${NC}"
rm -rf "$PROJ/.performance-baselines"
export CLAP_PLUGIN_UNDER_TEST="$STUB_PLUGIN"
OUT_1="$TMP_DIR/case_1.log"
rc=0
run_wrapper --bootstrap-baseline > "$OUT_1" 2>&1 || rc=$?
unset CLAP_PLUGIN_UNDER_TEST
[ "$rc" -eq 0 ] || { cat "$OUT_1" >&2; fail "case 1 bootstrap exited $rc"; }
[ -f "$SIDECAR" ] || fail "case 1 sidecar not written at $SIDECAR"

python3 - "$SIDECAR" "$STUB_BENCH" "$STUB_PLUGIN" "$PROJ/Cargo.lock" "$PROJ" <<'PY' || fail "case 1 sidecar validation failed"
import hashlib, json, subprocess, sys

sidecar, bench, plugin, lock, proj = sys.argv[1:6]


def sha(path):
    with open(path, "rb") as fh:
        return hashlib.sha256(fh.read()).hexdigest()


with open(sidecar) as fh:
    doc = json.load(fh)

top = ["bench_binary_sha256", "cargo_target_dir", "bench_profile", "plugin_under_test_sha256",
       "cargo_lock_sha256", "git_commit", "git_dirty", "host", "created_utc"]
host = ["bench_core", "governor", "boost", "cpu_online", "irq_isolation_ok", "sched_policy"]
for key in top:
    if key not in doc or doc[key] is None:
        sys.exit(f"missing or null top-level field: {key}")
for key in host:
    if key not in doc["host"] or doc["host"][key] is None:
        sys.exit(f"missing or null host field: {key}")
if doc["bench_binary_sha256"] != sha(bench):
    sys.exit("bench_binary_sha256 does not match the executable")
if doc["plugin_under_test_sha256"] != sha(plugin):
    sys.exit("plugin_under_test_sha256 does not match CLAP_PLUGIN_UNDER_TEST")
if doc["cargo_lock_sha256"] != sha(lock):
    sys.exit("cargo_lock_sha256 does not match Cargo.lock")
head = subprocess.run(["git", "-C", proj, "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
if doc["git_commit"] != head:
    sys.exit("git_commit does not match HEAD")
if doc["git_dirty"] is not False:
    sys.exit("git_dirty should be false in the clean fixture")
if doc["host"]["bench_core"] != "0":
    sys.exit("host.bench_core should reflect NAM_BENCH_CORE")
if doc["host"]["governor"] != "performance":
    sys.exit("host.governor should reflect the governor file")
if doc["host"]["sched_policy"] == "":
    sys.exit("host.sched_policy empty")
PY
echo -e "  ${GREEN}✓ Sidecar complete: all fields present, non-null, digests bound to the real artifacts.${NC}"

# ── [2/4] Field-level compatibility ──────────────────────────────────────
echo -e "\n${BLUE}[2/4] Testing field-level compatibility of previous vs current baseline...${NC}"
eval "$(sed -n '/^perf_fingerprint_compatible() {/,/^}/p' "$BOOTSTRAP_SCRIPT")"

FP_JSON='{"cpu_model":"x","cpu_microarchitecture":"x86-64-v3","physical_cores":8,"rustc_version":"rustc 1","target_triple":"x86_64-unknown-linux-gnu","rustflags":"","build_profile":"release","frequency_governor":"performance","bench_core":"0"}'

make_pair() {
    # make_pair <name>: previous and current dirs, each with fingerprint + sidecar copied from case 1
    local dir="$TMP_DIR/pair-$1"
    mkdir -p "$dir/previous" "$dir/current"
    printf '%s\n' "$FP_JSON" > "$dir/previous/baseline-fingerprint.json"
    printf '%s\n' "$FP_JSON" > "$dir/current/baseline-fingerprint.json"
    cp "$SIDECAR" "$dir/previous/baseline-artifact.json"
    cp "$SIDECAR" "$dir/current/baseline-artifact.json"
    echo "$dir"
}

compare() {
    # compare <dir>: sets CMP_RC and CMP_OUT
    CMP_RC=0
    CMP_OUT="$(perf_fingerprint_compatible "$1/previous/baseline-fingerprint.json" "$1/current/baseline-fingerprint.json" 2>/dev/null)" || CMP_RC=$?
}

PAIR="$(make_pair identical)"
compare "$PAIR"
[ "$CMP_RC" -eq 0 ] || fail "case 2 identical baselines reported incompatible: $CMP_OUT"

PAIR="$(make_pair tampered_binary)"
python3 - "$PAIR/previous/baseline-artifact.json" <<'PY'
import json, sys
p = sys.argv[1]
with open(p) as fh:
    d = json.load(fh)
d["bench_binary_sha256"] = "0" * 64
with open(p, "w") as fh:
    json.dump(d, fh)
PY
compare "$PAIR"
[ "$CMP_RC" -eq 1 ] || fail "case 2 tampered bench_binary_sha256 expected rc 1, got $CMP_RC"
[ "$CMP_OUT" = "BASELINE_INCOMPATIBLE: bench_binary_sha256" ] || fail "case 2 unexpected reason: '$CMP_OUT'"

PAIR="$(make_pair host_boost)"
python3 - "$PAIR/current/baseline-artifact.json" <<'PY'
import json, sys
p = sys.argv[1]
with open(p) as fh:
    d = json.load(fh)
d["host"]["boost"] = "1" if d["host"]["boost"] != "1" else "0"
with open(p, "w") as fh:
    json.dump(d, fh)
PY
compare "$PAIR"
[ "$CMP_RC" -eq 1 ] && [ "$CMP_OUT" = "BASELINE_INCOMPATIBLE: host.boost" ] || fail "case 2 host.boost tamper not reported: rc=$CMP_RC out='$CMP_OUT'"

PAIR="$(make_pair no_sidecar)"
rm -f "$PAIR/previous/baseline-artifact.json"
compare "$PAIR"
[ "$CMP_RC" -eq 3 ] || fail "case 2 missing sidecar expected rc 3, got $CMP_RC"
case "$CMP_OUT" in
    "BASELINE_INCOMPATIBLE: missing_fields:"*) ;;
    *) fail "case 2 missing sidecar not reported as missing_fields: '$CMP_OUT'" ;;
esac
echo -e "  ${GREEN}✓ Identical -> compatible; tampered binary -> BASELINE_INCOMPATIBLE: bench_binary_sha256; host drift and missing sidecar -> incompatible.${NC}"

# ── [3/4] Infrastructure sanity ─────────────────────────────────────────
echo -e "\n${BLUE}[3/4] Testing infrastructure series outside [0.5, 2.0] withhold the verdict...${NC}"
rm -rf "$CRIT_ROOT"
# Measured: Bypass/Block_64samp fresh/prev=0.124 (ratio = 1 + mean change).
write_change "CLAP_Infrastructure_Bypass/Block_64samp" -0.876 -0.876
write_change "CLAP_WorstCase_SmallBlocks_Passthrough/64_samp" -0.827 -0.827
write_change "CLAP_Inference_WaveNet_A1_CabSim/Standard" 0.02 0.02
OUT_3="$TMP_DIR/case_3.log"
rc=0
run_wrapper --check > "$OUT_3" 2>&1 || rc=$?
[ "$rc" -eq 1 ] || { cat "$OUT_3" >&2; fail "case 3 infra mismatch expected rc 1, got $rc"; }
grep -qF "TEST_RESULT[perf_regression]=FAIL:baseline_infra_mismatch" "$OUT_3" \
    || { cat "$OUT_3" >&2; fail "case 3 missing BASELINE_INFRA_MISMATCH verdict"; }
grep -qF "[INFRA] CLAP_Infrastructure_Bypass/Block_64samp" "$OUT_3" \
    || { cat "$OUT_3" >&2; fail "case 3 offending series not named"; }
if grep -qF "[INFRA] CLAP_Inference_WaveNet" "$OUT_3"; then
    fail "case 3 inference series wrongly treated as infrastructure"
fi

rm -rf "$CRIT_ROOT"
write_change "CLAP_Infrastructure_Bypass/Block_64samp" 0.03 0.03
write_change "CLAP_WorstCase_SmallBlocks_Passthrough/64_samp" -0.02 -0.02
rc=0
run_wrapper --check > "$TMP_DIR/case_3b.log" 2>&1 || rc=$?
[ "$rc" -eq 0 ] || { cat "$TMP_DIR/case_3b.log" >&2; fail "case 3 in-band infra control expected rc 0, got $rc"; }
echo -e "  ${GREEN}✓ Bypass ratio 0.124 -> BASELINE_INFRA_MISMATCH (verdict withheld); in-band infra series pass through.${NC}"

# ── [4/4] Unstable baseline escalation ───────────────────────────────────
echo -e "\n${BLUE}[4/4] Testing BASELINE_UNSTABLE is fatal only under full audit / strict perf...${NC}"
rm -rf "$CRIT_ROOT"
write_change "CLAP_Inference_WaveNet_A2_Slimmable_BlockSize/32_samp" -0.14 -0.14
rc=0
run_wrapper --check > "$TMP_DIR/case_4a.log" 2>&1 || rc=$?
[ "$rc" -eq 0 ] || fail "case 4 unstable baseline without audit env expected rc 0 (WARN GAP), got $rc"

for env_name in NAM_BOOTSTRAP_FULL_AUDIT NAM_BOOTSTRAP_STRICT_PERF; do
    rc=0
    export "$env_name=1"
    run_wrapper --check > "$TMP_DIR/case_4b.log" 2>&1 || rc=$?
    unset "$env_name"
    [ "$rc" -eq 1 ] || { cat "$TMP_DIR/case_4b.log" >&2; fail "case 4 unstable baseline with $env_name=1 expected rc 1, got $rc"; }
    grep -qF "TEST_RESULT[perf_regression]=FAIL:baseline_unstable" "$TMP_DIR/case_4b.log" \
        || fail "case 4 missing FAIL:baseline_unstable marker with $env_name=1"
done
echo -e "  ${GREEN}✓ Unstable series: WARN GAP by default, fatal with NAM_BOOTSTRAP_FULL_AUDIT=1 or NAM_BOOTSTRAP_STRICT_PERF=1.${NC}"

echo -e "\n${GREEN}${BOLD}All 4 test_perf_baseline_integrity scenarios passed!${NC}"
