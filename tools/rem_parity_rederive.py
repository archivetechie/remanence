#!/usr/bin/env python3
"""Independently re-derive REM-PARITY publication-vector mathematics.

This module is intentionally a specification-only implementation.  It uses
only Python's standard library and follows the wire definitions in
``rem-parity-1-specification.md``: bit-serial GF(2^8) arithmetic, the fixed
Cauchy seed partition, CRC-64/XZ, deterministic CBOR for canonical filemark
maps, sidecar indices, bootstrap digest records, and parity-map digest fields.
It does not import or inspect Remanence's Rust implementation.
"""

from __future__ import annotations

import dataclasses
import hashlib
import hmac
import json
import pathlib
import re
import struct
import time
from collections.abc import Iterable, Mapping, Sequence
from typing import Any


BOOTSTRAP_MAGIC = bytes.fromhex("52 45 4d 00 42 4f 4f 01")
SIDECAR_MAGIC_LABEL = bytes.fromhex("52 45 4d 00 50 41 52 01")
SIDECAR_FOOTER_MAGIC_LABEL = bytes.fromhex(
    "52 45 4d 00 50 41 52 46 4f 4f 54 01"
)
PARITY_MAP_MAGIC_LABEL = bytes.fromhex("52 45 4d 00 50 4d 41 50 01")
SIDECAR_METADATA_HASH_DOMAIN = b"remanence-sidecar-metadata-v1"
SCHEME_ID = "rs-cauchy-gf256-v1"
PARITY_MAP_FORMAT_ID = "rem-parity-map-v1"
GF_REDUCTION_POLYNOMIAL = 0x11D
CRC64_XZ_REFLECTED_POLYNOMIAL = 0xC96C5795D7870F42
MASK64 = (1 << 64) - 1
UINT32_MAX = (1 << 32) - 1
TAPE_FILE_PATTERN = re.compile(
    r"^(?:committed-|appended-)?tape-file-(\d+)-.+\.bin$"
)
HEX_SHA256_PATTERN = re.compile(r"^[0-9a-f]{64}$")


class RederivationError(AssertionError):
    """A published value does not match its independent re-derivation."""


def _require(condition: bool, message: str) -> None:
    if not condition:
        raise RederivationError(message)


def _match(label: str, actual: object, expected: object) -> None:
    if actual != expected:
        raise RederivationError(f"{label}: re-derived {actual!r} != pinned {expected!r}")


def _is_uint(value: object, maximum: int = MASK64) -> bool:
    """Return whether ``value`` is a non-boolean integer in the wire domain."""
    return type(value) is int and 0 <= value <= maximum


def _u16(data: bytes, offset: int) -> int:
    return int.from_bytes(data[offset : offset + 2], "little")


def _u32(data: bytes, offset: int) -> int:
    return int.from_bytes(data[offset : offset + 4], "little")


def _u64(data: bytes, offset: int) -> int:
    return int.from_bytes(data[offset : offset + 8], "little")


def gf_mul(a: int, b: int) -> int:
    """Multiply two bytes in GF(2^8), bit by bit, per specification §6.1."""
    _require(0 <= a <= 0xFF and 0 <= b <= 0xFF, "GF operands must be bytes")
    product = 0
    for _ in range(8):
        if b & 1:
            product ^= a
        b >>= 1
        a <<= 1
        if a & 0x100:
            a ^= GF_REDUCTION_POLYNOMIAL
    return product


def gf_pow(value: int, exponent: int) -> int:
    """Exponentiate in GF(2^8) using square-and-multiply."""
    _require(exponent >= 0, "GF exponent must be nonnegative")
    result = 1
    while exponent:
        if exponent & 1:
            result = gf_mul(result, value)
        value = gf_mul(value, value)
        exponent >>= 1
    return result


def gf_inv(value: int) -> int:
    """Invert a nonzero field element as v^254, per specification §6.1."""
    _require(value != 0, "GF inversion of zero")
    return gf_pow(value, 254)


def cauchy_matrix(k: int, m: int) -> tuple[tuple[int, ...], ...]:
    """Derive the m-by-k Cauchy matrix from X_j=k+j and Y_i=i (§6.2)."""
    _require(k >= 2, f"invalid Cauchy k={k}")
    _require(1 <= m <= k, f"invalid Cauchy m={m} for k={k}")
    _require(k + m <= 255, f"Cauchy seed partition overlaps for k={k}, m={m}")
    return tuple(
        tuple(gf_inv((k + parity_index) ^ data_index) for data_index in range(k))
        for parity_index in range(m)
    )


def _multiplication_table(coefficient: int) -> bytes:
    """Build one translation table solely from the bitwise GF multiplier."""
    return bytes(gf_mul(coefficient, value) for value in range(256))


def encode_parity(
    data_shards: Sequence[bytes],
    matrix: Sequence[Sequence[int]] | None = None,
) -> tuple[bytes, ...]:
    """Encode systematic parity shards byte-wise with the spec Cauchy matrix."""
    _require(len(data_shards) >= 2, "parity encoding requires at least two data shards")
    shard_size = len(data_shards[0])
    _require(shard_size > 0, "parity encoding requires nonempty shards")
    _require(
        all(len(shard) == shard_size for shard in data_shards),
        "data shard sizes differ",
    )
    if matrix is None:
        raise RederivationError("encode_parity requires an explicitly derived matrix")
    _require(all(len(row) == len(data_shards) for row in matrix), "matrix width differs")

    tables = {
        coefficient: _multiplication_table(coefficient)
        for row in matrix
        for coefficient in row
    }
    parity: list[bytes] = []
    for row in matrix:
        accumulator = bytearray(shard_size)
        for coefficient, shard in zip(row, data_shards, strict=True):
            product = shard.translate(tables[coefficient])
            for offset, value in enumerate(product):
                accumulator[offset] ^= value
        parity.append(bytes(accumulator))
    return tuple(parity)


def _crc64_table_entry(value: int) -> int:
    """Derive one reflected CRC table entry using eight literal bit steps."""
    remainder = value
    for _ in range(8):
        remainder = (
            (remainder >> 1) ^ CRC64_XZ_REFLECTED_POLYNOMIAL
            if remainder & 1
            else remainder >> 1
        )
    return remainder & MASK64


CRC64_XZ_TABLE = tuple(_crc64_table_entry(value) for value in range(256))


def crc64_xz(data: bytes) -> int:
    """Compute reflected CRC-64/XZ from its complete §5.1 parameters."""
    remainder = MASK64
    for value in data:
        remainder = CRC64_XZ_TABLE[(remainder ^ value) & 0xFF] ^ (remainder >> 8)
    return remainder ^ MASK64


def _cbor_type_and_length(major: int, value: int) -> bytes:
    _require(0 <= major <= 7 and value >= 0, "invalid CBOR type or length")
    lead = major << 5
    if value < 24:
        return bytes([lead | value])
    if value <= 0xFF:
        return bytes([lead | 24, value])
    if value <= 0xFFFF:
        return bytes([lead | 25]) + struct.pack(">H", value)
    if value <= 0xFFFFFFFF:
        return bytes([lead | 26]) + struct.pack(">I", value)
    if value <= 0xFFFFFFFFFFFFFFFF:
        return bytes([lead | 27]) + struct.pack(">Q", value)
    raise RederivationError(f"CBOR integer too large: {value}")


def encode_deterministic_cbor(value: Any) -> bytes:
    """Encode the deterministic CBOR subset used by REM-PARITY."""
    if value is None:
        return b"\xf6"
    if value is False:
        return b"\xf4"
    if value is True:
        return b"\xf5"
    if isinstance(value, int):
        if value >= 0:
            return _cbor_type_and_length(0, value)
        return _cbor_type_and_length(1, -1 - value)
    if isinstance(value, bytes):
        return _cbor_type_and_length(2, len(value)) + value
    if isinstance(value, str):
        encoded = value.encode("utf-8")
        return _cbor_type_and_length(3, len(encoded)) + encoded
    if isinstance(value, (list, tuple)):
        return _cbor_type_and_length(4, len(value)) + b"".join(
            encode_deterministic_cbor(item) for item in value
        )
    if isinstance(value, Mapping):
        encoded_items = [
            (encode_deterministic_cbor(key), encode_deterministic_cbor(item))
            for key, item in value.items()
        ]
        encoded_items.sort(key=lambda item: (len(item[0]), item[0]))
        return _cbor_type_and_length(5, len(encoded_items)) + b"".join(
            key + item for key, item in encoded_items
        )
    raise RederivationError(f"unsupported deterministic CBOR value: {type(value)!r}")


