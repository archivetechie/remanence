#!/usr/bin/env bash
set -euo pipefail

SCRIPT_NAME="09-media-ready"
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

usage() {
  cat <<'EOF'
Usage:
09-media-ready.sh --count N [--condition-all] [--timeout 2.5h] [--poll 30s] [--no-wait]
09-media-ready.sh --resume UUID [--timeout 2.5h] [--poll 30s] [--no-wait]
09-media-ready.sh --barcode BARCODE [--timeout 2.5h] [--poll 30s] [--no-wait]
09-media-ready.sh --drive-element 0xNNNN [--timeout 2.5h] [--poll 30s] [--no-wait]

Polls TEST UNIT READY for allowlisted scratch media already loaded in a drive.
The script never moves cartridges. Slot-only tapes are recorded as SKIP/not_loaded
evidence and are left for the later tape-init phase.
EOF
}

selected_library_or_env() {
  if [[ -n "${FIELDTEST_LIBRARY_SERIAL:-}" ]]; then
    printf '%s\n' "$FIELDTEST_LIBRARY_SERIAL"
    return 0
  fi
  fieldtest_selected_library_serial
}

media_readiness_ledger_path() {
  printf '%s\n' "$(fieldtest_state_dir)/media-readiness.jsonl"
}

safe_artifact_token() {
  python3 - "$1" <<'PY'
import re
import sys

token = re.sub(r"[^A-Za-z0-9_.:-]+", "_", sys.argv[1].strip())
print(token or "target")
PY
}

readiness_result_extra_json() {
  local path="$1" code="$2"
  python3 - "$path" "$code" <<'PY'
import json
import re
import sys
from pathlib import Path

path = Path(sys.argv[1])
code = int(sys.argv[2])
payload = json.loads(path.read_text())
stdout = payload.get("stdout") or ""
stderr = payload.get("stderr") or ""
text = f"{stdout}\n{stderr}"
stdout_json = {}
try:
    parsed = json.loads(stdout)
    if isinstance(parsed, dict):
        stdout_json = parsed
except json.JSONDecodeError:
    pass
state = stdout_json.get("state")
if not state:
    match = re.search(r"\bmedia_readiness_state=([A-Za-z0-9_]+)\b", text)
    if match:
        state = match.group(1)
if not state:
    state = {
        0: "ready",
        10: "media_initializing",
        20: "timeout_unknown",
        30: "terminal_error",
        40: "transport_unknown",
        50: "ownership_refused",
        130: "aborted_unknown",
    }.get(code, "terminal_error")
extra = {
    "media_readiness_state": state,
    "rem_exit_code": code,
}
for key in (
    "operation_id",
    "library_serial",
    "drive_element",
    "barcode",
    "ready",
    "retryable",
    "timed_out",
    "attempts",
    "summary",
):
    if key in stdout_json:
        extra[key] = stdout_json[key]
print(json.dumps(extra, separators=(",", ":")))
PY
}

append_readiness_ledger() {
  local serial="$1" target_kind="$2" target="$3" detail_path="$4" code="$5" state_override="${6:-}"
  local ledger
  ledger="$(media_readiness_ledger_path)"
  mkdir -p "$(dirname -- "$ledger")"
  python3 - "$ledger" "$(fieldtest_now_utc)" "$serial" "$target_kind" "$target" "$detail_path" "$code" "$state_override" <<'PY'
import json
import re
import sys
from pathlib import Path

ledger = Path(sys.argv[1])
ts, serial, target_kind, target, detail_path, code_text, state_override = sys.argv[2:]
code = int(code_text)
record = {
    "ts": ts,
    "script": "09-media-ready",
    "library_serial": serial,
    "target_kind": target_kind,
    "target": target,
    "detail_path": detail_path or None,
    "rem_exit_code": code,
}
if detail_path:
    payload = json.loads(Path(detail_path).read_text())
    stdout = payload.get("stdout") or ""
    stderr = payload.get("stderr") or ""
    stdout_json = {}
    try:
        parsed = json.loads(stdout)
        if isinstance(parsed, dict):
            stdout_json = parsed
    except json.JSONDecodeError:
        pass
    record.update({k: v for k, v in stdout_json.items() if k in {
        "operation_id",
        "drive_element",
        "barcode",
        "state",
        "ready",
        "retryable",
        "timed_out",
        "attempts",
        "summary",
        "exit_code",
    }})
    if "state" not in record:
        match = re.search(r"\bmedia_readiness_state=([A-Za-z0-9_]+)\b", f"{stdout}\n{stderr}")
        if match:
            record["state"] = match.group(1)
if state_override:
    record["state"] = state_override
if "state" not in record:
    record["state"] = {
        0: "ready",
        10: "media_initializing",
        20: "timeout_unknown",
        30: "terminal_error",
        40: "transport_unknown",
        50: "ownership_refused",
        130: "aborted_unknown",
    }.get(code, "terminal_error")
if "ready" not in record:
    record["ready"] = record["state"] == "ready"
ledger.open("a").write(json.dumps(record, separators=(",", ":")) + "\n")
PY
}

