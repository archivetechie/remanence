#!/usr/bin/env bash
set -euo pipefail

SCRIPT_NAME="10-init-pools"
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

usage() {
  cat <<'EOF'
Usage: 10-init-pools.sh [--count N] [--help]

Initializes allowlisted scratch tapes and verifies the catalog sees them in the
fieldtest-a / fieldtest-b tape pools.
EOF
}

selected_library() {
  local libs_json="$1"
  local selected
  if selected="$(fieldtest_selected_library_serial 2>/dev/null || true)"; [[ -n "$selected" ]]; then
    printf '%s\n' "$selected"
    return 0
  fi
  python3 - "$libs_json" <<'PY'
import json
import sys
from pathlib import Path

payload = json.loads(Path(sys.argv[1]).read_text())
libs = payload.get("libraries", [])
if not libs:
    raise SystemExit("no libraries discovered")
for lib in libs:
    if (lib.get("product") or "").strip() == "MSL G3 Series" and (lib.get("revision") or "").strip() == "D.00":
        print(lib["serial"])
        raise SystemExit(0)
print(libs[0]["serial"])
PY
}

init_media_not_ready() {
  local path="$1"
  grep -qiE 'media not ready for tape init|media initializing/calibrating|logical unit becoming ready|target busy during readiness probe|transport completion unknown during readiness probe' "$path"
}

init_transport_unknown() {
  local path="$1"
  python3 - "$path" <<'PY'
import json
import re
import sys
from pathlib import Path

payload = json.loads(Path(sys.argv[1]).read_text())
text = f"{payload.get('stdout', '')}\n{payload.get('stderr', '')}"
lower = text.lower()
needles = [
    "transport error",
    "completion unknown",
    "did_time_out",
    "task aborted",
    "resetting scsi",
    "sg_io transport error",
]
if any(needle in lower for needle in needles):
    raise SystemExit(0)
for match in re.finditer(r"\bhost_status=0x([0-9a-fA-F]+)\b", text):
    if int(match.group(1), 16) != 0:
        raise SystemExit(0)
raise SystemExit(1)
PY
}

init_readiness_code() {
  local path="$1"
  python3 - "$path" <<'PY'
import json
import re
import sys
from pathlib import Path

payload = json.loads(Path(sys.argv[1]).read_text())
text = f"{payload.get('stdout', '')}\n{payload.get('stderr', '')}"
marker = re.search(r"\bmedia_readiness_exit_code=(\d+)\b", text)
if marker:
    print(marker.group(1))
    raise SystemExit(0)
exit_code = payload.get("exit_code")
if exit_code in (10, 20, 30, 40, 50, 130):
    print(exit_code)
PY
}

init_raw_exit_code() {
  local path="$1"
  python3 - "$path" <<'PY'
import json
import sys
from pathlib import Path

payload = json.loads(Path(sys.argv[1]).read_text())
exit_code = payload.get("exit_code")
if isinstance(exit_code, int):
    print(exit_code)
PY
}

init_readiness_extra_json() {
  local path="$1" code="${2:-}"
  python3 - "$path" "$code" <<'PY'
import json
import re
import sys
from pathlib import Path

payload = json.loads(Path(sys.argv[1]).read_text())
code = sys.argv[2].strip()
text = f"{payload.get('stdout', '')}\n{payload.get('stderr', '')}"
state = None
match = re.search(r"\bmedia_readiness_state=([A-Za-z0-9_]+)\b", text)
if match:
    state = match.group(1)
else:
    try:
        stdout_json = json.loads(payload.get("stdout") or "{}")
        if isinstance(stdout_json, dict):
            state = stdout_json.get("state")
    except json.JSONDecodeError:
        pass
extra = {}
if state:
    extra["media_readiness_state"] = state
if code:
    extra["rem_exit_code"] = int(code)
print(json.dumps(extra, separators=(",", ":")))
PY
}

record_init_readiness_stop() {
  local barcode="$1" path="$2" code="${3:-10}"
  fieldtest_evidence_record "$SCRIPT_NAME" "init-${barcode}" INFO "media not ready/calibrating for ${barcode}; leave the tape in the drive, run 09-media-ready.sh, and do not rerun init until readiness is ready" "$path" "$(init_readiness_extra_json "$path" "$code")"
}

