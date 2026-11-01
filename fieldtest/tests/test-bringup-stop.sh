#!/usr/bin/env bash
# Exercise allowlisted daemon unloads before graceful and forced process stops.
set -euo pipefail
scripts="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../scripts" && pwd)"
python3 - "$scripts" <<'PY'
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

scripts = Path(sys.argv[1])
with tempfile.TemporaryDirectory(prefix="bringup-stop-") as tmp:
    root = Path(tmp)
    bin_dir = root / "bin"
    bin_dir.mkdir()
    tmux = bin_dir / "tmux"
    tmux.write_text("#!/bin/sh\nexit 1\n")
    tmux.chmod(0o755)
    daemon = bin_dir / "rem-daemon"
    daemon.write_text('''#!/usr/bin/env python3
import os, signal, time
from pathlib import Path
stop_at = None
def stop(*_):
    global stop_at
    with open(os.environ["EVENTS_FILE"], "a") as events:
        events.write("TERM\\n")
    if os.environ["STOP_MODE"] == "graceful" and stop_at is None:
        stop_at = time.monotonic() + 2
signal.signal(signal.SIGTERM, stop)
Path(os.environ["READY_FILE"]).touch()
while stop_at is None or time.monotonic() < stop_at:
    time.sleep(.05)
''')
    daemon.chmod(0o755)
    rem = bin_dir / "rem"
    rem.write_text('''#!/usr/bin/env python3
import json, os, sys
args = sys.argv[1:]
mode = os.environ["INVENTORY_MODE"]
with open(os.environ["EVENTS_FILE"], "a") as events:
    events.write(json.dumps(args) + "\\n")
if args[0] == "top":
    if mode == "failed":
        sys.exit(1)
    if mode == "invalid":
        print("not JSON")
        sys.exit(0)
    library = {"library": {"library_serial": "SELECTED"}, "drives": [
        {"element_address": 256, "loaded_tape_barcode": "ALLOW1"},
        {"element_address": 257, "loaded_tape_barcode": "FOREIGN"},
        {"element_address": 258, "loaded_tape_barcode": "ALLOW2"},
        {"element_address": 259, "loaded_tape_barcode": None},
        {"element_address": 260, "loaded_tape_barcode": "CLN001"}]}
    print(json.dumps({"libraries": [library, {
        "library": {"library_serial": "OTHER"}, "drives": [
            {"element_address": 300, "loaded_tape_barcode": "ALLOW1"}]}]}))
elif args[:2] == ["drive", "unload"]:
    assert args[args.index("--library") + 1] == "SELECTED", args
    assert "--dest" not in args, args
    bay = args[args.index("--bay") + 1]
    assert bay in ["256", "258", "260"], args
    if bay == "258":
        print("injected unload refusal")
        sys.exit(1)
    print("returned to source slot")
else:
    raise AssertionError(args)
''')
    rem.chmod(0o755)
    cases = [("graceful", "08", "PASS", "valid"),
             ("never", "02", "WARN", "valid"),
             ("graceful", "08", "PASS", "failed"),
             ("graceful", "08", "PASS", "invalid")]
    for mode, timeout, status, inventory_mode in cases:
        home = root / f"{mode}-{inventory_mode}"
        home.mkdir()
        ready = home / "ready"
        (home / "state").mkdir()
        (home / "state/selected-library").write_text("SELECTED\n")
        (home / "allowlist.txt").write_text("ALLOW1\nALLOW2 # test failure\nCLN:CLN001\n")
        (home / "bin").mkdir()
        (home / "bin/rem").symlink_to(rem)
        events = home / "events"
        env = dict(os.environ, REMFIELD_HOME=str(home), FIELDTEST_LOCK_HELD="1",
                   REMFIELD_STOP_TIMEOUT_SECONDS=timeout, STOP_MODE=mode,
                   READY_FILE=str(ready), EVENTS_FILE=str(events), INVENTORY_MODE=inventory_mode, PATH=f"{bin_dir}:{os.environ['PATH']}")
        process = subprocess.Popen([str(daemon), "--config", str(home / "config.toml")], env=env)
        try:
            deadline = time.monotonic() + 5
            while not ready.exists():
                assert time.monotonic() < deadline, "fake daemon startup"
                time.sleep(.01)
            subprocess.run(["bash", str(scripts / "03-bringup.sh"), "--stop"], env=env, check=True, timeout=12)
            process.wait(timeout=2)
            records = [json.loads(line) for line in (home / "evidence/records.jsonl").read_text().splitlines()]
            sequence = events.read_text().splitlines()
            assert sequence[-1] == "TERM", sequence
            calls = [json.loads(line) for line in sequence[:-1]]
            unloads = [call for call in calls if call[:2] == ["drive", "unload"]]
            if inventory_mode == "valid":
                assert [call[call.index("--bay") + 1] for call in unloads] == ["256", "258", "260"], calls
                unload_records = [row for row in records if row["test_id"].startswith("stop-unload-")]
                assert [row["status"] for row in unload_records] == ["PASS", "WARN", "PASS"], unload_records
                assert "bay 258" in unload_records[1]["summary"]
                assert all(Path(row["detail_path"]).is_file() for row in unload_records)
                assert "injected unload refusal" in Path(unload_records[1]["detail_path"]).read_text()
            else:
                assert not unloads, calls
                assert any(row["test_id"] == "stop-inventory" and row["status"] == "WARN" for row in records), records
            record = next(row for row in records if row["test_id"] == "stop")
            assert record["status"] == status, record
            assert record["duration_seconds"] >= 2, record
            if status == "WARN":
                assert "rem top --once" in record["summary"]
                assert "rem tape quarantine list" in record["summary"]
                assert process.returncode == -9
            else:
                assert process.returncode == 0
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
print("PASS: stop unloads only selected allowlisted media before TERM; failures warn; graceful and forced waits are evidenced")
PY
