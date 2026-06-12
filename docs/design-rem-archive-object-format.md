# Design + work order — Rem Archive Object format (unifies rem-tar-v1 + AOF1)

> **Status:** design, for implementation by **codex**. **Repo: `~/remanence`.**
> Self-contained: assume no prior conversation. Read §1 (motivations) first — it is
> the load-bearing rationale and must not be undone.
>
> **This supersedes two existing formats and folds one project in:**
> - extends/replaces `docs/rem-tar-v1-design.md` (the on-tape body format),
> - absorbs the encryption construction + crypto code of **amber / AOF1**
>   (`~/amber`, `docs/aof1_candidate_specification.md`),
> - **retires amber as a separate project** (its crypto moves into remanence;
>   the standalone repo goes away once consumers migrate — no legacy reader
>   moves, per the 2026-06-11 amendment below).
>
> Working name for the format: **RAO (Rem Archive Object)**. Final name is decision
> #1 (§9). It is rem-tar's next major version.
>
> **Amendment (2026-06-11, owner — after the spec landed):** neither rem-tar-v1
> nor AOF1 was ever published or used in production, so RAO carries **no
> predecessor compatibility** (see `rao-v1-specification.md` §13, A.13):
> the AOF1 reader is **not** ported (§1.6 and task B amended below); the
> legacy-read acceptance test is dropped (§5); pre-release artifacts
> (O/N/L/M scenario objects, dev tapes) are **regenerated**, not read; and the
> stream identifier is `rao-v1`, so "byte-stable/byte-compatible plaintext"
> statements below read as "layout-stable; the format_id keyword value is
> renamed" (existing rem-tar tests update the identifier and stay green
> otherwise). The normative spec this work order implements is
> `docs/rao-v1-specification.md` — **where any detail below disagrees with
> the spec, the spec wins**; known divergences are amended inline (size
> leakage §2.2, `stored_digest` placement §2.2, keyless inspect §2.3, empty
> AAD §6, derived salts §2.2/§6).
>
> **Amendment 2 (2026-06-11, claude — approved by owner):** §10 adds the
> code-merge plan for absorbing `amber_core` — module-by-module merge map,
> the crate decision (`remanence-aead`), vector/fuzz migration, licensing.
> The published implementation target is `docs/rao-1.0-specification.md`;
> §9 decisions are now all resolved.

---

## 1. Motivations (first-principles; do not re-litigate)

### 1.1 What we are building
An archival store for large media objects (video masters etc.). Files are
content-addressed (sha256), grouped into artifacts, and **bundled** (many files →
one object) so tape is written in long sequential streams (anti-shoeshine). Each
master is kept as **three copies with different jobs**:

| copy | role | object | who writes it |
|---|---|---|---|
| copy-1 | **working** (random-access restore) | RAO, **plaintext** | remanence → rem tape |
| copy-2 | **offsite/DR** + the S3 cloud blob | RAO, **encrypted** | remanence/portable writer → rem tape + S3 |
| copy-3 | **shelf** (cold, last resort) | **plain GNU tar** | GNU tar → d2 tape — **NOT remanence** |

Remanence owns the RAO format and copies 1–2. Copy-3 is deliberately a *different*
format (see §1.5). This work order is about copies 1–2 and the RAO format.

### 1.2 The requirements that drove the design (the only things that get a vote)
1. **Bundle** N files into one object, one sequential write.
2. **Self-describing index** — the object carries its own CBOR manifest (per-file
   path, sha256, size, and *where each file lives inside the object*). Restorable
   from the object alone; lets a catalog be rebuilt from the medium.
3. **Partial file restore (PFR)** — pull one file out by byte range without reading
   the whole object (closed-form offset arithmetic).
4. **Integrity** — per-file plaintext sha256; an object `stored_digest` that backends
   scrub **without keys**.
5. **Selective encryption** — at least one copy confidential, key-managed, and
   **still PFR-able with the key**.
6. **Durability** — parity (Layer 3c) + multiple copies on different media.
7. **Longevity + diversity** — re-implementable from a doc; plaintext objects
   extractable with commodity `tar`; and **not all copies in one format/
   implementation**, so a latent bug can't take them all.

Note what is **not** a differentiator: which backend (tape/disk/S3) and which tool.
A format is just bytes — any conformant tool can write it and any backend can store
it. "Backend independence" is therefore inherent to a well-specified format, not a
property that justifies a separate format.

