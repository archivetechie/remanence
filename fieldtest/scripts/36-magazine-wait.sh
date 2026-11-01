#!/usr/bin/env bash
set -euo pipefail

SCRIPT_NAME="36-magazine-wait"
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

usage() {
  cat <<'EOF'
Usage: 36-magazine-wait.sh [--help]

Checks that a daemon changer move waits out an open magazine instead of failing.
With the daemon up, the operator pulls out one magazine that holds none of the
allowlisted tapes. The leg asks the daemon to load an allowlisted tape into an
empty bay, checks that live status shows the move waiting on the library, and
asks the operator to push the magazine back in. The load must then complete
after the library re-inventories, and the tape is unloaded to its home slot.

Interactive: it reads the operator's answers from the terminal (/dev/tty), so
run it in tmux, as run.sh is run. It never runs without a terminal.
EOF
}

# The poll and the deadlines. The daemon waits up to changer_not_ready_wait_seconds
# (default 600 s) for the library; the operator closes the magazine well inside it.
POLL_SECONDS="${FIELD_MAGAZINE_POLL_SECONDS:-5}"
# The load client, global so the EXIT trap can still see it after main returns.
LOAD_PID=""
SEE_WAIT_SECONDS="${FIELD_MAGAZINE_SEE_WAIT_SECONDS:-120}"
HOLD_OPEN_SECONDS="${FIELD_MAGAZINE_HOLD_OPEN_SECONDS:-30}"
LOAD_DONE_SECONDS="${FIELD_MAGAZINE_LOAD_DONE_SECONDS:-900}"

# The operator's terminal, opened once in main. Tests point FIELD_MAGAZINE_TTY_IN and
# FIELD_MAGAZINE_TTY_OUT at files.
TTY_IN="${FIELD_MAGAZINE_TTY_IN:-/dev/tty}"
TTY_OUT="${FIELD_MAGAZINE_TTY_OUT:-/dev/tty}"

# shellcheck disable=SC2059  # say is a printf wrapper: its first argument is the format.
say() {
  printf "$@" >&"$tty_out"
}

ask() {
  # ask <prompt> <word>: wait until the operator types <word> at the terminal.
  local prompt="$1" word="$2" answer
  while :; do
    say '\n>>> %s\n>>> Type "%s" and press Enter: ' "$prompt" "$word"
    IFS= read -r answer <&"$tty_in" || return 1
    [[ "$answer" == "$word" ]] && return 0
    say '>>> That was not "%s".\n' "$word"
  done
}

live_status() {
  "$(fieldtest_rem_bin)" top --endpoint "$(fieldtest_rem_endpoint)" --once --json
}

# pick <live.json> <serial> <allowlist barcodes...>: print "<slot> <barcode> <bay>"
# for the first allowlisted tape in a storage slot and the first empty drive bay.
pick() {
  python3 - "$@" <<'PY'
import json, sys
path, serial, *allowed = sys.argv[1:]
data = json.load(open(path))
lib = next((l for l in data.get("libraries", [])
            if (l.get("library") or {}).get("library_serial") == serial), None)
if lib is None:
    sys.exit(f"library {serial} not in live status")
loaded = {d.get("loaded_tape_barcode") for d in lib.get("drives", [])}
slot = next((s for s in lib.get("slots", [])
             if s.get("voltag") in allowed and s.get("voltag") not in loaded), None)
bay = next((d for d in lib.get("drives", [])
            if not d.get("loaded_tape_barcode") and not d.get("loaded_tape_uuid")), None)
if slot is None:
    sys.exit("no allowlisted tape is in a storage slot")
if bay is None:
    sys.exit("no drive bay is empty")
print(slot["element_address"], slot["voltag"], bay["element_address"])
PY
}

# changer_wait <live.json> <serial>: print the library's changer wait, if any.
changer_wait() {
  python3 - "$1" "$2" <<'PY'
import json, sys
data = json.load(open(sys.argv[1]))
for lib in data.get("libraries", []):
    if (lib.get("library") or {}).get("library_serial") == sys.argv[2]:
        print(lib.get("changer_wait") or "")
PY
}

