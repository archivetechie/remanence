#!/usr/bin/env bash
# Owner-run session epoch fault gates. All evidence survives workdir cleanup.
set -euo pipefail
SCRIPT_NAME=41-session-closeout
scripts="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
source "$scripts/lib.sh"
main() {
  set -euo pipefail
  if [[ ${1:-} == --help ]]; then
    echo 'Usage: 41-session-closeout.sh MODE --barcode AOX... [--live-control]'
    return
  fi
  [[ $# == 3 || $# == 4 ]] || { echo 'expected mode --barcode BARCODE' >&2; return 2; }
  mode=$1 barcode=$3 control=${4:-}
  case "$mode" in kill-mid-write|abort-mid-object|failed-append|measure-full-epoch) ;; *) return 2;; esac
  [[ $2 == --barcode && ( -z $control || $control == --live-control ) ]] || return 2
  [[ -z $control || $mode == measure-full-epoch ]] || return 2
  case "$barcode" in AOX032L9|AOX036L9|AOX038L9|AOX058L9) ;; *) echo 'barcode outside leg A allowlist' >&2; return 2;; esac
  fieldtest_require_allowlisted "$barcode" || return $?
  [[ ${FIELD_SESSION_EPOCH_ARM:-} == 'session-epoch-a' ]] || { echo 'owner must arm FIELD_SESSION_EPOCH_ARM=session-epoch-a' >&2; return 2; }
  # Trap arguments remain in scope after main returns.
  run_id=${FIELD_SESSION_RUN_ID:-manual}
  [[ $run_id =~ ^[A-Za-z0-9_-][A-Za-z0-9_.-]*$ ]] || return 2
  root="$(fieldtest_evidence_dir)/41-session-closeout/$run_id"
  mkdir -p "$root"
  evidence=$(mktemp -d "$root/$(fieldtest_timestamp_id)-$mode-$barcode${control:+-live}.XXXXXX")
  runner=${FIELD_SESSION_RUNNER:-$scripts/session-epoch-runner.sh}
  export FIELD_SESSION_CONTROL="$control"
  trap 'rc=$?; trap - EXIT; "$runner" cleanup "$mode" "$barcode" "$evidence" || true; exit "$rc"' EXIT
  trap 'exit 130' INT TERM HUP
  for step in begin prepare dispatch fault closeout verify collect; do
    "$runner" "$step" "$mode" "$barcode" "$evidence" || return $?
  done
  fieldtest_evidence_record "$SCRIPT_NAME" "$mode${control:+-live}" PASS "exact-tape closeout and prefix verified" "$evidence/manifest.json"
}
fieldtest_run_with_lock main "$@"
