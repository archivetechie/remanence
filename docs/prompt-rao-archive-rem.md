# Codex prompt — RAO ingest archive (remanence): `rem archive --rules`, wrapping, `.idx`, scan, restore

> Design by Claude + the owner; implementation by codex. **Repo: `~/remanence`** —
> the archive *mechanism*. One of a trio; companions:
> `~/sutradhara/repo/docs/prompt-rao-archive-sutradhara.md` (orchestration) and
> `~/system/docs/prompt-rao-archive-harness.md` (scenario). The **Shared contract**
> below is identical in all three. **Source of truth:**
> `~/system/docs/design-ingest-v2-rao-archive.md` (Part A is yours). Read
> `CLAUDE.md` + `AGENTS.md` first.

## What already exists (build on it, don't rebuild)
`rem archive build --inputs … [--encrypt]` already produces multi-file `rao-v1`
/ `RAO1` objects with a native member manifest (`path`, `file_sha256`,
`first_chunk_lba`, `size_bytes`); `archive inspect`/`extract` already do
keyless/keyed inspect and **ranged single-member extract incl. the encrypted
path** (proven by the RAO scenarios). Your job is the *ingest-policy* layer on
top — none of it changes the RAO/REM-PARITY wire formats.

## Scope (design §A)
1. **Ruleset engine (§A2).** Parse an ordered ruleset file — verbs `blob` /
   `exclude` (granular is the implicit default; no catch-all required),
   gitignore-style globs, `option case-insensitive`, **first-match-wins**.
   Evaluate against the input tree → per-path decision. **Lint unreachable
   rules** (a rule whose match-set is subsumed by an earlier broader one; cheap
   cases: a non-last catch-all, exact duplicates, a literal sub-path of an
   earlier dir/blob pattern). Put the engine in a higher crate; **the RAO format
   crate stays oblivious** to rulesets.
2. **Wrapping (§A3–A4).** `blob <dir>` → one `<path>.remwrap.tar` entry; a
   granular file that can't be a native entry (non-UTF-8 / device node) → a
   one-member `.remwrap.tar` (`wrap-unit=file` default; `dir` per ruleset).
   **Superseded (2026-06-15): hardlinks are now native RAO 1.0 entries
   (typeflag 1) and xattrs are preserved via RAO 1.1 — neither is a wrap
   trigger. See `rao-hardlinks-design-v0.1.md` and
   `ingest-layer-implementation-design-v0.1.md`.** Writer = a **pinned mainstream tar
   engine** (GNU tar or bsdtar/libarchive — shelled-out or FFI-linked), **never a
   bespoke Rust pax codec**. **Gating deliverable:** the §A3.5 round-trip test —
   the chosen invocation MUST byte/metadata-round-trip a non-UTF-8 name, an
   xattr'd file, a `._` AppleDouble sidecar, a dangling symlink, and an empty dir;
   record the winning invocation.
3. **On-tape `.remwrap.idx` (§A5).** For each `blob`, emit a sibling RAO entry
   `<path>.remwrap.idx` mapping member path → plaintext offset+length (+ sha256).
   Default on; `--no-index` per-rule disables. Derived state (rebuildable by
   re-scanning the `.remwrap.tar`).
