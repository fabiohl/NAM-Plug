#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Deterministic unit test for bootstrap-repo.sh environment restore auto-verification (T2.5, D4, C1).
# Validates:
#   1. Clean restore reports '[OK] ambiente restaurado'
#   2. Managed IRQs (60-67, nvme0q*) are excluded from comparison and never cause false divergence
#   3. Unmanaged IRQ divergence is detected and reported
#   4. CPU9, boost, governor, and paranoid divergences are detected and reported

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"

# ANSI style helpers
RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m'

# Locate bootstrap-repo.sh
BOOTSTRAP_SCRIPT=""
for candidate in "$PROJECT_DIR/../.agents/scripts/bootstrap-repo.sh" "$HOME/NAM/.agents/scripts/bootstrap-repo.sh"; do
    if [ -f "$candidate" ]; then
        BOOTSTRAP_SCRIPT="$candidate"
        break
    fi
done

if [ -z "$BOOTSTRAP_SCRIPT" ]; then
    echo "ERROR: bootstrap-repo.sh not found!" >&2
    exit 1
fi

TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

echo -e "${BLUE}${BOLD}===============================================================${NC}"
echo -e "${BLUE}${BOLD}    test_restore_verification: restore auto-verification (T2.5) ${NC}"
echo -e "${BLUE}${BOLD}===============================================================${NC}"

# Setup fake sysfs and procfs roots
FAKE_SYSFS="$TMP_DIR/sys"
FAKE_PROCFS="$TMP_DIR/proc"

setup_fake_roots() {
    rm -rf "$FAKE_SYSFS" "$FAKE_PROCFS"
    mkdir -p "$FAKE_SYSFS/devices/system/cpu/cpu9"
    mkdir -p "$FAKE_SYSFS/devices/system/cpu/cpufreq"
    mkdir -p "$FAKE_SYSFS/devices/system/cpu/cpu0/cpufreq"
    mkdir -p "$FAKE_SYSFS/devices/system/cpu/cpu8/cpufreq"
    mkdir -p "$FAKE_PROCFS/sys/kernel"
    mkdir -p "$FAKE_PROCFS/irq"
    mkdir -p "$FAKE_PROCFS/irq/10"
    mkdir -p "$FAKE_PROCFS/irq/60/nvme0q1"
    mkdir -p "$FAKE_PROCFS/irq/62/nvme0q3"

    echo 1 > "$FAKE_SYSFS/devices/system/cpu/cpu9/online"
    echo 1 > "$FAKE_SYSFS/devices/system/cpu/cpufreq/boost"
    echo "powersave" > "$FAKE_SYSFS/devices/system/cpu/cpu0/cpufreq/scaling_governor"
    echo "powersave" > "$FAKE_SYSFS/devices/system/cpu/cpu8/cpufreq/scaling_governor"
    echo 2 > "$FAKE_PROCFS/sys/kernel/perf_event_paranoid"
    echo "ffff" > "$FAKE_PROCFS/irq/default_smp_affinity"
    echo "0-15" > "$FAKE_PROCFS/irq/10/smp_affinity_list"
    echo "0" > "$FAKE_PROCFS/irq/60/smp_affinity_list"
    echo "8-9" > "$FAKE_PROCFS/irq/62/smp_affinity_list"
}

# ── Test 1: Clean restore matches snapshot -> [OK] ambiente restaurado ───────
echo -e "\n${BLUE}[1/4] Testing clean restore matches snapshot...${NC}"
setup_fake_roots

OUT_1=$(
    bash -c '
        set -euo pipefail
        export NAM_SYSFS_ROOT="'"$FAKE_SYSFS"'"
        export NAM_PROCFS_ROOT="'"$FAKE_PROCFS"'"
        export NAM_BENCH_CORE=8
        export NAM_RESTORE_CPU=1

        # Source snapshot and restore function from bootstrap-repo.sh
        eval "$(sed -n "/^NAM_SYSFS_ROOT=/,/^trap nam_restore_cpu_state EXIT/ { /^trap /d; p }" "'"$BOOTSTRAP_SCRIPT"'")"

        # Mock privileged write to write to fake filesystem directly without sudo
        nam_privileged_write() {
            echo "$1" > "$2"
        }

        # Simulate bootstrap modifications
        echo 0 > "$NAM_SYSFS_ROOT/devices/system/cpu/cpu9/online"
        echo 0 > "$NAM_SYSFS_ROOT/devices/system/cpu/cpufreq/boost"
        echo "performance" > "$NAM_SYSFS_ROOT/devices/system/cpu/cpu8/cpufreq/scaling_governor"
        echo 1 > "$NAM_PROCFS_ROOT/sys/kernel/perf_event_paranoid"
        echo "fcff" > "$NAM_PROCFS_ROOT/irq/default_smp_affinity"
        echo "0-7,10-15" > "$NAM_PROCFS_ROOT/irq/10/smp_affinity_list"

        # Execute restore
        nam_restore_cpu_state
    ' 2>&1
)

if ! echo "$OUT_1" | grep -qF "[OK] ambiente restaurado"; then
    echo -e "${RED}[FAIL] Expected '[OK] ambiente restaurado', got:\n$OUT_1${NC}" >&2
    exit 1
fi
echo -e "${GREEN}[OK] Test 1 passed successfully.${NC}"

