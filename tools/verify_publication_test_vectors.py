#!/usr/bin/env python3
"""Verify the standalone REM-OBJECT/REM-PARITY publication archive and coverage."""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import sys
import tarfile
import tempfile


sys.dont_write_bytecode = True


REQUIRED_POSITIVE = {
    "minimal-image",
    "final-partial-epoch",
    "external-parity-map",
    "no-parity",
    "checkpoint-prefix",
    "resume-round-trip",
    "default-geometry-header",
    "short-epoch-r-less-than-s",
    "object-id-36-bootstrap",
    "key-30-plaintext-attested",
    "key-30-encrypted-attested",
}
REQUIRED_NEGATIVE = {
    "bootstrap": {
        "bootstrap-bad-magic", "bootstrap-schema-major-2",
        "bootstrap-header-crc-bit-flip", "bootstrap-payload-crc-bit-flip",
        "bootstrap-payload-truncation", "bootstrap-inline-and-external-directory",
        "bootstrap-drive-compression-enabled", "bootstrap-oversize-payload",
        "bootstrap-object-id-65",
    },
    "sidecar": {
        "sidecar-magic", "sidecar-tape-uuid", "sidecar-k-zero", "sidecar-m-zero",
        "sidecar-s-zero", "sidecar-block-size", "sidecar-schema-version",
        "sidecar-end-not-after-start", "sidecar-logical-shard-count",
        "sidecar-real-data-shard-count", "sidecar-parity-block-count",
        "sidecar-data-crc-count", "sidecar-header-block-count",
        "sidecar-inline-index-bytes", "sidecar-total-block-count",
        "sidecar-primary-start", "sidecar-tail-start", "sidecar-footer-index",
        "sidecar-copy-kind", "sidecar-copy-kind-reserved",
        "sidecar-copy-generation", "sidecar-canonical-hash",
        "sidecar-header-reserved", "sidecar-header-crc", "sidecar-zero-fill",
        "sidecar-block0-crc", "sidecar-epoch-start", "sidecar-index-entry-straddle",
        "sidecar-spill-block-crc", "sidecar-index-reserved",
        "sidecar-primary-tail-disagreement", "sidecar-footer-total-disagreement",
    },
    "parity-map": {
        "parity-map-payload-sha256", "parity-map-locator-header-disagreement",
        "parity-map-directory-unknown-flag", "parity-map-directory-nonascending",
        "parity-map-directory-watermark",
    },
    "digest": {
        "digest-tape-file-count", "digest-map-total-data-ordinals",
        "digest-highest-protected-ordinal",
    },
    "recovery": {
        "recovery-m-plus-one-erasures", "recovery-corrupt-peer-as-erasure",
        "recovery-reconstructed-crc-mismatch", "recovery-pending-epoch",
        "recovery-outside-prefix",
    },
    "directory": {
        "directory-overlapping-ranges",
        "directory-gapped-ranges",
        "directory-duplicate-epoch",
        "directory-nonzero-first-start",
    },
}
REQUIRED_DAMAGE = {
    "object-head",
    "sidecar-primary-header",
    "sidecar-footer",
    "sidecar-footer-and-primary",
    "parity-map-primary",
    "bootstrap-copy",
    "multi-parity-map-selection",
    "boundary-straddling-burst-m-limit",
    "boundary-straddling-burst-m-plus-one",
    "short-epoch-boundary-burst-unrecoverable",
}
REQUIRED_REM_OBJECT_OBJECTS = {
    "rem-object-tv-attribute-ext-combined.rem-object",
    "rem-object-tv-boundary.rem-object",
    "rem-object-tv-d1-encrypted.rem-object",
    "rem-object-tv-d1-plaintext.rem-object",
    "rem-object-tv-e2.rem-object",
    "rem-object-tv-ext-member.rem-object",
    "rem-object-tv-empty-file.rem-object",
    "rem-object-tv-empty.rem-object",
    "rem-object-tv-hardlinks.rem-object",
    "rem-object-tv-manifest.rem-object",
    "rem-object-tv-metadata.rem-object",
    "rem-object-tv-nonregular.rem-object",
    "rem-object-tv-nonuser-attribute.rem-object",
    "rem-object-tv-one-byte.rem-object",
    "rem-object-tv-order.rem-object",
    "rem-object-tv-p1.rem-object",
    "rem-object-tv-paths.rem-object",
    "rem-object-tv-portable-core-only.rem-object",
    "rem-object-tv-xattrs.rem-object",
}
REQUIRED_REM_OBJECT_INCREMENT = {
    "REM-OBJECT-TV-PORTABLE-CORE-ONLY",
    "REM-OBJECT-TV-NONUSER-ATTRIBUTE",
    "REM-OBJECT-TV-EXT-MEMBER",
    "REM-OBJECT-TV-ATTRIBUTE-EXT-COMBINED",
}
REQUIRED_REM_OBJECT_NEGATIVE = {
    "inventory-disagrees-with-entries": "ManifestInvalid",
    "ext-value-not-map": "ManifestInvalid",
    "ext-member-noncanonical-cbor": "Cbor",
    "manifest-tamper-repointed-path": "ManifestDigestMismatch",
    "manifest-tamper-swapped-file-sha256": "ManifestDigestMismatch",
    "manifest-tamper-altered-first-chunk-lba": "ManifestDigestMismatch",
}
REQUIRED_REM_OBJECT_ENVELOPE_NEGATIVE = {
    "reserved-wrap-suite-01": "InvalidWrapSuite",
    "key-frame-len-below-minimum": "InvalidKeyFrameLength",
    "key-frame-len-above-maximum": "InvalidKeyFrameLength",
}
REQUIRED_REM_OBJECT_KATS = {
    "xwing-draft10-kat.txt",
    "xwing-wrap-kat.txt",
}
REQUIRED_REM_OBJECT_RANGE = {
    "encrypted-last-object-chunk": ("positive/range", "authenticated-range"),
    "encrypted-last-object-chunk-wrong-finality": (
        "negative/range",
        "AeadAuthenticationFailed",
    ),
}
REQUIRED_KEY_FRAME_CASES = {
    "version-flip",
    "suite-flip",
    "reserved-wrap-suite-01",
    "truncated-key-frame",
    "duplicate-slots",
    "misordered-slots",
    "key-frame-trailing-byte",
    "oversize-key-frame",
    "key-frame-label-tamper",
    "key-frame-enc-tamper",
    "key-frame-ciphertext-tamper",
    "key-frame-slot-inserted",
    "key-frame-slot-removed",
    "slot-count-zero",
    "slot-count-nine",
    "writer-zero-slots",
    "writer-one-slot",
    "writer-nine-slots",
    "reader-one-slot",
    "wrap-suite-zero-nonempty",
    "hpke-zero-key-frame-len",
    "hpke-undersized-key-frame-len",
    "duplicate-recipient-epoch-id",
    "internal-slot-truncation",
    "nonzero-reserved-key-region",
    "malformed-key-frame-magic",
    "wrong-recipient-private-key",
    "malformed-encapsulation",
}


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def fail(message: str) -> None:
    raise SystemExit(f"publication vector verification failed: {message}")


