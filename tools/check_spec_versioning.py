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
  6. Every local "Section N" and "Appendix X" reference resolves to a heading
     that exists in that document.
  7. No version string is reused for different bytes: if a document's current
     version appears in DEPOSITED.sha256, the repository copy must hash to the
     digest recorded there. This is what makes each document's "the deposited
     revision governs" rule mechanically checkable rather than a promise.
  8. A revision being prepared in specs/in-progress/ is a second copy of a
     document in the same repository, which is precisely the drift hazard this
     linter was written for. It is held to the same structural rules as the
     published copy, and must strictly supersede it.
  9. Section 1 of each specification contains exactly one subsection headed
     "What this document specifies, and what it does not", and that
     subsection reads the same, word for word, in every document that carries
     it. Only whitespace is normalised. The published revisions that predate
     the subsection are excepted, listed by exact version; every other copy
     must carry it.
 10. A placeholder heading (a numbered heading, with or without the dot
     after its number, whose title ends "(in REM-OBJECT)" or
     "(in REM-ENCRYPT)") marks a section that the companion holds. It is not a
     section of the document that carries it, and neither is any heading under
     it: a reference that resolves only to a placeholder is reported, and can
     never be listed as known. Each placeholder must repeat exactly, at the
     same level, the heading the companion gives that number; must not share
     its number with a section or another placeholder of its own document; and
     has no body, a subordinate heading included. A heading that ends in the
     suffix without being a numbered placeholder is reported.

