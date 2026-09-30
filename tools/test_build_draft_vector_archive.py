#!/usr/bin/env python3
"""Fast synthetic regressions for the draft deposit's packaging boundary."""
import io
import os
import runpy
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import build_draft_vector_archive as builder
import verify_draft_vector_archive as verifier
from draft_vector_archive_verify import check_members, sha


class ArchiveTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.archive = self.root / builder.ARCHIVE_NAME
        # A tiny source tree: no real vectors, Rust, crypto, or git needed.
        self.source = self.root / "source"
        self.source.mkdir()
        (self.source / "hello.txt").write_bytes(b"synthetic vector\n")
        self.payload = {
            "data/hello.txt": (self.source / "hello.txt").read_bytes(),
            "verify.py": (Path(__file__).parent / "draft_vector_archive_verify.py").read_bytes(),
            "claim.py": b"print('PASS synthetic claim')\n",
            "CLAIMS_TO_ARTIFACTS.tsv": b"claim\tentrypoint\tartifacts\nsynthetic\tpython3 claim.py\tdata/hello.txt\n",
        }

    def build(self):
        builder.build(self.payload, self.archive)

    def test_determinism_metadata_long_paths_and_end_to_end(self):
        name = "data/" + "long-" * 30 + "/vector.bin"
        self.payload[name] = b"long path payload"
        self.build()
        second = self.root / "again.tar"
        builder.build(self.payload, second)
        self.assertEqual(self.archive.read_bytes(), second.read_bytes())
        with tarfile.open(self.archive) as tar:
            self.assertEqual(tar.extractfile(name).read(), self.payload[name])
            self.assertEqual(tar.getnames(), sorted(tar.getnames()))
            for member in tar:
                self.assertEqual((member.uid, member.gid, member.mtime, member.uname, member.gname), (0, 0, 0, "", ""))
                self.assertEqual(member.mode, 0o755 if member.name == "verify.py" else 0o644)
                self.assertFalse(member.pax_headers)
        verifier.verify_archive(self.archive)
        self.assertEqual(verifier.privacy_hits(verifier.read_archive(self.archive)), [])

    def test_no_specification_members_texts_or_hashes(self):
        specifications = []
        for index, name in enumerate(builder.SPEC_NAMES):
            source = self.source / name
            source.write_text(f"Synthetic specification {index}: normative text.\n")
            specifications.append(source)
        self.build()
        members = verifier.read_archive(self.archive)
        builder.check_specification_exclusion(members, specifications)
        for source in specifications:
            for name, data in ((source.name, b"renamed content"),
                               ("disguised.txt", source.read_bytes()),
                               ("digest.txt", sha(source.read_bytes()).encode())):
                with self.subTest(source=source.name, member=name), self.assertRaises(ValueError):
                    builder.check_specification_exclusion({**members, name: data}, specifications)

    def test_unsafe_names(self):
        for name in ("/absolute", "../escape", "a/../b", "a//b", "./a", "C:/a", "a\\b"):
            with self.subTest(name=name), self.assertRaises(ValueError):
                builder.build({name: b"x"}, self.archive)

    def test_refuses_overwrite_and_replace_is_explicit(self):
        self.build()
        original = self.archive.read_bytes()
        builder.build(self.payload, self.archive)
        self.payload["data/hello.txt"] = b"changed"
        with self.assertRaisesRegex(ValueError, "--replace"):
            self.build()
        self.assertEqual(self.archive.read_bytes(), original)
        builder.build(self.payload, self.archive, replace=True)
        self.assertNotEqual(self.archive.read_bytes(), original)

    def test_changed_member_and_checksums(self):
        for name in ("data/hello.txt", "CHECKSUMS.sha256"):
            with self.subTest(name=name):
                members = builder.inventories(self.payload)
                members[name] += b"changed"
                builder.write_tar(self.archive, members)
                with self.assertRaises(ValueError):
                    verifier.verify_archive(self.archive)

    def test_traversal_links_devices_and_duplicates(self):
        for name, kind in (("../escape", tarfile.REGTYPE), ("/absolute", tarfile.REGTYPE),
                           ("link", tarfile.SYMTYPE), ("hardlink", tarfile.LNKTYPE),
                           ("device", tarfile.CHRTYPE), ("fifo", tarfile.FIFOTYPE)):
            with self.subTest(name=name):
                with tarfile.open(self.archive, "w", format=tarfile.GNU_FORMAT) as tar:
                    info = tarfile.TarInfo(name)
                    info.type = kind
                    info.linkname = "data/hello.txt" if kind in (tarfile.SYMTYPE, tarfile.LNKTYPE) else ""
                    tar.addfile(info, io.BytesIO())
                with self.assertRaises(ValueError):
                    verifier.read_archive(self.archive)
        with tarfile.open(self.archive, "w") as tar:
            for _ in range(2):
                tar.addfile(tarfile.TarInfo("duplicate"), io.BytesIO())
        with self.assertRaises(ValueError):
            verifier.read_archive(self.archive)
        self.assertFalse((self.root.parent / "escape").exists())

    def test_inventory_coverage_and_duplicates(self):
        for change in (lambda m: m.pop("claim.py"), lambda m: m.update(extra=b"x"),
                       lambda m: m.update({"MANIFEST.tsv": m["MANIFEST.tsv"] + m["MANIFEST.tsv"].splitlines(keepends=True)[1]})):
            members = builder.inventories(self.payload)
            change(members)
            with self.assertRaises(ValueError):
                check_members(members)

    def test_claim_failure_is_fatal(self):
        self.payload["claim.py"] = b"raise SystemExit(7)\n"
        self.build()
        with self.assertRaises(subprocess.CalledProcessError):
            verifier.verify_archive(self.archive)

    def test_privacy_text_binary_names_and_false_substrings(self):
        clean = b"a harmless line of text, sha256 and hashes distinguished\n"
        self.assertEqual(verifier.privacy_hits({"clean": clean}), [])
        for marker in (b"/home/", b"/Users/", b"/root/", b"C:\\Users\\", b"C:/Users/", b"a@example.test",
                       b"github_pat_", b"ghp_", b"sk-or-", b"AKIA", b"ZENODO",
                       b"-----BEGIN RSA PRIVATE KEY-----"):
            with self.subTest(marker=marker):
                hits = verifier.privacy_hits({"binary": b"\x00\xff\n" + marker + b"\x00"})
                self.assertTrue(hits)
                self.assertIn("binary:2:", hits[0])

    def test_denied_words_are_whole_word_case_insensitive_and_never_printed(self):
        words = ["canaryname"]
        self.assertEqual(verifier.privacy_hits({"clean": b"canarynames canarynamed\n"}, None, words), [])
        for text in (b"CanaryName", b"a canaryname here", b"x-canaryname-y"):
            hits = verifier.privacy_hits({"f.txt": text}, None, words)
            self.assertEqual(len(hits), 1, text)
            self.assertIn("denied word #1", hits[0])
            self.assertNotIn("canaryname", hits[0].lower())
        named = verifier.privacy_hits({"canaryname.bin": b"x"}, None, words)
        self.assertEqual(len(named), 1, "a member named with a denied word is a hit")
        self.assertIn("<denied>.bin", named[0])
        self.assertNotIn("canaryname", named[0].lower())

    def test_privacy_cli_fails_on_a_real_member_hit(self):
        self.payload["data/hello.txt"] = b"line one\n/home/someone/file\n"
        self.build()
        result = subprocess.run([sys.executable, str(Path(verifier.__file__)),
                                 "--archive", str(self.archive), "--privacy"],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("data/hello.txt:2: home path", result.stdout)

    def test_privacy_cli_with_a_private_deny_words_file(self):
        self.payload["data/hello.txt"] = b"line one\nCanaryName\n"
        self.build()
        words = Path(self.archive.parent) / "deny.txt"
        words.write_text("# private list\ncanaryname\n", encoding="utf-8")
        base = [sys.executable, str(Path(verifier.__file__)), "--archive", str(self.archive), "--privacy"]
        without = subprocess.run(base, capture_output=True, text=True)
        self.assertEqual(without.returncode, 0, without.stdout)
        self.assertIn("no denied words", without.stdout)
        with_words = subprocess.run(base + ["--deny-words-file", str(words)], capture_output=True, text=True)
        self.assertEqual(with_words.returncode, 1, with_words.stdout)
        self.assertIn("data/hello.txt:2: denied word #1", with_words.stdout)
        self.assertNotIn("CanaryName", with_words.stdout + with_words.stderr)

    def spec_texts(self, text):
        directory = self.root / "specs/in-progress"
        directory.mkdir(parents=True, exist_ok=True)
        for name in verifier.SPEC_NAMES:
            (directory / name).write_text(text)

    def test_check_text_all_mentions_and_missing_state(self):
        digest = "a" * 64
        mention = f"The `{builder.ARCHIVE_NAME}`, SHA-256\n`{digest}`.\n"
        self.spec_texts(mention)
        self.assertEqual(verifier.check_text(self.root, digest)[0], 0)
        self.spec_texts(f"SHA-256 `{digest}` for `{builder.ARCHIVE_NAME}`.\n")
        self.assertEqual(verifier.check_text(self.root, digest)[0], 0)
        self.spec_texts(mention + "\n" + mention.replace(digest, "b" * 64))
        self.assertEqual(verifier.check_text(self.root, digest)[0], 1)
        self.spec_texts("Preparing; not pinned yet.\n")
        code, messages = verifier.check_text(self.root, digest)
        self.assertEqual(code, 2)
        self.assertEqual(len(messages), 3)
        self.spec_texts(f"The `{builder.ARCHIVE_NAME}`.\n")
        self.assertNotEqual(verifier.check_text(self.root, digest)[0], 0)

    def run_archive(self, *arguments):
        return subprocess.run([sys.executable, str(Path(verifier.__file__)),
                               "--archive", str(self.archive), *arguments],
                              capture_output=True, text=True)

    def test_missing_dependencies_skip_never_passes(self):
        # A meta-path finder deterministically hides each dependency, even in a venv.
        for dependency in ("cryptography", "kyber_py"):
            with self.subTest(dependency=dependency):
                self.payload["dependency_gate.py"] = (
                    "import importlib.abc, runpy, sys\n"
                    "class Missing(importlib.abc.MetaPathFinder):\n"
                    "    def find_spec(self, fullname, path=None, target=None):\n"
                    f"        if fullname == {dependency!r}: return None\n"
                    "        return next((s for f in original if (s := f.find_spec(fullname, path, target)) is not None), None)\n"
                    "original = sys.meta_path[:]\nsys.meta_path[:] = [Missing()]\n"
                    "runpy.run_path('verify.py', run_name='__main__')\n"
                ).encode()
                self.payload["CLAIMS_TO_ARTIFACTS.tsv"] = (
                    "claim\tentrypoint\tartifacts\n" + "".join(
                        f"{claim}\tpython3 dependency_gate.py --claim {claim}\tdata/*\n"
                        for claim in ("objects", "supplement", "second-tests"))
                ).encode()
                builder.build(self.payload, self.archive, replace=True)
                for arguments, status in (((), 1), (("--allow-skip",), 0)):
                    result = self.run_archive(*arguments)
                    self.assertEqual(result.returncode, status, result.stdout + result.stderr)
                    self.assertIn("Claims: 0 passed, 3 skipped, 0 failed", result.stdout)
                    for claim in ("objects", "supplement", "second-tests"):
                        self.assertIn("SKIP " + claim, result.stdout)
                        self.assertNotIn("OK " + claim, result.stdout)

    def test_digest_is_checked_before_reading_or_running_archive(self):
        self.archive.write_bytes(b"not even a tar")
        with mock.patch.object(verifier.tarfile, "open") as open_tar:
            with self.assertRaisesRegex(ValueError, "does not match"):
                verifier.verify_archive(self.archive, "0" * 64)
            open_tar.assert_not_called()
        builder.build(self.payload, self.archive, replace=True)
        verifier.verify_archive(self.archive, sha(self.archive.read_bytes()).upper())
        for digest in ("invalid", "a" * 63):
            with self.assertRaisesRegex(ValueError, "64 hexadecimal"):
                verifier.verify_archive(self.archive, digest)

    def test_warning_and_scrubbed_environment_reaches_claim(self):
        self.payload["claim.py"] = (
            "import os, pathlib\n"
            "assert 'W2_EXECUTION_CANARY' not in os.environ\n"
            "assert set(os.environ) == {'PATH', 'LANG', 'LC_ALL', 'HOME', 'PYTHONHASHSEED'}\n"
            "assert pathlib.Path(os.environ['HOME']) == pathlib.Path.cwd()\n"
            "assert os.environ['LANG'] == os.environ['LC_ALL'] == 'C.UTF-8'\n"
            "assert os.environ['PYTHONHASHSEED'] == '0'\n"
        ).encode()
        self.build()
        with mock.patch.dict(os.environ, {"W2_EXECUTION_CANARY": "private-canary"}):
            result = self.run_archive()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("self-attested", result.stderr)
        self.assertIn("archive's code", result.stderr)
        self.assertIn("--expect-sha256", result.stderr)

    def test_claim_artifacts_and_entrypoints_must_exist(self):
        for entrypoint, artifacts in (("python3 claim.py", "data/*; missing/*"),
                                      ("python3 absent.py", "data/*"),
                                      ("sh claim.py", "data/*"),
                                      ("python3 -c 'print(1)'", "data/*")):
            with self.subTest(entrypoint=entrypoint, artifacts=artifacts):
                self.payload["CLAIMS_TO_ARTIFACTS.tsv"] = (
                    f"claim\tentrypoint\tartifacts\ninvalid\t{entrypoint}\t{artifacts}\n").encode()
                builder.build(self.payload, self.archive, replace=True)
                result = self.run_archive()
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIn("Claims: 0 passed, 0 skipped, 1 failed", result.stdout)
                self.assertNotIn("PASS synthetic claim", result.stdout)

    def test_clean_tree_required_for_every_source_root(self):
        clean, dirty = self.root / "clean", self.root / "dirty"
        for root in (clean, dirty):
            root.mkdir()
            subprocess.run(["git", "init", "-q", str(root)], check=True)
        builder.require_clean_sources([clean, dirty])
        (dirty / "untracked").write_text("dirty")
        with self.assertRaisesRegex(ValueError, "--allow-dirty"):
            builder.require_clean_sources([clean, dirty])
        builder.require_clean_sources([clean, dirty], allow_dirty=True)
        # Also ensure the public build refuses before generation or source reads.
        with mock.patch.object(builder, "source_members") as read_sources:
            with self.assertRaisesRegex(ValueError, "--allow-dirty"):
                builder.build_repository(dirty, self.source, self.archive)
            read_sources.assert_not_called()
        with mock.patch.object(builder.subprocess, "check_output", return_value=b"M  staged\n"):
            with self.assertRaisesRegex(ValueError, "--allow-dirty"):
                builder.require_clean_sources([clean])

    def test_privacy_utf16_and_secret_environment_values(self):
        secret = "canary-7c48194b-private"
        with mock.patch.dict(os.environ, {"W2_SCAN_CANARY": secret}):
            for encoding in ("utf-8", "utf-16-le", "utf-16-be"):
                for prefix in (b"", b"\xff"):
                    with self.subTest(encoding=encoding, prefix=prefix):
                        data = prefix + ("prefix " + secret + " C:\\Users\\someone").encode(encoding)
                        hits = verifier.privacy_hits({"vector.bin": data}, ["W2_SCAN_CANARY"])
                        self.assertTrue(any("home path" in hit for hit in hits), hits)
                        expected_offset = len(prefix) + len("prefix ".encode(encoding))
                        self.assertIn(f"vector.bin: byte {expected_offset}", hits)
                        self.assertNotIn(secret, "\n".join(hits))
            self.payload["data/hello.txt"] = secret.encode()
            self.build()
            result = self.run_archive("--privacy", "--secret-env", "W2_SCAN_CANARY")
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
            self.assertIn("data/hello.txt: byte 0", result.stdout)
            self.assertNotIn(secret, result.stdout + result.stderr)
            hits = verifier.privacy_hits({secret + ".bin": secret.encode()}, ["W2_SCAN_CANARY"])
            self.assertTrue(hits)
            self.assertNotIn(secret, "\n".join(hits))
        with mock.patch.dict(os.environ, {"W2_SCAN_CANARY": "ab"}):
            self.assertIn("short: byte 0", verifier.privacy_hits({"short": b"a\0b\0"}, ["W2_SCAN_CANARY"]))

    def test_missing_crypto_errors_name_package_and_requirements(self):
        import verify_rem_object_vectors_independent as independent
        with mock.patch.object(independent, "CRYPTOGRAPHY_AVAILABLE", False):
            with self.assertRaisesRegex(RuntimeError, "cryptography.*requirements-rem-object-independent.txt"):
                independent.x25519_public(bytes(32))
            with self.assertRaisesRegex(RuntimeError, "cryptography.*requirements-rem-object-independent.txt"):
                independent.rederive_supplement_object({}, b"")
        with mock.patch.object(independent.importlib.metadata, "version",
                               side_effect=independent.importlib.metadata.PackageNotFoundError):
            with self.assertRaisesRegex(RuntimeError, "kyber-py.*requirements-rem-object-independent.txt"):
                independent.ml_kem_768()

    def test_archive_layout_requires_marker_not_directory(self):
        (self.root / "tools").mkdir()
        script = self.root / "tools/rem_parity_second_implementation.py"
        script.write_bytes((Path(__file__).parent / script.name).read_bytes())
        (self.root / "rem-parity-generation-2").mkdir()
        self.assertFalse(runpy.run_path(str(script))["ARCHIVE_LAYOUT"])
        (self.root / "ARCHIVE-LAYOUT").write_bytes(b"")
        self.assertTrue(runpy.run_path(str(script))["ARCHIVE_LAYOUT"])

    def test_companion_preparing_counterpart_is_excluded_when_present(self):
        companion = self.root / "specs/in-progress/formats-explained.md"
        companion.parent.mkdir(parents=True)
        self.assertNotIn(companion, builder.specification_sources(self.root))
        companion.write_bytes(b"Preparing companion text")
        self.assertIn(companion, builder.specification_sources(self.root))
        for name, data in (("formats-explained.md", b"renamed"),
                           ("text", companion.read_bytes()),
                           ("hash", sha(companion.read_bytes()).encode())):
            with self.subTest(name=name), self.assertRaises(ValueError):
                builder.check_specification_exclusion({name: data}, [companion])


    def test_readme_is_verbatim_supervisor_template(self):
        # Supervisor template, with only the requested trust paragraph added.
        expected = """# Remanence draft conformance vectors

These are the review vectors that accompany the draft revisions of the Remanence format
specifications:

- REM-PARITY 1.0.0-draft.5, the tape layout of generation 2;
- REM-OBJECT Core Format 1.0.0-draft.4;
- REM-ENCRYPT 1.0.0-draft.4.

They are review material. The specifications are not final until they freeze, which is planned for
31 July 2027, and the vectors that freeze with them replace these. Where a vector and the text of
the document it accompanies disagree, the document governs, and the disagreement is an error to
report against the vector.

The archive names document versions by their version strings and contains no specification text,
so that a specification can cite this archive by its hash and the archive does not depend on the
text.

## What is in it

| Directory | What it holds |
| --- | --- |
| `rem-object/` | The REM-OBJECT and REM-ENCRYPT positive objects, the negative cases and the known-answer files for the encrypted envelope. |
| `rem-object-supplement/` | Further REM-OBJECT cases that accompany draft.4 and are not yet part of the frozen set. |
| `rem-parity-generation-2/` | The REM-PARITY generation-2 vectors: the whole-tape images, including the byte streams of every tape file; the damage matrix, with its fault maps and the outcome each case expects; the resume vectors; the negative vectors; and the terminal-index vector sets at each legal block size. |
| `rem-parity-second-implementation/` | A second implementation of the generation-2 rules, written from the specification text alone, and the decisions it reaches on the vectors. |
| `tools/` | The verifiers that ship with the vectors. |

`MANIFEST.tsv` lists every member with its size and SHA-256. `CHECKSUMS.sha256` is the same digests in
the form `sha256sum -c` reads. `CLAIMS_TO_ARTIFACTS.tsv` says which command checks which claim, and
which members it covers.

## Checking the archive

Before running `verify.py`, compare the archive's SHA-256 with the digest recorded for it.
`verify.py` is a program the archive supplies.

Extract it and run `python3 verify.py` in the extracted directory. It checks every digest and runs
each command in `CLAIMS_TO_ARTIFACTS.tsv`. It needs Python 3.12 and the standard library, and it uses
no network. `tools/requirements-rem-object-independent.txt` lists the one package that the
independent REM-ENCRYPT verifier needs.

## Which tapes and documents these cover

Generation 1 and generation 2 are different tape layouts, and neither reads the other's tapes. A
generation-2 reader cannot read a generation-1 tape, and generation-1 tooling cannot read a
generation-2 tape. REM-PARITY 1.0.0-draft.5 and the vectors here describe generation 2. A tape
written under generation 1 is governed by the text of REM-PARITY 1.0.0-draft.2, and by the
generation-1 archive `remanence-test-vectors.tar`, which is deposited unchanged in the same record as
this archive. The generation-1 archive contains vectors that generation-2 code must not accept.

## What these vectors do not cover

- Tapes written by a physical drive. The vectors are generated, and the supervised physical-media
  validation is still open.
- The fuzz campaigns for the terminal structures.
- A second implementation from a party other than the project. The one included is technical
  independence: it was written from the text without the reference implementation, and it is not the
  independence of a second institution.

## Licence

CC0 1.0. One file reproduces third-party material: `rem-object/kats/xwing-draft10-kat.txt` is vector 1
of Appendix C of draft-connolly-cfrg-xwing-kem-10, an IETF Internet-Draft, and its first line says so.
The IETF Trust's terms, not CC0, govern that draft.
"""
        self.assertEqual((Path(__file__).parent / "draft_vector_archive_README.md").read_bytes(), expected.encode())


if __name__ == "__main__":
    unittest.main()
