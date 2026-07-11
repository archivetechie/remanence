#!/usr/bin/env python3
"""Verify archive checksums and complete REM-PARITY Section 17 coverage."""

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
}
REQUIRED_NEGATIVE = {
    "bootstrap": {
        "bootstrap-bad-magic", "bootstrap-schema-major-2",
        "bootstrap-header-crc-bit-flip", "bootstrap-payload-crc-bit-flip",
        "bootstrap-payload-truncation", "bootstrap-inline-and-external-directory",
        "bootstrap-drive-compression-enabled", "bootstrap-oversize-payload",
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
}
REQUIRED_DAMAGE = {
    "object-head",
    "sidecar-primary-header",
    "sidecar-footer",
    "sidecar-footer-and-primary",
    "parity-map-primary",
    "bootstrap-copy",
}


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def fail(message: str) -> None:
    raise SystemExit(f"publication vector verification failed: {message}")


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

    vector_file = root / "rem-parity-1" / "vectors.json"
    try:
        document = json.loads(vector_file.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        fail(f"cannot parse vectors.json: {error}")
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

    print(f"PASS: {len(vectors)} REM-PARITY vectors and all archive checksums verified")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
