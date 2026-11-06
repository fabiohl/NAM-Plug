#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
# lints.sh — Análise estática, formatação e conformidade textual. Classe A.
# Uso: utils/lints.sh
# Orçamento: 115
set -x; PS4='+ ${BASH_SOURCE##*/}:${LINENO}: '
set -euo pipefail
cd "$(dirname "$0")/.."
. utils/_lib.sh

case "${1:-}" in "") ;; -h|--help) echo "Uso: utils/lints.sh"; exit 0 ;; *) usage_fail "lints.sh aceita apenas -h/--help" ;; esac
lib_init
trap 'trap_fail $?' ERR

phase_fmt() {
  if [ "${CI:-}" = "true" ]; then cargo fmt --all -- --check; else cargo fmt --all && git diff --stat; fi
}
run_step fmt phase_fmt

phase_check() {
  cargo check --locked --all-targets --all-features
  cargo check --locked --lib --no-default-features
  cargo check --locked --all-targets --no-default-features
}
run_step check phase_check

phase_clippy() {
  cargo clippy --locked --all-targets --all-features -- -D warnings
  cargo clippy --locked --lib --no-default-features -- -D warnings
  cargo clippy --locked --all-targets --no-default-features -- -D warnings
}
run_step clippy phase_clippy
# Cross-checks target-specific cfg branches (N-26). Native aarch64 already ran clippy in phase_clippy;
# x86_64 hosts only emit .rmeta with PKG_CONFIG_ALLOW_CROSS=1 for pkg-config metadata discovery.
PHASE_NAME=cross_aarch64_check
if [ "$(rustc -vV | sed -n 's/^host: //p')" = aarch64-unknown-linux-gnu ]; then run_step cross_aarch64_check true
elif ! rustup target list --installed | grep -qx aarch64-unknown-linux-gnu; then gap "cross_aarch64_check:target-not-installed"
else run_step cross_aarch64_check env PKG_CONFIG_ALLOW_CROSS=1 cargo clippy --locked --lib --target aarch64-unknown-linux-gnu -- -D warnings; fi

phase_doc() {
  RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --all-features
}
run_step doc phase_doc

phase_spdx() {
  local listing files missing invalid
  listing=$(git ls-files 'src/*.rs' 'tests/*.rs' 'benches/*.rs' 'utils/*.sh' 'build.rs' 'Cargo.toml') \
    || { echo "SPDX scan failed: git ls-files failed" >&2; return 1; }
  [ -n "$listing" ] || { echo "SPDX scan failed: git ls-files returned no files" >&2; return 1; }
  mapfile -t files <<< "$listing"
  missing=$(grep -L "SPDX-License-Identifier" "${files[@]}" || true)
  if [ -n "$missing" ]; then
    echo "Missing SPDX header:" >&2; echo "$missing" | sed 's/^/  /' >&2; exit 1
  fi
  invalid=$(grep -l "SPDX-License-Identifier" "${files[@]}" | xargs grep -L -E "SPDX-License-Identifier: (GPL-3\.0-or-later|MIT)" || true)
  if [ -n "$invalid" ]; then
    echo "Invalid SPDX identifier:" >&2; echo "$invalid" | sed 's/^/  /' >&2; exit 1
  fi
}
run_step spdx phase_spdx

phase_policies() {
  if [ -d "tests/common" ] && grep -rnF "#[test]" tests/common/ > /dev/null 2>&1; then
    echo "ERROR: #[test] in tests/common/" >&2; exit 1
  fi

  lib_quiet=1 utils/verify_no_rt_alloc.sh

  local clippy_errs
  clippy_errs=$(git ls-files 'src/*.rs' 'tests/*.rs' 'benches/*.rs' | xargs awk '
    FNR == 1 { prev = 0 }
    /^[[:space:]]*#\[allow\(clippy::/ {
      if (!prev) print FILENAME ":" FNR ": " $0
      prev = 0; next
    }
    /^[[:space:]]*(\/\/[[:space:]]*(SPDX|Copyright)|\/\/\/|\/\/!)/ { prev = 0; next }
    /^[[:space:]]*(\/\/[^/!]|[\/]\*)/ { prev = 1; next }
    /^[[:space:]]*$/ { next }
    { prev = 0 }
  ')
  if [ -n "$clippy_errs" ]; then
    echo "Uncommented clippy allow attributes found:" >&2
    echo "$clippy_errs" >&2
    exit 1
  fi

  local cargo_ver metainfo_file xml_ver
  cargo_ver=$(grep -m1 '^version = ' Cargo.toml | cut -d '"' -f2)
  metainfo_file="packaging/flatpak/org.freedesktop.LinuxAudio.Plugins.NAMPlug.metainfo.xml"
  if [ -f "$metainfo_file" ]; then
    xml_ver=$(grep -m1 '<release version=' "$metainfo_file" | sed -E 's/.*version="([^"]+)".*/\1/')
    if [ "$cargo_ver" != "$xml_ver" ]; then
      echo "ERROR: Version mismatch between Cargo.toml ($cargo_ver) and $metainfo_file ($xml_ver)!" >&2
      exit 1
    fi
  fi
}
run_step policies phase_policies

# AppStream metadata was validated manually before; make it a real gate.
phase_packaging() {
  local m="packaging/flatpak/org.freedesktop.LinuxAudio.Plugins.NAMPlug.metainfo.xml"
  [ -f "$m" ] || { echo "Missing mandatory packaging path: $m" >&2; return 1; }
  command -v appstreamcli >/dev/null 2>&1 || { echo "appstreamcli is required (appstream package)" >&2; return 1; }
  appstreamcli validate --no-net --strict "$m"
}
run_step packaging phase_packaging

phase_script_budget() { local f b; for f in $(find utils -name '*.sh'); do b=$(sed -n 's/^# Orçamento: *\([0-9]*\).*/\1/p;T;q' "$f"); [ -n "$b" ] && [ "$(wc -l < "$f")" -le "$b" ] || { echo "ORÇAMENTO: $f teto=${b:-AUSENTE} linhas=$(wc -l < "$f")" >&2; return 1; }; done; }
run_step script-budget phase_script_budget

finish