### 1.3 Why ONE unified format (the core decision)
We had two overlapping formats, both enveloping uncompressed tar streams for archival
storage:
- **rem-tar-v1** already delivers reqs 1, 2, 3, 4, 6 and plaintext-7: bundling, the
  **self-describing CBOR manifest**, PFR, chunk-alignment, parity integration,
  `tar`-readable plaintext. It is missing **only** encryption (it punts encryption to
  "Layer 6", by our past choice — not a law).
- **AOF1 (amber)** is, stripped down, a *weaker container* (no self-describing
  per-file index) around the same tar, **plus a good encryption construction**
  (age-STREAM ChaCha20-Poly1305, HKDF dual keys, `key_id`, salt, PFR-preserving).

So AOF1's **container** is redundant with — and inferior to — rem-tar-v1's (it lacks
the index); AOF1's **crypto** is the part worth keeping. The right answer is to
**merge**: take rem-tar-v1's self-describing bundled PFR container and add an
**optional encryption mode** that absorbs AOF1's chunked-AEAD construction. The
single format keeps the index AOF1 lacked and gains the encryption rem-tar punted,
and supersedes both.

"One format" does **not** mean crypto tangled into the container code: the encryption
stays a small, isolated, independently-auditable module (reuse amber's `amber_core`
as a *called library*), with the container invoking an `encrypt` mode.

### 1.4 Format ≠ tool
RAO is a byte format. Remanence is its tape tool, but a **portable writer/reader**
(same crate, writing to a file instead of a tape `BlockSink`) produces/consumes RAO
objects for disk and S3. The encrypted offsite copy and the S3 cloud blob are the
*same* RAO object format; remanence-the-daemon is not required to produce one.

### 1.5 Why remanence is deliberately NOT the only format (the diversity boundary)
Copy-3 exists for **format/implementation diversity** — the archival "don't put all
eggs in one basket" rule. Three copies from the same (new) RAO writer are one basket
photographed thrice; a latent writer/format bug corrupts all three identically and is
found years later. Copy-3 is a **plain GNU tar written by GNU tar** — proven 20+ years
industry-wide and ~6 years in our own archives — an independent basket that survives
any wholesale RAO failure. **Copy-3 is out of scope here** (it's sutradhara + GNU tar
+ d2), but it's why we don't try to make RAO the only format, and why copies 1–2
sharing RAO is acceptable (the diversity is copy-3's job).

### 1.6 Fold amber in
Going forward there is one archival object format (RAO). amber's value — the crypto
construction — becomes RAO's encrypt mode. Therefore:
- **Move `amber_core` (the AEAD/HKDF/key-handling crypto) into remanence** as a crate
  (e.g. `remanence-aead`) or vendor it; RAO's encrypt mode calls it.
- ~~Preserve the ability to *read* legacy AOF1 objects~~ **Amended 2026-06-11:
  no AOF1 reader is ported.** AOF1 was never published or used in production;
  the O/N/L/M scenario artifacts are regenerated as RAO objects
  (`rao-v1-specification.md` §13, A.13).
- **Retire AOF1 outright.** The AOF1 spec stays in `docs/` as a historical
  reference only.
- **Sequencing:** do not delete `~/amber` yet. Consumers (sutradhara's
  `AmberCliSealer`, the O/N/L/M scenarios) still shell out to it; they migrate to RAO
  in later, separate work. This order: land RAO + the absorbed crypto/reader in
  remanence first; flip consumers after.

---

## 2. The RAO format

RAO = **rem-tar-v1's on-tape layout, unchanged for the plaintext case**, plus an
**encrypt mode**. Read `docs/rem-tar-v1-design.md` for the base layout (object
framing, 256 KiB chunk alignment, pax `REMANENCE.*` keywords, the
`_remanence/manifest.cbor` per-file index, per-object `BodyLba`). RAO adds:

### 2.1 Plaintext mode (`encryption = none`)
- **Byte-identical to today's rem-tar-v1.** Self-describing (manifest in the clear),
  PFR via per-file `BodyLba`, and **extractable by commodity `tar`/bsdtar** (the
  longevity net for copy-1). No change beyond keeping this true.

### 2.2 Encrypt mode (`encryption = aead-stream-v1`, absorbed from AOF1)
- **Construction = amber's `aead-stream-v1`** (the reference impl is `~/amber`,
  `crates/amber_core`; spec `~/amber/docs/aof1_candidate_specification.md` §6–§7,
  §1.1, §701-on): age-style STREAM, **ChaCha20-Poly1305**, HKDF-SHA256 key
  derivation, a per-object 16-byte `hkdf_salt` *(amended 2026-06-11: derived
  from root_key + object_id + plaintext_digest + metadata bytes, not random,
  and verified on keyed open — spec §5.4.1, §5.9, A.15/A.16)*, a 16-byte
  `key_id`, and
  **separate metadata and payload keys** (`HKDF("…-metadata")` vs `HKDF("…-payload")`
  — required because the metadata nonce collides with payload chunk-0's nonce; the
  separation is what prevents nonce reuse). Reuse this construction verbatim via the
  absorbed crate; **do not hand-roll crypto.**
