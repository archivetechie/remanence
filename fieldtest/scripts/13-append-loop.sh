#!/usr/bin/env bash
set -euo pipefail

SCRIPT_NAME="13-append-loop"
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

usage() {
  cat <<'EOF'
Usage: 13-append-loop.sh [--pool POOL] [--count N] [--payload-mb N] [--mode cycle|session] [--help]

Writes N independent objects to one daemon pool, asserts that successful
responses land on one tape with dense tape-file numbers, then reads every
object back and verifies SHA-256 fidelity.

Modes:
  cycle    one remfield-io write per object, with full open/close per object
  session  one remfield-io write-many call, with all appends in one session
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
  local path="$1" bytes="$2" idx="$3"
  python3 - "$path" "$bytes" "$idx" <<'PY'
import os
import random
import sys
from pathlib import Path

path = Path(sys.argv[1])
bytes_needed = int(sys.argv[2])
idx = int(sys.argv[3])
chunk = 4 * 1024 * 1024
rng = random.Random(0xA99E0000 + idx)
path.parent.mkdir(parents=True, exist_ok=True)
with path.open("wb") as handle:
    remaining = bytes_needed
    while remaining > 0:
        n = min(chunk, remaining)
        if idx % 3 == 1:
            handle.write(b"\0" * n)
        elif idx % 3 == 2:
            handle.write(bytes([idx % 251]) * n)
        else:
            handle.write(rng.randbytes(n) if hasattr(rng, "randbytes") else os.urandom(n))
        remaining -= n
PY
}

default_count() {
  if [[ "${REMFIELD_ENV:-unknown}" == vtl ]]; then
    printf '%s\n' "${FIELD_APPEND_COUNT:-${FIELD_APPEND_COUNT_VTL:-3}}"
  else
    printf '%s\n' "${FIELD_APPEND_COUNT:-6}"
  fi
}

default_payload_mb() {
  if [[ "${REMFIELD_ENV:-unknown}" == vtl ]]; then
    printf '%s\n' "${FIELD_APPEND_PAYLOAD_MB:-${FIELD_APPEND_PAYLOAD_MB_VTL:-4}}"
  else
    printf '%s\n' "${FIELD_APPEND_PAYLOAD_MB:-64}"
  fi
}

append_loop_validate_mode() {
  case "$1" in
    cycle|session) return 0 ;;
    *) return 1 ;;
  esac
}

json_field_from_file() {
  local path="$1" field="$2"
  python3 - "$path" "$field" <<'PY'
import json
import sys
from pathlib import Path

payload = json.loads(Path(sys.argv[1]).read_text())
value = payload.get(sys.argv[2])
if value is None:
    raise SystemExit(1)
print(value)
PY
}

