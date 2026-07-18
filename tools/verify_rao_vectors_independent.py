#!/usr/bin/env python3
"""Independently re-derive and open the RAO positive publication vectors.

This verifier deliberately avoids the Remanence Rust crates. It rebuilds the
RAO-TV-P1 and RAO-TV-D1 plaintext tar streams, the additional positive
plaintext vectors, and deterministic manifest CBOR. It independently opens
the pinned RAO-TV-E2 and encrypted RAO-TV-D1 objects with generic X25519,
HKDF-SHA-256, and ChaCha20-Poly1305 primitives, then verifies the recovered
canonical bytes, plaintext digest, manifest, and per-file digests.

With --check-plaintext-interop it also exercises the Section 14 plaintext
interop gate for the positive plaintext vectors using GNU tar, bsdtar, and
Python's tarfile module.

"""

from __future__ import annotations

import argparse
import hashlib
import hmac
import io
import json
import pathlib
import shutil
import subprocess
import sys
import tarfile
import tempfile
from dataclasses import dataclass, field
from typing import Any

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.x25519 import (
    X25519PrivateKey,
    X25519PublicKey,
)
from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305


ROOT = pathlib.Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "fixtures" / "rao"
TAR_RECORD_SIZE = 512
FORMAT_ID = "rao-v1"
SCHEMA_VERSION = "1.0"
SCHEMA_VERSION_XATTRS = "1.1"
MANIFEST_PATH = "_remanence/manifest.cbor"
TYPE_REGULAR = b"0"[0]
TYPE_HARDLINK = b"1"[0]
TYPE_SYMLINK = b"2"[0]
TYPE_DIRECTORY = b"5"[0]
TYPE_PAX_EXTENDED = b"x"[0]
TYPE_PAX_GLOBAL = b"g"[0]
PAD_KEY = "REMANENCE.pad"
RAO_HEADER_LEN = 128
RAO_FOOTER = b"RAO1_STREAM_END."
LABEL_SALT = b"rao2-salt-v1"
LABEL_OBJECT = b"rao2-object-v1"
LABEL_METADATA = b"rao2-metadata-v1"
LABEL_PAYLOAD = b"rao2-payload-v1"
WRAP_INFO_PREFIX = b"rao-wrap-v1\0"
HPKE_VERSION_LABEL = b"HPKE-v1"
HPKE_KEM_ID = 0x0020
HPKE_KDF_ID = 0x0001
HPKE_AEAD_ID = 0x0003
RAO_KEY_FRAME_MIN_LEN = 103
RAO_KEY_FRAME_MAX_LEN = 4096
RAO_KEY_FRAME_MAX_SLOTS = 8


@dataclass
class PlaintextVector:
    vector_id: str
    chunk_size: int
    plaintext: bytes
    expected_files: dict[str, bytes]
    expected_symlinks: dict[str, str]
    expected_hardlinks: dict[str, str]
    expected_directories: set[str]


@dataclass
class FileSpec:
    path: str
    file_id: str
    data: bytes
    entry_type: str = "regular"
    link_target: str | None = None
    executable: bool | None = None
    mtime: str | None = None
    xattrs: dict[str, bytes] = field(default_factory=dict)

    @property
    def size_bytes(self) -> int:
        if self.entry_type != "regular":
            return 0
        return len(self.data)

    @property
    def file_sha256(self) -> bytes | None:
        if self.entry_type != "regular":
            return None
        return sha256(self.data)


@dataclass
class FileLayout:
    path: str
    file_id: str
    size_bytes: int
    file_sha256: bytes | None
    entry_type: str
    link_target: str | None
    xattrs: dict[str, bytes]
    executable: bool | None
    first_chunk_lba: int | None
    chunk_count: int
    pax_header_offset: int
    data_offset: int
    pad_spaces: int


@dataclass
class EncryptedHeader:
    bytes: bytes
    chunk_size: int
    salt: bytes
    metadata_frame_len: int
    object_id: str
    wrap_suite: int
    key_frame_len: int


@dataclass
class RecipientSlot:
    slot_index: int
    recipient_epoch_id: bytes
    epoch_label: str
    enc: bytes
    ciphertext: bytes


def sha256(data: bytes) -> bytes:
    return hashlib.sha256(data).digest()


def hx(data: bytes) -> str:
    return data.hex()


def load(directory: pathlib.Path, name: str) -> dict[str, Any]:
    with (directory / name).open("r", encoding="utf-8") as handle:
        return json.load(handle)


def assert_eq(actual: Any, expected: Any, label: str) -> None:
    if actual != expected:
        raise AssertionError(f"{label}: got {actual!r}, expected {expected!r}")


def round_up(value: int, unit: int) -> int:
    if unit <= 0:
        raise ValueError("unit must be positive")
    remainder = value % unit
    return value if remainder == 0 else value + unit - remainder


def decimal_digits(value: int) -> int:
    return len(str(value))


def pax_record_len(key: str, value_len: int) -> int:
    base = len(key.encode("ascii")) + value_len + 3
    digits = decimal_digits(base)
    while True:
        total = base + digits
        next_digits = decimal_digits(total)
        if next_digits == digits:
            return total
        digits = next_digits


def encode_pax_record(key: str, value: str) -> bytes:
    value_bytes = value.encode("utf-8")
    length = pax_record_len(key, len(value_bytes))
    line = str(length).encode("ascii") + b" " + key.encode("ascii") + b"=" + value_bytes + b"\n"
    if len(line) != length:
        raise AssertionError("pax record fixed-point mismatch")
    return line


def encode_pax_records(records: dict[str, str]) -> bytes:
    out = bytearray()
    for key in sorted(records):
        out.extend(encode_pax_record(key, records[key]))
    return bytes(out)


def pax_records_len(records: dict[str, str]) -> int:
    return sum(pax_record_len(key, len(value.encode("utf-8"))) for key, value in records.items())


def max_pad_len_for_target(base_len: int, target: int) -> int:
    lo = 0
    hi = target
    best = 0
    while lo <= hi:
        mid = lo + (hi - lo) // 2
        length = base_len + pax_record_len(PAD_KEY, mid)
        if length <= target:
            best = mid
            lo = mid + 1
        elif mid == 0:
            break
        else:
            hi = mid - 1
    return best


def with_alignment_pad(offset: int, chunk_size: int, base_records: dict[str, str]) -> dict[str, str]:
    base_len = pax_records_len(base_records)
    min_rounded = round_up(base_len + pax_record_len(PAD_KEY, 0), TAR_RECORD_SIZE)
    residue = (chunk_size - ((offset + 1024) % chunk_size)) % chunk_size
    while min_rounded % chunk_size != residue:
        min_rounded += TAR_RECORD_SIZE

    target = min_rounded
    max_target = min_rounded + chunk_size * 4
    while target <= max_target:
        pad_len = max_pad_len_for_target(base_len, target)
        body_len = base_len + pax_record_len(PAD_KEY, pad_len)
        if round_up(body_len, TAR_RECORD_SIZE) == target:
            records = dict(base_records)
            records[PAD_KEY] = " " * pad_len
            return records
        target += chunk_size
    raise AssertionError("could not solve REMANENCE.pad")


def write_octal(block: bytearray, start: int, end: int, value: int) -> None:
    width = end - start
    encoded = f"{value:0{width - 1}o}".encode("ascii")
    if len(encoded) > width - 1:
        raise ValueError(f"octal field overflow for {value}")
    block[start:end] = b"\0" * width
    block[start : start + len(encoded)] = encoded


def write_name(block: bytearray, start: int, end: int, value: str) -> None:
    field_len = end - start
    data = value.encode("utf-8")
    if len(data) > field_len:
        data = b"remanence/pax-path"
    block[start : start + len(data)] = data


def write_name_field(block: bytearray, start: int, end: int, value: bytes) -> None:
    block[start : start + min(len(value), end - start)] = value[: end - start]


def encode_header(path: str, size: int, typeflag: int, mode: int) -> bytes:
    block = bytearray(TAR_RECORD_SIZE)
    write_name(block, 0, 100, path)
    write_octal(block, 100, 108, mode)
    write_octal(block, 108, 116, 0)
    write_octal(block, 116, 124, 0)
    write_octal(block, 124, 136, size)
    write_octal(block, 136, 148, 0)
    block[148:156] = b" " * 8
    block[156] = typeflag
    block[257:263] = b"ustar\0"
    block[263:265] = b"00"
    write_name_field(block, 265, 297, b"remanence")
    write_name_field(block, 297, 329, b"remanence")
    checksum = sum(block)
    encoded = f"{checksum:06o}\0 ".encode("ascii")
    block[148:156] = encoded
    return bytes(block)


def is_portable_ustar_name(path: str) -> bool:
    data = path.encode("utf-8")
    return bool(data) and len(data) <= 100 and all(0x20 <= byte < 0x7F for byte in data)


def encode_pax_backed_regular_header(path: str, size: int, mode: int) -> bytes:
    header_path = path if is_portable_ustar_name(path) else "remanence/pax-path"
    header_size = size if size <= 0o77777777777 else 0
    return encode_header(header_path, header_size, TYPE_REGULAR, mode)


