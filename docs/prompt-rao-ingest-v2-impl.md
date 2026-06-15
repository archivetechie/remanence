# Codex prompt — RAO ingest-v2 + format completion (implementation)

> **Repo: `~/remanence`.** Read `CLAUDE.md` + `AGENTS.md` first. Implement the
> work below **in the given order**. Each phase has a dedicated design doc (its
> work order) and the normative spec it conforms to. **Specs are normative:**
> implement *to* them; if you find a spec gap or ambiguity, **flag it** — do not
> silently weaken the spec to match the code. Full rationale for every decision:
> `docs/ingest-archive-deferred-items-design-v0.1.md`.

## Authoritative sources

- **Specs (normative):** `specs/rao-1.0-specification.md` (native hardlinks are
  now part of 1.0), `specs/rao-1.1-specification.md` (xattr metadata
  preservation).
- **Work orders (per phase):** `docs/rao-hardlinks-design-v0.1.md`,
  `docs/rao-1.1-metadata-preservation-design-v0.1.md`,
  `docs/ingest-layer-implementation-design-v0.1.md`,
  `docs/foreign-format-plugin-architecture-design-v0.1.md`.
- **Decision record (rationale):** `docs/ingest-archive-deferred-items-design-v0.1.md`.

## Cross-cutting constraints (every phase)

- `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings`
  + `cargo test` stay green. Commit per green milestone; paste test output
  (AGENTS.md). Rebuild **release** binaries before any harness run (freshness
  guard).
- **Byte-stability:** an object with only regular files (no hardlinks, symlinks,
  directories, or preserved xattrs) MUST stay byte-identical to today, and
  `REMANENCE.schema_version` stays `1.0` unless a 1.1 feature is used. Existing
  RAO test vectors stay green.
- Add the new test vectors named in each phase's design doc / spec §13.
- Update `docs/INDEX.md` statuses as phases land.

## Phase 1 — Format (crate `remanence-format`)

Implement to the already-edited specs. The two units are independent of each
other; do **1a then 1b** (hardlinks complete the 1.0 entry set; xattrs are the
1.1 minor).

**1a. Native hardlinks (ustar typeflag `1`)** — `docs/rao-hardlinks-design-v0.1.md`;
spec `specs/rao-1.0` §4.3.4, §4.6, §4.7.2, §4.9, §6, §7.4, §11.1, §13.
**CRITICAL — the model is "option B" (tar-faithful):** a hardlink entry carries
**no content fields of its own** — `size = 0`, no `file_sha256`,
`chunk_count = 0`, `first_chunk_lba = null`, exactly like a symlink. A
hardlinked name's content, hash, and PFR coordinates **resolve through
`link_target` to the primary** at read time. **Do NOT copy the primary's
coordinates onto the link entry.** Referential integrity: the target MUST
resolve to a regular-file primary appearing *before* the link
(`InvalidHardlinkTarget` otherwise). Deterministic primary selection (first in
archive order; first non-excluded if that one is omitted). Restore: materialize
the primary first, then `link(2)`, with the symlink traversal-safety discipline.
PFR (§6) and a catalog-backed index must resolve via `link_target`.

**1b. RAO 1.1 xattr metadata preservation** —
`docs/rao-1.1-metadata-preservation-design-v0.1.md`; spec `specs/rao-1.1`.
Fill the reserved per-entry `metadata_preservation_data` with an `xattrs` map;
CBOR **byte-string** values (no base64); deterministic (sorted) encoding; empty
container when none (byte-stability). Emit `schema_version 1.1` only when a
non-empty container is present. Non-UTF-8 xattr **names** are out of scope
(dropped/recorded or wrapped — handled in Phase 2c). Restore reapplies via
`setxattr`.

## Phase 2 — Ingest CLI (crate `remanence-cli`)

`docs/ingest-layer-implementation-design-v0.1.md`, units **A → B → C → D**.
C depends on 1b; D depends on 1a.

- **2a (A) — member-name reversible escaping.** Replace the `from_utf8_lossy`
  member-name handling in `build_wrap_index` / `parse_pax_records` /
  `tar_header_path` with the reversible `\xHH` escape (escape `\` too); provide
  encoder + decoder; lookups (`resolve_blob_member_from_index`, `--blob-member`)
  compare raw bytes. **Shared contract:** the customer manifest (sutradhara)
  uses the identical rule — keep them in lockstep.
- **2b (B) — cheap classification-only `--scan-only`.** Factor the per-entry
  classification into one **shared classifier** that build and scan both call;
  `--scan-only` does a records-only walk (no content hashing, no wrapper tar, no
  `.idx`, no `files`/`manifest_entries`). Switch xattr **detection** to the
  `xattr` **syscall crate** (replaces the `getfattr` subprocess in
  `has_xattrs`). (Wrapper *creation* still uses bsdtar.)
- **2c (C) — xattr ingest policy** (needs 1b). Built-in universal junk baseline
  (always dropped); ruleset `option xattr-mode denylist|allowlist` +
  `xattr-keep`/`xattr-drop`; fail-safe default (denylist + baseline). Collect
  surviving xattrs, route **small → 1.1 annotation** / **oversized → wrap**;
  count all drops (never-silent). xattr presence is **no longer** a wrap trigger
  by itself.
- **2d (D) — hardlink ingest** (needs 1a). Detect `(dev, ino)` groups; emit a
  native primary + typeflag-`1` link entries (**zero-payload, `link_target` →
  primary, no coord copy**). Deterministic primary (excluded-primary fallback);
  a group split across a blob boundary falls back to an independent copy
  (recorded). **Remove** `collect_hardlink_roots` / the common-ancestor
  machinery; `nlink > 1` is no longer a wrap trigger.

## Phase 3 — Foreign-format plugin architecture (independent)

`docs/foreign-format-plugin-architecture-design-v0.1.md`. Promote the
foreign-format-driver trait to a published extension point; move
`remanence-bru` out of the core workspace into its own crate implementing it;
rem **core builds with zero foreign-format deps** (add a CI guard, like the
platform-crate guarantee); foreign ops only via the generic `rem archive <op>
--format <plugin>` dispatch (plugin-gated); native restore = `rem restore` /
`rem archive extract`; **remove** the BRU-specific core `archive restore`.
Incremental sequencing per the doc's §3.4. Schedule independently of Phases 1–2.

## Out of scope (sutradhara repo — separate)

AppleDouble `._`→xattr staging merge, upstream compression for VM images, and
the customer-manifest member escaping (must match 2a's rule). See
`docs/prompt-ingest-v2-sutra-followups.md`.

## DoD

Per `AGENTS.md`: build + run the changed tests and paste output; commit per
logical unit; keep regular-only objects byte-stable; update `docs/INDEX.md`;
flag any spec gap rather than weakening the spec.
