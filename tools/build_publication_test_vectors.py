#!/usr/bin/env python3
"""Build the deterministic standalone RAO/REM-PARITY publication archive."""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tarfile
import tempfile
from collections.abc import Callable
from typing import Any

import verify_rao_vectors_independent as independent


ROOT = pathlib.Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "specs" / "publication" / "remanence-test-vectors.tar"
BLOCK_SIZE = 4096
BOOTSTRAP_HEADER_SIZE = 52
PRE_B1_ARCHIVE_SHA256 = (
    "f4e4331c14e67c059d1292f54e14efd8408c7d41364d2dba7f8e7567aa16c2a6"
)
RAO_ENCRYPTED_OBJECTS = (
    "rao-tv-d1-encrypted.rao",
    "rao-tv-e2.rao",
)
RAO_KAT_FILES = (
    "xwing-draft10-kat.txt",
    "xwing-wrap-kat.txt",
)
RAO_INCREMENT_OBJECTS = (
    "rao-tv-portable-core-only.rao",
    "rao-tv-nonuser-attribute.rao",
    "rao-tv-ext-member.rao",
    "rao-tv-attribute-ext-combined.rao",
)
RAO_INCREMENT_FIXTURES = {
    "rao-tv-portable-core-only.rao": "rao-tv-portable-core-only.json",
    "rao-tv-nonuser-attribute.rao": "rao-tv-nonuser-attribute.json",
    "rao-tv-ext-member.rao": "rao-tv-ext-member.json",
    "rao-tv-attribute-ext-combined.rao": "rao-tv-attribute-ext-combined.json",
}


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_json(path: pathlib.Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(value, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
        newline="\n",
    )


def generate_rao_objects(output: pathlib.Path) -> None:
    """Regenerate current X-Wing envelopes and plaintext increment objects."""
    output.mkdir(parents=True)
    environment = os.environ.copy()
    environment["RAO_VECTOR_EXPORT_DIR"] = str(output)
    subprocess.run(
        [
            "cargo",
            "test",
            "--quiet",
            "-p",
            "remanence-format",
            "--test",
            "rao_vectors",
            "rao_publication_objects_regenerate_byte_exactly",
            "--",
            "--exact",
        ],
        cwd=ROOT,
        env=environment,
        check=True,
    )


def verify_regenerated_rao_objects(first: pathlib.Path, second: pathlib.Path) -> None:
    """Require two Rust exports to agree and carry the frozen X-Wing framing."""
    for filename in (*RAO_ENCRYPTED_OBJECTS, *RAO_INCREMENT_OBJECTS):
        first_bytes = (first / filename).read_bytes()
        second_bytes = (second / filename).read_bytes()
        if first_bytes != second_bytes:
            raise AssertionError(f"{filename} differs across deterministic regenerations")
    for filename in RAO_ENCRYPTED_OBJECTS:
        stored = (first / filename).read_bytes()
        if len(stored) < independent.RAO_HEADER_LEN:
            raise AssertionError(f"{filename} is shorter than its scalar header")
        if stored[0x38] != independent.RAO_WRAP_SUITE_XWING:
            raise AssertionError(f"{filename} does not carry wrap_suite 0x02")
        key_frame_len = int.from_bytes(stored[0x3C:0x40], "big")
        if not (
            independent.RAO_KEY_FRAME_MIN_LEN
            <= key_frame_len
            <= independent.RAO_KEY_FRAME_MAX_LEN
        ):
            raise AssertionError(f"{filename} has an out-of-range key frame")
        slots = independent.parse_key_frame(
            filename,
            stored[independent.RAO_HEADER_LEN : independent.RAO_HEADER_LEN + key_frame_len],
            independent.RAO_WRAP_SUITE_XWING,
        )
        if not slots or any(
            len(slot.enc) != independent.XWING_CIPHERTEXT_LEN for slot in slots
        ):
            raise AssertionError(f"{filename} does not contain 1120-byte X-Wing enc values")


def stage_rao_kats(output: pathlib.Path) -> None:
    """Copy the frozen component and wrap KATs into the standalone archive."""
    output.mkdir(parents=True)
    testdata = ROOT / "crates" / "remanence-aead" / "testdata"
    for filename in RAO_KAT_FILES:
        shutil.copyfile(testdata / filename, output / filename)


def crc64_xz(data: bytes | bytearray) -> int:
    """Return the Section 5.1 reflected CRC-64/XZ value."""
    crc = 0xFFFFFFFFFFFFFFFF
    for byte in data:
        crc ^= byte
        for _ in range(8):
            crc = (crc >> 1) ^ (0xC96C5795D7870F42 if crc & 1 else 0)
    return crc ^ 0xFFFFFFFFFFFFFFFF


if crc64_xz(b"123456789") != 0x995DC9BBDF1939FA:
    raise RuntimeError("publication builder CRC-64/XZ self-check failed")


def add_tree(archive: tarfile.TarFile, root: pathlib.Path) -> None:
    for path in sorted(root.rglob("*"), key=lambda item: item.as_posix()):
        relative = path.relative_to(root).as_posix()
        info = archive.gettarinfo(str(path), arcname=relative)
        info.uid = 0
        info.gid = 0
        info.uname = ""
        info.gname = ""
        info.mtime = 0
        info.mode = 0o755 if path.is_dir() else 0o644
        if path.is_file():
            with path.open("rb") as handle:
                archive.addfile(info, handle)
        else:
            archive.addfile(info)


def artifact_checksum(root: pathlib.Path) -> tuple[str, list[dict[str, Any]]]:
    files = sorted(path for path in root.rglob("*") if path.is_file())
    artifacts = [
        {
            "path": path.relative_to(root).as_posix(),
            "size": path.stat().st_size,
            "sha256": sha256(path),
        }
        for path in files
    ]
    canonical = "".join(
        f"{item['sha256']}  {item['path']}\n" for item in artifacts
    ).encode("utf-8")
    return hashlib.sha256(canonical).hexdigest(), artifacts


def mutation_file(
    directory: pathlib.Path,
    base: pathlib.Path,
    output_name: str,
    mutations: list[tuple[int, int]],
    metadata: dict[str, Any],
    repair: Callable[[bytearray], None] | None = None,
) -> None:
    data = bytearray(base.read_bytes())
    for offset, xor_value in mutations:
        if offset < 0 or offset >= len(data):
            raise ValueError(f"mutation offset {offset} outside {base}")
        data[offset] ^= xor_value
    if repair is not None:
        repair(data)
    directory.mkdir(parents=True, exist_ok=True)
    (directory / output_name).write_bytes(data)
    write_json(
        directory / "expected.json",
        {
            **metadata,
            "input": {
                "base_artifact": base.name,
                "mutations": [
                    {"offset": offset, "xor": xor_value}
                    for offset, xor_value in mutations
                ],
            },
        },
    )


def rewrite_bootstrap_payload(
    base: pathlib.Path,
    mutate: Callable[[dict[int, Any]], None],
) -> bytes:
    """Return a validly framed bootstrap whose CBOR payload has one semantic fault."""
    data = bytearray(base.read_bytes())
    payload_len = int.from_bytes(data[0x28:0x2C], "little")
    payload_end = BOOTSTRAP_HEADER_SIZE + payload_len
    payload = independent.decode_cbor_exact(
        bytes(data[BOOTSTRAP_HEADER_SIZE:payload_end])
    )
    if not isinstance(payload, dict):
        raise AssertionError(f"{base} bootstrap payload is not a CBOR map")
    mutate(payload)
    encoded = independent.cbor(payload)
    framed_end = BOOTSTRAP_HEADER_SIZE + len(encoded) + 8
    if framed_end > len(data):
        raise AssertionError(f"mutated bootstrap payload does not fit {base}")
    data[0x28:0x2C] = len(encoded).to_bytes(4, "little")
    data[0x2C:0x34] = crc64_xz(data[:0x2C]).to_bytes(8, "little")
    data[BOOTSTRAP_HEADER_SIZE:] = b"\0" * (
        len(data) - BOOTSTRAP_HEADER_SIZE
    )
    data[BOOTSTRAP_HEADER_SIZE : BOOTSTRAP_HEADER_SIZE + len(encoded)] = encoded
    data[
        BOOTSTRAP_HEADER_SIZE + len(encoded) : framed_end
    ] = crc64_xz(encoded).to_bytes(8, "little")
    return bytes(data)


def semantic_vector(directory: pathlib.Path, metadata: dict[str, Any]) -> None:
    directory.mkdir(parents=True, exist_ok=True)
    write_json(directory / "input.json", metadata.pop("input"))
    write_json(directory / "expected.json", metadata)


def independent_plaintext_definition(
    vector_id: str,
) -> tuple[dict[str, Any], list[independent.FileSpec]]:
    """Return one fixed independent RAO definition by publication vector ID."""
    for _filename, candidate_id, options, files in (
        independent.positive_plaintext_vector_definitions()
    ):
        if candidate_id == vector_id:
            return options, files
    raise AssertionError(f"independent RAO definition {vector_id!r} is absent")


def generate_rao_negatives(rao_root: pathlib.Path) -> list[dict[str, Any]]:
    """Materialize the revised Section 13.6 manifest-profile negative objects."""
    negative_root = rao_root / "negative" / "manifest"
    records: list[dict[str, Any]] = []

    def build_case(
        vector_id: str,
        base_vector_id: str,
        expected_error: str,
        assertion: str,
        mutate: Callable[[dict[str, Any], bytes], bytes],
        external_anchor: bool = False,
    ) -> tuple[str, str]:
        options, files = independent_plaintext_definition(base_vector_id)
        base_bytes, base_layout = independent.build_plaintext(options, files)
        manifest_value = independent.decode_cbor_exact(base_layout["manifest_cbor"])
        if not isinstance(manifest_value, dict):
            raise AssertionError(f"{base_vector_id} manifest is not a map")
        tampered_manifest = mutate(copy.deepcopy(manifest_value), base_layout["manifest_cbor"])
        tampered_bytes, tampered_layout = independent.build_plaintext_with_manifest(
            options,
            files,
            tampered_manifest,
        )
        original_digest = hashlib.sha256(base_bytes).hexdigest()
        tampered_digest = hashlib.sha256(tampered_bytes).hexdigest()
        if original_digest == tampered_digest:
            raise AssertionError(f"{vector_id} did not change plaintext_digest")

        directory = negative_root / vector_id
        directory.mkdir(parents=True, exist_ok=True)
        (directory / "faulted-object.rao").write_bytes(tampered_bytes)
        write_json(
            directory / "input.json",
            {
                "base_vector_id": base_vector_id,
                "base_plaintext_digest": original_digest,
                "base_manifest_sha256": base_layout["manifest_sha256"].hex(),
                "constant_payloads": {
                    spec.path: hashlib.sha256(spec.data).hexdigest()
                    for spec in files
                    if spec.entry_type == "regular"
                },
                **(
                    {"external_manifest_anchor": base_layout["manifest_sha256"].hex()}
                    if external_anchor
                    else {}
                ),
            },
        )
        write_json(
            directory / "expected.json",
            {
                "vector_id": vector_id,
                "category": "manifest",
                "expected_error": expected_error,
                "assertion": assertion,
                "plaintext_digest": tampered_digest,
                "stored_digest": tampered_digest,
                "manifest_sha256": tampered_layout["manifest_sha256"].hex(),
                "payload_bytes_unchanged": True,
            },
        )
        records.append(
            {
                "id": vector_id,
                "category": "negative/manifest",
                "path": directory,
            }
        )
        return tampered_digest, tampered_layout["manifest_sha256"].hex()

    def inventory_disagrees(manifest: dict[str, Any], _encoded: bytes) -> bytes:
        manifest["object_metadata"]["attribute_namespaces"] = ["trusted"]
        return independent.cbor(manifest)

    def ext_is_not_map(manifest: dict[str, Any], _encoded: bytes) -> bytes:
        manifest["object_metadata"]["ext"] = 1
        return independent.cbor(manifest)

    def noncanonical_ext_value(_manifest: dict[str, Any], encoded: bytes) -> bytes:
        member = independent.cbor("org.remanence.publication")
        canonical = member + b"\x01"
        replacement = member + b"\x18\x01"
        if encoded.count(canonical) != 1:
            raise AssertionError("canonical extension member marker is not unique")
        return encoded.replace(canonical, replacement, 1)

    build_case(
        "inventory-disagrees-with-entries",
        "RAO-TV-NONUSER-ATTRIBUTE",
        "ManifestInvalid",
        "object_metadata attribute_namespaces differs from the entry xattr namespace",
        inventory_disagrees,
    )
    build_case(
        "ext-value-not-map",
        "RAO-TV-EXT-MEMBER",
        "ManifestInvalid",
        "object_metadata ext value is not a map",
        ext_is_not_map,
    )
    build_case(
        "ext-member-noncanonical-cbor",
        "RAO-TV-EXT-MEMBER",
        "Cbor",
        "ext member integer uses a non-shortest CBOR encoding",
        noncanonical_ext_value,
    )

    def repoint_path(manifest: dict[str, Any], _encoded: bytes) -> bytes:
        manifest["file_entries"][0]["path"] = "manifest/gamma.bin"
        return independent.cbor(manifest)

    def swap_file_sha256(manifest: dict[str, Any], _encoded: bytes) -> bytes:
        entries = manifest["file_entries"]
        entries[0]["file_sha256"], entries[1]["file_sha256"] = (
            entries[1]["file_sha256"],
            entries[0]["file_sha256"],
        )
        return independent.cbor(manifest)

    def alter_first_chunk_lba(manifest: dict[str, Any], _encoded: bytes) -> bytes:
        manifest["file_entries"][0]["first_chunk_lba"] += 1
        return independent.cbor(manifest)

    tamper_digests = {
        build_case(
            "manifest-tamper-repointed-path",
            "RAO-TV-MANIFEST",
            "ManifestDigestMismatch",
            "external manifest anchor rejects a repointed path with constant payloads",
            repoint_path,
            external_anchor=True,
        )[0],
        build_case(
            "manifest-tamper-swapped-file-sha256",
            "RAO-TV-MANIFEST",
            "ManifestDigestMismatch",
            "external manifest anchor rejects swapped file_sha256 values with constant payloads",
            swap_file_sha256,
            external_anchor=True,
        )[0],
        build_case(
            "manifest-tamper-altered-first-chunk-lba",
            "RAO-TV-MANIFEST",
            "ManifestDigestMismatch",
            "external manifest anchor rejects altered first_chunk_lba with constant payloads",
            alter_first_chunk_lba,
            external_anchor=True,
        )[0],
    }
    if len(tamper_digests) != 3:
        raise AssertionError("manifest tamper vectors must have distinct plaintext digests")
    return records


def generate_rao_envelope_negatives(rao_root: pathlib.Path) -> list[dict[str, Any]]:
    """Materialize RAO 2.0 discriminator and key-frame-bound failures."""
    base = rao_root / "objects" / "rao-tv-e2.rao"
    base_bytes = base.read_bytes()
    base_digest = hashlib.sha256(base_bytes).hexdigest()
    if base_bytes[0x38] != independent.RAO_WRAP_SUITE_XWING:
        raise AssertionError("RAO-TV-E2 is not an X-Wing envelope")

    records: list[dict[str, Any]] = []
    cases = (
        (
            "legacy-x25519-wrap-suite",
            "InvalidWrapSuite",
            "set wrap_suite to the permanently reserved X25519-only value 0x01",
            0x38,
            bytes([independent.RAO_WRAP_SUITE_X25519]),
            {"offset": 0x38, "value": independent.RAO_WRAP_SUITE_X25519},
        ),
        (
            "key-frame-len-below-minimum",
            "InvalidKeyFrameLength",
            "set key_frame_len to 1190, below the inclusive RAO 2.0 minimum 1191",
            0x3C,
            (independent.RAO_KEY_FRAME_MIN_LEN - 1).to_bytes(4, "big"),
            {
                "offset": 0x3C,
                "value": independent.RAO_KEY_FRAME_MIN_LEN - 1,
                "encoding": "u32be",
            },
        ),
        (
            "key-frame-len-above-maximum",
            "InvalidKeyFrameLength",
            "set key_frame_len to 16385, above the inclusive RAO 2.0 maximum 16384",
            0x3C,
            (independent.RAO_KEY_FRAME_MAX_LEN + 1).to_bytes(4, "big"),
            {
                "offset": 0x3C,
                "value": independent.RAO_KEY_FRAME_MAX_LEN + 1,
                "encoding": "u32be",
            },
        ),
    )
    for vector_id, expected_error, assertion, offset, replacement, mutation in cases:
        faulted = bytearray(base_bytes)
        faulted[offset : offset + len(replacement)] = replacement
        if faulted == base_bytes:
            raise AssertionError(f"{vector_id} did not change its base object")
        directory = rao_root / "negative" / "envelope" / vector_id
        directory.mkdir(parents=True, exist_ok=True)
        (directory / "faulted-object.rao").write_bytes(faulted)
        write_json(
            directory / "input.json",
            {
                "base_artifact": "objects/rao-tv-e2.rao",
                "base_sha256": base_digest,
                "mutation": mutation,
            },
        )
        write_json(
            directory / "expected.json",
            {
                "vector_id": vector_id,
                "category": "envelope",
                "expected_error": expected_error,
                "assertion": assertion,
                "faulted_sha256": hashlib.sha256(faulted).hexdigest(),
            },
        )
        records.append(
            {
                "id": vector_id,
                "category": "negative/envelope",
                "path": directory,
            }
        )
    return records


def generate_rao_range_vectors(rao_root: pathlib.Path) -> list[dict[str, Any]]:
    """Describe the authenticated range that covers D1's true final object chunk."""
    fixture = json.loads(
        (rao_root / "manifests" / "rao-tv-d1.json").read_text(encoding="utf-8")
    )
    plaintext = fixture["expected"]["plaintext"]
    encrypted = fixture["expected"]["encrypted"]
    manifest = plaintext["manifest_layout"]
    object_path = rao_root / "objects" / "rao-tv-d1-encrypted.rao"
    chunk_size = fixture["inputs"]["chunk_size"]
    object_chunk_count = encrypted["chunk_count"]
    first_chunk = manifest["first_chunk_lba"]
    if first_chunk != object_chunk_count - 1:
        raise AssertionError("RAO-TV-D1 manifest is not in the final object chunk")
    stored_range_start = (
        128
        + encrypted["key_frame_len"]
        + encrypted["metadata_frame_len"]
        + first_chunk * (chunk_size + 16)
    )
    source_sha256 = sha256(object_path)
    common_input = {
        "base_artifact": "objects/rao-tv-d1-encrypted.rao",
        "source_sha256": source_sha256,
        "recipient": fixture["inputs"]["recipients"][0],
        "first_inner_chunk": first_chunk,
        "range_start": 0,
        "range_len": plaintext["manifest_cbor_len"],
        "file_chunk_count": manifest["chunk_count"],
        "object_chunk_count": object_chunk_count,
        "stored_range_start": stored_range_start,
        "stored_range_len": chunk_size + 16,
    }
    common_expected = {
        "first_chunk": first_chunk,
        "chunk_count": 1,
        "object_chunk_count": object_chunk_count,
        "manifest_cbor_hex": plaintext["manifest_cbor_hex"],
        "manifest_sha256": plaintext["manifest_sha256"],
        "plaintext_digest": encrypted["plaintext_digest"],
        "source_sha256": source_sha256,
    }
    records: list[dict[str, Any]] = []
    for vector_id, category, final_flag, outcome in (
        (
            "encrypted-last-object-chunk",
            "positive/range",
            True,
            {"expected_outcome": "authenticated-range"},
        ),
        (
            "encrypted-last-object-chunk-wrong-finality",
            "negative/range",
            False,
            {"expected_error": "AeadAuthenticationFailed"},
        ),
    ):
        directory = rao_root / category / vector_id
        semantic_vector(
            directory,
            {
                "input": {
                    **common_input,
                    "final_flag": final_flag,
                },
                "vector_id": vector_id,
                "category": category,
                **outcome,
                **common_expected,
                "assertion": (
                    "chunk index object_chunk_count - 1 authenticates only with final_flag = true"
                ),
            },
        )
        records.append({"id": vector_id, "category": category, "path": directory})
    return records


def generate_negatives(
    rem_root: pathlib.Path, rao_root: pathlib.Path
) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    negative_root = rem_root / "negative"
    minimal = rem_root / "positive" / "minimal-image"
    external = rem_root / "positive" / "external-parity-map"
    bootstrap = minimal / "tape-file-003-final-bootstrap.bin"
    sidecar = minimal / "tape-file-002-sidecar.bin"
    parity_map = external / "tape-file-003-parity-map.bin"
    records: list[dict[str, Any]] = []
    sidecar_bytes = sidecar.read_bytes()
    sidecar_tail = int.from_bytes(sidecar_bytes[0x70:0x78], "little") * BLOCK_SIZE
    sidecar_footer = int.from_bytes(sidecar_bytes[0x78:0x80], "little") * BLOCK_SIZE

    def repair_bootstrap_header(data: bytearray) -> None:
        data[0x2C:0x34] = crc64_xz(data[:0x2C]).to_bytes(8, "little")

    def repair_sidecar_headers(data: bytearray) -> None:
        for start in (0, sidecar_tail):
            data[start + 0xB0 : start + 0xB8] = crc64_xz(
                data[start : start + 0xB0]
            ).to_bytes(8, "little")
            data[start + BLOCK_SIZE - 8 : start + BLOCK_SIZE] = crc64_xz(
                data[start : start + BLOCK_SIZE - 8]
            ).to_bytes(8, "little")

    def repair_sidecar_block_crcs(data: bytearray) -> None:
        for start in (0, sidecar_tail):
            data[start + BLOCK_SIZE - 8 : start + BLOCK_SIZE] = crc64_xz(
                data[start : start + BLOCK_SIZE - 8]
            ).to_bytes(8, "little")

    def repair_sidecar_footer(data: bytearray) -> None:
        data[sidecar_footer + 0x78 : sidecar_footer + 0x80] = crc64_xz(
            data[sidecar_footer : sidecar_footer + 0x78]
        ).to_bytes(8, "little")

    def repair_parity_map_footer(data: bytearray) -> None:
        footer = len(data) - BLOCK_SIZE
        data[footer + 0xB0 : footer + 0xB8] = crc64_xz(
            data[footer : footer + 0xB0]
        ).to_bytes(8, "little")

    def binary(
        vector_id: str,
        category: str,
        base: pathlib.Path,
        output_name: str,
        mutations: list[tuple[int, int]],
        expected_error: str,
        assertion: str,
        repair: Callable[[bytearray], None] | None = None,
    ) -> None:
        directory = negative_root / category / vector_id
        mutation_file(
            directory,
            base,
            output_name,
            mutations,
            {
                "vector_id": vector_id,
                "category": category,
                "expected_error": expected_error,
                "assertion": assertion,
            },
            repair,
        )
        records.append({"id": vector_id, "category": f"negative/{category}", "path": directory})

    def semantic(
        vector_id: str,
        category: str,
        input_value: dict[str, Any],
        expected_error: str,
        assertion: str,
    ) -> None:
        directory = negative_root / category / vector_id
        expectation_key = "expected_outcome" if expected_error == "recovered" else "expected_error"
        semantic_vector(
            directory,
            {
                "input": input_value,
                "vector_id": vector_id,
                "category": category,
                expectation_key: expected_error,
                "assertion": assertion,
            },
        )
        records.append({"id": vector_id, "category": f"negative/{category}", "path": directory})

    def bootstrap_payload_case(
        vector_id: str,
        category: str,
        base: pathlib.Path,
        mutate: Callable[[dict[int, Any]], None],
        expected_error: str,
        assertion: str,
        mutation_description: str,
    ) -> None:
        directory = negative_root / category / vector_id
        directory.mkdir(parents=True, exist_ok=True)
        (directory / "faulted-bootstrap.bin").write_bytes(
            rewrite_bootstrap_payload(base, mutate)
        )
        write_json(
            directory / "expected.json",
            {
                "vector_id": vector_id,
                "category": category,
                "expected_error": expected_error,
                "assertion": assertion,
                "input": {
                    "base_artifact": base.name,
                    "payload_mutation": mutation_description,
                },
            },
        )
        records.append(
            {
                "id": vector_id,
                "category": f"negative/{category}",
                "path": directory,
            }
        )

    binary("bootstrap-bad-magic", "bootstrap", bootstrap, "faulted-bootstrap.bin", [(0x00, 0x01)], "BootstrapParse", "magic mismatch")
    binary("bootstrap-schema-major-2", "bootstrap", bootstrap, "faulted-bootstrap.bin", [(0x09, 0x03)], "BootstrapParse", "unsupported schema_major = 2", repair_bootstrap_header)
    binary("bootstrap-header-crc-bit-flip", "bootstrap", bootstrap, "faulted-bootstrap.bin", [(0x2C, 0x01)], "BootstrapParse", "header CRC mismatch")
    bootstrap_payload_len = int.from_bytes(bootstrap.read_bytes()[0x28:0x2C], "little")
    binary("bootstrap-payload-crc-bit-flip", "bootstrap", bootstrap, "faulted-bootstrap.bin", [(0x34 + bootstrap_payload_len, 0x01)], "BootstrapParse", "payload CRC mismatch")
    directory = negative_root / "bootstrap" / "bootstrap-payload-truncation"
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "faulted-bootstrap.bin").write_bytes(bootstrap.read_bytes()[:60])
    write_json(directory / "expected.json", {"vector_id": "bootstrap-payload-truncation", "category": "bootstrap", "expected_error": "BootstrapParse", "assertion": "declared payload extends past supplied bytes", "input": {"base_artifact": bootstrap.name, "truncate_to_bytes": 60}})
    records.append({"id": "bootstrap-payload-truncation", "category": "negative/bootstrap", "path": directory})
    semantic("bootstrap-inline-and-external-directory", "bootstrap", {"sidecar_epoch_directory": "minimal-image directory", "parity_map_reference": "external-parity-map reference"}, "BootstrapParse", "CBOR keys 20 and 21 are mutually exclusive")
    semantic("bootstrap-drive-compression-enabled", "bootstrap", {"base": bootstrap.name, "drive_compression": True, "parity_enabled": True}, "DriveCompressionEnabled", "parity bootstrap records drive_compression = true")
    semantic("bootstrap-oversize-payload", "bootstrap", {"block_size": 64, "written_by_version": "x" * 128}, "BootstrapPayloadTooLarge", "framed bootstrap cannot fit declared block")
    object_id_bootstrap = (
        rem_root
        / "positive"
        / "object-id-36-bootstrap"
        / "tape-file-000-bootstrap.bin"
    )

    def object_id_65(payload: dict[int, Any]) -> None:
        payload[30][0][4] = b"x" * 65

    bootstrap_payload_case(
        "bootstrap-object-id-65",
        "bootstrap",
        object_id_bootstrap,
        object_id_65,
        "BootstrapParse",
        "object row object_id must contain 1..=64 non-NUL bytes",
        "replace the 36-byte object-row key 4 with 65 non-NUL bytes",
    )

    def directory_rows(payload: dict[int, Any]) -> tuple[dict[int, Any], dict[int, Any]]:
        directory = payload[20]
        first = copy.deepcopy(directory[5][0])
        second = copy.deepcopy(first)
        first[1] = 2
        second[1] = 3
        directory[5] = [first, second]
        return first, second

    def directory_overlap(payload: dict[int, Any]) -> None:
        first, second = directory_rows(payload)
        first[3], first[4] = 0, 3
        second[2], second[3], second[4] = 1, 2, 4

    def directory_gap(payload: dict[int, Any]) -> None:
        first, second = directory_rows(payload)
        first[3], first[4] = 0, 1
        second[2], second[3], second[4] = 1, 2, 4

    def directory_duplicate_epoch(payload: dict[int, Any]) -> None:
        first, second = directory_rows(payload)
        first[3], first[4] = 0, 2
        second[2], second[3], second[4] = 0, 2, 4

    def directory_nonzero_first(payload: dict[int, Any]) -> None:
        first, second = directory_rows(payload)
        first[3], first[4] = 1, 2
        second[2], second[3], second[4] = 1, 2, 4

    for vector_id, mutate, assertion in (
        (
            "directory-overlapping-ranges",
            directory_overlap,
            "second protected range starts before the preceding range ends",
        ),
        (
            "directory-gapped-ranges",
            directory_gap,
            "second protected range starts after the preceding range ends",
        ),
        (
            "directory-duplicate-epoch",
            directory_duplicate_epoch,
            "epoch ids are not unique and consecutive from zero",
        ),
        (
            "directory-nonzero-first-start",
            directory_nonzero_first,
            "first protected range does not start at zero",
        ),
    ):
        bootstrap_payload_case(
            vector_id,
            "directory",
            bootstrap,
            mutate,
            "DirectoryInvalid",
            assertion,
            assertion,
        )

    header_faults = [
        ("sidecar-magic", 0x00, "derived magic mismatch"),
        ("sidecar-tape-uuid", 0x08, "tape_uuid mismatch"),
        ("sidecar-k-zero", 0x20, "k must be nonzero"),
        ("sidecar-m-zero", 0x22, "m must be nonzero"),
        ("sidecar-s-zero", 0x24, "S must be nonzero"),
        ("sidecar-block-size", 0x28, "declared block_size must equal actual block size"),
        ("sidecar-schema-version", 0x2C, "schema_version must be 1"),
        ("sidecar-end-not-after-start", 0x38, "protected end must exceed start"),
        ("sidecar-logical-shard-count", 0x40, "logical_shard_count must equal S times k"),
        ("sidecar-real-data-shard-count", 0x48, "real count must equal end minus start and not exceed logical count"),
        ("sidecar-parity-block-count", 0x50, "parity count must equal S times m"),
        ("sidecar-data-crc-count", 0x54, "data CRC count must equal real count"),
        ("sidecar-header-block-count", 0x58, "H must equal recomputed layout"),
        ("sidecar-inline-index-bytes", 0x5C, "inline bytes must equal recomputed layout"),
        ("sidecar-total-block-count", 0x60, "total must equal 2H + P + 1"),
        ("sidecar-primary-start", 0x68, "primary start must be zero"),
        ("sidecar-tail-start", 0x70, "tail start must equal H + P"),
        ("sidecar-footer-index", 0x78, "footer index must equal 2H + P"),
        ("sidecar-copy-kind", 0x80, "copy kind must be primary or tail"),
        ("sidecar-copy-kind-reserved", 0x82, "copy-kind reserved field must be zero"),
        ("sidecar-copy-generation", 0x84, "copy generation must be zero"),
        ("sidecar-canonical-hash", 0x88, "canonical metadata hash mismatch"),
        ("sidecar-header-reserved", 0xA8, "header reserved field must be zero"),
        ("sidecar-header-crc", 0xB0, "header CRC mismatch"),
        ("sidecar-zero-fill", 0xD8, "header/index fill must be zero"),
        ("sidecar-block0-crc", BLOCK_SIZE - 8, "block0 CRC mismatch"),
    ]
    for vector_id, offset, assertion in header_faults:
        xor_value = {
            "sidecar-k-zero": 0x02,
            "sidecar-m-zero": 0x02,
            "sidecar-s-zero": 0x02,
            "sidecar-end-not-after-start": 0x04,
        }.get(vector_id, 0x01)
        mutations = [(offset, xor_value), (sidecar_tail + offset, xor_value)]
        repair = repair_sidecar_headers
        if vector_id == "sidecar-header-crc":
            repair = None
        elif vector_id == "sidecar-block0-crc":
            repair = None
        elif vector_id == "sidecar-zero-fill":
            mutations = [(0x120, 0x01), (sidecar_tail + 0x120, 0x01)]
            repair = repair_sidecar_block_crcs
        binary(vector_id, "sidecar", sidecar, "faulted-sidecar.bin", mutations, "SidecarParse", assertion, repair)
    semantic("sidecar-epoch-start", "sidecar", {"epoch_id": 1, "S": 2, "k": 2, "protected_ordinal_start": 0}, "SidecarParse", "protected start must equal epoch_id times S times k")
    semantic("sidecar-index-entry-straddle", "sidecar", {"block_size": 192, "entry_offset": 177, "entry_length": 16, "usable_limit": 184}, "SidecarParse", "index entry straddles usable area")
    semantic("sidecar-spill-block-crc", "sidecar", {"base": sidecar.name, "spill_block_index": 1, "trailing_crc_xor": 1}, "SidecarParse", "spill-block CRC mismatch")
    binary("sidecar-index-reserved", "sidecar", sidecar, "faulted-sidecar.bin", [(0xB8 + 6, 0x01), (sidecar_tail + 0xB8 + 6, 0x01)], "SidecarParse", "parity index reserved field nonzero", repair_sidecar_block_crcs)
    semantic("sidecar-primary-tail-disagreement", "sidecar", {"base": sidecar.name, "primary_and_tail": "individually valid but unequal index streams"}, "SidecarParse", "both valid metadata copies must agree")
    binary("sidecar-footer-total-disagreement", "sidecar", sidecar, "faulted-sidecar.bin", [(sidecar_footer + 0x40, 0x01)], "SidecarParse", "footer total disagrees with header/map entry", repair_sidecar_footer)

    binary("parity-map-payload-sha256", "parity-map", parity_map, "faulted-parity-map.bin", [(0xB8, 0x01), (BLOCK_SIZE + 0xB8, 0x01)], "ParityMapParse", "payload SHA-256 mismatch in both copies")
    binary("parity-map-locator-header-disagreement", "parity-map", parity_map, "faulted-parity-map.bin", [(2 * BLOCK_SIZE + 0x90, 0x01)], "ParityMapParse", "footer locator and copy header disagree", repair_parity_map_footer)
    semantic("parity-map-directory-unknown-flag", "parity-map", {"directory_entry_flags": 8}, "ParityMapParse", "unknown directory flag bit")
    semantic("parity-map-directory-nonascending", "parity-map", {"tape_file_numbers": [4, 2]}, "ParityMapParse", "directory rows are not strictly ascending")
    semantic("parity-map-directory-watermark", "parity-map", {"entry_end": 4, "directory_scope_highest_protected_ordinal": 3}, "ParityMapParse", "directory watermark differs from maximum entry end")

    for scalar in ("tape_file_count", "map_total_data_ordinals", "highest_protected_ordinal"):
        semantic(f"digest-{scalar.replace('_', '-')}", "digest", {"base": "positive/minimal-image", "flipped_scalar": scalar, "delta": 1}, "FilemarkMapDigestMismatch", f"canonical map scope scalar {scalar} differs")

    semantic("recovery-m-plus-one-erasures", "recovery", {"base": "positive/minimal-image", "failed_ordinal": 0, "erased_same_stripe_positions": ["data:0", "data:1", "parity:0"], "m": 2}, "Unrecoverable", "lost_count = 3 and limit = 2")
    semantic("recovery-corrupt-peer-as-erasure", "recovery", {"base": "positive/minimal-image", "failed_ordinal": 0, "corrupt_peer": "data:1"}, "recovered", "CRC-bad peer is counted as an erasure and recovery succeeds")
    semantic("recovery-reconstructed-crc-mismatch", "recovery", {"base": "positive/minimal-image", "failed_ordinal": 0, "expected_data_crc_xor": 1}, "Unrecoverable", "reconstructed bytes fail the pinned data CRC")
    semantic("recovery-pending-epoch", "recovery", {"failed_ordinal": 4, "highest_protected_ordinal": 4}, "UnrecoverablePendingEpoch", "ordinal at the protection watermark is refused before I/O")
    semantic("recovery-outside-prefix", "recovery", {"failed_ordinal": 4, "validated_prefix_ordinals": 4}, "OutsideValidatedMapPrefix", "ordinal outside authenticated prefix is refused before I/O")
    return records, generate_rao_negatives(rao_root)


