#!/usr/bin/env bash
# Exercise the verification leg with isolated binaries, catalog and daemon log.
set -euo pipefail
scripts="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../scripts" && pwd)"
python3 - "$scripts" <<'PY'
import csv
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

scripts = Path(sys.argv[1])
tape = "11111111-1111-4111-8111-111111111111"
foreign = "22222222-2222-4222-8222-222222222222"
# Only elapsed/command figures explicitly supplied by the field baseline are
# measurements. Counts, buckets, bytes and parity-map command time are fixtures.
mock = r'''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
home = Path(os.environ["REMFIELD_HOME"])
a = sys.argv[1:]
mode = os.environ.get("MOCK_MODE", "baseline")
tape = "11111111-1111-4111-8111-111111111111"
foreign = "22222222-2222-4222-8222-222222222222"
def emit(value):
    print(json.dumps(value))
def opt(name):
    return a[a.index(name)+1]
with (home / "calls.jsonl").open("a") as h:
    h.write(json.dumps([Path(sys.argv[0]).name, *a]) + "\n")
if Path(sys.argv[0]).name == "remfield-io":
    if "list" in a:
        emit({"objects": [
            {"object_id": foreign, "tape_uuid": foreign, "pool_id": "fieldtest-a", "logical_size_bytes": 9999999},
            {"object_id": tape, "tape_uuid": tape.replace("-", ""), "pool_id": "fieldtest-a", "logical_size_bytes": 128}]})
    elif "read" in a:
        assert json.loads(opt("--object"))["tape_uuid"].replace("-", "") == tape.replace("-", "")
        assert opt("--length") == "128"
        if not (home / "readiness-resumed").exists():
            emit({"error": "media-readiness fence operation=" + tape})
            raise SystemExit(10)
        Path(opt("--out")).write_bytes(b"x" * 128)
        emit({"bytes": 128})
    else:
        raise SystemExit("unexpected io command")
    raise SystemExit(0)
if a[:2] == ["tape", "--help"]:
    print("Commands:\n  finalize  Close a tape" if mode != "unsupported" else "Commands:\n  init  Initialize")
elif a[:3] == ["tape", "verify-index", "--help"]:
    print("Options:\n  --ready-wait <DURATION>" if mode != "fallback" else "Options:\n  --json")
elif a[:2] == ["tape", "wait-ready"]:
    assert opt("--resume") == tape and opt("--library") == "TESTLIB" and "--wait" in a
    (home / "readiness-resumed").touch()
    emit({"elapsed_seconds": 0, "attempts": 1})
elif a[0] == "top":
    drives = [{"loaded_tape_uuid": tape, "status": "idle"}]
    if mode == "busy":
        drives[0]["status"] = "busy"
    emit({"data": {"libraries": [] if mode == "missing-library" else [
        {"library": {"library_serial": "TESTLIB"}, "slots": [], "drives": drives}]}})
elif a[:2] == ["catalog", "tapes"]:
    state = "ready" if mode in ("finalize", "unsupported") else "sealed"
    emit({"data": {"tapes": [
        {"tape_uuid": foreign, "voltag": "FOREIGN", "pool_id": "fieldtest-a", "state": "sealed"},
        {"tape_uuid": tape, "voltag": "BENCH", "pool_id": "fieldtest-a", "state": state}]}})
elif a[:2] == ["tape", "finalize"]:
    assert mode == "finalize" and opt("--tape-uuid") == opt("--ack-tape-uuid") == tape
    assert opt("--expected-pool") == "fieldtest-a" and "--wait" in a
    emit({"data": {"outcome": "finalized", "tape_uuid": tape}})
elif a[:2] == ["tape", "verify-index"]:
    assert opt("--tape-uuid") == tape
    if mode == "fallback":
        assert "--ready-wait" not in a
    else:
        assert opt("--ready-wait") == "15m"
    if mode == "timeout":
        import time
        time.sleep(5)
    scale = 0.9 if mode == "fallback" else 1
    def counts(elapsed, command, size, n=1):
        return {"elapsed_ms": str(round(elapsed*scale)), "command_ms": str(round(command*scale)),
                "max_command_ms": str(round(20000*scale)) if n else "0", "bytes": str(size),
                "locate": str(n), "space": str(n), "read_position": str(n),
                "single_read": str(2*n), "batch_read": str(4*n), "other": str(n),
                "command_latency_buckets": [str(x*n) for x in (2,2,2,2,1,1)]}
    phases = [{"phase": name, "counts": counts(elapsed, command, size)} for name, elapsed, command, size in (
        ("walk_terminal", 398000, 394000, 36000000000), ("parity_map", 27500, 27000, 1000000000),
        ("sidecar", 184000, 175000, 2000000000), ("protected_data", 266000, 135000, 36000000000))]
    phases.insert(0, {"phase": "setup", "counts": counts(0,0,0,0)})
    if mode == "rotated-log":
        (home / "log/rem-daemon.log").rename(home / "log/rem-daemon.rotated")
    if mode != "no-diag":
        with (home / "log/rem-daemon.log").open("a") as h:
            h.write("unrelated event\n")
            for p in phases[1:]:
                h.write(json.dumps({"target": "remanence_verify_diag", "phase": p["phase"], "run": mode})+"\n")
            h.write("INFO remanence_verify_diag phase=total\n")
    result = {"data": {"tape_uuid": tape, "verification_state": "verified_degraded" if mode == "degraded" else "verified_complete",
                       "io": {"phases": phases, "total": counts(875000, 731000, 75000000000, 4)}}}
    if mode == "malformed":
        result["data"]["io"]["phases"][1]["counts"]["command_latency_buckets"] = []
    emit(result)
    if mode == "nonzero":
        raise SystemExit(3)
else:
    raise SystemExit("unexpected rem command: " + repr(a))
'''
with tempfile.TemporaryDirectory(prefix="verify-throughput-test-") as temp:
    root = Path(temp)
    # Copy the leg and library so binary resolution cannot reach a real helper.
    isolated = root / "kit/fieldtest/scripts"
    isolated.mkdir(parents=True)
    for name in ("lib.sh", "34-verify-throughput.sh"):
        shutil.copyfile(scripts / name, isolated / name)
    home = root / "home"
    for name in ("bin", "state", "spool", "log", "evidence"):
        (home / name).mkdir(parents=True)
    (home / "state/selected-library").write_text("TESTLIB\n")
    (home / "log/rem-daemon.log").write_text('remanence_verify_diag excluded-start-line\n')
    for name in ("rem", "remfield-io"):
        path = home / "bin" / name
        path.write_text(mock)
        path.chmod(0o755)
    env = dict(os.environ, HOME=str(root), REMFIELD_HOME=str(home), REMFIELD_ENV="vtl",
               FIELD_BUSY_WAIT_S="0", FIELD_BUSY_RETRY_INTERVAL_S="0")
    for name in ("FIELDTEST_LOCK_HELD", "FIELDTEST_FENCE_COUNTERS_FILE", "FIELD_CONFIRM_FINALIZE", "FIELD_VERIFY_TIMEOUT_SECONDS"):
        env.pop(name, None)
    leg = isolated / "34-verify-throughput.sh"
    evidence = home / "evidence/34-verify-throughput"
    def run(mode, *args, ok=True, **overrides):
        result = subprocess.run(["bash", str(leg), *args], env=dict(env, MOCK_MODE=mode, **overrides),
                                text=True, capture_output=True, timeout=20)
        if (result.returncode == 0) != ok:
            raise AssertionError(f"{mode}: rc={result.returncode}\n{result.stdout}\n{result.stderr}")
        return result
    run("baseline")
    first = next(evidence.glob("verify-split-*.tsv"))
    baseline = first.read_text()
    rows = list(csv.DictReader(baseline.splitlines(), delimiter="\t"))
    assert [r["phase"] for r in rows] == ["walk_terminal", "parity_map", "sidecar", "protected_data", "total"]
    assert rows[3]["between_s"] == "131.000" and rows[-1]["elapsed_s"] == "875.000"
    assert rows[0]["commands"] == "10" and rows[0]["ge10s"] == "1"
    assert rows[3]["MB_per_s"] == "135.338"
    # A newer split for another tape must never become the comparison reference.
    decoy = evidence / "verify-split-foreign.tsv"
    decoy.write_text(baseline.replace(tape, foreign))
    run("fallback", "--barcode", "BENCH")
    second = next(p for p in evidence.glob("verify-split-*.tsv") if p not in (first, decoy))
    delta = (evidence / second.name.replace("verify-split-", "verify-delta-").replace(".tsv", ".txt")).read_text()
    assert f"reference={first.name}" in delta
    assert "protected_data: elapsed_s=-26.600 (-10.00%); command_s=-13.500 (-10.00%); between_s=-13.100 (-10.00%)" in delta
    assert not list((home / "spool").iterdir())
    for path in evidence.glob("verify-diag-*.jsonl"):
        lines = path.read_text().splitlines()
        assert len(lines) == 5 and "excluded-start-line" not in path.read_text()
        assert json.loads(lines[-1])["line"] == "INFO remanence_verify_diag phase=total"
    calls = [json.loads(x) for x in (home / "calls.jsonl").read_text().splitlines()]
    assert sum(c[0] == "remfield-io" and "read" in c for c in calls) == 2
    assert any(c[1:3] == ["tape", "wait-ready"] for c in calls)
    run("baseline", "--tape-uuid", tape)
    run("baseline", "--tape-uuid", foreign, ok=False)
    run("finalize", "--finalize-bench", ok=False)
    assert not any(json.loads(x)[1:3] == ["tape", "finalize"] for x in (home / "calls.jsonl").read_text().splitlines())
    run("unsupported", "--finalize-bench", ok=False, FIELD_CONFIRM_FINALIZE="1")
    run("finalize", "--finalize-bench", FIELD_CONFIRM_FINALIZE="1")
    for mode in ("busy", "missing-library", "degraded", "malformed", "no-diag", "nonzero", "rotated-log"):
        run(mode, ok=False)
    run("timeout", ok=False, FIELD_VERIFY_TIMEOUT_SECONDS="1")
    records = [json.loads(x) for x in (home / "evidence/records.jsonl").read_text().splitlines()]
    assert any(r["test_id"] == "finalize" and r["status"] == "PASS" and tape in r["summary"] for r in records)
    assert any(r["test_id"] == "verification" and r["status"] == "FAIL" and "verified_degraded" in r["summary"] for r in records)
    assert any(r["test_id"] == "delta" and "-26.600" in r["summary"] for r in records)
    print("MOCK BASELINE TSV (counts, buckets and bytes are synthetic):")
    print(baseline, end="")
    print("MOCK SECOND RUN TSV:")
    print(second.read_text(), end="")
    print("MOCK DELTAS:")
    print(delta, end="")
    print("PASS: parser, TSV, same-tape deltas, both readiness branches with resume, spool cleanup, log window")
    print("PASS: UUID/barcode/library selection, finalization guard and command, idle failure, degraded verdict")
    print("PASS: malformed timing, missing/rotated diagnostics, nonzero command, timeout")
PY
