#!/usr/bin/env python3
"""Regression tests for the publication specification guard."""

import unittest

from check_spec_versioning import unresolved_section_references


class SectionReferenceTests(unittest.TestCase):
    def test_singular_reference(self) -> None:
        text = "## 1. Present\nSee Section 99."
        self.assertEqual(unresolved_section_references(text), {99})

    def test_plural_and_nested_references(self) -> None:
        text = "## 4. Present\nSee Sections 4.7.5, 9, and 10."
        self.assertEqual(unresolved_section_references(text), {9, 10})

    def test_ranges_include_unwritten_intermediate_sections(self) -> None:
        text = "## 1. Present\n## 4. Present\nSee Sections 1 through 4."
        self.assertEqual(unresolved_section_references(text), {2, 3})

    def test_omission_does_not_exempt_later_reference(self) -> None:
        text = "## 1. Present\nThis omits Sections 9 and 10. See Section 9."
        self.assertEqual(unresolved_section_references(text), {9})

    def test_revision_history_is_not_active_prose(self) -> None:
        for heading in [
            "## Appendix A. Revision History (Informative)",
            "## Revision History (Informative)",
        ]:
            with self.subTest(heading=heading):
                text = f"## 1. Present\n{heading}\n- Removed Section 9."
                self.assertEqual(unresolved_section_references(text), set())


import collections
import pathlib
import tempfile
import check_spec_versioning as lint


