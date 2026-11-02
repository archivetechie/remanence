#!/usr/bin/env bash
# Run one field-test plan unattended, as the kit user, without a remote session.
#
#   run.sh plan/<name>.env
#
# Runs the plan's legs in order. However it ends (success, a fatal leg, or INT, TERM
# or HUP), it captures read-only diagnostics if something failed. The stop step
# unloads allowlisted cartridges; refusals and timeouts are WARNs in the evidence.
# It stops the daemon (03-bringup.sh --stop), then packs the evidence
# (90-collect-evidence.sh) and prints a PASS/FAIL table and the pack's sha256.
# It never uses sudo and never touches a library other than the plan's.
set -uo pipefail

pkg="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
scripts="$pkg/scripts"

die() {
  printf 'run.sh: refused: %s\n' "$*" >&2
  exit 2
}

[[ "${1:-}" == --help || "${1:-}" == -h || $# -ne 1 ]] && {
  sed -n '2,10p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  [[ $# -eq 1 ]] && exit 0
  exit 2
}
plan="$1"
[[ "$plan" == /* ]] || plan="$pkg/$plan"
[[ -f "$plan" ]] || die "no plan file $plan"
(( EUID != 0 )) || die "run as the kit user, not root"

PLAN_NAME=""
PLAN_LIBRARY=""
PLAN_ALLOWLIST=""
PLAN_LEGS=()
# shellcheck source=/dev/null
source "$plan"
[[ -n "$PLAN_NAME" && -n "$PLAN_LIBRARY" && -n "$PLAN_ALLOWLIST" && ${#PLAN_LEGS[@]} -gt 0 ]] ||
  die "$plan must set PLAN_NAME, PLAN_LIBRARY, PLAN_ALLOWLIST and PLAN_LEGS"

for entry in "${PLAN_LEGS[@]}"; do
  read -r policy leg _ <<<"$entry"
  [[ "$policy" == fatal || "$policy" == continue ]] || die "leg '$entry': policy must be fatal or continue"
  [[ "$leg" =~ ^[0-9][0-9]-[a-z0-9-]+\.sh$ && -x "$scripts/$leg" ]] || die "leg '$entry': no such kit script"
done

export FIELD_BIN_DIR="$pkg/bin"
export FIELDTEST_LIBRARY_SERIAL="$PLAN_LIBRARY"
export REMFIELD_ENV="${REMFIELD_ENV:-field}"
# shellcheck disable=SC1091 # Dynamic kit root; lib.sh is checked separately.
# shellcheck source=scripts/lib.sh
source "$scripts/lib.sh"
set +e
fieldtest_init_layout

# The allowlist must be exactly the plan's barcodes, with no cleaning cartridge, and a
# library chosen earlier must be the plan's.
allowlist="$(fieldtest_allowlist_path)"
[[ -f "$allowlist" ]] || die "no allowlist at $allowlist; write it with scripts/01-allowlist.sh first"
have="$(grep -v '^[[:space:]]*$' "$allowlist" | sort | tr '\n' ' ')"
want="$(tr ' ' '\n' <<<"$PLAN_ALLOWLIST" | grep -v '^$' | sort | tr '\n' ' ')"
[[ "$have" == "$want" ]] || die "allowlist is [${have% }], the plan allows exactly [${want% }]"
if selected="$(fieldtest_selected_library_serial 2>/dev/null)" && [[ "$selected" != "$PLAN_LIBRARY" ]]; then
  die "the kit state selects library $selected, the plan is for $PLAN_LIBRARY"
fi

pidfile="$(fieldtest_state_dir)/run.pid"
if [[ -f "$pidfile" ]] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then
  die "another run.sh is running (pid $(cat "$pidfile"))"
fi
printf '%s\n' "$$" >"$pidfile"

stamp="$(fieldtest_timestamp_id)"
# The log and the table live in evidence/, so the pack carries them.
log="$(fieldtest_evidence_dir)/run-${PLAN_NAME}-${stamp}.log"
table="$(fieldtest_evidence_dir)/run-${PLAN_NAME}-${stamp}.tsv"
result_file="$(fieldtest_evidence_dir)/run-${PLAN_NAME}-${stamp}.result"
FIELD_EVIDENCE_PACK="$(fieldtest_home)/evidence-pack-${PLAN_NAME}-${stamp}.tar.gz"
export FIELD_EVIDENCE_PACK
# Ctrl-c, a closed tmux window or a shutdown signals the whole process group. The
# logger must survive that, and keep writing the log if the terminal goes away, or
# run.sh would die on a broken pipe before its teardown.
trap '' PIPE
exec > >(trap '' INT TERM HUP PIPE; exec tee --output-error=warn -a "$log") 2>&1
printf 'leg\tpolicy\texit\tseconds\n' >"$table"
echo "run.sh: plan $PLAN_NAME on $PLAN_LIBRARY, kit $(cat "$pkg/KIT-COMMIT" 2>/dev/null || echo unknown), log $log"

child=""
interrupted=""
# Called by the traps below.
# shellcheck disable=SC2317
on_signal() {
  interrupted="$1"
  echo "run.sh: $1 received; stopping the current leg and tearing down"
  # A leg started in the background ignores INT, so it always gets TERM.
  [[ -n "$child" ]] && kill -TERM "$child" 2>/dev/null
  return 0
}
trap 'on_signal TERM' TERM
trap 'on_signal INT' INT
trap 'on_signal HUP' HUP

# run_step <name> <command...>: run in the background so a signal reaches it, and
# wait until it has finished even if a signal interrupts the wait.
run_step() {
  local rc
  "$@" &
  child=$!
  while :; do
    wait "$child"
    rc=$?
    kill -0 "$child" 2>/dev/null || break
  done
  child=""
  return "$rc"
}

failed=""
current=""
for entry in "${PLAN_LEGS[@]}"; do
  [[ -n "$interrupted" ]] && break
  read -r policy leg args <<<"$entry"
  argv=()
  read -r -a argv <<<"${args:-}"
  echo "run.sh: === $leg ${args:-}"
  start=$SECONDS
  run_step "$scripts/$leg" "${argv[@]}"
  rc=$?
  printf '%s\t%s\t%s\t%s\n' "$leg${args:+ $args}" "$policy" "$rc" "$(( SECONDS - start ))" >>"$table"
  echo "run.sh: === $leg exit $rc after $(( SECONDS - start ))s"
  current="$leg"
  # A leg a signal stopped is reported as the interruption, not as a failure.
  if (( rc != 0 )) && [[ "$policy" == fatal && -z "$interrupted" ]]; then
    failed="$leg"
    break
  fi
done

# Teardown, whatever happened. Further signals no longer interrupt it.
trap '' TERM INT HUP
if [[ -n "$interrupted" ]]; then
  reason="interrupted by $interrupted${current:+ during $current}"
elif [[ -n "$failed" ]]; then
  reason="$failed failed"
fi
if [[ -n "$failed" || -n "$interrupted" ]]; then
  echo "run.sh: === diagnostics ($reason)"
  # Teardown ignores TERM, so use SIGKILL to enforce this last diagnostic
  # deadline and always reach the daemon stop that returns cartridges home.
  diag_rc=0
  timeout --foreground --signal=KILL 300 "$scripts/33-diag-capture.sh" --note "run.sh teardown: $reason" || diag_rc=$?
  if (( diag_rc == 124 || diag_rc == 137 )); then
    fieldtest_evidence_record run teardown-diagnostics WARN "diagnostics cut after the 300s deadline; continuing to daemon stop"
  elif (( diag_rc != 0 )); then
    echo "run.sh: diagnostics capture failed (continuing)"
  fi
fi
echo "run.sh: === returning cartridges and stopping the daemon"
"$scripts/03-bringup.sh" --stop || echo "run.sh: 03-bringup.sh --stop failed; check the library and the daemon by hand"
if [[ -n "$interrupted" ]]; then
  result="INTERRUPTED ($interrupted)"
elif [[ -n "$failed" ]]; then
  result="FAIL ($failed)"
else
  result="PASS"
fi
printf 'result: %s\nplan: %s\nlibrary: %s\nkit: %s\n' "$result" "$PLAN_NAME" "$PLAN_LIBRARY" \
  "$(cat "$pkg/KIT-COMMIT" 2>/dev/null || echo unknown)" >"$result_file"
echo
column -t -s $'\t' "$table" 2>/dev/null || cat "$table"
echo "result: $result"
echo "run.sh: === packing evidence"
rm -f -- "$FIELD_EVIDENCE_PACK"
"$scripts/90-collect-evidence.sh" || echo "run.sh: evidence collection failed"
rm -f -- "$pidfile"
if [[ -f "$FIELD_EVIDENCE_PACK" ]]; then
  echo "evidence pack: $FIELD_EVIDENCE_PACK"
  sha256sum -- "$FIELD_EVIDENCE_PACK"
else
  echo "no evidence pack was written; the raw evidence is in $(fieldtest_evidence_dir)"
fi
echo "result: $result"
case "$result" in
  PASS) exit 0 ;;
  FAIL*) exit 1 ;;
  *) exit 130 ;;
esac
