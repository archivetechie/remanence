#!/usr/bin/env bash
# Exercise run.sh against stub leg scripts: leg order, fatal and continue policies,
# teardown on failure and on TERM, and the refusals that run before any leg.
set -euo pipefail
kit="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
root="$(mktemp -d "${TMPDIR:-/tmp}/run-plan-test.XXXXXX")"
trap '[[ -n "${KEEP:-}" ]] || rm -rf -- "$root"' EXIT
failures=0

ok() { printf 'ok   %s\n' "$1"; }
bad() { printf 'FAIL %s\n' "$1"; failures=$((failures + 1)); }
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi; }

# make_case: a package with stub legs and a kit home holding the plan's allowlist.
make_case() {
  local case_dir pkg leg
  case_dir="$(mktemp -d "$root/case.XXXXXX")"
  pkg="$case_dir/pkg"
  mkdir -p "$pkg/scripts" "$pkg/plan" "$pkg/bin" "$case_dir/home/remfield"
  cp "$kit/run.sh" "$pkg/run.sh"
  cp "$kit/scripts/lib.sh" "$pkg/scripts/lib.sh"
  for leg in 00-preflight 09-media-ready 03-bringup 02-discovery 11-happy-path 33-diag-capture 90-collect-evidence; do
    cat >"$pkg/scripts/$leg.sh" <<'STUB'
#!/usr/bin/env bash
name="$(basename -- "$0")"
printf '%s %s\n' "$name" "$*" >>"$HOME/calls"
printf '%s %s\n' "${FIELD_BIN_DIR:-}" "${FIELDTEST_LIBRARY_SERIAL:-}" >"$HOME/env"
if [[ "$(cat "$HOME/hang-on" 2>/dev/null)" == "$name" ]]; then
  trap 'echo termed >>"$HOME/calls"; exit 143' TERM HUP
  touch "$HOME/hanging"
  while :; do sleep 0.1; done
fi
[[ "$name" == 90-collect-evidence.sh ]] && cp "$REMFIELD_HOME"/evidence/run-*.log "$REMFIELD_HOME/packed-log" && : >"$FIELD_EVIDENCE_PACK"
[[ "$(cat "$HOME/fail-on" 2>/dev/null)" == "$name" ]] && exit 7
exit 0
STUB
    chmod +x "$pkg/scripts/$leg.sh"
  done
  cp "$kit/plan/smoke.env" "$pkg/plan/smoke.env"
  printf '%s\n' AOX032L9 AOX036L9 AOX038L9 AOX058L9 >"$case_dir/home/remfield/allowlist.txt"
  printf '%s\n' "$case_dir"
}

run_case() {
  local case_dir="$1"
  shift
  env -i PATH="$case_dir/pkg/bin:/usr/bin:/bin" HOME="$case_dir/home" REMFIELD_HOME="$case_dir/home/remfield" \
    bash "$case_dir/pkg/run.sh" "$@" >"$case_dir/out" 2>&1
}

calls() { tr '\n' '|' <"$1/home/calls" 2>/dev/null; }

# A passing plan runs every leg in order, then stops the daemon and packs evidence.
c="$(make_case)"
rc=0; run_case "$c" plan/smoke.env || rc=$?
check "pass: exit 0" "[[ $rc -eq 0 ]]"
check "pass: leg order and teardown" "[[ '$(calls "$c")' == '00-preflight.sh |09-media-ready.sh --count 4 --no-wait|03-bringup.sh |02-discovery.sh |11-happy-path.sh |03-bringup.sh --stop|33-diag-capture.sh --note smoke-plan-baseline|03-bringup.sh --stop|90-collect-evidence.sh |' ]]"
check "pass: result line" "grep -q '^result: PASS' '$c/out'"
check "pass: named pack reported" "grep -q 'evidence pack: .*/evidence-pack-smoke-.*tar.gz' '$c/out'"
check "pass: the log and result are in evidence before packing" "grep -q '11-happy-path.sh exit 0' '$c/home/remfield/packed-log' && grep -q '^result: PASS' '$c'/home/remfield/evidence/run-smoke-*.result"
check "pass: table has seven legs" "[[ \$(tail -n +2 '$c'/home/remfield/evidence/run-smoke-*.tsv | wc -l) -eq 7 ]]"
check "pass: legs see the package bins and the plan's library" "[[ \"\$(cat '$c/home/env')\" == \"\$(cd '$c/pkg/bin' && pwd -P) DEC418146K_LL01\" ]]"

