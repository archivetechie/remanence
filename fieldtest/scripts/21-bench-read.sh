#!/usr/bin/env bash
set -euo pipefail

SCRIPT_NAME="21-bench-read"
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

usage() {
  cat <<'EOF'
Usage: 21-bench-read.sh [--help]

Measures sustained read throughput plus range-read timing at early/mid/late
offsets within a tape-written object.
EOF
}

sha256_file() {
  python3 - "$1" <<'PY'
import hashlib
import sys
from pathlib import Path

digest = hashlib.sha256()
with Path(sys.argv[1]).open("rb") as handle:
    while True:
        chunk = handle.read(1024 * 1024)
        if not chunk:
            break
        digest.update(chunk)
print(digest.hexdigest())
PY
}

make_payload() {
  local path="$1" bytes="$2"
  python3 - "$path" "$bytes" <<'PY'
import os
import sys
from pathlib import Path

path = Path(sys.argv[1])
bytes_needed = int(sys.argv[2])
chunk = 8 * 1024 * 1024
path.parent.mkdir(parents=True, exist_ok=True)
with path.open("wb") as handle:
    remaining = bytes_needed
    while remaining > 0:
        n = min(chunk, remaining)
        handle.write(os.urandom(n))
        remaining -= n
PY
}

default_bench_bytes() {
  if [[ "${REMFIELD_ENV:-unknown}" == vtl ]]; then
    printf '%s\n' "${FIELD_BENCH_READ_BYTES_VTL:-1073741824}"
  else
    printf '%s\n' "${FIELD_BENCH_READ_BYTES:-4294967296}"
  fi
}

main() {
  if [[ "${1:-}" == --help || "${1:-}" == -h ]]; then
    usage
    exit 0
  fi

  fieldtest_init_layout
  fieldtest_detect_env || true
  local serial
  serial="$(fieldtest_selected_library_serial)"
  if [[ -z "$serial" ]]; then
    echo "error: no selected library; run bringup first" >&2
    exit 1
  fi
  fieldtest_require_pool_appendable_tapes fieldtest-a 1 "read benchmark fixture write"

  local size_bytes workdir stamp source object locator restored
  size_bytes="$(default_bench_bytes)"
  stamp="$(fieldtest_timestamp_id)"
  workdir="$(mktemp -d "$(fieldtest_spool_dir)/bench-read-${stamp}.XXXXXX")"
  source="$workdir/read-source.bin"
  object="$workdir/read-object.rao"
  locator="$workdir/locator.json"
  restored="$workdir/restored.rao"
  make_payload "$source" "$size_bytes"
  if ! fieldtest_capture_json "$workdir/build.json" "$(fieldtest_rem_bin)" archive build --inputs "$source" --out "$object"; then
    fieldtest_evidence_record "$SCRIPT_NAME" build FAIL "archive build failed for read benchmark" "$workdir/build.json"
    exit 1
  fi
  if ! fieldtest_capture_io_json "$locator" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" write --library "$serial" --file "$object" --pool fieldtest-a; then
    fieldtest_evidence_record "$SCRIPT_NAME" write FAIL "daemon write failed for read benchmark" "$locator"
    exit 1
  fi

  local start end seconds mb_s bytes
  start="$(python3 -c 'import time; print(f"{time.monotonic():.9f}")')"
  if ! fieldtest_capture_io_json "$workdir/read.json" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" read --object "$(cat "$locator")" --out "$restored"; then
    fieldtest_evidence_record "$SCRIPT_NAME" read FAIL "daemon read failed for read benchmark" "$workdir/read.json"
    exit 1
  fi
  end="$(python3 -c 'import time; print(f"{time.monotonic():.9f}")')"
  bytes="$(stat -c '%s' "$restored")"
  seconds="$(python3 - "$start" "$end" <<'PY'
import sys
print(float(sys.argv[2]) - float(sys.argv[1]))
PY
)"
  mb_s="$(python3 - "$bytes" "$seconds" <<'PY'
import sys
bytes_count = int(sys.argv[1])
seconds = float(sys.argv[2])
print(f"{(bytes_count / seconds) / (1024 * 1024):.2f}" if seconds > 0 else "inf")
PY
)"
  fieldtest_bench_record read "$serial" n/a fieldtest-a "$mb_s" "$seconds" "$bytes"
  fieldtest_evidence_record "$SCRIPT_NAME" sustained PASS "full-object read restored $bytes bytes at ${mb_s} MB/s" "$workdir/read.json"

  local range_dir
  range_dir="$workdir/range"
  mkdir -p "$range_dir"
  local mid_off late_off
  mid_off=$(( size_bytes / 2 ))
  late_off=$(( size_bytes - 1048576 ))
  for spec in "early:0:1048576" "mid:${mid_off}:1048576" "late:${late_off}:1048576"; do
    local label start_off len
    IFS=: read -r label start_off len <<<"$spec"
    if ! fieldtest_capture_json "$workdir/$label.json" "$(fieldtest_rem_bin)" archive extract --object "$object" --dest "$range_dir/$label" --path read-source.bin --range "${start_off}:${len}" --overwrite; then
      fieldtest_evidence_record "$SCRIPT_NAME" "$label" FAIL "range extract failed for $label" "$workdir/$label.json"
      exit 1
    fi
    fieldtest_evidence_record "$SCRIPT_NAME" "$label" PASS "range extract succeeded for $label window" "$workdir/$label.json"
  done

  rm -rf -- "$workdir"
}

fieldtest_run_with_lock main "$@"
