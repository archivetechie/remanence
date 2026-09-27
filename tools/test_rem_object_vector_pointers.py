#!/usr/bin/env python3
"""Keep the candidate erratum table equal to the read-only archive report."""

import unittest

from rem_object_vector_pointers import ROOT, pointer_rows, render_table


class PointerTableTests(unittest.TestCase):
    def test_owning_sections_exist(self):
        documents = {
            "REM-OBJECT": "rem-object-core-1-specification.md",
            "REM-ENCRYPT": "rem-encrypt-1-specification.md",
        }
        for _, _, _, owners in pointer_rows():
            for owner in owners.split("; "):
                document, section = owner.split()
                text = (ROOT / "specs/in-progress" / documents[document]).read_text()
                self.assertIn(f"### {section}. ", text, owner)

    def test_readme_table(self):
        readme = (ROOT / "fixtures/rem-object-supplement-draft/README.md").read_text()
        table = readme.split("<!-- vector-pointers:start -->\n", 1)[1].split(
            "<!-- vector-pointers:end -->", 1)[0]
        self.assertEqual(table, render_table())


if __name__ == "__main__":
    unittest.main()
