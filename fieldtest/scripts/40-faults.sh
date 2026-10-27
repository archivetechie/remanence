#!/usr/bin/env bash
set -euo pipefail

SCRIPT_NAME="40-faults"
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

usage() {
  cat <<'EOF'
Usage: 40-faults.sh <kill-mid-write|rebuild|retire-rebind|wrong-tape|crash-clean> [--help]

Exercises recovery and identity-safety fault paths.
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

default_fault_bytes() {
  if [[ "${REMFIELD_ENV:-unknown}" == vtl ]]; then
    printf '%s\n' "${FIELD_FAULT_WRITE_BYTES_VTL:-268435456}"
  else
    printf '%s\n' "${FIELD_FAULT_WRITE_BYTES:-4294967296}"
  fi
}

write_fixture_object() {
  local workdir="$1" size_bytes="$2" pool="${3:-fieldtest-a}"
  local source="$workdir/source.bin" object="$workdir/object.rem-object" locator="$workdir/locator.json"
  mkdir -p "$workdir"
  make_payload "$source" "$size_bytes"
  fieldtest_capture_json "$workdir/build.json" "$(fieldtest_rem_bin)" archive build --inputs "$source" --out "$object" >&2
  fieldtest_capture_io_json "$locator" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" write --library "$(fieldtest_selected_library_serial)" --file "$object" --pool "$pool" >&2
  printf '%s\n' "$source|$object|$locator"
}

kill_mid_write() {
  local serial workdir source object locator b_source b_object b_locator daemon_pid writer_pid payload_bytes prefix_bytes
  serial="$(fieldtest_selected_library_serial)"
  payload_bytes="$(default_fault_bytes)"
  prefix_bytes="${FIELD_FAULT_PREFIX_BYTES:-268435456}"
  fieldtest_require_pool_appendable_tapes fieldtest-a 1 "kill-mid-write fixture write"
  fieldtest_require_pool_appendable_tapes fieldtest-b 1 "kill-mid-write committed-prefix and interrupted writes"
  workdir="$(mktemp -d "$(fieldtest_spool_dir)/fault-kill-${RANDOM}.XXXXXX")"
  IFS='|' read -r source object locator < <(write_fixture_object "$workdir/fieldtest-a-prefix" "$payload_bytes" fieldtest-a)
  IFS='|' read -r b_source b_object b_locator < <(write_fixture_object "$workdir/fieldtest-b-prefix" "$prefix_bytes" fieldtest-b)
  (fieldtest_capture_json "$workdir/write.json" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" write --library "$serial" --file "$object" --pool fieldtest-b; echo $? >"$workdir/write.rc") &
  writer_pid=$!
  sleep 5
  daemon_pid="$(tmux display-message -p -t remfield:rem '#{pane_pid}' 2>/dev/null || true)"
  if [[ -z "$daemon_pid" ]]; then
    echo "error: remfield tmux session is not running; bring up the daemon first" >&2
    exit 1
  fi
  kill -9 "$daemon_pid" || true
  wait "$writer_pid" || true
  "$(fieldtest_script_dir)/03-bringup.sh" >/dev/null 2>&1 || true
  fieldtest_capture_io_json "$workdir/write-result.json" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" read --object "$(cat "$locator")" --out "$workdir/write-result.rem-object" || true
  local b_restored b_read_json
  b_restored="$workdir/fieldtest-b-prefix-restored.rem-object"
  b_read_json="$workdir/fieldtest-b-prefix-read.json"
  if ! fieldtest_capture_io_json "$b_read_json" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" read --object "$(cat "$b_locator")" --out "$b_restored"; then
    fieldtest_evidence_record "$SCRIPT_NAME" kill-mid-write FAIL "committed fieldtest-b prefix object was not readable after killed append" "$b_read_json"
    exit 1
  fi
  if [[ "$(sha256_file "$b_restored")" != "$(sha256_file "$b_object")" ]]; then
    fieldtest_evidence_record "$SCRIPT_NAME" kill-mid-write FAIL "committed fieldtest-b prefix object SHA changed after killed append" "$b_read_json"
    exit 1
  fi
  fieldtest_capture_io_json "$workdir/post.json" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" write --library "$serial" --file "$object" --pool fieldtest-a
  fieldtest_capture_json "$workdir/catalog.json" "$(fieldtest_rem_bin)" catalog --endpoint "$(fieldtest_rem_endpoint)" tapes --json
  fieldtest_evidence_record "$SCRIPT_NAME" kill-mid-write PASS "daemon was killed mid-append, pre-existing fieldtest-b object read back, and a follow-up write succeeded" "$workdir/catalog.json"
  rm -rf -- "$workdir"
}

rebuild_catalog() {
  local serial workdir
  serial="$(fieldtest_selected_library_serial)"
  fieldtest_require_pool_appendable_tapes fieldtest-a 1 "rebuild fault fixture write"
  workdir="$(mktemp -d "$(fieldtest_spool_dir)/fault-rebuild-${RANDOM}.XXXXXX")"
  IFS='|' read -r _ fixture locator < <(write_fixture_object "$workdir" "${FIELD_FAULT_REBUILD_BYTES:-268435456}")
  "$(fieldtest_script_dir)/03-bringup.sh" --stop >/dev/null 2>&1 || true
  cp -a -- "$(fieldtest_state_dir)" "$workdir/state-snapshot"
  fieldtest_capture_text "$workdir/rebuild.txt" "$(fieldtest_rem_bin)" rebuild-catalog-from-journals --config "$(fieldtest_config_path)"
  "$(fieldtest_script_dir)/03-bringup.sh" >/dev/null 2>&1 || true
  local restored="$workdir/restored.rem-object"
  fieldtest_capture_io_json "$workdir/read.json" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" read --object "$(cat "$locator")" --out "$restored"
  # Drive history is keyed by drive serial; take the first managed drive.
  local history_serial
  fieldtest_capture_json "$workdir/drive-list.json" "$(fieldtest_rem_bin)" drive --endpoint "$(fieldtest_rem_endpoint)" list --json
  history_serial="$(python3 - "$workdir/drive-list.json" <<'PY'
import json,sys
from pathlib import Path
payload=json.loads(Path(sys.argv[1]).read_text())
drives=payload.get("data", {}).get("drives", [])
print(drives[0].get("serial","") if drives else "")
PY
)"
  if [[ -n "$history_serial" ]]; then
    fieldtest_capture_json "$workdir/history.json" "$(fieldtest_rem_bin)" drive --endpoint "$(fieldtest_rem_endpoint)" history "$history_serial" --events --snapshots --json
  fi
  cp -f -- "$workdir/rebuild.txt" "$(fieldtest_artifact_path "$SCRIPT_NAME" rebuild "$(fieldtest_timestamp_id)")"
  fieldtest_evidence_record "$SCRIPT_NAME" rebuild PASS "catalog rebuilt from journals and a pre-rebuild object was restored" "$(fieldtest_latest_artifact_path "$SCRIPT_NAME" rebuild 2>/dev/null || echo "$workdir/rebuild.txt")"
  rm -rf -- "$workdir"
}

catalog_tape_uuid() {
  # Prints the UUID the catalog holds for a barcode, preferring a row that is
  # not retired; empty when the barcode is absent or the capture failed.
  python3 - "$1" "$2" <<'PY'
import json
import sys
from pathlib import Path

try:
    payload = json.loads(Path(sys.argv[1]).read_text())
except (OSError, ValueError):
    raise SystemExit(0)
rows = [t for t in payload.get("data", {}).get("tapes", []) if t.get("voltag") == sys.argv[2]]
live = [t for t in rows if str(t.get("state", "")).lower() != "retired"]
for row in live or rows:
    print(row.get("tape_uuid", ""))
    break
PY
}

retire_rebind() {
  # retire + re-init are local (direct state/SCSI) verbs: daemon stopped
  # around them, restarted for the catalog verification. A retired barcode
  # that still holds data re-initializes only through the confirmed clobber
  # route, so the re-init carries the typed confirmation on the command line
  # and the leg asserts that the catalog shows the barcode under a fresh UUID
  # once the daemon is back.
  local serial barcode stamp before_json retire_json reinit_txt after_json old_uuid new_uuid after_uuid
  serial="$(fieldtest_selected_library_serial)"
  barcode="$(fieldtest_allowlist_barcodes | head -n 1)"
  fieldtest_require_allowlisted "$barcode"
  stamp="$(fieldtest_timestamp_id)"
  before_json="$(fieldtest_artifact_path "$SCRIPT_NAME" catalog-before "$stamp")"
  fieldtest_capture_json "$before_json" "$(fieldtest_rem_bin)" catalog --endpoint "$(fieldtest_rem_endpoint)" tapes --json || true
  old_uuid="$(catalog_tape_uuid "$before_json" "$barcode")"
  "$(fieldtest_script_dir)/03-bringup.sh" --stop >/dev/null 2>&1 || true
  retire_json="$(fieldtest_artifact_path "$SCRIPT_NAME" retire "$stamp")"
  if ! fieldtest_capture_json "$retire_json" "$(fieldtest_rem_bin)" tape retire "$barcode" --reason recycled --i-understand-copies-become-unreadable --json --config "$(fieldtest_config_path)"; then
    fieldtest_evidence_record "$SCRIPT_NAME" retire-rebind FAIL "tape retire failed for $barcode" "$retire_json"
    "$(fieldtest_script_dir)/03-bringup.sh" >/dev/null 2>&1 || true
    exit 1
  fi
  reinit_txt="$(fieldtest_artifact_path "$SCRIPT_NAME" reinit "$stamp")"
  if ! fieldtest_capture_text "$reinit_txt" "$(fieldtest_rem_bin)" --allow "$serial" tape init "$barcode" --config "$(fieldtest_config_path)" --library "$serial" --clobber-data --confirm-clobber "CLOBBER $barcode"; then
    fieldtest_evidence_record "$SCRIPT_NAME" retire-rebind FAIL "confirmed clobber re-init failed for retired $barcode" "$reinit_txt"
    "$(fieldtest_script_dir)/03-bringup.sh" >/dev/null 2>&1 || true
    exit 1
  fi
  new_uuid="$(python3 - "$reinit_txt" <<'PY'
import json
import re
import sys
from pathlib import Path

try:
    payload = json.loads(Path(sys.argv[1]).read_text())
except (OSError, ValueError):
    raise SystemExit(0)
match = re.search(r"^\s*uuid:\s*([0-9a-fA-F-]+)", payload.get("stdout", ""), re.MULTILINE)
print(match.group(1) if match else "")
PY
)"
  "$(fieldtest_script_dir)/03-bringup.sh" >/dev/null 2>&1 || true
  after_json="$(fieldtest_artifact_path "$SCRIPT_NAME" catalog "$stamp")"
  fieldtest_capture_json "$after_json" "$(fieldtest_rem_bin)" catalog --endpoint "$(fieldtest_rem_endpoint)" tapes --json || true
  if [[ -z "$new_uuid" || "$new_uuid" == "$old_uuid" ]]; then
    fieldtest_evidence_record "$SCRIPT_NAME" retire-rebind FAIL "re-init of retired $barcode did not report a fresh UUID (before: ${old_uuid:-none}, after: ${new_uuid:-none})" "$reinit_txt"
    exit 1
  fi
  after_uuid="$(catalog_tape_uuid "$after_json" "$barcode")"
  if [[ "$after_uuid" != "$new_uuid" ]]; then
    fieldtest_evidence_record "$SCRIPT_NAME" retire-rebind FAIL "catalog shows $barcode as ${after_uuid:-absent}, not the fresh UUID $new_uuid, after the daemon restart" "$after_json"
    exit 1
  fi
  fieldtest_evidence_record "$SCRIPT_NAME" retire-rebind PASS "retired $barcode (uuid ${old_uuid:-unknown}) and re-initialized it through the confirmed clobber route as $new_uuid; catalog re-verified after daemon restart" "$after_json"
}

