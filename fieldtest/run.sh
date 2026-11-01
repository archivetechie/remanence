#!/usr/bin/env bash
# Run one field-test plan unattended, as the kit user, without a remote session.
#
#   run.sh plan/<name>.env
#
# Runs the plan's legs in order. However it ends (success, a fatal leg, or INT, TERM
# or HUP), it captures read-only diagnostics if something failed, returns allowlisted
# cartridges home and stops the daemon (03-bringup.sh --stop), then packs the evidence
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
log="$(fieldtest_log_dir)/run-${PLAN_NAME}-${stamp}.log"
table="$(fieldtest_evidence_dir)/run-${PLAN_NAME}-${stamp}.tsv"
exec > >(tee -a "$log") 2>&1
printf 'leg\tpolicy\texit\tseconds\n' >"$table"
echo "run.sh: plan $PLAN_NAME on $PLAN_LIBRARY, kit $(cat "$pkg/KIT-COMMIT" 2>/dev/null || echo unknown), log $log"

child=""
interrupted=""
on_signal() {
  interrupted="$1"
  echo "run.sh: $1 received; stopping the current leg and tearing down"
  [[ -n "$child" ]] && kill "-$1" "$child" 2>/dev/null
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
  echo "run.sh: === $leg ${args:-}"
  start=$SECONDS
  # shellcheck disable=SC2086 # plan arguments are split on purpose
  run_step "$scripts/$leg" ${args:-}
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
  "$scripts/33-diag-capture.sh" --note "run.sh teardown: $reason" ||
    echo "run.sh: diagnostics capture failed (continuing)"
fi
echo "run.sh: === returning cartridges and stopping the daemon"
"$scripts/03-bringup.sh" --stop || echo "run.sh: 03-bringup.sh --stop failed; check the library and the daemon by hand"
echo "run.sh: === packing evidence"
"$scripts/90-collect-evidence.sh" || echo "run.sh: evidence collection failed; the raw evidence is in $(fieldtest_evidence_dir)"
rm -f -- "$pidfile"

echo
column -t -s $'\t' "$table" 2>/dev/null || cat "$table"
pack="$(ls -t "$(fieldtest_home)"/evidence-pack-*.tar.gz 2>/dev/null | head -1)"
if [[ -n "$pack" ]]; then
  echo "evidence pack: $pack"
  sha256sum -- "$pack"
fi
if [[ -n "$interrupted" ]]; then
  echo "result: INTERRUPTED ($interrupted)"
  exit 130
elif [[ -n "$failed" ]]; then
  echo "result: FAIL ($failed)"
  exit 1
fi
echo "result: PASS"