def is_portable_ustar_linkname(target: str) -> bool:
    data = target.encode("utf-8")
    return len(data) <= 100 and all(byte != 0 and byte >= 0x20 for byte in data)


def encode_header_with_link(path: str, size: int, typeflag: int, mode: int, linkname: str) -> bytes:
    block = bytearray(encode_header(path, size, typeflag, mode))
    data = linkname.encode("utf-8")
    block[157 : 157 + min(len(data), 100)] = data[:100]
    block[148:156] = b" " * 8
    checksum = sum(block)
    encoded = f"{checksum:06o}\0 ".encode("ascii")
    block[148:156] = encoded
    return bytes(block)


def encode_pax_backed_symlink_header(path: str, target: str, target_in_pax: bool) -> bytes:
    header_path = path if is_portable_ustar_name(path) else "remanence/pax-path"
    linkname = "remanence/pax-linkpath" if target_in_pax else target
    return encode_header_with_link(header_path, 0, TYPE_SYMLINK, 0o777, linkname)


def encode_pax_backed_hardlink_header(path: str, target: str, target_in_pax: bool) -> bytes:
    header_path = path if is_portable_ustar_name(path) else "remanence/pax-path"
    linkname = "remanence/pax-linkpath" if target_in_pax else target
    return encode_header_with_link(header_path, 0, TYPE_HARDLINK, 0o644, linkname)


def encode_pax_backed_directory_header(path: str) -> bytes:
    header_path = path if is_portable_ustar_name(path) else "remanence/pax-path"
    return encode_header(header_path, 0, TYPE_DIRECTORY, 0o755)


def chunk_count(size_bytes: int, chunk_size: int) -> int:
    return 0 if size_bytes == 0 else (size_bytes - 1) // chunk_size + 1


def cbor_type_len(major: int, value: int) -> bytes:
    prefix = major << 5
    if value <= 23:
        return bytes([prefix | value])
    if value <= 0xFF:
        return bytes([prefix | 24, value])
    if value <= 0xFFFF:
        return bytes([prefix | 25]) + value.to_bytes(2, "big")
    if value <= 0xFFFF_FFFF:
        return bytes([prefix | 26]) + value.to_bytes(4, "big")
    return bytes([prefix | 27]) + value.to_bytes(8, "big")


def cbor(value: Any) -> bytes:
    if isinstance(value, bool):
        return b"\xf5" if value else b"\xf4"
    if value is None:
        return b"\xf6"
    if isinstance(value, int):
        if value < 0:
            raise ValueError("negative CBOR integers are outside these vectors")
        return cbor_type_len(0, value)
    if isinstance(value, bytes):
        return cbor_type_len(2, len(value)) + value
    if isinstance(value, str):
        data = value.encode("utf-8")
        return cbor_type_len(3, len(data)) + data
    if isinstance(value, list):
        return cbor_type_len(4, len(value)) + b"".join(cbor(item) for item in value)
    if isinstance(value, dict):
        pairs = sorted(((cbor(key), cbor(val)) for key, val in value.items()), key=lambda pair: pair[0])
        return cbor_type_len(5, len(pairs)) + b"".join(key + val for key, val in pairs)
    raise TypeError(f"unsupported CBOR value {value!r}")


def decode_cbor_head(data: bytes, offset: int) -> tuple[int, int, int]:
    if offset >= len(data):
        raise ValueError("unexpected end of CBOR item")
    initial = data[offset]
    major = initial >> 5
    additional = initial & 0x1F
    offset += 1
    if additional <= 23:
        return major, additional, offset
    if additional == 24:
        end = offset + 1
        if end > len(data):
            raise ValueError("truncated CBOR uint8 argument")
        value = data[offset]
        if value <= 23:
            raise ValueError("non-shortest CBOR integer or length")
        return major, value, end
    if additional == 25:
        end = offset + 2
        if end > len(data):
            raise ValueError("truncated CBOR uint16 argument")
        value = int.from_bytes(data[offset:end], "big")
        if value <= 0xFF:
            raise ValueError("non-shortest CBOR integer or length")
        return major, value, end
    if additional == 26:
        end = offset + 4
        if end > len(data):
            raise ValueError("truncated CBOR uint32 argument")
        value = int.from_bytes(data[offset:end], "big")
        if value <= 0xFFFF:
            raise ValueError("non-shortest CBOR integer or length")
        return major, value, end
    if additional == 27:
        end = offset + 8
        if end > len(data):
            raise ValueError("truncated CBOR uint64 argument")
        value = int.from_bytes(data[offset:end], "big")
        if value <= 0xFFFF_FFFF:
            raise ValueError("non-shortest CBOR integer or length")
        return major, value, end
    raise ValueError("unsupported or indefinite-length CBOR item")


def decode_cbor_item(data: bytes, offset: int = 0, depth: int = 0) -> tuple[Any, int, bytes]:
    if depth > 32:
        raise ValueError("CBOR nesting limit exceeded")
    start = offset
    major, value, offset = decode_cbor_head(data, offset)
    if major == 0:
        return value, offset, data[start:offset]
    if major == 1:
        return -1 - value, offset, data[start:offset]
    if major in (2, 3):
        end = offset + value
        if end > len(data):
            raise ValueError("truncated CBOR string")
        raw = data[offset:end]
        if major == 2:
            return raw, end, data[start:end]
        try:
            return raw.decode("utf-8"), end, data[start:end]
        except UnicodeDecodeError as exc:
            raise ValueError("invalid CBOR UTF-8 text") from exc
    if major == 4:
        items = []
        for _ in range(value):
            item, offset, _encoded = decode_cbor_item(data, offset, depth + 1)
            items.append(item)
        return items, offset, data[start:offset]
    if major == 5:
        result: dict[Any, Any] = {}
        previous_key_encoding: bytes | None = None
        for _ in range(value):
            key, offset, key_encoding = decode_cbor_item(data, offset, depth + 1)
            if previous_key_encoding is not None and key_encoding <= previous_key_encoding:
                raise ValueError("CBOR map keys are not strictly sorted")
            previous_key_encoding = key_encoding
            try:
                duplicate = key in result
            except TypeError as exc:
                raise ValueError("unhashable CBOR map key") from exc
            if duplicate:
                raise ValueError("duplicate CBOR map key")
            result[key], offset, _encoded = decode_cbor_item(data, offset, depth + 1)
        return result, offset, data[start:offset]
    if major == 7:
        if value == 20:
            return False, offset, data[start:offset]
        if value == 21:
            return True, offset, data[start:offset]
        if value == 22:
            return None, offset, data[start:offset]
    raise ValueError("unsupported CBOR value")


def decode_cbor_exact(data: bytes) -> Any:
    value, offset, _encoded = decode_cbor_item(data)
    if offset != len(data):
        raise ValueError("trailing bytes after CBOR item")
    return value


def stream_schema_version(files: list[FileSpec]) -> str:
    return SCHEMA_VERSION_XATTRS if any(spec.xattrs for spec in files) else SCHEMA_VERSION


def global_pax_records(options: dict[str, Any], schema_version: str) -> dict[str, str]:
    return {
        "REMANENCE.caller_object_id": options["caller_object_id"],
        "REMANENCE.chunk_size": str(options["chunk_size"]),
        "REMANENCE.encryption": "none",
        "REMANENCE.format_id": FORMAT_ID,
        "REMANENCE.metadata_preservation": options["metadata_preservation"],
        "REMANENCE.object_id": options["object_id"],
        "REMANENCE.schema_version": schema_version,
        "REMANENCE.write_timestamp": options["write_timestamp"],
    }


def file_pax_records(spec: FileSpec, chunk_size: int, is_manifest: bool) -> dict[str, str]:
    records = {
        "path": spec.path,
        "size": str(spec.size_bytes),
        "REMANENCE.chunk_count": str(chunk_count(spec.size_bytes, chunk_size)),
        "REMANENCE.compression": "none",
        "REMANENCE.file_id": spec.file_id,
    }
    if spec.file_sha256 is not None:
        records["REMANENCE.file_sha256"] = hx(spec.file_sha256)
    if spec.executable is not None:
        records["REMANENCE.executable"] = str(spec.executable).lower()
    if spec.mtime is not None:
        records["mtime"] = spec.mtime
    if is_manifest:
        records["REMANENCE.is_manifest"] = "true"
    if spec.entry_type in {"hardlink", "symlink"}:
        if spec.link_target is None:
            raise AssertionError(f"{spec.entry_type} vector entry missing link target")
        if not is_portable_ustar_linkname(spec.link_target):
            records["linkpath"] = spec.link_target
    return records


def plan_one_file(chunk_size: int, offset: int, spec: FileSpec, is_manifest: bool) -> tuple[FileLayout, dict[str, str], int]:
    base_records = file_pax_records(spec, chunk_size, is_manifest)
    records = base_records if spec.size_bytes == 0 else with_alignment_pad(offset, chunk_size, base_records)
    pax_body_len = len(encode_pax_records(records))
    pax_body_padded = round_up(pax_body_len, TAR_RECORD_SIZE)
    data_offset = offset + TAR_RECORD_SIZE + pax_body_padded + TAR_RECORD_SIZE
    next_offset = data_offset + spec.size_bytes + (TAR_RECORD_SIZE - spec.size_bytes % TAR_RECORD_SIZE) % TAR_RECORD_SIZE
    layout = FileLayout(
        path=spec.path,
        file_id=spec.file_id,
        size_bytes=spec.size_bytes,
        file_sha256=spec.file_sha256,
        entry_type=spec.entry_type,
        link_target=spec.link_target,
        xattrs=dict(spec.xattrs),
        executable=spec.executable,
        first_chunk_lba=None if spec.size_bytes == 0 else data_offset // chunk_size,
        chunk_count=chunk_count(spec.size_bytes, chunk_size),
        pax_header_offset=offset,
        data_offset=data_offset,
        pad_spaces=len(records.get(PAD_KEY, "")),
    )
    return layout, records, next_offset


