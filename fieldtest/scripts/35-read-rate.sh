#!/usr/bin/env bash
# Measure forward reads and filemark spacing with the field daemon stopped.
set -euo pipefail
SCRIPT_NAME="35-read-rate"
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

usage() {
  cat <<'HELP'
Usage: 35-read-rate.sh [--barcode LABEL]

Select the first sealed tape in the selected library, as in 34-verify-throughput.
An exact --barcode also accepts a ready tape with committed files.
Capture idle bays and catalog, then stop the daemon with 03-bringup.sh --stop.
The daemon must remain stopped during all four measurements; this leg never starts it.
FIELD_READ_RATE_FILES sets files per window (default 20).
FIELD_READ_RATE_TIMEOUT_SECONDS bounds each measurement (default 14400).
For an already stopped daemon, supply both FIELD_READ_RATE_CATALOG and
FIELD_READ_RATE_BAYS as catalog/top JSON snapshots from the same tape population.
Read and space start at file 1, then at a file near the written tape's midpoint.
Evidence lands in $REMFIELD_HOME/evidence/35-read-rate/ (default ~/remfield).
HELP
}

fail() {
  echo "error: $2" >&2
  fieldtest_evidence_record "$SCRIPT_NAME" "$1" FAIL "$2" "${3:-}"
  exit 1
}