Exit 0 clean; exit 1 with findings on stderr.
"""

import hashlib
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

# Rule 9: the scope statement in Section 1 of every specification.
SCOPE_HEADING = "What this document specifies, and what it does not"
SCOPE_BLOCK_EXEMPT = {
    # Published before the scope statement existed. A published copy at any
    # other version must carry it.
    ("publication/rem-object-core-1-specification.md", "1.0.0-draft.3"),
    ("publication/rem-encrypt-1-specification.md", "1.0.0-draft.3"),
    ("publication/rem-parity-1-specification.md", "1.0.0-draft.2"),
}

findings: list[str] = []


def fail(msg: str) -> None:
    findings.append(msg)


def version_key(v: str):
    """Order version strings. A -draft.N orders before the release it anticipates."""
    m = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)(?:-draft\.(\d+))?", v)
    if not m:
        return (0, 0, 0, 0)
    major, minor, patch, draft = m.groups()
    return (int(major), int(minor), int(patch),
            int(draft) if draft is not None else float("inf"))


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


SECTION_NUMBER = r"\d+(?:\.\d+)*"
SECTION_LIST = rf"{SECTION_NUMBER}(?:(?:\s*,\s*(?:and\s+)?|\s+and\s+|\s+through\s+|\s+to\s+|\s*[-–]\s*){SECTION_NUMBER})*"


def unresolved_section_references(text: str) -> set[int]:
    """Return local `Section N` references with no top-level heading.

    The paired REM-OBJECT/REM-ENCRYPT documents explicitly say they omit
    sections owned by the other document. Those declarations explain a gap;
    they are not local cross-references and are excluded deliberately.
    """
    have = {int(value) for value in re.findall(r"^## (\d+)\.", text, re.M)}
    # Historical entries describe references that were removed; they are not
    # active cross-references in the document body.
    active_text = re.sub(
        r"^## (?:Appendix [A-Z]\. )?Revision History.*?(?=^## |\Z)",
        "",
        text,
        flags=re.M | re.S,
    )
    # An omission declaration explains numbering owned by the companion; it is
    # not itself a cross-reference. Remove only the declaration, rather than
    # globally exempting its numbers, so a later `See Section N` still resolves.
    active_text = re.sub(
        rf"\bomits?\s+Sections?\s+{SECTION_LIST}",
        "",
        active_text,
        flags=re.I,
    )
    unresolved: set[int] = set()
    for match in re.finditer(rf"\bSections?\s+({SECTION_LIST})", active_text):
        section_list = match.group(1)
        referenced = {
            int(value.split(".", 1)[0])
            for value in re.findall(SECTION_NUMBER, section_list)
        }
        for start, end in re.findall(
            rf"({SECTION_NUMBER})\s*(?:through|to|[-–])\s*({SECTION_NUMBER})",
            section_list,
            flags=re.I,
        ):
            start_top = int(start.split(".", 1)[0])
            end_top = int(end.split(".", 1)[0])
            referenced.update(range(min(start_top, end_top), max(start_top, end_top) + 1))
        for section in referenced:
            if section not in have:
                unresolved.add(section)
    return unresolved


# Structural rules use one preprocessing boundary. Existing publication-PARITY
# top-level reference semantics above remain intentionally unchanged.
from collections import Counter
import html
from typing import NamedTuple
import unicodedata

DEEP_NUMBER = r"\d+(?:\.\d+)*"
DEEP_LIST = rf"{DEEP_NUMBER}(?:(?:\s*,\s*(?:(?:and|or)\s+)?|\s+(?:and|or|through|to)\s+|\s*[/–-]\s*){DEEP_NUMBER})*"
COMPANIONS = {"Core": "rem-object-core-1-specification.md", "REM-OBJECT": "rem-object-core-1-specification.md",
              "[REMOBJECT]": "rem-object-core-1-specification.md", "REM-ENCRYPT": "rem-encrypt-1-specification.md",
              "[REMENCRYPT]": "rem-encrypt-1-specification.md", "REM-PARITY": "rem-parity-1-specification.md",
              "[REMPARITY]": "rem-parity-1-specification.md"}
ATTRIBUTOR = r"(?:\[[^\]\r\n]+\]|RFC\s+\d+|Core|REM-OBJECT|REM-ENCRYPT|REM-PARITY)"
# Rule 10. REM-OBJECT and REM-ENCRYPT number their sections together; a
# section one of them holds appears in the other as a placeholder heading
# that names the holder. The dot after the number is optional, as it is for
# SECTION_HEADING, so a heading is never a section and a placeholder at once.
SECTION_HEADING = r"(\d+(?:\.\d+)*)(?:\.|\s|$)"
PLACEHOLDER_SUFFIX = re.compile(r"\s*\(in (?:REM-OBJECT|REM-ENCRYPT)\)$")
PLACEHOLDER = re.compile(r"^(\d+(?:\.\d+)*)\.?\s+(.+?)\s+\(in (REM-OBJECT|REM-ENCRYPT)\)$")
PLACEHOLDER_OWNERS = {"REM-OBJECT": "rem-object-core-1-specification.md",
                      "REM-ENCRYPT": "rem-encrypt-1-specification.md"}
HEADING_LINE = re.compile(r"^ {0,3}(#{1,6})\s+(.+?)(?:\s+#+)?\s*$", re.M)


def document_views(text: str) -> tuple[str, str]:
    """Return active prose (fences kept) and full text (fences blanked)."""
    lines = text.splitlines(keepends=True)
    full = []
    fence = None
    for line in lines:
        marker = re.match(r"^ {0,3}(`{3,}|~{3,})", line)
        if fence is None and marker:
            fence = marker.group(1)
            full.append("\n" if line.endswith("\n") else "")
        elif fence is not None:
            if re.match(r"^ {0,3}" + re.escape(fence[0]) + "{" + str(len(fence)) + r",}\s*$", line):
                fence = None
            full.append("\n" if line.endswith("\n") else "")
        else:
            full.append(line)
    active_lines = list(lines)
    historical = False
    for i, line in enumerate(full):
        if re.match(r"^## ", line):
            historical = bool(re.match(r"^## (?:Appendix [A-Z]\. )?Revision History", line))
        if historical:
            active_lines[i] = "\n" if lines[i].endswith("\n") else ""
    active = "".join(active_lines)
    active = re.sub(rf"\bomits?\s+(?:Sections?\s+|§§?\s*){DEEP_LIST}",
                    lambda m: "\n" * m.group().count("\n"), active, flags=re.I)
    return active, "".join(full)


def discovered_documents(root: pathlib.Path = ROOT) -> dict[str, str]:
    """Discover every recognized specification copy, excluding support files."""
    return {str(path.relative_to(root / "specs")): path.read_text()
            for folder in ("publication", "in-progress")
            for path in sorted((root / "specs" / folder).glob("*.md"))
            if path.name in SPECS or path.name == COMPANION}


def headings(text: str) -> list[str]:
    return re.findall(r"^ {0,3}#{1,6}\s+(.+?)(?:\s+#+)?\s*$", document_views(text)[1], re.M)


def heading_entries(text: str) -> list[tuple[int, int, str]]:
    """(line index, level, heading text) for every heading outside fences,
    found exactly as headings() finds them."""
    full = document_views(text)[1]
    entries, line, last = [], 0, 0
    for m in HEADING_LINE.finditer(full):
        line += full.count("\n", last, m.start())
        last = m.start()
        entries.append((line, len(m.group(1)), m.group(2)))
    return entries


def section_numbers(text: str) -> set[str]:
    """Numbers of the sections a document holds. A placeholder is not one, and
    neither is a heading under a placeholder (which is a forbidden body)."""
    numbers, under = set(), None
    for _, level, heading in heading_entries(text):
        if under is not None and level > under:
            continue
        under = level if PLACEHOLDER.match(heading) else None
        if under is None and (m := re.match(SECTION_HEADING, heading)):
            numbers.add(m.group(1))
    return numbers


class Placeholder(NamedTuple):
    index: int      # line index of the heading
    level: int      # number of '#'
    number: str
    heading: str    # heading text after the '#'s, suffix included
    holder: str     # "REM-OBJECT" or "REM-ENCRYPT"


def placeholder_headings(text: str) -> list[Placeholder]:
    """Every placeholder heading, in document order, repeats included."""
    return [Placeholder(index, level, m.group(1), heading, m.group(3))
            for index, level, heading in heading_entries(text)
            if (m := PLACEHOLDER.match(heading))]


def expand_references(value: str) -> list[str]:
    numbers = re.findall(DEEP_NUMBER, value)
    for first, last in re.findall(rf"({DEEP_NUMBER})\s*(?:through|to|[-–])\s*({DEEP_NUMBER})", value):
        left, right = first.split("."), last.split(".")
        if left[:-1] != right[:-1]:
            # A cross-level range has no unambiguous set of section numbers.
            numbers.append("invalid-range:" + first + "–" + last)
            continue
        low, high = sorted((int(left[-1]), int(right[-1])))
        if high - low > 10000:
            numbers.append("invalid-range:" + first + "–" + last)
            continue
        numbers.extend(".".join([*left[:-1], str(n)]) for n in range(low + 1, high))
    return numbers


def section_citations(text: str) -> list[tuple[str, str, str, str]]:
    """Lex references with attribution, preserving occurrence multiplicity:
    (attribution, number, containing paragraph, the citation as written)."""
    active, _ = document_views(text)
    occurrences = []
    for match in re.finditer(rf"(?:\bSections?\s+|§§?\s*)({DEEP_LIST})", active):
        prefix = re.search(rf"(?<!\w)({ATTRIBUTOR})[\W_]*$", active[:match.start()])
        suffix = re.match(rf"\s+of\s+({ATTRIBUTOR})", active[match.end():])
        tag = suffix.group(1) if suffix else prefix.group(1) if prefix else None
        attribution = COMPANIONS.get(tag, "external" if tag else "local")
        # Context is the complete containing paragraph, insensitive to wrapping.
        start = active.rfind("\n\n", 0, match.start())
        start = start + 2 if start >= 0 else 0
        end = active.find("\n\n", match.end())
        context = re.sub(r"\s+", " ", active[max(0, start):end if end >= 0 else len(active)]).strip()
        written = (match.group(0) + suffix.group(0) if suffix
                   else active[prefix.start(1):match.end()] if prefix else match.group(0))
        cited = re.sub(r"\s+", " ", written).strip()
        for number in expand_references(match.group(1)):
            occurrences.append((attribution, number, context, cited))
    return occurrences


def section_references(text: str) -> list[tuple[str, str, str]]:
    """Lex references with attribution, preserving occurrence multiplicity."""
    return [(attribution, number, context) for attribution, number, context, _ in section_citations(text)]


def reference_target(name: str, attribution: str, documents: dict[str, str]) -> str | None:
    """The document a reference resolves against: itself, or the companion's
    preparing copy, else its published copy."""
    if attribution == "local":
        return name
    return next((p for p in ("in-progress/" + attribution, "publication/" + attribution) if p in documents), None)


def unresolved_citations(name: str, documents: dict[str, str]):
    """Each local or companion citation whose number is not a section of the
    document it resolves against: (attribution, number, context, cited, target)."""
    for attribution, number, context, cited in section_citations(documents[name]):
        if attribution == "external":
            continue
        target = reference_target(name, attribution, documents)
        if target is None or number not in section_numbers(documents[target]):
            yield attribution, number, context, cited, target


def deep_unresolved(name: str, documents: dict[str, str]) -> Counter:
    """Resolve local and companion references at their full numbered depth."""
    return Counter((name, attribution, number, context)
                   for attribution, number, context, _, _ in unresolved_citations(name, documents))


def reference_findings(name: str, documents: dict[str, str]) -> tuple[Counter, list[str]]:
    """Split unresolved references into those that meet only a placeholder,
    which are always findings, and the rest, which are reconciled against
    KNOWN_REFERENCES as before."""
    ordinary, messages = Counter(), []
    for attribution, number, context, cited, target in unresolved_citations(name, documents):
        placeholder = next((p for p in placeholder_headings(documents[target]) if p.number == number),
                           None) if target else None
        if placeholder is None:
            ordinary[(name, attribution, number, context)] += 1
            continue
        if len(re.findall(DEEP_NUMBER, cited)) == 1:
            subject = f"{cited} resolves only"
        else:
            subject = f"{cited} cites {number}, which resolves only"
        if PLACEHOLDER_OWNERS[placeholder.holder] == pathlib.Path(name).name:
            advice = f"the section is in this document, so cite it as Section {number}"
        else:
            advice = f"the section is in {placeholder.holder}, so the reference must name that document"
        messages.append(f"{name}: {subject} to the placeholder {placeholder.heading!r} in {target}; "
                        f"{advice} (in: {context[:80]!r})")
    return ordinary, messages


def placeholder_owner(name: str, holder: str, documents: dict[str, str]) -> str | None:
    """The holder's copy in the placeholder's own folder, else in the other."""
    folder = name.split("/", 1)[0] if "/" in name else ""
    folders = [folder] + [f for f in ("in-progress", "publication") if f != folder]
    return next((f"{f}/{PLACEHOLDER_OWNERS[holder]}" for f in folders
                 if f"{f}/{PLACEHOLDER_OWNERS[holder]}" in documents), None)


def placeholder_findings(name: str, documents: dict[str, str]) -> list[str]:
    """Rule 10's checks on the placeholders a document carries."""
    text = documents[name]
    errors = []
    lines = text.splitlines()
    entries = heading_entries(text)
    own = section_numbers(text)
    first: dict[str, str] = {}
    for p in placeholder_headings(text):
        shown = "#" * p.level + " " + p.heading
        if p.number in first:
            errors.append(f"{name}: placeholder {shown!r} shares its number with the placeholder {first[p.number]!r}")
        first.setdefault(p.number, shown)
        if p.number in own:
            errors.append(f"{name}: placeholder {shown!r} duplicates Section {p.number} of this document")
        # The body runs to the next heading at the same or a higher level, so a
        # subordinate heading is body too.
        end = next((i for i, level, _ in entries if i > p.index and level <= p.level), len(lines))
        if any(line.strip() for line in lines[p.index + 1:end]):
            errors.append(f"{name}: placeholder {shown!r} has content before the next heading at its level "
                          "or above; a placeholder has no body")
        owner = placeholder_owner(name, p.holder, documents)
        if owner is None:
            errors.append(f"{name}: placeholder {shown!r} names {p.holder}, which is not present")
            continue
        wanted = [(level, heading) for _, level, heading in heading_entries(documents[owner])
                  if not PLACEHOLDER.match(heading)
                  and (m := re.match(SECTION_HEADING, heading)) and m.group(1) == p.number]
        if not wanted:
            errors.append(f"{name}: placeholder {shown!r} names {p.holder}, but {owner} has no Section {p.number}")
        elif wanted[0] != (p.level, PLACEHOLDER_SUFFIX.sub("", p.heading)):
            errors.append(f"{name}: placeholder {shown!r} does not match the heading "
                          f"{'#' * wanted[0][0] + ' ' + wanted[0][1]!r} of {owner}")
    for _, level, heading in entries:
        if PLACEHOLDER_SUFFIX.search(heading) and not PLACEHOLDER.match(heading):
            shown = "#" * level + " " + heading
            errors.append(f"{name}: heading {shown!r} ends like a placeholder but is not a numbered placeholder heading")
    return errors


