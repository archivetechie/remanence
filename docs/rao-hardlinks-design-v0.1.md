# RAO native hardlinks + entry-type scope — design v0.1

**Status:** approved for implementation (2026-06-15, owner + claude). Codex
work order. Format-level, pre-freeze, additive. Supersedes the hardlink
*deferral* in `rao-nonregular-entries-design-v0.1.md` (hardlinks are now **in
scope**, handled natively). Decision record + rationale:
`ingest-archive-deferred-items-design-v0.1.md` item 4. Normative target:
`specs/rao-1.0-specification.md`.

## 1. Decision

RAO encodes **hardlinks natively, the way tar does** — ustar typeflag `1`. The
first occurrence of an inode is a normal regular-file entry holding the bytes
(the **primary**); every later name for that inode is a **hardlink entry**:
typeflag `1`, zero payload, whose target is the primary's **in-object path**.
This reuses the symlink machinery (zero-payload entry, `linkname`/pax
`linkpath`, manifest `entry_type`/`link_target`) added for
`rao-nonregular-entries`.

Chosen over "flatten to independent copies" because it completes the file-tree
entry set, stores the bytes once, preserves the link relationship, stays
stock-`tar`-faithful (a plain `tar` recreates the hardlink), and — because tar
hardlinks are in-archive *path references* with no common-ancestor concept —
dissolves the cross-tree-collapse edge that the old wrap-the-common-ancestor
mechanism created.

## 2. Entry-type scope (the governing principle)

RAO's native entry set is **{regular, symlink, directory, hardlink}** — "a
faithful tree of files." The boundary is **content / file-tree structure vs.
OS-runtime handle**, not "how much of tar":

- **In:** regular (data), directory (container), symlink (stored path string),
  hardlink (a second name for existing data). Meaningful on any filesystem, any
  backend, decades out.
- **Out, on principle and on safety:** character/block devices, FIFOs, sockets.
  Zero content; they are handles into a running kernel (`mknod` major/minor,
  IPC), meaningful only on a live OS, and a restore-time hazard
  (device-node/setuid extraction is a classic attack surface — RAO already
  deliberately drops ownership/setuid). The constrained subset is what buys
  RAO determinism, hostile-input safety, and re-implementability from a short
  spec; "full tar" would inherit its vendor extensions, obsolete types,
  ambiguity, and attack surface and forfeit those guarantees.

Non-content types when encountered at ingest are **skipped-and-recorded**
(default) or **blobbed** if round-trip is explicitly requested (tar-in-blob
preserves them; operator's recorded choice) — never a new native typeflag.

**Spec action:** add this as a scope/rationale statement in the published RAO
spec (§1.5 and a rationale appendix entry — the "why don't you support X" FAQ),
so the boundary is stated, not implicit.

## 3. Wire format

### 3.1 Typeflag (spec §4.3.4)

Accept typeflag `1` (LNKTYPE) in addition to `0`, `2`, `5`, `g`, `x`, NUL.
A `1` entry MUST have `size = 0` and no payload/data blocks.

### 3.2 Hardlink entry

- ustar header typeflag `1`; `size` 0.
- **Target = the primary entry's in-object path** (a canonical relative path
  per §4.6.6, naming another entry in the same object). Stored in ustar
  `linkname` when ≤ 100 bytes; otherwise pax `linkpath` with the
  `PAX_PATH_PLACEHOLDER` in `linkname` — identical mechanism to symlinks.
- **Referential integrity (new, vs symlinks):** unlike a symlink target (an
  arbitrary, possibly-dangling string), a hardlink target MUST resolve to a
  **regular-file primary entry in the same object**. Writers MUST guarantee it;
  Readers/Verifiers MUST reject a hardlink whose target is absent, is not a
  regular-file primary, or points forward to an entry that appears later than
  the link (targets MUST precede their links) — error `InvalidHardlinkTarget`
  (add to §11.1).

### 3.3 Manifest (spec §4.7.2)

The hardlink entry's `file_entries` element:
- `entry_type = "hardlink"` (the `entry_type` field already exists from
  `rao-nonregular-entries`; add `hardlink` as a permitted value).
- `link_target` = the primary's in-object path (the field already exists for
  symlinks; for hardlinks it names an in-object entry, not an arbitrary string).
