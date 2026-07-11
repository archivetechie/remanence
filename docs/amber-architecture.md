# Amber Architecture

> **Historical reference only.** Amber/AOF1 was retired before publication or
> deployment. Its useful AEAD construction was absorbed into Remanence's RAO
> implementation; it is not a current workspace, CLI, service, or dependency.
> For the current format and implementation boundary, read
> [`rao-v1-specification.md`](rao-v1-specification.md) and
> [`architecture-overview.md`](architecture-overview.md). The remainder of
> this document is retained to explain the predecessor design and migration
> decisions, not to describe the live code.

This document describes how the Amber workspace is put together and records
the reasoning behind its main design decisions, so future changes do not
accidentally reverse them. The normative format definition lives in the
[AOF1 Candidate Specification](historical/aof1_candidate_specification.md); where the
two disagree, the specification wins.

## 1. What Amber Is

Amber is the reference implementation of the Archive Object Format v1
(AOF1): a durable object container that envelopes large archival payloads
(typically uncompressed tar streams) before they are dispatched to storage
tiers such as tape, object storage, or disk pools. Amber deliberately does
*one* job — framing, optional encryption, and verification of single
objects — and leaves orchestration, cataloging, key management, and storage
placement to the systems around it:

- An **orchestrator** builds payloads, computes their size and digest,
  prepares metadata, invokes Amber, and records the results.
- **Storage backends** store `.aof` files as opaque bytes and scrub them
  with `stored_digest`, never needing keys or plaintext access.
- A **key registry** owns root key material and maps the 16-byte `key_id`
  in each object header to keys or retrieval procedures.

This separation is load-bearing: it is what lets one sealed object be
replicated byte-for-byte to every backend, and what keeps the cryptographic
core small enough to specify, test, and recover decades later.

## 2. Workspace Layout

| Crate / path | Role |
| --- | --- |
| `crates/amber_core` | The AOF1 library: framing, deterministic CBOR, key derivation, AEAD streaming, verification, PFR range math |
| `crates/amber_cli` | The `amber` binary: argument parsing, file/pipe plumbing, development key files, `.partial` commit protocol |
| `test-vectors/` | Static conformance vectors (positive manifests and negative binary fixtures) |
| `fuzz/` | `cargo-fuzz` targets for the header parser, CBOR decoder, and whole-object verifier |

Module map inside `amber_core`:

| Module | Owns |
| --- | --- |
| `header` | The fixed 64-byte public header: parse, serialize, validate, hash |
| `cbor` | The AOF1-CBOR deterministic subset: canonical-validating decoder and limit-enforcing encoder |
| `metadata` | The typed `AofMetadata` schema over CBOR, preserving unknown descriptive keys |
| `kdf` | `RootKey`, the `KeyResolver` trait, and HKDF-SHA-256 key derivation |
| `stream` | Byte accounting (hashing readers/writers), metadata AEAD, and the chunked payload STREAM |
| `seal` / `open` / `verify` / `inspect` | The four top-level operations |
| `pfr` | Partial-file-restore range math, including a streaming iterator |
| `error` | The typed `AofError` taxonomy mirroring the specification |

## 3. Core API Boundaries

The library exposes synchronous functions over standard I/O traits:

```rust
pub fn seal<R: Read, W: Write>(input: R, output: W, options: SealOptions) -> Result<SealReport>;
pub fn open<R: Read, W: Write>(input: R, output: W, options: OpenOptions) -> Result<OpenReport>;
pub fn verify<R: Read>(input: R, options: VerifyOptions) -> Result<VerifyReport>;
pub fn inspect<R: Read>(input: R) -> Result<InspectReport>;
```

**The decision:** core operations consume generic `std::io::Read` and
`std::io::Write` values and nothing else.

**Why:** this decouples Amber completely from the filesystem and network. A
caller can supply an in-memory buffer, a pipe from a tar subprocess, or a
socket, and Amber streams the cryptographic transformation through it. It
also keeps the crate free of async runtimes: archival sealing is a
sequential, CPU-bound pipeline, and a synchronous core is easier to verify,
profile, and embed (including across FFI boundaries). Concurrency belongs
at the job level in the orchestrator, not inside the format crate.

## 4. The Key Management Boundary

```rust
pub trait KeyResolver {
    fn resolve_root_key(&self, key_id: [u8; 16]) -> Result<RootKey>;
}
```

**The decision:** Amber never fetches keys. Sealing takes root key material
directly in `SealMode::AeadStream`; opening and verification take an
optional `&dyn KeyResolver` in their options.

**Why:** key retrieval is environment policy — KMIP, HSMs, agents, escrow —
and embedding any of it would drag network failure modes and authentication
policy into the cryptographic core. The crate ships two trivial resolvers
(`SingleKeyResolver`, `StaticKeyResolver`) sufficient for the CLI and
tests; real deployments implement the trait against their registry.
`RootKey` enforces the 32-byte minimum at construction, zeroizes on drop,
and redacts itself from debug output.

