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

main() {
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
  local idx init_out init_level init_bay catalog_json ok=0
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
    else
      if init_media_not_ready "$init_out"; then
        fieldtest_evidence_record "$SCRIPT_NAME" "init-${data_barcodes[$idx]}" FAIL "media not ready/calibrating for ${data_barcodes[$idx]}; wait for the library UI to leave Calib/initializing, then rerun 10-init-pools" "$init_out"
        exit 1
      fi
    fi
    if ! fieldtest_capture_text "$init_out" "$(fieldtest_rem_bin)" --allow "$serial" tape init "${data_barcodes[$idx]}" --config "$(fieldtest_config_path)" --library "$serial"; then
      if init_media_not_ready "$init_out"; then
        fieldtest_evidence_record "$SCRIPT_NAME" "init-${data_barcodes[$idx]}" FAIL "media not ready/calibrating for ${data_barcodes[$idx]}; wait for the library UI to leave Calib/initializing, then rerun 10-init-pools" "$init_out"
        exit 1
      fi
      init_level="force"
      if ! fieldtest_capture_text "$init_out" "$(fieldtest_rem_bin)" --allow "$serial" tape init "${data_barcodes[$idx]}" --config "$(fieldtest_config_path)" --library "$serial" --force; then
        if init_media_not_ready "$init_out"; then
          fieldtest_evidence_record "$SCRIPT_NAME" "init-${data_barcodes[$idx]}" FAIL "media not ready/calibrating for ${data_barcodes[$idx]}; wait for the library UI to leave Calib/initializing, then rerun 10-init-pools" "$init_out"
          exit 1
        fi
        init_level="clobber-data"
        if ! fieldtest_capture_text "$init_out" "$(fieldtest_rem_bin)" --allow "$serial" tape init "${data_barcodes[$idx]}" --config "$(fieldtest_config_path)" --library "$serial" --clobber-data; then
          if init_media_not_ready "$init_out"; then
            fieldtest_evidence_record "$SCRIPT_NAME" "init-${data_barcodes[$idx]}" FAIL "media not ready/calibrating for ${data_barcodes[$idx]}; wait for the library UI to leave Calib/initializing, then rerun 10-init-pools" "$init_out"
            exit 1
          fi
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

  ok=1
  fieldtest_evidence_record "$SCRIPT_NAME" init-summary PASS "initialized $count allowlisted tapes (catalog visibility verified after bringup)"

  if [[ -n "$cleaning_barcode" ]]; then
    fieldtest_evidence_record "$SCRIPT_NAME" cleaning INFO "allowlist also contains CLN:$cleaning_barcode"
  fi
}

fieldtest_run_with_lock main "$@"
