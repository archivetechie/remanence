#!/usr/bin/env python3
"""Tests for the spec-only REM-PARITY publication-vector re-derivation."""

from __future__ import annotations

import copy
import pathlib
import tarfile
import tempfile
import unittest

from rem_parity_rederive import (
    RederivationError,
    UINT32_MAX,
    _parse_digest,
    _parse_directory,
    _parse_bootstrap_object_rows,
    _parse_parity_map_reference,
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

    def test_u32_wire_fields_reject_the_next_integer(self) -> None:
        """The spec-only reader enforces the same current widths as Rust."""
        over = UINT32_MAX + 1
        digest = {1: bytes(32), 2: 1, 3: 0, 4: 0, 5: False}
        directory = {
            1: 1,
            2: 1,
            3: 1,
            4: False,
            5: [
                {
                    1: 1,
                    2: 0,
                    3: 0,
                    4: 1,
                    5: 3,
                    6: 1,
                    7: 1,
                    8: bytes(32),
                    9: 0,
                }
            ],
        }
        reference = {
            1: 1,
            2: 3,
            3: 1,
            4: 1,
            5: 1,
            6: False,
            7: bytes(32),
            8: bytes(32),
        }

        _parse_digest(digest, "width-boundary")
        _parse_directory(directory, "width-boundary")
        _parse_parity_map_reference(reference, "width-boundary")

        u32_cases = [
            (_parse_digest, digest, (2,)),
            (_parse_directory, directory, (1,)),
            *[
                (_parse_directory, directory, (5, 0, key))
                for key in (1, 6, 7, 9)
            ],
            *[
                (_parse_parity_map_reference, reference, (key,))
                for key in (1, 3)
            ],
        ]
        for parser, value, path in u32_cases:
            for invalid in (over, True):
                damaged = copy.deepcopy(value)
                target = damaged
                for component in path[:-1]:
                    target = target[component]
                target[path[-1]] = invalid
                with self.subTest(parser=parser.__name__, path=path, invalid=invalid):
                    with self.assertRaises(RederivationError):
                        parser(damaged, "width-boundary")

        u64_cases = [
            *[(_parse_digest, digest, (key,)) for key in (3, 4)],
            *[(_parse_directory, directory, (key,)) for key in (2, 3)],
            *[
                (_parse_directory, directory, (5, 0, key))
                for key in (2, 3, 4, 5)
            ],
            *[
                (_parse_parity_map_reference, reference, (key,))
                for key in (2, 4, 5)
            ],
        ]
        for parser, value, path in u64_cases:
            damaged = copy.deepcopy(value)
            target = damaged
            for component in path[:-1]:
                target = target[component]
            target[path[-1]] = 1 << 64
            with self.subTest(parser=parser.__name__, path=path):
                with self.assertRaises(RederivationError):
                    parser(damaged, "width-boundary")

        for parser, value, key in (
            (_parse_digest, digest, 5),
            (_parse_directory, directory, 4),
            (_parse_parity_map_reference, reference, 6),
        ):
            damaged = copy.deepcopy(value)
            damaged[key] = 0
            with self.subTest(parser=parser.__name__, boolean_key=key):
                with self.assertRaises(RederivationError):
                    parser(damaged, "width-boundary")

    def test_object_row_wire_widths_match_rust(self) -> None:
        """Legacy key-30 rows reject over-wide and boolean integers."""
        plaintext = {
            1: 1,
            2: "plaintext",
            3: 2,
            10: 0,
            11: 1,
            12: 1,
            13: bytes(32),
        }
        encrypted = {
            1: 1,
            2: "encrypted",
            3: 2,
            21: 66,
            22: [bytes.fromhex("01" * 16)],
            23: 1191,
        }

        _parse_bootstrap_object_rows([plaintext, encrypted], "width-boundary")
        for row, key in ((plaintext, 1), (encrypted, 23)):
            for invalid in (UINT32_MAX + 1, True):
                damaged = dict(row)
                damaged[key] = invalid
                with self.subTest(representation=row[2], key=key, invalid=invalid):
                    with self.assertRaises(RederivationError):
                        _parse_bootstrap_object_rows([damaged], "width-boundary")

        for key in (3, 10, 11, 12):
            damaged = dict(plaintext)
            damaged[key] = (1 << 64) if key != 3 else True
            with self.subTest(representation="plaintext", key=key):
                with self.assertRaises(RederivationError):
                    _parse_bootstrap_object_rows([damaged], "width-boundary")

        damaged = dict(encrypted)
        damaged[21] = 1 << 64
        with self.assertRaises(RederivationError):
            _parse_bootstrap_object_rows([damaged], "width-boundary")


if __name__ == "__main__":
    unittest.main()
