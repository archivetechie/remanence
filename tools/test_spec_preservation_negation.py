"""Regression tests for normative-keyword preservation in spec_preservation."""
from __future__ import annotations

import unittest

import spec_preservation as p


class NormativePreservationTests(unittest.TestCase):
    @staticmethod
    def evidence(baseline: str, candidate: str, *, exceptions=(), edit_kind="substantive"):
        expected = p.inventory(baseline, candidate)
        rows = [
            {
                "id": item["id"],
                "status": "changed",
                "candidate_span": [1, len(candidate.splitlines())],
                "rationale": "Independent review assessed this substantive change.",
                "exceptions": list(exceptions),
            }
            for item in expected["items"]
        ]
        dispositions = {
            "baseline_sha256": expected["baseline_sha256"],
            "candidate_sha256": expected["candidate_sha256"],
            "items": rows,
        }
        receipt = {
            "baseline_sha256": expected["baseline_sha256"],
            "candidate_sha256": expected["candidate_sha256"],
            "inventory_sha256": p.canonical_hash(expected),
            "dispositions_sha256": p.canonical_hash(dispositions),
            "verdict": "passed",
            "reviewer": "independent-reviewer",
            "author": "spec-author",
            "reviewed_ids": [row["id"] for row in rows],
        }
        return p.check(
            baseline,
            candidate,
            dispositions,
            receipt,
            edit_kind=edit_kind,
        )

    def test_dropping_normative_negation_or_optional_is_rejected(self):
        pairs = (
            ("MUST NOT", "MUST"),
            ("SHALL NOT", "SHALL"),
            ("SHOULD NOT", "SHOULD"),
            ("NOT RECOMMENDED", "RECOMMENDED"),
            ("OPTIONAL", "REQUIRED"),
        )
        for old, new in pairs:
            with self.subTest(old=old):
                errors = self.evidence(
                    f"The service {old} expose data.\n",
                    f"The service {new} expose data.\n",
                )
                self.assertTrue(any("normative-keywords" in error for error in errors), errors)

    def test_substantive_normative_exception_is_explicit_and_effective(self):
        errors = self.evidence(
            "The service MUST NOT expose data.\n",
            "The service MUST expose data.\n",
            exceptions=("normative-keywords",),
            edit_kind="substantive",
        )
        self.assertEqual(errors, [])

    def test_wording_edit_cannot_waive_normative_exception(self):
        errors = self.evidence(
            "The service MUST NOT expose data.\n",
            "The service MUST expose data.\n",
            exceptions=("normative-keywords",),
            edit_kind="wording",
        )
        self.assertTrue(any("wording edits cannot waive" in error for error in errors), errors)

    def test_exception_requires_explicit_substantive_classification(self):
        errors = self.evidence(
            "The service MUST NOT expose data.\n",
            "The service MUST expose data.\n",
            exceptions=("normative-keywords",),
            edit_kind=None,
        )
        self.assertTrue(any("substantive" in error for error in errors), errors)

    def test_line_wrapping_preserves_negated_phrase_after_normalization(self):
        errors = self.evidence(
            "The service MUST NOT expose data.\n",
            "The service MUST\nNOT expose data.\n",
        )
        self.assertEqual(errors, [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
