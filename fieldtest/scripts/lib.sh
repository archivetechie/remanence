#!/usr/bin/env bash
set -euo pipefail

if [[ -z "${BASH_VERSION:-}" ]]; then
  echo "error: lib.sh requires bash" >&2
  return 1 2>/dev/null || exit 1
fi

if [[ -z "${FIELDTEST_LIB_SOURCED:-}" ]]; then
  readonly FIELDTEST_LIB_SOURCED=1
fi

fieldtest_script_dir() {
  cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd
}

fieldtest_repo_root() {
  # scripts/ -> fieldtest/ -> repo root (two levels up)
  cd -- "$(fieldtest_script_dir)/../.." && pwd
}

fieldtest_now_utc() {
  date -u +%Y-%m-%dT%H:%M:%SZ
}

fieldtest_timestamp_id() {
  date -u +%Y%m%dT%H%M%SZ
}

fieldtest_host() {
  hostname -s 2>/dev/null || hostname
}

fieldtest_home() {
  printf '%s\n' "${REMFIELD_HOME:-$HOME/remfield}"
}

fieldtest_init_layout() {
  local home
  home="$(fieldtest_home)"
  mkdir -p \
    "$home" \
    "$home/bin" \
    "$home/state" \
    "$home/spool" \
    "$home/log" \
    "$home/evidence"
}

fieldtest_bin_dir() {
  printf '%s\n' "$(fieldtest_home)/bin"
}

fieldtest_target_release_dir() {
  printf '%s\n' "$(fieldtest_repo_root)/target/release"
}

fieldtest_resolve_bin() {
  local name="$1"
  local home_bin target_bin
  home_bin="$(fieldtest_bin_dir)/$name"
  target_bin="$(fieldtest_target_release_dir)/$name"
  if [[ -x "$home_bin" ]]; then
    printf '%s\n' "$home_bin"
    return 0
  fi
  if [[ -x "$target_bin" ]]; then
    printf '%s\n' "$target_bin"
    return 0
  fi
  echo "error: cannot find executable $name in $(fieldtest_bin_dir) or $(fieldtest_target_release_dir)" >&2
  return 1
}

fieldtest_rem_bin() {
  fieldtest_resolve_bin rem
}

fieldtest_rem_daemon_bin() {
  fieldtest_resolve_bin rem-daemon
}

fieldtest_rem_debug_bin() {
  fieldtest_resolve_bin rem-debug
}

fieldtest_io_bin() {
  local tool_bin home_bin
  tool_bin="$(fieldtest_repo_root)/fieldtest/tools/remfield-io/target/release/remfield-io"
  home_bin="$(fieldtest_bin_dir)/remfield-io"
  if [[ -x "$tool_bin" ]]; then
    printf '%s\n' "$tool_bin"
    return 0
  fi
  if [[ -x "$home_bin" ]]; then
    printf '%s\n' "$home_bin"
    return 0
  fi
  echo "error: cannot find executable remfield-io in $(dirname -- "$tool_bin") or $(fieldtest_bin_dir)" >&2
  return 1
}

fieldtest_socket_path() {
  printf '%s\n' "$(fieldtest_home)/rem.sock"
}

fieldtest_allowlist_path() {
  printf '%s\n' "$(fieldtest_home)/allowlist.txt"
}

fieldtest_config_path() {
  printf '%s\n' "$(fieldtest_home)/config.toml"
}

fieldtest_state_dir() {
  printf '%s\n' "$(fieldtest_home)/state"
}

fieldtest_spool_dir() {
  printf '%s\n' "$(fieldtest_home)/spool"
}

fieldtest_log_dir() {
  printf '%s\n' "$(fieldtest_home)/log"
}

fieldtest_evidence_dir() {
  printf '%s\n' "$(fieldtest_home)/evidence"
}

fieldtest_script_evidence_dir() {
  local script="$1"
  printf '%s\n' "$(fieldtest_evidence_dir)/$script"
}