validate_append_writes() {
  local summary="$1" pool="$2"
  shift 2
  python3 - "$summary" "$pool" "$@" <<'PY'
import json
import sys
from pathlib import Path

summary = Path(sys.argv[1])
pool = sys.argv[2]
paths = [Path(arg) for arg in sys.argv[3:]]
if not paths:
    raise SystemExit("no write locators were supplied")

writes = []
for idx, path in enumerate(paths):
    payload = json.loads(path.read_text())
    if "error" in payload:
        raise SystemExit(f"write {idx} failed: {payload['error']}")
    info = payload.get("append_commit_info")
    if not isinstance(info, dict):
        raise SystemExit(f"write {idx} missing append_commit_info")
    tape_uuid = payload.get("tape_uuid")
    tape_file = payload.get("tape_file_number")
    if not tape_uuid:
        raise SystemExit(f"write {idx} missing tape_uuid")
    if not isinstance(tape_file, int) or tape_file <= 0:
        raise SystemExit(f"write {idx} has invalid tape_file_number {tape_file!r}")
    if info.get("tape_uuid") != tape_uuid:
        raise SystemExit(f"write {idx} append_commit_info.tape_uuid mismatch")
    if info.get("tape_file_number") != tape_file:
        raise SystemExit(f"write {idx} append_commit_info.tape_file_number mismatch")
    expected_mode = "fresh" if tape_file == 1 else "append"
    if info.get("append_mode") != expected_mode:
        raise SystemExit(
            f"write {idx} append_mode {info.get('append_mode')!r} != {expected_mode!r}"
        )
    # MTA-1 currently exposes only locator-derived evidence. When durable
    # append records land, update this assertion and the runbook together.
    for field in (
        "position_before_lba",
        "position_after_lba",
        "journal_record_ordinal",
        "estimated_remaining_bytes",
        "sealed_after_write",
    ):
        if info.get(field) is not None:
            raise SystemExit(f"write {idx} unexpectedly populated unproven field {field}")
    writes.append({
        "idx": idx,
        "object_id": payload.get("object_id"),
        "caller_object_id": payload.get("caller_object_id"),
        "content_sha256": payload.get("content_sha256"),
        "tape_uuid": tape_uuid,
        "tape_file_number": tape_file,
        "first_body_lba": payload.get("first_body_lba"),
        "append_mode": info.get("append_mode"),
        "locator_path": str(path),
    })

tape_uuid = writes[0]["tape_uuid"]
files = [item["tape_file_number"] for item in writes]
if any(item["tape_uuid"] != tape_uuid for item in writes):
    raise SystemExit("append loop did not stay on one tape_uuid")
for previous, current in zip(files, files[1:]):
    if current != previous + 1:
        raise SystemExit(f"append loop tape files are not dense: {files}")

summary.parent.mkdir(parents=True, exist_ok=True)
summary.write_text(json.dumps({
    "pool": pool,
    "count": len(writes),
    "tape_uuid": tape_uuid,
    "first_tape_file_number": files[0],
    "last_tape_file_number": files[-1],
    "dense_tape_file_numbers": files,
    "writes": writes,
}, indent=2, sort_keys=True) + "\n")
PY
}

split_write_many_records() {
  local batch="$1" count="$2" summary_out="$3"
  shift 3
  python3 - "$batch" "$count" "$summary_out" "$@" <<'PY'
import json
import sys
from pathlib import Path

batch = Path(sys.argv[1])
expected_count = int(sys.argv[2])
summary_out = Path(sys.argv[3])
paths = [Path(arg) for arg in sys.argv[4:]]
records = [
    json.loads(line)
    for line in batch.read_text().splitlines()
    if line.strip()
]
objects = [record for record in records if record.get("record_type") == "object"]
summaries = [record for record in records if record.get("record_type") == "summary"]
errors = [record for record in objects if record.get("error")]
if errors:
    first = errors[0]
    raise SystemExit(
        f"write-many failed at object {first.get('object_index')}: {first.get('error')}"
    )
if len(objects) != expected_count:
    raise SystemExit(f"write-many emitted {len(objects)} object records, expected {expected_count}")
if len(paths) != expected_count:
    raise SystemExit(f"internal error: {len(paths)} locator paths for {expected_count} records")
for idx, (record, path) in enumerate(zip(objects, paths)):
    if record.get("object_index") != idx:
        raise SystemExit(f"write-many object index {record.get('object_index')} != {idx}")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(record, indent=2, sort_keys=True) + "\n")
if summaries:
    summary_out.parent.mkdir(parents=True, exist_ok=True)
    summary_out.write_text(json.dumps(summaries[-1], indent=2, sort_keys=True) + "\n")
PY
}

augment_append_summary() {
  local summary="$1" mode="$2" latency_jsonl="$3" fence_json="$4"
  python3 - "$summary" "$mode" "$latency_jsonl" "$fence_json" <<'PY'
import json
import statistics
import sys
from pathlib import Path

summary_path = Path(sys.argv[1])
mode = sys.argv[2]
latency_path = Path(sys.argv[3])
fence = json.loads(sys.argv[4])
summary = json.loads(summary_path.read_text())
latencies = [
    float(json.loads(line)["seconds"])
    for line in latency_path.read_text().splitlines()
    if line.strip()
]
if not latencies:
    raise SystemExit("no write latency samples were recorded")
latencies_sorted = sorted(latencies)
stats = {
    "min_seconds": latencies_sorted[0],
    "median_seconds": statistics.median(latencies_sorted),
    "max_seconds": latencies_sorted[-1],
    "samples": len(latencies_sorted),
}
summary["mode"] = mode
summary["write_latency_seconds"] = stats
summary["fence_accounting"] = fence
summary_path.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
PY
}

