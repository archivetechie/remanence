#!/usr/bin/env bash
# Exercise tape selection, stop ordering and measurement evidence with isolated commands.
set -euo pipefail
scripts="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../scripts" && pwd)"
python3 - "$scripts" <<'PY'
import fcntl
import ast
import json
import os
import re
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

scripts = Path(sys.argv[1])
# Parse every Python heredoc with the oldest supported grammar and reject imports
# unavailable on the field host, including imports hidden in unexecuted branches.
for path in (scripts / "35-read-rate.sh", scripts.parent / "tests/test-read-rate.sh"):
    for source in re.findall(r"<<'PY'[^\n]*\n(.*?)\nPY\b", path.read_text(), re.S):
        tree = ast.parse(source, feature_version=(3, 9))
        for node in ast.walk(tree):
            if isinstance(node, ast.Import):
                assert all(alias.name.split(".")[0] != "tomllib" for alias in node.names), path
            if isinstance(node, ast.ImportFrom):
                assert node.module != "tomllib", path
tape = "11111111-1111-4111-8111-111111111111"
foreign = "22222222-2222-4222-8222-222222222222"
mock = r'''#!/usr/bin/env python3
import json, os, sys, time
from pathlib import Path
home = Path(os.environ["REMFIELD_HOME"])
a = sys.argv[1:]
mode = os.environ.get("MOCK_MODE", "normal")
name = Path(sys.argv[0]).name
def opt(flag):
    return a[a.index(flag)+1]
def emit(value):
    print(json.dumps(value))
with (home / "calls.jsonl").open("a") as h:
    h.write(json.dumps([name, *a]) + "\n")
tape = "11111111-1111-4111-8111-111111111111"
foreign = "22222222-2222-4222-8222-222222222222"
if name == "03-bringup.sh":
    assert a == ["--stop"]
    if mode == "stop-failure":
        raise SystemExit(1)
    (home / "stopped").touch()
elif a[0] == "daemon":
    raise SystemExit(0 if mode == "still-running" or not (home / "stopped").exists() else 1)
elif a[0] == "top":
    emit({"data": {"libraries": [{"library": {"library_serial": "TESTLIB"},
        "slots": [], "drives": [{"loaded_tape_uuid": tape.replace("-", ""), "status": "busy" if mode == "busy" else "idle"}]}]}})
elif a[:2] == ["catalog", "tapes"]:
    emit({"data": {"tapes": [
        {"tape_uuid": foreign, "voltag": "FOREIGNL9", "state": "sealed", "last_committed_tape_file": "100"},
        {"tape_uuid": tape, "voltag": "BENCHL9", "state": "ready" if mode == "written" else "sealed",
         "last_committed_tape_file": os.environ.get("MOCK_LAST", "60")}]}})
elif a[0] == "library":
    assert (home / "stopped").exists(), "discovery precedes daemon stop"
    assert a[1:] == ["TESTLIB", "--slots", "--json"]
    emit({"serial": "TESTLIB", "slots": [], "drives": [{"loaded": True, "loaded_tape": "BENCHL9",
          "source_slot_raw": None if mode == "no-home" else 1024}]})
elif "read-rate" in a:
    assert (home / "stopped").exists(), "measurement precedes daemon stop"
    assert opt("--allow") == opt("--library") == "TESTLIB"
    assert opt("--config") == str(home / "config.toml")
    assert opt("--expected-home-slot") == "1024" and "--json" in a
    assert a[a.index("read-rate")+1] == "BENCHL9"
    count, start = int(opt("--files")), int(opt("--from-file"))
    if mode == "timeout":
        time.sleep(3)
    if mode == "nonzero":
        emit({"ok": False, "outcome": "command_error"})
        raise SystemExit(2)
    if mode == "cleanup-error":
        emit({"ok": False, "outcome": "complete", "cleanup_error": "park failed"})
        raise SystemExit(1)
    if mode == "malformed":
        print("broken json")
        raise SystemExit(0)
    emit({"schema": "rem.tape.read-rate.v1", "ok": mode != "medium-error",
          "outcome": "medium_error" if mode == "medium-error" else "complete", "tape_uuid": foreign if mode == "wrong-tape" else tape,
          "barcode": "BENCHL9", "mode": opt("--mode"), "from_file": start, "files_requested": count,
          "files": [{"tape_file": start+i} for i in range(count)],
          "lookahead": {"outcome": "end_of_data" if mode == "lookahead-eod" else "complete"}})
else:
    raise SystemExit("unexpected command: " + repr(a))
'''
ast.parse(mock, feature_version=(3, 9))
with tempfile.TemporaryDirectory(prefix="read-rate-test-") as temp:
    root = Path(temp)
    isolated = root / "kit/fieldtest/scripts"
    isolated.mkdir(parents=True)
    for name in ("lib.sh", "35-read-rate.sh"):
        shutil.copyfile(scripts / name, isolated / name)
    # The real stop helper must never signal a process during this test.
    stop = isolated / "03-bringup.sh"
    stop.write_text('#!/usr/bin/env bash\nexec "$REMFIELD_HOME/bin/03-bringup.sh" "$@"\n')
    home = root / "home"
    for name in ("bin", "state", "evidence"):
        (home / name).mkdir(parents=True)
    (home / "state/selected-library").write_text("TESTLIB\n")
    (home / "config.toml").write_text(f'[daemon]\nstate_dir = "{home / "state"}"\n')
    for name in ("rem", "rem-debug", "03-bringup.sh"):
        path = home / "bin" / name
        path.write_text(mock)
        path.chmod(0o755)
    env = dict(os.environ, HOME=str(root), REMFIELD_HOME=str(home), REMFIELD_ENV="vtl",
               FIELD_BUSY_WAIT_S="0", FIELD_BUSY_RETRY_INTERVAL_S="0")
    for name in ("FIELDTEST_LOCK_HELD", "FIELDTEST_FENCE_COUNTERS_FILE", "FIELD_READ_RATE_FILES",
                 "FIELD_READ_RATE_TIMEOUT_SECONDS", "FIELD_READ_RATE_CATALOG", "FIELD_READ_RATE_BAYS", "MOCK_LAST"):
        env.pop(name, None)
    calls_path = home / "calls.jsonl"
    leg = isolated / "35-read-rate.sh"
    evidence = home / "evidence/35-read-rate"
    def run(mode="normal", *args, ok=True, stopped=False, **overrides):
        (home / "stopped").unlink(missing_ok=True)
        if stopped:
            (home / "stopped").touch()
        calls_path.write_text("")
        result = subprocess.run(["bash", str(leg), *args], env=dict(env, MOCK_MODE=mode, **overrides),
                                capture_output=True, text=True, timeout=20)
        if (result.returncode == 0) != ok:
            raise AssertionError(f"{mode}: rc={result.returncode}\n{result.stdout}\n{result.stderr}")
        return [json.loads(x) for x in calls_path.read_text().splitlines()]
    assert not run("normal", "--help")
    calls = run()
    reads = [c for c in calls if "read-rate" in c]
    assert [(c[c.index("--mode")+1], c[c.index("--from-file")+1]) for c in reads] == [
        ("read", "1"), ("space", "1"), ("read", "31"), ("space", "31")]
    assert all(c[c.index("--files")+1] == "20" for c in reads)
    assert sum(c == ["03-bringup.sh", "--stop"] for c in calls) == 1
    assert (home / "stopped").exists()
    outputs = list(evidence.glob("*-from-*.json"))
    assert len(outputs) == 4
    for path in outputs:
        assert json.loads(path.read_text())["outcome"] == "complete"
    catalog = next(evidence.glob("*-catalog.json"))
    bays = next(evidence.glob("*-bays.json"))
    calls = run("normal", stopped=True, FIELD_READ_RATE_CATALOG=str(catalog), FIELD_READ_RATE_BAYS=str(bays))
    assert not any(c[0] == "03-bringup.sh" for c in calls)
    assert len([c for c in calls if "read-rate" in c]) == 4
    run("normal", ok=False, FIELD_READ_RATE_CATALOG=str(catalog), FIELD_READ_RATE_BAYS=str(bays))
    run("normal", ok=False, FIELD_READ_RATE_CATALOG=str(catalog))
    calls = run("written", "--barcode", "BENCHL9", FIELD_READ_RATE_FILES="40")
    assert [c[c.index("--from-file")+1] for c in calls if "read-rate" in c] == ["1", "1", "20", "20"]
    # The full-window threshold reserves a next-file READ after both windows.
    for last, count, starts, partial in [(1, 1, [1], True), (2, 1, [1], True),
                                        (3, 1, [1, 2], True), (21, 19, [1, 2], True),
                                        (22, 20, [1, 2], False)]:
        before = set(evidence.glob("*-selected.json"))
        calls = run(MOCK_LAST=str(last))
        reads = [c for c in calls if "read-rate" in c]
        assert [int(c[c.index("--from-file")+1]) for c in reads] == [s for s in starts for _ in range(2)]
        assert all(int(c[c.index("--files")+1]) == count for c in reads)
        selected = json.loads(next(iter(set(evidence.glob("*-selected.json")) - before)).read_text())
        assert selected["partial"] is partial and selected["files_requested"] == 20
        records = [json.loads(x) for x in (home / "evidence/records.jsonl").read_text().splitlines()]
        measurements = records[-len(reads):]
        assert all(r["status"] == ("INFO" if partial else "PASS") for r in measurements)
        if partial:
            assert all("partial measurement" in r["summary"] for r in measurements)
        if last >= 3:
            assert all(s + count <= last for s in starts)
    calls = run(ok=False, MOCK_LAST="0")
    assert not any("read-rate" in c for c in calls)
    assert len([c for c in run("lookahead-eod") if "read-rate" in c]) == 4
    for mode, args in [("written", ()), ("normal", ("--barcode", "FOREIGNL9")), ("busy", ()),
                       ("stop-failure", ()), ("still-running", ()), ("no-home", ())]:
        calls = run(mode, *args, ok=False)
        assert not any("read-rate" in c for c in calls)
    with (home / "state/state.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        calls = run(ok=False)
        assert not any("library" in c or "read-rate" in c for c in calls)
    for mode in ("medium-error", "wrong-tape"):
        calls = run(mode, ok=False)
        assert len([c for c in calls if "read-rate" in c]) == 4
    for mode in ("nonzero", "cleanup-error", "malformed"):
        calls = run(mode, ok=False)
        assert len([c for c in calls if "read-rate" in c]) == (4 if mode == "malformed" else 1)
    calls = run("timeout", ok=False, FIELD_READ_RATE_TIMEOUT_SECONDS="1")
    assert len([c for c in calls if "read-rate" in c]) == 1
    for count in ("0", "-1", "abc"):
        calls = run(ok=False, FIELD_READ_RATE_FILES=count)
        assert not calls
    records = [json.loads(x) for x in (home / "evidence/records.jsonl").read_text().splitlines()]
    assert sum(r["status"] == "PASS" and r["script"] == "35-read-rate" for r in records) == 20
    assert any(r["status"] == "FAIL" and r["test_id"] == "daemon" for r in records)
    print("PASS: default sealed and explicit written selection, library membership, exact home, midpoint clamping")
    print("PASS: stop helper ordering, stopped snapshots, live-daemon and held-lock refusal, no automatic restart")
    print("PASS: four JSON artifacts and evidence records, partial outcomes, identity mismatch, malformed output, timeout")
    print("PASS: Python 3.9 heredocs reject tomllib, short tapes are partial, lookahead EOD succeeds")
    print("PASS: command, cleanup and timeout exits skip remaining measurements")
PY