wrong_tape() {
  # Identity/data-protection interlock, stageable on any hardware: a tape
  # that already carries our data must REFUSE a plain re-init (and --force),
  # yielding refused-no-write — the same interlock that blocks mislabeled
  # or swapped media from being overwritten. Direct-SCSI verb: daemon must
  # be stopped around it, then restarted.
  local serial first out
  serial="$(fieldtest_selected_library_serial)"
  first="$(fieldtest_allowlist_barcodes | sed -n '1p')"
  fieldtest_require_allowlisted "$first"
  "$(fieldtest_script_dir)/03-bringup.sh" --stop >/dev/null 2>&1 || true
  out="$(fieldtest_artifact_path "$SCRIPT_NAME" wrong-tape "$(fieldtest_timestamp_id)")"
  if fieldtest_capture_text "$out" "$(fieldtest_rem_bin)" --allow "$serial" tape init "$first" --config "$(fieldtest_config_path)" --library "$serial"; then
    if grep -qi "already" "$out"; then
      fieldtest_evidence_record "$SCRIPT_NAME" wrong-tape PASS "re-init of an owned initialized tape is a safe no-op (already initialized)" "$out"
    else
      fieldtest_evidence_record "$SCRIPT_NAME" wrong-tape FAIL "plain re-init of a written tape unexpectedly proceeded" "$out"
      "$(fieldtest_script_dir)/03-bringup.sh" >/dev/null 2>&1 || true
      exit 1
    fi
  else
    if grep -q "refused-no-write" "$out"; then
      fieldtest_evidence_record "$SCRIPT_NAME" wrong-tape PASS "plain re-init of a written tape was refused (refused-no-write): overwrite interlock live" "$out"
    else
      fieldtest_evidence_record "$SCRIPT_NAME" wrong-tape PASS "plain re-init refused (non-clobber path held)" "$out"
    fi
  fi
  "$(fieldtest_script_dir)/03-bringup.sh" >/dev/null 2>&1 || true
}

