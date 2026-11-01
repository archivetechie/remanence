#!/usr/bin/env bash
# Exercise the kit's disk guard with stubbed df, du and findmnt: floors on a shared
# and on a dedicated spool filesystem, the kit cap, refusal evidence, run-dir cleanup
# on exit and on signals, the stale-run sweep and the kit TMPDIR.
set -euo pipefail
scripts="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../scripts" && pwd)"
root="$(mktemp -d "${TMPDIR:-/tmp}/disk-guard-test.XXXXXX")"
trap 'rm -rf -- "$root"' EXIT

GIB=$((1024 * 1024 * 1024))
stubs="$root/stubs"
mkdir -p "$stubs"
cat >"$stubs/df" <<'EOF'
#!/bin/sh
echo "Filesystem 1-blocks Used Available Capacity Mounted"
echo "stubfs $STUB_TOTAL 0 $STUB_AVAIL 0% /stub"
EOF
cat >"$stubs/du" <<'EOF'
#!/bin/sh
if [ -n "${STUB_KIT:-}" ]; then printf '%s\t%s\n' "$STUB_KIT" "stub"; else exec /usr/bin/du "$@"; fi
EOF
cat >"$stubs/findmnt" <<'EOF'
#!/bin/sh
case "$*" in
  *FSTYPE*) printf '%s\n' "${STUB_FSTYPE:-xfs}" ;;
  *SOURCE*) printf '%s\n' "${STUB_SOURCE:-/dev/loop0}" ;;
  *) printf '%s\n' "$STUB_TARGET" ;;
esac
EOF
chmod +x "$stubs/df" "$stubs/du" "$stubs/findmnt"

failures=0

# run_guard <total_gib> <avail_gib> <kit_gib> <mount_target> <shell code> [env...]
# Runs the code with lib.sh sourced in a fresh home; prints its stdout and stderr.
run_guard() {
  local total="$1" avail="$2" kit="$3" target="$4" code="$5"
  shift 5
  local home
  home="$(mktemp -d "$root/case.XXXXXX")"
  mkdir -p "$home/remfield/spool"
  home="$(cd -- "$home" && pwd -P)"
  target="${target//@HOME@/$home}"
  env -i PATH="$stubs:/usr/bin:/bin" HOME="$home" REMFIELD_HOME="$home/remfield" \
    STUB_TOTAL=$((total * GIB)) STUB_AVAIL=$((avail * GIB)) STUB_KIT=$((kit * GIB)) \
    STUB_TARGET="$target" SCRIPT_NAME=test-disk-guard "$@" \
    bash -c "source '$scripts/lib.sh'; $code" 2>&1
}

expect() {
  # expect <name> <expected substring or "EMPTY"> <actual>
  local name="$1" want="$2" got="$3"
  if [[ "$want" == EMPTY && -z "$got" ]] || { [[ "$want" != EMPTY ]] && [[ "$got" == *"$want"* ]]; }; then
    printf 'ok   %s\n' "$name"
  else
    printf 'FAIL %s\n  want: %s\n  got:  %s\n' "$name" "$want" "$got"
    failures=$((failures + 1))
  fi
}

refusal() { run_guard "$1" "$2" "$3" "$4" "fieldtest_space_refusal $(( $5 * GIB ))" "${@:6}"; }

# Shared filesystem (the spool's mount is /home): 150 GiB and 25% floors, 40 GiB kit cap.
expect "shared: fits" EMPTY "$(refusal 618 363 1 /home 10)"
expect "shared: below the 150 GiB floor" "below the 150 GiB floor" "$(refusal 618 200 1 /home 60)"
expect "shared: floor names the mode" "on the shared spool filesystem" "$(refusal 618 200 1 /home 60)"
expect "shared: below 25%" "below 25% of it" "$(refusal 1000 260 1 /home 20)"
expect "shared: kit cap" "above its 40 GiB cap" "$(refusal 618 363 35 /home 10)"
expect "shared: env floor override" EMPTY "$(refusal 618 170 1 /home 30 FIELD_MIN_FREE_GB=100 FIELD_MIN_FREE_PCT=20)"

