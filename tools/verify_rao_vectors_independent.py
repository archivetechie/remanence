#!/usr/bin/env python3
"""Independently re-derive the RAO 1.0 positive vector manifests.

This verifier deliberately avoids the Remanence Rust crates. It rebuilds the
RAO-TV-P1 and RAO-TV-D1 plaintext tar streams, the additional RAO 13.1
positive plaintext vectors, deterministic manifest CBOR, and RAO-TV-E1/D1
encrypted envelopes using Python standard-library code plus cryptography's
ChaCha20-Poly1305 implementation. The goal is to catch reference
implementation bugs before pinned-at-generation values become conformance
anchors.

With --check-plaintext-interop it also exercises the Section 14 plaintext
interop gate for the positive plaintext vectors using GNU tar, bsdtar, and
Python's tarfile module.

With --long-term-recovery-drill it performs the Section 14 drill from the
stored bytes alone: standard tar extracts a payload and manifest from a
plaintext object, a standalone CBOR decoder verifies the manifest, and a
generic ChaCha20-Poly1305/HKDF opener decrypts the encrypted twin before the
same tar/CBOR payload verification.
"""

from __future__ import annotations

import argparse
import hashlib
import hmac
import json
import pathlib
import shutil
import subprocess
import sys
import tarfile
import tempfile
from dataclasses import dataclass, field
from typing import Any

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
LABEL_SALT = b"rao1-salt-v1"
LABEL_OBJECT = b"rao1-object-v1"
LABEL_METADATA = b"rao1-metadata-v1"
LABEL_PAYLOAD = b"rao1-payload-v1"
KEY_ID = b"KID:rao-tv-e1.01"
ROOT_KEY = bytes(range(32))


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
    key_id: bytes
    salt: bytes
    metadata_frame_len: int
    object_id: str


def sha256(data: bytes) -> bytes:
    return hashlib.sha256(data).digest()


def hx(data: bytes) -> str:
    return data.hex()


def load(name: str) -> dict[str, Any]:
    with (FIXTURES / name).open("r", encoding="utf-8") as handle:
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


def metadata_plaintext(plaintext_size: int, plaintext_digest: bytes) -> bytes:
    return cbor({0: 1, 1: plaintext_size, 2: "sha256", 3: plaintext_digest})


def derive_salt(root_key: bytes, object_id: str, plaintext_digest: bytes, metadata: bytes) -> bytes:
    metadata_hash = sha256(metadata)
    oid = object_id_field(object_id)
    for ctr in range(256):
        info = LABEL_SALT + bytes([ctr]) + oid + plaintext_digest + metadata_hash
        salt = hkdf(b"", root_key, info, 16)
        if salt != b"\0" * 16:
            return salt
    raise AssertionError("could not derive nonzero salt")


def serialize_header(chunk_size: int, key_id: bytes, salt: bytes, metadata_frame_len: int, object_id: str) -> bytes:
    return (
        b"RAO1"
        + RAO_HEADER_LEN.to_bytes(2, "big")
        + b"\x01\x01"
        + chunk_size.to_bytes(4, "big")
        + (0).to_bytes(4, "big")
        + key_id
        + salt
        + metadata_frame_len.to_bytes(8, "big")
        + b"\0" * 8
        + object_id_field(object_id)
    )


def stream_nonce(counter: int, final_chunk: bool) -> bytes:
    return b"\0\0\0" + counter.to_bytes(8, "big") + (b"\x01" if final_chunk else b"\x00")


