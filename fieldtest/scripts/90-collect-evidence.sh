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
  local evidence_dir summary pack_path py daemon_running=0
  evidence_dir="$(fieldtest_evidence_dir)"
  py="$(fieldtest_python)" || return 2
  # Make one decision for both the summary and the archive, before writing either.
  if fieldtest_daemon_running >/dev/null; then daemon_running=1; fi
  summary="$evidence_dir/SUMMARY.md"
  pack_path="${FIELD_EVIDENCE_PACK:-$(fieldtest_home)/evidence-pack-$(date -u +%Y%m%d).tar.gz}"
  "$py" - "$evidence_dir" "$summary" "$daemon_running" <<'PY'
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
if sys.argv[3] == "1":
    lines += ["The daemon is running; library and tape logs are taken at teardown, with the authority manifest and reset receipt.", ""]
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
    lines.append("| Metric | Drive | Block Size | Payload | MB/s | Seconds | Bytes | Object | Daemon append MiB/s | Staging | Diagnostics |")
    lines.append("|---|---|---|---|---|---|---|---|---|---|---|")
    with bench.open(newline="") as fh:
        reader = csv.DictReader(fh)
        for row in reader:
            lines.append(f"| {row['metric']} | {row['drive']} | {row['block_size']} | {row['payload']} | {row['MB_s']} | {row['seconds']} | {row['bytes']} | {row.get('object_id', '')} | {row.get('daemon_drive_append_mib_s', '')} | {row.get('staging_mode', '')} | {row.get('daemon_diagnostics', '')} |")
summary.write_text("\n".join(lines) + "\n")
PY
  "$py" - "$evidence_dir" "$pack_path" "$(fieldtest_state_dir)" "$daemon_running" <<'PY'
import os
import tarfile
import sys
from pathlib import Path

evidence = Path(sys.argv[1])
pack = Path(sys.argv[2])
state = Path(sys.argv[3])
with tarfile.open(pack, "w:gz") as tf:
    tf.add(evidence, arcname="evidence")
    config = evidence.parent / "config.toml"
    if config.exists():
        tf.add(config, arcname="config.toml")
    log = evidence.parent / "log" / "rem-daemon.log"
    if log.exists():
        tf.add(log, arcname="rem-daemon.log")
    if sys.argv[4] == "0":
        for pattern in ("audit/*.remlib", "journals/tapes/*.remtape",
                        "authority-manifest.json", "authority-reset-receipt.json"):
            for path in sorted(state.glob(pattern)):
                if path.is_file():
                    tf.add(path, arcname=str(Path("state") / path.relative_to(state)))
PY
  fieldtest_evidence_record "$SCRIPT_NAME" collect PASS "wrote $(basename "$pack_path") and SUMMARY.md" "$summary"
  printf '%s\n' "$pack_path"
}

fieldtest_run_with_lock main "$@"
