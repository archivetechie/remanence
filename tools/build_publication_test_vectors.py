#!/usr/bin/env python3
"""Build the deterministic standalone RAO/REM-PARITY publication archive."""

from __future__ import annotations

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


ROOT = pathlib.Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "specs" / "publication" / "remanence-test-vectors.tar"
BLOCK_SIZE = 4096
RAO_V2_ENCRYPTED_OBJECTS = (
    "rao-tv-d1-encrypted.rao",
    "rao-tv-e2.rao",
)


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


def generate_v2_encrypted_objects(output: pathlib.Path) -> None:
    """Regenerate the pinned v2 objects through the Rust deterministic hook."""
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
            "rao_v2_publication_objects_regenerate_byte_exactly",
            "--",
            "--exact",
        ],
        cwd=ROOT,
        env=environment,
        check=True,
    )


def verify_v2_encrypted_regeneration(first: pathlib.Path, second: pathlib.Path) -> None:
    """Require two regenerations and the checked-in pins to be byte-identical."""
    pinned = ROOT / "fixtures" / "rao" / "objects"
    for filename in RAO_V2_ENCRYPTED_OBJECTS:
        first_bytes = (first / filename).read_bytes()
        second_bytes = (second / filename).read_bytes()
        pinned_bytes = (pinned / filename).read_bytes()
        if first_bytes != second_bytes:
            raise AssertionError(f"{filename} differs across deterministic regenerations")
        if first_bytes != pinned_bytes:
            raise AssertionError(f"{filename} differs from its checked-in pin")


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


def semantic_vector(directory: pathlib.Path, metadata: dict[str, Any]) -> None:
    directory.mkdir(parents=True, exist_ok=True)
    write_json(directory / "input.json", metadata.pop("input"))
    write_json(directory / "expected.json", metadata)


def generate_negatives(rem_root: pathlib.Path) -> list[dict[str, Any]]:
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
    return records


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
    return records


def build_vector_index(rem_root: pathlib.Path, records: list[dict[str, Any]]) -> None:
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


def main() -> int:
    with tempfile.TemporaryDirectory(prefix="remanence-publication-vectors-") as tmp_name:
        temporary_root = pathlib.Path(tmp_name)
        stage = pathlib.Path(tmp_name) / "remanence-test-vectors"
        (stage / "rao" / "manifests").mkdir(parents=True)
        (stage / "rao" / "objects").mkdir(parents=True)
        rem_root = stage / "rem-parity-1"
        rem_root.mkdir(parents=True)

        for source in sorted((ROOT / "fixtures" / "rao").glob("*")):
            if source.is_file():
                shutil.copyfile(source, stage / "rao" / "manifests" / source.name)
        for source in sorted((ROOT / "fixtures" / "rem-parity-1").glob("*")):
            if source.is_file():
                shutil.copyfile(source, rem_root / source.name)

        encrypted_first = temporary_root / "rao-v2-encrypted-first"
        encrypted_second = temporary_root / "rao-v2-encrypted-second"
        generate_v2_encrypted_objects(encrypted_first)
        generate_v2_encrypted_objects(encrypted_second)
        verify_v2_encrypted_regeneration(encrypted_first, encrypted_second)

        subprocess.run(
            [
                sys.executable,
                str(ROOT / "tools" / "verify_rao_vectors_independent.py"),
                "--export-directory",
                str(stage / "rao" / "objects"),
                "--encrypted-object-directory",
                str(encrypted_first),
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
        records.extend(generate_negatives(rem_root))
        records.extend(generate_damage_matrix(rem_root))
        build_vector_index(rem_root, records)

        shutil.copyfile(
            ROOT / "tools" / "verify_publication_test_vectors.py",
            stage / "verify.py",
        )
        claims = (
            "claim\tentrypoint\tartifacts\n"
            "RAO positive byte identity\tpython3 verify.py\trao/objects/*.rao\n"
            "RAO negative conformance\tpython3 verify.py\trao/manifests/negative-*.json\n"
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

        OUTPUT.parent.mkdir(parents=True, exist_ok=True)
        temporary_output = OUTPUT.with_suffix(".tar.tmp")
        with tarfile.open(temporary_output, mode="w", format=tarfile.PAX_FORMAT) as archive:
            add_tree(archive, stage)
        os.replace(temporary_output, OUTPUT)

    print(f"{sha256(OUTPUT)}  {OUTPUT.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
