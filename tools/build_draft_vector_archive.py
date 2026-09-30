#!/usr/bin/env python3
"""Build the generation-2 draft deposit from explicitly selected source roots.

Only git-tracked files under the three fixture roots below are eligible. The
frozen archive supplies its published REM-OBJECT index; it is never rebuilt.
Exported streams are selected by the pinned image manifest, never by a glob.
"""
from __future__ import annotations

import argparse
import csv
import io
from pathlib import Path
import subprocess
import tarfile
import tempfile

from draft_vector_archive_verify import check_members, safe_name, sha, verify_streams

ROOT = Path(__file__).resolve().parents[1]
ARCHIVE_NAME = "remanence-draft-test-vectors.tar"
TOOLS = ("verify_terminal_index_vectors.py", "verify_rem_object_vectors_independent.py",
         "rem_parity_rederive.py", "requirements-rem-object-independent.txt")
FIXTURE_ROOTS = ("fixtures/rem-object", "fixtures/rem-object-supplement-draft",
                 "fixtures/rem-parity-terminal-index-draft")
SPEC_NAMES = ("rem-parity-1-specification.md", "rem-object-core-1-specification.md",
              "rem-encrypt-1-specification.md", "formats-explained.md")
CLAIMS = [
    ("Tape image bytes and concatenations", "python3 verify.py --claim streams", "rem-parity-generation-2/tape-images/MANIFEST.tsv; rem-parity-generation-2/tape-images/streams/*"),
    ("Terminal-index profiles, mutations, selections and interruption cuts", "python3 tools/verify_terminal_index_vectors.py rem-parity-generation-2", "rem-parity-generation-2/*"),
    ("REM-OBJECT plaintext fixture rederivation", "python3 tools/verify_rem_object_vectors_independent.py --plaintext-only --fixture-directory rem-object/manifests", "rem-object/manifests/*"),
    ("REM-ENCRYPT OPEN and KATs", "python3 verify.py --claim objects", "rem-object/manifests/*; rem-object/objects/*; rem-object/kats/*"),
    ("REM-OBJECT supplement cases", "python3 verify.py --claim supplement", "rem-object-supplement/*"),
    ("Second implementation artifact rederivation", "python3 verify.py --claim second-build", "rem-parity-generation-2/*; rem-parity-second-implementation/build-report.json"),
    ("Second implementation damage, resume, negatives and terminal decisions", "python3 verify.py --claim second-decisions", "rem-parity-second-implementation/*; rem-parity-generation-2/tape-images/*"),
    ("Second implementation regression tests", "python3 verify.py --claim second-tests", "rem-parity-second-implementation/test_rem_parity_second_implementation.py"),
]


def tracked_files(root: Path, directory: str) -> list[str]:
    names = subprocess.check_output(["git", "ls-files", "-z", "--", directory + "/"], cwd=root).decode().split("\0")
    names = [n for n in names if n]
    if not names:
        raise ValueError(f"empty tracked source set: {directory}")
    return sorted(names)


def source_members(root: Path, images: Path) -> dict[str, bytes]:
    members = {"ARCHIVE-LAYOUT": b""}

    def add(name: str, source: Path) -> None:
        safe_name(name)
        if name in members or source.is_symlink() or not source.is_file():
            raise ValueError(f"duplicate or nonregular source: {name}")
        members[name] = source.read_bytes()

    for directory in FIXTURE_ROOTS:
        for name in tracked_files(root, directory):
            relative = name[len(directory) + 1:]
            if directory == FIXTURE_ROOTS[0]:
                destination = "rem-object/" + ("" if relative.startswith("objects/") else "manifests/") + relative
            elif directory == FIXTURE_ROOTS[1]:
                destination = "rem-object-supplement/" + relative
            elif relative.startswith("second-implementation/"):
                destination = "rem-parity-second-implementation/" + relative.removeprefix("second-implementation/")
            else:
                destination = "rem-parity-generation-2/" + relative
            add(destination, root / name)
    for name in ("xwing-draft10-kat.txt", "xwing-wrap-kat.txt"):
        add("rem-object/kats/" + name, root / "crates/remanence-aead/testdata" / name)
    with tarfile.open(root / "specs/publication/remanence-test-vectors.tar", "r:") as frozen:
        members["rem-object/vectors.json"] = frozen.extractfile("rem-object/vectors.json").read()
    for name in TOOLS:
        add("tools/" + name, root / "tools" / name)
    for name in ("rem_parity_second_implementation.py", "test_rem_parity_second_implementation.py"):
        add("rem-parity-second-implementation/" + name, root / "tools" / name)
    add("README.md", root / "tools/draft_vector_archive_README.md")
    add("verify.py", root / "tools/draft_vector_archive_verify.py")
    rows = csv.DictReader(io.StringIO(members["rem-parity-generation-2/tape-images/MANIFEST.tsv"].decode()), delimiter="\t")
    for row in rows:
        if row["tape_file"] == "ALL":
            continue
        suffix = "-torn" if row["filemark_record"] == "none" else ""
        relative = safe_name(f"{row['image']}/tape-file-{int(row['tape_file']):03}{suffix}.bin")
        add("rem-parity-generation-2/tape-images/streams/" + relative, images / relative)
    members["CLAIMS_TO_ARTIFACTS.tsv"] = ("claim\tentrypoint\tartifacts\n" + "".join("\t".join(row) + "\n" for row in CLAIMS)).encode()
    return members