# ── Test 2: Managed IRQ 60-67 (nvme0q*) divergence is ignored ─────────────────
echo -e "\n${BLUE}[2/4] Testing managed IRQ (60-67 / nvme0q*) divergence is ignored...${NC}"
setup_fake_roots

OUT_2=$(
    bash -c '
        set -euo pipefail
        export NAM_SYSFS_ROOT="'"$FAKE_SYSFS"'"
        export NAM_PROCFS_ROOT="'"$FAKE_PROCFS"'"
        export NAM_BENCH_CORE=8
        export NAM_RESTORE_CPU=1

        eval "$(sed -n "/^NAM_SYSFS_ROOT=/,/^trap nam_restore_cpu_state EXIT/ { /^trap /d; p }" "'"$BOOTSTRAP_SCRIPT"'")"

        nam_privileged_write() {
            echo "$1" > "$2"
        }

        # Mutate managed IRQs 60 and 62 to something different from snapshot
        echo "0-7,10-15" > "$NAM_PROCFS_ROOT/irq/60/smp_affinity_list"
        echo "0-7,10-15" > "$NAM_PROCFS_ROOT/irq/62/smp_affinity_list"

        # Restore CPU state: managed IRQs should be excluded from both writes and check
        nam_restore_cpu_state
    ' 2>&1
)

if ! echo "$OUT_2" | grep -qF "[OK] ambiente restaurado"; then
    echo -e "${RED}[FAIL] Expected '[OK] ambiente restaurado' despite managed IRQ mutations, got:\n$OUT_2${NC}" >&2
    exit 1
fi
echo -e "${GREEN}[OK] Test 2 passed successfully.${NC}"

# ── Test 3: Unmanaged IRQ divergence is reported ──────────────────────────────
echo -e "\n${BLUE}[3/4] Testing unmanaged IRQ divergence is reported...${NC}"
setup_fake_roots

OUT_3=$(
    bash -c '
        set -euo pipefail
        export NAM_SYSFS_ROOT="'"$FAKE_SYSFS"'"
        export NAM_PROCFS_ROOT="'"$FAKE_PROCFS"'"
        export NAM_BENCH_CORE=8
        export NAM_RESTORE_CPU=1

        eval "$(sed -n "/^NAM_SYSFS_ROOT=/,/^trap nam_restore_cpu_state EXIT/ { /^trap /d; p }" "'"$BOOTSTRAP_SCRIPT"'")"

        # Mock write that fails to restore IRQ 10
        nam_privileged_write() {
            if [[ "$2" == *"irq/10"* ]]; then
                return 0 # simulate unwritten
            fi
            echo "$1" > "$2"
        }

        echo "0-7" > "$NAM_PROCFS_ROOT/irq/10/smp_affinity_list"

        nam_restore_cpu_state
    ' 2>&1
)

if ! echo "$OUT_3" | grep -qF "[AVISO] divergências: irq/10/smp_affinity_list (esperado: 0-15, atual: 0-7)"; then
    echo -e "${RED}[FAIL] Expected divergence report for irq/10, got:\n$OUT_3${NC}" >&2
    exit 1
fi
echo -e "${GREEN}[OK] Test 3 passed successfully.${NC}"

# ── Test 4: CPU9, boost, governor, and paranoid divergences reported ─────────
echo -e "\n${BLUE}[4/4] Testing CPU9, boost, governor, and paranoid divergences reported...${NC}"
setup_fake_roots

OUT_4=$(
    bash -c '
        set -euo pipefail
        export NAM_SYSFS_ROOT="'"$FAKE_SYSFS"'"
        export NAM_PROCFS_ROOT="'"$FAKE_PROCFS"'"
        export NAM_BENCH_CORE=8
        export NAM_RESTORE_CPU=1

        eval "$(sed -n "/^NAM_SYSFS_ROOT=/,/^trap nam_restore_cpu_state EXIT/ { /^trap /d; p }" "'"$BOOTSTRAP_SCRIPT"'")"

        # No-op write so all mutations persist
        nam_privileged_write() {
            return 0
        }

        echo 0 > "$NAM_SYSFS_ROOT/devices/system/cpu/cpu9/online"
        echo 0 > "$NAM_SYSFS_ROOT/devices/system/cpu/cpufreq/boost"
        echo "performance" > "$NAM_SYSFS_ROOT/devices/system/cpu/cpu8/cpufreq/scaling_governor"
        echo 1 > "$NAM_PROCFS_ROOT/sys/kernel/perf_event_paranoid"

        nam_restore_cpu_state
    ' 2>&1
)

if ! echo "$OUT_4" | grep -qF "cpu9/online (esperado: 1, atual: 0)" || \
   ! echo "$OUT_4" | grep -qF "boost (esperado: 1, atual: 0)" || \
   ! echo "$OUT_4" | grep -qF "cpu8/governor (esperado: powersave, atual: performance)" || \
   ! echo "$OUT_4" | grep -qF "perf_event_paranoid (esperado: 2, atual: 1)"; then
    echo -e "${RED}[FAIL] Expected full divergence report, got:\n$OUT_4${NC}" >&2
    exit 1
fi
echo -e "${GREEN}[OK] Test 4 passed successfully.${NC}"

echo -e "\n${GREEN}${BOLD}All test_restore_verification cases passed successfully!${NC}"
exit 0