record_init_transport_stop() {
  local barcode="$1" path="$2" code="${3:-40}"
  fieldtest_evidence_record "$SCRIPT_NAME" "init-${barcode}" FAIL "transport/completion-unknown while initializing ${barcode}; stop destructive escalation and collect RCA evidence" "$path" "$(init_readiness_extra_json "$path" "$code")"
}

record_init_terminal_stop() {
  local barcode="$1" path="$2" code="$3"
  fieldtest_evidence_record "$SCRIPT_NAME" "init-${barcode}" FAIL "media readiness stopped init for ${barcode} with rem exit code ${code}; do not escalate to force or clobber until RCA/recovery is complete" "$path" "$(init_readiness_extra_json "$path" "$code")"
}

record_init_unclassified_stop() {
  local barcode="$1" path="$2" code="$3"
  fieldtest_evidence_record "$SCRIPT_NAME" "init-${barcode}" FAIL "unclassified rem exit code ${code} while initializing ${barcode}; stop destructive escalation and inspect the captured stderr/stdout before retrying" "$path" "$(init_readiness_extra_json "$path" "$code")"
}

stop_init_escalation_if_readiness_blocked() {
  local barcode="$1" path="$2" code raw_code
  code="$(init_readiness_code "$path" || true)"
  case "$code" in
    10)
      record_init_readiness_stop "$barcode" "$path" "$code"
      exit 10
      ;;
    20|30|50|130)
      record_init_terminal_stop "$barcode" "$path" "$code"
      exit "$code"
      ;;
    40)
      record_init_transport_stop "$barcode" "$path" "$code"
      exit 40
      ;;
  esac
  raw_code="$(init_raw_exit_code "$path" || true)"
  if [[ -n "$raw_code" && "$raw_code" != 0 && "$raw_code" != 1 ]]; then
    record_init_unclassified_stop "$barcode" "$path" "$raw_code"
    exit "$raw_code"
  fi
  if init_media_not_ready "$path"; then
    record_init_readiness_stop "$barcode" "$path" 10
    exit 10
  fi
  if init_transport_unknown "$path"; then
    record_init_transport_stop "$barcode" "$path" 40
    exit 40
  fi
}

init_pools_selftest() {
  local tmpdir detail code extra records
  tmpdir="$(mktemp -d)"
  mkdir -p "$tmpdir/home/evidence" "$tmpdir/home/state" "$tmpdir/home/log" "$tmpdir/home/spool"
  detail="$tmpdir/home/evidence/init-AOX030L9.json"
  cat >"$detail" <<'JSON'
{
  "command": "rem tape init AOX030L9 --dry-run",
  "exit_code": 10,
  "stdout": "",
  "stderr": "media not ready for tape init on AOX030L9 in drive 0x0001 media_readiness_state=media_initializing media_readiness_exit_code=10: media initializing/calibrating"
}
JSON
  REMFIELD_HOME="$tmpdir/home"
  export REMFIELD_HOME
  code="$(init_readiness_code "$detail")"
  [[ "$code" == 10 ]]
  extra="$(init_readiness_extra_json "$detail" "$code")"
  [[ "$extra" == *'"media_readiness_state":"media_initializing"'* ]]
  [[ "$extra" == *'"rem_exit_code":10'* ]]
  fieldtest_evidence_record "$SCRIPT_NAME" init-AOX030L9 INFO "media not ready/calibrating for AOX030L9" "$detail" "$extra"
  records="$(fieldtest_records_path)"
  grep -q '"status":"INFO"' "$records"
  grep -q '"media_readiness_state":"media_initializing"' "$records"
  grep -q '"rem_exit_code":10' "$records"
  rm -rf "$tmpdir"
}

