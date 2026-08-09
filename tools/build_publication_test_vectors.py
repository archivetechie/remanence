#!/usr/bin/env python3
"""Verify and byte-identically repack the frozen publication vector archive.

The publication archive is an immutable baseline for the frozen REM-PARITY
profile.  It intentionally contains historical key-30 and intermediate/final
Bootstrap vectors that schema-major 2 production code must not understand.
Consequently this tool never imports or invokes the live Rust parity codec.

The committed tar is the sole frozen input.  Its SHA-256 is pinned below.  The
tool extracts it safely, runs the verifier entrypoints shipped inside the
packet, canonically repacks the tree, and refuses to replace the archive unless
the candidate is byte-for-byte identical to the frozen input.
"""

from __future__ import annotations

import argparse
import filecmp
import hashlib
import os
import pathlib
import shutil
import subprocess
import sys
import tarfile
import tempfile


ROOT = pathlib.Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "specs" / "publication" / "remanence-test-vectors.tar"
FROZEN_ARCHIVE_SHA256 = (
    "77be73e780e9ff2c265c8357b6ba684b4c69800213820ae1331850f742b1d83d"
)


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def artifact_checksum(root: pathlib.Path) -> str:
    files = sorted(path for path in root.rglob("*") if path.is_file())
    canonical = "".join(
        f"{sha256(path)}  {path.relative_to(root).as_posix()}\n" for path in files
    ).encode("utf-8")
    return hashlib.sha256(canonical).hexdigest()


def add_tree(archive: tarfile.TarFile, root: pathlib.Path) -> None:
    """Add one tree with the frozen archive's deterministic metadata policy."""
    for path in sorted(root.rglob("*"), key=lambda item: item.as_posix()):
        relative = path.relative_to(root).as_posix()
        info = archive.gettarinfo(str(path), arcname=relative)
        info.uid = 0
        info.gid = 0
        info.uname = ""
        info.gname = ""
        info.mtime = 0
        info.mode = 0o755 if path.is_dir() else 0o644
        if path.is_file():
            with path.open("rb") as handle:
                archive.addfile(info, handle)
        else:
            archive.addfile(info)


def extract_frozen_archive(archive_path: pathlib.Path, destination: pathlib.Path) -> None:
    """Extract only ordinary relative files and directories from the baseline."""
    with tarfile.open(archive_path, mode="r:") as archive:
        members = archive.getmembers()
        for member in members:
            relative = pathlib.PurePosixPath(member.name)
            if (
                relative.is_absolute()
                or ".." in relative.parts
                or not (member.isfile() or member.isdir())
            ):
                raise RuntimeError(
                    f"frozen publication archive has unsafe member {member.name!r}"
                )
        archive.extractall(destination, members=members, filter="data")


def verify_frozen_tree(stage: pathlib.Path) -> None:
    """Run the two verifier routes advertised by the publication packet."""
    subprocess.run(
        [sys.executable, str(stage / "verify.py"), str(stage)],
        cwd=ROOT,
        check=True,
    )
    subprocess.run(
        [
            "uv",
            "run",
            "--isolated",
            "--no-project",
            "--with-requirements",
            str(stage / "tools" / "requirements-rem-object-independent.txt"),
            "python",
            str(stage / "tools" / "verify_rem_object_vectors_independent.py"),
            "--fixture-directory",
            str(stage / "rem-object" / "manifests"),
            "--encrypted-object-directory",
            str(stage / "rem-object" / "objects"),
            "--kat-directory",
            str(stage / "rem-object" / "kats"),
            "--publication-root",
            str(stage),
        ],
        cwd=ROOT,
        check=True,
    )


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--stage-directory",
        type=pathlib.Path,
        help="extract the verified frozen tree here and leave the tar unchanged",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    observed_sha256 = sha256(OUTPUT)
    if observed_sha256 != FROZEN_ARCHIVE_SHA256:
        raise RuntimeError(
            "refusing to build from a changed publication baseline: "
            f"expected {FROZEN_ARCHIVE_SHA256}, observed {observed_sha256}"
        )

    with tempfile.TemporaryDirectory(
        prefix="remanence-frozen-publication-vectors-"
    ) as tmp_name:
        temporary_root = pathlib.Path(tmp_name)
        stage = temporary_root / "remanence-test-vectors"
        stage.mkdir()
        extract_frozen_archive(OUTPUT, stage)
        verify_frozen_tree(stage)

        if args.stage_directory is not None:
            shutil.copytree(stage, args.stage_directory)
            print(
                f"{artifact_checksum(args.stage_directory)}  "
                f"{args.stage_directory} (verified frozen tree; tar unchanged)"
            )
            return 0

        candidate = temporary_root / "remanence-test-vectors.tar"
        with tarfile.open(candidate, mode="w", format=tarfile.PAX_FORMAT) as archive:
            add_tree(archive, stage)
        candidate_sha256 = sha256(candidate)
        if candidate_sha256 != FROZEN_ARCHIVE_SHA256 or not filecmp.cmp(
            OUTPUT, candidate, shallow=False
        ):
            raise RuntimeError(
                "canonical repack differs from the frozen publication archive: "
                f"expected {FROZEN_ARCHIVE_SHA256}, observed {candidate_sha256}"
            )

        temporary_output = OUTPUT.with_suffix(".tar.tmp")
        shutil.copyfile(candidate, temporary_output)
        os.replace(temporary_output, OUTPUT)

    print(f"{FROZEN_ARCHIVE_SHA256}  {OUTPUT.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
