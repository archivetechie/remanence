#!/usr/bin/env bash
set -euo pipefail

SCRIPT_NAME="20-bench-write"
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

usage() {
  cat <<'EOF'
Usage: 20-bench-write.sh [--source DIR] [--help]

Measures sustained write throughput for incompressible and compressible payloads
and samples rem top every 5s while the write is in flight.
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
  local path="$1" bytes="$2" mode="$3"
  python3 - "$path" "$bytes" "$mode" <<'PY'
import os
import sys
from pathlib import Path

path = Path(sys.argv[1])
bytes_needed = int(sys.argv[2])
mode = sys.argv[3]
chunk = 8 * 1024 * 1024
path.parent.mkdir(parents=True, exist_ok=True)
with path.open("wb") as handle:
    remaining = bytes_needed
    while remaining > 0:
        n = min(chunk, remaining)
        if mode == "random":
            handle.write(os.urandom(n))
        else:
            handle.write(b"\0" * n)
        remaining -= n
PY
}

default_bench_bytes() {
  if [[ "${REMFIELD_ENV:-unknown}" == vtl ]]; then
    printf '%s\n' "${FIELD_BENCH_BYTES_VTL:-1073741824}"
  else
    printf '%s\n' "${FIELD_BENCH_BYTES:-2147483648}"
  fi
}

sample_top() {
  local stop_file="$1"
  local n=0
  while [[ ! -f "$stop_file" ]]; do
    local sample
    sample="$(fieldtest_artifact_path "$SCRIPT_NAME" "top-${n}" "$(fieldtest_timestamp_id)")"
    fieldtest_capture_json "$sample" "$(fieldtest_rem_bin)" top --endpoint "$(fieldtest_rem_endpoint)" --once --json || true
    sleep 5
    n=$((n + 1))
  done
}

bench_write_case() {
  local serial="$1" source="$2" pool="$3" label="$4"
  local start end seconds mb_s bytes top_stop stamp out_path
  stamp="$(fieldtest_timestamp_id)"
  out_path="$(fieldtest_artifact_path "$SCRIPT_NAME" "$label" "$stamp")"
  top_stop="$out_path.stop"
  mkdir -p "$(dirname -- "$out_path")"
  : >"$top_stop"
  sample_top "$top_stop" &
  local sampler_pid=$!
  start="$(python3 -c 'import time; print(f"{time.monotonic():.9f}")')"
  if ! fieldtest_capture_json "$out_path" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" write --library "$serial" --file "$source" --pool "$pool"; then
    touch "$top_stop"
    wait "$sampler_pid" || true
    fieldtest_evidence_record "$SCRIPT_NAME" "$label" FAIL "daemon write failed for $label" "$out_path"
    return 1
  fi
  end="$(python3 -c 'import time; print(f"{time.monotonic():.9f}")')"
  touch "$top_stop"
  wait "$sampler_pid" || true
  bytes="$(stat -c '%s' "$source")"
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
  fieldtest_bench_record "write-$label" "$serial" "n/a" "$pool" "$mb_s" "$seconds" "$bytes"
  fieldtest_evidence_record "$SCRIPT_NAME" "$label" PASS "wrote $bytes bytes at ${mb_s} MB/s" "$out_path"
}

main() {
  if [[ "${1:-}" == --help || "${1:-}" == -h ]]; then
    usage
    exit 0
  fi

  local source_dir=""
  if [[ "${1:-}" == --source ]]; then
    source_dir="${2:?missing source directory}"
    shift 2
  fi

  fieldtest_init_layout
  fieldtest_detect_env || true
  local serial
  serial="$(fieldtest_selected_library_serial)"
  if [[ -z "$serial" ]]; then
    echo "error: no selected library; run bringup first" >&2
    exit 1
  fi

  local stamp workdir payload_bytes random_file zeros_file
  stamp="$(fieldtest_timestamp_id)"
  workdir="$(mktemp -d "$(fieldtest_spool_dir)/bench-write-${stamp}.XXXXXX")"
  payload_bytes="$(default_bench_bytes)"
  if [[ -n "$source_dir" ]]; then
    random_file="$source_dir/random.bin"
    zeros_file="$source_dir/zeros.bin"
  else
    random_file="$workdir/random.bin"
    zeros_file="$workdir/zeros.bin"
    make_payload "$random_file" "$payload_bytes" random
    make_payload "$zeros_file" "$payload_bytes" zero
  fi

  bench_write_case "$serial" "$random_file" "fieldtest-a" "incompressible"
  bench_write_case "$serial" "$zeros_file" "fieldtest-b" "compressible"
  fieldtest_evidence_record "$SCRIPT_NAME" summary PASS "bench-write completed for incompressible and compressible payloads"
  rm -rf -- "$workdir"
}

fieldtest_run_with_lock main "$@"