def generate_damage_matrix(rem_root: pathlib.Path) -> list[dict[str, Any]]:
    damage_root = rem_root / "damage-matrix"
    minimal = rem_root / "positive" / "minimal-image"
    external = rem_root / "positive" / "external-parity-map"
    records: list[dict[str, Any]] = []
    recovered_block_sha256 = hashlib.sha256(
        (minimal / "tape-file-001-object.bin").read_bytes()[:BLOCK_SIZE]
    ).hexdigest()

    cells = [
        ("object-head", minimal / "tape-file-001-object.bin", [0], "recovered", "object block reconstructed from sidecar"),
        ("sidecar-primary-header", minimal / "tape-file-002-sidecar.bin", [0], "copy-health-downgrade", "tail metadata copy remains usable; recovery succeeds"),
        ("sidecar-footer", minimal / "tape-file-002-sidecar.bin", [6], "copy-health-downgrade", "inline directory locates a valid metadata copy; recovery succeeds"),
        ("sidecar-footer-and-primary", minimal / "tape-file-002-sidecar.bin", [0, 6], "copy-health-downgrade", "directory-assisted tail rescue succeeds"),
        ("parity-map-primary", external / "tape-file-003-parity-map.bin", [0], "copy-health-downgrade", "tail parity_map copy is selected; protected epoch remains available"),
        ("bootstrap-copy", minimal / "tape-file-000-bootstrap.bin", [0], "recovered", "unreadable in-scope bootstrap is re-typed and the later final bootstrap validates the map"),
    ]
    for vector_id, base, unreadable_blocks, outcome, assertion in cells:
        directory = damage_root / vector_id
        directory.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(base, directory / "source-artifact.bin")
        write_json(
            directory / "fault-map.json",
            {
                "fault_model": "transport-medium-error",
                "block_size": BLOCK_SIZE,
                "unreadable_block_indices": unreadable_blocks,
            },
        )
        write_json(
            directory / "expected.json",
            {
                "vector_id": vector_id,
                "category": "damage-matrix",
                "expected_outcome": outcome,
                "assertion": assertion,
                "whole_tape_failure": False,
                **(
                    {
                        "recovery_target_ordinal": 0,
                        "recovered_block_sha256": recovered_block_sha256,
                    }
                    if vector_id.startswith("object-") or vector_id.startswith("sidecar-")
                    else {}
                ),
            },
        )
        records.append({"id": vector_id, "category": "damage-matrix", "path": directory})

    def add_boundary_burst(
        vector_id: str,
        image_id: str,
        burst_span_records: int,
        expected_outcome: str,
        span_formula: str,
    ) -> None:
        image = rem_root / "positive" / image_id
        object_path = image / "tape-file-001-object.bin"
        sidecar_path = image / "tape-file-002-sidecar.bin"
        object_bytes = object_path.read_bytes()
        sidecar_bytes = sidecar_path.read_bytes()
        if len(object_bytes) % BLOCK_SIZE or len(sidecar_bytes) % BLOCK_SIZE:
            raise AssertionError(f"{image_id} burst source is not block aligned")
        real_data_blocks = len(object_bytes) // BLOCK_SIZE
        header_blocks = int.from_bytes(sidecar_bytes[0x58:0x5C], "little")
        parity_blocks = int.from_bytes(sidecar_bytes[0x50:0x54], "little")
        m = int.from_bytes(sidecar_bytes[0x22:0x24], "little")
        stripes = int.from_bytes(sidecar_bytes[0x24:0x28], "little")
        if parity_blocks != m * stripes:
            raise AssertionError(f"{image_id} parity geometry is inconsistent")
        target_ordinal = real_data_blocks - 1
        target_stripe = target_ordinal % stripes
        damaged_sidecar_blocks = burst_span_records - 2
        if damaged_sidecar_blocks <= header_blocks:
            raise AssertionError(f"{vector_id} does not reach parity blocks")
        damaged_parity_blocks = damaged_sidecar_blocks - header_blocks
        parity_losses = sum(
            index % stripes == target_stripe
            for index in range(damaged_parity_blocks)
        )
        lost_count = 1 + parity_losses
        directory = damage_root / vector_id
        directory.mkdir(parents=True, exist_ok=True)
        (directory / "source-artifact.bin").write_bytes(object_bytes + sidecar_bytes)
        write_json(
            directory / "tape-layout.json",
            {
                "tape_files": [
                    {
                        "tape_file_number": 1,
                        "artifact": object_path.name,
                        "concatenated_start_block": 0,
                        "block_count": real_data_blocks,
                        "trailing_filemark": True,
                    },
                    {
                        "tape_file_number": 2,
                        "artifact": sidecar_path.name,
                        "concatenated_start_block": real_data_blocks,
                        "block_count": len(sidecar_bytes) // BLOCK_SIZE,
                        "trailing_filemark": True,
                    },
                ]
            },
        )
        unreadable_blocks = [target_ordinal] + list(
            range(
                real_data_blocks,
                real_data_blocks + damaged_sidecar_blocks,
            )
        )
        write_json(
            directory / "fault-map.json",
            {
                "fault_model": "transport-medium-error",
                "block_size": BLOCK_SIZE,
                "burst_start_record": target_ordinal,
                "burst_span_records": burst_span_records,
                "burst_includes_filemark_after_tape_file": 1,
                "unreadable_block_indices": unreadable_blocks,
                "unreadable_filemarks": [
                    {
                        "after_tape_file_number": 1,
                        "logical_record_index": real_data_blocks,
                    }
                ],
            },
        )
        write_json(
            directory / "expected.json",
            {
                "vector_id": vector_id,
                "category": "damage-matrix",
                "expected_outcome": expected_outcome,
                "assertion": (
                    f"{span_formula}; stripe {target_stripe} loses "
                    f"{lost_count} shards with limit {m}"
                ),
                "whole_tape_failure": False,
                "geometry": {
                    "m": m,
                    "S": stripes,
                    "H": header_blocks,
                    "R": real_data_blocks,
                },
                "burst_span_records": burst_span_records,
                "recovery_target_ordinal": target_ordinal,
                "straddled_stripe": target_stripe,
                "lost_count": lost_count,
                "limit": m,
                "recovered_block_sha256": hashlib.sha256(
                    object_bytes[
                        target_ordinal
                        * BLOCK_SIZE : (target_ordinal + 1)
                        * BLOCK_SIZE
                    ]
                ).hexdigest(),
            },
        )
        records.append(
            {"id": vector_id, "category": "damage-matrix", "path": directory}
        )

    full_sidecar = (
        rem_root / "positive" / "minimal-image" / "tape-file-002-sidecar.bin"
    ).read_bytes()
    full_h = int.from_bytes(full_sidecar[0x58:0x5C], "little")
    full_m = int.from_bytes(full_sidecar[0x22:0x24], "little")
    full_s = int.from_bytes(full_sidecar[0x24:0x28], "little")
    add_boundary_burst(
        "boundary-straddling-burst-m-limit",
        "minimal-image",
        full_m * full_s + full_h + 1,
        "recovered",
        "m*S + H + 1 boundary-straddling records are fully recovered",
    )
    add_boundary_burst(
        "boundary-straddling-burst-m-plus-one",
        "minimal-image",
        full_m * full_s + full_h + 2,
        "Unrecoverable",
        "m*S + H + 2 boundary-straddling records exceed one stripe's limit",
    )
    short_sidecar = (
        rem_root
        / "positive"
        / "short-epoch-r-less-than-s"
        / "tape-file-002-sidecar.bin"
    ).read_bytes()
    short_h = int.from_bytes(short_sidecar[0x58:0x5C], "little")
    short_m = int.from_bytes(short_sidecar[0x22:0x24], "little")
    short_s = int.from_bytes(short_sidecar[0x24:0x28], "little")
    short_r = (
        rem_root
        / "positive"
        / "short-epoch-r-less-than-s"
        / "tape-file-001-object.bin"
    ).stat().st_size // BLOCK_SIZE
    if short_r >= short_s:
        raise AssertionError("short-epoch damage vector requires R < S")
    add_boundary_burst(
        "short-epoch-boundary-burst-unrecoverable",
        "short-epoch-r-less-than-s",
        (short_m - 1) * short_s + short_r + short_h + 2,
        "Unrecoverable",
        "(m-1)*S + R + H + 2 short-epoch boundary records exceed one stripe's limit",
    )

    source_directory = (
        rem_root / "generated-sources" / "multi-parity-map-selection"
    )
    selection = json.loads(
        (source_directory / "selection.json").read_text(encoding="utf-8")
    )
    vector_id = "multi-parity-map-selection"
    directory = damage_root / vector_id
    directory.mkdir(parents=True, exist_ok=True)
    tape_files = sorted(source_directory.glob("tape-file-*.bin"))
    layout = []
    source_bytes = bytearray()
    for tape_file_number, source in enumerate(tape_files):
        data = source.read_bytes()
        if len(data) % BLOCK_SIZE != 0:
            raise AssertionError(f"{source.name} is not block aligned")
        start_block = len(source_bytes) // BLOCK_SIZE
        block_count = len(data) // BLOCK_SIZE
        layout.append(
            {
                "tape_file_number": tape_file_number,
                "artifact": source.name,
                "concatenated_start_block": start_block,
                "block_count": block_count,
                "trailing_filemark": True,
            }
        )
        shutil.copyfile(source, directory / source.name)
        source_bytes.extend(data)
    damaged_bootstrap = selection["damaged_referencing_bootstrap"]
    bootstrap_layout = layout[damaged_bootstrap["tape_file_number"]]
    unreadable_block = (
        bootstrap_layout["concatenated_start_block"]
        + damaged_bootstrap["block_index"]
    )
    (directory / "source-artifact.bin").write_bytes(source_bytes)
    write_json(directory / "tape-layout.json", {"tape_files": layout})
    write_json(
        directory / "fault-map.json",
        {
            "fault_model": "transport-medium-error",
            "block_size": BLOCK_SIZE,
            "unreadable_block_indices": [unreadable_block],
            "unreadable_tape_records": [
                {
                    **damaged_bootstrap,
                    "concatenated_block_index": unreadable_block,
                }
            ],
        },
    )
    write_json(
        directory / "expected.json",
        {
            "vector_id": vector_id,
            "category": "damage-matrix",
            "expected_outcome": "structural-parity-map-selected",
            "assertion": "ranking selects sequence 7, tape_file_number 4 wins the equal-key tie with a conflict report, and candidate overlay re-types the damaged referencing bootstrap before digest validation",
            "whole_tape_failure": False,
            "no_usable_bootstrap_directory": True,
            **selection,
        },
    )
    records.append(
        {"id": vector_id, "category": "damage-matrix", "path": directory}
    )
    shutil.rmtree(rem_root / "generated-sources")
    return records


