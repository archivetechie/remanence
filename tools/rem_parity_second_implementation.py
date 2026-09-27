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

A case file named ``fault-map.json`` or ``inputs.json`` takes its case id from
its directory.
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
    "hint_discovery": ("8.4", "A Scanner that cannot read the bootstrap MAY perform the discovery above when it is given the three values Section 8.4.1 names."),
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
    "replica_footer_type": ("12.3", "When the head is unreadable, a matching, fully parsed terminal footer may establish the same type after its measured count is checked."),
    "separation_footer_type": ("12.3", "A matching, fully parsed footer may establish the type when the head is unreadable."),
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
    "sidecar_copy_agree": ("9.1", "When both copies parse, a Reader MUST verify that they agree and reject divergence."),
    "parity_locator": ("9.1", "A Reader MUST locate a parity shard by computing `block(stripe, parity_index)` from its explicit fields, never by the position of its index entry."),
    "pm_copies": ("10.1.3", "When one header copy is invalid and the other is valid, the valid copy is used; when both are valid they MUST agree (Section 10.1.4)."),
    "pm_crc": ("10.1.3", "A Reader MUST verify the CRC-64/XZ at 0xC0 of each header copy and of the footer, and MUST NOT rely on a block whose CRC does not verify."),
    "tt2_walk_pm": ("D", "The walk route cannot find a ParityMap whose block 0 is unreadable; the replica route survives that damage by locating it through structural rows."),
    "selection_guide": ("C", "the selection order among agreeing replicas"),
    "bootstrap_supplied_refused": ("8.4", "A readable bootstrap whose values disagree with the supplied ones is refused; the supplied values never take its place."),
    "hint_walk_accept": ("8.4.1", "A Scanner MUST accept an expected tape UUID,"),
}


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


