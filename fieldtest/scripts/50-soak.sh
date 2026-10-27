#!/usr/bin/env bash
set -euo pipefail

SCRIPT_NAME="50-soak"
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

usage() {
  cat <<'EOF'
Usage: 50-soak.sh start|stop|report [--help]

Runs or reports a background write/read/verify soak loop. The report sums the
cycle records of every run in the soak journal and prints each run's counts.
EOF
}

soak_loop() {
  fieldtest_init_layout
  fieldtest_detect_env || true
  local serial pidfile journal workdir source object locator restore count=0 run_id
  serial="$(fieldtest_selected_library_serial)"
  run_id="$(fieldtest_timestamp_id)-$$"
  pidfile="$(fieldtest_soak_pidfile)"
  journal="$(fieldtest_soak_journal)"
  workdir="$(mktemp -d "$(fieldtest_spool_dir)/soak-${RANDOM}.XXXXXX")"
  mkdir -p "$(dirname -- "$journal")"
  printf '%s\n' "$$" >"$pidfile"
  trap 'rm -f "$pidfile"; rm -rf -- "$workdir"' EXIT
  while true; do
    if ! fieldtest_try_lock; then
      sleep 5
      continue
    fi
    source="$workdir/soak-$count.bin"
    object="$workdir/soak-$count.rem-object"
    locator="$workdir/soak-$count.json"
    restore="$workdir/restore-$count.rem-object"
    python3 - "$source" 268435456 <<'PY'
import os,sys
from pathlib import Path
path=Path(sys.argv[1]); size=int(sys.argv[2]); chunk=8*1024*1024
path.parent.mkdir(parents=True, exist_ok=True)
with path.open("wb") as handle:
    remaining=size
    while remaining>0:
        n=min(chunk, remaining)
        handle.write(os.urandom(n))
        remaining-=n
PY
    fieldtest_capture_json "$workdir/build-$count.json" "$(fieldtest_rem_bin)" archive build --inputs "$source" --out "$object" || true
    if fieldtest_capture_io_json "$locator" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" write --library "$serial" --file "$object" --pool fieldtest-a; then
      fieldtest_capture_io_json "$workdir/read-$count.json" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" read --object "$(cat "$locator")" --out "$restore" || true
      echo "{\"run\":\"$run_id\",\"utc\":\"$(fieldtest_now_utc)\",\"cycle\":$count,\"status\":\"ok\",\"object\":\"$object\"}" >>"$journal"
    else
      echo "{\"run\":\"$run_id\",\"utc\":\"$(fieldtest_now_utc)\",\"cycle\":$count,\"status\":\"write-failed\"}" >>"$journal"
    fi
    count=$((count + 1))
    fieldtest_release_lock
    sleep 30
  done
}

start_soak() {
  local pidfile
  pidfile="$(fieldtest_soak_pidfile)"
  if [[ -f "$pidfile" ]] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then
    fieldtest_evidence_record "$SCRIPT_NAME" start INFO "soak loop already running"
    exit 0
  fi
  fieldtest_init_layout
  fieldtest_detect_env || true
  fieldtest_require_pool_appendable_tapes fieldtest-a 1 "soak loop"
  nohup bash "$(fieldtest_script_dir)/50-soak.sh" --loop >/dev/null 2>&1 9>&- &
  echo $! >"$pidfile"
  fieldtest_evidence_record "$SCRIPT_NAME" start PASS "soak loop started" "$pidfile"
}

stop_soak() {
  local pidfile
  pidfile="$(fieldtest_soak_pidfile)"
  if [[ -f "$pidfile" ]]; then
    kill "$(cat "$pidfile")" 2>/dev/null || true
    rm -f -- "$pidfile"
  fi
  fieldtest_evidence_record "$SCRIPT_NAME" stop PASS "soak loop stopped"
}

report_soak() {
  local journal
  journal="$(fieldtest_soak_journal)"
  if [[ ! -f "$journal" ]]; then
    echo "no soak journal present"
    exit 0
  fi
  python3 - "$journal" <<'PY'
import json,sys
from pathlib import Path
journal = Path(sys.argv[1])
runs = {}
untagged = 0
previous_cycle = None
for raw in journal.read_text().splitlines():
    if not raw.strip():
        continue
    data = json.loads(raw)
    key = data.get("run")
    if not key:
        # A record without a run tag: every run's cycle counter starts at 0,
        # so a cycle that does not follow the previous one starts a new run.
        cycle = data.get("cycle")
        if untagged == 0 or (isinstance(cycle, int) and isinstance(previous_cycle, int) and cycle <= previous_cycle):
            untagged += 1
        previous_cycle = cycle
        key = f"untagged-{untagged}"
    run = runs.setdefault(key, {"cycles": 0, "ok": 0, "failed": 0, "first": data.get("utc"), "last": None})
    run["cycles"] += 1
    if data.get("status") == "ok":
        run["ok"] += 1
    else:
        run["failed"] += 1
    run["last"] = data.get("utc") or run["last"]
for key, run in runs.items():
    span = f" {run['first']} to {run['last']}" if run["first"] else ""
    print(f"run {key}{span}: cycles={run['cycles']} ok={run['ok']} failed={run['failed']}")
total = {field: sum(run[field] for run in runs.values()) for field in ("cycles", "ok", "failed")}
print(f"runs={len(runs)} cycles={total['cycles']} ok={total['ok']} failed={total['failed']}")
PY
}

main() {
  case "${1:-}" in
    start) shift; fieldtest_run_with_lock start_soak ;;
    stop) shift; fieldtest_run_with_lock stop_soak ;;
    report) shift; fieldtest_run_with_lock report_soak ;;
    --loop) shift; soak_loop ;;
    --help|-h|"")
      usage
      exit 0
      ;;
    *)
      usage >&2
      exit 1
      ;;
  esac
}

main "$@"
