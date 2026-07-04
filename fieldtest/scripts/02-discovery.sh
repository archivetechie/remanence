#!/usr/bin/env bash
set -euo pipefail

SCRIPT_NAME="02-discovery"
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

usage() {
  cat <<'EOF'
Usage: 02-discovery.sh [--help]

Captures library, slot, and daemon drive inventory and checks allowlisted media visibility.
EOF
}

main() {
  if [[ "${1:-}" == --help || "${1:-}" == -h ]]; then
    usage
    exit 0
  fi

  fieldtest_init_layout
  fieldtest_detect_env || true

  local stamp libs_json slots_text drive_list_json selected_serial selected_count drive_count
  stamp="$(fieldtest_timestamp_id)"
  libs_json="$(fieldtest_artifact_path "$SCRIPT_NAME" libraries "$stamp")"
  fieldtest_capture_json "$libs_json" "$(fieldtest_rem_bin)" libraries --json

  selected_serial="$(fieldtest_selected_library_serial || true)"
  if [[ -z "$selected_serial" ]]; then
    selected_serial="$(python3 - "$libs_json" <<'PY'
import json,sys
from pathlib import Path
payload=json.loads(Path(sys.argv[1]).read_text())
print(payload["libraries"][0]["serial"])
PY
)"
  fi

  slots_text="$(fieldtest_artifact_path "$SCRIPT_NAME" library-slots "$stamp")"
  # Daemon-truth inventory: rem top --once --json (GetLiveStatus) carries the
  # slot map + drive-loaded voltags for every library. Direct-SCSI reads are
  # unreliable while the daemon owns the devices.
  fieldtest_capture_json "$slots_text" "$(fieldtest_rem_bin)" top --endpoint "unix:$(fieldtest_socket_path)" --once --json

  drive_list_json="$(fieldtest_artifact_path "$SCRIPT_NAME" drive-list "$stamp")"
  fieldtest_capture_json "$drive_list_json" "$(fieldtest_rem_bin)" drive --endpoint "unix:$(fieldtest_socket_path)" list --foreign --retired --json

  drive_count="$(python3 - "$drive_list_json" <<'PY'
import json,sys
from pathlib import Path
payload=json.loads(Path(sys.argv[1]).read_text())
print(len(payload["data"]["drives"]))
PY
)"
  selected_count="$(python3 - "$libs_json" "$selected_serial" <<'PY'
import json,sys
from pathlib import Path
payload=json.loads(Path(sys.argv[1]).read_text())
serial=sys.argv[2]
for lib in payload["libraries"]:
    if lib["serial"] == serial:
        print(lib["drive_count"])
        break
else:
    raise SystemExit("selected library not found")
PY
)"

  if [[ "$drive_count" -eq 0 ]]; then
    fieldtest_evidence_record "$SCRIPT_NAME" drive-list FAIL "daemon drive list is empty" "$drive_list_json"
    exit 1
  fi

  if [[ "${REMFIELD_ENV:-unknown}" == vtl ]]; then
    if [[ "$drive_count" -lt 1 ]]; then
      fieldtest_evidence_record "$SCRIPT_NAME" drive-count FAIL "vtl environment needs at least one drive"
      exit 1
    fi
  else
    if [[ "$selected_count" -lt 2 ]]; then
      fieldtest_evidence_record "$SCRIPT_NAME" drive-count FAIL "real environment should expose at least two drives"
      exit 1
    fi
  fi

  local allow_ok=0
  if [[ -f "$(fieldtest_allowlist_path)" ]]; then
    allow_ok=1
    while IFS= read -r barcode; do
      [[ -n "$barcode" ]] || continue
      if ! grep -Fq "$barcode" "$slots_text"; then
        fieldtest_evidence_record "$SCRIPT_NAME" allowlist FAIL "allowlisted barcode $barcode not visible in slot inventory" "$slots_text"
        exit 1
      fi
    done < <(fieldtest_allowlist_barcodes)
  fi

  fieldtest_evidence_record "$SCRIPT_NAME" libraries PASS "selected library $selected_serial discovered; daemon drive count=$drive_count" "$libs_json"
  fieldtest_evidence_record "$SCRIPT_NAME" library-slots PASS "slot inventory captured for $selected_serial" "$slots_text"
  fieldtest_evidence_record "$SCRIPT_NAME" drive-list PASS "daemon drive list captured" "$drive_list_json"
  if [[ $allow_ok -eq 1 ]]; then
    fieldtest_evidence_record "$SCRIPT_NAME" allowlist PASS "every allowlisted barcode was visible in slot inventory" "$slots_text"
  fi
}

fieldtest_run_with_lock main "$@"