append_loop_summary_extra_json() {
  local summary="$1"
  python3 - "$summary" <<'PY'
import json
import sys
from pathlib import Path

summary = json.loads(Path(sys.argv[1]).read_text())
latency = summary["write_latency_seconds"]
fence = summary["fence_accounting"]
extra = {
    "mode": summary["mode"],
    "write_latency_min_seconds": latency["min_seconds"],
    "write_latency_median_seconds": latency["median_seconds"],
    "write_latency_max_seconds": latency["max_seconds"],
    "write_latency_samples": latency["samples"],
    "fence_count": fence.get("fence_count", 0),
    "fence_wait_seconds": fence.get("fence_wait_seconds", 0.0),
    "fence_ratio": fence.get("fence_ratio", 0.0),
    "io_calls": fence.get("io_calls", 0),
}
print(json.dumps(extra, separators=(",", ":")))
PY
}

append_loop_selftest_setup() {
  local tmpdir="$1" wait_elapsed="$2"
  mkdir -p "$tmpdir/home/bin" "$tmpdir/home/state" "$tmpdir/home/evidence" "$tmpdir/home/spool" "$tmpdir/home/log"
  printf '%s\n' LIBMAIN >"$tmpdir/home/state/selected-library"
  : >"$tmpdir/home/config.toml"
  cat >"$tmpdir/home/bin/rem" <<EOF
#!/usr/bin/env bash
set -euo pipefail
if [[ "\${1:-}" == tape && "\${2:-}" == wait-ready ]]; then
  cat <<'JSON'
{"schema":"rem.tape.wait_ready.v1","state":"ready","ready":true,"attempts":3,"elapsed":${wait_elapsed},"exit_code":0}
JSON
  exit 0
fi
echo "unexpected rem invocation: \$*" >&2
exit 1
EOF
  chmod +x "$tmpdir/home/bin/rem"
  cat >"$tmpdir/mock-io" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
state="${MOCK_IO_STATE:?}"
count=0
if [[ -f "$state" ]]; then
  count="$(cat "$state")"
fi
printf '%s\n' "$((count + 1))" >"$state"
if [[ "$count" -eq 0 ]]; then
  echo '{"error":"media-readiness fence operation=11111111-1111-1111-1111-111111111111"}' >&2
  exit 1
fi
echo '{"ok":true}'
EOF
  chmod +x "$tmpdir/mock-io"
}

append_loop_selftest_warn_threshold() {
  local tmpdir="$1" counters outfile records
  append_loop_selftest_setup "$tmpdir" 5
  REMFIELD_HOME="$tmpdir/home"
  REMFIELD_ENV=selftest
  FIELD_IO_READY_RETRIES=1
  FIELD_READY_WARN_SECS=1
  FIELD_READY_FAIL_SECS=100
  MOCK_IO_STATE="$tmpdir/io-count"
  export REMFIELD_HOME REMFIELD_ENV FIELD_IO_READY_RETRIES FIELD_READY_WARN_SECS FIELD_READY_FAIL_SECS MOCK_IO_STATE
  counters="$tmpdir/home/state/fence-counters.json"
  FIELDTEST_FENCE_COUNTERS_FILE="$counters"
  export FIELDTEST_FENCE_COUNTERS_FILE
  fieldtest_init_fence_counters "$counters"
  outfile="$tmpdir/home/evidence/io.json"
  if ! fieldtest_capture_io_json "$outfile" "$tmpdir/mock-io" >/dev/null 2>"$tmpdir/stderr"; then
    echo "selftest: warned fence should retry successfully" >&2
    return 1
  fi
  fieldtest_emit_fence_summary >/dev/null
  records="$tmpdir/home/evidence/records.jsonl"
  grep -q '"readiness_warning":true' "$records" || {
    echo "selftest: warning threshold did not mark readiness_warning" >&2
    return 1
  }
  grep -q '"test_id":"fence-summary"' "$records" || {
    echo "selftest: fence-summary record was not emitted" >&2
    return 1
  }
}