def manifest_entry(layout: FileLayout) -> dict[str, Any]:
    metadata_preservation_data: dict[str, Any] = {}
    if layout.xattrs:
        metadata_preservation_data["xattrs"] = dict(layout.xattrs)
    entry = {
        "chunk_count": layout.chunk_count,
        "executable": layout.executable,
        "file_id": layout.file_id,
        "first_chunk_lba": layout.first_chunk_lba,
        "metadata_preservation_data": metadata_preservation_data,
        "path": layout.path,
        "size_bytes": layout.size_bytes,
    }
    if layout.file_sha256 is not None:
        entry["file_sha256"] = layout.file_sha256
    if layout.entry_type != "regular":
        entry["entry_type"] = layout.entry_type
    if layout.link_target is not None:
        entry["link_target"] = layout.link_target
    return entry


def encode_manifest(options: dict[str, Any], layouts: list[FileLayout]) -> bytes:
    return cbor(
        {
            "caller_object_id": options["caller_object_id"],
            "chunk_size": options["chunk_size"],
            "external_references": [],
            "file_entries": [manifest_entry(layout) for layout in layouts],
            "object_id": options["object_id"],
            "object_metadata": {},
            "schema_version": 1,
        }
    )


def append_file_entry(out: bytearray, spec: FileSpec, records: dict[str, str], is_manifest: bool) -> None:
    body = encode_pax_records(records)
    pax_name = "PaxHeaders.0/_remanence_manifest" if is_manifest else "PaxHeaders.0/remanence_file"
    out.extend(encode_header(pax_name, len(body), TYPE_PAX_EXTENDED, 0o644))
    out.extend(body)
    out.extend(b"\0" * (round_up(len(body), TAR_RECORD_SIZE) - len(body)))
    if spec.entry_type == "regular":
        mode = 0o755 if spec.executable is True else 0o644
        out.extend(encode_pax_backed_regular_header(spec.path, spec.size_bytes, mode))
        out.extend(spec.data)
        out.extend(b"\0" * ((TAR_RECORD_SIZE - spec.size_bytes % TAR_RECORD_SIZE) % TAR_RECORD_SIZE))
    elif spec.entry_type == "hardlink":
        if spec.link_target is None:
            raise AssertionError("hardlink vector entry missing link target")
        out.extend(
            encode_pax_backed_hardlink_header(
                spec.path,
                spec.link_target,
                "linkpath" in records,
            )
        )
    elif spec.entry_type == "symlink":
        if spec.link_target is None:
            raise AssertionError("symlink vector entry missing link target")
        out.extend(
            encode_pax_backed_symlink_header(
                spec.path,
                spec.link_target,
                "linkpath" in records,
            )
        )
    elif spec.entry_type == "directory":
        out.extend(encode_pax_backed_directory_header(spec.path))
    else:
        raise AssertionError(f"unknown vector entry_type {spec.entry_type!r}")


def build_plaintext(options: dict[str, Any], files: list[FileSpec]) -> tuple[bytes, dict[str, Any]]:
    chunk_size = options["chunk_size"]
    out = bytearray()
    global_body = encode_pax_records(global_pax_records(options, stream_schema_version(files)))
    out.extend(encode_header("GlobalHead.0/PaxHeaders/remanence", len(global_body), TYPE_PAX_GLOBAL, 0o644))
    out.extend(global_body)
    out.extend(b"\0" * (round_up(len(global_body), TAR_RECORD_SIZE) - len(global_body)))

    layouts: list[FileLayout] = []
    for spec in files:
        layout, records, next_offset = plan_one_file(chunk_size, len(out), spec, False)
        append_file_entry(out, spec, records, False)
        assert_eq(len(out), next_offset, f"{spec.path} next offset")
        layouts.append(layout)

    manifest_cbor = encode_manifest(options, layouts)
    manifest_spec = FileSpec(
        path=MANIFEST_PATH,
        file_id=options["manifest_file_id"],
        data=manifest_cbor,
        executable=False,
    )
    manifest_layout, manifest_records, next_offset = plan_one_file(chunk_size, len(out), manifest_spec, True)
    append_file_entry(out, manifest_spec, manifest_records, True)
    assert_eq(len(out), next_offset, "manifest next offset")
    out.extend(b"\0" * (2 * TAR_RECORD_SIZE))
    total_size = round_up(len(out), chunk_size)
    out.extend(b"\0" * (total_size - len(out)))
    return bytes(out), {
        "files": layouts,
        "manifest": manifest_layout,
        "manifest_cbor": manifest_cbor,
        "manifest_sha256": sha256(manifest_cbor),
    }


def hkdf_extract(salt: bytes, ikm: bytes) -> bytes:
    return hmac.new(salt, ikm, hashlib.sha256).digest()


def hkdf_expand(prk: bytes, info: bytes, length: int) -> bytes:
    okm = bytearray()
    previous = b""
    counter = 1
    while len(okm) < length:
        previous = hmac.new(prk, previous + info + bytes([counter]), hashlib.sha256).digest()
        okm.extend(previous)
        counter += 1
    return bytes(okm[:length])


def hkdf(salt: bytes, ikm: bytes, info: bytes, length: int) -> bytes:
    return hkdf_expand(hkdf_extract(salt, ikm), info, length)


def object_id_field(object_id: str) -> bytes:
    data = object_id.encode("utf-8")
    if not data or len(data) > 64 or b"\0" in data:
        raise ValueError("invalid object_id field")
    return data + b"\0" * (64 - len(data))


def derive_salt(
    dek: bytes,
    object_id: str,
    plaintext_digest: bytes,
    metadata: bytes,
) -> tuple[bytes, int]:
    metadata_hash = sha256(metadata)
    oid = object_id_field(object_id)
    for ctr in range(256):
        info = LABEL_SALT + bytes([ctr]) + oid + plaintext_digest + metadata_hash
        salt = hkdf(b"", dek, info, 16)
        if salt != b"\0" * 16:
            return salt, ctr
    raise AssertionError("could not derive nonzero salt")


def stream_nonce(counter: int, final_chunk: bool) -> bytes:
    return b"\0\0\0" + counter.to_bytes(8, "big") + (b"\x01" if final_chunk else b"\x00")


