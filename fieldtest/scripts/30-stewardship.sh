#!/usr/bin/env bash
set -euo pipefail

SCRIPT_NAME="30-stewardship"
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

usage() {
  cat <<'EOF'
Usage: 30-stewardship.sh [--tapealert-probe] [--help]

Captures drive inventory, drive health snapshots, and LOG SENSE / alert evidence.
EOF
}

main() {
  local tapealert_probe=0
  case "${1:-}" in
    --help|-h)
      usage
      exit 0
      ;;
    --tapealert-probe)
      tapealert_probe=1
      shift
      ;;
  esac

  fieldtest_init_layout
  fieldtest_detect_env || true
  local serial
  serial="$(fieldtest_selected_library_serial)"
  if [[ -z "$serial" ]]; then
    echo "error: no selected library; run bringup first" >&2
    exit 1
  fi

  local endpoint stamp drive_list_json
  endpoint="$(fieldtest_rem_endpoint)"
  stamp="$(fieldtest_timestamp_id)"
  drive_list_json="$(fieldtest_artifact_path "$SCRIPT_NAME" drive-list "$stamp")"
  if ! fieldtest_capture_json "$drive_list_json" "$(fieldtest_rem_bin)" drive --endpoint "$endpoint" list --foreign --retired --json; then
    fieldtest_evidence_record "$SCRIPT_NAME" drive-list FAIL "drive list failed" "$drive_list_json"
    exit 1
  fi

  python3 - "$drive_list_json" <<'PY'
import json
import sys
from pathlib import Path

payload = json.loads(Path(sys.argv[1]).read_text())
drives = payload.get("data", {}).get("drives", [])
for drive in drives:
    print(drive.get("serial", ""))
PY

  local drive_show_file drive_poll_file drive_hist_file drive_alert_file
  while IFS= read -r drive_serial; do
    [[ -n "$drive_serial" ]] || continue
    drive_show_file="$(fieldtest_artifact_path "$SCRIPT_NAME" "show-$drive_serial" "$stamp")"
    drive_poll_file="$(fieldtest_artifact_path "$SCRIPT_NAME" "poll-$drive_serial" "$stamp")"
    drive_hist_file="$(fieldtest_artifact_path "$SCRIPT_NAME" "history-$drive_serial" "$stamp")"
    drive_alert_file="$(fieldtest_artifact_path "$SCRIPT_NAME" "alerts-$drive_serial" "$stamp")"
    fieldtest_capture_json "$drive_show_file" "$(fieldtest_rem_bin)" drive --endpoint "$endpoint" show "$drive_serial" --json
    fieldtest_capture_json "$drive_poll_file" "$(fieldtest_rem_bin)" drive --endpoint "$endpoint" poll "$drive_serial" --json
    fieldtest_capture_json "$drive_hist_file" "$(fieldtest_rem_bin)" drive --endpoint "$endpoint" history "$drive_serial" --events --snapshots --json
    fieldtest_capture_json "$drive_alert_file" "$(fieldtest_rem_bin)" drive --endpoint "$endpoint" alerts "$drive_serial" --json || true
    cp -f -- "$drive_show_file" "$(fieldtest_latest_artifact_path "$SCRIPT_NAME" "show-$drive_serial")"
    cp -f -- "$drive_poll_file" "$(fieldtest_latest_artifact_path "$SCRIPT_NAME" "poll-$drive_serial")"
    cp -f -- "$drive_hist_file" "$(fieldtest_latest_artifact_path "$SCRIPT_NAME" "history-$drive_serial")"
    cp -f -- "$drive_alert_file" "$(fieldtest_latest_artifact_path "$SCRIPT_NAME" "alerts-$drive_serial")"
    fieldtest_evidence_record "$SCRIPT_NAME" "drive-$drive_serial" PASS "captured show, poll, history, and alerts snapshots" "$drive_show_file"

    if [[ $tapealert_probe -eq 1 ]]; then
      if [[ "${REMFIELD_ENV:-unknown}" == vtl ]]; then
        fieldtest_evidence_record "$SCRIPT_NAME" "tapealert-$drive_serial" SKIP "TapeAlert clear-on-read probe is real-iron only on this VTL"
        continue
      fi
      local element_addr first second first_flags second_flags
      element_addr="$(python3 - "$drive_show_file" <<'PY'
import json,sys
from pathlib import Path
payload=json.loads(Path(sys.argv[1]).read_text())
print(payload["data"]["last_element_address"])
PY
)"
      first="$(fieldtest_artifact_path "$SCRIPT_NAME" "tapealert-$drive_serial-first" "$stamp")"
      second="$(fieldtest_artifact_path "$SCRIPT_NAME" "tapealert-$drive_serial-second" "$stamp")"
      if ! fieldtest_capture_json "$first" "$(fieldtest_rem_bin)" tape alerts --bay "$element_addr" --config "$(fieldtest_config_path)" --library "$serial"; then
        fieldtest_evidence_record "$SCRIPT_NAME" "tapealert-$drive_serial" FAIL "first tape alert read failed" "$first"
        exit 1
      fi
      if ! fieldtest_capture_json "$second" "$(fieldtest_rem_bin)" tape alerts --bay "$element_addr" --config "$(fieldtest_config_path)" --library "$serial"; then
        fieldtest_evidence_record "$SCRIPT_NAME" "tapealert-$drive_serial" FAIL "second tape alert read failed" "$second"
        exit 1
      fi
      first_flags="$(python3 - "$first" <<'PY'
import json,sys
from pathlib import Path
payload=json.loads(Path(sys.argv[1]).read_text())
print(",".join(flag["name"] for flag in payload.get("data", {}).get("active", [])))
PY
)"
      second_flags="$(python3 - "$second" <<'PY'
import json,sys
from pathlib import Path
payload=json.loads(Path(sys.argv[1]).read_text())
print(",".join(flag["name"] for flag in payload.get("data", {}).get("active", [])))
PY
)"
      if [[ "$first_flags" != "$second_flags" ]]; then
        fieldtest_evidence_record "$SCRIPT_NAME" "tapealert-$drive_serial" PASS "TapeAlert clear-on-read behavior changed active flags from '$first_flags' to '$second_flags'" "$second"
      else
        fieldtest_evidence_record "$SCRIPT_NAME" "tapealert-$drive_serial" INFO "TapeAlert flags were stable across two reads: $first_flags" "$second"
      fi
    fi
  done < <(python3 - "$drive_list_json" <<'PY'
import json,sys
from pathlib import Path
payload=json.loads(Path(sys.argv[1]).read_text())
for drive in payload.get("data", {}).get("drives", []):
    print(drive.get("serial",""))
PY
  )
}

fieldtest_run_with_lock main "$@"
