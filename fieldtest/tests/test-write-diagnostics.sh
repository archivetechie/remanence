#!/usr/bin/env bash
# Check object correlation, missing evidence, and CSV compatibility without devices.
set -euo pipefail
scripts="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../scripts" && pwd)"
# shellcheck disable=SC1091
source "$scripts/lib.sh"
REMFIELD_HOME="$(mktemp -d)"
export REMFIELD_HOME
trap 'rm -rf -- "$REMFIELD_HOME"' EXIT
mkdir -p "$REMFIELD_HOME/log" "$REMFIELD_HOME/evidence"
receipt="$REMFIELD_HOME/receipt.json"
log="$REMFIELD_HOME/log/rem-daemon.log"
printf '%s\n' '{"object_id":"11111111-1111-4111-8111-111111111111"}' >"$receipt"
printf '%s\n' '{"phase":"drive_append_total","object_id":"11111111-1111-4111-8111-111111111111","throughput_mib_s":999,"staging_mode":"serial"}' >"$log"
offset="$(fieldtest_daemon_log_offset)"
{
  printf '%s\n' '{"fields":{"phase":"drive_append_total","object_id":"11111111-1111-4111-8111-111111111111","throughput_mib_s":123.5,"staging_mode":"overlap","staging_reason":"configured_overlap"}}'
  printf '%s\n' '{"phase":"drive_append_total","object_id":"22222222-2222-4222-8222-222222222222","throughput_mib_s":456,"staging_mode":"serial"}'
  printf '%s\n' '{"phase":"transfer","object_id":"11111111-1111-4111-8111-111111111111","throughput_mib_s":321}'
} >>"$log"
expected='11111111-1111-4111-8111-111111111111 123.5 overlap configured_overlap matched 321'
figures="$(fieldtest_daemon_write_figures "$receipt" "$offset")"
[[ "$figures" == "$expected" ]]
# A legacy row survives extension of the benchmark schema.
printf 'metric,drive,block_size,payload,MB_s,seconds,bytes\nread,test,n/a,test,1,2,3\n' >"$(fieldtest_bench_csv_path)"
chmod 0640 "$(fieldtest_bench_csv_path)"
# Recording supplied figures does not read the log a second time.
mv "$log" "$log.saved"
fieldtest_bench_record write test n/a test 20 2 40 "$receipt" "$offset" "$figures"
mv "$log.saved" "$log"
python3 - "$(fieldtest_bench_csv_path)" <<'PY'
import csv, os, stat, sys
assert stat.S_IMODE(os.stat(sys.argv[1]).st_mode) == 0o640
with open(sys.argv[1], newline="") as stream:
    rows = list(csv.DictReader(stream))
assert rows[0]["metric"] == "read"
assert rows[1]["daemon_drive_append_mib_s"] == "123.5"
assert rows[1]["staging_mode"] == "overlap"
assert rows[1]["staging_reason"] == "configured_overlap"
assert rows[1]["daemon_diagnostics"] == "matched"
assert rows[1]["daemon_transfer_mib_s"] == "321"
PY
# Do not reuse the earlier matching object when this invocation has no event.
offset="$(fieldtest_daemon_log_offset)"
[[ "$(fieldtest_daemon_write_figures "$receipt" "$offset")" == *'unavailable unavailable unavailable' ]]
# Errors and replays do not report a fresh write rate.
printf '%s\n' '{"phase":"drive_append_total","object_id":"11111111-1111-4111-8111-111111111111","throughput_mib_s":999,"staging_mode":"serial","replay":true}' >>"$log"
printf '%s\n' '{"phase":"drive_append_total","object_id":"11111111-1111-4111-8111-111111111111","throughput_mib_s":999,"staging_mode":"serial","status":"error"}' >>"$log"
[[ "$(fieldtest_daemon_write_figures "$receipt" "$offset")" == *'unavailable unavailable unavailable' ]]
rm "$log"
[[ "$(fieldtest_daemon_write_figures "$receipt" "$offset")" == *'unavailable unavailable unavailable' ]]
# Two independent writers preserve a populated CSV during concurrent updates.
python3 - "$(fieldtest_bench_csv_path)" <<'PYSEED'
import csv, sys
with open(sys.argv[1], newline="") as stream:
    reader = csv.DictReader(stream)
    fields = reader.fieldnames
with open(sys.argv[1], "a", newline="") as stream:
    writer = csv.DictWriter(stream, fieldnames=fields)
    writer.writerows({"metric": f"seed-{i}"} for i in range(1000))
PYSEED
for writer in first second; do
  (
    for row in {1..25}; do
      fieldtest_bench_record "$writer-$row" test n/a test 1 1 1
    done
  ) &
done
wait
python3 - "$(fieldtest_bench_csv_path)" <<'PYTEST'
import csv, os, stat, sys
assert stat.S_IMODE(os.stat(sys.argv[1]).st_mode) == 0o640
with open(sys.argv[1], newline="") as stream:
    rows = list(csv.DictReader(stream))
assert len(rows) == 1052, len(rows)
assert {r["metric"] for r in rows[1002:]} == {f"{writer}-{i}" for writer in ("first", "second") for i in range(1, 26)}
PYTEST
mkdir -p "$(dirname -- "$(fieldtest_config_path)")"
printf '[daemon]\n' >"$(fieldtest_config_path)"
fieldtest_require_overlap_staging 2>"$REMFIELD_HOME/guard.log"
[[ "$(cat "$REMFIELD_HOME/guard.log")" == *'defaults to overlap'* ]]
printf 'append_staging_mode = "serial"\n' >>"$(fieldtest_config_path)"
if fieldtest_require_overlap_staging 2>/dev/null; then exit 1; fi
FIELD_ALLOW_SERIAL_STAGING=1 fieldtest_require_overlap_staging 2>/dev/null
fieldtest_write_config "$(fieldtest_config_path)" TEST-LIB
python3 - "$(fieldtest_config_path)" <<'PYCONFIG'
import re, sys
from pathlib import Path
daemon = Path(sys.argv[1]).read_text().split("[daemon]", 1)[1].split("[[libraries]]", 1)[0]
assert re.search(r'^append_staging_mode = "overlap"$', daemon, re.M)
assert re.search(r'^append_overlap_min_bytes = "1GiB"$', daemon, re.M)
PYCONFIG
fieldtest_require_overlap_staging 2>"$REMFIELD_HOME/guard.log"
[[ "$(cat "$REMFIELD_HOME/guard.log")" == *'append_staging_mode=overlap; append_overlap_min_bytes=1GiB'* ]]
echo 'write diagnostics: correlation, missing logs, replay/error exclusion, CSV upgrade, concurrent writers, CSV permissions, staging reason, generated threshold, and default-mode guard pass'