def check_specification_exclusion(members: dict[str, bytes], specifications: list[Path]) -> None:
    """Exclude specification/companion member names, whole texts, and whole hashes.

    Provenance references within fixture content are retained verbatim; a
    reference to a document filename is not itself a copy of the document.
    No whole specification text or whole-text hash becomes an input to the tar writer.
    Excerpts and hashes of excerpts are not covered by this rule.
    """
    for source in specifications:
        data = source.read_bytes()
        digest = sha(data).encode()
        for name, content in members.items():
            if source.name in name or digest in content or (data and data in content):
                raise ValueError(f"specification material in archive member: {name}")


def inventories(payload: dict[str, bytes]) -> dict[str, bytes]:
    if {"MANIFEST.tsv", "CHECKSUMS.sha256"} & payload.keys():
        raise ValueError("payload must not supply generated inventories")
    members = dict(payload)
    for name in members:
        safe_name(name)
    members["MANIFEST.tsv"] = ("path\tsize\tsha256\n" + "".join(
        f"{n}\t{len(members[n])}\t{sha(members[n])}\n" for n in sorted(members))).encode()
    members["CHECKSUMS.sha256"] = "".join(f"{sha(members[n])}  {n}\n" for n in sorted(members)).encode()
    check_members(members)
    return members


def write_tar(path: Path, members: dict[str, bytes]) -> None:
    with tarfile.open(path, "w", format=tarfile.GNU_FORMAT) as archive:
        for name in sorted(members):
            safe_name(name)
            info = tarfile.TarInfo(name)
            info.size = len(members[name])
            info.mtime = info.uid = info.gid = 0
            info.uname = info.gname = ""
            info.mode = 0o755 if name == "verify.py" else 0o644
            archive.addfile(info, io.BytesIO(members[name]))


def build(payload: dict[str, bytes], destination: Path, replace: bool = False) -> None:
    """Build twice, compare, then atomically install or refuse a changed archive."""
    members = inventories(payload)
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=destination.parent, prefix=".draft-build-") as tmp:
        first, second = Path(tmp) / "first.tar", Path(tmp) / "second.tar"
        write_tar(first, members)
        write_tar(second, members)
        if first.read_bytes() != second.read_bytes():
            raise ValueError("archive is not reproducible")
        if destination.exists():
            if destination.read_bytes() == first.read_bytes():
                return
            if not replace:
                raise ValueError("existing archive differs; use --replace after reviewing the change")
        first.replace(destination)


def specification_sources(root: Path) -> list[Path]:
    """Include the published documents and every existing preparing counterpart."""
    return [root / "specs" / folder / name
            for folder in ("publication", "in-progress") for name in SPEC_NAMES
            if folder == "publication" or (root / "specs" / folder / name).exists()]


def require_clean_sources(roots: list[Path], allow_dirty: bool = False) -> None:
    """Check each source checkout, excluding ignored outputs such as dist/."""
    for root in roots:
        status = subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=all"], cwd=root)
        if status and not allow_dirty:
            raise ValueError("source tree is dirty; use --allow-dirty for tests and development")


def build_repository(root: Path, images: Path | None, output: Path, replace: bool = False,
                     allow_dirty: bool = False) -> None:
    require_clean_sources([root], allow_dirty)
    with tempfile.TemporaryDirectory(prefix="draft-images-") as tmp:
        if images is None:
            images = Path(tmp) / "streams"
            subprocess.run(["cargo", "run", "--offline", "-q", "-p", "remanence-cli", "--example",
                            "generate_tape_images", "--", "--export-images", str(images)], cwd=root, check=True)
        payload = source_members(root, images)
        check_specification_exclusion(payload, specification_sources(root))
        # Check pins, including ALL concatenations, before installing the archive.
        staging = Path(tmp) / "pins"
        for name, data in payload.items():
            if name.startswith("rem-parity-generation-2/tape-images/") and ("/streams/" in name or name.endswith("/tape-images/MANIFEST.tsv")):
                path = staging / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(data)
        verify_streams(staging)
        build(payload, output, replace)
    with tarfile.open(output, "r:") as archive:
        count = len(archive.getmembers())
    print(f"{output.name}: {output.stat().st_size} bytes, {count} members, SHA-256 {sha(output.read_bytes())}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--images", type=Path, help="streams from generate_tape_images --export-images (otherwise generate offline)")
    parser.add_argument("--output", type=Path, default=ROOT / "dist" / ARCHIVE_NAME)
    parser.add_argument("--replace", action="store_true")
    parser.add_argument("--allow-dirty", action="store_true", help="allow uncommitted source changes for tests and development")
    args = parser.parse_args()
    build_repository(ROOT, args.images, args.output, args.replace, args.allow_dirty)


if __name__ == "__main__":
    main()
