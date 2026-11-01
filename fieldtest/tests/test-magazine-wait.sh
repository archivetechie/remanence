#!/usr/bin/env bash
# Exercise 36-magazine-wait.sh against a fake rem client: a load that waits on the
# library and completes, one that never waits, one that fails, and no terminal.
set -euo pipefail
scripts="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../scripts" && pwd)"
python3 - "$scripts" <<'PY'
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

scripts = Path(sys.argv[1])
SERIAL = "TESTLIB_LL01"
FAKE_REM = r'''#!/usr/bin/env python3
import json, os, sys, time
from pathlib import Path
home = Path(os.environ["REMFIELD_HOME"])
state_path = home / "fake-state.json"
log = home / "log" / "rem-daemon.log"
def load():
    return json.loads(state_path.read_text())
def save(state):
    tmp = state_path.with_suffix(".tmp")
    tmp.write_text(json.dumps(state))
    tmp.replace(state_path)
def note(line):
    with log.open("a") as handle:
        handle.write(line + "\n")
def arg(name):
    return sys.argv[sys.argv.index(name) + 1]
args = sys.argv[1:]
mode = os.environ["FAKE_MODE"]
if args[:1] == ["top"]:
    state = load()
    bay = state["bay_tape"]
    lib = {
        "library": {"library_serial": os.environ["FAKE_SERIAL"]},
        "drives": [{"element_address": 256, "loaded_tape_barcode": bay, "loaded_tape_uuid": None}],
        "slots": [{"element_address": address, "voltag": None if barcode == bay else barcode}
                  for barcode, address in state["home"].items()],
    }
    if state["wait"]:
        lib["changer_wait"] = state["wait"]
    print(json.dumps({"libraries": [lib]}))
elif args[:2] == ["drive", "load"]:
    barcode = arg("--barcode")
    state = load()
    if mode in ("wait", "fail"):
        state["wait"] = "waiting for library: medium magazine removed (02/3b/12); move 0x1001 -> 0x0100; 1s of 600s"
        save(state)
        note("WARN waiting for library: medium magazine removed")
        time.sleep(4)
        state["wait"] = None
        save(state)
        if mode == "fail":
            note("WARN gave up waiting for library")
            print("error: move failed", file=sys.stderr)
            sys.exit(1)
        note(f"WARN library {os.environ['FAKE_SERIAL']} ready after 4s; re-checking 0x1001 and 0x0100 before retrying MOVE MEDIUM")
    state["bay_tape"] = barcode
    save(state)
    print(json.dumps({"loaded": barcode}))
elif args[:2] == ["drive", "unload"]:
    state = load()
    state["bay_tape"] = None
    save(state)
    print(json.dumps({"unloaded": True}))
else:
    print(f"fake rem: unexpected {args}", file=sys.stderr)
    sys.exit(2)
'''

def run(root, name, mode, answers, tty_in=None):
    home = root / name
    (home / "state").mkdir(parents=True)
    (home / "log").mkdir()
    bin_dir = root / "bin"
    (home / "state" / "selected-library").write_text(SERIAL + "\n")
    (home / "allowlist.txt").write_text("AOX032L9\nAOX036L9\n")
    (home / "fake-state.json").write_text(json.dumps(
        {"bay_tape": None, "wait": None, "home": {"AOX032L9": 4097, "AOX036L9": 4098}}))
    answers_path = home / "answers"
    answers_path.write_text(answers)
    out_path = home / "operator.out"
    env = dict(os.environ, REMFIELD_HOME=str(home), FIELD_BIN_DIR=str(bin_dir), FIELDTEST_LOCK_HELD="1",
               REMFIELD_ENV="real", FAKE_MODE=mode, FAKE_SERIAL=SERIAL,
               FIELD_MAGAZINE_TTY_IN=str(tty_in or answers_path), FIELD_MAGAZINE_TTY_OUT=str(out_path),
               FIELD_MAGAZINE_POLL_SECONDS="1", FIELD_MAGAZINE_SEE_WAIT_SECONDS="8",
               FIELD_MAGAZINE_HOLD_OPEN_SECONDS="0", FIELD_MAGAZINE_LOAD_DONE_SECONDS="20")
    proc = subprocess.run(["bash", str(scripts / "36-magazine-wait.sh")], env=env, timeout=90,
                          capture_output=True, text=True)
    records_path = home / "evidence" / "records.jsonl"
    records = {}
    if records_path.exists():
        for line in records_path.read_text().splitlines():
            row = json.loads(line)
            records[row["test_id"]] = row
    state = json.loads((home / "fake-state.json").read_text())
    operator = out_path.read_text() if out_path.exists() else ""
    return proc, records, state, operator

with tempfile.TemporaryDirectory(prefix="magazine-wait-") as tmp:
    root = Path(tmp)
    (root / "bin").mkdir()
    rem = root / "bin" / "rem"
    rem.write_text(FAKE_REM)
    rem.chmod(0o755)

    proc, records, state, operator = run(root, "wait", "wait", "nope\nopen\nclosed\n")
    assert proc.returncode == 0, (proc.stdout, proc.stderr)
    row = records["magazine-wait"]
    assert row["status"] == "PASS", row
    assert "medium magazine removed" in row["changer_wait"], row
    assert row["load_exit_code"] == 0 and row["tape_location_after"] == "bay 256", row
    assert records["unload"]["status"] == "PASS", records["unload"]
    assert state["bay_tape"] is None, state
    assert 'That was not "open"' in operator and "AOX032L9 from slot 0x1001" in operator, operator
    assert "live status: waiting for library" in operator, operator

    proc, records, state, _ = run(root, "nowait", "nowait", "open\nclosed\n")
    assert proc.returncode == 1, (proc.stdout, proc.stderr)
    row = records["magazine-wait"]
    assert row["status"] == "FAIL" and "never showed a changer wait" in row["summary"], row
    assert records["unload"]["status"] == "PASS", records
    assert state["bay_tape"] is None, state

    proc, records, state, _ = run(root, "fail", "fail", "open\nclosed\n")
    assert proc.returncode == 1, (proc.stdout, proc.stderr)
    row = records["magazine-wait"]
    assert row["status"] == "FAIL" and "did not complete" in row["summary"], row
    assert row["load_exit_code"] == 1, row
    assert "unload" not in records, records

    proc, records, state, _ = run(root, "notty", "wait", "", tty_in=root / "missing" / "tty")
    assert proc.returncode == 1, (proc.stdout, proc.stderr)
    assert records["magazine"]["status"] == "FAIL" and "no terminal" in records["magazine"]["summary"], records
    assert state["bay_tape"] is None, state
print("PASS: magazine wait leg passes a waited load, fails no wait, a failed load and no terminal")
PY
