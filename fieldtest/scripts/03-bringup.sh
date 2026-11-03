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
  if ! fieldtest_require_overlap_staging; then
    fieldtest_evidence_record "$SCRIPT_NAME" staging-mode FAIL "refusing to start the daemon in serial append staging mode; set append_staging_mode = \"overlap\" (see stderr)"
    exit 1
  fi
  if ! fieldtest_require_memlock; then
    fieldtest_evidence_record "$SCRIPT_NAME" memlock FAIL "refusing to start the daemon under a locked-memory limit below 1 GiB, which caps the read reservoir; raise RLIMIT_MEMLOCK for the daemon (see stderr)"
    exit 1
  fi
  # Close the work-lock descriptor for the tmux server: a server started here
  # would otherwise inherit fd 9 and hold the kit lock for its whole life.
  tmux new-session -d -s "$session" -n rem 9>&-
  tmux pipe-pane -o -t "$session":rem "cat >> \"$log_file\""
  local field_env="" key
  # tmux may predate this shell: explicitly carry the owner-armed kit facility.
  # Ordinary daemon builds do not compile these hooks.
  for key in REM_FIELD_GATE_DIR REM_FIELD_TRACE REM_FIELD_PROGRESS REM_FIELD_ALLOW_FAIL; do
    if [[ -n ${!key:-} ]]; then field_env+="$(printf '%q' "$key=${!key}") "; fi
  done
  tmux send-keys -t "$session":rem "exec env $field_env$(printf '%q' "$(fieldtest_rem_daemon_bin)") --config $(printf '%q' "$config")" C-m
}

is_socket_live() {
  local endpoint="$1"
  set +e
  "$(fieldtest_rem_bin)" daemon --endpoint "$endpoint" health >/dev/null 2>&1
  local rc=$?
  set -e
  [[ $rc -eq 0 ]]
}

# Return only allowlisted media in the selected library while the daemon owns it.
# Each CLI unload waits for the operation and uses its recorded source slot.
unload_allowlisted_before_stop() {
  local serial inventory loaded bay barcode eligible="" count=0
  local retry_window="$1"
  local call_limit=$((retry_window + 900))
  if ! serial="$(fieldtest_selected_library_serial)" || [[ -z "$serial" ]]; then
    fieldtest_evidence_record "$SCRIPT_NAME" stop-inventory WARN "cannot return cartridges: selected library is unavailable"
    return
  fi
  inventory="$(fieldtest_artifact_path "$SCRIPT_NAME" stop-inventory "$(fieldtest_timestamp_id)")"
  if ! fieldtest_capture_json "$inventory" timeout --foreground --signal=KILL 60 "$(fieldtest_rem_bin)" top --endpoint "$(fieldtest_rem_endpoint)" --once --json; then
    fieldtest_evidence_record "$SCRIPT_NAME" stop-inventory WARN "cannot inspect seated cartridges in library $serial before stopping" "$inventory"
    return
  fi
  if ! loaded="$(python3 - "$inventory" "$serial" <<'PYLOAD'
import json, sys
from pathlib import Path
payload = json.loads(Path(sys.argv[1]).read_text())
libraries = [lib for lib in payload["data"]["libraries"]
             if (lib.get("library") or {}).get("library_serial") == sys.argv[2]]
if len(libraries) != 1:
    raise SystemExit("selected library absent or ambiguous in live status")
for drive in libraries[0]["drives"]:
    barcode = drive.get("loaded_tape_barcode")
    if not barcode:
        continue  # An unidentified cartridge cannot pass the allowlist.
    bay = drive.get("element_address")
    if bay is None:
        continue  # Unplaced drive; it has no address for a safe unload.
    if type(bay) is not int or not 0 < bay <= 65535 or any(c.isspace() for c in barcode):
        raise SystemExit("invalid bay or barcode in live status")
    print(f"{bay}\t{barcode}")
PYLOAD
)"; then
    fieldtest_evidence_record "$SCRIPT_NAME" stop-inventory WARN "cannot parse seated cartridges in library $serial; no unload attempted" "$inventory"
    return
  fi
  while IFS=$'\t' read -r bay barcode; do
    [[ -n "$bay" ]] || continue
    fieldtest_require_allowlisted "$barcode" || continue
    eligible+="$bay"$'\t'"$barcode"$'\n'
    count=$((count + 1))
  done <<<"$loaded"
  # Inventory has its own deadline. Give every allowlisted seated bay its
  # full call budget, plus 60 seconds for phase bookkeeping.
  if ! timeout --signal=KILL "$((call_limit * count + 60))" bash "${BASH_SOURCE[0]}" --stop-unload-phase "$retry_window" "$serial" "$eligible"; then
    fieldtest_evidence_record "$SCRIPT_NAME" stop-unload-phase WARN "unload phase failed or timed out; inspect cartridge placement; proceeding to daemon stop"
  fi
}

