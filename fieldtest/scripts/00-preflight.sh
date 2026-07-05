#!/usr/bin/env bash
set -euo pipefail

SCRIPT_NAME="00-preflight"
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

usage() {
  cat <<'EOF'
Usage: 00-preflight.sh [--help]

Probes binaries, /dev/sg* visibility, disk free, and spool-volume read speed.
Writes evidence under $REMFIELD_HOME/evidence/ and exits nonzero on hard blockers.
EOF
}

latest_artifact() {
  fieldtest_latest_artifact_path "$SCRIPT_NAME" "$1"
}

record_artifact() {
  local name="$1" path="$2"
  cp -f -- "$path" "$(latest_artifact "$name")"
}

has_preselected_library() {
  [[ -n "${FIELDTEST_LIBRARY_SERIAL:-}" ]] && return 0
  fieldtest_selected_library_serial >/dev/null 2>&1
}

preselected_library_serial() {
  local json_file="$1" selected source
  if [[ -n "${FIELDTEST_LIBRARY_SERIAL:-}" ]]; then
    selected="$FIELDTEST_LIBRARY_SERIAL"
    source="FIELDTEST_LIBRARY_SERIAL"
  else
    selected="$(fieldtest_selected_library_serial)"
    source="$(fieldtest_state_dir)/selected-library"
  fi

  python3 - "$json_file" "$selected" "$source" <<'PY'
import json
import sys
from pathlib import Path

payload = json.loads(Path(sys.argv[1]).read_text())
selected = sys.argv[2].strip()
source = sys.argv[3]
serials = {str(lib.get("serial", "")).strip() for lib in payload.get("libraries", [])}
if selected not in serials:
    found = ", ".join(sorted(serials)) or "(none)"
    raise SystemExit(f"{source} selects {selected!r}, but discovered libraries are: {found}")
print(selected)
PY
}

main() {
  if [[ "${1:-}" == --help || "${1:-}" == -h ]]; then
    usage
    exit 0
  fi

  fieldtest_init_layout
  fieldtest_detect_env || true

  local stamp bin rem_help_path
  stamp="$(fieldtest_timestamp_id)"
  bin="$(fieldtest_rem_bin)"

  rem_help_path="$(fieldtest_artifact_path "$SCRIPT_NAME" rem-help "$stamp")"
  mkdir -p "$(dirname -- "$rem_help_path")"
  if ! fieldtest_capture_text "$rem_help_path" "$bin" --help; then
    local help_text
    help_text="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["stderr"])' "$rem_help_path" 2>/dev/null || true)"
    fieldtest_evidence_record "$SCRIPT_NAME" rem-help FAIL "rem --help did not execute"
    if [[ "$help_text" == *GLIBC* || "$help_text" == *glibc* ]]; then
      printf '%s\n' "plan B: use bin/musl/rem if present; plan C: build from toolchain/README.md" >&2
    fi
    exit 1
  fi
  record_artifact rem-help "$rem_help_path"
  fieldtest_evidence_record "$SCRIPT_NAME" rem-help PASS "rem binary executes and prints help" "$rem_help_path"

  local sg_list
  sg_list="$(compgen -G '/dev/sg*' || true)"
  if [[ -z "$sg_list" ]]; then
    fieldtest_evidence_record "$SCRIPT_NAME" sg-devices FAIL "no /dev/sg* devices are visible"
    printf '%s\n' "hardware blocker: no /dev/sg* nodes found; check SAS wiring and kernel device enumeration" >&2
    exit 1
  fi

  local rem_libraries_json
  rem_libraries_json="$(fieldtest_artifact_path "$SCRIPT_NAME" rem-libraries "$stamp")"
  mkdir -p "$(dirname -- "$rem_libraries_json")"
  if ! fieldtest_capture_json "$rem_libraries_json" "$bin" libraries --json; then
    local stderr_text
    stderr_text="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["stderr"])' "$rem_libraries_json")"
    if [[ "$stderr_text" == *EPERM* || "$stderr_text" == *"Operation not permitted"* || "$stderr_text" == *"Permission denied"* || "$stderr_text" == *"os error 13"* ]]; then
      fieldtest_evidence_record "$SCRIPT_NAME" sg-permissions FAIL "rem libraries hit a SCSI permission denial" "$rem_libraries_json"
      fieldtest_sudo_surface_lines >&2
    else
      fieldtest_evidence_record "$SCRIPT_NAME" sg-enumeration FAIL "rem libraries failed before discovery" "$rem_libraries_json"
      printf '%s\n' "hardware blocker: rem libraries failed; inspect $rem_libraries_json" >&2
    fi
    exit 1
  fi
  record_artifact rem-libraries "$rem_libraries_json"

  local library_count
  library_count="$(fieldtest_json_get 'libraries' <"$rem_libraries_json" | python3 -c 'import json,sys; print(len(json.loads(sys.stdin.read())))')"
  if [[ "$library_count" -eq 0 ]]; then
    fieldtest_evidence_record "$SCRIPT_NAME" libraries FAIL "rem libraries returned no libraries"
    printf '%s\n' "hardware blocker: discovery returned no libraries" >&2
    exit 1
  fi

  local selected_serial selected_library_json
  selected_serial=""
  if has_preselected_library; then
    if ! selected_serial="$(preselected_library_serial "$rem_libraries_json")"; then
      fieldtest_evidence_record "$SCRIPT_NAME" library-selection FAIL "preselected library is not currently discoverable" "$rem_libraries_json"
      exit 1
    fi
  fi
  if [[ -z "$selected_serial" ]]; then
    selected_serial="$(fieldtest_choose_library_serial "$rem_libraries_json" || true)"
  fi
  if [[ -z "$selected_serial" ]]; then
    if [[ -t 0 ]]; then
      local choice_file
      choice_file="$(mktemp)"
      fieldtest_interactive_choose_library "$rem_libraries_json" "$choice_file"
      selected_serial="$(cat "$choice_file")"
      rm -f "$choice_file"
    else
      selected_serial="$(python3 - "$rem_libraries_json" <<'PY'
import json,sys
from pathlib import Path
payload=json.loads(Path(sys.argv[1]).read_text())
print(payload["libraries"][0]["serial"])
PY
)"
    fi
  fi
  fieldtest_write_selected_library_serial "$selected_serial"

  selected_library_json="$(fieldtest_artifact_path "$SCRIPT_NAME" library-slots "$stamp")"
  if ! fieldtest_capture_text "$selected_library_json" "$bin" library "$selected_serial" --slots; then
    fieldtest_evidence_record "$SCRIPT_NAME" library-slots FAIL "rem library $selected_serial --slots failed" "$selected_library_json"
    exit 1
  fi
  record_artifact library-slots "$selected_library_json"

  local spool_dir required_gb free_bytes
  spool_dir="$(fieldtest_spool_dir)"
  required_gb="${FIELD_PRELIGHT_REQUIRED_GB:-8}"
  free_bytes="$(df -Pk "$spool_dir" | awk 'NR==2 {print $4 * 1024}')"
  if [[ "${free_bytes:-0}" -lt $((required_gb * 1024 * 1024 * 1024)) ]]; then
    fieldtest_evidence_record "$SCRIPT_NAME" spool-free FAIL "spool filesystem has less than ${required_gb}GiB free"
    printf '%s\n' "hard blocker: spool volume does not have ${required_gb}GiB free" >&2
    exit 1
  fi

  local sample_gb sample_file read_start read_end read_seconds read_mbps sample_bytes disk_speed_json
  sample_gb="${FIELD_PRELIGHT_SAMPLE_GB:-4}"
  sample_file="$spool_dir/preflight-${stamp}.bin"
  sample_bytes="$(python3 - "$sample_gb" <<'PY'