init_pools_escalation_selftest() {
  local tmpdir child_rc rem_log records
  tmpdir="$(mktemp -d)"
  mkdir -p "$tmpdir/home/bin" "$tmpdir/home/evidence" "$tmpdir/home/state" "$tmpdir/home/log" "$tmpdir/home/spool"
  cat >"$tmpdir/home/allowlist.txt" <<'EOF'
AOX030L9
EOF
  cat >"$tmpdir/home/bin/rem" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
home="${REMFIELD_HOME:?}"
printf '%s\n' "$*" >>"$home/rem-invocations.log"
if [[ "${1:-}" == libraries && "${2:-}" == --json ]]; then
  cat <<'JSON'
{"libraries":[{"serial":"LIBMAIN","product":"MSL G3 Series","revision":"D.00","vendor":"HPE","drive_count":1,"slot_count":1,"loaded_slot_count":1,"ie_port_count":0}]}
JSON
  exit 0
fi
if [[ "${1:-}" == library && "${3:-}" == --json && "${4:-}" == --slots ]]; then
  cat <<'JSON'
{"serial":"LIBMAIN","drives":[],"slots":[{"element_address":"0x03eb","full":true,"cartridge":"AOX030L9"}]}
JSON
  exit 0
fi
if [[ "${1:-}" == --allow && "${3:-}" == tape && "${4:-}" == init ]]; then
  if [[ " $* " == *" --dry-run "* ]]; then
    echo "media not ready for tape init on AOX030L9 in drive 0x0001 media_readiness_state=media_initializing media_readiness_exit_code=10: media initializing/calibrating" >&2
    exit 10
  fi
  echo "destructive escalation should not run: $*" >&2
  exit 99
fi
echo "unexpected mock rem invocation: $*" >&2
exit 98
EOF
  chmod +x "$tmpdir/home/bin/rem"
  REMFIELD_HOME="$tmpdir/home" bash "$0" --count 1 >/dev/null 2>"$tmpdir/selftest.stderr" || child_rc=$?
  child_rc="${child_rc:-0}"
  rem_log="$tmpdir/home/rem-invocations.log"
  records="$tmpdir/home/evidence/records.jsonl"
  if [[ "$child_rc" -ne 10 ]]; then
    echo "selftest: expected readiness exit 10, got $child_rc" >&2
    cat "$tmpdir/selftest.stderr" >&2 || true
    rm -rf "$tmpdir"
    return 1
  fi
  grep -q -- '--dry-run' "$rem_log"
  if grep -q -- '--force' "$rem_log"; then
    echo "selftest: --force should not run after readiness exit 10" >&2
    rm -rf "$tmpdir"
    return 1
  fi
  if grep -q -- '--clobber-data' "$rem_log"; then
    echo "selftest: --clobber-data should not run after readiness exit 10" >&2
    rm -rf "$tmpdir"
    return 1
  fi
  grep -q '"status":"INFO"' "$records"
  grep -q '"media_readiness_state":"media_initializing"' "$records"
  grep -q '"rem_exit_code":10' "$records"
  rm -rf "$tmpdir"
}

init_pools_unclassified_exit_selftest() {
  local tmpdir child_rc rem_log records
  tmpdir="$(mktemp -d)"
  mkdir -p "$tmpdir/home/bin" "$tmpdir/home/evidence" "$tmpdir/home/state" "$tmpdir/home/log" "$tmpdir/home/spool"
  cat >"$tmpdir/home/allowlist.txt" <<'EOF'
AOX030L9
EOF
  cat >"$tmpdir/home/bin/rem" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
home="${REMFIELD_HOME:?}"
printf '%s\n' "$*" >>"$home/rem-invocations.log"
if [[ "${1:-}" == libraries && "${2:-}" == --json ]]; then
  cat <<'JSON'
{"libraries":[{"serial":"LIBMAIN","product":"MSL G3 Series","revision":"D.00","vendor":"HPE","drive_count":1,"slot_count":1,"loaded_slot_count":1,"ie_port_count":0}]}
JSON
  exit 0
fi
if [[ "${1:-}" == library && "${3:-}" == --json && "${4:-}" == --slots ]]; then
  cat <<'JSON'
{"serial":"LIBMAIN","drives":[],"slots":[{"element_address":"0x03eb","full":true,"cartridge":"AOX030L9"}]}
JSON
  exit 0
fi
if [[ "${1:-}" == --allow && "${3:-}" == tape && "${4:-}" == init ]]; then
  if [[ " $* " == *" --dry-run "* ]]; then
    echo "synthetic usage/config failure without readiness marker" >&2
    exit 2
  fi
  echo "destructive escalation should not run: $*" >&2
  exit 99
fi
echo "unexpected mock rem invocation: $*" >&2
exit 98
EOF
  chmod +x "$tmpdir/home/bin/rem"
  REMFIELD_HOME="$tmpdir/home" bash "$0" --count 1 >/dev/null 2>"$tmpdir/selftest.stderr" || child_rc=$?
  child_rc="${child_rc:-0}"
  rem_log="$tmpdir/home/rem-invocations.log"
  records="$tmpdir/home/evidence/records.jsonl"
  if [[ "$child_rc" -ne 2 ]]; then
    echo "selftest: expected unclassified exit 2, got $child_rc" >&2
    cat "$tmpdir/selftest.stderr" >&2 || true
    rm -rf "$tmpdir"
    return 1
  fi
  grep -q -- '--dry-run' "$rem_log"
  if grep -q -- '--force' "$rem_log"; then
    echo "selftest: --force should not run after unclassified non-policy exit" >&2
    rm -rf "$tmpdir"
    return 1
  fi
  if grep -q -- '--clobber-data' "$rem_log"; then
    echo "selftest: --clobber-data should not run after unclassified non-policy exit" >&2
    rm -rf "$tmpdir"
    return 1
  fi
  grep -q '"status":"FAIL"' "$records"
  grep -q '"rem_exit_code":2' "$records"
  rm -rf "$tmpdir"
}

