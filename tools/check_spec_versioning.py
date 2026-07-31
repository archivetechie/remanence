#!/usr/bin/env python3
"""Versioning consistency linter for the publication documents.

Born from the 2026-07-31 versioning panel: three hand-copied policy sections
plus the site is four copies with no consistency check, and the freeze-day
errata shipped a document carrying two different version numbers in one file
and a Section 18 that contradicted its own Status section. Every rule below
corresponds to a defect that actually occurred or one the panel showed would
occur at the first future revision.

Checked:
  1. Each document's Status `Version` row is three-part and extends the
     major.minor line named by its Identifiers table / title.
  2. The change-policy core is identical across the three specifications
     modulo the declared substitution table.
  3. The pinned vector-archive SHA is identical at every site that quotes it.
  4. No document cites the retired software version-DOI as its own.
  5. Cross-reference titles use the canonical citation forms.
  6. Every "Appendix X" reference inside a document resolves to an appendix
     heading that exists in that document.

Exit 0 clean; exit 1 with findings on stderr.
"""

import re
import sys
import pathlib

ROOT = pathlib.Path(__file__).resolve().parent.parent
PUB = ROOT / "specs" / "publication"

SPECS = {
    "rem-parity-1-specification.md": {"line": "1.0", "noun": "tape"},
    "rem-object-core-1-specification.md": {"line": "1.0", "noun": "object"},
    "rem-encrypt-1-specification.md": {"line": "1.0", "noun": "encrypted object"},
}
COMPANION = "formats-explained.md"

# Sites that must agree on the pinned archive SHA (repo-side; the site's two
# pages are on the release checklist, not reachable from this repo).
SHA_SITES = [
    "specs/publication/rem-parity-1-specification.md",
    "specs/publication/rem-object-core-1-specification.md",
    "specs/publication/rem-encrypt-1-specification.md",
    "specs/publication/formats-explained.md",
    "CHANGELOG.md",
]

CANONICAL_TITLES = [
    "Rem Tape Parity (REM-PARITY) Format",
    "REM-OBJECT Core Format",
    "REM-ENCRYPT",
]
BANNED_TITLE_FORMS = [
    "REM-PARITY Tape Format Specification",   # variant found by the panel
]

findings: list[str] = []


def fail(msg: str) -> None:
    findings.append(msg)


def policy_core(text: str, noun: str) -> str:
    """Extract and normalise the shared change-policy block."""
    m = re.search(r"\*\*Deciding what a change is\.\*\*.*?revision-history\s+entry\.",
                  text, re.S)
    if not m:
        return ""
    core = m.group(0)
    # Normalise the declared substitutions.
    core = core.replace(noun.capitalize() + "s written", "ARTIFACTs written")
    core = re.sub(re.escape(noun) + r"s?", "ARTIFACT", core)
    core = core.replace("Encrypted ARTIFACT", "ARTIFACT").replace("ARTIFACTs", "ARTIFACT")
    # Discriminator clause differs by design: mask the major-version parenthetical.
    core = re.sub(r"\*\*new major version\*\*: [^.]*, and a\s+separate document",
                  "**new major version**: DISCRIMINATOR, and a separate document", core)
    # PARITY carries an extra example sentence inside question 2; strip
    # parentheticals so per-document examples are permitted.
    core = re.sub(r"\([^)]*\)", " ", core)
    core = re.sub(r"\ban ARTIFACT", "a ARTIFACT", core)
    return re.sub(r"\s+", " ", core).strip()


def main() -> int:
    texts = {name: (PUB / name).read_text() for name in SPECS}

    # 1. Version rows.
    for name, meta in SPECS.items():
        t = texts[name]
        m = re.search(r"^\| Version \| (\S+) \|", t, re.M)
        if not m:
            fail(f"{name}: no Status Version row")
            continue
        v = m.group(1)
        if not re.fullmatch(r"\d+\.\d+\.\d+", v):
            fail(f"{name}: Version {v!r} is not three-part")
        elif not v.startswith(meta["line"] + "."):
            fail(f"{name}: Version {v} does not extend the {meta['line']} line")
        dvs = re.findall(r"^\| Document version \| (\S+) \|", t, re.M)
        if not dvs:
            fail(f"{name}: no 'Document version' (line) row")
        for dv in dvs:
            if dv != meta["line"]:
                fail(f"{name}: Identifiers 'Document version' {dv} != line {meta['line']}")

    # 2. Policy core identical modulo substitutions.
    cores = {n: policy_core(t, SPECS[n]["noun"]) for n, t in texts.items()}
    ref_name = "rem-parity-1-specification.md"
    for n, c in cores.items():
        if not c:
            fail(f"{n}: change-policy core not found")
    if all(cores.values()):
        for n, c in cores.items():
            if n != ref_name and c != cores[ref_name]:
                fail(f"{n}: change-policy core diverges from {ref_name} "
                     f"(normalised lengths {len(c)} vs {len(cores[ref_name])})")

    # 3. Archive SHA agreement.
    shas = {}
    for rel in SHA_SITES:
        t = (ROOT / rel).read_text()
        found = set(re.findall(r"\b([0-9a-f]{64})\b", t))
        if found:
            shas[rel] = found
    all_shas = set().union(*shas.values()) if shas else set()
    # The one current pin plus the historically superseded pin in CHANGELOG prose.
    current = [s for s in all_shas if s.startswith("77be73e7")]
    if len(current) != 1:
        fail(f"expected exactly one current archive SHA, saw {sorted(all_shas)}")
    else:
        for rel, found in shas.items():
            if current[0] not in found and rel != "CHANGELOG.md":
                fail(f"{rel}: does not quote the current archive SHA")

    # 4. Retired DOI row.
    for n, t in texts.items():
        if re.search(r"^\| Version DOI \(this release\) \|", t, re.M):
            fail(f"{n}: retired 'Version DOI (this release)' row present")

    # 5. Citation title forms.
    corpus = dict(texts)
    corpus[COMPANION] = (PUB / COMPANION).read_text()
    for n, t in corpus.items():
        for bad in BANNED_TITLE_FORMS:
            if bad in t:
                fail(f"{n}: non-canonical citation form {bad!r}")

    # 6b. Revision histories strictly newest-first.
    for n, t in corpus.items():
        for m in re.finditer(r"^## Appendix [A-Z]\. Revision History.*?(?=^## |\Z)",
                             t, re.M | re.S):
            dates = re.findall(r"^- \*\*(\d{4}-\d{2}-\d{2})", m.group(0), re.M)
            if dates != sorted(dates, reverse=True):
                fail(f"{n}: revision history not newest-first: {dates}")

    # 6. Appendix references resolve.
    for n, t in corpus.items():
        have = set(re.findall(r"^## Appendix ([A-Z])\.", t, re.M))
        for ref in set(re.findall(r"Appendix ([A-Z])(?:[ .,;)]|$)", t)):
            if ref not in have:
                fail(f"{n}: reference to Appendix {ref} but no such appendix")

    if findings:
        print(f"check_spec_versioning: {len(findings)} finding(s)", file=sys.stderr)
        for f in findings:
            print("  - " + f, file=sys.stderr)
        return 1
    print("check_spec_versioning: clean")
    return 0


if __name__ == "__main__":
    sys.exit(main())