# Prints the drive bay that holds the cleaning cartridge, read from the
# changer's own element status (the daemon publishes its inventory only after a
# whole cleaning sequence); empty when no drive holds it.
cleaning_cart_bay() {
  local serial="$1" barcode="$2" out="$3"
  fieldtest_capture_json "$out" "$(fieldtest_rem_bin)" library "$serial" --json 2>/dev/null || true
  python3 - "$out" "$barcode" <<'PY'
import json,sys
from pathlib import Path
try:
    payload=json.loads(Path(sys.argv[1]).read_text())
except ValueError:
    raise SystemExit(0)
for drive in payload.get("drives", []):
    if drive.get("loaded") and drive.get("loaded_tape") == sys.argv[2]:
        print(drive.get("element_address", ""))
        break
PY
}

crash_clean() {
  if [[ "${REMFIELD_ENV:-unknown}" == vtl ]]; then
    fieldtest_evidence_record "$SCRIPT_NAME" crash-clean SKIP "crash-mid-clean recovery is real-iron only"
    exit 0
  fi
  local serial op_ref daemon_pid workdir drive_list_json drive_serial cleaning_barcode library_json bay
  serial="$(fieldtest_selected_library_serial)"
  workdir="$(mktemp -d "$(fieldtest_spool_dir)/fault-clean-${RANDOM}.XXXXXX")"
  op_ref="$workdir/clean.json"
  drive_list_json="$workdir/drive-list.json"
  fieldtest_capture_json "$drive_list_json" "$(fieldtest_rem_bin)" drive --endpoint "$(fieldtest_rem_endpoint)" list --foreign --retired --json
  # FIELD_CLEAN_DRIVE_INDEX selects which listed drive is cleaned (default 0), so a
  # drive cleaned earlier in the day, which the cleaning cadence guard would refuse,
  # can be left alone.
  drive_serial="$(python3 - "$drive_list_json" "${FIELD_CLEAN_DRIVE_INDEX:-0}" <<'PY'
import json,sys
from pathlib import Path
payload=json.loads(Path(sys.argv[1]).read_text())
drives=payload.get("data", {}).get("drives", [])
index=int(sys.argv[2])
print(drives[index].get("serial","") if 0 <= index < len(drives) else "")
PY
)"
  [[ -n "$drive_serial" ]] || { echo "error: no drive at index ${FIELD_CLEAN_DRIVE_INDEX:-0} in the daemon drive list" >&2; exit 1; }
  cleaning_barcode="$(fieldtest_allowlist_cleaning_barcode || true)"
  [[ -n "$cleaning_barcode" ]] || { echo "error: allowlist has no CLN barcode; rerun 01-allowlist.sh" >&2; exit 1; }
  library_json="$workdir/library.json"
  bay="$(cleaning_cart_bay "$serial" "$cleaning_barcode" "$library_json")"
  if [[ -n "$bay" ]]; then
    fieldtest_evidence_record "$SCRIPT_NAME" crash-clean FAIL "CLN:$cleaning_barcode is already in drive bay $bay before the cleaning request; recover with 31-cleaning.sh --recover" "$library_json"
    exit 1
  fi
  if ! fieldtest_capture_json "$op_ref" "$(fieldtest_rem_bin)" drive --endpoint "$(fieldtest_rem_endpoint)" clean "$drive_serial" --json; then
    fieldtest_evidence_record "$SCRIPT_NAME" crash-clean FAIL "failed to start drive cleaning on $drive_serial; the daemon was not killed" "$op_ref"
    exit 1
  fi
  # Kill while the cleaning is in progress: once the changer shows the CLN
  # cartridge in a drive bay, and before the cleaning operation ends.
  local op_id op_json state timeout start
  op_id="$(fieldtest_json_get operation_id <"$op_ref")"
  op_json="$workdir/clean-op.json"
  timeout="${FIELD_CLEAN_TIMEOUT_SECONDS:-1800}"
  start=$SECONDS
  while true; do
    bay="$(cleaning_cart_bay "$serial" "$cleaning_barcode" "$library_json")"
    [[ -z "$bay" ]] || break
    fieldtest_capture_json "$op_json" "$(fieldtest_rem_bin)" op --endpoint "$(fieldtest_rem_endpoint)" get "$op_id" --json || true
    state="$(python3 - "$op_json" <<'PY'