4. **Conformance scan (§A4).** Classify every entry (native / wrap-fallback /
   excluded) **before any write**, output **aggregated/clustered** — counts by
   `directory-prefix × reason` + a few sample paths per cluster, never a flat
   per-entry dump. Apply the **density+count** rule to propose blobs:
   `ratio ≥ R ∧ count ≥ N` over a subtree → blob suggestion (topmost dense dir,
   but don't climb past a directory holding a substantial compliant subtree);
   sparse → straggler; high-but-spread-thin → sanity-ceiling verdict (wrong
   source/ruleset). `R`/`N`/ceiling are tunable. Expose a **`--scan-only`** dry
   run that emits this clustered report and **MUST NOT auto-switch** (it only
   suggests). NB: the `expect` *decision* (halt vs proceed) is sutradhara's; you
   produce the classification it consumes.
5. **`rem archive build --rules <file> [--manifest-out <path>]`.** Build applying
   the ruleset; `--manifest-out` writes the member listing (path/size/sha256 +
   ruleset name/hash + exclusion summary) — the raw material for sutradhara's
   customer manifest.
6. **Restore (§A5/§B5).** `rem restore` unwraps `.remwrap.tar` by default
   (destination byte/structure-identical to ingested, minus exclusions);
   `--no-unwrap` keeps literal entries. **Blob single-file ranged extract:** given
   a blob entry + a member path, read the `.idx` (ranged), resolve `(offset,len)`,
   ranged-extract that member — keyless for `rao-plain`, keyed for `rao-aead`.

## Shared contract (IDENTICAL in all three prompts)
The full design is `~/system/docs/design-ingest-v2-rao-archive.md` (cited "design
§X"); it is the source of truth. These invariants bind all three repos:
1. **Representations** (the only ones): `rao-plain-v1`, `rao-aead-v1`,
   `d2tar-raw`, `raw-bytes`. Encryption is **part of** the representation
   (`rao-aead-v1` = encrypted) — no separate flag, no `(class,copy)→representation`
   policy.
2. **RAO geometry:** 256 KiB chunks; `rao-aead-v1` is per-chunk AEAD
   (independently decryptable; encrypted ranged extract is proven).
3. **Ruleset (§A2):** ordered, **first-match-wins** (rsync/borg-style; the
   documented deviation from gitignore — lint unreachable rules). Verbs **`blob`,
   `exclude`** only; **granular is the implicit default**. `rem archive build
   --rules` is the canonical engine; sutradhara names the ruleset via the
   artifactclass policy `ruleset` field. Re-include deferred.
4. **Wrapping (§A3–A5):** `.remwrap.tar` via a pinned mainstream tar engine
   (never a Rust codec; dialect pinned by the §A3.5 round-trip test); each blob
   carries an on-tape sibling **`.remwrap.idx`** (member → offset/len + sha256),
   default-on, `--no-index` to disable.
5. **Asset identity (§B4):** the per-file **plaintext sha256**, copy-independent;
   restored bytes always verify against it.
6. **Per-copy locator (§B4/B5):** granular member →
   `{member_path, first_chunk_lba, size_bytes}` (rem) / `{member_path,
   block_range}` (d2) in `asset_locator`; a file *inside* a blob → coords from the
   on-object `.idx` (never per-member DB rows), found via the coarse `blob_root`.
7. **Bundle (§B1–B4):** single-artifactclass; **synthetic `bundle_id`**; strict
   container discriminator; `open → sealed`.
8. **Placement (§B6):** **`artifactclass ↔ pool` many-to-many** (`active`); a
   **pool is self-describing** `{id, backend, representation, location,
   offsite_gate, tier}` and **owns** representation, immutable once non-empty. **No
   `content_class` tier, no `PlacementTagPin`** — invariants are pool-immutability
   + scrub `copy.representation == pool.representation`. Restore = ordered pool
   preference.
9. **Conformance + `expect` (§A4):** scan classifies native/wrap-fallback/excluded,
   output aggregated/clustered (prefix × reason + samples + counts) with the
   density+count rule (blob-suggest / straggler / sanity-ceiling). Per-ruleset
   **`expect`** (`compliant`|`messy`) decides halt-on-deviation vs auto-wrap. Scan
   suggests; reviewer confirms.
10. **Customer manifest (§A5):** on a blob/archive emit a receipt (archive ID +
    listing + exclusion summary, signed) — re-issuable from the on-object `.idx`.

## Constraints / DoD
- No RAO/REM-PARITY wire-format change. `cargo fmt`/`clippy -D warnings` clean;
  the §A3.5 fidelity test and the unreachable-rule lint have real tests.
- Per `AGENTS.md`: run + paste test output; commit to `main` at green milestones;
  update `docs/INDEX.md` (this prompt pending → implemented).

## Sequencing
Independent of sutradhara for the clean path (it drives today's `archive build`);
the messy-source scenario in the harness gates on `--rules`/wrapping/`.idx`/scan.