def heading_slugs(text: str) -> list[str]:
    """Port GitHub heading slug behavior, including duplicate-name collisions."""
    seen = set()
    output = []
    for heading in headings(text):
        plain = re.sub(r"!?\[([^\]]*)\]\([^)]*\)", r"\1", heading)
        plain = html.unescape(re.sub(r"<[^>]+>", "", plain))
        plain = re.sub(r"\\([\\`*{}\[\]()#+.!_-])", r"\1", plain)
        plain = plain.replace("`", "").replace("*", "")
        base = "".join(c for c in plain.lower() if c in " -" or unicodedata.category(c).startswith(("L", "M")) or unicodedata.category(c) in {"Nd", "Nl", "Pc"}).replace(" ", "-")
        slug, count = base, 0
        while slug in seen:
            count += 1
            slug = f"{base}-{count}"
        seen.add(slug)
        output.append(slug)
    return output


def anchor_occurrences(name: str, text: str) -> Counter:
    _, full = document_views(text)
    return Counter((name, match.group(1), re.sub(r"\s+", " ", match.group(0)))
                   for match in re.finditer(r"\[[^\]\n]*\]\(#([^\s)]+)\)", full))


def unresolved_anchors(name: str, text: str) -> Counter:
    slugs = set(heading_slugs(text))
    return Counter({key: count for key, count in anchor_occurrences(name, text).items() if key[1] not in slugs})


