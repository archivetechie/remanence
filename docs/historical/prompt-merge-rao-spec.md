# Prompt — author the unified Rem Archive Object (RAO) specification

> **For a fresh session. Role: specification author. Repo: `~/remanence`. Output is a
> SPEC, not code — do not touch `crates/`.**
>
> Spec-first by design: we are writing the normative format spec *before*
> implementation so design and implementation stay honest — the spec, anchored by
> **static test vectors**, is the contract the implementation is later checked
> against.

## Task
Merge two of our own existing formats into ONE official, normative specification —
the **Rem Archive Object (RAO)** format. All sources are in `docs/`:

- **rem-tar-v1** — the self-describing, bundled, chunk-aligned tape container (the
  base): `rem-tar-v1-design.md`, `rem-tar-v1-candidate-specification.md`.
- **AOF1 / amber** — the archival encryption construction:
  `aof1_candidate_specification.md`, `amber-architecture.md`.
- **Design rationale + decisions already made:** `design-rem-archive-object-format.md`
  — **read its §1 first; it is the WHY and is not to be re-litigated.**

## Why we're merging (so you don't undo it)
rem-tar-v1 already solves bundling, the **self-describing CBOR index**, PFR, chunk
alignment, parity integration, and plaintext `tar`-readability — it lacks only
encryption. AOF1 is a *weaker container* (no per-file index) around the same tar
payload **plus a good encryption construction**. So: keep rem-tar-v1's container and
fold in AOF1's encryption as an **optional mode**. One format, supersedes both.
(Diversity for the cold "shelf" copy is provided separately by a plain GNU tar — out
of scope here. RAO is the working + offsite copies.)

## Decisions already locked (carry into the spec; record every conflict you resolve)
- **Plaintext mode = rem-tar-v1's on-tape layout, byte-compatible** — commodity `tar`
  extracts it (the longevity net).
- **Encrypt mode = AOF1's `aead-stream-v1` construction** — age-style STREAM,
  ChaCha20-Poly1305, HKDF-SHA256, **separate metadata/payload keys**, per-object
  random `hkdf_salt`, 16-byte `key_id`. Reuse verbatim.
- **Encrypt mode is confidential** — the manifest/index is encrypted too. A small
  plaintext header carries only `format_id`, `format_version`, `object_id`,
  `encryption` suite_id, `key_id`, `hkdf_salt` (for keyless inspect / key recovery).
- **AEAD chunking aligns to the 256 KiB body block** so PFR works on ciphertext with
  the key. (AOF1 uses 64 KiB STREAM chunks — resolve this explicitly: prefer 256 KiB
  alignment; document the chosen size + the offset mapping + rationale.)
- Keep AOF1's **`plaintext_digest`** (logical identity, shared by a plaintext and an
  encrypted copy of the same payload) vs **`stored_digest`** (physical bytes; backends
  scrub keyless).
- **Layer 3c parity** is computed over the stored (ciphertext-when-encrypted) bytes;
  unchanged.
- **Key registry is external**; objects carry only `key_id`; the format never holds
  key material.

## The spec MUST contain (normative; RFC 2119 MUST/SHOULD)
1. Scope, terminology, conformance roles (writer, reader, verifier, **keyless
   verifier**, restorer).
2. Object layout: fixed header; global pax header; per-file pax+ustar entries; chunk
   alignment (`REMANENCE.pad`); the `_remanence/manifest.cbor` index (the **full CBOR
   schema**); tar EOF; final-block fill.
3. Plaintext mode — reference the rem-tar-v1 layout; assert byte-compatibility and
   commodity-`tar` extractability (incl. the `-b 512` blocking note).
4. Encrypt mode — the **complete** construction: key derivation (HKDF labels), nonce
   scheme, AAD, per-chunk framing, tag placement, the confidential manifest, and the
   plaintext header fields. Enough that an independent implementer can conform without
   the source code.
5. Digests, integrity, and the **no-extra-read verification chain** (streamed sha256
   at write vs the caller-supplied expected hash; re-verify after write).
6. **PFR** — closed-form payload-range → stored-range arithmetic, for both modes.
7. Versioning (`format_id`, `format_version`; plaintext stays 1.0-compatible).
8. Backend independence (it is a byte format — tape / disk / object store) + the
   **file-object framing vs the tape object** (parity sidecars are tape-only).
9. Relationship to Layer 3c parity.
10. **Static test vectors** — at minimum a plaintext object and an encrypted object:
    fixed key + fixed payload → fixed stored bytes (or their digests) + `plaintext_digest`
    / `stored_digest`. This is the honesty anchor the implementation is checked
    against. Give the inputs and the expected outputs explicitly.
11. Security considerations (nonce uniqueness, metadata/payload key separation,
    fail-closed on tag/parity failure, metadata confidentiality).
12. A "**Changes from rem-tar-v1 and AOF1**" section + a **legacy note**: existing
    AOF1 objects MUST remain readable (the AOF1 reader is being ported into
    remanence); the AOF1 spec is retained as the legacy-format reference.

## Discipline (this is what keeps us honest)
- **Spec only — no implementation. Do not modify `crates/`.**
- Resolve **every** conflict between the two source specs explicitly and list them in
  a "Resolved conflicts" section — at least: AEAD chunk size (64 KiB vs 256 KiB);
  the encryption keyword (rem-tar's `REMANENCE.encryption=aes-gcm-256` flag vs AOF1's
  software `aead-stream-v1`); digest definitions; metadata handling; object framing /
  filemarks.
- Where the sources disagree, the **design doc's decisions win**; where it is silent,
  choose and record the rationale inline. Never silently paper over a gap — flag
  genuinely open spec questions for the maintainer.
- **Preserve the source specs** (`rem-tar-v1-*.md`, `aof1_candidate_specification.md`,
  `amber-architecture.md`) — do not overwrite them.

## Output
- `docs/<name>-specification.md` — the unified normative spec. Working name **RAO /
  rem-archive-object**; confirm or pick the official name (it may be published) and
  state it at the top with status/version.
- Nothing else changes. The implementation work order
  (`design-rem-archive-object-format.md`) follows this spec in a later session.
