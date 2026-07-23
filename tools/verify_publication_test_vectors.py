#!/usr/bin/env python3
"""Verify the standalone RAO/REM-PARITY publication archive and coverage."""

from __future__ import annotations

import hashlib
import json
import pathlib
import sys


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
REQUIRED_RAO_OBJECTS = {
    "rao-tv-attribute-ext-combined.rao",
    "rao-tv-boundary.rao",
    "rao-tv-d1-encrypted.rao",
    "rao-tv-d1-plaintext.rao",
    "rao-tv-e2.rao",
    "rao-tv-ext-member.rao",
    "rao-tv-empty-file.rao",
    "rao-tv-empty.rao",
    "rao-tv-hardlinks.rao",
    "rao-tv-manifest.rao",
    "rao-tv-metadata.rao",
    "rao-tv-nonregular.rao",
    "rao-tv-nonuser-attribute.rao",
    "rao-tv-one-byte.rao",
    "rao-tv-order.rao",
    "rao-tv-p1.rao",
    "rao-tv-paths.rao",
    "rao-tv-portable-core-only.rao",
    "rao-tv-xattrs.rao",
}
REQUIRED_RAO_INCREMENT = {
    "RAO-TV-PORTABLE-CORE-ONLY",
    "RAO-TV-NONUSER-ATTRIBUTE",
    "RAO-TV-EXT-MEMBER",
    "RAO-TV-ATTRIBUTE-EXT-COMBINED",
}
REQUIRED_RAO_NEGATIVE = {
    "inventory-disagrees-with-entries": "ManifestInvalid",
    "ext-value-not-map": "ManifestInvalid",
    "ext-member-noncanonical-cbor": "Cbor",
    "manifest-tamper-repointed-path": "ManifestDigestMismatch",
    "manifest-tamper-swapped-file-sha256": "ManifestDigestMismatch",
    "manifest-tamper-altered-first-chunk-lba": "ManifestDigestMismatch",
}
REQUIRED_RAO_ENVELOPE_NEGATIVE = {
    "legacy-x25519-wrap-suite": "InvalidWrapSuite",
    "key-frame-len-below-minimum": "InvalidKeyFrameLength",
    "key-frame-len-above-maximum": "InvalidKeyFrameLength",
}
REQUIRED_RAO_KATS = {
    "xwing-draft10-kat.txt",
    "xwing-wrap-kat.txt",
}
REQUIRED_RAO_RANGE = {
    "encrypted-last-object-chunk": ("positive/range", "authenticated-range"),
    "encrypted-last-object-chunk-wrong-finality": (
        "negative/range",
        "AeadAuthenticationFailed",
    ),
}
REQUIRED_KEY_FRAME_CASES = {
    "version-flip",
    "suite-flip",
    "legacy-x25519-wrap-suite",
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
    if len(stored) < 128 or stored[:4] != b"RAO1":
        fail(f"{label} lacks a complete RAO scalar header")
    if stored[0x38] != 0x02:
        fail(f"{label} does not carry wrap_suite 0x02")
    key_frame_len = int.from_bytes(stored[0x3C:0x40], "big")
    if not 1191 <= key_frame_len <= 16384:
        fail(f"{label} key_frame_len is outside [1191,16384]")
    encoded = stored[128 : 128 + key_frame_len]
    if len(encoded) != key_frame_len or encoded[:4] != b"RAOK":
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


def main() -> int:
    root = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else pathlib.Path(__file__).resolve().parent
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
        "tools/verify_rao_vectors_independent.py",
        "tools/requirements-rao-independent.txt",
    ):
        if not (root / relative).is_file():
            fail(f"standalone independent verifier artifact is absent: {relative}")

    rao_root = root / "rao"
    rao_objects = {path.name for path in (rao_root / "objects").glob("*.rao")}
    if rao_objects != REQUIRED_RAO_OBJECTS:
        fail(
            "RAO object inventory differs: "
            f"missing={sorted(REQUIRED_RAO_OBJECTS - rao_objects)}, "
            f"extra={sorted(rao_objects - REQUIRED_RAO_OBJECTS)}"
        )
    manifests = rao_root / "manifests"
    e2 = load_json(manifests / "rao-tv-e2.json")
    if e2.get("vector_id") != "RAO-TV-E2":
        fail("rao-tv-e2.json has the wrong vector_id")
    if e2.get("status") != "pinned-at-generation":
        fail("rao-tv-e2.json is not a current generated vector")
    verify_xwing_recipient_material(e2, "RAO-TV-E2")
    e2_expected = e2.get("expected")
    if not isinstance(e2_expected, dict) or e2_expected.get("stored_digest") != sha256(
        rao_root / "objects" / "rao-tv-e2.rao"
    ):
        fail("RAO-TV-E2 stored_digest does not match its pinned object")
    verify_xwing_key_frame(
        (rao_root / "objects" / "rao-tv-e2.rao").read_bytes(),
        "RAO-TV-E2",
    )
    d1 = load_json(manifests / "rao-tv-d1.json")
    if d1.get("encrypted_status") != "pinned-at-generation":
        fail("rao-tv-d1.json encrypted half is not a current generated vector")
    verify_xwing_recipient_material(d1, "RAO-TV-D1 encrypted")
    d1_expected = d1.get("expected")
    d1_encrypted = d1_expected.get("encrypted") if isinstance(d1_expected, dict) else None
    if not isinstance(d1_encrypted, dict) or d1_encrypted.get("stored_digest") != sha256(
        rao_root / "objects" / "rao-tv-d1-encrypted.rao"
    ):
        fail("RAO-TV-D1 encrypted stored_digest does not match its pinned object")
    verify_xwing_key_frame(
        (rao_root / "objects" / "rao-tv-d1-encrypted.rao").read_bytes(),
        "RAO-TV-D1 encrypted",
    )

    kat_root = rao_root / "kats"
    kat_files = {path.name for path in kat_root.glob("*.txt")}
    if kat_files != REQUIRED_RAO_KATS:
        fail(
            "RAO KAT inventory differs: "
            f"missing={sorted(REQUIRED_RAO_KATS - kat_files)}, "
            f"extra={sorted(kat_files - REQUIRED_RAO_KATS)}"
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
            fail(f"RAO X-Wing wrap KAT field {name} is not {size} bytes")

    rao_index = load_json(rao_root / "vectors.json")
    if rao_index.get("vector_set") != "RAO-2.0-PUBLICATION-INCREMENT":
        fail("rao/vectors.json has the wrong vector_set")
    rao_vectors = rao_index.get("vectors")
    if not isinstance(rao_vectors, list) or not all(
        isinstance(item, dict) for item in rao_vectors
    ):
        fail("rao/vectors.json has no valid vector list")
    increment = {
        item.get("id") for item in rao_vectors if item.get("category") == "positive"
    }
    if increment != REQUIRED_RAO_INCREMENT:
        fail(
            "RAO increment coverage differs: "
            f"missing={sorted(REQUIRED_RAO_INCREMENT - increment)}, "
            f"extra={sorted(increment - REQUIRED_RAO_INCREMENT)}"
        )
    rao_negative = {
        item.get("id"): item
        for item in rao_vectors
        if item.get("category") == "negative/manifest"
    }
    if set(rao_negative) != set(REQUIRED_RAO_NEGATIVE):
        fail(
            "RAO manifest-negative coverage differs: "
            f"missing={sorted(set(REQUIRED_RAO_NEGATIVE) - set(rao_negative))}, "
            f"extra={sorted(set(rao_negative) - set(REQUIRED_RAO_NEGATIVE))}"
        )
    rao_envelope_negative = {
        item.get("id"): item
        for item in rao_vectors
        if item.get("category") == "negative/envelope"
    }
    if set(rao_envelope_negative) != set(REQUIRED_RAO_ENVELOPE_NEGATIVE):
        fail(
            "RAO envelope-negative coverage differs: "
            f"missing={sorted(set(REQUIRED_RAO_ENVELOPE_NEGATIVE) - set(rao_envelope_negative))}, "
            f"extra={sorted(set(rao_envelope_negative) - set(REQUIRED_RAO_ENVELOPE_NEGATIVE))}"
        )
    rao_ranges = {
        item.get("id"): item
        for item in rao_vectors
        if item.get("category") in {"positive/range", "negative/range"}
    }
    if set(rao_ranges) != set(REQUIRED_RAO_RANGE):
        fail(
            "RAO range coverage differs: "
            f"missing={sorted(set(REQUIRED_RAO_RANGE) - set(rao_ranges))}, "
            f"extra={sorted(set(rao_ranges) - set(REQUIRED_RAO_RANGE))}"
        )
    tamper_digests = set()
    for item in rao_vectors:
        artifacts = item.get("artifacts")
        if not isinstance(artifacts, list):
            fail(f"RAO vector {item.get('id')!r} has no artifacts")
        canonical = "".join(
            f"{artifact['sha256']}  {artifact['path']}\n"
            for artifact in sorted(artifacts, key=lambda value: value["path"])
        ).encode("utf-8")
        if hashlib.sha256(canonical).hexdigest() != item.get("checksum_sha256"):
            fail(f"RAO vector checksum mismatch for {item.get('id')}")
        vector_root = rao_root
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
                fail(f"RAO vector artifact mismatch for {item.get('id')}/{artifact['path']}")
        if item.get("category") == "positive":
            object_path = rao_root / item["archive_path"]
            if item.get("full_object_sha256") != sha256(object_path):
                fail(f"RAO positive {item.get('id')} full_object_sha256 mismatch")
            if item.get("plaintext_digest") != item.get("full_object_sha256"):
                fail(f"RAO positive {item.get('id')} plaintext_digest mismatch")
            if item.get("first_block_sha256") != hashlib.sha256(
                object_path.read_bytes()[:4096]
            ).hexdigest():
                fail(f"RAO positive {item.get('id')} first_block_sha256 mismatch")
            fixture_artifacts = [
                artifact
                for artifact in artifacts
                if str(artifact.get("path", "")).endswith(".json")
            ]
            if len(fixture_artifacts) != 1:
                fail(f"RAO positive {item.get('id')} does not have one fixture artifact")
            fixture = load_json(rao_root / fixture_artifacts[0]["path"])
            fixture_expected = fixture.get("expected")
            if not isinstance(fixture_expected, dict):
                fail(f"RAO positive {item.get('id')} fixture lacks expected pins")
            for field in (
                "full_object_sha256",
                "plaintext_digest",
                "first_block_sha256",
                "manifest_sha256",
                "object_metadata",
            ):
                if item.get(field) != fixture_expected.get(field):
                    fail(f"RAO positive {item.get('id')} index disagrees on {field}")
        elif item.get("category") == "negative/manifest":
            expected = load_json(vector_root / "expected.json")
            required_error = REQUIRED_RAO_NEGATIVE[item["id"]]
            if expected.get("expected_error") != required_error:
                fail(f"RAO negative {item['id']} has the wrong typed error")
            for field in (
                "expected_error",
                "plaintext_digest",
                "stored_digest",
                "manifest_sha256",
            ):
                if item.get(field) != expected.get(field):
                    fail(f"RAO negative {item['id']} index disagrees on {field}")
            if expected.get("stored_digest") != expected.get("plaintext_digest"):
                fail(f"RAO negative {item['id']} plaintext stored_digest differs")
            if expected.get("payload_bytes_unchanged") is not True:
                fail(f"RAO negative {item['id']} does not pin constant payloads")
            if item["id"].startswith("manifest-tamper-"):
                input_value = load_json(vector_root / "input.json")
                if not isinstance(input_value.get("external_manifest_anchor"), str):
                    fail(f"RAO tamper {item['id']} lacks its external anchor")
                if expected.get("plaintext_digest") == input_value.get(
                    "base_plaintext_digest"
                ):
                    fail(f"RAO tamper {item['id']} did not change plaintext_digest")
                tamper_digests.add(expected.get("plaintext_digest"))
        elif item.get("category") == "negative/envelope":
            expected = load_json(vector_root / "expected.json")
            input_value = load_json(vector_root / "input.json")
            required_error = REQUIRED_RAO_ENVELOPE_NEGATIVE[item["id"]]
            faulted = vector_root / "faulted-object.rao"
            if (
                expected.get("expected_error") != required_error
                or item.get("expected_error") != required_error
                or expected.get("faulted_sha256") != sha256(faulted)
                or item.get("faulted_sha256") != sha256(faulted)
            ):
                fail(f"RAO envelope negative {item['id']} has inconsistent pins")
            base = rao_root / str(input_value.get("base_artifact"))
            if (
                not base.is_file()
                or input_value.get("base_sha256") != sha256(base)
            ):
                fail(f"RAO envelope negative {item['id']} has the wrong base")
            faulted_bytes = faulted.read_bytes()
            if item["id"] == "legacy-x25519-wrap-suite":
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
            required_category, required_result = REQUIRED_RAO_RANGE[item["id"]]
            if item.get("category") != required_category:
                fail(f"RAO range {item['id']} has the wrong category")
            result = expected.get("expected_outcome", expected.get("expected_error"))
            if result != required_result:
                fail(f"RAO range {item['id']} has the wrong expected result")
            object_chunk_count = expected.get("object_chunk_count")
            if not isinstance(object_chunk_count, int) or object_chunk_count < 1:
                fail(f"RAO range {item['id']} has an invalid object chunk count")
            if (
                expected.get("first_chunk") != object_chunk_count - 1
                or expected.get("chunk_count") != 1
            ):
                fail(f"RAO range {item['id']} does not cover the final object chunk")
            if input_value.get("file_chunk_count") != 1:
                fail(f"RAO range {item['id']} does not pin the one-chunk file view")
            expected_finality = item.get("category") == "positive/range"
            if input_value.get("final_flag") is not expected_finality:
                fail(f"RAO range {item['id']} pins the wrong final_flag")
            source = rao_root / str(input_value.get("base_artifact"))
            if (
                not source.is_file()
                or sha256(source) != expected.get("source_sha256")
                or item.get("source_sha256") != expected.get("source_sha256")
            ):
                fail(f"RAO range {item['id']} source digest mismatch")
            d1_plaintext = d1_expected.get("plaintext")
            if not isinstance(d1_plaintext, dict):
                fail("RAO-TV-D1 plaintext fixture is malformed")
            if (
                expected.get("manifest_sha256")
                != d1_plaintext.get("manifest_sha256")
                or expected.get("plaintext_digest")
                != d1_encrypted.get("plaintext_digest")
            ):
                fail(f"RAO range {item['id']} disagrees with RAO-TV-D1 anchors")
    if len(tamper_digests) != 3:
        fail("RAO manifest tamper plaintext digests are not distinct")
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

    print(f"PASS: {len(vectors)} REM-PARITY vectors and all archive checksums verified")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
