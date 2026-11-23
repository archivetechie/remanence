#!/usr/bin/env bash
# Prove an older state copy refuses a pinned append while reads still work,
# then put back the newer records and complete the same append on the same tape.
set -euo pipefail

SCRIPT_NAME="14-restore-tail"
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

final_recorded=0
state_in_place='current (nothing swapped)'
snapshot_dir=''
tape=''
displaced_count=0

format_fail() {
  local message="$1"
  message+="; state in place: $state_in_place; snapshots: ${snapshot_dir:-none yet}; T=${tape:-not yet known}"
  case "$state_in_place" in
    older|partial\ *) message+='; run no other plan with this kit home; send the pack; the snapshots stay on the host' ;;
  esac
  printf '%s\n' "$message"
}

record_step() {
  local step="$1" status="$2" summary="$3" detail="${4:-}"
  [[ "$status" != FAIL ]] || summary="$(format_fail "$summary")"
  if [[ -d "$detail" && -n "${evidence_dir:-}" ]]; then
    printf '%s\nlocation: %s\n' "$summary" "$detail" >"$evidence_dir/$step.txt"
    detail="$evidence_dir/$step.txt"
  fi
  fieldtest_evidence_record "$SCRIPT_NAME" "restore-tail-$step" "$status" "$summary" "$detail"
  [[ "$step" != final ]] || final_recorded=1
}

finish_exit() {
  local rc="$1" message='leg ended before its final result'
  if [[ "$final_recorded" != 1 ]]; then
    record_step final FAIL "$message"
    [[ "$rc" != 0 ]] || rc=1
  fi
  fieldtest_cleanup_workdirs
  trap - EXIT
  exit "$rc"
}

fail() {
  local step="$1" message="$2" detail="${3:-}"
  record_step "$step" FAIL "$message" "$detail"
  record_step final FAIL "$message" "$detail"
  exit 1
}

stop_daemon() {
  local step="$1" detail="$evidence_dir/$1.txt"
  fieldtest_capture_json "$evidence_dir/$step-catalog.json" "$(fieldtest_rem_bin)" catalog \
    --endpoint "$(fieldtest_rem_endpoint)" tapes --json || fail "$step" 'cannot capture catalog before stop' "$evidence_dir/$step-catalog.json"
  fieldtest_capture_text "$detail" "$(fieldtest_script_dir)/03-bringup.sh" --stop \
    || fail "$step" 'stop helper failed' "$detail"
  if fieldtest_daemon_running >>"$detail" 2>&1; then
    fail "$step" 'daemon still running after stop' "$detail"
  fi
  flock -n "$(fieldtest_state_dir)/state.lock" true >>"$detail" 2>&1 \
    || fail "$step" 'state lock is held or unavailable after stop' "$detail"
  if ! "$(fieldtest_python)" - "$(fieldtest_records_path)" >>"$detail" 2>&1 <<'PY'
import json
import sys
from pathlib import Path

rows = [json.loads(line) for line in Path(sys.argv[1]).read_text().splitlines()]
stops = [r for r in rows if r['script'] == '03-bringup' and r['test_id'] == 'stop']
if not stops or stops[-1]['status'] != 'PASS':
    raise SystemExit('newest 03-bringup stop record is not PASS')
print('daemon down; state lock available; newest stop record PASS')
PY
  then
    fail "$step" 'stop was not a clean PASS (forced kills are refused)' "$detail"
  fi
  record_step "$step" PASS 'clean stop; daemon down and state lock available; catalog captured' "$detail"
}

start_daemon() {
  local step="$1" detail="$evidence_dir/$1.txt"
  fieldtest_capture_text "$detail" "$(fieldtest_script_dir)/03-bringup.sh" \
    || fail "$step" 'start helper failed' "$detail"
  fieldtest_daemon_running >>"$detail" 2>&1 || fail "$step" 'daemon not running after start' "$detail"
  record_step "$step" PASS 'daemon started' "$detail"
}

is_kit_state() {
  # A new kit file in state/ must join this exclusion list.
  case "${1##*/}" in
    selected-library|run.pid|soak.pid|fence-counters.*|media-readiness.jsonl) return 0 ;;
    state.lock) return 0 ;; # Keep the same lock inode through every copy and move.
    *) return 1 ;;
  esac
}

