# Prompt RM3.3 — ranged-ciphertext AEAD extract-stream (bundle O(N×object) → O(object))

**Status:** pending (gpt-5.6-sol). RM3.3 (after RM3.0/RM3.1a; sequence-independent of RM3.2). Remanence
(streaming ranged decrypt + extract-stream) with a small sutradhara integration (pass the plaintext member
range). **Physical throughput acceptance is the MSL3040 window; this delivers the code + hermetic
correctness/bounded-RSS tests.**
**Normative (read FIRST, binding — do NOT inline):** `docs/design-restore-tape-leg-v0.1.md` **§6.6**
(keep the plaintext→ciphertext mapping in RUST — `range.rs cipher_offset`; sutradhara passes the PLAINTEXT
member range; adopt **option (c)**: a small remanence query returns the covering STORED byte-range for
`(object_id, file_id, plaintext_start, len)`, sutradhara issues a bounded `ReadObjectRange(start,end)` and
pumps only that into a TRIMMING reader/stream variant; **N independent ranged opens per member** — do NOT
do a single-stream multi-member pass (that is where the multi-object nonce-boundary ambiguity bites));
**§6.1** (the ranged primitive is BUILT + wired to the BUFFERED `archive extract` path + tested — the
STREAMING `extract-stream` path uses whole-object `open()`; RM3.3 is a new SURFACE on a TESTED core, not
new crypto). **Survey the CURRENT code, cite real lines.**
**Survey + verify against CURRENT code:** `crates/remanence-aead/src/range.rs` — `open_plaintext_range_*`
(~45/67/83), `open_plaintext_range_with_context`, `open_authenticated_metadata` (the fail-closed metadata
parse), `cipher_offset*` (~15), `decrypt_chunk`, per-chunk `(chunk_index, final_chunk)` nonces (NO chaining,
only covering chunks fetched+authenticated). These take `input: &[u8]` and index ABSOLUTE offsets — a
reader/stream variant is needed. `crates/remanence-cli/src/lib.rs` — `run_archive_extract_stream` (~3261),
the `extract-stream` command (~1764/2372), the whole-object `open()` streaming path + the `--range`
plaintext-OUTPUT trim. The BUFFERED path that already uses the ranged primitive (for reference).

## Scope
1. **Reader/stream variant of the ranged decrypt (bounded — no whole-`&[u8]`).** Add a `range.rs` API that:
   parses header + metadata from a small AUTHENTICATED PREFIX (`open_authenticated_metadata`, fail-closed),
   then pulls `stored_chunk_len`-sized chunks from a bounded `Read` seeked to `stored_range_start`,
   decrypting only the COVERING chunks (reuse `decrypt_chunk` + the `(chunk_index, final_chunk)` nonce;
   `object_chunk_count` from the prefix metadata for the final-chunk flag), streaming with first/last-chunk
   edge trim (replacing the current whole-Vec trim). Preserve the bounded/authenticated guarantees — never
   buffer the whole object; per-chunk auth intact; fail-closed.
2. **Covering-stored-range query (option c).** A small remanence surface that, for `(object_id, file_id,
   plaintext_start, len)`, returns the covering STORED byte-range (via the Rust `cipher_offset` mapping +
   tag padding + metadata frame len) — the single source of truth for the plaintext→ciphertext mapping.
   Do NOT reimplement the mapping in Python.
3. **Wire ranged mode into `extract-stream`.** Add a ranged-ciphertext mode (a `--range`-driven path, or
   `extract-stream-range`) that, instead of whole-object `open()`, drives the reader/stream variant over
   ONLY the covering stored bytes. **N independent ranged opens per member** for a bundle (each open reads
   only its own covering range → O(object) tape bytes, not O(N×object)); do NOT attempt the single-stream
   multi-member linear pass (defer per §6.6 — it saves locate-count only and hits the boundary ambiguity).
4. **Sutradhara integration (small — a patch, since --cd remanence can't write it).** Where sutradhara's
   AEAD extract path currently pumps the whole `_stored_object_range` per member, change it to: pass the
   PLAINTEXT member range, obtain the covering stored range from the remanence query (option c), issue a
   bounded `ReadObjectRange(start,end)` for only that, and pump it into the trimming ranged extract. Emit
   this as a `sutradhara-rm3-3.patch` in the worktree root (like RM3.1a) if you cannot write sutradhara.

## Binding invariants
- Plaintext→ciphertext mapping stays in Rust (single source of truth). Bounded memory (no whole-object
  buffer) — the streaming reader variant preserves RM0's bounded-RSS contract. Per-chunk AEAD auth intact,
  fail-closed metadata parse. N ranged opens per member (not multi-member single stream). The BUFFERED
  extract path + whole-object streaming path stay working (additive). No proto change beyond the covering-
  range query if one is added.

## Tests (verification member — REQUIRED, non-vacuous, no skip)
- **Correctness:** a ranged extract of a member from an AEAD object returns byte-identical plaintext to a
  whole-object extract + trim (fixture; plaintext + envelope-recipient forms).
- **Bounded RSS:** ranged-extracting a small member from a LARGE AEAD object stays O(covering-chunks), not
  O(object) — a test that would fail if it buffered the whole object.
- **Per-chunk auth / fail-closed:** a tampered covering chunk / tampered metadata prefix is rejected.
- **N-member bundle:** N ranged opens each read only their covering range (assert bytes read ≈ Σ member
  covering ranges, not N×object).
- Existing buffered `archive extract` ranged tests + whole-object `extract-stream` tests stay green.

## Definition of done (this repo's AGENTS.md)
`cargo build`+`cargo test`+`cargo fmt --check`+`cargo clippy --all-targets -- -D warnings` clean (paste
tallies); if a `sutradhara-rm3-3.patch` is emitted, note it (the operator applies + gates it). Summary:
files touched (real current lines); the reader/stream API; the covering-range query; each test → scope
item; explicit statement that (a) the mapping stays in Rust, (b) memory is bounded, (c) N ranged opens per
member (no multi-member single pass), (d) auth is fail-closed, (e) physical throughput acceptance is
deferred to the MSL3040 window. Do NOT implement the diag (RM3.2) or the app-restart contract (RM3.1b).
