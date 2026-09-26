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
            "in-progress/rem-parity-1-specification.md": (25, 59, 94),
            "in-progress/rem-object-core-1-specification.md": (21, 52, 120),
            "in-progress/rem-encrypt-1-specification.md": (0, 40, 37),
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
            scratch.write_text(docs[name].replace("directory (Section 10.1.5)", "directory (Section 10.9)"))
            mutated = {**docs, name: scratch.read_text()}
            defects = lint.deep_unresolved(name, mutated)
            self.assertTrue(any(key[2] == "10.9" for key in defects))
            self.assertTrue(lint.reconcile_known(defects, lint.KNOWN_REFERENCES))


class ScopeStatementTests(unittest.TestCase):
    BODY = ("This document defines a format.\n\n1. It would produce bytes that\n"
            "   another reader cannot read.\n\nSee the Guide.\n")

    def document(self, version, number="1.6", body=None, extra=""):
        body = self.BODY if body is None else body
        return (f"| Version | {version} |\n\n## 1. Introduction\n\n"
                f"### {number}. {lint.SCOPE_HEADING}\n\n{body}\n{extra}"
                "## 2. Conventions\n\nText.\n")

    def corpus(self, **overrides):
        docs = {}
        for name in lint.SPECS:
            docs["in-progress/" + name] = self.document("1.0.0-draft.9")
        exempt = {path: version for path, version in lint.SCOPE_BLOCK_EXEMPT}
        for path, version in exempt.items():
            docs[path] = f"| Version | {version} |\n\n## 1. Introduction\n\nText.\n"
        docs.update(overrides)
        return docs

    def test_identical_blocks_pass_across_numbers_and_wrapping(self):
        rewrapped = self.BODY.replace("bytes that\n   another", "bytes that another")
        docs = self.corpus(**{
            "in-progress/rem-parity-1-specification.md":
                self.document("1.0.0-draft.9", number="1.5", body=rewrapped)})
        self.assertEqual(lint.scope_block_findings(docs), [])

    def test_missing_block_in_a_preparing_copy_fails(self):
        name = "in-progress/rem-object-core-1-specification.md"
        docs = self.corpus(**{name: "| Version | 1.0.0-draft.9 |\n\n## 1. Introduction\n"})
        self.assertTrue(any(name in f and "missing" in f
                            for f in lint.scope_block_findings(docs)))

    def test_two_blocks_in_one_document_fail(self):
        name = "in-progress/rem-encrypt-1-specification.md"
        twice = self.document("1.0.0-draft.9",
                              extra=f"### 1.7. {lint.SCOPE_HEADING}\n\n{self.BODY}\n")
        docs = self.corpus(**{name: twice})
        self.assertTrue(any(name in f and "exactly one" in f
                            for f in lint.scope_block_findings(docs)))

    def test_one_character_difference_fails(self):
        name = "in-progress/rem-parity-1-specification.md"
        changed = self.document("1.0.0-draft.9", body=self.BODY.replace("format.", "format;"))
        docs = self.corpus(**{name: changed})
        self.assertTrue(any("differs" in f for f in lint.scope_block_findings(docs)))

    def test_exempt_published_versions_pass_and_new_published_versions_fail(self):
        self.assertEqual(lint.scope_block_findings(self.corpus()), [])
        name = "publication/rem-object-core-1-specification.md"
        docs = self.corpus(**{name: "| Version | 1.0.0-draft.4 |\n\n## 1. Introduction\n"})
        self.assertTrue(any(name in f and "missing" in f
                            for f in lint.scope_block_findings(docs)))

    def test_block_ends_at_a_heading_of_the_same_or_higher_level(self):
        text = self.document("1.0.0-draft.9", extra="#### A deeper heading\n\nKept.\n")
        block = lint.scope_blocks(text)[0]
        self.assertIn("A deeper heading", block)
        self.assertNotIn("Conventions", block)

    def test_fenced_heading_like_lines_neither_end_nor_hide_the_block(self):
        fenced = self.BODY + "\n```text\n## 2. Looks like a heading\n```\n\nAfter the fence.\n"
        changed = fenced.replace("After the fence.", "After the fence!")
        docs = self.corpus(**{
            "in-progress/rem-object-core-1-specification.md": self.document("1.0.0-draft.9", body=fenced),
            "in-progress/rem-encrypt-1-specification.md": self.document("1.0.0-draft.9", body=fenced),
            "in-progress/rem-parity-1-specification.md": self.document("1.0.0-draft.9", body=changed)})
        self.assertTrue(any("differs" in f for f in lint.scope_block_findings(docs)))
        self.assertIn("After the fence.", lint.scope_blocks(self.document("x", body=fenced))[0])

    def test_the_statement_must_be_a_subsection_of_section_one(self):
        name = "in-progress/rem-object-core-1-specification.md"
        moved = self.document("1.0.0-draft.9", number="2.4")
        docs = self.corpus(**{name: moved})
        self.assertTrue(any(name in f and "missing" in f
                            for f in lint.scope_block_findings(docs)))

    def test_the_statement_must_be_a_level_three_subsection(self):
        name = "in-progress/rem-parity-1-specification.md"
        promoted = self.document("1.0.0-draft.9").replace(
            f"### 1.6. {lint.SCOPE_HEADING}", f"## 1.6. {lint.SCOPE_HEADING}")
        docs = self.corpus(**{name: promoted})
        self.assertTrue(any(name in f and "missing" in f
                            for f in lint.scope_block_findings(docs)))

    def test_repository_documents_carry_one_identical_statement(self):
        docs = lint.discovered_documents()
        self.assertEqual(lint.scope_block_findings(docs), [])
        blocks = {name: lint.scope_blocks(text) for name, text in docs.items()
                  if name.startswith("in-progress/") and pathlib.Path(name).name in lint.SPECS}
        self.assertEqual(len(blocks), 3)
        self.assertTrue(all(len(found) == 1 for found in blocks.values()))


