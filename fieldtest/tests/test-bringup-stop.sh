#!/usr/bin/env bash
# Exercise graceful and forced daemon stops with process-only fixtures.
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
    if os.environ["STOP_MODE"] == "graceful" and stop_at is None:
        stop_at = time.monotonic() + 2
signal.signal(signal.SIGTERM, stop)
Path(os.environ["READY_FILE"]).touch()
while stop_at is None or time.monotonic() < stop_at:
    time.sleep(.05)
''')
    daemon.chmod(0o755)
    for mode, timeout, status in [("graceful", "08", "PASS"), ("never", "02", "WARN")]:
        home = root / mode
        home.mkdir()
        ready = home / "ready"
        env = dict(os.environ, REMFIELD_HOME=str(home), FIELDTEST_LOCK_HELD="1",
                   REMFIELD_STOP_TIMEOUT_SECONDS=timeout, STOP_MODE=mode,
                   READY_FILE=str(ready), PATH=f"{bin_dir}:{os.environ['PATH']}")
        process = subprocess.Popen([str(daemon), "--config", str(home / "config.toml")], env=env)
        try:
            deadline = time.monotonic() + 5
            while not ready.exists():
                assert time.monotonic() < deadline, "fake daemon startup"
                time.sleep(.01)
            subprocess.run(["bash", str(scripts / "03-bringup.sh"), "--stop"], env=env, check=True, timeout=12)
            process.wait(timeout=2)
            records = [json.loads(line) for line in (home / "evidence/records.jsonl").read_text().splitlines()]
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
print("PASS: graceful stop waits for dismount; forced stop records WARN and duration")
PY
