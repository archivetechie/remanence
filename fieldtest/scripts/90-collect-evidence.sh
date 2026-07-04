#!/usr/bin/env bash
set -euo pipefail

SCRIPT_NAME="90-collect-evidence"
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

usage() {
  cat <<'EOF'
Usage: 90-collect-evidence.sh [--help]

Bundles evidence/ into a tarball and generates SUMMARY.md from records.jsonl and
bench.csv.
EOF
}

main() {
  if [[ "${1:-}" == --help || "${1:-}" == -h ]]; then
    usage
    exit 0
  fi

  fieldtest_init_layout
  local evidence_dir summary pack_path
  evidence_dir="$(fieldtest_evidence_dir)"
  summary="$evidence_dir/SUMMARY.md"
  pack_path="$(fieldtest_home)/evidence-pack-$(date -u +%Y%m%d).tar.gz"
  python3 - "$evidence_dir" "$summary" <<'PY'
import csv
import json
import sys
from collections import defaultdict
from pathlib import Path

evidence = Path(sys.argv[1])
summary = Path(sys.argv[2])
records = evidence / "records.jsonl"
bench = evidence / "bench.csv"
lines = ["# Evidence Summary", ""]
if records.exists():
    by_script = defaultdict(list)
    for raw in records.read_text().splitlines():
        if not raw.strip():
            continue
        row = json.loads(raw)
        by_script[row["script"]].append(row)
    lines.append("## Test Matrix")
    lines.append("| Script | Test ID | Status | Summary | Detail |")
    lines.append("|---|---|---|---|---|")
    for script in sorted(by_script):
        for row in by_script[script]:
            lines.append(f"| {row['script']} | {row['test_id']} | {row['status']} | {row['summary']} | {row.get('detail_path') or ''} |")
    lines.append("")
if bench.exists():
    lines.append("## Benchmarks")
    lines.append("| Metric | Drive | Block Size | Payload | MB/s | Seconds | Bytes |")
    lines.append("|---|---|---|---|---|---|---|")
    with bench.open(newline="") as fh:
        reader = csv.DictReader(fh)
        for row in reader:
            lines.append(f"| {row['metric']} | {row['drive']} | {row['block_size']} | {row['payload']} | {row['MB_s']} | {row['seconds']} | {row['bytes']} |")
summary.write_text("\n".join(lines) + "\n")
PY
  python3 - "$evidence_dir" "$pack_path" <<'PY'
import os
import tarfile
import sys
from pathlib import Path

evidence = Path(sys.argv[1])
pack = Path(sys.argv[2])
with tarfile.open(pack, "w:gz") as tf:
    tf.add(evidence, arcname="evidence")
    config = evidence.parent / "config.toml"
    if config.exists():
        tf.add(config, arcname="config.toml")
    log = evidence.parent / "log" / "rem-daemon.log"
    if log.exists():
        tf.add(log, arcname="rem-daemon.log")
PY
  fieldtest_evidence_record "$SCRIPT_NAME" collect PASS "wrote $(basename "$pack_path") and SUMMARY.md" "$summary"
  printf '%s\n' "$pack_path"
}

fieldtest_run_with_lock main "$@"
