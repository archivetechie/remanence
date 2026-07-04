#!/usr/bin/env bash
set -euo pipefail

SCRIPT_NAME="91-cleanup"
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

usage() {
  cat <<'EOF'
Usage: 91-cleanup.sh [--state-only|--purge|--help]

Stops the daemon, prints the sudo surface needed for cleanup, and shows tape
disposition information from the catalog.
EOF
}

catalog_tapes() {
  local endpoint="$1" out="$2"
  fieldtest_capture_json "$out" "$(fieldtest_rem_bin)" catalog tapes --endpoint "$endpoint" --json || true
}

main() {
  local state_only=0 purge=0
  case "${1:-}" in
    --help|-h)
      usage
      exit 0
      ;;
    --state-only)
      state_only=1
      shift
      ;;
    --purge)
      purge=1
      shift
      ;;
  esac

  fieldtest_init_layout
  local endpoint serial tapes_json stamp
  endpoint="$(fieldtest_rem_endpoint)"
  serial="$(fieldtest_selected_library_serial || true)"
  stamp="$(fieldtest_timestamp_id)"
  tapes_json="$(fieldtest_artifact_path "$SCRIPT_NAME" tapes "$stamp")"
  if [[ -n "$serial" ]]; then
    catalog_tapes "$endpoint" "$tapes_json"
  fi
  "$(fieldtest_script_dir)/03-bringup.sh" --stop >/dev/null 2>&1 || true

  printf '%s\n' "sudo setcap -r $(fieldtest_home)/bin/rem"
  printf '%s\n' "sudo setcap -r $(fieldtest_home)/bin/rem-daemon"
  printf '%s\n' "sudo setcap -r $(fieldtest_home)/bin/rem-debug"
  printf 'sudo setfacl -x u:%s /dev/sg*\n' "$(id -un)"

  if [[ -f "$tapes_json" ]]; then
    python3 - "$tapes_json" <<'PY'
import json,sys
from pathlib import Path
payload=json.loads(Path(sys.argv[1]).read_text())
for tape in payload.get("data", {}).get("tapes", []):
    print(f"{tape.get('voltag','?')} state={tape.get('state','?')} pool={tape.get('pool_id','?')} block_size={tape.get('block_size_bytes','?')}")
PY
  fi

  if [[ $state_only -eq 1 || $purge -eq 1 ]]; then
    rm -rf -- "$(fieldtest_state_dir)"
  fi
  if [[ $purge -eq 1 ]]; then
    rm -rf -- "$(fieldtest_spool_dir)" "$(fieldtest_log_dir)"
  fi
  fieldtest_evidence_record "$SCRIPT_NAME" cleanup PASS "daemon stopped and cleanup surface printed"
}

fieldtest_run_with_lock main "$@"