snapshot_state() {
  local name="$1" entry state_fd
  exec {state_fd}>>"$(fieldtest_state_dir)/state.lock" || return
  flock -n "$state_fd" || fail "snapshot-$name" 'state lock is held or unavailable for snapshot'
  mkdir -- "$snapshot_dir/$name" || return
  # calibration/ rolls back with the copy, as a real restore of the records would.
  for entry in "$(fieldtest_state_dir)"/*; do
    is_kit_state "$entry" && continue
    cp -a -- "$entry" "$snapshot_dir/$name/" || return
  done
  exec {state_fd}>&- || return
  record_step "snapshot-$name" PASS "state copied to $snapshot_dir/$name" "$snapshot_dir/$name"
}

restore_state() {
  local name="$1" entry state_fd incoming="$snapshot_dir/incoming-$1" displaced
  exec {state_fd}>>"$(fieldtest_state_dir)/state.lock" || return
  flock -n "$state_fd" || fail "restore-$name" 'state lock is held or unavailable for restore'
  mkdir -- "$incoming" || return
  cp -a -- "$snapshot_dir/$name/." "$incoming/" || return
  # Compare before moving even one entry: a bad staged copy leaves state current.
  verify_manifest "$snapshot_dir/$name" "$incoming" >"$evidence_dir/restore-$name-manifest.txt" 2>&1 \
    || fail "restore-$name" 'staged copy differs from snapshot (manifest verification failed)' "$evidence_dir/restore-$name-manifest.txt"
  displaced_count=$((displaced_count + 1))
  displaced="$snapshot_dir/displaced-$displaced_count"
  mkdir -- "$displaced" || return
  state_in_place="partial (restoring $name; daemon down; neither copy whole in state/); incoming: $incoming; displaced: $displaced"
  for entry in "$(fieldtest_state_dir)"/*; do
    is_kit_state "$entry" && continue
    mv -- "$entry" "$displaced/" || return
  done
  for entry in "$incoming"/*; do
    mv -- "$entry" "$(fieldtest_state_dir)/" || return
  done
  state_in_place="$name"
  rmdir -- "$incoming" || return
  exec {state_fd}>&- || return
  record_step "restore-$name" PASS "$name state restored; kit files retained" "$snapshot_dir/$name"
}

verify_manifest() {
  "$(fieldtest_python)" - "$1" "$2" <<'PY'
"""Compare state trees without following links; reject every metadata/content drift."""
import hashlib
import os
from pathlib import Path
import stat
import sys

def manifest(root):
    rows = []
    def visit(directory):
        for path in sorted(directory.iterdir()):
            metadata = path.lstat()
            kind = stat.S_IFMT(metadata.st_mode)
            mode = stat.S_IMODE(metadata.st_mode)
            content = None
            size = None
            if stat.S_ISREG(metadata.st_mode):
                size = metadata.st_size
                digest = hashlib.sha256()
                with path.open('rb') as stream:
                    while True:
                        chunk = stream.read(1024 * 1024)
                        if not chunk:
                            break
                        digest.update(chunk)
                content = digest.hexdigest()
            elif stat.S_ISLNK(metadata.st_mode):
                content = os.readlink(path)
            elif not stat.S_ISDIR(metadata.st_mode):
                raise SystemExit('unsupported state entry: ' + str(path))
            rows.append((str(path.relative_to(root)), kind, mode, size, content))
            if stat.S_ISDIR(metadata.st_mode):
                visit(path)
    visit(root)
    return rows

source, incoming = [manifest(Path(arg)) for arg in sys.argv[1:]]
if source != incoming:
    print('snapshot manifest:', source)
    print('incoming manifest:', incoming)
    raise SystemExit('staged manifest differs from snapshot')
print('staged manifest matches snapshot (paths, types, modes, file sizes, SHA-256, links)')
PY
}

require_quiescent() {
  local step="$1" detail="$evidence_dir/$1-top.json"
  fieldtest_capture_json "$detail" "$(fieldtest_rem_bin)" top \
    --endpoint "$(fieldtest_rem_endpoint)" --once --json \
    || fail "$step" 'cannot capture closeout debt before snapshot stop' "$detail"
  "$(fieldtest_python)" - "$detail" <<'PY'
import json
from pathlib import Path
import sys

debt = json.loads(Path(sys.argv[1]).read_text())['data']['closeout_debt']
if not isinstance(debt, list) or debt:
    raise SystemExit('closeout_debt must be an empty list before snapshot stop')
PY
}

require_snapshot_space() {
  local used available
  used="$(du -sb -- "$(fieldtest_state_dir)")" || fail space 'cannot measure state size'
  used="${used%%[[:space:]]*}"
  available="$(df -PB1 -- "$(fieldtest_home)" | awk 'NR==2 {print $4}')" || fail space 'cannot measure home free space'
  [[ "$used" =~ ^[0-9]+$ && "$available" =~ ^[0-9]+$ ]] || fail space 'invalid state size or free space'
  (( available >= 4 * used + 1073741824 )) || fail space 'snapshot space requires four times state size plus 1 GiB free'
  fieldtest_require_space "$((4 * used))" 'restore-tail state copies'
  record_step space PASS "home free bytes $available >= 4 * state bytes $used + 1 GiB; state copies passed the kit disk guard"
}

locator_tape() {
  "$(fieldtest_python)" - "$1" <<'PY'
import json
import sys
import uuid
from pathlib import Path

# UUID rejects null, empty and malformed placement, and normalizes both formats.
payload = json.loads(Path(sys.argv[1]).read_text())
print(uuid.UUID(payload['tape_uuid']).hex)
PY
}

pool_write() {
  local label="$1"
  fieldtest_capture_io_json "$evidence_dir/$label-locator.json" "$(fieldtest_io_bin)" \
    --endpoint "$(fieldtest_rem_endpoint)" write --library "$serial" --file "$workdir/$label.bin" --pool fieldtest-a \
    || fail "write-$label" "$label pool write failed" "$evidence_dir/$label-locator.json"
  locator_tape "$evidence_dir/$label-locator.json" >"$evidence_dir/$label-tape.txt" \
    || fail "write-$label" "$label has no valid non-null tape_uuid" "$evidence_dir/$label-locator.json"
  record_step "write-$label" PASS "$label written to fieldtest-a" "$evidence_dir/$label-locator.json"
}

read_object() {
  local label="$1" step="$2" locator="$3" compare_source="$4"
  local output="$workdir/$step.bin" detail="$evidence_dir/$step.json"
  fieldtest_capture_io_json "$detail" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" \
    read --object "$(cat "$locator")" --out "$output" || return 1
  "$(fieldtest_python)" - "$output" "$workdir/$label.bin" "$compare_source" "$evidence_dir/$step-sha256.json" "$evidence_dir/C-receipt.json" <<'PY'
import hashlib
import json
import sys
from pathlib import Path

def sha(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as stream:
        while True:
            chunk = stream.read(1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
    return digest.hexdigest()

restored = sha(sys.argv[1])
if sys.argv[3] == 'yes':
    source = sha(sys.argv[2])
    result = {'source_sha256': source, 'read_sha256': restored}
else:
    receipt = json.loads(Path(sys.argv[5]).read_text())
    result = {'content_sha256': receipt['objects'][0]['content_sha256'], 'read_sha256': restored}
Path(sys.argv[4]).write_text(json.dumps(result, indent=2) + '\n')
if sys.argv[3] == 'yes' and source != restored:
    raise SystemExit('SHA-256 mismatch')
PY
}

put_c() {
  fieldtest_capture_json "$1" "$(fieldtest_rem_bin)" put --endpoint "$(fieldtest_rem_endpoint)" \
    --tape "$tape" --pool fieldtest-a --json "$workdir/C.bin"
}

expected_refusal() {
  "$(fieldtest_python)" - "$1" "$tape" <<'PY'
import json
import re
import sys
from pathlib import Path

payload = json.loads(Path(sys.argv[1]).read_text())
for line in payload['stderr'].splitlines():
    try:
        error = json.loads(line)
    except ValueError:
        continue
    if not isinstance(error, dict):
        continue
    text = error.get('message')
    if error.get('schema') != 'rem.error.v1' or error.get('code') != 'failed_precondition' or not isinstance(text, str):
        continue
    match = re.search(r'TailBeyondKnownState: tape ([0-9a-fA-F-]+)(?=,|\s|$)', text)
    if (match and match[1].replace('-', '').lower() == sys.argv[2]
            and 'start from a newer copy of the state directory' in text
            and 'supersede-tail' in text and 'durable library fence failed' not in text):
        break
else:
    raise SystemExit('not the required durable TailBeyondKnownState refusal for T')
PY
  local rc=$?
  [[ "$rc" == 0 ]] || return "$rc"
  local detail="$evidence_dir/C-older-top.json"
  fieldtest_capture_json "$detail" "$(fieldtest_rem_bin)" top \
    --endpoint "$(fieldtest_rem_endpoint)" --once --json || return
  "$(fieldtest_python)" - "$detail" "$tape" <<'PY'
import json
from pathlib import Path
import sys
import uuid

fences = json.loads(Path(sys.argv[1]).read_text())['data']['active_tape_io_fences']
if not isinstance(fences, list) or not any(uuid.UUID(fence['tape_uuid']).hex == sys.argv[2] for fence in fences):
    raise SystemExit('no active tape I/O fence on T')
PY
}

c_locator() {
  "$(fieldtest_python)" - "$evidence_dir/C-receipt.json" "$tape" <<'PY'
import json
import sys
import uuid
from pathlib import Path

receipt = json.loads(Path(sys.argv[1]).read_text())
objects = receipt['objects']
if receipt['objects_committed'] != 1 or len(objects) != 1:
    raise SystemExit('C did not commit exactly one object')
obj = objects[0]
copies = obj['copies']
if not copies or any(uuid.UUID(c['tape_uuid']).hex != sys.argv[2] for c in copies):
    raise SystemExit('C has missing placement or a copy off T')
print(json.dumps({'object_id': obj['object_id'], 'tape_uuid': sys.argv[2], 'pool_id': 'fieldtest-a'}))
PY
}

main() {
  set -e
  trap 'finish_exit "$?"' EXIT
  [[ $# -eq 0 ]] || fail arguments '14-restore-tail.sh takes no arguments'
  fieldtest_init_layout
  fieldtest_detect_env || true
  fieldtest_daemon_running >/dev/null || fail entry 'daemon must be up on entry'
  local serial stamp workdir evidence_dir state_device home_device
  state_device="$(stat -c %d -- "$(fieldtest_state_dir)")" || fail entry 'cannot stat state filesystem'
  home_device="$(stat -c %d -- "$(fieldtest_home)")" || fail entry 'cannot stat home filesystem'
  [[ "$state_device" == "$home_device" ]] \
    || fail entry 'state and home must be on the same filesystem'
  local bytes size_mb="${FIELD_RESTORE_TAIL_MB:-64}" a_read_ok=1 outcome
  [[ "${REMFIELD_ENV:-unknown}" != vtl || -n "${FIELD_RESTORE_TAIL_MB:-}" ]] || size_mb=4
  serial="$(fieldtest_selected_library_serial)"
  [[ -n "$serial" ]] || fail entry 'no selected library'
  fieldtest_require_pool_appendable_tapes fieldtest-a 1 'restore-tail writes' || fail entry 'pool is not appendable'
  bytes="$("$(fieldtest_python)" - "$size_mb" <<'PY'
from decimal import Decimal
import sys
size = Decimal(sys.argv[1]) * 1048576
if not size.is_finite() or size < 1:
    raise SystemExit('FIELD_RESTORE_TAIL_MB must specify a positive size')
print(int(size))
PY
)" || fail inputs 'invalid FIELD_RESTORE_TAIL_MB'
  stamp="$(fieldtest_timestamp_id)"
  mkdir -p -- "$(fieldtest_home)/restore-tail"
  # mktemp also distinguishes invocations started within the same second.
  snapshot_dir="$(mktemp -d "$(fieldtest_home)/restore-tail/$stamp.XXXXXX")"
  evidence_dir="$(fieldtest_script_evidence_dir "$SCRIPT_NAME")/${snapshot_dir##*/}"
  mkdir -p -- "$evidence_dir"
  fieldtest_make_workdir workdir "restore-tail-$stamp" "$((bytes * 8))" 'restore-tail payloads'
  trap 'finish_exit "$?"' EXIT
  shopt -s dotglob nullglob
  "$(fieldtest_python)" - "$workdir" "$bytes" <<'PY'