def _decode_cbor_head(data: bytes, offset: int) -> tuple[int, int, int]:
    _require(offset < len(data), "truncated CBOR item")
    initial = data[offset]
    offset += 1
    major = initial >> 5
    additional = initial & 0x1F
    _require(additional != 31, "indefinite-length CBOR is forbidden")
    if additional < 24:
        return major, additional, offset
    widths = {24: 1, 25: 2, 26: 4, 27: 8}
    _require(additional in widths, f"reserved CBOR additional value {additional}")
    width = widths[additional]
    _require(offset + width <= len(data), "truncated CBOR head")
    value = int.from_bytes(data[offset : offset + width], "big")
    minimum = {1: 24, 2: 0x100, 4: 0x10000, 8: 0x100000000}[width]
    _require(value >= minimum, "non-shortest deterministic CBOR integer/length")
    return major, value, offset + width


def _decode_cbor_item(
    data: bytes, offset: int = 0, depth: int = 0
) -> tuple[Any, int, bytes]:
    _require(depth <= 64, "CBOR nesting exceeds 64 levels")
    start = offset
    major, value, offset = _decode_cbor_head(data, offset)
    if major == 0:
        decoded: Any = value
    elif major == 1:
        decoded = -1 - value
    elif major in (2, 3):
        _require(offset + value <= len(data), "truncated CBOR byte/text string")
        raw = data[offset : offset + value]
        offset += value
        if major == 2:
            decoded = raw
        else:
            try:
                decoded = raw.decode("utf-8")
            except UnicodeDecodeError as error:
                raise RederivationError(f"invalid CBOR UTF-8: {error}") from error
    elif major == 4:
        decoded = []
        for _ in range(value):
            item, offset, _ = _decode_cbor_item(data, offset, depth + 1)
            decoded.append(item)
    elif major == 5:
        decoded = {}
        previous_order: tuple[int, bytes] | None = None
        for _ in range(value):
            key, offset, encoded_key = _decode_cbor_item(data, offset, depth + 1)
            order = (len(encoded_key), encoded_key)
            _require(
                previous_order is None or previous_order < order,
                "CBOR map keys are duplicate or not in deterministic order",
            )
            previous_order = order
            _require(key not in decoded, "duplicate CBOR map key")
            decoded[key], offset, _ = _decode_cbor_item(data, offset, depth + 1)
    elif major == 7 and value in (20, 21, 22):
        decoded = {20: False, 21: True, 22: None}[value]
    else:
        raise RederivationError(f"unsupported CBOR major/simple value {major}/{value}")
    return decoded, offset, data[start:offset]


def decode_deterministic_cbor(data: bytes) -> Any:
    """Decode one exact deterministic CBOR item."""
    value, offset, _ = _decode_cbor_item(data)
    _require(offset == len(data), "trailing bytes after deterministic CBOR item")
    _match("deterministic CBOR round trip", encode_deterministic_cbor(value), data)
    return value


@dataclasses.dataclass(frozen=True)
class Scheme:
    """One recorded rs-cauchy-gf256-v1 scheme triple."""

    k: int
    m: int
    stripes: int

    def validate(self, label: str) -> None:
        _require(self.k >= 2, f"{label}: k must be at least 2")
        _require(1 <= self.m <= self.k, f"{label}: invalid m")
        _require(self.stripes >= 1, f"{label}: S must be positive")
        _require(self.k + self.m <= 255, f"{label}: k+m exceeds 255")
        _require(
            self.stripes * (self.k + self.m) <= 0xFFFFFFFF,
            f"{label}: S*(k+m) exceeds u32",
        )


@dataclasses.dataclass(frozen=True)
class DigestRecord:
    """The canonical filemark-map digest and its four scope fields."""

    sha256: bytes
    tape_file_count: int
    total_data_ordinals: int
    highest_protected_ordinal: int
    is_final: bool


@dataclasses.dataclass(frozen=True)
class DirectoryEntry:
    """One sidecar row from an inline or external epoch directory."""

    tape_file_number: int
    epoch_id: int
    start: int
    end: int
    sidecar_blocks: int
    header_blocks: int
    parity_blocks: int
    metadata_hash: bytes
    flags: int


@dataclasses.dataclass(frozen=True)
class Bootstrap:
    """A parsed bootstrap frame relevant to independent digest validation."""

    block_size: int
    tape_uuid: bytes
    scheme: Scheme | None
    digest: DigestRecord | None
    directory: tuple[DirectoryEntry, ...] | None
    parity_map_reference: int | None
    payload: Mapping[int, Any]


@dataclasses.dataclass(frozen=True)
class Sidecar:
    """A parsed sidecar, including raw parity shards and index CRC pins."""

    scheme: Scheme
    epoch_id: int
    start: int
    end: int
    block_size: int
    header_blocks: int
    parity_blocks: int
    total_blocks: int
    metadata_hash: bytes
    parity_crc64: Mapping[tuple[int, int], int]
    data_crc64: tuple[int, ...]
    parity_shards: Mapping[tuple[int, int], bytes]


@dataclasses.dataclass(frozen=True)
class ParityMap:
    """A parsed parity_map and its independently hashed payload."""

    sequence: int
    block_size: int
    payload: bytes
    payload_sha256: bytes
    canonical_map_digest: bytes
    directory: tuple[DirectoryEntry, ...]
    scope: DigestRecord


@dataclasses.dataclass(frozen=True)
class FilemarkEntry:
    """The seven fields projected into the canonical map digest."""

    tape_file_number: int
    kind: int
    block_count: int
    first_data_ordinal: int | None = None
    protected_start: int | None = None
    protected_end: int | None = None
    epoch_id: int | None = None

    def projection(self) -> list[int | None]:
        return [
            self.tape_file_number,
            self.kind,
            self.block_count,
            self.first_data_ordinal,
            self.protected_start,
            self.protected_end,
            self.epoch_id,
        ]


@dataclasses.dataclass
class ImageSummary:
    """Per-image counters printed by the publication verifier."""

    name: str
    shards_recomputed: int = 0
    shards_matched: int = 0
    crcs_recomputed: int = 0
    crcs_matched: int = 0
    digests_recomputed: int = 0
    digests_matched: int = 0

    def match_shard(self, label: str, actual: bytes, expected: bytes) -> None:
        self.shards_recomputed += 1
        _match(label, actual, expected)
        self.shards_matched += 1

    def match_crc(self, label: str, actual: int, expected: int) -> None:
        self.crcs_recomputed += 1
        _match(label, f"{actual:016x}", f"{expected:016x}")
        self.crcs_matched += 1

    def match_digest(self, label: str, actual: bytes, expected: bytes) -> None:
        self.digests_recomputed += 1
        _match(label, actual.hex(), expected.hex())
        self.digests_matched += 1


@dataclasses.dataclass
class RederivationSummary:
    """Aggregate result returned after every requested re-derivation matches."""

    images: list[ImageSummary]
    normative_gf_vectors: int
    normative_crc_vectors: int
    schemes: set[Scheme]
    elapsed_seconds: float = 0.0

    @property
    def shards(self) -> tuple[int, int]:
        return (
            sum(image.shards_recomputed for image in self.images),
            sum(image.shards_matched for image in self.images),
        )

    @property
    def crcs(self) -> tuple[int, int]:
        return (
            sum(image.crcs_recomputed for image in self.images),
            sum(image.crcs_matched for image in self.images),
        )

    @property
    def digests(self) -> tuple[int, int]:
        return (
            sum(image.digests_recomputed for image in self.images),
            sum(image.digests_matched for image in self.images),
        )

    def report_lines(self) -> list[str]:
        """Render the mandatory per-image and aggregate result summary."""
        lines = [
            (
                "RE-DERIVATION arithmetic: "
                f"GF/RS vectors {self.normative_gf_vectors}/"
                f"{self.normative_gf_vectors}, CRC vectors "
                f"{self.normative_crc_vectors}/{self.normative_crc_vectors}, "
                f"Cauchy schemes {len(self.schemes)}"
            )
        ]
        for image in self.images:
            lines.append(
                f"RE-DERIVATION {image.name}: "
                f"shards {image.shards_matched}/{image.shards_recomputed}, "
                f"CRCs {image.crcs_matched}/{image.crcs_recomputed}, "
                f"digests {image.digests_matched}/{image.digests_recomputed}"
            )
        shards = self.shards
        crcs = self.crcs
        digests = self.digests
        lines.append(
            "RE-DERIVATION PASS: "
            f"shards {shards[1]}/{shards[0]}, CRCs {crcs[1]}/{crcs[0]}, "
            f"digests {digests[1]}/{digests[0]} in "
            f"{self.elapsed_seconds:.3f}s"
        )
        return lines


