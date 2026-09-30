#!/usr/bin/env python3
"""Check the implementation Guide and docs against the preparing specifications.

Preserves the Guide checkers' quotation and title rules: quotation whitespace is
collapsed, a final full stop may be omitted, and sections include subsections.
REM-OBJECT, REM-ENCRYPT and REM-PARITY citations outside fenced code in docs
Markdown must resolve to real headings; cross-spec placeholders do not qualify
as citation targets, but remain included in quotation section spans.
"""

import argparse
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[1]
DOCS = {
    "REM-OBJECT": "rem-object-core-1-specification.md",
    "REM-ENCRYPT": "rem-encrypt-1-specification.md",
    "REM-PARITY": "rem-parity-1-specification.md",
}
KW = re.compile(r"\b(?:MUST|SHOULD|MAY|REQUIRED|SHALL|RECOMMENDED|OPTIONAL)\b")
NUMBER = r"[0-9A-Z]+(?:\.[0-9]+)*"
HEADING = re.compile(r"^(#{2,4}) (?:Appendix )?(" + NUMBER + r")\.? (.+?)\s*$")
PLACEHOLDER = re.compile(r"\(in REM-(?:OBJECT|ENCRYPT)\)$")
CITATION = re.compile(r"\b(REM-OBJECT|REM-ENCRYPT|REM-PARITY)\s+§(" + NUMBER + r")\b")
ATTRIBUTION = re.compile(
    r'["“]([^"”]+)["”]\s*\(?\s*(?:\((REM-OBJECT|REM-ENCRYPT|REM-PARITY) §('
    + NUMBER
    + r')|[^"“”]{0,40}?\((REM-OBJECT|REM-ENCRYPT|REM-PARITY) §('
    + NUMBER + r'))'
)


def norm(text):
    """Collapse whitespace, as in the original quotation checker."""
    return re.sub(r"\s+", " ", text).strip()


def strip_fences(text):
    """Blank fenced code while preserving line numbers for section boundaries."""
    out, fence = [], None
    for line in text.split("\n"):
        match = re.match(r"^ {0,3}(`{3,}|~{3,})", line)
        if match:
            fence = None if fence and line.strip().startswith(fence) else (fence or match.group(1))
            out.append("")
            continue
        out.append("" if fence else line)
    return "\n".join(out)


def numbered_headings(text):
    """Yield line index, heading level, section number, and title."""
    for index, line in enumerate(strip_fences(text).split("\n")):
        match = HEADING.match(line)
        if match:
            yield index, len(match[1]), match[2], match[3].strip()


def headings(text):
    """Map section numbers to titles, excluding cross-spec placeholders."""
    result = {}
    for _, _, number, title in numbered_headings(text):
        if not PLACEHOLDER.search(title):
            result.setdefault(number, title)
    return result


def sections(text):
    """Map numbers to normalized spans through the next peer/ancestor heading."""
    lines = text.split("\n")
    heads = list(numbered_headings(text))
    spans = {}
    for k, (index, level, number, _) in enumerate(heads):
        end = next((j for j, lv, _, _ in heads[k + 1:] if lv <= level), len(lines))
        spans.setdefault(number, norm("\n".join(lines[index:end])))
    return spans


def check_quotations(guide, specs):
    """Check Guide requirement keywords and attributed quotations."""
    whole = {doc: norm(text) for doc, text in specs.items()}
    secs = {doc: sections(text) for doc, text in specs.items()}
    flat = norm(guide)
    outside = KW.findall(re.sub(r'"[^"]*"|“[^”]*”', "", flat))
    bad, ok = [], 0
    for match in ATTRIBUTION.finditer(flat):
        quote = match[1].rstrip()
        doc, number = (match[2], match[3]) if match[2] else (match[4], match[5])
        variants = {quote, quote[:-1] if quote.endswith(".") else quote}
        if not any(v in whole[doc] for v in variants):
            bad.append(("NOT FOUND", doc, number, quote[:90]))
        elif any(v in secs[doc].get(number, "") for v in variants):
            ok += 1
        else:
            bad.append(("WRONG SECTION", doc, number, quote[:90]))
    print(f"keywords outside quoted spans: {outside or 'none'}")
    print(f"attributed quotations in their named section: {ok}; problems: {len(bad)}")
    for problem in bad:
        print("  ", *problem)
    return len(outside) + len(bad)


def check_citations(documents, heads):
    """Check the three specs' citations outside fences, including cited titles."""
    count = bad = title_count = title_bad = 0
    for path, text in documents.items():
        flat = norm(strip_fences(text))
        for match in CITATION.finditer(flat):
            count += 1
            doc, number = match.groups()
            title = heads.get(doc, {}).get(number)
            if title is None:
                bad += 1
                print(f"NO-HEADING {path}: {doc} §{number}")
            if not flat[match.end():].startswith(", "):
                continue
            rest = flat[match.end() + 2:match.end() + 162]
            if not re.match(r"[A-Z`]", rest):
                continue  # Prose such as "§14, step 3", not a title citation.
            title_count += 1
            cut = re.split(r"[.;)]| and REM-", rest)[0].strip()
            if title is None:
                title_bad += 1
                print(f"NO-HEADING {path}: {doc} §{number} cited as {cut!r}")
            elif not (rest.startswith(title) or title.startswith(cut)):
                title_bad += 1
                print(f"TITLE {path}: {doc} §{number} cited as {cut!r}; heading is {title!r}")
    print(f"title citations checked: {title_count}; problems: {title_bad}")
    print(f"section citations checked: {count}; problems: {bad}")
    return bad + title_bad


def check(root):
    """Run all checks against a checkout; return a shell exit status."""
    specs = {doc: (root / "specs/in-progress" / name).read_text(encoding="utf-8")
             for doc, name in DOCS.items()}
    guide_path = root / "docs/rem-implementation-guide.md"
    guide = guide_path.read_text(encoding="utf-8")  # Missing inputs must fail.
    paths = [guide_path] + sorted((root / "docs").rglob("*.md"))
    documents = {str(path.relative_to(root)): path.read_text(encoding="utf-8")
                 for path in dict.fromkeys(paths)}
    problems = check_quotations(guide, specs)
    problems += check_citations(documents, {doc: headings(text) for doc, text in specs.items()})
    return int(problems != 0)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT, help="checkout to check (default: this checkout)")
    args = parser.parse_args()
    return check(args.root.resolve())


if __name__ == "__main__":
    raise SystemExit(main())
