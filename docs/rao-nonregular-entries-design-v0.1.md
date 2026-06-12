# RAO non-regular entries (symlinks + empty directories) — v0.1

**Status:** decision approved (2026-06-11, owner); spec edit + implementation
**deferred**. This note is the driving record for a later update to
`rao-1.0-specification.md`, the RAO work order
(`design-rem-archive-object-format.md`), and `remanence-format`.

## 1. Decision

RAO 1.0 will encode **symlinks and directories natively, using standard
ustar typeflags** (`2` = symbolic link, `5` = directory), rather than the
manifest-annotation alternative. This expands the current "regular files
only" scope (spec §1.5) **before freeze**.

Hardlinks remain out of scope (they need inode-identity/refcount semantics);
this note covers symlinks and directories only. "Non-regular entries" is the
collective term so the surface can extend later.

## 2. Why, and why now

- **Standard-tool fidelity is the point.** Native ustar typeflags mean a
  stock GNU `tar` / bsdtar recreates symlinks and directories *correctly* on
  restore — preserving the longevity net (spec §4.10) that justifies the
  plaintext representation. The annotation alternative (a symlink stored as a
  regular file containing its target) would make stock `tar` restore a
  regular file instead of a link, breaking fidelity on exactly the fallback
  path that matters.
- **Common in the workflow.** Empty directories (folder-structure
  conventions) and symlinks (project-folder references, both resolving and
  dangling) are routine in media trees and in our pipeline.
- **The window is now.** Spec §10 makes *new entry kinds with payload
  semantics* a new-`format_id` (RAO v2) change **once 1.0 is frozen**. RAO
  1.0 is still **draft**, and neither it nor any predecessor was ever
  published or deployed ([[project-rao-no-predecessor-compat]]), so there is
  no deployed 1.0 reader to break: folding symlinks/dirs in now is a
  pre-freeze scope expansion, not a version break. After freeze it would cost
  a major version. **This must land before the RAO freeze.**

**Dangling symlinks are free.** A symlink stores only its target string;
whether the target resolves is a restore-time fact, not a format property.
Storing symlinks at all covers dangling ones with no special case.

## 3. Byte-stability constraint (non-negotiable)

All additions MUST be **absent on regular-file entries**, so that an object
containing only regular files is byte-identical to today's and its
`plaintext_digest` is unchanged. Concretely: a regular-file manifest entry
emits no new keys; the new typeflags and new manifest fields appear only on
symlink/directory entries. This keeps every existing regular-file test vector
green and preserves the shared-identity guarantees.

## 4. Wire-format changes (plaintext representation)

### 4.1 Typeflags (spec §4.3.4)
Add to the accepted set:

| Typeflag | Meaning |
| --- | --- |
| `2` (0x32) | Symbolic link |
| `5` (0x35) | Directory |

Readers accept `g`, `x`, `0`, NUL, **`2`, `5`**; all others still rejected
(`UnsupportedTarTypeflag`).

### 4.2 Symlink entry
- ustar header typeflag `2`; `size` = 0; **no payload, no data blocks.**
- Target stored in the ustar `linkname` field (offset 157, 100 bytes) when
  ≤ 100 bytes; otherwise via the pax **`linkpath`** keyword (the link-target
  analog of `path`; add to §4.4.4). `linkname` carries a placeholder when
  `linkpath` is used, mirroring the existing `PAX_PATH_PLACEHOLDER` pattern.
- Manifest: `size_bytes` = 0, `chunk_count` = 0, `first_chunk_lba` = null.

### 4.3 Directory entry
- ustar header typeflag `5`; `size` = 0; no payload.
- **Only directories that cannot exist implicitly need an entry** — i.e.
  empty directories. Populated directories are implied by their children's
  paths and need no entry (no change there). Emitting entries for *all*
  directories (to carry directory mode/mtime) is a sub-decision (§9).

### 4.4 Link targets are NOT RAO paths
A symlink target is an opaque OS string: it MAY be absolute, MAY contain
`..`, MAY be non-canonical. The §4.6.6 canonical-relative-path rules apply to
an entry's **own path**, never to a link **target**. Target validation is
limited to the pax value grammar (UTF-8, no NUL/control per §4.4.1). The
entry's own path still obeys all §4.6.6 rules.

