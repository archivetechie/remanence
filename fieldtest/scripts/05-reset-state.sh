#!/usr/bin/env bash
# Reset service authority through rem before the session-epoch plan opens state.
set -euo pipefail
SCRIPT_NAME="05-reset-state"
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

main() {
  set -euo pipefail
  fieldtest_init_layout
  if fieldtest_daemon_running >/dev/null; then
    fieldtest_evidence_record "$SCRIPT_NAME" live-daemon FAIL "refusing reset while this kit's daemon is running; run 03-bringup.sh --stop first"
    echo 'refusing reset: run 03-bringup.sh --stop first' >&2
    return 1
  fi

  local serial output rc=0
  serial="${FIELDTEST_LIBRARY_SERIAL:-}"
  if [[ -z "$serial" ]]; then
    serial="$(fieldtest_selected_library_serial || true)"
  fi
  if [[ -z "$serial" ]]; then
    local message="no selected library: set FIELDTEST_LIBRARY_SERIAL or provide $(fieldtest_state_dir)/selected-library"
    fieldtest_evidence_record "$SCRIPT_NAME" selected-library FAIL "$message"
    printf '%s\n' "$message" >&2
    return 1
  fi
  fieldtest_write_selected_library_serial "$serial"
  fieldtest_write_config "$(fieldtest_config_path)" "$serial"
  output="$(fieldtest_artifact_path "$SCRIPT_NAME" reset "$(fieldtest_timestamp_id)")"
  mkdir -p -- "$(dirname -- "$output")"
  fieldtest_capture_text "$output" "$(fieldtest_rem_bin)" reset-catalog --config "$(fieldtest_config_path)" \
    --i-understand-this-erases-the-catalog || rc=$?
  if (( rc != 0 )); then
    local last_line
    last_line="$(python3 - "$output" <<'PY'
import json
import sys
from pathlib import Path

payload = json.loads(Path(sys.argv[1]).read_text())
lines = [line for line in payload['stderr'].splitlines() if line.strip()]
if not lines:
    lines = [line for line in payload['stdout'].splitlines() if line.strip()]
print(lines[-1] if lines else '')
PY
)"
    fieldtest_evidence_record "$SCRIPT_NAME" reset FAIL "reset failed (exit $rc): $last_line" "$output"
    return "$rc"
  fi
  cp -- "$(fieldtest_state_dir)/authority-manifest.json" "$(fieldtest_evidence_dir)/authority-manifest.json" || {
    fieldtest_evidence_record "$SCRIPT_NAME" manifest-copy FAIL "authority-manifest.json copy failed" "$output"
    return 1
  }
  if [[ -f "$(fieldtest_state_dir)/authority-reset-receipt.json" ]]; then
    cp -- "$(fieldtest_state_dir)/authority-reset-receipt.json" "$(fieldtest_evidence_dir)/authority-reset-receipt.json" || {
      fieldtest_evidence_record "$SCRIPT_NAME" receipt-copy FAIL "authority-reset-receipt.json copy failed" "$output"
      return 1
    }
  fi
  local spool home spool_action="left as is"
  if ! spool="$(readlink -f -- "$(fieldtest_spool_dir)")" || [[ -z "$spool" ]] ||
     ! home="$(readlink -f -- "$(fieldtest_home)")" || [[ -z "$home" ]]; then
    fieldtest_evidence_record "$SCRIPT_NAME" spool FAIL "spool resolution failed for $(fieldtest_spool_dir)" "$output"
    return 1
  fi
  if [[ -d "$spool" && "$spool" == "$home/"* ]]; then
    # The root is resolved; find and rm do not follow links within the spool.
    find "$spool" -mindepth 1 -maxdepth 1 -exec rm -rf -- {} + || {
      fieldtest_evidence_record "$SCRIPT_NAME" spool FAIL "spool wipe failed for $(fieldtest_spool_dir) ($spool)" "$output"
      return 1
    }
    spool_action="emptied"
  else
    fieldtest_evidence_record "$SCRIPT_NAME" spool WARN "spool $spool lies outside the kit home and was left as is"
  fi
  fieldtest_evidence_record "$SCRIPT_NAME" reset PASS "catalog reset for $serial; authority evidence copied and spool $spool_action" "$output"
}

fieldtest_run_with_lock main "$@"