def load_json(path: pathlib.Path) -> dict[str, object]:
    """Load one required archive JSON document or fail with its path."""
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        fail(f"cannot parse {path}: {error}")
    if not isinstance(value, dict):
        fail(f"{path} is not a JSON object")
    return value


def kat_fields(path: pathlib.Path) -> dict[str, bytes]:
    """Parse one strict name=hex KAT document."""
    fields: dict[str, bytes] = {}
    for line in path.read_text(encoding="ascii").splitlines():
        if not line or line.startswith("#"):
            continue
        try:
            name, encoded = line.split("=", 1)
            value = bytes.fromhex(encoded)
        except ValueError as error:
            fail(f"invalid KAT line in {path}: {line!r}: {error}")
        if not name or name in fields:
            fail(f"missing or duplicate KAT field name in {path}: {name!r}")
        fields[name] = value
    return fields


def verify_xwing_key_frame(stored: bytes, label: str) -> None:
    """Check the fixed X-Wing envelope discriminator, bounds, and slot sizes."""
    if len(stored) < 128 or stored[:4] != b"REMO":
        fail(f"{label} lacks a complete REM-OBJECT scalar header")
    if stored[0x38] != 0x02:
        fail(f"{label} does not carry wrap_suite 0x02")
    key_frame_len = int.from_bytes(stored[0x3C:0x40], "big")
    if not 1191 <= key_frame_len <= 16384:
        fail(f"{label} key_frame_len is outside [1191,16384]")
    encoded = stored[128 : 128 + key_frame_len]
    if len(encoded) != key_frame_len or encoded[:4] != b"REMK":
        fail(f"{label} has a truncated or malformed key frame")
    slot_count = encoded[4]
    if not 1 <= slot_count <= 8:
        fail(f"{label} has an invalid slot count")
    cursor = 5
    for _ in range(slot_count):
        if cursor + 18 > len(encoded):
            fail(f"{label} has a truncated slot prefix")
        label_len = encoded[cursor + 17]
        cursor += 18
        end = cursor + label_len + 1120 + 48
        if label_len > 32 or end > len(encoded):
            fail(f"{label} does not carry fixed 1120-byte X-Wing enc values")
        cursor = end
    if cursor != len(encoded):
        fail(f"{label} has trailing key-frame bytes")


def verify_xwing_recipient_material(fixture: dict[str, object], label: str) -> None:
    """Require 32-byte seeds and 1216-byte public keys in staged fixtures."""
    inputs = fixture.get("inputs")
    recipients = inputs.get("recipients") if isinstance(inputs, dict) else None
    if not isinstance(recipients, list) or not recipients:
        fail(f"{label} has no recipient material")
    for index, recipient in enumerate(recipients):
        if not isinstance(recipient, dict):
            fail(f"{label} recipient {index} is malformed")
        try:
            seed = bytes.fromhex(str(recipient.get("private_key")))
            public_key = bytes.fromhex(str(recipient.get("public_key")))
        except ValueError:
            fail(f"{label} recipient {index} key material is not hex")
        if (
            len(seed) != 32
            or len(public_key) != 1216
            or recipient.get("private_key_role") != "xwing-seed-32"
        ):
            fail(f"{label} recipient {index} does not carry X-Wing seed/key sizes")