## 5. Alignment refinement for zero-payload entries (spec §4.6.3)

Today §4.6.3 chunk-aligns *every* entry's payload start, including
zero-length ones (writer-side; readers only enforce it for `size > 0`). With
many tiny entries that has a real cost: each empty dir / symlink would burn
up to one `chunk_size` (256 KiB) of alignment padding. A tree with hundreds
of empty dirs could waste hundreds of MB.

**Recommended refinement:** exempt **zero-payload entries** (`size = 0` —
symlinks, empty dirs, and empty regular files) from the chunk-alignment
invariant. They use plain 512-byte tar-record alignment, carry
`first_chunk_lba = null` / `chunk_count = 0`, and consume no chunk padding.
The invariant remains exactly as-is for `size > 0` entries, so PFR arithmetic
and the layout of every non-empty file are unchanged.

Caveat: this changes the byte layout of objects containing **empty regular
files** (the §13.1 "empty file" vector regenerates). Acceptable pre-freeze;
flag it. Confirm against the `layout.rs` planner.

## 6. Manifest changes (spec §4.7.2)

Add two **optional, absent-by-default** keys to each `file_entries` element:

- `entry_type` — `regular` (or absent ⇒ regular), `symlink`, `directory`.
  Absent on regular entries (§3). Encoding (text vs small-int) is a
  sub-decision (§9); whichever is chosen, deterministic-CBOR key ordering
  (§4.7.1) must be recomputed.
- `link_target` — text; present **iff** `entry_type = symlink`; the symlink
  target string.

For symlink/dir entries: `size_bytes` = 0, `chunk_count` = 0,
`first_chunk_lba` = null. `file_sha256` handling is a sub-decision (§9).
The "exactly eight keys" wording in §4.7.2 relaxes to "the eight base keys,
plus `entry_type`/`link_target` where applicable."

## 7. Reader, restore, and security

- **Reader dispatch (§4.9):** handle typeflags `2` and `5` — emit
  symlink/directory entries to the consumer; no payload bytes to stream.
- **Path traversal (§12.10) — material expansion, call it out.** A restored
  symlink can point anywhere (absolute, `../../…`), and a *later* entry
  restored "through" a previously-extracted symlink can escape the
  destination tree (a classic tar extraction CVE class). Restore MUST: never
  follow symlinks in the destination tree while materializing entries
  (`O_NOFOLLOW`/`openat` discipline, re-checking each component), and create
  symlinks without dereferencing. The framing layer accepting a path is not a
  safety claim — this was already true (§12.10) and is now load-bearing.
- **Standard-tool extraction (§4.10):** stock `tar` recreates the symlink/dir
  faithfully — including a dangling or absolute symlink, which is the correct
  faithful behavior. The traversal caution applies to stock `tar` too (its
  own protections).

## 8. What does NOT change

- **Encrypted representation (spec §5): no change.** The envelope wraps the
  canonical plaintext stream byte-for-byte regardless of entry types;
  symlinks and dirs ride inside the encrypted payload like any other bytes.
  State this explicitly so the blast radius is bounded to the plaintext
  representation + manifest.
- **Parity layer (REM-PARITY): no change.** Objects remain opaque block
  strings to it.
- **PFR arithmetic for regular files: unchanged** (zero-payload entries have
  no body to address).

## 9. Sub-decisions to resolve at spec-edit time

1. **Directory path spelling.** Stock tar writes directory entries with a
   trailing `/` in `name`; RAO §4.6.6 forbids trailing `/` on paths. Decide:
   store dir paths with the tar-conventional trailing slash (best stock-tar
   interop) and carve a §4.6.6 exception for typeflag-`5` entries, vs. rely on
   typeflag `5` alone. *Lean: trailing-slash + typeflag, matching what GNU tar
   emits/expects.*