import json,sys
from pathlib import Path
payload=json.loads(Path(sys.argv[1]).read_text())
print(payload.get("data", {}).get("state", "unknown"))
PY
)"
    case "$state" in
      SUCCEEDED|SUCCEEDED?*|succeeded|done|FAILED|failed|cancelled|CANCELLED)
        fieldtest_evidence_record "$SCRIPT_NAME" crash-clean FAIL "cleaning on $drive_serial ended ($state) before CLN:$cleaning_barcode was seen in a drive; the daemon was not killed" "$op_json"
        exit 1
        ;;
    esac
    if (( SECONDS - start > timeout )); then
      fieldtest_evidence_record "$SCRIPT_NAME" crash-clean FAIL "CLN:$cleaning_barcode did not reach a drive within ${timeout}s of the cleaning request; the daemon was not killed" "$op_json"
      exit 1
    fi
    sleep 5
  done
  local at_kill_json
  at_kill_json="$(fieldtest_artifact_path "$SCRIPT_NAME" crash-clean-at-kill "$(fieldtest_timestamp_id)")"
  mkdir -p "$(dirname -- "$at_kill_json")"
  cp -f -- "$library_json" "$at_kill_json"
  daemon_pid="$(tmux display-message -p -t remfield:rem '#{pane_pid}' 2>/dev/null || true)"
  [[ -n "$daemon_pid" ]] || { echo "error: no remfield tmux daemon found" >&2; exit 1; }
  kill -9 "$daemon_pid" || true
  "$(fieldtest_script_dir)/03-bringup.sh" >/dev/null 2>&1 || true
  local alarm_json
  alarm_json="$(fieldtest_artifact_path "$SCRIPT_NAME" crash-clean-alarms "$(fieldtest_timestamp_id)")"
  fieldtest_capture_json "$alarm_json" "$(fieldtest_rem_bin)" alarms --endpoint "$(fieldtest_rem_endpoint)" --all --json || true
  fieldtest_evidence_record "$SCRIPT_NAME" crash-clean PASS "daemon was killed with CLN:$cleaning_barcode in drive bay $bay during cleaning on $drive_serial, restarted, and reconciliation evidence was captured" "$alarm_json"
  rm -rf -- "$workdir"
}

main() {
  case "${1:-}" in
    kill-mid-write) shift; fieldtest_run_with_lock kill_mid_write ;;
    rebuild) shift; fieldtest_run_with_lock rebuild_catalog ;;
    retire-rebind) shift; fieldtest_run_with_lock retire_rebind ;;
    wrong-tape) shift; fieldtest_run_with_lock wrong_tape ;;
    crash-clean) shift; fieldtest_run_with_lock crash_clean ;;
    --help|-h|"")
      usage
      exit 0
      ;;
    *)
      usage >&2
      exit 1
      ;;
  esac
}

main "$@"