def verify_existing_parity_pins(rem_root: pathlib.Path) -> None:
    """Allow only the B1 parity-block permutation in previously pinned images."""
    allow_b1_transition = sha256(OUTPUT) == PRE_B1_ARCHIVE_SHA256
    with tarfile.open(OUTPUT, mode="r") as archive:
        members: dict[str, bytes] = {}
        for member in archive.getmembers():
            if not member.isfile():
                continue
            extracted = archive.extractfile(member)
            if extracted is not None:
                members[member.name] = extracted.read()
        sidecar_geometries: dict[int, tuple[int, int, int, int]] = {}
        for name, pinned_bytes in members.items():
            if (
                name.startswith("rem-parity-1/positive/")
                and "sidecar" in pathlib.PurePosixPath(name).name
            ):
                header_blocks = int.from_bytes(
                    pinned_bytes[0x58:0x5C], "little"
                )
                parity_blocks = int.from_bytes(
                    pinned_bytes[0x50:0x54], "little"
                )
                m = int.from_bytes(pinned_bytes[0x22:0x24], "little")
                stripes = int.from_bytes(pinned_bytes[0x24:0x28], "little")
                sidecar_geometries[len(pinned_bytes)] = (
                    header_blocks,
                    parity_blocks,
                    m,
                    stripes,
                )

        for member in archive.getmembers():
            prefix = "rem-parity-1/"
            if (
                not member.isfile()
                or not member.name.startswith(prefix)
                or member.name == f"{prefix}vectors.json"
            ):
                continue
            relative = member.name.removeprefix(prefix)
            generated = rem_root / relative
            if not generated.is_file():
                raise AssertionError(
                    f"previously published parity artifact is absent: {member.name}"
                )
            pinned = archive.extractfile(member)
            if pinned is None:
                raise AssertionError(f"cannot read pinned parity artifact: {member.name}")
            old_bytes = pinned.read()
            new_bytes = generated.read_bytes()
            if new_bytes == old_bytes:
                continue
            if not allow_b1_transition:
                raise AssertionError(
                    f"previously published parity artifact changed: {member.name}"
                )
            basename = pathlib.PurePosixPath(member.name).name
            if (
                "/damage-matrix/multi-parity-map-selection/" in member.name
                and (
                    basename.endswith(".bin")
                    or basename == "expected.json"
                )
            ):
                # C9 makes the old synthetic epoch ids 11/22 invalid. The
                # Rust generator now emits valid epoch 0 rows while preserving
                # the selection/ranking scenario.
                continue
            is_sidecar_image = (
                "sidecar" in basename
                or (
                    basename == "source-artifact.bin"
                    and "/damage-matrix/sidecar-" in member.name
                )
            )
            geometry = sidecar_geometries.get(len(old_bytes))
            if not is_sidecar_image or geometry is None:
                raise AssertionError(
                    f"previously published parity artifact changed: {member.name}"
                )
            header_blocks, parity_blocks, m, stripes = geometry
            if parity_blocks != m * stripes:
                raise AssertionError(
                    f"pinned sidecar geometry is inconsistent: {member.name}"
                )
            parity_start = header_blocks * BLOCK_SIZE
            parity_end = parity_start + parity_blocks * BLOCK_SIZE
            if (
                old_bytes[:parity_start] != new_bytes[:parity_start]
                or old_bytes[parity_end:] != new_bytes[parity_end:]
            ):
                raise AssertionError(
                    "B1 changed sidecar metadata/footer bytes instead of only parity blocks: "
                    f"{member.name}"
                )
            for parity_index in range(m):
                for stripe in range(stripes):
                    old_index = stripe * m + parity_index
                    new_index = parity_index * stripes + stripe
                    old_start = parity_start + old_index * BLOCK_SIZE
                    new_start = parity_start + new_index * BLOCK_SIZE
                    if (
                        old_bytes[old_start : old_start + BLOCK_SIZE]
                        != new_bytes[new_start : new_start + BLOCK_SIZE]
                    ):
                        raise AssertionError(
                            "B1 sidecar bytes are not the exact stripe-major to "
                            f"parity-index-major permutation: {member.name}"
                        )


