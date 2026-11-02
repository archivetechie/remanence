#!/usr/bin/env python3
"""Validate field identities and evidence; sample the daemon, never the CLI."""
import hashlib
import json
import os
from pathlib import Path
import signal
import re
import sys
import time
import tomllib

ALLOW = {"AOX032L9", "AOX036L9", "AOX038L9", "AOX058L9"}

def load(path):
    return json.loads(Path(path).read_text())

def save(path, value):
    with Path(path).open("x") as stream:
        json.dump(value, stream, sort_keys=True)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())

def process(pid, binary, config):
    proc = Path("/proc") / str(pid)
    # File-capability executables are nondumpable: /proc/PID/exe may be denied
    # even to their owner. 03 uses exec with the exact absolute binary path.
    args = proc.joinpath("cmdline").read_bytes().split(b"\0")
    assert args[0] and Path(os.fsdecode(args[0])).resolve() == Path(binary).resolve(), "wrong daemon executable"
    assert b"--config" in args and os.fsencode(config) in args, "wrong daemon config"
    uid = next(line.split()[1:3] for line in proc.joinpath("status").read_text().splitlines() if line.startswith("Uid:"))
    assert uid == [str(os.getuid())] * 2, "daemon is not owned by kit user"
    return proc

def gate_state(evidence, operation, require_holding=False):
    """Retain the gate observation even when refusing stale fault evidence."""
    gate = evidence / "gate"
    state = {"holding": (gate / "holding.json").exists(),
             "timeout": (gate / "timeout.json").exists()}
    save(evidence / (operation + "-gate-state.json"), state)
    assert not state["timeout"], "field hold expired: gate/timeout.json exists"
    assert not require_holding or state["holding"], "field hold is not active: gate/holding.json missing"
    return state