class ReadFailure(Exception):
    def __init__(self, error: str, reason: str) -> None:
        super().__init__(f"{error}: {reason}")
        self.error = error
        self.reason = reason


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
            if tape_file != previous[3] + 1:
                return "tape-file numbers not dense"
            if start != previous[4] + previous[5] + 1:
                return "start LBA does not advance by record_count + 1"
    if not (tuples[0][5] == tuples[2][5] == tuples[4][5]):
        return "replica record counts differ"
    if tuples[1][5] != tuples[3][5]:
        return "separation record counts differ"
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
    if payload_len > (1 << 64) - 1:
        problems.append("payload length overflows u64")
    payload_records = ceil_div(payload_len, block_size)
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
    if sidecars and watermark != total:
        problems.append("highest protected ordinal differs from total data ordinals")
    if not (frame["covered"] == len(entries) == replica_a_tape_file):
        problems.append("covered count, structural_row_count and A's tape-file number differ")
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

    Spacing back from EOD, the record before each filemark is the last record
    of a tape file. The first of those that parses as a terminal-replica
    footer supplies the planned layout. The search covers the five tape files
    of a terminal suffix.
    """
    for filemark in tape.filemark_positions_before(tape.eod())[:5]:
        position = filemark - 1
        if position < 0:
            continue
        try:
            record = tape.read_data(position)
            footer = parse_replica_frame(record, tape_uuid, block_size, 2)
        except (MediumError, ReadFailure):
            continue
        return footer["tuples"], f"replica footer at LBA {position} (replica ordinal {footer['ordinal']})"
    return None, "no terminal-replica footer found from EOD"


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
    if footer["total"] != 2 * footer["H"] + footer["P"] + 1 or footer["primary_start"] != 0 or \
            footer["tail_start"] != footer["H"] + footer["P"]:
        raise ReadFailure("SidecarParse", "footer locator arithmetic")
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


def classify_file(tape: DamagedTape, tape_file: int, start: int, count: int, tape_uuid: bytes,
                  block_size: int) -> WalkedFile:
    try:
        head = tape.read_data(start)
        head_error = None
    except MediumError as error:
        head = None
        head_error = f"head unreadable (medium error at LBA {error.args[0]})"

    def sidecar_footer_probe() -> WalkedFile | None:
        try:
            footer = parse_sidecar_footer(tape.read_data(start + count - 1), tape_uuid)
        except (MediumError, ReadFailure):
            return None
        if footer["total"] != count:
            return WalkedFile(tape_file, start, count, None,
                              f"classification failed: sidecar footer total {footer['total']} differs from the "
                              f"measured {count} blocks (hard error, Section 12.3 item 6)")
        try:
            tail = parse_sidecar_copy(read_blocks(tape, start + footer["tail_start"], footer["H"]), tape_uuid, block_size, 2)
            if not copy_matches_footer(tail, footer):
                return WalkedFile(tape_file, start, count, None, "classification failed: tail copy differs from footer")
            note = "sidecar by footer probe; tail copy verified against the footer"
        except MediumError:
            note = "sidecar by footer fields alone; tail copy unreadable"
        except ReadFailure as failure:
            return WalkedFile(tape_file, start, count, None, f"classification failed: tail copy: {failure.reason}")
        return WalkedFile(tape_file, start, count, KIND_SIDECAR, note, footer["epoch_id"], footer["start"], footer["end"])

    if head is not None:
        magic = head[0:8]
        if magic == BOOTSTRAP_MAGIC_BYTES:
            try:
                boot = parse_bootstrap(head, block_size)
                if boot["tape_uuid"] == tape_uuid and count == 1:
                    return WalkedFile(tape_file, start, count, KIND_BOOTSTRAP, "bootstrap")
            except ReadFailure:
                pass
            return WalkedFile(tape_file, start, count, None, "classification failed: bootstrap magic, frame invalid")
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
                return WalkedFile(tape_file, start, count, None, f"classification failed: ParityMap: {failure.reason}")
        if magic == role_magic(tape_uuid, LABEL_SIDECAR):
            try:
                header_blocks_guess = rd64(head, 0x60)
                if 1 <= header_blocks_guess <= count:
                    primary = parse_sidecar_copy(read_blocks(tape, start, header_blocks_guess), tape_uuid, block_size, 1)
                    if primary["total"] != count:
                        return WalkedFile(tape_file, start, count, None,
                                          "classification failed: primary header total differs from the measured "
                                          "count (hard error, Section 12.3 item 5)")
                    return WalkedFile(tape_file, start, count, KIND_SIDECAR, "sidecar by primary header",
                                      primary["epoch_id"], primary["start"], primary["end"])
            except (MediumError, ReadFailure):
                pass
        probe = sidecar_footer_probe()
        if probe is not None:
            return probe
        return WalkedFile(tape_file, start, count, KIND_OBJECT, "Object by elimination")
    # Unreadable head block.
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
    tape = ctx.tape
    start = lba_of_file(ctx.entries, sidecar.tape_file_number)
    count = sidecar.block_count
    notes = []
    citations.append(cite("footer_first"))
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
        copies = {}
        for name, first, kind in (("primary", 0, 1), ("tail", footer["tail_start"], 2)):
            try:
                copy = parse_sidecar_copy(read_blocks(tape, start + first, footer["H"]), ctx.tape_uuid, ctx.block_size, kind)
                if copy_matches_footer(copy, footer):
                    copies[name] = copy
                else:
                    notes.append(f"{name} copy differs from the footer")
            except MediumError as error:
                notes.append(f"{name} copy unreadable (LBA {error.args[0]})")
            except ReadFailure as failure:
                notes.append(f"{name} copy invalid ({failure.reason})")
        if len(copies) == 2 and copies["primary"]["metadata"] != copies["tail"]["metadata"]:
            citations.append(cite("sidecar_copy_agree"))
            return None, "SidecarParse: header copies disagree"
        if copies:
            chosen = "primary" if "primary" in copies else "tail"
            return copies[chosen], "; ".join(notes + [f"footer valid; {chosen} copy used"])
    citations.append(cite("primary_fallback"))
    try:
        head = tape.read_data(start)
        header_blocks = rd64(head, 0x60)
        if not 1 <= header_blocks <= count:
            raise ReadFailure("SidecarParse", "implausible H")
        primary = parse_sidecar_copy(read_blocks(tape, start, header_blocks), ctx.tape_uuid, ctx.block_size, 1)
        if primary["total"] != count:
            raise ReadFailure("SidecarParse", "primary total differs from the map entry")
        return primary, "; ".join(notes + ["primary copy used (fallback)"])
    except MediumError as error:
        notes.append(f"primary copy unreadable (LBA {error.args[0]})")
    except ReadFailure as failure:
        notes.append(f"primary copy invalid ({failure.reason})")
    citations.append(cite("tail_rescue"))
    citations.append(cite("entry_available"))
    entry = ctx.directory.get(sidecar.tape_file_number) if ctx.directory is not None else None
    if entry is None:
        notes.append("no sidecar epoch directory entry is available (no final ParityMap validates)")
    elif (entry["epoch_id"], entry["start"], entry["end"], entry["total"]) != (
            sidecar.epoch_id, sidecar.protected_ordinal_start, sidecar.protected_ordinal_end_exclusive, count):
        citations.append(cite("entry_agrees"))
        notes.append("directory entry disagrees with the map entry; no read placed from it")
    else:
        citations.append(cite("entry_agrees"))
        tail_first = entry["total"] - 1 - entry["H"]
        try:
            tail = parse_sidecar_copy(read_blocks(tape, start + tail_first, entry["H"]), ctx.tape_uuid, ctx.block_size, 2)
            if tail["hash"] != entry["hash"]:
                raise ReadFailure("SidecarParse", "tail hash differs from the directory entry")
            return tail, "; ".join(notes + ["tail copy used (directory-assisted rescue)"])
        except MediumError as error:
            notes.append(f"tail copy unreadable (LBA {error.args[0]})")
        except ReadFailure as failure:
            notes.append(f"tail copy invalid ({failure.reason})")
    citations.append(cite("metadata_unavailable"))
    return None, "; ".join(notes)


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
        result.update(result="error", error="SidecarMetadataUnavailable" if "disagree" not in note else "SidecarParse")
        if result["error"] == "SidecarMetadataUnavailable":
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
    "resume_step1": ("14", "Derive the committed prefix from the off-tape commit records (Section 3.4), dropping any torn tail, and compute `W` and `T` from it."),
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

    def refuse(at: str, error: str, citations: list[str]) -> tuple[dict[str, Any], None]:
        refusal.update(result="refused", error=error, refused_at=at, before_any_tape_read=not reads,
                       before_any_write=True, records_read=list(reads))
        refusal["citations"].extend(cite(key) for key in citations)
        return decision, None

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
            value = "medium error"
        if not isinstance(value, bytes) or len(value) != boot["block_size"]:
            found = value if isinstance(value, str) else f"a {len(value)}-byte record"
            step3.update(result="fatal", failure=f"ordinal {ordinal} at LBA {lba}: {found} where data is expected")
            step3["citations"].append(cite("boundary_outcomes"))
            decision["undecided"].append({
                "aspect": "decision.error",
                "readings": [
                    "ResumeAppend: the committed prefix does not describe the tape, a Section 14 invariant violation",
                    "TapeIo: a Filemark or EndOfData outcome where a block was expected is a tape I/O failure, "
                    "not a format violation",
                ],
                "citations": [cite("resume_step3"), cite("err_resume"), cite("err_tapeio")],
            })
            return refuse("step 3", "undecided", ["resume_step3"])
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
# Deciding one case.
# ---------------------------------------------------------------------------


def case_id_of(path: pathlib.Path) -> str:
    return path.parent.name if path.name in ("fault-map.json", "inputs.json") else path.stem


def damaged_tape_for(image: ImageBuild, case: Mapping[str, Any]) -> tuple[DamagedTape, list[str]]:
    """Apply the case's faults to my undamaged build (the fault model is given, not derived)."""
    records = image.records()
    notes = []
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