## 5. Separation of Identity and Representation

Every object carries two hashes with distinct jobs:

- **`plaintext_digest`** — SHA-256 of the payload bytes before framing.
  This is the *logical* identity: a raw copy and an encrypted copy of the
  same payload share it, and an external index joins them as one asset.
- **`stored_digest`** — SHA-256 of the complete `.aof` bytes. This is the
  *physical* identity: backends scrub against it without keys, and two
  backends holding the same object can compare it byte-for-byte.

Amber computes both during every seal, open, and keyed verify, and refuses
to commit an object whose payload does not match its injected metadata —
the completion footer is only written after the check passes, so a
truncated or mismatched write is never a valid object.

## 6. Deterministic CBOR, In-House

**The decision:** metadata is RFC 8949 deterministic CBOR restricted to an
enumerated subset (AOF1-CBOR), implemented by a small in-tree
encoder/decoder rather than a general-purpose CBOR dependency.

**Why:** the format requires byte-for-byte reproducible metadata (the frame
length is in the header, and the header hash keys the AEAD), strict
canonical *validation* on read (over the original bytes, so unknown
descriptive keys survive), and rejection of floats, tags, and
indefinite-length items. General CBOR libraries guarantee none of this by
default, and auditing one for these properties is harder than maintaining
~400 lines of subset code. Both directions enforce the format's structural
limits (nesting depth 32, 65,536 items) so neither hostile input nor
caller-built values can drive unbounded memory or recursion, and the
decoder bounds pre-allocation by the item budget.

## 7. Cryptographic Design

The chain, in one picture:

```text
root_key (registry)  +  hkdf_salt (fresh CSPRNG, per object)
        |                       |
        +--- HKDF-Extract ------+        header_hash = SHA-256(64-byte header)
                  |                                 |
                  +---- HKDF-Expand("aof1-object-v1" || header_hash) ----+
                                                                         |
                                                              object_secret (32 bytes)
                                                              /                \
                                       HKDF("aof1-metadata-v1")        HKDF("aof1-payload-v1")
                                              |                                |
                                        metadata_key                     payload_key
                                     (one-shot AEAD,                (64 KiB STREAM chunks,
                                      zero nonce)                    counter nonces)
```

Three properties are deliberate and must survive refactoring:

1. **Salt freshness is the single uniqueness source.** The header contains
   no payload-derived entropy (a privacy decision), so per-object key
   uniqueness — and therefore the safety of the zero metadata nonce and the
   counter payload nonces — rests entirely on the fresh random salt. The
   sealing API names its test-only override `fixed_salt_for_test_vectors`
   to keep this from being misused.
2. **Metadata and payload keys must stay separate.** The metadata nonce
   (all zeros) is byte-identical to payload chunk 0's non-final nonce;
   only the key separation makes that harmless.
3. **The header is bound through derivation, not AAD.** Any header bit flip
   changes `header_hash`, which changes every derived key, which fails all
   authentication — so the AEAD AAD can stay empty.

The payload uses the age-style STREAM construction: 64 KiB chunks, an
11-byte big-endian counter plus a final-chunk flag in each nonce, an
authenticated (possibly empty) final chunk required, and chunk counts
derived from the authenticated `plaintext_size` rather than discovered from
the stream.

## 8. Failure Discipline

- **No panics on untrusted input.** `unwrap`, `expect`, unchecked indexing,
  and unchecked arithmetic are forbidden (and lint-enforced) on every path
  reachable from object bytes or injected metadata; fuzz targets exercise
  the claim.
- **Typed errors end-to-end.** Every failure maps to a variant of
  `AofError` matching the specification's taxonomy, so test vectors can pin
  exact failure modes and callers can distinguish storage faults (`Io`)
  from invalid objects.
- **Commit protocols, not best effort.** The CLI writes to an exclusively
  created `.partial` file, fsyncs it, renames it, and fsyncs the directory
  before reporting success; the library never writes a footer on a
  digest or size mismatch. An object missing its footer is incomplete by
  definition.
- **Bounded resources.** The metadata frame is capped at 16 MiB, decoded
  CBOR is capped by depth and item count, and payload processing uses
  constant memory per chunk regardless of object size.

## 9. Why Rust

Implementing AEAD stream encryption, key derivation, and binary framing
demands memory safety without runtime overhead: parsing attacker-controlled
bytes is exactly where buffer errors become vulnerabilities. Rust provides
that at compile time, while the absence of a garbage collector and async
runtime keeps streaming throughput predictable for multi-hundred-MB/s
archival pipelines. The crate forbids `unsafe` entirely.