import os
import sys
from pathlib import Path
for name in 'ABC':
    remaining = int(sys.argv[2])
    with (Path(sys.argv[1]) / f'{name}.bin').open('wb') as stream:
        while remaining:
            chunk = os.urandom(min(remaining, 1024 * 1024))
            stream.write(chunk)
            remaining -= len(chunk)
PY
  record_step inputs PASS "three distinct random payloads; $bytes bytes each" "$evidence_dir"

  pool_write A
  tape="$(cat "$evidence_dir/A-tape.txt")"
  read_object A read-A "$evidence_dir/A-locator.json" yes || fail read-A 'A read or SHA-256 comparison failed' "$evidence_dir/read-A.json"
  record_step read-A PASS 'A read back with matching SHA-256' "$evidence_dir/read-A-sha256.json"
  require_snapshot_space
  require_quiescent stop-older || fail stop-older 'closeout debt is non-empty or missing; nothing swapped' "$evidence_dir/stop-older-top.json"
  stop_daemon stop-older
  snapshot_state older || fail snapshot-older 'older snapshot failed' "$snapshot_dir"

  start_daemon start-for-B
  pool_write B
  [[ "$(cat "$evidence_dir/B-tape.txt")" == "$tape" ]] || fail placement-B 'B landed off T; no restore attempted' "$evidence_dir/B-locator.json"
  read_object B read-B "$evidence_dir/B-locator.json" yes || fail read-B 'B read or SHA-256 comparison failed' "$evidence_dir/read-B.json"
  record_step read-B PASS 'B on T read back with matching SHA-256' "$evidence_dir/read-B-sha256.json"
  require_quiescent stop-newer || fail stop-newer 'closeout debt is non-empty or missing; nothing swapped' "$evidence_dir/stop-newer-top.json"
  stop_daemon stop-newer
  snapshot_state newer || fail snapshot-newer 'newer snapshot failed' "$snapshot_dir"

  restore_state older || fail restore-older 'older restore failed' "$snapshot_dir"
  start_daemon start-older
  outcome=other
  if put_c "$evidence_dir/C-older.json"; then
    outcome=accepted
  elif expected_refusal "$evidence_dir/C-older.json"; then
    outcome=refused
  fi
  if [[ "$outcome" != refused ]]; then
    printf '%s\n' "$outcome" >"$snapshot_dir/OUTCOME"
    printf '%s\n' "$outcome" >"$evidence_dir/OUTCOME"
    fail C-older "C outcome $outcome under older; no restore attempted; keep both snapshots; leg does nothing more; plan teardown then stops the daemon as after any fatal leg" "$evidence_dir/C-older.json"
  fi
  record_step C-older PASS 'pinned C refused with TailBeyondKnownState and newer-state guidance' "$evidence_dir/C-older.json"
  if read_object A read-A-older "$evidence_dir/A-locator.json" yes; then
    record_step read-A-older PASS 'A still reads through the write fence with matching SHA-256' "$evidence_dir/read-A-older-sha256.json"
  else
    record_step read-A-older FAIL 'A read or SHA-256 comparison failed; restoring newer before ending FAIL' "$evidence_dir/read-A-older.json"
    a_read_ok=0
  fi

  stop_daemon stop-after-refusal
  restore_state newer || fail restore-newer 'newer restore failed' "$snapshot_dir"
  start_daemon start-newer
  [[ "$a_read_ok" == 1 ]] || fail recovery 'A failed to read under older; newer is back in place; keep snapshots' "$evidence_dir/read-A-older.json"
  put_c "$evidence_dir/C-receipt.json" || fail C-newer 'pinned C failed under newer; keep snapshots' "$evidence_dir/C-receipt.json"
  c_locator >"$evidence_dir/C-locator.json" || fail C-newer 'C receipt invalid or off T; keep snapshots' "$evidence_dir/C-receipt.json"
  record_step C-newer PASS 'same pinned C committed exactly once on T under newer' "$evidence_dir/C-receipt.json"
  read_object B read-B-newer "$evidence_dir/B-locator.json" yes || fail read-B-newer 'B read or SHA-256 comparison failed' "$evidence_dir/read-B-newer.json"
  record_step read-B-newer PASS 'B reads with matching SHA-256 under newer' "$evidence_dir/read-B-newer-sha256.json"
  read_object C read-C "$evidence_dir/C-locator.json" no || fail read-C 'C read failed' "$evidence_dir/read-C.json"
  record_step read-C PASS 'C read succeeded; receipt content SHA-256 and read SHA-256 recorded side by side' "$evidence_dir/read-C-sha256.json"
  fieldtest_daemon_running >/dev/null || fail exit 'daemon must be up on exit'
  rm -rf -- "$snapshot_dir"
  record_step final PASS "older refused C and still read A; newer accepted C on T=$tape and read B and C; snapshots removed" "$evidence_dir"
}

fieldtest_run_with_lock main "$@"