fieldtest_artifact_path() {
  local script="$1" name="$2" ts="${3:-$(fieldtest_timestamp_id)}"
  printf '%s\n' "$(fieldtest_script_evidence_dir "$script")/$ts-$name.json"
}

fieldtest_latest_artifact_path() {
  local script="$1" name="$2"
  printf '%s\n' "$(fieldtest_evidence_dir)/$script-$name.json"
}

fieldtest_records_path() {
  printf '%s\n' "$(fieldtest_evidence_dir)/records.jsonl"
}

fieldtest_bench_csv_path() {
  printf '%s\n' "$(fieldtest_evidence_dir)/bench.csv"
}

fieldtest_work_lock() {
  printf '%s\n' "$(fieldtest_home)/work.lock"
}

fieldtest_soak_pidfile() {
  printf '%s\n' "$(fieldtest_home)/state/soak.pid"
}

fieldtest_soak_journal() {
  printf '%s\n' "$(fieldtest_log_dir)/soak.log"
}

fieldtest_json_get() {
  local path="$1"
  python3 -c '
import json
import re
import sys

path = sys.argv[1]
value = json.load(sys.stdin)

token_re = re.compile(r"([A-Za-z0-9_]+)|\[(\d+)\]")
tokens = []
idx = 0
while idx < len(path):
    if path[idx] == ".":
        idx += 1
        continue
    match = token_re.match(path, idx)
    if not match:
        raise SystemExit(f"invalid json path component near {path[idx:]!r}")
    key, item = match.groups()
    tokens.append(key if key is not None else int(item))
    idx = match.end()

for token in tokens:
    if isinstance(token, int):
        value = value[token]
    else:
        value = value[token]

if isinstance(value, (dict, list)):
    print(json.dumps(value, separators=(",", ":")))
elif value is True:
    print("true")
elif value is False:
    print("false")
elif value is None:
    print("null")
else:
    print(value)
' "$path"
}

fieldtest_json_list() {
  local path="$1"
  python3 -c '
import json
import re
import sys

path = sys.argv[1]
value = json.load(sys.stdin)
token_re = re.compile(r"([A-Za-z0-9_]+)|\[(\d+)\]")
tokens = []
idx = 0
while idx < len(path):
    if path[idx] == ".":
        idx += 1
        continue
    match = token_re.match(path, idx)
    if not match:
        raise SystemExit(f"invalid json path component near {path[idx:]!r}")
    key, item = match.groups()
    tokens.append(key if key is not None else int(item))
    idx = match.end()

for token in tokens:
    if isinstance(token, int):
        value = value[token]
    else:
        value = value[token]

if not isinstance(value, list):
    raise SystemExit("json path does not resolve to a list")
for item in value:
    if isinstance(item, (dict, list)):
        print(json.dumps(item, separators=(",", ":")))
    elif item is True:
        print("true")
    elif item is False:
        print("false")
    elif item is None:
        print("null")
    else:
        print(item)
' "$path"
}

fieldtest_json_value() {
  local path="$1"
  fieldtest_json_get "$path"
}

fieldtest_trim_lines() {
  sed '/^[[:space:]]*$/d'
}