def verify(evidence, mode, control):
    gate_state(evidence, "verify")
    before, after = (load(evidence / name) for name in ("before.json", "after.json"))
    assert before["W"] < before["T"] and before["open_session"], "no short checkpoint"
    assert after["W"] == after["T"] == before["T"], "closeout changed committed range"
    assert after["sequence"] > before["sequence"] and not after["open_session"]
    assert after["C"]["lba"] > before["C"]["lba"] and "ClosedOut" in after["sidecar"]
    closed = after["closed_out"]
    assert (closed["from_w"], closed["to_t"], closed["check_before"]) == (before["W"], before["T"], before["check"]), "wrong closeout range/check"
    sidecars = [r["file_row"] for r in after["records"] if r.get("file_row") and r["file_row"]["tape_file_number"] == closed["sidecar_file_number"]]
    assert len(sidecars) == 1
    sidecar = sidecars[0]
    assert sidecar["kind"] == "ParitySidecar" and len(sidecar["canonical_metadata_hash"]) == 32
    assert (sidecar["protected_ordinal_start"], sidecar["protected_ordinal_end_exclusive"], sidecar["epoch_id"]) == (before["W"], before["T"], closed["epoch_id"])
    if mode == "measure-full-epoch":
        assert 0 < before["epoch_blocks"] - (before["T"] - before["W"]) <= 32, "not a near-full epoch"
    receipt, restored = (load(evidence / name) for name in ("receipt.json", "verified.json"))
    assert restored["sha256"] == receipt["content_sha256"] and restored["bytes"] == receipt["logical_size_bytes"]
    objects = load(evidence / "objects-after.json")["data"]["objects"]
    if not control:
        proof = load(evidence / "fault.json")
        assert proof["tape_uuid"] == before["tape_uuid"] and proof["bytes_completed"] > 0
        assert not proof["filemark"] and not proof["committed"]
        assert all(obj["object_id"] != proof["object_id"] for obj in objects), "abandoned receipt leaked"
        tails = [r["tail"] for r in after["records"] if r["sequence"] > before["sequence"] and r.get("tail")]
        assert len(tails) == 1, "unexpected tail count"
        tail = tails[0]
        assert tail["identity"] in ({"object_id":proof["object_id"]}, {"torn_sha256":proof["sha256"]}), "unexpected tail identity"
        assert tail["unknown_blocks"] == 0 and tail["filemark"] == "Absent"
        assert tail["known_blocks"] * before["block_size"] == proof["bytes_completed"], "extra abandoned writes"
    trace = (evidence / "commands.jsonl").read_bytes()
    commands = [json.loads(line) for line in trace.splitlines()]
    offset = before["trace_offset"] if control else proof["trace_offset"]
    assert isinstance(offset, int) and 0 < offset <= len(trace) and trace[offset-1:offset] == b"\n"
    subsequent = [json.loads(line) for line in trace[offset:].splitlines()]
    writes = [(i,r) for i,r in enumerate(subsequent) if r["cdb"][0] == 10]
    writes_after = sum(r["bytes_transferred"] for _,r in writes)
    assert writes_after == (after["C"]["lba"]-before["C"]["lba"]-1)*before["block_size"], "extra/missing closeout writes"
    assert writes, "missing sidecar write"
    marks = [i for i,r in enumerate(subsequent) if i > writes[-1][0] and r["cdb"][0] == 0x10 and r["cdb"][2:5] == [0,0,1]]
    assert marks and any(i > marks[0] and r["cdb"][0] == 0x10 and r["cdb"][2:5] == [0,0,0] for i,r in enumerate(subsequent)), "missing sidecar WRITE -> filemark -> barrier order"
    watches = [json.loads(line) for line in (evidence / "closeout.jsonl").read_text().splitlines()]
    assert watches and any(r["progress"].get("phase") == "complete" for r in watches)
    watches += [json.loads(line) for line in (evidence / "progress.jsonl").read_text().splitlines()]
    memory = [json.loads(line) for line in (evidence / "memory.jsonl").read_text().splitlines()]
    assert memory and all(r["VmRSS_kib"] > 0 and r["VmHWM_kib"] > 0 for r in memory)
    by_pid = {}
    for row in memory:
        by_pid.setdefault(row["pid"], []).append(row)
    peak = max(int(r["progress"].get("peak_reserved_bytes", 0)) for r in watches)
    assert 0 < peak <= load(evidence / "identity.json")["reservation_ceiling_bytes"], "missing/excessive reservation"
    stage_durations = []
    by_operation = {}
    for row in watches:
        if "elapsed_ms" in row["progress"]:
            by_operation.setdefault(row["operation_id"], []).append(row["progress"])
    for operation, rows in by_operation.items():
        rows.sort(key=lambda r: int(r["elapsed_ms"]))
        for first, second in zip(rows, rows[1:]):
            stage_durations.append({"operation_id": operation, "phase": first.get("phase"),
                                    "duration_ms": int(second["elapsed_ms"])-int(first["elapsed_ms"])})
    host = [json.loads(line) for line in (evidence / "host-stages.jsonl").read_text().splitlines()]
    assert any(r["stage"] == "checksum" and int(r["duration_ns"]) > 0 for r in host)
    assert any(r["stage"] == "native_publication_fsync" for r in host)
    command_times = {name: sum(int(r["duration_ns"]) for r in commands if r["cdb"][0] in opcodes)
                     for name, opcodes in {"read": [8], "write": [10], "position": [1, 0x11, 0x2b, 0x34, 0x92], "filemark_and_barrier": [0x10]}.items()}
    save(evidence / "metrics.json", {
        "sidecar_identity": sidecar,
        "transport_duration_ns": command_times,
        "host_stages": host,
        "attempts": [{"operation_id": r["operation_id"], "attempt": r["progress"].get("attempt")} for r in watches],
        "daemon_memory": [{"pid": pid, "baseline_kib": rows[0]["VmRSS_kib"],
                           "baseline_hwm_kib": rows[0]["VmHWM_kib"],
                           "peak_kib": max(r["VmHWM_kib"] for r in rows),
                           "delta_kib": max(r["VmHWM_kib"] for r in rows)-rows[0]["VmHWM_kib"]} for pid, rows in by_pid.items()],
        "bytes_read": sum(r["bytes_transferred"] for r in commands if r["cdb"][0] == 8),
        "bytes_written": sum(r["bytes_transferred"] for r in commands if r["cdb"][0] == 10),
        "peak_reserved_bytes": peak,
        "closeout_stage_durations": stage_durations,
        "closeout_stages": [{"phase":r["progress"].get("phase"), "elapsed_ms":r["progress"].get("elapsed_ms")} for r in watches],
        "command_duration_ns": sum(int(r["duration_ns"]) for r in commands),
        "abandoned_receipts": 0})