# A fatal leg stops the plan, captures diagnostics, then tears down.
c="$(make_case)"
echo 03-bringup.sh >"$c/home/fail-on"
rc=0; run_case "$c" plan/smoke.env || rc=$?
check "fatal: exit 1" "[[ $rc -eq 1 ]]"
check "fatal: stops after the failing leg, then diagnoses and tears down" "[[ '$(calls "$c")' == '00-preflight.sh |09-media-ready.sh --count 4 --no-wait|03-bringup.sh |33-diag-capture.sh --note run.sh teardown: 03-bringup.sh failed|03-bringup.sh --stop|90-collect-evidence.sh |' ]]"
check "fatal: result names the leg" "grep -q '^result: FAIL (03-bringup.sh)' '$c/out'"

# A hung teardown diagnostic is killed at its deadline and still reaches stop.
c="$(make_case)"
echo 03-bringup.sh >"$c/home/fail-on"
echo 33-diag-capture.sh >"$c/home/hang-on"
cat >"$c/pkg/bin/timeout" <<'TIMEOUT'
#!/usr/bin/env bash
[[ "$1 $2 $3" == '--foreground --signal=KILL 300' ]] || exit 99
printf '%s\n' "$1 $2 $3" >"$HOME/diag-timeout"
shift 3
exec /usr/bin/timeout --foreground --signal=KILL 1 "$@"
TIMEOUT
chmod +x "$c/pkg/bin/timeout"
rc=0; run_case "$c" plan/smoke.env || rc=$?
check "hung diagnostics: exit 1" "[[ $rc -eq 1 ]]"
check "hung diagnostics: production deadline used" "[[ \$(cat '$c/home/diag-timeout') == '--foreground --signal=KILL 300' ]]"
check "hung diagnostics: stop and packing still run" "[[ '$(calls "$c")' == *'33-diag-capture.sh --note run.sh teardown: 03-bringup.sh failed|03-bringup.sh --stop|90-collect-evidence.sh |' ]]"
check "hung diagnostics: WARN recorded" "grep -q '\"status\":\"WARN\".*diagnostics cut after the 300s deadline' '$c/home/remfield/evidence/records.jsonl'"

# A failing "continue" leg is recorded but does not fail the plan.
c="$(make_case)"
echo 33-diag-capture.sh >"$c/home/fail-on"
rc=0; run_case "$c" plan/smoke.env || rc=$?
check "continue: exit 0" "[[ $rc -eq 0 ]]"
check "continue: exit status recorded" "grep -q \$'33-diag-capture.sh --note smoke-plan-baseline\tcontinue\t7' '$c'/home/remfield/evidence/run-smoke-*.tsv"

# TERM reaches the running leg; the plan then tears down and reports the interruption.
c="$(make_case)"
echo 11-happy-path.sh >"$c/home/hang-on"
env -i PATH=/usr/bin:/bin HOME="$c/home" REMFIELD_HOME="$c/home/remfield" \
  bash "$c/pkg/run.sh" plan/smoke.env >"$c/out" 2>&1 &
pid=$!
for _ in $(seq 100); do [[ -e "$c/home/hanging" ]] && break; sleep 0.1; done
kill -TERM "$pid"
rc=0; wait "$pid" || rc=$?
check "TERM: exit 130" "[[ $rc -eq 130 ]]"
check "TERM: leg got the signal, then teardown ran" "[[ '$(calls "$c")' == *'11-happy-path.sh |termed|33-diag-capture.sh --note run.sh teardown: interrupted by TERM during 11-happy-path.sh|03-bringup.sh --stop|90-collect-evidence.sh |' ]]"
check "TERM: pidfile removed" "[[ ! -e '$c/home/remfield/state/run.pid' ]]"

