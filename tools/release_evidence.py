#!/usr/bin/env python3
"""Bind release eligibility, build inputs and artifact bytes to one source commit."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
from pathlib import Path


def run(*command: str) -> str:
    return subprocess.check_output(command, text=True).strip()


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        result = hashlib.sha256()
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            result.update(chunk)
        return result.hexdigest()


def write(path: Path, value: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def eligible_runs(payload: dict, sha: str, repository: str) -> list[dict]:
    """Validate returned rows, independently of GitHub query filtering."""
    return [
        row
        for row in payload.get("workflow_runs", [])
        if row.get("head_sha") == sha
        and row.get("head_branch") == "main"
        and row.get("event") == "push"
        and row.get("status") == "completed"
        and row.get("conclusion") == "success"
        and row.get("path") == ".github/workflows/ci.yml"
        and (row.get("head_repository") or {}).get("full_name") == repository
        and isinstance(row.get("id"), int)
        and row.get("html_url")
    ]


def context() -> dict:
    source = run("git", "rev-parse", "HEAD")
    subprocess.run(
        ["git", "diff", "--exit-code", "HEAD", "--"], check=True, stdout=subprocess.DEVNULL
    )
    if os.environ.get("GITHUB_SHA") != source:
        raise ValueError("checked-out source does not match GITHUB_SHA")
    return {
        "source_sha": source,
        "repository": os.environ["GITHUB_REPOSITORY"],
        "ref": os.environ["GITHUB_REF"],
        "run_id": os.environ["GITHUB_RUN_ID"],
        "run_attempt": os.environ["GITHUB_RUN_ATTEMPT"],
        "workflow": os.environ["GITHUB_WORKFLOW_REF"],
        "run_url": f"https://github.com/{os.environ['GITHUB_REPOSITORY']}/actions/runs/{os.environ['GITHUB_RUN_ID']}",
    }


def gate(output: Path) -> None:
    value = context()
    if not value["ref"].startswith("refs/tags/"):
        write(
            output, {**value, "eligible": False, "reason": "preview build; publication forbidden"}
        )
        return
    tag = os.environ["GITHUB_REF_NAME"]
    if (
        run("git", "cat-file", "-t", tag) != "tag"
        or run("git", "rev-parse", tag + "^{}") != value["source_sha"]
    ):
        raise ValueError("release requires an annotated tag on the checked-out commit")
    subprocess.run(["git", "fetch", "--no-tags", "origin", "main"], check=True)
    subprocess.run(
        ["git", "merge-base", "--is-ancestor", value["source_sha"], "origin/main"], check=True
    )
    oid = run("git", "rev-parse", tag + "^{tag}")
    tag_proof = json.loads(run("gh", "api", f"repos/{value['repository']}/git/tags/{oid}"))
    if (
        tag_proof.get("sha") != oid
        or tag_proof.get("verification", {}).get("verified") is not True
        or tag_proof.get("object")
        != {
            "type": "commit",
            "sha": value["source_sha"],
            "url": tag_proof.get("object", {}).get("url"),
        }
    ):
        raise ValueError("tag identity or GitHub-verified signature is missing")
    payload = json.loads(
        run(
            "gh",
            "api",
            "-X",
            "GET",
            f"repos/{value['repository']}/actions/workflows/ci.yml/runs",
            "-f",
            "branch=main",
            "-f",
            "event=push",
            "-f",
            "head_sha=" + value["source_sha"],
            "-f",
            "status=success",
        )
    )
    runs = eligible_runs(payload, value["source_sha"], value["repository"])
    if not runs:
        raise ValueError("no successful exact-commit main-push CI run")
    write(
        output,
        {
            **value,
            "eligible": True,
            "tag_object": oid,
            "signature": tag_proof["verification"],
            "ci_runs": [
                {"id": r["id"], "url": r["html_url"], "head_sha": r["head_sha"]} for r in runs
            ],
        },
    )


def build_record(args) -> None:
    value = context()
    artifacts = sorted(
        p
        for p in args.dist.iterdir()
        if p.is_file()
        and p.name not in {"build-" + args.name + ".json", "release-manifest.json"}
        and p.name not in {"eligibility.json", "SHA256SUMS"}
    )
    if not artifacts:
        raise ValueError("build produced no artifacts")
    rust = (
        args.rust_version_file.read_text().strip()
        if args.rust_version_file
        else run("rustc", "-vV")
    )
    if not rust.startswith("rustc ") or "commit-hash:" not in rust:
        raise ValueError("actual rustc -vV output is required")
    write(
        args.dist / ("build-" + args.name + ".json"),
        {
            **value,
            "name": args.name,
            "rustc": rust,
            "cargo": args.cargo_version_file.read_text().strip()
            if args.cargo_version_file
            else run("cargo", "-V"),
            "python": run("python", "--version"),
            "runner": {
                k: os.environ.get(k)
                for k in ("RUNNER_OS", "RUNNER_ARCH", "ImageOS", "ImageVersion")
            },
            "locks": {str(p): digest(p) for p in args.lock},
            "artifacts": {
                p.name: {"sha256": digest(p), "size": p.stat().st_size} for p in artifacts
            },
        },
    )


def manifest(args) -> None:
    value = context()
    eligibility = json.loads((args.dist / "eligibility.json").read_text())
    if any(
        eligibility.get(k) != value[k]
        for k in ("source_sha", "repository", "ref", "run_id", "run_attempt")
    ):
        raise ValueError("eligibility belongs to a different source or workflow attempt")
    if value["ref"].startswith("refs/tags/") and eligibility.get("eligible") is not True:
        raise ValueError("tagged release lacks eligibility proof")
    builds = []
    locks = {str(p): digest(p) for p in args.lock}
    covered = set()
    for name in args.require_build:
        receipt = json.loads((args.dist / ("build-" + name + ".json")).read_text())
        if (
            receipt.get("name") != name
            or any(
                receipt.get(k) != value[k]
                for k in ("source_sha", "repository", "ref", "run_id", "run_attempt")
            )
            or receipt.get("locks") != locks
        ):
            raise ValueError("build identity/lockfiles mismatch: " + name)
        if not receipt.get("artifacts") or not receipt.get("rustc"):
            raise ValueError("build receipt is incomplete: " + name)
        for filename, proof in receipt["artifacts"].items():
            path = args.dist / filename
            if (
                Path(filename).name != filename
                or path.is_symlink()
                or not path.is_file()
                or digest(path) != proof["sha256"]
                or path.stat().st_size != proof["size"]
            ):
                raise ValueError("artifact missing or changed: " + filename)
            covered.add(filename)
        builds.append(receipt)
    artifacts = sorted(
        p
        for p in args.dist.iterdir()
        if p.is_file() and p.name not in {"release-manifest.json", "SHA256SUMS"}
    )
    payloads = {
        p.name
        for p in artifacts
        if p.name not in {"build-" + name + ".json" for name in args.require_build}
        and p.name != "eligibility.json"
    }
    if not builds or payloads != covered:
        raise ValueError("every published payload needs an exact build receipt")
    write(
        args.dist / "release-manifest.json",
        {
            **value,
            "schema": "release-evidence.v1",
            "eligibility": eligibility,
            "builds": builds,
            "locks": locks,
            "artifacts": {
                p.name: {"sha256": digest(p), "size": p.stat().st_size} for p in artifacts
            },
        },
    )
    with (args.dist / "SHA256SUMS").open("w") as output:
        for path in sorted([*artifacts, args.dist / "release-manifest.json"]):
            output.write(f"{digest(path)}  {path.name}\n")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    guard = sub.add_parser("gate")
    guard.add_argument("--output", type=Path, required=True)
    build = sub.add_parser("record")
    build.add_argument("--name", required=True)
    build.add_argument("--rust-version-file", type=Path)
    build.add_argument("--cargo-version-file", type=Path)
    assemble = sub.add_parser("manifest")
    assemble.add_argument("--require-build", action="append", required=True)
    for command in (build, assemble):
        command.add_argument("--dist", type=Path, required=True)
        command.add_argument("--lock", type=Path, action="append", required=True)
    args = parser.parse_args()
    try:
        if args.command == "gate":
            gate(args.output)
        elif args.command == "record":
            build_record(args)
        else:
            manifest(args)
        return 0
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as exc:
        print("release evidence FAILED:", exc)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