def main():
    command, root, *args = sys.argv[1:]
    e = Path(root)
    if command == "identity":
        barcode, config_path = args
        assert barcode in ALLOW
        config = tomllib.loads(Path(config_path).read_text())
        assert [r["serial"] for r in config["libraries"]] == ["DEC418146K_LL01"]
        tapes = load(e / "catalog.json")["data"]["tapes"]
        assert all(t.get("state") == "retired" or t.get("voltag") in ALLOW for t in tapes), "catalog outside allowlist"
        selected = [t for t in tapes if t.get("voltag") == barcode and t.get("state") != "retired"]
        assert len(selected) == 1
        tape = selected[0]
        tape_id = tape["tape_uuid"]
        log = Path(config["journal"]["dir"]) / "tapes" / (tape_id.replace("-", "") + ".remtape")
        (e / "identity.args").write_text(f'{tape_id}\n{tape["pool_id"]}\n{log}\n')
        ceiling = config["daemon"]["io_memory_ceiling"]
        amount, unit = re.fullmatch(r"(\d+)(KiB|MiB|GiB|B)", ceiling).groups()
        ceiling = int(amount) * {"KiB":1024,"MiB":1024**2,"GiB":1024**3,"B":1}[unit]
        save(e / "identity.json", {"tape":tape, "reservation_ceiling_bytes":ceiling,
                                  "library_and_drives":load(e / "top-before.json")["data"]["libraries"],
                                  "drive_catalog":load(e / "drives.json"), "library_inventory":load(e / "library.json")})
    elif command == "pid":
        matches = []
        for p in Path("/proc").iterdir():
            if not p.name.isdecimal():
                continue
            try:
                process(int(p.name), *args)
                matches.append(int(p.name))
            except (OSError, AssertionError):
                pass
        assert len(matches) == 1, "expected one exact daemon PID"
        (e / "daemon.pid").write_text(str(matches[0]))
    elif command in ("stop-helper", "stop-samplers"):
        names = ["helper"] if command == "stop-helper" else ["sampler", "restarted-sampler"]
        for name in names:
            file = e / (name + ".pid")
            if not file.exists(): continue
            pid = int(file.read_text())
            proc = Path(f"/proc/{pid}")
            try:
                cmd = proc.joinpath("cmdline").read_bytes().split(b"\0")
                if not any(cmd): continue
                if name == "helper":
                    assert proc.joinpath("exe").resolve() == Path(args[0]).resolve()
                    assert os.fsencode(e) in cmd
                else:
                    assert os.fsencode(Path(__file__).resolve()) in cmd and b"sample" in cmd and os.fsencode(e) in cmd
                os.kill(pid, signal.SIGTERM)
                for _ in range(100):
                    if not proc.exists() or not proc.joinpath("cmdline").read_bytes(): break
                    time.sleep(.01)
                else: raise AssertionError("field child did not stop")
            except FileNotFoundError:
                pass
        if command == "stop-helper" and (e / "gate/holding.json").exists() and not (e / "kill.json").exists():
            deadline = time.monotonic() + 5
            while not (e / "gate/cancelled.json").exists():
                assert not (e / "gate/timeout.json").exists(), "field hold expired during cleanup"
                assert time.monotonic() < deadline, "helper stopped but daemon has not acknowledged cancellation; hold not released"
                time.sleep(.01)
            cancelled, held = load(e / "gate/cancelled.json"), load(e / "gate/holding.json")
            assert all(cancelled[k] == held[k] for k in ("pid", "nonce", "tape_uuid")), "cleanup cancellation differs from hold"
    elif command in ("sample", "sample-once"):
        pid = int((e / "daemon.pid").read_text())
        start = time.monotonic_ns()
        while True:
            try:
                values = {}
                for line in Path(f"/proc/{pid}/status").read_text().splitlines():
                    if line.startswith(("VmRSS:", "VmHWM:")):
                        key, value, _unit = line.split()
                        values[key.rstrip(":") + "_kib"] = int(value)
                if len(values) != 2:
                    return
                print(json.dumps(dict(values, pid=pid, wall_ns=time.time_ns(), monotonic_ns=time.monotonic_ns(), elapsed_ns=time.monotonic_ns()-start)), flush=True)
                if command == "sample-once": return
                time.sleep(0.1)
            except FileNotFoundError:
                return
    elif command in ("wait", "settled"):
        assert not (e / "gate/timeout.json").exists(), "field hold expired: gate/timeout.json exists"
        target = e / ("prepared.json" if command == "settled" or args and args[0] else "fault.json")
        deadline = time.monotonic() + 14400
        while not target.exists():
            assert not (e / "gate/timeout.json").exists(), "field hold expired: gate/timeout.json exists"
            pid = int((e / "helper.pid").read_text())
            proc = Path(f"/proc/{pid}/stat")
            assert proc.exists() and proc.read_text().split(") ", 1)[1][0] != "Z", "helper failed; inspect stderr"
            assert time.monotonic() < deadline, "field gate timed out"
            time.sleep(0.1)
    elif command == "kill":
        proof = load(e / "fault.json")
        pid = int((e / "daemon.pid").read_text())
        assert proof["pid"] == pid
        process(pid, *args)
        state = gate_state(e, "kill", require_holding=True)
        holding = load(e / "gate/holding.json")
        assert all(holding[key] == proof[key] for key in ("pid", "nonce", "tape_uuid", "object_id")), "live hold differs from dispatch proof"
        assert not (e / "gate/timeout.json").exists(), "field hold expired before SIGKILL"
        assert (e / "gate/holding.json").exists(), "field hold ended before SIGKILL"
        os.kill(pid, signal.SIGKILL)
        save(e / "kill.json", {"pid":pid,"signal":"SIGKILL","wall_ns":time.time_ns(),"monotonic_ns":time.monotonic_ns(),"gate":state})
        for _ in range(100):
            try:
                process(pid, *args)
            except (OSError, AssertionError):
                return
            time.sleep(0.1)
        raise AssertionError("killed daemon did not exit")
    elif command == "verify":
        verify(e, *args)
    elif command == "manifest":
        required = ["KIT-COMMIT", "SOURCE-COMMIT", "binary-sha256.txt", "identity.json", "drives.json", "library.json", "progress.jsonl", "session.json", "receipt.json", "before.json", "after.json", "closeout-key.json", "closeout.jsonl", "metrics.json", "verified.json", "commands.jsonl", "host-stages.jsonl", "memory.jsonl", "objects-after.json"]
        if not os.environ.get("FIELD_SESSION_CONTROL"):
            required += ["fault.json", "gate/consumed.json"]
            if args[0] == "kill-mid-write":
                required += ["kill.json", "crash-preview.json"]
                if load(e / "crash-preview.json").get("found"):
                    required.append("crash-ack.json")
            elif args[0] == "failed-append":
                required += ["append-error.json", "abort.json"]
            else:
                required += ["stream-cancelled.json", "abort.json"]
        assert all((e / f).stat().st_size > 0 for f in required)
        save(e / "manifest.json", {"mode":args[0],"barcode":args[1],"wall_ns":time.time_ns(),"monotonic_ns":time.monotonic_ns(),
             "files":{str(f.relative_to(e)): hashlib.sha256(f.read_bytes()).hexdigest() for f in sorted(e.rglob("*")) if f.is_file()}})
    else:
        raise SystemExit("unknown evidence operation")

if __name__ == "__main__":
    main()
