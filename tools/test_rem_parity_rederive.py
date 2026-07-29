#!/usr/bin/env python3
"""Tests for the spec-only REM-PARITY publication-vector re-derivation."""

from __future__ import annotations

import pathlib
import tarfile
import tempfile
import unittest

from rem_parity_rederive import (
    RederivationError,
    cauchy_matrix,
    crc64_xz,
    encode_parity,
    gf_inv,
    rederive_publication_vectors,
)


ROOT = pathlib.Path(__file__).resolve().parents[1]
ARCHIVE = ROOT / "specs" / "publication" / "remanence-test-vectors.tar"


class RemParityRederivationTests(unittest.TestCase):
    """Exercise normative primitives and the complete frozen archive."""

    def test_normative_arithmetic(self) -> None:
        """The independent primitives reproduce the inline specification values."""
        self.assertEqual(gf_inv(0x02), 0x8E)
        self.assertEqual(gf_inv(0x03), 0xF4)
        matrix = cauchy_matrix(2, 2)
        self.assertEqual(matrix, ((0x8E, 0xF4), (0xF4, 0x8E)))
        self.assertEqual(
            encode_parity(
                (bytes.fromhex("01020304"), bytes.fromhex("10203040")),
                matrix,
            ),
            (bytes.fromhex("75ea9fc9"), bytes.fromhex("fce519d7")),
        )
        self.assertEqual(crc64_xz(b"123456789"), 0x995DC9BBDF1939FA)

    def test_published_archive_rederives(self) -> None:
        """All parity, shard CRC, and digest pins match the published raw bytes."""
        with tempfile.TemporaryDirectory(
            prefix="remanence-parity-test-"
        ) as temporary_name:
            root = pathlib.Path(temporary_name)
            with tarfile.open(ARCHIVE, mode="r") as archive:
                archive.extractall(root, filter="data")
            summary = rederive_publication_vectors(root)
        self.assertEqual(summary.shards, (93, 93))
        self.assertEqual(summary.crcs, (153, 153))
        self.assertEqual(summary.digests, (64, 64))

    def test_parity_mutation_is_a_hard_failure(self) -> None:
        """A changed pinned parity byte raises instead of being accepted or skipped."""
        with tempfile.TemporaryDirectory(
            prefix="remanence-parity-mismatch-test-"
        ) as temporary_name:
            root = pathlib.Path(temporary_name)
            with tarfile.open(ARCHIVE, mode="r") as archive:
                archive.extractall(root, filter="data")
            sidecar = (
                root
                / "rem-parity-1"
                / "positive"
                / "minimal-image"
                / "tape-file-002-sidecar.bin"
            )
            data = bytearray(sidecar.read_bytes())
            data[4096] ^= 1
            sidecar.write_bytes(data)
            with self.assertRaises(RederivationError):
                rederive_publication_vectors(root)


if __name__ == "__main__":
    unittest.main()