def reconcile_known(actual: Counter, known: Counter) -> list[str]:
    """Both a new defect and an obsolete exception are failures, occurrence by occurrence."""
    return ([f"unlisted {key!r} ({count} occurrence(s))" for key, count in (actual - known).items()]
            + [f"stale known entry {key!r} ({count} occurrence(s))" for key, count in (known - actual).items()])


def heading_spacing(text: str) -> list[int]:
    _, full = document_views(text)
    original = text.splitlines()
    return [i + 1 for i, line in enumerate(full.splitlines())
            if i and re.match(r"^ {0,3}#{1,6}\s", line) and original[i-1].strip()]


def unresolved_appendices(text: str) -> set[str]:
    active, _ = document_views(text)
    have = set()
    for heading in headings(text):
        match = re.match(r"(?:Appendix )?([A-Z](?:\.\d+)*)(?:\.|\s|$)", heading)
        if match:
            have.add(match.group(1))
    return set(re.findall(r"\bAppendix ([A-Z](?:\.\d+)*)", active)) - have


def readme_state_findings(root: pathlib.Path, documents: dict[str, str]) -> list[str]:
    """Validate the preparing-copy table as a bijection, then compare versions."""
    path = root / "specs/in-progress/README.md"
    if not path.is_file():
        return ["in-progress README missing"]
    text = path.read_text()
    section = re.search(r"^## Current state\s*$(.*?)(?=^## |\Z)", text, re.M | re.S)
    if not section:
        return ["in-progress README Current state table missing"]
    rows = [line.strip().strip("|").split("|") for line in section.group(1).splitlines() if line.startswith("|")]
    if not rows or [c.strip() for c in rows[0]][:3] != ["Document", "Preparing", "Published"]:
        return ["in-progress README Current state columns missing"]
    mapping = {"REM-PARITY": "rem-parity-1-specification.md", "REM-OBJECT": "rem-object-core-1-specification.md", "REM-ENCRYPT": "rem-encrypt-1-specification.md", "formats-explained": COMPANION}
    expected = {name.removeprefix("in-progress/") for name in documents if name.startswith("in-progress/")}
    seen, errors = Counter(), []
    for row in rows[2:]:
        row = [c.strip() for c in row]
        name = mapping.get(row[0])
        if name is None or len(row) < 3:
            errors.append("unknown or malformed preparing row: " + row[0])
            continue
        seen[name] += 1
        for prefix, cell in (("in-progress/", row[1]), ("publication/", row[2])):
            content = documents.get(prefix + name)
            match = re.search(r"^\| Version \| (\S+) \|", content or "", re.M)
            actual = cell.split()[0] if prefix == "in-progress/" and cell.split() else cell
            if not match or actual != match.group(1):
                errors.append(f"README {prefix}{name}: missing file/version or mismatched cell {cell!r}")
    if seen != Counter(expected):
        errors.append(f"README is not a bijection: expected {sorted(expected)}, observed {dict(seen)}")
    return errors


