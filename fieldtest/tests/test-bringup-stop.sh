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
    # Discovery may fail before config generation: stop must be a quiet no-op.
    missing = root / "missing-config"
    result = subprocess.run(["bash", str(scripts / "03-bringup.sh"), "--stop"],
                            env=dict(os.environ, REMFIELD_HOME=str(missing), FIELDTEST_LOCK_HELD="1"),
                            capture_output=True, text=True, timeout=10)
    assert result.returncode == 0, result.stderr
    assert "Traceback" not in result.stderr and "non-negative integer" not in result.stderr
    rows = [json.loads(line) for line in (missing / "evidence/records.jsonl").read_text().splitlines()]
    assert any(row["test_id"] == "stop" and "nothing to stop" in row["summary"] for row in rows)
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
import json, os, sys, time, signal
from pathlib import Path
args = sys.argv[1:]
mode = os.environ["INVENTORY_MODE"]
with open(os.environ["EVENTS_FILE"], "a") as events:
    events.write(json.dumps(args) + "\\n")
if mode == "hang-top" or (mode in ("hang-phase", "hang-unload") and args[0] == "drive"):
    with open(os.environ["REMFIELD_HOME"] + "/hung-pids", "a") as pids:
        pids.write(str(os.getpid()) + "\\n")
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    signal.signal(signal.SIGINT, signal.SIG_IGN)
    signal.signal(signal.SIGHUP, signal.SIG_IGN)
    while True:
        time.sleep(1)
if args[0] == "top":
    if mode == "failed":
        sys.exit(1)
    if mode == "invalid":
        print("not JSON")
        sys.exit(0)
    print(open(os.environ["TOP_FIXTURE"]).read())
elif args[:2] == ["drive", "unload"]:
    if mode == "slow-bays":
        time.sleep(1.3)
    assert args[args.index("--library") + 1] == "SELECTED", args
    assert "--dest" not in args, args
    bay = args[args.index("--bay") + 1]
    assert bay in ["256", "258", "260"], args
    if mode == "busy" and bay == "256":
        counter = Path(os.environ["REMFIELD_HOME"]) / "attempts"
        attempt = int(counter.read_text()) + 1 if counter.exists() else 1
        counter.write_text(str(attempt))
        if attempt <= 2:
            print("drive busy: held by dead client session")
            sys.exit(1)
    if bay == "258":
        print("injected unload refusal")
        sys.exit(1)
    progress = {}
    print(json.dumps({"schema": "rem.operation.status.v1", "kind": "item", "data": {"progress": progress}}))
else:
    raise AssertionError(args)