fieldtest_allowlist_barcodes() {
  local file
  file="$(fieldtest_allowlist_path)"
  [[ -f "$file" ]] || return 0
  while IFS= read -r line; do
    line="${line%%#*}"
    line="${line//$'\r'/}"
    line="${line#"${line%%[![:space:]]*}"}"
    line="${line%"${line##*[![:space:]]}"}"
    [[ -n "$line" ]] || continue
    if [[ "$line" == CLN:* ]]; then
      continue
    fi
    printf '%s\n' "$line"
  done <"$file"
}

fieldtest_allowlist_cleaning_barcode() {
  local file
  file="$(fieldtest_allowlist_path)"
  [[ -f "$file" ]] || return 1
  python3 - "$file" <<'PY'
import sys
from pathlib import Path

path = Path(sys.argv[1])
for raw in path.read_text().splitlines():
    line = raw.split("#", 1)[0].strip()
    if not line:
        continue
    if line.startswith("CLN:"):
        print(line.split(":", 1)[1].strip())
        raise SystemExit(0)
raise SystemExit(1)
PY
}

fieldtest_require_allowlisted() {
  local barcode="${1:-}"
  local file
  file="$(fieldtest_allowlist_path)"
  if [[ -z "$barcode" ]]; then
    echo "error: allowlist check requires a barcode" >&2
    return 1
  fi
  if [[ ! -f "$file" ]]; then
    echo "error: missing allowlist file $file; run 01-allowlist.sh first" >&2
    return 1
  fi
  python3 - "$file" "$barcode" <<'PY'
import sys
from pathlib import Path

allowlist = Path(sys.argv[1])
barcode = sys.argv[2].strip()
allowed = False
for raw in allowlist.read_text().splitlines():
    line = raw.split("#", 1)[0].strip()
    if not line:
        continue
    if line == barcode:
        allowed = True
        break
    if line.startswith("CLN:") and barcode == line.split(":", 1)[1].strip():
        allowed = True
        break
if not allowed:
    raise SystemExit(1)
PY
}

fieldtest_json_escape() {
  python3 - "$1" <<'PY'
import json, sys
print(json.dumps(sys.argv[1]))
PY
}

fieldtest_color_for_status() {
  case "$1" in
    PASS) printf '\033[32m' ;;
    FAIL) printf '\033[31m' ;;
    SKIP) printf '\033[33m' ;;
    INFO) printf '\033[36m' ;;
    *) printf '\033[0m' ;;
  esac
}

fieldtest_human_line() {
  local script="$1" test_id="$2" status="$3" summary="$4"
  local color reset
  reset='\033[0m'
  color="$(fieldtest_color_for_status "$status")"
  if [[ -t 1 ]]; then
    printf '%b[%s] %s %s: %s%b\n' "$color" "$status" "$script" "$test_id" "$summary" "$reset"
  else
    printf '[%s] %s %s: %s\n' "$status" "$script" "$test_id" "$summary"
  fi
}

fieldtest_evidence_record() {
  local script="$1" test_id="$2" status="$3" summary="$4" detail_path="${5:-}"
  local records host env ts line
  records="$(fieldtest_records_path)"
  host="$(fieldtest_host)"
  env="${REMFIELD_ENV:-unknown}"
  ts="$(fieldtest_now_utc)"
  mkdir -p "$(dirname -- "$records")"
  line="$(
    python3 - "$ts" "$host" "$env" "$script" "$test_id" "$status" "$summary" "$detail_path" <<'PY'
import json
import sys

ts, host, env, script, test_id, status, summary, detail = sys.argv[1:]
print(json.dumps({
    "ts": ts,
    "host": host,
    "env": env,
    "script": script,
    "test_id": test_id,
    "status": status,
    "summary": summary,
    "detail_path": detail or None,
}, separators=(",", ":")))
PY
  )"
  printf '%s\n' "$line" >>"$records"
  fieldtest_human_line "$script" "$test_id" "$status" "$summary"
}