- `size_bytes`, `chunk_count`, `first_chunk_lba`, `file_sha256`: **zero/absent,
  exactly like a symlink** — a hardlink entry carries no content fields of its
  own. A hardlinked name's content, hash, and PFR coordinates are its primary's,
  **resolved at read time through `link_target`** (the tar-faithful model: a tar
  hardlink record likewise stores no content). This keeps the entry internally
  consistent (no `size = 0` vs carries-the-primary's-size contradiction) and
  avoids duplicating drift-prone coordinates.

### 3.4 Byte-stability

Objects with no hardlinks are byte-identical to today. Hardlink support does
not change `schema_version` by itself (it's part of the pre-freeze non-regular
entry set, same major.minor as symlinks/dirs).

### 3.5 What does NOT change

Encrypted representation, parity, and PFR arithmetic for regular files are
untouched. A hardlink entry is a zero-payload header that points at an existing
data region; it adds no body blocks.

## 4. Determinism: primary selection

For a group of names sharing one inode, exactly one is the primary (stores
bytes); the rest are links. The rule MUST be deterministic for byte-stable
output:

1. The primary is the **first** group member in the object's entry order
   (caller-supplied order, already deterministic).
2. **If that member is excluded** by a ruleset, the first *non-excluded*
   member becomes the primary.
3. **If only one member survives** (others excluded), it is a plain regular
   entry — no hardlink entries emitted.

## 5. Reader / restore

- Reader dispatches typeflag `1` → emit a hardlink entry (target + the shared
  content coordinates).
- Restore: materialize the **primary first**, then create each link
  (`link(2)`) from the link path to the already-restored primary, with the same
  traversal-safety discipline as symlinks (no following symlinks in the
  destination tree; the target is an in-tree path by construction).
- Verifier: check referential integrity (§3.2) and that a link's shared
  `file_sha256` equals its primary's.

## 6. Ingest edges (handled in the ingest layer; see the ingest design doc)

- **Detection:** group input files by `(dev, ino)` (a `stat` per file the
  classifier already does). A group of size > 1 → primary + links.
- **Group split across a blob boundary** (one name inside a blobbed dir, one
  granular): they can't link across the boundary → the affected member falls
  back to an independent copy (stored bytes), recorded.
- Removes `collect_hardlink_roots` + its common-ancestor computation and the
  cross-tree-collapse machinery; `nlink > 1` is no longer a wrap trigger.

## 7. Spec edits (`specs/rao-1.0-specification.md`)

- §1.5 / rationale — the §2 scope principle (entry set; content-not-kernel; the
  why-not-X FAQ).
- §4.3.4 — typeflag `1` accepted; `size = 0` required.
- §4.6 — hardlink entry frame (zero payload; target in linkname/linkpath).
- §4.6.6 — hardlink target is an in-object path that MUST resolve to a primary
  (distinct from symlink targets); referential-integrity rule.
- §4.7.2 — `entry_type = hardlink`; `link_target` = primary's in-object path;
  content fields zero/absent (resolve via `link_target`, like a symlink).
- §4.9 — reader dispatch + the precede-the-link ordering requirement.
- §11.1 — `InvalidHardlinkTarget`.
- §13 — vectors (below).

## 8. Test vectors

- Two names → one inode: primary (typeflag 0 + bytes) + link (typeflag 1, no
  bytes, zero content fields, `link_target` → primary), byte-pinned.
- Long target → pax `linkpath`.
- Restore round-trip: restored tree has the two names sharing one inode
  (`st_ino` equal, `st_nlink == 2`).
- Negative: link target absent / not a primary / appearing after the link →
  `InvalidHardlinkTarget`.
- Determinism: same input twice → identical bytes (primary selection stable).
- Edge: a group with the natural primary excluded → next member is primary.

## 9. Implementation pointers (`remanence-format`)

- `tar.rs` — emit/parse typeflag `1`; reuse the symlink linkname/linkpath path.
- `model.rs` / `layout.rs` / `writer.rs` — hardlink entry kind; primary/link
  emission (link entries are zero-payload, like symlinks; no coord copy).
- `manifest.rs` — `entry_type=hardlink`, `link_target`, validation +
  referential integrity.
- `reader.rs` — dispatch + restore ordering + `link(2)` creation + verify.

## 10. DoD

`cargo fmt`/`clippy -D warnings` clean; the §8 vectors exist and pass;
regular-only objects byte-identical (existing vectors green);
`rao-nonregular-entries` doc updated to mark hardlinks in-scope.