# Dedicated filesystem (a capped home, or a tmpfs under it): it is its own cap, so
# only a small floor applies and the kit cap does not.
expect "dedicated home: fits" EMPTY "$(refusal 50 45 1 @HOME@ 30)"
expect "dedicated home: 2 GiB floor" "below the 2 GiB floor" "$(refusal 50 45 1 @HOME@ 44)"
expect "dedicated home: names the mode" "on the dedicated spool filesystem" "$(refusal 50 45 1 @HOME@ 44)"
expect "dedicated tmpfs under home" EMPTY "$(refusal 320 300 1 @HOME@/remfield/spool 100)"
expect "dedicated: no kit cap" EMPTY "$(refusal 50 45 39 @HOME@ 5)"
expect "detection: NFS home is shared" "150 GiB floor" "$(refusal 618 200 1 @HOME@ 60 STUB_SOURCE=nas:/export/home STUB_FSTYPE=nfs4)"
expect "detection: bind mount is shared" "150 GiB floor" "$(refusal 618 200 1 @HOME@ 60 'STUB_SOURCE=/dev/loop0[/srv/share]')"
expect "detection: plain disk at home is shared" "150 GiB floor" "$(refusal 618 200 1 @HOME@ 60 STUB_SOURCE=/dev/sdb1)"
expect "detection: sibling of home is shared" "150 GiB floor" "$(refusal 618 200 1 @HOME@-other 60)"
expect "override: forced shared" "150 GiB floor" "$(refusal 50 45 1 @HOME@ 30 FIELD_SPOOL_DEDICATED=0)"
expect "override: forced dedicated" EMPTY "$(refusal 618 200 1 /home 60 FIELD_SPOOL_DEDICATED=1)"
# A tmpfs spool also keeps RAM headroom for production.
printf 'MemAvailable:   %s kB\n' $((400 * 1024 * 1024)) >"$root/meminfo"
expect "tmpfs: fits in RAM" EMPTY "$(refusal 320 300 1 @HOME@/remfield/spool 100 STUB_FSTYPE=tmpfs FIELD_MEMINFO="$root/meminfo")"
printf 'MemAvailable:   %s kB\n' $((200 * 1024 * 1024)) >"$root/meminfo-low"
expect "tmpfs: RAM floor" "below the 64 GiB floor (FIELD_MIN_FREE_RAM_GB)" "$(refusal 320 300 1 @HOME@/remfield/spool 150 STUB_FSTYPE=tmpfs FIELD_MEMINFO="$root/meminfo-low")"
expect "floors report" "2 0 dedicated" "$(run_guard 50 45 1 @HOME@ fieldtest_space_floors)"