fieldtest_capture_json() {
  local outfile="$1"
  shift
  local tmp_out tmp_err rc cmd_display
  mkdir -p "$(dirname -- "$outfile")"
  tmp_out="$(mktemp)"
  tmp_err="$(mktemp)"
  cmd_display="$(printf '%q ' "$@")"
  set +e
  "$@" >"$tmp_out" 2>"$tmp_err"
  rc=$?
  set -e
  if [[ $rc -eq 0 ]]; then
    mv "$tmp_out" "$outfile"
  else
    python3 - "$outfile" "$rc" "$cmd_display" "$tmp_out" "$tmp_err" <<'PY'
import json
import sys
from pathlib import Path

outfile, rc, cmd, stdout_path, stderr_path = sys.argv[1:]
payload = {
    "command": cmd.strip(),
    "exit_code": int(rc),
    "stdout": Path(stdout_path).read_text(errors="replace"),
    "stderr": Path(stderr_path).read_text(errors="replace"),
}
Path(outfile).write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n")
PY
  fi
  if [[ -s "$tmp_err" && $rc -eq 0 ]]; then
    cat "$tmp_err" >&2
  fi
  rm -f "$tmp_out" "$tmp_err"
  return "$rc"
}

fieldtest_capture_text() {
  local outfile="$1"
  shift
  local tmp_out tmp_err rc cmd_display
  tmp_out="$(mktemp)"
  tmp_err="$(mktemp)"
  cmd_display="$(printf '%q ' "$@")"
  set +e
  "$@" >"$tmp_out" 2>"$tmp_err"
  rc=$?
  set -e
  python3 - "$outfile" "$rc" "$cmd_display" "$tmp_out" "$tmp_err" <<'PY'
import json
import sys
from pathlib import Path

outfile, rc, cmd, stdout_path, stderr_path = sys.argv[1:]
payload = {
    "command": cmd.strip(),
    "exit_code": int(rc),
    "stdout": Path(stdout_path).read_text(errors="replace"),
    "stderr": Path(stderr_path).read_text(errors="replace"),
}
Path(outfile).write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n")
PY
  rm -f "$tmp_out" "$tmp_err"
  return "$rc"
}

fieldtest_write_json_file() {
  local outfile="$1"
  shift
  python3 - "$outfile" "$@" <<'PY'
import json
import sys
from pathlib import Path

outfile = Path(sys.argv[1])
data = sys.stdin.read()
outfile.write_text(data if data.endswith("\n") else data + "\n")
PY
}

fieldtest_bench_csv_header() {
  printf 'metric,drive,block_size,payload,MB_s,seconds,bytes\n'
}

fieldtest_bench_record() {
  local metric="$1" drive="$2" block_size="$3" payload="$4" mb_s="$5" seconds="$6" bytes="$7"
  local csv
  csv="$(fieldtest_bench_csv_path)"
  mkdir -p "$(dirname -- "$csv")"
  if [[ ! -f "$csv" ]]; then
    fieldtest_bench_csv_header >"$csv"
  fi
  printf '%s,%s,%s,%s,%s,%s,%s\n' "$metric" "$drive" "$block_size" "$payload" "$mb_s" "$seconds" "$bytes" >>"$csv"
}

fieldtest_detect_env() {
  if [[ "${REMFIELD_ENV:-}" == vtl || "${REMFIELD_ENV:-}" == real ]]; then
    export REMFIELD_ENV
    return 0
  fi
  local json_file
  json_file="$(mktemp)"
  if ! fieldtest_capture_json "$json_file" "$(fieldtest_rem_bin)" libraries --json; then
    export REMFIELD_ENV=unknown
    rm -f "$json_file"
    return 1
  fi
  local detected
  detected="$(
    python3 - "$json_file" <<'PY'
import json
import sys
from pathlib import Path

payload = json.loads(Path(sys.argv[1]).read_text())
libraries = payload.get("libraries", [])
for lib in libraries:
    vendor = (lib.get("vendor") or "").strip()
    product = (lib.get("product") or "").strip()
    revision = (lib.get("revision") or "").strip()
    if vendor == "QuadStor" and product == "MSL G3 Series" and revision == "D.00":
        print("vtl")
        raise SystemExit(0)
print("real")
PY
  )"
  export REMFIELD_ENV="$detected"
  rm -f "$json_file"
}

fieldtest_selected_library_serial() {
  local path="$(
    fieldtest_state_dir
  )/selected-library"
  if [[ -f "$path" ]]; then
    cat "$path"
    return 0
  fi
  return 1
}