library_inventory_path() {
  local serial="$1" stamp="$2" out
  out="$(fieldtest_artifact_path "$SCRIPT_NAME" "library-${serial}" "$stamp")"
  fieldtest_capture_json "$out" "$(fieldtest_rem_bin)" library "$serial" --json --slots
  printf '%s\n' "$out"
}

inventory_barcode_location() {
  local inventory="$1" barcode="$2"
  python3 - "$inventory" "$barcode" <<'PY'
import json
import sys
from pathlib import Path

payload = json.loads(Path(sys.argv[1]).read_text())
barcode = sys.argv[2]
for drive in payload.get("drives", []):
    if drive.get("loaded") and drive.get("loaded_tape") == barcode:
        print(f"drive\t{drive.get('element_address')}\t{barcode}")
        raise SystemExit(0)
for slot in payload.get("slots", []):
    if slot.get("full") and slot.get("cartridge") == barcode:
        print(f"slot\t{slot.get('element_address')}\t{barcode}")
        raise SystemExit(0)
print(f"missing\t\t{barcode}")
PY
}

inventory_drive_barcode() {
  local inventory="$1" drive_element="$2"
  python3 - "$inventory" "$drive_element" <<'PY'
import json
import sys
from pathlib import Path

payload = json.loads(Path(sys.argv[1]).read_text())
target = int(sys.argv[2], 0)
for drive in payload.get("drives", []):
    raw = drive.get("element_address_raw")
    if raw is None:
        try:
            raw = int(str(drive.get("element_address")), 0)
        except ValueError:
            continue
    if int(raw) == target:
        if not drive.get("loaded"):
            raise SystemExit(f"drive 0x{target:04x} is empty")
        barcode = drive.get("loaded_tape")
        if not barcode:
            raise SystemExit(f"drive 0x{target:04x} is loaded but barcode is unreadable")
        print(barcode)
        raise SystemExit(0)
raise SystemExit(f"drive 0x{target:04x} is not in the selected library inventory")
PY
}

selected_batch_targets() {
  local inventory="$1" count="$2" condition_all="$3"
  python3 - "$inventory" "$(fieldtest_allowlist_path)" "$count" "$condition_all" <<'PY'
import json
import sys
from pathlib import Path

inventory = json.loads(Path(sys.argv[1]).read_text())
allowlist = Path(sys.argv[2])
count = int(sys.argv[3])
condition_all = sys.argv[4] == "1"
allowed = []
for raw in allowlist.read_text().splitlines():
    line = raw.split("#", 1)[0].strip()
    if not line or line.startswith("CLN:"):
        continue
    allowed.append(line)
locations = {}
for drive in inventory.get("drives", []):
    barcode = drive.get("loaded_tape") if drive.get("loaded") else None
    if barcode:
        locations[barcode] = ("drive", drive.get("element_address"))
for slot in inventory.get("slots", []):
    barcode = slot.get("cartridge") if slot.get("full") else None
    if barcode and barcode not in locations:
        locations[barcode] = ("slot", slot.get("element_address"))
visible = [(barcode, *locations[barcode]) for barcode in allowed if barcode in locations]
if condition_all:
    selected = visible
else:
    selected = visible[:count]
    if len(selected) < count:
        found = ", ".join(barcode for barcode, _, _ in visible) or "(none)"
        raise SystemExit(f"need {count} visible allowlisted data barcode(s) in selected library; found {len(selected)}: {found}")
for barcode, kind, element in selected:
    print(f"{barcode}\t{kind}\t{element}")
PY
}

ensure_config() {
  local serial="$1"
  if [[ -f "$(fieldtest_config_path)" ]]; then
    return 0
  fi
  fieldtest_write_config "$(fieldtest_config_path)" "$serial"
  echo "generated $(fieldtest_config_path) for media-readiness checks"
}