@dataclasses.dataclass
class TapeContext:
    """Raw tape files and spec-parsed control structures for one image."""

    name: str
    files: dict[int, bytes]
    bootstrap: dict[int, Bootstrap]
    sidecars: dict[int, Sidecar]
    parity_maps: dict[int, ParityMap]
    block_size: int
    tape_uuid: bytes

    def data_blocks(self, replacements: Mapping[int, bytes] | None = None) -> list[bytes]:
        """Concatenate object blocks in file order, excluding all control files."""
        replacements = replacements or {}
        control = set(self.bootstrap) | set(self.sidecars) | set(self.parity_maps)
        blocks: list[bytes] = []
        for number in sorted(self.files):
            if number in control:
                continue
            data = replacements.get(number, self.files[number])
            _require(
                len(data) % self.block_size == 0,
                f"{self.name}/file-{number}: object is not block aligned",
            )
            blocks.extend(
                data[offset : offset + self.block_size]
                for offset in range(0, len(data), self.block_size)
            )
        return blocks


def _parse_scheme(value: object, label: str) -> Scheme:
    _require(isinstance(value, dict), f"{label}: scheme record is not a map")
    _match(f"{label}: scheme id", value.get(1), SCHEME_ID)
    scheme = Scheme(
        k=value.get(2),  # type: ignore[arg-type]
        m=value.get(3),  # type: ignore[arg-type]
        stripes=value.get(4),  # type: ignore[arg-type]
    )
    _require(
        all(isinstance(item, int) for item in dataclasses.astuple(scheme)),
        f"{label}: scheme fields are not integers",
    )
    scheme.validate(label)
    return scheme


def _parse_digest(value: object, label: str) -> DigestRecord:
    _require(isinstance(value, dict), f"{label}: digest record is not a map")
    digest = DigestRecord(
        sha256=value.get(1),  # type: ignore[arg-type]
        tape_file_count=value.get(2),  # type: ignore[arg-type]
        total_data_ordinals=value.get(3),  # type: ignore[arg-type]
        highest_protected_ordinal=value.get(4),  # type: ignore[arg-type]
        is_final=value.get(5),  # type: ignore[arg-type]
    )
    _require(
        isinstance(digest.sha256, bytes) and len(digest.sha256) == 32,
        f"{label}: digest SHA-256 is not 32 bytes",
    )
    _require(
        _is_uint(digest.tape_file_count, UINT32_MAX)
        and _is_uint(digest.total_data_ordinals)
        and _is_uint(digest.highest_protected_ordinal)
        and isinstance(digest.is_final, bool),
        f"{label}: digest scope fields are malformed or out of range",
    )
    return digest


def _parse_directory(value: object, label: str) -> tuple[DirectoryEntry, ...]:
    _require(isinstance(value, dict), f"{label}: directory is not a map")
    _require(
        _is_uint(value.get(1), UINT32_MAX)
        and _is_uint(value.get(2))
        and _is_uint(value.get(3))
        and isinstance(value.get(4), bool),
        f"{label}: directory scope fields are malformed or out of range",
    )
    rows = value.get(5)
    _require(isinstance(rows, list), f"{label}: directory entries are not an array")
    result: list[DirectoryEntry] = []
    for index, row in enumerate(rows):
        _require(isinstance(row, dict), f"{label}: directory row {index} is not a map")
        entry = DirectoryEntry(
            tape_file_number=row.get(1),  # type: ignore[arg-type]
            epoch_id=row.get(2),  # type: ignore[arg-type]
            start=row.get(3),  # type: ignore[arg-type]
            end=row.get(4),  # type: ignore[arg-type]
            sidecar_blocks=row.get(5),  # type: ignore[arg-type]
            header_blocks=row.get(6),  # type: ignore[arg-type]
            parity_blocks=row.get(7),  # type: ignore[arg-type]
            metadata_hash=row.get(8),  # type: ignore[arg-type]
            flags=row.get(9),  # type: ignore[arg-type]
        )
        _require(
            _is_uint(entry.tape_file_number, UINT32_MAX)
            and _is_uint(entry.epoch_id)
            and _is_uint(entry.start)
            and _is_uint(entry.end)
            and _is_uint(entry.sidecar_blocks)
            and _is_uint(entry.header_blocks, UINT32_MAX)
            and _is_uint(entry.parity_blocks, UINT32_MAX)
            and _is_uint(entry.flags, UINT32_MAX),
            f"{label}: directory row {index} has malformed or out-of-range integers",
        )
        _require(
            isinstance(entry.metadata_hash, bytes) and len(entry.metadata_hash) == 32,
            f"{label}: directory row {index} metadata hash is not 32 bytes",
        )
        result.append(entry)
    return tuple(result)


def _parse_parity_map_reference(value: object, label: str) -> int:
    _require(isinstance(value, dict), f"{label}: parity-map reference malformed")
    _require(
        _is_uint(value.get(1), UINT32_MAX)
        and _is_uint(value.get(2))
        and _is_uint(value.get(3), UINT32_MAX)
        and _is_uint(value.get(4))
        and _is_uint(value.get(5))
        and isinstance(value.get(6), bool)
        and isinstance(value.get(7), bytes)
        and len(value[7]) == 32
        and isinstance(value.get(8), bytes)
        and len(value[8]) == 32,
        f"{label}: parity-map reference fields are malformed or out of range",
    )
    return value[1]


def _parse_bootstrap_object_rows(value: object, label: str) -> None:
    _require(isinstance(value, list), f"{label}: object rows are not an array")
    for index, row in enumerate(value):
        _require(isinstance(row, dict), f"{label}: object row {index} is not a map")
        representation = row.get(2)
        common_fields_valid = (
            _is_uint(row.get(1), UINT32_MAX)
            and _is_uint(row.get(3))
            and (4 not in row or isinstance(row[4], bytes))
        )
        if representation == "plaintext":
            representation_fields_valid = (
                _is_uint(row.get(10))
                and _is_uint(row.get(11))
                and _is_uint(row.get(12))
                and isinstance(row.get(13), bytes)
                and len(row[13]) == 32
            )
        elif representation == "encrypted":
            epoch_ids = row.get(22)
            representation_fields_valid = (
                _is_uint(row.get(21))
                and isinstance(epoch_ids, list)
                and all(isinstance(epoch_id, bytes) and len(epoch_id) == 16 for epoch_id in epoch_ids)
                and _is_uint(row.get(23), UINT32_MAX)
            )
        else:
            representation_fields_valid = False
        _require(
            common_fields_valid and representation_fields_valid,
            f"{label}: object row {index} fields are malformed or out of range",
        )


def parse_bootstrap(data: bytes, label: str) -> Bootstrap:
    """Parse a bootstrap directly from its mixed-endian §8.1 wire frame."""
    _require(len(data) >= 0x3C, f"{label}: bootstrap block is too short")
    _match(f"{label}: bootstrap magic", data[:8], BOOTSTRAP_MAGIC)
    _match(f"{label}: schema major", int.from_bytes(data[8:10], "big"), 1)
    block_size = int.from_bytes(data[0x20:0x24], "big")
    _match(f"{label}: bootstrap block size", len(data), block_size)
    header_crc = int.from_bytes(data[0x2C:0x34], "little")
    _match(f"{label}: bootstrap header CRC", crc64_xz(data[:0x2C]), header_crc)
    payload_len = int.from_bytes(data[0x28:0x2C], "little")
    payload_end = 0x34 + payload_len
    _require(payload_end + 8 <= len(data), f"{label}: bootstrap payload is truncated")
    payload_bytes = data[0x34:payload_end]
    payload_crc = int.from_bytes(data[payload_end : payload_end + 8], "little")
    _match(f"{label}: bootstrap payload CRC", crc64_xz(payload_bytes), payload_crc)
    payload = decode_deterministic_cbor(payload_bytes)
    _require(isinstance(payload, dict), f"{label}: bootstrap payload is not a map")
    tape_uuid = data[0x10:0x20]
    scheme = _parse_scheme(payload[1], label) if 1 in payload else None
    digest = _parse_digest(payload[2], label) if 2 in payload else None
    directory = _parse_directory(payload[20], label) if 20 in payload else None
    reference: int | None = None
    if 21 in payload:
        reference = _parse_parity_map_reference(payload[21], label)
    if 30 in payload:
        _parse_bootstrap_object_rows(payload[30], label)
    return Bootstrap(
        block_size=block_size,
        tape_uuid=tape_uuid,
        scheme=scheme,
        digest=digest,
        directory=directory,
        parity_map_reference=reference,
        payload=payload,
    )