fieldtest_write_selected_library_serial() {
  local serial="$1"
  mkdir -p "$(fieldtest_state_dir)"
  printf '%s\n' "$serial" >"$(fieldtest_state_dir)/selected-library"
}

fieldtest_library_json() {
  local serial
  serial="${1:-}"
  if [[ -z "$serial" ]]; then
    echo "error: library serial required" >&2
    return 1
  fi
  local outfile
  outfile="$(mktemp)"
  fieldtest_capture_json "$outfile" "$(fieldtest_rem_bin)" library "$serial" --slots
}

fieldtest_list_libraries_json_file() {
  local outfile="$1"
  fieldtest_capture_json "$outfile" "$(fieldtest_rem_bin)" libraries --json
}

fieldtest_rem_endpoint() {
  printf 'unix:%s' "$(fieldtest_socket_path)"
}

fieldtest_require_pool_writable_tapes() {
  local pool="$1" required="$2" context="${3:-field test step}"
  local script="${SCRIPT_NAME:-fieldtest}"
  local inventory have
  inventory="$(fieldtest_artifact_path "$script" "media-${pool}" "$(fieldtest_timestamp_id)")"
  if ! fieldtest_capture_json "$inventory" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" list --pool "$pool"; then
    fieldtest_evidence_record "$script" media-budget FAIL "could not inspect pool $pool before $context; is the field daemon running?" "$inventory"
    return 1
  fi
  if ! have="$(
    python3 - "$inventory" <<'PY'
import json
import sys
from pathlib import Path

payload = json.loads(Path(sys.argv[1]).read_text())
ready = 0
for tape in payload.get("tapes", []):
    state = "".join(ch for ch in str(tape.get("state") or "").lower() if ch.isalnum())
    object_count = int(tape.get("object_count") or 0)
    last_file = tape.get("last_committed_tape_file")
    if last_file in (None, ""):
        last_file = 0
    try:
        last_file = int(last_file)
    except (TypeError, ValueError):
        last_file = 1
    if (state == "ready" or state.endswith("stateready")) and object_count == 0 and last_file == 0:
        ready += 1
print(ready)
PY
  )"; then
    fieldtest_evidence_record "$script" media-budget FAIL "could not parse pool inventory for $pool before $context" "$inventory"
    return 1
  fi
  if (( have < required )); then
    fieldtest_evidence_record "$script" media-budget FAIL "need ${required} unused ready tape(s) in $pool for $context; found $have. Add allowlisted scratch media and rerun 10-init-pools before bringup, or skip this phase." "$inventory"
    return 1
  fi
}

fieldtest_require_pool_appendable_tapes() {
  local pool="$1" required="$2" context="${3:-field test step}"
  local script="${SCRIPT_NAME:-fieldtest}"
  local inventory have
  inventory="$(fieldtest_artifact_path "$script" "appendable-media-${pool}" "$(fieldtest_timestamp_id)")"
  if ! fieldtest_capture_json "$inventory" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" list --pool "$pool"; then
    fieldtest_evidence_record "$script" media-budget FAIL "could not inspect pool $pool before $context; is the field daemon running?" "$inventory"
    return 1
  fi
  if ! have="$(
    python3 - "$inventory" <<'PY'
import json
import sys
from pathlib import Path

payload = json.loads(Path(sys.argv[1]).read_text())
ready = 0
for tape in payload.get("tapes", []):
    state = "".join(ch for ch in str(tape.get("state") or "").lower() if ch.isalnum())
    if state in {"ready", "tapestateready"}:
        ready += 1
print(ready)
PY
  )"; then
    fieldtest_evidence_record "$script" media-budget FAIL "could not parse pool inventory for $pool before $context" "$inventory"
    return 1
  fi
  if (( have < required )); then
    fieldtest_evidence_record "$script" media-budget FAIL "need ${required} appendable ready tape(s) in $pool for $context; found $have. Run 10-init-pools with allowlisted scratch media, bring the daemon back up, or skip this phase." "$inventory"
    return 1
  fi
  fieldtest_evidence_record "$script" media-budget PASS "found $have appendable ready tape(s) in $pool for $context" "$inventory"
}

