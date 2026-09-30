#!/usr/bin/env python3
"""Validate, rebuild, privacy-scan, and check citations of the draft deposit.

Privacy scanning applies byte regexes to every member's name and contents,
including binary data. ASCII word boundaries prevent personal-name substrings
inside ordinary words or identifiers from producing false positives. Nothing
that matches is allowlisted. Binary line numbers count LF bytes, just as text
line numbers do. Interleaved-NUL data is also decoded as UTF-16LE and UTF-16BE
and rescanned. --secret-env scans literal environment values without printing
them. This does not cover base64 or other encodings, compressed/encrypted
contents, unknown secret formats, or personal names outside the listed names.
No matched secret contents are printed.

--check-text exits 2 for missing mentions/pins (pre-pin CI state), 1 for wrong
pins, and 0 when all three preparing copies quote only the built digest.
"""
from __future__ import annotations

import argparse
import io
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tempfile

from build_draft_vector_archive import ARCHIVE_NAME, ROOT, build_repository, check_specification_exclusion, specification_sources
from draft_vector_archive_verify import check_members, child_environment, safe_name, sha

SPEC_NAMES = ("rem-parity-1-specification.md", "rem-object-core-1-specification.md", "rem-encrypt-1-specification.md")
PATTERNS = {
    "home path": rb"/home/|/Users/|/root/|C:\\Users\\|C:/Users/",
    "email": rb"[a-zA-Z0-9.!#$%&'*+/=?^_`{|}~-]{1,64}@[a-zA-Z0-9](?:[a-zA-Z0-9.-]{0,251}[a-zA-Z0-9])?\.[a-zA-Z]{2,63}",
    "secret marker": rb"github_pat_|ghp_|sk-or-|AKIA|ZENODO",
    "PEM private key": rb"-----BEGIN[^\r\n]*PRIVATE KEY-----",
}


def read_archive(path: Path, expect_sha256: str | None = None) -> dict[str, bytes]:
    """Accept regular files only; validate all names before any extraction."""
    data = path.read_bytes()
    if expect_sha256 is not None:
        if not re.fullmatch(r"[0-9a-fA-F]{64}", expect_sha256):
            raise ValueError("expected SHA-256 must contain exactly 64 hexadecimal digits")
        if sha(data) != expect_sha256.lower():
            raise ValueError("archive SHA-256 does not match the externally supplied digest")
    members = {}
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:") as archive:
        for member in archive:
            safe_name(member.name)
            if not member.isfile() or member.name in members:
                raise ValueError(f"nonregular or duplicate member: {member.name}")
            members[member.name] = archive.extractfile(member).read()
    return members


def verify_archive(path: Path, expect_sha256: str | None = None, allow_skip: bool = False) -> None:
    if expect_sha256 is None:
        print("WARNING: the archive's own checksums are self-attested. Verification runs the archive's code; "
              "supply an externally recorded digest with --expect-sha256 before running it.", file=sys.stderr, flush=True)
    members = read_archive(path, expect_sha256)
    check_members(members)
    check_specification_exclusion(members, specification_sources(ROOT))
    with tempfile.TemporaryDirectory(prefix="draft-verify-") as tmp:
        root = Path(tmp)
        # Do not use extractall: regular files with prevalidated names only.
        for name, data in members.items():
            target = root / name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(data)
        command = [sys.executable, "-B", "verify.py"] + (["--allow-skip"] if allow_skip else [])
        subprocess.run(command, cwd=root, env=child_environment(root), check=True)
    print(f"PASS archive: {len(members)} members")


def load_deny_words(path: Path) -> list[str]:
    """Words to refuse, one per line; blank lines and lines starting with # are ignored.

    The list is supplied privately at scan time so that no personal name is kept in
    this public repository. A hit is reported by its number in the file, never by the word.
    """
    words = [line.strip() for line in path.read_text(encoding="utf-8").splitlines()]
    return [word for word in words if word and not word.startswith("#")]


def privacy_hits(members: dict[str, bytes], secret_env: list[str] | None = None,
                 deny_words: list[str] | None = None) -> list[str]:
    patterns = dict(PATTERNS)
    for number, word in enumerate(deny_words or [], 1):
        patterns[f"denied word #{number}"] = (
            rb"(?i)(?<![a-z0-9_])" + re.escape(word.encode()) + rb"(?![a-z0-9_])")
    secrets = []
    for variable in secret_env or []:
        value = os.environ.get(variable)
        if not value:
            raise ValueError(f"--secret-env variable is unset or empty: {variable}")
        secrets.append(value.encode())
    hits = []
    for name, data in sorted(members.items()):
        display_name = name
        for secret in secrets:
            display_name = display_name.replace(secret.decode(), "<redacted>")
        for word in deny_words or []:
            display_name = re.sub(re.escape(word), "<denied>", display_name, flags=re.IGNORECASE)
        views = [("name", name.encode(), None, 0), ("content", data, None, 0)]
        # Conservatively decode every NUL-containing member, including short values
        # and UTF-16 text embedded at an odd byte offset in a binary member.
        if b"\0" in data:
            for encoding in ("utf-16-le", "utf-16-be"):
                for alignment in (0, 1):
                    decoded = data[alignment:].decode(encoding, errors="replace").encode()
                    views.append((encoding, decoded, encoding, alignment))
        for location, content, encoding, alignment in views:
            def offset(position: int) -> int:
                if encoding is None:
                    return position
                return alignment + len(content[:position].decode().encode(encoding))

            for label, pattern in patterns.items():
                if label == "email" and b"@" not in content:
                    continue
                for match in re.finditer(pattern, content):
                    line = content.count(b"\n", 0, match.start()) + 1
                    hits.append(f"{display_name}:{line}: {label} ({location}, byte {offset(match.start())})")
            for secret in secrets:
                start = 0
                while (position := content.find(secret, start)) >= 0:
                    hits.append(f"{display_name}: byte {offset(position)}")
                    start = position + 1
    return hits