def build_rao_vector_index(
    rao_root: pathlib.Path,
    negative_records: list[dict[str, Any]],
    range_records: list[dict[str, Any]],
) -> None:
    """Index the additive RAO metadata positives and executable negatives."""
    indexed: list[dict[str, Any]] = []
    for object_name, fixture_name in sorted(RAO_INCREMENT_FIXTURES.items()):
        object_path = rao_root / "objects" / object_name
        fixture_path = rao_root / "manifests" / fixture_name
        fixture = json.loads(fixture_path.read_text(encoding="utf-8"))
        expected = fixture["expected"]
        artifacts = sorted(
            [
                {
                    "path": object_path.relative_to(rao_root).as_posix(),
                    "size": object_path.stat().st_size,
                    "sha256": sha256(object_path),
                },
                {
                    "path": fixture_path.relative_to(rao_root).as_posix(),
                    "size": fixture_path.stat().st_size,
                    "sha256": sha256(fixture_path),
                },
            ],
            key=lambda item: item["path"],
        )
        canonical = "".join(
            f"{item['sha256']}  {item['path']}\n" for item in artifacts
        ).encode("utf-8")
        if expected["full_object_sha256"] != sha256(object_path):
            raise AssertionError(f"{object_name} fixture does not pin its object bytes")
        indexed.append(
            {
                "id": fixture["vector_id"],
                "category": "positive",
                "archive_path": object_path.relative_to(rao_root).as_posix(),
                "checksum_sha256": hashlib.sha256(canonical).hexdigest(),
                "artifacts": artifacts,
                "full_object_sha256": expected["full_object_sha256"],
                "plaintext_digest": expected["plaintext_digest"],
                "first_block_sha256": expected["first_block_sha256"],
                "manifest_sha256": expected["manifest_sha256"],
                "object_metadata": expected["object_metadata"],
            }
        )
    for record in sorted(negative_records, key=lambda item: item["id"]):
        checksum, artifacts = artifact_checksum(record["path"])
        expected = json.loads(
            (record["path"] / "expected.json").read_text(encoding="utf-8")
        )
        item = {
            "id": record["id"],
            "category": record["category"],
            "archive_path": record["path"].relative_to(rao_root).as_posix(),
            "checksum_sha256": checksum,
            "artifacts": artifacts,
            "expected_error": expected["expected_error"],
        }
        for field in (
            "plaintext_digest",
            "stored_digest",
            "manifest_sha256",
            "faulted_sha256",
        ):
            if field in expected:
                item[field] = expected[field]
        indexed.append(item)
    for record in sorted(range_records, key=lambda item: item["id"]):
        checksum, artifacts = artifact_checksum(record["path"])
        expected = json.loads(
            (record["path"] / "expected.json").read_text(encoding="utf-8")
        )
        indexed.append(
            {
                "id": record["id"],
                "category": record["category"],
                "archive_path": record["path"].relative_to(rao_root).as_posix(),
                "checksum_sha256": checksum,
                "artifacts": artifacts,
                **(
                    {"expected_outcome": expected["expected_outcome"]}
                    if "expected_outcome" in expected
                    else {"expected_error": expected["expected_error"]}
                ),
                "first_chunk": expected["first_chunk"],
                "chunk_count": expected["chunk_count"],
                "object_chunk_count": expected["object_chunk_count"],
                "manifest_sha256": expected["manifest_sha256"],
                "plaintext_digest": expected["plaintext_digest"],
                "source_sha256": expected["source_sha256"],
            }
        )
    write_json(
        rao_root / "vectors.json",
        {
            "vector_set": "RAO-2.0-PUBLICATION-INCREMENT",
            "spec_section": "13.1 and 13.6",
            "status": "complete-standalone-distribution",
            "checksum_definition": "SHA-256 of sorted '<artifact-sha256>  <relative-path>\\n' records within each vector",
            "vectors": indexed,
        },
    )