# where <live.json> <serial> <barcode>: print "slot <addr>", "bay <addr>" or "missing".
where() {
  python3 - "$1" "$2" "$3" <<'PY'
import json, sys
data = json.load(open(sys.argv[1]))
serial, barcode = sys.argv[2:]
for lib in data.get("libraries", []):
    if (lib.get("library") or {}).get("library_serial") != serial:
        continue
    for d in lib.get("drives", []):
        if d.get("loaded_tape_barcode") == barcode:
            print("bay", d["element_address"]); sys.exit()
    for s in lib.get("slots", []):
        if s.get("voltag") == barcode:
            print("slot", s["element_address"]); sys.exit()
print("missing")
PY
}

main() {
  if [[ "${1:-}" == --help || "${1:-}" == -h ]]; then
    usage
    exit 0
  fi
  fieldtest_init_layout
  fieldtest_detect_env || true
  local serial stamp
  serial="$(fieldtest_selected_library_serial)" || {
    echo "error: no selected library; run bringup first" >&2
    exit 1
  }
  stamp="$(fieldtest_timestamp_id)"
  if ! { exec {tty_in}<"$TTY_IN" {tty_out}>"$TTY_OUT"; } 2>/dev/null; then
    fieldtest_evidence_record "$SCRIPT_NAME" magazine FAIL "no terminal: this leg needs the operator at the library and at the tmux window"
    exit 1
  fi

  local before picked slot barcode bay
  before="$(fieldtest_artifact_path "$SCRIPT_NAME" before "$stamp")"
  if ! fieldtest_capture_json "$before" live_status; then
    fieldtest_evidence_record "$SCRIPT_NAME" magazine FAIL "the daemon did not answer a live-status query" "$before"
    exit 1
  fi
  local -a allowed
  mapfile -t allowed < <(fieldtest_allowlist_barcodes)
  if ! picked="$(pick "$before" "$serial" "${allowed[@]}")"; then
    fieldtest_evidence_record "$SCRIPT_NAME" magazine FAIL "could not choose a tape and a bay: ${picked:-see $before}" "$before"
    exit 1
  fi
  read -r slot barcode bay <<<"$picked"
  printf -v slot_hex '0x%04x' "$slot"
  printf -v bay_hex '0x%04x' "$bay"

  cat >&"$tty_out" <<EOF

=== Magazine wait check on library $serial
The daemon will load $barcode from slot $slot_hex into drive bay $bay_hex while a
magazine is out. Pull out ONE magazine that holds none of these tapes:
${allowed[*]}
Opening a magazine pauses the robot for every partition, so pick a moment when
the other partitions are not moving tapes. Touch nothing inside the magazine.
EOF
  ask "Pull the magazine fully out now." open

  local offset load_json t_open t_seen="" t_closed wait_text="" snap
  offset="$(fieldtest_daemon_log_offset)"
  load_json="$(fieldtest_artifact_path "$SCRIPT_NAME" load "$stamp")"
  t_open="$SECONDS"
  fieldtest_capture_json "$load_json" "$(fieldtest_rem_bin)" drive load \
    --endpoint "$(fieldtest_rem_endpoint)" --library "$serial" --barcode "$barcode" --bay "$bay" --json &
  LOAD_PID=$!
  local load_pid=$LOAD_PID
  # A TERM to this leg also stops the client; the daemon keeps the move's outcome either way.
  trap 'kill "$LOAD_PID" 2>/dev/null || true' EXIT

  snap="$(fieldtest_artifact_path "$SCRIPT_NAME" waiting "$stamp")"
  local deadline=$(( t_open + SEE_WAIT_SECONDS ))
  while (( SECONDS < deadline )) && kill -0 "$load_pid" 2>/dev/null; do
    sleep "$POLL_SECONDS"
    if live_status >"$snap.tmp" 2>/dev/null; then
      wait_text="$(changer_wait "$snap.tmp" "$serial")"
      if [[ -n "$wait_text" ]]; then
        mv "$snap.tmp" "$snap"
        t_seen="$SECONDS"
        say '\nlive status: %s\n' "$wait_text"
        break
      fi
    fi
  done
  rm -f "$snap.tmp"

  if [[ -n "$t_seen" ]]; then
    say 'Keeping the magazine out %ss longer so the wait polls a few times.\n' "$HOLD_OPEN_SECONDS"
    sleep "$HOLD_OPEN_SECONDS"
  fi
  ask "Push the magazine back in now and wait for the library to finish its inventory." closed
  t_closed="$SECONDS"

  local load_rc=0
  deadline=$(( t_closed + LOAD_DONE_SECONDS ))
  while kill -0 "$load_pid" 2>/dev/null && (( SECONDS < deadline )); do
    sleep "$POLL_SECONDS"
  done
  if kill -0 "$load_pid" 2>/dev/null; then
    kill "$load_pid" 2>/dev/null || true
    load_rc=124
  fi
  local wait_rc=0
  wait "$load_pid" || wait_rc=$?
  (( load_rc == 0 )) && load_rc=$wait_rc
  trap - EXIT
  local t_done
  t_done="$SECONDS"

  local log_lines after location
  log_lines="$(fieldtest_artifact_path "$SCRIPT_NAME" daemon-log "$stamp")"
  tail -c "+$(( offset + 1 ))" "$(fieldtest_log_dir)/rem-daemon.log" 2>/dev/null |
    grep -E "waiting for library|ready after|re-checking|gave up|MOVE MEDIUM|not ready" >"$log_lines" || true
  after="$(fieldtest_artifact_path "$SCRIPT_NAME" after "$stamp")"
  fieldtest_capture_json "$after" live_status || true
  location="$(where "$after" "$serial" "$barcode" 2>/dev/null || echo unknown)"

  local extra
  extra="$(python3 - "$wait_text" "$t_open" "${t_seen:-}" "$t_closed" "$t_done" "$load_rc" "$location" <<'PY'
import json, sys
text, t_open, t_seen, t_closed, t_done, rc, location = sys.argv[1:]
print(json.dumps({
    "changer_wait": text or None,
    "seconds_open_to_wait_seen": (int(t_seen) - int(t_open)) if t_seen else None,
    "seconds_open": int(t_closed) - int(t_open),
    "seconds_closed_to_load_done": int(t_done) - int(t_closed),
    "load_exit_code": int(rc),
    "tape_location_after": location,
}))
PY
)"

  local status=PASS summary
  if [[ -z "$t_seen" ]]; then
    status=FAIL
    summary="live status never showed a changer wait within ${SEE_WAIT_SECONDS}s of the magazine opening"
  elif (( load_rc != 0 )); then
    status=FAIL
    summary="the load did not complete after the magazine closed (exit $load_rc)"
  elif [[ "$location" != "bay $bay" ]]; then
    status=FAIL
    summary="the load reported success but $barcode is at '$location', not bay $bay_hex"
  elif ! grep -q "re-checking" "$log_lines"; then
    status=WARN
    summary="the load completed after the wait, but the daemon log has no re-check line"
  else
    summary="the load waited for the magazine ($wait_text), re-checked and completed"
  fi
  fieldtest_evidence_record "$SCRIPT_NAME" magazine-wait "$status" "$summary" "$load_json" "$extra"

  # Send the tape home whatever happened; run.sh's teardown also returns cartridges.
  if [[ "$location" == "bay $bay" ]]; then
    local unload_json
    unload_json="$(fieldtest_artifact_path "$SCRIPT_NAME" unload "$stamp")"
    if fieldtest_capture_json "$unload_json" "$(fieldtest_rem_bin)" drive unload \
      --endpoint "$(fieldtest_rem_endpoint)" --library "$serial" --bay "$bay" --json; then
      fieldtest_capture_json "$after" live_status || true
      location="$(where "$after" "$serial" "$barcode" 2>/dev/null || echo unknown)"
      if [[ "$location" == "slot $slot" ]]; then
        fieldtest_evidence_record "$SCRIPT_NAME" unload PASS "$barcode is back in slot $slot_hex" "$unload_json"
      else
        fieldtest_evidence_record "$SCRIPT_NAME" unload WARN "$barcode unloaded but is at '$location', not slot $slot_hex" "$unload_json"
      fi
    else
      fieldtest_evidence_record "$SCRIPT_NAME" unload FAIL "unloading $barcode from bay $bay_hex failed; teardown will retry" "$unload_json"
    fi
  fi
  [[ "$status" != FAIL ]]
}

fieldtest_run_with_lock main "$@"
