#!/usr/bin/env bash
set -euo pipefail

SCRIPT_NAME="01-allowlist"
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

usage() {
  cat <<'EOF'
Usage: 01-allowlist.sh [--help]

Shows the current slot inventory and interactively records the barcode allowlist.
EOF
}

main() {
  if [[ "${1:-}" == --help || "${1:-}" == -h ]]; then
    usage
    exit 0
  fi

  fieldtest_init_layout
  fieldtest_detect_env || true

  local libs_json selected_serial slots_path stamp allowlist_path slots_text
  stamp="$(fieldtest_timestamp_id)"
  libs_json="$(fieldtest_artifact_path "$SCRIPT_NAME" libraries "$stamp")"
  fieldtest_capture_json "$libs_json" "$(fieldtest_rem_bin)" libraries --json

  selected_serial="$(fieldtest_selected_library_serial || true)"
  if [[ -z "$selected_serial" ]]; then
    if [[ -t 0 ]]; then
      local choice_file
      choice_file="$(mktemp)"
      fieldtest_interactive_choose_library "$libs_json" "$choice_file"
      selected_serial="$(cat "$choice_file")"
      rm -f "$choice_file"
    else
      # Non-interactive: NEVER guess between libraries. Take an explicit
      # env override, or auto-select only when exactly one library exists.
      selected_serial="$(python3 - "$libs_json" <<'PY'
import json, os, sys
from pathlib import Path
payload = json.loads(Path(sys.argv[1]).read_text())
libs = payload["libraries"]
override = os.environ.get("FIELDTEST_LIBRARY_SERIAL", "").strip()
if override:
    if override not in {l["serial"] for l in libs}:
        sys.exit(f"FIELDTEST_LIBRARY_SERIAL={override} not among discovered libraries")
    print(override)
elif len(libs) == 1:
    print(libs[0]["serial"])
else:
    serials = ", ".join(f'{l["serial"]} ({l.get("vendor","")} {l.get("product","")}, {l.get("drives","?")} drives)' for l in libs)
    sys.exit(f"multiple libraries discovered [{serials}]; set FIELDTEST_LIBRARY_SERIAL to choose non-interactively")
PY
)"
    fi
    fieldtest_write_selected_library_serial "$selected_serial"
  fi

  slots_path="$(fieldtest_artifact_path "$SCRIPT_NAME" slots "$stamp")"
  fieldtest_capture_text "$slots_path" "$(fieldtest_rem_bin)" library "$selected_serial" --slots

  slots_text="$(python3 - "$slots_path" <<'PY'
import json,sys
from pathlib import Path
print(json.loads(Path(sys.argv[1]).read_text())["stdout"])
PY
)"

  printf '%s\n' "Slot inventory for $selected_serial:"
  printf '%s\n' "$slots_text"
  printf '%s\n' "Every barcode entered here can be erased or reinitialized by this kit."
  printf '%s\n' "Do not include production media."

  local barcodes=()
  while true; do
    local barcode confirm
    read -r -p "scratch barcode [blank to finish]: " barcode
    barcode="${barcode//$'\r'/}"
    barcode="${barcode#"${barcode%%[![:space:]]*}"}"
    barcode="${barcode%"${barcode##*[![:space:]]}"}"
    if [[ -z "$barcode" ]]; then
      break
    fi
    read -r -p "confirm $barcode? [yes/no]: " confirm
    if [[ "$confirm" != yes && "$confirm" != y && "$confirm" != YES ]]; then
      printf '%s\n' "skipping $barcode"
      continue
    fi
    barcodes+=("$barcode")
  done

  local cleaning=""
  read -r -p "cleaning cartridge barcode [blank if none]: " cleaning || true
  cleaning="${cleaning//$'\r'/}"
  cleaning="${cleaning#"${cleaning%%[![:space:]]*}"}"
  cleaning="${cleaning%"${cleaning##*[![:space:]]}"}"

  printf '%s\n' "Proposed allowlist:"
  for barcode in "${barcodes[@]}"; do
    printf '%s\n' "$barcode"
  done
  if [[ -n "$cleaning" ]]; then
    printf 'CLN:%s\n' "$cleaning"
  fi
  printf '%s\n' "Everything above is destruction-eligible."
  read -r -p "Type DESTROY to write the allowlist: " verdict
  if [[ "$verdict" != DESTROY ]]; then
    echo "error: allowlist not written" >&2
    exit 1
  fi

  allowlist_path="$(fieldtest_allowlist_path)"
  {
    for barcode in "${barcodes[@]}"; do
      printf '%s\n' "$barcode"
    done
    if [[ -n "$cleaning" ]]; then
      printf 'CLN:%s\n' "$cleaning"
    fi
  } >"$allowlist_path"

  local latest
  latest="$(fieldtest_latest_artifact_path "$SCRIPT_NAME" allowlist)"
  cp -f -- "$allowlist_path" "$latest"
  fieldtest_evidence_record "$SCRIPT_NAME" allowlist PASS "wrote ${#barcodes[@]} scratch barcode(s) and ${cleaning:+one cleaning barcode}" "$latest"
}

fieldtest_run_with_lock main "$@"
