# Rem Archive Object (RAO) Format — Version 1.1

## Specification (additive minor over RAO 1.0)

| | |
| --- | --- |
| Status | Draft for review |
| Version | 1.1 (minor; additive over 1.0) |
| Date | 2026-06-15 |
| Base | `rao-1.0-specification.md` (normative for everything not restated here) |
| Stream version | `REMANENCE.schema_version = 1.1` (only when a 1.1 feature is used) |
| Adds | native hardlink entries (ustar typeflag `1`); per-entry xattr metadata preservation; the entry-type scope statement |
| Design records | `docs/rao-hardlinks-design-v0.1.md`, `docs/rao-1.1-metadata-preservation-design-v0.1.md` |

> **Editorial note (remove at freeze):** this is a normative additive delta on
> RAO 1.0 — it specifies only what 1.1 adds; 1.0 governs everything else. At
> freeze it MAY be consolidated into a single standalone document. The
> requirements language (BCP 14) and all conventions are inherited from
> RAO 1.0 §2.

## 1. Relationship to 1.0 and version gating

RAO 1.1 is the **full-filesystem-fidelity** minor: 1.0 is the core archive
(regular, symlink, directory entries; reserved metadata containers empty), and
1.1 adds the two things a faithful backup of a real tree also needs —
**hardlinks** and **preserved extended attributes**.

1.1 is **additive and backward-tolerant**:

- An object that uses **no** 1.1 feature is a 1.0 object, **byte-identical**
  (`REMANENCE.schema_version = 1.0`, no hardlink entries, empty
  `metadata_preservation_data`). All RAO 1.0 test vectors and `plaintext_digest`
  values are unchanged.
- An object that uses a 1.1 feature sets `REMANENCE.schema_version = 1.1`. The
  manifest CBOR `schema_version` integer stays **1** (per 1.0 §4.7.2 the
  reserved containers and optional `entry_type` are the designated 1.x
  extension surface; filling them does not bump the integer).

**1.0-reader behavior on a 1.1 object** (both correct, both fail-safe — this is
why 1.1 is a minor and not a new `format_id`, consistent with 1.0 §10):

- *xattr metadata* — a 1.0 reader gates on the **major** version, then ignores
  unknown content in `metadata_preservation_data` (1.0 §4.7.2 obligation 3). It
  reads the object's files **correctly**, simply without the preserved xattrs.
  Fully backward-compatible; no misinterpretation.
- *hardlink entries* — a 1.0 reader rejects ustar typeflag `1` with
  `UnsupportedTarTypeflag` (1.0 §4.3.4). It **fail-closes** on the hardlink
  entry rather than misinterpreting it. A 1.1 object containing hardlinks is
  therefore not a valid *1.0* object, which is the normal semantics of a minor
  that adds a feature; §10's reserve-for-`format_id` rule targets *silent
  misinterpretation*, which does not occur here (zero-payload entry, clean
  rejection).

