"""Independently decode B2 medium/fence CBOR from production-generated vectors.

Uses only Python's standard library, with hand-written layouts rather than Rust
field declarations. Checks all complete, valid frames (including fold refusals).
Malformed byte-tail fixtures remain the production decoder tests' responsibility.
"""

from pathlib import Path
import struct
import sys


def cbor(data, pos=0):
    initial = data[pos]
    pos += 1
    major, value = initial >> 5, initial & 31
    if 24 <= value <= 27:
        size = 1 << (value - 24)
        value = int.from_bytes(data[pos:pos + size], "big")
        pos += size
    if major == 0:
        return value, pos
    if major == 1:
        return -1 - value, pos
    if major in (2, 3):
        raw = data[pos:pos + value]
        assert len(raw) == value
        return raw if major == 2 else raw.decode(), pos + value
    if major == 4:
        result = []
        for _ in range(value):
            item, pos = cbor(data, pos)
            result.append(item)
        return result, pos
    if major == 5:
        result = {}
        for _ in range(value):
            key, pos = cbor(data, pos)
            item, pos = cbor(data, pos)
            assert key not in result
            result[key] = item
        return result, pos
    if major == 7 and value in (20, 21, 22):
        return {20: False, 21: True, 22: None}[value], pos
    raise ValueError("unsupported CBOR")


def keys(value, count):
    assert type(value) is dict and list(value) == list(range(count))


def uint(value, bits):
    assert type(value) is int and 0 <= value < 1 << bits


def medium(value):
    keys(value, 6)
    family = value[0]
    assert type(family) is list and len(family) == 2
    uint(family[0], 8)
    if family[0] == 0:
        uint(family[1], 8)
        assert family[1] <= 9
    else:
        assert family == [1, None]
    uint(value[1], 64)
    uint(value[2], 32)
    assert type(value[3]) is bool and type(value[4]) is bool
    assert type(value[5]) is int and value[5] in (0, 1, 2)
    return family[0]


def main(root):
    seen = set()
    count = 0
    # FRAMES contains frames successfully decoded by the production reader;
    # offsets let this independent decoder inspect exactly the same raw bytes.
    for line in (root / "FRAMES.tsv").read_text().splitlines()[1:]:
        case, sequence, tag, filename, start, end, digest = line.split("\t")
        data = (root / case / filename).read_bytes()[int(start):int(end)]
        version, code, length, payload, seq = struct.unpack_from("<HHIIQ", data)
        assert version == 1 and seq == int(sequence)
        assert payload == 32 or (case == "rejected/payload-length" and payload == 0)
        value, stop = cbor(data, 28)
        assert stop == 28 + length and len(data) == 44 + length + payload
        assert data[36 + length:36 + length + payload].hex() == digest
        keys(value, 2)
        body = value[1]
        if code == 4:
            assert tag == "Reprovision"
            keys(body, 10)
            seen.add(("family", medium(body[6])))
            count += 1
        elif code == 5:
            assert tag == "MediumSpecSet"
            keys(body, 3)
            medium(body[1])
            keys(body[2], 2)
            seen.add(("tag", 5))
            count += 1
        elif code == 8:
            keys(body, 6)
            scope = body[1]
            assert type(scope) is list and len(scope) == 2
            uint(scope[0], 8)
            assert scope[0] in (0, 1, 2)
            uint(body[2], 8)
            assert body[2] <= 12
            if scope[0] == 2:
                keys(scope[1], 2)
                assert all(type(v) is str and len(v.encode()) <= 4096 for v in scope[1].values())
                seen.add(("reserved", "model"))
            if body[2] == 12:
                seen.add(("reserved", "quarantine"))
    required = {("family", 0), ("family", 1), ("tag", 5),
                ("reserved", "model"), ("reserved", "quarantine")}
    assert required <= seen, required - seen
    print(f"independent decoder: {count} medium records; both families and reserved fence codes")


if __name__ == "__main__":
    main(Path(sys.argv[1]))