def _derived_magic(tape_uuid: bytes, label: bytes) -> bytes:
    return hmac.new(tape_uuid, label, hashlib.sha256).digest()[:8]


def _sidecar_layout(
    block_size: int, stripes: int, parity_count: int, real_count: int
) -> tuple[int, int]:
    _require(block_size >= 0xC0, "sidecar block size is below 0xC0")
    limit = block_size - 8
    offset = 0xB8
    blocks = 1
    inline: int | None = None
    for entry_size in ([16] * parity_count + [8] * real_count):
        if offset + entry_size > limit:
            if blocks == 1 and inline is None:
                inline = offset - 0xB8
            blocks += 1
            offset = 0
        offset += entry_size
    if inline is None:
        inline = offset - 0xB8
    _require(parity_count == stripes * (parity_count // stripes), "bad parity count")
    return blocks, inline


def _extract_index_stream(
    data: bytes,
    copy_start_block: int,
    block_size: int,
    header_blocks: int,
    entry_sizes: Sequence[int],
    label: str,
) -> tuple[bytes, ...]:
    for relative_block in range(header_blocks):
        start = (copy_start_block + relative_block) * block_size
        block = data[start : start + block_size]
        _require(len(block) == block_size, f"{label}: truncated index block")
        expected = _u64(block, block_size - 8)
        _match(
            f"{label}: index block {relative_block} CRC",
            crc64_xz(block[: block_size - 8]),
            expected,
        )
    entries: list[bytes] = []
    relative_block = 0
    offset = 0xB8
    limit = block_size - 8
    for size in entry_sizes:
        if offset + size > limit:
            relative_block += 1
            offset = 0
        _require(relative_block < header_blocks, f"{label}: index exceeds H blocks")
        absolute = (copy_start_block + relative_block) * block_size + offset
        entry = data[absolute : absolute + size]
        _require(len(entry) == size, f"{label}: truncated index entry")
        entries.append(entry)
        offset += size
    return tuple(entries)


def parse_sidecar(data: bytes, tape_uuid: bytes, label: str) -> Sidecar:
    """Parse and hash both sidecar copies and their raw parity payload."""
    _require(len(data) >= 0xC0, f"{label}: sidecar is too short")
    expected_magic = _derived_magic(tape_uuid, SIDECAR_MAGIC_LABEL)
    _match(f"{label}: sidecar magic", data[:8], expected_magic)
    block_size = _u32(data, 0x28)
    _require(block_size >= 0xC0 and len(data) % block_size == 0, f"{label}: bad block size")
    scheme = Scheme(_u16(data, 0x20), _u16(data, 0x22), _u32(data, 0x24))
    scheme.validate(label)
    epoch_id = _u64(data, 0x18)
    start = _u64(data, 0x30)
    end = _u64(data, 0x38)
    real_count = _u64(data, 0x48)
    parity_count = _u32(data, 0x50)
    header_blocks = _u32(data, 0x58)
    inline_bytes = _u32(data, 0x5C)
    total_blocks = _u64(data, 0x60)
    tail_start = _u64(data, 0x70)
    footer_index = _u64(data, 0x78)
    _match(f"{label}: tape UUID", data[8:0x18], tape_uuid)
    _match(f"{label}: schema version", _u32(data, 0x2C), 1)
    _match(f"{label}: logical shard count", _u64(data, 0x40), scheme.k * scheme.stripes)
    _match(f"{label}: real shard count", real_count, end - start)
    _require(0 < real_count <= scheme.k * scheme.stripes, f"{label}: bad real count")
    _match(f"{label}: parity block count", parity_count, scheme.m * scheme.stripes)
    _match(f"{label}: data CRC count", _u32(data, 0x54), real_count)
    computed_h, computed_inline = _sidecar_layout(
        block_size, scheme.stripes, parity_count, real_count
    )
    _match(f"{label}: sidecar H", header_blocks, computed_h)
    _match(f"{label}: inline index bytes", inline_bytes, computed_inline)
    _match(f"{label}: primary start", _u64(data, 0x68), 0)
    _match(f"{label}: tail start", tail_start, header_blocks + parity_count)
    _match(f"{label}: total blocks", total_blocks, 2 * header_blocks + parity_count + 1)
    _match(f"{label}: footer index", footer_index, total_blocks - 1)
    _match(f"{label}: measured block count", len(data) // block_size, total_blocks)

    entry_sizes = [16] * parity_count + [8] * real_count
    primary_entries = _extract_index_stream(
        data, 0, block_size, header_blocks, entry_sizes, f"{label}/primary"
    )
    tail_entries = _extract_index_stream(
        data,
        tail_start,
        block_size,
        header_blocks,
        entry_sizes,
        f"{label}/tail",
    )
    _match(f"{label}: primary/tail index stream", primary_entries, tail_entries)

    primary_header = data[:block_size]
    tail_offset = tail_start * block_size
    tail_header = data[tail_offset : tail_offset + block_size]
    _match(f"{label}: primary copy kind", _u16(primary_header, 0x80), 1)
    _match(f"{label}: tail copy kind", _u16(tail_header, 0x80), 2)
    _match(f"{label}: primary header CRC", crc64_xz(primary_header[:0xB0]), _u64(primary_header, 0xB0))
    _match(f"{label}: tail header CRC", crc64_xz(tail_header[:0xB0]), _u64(tail_header, 0xB0))
    _match(f"{label}: tail fixed metadata", tail_header[:0x80], primary_header[:0x80])
    metadata_hash = hashlib.sha256(
        SIDECAR_METADATA_HASH_DOMAIN + primary_header[:0x80] + b"".join(primary_entries)
    ).digest()
    _match(f"{label}: primary metadata hash", metadata_hash, primary_header[0x88:0xA8])
    _match(f"{label}: tail metadata hash", metadata_hash, tail_header[0x88:0xA8])

    parity_crc64: dict[tuple[int, int], int] = {}
    for index, entry in enumerate(primary_entries[:parity_count]):
        stripe = _u32(entry, 0)
        parity_index = _u16(entry, 4)
        _match(f"{label}: parity entry {index} stripe", stripe, index // scheme.m)
        _match(f"{label}: parity entry {index} parity", parity_index, index % scheme.m)
        _match(f"{label}: parity entry {index} reserved", _u16(entry, 6), 0)
        parity_crc64[(stripe, parity_index)] = _u64(entry, 8)
    data_crc64 = tuple(_u64(entry, 0) for entry in primary_entries[parity_count:])

    parity_shards: dict[tuple[int, int], bytes] = {}
    for parity_index in range(scheme.m):
        for stripe in range(scheme.stripes):
            block_index = header_blocks + parity_index * scheme.stripes + stripe
            offset = block_index * block_size
            parity_shards[(stripe, parity_index)] = data[offset : offset + block_size]

    footer = data[footer_index * block_size : (footer_index + 1) * block_size]
    _match(
        f"{label}: footer magic",
        footer[:8],
        _derived_magic(tape_uuid, SIDECAR_FOOTER_MAGIC_LABEL),
    )
    _match(f"{label}: footer version", _u16(footer, 8), 1)
    _match(f"{label}: footer CRC", crc64_xz(footer[:0x78]), _u64(footer, 0x78))
    _match(f"{label}: footer UUID", footer[0x10:0x20], tape_uuid)
    _match(f"{label}: footer epoch", _u64(footer, 0x20), epoch_id)
    _match(f"{label}: footer start", _u64(footer, 0x28), start)
    _match(f"{label}: footer end", _u64(footer, 0x30), end)
    _match(f"{label}: footer H", _u32(footer, 0x38), header_blocks)
    _match(f"{label}: footer P", _u32(footer, 0x3C), parity_count)
    _match(f"{label}: footer total", _u64(footer, 0x40), total_blocks)
    _match(f"{label}: footer metadata hash", footer[0x58:0x78], metadata_hash)

    return Sidecar(
        scheme=scheme,
        epoch_id=epoch_id,
        start=start,
        end=end,
        block_size=block_size,
        header_blocks=header_blocks,
        parity_blocks=parity_count,
        total_blocks=total_blocks,
        metadata_hash=metadata_hash,
        parity_crc64=parity_crc64,
        data_crc64=data_crc64,
        parity_shards=parity_shards,
    )


def _parse_parity_map_header(
    block: bytes,
    tape_uuid: bytes,
    expected_magic: bytes | None,
    expected_copy_kind: int | None,
    label: str,
) -> dict[str, int | bytes | bool]:
    _require(len(block) >= 0xB8, f"{label}: parity-map header is too short")
    if expected_magic is not None:
        _match(f"{label}: magic", block[:8], expected_magic)
    _match(f"{label}: version", _u16(block, 8), 1)
    if expected_copy_kind is not None:
        _match(f"{label}: copy kind", _u16(block, 0x0A), expected_copy_kind)
    else:
        _match(f"{label}: footer reserved", _u16(block, 0x0A), 0)
    _match(f"{label}: reserved", _u32(block, 0x0C), 0)
    _match(f"{label}: UUID", block[0x10:0x20], tape_uuid)
    _match(f"{label}: CRC", crc64_xz(block[:0xB0]), _u64(block, 0xB0))
    _require(block[0x84] in (0, 1), f"{label}: invalid final-directory byte")
    _require(block[0x85:0x88] == b"\0\0\0", f"{label}: nonzero header pad")
    return {
        "sequence": _u32(block, 0x20),
        "block_size": _u32(block, 0x24),
        "payload_len": _u64(block, 0x28),
        "payload_sha256": block[0x30:0x50],
        "canonical_map_digest": block[0x50:0x70],
        "scope_count": _u32(block, 0x70),
        "scope_total": _u64(block, 0x74),
        "scope_highest": _u64(block, 0x7C),
        "is_final": bool(block[0x84]),
        "copy_blocks": _u64(block, 0x88),
        "total_blocks": _u64(block, 0x90),
        "primary_start": _u64(block, 0x98),
        "tail_start": _u64(block, 0xA0),
        "footer_index": _u64(block, 0xA8),
    }


def parse_parity_map(data: bytes, tape_uuid: bytes, label: str) -> ParityMap:
    """Parse both parity-map copies and independently hash their payload."""
    expected_magic = _derived_magic(tape_uuid, PARITY_MAP_MAGIC_LABEL)
    _require(len(data) >= 0xB8, f"{label}: parity map is too short")
    block_size = _u32(data, 0x24)
    _require(block_size >= 0xB8 and len(data) % block_size == 0, f"{label}: bad block size")
    primary = _parse_parity_map_header(
        data[:block_size], tape_uuid, expected_magic, 1, f"{label}/primary"
    )
    _match(f"{label}: header block size", primary["block_size"], block_size)
    payload_len = primary["payload_len"]
    _require(isinstance(payload_len, int), f"{label}: payload length malformed")
    copy_blocks = (0xB8 + payload_len + block_size - 1) // block_size
    _match(f"{label}: copy block count", primary["copy_blocks"], copy_blocks)
    _match(f"{label}: total block count", primary["total_blocks"], 2 * copy_blocks + 1)
    _match(f"{label}: primary start", primary["primary_start"], 0)
    _match(f"{label}: tail start", primary["tail_start"], copy_blocks)
    _match(f"{label}: footer index", primary["footer_index"], 2 * copy_blocks)
    _match(f"{label}: measured blocks", len(data) // block_size, 2 * copy_blocks + 1)

    tail_offset = copy_blocks * block_size
    tail = _parse_parity_map_header(
        data[tail_offset : tail_offset + block_size],
        tape_uuid,
        expected_magic,
        2,
        f"{label}/tail",
    )
    _match(f"{label}: primary/tail locator", primary, tail)
    primary_payload = data[0xB8 : 0xB8 + payload_len]
    tail_payload = data[tail_offset + 0xB8 : tail_offset + 0xB8 + payload_len]
    _match(f"{label}: primary/tail payload", primary_payload, tail_payload)
    payload_sha256 = hashlib.sha256(primary_payload).digest()
    _match(f"{label}: payload SHA-256", payload_sha256, primary["payload_sha256"])

    footer_offset = 2 * copy_blocks * block_size
    footer = _parse_parity_map_header(
        data[footer_offset : footer_offset + block_size],
        tape_uuid,
        None,
        None,
        f"{label}/footer",
    )
    _match(f"{label}: footer locator", footer, primary)

    payload_value = decode_deterministic_cbor(primary_payload)
    _require(isinstance(payload_value, dict), f"{label}: payload is not a map")
    _match(f"{label}: format id", payload_value.get(1), PARITY_MAP_FORMAT_ID)
    _match(f"{label}: payload UUID", payload_value.get(2), tape_uuid)
    _match(f"{label}: payload sequence", payload_value.get(3), primary["sequence"])
    directory = _parse_directory(payload_value.get(4), label)
    canonical_digest = payload_value.get(5)
    _require(
        isinstance(canonical_digest, bytes) and len(canonical_digest) == 32,
        f"{label}: payload canonical digest malformed",
    )
    _match(
        f"{label}: header/payload canonical digest",
        canonical_digest,
        primary["canonical_map_digest"],
    )
    directory_map = payload_value[4]
    _match(f"{label}: directory scope count", directory_map.get(1), primary["scope_count"])
    _match(f"{label}: directory total", directory_map.get(2), primary["scope_total"])
    _match(f"{label}: directory watermark", directory_map.get(3), primary["scope_highest"])
    _match(f"{label}: directory final flag", directory_map.get(4), primary["is_final"])
    scope = DigestRecord(
        sha256=canonical_digest,
        tape_file_count=primary["scope_count"],  # type: ignore[arg-type]
        total_data_ordinals=primary["scope_total"],  # type: ignore[arg-type]
        highest_protected_ordinal=primary["scope_highest"],  # type: ignore[arg-type]
        is_final=primary["is_final"],  # type: ignore[arg-type]
    )
    return ParityMap(
        sequence=primary["sequence"],  # type: ignore[arg-type]
        block_size=block_size,
        payload=primary_payload,
        payload_sha256=payload_sha256,
        canonical_map_digest=canonical_digest,
        directory=directory,
        scope=scope,
    )


def canonical_map_bytes(entries: Sequence[FilemarkEntry]) -> bytes:
    """Encode the §7.3 array-of-seven-element-arrays projection."""
    ordered = sorted(entries, key=lambda entry: entry.tape_file_number)
    _match(
        "canonical map dense file numbers",
        [entry.tape_file_number for entry in ordered],
        list(range(len(ordered))),
    )
    return encode_deterministic_cbor([entry.projection() for entry in ordered])


def _build_filemark_map(
    context: TapeContext,
    scope_count: int,
    directory: Sequence[DirectoryEntry] | None,
) -> tuple[list[FilemarkEntry], int, int]:
    _require(0 < scope_count <= len(context.files), f"{context.name}: invalid digest scope")
    _match(
        f"{context.name}: dense tape files",
        sorted(context.files),
        list(range(len(context.files))),
    )
    directory_by_file = {
        entry.tape_file_number: entry for entry in (directory or ())
    }
    entries: list[FilemarkEntry] = []
    next_ordinal = 0
    watermark = 0
    for number in range(scope_count):
        data = context.files[number]
        _require(
            len(data) % context.block_size == 0 and data,
            f"{context.name}/file-{number}: invalid block count",
        )
        block_count = len(data) // context.block_size
        if number in context.bootstrap:
            entry = FilemarkEntry(number, 2, block_count)
        elif number in context.parity_maps:
            entry = FilemarkEntry(number, 3, block_count)
        elif number in directory_by_file:
            row = directory_by_file[number]
            _match(
                f"{context.name}/file-{number}: directory block count",
                block_count,
                row.sidecar_blocks,
            )
            parsed = context.sidecars.get(number)
            if parsed is not None:
                _match(f"{context.name}/file-{number}: directory epoch", parsed.epoch_id, row.epoch_id)
                _match(f"{context.name}/file-{number}: directory start", parsed.start, row.start)
                _match(f"{context.name}/file-{number}: directory end", parsed.end, row.end)
                _match(
                    f"{context.name}/file-{number}: directory metadata hash",
                    parsed.metadata_hash,
                    row.metadata_hash,
                )
            entry = FilemarkEntry(
                number,
                1,
                block_count,
                protected_start=row.start,
                protected_end=row.end,
                epoch_id=row.epoch_id,
            )
            watermark = max(watermark, row.end)
        elif number in context.sidecars:
            sidecar = context.sidecars[number]
            entry = FilemarkEntry(
                number,
                1,
                block_count,
                protected_start=sidecar.start,
                protected_end=sidecar.end,
                epoch_id=sidecar.epoch_id,
            )
            watermark = max(watermark, sidecar.end)
        else:
            entry = FilemarkEntry(
                number, 0, block_count, first_data_ordinal=next_ordinal
            )
            next_ordinal += block_count
        entries.append(entry)
    return entries, next_ordinal, watermark


def _validate_map_digest(
    context: TapeContext,
    digest: DigestRecord,
    directory: Sequence[DirectoryEntry] | None,
    summary: ImageSummary,
    label: str,
) -> bytes:
    entries, total, watermark = _build_filemark_map(
        context, digest.tape_file_count, directory
    )
    _match(f"{label}: total data ordinals", total, digest.total_data_ordinals)
    _match(f"{label}: highest protected ordinal", watermark, digest.highest_protected_ordinal)
    encoded = canonical_map_bytes(entries)
    actual = hashlib.sha256(encoded).digest()
    summary.match_digest(f"{label}: canonical filemark-map digest", actual, digest.sha256)
    return actual


def _verify_sidecar_parity(
    sidecar: Sidecar,
    data_blocks: Sequence[bytes],
    summary: ImageSummary,
    label: str,
) -> None:
    _require(
        sidecar.end <= len(data_blocks),
        f"{label}: protected range exceeds available object data",
    )
    real_shards = data_blocks[sidecar.start : sidecar.end]
    for ordinal_offset, shard in enumerate(real_shards):
        summary.match_crc(
            f"{label}: data CRC ordinal {sidecar.start + ordinal_offset}",
            crc64_xz(shard),
            sidecar.data_crc64[ordinal_offset],
        )
    for key, shard in sorted(sidecar.parity_shards.items()):
        summary.match_crc(
            f"{label}: parity CRC stripe/parity {key}",
            crc64_xz(shard),
            sidecar.parity_crc64[key],
        )

    matrix = cauchy_matrix(sidecar.scheme.k, sidecar.scheme.m)
    zero = bytes(sidecar.block_size)
    real_count = len(real_shards)
    for stripe in range(sidecar.scheme.stripes):
        stripe_data = []
        for data_index in range(sidecar.scheme.k):
            logical_offset = data_index * sidecar.scheme.stripes + stripe
            stripe_data.append(
                real_shards[logical_offset] if logical_offset < real_count else zero
            )
        computed = encode_parity(stripe_data, matrix)
        for parity_index, shard in enumerate(computed):
            summary.match_shard(
                f"{label}: parity stripe {stripe}, index {parity_index}",
                shard,
                sidecar.parity_shards[(stripe, parity_index)],
            )


def _tape_files(directory: pathlib.Path) -> dict[int, bytes]:
    result: dict[int, bytes] = {}
    for path in sorted(directory.glob("*.bin")):
        match = TAPE_FILE_PATTERN.match(path.name)
        if match is None:
            continue
        number = int(match.group(1))
        _require(number not in result, f"{directory}: duplicate tape file {number}")
        result[number] = path.read_bytes()
    _require(result, f"{directory}: no tape-file artifacts")
    _match(f"{directory}: dense tape files", sorted(result), list(range(len(result))))
    return result


def _make_tape_context(name: str, files: dict[int, bytes]) -> TapeContext:
    _require(0 in files, f"{name}: tape file zero is absent")
    first = parse_bootstrap(files[0], f"{name}/file-0")
    block_size = first.block_size
    tape_uuid = first.tape_uuid
    sidecar_magic = _derived_magic(tape_uuid, SIDECAR_MAGIC_LABEL)
    parity_map_magic = _derived_magic(tape_uuid, PARITY_MAP_MAGIC_LABEL)
    bootstraps: dict[int, Bootstrap] = {0: first}
    sidecars: dict[int, Sidecar] = {}
    parity_maps: dict[int, ParityMap] = {}
    for number, data in sorted(files.items()):
        if number == 0:
            continue
        _require(
            len(data) % block_size == 0,
            f"{name}/file-{number}: size is not a block multiple",
        )
        if data.startswith(BOOTSTRAP_MAGIC):
            bootstraps[number] = parse_bootstrap(data, f"{name}/file-{number}")
        elif data.startswith(sidecar_magic):
            sidecars[number] = parse_sidecar(data, tape_uuid, f"{name}/file-{number}")
        elif data.startswith(parity_map_magic):
            parity_maps[number] = parse_parity_map(
                data, tape_uuid, f"{name}/file-{number}"
            )
    return TapeContext(
        name=name,
        files=files,
        bootstrap=bootstraps,
        sidecars=sidecars,
        parity_maps=parity_maps,
        block_size=block_size,
        tape_uuid=tape_uuid,
    )


def _bootstrap_directory(context: TapeContext, bootstrap: Bootstrap) -> tuple[DirectoryEntry, ...] | None:
    if bootstrap.directory is not None:
        return bootstrap.directory
    if bootstrap.parity_map_reference is not None:
        parity_map = context.parity_maps.get(bootstrap.parity_map_reference)
        _require(
            parity_map is not None,
            f"{context.name}: referenced parity map {bootstrap.parity_map_reference} absent",
        )
        return parity_map.directory
    return None


def _verify_context(
    context: TapeContext, summary: ImageSummary, schemes: set[Scheme]
) -> dict[str, Any]:
    declared_schemes = {
        bootstrap.scheme
        for bootstrap in context.bootstrap.values()
        if bootstrap.scheme is not None
    }
    schemes.update(declared_schemes)
    schemes.update(sidecar.scheme for sidecar in context.sidecars.values())
    if context.sidecars:
        _require(
            len(declared_schemes) == 1,
            f"{context.name}: sidecars do not have one bootstrap scheme",
        )
        declared = next(iter(declared_schemes))
        for number, sidecar in context.sidecars.items():
            _match(f"{context.name}/file-{number}: recorded scheme", sidecar.scheme, declared)

    bootstrap_digests: dict[int, bytes] = {}
    for number, bootstrap in sorted(context.bootstrap.items()):
        if bootstrap.digest is not None:
            bootstrap_digests[number] = _validate_map_digest(
                context,
                bootstrap.digest,
                _bootstrap_directory(context, bootstrap),
                summary,
                f"{context.name}/bootstrap-{number}",
            )

    parity_map_digests: dict[int, bytes] = {}
    for number, parity_map in sorted(context.parity_maps.items()):
        summary.match_digest(
            f"{context.name}/parity-map-{number}: payload SHA-256",
            hashlib.sha256(parity_map.payload).digest(),
            parity_map.payload_sha256,
        )
        parity_map_digests[number] = _validate_map_digest(
            context,
            parity_map.scope,
            parity_map.directory,
            summary,
            f"{context.name}/parity-map-{number}",
        )

    data_blocks = context.data_blocks()
    for number, sidecar in sorted(context.sidecars.items()):
        summary.match_digest(
            f"{context.name}/sidecar-{number}: canonical metadata hash",
            sidecar.metadata_hash,
            context.files[number][0x88:0xA8],
        )
        _verify_sidecar_parity(
            sidecar, data_blocks, summary, f"{context.name}/sidecar-{number}"
        )
    return {
        "bootstrap_digests": bootstrap_digests,
        "parity_map_digests": parity_map_digests,
        "data_blocks": data_blocks,
    }


class _ExpectedPins:
    """Track every SHA-256-like pin in REM-PARITY expected.json documents."""

    def __init__(self, rem_root: pathlib.Path) -> None:
        self.rem_root = rem_root
        self.expected: dict[tuple[str, str], str] = {}
        self.matched: set[tuple[str, str]] = set()
        for base in ("positive", "damage-matrix"):
            for path in sorted((rem_root / base).glob("*/expected.json")):
                document = json.loads(path.read_text(encoding="utf-8"))
                for key, value in self._digest_values(document):
                    if (
                        HEX_SHA256_PATTERN.fullmatch(value)
                        and ("sha256" in key or "digest" in key)
                    ):
                        relative = path.relative_to(rem_root).as_posix()
                        self.expected[(relative, key)] = value

    @classmethod
    def _digest_values(
        cls, value: object, prefix: str = ""
    ) -> Iterable[tuple[str, str]]:
        """Yield recursively named 64-hex strings so future pins cannot be missed."""
        if isinstance(value, dict):
            for key, item in value.items():
                name = f"{prefix}.{key}" if prefix else str(key)
                yield from cls._digest_values(item, name)
        elif isinstance(value, list):
            for index, item in enumerate(value):
                yield from cls._digest_values(item, f"{prefix}[{index}]")
        elif isinstance(value, str) and HEX_SHA256_PATTERN.fullmatch(value):
            yield prefix, value

    def match(
        self,
        path: pathlib.Path,
        key: str,
        actual: bytes,
        summary: ImageSummary,
    ) -> None:
        identity = (path.relative_to(self.rem_root).as_posix(), key)
        _require(identity in self.expected, f"unexpected expected.json digest pin {identity}")
        _require(identity not in self.matched, f"expected.json digest pin checked twice: {identity}")
        summary.match_digest(
            f"{identity[0]}:{key}", actual, bytes.fromhex(self.expected[identity])
        )
        self.matched.add(identity)

    def finish(self) -> None:
        missing = sorted(set(self.expected) - self.matched)
        _require(not missing, f"expected.json SHA-256 pins were not re-derived: {missing}")


def _verify_positive_expected(
    context: TapeContext,
    derived: Mapping[str, Any],
    expected_path: pathlib.Path,
    pins: _ExpectedPins,
    summary: ImageSummary,
) -> None:
    expected = json.loads(expected_path.read_text(encoding="utf-8"))
    vector_id = expected["vector_id"]
    bootstrap_digests: dict[int, bytes] = derived["bootstrap_digests"]
    data_blocks: list[bytes] = derived["data_blocks"]
    if "map_sha256" in expected:
        _require(bootstrap_digests, f"{vector_id}: no bootstrap digest was derived")
        pins.match(
            expected_path,
            "map_sha256",
            bootstrap_digests[max(bootstrap_digests)],
            summary,
        )
    if "final_map_sha256" in expected:
        _require(bootstrap_digests, f"{vector_id}: no final bootstrap digest was derived")
        pins.match(
            expected_path,
            "final_map_sha256",
            bootstrap_digests[max(bootstrap_digests)],
            summary,
        )
    if "payload_sha256" in expected:
        _require(context.parity_maps, f"{vector_id}: no parity-map payload was parsed")
        payload = context.parity_maps[min(context.parity_maps)].payload
        pins.match(expected_path, "payload_sha256", hashlib.sha256(payload).digest(), summary)
    if "recovered_block_sha256" in expected:
        ordinal = expected.get("recover_ordinal")
        _require(
            isinstance(ordinal, int) and 0 <= ordinal < len(data_blocks),
            f"{vector_id}: recovery ordinal is malformed",
        )
        block = data_blocks[ordinal]
        pins.match(
            expected_path, "recovered_block_sha256", hashlib.sha256(block).digest(), summary
        )
        prefix = expected.get("recovered_block_hex_prefix")
        if prefix is not None:
            _match(f"{vector_id}: recovered block prefix", block[: len(prefix) // 2].hex(), prefix)
    if "plaintext_digest" in expected:
        object_files = [
            context.files[number]
            for number in sorted(context.files)
            if number
            not in set(context.bootstrap) | set(context.sidecars) | set(context.parity_maps)
        ]
        _match(f"{vector_id}: object count", len(object_files), 1)
        pins.match(
            expected_path,
            "plaintext_digest",
            hashlib.sha256(object_files[0]).digest(),
            summary,
        )
        rows = next(
            (
                bootstrap.payload[30]
                for bootstrap in context.bootstrap.values()
                if 30 in bootstrap.payload
            ),
            None,
        )
        _require(isinstance(rows, list) and len(rows) == 1, f"{vector_id}: object row absent")
        row = rows[0]
        _require(isinstance(row, dict), f"{vector_id}: object row malformed")
        manifest_lba = row.get(10)
        manifest_size = row.get(11)
        _require(
            isinstance(manifest_lba, int)
            and isinstance(manifest_size, int)
            and manifest_lba >= 0
            and manifest_size > 0,
            f"{vector_id}: manifest anchor malformed",
        )
        start = manifest_lba * context.block_size
        manifest = object_files[0][start : start + manifest_size]
        _match(f"{vector_id}: manifest extent", len(manifest), manifest_size)
        manifest_digest = hashlib.sha256(manifest).digest()
        _match(f"{vector_id}: bootstrap manifest digest", manifest_digest, row.get(13))
        pins.match(expected_path, "manifest_sha256", manifest_digest, summary)


def _verify_normative_definitions(rem_root: pathlib.Path) -> tuple[int, int]:
    """Self-check the independent primitives against the spec's inline vectors."""
    _match("spec gf_inv(0x02)", gf_inv(0x02), 0x8E)
    _match("spec gf_inv(0x03)", gf_inv(0x03), 0xF4)
    matrix = cauchy_matrix(2, 2)
    _match("spec k=2,m=2 Cauchy matrix", matrix, ((0x8E, 0xF4), (0xF4, 0x8E)))
    parity = encode_parity(
        (bytes.fromhex("01020304"), bytes.fromhex("10203040")), matrix
    )
    _match(
        "spec k=2,m=2 parity",
        parity,
        (bytes.fromhex("75ea9fc9"), bytes.fromhex("fce519d7")),
    )
    crc_vectors = (
        (b"123456789", 0x995DC9BBDF1939FA),
        (b"", 0),
        (b"\x00", 0x1FADA17364673F59),
        (b"\xff", 0xFF00000000000000),
        (b"\x00" * 262144, 0x261BDF3D299838FC),
        (b"\xff" * 262144, 0x55433DD0F38908BA),
    )
    for data, expected in crc_vectors:
        _match(f"spec CRC vector length {len(data)}", crc64_xz(data), expected)

    normative_entries = (
        FilemarkEntry(0, 2, 1),
        FilemarkEntry(1, 0, 3, first_data_ordinal=0),
        FilemarkEntry(2, 1, 2, protected_start=0, protected_end=3, epoch_id=7),
    )
    normative_cbor = bytes.fromhex(
        "8387000201f6f6f6f68701000300f6f6f687020102f6000307"
    )
    _match("spec canonical map CBOR", canonical_map_bytes(normative_entries), normative_cbor)
    _match(
        "spec canonical map SHA-256",
        hashlib.sha256(normative_cbor).hexdigest(),
        "548ca6c967073a6c1ad011d10fc132c2739e251d015ea45a628bbec96892c26b",
    )

    arithmetic = json.loads((rem_root / "vectors.json").read_text(encoding="utf-8"))[
        "arithmetic"
    ]
    reed_solomon = arithmetic["reed_solomon"]
    _match(
        "archive arithmetic generator rows",
        [bytes(row).hex() for row in matrix],
        reed_solomon["generator_rows_hex"],
    )
    _match(
        "archive arithmetic parity",
        [shard.hex() for shard in parity],
        reed_solomon["parity_hex"],
    )
    for vector in arithmetic["crc64_xz"]:
        _match(
            f"archive arithmetic CRC {vector['input_hex']!r}",
            f"{crc64_xz(bytes.fromhex(vector['input_hex'])):016x}",
            vector["expected"],
        )
    _match(
        "archive arithmetic canonical CBOR",
        normative_cbor.hex(),
        arithmetic["canonical_map_projection"]["cbor_hex"],
    )
    _match(
        "archive arithmetic canonical SHA-256",
        hashlib.sha256(normative_cbor).hexdigest(),
        arithmetic["canonical_map_projection"]["sha256"],
    )
    return 5, len(crc_vectors)


def _positive_contexts(
    rem_root: pathlib.Path,
    pins: _ExpectedPins,
    images: list[ImageSummary],
    schemes: set[Scheme],
) -> tuple[list[TapeContext], dict[str, tuple[TapeContext, int]], dict[str, TapeContext]]:
    contexts: list[TapeContext] = []
    artifact_catalog: dict[str, tuple[TapeContext, int]] = {}
    pair_catalog: dict[str, TapeContext] = {}
    for directory in sorted((rem_root / "positive").iterdir()):
        if not directory.is_dir():
            continue
        summary = ImageSummary(f"positive/{directory.name}")
        context = _make_tape_context(summary.name, _tape_files(directory))
        derived = _verify_context(context, summary, schemes)
        _verify_positive_expected(
            context, derived, directory / "expected.json", pins, summary
        )
        contexts.append(context)
        images.append(summary)
        for number, data in context.files.items():
            artifact_catalog.setdefault(hashlib.sha256(data).hexdigest(), (context, number))
        if len(context.sidecars) == 1:
            sidecar_number = next(iter(context.sidecars))
            object_numbers = [
                number
                for number in sorted(context.files)
                if number
                not in set(context.bootstrap)
                | set(context.sidecars)
                | set(context.parity_maps)
            ]
            pair_bytes = b"".join(context.files[number] for number in object_numbers)
            pair_bytes += context.files[sidecar_number]
            pair_catalog[hashlib.sha256(pair_bytes).hexdigest()] = context
    return contexts, artifact_catalog, pair_catalog


def _split_damage_layout(
    directory: pathlib.Path, source: bytes, block_size: int
) -> dict[int, bytes]:
    layout = json.loads((directory / "tape-layout.json").read_text(encoding="utf-8"))
    files: dict[int, bytes] = {}
    for row in layout["tape_files"]:
        number = row["tape_file_number"]
        start = row["concatenated_start_block"] * block_size
        end = start + row["block_count"] * block_size
        files[number] = source[start:end]
        _match(
            f"{directory.name}: layout extent for file {number}",
            len(files[number]),
            row["block_count"] * block_size,
        )
        artifact = directory / row["artifact"]
        if artifact.is_file():
            _match(f"{directory.name}: split artifact {artifact.name}", files[number], artifact.read_bytes())
    return files


def _match_expected_recovered_block(
    directory: pathlib.Path,
    data_blocks: Sequence[bytes],
    pins: _ExpectedPins,
    summary: ImageSummary,
) -> None:
    expected_path = directory / "expected.json"
    expected = json.loads(expected_path.read_text(encoding="utf-8"))
    if "recovered_block_sha256" not in expected:
        return
    ordinal = expected.get("recovery_target_ordinal")
    _require(
        isinstance(ordinal, int) and 0 <= ordinal < len(data_blocks),
        f"{directory.name}: recovery target ordinal is malformed",
    )
    pins.match(
        expected_path,
        "recovered_block_sha256",
        hashlib.sha256(data_blocks[ordinal]).digest(),
        summary,
    )


def _verify_damage_matrix(
    rem_root: pathlib.Path,
    artifact_catalog: Mapping[str, tuple[TapeContext, int]],
    pair_catalog: Mapping[str, TapeContext],
    pins: _ExpectedPins,
    images: list[ImageSummary],
    schemes: set[Scheme],
) -> None:
    damage_root = rem_root / "damage-matrix"
    for directory in sorted(damage_root.iterdir()):
        if not directory.is_dir():
            continue
        summary = ImageSummary(f"damage-matrix/{directory.name}")
        source = (directory / "source-artifact.bin").read_bytes()
        source_sha = hashlib.sha256(source).hexdigest()
        layout_path = directory / "tape-layout.json"
        data_blocks: list[bytes] = []

        if directory.name == "multi-parity-map-selection":
            files = _split_damage_layout(directory, source, 4096)
            context = _make_tape_context(summary.name, files)
            derived = _verify_context(context, summary, schemes)
            selected = context.parity_maps[4]
            entries, _, _ = _build_filemark_map(
                context, selected.scope.tape_file_count, selected.directory
            )
            recovered_map = canonical_map_bytes(entries)
            expected_path = directory / "expected.json"
            expected = json.loads(expected_path.read_text(encoding="utf-8"))
            _match(
                f"{summary.name}: recovered map CBOR",
                recovered_map.hex(),
                expected["recovered_map_cbor_hex"],
            )
            pins.match(
                expected_path,
                "recovered_map_sha256",
                hashlib.sha256(recovered_map).digest(),
                summary,
            )
            data_blocks = derived["data_blocks"]
        elif layout_path.is_file():
            positive = pair_catalog.get(source_sha)
            _require(
                positive is not None,
                f"{summary.name}: concatenated source has no positive image",
            )
            files = _split_damage_layout(directory, source, positive.block_size)
            object_rows = json.loads(layout_path.read_text(encoding="utf-8"))["tape_files"]
            object_numbers = [
                row["tape_file_number"]
                for row in object_rows
                if "object" in row["artifact"]
            ]
            sidecar_numbers = [
                row["tape_file_number"]
                for row in object_rows
                if "sidecar" in row["artifact"]
            ]
            _match(f"{summary.name}: object file count", len(object_numbers), 1)
            _match(f"{summary.name}: sidecar file count", len(sidecar_numbers), 1)
            object_data = files[object_numbers[0]]
            data_blocks = [
                object_data[offset : offset + positive.block_size]
                for offset in range(0, len(object_data), positive.block_size)
            ]
            sidecar = parse_sidecar(
                files[sidecar_numbers[0]], positive.tape_uuid, f"{summary.name}/sidecar"
            )
            schemes.add(sidecar.scheme)
            _verify_sidecar_parity(sidecar, data_blocks, summary, f"{summary.name}/sidecar")
            summary.match_digest(
                f"{summary.name}: canonical metadata hash",
                sidecar.metadata_hash,
                files[sidecar_numbers[0]][0x88:0xA8],
            )
        else:
            match = artifact_catalog.get(source_sha)
            _require(match is not None, f"{summary.name}: source is not a positive artifact")
            positive, number = match
            if number in positive.sidecars:
                sidecar = parse_sidecar(source, positive.tape_uuid, f"{summary.name}/sidecar")
                data_blocks = positive.data_blocks()
                _verify_sidecar_parity(
                    sidecar, data_blocks, summary, f"{summary.name}/sidecar"
                )
                summary.match_digest(
                    f"{summary.name}: canonical metadata hash",
                    sidecar.metadata_hash,
                    source[0x88:0xA8],
                )
            elif number in positive.bootstrap:
                bootstrap = parse_bootstrap(source, f"{summary.name}/bootstrap")
                if bootstrap.digest is not None:
                    _validate_map_digest(
                        positive,
                        bootstrap.digest,
                        _bootstrap_directory(positive, bootstrap),
                        summary,
                        f"{summary.name}/bootstrap",
                    )
                data_blocks = positive.data_blocks()
            elif number in positive.parity_maps:
                parity_map = parse_parity_map(
                    source, positive.tape_uuid, f"{summary.name}/parity-map"
                )
                summary.match_digest(
                    f"{summary.name}: payload SHA-256",
                    hashlib.sha256(parity_map.payload).digest(),
                    parity_map.payload_sha256,
                )
                _validate_map_digest(
                    positive,
                    parity_map.scope,
                    parity_map.directory,
                    summary,
                    f"{summary.name}/parity-map",
                )
                data_blocks = positive.data_blocks()
            else:
                replacements = {number: source}
                data_blocks = positive.data_blocks(replacements)
                _require(
                    len(positive.sidecars) == 1,
                    f"{summary.name}: object source has no unique companion sidecar",
                )
                sidecar_number = next(iter(positive.sidecars))
                sidecar = positive.sidecars[sidecar_number]
                _verify_sidecar_parity(
                    sidecar, data_blocks, summary, f"{summary.name}/sidecar"
                )
                summary.match_digest(
                    f"{summary.name}: canonical metadata hash",
                    sidecar.metadata_hash,
                    positive.files[sidecar_number][0x88:0xA8],
                )

        _match_expected_recovered_block(directory, data_blocks, pins, summary)
        images.append(summary)


def rederive_publication_vectors(publication_root: pathlib.Path) -> RederivationSummary:
    """Re-derive all pinned REM-PARITY arithmetic and image-level values."""
    started = time.perf_counter()
    publication_root = publication_root.resolve()
    rem_root = publication_root / "rem-parity-1"
    _require(rem_root.is_dir(), f"REM-PARITY vector root is absent: {rem_root}")
    normative_gf, normative_crc = _verify_normative_definitions(rem_root)
    pins = _ExpectedPins(rem_root)
    images: list[ImageSummary] = []
    schemes: set[Scheme] = set()
    _, artifact_catalog, pair_catalog = _positive_contexts(
        rem_root, pins, images, schemes
    )
    _verify_damage_matrix(
        rem_root, artifact_catalog, pair_catalog, pins, images, schemes
    )
    pins.finish()
    for scheme in schemes:
        scheme.validate(f"archive scheme {scheme}")
        cauchy_matrix(scheme.k, scheme.m)
    summary = RederivationSummary(
        images=images,
        normative_gf_vectors=normative_gf,
        normative_crc_vectors=normative_crc,
        schemes=schemes,
        elapsed_seconds=time.perf_counter() - started,
    )
    _require(summary.shards[0] > 0, "no parity shards were re-derived")
    _require(summary.crcs[0] > 0, "no shard CRCs were re-derived")
    _require(summary.digests[0] > 0, "no SHA-256 digests were re-derived")
    return summary


__all__ = [
    "RederivationError",
    "canonical_map_bytes",
    "cauchy_matrix",
    "crc64_xz",
    "decode_deterministic_cbor",
    "encode_deterministic_cbor",
    "encode_parity",
    "gf_inv",
    "gf_mul",
    "rederive_publication_vectors",
]