- **What is encrypted:** the **entire object body, manifest included** (decision §9).
  An encrypted RAO is **confidential** — it leaks no filenames or structure
  *(amended 2026-06-11: the exact payload size IS derivable from the stored
  length — spec §5.10/§12.5; encryption hides content, never size)*. A
  small **plaintext header** remains, carrying only what's needed to identify the key
  and verify structure — the spec §5.2 field list governs: magic `RAO1` (the
  format identifier), `format_version`, `suite_id`, `chunk_size`, `key_id`,
  `hkdf_salt`, `metadata_frame_len`, `object_id` *(amended: `stored_digest`
  stays external, catalog-held — spec A.10)*. This mirrors AOF1's
  keyless `inspect` so a caller can recover *which* epoch key to materialize without
  holding it.
- **Self-describing *with the key*:** decrypt → the plaintext RAO (manifest + files).
  Without the key, opaque. (Plaintext objects, copy-1, remain self-describing in the
  clear.)
- **PFR survives encryption:** AEAD chunk boundaries **align to the object's body
  blocks** (decision §9: align to the 256 KiB `BodyLba` block, or document the
  mapping), so the catalog's per-file `BodyLba` still locates a file's chunks in the
  ciphertext — restore decrypts only the covering chunks with the key. AOF1's `pfr`
  range math is the reference.
- **Digests:** keep AOF1's split — `plaintext_digest` (SHA-256 of the plaintext
  payload, identical across a plaintext and an encrypted copy of the same bundle) vs
  `stored_digest` (SHA-256 of the complete stored object bytes; backends scrub by this
  **without keys**).
- **Parity (Layer 3c) is computed over the stored (ciphertext) blocks** — encrypt
  first, then parity. 3c is unchanged; it protects bytes regardless of content.

### 2.3 Key contract (remanence never owns keys)
- The CLI takes `--key-file <path>` (the 32-byte root key, materialized by the caller
  from the shared registry `$SUTRADHARA_KEY_REGISTRY_DIR`) and `--key-id <hex>`
  (recorded in the header). Identical to amber's contract. Remanence never reads the
  registry, never persists key material, zeroizes keys after use.
- **Keyless `inspect`** reveals only `suite_id` / `key_id` / `hkdf_salt` /
  `object_id`, the frame geometry, and the exact payload size *(amended
  2026-06-11 — spec §5.10)*.

### 2.4 Verification (no wasted reads)
- **At build:** the writer streams each file, hashes it, and **rejects** the object if
  the observed sha256 ≠ the caller-supplied expected sha256 (rem-tar-v1 §9.2).
- **After write (every copy):** re-read each file via the object's read path and
  re-verify its sha256 — a media/transmission guard, distinct from the build check.

---

## 3. Implementation tasks

**A. Format spec.** Write the RAO spec (supersedes `rem-tar-v1-design.md` and the
AOF1 container; cite both). Cover the plaintext layout (by reference) + the encrypt
mode (§2.2) + static **test vectors** (a known key + payload → a fixed encrypted
object, so a future implementer can conform). Put it in `docs/`.

**B. Absorb amber.** Move `amber_core` crypto into remanence (`remanence-aead` crate
or vendored); wire RAO's encrypt mode to the absorbed crypto, with the HKDF
labels and STREAM chunk size as parameters (`rao1-*`, body-block size — spec
§5.4/§5.6, A.5). *(Amended 2026-06-11: no AOF1 reader port — §1.6.)*

**C. Encrypt mode in the writer/reader.** `crates/remanence-format/src/writer.rs`
(`write_rem_tar_object`) gains an encryption path; `reader.rs` gains decrypt + verify.
Confidential manifest (§2.2). Chunk-aligned AEAD (§2.2). `VecBlockSink` tests exist —
extend them.

