# Prompt — sutradhara ingest-v2 follow-ups (design from this)

> Hand this to the Claude Code instance working on the **sutradhara** repo. It
> encapsulates decisions + reasoning made jointly with the remanence side
> (2026-06-15). Your job: produce the sutradhara-side **design** for these,
> consistent with the rem-side decisions below. The rem decisions are settled —
> design sutra to match them, don't relitigate them. Source-of-truth for the
> overall model: `~/system/docs/design-ingest-v2-rao-archive.md` (Part B is
> yours). Companion rem-side design docs (read for the shared contracts):
> `remanence/docs/ingest-archive-deferred-items-design-v0.1.md`,
> `rao-1.1-metadata-preservation-design-v0.1.md`,
> `ingest-layer-implementation-design-v0.1.md`,
> `foreign-format-plugin-architecture-design-v0.1.md`.

## Context

rem is the archive *mechanism* (faithful byte engine: RAO format, wrapping,
`.remwrap.idx`, restore). sutradhara is *policy + orchestration* (intake,
artifactclass policy, ruleset naming, bundling, fan-out, catalog, customer
manifest, restore orchestration). The dividing line we keep: **rem stays a
faithful byte engine; messy/platform-specific normalization happens upstream in
sutradhara before `rem archive build` ever runs.** Four sutra-side items follow.

## 1. AppleDouble `._` → native-xattr staging normalization (optional, opt-in)

**Decision.** Offer an optional, per-artifactclass, **staging-time** transform
that merges macOS AppleDouble `._foo` sidecar files into native xattrs on `foo`
(then removes the `._foo`), on the **staged copy**, before `rem archive build`.

**Reasoning.** macOS externalizes a file's xattrs + resource fork into a
`._foo` sidecar whenever it's written to a filesystem that can't hold xattrs
natively (exFAT, SMB, zip). Whether your Mac source arrives as native xattrs or
as `._` sidecars depends purely on the transport. rem deliberately does **not**
transcode `._` ↔ xattr — it treats a `._foo` as an opaque file (archives it as
a file, restores it as a file; macOS re-merges sidecars on its own end). So to
get *uniform* metadata capture (everything as native xattrs, which rem's RAO
1.1 annotation then preserves on clean native entries), the merge must happen
upstream — and that's policy, so it's yours. Because rem only ever sees native
xattrs, **its contract is unchanged** whether they arrived natively or via your
merge.

**Design constraints to honor:**
- Opt-in per artifactclass (non-Mac sources skip it); operate on the *staged*
  copy, never the original source.
- **Recorded, never silent** ("N AppleDouble sidecars merged into xattrs"). A
  bonus: after merging, there are no `._` files left, so an `exclude **/._*`
  rule can't accidentally delete the metadata (the trap that exists if you
  exclude `._*` on un-merged data — call this out).
- Tooling on the Linux staging host: Netatalk-style AppleDouble handling or a
  small parser; `dot_clean` is macOS-only and unusable here.
- Caveat: merging a *large* resource fork makes a large xattr → rem then wraps
  that file (per its size threshold). Lossless; just note it.
- Restore symmetry is free (macOS re-externalizes native xattrs to `._` on
  non-Mac filesystems automatically).

## 2. Upstream compression for sparse / large compressible objects (e.g. VM images)

**Decision.** Large compressible objects (VM images, dept-backup blobs) are
**compressed upstream in sutradhara before archiving** — a selective,
per-artifactclass, staging-time transform — rather than rem growing a sparse
profile. rem archives the compressed file as a normal dense object.

**Reasoning.** A sparse VM image (100 GB logical, 8 GB real) inflates if
archived naively (stores the holes' zeros). We rejected a RAO sparse profile:
it would forfeit RAO's defining stock-`tar` extractability, change the body
layout (needing a hard detect-and-refuse gate), and pollute a spec we're
publishing for media; and tar's own sparse formats are a vendor minefield (no
POSIX standard; GNU 0.0/0.1/1.0 + oldgnu + star, mutually incompatible;
filesystem-dependent non-deterministic hole maps). Compression upstream eats
the holes' zeros **and** compresses the real data (beats elision on space),
reuses battle-tested compressors, and keeps RAO pure. It's the same
staging-transform pattern as item 1.

**Design constraints to honor:**
- **Selective by policy** — compress compressible classes only; never media
  (already compressed; wasted CPU).
- **Pin the compressor + level and record them** (byte-stable fanout to the
  copies; reproducibility). **Compress before encrypt** (the only order that
  compresses).
- **Record the original logical sha256** (so asset identity is the real file,
  not the compressed blob) and **verify-after-decompress** on restore.
- **Own the symmetric decompress** on restore, recorded in the catalog (this
  object is `zstd`-level-N).
- Accept that **PFR dies for a compressed object** — fine, VM images restore
  whole; nobody pulls a byte range. Identity is preserved via the logical hash.
- **Boundary:** if partial access *into* a large image without full restore is
  ever needed, revisit seekable compression (zstd seekable) or rem-side
  elision — not the dept-backup pattern; out of scope now.

## 3. Customer manifest — member-name reversible escaping (shared contract)

**Decision.** The customer manifest must use the **identical reversible
member-name escaping** rem uses in the `.remwrap.idx`, so the member identifier
is consistent across the `.idx`, the manifest, and the restore request.

**The rule** (applied to raw name bytes; deterministic): literal `\` → `\\`;
any byte not part of valid UTF-8, plus control chars (`< 0x20`, `0x7F`) →
`\xHH` lowercase hex; all other valid UTF-8 passed through. It's a bijection
(escaping the backslash is what makes it reversible). A customer can copy an
escaped name from the manifest into a restore request and the tooling decodes
it to bytes. **Reasoning:** non-UTF-8 member names (legacy 8-bit encodings) are
exactly what wrapping preserves; a lossy `U+FFFD` rendering would collide
distinct names and corrupt restored names. Implement the same rule on your side.

## 4. Ruleset carries xattr policy + restore verb alignment (awareness items)

These are mostly rem-side, but sutra must align:

- **Ruleset xattr directives.** The ruleset (which sutradhara names per
  artifactclass) can now carry `option xattr-mode denylist|allowlist` plus
  `xattr-keep`/`xattr-drop` lines; rem parses and applies them. Default
  (absent) is fail-safe denylist with a built-in junk baseline. sutra just
  needs to know these directives exist when authoring/selecting rulesets per
  artifactclass (e.g. the photo class may want `allowlist` keeping color tags).
- **Foreign formats become rem plugins; restore-verb change.** BRU and other
  legacy read formats move out of rem core into compile-time plugins; native
  RAO restore is `rem restore` / `rem archive extract`, and foreign restore is
  the generic `rem archive <op> --format <plugin>`. sutra's restore
  orchestration should dispatch on representation/format accordingly and not
  assume a BRU-specific core command.

## What to produce

A sutradhara-side design covering items 1–3 as concrete staging-time transforms
and catalog/restore bookkeeping (where the merged/compressed state and the
logical hashes are recorded; how restore reverses them), and item 4 as the
ruleset-authoring + restore-dispatch alignment. Keep Part B's bundling/catalog/
fan-out model intact; these are additive. Flag anything where a rem-side
assumption above doesn't fit sutradhara's actual structures so we can reconcile.
