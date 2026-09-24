"""Immutable oracle tests for the normative specification preservation gate.

The historical fixtures are public Remanence specification text captured from
two pinned commits.  The tests deliberately invoke the future command-line
tool rather than importing its implementation, so the preservation inventory
and its disposition checker are owned independently of the implementation.
"""

from __future__ import annotations

import gzip
import hashlib
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "tools" / "testdata" / "spec-preservation"
BEFORE_FIXTURE = FIXTURES / "before.md.gz"
AFTER_FIXTURE = FIXTURES / "after.md.gz"


class SpecPreservationOracle(unittest.TestCase):
    """Safety properties for inventory, disposition, and receipt checking."""

    def _text(self, path: Path) -> str:
        with gzip.open(path, "rt", encoding="utf-8") as stream:
            return stream.read()

    def _sha256(self, text: str) -> str:
        return hashlib.sha256(text.encode("utf-8")).hexdigest()

    def _run(self, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(ROOT / "tools" / "spec_preservation.py"), *args],
            cwd=ROOT,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )

    def _assert_tool_executed(self, result: subprocess.CompletedProcess[str]) -> None:
        output = result.stdout + result.stderr
        self.assertNotIn("can't open file", output.lower())
        self.assertNotIn("no such file or directory", output.lower())

    def _inventory(self, baseline: Path, candidate: Path, out: Path) -> dict:
        result = self._run(
            "inventory",
            "--baseline",
            str(baseline),
            "--candidate",
            str(candidate),
            "--out",
            str(out),
        )
        self._assert_tool_executed(result)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return json.loads(out.read_text(encoding="utf-8"))

    def _check(
        self,
        baseline: Path,
        candidate: Path,
        *,
        dispositions: Path | None = None,
        receipt: Path | None = None,
    ) -> subprocess.CompletedProcess[str]:
        args = ["check", "--baseline", str(baseline), "--candidate", str(candidate)]
        if dispositions is not None:
            args.extend(["--dispositions", str(dispositions)])
        if receipt is not None:
            args.extend(["--receipt", str(receipt)])
        result = self._run(*args)
        self._assert_tool_executed(result)
        return result

    def _write_text_fixture(self, directory: Path, name: str, text: str) -> Path:
        path = directory / name
        path.write_text(text, encoding="utf-8")
        return path

    def _line_span(self, text: str, fragment: str, start: int = 1, end: int | None = None) -> list[int]:
        """Find an exact line span using one-based inclusive line numbers."""
        lines = text.splitlines()
        fragment_lines = fragment.splitlines()
        limit = end or len(lines)
        for index in range(max(0, start - 1), min(len(lines), limit) - len(fragment_lines) + 1):
            if lines[index:index + len(fragment_lines)] == fragment_lines:
                return [index + 1, index + len(fragment_lines)]
        self.fail(f"fixture fragment was not found in candidate: {fragment!r}")

    def _section_span(self, text: str, heading: str) -> list[int]:
        lines = text.splitlines()
        starts = [i for i, line in enumerate(lines) if line == heading]
        self.assertEqual(len(starts), 1, heading)
        start = starts[0]
        end = len(lines)
        for index in range(start + 1, len(lines)):
            if lines[index].startswith("## "):
                end = index
                break
        return [start + 1, end]

    def _write_dispositions(
        self,
        path: Path,
        baseline: str,
        candidate: str,
        items: list[dict],
        *,
        status: str,
        candidate_start: int = 1,
        candidate_end: int | None = None,
    ) -> None:
        dispositions = []
        for item in items:
            source_text = item.get("text", "")
            self.assertTrue(source_text.strip(), f"inventory item has no text: {item!r}")
            span = self._line_span(candidate, source_text, candidate_start, candidate_end)
            dispositions.append(
                {
                    "id": item["id"],
                    "status": status,
                    "candidate_span": span,
                    "rationale": "The normative block was retained at its new location.",
                }
            )
        payload = {
            "baseline_sha256": self._sha256(baseline),
            "candidate_sha256": self._sha256(candidate),
            "items": dispositions,
        }
        path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")

    def test_historical_inventory_discovers_changed_content(self) -> None:
        """The pinned rewrite must yield a nonempty structured inventory."""
        before = self._text(BEFORE_FIXTURE)
        after = self._text(AFTER_FIXTURE)
        with tempfile.TemporaryDirectory() as temporary:
            out = Path(temporary) / "inventory.json"
            inventory = self._inventory(BEFORE_FIXTURE, AFTER_FIXTURE, out)
        self.assertEqual(inventory["baseline_sha256"], self._sha256(before))
        self.assertEqual(inventory["candidate_sha256"], self._sha256(after))
        self.assertIsInstance(inventory["items"], list)
        self.assertGreater(len(inventory["items"]), 1)
        for item in inventory["items"]:
            self.assertTrue({"id", "kind", "source_span", "sha256", "text"} <= item.keys())
            self.assertEqual(len(item["source_span"]), 2)
            self.assertLessEqual(item["source_span"][0], item["source_span"][1])
            self.assertRegex(item["sha256"], r"^[0-9a-f]{64}$")

    def test_historical_rewrite_without_dispositions_fails(self) -> None:
        result = self._check(BEFORE_FIXTURE, AFTER_FIXTURE)
        self.assertNotEqual(result.returncode, 0)

    def test_mapping_every_old_item_to_surviving_section_fails(self) -> None:
        """A surviving heading cannot falsely account for deleted schemas/rules."""
        before = self._text(BEFORE_FIXTURE)
        after = self._text(AFTER_FIXTURE)
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            inventory_path = directory / "inventory.json"
            inventory = self._inventory(BEFORE_FIXTURE, AFTER_FIXTURE, inventory_path)
            section_span = self._section_span(after, "## 10. Final ParityMap and Terminal Inventory")
            dispositions = directory / "bad-dispositions.json"
            items = [
                {
                    "id": item["id"],
                    "status": "retained",
                    "candidate_span": section_span,
                    "rationale": "Incorrectly treated the surviving section as complete.",
                }
                for item in inventory["items"]
            ]
            dispositions.write_text(
                json.dumps(
                    {
                        "baseline_sha256": self._sha256(before),
                        "candidate_sha256": self._sha256(after),
                        "items": items,
                    },
                    indent=2,
                )
                + "\n",
                encoding="utf-8",
            )
            result = self._check(BEFORE_FIXTURE, AFTER_FIXTURE, dispositions=dispositions)
        self.assertNotEqual(result.returncode, 0)

    def test_schema_and_references_deleted_together_still_fail(self) -> None:
        """Removing a schema and its links must fail without a dangling reference."""
        before_text = (
            "# Public format\n\n"
            "## 10. Contract\n\n"
            "### 10.1. Payload schema\n\n"
            "| Key | Type |\n| --- | --- |\n| 1 | bytes |\n\n"
            "A reader MUST reject an invalid payload.\n"
        )
        after_text = "# Public format\n\n## 10. Contract\n\nThe payload details are omitted.\n"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            before = self._write_text_fixture(directory, "before.md", before_text)
            after = self._write_text_fixture(directory, "after.md", after_text)
            result = self._check(before, after)
        self.assertNotEqual(result.returncode, 0)

    def test_noop_revision_succeeds(self) -> None:
        result = self._check(BEFORE_FIXTURE, BEFORE_FIXTURE)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_complete_section_move_with_exact_destinations_succeeds(self) -> None:
        before_text = (
            "# Public format\n\n"
            "## 1. Introduction\n\nThe format is public.\n\n"
            "## 2. Contract\n\n"
            "### 2.1. Payload schema\n\n"
            "| Key | Type |\n| --- | --- |\n| 1 | bytes |\n\n"
            "### 2.2. Validation\n\nA reader MUST validate the payload.\n\n"
            "## 3. Closing notes\n\nThe document ends here.\n"
        )
        after_text = (
            "# Public format\n\n"
            "## 1. Introduction\n\nThe format is public.\n\n"
            "## 3. Closing notes\n\nThe document ends here.\n\n"
            "## 2. Contract\n\n"
            "### 2.1. Payload schema\n\n"
            "| Key | Type |\n| --- | --- |\n| 1 | bytes |\n\n"
            "### 2.2. Validation\n\nA reader MUST validate the payload.\n"
        )
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            before = self._write_text_fixture(directory, "before.md", before_text)
            after = self._write_text_fixture(directory, "after.md", after_text)
            inventory_path = directory / "inventory.json"
            inventory = self._inventory(before, after, inventory_path)
            dispositions = directory / "dispositions.json"
            self._write_dispositions(
                dispositions,
                before_text,
                after_text,
                inventory["items"],
                status="moved",
            )
            result = self._check(before, after, dispositions=dispositions)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
