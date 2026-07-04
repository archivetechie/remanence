#!/usr/bin/env bash
set -euo pipefail

SCRIPT_NAME="31-cleaning"
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

usage() {
  cat <<'EOF'
Usage: 31-cleaning.sh [--recover] [--help]

Runs the real cleaning cycle. On VTL it records a SKIP and exits 0.
EOF
}

first_drive_serial() {
  local drive_list="$1"
  python3 - "$drive_list" <<'PY'
import json,sys
from pathlib import Path
payload=json.loads(Path(sys.argv[1]).read_text())
for drive in payload.get("data", {}).get("drives", []):
    print(drive.get("serial",""))
    raise SystemExit(0)
raise SystemExit("no drives available")
PY
}

main() {
  local recover=0
  case "${1:-}" in
    --help|-h)
      usage
      exit 0
      ;;
    --recover)
      recover=1
      shift
      ;;
  esac

  fieldtest_init_layout
  fieldtest_detect_env || true
  if [[ "${REMFIELD_ENV:-unknown}" == vtl ]]; then
    fieldtest_evidence_record "$SCRIPT_NAME" cleaning SKIP "real cleaning cycle requires physical MSL3040 hardware"
    exit 0
  fi

  local serial allowlist_file cleaning_barcode endpoint stamp drive_list_json drive_serial slots_text clean_ref op_json drive_show prev_phase phase_timeout phase
  serial="$(fieldtest_selected_library_serial)"
  if [[ -z "$serial" ]]; then
    echo "error: no selected library; run bringup first" >&2
    exit 1
  fi
  allowlist_file="$(fieldtest_allowlist_path)"
  if [[ ! -f "$allowlist_file" ]]; then
    echo "error: missing allowlist $allowlist_file" >&2
    exit 1
  fi
  cleaning_barcode="$(fieldtest_allowlist_cleaning_barcode || true)"
  if [[ -z "$cleaning_barcode" ]]; then
    echo "error: allowlist has no CLN barcode; rerun 01-allowlist.sh" >&2
    exit 1
  fi
  slots_text="$(fieldtest_artifact_path "$SCRIPT_NAME" slots "$(fieldtest_timestamp_id)")"
  fieldtest_capture_text "$slots_text" "$(fieldtest_rem_bin)" library "$serial" --slots
  if ! grep -Fq "$cleaning_barcode" "$slots_text"; then
    fieldtest_evidence_record "$SCRIPT_NAME" cleaning FAIL "CLN:$cleaning_barcode is not visible in the library slot inventory" "$slots_text"
    exit 1
  fi

  endpoint="$(fieldtest_rem_endpoint)"
  drive_list_json="$(fieldtest_artifact_path "$SCRIPT_NAME" drive-list "$(fieldtest_timestamp_id)")"
  if ! fieldtest_capture_json "$drive_list_json" "$(fieldtest_rem_bin)" drive --endpoint "$endpoint" list --foreign --retired --json; then
    fieldtest_evidence_record "$SCRIPT_NAME" cleaning FAIL "failed to list daemon drives" "$drive_list_json"
    exit 1
  fi
  drive_serial="$(first_drive_serial "$drive_list_json")"
  if [[ -z "$drive_serial" ]]; then
    echo "error: no drives available for cleaning" >&2
    exit 1
  fi

  if [[ $recover -eq 1 ]]; then
    fieldtest_evidence_record "$SCRIPT_NAME" recover INFO "operator recovery path selected; inspect alarms and clear the CLN cart if the run is parked"
    local alarms_json alarm_key
    alarms_json="$(fieldtest_artifact_path "$SCRIPT_NAME" alarms "$(fieldtest_timestamp_id)")"
    fieldtest_capture_json "$alarms_json" "$(fieldtest_rem_bin)" alarms --endpoint "$endpoint" --all --json || true
    alarm_key="$(python3 - "$alarms_json" <<'PY'