record_slot_skip() {
  local serial="$1" barcode="$2" element="$3"
  local extra
  extra="$(
    python3 - "$serial" "$barcode" "$element" <<'PY'
import json
import sys

serial, barcode, element = sys.argv[1:]
print(json.dumps({
    "media_readiness_state": "not_loaded",
    "library_serial": serial,
    "barcode": barcode,
    "slot_element": element,
}, separators=(",", ":")))
PY
  )"
  fieldtest_evidence_record "$SCRIPT_NAME" "wait-ready-${barcode}" SKIP "allowlisted ${barcode} is in slot ${element}; 09-media-ready does not move cartridges" "" "$extra"
  append_readiness_ledger "$serial" "barcode" "$barcode" "" 0 "not_loaded"
}

record_missing_target() {
  local serial="$1" target="$2"
  local extra
  extra="$(
    python3 - "$serial" "$target" <<'PY'
import json
import sys

serial, target = sys.argv[1:]
print(json.dumps({
    "media_readiness_state": "not_visible",
    "library_serial": serial,
    "barcode": target,
}, separators=(",", ":")))
PY
  )"
  fieldtest_evidence_record "$SCRIPT_NAME" "wait-ready-${target}" FAIL "allowlisted ${target} is not visible in selected library ${serial}" "" "$extra"
}

run_wait_ready() {
  local serial="$1" target_kind="$2" target="$3" timeout="$4" poll="$5" wait="$6" stamp="$7"
  local token out rc extra
  local -a cmd wait_args
  token="$(safe_artifact_token "$target")"
  out="$(fieldtest_artifact_path "$SCRIPT_NAME" "wait-ready-${token}" "$stamp")"
  mkdir -p "$(dirname -- "$out")"
  wait_args=()
  if [[ "$wait" == 1 ]]; then
    wait_args+=(--wait)
  fi
  cmd=("$(fieldtest_rem_bin)" tape wait-ready --config "$(fieldtest_config_path)" --library "$serial")
  case "$target_kind" in
    resume)
      cmd+=(--resume "$target")
      ;;
    barcode)
      fieldtest_require_allowlisted "$target"
      cmd+=(--barcode "$target")
      ;;
    drive)
      cmd+=(--drive-element "$target" --already-loaded)
      ;;
    *)
      echo "internal error: unknown wait-ready target kind $target_kind" >&2
      exit 1
      ;;
  esac
  cmd+=("${wait_args[@]}" --timeout "$timeout" --poll "$poll" --json)
  set +e
  fieldtest_capture_text "$out" "${cmd[@]}"
  rc=$?
  set -e
  extra="$(readiness_result_extra_json "$out" "$rc")"
  append_readiness_ledger "$serial" "$target_kind" "$target" "$out" "$rc"
  case "$rc" in
    0)
      fieldtest_evidence_record "$SCRIPT_NAME" "wait-ready-${token}" PASS "media ready for $target" "$out" "$extra"
      return 0
      ;;
    10)
      fieldtest_evidence_record "$SCRIPT_NAME" "wait-ready-${token}" INFO "media initializing for $target; do not move/unload/retry; leave the cartridge in the drive and resume wait-ready later" "$out" "$extra"
      echo "media initializing for $target: do not move/unload/retry; leave the cartridge in the drive" >&2
      return 10
      ;;
    20)
      fieldtest_evidence_record "$SCRIPT_NAME" "wait-ready-${token}" FAIL "timeout_unknown while waiting for $target; stop and collect RCA evidence before retrying" "$out" "$extra"
      return 20
      ;;
    30)
      fieldtest_evidence_record "$SCRIPT_NAME" "wait-ready-${token}" FAIL "terminal readiness state for $target; stop and inspect quarantine/RCA evidence" "$out" "$extra"
      return 30
      ;;
    40)
      fieldtest_evidence_record "$SCRIPT_NAME" "wait-ready-${token}" FAIL "transport_unknown while waiting for $target; stop and collect dmesg/SCSI RCA evidence before retrying" "$out" "$extra"
      return 40
      ;;
    50)
      fieldtest_evidence_record "$SCRIPT_NAME" "wait-ready-${token}" FAIL "ownership/refusal while waiting for $target; verify selected library, allowlist, loaded barcode, and other owners before retrying" "$out" "$extra"
      return 50
      ;;
    130)
      fieldtest_evidence_record "$SCRIPT_NAME" "wait-ready-${token}" FAIL "media readiness interrupted for $target; leave the tape in place and inspect the aborted_unknown fence" "$out" "$extra"
      return 130
      ;;
    *)
      fieldtest_evidence_record "$SCRIPT_NAME" "wait-ready-${token}" FAIL "media readiness failed for $target (rc=$rc); stop and inspect captured output" "$out" "$extra"
      return "$rc"
      ;;
  esac
}

