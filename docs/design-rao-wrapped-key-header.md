# Design: RAO envelope-encryption — wrapped data key in the header (v2)

**Status: DRAFT 2026-07-10 — pending panel review.**
Origin: backup-copy key-architecture thread with the owner (2026-07-10, system session).
Companion context: `~/sutradhara/docs/consult-backup-ledger-keys-2026-07-10.md`
(three-domain key architecture; dev-seed blocker), STANDING row on the deterministic
key-registry seed.

## Decision record (already made — the panel reviews the mechanism, not these)

1. **Envelope encryption for the archive AEAD copy** (the owner): the sealing host holds
   an encrypt-only public key; the decrypting private key is air-gapped + escrowed.
   Justified by restore-path frequency: the policy's ordered pool preference makes
   the AEAD copy a rare, last-resort read (plaintext on-site pools serve daily
   restores), so a human unwrap ceremony on that path is acceptable and desirable.
2. **The wrapped data key travels IN the RAO header** (the owner, over Claude's
   sibling-object alternative): self-containment becomes a property of the FORMAT,
   not a convention of tape layout. Medium-independent: the same object is
   recoverable from tape, the LAN backup box, s3, or a drive in a drawer — with no
   catalog. The rejected sibling-object approach also breaks tape streaming (a
   ~100-byte object cannot stream; its flush forces a back-hitch per bundle) and
   doubles filemark count; but the deciding argument is self-description, not
   throughput.
3. **Proof re-derivation is accepted** (the owner, explicit): the Lean kernels exist to
   make format changes safe, not to forbid them. Per the new-proof rule (pure
   kernel, high blast radius, stable interface) this surface is exactly what the
   verif estate is for.

## Current format (v1, ground truth)

`crates/remanence-aead/src/header.rs`: fixed **128-byte header** — magic `RAO1`,
`FORMAT_VERSION = 1`, suite `0x01` (HKDF-SHA256 + ChaCha20-Poly1305), `chunk_size`,
`key_id[16]` (opaque archive key identifier), `hkdf_salt[16]` (deterministic
per-object), `metadata_frame_len`, `object_id` — followed by an encrypted metadata
frame (≤16 MiB, AEAD-tagged), body chunks, and the `RAO1_STREAM_END.` footer.
Key material today: a symmetric registry root key (sutradhara `KeyRegistry`
materializes a key file for BOTH seal and restore — the same key encrypts and
decrypts, and it derives from a hard-coded dev seed; see STANDING).

Verified surface touched by this design: `verif/rao-header` (parse/roundtrip),
`verif/aead-framing` (offsets/lengths), fuzz target
`fuzz/fuzz_targets/rao_envelope_header.rs`, negative vectors in
`crates/remanence-format/tests/rao_negative_vectors.rs`.

## Proposed v2

### Header

- `FORMAT_VERSION = 2`, **fixed header length 256 bytes** (v1 readers reject on
  version, as today; v2 readers accept both lengths keyed on version).
- New fields in the extension area:
  - `key_mode: u8` — `0x01` registry-symmetric (v1 semantics) | `0x02` envelope.
  - `recipient_id[16]` — generation-tagged recipient identifier (mirrors `key_id`
    style; names the offsite keypair generation that can unwrap).
  - `wrapped_dek` block — the per-object data-encryption key sealed to the
    recipient public key (X25519 sealed-box/HPKE: ~32 B epk + 32 B key + 16 B tag;
    fixed 96 B slot, zero-padded), plus a second **optional recipient slot**
    (see Multi-recipient below).
  - Reserved zero padding to 256 B for future fields.
- In envelope mode, `key_id` carries the recipient generation and the HKDF input
  is the **random per-object DEK** instead of a registry root key. (Side effect
  worth stating loudly: envelope objects get honest random keys — the dev-seed
  weakness does not apply to them. v1 objects and non-envelope domains still need
  the registry fix.)

### Authentication of the wrapped key

A swapped/corrupted wrapped DEK cannot decrypt anything (wrong DEK → AEAD tag
failure on the first frame), so confidentiality/integrity of content never depends
on header protection. To make tampering *diagnosable* rather than incidental,
v2 binds the full 256-byte header as AAD of the metadata frame (v1 binds
`hkdf_salt`-derived context only — panel: verify this claim against
`seal.rs`/`stream.rs` and price the change). DoS-by-corruption of the header is
mitigated by redundant wrapped-key copies (below), not by the format.

### Seal / restore flows

- **Seal (unattended, hot host):** generate random 32-byte DEK → seal object with
  DEK → wrap DEK to configured recipient public key(s) → emit into header →
  discard plaintext DEK. The host never holds a decrypt capability for v2 envelope
  objects.
- **Restore (rare, ceremonial):** read header (any copy of the object, any medium)
  → unwrap DEK on the air-gapped machine (standalone tool, below) → hand DEK key
  file to the existing restore adapter (interface unchanged: it takes a key file).

### Multi-recipient (recommended, panel to confirm)

Two recipient slots: the offsite-safe keypair AND a separate escrow keypair.
Either private key alone recovers the object (~96 bytes extra per object). This
doubles the disaster paths — safe lost OR escrow lost still recovers — and it is
the cheapest redundancy in the whole design.

### Redundant wrapped-key storage (unchanged from the thread's decisions)

Header (authoritative, self-describing) + sutradhara catalog (operational fast
path) + escrow exports (belt-and-suspenders). Catalog/escrow copies are
conveniences; the header makes them non-load-bearing.

### Standalone unwrap tool

`rao-unwrap` (new small bin in remanence): input = `.rao` path (or just its first
256 bytes) + recipient private key file → output = DEK key file. No daemon, no
catalog, no network — must run on an air-gapped machine. Ships with the DR
runbook; the drill (quarterly, per the escrow thread) exercises it end to end.

## Impact map

- **remanence:** header v2 (`remanence-aead`), seal/stream offset handling, the
  `rao-unwrap` bin, negative vectors (wrong-recipient, truncated slot, v1/v2
  cross-reads), fuzz target update. **Proofs:** `rao-header` + `aead-framing`
  kernels re-derived for v2; drift guards + `make proof-inventory` updated.
- **sutradhara:** pool/representation config gains recipient generation; envelope
  pools pass recipient pubkey instead of materializing a root key; catalog +
  escrow-export wrapped-key copies; `backup` key domain unaffected (stays hot
  symmetric per the recorded decision).
- **Sequencing:** with or after the dev-seed replacement (one key-architecture
  arc); independent of receive-dedup phases.

## Threat model (what this buys, honestly)

- Offsite tape theft: unreadable (no standing decrypt capability exists online).
- Full compromise of the archive host: cannot read v2 envelope copies (can read
  the plaintext on-site copies — physical/host security still owns that).
- Catalog/database loss: v2 objects fully recoverable from any medium + the safe.
- NOT protected: content confidentiality against an attacker with drive access to
  the plaintext on-site pools; availability of a single header copy (mitigated by
  copy redundancy, not the format).

## Open questions for the panel

1. Fixed 256 B header vs a length-prefixed key frame (TLV) — extensibility vs
   parse-kernel simplicity.
2. HPKE (RFC 9180) vs libsodium sealed-box for the wrap — pick the one with the
   most boring, auditable Rust implementation.
3. Exact AAD binding of the header in v1 today, and the v2 rule.
4. Whether `verify`-sweep tooling should parse v2 headers to alarm on
   recipient-generation drift (objects wrapped to a retired generation).
5. Migration posture for existing v1 AEAD objects: leave as v1 forever (registry
   key escrowed) vs opportunistic re-seal on rewrite/migration cycles.