append_loop_selftest_fail_threshold() {
  local tmpdir="$1" counters outfile records
  append_loop_selftest_setup "$tmpdir" 5
  REMFIELD_HOME="$tmpdir/home"
  REMFIELD_ENV=selftest
  FIELD_IO_READY_RETRIES=1
  FIELD_READY_WARN_SECS=1
  FIELD_READY_FAIL_SECS=2
  MOCK_IO_STATE="$tmpdir/io-count"
  export REMFIELD_HOME REMFIELD_ENV FIELD_IO_READY_RETRIES FIELD_READY_WARN_SECS FIELD_READY_FAIL_SECS MOCK_IO_STATE
  counters="$tmpdir/home/state/fence-counters.json"
  FIELDTEST_FENCE_COUNTERS_FILE="$counters"
  export FIELDTEST_FENCE_COUNTERS_FILE
  fieldtest_init_fence_counters "$counters"
  outfile="$tmpdir/home/evidence/io.json"
  if fieldtest_capture_io_json "$outfile" "$tmpdir/mock-io" >/dev/null 2>"$tmpdir/stderr"; then
    echo "selftest: fail threshold should abort retry loop" >&2
    return 1
  fi
  records="$tmpdir/home/evidence/records.jsonl"
  grep -q '"readiness_failure_threshold_exceeded":true' "$records" || {
    echo "selftest: fail threshold did not mark readiness_failure_threshold_exceeded" >&2
    return 1
  }
  grep -q '"status":"FAIL"' "$records" || {
    echo "selftest: fail threshold did not log FAIL" >&2
    return 1
  }
}

append_loop_selftest() {
  local tmpdir
  append_loop_validate_mode cycle || {
    echo "selftest: cycle mode rejected" >&2
    return 1
  }
  append_loop_validate_mode session || {
    echo "selftest: session mode rejected" >&2
    return 1
  }
  if append_loop_validate_mode junk; then
    echo "selftest: junk mode accepted" >&2
    return 1
  fi
  local default_mode="cycle"
  [[ "$default_mode" == cycle ]] || {
    echo "selftest: default mode is not cycle" >&2
    return 1
  }

  tmpdir="$(mktemp -d)"
  APPEND_LOOP_SELFTEST_TMPDIR="$tmpdir"
  trap 'rm -rf -- "${APPEND_LOOP_SELFTEST_TMPDIR:-}"' EXIT
  append_loop_selftest_warn_threshold "$tmpdir/warn"
  append_loop_selftest_fail_threshold "$tmpdir/fail"
}