fieldtest_run_with_lock() {
  local lockfile
  lockfile="$(fieldtest_work_lock)"
  mkdir -p "$(dirname -- "$lockfile")"
  exec 9>"$lockfile"
  flock -x 9
  "$@"
}

fieldtest_try_lock() {
  local lockfile
  lockfile="$(fieldtest_work_lock)"
  mkdir -p "$(dirname -- "$lockfile")"
  exec 9>"$lockfile"
  flock -n 9
}

fieldtest_release_lock() {
  flock -u 9 || true
}

fieldtest_confirm() {
  local prompt="${1:-Proceed?}"
  local answer
  read -r -p "$prompt [yes/no] " answer
  [[ "$answer" == yes || "$answer" == y || "$answer" == YES ]]
}

fieldtest_choose_library_serial() {
  local json_file="$1"
  local selected
  selected="$(
    python3 - "$json_file" <<'PY'
import json
import sys
from pathlib import Path

payload = json.loads(Path(sys.argv[1]).read_text())
libs = payload.get("libraries", [])
if not libs:
    raise SystemExit(1)
prefer = []
for lib in libs:
    product = (lib.get("product") or "").strip()
    revision = (lib.get("revision") or "").strip()
    if product == "MSL G3 Series" and revision == "D.00":
        prefer.append(lib)
if len(prefer) == 1:
    print(prefer[0]["serial"])
    raise SystemExit(0)
if len(libs) == 1:
    print(libs[0]["serial"])
    raise SystemExit(0)
raise SystemExit(2)
PY
  )" && {
    printf '%s\n' "$selected"
    return 0
  }
  return 1
}

fieldtest_interactive_choose_library() {
  local json_file="$1"
  local serials_file="$2"
  python3 - "$json_file" "$serials_file" <<'PY'
import json
import sys
from pathlib import Path

payload = json.loads(Path(sys.argv[1]).read_text())
libs = payload.get("libraries", [])
if not libs:
    raise SystemExit("no libraries discovered")
for idx, lib in enumerate(libs, start=1):
    print(
        f"{idx}. {lib.get('serial')}  {lib.get('vendor','').strip()} {lib.get('product','').strip()} "
        f"{lib.get('revision','').strip()}  drives={lib.get('drive_count')} slots={lib.get('slot_count')}"
    )
choice = input("Choose library number: ").strip()
try:
    idx = int(choice)
except ValueError as exc:
    raise SystemExit(f"invalid selection {choice!r}") from exc
if idx < 1 or idx > len(libs):
    raise SystemExit(f"selection {idx} out of range")
Path(sys.argv[2]).write_text(libs[idx - 1]["serial"] + "\n")
PY
}

fieldtest_sudo_surface_lines() {
  local bin
  bin="$(fieldtest_home)/bin/rem"
  printf 'sudo setcap cap_sys_rawio+ep %s\n' "$bin"
  printf 'sudo setcap cap_sys_rawio+ep %s\n' "$(fieldtest_home)/bin/rem-daemon"
  printf 'sudo setcap cap_sys_rawio+ep %s\n' "$(fieldtest_home)/bin/rem-debug"
  printf 'sudo setfacl -m u:%s:rw /dev/sg*\n' "$(id -un)"
}