# A refusal exits 1 before writing and records FAIL evidence.
out="$(run_guard 618 200 1 /home "( fieldtest_require_space $((60 * GIB)) 'a test payload'; echo reached ) || echo \"exit=\$?\"; cat \"\$(fieldtest_records_path)\"")"
expect "require_space: exits 1" "exit=1" "$out"
expect "require_space: stops before the write" "disk guard: refused a test payload" "$out"
[[ "$out" != *reached* ]] || { echo "FAIL require_space: continued after refusal"; failures=$((failures + 1)); }
expect "require_space: FAIL evidence" "disk-guard" "$(grep -F '"FAIL"' <<<"${out//\": \"/\":\"}" || true)"

# A run dir is removed on normal exit, on error exit and on TERM.
out="$(run_guard 618 363 1 /home 'fieldtest_make_workdir wd happy 1024 "a run"; echo "$wd"; touch "$wd/payload"')"
expect "workdir: under the spool" "/remfield/spool/happy." "$out"
[[ ! -e "$out" ]] && echo "ok   workdir: removed on exit" || { echo "FAIL workdir: left $out"; failures=$((failures + 1)); }
out="$(run_guard 618 363 1 /home 'fieldtest_make_workdir wd fault 1024 "a run"; echo "$wd"; false' || true)"
[[ -n "$out" && ! -e "$out" ]] && echo "ok   workdir: removed on error exit" || { echo "FAIL workdir: error exit left $out"; failures=$((failures + 1)); }
out="$(run_guard 618 363 1 /home 'fieldtest_make_workdir wd soak 1024 "a run"; echo "$wd"; kill -TERM $$; sleep 5' || true)"
out="${out%%$'\n'*}"
[[ -n "$out" && ! -e "$out" ]] && echo "ok   workdir: removed on TERM" || { echo "FAIL workdir: TERM left $out"; failures=$((failures + 1)); }

# Cleanup never removes a path outside the spool.
outside="$root/outside"
mkdir -p "$outside"
run_guard 618 363 1 /home "FIELDTEST_WORKDIRS+=('$outside'); fieldtest_cleanup_workdirs" >/dev/null
[[ -d "$outside" ]] && echo "ok   cleanup: keeps paths outside the spool" || { echo "FAIL cleanup removed $outside"; failures=$((failures + 1)); }

# The sweep removes old kit run dirs and old scratch files, and keeps everything else.
home="$(mktemp -d "$root/case.XXXXXX")"
spool="$home/remfield/spool"
mkdir -p "$spool"/{happy-old,bench-old,soak-old,happy-new,keepme-old} "$home/remfield/tmp/old-scratch" "$home/remfield/state"
touch -d '10 hours ago' "$spool"/{happy-old,bench-old,soak-old,keepme-old} "$home/remfield/tmp/old-scratch"
sleep 30 & live=$!
echo "$live" >"$home/remfield/state/soak.pid"
count="$(env -i PATH="$stubs:/usr/bin:/bin" HOME="$home" REMFIELD_HOME="$home/remfield" \
  bash -c "source '$scripts/lib.sh'; fieldtest_sweep_stale_runs" 2>/dev/null)"
kill "$live" 2>/dev/null || true
expect "sweep: count" "2" "$count"
for gone in happy-old bench-old; do
  [[ ! -e "$spool/$gone" ]] && echo "ok   sweep: removed $gone" || { echo "FAIL sweep kept $gone"; failures=$((failures + 1)); }
done
for kept in soak-old happy-new keepme-old; do
  [[ -d "$spool/$kept" ]] && echo "ok   sweep: kept $kept" || { echo "FAIL sweep removed $kept"; failures=$((failures + 1)); }
done
[[ ! -e "$home/remfield/tmp/old-scratch" ]] && echo "ok   sweep: removed old scratch" || { echo "FAIL sweep kept old scratch"; failures=$((failures + 1)); }

# Layout init points TMPDIR inside the kit.
expect "init_layout: TMPDIR in the kit" "/remfield/tmp" "$(run_guard 618 363 1 /home 'fieldtest_init_layout; echo "$TMPDIR"; mktemp')"

# Symlinked spool: the kit size counts what the link points at, and the sweep follows it.
home="$(mktemp -d "$root/case.XXXXXX")"
mkdir -p "$home/remfield/state" "$home/elsewhere/happy-old"
ln -s "$home/elsewhere" "$home/remfield/spool"
head -c $((3 * 1024 * 1024)) /dev/zero >"$home/elsewhere/payload"
touch -d '10 hours ago' "$home/elsewhere/happy-old"
report="$(env -i PATH="$stubs:/usr/bin:/bin" HOME="$home" REMFIELD_HOME="$home/remfield" STUB_TOTAL=1 STUB_AVAIL=1 \
  bash -c "source '$scripts/lib.sh'; fieldtest_space_report")"
kit_bytes="${report##* }"
if (( kit_bytes >= 3 * 1024 * 1024 )); then echo "ok   symlinked spool: kit counts its payload"; else echo "FAIL symlinked spool: kit=$kit_bytes"; failures=$((failures + 1)); fi
count="$(env -i PATH="$stubs:/usr/bin:/bin" HOME="$home" REMFIELD_HOME="$home/remfield" \
  bash -c "source '$scripts/lib.sh'; fieldtest_sweep_stale_runs" 2>/dev/null)"
expect "symlinked spool: sweep follows the link" "1" "$count"
if [[ ! -e "$home/elsewhere/happy-old" ]]; then echo "ok   symlinked spool: removed happy-old"; else echo "FAIL symlinked spool kept happy-old"; failures=$((failures + 1)); fi

# TERM sent to a locked script's own PID stops main and removes its run dir.
home="$(mktemp -d "$root/case.XXXXXX")"
mkdir -p "$home/remfield/spool"
cat >"$home/locked.sh" <<LOCKED
source '$scripts/lib.sh'
main() {
  fieldtest_make_workdir wd happy 1024 "a locked run"
  printf '%s\n' "\$wd" >"\$HOME/wd"
  while :; do date +%s%N >>"\$HOME/progress"; sleep 0.1; done
}
fieldtest_run_with_lock main
LOCKED
env -i PATH="$stubs:/usr/bin:/bin" HOME="$home" REMFIELD_HOME="$home/remfield" \
  STUB_TOTAL=$((618 * GIB)) STUB_AVAIL=$((363 * GIB)) STUB_KIT=$GIB STUB_TARGET=/home \
  bash "$home/locked.sh" >/dev/null 2>&1 &
locked=$!
for _ in $(seq 100); do [[ -s "$home/progress" ]] && break; sleep 0.1; done
kill -TERM "$locked"
rc=0
wait "$locked" || rc=$?
lines="$(wc -l <"$home/progress")"
sleep 0.5
expect "locked TERM: exit status" "143" "$rc"
expect "locked TERM: main stopped" "$lines" "$(wc -l <"$home/progress")"
wd="$(cat "$home/wd")"
if [[ -n "$wd" && ! -e "$wd" ]]; then echo "ok   locked TERM: run dir removed"; else echo "FAIL locked TERM left $wd"; failures=$((failures + 1)); fi

if (( failures )); then
  printf '%d disk-guard check(s) failed\n' "$failures"
  exit 1
fi
echo "disk guard: all checks passed"
