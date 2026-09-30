#!/usr/bin/env python3
"""Archive-native integrity and claim runner for the draft vector deposit.

MANIFEST covers all payload files, including this runner and the claims table.
CHECKSUMS covers that same set plus MANIFEST; neither inventory hashes itself.
This explicit acyclic convention is checked before any claim is executed.
"""
from __future__ import annotations

import argparse
import csv
import fnmatch
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path, PurePosixPath
import shlex
import subprocess
import sys
import tempfile

INVENTORIES = {"MANIFEST.tsv", "CHECKSUMS.sha256"}
SKIPPED = 77


class ClaimSkipped(Exception):
    """A claim could not run its subject, rather than passing it."""


def child_environment(root: Path) -> dict[str, str]:
    """Pass only the execution path and deterministic, temporary process settings."""
    return {"PATH": os.environ.get("PATH", os.defpath), "LANG": "C.UTF-8",
            "LC_ALL": "C.UTF-8", "HOME": str(root), "PYTHONHASHSEED": "0"}


def require_dependencies() -> None:
    missing = [p for p in ("cryptography", "kyber_py") if importlib.util.find_spec(p) is None]
    if missing:
        raise ClaimSkipped("missing " + ", ".join(missing)
                           + "; install tools/requirements-rem-object-independent.txt")


def claim_command(row: dict[str, str], members: dict[str, bytes]) -> list[str]:
    """Require artifacts for every pattern and a Python script shipped in the archive."""
    for pattern in row["artifacts"].split(";"):
        pattern = pattern.strip()
        if not pattern or not any(fnmatch.fnmatchcase(name, pattern) for name in members):
            raise ValueError(f"unmatched artifact pattern: {pattern!r}")
    command = shlex.split(row["entrypoint"])
    if (not row["entrypoint"].startswith("python3 ") or len(command) < 2
            or command[0] != "python3" or command[1].startswith("-")):
        raise ValueError("claim entrypoint must begin with python3 and a script path")
    if safe_name(command[1]) not in members:
        raise ValueError(f"claim entrypoint script is not an archive member: {command[1]}")
    return [sys.executable, "-B", *command[1:]]


def safe_name(name: str) -> str:
    """Require a canonical relative POSIX filename, safe on every host."""
    path = PurePosixPath(name)
    if (not name or path.is_absolute() or any(p in ("", ".", "..") for p in name.split("/"))
            or "\\" in name or ":" in name or any(ord(c) < 32 for c in name)):
        raise ValueError(f"unsafe member name: {name!r}")
    return name


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def check_members(members: dict[str, bytes]) -> None:
    """Check exact inventory coverage, ordering, sizes, and both digest views."""
    for name in members:
        safe_name(name)
    rows = list(csv.reader(io.StringIO(members["MANIFEST.tsv"].decode()), delimiter="\t"))
    if not rows or rows.pop(0) != ["path", "size", "sha256"]:
        raise ValueError("invalid MANIFEST header")
    names = [r[0] for r in rows]
    if names != sorted(set(members) - INVENTORIES):
        raise ValueError("MANIFEST member coverage/order differs")
    for name, size, digest in rows:
        if str(len(members[name])) != size or sha(members[name]) != digest:
            raise ValueError(f"MANIFEST mismatch: {name}")
    expected = "".join(f"{sha(members[n])}  {n}\n" for n in sorted(set(members) - {"CHECKSUMS.sha256"}))
    if members["CHECKSUMS.sha256"].decode() != expected:
        raise ValueError("CHECKSUMS mismatch or coverage/order differs")


def tree_members(root: Path) -> dict[str, bytes]:
    members = {}
    for path in root.rglob("*"):
        if path.is_symlink():
            raise ValueError(f"link in extracted archive: {path}")
        if path.is_file():
            members[path.relative_to(root).as_posix()] = path.read_bytes()
        elif not path.is_dir():
            raise ValueError(f"special file in extracted archive: {path}")
    return members


def verify_streams(root: Path) -> None:
    base = root / "rem-parity-generation-2/tape-images"
    rows = list(csv.DictReader((base / "MANIFEST.tsv").open(), delimiter="\t"))
    expected = set()
    totals = {}
    counts = {}
    for row in rows:
        name = row["image"]
        if row["tape_file"] == "ALL":
            size, digest = totals.pop(name)
            if size != int(row["bytes"]) or digest.hexdigest() != row["sha256"]:
                raise ValueError(f"image concatenation mismatch: {name}")
            continue
        index = int(row["tape_file"])
        if index != counts.get(name, 0):
            raise ValueError(f"tape file ordering differs: {name}")
        counts[name] = index + 1
        suffix = "-torn" if row["filemark_record"] == "none" else ""
        relative = f"{name}/tape-file-{index:03}{suffix}.bin"
        expected.add(relative)
        data = (base / "streams" / relative).read_bytes()
        if len(data) != int(row["bytes"]) or sha(data) != row["sha256"]:
            raise ValueError(f"stream mismatch: {relative}")
        size, digest = totals.get(name, (0, hashlib.sha256()))
        digest.update(data)
        totals[name] = (size + len(data), digest)
    actual = {p.relative_to(base / "streams").as_posix() for p in (base / "streams").rglob("*") if p.is_file()}
    if totals or not counts or expected != actual:
        raise ValueError("stream coverage differs")
    print(f"PASS streams: {len(counts)} images, {len(expected)} tape files and all concatenations")


