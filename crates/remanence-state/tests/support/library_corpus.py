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
        if len(raw) != value:
            raise ValueError("truncated CBOR string")
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
            if key in result:
                raise ValueError("duplicate CBOR map key")
            result[key] = item
        return result, pos
    if major == 7 and value in (20, 21, 22):
        return {20: False, 21: True, 22: None}[value], pos
    raise ValueError("unsupported CBOR")


def keys(value, count):
    if not (type(value) is dict and list(value) == list(range(count))):
        raise ValueError(f"expected map with ascending keys 0..{count - 1}")


def uint(value, bits):
    if not (type(value) is int and 0 <= value < 1 << bits):
        raise ValueError(f"expected unsigned {bits}-bit integer")


def medium_family(family):
    if not (type(family) is list and len(family) == 2):
        raise ValueError("expected two-element medium family union")
    uint(family[0], 8)
    if family[0] == 0:
        uint(family[1], 8)
        if family[1] > 9:
            raise ValueError("invalid LTO generation")
    else:
        if family != [1, None]:
            raise ValueError('invalid medium family')
    return family[0]


def medium(value):
    keys(value, 6)
    family_code = medium_family(value[0])
    uint(value[1], 64)
    uint(value[2], 32)
    if not (type(value[3]) is bool and type(value[4]) is bool):
        raise ValueError("medium capabilities must be booleans")
    if not (type(value[5]) is int and value[5] in (0, 1, 2)):
        raise ValueError("invalid medium source")
    return family_code


def detail(entries):
    # Ordering is a fold rule: shape-negative corpus entries still decode here.
    if not (type(entries) is list and len(entries) <= 256):
        raise ValueError("invalid detail array")
    for entry in entries:
        if not (type(entry) is list and len(entry) == 2 and type(entry[0]) is str and len(entry[0].encode()) <= 4096):
            raise ValueError("invalid detail entry")
        value = entry[1]
        if not (type(value) is list and len(value) == 2 and type(value[0]) is int and value[0] in range(6)):
            raise ValueError("invalid diagnostic value")
        kind, payload = value
        if kind == 0 and not (type(payload) is str and len(payload.encode()) <= 4096):
            raise ValueError("invalid detail text")
        if kind == 1:
            uint(payload, 64)
        if kind == 2 and not (type(payload) is int and -(1 << 63) <= payload < 0):
            raise ValueError("invalid negative detail")
        if kind == 3 and not (type(payload) is bytes and len(payload) <= 4194304):
            raise ValueError("invalid detail bytes")
        if kind == 4 and type(payload) is not bool:
            raise ValueError("invalid detail bool")
        if kind == 5 and payload is not None:
            raise ValueError("invalid detail null")