def seal(plaintext: bytes, options: dict[str, Any], root_key: bytes, key_id: bytes) -> dict[str, Any]:
    chunk_size = options["chunk_size"]
    plaintext_digest = sha256(plaintext)
    metadata = metadata_plaintext(len(plaintext), plaintext_digest)
    metadata_frame_len = len(metadata) + 16
    salt = derive_salt(root_key, options["object_id"], plaintext_digest, metadata)
    header = serialize_header(chunk_size, key_id, salt, metadata_frame_len, options["object_id"])
    header_hash = sha256(header)
    object_secret = hkdf(salt, root_key, LABEL_OBJECT + header_hash, 32)
    metadata_key = hkdf(b"", object_secret, LABEL_METADATA, 32)
    payload_key = hkdf(b"", object_secret, LABEL_PAYLOAD, 32)

    metadata_frame = ChaCha20Poly1305(metadata_key).encrypt(b"\0" * 12, metadata, b"")
    payload = bytearray()
    chunks = [plaintext[i : i + chunk_size] for i in range(0, len(plaintext), chunk_size)]
    for index, chunk in enumerate(chunks):
        payload.extend(
            ChaCha20Poly1305(payload_key).encrypt(
                stream_nonce(index, index + 1 == len(chunks)),
                chunk,
                b"",
            )
        )
    stored = bytearray(header + metadata_frame + bytes(payload) + RAO_FOOTER)
    stored_size = round_up(len(stored), chunk_size)
    stored.extend(b"\0" * (stored_size - len(stored)))
    return {
        "plaintext_size": len(plaintext),
        "chunk_count": len(chunks),
        "metadata_plaintext_len": len(metadata),
        "metadata_frame_len": metadata_frame_len,
        "payload_frame_start": RAO_HEADER_LEN + metadata_frame_len,
        "payload_frame_end_inclusive": RAO_HEADER_LEN + metadata_frame_len + len(payload) - 1,
        "footer_offset": RAO_HEADER_LEN + metadata_frame_len + len(payload),
        "stored_size_bytes": len(stored),
        "stored_size_blocks": len(stored) // chunk_size,
        "hkdf_salt": salt,
        "header": header,
        "header_hash": header_hash,
        "metadata_key": metadata_key,
        "payload_key": payload_key,
        "metadata_frame": metadata_frame,
        "payload_frame": bytes(payload),
        "stored": bytes(stored),
        "plaintext_digest": plaintext_digest,
    }


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


def check_encrypted(vector_id: str, actual: dict[str, Any], expected: dict[str, Any]) -> None:
    scalar_fields = [
        "plaintext_size",
        "chunk_count",
        "metadata_plaintext_len",
        "metadata_frame_len",
        "payload_frame_start",
        "payload_frame_end_inclusive",
        "footer_offset",
        "stored_size_bytes",
        "stored_size_blocks",
    ]
    for field in scalar_fields:
        assert_eq(actual[field], expected[field], f"{vector_id} {field}")
    assert_eq(hx(actual["hkdf_salt"]), expected["hkdf_salt"], f"{vector_id} hkdf_salt")
    assert_eq(hx(actual["header"]), expected["header_hex"], f"{vector_id} header_hex")
    assert_eq(hx(actual["header_hash"]), expected["header_hash"], f"{vector_id} header_hash")
    assert_eq(hx(actual["metadata_key"]), expected["metadata_key"], f"{vector_id} metadata_key")
    assert_eq(hx(actual["payload_key"]), expected["payload_key"], f"{vector_id} payload_key")
    assert_eq(hx(actual["metadata_frame"]), expected["metadata_frame_hex"], f"{vector_id} metadata_frame_hex")
    assert_eq(hx(sha256(actual["payload_frame"])), expected["payload_frame_sha256"], f"{vector_id} payload_frame_sha256")
    assert_eq(hx(sha256(actual["stored"])), expected["stored_digest"], f"{vector_id} stored_digest")
    assert_eq(hx(actual["plaintext_digest"]), expected["plaintext_digest"], f"{vector_id} plaintext_digest")


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


def require_standard_tar() -> str:
    path = shutil.which("tar")
    if path is None:
        raise AssertionError("standard tar not found on PATH")
    return path