2. **`file_sha256` for non-regular entries.** Options: absent for both;
   absent for dirs + SHA-256 of the target string for symlinks; absent for
   both with integrity coming from the manifest/`plaintext_digest`. *Lean:
   absent for both* (symlink target integrity is covered by the manifest and
   the whole-object digest; a content hash of a metadata string adds little).
   Requires making `file_sha256` optional in the schema for non-regular
   entries.
3. **Empty-only vs all directories.** Emit dir entries only for empty dirs
   (minimum to avoid silent loss), vs. all dirs (to preserve directory
   mode/mtime). *Lean: empty-only for v1*; all-dirs is a later
   metadata-preservation-tier concern.
4. **Zero-payload alignment exemption (§5).** Confirm the refinement and
   regenerate the empty-file vector.
5. **`entry_type` encoding** (text vs small unsigned) and resulting CBOR key
   sort order.

## 10. Change checklist — `rao-1.0-specification.md`

- §1.5 — drop "regular files only"; state symlinks + directories are encoded;
  hardlinks remain reserved.
- §2.3 — extend the "Entry" definition.
- §4.3.4 — add typeflags `2`, `5`.
- §4.4.4 — add the `linkpath` standard keyword.
- §4.6.1 — entry frame for zero-payload entries.
- §4.6.3 — zero-payload alignment refinement (§5 here).
- §4.6.4 — chunk geometry: `chunk_count`/`first_chunk_lba` for non-regular.
- §4.6.6 — path rules for dir entries; explicit "targets are not paths."
- §4.7.2 — `entry_type` + `link_target`; relax "exactly eight keys";
  `file_sha256` optionality.
- §4.9 — reader dispatch for `2`/`5`.
- §4.10 — note stock-tar symlink/dir restore + traversal caution.
- §12.10 — expand the symlink traversal analysis.
- §13 — new vectors: empty dir; short-target symlink (linkname); long-target
  symlink (pax `linkpath`); dangling symlink; mixed object. Negatives: a
  symlink/dir entry with `size > 0`; a symlink whose restore would traverse
  out of tree. Regenerate the empty-file vector (§5).
- §10 — note the pre-freeze scope expansion explicitly (so the rationale
  survives): why this was 1.0 and not v2.

## 11. Implementation impact (later, with codex)

- `remanence-format`: `model.rs` (`RemTarFileSpec` gains entry-type +
  link-target), `layout.rs`/`writer.rs` (emit typeflag `2`/`5`,
  linkname/`linkpath`, manifest fields, zero-payload alignment),
  `reader.rs` (typeflag dispatch + safe restore), `pax.rs` (`linkpath`).
- **Ingest layer** (`rem archive build` / sutradhara): walk with `lstat`
  (don't follow symlinks during the walk), detect empty dirs, build the right
  specs. This is also where the §12.10 restore-safety discipline lives.
- **Coordinate with the amber merge** (Amendment 2 of the work order, in
  flight): that change reworks the same writer/reader entry paths for
  encryption. Sequence so they don't collide — ideally land non-regular
  entries after the amber merge settles, or on a shared branch with explicit
  ordering.
- Add a task to `design-rem-archive-object-format.md` (or supersede §1.5's
  "regular files only" line there) once the spec edit lands.

## 12. Still open / separate threads

- **Mac extended-attribute metadata** (Finder color tags =
  `com.apple.metadata:_kMDItemUserTags` / legacy `com.apple.FinderInfo`
  xattrs; surfacing as `._` AppleDouble sidecars off Mac-native filesystems)
  is a **separate decision**, not folded in here. It is an xattr-capture
  problem, not an entry-type one. Decide next: capture xattrs into the
  reserved `metadata_preservation_data` manifest container, treat incoming
  `._`/`.DS_Store` files as ordinary payload (and don't let an ingest filter
  delete them), or rely on the wrap-then-store fallback.
- **Wrap-then-store** remains the zero-format-work, zero-loss fallback for
  *any* non-conforming subtree: a faithful inner `tar --xattrs` stored as one
  RAO payload captures dirs, symlinks, and xattrs together, trading away
  per-inner-file PFR and hashing. It stays available regardless of this
  change.