# A signal to the whole process group (Ctrl-c in tmux, a closed window, shutdown)
# must not kill the logger or skip teardown, and the leg must stop. run.sh starts
# with SIGINT at its default, as in a tmux pane; a background job would inherit
# it ignored, and bash cannot trap a signal ignored on entry.
for sig in INT HUP TERM; do
  c="$(make_case)"
  echo 11-happy-path.sh >"$c/home/hang-on"
  env -i PATH=/usr/bin:/bin HOME="$c/home" REMFIELD_HOME="$c/home/remfield" \
    setsid -w python3 -c 'import os, signal, sys; signal.signal(signal.SIGINT, signal.SIG_DFL); os.execvp(sys.argv[1], sys.argv[1:])' \
    bash "$c/pkg/run.sh" plan/smoke.env >"$c/out" 2>&1 &
  pid=$!
  for _ in $(seq 100); do [[ -e "$c/home/hanging" ]] && break; sleep 0.1; done
  pgid="$(ps -o pgid= -p "$(cat "$c/home/remfield/state/run.pid")" | tr -d ' ')"
  if [[ -z "$pgid" || "$pgid" == "$(ps -o pgid= -p $$ | tr -d ' ')" ]]; then
    bad "group $sig: run.sh is not in its own process group"
    kill -TERM "$pid" 2>/dev/null
  else
    kill "-$sig" -- "-$pgid"
  fi
  rc=0; wait "$pid" || rc=$?
  check "group $sig: exit 130" "[[ $rc -eq 130 ]]"
  check "group $sig: leg stopped, then teardown ran" "[[ '$(calls "$c")' == *'11-happy-path.sh |termed|33-diag-capture.sh --note run.sh teardown: interrupted by $sig during 11-happy-path.sh|03-bringup.sh --stop|90-collect-evidence.sh |' ]]"
  check "group $sig: log kept to the end" "grep -q '^result: INTERRUPTED ($sig)' '$c'/home/remfield/evidence/run-smoke-*.log"
done

# Refusals happen before any leg runs.
c="$(make_case)"
printf '%s\n' AOX032L9 AOX036L9 >"$c/home/remfield/allowlist.txt"
rc=0; run_case "$c" plan/smoke.env || rc=$?
check "refuse: short allowlist" "[[ $rc -eq 2 && ! -e '$c/home/calls' ]] && grep -q 'allowlist is' '$c/out'"
c="$(make_case)"
printf '%s\n' AOX032L9 AOX036L9 AOX038L9 AOX058L9 CLN:CLN001L1 >"$c/home/remfield/allowlist.txt"
rc=0; run_case "$c" plan/smoke.env || rc=$?
check "refuse: cleaning cartridge in the allowlist" "[[ $rc -eq 2 && ! -e '$c/home/calls' ]]"
c="$(make_case)"
mkdir -p "$c/home/remfield/state"
echo DEC418146K_LL02 >"$c/home/remfield/state/selected-library"
rc=0; run_case "$c" plan/smoke.env || rc=$?
check "refuse: state selects another library" "[[ $rc -eq 2 && ! -e '$c/home/calls' ]] && grep -q 'selects library DEC418146K_LL02' '$c/out'"
c="$(make_case)"
printf 'PLAN_NAME=x\nPLAN_LIBRARY=L\nPLAN_ALLOWLIST="AOX032L9 AOX036L9 AOX038L9 AOX058L9"\nPLAN_LEGS=("fatal ../../bin/evil.sh")\n' >"$c/pkg/plan/bad.env"
rc=0; run_case "$c" plan/bad.env || rc=$?
check "refuse: leg outside the kit scripts" "[[ $rc -eq 2 && ! -e '$c/home/calls' ]]"
c="$(make_case)"
mkdir -p "$c/home/remfield/state"
sleep 30 &
live=$!
echo "$live" >"$c/home/remfield/state/run.pid"
rc=0; run_case "$c" plan/smoke.env || rc=$?
kill "$live" 2>/dev/null || true
check "refuse: another run is live" "[[ $rc -eq 2 && ! -e '$c/home/calls' ]]"

if (( failures )); then
  printf '%d run-plan check(s) failed\n' "$failures"
  exit 1
fi
echo "run plan: all checks passed"