def verify_tree(root: pathlib.Path, rederive_parity: bool) -> int:
    """Verify one extracted publication tree, including independent parity."""
    checksums = root / "CHECKSUMS.sha256"
    if not checksums.is_file():
        fail("CHECKSUMS.sha256 is absent")
    for line in checksums.read_text(encoding="utf-8").splitlines():
        expected, relative = line.split("  ./", 1)
        path = root / relative
        if not path.is_file():
            fail(f"missing checksummed artifact {relative}")
        actual = sha256(path)
        if actual != expected:
            fail(f"checksum mismatch for {relative}: {actual} != {expected}")
    for relative in (
        "tools/rem_parity_rederive.py",
        "tools/verify_rem_object_vectors_independent.py",
        "tools/requirements-rem-object-independent.txt",
    ):
        if not (root / relative).is_file():
            fail(f"standalone independent verifier artifact is absent: {relative}")

    rem_object_root = root / "rem-object"
    rem_object_objects = {path.name for path in (rem_object_root / "objects").glob("*.rem-object")}
    if rem_object_objects != REQUIRED_REM_OBJECT_OBJECTS:
        fail(
            "REM-OBJECT object inventory differs: "
            f"missing={sorted(REQUIRED_REM_OBJECT_OBJECTS - rem_object_objects)}, "
            f"extra={sorted(rem_object_objects - REQUIRED_REM_OBJECT_OBJECTS)}"
        )
    manifests = rem_object_root / "manifests"
    e2 = load_json(manifests / "rem-object-tv-e2.json")
    if e2.get("vector_id") != "REM-OBJECT-TV-E2":
        fail("rem-object-tv-e2.json has the wrong vector_id")
    if e2.get("status") != "pinned-at-generation":
        fail("rem-object-tv-e2.json is not a current generated vector")
    verify_xwing_recipient_material(e2, "REM-OBJECT-TV-E2")
    e2_expected = e2.get("expected")
    if not isinstance(e2_expected, dict) or e2_expected.get("stored_digest") != sha256(
        rem_object_root / "objects" / "rem-object-tv-e2.rem-object"
    ):
        fail("REM-OBJECT-TV-E2 stored_digest does not match its pinned object")
    verify_xwing_key_frame(
        (rem_object_root / "objects" / "rem-object-tv-e2.rem-object").read_bytes(),
        "REM-OBJECT-TV-E2",
    )
    d1 = load_json(manifests / "rem-object-tv-d1.json")
    if d1.get("encrypted_status") != "pinned-at-generation":
        fail("rem-object-tv-d1.json encrypted half is not a current generated vector")
    verify_xwing_recipient_material(d1, "REM-OBJECT-TV-D1 encrypted")
    d1_expected = d1.get("expected")
    d1_encrypted = d1_expected.get("encrypted") if isinstance(d1_expected, dict) else None
    if not isinstance(d1_encrypted, dict) or d1_encrypted.get("stored_digest") != sha256(
        rem_object_root / "objects" / "rem-object-tv-d1-encrypted.rem-object"
    ):
        fail("REM-OBJECT-TV-D1 encrypted stored_digest does not match its pinned object")
    verify_xwing_key_frame(
        (rem_object_root / "objects" / "rem-object-tv-d1-encrypted.rem-object").read_bytes(),
        "REM-OBJECT-TV-D1 encrypted",
    )

    kat_root = rem_object_root / "kats"
    kat_files = {path.name for path in kat_root.glob("*.txt")}
    if kat_files != REQUIRED_REM_OBJECT_KATS:
        fail(
            "REM-OBJECT KAT inventory differs: "
            f"missing={sorted(REQUIRED_REM_OBJECT_KATS - kat_files)}, "
            f"extra={sorted(kat_files - REQUIRED_REM_OBJECT_KATS)}"
        )
    draft10 = kat_fields(kat_root / "xwing-draft10-kat.txt")
    wrap = kat_fields(kat_root / "xwing-wrap-kat.txt")
    for name, size in {
        "seed": 32,
        "eseed": 64,
        "ss": 32,
        "pk": 1216,
        "enc": 1120,
    }.items():
        if len(draft10.get(name, b"")) != size:
            fail(f"draft-10 X-Wing KAT field {name} is not {size} bytes")
    for name, size in {
        "seed": 32,
        "encapsulation_randomness": 64,
        "dek": 32,
        "recipient_epoch_id": 16,
        "slot_index": 1,
        "pk": 1216,
        "enc": 1120,
        "ss": 32,
        "ciphertext": 48,
    }.items():
        if len(wrap.get(name, b"")) != size:
            fail(f"REM-OBJECT X-Wing wrap KAT field {name} is not {size} bytes")

    rem_object_index = load_json(rem_object_root / "vectors.json")
    if rem_object_index.get("vector_set") != "REM-OBJECT-2.0-PUBLICATION-INCREMENT":
        fail("rem_object/vectors.json has the wrong vector_set")
    rem_object_vectors = rem_object_index.get("vectors")
    if not isinstance(rem_object_vectors, list) or not all(
        isinstance(item, dict) for item in rem_object_vectors
    ):
        fail("rem_object/vectors.json has no valid vector list")
    increment = {
        item.get("id") for item in rem_object_vectors if item.get("category") == "positive"
    }
    if increment != REQUIRED_REM_OBJECT_INCREMENT:
        fail(
            "REM-OBJECT increment coverage differs: "
            f"missing={sorted(REQUIRED_REM_OBJECT_INCREMENT - increment)}, "
            f"extra={sorted(increment - REQUIRED_REM_OBJECT_INCREMENT)}"
        )
    rem_object_negative = {
        item.get("id"): item
        for item in rem_object_vectors
        if item.get("category") == "negative/manifest"
    }
    if set(rem_object_negative) != set(REQUIRED_REM_OBJECT_NEGATIVE):
        fail(
            "REM-OBJECT manifest-negative coverage differs: "
            f"missing={sorted(set(REQUIRED_REM_OBJECT_NEGATIVE) - set(rem_object_negative))}, "
            f"extra={sorted(set(rem_object_negative) - set(REQUIRED_REM_OBJECT_NEGATIVE))}"
        )
    rem_object_envelope_negative = {
        item.get("id"): item
        for item in rem_object_vectors
        if item.get("category") == "negative/envelope"
    }
    if set(rem_object_envelope_negative) != set(REQUIRED_REM_OBJECT_ENVELOPE_NEGATIVE):
        fail(
            "REM-OBJECT envelope-negative coverage differs: "
            f"missing={sorted(set(REQUIRED_REM_OBJECT_ENVELOPE_NEGATIVE) - set(rem_object_envelope_negative))}, "
            f"extra={sorted(set(rem_object_envelope_negative) - set(REQUIRED_REM_OBJECT_ENVELOPE_NEGATIVE))}"
        )
    rem_object_ranges = {
        item.get("id"): item
        for item in rem_object_vectors
        if item.get("category") in {"positive/range", "negative/range"}
    }
    if set(rem_object_ranges) != set(REQUIRED_REM_OBJECT_RANGE):
        fail(
            "REM-OBJECT range coverage differs: "
            f"missing={sorted(set(REQUIRED_REM_OBJECT_RANGE) - set(rem_object_ranges))}, "
            f"extra={sorted(set(rem_object_ranges) - set(REQUIRED_REM_OBJECT_RANGE))}"
        )
    tamper_digests = set()
    for item in rem_object_vectors:
        artifacts = item.get("artifacts")
        if not isinstance(artifacts, list):
            fail(f"REM-OBJECT vector {item.get('id')!r} has no artifacts")
        canonical = "".join(
            f"{artifact['sha256']}  {artifact['path']}\n"
            for artifact in sorted(artifacts, key=lambda value: value["path"])
        ).encode("utf-8")
        if hashlib.sha256(canonical).hexdigest() != item.get("checksum_sha256"):
            fail(f"REM-OBJECT vector checksum mismatch for {item.get('id')}")
        vector_root = rem_object_root
        if item.get("category") in {
            "negative/manifest",
            "negative/envelope",
            "positive/range",
            "negative/range",
        }:
            vector_root /= item["archive_path"]
        for artifact in artifacts:
            path = vector_root / artifact["path"]
            if not path.is_file() or sha256(path) != artifact["sha256"]:
                fail(f"REM-OBJECT vector artifact mismatch for {item.get('id')}/{artifact['path']}")
        if item.get("category") == "positive":
            object_path = rem_object_root / item["archive_path"]
            if item.get("full_object_sha256") != sha256(object_path):
                fail(f"REM-OBJECT positive {item.get('id')} full_object_sha256 mismatch")
            if item.get("plaintext_digest") != item.get("full_object_sha256"):
                fail(f"REM-OBJECT positive {item.get('id')} plaintext_digest mismatch")
            if item.get("first_block_sha256") != hashlib.sha256(
                object_path.read_bytes()[:4096]
            ).hexdigest():
                fail(f"REM-OBJECT positive {item.get('id')} first_block_sha256 mismatch")
            fixture_artifacts = [
                artifact
                for artifact in artifacts
                if str(artifact.get("path", "")).endswith(".json")
            ]
            if len(fixture_artifacts) != 1:
                fail(f"REM-OBJECT positive {item.get('id')} does not have one fixture artifact")
            fixture = load_json(rem_object_root / fixture_artifacts[0]["path"])
            fixture_expected = fixture.get("expected")
            if not isinstance(fixture_expected, dict):
                fail(f"REM-OBJECT positive {item.get('id')} fixture lacks expected pins")
            for field in (
                "full_object_sha256",
                "plaintext_digest",
                "first_block_sha256",
                "manifest_sha256",
                "object_metadata",
            ):
                if item.get(field) != fixture_expected.get(field):
                    fail(f"REM-OBJECT positive {item.get('id')} index disagrees on {field}")
        elif item.get("category") == "negative/manifest":
            expected = load_json(vector_root / "expected.json")
            required_error = REQUIRED_REM_OBJECT_NEGATIVE[item["id"]]
            if expected.get("expected_error") != required_error:
                fail(f"REM-OBJECT negative {item['id']} has the wrong typed error")
            for field in (
                "expected_error",
                "plaintext_digest",
                "stored_digest",
                "manifest_sha256",
            ):
                if item.get(field) != expected.get(field):
                    fail(f"REM-OBJECT negative {item['id']} index disagrees on {field}")
            if expected.get("stored_digest") != expected.get("plaintext_digest"):
                fail(f"REM-OBJECT negative {item['id']} plaintext stored_digest differs")
            if expected.get("payload_bytes_unchanged") is not True:
                fail(f"REM-OBJECT negative {item['id']} does not pin constant payloads")
            if item["id"].startswith("manifest-tamper-"):
                input_value = load_json(vector_root / "input.json")
                if not isinstance(input_value.get("external_manifest_anchor"), str):
                    fail(f"REM-OBJECT tamper {item['id']} lacks its external anchor")
                if expected.get("plaintext_digest") == input_value.get(
                    "base_plaintext_digest"
                ):
                    fail(f"REM-OBJECT tamper {item['id']} did not change plaintext_digest")
                tamper_digests.add(expected.get("plaintext_digest"))
        elif item.get("category") == "negative/envelope":
            expected = load_json(vector_root / "expected.json")
            input_value = load_json(vector_root / "input.json")
            required_error = REQUIRED_REM_OBJECT_ENVELOPE_NEGATIVE[item["id"]]
            faulted = vector_root / "faulted-object.rem-object"
            if (
                expected.get("expected_error") != required_error
                or item.get("expected_error") != required_error
                or expected.get("faulted_sha256") != sha256(faulted)
                or item.get("faulted_sha256") != sha256(faulted)
            ):
                fail(f"REM-OBJECT envelope negative {item['id']} has inconsistent pins")
            base = rem_object_root / str(input_value.get("base_artifact"))
            if (
                not base.is_file()
                or input_value.get("base_sha256") != sha256(base)
            ):
                fail(f"REM-OBJECT envelope negative {item['id']} has the wrong base")
            faulted_bytes = faulted.read_bytes()
            if item["id"] == "reserved-wrap-suite-01":
                if faulted_bytes[0x38] != 0x01:
                    fail("legacy wrap-suite negative does not carry 0x01")
            elif item["id"] == "key-frame-len-below-minimum":
                if int.from_bytes(faulted_bytes[0x3C:0x40], "big") != 1190:
                    fail("below-minimum key-frame negative does not carry 1190")
            elif item["id"] == "key-frame-len-above-maximum":
                if int.from_bytes(faulted_bytes[0x3C:0x40], "big") != 16385:
                    fail("above-maximum key-frame negative does not carry 16385")
        elif item.get("category") in {"positive/range", "negative/range"}:
            expected = load_json(vector_root / "expected.json")
            input_value = load_json(vector_root / "input.json")
            required_category, required_result = REQUIRED_REM_OBJECT_RANGE[item["id"]]
            if item.get("category") != required_category:
                fail(f"REM-OBJECT range {item['id']} has the wrong category")
            result = expected.get("expected_outcome", expected.get("expected_error"))
            if result != required_result:
                fail(f"REM-OBJECT range {item['id']} has the wrong expected result")
            object_chunk_count = expected.get("object_chunk_count")
            if not isinstance(object_chunk_count, int) or object_chunk_count < 1:
                fail(f"REM-OBJECT range {item['id']} has an invalid object chunk count")
            if (
                expected.get("first_chunk") != object_chunk_count - 1
                or expected.get("chunk_count") != 1
            ):
                fail(f"REM-OBJECT range {item['id']} does not cover the final object chunk")
            if input_value.get("file_chunk_count") != 1:
                fail(f"REM-OBJECT range {item['id']} does not pin the one-chunk file view")
            expected_finality = item.get("category") == "positive/range"
            if input_value.get("final_flag") is not expected_finality:
                fail(f"REM-OBJECT range {item['id']} pins the wrong final_flag")
            source = rem_object_root / str(input_value.get("base_artifact"))
            if (
                not source.is_file()
                or sha256(source) != expected.get("source_sha256")
                or item.get("source_sha256") != expected.get("source_sha256")
            ):
                fail(f"REM-OBJECT range {item['id']} source digest mismatch")
            d1_plaintext = d1_expected.get("plaintext")
            if not isinstance(d1_plaintext, dict):
                fail("REM-OBJECT-TV-D1 plaintext fixture is malformed")
            if (
                expected.get("manifest_sha256")
                != d1_plaintext.get("manifest_sha256")
                or expected.get("plaintext_digest")
                != d1_encrypted.get("plaintext_digest")
            ):
                fail(f"REM-OBJECT range {item['id']} disagrees with REM-OBJECT-TV-D1 anchors")
    if len(tamper_digests) != 3:
        fail("REM-OBJECT manifest tamper plaintext digests are not distinct")
    key_frame_negative = load_json(manifests / "negative-key-frame.json")
    if key_frame_negative.get("status") != "complete":
        fail("negative-key-frame.json is not marked complete")
    key_frame_cases = key_frame_negative.get("cases")
    if not isinstance(key_frame_cases, list) or not all(
        isinstance(case, dict) for case in key_frame_cases
    ):
        fail("negative-key-frame.json cases are malformed")
    key_frame_case_ids = [case.get("id") for case in key_frame_cases]
    if not all(isinstance(case_id, str) for case_id in key_frame_case_ids):
        fail("negative-key-frame.json case ids are malformed")
    if len(key_frame_case_ids) != len(set(key_frame_case_ids)):
        fail("negative-key-frame.json contains duplicate case ids")
    if set(key_frame_case_ids) != REQUIRED_KEY_FRAME_CASES:
        fail(
            "key-frame coverage differs: "
            f"missing={sorted(REQUIRED_KEY_FRAME_CASES - set(key_frame_case_ids))}, "
            f"extra={sorted(set(key_frame_case_ids) - REQUIRED_KEY_FRAME_CASES)}"
        )
    for case in key_frame_cases:
        outcomes = int("expected_error" in case) + int("expected_outcome" in case)
        if outcomes != 1 or not isinstance(case.get("operation"), str):
            fail(f"key-frame case {case.get('id')!r} has an invalid outcome/operation")

    vector_file = root / "rem-parity-1" / "vectors.json"
    document = load_json(vector_file)
    vectors = document.get("vectors")
    if not isinstance(vectors, list):
        fail("vectors.json has no vector list")

    positive = {item["id"] for item in vectors if item.get("category") == "positive"}
    if positive != REQUIRED_POSITIVE:
        fail(f"positive coverage differs: missing={sorted(REQUIRED_POSITIVE - positive)}, extra={sorted(positive - REQUIRED_POSITIVE)}")
    for category, required_ids in REQUIRED_NEGATIVE.items():
        actual_ids = {
            item["id"]
            for item in vectors
            if item.get("category") == f"negative/{category}"
        }
        if actual_ids != required_ids:
            fail(f"negative/{category} differs: missing={sorted(required_ids - actual_ids)}, extra={sorted(actual_ids - required_ids)}")
    damage = {item["id"] for item in vectors if item.get("category") == "damage-matrix"}
    if damage != REQUIRED_DAMAGE:
        fail(f"damage matrix differs: missing={sorted(REQUIRED_DAMAGE - damage)}, extra={sorted(damage - REQUIRED_DAMAGE)}")

    for item in vectors:
        vector_root = root / "rem-parity-1" / item["archive_path"]
        canonical = "".join(
            f"{artifact['sha256']}  {artifact['path']}\n"
            for artifact in item["artifacts"]
        ).encode("utf-8")
        if hashlib.sha256(canonical).hexdigest() != item["checksum_sha256"]:
            fail(f"vector checksum mismatch for {item['id']}")
        for artifact in item["artifacts"]:
            path = vector_root / artifact["path"]
            if not path.is_file() or sha256(path) != artifact["sha256"]:
                fail(f"vector artifact mismatch for {item['id']}/{artifact['path']}")
        expected_file = vector_root / "expected.json"
        if not expected_file.is_file():
            fail(f"vector {item['id']} has no expected.json")
        expected = json.loads(expected_file.read_text(encoding="utf-8"))
        if "expected_outcome" not in expected and "expected_error" not in expected:
            fail(f"vector {item['id']} has neither an expected outcome nor typed error")
        if item["category"].startswith("negative/") and not any(
            path.name.startswith("faulted-") or path.name == "input.json"
            for path in vector_root.iterdir()
        ):
            fail(f"negative vector {item['id']} has no deterministic input artifact")
        if item["id"] == "object-id-36-bootstrap":
            object_id = expected.get("object_id")
            bootstrap = vector_root / "tape-file-000-bootstrap.bin"
            object_path = vector_root / "tape-file-001-object.bin"
            if (
                not isinstance(object_id, str)
                or len(object_id.encode("utf-8")) != 36
                or bootstrap.read_bytes().count(object_id.encode("utf-8")) != 1
            ):
                fail("object-id-36 bootstrap does not carry one exact 36-byte id")
            if sha256(object_path) != expected.get("plaintext_digest"):
                fail("object-id-36 bootstrap object digest is not pinned")
        if item["id"] in {
            "key-30-plaintext-attested",
            "key-30-encrypted-attested",
        }:
            object_id = expected.get("object_id")
            final_bootstrap = vector_root / "tape-file-003-final-bootstrap.bin"
            object_path = vector_root / "tape-file-001-object.bin"
            source_name = (
                "rem-object-tv-p1.rem-object"
                if item["id"] == "key-30-plaintext-attested"
                else "rem-object-tv-e2.rem-object"
            )
            expected_outcome = (
                "attested-manifest-verified"
                if item["id"] == "key-30-plaintext-attested"
                else "attested-envelope-consistent"
            )
            if (
                not isinstance(object_id, str)
                or len(object_id.encode("utf-8")) != 36
                or final_bootstrap.read_bytes().count(object_id.encode("utf-8")) != 1
                or expected.get("expected_outcome") != expected_outcome
                or expected.get("attested_scope_tape_file_count") != 4
                or object_path.read_bytes()
                != (rem_object_root / "objects" / source_name).read_bytes()
            ):
                fail(f"{item['id']} is not the pinned attested key-30 image")
        if item["id"] == "bootstrap-object-id-65":
            faulted = vector_root / "faulted-bootstrap.bin"
            if faulted.read_bytes().count(b"x" * 65) != 1:
                fail("object-id-65 negative does not carry one 65-byte id")
            if expected.get("expected_error") != "BootstrapParse":
                fail("object-id-65 negative has the wrong typed error")
        if item.get("category") == "negative/directory":
            if expected.get("expected_error") != "DirectoryInvalid":
                fail(f"directory vector {item['id']} has the wrong typed error")
            if not (vector_root / "faulted-bootstrap.bin").is_file():
                fail(f"directory vector {item['id']} lacks its faulted image")
        if item["category"] == "damage-matrix":
            fault_map_file = vector_root / "fault-map.json"
            source_file = vector_root / "source-artifact.bin"
            if not fault_map_file.is_file() or not source_file.is_file():
                fail(f"damage vector {item['id']} lacks source artifact or fault map")
            fault_map = json.loads(fault_map_file.read_text(encoding="utf-8"))
            if fault_map.get("fault_model") != "transport-medium-error":
                fail(f"damage vector {item['id']} has the wrong fault model")
            if not fault_map.get("unreadable_block_indices"):
                fail(f"damage vector {item['id']} has no unreadable blocks")
            if expected.get("whole_tape_failure") is not False:
                fail(f"damage vector {item['id']} does not rule out whole-tape failure")
            if "burst_span_records" in expected:
                geometry = expected.get("geometry")
                if not isinstance(geometry, dict):
                    fail(f"damage vector {item['id']} lacks burst geometry")
                m = geometry.get("m")
                stripes = geometry.get("S")
                header_blocks = geometry.get("H")
                real_data_blocks = geometry.get("R")
                if not all(
                    isinstance(value, int) and value > 0
                    for value in (m, stripes, header_blocks, real_data_blocks)
                ):
                    fail(f"damage vector {item['id']} has invalid burst geometry")
                if item["id"] == "boundary-straddling-burst-m-limit":
                    required_span = m * stripes + header_blocks + 1
                    required_lost = m
                    required_outcome = "recovered"
                elif item["id"] == "boundary-straddling-burst-m-plus-one":
                    required_span = m * stripes + header_blocks + 2
                    required_lost = m + 1
                    required_outcome = "Unrecoverable"
                elif item["id"] == "short-epoch-boundary-burst-unrecoverable":
                    if real_data_blocks >= stripes:
                        fail("short-epoch burst does not satisfy R < S")
                    required_span = (
                        (m - 1) * stripes
                        + real_data_blocks
                        + header_blocks
                        + 2
                    )
                    required_lost = m + 1
                    required_outcome = "Unrecoverable"
                else:
                    fail(f"unknown burst damage vector {item['id']}")
                if (
                    expected.get("burst_span_records") != required_span
                    or fault_map.get("burst_span_records") != required_span
                    or expected.get("lost_count") != required_lost
                    or expected.get("limit") != m
                    or expected.get("expected_outcome") != required_outcome
                ):
                    fail(f"damage vector {item['id']} has inconsistent burst arithmetic")
            if item["id"] == "multi-parity-map-selection":
                layout = load_json(vector_root / "tape-layout.json")
                tape_files = layout.get("tape_files")
                if not isinstance(tape_files, list) or len(tape_files) != 8:
                    fail("multi-parity-map image does not pin eight tape files")
                parity_maps = [
                    row
                    for row in tape_files
                    if isinstance(row, dict)
                    and "parity-map" in str(row.get("artifact", ""))
                ]
                if len(parity_maps) < 2:
                    fail("multi-parity-map image has fewer than two parity_map files")
                if expected.get("no_usable_bootstrap_directory") is not True:
                    fail("multi-parity-map image does not rule out bootstrap directory use")
                if expected.get("selected_parity_map_tape_file_number") != 4:
                    fail("multi-parity-map image selected the wrong tape file")
                scope = expected.get("selected_scope")
                if not isinstance(scope, dict) or scope != {
                    "highest_protected_ordinal": 1,
                    "is_final_directory": True,
                    "tape_file_count": 8,
                    "total_data_ordinals": 2,
                }:
                    fail("multi-parity-map selected scope is not pinned exactly")
                conflict = expected.get("identical_key_report")
                if not isinstance(conflict, dict) or conflict != {
                    "candidate_tape_file_numbers": [4, 6],
                    "chosen_tape_file_number": 4,
                    "content_disagrees": True,
                }:
                    fail("multi-parity-map identical-key report is incomplete")
                if expected.get("ranking_candidates") != [
                    {"key": [True, 6, 2], "tape_file_number": 2},
                    {"key": [True, 7, 2], "tape_file_number": 4},
                    {"key": [True, 7, 2], "tape_file_number": 6},
                ]:
                    fail("multi-parity-map ranking candidates are not pinned exactly")
                if not isinstance(expected.get("recovered_map_cbor_hex"), str) or not isinstance(
                    expected.get("recovered_map_sha256"), str
                ):
                    fail("multi-parity-map recovered map is not byte-pinned")
                try:
                    recovered_map = bytes.fromhex(expected["recovered_map_cbor_hex"])
                except ValueError:
                    fail("multi-parity-map recovered map is not valid hex")
                if hashlib.sha256(recovered_map).hexdigest() != expected[
                    "recovered_map_sha256"
                ]:
                    fail("multi-parity-map recovered map digest does not match its bytes")
                concatenated = b"".join(
                    (vector_root / row["artifact"]).read_bytes()
                    for row in tape_files
                )
                if concatenated != source_file.read_bytes():
                    fail("multi-parity-map source artifact differs from its tape files")
                unreadable = fault_map.get("unreadable_tape_records")
                if not isinstance(unreadable, list) or unreadable != [
                    {
                        "block_index": 0,
                        "concatenated_block_index": 13,
                        "tape_file_number": 7,
                    }
                ]:
                    fail("multi-parity-map fault does not target the referencing bootstrap head")

    if rederive_parity:
        module_directory = pathlib.Path(__file__).resolve().parent
        if not (module_directory / "rem_parity_rederive.py").is_file():
            module_directory /= "tools"
        if not (module_directory / "rem_parity_rederive.py").is_file():
            fail("independent REM-PARITY re-derivation module is absent")
        sys.path.insert(0, str(module_directory))
        try:
            from rem_parity_rederive import (
                RederivationError,
                rederive_publication_vectors,
            )
        except ImportError as error:
            fail(f"cannot import independent REM-PARITY verifier: {error}")
        try:
            summary = rederive_publication_vectors(root)
        except (OSError, ValueError, KeyError, TypeError, RederivationError) as error:
            fail(f"independent REM-PARITY re-derivation mismatch: {error}")
        finally:
            sys.path.pop(0)
        for line in summary.report_lines():
            print(line)

    print(f"PASS: {len(vectors)} REM-PARITY vectors and all archive checksums verified")
    return 0


