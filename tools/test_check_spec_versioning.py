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
import re
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
            "in-progress/rem-object-core-1-specification.md": (22, 52, 120),
            "in-progress/rem-encrypt-1-specification.md": (21, 40, 37),
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
        # A local citation of a section the companion holds meets only the placeholder.
        name = "in-progress/rem-object-core-1-specification.md"
        mutated = {**docs, name: docs[name].replace("REM-ENCRYPT §6.3 (ciphertext)", "Section 6.3 (ciphertext)")}
        self.assertNotEqual(mutated[name], docs[name])
        ordinary, messages = lint.reference_findings(name, mutated)
        self.assertFalse(ordinary)
        self.assertEqual(len(messages), 1)
        self.assertIn("placeholder '6.3. Ciphertext Mapping (in REM-ENCRYPT)'", messages[0])


class PlaceholderTests(unittest.TestCase):
    """Rule 10: placeholder headings in the paired REM-OBJECT/REM-ENCRYPT numbering."""
    OBJ_NAME = "in-progress/rem-object-core-1-specification.md"
    ENC_NAME = "in-progress/rem-encrypt-1-specification.md"
    OBJ = ("## 4. Plaintext Representation\n\nText.\n\n"
           "## 5. Encrypted Representation (in REM-ENCRYPT)\n\n"
           "## 6. Partial File Restore\n\n"
           "### 6.3. Ciphertext Mapping (in REM-ENCRYPT)\n\n"
           "### 6.4. Stored-Block Mapping\n\nText.\n")
    ENC = ("## 4. Plaintext Representation (in REM-OBJECT)\n\n"
           "## 5. Encrypted Representation\n\nText.\n\n"
           "## 6. Partial File Restore\n\nText.\n\n"
           "### 6.3. Ciphertext Mapping\n\nText.\n\n"
           "### 6.4. Stored-Block Mapping\n\nText.\n")

    def docs(self, obj=None, enc=None, obj_extra="", enc_extra=""):
        return {self.OBJ_NAME: (self.OBJ if obj is None else obj) + obj_extra,
                self.ENC_NAME: (self.ENC if enc is None else enc) + enc_extra}

    def test_placeholder_is_not_a_section_number(self):
        self.assertEqual(lint.section_numbers(self.OBJ), {"4", "6", "6.4"})
        self.assertEqual([p.number for p in lint.placeholder_headings(self.OBJ)], ["5", "6.3"])
        # Only the exact suffix naming one of the paired documents makes a placeholder.
        ordinary = "### 6.2. Inner Mapping (Both Representations)\n\n### 4.1. Frames (in REM-PARITY)\n"
        self.assertEqual(lint.section_numbers(ordinary), {"6.2", "4.1"})
        self.assertEqual(lint.placeholder_headings(ordinary), [])

    def test_placeholder_without_the_dot_is_a_placeholder(self):
        # section_numbers accepts "6.3 Title" as well as "6.3. Title"; so does PLACEHOLDER.
        undotted = self.OBJ.replace("### 6.3. Ciphertext Mapping (in REM-ENCRYPT)",
                                    "### 6.3 Ciphertext Mapping (in REM-ENCRYPT)")
        self.assertNotIn("6.3", lint.section_numbers(undotted))
        self.assertEqual([p.number for p in lint.placeholder_headings(undotted)], ["5", "6.3"])
        ordinary, messages = lint.reference_findings(self.OBJ_NAME, self.docs(obj=undotted, obj_extra="\nSee Section 6.3.\n"))
        self.assertFalse(ordinary)
        self.assertEqual(len(messages), 1)
        # Checked like any other: it must repeat the holder's heading exactly, dot included.
        findings = lint.placeholder_findings(self.OBJ_NAME, self.docs(obj=undotted))
        self.assertEqual(len(findings), 1)
        self.assertIn("does not match the heading '### 6.3. Ciphertext Mapping'", findings[0])

    def test_a_suffix_on_a_heading_that_is_not_a_placeholder_is_reported(self):
        for heading in ("### Notes (in REM-ENCRYPT)", "### 6.3. (in REM-ENCRYPT)"):
            with self.subTest(heading=heading):
                findings = lint.placeholder_findings(self.OBJ_NAME, self.docs(obj_extra=f"\n{heading}\n"))
                self.assertTrue(any(f"{heading!r} ends like a placeholder but is not a numbered placeholder heading" in f
                                    for f in findings))

    def test_rule_ten_reaches_the_linter_output(self):
        """structural_findings, which main() runs, carries every Rule 10 finding."""
        source = pathlib.Path(lint.__file__).resolve().parent.parent / "specs"
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            for folder in ("publication", "in-progress"):
                (root / "specs" / folder).mkdir(parents=True)
                for path in (source / folder).glob("*.md"):
                    (root / "specs" / folder / path.name).write_text(path.read_text())
            self.assertEqual(lint.structural_findings(root), [])
            plants = {
                self.OBJ_NAME: [
                    ("REM-ENCRYPT §6.3 (ciphertext)", "Section 6.3 (ciphertext)"),
                    ("### 12.4. Fail-Closed (in REM-ENCRYPT)", "### 12.4. Fail Closed (in REM-ENCRYPT)"),
                    ("\n### 6.4. Stored-Block Mapping (Tape",
                     "\n### 6.4. Stored-Block Mapping (in REM-ENCRYPT)\n\n### 6.4. Stored-Block Mapping (Tape"),
                ],
                self.ENC_NAME: [
                    ("## 14. Conformance (in REM-OBJECT)\n", "## 14. Conformance (in REM-OBJECT)\n\nStray text.\n"),
                    ("### 6.2. Inner Mapping (Both Representations) (in REM-OBJECT)\n",
                     "### 6.2. Inner Mapping (Both Representations) (in REM-OBJECT)\n\n#### 6.2.1. Detail\n"),
                    ("### 7.5. Scrub (in REM-OBJECT)\n", "### 7.5. Scrub (in REM-OBJECT)\n\n### 7.5. Scrub (in REM-OBJECT)\n"),
                    ("\n## 13. Envelope Test Vectors\n",
                     "\n### 12.13. Nothing (in REM-OBJECT)\n\n### Notes (in REM-OBJECT)\n\n## 13. Envelope Test Vectors\n"),
                ],
            }
            for name, edits in plants.items():
                path = root / "specs" / name
                text = path.read_text()
                for old, new in edits:
                    self.assertEqual(text.count(old), 1, old)
                    text = text.replace(old, new)
                path.write_text(text)
            findings = lint.structural_findings(root)
            for fragment in ("Section 6.3 resolves only to the placeholder",
                             "'### 12.4. Fail Closed (in REM-ENCRYPT)' does not match the heading",
                             "duplicates Section 6.4 of this document",
                             "'## 14. Conformance (in REM-OBJECT)' has content before the next heading",
                             "'### 6.2. Inner Mapping (Both Representations) (in REM-OBJECT)' has content before",
                             "'### 7.5. Scrub (in REM-OBJECT)' shares its number",
                             "has no Section 12.13",
                             "'### Notes (in REM-OBJECT)' ends like a placeholder"):
                with self.subTest(fragment=fragment):
                    self.assertTrue(any(fragment in finding for finding in findings), findings)

    def test_fenced_lines_are_not_placeholders(self):
        fenced = "\n```text\n### 6.7. Fenced (in REM-ENCRYPT)\n```\n\nSee Section 6.7.\n"
        docs = self.docs(obj_extra=fenced)
        self.assertEqual([p.number for p in lint.placeholder_headings(docs[self.OBJ_NAME])], ["5", "6.3"])
        self.assertEqual(lint.placeholder_findings(self.OBJ_NAME, docs), [])
        ordinary, messages = lint.reference_findings(self.OBJ_NAME, docs)
        self.assertTrue(any(key[2] == "6.7" for key in ordinary))
        self.assertEqual(messages, [])

    def test_reference_to_a_placeholder_is_reported_in_both_directions(self):
        reported = [
            (self.OBJ_NAME, "See Section 6.3.", "Section 6.3 resolves only", "the section is in REM-ENCRYPT"),
            (self.OBJ_NAME, "See REM-ENCRYPT §4.", "REM-ENCRYPT §4 resolves only", "cite it as Section 4"),
            (self.ENC_NAME, "See Section 4.", "Section 4 resolves only", "the section is in REM-OBJECT"),
            # The message quotes the citation as written, not the target's name.
            (self.ENC_NAME, "See Core §6.3.", "Core §6.3 resolves only", "cite it as Section 6.3"),
            (self.ENC_NAME, "See Section 6.3 of [REMOBJECT].", "Section 6.3 of [REMOBJECT] resolves only", "cite it as Section 6.3"),
            (self.ENC_NAME, "See Core §§6.3 and 6.4.", "Core §§6.3 and 6.4 cites 6.3, which resolves only", "cite it as Section 6.3"),
        ]
        for name, text, cited, advice in reported:
            with self.subTest(name=name, text=text):
                extra = "\n" + text + "\n"
                docs = self.docs(obj_extra=extra) if name == self.OBJ_NAME else self.docs(enc_extra=extra)
                ordinary, messages = lint.reference_findings(name, docs)
                # Never in the Counter reconciled against KNOWN_REFERENCES, so it cannot be waived.
                self.assertFalse(ordinary)
                self.assertEqual(len(messages), 1)
                self.assertIn(cited + " to the placeholder", messages[0])
                self.assertIn(advice, messages[0])
        clean = [(self.OBJ_NAME, "See REM-ENCRYPT §6.3."), (self.OBJ_NAME, "See Section 4."),
                 (self.ENC_NAME, "See Core §6.4."), (self.ENC_NAME, "See Section 6.3."),
                 (self.ENC_NAME, "See Core §4.")]
        for name, text in clean:
            with self.subTest(name=name, text=text):
                extra = "\n" + text + "\n"
                docs = self.docs(obj_extra=extra) if name == self.OBJ_NAME else self.docs(enc_extra=extra)
                self.assertEqual(lint.reference_findings(name, docs), (collections.Counter(), []))
        # A number neither document has stays an ordinary unresolved reference.
        ordinary, messages = lint.reference_findings(self.OBJ_NAME, self.docs(obj_extra="\nSee Section 6.9.\n"))
        self.assertTrue(any(key[2] == "6.9" for key in ordinary))
        self.assertEqual(messages, [])

    def test_placeholder_heading_must_match_its_holder(self):
        self.assertEqual(lint.placeholder_findings(self.OBJ_NAME, self.docs()), [])
        self.assertEqual(lint.placeholder_findings(self.ENC_NAME, self.docs()), [])
        placeholder = "### 6.3. Ciphertext Mapping (in REM-ENCRYPT)"
        for bad, expected in (("### 6.3. Old Title (in REM-ENCRYPT)", "does not match the heading"),
                              ("#### 6.3. Ciphertext Mapping (in REM-ENCRYPT)", "does not match the heading"),
                              ("### 6.9. Ciphertext Mapping (in REM-ENCRYPT)", "has no Section 6.9")):
            with self.subTest(bad=bad):
                findings = lint.placeholder_findings(self.OBJ_NAME, self.docs(obj=self.OBJ.replace(placeholder, bad)))
                self.assertEqual(len(findings), 1)
                self.assertIn(expected, findings[0])
        # A retitle in the holder leaves the placeholder stale.
        retitled = self.ENC.replace("### 6.3. Ciphertext Mapping\n", "### 6.3. Ciphertext Ranges\n")
        self.assertTrue(lint.placeholder_findings(self.OBJ_NAME, self.docs(enc=retitled)))
        self.assertIn("not present", lint.placeholder_findings(self.OBJ_NAME, {self.OBJ_NAME: self.OBJ})[0])
        # The holder is looked up in the placeholder's own folder first.
        stale = self.ENC.replace("### 6.3. Ciphertext Mapping\n", "### 6.3. Old Title\n")
        docs = {**self.docs(), "publication/rem-encrypt-1-specification.md": stale,
                "publication/rem-object-core-1-specification.md": self.OBJ}
        self.assertEqual(lint.placeholder_findings(self.OBJ_NAME, docs), [])
        self.assertTrue(lint.placeholder_findings("publication/rem-object-core-1-specification.md", docs))

    def test_placeholder_must_not_reuse_a_number_its_document_has(self):
        findings = lint.placeholder_findings(self.OBJ_NAME, self.docs(obj_extra="\n### 6.3. Ciphertext Mapping\n\nText.\n"))
        self.assertEqual(len(findings), 1)
        self.assertIn("duplicates Section 6.3 of this document", findings[0])

    def test_two_placeholders_with_one_number_are_each_checked(self):
        second = "\n### 6.3. Stale Title (in REM-ENCRYPT)\n\nStray text.\n"
        findings = lint.placeholder_findings(self.OBJ_NAME, self.docs(obj_extra=second))
        self.assertEqual(len(findings), 3)
        self.assertTrue(all("'### 6.3. Stale Title (in REM-ENCRYPT)'" in f for f in findings))
        self.assertEqual(sum("shares its number with the placeholder '### 6.3. Ciphertext Mapping (in REM-ENCRYPT)'" in f for f in findings), 1)
        self.assertEqual(sum("does not match the heading" in f for f in findings), 1)
        self.assertEqual(sum("a placeholder has no body" in f for f in findings), 1)
        self.assertEqual([p.number for p in lint.placeholder_headings(self.OBJ + second)], ["5", "6.3", "6.3"])

    def test_placeholder_heading_has_no_body(self):
        placeholder = "### 6.3. Ciphertext Mapping (in REM-ENCRYPT)\n"
        for body in ("\nStray text.\n", "\n```text\nfenced\n```\n", "stray line\n"):
            with self.subTest(body=body):
                findings = lint.placeholder_findings(
                    self.OBJ_NAME, self.docs(obj=self.OBJ.replace(placeholder, placeholder + body)))
                self.assertEqual(len(findings), 1)
                self.assertIn("a placeholder has no body", findings[0])
        blank = self.OBJ.replace(placeholder, placeholder + "\n  \n")
        self.assertEqual(lint.placeholder_findings(self.OBJ_NAME, self.docs(obj=blank)), [])

    def test_a_subordinate_heading_is_part_of_the_placeholder_body(self):
        placeholder = "### 6.3. Ciphertext Mapping (in REM-ENCRYPT)\n"
        nested = self.OBJ.replace(placeholder, placeholder + "\n#### 6.3.1. Detail\n\n##### 6.3.1.1. Deeper\n\n")
        findings = lint.placeholder_findings(self.OBJ_NAME, self.docs(obj=nested))
        self.assertEqual(len(findings), 1)
        self.assertIn("a placeholder has no body", findings[0])
        # A heading under a placeholder is not a section of the document that carries it;
        # the next heading at the placeholder's level ends the placeholder.
        self.assertEqual(lint.section_numbers(nested), {"4", "6", "6.4"})
        ordinary, messages = lint.reference_findings(self.OBJ_NAME, self.docs(obj=nested, obj_extra="\nSee Section 6.3.1.\n"))
        self.assertTrue(any(key[2] == "6.3.1" for key in ordinary))

    # The Section 1.1 numbering paragraphs, word for word (design v4, D5).
    NUMBERING = {
        OBJ_NAME: ("REM-OBJECT and REM-ENCRYPT are numbered together, so the two can be read side by side. "
                   "This document omits Section 5, the encrypted representation, which is in REM-ENCRYPT; "
                   "REM-ENCRYPT omits Sections 4, 9 and 14, the plaintext representation, the relationship "
                   "to the parity layer, and conformance, which are here. Each document marks a section it "
                   "omits with a placeholder heading that names the document holding it. Sections 6, 7, 8, "
                   "11 and 12 number their subsections together in the same way: a number used in both "
                   "documents names sections on the same subject, and a placeholder marks a subsection that "
                   "only the other document contains. Sections 1, 3, 10 and 13 number their subsections "
                   "separately, so the same number there can name unrelated sections; Sections 2 and 16 "
                   "match one for one. A reference into the other document therefore always names it, as "
                   "in “REM-ENCRYPT §6.4”, while “Section 6.4” alone means this document's section."),
        ENC_NAME: ("REM-ENCRYPT and the REM-OBJECT Core Format ([REMOBJECT], called Core in this document) "
                   "are numbered together, so the two can be read side by side. This document omits "
                   "Sections 4, 9 and 14, the plaintext representation, the relationship to the parity "
                   "layer, and conformance, which are in Core; Core omits Section 5, the encrypted "
                   "representation, which is here. Each document marks a section it omits with a "
                   "placeholder heading that names the document holding it. Sections 6, 7, 8, 11 and 12 "
                   "number their subsections together in the same way: a number used in both documents "
                   "names sections on the same subject, and a placeholder marks a subsection that only the "
                   "other document contains. Sections 1, 3, 10 and 13 number their subsections separately, "
                   "so the same number there can name unrelated sections; Sections 2 and 16 match one for "
                   "one. A reference into Core therefore always names it, as in “Core §6.4”, while "
                   "“Section 6.4” alone means this document's section."),
    }

    def skeleton(self, own, placeholders, holder, paragraph):
        """Section 1 holds the paragraph; the other top-level numbers are real
        headings or placeholders; Section 6 has a real 6.4."""
        parts = [f"## 1. Introduction\n\n{paragraph}\n"]
        for n in sorted((own | placeholders) - {1}):
            suffix = f" (in {holder})" if n in placeholders else ""
            parts.append(f"## {n}. Part {n}{suffix}\n")
            if n == 6:
                parts.append("### 6.4. Stored-Block Mapping\n")
        return "\n".join(parts)

    def test_numbering_paragraphs_pass(self):
        real = lint.discovered_documents()
        flat = lambda s: re.sub(r"\s+", " ", s).strip()
        for name, paragraph in self.NUMBERING.items():
            with self.subTest(name=name):
                self.assertIn(paragraph, flat(real[name]))
        shared = {1, 2, 3, 6, 7, 8, 10, 11, 12, 13, 16}
        def corpus(obj_paragraph, enc_paragraph):
            return {self.OBJ_NAME: self.skeleton(shared | {4, 9, 14, 15}, {5}, "REM-ENCRYPT", obj_paragraph),
                    self.ENC_NAME: self.skeleton(shared | {5, 15}, {4, 9, 14}, "REM-OBJECT", enc_paragraph)}
        docs = corpus(self.NUMBERING[self.OBJ_NAME], self.NUMBERING[self.ENC_NAME])
        for name in docs:
            with self.subTest(name=name):
                self.assertEqual(lint.reference_findings(name, docs), (collections.Counter(), []))
                self.assertEqual(lint.placeholder_findings(name, docs), [])
        # Negative controls: without the "omits" declarations the numbers meet placeholders.
        docs = corpus(self.NUMBERING[self.OBJ_NAME].replace("This document omits Section 5",
                                                            "This document leaves out Section 5"),
                      self.NUMBERING[self.ENC_NAME].replace("This document omits Sections 4, 9 and 14",
                                                            "This document leaves out Sections 4, 9 and 14"))
        self.assertEqual(len(lint.reference_findings(self.OBJ_NAME, docs)[1]), 1)
        self.assertEqual(len(lint.reference_findings(self.ENC_NAME, docs)[1]), 3)


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
