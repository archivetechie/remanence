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


if __name__ == "__main__":
    unittest.main()
