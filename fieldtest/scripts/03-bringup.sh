#!/usr/bin/env bash
set -euo pipefail

SCRIPT_NAME="03-bringup"
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

usage() {
  cat <<'EOF'
Usage: 03-bringup.sh [--status|--stop|--help]

Generates config.toml and starts/stops rem-daemon in tmux session remfield/window rem.
EOF
}

selected_library_from_discovery() {
  local libs_json="$1"
  python3 - "$libs_json" <<'PY'
import json,sys
from pathlib import Path
payload=json.loads(Path(sys.argv[1]).read_text())
libs=payload["libraries"]
prefer=[lib for lib in libs if lib.get("product","").strip()=="MSL G3 Series" and lib.get("revision","").strip()=="D.00"]
if len(prefer)==1:
    print(prefer[0]["serial"])
elif len(libs)==1:
    print(libs[0]["serial"])
else:
    print(libs[0]["serial"])
PY
}


start_daemon() {
  local config="$1" log_file="$2" session="$3"
  if tmux has-session -t "$session" 2>/dev/null; then
    tmux kill-session -t "$session" || true
  fi
  # Drain any cartridges left in drives (previous use, prior kit runs)
  # BEFORE the daemon takes device ownership — a 2-drive library with
  # occupied drives can't serve writes.
  fieldtest_drain_drives "$(fieldtest_selected_library_serial)" || true
  tmux new-session -d -s "$session" -n rem
  tmux pipe-pane -o -t "$session":rem "cat >> \"$log_file\""
  tmux send-keys -t "$session":rem "exec $(printf '%q' "$(fieldtest_rem_daemon_bin)") --config $(printf '%q' "$config")" C-m
}

is_socket_live() {
  local endpoint="$1"
  set +e
  "$(fieldtest_rem_bin)" daemon --endpoint "$endpoint" health >/dev/null 2>&1
  local rc=$?
  set -e
  [[ $rc -eq 0 ]]
}

main() {
  case "${1:-}" in
    --help|-h)
      usage
      exit 0
      ;;
    --stop)
      fieldtest_init_layout
      local session
      session="remfield"
      if tmux has-session -t "$session" 2>/dev/null; then
        tmux send-keys -t "$session":rem C-c || true
        sleep 1
        tmux kill-session -t "$session" || true
      fi
      rm -f -- "$(fieldtest_socket_path)"
      fieldtest_evidence_record "$SCRIPT_NAME" stop PASS "tmux session stopped"
      exit 0
      ;;
    --status)
      fieldtest_init_layout
      if tmux has-session -t remfield 2>/dev/null; then
        printf '%s\n' "tmux: remfield is running"
      else
        printf '%s\n' "tmux: remfield is not running"
      fi
      if [[ -S "$(fieldtest_socket_path)" ]] && is_socket_live "$(fieldtest_rem_endpoint)"; then
        printf '%s\n' "socket: live at $(fieldtest_socket_path)"
      else
        printf '%s\n' "socket: not live"
      fi
      exit 0
      ;;
  esac

  fieldtest_init_layout
  fieldtest_detect_env || true

  local libs_json selected_serial config log_file endpoint
  libs_json="$(fieldtest_artifact_path "$SCRIPT_NAME" libraries "$(fieldtest_timestamp_id)")"
  fieldtest_capture_json "$libs_json" "$(fieldtest_rem_bin)" libraries --json
  selected_serial="$(fieldtest_selected_library_serial || true)"
  if [[ -z "$selected_serial" ]]; then
    selected_serial="$(selected_library_from_discovery "$libs_json")"
    fieldtest_write_selected_library_serial "$selected_serial"
  fi

  config="$(fieldtest_config_path)"
  fieldtest_write_config "$config" "$selected_serial"
  log_file="$(fieldtest_log_dir)/rem-daemon.log"
  mkdir -p "$(dirname -- "$log_file")"

  endpoint="$(fieldtest_rem_endpoint)"
  if [[ -S "/var/lib/replica/rem.sock" ]] && is_socket_live "unix:/var/lib/replica/rem.sock"; then
    fieldtest_evidence_record "$SCRIPT_NAME" foreign-daemon FAIL "refusing to start because the shared /var/lib/replica rem-daemon is still running"
    printf '%s\n' "refusing to start: /var/lib/replica/rem.sock is live. Stop the existing harness daemon first, then rerun 03-bringup.sh." >&2
    exit 1
  fi
  if [[ -S "$(fieldtest_socket_path)" ]] && is_socket_live "$endpoint"; then
    fieldtest_evidence_record "$SCRIPT_NAME" socket-occupied FAIL "socket $(fieldtest_socket_path) is already live; stop the existing rem-daemon first"
    printf '%s\n' "refusing to start because $(fieldtest_socket_path) is already owned by a running daemon" >&2
    exit 1
  fi

  # One retry: daemon startup can lose a transient SQLite lock race (e.g. a
  # CLI catalog reader closing) and exit; a second start is reliably clean.
  local attempt waited started=0
  for attempt in 1 2; do
    start_daemon "$config" "$log_file" remfield
    waited=0
    until is_socket_live "$endpoint"; do
      sleep 1
      waited=$((waited + 1))
      if [[ $waited -ge 30 ]]; then
        break
      fi
    done
    if is_socket_live "$endpoint"; then
      started=1
      break
    fi
    printf '%s\n' "daemon start attempt $attempt failed; retrying" >&2
  done
  if [[ $started -ne 1 ]]; then
    fieldtest_evidence_record "$SCRIPT_NAME" start FAIL "daemon did not become healthy after 2 attempts" "$log_file"
    printf '%s\n' "daemon did not become healthy; inspect $log_file" >&2
    exit 1
  fi

  # Verify every allowlisted (initialized) barcode is visible in the daemon
  # catalog — the check that used to live in 10-init-pools, which now runs
  # before the daemon exists.
  local catalog_json barcode
  catalog_json="$(fieldtest_artifact_path "$SCRIPT_NAME" catalog-tapes "$(fieldtest_timestamp_id)")"
  if fieldtest_capture_json "$catalog_json" "$(fieldtest_rem_bin)" catalog --endpoint "$endpoint" tapes --json; then
    while IFS= read -r barcode; do
      [[ -n "$barcode" ]] || continue
      if grep -Fq "$barcode" "$catalog_json"; then
        fieldtest_evidence_record "$SCRIPT_NAME" "catalog-$barcode" PASS "initialized barcode $barcode visible in daemon catalog"
      else
        fieldtest_evidence_record "$SCRIPT_NAME" "catalog-$barcode" INFO "barcode $barcode not (yet) in daemon catalog — run 10-init-pools before writes"
      fi
    done < <(fieldtest_allowlist_barcodes)
  fi

  local status_json
  status_json="$(fieldtest_artifact_path "$SCRIPT_NAME" daemon-health "$(fieldtest_timestamp_id)")"
  fieldtest_capture_json "$status_json" "$(fieldtest_rem_bin)" daemon --endpoint "$endpoint" health
  cp -f -- "$status_json" "$(fieldtest_latest_artifact_path "$SCRIPT_NAME" daemon-health)"
  fieldtest_evidence_record "$SCRIPT_NAME" bringup PASS "daemon started in tmux session remfield; config written for $selected_serial" "$config"
}

fieldtest_run_with_lock main "$@"
