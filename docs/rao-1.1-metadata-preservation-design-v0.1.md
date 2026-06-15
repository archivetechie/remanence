# RAO 1.1 — metadata preservation (xattrs) — design v0.1

**Status:** design approved in discussion (2026-06-15, owner + claude);
spec edit + implementation **pending** (codex). A format-level work item —
distinct from the CLI ingest fixups, which held "no wire-format change." This
is additive and 1.0-reader-compatible by construction. Refines
`specs/rao-1.0-specification.md`; companion to
`ingest-archive-deferred-items-design-v0.1.md` (the xattr policy that drives
it) and `rao-nonregular-entries-design-v0.1.md` (the other pre-freeze
manifest expansion).

## 1. Why, and why now

The ingest layer needs to preserve *meaningful* file metadata — chiefly macOS
Finder color tags (`com.apple.metadata:_kMDItemUserTags`) — without forcing
those files into wrappers. RAO 1.0 native entries have nowhere to put an
xattr, so the only way to keep one was to wrap the file in a `.remwrap.tar`.
On Mac sources that over-wraps massively (macOS sprinkles xattrs like
`com.apple.quarantine` on nearly everything) and, under `expect=compliant`,
spuriously halts bundles. 1.1 gives native entries a metadata slot, so a
meaningful xattr rides on a clean native entry and wrapping is reserved for
things that genuinely can't be native (non-UTF-8 names, device nodes,
hardlinks, oversized metadata).

**Now is the right time:** RAO 1.0 is not frozen; the manifest already
*reserves* `metadata_preservation_data` (per-entry) and `object_metadata`
(top-level) as empty-in-1.0 containers; and 1.0 readers already MUST tolerate
them non-empty (spec §4.7.2 obligation 3 / §10 — the designated 1.x extension
surface). Doing it now also exercises the additive 1.x versioning mechanism
with a real second version before freeze, validating that design.

## 2. Scope

**In:** per-entry **xattrs** in `metadata_preservation_data`.