def parse_args(argv: list[str]) -> argparse.Namespace:
    """Parse the standalone verifier command line."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "publication",
        nargs="?",
        type=pathlib.Path,
        help=(
            "extracted publication root or remanence-test-vectors.tar; "
            "defaults to the bundled tree or repository archive"
        ),
    )
    parity = parser.add_mutually_exclusive_group()
    parity.add_argument(
        "--rederive-parity",
        dest="rederive_parity",
        action="store_true",
        default=True,
        help="independently re-derive REM-PARITY pins (default)",
    )
    parity.add_argument(
        "--no-rederive-parity",
        dest="rederive_parity",
        action="store_false",
        help="run structural/checksum verification without parity re-derivation",
    )
    return parser.parse_args(argv)


def _default_publication_path() -> pathlib.Path:
    """Select the bundled tree or the checkout's published tar archive."""
    script_directory = pathlib.Path(__file__).resolve().parent
    if (script_directory / "CHECKSUMS.sha256").is_file():
        return script_directory
    archive = (
        script_directory.parent
        / "specs"
        / "publication"
        / "remanence-test-vectors.tar"
    )
    if archive.is_file():
        return archive
    fail("no publication root or remanence-test-vectors.tar was supplied")
    raise AssertionError("unreachable")


def _extract_archive(archive_path: pathlib.Path, destination: pathlib.Path) -> pathlib.Path:
    """Extract a publication tar after rejecting links and unsafe member paths."""
    try:
        with tarfile.open(archive_path, mode="r:*") as archive:
            members = archive.getmembers()
            for member in members:
                relative = pathlib.PurePosixPath(member.name)
                if (
                    relative.is_absolute()
                    or ".." in relative.parts
                    or not (member.isfile() or member.isdir())
                ):
                    fail(f"unsafe archive member {member.name!r}")
            archive.extractall(destination, members=members)
    except (OSError, tarfile.TarError) as error:
        fail(f"cannot extract {archive_path}: {error}")
    if (destination / "CHECKSUMS.sha256").is_file():
        return destination
    roots = [
        child
        for child in destination.iterdir()
        if child.is_dir() and (child / "CHECKSUMS.sha256").is_file()
    ]
    if len(roots) != 1:
        fail(f"{archive_path} does not contain one publication root")
    return roots[0]


def main(argv: list[str] | None = None) -> int:
    """Resolve a tree or tar input and run the complete verifier."""
    args = parse_args(sys.argv[1:] if argv is None else argv)
    publication = (
        args.publication.resolve()
        if args.publication is not None
        else _default_publication_path()
    )
    if publication.is_dir():
        return verify_tree(publication, args.rederive_parity)
    if not publication.is_file():
        fail(f"publication path does not exist: {publication}")
    with tempfile.TemporaryDirectory(
        prefix="remanence-publication-verify-"
    ) as temporary_name:
        root = _extract_archive(publication, pathlib.Path(temporary_name))
        return verify_tree(root, args.rederive_parity)


if __name__ == "__main__":
    raise SystemExit(main())