KNOWN_REFERENCES = Counter()
KNOWN_ANCHORS = Counter()


def scope_blocks(text: str) -> list[str]:
    """Each scope subsection, from its heading title to the next heading of the
    same or higher level, with runs of whitespace (including line breaks)
    collapsed. The section number in the heading is not part of the result."""
    # The statement is a subsection of Section 1, so its heading is level 3.
    # Headings are found in the view with fenced lines blanked, so a heading-like
    # line inside a code fence neither starts nor ends a block; the compared text
    # comes from the original lines, fences included.
    # It must be numbered as a subsection of Section 1; the number is not compared.
    pattern = re.compile(r"^### 1\.\d+\.? " + re.escape(SCOPE_HEADING) + r"\s*$")
    lines = text.splitlines()
    unfenced = document_views(text)[1].splitlines()
    blocks = []
    for index, line in enumerate(unfenced):
        if not pattern.match(line):
            continue
        body = [SCOPE_HEADING]
        for offset, following in enumerate(unfenced[index + 1:], index + 1):
            if re.match(r"^#{1,3} ", following):
                break
            body.append(lines[offset])
        blocks.append(re.sub(r"\s+", " ", "\n".join(body)).strip())
    return blocks


def scope_block_findings(documents: dict[str, str]) -> list[str]:
    """Rule 9 over every discovered specification copy."""
    errors, found = [], {}
    for name, text in sorted(documents.items()):
        if pathlib.Path(name).name not in SPECS:
            continue
        blocks = scope_blocks(text)
        if len(blocks) > 1:
            errors.append(f"{name}: {len(blocks)} scope statements; exactly one is required")
            continue
        if blocks:
            found[name] = blocks[0]
            continue
        version = re.search(r"^\| Version \| (\S+) \|", text, re.M)
        if (name, version.group(1) if version else "") not in SCOPE_BLOCK_EXEMPT:
            errors.append(f"{name}: scope statement {SCOPE_HEADING!r} missing")
    if found:
        reference = sorted(found)[0]
        for name, block in sorted(found.items()):
            if block != found[reference]:
                errors.append(f"{name}: scope statement differs from {reference}")
    return errors


