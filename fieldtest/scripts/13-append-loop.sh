#!/usr/bin/env bash
set -euo pipefail

SCRIPT_NAME="13-append-loop"
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

usage() {
  cat <<'EOF'
Usage: 13-append-loop.sh [--pool POOL] [--count N] [--payload-mb N] [--help]

Writes N independent objects to one daemon pool, asserts that successful
responses land on one tape with dense tape-file numbers, then reads every
object back and verifies SHA-256 fidelity.
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

main() {
  local pool="fieldtest-a" count="" payload_mb=""
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
  fieldtest_require_pool_appendable_tapes "$pool" 1 "append loop"

  local stamp workdir payload_bytes summary validation_log sha_results
  stamp="$(fieldtest_timestamp_id)"
  workdir="$(mktemp -d "$(fieldtest_spool_dir)/append-${stamp}.XXXXXX")"
  trap '[[ -n "${workdir:-}" && -d "$workdir" ]] && rm -rf -- "$workdir"' EXIT
  payload_bytes=$(( payload_mb * 1024 * 1024 ))
  summary="$(fieldtest_artifact_path "$SCRIPT_NAME" summary "$stamp")"
  validation_log="$(fieldtest_artifact_path "$SCRIPT_NAME" validation "$stamp")"
  sha_results="$workdir/sha-results.jsonl"
  mkdir -p "$(dirname -- "$summary")"

  local -a locators source_shas
  local idx source locator
  for ((idx = 0; idx < count; idx++)); do
    source="$workdir/object-${idx}.bin"
    locator="$(fieldtest_artifact_path "$SCRIPT_NAME" "write-${idx}" "$stamp")"
    make_payload "$source" "$payload_bytes" "$idx"
    source_shas[idx]="$(sha256_file "$source")"
    if ! fieldtest_capture_io_json "$locator" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" write --library "$serial" --file "$source" --pool "$pool"; then
      fieldtest_evidence_record "$SCRIPT_NAME" "write-${idx}" FAIL "append-loop write $idx failed" "$locator"
      exit 1
    fi
    locators[idx]="$locator"
  done

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

  fieldtest_evidence_record "$SCRIPT_NAME" summary PASS "append loop completed: $count objects, ${payload_mb} MiB each, pool $pool" "$summary"
}

fieldtest_run_with_lock main "$@"