**Out (deliberately):**
- *Ownership* (uid/gid/names) — 1.0 fixes these to constants on purpose
  (a root-run `tar` can't apply ownership the format never recorded); don't
  reverse it.
- *mtime* — already supported in 1.0 as the optional pax `mtime` keyword, with
  fractional-second precision. Nothing to add.
- *Mode beyond the existing `executable` bit*, *ACLs* — low value for media,
  platform-tangled. Defer to a later minor if a concrete need appears.

The container is shaped as a keyed map so those future types slot in later
without restructuring.

## 3. Wire format

### 3.1 Manifest entry change (spec §4.7.2)

Today each `file_entries` element's `metadata_preservation_data` MUST be empty.
1.1 allows it to carry:

```text
metadata_preservation_data = {
    "xattrs": {            # text key
        <name> : <value>,  # name: text (xattr names are ASCII/UTF-8 in practice)
        ...                # value: CBOR byte string (raw bytes, any content)
    }
}
```

- The manifest is **CBOR** (manifest profile), which has native byte strings —
  so xattr **values** are stored as raw bytes directly; **no base64** (the
  item-1 JSON constraint does not recur here). xattr **names** are text keys.
  Should a name ever not be valid UTF-8, encode it with the item-1 reversible
  escaping; in practice xattr names are ASCII.
- **Deterministic encoding** as everywhere in the manifest: the `xattrs` map's
  keys sorted by the §4.7.1 canonical rule, shortest-form integers/lengths,
  no duplicate names. Required so the manifest stays a reproducible projection.
- Unknown keys inside `metadata_preservation_data` (e.g. a future `"acl"`) are
  ignored by a 1.1 reader, same 1.x tolerance.

### 3.2 schema_version

The writer emits `REMANENCE.schema_version = 1.1` **iff** it actually writes
non-empty preservation data; otherwise it stays `1.0` and the object is
**byte-identical** to today. This preserves `plaintext_digest` and every
existing 1.0 vector for objects that carry no preserved metadata. A 1.0 reader
accepts 1.1 because it gates on the major version only.

### 3.3 What does NOT change

The encrypted representation (§5), the parity layer, and PFR are untouched —
this is a manifest-content addition. Objects without preserved metadata are
bit-for-bit unchanged.

## 4. Policy: which xattrs, and size

(The full policy lives in `ingest-archive-deferred-items-design-v0.1.md`;
summarized here for the format's obligations.)

- **Denylist** of ephemeral xattrs (`com.apple.quarantine`,
  `com.apple.metadata:kMDItemWhereFroms`, `com.apple.lastuseddate#PS`,
  Spotlight/FinderInfo noise — tunable) → dropped, never stored.
- **Non-denylisted xattrs** → stored in the annotation when **small**; when an
  xattr exceeds the size threshold the file **wraps** instead (the bulk goes
  into the tar, which is built for it). Threshold: **~4 KiB per xattr**, plus a
  modest **per-file total cap** (so many small xattrs can't bloat the
  manifest). Tunable; not normative wire.
- The only routinely-large xattr is the resource fork
  (`com.apple.ResourceFork`) — a second data stream, not metadata — and it is
  near-absent in modern media, so the wrap path is a rare safety net.

## 5. Read / restore

- Reader surfaces `metadata_preservation_data.xattrs` per entry.
- Restore re-applies them with `setxattr` (the `xattr` crate), so a native
  entry round-trips its xattrs. (If the restore target is a non-Mac filesystem
  and the consumer is macOS, macOS re-externalizes to `._` sidecars on its own
  — no work for us.)
- xattr **detection** at ingest is via the `xattr` syscall crate, not a
  `getfattr` subprocess (shared with the item-2 cheap-scan decision; removes
  the external `attr` dependency).

## 6. Determinism & byte-stability obligations

1. An object with no preserved metadata is byte-identical to a 1.0 object
   (schema_version 1.0, empty container) — existing vectors unaffected.
2. Two writers given the same files + same xattrs produce the identical
   manifest bytes (sorted xattr keys, canonical CBOR).
3. Round-trip: ingest → restore reproduces the file's xattr set exactly
   (modulo the dropped denylist).

## 7. Spec edits required (`specs/rao-1.0-specification.md`)

- §4.7.2 — `metadata_preservation_data` may carry the `xattrs` map; define its
  shape and the byte-string value rule; relax the "MUST be empty" to "empty in
  1.0; carries the §X structure in 1.1+".
- §4.5.1 / schema_version — document 1.1 and the emit-1.1-only-when-non-empty
  rule.
- §10 — note 1.1 as the first additive minor and what it adds.
- §13 — vectors (below).

## 8. Test vectors

- An object with a small xattr (color tag) → annotation present,
  schema_version 1.1, byte-pinned.
- The same files with **no** xattrs → schema_version 1.0, byte-identical to the
  existing vector (proves byte-stability).
- A denylisted xattr present → dropped, object stays 1.0.
- An oversized xattr → file wraps (not annotated).
- Restore round-trip: xattrs reapplied byte-exact via setxattr.
- Determinism: two builds of the same input → identical manifest bytes.

## 9. Implementation pointers (`remanence-format`)

- `manifest.rs` — encode/validate the `xattrs` map in
  `metadata_preservation_data`; sorted-key canonical CBOR; size checks.
- `model.rs` / `layout.rs` / `writer.rs` — carry per-entry xattrs from the spec
  into the manifest; schema_version bump logic.
- `reader.rs` — surface preserved xattrs; restore-time `setxattr`.
- Ingest (`remanence-cli/archive_ingest.rs`) — collect xattrs via the `xattr`
  crate, apply the denylist + size threshold, hand small ones to the format as
  annotation and route oversized ones to the wrap path.

## 10. Cross-repo

- **sutradhara** — optional, recorded, staging-time AppleDouble `._`→native
  xattr normalization so rem uniformly sees native xattrs (Case A) regardless
  of transport; rem's contract is unchanged either way. Specify in the
  sutradhara design.
- **Customer manifest** — may surface preserved-metadata presence; coordinate
  when finalized.

## 11. Open sub-decision

Denylist (preserve every non-junk xattr) vs. allowlist (keep only
known-meaningful, drop the rest). Lean **denylist** (don't-lose-data), drop
list tunable. Final call before the spec edit.