def object_claim(root: Path, supplement: bool) -> None:
    require_dependencies()
    command = [sys.executable, "-B", "tools/verify_rem_object_vectors_independent.py",
               "--fixture-directory", "rem-object/manifests",
               "--encrypted-object-directory", "rem-object/objects", "--kat-directory", "rem-object/kats"]
    if supplement:
        command += ["--supplement", "rem-object-supplement", "--published-vector-index", "rem-object/vectors.json"]
    subprocess.run(command, cwd=root, env=child_environment(root), check=True)


def second_claim(root: Path, kind: str) -> None:
    if kind == "second-tests":
        require_dependencies()
    sys.path.insert(0, str(root / "tools"))
    sys.path.insert(0, str(root / "rem-parity-second-implementation"))
    import rem_parity_second_implementation as impl
    if kind == "second-build":
        with tempfile.TemporaryDirectory() as tmp:
            report = impl.run_build(Path(tmp) / "report.json")
        if report["mismatched"] or not report["reproduced"]:
            raise ValueError(f"second build mismatch: {report}")
        print(f"PASS second build: {report['reproduced']} artifacts")
        return
    if kind == "second-tests":
        subprocess.run([sys.executable, "-B", "-m", "unittest", "discover", "-s",
                        "rem-parity-second-implementation", "-p", "test_rem_parity_second_implementation.py"],
                       cwd=root, env=child_environment(root), check=True)
        return
    base = root / "rem-parity-second-implementation"
    blind = base / "blind-inputs"
    tape = root / "rem-parity-generation-2/tape-images"
    jobs = [
        ("decisions-real-ids.json", lambda out: impl.run_decide(sorted(tape.glob("cases/*/fault-map.json")), out)),
        ("resume-decisions-real-ids.json", lambda out: impl.run_resume(sorted(tape.glob("resume/*/inputs.json")), out)),
        ("negative-decisions.json", lambda out: impl.run_negatives(blind / "negative-cases-blind.json", out)),
        ("negative-supplement-decisions.json", lambda out: impl.run_supplement(blind / "supplement-blind.json", out)),
        ("negative-e1-decisions.json", lambda out: impl.run_negatives(blind / "negatives-e1-blind.json", out, impl.e1_negatives_table())),
        ("negative-block-digests.json", lambda out: impl.run_negative_blocks(blind / "negative-cases-blind.json", blind / "supplement-blind.json", tape / "negatives/MANIFEST.tsv", out, blind / "negatives-e1-blind.json")),
        ("mutation-decisions.json", lambda out: impl.run_mutations(blind / "mutations-blind.json", out)),
        ("selection-decisions.json", lambda out: impl.run_selection(blind / "selection-blind.json", out)),
    ]
    with tempfile.TemporaryDirectory() as tmp:
        for name, run in jobs:
            out = Path(tmp) / name
            run(out)
            if out.read_bytes() != (base / name).read_bytes():
                raise ValueError(f"second implementation decision differs: {name}")
            print(f"PASS second decisions: {name}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--claim", choices=["streams", "objects", "supplement", "second-build", "second-decisions", "second-tests"])
    parser.add_argument("--allow-skip", action="store_true", help="report unavailable claims as SKIP without failing")
    args = parser.parse_args()
    root = Path(__file__).resolve().parent
    if args.claim:
        try:
            if args.claim == "streams":
                verify_streams(root)
            elif args.claim in ("objects", "supplement"):
                object_claim(root, args.claim == "supplement")
            else:
                second_claim(root, args.claim)
        except ClaimSkipped as error:
            print(f"SKIP {args.claim}: {error}", flush=True)
            print("Claims: 0 passed, 1 skipped, 0 failed", flush=True)
            return 0 if args.allow_skip else SKIPPED
        return 0
    members = tree_members(root)
    check_members(members)
    print("PASS MANIFEST and CHECKSUMS: complete member coverage", flush=True)
    with (root / "CLAIMS_TO_ARTIFACTS.tsv").open() as source:
        claims = list(csv.DictReader(source, delimiter="\t"))
    if not claims or set(claims[0]) != {"claim", "entrypoint", "artifacts"}:
        raise ValueError("invalid or empty claim table")
    passed = skipped = failed = 0
    for row in claims:
        try:
            command = claim_command(row, members)
            print("RUN " + row["claim"], flush=True)
            result = subprocess.run(command, cwd=root, env=child_environment(root))
            if result.returncode == SKIPPED:
                skipped += 1
                print("SKIP " + row["claim"], flush=True)
            elif result.returncode:
                raise ValueError(f"entrypoint exited {result.returncode}")
            else:
                passed += 1
                print("OK " + row["claim"], flush=True)
        except (ValueError, OSError, KeyError, TypeError) as error:
            failed += 1
            print(f"FAIL {row.get('claim', '<unnamed>')}: {error}", flush=True)
    print(f"Claims: {passed} passed, {skipped} skipped, {failed} failed", flush=True)
    return int(bool(failed or (skipped and not args.allow_skip)))


if __name__ == "__main__":
    sys.exit(main())