**D. Portability.** Add a `FileBlockSink`/`FileBlockSource` (object ⇄ local file),
so RAO objects can be built/read to/from disk, not just tape (§1.4). Document the
file-object framing vs the tape object (parity sidecars are tape-only).

**E. Multi-file build/write CLI.** `crates/remanence-cli` (`pool_ops.rs` today has a
single-file `rem archive write` + a `read` skeleton):
- `rem archive build --inputs <list|dir> --out <file> [--encrypt --key-file --key-id]`
  → build a multi-file RAO object to a local file; emit a JSON report (`object_id`,
  `stored_digest`, `encryption`, `key_id`, and the per-file index rows).
- extend `rem archive write` to multi-file input + `--encrypt`, writing to a pool.

**F. Read/restore path.** Implement `rem archive extract` (locate a file or
`--range start:len` from a `--file` object or a tape `--locator`, decrypt with
`--key-file`, verify sha256) and `rem archive inspect` (keyless header/structure;
manifest only for plaintext objects or with the key). Extend `rem archive verify`.

**Not changing:** the rem-tar-v1 **plaintext on-tape layout** (identifier
keyword aside — see the 2026-06-11 amendment); **Layer 3c parity** — the
parity construction and geometry are untouched; the bootstrap *row schema*
gains envelope fields for encrypted objects (spec §9, A.14, D.6);
key-registry ownership.

---

## 4. Out of scope (explicitly — later, separate work)
- sutradhara orchestration (building bundles, fan-out, migrating off the amber CLI).
- the system/harness scenarios.
- **copy-3 / the GNU-tar shelf copy** and the d2-cli.
- deleting `~/amber` (consumers must migrate first).

---

## 5. Acceptance tests
- **Plaintext round-trip** (file *and* pool): build multi-file → extract every member
  → hashes match.
- **Longevity gate:** a plaintext RAO object extracts under **stock GNU `tar` and
  bsdtar** (with the `-b 512` blocking note, rem-tar-v1 §12.1); member hashes match.
- **Encrypt round-trip:** `build --encrypt`; keyless `inspect` shows
  `key_id`/`suite_id`/`salt` and **no** filenames; `extract --key-file` restores
  plaintext; wrong/absent key fails closed.
- **Byte-range from encrypted:** `extract --range` decrypts only the covering chunks
  and returns exact bytes (proves chunk-aligned PFR with the key).
- **Parity over ciphertext:** corrupt blocks in an encrypted object; 3c recovers;
  decrypt still succeeds.
- **`plaintext_digest` equality:** a plaintext and an encrypted copy of the same
  bundle share `plaintext_digest`; `stored_digest` differs.
- ~~Legacy read~~ *(dropped 2026-06-11: no AOF1 reader; pre-release artifacts
  are regenerated — see the amendment at the top of this document).*
- **No-extra-read verification:** `build` rejects an input whose streamed sha256 ≠
  the expected sha256.

---

## 6. Crypto-review checklist (codex)
- No bespoke crypto — reuse the absorbed `amber_core` construction.
- Nonce uniqueness: per-object key (derived salt — spec §5.4.1, A.15) ×
  per-chunk STREAM nonce ⇒ no reuse; keep amber's metadata/payload key
  separation.
- Object identity and chunk index are bound structurally — `object_id` via
  header-hash key derivation, index/finality via the nonce; **the AAD is
  empty** *(amended 2026-06-11 — spec §12.3, A.11)*. Verify those bindings,
  not an AAD.
- Keys via `--key-file`, transient, zeroized; only `key_id` is persisted in the object.
- Fail-closed on any tag/CRC/parity failure; never emit partial plaintext.
- Confirm no plaintext metadata leaks outside the encrypted region.

---

## 7. Grounding (where things are)
- `crates/remanence-format/` — the format: `writer.rs` (`write_rem_tar_object`,
  `BodyBlockWriter`), `reader.rs`, `layout.rs` (manifest + per-file layout/offsets),
  `pax.rs` (`REMANENCE.pad` alignment), `model.rs` (`RemTarObjectOptions`,
  `RemTarFileSpec`, `RemTarFileLayout`), `driver.rs`. `VecBlockSink` (in
  `remanence-library`) is the in-memory sink the tests already use.