media_ready_selftest() {
  local tmpdir child_rc records ledger rem_log
  tmpdir="$(mktemp -d)"
  mkdir -p "$tmpdir/home/bin" "$tmpdir/home/evidence" "$tmpdir/home/state" "$tmpdir/home/log" "$tmpdir/home/spool"
  printf '%s\n' LIBMAIN >"$tmpdir/home/state/selected-library"
  cat >"$tmpdir/home/allowlist.txt" <<'EOF'
AOX030L9
AOX031L9
AOX032L9
EOF
  cat >"$tmpdir/home/bin/rem" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
home="${REMFIELD_HOME:?}"
printf '%s\n' "$*" >>"$home/rem-invocations.log"
if [[ "${1:-}" == libraries && "${2:-}" == --json ]]; then
  cat <<'JSON'
{"libraries":[{"serial":"LIBMAIN","product":"MSL3040","revision":"3350","vendor":"HPE","drive_count":2,"slot_count":3,"loaded_slot_count":3,"ie_port_count":0}]}
JSON
  exit 0
fi
if [[ "${1:-}" == library && "${2:-}" == LIBMAIN && "$*" == *" --json "* && "$*" == *" --slots"* ]]; then
  cat <<'JSON'
{"serial":"LIBMAIN","drives":[{"element_address":"0x0001","element_address_raw":1,"loaded":true,"loaded_tape":"AOX030L9"},{"element_address":"0x0002","element_address_raw":2,"loaded":true,"loaded_tape":"AOX031L9"}],"slots":[{"element_address":"0x03eb","element_address_raw":1003,"full":false,"cartridge":null},{"element_address":"0x03ec","element_address_raw":1004,"full":false,"cartridge":null},{"element_address":"0x03ed","element_address_raw":1005,"full":true,"cartridge":"AOX032L9"}]}
JSON
  exit 0
fi
if [[ "${1:-}" == tape && "${2:-}" == wait-ready ]]; then
  if [[ " $* " == *" --barcode AOX030L9 "* ]]; then
    cat <<'JSON'
{"schema":"rem.tape.wait_ready.v1","operation_id":"00000000-0000-0000-0000-000000000030","library_serial":"LIBMAIN","drive_element":"0x0001","barcode":"AOX030L9","state":"ready","ready":true,"retryable":false,"timed_out":false,"attempts":1,"exit_code":0,"summary":"ready"}
JSON
    exit 0
  fi
  if [[ " $* " == *" --barcode AOX031L9 "* ]]; then
    cat <<'JSON'
{"schema":"rem.tape.wait_ready.v1","operation_id":"00000000-0000-0000-0000-000000000031","library_serial":"LIBMAIN","drive_element":"0x0002","barcode":"AOX031L9","state":"media_initializing","ready":false,"retryable":true,"timed_out":false,"attempts":1,"exit_code":10,"summary":"media initializing/calibrating"}
JSON
    exit 10
  fi
fi
echo "unexpected mock rem invocation: $*" >&2
exit 98
EOF
  chmod +x "$tmpdir/home/bin/rem"
  REMFIELD_HOME="$tmpdir/home" bash "$0" --count 3 --no-wait >"$tmpdir/selftest.stdout" 2>"$tmpdir/selftest.stderr" || child_rc=$?
  child_rc="${child_rc:-0}"
  records="$tmpdir/home/evidence/records.jsonl"
  ledger="$tmpdir/home/state/media-readiness.jsonl"
  rem_log="$tmpdir/home/rem-invocations.log"
  if [[ "$child_rc" -ne 10 ]]; then
    echo "selftest: expected media-initializing exit 10, got $child_rc" >&2
    cat "$tmpdir/selftest.stderr" >&2 || true
    rm -rf "$tmpdir"
    return 1
  fi
  grep -q '"status":"PASS"' "$records"
  grep -q '"media_readiness_state":"ready"' "$records"
  grep -q '"status":"INFO"' "$records"
  grep -q '"media_readiness_state":"media_initializing"' "$records"
  grep -q '"state":"ready"' "$ledger"
  grep -q '"state":"media_initializing"' "$ledger"
  if grep -Eq ' rem-debug | unload | load | move ' "$rem_log"; then
    echo "selftest: readiness sweep must not move cartridges" >&2
    rm -rf "$tmpdir"
    return 1
  fi
  grep -q 'do not move/unload/retry' "$tmpdir/selftest.stderr"
  rm -rf "$tmpdir"

  tmpdir="$(mktemp -d)"
  mkdir -p "$tmpdir/home/bin" "$tmpdir/home/evidence" "$tmpdir/home/state" "$tmpdir/home/log" "$tmpdir/home/spool"
  printf '%s\n' LIBMAIN >"$tmpdir/home/state/selected-library"
  printf '%s\n' AOX032L9 >"$tmpdir/home/allowlist.txt"
  cat >"$tmpdir/home/bin/rem" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
home="${REMFIELD_HOME:?}"
printf '%s\n' "$*" >>"$home/rem-invocations.log"
if [[ "${1:-}" == libraries && "${2:-}" == --json ]]; then
  echo '{"libraries":[{"serial":"LIBMAIN","product":"MSL3040","revision":"3350","vendor":"HPE","drive_count":1,"slot_count":1,"loaded_slot_count":1,"ie_port_count":0}]}'
  exit 0
fi
if [[ "${1:-}" == library && "${2:-}" == LIBMAIN && "$*" == *" --json "* && "$*" == *" --slots"* ]]; then
  echo '{"serial":"LIBMAIN","drives":[{"element_address":"0x0001","element_address_raw":1,"loaded":false,"loaded_tape":null}],"slots":[{"element_address":"0x03ed","element_address_raw":1005,"full":true,"cartridge":"AOX032L9"}]}'
  exit 0
fi
if [[ "${1:-}" == tape && "${2:-}" == wait-ready ]]; then
  echo "slot-only selftest must not invoke wait-ready" >&2
  exit 97
fi
echo "unexpected mock rem invocation: $*" >&2
exit 98
EOF
  chmod +x "$tmpdir/home/bin/rem"
  REMFIELD_HOME="$tmpdir/home" bash "$0" --count 1 --no-wait >/dev/null
  records="$tmpdir/home/evidence/records.jsonl"
  ledger="$tmpdir/home/state/media-readiness.jsonl"
  rem_log="$tmpdir/home/rem-invocations.log"
  grep -q '"status":"SKIP"' "$records"
  grep -q '"media_readiness_state":"not_loaded"' "$records"
  grep -q '"state":"not_loaded"' "$ledger"
  if grep -q 'tape wait-ready' "$rem_log"; then
    echo "selftest: slot-only count mode must not invoke wait-ready" >&2
    rm -rf "$tmpdir"
    return 1
  fi
  rm -rf "$tmpdir"
}

