#!/usr/bin/env bash
# Exercise dual-bench aggregation with isolated data and no device commands.
set -euo pipefail
scripts="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../scripts" && pwd)"
python3 - "$scripts" <<'PY'
import csv
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

scripts = Path(sys.argv[1])
with tempfile.TemporaryDirectory(prefix="bench-dual-test-") as temp:
    root = Path(temp)
    kit = root / "scripts"
    kit.mkdir()
    shutil.copyfile(scripts / "22-bench-dual.sh", kit / "22-bench-dual.sh")
    library = (scripts / "lib.sh").read_text()
    library += r'''
# Replace external operations while retaining the benchmark's orchestration and CSV writer.
fieldtest_detect_env() { REMFIELD_ENV=vtl; }
fieldtest_io_bin() { echo mock-io; }
fieldtest_rem_bin() { echo mock-rem; }
fieldtest_selected_library_serial() { echo TEST-LIB; }
fieldtest_require_pool_appendable_tapes() { :; }
wait_for_idle_bays() { :; }
fieldtest_capture_io_json() { printf '{}\n' >"$1"; }
fieldtest_capture_json() { :; }
fieldtest_capture_tape_io_mode() { :; }
fieldtest_evidence_record() { :; }
fieldtest_daemon_write_figures() {
  [[ ! -e "$REMFIELD_HOME/figures-read" ]] || { echo 'daemon log is read twice' >&2; return 1; }
  touch "$REMFIELD_HOME/figures-read"
  printf '11111111-1111-4111-8111-111111111111 123.5 overlap configured_overlap matched 321\n'
}
fieldtest_run_with_lock() {
  mkdir -p "$REMFIELD_HOME"
  printf '%s %s\n' "$(REMFIELD_ENV=vtl default_bench_bytes)" "$(REMFIELD_ENV=real default_bench_bytes)" >"$REMFIELD_HOME/bench-sizes"
  make_payload() { : >"$1"; }
  bench_one() { echo 0 >"$4.log-offset"; echo '{} ' >"$4"; echo '1024 1 1'; }
  bench_read_one() { echo '1024 1 1'; }
  "$@"
}
'''
    (kit / "lib.sh").write_text(library)
    home = root / "home"
    env = dict(os.environ, REMFIELD_HOME=str(home))
    env.pop("FIELD_BENCH_DUAL_BYTES_VTL", None)
    env.pop("FIELD_BENCH_DUAL_BYTES", None)
    subprocess.run(["bash", str(kit / "22-bench-dual.sh")], env=env, check=True, timeout=15)
    assert (home / "bench-sizes").read_text().split() == ["1073741824", "2147483648"]
    with (home / "evidence/bench.csv").open(newline="") as stream:
        rows = {row["metric"]: row for row in csv.DictReader(stream)}
    assert set(rows) == {"dual-restore", "dual-b", "dual-aggregate"}
    for field in ("object_id", "daemon_drive_append_mib_s", "staging_mode", "staging_reason", "daemon_diagnostics", "daemon_transfer_mib_s"):
        assert rows["dual-b"][field] == rows["dual-aggregate"][field], field
    assert rows["dual-aggregate"]["staging_reason"] == "configured_overlap"
    assert rows["dual-aggregate"]["daemon_drive_append_mib_s"] == "123.5"
print("PASS: dual bench reuses parsed figures, records staging reason, and sizes both environments for the ring")
PY