# UUID membership matches the verification leg; live barcode/home checks follow the stop.
select_tape() {
  python3 - "$@" <<'PY'
import json, sys, uuid
from pathlib import Path
catalog, top, serial, barcode, count = sys.argv[1:]
count = int(count)
def uid(value):
    return str(uuid.UUID(value)) if value else None
library = next(x for x in json.loads(Path(top).read_text())["data"]["libraries"]
               if x["library"]["library_serial"] == serial)
if any(d.get("status") in ("busy", "cleaning") for d in library["drives"]):
    raise SystemExit("selected library has busy bays")
resident = {uid(x.get("tape_uuid")) for x in library["slots"]}
resident.update(uid(x.get("loaded_tape_uuid")) for x in library["drives"])
for tape in json.loads(Path(catalog).read_text())["data"]["tapes"]:
    if uid(tape["tape_uuid"]) not in resident:
        continue
    if barcode and tape.get("voltag") != barcode:
        continue
    if tape["state"] != "sealed" and not (barcode and tape["state"] == "ready"):
        continue
    if not tape.get("voltag"):
        raise SystemExit("selected tape has no barcode")
    last = int(tape.get("last_committed_tape_file") or 0)
    # Two distinct full windows and their lookaheads require last >= count + 2.
    if last < 1:
        raise SystemExit("selected tape has no committed data files")
    requested = count
    partial = last < count + 2
    count = min(count, max(1, last - 2))
    middle = max(1, min(1 + last // 2, last - count))
    print(json.dumps(dict(tape, tape_uuid=uid(tape["tape_uuid"]), from_file=1,
                          second_from_file=middle, files=count, files_requested=requested,
                          partial=partial), separators=(",", ":")))
    break
else:
    raise SystemExit("no eligible tape in the selected library")
PY
}

main() {
  set -e
  local barcode=""
  while (($#)); do
    case "$1" in
      --help|-h) usage; return 0 ;;
      --barcode)
        [[ $# -ge 2 && -n "$2" && "$2" != --* ]] || { usage >&2; return 2; }
        barcode="$2"; shift 2 ;;
      *) usage >&2; return 2 ;;
    esac
  done
  local count="${FIELD_READ_RATE_FILES:-20}" limit="${FIELD_READ_RATE_TIMEOUT_SECONDS:-14400}"
  [[ "$count" =~ ^[1-9][0-9]*$ ]] || fail arguments 'FIELD_READ_RATE_FILES must be a positive integer'
  [[ "$limit" =~ ^[1-9][0-9]*$ ]] || fail arguments 'FIELD_READ_RATE_TIMEOUT_SECONDS must be a positive integer'
  command -v timeout >/dev/null || fail timeout 'timeout is required'
  command -v flock >/dev/null || fail daemon 'flock is required'
  fieldtest_init_layout
  local serial rem debug endpoint dir stamp top catalog selected inventory middle home tape out mode start partial rc=0
  serial="$(fieldtest_selected_library_serial)" || fail library 'select a library with bringup first'
  [[ -n "$serial" ]] || fail library 'selected library is empty'
  rem="$(fieldtest_rem_bin)" || fail binary 'rem is unavailable'
  debug="$(fieldtest_rem_debug_bin)" || fail binary 'rem-debug is unavailable'
  endpoint="$(fieldtest_rem_endpoint)"
  dir="$(fieldtest_script_evidence_dir "$SCRIPT_NAME")"
  mkdir -p "$dir"
  stamp="$(date -u +%Y%m%dT%H%M%S.%NZ)"
  top="$dir/$stamp-bays.json"
  catalog="$dir/$stamp-catalog.json"
  selected="$dir/$stamp-selected.json"
  inventory="$dir/$stamp-library.json"
  if [[ -n "${FIELD_READ_RATE_CATALOG:-}${FIELD_READ_RATE_BAYS:-}" ]]; then
    [[ -n "${FIELD_READ_RATE_CATALOG:-}" && -n "${FIELD_READ_RATE_BAYS:-}" ]] || fail selection 'supply both FIELD_READ_RATE_CATALOG and FIELD_READ_RATE_BAYS'
    if "$rem" daemon --endpoint "$endpoint" health >/dev/null 2>&1; then
      fail daemon 'daemon is running; stop it with 03-bringup.sh --stop before using snapshots'
    fi
    cp -- "$FIELD_READ_RATE_CATALOG" "$catalog"
    cp -- "$FIELD_READ_RATE_BAYS" "$top"
  else
    wait_for_idle_bays "$serial" "$top" || fail bays 'cannot capture idle bays; for a stopped daemon supply FIELD_READ_RATE_CATALOG and FIELD_READ_RATE_BAYS' "$top"
    fieldtest_capture_json "$catalog" "$rem" catalog tapes --json --endpoint "$endpoint" || fail catalog 'catalog tapes fails' "$catalog"
  fi
  select_tape "$catalog" "$top" "$serial" "$barcode" "$count" >"$selected" || fail selection 'cannot select two written windows; inspect selection diagnostics' "$selected"
  barcode="$(fieldtest_json_get voltag <"$selected")"
  tape="$(fieldtest_json_get tape_uuid <"$selected")"
  middle="$(fieldtest_json_get second_from_file <"$selected")"
  count="$(fieldtest_json_get files <"$selected")"
  partial="$(fieldtest_json_get partial <"$selected")"
  fieldtest_evidence_record "$SCRIPT_NAME" selection INFO "barcode=$barcode tape_uuid=$tape files=$count starts=1,$middle" "$selected"
  if [[ -z "${FIELD_READ_RATE_CATALOG:-}" ]]; then
    bash "$(fieldtest_script_dir)/03-bringup.sh" --stop || fail daemon 'stop helper fails; run 03-bringup.sh --stop before measuring'
  fi
  if "$rem" daemon --endpoint "$endpoint" health >/dev/null 2>&1; then
    fail daemon 'daemon is running; stop it with 03-bringup.sh --stop before measuring'
  fi
  # The command takes this same kernel lock before discovery and holds it through parking.
  flock -n "$(fieldtest_state_dir)/state.lock" true || fail daemon 'state lock is held or unavailable; run 03-bringup.sh --stop and check state directory'
  fieldtest_capture_json "$inventory" "$debug" library "$serial" --slots --json || fail inventory 'cannot inspect stopped library' "$inventory"
  home="$(python3 - "$inventory" "$serial" "$barcode" <<'PY'
import json, sys
from pathlib import Path
library = json.loads(Path(sys.argv[1]).read_text())
if library["serial"] != sys.argv[2]:
    raise SystemExit("wrong library")
barcode = sys.argv[3]
homes = [s["element_address_raw"] for s in library["slots"] if s["full"] and s.get("cartridge") == barcode]
homes += [d["source_slot_raw"] for d in library["drives"] if d["loaded"] and d.get("loaded_tape") == barcode]
if len(homes) != 1 or homes[0] is None:
    raise SystemExit("barcode has no unique exact home slot")
print(homes[0])
PY
)" || fail inventory 'barcode has no unique exact home slot' "$inventory"
  local -a starts=(1)
  [[ "$middle" == 1 ]] || starts+=("$middle")
  for start in "${starts[@]}"; do
    for mode in read space; do
      out="$dir/$stamp-$mode-from-$start.json"
      fieldtest_capture_json "$out" timeout --kill-after=30s "${limit}s" "$debug" --allow "$serial" tape read-rate "$barcode" \
        --config "$(fieldtest_config_path)" --library "$serial" --expected-home-slot "$home" --json \
        --from-file "$start" --files "$count" --mode "$mode" || fail "$mode-$start" 'command fails; inspect tape position and home slot before retrying' "$out"
      if python3 - "$out" "$tape" "$barcode" "$mode" "$start" "$count" <<'PY'
import json, sys, uuid
from pathlib import Path
value = json.loads(Path(sys.argv[1]).read_text())
assert value["schema"] == "rem.tape.read-rate.v1"
assert uuid.UUID(value["tape_uuid"]) == uuid.UUID(sys.argv[2])
assert value["barcode"] == sys.argv[3] and value["mode"] == sys.argv[4]
assert value["from_file"] == int(sys.argv[5]) and value["files_requested"] == int(sys.argv[6])
assert value["ok"] and value["outcome"] == "complete"
assert len(value["files"]) == int(sys.argv[6])
PY
      then
        if [[ "$partial" == true ]]; then
          fieldtest_evidence_record "$SCRIPT_NAME" "$mode-$start" INFO "partial measurement: tape too short for requested windows; barcode=$barcode files=$count from_file=$start" "$out"
        else
          fieldtest_evidence_record "$SCRIPT_NAME" "$mode-$start" PASS "barcode=$barcode files=$count from_file=$start" "$out"
        fi
      else
        fieldtest_evidence_record "$SCRIPT_NAME" "$mode-$start" FAIL 'measurement is incomplete or identity/range does not match; inspect JSON outcome' "$out"
        rc=1
      fi
    done
  done
  return "$rc"
}

fieldtest_run_with_lock main "$@"