def build_vector_index(
    rem_root: pathlib.Path,
    records: list[dict[str, Any]],
    rao_root: pathlib.Path,
    rao_negative_records: list[dict[str, Any]],
    rao_range_records: list[dict[str, Any]],
) -> None:
    arithmetic = json.loads((rem_root / "vectors.json").read_text(encoding="utf-8"))["arithmetic"]
    indexed = []
    for record in sorted(records, key=lambda item: (item["category"], item["id"])):
        checksum, artifacts = artifact_checksum(record["path"])
        indexed.append(
            {
                "id": record["id"],
                "category": record["category"],
                "archive_path": record["path"].relative_to(rem_root).as_posix(),
                "checksum_sha256": checksum,
                "artifacts": artifacts,
            }
        )
    write_json(
        rem_root / "vectors.json",
        {
            "vector_set": "REM-PARITY-1.0",
            "spec_section": "17",
            "status": "complete-standalone-distribution",
            "checksum_definition": "SHA-256 of sorted '<artifact-sha256>  <relative-path>\\n' records within the vector directory",
            "arithmetic": arithmetic,
            "vectors": indexed,
        },
    )
    build_rao_vector_index(rao_root, rao_negative_records, rao_range_records)


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--stage-directory",
        type=pathlib.Path,
        help="copy the verified tree here and do not regenerate the pinned tar",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    with tempfile.TemporaryDirectory(prefix="remanence-publication-vectors-") as tmp_name:
        temporary_root = pathlib.Path(tmp_name)
        stage = pathlib.Path(tmp_name) / "remanence-test-vectors"
        (stage / "rao" / "manifests").mkdir(parents=True)
        (stage / "rao" / "objects").mkdir(parents=True)
        stage_rao_kats(stage / "rao" / "kats")
        rem_root = stage / "rem-parity-1"
        rem_root.mkdir(parents=True)

        for source in sorted((ROOT / "fixtures" / "rao").glob("*")):
            if source.is_file():
                shutil.copyfile(source, stage / "rao" / "manifests" / source.name)
        for source in sorted((ROOT / "fixtures" / "rem-parity-1").glob("*")):
            if source.is_file():
                shutil.copyfile(source, rem_root / source.name)

        rao_objects_first = temporary_root / "rao-objects-first"
        rao_objects_second = temporary_root / "rao-objects-second"
        generate_rao_objects(rao_objects_first)
        generate_rao_objects(rao_objects_second)
        verify_regenerated_rao_objects(rao_objects_first, rao_objects_second)
        independent.write_current_xwing_fixture_pins(
            stage / "rao" / "manifests",
            rao_objects_first,
        )
        first_envelope_pins = {
            filename: (stage / "rao" / "manifests" / filename).read_bytes()
            for filename in ("rao-tv-e2.json", "rao-tv-d1.json")
        }
        independent.write_current_xwing_fixture_pins(
            stage / "rao" / "manifests",
            rao_objects_first,
        )
        for filename, first_pin in first_envelope_pins.items():
            if (stage / "rao" / "manifests" / filename).read_bytes() != first_pin:
                raise AssertionError(
                    f"{filename} differs across independent pinning runs"
                )

        subprocess.run(
            [
                sys.executable,
                str(ROOT / "tools" / "verify_rao_vectors_independent.py"),
                "--export-directory",
                str(stage / "rao" / "objects"),
                "--fixture-directory",
                str(stage / "rao" / "manifests"),
                "--encrypted-object-directory",
                str(rao_objects_first),
                "--kat-directory",
                str(stage / "rao" / "kats"),
                "--rust-object-directory",
                str(rao_objects_first),
            ],
            cwd=ROOT,
            check=True,
        )
        subprocess.run(
            [
                "cargo",
                "run",
                "--quiet",
                "-p",
                "remanence-parity",
                "--example",
                "generate_publication_vectors",
                "--",
                str(rem_root),
            ],
            cwd=ROOT,
            check=True,
        )

        records = [
            {"id": directory.name, "category": "positive", "path": directory}
            for directory in sorted((rem_root / "positive").iterdir())
            if directory.is_dir()
        ]
        rem_negative_records, rao_negative_records = generate_negatives(
            rem_root, stage / "rao"
        )
        rao_negative_records.extend(
            generate_rao_envelope_negatives(stage / "rao")
        )
        rao_range_records = generate_rao_range_vectors(stage / "rao")
        records.extend(rem_negative_records)
        records.extend(generate_damage_matrix(rem_root))
        verify_existing_parity_pins(rem_root)
        build_vector_index(
            rem_root,
            records,
            stage / "rao",
            rao_negative_records,
            rao_range_records,
        )

        shutil.copyfile(
            ROOT / "tools" / "verify_publication_test_vectors.py",
            stage / "verify.py",
        )
        (stage / "tools").mkdir()
        shutil.copyfile(
            ROOT / "tools" / "verify_rao_vectors_independent.py",
            stage / "tools" / "verify_rao_vectors_independent.py",
        )
        shutil.copyfile(
            ROOT / "tools" / "requirements-rao-independent.txt",
            stage / "tools" / "requirements-rao-independent.txt",
        )
        claims = (
            "claim\tentrypoint\tartifacts\n"
            "RAO positive byte identity\tpython3 verify.py\trao/objects/*.rao\n"
            "RAO negative conformance\tpython3 verify.py\trao/manifests/negative-*.json\n"
            "RAO X-Wing independent OPEN and KATs\tpython3 tools/verify_rao_vectors_independent.py --fixture-directory rao/manifests --encrypted-object-directory rao/objects --kat-directory rao/kats --publication-root .\trao/kats/*.txt; rao/objects/rao-tv-e2.rao; rao/objects/rao-tv-d1-encrypted.rao\n"
            "RAO envelope discriminator and length negatives\tpython3 verify.py\trao/negative/envelope/*\n"
            "RAO metadata and extension increment\tpython3 verify.py\trao/vectors.json; rao/negative/manifest/*\n"
            "RAO encrypted final-chunk range\tpython3 verify.py\trao/positive/range/*; rao/negative/range/*\n"
            "REM-PARITY positive images\tpython3 verify.py\trem-parity-1/positive/*\n"
            "REM-PARITY negative taxonomy\tpython3 verify.py\trem-parity-1/negative/*/*\n"
            "REM-PARITY damage matrix\tpython3 verify.py\trem-parity-1/damage-matrix/*\n"
        )
        (stage / "CLAIMS_TO_ARTIFACTS.tsv").write_text(claims, encoding="utf-8", newline="\n")

        payload_files = sorted(path for path in stage.rglob("*") if path.is_file())
        manifest = "".join(
            f"{path.relative_to(stage).as_posix()}\t{path.stat().st_size}\n"
            for path in payload_files
        )
        (stage / "MANIFEST.tsv").write_text(manifest, encoding="utf-8", newline="\n")
        checksum_files = sorted(path for path in stage.rglob("*") if path.is_file())
        checksums = "".join(
            f"{sha256(path)}  ./{path.relative_to(stage).as_posix()}\n"
            for path in checksum_files
        )
        (stage / "CHECKSUMS.sha256").write_text(checksums, encoding="utf-8", newline="\n")

        subprocess.run([sys.executable, str(stage / "verify.py"), str(stage)], check=True)
        subprocess.run(
            [
                sys.executable,
                str(stage / "tools" / "verify_rao_vectors_independent.py"),
                "--fixture-directory",
                str(stage / "rao" / "manifests"),
                "--encrypted-object-directory",
                str(stage / "rao" / "objects"),
                "--kat-directory",
                str(stage / "rao" / "kats"),
                "--publication-root",
                str(stage),
            ],
            cwd=ROOT,
            check=True,
        )

        if args.stage_directory is not None:
            shutil.copytree(stage, args.stage_directory)
            checksum, _artifacts = artifact_checksum(args.stage_directory)
            print(f"{checksum}  {args.stage_directory} (verified staging tree; tar unchanged)")
            return 0

        OUTPUT.parent.mkdir(parents=True, exist_ok=True)
        temporary_output = OUTPUT.with_suffix(".tar.tmp")
        with tarfile.open(temporary_output, mode="w", format=tarfile.PAX_FORMAT) as archive:
            add_tree(archive, stage)
        os.replace(temporary_output, OUTPUT)

    print(f"{sha256(OUTPUT)}  {OUTPUT.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
