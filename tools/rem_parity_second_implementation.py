#!/usr/bin/env python3
"""Second implementation of REM-PARITY generation 2, written from the text.

This program re-derives the candidate REM-PARITY generation-2 artifacts from
their recorded inputs (``build``) and decides damage cases as a Reader under
the specification text (``decide``). It was written from the REM-PARITY,
REM-OBJECT and REM-ENCRYPT preparing copies alone, without the reference
implementation.

It imports two groups of definitions from other files in this repository:

* from ``tools/rem_parity_rederive.py``, the GF(2^8), Cauchy, Reed-Solomon
  encoding, CRC-64/XZ and deterministic CBOR definitions; and
* from ``tools/verify_rem_object_vectors_independent.py``, the plaintext
  REM-OBJECT builder ``build_plaintext_with_manifest`` and ``FileSpec``.

Each was checked against the specification text; the checks are recorded in
``fixtures/rem-parity-terminal-index-draft/second-implementation/``.

Usage:
    rem_parity_second_implementation.py build [--out DIR]
    rem_parity_second_implementation.py decide CASE.json [CASE.json ...] [--out FILE]

A case file named ``fault-map.json`` takes its case id from its directory.
"""

from __future__ import annotations

import argparse
import dataclasses
import hashlib
import hmac
import json
import math
import os
import pathlib
import re
import struct
import sys
import uuid as uuid_module
from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from typing import Any

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

# REM-PARITY Sections 5.1, 5.3 and 6. The functions below are called by name;
# gf_pow, _crc64_table_entry, the CRC table and the CBOR head and item decoders
# are used only through them. The three constants are imported so the tests
# can check them against Sections 5.1 and 6.1.
from rem_parity_rederive import (  # noqa: E402
    CRC64_XZ_REFLECTED_POLYNOMIAL,
    GF_REDUCTION_POLYNOMIAL,
    MASK64,
    RederivationError,
    _cbor_type_and_length,
    _multiplication_table,
    cauchy_matrix,
    crc64_xz,
    decode_deterministic_cbor,
    encode_deterministic_cbor,
    encode_parity,
    gf_inv,
    gf_mul,
)

# REM-OBJECT Section 4: the plaintext builder and its file specification.
from verify_rem_object_vectors_independent import FileSpec, build_plaintext_with_manifest  # noqa: E402


# ---------------------------------------------------------------------------
# Everything below is written for this implementation from the text.
# ---------------------------------------------------------------------------

REPO_ROOT = pathlib.Path(__file__).resolve().parents[1]
FIXTURE_ROOT = REPO_ROOT / "fixtures" / "rem-parity-terminal-index-draft"
OUTPUT_ROOT = FIXTURE_ROOT / "second-implementation"
SPEC_PATH = REPO_ROOT / "specs" / "in-progress" / "rem-parity-1-specification.md"

# REM-PARITY Section 2.5 constants.
BOOTSTRAP_MAGIC_BYTES = bytes.fromhex("52454d00424f4f01")
BOOTSTRAP_SCHEMA_MAJOR_VALUE = 2
BOOTSTRAP_SCHEMA_MINOR_VALUE = 3
BOOTSTRAP_HEADER_LENGTH = 0x38
LABEL_SIDECAR = bytes.fromhex("52454d0050415201")
LABEL_SIDECAR_FOOTER = bytes.fromhex("52454d00504152464f4f5401")
LABEL_PARITY_MAP = b"REM\x00PMAP\x01"
LABEL_REPLICA_HEADER = bytes.fromhex("52454d00544952455001" "48")
LABEL_REPLICA_FOOTER = bytes.fromhex("52454d00544952455001" "46")
LABEL_SEPARATION_HEADER = bytes.fromhex("52454d00544953455001" "48")
LABEL_SEPARATION_FOOTER = bytes.fromhex("52454d00544953455001" "46")
SIDECAR_METADATA_DOMAIN = b"remanence-sidecar-metadata-v1"
SCHEME_IDENTIFIER = "rs-cauchy-gf256-v1"
PARITY_MAP_FORMAT = "rem-parity-map-v1"
TERMINAL_RECORD_SIZES = (262144, 524288, 1048576)
DISCOVERY_CANDIDATES = (262144, 524288, 1048576)
REPLICA_FRAME_LEN = 0x400
SEPARATION_FRAME_LEN = 0x200
LAYOUT_DOMAIN = b"REM-TERMINAL-TAIL-LAYOUT-V1\x00"
PAYLOAD_DOMAIN = b"REM-TAPE-INDEX-REPLICA-PAYLOAD-V1\x00"
EDITION_DOMAIN = b"REM-TAPE-INDEX-EDITION-V1\x00"
REPLICA_DESCRIPTOR_DOMAIN = b"REM-TAPE-INDEX-REPLICA-DESCRIPTOR-V1\x00"
SEPARATION_DESCRIPTOR_DOMAIN = b"REM-INDEX-SEPARATION-DESCRIPTOR-V1\x00"
STRUCTURAL_SLOT = 64
OBJECT_SLOT = 256

KIND_OBJECT = 0
KIND_SIDECAR = 1
KIND_BOOTSTRAP = 2
KIND_PARITY_MAP = 3
KIND_REPLICA = 4
KIND_SEPARATION = 5
KIND_NAMES = {
    0: "Object",
    1: "ParitySidecar",
    2: "Bootstrap",
    3: "ParityMap",
    4: "TapeIndexReplica",
    5: "IndexSeparationExtent",
}
KIND_CODES = {name: code for code, name in KIND_NAMES.items()}

FLAG_FINAL_PARTIAL_EPOCH = 0x01
FLAG_PRIMARY_KNOWN_GOOD = 0x02
FLAG_TAIL_KNOWN_GOOD = 0x04
DIRECTORY_FLAG_MASK = 0x07


class BuildRefusal(Exception):
    """A Writer refusal or an inconsistent input, naming the input field."""

    def __init__(self, field_name: str, message: str) -> None:
        super().__init__(f"{field_name}: {message}")
        self.field_name = field_name
        self.message = message


def le16(value: int) -> bytes:
    return struct.pack("<H", value)


def le32(value: int) -> bytes:
    return struct.pack("<I", value)


def le64(value: int) -> bytes:
    return struct.pack("<Q", value)


def digest(data: bytes) -> bytes:
    return hashlib.sha256(data).digest()


def role_magic(tape_uuid: bytes, label: bytes) -> bytes:
    """Section 5.2: HMAC-SHA-256(key = tape_uuid, message = LABEL)[0..8]."""
    return hmac.new(tape_uuid, label, hashlib.sha256).digest()[:8]


def ceil_div(numerator: int, denominator: int) -> int:
    return -(-numerator // denominator)


@dataclass(frozen=True)
class MapEntry:
    """One Section 7.1 filemark-map entry."""

    tape_file_number: int
    kind: int
    block_count: int
    first_parity_data_ordinal: int | None = None
    protected_ordinal_start: int | None = None
    protected_ordinal_end_exclusive: int | None = None
    epoch_id: int | None = None

    def row(self) -> list[Any]:
        return [
            self.tape_file_number,
            self.kind,
            self.block_count,
            self.first_parity_data_ordinal,
            self.protected_ordinal_start,
            self.protected_ordinal_end_exclusive,
            self.epoch_id,
        ]


def canonical_projection(entries: Sequence[MapEntry]) -> bytes:
    """Section 7.3: deterministic CBOR of the array of 7-element arrays."""
    return encode_deterministic_cbor([entry.row() for entry in entries])


def canonical_digest(entries: Sequence[MapEntry]) -> bytes:
    return digest(canonical_projection(entries))


def derived_total(entries: Sequence[MapEntry]) -> int:
    """Section 7.2 T."""
    values = [
        entry.first_parity_data_ordinal + entry.block_count
        for entry in entries
        if entry.kind == KIND_OBJECT
    ]
    return max(values) if values else 0


def derived_watermark(entries: Sequence[MapEntry]) -> int:
    """Section 7.2 W."""
    values = [
        entry.protected_ordinal_end_exclusive
        for entry in entries
        if entry.kind == KIND_SIDECAR
    ]
    return max(values) if values else 0


def lba_of_file(entries: Sequence[MapEntry], tape_file_number: int) -> int:
    """Section 3.2: LBA(f, 0) = sum over g < f of (block_count(g) + 1)."""
    return sum(entry.block_count + 1 for entry in entries[:tape_file_number])


# ---------------------------------------------------------------------------
# Section 8.1/8.2: the bootstrap.
# ---------------------------------------------------------------------------


def build_bootstrap(
    tape_uuid: bytes,
    block_size: int,
    scheme: tuple[int, int, int] | None,
    writer_version: str | None,
    write_timestamp: str | None,
    drive_compression: bool = False,
) -> bytes:
    payload: dict[int, Any] = {}
    flags = 0
    if scheme is None:
        flags |= 1
    else:
        k, m, stripes = scheme
        payload[1] = {1: SCHEME_IDENTIFIER, 2: k, 3: m, 4: stripes}
        bot_entry = MapEntry(0, KIND_BOOTSTRAP, 1)
        payload[2] = {1: canonical_digest([bot_entry]), 2: 1, 3: 0, 4: 0, 5: False}
    if writer_version is not None:
        payload[3] = writer_version
    if write_timestamp is not None:
        payload[4] = write_timestamp
    payload[5] = drive_compression
    encoded = encode_deterministic_cbor(payload)
    header = bytearray()
    header += BOOTSTRAP_MAGIC_BYTES
    header += struct.pack(">HHI", BOOTSTRAP_SCHEMA_MAJOR_VALUE, BOOTSTRAP_SCHEMA_MINOR_VALUE, flags)
    header += tape_uuid
    header += struct.pack(">I", block_size)
    header += struct.pack(">Q", 0)
    header += struct.pack("<I", len(encoded))
    header += le64(crc64_xz(bytes(header)))
    block = bytes(header) + encoded + le64(crc64_xz(encoded))
    if len(block) > block_size:
        raise BuildRefusal("block_size", "BootstrapPayloadTooLarge")
    return block + bytes(block_size - len(block))


BOOTSTRAP_FIELDS = [
    (0x00, 8, "magic"),
    (0x08, 2, "schema_major"),
    (0x0A, 2, "schema_minor"),
    (0x0C, 4, "flags"),
    (0x10, 16, "tape_uuid"),
    (0x20, 4, "block_size_bytes"),
    (0x24, 8, "sequence"),
    (0x2C, 4, "cbor_payload_len"),
    (0x30, 8, "crc64_header"),
]


def describe_table(table: Sequence[tuple[int, int, str]], offset: int) -> str | None:
    for start, length, name in table:
        if start <= offset < start + length:
            return f"{name} (frame offset 0x{start:X}, byte {offset - start})"
    return None


def describe_bootstrap(block: bytes, offset: int) -> str:
    named = describe_table(BOOTSTRAP_FIELDS, offset)
    if named:
        return named
    payload_len = struct.unpack_from("<I", block, 0x2C)[0]
    if offset < 0x38 + payload_len:
        return f"CBOR payload byte {offset - 0x38}"
    if offset < 0x38 + payload_len + 8:
        return "crc64_payload"
    return "trailing zero fill"


# ---------------------------------------------------------------------------
# Section 9: the parity sidecar.
# ---------------------------------------------------------------------------


def sidecar_layout(block_size: int, stripes: int, m: int, real: int) -> tuple[int, int, list[list[int]]]:
    """Section 9.4, returning H, inline_index_entry_bytes and per-block entry offsets.

    The per-block list gives, for each index block, the byte offset of each
    entry placed in it (entries are numbered in stream order).
    """
    limit = block_size - 8
    offset = 0xC8
    blocks = 1
    inline: int | None = None
    placements: list[list[int]] = [[]]
    total_entries = stripes * m + real
    for index in range(total_entries):
        length = 16 if index < stripes * m else 8
        if offset + length > limit:
            if offset == (0xC8 if blocks == 1 else 0):
                raise BuildRefusal("block_size", "block_size cannot hold an index entry")
            if blocks == 1 and inline is None:
                inline = offset - 0xC8
            blocks += 1
            offset = 0
            placements.append([])
        placements[-1].append(offset)
        offset += length
    if inline is None:
        inline = offset - 0xC8
    return blocks, inline, placements


@dataclass
class SidecarBuild:
    epoch_id: int
    start: int
    end: int
    header_blocks: int
    parity_blocks: int
    total_blocks: int
    inline_bytes: int
    metadata_hash: bytes
    blocks: list[bytes]
    parity_crcs: dict[tuple[int, int], int]
    data_crcs: list[int]
    placements: list[list[int]]


def _sidecar_header(
    tape_uuid: bytes,
    block_size: int,
    scheme: tuple[int, int, int],
    epoch_id: int,
    start: int,
    end: int,
    header_blocks: int,
    inline_bytes: int,
    copy_kind: int,
    metadata_hash: bytes,
) -> bytes:
    k, m, stripes = scheme
    parity_blocks = stripes * m
    real = end - start
    header = bytearray(0xC8)
    header[0x00:0x08] = role_magic(tape_uuid, LABEL_SIDECAR)
    header[0x08:0x18] = tape_uuid
    header[0x18:0x20] = le64(epoch_id)
    header[0x20:0x22] = le16(k)
    header[0x22:0x24] = le16(m)
    header[0x24:0x28] = le32(stripes)
    header[0x28:0x2C] = le32(block_size)
    header[0x2C:0x30] = le32(2)
    header[0x30:0x38] = le64(start)
    header[0x38:0x40] = le64(end)
    header[0x40:0x48] = le64(stripes * k)
    header[0x48:0x50] = le64(real)
    header[0x50:0x58] = le64(parity_blocks)
    header[0x58:0x60] = le64(real)
    header[0x60:0x68] = le64(header_blocks)
    header[0x68:0x70] = le64(inline_bytes)
    header[0x70:0x78] = le64(2 * header_blocks + parity_blocks + 1)
    header[0x78:0x80] = le64(0)
    header[0x80:0x88] = le64(header_blocks + parity_blocks)
    header[0x88:0x90] = le64(2 * header_blocks + parity_blocks)
    header[0x90:0x92] = le16(copy_kind)
    header[0x92:0x94] = le16(0)
    header[0x94:0x98] = le32(0)
    header[0x98:0xB8] = metadata_hash
    header[0xB8:0xC0] = le64(0)
    header[0xC0:0xC8] = le64(crc64_xz(bytes(header[0x00:0xC0])))
    return bytes(header)


def build_sidecar(
    tape_uuid: bytes,
    block_size: int,
    scheme: tuple[int, int, int],
    epoch_id: int,
    start: int,
    end: int,
    data_block: Any,
) -> SidecarBuild:
    k, m, stripes = scheme
    real = end - start
    if not 1 <= real <= stripes * k:
        raise BuildRefusal("scheme", f"epoch range [{start}, {end}) is not 1..S*k data ordinals")
    parity_blocks = stripes * m
    matrix = cauchy_matrix(k, m)
    zero = bytes(block_size)
    parity: dict[tuple[int, int], bytes] = {}
    for stripe in range(stripes):
        shards = []
        for data_index in range(k):
            ordinal = start + data_index * stripes + stripe
            shards.append(data_block(ordinal) if ordinal < end else zero)
        for parity_index, shard in enumerate(encode_parity(shards, matrix)):
            parity[(stripe, parity_index)] = shard
    parity_crcs = {key: crc64_xz(value) for key, value in parity.items()}
    data_crcs = [crc64_xz(data_block(ordinal)) for ordinal in range(start, end)]
    entries: list[bytes] = []
    for stripe in range(stripes):
        for parity_index in range(m):
            entries.append(struct.pack("<IHHQ", stripe, parity_index, 0, parity_crcs[(stripe, parity_index)]))
    for value in data_crcs:
        entries.append(le64(value))
    header_blocks, inline_bytes, placements = sidecar_layout(block_size, stripes, m, real)
    provisional = _sidecar_header(
        tape_uuid, block_size, scheme, epoch_id, start, end, header_blocks, inline_bytes, 1, bytes(32)
    )
    metadata_hash = digest(SIDECAR_METADATA_DOMAIN + provisional[0x00:0x90] + b"".join(entries))

    def index_copy(copy_kind: int) -> list[bytes]:
        header = _sidecar_header(
            tape_uuid, block_size, scheme, epoch_id, start, end, header_blocks, inline_bytes,
            copy_kind, metadata_hash,
        )
        blocks_out = []
        entry_number = 0
        for block_index, offsets in enumerate(placements):
            block = bytearray(block_size)
            if block_index == 0:
                block[0:0xC8] = header
            for offset in offsets:
                entry = entries[entry_number]
                block[offset : offset + len(entry)] = entry
                entry_number += 1
            block[block_size - 8 :] = le64(crc64_xz(bytes(block[: block_size - 8])))
            blocks_out.append(bytes(block))
        return blocks_out

    footer = bytearray(block_size)
    footer[0x00:0x08] = role_magic(tape_uuid, LABEL_SIDECAR_FOOTER)
    footer[0x08:0x0A] = le16(2)
    footer[0x10:0x20] = tape_uuid
    footer[0x20:0x28] = le64(epoch_id)
    footer[0x28:0x30] = le64(start)
    footer[0x30:0x38] = le64(end)
    footer[0x38:0x40] = le64(header_blocks)
    footer[0x40:0x48] = le64(parity_blocks)
    footer[0x48:0x50] = le64(2 * header_blocks + parity_blocks + 1)
    footer[0x50:0x58] = le64(0)
    footer[0x58:0x60] = le64(header_blocks + parity_blocks)
    footer[0x60:0x80] = metadata_hash
    footer[0x80:0x88] = le64(crc64_xz(bytes(footer[0x00:0x80])))
    parity_region = [bytes(block_size)] * parity_blocks
    for (stripe, parity_index), shard in parity.items():
        parity_region[parity_index * stripes + stripe] = shard
    blocks = index_copy(1) + parity_region + index_copy(2) + [bytes(footer)]
    return SidecarBuild(
        epoch_id, start, end, header_blocks, parity_blocks, len(blocks), inline_bytes,
        metadata_hash, blocks, parity_crcs, data_crcs, placements,
    )


SIDECAR_HEADER_FIELDS = [
    (0x00, 8, "magic"), (0x08, 16, "tape_uuid"), (0x18, 8, "epoch_id"), (0x20, 2, "k"),
    (0x22, 2, "m"), (0x24, 4, "S"), (0x28, 4, "block_size"), (0x2C, 4, "schema_version"),
    (0x30, 8, "protected_ordinal_start"), (0x38, 8, "protected_ordinal_end_exclusive"),
    (0x40, 8, "logical_shard_count"), (0x48, 8, "real_data_shard_count"),
    (0x50, 8, "parity_block_count"), (0x58, 8, "data_crc_count"),
    (0x60, 8, "sidecar_header_block_count"), (0x68, 8, "inline_index_entry_bytes"),
    (0x70, 8, "sidecar_total_block_count"), (0x78, 8, "primary_header_start_block"),
    (0x80, 8, "tail_header_start_block"), (0x88, 8, "footer_block_index"),
    (0x90, 2, "copy_kind"), (0x92, 2, "reserved"), (0x94, 4, "copy_generation"),
    (0x98, 32, "canonical_metadata_hash"), (0xB8, 8, "reserved u64"), (0xC0, 8, "header_crc64"),
]
SIDECAR_FOOTER_FIELDS = [
    (0x00, 8, "magic"), (0x08, 2, "footer_version"), (0x0A, 6, "reserved"), (0x10, 16, "tape_uuid"),
    (0x20, 8, "epoch_id"), (0x28, 8, "protected_ordinal_start"), (0x30, 8, "protected_ordinal_end_exclusive"),
    (0x38, 8, "H"), (0x40, 8, "P"), (0x48, 8, "sidecar_total_block_count"),
    (0x50, 8, "primary_header_start_block"), (0x58, 8, "tail_header_start_block"),
    (0x60, 32, "canonical_metadata_hash"), (0x80, 8, "footer_crc64"),
]


def describe_sidecar(info: dict[str, Any], block_index: int, offset: int) -> str:
    header_blocks = info["H"]
    parity_blocks = info["P"]
    stripes = info["S"]
    block_size = info["block_size"]
    if block_index == 2 * header_blocks + parity_blocks:
        named = describe_table(SIDECAR_FOOTER_FIELDS, offset)
        return "footer block: " + (named or "zero fill")
    if header_blocks <= block_index < header_blocks + parity_blocks:
        shard = block_index - header_blocks
        return (
            f"parity shard block {block_index} (stripe {shard % stripes}, parity_index "
            f"{shard // stripes}), byte {offset}"
        )
    copy = "primary" if block_index < header_blocks else "tail"
    copy_block = block_index if copy == "primary" else block_index - header_blocks - parity_blocks
    prefix = f"{copy} header/index copy block {copy_block}: "
    if offset >= block_size - 8:
        return prefix + "block CRC-64"
    if copy_block == 0 and offset < 0xC8:
        return prefix + (describe_table(SIDECAR_HEADER_FIELDS, offset) or "header")
    offsets = info["placements"][copy_block]
    parity_entries = info["S"] * info["m"]
    first_entry = sum(len(block) for block in info["placements"][:copy_block])
    for position, start in enumerate(offsets):
        number = first_entry + position
        length = 16 if number < parity_entries else 8
        if start <= offset < start + length:
            if number < parity_entries:
                return prefix + (
                    f"parity index entry {number} (stripe {number // info['m']}, "
                    f"parity_index {number % info['m']}), byte {offset - start}"
                )
            return prefix + f"data-CRC entry for ordinal index {number - parity_entries}, byte {offset - start}"
    return prefix + "zero fill below the block CRC"


# ---------------------------------------------------------------------------
# Section 10.1: the final ParityMap.
# ---------------------------------------------------------------------------


@dataclass
class ParityMapBuild:
    blocks: list[bytes]
    copy_blocks: int
    payload: bytes
    payload_len: int


def build_parity_map(
    tape_uuid: bytes,
    block_size: int,
    sequence: int,
    tape_file_number: int,
    directory_entries: list[dict[int, Any]],
    prefix_entries: list[MapEntry],
    writer_version: str | None,
    write_timestamp: str | None,
) -> tuple[ParityMapBuild, MapEntry]:
    """Build the one final ParityMap and its structural entry (Section 10.1)."""

    def payload_for(map_digest: bytes, scope: int, total: int, watermark: int) -> bytes:
        directory = {1: scope, 2: total, 3: watermark, 4: True, 5: directory_entries}
        payload_map: dict[int, Any] = {
            1: PARITY_MAP_FORMAT,
            2: tape_uuid,
            3: sequence,
            4: directory,
            5: map_digest,
        }
        if writer_version is not None:
            payload_map[6] = writer_version
        if write_timestamp is not None:
            payload_map[7] = write_timestamp
        return encode_deterministic_cbor(payload_map)

    scope = tape_file_number + 1
    total = derived_total(prefix_entries)
    watermark = derived_watermark(prefix_entries)
    provisional = payload_for(bytes(32), scope, total, watermark)
    copy_blocks = ceil_div(0xC8 + len(provisional), block_size)
    entry = MapEntry(tape_file_number, KIND_PARITY_MAP, 2 * copy_blocks + 1)
    map_digest = canonical_digest(list(prefix_entries) + [entry])
    payload = payload_for(map_digest, scope, total, watermark)
    if len(payload) != len(provisional):
        raise BuildRefusal("parity_map", "payload length changed with the digest")

    def header(copy_kind: int) -> bytes:
        frame = bytearray(0xC8)
        frame[0x00:0x08] = role_magic(tape_uuid, LABEL_PARITY_MAP)
        frame[0x08:0x0A] = le16(2)
        frame[0x0A:0x0C] = le16(copy_kind)
        frame[0x10:0x20] = tape_uuid
        frame[0x20:0x28] = le64(sequence)
        frame[0x28:0x2C] = le32(block_size)
        frame[0x30:0x38] = le64(len(payload))
        frame[0x38:0x58] = digest(payload)
        frame[0x58:0x78] = map_digest
        frame[0x78:0x80] = le64(scope)
        frame[0x80:0x88] = le64(total)
        frame[0x88:0x90] = le64(watermark)
        frame[0x90] = 1
        frame[0x98:0xA0] = le64(copy_blocks)
        frame[0xA0:0xA8] = le64(2 * copy_blocks + 1)
        frame[0xA8:0xB0] = le64(0)
        frame[0xB0:0xB8] = le64(copy_blocks)
        frame[0xB8:0xC0] = le64(2 * copy_blocks)
        frame[0xC0:0xC8] = le64(crc64_xz(bytes(frame[0x00:0xC0])))
        return bytes(frame)

    def copy(copy_kind: int) -> list[bytes]:
        body = header(copy_kind) + payload
        body += bytes(copy_blocks * block_size - len(body))
        return [body[i * block_size : (i + 1) * block_size] for i in range(copy_blocks)]

    footer = header(0) + bytes(block_size - 0xC8)
    blocks = copy(1) + copy(2) + [footer]
    return ParityMapBuild(blocks, copy_blocks, payload, len(payload)), entry


PARITY_MAP_FIELDS = [
    (0x00, 8, "magic"), (0x08, 2, "schema/footer version"), (0x0A, 2, "copy_kind"),
    (0x0C, 4, "reserved"), (0x10, 16, "tape_uuid"), (0x20, 8, "sequence"), (0x28, 4, "block_size"),
    (0x2C, 4, "reserved alignment"), (0x30, 8, "payload_len"), (0x38, 32, "payload_sha256"),
    (0x58, 32, "canonical_map_digest"), (0x78, 8, "directory_scope_tape_file_count"),
    (0x80, 8, "directory_scope_total_data_ordinals"),
    (0x88, 8, "directory_scope_highest_protected_ordinal"), (0x90, 1, "is_final_directory"),
    (0x91, 7, "reserved"), (0x98, 8, "copy_block_count"), (0xA0, 8, "parity_map_total_block_count"),
    (0xA8, 8, "primary_copy_start_block"), (0xB0, 8, "tail_copy_start_block"),
    (0xB8, 8, "footer_block_index"), (0xC0, 8, "header CRC-64"),
]


def describe_parity_map(info: dict[str, Any], block_index: int, offset: int) -> str:
    copy_blocks = info["M"]
    block_size = info["block_size"]
    if block_index == 2 * copy_blocks:
        return "footer: " + (describe_table(PARITY_MAP_FIELDS, offset) or "zero fill")
    copy = "primary" if block_index < copy_blocks else "tail"
    within = (block_index % copy_blocks) * block_size + offset
    if within < 0xC8:
        return f"{copy} copy header: " + (describe_table(PARITY_MAP_FIELDS, within) or "")
    if within < 0xC8 + info["payload_len"]:
        return f"{copy} copy CBOR payload byte {within - 0xC8}"
    return f"{copy} copy unused zero tail"


# ---------------------------------------------------------------------------
# Sections 8.3, 10.2-10.5: the terminal suffix.
# ---------------------------------------------------------------------------


def encode_slot(encoded: bytes, slot_size: int) -> bytes:
    """Section 10.2: encoded_len:u16 || deterministic CBOR || zero padding."""
    if not 1 <= len(encoded) <= slot_size - 2:
        raise BuildRefusal("slot", f"encoded length {len(encoded)} does not fit a {slot_size}-byte slot")
    return le16(len(encoded)) + encoded + bytes(slot_size - 2 - len(encoded))


def component_tuple(kind: int, ordinal: int, tape_file: int, start_lba: int, records: int) -> bytes:
    return struct.pack("<HHIQQQ", kind, ordinal, 1, tape_file, start_lba, records)


@dataclass
class TerminalInputs:
    tape_uuid: bytes
    block_size: int
    structural: list[MapEntry]
    object_rows: list[Any]
    edition_id: bytes
    edition_sequence: int
    writer_version: bytes
    write_timestamp: bytes
    nominal_extent_bytes: int
    start_tape_file: int
    start_lba: int
    covered_count: int
    total_data_ordinals: int
    highest_protected_ordinal: int
    partition: int = 0
    object_slots: list[bytes] | None = None


@dataclass
class TerminalBuild:
    components: list[list[bytes]]
    tuples: list[bytes]
    plan: list[tuple[int, int, int, int, int]]
    expected_eod: int
    payload_len: int
    payload_records: int
    replica_records: int
    separation_records: int
    payload_sha256: bytes
    canonical_sha256: bytes
    edition_digest: bytes
    layout_digest: bytes
    replica_descriptors: list[bytes]
    separation_descriptors: list[bytes]
    structural_slots: list[bytes]
    object_slot_bytes: list[bytes]


def build_payload_slots(inputs: TerminalInputs) -> tuple[list[bytes], list[bytes]]:
    structural = [encode_slot(encode_deterministic_cbor(entry.row()), STRUCTURAL_SLOT) for entry in inputs.structural]
    if inputs.object_slots is not None:
        objects = list(inputs.object_slots)
    else:
        objects = [encode_slot(encode_deterministic_cbor(row), OBJECT_SLOT) for row in inputs.object_rows]
    return structural, objects


def terminal_geometry(structural_rows: int, object_rows: int, block_size: int, nominal: int) -> tuple[int, int, int, int]:
    payload_len = STRUCTURAL_SLOT * structural_rows + OBJECT_SLOT * object_rows
    payload_records = ceil_div(payload_len, block_size)
    replica_records = 2 + payload_records
    separation_records = ceil_div(nominal, block_size)
    if separation_records < 2:
        raise BuildRefusal("nominal_extent_bytes", "a separation extent needs at least two records")
    return payload_len, payload_records, replica_records, separation_records


def terminal_plan(
    start_tape_file: int, start_lba: int, replica_records: int, separation_records: int
) -> tuple[list[tuple[int, int, int, int, int]], int]:
    order = [(4, 1), (5, 1), (4, 2), (5, 2), (4, 3)]
    plan = []
    tape_file = start_tape_file
    lba = start_lba
    for kind, ordinal in order:
        records = replica_records if kind == KIND_REPLICA else separation_records
        plan.append((kind, ordinal, tape_file, lba, records))
        tape_file += 1
        lba += records + 1
    return plan, lba


def layout_digest_of(partition: int, block_size: int, tuples: Sequence[bytes], eod: int) -> bytes:
    return digest(
        LAYOUT_DOMAIN + le32(partition) + le32(block_size) + le16(5) + b"".join(tuples) + le64(eod)
    )


def edition_digest_of(
    tape_uuid: bytes, edition_id: bytes, edition_sequence: int, partition: int, block_size: int,
    covered: int, total: int, watermark: int, structural_rows: int, object_rows: int,
    payload_len: int, payload_records: int, payload_sha: bytes, canonical_sha: bytes,
    writer_version: bytes, write_timestamp: bytes,
) -> bytes:
    return digest(
        EDITION_DOMAIN
        + le16(1)
        + tape_uuid
        + edition_id
        + le64(edition_sequence)
        + le32(partition)
        + le32(block_size)
        + le32(0)
        + le64(covered)
        + le64(total)
        + le64(watermark)
        + le64(structural_rows)
        + le64(object_rows)
        + le64(payload_len)
        + le64(payload_records)
        + payload_sha
        + canonical_sha
        + le64(len(writer_version))
        + writer_version
        + le64(len(write_timestamp))
        + write_timestamp
    )


def replica_descriptor_of(
    edition: bytes, layout: bytes, ordinal: int, local_tuple: bytes, footer_offset: int
) -> bytes:
    return digest(
        REPLICA_DESCRIPTOR_DOMAIN + edition + layout + le16(ordinal) + le16(3) + local_tuple + le64(footer_offset)
    )


def separation_descriptor_of(
    tape_uuid: bytes, edition_id: bytes, ordinal: int, partition: int, block_size: int,
    nominal: int, total_records: int, local_tuple: bytes, predecessor: bytes, successor: bytes,
    layout: bytes,
) -> bytes:
    return digest(
        SEPARATION_DESCRIPTOR_DOMAIN
        + tape_uuid
        + edition_id
        + le16(ordinal)
        + le16(2)
        + le32(partition)
        + le32(block_size)
        + le64(nominal)
        + le64(total_records)
        + local_tuple
        + predecessor
        + successor
        + layout
    )


def replica_frame(
    inputs: TerminalInputs,
    geometry: dict[str, Any],
    role: int,
    ordinal: int,
    local: tuple[int, int, int, int, int],
    descriptor: bytes,
    header_record_hash: bytes | None,
) -> bytes:
    frame = bytearray(REPLICA_FRAME_LEN)
    label = LABEL_REPLICA_HEADER if role == 1 else LABEL_REPLICA_FOOTER
    frame[0x000:0x008] = role_magic(inputs.tape_uuid, label)
    frame[0x008:0x00A] = le16(1)
    frame[0x00A:0x00C] = le16(role)
    frame[0x00C:0x010] = le32(1)
    frame[0x010:0x020] = inputs.tape_uuid
    frame[0x020:0x030] = inputs.edition_id
    frame[0x030:0x038] = le64(inputs.edition_sequence)
    frame[0x038:0x03A] = le16(ordinal)
    frame[0x03A:0x03C] = le16(3)
    frame[0x03C:0x040] = le32(inputs.partition)
    frame[0x040:0x044] = le32(inputs.block_size)
    frame[0x044:0x048] = le32(0)
    frame[0x048:0x050] = le64(inputs.covered_count)
    frame[0x050:0x058] = le64(inputs.total_data_ordinals)
    frame[0x058:0x060] = le64(inputs.highest_protected_ordinal)
    frame[0x060:0x068] = le64(geometry["structural_rows"])
    frame[0x068:0x070] = le64(geometry["object_rows"])
    frame[0x070:0x078] = le64(geometry["payload_len"])
    frame[0x078:0x080] = le64(geometry["payload_records"])
    frame[0x080:0x088] = le64(geometry["replica_records"])
    frame[0x088:0x090] = le64(local[2])
    frame[0x090:0x098] = le64(local[3])
    frame[0x098:0x0A0] = le64(1 + geometry["payload_records"])
    frame[0x0A0:0x0A8] = le64(geometry["expected_eod"])
    frame[0x0A8:0x0C8] = geometry["payload_sha256"]
    frame[0x0C8:0x0E8] = geometry["canonical_sha256"]
    frame[0x0E8:0x108] = geometry["edition_digest"]
    frame[0x108:0x128] = geometry["layout_digest"]
    frame[0x128:0x148] = descriptor
    frame[0x148:0x1E8] = b"".join(geometry["tuples"])
    frame[0x1F0:0x1F2] = le16(len(inputs.writer_version))
    frame[0x1F2:0x1F4] = le16(len(inputs.write_timestamp))
    frame[0x1F8 : 0x1F8 + len(inputs.writer_version)] = inputs.writer_version
    frame[0x278 : 0x278 + len(inputs.write_timestamp)] = inputs.write_timestamp
    if role == 2:
        records = local[4]
        frame[0x2B8:0x2D8] = header_record_hash
        frame[0x2D8:0x2E0] = le64(local[2])
        frame[0x2E0:0x2E8] = le64(local[3])
        frame[0x2E8:0x2F0] = le64(records)
        frame[0x2F0:0x2F8] = le64(local[3] + records - 1)
        frame[0x2F8:0x300] = le64(records - 1)
    frame[0x3F8:0x400] = le64(crc64_xz(bytes(frame[0x000:0x3F8])))
    return bytes(frame) + bytes(inputs.block_size - REPLICA_FRAME_LEN)


def separation_frame(
    inputs: TerminalInputs,
    geometry: dict[str, Any],
    role: int,
    ordinal: int,
    local: tuple[int, int, int, int, int],
    predecessor: tuple[int, int, int, int, int],
    successor: tuple[int, int, int, int, int],
    descriptor: bytes,
    header_record_hash: bytes | None,
) -> bytes:
    frame = bytearray(SEPARATION_FRAME_LEN)
    label = LABEL_SEPARATION_HEADER if role == 1 else LABEL_SEPARATION_FOOTER
    total_records = local[4]
    frame[0x000:0x008] = role_magic(inputs.tape_uuid, label)
    frame[0x008:0x00A] = le16(1)
    frame[0x00A:0x00C] = le16(role)
    frame[0x00C:0x010] = le32(0)
    frame[0x010:0x020] = inputs.tape_uuid
    frame[0x020:0x030] = inputs.edition_id
    frame[0x030:0x032] = le16(ordinal)
    frame[0x032:0x034] = le16(2)
    frame[0x034:0x038] = le32(inputs.partition)
    frame[0x038:0x03C] = le32(inputs.block_size)
    frame[0x03C:0x040] = le32(0)
    frame[0x040:0x048] = le64(local[2])
    frame[0x048:0x050] = le64(local[3])
    frame[0x050:0x058] = le64(inputs.nominal_extent_bytes)
    frame[0x058:0x060] = le64(total_records)
    frame[0x060:0x068] = le64(total_records - 1)
    frame[0x068:0x070] = le64(total_records - 2)
    frame[0x070:0x072] = le16(predecessor[1])
    frame[0x072:0x074] = le16(successor[1])
    frame[0x074:0x078] = le32(0)
    frame[0x078:0x080] = le64(predecessor[2])
    frame[0x080:0x088] = le64(successor[2])
    frame[0x088:0x090] = le64(predecessor[3])
    frame[0x090:0x098] = le64(successor[3])
    frame[0x098:0x0B8] = geometry["layout_digest"]
    frame[0x0B8:0x0D8] = descriptor
    frame[0x0D8:0x0E0] = le64(geometry["expected_eod"])
    frame[0x0E0:0x180] = b"".join(geometry["tuples"])
    if role == 2:
        frame[0x180:0x1A0] = header_record_hash
        frame[0x1A0:0x1A8] = le64(local[2])
        frame[0x1A8:0x1B0] = le64(local[3])
        frame[0x1B0:0x1B8] = le64(total_records)
        frame[0x1B8:0x1C0] = le64(local[3] + total_records - 1)
        frame[0x1C0:0x1C8] = le64(total_records - 1)
    frame[0x1F8:0x200] = le64(crc64_xz(bytes(frame[0x000:0x1F8])))
    return bytes(frame) + bytes(inputs.block_size - SEPARATION_FRAME_LEN)


def build_terminal_suffix(inputs: TerminalInputs) -> TerminalBuild:
    block_size = inputs.block_size
    if block_size not in TERMINAL_RECORD_SIZES:
        raise BuildRefusal("block_size", "terminal record size must be 256 KiB, 512 KiB or 1 MiB")
    structural_slots, object_slots = build_payload_slots(inputs)
    payload_len, payload_records, replica_records, separation_records = terminal_geometry(
        len(structural_slots), len(object_slots), block_size, inputs.nominal_extent_bytes
    )
    plan, expected_eod = terminal_plan(inputs.start_tape_file, inputs.start_lba, replica_records, separation_records)
    tuples = [component_tuple(*component) for component in plan]
    layout = layout_digest_of(inputs.partition, block_size, tuples, expected_eod)
    slots = structural_slots + object_slots
    payload_sha = digest(PAYLOAD_DOMAIN + b"".join(slots))
    canonical_sha = canonical_digest(inputs.structural)
    edition = edition_digest_of(
        inputs.tape_uuid, inputs.edition_id, inputs.edition_sequence, inputs.partition, block_size,
        inputs.covered_count, inputs.total_data_ordinals, inputs.highest_protected_ordinal,
        len(structural_slots), len(object_slots), payload_len, payload_records, payload_sha,
        canonical_sha, inputs.writer_version, inputs.write_timestamp,
    )
    geometry = {
        "structural_rows": len(structural_slots),
        "object_rows": len(object_slots),
        "payload_len": payload_len,
        "payload_records": payload_records,
        "replica_records": replica_records,
        "expected_eod": expected_eod,
        "payload_sha256": payload_sha,
        "canonical_sha256": canonical_sha,
        "edition_digest": edition,
        "layout_digest": layout,
        "tuples": tuples,
    }
    payload = b"".join(slots)
    payload += bytes(payload_records * block_size - len(payload))
    payload_blocks = [payload[i * block_size : (i + 1) * block_size] for i in range(payload_records)]
    components: list[list[bytes]] = []
    replica_descriptors = []
    separation_descriptors = []
    for index, local in enumerate(plan):
        kind, ordinal = local[0], local[1]
        if kind == KIND_REPLICA:
            descriptor = replica_descriptor_of(edition, layout, ordinal, tuples[index], 1 + payload_records)
            replica_descriptors.append(descriptor)
            header = replica_frame(inputs, geometry, 1, ordinal, local, descriptor, None)
            footer = replica_frame(inputs, geometry, 2, ordinal, local, descriptor, digest(header))
            components.append([header] + payload_blocks + [footer])
        else:
            predecessor = plan[index - 1]
            successor = plan[index + 1]
            descriptor = separation_descriptor_of(
                inputs.tape_uuid, inputs.edition_id, ordinal, inputs.partition, block_size,
                inputs.nominal_extent_bytes, local[4], tuples[index], tuples[index - 1],
                tuples[index + 1], layout,
            )
            separation_descriptors.append(descriptor)
            header = separation_frame(inputs, geometry, 1, ordinal, local, predecessor, successor, descriptor, None)
            footer = separation_frame(
                inputs, geometry, 2, ordinal, local, predecessor, successor, descriptor, digest(header)
            )
            components.append([header] + [bytes(block_size)] * (local[4] - 2) + [footer])
    return TerminalBuild(
        components, tuples, plan, expected_eod, payload_len, payload_records, replica_records,
        separation_records, payload_sha, canonical_sha, edition, layout, replica_descriptors,
        separation_descriptors, structural_slots, object_slots,
    )


REPLICA_FRAME_FIELDS = [
    (0x000, 8, "role magic"), (0x008, 2, "schema version"), (0x00A, 2, "role"), (0x00C, 4, "flags"),
    (0x010, 16, "tape UUID"), (0x020, 16, "edition ID"), (0x030, 8, "edition sequence"),
    (0x038, 2, "replica ordinal"), (0x03A, 2, "replica count"), (0x03C, 4, "partition"),
    (0x040, 4, "block size"), (0x044, 4, "compression mode"), (0x048, 8, "covered-prefix tape-file count"),
    (0x050, 8, "total data ordinals"), (0x058, 8, "highest protected ordinal"),
    (0x060, 8, "structural_row_count"), (0x068, 8, "object_row_count"), (0x070, 8, "payload length"),
    (0x078, 8, "payload record count"), (0x080, 8, "replica record count"),
    (0x088, 8, "planned local tape-file number"), (0x090, 8, "planned local start LBA"),
    (0x098, 8, "forward footer block offset"), (0x0A0, 8, "planned terminal EOD LBA"),
    (0x0A8, 32, "payload SHA-256"), (0x0C8, 32, "canonical-map SHA-256"), (0x0E8, 32, "edition digest"),
    (0x108, 32, "terminal-layout digest"), (0x128, 32, "replica descriptor digest"),
    (0x148, 160, "planned component tuples"), (0x1E8, 8, "reserved zero"),
    (0x1F0, 2, "writer-version length"), (0x1F2, 2, "write-timestamp length"), (0x1F4, 4, "reserved zero"),
    (0x1F8, 128, "writer version"), (0x278, 64, "write timestamp"), (0x2B8, 32, "header-record SHA-256"),
    (0x2D8, 8, "observed tape-file number"), (0x2E0, 8, "observed start LBA"),
    (0x2E8, 8, "observed record count"), (0x2F0, 8, "observed footer LBA"),
    (0x2F8, 8, "backward start delta"), (0x300, 248, "reserved zero"), (0x3F8, 8, "CRC-64/XZ"),
]
SEPARATION_FRAME_FIELDS = [
    (0x000, 8, "role magic"), (0x008, 2, "schema version"), (0x00A, 2, "role"), (0x00C, 4, "flags"),
    (0x010, 16, "tape UUID"), (0x020, 16, "edition ID"), (0x030, 2, "separation ordinal"),
    (0x032, 2, "separation count"), (0x034, 4, "partition"), (0x038, 4, "block size"),
    (0x03C, 4, "compression mode"), (0x040, 8, "planned local tape-file number"),
    (0x048, 8, "planned local start LBA"), (0x050, 8, "nominal total extent bytes"),
    (0x058, 8, "total record count"), (0x060, 8, "footer block offset"),
    (0x068, 8, "zero interior record count"), (0x070, 2, "predecessor replica ordinal"),
    (0x072, 2, "successor replica ordinal"), (0x074, 4, "fill kind"),
    (0x078, 8, "predecessor tape-file number"), (0x080, 8, "successor tape-file number"),
    (0x088, 8, "predecessor start LBA"), (0x090, 8, "successor start LBA"),
    (0x098, 32, "terminal-layout digest"), (0x0B8, 32, "separation descriptor digest"),
    (0x0D8, 8, "planned terminal EOD LBA"), (0x0E0, 160, "planned component tuples"),
    (0x180, 32, "header-record SHA-256"), (0x1A0, 8, "observed tape-file number"),
    (0x1A8, 8, "observed start LBA"), (0x1B0, 8, "observed record count"),
    (0x1B8, 8, "observed footer LBA"), (0x1C0, 8, "backward start delta"),
    (0x1C8, 48, "reserved zero"), (0x1F8, 8, "CRC-64/XZ"),
]


def describe_terminal(component_kind: int, info: dict[str, Any], record: int, offset: int) -> str:
    records = info["records"]
    if record == 0 or record == records - 1:
        role = "header" if record == 0 else "footer"
        table = REPLICA_FRAME_FIELDS if component_kind == KIND_REPLICA else SEPARATION_FRAME_FIELDS
        frame_len = REPLICA_FRAME_LEN if component_kind == KIND_REPLICA else SEPARATION_FRAME_LEN
        if offset >= frame_len:
            return f"{role} record: zero padding after the frame"
        named = describe_table(table, offset)
        if named and "tuples" in named:
            within = offset - (0x148 if component_kind == KIND_REPLICA else 0x0E0)
            names = ["kind", "kind", "ordinal", "ordinal", "filemark count", "filemark count", "filemark count",
                     "filemark count"] + ["tape-file number"] * 8 + ["start LBA"] * 8 + ["record count"] * 8
            named += f" [tuple {within // 32}, {names[within % 32]}]"
        return f"{role} record: {named}"
    if component_kind == KIND_SEPARATION:
        return f"interior record {record}, byte {offset}"
    payload_offset = (record - 1) * info["block_size"] + offset
    structural_bytes = STRUCTURAL_SLOT * info["structural_rows"]
    if payload_offset < structural_bytes:
        slot = payload_offset // STRUCTURAL_SLOT
        within = payload_offset % STRUCTURAL_SLOT
        part = "encoded_len" if within < 2 else "CBOR or zero padding"
        return f"payload structural slot {slot} (tape file {slot}), byte {within} ({part})"
    object_bytes = OBJECT_SLOT * info["object_rows"]
    if payload_offset < structural_bytes + object_bytes:
        slot = (payload_offset - structural_bytes) // OBJECT_SLOT
        within = (payload_offset - structural_bytes) % OBJECT_SLOT
        part = "encoded_len" if within < 2 else "CBOR or zero padding"
        return f"payload Object-row slot {slot}, byte {within} ({part})"
    return "payload padding after the last slot"


# ---------------------------------------------------------------------------
# Comparison and reporting.
# ---------------------------------------------------------------------------


@dataclass
class Comparison:
    artifact: str
    reproduced: bool
    detail: str

    def as_json(self) -> dict[str, Any]:
        return {"artifact": self.artifact, "result": "reproduced" if self.reproduced else "mismatch",
                "detail": self.detail}


def first_difference(built: bytes, candidate: bytes) -> int | None:
    if built == candidate:
        return None
    limit = min(len(built), len(candidate))
    low, high = 0, limit
    # Binary search over equal prefixes, block by block for speed.
    step = 65536
    position = 0
    while position < limit:
        end = min(position + step, limit)
        if built[position:end] != candidate[position:end]:
            for index in range(position, end):
                if built[index] != candidate[index]:
                    return index
        position = end
    del low, high
    return limit


def compare_bytes(artifact: str, built: bytes, candidate: bytes, describe: Any) -> Comparison:
    index = first_difference(built, candidate)
    if index is None:
        return Comparison(artifact, True, "reproduced")
    if index >= min(len(built), len(candidate)):
        return Comparison(
            artifact, False,
            f"length differs: built {len(built)} bytes, candidate {len(candidate)} bytes; first "
            f"differing byte {index}",
        )
    return Comparison(
        artifact, False,
        f"first differing byte {index} (built 0x{built[index]:02x}, candidate 0x{candidate[index]:02x}): "
        f"{describe(index)}",
    )


def compare_value(artifact: str, column: str, built: Any, recorded: Any) -> Comparison:
    if str(built) == str(recorded):
        return Comparison(f"{artifact} [{column}]", True, "reproduced")
    return Comparison(f"{artifact} [{column}]", False, f"field {column}: built {built}, recorded {recorded}")


def read_tsv(path: pathlib.Path) -> list[dict[str, str]]:
    lines = path.read_text(encoding="utf-8").splitlines()
    header = lines[0].split("\t")
    return [dict(zip(header, line.split("\t"), strict=True)) for line in lines[1:] if line.strip()]


# ---------------------------------------------------------------------------
# Full tape images (tape-images/inputs/*.json).
# ---------------------------------------------------------------------------


@dataclass
class TapeFile:
    tape_file_number: int
    kind: int | None
    blocks: list[bytes]
    has_filemark: bool
    info: dict[str, Any]


@dataclass
class ImageBuild:
    name: str
    block_size: int
    tape_uuid: bytes
    scheme: tuple[int, int, int]
    files: list[TapeFile]
    prefix_entries: list[MapEntry]
    object_rows: list[dict[int, Any]]
    terminal: TerminalBuild | None
    finalized: bool

    def records(self) -> list[Any]:
        """The record sequence: data blocks as bytes, filemarks as None."""
        out: list[Any] = []
        for tape_file in self.files:
            out.extend(tape_file.blocks)
            if tape_file.has_filemark:
                out.append(None)
        return out

    def file_start_lba(self, tape_file_number: int) -> int:
        lba = 0
        for tape_file in self.files[:tape_file_number]:
            lba += len(tape_file.blocks) + (1 if tape_file.has_filemark else 0)
        return lba


def _require_field(mapping: Mapping[str, Any], key: str, path: str) -> Any:
    if key not in mapping:
        raise BuildRefusal(path + key, "missing")
    return mapping[key]


def decode_payload(spec: Mapping[str, Any], path: str) -> bytes:
    if spec.get("encoding") != "repeat-byte":
        raise BuildRefusal(path + "encoding", f"unsupported payload encoding {spec.get('encoding')!r}")
    value = spec["byte"]
    count = spec["count"]
    if not (isinstance(value, int) and 0 <= value <= 255 and isinstance(count, int) and count >= 0):
        raise BuildRefusal(path + "byte", "repeat-byte payload needs a byte and a count")
    return bytes([value]) * count


_RFC3339 = re.compile(
    r"^\d{4}-\d{2}-\d{2}[Tt]\d{2}:\d{2}:\d{2}(\.\d+)?([Zz]|[+-]\d{2}:\d{2})$"
)
_PAX_MTIME = re.compile(r"^\d+(\.\d+)?$")


def check_path_rules(path_value: str, entry_type: str, field_name: str) -> None:
    """REM-OBJECT Section 4.6.6 rules 1-3, which bind the Builder."""
    data = path_value.encode("utf-8")
    if not data or any(byte < 0x20 for byte in data):
        raise BuildRefusal(field_name, "InvalidInput: empty path or control byte")
    body = path_value[:-1] if entry_type == "directory" and path_value.endswith("/") else path_value
    if path_value.startswith("/") or (path_value.endswith("/") and entry_type != "directory"):
        raise BuildRefusal(field_name, "InvalidInput: non-canonical path")
    if entry_type == "directory" and not path_value.endswith("/"):
        raise BuildRefusal(field_name, "InvalidInput: directory path without trailing slash")
    if any(part in ("", ".", "..") for part in body.split("/")):
        raise BuildRefusal(field_name, "InvalidInput: non-canonical path component")
    if path_value == "_remanence" or path_value.startswith("_remanence/"):
        raise BuildRefusal(field_name, "InvalidInput: reserved _remanence path")


def build_rem_object(obj: Mapping[str, Any], block_size: int, index: int) -> tuple[bytes, dict[str, Any]]:
    """Build one plaintext REM-OBJECT from an image's recorded Object inputs."""
    prefix = f"objects[{index}]."
    options = _require_field(obj, "options", prefix)
    if options.get("encryption") != "none":
        raise BuildRefusal(prefix + "options.encryption", "only the plaintext representation is recorded")
    if options.get("chunk_size") != block_size:
        raise BuildRefusal(prefix + "options.chunk_size", "REM-OBJECT 8.2: chunk_size MUST equal the block size")
    if options["chunk_size"] <= 0 or options["chunk_size"] % 512:
        raise BuildRefusal(prefix + "options.chunk_size", "InvalidInput: chunk_size not a multiple of 512")
    if options.get("metadata_preservation") not in ("minimal", "archival", "full"):
        raise BuildRefusal(prefix + "options.metadata_preservation", "not minimal, archival or full")
    object_id = options["object_id"]
    raw_id = object_id.encode("utf-8")
    if not 1 <= len(raw_id) <= 64 or b"\x00" in raw_id:
        raise BuildRefusal(prefix + "options.object_id", "object_id must be 1-64 non-NUL bytes")
    if not options.get("caller_object_id"):
        raise BuildRefusal(prefix + "options.caller_object_id", "must be non-empty")
    if not _RFC3339.match(options.get("write_timestamp", "")):
        raise BuildRefusal(prefix + "options.write_timestamp", "not an RFC 3339 timestamp")
    specs: list[FileSpec] = []
    seen_paths: set[str] = set()
    seen_ids: set[str] = set()
    for file_index, entry in enumerate(obj.get("files", [])):
        path = f"{prefix}files[{file_index}]."
        entry_type = entry.get("entry_type", "regular")
        if entry_type not in ("regular", "hardlink", "symlink", "directory"):
            raise BuildRefusal(path + "entry_type", "unknown entry type")
        data = decode_payload(entry["payload"], path + "payload.") if entry_type == "regular" else b""
        if entry.get("size_bytes") != len(data):
            raise BuildRefusal(path + "size_bytes", "InvalidInput: payload size differs from size_bytes")
        if entry_type == "regular" and entry.get("file_sha256") != hashlib.sha256(data).hexdigest():
            raise BuildRefusal(path + "file_sha256", "InvalidInput: payload hash differs from file_sha256")
        check_path_rules(entry["path"], entry_type, path + "path")
        if entry["path"] in seen_paths:
            raise BuildRefusal(path + "path", "InvalidInput: duplicate path")
        if not entry.get("file_id") or entry["file_id"] in seen_ids:
            raise BuildRefusal(path + "file_id", "InvalidInput: empty or duplicate file_id")
        seen_paths.add(entry["path"])
        seen_ids.add(entry["file_id"])
        if entry.get("mtime") is not None and not _PAX_MTIME.match(entry["mtime"]):
            raise BuildRefusal(path + "mtime", "InvalidInput: malformed mtime")
        if entry.get("executable") not in (None, True, False):
            raise BuildRefusal(path + "executable", "must be true, false or null")
        if entry.get("xattrs"):
            raise BuildRefusal(path + "xattrs", "the inputs record no encoding for attribute values")
        if entry_type in ("hardlink", "symlink") and entry.get("link_target") is None:
            raise BuildRefusal(path + "link_target", "InvalidInput: link target missing")
        if entry_type in ("regular", "directory") and entry.get("link_target") is not None:
            raise BuildRefusal(path + "link_target", "only links carry a target")
        specs.append(
            FileSpec(
                path=entry["path"],
                file_id=entry["file_id"],
                data=data,
                entry_type=entry_type,
                link_target=entry.get("link_target"),
                executable=entry.get("executable"),
                mtime=entry.get("mtime"),
                xattrs={},
                extensions=dict(entry.get("extensions", {})),
            )
        )
    manifest_file_id = options.get("manifest_file_id")
    if not manifest_file_id or manifest_file_id in seen_ids:
        raise BuildRefusal(prefix + "options.manifest_file_id", "InvalidInput: empty or colliding manifest file_id")
    builder_options = {
        "caller_object_id": options["caller_object_id"],
        "chunk_size": options["chunk_size"],
        "metadata_preservation": options["metadata_preservation"],
        "object_id": object_id,
        "write_timestamp": options["write_timestamp"],
        "manifest_file_id": manifest_file_id,
        "extensions": dict(options.get("extensions", {})),
    }
    stored, layout = build_plaintext_with_manifest(builder_options, specs)
    if len(stored) % block_size or not stored:
        raise BuildRefusal(prefix + "options.chunk_size", "stored length is not a positive block multiple")
    return stored, layout


def describe_object(info: dict[str, Any], offset: int) -> str:
    block_size = info["block_size"]
    where = f"object block {offset // block_size}, byte {offset % block_size}"
    layout = info["layout"]
    regions = [(0, "global pax header")]
    for number, entry in enumerate(layout["files"]):
        regions.append((entry.pax_header_offset, f"entry {number} ({entry.path}) pax header and ustar header"))
        if entry.size_bytes:
            regions.append((entry.data_offset, f"entry {number} ({entry.path}) payload"))
    manifest = layout["manifest"]
    regions.append((manifest.pax_header_offset, "manifest pax header and ustar header"))
    regions.append((manifest.data_offset, "manifest CBOR"))
    eof = manifest.data_offset + manifest.size_bytes + (-manifest.size_bytes % 512)
    regions.append((eof, "tar EOF records"))
    regions.append((eof + 1024, "final zero fill"))
    label = "?"
    for start, name in regions:
        if offset >= start:
            label = name
    return f"{where}: {label}"


def image_scheme(inputs: Mapping[str, Any]) -> tuple[int, int, int]:
    scheme = _require_field(inputs, "scheme", "")
    if scheme.get("id") != SCHEME_IDENTIFIER:
        raise BuildRefusal("scheme.id", "scheme_id MUST be rs-cauchy-gf256-v1")
    k = scheme["data_blocks_per_stripe"]
    m = scheme["parity_blocks_per_stripe"]
    stripes = scheme["stripes_per_neighborhood"]
    if not (k >= 2 and 1 <= m <= k and stripes >= 1 and k + m <= 255 and stripes * (k + m) <= 2**32 - 1):
        raise BuildRefusal("scheme", "scheme triple violates Section 6.6 validity")
    return k, m, stripes


def build_image(inputs: Mapping[str, Any], name: str) -> ImageBuild:
    block_size = _require_field(inputs, "block_size", "")
    if block_size not in DISCOVERY_CANDIDATES:
        raise BuildRefusal("block_size", "a production Writer MUST use a discovery-candidate block size")
    tape_uuid = uuid_module.UUID(_require_field(inputs, "tape_uuid", "")).bytes
    if _require_field(inputs, "compression", "") is not False:
        raise BuildRefusal("compression", "DriveCompressionEnabled: compression MUST be off for a parity write")
    k, m, stripes = scheme = image_scheme(inputs)
    diagnostics = bool(_require_field(inputs, "diagnostic_keys_present", ""))
    identity = _require_field(inputs, "writer_identity", "")
    writer_version = identity["writer_version"] if diagnostics else None
    write_timestamp = identity["write_timestamp"] if diagnostics else None
    if diagnostics:
        raw = writer_version.encode("ascii")
        if len(raw) > 128 or any(not 0x20 <= byte <= 0x7E for byte in raw):
            raise BuildRefusal("writer_identity.writer_version", "not at most 128 printable US-ASCII bytes")
        if len(write_timestamp.encode()) > 64 or not _RFC3339.match(write_timestamp):
            raise BuildRefusal("writer_identity.write_timestamp", "not an RFC 3339 date-time of at most 64 bytes")
    directory_flags = _require_field(inputs, "directory_flags", "")
    if directory_flags & ~(FLAG_PRIMARY_KNOWN_GOOD | FLAG_TAIL_KNOWN_GOOD):
        raise BuildRefusal("directory_flags", "only the known-good bits may be supplied; bit 0 is Writer-set")
    checkpoints = _require_field(inputs, "checkpoint_after_objects", "")
    objects = _require_field(inputs, "objects", "")
    for checkpoint in checkpoints:
        if not (isinstance(checkpoint, int) and 0 <= checkpoint < len(objects)):
            raise BuildRefusal("checkpoint_after_objects", f"no Object {checkpoint} to checkpoint after")
    stop = _require_field(inputs, "stop", "")
    files: list[TapeFile] = []
    entries: list[MapEntry] = []
    boot = build_bootstrap(tape_uuid, block_size, scheme, writer_version, write_timestamp)
    files.append(TapeFile(0, KIND_BOOTSTRAP, [boot], True, {}))
    entries.append(MapEntry(0, KIND_BOOTSTRAP, 1))
    data_blocks: dict[int, bytes] = {}
    ordinal = 0
    epoch_start = 0
    next_epoch = 0
    pending: list[tuple[int, int, int, int]] = []
    directory: list[dict[int, Any]] = []
    object_rows: list[dict[int, Any]] = []

    def emit_sidecar(epoch_id: int, start: int, end: int, flags: int) -> None:
        sidecar = build_sidecar(tape_uuid, block_size, scheme, epoch_id, start, end, data_blocks.__getitem__)
        tape_file_number = len(files)
        info = {"H": sidecar.header_blocks, "P": sidecar.parity_blocks, "S": stripes, "m": m,
                "block_size": block_size, "placements": sidecar.placements, "sidecar": sidecar}
        files.append(TapeFile(tape_file_number, KIND_SIDECAR, sidecar.blocks, True, info))
        entries.append(MapEntry(tape_file_number, KIND_SIDECAR, sidecar.total_blocks, None, start, end, epoch_id))
        directory.append({
            1: tape_file_number, 2: epoch_id, 3: start, 4: end, 5: sidecar.total_blocks,
            6: sidecar.header_blocks, 7: sidecar.parity_blocks, 8: sidecar.metadata_hash, 9: flags,
        })

    for index, obj in enumerate(objects):
        stored, layout = build_rem_object(obj, block_size, index)
        blocks = [stored[i : i + block_size] for i in range(0, len(stored), block_size)]
        tape_file_number = len(files)
        first_ordinal = ordinal
        files.append(TapeFile(tape_file_number, KIND_OBJECT, blocks, True,
                              {"layout": layout, "block_size": block_size}))
        entries.append(MapEntry(tape_file_number, KIND_OBJECT, len(blocks), first_ordinal))
        for block in blocks:
            data_blocks[ordinal] = block
            ordinal += 1
            if ordinal - epoch_start == stripes * k:
                pending.append((next_epoch, epoch_start, ordinal, directory_flags))
                next_epoch += 1
                epoch_start = ordinal
        for epoch_id, start, end, flags in pending:
            emit_sidecar(epoch_id, start, end, flags)
        pending = []
        manifest = layout["manifest"]
        object_rows.append({
            1: tape_file_number,
            2: "plaintext",
            3: len(blocks),
            4: obj["options"]["object_id"].encode("utf-8"),
            10: manifest.first_chunk_lba,
            11: manifest.size_bytes,
            12: manifest.chunk_count,
            13: layout["manifest_sha256"],
        })
        if index in checkpoints and ordinal > epoch_start:
            emit_sidecar(next_epoch, epoch_start, ordinal, directory_flags)
            next_epoch += 1
            epoch_start = ordinal

    terminal: TerminalBuild | None = None
    finalized = False
    if stop.get("kind") == "finalized":
        finalized = True
        if ordinal > epoch_start:
            emit_sidecar(next_epoch, epoch_start, ordinal, directory_flags | FLAG_FINAL_PARTIAL_EPOCH)
            next_epoch += 1
            epoch_start = ordinal
        if directory:
            parity_map, parity_entry = build_parity_map(
                tape_uuid, block_size, _require_field(inputs, "parity_map_sequence_start", ""),
                len(files), directory, list(entries), writer_version, write_timestamp,
            )
            files.append(TapeFile(len(files), KIND_PARITY_MAP, parity_map.blocks, True,
                                  {"M": parity_map.copy_blocks, "payload_len": parity_map.payload_len,
                                   "block_size": block_size}))
            entries.append(parity_entry)
        prefix_entries = list(entries)
        edition_id = bytes.fromhex(_require_field(inputs, "edition_id_hex", ""))
        edition_sequence = _require_field(inputs, "edition_sequence", "")
        if len(edition_id) != 16 or edition_id == bytes(16):
            raise BuildRefusal("edition_id_hex", "the edition ID MUST be 16 bytes and not all zero")
        if edition_sequence == 0:
            raise BuildRefusal("edition_sequence", "the edition sequence MUST NOT be zero")

        def terminal_for(edition: bytes, sequence: int) -> TerminalBuild:
            return build_terminal_suffix(TerminalInputs(
                tape_uuid=tape_uuid,
                block_size=block_size,
                structural=prefix_entries,
                object_rows=object_rows,
                edition_id=edition,
                edition_sequence=sequence,
                writer_version=(writer_version or "").encode("ascii"),
                write_timestamp=(write_timestamp or "").encode("ascii"),
                nominal_extent_bytes=_require_field(inputs, "nominal_extent_bytes", ""),
                start_tape_file=len(prefix_entries),
                start_lba=sum(entry.block_count + 1 for entry in prefix_entries),
                covered_count=len(prefix_entries),
                total_data_ordinals=derived_total(prefix_entries),
                highest_protected_ordinal=derived_watermark(prefix_entries),
            ))

        terminal = terminal_for(edition_id, edition_sequence)
        components = list(terminal.components)
        replacement = inputs.get("replica_b_replacement")
        if replacement is not None:
            if replacement.get("other_inputs") != "identical":
                raise BuildRefusal("replica_b_replacement.other_inputs", "only an identical recipe is recorded")
            second = terminal_for(bytes.fromhex(replacement["edition_id_hex"]), replacement["edition_sequence"])
            if replacement.get("source_tape_file") != terminal.plan[2][2]:
                raise BuildRefusal("replica_b_replacement.source_tape_file", "is not replica B's tape file")
            components[2] = second.components[2]
        for index, component in enumerate(components):
            kind = terminal.plan[index][0]
            info = {"records": len(component), "block_size": block_size,
                    "structural_rows": len(prefix_entries), "object_rows": len(object_rows)}
            files.append(TapeFile(len(files), kind, component, True, info))
    elif stop.get("kind") == "session-end":
        # Not a fixture stop kind: the session ends after its last Object and a
        # barrier closes the open epoch, even a short one (Section 11.2). Used
        # only to build the uninterrupted-session check for the resume cases.
        if ordinal > epoch_start:
            emit_sidecar(next_epoch, epoch_start, ordinal, directory_flags)
            next_epoch += 1
            epoch_start = ordinal
    elif stop.get("kind") == "committed-prefix":
        torn_records = stop.get("torn_records")
        fill = stop.get("torn_record_fill")
        if not (isinstance(torn_records, int) and torn_records >= 0 and isinstance(fill, int) and 0 <= fill <= 255):
            raise BuildRefusal("stop", "a torn tail needs a record count and a fill byte")
        if torn_records:
            files.append(TapeFile(len(files), None, [bytes([fill]) * block_size] * torn_records, False, {}))
    else:
        raise BuildRefusal("stop.kind", f"unknown stop kind {stop.get('kind')!r}")
    total_bytes = sum(len(block) for tape_file in files for block in tape_file.blocks)
    if total_bytes > _require_field(inputs, "capacity_bytes", ""):
        raise BuildRefusal("capacity_bytes", "the image does not fit the recorded capacity")
    prefix = list(entries)
    return ImageBuild(name, block_size, tape_uuid, scheme, files, prefix, object_rows, terminal, finalized)


def describe_tape_file(tape_file: TapeFile, offset: int, block_size: int) -> str:
    block_index = offset // block_size
    within = offset % block_size
    base = f"tape file {tape_file.tape_file_number} "
    if tape_file.kind is None:
        return base + f"(torn tail) record {block_index}, byte {within}"
    kind_name = KIND_NAMES[tape_file.kind]
    base += f"({kind_name}) record {block_index}: "
    if tape_file.kind == KIND_BOOTSTRAP:
        return base + describe_bootstrap(tape_file.blocks[0], within)
    if tape_file.kind == KIND_SIDECAR:
        return base + describe_sidecar(tape_file.info, block_index, within)
    if tape_file.kind == KIND_PARITY_MAP:
        return base + describe_parity_map(tape_file.info, block_index, within)
    if tape_file.kind == KIND_OBJECT:
        return base + describe_object(tape_file.info, offset)
    return base + describe_terminal(tape_file.kind, tape_file.info, block_index, within)


def image_rows(image: ImageBuild) -> list[dict[str, Any]]:
    """The MANIFEST.tsv rows my build implies, one per tape file and one for ALL."""
    rows = []
    records = image.records()
    eod = len(records)
    lba = 0
    for tape_file in image.files:
        data = b"".join(tape_file.blocks)
        rows.append({
            "image": image.name,
            "tape_file": str(tape_file.tape_file_number),
            "start_record": str(lba),
            "data_records": str(len(tape_file.blocks)),
            "bytes": str(len(data)),
            "sha256": hashlib.sha256(data).hexdigest(),
            "filemark_record": str(lba + len(tape_file.blocks)) if tape_file.has_filemark else "none",
            "eod_record": str(eod),
        })
        lba += len(tape_file.blocks) + (1 if tape_file.has_filemark else 0)
    everything = hashlib.sha256()
    count = 0
    size = 0
    for tape_file in image.files:
        for block in tape_file.blocks:
            everything.update(block)
            count += 1
            size += len(block)
    rows.append({
        "image": image.name, "tape_file": "ALL", "start_record": "0", "data_records": str(count),
        "bytes": str(size), "sha256": everything.hexdigest(), "filemark_record": "structural",
        "eod_record": str(eod),
    })
    return rows


def compare_image(
    image: ImageBuild,
    manifest_rows: list[dict[str, str]],
    reference: ImageBuild | None = None,
) -> list[Comparison]:
    """Compare every tape file, the torn tail and ALL with MANIFEST.tsv.

    The pinned image bytes are not checked in, only their sizes and digests.
    When a digest differs and a reference build whose digests all match the
    manifest is supplied, the first differing byte is located against it.
    """
    results = []
    built_rows = {row["tape_file"]: row for row in image_rows(image)}
    recorded = {row["tape_file"]: row for row in manifest_rows if row["image"] == image.name}
    reference_files = {str(f.tape_file_number): f for f in reference.files} if reference else {}
    for key in sorted(set(built_rows) | set(recorded), key=lambda value: (value == "ALL", len(value), value)):
        artifact = f"{image.name} tape file {key}"
        if key not in recorded:
            results.append(Comparison(artifact, False, "built tape file has no MANIFEST.tsv row"))
            continue
        if key not in built_rows:
            results.append(Comparison(artifact, False, "MANIFEST.tsv row has no built tape file"))
            continue
        built = built_rows[key]
        row = recorded[key]
        problems = [column for column in ("start_record", "data_records", "bytes", "sha256",
                                          "filemark_record", "eod_record") if built[column] != row[column]]
        if not problems:
            results.append(Comparison(artifact, True, "reproduced"))
            continue
        detail = "; ".join(f"field {column}: built {built[column]}, recorded {row[column]}" for column in problems)
        if key != "ALL" and key in reference_files:
            mine = b"".join(image.files[int(key)].blocks)
            theirs = b"".join(reference_files[key].blocks)
            index = first_difference(mine, theirs)
            if index is not None and index < min(len(mine), len(theirs)):
                detail += "; first differing byte " + str(index) + ": " + describe_tape_file(
                    image.files[int(key)], index, image.block_size)
        results.append(Comparison(artifact, False, detail))
    return results


# ---------------------------------------------------------------------------
# Terminal profiles (minimal-*, multi-*), maximums, extensions, streaming.
# ---------------------------------------------------------------------------


def structural_from_json(entries: list[Mapping[str, Any]]) -> list[MapEntry]:
    out = []
    for index, entry in enumerate(entries):
        kind = entry["kind"]
        if kind not in KIND_CODES:
            raise BuildRefusal(f"structural_entries[{index}].kind", f"unknown kind {kind!r}")
        out.append(MapEntry(
            entry["tape_file_number"], KIND_CODES[kind], entry["block_count"],
            entry["first_parity_data_ordinal"], entry["protected_ordinal_start"],
            entry["protected_ordinal_end_exclusive"], entry["epoch_id"],
        ))
    return out


def object_row_from_json(row: Mapping[str, Any], index: int) -> dict[int, Any]:
    representation = row["representation"]
    out: dict[int, Any] = {1: row["tape_file_number"], 3: row["stored_block_count"],
                           4: bytes.fromhex(row["object_id"])}
    if representation["kind"] == "Plaintext":
        out[2] = "plaintext"
        out[10] = representation["manifest_first_chunk_lba"]
        out[11] = representation["manifest_size_bytes"]
        out[12] = representation["manifest_chunk_count"]
        out[13] = bytes.fromhex(representation["manifest_sha256"])
    elif representation["kind"] == "Encrypted":
        out[2] = "encrypted"
        out[21] = representation["metadata_frame_len"]
        out[22] = [bytes.fromhex(value) for value in representation["recipient_epoch_ids"]]
        out[23] = representation["key_frame_len"]
    else:
        raise BuildRefusal(f"object_rows[{index}].representation.kind", "unknown representation")
    return out


def writer_check_prefix(structural: list[MapEntry], rows: list[dict[int, Any]], scope: Mapping[str, Any]) -> None:
    """Refuse a terminal payload a conformant Writer could not produce (Sections 7.2, 10.2, 10.3, 10.6)."""
    ordinal = 0
    chain = 0
    epoch = 0
    for index, entry in enumerate(structural):
        name = f"structural_entries[{index}]"
        if entry.tape_file_number != index:
            raise BuildRefusal(name + ".tape_file_number", "tape-file numbers must be dense from 0")
        if entry.kind not in (0, 1, 2, 3) or entry.block_count <= 0:
            raise BuildRefusal(name + ".kind", "kinds 0-3 with a nonzero block count only")
        fields = (entry.first_parity_data_ordinal, entry.protected_ordinal_start,
                  entry.protected_ordinal_end_exclusive, entry.epoch_id)
        if entry.kind == KIND_BOOTSTRAP and (index != 0 or entry.block_count != 1 or any(v is not None for v in fields)):
            raise BuildRefusal(name, "the bootstrap is the one-block tape file 0")
        if entry.kind == KIND_OBJECT:
            if entry.first_parity_data_ordinal != ordinal or any(v is not None for v in fields[1:]):
                raise BuildRefusal(name + ".first_parity_data_ordinal", "Object first ordinals must be dense")
            ordinal += entry.block_count
        if entry.kind == KIND_SIDECAR:
            if fields[0] is not None or entry.protected_ordinal_start != chain or \
                    entry.protected_ordinal_end_exclusive is None or entry.protected_ordinal_end_exclusive <= chain \
                    or entry.epoch_id != epoch:
                raise BuildRefusal(name + ".protected_ordinal_start", "sidecar ranges and epochs must chain from 0")
            chain = entry.protected_ordinal_end_exclusive
            epoch += 1
        if entry.kind == KIND_PARITY_MAP and (index != len(structural) - 1 or any(v is not None for v in fields)):
            raise BuildRefusal(name + ".kind", "the ParityMap is the final structural row")
    if not structural or structural[0].kind != KIND_BOOTSTRAP:
        raise BuildRefusal("structural_entries[0].kind", "structural row 0 must be the bootstrap")
    has_sidecar = any(e.kind == KIND_SIDECAR for e in structural)
    has_parity_map = any(e.kind == KIND_PARITY_MAP for e in structural)
    if has_sidecar != has_parity_map:
        raise BuildRefusal("structural_entries", "a final ParityMap is present if and only if a sidecar is")
    if has_sidecar and chain != ordinal:
        raise BuildRefusal("structural_entries", "finalization must leave no Object ordinal unprotected")
    objects = [e for e in structural if e.kind == KIND_OBJECT]
    if len(objects) != len(rows):
        raise BuildRefusal("object_rows", "one Object row per kind-0 structural row")
    for index, (entry, row) in enumerate(zip(objects, rows, strict=True)):
        if row[1] != entry.tape_file_number:
            raise BuildRefusal(f"object_rows[{index}].tape_file_number", "must match the kind-0 structural row")
        if row[3] != entry.block_count:
            raise BuildRefusal(f"object_rows[{index}].stored_block_count", "must match the structural block count")
        if not 1 <= len(row[4]) <= 64 or b"\x00" in row[4]:
            raise BuildRefusal(f"object_rows[{index}].object_id", "1-64 non-NUL bytes")
    if scope["covered_prefix_tape_file_count"] != len(structural):
        raise BuildRefusal("scope.covered_prefix_tape_file_count", f"the structural rows give {len(structural)}")
    if scope["total_data_ordinals"] != derived_total(structural):
        raise BuildRefusal("scope.total_data_ordinals", f"the structural rows give {derived_total(structural)}")
    if scope["highest_protected_ordinal"] != derived_watermark(structural):
        raise BuildRefusal("scope.highest_protected_ordinal", f"the structural rows give {derived_watermark(structural)}")


def terminal_inputs_from_profile(inputs: Mapping[str, Any]) -> TerminalInputs:
    if inputs.get("compression_enabled") is not False:
        raise BuildRefusal("compression_enabled", "the terminal layout requires compression disabled")
    try:
        structural = structural_from_json(inputs["structural_entries"])
        rows = [object_row_from_json(row, index) for index, row in enumerate(inputs["object_rows"])]
    except (KeyError, TypeError, ValueError) as failure:
        if isinstance(failure, BuildRefusal):
            raise
        raise BuildRefusal("object_rows", f"missing or malformed field: {failure}") from failure
    writer_check_prefix(structural, rows, inputs["scope"])
    counts = inputs["counts"]
    if counts["structural_entry_count"] != len(structural):
        raise BuildRefusal("counts.structural_entry_count", "differs from the structural entries")
    if counts["object_row_count"] != len(rows):
        raise BuildRefusal("counts.object_row_count", "differs from the object rows")
    layout = inputs["terminal_layout"]
    start_lba = sum(entry.block_count + 1 for entry in structural)
    if layout["prefix_end_lba"] != start_lba:
        raise BuildRefusal("terminal_layout.prefix_end_lba", f"the structural rows give {start_lba}")
    if layout["start_tape_file"] != len(structural):
        raise BuildRefusal("terminal_layout.start_tape_file", f"the structural rows give {len(structural)}")
    block_size = inputs["block_size"]
    _, _, replica_records, separation_records = terminal_geometry(
        len(structural), len(rows), block_size, inputs["separation_extent"]["nominal_extent_bytes"])
    if layout["replica_record_count"] != replica_records:
        raise BuildRefusal("terminal_layout.replica_record_count", f"Section 10.4 gives {replica_records}")
    if layout["separation_records"] != separation_records:
        raise BuildRefusal("terminal_layout.separation_records", f"Section 10.5 gives {separation_records}")
    if inputs["separation_extent"]["total_records"] != separation_records:
        raise BuildRefusal("separation_extent.total_records", f"Section 10.5 gives {separation_records}")
    if layout["partition"] != 0:
        raise BuildRefusal("terminal_layout.partition", "all positions are partition 0")
    diagnostics = inputs.get("diagnostics") or {}
    scope = inputs["scope"]
    edition_id = bytes.fromhex(inputs["edition_id"])
    if len(edition_id) != 16 or edition_id == bytes(16):
        raise BuildRefusal("edition_id", "the edition ID MUST be 16 nonzero bytes")
    if inputs["edition_sequence"] == 0:
        raise BuildRefusal("edition_sequence", "the edition sequence MUST NOT be zero")
    return TerminalInputs(
        tape_uuid=bytes.fromhex(inputs["tape_uuid"]),
        block_size=block_size,
        structural=structural,
        object_rows=rows,
        edition_id=edition_id,
        edition_sequence=inputs["edition_sequence"],
        writer_version=diagnostics.get("writer_version", "").encode("ascii"),
        write_timestamp=diagnostics.get("write_timestamp", "").encode("ascii"),
        nominal_extent_bytes=inputs["separation_extent"]["nominal_extent_bytes"],
        start_tape_file=len(structural),
        start_lba=start_lba,
        covered_count=scope["covered_prefix_tape_file_count"],
        total_data_ordinals=scope["total_data_ordinals"],
        highest_protected_ordinal=scope["highest_protected_ordinal"],
        partition=layout["partition"],
    )


PROFILE_COMPONENT_FILES = ["replica-a.bin", "gap-ab.bin", "replica-b.bin", "gap-bc.bin", "replica-c.bin"]


def component_describer(inputs: Mapping[str, Any], terminal: TerminalBuild, index: int) -> Any:
    """Name the field at a byte offset of one terminal component of a profile."""
    kind = terminal.plan[index][0]
    info = {"records": len(terminal.components[index]), "block_size": inputs["block_size"],
            "structural_rows": len(inputs["structural_entries"]), "object_rows": len(inputs["object_rows"])}

    def describe(offset: int) -> str:
        record = offset // info["block_size"]
        return f"record {record}: " + describe_terminal(kind, info, record, offset % info["block_size"])

    return describe


def compare_profile(name: str, inputs: Mapping[str, Any], manifest_rows: list[dict[str, str]],
                    candidate_dir: pathlib.Path) -> list[Comparison]:
    terminal = build_terminal_suffix(terminal_inputs_from_profile(inputs))
    results = []
    rows = {row["component"]: row for row in manifest_rows if row["profile"] == name}
    for index, filename in enumerate(PROFILE_COMPONENT_FILES):
        built = b"".join(terminal.components[index])
        candidate = (candidate_dir / filename).read_bytes()
        results.append(compare_bytes(f"{name}/{filename}", built, candidate,
                                     component_describer(inputs, terminal, index)))
        row = rows.get(filename)
        if row is None:
            results.append(Comparison(f"{name}/{filename} [MANIFEST.tsv]", False, "no MANIFEST.tsv row"))
            continue
        built_columns = {
            "block_size": inputs["block_size"],
            "structural_rows": len(inputs["structural_entries"]),
            "object_rows": len(inputs["object_rows"]),
            "replica_records": terminal.replica_records,
            "gap_records": terminal.separation_records,
            "expected_eod_lba": terminal.expected_eod,
            "bytes": len(built),
            "sha256": hashlib.sha256(built).hexdigest(),
            "edition_digest": terminal.edition_digest.hex(),
            "layout_digest": terminal.layout_digest.hex(),
            "payload_sha256": terminal.payload_sha256.hex(),
            "canonical_map_sha256": terminal.canonical_sha256.hex(),
        }
        mismatches = [f"field {column}: built {value}, recorded {row[column]}"
                      for column, value in built_columns.items() if str(value) != row[column]]
        results.append(Comparison(f"{name}/{filename} [MANIFEST.tsv row]", not mismatches,
                                  "; ".join(mismatches) or "reproduced"))
    return results


def encode_typed(value: Mapping[str, Any]) -> bytes:
    """Encode a typed CBOR value from inputs.json, keeping map pairs in the given order."""
    if len(value) != 1:
        raise BuildRefusal("typed value", f"expected one type tag, got {sorted(value)}")
    tag, item = next(iter(value.items()))
    if tag == "uint":
        return _cbor_type_and_length(0, item)
    if tag == "negint":
        return _cbor_type_and_length(1, -1 - item)
    if tag == "text":
        raw = item.encode("utf-8")
        return _cbor_type_and_length(3, len(raw)) + raw
    if tag == "bytes":
        raw = bytes.fromhex(item)
        return _cbor_type_and_length(2, len(raw)) + raw
    if tag == "bool":
        return b"\xf5" if item else b"\xf4"
    if tag == "null":
        return b"\xf6"
    if tag == "array":
        return _cbor_type_and_length(4, len(item)) + b"".join(encode_typed(element) for element in item)
    if tag == "map":
        return _cbor_type_and_length(5, len(item)) + b"".join(
            encode_typed(key) + encode_typed(val) for key, val in item)
    raise BuildRefusal("typed value", f"unknown type tag {tag!r}")


def compare_maximums(inputs: Mapping[str, Any], manifest_rows: list[dict[str, str]],
                     candidate_dir: pathlib.Path) -> list[Comparison]:
    results = []
    rows = {row["artifact"]: row for row in manifest_rows}
    artifacts = inputs["artifacts"]
    for name in ("plaintext-row.slot", "encrypted-row.slot"):
        encoded = encode_typed(artifacts[name]["fields"])
        # The recorded field order must already be the deterministic order.
        decode_deterministic_cbor(encoded)
        slot = encode_slot(encoded, OBJECT_SLOT)
        candidate = (candidate_dir / name).read_bytes()
        results.append(compare_bytes(f"maximums/{name}", slot, candidate,
                                     lambda offset: "encoded_len" if offset < 2 else f"slot byte {offset}"))
        row = rows[name]
        built = {"block_size": row["block_size"], "encoded_len": len(encoded), "bytes": len(slot),
                 "sha256": hashlib.sha256(slot).hexdigest(), "writer_version_len": 0, "write_timestamp_len": 0}
        mismatches = [f"field {column}: built {value}, recorded {row[column]}"
                      for column, value in built.items() if str(value) != row[column]]
        results.append(Comparison(f"maximums/{name} [MAXIMUMS.tsv row]", not mismatches,
                                  "; ".join(mismatches) or "reproduced"))
    name = "bootstrap-footer.bin"
    spec = artifacts[name]
    terminal_inputs = terminal_inputs_from_profile(spec)
    terminal = build_terminal_suffix(terminal_inputs)
    ordinal = spec["replica_ordinal"]
    component_index = {1: 0, 2: 2, 3: 4}[ordinal]
    local = terminal.plan[component_index]
    observation = spec["observation"]
    if (observation["tape_file_number"], observation["start_lba"], observation["record_count"]) != (
            local[2], local[3], local[4]):
        raise BuildRefusal("observation", "Section 8.3: local observations in a footer MUST equal the plan")
    footer = terminal.components[component_index][-1]
    candidate = (candidate_dir / name).read_bytes()
    info = {"records": 1, "block_size": terminal_inputs.block_size, "structural_rows": 1, "object_rows": 0}
    results.append(compare_bytes(f"maximums/{name}", footer, candidate,
                                 lambda offset: describe_terminal(KIND_REPLICA, {**info, "records": 2}, 1, offset)))
    row = rows[name]
    built = {"block_size": terminal_inputs.block_size, "encoded_len": 0, "bytes": len(footer),
             "sha256": hashlib.sha256(footer).hexdigest(),
             "writer_version_len": len(terminal_inputs.writer_version),
             "write_timestamp_len": len(terminal_inputs.write_timestamp)}
    mismatches = [f"field {column}: built {value}, recorded {row[column]}"
                  for column, value in built.items() if str(value) != row[column]]
    results.append(Comparison(f"maximums/{name} [MAXIMUMS.tsv row]", not mismatches,
                              "; ".join(mismatches) or "reproduced"))
    return results


def extension_slot(case: Mapping[str, Any], profiles: Mapping[str, Mapping[str, Any]]) -> tuple[bytes, int]:
    base_profile = profiles[case["base_profile"]]
    row = object_row_from_json(base_profile["object_rows"][case["row_index"]], case["row_index"])
    change = case["change"]
    key = change["integer_key"]
    if key in row:
        raise BuildRefusal("change.integer_key", "the key is already present in the base row")
    pairs = [(encode_deterministic_cbor(existing), encode_deterministic_cbor(value)) for existing, value in row.items()]
    pairs.append((encode_deterministic_cbor(key), encode_typed(change["value"])))
    pairs.sort(key=lambda pair: (len(pair[0]), pair[0]))
    encoded = _cbor_type_and_length(5, len(pairs)) + b"".join(k + v for k, v in pairs)
    slot = bytearray(encode_slot(encoded, OBJECT_SLOT))
    encoded_len = len(encoded)
    edit = case.get("byte_edit")
    if edit is not None:
        offset = edit["slot_offset"]
        old = bytes.fromhex(edit["old_bytes"])
        new = bytes.fromhex(edit["new_bytes"])
        if bytes(slot[offset : offset + len(old)]) != old:
            raise BuildRefusal("byte_edit.old_bytes", "the slot does not hold the recorded old bytes")
        slot[offset : offset + len(new)] = new
        slot[0:2] = bytes.fromhex(edit["new_length_prefix"])
        encoded_len = struct.unpack_from("<H", slot, 0)[0]
    return bytes(slot), encoded_len


def compare_extensions(inputs: Mapping[str, Any], manifest_rows: list[dict[str, str]],
                       profiles: Mapping[str, Mapping[str, Any]]) -> list[Comparison]:
    results = []
    rows = {row["case_id"]: row for row in manifest_rows}
    for case in inputs["cases"]:
        row = rows[case["case_id"]]
        slot, encoded_len = extension_slot(case, profiles)
        candidate = (FIXTURE_ROOT / row["artifact"]).read_bytes()
        results.append(compare_bytes(f"object-row-extensions/{case['case_id']}", slot, candidate,
                                     lambda offset: "encoded_len" if offset < 2 else f"slot byte {offset}"))
        built = {"base_profile": case["base_profile"], "row_index": case["row_index"],
                 "encoded_len": encoded_len, "bytes": len(slot), "sha256": hashlib.sha256(slot).hexdigest()}
        mismatches = [f"field {column}: built {value}, recorded {row[column]}"
                      for column, value in built.items() if str(value) != row[column]]
        results.append(Comparison(f"object-row-extensions/{case['case_id']} [OBJECT_ROW_EXTENSIONS.tsv row]",
                                  not mismatches, "; ".join(mismatches) or "reproduced"))
    return results


def _apply_template(template: Mapping[str, Any], dependent: Mapping[str, Any], index: int) -> dict[str, Any]:
    out = json.loads(json.dumps(template))
    for key, rule in dependent.items():
        if set(rule) != {"i_plus"}:
            raise BuildRefusal("i_dependent_fields", f"unknown rule {rule}")
        out[key] = index + rule["i_plus"]
    return out


def streaming_digests(inputs: Mapping[str, Any]) -> dict[str, Any]:
    """Re-derive the million-row stream's digests in constant memory."""
    block_size = inputs["block_size"]
    initial = inputs["structural_entries"]["initial"]
    repeat = inputs["structural_entries"]["repeat"]
    object_spec = inputs["object_rows"]
    structural_count = len(initial) + repeat["i_end_exclusive"] - repeat["i_start"]
    object_count = object_spec["i_end_exclusive"] - object_spec["i_start"]
    counts = inputs["counts"]
    if counts["structural_entry_count"] != structural_count:
        raise BuildRefusal("counts.structural_entry_count", f"the plan gives {structural_count}")
    if counts["object_row_count"] != object_count:
        raise BuildRefusal("counts.object_row_count", f"the plan gives {object_count}")
    payload_hash = hashlib.sha256(PAYLOAD_DOMAIN)
    canonical_hash = hashlib.sha256(_cbor_type_and_length(4, structural_count))
    structural_passes = 0
    object_passes = 0
    retained = 0
    total = 0
    watermark = 0
    have_sidecar = False
    object_keys: list[tuple[int, int]] = []

    def structural_rows():
        for entry in initial:
            yield entry
        for index in range(repeat["i_start"], repeat["i_end_exclusive"]):
            yield _apply_template(repeat["template"], repeat["i_dependent_fields"], index)

    structural_passes += 1
    expected_number = 0
    object_file_numbers = []
    for entry_json in structural_rows():
        entry = structural_from_json([entry_json])[0]
        if entry.tape_file_number != expected_number:
            raise BuildRefusal("structural_entries", "tape-file numbers are not dense")
        expected_number += 1
        encoded = encode_deterministic_cbor(entry.row())
        canonical_hash.update(encoded)
        payload_hash.update(encode_slot(encoded, STRUCTURAL_SLOT))
        if entry.kind == KIND_OBJECT:
            total = max(total, entry.first_parity_data_ordinal + entry.block_count)
        if entry.kind == KIND_SIDECAR:
            have_sidecar = True
            watermark = max(watermark, entry.protected_ordinal_end_exclusive)
    del object_keys, object_file_numbers
    object_passes += 1
    for index in range(object_spec["i_start"], object_spec["i_end_exclusive"]):
        row_json = _apply_template(object_spec["template"], object_spec["i_dependent_fields"], index)
        row = object_row_from_json(row_json, index)
        payload_hash.update(encode_slot(encode_deterministic_cbor(row), OBJECT_SLOT))
    scope = inputs["scope"]
    if scope["total_data_ordinals"] != total:
        raise BuildRefusal("scope.total_data_ordinals", f"the plan gives {total}")
    if scope["highest_protected_ordinal"] != watermark:
        raise BuildRefusal("scope.highest_protected_ordinal", f"the plan gives {watermark}")
    if scope["covered_prefix_tape_file_count"] != structural_count:
        raise BuildRefusal("scope.covered_prefix_tape_file_count", f"the plan gives {structural_count}")
    del have_sidecar
    payload_len, payload_records, replica_records, separation_records = terminal_geometry(
        structural_count, object_count, block_size, inputs["separation_extent"]["nominal_extent_bytes"])
    layout = inputs["terminal_layout"]
    start_lba = layout["prefix_end_lba"]
    plan, eod = terminal_plan(structural_count, start_lba, replica_records, separation_records)
    if layout["replica_record_count"] != replica_records:
        raise BuildRefusal("terminal_layout.replica_record_count", f"Section 10.4 gives {replica_records}")
    if layout["start_tape_file"] != structural_count:
        raise BuildRefusal("terminal_layout.start_tape_file", f"the plan gives {structural_count}")
    tuples = [component_tuple(*component) for component in plan]
    layout_digest = layout_digest_of(layout["partition"], block_size, tuples, eod)
    payload_sha = payload_hash.digest()
    canonical_sha = canonical_hash.digest()
    diagnostics = inputs.get("diagnostics") or {}
    edition = edition_digest_of(
        bytes.fromhex(inputs["tape_uuid"]), bytes.fromhex(inputs["edition_id"]), inputs["edition_sequence"],
        layout["partition"], block_size, structural_count, total, watermark, structural_count, object_count,
        payload_len, payload_records, payload_sha, canonical_sha,
        diagnostics.get("writer_version", "").encode("ascii"), diagnostics.get("write_timestamp", "").encode("ascii"),
    )
    return {
        "block_size": block_size, "structural_rows": structural_count, "object_rows": object_count,
        "payload_bytes": payload_len, "payload_records": payload_records, "replica_records": replica_records,
        "expected_eod_lba": eod, "structural_passes": structural_passes, "object_passes": object_passes,
        "retained_rows": retained, "payload_sha256": payload_sha.hex(), "canonical_map_sha256": canonical_sha.hex(),
        "edition_digest": edition.hex(), "layout_digest": layout_digest.hex(),
    }


def compare_streaming(inputs: Mapping[str, Any], manifest_rows: list[dict[str, str]]) -> list[Comparison]:
    built = streaming_digests(inputs)
    row = manifest_rows[0]
    results = []
    for column, value in built.items():
        results.append(compare_value(f"STREAMING.tsv {row['vector']}", column, value, row[column]))
    return results


# ---------------------------------------------------------------------------
# The build command.
# ---------------------------------------------------------------------------

IMAGE_NAMES = ["a4-minimal", "short-epoch", "two-epoch", "two-edition", "unfinalized-closed", "unfinalized-open"]
PROFILE_NAMES = ["minimal-256k", "minimal-512k", "minimal-1024k", "multi-256k", "multi-512k", "multi-1024k"]


def load_json(path: pathlib.Path) -> Any:
    return json.loads(path.read_text(encoding="utf-8"))


def load_image_inputs(name: str) -> dict[str, Any]:
    return load_json(FIXTURE_ROOT / "tape-images" / "inputs" / f"{name}.json")


def build_all_images() -> dict[str, ImageBuild]:
    return {name: build_image(load_image_inputs(name), name) for name in IMAGE_NAMES}


def run_build(out_dir: pathlib.Path, include_streaming: bool = True) -> dict[str, Any]:
    results: list[Comparison] = []
    image_manifest = read_tsv(FIXTURE_ROOT / "tape-images" / "MANIFEST.tsv")
    for name in IMAGE_NAMES:
        try:
            image = build_image(load_image_inputs(name), name)
        except BuildRefusal as refusal:
            results.append(Comparison(f"{name}", False, f"build refused: field {refusal.field_name}: {refusal.message}"))
            continue
        results.extend(compare_image(image, image_manifest))
    profile_manifest = read_tsv(FIXTURE_ROOT / "MANIFEST.tsv")
    profiles = {}
    for name in PROFILE_NAMES:
        profiles[name] = load_json(FIXTURE_ROOT / name / "inputs.json")
        try:
            results.extend(compare_profile(name, profiles[name], profile_manifest, FIXTURE_ROOT / name))
        except BuildRefusal as refusal:
            results.append(Comparison(name, False, f"build refused: field {refusal.field_name}: {refusal.message}"))
    results.extend(compare_maximums(load_json(FIXTURE_ROOT / "maximums" / "inputs.json"),
                                    read_tsv(FIXTURE_ROOT / "MAXIMUMS.tsv"), FIXTURE_ROOT / "maximums"))
    results.extend(compare_extensions(load_json(FIXTURE_ROOT / "object-row-extensions" / "inputs.json"),
                                      read_tsv(FIXTURE_ROOT / "OBJECT_ROW_EXTENSIONS.tsv"), profiles))
    if include_streaming:
        results.extend(compare_streaming(load_json(FIXTURE_ROOT / "streaming-inputs.json"),
                                         read_tsv(FIXTURE_ROOT / "STREAMING.tsv")))
    report = {
        "schema": "rem-parity-second-implementation-build/1",
        "results": [result.as_json() for result in results],
        "reproduced": sum(1 for result in results if result.reproduced),
        "mismatched": sum(1 for result in results if not result.reproduced),
    }
    out_dir.mkdir(parents=True, exist_ok=True)
    (out_dir / "build-report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    return report


# ---------------------------------------------------------------------------
# The Reader. Everything from here on sees only the damaged record stream,
# its filemarks and EOD, the case's hints and its failed data addresses.
# ---------------------------------------------------------------------------

# Sentences the decisions cite, quoted from REM-PARITY. The test suite checks
# that every one of them occurs in the specification text.
QUOTES: dict[str, tuple[str, str]] = {
    "boot_first": ("8.4", "A Scanner with no off-tape state first reads the bootstrap at BOT to establish tape identity, block size, and parity geometry."),
    "candidates": ("8.4", "When the block size is unknown, a Scanner MUST apply each discovery candidate (256 KiB, 512 KiB, 1 MiB) as a real drive reconfiguration before reading."),
    "accept_size": ("8.4", "It accepts a parsed bootstrap only if its `block_size_bytes` equals the configured read size."),
    "no_bootstrap": ("15", "NoBootstrapFound                bootstrap is absent or invalid"),
    "hint_discovery": ("8.4", "A Scanner that cannot read the bootstrap, and is given the three values Section 8.4.1 names, MUST perform the discovery above with them."),
    "hint_uuid": ("8.4", "It then uses the supplied tape UUID as the key of the role magics (Section 5.2)."),
    "hints_required": ("8.4.1", "If it is unreadable, the expected tape UUID, the block size and the parity scheme (`k`, `m` and `S` of `rs-cauchy-gf256-v1`, or no parity) supplied out of band are required."),
    "hint_size": ("8.4.1", "A block-size hint makes the size known and is applied as a configured read size under the Section 8.4 hint path, suppressing candidate rotation."),
    "identity_12_1": ("12.1", "The mounted tape's identity comes from the bootstrap or, when the bootstrap is unreadable, from the tape UUID supplied under Section 8.4.1, and a finalized tape's selected terminal replica supplies the authoritative inventory."),
    "locate_footer": ("8.4", "locate, from EOD, the local footer of a terminal replica, and from it the planned terminal layout (Section 8.3);"),
    "validate_every": ("8.4", "validate the header, footer and trailing filemark of every replica that layout plans; a replica's payload need be validated only for the replica to be accepted, or when two replica envelopes differ in an edition-common field;"),
    "consider_conflict": ("8.4", "before accepting any replica, consider every replica that validates for conflict under Section 8.5;"),
    "no_replica_walk": ("8.4", "if no replica validates, perform the BOT structural recovery walk in Section 8.4.1."),
    "medium_error": ("8.4", "A medium error invalidates the affected candidate, while a non-medium transport error aborts discovery."),
    "magic_crc_miss": ("8.4", "A magic or CRC miss invalidates that candidate."),
    "locate_planned": ("8.4", "A Reader locates the frame from the immutable planned tuple, checks device-reported post-read positions for the addressed records and trailing filemark, and cross-checks the footer observation against that tuple."),
    "footer_local": ("8.4", "Footer-local observed positions MUST agree with the footer's planned shared layout before that replica is eligible."),
    "fully_valid": ("8.5", "A terminal replica is *fully valid* when it is locally eligible under Section 10.6."),
    "conflict_rule": ("8.5", "A Scanner MUST NOT accept a replica while another fully valid replica differs from it in any edition-common field: any row of kind *common* in the replica frame of Section 10.4, which together are the replicas' *edition* (Section 2.3)."),
    "conflict_error": ("8.5", "A disagreement in any edition-common field is `TerminalIndexReplicaConflict` and is never resolved by ordinal preference; a Scanner MUST NOT choose one side of a conflict merely because it is newer in the terminal suffix."),
    "degraded_evidence": ("8.5", "A missing or invalid replica is degraded evidence, not a conflict, and does not invalidate an agreeing survivor."),
    "select_walk": ("8.5", "If no replica validates, selection yields the explicit BOT structural recovery path of Section 8.4.1."),
    "eligible_all": ("10.6", "A Scanner MUST NOT treat a terminal replica as locally eligible unless every condition below holds."),
    "device_agree": ("10.6", "The declared locations, the counts, and the trailing filemark agree with the device's measurements."),
    "payload_valid": ("10.6", "Every fixed slot of the payload decodes, and the dense map, scope, deterministic CBOR, zero padding, exact map↔Object-row bijection, payload digest, and canonical-map digest validate."),
    "role_magic": ("10.6", "The header and footer role magics match the tape; matching magic with malformed content is a typed control failure, never an Object."),
    "bot_required": ("12.4", "If A, B, and C all fail, the Scanner returns an explicit `BotStructuralRecoveryRequired` outcome and performs the Section 8.4.1 walk; BOT Object classifications are likewise streamed before their terminal summary."),
    "survivor": ("12.6", "On a tape presented without host state, a validating C/B/A survivor attests the complete terminal inventory carried in that replica."),
    "degraded_result": ("12.6", "Missing or invalid siblings make the result degraded; disagreement makes it a conflict."),
    "walk_evidence": ("12.6", "No valid replica invokes the explicit BOT structural walk, whose result is recovery evidence rather than a fabricated terminal edition."),
    "walk_offer": ("8.4.1", "When A, B, and C are all absent or invalid, the Scanner MUST offer a full structural walk from BOT."),
    "identity_unknown": ("8.4.1", "It reports each candidate's identity as unknown unless a separate exact Object-recovery authority succeeds."),
    "authority_not_recovered": ("8.4.1", "The Scanner MUST report that terminal authority was not recovered."),
    "walk_file": ("12.2", "Per tape file: read the head block; measure the file's length by filemark spacing (space to the next filemark; the file's block count is the position delta minus one); a zero-block file or a missing trailing filemark is structural damage; EOD at a file start ends the walk."),
    "unreadable_head": ("12.3", "It MUST measure the file by filemark spacing, run the footer/tail sidecar probe, and otherwise classify the file as an object candidate."),
    "replica_footer_type": ("12.3", "When the head is unreadable, a matching, fully parsed terminal footer establishes the same type, after its measured count is checked."),
    "separation_footer_type": ("12.3", "When the head is unreadable, a matching, fully parsed footer establishes the type."),
    "ladder_bootstrap": ("12.3", "**Bootstrap**: the fixed magic matches, the full frame parses, the frame's `block_size_bytes` equals the read size, the frame's `tape_uuid` equals the tape identity of Section 12.1, and the file measures exactly 1 block."),
    "ladder_replica": ("12.3", "**TapeIndexReplica**: matching terminal-replica header magic commits the tape file to this control type."),
    "ladder_separation": ("12.3", "**IndexSeparationExtent**: matching separation-header magic commits the tape file to this control type under the same malformed-control and measured count rules."),
    "ladder_parity_map": ("12.3", "**ParityMap**: a complete copy/header and payload validate, and the measured block count agrees with its locator footer."),
    "ladder_sidecar": ("12.3", "**Sidecar (primary)**: the primary header parses, and the measured block count MUST equal the header's `sidecar_total_block_count`."),
    "footer_probe": ("12.3", "If it parses as a sidecar footer, the footer's total MUST equal the measured block count, and the Scanner verifies the tail header copy against the footer, field for field."),
    "ladder_object": ("12.3", "**Object, by elimination** — never by reading object content."),
    "item7": ("12.3", "Item 7 applies to a tape file that none of items 1 to 6 recognises."),
    "hard_error_scope": ("12.3", "In items 2 through 6, a count-mismatch “hard error” is scoped to that rung and that tape file's classification: the Scanner reports the failed classification and continues the walk with the next tape file."),
    "second_pass": ("12.3", "After the walk, when the tape's final ParityMap validates and is marked `is_final_directory`, the Scanner MUST perform a second pass that identifies as a sidecar each Object candidate at a tape file a directory entry names whose measured length equals the entry's `sidecar_total_block_count`."),
    "walk_validated": ("13.1", "A map produced by the Section 8.4.1 walk, after the second pass of Section 12.3, is validated against the tape's final ParityMap when that ParityMap validates and is marked `is_final_directory`, and the map's canonical projection (Section 7.3) through the ParityMap's own entry hashes to its `canonical_map_digest`."),
    "walk_scope": ("13.1", "The validated scope and durable boundary are the first `directory_scope_tape_file_count` tape files, ending with that ParityMap's entry; `W` is `directory_scope_highest_protected_ordinal`."),
    "walk_no_map": ("13.1", "A walked map whose projection does not hash to a validated final ParityMap's `canonical_map_digest`, or whose prefix disagrees with those scope fields, is not validated and gives the Recoverer no map, with no fallback to the bootstrap's scope."),
    "reconstruct_error": ("15", "FilemarkMapReconstruct          BOT recovery walk could not produce a valid map"),
    "recoverer_inputs": ("13.1", "A validated, scoped map (Section 12); the bootstrap's scheme record, or the scheme supplied out of band when the bootstrap is unreadable (Section 8.4.1); and the failed addresses — `(tape_file_number, object_block_index)` pairs or ordinals."),
    "refusals": ("13.2", "The Recoverer MUST reject, as typed refusals distinct from recovery failures: ordinals outside the validated scope (`OutsideValidatedMapPrefix`); ordinals ≥ `W` — the pending epoch, whose parity does not exist yet (`UnrecoverablePendingEpoch`); and failed blocks or sidecars in tape files outside the durable boundary."),
    "stripe_map": ("3.3", "For scheme `(k, m, S)` (Section 6.6), locate the unique sidecar descriptor whose explicit range `[start, end)`, of at most `S × k` data ordinals, contains ordinal `o`."),
    "footer_first": ("13.3", "If it parses as the epoch's footer and its total matches the map entry: read and verify **both** header copies against the footer locator, including the `canonical_metadata_hash`; use the primary if valid, else the tail; record copy health (both-usable / tail-lost / primary-lost)."),
    "primary_fallback": ("13.3", "If the footer is unreadable, unparseable, **or inconsistent with the map entry** — a footer that parses but contradicts the map is treated as an invalid footer, not as a hard stop — fall back to the primary header at block 0 (copy-kind and map-entry block-count cross-checks apply)."),
    "tail_rescue": ("13.3", "If the primary also fails and a sidecar epoch directory entry is available: locate the tail copy at block `sidecar_total_block_count − 1 − sidecar_header_block_count` using the entry's counts, and verify its `canonical_metadata_hash` against the entry."),
    "entry_available": ("13.3", "An entry is available whenever the tape's final ParityMap validates and is marked `is_final_directory`, reached through a validated replica's structural rows or found by the Section 8.4.1 walk."),
    "entry_agrees": ("13.3", "A Recoverer MUST NOT place a read from a directory entry unless the entry agrees with the sidecar's map entry in tape file, epoch, protected range and block count."),
    "metadata_unavailable": ("13.3", "Only when no header/index copy can be validated is the epoch **metadata-unavailable** — and only that epoch (Section 12.5)."),
    "pin_scheme": ("13.3", "The acquired index MUST then be pinned against the bootstrap's scheme record (`k`, `m`, `S`, block size), or against the supplied scheme and block size when the bootstrap is unreadable, and against the map entry's ordinal range; disagreement is `SchemeMismatch`."),
    "trusted": ("13.4", "**Trusted shard**: the read succeeded, AND its CRC-64 matches the sidecar index (data CRC for data peers, parity CRC for parity peers), AND — for data peers — its object tape file is inside the durable boundary (in catalog-less recovery, inside the validated map scope; Section 13.2)."),
    "erasure": ("13.4", "**Erasure**: a read failure, a CRC mismatch, or a position outside the durable boundary."),
    "implicit": ("13.4", "**Implicit zero**: an ordinal ≥ `protected_ordinal_end_exclusive` — an all-zero shard supplied without tape I/O; not an erasure (Section 6.4)."),
    "reconstruct": ("13.5", "Reconstruct per Section 6.5 from the first `k` trusted or implicit shards (data shards first in index order, then parity)."),
    "unrecoverable": ("13.5", "More than `m` erasures in a stripe is unrecoverable — a typed result carrying the stripe and the counts (`Unrecoverable{stripe, lost_count, limit}`)."),
    "verify_crc": ("13.5", "Every reconstructed data block MUST be verified against its sidecar data CRC before release."),
    "epoch_isolation": ("12.5", "Damage confined to one sidecar's metadata — any or all of its header copies, its footer, its directory entry's health flags — MUST NOT degrade classification, mapping, digest validation, or recovery of any other epoch."),
    "verifier_role": ("2.2", "**Verifier**: validates a tape's structures and digests end to end without recovering payload — the Scanner's checks plus the Recoverer's index and CRC validation."),
    "verifier_separation": ("10.6", "A Verifier that finds a separation extent invalid MUST report it and MUST NOT report the terminal suffix as complete."),
    "verifier_interior": ("10.6", "A Verifier performing full verification MUST check that every interior byte of both separation extents is zero."),
    "separation_edition": ("10.6", "Its edition ID MUST equal the replicas'."),
    "separation_not_needed": ("10.6", "A Scanner that reads only the replicas to return an inventory need not read the separation extents."),
    "normal_finalized": ("12.6", "A normal finalized tape has the exact terminal suffix of Section 8.3 and EOD immediately after C's trailing filemark."),
    "fill_verifier": ("8.1", "Verifiers MUST verify it and report a nonzero fill as a nonconformity."),
    "err_metadata_unavailable": ("15", "SidecarMetadataUnavailable{epoch_id}   no header/index copy validated (Section 13.3)"),
    "err_scheme": ("15", "SchemeMismatch                  sidecar geometry disagrees with the bootstrap or supplied scheme"),
    "err_conflict": ("15", "TerminalIndexReplicaConflict    independently valid survivors disagree"),
    "err_bot": ("15", "BotStructuralRecoveryRequired   no terminal replica validates; explicit BOT walk required"),
    "err_tapeio": ("15", "TapeIo                          transport/medium failure (not a format violation)"),
    "err_digest_mismatch": ("15", "FilemarkMapDigestMismatch       replica structural projection digest mismatch, or a walked map's"),
    "io_distinct": ("15", "Refusals (Section 13.2), parse failures, and reconstruction failures MUST remain distinguishable; I/O faults MUST remain distinct from format violations."),
    "sidecar_copy_agree": ("9.1", "When both copies are valid and their canonical metadata hashes (Section 9.5) differ, a Recoverer MUST use a copy that the footer or the sidecar epoch directory vouches for and neither contradicts, and MUST reject both when neither is available (Section 13.3)."),
    "parity_locator": ("9.1", "A Reader MUST locate a parity shard by computing `block(stripe, parity_index)` from its explicit fields, never by the position of its index entry."),
    "pm_copies": ("10.1.3", "When one header copy is invalid and the other is valid, the valid copy is used; when both are valid they MUST agree (Section 10.1.4)."),
    "pm_crc": ("10.1.3", "A Reader MUST verify the CRC-64/XZ at 0xC0 of each header copy and of the footer, and MUST NOT rely on a block whose CRC does not verify."),
    "tt2_walk_pm": ("D", "The walk route cannot find a ParityMap whose block 0 is unreadable; the replica route survives that damage by locating it through structural rows."),
    "selection_guide": ("C", "the selection order among agreeing replicas"),
    "bootstrap_supplied_refused": ("8.4", "A readable bootstrap whose values disagree with the supplied ones is refused; the supplied values never take its place."),
    "hint_walk_accept": ("8.4.1", "A Scanner MUST accept an expected tape UUID,"),
}



# Sentences of the revised text that the decisions cite (the F0 revision:
# Sections 2.2, 2.4, 3.3, 3.5, 7.2, 8.4, 9.1, 9.6, 10.1.3, 10.1.4, 10.6,
# 12.3, 12.6, 13.3-13.5, 14, 15, 16.3 and Appendix D).
QUOTES.update({
    'verifier_divergence': ('9.1', "A Verifier MUST read every sidecar's primary copy, tail copy and footer, and MUST report a divergence between the copies as `SidecarParse`, even when the footer or the directory decides it."),
    'footer_decides': ('13.3', 'Such a footer is valid, and it decides, unless an available sidecar epoch directory entry (step 3) disagrees with it: a copy it contradicts is damaged, and the directory-assisted rescue of step 3 is not used.'),
    'footer_entry_disagree': ('13.3', "When an available entry disagrees with a valid footer, on the canonical metadata hash or on the tail copy's position, neither decides: a copy that either contradicts is not used."),
    'entry_decides': ('13.3', "When a sidecar epoch directory entry is available (step 3), it decides as the footer would: a copy is used only if its `canonical_metadata_hash` equals the entry's."),
    'read_tail_too': ('13.3', "When no entry is available, a Recoverer whose primary copy validates MUST also read the tail copy at block `H + P`, where `H` is the primary's `sidecar_header_block_count` and `P = S × m`."),
    'tail_differs_unavailable': ('13.3', "A valid tail copy whose canonical metadata hash (Section 9.5) differs from the primary's leaves nothing to decide between them, and the epoch is metadata-unavailable."),
    'rescue_preconditions': ('13.3', "The rescue also requires the entry's `sidecar_total_block_count` to equal `2H + P + 1`, with `H > 0`. An entry that fails either precondition is not available."),
    'rescue_block': ('13.3', "For an entry that meets the preconditions below, that block is `H + P`, where `H` is the entry's `sidecar_header_block_count` and `P = S × m`."),
    'unavailable_report': ('13.3', "The Recoverer then reports `SidecarMetadataUnavailable` for the epoch, whatever each copy's own failure was."),
    'exact_values': ('2.4', 'Every formula in this document denotes its exact integer value.'),
    'reject_if_not_fit': ('2.4', 'A Reader rejects a value that is recorded, or used as a position, a count or an ordinal, when its exact value does not fit the field or the u64 that holds it; the error is the one named for the structure that carries the value.'),
    'compare_exact': ('2.4', 'A value that is only compared is compared exactly, and is never too large.'),
    'intermediate_ok': ('2.4', 'An intermediate overflow in one way of writing a formula is not a format violation, and a Reader MUST NOT reject for it.'),
    'offtape_device': ('2.4', 'The same rule applies to values from off-tape commit records, which are `ResumeAppend` when they do not fit (Sections 3.4 and 14), and to positions a device reports, which are `TapeIo` when they do not fit or when they go backwards: the device, not the tape, is then at fault.'),
    'inverse_exact': ('3.3', 'That decision compares the exact value of `o` with the protected end, as Section 2.4 requires, so a position whose `o` would not fit in u64 is an implicit zero, and is not rejected.'),
    'wrong_length': ('3.5', 'A Reader treats it as invalid content of the component it belongs to: an invalid candidate for a control component, an erasure when the Recoverer reads a data block or parity shard (Section 13.4), and, for the first record with a supplied or known block size, the refusal of Sections 8.4 and 15.'),
    'wrong_length_never_tapeio': ('3.5', 'It is never `TapeIo`, which is reserved for a transport or medium failure and for the device reports that Section 2.4 names.'),
    'positions_fit': ('7.2', "Every record position and every trailing filemark position that the map describes, by Section 3.2's `LBA(f, b)`, MUST fit in u64; a recorded map that breaks this is invalid."),
    'walked_positions': ('7.2', "A walked map's positions are device reports, so for it Section 2.4's `TapeIo` applies."),
    'step1_spacing': ('8.4', 'the Scanner spaces back over up to five filemarks from EOD and reads the record before each, stopping early when a backspace does not cross exactly one filemark or leaves the partition.'),
    'footer_supplies': ('8.4', "A terminal replica's footer that parses supplies a planned layout only when its recorded footer position equals the position at which it was read, and the layout's planned EOD is at or after the tape's EOD."),
    'layout_before_eod': ('8.4', "A layout whose planned EOD lies before the tape's EOD may be followed by later tape files, so it is not used, and when no footer supplies a layout, no replica validates;"),
    'first_record_stages': ('8.4', 'With supplied values, the Scanner judges the first record in two stages: the physical read first and then, for a record of the right length, its content.'),
    'first_record_unreadable': ('8.4', '| a medium error, or a filemark or EOD where the record should be | treats the bootstrap as unreadable and continues on the supplied values |'),
    'first_record_transport': ('8.4', '| a transport failure | reports `TapeIo` |'),
    'first_record_length': ('8.4', '| a record whose measured length differs from the supplied block size, shorter or longer | refuses it (`BootstrapParse`, naming the block size) |'),
    'first_record_magic': ('8.4', '| the magic is missing, or the header CRC fails | treats the bootstrap as unreadable |'),
    'first_record_fields': ('8.4', '| the header CRC is valid, and the format major, tape UUID, block size, sequence (which Section 8.1 fixes at 0) or no-parity flag is impossible or disagrees with the supplied values | refuses it (`BootstrapParse`, naming the first such field in that order), even when the payload is damaged |'),
    'first_record_payload': ('8.4', "| the header is valid, and the payload CRC fails or the payload's length runs past the block | treats the bootstrap as unreadable |"),
    'first_record_later_rule': ('8.4', '| both CRCs are valid, and the payload breaks a later rule of Section 8 | treats the bootstrap as unreadable, unless a value that can still be decoded disagrees, as in the next two rows |'),
    'first_record_compression': ('8.4', '| a decodable `drive_compression` is true, and the tape is a parity tape | refuses it (`DriveCompressionEnabled`) |'),
    'first_record_scheme': ('8.4', '| a decodable parity scheme disagrees with the supplied scheme | refuses it (`BootstrapParse`, naming the scheme) |'),
    'unreadable_untrusted': ('8.4', 'A bootstrap that is treated as unreadable is not trusted for any value.'),
    'recognises': ('12.3', 'For items 1, 4, 5 and 6, a rung *recognises* a tape file only when the file passes every check of that rung, its measured block count included; a file that fails one of them is not recognised.'),
    'failed_reports': ('12.3', 'A rung that parses a file but fails its measured count reports the failed classification.'),
    'item5_decides': ('12.3', "When a sidecar's primary header parses, item 5 decides the sidecar rung for that file: if its count fails, item 6 is not tried."),
    'not_recognised_item7': ('12.3', 'Under items 2 and 3 the file keeps its control type, damaged; under items 4, 5 and 6, as under item 1, the file is not recognised, and item 7 applies.'),
    'tail_probe_serves': ('12.3', 'The last block it reads for that probe also serves the terminal footer probe of items 2 and 3.'),
    'degraded_def': ('12.6', 'An inventory is *degraded* when the Scanner found a replica of the planned layout missing or invalid.'),
    'unread_payload': ('12.6', "A Scanner need not read every replica's payload (Section 8.4 step 2), so a replica whose payload it did not read is not found invalid for that reason; a Verifier's full check reports every replica."),
    'lost_count_def': ('13.5', '`lost_count` is the number of erasures in the stripe, including the failed block, and `limit` is `m`.'),
    'resume_finalizing': ('14', "A committed prefix that records a terminal component or a final ParityMap describes a tape whose finalization has begun, and Section 3.4's transition to `Finalizing` permanently disables Object admission, so it is refused as `ResumeAppend` too."),
    'resume_step3_errors': ('14', 'A read that finds a filemark, EOD or a record shorter or longer than one block where the committed prefix places a data block contradicts the commit record, and is `ResumeAppend`; a medium or transport failure is `TapeIo`.'),
    'boot_names_level': ('15', 'The bootstrap names depend on the level at which a Reader meets the block.'),
    'boot_parser_name': ('15', 'A parser given one block as the bootstrap reports a breach of Section 8 as `BootstrapParse`, including a missing magic and a failed CRC, except that a parity bootstrap recording drive compression is `DriveCompressionEnabled`.'),
    'boot_discovery_name': ('15', 'Discovery over the candidate block sizes, without supplied values, reports `NoBootstrapFound` when it finds no usable bootstrap at any candidate, and `DriveCompressionEnabled` when a parity bootstrap records compression; a record of another length only rules out that candidate.'),
    'boot_known_size': ('15', 'Discovery with only a known block size is discovery over that one candidate, except that a record of another length, or a bootstrap whose recorded block size differs from it, is `BootstrapParse`.'),
    'boot_too_large_writer': ('15', '`BootstrapPayloadTooLarge` is a Writer error.'),
    'replica_parse_positions': ('15', '`TerminalIndexReplicaParse` includes a replica map that describes a position that does not fit (Section 7.2).'),
    'verifier_reports': ('2.2', 'It reports damage it finds before the terminal suffix with the error a Reader reports for that component.'),
    'size_exact': ('10.6', 'As Section 2.4 defines it, a formula evaluates without overflow when its exact value fits in u64.'),
    'w_t_recorded': ('10.6', 'both are the recorded fields, and Section 7.4 separately requires each to equal the value recomputed from the map;'),
    'pm_block_size': ('10.1.3', "`block_size` MUST equal the tape's block size, which is the length of every block read, as the sidecar header's `block_size` must (Section 9.2)."),
    'pm_agreement_all': ('10.1.4', 'The agreement between a header copy and the footer covers every field that both carry, other than `copy_kind` and the CRC, not only the locator fields.'),
    "err_drive_compression": ("15", "DriveCompressionEnabled         compression detected on / recorded for a parity tape"),
    "compression_rejects": ("8.4", "`drive_compression = true` on a parity bootstrap still rejects the tape (Sections 11.4, 16.3)."),
    "no_parity_may_omit": ("8.2", "It MAY omit the scheme record (key 1) and the digest record (key 2). A Reader MUST NOT require those records on it."),
    "compression_key_rule": ("8.2", "`true` on a parity bootstrap MUST be rejected (Sections 8.4, 11.4)"),
    "canonical_reject": ("5.3", "When a Reader decodes any CBOR item of this format, it MUST reject duplicate keys and non-canonical encoding, and MUST ignore unknown integer keys at every map level."),
    'footer_uuid': ('9.6', "The footer's `tape_uuid` MUST match the bootstrap or, when the bootstrap is unreadable, the tape UUID supplied under Section 8.4.1, as each header copy's `tape_uuid` must (Section 9.2)."),
    'short_read_failure': ('13.4', 'A record shorter or longer than one block is a read failure (Section 3.5).'),
    'compression_parity_only': ('16.3', 'Both sentences concern a parity tape (Section 11.4): a no-parity bootstrap may record compression.'),
    'full_verification_open': ('D', 'Whether a full verification checks every data block and parity shard, or only structure and metadata, is to be decided, and the reference brought into line, before freeze.'),
    'tail_route_open': ('D', "One stays open: whether a Recoverer may locate a sidecar's tail copy from its structural row when the ParityMap and the sidecar's primary header and footer are all unreadable (Section 13.3; `parity-map-and-sidecar`)."),
})


def cite(key: str) -> dict[str, str]:
    section, quote = QUOTES[key]
    return {"section": section, "quote": " ".join(quote.split())}


class MediumError(Exception):
    """A READ that began at an unreadable record failed and returned no data."""


FILEMARK = "filemark"
END_OF_DATA = "end of data"


class DamagedTape:
    """The case's fault model applied to a record stream (bytes or None for a filemark)."""

    def __init__(self, records: list[Any], unreadable: set[int]) -> None:
        self._records = records
        self._unreadable = set(unreadable)

    def eod(self) -> int:
        return len(self._records)

    def read(self, lba: int) -> Any:
        if lba >= len(self._records):
            return END_OF_DATA
        if lba in self._unreadable:
            raise MediumError(lba)
        record = self._records[lba]
        return FILEMARK if record is None else record

    def read_data(self, lba: int) -> bytes:
        """Read one data record; a filemark or EOD here is a failed read of data."""
        value = self.read(lba)
        if not isinstance(value, bytes):
            raise ReadFailure("TapeIo", f"LBA {lba}: expected a data block, found {value}")
        return value

    def next_filemark(self, lba: int) -> int | None:
        """Space forward to the next filemark (positioning is unaffected by damage)."""
        for position in range(lba, len(self._records)):
            if self._records[position] is None:
                return position
        return None

    def filemark_positions_before(self, lba: int) -> list[int]:
        """Space backward: filemark positions below lba, nearest first."""
        return [p for p in range(min(lba, len(self._records)) - 1, -1, -1) if self._records[p] is None]

    def is_filemark(self, lba: int) -> bool:
        """Whether the position holds a filemark (a positioning fact, unaffected by damage)."""
        return 0 <= lba < len(self._records) and self._records[lba] is None


class ReadFailure(Exception):
    def __init__(self, error: str, reason: str) -> None:
        super().__init__(f"{error}: {reason}")
        self.error = error
        self.reason = reason


U64_MAX = (1 << 64) - 1


def overflows(*values: int) -> bool:
    """Section 2.4: arithmetic on values read from tape is checked; outside u64 is rejection."""
    return any(value < 0 or value > U64_MAX for value in values)


def rd16(data: bytes, offset: int) -> int:
    return struct.unpack_from("<H", data, offset)[0]


def rd32(data: bytes, offset: int) -> int:
    return struct.unpack_from("<I", data, offset)[0]


def rd64(data: bytes, offset: int) -> int:
    return struct.unpack_from("<Q", data, offset)[0]


def decode_integer_keyed_map(data: bytes, label: str, error: str) -> dict[Any, Any]:
    try:
        value = decode_deterministic_cbor(data)
    except (RederivationError, AssertionError, TypeError) as failure:
        raise ReadFailure(error, f"{label}: {failure}") from failure
    if not isinstance(value, dict):
        raise ReadFailure(error, f"{label}: not a map")
    return value


def is_uint(value: Any, limit: int = (1 << 64) - 1) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and 0 <= value <= limit


# ----- bootstrap -----------------------------------------------------------


def parse_bootstrap(block: bytes, read_size: int) -> dict[str, Any]:
    if len(block) < 0x40:
        raise ReadFailure("NoBootstrapFound", "block shorter than 0x40")
    if block[0:8] != BOOTSTRAP_MAGIC_BYTES:
        raise ReadFailure("NoBootstrapFound", "bootstrap magic does not match")
    if crc64_xz(block[0:0x30]) != rd64(block, 0x30):
        raise ReadFailure("NoBootstrapFound", "header CRC does not verify")
    major, minor, flags = struct.unpack_from(">HHI", block, 0x08)
    if major != 2:
        raise ReadFailure("BootstrapParse", f"schema_major {major}")
    payload_len = rd32(block, 0x2C)
    if 0x38 + payload_len + 8 > len(block):
        raise ReadFailure("BootstrapParse", "payload bounds exceed the block")
    payload = block[0x38 : 0x38 + payload_len]
    if crc64_xz(payload) != rd64(block, 0x38 + payload_len):
        raise ReadFailure("NoBootstrapFound", "payload CRC does not verify")
    values = decode_integer_keyed_map(payload, "bootstrap payload", "BootstrapParse")
    tape_uuid = block[0x10:0x20]
    block_size = struct.unpack_from(">I", block, 0x20)[0]
    sequence = struct.unpack_from(">Q", block, 0x24)[0]
    if block_size != read_size:
        raise ReadFailure("NoBootstrapFound", "block_size_bytes differs from the configured read size")
    if sequence != 0:
        raise ReadFailure("BootstrapParse", "sequence is not 0")
    for reserved in (20, 21, 30):
        if reserved in values:
            raise ReadFailure("BootstrapParse", f"reserved key {reserved} present")
    no_parity = bool(flags & 1)
    scheme = None
    if not no_parity:
        record = values.get(1)
        if not isinstance(record, dict) or record.get(1) != SCHEME_IDENTIFIER:
            raise ReadFailure("BootstrapParse", "scheme record missing or scheme_id wrong")
        k, m, stripes = record.get(2), record.get(3), record.get(4)
        if not all(is_uint(v) for v in (k, m, stripes)) or not (
                k >= 2 and 1 <= m <= k and stripes >= 1 and k + m <= 255 and stripes * (k + m) <= 2**32 - 1):
            raise ReadFailure("BootstrapParse", "scheme triple invalid")
        scheme = (k, m, stripes)
        digest_record = values.get(2)
        if not isinstance(digest_record, dict):
            raise ReadFailure("BootstrapParse", "digest record missing")
    compression = values.get(5, False)
    if compression is True and not no_parity:
        raise ReadFailure("DriveCompressionEnabled", "drive_compression true on a parity bootstrap")
    fill_zero = all(byte == 0 for byte in block[0x38 + payload_len + 8 :])
    return {"tape_uuid": tape_uuid, "block_size": block_size, "scheme": scheme, "minor": minor,
            "flags": flags, "fill_zero": fill_zero}


# ----- terminal frames -----------------------------------------------------


def parse_tuples(data: bytes) -> list[tuple[int, int, int, int, int, int]]:
    return [struct.unpack_from("<HHIQQQ", data, 32 * index) for index in range(5)]


def check_layout(tuples: list[tuple[int, int, int, int, int, int]], eod: int) -> str | None:
    """Section 8.3 planned-layout rules; returns a reason on failure."""
    order = [(4, 1), (5, 1), (4, 2), (5, 2), (4, 3)]
    for index, (kind, ordinal, filemarks, tape_file, start, records) in enumerate(tuples):
        if (kind, ordinal) != order[index]:
            return "component order or ordinal"
        if filemarks != 1:
            return "trailing filemark count"
        if records < 2:
            return "component with fewer than two records"
        if index:
            previous = tuples[index - 1]
            if overflows(previous[3] + 1, previous[4] + previous[5] + 1):
                return "a start-position formula's exact value exceeds u64, so no recorded position equals it"
            if tape_file != previous[3] + 1:
                return "tape-file numbers not dense"
            if start != previous[4] + previous[5] + 1:
                return "start LBA does not advance by record_count + 1"
    if not (tuples[0][5] == tuples[2][5] == tuples[4][5]):
        return "replica record counts differ"
    if tuples[1][5] != tuples[3][5]:
        return "separation record counts differ"
    if overflows(tuples[4][4] + tuples[4][5] + 1):
        return "the EOD formula's exact value exceeds u64, so the recorded EOD cannot equal it"
    if eod != tuples[4][4] + tuples[4][5] + 1:
        return "planned EOD is not C's start plus its records plus one"
    return None


def printable_text(data: bytes) -> bool:
    return all(0x20 <= byte <= 0x7E for byte in data)


REPLICA_COMMON = [
    (0x010, 16), (0x020, 16), (0x030, 8), (0x040, 4), (0x048, 8), (0x050, 8), (0x058, 8), (0x060, 8),
    (0x068, 8), (0x070, 8), (0x078, 8), (0x080, 8), (0x098, 8), (0x0A0, 8), (0x0A8, 32), (0x0C8, 32),
    (0x0E8, 32), (0x108, 32), (0x148, 160), (0x1F8, 128), (0x278, 64),
]


def replica_common_fields(frame: bytes) -> bytes:
    return b"".join(frame[offset : offset + length] for offset, length in REPLICA_COMMON)


def parse_replica_frame(record: bytes, tape_uuid: bytes, block_size: int, role: int) -> dict[str, Any]:
    """Frame-level Section 10.4/10.6 checks for one replica header or footer record."""
    if len(record) != block_size:
        raise ReadFailure("TerminalIndexReplicaParse", "record length")
    frame = record[:REPLICA_FRAME_LEN]
    label = LABEL_REPLICA_HEADER if role == 1 else LABEL_REPLICA_FOOTER
    if frame[0:8] != role_magic(tape_uuid, label):
        raise ReadFailure("TerminalIndexReplicaParse", "role magic does not match")
    if crc64_xz(frame[:0x3F8]) != rd64(frame, 0x3F8):
        raise ReadFailure("TerminalIndexReplicaParse", "frame CRC does not verify")
    problems = []
    if rd16(frame, 0x008) != 1:
        problems.append("schema version")
    if rd16(frame, 0x00A) != role:
        problems.append("role")
    if rd32(frame, 0x00C) != 1:
        problems.append("flags")
    if frame[0x010:0x020] != tape_uuid:
        problems.append("tape UUID")
    if frame[0x020:0x030] == bytes(16):
        problems.append("zero edition ID")
    if rd64(frame, 0x030) == 0:
        problems.append("zero edition sequence")
    ordinal = rd16(frame, 0x038)
    if ordinal not in (1, 2, 3):
        problems.append("replica ordinal")
    if rd16(frame, 0x03A) != 3 or rd32(frame, 0x03C) != 0 or rd32(frame, 0x044) != 0:
        problems.append("replica count, partition or compression")
    if rd32(frame, 0x040) != block_size or block_size not in TERMINAL_RECORD_SIZES:
        problems.append("block size")
    if any(frame[0x1E8:0x1F0]) or any(frame[0x1F4:0x1F8]) or any(frame[0x300:0x3F8]) or any(record[REPLICA_FRAME_LEN:]):
        problems.append("reserved bytes or padding nonzero")
    version_len, stamp_len = rd16(frame, 0x1F0), rd16(frame, 0x1F2)
    version = frame[0x1F8 : 0x1F8 + min(version_len, 128)]
    stamp = frame[0x278 : 0x278 + min(stamp_len, 64)]
    if version_len > 128 or stamp_len > 64 or any(frame[0x1F8 + version_len : 0x278]) or any(frame[0x278 + stamp_len : 0x2B8]):
        problems.append("diagnostic lengths or padding")
    if not printable_text(version) or (stamp_len and not _RFC3339.match(stamp.decode("ascii", "replace"))):
        problems.append("diagnostic text")
    structural_rows = rd64(frame, 0x060)
    object_rows = rd64(frame, 0x068)
    payload_len = STRUCTURAL_SLOT * structural_rows + OBJECT_SLOT * object_rows
    if overflows(payload_len):
        problems.append("payload_len formula overflows u64 (Section 10.6: every size formula evaluates without overflow)")
        raise ReadFailure("TerminalIndexReplicaParse", "; ".join(problems))
    payload_records = ceil_div(payload_len, block_size)
    # Section 2.4 and 10.6: each formula is its exact value. The padding,
    # count × B − payload_len, is below B whatever the intermediate product;
    # the record counts must fit.
    if overflows(1 + payload_records, 2 + payload_records):
        problems.append("footer_block_offset or replica_record_count exceeds u64 (Section 10.6)")
        raise ReadFailure("TerminalIndexReplicaParse", "; ".join(problems))
    if rd64(frame, 0x070) != payload_len or rd64(frame, 0x078) != payload_records:
        problems.append("payload geometry")
    if rd64(frame, 0x080) != 2 + payload_records or rd64(frame, 0x098) != 1 + payload_records:
        problems.append("replica record count or footer offset")
    tuples = parse_tuples(frame[0x148:0x1E8])
    eod = rd64(frame, 0x0A0)
    layout_problem = check_layout(tuples, eod)
    if layout_problem:
        problems.append("planned layout: " + layout_problem)
    raw_tuples = frame[0x148:0x1E8]
    layout = layout_digest_of(0, block_size, [raw_tuples[32 * i : 32 * (i + 1)] for i in range(5)], eod)
    if layout != frame[0x108:0x128]:
        problems.append("layout digest")
    edition = edition_digest_of(
        tape_uuid, frame[0x020:0x030], rd64(frame, 0x030), 0, block_size, rd64(frame, 0x048),
        rd64(frame, 0x050), rd64(frame, 0x058), structural_rows, object_rows, payload_len, payload_records,
        frame[0x0A8:0x0C8], frame[0x0C8:0x0E8], version, stamp,
    )
    if edition != frame[0x0E8:0x108]:
        problems.append("edition digest")
    local_index = {1: 0, 2: 2, 3: 4}.get(ordinal)
    local = tuples[local_index] if local_index is not None else None
    if local is not None:
        if (rd64(frame, 0x088), rd64(frame, 0x090)) != (local[3], local[4]) or local[5] != 2 + payload_records:
            problems.append("local planned location")
        descriptor = replica_descriptor_of(edition, layout, ordinal, raw_tuples[32 * local_index : 32 * (local_index + 1)],
                                           1 + payload_records)
        if descriptor != frame[0x128:0x148]:
            problems.append("descriptor digest")
    footer_fields = frame[0x2B8:0x300]
    if role == 1:
        if any(footer_fields):
            problems.append("footer-only fields nonzero in a header")
    elif local is not None:
        count = rd64(frame, 0x2E8)
        if rd64(frame, 0x2F8) != count - 1 or rd64(frame, 0x2F0) != rd64(frame, 0x2E0) + count - 1:
            problems.append("footer arithmetic")
        if (rd64(frame, 0x2D8), rd64(frame, 0x2E0), count) != (local[3], local[4], local[5]):
            problems.append("footer observation differs from the planned tuple")
    if problems:
        raise ReadFailure("TerminalIndexReplicaParse", "; ".join(problems))
    return {
        "ordinal": ordinal, "tuples": tuples, "raw_tuples": raw_tuples, "eod": eod,
        "structural_rows": structural_rows, "object_rows": object_rows, "payload_len": payload_len,
        "payload_records": payload_records, "payload_sha256": frame[0x0A8:0x0C8],
        "canonical_sha256": frame[0x0C8:0x0E8], "covered": rd64(frame, 0x048), "total": rd64(frame, 0x050),
        "watermark": rd64(frame, 0x058), "edition_id": frame[0x020:0x030], "common": replica_common_fields(frame),
        "descriptor": frame[0x128:0x148], "header_hash": frame[0x2B8:0x2D8],
        "observed_footer_lba": rd64(frame, 0x2F0), "delta": rd64(frame, 0x2F8),
        "observed_start": rd64(frame, 0x2E0),
    }


def parse_separation_frame(record: bytes, tape_uuid: bytes, block_size: int, role: int) -> dict[str, Any]:
    if len(record) != block_size:
        raise ReadFailure("TerminalIndexSeparationParse", "record length")
    frame = record[:SEPARATION_FRAME_LEN]
    label = LABEL_SEPARATION_HEADER if role == 1 else LABEL_SEPARATION_FOOTER
    if frame[0:8] != role_magic(tape_uuid, label):
        raise ReadFailure("TerminalIndexSeparationParse", "role magic does not match")
    if crc64_xz(frame[:0x1F8]) != rd64(frame, 0x1F8):
        raise ReadFailure("TerminalIndexSeparationParse", "frame CRC does not verify")
    problems = []
    if rd16(frame, 0x008) != 1 or rd16(frame, 0x00A) != role or rd32(frame, 0x00C) != 0:
        problems.append("schema version, role or flags")
    if frame[0x010:0x020] != tape_uuid:
        problems.append("tape UUID")
    if frame[0x020:0x030] == bytes(16):
        problems.append("zero edition ID")
    ordinal = rd16(frame, 0x030)
    if ordinal not in (1, 2) or rd16(frame, 0x032) != 2 or rd32(frame, 0x034) != 0 or rd32(frame, 0x03C) != 0:
        problems.append("ordinal, count, partition or compression")
    if rd32(frame, 0x038) != block_size or block_size not in TERMINAL_RECORD_SIZES:
        problems.append("block size")
    nominal = rd64(frame, 0x050)
    total_records = ceil_div(nominal, block_size)
    if overflows(total_records * block_size):
        problems.append("actual_bytes formula overflows u64 (total_records × B)")
    if rd64(frame, 0x058) != total_records or total_records < 2:
        problems.append("total record count")
    if rd64(frame, 0x060) != total_records - 1 or rd64(frame, 0x068) != total_records - 2:
        problems.append("footer offset or interior count")
    if rd32(frame, 0x074) != 0:
        problems.append("fill kind")
    if any(frame[0x1C8:0x1F8]) or any(record[SEPARATION_FRAME_LEN:]):
        problems.append("reserved bytes or padding nonzero")
    tuples = parse_tuples(frame[0x0E0:0x180])
    eod = rd64(frame, 0x0D8)
    layout_problem = check_layout(tuples, eod)
    if layout_problem:
        problems.append("planned layout: " + layout_problem)
    raw = frame[0x0E0:0x180]
    raw_tuples = [raw[32 * i : 32 * (i + 1)] for i in range(5)]
    layout = layout_digest_of(0, block_size, raw_tuples, eod)
    if layout != frame[0x098:0x0B8]:
        problems.append("layout digest")
    local_index = {1: 1, 2: 3}.get(ordinal)
    if local_index is not None:
        local, predecessor, successor = tuples[local_index], tuples[local_index - 1], tuples[local_index + 1]
        if (rd64(frame, 0x040), rd64(frame, 0x048)) != (local[3], local[4]) or local[5] != total_records:
            problems.append("local planned location")
        if (rd16(frame, 0x070), rd16(frame, 0x072)) != (predecessor[1], successor[1]):
            problems.append("neighbour ordinals")
        if (rd64(frame, 0x078), rd64(frame, 0x080), rd64(frame, 0x088), rd64(frame, 0x090)) != (
                predecessor[3], successor[3], predecessor[4], successor[4]):
            problems.append("neighbour locations")
        descriptor = separation_descriptor_of(
            tape_uuid, frame[0x020:0x030], ordinal, 0, block_size, nominal, total_records,
            raw_tuples[local_index], raw_tuples[local_index - 1], raw_tuples[local_index + 1], layout)
        if descriptor != frame[0x0B8:0x0D8]:
            problems.append("separation descriptor digest")
        if role == 1:
            if any(frame[0x180:0x1C8]):
                problems.append("footer-only fields nonzero in a header")
        else:
            count = rd64(frame, 0x1B0)
            if rd64(frame, 0x1C0) != count - 1 or rd64(frame, 0x1B8) != rd64(frame, 0x1A8) + count - 1:
                problems.append("footer arithmetic")
            if (rd64(frame, 0x1A0), rd64(frame, 0x1A8), count) != (local[3], local[4], local[5]):
                problems.append("footer observation differs from the planned tuple")
    if problems:
        raise ReadFailure("TerminalIndexSeparationParse", "; ".join(problems))
    common = frame[0x010:0x0E0] + frame[0x0E0:0x180]
    return {"ordinal": ordinal, "tuples": tuples, "edition_id": frame[0x020:0x030], "common": common,
            "header_hash": frame[0x180:0x1A0], "total_records": total_records}


# ----- terminal payload ----------------------------------------------------


def decode_slot(slot: bytes, label: str) -> Any:
    length = rd16(slot, 0)
    if not 1 <= length <= len(slot) - 2:
        raise ReadFailure("TerminalIndexReplicaParse", f"{label}: encoded_len {length}")
    if any(slot[2 + length :]):
        raise ReadFailure("TerminalIndexReplicaParse", f"{label}: nonzero slot padding")
    try:
        return decode_deterministic_cbor(slot[2 : 2 + length])
    except (RederivationError, AssertionError, TypeError) as failure:
        raise ReadFailure("TerminalIndexReplicaParse", f"{label}: {failure}") from failure


def validate_structural_rows(rows: list[Any], frame: dict[str, Any], replica_a_tape_file: int) -> list[MapEntry]:
    entries: list[MapEntry] = []
    problems: list[str] = []
    for index, row in enumerate(rows):
        if not (isinstance(row, list) and len(row) == 7):
            raise ReadFailure("TerminalIndexReplicaParse", f"structural row {index} is not a 7-element array")
        if not all(value is None or is_uint(value) for value in row) or any(value is None for value in row[:3]):
            raise ReadFailure("TerminalIndexReplicaParse", f"structural row {index} types")
        entries.append(MapEntry(*row))
    ordinal = 0
    epoch = 0
    watermark_chain = 0
    for index, entry in enumerate(entries):
        if entry.tape_file_number != index:
            problems.append(f"row {index}: tape-file numbers not dense")
        if entry.kind not in (0, 1, 2, 3):
            problems.append(f"row {index}: kind {entry.kind} not allowed before A")
        if entry.block_count == 0:
            problems.append(f"row {index}: zero block count")
        fields = (entry.first_parity_data_ordinal, entry.protected_ordinal_start,
                  entry.protected_ordinal_end_exclusive, entry.epoch_id)
        if entry.kind == KIND_OBJECT:
            if fields[0] is None or any(value is not None for value in fields[1:]):
                problems.append(f"row {index}: Object fields")
            elif entry.first_parity_data_ordinal != ordinal:
                problems.append(f"row {index}: first ordinal not dense")
            else:
                ordinal += entry.block_count
        elif entry.kind == KIND_SIDECAR:
            if fields[0] is not None or any(value is None for value in fields[1:]):
                problems.append(f"row {index}: sidecar fields")
            else:
                if entry.protected_ordinal_start != watermark_chain or entry.protected_ordinal_end_exclusive <= entry.protected_ordinal_start:
                    problems.append(f"row {index}: sidecar range not contiguous")
                if entry.epoch_id != epoch:
                    problems.append(f"row {index}: epoch ids not consecutive")
                watermark_chain = entry.protected_ordinal_end_exclusive or 0
                epoch += 1
        elif any(value is not None for value in fields):
            problems.append(f"row {index}: fields outside the kind")
        if entry.kind == KIND_BOOTSTRAP and (index != 0 or entry.block_count != 1):
            problems.append(f"row {index}: bootstrap not a one-block tape file 0")
    if not entries or entries[0].kind != KIND_BOOTSTRAP:
        problems.append("structural row 0 is not the bootstrap")
    total = derived_total(entries)
    watermark = derived_watermark(entries)
    if total != frame["total"] or watermark != frame["watermark"]:
        problems.append("T or W differs from the frame")
    parity_maps = [entry for entry in entries if entry.kind == KIND_PARITY_MAP]
    sidecars = [entry for entry in entries if entry.kind == KIND_SIDECAR]
    if parity_maps and (len(parity_maps) != 1 or entries[-1].kind != KIND_PARITY_MAP):
        problems.append("the ParityMap row is not the single final row")
    if bool(parity_maps) != bool(sidecars):
        problems.append("a ParityMap is present if and only if a sidecar is")
    if sidecars and frame["watermark"] != frame["total"]:
        # Section 10.6: both are the recorded fields; Section 7.4's
        # cross-check against the recomputed values is the rule above.
        problems.append("recorded highest protected ordinal differs from recorded total data ordinals")
    if not (frame["covered"] == len(entries) == replica_a_tape_file):
        problems.append("covered count, structural_row_count and A's tape-file number differ")
    # Section 7.2: every record and trailing-filemark position the map
    # describes, LBA(f, b), must fit in u64 (the append point after the last
    # filemark is not a rule of the map).
    position = 0
    for entry in entries:
        if position + entry.block_count > U64_MAX:
            problems.append(f"row {entry.tape_file_number}: its records and trailing filemark reach position "
                            f"{position + entry.block_count}, which does not fit in u64 (Section 7.2)")
            break
        position += entry.block_count + 1
    if problems:
        raise ReadFailure("TerminalIndexReplicaParse", "; ".join(problems))
    return entries


def validate_object_row(row: Any, entry: MapEntry, block_size: int, index: int) -> None:
    label = f"Object row {index}"
    if not isinstance(row, dict) or not all(isinstance(key, int) and not isinstance(key, bool) for key in row):
        raise ReadFailure("TerminalIndexReplicaParse", f"{label}: not an integer-keyed map")
    problems = []
    if row.get(1) != entry.tape_file_number:
        problems.append("tape_file_number")
    if row.get(3) != entry.block_count or not is_uint(row.get(3)) or row.get(3) == 0:
        problems.append("stored_block_count")
    object_id = row.get(4)
    if not isinstance(object_id, bytes) or not 1 <= len(object_id) <= 64 or b"\x00" in object_id:
        problems.append("object_id")
    representation = row.get(2)
    plaintext_keys = {10, 11, 12, 13}
    encrypted_keys = {21, 22, 23}
    if representation == "plaintext":
        if not plaintext_keys <= set(row) or encrypted_keys & set(row):
            problems.append("plaintext key set")
        else:
            first, size, count, sha = row[10], row[11], row[12], row[13]
            if not (is_uint(first) and is_uint(size) and is_uint(count) and size > 0 and count > 0):
                problems.append("manifest fields")
            elif first + count > entry.block_count or size > count * block_size:
                problems.append("manifest range")
            if not (isinstance(sha, bytes) and len(sha) == 32):
                problems.append("manifest_sha256")
    elif representation == "encrypted":
        if not encrypted_keys <= set(row) or plaintext_keys & set(row):
            problems.append("encrypted key set")
        else:
            ids = row[22]
            if not (is_uint(row[21]) and 17 <= row[21] <= 16 * 1024 * 1024):
                problems.append("metadata_frame_len")
            if not (is_uint(row[23]) and 1191 <= row[23] <= 16384):
                problems.append("key_frame_len")
            if not (isinstance(ids, list) and 1 <= len(ids) <= 8 and all(
                    isinstance(v, bytes) and len(v) == 16 and v != bytes(16) for v in ids) and len(set(ids)) == len(ids)):
                problems.append("recipient_epoch_ids")
    else:
        problems.append("representation marker")
    if problems:
        raise ReadFailure("TerminalIndexReplicaParse", f"{label}: " + ", ".join(problems))


def validate_payload(payload_blocks: list[bytes], frame: dict[str, Any], block_size: int) -> tuple[list[MapEntry], list[dict]]:
    payload = b"".join(payload_blocks)
    length = frame["payload_len"]
    if any(payload[length:]):
        raise ReadFailure("TerminalIndexReplicaParse", "nonzero payload padding")
    structural_slots = [payload[i * STRUCTURAL_SLOT : (i + 1) * STRUCTURAL_SLOT] for i in range(frame["structural_rows"])]
    base = STRUCTURAL_SLOT * frame["structural_rows"]
    object_slots = [payload[base + i * OBJECT_SLOT : base + (i + 1) * OBJECT_SLOT] for i in range(frame["object_rows"])]
    if digest(PAYLOAD_DOMAIN + payload[:length]) != frame["payload_sha256"]:
        raise ReadFailure("TerminalIndexReplicaParse", "payload digest")
    rows = [decode_slot(slot, f"structural slot {i}") for i, slot in enumerate(structural_slots)]
    entries = validate_structural_rows(rows, frame, frame["tuples"][0][3])
    if canonical_digest(entries) != frame["canonical_sha256"]:
        raise ReadFailure("FilemarkMapDigestMismatch", "canonical-map digest")
    objects = [entry for entry in entries if entry.kind == KIND_OBJECT]
    if len(objects) != len(object_slots):
        raise ReadFailure("TerminalIndexReplicaParse", "Object-row count differs from kind-0 rows")
    object_rows = []
    for index, (slot, entry) in enumerate(zip(object_slots, objects, strict=True)):
        row = decode_slot(slot, f"Object slot {index}")
        validate_object_row(row, entry, block_size, index)
        object_rows.append(row)
    return entries, object_rows


# ----- replica and separation validation at the device ---------------------


@dataclass
class ReplicaCheck:
    letter: str
    envelope_valid: bool
    payload_valid: bool | None
    reason: str
    header: dict[str, Any] | None = None
    entries: list[MapEntry] | None = None
    object_rows: list[dict] | None = None

    @property
    def fully_valid(self) -> bool:
        return self.envelope_valid and bool(self.payload_valid)


LETTERS = {1: "A", 2: "B", 3: "C"}


def check_replica(tape: DamagedTape, layout: list[tuple], ordinal: int, tape_uuid: bytes, block_size: int) -> ReplicaCheck:
    letter = LETTERS[ordinal]
    tuple_index = {1: 0, 2: 2, 3: 4}[ordinal]
    kind, _, _, tape_file, start, records = layout[tuple_index]
    try:
        header_record = tape.read_data(start)
        header = parse_replica_frame(header_record, tape_uuid, block_size, 1)
        footer_record = tape.read_data(start + records - 1)
        footer = parse_replica_frame(footer_record, tape_uuid, block_size, 2)
    except MediumError as error:
        return ReplicaCheck(letter, False, None, f"medium error at LBA {error.args[0]}")
    except ReadFailure as failure:
        return ReplicaCheck(letter, False, None, f"{failure.error}: {failure.reason}")
    problems = []
    if header["ordinal"] != ordinal or footer["ordinal"] != ordinal:
        problems.append("frame ordinal differs from the planned component")
    if header["tuples"] != layout or footer["tuples"] != layout:
        problems.append("frame layout differs from the discovered planned layout")
    if footer["observed_start"] + footer["delta"] != start + records - 1 or footer["observed_start"] != start:
        problems.append("footer backward delta does not name the planned header start")
    if footer["header_hash"] != digest(header_record):
        problems.append("footer header-record SHA-256 differs")
    if header["common"] != footer["common"] or header["descriptor"] != footer["descriptor"]:
        problems.append("header and footer common descriptor differ")
    if tape.next_filemark(start) != start + records:
        problems.append("trailing filemark not measured at the planned position")
    if problems:
        return ReplicaCheck(letter, False, None, "; ".join(problems), header)
    try:
        payload_blocks = [tape.read_data(start + 1 + i) for i in range(header["payload_records"])]
        entries, object_rows = validate_payload(payload_blocks, header, block_size)
    except MediumError as error:
        return ReplicaCheck(letter, True, False, f"payload record unreadable: medium error at LBA {error.args[0]}", header)
    except ReadFailure as failure:
        return ReplicaCheck(letter, True, False, f"payload: {failure.error}: {failure.reason}", header)
    return ReplicaCheck(letter, True, True, "locally eligible under Section 10.6", header, entries, object_rows)


def check_separation(tape: DamagedTape, layout: list[tuple], ordinal: int, tape_uuid: bytes, block_size: int) -> tuple[str, str, bytes | None]:
    """Full verification of one separation extent; returns (status, reason, edition ID)."""
    tuple_index = {1: 1, 2: 3}[ordinal]
    _, _, _, _, start, records = layout[tuple_index]
    try:
        header_record = tape.read_data(start)
        header = parse_separation_frame(header_record, tape_uuid, block_size, 1)
        footer_record = tape.read_data(start + records - 1)
        footer = parse_separation_frame(footer_record, tape_uuid, block_size, 2)
        interior = [tape.read_data(start + 1 + i) for i in range(records - 2)]
    except MediumError as error:
        return "unreadable", f"medium error at LBA {error.args[0]}", None
    except ReadFailure as failure:
        return "invalid", f"{failure.error}: {failure.reason}", None
    problems = []
    if header["ordinal"] != ordinal or footer["ordinal"] != ordinal:
        problems.append("frame ordinal")
    if header["tuples"] != layout or footer["tuples"] != layout:
        problems.append("frame layout differs from the discovered planned layout")
    if footer["header_hash"] != digest(header_record):
        problems.append("footer header-record SHA-256")
    if header["common"] != footer["common"]:
        problems.append("header and footer differ")
    if any(any(block) for block in interior):
        problems.append("nonzero interior")
    if tape.next_filemark(start) != start + records:
        problems.append("trailing filemark not measured at the planned position")
    if problems:
        return "invalid", "; ".join(problems), header["edition_id"]
    return "valid", "header, footer, zero interior and trailing filemark validate", header["edition_id"]


def discover_layout(tape: DamagedTape, tape_uuid: bytes, block_size: int) -> tuple[list[tuple] | None, str]:
    """Section 8.4 step 1: find, from EOD, a replica footer and read its planned layout.

    The Scanner spaces back over up to five filemarks from EOD and reads the
    record before each. It stops early when a backspace does not cross
    exactly one filemark (the record before a filemark is itself a filemark,
    or no filemark is where one is spaced over) or leaves the partition. A
    replica footer that parses supplies the layout only when its recorded
    footer position equals the position at which it was read and its planned
    EOD is at or after the tape's EOD; when no footer supplies one, there is
    no layout and no replica validates.
    """
    eod = tape.eod()
    position = eod  # the position just after the next filemark to space back over
    rejected = []
    for _ in range(5):
        filemark = position - 1
        if filemark < 0 or not tape.is_filemark(filemark):
            break
        record_lba = filemark - 1
        if record_lba < 0 or tape.is_filemark(record_lba):
            break
        try:
            record = tape.read_data(record_lba)
            footer = parse_replica_frame(record, tape_uuid, block_size, 2)
        except (MediumError, ReadFailure):
            footer = None
        if footer is not None:
            if footer["observed_footer_lba"] != record_lba:
                rejected.append(f"footer at LBA {record_lba} records its position as {footer['observed_footer_lba']}")
            elif footer["eod"] < eod:
                rejected.append(f"footer at LBA {record_lba} plans EOD {footer['eod']}, before the tape's EOD {eod}")
            else:
                return footer["tuples"], f"replica footer at LBA {record_lba} (replica ordinal {footer['ordinal']})"
        previous = tape.filemark_positions_before(record_lba)
        if not previous:
            break
        position = previous[0] + 1
    note = "no terminal-replica footer supplies a layout from EOD"
    return None, note + (" (" + "; ".join(rejected) + ")" if rejected else "")


# ----- sidecars and ParityMaps ---------------------------------------------


def parse_sidecar_copy(blocks: list[bytes], tape_uuid: bytes, block_size: int, copy_kind: int) -> dict[str, Any]:
    """Validate one header/index copy (Sections 9.2-9.5)."""
    head = blocks[0]
    if head[0:8] != role_magic(tape_uuid, LABEL_SIDECAR):
        raise ReadFailure("SidecarParse", "sidecar magic")
    if crc64_xz(head[0:0xC0]) != rd64(head, 0xC0):
        raise ReadFailure("SidecarParse", "header CRC")
    if crc64_xz(head[: block_size - 8]) != rd64(head, block_size - 8):
        raise ReadFailure("SidecarParse", "block 0 CRC")
    fields = {
        "tape_uuid": head[0x08:0x18], "epoch_id": rd64(head, 0x18), "k": rd16(head, 0x20), "m": rd16(head, 0x22),
        "S": rd32(head, 0x24), "block_size": rd32(head, 0x28), "schema": rd32(head, 0x2C), "start": rd64(head, 0x30),
        "end": rd64(head, 0x38), "logical": rd64(head, 0x40), "real": rd64(head, 0x48), "P": rd64(head, 0x50),
        "data_crc_count": rd64(head, 0x58), "H": rd64(head, 0x60), "inline": rd64(head, 0x68),
        "total": rd64(head, 0x70), "primary_start": rd64(head, 0x78), "tail_start": rd64(head, 0x80),
        "footer_index": rd64(head, 0x88), "copy_kind": rd16(head, 0x90), "hash": head[0x98:0xB8],
    }
    problems = []
    if fields["tape_uuid"] != tape_uuid:
        problems.append("tape_uuid")
    if 0 in (fields["k"], fields["m"], fields["S"]):
        problems.append("zero k, m or S")
    if fields["block_size"] != block_size or fields["schema"] != 2:
        problems.append("block size or schema version")
    if fields["end"] <= fields["start"]:
        problems.append("empty range")
    if fields["logical"] != fields["S"] * fields["k"] or fields["P"] != fields["S"] * fields["m"]:
        problems.append("logical or parity counts")
    if fields["real"] != fields["end"] - fields["start"] or not 1 <= fields["real"] <= fields["logical"]:
        problems.append("real_data_shard_count")
    if fields["data_crc_count"] != fields["real"]:
        problems.append("data_crc_count")
    if problems:
        raise ReadFailure("SidecarParse", ", ".join(problems))
    try:
        header_blocks, inline, placements = sidecar_layout(block_size, fields["S"], fields["m"], fields["real"])
    except BuildRefusal as refusal:
        raise ReadFailure("SidecarParse", refusal.message) from refusal
    if (fields["H"], fields["inline"]) != (header_blocks, inline):
        problems.append("H or inline_index_entry_bytes differ from Section 9.4")
    if fields["total"] != 2 * header_blocks + fields["P"] + 1 or fields["primary_start"] != 0 or \
            fields["tail_start"] != header_blocks + fields["P"] or fields["footer_index"] != 2 * header_blocks + fields["P"]:
        problems.append("layout counts")
    if fields["copy_kind"] != copy_kind or rd16(head, 0x92) or rd32(head, 0x94) or rd64(head, 0xB8):
        problems.append("copy_kind, reserved or copy_generation")
    if problems:
        raise ReadFailure("SidecarParse", ", ".join(problems))
    if len(blocks) != header_blocks:
        raise ReadFailure("SidecarParse", "wrong number of copy blocks supplied")
    stream = bytearray()
    parity_entries = fields["S"] * fields["m"]
    parity_crcs: dict[tuple[int, int], int] = {}
    data_crcs: list[int] = []
    number = 0
    for block_index, offsets in enumerate(placements):
        block = blocks[block_index]
        if block_index and crc64_xz(block[: block_size - 8]) != rd64(block, block_size - 8):
            raise ReadFailure("SidecarParse", f"index block {block_index} CRC")
        used = bytearray(block[: block_size - 8])
        if block_index == 0:
            used[0:0xC8] = bytes(0xC8)
        for offset in offsets:
            length = 16 if number < parity_entries else 8
            entry = bytes(block[offset : offset + length])
            stream += entry
            if number < parity_entries:
                stripe, parity_index, reserved, value = struct.unpack("<IHHQ", entry)
                if (stripe, parity_index) != (number // fields["m"], number % fields["m"]) or reserved:
                    raise ReadFailure("SidecarParse", "parity index entry order or reserved field")
                parity_crcs[(stripe, parity_index)] = value
            else:
                data_crcs.append(struct.unpack("<Q", entry)[0])
            used[offset : offset + length] = bytes(length)
            number += 1
        if any(used):
            raise ReadFailure("SidecarParse", f"index block {block_index} unused space nonzero")
    if digest(SIDECAR_METADATA_DOMAIN + head[0x00:0x90] + bytes(stream)) != fields["hash"]:
        raise ReadFailure("SidecarParse", "canonical_metadata_hash does not verify")
    fields["parity_crcs"] = parity_crcs
    fields["data_crcs"] = data_crcs
    fields["metadata"] = head[0x00:0x90] + bytes(stream)
    return fields


def parse_sidecar_footer(block: bytes, tape_uuid: bytes) -> dict[str, Any]:
    if block[0:8] != role_magic(tape_uuid, LABEL_SIDECAR_FOOTER):
        raise ReadFailure("SidecarParse", "footer magic")
    if crc64_xz(block[0:0x80]) != rd64(block, 0x80):
        raise ReadFailure("SidecarParse", "footer CRC")
    if rd16(block, 0x08) != 2 or any(block[0x0A:0x10]) or block[0x10:0x20] != tape_uuid or any(block[0x88:]):
        raise ReadFailure("SidecarParse", "footer version, reserved, UUID or fill")
    footer = {"epoch_id": rd64(block, 0x20), "start": rd64(block, 0x28), "end": rd64(block, 0x30),
              "H": rd64(block, 0x38), "P": rd64(block, 0x40), "total": rd64(block, 0x48),
              "primary_start": rd64(block, 0x50), "tail_start": rd64(block, 0x58), "hash": block[0x60:0x80]}
    if overflows(footer["H"] + footer["P"]):
        raise ReadFailure("SidecarParse", "footer H + P exceeds u64, so tail_header_start_block cannot equal it")
    if footer["primary_start"] != 0 or footer["tail_start"] != footer["H"] + footer["P"]:
        raise ReadFailure("SidecarParse", "footer locator: primary start is not 0 or tail start is not H + P")
    return footer


def copy_matches_footer(copy: dict[str, Any], footer: dict[str, Any]) -> bool:
    return all(copy[key] == footer[key] for key in ("epoch_id", "start", "end", "H", "P", "total", "hash"))


def read_blocks(tape: DamagedTape, start: int, count: int) -> list[bytes]:
    return [tape.read_data(start + index) for index in range(count)]


def parse_parity_map(tape: DamagedTape, start: int, count: int, tape_uuid: bytes, block_size: int) -> dict[str, Any]:
    """Read and validate a final ParityMap tape file (Section 10.1)."""

    def header_fields(block: bytes, copy_kind: int) -> dict[str, Any]:
        if block[0:8] != role_magic(tape_uuid, LABEL_PARITY_MAP):
            raise ReadFailure("ParityMapParse", "magic")
        if crc64_xz(block[0:0xC0]) != rd64(block, 0xC0):
            raise ReadFailure("ParityMapParse", "CRC")
        fields = {
            "version": rd16(block, 0x08), "copy_kind": rd16(block, 0x0A), "uuid": block[0x10:0x20],
            "sequence": rd64(block, 0x20), "block_size": rd32(block, 0x28), "payload_len": rd64(block, 0x30),
            "payload_sha": block[0x38:0x58], "digest": block[0x58:0x78], "scope": rd64(block, 0x78),
            "total": rd64(block, 0x80), "watermark": rd64(block, 0x88), "final": block[0x90],
            "M": rd64(block, 0x98), "total_blocks": rd64(block, 0xA0), "primary": rd64(block, 0xA8),
            "tail": rd64(block, 0xB0), "footer": rd64(block, 0xB8),
        }
        copy_blocks = ceil_div(0xC8 + fields["payload_len"], block_size)
        if fields["version"] != 2 or fields["copy_kind"] != copy_kind or rd32(block, 0x0C) or rd32(block, 0x2C) \
                or any(block[0x91:0x98]) or fields["uuid"] != tape_uuid or fields["block_size"] != block_size:
            raise ReadFailure("ParityMapParse", "fixed fields")
        if fields["final"] not in (0, 1):
            raise ReadFailure("ParityMapParse", "is_final_directory byte")
        if (fields["M"], fields["total_blocks"], fields["primary"], fields["tail"], fields["footer"]) != (
                copy_blocks, 2 * copy_blocks + 1, 0, copy_blocks, 2 * copy_blocks):
            raise ReadFailure("ParityMapParse", "locator arithmetic")
        return fields

    def parse_copy(first_block: int, copy_kind: int) -> dict[str, Any]:
        head = tape.read_data(start + first_block)
        fields = header_fields(head, copy_kind)
        blocks = [head] + [tape.read_data(start + first_block + i) for i in range(1, fields["M"])]
        body = b"".join(blocks)
        payload = body[0xC8 : 0xC8 + fields["payload_len"]]
        if any(body[0xC8 + fields["payload_len"] :]):
            raise ReadFailure("ParityMapParse", "copy tail not zero")
        if digest(payload) != fields["payload_sha"]:
            raise ReadFailure("ParityMapParse", "payload_sha256")
        fields["payload"] = payload
        fields["header_bytes"] = head[:0xC8]
        return fields

    copies = {}
    failures = {}
    for name, first, kind in (("primary", 0, 1), ("tail", None, 2)):
        try:
            if first is None:
                primary = copies.get("primary")
                if primary is not None:
                    first = primary["M"]
                else:
                    footer_block = tape.read_data(start + count - 1)
                    first = header_fields(footer_block, 0)["M"]
            copies[name] = parse_copy(first, kind)
        except MediumError as error:
            failures[name] = f"medium error at LBA {error.args[0]}"
        except ReadFailure as failure:
            failures[name] = failure.reason
    if not copies:
        raise ReadFailure("ParityMapParse", "no header copy validates (" + "; ".join(f"{k}: {v}" for k, v in failures.items()) + ")")
    if len(copies) == 2:
        a, b = copies["primary"], copies["tail"]
        if a["header_bytes"][:0x0A] + a["header_bytes"][0x0C:0xC0] != b["header_bytes"][:0x0A] + b["header_bytes"][0x0C:0xC0] \
                or a["payload"] != b["payload"]:
            raise ReadFailure("ParityMapParse", "primary and tail copies disagree")
    chosen = copies.get("primary") or copies["tail"]
    try:
        footer_fields = header_fields(tape.read_data(start + count - 1), 0)
        footer_block = tape.read_data(start + count - 1)
        if any(footer_block[0xC8:]):
            raise ReadFailure("ParityMapParse", "footer fill")
        for key in ("sequence", "payload_len", "payload_sha", "digest", "scope", "total", "watermark", "final", "M"):
            if footer_fields[key] != chosen[key]:
                raise ReadFailure("ParityMapParse", f"footer {key} differs from the header")
    except MediumError:
        footer_fields = None
    if chosen["total_blocks"] != count:
        raise ReadFailure("ParityMapParse", "measured length differs from the locator")
    payload = decode_integer_keyed_map(chosen["payload"], "ParityMap payload", "ParityMapParse")
    if payload.get(1) != PARITY_MAP_FORMAT or payload.get(2) != tape_uuid or payload.get(3) != chosen["sequence"] \
            or payload.get(5) != chosen["digest"]:
        raise ReadFailure("ParityMapParse", "payload does not match the header")
    directory = payload.get(4)
    if not isinstance(directory, dict) or any(key not in directory for key in (1, 2, 3, 4, 5)):
        raise ReadFailure("ParityMapParse", "directory keys")
    if (directory[1], directory[2], directory[3]) != (chosen["scope"], chosen["total"], chosen["watermark"]) or \
            directory[4] not in (True, False) or (1 if directory[4] else 0) != chosen["final"]:
        raise ReadFailure("ParityMapParse", "directory scope differs from the header")
    entries = []
    for number, item in enumerate(directory[5] if isinstance(directory[5], list) else []):
        if not isinstance(item, dict) or any(key not in item for key in range(1, 10)):
            raise ReadFailure("ParityMapParse", f"directory entry {number} keys")
        if not all(is_uint(item[key]) for key in (1, 2, 3, 4, 5, 6, 7)) or not is_uint(item[9], 2**32 - 1) \
                or not (isinstance(item[8], bytes) and len(item[8]) == 32):
            raise ReadFailure("ParityMapParse", f"directory entry {number} types")
        entries.append({"tape_file_number": item[1], "epoch_id": item[2], "start": item[3], "end": item[4],
                        "total": item[5], "H": item[6], "P": item[7], "hash": item[8], "flags": item[9]})
    # Section 10.1.5 invariants.
    previous_file = -1
    chain = 0
    for number, item in enumerate(entries):
        if item["tape_file_number"] <= previous_file or item["tape_file_number"] >= directory[1]:
            raise ReadFailure("DirectoryInvalid", "entries not ascending or outside the scope")
        if 0 in (item["total"], item["H"], item["P"]) or item["start"] != chain or item["end"] <= item["start"]:
            raise ReadFailure("DirectoryInvalid", "counts or ranges")
        if item["epoch_id"] != number or item["flags"] & ~DIRECTORY_FLAG_MASK:
            raise ReadFailure("DirectoryInvalid", "epoch ids or flags")
        previous_file = item["tape_file_number"]
        chain = item["end"]
    if directory[3] > directory[2] or chain != directory[3]:
        raise ReadFailure("DirectoryInvalid", "protected ranges do not partition [0, W)")
    return {"scope": chosen["scope"], "total": chosen["total"], "watermark": chosen["watermark"],
            "final": bool(chosen["final"]), "digest": chosen["digest"], "entries": entries,
            "copies": sorted(copies), "copy_failures": failures, "footer_readable": footer_fields is not None}


# ----- the BOT structural walk ---------------------------------------------


@dataclass
class WalkedFile:
    tape_file_number: int
    start: int
    count: int
    kind: int | None
    note: str
    epoch_id: int | None = None
    start_ordinal: int | None = None
    end_ordinal: int | None = None
    failed_classification: str | None = None


def classify_file(tape: DamagedTape, tape_file: int, start: int, count: int, tape_uuid: bytes,
                  block_size: int) -> WalkedFile:
    """Section 12.3's ladder for one measured tape file.

    Items 2 and 3 commit a file to its control type as soon as its magic
    matches. Items 1, 4, 5 and 6 recognise a file only when it passes every
    check of the rung, its measured count included; a file that one of them
    parses but does not recognise has its failed classification reported,
    and item 7 then makes it an Object candidate.
    """
    try:
        head = tape.read_data(start)
        head_error = None
    except MediumError as error:
        head = None
        head_error = f"head unreadable (medium error at LBA {error.args[0]})"
    except ReadFailure as failure:
        head = None
        head_error = f"head is not one block ({failure.reason})"

    def unrecognised(failure: str) -> WalkedFile:
        return WalkedFile(tape_file, start, count, KIND_OBJECT,
                          f"failed classification: {failure}; not recognised, so item 7 applies: Object by elimination",
                          failed_classification=failure)

    def sidecar_footer_probe() -> WalkedFile | None:
        try:
            footer = parse_sidecar_footer(tape.read_data(start + count - 1), tape_uuid)
        except (MediumError, ReadFailure):
            return None
        if footer["total"] != count:
            return unrecognised(f"sidecar footer total {footer['total']} differs from the measured {count} blocks "
                                "(Section 12.3 item 6)")
        try:
            tail = parse_sidecar_copy(read_blocks(tape, start + footer["tail_start"], footer["H"]), tape_uuid, block_size, 2)
            if not copy_matches_footer(tail, footer):
                return unrecognised("sidecar tail copy differs from its footer (Section 12.3 item 6)")
            note = "sidecar by footer probe; tail copy verified against the footer"
        except MediumError:
            note = "sidecar by footer fields alone; tail copy unreadable"
        except ReadFailure as failure:
            return unrecognised(f"sidecar tail copy: {failure.reason} (Section 12.3 item 6)")
        return WalkedFile(tape_file, start, count, KIND_SIDECAR, note, footer["epoch_id"], footer["start"], footer["end"])

    if head is not None:
        magic = head[0:8]
        if magic == BOOTSTRAP_MAGIC_BYTES:
            try:
                boot = parse_bootstrap(head, block_size)
                if boot["tape_uuid"] == tape_uuid and count == 1:
                    return WalkedFile(tape_file, start, count, KIND_BOOTSTRAP, "bootstrap")
                failure = "bootstrap magic, but the tape UUID differs or the file is not one block"
            except ReadFailure as failure_detail:
                failure = f"bootstrap magic, frame invalid ({failure_detail.reason})"
            return unrecognised(failure + " (Section 12.3 item 1)")
        if magic == role_magic(tape_uuid, LABEL_REPLICA_HEADER):
            try:
                header = parse_replica_frame(head, tape_uuid, block_size, 1)
                local = header["tuples"][{1: 0, 2: 2, 3: 4}[header["ordinal"]]]
                if (local[3], local[4], local[5]) != (tape_file, start, count):
                    return WalkedFile(tape_file, start, count, KIND_REPLICA,
                                      "damaged terminal replica: header plan differs from the measured file")
                return WalkedFile(tape_file, start, count, KIND_REPLICA, "terminal replica header matches the plan")
            except ReadFailure as failure:
                return WalkedFile(tape_file, start, count, KIND_REPLICA, f"damaged terminal replica: {failure.reason}")
        if magic == role_magic(tape_uuid, LABEL_SEPARATION_HEADER):
            try:
                header = parse_separation_frame(head, tape_uuid, block_size, 1)
                local = header["tuples"][{1: 1, 2: 3}[header["ordinal"]]]
                if (local[3], local[4], local[5]) != (tape_file, start, count):
                    return WalkedFile(tape_file, start, count, KIND_SEPARATION,
                                      "damaged separation extent: header plan differs from the measured file")
                return WalkedFile(tape_file, start, count, KIND_SEPARATION, "separation header matches the plan")
            except ReadFailure as failure:
                return WalkedFile(tape_file, start, count, KIND_SEPARATION, f"damaged separation extent: {failure.reason}")
        if magic == role_magic(tape_uuid, LABEL_PARITY_MAP):
            try:
                parity_map = parse_parity_map(tape, start, count, tape_uuid, block_size)
                walked = WalkedFile(tape_file, start, count, KIND_PARITY_MAP, "ParityMap validates")
                walked.parity_map = parity_map  # type: ignore[attr-defined]
                return walked
            except ReadFailure as failure:
                probe = sidecar_footer_probe()
                if probe is not None:
                    return probe
                return unrecognised(f"ParityMap: {failure.reason} (Section 12.3 item 4)")
        if magic == role_magic(tape_uuid, LABEL_SIDECAR):
            try:
                header_blocks_guess = rd64(head, 0x60)
                if 1 <= header_blocks_guess <= count:
                    primary = parse_sidecar_copy(read_blocks(tape, start, header_blocks_guess), tape_uuid, block_size, 1)
                    if primary["total"] != count:
                        # Item 5 decides the sidecar rung when the primary parses.
                        return unrecognised(f"primary header total {primary['total']} differs from the measured "
                                            f"{count} blocks (Section 12.3 item 5; item 6 is not tried)")
                    return WalkedFile(tape_file, start, count, KIND_SIDECAR, "sidecar by primary header",
                                      primary["epoch_id"], primary["start"], primary["end"])
            except (MediumError, ReadFailure):
                pass
        probe = sidecar_footer_probe()
        if probe is not None:
            return probe
        return WalkedFile(tape_file, start, count, KIND_OBJECT, "Object by elimination")
    # Unreadable head block: the last block read for the sidecar probe also
    # serves the terminal footer probe of items 2 and 3.
    try:
        last = tape.read_data(start + count - 1)
    except MediumError:
        return WalkedFile(tape_file, start, count, KIND_OBJECT,
                          f"Object candidate: {head_error}; last block unreadable")
    except ReadFailure:
        last = None
    if last is not None and last[0:8] == role_magic(tape_uuid, LABEL_REPLICA_FOOTER):
        try:
            footer = parse_replica_frame(last, tape_uuid, block_size, 2)
            local = footer["tuples"][{1: 0, 2: 2, 3: 4}[footer["ordinal"]]]
            if (local[3], local[4], local[5]) == (tape_file, start, count):
                return WalkedFile(tape_file, start, count, KIND_REPLICA,
                                  f"damaged terminal replica: {head_error}; type established by its footer")
        except ReadFailure:
            pass
    if last is not None and last[0:8] == role_magic(tape_uuid, LABEL_SEPARATION_FOOTER):
        try:
            footer = parse_separation_frame(last, tape_uuid, block_size, 2)
            local = footer["tuples"][{1: 1, 2: 3}[footer["ordinal"]]]
            if (local[3], local[4], local[5]) == (tape_file, start, count):
                return WalkedFile(tape_file, start, count, KIND_SEPARATION,
                                  f"damaged separation extent: {head_error}; type established by its footer")
        except ReadFailure:
            pass
    probe = sidecar_footer_probe()
    if probe is not None:
        return probe
    return WalkedFile(tape_file, start, count, KIND_OBJECT, f"Object candidate: {head_error}")


def bot_walk(tape: DamagedTape, tape_uuid: bytes, block_size: int) -> tuple[list[WalkedFile], list[str]]:
    files: list[WalkedFile] = []
    damage: list[str] = []
    position = 0
    tape_file = 0
    while position < tape.eod():
        filemark = tape.next_filemark(position)
        if filemark is None:
            damage.append(f"tape file {tape_file}: missing trailing filemark before EOD")
            count = tape.eod() - position
        else:
            count = filemark - position
        if count == 0:
            damage.append(f"tape file {tape_file}: zero-block file")
            files.append(WalkedFile(tape_file, position, 0, None, "structural damage: zero-block file"))
        else:
            files.append(classify_file(tape, tape_file, position, count, tape_uuid, block_size))
        if filemark is None:
            break
        position = filemark + 1
        tape_file += 1
    return files, damage


def second_pass_and_map(files: list[WalkedFile]) -> dict[str, Any]:
    """Section 12.3 second pass, then the Section 13.1 validation of the walked map."""
    parity_maps = [f for f in files if f.kind == KIND_PARITY_MAP and getattr(f, "parity_map", {}).get("final")]
    identified = []
    result: dict[str, Any] = {"parity_map": None, "identified": identified, "validated": False,
                              "entries": None, "reason": ""}
    if parity_maps:
        parity_file = parity_maps[0]
        parity_map = parity_file.parity_map  # type: ignore[attr-defined]
        result["parity_map"] = (parity_file.tape_file_number, parity_map)
        named = {entry["tape_file_number"]: entry for entry in parity_map["entries"]}
        for walked in files:
            entry = named.get(walked.tape_file_number)
            if walked.kind == KIND_OBJECT and entry is not None and walked.count == entry["total"]:
                walked.kind = KIND_SIDECAR
                walked.epoch_id = entry["epoch_id"]
                walked.start_ordinal = entry["start"]
                walked.end_ordinal = entry["end"]
                walked.note += "; identified as a sidecar by the second pass"
                identified.append(walked.tape_file_number)
    # Build the walked map with recounted Object ordinals.
    entries: list[MapEntry] = []
    ordinal = 0
    failed = [walked for walked in files if walked.kind is None]
    for walked in files:
        if walked.kind is None:
            break
        if walked.kind == KIND_OBJECT:
            entries.append(MapEntry(walked.tape_file_number, KIND_OBJECT, walked.count, ordinal))
            ordinal += walked.count
        elif walked.kind == KIND_SIDECAR:
            entries.append(MapEntry(walked.tape_file_number, KIND_SIDECAR, walked.count, None,
                                    walked.start_ordinal, walked.end_ordinal, walked.epoch_id))
        else:
            entries.append(MapEntry(walked.tape_file_number, walked.kind, walked.count))
    if failed:
        result["reason"] = "tape file(s) " + ", ".join(str(w.tape_file_number) for w in failed) + " could not be classified"
        result["error"] = "FilemarkMapReconstruct"
        return result
    if result["parity_map"] is None:
        result["reason"] = "no final ParityMap validates; no validated scope beyond the bootstrap's"
        result["entries"] = entries
        result["error"] = None
        return result
    parity_tape_file, parity_map = result["parity_map"]
    prefix = entries[: parity_tape_file + 1]
    checks = []
    if parity_map["scope"] != parity_tape_file + 1:
        checks.append("scope count is not the ParityMap's tape file number plus one")
    if canonical_digest(prefix) != parity_map["digest"]:
        checks.append("walked projection does not hash to canonical_map_digest")
    if derived_total(prefix) != parity_map["total"] or derived_watermark(prefix) != parity_map["watermark"]:
        checks.append("recomputed T or W differs from the scope fields")
    if checks:
        result["reason"] = "; ".join(checks)
        result["error"] = "FilemarkMapDigestMismatch"
        return result
    result["validated"] = True
    result["entries"] = prefix
    result["all_entries"] = entries
    result["error"] = None
    result["reason"] = "walked map validated against the final ParityMap"
    return result


# ----- the Recoverer -------------------------------------------------------


@dataclass
class RecoveryContext:
    tape: DamagedTape
    tape_uuid: bytes
    block_size: int
    scheme: tuple[int, int, int]
    entries: list[MapEntry]
    scope: int
    watermark: int
    directory: dict[int, dict[str, Any]] | None
    route: str


def gf_solve_row(rows: list[list[int]], target: int, k: int) -> list[int]:
    """Gauss-Jordan inverse of the k x k matrix `rows`; return the row for data index `target`."""
    matrix = [list(row) + [1 if i == j else 0 for j in range(k)] for i, row in enumerate(rows)]
    for column in range(k):
        pivot = next((r for r in range(column, k) if matrix[r][column]), None)
        if pivot is None:
            raise ReadFailure("ReedSolomon", "singular matrix")
        matrix[column], matrix[pivot] = matrix[pivot], matrix[column]
        inverse = gf_inv(matrix[column][column])
        matrix[column] = [gf_mul(inverse, value) for value in matrix[column]]
        for r in range(k):
            if r != column and matrix[r][column]:
                factor = matrix[r][column]
                matrix[r] = [value ^ gf_mul(factor, pivot_value) for value, pivot_value in zip(matrix[r], matrix[column])]
    return matrix[target][k:]


def combine(coefficients: list[int], shards: list[bytes]) -> bytes:
    size = len(shards[0])
    accumulator = 0
    for coefficient, shard in zip(coefficients, shards, strict=True):
        if coefficient:
            accumulator ^= int.from_bytes(shard.translate(_multiplication_table(coefficient)), "big")
    return accumulator.to_bytes(size, "big")


def acquire_index(ctx: RecoveryContext, sidecar: MapEntry, citations: list) -> tuple[dict[str, Any] | None, str]:
    """Section 13.3: acquire one validated header/index copy for the epoch, or none.

    Step 1 uses a valid footer, which decides unless an available directory
    entry disagrees with it. Step 2 falls back to the primary; an available
    entry then decides by hash, and without one the tail copy at H + P is
    read as well. Step 3 rescues the tail through an available entry, whose
    total must be 2H + P + 1 with H > 0. Otherwise the epoch is
    metadata-unavailable (step 4).
    """
    tape = ctx.tape
    start = lba_of_file(ctx.entries, sidecar.tape_file_number)
    count = sidecar.block_count
    _, m, stripes = ctx.scheme
    parity_blocks = stripes * m
    notes: list[str] = []
    citations.append(cite("footer_first"))

    # The directory entry, when one is available (step 3's preconditions).
    entry = ctx.directory.get(sidecar.tape_file_number) if ctx.directory is not None else None
    if entry is not None:
        if (entry["epoch_id"], entry["start"], entry["end"], entry["total"]) != (
                sidecar.epoch_id, sidecar.protected_ordinal_start, sidecar.protected_ordinal_end_exclusive, count):
            citations.append(cite("entry_agrees"))
            notes.append("directory entry disagrees with the map entry; no read is placed from it")
            entry = None
        elif entry["H"] == 0 or entry["total"] != 2 * entry["H"] + parity_blocks + 1:
            citations.append(cite("rescue_preconditions"))
            notes.append(f"directory entry total {entry['total']} is not 2H + P + 1 with H = {entry['H']} > 0, "
                         "so the entry is not available")
            entry = None

    def read_copy(first: int, blocks: int, kind: int, label: str) -> dict[str, Any] | None:
        try:
            copy = parse_sidecar_copy(read_blocks(tape, start + first, blocks), ctx.tape_uuid, ctx.block_size, kind)
        except MediumError as error:
            notes.append(f"{label} copy unreadable (LBA {error.args[0]})")
            return None
        except ReadFailure as failure:
            notes.append(f"{label} copy invalid ({failure.reason})")
            return None
        if copy["total"] != count:
            notes.append(f"{label} copy total differs from the map entry")
            return None
        return copy

    def unavailable() -> tuple[None, str]:
        citations.extend([cite("metadata_unavailable"), cite("unavailable_report")])
        return None, "; ".join(notes)

    # Step 1: footer first.
    footer = None
    try:
        footer = parse_sidecar_footer(tape.read_data(start + count - 1), ctx.tape_uuid)
        if (footer["epoch_id"], footer["start"], footer["end"], footer["total"]) != (
                sidecar.epoch_id, sidecar.protected_ordinal_start, sidecar.protected_ordinal_end_exclusive, count):
            notes.append("footer contradicts the map entry")
            footer = None
    except MediumError as error:
        notes.append(f"footer unreadable (LBA {error.args[0]})")
    except ReadFailure as failure:
        notes.append(f"footer invalid ({failure.reason})")
    if footer is not None:
        citations.append(cite("footer_decides"))
        disagree = entry is not None and (entry["hash"] != footer["hash"]
                                          or entry["H"] + parity_blocks != footer["tail_start"])
        if disagree:
            citations.append(cite("footer_entry_disagree"))
            notes.append("the directory entry disagrees with the valid footer, so neither decides")
        copies = {}
        for name, first, kind in (("primary", 0, 1), ("tail", footer["tail_start"], 2)):
            if disagree and name == "tail" and entry["H"] + parity_blocks != footer["tail_start"]:
                notes.append("tail copy position is contradicted; not used")
                continue
            copy = read_copy(first, footer["H"], kind, name)
            if copy is None:
                continue
            if not copy_matches_footer(copy, footer) or (disagree and copy["hash"] != entry["hash"]):
                notes.append(f"{name} copy is contradicted by the footer or the directory entry")
                continue
            copies[name] = copy
        if not copies:
            return unavailable()
        chosen = "primary" if "primary" in copies else "tail"
        if len(copies) == 2 or chosen == "tail":
            citations.append(cite("sidecar_copy_agree"))
        return copies[chosen], "; ".join(notes + [f"footer valid; {chosen} copy used"])

    # Step 2: primary fallback.
    citations.append(cite("primary_fallback"))
    primary = None
    try:
        header_blocks = rd64(tape.read_data(start), 0x60)
        if 1 <= header_blocks <= count:
            primary = read_copy(0, header_blocks, 1, "primary")
        else:
            notes.append("primary copy invalid (implausible H)")
    except MediumError as error:
        notes.append(f"primary copy unreadable (LBA {error.args[0]})")
    except ReadFailure as failure:
        notes.append(f"primary copy invalid ({failure.reason})")
    if primary is not None:
        if entry is not None:
            citations.append(cite("entry_decides"))
            if primary["hash"] == entry["hash"]:
                return primary, "; ".join(notes + ["primary copy used; its hash equals the directory entry's"])
            notes.append("primary copy's hash differs from the directory entry's")
        else:
            citations.append(cite("read_tail_too"))
            tail_first = primary["H"] + parity_blocks
            tail = None
            if tail_first + primary["H"] <= count:
                tail = read_copy(tail_first, primary["H"], 2, "tail")
            else:
                notes.append("tail copy position lies outside the file")
            if tail is not None and tail["hash"] != primary["hash"]:
                citations.extend([cite("tail_differs_unavailable"), cite("sidecar_copy_agree")])
                notes.append("valid tail copy's hash differs from the primary's; nothing decides between them")
                return unavailable()
            return primary, "; ".join(notes + ["primary copy used (fallback)"])

    # Step 3: directory-assisted tail rescue.
    citations.extend([cite("tail_rescue"), cite("entry_available")])
    if entry is None:
        if ctx.directory is None:
            notes.append("no sidecar epoch directory entry is available (no final ParityMap validates)")
        return unavailable()
    citations.extend([cite("entry_agrees"), cite("rescue_block")])
    tail_first = entry["H"] + parity_blocks
    tail = read_copy(tail_first, entry["H"], 2, "tail") if tail_first + entry["H"] <= count else None
    if tail is not None and tail["hash"] == entry["hash"]:
        return tail, "; ".join(notes + ["tail copy used (directory-assisted rescue)"])
    if tail is not None:
        notes.append("tail copy's hash differs from the directory entry's")
    return unavailable()


def recover_address(ctx: RecoveryContext, address: list[int]) -> dict[str, Any]:
    tape_file, block_index = address
    result: dict[str, Any] = {"address": list(address), "epoch": None, "result": None, "error": None,
                              "stripe": None, "lost": None, "limit": None, "bytes_match": None, "citations": []}
    citations = result["citations"]
    citations.append(cite("recoverer_inputs"))
    if tape_file >= ctx.scope or tape_file >= len(ctx.entries):
        citations.append(cite("refusals"))
        result.update(result="error", error="OutsideValidatedMapPrefix")
        return result
    entry = ctx.entries[tape_file]
    if entry.kind != KIND_OBJECT or block_index >= entry.block_count:
        citations.append(cite("refusals"))
        result.update(result="error", error="OutsideValidatedMapPrefix")
        return result
    ordinal = entry.first_parity_data_ordinal + block_index
    if ordinal >= ctx.watermark:
        citations.append(cite("refusals"))
        result.update(result="error", error="UnrecoverablePendingEpoch")
        return result
    citations.append(cite("stripe_map"))
    sidecars = [e for e in ctx.entries[: ctx.scope] if e.kind == KIND_SIDECAR
                and e.protected_ordinal_start <= ordinal < e.protected_ordinal_end_exclusive]
    if len(sidecars) != 1:
        result.update(result="error", error="OutsideValidatedMapPrefix")
        return result
    sidecar = sidecars[0]
    result["epoch"] = sidecar.epoch_id
    index, note = acquire_index(ctx, sidecar, citations)
    result["index_acquisition"] = note
    if index is None:
        result.update(result="error", error="SidecarMetadataUnavailable")
        citations.append(cite("err_metadata_unavailable"))
        return result
    citations.append(cite("pin_scheme"))
    k, m, stripes = ctx.scheme
    if (index["k"], index["m"], index["S"], index["block_size"]) != (k, m, stripes, ctx.block_size) or (
            index["start"], index["end"]) != (sidecar.protected_ordinal_start, sidecar.protected_ordinal_end_exclusive):
        citations.append(cite("err_scheme"))
        result.update(result="error", error="SchemeMismatch")
        return result
    offset = ordinal - index["start"]
    stripe = offset % stripes
    target = offset // stripes
    result["stripe"] = stripe
    sidecar_start = lba_of_file(ctx.entries, sidecar.tape_file_number)
    matrix = cauchy_matrix(k, m)
    zero = bytes(ctx.block_size)
    positions = []  # (row, shard or None, status)
    erasures = 0
    for data_index in range(k):
        peer = index["start"] + data_index * stripes + stripe
        row = [1 if column == data_index else 0 for column in range(k)]
        if peer >= index["end"]:
            positions.append((row, zero, "implicit"))
            continue
        if data_index == target:
            positions.append((row, None, "erasure (the failed block)"))
            erasures += 1
            continue
        holder = next((e for e in ctx.entries[: ctx.scope] if e.kind == KIND_OBJECT
                       and e.first_parity_data_ordinal <= peer < e.first_parity_data_ordinal + e.block_count), None)
        if holder is None:
            positions.append((row, None, "erasure (outside the durable boundary)"))
            erasures += 1
            continue
        lba = lba_of_file(ctx.entries, holder.tape_file_number) + peer - holder.first_parity_data_ordinal
        try:
            shard = ctx.tape.read_data(lba)
        except (MediumError, ReadFailure):
            positions.append((row, None, f"erasure (read failure at LBA {lba})"))
            erasures += 1
            continue
        if crc64_xz(shard) != index["data_crcs"][peer - index["start"]]:
            positions.append((row, None, f"erasure (CRC mismatch at LBA {lba})"))
            erasures += 1
            continue
        positions.append((row, shard, "trusted"))
    for parity_index in range(m):
        lba = sidecar_start + index["H"] + parity_index * stripes + stripe
        row = list(matrix[parity_index])
        try:
            shard = ctx.tape.read_data(lba)
        except (MediumError, ReadFailure):
            positions.append((row, None, f"erasure (read failure at LBA {lba})"))
            erasures += 1
            continue
        if crc64_xz(shard) != index["parity_crcs"][(stripe, parity_index)]:
            positions.append((row, None, f"erasure (CRC mismatch at LBA {lba})"))
            erasures += 1
            continue
        positions.append((row, shard, "trusted"))
    citations.extend([cite("trusted"), cite("erasure")])
    if any(status == "implicit" for _, _, status in positions):
        citations.append(cite("implicit"))
    result["peers"] = [status for _, _, status in positions]
    if erasures > m:
        citations.append(cite("unrecoverable"))
        result.update(result="error", error="Unrecoverable", lost=erasures, limit=m)
        return result
    usable = [(row, shard) for row, shard, status in positions if shard is not None][:k]
    citations.append(cite("reconstruct"))
    inverse_row = gf_solve_row([row for row, _ in usable], target, k)
    block = combine(inverse_row, [shard for _, shard in usable])
    citations.append(cite("verify_crc"))
    if crc64_xz(block) != index["data_crcs"][ordinal - index["start"]]:
        result.update(result="error", error="Unrecoverable", lost=erasures, limit=m)
        return result
    result["result"] = "recovered"
    result["_block"] = block
    return result


# ---------------------------------------------------------------------------
# The Resumer (Section 14, and Section 3.4 as far as a portable committed
# prefix reaches).
# ---------------------------------------------------------------------------

QUOTES.update({
    "resume_after": ("14", "A later session appends **after the last committed tape file** — not after the last object, and not at the watermark."),
    "resume_step1": ("14", "Derive the committed prefix from the off-tape commit records (Section 3.4), dropping the tape's torn tail, if any, and compute `W` and `T` from it."),
    "resume_step2": ("14", "Enforce the version-1 bound: `T − W < S × k` (at most one open epoch)."),
    "resume_step2_rules": ("14", "`W ≤ T`; committed sidecar ranges MUST be contiguous from zero through `W`; epoch ids MUST be consecutive; and the prefix's final object entry MUST end exactly at `T`. A violation is `ResumeAppend`."),
    "resume_step3": ("14", "Rebuild the open epoch by **re-reading ordinals `[W, T)` from the committed prefix on tape** — a boundary or short read where data is expected is fatal — recomputing per-block CRCs and re-accumulating parity."),
    "resume_step4": ("14", "**Position to the append point (`Σ(block_count + 1)` over the prefix) before writing anything.**"),
    "resume_first_block": ("14", "The first block written lands exactly at the append point; a write issued anywhere else could land over committed data or short of the append point."),
    "resume_superseded": ("14", "Anything physically on tape beyond the committed prefix is superseded by the next append and MUST NOT be trusted for recovery."),
    "resume_validated_records": ("3.4", "Before a Resumer positions to an append point or writes, the validated combination of the records it relies on MUST determine exactly one committed prefix and its append point."),
    "append_point": ("3.2", "The append point after a committed prefix is `Σ(block_count + 1)` over all committed files."),
    "w_le_t": ("2.3", "Always `W ≤ T`."),
    "derived_scalars": ("7.2", "Tape file numbers are dense from 0. Object first-ordinals are dense and contiguous from 0 in tape order (Section 3.2)."),
    "epoch_ids": ("9.2", "Epoch ids MUST increase by one but carry no range arithmetic."),
    "epoch_close": ("11.2", "At `S × k` data blocks the epoch closes into a **pending** sidecar; no tape I/O occurs mid-object."),
    "pending_emit": ("11.2", "Pending sidecars are emitted as tape files when the current object closes."),
    "barrier_short": ("11.2", "A barrier may close a non-empty short epoch and emit its sidecar without `FINAL_PARTIAL_EPOCH`."),
    "boundary_outcomes": ("3.5", "Fixed-block reads and writes only; a read returning other than exactly one block is an error, with two classified boundary outcomes: **Filemark** and **EndOfData**."),
    "err_resume": ("15", "ResumeAppend                    Section 14 invariant violation"),
    "torn_superseded": ("3.4", "There is no on-tape commit marker and no on-tape \"unclean\" marker: an interrupted tail simply lies beyond the last committed file and is physically superseded on resume (Section 14)."),
    "finalized_no_append": ("11.3", "A finalized tape accepts no further appends."),
})


def prefix_from_json(entries: list[Mapping[str, Any]]) -> list[MapEntry]:
    return structural_from_json(entries)


def step2_violations(prefix: list[MapEntry], watermark: int, total: int) -> list[str]:
    """The Section 14 step-2 rules that need no scheme, each violation named."""
    violations = []
    if watermark > total:
        violations.append(f"W ≤ T fails: W = {watermark}, T = {total}")
    chain = 0
    for entry in [e for e in prefix if e.kind == KIND_SIDECAR]:
        if entry.protected_ordinal_start != chain:
            violations.append(f"sidecar ranges not contiguous from zero: tape file {entry.tape_file_number} starts at "
                              f"{entry.protected_ordinal_start}, not {chain}")
            break
        chain = entry.protected_ordinal_end_exclusive
    else:
        if chain != watermark:
            violations.append(f"sidecar ranges do not reach W: they end at {chain}, W = {watermark}")
    epochs = [e.epoch_id for e in prefix if e.kind == KIND_SIDECAR]
    if epochs != list(range(len(epochs))):
        violations.append(f"epoch ids not consecutive from 0: {epochs}")
    objects = [e for e in prefix if e.kind == KIND_OBJECT]
    final_end = objects[-1].first_parity_data_ordinal + objects[-1].block_count if objects else 0
    if final_end != total:
        violations.append(f"the final Object entry ends at {final_end}, not at T = {total}")
    finalizing = [e.tape_file_number for e in prefix if e.kind in (KIND_PARITY_MAP, KIND_REPLICA, KIND_SEPARATION)]
    if finalizing:
        violations.append(f"the prefix records a final ParityMap or a terminal component (tape file(s) "
                          f"{', '.join(map(str, finalizing))}): finalization has begun")
    return violations


def prefix_map_findings(prefix: list[MapEntry]) -> list[str]:
    """Section 7.2 validity findings that step 2 does not list (reported, not decisive)."""
    findings = []
    ordinal = 0
    for index, entry in enumerate(prefix):
        if entry.tape_file_number != index:
            findings.append(f"tape file numbers not dense at entry {index}")
        if entry.kind == KIND_OBJECT:
            if entry.first_parity_data_ordinal != ordinal:
                findings.append(f"Object at tape file {entry.tape_file_number} has first ordinal "
                                f"{entry.first_parity_data_ordinal}, not the running total {ordinal}")
            ordinal = entry.first_parity_data_ordinal + entry.block_count
    return findings


def empty_resume_decision(image_name: str) -> dict[str, Any]:
    return {
        "image": image_name,
        "prefix": {"entries": None, "derived_W": None, "derived_T": None, "given_W": None, "given_T": None,
                   "W_equal": None, "T_equal": None, "append_point_lba": None, "map_findings": [], "citations": []},
        "scheme": {"k": None, "m": None, "S": None, "source": None},
        "step2": {"result": "not_run", "violations": [], "citations": []},
        "step3": {"result": "not_run", "ordinals": [], "lbas": [], "failure": None, "citations": []},
        "step4": {"result": "not_run", "append_point_lba": None, "citations": []},
        "decision": {"result": None, "error": None, "refused_at": None, "before_any_tape_read": None,
                     "before_any_write": None, "records_read": [], "citations": []},
        "append": None,
        "undecided": [],
    }


def resume_case(case: Mapping[str, Any], image: ImageBuild,
                trace: dict[str, Any]) -> tuple[dict[str, Any], ImageBuild | None]:
    """Act as a Resumer on the undamaged image with the given prefix as commit authority."""
    decision = empty_resume_decision(case["image"])
    tape = DamagedTape(image.records(), set())
    reads: list[int] = []

    def read(lba: int) -> Any:
        reads.append(lba)
        return tape.read(lba)

    # Step 1: the prefix is the commit authority; W and T come from it (Section 7.2).
    prefix = prefix_from_json(case["committed_prefix"])
    watermark = derived_watermark(prefix)
    total = derived_total(prefix)
    append_point = sum(entry.block_count + 1 for entry in prefix)
    p = decision["prefix"]
    p.update(entries=len(prefix), derived_W=watermark, derived_T=total, given_W=case["W"], given_T=case["T"],
             W_equal=watermark == case["W"], T_equal=total == case["T"], append_point_lba=append_point,
             map_findings=prefix_map_findings(prefix))
    p["citations"].extend([cite("resume_step1"), cite("resume_validated_records"), cite("append_point")])
    refusal = decision["decision"]

    def refuse_now(at: str, reason: str) -> tuple[dict[str, Any], None]:
        refusal.update(result="refused", error="ResumeAppend", refused_at=at, before_any_tape_read=not reads,
                       before_any_write=True, records_read=list(reads))
        refusal["citations"].extend([cite("resume_validated_records"), cite("err_resume")])
        decision["step2"]["violations"].append(reason)
        return decision, None

    def refuse(at: str, error: str, citations: list[str]) -> tuple[dict[str, Any], None]:
        refusal.update(result="refused", error=error, refused_at=at, before_any_tape_read=not reads,
                       before_any_write=True, records_read=list(reads))
        refusal["citations"].extend(cite(key) for key in citations)
        return decision, None

    ends = [e.first_parity_data_ordinal + e.block_count for e in prefix if e.kind == KIND_OBJECT]
    if overflows(append_point, total, watermark, *ends):
        return refuse_now("step 1", "W, T or the append point overflows u64 (checked arithmetic, Section 2.4), so "
                                    "the records do not determine one committed prefix and append point")

    # Step 2, the rules that need no scheme.
    step2 = decision["step2"]
    step2["citations"].extend([cite("resume_step2"), cite("resume_step2_rules")])
    violations = step2_violations(prefix, watermark, total)
    if violations:
        step2.update(result="violation", violations=violations)
        if any("W ≤ T" in v for v in violations):
            step2["citations"].append(cite("w_le_t"))
        if any("epoch ids" in v for v in violations):
            step2["citations"].append(cite("epoch_ids"))
        if any("finalization has begun" in v for v in violations):
            step2["citations"].append(cite("resume_finalizing"))
        return refuse("step 2", "ResumeAppend", ["resume_step2_rules", "err_resume"])

    # The step-2 bound needs S and k, which the prefix does not carry. The text
    # does not say where a Resumer obtains them; this one reads the bootstrap.
    try:
        block = read(0)
        boot = parse_bootstrap(block, len(block))
    except (MediumError, ReadFailure) as failure:
        reason = failure.reason if isinstance(failure, ReadFailure) else f"medium error at LBA {failure.args[0]}"
        step2.update(result="undecided", violations=[f"the scheme is unavailable: bootstrap {reason}"])
        return refuse("step 2", "undecided", ["resume_step2"])
    k, m, stripes = boot["scheme"]
    decision["scheme"].update(k=k, m=m, S=stripes, source="bootstrap at LBA 0")
    if not total - watermark < stripes * k:
        step2.update(result="violation",
                     violations=[f"T − W = {total - watermark} is not below S × k = {stripes * k}"])
        refuse("step 2", "ResumeAppend", ["resume_step2", "err_resume"])
        refusal["before_any_tape_read"] = "undecided"
        decision["undecided"].append({
            "aspect": "decision.before_any_tape_read",
            "readings": [
                "true: the Resumer's off-tape state carries the scheme, so the step-2 bound is applied before any "
                "tape read",
                "false: the scheme comes from the bootstrap, so the Resumer reads LBA 0 before it can apply the "
                "step-2 bound; this Resumer does so",
            ],
            "citations": [cite("resume_step2"), cite("resume_step1")],
        })
        return decision, None
    step2["result"] = "pass"

    # Step 3: re-read [W, T) from the committed prefix on tape.
    step3 = decision["step3"]
    step3["citations"].append(cite("resume_step3"))
    open_blocks: dict[int, bytes] = {}
    for ordinal in range(watermark, total):
        holder = next(e for e in prefix if e.kind == KIND_OBJECT
                      and e.first_parity_data_ordinal <= ordinal < e.first_parity_data_ordinal + e.block_count)
        lba = lba_of_file(prefix, holder.tape_file_number) + ordinal - holder.first_parity_data_ordinal
        step3["ordinals"].append(ordinal)
        step3["lbas"].append(lba)
        try:
            value = read(lba)
        except MediumError:
            step3.update(result="fatal", failure=f"ordinal {ordinal} at LBA {lba}: a medium error where data is expected")
            return refuse("step 3", "TapeIo", ["resume_step3", "resume_step3_errors", "err_tapeio"])
        if not isinstance(value, bytes) or len(value) != boot["block_size"]:
            found = value if isinstance(value, str) else f"a {len(value)}-byte record"
            step3.update(result="fatal", failure=f"ordinal {ordinal} at LBA {lba}: {found} where data is expected")
            step3["citations"].append(cite("boundary_outcomes"))
            return refuse("step 3", "ResumeAppend", ["resume_step3", "resume_step3_errors", "err_resume"])
        open_blocks[ordinal] = value
    step3["result"] = "run"

    # Step 4: position to the append point, then write the Object and end the session.
    step4 = decision["step4"]
    step4.update(result="run", append_point_lba=append_point)
    step4["citations"].extend([cite("resume_step4"), cite("resume_first_block"), cite("resume_superseded")])
    block_size = boot["block_size"]
    stored, layout = build_rem_object(case["append_object"], block_size, 0)
    new_blocks = [stored[i : i + block_size] for i in range(0, len(stored), block_size)]
    data = dict(open_blocks)
    ordinal = total
    epoch_start = watermark
    next_epoch = len([e for e in prefix if e.kind == KIND_SIDECAR])
    files: list[TapeFile] = []
    physical = image.records()
    lba = 0
    for entry in prefix:
        blocks = physical[lba : lba + entry.block_count]
        files.append(TapeFile(entry.tape_file_number, entry.kind, blocks, True, {}))
        lba += entry.block_count + 1
    object_tape_file = len(files)
    files.append(TapeFile(object_tape_file, KIND_OBJECT, new_blocks, True, {}))
    first_ordinal = ordinal
    sidecars: list[tuple[int, int, int, bool]] = []
    for block in new_blocks:
        data[ordinal] = block
        ordinal += 1
        if ordinal - epoch_start == stripes * k:
            sidecars.append((next_epoch, epoch_start, ordinal, False))
            next_epoch += 1
            epoch_start = ordinal
    if ordinal > epoch_start:
        sidecars.append((next_epoch, epoch_start, ordinal, True))
    sidecar_reports = []
    for epoch_id, start, end, short in sidecars:
        built = build_sidecar(boot["tape_uuid"], block_size, (k, m, stripes), epoch_id, start, end, data.__getitem__)
        files.append(TapeFile(len(files), KIND_SIDECAR, built.blocks, True, {}))
        sidecar_reports.append({"epoch_id": epoch_id, "protected_ordinal_start": start,
                                "protected_ordinal_end_exclusive": end, "total_blocks": built.total_blocks,
                                "closed_by": "session-end barrier (short epoch)" if short else "Object close (full epoch)",
                                "tape_file": len(files) - 1})
    result_name = trace.get("case_id", "resumed")
    result = ImageBuild(result_name, block_size, boot["tape_uuid"], (k, m, stripes), files, [], [], None, False)
    rows = image_rows(result)
    for report in sidecar_reports:
        report["first_lba"] = int(rows[report["tape_file"]]["start_record"])
    manifest = layout["manifest"]
    decision["append"] = {
        "object_tape_file": object_tape_file,
        "object_first_lba": append_point,
        "object_blocks": len(new_blocks),
        "object_first_ordinal": first_ordinal,
        "object_row": {"1": object_tape_file, "2": "plaintext", "3": len(new_blocks),
                       "4": case["append_object"]["options"]["object_id"].encode("utf-8").hex(),
                       "10": manifest.first_chunk_lba, "11": manifest.size_bytes, "12": manifest.chunk_count,
                       "13": layout["manifest_sha256"].hex()},
        "sidecars": sidecar_reports,
        "tape_files": rows,
        "eod": int(rows[-1]["eod_record"]),
        "uninterrupted_equal": None,
        "uninterrupted_differences": [],
    }
    if any(r["closed_by"].startswith("session-end") for r in sidecar_reports):
        decision["append"]["session_end_citations"] = [cite("barrier_short")]
    else:
        decision["append"]["session_end_citations"] = [cite("epoch_close"), cite("pending_emit")]
    refusal.update(result="accepted", error=None, refused_at=None, before_any_tape_read=False,
                   before_any_write=None, records_read=list(reads))
    refusal["citations"].extend([cite("resume_after"), cite("torn_superseded")])
    return decision, result


def uninterrupted_build(case: Mapping[str, Any], image_inputs: Mapping[str, Any]) -> ImageBuild:
    """My own check: the prefix's Objects and the appended Object in one session, ended by a barrier."""
    prefix = prefix_from_json(case["committed_prefix"])
    count = len([e for e in prefix if e.kind == KIND_OBJECT])
    inputs = json.loads(json.dumps(image_inputs))
    inputs["objects"] = inputs["objects"][:count] + [case["append_object"]]
    inputs["checkpoint_after_objects"] = [c for c in inputs["checkpoint_after_objects"] if c < count]
    inputs["stop"] = {"kind": "session-end"}
    return build_image(inputs, "uninterrupted")


def compare_tapes(resumed: ImageBuild, other: ImageBuild) -> list[str]:
    differences = []
    if len(resumed.files) != len(other.files):
        differences.append(f"tape-file count: resumed {len(resumed.files)}, uninterrupted {len(other.files)}")
    for mine, theirs in zip(resumed.files, other.files):
        a, b = b"".join(mine.blocks), b"".join(theirs.blocks)
        if a != b or mine.has_filemark != theirs.has_filemark:
            index = first_difference(a, b)
            differences.append(f"tape file {mine.tape_file_number}: first differing byte {index}")
    return differences


def run_resume(case_paths: list[pathlib.Path], out_path: pathlib.Path) -> dict[str, Any]:
    images: dict[str, ImageBuild] = {}
    cases: dict[str, Any] = {}
    for path in sorted(case_paths, key=case_id_of):
        case = load_json(path)
        name = case["image"]
        if name not in images:
            images[name] = build_image(load_image_inputs(name), name)
        trace = {"case_id": case_id_of(path)}
        decision, resumed = resume_case(case, images[name], trace)
        if resumed is not None:
            # Reporting-only, after the decision: compare with one uninterrupted session.
            other = uninterrupted_build(case, load_image_inputs(name))
            differences = compare_tapes(resumed, other)
            decision["append"]["uninterrupted_equal"] = not differences
            decision["append"]["uninterrupted_differences"] = differences
        cases[case_id_of(path)] = decision
    output = {"schema": "rem-parity-second-implementation-resume/1", "cases": cases}
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(output, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    return output


def resume_summary(decision: dict[str, Any]) -> str:
    d = decision["decision"]
    text = f"{decision['image']}: W {decision['prefix']['derived_W']} T {decision['prefix']['derived_T']}; {d['result']}"
    if d["result"] == "refused":
        text += f" at {d['refused_at']} ({d['error']}); reads {d['records_read'] or 'none'}"
    else:
        a = decision["append"]
        text += (f"; re-read LBAs {decision['step3']['lbas'] or 'none'}; append at LBA {decision['step4']['append_point_lba']}; "
                 f"Object tape file {a['object_tape_file']}; sidecar(s) " +
                 ", ".join(f"file {s['tape_file']} epoch {s['epoch_id']} [{s['protected_ordinal_start']}, "
                           f"{s['protected_ordinal_end_exclusive']})" for s in a["sidecars"]) +
                 f"; EOD {a['eod']}; equals uninterrupted: {a['uninterrupted_equal']}")
    return text


# ---------------------------------------------------------------------------
# Negative cases (design D6): apply a text-level mutation to my own bytes,
# then decide the outcome under the text and run the target role.
# ---------------------------------------------------------------------------

QUOTES.update({
    "checked": ("2.4", "Arithmetic on values read from tape MUST be checked; overflow is rejection, never wraparound (Section 16.2)."),
    "hostile_checked": ("16.2", "Every declared count or length is validated against the measured physical extent. All arithmetic on tape-derived values is checked. Reserved fields and declared zero-fill MUST be verified zero."),
    "validity_6_6": ("6.6", "k ≥ 2      1 ≤ m ≤ k      S ≥ 1      k + m ≤ 255      S × (k + m) ≤ 2³² − 1"),
    "boot_scheme_valid": ("8.2", "For a parity bootstrap the scheme record's `scheme_id` MUST be `rs-cauchy-gf256-v1` and `(k, m, S)` MUST satisfy Section 6.6 validity."),
    "boot_parse_order": ("8.1", "Parse order: block length ≥ 0x40 → magic → header CRC → schema_major → payload bounds (checked against the block) → payload CRC → CBOR."),
    "err_boot_parse": ("15", "BootstrapParse                  bootstrap frame or payload violates Section 8"),
    "err_boot_large": ("15", "BootstrapPayloadTooLarge        framed BOT payload cannot fit the block"),
    "err_sidecar": ("15", "SidecarParse                    sidecar structure violates Section 9"),
    "err_pm": ("15", "ParityMapParse                  final ParityMap violates Section 10.1"),
    "err_directory": ("15", "DirectoryInvalid                well-formed sidecar epoch directory breaks an invariant of Section 10.1.5"),
    "err_replica": ("15", "TerminalIndexReplicaParse       replica framing or fixed-slot payload violates Section 8.3, 10.2–10.4, or 10.6"),
    "err_separation": ("15", "TerminalIndexSeparationParse    separation extent violates Section 8.3, 10.5, or 10.6"),
    "pm_category": ("15", "The name is normative as a category: this document does not specify which of the two a Reader reports once it has failed both ParityMap copies."),
    "sc_k": ("9.2", "| 0x20 | 2 | k u16 | ≠ 0 |"),
    "sc_m": ("9.2", "| 0x22 | 2 | m u16 | ≠ 0 |"),
    "sc_S": ("9.2", "| 0x24 | 4 | S u32 | ≠ 0 |"),
    "sc_block_size": ("9.2", "| 0x28 | 4 | block_size u32 | MUST equal the actual block size |"),
    "sc_uuid": ("9.2", "| 0x08 | 16 | tape_uuid | MUST match the bootstrap or, when the bootstrap is unreadable, the tape UUID supplied under Section 8.4.1 |"),
    "sc_end": ("9.2", "| 0x38 | 8 | protected_ordinal_end_exclusive u64 | > start |"),
    "sc_logical": ("9.2", "| 0x40 | 8 | logical_shard_count u64 | MUST = S × k |"),
    "sc_real": ("9.2", "| 0x48 | 8 | real_data_shard_count u64 | = end − start; ≤ logical_shard_count |"),
    "sc_real_text": ("9.2", "`real_data_shard_count` MUST equal `protected_ordinal_end_exclusive − protected_ordinal_start` and MUST be in `1..=S × k`."),
    "sc_P": ("9.2", "| 0x50 | 8 | parity_block_count u64 | MUST = S × m |"),
    "sc_crc_count": ("9.2", "| 0x58 | 8 | data_crc_count u64 | MUST = real_data_shard_count |"),
    "sc_H": ("9.2", "| 0x60 | 8 | sidecar_header_block_count u64 (H) | MUST equal the recomputed layout (Section 9.4) |"),
    "sc_inline": ("9.2", "| 0x68 | 8 | inline_index_entry_bytes u64 | MUST equal the recomputed layout (Section 9.4) |"),
    "sc_total": ("9.2", "| 0x70 | 8 | sidecar_total_block_count u64 | = 2H + P + 1 |"),
    "sc_primary_start": ("9.2", "| 0x78 | 8 | primary_header_start_block u64 | MUST = 0 |"),
    "sc_tail_start": ("9.2", "| 0x80 | 8 | tail_header_start_block u64 | MUST = H + P |"),
    "sc_footer_index": ("9.2", "| 0x88 | 8 | footer_block_index u64 | MUST = 2H + P |"),
    "sc_copy_kind": ("9.2", "| 0x90 | 2 | copy_kind u16 | 1 = primary, 2 = tail |"),
    "sc_reserved92": ("9.2", "| 0x92 | 2 | reserved | MUST be 0 |"),
    "sc_generation": ("9.2", "| 0x94 | 4 | copy_generation u32 | MUST be 0 while sidecar `schema_version` = 2 |"),
    "sc_reservedB8": ("9.2", "| 0xB8 | 8 | reserved u64 | MUST be 0 |"),
    "sc_fill": ("9.2", "| … | | zero fill | MUST be zero up to offset block_size − 8 |"),
    "sc_entry_reserved": ("9.3", "**Parity entry (16 bytes)**: u32 stripe_index; u16 parity_index; u16 reserved, MUST be 0; u64 parity_shard_crc64."),
    "sc_unused_zero": ("9.3", "Every index block — block 0 and each spill block — ends with a u64 LE CRC-64/XZ over its bytes 0..block_size−8, and unused space below the CRC MUST be zero."),
    "sc_recompute": ("9.4", "Readers MUST recompute both and reject header values that disagree."),
    "sc_hash_items": ("9.5", "the header's exact wire bytes 0x00 through 0x8F inclusive — magic through `footer_block_index`, with `primary_header_start_block` as 0 — i.e. every field *before* `copy_kind`;"),
    "sc_hash_verify": ("9.5", "Readers MUST verify the hash on every index parse."),
    "ft_reserved": ("9.6", "| 0x0A | 2 + 4 | reserved, MUST be 0 |"),
    "ft_tail": ("9.6", "| 0x58 | 8 | tail_header_start_block u64 = H + P |"),
    "ft_fill": ("9.6", "| 0x88… | | zero fill, MUST be zero |"),
    "pin_then": ("13.3", "The acquired index MUST then be pinned against the bootstrap's scheme record (`k`, `m`, `S`, block size), or against the supplied scheme and block size when the bootstrap is unreadable, and against the map entry's ordinal range; disagreement is `SchemeMismatch`."),
    "geometry_c": ("C", "Geometry and ordinal-range disagreements raise `SchemeMismatch`, as this document specifies, rather than a generic parse error."),
    "pm_M": ("10.1.2", "With payload length `L`, block size `B`, and `M = ceil((0xC8 + L) / B)` blocks per copy:"),
    "pm_min": ("10.1.2", "The minimum block size for ParityMap files is 0xC8."),
    "pm_locator": ("10.1.4", "Readers MUST validate the locator arithmetic (`M` from `payload_len` and `block_size`; total = 2M + 1; the three start indices) and reject disagreement between header, footer, and the measured tape file length."),
    "pm_match": ("10.1.4", "The decoded payload MUST match the header/footer locator fields (UUID, sequence, digest, scope, and `is_final_directory`) and the payload bytes MUST hash to `payload_sha256`."),
    "dir_rule": ("10.1.5", "Whenever a Reader decodes a sidecar epoch directory, it MUST validate all of the following invariants; a violation is `DirectoryInvalid` (Section 15)."),
    "dir_ascending": ("10.1.5", "entries strictly ascending by `tape_file_number`, each `< scope_tape_file_count`;"),
    "dir_nonzero": ("10.1.5", "non-zero `sidecar_total_block_count`, `sidecar_header_block_count`, and `parity_shard_block_count`;"),
    "row_manifest": ("10.3", "For plaintext rows, `manifest_chunk_count` and `manifest_size_bytes` MUST be positive, the manifest chunk range MUST fit within `stored_block_count`, and the manifest byte length MUST fit within `manifest_chunk_count × block_size_bytes`."),
    "row_id": ("10.3", "| 4 | bytes, 1–64 | REQUIRED |"),
    "close_reserve": ("10.3", "A tape's close reserve is the space its finalization needs: parity closeout, the checked terminal payload `64 × structural_row_count + 256 × object_row_count` in three rounded replica records, both separation extents, and five filemark charges."),
    "size_formulas": ("10.6", "Every size formula of Section 10.4 evaluates without overflow, and the recorded geometry fields equal its results."),
    "layout_valid": ("10.6", "The planned layout of the terminal suffix validates against Section 8.3, and its layout digest recomputes to the recorded value."),
    "conjunction": ("10.6", "The conditions are a conjunction, and this document does not fix the order in which a Reader checks them, except that a matching role magic commits the tape file to its control type, so malformed control never falls through to Object (Section 12.3)."),
    "covered_equal": ("10.6", "`covered_prefix_tape_file_count`, `structural_row_count`, and replica A's planned tape-file number are equal."),
    "w_equals_t": ("10.6", "when sidecars are present, `highest_protected_ordinal` equals `total_data_ordinals`, so finalization left no Object ordinal unprotected;"),
    "sep_arith": ("10.6", "The footer arithmetic of Section 10.4 applies to its footer fields (`0x1A8`, `0x1B0`, `0x1B8`, `0x1C0`)."),
    "sep_valid": ("10.6", "A separation extent is valid when its header and footer satisfy the conditions in the first list above, read against the fields of Section 10.5 in place of those of Section 10.4, with two exceptions:"),
    "sep_records": ("10.5", "total_records    = ceil(E / B), required ≥ 2"),
    "footer_delta": ("10.4", "In a footer, the backward start delta (`0x2F8`) MUST equal the observed record count (`0x2E8`) minus one, and the observed footer LBA (`0x2F0`) MUST equal the observed start LBA (`0x2E0`) plus that delta."),
    "eod_rule": ("8.3", "Logical start positions advance by `record_count + 1`, where the `1` is the actual trailing filemark. EOD is the start of C plus C's record count plus one filemark."),
    "digest_scope": ("7.4", "When a Reader validates a terminal digest, it MUST recompute the digest over exactly the leading `tape_file_count` entries and cross-check all three scalars."),
    "lba_formula": ("3.2", "`LBA(f, b) = Σ_{g<f}(block_count(g) + 1) + b`."),
    "stripe_inverse": ("3.3", "inverse:  o = start + data_index·S + stripe"),
    "parity_block_formula": ("9.1", "block(stripe, parity_index) = H + parity_index·S + stripe"),
    "sep_magic_candidate": ("8.4", "A magic or CRC miss invalidates that candidate."),
    "item5": ("12.3", "**Sidecar (primary)**: the primary header parses, and the measured block count MUST equal the header's `sidecar_total_block_count`."),
})

NEG_CASES_DEFAULT = pathlib.Path("/dev/null")


class Workspace:
    """Copy-on-write blocks of one base artifact: an image or a terminal profile."""

    def __init__(self, image: ImageBuild | None = None, profile: str | None = None) -> None:
        self.image = image
        self.profile = profile
        self.blocks: dict[tuple, bytearray] = {}
        self.touched: list[tuple] = []
        if profile is not None:
            self.inputs = load_json(FIXTURE_ROOT / profile / "inputs.json")
            self.terminal = build_terminal_suffix(terminal_inputs_from_profile(self.inputs))
            self.tape_uuid = bytes.fromhex(self.inputs["tape_uuid"])
            self.block_size = self.inputs["block_size"]
        else:
            self.tape_uuid = image.tape_uuid
            self.block_size = image.block_size

    def original(self, key: tuple) -> bytes:
        if self.profile is not None:
            component, record = key
            return self.terminal.components[component][record]
        tape_file, block = key
        return self.image.files[tape_file].blocks[block]

    def block(self, key: tuple) -> bytearray:
        if key not in self.blocks:
            self.blocks[key] = bytearray(self.original(key))
        if key not in self.touched:
            self.touched.append(key)
        return self.blocks[key]

    def records(self) -> list[Any]:
        """The whole record stream with the mutated blocks in place (filemarks as None)."""
        out: list[Any] = []
        if self.profile is not None:
            for entry in structural_from_json(self.inputs["structural_entries"]):
                out.extend([bytes(self.block_size)] * entry.block_count)
                out.append(None)
            for component, blocks in enumerate(self.terminal.components):
                for record in range(len(blocks)):
                    key = (component, record)
                    out.append(bytes(self.blocks[key]) if key in self.blocks else blocks[record])
                out.append(None)
            return out
        for tape_file in self.image.files:
            for block in range(len(tape_file.blocks)):
                key = (tape_file.tape_file_number, block)
                out.append(bytes(self.blocks[key]) if key in self.blocks else tape_file.blocks[block])
            if tape_file.has_filemark:
                out.append(None)
        return out

    def describe(self, key: tuple) -> str:
        if self.profile is not None:
            return f"{self.profile}/{PROFILE_COMPONENT_FILES[key[0]]} record {key[1]}"
        return f"{self.image.name} tape file {key[0]} block {key[1]}"


class Resolution:
    def __init__(self) -> None:
        self.checks: list[dict[str, Any]] = []
        self.repairs: list[str] = []
        self.notes: list[str] = []

    @property
    def resolved(self) -> bool:
        return all(check["matches"] for check in self.checks)


def edit_int(ws: Workspace, res: Resolution, key: tuple, offset: int, width: int, frm: int | None, to: int,
             field_name: str) -> None:
    block = ws.block(key)
    found = int.from_bytes(block[offset : offset + width], "little")
    res.checks.append({"where": ws.describe(key), "offset": f"0x{offset:X}", "field": field_name,
                       "expected_from": frm, "found": found, "matches": frm is None or found == frm, "to": to})
    block[offset : offset + width] = to.to_bytes(width, "little")


def edit_bytes(ws: Workspace, res: Resolution, key: tuple, offset: int, frm: bytes | None, to: bytes,
               field_name: str) -> None:
    block = ws.block(key)
    found = bytes(block[offset : offset + len(to)])
    res.checks.append({"where": ws.describe(key), "offset": f"0x{offset:X}", "field": field_name,
                       "expected_from": frm.hex() if frm is not None else None, "found": found.hex(),
                       "matches": frm is None or found == frm, "to": to.hex()})
    block[offset : offset + len(to)] = to


def edit_xor(ws: Workspace, res: Resolution, key: tuple, offset: int, mask: int, field_name: str) -> None:
    block = ws.block(key)
    found = block[offset]
    res.checks.append({"where": ws.describe(key), "offset": f"0x{offset:X}", "field": field_name,
                       "expected_from": None, "found": found, "matches": True, "to": found ^ mask})
    block[offset] ^= mask


# ----- repairs ---------------------------------------------------------------


def repair_sc_hashed(ws: Workspace, res: Resolution, key: tuple, stream_len: int = 96) -> None:
    block = ws.block(key)
    block[0x98:0xB8] = digest(SIDECAR_METADATA_DOMAIN + bytes(block[0x00:0x90]) + bytes(block[0xC8 : 0xC8 + stream_len]))
    block[0xC0:0xC8] = le64(crc64_xz(bytes(block[0x00:0xC0])))
    block[ws.block_size - 8 :] = le64(crc64_xz(bytes(block[: ws.block_size - 8])))
    res.repairs.append(f"R-SC-HASHED on {ws.describe(key)} (hash over 0x00..0x8F and {stream_len} entry bytes)")


def repair_sc_unhashed(ws: Workspace, res: Resolution, key: tuple, name: str = "R-SC-UNHASHED") -> None:
    block = ws.block(key)
    block[0xC0:0xC8] = le64(crc64_xz(bytes(block[0x00:0xC0])))
    block[ws.block_size - 8 :] = le64(crc64_xz(bytes(block[: ws.block_size - 8])))
    res.repairs.append(f"{name} on {ws.describe(key)} (header_crc64 and block0_crc64)")


def repair_sc_fill(ws: Workspace, res: Resolution, key: tuple) -> None:
    block = ws.block(key)
    block[ws.block_size - 8 :] = le64(crc64_xz(bytes(block[: ws.block_size - 8])))
    res.repairs.append(f"R-SC-FILL on {ws.describe(key)} (block CRC)")


def repair_sc_footer(ws: Workspace, res: Resolution, key: tuple) -> None:
    block = ws.block(key)
    block[0x80:0x88] = le64(crc64_xz(bytes(block[0x00:0x80])))
    res.repairs.append(f"R-SC-FOOTER on {ws.describe(key)} (footer_crc64)")


def repair_pm_crc(ws: Workspace, res: Resolution, keys: list[tuple], name: str) -> None:
    for key in keys:
        block = ws.block(key)
        block[0xC0:0xC8] = le64(crc64_xz(bytes(block[0x00:0xC0])))
    res.repairs.append(f"{name}: CRC-64/XZ at 0xC0 of " + ", ".join(ws.describe(k) for k in keys))


def repair_pm_payload(ws: Workspace, res: Resolution, tape_file: int, transform: Any) -> None:
    """R-PM-PAYLOAD: change the payload identically in both copies (M = 1), then fix the locators."""
    primary = ws.block((tape_file, 0))
    length = rd64(primary, 0x30)
    payload = decode_deterministic_cbor(bytes(primary[0xC8 : 0xC8 + length]))
    transform(payload)
    encoded = encode_deterministic_cbor(payload)
    if 0xC8 + len(encoded) > ws.block_size:
        raise BuildRefusal("payload", "the changed payload does not fit M = 1")
    for block_index in (0, 1):
        block = ws.block((tape_file, block_index))
        block[0xC8:] = encoded + bytes(ws.block_size - 0xC8 - len(encoded))
    for block_index in (0, 1, 2):
        block = ws.block((tape_file, block_index))
        block[0x30:0x38] = le64(len(encoded))
        block[0x38:0x58] = digest(encoded)
        block[0xC0:0xC8] = le64(crc64_xz(bytes(block[0x00:0xC0])))
    res.repairs.append(f"R-PM-PAYLOAD on tape file {tape_file}: payload {length} -> {len(encoded)} bytes, "
                       "payload_len and payload_sha256 in both headers and the footer, CRC at 0xC0 of all three")


def replica_recompute(frame: bytearray, tape_uuid: bytes) -> None:
    """Recompute the edition, layout and descriptor digests of one replica frame from its own fields."""
    block_size = rd32(frame, 0x040)
    version_len, stamp_len = rd16(frame, 0x1F0), rd16(frame, 0x1F2)
    edition = digest(
        EDITION_DOMAIN + bytes(frame[0x008:0x00A]) + tape_uuid + bytes(frame[0x020:0x030]) + bytes(frame[0x030:0x038])
        + bytes(frame[0x03C:0x040]) + bytes(frame[0x040:0x044]) + bytes(frame[0x044:0x048]) + bytes(frame[0x048:0x080])
        + bytes(frame[0x0A8:0x0C8]) + bytes(frame[0x0C8:0x0E8])
        + le64(version_len) + bytes(frame[0x1F8 : 0x1F8 + version_len])
        + le64(stamp_len) + bytes(frame[0x278 : 0x278 + stamp_len]))
    tuples = bytes(frame[0x148:0x1E8])
    layout = digest(LAYOUT_DOMAIN + bytes(frame[0x03C:0x040]) + le32(block_size) + le16(5) + tuples + bytes(frame[0x0A0:0x0A8]))
    ordinal = rd16(frame, 0x038)
    index = {1: 0, 2: 2, 3: 4}.get(ordinal, 0)
    descriptor = digest(REPLICA_DESCRIPTOR_DOMAIN + edition + layout + le16(ordinal) + bytes(frame[0x03A:0x03C])
                        + tuples[32 * index : 32 * (index + 1)] + bytes(frame[0x098:0x0A0]))
    frame[0x0E8:0x108] = edition
    frame[0x108:0x128] = layout
    frame[0x128:0x148] = descriptor


def repair_tr_common(ws: Workspace, res: Resolution, component: int, name: str = "R-TR-COMMON") -> None:
    records = len(ws.terminal.components[component])
    header = ws.block((component, 0))
    footer = ws.block((component, records - 1))
    replica_recompute(header, ws.tape_uuid)
    header[0x3F8:0x400] = le64(crc64_xz(bytes(header[:0x3F8])))
    replica_recompute(footer, ws.tape_uuid)
    footer[0x2B8:0x2D8] = digest(bytes(header))
    footer[0x3F8:0x400] = le64(crc64_xz(bytes(footer[:0x3F8])))
    res.repairs.append(f"{name} on {PROFILE_COMPONENT_FILES[component]}: edition, layout and descriptor digests "
                       "recomputed from the frame fields, header CRC, footer header-record SHA-256, footer CRC")


def repair_tr_payload(ws: Workspace, res: Resolution, component: int) -> None:
    header = ws.block((component, 0))
    records = len(ws.terminal.components[component])
    payload_blocks = [bytes(ws.block((component, r))) for r in range(1, records - 1)]
    payload = b"".join(payload_blocks)
    length = rd64(header, 0x070)
    structural = rd64(header, 0x060)
    payload_sha = digest(PAYLOAD_DOMAIN + payload[:length])
    rows = []
    for i in range(structural):
        slot = payload[64 * i : 64 * (i + 1)]
        rows.append(decode_deterministic_cbor(slot[2 : 2 + rd16(slot, 0)]))
    canonical = digest(encode_deterministic_cbor(rows))
    footer = ws.block((component, records - 1))
    for frame in (header, footer):
        frame[0x0A8:0x0C8] = payload_sha
        frame[0x0C8:0x0E8] = canonical
    res.repairs.append(f"R-TR-PAYLOAD on {PROFILE_COMPONENT_FILES[component]}: payload SHA-256 and canonical-map "
                       "SHA-256 recomputed")
    repair_tr_common(ws, res, component, "then R-TR-COMMON")


def repair_tr_footer_local(ws: Workspace, res: Resolution, component: int) -> None:
    records = len(ws.terminal.components[component])
    footer = ws.block((component, records - 1))
    footer[0x3F8:0x400] = le64(crc64_xz(bytes(footer[:0x3F8])))
    res.repairs.append(f"R-TR-FOOTER-LOCAL on {PROFILE_COMPONENT_FILES[component]}: footer CRC")


def separation_recompute(frame: bytearray, tape_uuid: bytes) -> None:
    ordinal = rd16(frame, 0x030)
    tuples = bytes(frame[0x0E0:0x180])
    index = {1: 1, 2: 3}.get(ordinal, 1)
    descriptor = digest(
        SEPARATION_DESCRIPTOR_DOMAIN + tape_uuid + bytes(frame[0x020:0x030]) + bytes(frame[0x030:0x032])
        + bytes(frame[0x032:0x034]) + bytes(frame[0x034:0x038]) + bytes(frame[0x038:0x03C]) + bytes(frame[0x050:0x058])
        + bytes(frame[0x058:0x060]) + tuples[32 * index : 32 * (index + 1)] + tuples[32 * (index - 1) : 32 * index]
        + tuples[32 * (index + 1) : 32 * (index + 2)] + bytes(frame[0x098:0x0B8]))
    frame[0x0B8:0x0D8] = descriptor


def repair_sep(ws: Workspace, res: Resolution, component: int, footer_only: bool = False) -> None:
    records = len(ws.terminal.components[component])
    header = ws.block((component, 0))
    footer = ws.block((component, records - 1))
    if footer_only:
        footer[0x1F8:0x200] = le64(crc64_xz(bytes(footer[:0x1F8])))
        res.repairs.append(f"separation footer CRC on {PROFILE_COMPONENT_FILES[component]}")
        return
    separation_recompute(header, ws.tape_uuid)
    header[0x1F8:0x200] = le64(crc64_xz(bytes(header[:0x1F8])))
    separation_recompute(footer, ws.tape_uuid)
    footer[0x180:0x1A0] = digest(bytes(header))
    footer[0x1F8:0x200] = le64(crc64_xz(bytes(footer[:0x1F8])))
    res.repairs.append(f"R-SEP on {PROFILE_COMPONENT_FILES[component]}: descriptor digest recomputed, header CRC, "
                       "footer header-record SHA-256, footer CRC")


def repair_boot_hdr(ws: Workspace, res: Resolution) -> None:
    block = ws.block((0, 0))
    block[0x30:0x38] = le64(crc64_xz(bytes(block[0x00:0x30])))
    res.repairs.append("R-BOOT-HDR: crc64_header at 0x30")


def mutate_boot_scheme(ws: Workspace, res: Resolution, key_in_scheme: int, frm: int, to: int) -> None:
    block = ws.block((0, 0))
    length = rd32(block, 0x2C)
    payload = decode_deterministic_cbor(bytes(block[0x38 : 0x38 + length]))
    found = payload[1][key_in_scheme]
    res.checks.append({"where": ws.describe((0, 0)), "offset": f"payload key 1 key {key_in_scheme}", "field": "scheme",
                       "expected_from": frm, "found": found, "matches": found == frm, "to": to})
    payload[1][key_in_scheme] = to
    encoded = encode_deterministic_cbor(payload)
    tail = encoded + le64(crc64_xz(encoded))
    block[0x2C:0x30] = le32(len(encoded))
    block[0x30:0x38] = le64(crc64_xz(bytes(block[0x00:0x30])))
    block[0x38:] = tail + bytes(ws.block_size - 0x38 - len(tail))
    res.repairs.append(f"bootstrap payload {length} -> {len(encoded)} bytes: cbor_payload_len, crc64_header, "
                       "crc64_payload at its new position, zero fill")


def set_element(index: Any, frm: Any, to: Any) -> Any:
    """A slot change: check the element's current value, then replace it."""
    def change(value: Any) -> tuple[bool, Any]:
        ok = value[index] == frm
        value[index] = to
        return ok, frm
    return change


def edit_slot(ws: Workspace, res: Resolution, component: int, payload_offset: int, slot_size: int,
              change: Any, label: str) -> None:
    record = 1 + payload_offset // ws.block_size
    offset = payload_offset % ws.block_size
    block = ws.block((component, record))
    slot = bytes(block[offset : offset + slot_size])
    value = decode_deterministic_cbor(slot[2 : 2 + rd16(slot, 0)])
    before = json.loads(json.dumps(value, default=lambda v: v.hex()))
    ok, detail = change(value)
    encoded = encode_deterministic_cbor(value)
    new_slot = encode_slot(encoded, slot_size)
    block[offset : offset + slot_size] = new_slot
    res.checks.append({"where": f"{ws.describe((component, record))} payload offset {payload_offset}", "offset": label,
                       "field": "slot", "expected_from": detail, "found": before, "matches": ok,
                       "to": f"encoded_len {len(encoded)}"})


# ----- the target roles ----------------------------------------------------


def image_context(ws: Workspace) -> tuple[DamagedTape, dict[str, Any]]:
    tape = DamagedTape(ws.records(), set())
    boot = parse_bootstrap(tape.read_data(0), ws.block_size) if ws.image is not None else None
    return tape, {"bootstrap": boot}


def role_sidecar_copy(ws: Workspace, tape_file: int, block: int, copy_kind: int) -> dict[str, Any]:
    try:
        parse_sidecar_copy([bytes(ws.block((tape_file, block)))], ws.tape_uuid, ws.block_size, copy_kind)
        return {"level": f"{'primary' if copy_kind == 1 else 'tail'} header/index copy", "result": "accepted",
                "error": None, "reason": "validates under Sections 9.2-9.5"}
    except ReadFailure as failure:
        return {"level": f"{'primary' if copy_kind == 1 else 'tail'} header/index copy", "result": "rejected",
                "error": failure.error, "reason": failure.reason}


def role_sidecar_footer(ws: Workspace, tape_file: int, measured: int) -> dict[str, Any]:
    footer_key = (tape_file, measured - 1)
    try:
        footer = parse_sidecar_footer(bytes(ws.block(footer_key)), ws.tape_uuid)
    except ReadFailure as failure:
        return {"level": "sidecar footer", "result": "rejected", "error": failure.error, "reason": failure.reason}
    if footer["total"] != measured:
        return {"level": "sidecar footer", "result": "rejected", "error": None,
                "reason": f"parses, but its total {footer['total']} differs from the measured {measured} blocks and "
                          "the map entry"}
    return {"level": "sidecar footer", "result": "accepted", "error": None, "reason": "parses and matches"}


def role_recoverer(ws: Workspace, address: list[int]) -> dict[str, Any]:
    tape = DamagedTape(ws.records(), set())
    boot = parse_bootstrap(tape.read_data(0), ws.block_size)
    entries = ws.image.prefix_entries
    directory, note = load_parity_map_directory(tape, entries, ws.tape_uuid, ws.block_size)
    ctx = RecoveryContext(tape, ws.tape_uuid, ws.block_size, boot["scheme"], entries, len(entries),
                          derived_watermark(entries), directory, "replica")
    outcome = recover_address(ctx, list(address))
    return {"level": f"Recoverer, failed address {address}", "result": outcome["result"], "error": outcome["error"],
            "epoch": outcome["epoch"], "index_acquisition": outcome.get("index_acquisition"), "directory": note}


def role_verifier_copies(ws: Workspace, tape_file: int) -> dict[str, Any]:
    """A Verifier reading both copies of one sidecar and checking that they agree (Section 9.1)."""
    try:
        primary = parse_sidecar_copy([bytes(ws.block((tape_file, 0)))], ws.tape_uuid, ws.block_size, 1)
        tail = parse_sidecar_copy([bytes(ws.block((tape_file, 5)))], ws.tape_uuid, ws.block_size, 2)
    except ReadFailure as failure:
        return {"level": "Verifier, both copies", "result": "rejected", "error": failure.error, "reason": failure.reason}
    if primary["metadata"] != tail["metadata"] or primary["hash"] != tail["hash"]:
        return {"level": "Verifier, both copies", "result": "rejected", "error": "SidecarParse",
                "reason": "both copies parse, and they diverge (index content and canonical_metadata_hash)"}
    return {"level": "Verifier, both copies", "result": "accepted", "error": None, "reason": "copies agree"}


def role_bootstrap(ws: Workspace) -> dict[str, Any]:
    """A parser given one block as the bootstrap (Section 15: every breach of Section 8 is BootstrapParse,
    a missing magic and a failed CRC included, except recorded drive compression on a parity bootstrap)."""
    try:
        parse_bootstrap(bytes(ws.block((0, 0))), ws.block_size)
        return {"level": "bootstrap frame parser", "result": "accepted", "error": None, "reason": ""}
    except ReadFailure as failure:
        error = "DriveCompressionEnabled" if failure.error == "DriveCompressionEnabled" else "BootstrapParse"
        return {"level": "bootstrap frame parser", "result": "rejected", "error": error, "reason": failure.reason}


def role_bootstrap_discovery(ws: Workspace) -> dict[str, Any]:
    """Discovery over the candidate sizes without supplied values, and with the image's own values supplied."""
    tape = DamagedTape(ws.records(), set())
    unsupplied: dict[str, Any] = {"discovery": {"result": None, "block_size": None, "used_hints": False, "error": None,
                                                "citations": []}}
    boot = discover_bootstrap(tape, None, unsupplied, {})
    hints = {"tape_uuid": ws.tape_uuid, "block_size": ws.block_size, "scheme": ws.image.scheme}
    outcome, error, reason, _, _, _ = judge_supplied_bootstrap(tape, hints)
    return {"level": "bootstrap discovery", "result": "found" if boot is not None else "rejected",
            "error": unsupplied["discovery"]["error"],
            "with_supplied_values": {"outcome": outcome, "error": error, "reason": reason}}


def role_parity_map(ws: Workspace, tape_file: int) -> dict[str, Any]:
    tape = DamagedTape(ws.records(), set())
    start = ws.image.file_start_lba(tape_file)
    count = len(ws.image.files[tape_file].blocks)
    try:
        parse_parity_map(tape, start, count, ws.tape_uuid, ws.block_size)
        return {"level": "ParityMap validation", "result": "accepted", "error": None, "reason": ""}
    except ReadFailure as failure:
        return {"level": "ParityMap validation", "result": "rejected", "error": failure.error, "reason": failure.reason}


def role_terminal(ws: Workspace) -> dict[str, Any]:
    tape = DamagedTape(ws.records(), set())
    layout, note = discover_layout(tape, ws.tape_uuid, ws.block_size)
    out: dict[str, Any] = {"level": "terminal discovery, replica validation and selection", "layout_source": note}
    if layout is None:
        out.update(replicas={}, selection="BotStructuralRecoveryRequired", separations={})
        return out
    checks = {LETTERS[o]: check_replica(tape, layout, o, ws.tape_uuid, ws.block_size) for o in (1, 2, 3)}
    out["replicas"] = {l: {"valid": c.fully_valid, "reason": c.reason} for l, c in checks.items()}
    valid = [l for l, c in checks.items() if c.fully_valid]
    if not valid:
        out["selection"] = "BotStructuralRecoveryRequired"
    elif len({checks[l].header["common"] for l in valid}) > 1:
        out["selection"] = "TerminalIndexReplicaConflict"
    else:
        out["selection"] = f"Inventory from {'/'.join(valid)}" + (" (degraded)" if len(valid) < 3 else "")
    out["separations"] = {}
    for ordinal, name in ((1, "A-B"), (2, "B-C")):
        status, reason, _ = check_separation(tape, layout, ordinal, ws.tape_uuid, ws.block_size)
        out["separations"][name] = {"status": status, "reason": reason}
    return out


# ----- the case table --------------------------------------------------------
#
# Each entry gives the mutation (apply), the text decision (decide) and what my
# implementation is expected to report (expect), which the run checks.

P64 = 1 << 64


def a4() -> ImageBuild:
    return _image_cache("a4-minimal")


_IMAGES: dict[str, ImageBuild] = {}


def _image_cache(name: str) -> ImageBuild:
    if name not in _IMAGES:
        _IMAGES[name] = build_image(load_image_inputs(name), name)
    return _IMAGES[name]


def sidecar_field_case(offset: int, width: int, frm: int, to: int, name: str, repair: str, image: str = "a4-minimal",
                       tape_file: int = 2, block: int = 0, stream_len: int = 96):
    def apply(ws: Workspace, res: Resolution) -> None:
        edit_int(ws, res, (tape_file, block), offset, width, frm, to, name)
        if repair == "hashed":
            repair_sc_hashed(ws, res, (tape_file, block), stream_len)
        elif repair == "unhashed":
            repair_sc_unhashed(ws, res, (tape_file, block))
        elif repair == "footer":
            repair_sc_footer(ws, res, (tape_file, block))
    return image, apply


def decision(outcome: str, error: str | None = None, rules: list[str] | None = None, order: str = "",
             readings: list[str] | None = None, error_set: list[str] | None = None, must_reject: Any = True,
             formula: dict | None = None, note: str = "") -> dict[str, Any]:
    return {"outcome": outcome, "error": error, "error_set": error_set, "readings": readings,
            "rules": [cite(k) for k in (rules or [])], "order": order, "reader_must_reject": must_reject,
            "formula": formula, "note": note}


SIDECAR_ORDER = ("Section 9.2 gives its constraints as a table with no order among them; every failing rule is a "
                 "Section 9 rule, so the name is SidecarParse whichever a Reader checks first. The Section 13.3 pin "
                 "(SchemeMismatch) applies only to an acquired index, after validation, so it is not reached for "
                 "this copy.")
BOOT_NAME_READINGS = [
    "BootstrapParse: the frame or payload violates Section 8",
    "NoBootstrapFound: Section 15 also names an invalid bootstrap 'absent or invalid', and Appendix D TT-7 records "
    "that which bootstraps count as unreadable is still open",
]


BOOT_LEVELS_SCHEME = {
    "a parser given the block": "BootstrapParse (Section 15)",
    "discovery over the candidate sizes, without supplied values": "NoBootstrapFound (Section 15)",
    "discovery with supplied values": "BootstrapParse: the decodable scheme disagrees with the supplied scheme (Section 8.4)",
}
BOOT_LEVELS_PAYLOAD = {
    "a parser given the block": "BootstrapParse (Section 15)",
    "discovery over the candidate sizes, without supplied values": "NoBootstrapFound (Section 15)",
    "discovery with supplied values": "no error: the payload runs past the block, so the bootstrap is treated as "
                                      "unreadable and discovery continues on the supplied values (Section 8.4)",
}


def with_levels(entry: dict[str, Any], levels: dict[str, str]) -> dict[str, Any]:
    """A decision whose Section 15 name depends on the level at which the Reader meets the bootstrap."""
    return dict(entry, by_level=levels)


def formula(expression: str, inputs: dict, value: str, overflows: bool, width: str = "u64") -> dict[str, Any]:
    return {"expression": expression, "inputs": inputs, "value": value, "type": width, "overflows": overflows}


def negatives_table() -> dict[str, dict[str, Any]]:
    t: dict[str, dict[str, Any]] = {}

    def sc(case_id, offset, width, frm, to, name, repair, rules, expect_error="SidecarParse", tape_file=2,
           image="a4-minimal", address=(1, 0), block=0, extra=None, order=SIDECAR_ORDER, form=None, note=""):
        img, apply = sidecar_field_case(offset, width, frm, to, name, repair, image, tape_file, block)
        if extra is not None:
            base_apply = apply

            def apply(ws, res, base_apply=base_apply):
                base_apply(ws, res)
                extra(ws, res)
        t[case_id] = {"image": img, "apply": apply,
                      "roles": [("copy", tape_file, block, 1 if block == 0 else 2), ("recoverer", list(address))],
                      "decide": decision("rejected", "SidecarParse", rules + ["err_sidecar"], order,
                                         formula=form, note=note),
                      "expect": {"copy": expect_error}}

    # -- neg-01: replica planned-layout EOD formula overflow
    def apply01(ws, res):
        for key in ((0, 0), (0, 2)):
            for i, (frm, to) in enumerate(zip((2, 6, 10, 14, 18), (P64 - 20, P64 - 16, P64 - 12, P64 - 8, P64 - 4))):
                edit_int(ws, res, key, 0x158 + 32 * i, 8, frm, to, f"planned_start_lba of tuple {i}")
            edit_int(ws, res, key, 0x0A0, 8, 22, 0, "planned terminal EOD LBA")
            edit_int(ws, res, key, 0x090, 8, 2, P64 - 20, "local planned start LBA")
        repair_tr_common(ws, res, 0)
    t["neg-01"] = {"profile": "minimal-256k", "apply": apply01, "roles": [("terminal",)],
                   "decide": decision("rejected", "TerminalIndexReplicaParse",
                                      ["eod_rule", "compare_exact", "layout_valid", "footer_local", "device_agree", "conjunction", "err_replica"],
                                      "Several rules fail at once: the EOD formula's exact value, 2^64, exceeds u64, so the "
                                      "recorded EOD cannot equal it (Section 2.4 compares it exactly); the footer's observed "
                                      "start (2) differs from its planned tuple; and the declared location differs from the "
                                      "device's. Section 10.6 fixes no order among its conditions, and all are "
                                      "TerminalIndexReplicaParse.",
                                      formula=formula("EOD = start(C) + record_count(C) + 1", {"start(C)": "2^64 - 4", "record_count(C)": 3},
                                                      "2^64", True),
                                      note="Replica A is ineligible; B and C are untouched, so the Scanner accepts an agreeing "
                                           "survivor and the result is degraded."),
                   "expect": {"replica A": "TerminalIndexReplicaParse"}}

    # -- neg-02, neg-30, neg-57: the bootstrap's S = 2^63
    for case_id, expr, value, level in (("neg-02", "P = S × m", "2^64", "parser"),
                                        ("neg-30", "S × (k + m) ≤ 2^32 − 1", "2^65", "discovery"),
                                        ("neg-57", "logical_shard_count = S × k", "2^64", "parser")):
        rules = ["boot_scheme_valid", "validity_6_6", "boot_names_level"] + (
            ["boot_discovery_name", "no_bootstrap"] if level == "discovery" else ["boot_parser_name", "err_boot_parse"])
        product = {"neg-02": "P = S × m is a count whose exact value does not fit, so a Reader that forms it rejects it; ",
                   "neg-30": "", "neg-57": "S × k is only compared, in the Recoverer's pin and the Resumer's bound, so it "
                                            "is compared exactly and is never too large; "}[case_id]
        t[case_id] = {"image": "a4-minimal",
                      "apply": lambda ws, res: mutate_boot_scheme(ws, res, 4, 2, 1 << 63),
                      "roles": [("bootstrap",), ("bootstrap-discovery",)],
                      "decide": with_levels(decision(
                          "rejected", "NoBootstrapFound" if level == "discovery" else "BootstrapParse", rules,
                          "Section 8.2 requires the recorded scheme to satisfy Section 6.6, and S × (k + m) = 2^65 exceeds "
                          "2^32 − 1 (compared exactly, Section 2.4), so the bootstrap breaks Section 8. " + product +
                          "The name depends on the level at which the Reader meets the block (Section 15); the case's "
                          f"target is {'discovery' if level == 'discovery' else 'a Reader of the one block'}.",
                          formula=formula(expr, {"S": "2^63", "k": 2, "m": 2}, value, case_id != "neg-57"),
                          note="Without a usable bootstrap and without supplied values, discovery stops (Section 8.4.1)."),
                          BOOT_LEVELS_SCHEME),
                      "expect": {"bootstrap": "BootstrapParse", "bootstrap-discovery": "NoBootstrapFound"}}

    # -- neg-03: T overflow in replica scope
    def apply03(ws, res):
        edit_slot(ws, res, 0, 192, 64, set_element(2, 3, P64 - 2), "structural slot 3 element 2")
        edit_slot(ws, res, 0, 640, 256, set_element(3, 3, P64 - 2), "Object-row slot 1 key 3")
        repair_tr_payload(ws, res, 0)
    t["neg-03"] = {"profile": "multi-256k", "apply": apply03, "roles": [("terminal",)],
                   "decide": decision("rejected", "TerminalIndexReplicaParse",
                                      ["digest_scope", "compare_exact", "payload_valid", "positions_fit", "replica_parse_positions", "err_replica"],
                                      "T = 2 + (2^64 − 2) is 2^64 exactly, so the recorded T (5) cannot equal it (Section 7.4, "
                                      "compared exactly under Section 2.4), and the positions of tape file 3's records and "
                                      "filemark do not fit in u64 (Section 7.2). All are replica rules with one name.",
                                      formula=formula("T = max(first_parity_data_ordinal + block_count)", {"first": 2, "block_count": "2^64 - 2"},
                                                      "2^64", True),
                                      note="Replica A is ineligible; B and C supply the inventory (degraded)."),
                   "expect": {"replica A": "TerminalIndexReplicaParse"}}

    # -- neg-04: no tape vector
    t["neg-04"] = {"none": True,
                   "decide": decision("no-vector", "TapeIo", ["walk_file", "offtape_device", "walked_positions", "err_tapeio"],
                                      "The count comes from positions the device reports. A count whose exact value (−1) does "
                                      "not fit, from a device that reports no motion over a filemark, puts the device at fault: "
                                      "TapeIo (Sections 2.4 and 7.2).",
                                      must_reject=True,
                                      formula=formula("block_count = position delta − 1", {"delta": 0}, "-1 (does not fit)", True),
                                      note="No tape vector: the input is a device report.")}

    # -- neg-05: unit-level peer ordinal
    t["neg-05"] = {"none": True,
                   "decide": decision("accepted", None, ["stripe_inverse", "inverse_exact", "compare_exact", "implicit"],
                                      "The Recoverer compares the exact value of the peer ordinal, 2^64, with the protected end, "
                                      "2^64 − 1: the peer is an implicit zero and is not rejected (Section 3.3).",
                                      must_reject=False,
                                      formula=formula("o = start + data_index·S + stripe", {"start": "2^64 - 2", "data_index": 1, "S": 2, "stripe": 0},
                                                      "2^64, compared exactly", False),
                                      note="Not constructible as a byte image: a validated map needs contiguous epochs from 0.")}

    # -- neg-06: primary_header_start_block
    for variant, keep_hash in (("a-hash-over-wire", False), ("b-hash-with-zero", True)):
        def apply06(ws, res, keep_hash=keep_hash):
            edit_int(ws, res, (2, 0), 0x78, 8, 0, 1, "primary_header_start_block")
            if keep_hash:
                repair_sc_unhashed(ws, res, (2, 0), "hash kept; header_crc64 and block0_crc64")
            else:
                repair_sc_hashed(ws, res, (2, 0))
        t[f"neg-06/{variant}"] = {"image": "a4-minimal", "apply": apply06,
                                  "roles": [("copy", 2, 0, 1), ("recoverer", [1, 0])],
                                  "decide": decision("rejected", "SidecarParse", ["sc_primary_start", "sc_hash_items", "sc_hash_verify", "err_sidecar"],
                                                     SIDECAR_ORDER + " The hash rule's result depends on how 'with "
                                                     "primary_header_start_block as 0' is read (GAPS F-1): under the "
                                                     "wire-bytes reading variant a's hash verifies and b's does not; under "
                                                     "the substitute-0 reading the reverse. The 0x78 rule fails in both "
                                                     "variants, so the outcome does not depend on the reading."),
                                  "expect": {"copy": "SidecarParse"}}

    # -- neg-07: fill bytes
    def fill(block, offset, repair):
        def apply(ws, res):
            edit_int(ws, res, (2, block), offset, 1, 0, 1, "fill byte")
            if repair:
                repair_sc_fill(ws, res, (2, block))
        return apply
    t["neg-07/a-index-block-fill"] = {"image": "a4-minimal", "apply": fill(0, 0x20000, True),
                                      "roles": [("copy", 2, 0, 1), ("recoverer", [1, 0])],
                                      "decide": decision("rejected", "SidecarParse", ["sc_fill", "sc_unused_zero", "hostile_checked", "err_sidecar"]),
                                      "expect": {"copy": "SidecarParse"}}
    t["neg-07/b-tail-copy-fill"] = {"image": "a4-minimal", "apply": fill(5, 0x20000, True),
                                    "roles": [("copy", 2, 5, 2), ("recoverer", [1, 0])],
                                    "decide": decision("rejected", "SidecarParse", ["sc_fill", "sc_unused_zero", "hostile_checked", "err_sidecar"]),
                                    "expect": {"copy": "SidecarParse"}}
    t["neg-07/c-footer-fill"] = {"image": "a4-minimal", "apply": fill(6, 0x1000, False),
                                 "roles": [("footer", 2, 7), ("recoverer", [1, 0])],
                                 "decide": decision("rejected", "SidecarParse", ["ft_fill", "hostile_checked", "err_sidecar"],
                                                    note="The footer is not used; the Recoverer falls back to the primary (Section 13.3 step 2)."),
                                 "expect": {"footer": "SidecarParse"}}

    sc("neg-08", 0x22, 2, 2, 0, "m u16", "hashed", ["sc_m", "sc_P", "sc_recompute"])
    sc("neg-09", 0x48, 8, 4, 1 << 61, "real_data_shard_count", "hashed", ["sc_real_text", "sc_crc_count", "sc_recompute", "checked"],
       extra=lambda ws, res: (edit_int(ws, res, (2, 0), 0x38, 8, 4, 1 << 61, "protected_ordinal_end_exclusive"),
                              repair_sc_hashed(ws, res, (2, 0))),
       order="Section 9.2 fixes no order. The range rule (1..=S×k) and data_crc_count reject without the Section 9.4 "
             "computation. A Reader that computes the layout first compares its exact results with the recorded H and "
             "inline_index_entry_bytes (Section 2.4), and they differ. Every path is SidecarParse.",
       form=formula("16·S·m + 8·real_data_shard_count", {"S": 2, "m": 2, "real": "2^61"}, "2^64 + 64", True))
    sc("neg-10", 0x38, 8, 8, 3, "protected_ordinal_end_exclusive", "hashed", ["sc_end", "sc_real_text", "checked"],
       image="two-epoch", tape_file=3, address=(1, 4),
       form=formula("real_data_shard_count = end − start", {"end": 3, "start": 4}, "-1 (underflow)", True))

    # -- neg-11: tail copy diverges
    def apply11(ws, res):
        edit_xor(ws, res, (2, 5), 0x108, 0x01, "data_shard_crc64 of data-CRC entry 0 (tail copy)")
        repair_sc_hashed(ws, res, (2, 5))
    t["neg-11"] = {"image": "a4-minimal", "apply": apply11,
                   "roles": [("copy", 2, 5, 2), ("verifier-copies", 2), ("recoverer", [1, 0])],
                   "decide": decision("rejected", "SidecarParse", ["verifier_divergence", "sidecar_copy_agree", "footer_decides", "err_sidecar"],
                                      "Each copy validates alone, and their canonical metadata hashes differ. A Verifier reads "
                                      "both copies and the footer and reports the divergence as SidecarParse (Section 9.1).",
                                      note="A Recoverer uses the primary: the valid footer vouches for it, and the directory "
                                           "entry agrees with the footer (Sections 9.1 and 13.3 step 1)."),
                   "expect": {"verifier": "SidecarParse", "copy": "accepted", "recoverer": "recovered"}}

    # -- neg-12: bootstrap payload bounds
    for variant, value in (("a-one-past", 262081), ("b-hostile-max", 4294967295)):
        def apply12(ws, res, value=value):
            recorded = rd32(a4().files[0].blocks[0], 0x2C)
            edit_int(ws, res, (0, 0), 0x2C, 4, recorded, value, "cbor_payload_len u32 LE")
            repair_boot_hdr(ws, res)
        t[f"neg-12/{variant}"] = {"image": "a4-minimal", "apply": apply12, "roles": [("bootstrap",), ("bootstrap-discovery",)],
                                  "decide": with_levels(decision(
                                      "rejected", "NoBootstrapFound",
                                      ["boot_parse_order", "compare_exact", "boot_names_level", "boot_discovery_name",
                                       "boot_parser_name", "boot_too_large_writer", "no_bootstrap"],
                                      "Section 8.1 fixes the parse order: the header CRC (repaired) and schema_major pass, and "
                                      "the payload-bounds step fails before the payload CRC; the bound is compared exactly "
                                      "(Section 2.4). The case's target is the parser's step in Scanner discovery: without "
                                      "supplied values no candidate gives a usable bootstrap, which is NoBootstrapFound "
                                      "(Section 15). BootstrapPayloadTooLarge is a Writer error.",
                                      formula=formula("0x38 + cbor_payload_len + 8 ≤ B", {"cbor_payload_len": value, "B": 262144},
                                                      str(0x38 + value + 8) + ", compared exactly", False)),
                                      BOOT_LEVELS_PAYLOAD),
                                  "expect": {"bootstrap": "BootstrapParse", "bootstrap-discovery": "NoBootstrapFound"}}

    sc("neg-13", 0x40, 8, 4, 5, "logical_shard_count", "hashed", ["sc_logical"])

    # -- neg-14, neg-45, neg-27: ParityMap locator
    def apply14(ws, res):
        for block in (0, 1, 2):
            edit_int(ws, res, (3, block), 0x28, 4, 262144, 2, "block_size u32")
            edit_int(ws, res, (3, block), 0x30, 8, None, P64 - 0xC9, "payload_len")
        repair_pm_crc(ws, res, [(3, 0), (3, 1), (3, 2)], "CRC at 0xC0 of all three blocks")
    t["neg-14"] = {"image": "a4-minimal", "apply": apply14, "roles": [("parity-map", 3), ("recoverer", [1, 0])],
                   "decide": decision("rejected", "ParityMapParse", ["pm_block_size", "pm_locator", "pm_M", "compare_exact", "err_pm"],
                                      "The header and footer block_size (2) is not the tape's block size, which Section 10.1.3 "
                                      "requires. With the tape's block size, M = ceil((0xC8 + L) / B) = 2^46 exactly, which "
                                      "differs from the recorded copy_block_count 1, and the payload cannot fit the measured "
                                      "3-block file. Every rule that fails is ParityMapParse.",
                                      formula=formula("M = ceil((0xC8 + L) / B)", {"L": "2^64 - 0xC9", "B": 262144},
                                                      "M = 2^46, compared exactly", False),
                                      note="The directory is unavailable; the Recoverer still acquires the index by the footer and primary."),
                   "expect": {"parity-map": "ParityMapParse"}}

    def apply45(ws, res):
        for block in (0, 1, 2):
            edit_int(ws, res, (3, block), 0x30, 8, None, P64 - 1, "payload_len")
        repair_pm_crc(ws, res, [(3, 0), (3, 1), (3, 2)], "CRC at 0xC0 of all three blocks")
    for case_id in ("neg-45", "neg-27"):
        t[case_id] = {"image": "a4-minimal", "apply": apply45, "roles": [("parity-map", 3), ("recoverer", [1, 0])],
                      "decide": decision("rejected", "ParityMapParse", ["pm_locator", "pm_M", "exact_values", "intermediate_ok", "err_pm"],
                                         "M's exact value, ceil((0xC8 + 2^64 − 1) / 2^18) = 2^46 + 1, fits in u64; the "
                                         "intermediate 0xC8 + L is not a violation (Section 2.4). M differs from the recorded "
                                         "copy_block_count 1, and the payload cannot fit the measured file, so the locator "
                                         "arithmetic rejects the ParityMap.",
                                         formula=formula("M = ceil((0xC8 + L) / B)" if case_id == "neg-45" else "tail copy at block M",
                                                         {"L": "2^64 - 1", "B": 262144}, "M = 2^46 + 1", False),
                                         note="" if case_id == "neg-45" else
                                         "The case gives its mutation 'as overflow-10.1.2-M'; resolved to neg-45's mutation, "
                                         "whose formula is M's. The tail index adds no arithmetic of its own."),
                      "expect": {"parity-map": "ParityMapParse"}}

    # -- neg-15: Object-row manifest range
    for variant, key10, key12 in (("a-range-sum", P64 - 1, 1), ("b-byte-product", 0, 1 << 46)):
        def apply15(ws, res, key10=key10, key12=key12):
            def change(v):
                ok = v.get(10) == 0 and v.get(12) == 1
                v[10], v[12] = key10, key12
                return ok, "key 10 = 0, key 12 = 1"
            edit_slot(ws, res, 0, 384, 256, change, "Object-row slot 0 keys 10 and 12")
            repair_tr_payload(ws, res, 0)
        t[f"neg-15/{variant}"] = {"profile": "multi-256k", "apply": apply15, "roles": [("terminal",)],
                                  "decide": decision("rejected", "TerminalIndexReplicaParse", ["row_manifest", "compare_exact", "payload_valid", "err_replica"],
                                                     "The chunk range fails in both variants: first + count, compared exactly "
                                                     "(Section 2.4), exceeds stored_block_count 2. The byte-length rule compares "
                                                     "size with count × B exactly, and holds. The name is TerminalIndexReplicaParse.",
                                                     formula=formula("first + count ≤ stored; size ≤ count × B",
                                                                     {"first": str(key10), "count": str(key12), "B": 262144},
                                                                     "compared exactly", False),
                                                     note="Replica A is ineligible; B and C supply the inventory (degraded)."),
                                  "expect": {"replica A": "TerminalIndexReplicaParse"}}

    sc("neg-16", 0x94, 4, 0, 1, "copy_generation", "unhashed", ["sc_generation"])

    # -- neg-17: sidecar footer P
    def apply17(ws, res):
        edit_int(ws, res, (2, 6), 0x40, 8, 4, P64 - 1, "footer P")
        repair_sc_footer(ws, res, (2, 6))
    t["neg-17"] = {"image": "a4-minimal", "apply": apply17, "roles": [("footer", 2, 7), ("recoverer", [1, 0])],
                   "decide": decision("rejected", "SidecarParse", ["ft_tail", "compare_exact", "err_sidecar"],
                                      "H + P is 2^64 exactly, so tail_header_start_block (5) cannot equal it (Section 2.4 "
                                      "compares it exactly).",
                                      formula=formula("tail_header_start_block = H + P", {"H": 1, "P": "2^64 - 1"}, "2^64", True),
                                      note="The footer is not used: the Recoverer falls back to the primary (Section 13.3 "
                                           "step 2), and the Scanner's footer probe fails (Section 12.3 item 6)."),
                   "expect": {"footer": "SidecarParse"}}

    for variant, to in (("a-equal", 4), ("b-below", 3)):
        sc(f"neg-18/{variant}", 0x38, 8, 8, to, "protected_ordinal_end_exclusive", "hashed", ["sc_end", "sc_real_text", "checked"],
           image="two-epoch", tape_file=3, address=(1, 4),
           form=formula("real_data_shard_count = end − start", {"end": to, "start": 4}, str(to - 4) + (" (underflow)" if to < 4 else ""), to < 4))
    sc("neg-19", 0x24, 4, 2, 0, "S u32", "hashed", ["sc_S", "sc_logical", "sc_P", "stripe_map"],
       order=SIDECAR_ORDER + " The Section 3.3 division by S is never reached, because the copy is rejected at validation.",
       form=formula("stripe = d mod S; data_index = d / S", {"S": 0}, "undefined (division by zero)", False))
    sc("neg-20", 0x20, 2, 2, 0, "k u16", "hashed", ["sc_k", "sc_logical"])

    # -- neg-21: payload_len overflow
    for variant, s_rows, o_rows in (("a-64s", 1 << 58, 0), ("b-256o", 1, 1 << 56), ("c-sum", 1 << 57, 1 << 55)):
        def apply21(ws, res, s_rows=s_rows, o_rows=o_rows):
            for key in ((0, 0), (0, 2)):
                edit_int(ws, res, key, 0x060, 8, 1, s_rows, "structural_row_count")
                edit_int(ws, res, key, 0x068, 8, 0, o_rows, "object_row_count")
            repair_tr_common(ws, res, 0)
        t[f"neg-21/{variant}"] = {"profile": "minimal-256k", "apply": apply21, "roles": [("terminal",)],
                                  "decide": decision("rejected", "TerminalIndexReplicaParse", ["size_formulas", "checked", "err_replica"],
                                                     "Section 10.6 names this rule: every Section 10.4 size formula evaluates "
                                                     "without overflow. Other rules also fail (the recorded payload length 64 is "
                                                     "not the formula's result), with the same name.",
                                                     formula=formula("payload_len = 64 × s + 256 × o", {"s": str(s_rows), "o": str(o_rows)}, "2^64", True),
                                                     note="Replica A is ineligible; B and C supply the inventory (degraded)."),
                                  "expect": {"replica A": "TerminalIndexReplicaParse"}}

    sc("neg-22", 0x58, 8, 4, 3, "data_crc_count", "hashed", ["sc_crc_count"])
    sc("neg-23", 0x28, 4, 262144, 524288, "block_size u32", "hashed", ["sc_block_size", "pin_then", "geometry_c"],
       order="The Section 9.2 block-size rule is a copy-validity rule, and Section 13.3 pins only an acquired index "
             "('then pinned'), so SidecarParse fires first for this copy. Appendix C (informative) says geometry "
             "disagreements raise SchemeMismatch; that applies when a copy that validates disagrees with the bootstrap "
             "(GAPS F-2).")
    for variant, to in (("a", 3), ("b", 0)):
        sc(f"neg-24/{variant}", 0x90, 2, 1, to, "copy_kind u16", "unhashed", ["sc_copy_kind", "primary_fallback"])

    # -- neg-25: separation E
    for variant, value in (("a-E-zero", 0), ("b-E-one-block", 262144)):
        def apply25(ws, res, value=value):
            for key in ((1, 0), (1, 2)):
                edit_int(ws, res, key, 0x050, 8, 786432, value, "nominal total extent bytes")
            repair_sep(ws, res, 1)
        t[f"neg-25/{variant}"] = {"profile": "minimal-256k", "apply": apply25, "roles": [("terminal",)],
                                  "decide": decision("rejected", "TerminalIndexSeparationParse", ["sep_records", "sep_valid", "checked", "err_separation"],
                                                     "total_records ≥ 2 fails, the recorded total (3) is not ceil(E/B), and the "
                                                     "footer offset and interior count underflow; all are separation rules with one "
                                                     "name, and Section 10.6 fixes no order.",
                                                     formula=formula("footer_offset = total_records − 1; interior = total_records − 2",
                                                                     {"E": value, "B": 262144, "total_records": value // 262144},
                                                                     "underflow", True),
                                                     note="The Verifier must not report the suffix complete (Section 10.6); the "
                                                          "Scanner's inventory is unaffected."),
                                  "expect": {"separation A-B": "invalid"}}

    t["neg-26"] = {"none": True,
                   "decide": decision("no-vector", None, ["close_reserve"], "", must_reject=False,
                                      formula=formula("close reserve", {}, "evaluated only by a Writer", False),
                                      note="No Reader evaluates this formula. The Writer's capacity admission is practice in "
                                           "the Guide; Appendix C records that this document no longer requires a tool to "
                                           "refuse an Object that would leave too little room to finalize. Nothing for a "
                                           "Reader to reject.")}

    sc("neg-28", 0x80, 8, 5, 6, "tail_header_start_block", "hashed", ["sc_tail_start"])
    sc("neg-29/a-header-0xB8", 0xB8, 8, 0, 1, "reserved u64", "unhashed", ["sc_reservedB8", "hostile_checked"])
    sc("neg-29/b-parity-entry-reserved", 0xCE, 2, 0, 1, "reserved u16 of parity entry 0", "hashed", ["sc_entry_reserved", "hostile_checked"])

    def apply29c(ws, res):
        edit_int(ws, res, (2, 6), 0x0A, 2, 0, 1, "footer reserved")
        repair_sc_footer(ws, res, (2, 6))
    t["neg-29/c-footer-reserved"] = {"image": "a4-minimal", "apply": apply29c, "roles": [("footer", 2, 7), ("recoverer", [1, 0])],
                                     "decide": decision("rejected", "SidecarParse", ["ft_reserved", "hostile_checked", "err_sidecar"],
                                                        note="The footer is not used; the Recoverer falls back to the primary."),
                                     "expect": {"footer": "SidecarParse"}}
    sc("neg-31", 0x92, 2, 0, 1, "reserved u16", "unhashed", ["sc_reserved92"])
    sc("neg-32", 0x68, 8, 96, 88, "inline_index_entry_bytes", "hashed", ["sc_inline", "sc_recompute"])

    # -- neg-33: directory-assisted rescue with hostile counts
    def corrupt_sidecar(ws, res):
        edit_xor(ws, res, (2, 6), 0x80, 0x01, "footer_crc64 byte (precondition)")
        edit_xor(ws, res, (2, 0), 0xC0, 0x01, "header_crc64 byte (precondition)")
    for variant, key, value in (("a-H-seven", 6, 7), ("b-H-max", 6, P64 - 1), ("c-total-zero", 5, 0)):
        def apply33(ws, res, key=key, value=value):
            def transform(payload, key=key, value=value):
                entry = payload[4][5][0]
                expected = {6: 1, 5: 7}[key]
                res.checks.append({"where": "a4-minimal tape file 3 payload", "offset": f"directory entry 0 key {key}",
                                   "field": "directory entry", "expected_from": expected, "found": entry[key],
                                   "matches": entry[key] == expected, "to": value})
                entry[key] = value
            repair_pm_payload(ws, res, 3, transform)
            corrupt_sidecar(ws, res)
        underflow = {"a-H-seven": "7 − 1 − 7", "b-H-max": "7 − 1 − (2^64 − 1)", "c-total-zero": "0 − 1 − 1"}[variant]
        t[f"neg-33/{variant}"] = {
            "image": "a4-minimal", "apply": apply33, "roles": [("parity-map", 3), ("recoverer", [1, 0])],
            "decide": decision("rejected", "SidecarMetadataUnavailable",
                               ["tail_rescue", "entry_agrees", "metadata_unavailable", "unavailable_report", "err_metadata_unavailable"]
                               + (["dir_nonzero", "dir_rule", "pm_category"] if variant.startswith("c") else ["rescue_preconditions"]),
                               ("The directory entry agrees with the map entry in tape file, epoch, range and block count, "
                                "but its total (7) is not 2H + P + 1 for its H (Section 13.3), so the entry is not available "
                                "and no rescue read is placed; with the primary header and footer failed, no header/index "
                                "copy validates, and epoch 0 is metadata-unavailable."
                                if not variant.startswith("c") else
                                "The zero total breaks the directory invariant 'non-zero sidecar_total_block_count', so the "
                                "ParityMap does not validate (DirectoryInvalid or ParityMapParse, the category Section 15 "
                                "leaves open once both copies fail); no directory entry is available, and epoch 0 is "
                                "metadata-unavailable. The Section 13.3 agreement rule would also forbid the read."),
                               formula=(formula("total = 2H + P + 1 (the rescue's precondition)", {"H": {"a-H-seven": "7", "b-H-max": "2^64 - 1"}[variant], "P": 4, "total": 7},
                                                {"a-H-seven": "19", "b-H-max": "2^65 + 3"}[variant] + ", not 7", False)
                                        if not variant.startswith("c") else
                                        formula("tail block = total − 1 − H", {"expression": underflow}, "underflow", True)),
                               note="SidecarMetadataUnavailable{epoch_id: 0} for the failed address (1, 0)."),
            "expect": {"recoverer": "SidecarMetadataUnavailable"}}

    sc("neg-34", 0x24, 4, 2, 0, "S u32", "hashed", ["sc_S", "sc_logical", "sc_P"])

    # -- neg-35, neg-56: the Resumer
    def resume_prefix(change):
        image = _image_cache("unfinalized-open")
        prefix = [{"block_count": e.block_count, "epoch_id": e.epoch_id,
                   "first_parity_data_ordinal": e.first_parity_data_ordinal, "kind": KIND_NAMES[e.kind],
                   "protected_ordinal_end_exclusive": e.protected_ordinal_end_exclusive,
                   "protected_ordinal_start": e.protected_ordinal_start, "tape_file_number": e.tape_file_number}
                  for e in image.prefix_entries]
        change(prefix)
        return prefix
    t["neg-35"] = {"resume": lambda: {"image": "unfinalized-open", "W": 8, "T": 6,
                                      "committed_prefix": resume_prefix(lambda p: p[2].update(protected_ordinal_end_exclusive=8)),
                                      "append_object": load_image_inputs("unfinalized-open")["objects"][1]},
                   "decide": decision("rejected", "ResumeAppend", ["resume_step2", "resume_step2_rules", "w_le_t", "compare_exact", "err_resume"],
                                      "W ≤ T fails (8 > 6), a step-2 violation, which is ResumeAppend. The bound compares "
                                      "T − W = −2 exactly (Section 2.4) and holds, so it is not the rule that refuses.",
                                      formula=formula("T − W < S × k", {"T": 6, "W": 8, "S": 2, "k": 2}, "-2 < 4, compared exactly", False),
                                      note="Decidable from the prefix alone, before any tape read (GAPS E-2)."),
                   "expect": {"resume": "ResumeAppend"}}
    t["neg-56"] = {"resume": lambda: {"image": "unfinalized-open", "W": 4, "T": 6,
                                      "committed_prefix": resume_prefix(lambda p: p[3].update(block_count=P64 - 1)),
                                      "append_object": load_image_inputs("unfinalized-open")["objects"][1]},
                   "decide": decision("rejected", "ResumeAppend", ["resume_step1", "append_point", "checked", "resume_validated_records", "err_resume"],
                                      "T and the append point overflow in step 1. Checked arithmetic rejects them, so the "
                                      "records do not determine one committed prefix and append point (Section 3.4), which "
                                      "is ResumeAppend.",
                                      formula=formula("append point = Σ(block_count + 1)", {"block_counts": [1, 4, 7, "2^64 - 1"]}, "2^64 + 14", True),
                                      note="Before any tape read."),
                   "expect": {"resume": "ResumeAppend"}}

    # -- neg-36: ParityMap footer disagrees
    def apply36(offset, delta_to):
        def apply(ws, res):
            block = ws.block((3, 2))
            current = rd64(block, offset)
            edit_int(ws, res, (3, 2), offset, 8, None if offset == 0x30 else 0, delta_to(current), "footer field")
            repair_pm_crc(ws, res, [(3, 2)], "R-PM-FOOTER")
        return apply
    t["neg-36/a-footer-sequence"] = {"image": "a4-minimal", "apply": apply36(0x20, lambda c: 1),
                                     "roles": [("parity-map", 3)],
                                     "decide": decision("rejected", "ParityMapParse", ["pm_match", "pm_locator", "err_pm"]),
                                     "expect": {"parity-map": "ParityMapParse"}}
    t["neg-36/b-footer-payload-len"] = {"image": "a4-minimal", "apply": apply36(0x30, lambda c: c + 1),
                                        "roles": [("parity-map", 3)],
                                        "decide": decision("rejected", "ParityMapParse", ["pm_locator", "err_pm"]),
                                        "expect": {"parity-map": "ParityMapParse"}}

    # -- neg-37: directory entries out of order
    def apply37(ws, res):
        def transform(payload):
            entries = payload[4][5]
            res.checks.append({"where": "two-epoch tape file 4 payload", "offset": "directory key 5", "field": "entries",
                               "expected_from": [2, 3], "found": [e[1] for e in entries],
                               "matches": [e[1] for e in entries] == [2, 3], "to": [3, 2]})
            entries.reverse()
        repair_pm_payload(ws, res, 4, transform)
    t["neg-37"] = {"image": "two-epoch", "apply": apply37, "roles": [("parity-map", 4)],
                   "decide": decision("rejected", None, ["dir_rule", "dir_ascending", "pm_category"],
                                      "The directory is well formed and breaks the ascending-order invariant (and the "
                                      "partition invariant). Both copies carry it, so both fail; Section 15 leaves the name "
                                      "open as a category.",
                                      error_set=["DirectoryInvalid", "ParityMapParse"]),
                   "expect": {"parity-map": "DirectoryInvalid"}}

    # -- neg-38: separation header magic
    def apply38(ws, res):
        edit_xor(ws, res, (1, 0), 0x000, 0x01, "separation header role magic byte 0")
        repair_sep(ws, res, 1)
    t["neg-38"] = {"profile": "minimal-256k", "apply": apply38, "roles": [("terminal",)],
                   "decide": decision("rejected", "TerminalIndexSeparationParse", ["role_magic", "sep_valid", "sep_magic_candidate", "err_separation"],
                                      note="A Verifier finds AB invalid and must not report the suffix complete; the "
                                           "Scanner's inventory is unaffected."),
                   "expect": {"separation A-B": "invalid"}}

    def uuid_edit(ws, res):
        edit_bytes(ws, res, (2, 0), 0x08, bytes.fromhex("12345678123442348234123456789abc"), b"\xee" * 16, "tape_uuid")
        repair_sc_hashed(ws, res, (2, 0))
    t["neg-39"] = {"image": "a4-minimal", "apply": uuid_edit, "roles": [("copy", 2, 0, 1), ("recoverer", [1, 0])],
                   "decide": decision("rejected", "SidecarParse", ["sc_uuid", "err_sidecar"]),
                   "expect": {"copy": "SidecarParse"}}

    def apply40a(ws, res):
        edit_int(ws, res, (2, 6), 0x38, 8, 1, 1 << 63, "footer H")
        repair_sc_footer(ws, res, (2, 6))
    t["neg-40/a-footer"] = {"image": "a4-minimal", "apply": apply40a, "roles": [("footer", 2, 7), ("recoverer", [1, 0])],
                            "decide": decision("rejected", "SidecarParse", ["ft_tail", "err_sidecar"],
                                               "Section 9.6 gives the footer's total no formula, so 2H + P + 1 is not a footer "
                                               "rule. The rule that fails is tail_header_start_block = H + P: 5 ≠ 2^63 + 4, "
                                               "which does not overflow.",
                                               formula=formula("tail_header_start_block = H + P", {"H": "2^63", "P": 4}, "2^63 + 4", False),
                                               note="The footer is not used; the Recoverer falls back to the primary."),
                            "expect": {"footer": "SidecarParse"}}
    sc("neg-40/b-header", 0x60, 8, 1, 1 << 63, "H", "hashed", ["sc_H", "sc_total", "sc_recompute", "checked"],
       form=formula("total = 2H + P + 1", {"H": "2^63", "P": 4}, "2^64 + 5", True))
    sc("neg-41", 0x88, 8, 6, 7, "footer_block_index", "hashed", ["sc_footer_index"])

    def apply42(ws, res):
        for key in ((0, 0), (0, 2)):
            edit_int(ws, res, key, 0x158, 8, 2, P64 - 2, "planned_start_lba of (4,1)")
            edit_int(ws, res, key, 0x090, 8, 2, P64 - 2, "local planned start LBA")
        repair_tr_common(ws, res, 0)
    t["neg-42"] = {"profile": "minimal-256k", "apply": apply42, "roles": [("terminal",)],
                   "decide": decision("rejected", "TerminalIndexReplicaParse", ["eod_rule", "compare_exact", "layout_valid", "footer_local", "conjunction", "err_replica"],
                                      "The next start's exact value, 2^64 + 2, is compared with (5,1)'s recorded start of 6 "
                                      "and differs (Section 2.4). The footer's observed start (2) also differs from its tuple, "
                                      "and the declared location from the device's. No order is fixed; one name.",
                                      formula=formula("start(next) = start + record_count + 1", {"start": "2^64 - 2", "record_count": 3}, "2^64 + 2", True),
                                      note="Replica A is ineligible; B and C supply the inventory (degraded)."),
                   "expect": {"replica A": "TerminalIndexReplicaParse"}}
    sc("neg-43", 0x60, 8, 1, 2, "H", "hashed", ["sc_H", "sc_recompute"])

    def apply44(ws, res):
        for component in (0, 2, 4):
            edit_slot(ws, res, component, 256, 64, set_element(2, 5, P64 - 1), "structural slot 4 element 2")
            repair_tr_payload(ws, res, component)
    t["neg-44"] = {"profile": "multi-256k", "apply": apply44, "roles": [("terminal",)],
                   "decide": decision("rejected", "TerminalIndexReplicaParse", ["positions_fit", "replica_parse_positions", "lba_formula", "no_replica_walk", "err_bot"],
                                      "The map describes tape file 4's trailing filemark at position 2 + 3 + 6 + 4 + (2^64 − 1) = "
                                      "2^64 + 14, which does not fit in u64, so the recorded map is invalid (Section 7.2), and "
                                      "each replica is TerminalIndexReplicaParse (Section 15).",
                                      formula=formula("LBA(4, block_count(4)) = Σ_{g<4}(block_count(g) + 1) + block_count(4)",
                                                      {"block_counts": [1, 2, 5, 3, "2^64 - 1"]}, "2^64 + 14", True),
                                      note="All three replicas carry the same map, so no replica validates: "
                                           "BotStructuralRecoveryRequired."),
                   "expect": {"selection": "BotStructuralRecoveryRequired"}}

    def apply46(ws, res):
        for key in ((0, 0), (0, 2)):
            edit_int(ws, res, key, 0x060, 8, 1, (1 << 58) - 1, "structural_row_count")
            edit_int(ws, res, key, 0x068, 8, 0, 0, "object_row_count")
        repair_tr_common(ws, res, 0)
    t["neg-46"] = {"profile": "minimal-256k", "apply": apply46, "roles": [("terminal",)],
                   "decide": decision("rejected", "TerminalIndexReplicaParse", ["size_formulas", "size_exact", "intermediate_ok", "err_replica"],
                                      "payload_len = 2^64 − 64 fits, and so does payload_padding_bytes, whose exact value is 64: "
                                      "the intermediate product count × B = 2^64 is not a violation (Sections 2.4 and 10.6). "
                                      "The recorded payload length (64) differs from the formula's 2^64 − 64, so the recorded "
                                      "geometry fails, which is TerminalIndexReplicaParse.",
                                      formula=formula("payload_padding_bytes = payload_record_count × B − payload_len",
                                                      {"payload_record_count": "2^46", "B": "2^18"}, "64", False),
                                      note="footer_block_offset and replica_record_count cannot overflow here. Replica A is "
                                           "ineligible; B and C supply the inventory."),
                   "expect": {"replica A": "TerminalIndexReplicaParse"}}

    for variant, profile, offset, frm, to, rules in (
            ("a-covered-prefix", "multi-256k", 0x048, 6, 7, ["covered_equal", "digest_scope"]),
            ("b-total-data-ordinals", "multi-256k", 0x050, 5, 6, ["digest_scope", "w_equals_t"]),
            ("c-highest-protected", "multi-256k", 0x058, 5, 4, ["digest_scope", "w_equals_t"]),
            ("d-total-isolated", "minimal-256k", 0x050, 0, 1, ["digest_scope"])):
        def apply47(ws, res, offset=offset, frm=frm, to=to):
            for key in ((0, 0), (0, 2)):
                edit_int(ws, res, key, offset, 8, frm, to, "scope field")
            repair_tr_common(ws, res, 0)
        t[f"neg-47/{variant}"] = {"profile": profile, "apply": apply47, "roles": [("terminal",)],
                                  "decide": decision("rejected", "TerminalIndexReplicaParse", rules + ["err_replica"],
                                                     "The failing rules are replica scope rules (Sections 7.4 and 10.6), "
                                                     "with one name. The canonical-map digest still matches its structural "
                                                     "rows, so FilemarkMapDigestMismatch does not apply.",
                                                     note="Replica A is ineligible; B and C supply the inventory (degraded)."),
                                  "expect": {"replica A": "TerminalIndexReplicaParse"}}

    def apply48(ws, res):
        edit_xor(ws, res, (2, 0), 0x98, 0x01, "canonical_metadata_hash byte 0")
        repair_sc_unhashed(ws, res, (2, 0), "header_crc64 and block0_crc64 only; hash not recomputed")
    t["neg-48"] = {"image": "a4-minimal", "apply": apply48, "roles": [("copy", 2, 0, 1), ("recoverer", [1, 0])],
                   "decide": decision("rejected", "SidecarParse", ["sc_hash_verify", "err_sidecar"]),
                   "expect": {"copy": "SidecarParse"}}
    sc("neg-49", 0x60, 8, 1, P64 - 1, "H", "hashed", ["sc_H", "sc_total", "sc_recompute", "parity_block_formula", "checked"],
       order=SIDECAR_ORDER + " With this H the copy is rejected at validation, so the Section 9.1 parity locator is never "
             "evaluated with it.",
       form=formula("block(0, 1) = H + 1·S + 0", {"H": "2^64 - 1", "S": 2}, "2^64 + 1", True))

    def apply50(ws, res):
        for key in ((1, 0), (1, 2)):
            edit_int(ws, res, key, 0x050, 8, 786432, P64 - 1, "nominal total extent bytes")
        repair_sep(ws, res, 1)
    t["neg-50"] = {"profile": "minimal-256k", "apply": apply50, "roles": [("terminal",)],
                   "decide": decision("rejected", "TerminalIndexSeparationParse", ["sep_records", "size_formulas", "sep_valid", "checked", "err_separation"],
                                      "actual_bytes = 2^46 × 2^18 overflows, and the recorded total (3) is not ceil(E/B) = 2^46; "
                                      "both are separation rules with one name.",
                                      formula=formula("actual_bytes = ceil(E / B) × B", {"E": "2^64 - 1", "B": "2^18"}, "2^64", True),
                                      note="A Verifier must not report the suffix complete; the Scanner is unaffected."),
                   "expect": {"separation A-B": "invalid"}}

    for variant in ("a-header-magic-flipped", "b-header-carries-footer-magic"):
        def apply51(ws, res, variant=variant):
            if variant.startswith("a"):
                edit_xor(ws, res, (0, 0), 0x000, 0x01, "header role magic byte 0")
            else:
                edit_bytes(ws, res, (0, 0), 0x000, role_magic(ws.tape_uuid, LABEL_REPLICA_HEADER),
                           role_magic(ws.tape_uuid, LABEL_REPLICA_FOOTER), "header role magic")
            repair_tr_common(ws, res, 0, "header CRC, footer 0x2B8, footer CRC (digests unchanged)")
        t[f"neg-51/{variant}"] = {"profile": "minimal-256k", "apply": apply51, "roles": [("terminal",)],
                                  "decide": decision("rejected", "TerminalIndexReplicaParse", ["role_magic", "sep_magic_candidate", "err_replica"],
                                                     note="Replica A is ineligible; C's footer supplies the layout, and B and C "
                                                          "supply the inventory (degraded)."),
                                  "expect": {"replica A": "TerminalIndexReplicaParse"}}
    sc("neg-52", 0x50, 8, 4, 3, "parity_block_count", "hashed", ["sc_P"])

    for variant, components in (("a-replica-A-only", (0,)), ("b-all-replicas", (0, 2, 4))):
        def apply53(ws, res, components=components):
            for component in components:
                def change(v):
                    ok = v.get(4) == b"minimal-plaintext-object"
                    v[4] = b"a" * 65
                    return ok, "minimal-plaintext-object"
                edit_slot(ws, res, component, 384, 256, change, "Object-row slot 0 key 4")
                repair_tr_payload(ws, res, component)
        t[f"neg-53/{variant}"] = {"profile": "multi-256k", "apply": apply53, "roles": [("terminal",)],
                                  "decide": decision("rejected", "TerminalIndexReplicaParse" if variant.startswith("a") else "BotStructuralRecoveryRequired",
                                                     ["row_id", "payload_valid", "err_replica"] + ([] if variant.startswith("a") else ["no_replica_walk", "err_bot"]),
                                                     note=("Replica A is ineligible; B and C supply the inventory (degraded)."
                                                           if variant.startswith("a") else
                                                           "Every replica is ineligible (TerminalIndexReplicaParse each), so the "
                                                           "tape-level outcome is BotStructuralRecoveryRequired.")),
                                  "expect": {"replica A": "TerminalIndexReplicaParse"} if variant.startswith("a")
                                  else {"selection": "BotStructuralRecoveryRequired"}}

    sc("neg-54/a-header-wrong", 0x70, 8, 7, 8, "sidecar_total_block_count", "hashed", ["sc_total", "item5"])
    sc("neg-54/b-header-zero-hostile", 0x70, 8, 7, 0, "sidecar_total_block_count", "hashed", ["sc_total", "checked"])

    def apply54c(ws, res):
        edit_int(ws, res, (2, 6), 0x48, 8, 7, 0, "footer sidecar_total_block_count")
        repair_sc_footer(ws, res, (2, 6))
    t["neg-54/c-footer-zero-hostile"] = {
        "image": "a4-minimal", "apply": apply54c, "roles": [("footer", 2, 7), ("recoverer", [1, 0])],
        "decide": decision("rejected", "undecided", ["footer_probe", "primary_fallback", "hard_error_scope"],
                           "Section 9.6 gives the footer's total no formula, so the footer is rejected only by comparison: "
                           "its total (0) differs from the measured length (Section 12.3 item 6) and from the map entry "
                           "(Section 13.3 step 1).",
                           readings=["SidecarParse: the footer is a sidecar structure that fails its checks",
                                     "no Section 15 error: Section 13.3 treats it as an invalid footer and falls back to the "
                                     "primary, and Section 12.3 reports a failed classification that Section 15 does not name"],
                           note="The footer is not used; the Recoverer falls back to the primary."),
        "expect": {"footer": "rejected"}}

    for variant, component, offset, frm, to, footer_only in (
            ("a-count-zero", 0, 0x2E8, 3, 0, False), ("b-start-max", 0, 0x2E0, 2, P64 - 1, False),
            ("c-separation-count-zero", 1, 0x1B0, 3, 0, True)):
        def apply55(ws, res, component=component, offset=offset, frm=frm, to=to, footer_only=footer_only):
            edit_int(ws, res, (component, 2), offset, 8, frm, to, "footer observation")
            if footer_only:
                repair_sep(ws, res, component, footer_only=True)
            else:
                repair_tr_footer_local(ws, res, component)
        sep = variant.startswith("c")
        t[f"neg-55/{variant}"] = {"profile": "minimal-256k", "apply": apply55, "roles": [("terminal",)],
                                  "decide": decision("rejected", "TerminalIndexSeparationParse" if sep else "TerminalIndexReplicaParse",
                                                     ["footer_delta", "checked", "footer_local"] + (["sep_arith", "err_separation"] if sep else ["err_replica"]),
                                                     "The footer arithmetic underflows or overflows, and the observation also "
                                                     "differs from the planned tuple; one name.",
                                                     formula=formula("delta = count − 1; footer LBA = start + delta",
                                                                     {"count": 0 if "count" in variant else 3, "start": "2^64 - 1" if "start" in variant else 2},
                                                                     "underflow" if "count" in variant else "2^64 + 1", True),
                                                     note=("A Verifier finds AB invalid; the Scanner is unaffected." if sep else
                                                           "Replica A is ineligible; B and C supply the inventory (degraded).")),
                                  "expect": {"separation A-B": "invalid"} if sep else {"replica A": "TerminalIndexReplicaParse"}}

    # -- neg-58: real_data_shard_count, isolated
    def apply58a(ws, res):
        key = (2, 0)
        edit_int(ws, res, key, 0x48, 8, 4, 3, "real_data_shard_count")
        edit_int(ws, res, key, 0x58, 8, 4, 3, "data_crc_count")
        edit_int(ws, res, key, 0x68, 8, 96, 88, "inline_index_entry_bytes")
        fourth = bytes(a4().files[2].blocks[0][0x120:0x128])
        edit_bytes(ws, res, key, 0x120, fourth, bytes(8), "fourth data-CRC entry")
        repair_sc_hashed(ws, res, key, 88)

    def apply58b(ws, res):
        key = (2, 0)
        edit_int(ws, res, key, 0x38, 8, 4, 5, "protected_ordinal_end_exclusive")
        edit_int(ws, res, key, 0x48, 8, 4, 5, "real_data_shard_count")
        edit_int(ws, res, key, 0x58, 8, 4, 5, "data_crc_count")
        edit_int(ws, res, key, 0x68, 8, 96, 104, "inline_index_entry_bytes")
        edit_bytes(ws, res, key, 0x128, bytes(8), le64(0x261BDF3D299838FC), "fifth data-CRC entry")
        repair_sc_hashed(ws, res, key, 104)
    t["neg-58/a-not-end-minus-start"] = {"image": "a4-minimal", "apply": apply58a, "roles": [("copy", 2, 0, 1), ("recoverer", [1, 0])],
                                         "decide": decision("rejected", "SidecarParse", ["sc_real_text", "err_sidecar"],
                                                            "Exactly one Section 9.2 rule is broken: real (3) ≠ end − start (4)."),
                                         "expect": {"copy": "SidecarParse"}}
    t["neg-58/b-above-logical"] = {"image": "a4-minimal", "apply": apply58b, "roles": [("copy", 2, 0, 1), ("recoverer", [1, 0])],
                                   "decide": decision("rejected", "SidecarParse", ["sc_real_text", "sc_real", "err_sidecar"],
                                                      "Exactly one Section 9.2 rule is broken: real (5) is outside 1..=S×k (4)."),
                                   "expect": {"copy": "SidecarParse"}}
    return t


def run_negative(entry_id: str, spec: dict[str, Any], case: dict[str, Any]) -> dict[str, Any]:
    case_id, _, variant = entry_id.partition("/")
    out: dict[str, Any] = {"id": case_id, "variant": variant or None, "target": case.get("target", case.get("role")),
                           "apply": None, "decision": spec["decide"], "implementation": [], "self_check": None}
    if spec.get("none"):
        out["apply"] = {"resolved": None, "vector": "none", "checks": [], "repairs": [], "mutated_blocks": [],
                        "notes": ["no tape vector: " + (case.get("evaluation") or case.get("base", {}).get("note", ""))]}
        out["self_check"] = {"agrees": None, "detail": "nothing for the implementation to run"}
        return out
    if "resume" in spec:
        resume_input = spec["resume"]()
        decision_out, _ = resume_case(resume_input, _image_cache(resume_input["image"]), {"case_id": entry_id})
        d = decision_out["decision"]
        out["apply"] = {"resolved": True, "vector": "injected commit record (no tape bytes change)", "checks": [],
                        "repairs": [], "mutated_blocks": [],
                        "notes": [f"prefix: derived W {decision_out['prefix']['derived_W']}, T {decision_out['prefix']['derived_T']}"]}
        out["implementation"].append({"level": "Resumer", "result": d["result"], "error": d["error"],
                                      "refused_at": d["refused_at"], "records_read": d["records_read"],
                                      "violations": decision_out["step2"]["violations"]})
        agrees = d["error"] == spec["expect"]["resume"]
        out["self_check"] = {"agrees": agrees, "detail": f"Resumer {d['result']} {d['error']} at {d['refused_at']}"}
        return out
    ws = Workspace(image=_image_cache(spec["image"])) if "image" in spec else Workspace(profile=spec["profile"])
    res = Resolution()
    spec["apply"](ws, res)
    mutated = [{"where": ws.describe(key), "bytes": len(ws.blocks[key]),
                "sha256": hashlib.sha256(bytes(ws.blocks[key])).hexdigest()} for key in ws.touched]
    out["apply"] = {"resolved": res.resolved, "vector": "bytes", "checks": res.checks, "repairs": res.repairs,
                    "mutated_blocks": mutated, "notes": res.notes}
    if not res.resolved:
        out["decision"] = decision("unresolved", None, [], "", note="the description does not resolve against my bytes")
        return out
    for role in spec.get("roles", []):
        kind = role[0]
        if kind == "copy":
            out["implementation"].append(role_sidecar_copy(ws, role[1], role[2], role[3]))
        elif kind == "footer":
            out["implementation"].append(role_sidecar_footer(ws, role[1], role[2]))
        elif kind == "recoverer":
            out["implementation"].append(role_recoverer(ws, role[1]))
        elif kind == "verifier-copies":
            out["implementation"].append(role_verifier_copies(ws, role[1]))
        elif kind == "bootstrap":
            out["implementation"].append(role_bootstrap(ws))
        elif kind == "bootstrap-discovery":
            out["implementation"].append(role_bootstrap_discovery(ws))
        elif kind == "parity-map":
            out["implementation"].append(role_parity_map(ws, role[1]))
        elif kind == "terminal":
            out["implementation"].append(role_terminal(ws))
    out["self_check"] = self_check(spec["expect"], out["implementation"])
    return out


def self_check(expect: dict[str, str], results: list[dict[str, Any]]) -> dict[str, Any]:
    details = []
    agrees = True
    for key, wanted in expect.items():
        found = None
        if key in ("copy", "footer", "bootstrap", "bootstrap-discovery", "parity-map", "verifier"):
            level = {"copy": "copy", "footer": "sidecar footer", "bootstrap": "bootstrap frame parser",
                     "bootstrap-discovery": "bootstrap discovery", "parity-map": "ParityMap",
                     "verifier": "Verifier"}[key]
            match = next((r for r in results if level in r["level"]), None)
            if match is not None:
                found = match["error"] if match["result"] == "rejected" and match["error"] else match["result"]
                if wanted == "rejected":
                    found = match["result"]
        elif key == "recoverer":
            match = next((r for r in results if "Recoverer" in r["level"]), None)
            found = match and (match["error"] or match["result"])
        elif key.startswith("replica "):
            match = next((r for r in results if "replicas" in r), None)
            if match:
                replica = match["replicas"].get(key.split()[1], {})
                reason = replica.get("reason", "")
                if reason.startswith("payload: "):
                    reason = reason[len("payload: "):]
                found = "valid" if replica.get("valid") else reason.split(":")[0]
        elif key.startswith("separation "):
            match = next((r for r in results if "separations" in r), None)
            found = match and match["separations"].get(key.split()[1], {}).get("status")
        elif key == "selection":
            match = next((r for r in results if "selection" in r), None)
            found = match and match["selection"]
        ok = found == wanted
        agrees = agrees and ok
        details.append(f"{key}: expected {wanted}, implementation {found}")
    return {"agrees": agrees, "detail": "; ".join(details)}


def mutate_boot_no_parity_compression(ws: Workspace, res: Resolution) -> None:
    """e1-16: set the no-parity flag, drop the scheme record and record compression; repair lengths, fill and CRCs."""
    block = ws.block((0, 0))
    flags = struct.unpack_from(">I", block, 0x0C)[0]
    res.checks.append({"where": ws.describe((0, 0)), "offset": "0x0C", "field": "flags bit 0 (no-parity)",
                       "expected_from": 0, "found": flags & 1, "matches": flags & 1 == 0, "to": 1})
    length = rd32(block, 0x2C)
    payload = decode_deterministic_cbor(bytes(block[0x38 : 0x38 + length]))
    res.checks.append({"where": ws.describe((0, 0)), "offset": "payload key 1", "field": "scheme record present",
                       "expected_from": True, "found": 1 in payload, "matches": 1 in payload, "to": "removed"})
    res.checks.append({"where": ws.describe((0, 0)), "offset": "payload key 5", "field": "drive_compression",
                       "expected_from": False, "found": payload.get(5), "matches": payload.get(5) is False, "to": True})
    kept = {key: payload[key] for key in (2, 3, 4)}
    payload.pop(1, None)
    payload[5] = True
    res.checks.append({"where": ws.describe((0, 0)), "offset": "payload keys 2, 3, 4", "field": "unchanged",
                       "expected_from": "unchanged", "found": "unchanged" if all(payload[k] == v for k, v in kept.items())
                       else "changed", "matches": all(payload[k] == v for k, v in kept.items()), "to": "unchanged"})
    encoded = encode_deterministic_cbor(payload)
    tail = encoded + le64(crc64_xz(encoded))
    block[0x0C:0x10] = struct.pack(">I", flags | 1)
    block[0x2C:0x30] = le32(len(encoded))
    block[0x30:0x38] = le64(crc64_xz(bytes(block[0x00:0x30])))
    block[0x38:] = tail + bytes(ws.block_size - 0x38 - len(tail))
    res.repairs.append(f"bootstrap payload {length} -> {len(encoded)} bytes: cbor_payload_len, zero fill after the "
                       "shorter payload, crc64_payload at its new position, crc64_header")


BOOT_LEVELS_NO_PARITY_COMPRESSION = {
    "a parser given the block": "accepted: a no-parity bootstrap may record compression (Sections 8.2 and 16.3)",
    "discovery over the candidate sizes, without supplied values": "found: a usable no-parity bootstrap; "
        "DriveCompressionEnabled concerns only a parity bootstrap (Section 15)",
    "discovery with supplied values": "depends on the values: supplied without parity, accepted; supplied with a "
        "parity scheme, the no-parity flag disagrees, so it is refused (BootstrapParse, naming the no-parity flag, "
        "Section 8.4)",
}


def e1_negatives_table() -> dict[str, dict[str, Any]]:
    """The E1 negative case (a bootstrap-role case), decided from the text."""
    return {"e1-16": {
        "image": "a4-minimal",
        "apply": mutate_boot_no_parity_compression,
        "roles": [("bootstrap",), ("bootstrap-discovery",)],
        "decide": with_levels(decision(
            "accepted", None, ["compression_key_rule", "compression_parity_only", "no_parity_may_omit",
                               "boot_parser_name"],
            "No rule of Section 8 fails. Key 5 is rejected only on a parity bootstrap, and this one sets the no-parity "
            "flag. A no-parity bootstrap may omit the scheme record, and a Reader must not require it. Its digest "
            "record (key 2) may be kept. The parser's one exception for recorded compression names a parity bootstrap.",
            must_reject=False,
            note="The tape identifies itself as written without parity and with compression; Section 16.3's rejection "
                 "concerns a parity tape."),
            BOOT_LEVELS_NO_PARITY_COMPRESSION),
        "expect": {"bootstrap": "accepted", "bootstrap-discovery": "found"},
    }}


def run_negatives(cases_path: pathlib.Path, out_path: pathlib.Path,
                  table: dict[str, dict[str, Any]] | None = None) -> dict[str, Any]:
    source = load_json(cases_path)
    by_id = {case["id"]: case for case in source["cases"]}
    table = negatives_table() if table is None else table
    entries: dict[str, Any] = {}
    covered = set()
    for case in source["cases"]:
        variants = [v["variant"] for v in case.get("variants", [])] or [None]
        for variant in variants:
            entry_id = case["id"] + (f"/{variant}" if variant else "")
            covered.add(entry_id)
            if entry_id not in table:
                entries[entry_id] = {"id": case["id"], "variant": variant, "target": case.get("target"),
                                     "apply": {"resolved": False, "notes": ["no entry in this implementation's table"]},
                                     "decision": decision("unresolved", note="not analysed"), "implementation": [],
                                     "self_check": None}
                continue
            entries[entry_id] = run_negative(entry_id, table[entry_id], by_id[case["id"]])
    extra = sorted(set(table) - covered)
    output = {"schema": "rem-parity-second-implementation-negatives/1", "entries": entries,
              "table_entries_without_a_case": extra}
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(output, indent=2, ensure_ascii=False, default=_json_default) + "\n", encoding="utf-8")
    return output


def negative_summary(entry: dict[str, Any]) -> str:
    d = entry["decision"]
    text = d["outcome"]
    if d["error"] and d["error"] != "undecided":
        text += f" {d['error']}"
    if d["error_set"]:
        text += " {" + " | ".join(d["error_set"]) + "}"
    if d["error"] == "undecided" and d["readings"]:
        text += " (name undecided: " + " | ".join(r.split(":")[0] for r in d["readings"]) + ")"
    elif d["outcome"] in ("undecided", "no-vector") and d["readings"]:
        text += " (" + " | ".join(r.split(":")[0] for r in d["readings"]) + ")"
    if entry["apply"] and entry["apply"].get("resolved") is False:
        text += " [UNRESOLVED]"
    check = entry["self_check"]
    if check and check["agrees"] is False:
        text += f" [SELF-CHECK FAILED: {check['detail']}]"
    return text


# ---------------------------------------------------------------------------
# Supplemental single-violation negatives: apply, check isolation rule by
# rule, and decide.
# ---------------------------------------------------------------------------

QUOTES.update({
    "p_defined": ("9.1", "One sidecar is written per parity epoch, in its own tape file of `total = 2H + P + 1` blocks, where `H` is the header/index copy block count (Section 9.4) and `P = S × m` is the parity shard block count:"),
    "h_label": ("9.2", "| 0x60 | 8 | sidecar_header_block_count u64 (H) | MUST equal the recomputed layout (Section 9.4) |"),
    "terminal_u64": ("8.3", "Arithmetic on these fields is checked in `u64` (Section 2.4)."),
    "sep_formulas": ("10.5", "actual_bytes     = total_records × B"),
    "size_formula_10_4": ("10.4", "payload_padding_bytes  = payload_record_count × B − payload_len"),
    "dir_partition": ("10.1.5", "the protected ranges **partition `[0, scope_highest_protected_ordinal)`**: taken in ascending `tape_file_number` order, the first `protected_ordinal_start` is `0`, each entry's `protected_ordinal_start` equals the previous entry's `protected_ordinal_end_exclusive` (contiguous, no gaps and no overlaps), every range is non-empty, and the last `protected_ordinal_end_exclusive` equals `scope_highest_protected_ordinal` (or the directory is empty and `scope_highest_protected_ordinal = 0`);"),
    "dir_epochs": ("10.1.5", "`epoch_id` values are unique and consecutive starting from `0` (`0, 1, …, count−1`), matching the bare monotonic epoch counter of Section 2.3."),
    "epoch_at_most": ("2.3", "**Epoch**: one parity protection unit covering a non-empty explicit, half-open range of at most `S × k` data ordinals."),
    "structural_invariants": ("10.2", "Every structural and ordinal-range invariant is validated."),
    "scope_in_list": ("10.6", "Every fixed slot of the payload decodes, and the dense map, scope, deterministic CBOR, zero padding, exact map↔Object-row bijection, payload digest, and canonical-map digest validate."),
    "footer_observations": ("8.4", "The footer's tape-file, start, and count fields are Writer observations, not an independent device tape-file counter."),
    "only_kind_differ": ("9.1", "The tail copy MUST carry metadata and index content identical to the primary; only `copy_kind` and the recomputed CRCs differ."),
})


def pack_sidecar_header(tape_uuid: bytes, v: dict[str, int], copy_kind: int, metadata_hash: bytes) -> bytearray:
    header = bytearray(0xC8)
    header[0x00:0x08] = role_magic(tape_uuid, LABEL_SIDECAR)
    header[0x08:0x18] = v.get("uuid", tape_uuid)
    fields = [(0x18, 8, "epoch"), (0x20, 2, "k"), (0x22, 2, "m"), (0x24, 4, "S"), (0x28, 4, "block_size"), (0x2C, 4, "schema"),
              (0x30, 8, "start"), (0x38, 8, "end"), (0x40, 8, "logical"), (0x48, 8, "real"), (0x50, 8, "P"),
              (0x58, 8, "crc_count"), (0x60, 8, "H"), (0x68, 8, "inline"), (0x70, 8, "total"), (0x78, 8, "primary"),
              (0x80, 8, "tail"), (0x88, 8, "footer_index")]
    for offset, width, key in fields:
        header[offset : offset + width] = v[key].to_bytes(width, "little")
    header[0x90:0x92] = le16(copy_kind)
    header[0x98:0xB8] = metadata_hash
    header[0xC0:0xC8] = le64(crc64_xz(bytes(header[0x00:0xC0])))
    return header


def build_sidecar_explicit(tape_uuid: bytes, block_size: int, v: dict[str, int], entries: list[bytes],
                           parity: list[bytes], copy_blocks: int, extra_tail_blocks: int = 0) -> tuple[list[bytes], bytes]:
    """Assemble a sidecar from explicit field values (for the rebuilt-image variants).

    Each copy is `copy_blocks` blocks: block 0 holds the header and every entry,
    later blocks are empty index blocks (zero below a valid CRC). The parity
    shards follow the primary copy in the given order, then the tail copy, the
    footer, and any extra blocks.
    """
    stream = b"".join(entries)
    provisional = pack_sidecar_header(tape_uuid, v, 1, bytes(32))
    metadata_hash = digest(SIDECAR_METADATA_DOMAIN + bytes(provisional[0x00:0x90]) + stream)

    def copy(kind: int) -> list[bytes]:
        head = bytearray(block_size)
        head[0:0xC8] = pack_sidecar_header(tape_uuid, v, kind, metadata_hash)
        head[0xC8 : 0xC8 + len(stream)] = stream
        head[block_size - 8 :] = le64(crc64_xz(bytes(head[: block_size - 8])))
        blocks = [bytes(head)]
        for _ in range(copy_blocks - 1):
            empty = bytearray(block_size)
            empty[block_size - 8 :] = le64(crc64_xz(bytes(empty[: block_size - 8])))
            blocks.append(bytes(empty))
        return blocks

    footer = bytearray(block_size)
    footer[0x00:0x08] = role_magic(tape_uuid, LABEL_SIDECAR_FOOTER)
    footer[0x08:0x0A] = le16(2)
    footer[0x10:0x20] = tape_uuid
    for offset, key in ((0x20, "epoch"), (0x28, "start"), (0x30, "end"), (0x38, "H"), (0x40, "P"), (0x48, "total"),
                        (0x50, "primary"), (0x58, "tail")):
        footer[offset : offset + 8] = le64(v[key])
    footer[0x60:0x80] = metadata_hash
    footer[0x80:0x88] = le64(crc64_xz(bytes(footer[0x00:0x80])))
    blocks = copy(1) + list(parity) + copy(2) + [bytes(footer)] + [bytes(block_size)] * extra_tail_blocks
    return blocks, metadata_hash


def rebuild_a4(sidecar_blocks: list[bytes], sidecar_entry: MapEntry, directory_entry: dict[int, Any],
               object_blocks: list[bytes] | None = None, object_row: dict[int, Any] | None = None,
               name: str = "a4-minimal (rebuilt)") -> ImageBuild:
    """ISO-REBUILD: a4-minimal with a new sidecar (and optionally a new Object), everything after regenerated."""
    base = _image_cache("a4-minimal")
    inputs = load_image_inputs("a4-minimal")
    identity = inputs["writer_identity"]
    object_blocks = object_blocks if object_blocks is not None else base.files[1].blocks
    object_row = object_row if object_row is not None else base.object_rows[0]
    entries = [MapEntry(0, KIND_BOOTSTRAP, 1), MapEntry(1, KIND_OBJECT, len(object_blocks), 0), sidecar_entry]
    parity_map, parity_entry = build_parity_map(base.tape_uuid, base.block_size, 0, 3, [directory_entry], entries,
                                                identity["writer_version"], identity["write_timestamp"])
    prefix = entries + [parity_entry]
    terminal = build_terminal_suffix(TerminalInputs(
        tape_uuid=base.tape_uuid, block_size=base.block_size, structural=prefix, object_rows=[object_row],
        edition_id=bytes.fromhex(inputs["edition_id_hex"]), edition_sequence=inputs["edition_sequence"],
        writer_version=identity["writer_version"].encode(), write_timestamp=identity["write_timestamp"].encode(),
        nominal_extent_bytes=inputs["nominal_extent_bytes"], start_tape_file=len(prefix),
        start_lba=sum(e.block_count + 1 for e in prefix), covered_count=len(prefix),
        total_data_ordinals=derived_total(prefix), highest_protected_ordinal=derived_watermark(prefix)))
    files = [TapeFile(0, KIND_BOOTSTRAP, base.files[0].blocks, True, {}),
             TapeFile(1, KIND_OBJECT, list(object_blocks), True, {}),
             TapeFile(2, KIND_SIDECAR, list(sidecar_blocks), True, {}),
             TapeFile(3, KIND_PARITY_MAP, parity_map.blocks, True, {})]
    for index, component in enumerate(terminal.components):
        files.append(TapeFile(4 + index, terminal.plan[index][0], component, True, {}))
    return ImageBuild(name, base.block_size, base.tape_uuid, base.scheme, files, prefix, [object_row], terminal, True)


def a4_sidecar_values() -> dict[str, int]:
    head = _image_cache("a4-minimal").files[2].blocks[0]
    keys = [(0x18, 8, "epoch"), (0x20, 2, "k"), (0x22, 2, "m"), (0x24, 4, "S"), (0x28, 4, "block_size"), (0x2C, 4, "schema"),
            (0x30, 8, "start"), (0x38, 8, "end"), (0x40, 8, "logical"), (0x48, 8, "real"), (0x50, 8, "P"),
            (0x58, 8, "crc_count"), (0x60, 8, "H"), (0x68, 8, "inline"), (0x70, 8, "total"), (0x78, 8, "primary"),
            (0x80, 8, "tail"), (0x88, 8, "footer_index")]
    return {key: int.from_bytes(head[o : o + w], "little") for o, w, key in keys}


class TerminalWorkspace(Workspace):
    """A Workspace over a terminal suffix built here, with placeholder prefix records."""

    def __init__(self, terminal: TerminalBuild, tape_uuid: bytes, block_size: int, placeholder: list[int], label: str) -> None:
        self.image = None
        self.profile = label
        self.blocks = {}
        self.touched = []
        self.terminal = terminal
        self.tape_uuid = tape_uuid
        self.block_size = block_size
        self.placeholder = placeholder

    def records(self) -> list[Any]:
        out: list[Any] = []
        for count in self.placeholder:
            out.extend([bytes(self.block_size)] * count)
            out.append(None)
        for component, blocks in enumerate(self.terminal.components):
            for record in range(len(blocks)):
                key = (component, record)
                out.append(bytes(self.blocks[key]) if key in self.blocks else blocks[record])
            out.append(None)
        return out


def multi_inputs() -> dict[str, Any]:
    return load_json(FIXTURE_ROOT / "multi-256k" / "inputs.json")


def terminal_workspace(structural: list[MapEntry], rows: list[dict], placeholder: list[int], label: str,
                       total: int, watermark: int, start_tape_file: int | None = None,
                       start_lba: int | None = None) -> TerminalWorkspace:
    base = multi_inputs()
    terminal = build_terminal_suffix(TerminalInputs(
        tape_uuid=bytes.fromhex(base["tape_uuid"]), block_size=base["block_size"], structural=structural,
        object_rows=rows, edition_id=bytes.fromhex(base["edition_id"]), edition_sequence=base["edition_sequence"],
        writer_version=base["diagnostics"]["writer_version"].encode(),
        write_timestamp=base["diagnostics"]["write_timestamp"].encode(),
        nominal_extent_bytes=base["separation_extent"]["nominal_extent_bytes"],
        start_tape_file=len(structural) if start_tape_file is None else start_tape_file,
        start_lba=(sum(c + 1 for c in placeholder) if start_lba is None else start_lba),
        covered_count=len(structural), total_data_ordinals=total, highest_protected_ordinal=watermark))
    return TerminalWorkspace(terminal, bytes.fromhex(base["tape_uuid"]), base["block_size"], placeholder, label)


# ----- the rule-by-rule sidecar copy auditor -----------------------------------


def audit_sidecar_copy(blocks: list[bytes], tape_uuid: bytes, block_size: int, copy_kind: int) -> dict[str, Any]:
    """Evaluate every Section 9.2-9.5 rule of one header/index copy on its own.

    The locator formulas (total, tail start, footer index) are evaluated under
    two readings of their symbols: the text's (H = the field the table labels
    "(H)", P = S × m as Section 9.1 defines it) and this implementation's
    parser's (H = the Section 9.4 recompute, P = the 0x50 field). The hash is
    evaluated under both readings of Section 9.5 (wire bytes; 0x78 taken as 0).
    """
    head = blocks[0]
    g = lambda o, w: int.from_bytes(head[o : o + w], "little")  # noqa: E731
    k, m, stripes, bs = g(0x20, 2), g(0x22, 2), g(0x24, 4), g(0x28, 4)
    start, end, logical, real, p_field = g(0x30, 8), g(0x38, 8), g(0x40, 8), g(0x48, 8), g(0x50, 8)
    crc_count, h_field, inline_field, total = g(0x58, 8), g(0x60, 8), g(0x68, 8), g(0x70, 8)
    primary, tail, footer_index = g(0x78, 8), g(0x80, 8), g(0x88, 8)
    common: list[str] = []

    def rule(ok: bool, text: str) -> None:
        if not ok:
            common.append(text)

    rule(head[0:8] == role_magic(tape_uuid, LABEL_SIDECAR), "§9.2 magic")
    rule(head[0x08:0x18] == tape_uuid, "§9.2 tape_uuid MUST match the bootstrap")
    rule(k != 0, "§9.2 k ≠ 0")
    rule(m != 0, "§9.2 m ≠ 0")
    rule(stripes != 0, "§9.2 S ≠ 0")
    rule(bs == block_size, "§9.2 block_size MUST equal the actual block size")
    rule(g(0x2C, 4) == 2, "§9.2 schema_version MUST be 2")
    rule(end > start, "§9.2 end > start")
    rule(logical == stripes * k, "§9.2 logical_shard_count MUST = S × k")
    rule(end >= start and real == end - start, "§9.2 real_data_shard_count MUST equal end − start"
         + (" (end − start underflows)" if end < start else ""))
    rule(1 <= real <= stripes * k and real <= logical, "§9.2 real_data_shard_count MUST be in 1..=S × k (≤ logical)")
    rule(p_field == stripes * m, "§9.2 parity_block_count MUST = S × m")
    rule(crc_count == real, "§9.2 data_crc_count MUST = real_data_shard_count")
    layout = None
    if stripes and stripes * m + real <= 4_000_000:
        try:
            layout = sidecar_layout(block_size, stripes, m, real)
        except BuildRefusal:
            layout = None
    if layout is None:
        common.append("§9.4 the layout cannot be recomputed for these counts (so H and inline cannot be checked)")
    else:
        rule(h_field == layout[0], "§9.2/§9.4 H MUST equal the recomputed layout")
        rule(inline_field == layout[1], "§9.2/§9.4 inline_index_entry_bytes MUST equal the recomputed layout")
    rule(primary == 0, "§9.2 primary_header_start_block MUST = 0")
    rule(g(0x90, 2) == copy_kind, f"§9.2 copy_kind MUST be {copy_kind} for this copy")
    rule(g(0x92, 2) == 0, "§9.2 reserved (0x92) MUST be 0")
    rule(g(0x94, 4) == 0, "§9.2 copy_generation MUST be 0")
    rule(g(0xB8, 8) == 0, "§9.2 reserved u64 (0xB8) MUST be 0")
    rule(crc64_xz(head[0:0xC0]) == g(0xC0, 8), "§9.2 header_crc64")
    rule(crc64_xz(head[: block_size - 8]) == int.from_bytes(head[block_size - 8 :], "little"), "§9.2 block0_crc64")
    stream = b""
    if layout is not None:
        placements = layout[2]
        parity_entries = stripes * m
        number = 0
        used_ok = True
        entries_ok = True
        for block_index, offsets in enumerate(placements):
            if block_index >= len(blocks):
                break
            block = blocks[block_index]
            used = bytearray(block[: block_size - 8])
            if block_index == 0:
                used[0:0xC8] = bytes(0xC8)
            elif crc64_xz(block[: block_size - 8]) != int.from_bytes(block[block_size - 8 :], "little"):
                common.append(f"§9.3 index block {block_index} CRC")
            for offset in offsets:
                length = 16 if number < parity_entries else 8
                entry = bytes(block[offset : offset + length])
                stream += entry
                if number < parity_entries:
                    s_i, p_i, reserved, _ = struct.unpack("<IHHQ", entry)
                    if (s_i, p_i) != (number // m, number % m):
                        entries_ok = False
                    if reserved:
                        common.append(f"§9.3 parity entry {number}: u16 reserved MUST be 0")
                used[offset : offset + length] = bytes(length)
                number += 1
            if any(used):
                used_ok = False
        for block_index in range(len(placements), len(blocks)):
            block = blocks[block_index]
            if crc64_xz(block[: block_size - 8]) != int.from_bytes(block[block_size - 8 :], "little"):
                common.append(f"§9.3 index block {block_index} CRC")
            if any(block[: block_size - 8]):
                used_ok = False
        if not entries_ok:
            common.append("§9.3 parity entries in stripe-major order")
        if not used_ok:
            common.append("§9.2/§9.3 unused space below the CRC MUST be zero")
    wire = digest(SIDECAR_METADATA_DOMAIN + bytes(head[0x00:0x90]) + stream)
    zeroed = bytearray(head[0x00:0x90])
    zeroed[0x78:0x80] = bytes(8)
    substituted = digest(SIDECAR_METADATA_DOMAIN + bytes(zeroed) + stream)
    readings = {}
    for label, h_value, p_value in (("text (H = 0x60 field, P = S × m)", h_field, stripes * m),
                                    ("implementation (H = §9.4 recompute, P = 0x50 field)",
                                     layout[0] if layout else h_field, p_field)):
        extra = []
        if overflows(2 * h_value + p_value + 1) or total != 2 * h_value + p_value + 1:
            extra.append("§9.2 sidecar_total_block_count = 2H + P + 1")
        if overflows(h_value + p_value) or tail != h_value + p_value:
            extra.append("§9.2 tail_header_start_block MUST = H + P")
        if overflows(2 * h_value + p_value) or footer_index != 2 * h_value + p_value:
            extra.append("§9.2 footer_block_index MUST = 2H + P")
        readings[label] = extra
    hash_readings = {"wire bytes": [] if head[0x98:0xB8] == wire else ["§9.5 canonical_metadata_hash does not verify"],
                     "0x78 taken as 0": [] if head[0x98:0xB8] == substituted else ["§9.5 canonical_metadata_hash does not verify"]}
    return {"common": common, "locator_readings": readings, "hash_readings": hash_readings, "stream": stream,
            "hash": bytes(head[0x98:0xB8])}


def isolation_summary(audits: dict[str, dict[str, Any]], named: str) -> dict[str, Any]:
    """Collect the failing rules per copy and reading, and say whether more than the named rule fails."""
    out: dict[str, Any] = {"by_copy": {}}
    disputes = []
    for copy_name, audit in audits.items():
        view = {}
        for locator_label, locator in audit["locator_readings"].items():
            for hash_label, hash_fail in audit["hash_readings"].items():
                failing = sorted(set(audit["common"] + locator + hash_fail))
                view[f"{locator_label}; hash over {hash_label}"] = failing
        out["by_copy"][copy_name] = view
        impl_key = "implementation (H = §9.4 recompute, P = 0x50 field); hash over wire bytes"
        text_keys = [key for key in view if key.startswith("text")]
        if len(view[impl_key]) > 1:
            disputes.append(f"{copy_name}: this implementation's readings find {len(view[impl_key])} failing rules: "
                            + "; ".join(view[impl_key]))
        text_counts = {key: len(view[key]) for key in text_keys}
        out["by_copy"][copy_name + " (count under the text reading)"] = text_counts
    out["disputed_by_implementation"] = bool(disputes)
    out["disputes"] = disputes
    out["named_rule"] = named
    return out


# ----- ISO-BOTH, footer faults and roles on a Workspace ------------------------


def iso_both(ws: Workspace, res: Resolution, edit: Any, hashed: bool, mirror_footer: bool, mirror_directory: bool,
             tape_file: int = 2, tail_block: int = 5, footer_block: int = 6, stream_len: int = 96) -> None:
    for block in (0, tail_block):
        edit(ws, res, (tape_file, block))
    if hashed:
        for block in (0, tail_block):
            b = ws.block((tape_file, block))
            b[0x98:0xB8] = digest(SIDECAR_METADATA_DOMAIN + bytes(b[0x00:0x90]) + bytes(b[0xC8 : 0xC8 + stream_len]))
    for block in (0, tail_block):
        b = ws.block((tape_file, block))
        b[0xC0:0xC8] = le64(crc64_xz(bytes(b[0x00:0xC0])))
        b[ws.block_size - 8 :] = le64(crc64_xz(bytes(b[: ws.block_size - 8])))
    new_hash = bytes(ws.block((tape_file, 0))[0x98:0xB8])
    res.repairs.append("ISO-BOTH: the edit in both copies" + (", canonical_metadata_hash recomputed in each" if hashed else
                                                             " (hash-excluded)") + ", header_crc64 and block0_crc64 of both")
    if hashed and mirror_footer:
        f = ws.block((tape_file, footer_block))
        f[0x60:0x80] = new_hash
        f[0x80:0x88] = le64(crc64_xz(bytes(f[0x00:0x80])))
        res.repairs.append("ISO-BOTH: the new hash in the footer (0x60), footer_crc64")
    if hashed and mirror_directory:
        pm_file = tape_file + 1

        def transform(payload: dict) -> None:
            payload[4][5][0][8] = new_hash
        repair_pm_payload(ws, res, pm_file, transform)
        res.repairs[-1] = "ISO-BOTH: the new hash in directory key 8; " + res.repairs[-1]


def lba(ws: Workspace, tape_file: int, block: int) -> int:
    return ws.image.file_start_lba(tape_file) + block


def role_scan_and_recover(ws: Workspace, unreadable: set[int], address: list[int]) -> dict[str, Any]:
    """The replica-route Scanner followed by the Recoverer, on the tape with its faults."""
    tape = DamagedTape(ws.records(), unreadable)
    boot = parse_bootstrap(tape.read_data(0), ws.block_size)
    layout, _ = discover_layout(tape, ws.tape_uuid, ws.block_size)
    checks = [check_replica(tape, layout, o, ws.tape_uuid, ws.block_size) for o in (1, 2, 3)] if layout else []
    valid = [c for c in checks if c.fully_valid]
    if not valid:
        return {"level": "Scanner then Recoverer", "result": "error", "error": "BotStructuralRecoveryRequired",
                "scanner": [c.reason for c in checks]}
    entries = valid[0].entries
    directory, directory_note = load_parity_map_directory(tape, entries, ws.tape_uuid, ws.block_size)
    ctx = RecoveryContext(tape, ws.tape_uuid, ws.block_size, boot["scheme"], entries, valid[0].header["covered"],
                          valid[0].header["watermark"], directory, "replica")
    outcome = recover_address(ctx, list(address))
    return {"level": f"Scanner (Inventory from {'/'.join(c.letter for c in valid)}) then Recoverer {address}",
            "result": outcome["result"], "error": outcome["error"], "epoch": outcome["epoch"],
            "index_acquisition": outcome.get("index_acquisition"), "directory": directory_note}


def sidecar_audits(ws: Workspace, tape_file: int = 2, tail_block: int = 5, copy_blocks: int = 1) -> dict[str, Any]:
    primary = [bytes(ws.block((tape_file, i))) for i in range(copy_blocks)]
    tail = [bytes(ws.block((tape_file, tail_block + i))) for i in range(copy_blocks)]
    return {"primary": audit_sidecar_copy(primary, ws.tape_uuid, ws.block_size, 1),
            "tail": audit_sidecar_copy(tail, ws.tape_uuid, ws.block_size, 2)}


def cross_checks(ws: Workspace, audits: dict[str, Any], footer_block: int | None, footer_readable: bool,
                 directory_file: int | None = 3) -> list[str]:
    """Rules between structures: the Section 9.1 agreement, the footer comparison and the directory hash."""
    failing = []
    p, t = audits["primary"], audits["tail"]
    ph = bytes(ws.block((2, 0))[0x00:0x90])
    tail_block = footer_block - 1 if footer_block else None
    if p["stream"] != t["stream"] or p["hash"] != t["hash"]:
        failing.append("§9.1 the tail copy MUST carry metadata and index content identical to the primary")
    if footer_readable and footer_block is not None:
        f = bytes(ws.block((2, footer_block)))
        if f[0x60:0x80] != p["hash"] or f[0x60:0x80] != t["hash"]:
            failing.append("§13.3 step 1 footer comparison: the footer's canonical_metadata_hash differs from a copy's")
    if directory_file is not None:
        try:
            tape = DamagedTape(ws.records(), set())
            entries = ws.image.prefix_entries
            directory, _ = load_parity_map_directory(tape, entries, ws.tape_uuid, ws.block_size)
            entry = (directory or {}).get(2)
            if entry is not None and entry["hash"] != p["hash"]:
                failing.append("§13.3 step 3 directory hash differs from the copies'")
        except ReadFailure:
            pass
    del ph, tail_block
    return failing


# ----- the ParityMap rule-by-rule auditor ---------------------------------------


def audit_parity_map(ws: Workspace, tape_file: int) -> dict[str, Any]:
    start = ws.image.file_start_lba(tape_file)
    count = len(ws.image.files[tape_file].blocks)
    blocks = [bytes(ws.block((tape_file, i))) for i in range(count)]
    failing: list[str] = []
    readings: dict[str, list[str]] = {}

    def fields(b: bytes) -> dict[str, Any]:
        return {"sequence": rd64(b, 0x20), "payload_len": rd64(b, 0x30), "payload_sha": b[0x38:0x58], "digest": b[0x58:0x78],
                "scope": rd64(b, 0x78), "total": rd64(b, 0x80), "watermark": rd64(b, 0x88), "final": b[0x90],
                "M": rd64(b, 0x98), "total_blocks": rd64(b, 0xA0), "primary": rd64(b, 0xA8), "tail": rd64(b, 0xB0),
                "footer": rd64(b, 0xB8), "uuid": b[0x10:0x20]}
    per = [fields(b) for b in blocks[:1] + blocks[1:2] + blocks[2:3]]
    for name, b, f in zip(("primary", "tail", "footer"), blocks, per):
        if crc64_xz(b[0:0xC0]) != rd64(b, 0xC0):
            failing.append(f"§10.1.3 {name} CRC")
        m_value = ceil_div(0xC8 + f["payload_len"], ws.block_size)
        if (f["M"], f["total_blocks"], f["primary"], f["tail"], f["footer"]) != (
                m_value, 2 * m_value + 1, 0, m_value, 2 * m_value):
            failing.append(f"§10.1.4 {name} locator arithmetic")
    if per[2]["total_blocks"] != count:
        failing.append("§10.1.4 measured length")
    payload = bytes(blocks[0][0xC8 : 0xC8 + per[0]["payload_len"]])
    decoded = decode_deterministic_cbor(payload)
    for name, f in zip(("primary", "tail", "footer"), per):
        mismatches = [key for key, value in (("sequence", decoded[3]), ("digest", decoded[5]), ("uuid", decoded[2]),
                                             ("scope", decoded[4][1]), ("total", decoded[4][2]),
                                             ("watermark", decoded[4][3]), ("final", 1 if decoded[4][4] else 0))
                      if f[key] != value]
        if mismatches:
            failing.append(f"§10.1.4 the decoded payload MUST match the {name}'s locator fields ({', '.join(mismatches)})")
    differing = [key for key in per[0] if key not in ("uuid",) and per[0][key] != per[2][key]]
    arithmetic_keys = {"payload_len", "M", "total_blocks", "primary", "tail", "footer"}
    readings["'reject disagreement' covers only the locator arithmetic"] = (
        [f"§10.1.4 header/footer disagreement in locator arithmetic ({', '.join(sorted(set(differing) & arithmetic_keys))})"]
        if set(differing) & arithmetic_keys else [])
    readings["'reject disagreement' covers every header field"] = (
        [f"§10.1.4 header/footer disagreement ({', '.join(sorted(differing))})"] if differing else [])
    entries = decoded[4][5]
    scope, watermark, total_ordinals = decoded[4][1], decoded[4][3], decoded[4][2]
    tfns = [e[1] for e in entries]
    if tfns != sorted(tfns) or len(set(tfns)) != len(tfns):
        failing.append("§10.1.5 entries strictly ascending by tape_file_number")
    if any(t >= scope for t in tfns):
        failing.append("§10.1.5 each tape_file_number < scope")
    if any(0 in (e[5], e[6], e[7]) for e in entries):
        failing.append("§10.1.5 non-zero total, H and P")
    if watermark > total_ordinals:
        failing.append("§10.1.5 W ≤ T")
    ordered = sorted(entries, key=lambda e: e[1])
    chain = 0
    partition_ok = True
    for e in ordered:
        if e[3] != chain or e[4] <= e[3]:
            partition_ok = False
        chain = e[4]
    if not partition_ok or chain != watermark:
        failing.append("§10.1.5 the protected ranges partition [0, W) in ascending tape_file_number order")
    ids = [e[2] for e in entries]
    readings["epoch ids as a set"] = [] if sorted(ids) == list(range(len(ids))) else ["§10.1.5 epoch ids unique and consecutive from 0"]
    readings["epoch ids in array order"] = [] if ids == list(range(len(ids))) else ["§10.1.5 epoch ids consecutive from 0 in array order"]
    readings["epoch ids in tape-file order"] = [] if [e[2] for e in ordered] == list(range(len(ids))) else \
        ["§10.1.5 epoch ids consecutive from 0 in tape-file order"]
    if any(e[9] & ~DIRECTORY_FLAG_MASK for e in entries):
        failing.append("§10.1.5 unknown flag bits")
    del start
    return {"common": failing, "readings": readings}


# ----- unit-level evaluators ---------------------------------------------------


def unit_eval(expression: str, value: int, inputs: dict[str, Any], note: str = "") -> dict[str, Any]:
    return {"expression": expression, "inputs": inputs, "value": str(value) if value >= 0 else f"{value} (underflow)",
            "type": "u64", "overflows": overflows(value), "note": note}


# ----- the supplement table ------------------------------------------------------


def supplement_table() -> dict[str, dict[str, Any]]:
    t: dict[str, dict[str, Any]] = {}
    base_values = a4_sidecar_values()

    def sidecar_rule(sup, parent, offset, width, frm, to, name, hashed, mirror_footer, mirror_directory, fault_footer,
                     rules, named, extra_note=""):
        def edit(ws, res, key):
            edit_int(ws, res, key, offset, width, frm, to, f"{name} ({'primary' if key[1] == 0 else 'tail'})")

        def apply(ws, res):
            iso_both(ws, res, edit, hashed, mirror_footer, mirror_directory)
        t[sup] = {"parent": parent, "image": "a4-minimal", "apply": apply,
                  "unreadable": [(2, 6)] if fault_footer else [],
                  "roles": ["audit", "copies", "recover"], "named": named,
                  "decide": decision("rejected", "SidecarParse", rules + ["err_sidecar"],
                                     "The named rule is a Section 9.2 copy rule; it fails in both copies, so neither "
                                     "validates. The Section 13.3 pin is not reached.",
                                     note="Both copies fail, so a Recoverer has no header/index copy for epoch 0: "
                                          "SidecarMetadataUnavailable{0} (Section 13.3 step 4)." + extra_note),
                  "expect": {"copy": "SidecarParse", "recoverer": "SidecarMetadataUnavailable"}}

    # -- unit-level variants
    units = {
        "sup-01": ("neg-45", "M = ceil((0xC8 + L) / B)", ceil_div((1 << 64) - 1 + 0xC8, 262144), {"L": "2^64 - 1", "B": 262144},
                   False, None, None, ["pm_M", "exact_values", "intermediate_ok"],
                   "M's exact value, 2^46 + 1, fits in u64. The sum 0xC8 + L exceeds u64 in one way of writing the "
                   "formula, which is not a format violation, and a Reader must not reject for it (Section 2.4). At image "
                   "level this payload_len is rejected anyway, by the payload hash and the measured length."),
        "sup-02": ("neg-42", "start(next) = start + record_count + 1", (1 << 64) - 3 + 3 + 1, {"start": "2^64 - 3", "record_count": 3},
                   True, "TerminalIndexReplicaParse", None, ["eod_rule", "checked", "layout_valid"],
                   "The next start is 2^64 + 1, which no u64 start field can equal, so the rule fails however it is evaluated."),
        "sup-04": ("neg-01", "EOD = start(C) + count(C) + 1", (1 << 64) - 4 + 3 + 1, {"start(C)": "2^64 - 4", "count(C)": 3},
                   True, "TerminalIndexReplicaParse", None, ["eod_rule", "checked", "layout_valid"],
                   "EOD would be 2^64, which no u64 EOD field can equal."),
        "sup-07": ("neg-21", "256 × object_row_count", 256 * (1 << 56), {"s": 1, "o": "2^56"}, True, "TerminalIndexReplicaParse", None,
                   ["size_formulas", "checked"], "Section 10.6 requires every Section 10.4 size formula to evaluate without overflow."),
        "sup-08": ("neg-15", "manifest_size_bytes ≤ manifest_chunk_count × block_size_bytes", 1,
                   {"first": 0, "count": "2^46", "size": 1, "stored": "2^46", "B": 262144}, False, None, None,
                   ["row_manifest", "compare_exact", "intermediate_ok"],
                   "The product 2^46 × 2^18 is only compared with manifest_size_bytes, so it is compared exactly and is "
                   "never too large (Section 2.4): 1 ≤ 2^64 holds, and the row is not rejected."),
        "sup-18": ("neg-40", "total = 2H + P + 1; footer index 2H + P", 2 * (1 << 63) + 4 + 1, {"H": "2^63", "P": 4}, True, "SidecarParse",
                   None, ["sc_total", "sc_footer_index", "checked"], "2^64 + 5 and 2^64 + 4 are values no u64 field can equal."),
        "sup-19": ("neg-55", "observed footer LBA = observed start LBA + delta", (1 << 64) - 1 + 2,
                   {"start": "2^64 - 1", "count": 3, "delta": 2}, True, "TerminalIndexReplicaParse", None, ["footer_delta", "checked"],
                   "The footer LBA would be 2^64 + 1, which no u64 field can equal."),
        "sup-20": ("neg-21", "payload_len = 64 × s + 256 × o", 64 * (1 << 57) + 256 * (1 << 55), {"s": "2^57", "o": "2^55"}, True,
                   "TerminalIndexReplicaParse", None, ["size_formulas", "checked"], "Each product fits; the sum 2^64 does not."),
        "sup-23": ("neg-57", "S × k", 0, {"S": "2^63", "k": 2}, False, None, None, ["compare_exact", "pin_then", "resume_step2"],
                   "In both roles the variant names, the Recoverer's pin and the Resumer's bound T − W < S × k, the "
                   "product is only compared, so it is compared exactly and is never too large (Section 2.4). A bootstrap "
                   "with this S is refused by Section 6.6 validity in the same bytes."),
        "sup-25": ("neg-49", "block(stripe, parity_index) = H + parity_index·S + stripe", (1 << 64) - 1 + 2,
                   {"H": "2^64 - 1", "S": 2, "parity_index": 1, "stripe": 0}, True, "SidecarParse", None,
                   ["parity_block_formula", "reject_if_not_fit"],
                   "The block index is used as a position, and its exact value 2^64 + 1 does not fit in u64, so the "
                   "Recoverer rejects it rather than place the read. The error is the one named for the structure that "
                   "carries the value, the sidecar whose H it is (Section 2.4)."),
        "sup-27": ("neg-02", "P = S × m", (1 << 63) * 2, {"S": "2^63", "m": 2}, True, "BootstrapParse", None,
                   ["reject_if_not_fit", "validity_6_6", "boot_names_level", "boot_parser_name"],
                   "P is a count, and its exact value 2^64 does not fit in u64, so a Reader deriving it rejects it. The "
                   "structure that carries S is the bootstrap's scheme record (Section 2.4); a parser given that block "
                   "reports BootstrapParse, and discovery without supplied values NoBootstrapFound (Section 15)."),
        "sup-28": ("neg-14", "total = 2M + 1 with M = ceil((0xC8 + L) / B)", 2 * (1 << 63) + 1, {"L": "2^64 - 0xC9", "B": 2},
                   True, "ParityMapParse", None, ["pm_locator", "checked"], "M = 2^63 fits; 2M + 1 = 2^64 + 1 does not."),
        "sup-29": ("neg-21", "payload_len = 64 × s", 64 * (1 << 58), {"s": "2^58", "o": 0}, True, "TerminalIndexReplicaParse", None,
                   ["size_formulas", "checked"], ""),
        "sup-31": ("neg-46", "payload_padding_bytes = payload_record_count × B − payload_len", 64,
                   {"s": "2^58 - 1", "o": 0, "B": 262144, "payload_record_count": "2^46"}, False, None, None,
                   ["size_formulas", "size_exact", "size_formula_10_4", "intermediate_ok"],
                   "The formula's exact value is 64, which fits, so it evaluates without overflow in Section 10.6's sense; "
                   "the intermediate product count × B = 2^64 is not a violation (Section 2.4)."),
        "sup-46": ("neg-55", "backward start delta = observed record count − 1", -1, {"count": 0}, True, "TerminalIndexReplicaParse", None,
                   ["footer_delta", "checked"], "−1 is not a u64; the recorded delta cannot equal it."),
        "sup-47": ("neg-50", "actual_bytes = total_records × B", (1 << 46) * 262144, {"E": "2^64 - 1", "B": 262144, "total_records": "2^46"},
                   True, "TerminalIndexSeparationParse", None, ["sep_formulas", "size_formulas", "checked"],
                   "Section 10.6 applies the overflow-free size rule to separations, read against Section 10.5."),
    }
    for sup, (parent, expression, value, inputs, must, name, readings, rules, note) in units.items():
        if must is True:
            outcome, error, shown = "rejected", name, None
        elif must is False:
            outcome, error, shown = "accepted", None, None
        else:
            outcome, error, shown = "undecided", None, readings
            note = (note + " " if note else "") + f"If it rejects, the name is {name}."
        unit = unit_eval(expression, value, inputs, note)
        if sup in ("sup-08", "sup-23"):
            product = {"sup-08": "2^46 × 2^18 = 2^64", "sup-23": "2^63 × 2 = 2^64"}[sup]
            unit.update(value=f"{product}, only compared", overflows=False, compared_exactly=True)
        t[sup] = {"parent": parent, "unit": unit, "named": "exact evaluation (Section 2.4)",
                  "decide": decision(outcome, error, rules, "Unit level: only the named computation is evaluated.",
                                     readings=shown, must_reject=must, formula=dict(unit, note=""),
                                     note=note)}

    # -- 7a: ISO-BOTH sidecar-copy variants on a4-minimal
    sidecar_rule("sup-03", "neg-32", 0x68, 8, 96, 88, "inline_index_entry_bytes", True, True, True, False,
                 ["sc_inline", "sc_recompute"], "§9.2/§9.4 inline")
    sidecar_rule("sup-05", "neg-22", 0x58, 8, 4, 3, "data_crc_count", True, True, True, False, ["sc_crc_count"], "§9.2 data_crc_count")
    sidecar_rule("sup-09", "neg-29", 0xB8, 8, 0, 1, "reserved u64", False, False, False, False, ["sc_reservedB8"], "§9.2 reserved 0xB8")
    sidecar_rule("sup-10", "neg-52", 0x50, 8, 4, 3, "parity_block_count", True, False, True, True, ["sc_P", "p_defined"],
                 "§9.2 parity_block_count",
                 " Isolation depends on the reading of P in the locator formulas (GAPS G-1).")
    sidecar_rule("sup-13", "neg-39", 0x08, 16, int.from_bytes(bytes.fromhex("12345678123442348234123456789abc"), "little"),
                 int.from_bytes(b"\xee" * 16, "little"), "tape_uuid", True, False, True, True, ["sc_uuid"], "§9.2 tape_uuid")
    sidecar_rule("sup-22", "neg-06", 0x78, 8, 0, 1, "primary_header_start_block", True, False, True, True,
                 ["sc_primary_start", "sc_hash_items"], "§9.2 primary start",
                 " Isolated under the wire-bytes reading of Section 9.5 (GAPS F-1).")
    sidecar_rule("sup-26", "neg-16", 0x94, 4, 0, 1, "copy_generation", False, False, False, False, ["sc_generation"], "§9.2 copy_generation")
    sidecar_rule("sup-30", "neg-28", 0x80, 8, 5, 6, "tail_header_start_block", True, False, True, True, ["sc_tail_start"],
                 "§9.2 tail start")
    sidecar_rule("sup-36", "neg-29", 0xCE, 2, 0, 1, "reserved u16 of parity entry 0", True, True, True, False,
                 ["sc_entry_reserved"], "§9.3 parity entry reserved")
    sidecar_rule("sup-40", "neg-31", 0x92, 2, 0, 1, "reserved u16", False, False, False, False, ["sc_reserved92"], "§9.2 reserved 0x92")
    sidecar_rule("sup-42", "neg-23", 0x28, 4, 262144, 524288, "block_size", True, True, True, False,
                 ["sc_block_size", "pin_then", "geometry_c"], "§9.2 block_size",
                 " Appendix C would call the geometry disagreement SchemeMismatch (GAPS F-2), but no copy is acquired, "
                 "so the pin is not reached.")
    sidecar_rule("sup-43", "neg-41", 0x88, 8, 6, 7, "footer_block_index", True, True, True, False, ["sc_footer_index"],
                 "§9.2 footer index")
    sidecar_rule("sup-49", "neg-13", 0x40, 8, 4, 5, "logical_shard_count", True, True, True, False, ["sc_logical"], "§9.2 logical")

    # sup-06: 0x78 = 1 with the original hash (substitute-0 reading); footer unreadable
    def apply06(ws, res):
        def edit(ws, res, key):
            edit_int(ws, res, key, 0x78, 8, 0, 1, f"primary_header_start_block ({'primary' if key[1] == 0 else 'tail'})")
        iso_both(ws, res, edit, hashed=False, mirror_footer=False, mirror_directory=False)
        res.repairs[-1] = "hash left as it was; header_crc64 and block0_crc64 of both copies"
    t["sup-06"] = {"parent": "neg-06", "image": "a4-minimal", "apply": apply06, "unreadable": [(2, 6)],
                   "roles": ["audit", "copies", "recover"], "named": "§9.2 primary start",
                   "decide": decision("rejected", "SidecarParse", ["sc_primary_start", "sc_hash_items", "sc_hash_verify", "err_sidecar"],
                                      "Isolated under the substitute-0 reading of Section 9.5; under the wire reading the hash "
                                      "rule fails too, with the same name (GAPS F-1).",
                                      note="Both copies fail: SidecarMetadataUnavailable{0} for a Recoverer."),
                   "expect": {"copy": "SidecarParse", "recoverer": "SidecarMetadataUnavailable"}}

    # sup-33: the same wrong hash in the copies, the footer and the directory
    def apply33(ws, res):
        wrong = bytearray(_image_cache("a4-minimal").files[2].blocks[0][0x98:0xB8])
        wrong[0] ^= 0x01

        def edit(ws, res, key):
            edit_bytes(ws, res, key, 0x98, None, bytes(wrong), "canonical_metadata_hash")
        iso_both(ws, res, edit, hashed=False, mirror_footer=False, mirror_directory=False)
        f = ws.block((2, 6))
        edit_bytes(ws, res, (2, 6), 0x60, None, bytes(wrong), "footer canonical_metadata_hash")
        f[0x80:0x88] = le64(crc64_xz(bytes(f[0x00:0x80])))

        def transform(payload):
            payload[4][5][0][8] = bytes(wrong)
        repair_pm_payload(ws, res, 3, transform)
        res.repairs.append("footer_crc64; the wrong hash in directory key 8")
    t["sup-33"] = {"parent": "neg-48", "image": "a4-minimal", "apply": apply33, "unreadable": [],
                   "roles": ["audit", "copies", "recover"], "named": "§9.5 hash",
                   "decide": decision("rejected", "SidecarParse", ["sc_hash_verify", "sc_hash_items", "err_sidecar"],
                                      note="Both copies fail: SidecarMetadataUnavailable{0} for a Recoverer."),
                   "expect": {"copy": "SidecarParse", "recoverer": "SidecarMetadataUnavailable"}}

    # sup-48: real = 3 in both copies with the companion edits
    def apply48(ws, res):
        fourth = bytes(_image_cache("a4-minimal").files[2].blocks[0][0x120:0x128])

        def edit(ws, res, key):
            for offset, frm, to, name in ((0x48, 4, 3, "real_data_shard_count"), (0x58, 4, 3, "data_crc_count"),
                                          (0x68, 96, 88, "inline_index_entry_bytes")):
                edit_int(ws, res, key, offset, 8, frm, to, name)
            edit_bytes(ws, res, key, 0x120, fourth, bytes(8), "fourth data-CRC entry")
        iso_both(ws, res, edit, hashed=True, mirror_footer=True, mirror_directory=True, stream_len=88)
    t["sup-48"] = {"parent": "neg-58", "image": "a4-minimal", "apply": apply48, "unreadable": [],
                   "roles": ["audit", "copies", "recover"], "named": "§9.2 real = end − start",
                   "decide": decision("rejected", "SidecarParse", ["sc_real_text", "err_sidecar"],
                                      note="Both copies fail: SidecarMetadataUnavailable{0} for a Recoverer."),
                   "expect": {"copy": "SidecarParse", "recoverer": "SidecarMetadataUnavailable"}}

    # sup-44: tail diverges; footer unreadable
    def apply44(ws, res):
        edit_xor(ws, res, (2, 5), 0x108, 0x01, "tail copy data-CRC entry 0")
        repair_sc_hashed(ws, res, (2, 5))
    t["sup-44"] = {"parent": "neg-11", "image": "a4-minimal", "apply": apply44, "unreadable": [(2, 6)],
                   "roles": ["audit", "copies", "verifier", "recover"], "named": "§9.1 agreement",
                   "decide": decision("rejected", "SidecarParse", ["verifier_divergence", "only_kind_differ", "entry_decides", "err_sidecar"],
                                      "Each copy validates alone; their hashes differ, which is the only rule that fails. "
                                      "A Verifier reads both copies and the footer and reports SidecarParse (Section 9.1).",
                                      note="With the footer unreadable, a Recoverer takes step 2: the directory entry is "
                                           "available and its hash equals the primary's, so the primary is used and the "
                                           "block is recovered (Section 13.3)."),
                   "expect": {"verifier": "SidecarParse", "recoverer": "recovered"}}

    # sup-38: footer P overflow; both header copies unreadable
    def apply38(ws, res):
        edit_int(ws, res, (2, 6), 0x40, 8, 4, (1 << 64) - 1, "footer P")
        repair_sc_footer(ws, res, (2, 6))
    t["sup-38"] = {"parent": "neg-17", "image": "a4-minimal", "apply": apply38, "unreadable": [(2, 0), (2, 5)],
                   "roles": ["footer", "recover"], "named": "§9.6 tail = H + P (checked)",
                   "decide": decision("rejected", "SidecarParse", ["ft_tail", "checked", "err_sidecar"],
                                      "H + P overflows, so the footer's only P-bearing rule cannot hold.",
                                      formula=unit_eval("tail_header_start_block = H + P", 1 + (1 << 64) - 1, {"H": 1, "P": "2^64 - 1"}),
                                      note="The footer is rejected. The primary is unreadable (step 2), and the directory's "
                                           "tail read meets the unreadable tail (step 3), so a Recoverer reports "
                                           "SidecarMetadataUnavailable{0}."),
                   "expect": {"footer": "SidecarParse", "recoverer": "SidecarMetadataUnavailable"}}

    # sup-16, sup-17: directory-assisted rescue with medium errors instead of CRC corruption
    for sup, value in (("sup-17", 7), ("sup-16", (1 << 64) - 1)):
        def apply_rescue(ws, res, value=value):
            def transform(payload):
                entry = payload[4][5][0]
                res.checks.append({"where": "a4-minimal tape file 3 payload", "offset": "directory entry 0 key 6",
                                   "field": "sidecar_header_block_count", "expected_from": 1, "found": entry[6],
                                   "matches": entry[6] == 1, "to": value})
                entry[6] = value
            repair_pm_payload(ws, res, 3, transform)
        t[sup] = {"parent": "neg-33", "image": "a4-minimal", "apply": apply_rescue, "unreadable": [(2, 0), (2, 6)],
                  "roles": ["pm-audit", "recover"], "named": "checked tail position",
                  "decide": decision("rejected", "SidecarMetadataUnavailable",
                                     ["tail_rescue", "entry_agrees", "rescue_preconditions", "metadata_unavailable",
                                      "unavailable_report", "err_metadata_unavailable"],
                                     "The directory validates and agrees with the map entry, but the entry's total (7) is "
                                     "not 2H + P + 1 for its H, so the entry is not available and no rescue read is placed "
                                     "(Section 13.3). The primary and footer are unreadable, so no copy validates.",
                                     formula=unit_eval("2H + P + 1 (the rescue's precondition, against total 7)", 2 * value + 4 + 1,
                                                       {"total": 7, "H": str(value), "P": 4}),
                                     note="SidecarMetadataUnavailable{0}."
                                          + (" 'As isolated-1' resolved to sup-17, the variant with the same parent "
                                             "that states the base and precondition." if sup == "sup-16" else "")),
                  "expect": {"recoverer": "SidecarMetadataUnavailable"}}

    # -- ISO-REBUILD variants on a4-minimal
    uuid = _image_cache("a4-minimal").tape_uuid
    block_size = _image_cache("a4-minimal").block_size
    a4_obj = _image_cache("a4-minimal").files[1].blocks
    parity_orig = _image_cache("a4-minimal").files[2].blocks[1:5]
    entries_orig = _image_cache("a4-minimal").files[2].info["sidecar"]

    def data_crc_entries(blocks):
        return [le64(crc64_xz(b)) for b in blocks]

    def parity_crc_entries(parity_shards, stripes, m):
        return [struct.pack("<IHHQ", s, j, 0, crc64_xz(parity_shards[j * stripes + s])) for s in range(stripes) for j in range(m)]

    def rebuild_variant(values, entries, parity, copy_blocks, extra_tail=0, object_blocks=None, object_row=None,
                        range_end=4, map_count=None):
        blocks, metadata_hash = build_sidecar_explicit(uuid, block_size, values, entries, parity, copy_blocks, extra_tail)
        count = len(blocks) if map_count is None else map_count
        entry = MapEntry(2, KIND_SIDECAR, count, None, 0, range_end, 0)
        directory = {1: 2, 2: 0, 3: 0, 4: range_end, 5: values["total"], 6: values["H"], 7: values.get("dir_P", values["P"]),
                     8: metadata_hash, 9: 6}
        return rebuild_a4(blocks, entry, directory, object_blocks, object_row), blocks

    def rebuild11():
        v = dict(base_values, m=0, P=0, inline=32, total=3, tail=1, footer_index=2, dir_P=4)
        entries = data_crc_entries(a4_obj)
        return rebuild_variant(v, entries, [], 1)

    def rebuild37():
        v = dict(base_values, H=2, tail=6, footer_index=8, total=9)
        entries = parity_crc_entries(parity_orig, 2, 2) + data_crc_entries(a4_obj)
        return rebuild_variant(v, entries, parity_orig, 2)

    def rebuild45():
        v = dict(base_values, total=8)
        entries = parity_crc_entries(parity_orig, 2, 2) + data_crc_entries(a4_obj)
        return rebuild_variant(v, entries, parity_orig, 1, extra_tail=1)

    def rebuild14():
        options = load_image_inputs("a4-minimal")["objects"][0]["options"]
        spec = FileSpec(path="payload.bin", file_id="00000000-0000-4000-8000-000000000003", data=bytes([53]) * (2 * block_size))
        builder_options = {k: options[k] for k in ("caller_object_id", "chunk_size", "metadata_preservation", "object_id",
                                                   "write_timestamp", "manifest_file_id")}
        builder_options["extensions"] = {}
        stored, layout = build_plaintext_with_manifest(builder_options, [spec])
        obj = [stored[i : i + block_size] for i in range(0, len(stored), block_size)]
        matrix = cauchy_matrix(2, 2)
        parity = [bytes(block_size)] * 4
        for s in range(2):
            for j, shard in enumerate(encode_parity([obj[s], obj[2 + s]], matrix)):
                parity[j * 2 + s] = shard
        v = dict(base_values, end=5, real=5, crc_count=5, inline=104)
        entries = parity_crc_entries(parity, 2, 2) + data_crc_entries(obj)
        manifest = layout["manifest"]
        row = {1: 1, 2: "plaintext", 3: len(obj), 4: options["object_id"].encode(), 10: manifest.first_chunk_lba,
               11: manifest.size_bytes, 12: manifest.chunk_count, 13: layout["manifest_sha256"]}
        return rebuild_variant(v, entries, parity, 1, object_blocks=obj, object_row=row, range_end=5)

    rebuilds = {
        "sup-11": ("neg-08", rebuild11, 1, "§9.2 m ≠ 0", ["sc_m", "validity_6_6", "pin_then"],
                   "m = 0 in both copies of a 3-block sidecar whose every other field is consistent with m = 0.",
                   {"copy": "SidecarParse", "recoverer": "SidecarMetadataUnavailable"}),
        "sup-14": ("neg-58", rebuild14, 1, "§9.2 real in 1..=S×k", ["sc_real_text", "sc_real", "epoch_at_most", "structural_invariants"],
                   "The copies break the range rule (5 > S×k = 4).",
                   {"copy": "SidecarParse", "recoverer": "SidecarMetadataUnavailable"}),
        "sup-37": ("neg-43", rebuild37, 2, "§9.2/§9.4 H", ["sc_H", "sc_recompute", "h_label", "p_defined"],
                   "H = 2 is recorded against a Section 9.4 recompute of 1, with every locator repaired for H = 2.",
                   {"copy": "SidecarParse", "recoverer": "SidecarMetadataUnavailable"}),
        "sup-45": ("neg-54", rebuild45, 1, "§9.2 total = 2H + P + 1", ["sc_total", "footer_probe"],
                   "total 8 against 2H + P + 1 = 7, in an 8-block file whose last block is zero.",
                   {"copy": "SidecarParse", "recoverer": "SidecarMetadataUnavailable"}),
    }
    for sup, (parent, builder, copy_blocks, named, rules, what, expect) in rebuilds.items():
        tail_block = {"sup-11": 1, "sup-37": 6}.get(sup, 5)
        note = {"sup-11": "Both copies fail the m rule. The directory entry's total (3) is not 2H + P + 1 = 7 with the "
                          "scheme's P = 4, so the entry is not available (Section 13.3), and a Recoverer reaches no valid "
                          "copy: SidecarMetadataUnavailable{0}.",
                "sup-14": "The copies fail. The text does not say whether replica validation ('Every structural and "
                          "ordinal-range invariant is validated') or the directory decoder must reject a range longer "
                          "than S × k (GAPS G-4). This implementation checks neither, so its Scanner accepts the "
                          "inventory and its Recoverer reports SidecarMetadataUnavailable{0}.",
                "sup-37": "Both copies fail the H rule. Under this implementation's reading of the locator formulas "
                          "(H = the recompute), three more rules fail as well (GAPS G-1). SidecarMetadataUnavailable{0} "
                          "for a Recoverer.",
                "sup-45": "Both copies fail. A Recoverer's footer-first step reads the zero last block (not a footer) and "
                          "falls back to the primary, which fails. The directory entry's total (8) is not 2H + P + 1 = 7, "
                          "so the entry is not available (Section 13.3): SidecarMetadataUnavailable{0}."}[sup]
        t[sup] = {"parent": parent, "rebuild": builder, "copy_blocks": copy_blocks, "tail_block": tail_block,
                  "unreadable": [], "roles": ["audit", "copies", "recover"], "named": named,
                  "decide": decision("rejected", "SidecarParse", rules + ["err_sidecar"], what, note=note),
                  "expect": expect}

    # -- 7b: ParityMap variants
    def apply12(ws, res):
        edit_int(ws, res, (3, 2), 0x20, 8, 0, 1, "footer sequence")
        repair_pm_crc(ws, res, [(3, 2)], "R-PM-FOOTER")
    t["sup-12"] = {"parent": "neg-36", "image": "a4-minimal", "apply": apply12, "unreadable": [], "roles": ["pm", "pm-audit"],
                   "named": "§10.1.4 payload/locator match",
                   "decide": decision("rejected", "ParityMapParse", ["pm_match", "pm_locator", "pm_agreement_all", "err_pm"],
                                      "The payload (sequence 0) does not match the footer's locator fields (1), and the "
                                      "header/footer agreement, which covers every field both carry (Section 10.1.4), fails "
                                      "on the sequence as well. Both rules are ParityMapParse."),
                   "expect": {"parity-map": "ParityMapParse"}}

    def apply35(ws, res):
        def transform(payload):
            entries = payload[4][5]
            res.checks.append({"where": "two-epoch tape file 4 payload", "offset": "directory key 5", "field": "entries",
                               "expected_from": [2, 3], "found": [e[1] for e in entries],
                               "matches": [e[1] for e in entries] == [2, 3], "to": [3, 2]})
            entries.reverse()
        repair_pm_payload(ws, res, 4, transform)
    t["sup-35"] = {"parent": "neg-37", "image": "two-epoch", "apply": apply35, "unreadable": [], "roles": ["pm", "pm-audit"],
                   "named": "§10.1.5 ascending",
                   "decide": decision("rejected", None, ["dir_ascending", "dir_partition", "dir_epochs", "pm_category"],
                                      "The ascending rule fails. The partition rule, stated 'taken in ascending "
                                      "tape_file_number order', holds. Whether the epoch-id rule also fails depends on the "
                                      "order it is read in, which the text does not state (GAPS G-6). Both copies fail, so "
                                      "Section 15 leaves the name open as a category.",
                                      error_set=["DirectoryInvalid", "ParityMapParse"]),
                   "expect": {"parity-map": "DirectoryInvalid"}}

    # -- 7b/7c: terminal profiles
    def multi_rows():
        return [object_row_from_json(r, i) for i, r in enumerate(multi_inputs()["object_rows"])]

    def ws15():
        rows = [{1: 1, 2: "plaintext", 3: 1, 4: b"huge-object-profile-row-1", 10: 0, 11: 32, 12: 1, 13: b"\x31" * 32},
                {1: 2, 2: "plaintext", 3: (1 << 64) - 1, 4: b"huge-object-profile-row-2", 10: 0, 11: 32, 12: 1, 13: b"\x31" * 32}]
        structural = [MapEntry(0, KIND_BOOTSTRAP, 1), MapEntry(1, KIND_OBJECT, 1, 0), MapEntry(2, KIND_OBJECT, (1 << 64) - 1, 1)]
        return terminal_workspace(structural, rows, [1, 1, 1], "huge-object-256k", total=0, watermark=0)

    def ws_unprotected(watermark):
        structural = [MapEntry(0, KIND_BOOTSTRAP, 1), MapEntry(1, KIND_OBJECT, 2, 0), MapEntry(2, KIND_SIDECAR, 5, None, 0, 2, 0),
                      MapEntry(3, KIND_OBJECT, 3, 2), MapEntry(4, KIND_PARITY_MAP, 3)]
        return terminal_workspace(structural, multi_rows(), [1, 2, 5, 3, 3], "multi-unprotected-256k", total=5, watermark=watermark)

    def ws24():
        structural = structural_from_json(multi_inputs()["structural_entries"])
        return terminal_workspace(structural, multi_rows(), [e.block_count for e in structural], "multi-256k (file numbers 7..11)",
                                  total=5, watermark=5, start_tape_file=7)

    terminal_cases = {
        "sup-15": ("neg-03", ws15, None, "T overflow", ["digest_scope", "compare_exact", "scope_in_list", "positions_fit",
                                                         "replica_parse_positions", "err_replica"],
                   "T = 1 + (2^64 − 1) is 2^64 exactly, so the recorded T (0) cannot equal it (Section 7.4, compared "
                   "exactly). Tape file 2's records and trailing filemark also reach positions that do not fit in u64, "
                   "which Section 7.2 now makes a rule of the map, so under the revised text the variant breaks two rules; "
                   "both are TerminalIndexReplicaParse (Section 15).",
                   "All three replicas carry it, so the Scanner reports BotStructuralRecoveryRequired.",
                   {"selection": "BotStructuralRecoveryRequired"},
                   "The profile's unspecified details are mine: the tape UUID, edition, diagnostics and extent of "
                   "multi-256k, the Object ids 'huge-object-profile-row-1/2', manifest_sha256 = 0x31 x 32, and A "
                   "placed at LBA 6 over one placeholder block per prefix file (no rule compares the rows with the "
                   "physical prefix)."),
        "sup-21": ("neg-47", lambda: ws_unprotected(2), None, "§10.6 W = T", ["w_equals_t", "err_replica"],
                   "Sidecars are present, and W (2) ≠ T (5).",
                   "All three replicas carry it: BotStructuralRecoveryRequired at the tape level.",
                   {"selection": "BotStructuralRecoveryRequired"}, ""),
        "sup-32": ("neg-47", lambda: ws_unprotected(5), None, "§7.4 W cross-check", ["digest_scope", "scope_in_list", "err_replica"],
                   "The recorded W (5) differs from its recompute (2). Section 15's TerminalIndexReplicaParse cites "
                   "Section 10.6, whose list includes 'scope', so the failure has that name.",
                   "All three replicas carry it: BotStructuralRecoveryRequired.", {"selection": "BotStructuralRecoveryRequired"},
                   "'As isolated-3' resolved to sup-21, the variant that states the multi-unprotected-256k profile."),
        "sup-24": ("neg-47", ws24, None, "§10.6 covered = rows = A's tape file", ["covered_equal", "footer_observations", "err_replica"],
                   "covered 6 = rows 6, but A's planned tape-file number is 7.",
                   "All three replicas carry it: BotStructuralRecoveryRequired.", {"selection": "BotStructuralRecoveryRequired"}, ""),
    }
    for sup, (parent, builder, _, named, rules, what, note, expect, resolution_note) in terminal_cases.items():
        t[sup] = {"parent": parent, "terminal": builder, "roles": ["terminal"], "named": named,
                  "decide": decision("rejected", "TerminalIndexReplicaParse", rules, what, note=note), "expect": expect,
                  "resolution_note": resolution_note}

    def apply34(ws, res):
        edit_slot(ws, res, 0, 384, 256, set_element(10, 0, (1 << 64) - 1), "Object-row slot 0 key 10")
        repair_tr_payload(ws, res, 0)
    t["sup-34"] = {"parent": "neg-15", "profile": "multi-256k", "apply": apply34, "roles": ["terminal"],
                   "named": "§10.3 manifest range fit",
                   "decide": decision("rejected", "TerminalIndexReplicaParse", ["row_manifest", "terminal_u64", "checked", "err_replica"],
                                      "first + count = 2^64 exceeds stored_block_count (2) whatever the evaluation; checked "
                                      "evaluation also rejects the overflow.",
                                      formula=unit_eval("manifest_first_chunk_lba + manifest_chunk_count", (1 << 64) - 1 + 1,
                                                        {"key 10": "2^64 - 1", "key 12": 1}),
                                      note="Replica A is ineligible; B and C supply the inventory (degraded)."),
                   "expect": {"replica A": "TerminalIndexReplicaParse"}}

    def apply41(ws, res):
        for key in ((0, 0), (0, 2)):
            edit_int(ws, res, key, 0x050, 8, 0, 1, "total data ordinals")
        repair_tr_common(ws, res, 0)
    t["sup-41"] = {"parent": "neg-47", "profile": "minimal-256k", "apply": apply41, "roles": ["terminal"],
                   "named": "§7.4 T cross-check",
                   "decide": decision("rejected", "TerminalIndexReplicaParse", ["digest_scope", "scope_in_list", "err_replica"],
                                      "Only the Section 7.4 T cross-check fails. Section 15's TerminalIndexReplicaParse cites "
                                      "Section 10.6, whose list includes 'scope', so the failure has that name (GAPS G-7).",
                                      note="Replica A is ineligible; B and C supply the inventory (degraded)."),
                   "expect": {"replica A": "TerminalIndexReplicaParse"}}

    # -- the Resumer
    def resume39():
        image = _image_cache("unfinalized-open")
        prefix = [{"block_count": e.block_count, "epoch_id": e.epoch_id,
                   "first_parity_data_ordinal": e.first_parity_data_ordinal, "kind": KIND_NAMES[e.kind],
                   "protected_ordinal_end_exclusive": e.protected_ordinal_end_exclusive,
                   "protected_ordinal_start": e.protected_ordinal_start, "tape_file_number": e.tape_file_number}
                  for e in image.prefix_entries]
        prefix[2]["block_count"] = (1 << 64) - 1
        return {"image": "unfinalized-open", "W": 4, "T": 6, "committed_prefix": prefix,
                "append_object": load_image_inputs("unfinalized-open")["objects"][1]}
    t["sup-39"] = {"parent": "neg-56", "resume": resume39, "named": "Σ(block_count + 1) (checked)",
                   "decide": decision("rejected", "ResumeAppend", ["append_point", "resume_step4", "resume_validated_records", "checked", "err_resume"],
                                      "T and W are unaffected and step 2 holds. The append point overflows, so the records do "
                                      "not determine one committed prefix and its append point, which Section 3.4 requires "
                                      "before the Resumer positions or writes: ResumeAppend.",
                                      formula=unit_eval("append point = Σ(block_count + 1)", 2 + 5 + (1 << 64), {"block_counts": [1, 4, "2^64 - 1", 2]}),
                                      note="Before any write. Whether before any tape read depends on when a Resumer "
                                           "computes the append point: this one does so in step 1 and reads nothing "
                                           "(GAPS E-2)."),
                   "expect": {"resume": "ResumeAppend"}}
    return t


def run_supplement_entry(sup: str, spec: dict[str, Any], variant: dict[str, Any]) -> dict[str, Any]:
    out: dict[str, Any] = {"id": sup, "parent": variant.get("parent_blind_id"), "group": variant.get("group"),
                           "target": variant.get("target"), "violates_only": variant.get("violates_only"),
                           "apply": None, "isolation": None, "decision": spec["decide"], "implementation": [],
                           "self_check": None}
    if "unit" in spec:
        out["apply"] = {"resolved": True, "vector": "unit", "inputs": variant.get("mutation", {}).get("inputs"),
                        "checks": [], "repairs": [], "mutated_blocks": [], "notes": []}
        out["isolation"] = {"claim": "only the named computation", "disputed_by_implementation": False,
                            "disputes": [], "evaluation": spec["unit"]}
        out["self_check"] = {"agrees": True, "detail": "unit evaluation with checked u64 arithmetic: "
                                                       + ("overflows" if spec["unit"]["overflows"] else "fits")}
        return out
    if "resume" in spec:
        resume_input = spec["resume"]()
        decision_out, _ = resume_case(resume_input, _image_cache(resume_input["image"]), {"case_id": sup})
        d = decision_out["decision"]
        out["apply"] = {"resolved": True, "vector": "injected commit record", "checks": [], "repairs": [],
                        "mutated_blocks": [], "notes": [f"derived W {decision_out['prefix']['derived_W']}, "
                                                        f"T {decision_out['prefix']['derived_T']}"]}
        out["isolation"] = {"claim": variant.get("every_other_rule_holds"), "disputed_by_implementation": False,
                            "disputes": [], "step2": decision_out["step2"]}
        out["implementation"].append({"level": "Resumer", "result": d["result"], "error": d["error"],
                                      "refused_at": d["refused_at"], "records_read": d["records_read"],
                                      "violations": decision_out["step2"]["violations"]})
        out["self_check"] = {"agrees": d["error"] == spec["expect"]["resume"], "detail": f"{d['result']} {d['error']}"}
        return out
    res = Resolution()
    if "rebuild" in spec:
        image, _ = spec["rebuild"]()
        _IMAGES[f"{sup}-rebuilt"] = image
        ws = Workspace(image=image)
        for key in [(2, i) for i in range(len(image.files[2].blocks))]:
            ws.block(key)
        res.repairs.append("ISO-REBUILD: sidecar, ParityMap and terminal suffix regenerated")
        rows = image_rows(image)
        res.notes.append("rebuilt tape files: " + ", ".join(f"{r['tape_file']}@{r['start_record']}×{r['data_records']}"
                                                            for r in rows[:-1]) + f"; EOD {rows[-1]['eod_record']}")
        res.notes.append({"rebuilt_tape_files": rows})
        tape_check = role_terminal_image(image)
        res.notes.append(f"rebuilt terminal suffix: {tape_check}")
    elif "terminal" in spec:
        ws = spec["terminal"]()
        res.repairs.append("the profile generated consistently: payload, canonical-map, edition, layout and descriptor "
                           "digests, CRCs and header-record hashes in all five components")
        res.notes.append({"generated_components": [
            {"component": PROFILE_COMPONENT_FILES[i], "bytes": len(b"".join(c)),
             "sha256": hashlib.sha256(b"".join(c)).hexdigest()} for i, c in enumerate(ws.terminal.components)]})
        if spec.get("resolution_note"):
            res.notes.append(spec["resolution_note"])
    elif "profile" in spec:
        ws = Workspace(profile=spec["profile"])
        spec["apply"](ws, res)
    else:
        ws = Workspace(image=_image_cache(spec["image"]))
        spec["apply"](ws, res)
    unreadable = set()
    if ws.image is not None:
        unreadable = {lba(ws, tf, b) for tf, b in spec.get("unreadable", [])}
        if unreadable:
            res.notes.append("presented unreadable (medium error, no byte changed): LBA " + ", ".join(map(str, sorted(unreadable))))
    mutated = [{"where": ws.describe(key), "bytes": len(ws.blocks[key]), "sha256": hashlib.sha256(bytes(ws.blocks[key])).hexdigest()}
               for key in ws.touched]
    out["apply"] = {"resolved": res.resolved, "vector": "bytes", "checks": res.checks, "repairs": res.repairs,
                    "mutated_blocks": mutated, "notes": res.notes}
    if not res.resolved:
        out["decision"] = decision("unresolved", None, [], "", note="the description does not resolve against my bytes")
        return out
    roles = spec.get("roles", [])
    isolation: dict[str, Any] = {"claim": variant.get("every_other_rule_holds"), "disputed_by_implementation": False, "disputes": []}
    if "audit" in roles:
        copy_blocks = spec.get("copy_blocks", 1)
        tail_block = spec.get("tail_block", 5)
        audits = sidecar_audits(ws, 2, tail_block, copy_blocks)
        isolation.update(isolation_summary(audits, spec["named"]))
        footer_block = len(ws.image.files[2].blocks) - (2 if spec.get("tail_block") == 5 and sup == "sup-45" else 1)
        footer_readable = (2, footer_block) not in spec.get("unreadable", [])
        cross = cross_checks(ws, audits, footer_block, footer_readable)
        isolation["cross_structure_failing"] = cross
        if cross and sup != "sup-44":
            isolation["disputed_by_implementation"] = True
            isolation["disputes"].append("cross-structure: " + "; ".join(cross))
    if "pm-audit" in roles:
        pm_file = 4 if spec.get("image") == "two-epoch" else 3
        audit = audit_parity_map(ws, pm_file)
        isolation["parity_map_rules_failing"] = audit["common"]
        isolation["parity_map_readings"] = audit["readings"]
    for role in roles:
        if role == "copies":
            copy_blocks = spec.get("copy_blocks", 1)
            tail_block = spec.get("tail_block", 5)
            for label, first, kind in (("primary", 0, 1), ("tail", tail_block, 2)):
                blocks = [bytes(ws.block((2, first + i))) for i in range(copy_blocks)]
                try:
                    parse_sidecar_copy(blocks, ws.tape_uuid, ws.block_size, kind)
                    out["implementation"].append({"level": f"{label} header/index copy", "result": "accepted", "error": None, "reason": ""})
                except ReadFailure as failure:
                    out["implementation"].append({"level": f"{label} header/index copy", "result": "rejected",
                                                  "error": failure.error, "reason": failure.reason})
        elif role == "verifier":
            out["implementation"].append(role_verifier_copies(ws, 2))
        elif role == "recover":
            out["implementation"].append(role_scan_and_recover(ws, unreadable, [1, 0]))
        elif role == "footer":
            out["implementation"].append(role_sidecar_footer(ws, 2, 7))
        elif role == "pm":
            out["implementation"].append(role_parity_map(ws, 4 if spec.get("image") == "two-epoch" else 3))
        elif role == "terminal":
            result = role_terminal(ws)
            out["implementation"].append(result)
            a = result.get("replicas", {}).get("A", {})
            isolation["replica_A_rules_failing"] = a.get("reason")
            reason = (a.get("reason") or "")
            body = reason.split(": ", 2)[-1] if reason.startswith("payload: ") else reason.split(": ", 1)[-1]
            if not a.get("valid") and body.count("; ") >= 1:
                isolation["disputed_by_implementation"] = True
                isolation["disputes"].append("replica A: this implementation finds " + str(body.count("; ") + 1)
                                             + " failing rules: " + body)
    if "pm-audit" in roles and isolation["parity_map_readings"].get("epoch ids in array order"):
        isolation["disputed_by_implementation"] = True
        isolation["disputes"].append("this implementation's directory decoder checks the range chain and the epoch ids in "
                                     "array order, so it also fails those two rules; in tape-file order, and for the "
                                     "epoch ids as a set, they hold")
    if "pm-audit" in roles and isolation["parity_map_readings"].get("'reject disagreement' covers every header field") \
            and not isolation["parity_map_readings"].get("'reject disagreement' covers only the locator arithmetic"):
        isolation["disputed_by_implementation"] = True
        isolation["disputes"].append("Section 10.1.4 now states that the header/footer agreement covers every field both "
                                     "carry, so the header/footer comparison fails as well as the payload/footer match: "
                                     "the variant breaks two rules under the text")
    if "pm-audit" in roles and isolation.get("parity_map_rules_failing") and len(isolation["parity_map_rules_failing"]) > 1:
        isolation["disputed_by_implementation"] = True
        isolation["disputes"].append("ParityMap: " + "; ".join(isolation["parity_map_rules_failing"]))
    out["isolation"] = isolation
    out["self_check"] = self_check(spec["expect"], [r if "level" in r else r for r in out["implementation"]])
    return out


def role_terminal_image(image: ImageBuild) -> str:
    tape = DamagedTape(image.records(), set())
    layout, _ = discover_layout(tape, image.tape_uuid, image.block_size)
    checks = [check_replica(tape, layout, o, image.tape_uuid, image.block_size) for o in (1, 2, 3)]
    return "replicas " + ", ".join(f"{c.letter} {'valid' if c.fully_valid else 'invalid: ' + c.reason}" for c in checks)


def run_supplement(path: pathlib.Path, out_path: pathlib.Path) -> dict[str, Any]:
    source = load_json(path)
    table = supplement_table()
    entries = {}
    for variant in source["variants"]:
        sup = variant["id"]
        if sup not in table:
            entries[sup] = {"id": sup, "apply": {"resolved": False, "notes": ["not analysed"]},
                            "decision": decision("unresolved"), "isolation": None, "implementation": [], "self_check": None}
            continue
        entries[sup] = run_supplement_entry(sup, table[sup], variant)
    output = {"schema": "rem-parity-second-implementation-negative-supplement/1", "entries": entries,
              "table_entries_without_a_variant": sorted(set(table) - {v["id"] for v in source["variants"]})}
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(output, indent=2, ensure_ascii=False, default=_json_default) + "\n", encoding="utf-8")
    return output


def supplement_summary(entry: dict[str, Any]) -> str:
    text = negative_summary(entry)
    isolation = entry.get("isolation") or {}
    if isolation.get("disputed_by_implementation"):
        text += " [isolation disputed: " + " | ".join(isolation["disputes"]) + "]"
    return text


# ---------------------------------------------------------------------------
# Deciding one case.
# ---------------------------------------------------------------------------


def case_id_of(path: pathlib.Path) -> str:
    """A case's id: its directory's name for the repository layouts
    (cases/<id>/fault-map.json, resume/<id>/inputs.json), else its file's stem."""
    return path.parent.name if path.name in ("fault-map.json", "inputs.json") else path.stem


class FaultMapError(Exception):
    """A fault map this Reader cannot apply exactly: an unknown or missing key, or a check that fails."""


# Every key a damage case's fault map may carry, at every level. Any other key
# fails the run: an ignored key would decide a damaged tape as an intact one.
FAULT_MAP_KEYS = {"failed_data_addresses", "hints", "image", "observations", "record_edits",
                  "removed_filemark_after_tape_file", "unreadable_records"}
FAULT_MAP_REQUIRED = {"failed_data_addresses", "image", "removed_filemark_after_tape_file", "unreadable_records"}
HINT_KEYS = {"block_size", "scheme", "tape_uuid"}
SCHEME_HINT_KEYS = {"S", "k", "m"}
OBSERVATION_KEYS = {"hints", "id"}
RECORD_EDIT_KEYS = {"construction", "edits", "lba", "length", "original_length", "record_index", "sha256", "tape_file"}
BYTE_EDIT_KEYS = {"new_bytes", "offset", "old_bytes", "reason"}
UNREADABLE_KEYS = {"filemark", "lba", "record_index", "tape_file"}


def _exact_keys(value: Any, allowed: set[str], required: set[str], where: str) -> None:
    if not isinstance(value, dict):
        raise FaultMapError(f"{where}: expected an object, found {type(value).__name__}")
    unknown = sorted(set(value) - allowed)
    if unknown:
        raise FaultMapError(f"{where}: unknown key(s) {', '.join(map(repr, unknown))}; this Reader refuses keys it "
                            f"does not know")
    missing = sorted(required - set(value))
    if missing:
        raise FaultMapError(f"{where}: missing key(s) {', '.join(map(repr, missing))}")


def _check_hints(hints: Any, where: str) -> None:
    if hints is None:
        return
    _exact_keys(hints, HINT_KEYS, HINT_KEYS, where)
    if hints["scheme"] is not None:
        _exact_keys(hints["scheme"], SCHEME_HINT_KEYS, SCHEME_HINT_KEYS, where + ".scheme")


def validate_fault_map(case: Any, where: str) -> None:
    """Refuse a fault map with any key this Reader does not know, at any level."""
    _exact_keys(case, FAULT_MAP_KEYS, FAULT_MAP_REQUIRED, where)
    if ("hints" in case) == ("observations" in case):
        raise FaultMapError(f"{where}: exactly one of 'hints' and 'observations' must be present")
    if "hints" in case:
        _check_hints(case["hints"], where + ".hints")
    else:
        observations = case["observations"]
        if not isinstance(observations, list) or not observations:
            raise FaultMapError(f"{where}.observations: expected a non-empty list")
        seen = set()
        for index, observation in enumerate(observations):
            _exact_keys(observation, OBSERVATION_KEYS, OBSERVATION_KEYS, f"{where}.observations[{index}]")
            if not isinstance(observation["id"], str) or observation["id"] in seen:
                raise FaultMapError(f"{where}.observations[{index}]: the id must be a string, and unique")
            seen.add(observation["id"])
            _check_hints(observation["hints"], f"{where}.observations[{index}].hints")
    for index, item in enumerate(case["unreadable_records"]):
        _exact_keys(item, UNREADABLE_KEYS, UNREADABLE_KEYS, f"{where}.unreadable_records[{index}]")
    for index, item in enumerate(case.get("record_edits", [])):
        _exact_keys(item, RECORD_EDIT_KEYS, RECORD_EDIT_KEYS, f"{where}.record_edits[{index}]")
        for position, edit in enumerate(item["edits"]):
            _exact_keys(edit, BYTE_EDIT_KEYS, BYTE_EDIT_KEYS, f"{where}.record_edits[{index}].edits[{position}]")


def load_fault_map(path: pathlib.Path) -> dict[str, Any]:
    case = load_json(path)
    validate_fault_map(case, str(path))
    return case


def observations_of(case: Mapping[str, Any]) -> list[tuple[str | None, dict[str, Any]]]:
    """Each observation as a case of its own (id, case with its hints); a case without observations is one."""
    if "observations" not in case:
        return [(None, dict(case))]
    base = {key: value for key, value in case.items() if key != "observations"}
    return [(observation["id"], dict(base, hints=observation["hints"])) for observation in case["observations"]]


def _construction_matches(construction: str, length: int, original_length: int) -> bool:
    """The three constructions a record edit may state, each checked against its lengths."""
    if construction == "the original record's length":
        return length == original_length
    match = re.fullmatch(r"the first (\d+) bytes of the original record", construction)
    if match:
        return int(match.group(1)) == length < original_length
    match = re.fullmatch(r"the original record followed by (\d+) zero bytes", construction)
    if match:
        return original_length + int(match.group(1)) == length
    return False


def _recomputed_check(record: bytes, offset: int, new: bytes) -> str | None:
    """Whether an edit that says it recomputes a bootstrap CRC holds the CRC I compute (reported, not enforced)."""
    if offset == 0x30 and len(new) == 8 and len(record) >= 0x38:
        return "header CRC agrees with mine" if crc64_xz(record[0:0x30]) == int.from_bytes(new, "little") else \
            "header CRC differs from mine"
    if len(record) >= 0x38:
        payload_len = rd32(record, 0x2C)
        if offset == 0x38 + payload_len and len(new) == 8 and 0x38 + payload_len + 8 <= len(record):
            return "payload CRC agrees with mine" if crc64_xz(record[0x38:0x38 + payload_len]) == int.from_bytes(new, "little") \
                else "payload CRC differs from mine"
    return None


def apply_record_edits(image: ImageBuild, records: list[Any], case: Mapping[str, Any]) -> list[dict[str, Any]]:
    """Replace each named record exactly as the case states, checking every stated old byte and the SHA-256."""
    applied = []
    for index, item in enumerate(case.get("record_edits", [])):
        where = f"record_edits[{index}]"
        tape_file, record_index = item["tape_file"], item["record_index"]
        if not (0 <= tape_file < len(image.files)) or not (0 <= record_index < len(image.files[tape_file].blocks)):
            raise FaultMapError(f"{where}: tape file {tape_file} has no data record {record_index}")
        lba = image.file_start_lba(tape_file) + record_index
        if item["lba"] != lba:
            raise FaultMapError(f"{where}: stated LBA {item['lba']}, but tape file {tape_file} record {record_index} "
                                f"is at LBA {lba}")
        original = records[lba]
        if not isinstance(original, bytes) or len(original) != item["original_length"]:
            raise FaultMapError(f"{where}: stated original length {item['original_length']}, mine is "
                                f"{len(original) if isinstance(original, bytes) else 'a filemark'}")
        if not _construction_matches(item["construction"], item["length"], item["original_length"]):
            raise FaultMapError(f"{where}: construction {item['construction']!r} is not one this Reader knows, or "
                                f"does not match length {item['length']}")
        record = bytearray(original[: item["length"]] + bytes(max(0, item["length"] - len(original))))
        checks = []
        for position, edit in enumerate(item["edits"]):
            old, new, offset = bytes.fromhex(edit["old_bytes"]), bytes.fromhex(edit["new_bytes"]), edit["offset"]
            if len(old) != len(new) or offset < 0 or offset + len(old) > len(record):
                raise FaultMapError(f"{where}.edits[{position}]: old and new bytes must be equally long and fit")
            if bytes(record[offset : offset + len(old)]) != old:
                raise FaultMapError(f"{where}.edits[{position}] ({edit['reason']}): stated old bytes {old.hex()} at "
                                    f"offset {offset}, mine are {bytes(record[offset:offset + len(old)]).hex()}")
            record[offset : offset + len(new)] = new
            check = {"offset": offset, "bytes": len(new), "reason": edit["reason"], "old_bytes_agree": True}
            if "recomputed" in edit["reason"]:
                check["recomputation"] = _recomputed_check(bytes(record), offset, new)
            checks.append(check)
        digest = hashlib.sha256(bytes(record)).hexdigest()
        if digest != item["sha256"]:
            raise FaultMapError(f"{where}: the edited record's SHA-256 is {digest}, the case states {item['sha256']}")
        records[lba] = bytes(record)
        applied.append({"tape_file": tape_file, "record_index": record_index, "lba": lba, "length": item["length"],
                        "construction": item["construction"], "edits": checks, "sha256_agrees": True})
    return applied


def damaged_tape_for(image: ImageBuild, case: Mapping[str, Any]) -> tuple[DamagedTape, list[str]]:
    """Apply the case's faults to my undamaged build (the fault model is given, not derived)."""
    records = image.records()
    notes = []
    for applied in apply_record_edits(image, records, case):
        notes.append(f"record at LBA {applied['lba']} (tape file {applied['tape_file']} record "
                     f"{applied['record_index']}) replaced: {applied['construction']}, {applied['length']} bytes, "
                     f"{len(applied['edits'])} byte edit(s); every stated old byte and the SHA-256 agree")
    removed = case.get("removed_filemark_after_tape_file")
    removed_lba = None
    if removed is not None:
        removed_lba = image.file_start_lba(removed) + len(image.files[removed].blocks)
        if records[removed_lba] is not None:
            raise ValueError(f"no filemark after tape file {removed}")
        del records[removed_lba]
        notes.append(f"filemark at LBA {removed_lba} removed; later records move down by one")
    unreadable = set()
    for item in case.get("unreadable_records", []):
        lba = item["lba"]
        tape_file = image.files[item["tape_file"]]
        start = image.file_start_lba(item["tape_file"])
        expected_filemark = item["record_index"] == len(tape_file.blocks)
        if start + item["record_index"] != lba or bool(item["filemark"]) != expected_filemark:
            raise ValueError(f"case record {item} does not match the image layout")
        if removed_lba is not None:
            if lba == removed_lba:
                continue
            if lba > removed_lba:
                lba -= 1
        unreadable.add(lba)
    return DamagedTape(records, unreadable), notes


def empty_decision(image_name: str) -> dict[str, Any]:
    return {
        "image": image_name,
        "image_unreproduced": [],
        "discovery": {"result": None, "block_size": None, "used_hints": False, "error": None, "citations": []},
        "scanner": {"result": "not_run", "error": None, "acceptable_selections": [], "degraded": None,
                    "replicas": {"A": {"valid": None, "reason": "not examined"},
                                 "B": {"valid": None, "reason": "not examined"},
                                 "C": {"valid": None, "reason": "not examined"}},
                    "inventory_equals_true_prefix": None, "citations": []},
        "walk": {"result": "not_run", "error": None, "classes": {}, "object_identity": None,
                 "terminal_authority_recovered": None, "citations": []},
        "recoverer": {"result": "not_run", "addresses": []},
        "verifier": {"result": "not_run", "error": None,
                     "separations": {"A-B": "not_run", "B-C": "not_run"}, "citations": []},
        "undecided": [],
    }


class _Ambiguous:
    """A map value this Reader cannot decode, because its key occurs more than once."""


AMBIGUOUS = _Ambiguous()


def decode_cbor_lenient(data: bytes) -> Any:
    """Decode well-formed CBOR without the Section 5.3 encoding rules, for Section 8.4's 'decodable' values.

    The canonical-form rules (key order, shortest form) are not applied, so a
    value in a payload that breaks only them can still be decoded. A
    definite-length item of major types 0 to 5, or false, true or null, is
    decoded; anything else is not decodable. A key that occurs twice has no
    decodable value (AMBIGUOUS). Bytes after the first item are left unread.
    """
    def item(offset: int, depth: int) -> tuple[Any, int]:
        if depth > 16 or offset >= len(data):
            raise ValueError("truncated or too deep")
        initial = data[offset]
        major, info = initial >> 5, initial & 0x1F
        offset += 1
        if major == 7:
            simple = {20: False, 21: True, 22: None}
            if info in simple:
                return simple[info], offset
            raise ValueError("float or other simple value")
        if info < 24:
            argument = info
        elif info in (24, 25, 26, 27):
            width = 1 << (info - 24)
            if offset + width > len(data):
                raise ValueError("truncated argument")
            argument = int.from_bytes(data[offset : offset + width], "big")
            offset += width
        else:
            raise ValueError("indefinite length or reserved")
        if major == 0:
            return argument, offset
        if major == 1:
            return -1 - argument, offset
        if major in (2, 3):
            if offset + argument > len(data):
                raise ValueError("truncated string")
            raw = data[offset : offset + argument]
            return (raw if major == 2 else raw.decode("utf-8")), offset + argument
        if major == 4:
            values = []
            for _ in range(argument):
                value, offset = item(offset, depth + 1)
                values.append(value)
            return values, offset
        if major == 5:
            mapping: dict[Any, Any] = {}
            for _ in range(argument):
                key, offset = item(offset, depth + 1)
                value, offset = item(offset, depth + 1)
                if isinstance(key, (list, dict)):
                    raise ValueError("unhashable key")
                mapping[key] = AMBIGUOUS if key in mapping else value
            return mapping, offset
        raise ValueError("tag")
    try:
        return item(0, 0)[0]
    except (ValueError, UnicodeDecodeError):
        return None


def judge_supplied_bootstrap(tape: DamagedTape, hints: dict[str, Any]) -> tuple[str, str | None, str, str, dict | None, str | None]:
    """Section 8.4's two-stage judgement of the first record with supplied values.

    Returns (outcome, error, reason, quote key, bootstrap, named). The outcome
    is "use", "unreadable" (the Scanner continues on the supplied values) or
    "refused" (with the error the table names, and `named` the value the
    refusal names: the block size, the first failing header field, or the
    scheme).
    """
    size = hints["block_size"]
    try:
        value = tape.read(0)
    except MediumError as error:
        return "unreadable", None, f"medium error at LBA {error.args[0]}", "first_record_unreadable", None, None
    if not isinstance(value, bytes):
        return "unreadable", None, f"{value} where the record should be", "first_record_unreadable", None, None
    if len(value) != size:
        return ("refused", "BootstrapParse", f"the record is {len(value)} bytes, not the supplied block size {size}",
                "first_record_length", None, "block size")
    block = value
    if len(block) < 0x40 or block[0:8] != BOOTSTRAP_MAGIC_BYTES or crc64_xz(block[0:0x30]) != rd64(block, 0x30):
        return "unreadable", None, "the magic is missing or the header CRC fails", "first_record_magic", None, None
    major, _minor, flags = struct.unpack_from(">HHI", block, 0x08)
    no_parity = bool(flags & 1)
    for name, ok in (("format major", major == BOOTSTRAP_SCHEMA_MAJOR_VALUE),
                     ("tape UUID", block[0x10:0x20] == hints["tape_uuid"]),
                     ("block size", struct.unpack_from(">I", block, 0x20)[0] == size),
                     ("sequence", struct.unpack_from(">Q", block, 0x24)[0] == 0),
                     ("no-parity flag", no_parity == (hints["scheme"] is None))):
        if not ok:
            return ("refused", "BootstrapParse", f"the header's {name} is impossible or disagrees with the supplied values",
                    "first_record_fields", None, name)
    payload_len = rd32(block, 0x2C)
    if 0x38 + payload_len + 8 > len(block) or crc64_xz(block[0x38 : 0x38 + payload_len]) != rd64(block, 0x38 + payload_len):
        return "unreadable", None, "the payload CRC fails or the payload runs past the block", "first_record_payload", None, None
    try:
        boot = parse_bootstrap(block, size)
    except ReadFailure as failure:
        # "a value that can still be decoded": decoded without Section 5.3's
        # encoding rules, which the payload may be what breaks.
        values = decode_cbor_lenient(block[0x38 : 0x38 + payload_len])
        if isinstance(values, dict):
            if values.get(5) is True and not no_parity:
                return ("refused", "DriveCompressionEnabled", "a decodable drive_compression is true on a parity tape",
                        "first_record_compression", None, None)
            record = values.get(1)
            if isinstance(record, dict) and hints["scheme"] is not None:
                decoded = (record.get(2), record.get(3), record.get(4))
                if all(is_uint(v) for v in decoded) and decoded != tuple(hints["scheme"]):
                    return ("refused", "BootstrapParse", "a decodable parity scheme disagrees with the supplied scheme",
                            "first_record_scheme", None, "scheme")
        return ("unreadable", None, f"the payload breaks a later rule of Section 8 ({failure.reason})",
                "first_record_later_rule", None, None)
    if boot["scheme"] != hints["scheme"]:
        return ("refused", "BootstrapParse", "the parity scheme disagrees with the supplied scheme",
                "first_record_scheme", None, "scheme")
    return "use", None, "the bootstrap agrees with the supplied values", "bootstrap_supplied_refused", boot, None


def discover_bootstrap(tape: DamagedTape, hints: dict[str, Any] | None, decision: dict[str, Any],
                       trace: dict[str, Any]) -> dict[str, Any] | None:
    discovery = decision["discovery"]
    discovery["citations"].append(cite("boot_first"))
    if hints:
        discovery["used_hints"] = True
        discovery["citations"].extend([cite("hint_size"), cite("first_record_stages")])
        outcome, error, reason, key, boot, named = judge_supplied_bootstrap(tape, hints)
        trace["bootstrap_attempts"] = [f"supplied block size {hints['block_size']}: {reason}"]
        if boot is None and key in ("first_record_scheme", "first_record_compression", "first_record_later_rule"):
            # Name the later rule the payload breaks, which the judgement's
            # reason leaves out when a decodable value decides.
            try:
                parse_bootstrap(tape.read_data(0), hints["block_size"])
            except (MediumError, ReadFailure) as failure:
                later = failure.reason if isinstance(failure, ReadFailure) else "medium error"
                if key != "first_record_later_rule":
                    trace["bootstrap_attempts"].append(f"the payload breaks a later rule of Section 8 ({later})")
                    discovery["citations"].append(cite("first_record_later_rule"))
                if "deterministic" in later:
                    discovery["citations"].append(cite("canonical_reject"))
        if outcome == "use":
            discovery.update(result="found", block_size=boot["block_size"])
            discovery["citations"].append(cite("accept_size"))
            return boot
        if outcome == "refused":
            discovery.update(result="error", error=error)
            if named is not None:
                discovery["refusal_names"] = named
            discovery["citations"].extend([cite(key), cite("bootstrap_supplied_refused" if error == "BootstrapParse"
                                                           else "compression_rejects")])
            return None
        trace["_bootstrap_unreadable"] = (key, reason)
        discovery.update(result="not_found", block_size=hints["block_size"])
        discovery["citations"].extend([cite(key), cite("unreadable_untrusted"), cite("hint_discovery"), cite("hint_uuid"),
                                       cite("identity_12_1")])
        return None
    discovery["citations"].append(cite("candidates"))
    attempts = []
    for size in DISCOVERY_CANDIDATES:
        try:
            block = tape.read_data(0)
        except MediumError as error:
            attempts.append(f"read size {size}: medium error at LBA {error.args[0]}")
            continue
        except ReadFailure as failure:
            attempts.append(f"read size {size}: {failure.reason}")
            continue
        if len(block) != size:
            attempts.append(f"read size {size}: the record is {len(block)} bytes, which only rules out this candidate")
            continue
        try:
            boot = parse_bootstrap(block, size)
        except ReadFailure as failure:
            attempts.append(f"read size {size}: {failure.error}: {failure.reason}")
            if failure.error == "DriveCompressionEnabled":
                trace["bootstrap_attempts"] = attempts
                discovery.update(result="error", error="DriveCompressionEnabled")
                discovery["citations"].append(cite("boot_discovery_name"))
                return None
            continue
        attempts.append(f"read size {size}: bootstrap parses")
        trace["bootstrap_attempts"] = attempts
        discovery.update(result="found", block_size=boot["block_size"])
        discovery["citations"].append(cite("accept_size"))
        return boot
    trace["bootstrap_attempts"] = attempts
    discovery.update(result="not_found", error="NoBootstrapFound")
    discovery["citations"].extend([cite("no_bootstrap"), cite("boot_discovery_name"), cite("hints_required")])
    return None


def load_parity_map_directory(tape: DamagedTape, entries: list[MapEntry], tape_uuid: bytes,
                              block_size: int) -> tuple[dict[int, dict] | None, str]:
    """Reach the final ParityMap through a validated replica's structural rows."""
    parity_entries = [entry for entry in entries if entry.kind == KIND_PARITY_MAP]
    if not parity_entries:
        return None, "no ParityMap row"
    entry = parity_entries[0]
    try:
        parity_map = parse_parity_map(tape, lba_of_file(entries, entry.tape_file_number), entry.block_count,
                                      tape_uuid, block_size)
    except ReadFailure as failure:
        return None, f"ParityMap does not validate ({failure.error}: {failure.reason})"
    if not parity_map["final"]:
        return None, "ParityMap not marked is_final_directory"
    return {item["tape_file_number"]: item for item in parity_map["entries"]}, "final ParityMap validates"


def verifier_prefix_findings(tape: DamagedTape, entries: list[MapEntry], scope: int, tape_uuid: bytes,
                             block_size: int, scheme: tuple[int, int, int], bootstrap: dict[str, Any] | None,
                             directory: dict[int, dict] | None,
                             bootstrap_unreadable: tuple[str, str] | None = None) -> list[dict[str, Any]]:
    """What a Verifier finds before replica A, each with the error a Reader reports for that component (Section 2.2).

    Every sidecar's primary copy, tail copy and footer are read (Section 9.1).
    Each finding says whether it is structure or metadata ("structure") or a
    data block or parity shard ("data"); whether a full verification checks
    the latter is open (Appendix D TT-2).
    """
    findings: list[dict[str, Any]] = []

    def add(component: str, finding: str, error: str | None, scope_kind: str = "structure") -> None:
        findings.append({"component": component, "finding": finding, "error": error, "checks": scope_kind})

    if bootstrap is None:
        # With supplied values the Scanner treats the bootstrap as unreadable
        # (Section 8.4). The Verifier reports why, with the error a Reader
        # reports for it: a medium error is TapeIo; damaged content is what a
        # parser given the block reports, BootstrapParse (Section 15).
        key, reason = bootstrap_unreadable or ("first_record_unreadable", "medium error")
        if key == "first_record_unreadable" and reason.startswith("medium error"):
            add("bootstrap", "unreadable (medium error)", "TapeIo")
        elif key == "first_record_unreadable":
            add("bootstrap", f"absent ({reason})", "NoBootstrapFound")
        else:
            add("bootstrap", f"{reason}, so with supplied values it is treated as unreadable", "BootstrapParse")
    elif not bootstrap["fill_zero"]:
        add("bootstrap", "trailing fill nonzero (a nonconformity; Section 8.1)", None)
    indexes: dict[int, dict] = {}
    _, m, stripes = scheme
    for entry in entries[:scope]:
        start = lba_of_file(entries, entry.tape_file_number)
        if entry.kind == KIND_SIDECAR:
            name = f"sidecar tape file {entry.tape_file_number}"
            copies: dict[str, dict] = {}
            footer = None
            try:
                footer = parse_sidecar_footer(tape.read_data(start + entry.block_count - 1), tape_uuid)
            except MediumError:
                add(name, "footer unreadable (medium error)", "TapeIo")
            except ReadFailure as failure:
                add(name, f"footer invalid ({failure.reason})", "SidecarParse")
            header_blocks = footer["H"] if footer is not None else None
            try:
                head = tape.read_data(start)
                guess = rd64(head, 0x60)
                if header_blocks is None and 1 <= guess <= entry.block_count:
                    header_blocks = guess
                copies["primary"] = parse_sidecar_copy(read_blocks(tape, start, header_blocks or 1), tape_uuid,
                                                       block_size, 1)
                header_blocks = copies["primary"]["H"]
            except MediumError:
                add(name, "primary copy unreadable (medium error)", "TapeIo")
            except ReadFailure as failure:
                add(name, f"primary copy invalid ({failure.reason})", "SidecarParse")
            if header_blocks is not None:
                # The footer locates the tail copy when it is valid (Section 9.6);
                # otherwise Section 13.3 step 2 places it at H + P.
                tail_first = footer["tail_start"] if footer is not None else header_blocks + stripes * m
                try:
                    copies["tail"] = parse_sidecar_copy(read_blocks(tape, start + tail_first, header_blocks), tape_uuid,
                                                        block_size, 2)
                except MediumError:
                    add(name, "tail copy unreadable (medium error)", "TapeIo")
                except ReadFailure as failure:
                    add(name, f"tail copy invalid ({failure.reason})", "SidecarParse")
            if len(copies) == 2 and copies["primary"]["hash"] != copies["tail"]["hash"]:
                add(name, "the two copies diverge", "SidecarParse")
            ctx = RecoveryContext(tape, tape_uuid, block_size, scheme, entries, scope, 0, directory, "verifier")
            index, note = acquire_index(ctx, entry, [])
            if index is None:
                add(name, f"no header/index copy validates ({note})", "SidecarMetadataUnavailable")
                continue
            k, m_index, s_index = scheme
            if (index["k"], index["m"], index["S"], index["block_size"]) != (k, m_index, s_index, block_size):
                add(name, "the acquired index disagrees with the bootstrap or supplied scheme", "SchemeMismatch")
            indexes[entry.tape_file_number] = index
            for block in range(entry.block_count):
                if index["H"] <= block < index["H"] + index["P"]:
                    shard = block - index["H"]
                    key = (shard % index["S"], shard // index["S"])
                    try:
                        if crc64_xz(tape.read_data(start + block)) != index["parity_crcs"][key]:
                            add(f"parity shard at LBA {start + block}", "CRC mismatch (an erasure, Section 13.4)", None,
                                "data")
                    except MediumError:
                        add(f"parity shard at LBA {start + block}", "unreadable (medium error)", "TapeIo", "data")
        elif entry.kind == KIND_PARITY_MAP:
            name = f"ParityMap tape file {entry.tape_file_number}"
            try:
                parity_map = parse_parity_map(tape, start, entry.block_count, tape_uuid, block_size)
                for copy_name, reason in parity_map["copy_failures"].items():
                    add(name, f"{copy_name} copy: {reason}", "TapeIo" if reason.startswith("medium error") else "ParityMapParse")
                if not parity_map["footer_readable"]:
                    add(name, "footer unreadable (medium error)", "TapeIo")
            except ReadFailure as failure:
                add(name, f"{failure.reason}", failure.error)
    for entry in entries[:scope]:
        if entry.kind != KIND_OBJECT:
            continue
        start = lba_of_file(entries, entry.tape_file_number)
        for block in range(entry.block_count):
            ordinal = entry.first_parity_data_ordinal + block
            covering = [e for e in entries[:scope] if e.kind == KIND_SIDECAR
                        and e.protected_ordinal_start <= ordinal < e.protected_ordinal_end_exclusive]
            component = f"Object tape file {entry.tape_file_number} block {block} (LBA {start + block})"
            try:
                data = tape.read_data(start + block)
            except MediumError:
                add(component, "unreadable (medium error)", "TapeIo", "data")
                continue
            if covering and covering[0].tape_file_number in indexes:
                index = indexes[covering[0].tape_file_number]
                if crc64_xz(data) != index["data_crcs"][ordinal - index["start"]]:
                    add(component, "CRC mismatch (an erasure, Section 13.4)", None, "data")
    return findings


def decide_case(case: Mapping[str, Any], image: ImageBuild, trace: dict[str, Any]) -> dict[str, Any]:
    decision = empty_decision(case["image"])
    tape, notes = damaged_tape_for(image, case)
    trace["fault_notes"] = notes
    hints = None
    if case.get("hints"):
        raw = case["hints"]
        supplied_uuid = image.tape_uuid if raw["tape_uuid"] == "the image's" else uuid_module.UUID(raw["tape_uuid"]).bytes
        scheme = raw["scheme"]
        hints = {"tape_uuid": supplied_uuid, "block_size": raw["block_size"],
                 "scheme": (scheme["k"], scheme["m"], scheme["S"]) if scheme else None}
    boot = discover_bootstrap(tape, hints, decision, trace)
    bootstrap_unreadable = trace.pop("_bootstrap_unreadable", None)
    if boot is not None:
        tape_uuid, block_size, scheme = boot["tape_uuid"], boot["block_size"], boot["scheme"]
    elif hints is not None and decision["discovery"]["result"] == "not_found":
        tape_uuid, block_size, scheme = hints["tape_uuid"], hints["block_size"], hints["scheme"]
    else:
        error = decision["discovery"]["error"]
        # NoBootstrapFound stops discovery for want of values; a refusal stops
        # it because the bootstrap contradicts the supplied values or records
        # compression on a parity tape (Section 8.4).
        error_cites = {"NoBootstrapFound": ["no_bootstrap"],
                       "BootstrapParse": ["bootstrap_supplied_refused", "err_boot_parse"],
                       "DriveCompressionEnabled": ["compression_rejects", "err_drive_compression"]}[error]
        scanner = decision["scanner"]
        scanner.update(result="error", error=error)
        scanner["citations"].extend([cite("boot_first")] + [cite(key) for key in error_cites])
        decision["walk"]["citations"].append(cite("hints_required"))
        verifier = decision["verifier"]
        verifier.update(result="error", error=error)
        verifier["citations"].extend([cite("verifier_role")] + [cite(key) for key in error_cites])
        if case.get("failed_data_addresses"):
            decision["recoverer"]["addresses"] = [
                {"address": list(a), "epoch": None, "result": "not_run", "error": None, "stripe": None,
                 "lost": None, "limit": None, "bytes_match": None, "citations": [cite("recoverer_inputs")]}
                for a in case["failed_data_addresses"]]
        return decision

    # ----- Scanner: terminal discovery and authoritative selection -----
    scanner = decision["scanner"]
    if boot is None:
        scanner["citations"].extend([cite("hint_discovery"), cite("hint_uuid")])
    scanner["citations"].extend([cite("locate_footer"), cite("validate_every")])
    layout, layout_note = discover_layout(tape, tape_uuid, block_size)
    trace["layout_source"] = layout_note
    checks: dict[str, ReplicaCheck] = {}
    if layout is not None:
        scanner["citations"].append(cite("locate_planned"))
        for ordinal in (1, 2, 3):
            check = check_replica(tape, layout, ordinal, tape_uuid, block_size)
            checks[check.letter] = check
            scanner["replicas"][check.letter] = {"valid": check.fully_valid, "reason": check.reason}
    else:
        for letter in "ABC":
            scanner["replicas"][letter] = {"valid": False, "reason": "no planned layout: " + layout_note}
        scanner["citations"].extend([cite("step1_spacing"), cite("footer_supplies"), cite("layout_before_eod")])
    fully_valid = [letter for letter, check in checks.items() if check.fully_valid]
    if any("medium error" in check.reason for check in checks.values()):
        scanner["citations"].append(cite("medium_error"))
    if any(not check.fully_valid and "medium error" not in check.reason for check in checks.values()):
        scanner["citations"].append(cite("device_agree"))
    selected: ReplicaCheck | None = None
    walk_needed = False
    if not fully_valid:
        scanner.update(result="BotStructuralRecoveryRequired", acceptable_selections=[])
        scanner["citations"].extend([cite("no_replica_walk"), cite("select_walk"), cite("bot_required")])
        walk_needed = True
    else:
        commons = {checks[letter].header["common"] for letter in fully_valid}
        scanner["citations"].extend([cite("consider_conflict"), cite("fully_valid")])
        if len(commons) > 1:
            scanner.update(result="error", error="TerminalIndexReplicaConflict", acceptable_selections=[])
            scanner["citations"].extend([cite("conflict_rule"), cite("conflict_error"), cite("err_conflict")])
        else:
            selected = checks[fully_valid[0]]
            scanner.update(result="Inventory", acceptable_selections=sorted(fully_valid))
            scanner["citations"].append(cite("survivor"))
            envelope_bad = [l for l, c in checks.items() if not c.envelope_valid]
            payload_only_bad = [l for l, c in checks.items() if c.envelope_valid and not c.payload_valid]
            if envelope_bad or not checks:
                scanner["degraded"] = True
                scanner["citations"].extend([cite("degraded_evidence"), cite("degraded_result")])
            elif payload_only_bad:
                # Section 12.6: the flag reports what the Scanner found. A
                # replica whose payload it did not read is not found invalid
                # for that reason, so the flag follows the Scanner's reads.
                scanner["degraded"] = "per reads"
                scanner["degraded_by_reads"] = {
                    f"replica {', '.join(payload_only_bad)}'s payload not read": False,
                    f"replica {', '.join(payload_only_bad)}'s payload read": True,
                }
                scanner["citations"].extend([cite("degraded_def"), cite("unread_payload"), cite("validate_every")])
            else:
                scanner["degraded"] = False
            scanner["inventory_equals_true_prefix"] = None  # filled after the decision

    # ----- BOT structural walk -----
    walk = decision["walk"]
    walk_map: dict[str, Any] | None = None
    walk_files: list[WalkedFile] = []
    if not walk_needed:
        walk["citations"].append(cite("no_replica_walk"))
    if walk_needed:
        walk["citations"].extend([cite("walk_offer"), cite("walk_file")])
        files, damage = bot_walk(tape, tape_uuid, block_size)
        walk_files = files
        walk_map = second_pass_and_map(files)
        trace["walk"] = [dataclasses.asdict(f) | {"parity_map": None} for f in files]
        trace["walk_damage"] = damage
        trace["walk_map"] = walk_map["reason"]
        walk["classes"] = {str(f.tape_file_number): (KIND_NAMES[f.kind] if f.kind is not None else "classification failed")
                           for f in files}
        failed = {str(f.tape_file_number): f.failed_classification for f in files if f.failed_classification}
        if failed:
            walk["failed_classifications"] = failed
        notes_text = " ".join(f.note for f in files)
        if "head unreadable" in notes_text:
            walk["citations"].append(cite("unreadable_head"))
        if "type established by its footer" in notes_text:
            walk["citations"].extend([cite("replica_footer_type"), cite("separation_footer_type"),
                                      cite("tail_probe_serves")])
        if walk_map["identified"]:
            walk["citations"].append(cite("second_pass"))
        if failed:
            walk["citations"].extend([cite("recognises"), cite("failed_reports"), cite("hard_error_scope"),
                                      cite("not_recognised_item7"), cite("item7")])
            if any("item 5" in note for note in failed.values()):
                walk["citations"].append(cite("item5_decides"))
            if any("item 6" in note for note in failed.values()):
                walk["citations"].append(cite("footer_probe"))
        if any(f.kind is None for f in files):
            walk["citations"].append(cite("walk_file"))
        if any(f.kind == KIND_OBJECT for f in files):
            walk["object_identity"] = "unknown"
            walk["citations"].append(cite("identity_unknown"))
        walk["terminal_authority_recovered"] = False
        walk["citations"].append(cite("authority_not_recovered"))
        if walk_map.get("error"):
            walk.update(result="error", error=walk_map["error"])
            walk["citations"].append(cite("reconstruct_error" if walk_map["error"] == "FilemarkMapReconstruct"
                                          else "walk_no_map"))
        else:
            walk["result"] = "run"
            if walk_map["validated"]:
                walk["citations"].extend([cite("walk_validated"), cite("walk_scope")])

    if selected is not None:
        trace["_inventory"] = (selected.entries, selected.object_rows)
    elif walk_map is not None and walk_map.get("validated"):
        trace["_walk_entries"] = walk_map["entries"]

    # ----- Recoverer -----
    recoverer = decision["recoverer"]
    context: RecoveryContext | None = None
    directory = None
    if selected is not None:
        directory, directory_note = load_parity_map_directory(tape, selected.entries, tape_uuid, block_size)
        trace["directory"] = directory_note
        context = RecoveryContext(tape, tape_uuid, block_size, scheme, selected.entries, selected.header["covered"],
                                  selected.header["watermark"], directory, "replica")
    elif walk_map is not None and walk_map.get("validated"):
        parity_tape_file, parity_map = walk_map["parity_map"]
        directory = {item["tape_file_number"]: item for item in parity_map["entries"]}
        trace["directory"] = "final ParityMap found by the walk"
        context = RecoveryContext(tape, tape_uuid, block_size, scheme, walk_map["entries"], parity_map["scope"],
                                  parity_map["watermark"], directory, "walk")
    if case.get("failed_data_addresses"):
        if context is None:
            recoverer["result"] = "not_run"
            recoverer["addresses"] = [
                {"address": list(a), "epoch": None, "result": "not_run", "error": None, "stripe": None,
                 "lost": None, "limit": None, "bytes_match": None, "citations": [cite("recoverer_inputs")]}
                for a in case["failed_data_addresses"]]
        else:
            recoverer["result"] = "run"
            for address in case["failed_data_addresses"]:
                outcome = recover_address(context, address)
                trace.setdefault("recoveries", []).append(
                    {key: value for key, value in outcome.items() if key not in ("citations", "_block")})
                recoverer["addresses"].append(outcome)

    # ----- Verifier -----
    verifier = decision["verifier"]
    verifier["citations"].extend([cite("verifier_role"), cite("fully_valid")])
    separations = {}
    edition_ids = set()
    if layout is not None:
        verifier["citations"].extend([cite("verifier_interior"), cite("verifier_separation")])
        for ordinal, name in ((1, "A-B"), (2, "B-C")):
            status, reason, edition_id = check_separation(tape, layout, ordinal, tape_uuid, block_size)
            separations[name] = (status, reason, edition_id)
        # The replicas' edition ID: that of the accepted edition when the
        # Scanner accepts one (an invalid replica "does not invalidate an
        # agreeing survivor", Section 8.5); with a conflict, the fully valid
        # replicas disagree; with no valid replica, the edition ID every
        # readable replica frame carries, when they agree.
        if selected is not None:
            edition_ids.add(selected.header["edition_id"])
        elif fully_valid:
            edition_ids.update(checks[letter].header["edition_id"] for letter in fully_valid)
        else:
            for ordinal in (1, 2, 3):
                _, _, _, _, start, records = layout[{1: 0, 2: 2, 3: 4}[ordinal]]
                for position, role in ((start, 1), (start + records - 1, 2)):
                    try:
                        frame = parse_replica_frame(tape.read_data(position), tape_uuid, block_size, role)
                        edition_ids.add(frame["edition_id"])
                    except (MediumError, ReadFailure):
                        pass
    for name, (status, reason, edition_id) in separations.items():
        if status == "valid":
            verifier["citations"].append(cite("separation_edition"))
            if len(edition_ids) == 1:
                if edition_id not in edition_ids:
                    status, reason = "invalid", "edition ID differs from the replicas'"
                verifier["separations"][name] = status
            else:
                verifier["separations"][name] = "undecided"
                decision["undecided"].append({
                    "aspect": f"verifier.separations.{name}",
                    "readings": [
                        "valid: its edition ID equals that of some of the replicas ("
                        + ", ".join(sorted(e.hex() for e in edition_ids)) + ")",
                        "invalid: the replicas carry different edition IDs, so no single value is 'the replicas'' "
                        "and the extent cannot equal it",
                    ],
                    "citations": [cite("separation_edition"), cite("conflict_rule")],
                })
        else:
            verifier["separations"][name] = status
        trace.setdefault("separations", {})[name] = reason
    if layout is None:
        # No planned layout: the Verifier has the walk's classification of
        # the separation extents, which keep their control type, damaged,
        # when their checks fail (Section 12.3 items 2 and 3).
        walked_separations = [f for f in walk_files if f.kind == KIND_SEPARATION]
        verifier["separations"] = {}
        for index, name in enumerate(("A-B", "B-C")):
            f = walked_separations[index] if index < len(walked_separations) else None
            damaged = f is not None and f.note.startswith("damaged")
            verifier["separations"][name] = "invalid" if damaged else "not_run"
            if f is not None:
                trace.setdefault("separations", {})[name] = "walk: " + f.note
        if any(status == "invalid" for status in verifier["separations"].values()):
            verifier["citations"].extend([cite("not_recognised_item7"), cite("verifier_separation")])
    separations_ok = all(verifier["separations"].get(n) == "valid" for n in ("A-B", "B-C"))
    eod_ok = layout is not None and tape.eod() == layout[4][4] + layout[4][5] + 1
    if scanner["result"] == "error" and scanner["error"] == "TerminalIndexReplicaConflict":
        verifier.update(result="error", error="TerminalIndexReplicaConflict")
        verifier["citations"].append(cite("degraded_result"))
    elif not fully_valid:
        verifier["result"] = "recovery_required"
        verifier["citations"].extend([cite("walk_evidence"), cite("err_bot")])
    elif len(fully_valid) < 3:
        verifier["result"] = "degraded"
        verifier["citations"].append(cite("degraded_result"))
    elif not separations_ok or not eod_ok:
        verifier["result"] = "undecided"
        verifier["citations"].append(cite("normal_finalized"))
        bad = [n for n in ("A-B", "B-C") if verifier["separations"].get(n) != "valid"]
        decision["undecided"].append({
            "aspect": "verifier",
            "readings": [
                f"degraded: all three replicas are valid but separation extent {', '.join(bad)} is not, and the "
                "Verifier reports the terminal suffix as degraded",
                f"error: the Verifier reports separation extent {', '.join(bad)} as a failure (a medium error is "
                "TapeIo, a format violation TerminalIndexSeparationParse) and no category of Section 12.6 applies",
            ],
            "citations": [cite("verifier_separation"), cite("degraded_result"), cite("io_distinct")],
        })
    else:
        verifier["result"] = "complete"
        verifier["citations"].append(cite("normal_finalized"))
    if context is not None:
        findings = verifier_prefix_findings(tape, context.entries, context.scope, tape_uuid, block_size, scheme,
                                            boot, directory, bootstrap_unreadable)
        trace["verifier_prefix_findings"] = findings
        if findings:
            # Section 2.2: the Verifier reports each finding before the
            # terminal suffix with the error a Reader reports for it.
            verifier["outside_terminal_suffix"] = findings
            verifier["citations"].append(cite("verifier_reports"))
            if any(f["component"] == "bootstrap" and f["error"] == "BootstrapParse" for f in findings):
                verifier["citations"].append(cite("boot_parser_name"))
            if any(f["component"].startswith("sidecar") for f in findings):
                verifier["citations"].append(cite("verifier_divergence"))
            data = [f for f in findings if f["checks"] == "data"]
            if data:
                decision["undecided"].append({
                    "aspect": "verifier.outside_terminal_suffix (data blocks and parity shards)",
                    "readings": [
                        "a full verification checks every data block and parity shard, and reports these findings: "
                        + "; ".join(f"{f['component']}: {f['finding']}" for f in data),
                        "a full verification checks only structure and metadata, and does not report them",
                    ],
                    "citations": [cite("verifier_role"), cite("full_verification_open")],
                })
    return decision


def reporting_only(decision: dict[str, Any], image: ImageBuild, trace: dict[str, Any]) -> None:
    """Compute bytes_match and inventory_equals_true_prefix after every decision is made."""
    for outcome in decision["recoverer"]["addresses"]:
        block = outcome.pop("_block", None)
        if outcome["result"] == "recovered" and block is not None:
            tape_file, block_index = outcome["address"]
            outcome["bytes_match"] = block == image.files[tape_file].blocks[block_index]
        for extra in ("index_acquisition", "peers"):
            outcome.pop(extra, None)
    scanner = decision["scanner"]
    inventory = trace.pop("_inventory", None)
    walked = trace.pop("_walk_entries", None)
    if inventory is not None:
        entries, rows = inventory
        scanner["inventory_equals_true_prefix"] = entries == image.prefix_entries and rows == image.object_rows
    elif walked is not None:
        scanner["inventory_equals_true_prefix"] = walked == image.prefix_entries
    else:
        scanner["inventory_equals_true_prefix"] = None


def run_decide(case_paths: list[pathlib.Path], out_path: pathlib.Path) -> dict[str, Any]:
    images: dict[str, ImageBuild] = {}
    decisions: dict[str, Any] = {}
    traces: dict[str, Any] = {}
    manifest_rows = read_tsv(FIXTURE_ROOT / "tape-images" / "MANIFEST.tsv")
    unreproduced_by_image: dict[str, list[str]] = {}
    # Every fault map is checked before any is decided, so an unknown key fails
    # the run before it writes anything.
    cases = {path: load_fault_map(path) for path in case_paths}
    for path in sorted(case_paths, key=lambda p: case_id_of(p)):
        case = cases[path]
        name = case["image"]
        if name not in images:
            images[name] = build_image(load_image_inputs(name), name)
            unreproduced_by_image[name] = [
                result.artifact.split(" tape file ")[1] if " tape file " in result.artifact else result.artifact
                for result in compare_image(images[name], manifest_rows) if not result.reproduced]
        image = images[name]
        per_observation: dict[str, Any] = {}
        per_observation_trace: dict[str, Any] = {}
        for observation_id, observed in observations_of(case):
            trace: dict[str, Any] = {}
            decision = decide_case(observed, image, trace)
            reporting_only(decision, image, trace)
            decision["image_unreproduced"] = [f"tape file {item}" for item in unreproduced_by_image[name]]
            per_observation[observation_id] = decision
            per_observation_trace[observation_id] = trace
        if "observations" not in case:
            decisions[case_id_of(path)] = per_observation[None]
            traces[case_id_of(path)] = per_observation_trace[None]
        else:
            # Each observation is a separate decision on the same damaged tape.
            decisions[case_id_of(path)] = {
                "image": name,
                "record_edits": apply_record_edits(image, image.records(), case),
                "observations": per_observation,
            }
            traces[case_id_of(path)] = {"observations": per_observation_trace}
    output = {"schema": "rem-parity-second-implementation-decisions/1", "cases": decisions}
    text = json.dumps(output, indent=2, sort_keys=False, ensure_ascii=False) + "\n"
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(text, encoding="utf-8")
    (out_path.with_name(out_path.stem + "-trace.json")).write_text(
        json.dumps(traces, indent=2, default=_json_default, ensure_ascii=False) + "\n", encoding="utf-8")
    return output


def _json_default(value: Any) -> Any:
    if isinstance(value, bytes):
        return value.hex()
    if isinstance(value, tuple):
        return list(value)
    return str(value)


# ---------------------------------------------------------------------------
# The negatives' mutated-block digests (Appendix D TT-2, first open item).
#
# The `negatives` and `negatives-supplement` commands already apply each
# mutation and its repairs to my own builds. This re-runs only that apply
# step, emits every block whose bytes the mutation and repairs change, and
# compares each with tape-images/negatives/MANIFEST.tsv. It writes a file of
# its own and leaves both earlier decision files untouched.
# ---------------------------------------------------------------------------

NEGATIVES_ROOT = FIXTURE_ROOT / "tape-images" / "negatives"
BLIND_INPUTS = OUTPUT_ROOT / "blind-inputs"


def _profile_first_tape_file(profile: str) -> int:
    """The tape-file number of replica A in a profile (component index 0)."""
    return load_json(FIXTURE_ROOT / profile / "inputs.json")["terminal_layout"]["start_tape_file"]


def _block_row(artifact: str, tape_file: int, block: int, data: bytes) -> dict[str, Any]:
    return {"artifact": artifact, "tape_file": tape_file, "block_within_file": block, "bytes": len(data),
            "sha256": hashlib.sha256(data).hexdigest()}


def _diff_image(base: ImageBuild, mutated: ImageBuild) -> list[dict[str, Any]]:
    """Blocks of a mutated image whose bytes differ from the base at the same (tape file, block),
    or that have no counterpart in the base."""
    rows = []
    for tape_file in mutated.files:
        number = tape_file.tape_file_number
        base_blocks = base.files[number].blocks if number < len(base.files) else []
        for index, block in enumerate(tape_file.blocks):
            if index >= len(base_blocks) or base_blocks[index] != block:
                rows.append(_block_row(f"tape-image {base.name}", number, index, block))
    return rows


def _diff_terminal(base: TerminalBuild, mutated_components: list[list[bytes]], profile: str) -> list[dict[str, Any]]:
    """Records of a mutated terminal suffix that differ from the base profile's, keyed by the
    profile's tape-file numbering (replica A = the profile's start tape file)."""
    first = _profile_first_tape_file(profile)
    rows = []
    for component, records in enumerate(mutated_components):
        base_records = base.components[component] if component < len(base.components) else []
        for index, record in enumerate(records):
            if index >= len(base_records) or base_records[index] != record:
                rows.append(_block_row(f"terminal profile {profile}", first + component, index, record))
    return rows


def _diff_workspace(ws: Workspace) -> list[dict[str, Any]]:
    rows = []
    for key in sorted(ws.blocks):
        data = bytes(ws.blocks[key])
        if data == ws.original(key):
            continue
        if ws.profile is not None:
            rows.append(_block_row(f"terminal profile {ws.profile}", _profile_first_tape_file(ws.profile) + key[0],
                                   key[1], data))
        else:
            rows.append(_block_row(f"tape-image {ws.image.name}", key[0], key[1], data))
    return rows


def _unchanged_block(artifact: str, tape_file: int, block: int, mutated: Any) -> bytes | None:
    """My block at a manifest key that my diff did not emit (for reporting only)."""
    if mutated is None:
        return None
    kind, value = mutated
    try:
        if kind == "image":
            return value.files[tape_file].blocks[block]
        if kind == "terminal":
            components, profile = value
            return components[tape_file - _profile_first_tape_file(profile)][block]
        if kind == "workspace":
            ws = value
            key = (tape_file - _profile_first_tape_file(ws.profile), block) if ws.profile is not None else (tape_file, block)
            return bytes(ws.blocks[key]) if key in ws.blocks else ws.original(key)
    except (IndexError, KeyError):
        return None
    return None


def _sup14_base_parity_candidate() -> ImageBuild:
    """A diagnostic alternative for sup-14, used only to locate differences.

    The variant says "Parity bytes are arbitrary (data_index 2 >= k has no
    generator column)": Section 6.2 defines the generator only for i in 0..k,
    so no parity is defined for an epoch of 5 ordinals at k = 2, S = 2. My
    build encodes the first S × k ordinals. This candidate keeps the base
    image's parity shards instead, a choice the variant equally permits. It
    never replaces my emitted bytes; a mismatch is located against it only
    when it reproduces the pinned digest.
    """
    base = _image_cache("a4-minimal")
    mine, _ = supplement_table()["sup-14"]["rebuild"]()
    objects = mine.files[1].blocks
    parity = base.files[2].blocks[1:5]
    values = dict(a4_sidecar_values(), end=5, real=5, crc_count=5, inline=104)
    entries = [struct.pack("<IHHQ", s, j, 0, crc64_xz(parity[j * 2 + s])) for s in range(2) for j in range(2)] + \
              [le64(crc64_xz(block)) for block in objects]
    blocks, metadata_hash = build_sidecar_explicit(base.tape_uuid, base.block_size, values, entries, parity, 1)
    return rebuild_a4(blocks, MapEntry(2, KIND_SIDECAR, len(blocks), None, 0, 5, 0),
                      {1: 2, 2: 0, 3: 0, 4: 5, 5: values["total"], 6: values["H"], 7: values["P"], 8: metadata_hash, 9: 6},
                      objects, mine.object_rows[0], "a4-minimal (sup-14, base parity)")


DIAGNOSTIC_CANDIDATES = {"sup-14": _sup14_base_parity_candidate}


def _field_at(tape_file: int, block: int, offset: int, image: ImageBuild) -> str:
    """Name the field at an offset of a block of an a4-minimal-shaped image (for locating only)."""
    kind = image.files[tape_file].kind
    count = len(image.files[tape_file].blocks)
    if kind == KIND_SIDECAR:
        if block == count - 1:
            return "sidecar footer: " + (describe_table(SIDECAR_FOOTER_FIELDS, offset) or "zero fill")
        if offset < 0xC8:
            return "sidecar header copy: " + (describe_table(SIDECAR_HEADER_FIELDS, offset) or "header")
        return "sidecar index entries or fill"
    if kind == KIND_PARITY_MAP:
        return "ParityMap: " + (describe_table(PARITY_MAP_FIELDS, offset) or "payload or fill")
    return f"{KIND_NAMES.get(kind, 'tape file')} byte {offset}"


def negative_entry_blocks(spec: dict[str, Any]) -> tuple[str, list[dict[str, Any]], Any, list[dict[str, Any]]]:
    """Apply one negative or supplement entry; return (vector, changed blocks, the mutated artifact, failed checks)."""
    if spec.get("none") or "resume" in spec or "unit" in spec:
        return ("none" if spec.get("none") else "resume" if "resume" in spec else "unit"), [], None, []
    if "rebuild" in spec:
        image, _ = spec["rebuild"]()
        return "bytes", _diff_image(_image_cache("a4-minimal"), image), ("image", image), []
    if "terminal" in spec:
        ws = spec["terminal"]()
        base = build_terminal_suffix(terminal_inputs_from_profile(multi_inputs()))
        return "bytes", _diff_terminal(base, ws.terminal.components, "multi-256k"), \
            ("terminal", (ws.terminal.components, "multi-256k")), []
    ws = Workspace(image=_image_cache(spec["image"])) if "image" in spec else Workspace(profile=spec["profile"])
    res = Resolution()
    spec["apply"](ws, res)
    failed = [check for check in res.checks if not check["matches"]]
    return "bytes", _diff_workspace(ws), ("workspace", ws), failed


def run_negative_blocks(negatives_path: pathlib.Path, supplement_path: pathlib.Path, manifest_path: pathlib.Path,
                        out_path: pathlib.Path, negatives_e1_path: pathlib.Path | None = None) -> dict[str, Any]:
    negatives_mapping = load_json(OUTPUT_ROOT / "blind-negatives-mapping.json")["mapping"]
    supplement_mapping = load_json(OUTPUT_ROOT / "blind-supplement-mapping.json")["mapping"]
    entries: dict[str, Any] = {}
    table = negatives_table()
    for case in load_json(negatives_path)["cases"]:
        for variant in [v["variant"] for v in case.get("variants", [])] or [None]:
            entry_id = case["id"] + (f"/{variant}" if variant else "")
            real = negatives_mapping[case["id"]] + (f"/{variant}" if variant else "")
            entries[entry_id] = {"real_id": real, "spec": table.get(entry_id)}
    supplement = supplement_table()
    for variant in load_json(supplement_path)["variants"]:
        entries[variant["id"]] = {"real_id": supplement_mapping[variant["id"]], "spec": supplement.get(variant["id"])}
    if negatives_e1_path is not None:
        # The E1 negative's id is already the repository's case name.
        e1_table = e1_negatives_table()
        for case in load_json(negatives_e1_path)["cases"]:
            entries[case["id"]] = {"real_id": case["id"], "spec": e1_table.get(case["id"])}
    mine: dict[tuple, dict[str, Any]] = {}
    mutated_by_case: dict[str, Any] = {}
    per_entry: dict[str, Any] = {}
    for entry_id, item in entries.items():
        spec = item["spec"]
        if spec is None:
            per_entry[entry_id] = {"real_id": item["real_id"], "vector": "not in this implementation's tables",
                                   "blocks": 0, "failed_from_checks": []}
            continue
        vector, rows, mutated, failed = negative_entry_blocks(spec)
        per_entry[entry_id] = {"real_id": item["real_id"], "vector": vector, "blocks": len(rows),
                               "failed_from_checks": failed}
        mutated_by_case[item["real_id"]] = mutated
        for row in rows:
            mine[(item["real_id"], row["artifact"], row["tape_file"], row["block_within_file"])] = dict(row, entry=entry_id)
    manifest: dict[tuple, dict[str, str]] = {}
    for row in read_tsv(manifest_path):
        manifest[(row["case"], row["artifact"], int(row["tape_file"]), int(row["block_within_file"]))] = row
    candidates = {supplement_mapping.get(entry_id, entry_id): builder() for entry_id, builder in DIAGNOSTIC_CANDIDATES.items()
                  if entry_id in entries}
    rows_out = []
    counts = {"matched": 0, "mismatched": 0, "missing_from_mine": 0, "missing_from_manifest": 0}
    for key in sorted(set(mine) | set(manifest), key=lambda k: (k[0], k[1], k[2], k[3])):
        case, artifact, tape_file, block = key
        out: dict[str, Any] = {"case": case, "artifact": artifact, "tape_file": tape_file, "block_within_file": block}
        mine_row, pinned = mine.get(key), manifest.get(key)
        if mine_row is not None:
            out["entry"] = mine_row["entry"]
        if mine_row is not None and pinned is not None:
            same = str(mine_row["bytes"]) == pinned["bytes"] and mine_row["sha256"] == pinned["sha256"]
            out.update(result="matched" if same else "mismatched", bytes=mine_row["bytes"], sha256=mine_row["sha256"],
                       pinned_bytes=int(pinned["bytes"]), pinned_sha256=pinned["sha256"])
            if not same:
                out["first_differing_byte"] = None
                out["detail"] = ("the manifest pins only the size and SHA-256 of the block, so the first differing "
                                 "byte cannot be located against it (compare GAPS C-1)")
                candidate = candidates.get(case)
                mutated = mutated_by_case.get(case)
                if candidate is not None and mutated is not None and mutated[0] == "image":
                    theirs = candidate.files[tape_file].blocks[block]
                    confirmed = hashlib.sha256(theirs).hexdigest() == pinned["sha256"]
                    out["diagnostic_candidate"] = {"name": candidate.name, "reproduces_pinned_digest": confirmed}
                    if confirmed:
                        ours = mutated[1].files[tape_file].blocks[block]
                        index = first_difference(ours, theirs)
                        out["first_differing_byte"] = index
                        out["field"] = _field_at(tape_file, block, index, mutated[1])
                        out["detail"] = ("located against the diagnostic candidate, which reproduces the pinned digest; "
                                         "my emitted bytes are unchanged")
        elif pinned is not None:
            out.update(result="missing_from_mine", pinned_bytes=int(pinned["bytes"]), pinned_sha256=pinned["sha256"])
            data = _unchanged_block(artifact, tape_file, block, mutated_by_case.get(case))
            if data is None:
                out["detail"] = "my build of this case has no block at this key"
            else:
                digest_here = hashlib.sha256(data).hexdigest()
                out["detail"] = ("my mutation leaves this block's bytes equal to the base's at the same key, so it is "
                                 "not emitted as changed; my block here has "
                                 + ("the pinned size and SHA-256" if (len(data), digest_here) == (int(pinned["bytes"]), pinned["sha256"])
                                    else f"{len(data)} bytes and SHA-256 {digest_here}"))
                out["unchanged_block_matches_pinned"] = (len(data), digest_here) == (int(pinned["bytes"]), pinned["sha256"])
        else:
            out.update(result="missing_from_manifest", bytes=mine_row["bytes"], sha256=mine_row["sha256"])
            pinned_cases = {k[0] for k in manifest}
            if case not in pinned_cases:
                out["detail"] = "the manifest pins no block for this case"
            elif case in candidates and artifact.startswith("tape-image "):
                candidate_block = candidates[case].files[tape_file].blocks[block]
                base_block = _image_cache(artifact.split(" ", 1)[1]).files[tape_file].blocks[block]
                out["detail"] = ("my block differs from the base's here; the diagnostic candidate's block "
                                 + ("equals the base's, so a manifest of changed blocks has no row for it"
                                    if candidate_block == base_block else "also differs from the base's"))
            else:
                out["detail"] = "the manifest has no row for this block of a case it pins"
        counts[out["result"]] += 1
        rows_out.append(out)
    output = {"schema": "rem-parity-second-implementation-negative-blocks/1",
              "sources": dict({"negatives": negatives_path.name, "supplement": supplement_path.name},
                              **({"negatives_e1": negatives_e1_path.name} if negatives_e1_path is not None else {}),
                              manifest="tape-images/negatives/MANIFEST.tsv"),
              "counts": dict(counts, manifest_rows=len(manifest), emitted_blocks=len(mine)),
              "entries": per_entry, "rows": rows_out}
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(output, indent=2, ensure_ascii=False, default=_json_default) + "\n", encoding="utf-8")
    return output


# ---------------------------------------------------------------------------
# The terminal-index mutations and survivor sets (Appendix D TT-2), decided
# blind. Each description is applied byte by byte to my own profile build,
# the Reader runs on the resulting record stream, and the decision comes from
# the text; the Reader's result is recorded beside it as a self-check.
# ---------------------------------------------------------------------------

QUOTES.update({
    "frame_fields": ("10.6", "Each frame's record length, CRC, schema version, role, flags, tape UUID, partition, block size, compression mode, ordinals and counts, reserved bytes, and padding validate against Section 10.4, and the block size is one of the terminal record sizes of Section 8.3."),
    "digests_recompute": ("10.6", "The edition digest and the local descriptor digest recompute to the recorded values."),
    "backward_delta": ("10.6", "The footer's backward delta names the same header start as the discovered planned layout; the header at that planned coordinate has a complete-record SHA-256 equal to the footer's recorded header hash, and header and footer carry the same common descriptor."),
    "record_whole": ("10.4", "The header and footer each occupy one complete tape record."),
    "frame_zero": ("10.4", "Their meaningful frame is the first `0x400` bytes; bytes from `0x400` through the end of the record are zero."),
    "record_sizes": ("8.3", "Terminal replicas and separation extents are written and read only at the record sizes 256 KiB, 512 KiB, and 1 MiB;"),
    "sep_not_affect": ("10.6", "The validity of a separation extent does not affect whether a replica is accepted for an inventory."),
    "sep_interior_invalid": ("10.6", "An extent whose interior is not entirely zero is invalid for full verification."),
    "sep_derive": ("10.5", "The frame records `E`, and Readers derive the geometry from the recorded value; the default of Section 2.5 is a Writer choice, not a validity condition."),
    "sep_frame_zero": ("10.5", "The meaningful frame is `0x200` bytes and the rest of the full record is zero."),
    "sep_full_records": ("10.5", "One extent is a full header record, the zero-filled interior records, a full footer record, and a trailing filemark."),
    "sep_digest_only": ("10.6", "and in the digest condition only the separation descriptor digest is recomputed, because an extent's frame carries no edition digest."),
    "slot_len": ("10.2", "`encoded_len` is little-endian, nonzero, and at most the slot size minus two; it counts exactly the bytes of the one deterministic CBOR item that follows, and every byte after that item in the slot is zero."),
    "payload_padding": ("10.4", "It runs contiguously across the payload records, and the `payload_padding_bytes` that follow it in the last payload record are zero."),
    "payload_digest_formula": ("10.4", "SHA256(\"REM-TAPE-INDEX-REPLICA-PAYLOAD-V1\\0\" || every complete fixed slot)"),
    "canonical_is": ("10.4", "The canonical-map digest is the Section 7.3 canonical digest of the structural rows."),
    "row_order": ("10.3", "After all `structural_row_count` structural slots, every terminal replica carries exactly `object_row_count` 256-byte fixed slots, one for each kind-0 structural row, in strictly increasing `tape_file_number` order — the same tape-file order as the structural rows."),
    "row_bijection": ("10.3", "The row set is the complete final-prefix Object inventory. Its count MUST equal the number of kind-0 structural slots, and the ordered `(tape_file_number, stored_block_count)` pairs MUST be a bijection with those slots."),
    "key21_bounds": ("10.3", "| 21 | uint | encrypted only | REM-ENCRYPT header `metadata_frame_len`; bounds `[17, 16 MiB]` |"),
    "compression_fixed": ("10.4", "| `0x044` | 4 | compression mode `0` (disabled) | fixed |"),
    "replica_count_fixed": ("10.4", "| `0x03A` | 2 | replica count `3` | fixed |"),
    "reserved_fixed": ("10.4", "| `0x300` | 248 | reserved zero | fixed |"),
    "kind_local": ("10.4", "| local | Replica-local: belongs to one replica | Compared with that replica's plan, its measured observation, or its recomputed digest |"),
    "kind_fixed": ("10.4", "| fixed | A format constant, a reserved zero, or a value set by the frame's role | Equal to the value the row states |"),
    "kind_common": ("10.4", "| common | Edition-common: a fact of the edition that A, B, and C share | Equal in every agreeing replica (Section 8.5); recomputed from, or bound by, the digests of this section |"),
    "fixed_io": ("3.5", "Fixed-block reads and writes only; a read returning other than exactly one block is an error, with two classified boundary outcomes: **Filemark** and **EndOfData**."),
    "footers_differ": ("8.4", "When footers propose different planned layouts, Section 8.5 decides which replicas are accepted."),
    "later_component": ("8.4", "A planned position or digest for a later component does not prove that the component was written."),
    "payload_need": ("8.4", "a replica's payload need be validated only for the replica to be accepted, or when two replica envelopes differ in an edition-common field;"),
    "footer_arith_reject": ("10.4", "A Reader rejects a footer whose values differ."),
    "edition_preimage_compression": ("10.4", "compression_mode:u32=0"),
    "descriptor_preimage_count": ("10.4", "replica_count:u16=3"),
    "generator_cols": ("6.2", "X_j = k + j   (j in 0..m)        Y_i = i   (i in 0..k)"),
    "tuple_filemark": ("8.3", "| `0x04` | 4 | trailing filemark count, exactly `1` |"),
    "missing_filemark": ("12.2", "a zero-block file or a missing trailing filemark is structural damage;"),
    "ladder_count": ("12.3", "The header and measured block count are checked against the encoded component plan."),
    "malformed_control": ("12.3", "A malformed frame or count mismatch is reported as a damaged terminal replica; it MUST NOT fall through to Object."),
})

MUTATIONS_SCHEMA = "rem-parity-second-implementation-mutations/1"
SELECTION_SCHEMA = "rem-parity-second-implementation-selection/1"
REPLICA_POSITIONS = {"A": 0, "B": 2, "C": 4}
_PROFILE_BUILDS: dict[str, tuple[dict[str, Any], TerminalBuild]] = {}


def profile_build(name: str) -> tuple[dict[str, Any], TerminalBuild]:
    """My own build of a terminal profile, from its inputs.json (cached)."""
    if name not in _PROFILE_BUILDS:
        inputs = load_json(FIXTURE_ROOT / name / "inputs.json")
        _PROFILE_BUILDS[name] = (inputs, build_terminal_suffix(terminal_inputs_from_profile(inputs)))
    return _PROFILE_BUILDS[name]


def component_stream(profile: str, filename: str) -> bytes:
    _, terminal = profile_build(profile)
    return b"".join(terminal.components[PROFILE_COMPONENT_FILES.index(filename)])


_BYTE_PATTERN = re.compile(r"^0x([0-9A-Fa-f]{2}) repeated (\d+) times$")


def _spec_bytes(spec: Any, size: int | None = None) -> bytes | None:
    """The bytes a mutation description gives for a field value, or None when it gives none."""
    if isinstance(spec, dict):
        if "hex" in spec:
            return bytes.fromhex(spec["hex"])
        if "byte_pattern" in spec:
            match = _BYTE_PATTERN.match(spec["byte_pattern"])
            if match:
                return bytes([int(match.group(1), 16)]) * int(match.group(2))
        if "unsigned_little_endian" in spec and size:
            return spec["unsigned_little_endian"].to_bytes(size, "little")
        return None
    if isinstance(spec, str):
        match = _BYTE_PATTERN.match(spec)
        if match:
            return bytes([int(match.group(1), 16)]) * int(match.group(2))
        match = re.match(r"^0x([0-9A-Fa-f]{2})\b", spec)
        return bytes([int(match.group(1), 16)]) if match else None
    if isinstance(spec, int) and not isinstance(spec, bool) and size:
        return spec.to_bytes(size, "little")
    return None


def _record_index(text: str) -> int:
    match = re.search(r"record (\d+)", text or "")
    return int(match.group(1)) if match else 0


class ByteChange:
    """Apply one component's byte change as a description states it, checking every stated old value."""

    def __init__(self, stream: bytes, block_size: int, where: str, res: Resolution) -> None:
        self.original = bytes(stream)
        self.stream = bytearray(stream)
        self.block_size = block_size
        self.where = where
        self.res = res

    def check(self, what: str, offset: int, expected: bytes | None, found: bytes, phase: str) -> None:
        self.res.checks.append({"where": self.where, "offset": f"0x{offset:X}" if offset >= 0 else str(offset),
                                "field": what, "phase": phase,
                                "expected": expected.hex() if expected is not None and len(expected) <= 64 else
                                (hashlib.sha256(expected).hexdigest()[:16] + "… (sha256 prefix)" if expected is not None else None),
                                "found": found.hex() if len(found) <= 64 else hashlib.sha256(found).hexdigest()[:16] + "… (sha256 prefix)",
                                "matches": expected is None or expected == found})

    def edit(self, edit: dict[str, Any], apply: bool = True) -> None:
        """Check an edit's old value against my original bytes; write (or, when apply is False, verify) its new value."""
        field_name = edit.get("field", "")
        if "changed_bytes" in edit:
            base = _record_index(edit.get("record", "")) * self.block_size
            for change in edit["changed_bytes"]:
                offset = base + int(change["offset"], 16)
                self.check(f"{field_name} byte", offset, bytes([change["old"]]), self.original[offset : offset + 1], "old")
                if apply:
                    self.stream[offset] = change["new"]
                self.check(f"{field_name} byte", offset, bytes([change["new"]]), bytes(self.stream[offset : offset + 1]), "new")
            return
        if "component_byte_offsets" in edit:
            offsets = edit["component_byte_offsets"]
            olds = [bytes.fromhex(h) for h in edit["old_slot_hex"]]
            news = [bytes.fromhex(h) for h in edit["new_slot_hex"]]
            slots = [bytes(self.original[o : o + len(olds[i])]) for i, o in enumerate(offsets)]
            for i, offset in enumerate(offsets):
                self.check(f"{field_name} (slot {i})", offset, olds[i], slots[i], "old")
            if apply and "Swap" in edit.get("description", ""):
                for i, offset in enumerate(offsets):
                    self.stream[offset : offset + len(slots[i])] = slots[len(offsets) - 1 - i]
            for i, offset in enumerate(offsets):
                self.check(f"{field_name} (slot {i})", offset, news[i], bytes(self.stream[offset : offset + len(news[i])]), "new")
            return
        offset = edit["component_byte_offset"]
        if "old_slot_hex" in edit:
            old_slot, new_slot = bytes.fromhex(edit["old_slot_hex"]), bytes.fromhex(edit["new_slot_hex"])
            self.check(field_name, offset, old_slot, self.original[offset : offset + len(old_slot)], "old")
            if apply:
                match = re.search(r"key (\d+) replaced by (\d+)", edit.get("description", ""))
                slot = bytes(self.original[offset : offset + OBJECT_SLOT])
                value = decode_deterministic_cbor(slot[2 : 2 + rd16(slot, 0)])
                key, to = int(match.group(1)), int(match.group(2))
                self.check(f"{field_name}: key {key}", offset, str(edit["old"]).encode(), str(value.get(key)).encode(), "old")
                value[key] = to
                self.stream[offset : offset + OBJECT_SLOT] = encode_slot(encode_deterministic_cbor(value), OBJECT_SLOT)
            self.check(field_name, offset, new_slot, bytes(self.stream[offset : offset + len(new_slot)]), "new")
            return
        if "operation" in edit:
            match = re.search(r"XOR the first byte with 0x([0-9A-Fa-f]{2})", edit["operation"])
            old, new = _spec_bytes(edit["old"]), _spec_bytes(edit["new"])
            self.check(field_name, offset, old, self.original[offset : offset + 1], "old")
            if apply:
                self.stream[offset] ^= int(match.group(1), 16)
            self.check(field_name, offset, new, bytes(self.stream[offset : offset + 1]), "new")
            return
        size = edit.get("size")
        if size is None and isinstance(edit.get("byte_pattern"), str) and edit["byte_pattern"].replace(" ", "") and \
                all(c in "0123456789abcdefABCDEF " for c in edit["byte_pattern"]):
            size = len(bytes.fromhex(edit["byte_pattern"].replace(" ", "")))
        old, new = _spec_bytes(edit["old"], size), _spec_bytes(edit["new"], size)
        width = len(new) if new is not None else (len(old) if old is not None else size or 1)
        self.check(field_name, offset, old, self.original[offset : offset + width], "old")
        if apply and new is not None:
            self.stream[offset : offset + width] = new
        self.check(field_name, offset, new, bytes(self.stream[offset : offset + width]), "new")

    def repair(self, text: str, frame_len: int) -> None:
        """Recompute one checksum or hash the description lists as recomputed, from my bytes."""
        records = len(self.stream) // self.block_size
        match = re.match(r"(Header|Footer) CRC-64/XZ at 0x([0-9A-Fa-f]+), over (?:header|footer) bytes \[0x000, 0x([0-9A-Fa-f]+)\)", text)
        if match:
            base = 0 if match.group(1) == "Header" else (records - 1) * self.block_size
            at, end = int(match.group(2), 16), int(match.group(3), 16)
            self.stream[base + at : base + at + 8] = le64(crc64_xz(bytes(self.stream[base : base + end])))
            self.res.repairs.append(f"{match.group(1).lower()} CRC-64/XZ at 0x{at:X} recomputed from my bytes")
            return
        match = re.match(r"Footer complete header-record SHA-256 at 0x([0-9A-Fa-f]+)", text)
        if match:
            base = (records - 1) * self.block_size
            at = int(match.group(1), 16)
            self.stream[base + at : base + at + 32] = digest(bytes(self.stream[0 : self.block_size]))
            self.res.repairs.append(f"footer header-record SHA-256 at 0x{at:X} recomputed from my header record")
            return
        if "encoded_len" in text:
            self.res.repairs.append("slot encoded_len and zero fill: part of my re-encoding of the slot")
            return
        self.res.notes.append(f"repair not recognised, not applied: {text}")
        self.res.checks.append({"where": self.where, "offset": "-", "field": "repair", "phase": "repair",
                                "expected": text, "found": None, "matches": False})

    def retained(self, item: dict[str, Any]) -> None:
        """Check a value the description says is retained without recomputation."""
        records = len(self.stream) // self.block_size
        base = 0 if item["record"] == "header" else (records - 1) * self.block_size
        offset = base + int(item["offset"], 16)
        value = item["value"]
        expected = _spec_bytes(value, 8 if "unsigned_little_endian" in value and "hex" not in value else None)
        size = len(expected) if expected is not None else 8
        self.check(f"retained {item['record']} {item['field']}", offset, expected,
                   bytes(self.stream[offset : offset + size]), "retained")


def apply_byte_change(stream: bytes, change: dict[str, Any], repairs: dict[str, Any], block_size: int, where: str,
                      res: Resolution, component_kind: int, profile: str, check_retained: bool = True) -> bytes:
    """Apply one described byte change (with its repairs) to my component bytes."""
    work = ByteChange(stream, block_size, where, res)
    description = change.get("description", "")
    frame_len = REPLICA_FRAME_LEN if component_kind == KIND_REPLICA else SEPARATION_FRAME_LEN
    length = change.get("component_length") or {}
    if "old" in length:
        work.check("component length", -1, str(length["old"]).encode(), str(len(work.original)).encode(), "old")
    edits = change.get("edits", [])
    recomputed = list(repairs.get("recomputed") or [])
    removal = re.match(r"Remove (\d+) bytes from the (beginning|end) of the component", description)
    append = re.match(r"Append one 0x([0-9A-Fa-f]{2}) byte at component offset (\d+)", description)
    footer_swap = re.match(r"Replace the entire footer record with the final (\d+) bytes of ([\w.\-]+)/([\w.\-]+)\.", description)
    header_swap = re.match(r"Replace ([\w.\-]+) header record 0 with the complete ([\w.\-]+) header record 0 from the same base profile",
                           description)
    if removal:
        count = int(removal.group(1))
        base = 0 if removal.group(2) == "beginning" else len(work.original) - block_size
        for item in change.get("removed_or_partial_fields", []) + change.get("removed_frame_fields", []):
            offset = int(item["offset"], 16)
            expected = _spec_bytes(item.get("old"), item.get("size"))
            if expected is not None:
                work.check(f"removed {item['field']}", base + offset, expected,
                           work.original[base + offset : base + offset + len(expected)], "old")
        work.stream = bytearray(work.original[count:] if removal.group(2) == "beginning" else work.original[:-count])
        res.repairs.append(f"removed {count} bytes from the {removal.group(2)} of the component; no byte rewritten")
    elif append:
        offset = int(append.group(2))
        work.check("append offset", offset, str(len(work.original)).encode(), str(offset).encode(), "old")
        work.stream = bytearray(work.original[:offset] + bytes([int(append.group(1), 16)]) + work.original[offset:])
        res.repairs.append(f"appended one byte at component offset {offset}; no byte rewritten")
    elif footer_swap:
        count = int(footer_swap.group(1))
        donor = component_stream(footer_swap.group(2), footer_swap.group(3))
        work.stream[-count:] = donor[-count:]
        res.repairs.append(f"footer record replaced by the last {count} bytes of my {footer_swap.group(2)}/{footer_swap.group(3)}")
        for edit in edits:
            work.edit(edit, apply=False)
    elif header_swap:
        donor = component_stream(profile, header_swap.group(2))
        work.stream[0:block_size] = donor[0:block_size]
        res.repairs.append(f"header record replaced by my {profile}/{header_swap.group(2)} header record")
        for edit in edits:
            work.edit(edit, apply=False)
    elif description:
        res.notes.append(f"description not recognised: {description}")
        res.checks.append({"where": where, "offset": "-", "field": "description", "phase": "apply", "expected": description,
                           "found": None, "matches": False})
    if not (removal or append or footer_swap or header_swap):
        outputs = []
        for edit in edits:
            name = edit.get("field", "")
            role = "Header" if edit.get("record", "").startswith("header") else "Footer"
            # An edit is a repair's output only when the description lists that
            # very checksum or hash as recomputed; otherwise it is the mutation.
            is_output = ("CRC-64/XZ" in name and any(text.startswith(f"{role} CRC-64/XZ") for text in recomputed)) or \
                        ("header-record SHA-256" in name and any("header-record SHA-256" in text for text in recomputed))
            if is_output:
                outputs.append(edit)
                continue
            work.edit(edit)
        for text in recomputed:
            work.repair(text, frame_len)
        for edit in outputs:
            work.edit(edit, apply=False)
    if "new" in length:
        work.check("component length", -1, str(length["new"]).encode(), str(len(work.stream)).encode(), "new")
    if check_retained:
        for item in repairs.get("not_recomputed") or []:
            work.retained(item)
    for text in repairs.get("left_stale") or []:
        res.notes.append("left stale by the description: " + text)
    for text in repairs.get("notes") or []:
        res.notes.append(text)
    return bytes(work.stream)


def terminal_tape(profile_inputs: dict[str, Any], streams: list[bytes | None], block_size: int,
                  absent_keeps_filemark: bool = True) -> DamagedTape:
    """The profile's prefix (placeholder records) and the five components as fixed-size records.

    A component stream is written as records of the block size in order; a
    stream whose length is not a multiple leaves a short final record. A
    component given as None has no records; its trailing filemark stays or
    goes as the caller asks.
    """
    records: list[Any] = []
    for entry in structural_from_json(profile_inputs["structural_entries"]):
        records.extend([bytes(block_size)] * entry.block_count)
        records.append(None)
    for stream in streams:
        if stream is None:
            if absent_keeps_filemark:
                records.append(None)
            continue
        records.extend(stream[i : i + block_size] for i in range(0, len(stream), block_size))
        records.append(None)
    return DamagedTape(records, set())


def _component_error(reason: str, parse_error: str) -> str | None:
    """The Section 15 name this Reader gives a failed component from its check reason."""
    if reason.startswith("medium error") or reason.startswith("payload record unreadable"):
        return "TapeIo"
    body = reason[len("payload: "):] if reason.startswith("payload: ") else reason
    head = body.split(":")[0]
    if head == "TapeIo" and ("found filemark" in body or "found end of data" in body):
        # A filemark or EOD where the plan puts a data record: the declared
        # location disagrees with the device (Section 10.6), a validity failure.
        return parse_error
    if head in ("TerminalIndexReplicaParse", "TerminalIndexSeparationParse", "FilemarkMapDigestMismatch", "TapeIo"):
        return head
    return parse_error


def scan_terminal(tape: DamagedTape, tape_uuid: bytes, block_size: int, terminal_first: int | None = None) -> dict[str, Any]:
    """This implementation's Scanner and Verifier over the terminal suffix (Sections 8.4, 8.5, 10.6)."""
    layout, note = discover_layout(tape, tape_uuid, block_size)
    out: dict[str, Any] = {"layout_source": note, "replicas": {}, "separations": {}}
    checks: dict[str, ReplicaCheck] = {}
    walked: dict[int, WalkedFile] = {}
    if layout is None:
        # No footer supplies a layout: the Scanner walks from BOT, and the
        # ladder names each terminal tape file (Section 12.3 items 2 and 3).
        files, _ = bot_walk(tape, tape_uuid, block_size)
        first = terminal_first if terminal_first is not None else max((f.tape_file_number for f in files), default=-1) - 4
        walked = {f.tape_file_number - first: f for f in files if f.tape_file_number >= first}
        out["walk_classes"] = {f"tape file {first + i}": (KIND_NAMES.get(f.kind, "structural damage") + ": " + f.note)
                               for i, f in sorted(walked.items())}
        for letter in "ABC":
            out["replicas"][letter] = {"valid": False, "error": None, "reason": "no planned layout: " + note}
    else:
        for ordinal in (1, 2, 3):
            check = check_replica(tape, layout, ordinal, tape_uuid, block_size)
            checks[check.letter] = check
            out["replicas"][check.letter] = {"valid": check.fully_valid, "envelope_valid": check.envelope_valid,
                                             "error": None if check.fully_valid else _component_error(check.reason, "TerminalIndexReplicaParse"),
                                             "reason": check.reason}
    valid = [letter for letter, check in checks.items() if check.fully_valid]
    if not valid:
        out.update(outcome="BotStructuralRecoveryRequired", acceptable_selections=[], degraded=None)
    elif len({checks[letter].header["common"] for letter in valid}) > 1:
        out.update(outcome="TerminalIndexReplicaConflict", acceptable_selections=[], degraded=None)
    else:
        out.update(outcome="inventory", acceptable_selections=sorted(valid), degraded=len(valid) < 3)
    accepted_edition = checks[valid[0]].header["edition_id"] if valid and out["outcome"] == "inventory" else None
    for ordinal, name in ((1, "A-B"), (2, "B-C")):
        if layout is None:
            f = walked.get({1: 1, 2: 3}[ordinal])
            damaged = f is not None and f.kind == KIND_SEPARATION and f.note.startswith("damaged")
            out["separations"][name] = {"status": "invalid" if damaged else "not_run",
                                        "error": "TerminalIndexSeparationParse" if damaged else None,
                                        "reason": ("walk: " + f.note) if f is not None else "no planned layout"}
            continue
        status, reason, edition_id = check_separation(tape, layout, ordinal, tape_uuid, block_size)
        if status == "valid" and accepted_edition is not None and edition_id != accepted_edition:
            status, reason = "invalid", "edition ID differs from the replicas'"
        error = None if status == "valid" else ("TapeIo" if status == "unreadable" else
                                                _component_error(reason, "TerminalIndexSeparationParse"))
        out["separations"][name] = {"status": status, "error": error, "reason": reason}
    return out


# ----- the text decisions ----------------------------------------------------

def _component(name: str | None, error: str | None, rules: list[str], why: str, error_set: list[str] | None = None,
               readings: list[str] | None = None, outcome: str = "rejected") -> dict[str, Any]:
    return {"name": name, "outcome": outcome, "error": error, "error_set": error_set, "readings": readings,
            "rules": [cite(k) for k in rules], "why": why}


B6_READINGS = [
    "true: a Scanner that validates replica A's payload (for example, because it tries A first and must validate a "
    "payload to accept a replica) finds A invalid, so a sibling is invalid and the result is degraded",
    "false: a Scanner that accepts B or C after validating only that payload, as Section 8.4 permits when the "
    "envelopes agree, never reads A's payload and reports no degraded evidence",
]
B4_READINGS = [
    "degraded: every replica is valid but separation extent A-B is not, and the Verifier reports the terminal suffix "
    "as degraded",
    "error: the Verifier reports separation extent A-B as a failure (TerminalIndexSeparationParse), and no category of "
    "Section 12.6 applies",
]


def _tape(kind: str) -> tuple[dict[str, Any], dict[str, Any], list[dict[str, Any]]]:
    """The tape-level decision, the Verifier's and the undecided aspects for one class of damage."""
    undecided: list[dict[str, Any]] = []
    if kind == "replica-A-envelope":
        tape = {"outcome": "inventory", "acceptable_selections": ["B", "C"], "degraded": True,
                "replicas": {"A": "invalid: its envelope fails a Section 10.6 condition", "B": "valid", "C": "valid"},
                "rules": [cite(k) for k in ("validate_every", "fully_valid", "consider_conflict", "degraded_evidence",
                                            "survivor", "degraded_result")],
                "why": "The Scanner must validate the header, footer and trailing filemark of every planned replica, so it "
                       "finds A invalid. B and C are fully valid and agree, and an invalid replica is degraded evidence, "
                       "not a conflict. The text does not order the selection among agreeing replicas, so B and C are both "
                       "acceptable."}
        verifier = {"result": "degraded", "separations": {"A-B": "valid", "B-C": "valid"},
                    "rules": [cite("verifier_role"), cite("degraded_result")]}
    elif kind == "replica-A-payload":
        tape = {"outcome": "inventory", "acceptable_selections": ["B", "C"], "degraded": "per reads",
                "degraded_by_reads": {"A's payload not read": False, "A's payload read": True},
                "replicas": {"A": "invalid: its payload fails; its envelope is valid and agrees with B's and C's",
                             "B": "valid", "C": "valid"},
                "rules": [cite(k) for k in ("validate_every", "fully_valid", "survivor", "degraded_def", "unread_payload",
                                            "selection_guide")],
                "why": "A's envelope is valid and agrees with B's and C's in every edition-common field, so no conflict can "
                       "arise and A's payload need be validated only if A is to be accepted. A cannot be accepted, because "
                       "its payload fails; B and C are fully valid and agree. The inventory is degraded when the Scanner "
                       "found a replica invalid, and a replica whose payload it did not read is not found invalid for that "
                       "reason (Section 12.6): the flag is false for a Scanner that never reads A's payload and true for "
                       "one that does. A Verifier's full check reports A."}
        verifier = {"result": "degraded", "separations": {"A-B": "valid", "B-C": "valid"},
                    "rules": [cite("verifier_role"), cite("payload_valid"), cite("degraded_result")]}
    elif kind == "separation-AB":
        tape = {"outcome": "inventory", "acceptable_selections": ["A", "B", "C"], "degraded": False,
                "replicas": {"A": "valid", "B": "valid", "C": "valid"},
                "rules": [cite(k) for k in ("sep_not_affect", "separation_not_needed", "consider_conflict", "survivor")],
                "why": "The separation extent does not affect replica acceptance, and all three replicas are fully valid "
                       "and agree. The Scanner's result is not degraded: Section 12.6's degraded evidence concerns "
                       "replicas."}
        verifier = {"result": "not complete", "separations": {"A-B": "invalid", "B-C": "valid"},
                    "rules": [cite("verifier_separation"), cite("verifier_interior")]}
        undecided.append({"aspect": "verifier.result", "readings": B4_READINGS,
                          "citations": [cite("verifier_separation"), cite("degraded_result")]})
    elif kind == "separation-AB-shifts-suffix":
        tape = {"outcome": "inventory", "acceptable_selections": ["A"], "degraded": True,
                "replicas": {"A": "valid", "B": "invalid: not at its planned coordinate",
                             "C": "invalid: not at its planned coordinate"},
                "rules": [cite(k) for k in ("step1_spacing", "footer_supplies", "locate_planned", "device_agree",
                                            "later_component", "sep_not_affect", "degraded_evidence", "survivor",
                                            "degraded_result")],
                "why": "The extent lost a record, so every record after it moved down one position: B, B-C and C are no "
                       "longer at the positions the immutable plan gives. C's and B's footers are not at their recorded "
                       "positions, so they supply no layout. A's footer is, and its planned EOD is after the tape's EOD, so "
                       "it supplies the plan (Section 8.4 step 1). Under it B's and C's declared locations disagree with "
                       "the device's measurements, and A, which precedes the change, stays valid."}
        verifier = {"result": "degraded", "separations": {"A-B": "invalid", "B-C": "invalid"},
                    "rules": [cite("verifier_separation"), cite("degraded_result")]}
    elif kind == "separation-AB-lengthens-suffix":
        tape = {"outcome": "BotStructuralRecoveryRequired", "acceptable_selections": [], "degraded": None,
                "replicas": {"A": "not validated: no footer supplies a planned layout",
                             "B": "not validated: no footer supplies a planned layout",
                             "C": "not validated: no footer supplies a planned layout"},
                "rules": [cite(k) for k in ("step1_spacing", "footer_supplies", "layout_before_eod", "no_replica_walk",
                                            "select_walk", "bot_required", "err_bot")],
                "why": "The extent gained a record, so every record after it moved up one position and the tape's EOD is "
                       "one beyond the planned EOD. C's and B's footers are not at their recorded positions, and A's footer, "
                       "which is, plans an EOD before the tape's EOD, so its layout is not used. No footer supplies a "
                       "layout, so no replica validates, and the Scanner walks from BOT (Section 8.4 step 1)."}
        verifier = {"result": "recovery_required", "separations": {"A-B": "invalid", "B-C": "invalid"},
                    "rules": [cite("walk_evidence"), cite("err_bot"), cite("ladder_separation"), cite("malformed_control")]}
    else:
        raise ValueError(kind)
    return tape, verifier, undecided


def mutations_table() -> dict[str, dict[str, Any]]:
    """The text decision for each blind mutation, written from the description and the text."""
    t: dict[str, dict[str, Any]] = {}
    A, AB = "replica A", "separation extent A-B"
    SEP_ORDER = " Section 10.6 fixes no order among its conditions, and every failing one has the same name."

    def replica(mid, rules, why, kind="replica-A-envelope", error="TerminalIndexReplicaParse", error_set=None):
        t[mid] = {"component": _component(A, error, rules + ["err_replica"], why, error_set=error_set), "kind": kind}

    def separation(mid, rules, why, kind="separation-AB", error="TerminalIndexSeparationParse", error_set=None):
        t[mid] = {"component": _component(AB, error, rules + ["sep_valid", "err_separation"], why, error_set=error_set),
                  "kind": kind}

    separation("mut-01", ["magic_crc_miss", "frame_fields", "sep_full_records", "wrong_length"],
               "Every record of the extent now starts 100 bytes later in the original stream: the header record begins "
               "with original byte 100, so its role magic does not match, and the footer record begins with byte 100 of "
               "the old footer and is 262 044 bytes long. The magic miss alone invalidates the extent; the short record "
               "is invalid content of the extent as well (Section 3.5). The extent still has three records, so nothing "
               "after it moves." + SEP_ORDER)
    replica("mut-02", ["frame_fields", "compression_fixed", "kind_fixed"],
            "Both frames record compression mode 1; the row is fixed at 0. The edition digest was left stale, but its "
            "preimage holds the constant compression_mode:u32=0, so whether it still recomputes depends on whether a "
            "Reader hashes the constant or the field (GAPS H-9); the fixed-field rule fails either way.")
    replica("mut-03", ["frame_fields", "record_sizes", "digests_recompute", "layout_valid"],
            "Both frames record a block size of 131 072, which is neither the record size read (262 144) nor a terminal "
            "record size; the stale edition and layout digests also fail. The block size is edition-common, but A is not "
            "fully valid, so it takes no part in the conflict rule." + SEP_ORDER)
    separation("mut-04", ["device_agree", "sep_full_records"],
               "The footer record is removed: the extent has two records, and the planned footer coordinate holds its "
               "trailing filemark, so the footer, the count and the trailing filemark all disagree with the plan.",
               kind="separation-AB-shifts-suffix")
    replica("mut-05", ["digests_recompute", "kind_common", "fully_valid"],
            "The edition ID is in the edition-digest preimage and the digest was left stale, so it does not recompute. A "
            "differs from B and C in an edition-common field, but a replica that is not fully valid takes no part in "
            "the conflict rule: this is degraded evidence, not a conflict. The separation extents carry the accepted "
            "edition's ID, so they stay valid (GAPS B-5).")
    replica("mut-06", ["digests_recompute"],
            "The recorded edition digest (0x94 x 32) is not the digest of the frame's fields. The descriptor digest, whose "
            "preimage holds the edition digest, was left stale as well.")
    separation("mut-07", ["magic_crc_miss", "device_agree", "sep_full_records"],
               "The header record is removed: the record at the planned header coordinate is the zero interior, whose "
               "magic does not match, and the extent has two records.", kind="separation-AB-shifts-suffix")
    replica("mut-08", ["backward_delta", "device_agree", "footer_local"],
            "A's footer is now B's: it names ordinal 2 and B's planned coordinates, its backward delta names B's header "
            "start (LBA 33) rather than A's (25), its recorded header hash is B's header's, and its descriptor differs "
            "from A's header's. It also declares a footer LBA the device does not measure." + SEP_ORDER)
    replica("mut-09", ["payload_valid", "row_order", "row_bijection", "payload_digest_formula"],
            "The two Object-row slots are exchanged: the rows are no longer in tape-file order or in bijection with the "
            "kind-0 structural rows, and the payload digest was left stale." + SEP_ORDER, kind="replica-A-payload")
    separation("mut-10", ["magic_crc_miss", "frame_fields"],
               "A reserved header byte is set and the header CRC was left stale: the CRC miss invalidates the extent, and "
               "the reserved bytes and the footer's header-record hash also fail." + SEP_ORDER)
    separation("mut-11", ["frame_fields", "sep_digest_only"],
               "Both frames claim separation ordinal 2 at the first extent's planned coordinate, so the ordinal disagrees "
               "with the plan, and the descriptor digest (left stale) does not recompute." + SEP_ORDER)
    replica("mut-12", ["frame_zero", "frame_fields", "backward_delta"],
            "Header byte 0x400 lies outside the CRC-covered frame, but the record past the frame must be zero, and the "
            "footer's header-record hash no longer matches the header record." + SEP_ORDER)
    replica("mut-13", ["footer_delta", "footer_arith_reject", "footer_local"],
            "The footer's observed record count is 4 with backward delta 2 and footer LBA 27, so the footer arithmetic "
            "fails, and the observation differs from the planned tuple (3 records)." + SEP_ORDER)
    replica("mut-14", ["layout_valid", "digests_recompute"],
            "The recorded layout digest (0x93 x 32) does not recompute from the tuples, and the descriptor digest (left "
            "stale) does not either." + SEP_ORDER)
    replica("mut-15", ["backward_delta"],
            "The footer's recorded header-record SHA-256 differs from the SHA-256 of the header record at the planned "
            "coordinate.")
    separation("mut-16", ["magic_crc_miss", "frame_fields"],
               "A reserved footer byte is set and the footer CRC was left stale: the CRC miss invalidates the extent.")
    separation("mut-17", ["frame_fields", "sep_digest_only"],
               "Both frames record tape UUID 0x12 x 16, not the tape's; the role magics still derive from the tape's UUID, "
               "so they match. The descriptor digest (left stale) does not recompute over either UUID." + SEP_ORDER)
    separation("mut-18", ["frame_fields", "sep_full_records", "wrong_length"],
               "The last 100 bytes of the footer record (zero padding) are removed, so the footer record is 262 044 bytes, "
               "not a full record: invalid content of the extent (Section 3.5), and the record-length condition fails. "
               "The record count is unchanged, so nothing after the extent moves.")
    replica("mut-19", ["digest_scope", "w_equals_t", "digests_recompute", "scope_in_list"],
            "The recorded total data ordinals (6) differs from T recomputed over the structural rows (5), W (5) no "
            "longer equals it although sidecars are present, and the edition digest (left stale) does not recompute."
            + SEP_ORDER)
    replica("mut-20", ["kind_local", "device_agree"],
            "The frames' planned local start LBA (26) differs from the local tuple's start (25) and from the device's "
            "measurement of A's start.")
    replica("mut-21", ["frame_fields", "kind_local", "digests_recompute"],
            "Both frames claim replica ordinal 2 at A's planned coordinate: the local tuple for ordinal 2 is B's (tape "
            "file 8), not the frame's planned tape file 6, and the descriptor digest (left stale) does not recompute."
            + SEP_ORDER)
    replica("mut-22", ["key21_bounds", "payload_valid", "payload_digest_formula"],
            "The encrypted row's metadata_frame_len (16) is below the bound [17, 16 MiB], and the payload digest was "
            "left stale." + SEP_ORDER, kind="replica-A-payload")
    replica("mut-23", ["digests_recompute", "kind_local"],
            "The recorded replica descriptor digest (0x95 x 32) does not recompute.")
    replica("mut-24", ["reserved_fixed", "kind_fixed", "frame_fields"],
            "The reserved field at 0x300 is nonzero in both frames; the checksums were repaired, so only the reserved-zero "
            "rule fails.")
    replica("mut-25", ["digests_recompute", "payload_valid", "payload_digest_formula"],
            "The recorded payload SHA-256 (0x91 x 32) is not the digest of the fixed slots, and the edition digest, whose "
            "preimage holds it, was left stale." + SEP_ORDER)
    replica("mut-26", ["frame_fields", "record_whole", "record_sizes", "wrong_length", "wrong_length_never_tapeio"],
            "The last 100 bytes of the footer record (zero padding) are removed, so the footer record is 262 044 bytes, "
            "not one complete record: invalid content of the replica, never TapeIo (Section 3.5), and the record-length "
            "condition fails.")
    replica("mut-27", ["magic_crc_miss", "role_magic", "frame_fields", "wrong_length"],
            "Every record now starts 100 bytes later in the original stream: the header record begins with original "
            "byte 100, so its role magic does not match, and the footer record is short and begins inside the old "
            "footer." + SEP_ORDER)
    replica("mut-28", ["frame_fields", "digests_recompute"],
            "Both frames record tape UUID 0x12 x 16, not the tape's; the role magics still derive from the tape's UUID. "
            "The edition digest (left stale) does not recompute." + SEP_ORDER)
    replica("mut-29", ["magic_crc_miss", "reserved_fixed"],
            "A reserved header byte is set and the header CRC was left stale: the CRC miss invalidates the candidate.")
    replica("mut-30", ["slot_len", "payload_valid", "payload_digest_formula"],
            "The first structural slot's encoded_len is 65 535, more than the slot size minus two, so the slot does not "
            "decode; the payload digest was left stale." + SEP_ORDER, kind="replica-A-payload")
    separation("mut-31", ["backward_delta", "frame_fields", "device_agree"],
               "The extent's footer is now B-C's: its ordinal, local and neighbour coordinates and descriptor are B-C's, "
               "its backward delta names B-C's header, and its header-record hash is B-C's header's." + SEP_ORDER)
    replica("mut-32", ["replica_count_fixed", "kind_fixed", "frame_fields"],
            "Both frames record a replica count of 2; the row is fixed at 3. The descriptor digest was left stale, but its "
            "preimage holds the constant replica_count:u16=3 (GAPS H-9); the fixed-field rule fails either way.")
    separation("mut-33", ["role_magic", "conjunction", "magic_crc_miss", "malformed_control"],
               "The extent's header record is replica A's header record. For the extent at its planned coordinate the "
               "separation role magic does not match, which invalidates it (TerminalIndexSeparationParse). The record "
               "carries the terminal-replica header magic, however, and a matching role magic commits a tape file to its "
               "control type; read that way, tape file 7 is a damaged terminal replica, whose frame plans itself at tape "
               "file 6 (TerminalIndexReplicaParse). The text fixes no order between the two (GAPS H-8). Replica A itself, "
               "at tape file 6, is untouched.",
               error=None, error_set=["TerminalIndexSeparationParse", "TerminalIndexReplicaParse"])
    t["mut-33"]["component"]["rules"].append(cite("err_replica"))
    replica("mut-34", ["payload_padding", "payload_valid"],
            "The first byte of the payload padding is nonzero. The payload digest covers the fixed slots only, so it "
            "still matches; the zero-padding rule is the one that fails.", kind="replica-A-payload")
    t["mut-35"] = {"component": _component(None, None, ["tuple_filemark", "missing_filemark"],
                                           "The row is an event with no byte effect: its description defines no changed "
                                           "byte, filemark or tuple, so the bytes to decide are the base profile's.",
                                           outcome="no-vector"),
                   "kind": "none"}
    replica("mut-36", ["payload_digest_formula", "payload_valid"],
            "The first Object row's object_id begins 'l' instead of 'm'. The row itself still decodes and fits every "
            "row rule, but the payload digest, left stale, no longer matches the slots.", kind="replica-A-payload")
    separation("mut-37", ["sep_arith", "footer_delta", "footer_arith_reject", "footer_local"],
               "The footer's observed start LBA is 30 with delta 2 and footer LBA 31, so the footer arithmetic fails, and "
               "the observation differs from the planned tuple (start 29)." + SEP_ORDER)
    separation("mut-38", ["device_agree", "sep_full_records", "wrong_length", "ladder_separation"],
               "One byte appended after the footer record makes a fourth, one-byte record: the extent's measured count "
               "and trailing filemark disagree with its own frames (3 records), and the extra record is not a full "
               "record (Section 3.5). Its magic commits the tape file to the separation type, damaged.",
               kind="separation-AB-lengthens-suffix")
    replica("mut-39", ["digests_recompute", "payload_valid", "canonical_is", "conjunction", "err_digest_mismatch"],
            "The recorded canonical-map SHA-256 (0x92 x 32) is not the canonical digest of the structural rows, and the "
            "edition digest, whose preimage holds it, was left stale. The edition-digest failure is "
            "TerminalIndexReplicaParse; the canonical-map mismatch is Section 15's FilemarkMapDigestMismatch ('replica "
            "structural projection digest mismatch') and also a Section 10.6 payload condition. Section 10.6 fixes no "
            "order, so either name is permitted (GAPS H-10).",
            error=None, error_set=["TerminalIndexReplicaParse", "FilemarkMapDigestMismatch"])
    separation("mut-40", ["verifier_interior", "sep_interior_invalid", "separation_not_needed"],
               "The first interior byte is 0x01. No CRC or digest covers the interior, so the frames validate; a Verifier "
               "performing full verification must find the interior nonzero, and such an extent is invalid for full "
               "verification. A Scanner need not read the extent at all.")
    replica("mut-41", ["covered_equal", "digest_scope", "digests_recompute"],
            "The covered-prefix tape-file count (7) differs from structural_row_count (6) and from A's planned tape-file "
            "number (6), and the edition digest (left stale) does not recompute." + SEP_ORDER)
    replica("mut-42", ["footer_delta", "footer_arith_reject", "backward_delta", "footer_local"],
            "The footer's observed start LBA is 26 with delta 2 and footer LBA 27, so the footer arithmetic fails; the "
            "backward delta no longer names the planned header start, and the observation differs from the plan."
            + SEP_ORDER)
    replica("mut-43", ["magic_crc_miss", "reserved_fixed"],
            "A reserved footer byte is set and the footer CRC was left stale: the CRC miss invalidates the candidate.")
    separation("mut-44", ["frame_fields"],
               "Both frames record compression mode 1; a separation frame's compression mode is 0. The checksums were "
               "repaired, and the descriptor preimage does not hold the compression mode, so only this rule fails.")
    separation("mut-45", ["separation_edition", "sep_digest_only"],
               "Both frames record edition ID 0x24 x 16, not the replicas' (0x22 x 16), and the descriptor digest (left "
               "stale) does not recompute." + SEP_ORDER)
    separation("mut-46", ["sep_records", "sep_derive", "sep_digest_only", "device_agree"],
               "The recorded total record count (4) is not ceil(E / B) = 3 from the recorded E, and differs from the "
               "planned tuple and the measured count; the descriptor digest (left stale) does not recompute." + SEP_ORDER)
    replica("mut-47", ["size_formulas", "checked", "terminal_u64"],
            "object_row_count = 2^56 makes payload_len = 64 x 6 + 256 x 2^56 = 2^64 + 384, which overflows u64; Section "
            "10.6 requires every size formula to evaluate without overflow. The stale edition digest and the retained "
            "geometry also fail." + SEP_ORDER)
    replica("mut-48", ["size_formulas", "checked", "terminal_u64"],
            "structural_row_count = 2^58 - 1 and object_row_count = 1 make payload_len = 64 x (2^58 - 1) + 256 = 2^64 + "
            "192, which overflows u64. The stale edition digest and the retained geometry also fail." + SEP_ORDER)
    separation("mut-49", ["sep_records", "sep_derive", "sep_digest_only"],
               "The recorded E (786 433) gives total_records = ceil(E / B) = 4, not the recorded 3, the planned tuple's 3 "
               "or the measured 3; the descriptor digest (left stale) does not recompute." + SEP_ORDER)
    replica("mut-50", ["size_formulas", "checked", "terminal_u64"],
            "structural_row_count = 2^64 - 1 makes payload_len = 64 x (2^64 - 1) + 512, which overflows u64. The stale "
            "edition digest and the retained geometry also fail." + SEP_ORDER)
    return t


def mutation_decision(mid: str, entry: dict[str, Any]) -> dict[str, Any]:
    """Assemble one mutation's decision: the component, the tape, the Verifier and the undecided aspects."""
    if entry["kind"] == "none":
        tape = {"outcome": "inventory", "acceptable_selections": ["A", "B", "C"], "degraded": False,
                "replicas": {"A": "valid", "B": "valid", "C": "valid"},
                "rules": [cite(k) for k in ("consider_conflict", "survivor")],
                "why": "With no byte changed, the three replicas are the base profile's: fully valid and in agreement."}
        return {"component": entry["component"], "tape": tape,
                "verifier": {"result": "complete", "separations": {"A-B": "valid", "B-C": "valid"},
                             "rules": [cite("normal_finalized")]},
                "undecided": [],
                "note": ("Informative only; the row does not define these. If the event removed replica A's trailing "
                         "filemark, A would fail its trailing-filemark condition and every later record would move down "
                         "one position, so B and C would not be at their planned coordinates either: no replica would "
                         "validate, BotStructuralRecoveryRequired. If it edited a component tuple's trailing filemark count "
                         "(Section 8.3: exactly 1), every frame carrying that tuple would fail layout validation; the "
                         "outcome would depend on which frames and which repairs, which the row does not say.")}
    tape, verifier, undecided = _tape(entry["kind"])
    return {"component": entry["component"], "tape": tape, "verifier": verifier, "undecided": undecided, "note": ""}


def _self_check_tape(decision: dict[str, Any], found: dict[str, Any], readings: list[dict[str, Any]] | None = None) -> list[str]:
    problems = []
    tape = decision
    options = readings or [tape]
    matched = False
    for option in options:
        if option["outcome"] != found["outcome"]:
            continue
        if option["outcome"] == "inventory":
            if option["acceptable_selections"] != found["acceptable_selections"]:
                continue
            if option["degraded"] not in ("undecided", "per reads", found["degraded"]):
                continue
        matched = True
    if not matched:
        problems.append(f"tape: implementation {found['outcome']} {found.get('acceptable_selections')} "
                        f"degraded {found.get('degraded')}")
    return problems


def implementation_summary(found: dict[str, Any]) -> str:
    text = f"implementation: {found['outcome']}"
    if found.get("acceptable_selections"):
        text += " " + "/".join(found["acceptable_selections"])
    return text + (" degraded" if found.get("degraded") else "")


def run_mutations(path: pathlib.Path, out_path: pathlib.Path) -> dict[str, Any]:
    source = load_json(path)
    table = mutations_table()
    entries: dict[str, Any] = {}
    for row in source["rows"]:
        mid = row["case_id"]
        profile = row["base_profile"]
        inputs, terminal = profile_build(profile)
        block_size = inputs["block_size"]
        tape_uuid = bytes.fromhex(inputs["tape_uuid"])
        streams: list[bytes | None] = [b"".join(c) for c in terminal.components]
        res = Resolution()
        target = PROFILE_COMPONENT_FILES.index(row["target"])
        vector = "bytes"
        if row["kind"] == "event":
            vector = "none"
            res.notes.append("event row: " + row["bytes"].get("description", ""))
            res.notes.append("repairs: " + (row["repairs"].get("description") or ""))
        else:
            kind = KIND_REPLICA if target in (0, 2, 4) else KIND_SEPARATION
            streams[target] = apply_byte_change(streams[target], row["bytes"], row["repairs"], block_size,
                                                f"{profile}/{row['target']}", res, kind, profile)
        records = {PROFILE_COMPONENT_FILES[i]: [len(s[j : j + block_size]) for j in range(0, len(s), block_size)]
                   for i, s in enumerate(streams) if s is not None}
        changed = []
        for i, stream in enumerate(streams):
            base = b"".join(terminal.components[i])
            if stream != base:
                changed.append({"component": PROFILE_COMPONENT_FILES[i], "bytes": len(stream),
                                "sha256": hashlib.sha256(stream).hexdigest()})
        spec = table.get(mid)
        out: dict[str, Any] = {"id": mid, "kind": row["kind"], "base_profile": profile, "target": row["target"],
                               "other_profile": row.get("other_profile") or None,
                               "apply": {"resolved": res.resolved, "vector": vector,
                                         "checks_total": len(res.checks),
                                         "checks_failed": [c for c in res.checks if not c["matches"]],
                                         "repairs": res.repairs, "notes": res.notes,
                                         "record_lengths": {name: (lengths if len(set(lengths)) > 1 or len(lengths) != 3 else
                                                                   f"3 x {lengths[0]}") for name, lengths in records.items()},
                                         "changed_components": changed},
                               "decision": None, "implementation": None, "self_check": None}
        if not res.resolved or spec is None:
            out["decision"] = {"component": _component(None, None, [], "the description does not resolve against my bytes"
                                                       if spec is not None else "not analysed", outcome="unresolved"),
                               "tape": None, "verifier": None, "undecided": [], "note": ""}
            entries[mid] = out
            continue
        decision = mutation_decision(mid, spec)
        out["decision"] = decision
        found = scan_terminal(terminal_tape(inputs, streams, block_size), tape_uuid, block_size,
                              len(inputs["structural_entries"]))
        out["implementation"] = found
        problems = _self_check_tape(decision["tape"], found)
        component = decision["component"]
        if component["name"] is not None:
            got = (found["replicas"]["A"] if component["name"] == "replica A" else
                   {"valid": found["separations"]["A-B"]["status"] == "valid", "error": found["separations"]["A-B"]["error"]})
            allowed = component["error_set"] or [component["error"]]
            if got["valid"] or got["error"] not in allowed:
                problems.append(f"{component['name']}: implementation {'valid' if got['valid'] else got['error']}")
        verifier_separation = decision["verifier"]["separations"]["A-B"]
        if verifier_separation in ("valid", "invalid") and \
                (found["separations"]["A-B"]["status"] == "valid") != (verifier_separation == "valid"):
            problems.append(f"separation A-B: implementation {found['separations']['A-B']['status']}")
        out["self_check"] = {"agrees": not problems, "detail": "; ".join(problems) or implementation_summary(found)}
        entries[mid] = out
    output = {"schema": MUTATIONS_SCHEMA, "source": path.name, "entries": entries}
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(output, indent=2, ensure_ascii=False, default=_json_default) + "\n", encoding="utf-8")
    return output


def mutation_summary(entry: dict[str, Any]) -> str:
    d = entry["decision"]
    component = d["component"]
    text = ""
    if component["outcome"] == "unresolved":
        return "UNRESOLVED: " + component["why"]
    if component["name"]:
        name = component["error"] or "{" + " | ".join(component["error_set"] or []) + "}"
        text += f"{component['name']} {component['outcome']} {name}; "
    else:
        text += f"no component mutated ({component['outcome']}); "
    tape = d["tape"]
    if tape["outcome"] == "inventory":
        degraded = {True: "degraded", False: "not degraded", "undecided": "degraded undecided",
                    "per reads": "degraded per reads (Section 12.6)"}[tape["degraded"]]
        text += f"tape: inventory from {'/'.join(tape['acceptable_selections'])}, {degraded}"
    else:
        text += f"tape: {tape['outcome']}"
    if d["verifier"] and d["verifier"]["result"] not in ("degraded", "complete"):
        text += f"; Verifier: {d['verifier']['result']}"
    if entry["self_check"] and not entry["self_check"]["agrees"]:
        text += f" [SELF-CHECK FAILED: {entry['self_check']['detail']}]"
    return text


# ----- survivor sets ---------------------------------------------------------

def parse_status(text: str) -> dict[str, Any]:
    """Classify one survivor status by its definition in the selection file."""
    if text.startswith("the profile's replica, unchanged"):
        return {"class": "unchanged"}
    if text.startswith("the profile's replica with this byte change:"):
        decoder = json.JSONDecoder()
        body = text[len("the profile's replica with this byte change:"):].lstrip()
        change, end = decoder.raw_decode(body)
        rest = body[end:].lstrip()
        repairs = {}
        if rest.startswith("; repairs:"):
            repairs, _ = decoder.raw_decode(rest[len("; repairs:"):].lstrip())
        return {"class": "byte-change", "change": change, "repairs": repairs}
    if text.startswith("no replica at this position"):
        return {"class": "absent"}
    match = re.match(r"at this position, the replica of the same position taken from the (\w+) profile at the same block size", text)
    if match:
        return {"class": "foreign", "profile_family": match.group(1)}
    return {"class": "unknown", "text": text}


def _status_text_decision(position: str, status: dict[str, Any], base_letter_valid: bool = True) -> tuple[str, list[str], str]:
    """(validity, rule keys, reason) for a replica of a given status, from the text."""
    cls = status["class"]
    if cls == "unchanged":
        return "valid", ["fully_valid"], "the profile's replica, which is locally eligible"
    if cls == "absent":
        return "missing", ["degraded_evidence", "walk_offer"], "no records at the planned coordinate"
    if cls == "foreign":
        return "invalid", ["device_agree", "locate_planned", "backward_delta", "footer_supplies", "degraded_evidence"], (
            "a replica of another edition planned for other coordinates (the minimal profile's layout): its footer's "
            "recorded position is not where it is read, so it supplies no layout, and its declared tape-file number and "
            "start LBA disagree with the device's measurements, so it is never locally eligible and takes no part in the "
            "conflict rule")
    change = status["change"]
    edits = change.get("edits", [])
    description = change.get("description", "")
    stale = " ".join(status.get("repairs", {}).get("left_stale") or [])
    if re.match(r"Remove \d+ bytes from the end", description):
        return "invalid", ["frame_fields", "record_whole", "record_sizes", "wrong_length"], (
            "its footer record is shorter than one complete record: invalid content of the replica (Section 3.5), and "
            "the record-length condition fails")
    if edits and all(e.get("record", "").startswith("header") for e in edits) and "Header CRC" in stale:
        return "invalid", ["magic_crc_miss", "reserved_fixed"], "a header byte changed under a stale header CRC: a CRC miss"
    return "unknown", [], "status not analysed"


def selection_decision(row: dict[str, Any], statuses: dict[str, dict[str, Any]]) -> dict[str, Any]:
    """Decide one survivor set from the text."""
    per = {letter: _status_text_decision(letter, statuses[row[letter]]) for letter in "ABC"}
    rules = ["locate_footer", "validate_every", "consider_conflict", "fully_valid", "conflict_rule", "degraded_evidence"]
    replicas = {letter: f"{v[0]}: {v[2]}" for letter, v in per.items()}
    for v in per.values():
        rules += [k for k in v[1] if k not in rules]
    if any(v[0] == "unknown" for v in per.values()):
        return {"outcome": "unresolved", "replicas": replicas, "rules": [], "undecided": []}
    valid = [letter for letter in "ABC" if per[letter][0] == "valid"]

    def inventory(selected: list[str]) -> dict[str, Any]:
        if not selected:
            return {"outcome": "BotStructuralRecoveryRequired", "acceptable_selections": [], "degraded": None}
        return {"outcome": "inventory", "acceptable_selections": selected, "degraded": len(selected) < 3}

    # Section 8.4 step 1: spacing back from EOD, the first replica footer
    # that parses, sits at its recorded position and plans an EOD at or after
    # the tape's supplies the planned layout. Here only the profile's own
    # footers (S0, and S1, whose change is confined to the header) do.
    supplies = {letter: _status_supplies_layout(statuses[row[letter]]) for letter in "ABC"}
    source = next((letter for letter in "CBA" if supplies[letter]), None)
    rules += ["step1_spacing", "footer_supplies"]
    if source is None:
        valid = []
        rules += ["layout_before_eod"]
    decided = inventory(valid)
    undecided = []
    readings = None
    if decided["outcome"] == "BotStructuralRecoveryRequired":
        rules += ["no_replica_walk", "select_walk", "bot_required", "err_bot"]
    elif decided["outcome"] == "inventory":
        rules += ["survivor"] + (["degraded_result"] if decided["degraded"] else []) + \
                 (["selection_guide"] if len(decided["acceptable_selections"]) > 1 else [])
    return dict(decided, replicas=replicas, rules=[cite(k) for k in dict.fromkeys(rules)], readings=readings,
                undecided=undecided)


def _status_supplies_layout(status: dict[str, Any]) -> bool:
    """Whether the footer a status leaves at its position supplies a layout under Section 8.4 step 1."""
    cls = status["class"]
    if cls == "unchanged":
        return True
    if cls in ("absent", "foreign"):
        # An absent replica has no footer; a foreign one records another position.
        return False
    change = status["change"]
    if re.match(r"Remove \d+ bytes from the end", change.get("description", "")):
        return False  # a short footer record is invalid content (Section 3.5)
    edits = change.get("edits", [])
    return bool(edits) and all(e.get("record", "").startswith("header") for e in edits)


def build_status_stream(status: dict[str, Any], position: str, profile: str, block_size: int, res: Resolution) -> bytes | None:
    filename = {"A": "replica-a.bin", "B": "replica-b.bin", "C": "replica-c.bin"}[position]
    cls = status["class"]
    if cls == "unchanged":
        return component_stream(profile, filename)
    if cls == "absent":
        return None
    if cls == "foreign":
        other = f"{status['profile_family']}-{profile.split('-', 1)[1]}"
        res.notes.append(f"position {position}: my {other}/{filename}")
        return component_stream(other, filename)
    if cls == "byte-change":
        return apply_byte_change(component_stream(profile, filename), status["change"], status["repairs"], block_size,
                                 f"{profile}/{filename} (position {position})", res, KIND_REPLICA, profile,
                                 check_retained=(position == "A"))
    raise ValueError(f"unknown status {status}")


def run_selection(path: pathlib.Path, out_path: pathlib.Path) -> dict[str, Any]:
    source = load_json(path)
    statuses = {name: parse_status(text) for name, text in source["statuses"].items()}
    entries: dict[str, Any] = {}
    for row in source["rows"]:
        profile = row["base_profile"]
        inputs, terminal = profile_build(profile)
        block_size = inputs["block_size"]
        tape_uuid = bytes.fromhex(inputs["tape_uuid"])
        res = Resolution()
        streams: list[bytes | None] = [b"".join(c) for c in terminal.components]
        for letter in "ABC":
            streams[REPLICA_POSITIONS[letter]] = build_status_stream(statuses[row[letter]], letter, profile, block_size, res)
        decision = selection_decision(row, statuses)
        results = {}
        models = [True, False] if any(statuses[row[l]]["class"] == "absent" for l in "ABC") else [True]
        for keeps in models:
            results["filemark kept" if keeps else "filemark absent"] = scan_terminal(
                terminal_tape(inputs, streams, block_size, absent_keeps_filemark=keeps), tape_uuid, block_size,
                len(inputs["structural_entries"]))
        found = results["filemark kept"]
        problems = []
        if not res.resolved:
            problems.append("a status description does not resolve against my bytes")
        for label, result in results.items():
            if decision["outcome"] == "unresolved":
                problems.append("decision unresolved")
            elif decision["outcome"] == "undecided":
                problems += [f"{label}: {p}" for p in _self_check_tape(decision, result, decision["readings"])]
            else:
                problems += [f"{label}: {p}" for p in _self_check_tape(decision, result)]
        entry = {"id": row["id"], "base_profile": profile, "A": row["A"], "B": row["B"], "C": row["C"],
                 "apply": {"resolved": res.resolved, "checks_total": len(res.checks),
                           "checks_failed": [c for c in res.checks if not c["matches"]], "repairs": res.repairs,
                           "notes": res.notes},
                 "decision": decision,
                 "implementation": found if len(results) == 1 else results,
                 "self_check": {"agrees": not problems, "detail": "; ".join(problems) or implementation_summary(found)}}
        entries[row["id"]] = entry
    output = {"schema": SELECTION_SCHEMA, "source": path.name,
              "statuses": {name: {"class": s["class"]} for name, s in statuses.items()}, "entries": entries}
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(output, indent=2, ensure_ascii=False, default=_json_default) + "\n", encoding="utf-8")
    return output


def selection_summary(entry: dict[str, Any]) -> str:
    d = entry["decision"]
    text = f"A {entry['A']} B {entry['B']} C {entry['C']}: "
    if d["outcome"] == "inventory":
        text += f"inventory from {'/'.join(d['acceptable_selections'])}, {'degraded' if d['degraded'] else 'not degraded'}"
    elif d["outcome"] == "undecided":
        text += "undecided: " + " | ".join(
            (f"inventory from {'/'.join(r['acceptable_selections'])}{' degraded' if r['degraded'] else ''}"
             if r["outcome"] == "inventory" else r["outcome"]) for r in d["readings"])
    else:
        text += d["outcome"]
    if not entry["self_check"]["agrees"]:
        text += f" [SELF-CHECK FAILED: {entry['self_check']['detail']}]"
    return text


# ---------------------------------------------------------------------------
# Command line.
# ---------------------------------------------------------------------------


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    build = commands.add_parser("build", help="re-derive every pinned artifact and compare")
    build.add_argument("--out", type=pathlib.Path, default=OUTPUT_ROOT)
    build.add_argument("--skip-streaming", action="store_true")
    decide = commands.add_parser("decide", help="decide damage cases as a Reader under the text")
    decide.add_argument("cases", nargs="+", type=pathlib.Path)
    decide.add_argument("--out", type=pathlib.Path, default=OUTPUT_ROOT / "decisions.json")
    negatives = commands.add_parser("negatives", help="apply and decide the generation-2 negative cases")
    negatives.add_argument("cases", type=pathlib.Path)
    negatives.add_argument("--out", type=pathlib.Path, default=OUTPUT_ROOT / "negative-decisions.json")
    negatives_e1 = commands.add_parser("negative-e1", help="apply and decide the E1 negative case (a bootstrap-role case)")
    negatives_e1.add_argument("cases", type=pathlib.Path)
    negatives_e1.add_argument("--out", type=pathlib.Path, default=OUTPUT_ROOT / "negative-e1-decisions.json")
    supplement = commands.add_parser("negatives-supplement", help="apply, isolate and decide the single-rule variants")
    supplement.add_argument("variants", type=pathlib.Path)
    supplement.add_argument("--out", type=pathlib.Path, default=OUTPUT_ROOT / "negative-supplement-decisions.json")
    resume = commands.add_parser("resume", help="act as a Resumer under Section 14 on resume cases")
    resume.add_argument("cases", nargs="+", type=pathlib.Path)
    resume.add_argument("--out", type=pathlib.Path, default=OUTPUT_ROOT / "resume-decisions.json")
    blocks = commands.add_parser("negative-blocks",
                                 help="emit every block the negatives change and compare with tape-images/negatives/MANIFEST.tsv")
    blocks.add_argument("--negatives", type=pathlib.Path, default=BLIND_INPUTS / "negative-cases-blind.json")
    blocks.add_argument("--supplement", type=pathlib.Path, default=BLIND_INPUTS / "supplement-blind.json")
    blocks.add_argument("--negatives-e1", type=pathlib.Path, default=BLIND_INPUTS / "negatives-e1-blind.json")
    blocks.add_argument("--manifest", type=pathlib.Path, default=NEGATIVES_ROOT / "MANIFEST.tsv")
    blocks.add_argument("--out", type=pathlib.Path, default=OUTPUT_ROOT / "negative-block-digests.json")
    mutations_cmd = commands.add_parser("mutations", help="apply and decide the terminal-index mutations")
    mutations_cmd.add_argument("mutations", type=pathlib.Path)
    mutations_cmd.add_argument("--out", type=pathlib.Path, default=OUTPUT_ROOT / "mutation-decisions.json")
    selection_cmd = commands.add_parser("selection", help="build and decide the terminal survivor sets")
    selection_cmd.add_argument("selection", type=pathlib.Path)
    selection_cmd.add_argument("--out", type=pathlib.Path, default=OUTPUT_ROOT / "selection-decisions.json")
    args = parser.parse_args(argv)
    if args.command == "negative-blocks":
        output = run_negative_blocks(args.negatives, args.supplement, args.manifest, args.out, args.negatives_e1)
        for row in output["rows"]:
            if row["result"] != "matched":
                print(f"{row['result']:22} {row['case']} {row['artifact']} tape file {row['tape_file']} block "
                      f"{row['block_within_file']}" + (f" -- {row['detail']}" if row.get("detail") else ""))
        print("counts", json.dumps(output["counts"], sort_keys=True))
        print("sha256", hashlib.sha256(args.out.read_bytes()).hexdigest())
        return 0
    if args.command == "mutations":
        output = run_mutations(args.mutations, args.out)
        for entry_id, entry in output["entries"].items():
            print(entry_id, mutation_summary(entry))
        print("sha256", hashlib.sha256(args.out.read_bytes()).hexdigest())
        return 0
    if args.command == "selection":
        output = run_selection(args.selection, args.out)
        for entry_id, entry in output["entries"].items():
            print(entry_id, selection_summary(entry))
        print("sha256", hashlib.sha256(args.out.read_bytes()).hexdigest())
        return 0
    if args.command == "build":
        report = run_build(args.out, include_streaming=not args.skip_streaming)
        for result in report["results"]:
            print(f"{result['result']:10} {result['artifact']}" + (
                "" if result["result"] == "reproduced" else f"  -- {result['detail']}"))
        print(f"reproduced {report['reproduced']}, mismatched {report['mismatched']}")
        return 0 if report["mismatched"] == 0 else 1
    if args.command == "negatives-supplement":
        output = run_supplement(args.variants, args.out)
        for entry_id, entry in output["entries"].items():
            print(entry_id, supplement_summary(entry))
        print("sha256", hashlib.sha256(args.out.read_bytes()).hexdigest())
        return 0
    if args.command == "negative-e1":
        output = run_negatives(args.cases, args.out, e1_negatives_table())
        for entry_id, entry in output["entries"].items():
            print(entry_id, negative_summary(entry))
        print("sha256", hashlib.sha256(args.out.read_bytes()).hexdigest())
        return 0
    if args.command == "negatives":
        output = run_negatives(args.cases, args.out)
        for entry_id, entry in output["entries"].items():
            print(entry_id, negative_summary(entry))
        print("sha256", hashlib.sha256(args.out.read_bytes()).hexdigest())
        return 0
    if args.command == "resume":
        output = run_resume(args.cases, args.out)
        for case_id, decision in output["cases"].items():
            print(case_id, resume_summary(decision))
        print("sha256", hashlib.sha256(args.out.read_bytes()).hexdigest())
        return 0
    try:
        output = run_decide(args.cases, args.out)
    except FaultMapError as error:
        print(f"error: fault map refused: {error}", file=sys.stderr)
        return 2
    for case_id, decision in output["cases"].items():
        if "observations" in decision:
            for observation_id, observed in decision["observations"].items():
                print(f"{case_id} [{observation_id}]", one_line_summary(observed))
        else:
            print(case_id, one_line_summary(decision))
    print("sha256", hashlib.sha256(args.out.read_bytes()).hexdigest())
    return 0


def one_line_summary(decision: dict[str, Any]) -> str:
    scanner = decision["scanner"]
    parts = [f"{decision['image']}:", f"discovery {decision['discovery']['result']}"]
    part = f"scanner {scanner['result']}"
    if scanner["error"]:
        part += f" {scanner['error']}"
    if scanner["acceptable_selections"]:
        part += " [" + "/".join(scanner["acceptable_selections"]) + "]"
    if scanner["degraded"] is True:
        part += " degraded"
    elif scanner["degraded"] == "per reads":
        part += " degraded per reads"
    parts.append(part)
    if decision["walk"]["result"] != "not_run":
        parts.append(f"walk {decision['walk']['result']}" + (f" {decision['walk']['error']}" if decision["walk"]["error"] else ""))
    for outcome in decision["recoverer"]["addresses"]:
        text = f"{tuple(outcome['address'])} {outcome['result']}"
        if outcome["error"]:
            text += f" {outcome['error']}"
        parts.append(text)
    parts.append(f"verifier {decision['verifier']['result']}" +
                 (f" {decision['verifier']['error']}" if decision["verifier"]["error"] else ""))
    if decision["undecided"]:
        parts.append("undecided: " + ", ".join(item["aspect"] for item in decision["undecided"]))
    return "; ".join(parts)


if __name__ == "__main__":
    sys.exit(main())
