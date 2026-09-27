#!/usr/bin/env python3
"""Exercise metadata-profile enforcement without requiring the optional ML-KEM package."""

import tempfile
import unittest
from pathlib import Path

from verify_rem_object_vectors_independent import (
    MetadataProfileError, cbor, decode_cbor_exact, decode_metadata,
    validate_encrypted_metadata, verify_supplement_manifest, ROOT,
)


class MetadataProfileTests(unittest.TestCase):
    def metadata(self, extra: bytes) -> bytes:
        # Four required entries followed by the unknown unsigned key 4.
        return b"\xa5" + cbor({0: 1, 1: 512, 2: "sha256", 3: b"d" * 32})[1:] + b"\x04" + extra

    def test_unknown_unsigned_key_accepts_profile_values(self):
        for extra in [cbor(0), cbor([False, True, None, {"nested": [0, b"b", "text"]}]),
                      b"\xa2\x00\x00\xf4\x00",  # Distinct 0 and false keys.
                      b"\xa1\x81\x00\xa0"]:  # Array key and map value.
            with self.subTest(extra=extra):
                value = decode_metadata(self.metadata(extra))
                self.assertEqual(validate_encrypted_metadata("test", value, 512), (512, b"d" * 32))

    def test_forbidden_values_at_every_depth(self):
        for forbidden in [b"\x20", b"\xf9\x3c\x00", b"\xfa\x00\x00\x00\x14",
                          b"\xc0\x00", b"\x5f\xff", b"\xf7", b"\xf8\x20"]:
            for wrap in [lambda v: v, lambda v: b"\x81" + v,
                         lambda v: b"\xa1\x00" + v, lambda v: b"\xa1" + v + b"\x00"]:
                with self.subTest(forbidden=forbidden, wrap=wrap):
                    with self.assertRaises(MetadataProfileError):
                        decode_metadata(self.metadata(wrap(forbidden)))
        # Generic manifest CBOR still permits negative integers.
        self.assertEqual(decode_cbor_exact(b"\x20"), -1)

    def test_encoding_and_structural_constraints(self):
        for data in [b"\x80", b"\xa1\x61x\x00", b"\xa1\xf4\x00",
                     self.metadata(b"\x00") + b"\x00",
                     self.metadata(b"\x18\x00"), self.metadata(b"\x61\xff"),
                     self.metadata(b"\xa2\x01\x00\x00\x00"),
                     self.metadata(b"\xa2\x00\x00\x00\x00")]:
            with self.subTest(data=data):
                with self.assertRaises(MetadataProfileError):
                    decode_metadata(data)

    def test_depth_and_item_limits(self):
        decode_metadata(self.metadata(b"\x81" * 31 + b"\x00"))
        with self.assertRaises(MetadataProfileError):
            decode_metadata(self.metadata(b"\x81" * 32 + b"\x00"))
        # Root + 8 required-field items + key 4 + array = 11 decoded items.
        decode_metadata(self.metadata(b"\x99\xff\xf5" + b"\x00" * 65525))
        with self.assertRaises(MetadataProfileError):
            decode_metadata(self.metadata(b"\x99\xff\xf6" + b"\x00" * 65526))

    def test_candidate_manifest_detects_tampering_and_omission(self):
        source = ROOT / "fixtures/rem-object-supplement-draft"
        verify_supplement_manifest(source)
        with tempfile.TemporaryDirectory() as scratch:
            import shutil
            target = Path(scratch) / "candidate"
            shutil.copytree(source, target)
            extra = target / "unlisted.txt"
            extra.write_text("unlisted")
            with self.assertRaises(AssertionError):
                verify_supplement_manifest(target)
            extra.unlink()
            readme = target / "README.md"
            readme.write_bytes(readme.read_bytes() + b"\n")
            with self.assertRaises(AssertionError):
                verify_supplement_manifest(target)


if __name__ == "__main__":
    unittest.main()