def archive_pin_findings(name: str, text: str, current: str) -> list[str]:
    """Every digest in a sentence that names the archive must be the current pin.

    The sentence runs from the archive's name to the next full stop that ends a
    sentence (a stop followed by whitespace or the end of the text)."""
    errors = []
    for match in re.finditer(r"remanence-test-vectors\.tar", text):
        rest = text[match.end():]
        stop = re.search(r"\.(?:\s|$)", rest)
        sentence = rest[:stop.start()] if stop else rest
        for quoted in re.findall(r"\b([0-9a-f]{64})\b", sentence):
            if quoted != current:
                errors.append(f"{name}: quotes archive digest {quoted[:16]}… "
                              f"with the archive name; the current pin is {current[:16]}…")
    return errors


def preparing_copy_rule_findings(name: str, text: str, current: str | None) -> list[str]:
    """Rules 1, 3, 4 and 5, applied to a preparing copy in specs/in-progress/."""
    errors = []
    label = f"in-progress/{name}"
    if name in SPECS:
        dvs = re.findall(r"^\| Document version \| (\S+) \|", text, re.M)
        if not dvs:
            errors.append(f"{label}: no 'Document version' (line) row")
        for dv in dvs:
            if dv != SPECS[name]["line"]:
                errors.append(f"{label}: Identifiers 'Document version' {dv} "
                              f"!= line {SPECS[name]['line']}")
        if current is not None:
            if current not in set(re.findall(r"\b([0-9a-f]{64})\b", text)):
                errors.append(f"{label}: does not quote the current archive SHA")
            errors.extend(archive_pin_findings(label, text, current))
    if re.search(r"^\| Version DOI \(this release\) \|", text, re.M):
        errors.append(f"{label}: retired 'Version DOI (this release)' row present")
    for bad in BANNED_TITLE_FORMS:
        if bad in text:
            errors.append(f"{label}: non-canonical citation form {bad!r}")
    return errors