fieldtest_selftest() {
  local tmpdir records bench_json allowlist config
  tmpdir="$(mktemp -d)"
  mkdir -p "$tmpdir/home/bin" "$tmpdir/home/state" "$tmpdir/home/spool" "$tmpdir/home/log" "$tmpdir/home/evidence"
  allowlist="$tmpdir/home/allowlist.txt"
  config="$tmpdir/home/config.toml"
  records="$tmpdir/home/evidence/records.jsonl"
  bench_json="$tmpdir/home/evidence/bench.csv"
  cat >"$allowlist" <<'EOF'
S20001L9
S20002L9
CLN:CLNU01L9
EOF
  cat >"$config" <<'EOF'
[daemon]
state_dir = "/tmp/remfield-selftest/state"
default_idle_timeout_seconds = 120
read_only = false
socket_path = "/tmp/remfield-selftest/rem.sock"

[[libraries]]
serial = "LIBMAIN"

[[tape_pools]]
id = "fieldtest-a"

[[tape_pool_rules]]
prefix = "S20001"
pool_id = "fieldtest-a"

[journal]
dir = "/tmp/remfield-selftest/journals"
require_trusted_volume = false

[audit]
dir = "/tmp/remfield-selftest/audit"
fsync = true

[index]
sqlite_path = "/tmp/remfield-selftest/index.sqlite"

[cache]
tape_catalog_dir = "/tmp/remfield-selftest/cache"
EOF
  cat >"$tmpdir/home/bin/rem" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == libraries && "${2:-}" == --json ]]; then
  cat <<'JSON'
{"libraries":[{"serial":"LIBMAIN","product":"MSL G3 Series","revision":"D.00","vendor":"QuadStor","drive_count":2,"slot_count":4,"loaded_slot_count":1,"ie_port_count":0}]}
JSON
  exit 0
fi
if [[ "${1:-}" == library ]]; then
  cat <<'TXT'
Library LIBMAIN
  Drives:
    [0x0001] drive one
  Slots:
    [0x03e9] full   S20001L9
    [0x03ea] full   CLNU01L9   (cleaning)
TXT
  exit 0
fi
echo "unexpected mock rem invocation: $*" >&2
exit 1
EOF
  chmod +x "$tmpdir/home/bin/rem"
  REMFIELD_HOME="$tmpdir/home"
  export REMFIELD_HOME
  if ! fieldtest_detect_env; then
    echo "selftest: detect_env failed" >&2
    return 1
  fi
  [[ "${REMFIELD_ENV:-}" == vtl ]]
  fieldtest_require_allowlisted S20001L9
  fieldtest_require_allowlisted CLNU01L9
  ! fieldtest_require_allowlisted BAD0001
  local record_path bench_path
  record_path="$(fieldtest_records_path)"
  fieldtest_evidence_record selftest t1 PASS "ok"
  [[ -f "$record_path" ]]
  bench_path="$(fieldtest_bench_csv_path)"
  fieldtest_bench_record write drive1 262144 compressible 123.4 1.25 1024
  [[ -f "$bench_path" ]]
  fieldtest_capture_json "$tmpdir/library.json" "$tmpdir/home/bin/rem" libraries --json
  [[ "$(fieldtest_json_get 'libraries[0].serial' <"$tmpdir/library.json")" == LIBMAIN ]]
  rm -rf "$tmpdir"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  case "${1:-}" in
    --selftest)
      shift
      fieldtest_selftest
      ;;
    --help|-h)
      cat <<'EOF'
fieldtest/scripts/lib.sh

This file is sourced by the field-test scripts.

Direct invocation:
  bash fieldtest/scripts/lib.sh --selftest
EOF
      ;;
    *)
      echo "error: lib.sh is a library; invoke with --selftest for the built-in checks" >&2
      exit 1
      ;;
  esac
fi

