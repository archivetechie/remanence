# Prompt RM0.3a — streaming AEAD decrypt helper (remanence)

**Status:** pending (gpt-5.6-sol). RM0.3 (option A) part a — the REMANENCE side of streaming encrypted
restore. RM0.3b (sutradhara integration into RestorePlan) follows after this lands.
**Context:** sutradhara's RM0.1/RM0.2 stream plaintext restores bounded; AEAD restores still materialize
the WHOLE encrypted object + shell `rem archive extract` to a file (OOM/OOD on tape-scale). The
`remanence-aead` crate ALREADY has the streaming primitive — `open<R: Read, W: Write>`
(`crates/remanence-aead/src/open.rs:30`) authenticates + decrypts + writes one plaintext chunk at a
time through generic Read/Write, per-chunk-authenticated (yields plaintext only after each chunk's tag
verifies). The gap is exposing it as a **helper-process protocol** sutradhara can pipe through, so the
ciphertext never fully materializes and plaintext delivery is bounded + backpressured.
**Read first:** `crates/remanence-aead/src/open.rs` (the streaming `open`, `OpenReport`),
`crates/remanence-aead/src/stream.rs` (chunk primitives, `stream_nonce`, `decrypt_chunk`,
`cipher_offset`), `crates/remanence-aead/src/range.rs` (existing `open_*_range_to_vec` — whole-buffer),
`crates/remanence-cli/src/lib.rs` (the `archive extract` command ~L1901/2472, `open_to_vec` call
~:9664, the `fs::read` whole-object load ~:9645/10399), and the RAO 1.0 AEAD spec
(`specs/rao-1.0-specification.md` §AEAD).

## Scope — a bounded, per-chunk-authenticated streaming decrypt CLI
1. **CLI helper (`rem archive extract-stream`, or a `--stream` mode on `archive extract`):** reads the
   encrypted RAO **ciphertext from stdin** (sequential stream — sutradhara feeds it from a ranged
   backend read, never a whole buffer), decrypts via the streaming `open<R, W>` with `R = stdin`,
   `W = stdout`, and writes **plaintext to stdout**. Bounded memory (one chunk in flight; NEVER buffer
   the whole object). Key from `--key-file` (reuse the existing `RootKey`/key-file handling from
   `archive extract`; do NOT add new key material handling). Emit the `OpenReport` (or a JSON report)
   to **stderr**, not stdout, so stdout is pure plaintext.
2. **Per-chunk authentication is the safety core (RAO AEAD = per-chunk `ciphertext||16-byte tag`,
   nonce from `(counter, final_chunk)`):** plaintext for a chunk is emitted to stdout **only after that
   chunk's Poly1305 tag verifies** (this is already `open`'s contract — do NOT weaken it). A tag failure
   ⇒ abort with a nonzero exit + a clear error to stderr, having emitted only already-authenticated
   plaintext. Final-chunk finality enforced (the `final_chunk` nonce flag — a truncated stream where the
   last-seen chunk is not marked final MUST fail, so truncation can't masquerade as a complete restore).
3. **Member / range within the decrypted object (if needed for a bundle member):** the decrypted
   plaintext is a pax tar (or a single object). Whole-object encrypted restore (the common case) =
   decrypt the whole stream to stdout. If a `--path <member>` / `--range START:LEN` is given, extract
   that member/range **from the decrypted plaintext stream** (streaming tar-member read / range slice),
   NOT by seeking ciphertext — keep it bounded. (Ciphertext-seek member-selective reads via
   `cipher_offset` are a later optimization; sequential decrypt is correct and bounded for tape.)
4. **Do NOT regress** the existing `rem archive extract` (to-file, whole-object) path — this is an
   ADDITIVE streaming mode. No new mode flag on the storage/write side; git revert is the backout.

## Binding invariants
- Never emit unauthenticated plaintext (per-chunk tag before release). Never buffer the whole object
  (bounded, one chunk). Truncated/last-chunk-not-final ⇒ hard fail. Wrap `remanence-aead`'s existing
  `open`/`stream` primitives — do NOT reimplement the AEAD/nonce/tag logic. stdout = pure plaintext;
  diagnostics to stderr. Reuse existing key-file handling.

## Tests (crate + CLI)
- **Round-trip:** seal an object, pipe its ciphertext through the stream helper, assert stdout plaintext
  == original (matches `open_to_vec`'s result byte-for-byte).
- **Bounded memory:** decrypt a large (e.g. ≥ 64 MiB) object; peak RSS bounded (a small multiple of the
  chunk size), not the object size.
- **Corrupt chunk ⇒ no bad plaintext:** flip a byte in a payload chunk's ciphertext/tag → the helper
  aborts nonzero with only the preceding authenticated plaintext emitted; the corrupt chunk's plaintext
  is NEVER written.
- **Truncation ⇒ fail:** cut the stream before the final chunk → hard fail (final-chunk finality), not a
  silent short restore.
- **Member/range (if implemented):** a boundary-spanning range and a final-chunk range decrypt to the
  correct plaintext bytes.
- **Whole `archive extract` (to-file) unchanged:** its existing tests stay green.

## Definition of done (AGENTS.md)
`cargo test --workspace` green, `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`.
Summary: files touched, each test → the property it covers, and an explicit statement that plaintext is
released only after per-chunk authentication and the whole object is never buffered. Do NOT `#[ignore]`
the corruption/truncation tests. Note for RM0.3b: document the exact stdin/stdout/stderr contract +
exit codes so the sutradhara integration can pipe through it.
