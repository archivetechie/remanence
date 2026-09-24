"""Release proof rejects stale CI, missing builders and changed payloads."""
# ruff: noqa: PT009, PT027 -- These standalone gates also run without pytest.

import argparse
import copy
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import release_evidence as release


class ReleaseEvidenceTests(unittest.TestCase):
    def test_ci_requires_exact_successful_main_push_and_repository(self):
        row = {
            "head_sha": "source",
            "head_branch": "main",
            "event": "push",
            "status": "completed",
            "conclusion": "success",
            "path": ".github/workflows/ci.yml",
            "head_repository": {"full_name": "owner/repository"},
            "id": 23,
            "html_url": "https://example.invalid/run",
        }
        self.assertEqual(
            release.eligible_runs({"workflow_runs": [row]}, "source", "owner/repository"), [row]
        )
        for field, wrong in (
            ("head_sha", "old"),
            ("head_branch", "feature"),
            ("event", "pull_request"),
            ("status", "in_progress"),
            ("conclusion", "failure"),
            ("path", ".github/workflows/other.yml"),
            ("head_repository", {"full_name": "fork/repository"}),
        ):
            candidate = {**row, field: wrong}
            self.assertFalse(
                release.eligible_runs({"workflow_runs": [candidate]}, "source", "owner/repository")
            )

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.previous_cwd = Path.cwd()
        os.chdir(self.root)
        subprocess.run(["git", "init", "-q"], check=True)
        for key, value in (
            ("user.name", "Test"),
            ("user.email", "test@example.invalid"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", "/dev/null"),
        ):
            subprocess.run(["git", "config", key, value], check=True)
        self.dist = self.root / "dist"
        self.dist.mkdir()
        self.lock = Path("Cargo.lock")
        self.lock.write_text("pinned dependencies\n")
        subprocess.run(["git", "add", "Cargo.lock"], check=True)
        subprocess.run(["git", "commit", "-qm", "fixture"], check=True)
        self.artifact = self.dist / "binary.tar.gz"
        self.artifact.write_bytes(b"actual archive bytes")
        self.context = {
            "source_sha": "source",
            "repository": "owner/repository",
            "ref": "refs/tags/v1",
            "run_id": "7",
            "run_attempt": "1",
            "workflow": "release.yml@source",
            "run_url": "https://example.invalid/run",
        }
        self.context_patch = patch.object(release, "context", return_value=self.context)
        self.context_patch.start()
        release.write(self.dist / "eligibility.json", {**self.context, "eligible": True})
        self.receipt = {
            **self.context,
            "name": "linux",
            "rustc": "rustc 1.95.0",
            "locks": {str(self.lock): release.digest(self.lock)},
            "artifacts": {
                self.artifact.name: {
                    "sha256": release.digest(self.artifact),
                    "size": self.artifact.stat().st_size,
                }
            },
        }
        self.args = argparse.Namespace(dist=self.dist, lock=[self.lock], require_build=["linux"])
        self.write_receipt()

    def tearDown(self):
        self.context_patch.stop()
        os.chdir(self.previous_cwd)
        self.temporary.cleanup()

    def write_receipt(self):
        release.write(self.dist / "build-linux.json", self.receipt)

    def test_manifest_binds_payload_and_its_own_checksum(self):
        release.manifest(self.args)
        result = json.loads((self.dist / "release-manifest.json").read_text())
        self.assertEqual(result["source_sha"], "source")
        self.assertEqual(
            result["artifacts"]["binary.tar.gz"]["sha256"], release.digest(self.artifact)
        )
        self.assertIn(
            release.digest(self.dist / "release-manifest.json"),
            (self.dist / "SHA256SUMS").read_text(),
        )

    def test_missing_builder_and_changed_bytes_fail(self):
        self.args.require_build.append("missing")
        with self.assertRaises(OSError):
            release.manifest(self.args)
        self.args.require_build.pop()
        self.artifact.write_bytes(b"changed artifact")
        with self.assertRaisesRegex(ValueError, "artifact missing or changed"):
            release.manifest(self.args)

    def test_other_source_attempt_or_locks_fail(self):
        original = copy.deepcopy(self.receipt)
        for field, value in (("source_sha", "old"), ("run_attempt", "2"), ("locks", {})):
            self.receipt = {**original, field: value}
            self.write_receipt()
            with self.assertRaisesRegex(ValueError, "identity/lockfiles mismatch"):
                release.manifest(self.args)

    def test_unrecorded_payload_and_preview_tag_fail(self):
        extra = self.dist / "unproven.zip"
        extra.write_bytes(b"no build receipt")
        with self.assertRaisesRegex(ValueError, "every published payload"):
            release.manifest(self.args)
        extra.unlink()
        release.write(self.dist / "eligibility.json", {**self.context, "eligible": False})
        with self.assertRaisesRegex(ValueError, "lacks eligibility"):
            release.manifest(self.args)

    def test_build_prefixed_unrecorded_payload_is_not_a_receipt(self):
        for name in ("build-unproven.tar.gz", "build-forged.json", "release-manifest-extra.zip"):
            extra = self.dist / name
            extra.write_bytes(b"not produced by a recorded build")
            with self.assertRaisesRegex(ValueError, "every published payload"):
                release.manifest(self.args)
            extra.unlink()

    def test_checkout_line_endings_do_not_change_committed_lock_identity(self):
        before = release.lockfiles([self.lock])
        self.lock.write_bytes(b"pinned dependencies\r\n")
        self.assertEqual(release.lockfiles([self.lock]), before)
        release.manifest(self.args)

    def test_changed_lockfile_and_traversal_fail(self):
        self.lock.write_text("changed dependency\n")
        with self.assertRaisesRegex(ValueError, "lockfile"):
            release.manifest(self.args)
        self.lock.write_text("pinned dependencies\n")
        self.receipt["locks"] = release.lockfiles([self.lock])
        self.receipt["artifacts"] = {"../outside": {"sha256": "untrusted", "size": 0}}
        self.write_receipt()
        with self.assertRaisesRegex(ValueError, "artifact missing or changed"):
            release.manifest(self.args)


if __name__ == "__main__":
    unittest.main()