main() {
  local pool="fieldtest-a" count="" payload_mb="" mode="cycle"
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --help|-h)
        usage
        exit 0
        ;;
      --pool)
        pool="${2:?missing pool}"
        shift 2
        ;;
      --count)
        count="${2:?missing count}"
        shift 2
        ;;
      --payload-mb)
        payload_mb="${2:?missing payload size in MiB}"
        shift 2
        ;;
      --mode)
        mode="${2:?missing mode}"
        if ! append_loop_validate_mode "$mode"; then
          echo "error: --mode must be cycle or session" >&2
          exit 1
        fi
        shift 2
        ;;
      *)
        usage >&2
        exit 1
        ;;
    esac
  done

  fieldtest_init_layout
  fieldtest_detect_env || true
  local serial
  serial="$(fieldtest_selected_library_serial)"
  if [[ -z "$serial" ]]; then
    echo "error: no selected library; run bringup first" >&2
    exit 1
  fi

  count="${count:-$(default_count)}"
  payload_mb="${payload_mb:-$(default_payload_mb)}"
  if ! [[ "$count" =~ ^[0-9]+$ ]] || (( count < 2 )); then
    echo "error: --count must be an integer >= 2" >&2
    exit 1
  fi
  if ! [[ "$payload_mb" =~ ^[0-9]+$ ]] || (( payload_mb < 1 )); then
    echo "error: --payload-mb must be a positive integer" >&2
    exit 1
  fi
  if ! append_loop_validate_mode "$mode"; then
    echo "error: --mode must be cycle or session" >&2
    exit 1
  fi
  fieldtest_require_pool_appendable_tapes "$pool" 1 "append loop"

  local stamp workdir payload_bytes summary validation_log sha_results latency_results
  stamp="$(fieldtest_timestamp_id)"
  workdir="$(mktemp -d "$(fieldtest_spool_dir)/append-${stamp}.XXXXXX")"
  trap '[[ -n "${workdir:-}" && -d "$workdir" ]] && rm -rf -- "$workdir"' EXIT
  payload_bytes=$(( payload_mb * 1024 * 1024 ))
  summary="$(fieldtest_artifact_path "$SCRIPT_NAME" summary "$stamp")"
  validation_log="$(fieldtest_artifact_path "$SCRIPT_NAME" validation "$stamp")"
  sha_results="$workdir/sha-results.jsonl"
  latency_results="$workdir/write-latencies.jsonl"
  mkdir -p "$(dirname -- "$summary")"

  local -a locators source_shas
  local idx source locator write_start write_end write_seconds batch write_many_summary
  if [[ "$mode" == cycle ]]; then
    for ((idx = 0; idx < count; idx++)); do
      source="$workdir/object-${idx}.bin"
      locator="$(fieldtest_artifact_path "$SCRIPT_NAME" "write-${idx}" "$stamp")"
      make_payload "$source" "$payload_bytes" "$idx"
      source_shas[idx]="$(sha256_file "$source")"
      write_start="$(fieldtest_monotonic_seconds)"
      if ! fieldtest_capture_io_json "$locator" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" write --library "$serial" --file "$source" --pool "$pool"; then
        fieldtest_evidence_record "$SCRIPT_NAME" "write-${idx}" FAIL "append-loop write $idx failed" "$locator"
        exit 1
      fi
      write_end="$(fieldtest_monotonic_seconds)"
      write_seconds="$(fieldtest_seconds_diff "$write_start" "$write_end")"
      python3 - "$latency_results" "$idx" "$write_seconds" <<'PY'
import json
import sys
from pathlib import Path

path = Path(sys.argv[1])
idx = int(sys.argv[2])
seconds = float(sys.argv[3])
with path.open("a") as handle:
    handle.write(json.dumps({"idx": idx, "seconds": seconds}, separators=(",", ":")) + "\n")
PY
      locators[idx]="$locator"
    done
  else
    batch="$(fieldtest_artifact_path "$SCRIPT_NAME" "write-many" "$stamp")"
    write_many_summary="$(fieldtest_artifact_path "$SCRIPT_NAME" "write-many-summary" "$stamp")"
    for ((idx = 0; idx < count; idx++)); do
      locators[idx]="$(fieldtest_artifact_path "$SCRIPT_NAME" "write-${idx}" "$stamp")"
    done
    if ! fieldtest_capture_io_json "$batch" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" write-many --library "$serial" --pool "$pool" --count "$count" --size-mib "$payload_mb" --caller-object-id-prefix "append-${stamp}"; then
      fieldtest_evidence_record "$SCRIPT_NAME" write-many FAIL "append-loop write-many failed" "$batch"
      exit 1
    fi
    if ! split_write_many_records "$batch" "$count" "$write_many_summary" "${locators[@]}" >"$validation_log" 2>&1; then
      fieldtest_evidence_record "$SCRIPT_NAME" write-many FAIL "append-loop write-many output was invalid" "$validation_log"
      exit 1
    fi
    rm -f -- "$validation_log"
    for ((idx = 0; idx < count; idx++)); do
      source_shas[idx]="$(json_field_from_file "${locators[$idx]}" content_sha256)"
      write_seconds="$(python3 - "${locators[$idx]}" <<'PY'
