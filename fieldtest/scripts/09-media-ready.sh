#!/usr/bin/env bash
set -euo pipefail

SCRIPT_NAME="09-media-ready"
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

usage() {
  cat <<'EOF'
Usage: 09-media-ready.sh [--resume UUID | --barcode BARCODE | --drive-element 0xNNNN] [--timeout 2.5h] [--poll 30s] [--no-wait]

Polls TEST UNIT READY for media that is already loaded in a drive. This script
does not move cartridges. Use it after the library UI shows Calib/initializing,
or after a tape init run stops with a media-readiness blocker.
EOF
}

main() {
  local resume="" barcode="" drive_element="" timeout="2.5h" poll="30s" wait_flag="--wait"
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --resume)
        resume="${2:?missing operation id}"
        shift 2
        ;;
      --barcode)
        barcode="${2:?missing barcode}"
        shift 2
        ;;
      --drive-element)
        drive_element="${2:?missing drive element}"
        shift 2
        ;;
      --timeout)
        timeout="${2:?missing timeout}"
        shift 2
        ;;
      --poll)
        poll="${2:?missing poll}"
        shift 2
        ;;
      --no-wait)
        wait_flag=""
        shift
        ;;
      --help|-h)
        usage
        exit 0
        ;;
      *)
        echo "error: unknown argument $1" >&2
        usage >&2
        exit 2
        ;;
    esac
  done

  local target_count=0
  [[ -n "$resume" ]] && target_count=$((target_count + 1))
  [[ -n "$barcode" ]] && target_count=$((target_count + 1))
  [[ -n "$drive_element" ]] && target_count=$((target_count + 1))
  if [[ "$target_count" -gt 1 ]]; then
    echo "error: use only one of --resume, --barcode, or --drive-element" >&2
    exit 2
  fi
  if [[ "$target_count" -eq 0 ]]; then
    echo "error: provide --resume, --barcode, or --drive-element" >&2
    exit 2
  fi

  fieldtest_init_layout
  fieldtest_detect_env || true

  local serial stamp out rc target_label
  serial="$(fieldtest_selected_library_serial 2>/dev/null || true)"
  if [[ -z "$serial" ]]; then
    echo "error: no selected library; run 00-preflight.sh or set FIELDTEST_LIBRARY_SERIAL" >&2
    exit 1
  fi
  if [[ ! -f "$(fieldtest_config_path)" ]]; then
    echo "error: missing config $(fieldtest_config_path); run 10-init-pools.sh once to create it" >&2
    exit 1
  fi

  stamp="$(fieldtest_timestamp_id)"
  target_label="${resume:-${barcode:-$drive_element}}"
  out="$(fieldtest_artifact_path "$SCRIPT_NAME" "wait-ready-${target_label//\//_}" "$stamp")"

  set +e
  if [[ -n "$resume" ]]; then
    fieldtest_capture_text "$out" "$(fieldtest_rem_bin)" tape wait-ready --config "$(fieldtest_config_path)" --library "$serial" --resume "$resume" $wait_flag --timeout "$timeout" --poll "$poll" --json
    rc=$?
  elif [[ -n "$barcode" ]]; then
    fieldtest_capture_text "$out" "$(fieldtest_rem_bin)" tape wait-ready --config "$(fieldtest_config_path)" --library "$serial" --barcode "$barcode" $wait_flag --timeout "$timeout" --poll "$poll" --json
    rc=$?
  else
    fieldtest_capture_text "$out" "$(fieldtest_rem_bin)" tape wait-ready --config "$(fieldtest_config_path)" --library "$serial" --drive-element "$drive_element" --already-loaded $wait_flag --timeout "$timeout" --poll "$poll" --json
    rc=$?
  fi
  set -e

  case "$rc" in
    0)
      fieldtest_evidence_record "$SCRIPT_NAME" wait-ready PASS "media ready for $target_label" "$out"
      ;;
    10)
      fieldtest_evidence_record "$SCRIPT_NAME" wait-ready INFO "media still initializing for $target_label; rerun with --timeout if needed" "$out"
      ;;
    20)
      fieldtest_evidence_record "$SCRIPT_NAME" wait-ready FAIL "timed out waiting for media readiness for $target_label" "$out"
      exit 1
      ;;
    40)
      fieldtest_evidence_record "$SCRIPT_NAME" wait-ready FAIL "transport unknown while waiting for media readiness for $target_label" "$out"
      exit 1
      ;;
    50)
      fieldtest_evidence_record "$SCRIPT_NAME" wait-ready FAIL "reservation conflict while waiting for media readiness for $target_label" "$out"
      exit 1
      ;;
    *)
      fieldtest_evidence_record "$SCRIPT_NAME" wait-ready FAIL "media readiness failed for $target_label (rc=$rc)" "$out"
      exit 1
      ;;
  esac
}

fieldtest_run_with_lock main "$@"