- `crates/remanence-cli/src/pool_ops.rs` — `rem archive write` (single-file today),
  `archive read` (skeleton), `archive verify`.
- `crates/remanence-parity/` — Layer 3c parity (unchanged).
- `docs/rem-tar-v1-design.md` — the base on-tape format (read first).
- `~/amber` — `crates/amber_core` (crypto to absorb), `docs/aof1_candidate_specification.md`
  (the `aead-stream-v1` construction — historical crypto reference only; no
  legacy reading, per the 2026-06-11 amendment), `docs/architecture.md`.

---

## 8. DoD
Per the repo's conventions: build + run the changed tests and paste output; commit
per logical unit (don't leave the tree dirty); update any docs index. Keep the
plaintext on-tape format byte-stable (existing rem-tar tests stay green).

---

## 9. Decisions to confirm before/while implementing
1. **Format name** — RAO vs `rem-tar-v2` vs other. It may become a public spec; name
   deliberately. *(Resolved: RAO; published as `rao-1.0-specification.md`.)*
2. **Encrypted manifest = confidential** (recommended: encrypt the index too) vs
   plaintext index for self-description without the key. *Recommend confidential.*
   *(Resolved by spec: confidential — §5.1, B.5.)*
3. **AEAD chunk size** — align to rem's 256 KiB `BodyLba` block (preserves PFR on
   ciphertext) vs amber's 64 KiB. *Recommend 256 KiB alignment;* confirm the absorbed
   construction tolerates it (else document the offset mapping).
   *(Resolved by spec: AEAD chunk = the object's `chunk_size` `C`, full final
   chunks only — §5.6.1, B.1, B.4.)*
4. **amber_core: new `remanence-aead` crate vs vendored.** *(Resolved in
   Amendment 2 §10.2: new crate `remanence-aead`.)*
5. **`format_version` label** for encrypt mode (plaintext stays `1.0`-compatible).
   *(Resolved by spec: envelope magic `RAO1`, `format_version` 1, `suite_id`
   `0x01`, all frozen — §5.2.)*

---

## 10. Amendment 2 (2026-06-11) — code-merge plan for absorbing amber_core

Tasks B–F above stand; this section adds the code-level map for executing
task B as a clean merge. **The normative implementation target is the
published `docs/rao-1.0-specification.md`** (cited as "spec §" below;
`rao-v1-specification.md` remains the internal audit-trail copy). Where
amber's code and the spec disagree, the spec wins — the divergences are
deliberate spec decisions, not porting slack.

### 10.1 What amber_core is (source inventory)

`~/amber/crates/amber_core/src/`: `cbor.rs`, `error.rs`, `header.rs`,
`inspect.rs`, `kdf.rs`, `metadata.rs`, `open.rs`, `pfr.rs`, `seal.rs`,
`stream.rs`, `verify.rs`. Deps: RustCrypto (`chacha20poly1305`, `hkdf`,
`sha2`, `zeroize`) — keep these. Plus `~/amber/test-vectors/` and
`~/amber/fuzz/` (adapted, §10.4). `amber_cli` is **not** ported (the `rem`
CLI subsumes it, tasks E/F).

### 10.2 Destination: new crate `crates/remanence-aead`

A dedicated crate, not vendored modules inside `remanence-format` —
keeping the crypto small, isolated, and independently auditable (§1.3),
consistent with the workspace's layering hygiene
(`tape-platform-seam-design-v0.1.md`). `remanence-aead` holds the envelope
(header, KDF, metadata frame, STREAM payload, footer, keyless inspect);
`remanence-format` depends on it for the Sealer/opener roles and wires the
inner-stream cross-checks. `remanence-aead` MUST NOT depend on any other
remanence crate.

### 10.3 Module-by-module merge map