# Unload every loaded drive bay back to its home slot (benign robotics).
# Used before daemon start and after tape inits — `rem tape init` leaves the
# cartridge in the drive, which exhausts a 2-drive library by the 3rd tape.
fieldtest_drain_drives() {
  local serial="$1"
  local lib_json bays bay
  lib_json="$(mktemp)"
  "$(fieldtest_rem_bin)" library "$serial" --json >"$lib_json" 2>/dev/null || {
    rm -f "$lib_json"; return 0
  }
  bays="$(python3 - "$lib_json" <<'PY'
import json, sys
from pathlib import Path
lib = json.loads(Path(sys.argv[1]).read_text())
for d in lib.get("drives", []):
    if d.get("loaded"):
        print(d.get("element_address"))
PY
)"
  rm -f "$lib_json"
  for bay in $bays; do
    echo "draining drive bay $bay back to its home slot"
    "$(fieldtest_rem_debug_bin)" --allow "$serial" unload --bay "$bay" "$serial" || \
      echo "warn: could not drain bay $bay (continuing)" >&2
  done
}

fieldtest_write_config() {
  local config="$1" selected_serial="$2"
  python3 - "$config" "$selected_serial" <<'PY'
import json
import sys
from pathlib import Path

config = Path(sys.argv[1])
selected = sys.argv[2]
home = config.parent
allowlist_path = home / "allowlist.txt"
data_barcodes = []
cleaning = []
if allowlist_path.exists():
    for raw in allowlist_path.read_text().splitlines():
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        if line.startswith("CLN:"):
            cleaning.append(line.split(":", 1)[1].strip())
        else:
            data_barcodes.append(line)
mid = (len(data_barcodes) + 1) // 2
pool_a = data_barcodes[:mid]
pool_b = data_barcodes[mid:]

def rule_block(barcode, pool):
    return f'[[tape_pool_rules]]\nprefix = "{barcode}"\npool_id = "{pool}"\n'

parts = [
    "[daemon]",
    f'state_dir = "{home / "state"}"',
    "default_idle_timeout_seconds = 120",
    "read_only = false",
    f'socket_path = "{home / "rem.sock"}"',
    "",
    "[[libraries]]",
    f'serial = "{selected}"',
    "",
    "[[tape_pools]]",
    'id = "fieldtest-a"',
    'display_name = "fieldtest-a"',
    'copy_class = "copy-a"',
    'content_class = "fieldtest"',
    'block_size = "256KiB"',
    "",
    "[[tape_pools]]",
    'id = "fieldtest-b"',
    'display_name = "fieldtest-b"',
    'copy_class = "copy-b"',
    'content_class = "fieldtest"',
    'block_size = "256KiB"',
    "",
]
for barcode in pool_a:
    parts.append(rule_block(barcode, "fieldtest-a"))
for barcode in pool_b:
    parts.append(rule_block(barcode, "fieldtest-b"))
parts.extend([
    "[drives]",
    f'managed_libraries = ["{selected}"]',
    "foreign_counter_poll = \"60m\"",
    "foreign_tapealert = false",
    "heartbeat = \"1h\"",
    "snapshot_miss_alarm = 3",
    "",
    "[cleaning]",
    "auto = true",
    "voltag_prefixes = [\"CLN\"]",
    "use_warn = 45",
    "complete_timeout = \"10m\"",
    "min_cycle_duration = \"60s\"",
    "min_interval = \"12h\"",
    "weekly_cap = 4",
    "",
    "[livestatus]",
    "min_poll_interval = \"250ms\"",
    "foreign_changer_poll = \"60s\"",
    "foreign_poll_lease = \"5m\"",
    "",
    "[journal]",
    f'dir = "{home / "state" / "journals"}"',
    "require_trusted_volume = false",
    "",
    "[audit]",
    f'dir = "{home / "state" / "audit"}"',
    "fsync = true",
    "clock_forward_tolerance_seconds = 300",
    "",
    "[index]",
    f'sqlite_path = "{home / "state" / "index" / "rem-state.sqlite"}"',
    "",
    "[cache]",
    f'tape_catalog_dir = "{home / "state" / "cache" / "tapes"}"',
    "",
])
config.write_text("\n".join(parts))
PY
}