main() {
  if [[ "${1:-}" == --selftest ]]; then
    media_ready_selftest
    exit 0
  fi

  local resume="" barcode="" drive_element="" timeout="2.5h" poll="30s" wait=1 count="" condition_all=0
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --resume)
        resume="${2:?missing operation id}"
        shift 2
        ;;
      --barcode)
        barcode="${2:?missing barcode}"
        shift 2
        ;;
      --drive-element)
        drive_element="${2:?missing drive element}"
        shift 2
        ;;
      --count)
        count="${2:?missing count}"
        shift 2
        ;;
      --condition-all)
        condition_all=1
        shift
        ;;
      --timeout)
        timeout="${2:?missing timeout}"
        shift 2
        ;;
      --poll)
        poll="${2:?missing poll}"
        shift 2
        ;;
      --no-wait)
        wait=0
        shift
        ;;
      --help|-h)
        usage
        exit 0
        ;;
      *)
        echo "error: unknown argument $1" >&2
        usage >&2
        exit 2
        ;;
    esac
  done

  local explicit_targets=0 batch_mode=0
  [[ -n "$resume" ]] && explicit_targets=$((explicit_targets + 1))
  [[ -n "$barcode" ]] && explicit_targets=$((explicit_targets + 1))
  [[ -n "$drive_element" ]] && explicit_targets=$((explicit_targets + 1))
  [[ -n "$count" || "$condition_all" -eq 1 ]] && batch_mode=1
  if [[ "$explicit_targets" -gt 1 ]]; then
    echo "error: use only one of --resume, --barcode, or --drive-element" >&2
    exit 2
  fi
  if [[ "$batch_mode" -eq 1 && "$explicit_targets" -gt 0 ]]; then
    echo "error: --count/--condition-all cannot be combined with --resume, --barcode, or --drive-element" >&2
    exit 2
  fi
  if [[ "$batch_mode" -eq 0 && "$explicit_targets" -eq 0 ]]; then
    echo "error: provide --count, --resume, --barcode, or --drive-element" >&2
    exit 2
  fi
  if [[ -n "$count" && ! "$count" =~ ^[0-9]+$ ]]; then
    echo "error: --count must be a positive integer" >&2
    exit 2
  fi
  if [[ -n "$count" && "$count" -lt 1 ]]; then
    echo "error: --count must be a positive integer" >&2
    exit 2
  fi

  fieldtest_init_layout
  fieldtest_detect_env || true

  local serial stamp inventory
  serial="$(selected_library_or_env 2>/dev/null || true)"
  if [[ -z "$serial" ]]; then
    echo "error: no selected library; run 00-preflight.sh or set FIELDTEST_LIBRARY_SERIAL" >&2
    exit 1
  fi
  fieldtest_write_selected_library_serial "$serial"
  ensure_config "$serial"
  stamp="$(fieldtest_timestamp_id)"
  inventory="$(library_inventory_path "$serial" "$stamp")"

  if [[ "$batch_mode" -eq 1 ]]; then
    if [[ ! -f "$(fieldtest_allowlist_path)" ]]; then
      echo "error: missing allowlist $(fieldtest_allowlist_path); run 01-allowlist.sh first" >&2
      exit 1
    fi
    local effective_count targets any_ready_wait=0
    effective_count="${count:-0}"
    mapfile -t targets < <(selected_batch_targets "$inventory" "$effective_count" "$condition_all")
    if [[ "${#targets[@]}" -eq 0 ]]; then
      echo "error: no visible allowlisted data barcodes in selected library $serial" >&2
      exit 1
    fi
    local line target_barcode target_kind target_element rc
    for line in "${targets[@]}"; do
      IFS=$'\t' read -r target_barcode target_kind target_element <<<"$line"
      fieldtest_require_allowlisted "$target_barcode"
      case "$target_kind" in
        drive)
          run_wait_ready "$serial" barcode "$target_barcode" "$timeout" "$poll" "$wait" "$stamp" || rc=$?
          rc="${rc:-0}"
          any_ready_wait=1
          if [[ "$rc" -eq 10 ]]; then
            exit 10
          fi
          if [[ "$rc" -ne 0 ]]; then
            exit "$rc"
          fi
          ;;
        slot)
          record_slot_skip "$serial" "$target_barcode" "$target_element"
          ;;
        *)
          record_missing_target "$serial" "$target_barcode"
          exit 1
          ;;
      esac
      rc=0
    done
    if [[ "$any_ready_wait" -eq 0 ]]; then
      fieldtest_evidence_record "$SCRIPT_NAME" wait-ready-summary SKIP "no selected allowlisted tapes are currently loaded in drives; no readiness polling was needed"
    else
      fieldtest_evidence_record "$SCRIPT_NAME" wait-ready-summary PASS "processed allowlisted loaded tape readiness for selected library $serial"
    fi
    exit 0
  fi

  if [[ -n "$barcode" ]]; then
    fieldtest_require_allowlisted "$barcode"
    local location_kind location_element _location_barcode location
    location="$(inventory_barcode_location "$inventory" "$barcode")"
    IFS=$'\t' read -r location_kind location_element _location_barcode <<<"$location"
    case "$location_kind" in
      drive)
        set +e
        run_wait_ready "$serial" barcode "$barcode" "$timeout" "$poll" "$wait" "$stamp"
        rc=$?
        set -e
        exit "$rc"
        ;;
      slot)
        echo "error: allowlisted $barcode is in slot $location_element; 09-media-ready does not move cartridges" >&2
        exit 2
        ;;
      *)
        echo "error: allowlisted $barcode is not visible in selected library $serial" >&2
        exit 2
        ;;
    esac
  fi

  if [[ -n "$drive_element" ]]; then
    local drive_barcode
    if ! drive_barcode="$(inventory_drive_barcode "$inventory" "$drive_element")"; then
      echo "error: $drive_barcode" >&2
      exit 2
    fi
    fieldtest_require_allowlisted "$drive_barcode"
    set +e
    run_wait_ready "$serial" drive "$drive_element" "$timeout" "$poll" "$wait" "$stamp"
    rc=$?
    set -e
    exit "$rc"
  fi

  set +e
  run_wait_ready "$serial" resume "$resume" "$timeout" "$poll" "$wait" "$stamp"
  rc=$?
  set -e
  exit "$rc"
}

fieldtest_run_with_lock main "$@"
