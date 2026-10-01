"""Exercise the Guide checker CLI on isolated copies of the repository documents."""

from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

from tools.check_guide import headings, sections, strip_fences

ROOT = Path(__file__).resolve().parents[1]
CHECKER = ROOT / "tools/check_guide.py"


class GuideHelperTests(unittest.TestCase):
    def test_closing_fence_with_trailing_text(self):
        for fence in ("```", "~~~~"):
            with self.subTest(fence=fence):
                text = f"before\n{fence}text\nhidden\n  {fence} trailing text\nafter\n"
                self.assertEqual(strip_fences(text), "before\n\n\n\nafter\n")

    def test_sections_include_placeholder_headings(self):
        text = "## 3. Shared (in REM-OBJECT)\nQuoted text.\n## 4. Next\nOther text.\n"
        self.assertEqual(sections(text), {
            "3": "## 3. Shared (in REM-OBJECT) Quoted text.",
            "4": "## 4. Next Other text.",
        })
        self.assertEqual(headings(text), {"4": "Next"})


class GuideCheckerTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        shutil.copytree(ROOT / "docs", self.root / "docs")
        shutil.copytree(ROOT / "specs/in-progress", self.root / "specs/in-progress")
        shutil.copytree(ROOT / "specs/publication", self.root / "specs/publication")

    def run_checker(self, success):
        result = subprocess.run(
            [sys.executable, str(CHECKER), "--root", str(self.root)],
            cwd=self.root, capture_output=True, text=True, check=False,
        )
        if success:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        return result.stdout + result.stderr

    def replace(self, path, old, new):
        target = self.root / path
        text = target.read_text(encoding="utf-8")
        self.assertIn(old, text, "Mutation must change its subject")
        target.write_text(text.replace(old, new, 1), encoding="utf-8")

    def add_document(self, text):
        target = self.root / "docs/nested/citation.md"
        target.parent.mkdir()
        target.write_text(text, encoding="utf-8")

    def test_repository_passes_with_original_summaries(self):
        output = self.run_checker(True)
        self.assertIn("keywords outside quoted spans: none", output)
        for summary in ("attributed quotations in their named section",
                        "title citations checked", "section citations checked"):
            self.assertRegex(output, summary + r": [1-9][0-9]*; problems: 0")

    def test_corrupted_quotation_fails(self):
        self.replace("specs/publication/rem-implementation-guide.md",
                     "Decoders MUST enforce both limits.",
                     "Decoders MUST enforce neither limit.")
        self.assertIn("NOT FOUND", self.run_checker(False))

    def test_renamed_chapter_title_fails(self):
        self.replace("specs/in-progress/rem-object-core-1-specification.md",
                     "### 12.10. Path Traversal", "### 12.10. Renamed Chapter")
        self.assertIn("TITLE", self.run_checker(False))

    def test_nonexistent_section_in_nested_document_fails(self):
        self.add_document("See REM-PARITY §99.\n")
        self.assertIn("NO-HEADING docs/nested/citation.md: REM-PARITY §99",
                      self.run_checker(False))

    def test_keyword_outside_quotation_fails(self):
        target = self.root / "specs/publication/rem-implementation-guide.md"
        with target.open("a", encoding="utf-8") as stream:
            stream.write("\nReaders MUST do this.\n")
        self.assertIn("keywords outside quoted spans: ['MUST']", self.run_checker(False))

    def test_quotation_in_wrong_section_fails(self):
        self.replace("specs/publication/rem-implementation-guide.md",
                     '"Decoders MUST enforce both limits." (REM-OBJECT §4.7.1)',
                     '"Decoders MUST enforce both limits." (REM-OBJECT §1)')
        self.assertIn("WRONG SECTION", self.run_checker(False))

    def test_appendix_citation_passes(self):
        self.add_document("See REM-PARITY\n§B.1.\n")
        self.run_checker(True)

    def test_other_specs_and_fenced_citations_are_ignored(self):
        baseline = self.run_checker(True)
        self.add_document(
            "See REM-OTHER §3.\n\n```text\nREM-PARITY §99, Bad Title\n```\n"
        )
        self.assertEqual(self.run_checker(True), baseline)

    def test_missing_guide_fails(self):
        (self.root / "specs/publication/rem-implementation-guide.md").unlink()
        self.assertIn("FileNotFoundError", self.run_checker(False))


if __name__ == "__main__":
    unittest.main()
