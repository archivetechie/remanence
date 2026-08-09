#!/usr/bin/env python3
"""Independently verify draft terminal-index bytes and hostile matrices.

This implementation intentionally does not import or execute the Rust codec. It
re-derives the fixed frames, digest domains, deterministic CBOR rows, streamed
large-count payload, local footer bindings, survivor selection, and compact
mutations directly from the draft byte tables.
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import hmac
import re
import struct
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import Any


LEGAL_BLOCK_SIZES = (256 * 1024, 512 * 1024, 1024 * 1024)
REPLICA_HEADER_LABEL = b"REM\0TIREP\x01H"
REPLICA_FOOTER_LABEL = b"REM\0TIREP\x01F"
GAP_HEADER_LABEL = b"REM\0TISEP\x01H"
GAP_FOOTER_LABEL = b"REM\0TISEP\x01F"
PAYLOAD_DOMAIN = b"REM-TAPE-INDEX-REPLICA-PAYLOAD-V1\0"
EDITION_DOMAIN = b"REM-TAPE-INDEX-EDITION-V1\0"
REPLICA_DESCRIPTOR_DOMAIN = b"REM-TAPE-INDEX-REPLICA-DESCRIPTOR-V1\0"
LAYOUT_DOMAIN = b"REM-TERMINAL-TAIL-LAYOUT-V1\0"
GAP_DESCRIPTOR_DOMAIN = b"REM-INDEX-SEPARATION-DESCRIPTOR-V1\0"
CRC64_XZ_POLY = 0xC96C5795D7870F42
MASK64 = (1 << 64) - 1
COMPONENTS = (
    ("replica-a.bin", 4, 1),
    ("gap-ab.bin", 5, 1),
    ("replica-b.bin", 4, 2),
    ("gap-bc.bin", 5, 2),
    ("replica-c.bin", 4, 3),
)
PROGRESS = (
    "BeforeReplicaA",
    "AfterReplicaA",
    "AfterSeparationAb",
    "AfterReplicaB",
    "AfterSeparationBc",
    "AfterReplicaC",
)


class VectorError(ValueError):
    """A typed independent-verifier rejection."""

    def __init__(self, code: str, message: str):
        super().__init__(f"{code}: {message}")
        self.code = code


@dataclass(frozen=True)
class CborMap:
    """Preserve deterministic CBOR map ordering and duplicate visibility."""

    pairs: tuple[tuple[Any, Any], ...]


@dataclass(frozen=True)
class ProfileContext:
    """Expected healthy envelope facts pinned by one manifest profile."""

    name: str
    directory: Path
    block_size: int
    tape_uuid: bytes
    edition_id: bytes
    edition_sequence: int
    structural_rows: int
    object_rows: int
    edition_digest: bytes
    layout_digest: bytes
    payload_digest: bytes
    map_digest: bytes
    expected_eod: int
    tuples: tuple[tuple[int, int, int, int, int], ...]


@dataclass(frozen=True)
class ReplicaSummary:
    """Cross-survivor agreement facts from one independently valid member."""

    ordinal: int
    tape_uuid: bytes
    edition_id: bytes
    edition_sequence: int
    scope: tuple[int, int, int]
    counts: tuple[int, int]
    payload_digest: bytes
    map_digest: bytes
    edition_digest: bytes
    layout_digest: bytes
    block_size: int
    writer_version: bytes
    write_timestamp: bytes

    def agreement_key(self) -> tuple[Any, ...]:
        return (
            self.tape_uuid,
            self.edition_id,
            self.edition_sequence,
            self.scope,
            self.counts,
            self.payload_digest,
            self.map_digest,
            self.edition_digest,
            self.layout_digest,
            self.block_size,
            self.writer_version,
            self.write_timestamp,
        )


def fail(code: str, message: str) -> None:
    raise VectorError(code, message)


def require(condition: bool, code: str, message: str) -> None:
    if not condition:
        fail(code, message)


def read_tsv(path: Path) -> list[dict[str, str]]:
    with path.open(newline="", encoding="utf-8") as handle:
        return list(csv.DictReader(handle, delimiter="\t"))


def crc64_xz(data: bytes) -> int:
    crc = MASK64
    for byte in data:
        crc ^= byte
        for _ in range(8):
            crc = (crc >> 1) ^ (CRC64_XZ_POLY if crc & 1 else 0)
    return crc ^ MASK64


def u16(data: bytes, offset: int) -> int:
    return struct.unpack_from("<H", data, offset)[0]


def u32(data: bytes, offset: int) -> int:
    return struct.unpack_from("<I", data, offset)[0]


def u64(data: bytes, offset: int) -> int:
    return struct.unpack_from("<Q", data, offset)[0]


def magic(tape_uuid: bytes, label: bytes) -> bytes:
    return hmac.new(tape_uuid, label, hashlib.sha256).digest()[:8]


def checked_add(left: int, right: int, context: str) -> int:
    value = left + right
    require(value <= MASK64, "arithmetic-overflow", context)
    return value


def checked_mul(left: int, right: int, context: str) -> int:
    value = left * right
    require(value <= MASK64, "arithmetic-overflow", context)
    return value


def verify_crc(record: bytes, frame_len: int, crc_offset: int, name: str) -> None:
    require(
        record[frame_len:] == bytes(len(record) - frame_len),
        "frame-padding",
        f"{name}: nonzero record padding",
    )
    expected = u64(record, crc_offset)
    actual = crc64_xz(record[:crc_offset])
    code = "crc-header" if "header" in name else "crc-footer"
    require(actual == expected, code, f"{name}: CRC64/XZ mismatch")


def ensure_zero(data: bytes, code: str, name: str) -> None:
    require(data == bytes(len(data)), code, f"{name}: reserved bytes are nonzero")


def tuple_at(frame: bytes, offset: int) -> tuple[int, int, int, int, int]:
    kind, ordinal, filemark_count = struct.unpack_from("<HHI", frame, offset)
    require(filemark_count == 1, "missing-filemark", f"component tuple {offset:#x}")
    return kind, ordinal, u64(frame, offset + 8), u64(frame, offset + 16), u64(frame, offset + 24)


def encode_tuple(component: tuple[int, int, int, int, int]) -> bytes:
    kind, ordinal, tape_file, start, records = component
    return struct.pack("<HHIQQQ", kind, ordinal, 1, tape_file, start, records)


def layout_digest(block_size: int, tuples: tuple[tuple[int, int, int, int, int], ...], eod: int) -> bytes:
    digest = hashlib.sha256()
    digest.update(LAYOUT_DOMAIN)
    digest.update(struct.pack("<IIH", 0, block_size, 5))
    for component in tuples:
        digest.update(encode_tuple(component))
    digest.update(struct.pack("<Q", eod))
    return digest.digest()


def validate_layout(
    block_size: int,
    tuples: tuple[tuple[int, int, int, int, int], ...],
    eod: int,
) -> None:
    require(block_size in LEGAL_BLOCK_SIZES, "wrong-block-size", f"unsupported {block_size}")
    require(len(tuples) == 5, "layout-digest", "terminal layout does not have five components")
    for index, (component, (_, expected_kind, expected_ordinal)) in enumerate(zip(tuples, COMPONENTS)):
        kind, ordinal, tape_file, start, records = component
        require(
            (kind, ordinal) == (expected_kind, expected_ordinal),
            "layout-digest",
            f"component {index} kind/ordinal",
        )
        require(records >= 2, "wrong-count", f"component {index} shorter than header/footer")
        if index:
            prior = tuples[index - 1]
            require(tape_file == prior[2] + 1, "layout-digest", "non-dense terminal files")
            require(
                start == checked_add(checked_add(prior[3], prior[4], "component end"), 1, "filemark"),
                "layout-digest",
                "non-contiguous terminal positions",
            )
    require(
        eod == checked_add(checked_add(tuples[-1][3], tuples[-1][4], "C end"), 1, "C filemark"),
        "layout-digest",
        "wrong terminal EOD",
    )


def cbor_head(major: int, argument: int) -> bytes:
    require(0 <= argument <= MASK64, "cbor", "CBOR argument outside u64")
    initial = major << 5
    if argument < 24:
        return bytes([initial | argument])
    if argument <= 0xFF:
        return bytes([initial | 24, argument])
    if argument <= 0xFFFF:
        return bytes([initial | 25]) + struct.pack(">H", argument)
    if argument <= 0xFFFFFFFF:
        return bytes([initial | 26]) + struct.pack(">I", argument)
    return bytes([initial | 27]) + struct.pack(">Q", argument)


def cbor_encode(value: Any) -> bytes:
    if value is None:
        return b"\xf6"
    if isinstance(value, int):
        return cbor_head(0, value)
    if isinstance(value, bytes):
        return cbor_head(2, len(value)) + value
    if isinstance(value, str):
        encoded = value.encode("utf-8")
        return cbor_head(3, len(encoded)) + encoded
    if isinstance(value, (tuple, list)):
        return cbor_head(4, len(value)) + b"".join(cbor_encode(item) for item in value)
    if isinstance(value, CborMap):
        return cbor_head(5, len(value.pairs)) + b"".join(
            cbor_encode(key) + cbor_encode(item) for key, item in value.pairs
        )
    fail("cbor", f"unsupported CBOR value {type(value).__name__}")


def decode_argument(data: bytes, cursor: int, additional: int) -> tuple[int, int]:
    if additional < 24:
        return additional, cursor
    sizes = {24: 1, 25: 2, 26: 4, 27: 8}
    require(additional in sizes, "cbor", "indefinite or reserved CBOR argument")
    size = sizes[additional]
    require(cursor + size <= len(data), "slot-length", "truncated CBOR argument")
    argument = int.from_bytes(data[cursor : cursor + size], "big")
    minima = {24: 24, 25: 0x100, 26: 0x1_0000, 27: 0x1_0000_0000}
    require(argument >= minima[additional], "cbor", "non-canonical CBOR argument width")
    return argument, cursor + size


def cbor_decode(data: bytes, cursor: int = 0) -> tuple[Any, int]:
    require(cursor < len(data), "slot-length", "truncated CBOR value")
    initial = data[cursor]
    cursor += 1
    major, additional = initial >> 5, initial & 0x1F
    if major == 7 and additional == 22:
        return None, cursor
    argument, cursor = decode_argument(data, cursor, additional)
    if major == 0:
        return argument, cursor
    if major in (2, 3):
        require(cursor + argument <= len(data), "slot-length", "truncated CBOR string")
        value = data[cursor : cursor + argument]
        cursor += argument
        if major == 3:
            try:
                return value.decode("utf-8"), cursor
            except UnicodeDecodeError as error:
                fail("cbor", f"invalid UTF-8: {error}")
        return value, cursor
    if major == 4:
        values = []
        for _ in range(argument):
            value, cursor = cbor_decode(data, cursor)
            values.append(value)
        return tuple(values), cursor
    if major == 5:
        pairs = []
        prior_key = -1
        for _ in range(argument):
            key, cursor = cbor_decode(data, cursor)
            require(isinstance(key, int), "cbor", "Object-row key is not an unsigned integer")
            require(key > prior_key, "cbor", "Object-row keys are duplicate or noncanonical")
            prior_key = key
            value, cursor = cbor_decode(data, cursor)
            pairs.append((key, value))
        return CborMap(tuple(pairs)), cursor
    fail("cbor", f"unsupported CBOR major type {major}")


def decode_slot(slot: bytes, expected_size: int, label: str) -> tuple[Any, bytes]:
    require(len(slot) == expected_size, "slot-length", f"{label}: wrong slot size")
    encoded_len = u16(slot, 0)
    require(0 < encoded_len <= expected_size - 2, "slot-length", f"{label}: encoded length")
    encoded = slot[2 : 2 + encoded_len]
    ensure_zero(slot[2 + encoded_len :], "slot-padding", label)
    value, cursor = cbor_decode(encoded)
    require(cursor == len(encoded), "cbor", f"{label}: trailing CBOR bytes")
    require(cbor_encode(value) == encoded, "cbor", f"{label}: non-deterministic encoding")
    return value, encoded


def object_fields(value: Any, block_size: int) -> tuple[int, int]:
    require(isinstance(value, CborMap), "map-row-bijection", "Object row is not a map")
    fields = dict(value.pairs)
    representation = fields.get(2)
    expected = (
        {1, 2, 3, 4, 10, 11, 12, 13}
        if representation == "plaintext"
        else {1, 2, 3, 4, 21, 22, 23}
        if representation == "encrypted"
        else set()
    )
    require(set(fields) == expected, "map-row-bijection", "Object row schema")
    tape_file, stored = fields[1], fields[3]
    require(isinstance(tape_file, int) and isinstance(stored, int) and stored > 0, "map-row-bijection", "Object locator")
    object_id = fields[4]
    require(
        isinstance(object_id, bytes) and 1 <= len(object_id) <= 64 and b"\0" not in object_id,
        "map-row-bijection",
        "Object identifier bounds",
    )
    if representation == "plaintext":
        first, size, count, digest = fields[10], fields[11], fields[12], fields[13]
        require(all(isinstance(item, int) for item in (first, size, count)), "map-row-bijection", "manifest integer")
        require(size > 0 and count > 0, "map-row-bijection", "manifest size/count")
        require(checked_add(first, count, "manifest range") <= stored, "map-row-bijection", "manifest range")
        require(size <= checked_mul(count, block_size, "manifest capacity"), "map-row-bijection", "manifest capacity")
        require(isinstance(digest, bytes) and len(digest) == 32, "map-row-bijection", "manifest digest")
    else:
        metadata, recipients, key_len = fields[21], fields[22], fields[23]
        require(isinstance(metadata, int) and 17 <= metadata <= 16 * 1024 * 1024, "map-row-bijection", "metadata length")
        require(isinstance(recipients, tuple) and 1 <= len(recipients) <= 8, "map-row-bijection", "recipient count")
        require(
            all(isinstance(item, bytes) and len(item) == 16 and item != bytes(16) for item in recipients)
            and len(set(recipients)) == len(recipients),
            "map-row-bijection",
            "recipient IDs",
        )
        require(isinstance(key_len, int) and 1191 <= key_len <= 16_384, "map-row-bijection", "key-frame length")
    return tape_file, stored


def validate_payload(data: bytes, header: bytes, block_size: int) -> tuple[bytes, bytes]:
    structural_count = u64(header, 0x60)
    object_count = u64(header, 0x68)
    structural_bytes = checked_mul(structural_count, 64, "structural slot bytes")
    object_bytes = checked_mul(object_count, 256, "Object-row slot bytes")
    payload_len = checked_add(structural_bytes, object_bytes, "payload bytes")
    require(payload_len == u64(header, 0x70), "wrong-count", "declared payload length")
    payload_records = (payload_len + block_size - 1) // block_size if payload_len else 0
    require(payload_records == u64(header, 0x78), "wrong-count", "payload record count")
    require(payload_records + 2 == u64(header, 0x80), "wrong-count", "replica record count")
    require(payload_records + 1 == u64(header, 0x98), "wrong-count", "footer offset")
    require(len(data) // block_size == payload_records + 2, "wrong-length", "replica physical record count")
    payload_records_bytes = data[block_size : block_size * (1 + payload_records)]
    require(
        payload_records_bytes[payload_len:] == bytes(len(payload_records_bytes) - payload_len),
        "payload-padding",
        "nonzero payload-record padding",
    )
    payload = payload_records_bytes[:payload_len]

    map_encodings = []
    object_locators: list[tuple[int, int]] = []
    expected_file = 0
    expected_data = 0
    expected_protected = 0
    expected_epoch = 0
    prefix_end = 0
    for index in range(structural_count):
        slot = payload[index * 64 : (index + 1) * 64]
        value, encoded = decode_slot(slot, 64, "structural row")
        require(isinstance(value, tuple) and len(value) == 7, "map-row-bijection", "structural row shape")
        tape_file, kind, blocks, first, start, end, epoch = value
        require(all(isinstance(item, int) for item in (tape_file, kind, blocks)), "map-row-bijection", "structural integer")
        require(tape_file == expected_file and blocks > 0, "map-row-bijection", "dense structural map")
        require(kind in (0, 1, 2, 3), "map-row-bijection", "terminal kind in pre-tail map")
        require(index != 0 or (kind == 2 and blocks == 1), "map-row-bijection", "row zero is not BOT Bootstrap")
        if kind == 0:
            require(first == expected_data and start is None and end is None and epoch is None, "map-row-bijection", "Object kind fields")
            object_locators.append((tape_file, blocks))
            expected_data = checked_add(expected_data, blocks, "data ordinal range")
        elif kind == 1:
            require(first is None and start == expected_protected and isinstance(end, int) and end > start and epoch == expected_epoch, "map-row-bijection", "sidecar range")
            expected_protected = end
            expected_epoch += 1
        else:
            require(first is None and start is None and end is None and epoch is None, "map-row-bijection", "control kind fields")
            require(kind != 2 or blocks == 1, "map-row-bijection", "Bootstrap block count")
        prefix_end = checked_add(prefix_end, checked_add(blocks, 1, "file span"), "covered prefix")
        expected_file += 1
        map_encodings.append(encoded)

    require((u64(header, 0x48), u64(header, 0x50), u64(header, 0x58)) == (structural_count, expected_data, expected_protected), "wrong-scope", "scope disagrees with rows")
    row_locators = []
    object_base = structural_count * 64
    for index in range(object_count):
        slot = payload[object_base + index * 256 : object_base + (index + 1) * 256]
        value, _ = decode_slot(slot, 256, "Object row")
        row_locators.append(object_fields(value, block_size))
    require(row_locators == object_locators, "map-row-bijection", "map and Object-row locators disagree")
    require(
        prefix_end == tuple_at(header, 0x148)[3],
        "wrong-start",
        "prefix end/replica A start mismatch",
    )

    payload_digest = hashlib.sha256(PAYLOAD_DOMAIN + payload).digest()
    map_projection = cbor_head(4, structural_count) + b"".join(map_encodings)
    map_digest = hashlib.sha256(map_projection).digest()
    return payload_digest, map_digest


def edition_digest(frame: bytes) -> bytes:
    writer_len, timestamp_len = u16(frame, 0x1F0), u16(frame, 0x1F2)
    writer = frame[0x1F8 : 0x1F8 + writer_len]
    timestamp = frame[0x278 : 0x278 + timestamp_len]
    digest = hashlib.sha256()
    digest.update(EDITION_DOMAIN)
    digest.update(struct.pack("<H", 1))
    digest.update(frame[0x10:0x30])
    digest.update(frame[0x30:0x38])
    digest.update(frame[0x3C:0x48])
    digest.update(frame[0x48:0x80])
    digest.update(frame[0xA8:0xE8])
    digest.update(struct.pack("<Q", len(writer)) + writer)
    digest.update(struct.pack("<Q", len(timestamp)) + timestamp)
    return digest.digest()


def replica_descriptor_digest(
    frame: bytes,
    tuples: tuple[tuple[int, int, int, int, int], ...],
    ordinal: int,
) -> bytes:
    digest = hashlib.sha256()
    digest.update(REPLICA_DESCRIPTOR_DOMAIN)
    digest.update(frame[0xE8:0x128])
    digest.update(struct.pack("<HH", ordinal, 3))
    digest.update(encode_tuple(tuples[(ordinal - 1) * 2]))
    digest.update(frame[0x98:0xA0])
    return digest.digest()


def diagnostic_fields(frame: bytes) -> tuple[bytes, bytes]:
    writer_len, timestamp_len = u16(frame, 0x1F0), u16(frame, 0x1F2)
    require(writer_len <= 128 and timestamp_len <= 64, "reserved-nonzero", "diagnostic length")
    ensure_zero(frame[0x1F4:0x1F8], "reserved-nonzero", "diagnostic alignment")
    writer = frame[0x1F8 : 0x1F8 + writer_len]
    timestamp = frame[0x278 : 0x278 + timestamp_len]
    require(all(0x20 <= byte <= 0x7E for byte in writer), "reserved-nonzero", "writer version charset")
    ensure_zero(frame[0x1E8:0x1F0], "reserved-nonzero", "fixed diagnostic reserved")
    try:
        timestamp_text = timestamp.decode("ascii")
        require(
            re.fullmatch(
                r"\d{4}-\d{2}-\d{2}[Tt]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:[Zz]|[+-]\d{2}:\d{2})",
                timestamp_text,
            )
            is not None,
            "reserved-nonzero",
            "timestamp is not RFC3339",
        )
        datetime.fromisoformat(timestamp_text.replace("Z", "+00:00").replace("z", "+00:00"))
    except (UnicodeDecodeError, ValueError) as error:
        fail("reserved-nonzero", f"timestamp is not RFC3339: {error}")
    ensure_zero(frame[0x1F8 + writer_len : 0x278], "reserved-nonzero", "writer padding")
    ensure_zero(frame[0x278 + timestamp_len : 0x2B8], "reserved-nonzero", "timestamp padding")
    return writer, timestamp


def verify_replica(data: bytes, context: ProfileContext, ordinal: int) -> ReplicaSummary:
    block_size = context.block_size
    require(len(data) % block_size == 0 and len(data) >= 2 * block_size, "wrong-length", "replica byte length")
    header, footer = data[:block_size], data[-block_size:]
    require(header[:8] == magic(context.tape_uuid, REPLICA_HEADER_LABEL), "wrong-tape", "replica header magic")
    require(footer[:8] == magic(context.tape_uuid, REPLICA_FOOTER_LABEL), "mixed-header-footer", "replica footer magic")
    verify_crc(header, 0x400, 0x3F8, "replica header")
    verify_crc(footer, 0x400, 0x3F8, "replica footer")
    require(header[0x0C:0x2B8] == footer[0x0C:0x2B8], "mixed-header-footer", "replica common frames")
    require((u16(header, 8), u16(header, 0x0A), u32(header, 0x0C)) == (1, 1, 1), "mixed-header-footer", "header role/version")
    require((u16(footer, 8), u16(footer, 0x0A), u32(footer, 0x0C)) == (1, 2, 1), "mixed-header-footer", "footer role/version")
    require(header[0x10:0x20] == context.tape_uuid, "wrong-tape", "embedded tape UUID")
    require(header[0x20:0x30] == context.edition_id, "wrong-edition", "edition ID")
    require(u64(header, 0x30) == context.edition_sequence, "wrong-edition", "edition sequence")
    require(u16(header, 0x38) == ordinal, "wrong-ordinal", "replica ordinal")
    require(u16(header, 0x3A) == 3, "wrong-replica-count", "replica count")
    require(u32(header, 0x3C) == 0, "wrong-scope", "partition")
    declared_block_size = u32(header, 0x40)
    require(declared_block_size in LEGAL_BLOCK_SIZES and declared_block_size == block_size, "wrong-block-size", "declared block size")
    require(u32(header, 0x44) == 0, "compression-enabled", "replica compression")

    # Check all media-derived size terms before walking slots or comparing counts.
    structural_count, object_count = u64(header, 0x60), u64(header, 0x68)
    checked_add(
        checked_mul(structural_count, 64, "structural slot multiplication"),
        checked_mul(object_count, 256, "Object slot multiplication"),
        "payload addition",
    )
    require((structural_count, object_count) == (context.structural_rows, context.object_rows), "wrong-scope", "row counts")
    require((u64(header, 0x48), u64(header, 0x50), u64(header, 0x58)) == (
        context.structural_rows,
        0 if context.object_rows == 0 else 5,
        0 if context.object_rows == 0 else 5,
    ), "wrong-scope", "manifest scope")
    require(header[0xA8:0xC8] == context.payload_digest, "payload-digest", "manifest payload digest")
    require(header[0xC8:0xE8] == context.map_digest, "map-digest", "manifest map digest")
    require(header[0xE8:0x108] == context.edition_digest, "wrong-edition", "manifest edition digest")
    require(header[0x108:0x128] == context.layout_digest, "layout-digest", "manifest layout digest")

    tuples = tuple(tuple_at(header, 0x148 + 32 * index) for index in range(5))
    validate_layout(block_size, tuples, u64(header, 0xA0))
    require(tuples == context.tuples and u64(header, 0xA0) == context.expected_eod, "layout-digest", "manifest layout")
    local = tuples[(ordinal - 1) * 2]
    require((u64(header, 0x88), u64(header, 0x90)) == (local[2], local[3]), "wrong-start", "local planned start")
    require(layout_digest(block_size, tuples, u64(header, 0xA0)) == header[0x108:0x128], "layout-digest", "recomputed layout")
    require(edition_digest(header) == header[0xE8:0x108], "wrong-edition", "recomputed edition")
    require(
        replica_descriptor_digest(header, tuples, ordinal) == header[0x128:0x148],
        "descriptor-digest",
        "replica descriptor",
    )
    writer, timestamp = diagnostic_fields(header)
    ensure_zero(header[0x2B8:0x3F8], "reserved-nonzero", "header local fields")
    ensure_zero(footer[0x300:0x3F8], "reserved-nonzero", "footer reserved")
    require(footer[0x2B8:0x2D8] == hashlib.sha256(header).digest(), "header-hash", "replica header hash")
    observed = (u64(footer, 0x2D8), u64(footer, 0x2E0), u64(footer, 0x2E8))
    require(observed == (local[2], local[3], local[4]), "wrong-observation", "replica local observation")
    expected_delta = local[4] - 1
    require((u64(footer, 0x2F0), u64(footer, 0x2F8)) == (local[3] + expected_delta, expected_delta), "wrong-observation", "replica backward range")
    payload_digest, map_digest = validate_payload(data, header, block_size)
    require(payload_digest == header[0xA8:0xC8], "payload-digest", "recomputed payload")
    require(map_digest == header[0xC8:0xE8], "map-digest", "recomputed map")
    return ReplicaSummary(
        ordinal=ordinal,
        tape_uuid=context.tape_uuid,
        edition_id=header[0x20:0x30],
        edition_sequence=u64(header, 0x30),
        scope=(u64(header, 0x48), u64(header, 0x50), u64(header, 0x58)),
        counts=(structural_count, object_count),
        payload_digest=payload_digest,
        map_digest=map_digest,
        edition_digest=header[0xE8:0x108],
        layout_digest=header[0x108:0x128],
        block_size=block_size,
        writer_version=writer,
        write_timestamp=timestamp,
    )


def gap_descriptor_digest(
    frame: bytes,
    tuples: tuple[tuple[int, int, int, int, int], ...],
    ordinal: int,
) -> bytes:
    index = 1 if ordinal == 1 else 3
    digest = hashlib.sha256()
    digest.update(GAP_DESCRIPTOR_DOMAIN)
    digest.update(frame[0x10:0x30])
    digest.update(struct.pack("<HH", ordinal, 2))
    digest.update(frame[0x34:0x3C])
    digest.update(frame[0x50:0x60])
    digest.update(encode_tuple(tuples[index]))
    digest.update(encode_tuple(tuples[index - 1]))
    digest.update(encode_tuple(tuples[index + 1]))
    digest.update(frame[0x98:0xB8])
    return digest.digest()


def verify_gap(data: bytes, context: ProfileContext, ordinal: int) -> None:
    block_size = context.block_size
    require(len(data) % block_size == 0 and len(data) >= 2 * block_size, "wrong-length", "gap byte length")
    header, footer = data[:block_size], data[-block_size:]
    require(header[:8] == magic(context.tape_uuid, GAP_HEADER_LABEL), "gap-misclassification", "gap header magic")
    require(footer[:8] == magic(context.tape_uuid, GAP_FOOTER_LABEL), "gap-misclassification", "gap footer magic")
    verify_crc(header, 0x200, 0x1F8, "gap header")
    verify_crc(footer, 0x200, 0x1F8, "gap footer")
    require(header[0x0C:0x180] == footer[0x0C:0x180], "mixed-header-footer", "gap common frames")
    require((u16(header, 8), u16(header, 0x0A), u32(header, 0x0C)) == (1, 1, 0), "gap-misclassification", "gap header role")
    require((u16(footer, 8), u16(footer, 0x0A), u32(footer, 0x0C)) == (1, 2, 0), "gap-misclassification", "gap footer role")
    require(header[0x10:0x20] == context.tape_uuid, "wrong-tape", "gap tape UUID")
    require(header[0x20:0x30] == context.edition_id, "wrong-edition", "gap edition")
    require(u16(header, 0x30) == ordinal, "wrong-ordinal", "gap ordinal")
    require(u16(header, 0x32) == 2, "wrong-count", "gap count")
    require(u32(header, 0x34) == 0, "wrong-range", "gap partition")
    declared_block_size = u32(header, 0x38)
    require(declared_block_size in LEGAL_BLOCK_SIZES and declared_block_size == block_size, "wrong-block-size", "gap block size")
    require(u32(header, 0x3C) == 0, "compression-enabled", "gap compression")
    records = len(data) // block_size
    require(u64(header, 0x58) == records, "wrong-count", "gap physical count")
    nominal = u64(header, 0x50)
    require((nominal + block_size - 1) // block_size == records, "wrong-range", "gap nominal extent")
    require((u64(header, 0x60), u64(header, 0x68)) == (records - 1, records - 2), "wrong-count", "gap geometry")
    require(u32(header, 0x74) == 0, "damaged-interior", "gap fill kind")
    tuples = tuple(tuple_at(header, 0x0E0 + 32 * index) for index in range(5))
    validate_layout(block_size, tuples, u64(header, 0xD8))
    require(tuples == context.tuples and u64(header, 0xD8) == context.expected_eod, "layout-digest", "gap manifest layout")
    require(header[0x98:0xB8] == context.layout_digest, "layout-digest", "gap stored layout")
    require(layout_digest(block_size, tuples, u64(header, 0xD8)) == header[0x98:0xB8], "layout-digest", "gap recomputed layout")
    require(gap_descriptor_digest(header, tuples, ordinal) == header[0xB8:0xD8], "wrong-range", "gap descriptor")
    index = 1 if ordinal == 1 else 3
    local, predecessor, successor = tuples[index], tuples[index - 1], tuples[index + 1]
    require((u64(header, 0x40), u64(header, 0x48)) == (local[2], local[3]), "wrong-observation", "gap planned local")
    require((u16(header, 0x70), u16(header, 0x72)) == (predecessor[1], successor[1]), "wrong-range", "gap neighbors")
    require((u64(header, 0x78), u64(header, 0x80), u64(header, 0x88), u64(header, 0x90)) == (predecessor[2], successor[2], predecessor[3], successor[3]), "wrong-range", "gap neighbor positions")
    ensure_zero(header[0x180:0x1F8], "reserved-nonzero", "gap header local fields")
    ensure_zero(footer[0x1C8:0x1F8], "reserved-nonzero", "gap footer reserved")
    require(footer[0x180:0x1A0] == hashlib.sha256(header).digest(), "header-hash", "gap header hash")
    require((u64(footer, 0x1A0), u64(footer, 0x1A8), u64(footer, 0x1B0)) == (local[2], local[3], local[4]), "wrong-observation", "gap observation")
    delta = local[4] - 1
    require((u64(footer, 0x1B8), u64(footer, 0x1C0)) == (local[3] + delta, delta), "wrong-observation", "gap backward range")
    require(data[block_size:-block_size] == bytes(len(data) - 2 * block_size), "damaged-interior", "gap interior")


def context_from_rows(root: Path, name: str, rows: list[dict[str, str]]) -> ProfileContext:
    require(len(rows) == 5, "manifest", f"{name}: not five components")
    by_name = {row["component"]: row for row in rows}
    require(set(by_name) == {item[0] for item in COMPONENTS}, "manifest", f"{name}: component set")
    block_size = int(rows[0]["block_size"])
    header = (root / name / "replica-a.bin").read_bytes()[:block_size]
    tuples = tuple(tuple_at(header, 0x148 + 32 * index) for index in range(5))
    context = ProfileContext(
        name=name,
        directory=root / name,
        block_size=block_size,
        tape_uuid=header[0x10:0x20],
        edition_id=header[0x20:0x30],
        edition_sequence=u64(header, 0x30),
        structural_rows=int(rows[0]["structural_rows"]),
        object_rows=int(rows[0]["object_rows"]),
        edition_digest=bytes.fromhex(rows[0]["edition_digest"]),
        layout_digest=bytes.fromhex(rows[0]["layout_digest"]),
        payload_digest=bytes.fromhex(rows[0]["payload_sha256"]),
        map_digest=bytes.fromhex(rows[0]["canonical_map_sha256"]),
        expected_eod=int(rows[0]["expected_eod_lba"]),
        tuples=tuples,
    )
    expected_kind = "minimal" if name.startswith("minimal-") else "multi"
    require(context.tape_uuid == bytes([0x11]) * 16, "manifest", f"{name}: pinned tape UUID")
    require(
        (context.edition_id, context.edition_sequence)
        == ((bytes([0x21]) * 16, 1) if expected_kind == "minimal" else (bytes([0x22]) * 16, 2)),
        "manifest",
        f"{name}: pinned edition identity",
    )
    for row in rows:
        require(
            (
                int(row["block_size"]),
                int(row["structural_rows"]),
                int(row["object_rows"]),
                int(row["expected_eod_lba"]),
            )
            == (context.block_size, context.structural_rows, context.object_rows, context.expected_eod),
            "manifest",
            f"{name}: inconsistent component metadata",
        )
    return context


def verify_profile(context: ProfileContext, rows: list[dict[str, str]]) -> None:
    by_name = {row["component"]: row for row in rows}
    require(context.block_size in LEGAL_BLOCK_SIZES, "manifest", f"{context.name}: block size")
    require(context.layout_digest == layout_digest(context.block_size, context.tuples, context.expected_eod), "layout-digest", context.name)
    for index, (filename, kind, ordinal) in enumerate(COMPONENTS):
        row = by_name[filename]
        data = (context.directory / filename).read_bytes()
        require(hashlib.sha256(data).hexdigest() == row["sha256"], "manifest", f"{context.name}/{filename}: SHA-256")
        require(len(data) == int(row["bytes"]), "manifest", f"{context.name}/{filename}: byte count")
        require(len(data) // context.block_size == context.tuples[index][4], "manifest", f"{context.name}/{filename}: tuple count")
        require(all(row[field] == expected.hex() for field, expected in (
            ("edition_digest", context.edition_digest),
            ("layout_digest", context.layout_digest),
            ("payload_sha256", context.payload_digest),
            ("canonical_map_sha256", context.map_digest),
        )), "manifest", f"{context.name}/{filename}: digest column")
        if kind == 4:
            verify_replica(data, context, ordinal)
            require(len(data) // context.block_size == int(row["replica_records"]), "manifest", "replica record column")
        else:
            verify_gap(data, context, ordinal)
            require(len(data) // context.block_size == int(row["gap_records"]), "manifest", "gap record column")


def put_u16(data: bytearray, offset: int, value: int) -> None:
    data[offset : offset + 2] = struct.pack("<H", value)


def put_u32(data: bytearray, offset: int, value: int) -> None:
    data[offset : offset + 4] = struct.pack("<I", value)


def put_u64(data: bytearray, offset: int, value: int) -> None:
    data[offset : offset + 8] = struct.pack("<Q", value)


def rewrite_crc(data: bytearray, base: int, crc_offset: int) -> None:
    put_u64(data, base + crc_offset, crc64_xz(bytes(data[base : base + crc_offset])))


def finalize_replica_pair(data: bytearray, block_size: int) -> None:
    footer_base = len(data) - block_size
    rewrite_crc(data, 0, 0x3F8)
    data[footer_base + 0x2B8 : footer_base + 0x2D8] = hashlib.sha256(data[:block_size]).digest()
    rewrite_crc(data, footer_base, 0x3F8)


def finalize_gap_pair(data: bytearray, block_size: int) -> None:
    footer_base = len(data) - block_size
    rewrite_crc(data, 0, 0x1F8)
    data[footer_base + 0x180 : footer_base + 0x1A0] = hashlib.sha256(data[:block_size]).digest()
    rewrite_crc(data, footer_base, 0x1F8)


def mutate_replica(data: bytes, mutation: str, block_size: int, other: bytes | None) -> bytes:
    out = bytearray(data)
    footer = len(out) - block_size
    if mutation == "damage-header":
        out[0x300] ^= 1
    elif mutation == "damage-footer":
        out[footer + 0x300] ^= 1
    elif mutation == "torn-header":
        return bytes(out[100:])
    elif mutation == "torn-footer":
        return bytes(out[:-100])
    elif mutation == "mixed-header-footer":
        require(other is not None, "matrix", "mixed replica lacks source")
        out[footer:] = other[-block_size:]
    elif mutation == "wrong-header-hash":
        out[footer + 0x2B8] ^= 1
        rewrite_crc(out, footer, 0x3F8)
    elif mutation == "wrong-observed-start":
        put_u64(out, footer + 0x2E0, u64(out, footer + 0x2E0) + 1)
        rewrite_crc(out, footer, 0x3F8)
    elif mutation == "wrong-observed-count":
        put_u64(out, footer + 0x2E8, u64(out, footer + 0x2E8) + 1)
        rewrite_crc(out, footer, 0x3F8)
    elif mutation == "payload-corrupt":
        marker = b"minimal-plaintext-object"
        offset = out.find(marker, block_size, footer)
        require(offset >= 0, "matrix", "payload mutation marker is absent")
        out[offset] ^= 1
    elif mutation == "slot-length":
        put_u16(out, block_size, 0xFFFF)
    elif mutation == "swap-object-rows":
        structural = u64(out, 0x60)
        first = block_size + structural * 64
        second = first + 256
        out[first : first + 256], out[second : second + 256] = out[second : second + 256], out[first : first + 256]
    elif mutation == "metadata-frame-too-short":
        structural = u64(out, 0x60)
        encrypted_slot_start = block_size + structural * 64 + 256
        slot = bytes(out[encrypted_slot_start : encrypted_slot_start + 256])
        value, _ = decode_slot(slot, 256, "encrypted Object row")
        require(isinstance(value, CborMap), "matrix", "encrypted mutation row is not a map")
        mutated = CborMap(
            tuple((key, 16 if key == 21 else field) for key, field in value.pairs)
        )
        encoded = cbor_encode(mutated)
        require(len(encoded) <= 254, "matrix", "mutated encrypted row exceeds slot")
        replacement = struct.pack("<H", len(encoded)) + encoded
        replacement += bytes(256 - len(replacement))
        out[encrypted_slot_start : encrypted_slot_start + 256] = replacement
    elif mutation == "payload-padding":
        out[block_size + u64(out, 0x70)] = 1
    elif mutation == "frame-padding":
        out[0x400] = 1
    elif mutation == "reserved-nonzero":
        out[0x300] = 1
        out[footer + 0x300] = 1
        finalize_replica_pair(out, block_size)
    else:
        changes: dict[str, tuple[int, bytes]] = {
            "wrong-tape": (0x10, bytes([0x12]) * 16),
            "wrong-edition": (0x20, bytes([0x24]) * 16),
            "wrong-ordinal": (0x38, struct.pack("<H", 2)),
            "wrong-count": (0x3A, struct.pack("<H", 2)),
            "wrong-scope": (0x48, struct.pack("<Q", u64(out, 0x48) + 1)),
            "wrong-range": (0x50, struct.pack("<Q", u64(out, 0x50) + 1)),
            "wrong-payload-digest": (0xA8, bytes([0x91]) * 32),
            "wrong-map-digest": (0xC8, bytes([0x92]) * 32),
            "wrong-edition-digest": (0xE8, bytes([0x94]) * 32),
            "wrong-descriptor-digest": (0x128, bytes([0x95]) * 32),
            "wrong-layout-digest": (0x108, bytes([0x93]) * 32),
            "wrong-start": (0x90, struct.pack("<Q", u64(out, 0x90) + 1)),
            "wrong-block-size": (0x40, struct.pack("<I", 128 * 1024)),
            "compression-enabled": (0x44, struct.pack("<I", 1)),
            "structural-overflow": (0x60, struct.pack("<Q", MASK64)),
            "object-overflow": (0x68, struct.pack("<Q", MASK64 // 256 + 1)),
            "payload-add-overflow": (0x60, struct.pack("<Q", MASK64 // 64)),
        }
        require(mutation in changes, "matrix", f"unknown replica mutation {mutation}")
        offset, replacement = changes[mutation]
        out[offset : offset + len(replacement)] = replacement
        out[footer + offset : footer + offset + len(replacement)] = replacement
        if mutation == "payload-add-overflow":
            put_u64(out, 0x68, 1)
            put_u64(out, footer + 0x68, 1)
        finalize_replica_pair(out, block_size)
    return bytes(out)


def mutate_gap(data: bytes, mutation: str, block_size: int, other: bytes | None, replica: bytes) -> bytes:
    out = bytearray(data)
    footer = len(out) - block_size
    if mutation == "damage-header":
        out[0x1C8] ^= 1
    elif mutation == "damage-footer":
        out[footer + 0x1C8] ^= 1
    elif mutation == "torn-header":
        return bytes(out[100:])
    elif mutation == "torn-footer":
        return bytes(out[:-100])
    elif mutation == "missing-header":
        return bytes(out[block_size:])
    elif mutation == "missing-footer":
        return bytes(out[:-block_size])
    elif mutation == "misclassify-as-replica":
        out[:block_size] = replica[:block_size]
    elif mutation == "wrong-total-length":
        return bytes(out) + b"\0"
    elif mutation == "mixed-header-footer":
        require(other is not None, "matrix", "mixed gap lacks source")
        out[footer:] = other[-block_size:]
    elif mutation == "wrong-observed-start":
        put_u64(out, footer + 0x1A8, u64(out, footer + 0x1A8) + 1)
        rewrite_crc(out, footer, 0x1F8)
    elif mutation == "interior-nonzero":
        out[block_size] = 1
    else:
        changes: dict[str, tuple[int, bytes]] = {
            "compression-enabled": (0x3C, struct.pack("<I", 1)),
            "wrong-range": (0x50, struct.pack("<Q", u64(out, 0x50) + 1)),
            "wrong-count": (0x58, struct.pack("<Q", u64(out, 0x58) + 1)),
            "wrong-tape": (0x10, bytes([0x12]) * 16),
            "wrong-edition": (0x20, bytes([0x24]) * 16),
            "wrong-ordinal": (0x30, struct.pack("<H", 2)),
        }
        require(mutation in changes, "matrix", f"unknown gap mutation {mutation}")
        offset, replacement = changes[mutation]
        out[offset : offset + len(replacement)] = replacement
        out[footer + offset : footer + offset + len(replacement)] = replacement
        finalize_gap_pair(out, block_size)
    return bytes(out)


def verify_mutations(root: Path, contexts: dict[str, ProfileContext]) -> int:
    rows = read_tsv(root / "MUTATIONS.tsv")
    required_cases = {
        "replica-wrong-tape", "replica-wrong-edition", "replica-wrong-ordinal",
        "replica-wrong-count", "replica-wrong-scope", "replica-wrong-range",
        "replica-wrong-payload-digest", "replica-wrong-start", "replica-wrong-block-size",
        "replica-wrong-edition-digest", "replica-wrong-descriptor-digest",
        "replica-compression-enabled", "replica-mixed-header-footer", "replica-map-row-mismatch",
        "replica-metadata-frame-too-short",
        "gap-header-damaged", "gap-footer-damaged", "gap-header-torn", "gap-footer-torn",
        "gap-header-missing", "gap-footer-missing", "gap-misclassified",
        "gap-wrong-total-length", "gap-compression-enabled", "gap-interior-damaged",
        "filemark-missing",
    }
    require(required_cases <= {row["case_id"] for row in rows}, "matrix", "hostile coverage set")
    for row in rows:
        if row["kind"] == "event":
            require(row["mutation"] == "missing-filemark" and row["expected"] == "missing-filemark", "matrix", row["case_id"])
            continue
        context = contexts[row["base_profile"]]
        source = (context.directory / row["target"]).read_bytes()
        other = (root / row["other_profile"]).read_bytes() if row["other_profile"] else None
        ordinal = 1 if row["target"] in ("replica-a.bin", "gap-ab.bin") else 2 if row["target"] in ("replica-b.bin", "gap-bc.bin") else 3
        if row["kind"] == "replica":
            damaged = mutate_replica(source, row["mutation"], context.block_size, other)
            verify = lambda: verify_replica(damaged, context, ordinal)
        else:
            replica = (context.directory / "replica-a.bin").read_bytes()
            damaged = mutate_gap(source, row["mutation"], context.block_size, other, replica)
            verify = lambda: verify_gap(damaged, context, ordinal)
        try:
            verify()
        except VectorError as error:
            require(error.code == row["expected"], "matrix", f"{row['case_id']}: got {error.code}, expected {row['expected']}")
        else:
            fail("matrix", f"{row['case_id']}: hostile input was accepted")
    return len(rows)


def verify_selection(root: Path, contexts: dict[str, ProfileContext]) -> int:
    rows = read_tsv(root / "SELECTION.tsv")
    expected_cases = {
        "healthy", "damage-a", "damage-b", "damage-c", "damage-a-b", "damage-a-c",
        "damage-b-c", "all-invalid", "all-torn", "all-missing", "all-conflicting",
        "conflicting-a", "conflicting-b", "conflicting-c",
    }
    require({row["case_id"] for row in rows} == expected_cases, "matrix", "selection coverage")
    filenames = ("replica-a.bin", "replica-b.bin", "replica-c.bin")
    for row in rows:
        base = contexts[row["base_profile"]]
        survivors: list[ReplicaSummary] = []
        for ordinal, (column, filename) in enumerate(zip(("a", "b", "c"), filenames), 1):
            state = row[column]
            if state == "missing":
                continue
            artifact_context = contexts[f"minimal-{base.block_size // 1024}k"] if state == "conflict-minimal" else base
            data = (artifact_context.directory / filename).read_bytes()
            if state == "damaged":
                data = mutate_replica(data, "damage-header", base.block_size, None)
            elif state == "torn":
                data = mutate_replica(data, "torn-footer", base.block_size, None)
            try:
                survivors.append(verify_replica(data, artifact_context, ordinal))
            except VectorError:
                continue
        if not survivors:
            outcome = "bot-structural-recovery"
        elif any(item.agreement_key() != survivors[0].agreement_key() for item in survivors[1:]):
            outcome = "conflict"
        else:
            outcome = f"select-{chr(ord('a') + max(item.ordinal for item in survivors) - 1)}"
        require(outcome == row["expected"], "matrix", f"{row['case_id']}: {outcome}")
    return len(rows)


def verify_interruptions(root: Path) -> int:
    rows = read_tsv(root / "INTERRUPTIONS.tsv")
    require(len(rows) == 68, "matrix", "complete live interruption boundary count")
    expected_fields = {
        "case_id", "phase", "component", "cut",
        "prefix_parity_map_blocks_accepted", "prefix_parity_map_filemark_accepted",
        "prefix_media_barrier_proved",
        "prefix_sink_journal", "prefix_sink_checkpoint",
        "component_block_streams_accepted", "component_filemark_commands_accepted",
        "component_media_barriers_proved",
        "sink_journal_components", "checkpoint_components", "sqlite_components",
        "sealed_checkpoint", "intent_present", "final_sqlite",
        "expected_checkpoint_progress", "expected_completed_replicas", "expected_resume",
    }
    require(
        all(set(row) == expected_fields for row in rows),
        "matrix", "interruption schema",
    )
    require(
        len({row["case_id"] for row in rows}) == len(rows),
        "matrix", "unique interruption case ids",
    )
    prefix_cuts = {
        "before_terminal_prefix", "before_final_parity_map",
        "after_final_parity_map", "after_terminal_prefix",
    }
    component_cuts = {
        "before_footer", "after_footer", "before_filemark", "after_filemark",
        "before_barrier", "after_barrier", "before_parity_journal_fsync",
        "after_parity_journal_fsync", "before_checkpoint_journal_fsync",
        "after_checkpoint_journal_fsync",
    }
    sqlite_cuts = {"before_sqlite_projection", "after_sqlite_projection"}
    final_cuts = {
        "before_final_checkpoint_fsync", "after_final_checkpoint_fsync",
        "before_final_sqlite_projection", "after_final_sqlite_projection",
    }
    component_names = [
        "replica_a", "separation_ab", "replica_b", "separation_bc", "replica_c"
    ]
    by_phase: dict[str, list[dict[str, str]]] = {}
    for row in rows:
        by_phase.setdefault(row["phase"], []).append(row)
        prefix_counts = [
            int(row[field]) for field in (
                "prefix_parity_map_blocks_accepted", "prefix_parity_map_filemark_accepted",
                "prefix_media_barrier_proved",
                "prefix_sink_journal", "prefix_sink_checkpoint",
            )
        ]
        counts = [
            int(row[field]) for field in (
                "component_block_streams_accepted", "component_filemark_commands_accepted",
                "component_media_barriers_proved",
                "sink_journal_components", "checkpoint_components", "sqlite_components",
            )
        ]
        final_flags = [
            int(row[field]) for field in (
                "sealed_checkpoint", "intent_present", "final_sqlite",
            )
        ]
        require(
            all(value in (0, 1) for value in prefix_counts + final_flags),
            "matrix", f"{row['case_id']}: binary authority fields",
        )
        prefix_blocks, prefix_filemark, prefix_barrier, prefix_journal, prefix_checkpoint = prefix_counts
        require(
            0 <= prefix_checkpoint <= prefix_journal <= prefix_barrier <= prefix_filemark <= prefix_blocks <= 1,
            "matrix", f"{row['case_id']}: prefix command/proof/authority ordering",
        )
        block_streams, filemark_commands, media_barriers, sink, checkpoint, sqlite = counts
        require(
            0 <= sqlite <= checkpoint <= sink <= media_barriers <= filemark_commands <= block_streams <= 5,
            "matrix", f"{row['case_id']}: command/proof/authority ordering",
        )
        require(
            row["expected_checkpoint_progress"] == PROGRESS[checkpoint],
            "matrix", f"{row['case_id']}: checkpoint progress",
        )
        require(
            int(row["expected_completed_replicas"]) == (checkpoint + 1) // 2,
            "matrix", f"{row['case_id']}: completed replica projection",
        )

    prefix = by_phase.get("prefix", [])
    require({row["cut"] for row in prefix} == prefix_cuts, "matrix", "prefix cuts")
    prefix_expected = {
        "before_terminal_prefix": (0, 0, 0, 0, 0, "finish-terminal-prefix"),
        "before_final_parity_map": (0, 0, 0, 0, 0, "finish-terminal-prefix"),
        "after_final_parity_map": (1, 1, 0, 0, 0, "finish-terminal-prefix"),
        "after_terminal_prefix": (1, 1, 1, 1, 1, "replica_a"),
    }
    for row in prefix:
        expected = prefix_expected[row["cut"]]
        actual = (
            int(row["prefix_parity_map_blocks_accepted"]),
            int(row["prefix_parity_map_filemark_accepted"]),
            int(row["prefix_media_barrier_proved"]), int(row["prefix_sink_journal"]),
            int(row["prefix_sink_checkpoint"]), row["expected_resume"],
        )
        require(actual == expected, "matrix", f"{row['case_id']}: exact prefix state")
        require(
            row["component"] == "parity_closeout"
            and all(row[field] == "0" for field in (
                "component_block_streams_accepted", "component_filemark_commands_accepted",
                "component_media_barriers_proved",
                "sink_journal_components", "checkpoint_components", "sqlite_components",
                "sealed_checkpoint", "final_sqlite",
            ))
            and row["intent_present"] == "1"
            and row["expected_checkpoint_progress"] == "BeforeReplicaA"
            and row["expected_completed_replicas"] == "0",
            "matrix", f"{row['case_id']}: exact prefix host state",
        )

    component_rows = by_phase.get("component", [])
    for index, component in enumerate(component_names):
        subset = [row for row in component_rows if row["component"] == component]
        expected_cuts = component_cuts | sqlite_cuts
        require({row["cut"] for row in subset} == expected_cuts, "matrix", f"{component}: cut set")
        for row in subset:
            cut = row["cut"]
            if cut == "before_footer":
                component_state = (index, index, index, index, index, index)
                resume = "reconcile-current"
            elif cut in {"after_footer", "before_filemark"}:
                component_state = (index + 1, index, index, index, index, index)
                resume = "reconcile-current"
            elif cut in {"after_filemark", "before_barrier"}:
                component_state = (index + 1, index + 1, index, index, index, index)
                resume = "reconcile-current"
            elif cut in {"after_barrier", "before_parity_journal_fsync"}:
                component_state = (index + 1, index + 1, index + 1, index, index, index)
                resume = "reconcile-current"
            elif cut in {"after_parity_journal_fsync", "before_checkpoint_journal_fsync"}:
                component_state = (index + 1, index + 1, index + 1, index + 1, index, index)
                resume = "promote-sink-transition"
            elif cut in {"after_checkpoint_journal_fsync", "before_sqlite_projection"}:
                component_state = (index + 1, index + 1, index + 1, index + 1, index + 1, index)
                resume = (
                    "repair-sqlite-then-finish-final-projection"
                    if index == 4 else "repair-sqlite-then-continue"
                )
            elif cut == "after_sqlite_projection":
                component_state = (index + 1,) * 6
                resume = "finish-final-projection" if index == 4 else "continue"
            else:
                fail("matrix", f"{row['case_id']}: unclassified component cut")
            actual_state = tuple(
                int(row[field]) for field in (
                    "component_block_streams_accepted", "component_filemark_commands_accepted",
                    "component_media_barriers_proved",
                    "sink_journal_components", "checkpoint_components", "sqlite_components",
                )
            )
            require(
                actual_state == component_state,
                "matrix", f"{row['case_id']}: exact component authority state",
            )
            require(
                all(row[field] == "1" for field in (
                    "prefix_parity_map_blocks_accepted", "prefix_parity_map_filemark_accepted",
                    "prefix_media_barrier_proved",
                    "prefix_sink_journal", "prefix_sink_checkpoint", "intent_present",
                ))
                and row["sealed_checkpoint"] == "0"
                and row["final_sqlite"] == "0"
                and row["expected_resume"] == resume,
                "matrix", f"{row['case_id']}: exact component recovery state",
            )

    final = by_phase.get("final_projection", [])
    require({row["cut"] for row in final} == final_cuts, "matrix", "final projection cuts")
    final_expected = {
        "before_final_checkpoint_fsync": (0, 1, 0, "finish-final-projection"),
        "after_final_checkpoint_fsync": (1, 1, 0, "replay-sealed-completion"),
        "before_final_sqlite_projection": (1, 0, 0, "replay-sealed-completion"),
        "after_final_sqlite_projection": (1, 0, 1, "none"),
    }
    for row in final:
        expected = final_expected[row["cut"]]
        actual = (
            int(row["sealed_checkpoint"]), int(row["intent_present"]),
            int(row["final_sqlite"]), row["expected_resume"],
        )
        require(actual == expected, "matrix", f"{row['case_id']}: exact final state")
        require(
            all(row[field] == "1" for field in (
                "prefix_parity_map_blocks_accepted", "prefix_parity_map_filemark_accepted",
                "prefix_media_barrier_proved",
                "prefix_sink_journal", "prefix_sink_checkpoint",
            ))
            and all(row[field] == "5" for field in (
                "component_block_streams_accepted", "component_filemark_commands_accepted",
                "component_media_barriers_proved",
                "sink_journal_components", "checkpoint_components", "sqlite_components",
            ))
            and row["expected_checkpoint_progress"] == "AfterReplicaC"
            and row["expected_completed_replicas"] == "3",
            "matrix", f"{row['case_id']}: exact final authority",
        )
    require(set(by_phase) == {"prefix", "component", "final_projection"}, "matrix", "known phases")
    return len(rows)


def verify_maximums(root: Path) -> int:
    rows = read_tsv(root / "MAXIMUMS.tsv")
    require({row["vector"] for row in rows} == {
        "maximum-plaintext-row", "maximum-encrypted-row", "maximum-one-block-footer"
    }, "manifest", "maximum vector set")
    for row in rows:
        data = (root / "maximums" / row["artifact"]).read_bytes()
        require(len(data) == int(row["bytes"]), "manifest", f"{row['vector']}: bytes")
        require(hashlib.sha256(data).hexdigest() == row["sha256"], "manifest", f"{row['vector']}: digest")
        if row["vector"].endswith("row"):
            value, encoded = decode_slot(data, 256, row["vector"])
            require(len(encoded) == int(row["encoded_len"]), "manifest", f"{row['vector']}: encoded maximum")
            fields = dict(value.pairs) if isinstance(value, CborMap) else {}
            require(fields.get(1) == MASK64 and fields.get(3) == MASK64 and fields.get(4) == bytes([0xFF]) * 64, "manifest", f"{row['vector']}: maximum u64/id")
            object_fields(value, int(row["block_size"]))
            if row["vector"] == "maximum-plaintext-row":
                require(len(encoded) == 164, "manifest", "plaintext maximum length")
            else:
                require(len(encoded) == 247 and len(fields[22]) == 8, "manifest", "encrypted maximum length")
        else:
            block_size = int(row["block_size"])
            require(block_size in LEGAL_BLOCK_SIZES and len(data) == block_size, "manifest", "footer is exactly one block")
            tape_uuid = data[0x10:0x20]
            require(data[:8] == magic(tape_uuid, REPLICA_FOOTER_LABEL), "manifest", "maximum footer magic")
            verify_crc(data, 0x400, 0x3F8, "maximum footer")
            require((u16(data, 8), u16(data, 0x0A), u32(data, 0x0C)) == (1, 2, 1), "manifest", "maximum footer role")
            require((u16(data, 0x1F0), u16(data, 0x1F2)) == (int(row["writer_version_len"]), int(row["write_timestamp_len"])), "manifest", "maximum diagnostics")
            writer, timestamp = diagnostic_fields(data)
            require(len(writer) == 128 and len(timestamp) == 64, "manifest", "maximum diagnostic bounds")
            require(u64(data, 0x68) == 0 and u64(data, 0x70) == 64, "manifest", "footer Object-count independence")
            tuples = tuple(tuple_at(data, 0x148 + 32 * index) for index in range(5))
            validate_layout(block_size, tuples, u64(data, 0xA0))
            require(layout_digest(block_size, tuples, u64(data, 0xA0)) == data[0x108:0x128], "manifest", "maximum footer layout")
            require(edition_digest(data) == data[0xE8:0x108], "manifest", "maximum footer edition")
            ordinal = u16(data, 0x38)
            require(replica_descriptor_digest(data, tuples, ordinal) == data[0x128:0x148], "manifest", "maximum footer descriptor")
            local = tuples[(ordinal - 1) * 2]
            require(
                (u64(data, 0x2D8), u64(data, 0x2E0), u64(data, 0x2E8))
                == (local[2], local[3], local[4]),
                "manifest",
                "maximum footer observation",
            )
            reconstructed_header = bytearray(data)
            reconstructed_header[:8] = magic(tape_uuid, REPLICA_HEADER_LABEL)
            put_u16(reconstructed_header, 0x0A, 1)
            reconstructed_header[0x2B8:0x300] = bytes(0x48)
            rewrite_crc(reconstructed_header, 0, 0x3F8)
            require(
                data[0x2B8:0x2D8] == hashlib.sha256(reconstructed_header).digest(),
                "manifest",
                "maximum footer header hash",
            )
    return len(rows)


def high_count_row_slot(file_number: int) -> bytes:
    value = CborMap((
        (1, file_number), (2, "plaintext"), (3, 1), (4, b"x"),
        (10, 0), (11, 1), (12, 1), (13, bytes([0x51]) * 32),
    ))
    encoded = cbor_encode(value)
    return struct.pack("<H", len(encoded)) + encoded + bytes(254 - len(encoded))


def structural_slot(value: tuple[Any, ...]) -> tuple[bytes, bytes]:
    encoded = cbor_encode(value)
    return struct.pack("<H", len(encoded)) + encoded + bytes(62 - len(encoded)), encoded


def synthetic_layout(block_size: int, first_file: int, first_lba: int, replica_records: int) -> tuple[tuple[tuple[int, int, int, int, int], ...], int]:
    specs = ((4, 1, replica_records), (5, 1, 3), (4, 2, replica_records), (5, 2, 3), (4, 3, replica_records))
    components = []
    tape_file, start = first_file, first_lba
    for kind, ordinal, records in specs:
        components.append((kind, ordinal, tape_file, start, records))
        tape_file += 1
        start += records + 1
    return tuple(components), start


def verify_streaming(root: Path) -> int:
    rows = read_tsv(root / "STREAMING.tsv")
    require(len(rows) == 1 and rows[0]["vector"] == "large-count-million", "manifest", "streaming vector set")
    row = rows[0]
    block_size = int(row["block_size"])
    structural_count, object_count = int(row["structural_rows"]), int(row["object_rows"])
    require(object_count == 1_000_000 and structural_count == object_count + 1, "manifest", "high-count cardinality")
    payload_len = 64 * structural_count + 256 * object_count
    payload_records = (payload_len + block_size - 1) // block_size
    require((payload_len, payload_records, payload_records + 2) == (
        int(row["payload_bytes"]), int(row["payload_records"]), int(row["replica_records"])
    ), "manifest", "streamed geometry")
    require((row["structural_passes"], row["object_passes"], row["retained_rows"]) == ("1", "1", "0"), "manifest", "constant-storage evidence")

    payload_hasher = hashlib.sha256()
    payload_hasher.update(PAYLOAD_DOMAIN)
    map_hasher = hashlib.sha256()
    map_hasher.update(cbor_head(4, structural_count))
    slot, encoded = structural_slot((0, 2, 1, None, None, None, None))
    payload_hasher.update(slot)
    map_hasher.update(encoded)
    for ordinal in range(object_count):
        slot, encoded = structural_slot((ordinal + 1, 0, 1, ordinal, None, None, None))
        payload_hasher.update(slot)
        map_hasher.update(encoded)
    for ordinal in range(object_count):
        payload_hasher.update(high_count_row_slot(ordinal + 1))
    payload_digest = payload_hasher.digest()
    map_digest = map_hasher.digest()
    require(payload_digest.hex() == row["payload_sha256"] and map_digest.hex() == row["canonical_map_sha256"], "manifest", "independent high-count digests")

    prefix_end = 2 * object_count + 2
    tuples, eod = synthetic_layout(block_size, structural_count, prefix_end, payload_records + 2)
    layout = layout_digest(block_size, tuples, eod)
    require(
        eod == int(row["expected_eod_lba"]) and layout.hex() == row["layout_digest"],
        "manifest",
        "independent high-count layout",
    )
    digest = hashlib.sha256()
    digest.update(EDITION_DOMAIN)
    digest.update(struct.pack("<H", 1))
    digest.update(bytes([0x61]) * 16 + bytes([0x62]) * 16)
    digest.update(struct.pack("<QIII", 1, 0, block_size, 0))
    digest.update(struct.pack("<QQQQQQ", structural_count, object_count, 0, structural_count, object_count, payload_len))
    digest.update(struct.pack("<Q", payload_records))
    digest.update(payload_digest + map_digest)
    writer, timestamp = b"synthetic-constant-storage-source/1", b"2026-08-09T00:00:00Z"
    digest.update(struct.pack("<Q", len(writer)) + writer + struct.pack("<Q", len(timestamp)) + timestamp)
    require(digest.hexdigest() == row["edition_digest"], "manifest", "independent high-count edition digest")
    return 1


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("directory", nargs="?", default="fixtures/rem-parity-terminal-index-draft")
    args = parser.parse_args()
    root = Path(args.directory)
    manifest_rows = read_tsv(root / "MANIFEST.tsv")
    grouped: dict[str, list[dict[str, str]]] = {}
    for row in manifest_rows:
        grouped.setdefault(row["profile"], []).append(row)
    expected_profiles = {f"{name}-{size // 1024}k" for name in ("minimal", "multi") for size in LEGAL_BLOCK_SIZES}
    require(set(grouped) == expected_profiles, "manifest", "healthy profile/block-size coverage")
    contexts = {name: context_from_rows(root, name, rows) for name, rows in grouped.items()}
    for name in sorted(grouped):
        verify_profile(contexts[name], grouped[name])
    maximums = verify_maximums(root)
    streaming = verify_streaming(root)
    mutations = verify_mutations(root, contexts)
    selections = verify_selection(root, contexts)
    interruptions = verify_interruptions(root)
    print(
        f"verified {len(grouped)} healthy profiles ({len(manifest_rows)} components), "
        f"{maximums} maximum artifacts, {streaming} million-Object stream, "
        f"{mutations} hostile mutations, {selections} survivor selections, and "
        f"{interruptions} interruption cuts"
    )


if __name__ == "__main__":
    main()