''')
    rem.chmod(0o755)
    # Keep production timeout arguments observable; shorten only hanging cases.
    timeout_bin = bin_dir / "timeout"
    timeout_bin.write_text("""#!/usr/bin/env python3
import os, sys
args = sys.argv[1:]
foreground = args[0] == "--foreground"
if foreground:
    args.pop(0)
assert args[0] == "--signal=KILL", args
limit = int(args[1])
with open(os.environ["REMFIELD_HOME"] + "/timeouts", "a") as log:
    log.write(str(limit) + "\\n")
assert limit in (60, 1080, 2220, 3300), args
mode = os.environ["INVENTORY_MODE"]
if mode == "slow-bays":
    args[1] = "4" if args[2] == "bash" else "2"
if mode == "hang-phase":
    if args[2] == "bash":
        args[1] = "1"
    else:
        args[1] = "120"
elif mode.startswith("hang-") and args[2] != "bash":
    args[1] = "1"
os.execv("/usr/bin/timeout", ["timeout", *(["--foreground"] if foreground else []), *args])
""")
    timeout_bin.chmod(0o755)
    cases = [("graceful", "08", "PASS", "valid"),
             ("never", "02", "WARN", "valid"),
             ("graceful", "08", "PASS", "failed"),
             ("graceful", "08", "PASS", "invalid"),
             ("graceful", "08", "PASS", "busy"),
             ("graceful", "08", "PASS", "hang-top"),
             ("graceful", "08", "PASS", "hang-unload"),
             ("graceful", "08", "PASS", "hang-phase"),
             ("graceful", "08", "PASS", "slow-bays"),
             ("graceful", None, "PASS", "default-300"),
             ("graceful", None, "PASS", "default-7"),
             ("graceful", None, "PASS", "default-legacy")]
    for mode, timeout, status, inventory_mode in cases:
        home = root / f"{mode}-{inventory_mode}"
        home.mkdir()
        ready = home / "ready"
        config = "[daemon]\ndefault_idle_timeout_seconds = 120\n"
        if inventory_mode != "default-legacy":
            config += f"changer_not_ready_wait_seconds = {7 if inventory_mode == 'default-7' else 300}\n"
        (home / "config.toml").write_text(config)
        (home / "state").mkdir()
        (home / "state/selected-library").write_text("SELECTED\n")
        (home / "allowlist.txt").write_text("ALLOW1\nALLOW2 # test failure\nCLN:CLN001\n")
        (home / "bin").mkdir()
        (home / "bin/rem").symlink_to(rem)
        events = home / "events"
        fixture = scripts.parent / "tests/fixtures/stop-top.json"
        if inventory_mode == "slow-bays":
            payload = json.loads(fixture.read_text())
            for library in payload["data"]["libraries"]:
                library["drives"] = [drive for drive in library["drives"] if drive.get("element_address") != 258]
            fixture = home / "top.json"
            fixture.write_text(json.dumps(payload))
        env = dict(os.environ, REMFIELD_HOME=str(home), FIELDTEST_LOCK_HELD="1", STOP_MODE=mode,
                   TOP_FIXTURE=str(fixture),
                   READY_FILE=str(ready), EVENTS_FILE=str(events), INVENTORY_MODE=inventory_mode, PATH=f"{bin_dir}:{os.environ['PATH']}")
        env.pop("REMFIELD_STOP_TIMEOUT_SECONDS", None)
        if timeout is not None:
            env["REMFIELD_STOP_TIMEOUT_SECONDS"] = timeout
        process = subprocess.Popen([str(daemon), "--config", str(home / "config.toml")], env=env)
        try:
            deadline = time.monotonic() + 5
            while not ready.exists():
                assert time.monotonic() < deadline, "fake daemon startup"
                time.sleep(.01)
            subprocess.run(["bash", str(scripts / "03-bringup.sh"), "--stop"], env=env, check=True, timeout=40)
            process.wait(timeout=2)
            hung_pids = home / "hung-pids"
            if hung_pids.exists():
                for pid in hung_pids.read_text().splitlines():
                    state_file = Path(f"/proc/{pid}/stat")
                    assert not state_file.exists() or state_file.read_text().split()[2] == "Z", "hung CLI escaped the phase deadline"
            records = [json.loads(line) for line in (home / "evidence/records.jsonl").read_text().splitlines()]
            sequence = events.read_text().splitlines()
            assert sequence[-1] == "TERM", sequence
            calls = [json.loads(line) for line in sequence[:-1]]
            unloads = [call for call in calls if call[:2] == ["drive", "unload"]]
            if inventory_mode in ("valid", "busy", "hang-unload", "slow-bays") or inventory_mode.startswith("default-"):
                expected = ["256", "256", "256", "258", "260"] if inventory_mode == "busy" else ["256", "258", "260"]
                if inventory_mode == "slow-bays":
                    expected = ["256", "260"]
                assert [call[call.index("--bay") + 1] for call in unloads] == expected, calls
                unload_records = [row for row in records if row["test_id"].startswith("stop-unload-")]
                expected_status = ["PASS", "WARN", "PASS"]
                if inventory_mode == "slow-bays":
                    expected_status = ["PASS", "PASS"]
                    assert (home / "timeouts").read_text().splitlines() == ["60", "2220", "1080", "1080"]
                if inventory_mode == "hang-unload":
                    expected_status = ["WARN"] * 3
                assert [row["status"] for row in unload_records] == expected_status, unload_records
                if inventory_mode != "slow-bays":
                    assert "bay 258" in unload_records[1]["summary"]
                assert all(Path(row["detail_path"]).is_file() for row in unload_records)
                if inventory_mode not in ("hang-unload", "slow-bays"):
                    assert "injected unload refusal" in Path(unload_records[1]["detail_path"]).read_text()
            elif inventory_mode == "hang-phase":
                assert len(unloads) == 1, calls
                assert any(row["test_id"] == "stop-unload-phase" and row["status"] == "WARN" for row in records), records
            else:
                assert not unloads, calls
                assert any(row["test_id"] in ("stop-inventory", "stop-unload-phase") and row["status"] == "WARN" for row in records), records
            record = next(row for row in records if row["test_id"] == "stop")
            assert record["status"] == status, record
            assert record["duration_seconds"] >= 2, record
            expected_timeout = int(timeout) if timeout is not None else {"default-300": 420, "default-7": 127, "default-legacy": 720}[inventory_mode]
            assert record["timeout_seconds"] == expected_timeout, record
            if status == "WARN":
                assert "rem top --once" in record["summary"]
                assert "unthreaded in its drive" in record["summary"]
                assert "library's front panel" in record["summary"]
                assert "manual LOAD may be needed" in record["summary"]
                assert process.returncode == -9
            else:
                assert process.returncode == 0
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
    # Exercise the actual generator, not a copy of its TOML text.
    subprocess.run(["bash", "-c", 'source "$1/lib.sh"; fieldtest_write_config "$2/config.toml" SELECTED',
                    "test-config", str(scripts), str(root)], env=dict(os.environ, REMFIELD_HOME=str(root)), check=True)
    import tomllib
    assert tomllib.loads((root / "config.toml").read_text())["daemon"]["changer_not_ready_wait_seconds"] == 300
print("PASS: stop unloads only selected allowlisted media before TERM; failures warn; graceful and forced waits are evidenced")
PY