def check_text(root: Path, digest: str) -> tuple[int, list[str]]:
    missing, wrong, messages = False, False, []
    for name in SPEC_NAMES:
        path = root / "specs/in-progress" / name
        if not path.is_file():
            missing = True
            messages.append(f"MISSING preparing specification: {name}")
            continue
        text = path.read_text()
        mentions = list(re.finditer(re.escape(ARCHIVE_NAME), text))
        if not mentions:
            missing = True
            messages.append(f"UNPINNED {name}: no archive mention")
        for mention in mentions:
            # The citation's sentence, not a neighbouring archive's paragraph.
            end = text.find("\n\n", mention.end())
            # Restrict to bytes after this name and before the next archive name.
            tail = text[mention.end():end if end >= 0 else len(text)]
            tail = re.split(r"[\w-]+\.tar", tail, maxsplit=1)[0]
            pins = re.findall(r"(?<![0-9a-fA-F])[0-9a-fA-F]{64}(?![0-9a-fA-F])", tail)
            if not pins:
                # Also accept the digest immediately before the archive name.
                start = text.rfind("\n\n", 0, mention.start())
                prefix = text[start + 2 if start >= 0 else 0:mention.start()]
                prefix = re.split(r"[.!?](?=\s)|[\w-]+\.tar", prefix)[-1]
                pins = re.findall(r"(?<![0-9a-fA-F])[0-9a-fA-F]{64}(?![0-9a-fA-F])", prefix)
            line = text.count("\n", 0, mention.start()) + 1
            if not pins:
                missing = True
                messages.append(f"UNPINNED {name}:{line}: mention has no adjacent SHA-256")
            elif any(pin.lower() != digest for pin in pins):
                wrong = True
                messages.append(f"MISMATCH {name}:{line}: quoted archive SHA-256 differs")
            else:
                messages.append(f"PASS {name}:{line}: archive pin")
    return (1 if wrong else 2 if missing else 0), messages


def check_recorded(root: Path, archive: Path) -> list[str]:
    from check_spec_versioning import deposited_archive_findings, read_deposited
    record = root / "specs/publication/DEPOSITED.sha256"
    deposited, errors = read_deposited(record)
    entries = {key: value for key, value in deposited.items() if key[0] == ARCHIVE_NAME}
    if not entries:
        print("NOTE draft archive has no deposited digest")
    findings, notes = deposited_archive_findings(root, entries, {ARCHIVE_NAME: archive})
    for note in notes:
        print(note)
    return errors + findings


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", type=Path, help="validate and execute an archive")
    parser.add_argument("--expect-sha256", help="externally recorded archive digest, checked before extraction or execution")
    parser.add_argument("--allow-skip", action="store_true", help="allow unavailable claims, still reported as SKIP")
    parser.add_argument("--allow-dirty", action="store_true", help="allow a development rebuild from uncommitted sources")
    parser.add_argument("--secret-env", action="append", default=[], metavar="NAME", help="scan for a literal environment value without displaying it")
    parser.add_argument("--deny-words-file", type=Path, metavar="PATH",
                        help="private file of words the archive must not contain (personal names), one per line")
    parser.add_argument("--rebuild-compare", action="store_true")
    parser.add_argument("--privacy", action="store_true")
    parser.add_argument("--check-text", action="store_true")
    parser.add_argument("--check-recorded", action="store_true")
    args = parser.parse_args()
    if (args.secret_env or args.deny_words_file) and not args.privacy:
        parser.error("--secret-env and --deny-words-file require --privacy")
    archive = args.archive or ROOT / "dist" / ARCHIVE_NAME
    if not any((args.archive, args.rebuild_compare, args.privacy, args.check_text, args.check_recorded)):
        parser.error("choose at least one verification mode")
    if args.archive:
        verify_archive(archive, args.expect_sha256, args.allow_skip)
    elif args.expect_sha256:
        read_archive(archive, args.expect_sha256)
    if args.rebuild_compare:
        with tempfile.TemporaryDirectory(prefix="draft-rebuild-") as tmp:
            rebuilt = Path(tmp) / ARCHIVE_NAME
            build_repository(ROOT, None, rebuilt, allow_dirty=args.allow_dirty)
            if archive.read_bytes() != rebuilt.read_bytes():
                raise ValueError("rebuilt archive differs")
        print("PASS rebuild: byte-identical")
    if args.privacy:
        deny_words = load_deny_words(args.deny_words_file) if args.deny_words_file else []
        hits = privacy_hits(read_archive(archive), args.secret_env, deny_words)
        for hit in hits:
            print("PRIVACY " + hit)
        print(f"Privacy scan: {len(hits)} hits; generic patterns"
              + (f" and {len(deny_words)} denied word(s)" if deny_words else " and no denied words (supply --deny-words-file)")
              + "; all text and binary members scanned; no allowlist")
        if hits:
            return 1
    if args.check_recorded:
        errors = check_recorded(ROOT, archive)
        if errors:
            print("\n".join(errors))
            return 1
    if args.check_text:
        status, messages = check_text(ROOT, sha(archive.read_bytes()))
        print("\n".join(messages))
        return status
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (ValueError, OSError, KeyError, tarfile.TarError, subprocess.CalledProcessError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        sys.exit(1)