def structural_findings(root: pathlib.Path = ROOT) -> list[str]:
    documents = discovered_documents(root)
    refs, anchors, errors = Counter(), Counter(), []
    for name, text in documents.items():
        if name != "publication/rem-parity-1-specification.md":
            ordinary, placeholder_refs = reference_findings(name, documents)
            refs.update(ordinary)
            errors.extend(placeholder_refs)
        errors.extend(placeholder_findings(name, documents))
        anchors.update(unresolved_anchors(name, text))
        errors.extend(f"{name}:{line}: heading needs preceding blank line" for line in heading_spacing(text))
        errors.extend(f"{name}: unresolved Appendix {ref}" for ref in sorted(unresolved_appendices(text)))
    errors.extend(reconcile_known(refs, KNOWN_REFERENCES))
    errors.extend(reconcile_known(anchors, KNOWN_ANCHORS))
    errors.extend(readme_state_findings(root, documents))
    errors.extend(scope_block_findings(documents))
    for key, count in (refs & KNOWN_REFERENCES).items():
        print(f"known L1: {key!r} ({count})")
    for key, count in (anchors & KNOWN_ANCHORS).items():
        print(f"known L2: {key!r} ({count})")
    return errors


def main() -> int:
    findings.clear()
    texts = {name: (PUB / name).read_text() for name in SPECS}

    # 1. Version rows.
    for name, meta in SPECS.items():
        t = texts[name]
        m = re.search(r"^\| Version \| (\S+) \|", t, re.M)
        if not m:
            fail(f"{name}: no Status Version row")
            continue
        v = m.group(1)
        # A three-part core, optionally carrying a pre-publication suffix
        # (-draft.N), which orders before the release it anticipates.
        if not re.fullmatch(r"\d+\.\d+\.\d+(-draft\.\d+)?", v):
            fail(f"{name}: Version {v!r} is not a three-part version, "
                 "optionally suffixed -draft.N")
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
        for rel in SHA_SITES:
            if rel != "CHANGELOG.md":
                for finding in archive_pin_findings(rel, (ROOT / rel).read_text(), current[0]):
                    fail(finding)

    # 4. Retired DOI row.
    for n, t in texts.items():
        if re.search(r"^\| Version DOI \(this release\) \|", t, re.M):
            fail(f"{n}: retired 'Version DOI (this release)' row present")

    # 5. Citation title forms.
    corpus = dict(texts)
    corpus[COMPANION] = (PUB / COMPANION).read_text()
    title_corpus = dict(corpus)
    title_corpus["specs/README.md"] = (ROOT / "specs" / "README.md").read_text()
    for n, t in title_corpus.items():
        for bad in BANNED_TITLE_FORMS:
            if bad in t:
                fail(f"{n}: non-canonical citation form {bad!r}")

    # 6b. Revision histories strictly newest-first.
    for n, t in corpus.items():
        for m in re.finditer(r"^## (?:Appendix [A-Z]\. )?Revision History.*?(?=^## |\Z)",
                             t, re.M | re.S):
            dates = re.findall(r"^- \*\*(\d{4}-\d{2}-\d{2})", m.group(0), re.M)
            if dates != sorted(dates, reverse=True):
                fail(f"{n}: revision history not newest-first: {dates}")

    # Publication PARITY alone retains the original top-level reference check.
    for ref in sorted(unresolved_section_references(texts["rem-parity-1-specification.md"])):
        fail(f"rem-parity-1-specification.md: unresolved top-level Section {ref}")
    findings.extend(structural_findings(ROOT))

    # 7. A version string is never reused for different bytes.
    #
    # The repository copy may legitimately be ahead of the deposited copy — that
    # is how the next revision is prepared — but then it must carry the next
    # version string. So a given version string identifies one sequence of bytes
    # whether you are holding the deposit, the repository, or a copy unpacked
    # from a source release years later.
    deposited_path = PUB / "DEPOSITED.sha256"
    deposited: dict[tuple[str, str], str] = {}
    if deposited_path.is_file():
        for lineno, line in enumerate(deposited_path.read_text().splitlines(), 1):
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            parts = line.split()
            if len(parts) != 3:
                fail(f"DEPOSITED.sha256 line {lineno}: expected "
                     f"'<filename> <version> <sha256>', got {line!r}")
                continue
            name, version, digest = parts
            if not re.fullmatch(r"[0-9a-f]{64}", digest):
                fail(f"DEPOSITED.sha256 line {lineno}: {digest!r} is not a sha256")
                continue
            if (name, version) in deposited and deposited[(name, version)] != digest:
                fail(f"DEPOSITED.sha256: {name} {version} recorded twice with "
                     "different digests — a published revision is immutable")
            deposited[(name, version)] = digest

    for name in list(SPECS) + [COMPANION]:
        path = PUB / name
        if not path.is_file():
            continue
        text = path.read_text()
        m = re.search(r"^\| Version \| (\S+) \|", text, re.M)
        if not m:
            continue
        version = m.group(1)
        recorded = deposited.get((name, version))
        if recorded is None:
            continue  # not yet deposited: an unreleased revision in preparation
        actual = hashlib.sha256(path.read_bytes()).hexdigest()
        if actual != recorded:
            fail(f"{name}: version {version} was deposited as {recorded[:16]}… but the "
                 f"repository copy hashes to {actual[:16]}… — a published revision is "
                 "immutable, so bump the version string instead of editing it")

    # 8. Revisions in preparation.
    #
    # specs/in-progress/ holds the next revision of a document while it is being
    # assembled, so that publication/ can be trusted to hold only what governs.
    # Two copies of one document in one repository is the drift hazard this
    # linter exists for, so the preparing copy is checked like the published one
    # and must strictly supersede it.
    wip = ROOT / "specs" / "in-progress"
    if wip.is_dir():
        for path in sorted(wip.glob("*.md")):
            name = path.name
            if name == "README.md":
                continue
            if name not in SPECS and name != COMPANION:
                fail(f"in-progress/{name}: not a known specification document")
                continue
            wtext = path.read_text()
            wm = re.search(r"^\| Version \| (\S+) \|", wtext, re.M)
            if not wm:
                fail(f"in-progress/{name}: no Status Version row")
                continue
            wv = wm.group(1)
            if not re.fullmatch(r"\d+\.\d+\.\d+(-draft\.\d+)?", wv):
                fail(f"in-progress/{name}: Version {wv!r} is not a three-part "
                     "version, optionally suffixed -draft.N")
                continue
            if name in SPECS and not wv.startswith(SPECS[name]["line"] + "."):
                fail(f"in-progress/{name}: Version {wv} does not extend the "
                     f"{SPECS[name]['line']} line")
            pub_path = PUB / name
            if pub_path.is_file():
                pm = re.search(r"^\| Version \| (\S+) \|", pub_path.read_text(), re.M)
                if pm and version_key(wv) <= version_key(pm.group(1)):
                    fail(f"in-progress/{name}: Version {wv} does not supersede the "
                         f"published {pm.group(1)} — a copy here is the NEXT revision")
            # Rules 1, 3, 4 and 5 apply to the preparing copy as well.
            for finding in preparing_copy_rule_findings(
                    name, wtext, current[0] if len(current) == 1 else None):
                fail(finding)
            if name in SPECS:
                wcore = policy_core(wtext, SPECS[name]["noun"])
                if not wcore:
                    fail(f"in-progress/{name}: change-policy core not found")
                elif cores.get(ref_name) and wcore != cores[ref_name]:
                    fail(f"in-progress/{name}: change-policy core diverges from the "
                         f"published {ref_name}")
            for m in re.finditer(r"^## (?:Appendix [A-Z]\. )?Revision History.*?(?=^## |\Z)",
                                 wtext, re.M | re.S):
                dates = re.findall(r"^- \*\*(\d{4}-\d{2}-\d{2})", m.group(0), re.M)
                if dates != sorted(dates, reverse=True):
                    fail(f"in-progress/{name}: revision history not newest-first: "
                         f"{dates}")

    if findings:
        print(f"check_spec_versioning: {len(findings)} finding(s)", file=sys.stderr)
        for f in findings:
            print("  - " + f, file=sys.stderr)
        return 1
    print("check_spec_versioning: clean")
    return 0


if __name__ == "__main__":
    sys.exit(main())
