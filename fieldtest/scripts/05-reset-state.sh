#!/usr/bin/env bash
# Reset service authority through rem before the session-epoch plan opens state.
set -euo pipefail
SCRIPT_NAME="05-reset-state"
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

main() {
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
  "$(fieldtest_rem_bin)" reset-catalog --config "$(fieldtest_config_path)" \
    --i-understand-this-erases-the-catalog >"$output" 2>&1 || rc=$?
  if (( rc != 0 )); then
    fieldtest_evidence_record "$SCRIPT_NAME" reset FAIL "reset failed (exit $rc): $(tail -n 1 "$output")" "$output"
    return "$rc"
  fi
  cp -- "$(fieldtest_state_dir)/authority-manifest.json" "$(fieldtest_evidence_dir)/authority-manifest.json"
  if [[ -f "$(fieldtest_state_dir)/authority-reset-receipt.json" ]]; then
    cp -- "$(fieldtest_state_dir)/authority-reset-receipt.json" "$(fieldtest_evidence_dir)/authority-reset-receipt.json"
  fi
  find -H "$(fieldtest_spool_dir)" -mindepth 1 -maxdepth 1 -exec rm -rf -- {} +
  fieldtest_evidence_record "$SCRIPT_NAME" reset PASS "catalog reset for $serial; authority evidence copied and spool emptied" "$output"
}

main "$@"