def check_plaintext(vector_id: str, fixture: dict[str, Any], plaintext: bytes, layout: dict[str, Any], expected: dict[str, Any]) -> None:
    assert_eq(len(plaintext), expected["stored_size_bytes"], f"{vector_id} stored_size_bytes")
    assert_eq(len(plaintext) // fixture["inputs"]["chunk_size"], expected["stored_size_blocks"], f"{vector_id} stored_size_blocks")
    assert_eq(hx(sha256(plaintext)), expected["stored_digest"], f"{vector_id} stored_digest")
    assert_eq(hx(sha256(plaintext[: fixture["inputs"]["chunk_size"]])), expected["first_block_sha256"], f"{vector_id} first_block_sha256")
    assert_eq(len(layout["manifest_cbor"]), expected["manifest_cbor_len"], f"{vector_id} manifest_cbor_len")
    assert_eq(hx(layout["manifest_cbor"]), expected["manifest_cbor_hex"], f"{vector_id} manifest_cbor_hex")
    assert_eq(hx(layout["manifest_sha256"]), expected["manifest_sha256"], f"{vector_id} manifest_sha256")


def check_layouts(vector_id: str, layouts: list[FileLayout], expected_layouts: list[dict[str, Any]]) -> None:
    for layout, expected in zip(layouts, expected_layouts, strict=True):
        for field in ["path", "pax_header_offset", "data_offset", "first_chunk_lba", "chunk_count", "pad_spaces"]:
            if field in expected:
                assert_eq(getattr(layout, field), expected[field], f"{vector_id} {layout.path} {field}")




def vector_options(suffix: int, caller_object_id: str, manifest_suffix: str) -> dict[str, Any]:
    return {
        "chunk_size": 4096,
        "object_id": f"00000000-0000-4000-8000-{suffix:012d}",
        "caller_object_id": caller_object_id,
        "write_timestamp": "2026-01-01T00:00:00Z",
        "metadata_preservation": "minimal",
        "manifest_file_id": f"00000000-0000-4000-8000-{manifest_suffix}",
    }


def bytes_mod(length: int, seed: int = 0) -> bytes:
    return bytes((seed + index) % 256 for index in range(length))


def positive_plaintext_vector_definitions() -> list[tuple[str, str, dict[str, Any], list[FileSpec]]]:
    long_path = "long/" + "a" * 102 + ".bin"
    inline_100 = "inline-" + "b" * 93
    long_target = "targets/" + "x" * 120
    long_hardlink_target = "hardlink-targets/" + "p" * 110 + ".bin"
    return [
        (
            "rao-tv-empty.json",
            "RAO-TV-EMPTY",
            vector_options(101, "rao-tv-empty", "000000000101"),
            [],
        ),
        (
            "rao-tv-empty-file.json",
            "RAO-TV-EMPTY-FILE",
            vector_options(102, "rao-tv-empty-file", "000000000102"),
            [
                FileSpec(
                    "empty.bin",
                    "00000000-0000-4000-8000-000000000120",
                    b"",
                )
            ],
        ),
        (
            "rao-tv-one-byte.json",
            "RAO-TV-ONE-BYTE",
            vector_options(103, "rao-tv-one-byte", "000000000103"),
            [
                FileSpec(
                    "one.bin",
                    "00000000-0000-4000-8000-000000000130",
                    b"\x7f",
                )
            ],
        ),
        (
            "rao-tv-boundary.json",
            "RAO-TV-BOUNDARY",
            vector_options(104, "rao-tv-boundary", "000000000104"),
            [
                FileSpec("boundary/c-minus-1.bin", "00000000-0000-4000-8000-000000000141", bytes_mod(4095, 1)),
                FileSpec("boundary/c.bin", "00000000-0000-4000-8000-000000000142", bytes_mod(4096, 2)),
                FileSpec("boundary/c-plus-1.bin", "00000000-0000-4000-8000-000000000143", bytes_mod(4097, 3)),
                FileSpec("boundary/multi.bin", "00000000-0000-4000-8000-000000000144", bytes_mod(9000, 4)),
            ],
        ),
        (
            "rao-tv-paths.json",
            "RAO-TV-PATHS",
            vector_options(105, "rao-tv-paths", "000000000105"),
            [
                FileSpec("unicode/vidéo.txt", "00000000-0000-4000-8000-000000000151", b"utf8 path\n"),
                FileSpec(long_path, "00000000-0000-4000-8000-000000000152", b"long path\n"),
                FileSpec(inline_100, "00000000-0000-4000-8000-000000000153", b"inline path\n"),
            ],
        ),
        (
            "rao-tv-metadata.json",
            "RAO-TV-METADATA",
            vector_options(106, "rao-tv-metadata", "000000000106"),
            [
                FileSpec(
                    "meta/mtime.txt",
                    "00000000-0000-4000-8000-000000000161",
                    b"mtime\n",
                    mtime="1700000000.123456789",
                ),
                FileSpec(
                    "meta/exec.sh",
                    "00000000-0000-4000-8000-000000000162",
                    b"#!/bin/sh\nexit 0\n",
                    executable=True,
                ),
                FileSpec(
                    "meta/null-exec.txt",
                    "00000000-0000-4000-8000-000000000163",
                    b"null executable\n",
                ),
            ],
        ),
        (
            "rao-tv-order.json",
            "RAO-TV-ORDER",
            vector_options(107, "rao-tv-order", "000000000107"),
            [
                FileSpec("z-last.txt", "00000000-0000-4000-8000-000000000171", b"first in caller order\n"),
                FileSpec("a-first.txt", "00000000-0000-4000-8000-000000000172", b"second in caller order\n"),
                FileSpec("m-middle.txt", "00000000-0000-4000-8000-000000000173", b"third in caller order\n"),
            ],
        ),
        (
            "rao-tv-manifest.json",
            "RAO-TV-MANIFEST",
            vector_options(108, "rao-tv-manifest", "000000000108"),
            [
                FileSpec("manifest/alpha.bin", "00000000-0000-4000-8000-000000000181", bytes_mod(17, 9)),
                FileSpec("manifest/beta.bin", "00000000-0000-4000-8000-000000000182", bytes_mod(513, 10)),
            ],
        ),
        (
            "rao-tv-nonregular.json",
            "RAO-TV-NONREGULAR",
            vector_options(109, "rao-tv-nonregular", "000000000109"),
            [
                FileSpec("empty/", "00000000-0000-4000-8000-000000000191", b"", entry_type="directory"),
                FileSpec(
                    "links/latest",
                    "00000000-0000-4000-8000-000000000192",
                    b"",
                    entry_type="symlink",
                    link_target="target.txt",
                ),
                FileSpec(
                    "links/long-target",
                    "00000000-0000-4000-8000-000000000193",
                    b"",
                    entry_type="symlink",
                    link_target=long_target,
                ),
                FileSpec(
                    "links/dangling",
                    "00000000-0000-4000-8000-000000000194",
                    b"",
                    entry_type="symlink",
                    link_target="missing.txt",
                ),
                FileSpec("target.txt", "00000000-0000-4000-8000-000000000195", b"target\n"),
            ],
        ),
        (
            "rao-tv-hardlinks.json",
            "RAO-TV-HARDLINKS",
            vector_options(110, "rao-tv-hardlinks", "000000000110"),
            [
                FileSpec(
                    "primary.txt",
                    "00000000-0000-4000-8000-000000000201",
                    b"shared hardlink payload\n",
                ),
                FileSpec(
                    "links/copy.txt",
                    "00000000-0000-4000-8000-000000000202",
                    b"",
                    entry_type="hardlink",
                    link_target="primary.txt",
                ),
                FileSpec(
                    long_hardlink_target,
                    "00000000-0000-4000-8000-000000000203",
                    b"long target hardlink payload\n",
                ),
                FileSpec(
                    "links/long-target-copy.bin",
                    "00000000-0000-4000-8000-000000000204",
                    b"",
                    entry_type="hardlink",
                    link_target=long_hardlink_target,
                ),
            ],
        ),
        (
            "rao-tv-xattrs.json",
            "RAO-TV-XATTRS",
            vector_options(111, "rao-tv-xattrs", "000000000111"),
            [
                FileSpec(
                    "tagged.txt",
                    "00000000-0000-4000-8000-000000000211",
                    b"xattr payload\n",
                    xattrs={
                        "user.comment": b"blue",
                        "user.remanence.color": bytes([0x01, 0x02, 0xff]),
                    },
                ),
                FileSpec(
                    "plain.txt",
                    "00000000-0000-4000-8000-000000000212",
                    b"plain payload\n",
                ),
            ],
        ),
    ]


def layout_json(layout: FileLayout) -> dict[str, Any]:
    return {
        "entry_type": layout.entry_type,
        "path": layout.path,
        "pax_header_offset": layout.pax_header_offset,
        "data_offset": layout.data_offset,
        "first_chunk_lba": layout.first_chunk_lba,
        "chunk_count": layout.chunk_count,
        "pad_spaces": layout.pad_spaces,
    }


def input_entry_json(spec: FileSpec) -> dict[str, Any]:
    entry = {
        "entry_type": spec.entry_type,
        "path": spec.path,
        "file_id": spec.file_id,
        "size_bytes": spec.size_bytes,
        "link_target": spec.link_target,
        "mtime": spec.mtime,
        "executable": spec.executable,
    }
    if spec.xattrs:
        entry["xattrs"] = {name: hx(value) for name, value in spec.xattrs.items()}
    return entry


def expected_xattrs(files: list[FileSpec]) -> list[dict[str, Any]]:
    return [
        {
            "path": spec.path,
            "xattrs": {name: hx(value) for name, value in spec.xattrs.items()},
        }
        for spec in files
        if spec.xattrs
    ]


def fixture_json(vector_id: str, options: dict[str, Any], files: list[FileSpec]) -> dict[str, Any]:
    plaintext, layout = build_plaintext(options, files)
    return {
        "vector_id": vector_id,
        "spec_section": "13.1",
        "status": "pinned-at-generation",
        "independent_rederivation": "verified by tools/verify_rao_vectors_independent.py",
        "inputs": {
            **options,
            "entries": [input_entry_json(spec) for spec in files],
        },
        "expected": {
            "schema_version": stream_schema_version(files),
            "stored_size_bytes": len(plaintext),
            "stored_size_blocks": len(plaintext) // options["chunk_size"],
            "stored_digest": hx(sha256(plaintext)),
            "first_block_sha256": hx(sha256(plaintext[: options["chunk_size"]])),
            "manifest_cbor_len": len(layout["manifest_cbor"]),
            "manifest_cbor_hex": hx(layout["manifest_cbor"]),
            "manifest_sha256": hx(layout["manifest_sha256"]),
            "file_payloads": [
                {
                    "path": spec.path,
                    "size_bytes": spec.size_bytes,
                    "sha256": hx(spec.file_sha256 or b""),
                }
                for spec in files
                if spec.entry_type == "regular"
            ],
            "symlinks": [
                {"path": spec.path, "target": spec.link_target}
                for spec in files
                if spec.entry_type == "symlink"
            ],
            "hardlinks": [
                {"path": spec.path, "target": spec.link_target}
                for spec in files
                if spec.entry_type == "hardlink"
            ],
            "directories": [spec.path for spec in files if spec.entry_type == "directory"],
            "xattrs": expected_xattrs(files),
            "file_layouts": [layout_json(file_layout) for file_layout in layout["files"]],
            "manifest_layout": layout_json(layout["manifest"]),
        },
    }


def expected_regular_files(files: list[FileSpec], manifest_cbor: bytes) -> dict[str, bytes]:
    expected = {spec.path: spec.data for spec in files if spec.entry_type == "regular"}
    for spec in files:
        if spec.entry_type != "hardlink":
            continue
        if spec.link_target is None:
            raise AssertionError(f"hardlink {spec.path!r} is missing a target")
        if spec.link_target not in expected:
            raise AssertionError(
                f"hardlink {spec.path!r} target {spec.link_target!r} is not a preceding regular file"
            )
        expected[spec.path] = expected[spec.link_target]
    expected[MANIFEST_PATH] = manifest_cbor
    return expected


def expected_symlinks(files: list[FileSpec]) -> dict[str, str]:
    return {
        spec.path: spec.link_target
        for spec in files
        if spec.entry_type == "symlink" and spec.link_target is not None
    }


def expected_hardlinks(files: list[FileSpec]) -> dict[str, str]:
    return {
        spec.path: spec.link_target
        for spec in files
        if spec.entry_type == "hardlink" and spec.link_target is not None
    }


def expected_directories(files: list[FileSpec]) -> set[str]:
    return {spec.path.rstrip("/") for spec in files if spec.entry_type == "directory"}


def assert_extracted_tree(
    vector_id: str,
    reader: str,
    root: pathlib.Path,
    expected: dict[str, bytes],
    expected_links: dict[str, str],
    expected_hardlink_targets: dict[str, str],
    expected_dirs: set[str],
) -> None:
    actual_paths: set[str] = set()
    actual_links: dict[str, str] = {}
    actual_dirs: set[str] = set()
    for path in root.rglob("*"):
        rel = path.relative_to(root).as_posix()
        if path.is_symlink():
            actual_links[rel] = path.readlink().as_posix()
            continue
        if path.is_dir():
            actual_dirs.add(rel)
            continue
        if path.is_file():
            actual_paths.add(rel)
            if rel not in expected:
                raise AssertionError(f"{vector_id} {reader}: unexpected extracted file {rel!r}")
            data = path.read_bytes()
            assert_eq(data, expected[rel], f"{vector_id} {reader} {rel}")
            continue
        raise AssertionError(f"{vector_id} {reader}: unsupported filesystem entry {rel!r}")

    missing = sorted(set(expected) - actual_paths)
    if missing:
        raise AssertionError(f"{vector_id} {reader}: missing extracted files {missing!r}")
    extra_links = sorted(set(actual_links) - set(expected_links))
    if extra_links:
        raise AssertionError(f"{vector_id} {reader}: unexpected symlinks {extra_links!r}")
    for rel, target in expected_links.items():
        if rel not in actual_links:
            raise AssertionError(f"{vector_id} {reader}: missing symlink {rel!r}")
        assert_eq(actual_links[rel], target, f"{vector_id} {reader} symlink {rel}")
    for rel, target in expected_hardlink_targets.items():
        if rel not in actual_paths:
            raise AssertionError(f"{vector_id} {reader}: missing hardlink file {rel!r}")
        if target not in actual_paths:
            raise AssertionError(f"{vector_id} {reader}: missing hardlink target {target!r}")
        if not (root / rel).samefile(root / target):
            raise AssertionError(
                f"{vector_id} {reader}: {rel!r} is not a hardlink to {target!r}"
            )
    missing_dirs = sorted(expected_dirs - actual_dirs)
    if missing_dirs:
        raise AssertionError(f"{vector_id} {reader}: missing directories {missing_dirs!r}")


def assert_python_tarfile(vector: PlaintextVector, archive: pathlib.Path) -> None:
    actual: dict[str, bytes] = {}
    actual_links: dict[str, str] = {}
    actual_hardlinks: dict[str, str] = {}
    actual_dirs: set[str] = set()
    with tarfile.open(archive, "r:") as handle:
        for member in handle.getmembers():
            if member.isfile():
                extracted = handle.extractfile(member)
                if extracted is None:
                    raise AssertionError(
                        f"{vector.vector_id} python tarfile: {member.name!r} has no data handle"
                    )
                actual[member.name] = extracted.read()
            elif member.islnk():
                actual_hardlinks[member.name] = member.linkname
                extracted = handle.extractfile(member)
                if extracted is None:
                    raise AssertionError(
                        f"{vector.vector_id} python tarfile: hardlink {member.name!r} has no data handle"
                    )
                actual[member.name] = extracted.read()
            elif member.issym():
                actual_links[member.name] = member.linkname
            elif member.isdir():
                actual_dirs.add(member.name.rstrip("/"))
                continue
            else:
                raise AssertionError(
                    f"{vector.vector_id} python tarfile: unsupported extracted member "
                    f"{member.name!r} type {member.type!r}"
                )

    if set(actual) != set(vector.expected_files):
        raise AssertionError(
            f"{vector.vector_id} python tarfile: got files {sorted(actual)!r}, "
            f"expected {sorted(vector.expected_files)!r}"
        )
    for path, expected in vector.expected_files.items():
        assert_eq(actual[path], expected, f"{vector.vector_id} python tarfile {path}")
    assert_eq(actual_links, vector.expected_symlinks, f"{vector.vector_id} python tarfile symlinks")
    assert_eq(
        actual_hardlinks,
        vector.expected_hardlinks,
        f"{vector.vector_id} python tarfile hardlinks",
    )
    missing_dirs = sorted(vector.expected_directories - actual_dirs)
    if missing_dirs:
        raise AssertionError(f"{vector.vector_id} python tarfile: missing directories {missing_dirs!r}")


def command_output(command: list[str]) -> str:
    result = subprocess.run(
        command,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    return result.stdout.strip()


def require_gnu_tar() -> str:
    path = shutil.which("tar")
    if path is None:
        raise AssertionError("GNU tar not found on PATH")
    version = command_output([path, "--version"]).splitlines()[0]
    if "GNU tar" not in version:
        raise AssertionError(f"{path} is not GNU tar: {version}")
    return path


def require_bsdtar(allow_missing: bool) -> str | None:
    path = shutil.which("bsdtar")
    if path is None:
        if allow_missing:
            return None
        raise AssertionError("bsdtar not found on PATH")
    version = command_output([path, "--version"]).splitlines()[0]
    if "bsdtar" not in version.lower():
        raise AssertionError(f"{path} is not bsdtar: {version}")
    return path


def run_tar_reader(
    vector: PlaintextVector,
    reader: str,
    binary: str,
    archive: pathlib.Path,
    tmp: pathlib.Path,
) -> None:
    out_dir = tmp / reader / vector.vector_id
    out_dir.mkdir(parents=True)
    result = subprocess.run(
        [binary, "-b", str(vector.chunk_size // TAR_RECORD_SIZE), "-xf", str(archive)],
        cwd=out_dir,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise AssertionError(
            f"{vector.vector_id} {reader} failed with exit {result.returncode}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    assert_extracted_tree(
        vector.vector_id,
        reader,
        out_dir,
        vector.expected_files,
        vector.expected_symlinks,
        vector.expected_hardlinks,
        vector.expected_directories,
    )


def check_plaintext_interop(vectors: list[PlaintextVector], allow_missing_bsdtar: bool) -> None:
    gnu_tar = require_gnu_tar()
    bsdtar = require_bsdtar(allow_missing_bsdtar)
    with tempfile.TemporaryDirectory(prefix="rao-plaintext-interop-") as tmp_name:
        tmp = pathlib.Path(tmp_name)
        for vector in vectors:
            archive = tmp / f"{vector.vector_id}.tar"
            archive.write_bytes(vector.plaintext)
            run_tar_reader(vector, "gnu-tar", gnu_tar, archive, tmp)
            if bsdtar is not None:
                run_tar_reader(vector, "bsdtar", bsdtar, archive, tmp)
            assert_python_tarfile(vector, archive)

    if bsdtar is None:
        readers = "GNU tar, Python tarfile (bsdtar skipped: not found)"
    else:
        readers = "GNU tar, bsdtar, Python tarfile"
    vector_ids = ", ".join(vector.vector_id for vector in vectors)
    print(f"verified RAO plaintext interop for {vector_ids} with {readers}")


def find_manifest_entry(manifest: dict[Any, Any], path: str) -> dict[Any, Any]:
    entries = manifest.get("file_entries")
    if not isinstance(entries, list):
        raise AssertionError("manifest file_entries is not an array")
    matches = [entry for entry in entries if isinstance(entry, dict) and entry.get("path") == path]
    if len(matches) != 1:
        raise AssertionError(f"manifest has {len(matches)} entries for {path!r}")
    return matches[0]


def parse_encrypted_header(stored: bytes) -> EncryptedHeader:
    if len(stored) < RAO_HEADER_LEN:
        raise AssertionError("encrypted object is shorter than the RAO1 header")
    header = stored[:RAO_HEADER_LEN]
    assert_eq(header[:4], b"RAO1", "encrypted header magic")
    assert_eq(int.from_bytes(header[4:6], "big"), RAO_HEADER_LEN, "encrypted header_len")
    assert_eq(header[6], 2, "encrypted format_version")
    assert_eq(header[7], 1, "encrypted suite_id")
    chunk_size = int.from_bytes(header[8:12], "big")
    if chunk_size <= 0 or chunk_size % TAR_RECORD_SIZE != 0:
        raise AssertionError(f"encrypted chunk_size is invalid: {chunk_size}")
    assert_eq(header[12:16], b"\0" * 4, "encrypted flags")
    assert_eq(header[16:32], b"\0" * 16, "encrypted reserved key region")
    salt = header[32:48]
    if salt == b"\0" * 16:
        raise AssertionError("encrypted hkdf_salt is all zero")
    metadata_frame_len = int.from_bytes(header[48:56], "big")
    if not 17 <= metadata_frame_len <= 16 * 1024 * 1024:
        raise AssertionError(f"metadata frame length is invalid: {metadata_frame_len}")
    wrap_suite = header[56]
    assert_eq(wrap_suite, 1, "encrypted wrap_suite")
    assert_eq(header[57:60], b"\0" * 3, "encrypted reserved")
    key_frame_len = int.from_bytes(header[60:64], "big")
    if not RAO_KEY_FRAME_MIN_LEN <= key_frame_len <= RAO_KEY_FRAME_MAX_LEN:
        raise AssertionError(f"encrypted key_frame_len is invalid: {key_frame_len}")
    object_id_field_bytes = header[64:128]
    if object_id_field_bytes == b"\0" * 64:
        raise AssertionError("encrypted object_id field is empty")
    first_nul = object_id_field_bytes.find(b"\0")
    if first_nul == -1:
        object_id_bytes = object_id_field_bytes
    else:
        object_id_bytes = object_id_field_bytes[:first_nul]
        if object_id_field_bytes[first_nul:] != b"\0" * (64 - first_nul):
            raise AssertionError("encrypted object_id field has an interior NUL")
    try:
        object_id = object_id_bytes.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise AssertionError("encrypted object_id field is not UTF-8") from exc
    return EncryptedHeader(
        header,
        chunk_size,
        salt,
        metadata_frame_len,
        object_id,
        wrap_suite,
        key_frame_len,
    )


def parse_key_frame(vector_id: str, encoded: bytes) -> list[RecipientSlot]:
    if not RAO_KEY_FRAME_MIN_LEN <= len(encoded) <= RAO_KEY_FRAME_MAX_LEN:
        raise AssertionError(f"{vector_id}: key frame length is outside bounds")
    assert_eq(encoded[:4], b"RAOK", f"{vector_id} key frame magic")
    slot_count = encoded[4]
    if not 1 <= slot_count <= RAO_KEY_FRAME_MAX_SLOTS:
        raise AssertionError(f"{vector_id}: invalid key-frame slot count {slot_count}")
    cursor = 5
    slots: list[RecipientSlot] = []
    for _ in range(slot_count):
        if cursor + 18 > len(encoded):
            raise AssertionError(f"{vector_id}: truncated key-frame slot prefix")
        slot_index = encoded[cursor]
        recipient_epoch_id = encoded[cursor + 1 : cursor + 17]
        label_len = encoded[cursor + 17]
        cursor += 18
        end = cursor + label_len + 32 + 48
        if label_len > 32 or end > len(encoded):
            raise AssertionError(f"{vector_id}: truncated or oversized key-frame slot")
        label_bytes = encoded[cursor : cursor + label_len]
        if not all(0x20 <= byte <= 0x7E for byte in label_bytes):
            raise AssertionError(f"{vector_id}: invalid key-frame label")
        epoch_label = label_bytes.decode("ascii")
        cursor += label_len
        enc = encoded[cursor : cursor + 32]
        cursor += 32
        ciphertext = encoded[cursor : cursor + 48]
        cursor += 48
        if slots and slot_index <= slots[-1].slot_index:
            raise AssertionError(f"{vector_id}: key-frame slots are not strictly ordered")
        if any(slot.recipient_epoch_id == recipient_epoch_id for slot in slots):
            raise AssertionError(f"{vector_id}: duplicate recipient_epoch_id")
        slots.append(
            RecipientSlot(
                slot_index,
                recipient_epoch_id,
                epoch_label,
                enc,
                ciphertext,
            )
        )
    if cursor != len(encoded):
        raise AssertionError(f"{vector_id}: trailing key-frame bytes")
    return slots


def hpke_labeled_extract(
    suite_id: bytes,
    salt: bytes,
    label: bytes,
    ikm: bytes,
) -> bytes:
    return hkdf_extract(salt, HPKE_VERSION_LABEL + suite_id + label + ikm)


def hpke_labeled_expand(
    suite_id: bytes,
    prk: bytes,
    label: bytes,
    info: bytes,
    length: int,
) -> bytes:
    labeled_info = (
        length.to_bytes(2, "big")
        + HPKE_VERSION_LABEL
        + suite_id
        + label
        + info
    )
    return hkdf_expand(prk, labeled_info, length)


def hpke_unwrap_dek(
    vector_id: str,
    slot: RecipientSlot,
    object_id: str,
    recipient: dict[str, Any],
) -> tuple[bytes, dict[str, bytes]]:
    private_bytes = bytes.fromhex(recipient["private_key"])
    expected_public = bytes.fromhex(recipient["public_key"])
    private_key = X25519PrivateKey.from_private_bytes(private_bytes)
    actual_public = private_key.public_key().public_bytes(
        serialization.Encoding.Raw,
        serialization.PublicFormat.Raw,
    )
    assert_eq(actual_public, expected_public, f"{vector_id} recipient public key")
    assert_eq(
        slot.recipient_epoch_id,
        bytes.fromhex(recipient["recipient_epoch_id"]),
        f"{vector_id} recipient epoch id",
    )
    assert_eq(slot.slot_index, recipient["slot_index"], f"{vector_id} recipient slot")
    assert_eq(slot.epoch_label, recipient["epoch_label"], f"{vector_id} recipient label")

    kem_suite_id = b"KEM" + HPKE_KEM_ID.to_bytes(2, "big")
    dh = private_key.exchange(X25519PublicKey.from_public_bytes(slot.enc))
    kem_context = slot.enc + actual_public
    eae_prk = hpke_labeled_extract(kem_suite_id, b"", b"eae_prk", dh)
    shared_secret = hpke_labeled_expand(
        kem_suite_id,
        eae_prk,
        b"shared_secret",
        kem_context,
        32,
    )

    hpke_suite_id = (
        b"HPKE"
        + HPKE_KEM_ID.to_bytes(2, "big")
        + HPKE_KDF_ID.to_bytes(2, "big")
        + HPKE_AEAD_ID.to_bytes(2, "big")
    )
    info = (
        WRAP_INFO_PREFIX
        + object_id_field(object_id)
        + slot.recipient_epoch_id
        + bytes([slot.slot_index, 2, 1])
    )
    psk_id_hash = hpke_labeled_extract(hpke_suite_id, b"", b"psk_id_hash", b"")
    info_hash = hpke_labeled_extract(hpke_suite_id, b"", b"info_hash", info)
    key_schedule_context = b"\0" + psk_id_hash + info_hash
    secret = hpke_labeled_extract(hpke_suite_id, shared_secret, b"secret", b"")
    key = hpke_labeled_expand(
        hpke_suite_id,
        secret,
        b"key",
        key_schedule_context,
        32,
    )
    base_nonce = hpke_labeled_expand(
        hpke_suite_id,
        secret,
        b"base_nonce",
        key_schedule_context,
        12,
    )
    dek = ChaCha20Poly1305(key).decrypt(base_nonce, slot.ciphertext, b"")
    assert_eq(len(dek), 32, f"{vector_id} unwrapped DEK length")
    return dek, {
        "shared_secret": shared_secret,
        "key": key,
        "base_nonce": base_nonce,
    }


def validate_encrypted_metadata(vector_id: str, metadata: Any, chunk_size: int) -> tuple[int, bytes]:
    if not isinstance(metadata, dict):
        raise AssertionError(f"{vector_id}: metadata is not a CBOR map")
    assert_eq(set(metadata), {0, 1, 2, 3}, f"{vector_id} metadata keys")
    assert_eq(metadata[0], 1, f"{vector_id} metadata_version")
    plaintext_size = metadata[1]
    if not isinstance(plaintext_size, int):
        raise AssertionError(f"{vector_id}: plaintext_size is not an integer")
    if plaintext_size <= 0 or plaintext_size % chunk_size != 0:
        raise AssertionError(f"{vector_id}: invalid plaintext_size {plaintext_size}")
    assert_eq(metadata[2], "sha256", f"{vector_id} plaintext_digest_alg")
    plaintext_digest = metadata[3]
    if not isinstance(plaintext_digest, bytes) or len(plaintext_digest) != 32:
        raise AssertionError(f"{vector_id}: plaintext_digest is not 32 bytes")
    return plaintext_size, plaintext_digest


def open_encrypted_with_generic_crypto(
    vector_id: str,
    stored: bytes,
    recipient: dict[str, Any],
    expected: dict[str, Any],
    expected_dek: bytes,
) -> tuple[bytes, EncryptedHeader, dict[str, Any]]:
    header = parse_encrypted_header(stored)
    key_frame_start = RAO_HEADER_LEN
    key_frame_end = key_frame_start + header.key_frame_len
    if key_frame_end > len(stored):
        raise AssertionError(f"{vector_id}: encrypted object ends inside key frame")
    key_frame = stored[key_frame_start:key_frame_end]
    slots = parse_key_frame(vector_id, key_frame)
    recipient_epoch_id = bytes.fromhex(recipient["recipient_epoch_id"])
    matching_slots = [
        slot for slot in slots if slot.recipient_epoch_id == recipient_epoch_id
    ]
    if len(matching_slots) != 1:
        raise AssertionError(
            f"{vector_id}: found {len(matching_slots)} slots for recipient epoch"
        )
    dek, hpke_trace = hpke_unwrap_dek(
        vector_id,
        matching_slots[0],
        header.object_id,
        recipient,
    )
    assert_eq(dek, expected_dek, f"{vector_id} unwrapped DEK")
    header_hash = sha256(header.bytes + key_frame)
    object_secret = hkdf(
        header.salt,
        dek,
        LABEL_OBJECT + header_hash,
        32,
    )
    metadata_key = hkdf(b"", object_secret, LABEL_METADATA, 32)
    payload_key = hkdf(b"", object_secret, LABEL_PAYLOAD, 32)

    metadata_start = key_frame_end
    metadata_end = metadata_start + header.metadata_frame_len
    if metadata_end > len(stored):
        raise AssertionError(f"{vector_id}: encrypted object ends inside metadata frame")
    metadata_frame = stored[metadata_start:metadata_end]
    metadata_plain = ChaCha20Poly1305(metadata_key).decrypt(b"\0" * 12, metadata_frame, b"")
    metadata = decode_cbor_exact(metadata_plain)
    plaintext_size, plaintext_digest = validate_encrypted_metadata(
        vector_id,
        metadata,
        header.chunk_size,
    )
    expected_salt, salt_derivation_counter = derive_salt(
        dek,
        header.object_id,
        plaintext_digest,
        metadata_plain,
    )
    assert_eq(header.salt, expected_salt, f"{vector_id} derived salt")

    chunk_total = plaintext_size // header.chunk_size
    payload_frame_len = plaintext_size + 16 * chunk_total
    payload_start = metadata_end
    footer_offset = payload_start + payload_frame_len
    stored_size = round_up(footer_offset + len(RAO_FOOTER), header.chunk_size)
    assert_eq(len(stored), stored_size, f"{vector_id} stored_size")
    assert_eq(stored[footer_offset : footer_offset + len(RAO_FOOTER)], RAO_FOOTER, f"{vector_id} footer")
    fill = stored[footer_offset + len(RAO_FOOTER) :]
    if fill != b"\0" * len(fill):
        raise AssertionError(f"{vector_id}: encrypted final fill is not all zero")

    plaintext = bytearray()
    for index in range(chunk_total):
        chunk_start = payload_start + index * (header.chunk_size + 16)
        chunk_end = chunk_start + header.chunk_size + 16
        frame = stored[chunk_start:chunk_end]
        if len(frame) != header.chunk_size + 16:
            raise AssertionError(f"{vector_id}: truncated encrypted chunk {index}")
        plaintext.extend(
            ChaCha20Poly1305(payload_key).decrypt(
                stream_nonce(index, index + 1 == chunk_total),
                frame,
                b"",
            )
        )

    recovered = bytes(plaintext)
    assert_eq(len(recovered), plaintext_size, f"{vector_id} recovered plaintext_size")
    assert_eq(sha256(recovered), plaintext_digest, f"{vector_id} recovered plaintext_digest")
    actual = {
        "plaintext_size": plaintext_size,
        "chunk_count": chunk_total,
        "metadata_plaintext_len": len(metadata_plain),
        "metadata_frame_len": len(metadata_frame),
        "metadata_plaintext_hex": hx(metadata_plain),
        "metadata_hash": hx(sha256(metadata_plain)),
        "key_frame_len": len(key_frame),
        "key_frame_hex": hx(key_frame),
        "header_hex": hx(header.bytes),
        "header_hash": hx(header_hash),
        "hkdf_salt": hx(header.salt),
        "salt_derivation_counter": salt_derivation_counter,
        "object_secret": hx(object_secret),
        "metadata_key": hx(metadata_key),
        "payload_key": hx(payload_key),
        "metadata_frame_hex": hx(metadata_frame),
        "payload_frame_start": payload_start,
        "payload_frame_end_inclusive": footer_offset - 1,
        "payload_frame_sha256": hx(sha256(stored[payload_start:footer_offset])),
        "footer_offset": footer_offset,
        "stored_size_bytes": len(stored),
        "stored_size_blocks": len(stored) // header.chunk_size,
        "stored_digest": hx(sha256(stored)),
        "plaintext_digest": hx(plaintext_digest),
    }
    unknown_expected = set(expected) - set(actual) - {"slots"}
    if unknown_expected:
        raise AssertionError(
            f"{vector_id}: unverified expected fields {sorted(unknown_expected)!r}"
        )
    for key, expected_value in expected.items():
        if key in actual:
            assert_eq(actual[key], expected_value, f"{vector_id} {key}")
    expected_slots = expected.get("slots", [])
    if expected_slots:
        assert_eq(len(slots), len(expected_slots), f"{vector_id} expected slots")
        for slot, expected_slot in zip(slots, expected_slots, strict=True):
            assert_eq(slot.slot_index, expected_slot["slot_index"], f"{vector_id} slot index")
            assert_eq(
                hx(slot.recipient_epoch_id),
                expected_slot["recipient_epoch_id"],
                f"{vector_id} slot epoch id",
            )
            assert_eq(slot.epoch_label, expected_slot["epoch_label"], f"{vector_id} slot label")
            assert_eq(hx(slot.enc), expected_slot["enc"], f"{vector_id} slot enc")
            assert_eq(
                hx(slot.ciphertext),
                expected_slot["wrapped_dek_ciphertext"],
                f"{vector_id} slot ciphertext",
            )
    trace_expected = next(
        (
            expected_slot
            for expected_slot in expected_slots
            if expected_slot["slot_index"] == recipient["slot_index"]
        ),
        {},
    )
    for key, value in hpke_trace.items():
        expected_key = f"hpke_{key}"
        if expected_key in trace_expected:
            assert_eq(hx(value), trace_expected[expected_key], f"{vector_id} {expected_key}")
    return recovered, header, actual


def verify_recovered_file_digests(
    vector_id: str,
    recovered: bytes,
    expected_files: dict[str, bytes],
    object_id: str,
    chunk_size: int,
) -> None:
    with tarfile.open(fileobj=io.BytesIO(recovered), mode="r:") as archive:
        manifest_file = archive.extractfile(MANIFEST_PATH)
        if manifest_file is None:
            raise AssertionError(f"{vector_id}: recovered manifest is absent")
        manifest_bytes = manifest_file.read()
        assert_eq(
            manifest_bytes,
            expected_files[MANIFEST_PATH],
            f"{vector_id} recovered manifest bytes",
        )
        manifest = decode_cbor_exact(manifest_bytes)
        if not isinstance(manifest, dict):
            raise AssertionError(f"{vector_id}: recovered manifest is not a map")
        assert_eq(manifest.get("object_id"), object_id, f"{vector_id} manifest object_id")
        assert_eq(manifest.get("chunk_size"), chunk_size, f"{vector_id} manifest chunk_size")
        for path, expected_payload in expected_files.items():
            if path == MANIFEST_PATH:
                continue
            payload_file = archive.extractfile(path)
            if payload_file is None:
                raise AssertionError(f"{vector_id}: recovered payload {path!r} is absent")
            payload = payload_file.read()
            assert_eq(payload, expected_payload, f"{vector_id} recovered {path}")
            entry = find_manifest_entry(manifest, path)
            assert_eq(entry.get("size_bytes"), len(payload), f"{vector_id} {path} size")
            assert_eq(
                entry.get("file_sha256"),
                sha256(payload),
                f"{vector_id} {path} digest",
            )


def check_recovery_vector(
    plaintext_vector: PlaintextVector,
    encrypted_vector_id: str,
    encrypted_stored: bytes,
    fixture: dict[str, Any],
    object_id: str,
) -> None:
    inputs = fixture["inputs"]
    expected = fixture["expected"]
    if "encrypted" in expected:
        expected = expected["encrypted"]
    recovered_plaintext = b""
    encrypted_header: EncryptedHeader | None = None
    for recipient in inputs["recipients"]:
        recovered_plaintext, current_header, _actual = open_encrypted_with_generic_crypto(
            encrypted_vector_id,
            encrypted_stored,
            recipient,
            expected,
            bytes.fromhex(inputs["deterministic_dek"]),
        )
        assert_eq(
            recovered_plaintext,
            plaintext_vector.plaintext,
            f"{encrypted_vector_id} recovered plaintext",
        )
        if encrypted_header is None:
            encrypted_header = current_header
        else:
            assert_eq(current_header, encrypted_header, f"{encrypted_vector_id} header")
    if encrypted_header is None:
        raise AssertionError(f"{encrypted_vector_id}: fixture has no recipient test material")
    assert_eq(
        encrypted_header.chunk_size,
        plaintext_vector.chunk_size,
        f"{encrypted_vector_id} header chunk_size",
    )
    assert_eq(encrypted_header.object_id, object_id, f"{encrypted_vector_id} header object_id")
    verify_recovered_file_digests(
        encrypted_vector_id,
        recovered_plaintext,
        plaintext_vector.expected_files,
        object_id,
        plaintext_vector.chunk_size,
    )


def check_positive_plaintext_fixture(
    fixture_directory: pathlib.Path,
    filename: str,
    vector_id: str,
    options: dict[str, Any],
    files: list[FileSpec],
) -> PlaintextVector:
    fixture = load(fixture_directory, filename)
    expected = fixture["expected"]
    assert_eq(fixture["vector_id"], vector_id, f"{vector_id} fixture id")
    plaintext, layout = build_plaintext(options, files)
    check_plaintext(vector_id, fixture, plaintext, layout, expected)
    check_layouts(vector_id, layout["files"], expected["file_layouts"])
    check_layouts(f"{vector_id} manifest", [layout["manifest"]], [expected["manifest_layout"]])
    for spec, payload in zip(
        [spec for spec in files if spec.entry_type == "regular"],
        expected["file_payloads"],
        strict=True,
    ):
        assert_eq(spec.path, payload["path"], f"{vector_id} {spec.path} payload path")
        assert_eq(spec.size_bytes, payload["size_bytes"], f"{vector_id} {spec.path} size_bytes")
        assert_eq(hx(spec.file_sha256 or b""), payload["sha256"], f"{vector_id} {spec.path} sha256")
    assert_eq(
        expected.get("symlinks", []),
        [{"path": path, "target": target} for path, target in expected_symlinks(files).items()],
        f"{vector_id} symlinks",
    )
    assert_eq(
        expected.get("hardlinks", []),
        [{"path": path, "target": target} for path, target in expected_hardlinks(files).items()],
        f"{vector_id} hardlinks",
    )
    assert_eq(expected.get("directories", []), [spec.path for spec in files if spec.entry_type == "directory"], f"{vector_id} directories")
    assert_eq(expected.get("xattrs", []), expected_xattrs(files), f"{vector_id} xattrs")
    return PlaintextVector(
        vector_id=vector_id,
        chunk_size=options["chunk_size"],
        plaintext=plaintext,
        expected_files=expected_regular_files(files, layout["manifest_cbor"]),
        expected_symlinks=expected_symlinks(files),
        expected_hardlinks=expected_hardlinks(files),
        expected_directories=expected_directories(files),
    )


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check-plaintext-interop",
        action="store_true",
        help="also require Section 14 extraction equality for positive plaintext vectors",
    )
    parser.add_argument(
        "--allow-missing-bsdtar",
        action="store_true",
        help="with --check-plaintext-interop, run available checks when bsdtar is absent",
    )
    parser.add_argument(
        "--write-new-plaintext-fixtures",
        action="store_true",
        help="regenerate the additional RAO 13.1 plaintext fixture JSON files",
    )
    parser.add_argument(
        "--export-directory",
        type=pathlib.Path,
        help="write the regenerated positive object byte streams to this directory",
    )
    parser.add_argument(
        "--fixture-directory",
        type=pathlib.Path,
        default=FIXTURES,
        help="directory containing the RAO fixture manifests",
    )
    parser.add_argument(
        "--encrypted-object-directory",
        type=pathlib.Path,
        default=FIXTURES / "objects",
        help="directory containing the pinned RAO-TV-E2 and encrypted RAO-TV-D1 objects",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    fixture_directory = args.fixture_directory
    if args.write_new_plaintext_fixtures:
        for filename, vector_id, options, files in positive_plaintext_vector_definitions():
            payload = fixture_json(vector_id, options, files)
            (fixture_directory / filename).write_text(
                json.dumps(payload, indent=2, ensure_ascii=False) + "\n",
                encoding="utf-8",
            )
        print("wrote additional RAO 13.1 positive plaintext fixtures")

    p1 = load(fixture_directory, "rao-tv-p1.json")
    e2 = load(fixture_directory, "rao-tv-e2.json")
    d1 = load(fixture_directory, "rao-tv-d1.json")

    p1_files = [
        FileSpec("a/hello.txt", "00000000-0000-4000-8000-000000000010", b"hello, rem archive object\n"),
        FileSpec("b/pattern.bin", "00000000-0000-4000-8000-000000000011", bytes(i % 256 for i in range(5000))),
    ]
    for spec, expected in zip(p1_files, p1["expected"]["file_payloads"], strict=True):
        assert_eq(spec.path, expected["path"], f"RAO-TV-P1 {spec.path} path")
        assert_eq(spec.size_bytes, expected["size_bytes"], f"RAO-TV-P1 {spec.path} size_bytes")
        assert_eq(hx(spec.file_sha256), expected["sha256"], f"RAO-TV-P1 {spec.path} sha256")
    p1_plaintext, p1_layout = build_plaintext(p1["inputs"], p1_files)
    check_plaintext("RAO-TV-P1", p1, p1_plaintext, p1_layout, p1["expected"])
    check_layouts("RAO-TV-P1", p1_layout["files"], p1["expected"]["file_layouts"])
    check_layouts("RAO-TV-P1 manifest", [p1_layout["manifest"]], [p1["expected"]["manifest_layout"]])

    d1_file = FileSpec(
        "v.bin",
        "00000000-0000-4000-8000-000000000012",
        bytes(i % 256 for i in range(262145)),
    )
    d1_expected_payload = d1["expected"]["plaintext"]["file_payload"]
    assert_eq(d1_file.path, d1_expected_payload["path"], "RAO-TV-D1 payload path")
    assert_eq(d1_file.size_bytes, d1_expected_payload["size_bytes"], "RAO-TV-D1 payload size_bytes")
    assert_eq(hx(d1_file.file_sha256), d1_expected_payload["sha256"], "RAO-TV-D1 payload sha256")
    d1_plaintext, d1_layout = build_plaintext(d1["inputs"], [d1_file])
    d1_expected_plain = d1["expected"]["plaintext"]
    check_plaintext("RAO-TV-D1 plaintext", d1, d1_plaintext, d1_layout, d1_expected_plain)
    check_layouts("RAO-TV-D1", d1_layout["files"], [d1_expected_plain["file_layout"]])
    check_layouts("RAO-TV-D1 manifest", [d1_layout["manifest"]], [d1_expected_plain["manifest_layout"]])

    p1_vector = PlaintextVector(
        vector_id="RAO-TV-P1",
        chunk_size=p1["inputs"]["chunk_size"],
        plaintext=p1_plaintext,
        expected_files=expected_regular_files(p1_files, p1_layout["manifest_cbor"]),
        expected_symlinks=expected_symlinks(p1_files),
        expected_hardlinks=expected_hardlinks(p1_files),
        expected_directories=expected_directories(p1_files),
    )
    d1_vector = PlaintextVector(
        vector_id="RAO-TV-D1",
        chunk_size=d1["inputs"]["chunk_size"],
        plaintext=d1_plaintext,
        expected_files=expected_regular_files([d1_file], d1_layout["manifest_cbor"]),
        expected_symlinks=expected_symlinks([d1_file]),
        expected_hardlinks=expected_hardlinks([d1_file]),
        expected_directories=expected_directories([d1_file]),
    )
    extra_plaintext_vectors = [
        check_positive_plaintext_fixture(
            fixture_directory, filename, vector_id, options, files
        )
        for filename, vector_id, options, files in positive_plaintext_vector_definitions()
    ]

    encrypted_directory = args.encrypted_object_directory
    e2_stored = (encrypted_directory / "rao-tv-e2.rao").read_bytes()
    d1_encrypted_stored = (encrypted_directory / "rao-tv-d1-encrypted.rao").read_bytes()
    check_recovery_vector(
        p1_vector,
        "RAO-TV-E2",
        e2_stored,
        e2,
        p1["inputs"]["object_id"],
    )
    check_recovery_vector(
        d1_vector,
        "RAO-TV-D1 encrypted",
        d1_encrypted_stored,
        d1,
        d1["inputs"]["object_id"],
    )

    if args.export_directory is not None:
        export_directory = args.export_directory
        export_directory.mkdir(parents=True, exist_ok=True)
        exports = {
            "rao-tv-p1.rao": p1_plaintext,
            "rao-tv-d1-plaintext.rao": d1_plaintext,
            "rao-tv-e2.rao": e2_stored,
            "rao-tv-d1-encrypted.rao": d1_encrypted_stored,
        }
        exports.update(
            {
                f"{vector.vector_id.lower()}.rao": vector.plaintext
                for vector in extra_plaintext_vectors
            }
        )
        for filename, payload in sorted(exports.items()):
            (export_directory / filename).write_bytes(payload)
        print(f"exported {len(exports)} positive RAO byte streams to {export_directory}")

    if args.check_plaintext_interop:
        check_plaintext_interop(
            [p1_vector, d1_vector, *extra_plaintext_vectors],
            args.allow_missing_bsdtar,
        )

    print(
        "verified RAO-TV-E2 and RAO-TV-D1 encrypted OPEN, RAO-TV-P1 and "
        "RAO-TV-D1 plaintext, and additional RAO positive vectors independently"
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as exc:  # noqa: BLE001 - command-line verifier prints concise failures.
        print(f"independent RAO vector verification failed: {exc}", file=sys.stderr)
        raise
