#!/usr/bin/env python3
"""Inventory changed specification content and validate explicit dispositions.

This mechanical gate checks completeness and byte-derived content locations.
It does not decide whether a changed requirement is correct: changed/retired
items require a candidate-bound independent review receipt from the supervisor.
Private dispositions and receipts need not be published with the generic tool.
"""
from __future__ import annotations

import argparse
import difflib
import gzip
import hashlib
import json
import re
from pathlib import Path


def read_text(path: Path) -> str:
    raw = path.read_bytes()
    if path.suffix == ".gz":
        raw = gzip.decompress(raw)
    return raw.decode("utf-8")


def sha(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def normalized(text: str) -> str:
    """Allow wrapping and whitespace only; punctuation and identifiers matter."""
    return re.sub(r"\s+", " ", text).strip()


def canonical_hash(value: dict) -> str:
    return sha(json.dumps(value, sort_keys=True, separators=(",", ":")))


def blocks(text: str) -> list[dict]:
    """Inventory every prose block plus schemas, headings, rows, and identifiers."""
    lines = text.splitlines()
    items = []

    def add(kind: str, start: int, end: int) -> None:
        while start <= end and not lines[start].strip():
            start += 1
        while end >= start and not lines[end].strip():
            end -= 1
        if start > end:
            return
        content = "\n".join(lines[start:end + 1])
        digest = sha(normalized(content))
        items.append({"id": f"{kind}:{start + 1}:{end + 1}:{digest[:20]}", "kind": kind,
                      "source_span": [start + 1, end + 1], "sha256": digest, "text": content})

    start = 0
    fence = None
    for index, line in enumerate(lines):
        marker = re.match(r"^ {0,3}(`{3,}|~{3,})", line)
        if fence is not None:
            if re.match(r"^ {0,3}" + re.escape(fence[0]) + "{" + str(len(fence)) + r",}\s*$", line):
                add("fence", start, index)
                start, fence = index + 1, None
            continue
        if marker:
            add("block", start, index - 1)
            start, fence = index, marker.group(1)
            continue
        if not line.strip():
            add("block", start, index - 1)
            start = index + 1
        elif re.match(r"^ {0,3}#{1,6}\s", line):
            add("block", start, index - 1)
            add("heading", index, index)
            start = index + 1
        if line.lstrip().startswith("|"):
            add("table-row", index, index)
        if re.search(r"`[^`]+`", line):
            add("identifier-line", index, index)
        if re.search(r"\b(?:MUST|SHALL|SHOULD|MAY|REQUIRED|RECOMMENDED)\b", line):
            add("normative-line", index, index)
        if re.search(r"(?:=|≤|≥|\\frac|ceil\(|floor\()", line) and not line.lstrip().startswith("|"):
            add("formula-line", index, index)
    add("fence" if fence else "block", start, len(lines) - 1)
    return sorted(items, key=lambda item: (item["source_span"], item["kind"]))


def inventory(baseline: str, candidate: str) -> dict:
    """Find affected original items without a hand-maintained omission list."""
    old, new = baseline.splitlines(), candidate.splitlines()
    changed = set()
    additions = []
    for tag, first, last, new_first, new_last in difflib.SequenceMatcher(a=old, b=new, autojunk=False).get_opcodes():
        if tag in {"delete", "replace"}:
            changed.update(range(first + 1, last + 1))
        if tag in {"insert", "replace"}:
            additions.append([new_first + 1, new_last])
    complete = blocks(baseline)
    sections = []
    for item in (item for item in complete if item["kind"] == "heading"):
        first = item["source_span"][0]
        depth = len(re.match(r"^ *(#+)", old[first - 1]).group(1))
        end = len(old)
        for index in range(first, len(old)):
            heading = re.match(r"^ {0,3}(#{1,6})\s", old[index])
            if heading and len(heading.group(1)) <= depth:
                end = index
                break
        sections.append({"id": item["id"], "source_span": [first, end]})
    affected = [item for item in complete if any(n in changed for n in range(item["source_span"][0], item["source_span"][1] + 1))]
    return {"schema": "spec-preservation.v1", "baseline_sha256": sha(baseline),
            "candidate_sha256": sha(candidate), "baseline_inventory": complete,
            "items": affected, "sections": sections, "added_spans": additions}


def span_text(candidate: str, span: object) -> str:
    lines = candidate.splitlines()
    if not isinstance(span, list) or len(span) != 2 or any(type(n) is not int for n in span):
        raise ValueError("candidate_span must contain two line numbers")
    start, end = span
    if not 1 <= start <= end <= len(lines):
        raise ValueError("candidate_span is out of range")
    return "\n".join(lines[start - 1:end])


def check(baseline: str, candidate: str, dispositions: dict | None = None,
          receipt: dict | None = None, design: dict | None = None, *, edit_kind: str | None = None) -> list[str]:
    """Reject unaccounted loss and stale, incomplete, or false disposition claims."""
    expected = inventory(baseline, candidate)
    required = {item["id"]: item for item in expected["items"]}
    if dispositions is None:
        return [f"unaccounted {item['kind']} at {item['source_span']}: {key}" for key, item in required.items()]
    errors = []
    for key in ("baseline_sha256", "candidate_sha256"):
        if dispositions.get(key) != expected[key]:
            errors.append(f"stale disposition {key}")
    provided = {}
    rows = dispositions.get("items")
    if not isinstance(rows, list):
        return errors + ["disposition items must be a list"]
    for row in rows:
        if not isinstance(row, dict) or not isinstance(row.get("id"), str):
            errors.append("invalid disposition row")
            continue
        if row["id"] in provided:
            errors.append("duplicate disposition: " + row["id"])
        provided[row["id"]] = row
    errors.extend("missing disposition: " + key for key in required.keys() - provided.keys())
    errors.extend("stale/unknown disposition: " + key for key in provided.keys() - required.keys())
    reviewed = set()
    for key in required.keys() & provided.keys():
        item, row = required[key], provided[key]
        status = row.get("status")
        if status not in {"retained", "moved", "changed", "retired"}:
            errors.append(f"{key}: unknown disposition")
            continue
        if status != "retired":
            try:
                target = span_text(candidate, row.get("candidate_span"))
                if status in {"retained", "moved"} and sha(normalized(target)) != item["sha256"]:
                    errors.append(f"{key}: original content is absent from the claimed candidate span")
            except ValueError as exc:
                errors.append(f"{key}: {exc}")
        if status in {"changed", "retired"}:
            reviewed.add(key)
            if not isinstance(row.get("rationale"), str) or len(row["rationale"].split()) < 4:
                errors.append(f"{key}: substantive rationale required")
        if status == "retired":
            if edit_kind != "substantive":
                errors.append(f"{key}: retirement requires an explicit substantive edit classification")
            design_key = row.get("design_item")
            containing = [section["id"] for section in expected["sections"]
                          if section["source_span"][0] <= item["source_span"][0]
                          and section["source_span"][1] >= item["source_span"][1]]
            if not design or design.get("baseline_sha256") != expected["baseline_sha256"] or design_key not in containing:
                errors.append(f"{key}: design item is not a pinned containing source section")
            if not design_key or not design or design.get("items", {}).get(design_key) != "retired" or any(
                    design.get("items", {}).get(parent) == "retained" for parent in containing):
                errors.append(f"{key}: retirement conflicts with or lacks a design disposition")
    if reviewed:
        if not isinstance(receipt, dict):
            errors.append("changed/retired content requires an independent review receipt")
        else:
            identities = {"baseline_sha256": expected["baseline_sha256"], "candidate_sha256": expected["candidate_sha256"],
                          "inventory_sha256": canonical_hash(expected), "dispositions_sha256": canonical_hash(dispositions)}
            if design:
                identities["design_sha256"] = canonical_hash(design)
            if any(receipt.get(key) != value for key, value in identities.items()):
                errors.append("review receipt is bound to different evidence")
            if receipt.get("verdict") != "passed" or not receipt.get("reviewer") or not receipt.get("author") or receipt.get("reviewer") == receipt.get("author"):
                errors.append("independent passing reviewer identity required")
            ids = receipt.get("reviewed_ids")
            if not isinstance(ids, list) or not reviewed.issubset(set(ids)):
                errors.append("review receipt omits changed/retired items")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("inventory", "check"))
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--out", type=Path)
    parser.add_argument("--dispositions", type=Path)
    parser.add_argument("--receipt", type=Path)
    parser.add_argument("--design", type=Path)
    parser.add_argument("--edit-kind", choices=("substantive", "wording"))
    args = parser.parse_args()
    try:
        baseline, candidate = read_text(args.baseline), read_text(args.candidate)
        if args.command == "inventory":
            value = json.dumps(inventory(baseline, candidate), indent=2) + "\n"
            if args.out:
                args.out.write_text(value)
            else:
                print(value, end="")
            return 0
        dispositions = json.loads(args.dispositions.read_text()) if args.dispositions else None
        receipt = json.loads(args.receipt.read_text()) if args.receipt else None
        design = json.loads(args.design.read_text()) if args.design else None
        errors = check(baseline, candidate, dispositions, receipt, design, edit_kind=args.edit_kind)
        for error in errors:
            print(error)
        print(f"spec-preservation: {'FAIL' if errors else 'PASS'} ({len(errors)} finding(s))")
        return 1 if errors else 0
    except (OSError, ValueError, TypeError, AttributeError, KeyError) as exc:
        print(f"spec-preservation: ERROR: {exc}")
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
