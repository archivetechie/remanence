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

Set FIELD_BENCH_BATCH_SWEEP=1 to run write_batch_blocks/read_batch_blocks
over 8, 16, 32, and 64, restarting the daemon between batches.
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
  rm -f -- "$top_stop"
  sample_top "$top_stop" 9>&- &
  local sampler_pid=$!
  start="$(python3 -c 'import time; print(f"{time.monotonic():.9f}")')"
  if ! fieldtest_capture_io_json "$out_path" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" write --library "$serial" --file "$source" --pool "$pool"; then
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

bench_write_batch_list() {
  if [[ "${FIELD_BENCH_BATCH_SWEEP:-0}" == 1 ]]; then
    printf '%s\n' 8 16 32 64
  else
    printf '%s\n' current
  fi
}

restart_daemon_for_batch() {
  local batch="$1"
  FIELD_TAPE_IO_WRITE_BATCH_BLOCKS="$batch" \
    FIELD_TAPE_IO_READ_BATCH_BLOCKS="$batch" \
    "$(fieldtest_script_dir)/03-bringup.sh" --stop
  FIELD_TAPE_IO_WRITE_BATCH_BLOCKS="$batch" \
    FIELD_TAPE_IO_READ_BATCH_BLOCKS="$batch" \
    "$(fieldtest_script_dir)/03-bringup.sh"
}

restore_daemon_batch_config() {
  local write_batch="$1" read_batch="$2"
  fieldtest_evidence_record "$SCRIPT_NAME" batch-restore INFO "restoring daemon tape_io batch ${write_batch}/${read_batch}"
  FIELD_TAPE_IO_WRITE_BATCH_BLOCKS="$write_batch" \
    FIELD_TAPE_IO_READ_BATCH_BLOCKS="$read_batch" \
    "$(fieldtest_script_dir)/03-bringup.sh" --stop
  FIELD_TAPE_IO_WRITE_BATCH_BLOCKS="$write_batch" \
    FIELD_TAPE_IO_READ_BATCH_BLOCKS="$read_batch" \
    "$(fieldtest_script_dir)/03-bringup.sh"
}

bench_write_selftest() {
  local default_list sweep_list
  default_list="$(bench_write_batch_list | paste -sd, -)"
  if [[ "$default_list" != current ]]; then
    echo "selftest: default batch list should be current, got $default_list" >&2
    return 1
  fi
  sweep_list="$(FIELD_BENCH_BATCH_SWEEP=1 bench_write_batch_list | paste -sd, -)"
  if [[ "$sweep_list" != "8,16,32,64" ]]; then
    echo "selftest: sweep batch list should be 8,16,32,64, got $sweep_list" >&2
    return 1
  fi
}

main() {
  if [[ "${1:-}" == --help || "${1:-}" == -h ]]; then
    usage
    exit 0
  fi
  if [[ "${1:-}" == --selftest ]]; then
    bench_write_selftest
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
  fieldtest_require_pool_appendable_tapes fieldtest-a 1 "incompressible write benchmark"
  fieldtest_require_pool_appendable_tapes fieldtest-b 1 "compressible write benchmark"

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

  local original_write_batch original_read_batch batch label_prefix
  original_write_batch="${FIELD_TAPE_IO_WRITE_BATCH_BLOCKS:-16}"
  original_read_batch="${FIELD_TAPE_IO_READ_BATCH_BLOCKS:-$original_write_batch}"
  if [[ "${FIELD_BENCH_BATCH_SWEEP:-0}" == 1 ]]; then
    trap 'restore_daemon_batch_config "$original_write_batch" "$original_read_batch"' EXIT
  fi
  while IFS= read -r batch; do
    if [[ "$batch" == current ]]; then
      label_prefix=""
    else
      fieldtest_evidence_record "$SCRIPT_NAME" "batch-${batch}" INFO "restarting daemon for tape_io batch ${batch}"
      restart_daemon_for_batch "$batch"
      fieldtest_require_pool_appendable_tapes fieldtest-a 1 "incompressible write benchmark batch ${batch}"
      fieldtest_require_pool_appendable_tapes fieldtest-b 1 "compressible write benchmark batch ${batch}"
      label_prefix="batch-${batch}-"
    fi
    bench_write_case "$serial" "$random_file" "fieldtest-a" "${label_prefix}incompressible"
    bench_write_case "$serial" "$zeros_file" "fieldtest-b" "${label_prefix}compressible"
  done < <(bench_write_batch_list)
  if [[ "${FIELD_BENCH_BATCH_SWEEP:-0}" == 1 ]]; then
    restore_daemon_batch_config "$original_write_batch" "$original_read_batch"
    trap - EXIT
  fi
  fieldtest_evidence_record "$SCRIPT_NAME" summary PASS "bench-write completed for incompressible and compressible payloads"
  rm -rf -- "$workdir"
}

fieldtest_run_with_lock main "$@"