class PreparingCopyRuleTests(unittest.TestCase):
    CURRENT = "a" * 64
    NAME = "rem-parity-1-specification.md"

    def copy(self, **changes):
        text = ("| Document version | 1.0 |\n| Version | 1.0.0-draft.9 |\n\n"
                f"The archive `remanence-test-vectors.tar`, SHA-256\n`{self.CURRENT}`.\n")
        for old, new in changes.items():
            text = text.replace(old.replace("_", " "), new)
        return text

    def findings(self, text):
        return lint.preparing_copy_rule_findings(self.NAME, text, self.CURRENT)

    def test_a_conforming_copy_passes(self):
        self.assertEqual(self.findings(self.copy()), [])

    def test_document_version_row_missing_or_wrong(self):
        self.assertTrue(self.findings(self.copy().replace("| Document version | 1.0 |\n", "")))
        self.assertTrue(self.findings(self.copy().replace("| Document version | 1.0 |", "| Document version | 2.0 |")))

    def test_current_pin_missing_or_contradicted(self):
        stale = "b" * 64
        self.assertTrue(self.findings(self.copy().replace(self.CURRENT, stale)))
        both = self.copy() + f"\nAn older `remanence-test-vectors.tar`, SHA-256 `{stale}`.\n"
        self.assertTrue(any("quotes archive digest" in f for f in self.findings(both)))
        one_sentence = self.copy().replace(f"`{self.CURRENT}`.", f"`{self.CURRENT}`, formerly `{stale}`.")
        self.assertTrue(any("quotes archive digest" in f for f in self.findings(one_sentence)))

    def test_retired_doi_row_and_banned_title(self):
        self.assertTrue(self.findings(self.copy() + "| Version DOI (this release) | x |\n"))
        self.assertTrue(self.findings(self.copy() + lint.BANNED_TITLE_FORMS[0]))

    def test_an_unrelated_document_digest_is_not_an_archive_pin(self):
        text = self.copy() + "\nThe published revision (SHA-256 `" + "c" * 64 + "`) governs.\n"
        self.assertEqual(self.findings(text), [])


if __name__ == "__main__":
    unittest.main()