import json
import sys
from pathlib import Path

record = json.loads(Path(sys.argv[1]).read_text())
transfer_ms = float(record.get("transfer_ms") or 0.0)
print(f"{transfer_ms / 1000.0:.6f}")
PY
)"
      python3 - "$latency_results" "$idx" "$write_seconds" <<'PY'
import json
import sys
from pathlib import Path

path = Path(sys.argv[1])
idx = int(sys.argv[2])
seconds = float(sys.argv[3])
with path.open("a") as handle:
    handle.write(json.dumps({"idx": idx, "seconds": seconds}, separators=(",", ":")) + "\n")
PY
    done
  fi

  if ! validate_append_writes "$summary" "$pool" "${locators[@]}" >"$validation_log" 2>&1; then
    fieldtest_evidence_record "$SCRIPT_NAME" dense-files FAIL "append writes were not same-tape dense commits" "$validation_log"
    exit 1
  fi
  rm -f -- "$validation_log"
  fieldtest_evidence_record "$SCRIPT_NAME" dense-files PASS "wrote $count objects to one tape with dense tape-file numbers" "$summary"

  local restored read_json restored_sha
  for ((idx = 0; idx < count; idx++)); do
    restored="$workdir/restored-${idx}.bin"
    read_json="$(fieldtest_artifact_path "$SCRIPT_NAME" "read-${idx}" "$stamp")"
    if ! fieldtest_capture_io_json "$read_json" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" read --object "$(cat "${locators[$idx]}")" --out "$restored"; then
      fieldtest_evidence_record "$SCRIPT_NAME" "read-${idx}" FAIL "append-loop read $idx failed" "$read_json"
      exit 1
    fi
    restored_sha="$(sha256_file "$restored")"
    if [[ "$restored_sha" != "${source_shas[$idx]}" ]]; then
      fieldtest_evidence_record "$SCRIPT_NAME" "fidelity-${idx}" FAIL "append-loop SHA mismatch for object $idx" "$read_json"
      exit 1
    fi
    python3 - "$sha_results" "$idx" "${source_shas[$idx]}" "$restored_sha" "$read_json" <<'PY'
import json
import sys
from pathlib import Path

path = Path(sys.argv[1])
idx, source_sha, restored_sha, read_json = sys.argv[2:]
with path.open("a") as handle:
    handle.write(json.dumps({
        "idx": int(idx),
        "source_sha256": source_sha,
        "restored_sha256": restored_sha,
        "read_artifact": read_json,
    }, separators=(",", ":")) + "\n")
PY
    fieldtest_evidence_record "$SCRIPT_NAME" "fidelity-${idx}" PASS "append-loop object $idx restored with matching SHA-256" "$read_json"
  done

  python3 - "$summary" "$sha_results" <<'PY'
import json
import sys
from pathlib import Path

summary_path = Path(sys.argv[1])
sha_path = Path(sys.argv[2])
summary = json.loads(summary_path.read_text())
summary["sha_results"] = [
    json.loads(line)
    for line in sha_path.read_text().splitlines()
    if line.strip()
]
summary_path.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
PY

  augment_append_summary "$summary" "$mode" "$latency_results" "$(fieldtest_fence_counters_json)"
  local summary_extra
  summary_extra="$(append_loop_summary_extra_json "$summary")"
  fieldtest_evidence_record "$SCRIPT_NAME" summary PASS "append loop completed in $mode mode: $count objects, ${payload_mb} MiB each, pool $pool" "$summary" "$summary_extra"
}

if [[ "${1:-}" == --selftest ]]; then
  append_loop_selftest
  exit 0
fi

fieldtest_run_with_lock main "$@"
