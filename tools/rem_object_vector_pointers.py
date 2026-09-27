#!/usr/bin/env python3
"""Print the frozen REM-OBJECT pointer errata without extracting or changing the tar.

Ownership is explicit below, after reading each case against the preparing
specifications. Recorded pointers are historical data, not inferred ownership.
D1 has two halves; the accepted manifest extension belongs to the positive
section even though its manifest is named negative-manifest.json.
"""

from __future__ import annotations

import json
import pathlib
import tarfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
ARCHIVE = ROOT / "specs/publication/remanence-test-vectors.tar"
MANIFEST_OWNERS = {
    "negative-envelope.json": "REM-ENCRYPT 13.4",
    "negative-inner.json": "REM-ENCRYPT 13.4",
    "negative-key-frame.json": "REM-ENCRYPT 13.4",
    "negative-manifest.json": "REM-OBJECT 13.6",
    "negative-plaintext-reader.json": "REM-OBJECT 13.6",
    "negative-writer.json": "REM-OBJECT 13.6",
    "rem-object-tv-attribute-ext-combined.json": "REM-OBJECT 13.1",
    "rem-object-tv-boundary.json": "REM-OBJECT 13.1",
    "rem-object-tv-d1.json": "REM-OBJECT 13.4; REM-ENCRYPT 13.2",
    "rem-object-tv-e2.json": "REM-ENCRYPT 13.1",
    "rem-object-tv-empty-file.json": "REM-OBJECT 13.1",
    "rem-object-tv-empty.json": "REM-OBJECT 13.1",
    "rem-object-tv-ext-member.json": "REM-OBJECT 13.1",
    "rem-object-tv-hardlinks.json": "REM-OBJECT 13.1",
    "rem-object-tv-manifest.json": "REM-OBJECT 13.1",
    "rem-object-tv-metadata.json": "REM-OBJECT 13.1",
    "rem-object-tv-nonregular.json": "REM-OBJECT 13.1",
    "rem-object-tv-nonuser-attribute.json": "REM-OBJECT 13.1",
    "rem-object-tv-one-byte.json": "REM-OBJECT 13.1",
    "rem-object-tv-order.json": "REM-OBJECT 13.1",
    "rem-object-tv-p1.json": "REM-OBJECT 13.2",
    "rem-object-tv-paths.json": "REM-OBJECT 13.1",
    "rem-object-tv-portable-core-only.json": "REM-OBJECT 13.1",
    "rem-object-tv-xattrs.json": "REM-OBJECT 13.5",
}
CASE_OVERRIDES = {
    ("negative-manifest.json", "unknown-extra-key-accepted"): "REM-OBJECT 13.1",
}
# The index spans documents and includes range cases outside either negative section.
INDEX_OWNERS = {
    "REM-OBJECT-TV-ATTRIBUTE-EXT-COMBINED": "REM-OBJECT 13.1",
    "REM-OBJECT-TV-EXT-MEMBER": "REM-OBJECT 13.1",
    "REM-OBJECT-TV-NONUSER-ATTRIBUTE": "REM-OBJECT 13.1",
    "REM-OBJECT-TV-PORTABLE-CORE-ONLY": "REM-OBJECT 13.1",
    "ext-member-noncanonical-cbor": "REM-OBJECT 13.6",
    "ext-value-not-map": "REM-OBJECT 13.6",
    "inventory-disagrees-with-entries": "REM-OBJECT 13.6",
    "key-frame-len-above-maximum": "REM-ENCRYPT 13.4",
    "key-frame-len-below-minimum": "REM-ENCRYPT 13.4",
    "manifest-tamper-altered-first-chunk-lba": "REM-OBJECT 13.6",
    "manifest-tamper-repointed-path": "REM-OBJECT 13.6",
    "manifest-tamper-swapped-file-sha256": "REM-OBJECT 13.6",
    "reserved-wrap-suite-01": "REM-ENCRYPT 13.4",
    "encrypted-last-object-chunk": "REM-ENCRYPT 13.2",
    "encrypted-last-object-chunk-wrong-finality": "REM-ENCRYPT 13.2",
}


def pointer_rows(archive: pathlib.Path = ARCHIVE) -> list[tuple[str, str, str, str]]:
    """Return every index entry and manifest case, failing on unmapped additions."""
    rows = []
    with tarfile.open(archive, "r") as tar:
        index = json.load(tar.extractfile("rem-object/vectors.json"))
        assert {v["id"] for v in index["vectors"]} == set(INDEX_OWNERS)
        for case in index["vectors"]:
            rows.append(("vectors.json", case["id"],
                         case.get("spec_section", index["spec_section"]),
                         INDEX_OWNERS[case["id"]]))
        manifests = sorted(m.name for m in tar.getmembers()
                           if m.name.startswith("rem-object/manifests/")
                           and m.name.endswith(".json"))
        assert {pathlib.PurePosixPath(m).name for m in manifests} == set(MANIFEST_OWNERS)
        for name in manifests:
            manifest = json.load(tar.extractfile(name))
            filename = pathlib.PurePosixPath(name).name
            for case in manifest.get("cases", [manifest]):
                case_id = case.get("id", case.get("vector_id"))
                assert case_id, name
                rows.append((filename, case_id,
                             case.get("spec_section", manifest["spec_section"]),
                             CASE_OVERRIDES.get((filename, case_id), MANIFEST_OWNERS[filename])))
    return rows


def render_table() -> str:
    """Render the table embedded verbatim in the candidate README."""
    rows = pointer_rows()
    lines = ["| Source | Case | Recorded `spec_section` | Current owner |",
             "| --- | --- | --- | --- |"]
    lines.extend(f"| {source} | {case} | {recorded} | {owner} |"
                 for source, case, recorded, owner in rows)
    return "\n".join(lines) + "\n"


if __name__ == "__main__":
    print(render_table(), end="")
