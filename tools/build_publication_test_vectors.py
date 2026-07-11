#!/usr/bin/env python3
"""Build the deterministic RAO/REM-PARITY publication vector archive."""

from __future__ import annotations

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


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def add_tree(archive: tarfile.TarFile, root: pathlib.Path) -> None:
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


def main() -> int:
    with tempfile.TemporaryDirectory(prefix="remanence-publication-vectors-") as tmp_name:
        stage = pathlib.Path(tmp_name) / "remanence-test-vectors"
        (stage / "rao" / "manifests").mkdir(parents=True)
        (stage / "rao" / "objects").mkdir(parents=True)
        (stage / "rem-parity-1").mkdir(parents=True)

        for source in sorted((ROOT / "fixtures" / "rao").glob("*")):
            if source.is_file():
                shutil.copyfile(source, stage / "rao" / "manifests" / source.name)
        for source in sorted((ROOT / "fixtures" / "rem-parity-1").glob("*")):
            if source.is_file():
                shutil.copyfile(source, stage / "rem-parity-1" / source.name)

        claims = (
            "claim\tentrypoint\tartifacts\n"
            "RAO positive byte identity\tmake publication-test-vectors\trao/objects/*.rao\n"
            "RAO negative conformance\tcargo test -p remanence-format --test rao_negative_vectors\trao/manifests/negative-*.json\n"
            "REM-PARITY arithmetic and recovery\tcargo test -p remanence-parity\trem-parity-1/vectors.json\n"
        )
        (stage / "CLAIMS_TO_ARTIFACTS.tsv").write_text(
            claims, encoding="utf-8", newline="\n"
        )

        subprocess.run(
            [
                sys.executable,
                str(ROOT / "tools" / "verify_rao_vectors_independent.py"),
                "--export-directory",
                str(stage / "rao" / "objects"),
            ],
            cwd=ROOT,
            check=True,
        )
        subprocess.run(
            [
                "cargo",
                "run",
                "--quiet",
                "-p",
                "remanence-parity",
                "--example",
                "generate_publication_vectors",
                "--",
                str(stage / "rem-parity-1" / "minimal-image"),
            ],
            cwd=ROOT,
            check=True,
        )

        files = sorted(path for path in stage.rglob("*") if path.is_file())
        manifest = "".join(
            f"{path.relative_to(stage).as_posix()}\t{path.stat().st_size}\n"
            for path in files
        )
        (stage / "MANIFEST.tsv").write_text(manifest, encoding="utf-8", newline="\n")
        checksum_files = sorted(path for path in stage.rglob("*") if path.is_file())
        checksums = "".join(
            f"{sha256(path)}  ./{path.relative_to(stage).as_posix()}\n"
            for path in checksum_files
        )
        (stage / "CHECKSUMS.sha256").write_text(
            checksums, encoding="utf-8", newline="\n"
        )

        OUTPUT.parent.mkdir(parents=True, exist_ok=True)
        temporary_output = OUTPUT.with_suffix(".tar.tmp")
        with tarfile.open(temporary_output, mode="w", format=tarfile.PAX_FORMAT) as archive:
            add_tree(archive, stage)
        os.replace(temporary_output, OUTPUT)

    print(f"{sha256(OUTPUT)}  {OUTPUT.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