init_pools_transport_regex_selftest() {
  local tmpdir zero_path nonzero_path
  tmpdir="$(mktemp -d)"
  zero_path="$tmpdir/host-status-zero.json"
  nonzero_path="$tmpdir/host-status-nonzero.json"
  cat >"$zero_path" <<'JSON'
{
  "command": "rem tape init AOX030L9",
  "exit_code": 1,
  "stdout": "",
  "stderr": "ordinary policy refusal with host_status=0x00 in diagnostic detail"
}
JSON
  cat >"$nonzero_path" <<'JSON'
{
  "command": "rem tape init AOX030L9",
  "exit_code": 1,
  "stdout": "",
  "stderr": "SG_IO transport error: status=0x00 host_status=0x0003 driver_status=0x0000"
}
JSON
  if init_transport_unknown "$zero_path"; then
    echo "selftest: host_status=0x00 must not classify as transport unknown" >&2
    rm -rf "$tmpdir"
    return 1
  fi
  init_transport_unknown "$nonzero_path"
  rm -rf "$tmpdir"
}

main() {
  if [[ "${1:-}" == --selftest ]]; then
    init_pools_selftest
    init_pools_escalation_selftest
    init_pools_unclassified_exit_selftest
    init_pools_transport_regex_selftest
    exit 0
  fi
  if [[ "${1:-}" == --help || "${1:-}" == -h ]]; then
    usage
    exit 0
  fi

  local count="${FIELD_INIT_COUNT:-4}"
  if [[ "${1:-}" == --count ]]; then
    count="${2:?missing count}"
    shift 2
  fi

  fieldtest_init_layout
  fieldtest_detect_env || true

  # Direct-SCSI phase: the daemon must NOT be running — robotics behind a
  # live daemon's back poisons its cached inventory (source-empty moves).
  if [[ -S "$(fieldtest_socket_path)" ]]; then
    echo "error: rem-daemon appears to be running; run 03-bringup.sh --stop first" >&2
    echo "       (tape init + drive drains are direct-SCSI and must precede the daemon)" >&2
    exit 1
  fi

  local stamp libs_json serial allowlist_file data_barcodes cleaning_barcode
  stamp="$(fieldtest_timestamp_id)"
  libs_json="$(fieldtest_artifact_path "$SCRIPT_NAME" libraries "$stamp")"
  fieldtest_capture_json "$libs_json" "$(fieldtest_rem_bin)" libraries --json
  serial="$(selected_library "$libs_json")"
  fieldtest_write_selected_library_serial "$serial"

  allowlist_file="$(fieldtest_allowlist_path)"
  if [[ ! -f "$allowlist_file" ]]; then
    echo "error: missing allowlist $allowlist_file; run 01-allowlist.sh first" >&2
    exit 1
  fi

  mapfile -t data_barcodes < <(fieldtest_allowlist_barcodes)
  cleaning_barcode="$(fieldtest_allowlist_cleaning_barcode || true)"
  if [[ "${#data_barcodes[@]}" -lt "$count" ]]; then
    echo "error: need at least $count allowlisted scratch barcodes; found ${#data_barcodes[@]}" >&2
    exit 1
  fi

  if [[ ! -f "$(fieldtest_config_path)" ]]; then
    fieldtest_write_config "$(fieldtest_config_path)" "$serial"
    echo "generated $(fieldtest_config_path) (pool rules from the allowlist)"
  fi
  fieldtest_drain_drives "$serial" || true

  local halfway=$(((count + 1) / 2))
  local idx init_out init_level init_bay
  for idx in "${!data_barcodes[@]}"; do
    if (( idx >= count )); then
      break
    fi
    fieldtest_require_allowlisted "${data_barcodes[$idx]}"
    init_out="$(fieldtest_artifact_path "$SCRIPT_NAME" "init-${data_barcodes[$idx]}" "$stamp")"
    # Escalation ladder: blank scratch passes plain; used scratch needs
    # --force or --clobber-data. The operator typed DESTROY over these
    # barcodes at allowlist time — that is the clobber consent.
    init_level="plain"
    if fieldtest_capture_text "$init_out" "$(fieldtest_rem_bin)" --allow "$serial" tape init "${data_barcodes[$idx]}" --config "$(fieldtest_config_path)" --library "$serial" --dry-run; then
      if grep -qi "already" "$init_out"; then
        fieldtest_evidence_record "$SCRIPT_NAME" "init-${data_barcodes[$idx]}" PASS "already initialized by this catalog (rerun-safe skip)" "$init_out"
        continue
      fi
      stop_init_escalation_if_readiness_blocked "${data_barcodes[$idx]}" "$init_out"
    else
      stop_init_escalation_if_readiness_blocked "${data_barcodes[$idx]}" "$init_out"
    fi
    if ! fieldtest_capture_text "$init_out" "$(fieldtest_rem_bin)" --allow "$serial" tape init "${data_barcodes[$idx]}" --config "$(fieldtest_config_path)" --library "$serial"; then
      stop_init_escalation_if_readiness_blocked "${data_barcodes[$idx]}" "$init_out"
      init_level="force"
      if ! fieldtest_capture_text "$init_out" "$(fieldtest_rem_bin)" --allow "$serial" tape init "${data_barcodes[$idx]}" --config "$(fieldtest_config_path)" --library "$serial" --force; then
        stop_init_escalation_if_readiness_blocked "${data_barcodes[$idx]}" "$init_out"
        init_level="clobber-data"
        if ! fieldtest_capture_text "$init_out" "$(fieldtest_rem_bin)" --allow "$serial" tape init "${data_barcodes[$idx]}" --config "$(fieldtest_config_path)" --library "$serial" --clobber-data; then
          stop_init_escalation_if_readiness_blocked "${data_barcodes[$idx]}" "$init_out"
          if grep -q "needs-explicit-rebuild" "$init_out"; then
            fieldtest_evidence_record "$SCRIPT_NAME" "init-${data_barcodes[$idx]}" FAIL \
              "${data_barcodes[$idx]} carries a FOREIGN remanence identity (initialized by another rem instance); no init flag overrides this by design — use a different scratch cartridge, or physically relabel/erase this one" "$init_out"
          else
            fieldtest_evidence_record "$SCRIPT_NAME" "init-${data_barcodes[$idx]}" FAIL "tape init failed for ${data_barcodes[$idx]} at every escalation level" "$init_out"
          fi
          exit 1
        fi
      fi
    fi
    cp -f -- "$init_out" "$(fieldtest_latest_artifact_path "$SCRIPT_NAME" "init-${data_barcodes[$idx]}")"
    # tape init leaves the cartridge in the drive; unload it back home or a
    # 2-drive library runs out of free drives by the 3rd init.
    init_bay="$(grep -oE 'drive: 0x[0-9a-fA-F]+' "$init_out" | head -1 | awk '{print $2}' || true)"
    if [[ -n "$init_bay" ]]; then
      "$(fieldtest_rem_debug_bin)" --allow "$serial" unload --bay "$init_bay" "$serial" >/dev/null 2>&1 || \
        echo "warn: could not unload bay $init_bay after init (continuing)" >&2
    fi
    fieldtest_evidence_record "$SCRIPT_NAME" "init-${data_barcodes[$idx]}" PASS "initialized ${data_barcodes[$idx]} (level: $init_level) into pool candidate $((idx < halfway ? 1 : 2))" "$init_out"
  done

  fieldtest_evidence_record "$SCRIPT_NAME" init-summary PASS "initialized $count allowlisted tapes (catalog visibility verified after bringup)"

  if [[ -n "$cleaning_barcode" ]]; then
    fieldtest_evidence_record "$SCRIPT_NAME" cleaning INFO "allowlist also contains CLN:$cleaning_barcode"
  fi
}

fieldtest_run_with_lock main "$@"
