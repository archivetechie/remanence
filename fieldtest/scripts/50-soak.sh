#!/usr/bin/env bash
set -euo pipefail

SCRIPT_NAME="50-soak"
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

usage() {
  cat <<'EOF'
Usage: 50-soak.sh start|stop|report [--help]

Runs or reports a background write/read/verify soak loop.
EOF
}

soak_loop() {
  fieldtest_init_layout
  fieldtest_detect_env || true
  local serial pidfile journal workdir source object locator restore count=0
  serial="$(fieldtest_selected_library_serial)"
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
    object="$workdir/soak-$count.rao"
    locator="$workdir/soak-$count.json"
    restore="$workdir/restore-$count.rao"
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
    if fieldtest_capture_json "$locator" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" write --library "$serial" --file "$object" --pool fieldtest-a; then
      fieldtest_capture_json "$workdir/read-$count.json" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" read --object "$(cat "$locator")" --out "$restore" || true
      echo "{\"cycle\":$count,\"status\":\"ok\",\"object\":\"$object\"}" >>"$journal"
    else
      echo "{\"cycle\":$count,\"status\":\"write-failed\"}" >>"$journal"
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
total = ok = failed = 0
for raw in journal.read_text().splitlines():
    if not raw.strip():
        continue
    total += 1
    data = json.loads(raw)
    if data.get("status") == "ok":
        ok += 1
    else:
        failed += 1
print(f"cycles={total} ok={ok} failed={failed}")
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