> **Open structural alternative (reviewer's call):** hardlinks could instead be
> folded into 1.0 itself pre-freeze, exactly as symlinks/directories were —
> making the 1.0 entry set regular/symlink/dir/hardlink and leaving 1.1 to be
> *only* xattr preservation. That is the more symmetric entry-set story; this
> document keeps 1.0 stable and groups hardlinks with xattrs as "1.1 fidelity"
> per the 2026-06-15 decision to keep the published 1.0 untouched. Flip by
> moving §2–§3 into 1.0 if preferred.

## 2. Entry-type scope (normative scope statement)

RAO's native entry set is exactly **{regular, symlink, directory, hardlink}** —
a faithful tree of files. The governing boundary is **content / file-tree
structure vs. OS-runtime handle**:

- **In scope:** regular files (data), directories (containers), symlinks (a
  stored path string), hardlinks (a second name for existing data). All four
  are meaningful on any filesystem, any backend, decades on. (Regular, symlink,
  directory are 1.0; hardlink is added here.)
- **Out of scope, normatively, on principle and on safety:** character devices,
  block devices, FIFOs, and sockets. They carry no content — they are handles
  into a running kernel — and materializing them on restore is a hazard
  (device-node/setuid extraction is a known attack surface; RAO already drops
  ownership and setuid, 1.0 §4.3.1). A conformant writer MUST NOT emit them as
  native entries; a conformant reader MUST reject any such ustar typeflag
  (`3`, `4`, `6`) with `UnsupportedTarTypeflag`. Tools that must round-trip a
  tree containing them do so out of band (e.g. wrapped inside a regular-file
  payload), never as a native RAO entry.

This statement SHOULD also be reflected in the 1.0 rationale (the
"why not full tar / why not device nodes" question).

## 3. Native hardlink entries (ustar typeflag `1`)

A hardlink entry records that a path is a second name for the bytes of another
entry (the **primary**) in the same object. It parallels the symlink machinery
of 1.0 §4.6, with one added obligation: **referential integrity**.

### 3.1 Wire format

- **Typeflag.** Accept typeflag `1` (`LNKTYPE`) in addition to the 1.0 set
  (`g`, `x`, `0`, `2`, `5`, NUL). A `1` entry MUST have `size = 0` and no
  payload / data blocks (as for symlinks and directories).
- **ustar `mode`.** `0000644\0` (a hardlink shares its primary's content; the
  primary carries the executable bit per 1.0 §4.3.1).
- **Target.** The primary's **in-object path** — a canonical relative path
  (1.0 §4.6.6) naming another entry in the same object. Stored in ustar
  `linkname` when ≤ 100 bytes; otherwise in pax `linkpath` with
  `PAX_PATH_PLACEHOLDER` in `linkname` (identical to symlink targets, 1.0
  §4.6.1) — **but** unlike a symlink target it is not an arbitrary string
  (§3.3).

### 3.2 Manifest

The hardlink entry's `file_entries` element (extending 1.0 §4.7.2):

- `entry_type` = `hardlink` (a new permitted value alongside `symlink` /
  `directory`).
- `link_target` = the primary's in-object path.
- `size_bytes`, `chunk_count`, `first_chunk_lba`, `file_sha256` — **carry the
  primary's values.** A hardlink has content (it shares the primary's), so PFR
  on a hardlinked name reads the primary's blocks directly with no link
  resolution, and the catalog sees identical content (same `file_sha256`). This
  is the sole deviation from symlinks, whose content fields are zero/`null`.

### 3.3 Referential integrity (normative)

Distinct from symlink targets (which 1.0 §4.6.6 allows to be absolute,
`..`-bearing, or dangling), a **hardlink target MUST resolve, within the same
object, to a regular-file primary entry that appears before the hardlink
entry.** Writers MUST guarantee this. Readers and Verifiers MUST reject a
hardlink whose target is absent, is not a regular-file primary, or appears at
or after the hardlink entry, with `InvalidHardlinkTarget` (§5). A Verifier MUST
also confirm the hardlink's `file_sha256` equals its primary's.

### 3.4 Deterministic primary selection

When several entries would name one underlying file, exactly one is the primary
(stores the bytes) and the rest are hardlink entries. For byte-stable output:

1. The primary is the **first** such entry in the object's entry order
   (caller-supplied order; deterministic).
2. If that entry is omitted (e.g. excluded by an ingest ruleset), the first
   surviving entry becomes the primary.
3. If only one survives, it is a plain regular entry (no hardlink entries).

### 3.5 Reader and restore

- Reader dispatch (extending 1.0 §4.9): typeflag `1` → require `size = 0`,
  compute the effective path and the in-object target, and deliver a hardlink
  entry with no payload, after the §3.3 checks.
- Restore MUST materialize the **primary before** its hardlinks, then create
  each hardlink (`link(2)`) from the link path to the already-restored primary,
  under the same destination-tree traversal safety 1.0 mandates for symlinks
  (1.0 §12.10 / restore section): never follow symlinks in the destination tree
  while materializing; the target is an in-tree path by construction.

### 3.6 Standard-tool extraction

A plaintext 1.1 object's hardlink entries are standard ustar typeflag-`1`
records, so commodity `tar`/bsdtar recreates the hardlinks faithfully — the
1.0 longevity property (1.0 §4.10) extends to hardlinks.

## 4. Metadata preservation: extended attributes

