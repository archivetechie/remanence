#!/usr/bin/env python3
"""Enumerate every field of the Layer 5 wire contract and its presence verdict.

WHY THIS EXISTS, AND WHY IT IS NOT A CODE SCANNER
--------------------------------------------------
proto3 scalars have no field presence by default: an unset number, string, bool
or bytes is indistinguishable on the wire from one deliberately set to zero,
empty or false. That cost a month of investigation once already — a drive that
was retired but still installed reported an empty identity, and nothing could
say whether that meant "no drive" or "I could not find out".

The obvious way to find the rest of that class is to search the code for places
that discard an absence. That method does not work, and it fails in the worst
direction: quietly. Scanning this repository for it produced 22 sites, then 142
once the scan was made multi-line aware, and a review panel independently
produced 78, 82, 86, 96 and 137. Nobody could reproduce anybody. Worse, the
single worst offender in the tree contains no absence to discard at all — it
constructs a message with eighteen zero-seeded fields and simply never fills
them in, so no scanner looking for discarded absences would ever see it.

So this tool enumerates the CONTRACT instead. The descriptor set emitted by
build.rs is the authoritative field list; it is finite, and walking it cannot
undercount. Every field gets a row and every row gets a verdict. A field that
has never been considered is visible as exactly that, which is the property the
scanning approach could never offer.

VERDICTS
--------
  optional         explicit presence already declared; absence is expressible
  enum             carries its own *_UNSPECIFIED = 0; absence is already stated
  repeated         emptiness is unambiguous by length
  message          presence is inherent to a submessage
  decided-plain    examined, and the zero value is genuinely the honest answer
  UNEXAMINED       nobody has yet said which of the above this is

The ledger of decided-plain verdicts lives in tools/wire-presence-ledger.txt,
one line per field with the reason. It is a record of decisions taken, not a
list of permitted exceptions: a field is in it because someone looked, and the
reason is there to be argued with.

THE GATE
--------
`--check` fails on any unexamined field, which is the end state. Until then
`--check-baseline` is what CI runs: it fails only on fields that are unexamined
AND absent from tools/wire-presence-baseline.txt.

That makes it a ratchet rather than a wall. The 266 fields nobody has looked at
yet do not block unrelated work, but a NEW field lands outside the baseline and
fails immediately -- so the debt can shrink and can never grow. Fixing a field
means deleting its baseline line, and the gate then refuses to let it regress:
a field that leaves the baseline can never quietly re-enter it.

The baseline is a debt register, not a permission list. Nothing should ever be
added to it by hand. `--write-baseline` regenerates it, and the gate rejects a
regeneration that grew.

Usage:
    python3 tools/wire_presence_census.py            # summary
    python3 tools/wire_presence_census.py --list     # every field and verdict
    python3 tools/wire_presence_census.py --check    # exit 1 on any UNEXAMINED
    python3 tools/wire_presence_census.py --check-baseline   # exit 1 on NEW unexamined
    python3 tools/wire_presence_census.py --write-baseline   # regenerate (must not grow)
"""

from __future__ import annotations

import argparse
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
LEDGER = ROOT / "tools" / "wire-presence-ledger.txt"
BASELINE = ROOT / "tools" / "wire-presence-baseline.txt"

# FieldDescriptorProto.type values we treat as presence-less scalars in proto3.
SCALAR_TYPES = {
    1: "double", 2: "float", 3: "int64", 4: "uint64", 5: "int32",
    6: "fixed64", 7: "fixed32", 8: "bool", 9: "string", 12: "bytes",
    13: "uint32", 15: "sfixed32", 16: "sfixed64", 17: "sint32", 18: "sint64",
}
TYPE_MESSAGE = 11
TYPE_ENUM = 14
LABEL_REPEATED = 3


def find_descriptor() -> pathlib.Path:
    hits = sorted(
        ROOT.glob("target/*/build/remanence-api-*/out/layer5_descriptor.bin"),
        key=lambda p: p.stat().st_mtime,
        reverse=True,
    )
    if not hits:
        sys.exit(
            "no descriptor set found — run `cargo build -p remanence-api` first "
            "(build.rs emits it into OUT_DIR)"
        )
    return hits[0]


def load_descriptor(path: pathlib.Path):
    try:
        from google.protobuf import descriptor_pb2
    except ImportError:
        sys.exit(
            "protobuf python package is required: python3 -m pip install protobuf"
        )
    fds = descriptor_pb2.FileDescriptorSet()
    fds.ParseFromString(path.read_bytes())
    return fds


def load_ledger() -> dict[str, str]:
    """field key -> reason. Absent file is fine; every field then reads UNEXAMINED."""
    out: dict[str, str] = {}
    if not LEDGER.is_file():
        return out
    for lineno, line in enumerate(LEDGER.read_text().splitlines(), 1):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split(None, 1)
        if len(parts) != 2:
            sys.exit(f"{LEDGER.name}:{lineno}: expected '<Message.field> <reason>'")
        out[parts[0]] = parts[1]
    return out


def read_baseline() -> set[str]:
    if not BASELINE.is_file():
        return set()
    return {
        line.strip()
        for line in BASELINE.read_text().splitlines()
        if line.strip() and not line.startswith("#")
    }


