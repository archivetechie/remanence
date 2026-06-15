# Rem Archive Object (RAO) Format — Version 1.1

## Specification (additive minor over RAO 1.0)

| | |
| --- | --- |
| Status | Draft for review |
| Version | 1.1 (minor; additive over 1.0) |
| Date | 2026-06-15 |
| Base | `rao-1.0-specification.md` (normative for everything not restated here) |
| Stream version | `REMANENCE.schema_version = 1.1` (only when 1.1 metadata is present) |
| Adds | per-entry extended-attribute (xattr) metadata preservation |
| Design record | `docs/rao-1.1-metadata-preservation-design-v0.1.md` |

> **Editorial note (remove at freeze):** a normative additive delta on RAO 1.0;
> it specifies only what 1.1 adds. BCP 14 language and all conventions are
> inherited from RAO 1.0 §2. (Native hardlinks and the entry-type scope
> statement are part of **RAO 1.0** — completing its file-tree entry set — not
> 1.1; see RAO 1.0 §4.3.4, §4.6, §4.7.2.)

## 1. Relationship to 1.0 and version gating

RAO 1.1 adds one thing: preservation of POSIX **extended attributes** per
entry, in the reserved `metadata_preservation_data` container that RAO 1.0
§4.7.2 already defines (empty in 1.0). It is **purely additive and fully
backward-compatible**:

- An object with no preserved xattrs is a 1.0 object, **byte-identical**
  (`REMANENCE.schema_version = 1.0`, empty `metadata_preservation_data`). Every
  RAO 1.0 test vector and `plaintext_digest` is unchanged.
- An object that preserves any xattr sets `REMANENCE.schema_version = 1.1`. The
  manifest CBOR `schema_version` integer stays **1** — per RAO 1.0 §4.7.2 the
  reserved containers are the designated 1.x extension surface, and filling one
  does not bump the integer.
- **A 1.0 reader reads a 1.1 object correctly.** It gates on the *major*
  version (RAO 1.0 §4.5.2), then ignores unknown content in
  `metadata_preservation_data` (RAO 1.0 §4.7.2 obligation 3). It recovers every
  file byte-for-byte; it simply does not reapply the preserved xattrs. No
  misinterpretation, no rejection — which is exactly why this is a minor and
  not a new `format_id` (RAO 1.0 §10).

## 2. Extended-attribute preservation

### 2.1 Wire format

An entry's `metadata_preservation_data` MAY be a CBOR map (manifest profile,
RAO 1.0 §4.7.1) containing:

```text
"xattrs" : { <name> : <value>, ... }
```

- `<name>` — text key. xattr names are ASCII/UTF-8 in practice; a non-UTF-8
  name uses the reversible escaping defined for ingest member names.
- `<value>` — a CBOR **byte string** holding the raw attribute bytes. The
  manifest is CBOR (not JSON), so raw bytes are stored directly — **no base64**.
- **Deterministic encoding** (RAO 1.0 §4.7.1): the `xattrs` map's keys MUST be
  in canonical sort order, shortest-form lengths, no duplicate names.
- The container is keyed (`"xattrs"`) so a later minor can add other
  preserved-metadata types without restructuring; readers MUST ignore unknown
  keys within it.
- An entry with no preserved xattrs MUST emit an **empty**
  `metadata_preservation_data` (preserving 1.0 byte-stability). A non-empty
  container is what marks a 1.1 object.

**Scope is xattrs only.** Ownership stays unpreserved (RAO 1.0 §4.3.1), `mtime`
is already covered (RAO 1.0 pax `mtime`, fractional seconds), and
mode-beyond-`executable` and ACLs are out of scope for 1.1.

### 2.2 Policy and size (informative; the rule is ingest-side)

*Which* xattrs are preserved is an ingest/orchestration policy
(`docs/ingest-archive-deferred-items-design-v0.1.md`): a built-in junk baseline
is always dropped; the rest is governed by a ruleset-selected denylist (keep
all but baseline) or allowlist (keep only listed) stance, fail-safe default
denylist; all drops are recorded. The format stores whatever surviving xattrs
it is handed. An xattr too large for the compact manifest (the resource fork is
the only routinely large case) causes the **file to be wrapped** rather than
annotated — a storage/ingest decision, not a format rule.

### 2.3 Read and restore

A 1.1 reader surfaces an entry's preserved xattrs and, on restore, reapplies
them (`setxattr`). A 1.0 reader ignores them (§1) and restores the file
without them.

## 3. Test vectors (additions to RAO 1.0 §13)

- **xattr round-trip:** a regular entry with a small xattr (e.g. a Finder color
  tag) → non-empty `metadata_preservation_data.xattrs`,
  `REMANENCE.schema_version 1.1`, byte-pinned; restore reapplies it byte-exact.
- **Byte-stability:** the same inputs with no xattrs → `schema_version 1.0`,
  byte-identical to the corresponding RAO 1.0 vector.
- **1.0-reader compatibility:** a 1.0 reader reads the xattr-bearing 1.1 object,
  recovering all files byte-exact, ignoring the preserved xattrs.
- **Determinism:** two builds of the same input + xattrs → identical bytes
  (canonical `xattrs` map ordering).

## 4. Conformance

A RAO 1.1 implementation implements all of RAO 1.0 plus §2 here. Freeze
criteria mirror RAO 1.0 §14 for the added surface: the §3 vectors exist and
pass, including the 1.0-byte-stability and 1.0-reader-compatibility vectors.
RAO 1.1 freezes no earlier than RAO 1.0.