1.1 fills the reserved per-entry `metadata_preservation_data` container (1.0
§4.7.2) with preserved POSIX extended attributes. **Scope is xattrs only** —
ownership remains deliberately unpreserved (1.0 §4.3.1), `mtime` is already
covered (1.0 pax `mtime`, fractional seconds), and mode-beyond-`executable`,
ACLs, etc. are out of scope for 1.1.

### 4.1 Wire format

`metadata_preservation_data` of an entry MAY be a CBOR map (manifest profile,
1.0 §4.7.1) containing:

```text
"xattrs" : { <name> : <value>, ... }
```

- `<name>`: text key (xattr names are ASCII/UTF-8 in practice; a non-UTF-8
  name uses the reversible escaping of the ingest member-name rule).
- `<value>`: a CBOR **byte string** holding the raw attribute bytes. Because
  the manifest is CBOR (not JSON), raw bytes are stored directly; **no base64**.
- Deterministic encoding (1.0 §4.7.1): the `xattrs` map's keys MUST be in the
  canonical sort order, shortest-form lengths, no duplicate names.
- The container is keyed (`"xattrs"`) so future preserved-metadata types
  (a later minor) slot in without restructuring. Readers ignore unknown keys
  within it.

An entry with no preserved xattrs MUST emit an **empty**
`metadata_preservation_data` (preserving 1.0 byte-stability) — i.e. a non-empty
container is what marks a 1.1 object.

### 4.2 Policy and size (informative; the rule is ingest-side)

*Which* xattrs are preserved is an ingest/orchestration policy
(`docs/ingest-archive-deferred-items-design-v0.1.md`): a built-in junk baseline
is always dropped; the rest is governed by a ruleset-selected denylist (keep
all but baseline) or allowlist (keep only listed) stance, fail-safe default
denylist; all drops are recorded. The format simply stores whatever surviving
xattrs it is given. An xattr too large to belong in the compact manifest
(threshold an ingest concern; the resource fork is the only routinely large
case) causes the **file to be wrapped** rather than annotated — a
storage/ingest decision, not a format rule.

### 4.3 Read / restore

A 1.1 reader surfaces an entry's preserved xattrs and, on restore, reapplies
them with `setxattr`. A 1.0 reader ignores them (§1) and restores the file
without them.

## 5. Errors (additions to 1.0 §11.1)

```text
InvalidHardlinkTarget   a hardlink entry's link_target is absent in the object,
                        is not a regular-file primary, appears at/after the
                        hardlink entry, or its file_sha256 differs from the
                        primary's (Section 3.3)
```

All other error names are inherited from 1.0 §11.

## 6. Test vectors (additions to 1.0 §13)

- **Hardlink round-trip:** two entries, one inode — primary (typeflag `0` +
  bytes) + hardlink (typeflag `1`, no bytes, `entry_type=hardlink`, manifest
  shows the shared `file_sha256`/coords); byte-pinned; restore yields two names
  sharing one inode (`st_ino` equal, `st_nlink == 2`).
- **Long hardlink target** → pax `linkpath`.
- **xattr round-trip:** a regular entry with a small xattr (e.g. a Finder color
  tag) → non-empty `metadata_preservation_data.xattrs`, `schema_version 1.1`,
  byte-pinned; restore reapplies it byte-exact.
- **Byte-stability:** the same inputs with no hardlinks and no xattrs →
  `schema_version 1.0`, byte-identical to the corresponding 1.0 vector.
- **Determinism:** two builds of a hardlinked input → identical bytes
  (primary selection stable).
- **Negatives:** hardlink target absent / not a primary / appearing after the
  link → `InvalidHardlinkTarget`; an excluded natural primary → next entry is
  the primary.
- **1.0-reader compatibility:** a 1.0 reader reads a 1.1 object that uses only
  xattr preservation (ignoring the xattrs); a 1.0 reader rejects a hardlink
  entry with `UnsupportedTarTypeflag`.

## 7. Conformance

A RAO 1.1 implementation implements all of RAO 1.0 plus §2–§4 here. Freeze
criteria mirror 1.0 §14 for the added surface: the §6 vectors exist and pass
(including the 1.0-byte-stability and 1.0-reader-compatibility vectors), and
the standard-tool extraction gate (1.0 §4.10) is demonstrated to recreate
hardlinks. RAO 1.1 freezes no earlier than RAO 1.0.
