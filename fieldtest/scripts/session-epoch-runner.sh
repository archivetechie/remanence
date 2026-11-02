#!/usr/bin/env bash
# Concrete field leg operations; invoked by 41 under the kit's work lock.
set -euo pipefail
scripts="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
source "$scripts/lib.sh"
step=${1:?} mode=${2:?} barcode=${3:?} evidence=${4:?}
rem=$(fieldtest_rem_bin)
io=$(fieldtest_io_bin)
endpoint=$(fieldtest_rem_endpoint)
gate="$evidence/gate"
work="$(fieldtest_spool_dir)/session-epoch-$(basename "$evidence")"
helper_args() {
  mapfile -t identity < "$evidence/identity.args"
  args=(--endpoint "$endpoint" session-epoch --mode "$mode" --tape "${identity[0]}" --pool "${identity[1]}" --barcode "$barcode" --evidence "$evidence" --gate "$gate" --log "${identity[2]}" --work "$work")
}
case "$step" in
begin)
  budget=$((8 * FIELDTEST_GIB))
  [[ $mode != measure-full-epoch ]] || budget=$((32 * FIELDTEST_GIB))
  fieldtest_require_space "$budget" "session epoch seed, daemon spool and restore"
  mkdir -p "$gate" "$work"
  "$rem" catalog tapes --endpoint "$endpoint" --json > "$evidence/catalog.json"
  "$rem" top --endpoint "$endpoint" --once --json > "$evidence/top-before.json"
  "$rem" drive list --endpoint "$endpoint" --json > "$evidence/drives.json"
  "$rem" library "$(fieldtest_selected_library_serial)" --json --slots > "$evidence/library.json"
  python3 "$scripts/session-epoch-evidence.py" identity "$evidence" "$barcode" "$(fieldtest_config_path)"
  # Fresh daemon VmHWM baseline for every leg; no live write exists yet.
  "$scripts/03-bringup.sh" --stop
  export REM_FIELD_GATE_DIR="$gate" REM_FIELD_TRACE="$evidence/commands.jsonl" REM_FIELD_PROGRESS="$evidence/progress.jsonl"
  "$scripts/03-bringup.sh"
  python3 "$scripts/session-epoch-evidence.py" pid "$evidence" "$(fieldtest_rem_daemon_bin)" "$(fieldtest_config_path)"
  python3 "$scripts/session-epoch-evidence.py" sample "$evidence" >> "$evidence/memory.jsonl" &
  echo "$!" > "$evidence/sampler.pid"
  pkg=$(cd "$scripts/.." && pwd -P)
  cp "$pkg/KIT-COMMIT" "$evidence/KIT-COMMIT"
  cp "$pkg/SOURCE-COMMIT" "$evidence/SOURCE-COMMIT"
  sha256sum "$rem" "$(fieldtest_rem_debug_bin)" "$(fieldtest_rem_daemon_bin)" "$io" > "$evidence/binary-sha256.txt"
  ;;
prepare)
  helper_args
  phase=prepare
  [[ -z ${FIELD_SESSION_CONTROL:-} ]] || phase=live-control
  "$io" "${args[@]}" --phase "$phase" > "$evidence/helper.jsonl" 2> "$evidence/helper.stderr" &
  echo "$!" > "$evidence/helper.pid"
  ;;
dispatch)
  python3 "$scripts/session-epoch-evidence.py" wait "$evidence" "${FIELD_SESSION_CONTROL:-}"
  ;;
fault)
  if [[ $mode == kill-mid-write && -z ${FIELD_SESSION_CONTROL:-} ]]; then
    python3 "$scripts/session-epoch-evidence.py" kill "$evidence" "$(fieldtest_rem_daemon_bin)" "$(fieldtest_config_path)"
    python3 "$scripts/session-epoch-evidence.py" stop-helper "$evidence" "$io"
    export REM_FIELD_GATE_DIR="$gate" REM_FIELD_TRACE="$evidence/commands.jsonl" REM_FIELD_PROGRESS="$evidence/progress.jsonl"
    "$scripts/03-bringup.sh"
    python3 "$scripts/session-epoch-evidence.py" pid "$evidence" "$(fieldtest_rem_daemon_bin)" "$(fieldtest_config_path)"
    python3 "$scripts/session-epoch-evidence.py" sample "$evidence" >> "$evidence/memory.jsonl" &
    echo "$!" > "$evidence/restarted-sampler.pid"
  else
    python3 "$scripts/session-epoch-evidence.py" settled "$evidence"
  fi
  ;;
closeout)
  helper_args
  "$io" "${args[@]}" --phase finish > "$evidence/finish.jsonl" 2> "$evidence/finish.stderr"
  ;;
verify)
  python3 "$scripts/session-epoch-evidence.py" sample-once "$evidence" >> "$evidence/memory.jsonl"
  touch "$evidence/commands.jsonl.stop" "$evidence/progress.jsonl.stop"
  python3 "$scripts/session-epoch-evidence.py" stop-samplers "$evidence"
  "$rem" top --endpoint "$endpoint" --once --json > "$evidence/top-after.json"
  python3 "$scripts/session-epoch-evidence.py" verify "$evidence" "$mode" "${FIELD_SESSION_CONTROL:-}"
  ;;
collect)
  python3 "$scripts/session-epoch-evidence.py" manifest "$evidence" "$mode" "$barcode"
  ;;
cleanup)
  # Release the compile-time dispatch hold even if the helper or verifier failed.
  [[ ! -d $gate ]] || touch "$gate/release"
  cleanup_rc=0
  python3 "$scripts/session-epoch-evidence.py" stop-samplers "$evidence" || cleanup_rc=1
  python3 "$scripts/session-epoch-evidence.py" stop-helper "$evidence" "$io" || cleanup_rc=1
  if [[ -f $evidence/session.json && ! -f $evidence/after.json ]]; then
    helper_args
    timeout --signal=TERM --kill-after=5s 30s "$io" "${args[@]}" --phase cancel > "$evidence/cleanup.jsonl" 2>&1 || true
  fi
  [[ ! -d $work ]] || rm -rf -- "$work"
  exit "$cleanup_rc"
  ;;
*) exit 2;;
esac