import sys
gb=float(sys.argv[1])
print(int(gb * 1024 * 1024 * 1024))
PY
)"
  read_start="$(python3 -c 'import time; print(f"{time.monotonic():.9f}")')"
  python3 - "$sample_file" "$sample_bytes" <<'PY'
import os
import sys
from pathlib import Path

path = Path(sys.argv[1])
bytes_needed = int(sys.argv[2])
chunk = 4 * 1024 * 1024
path.parent.mkdir(parents=True, exist_ok=True)
with path.open("wb") as handle:
    remaining = bytes_needed
    zeros = b"\0" * chunk
    while remaining > 0:
        n = min(chunk, remaining)
        handle.write(zeros[:n])
        remaining -= n
PY
  sync
  dd if="$sample_file" of=/dev/null bs=16M status=none
  read_end="$(python3 -c 'import time; print(f"{time.monotonic():.9f}")')"
  read_seconds="$(python3 - "$read_start" "$read_end" <<'PY'
import sys
print(float(sys.argv[2]) - float(sys.argv[1]))
PY
)"
  read_mbps="$(python3 - "$sample_bytes" "$read_seconds" <<'PY'
import sys
bytes_count = int(sys.argv[1])
seconds = float(sys.argv[2])
print(f"{(bytes_count / seconds) / (1024 * 1024):.2f}" if seconds > 0 else "inf")
PY
)"
  disk_speed_json="$(fieldtest_artifact_path "$SCRIPT_NAME" disk-read-speed "$stamp")"
  python3 - "$disk_speed_json" "$sample_file" "$sample_bytes" "$read_seconds" "$read_mbps" <<'PY'
import json
import sys
from pathlib import Path

outfile, sample_file, bytes_count, seconds, mbps = sys.argv[1:]
Path(outfile).write_text(
    json.dumps(
        {
            "method": "dd if=sample of=/dev/null bs=16M; drop-caches not required",
            "sample_file": sample_file,
            "bytes": int(bytes_count),
            "seconds": float(seconds),
            "MB_s": float(mbps),
        },
        indent=2,
        sort_keys=True,
    )
    + "\n"
)
PY
  record_artifact disk-read-speed "$disk_speed_json"
  if python3 - "$read_mbps" <<'PY'
import sys
raise SystemExit(0 if float(sys.argv[1]) >= 300.0 else 1)
PY
  then
    fieldtest_evidence_record "$SCRIPT_NAME" spool-speed PASS "spool read speed ${read_mbps} MB/s" "$disk_speed_json"
  else
    fieldtest_evidence_record "$SCRIPT_NAME" spool-speed INFO "spool read speed ${read_mbps} MB/s is below tape rate; benchmark scripts should switch to RAM-backed source" "$disk_speed_json"
  fi

  local cpu_json mem_json os_json
  cpu_json="$(fieldtest_artifact_path "$SCRIPT_NAME" cpu "$stamp")"
  mem_json="$(fieldtest_artifact_path "$SCRIPT_NAME" mem "$stamp")"
  os_json="$(fieldtest_artifact_path "$SCRIPT_NAME" os-release "$stamp")"
  fieldtest_capture_text "$cpu_json" uname -a || true
  fieldtest_capture_text "$mem_json" free -h || true
  fieldtest_capture_text "$os_json" cat /etc/os-release || true
  record_artifact cpu "$cpu_json"
  record_artifact mem "$mem_json"
  record_artifact os-release "$os_json"
  fieldtest_evidence_record "$SCRIPT_NAME" host-info PASS "captured kernel, memory, and os-release snapshots"

  rm -f -- "$sample_file"
}

fieldtest_run_with_lock main "$@"