def run_standard_tar_extract(
    vector_id: str,
    chunk_size: int,
    archive: pathlib.Path,
    out_dir: pathlib.Path,
) -> None:
    tar_path = require_standard_tar()
    result = subprocess.run(
        [
            tar_path,
            "-b",
            str(chunk_size // TAR_RECORD_SIZE),
            "-xf",
            str(archive),
            "-C",
            str(out_dir),
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise AssertionError(
            f"{vector_id} standard tar failed with exit {result.returncode}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )


def find_manifest_entry(manifest: dict[Any, Any], path: str) -> dict[Any, Any]:
    entries = manifest.get("file_entries")
    if not isinstance(entries, list):
        raise AssertionError("manifest file_entries is not an array")
    matches = [entry for entry in entries if isinstance(entry, dict) and entry.get("path") == path]
    if len(matches) != 1:
        raise AssertionError(f"manifest has {len(matches)} entries for {path!r}")
    return matches[0]


def verify_manifest_payload(
    vector_id: str,
    manifest_cbor: bytes,
    payload_path: str,
    payload: bytes,
    chunk_size: int,
    object_id: str,
) -> None:
    manifest = decode_cbor_exact(manifest_cbor)
    if not isinstance(manifest, dict):
        raise AssertionError(f"{vector_id}: manifest is not a CBOR map")
    assert_eq(manifest.get("schema_version"), 1, f"{vector_id} manifest schema_version")
    assert_eq(manifest.get("chunk_size"), chunk_size, f"{vector_id} manifest chunk_size")
    assert_eq(manifest.get("object_id"), object_id, f"{vector_id} manifest object_id")
    entry = find_manifest_entry(manifest, payload_path)
    assert_eq(entry.get("size_bytes"), len(payload), f"{vector_id} {payload_path} size_bytes")
    assert_eq(entry.get("file_sha256"), sha256(payload), f"{vector_id} {payload_path} file_sha256")
    assert_eq(entry.get("chunk_count"), chunk_count(len(payload), chunk_size), f"{vector_id} {payload_path} chunk_count")


def verify_payload_with_standard_tar(
    vector_id: str,
    archive_bytes: bytes,
    chunk_size: int,
    payload_path: str,
    expected_payload: bytes,
    object_id: str,
    tmp: pathlib.Path,
) -> None:
    archive = tmp / f"{vector_id}.rao"
    out_dir = tmp / f"{vector_id}-extract"
    out_dir.mkdir()
    archive.write_bytes(archive_bytes)
    run_standard_tar_extract(vector_id, chunk_size, archive, out_dir)
    payload_file = out_dir / payload_path
    manifest_file = out_dir / MANIFEST_PATH
    if not payload_file.is_file():
        raise AssertionError(f"{vector_id}: standard tar did not extract {payload_path!r}")
    if not manifest_file.is_file():
        raise AssertionError(f"{vector_id}: standard tar did not extract {MANIFEST_PATH!r}")
    payload = payload_file.read_bytes()
    assert_eq(payload, expected_payload, f"{vector_id} standard tar {payload_path}")
    verify_manifest_payload(
        vector_id,
        manifest_file.read_bytes(),
        payload_path,
        payload,
        chunk_size,
        object_id,
    )


def parse_encrypted_header(stored: bytes) -> EncryptedHeader:
    if len(stored) < RAO_HEADER_LEN:
        raise AssertionError("encrypted object is shorter than the RAO1 header")
    header = stored[:RAO_HEADER_LEN]
    assert_eq(header[:4], b"RAO1", "encrypted header magic")
    assert_eq(int.from_bytes(header[4:6], "big"), RAO_HEADER_LEN, "encrypted header_len")
    assert_eq(header[6], 1, "encrypted format_version")
    assert_eq(header[7], 1, "encrypted suite_id")
    chunk_size = int.from_bytes(header[8:12], "big")
    if chunk_size <= 0 or chunk_size % TAR_RECORD_SIZE != 0:
        raise AssertionError(f"encrypted chunk_size is invalid: {chunk_size}")
    assert_eq(header[12:16], b"\0" * 4, "encrypted flags")
    key_id = header[16:32]
    if key_id == b"\0" * 16:
        raise AssertionError("encrypted key_id is all zero")
    salt = header[32:48]
    if salt == b"\0" * 16:
        raise AssertionError("encrypted hkdf_salt is all zero")
    metadata_frame_len = int.from_bytes(header[48:56], "big")
    if metadata_frame_len < 17:
        raise AssertionError(f"metadata frame length is too small: {metadata_frame_len}")
    assert_eq(header[56:64], b"\0" * 8, "encrypted reserved")
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
    return EncryptedHeader(header, chunk_size, key_id, salt, metadata_frame_len, object_id)


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
    root_key: bytes,
    expected_key_id: bytes,
) -> tuple[bytes, EncryptedHeader]:
    header = parse_encrypted_header(stored)
    assert_eq(header.key_id, expected_key_id, f"{vector_id} key_id")
    header_hash = sha256(header.bytes)
    object_secret = hkdf(header.salt, root_key, LABEL_OBJECT + header_hash, 32)
    metadata_key = hkdf(b"", object_secret, LABEL_METADATA, 32)
    payload_key = hkdf(b"", object_secret, LABEL_PAYLOAD, 32)

    metadata_start = RAO_HEADER_LEN
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
    expected_salt = derive_salt(root_key, header.object_id, plaintext_digest, metadata_plain)
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
    return recovered, header


def check_long_term_recovery_drill(
    plaintext_vector: PlaintextVector,
    encrypted_vector_id: str,
    encrypted_stored: bytes,
    encrypted_root_key: bytes,
    encrypted_key_id: bytes,
    payload_path: str,
    object_id: str,
) -> None:
    expected_payload = plaintext_vector.expected_files[payload_path]
    with tempfile.TemporaryDirectory(prefix="rao-long-term-recovery-") as tmp_name:
        tmp = pathlib.Path(tmp_name)
        verify_payload_with_standard_tar(
            plaintext_vector.vector_id,
            plaintext_vector.plaintext,
            plaintext_vector.chunk_size,
            payload_path,
            expected_payload,
            object_id,
            tmp,
        )
        recovered_plaintext, encrypted_header = open_encrypted_with_generic_crypto(
            encrypted_vector_id,
            encrypted_stored,
            encrypted_root_key,
            encrypted_key_id,
        )
        assert_eq(recovered_plaintext, plaintext_vector.plaintext, f"{encrypted_vector_id} recovered plaintext")
        assert_eq(encrypted_header.chunk_size, plaintext_vector.chunk_size, f"{encrypted_vector_id} header chunk_size")
        assert_eq(encrypted_header.object_id, object_id, f"{encrypted_vector_id} header object_id")
        verify_payload_with_standard_tar(
            encrypted_vector_id,
            recovered_plaintext,
            encrypted_header.chunk_size,
            payload_path,
            expected_payload,
            encrypted_header.object_id,
            tmp,
        )
    print(
        "verified RAO long-term recovery drill for RAO-TV-P1 plaintext and "
        "RAO-TV-E1 encrypted twin"
    )


def check_positive_plaintext_fixture(
    filename: str,
    vector_id: str,
    options: dict[str, Any],
    files: list[FileSpec],
) -> PlaintextVector:
    fixture = load(filename)
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
        "--long-term-recovery-drill",
        action="store_true",
        help="also run the Section 14 plaintext/encrypted long-term recovery drill",
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
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    if args.write_new_plaintext_fixtures:
        for filename, vector_id, options, files in positive_plaintext_vector_definitions():
            payload = fixture_json(vector_id, options, files)
            (FIXTURES / filename).write_text(
                json.dumps(payload, indent=2, ensure_ascii=False) + "\n",
                encoding="utf-8",
            )
        print("wrote additional RAO 13.1 positive plaintext fixtures")

    p1 = load("rao-tv-p1.json")
    e1 = load("rao-tv-e1.json")
    d1 = load("rao-tv-d1.json")

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

    e1_actual = seal(p1_plaintext, p1["inputs"], ROOT_KEY, KEY_ID)
    check_encrypted("RAO-TV-E1", e1_actual, e1["expected"])
    assert_eq(e1["expected"]["plaintext_digest"], p1["expected"]["stored_digest"], "RAO-TV-E1 plaintext_digest equality")

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

    d1_actual = seal(d1_plaintext, d1["inputs"], bytes.fromhex(d1["inputs"]["encrypted_root_key"]), bytes.fromhex(d1["inputs"]["encrypted_key_id"]))
    check_encrypted("RAO-TV-D1 encrypted", d1_actual, d1["expected"]["encrypted"])
    assert_eq(d1["expected"]["encrypted"]["plaintext_digest"], d1_expected_plain["stored_digest"], "RAO-TV-D1 plaintext_digest equality")

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
        check_positive_plaintext_fixture(filename, vector_id, options, files)
        for filename, vector_id, options, files in positive_plaintext_vector_definitions()
    ]

    if args.check_plaintext_interop:
        check_plaintext_interop(
            [p1_vector, d1_vector, *extra_plaintext_vectors],
            args.allow_missing_bsdtar,
        )

    if args.long_term_recovery_drill:
        check_long_term_recovery_drill(
            p1_vector,
            "RAO-TV-E1",
            e1_actual["stored"],
            ROOT_KEY,
            KEY_ID,
            "b/pattern.bin",
            p1["inputs"]["object_id"],
        )

    print(
        "verified RAO-TV-P1, RAO-TV-E1, RAO-TV-D1, and additional "
        "RAO 13.1 positive plaintext vectors independently"
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as exc:  # noqa: BLE001 - command-line verifier prints concise failures.
        print(f"independent RAO vector verification failed: {exc}", file=sys.stderr)
        raise