def walk(fds, prefix_files=("layer5.proto",)):
    """Yield (qualified_name, field) for every field of every message, nested included."""
    def walk_message(msg, path):
        qualified = f"{path}.{msg.name}" if path else msg.name
        for field in msg.field:
            yield qualified, field
        for nested in msg.nested_type:
            yield from walk_message(nested, qualified)

    for f in fds.file:
        if not any(f.name.endswith(p) for p in prefix_files):
            continue
        for msg in f.message_type:
            yield from walk_message(msg, "")


def verdict_for(field, ledger_key: str, ledger: dict[str, str]) -> tuple[str, str]:
    if field.label == LABEL_REPEATED:
        return "repeated", "emptiness is unambiguous by length"
    if field.type == TYPE_MESSAGE:
        return "message", "presence is inherent to a submessage"
    if field.type == TYPE_ENUM:
        return "enum", "carries its own UNSPECIFIED = 0"
    if getattr(field, "proto3_optional", False):
        return "optional", "explicit presence declared"
    if ledger_key in ledger:
        return "decided-plain", ledger[ledger_key]
    return "UNEXAMINED", "no verdict recorded"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--list", action="store_true", help="print every field and verdict")
    ap.add_argument("--check", action="store_true", help="exit 1 if any field is UNEXAMINED")
    ap.add_argument("--unexamined", action="store_true", help="print only unexamined fields")
    ap.add_argument("--check-baseline", action="store_true",
                    help="exit 1 only for unexamined fields absent from the baseline")
    ap.add_argument("--write-baseline", action="store_true",
                    help="regenerate the baseline; refuses to grow it")
    args = ap.parse_args()

    fds = load_descriptor(find_descriptor())
    ledger = load_ledger()

    counts: dict[str, int] = {}
    rows: list[tuple[str, str, str, str]] = []
    for message, field in walk(fds):
        key = f"{message}.{field.name}"
        kind, reason = verdict_for(field, key, ledger)
        counts[kind] = counts.get(kind, 0) + 1
        type_name = SCALAR_TYPES.get(field.type, {11: "message", 14: "enum"}.get(field.type, "?"))
        rows.append((key, type_name, kind, reason))

    total = len(rows)
    if args.list or args.unexamined:
        for key, type_name, kind, reason in sorted(rows):
            if args.unexamined and kind != "UNEXAMINED":
                continue
            print(f"{kind:15} {type_name:9} {key:58} {reason}")
        print()

    print(f"layer5 contract: {total} fields")
    for kind in ("optional", "enum", "repeated", "message", "decided-plain", "UNEXAMINED"):
        if kind in counts:
            print(f"  {counts[kind]:4}  {kind}")

    unexamined_keys = {key for key, _, kind, _ in rows if kind == "UNEXAMINED"}

    if args.write_baseline:
        previous = read_baseline()
        added = unexamined_keys - previous
        if previous and added:
            print(
                f"refusing to grow the baseline by {len(added)} field(s):\n  "
                + "\n  ".join(sorted(added))
                + "\n\nThe baseline is a debt register. A field is added to it only by "
                "being added to the contract without a presence verdict, which is what "
                "the gate exists to prevent. Give these fields a verdict instead.",
                file=sys.stderr,
            )
            return 1
        BASELINE.write_text(
            "# Fields that had no presence verdict when the gate went in.\n"
            "#\n"
            "# A debt register, not a permission list. This file may only ever shrink:\n"
            "# delete a line when the field gets a verdict. Never add one by hand -- a\n"
            "# new field belongs in the contract with its presence already decided.\n"
            "#\n"
            "# Regenerate with: python3 tools/wire_presence_census.py --write-baseline\n"
            + "".join(f"{key}\n" for key in sorted(unexamined_keys))
        )
        print(f"baseline written: {len(unexamined_keys)} field(s)")
        if previous:
            print(f"  {len(previous - unexamined_keys)} field(s) resolved since last time")
        return 0

    if args.check_baseline:
        baseline = read_baseline()
        new_debt = sorted(unexamined_keys - baseline)
        resolved = sorted(baseline - unexamined_keys)
        if new_debt:
            print(
                f"\n{len(new_debt)} field(s) entered the contract with no presence "
                "verdict:\n  " + "\n  ".join(new_debt)
                + "\n\nEvery field must say whether its value can be unknown. Declare it "
                "`optional`, give the enum an UNSPECIFIED member, or record in "
                f"{LEDGER.name} why zero is the honest answer for it.\n"
                "Do NOT add these to the baseline -- it exists to bound work that "
                "predates the gate, not to absorb new work.",
                file=sys.stderr,
            )
            return 1
        print(f"\ngate: no new unexamined fields ({len(baseline)} pre-existing)")
        if resolved:
            print(
                f"  {len(resolved)} baseline field(s) now resolved -- drop them with "
                "--write-baseline"
            )
        return 0

    unexamined = counts.get("UNEXAMINED", 0)
    if args.check and unexamined:
        print(
            f"\n{unexamined} field(s) have no recorded presence verdict.\n"
            "Every field must be one of: declared `optional`, an enum with "
            "UNSPECIFIED, repeated, a submessage, or listed in "
            f"{LEDGER.relative_to(ROOT)} with the reason its zero value is honest.\n"
            "Run with --unexamined to list them.",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