class StructuralRulesTests(unittest.TestCase):
    def test_attribution_and_depth_branches(self):
        target = "## 4. Present\n\n### 4.2. Nested\n\n#### 4.2.1. Deep\n"
        branches = ["Section {n}", "§{n}", "Core §{n}", "Core\n§{n}",
                    "[REMOBJECT] §{n}", "[REMOBJECT], Section {n}",
                    "REM-OBJECT Section {n}", "REM-ENCRYPT Section {n}",
                    "REM-PARITY Section {n}", "Section {n} of [REMOBJECT]",
                    "Section {n} of [REMENCRYPT]", "Section {n} of [REMPARITY]"]
        for branch in branches:
            for number, fails in (("4.2.1", False), ("4.2.9", True)):
                with self.subTest(branch=branch, number=number):
                    docs = {"publication/local.md": target + branch.format(n=number)}
                    docs.update({"publication/" + name: target for name in lint.SPECS})
                    self.assertEqual(bool(lint.deep_unresolved("publication/local.md", docs)), fails)
        for reference in ("[RFC8949] Section 999.2", "RFC 8949 §999.2", "Section 999.2 of [RFC8949]", "§999.2 of RFC 8949", "[ISO/IEC] Section 999.2", "[NIST SP 800] Section 999.2", "[foo.bar] — Section 999.2"):
            with self.subTest(external=reference):
                self.assertFalse(lint.deep_unresolved("local", {"local": reference}))
                self.assertTrue(lint.deep_unresolved("local", {"local": "See §999.2"}))

    def test_companion_preparing_first_and_missing_companion(self):
        docs = {"local": "Core §9.8", "publication/rem-object-core-1-specification.md": "## 9.8. Present"}
        self.assertFalse(lint.deep_unresolved("local", docs))
        docs["in-progress/rem-object-core-1-specification.md"] = "## 9.7. Changed"
        self.assertTrue(lint.deep_unresolved("local", docs))
        self.assertTrue(lint.deep_unresolved("local", {"local": "Core §9.8"}))

    def test_ranges_lists_fences_omissions_and_history(self):
        base = "## 9.4. Start\n\n## 9.6. End\n\n"
        for suffix in ("Sections 9.4–9.6", "Sections 9.4/9.5/9.6", "§§9.4, 9.5 or 9.6", "```\nSection 9.5\n```", "omits Section 9.5. See Section 9.5"):
            with self.subTest(suffix=suffix):
                self.assertTrue(lint.deep_unresolved("local", {"local": base + suffix}))
                self.assertFalse(lint.deep_unresolved("local", {"local": base + "## 9.5. Middle\n\n" + suffix}))
        for suffix in ("omits Section 9.5", "## Appendix C. Revision History\n\nRemoved Section 9.5"):
            self.assertFalse(lint.deep_unresolved("local", {"local": base + suffix}))
        self.assertFalse(lint.deep_unresolved("local", {"local": base + "(Section 9.4's text)"}))

    def test_github_external_oracle(self):
        path = pathlib.Path(__file__).parent / "testdata/github-heading-anchors.tsv"
        rows = [row.split("\t") for row in path.read_text().splitlines() if row and not row.startswith("#")]
        self.assertEqual(len(rows), 441)
        by_file = collections.defaultdict(list)
        for name, heading, anchor in rows:
            by_file[name].append((heading, anchor))
        for name, values in by_file.items():
            with self.subTest(name=name):
                self.assertEqual(lint.heading_slugs("\n\n".join("## " + h for h, _ in values)), [a for _, a in values])
        self.assertEqual(lint.heading_slugs("# Echo\n\n# Echo\n\n# Echo-1"), ["echo", "echo-1", "echo-1-1"])
        self.assertEqual(lint.heading_slugs("# Θ GF(2⁸) `field_name` — & Author's Address"), ["θ-gf2-field_name---authors-address"])

    def test_headings_and_appendices_views(self):
        self.assertEqual(lint.heading_spacing("# First\n \t\n## Good\n~~~\n# fenced\n~~~\n\n## End"), [])
        self.assertEqual(lint.heading_spacing("paragraph\n# Bad"), [2])
        base = "## Appendix B. Rules\n\n### B.12. Nested\n\n"
        self.assertFalse(lint.unresolved_appendices(base + "Appendix B.12\n\n## Appendix C. Revision History\n\nRemoved Appendix E"))
        self.assertEqual(lint.unresolved_appendices(base + "Appendix B.99"), {"B.99"})
        self.assertFalse(lint.unresolved_anchors("x", "[history](#revision-history)\n\n## Revision History\n\n```\n[not a link](#missing)\n```"))

    def test_known_lists_are_exact_occurrence_multisets(self):
        text = "## 1. Present\n\nSee Section 9.\n"
        defects = lint.deep_unresolved("local", {"local": text})
        self.assertFalse(lint.reconcile_known(defects, defects))
        for replacement in (text + "\n## 9. Resolved", "## 1. Present\n", text + "\nSee Section 9.\n"):
            self.assertTrue(lint.reconcile_known(lint.deep_unresolved("local", {"local": replacement}), defects))
        anchor = "[gone](#gone)\n"
        known = lint.unresolved_anchors("x", anchor)
        self.assertFalse(lint.reconcile_known(known, known))
        for replacement in (anchor + "\n# Gone", "", anchor + anchor):
            self.assertTrue(lint.reconcile_known(lint.unresolved_anchors("x", replacement), known))

    def test_readme_bijection(self):
        table = "## Current state\n\n| Document | Preparing | Published |\n| --- | --- | --- |\n"
        row = "| REM-PARITY | 1.0.0-draft.5 (preparing) | 1.0.0-draft.2 |\n"
        docs = {"in-progress/rem-parity-1-specification.md": "| Version | 1.0.0-draft.5 |",
                "publication/rem-parity-1-specification.md": "| Version | 1.0.0-draft.2 |"}
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            path = root / "specs/in-progress/README.md"
            path.parent.mkdir(parents=True)
            path.write_text(table + row)
            self.assertFalse(lint.readme_state_findings(root, docs))
            for bad in (table, table + row + row, table + row.replace("REM-PARITY", "UNKNOWN"), table + row.replace("draft.2", "draft.1"), table + row.replace("draft.2 |", "draft.2 WRONG |")):
                path.write_text(bad)
                self.assertTrue(lint.readme_state_findings(root, docs))
            path.write_text(table + row)
            self.assertTrue(lint.readme_state_findings(root, {}))
            path.unlink()
            self.assertTrue(lint.readme_state_findings(root, docs))

    def test_corpus_floor_and_mutation(self):
        expected = {
            "publication/rem-object-core-1-specification.md": (21, 52, 120),
            "publication/rem-encrypt-1-specification.md": (0, 40, 37),
            "publication/formats-explained.md": (0, 7, 1),
            "in-progress/rem-parity-1-specification.md": (26, 59, 94),
            "publication/rem-parity-1-specification.md": (26, 61, 0),
        }
        docs = lint.discovered_documents()
        self.assertEqual(set(docs), set(expected))
        for name, (anchors, heads, references) in expected.items():
            with self.subTest(name=name):
                self.assertEqual(sum(lint.anchor_occurrences(name, docs[name]).values()), anchors)
                self.assertGreaterEqual(len(lint.headings(docs[name])), heads)
                self.assertGreaterEqual(len(lint.section_references(docs[name])), references)
        self.assertFalse(lint.structural_findings())
        name = "in-progress/rem-parity-1-specification.md"
        with tempfile.TemporaryDirectory() as temporary:
            scratch = pathlib.Path(temporary) / pathlib.Path(name).name
            scratch.write_text(docs[name].replace("directory (Section 10.5)", "directory (Section 10.9)"))
            mutated = {**docs, name: scratch.read_text()}
            defects = lint.deep_unresolved(name, mutated)
            self.assertTrue(any(key[2] == "10.9" for key in defects))
            self.assertTrue(lint.reconcile_known(defects, lint.KNOWN_REFERENCES))


if __name__ == "__main__":
    unittest.main()