def discover_bootstrap(tape: DamagedTape, hints: dict[str, Any] | None, decision: dict[str, Any],
                       trace: dict[str, Any]) -> dict[str, Any] | None:
    discovery = decision["discovery"]
    discovery["citations"].append(cite("boot_first"))
    sizes = [hints["block_size"]] if hints else list(DISCOVERY_CANDIDATES)
    discovery["citations"].append(cite("hint_size" if hints else "candidates"))
    attempts = []
    for size in sizes:
        try:
            block = tape.read_data(0)
            if len(block) != size:
                attempts.append(f"read size {size}: the record is {len(block)} bytes, not exactly one block")
                continue
            boot = parse_bootstrap(block, size)
            attempts.append(f"read size {size}: bootstrap parses")
            trace["bootstrap_attempts"] = attempts
            discovery.update(result="found", block_size=boot["block_size"])
            discovery["citations"].append(cite("accept_size"))
            if hints:
                discovery["used_hints"] = True
                if (boot["tape_uuid"], boot["block_size"], boot["scheme"]) != (
                        hints["tape_uuid"], hints["block_size"], hints["scheme"]):
                    discovery.update(result="error", error="NoBootstrapFound")
                    discovery["citations"].append(cite("bootstrap_supplied_refused"))
                    return None
            return boot
        except MediumError as error:
            attempts.append(f"read size {size}: medium error at LBA {error.args[0]}")
        except ReadFailure as failure:
            attempts.append(f"read size {size}: {failure.error}: {failure.reason}")
            if failure.error in ("DriveCompressionEnabled", "BootstrapParse"):
                trace["bootstrap_attempts"] = attempts
                discovery.update(result="error", error=failure.error)
                return None
    trace["bootstrap_attempts"] = attempts
    discovery["result"] = "not_found"
    if hints:
        discovery.update(used_hints=True, block_size=hints["block_size"])
        discovery["citations"].extend([cite("hint_discovery"), cite("hint_uuid"), cite("identity_12_1")])
        return None
    discovery["error"] = "NoBootstrapFound"
    discovery["citations"].extend([cite("no_bootstrap"), cite("hints_required")])
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
                             directory: dict[int, dict] | None) -> list[str]:
    """What a Verifier finds before replica A: unreadable records, failed copies, CRC and scheme checks."""
    findings = []
    if bootstrap is None:
        findings.append("bootstrap unreadable or absent: its frame and trailing fill cannot be verified (TapeIo)")
    elif not bootstrap["fill_zero"]:
        findings.append("bootstrap trailing fill nonzero (nonconformity)")
    indexes: dict[int, dict] = {}
    for entry in entries[:scope]:
        start = lba_of_file(entries, entry.tape_file_number)
        if entry.kind == KIND_SIDECAR:
            ctx = RecoveryContext(tape, tape_uuid, block_size, scheme, entries, scope, 0, directory, "verifier")
            index, note = acquire_index(ctx, entry, [])
            if "unreadable" in note or "invalid" in note or index is None:
                findings.append(f"sidecar tape file {entry.tape_file_number}: {note}")
            if index is not None:
                k, m, stripes = scheme
                if (index["k"], index["m"], index["S"], index["block_size"]) != (k, m, stripes, block_size):
                    findings.append(f"sidecar tape file {entry.tape_file_number}: SchemeMismatch against the "
                                    "bootstrap or supplied scheme")
                indexes[entry.tape_file_number] = index
                for block in range(entry.block_count):
                    if index["H"] <= block < index["H"] + index["P"]:
                        shard = block - index["H"]
                        key = (shard % index["S"], shard // index["S"])
                        try:
                            if crc64_xz(tape.read_data(start + block)) != index["parity_crcs"][key]:
                                findings.append(f"parity shard at LBA {start + block}: CRC mismatch")
                        except MediumError:
                            findings.append(f"parity shard at LBA {start + block}: unreadable (TapeIo)")
        elif entry.kind == KIND_PARITY_MAP:
            try:
                parity_map = parse_parity_map(tape, start, entry.block_count, tape_uuid, block_size)
                for name, reason in parity_map["copy_failures"].items():
                    findings.append(f"ParityMap {name} copy: {reason}")
            except ReadFailure as failure:
                findings.append(f"ParityMap tape file {entry.tape_file_number}: {failure.error}: {failure.reason}")
    for entry in entries[:scope]:
        if entry.kind != KIND_OBJECT:
            continue
        start = lba_of_file(entries, entry.tape_file_number)
        for block in range(entry.block_count):
            ordinal = entry.first_parity_data_ordinal + block
            covering = [e for e in entries[:scope] if e.kind == KIND_SIDECAR
                        and e.protected_ordinal_start <= ordinal < e.protected_ordinal_end_exclusive]
            try:
                data = tape.read_data(start + block)
            except MediumError:
                findings.append(f"Object tape file {entry.tape_file_number} block {block} (LBA {start + block}): "
                                "unreadable (TapeIo)")
                continue
            if covering and covering[0].tape_file_number in indexes:
                index = indexes[covering[0].tape_file_number]
                if crc64_xz(data) != index["data_crcs"][ordinal - index["start"]]:
                    findings.append(f"Object block at LBA {start + block}: CRC mismatch")
            elif covering:
                findings.append(f"Object block at LBA {start + block}: CRC not checkable (epoch metadata unavailable)")
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
    if boot is not None:
        tape_uuid, block_size, scheme = boot["tape_uuid"], boot["block_size"], boot["scheme"]
    elif hints is not None and decision["discovery"]["result"] == "not_found":
        tape_uuid, block_size, scheme = hints["tape_uuid"], hints["block_size"], hints["scheme"]
    else:
        error = decision["discovery"]["error"]
        scanner = decision["scanner"]
        scanner.update(result="error", error=error)
        scanner["citations"].extend([cite("boot_first"), cite("no_bootstrap")])
        decision["walk"]["citations"].append(cite("hints_required"))
        verifier = decision["verifier"]
        verifier.update(result="error", error=error)
        verifier["citations"].extend([cite("verifier_role"), cite("no_bootstrap")])
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
            scanner["replicas"][letter] = {"valid": False, "reason": "no planned layout: no replica footer found from EOD"}
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
                scanner["degraded"] = None
                decision["undecided"].append({
                    "aspect": "scanner.degraded",
                    "readings": [
                        f"true: the Scanner validates the payload of replica {', '.join(payload_only_bad)} and finds "
                        "it unreadable, so a sibling is invalid and the result is degraded",
                        f"false: the Scanner accepts {fully_valid[0]} after validating only that payload, as Section "
                        f"8.4 permits, never reads replica {', '.join(payload_only_bad)}'s payload, and reports no "
                        "degraded evidence",
                    ],
                    "citations": [cite("validate_every"), cite("degraded_result"), cite("selection_guide")],
                })
            else:
                scanner["degraded"] = False
            scanner["inventory_equals_true_prefix"] = None  # filled after the decision

    # ----- BOT structural walk -----
    walk = decision["walk"]
    walk_map: dict[str, Any] | None = None
    if not walk_needed:
        walk["citations"].append(cite("no_replica_walk"))
    if walk_needed:
        walk["citations"].extend([cite("walk_offer"), cite("walk_file")])
        files, damage = bot_walk(tape, tape_uuid, block_size)
        walk_map = second_pass_and_map(files)
        trace["walk"] = [dataclasses.asdict(f) | {"parity_map": None} for f in files]
        trace["walk_damage"] = damage
        trace["walk_map"] = walk_map["reason"]
        walk["classes"] = {str(f.tape_file_number): (KIND_NAMES[f.kind] if f.kind is not None else "classification failed")
                           for f in files}
        by_footer = [f for f in files if "type established by its footer" in f.note]
        if by_footer:
            for f in by_footer:
                walk["classes"][str(f.tape_file_number)] = "undecided"
            decision["undecided"].append({
                "aspect": "walk.classes",
                "readings": [
                    "tape files " + ", ".join(f"{f.tape_file_number} ({KIND_NAMES[f.kind]})" for f in by_footer)
                    + ": the head is unreadable and a matching, fully parsed footer establishes the type, as "
                    "Section 12.3 permits",
                    "tape files " + ", ".join(str(f.tape_file_number) for f in by_footer)
                    + ": Object candidates, because a Scanner that does not use that permission must classify an "
                    "unreadable-head file it cannot otherwise recognise as an object candidate",
                ],
                "citations": [cite("replica_footer_type"), cite("separation_footer_type"), cite("unreadable_head")],
            })
        notes_text = " ".join(f.note for f in files)
        if "head unreadable" in notes_text:
            walk["citations"].append(cite("unreadable_head"))
        if "type established by its footer" in notes_text:
            walk["citations"].extend([cite("replica_footer_type"), cite("separation_footer_type")])
        if walk_map["identified"]:
            walk["citations"].append(cite("second_pass"))
        if any(f.kind is None for f in files):
            walk["citations"].extend([cite("footer_probe"), cite("hard_error_scope"), cite("item7")])
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
        verifier["separations"] = {"A-B": "not_run", "B-C": "not_run"}
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
                                            boot, directory)
        trace["verifier_prefix_findings"] = findings
        if findings:
            decision["undecided"].append({
                "aspect": "verifier.outside_terminal_suffix",
                "readings": [
                    "the Verifier reports each finding before replica A (" + "; ".join(findings) + ") and its "
                    "terminal-suffix result stands",
                    "the findings make the Verifier's end-to-end result a failure; the text names no Verifier "
                    "outcome for them",
                ],
                "citations": [cite("verifier_role"), cite("io_distinct")],
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
    for path in sorted(case_paths, key=lambda p: case_id_of(p)):
        case = load_json(path)
        name = case["image"]
        if name not in images:
            images[name] = build_image(load_image_inputs(name), name)
            unreproduced_by_image[name] = [
                result.artifact.split(" tape file ")[1] if " tape file " in result.artifact else result.artifact
                for result in compare_image(images[name], manifest_rows) if not result.reproduced]
        image = images[name]
        trace: dict[str, Any] = {}
        decision = decide_case(case, image, trace)
        reporting_only(decision, image, trace)
        decision["image_unreproduced"] = [f"tape file {item}" for item in unreproduced_by_image[name]]
        decisions[case_id_of(path)] = decision
        traces[case_id_of(path)] = trace
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
    resume = commands.add_parser("resume", help="act as a Resumer under Section 14 on resume cases")
    resume.add_argument("cases", nargs="+", type=pathlib.Path)
    resume.add_argument("--out", type=pathlib.Path, default=OUTPUT_ROOT / "resume-decisions.json")
    args = parser.parse_args(argv)
    if args.command == "build":
        report = run_build(args.out, include_streaming=not args.skip_streaming)
        for result in report["results"]:
            print(f"{result['result']:10} {result['artifact']}" + (
                "" if result["result"] == "reproduced" else f"  -- {result['detail']}"))
        print(f"reproduced {report['reproduced']}, mismatched {report['mismatched']}")
        return 0 if report["mismatched"] == 0 else 1
    if args.command == "resume":
        output = run_resume(args.cases, args.out)
        for case_id, decision in output["cases"].items():
            print(case_id, resume_summary(decision))
        print("sha256", hashlib.sha256(args.out.read_bytes()).hexdigest())
        return 0
    output = run_decide(args.cases, args.out)
    for case_id, decision in output["cases"].items():
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
    if scanner["degraded"]:
        part += " degraded"
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