# Run only the inspected, allowlisted bays inside the shared phase deadline.
unload_inspected_bays() {
  local retry_window="$1" serial="$2" loaded="$3"
  local call_limit=$((retry_window + 900))
  local bay barcode evidence rc started cause attempt
  [[ "$serial" == "$(fieldtest_selected_library_serial)" ]] || return 1
  while IFS=$'\t' read -r bay barcode; do
    [[ -n "$bay" ]] || continue
    fieldtest_require_allowlisted "$barcode" || continue
    evidence="$(fieldtest_artifact_path "$SCRIPT_NAME" "stop-unload-$bay" "$(fieldtest_timestamp_id)")"
    started=$SECONDS
    attempt=0
    while :; do
      attempt=$((attempt + 1))
      # KILL is deliberate: run.sh teardown ignores TERM/INT/HUP, and its
      # children inherit those dispositions. Foreground mode keeps the CLI in
      # the outer phase timeout's process group, so that deadline kills it too.
      rc=0
      fieldtest_capture_text "$evidence.$attempt" timeout --foreground --signal=KILL "$call_limit" \
        "$(fieldtest_rem_bin)" drive unload --endpoint "$(fieldtest_rem_endpoint)" --library "$serial" --bay "$bay" --json || rc=$?
      cause="$(python3 - "$evidence.$attempt" <<'PYRESULT'
import json, sys
from pathlib import Path
capture = json.loads(Path(sys.argv[1]).read_text())
text = capture["stdout"] + "\n" + capture["stderr"]
if capture["exit_code"] in (124, 137):
    print("timeout")
elif capture["exit_code"]:
    print("busy" if "busy" in text.lower() or "resourceexhausted" in text.lower() or "resource_exhausted" in text.lower() else "failed")
else:
    try:
        payload = json.loads(capture["stdout"])
        notes = payload["data"]["progress"]
        print("ok")
    except (ValueError, KeyError, TypeError, AttributeError):
        print("unrecognized unload result")
PYRESULT
)"
      if [[ "$cause" == busy ]] && (( SECONDS - started + 10 <= retry_window )); then
        sleep 10
        # Scheduling delays must not start another attempt beyond the window.
        if (( SECONDS - started <= retry_window )); then
          continue
        fi
      fi
      if (( rc == 0 )) && [[ "$cause" == ok ]]; then
        fieldtest_evidence_record "$SCRIPT_NAME" "stop-unload-$bay" PASS "returned $barcode from library $serial bay $bay to its recorded source slot" "$evidence.$attempt"
      else
        fieldtest_evidence_record "$SCRIPT_NAME" "stop-unload-$bay" WARN "unload for $barcode in library $serial bay $bay: $cause (exit $rc, attempt $attempt); inspect cause and placement in evidence" "$evidence.$attempt"
      fi
      break
    done
  done <<<"$loaded"
}

main() {
  case "${1:-}" in
    --help|-h)
      usage
      exit 0
      ;;
    --stop-unload-phase)
      # Internal subprocess boundary gives the entire phase a hard deadline.
      fieldtest_init_layout
      unload_inspected_bays "$2" "$3" "$4"
      return
      ;;
    --stop)
      fieldtest_init_layout
      local py
      py="$(fieldtest_python)" || exit 2
      local session pid started elapsed retry_window stop_timeout forced=0
      session="remfield"
      # The daemon may already have dispatched UNLOAD. Let its changer wait
      # expire and finish cleanup before the kit resorts to a forced kill.
      stop_timeout="${REMFIELD_STOP_TIMEOUT_SECONDS:-}"
      if [[ -z "$stop_timeout" ]]; then
        stop_timeout="$("$py" - "$(fieldtest_config_path)" <<'PYSTOP'