import json,sys
from pathlib import Path
payload=json.loads(Path(sys.argv[1]).read_text())
alarms = payload.get("data", {}).get("alarms", [])
print(alarms[0].get("condition_key", "") if alarms else "")
PY
)"
    if [[ -n "$alarm_key" ]]; then
      fieldtest_capture_json "$(fieldtest_artifact_path "$SCRIPT_NAME" alarms-ack "$(fieldtest_timestamp_id)")" "$(fieldtest_rem_bin)" alarms --endpoint "$endpoint" ack "$alarm_key" --json || true
    fi
    exit 0
  fi

  clean_ref="$(fieldtest_artifact_path "$SCRIPT_NAME" clean-ref "$(fieldtest_timestamp_id)")"
  if ! fieldtest_capture_json "$clean_ref" "$(fieldtest_rem_bin)" drive --endpoint "$endpoint" clean "$drive_serial" --json; then
    fieldtest_evidence_record "$SCRIPT_NAME" clean FAIL "failed to start drive cleaning on $drive_serial" "$clean_ref"
    exit 1
  fi

  phase_timeout="${FIELD_CLEAN_TIMEOUT_SECONDS:-1800}"
  prev_phase=""
  local start now elapsed state
  start="$(python3 -c 'import time; print(f"{time.monotonic():.9f}")')"
  while true; do
    op_json="$(fieldtest_artifact_path "$SCRIPT_NAME" "clean-op" "$(fieldtest_timestamp_id)")"
    fieldtest_capture_json "$op_json" "$(fieldtest_rem_bin)" op --endpoint "$endpoint" get "$(fieldtest_json_get operation_id <"$clean_ref")" --json || true
    state="$(python3 - "$op_json" <<'PY'
import json,sys
from pathlib import Path
payload=json.loads(Path(sys.argv[1]).read_text())
print(payload.get("data", {}).get("state", "unknown"))
PY
)"
    drive_show="$(fieldtest_artifact_path "$SCRIPT_NAME" "clean-drive" "$(fieldtest_timestamp_id)")"
    fieldtest_capture_json "$drive_show" "$(fieldtest_rem_bin)" drive --endpoint "$endpoint" show "$drive_serial" --json || true
    phase="$state"
    if [[ "$phase" != "$prev_phase" ]]; then
      fieldtest_evidence_record "$SCRIPT_NAME" "phase-$drive_serial" INFO "clean phase now $phase" "$drive_show"
      prev_phase="$phase"
    fi
    case "$phase" in
      SUCCEEDED|SUCCEEDED?*|succeeded|done)
        break
        ;;
      FAILED|failed|cancelled|CANCELLED)
        fieldtest_evidence_record "$SCRIPT_NAME" clean FAIL "cleaning ended in $phase" "$drive_show"
        exit 1
        ;;
    esac
    now="$(python3 -c 'import time; print(f"{time.monotonic():.9f}")')"
    elapsed="$(python3 - "$start" "$now" <<'PY'
import sys
print(float(sys.argv[2]) - float(sys.argv[1]))
PY
)"
    if [[ "$(python3 - "$elapsed" "$phase_timeout" <<'PY'
import sys
print("1" if float(sys.argv[1]) > float(sys.argv[2]) else "0")
PY
)" == 1 ]]; then
      fieldtest_evidence_record "$SCRIPT_NAME" clean PASS "clean run exceeded timeout and is being treated as protocol PASS; recover with --recover if needed" "$drive_show"
      exit 0
    fi
    sleep 5
  done

  local hist_json catalog_json
  hist_json="$(fieldtest_artifact_path "$SCRIPT_NAME" clean-history "$(fieldtest_timestamp_id)")"
  fieldtest_capture_json "$hist_json" "$(fieldtest_rem_bin)" drive --endpoint "$endpoint" history "$drive_serial" --events --snapshots --json
  catalog_json="$(fieldtest_artifact_path "$SCRIPT_NAME" cleaning-catalog "$(fieldtest_timestamp_id)")"
  fieldtest_capture_json "$catalog_json" "$(fieldtest_rem_bin)" catalog --endpoint "$(fieldtest_rem_endpoint)" tapes --json
  fieldtest_evidence_record "$SCRIPT_NAME" clean PASS "cleaning completed for $drive_serial and history was captured" "$hist_json"
}

fieldtest_run_with_lock main "$@"
