#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
# codegen-report.py — Símbolos quentes e métricas JSON do gate RT (uso: verify_rt_codegen.sh).
import hashlib
import json
import os
import re
import sys


def hot_symbols(profile_path, dump_path):
    with open(profile_path, "r", encoding="utf-8") as h:
        profile = json.load(h)
    patterns = [re.compile(str(t["pattern"])) for t in profile.get("targets") or []]
    if profile.get("symbols"):
        patterns.append(re.compile(str(profile["symbols"])))
    sym_re = re.compile(r"^([0-9a-fA-F]+)\s+<(.+)>:\s*$")
    seen = set()
    with open(dump_path, "r", encoding="utf-8", errors="replace") as h:
        for line in h:
            m = sym_re.match(line.rstrip("\n"))
            if m and m.group(2) not in seen and any(p.search(m.group(2)) for p in patterns):
                seen.add(m.group(2))
                print(m.group(2))
    return 0


def metrics(profile_path, summary_path, binary_path, json_out):
    with open(profile_path, "r", encoding="utf-8") as h:
        profile = json.load(h)
    targets = profile.get("targets") or []
    target_ids = [str(t.get("id") or t.get("pattern")) for t in targets]
    required_ids = {str(t.get("id") or t.get("pattern")) for t in targets if t.get("required", True)}
    counts = {}
    totals = {"symbols": 0, "instructions": 0, "vfmadd": 0, "call": 0}
    with open(summary_path, "r", encoding="utf-8", errors="replace") as h:
        for line in h:
            line = line.rstrip("\n")
            if line.startswith("symbols="):
                for field in line.split():
                    k, _, v = field.partition("=")
                    if k in totals:
                        try:
                            totals[k] = int(v)
                        except ValueError:
                            pass
                continue
            if not line.startswith("symbol |") and " | " in line:
                parts = line.split(" | ")
                if len(parts) >= 2:
                    counts[parts[1]] = counts.get(parts[1], 0) + 1
    matched = sum(1 for tid in target_ids if counts.get(tid, 0) > 0)
    req_matched = sum(1 for tid in required_ids if counts.get(tid, 0) > 0)
    with open(binary_path, "rb") as h:
        digest = hashlib.sha256(h.read()).hexdigest()
    doc = {
        "binary": binary_path,
        "sha256": digest,
        "symbols": int(totals["symbols"]),
        "instructions": int(totals["instructions"]),
        "vfmadd": int(totals["vfmadd"]),
        "call": int(totals["call"]),
        "targets_total": len(target_ids),
        "targets_matched": matched,
        "required_total": len(required_ids),
        "required_matched": req_matched,
        "targets": {tid: counts.get(tid, 0) for tid in target_ids},
    }
    if json_out:
        out_dir = os.path.dirname(json_out)
        if out_dir:
            os.makedirs(out_dir, exist_ok=True)
        with open(json_out + ".tmp", "w", encoding="utf-8") as f:
            json.dump(doc, f, sort_keys=True)
        os.replace(json_out + ".tmp", json_out)
    print(
        f"symbols={doc['symbols']} instructions={doc['instructions']} vfmadd={doc['vfmadd']} "
        f"targets_matched={doc['targets_matched']} targets_total={doc['targets_total']} "
        f"required_matched={doc['required_matched']} required_total={doc['required_total']}"
    )
    return 0


if __name__ == "__main__":
    commands = {"hot-symbols": hot_symbols, "metrics": metrics}
    raise SystemExit(commands[sys.argv[1]](*sys.argv[2:]))