import sys, tomllib
from pathlib import Path
config = tomllib.loads(Path(sys.argv[1]).read_text())
# Older kit configs omit the setting and use the daemon's 600-second default.
seconds = config["daemon"].get("changer_not_ready_wait_seconds", 600)
if type(seconds) is not int or seconds < 0:
    raise SystemExit("invalid daemon.changer_not_ready_wait_seconds")
print(seconds + 120)
PYSTOP
)"
      fi
      if [[ ! "$stop_timeout" =~ ^[0-9]+$ ]]; then
        printf '%s\n' "REMFIELD_STOP_TIMEOUT_SECONDS must be a non-negative integer" >&2
        exit 1
      fi
      # Unload through the live daemon before SIGTERM. The daemon itself leaves
      # seated cartridges alone. Keep tmux alive for all operation diagnostics.
      pid="$(pgrep -f "rem-daemon --config $(fieldtest_config_path)" | head -1 || true)"
      if [[ -n "$pid" ]]; then
        if retry_window="$("$py" - "$(fieldtest_config_path)" <<'PYCONFIG'
import sys, tomllib
from pathlib import Path
config = tomllib.loads(Path(sys.argv[1]).read_text())
seconds = config["daemon"]["default_idle_timeout_seconds"]
if type(seconds) is not int or seconds < 0:
    raise SystemExit("invalid daemon.default_idle_timeout_seconds")
print(seconds + 60)
PYCONFIG
)"; then
          unload_allowlisted_before_stop "$retry_window"
        else
          fieldtest_evidence_record "$SCRIPT_NAME" stop-unload-phase WARN "cannot read idle timeout from kit config; no unload attempted; proceeding to daemon stop"
        fi
      fi
      # An unload can take minutes; resolve the process again before signalling.
      pid="$(pgrep -f "rem-daemon --config $(fieldtest_config_path)" | head -1 || true)"
      started=$SECONDS
      if [[ -n "$pid" ]]; then
        kill "$pid" 2>/dev/null || true
      fi
      while (( SECONDS - started < 10#$stop_timeout )); do
        pid="$(pgrep -f "rem-daemon --config $(fieldtest_config_path)" | head -1 || true)"
        [[ -n "$pid" ]] || break
        sleep 1
      done
      pid="$(pgrep -f "rem-daemon --config $(fieldtest_config_path)" | head -1 || true)"
      if [[ -n "$pid" ]]; then
        forced=1
        kill -9 "$pid" 2>/dev/null || true
        sleep 1
      fi
      elapsed=$((SECONDS - started))
      if pgrep -f "rem-daemon --config $(fieldtest_config_path)" >/dev/null 2>&1; then
        fieldtest_evidence_record "$SCRIPT_NAME" stop FAIL "rem-daemon would not die after ${elapsed}s; investigate manually" "" "{\"duration_seconds\":$elapsed}"
        exit 1
      fi
      if tmux has-session -t "$session" 2>/dev/null; then
        tmux kill-session -t "$session" || true
      fi
      rm -f -- "$(fieldtest_socket_path)"
      if (( forced )); then
        fieldtest_evidence_record "$SCRIPT_NAME" stop WARN "daemon needs a forced kill after ${elapsed}s; a forced kill may leave a cartridge unthreaded in its drive. Check rem top --once and the library's front panel; a manual LOAD may be needed" "" "{\"duration_seconds\":$elapsed,\"timeout_seconds\":$((10#$stop_timeout))}"
      else
        fieldtest_evidence_record "$SCRIPT_NAME" stop PASS "daemon stops gracefully and is gone after ${elapsed}s" "" "{\"duration_seconds\":$elapsed,\"timeout_seconds\":$((10#$stop_timeout))}"
      fi
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
  if pgrep -f "rem-daemon --config $(fieldtest_config_path)" >/dev/null 2>&1; then
    fieldtest_evidence_record "$SCRIPT_NAME" orphan-daemon FAIL "a rem-daemon with this config is already running; run 03-bringup.sh --stop first"
    printf '%s\n' "refusing to start: an existing rem-daemon (possibly orphaned) holds this config; run 03-bringup.sh --stop" >&2
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
