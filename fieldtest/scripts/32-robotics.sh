#!/usr/bin/env bash
set -euo pipefail

SCRIPT_NAME="32-robotics"
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

usage() {
  cat <<'EOF'
Usage: 32-robotics.sh [--help]

Moves an allowlisted cartridge slot-to-slot and, when IE ports exist, exercises
export/import with a physical operator prompt.
EOF
}

slot_inventory() {
  local slots_file="$1"
  python3 - "$slots_file" <<'PY'
import json, re, sys
from pathlib import Path

payload=json.loads(Path(sys.argv[1]).read_text())
text=payload.get("stdout","")
slot_re = re.compile(r"^\s*(0x[0-9a-fA-F]{4}|\d+)\s+.*?([A-Z0-9]{6,10})\s*$")
for line in text.splitlines():
    m = slot_re.search(line)
    if m:
        print(f"{m.group(1)} {m.group(2)}")
PY
}

main() {
  if [[ "${1:-}" == --help || "${1:-}" == -h ]]; then
    usage
    exit 0
  fi

  fieldtest_init_layout
  fieldtest_detect_env || true
  local serial
  serial="$(fieldtest_selected_library_serial)"
  if [[ -z "$serial" ]]; then
    echo "error: no selected library; run bringup first" >&2
    exit 1
  fi

  local stamp slots_path libs_json ie_count source_slot source_barcode dest_slot
  stamp="$(fieldtest_timestamp_id)"
  libs_json="$(fieldtest_artifact_path "$SCRIPT_NAME" libraries "$stamp")"
  fieldtest_capture_json "$libs_json" "$(fieldtest_rem_bin)" libraries --json
  ie_count="$(python3 - "$libs_json" <<'PY'
import json,sys
from pathlib import Path
payload=json.loads(Path(sys.argv[1]).read_text())
libraries = payload.get("libraries", [])
print(libraries[0].get("ie_port_count", 0) if libraries else 0)
PY
)"
  slots_path="$(fieldtest_artifact_path "$SCRIPT_NAME" slots "$stamp")"
  fieldtest_capture_json "$slots_path" "$(fieldtest_rem_bin)" top --endpoint "unix:$(fieldtest_socket_path)" --once --json
  if [[ -z "$(fieldtest_allowlist_barcodes)" ]]; then
    echo "error: allowlist is empty" >&2
    exit 1
  fi
  read -r source_slot source_barcode dest_slot <<<"$(python3 - "$slots_path" "$(fieldtest_allowlist_path)" "$serial" <<'PY'
import json
import sys
from pathlib import Path

top = json.loads(Path(sys.argv[1]).read_text())
serial = sys.argv[3] if len(sys.argv) > 3 else ""
allow = {line.split("#", 1)[0].strip() for line in Path(sys.argv[2]).read_text().splitlines() if line.split("#", 1)[0].strip()}
allow = {a.split(":", 1)[1].strip() if a.startswith("CLN:") else a for a in allow}
allowlisted = []
empty = []
for lib in top["data"]["libraries"]:
    if serial and lib.get("library", {}).get("library_serial") != serial:
        continue
    for slot in lib.get("slots", []):
        addr = slot.get("element_address")
        addr_hex = f"0x{addr:04x}" if isinstance(addr, int) else str(addr)
        voltag = (slot.get("voltag") or "").strip()
        if voltag and voltag in allow:
            allowlisted.append((addr_hex, voltag))
        if not voltag:
            empty.append(addr_hex)
if not allowlisted:
    raise SystemExit(1)
print(allowlisted[0][0], allowlisted[0][1], empty[0] if empty else "")
PY
)"
  if [[ -z "${source_slot:-}" || -z "${source_barcode:-}" ]]; then
    fieldtest_evidence_record "$SCRIPT_NAME" robotics FAIL "could not find an allowlisted cartridge in the slot inventory" "$slots_path"
    exit 1
  fi
  fieldtest_require_allowlisted "$source_barcode"
  if [[ -z "$dest_slot" ]]; then
    fieldtest_evidence_record "$SCRIPT_NAME" robotics FAIL "no empty slot was available for the robotics move" "$slots_path"
    exit 1
  fi

  # rem-debug move is direct-SCSI: stop the daemon around it, restart after,
  # so its cached inventory is rebuilt from truth.
  "$(fieldtest_script_dir)/03-bringup.sh" --stop >/dev/null 2>&1 || true
  local move_json
  move_json="$(fieldtest_artifact_path "$SCRIPT_NAME" move "$stamp")"
  if ! fieldtest_capture_json "$move_json" "$(fieldtest_rem_debug_bin)" --allow "$serial" move "$serial" --src "$source_slot" --dst "$dest_slot"; then
    fieldtest_evidence_record "$SCRIPT_NAME" move FAIL "rem-debug move failed for $source_barcode" "$move_json"
    exit 1
  fi
  "$(fieldtest_script_dir)/03-bringup.sh" >/dev/null 2>&1 || true
  sleep 2
  fieldtest_capture_json "$slots_path" "$(fieldtest_rem_bin)" top --endpoint "unix:$(fieldtest_socket_path)" --once --json
  if ! grep -Fq "$source_barcode" "$slots_path"; then
    fieldtest_evidence_record "$SCRIPT_NAME" move FAIL "moved barcode $source_barcode not visible after slot-to-slot move" "$slots_path"
    exit 1
  fi
  fieldtest_evidence_record "$SCRIPT_NAME" move PASS "slot-to-slot move completed for allowlisted barcode $source_barcode" "$move_json"

  if [[ "$ie_count" -gt 0 ]]; then
    if fieldtest_confirm "🖐 export/import via IE port now?"; then
      local export_json import_json
      export_json="$(fieldtest_artifact_path "$SCRIPT_NAME" ie-export "$stamp")"
      import_json="$(fieldtest_artifact_path "$SCRIPT_NAME" ie-import "$stamp")"
      "$(fieldtest_script_dir)/03-bringup.sh" --stop >/dev/null 2>&1 || true
      fieldtest_capture_json "$export_json" "$(fieldtest_rem_debug_bin)" --allow "$serial" export "$serial" --slot "$dest_slot"
      echo "remove the tape from the IE port, reinsert it, then press Enter to continue" >&2
      read -r _
      fieldtest_capture_json "$import_json" "$(fieldtest_rem_debug_bin)" --allow "$serial" import "$serial" --slot "$source_slot"
      "$(fieldtest_script_dir)/03-bringup.sh" >/dev/null 2>&1 || true
      fieldtest_evidence_record "$SCRIPT_NAME" ie PASS "export/import cycle completed for allowlisted tape" "$import_json"
    fi
  fi
}

fieldtest_run_with_lock main "$@"