| amber_core | Disposition |
| --- | --- |
| `kdf.rs` | **Keep structure** (HKDF-SHA-256; `object_secret` bound to the header via `info = label ‖ header_hash`; separate metadata/payload keys). **Change** labels `aof1-*` → `rao1-*` (spec §2.5: `rao1-salt-v1`, `rao1-object-v1`, `rao1-metadata-v1`, `rao1-payload-v1`). **Add** the salt derivation function (spec §5.4.1, incl. the `ctr` retry on an all-zero result). |
| `seal.rs` | **Drop the RNG machinery entirely** — `RandomSource`, `OsRandomSource`, `generate_nonzero_salt`, `seal_with_rng`, and the `fixed_salt_for_test_vectors` interface MUST NOT be ported: the spec forbids caller-supplied salts, requires derived salts (§5.4.1), and rejected the test-only fixed-salt interface (B.11). Sealing consumes no randomness; vectors reproduce by rule. **Rewrite** the workflow to spec §5.8: final-header-before-key-derivation (no placeholder backfill), independent size+digest recompute (step 5), footer written only on success. |
| `header.rs` | **Rewrite** to the RAO1 128-byte header (spec §5.2): magic `RAO1`, `header_len` 128, `format_version` 1, `suite_id` 0x01, `chunk_size` u32, zero `flags`/`reserved`, `key_id`, `hkdf_salt`, `metadata_frame_len` u64, 64-byte NUL-padded `object_id` field with its UTF-8/interior-NUL rules. Frozen-fields rule; validation order and §11.2 error names. |
| `metadata.rs` | **Rework** to spec §5.5.3: exactly the four required integer keys, no optional keys in v1 (the frame is a deterministic function of the object — load-bearing for the zero-nonce argument), bounds 17 ≤ M ≤ 16 MiB. |
| `stream.rs` | **Keep** the STREAM construction (counter nonces, final flag, tag layout, empty AAD — already empty in amber, matching spec §12.3). **Change**: chunk size becomes the per-object `C` (kill the compile-time `AOF1_CHUNK_SIZE`); **remove** the short-final-chunk path — `P` is an exact positive multiple of `C`, full final chunks only (spec §5.6.1, B.4); finality always computed from `chunk_count`, never probed. |
| `open.rs` | **Keep** the authenticate-then-release discipline. **Add** spec §5.9 step 4 (recompute the expected salt; reject `SaltDerivationMismatch`), step 7 (plaintext size+digest verification), step 9 hooks (inner cross-checks `InnerObjectMismatch`, wired in `remanence-format`). Fail-closed rule §5.9 verbatim. |
| `inspect.rs`, `verify.rs` | **Rework** to spec §5.10 keyless geometry (closed-form `chunk_count`; positional footer check; `stored_digest`; advisory-classification caveat) and §5.7 footer/fill (`RAO1_STREAM_END.`, zero fill inside stored bytes, `stored_size_bytes` a multiple of `C`). |
| `pfr.rs` | **Re-derive** to spec §6.3/§6.4: `cipher_offset(b) = 128 + M + b × (C + 16)`; inner `BodyLba` ≡ AEAD chunk index; plaintext offsets canonical, stored offsets derived. |
| `cbor.rs` | **Align** to the deterministic-CBOR **metadata profile** (spec §5.5.1). One shared validator with the manifest profile (already in `remanence-format`) if practical; duplication is acceptable if it keeps the no-internal-deps rule of §10.2. |
| `error.rs` | **Map** to the spec §11.2 envelope error names (normative for vector manifests). |

### 10.4 Test vectors and fuzzing

Adapt amber's `test-vectors/` harness and `fuzz/` targets to the RAO suite
(`fixtures/rao/`, spec §13): RAO-TV-P1 / RAO-TV-E1 / RAO-TV-D1 plus the
§13.5 negative suite. Pinned-at-generation values are generated, then
frozen — and per freeze criterion 2 independently re-derived by a second
implementation before freezing. Note RAO-TV-E1's required equality:
the encrypted twin's `plaintext_digest` equals RAO-TV-P1's
`stored_digest`. Fuzz targets per spec §12.9/§14: header parser, both CBOR
decoders, the tar record loop, whole-object open/verify.

### 10.5 Licensing and attribution

amber is MIT/Apache-2.0 dual-licensed; remanence is AGPL-3.0-or-later.
Both are in-house code, and permissive → copyleft incorporation is clean.
Preserve amber's copyright lines in moved files (or a NOTICE comment in
`remanence-aead/src/lib.rs` noting the amber_core origin).

### 10.6 Sequencing (restating §1.6 — unchanged)

`~/amber` is **not** deleted in this work: sutradhara's `AmberCliSealer`
and the O/N/L/M scenarios still shell out to it and migrate in later,
separate work. No AOF1 reader is ported. Definition of done per §8, plus:
`cargo test` + `cargo fmt --check` + `cargo clippy -- -D warnings`, the §5
acceptance tests for the encrypt paths, and a rebuild of release binaries
before any harness run (the freshness guard).