def main(root):
    seen = set()
    count = 0
    medium_cases = {}
    # FRAMES contains frames successfully decoded by the production reader;
    # offsets let this independent decoder inspect exactly the same raw bytes.
    for line in (root / "FRAMES.tsv").read_text().splitlines()[1:]:
        case, sequence, tag, filename, start, end, digest = line.split("\t")
        data = (root / case / filename).read_bytes()[int(start):int(end)]
        version, code, length, payload, seq = struct.unpack_from("<HHIIQ", data)
        if not (version == 1 and seq == int(sequence)):
            raise ValueError("frame version or sequence mismatch")
        if not (payload == 32 or (case == "rejected/payload-length" and payload == 0)):
            raise ValueError("unexpected frame payload length")
        value, stop = cbor(data, 28)
        if not (stop == 28 + length and len(data) == 44 + length + payload):
            raise ValueError("frame length mismatch")
        if not (data[36 + length:36 + length + payload].hex() == digest):
            raise ValueError("frame digest disagrees with manifest")
        keys(value, 2)
        body = value[1]
        if code == 6:
            keys(body, 5)
            keys(body[0], 3)
            for key, size in [(1, 16), (2, 16), (3, 32)]:
                if body[key] is not None and not (type(body[key]) is bytes and len(body[key]) == size):
                    raise ValueError("invalid optional supersede field")
            uint(body[4], 64)
            if body[0][1] == 0:
                seen.add(("empty-prefix", True))
                if body[2] is not None:
                    raise ValueError("fresh corpus supersede must have no session")
        if code == 4:
            if tag != "Reprovision":
                raise ValueError("Reprovision tag mismatch")
            keys(body, 13)
            if body[10] is not None:
                source = body[10]
                if not (type(source) is list and len(source) == 2 and type(source[0]) is int and source[0] in (0, 1)):
                    raise ValueError("invalid pool source")
                if source[0] == 0 and source[1] is not None:
                    raise ValueError("operator pool source requires null")
                if source[0] == 1:
                    keys(source[1], 2)
            for key in (11, 12):
                if body[key] is not None and not (type(body[key]) is bytes and len(body[key]) == 16):
                    raise ValueError("invalid reprovision identity")
            seen.add(("family", medium(body[6])))
            count += 1
            medium_cases[case] = medium_cases.get(case, 0) + 1
        elif code == 5:
            if tag != "MediumSpecSet":
                raise ValueError("MediumSpecSet tag mismatch")
            keys(body, 3)
            medium(body[1])
            keys(body[2], 2)
            seen.add(("tag", 5))
            count += 1
            medium_cases[case] = medium_cases.get(case, 0) + 1
        elif code == 8:
            keys(body, 7)
            detail(body[6])
            scope = body[1]
            if not (type(scope) is list and len(scope) == 2):
                raise ValueError("expected two-element fence scope union")
            uint(scope[0], 8)
            if scope[0] not in (0, 1, 2):
                raise ValueError("invalid fence scope")
            uint(body[2], 8)
            if body[2] > 14:
                raise ValueError("invalid fence cause")
            if scope[0] == 2:
                keys(scope[1], 2)
                if not all(type(v) is str and len(v.encode()) <= 4096 for v in scope[1].values()):
                    raise ValueError("invalid model scope text")
                seen.add(("reserved", "model"))
            if body[2] == 12:
                seen.add(("reserved", "quarantine"))
        elif code == 9:
            keys(body, 4)
            detail(body[3])
        elif code == 14:
            keys(body, 9)
            if body[1] is not None:
                keys(body[1], 3)
            keys(body[7], 2)
            uint(body[8], 8)
            if body[8] > 2:
                raise ValueError("invalid verification outcome")
        elif code == 12:
            keys(body, 5)
            subject = body[0]
            if not (type(subject) is list and len(subject) == 2):
                raise ValueError("invalid inventory subject")
            if type(subject[0]) is not int or subject[0] not in (0, 1):
                raise ValueError("invalid inventory subject code")
            serial = subject[1]
            if subject[0] == 1:
                keys(serial, 1)
                serial = serial[0]
            if not (type(serial) is str and len(serial.encode()) <= 4096):
                raise ValueError("invalid inventory subject serial")
            uint(body[1], 64)
            seen.add(("inventory-subject", subject[0]))
        elif code == 17:
            keys(body, 18)
            uint(body[4], 8)
            if body[4] > 50:
                raise ValueError("invalid readiness phase")
            if body[8] is not None:
                uint(body[8], 64)
            if body[15] is not None:
                uint(body[15], 16)
            if body[16] is not None and not (type(body[16]) is bytes and len(body[16]) == 16):
                raise ValueError("invalid readiness run")
            if body[17] is not None:
                uint(body[17], 8)
                if body[17] > 2:
                    raise ValueError("invalid cancellation source")
            if type(body[13]) is not int or body[13] not in (0, 1):
                raise ValueError("invalid readiness scope")
            seen.add(("readiness-scope", body[13]))
            if body[14] is not None:
                medium_family(body[14])
                seen.add(("readiness-media", True))
        elif code == 21:
            keys(body, 5)
            action, payload = body[4]
            if type(action) is not int or action not in range(6):
                raise ValueError("invalid cleaning action")
            if action == 1:
                keys(payload, 1)
            elif action == 2:
                keys(payload, 2)
                if payload[1] is not None:
                    uint(payload[1], 16)
                # Shape-negative histories decode; the fold refuses their meaning.
                if payload[0] == 2 and payload[1] is not None:
                    seen.add(("cleaning-home", True))
            elif action == 5:
                keys(payload, 1)
                seen.add(("cleaning-reject", True))
        elif code == 22:
            keys(body, 4)
            action, payload = body[2]
            if type(action) is not int or action not in range(4):
                raise ValueError("invalid alarm action")
            if action == 0:
                keys(payload, 3)
                uint(payload[0], 8)
                if payload[0] > 12:
                    raise ValueError("invalid alarm kind")
            if action == 3:
                keys(payload, 3)
                uint(payload[0], 8)
                if payload[2] is not None:
                    keys(payload[2], 2)
                seen.add(("alarm-refresh", True))
        elif code == 32:
            keys(body, 4)
            uint(body[0], 8)
            if body[0] > 20:
                raise ValueError("invalid diagnostic event")
            detail(body[3])
    required = {("empty-prefix", True), ("inventory-subject", 0), ("inventory-subject", 1), ("family", 0), ("family", 1), ("tag", 5),
                ("reserved", "model"), ("reserved", "quarantine"),
                ("readiness-scope", 0), ("readiness-scope", 1),
                ("readiness-media", True), ("alarm-refresh", True),
                ("cleaning-home", True), ("cleaning-reject", True)}
    if not required <= seen:
        raise ValueError(f"missing corpus coverage: {required - seen}")
    # These complete histories pin the measured-generation ruling before corpus freeze.
    for case, expected in {
        "medium-label-generation-correction": 3,
        "rejected/medium-measured-generation-change": 3,
        "rejected/medium-measured-two-step-generation-change": 4,
    }.items():
        if medium_cases.get(case) != expected:
            raise ValueError(f"missing medium history frames: {case}")
    print(f"independent decoder: {count} medium records; both families and reserved fence codes; all three generation histories; ruling 17/18 layouts")


if __name__ == "__main__":
    main(Path(sys.argv[1]))
