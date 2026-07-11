#!/usr/bin/env bash
set -euo pipefail

SCRIPT_NAME="22-bench-dual"
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

usage() {
  cat <<'EOF'
Usage: 22-bench-dual.sh [--help]

Runs a restore from fieldtest-a concurrently with an append to fieldtest-b: the
physical HBA-decision leg. If routing is opaque, per-session and aggregate
numbers are still recorded.
EOF
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
        handle.write(os.urandom(n) if mode == "random" else b"\0" * n)
        remaining -= n
PY
}

default_bench_bytes() {
  if [[ "${REMFIELD_ENV:-unknown}" == vtl ]]; then
    printf '%s\n' "${FIELD_BENCH_DUAL_BYTES_VTL:-536870912}"
  else
    printf '%s\n' "${FIELD_BENCH_DUAL_BYTES:-2147483648}"
  fi
}

bench_one() {
  local serial="$1" source="$2" pool="$3" out="$4"
  local start end seconds bytes mb_s
  start="$(python3 -c 'import time; print(f"{time.monotonic():.9f}")')"
  if ! fieldtest_capture_io_json "$out" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" write --library "$serial" --file "$source" --pool "$pool"; then
    return 1
  fi
  end="$(python3 -c 'import time; print(f"{time.monotonic():.9f}")')"
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
  printf '%s %s %s\n' "$bytes" "$seconds" "$mb_s"
}

bench_read_one() {
  local locator="$1" restored="$2" out="$3"
  local start end seconds bytes mb_s
  start="$(python3 -c 'import time; print(f"{time.monotonic():.9f}")')"
  if ! fieldtest_capture_io_json "$out" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" read --object "$(cat "$locator")" --out "$restored"; then
    return 1
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
  printf '%s %s %s\n' "$bytes" "$seconds" "$mb_s"
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
  fieldtest_require_pool_appendable_tapes fieldtest-a 1 "dual write fieldtest-a leg"
  fieldtest_require_pool_appendable_tapes fieldtest-b 1 "dual write fieldtest-b leg"

  local stamp workdir bytes random_file zeros_file prewarm_file top_stop locator restored
  stamp="$(fieldtest_timestamp_id)"
  workdir="$(mktemp -d "$(fieldtest_spool_dir)/bench-dual-${stamp}.XXXXXX")"
  bytes="$(default_bench_bytes)"
  random_file="$workdir/random.bin"
  zeros_file="$workdir/zeros.bin"
  make_payload "$random_file" "$bytes" random
  make_payload "$zeros_file" "$bytes" zero
  prewarm_file="$workdir/prewarm.bin"
  make_payload "$prewarm_file" 1048576 random
  locator="$workdir/read-fixture.json"
  restored="$workdir/restored.bin"
  if ! fieldtest_capture_io_json "$locator" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" write --library "$serial" --file "$random_file" --pool fieldtest-a; then
    fieldtest_evidence_record "$SCRIPT_NAME" fixture FAIL "could not write restore fixture before concurrent leg" "$locator"
    return 1
  fi
  fieldtest_capture_io_json "$workdir/prewarm-a.json" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" write --library "$serial" --file "$prewarm_file" --pool fieldtest-a 9>&- &
  local prewarm_pid_a=$!
  fieldtest_capture_io_json "$workdir/prewarm-b.json" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" write --library "$serial" --file "$prewarm_file" --pool fieldtest-b 9>&- &
  local prewarm_pid_b=$!
  local prewarm_rc_a=0 prewarm_rc_b=0
  wait "$prewarm_pid_a" || prewarm_rc_a=$?
  wait "$prewarm_pid_b" || prewarm_rc_b=$?
  if (( prewarm_rc_a != 0 || prewarm_rc_b != 0 )); then
    fieldtest_evidence_record "$SCRIPT_NAME" prewarm FAIL "kit-defect-9 concurrent pre-warm/resume-wait failed (a=$prewarm_rc_a b=$prewarm_rc_b)" "$workdir"
    return 1
  fi
  fieldtest_evidence_record "$SCRIPT_NAME" prewarm PASS "both dual-drive legs conditioned through readiness-aware fixture/pre-warm I/O"

  top_stop="$workdir/top.stop"
  rm -f -- "$top_stop"
  (while [[ ! -f "$top_stop" ]]; do
      fieldtest_capture_json "$(fieldtest_artifact_path "$SCRIPT_NAME" top "$(fieldtest_timestamp_id)")" "$(fieldtest_rem_bin)" top --endpoint "$(fieldtest_rem_endpoint)" --once --json || true
      sleep 5
    done) 9>&- &
  local sampler_pid=$!

  local out_a="$workdir/a.json" out_b="$workdir/b.json"
  local start end seconds_a seconds_b mb_a bytes_b mb_b total_mb total_bytes
  start="$(python3 -c 'import time; print(f"{time.monotonic():.9f}")')"
  bench_read_one "$locator" "$restored" "$out_a" >"$workdir/a.metrics" 9>&- &
  local pid_a=$!
  bench_one "$serial" "$zeros_file" fieldtest-b "$out_b" >"$workdir/b.metrics" 9>&- &
  local pid_b=$!
  local rc_a=0 rc_b=0
  wait "$pid_a" || rc_a=$?
  wait "$pid_b" || rc_b=$?
  end="$(python3 -c 'import time; print(f"{time.monotonic():.9f}")')"
  touch "$top_stop"
  wait "$sampler_pid" || true
  if (( rc_a != 0 || rc_b != 0 )); then
    fieldtest_evidence_record "$SCRIPT_NAME" summary FAIL "dual write failed (fieldtest-a rc=$rc_a, fieldtest-b rc=$rc_b)" "$workdir"
    return 1
  fi
  fieldtest_capture_tape_io_mode "$SCRIPT_NAME" dual-append staging_ring_open
  fieldtest_capture_tape_io_mode "$SCRIPT_NAME" dual-restore restore_total

  read -r total_bytes seconds_a mb_a <"$workdir/a.metrics" || true
  read -r bytes_b seconds_b mb_b <"$workdir/b.metrics" || true
  total_bytes="$(( total_bytes + bytes_b ))"
  total_mb="$(python3 - "$total_bytes" "$start" "$end" <<'PY'
import sys
total_bytes = int(sys.argv[1])
seconds = float(sys.argv[3]) - float(sys.argv[2])
print(f"{(total_bytes / seconds) / (1024 * 1024):.2f}" if seconds > 0 else "inf")
PY
)"
  fieldtest_bench_record dual-restore "$serial" n/a fieldtest-a "$mb_a" "$seconds_a" "$(( total_bytes - bytes_b ))"
  fieldtest_bench_record dual-b "$serial" n/a fieldtest-b "$mb_b" "$seconds_b" "$bytes_b"
  fieldtest_bench_record dual-aggregate "$serial" n/a concurrent "$total_mb" "$(python3 - "$start" "$end" <<'PY'
import sys
print(float(sys.argv[2]) - float(sys.argv[1]))
PY
)" "$total_bytes"
  if [[ "${REMFIELD_ENV:-unknown}" == vtl ]]; then
    fieldtest_evidence_record "$SCRIPT_NAME" summary INFO "concurrent restore+append ran on VTL; numbers are not physically meaningful"
  else
    fieldtest_evidence_record "$SCRIPT_NAME" summary PASS "concurrent restore+append completed against two pools; aggregate ${total_mb} MB/s" "$workdir"
  fi
  rm -rf -- "$workdir"
}

fieldtest_run_with_lock main "$@"
