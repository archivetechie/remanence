# Codex prompt — rem S5b correction: opaque object/payload-relative ranged reads

> Design by Claude + the owner; implementation by codex. **Primary repo: `~/remanence`**
> (the gRPC read server). A **shared contract** section covers the `~/system`
> harness flip that consumes it. Read `CLAUDE.md` + `AGENTS.md` in each repo
> first. This **supersedes the read-contract decision** in
> `docs/prompt-rem-s5b-ranged-reads.md` (implemented in commit `339d2b5`,
> reviewed in `docs/code-review-rem-s5b-ranged-reads-2026-06-16.md`). The S5b
> implementation is correct Rust but solved the wrong contract; this corrects it.

## Why this exists — a cross-repo contract mismatch the S5b review surfaced

S5b made the daemon's `ReadObjectRange` a **file-relative, `file_id`-required,
format-aware member read** (per the proto comment at the time). But the actual
gRPC client (sutradhara) calls it the opposite way, and the deployment's
encryption design forbids the S5b model for encrypted objects. Concretely:

- **What sutradhara sends** (`/home/user/sutradhara/src/sutradhara/backend/remanence.py:410`):
  `ReadObjectRangeRequest { session_id, object_id, start_byte, end_byte }` with
  **`file_id` empty** and **object/payload-relative** byte offsets. It reads the
  RAO envelope header (`[0,8)`, `[8,8+header_len)`) and chunks as raw byte ranges
  and does all member-offset math + **decryption client-side**
  (`archive_fanout.py:786` `start = first_chunk_lba * RAO_CHUNK_SIZE`;
  `archive_restore.py:450-493`).
- **What S5b does**: empty `file_id` + non-zero range →
  `InvalidArgument("file_id is required for ranged reads")`
  (`crates/remanence-api/src/lib.rs`, the `read_object_range` RPC). So the exact
  call the client makes is rejected; the harness flip off the d2 fallback would
  fail despite S5b's own tests passing.

**The deployment is "client-side crypto, opaque daemon":** sutradhara seals the
`.rao` **client-side via the rem CLI** (`rem archive build --encrypt --key-file
--key-id`, key from sutradhara's own `KeyRegistry`), streams the **already-sealed
bytes** to the daemon, and on read fetches opaque bytes back and **unseals
client-side**. The daemon never receives a key, a key_id, or a representation
flag on write or read. This is the strongest key posture: **the host that holds
the tapes never holds the keys.** A daemon-side key resolver / daemon sealing
would fight this and is explicitly **out of scope** (see "Do NOT").

## The corrected model (what to build)

`ReadObjectRange` is an **opaque, key-agnostic, representation-agnostic byte/
payload-range server**. Every daemon object is `rem-tar{ manifest, payload = S }`
with **exactly one payload member** (verified: `prepare_pool_object` →
`vec![prepare_regular_file(source_path, …)]` at
`crates/remanence-api/src/pool_write.rs:1606-1624`; `AppendObject` spools all
chunks to one file at `crates/remanence-api/src/write_owner.rs:404,789`). `S` is
the client's whole `.rao` (plaintext rem-tar **or** an encrypted `RAO1`
envelope — the daemon neither knows nor cares).

- **`file_id` empty** → resolve to the object's **sole payload member** and serve
  the requested **payload-relative** byte range `[start_byte, end_byte)` of `S`.
  This is the storage primitive sutradhara and the published-format ecosystem
  need. The daemon serves these bytes whether `S` is plaintext or encrypted —
  no key, no decryption.
- **`file_id` set** → keep S5b's member-scoped read (resolves the member via the
  catalog and serves its byte range). Today this is degenerate (one member, the
  payload), so it returns the same bytes as the empty-`file_id` form; it remains
  the natural surface if daemon objects ever carry multiple direct members. This
  is the "smart standalone daemon" face — **keep it, don't delete it.**
- Both forms: half-open `[start_byte, end_byte)`; both-zero = whole payload.

Member extraction *of the customer's inner files* and any decryption stay where
they already live and work: the **rem CLI** (`archive read`/`extract`/`restore`,
keyed via `--key-file`) for direct/standalone tape access, and the **client**
(sutradhara) for the gRPC path. The daemon is not in the crypto path.

## The work (rem, `~/remanence`)

1. **Accept empty `file_id` ranged reads.** In the `read_object_range` RPC
   (`crates/remanence-api/src/lib.rs`), remove the
   `InvalidArgument("file_id is required for ranged reads")` guard. When
   `file_id` is empty, resolve the object's **single** `object_files` row
   (`list_native_object_files` returns exactly one for daemon-written objects;
   if zero or more than one, that's `FailedPrecondition`/`Internal` with a clear
   message — do not guess) and dispatch the range against that member. Keep the
   both-zero whole-payload behavior unchanged. Keep the `file_id`-set path
   (member-scoped) working as-is.
2. **Serve payload-relative ranges through the existing engine.** The drive-actor
   `stream_one_file_range` + `read_core::read_plaintext_file_range` +
   `plan_plaintext_rao_file_range` already serve a member's byte range from its
   catalog `first_chunk_lba` — that member is `S`, so its byte range **is** the
   object/payload-relative range the client asked for. Reuse it; do not re-derive
   offset math. Confirm the daemon's per-member `first_chunk_lba` (offset of `S`
   within the wrapping object `O`) makes payload offset `k` map to the correct
   tape block — i.e. client `start_byte=0` returns the first byte of `S` (the
   `RAO1` magic / plaintext tar header), not `O`'s manifest.
3. **Neutralize the encrypted-`FailedPrecondition` branch.** In
   `stream_one_file_range` (`crates/remanence-api/src/write_owner.rs:1992`) the
   `copy.representation == OBJECT_COPY_REPRESENTATION_ENCRYPTED` →
   `FailedPrecondition` branch is **dead code** for daemon-written objects (the
   write path always records `plaintext`) and encodes the wrong mental model
   (that the daemon would decrypt if it had a key). Remove it. Replace with a
   short comment: ranged reads serve opaque payload bytes; decryption is a client
   responsibility; the daemon never holds keys. (If you keep any representation
   check, it must not imply daemon decryption.)
4. **Fix the proto contract comment** (`proto/layer5.proto:823-826` and the
   `ReadObjectRangeRequest` field comments `:865-872`). No field/wire change.
   Document the two modes: empty `file_id` = object/payload-relative range over
   the object's payload; set `file_id` = member-relative range resolved via the
   catalog; both half-open, both-zero = whole. State that the daemon serves
   opaque bytes and performs no decryption. The proto comment is the cross-repo
   source of truth — make it unambiguous so this mismatch can't recur.
5. **Whole-payload parity.** Confirm `ReadObjectRange(file_id empty, 0, 0)` and
   `ReadFile(file_id empty)` still return the whole payload `S` (they flow
   through `stream_one_object` today). Keep behavior; add a byte assertion if not
   covered.
6. **Tests** (DoD gate):
   - **Empty-`file_id` object-range read** (the client's actual call): a daemon-
     level test that an empty-`file_id`, object-relative `[start,end)` request
     returns exactly `S[start:end]`, byte-compared. Cover: a mid-payload slice, a
     slice to EOF, an empty-but-valid range, both-zero = whole payload.
   - **F3 reframed — encrypted payload served key-free.** Build an object whose
     payload `S` is an encrypted `RAO1` envelope (seal `S` with a test key, write
     it as the daemon object payload). Assert the daemon serves a payload byte
     range of `S` **with no key**, and that a **client-side** decrypt of the
     returned range (using the test key + the format-layer
     `read_encrypted_rao_file_range_to_vec`/`open_to_vec`) yields the expected
     plaintext. This proves the opaque path end-to-end and that the daemon needs
     no key. (Replaces the S5b "encrypted → refusal" expectation.)
   - **F4 — real-bytes daemon-path range test.** Exercise `stream_one_file_range`
     against a fake/real drive source with actual bytes (not only the mocked-
     drive dispatch test), so the catalog→plan→position→stream path is covered.
   - Past-EOF / overflow range → typed error (no panic), surfaced as
     `InvalidArgument`/`OutOfRange`.
   - `cargo fmt --check`, `clippy -p remanence-api -p remanence-format
     -p remanence-cli --all-targets -D warnings`, and
     `cargo test -p remanence-api -p remanence-format -p remanence-cli` all
     green; paste counts. **Rebuild `target/release` after changes** (harness
     freshness guard).
7. **F2 — journal + report.** Append a dated `journal/` entry and a short report:
   what the opaque ranged path proves, that the daemon stays key-free, and any
   case not covered and why. (Per `AGENTS.md`: a test never silently passes.)

## Shared contract (system harness, `~/system`)

Now actually achievable for **both** pools, because the daemon serves opaque
payload ranges regardless of representation:
- `scenarios/scenario_rao_archive.py` — drop the `_CliRangeReadRemBackend` cache
  hack + the "rem gRPC ranged reads are not available …" note (lines ~166-227);
  route `read_range` through the real rem gRPC `ReadObjectRange` for **both**
  `s-rao-work` (rao-plain-v1) and `s-rao-offsite` (rao-aead-v1) locators. Assert
  a **per-asset PFR restore pulls one member's bytes from the rem copy
  directly** — the client computes the payload byte range (`first_chunk_lba *
  RAO_CHUNK_SIZE`), the daemon serves opaque bytes, and for `s-rao-offsite` the
  client decrypts via its `KeyRegistry`. Keep the d2 shelf as the designed
  preference-fallback, but the rem copies must now serve ranged restore.
- `docs/scenario-registry.md` RAS row — change "rem ranged reads still bypassed
  during verification" to reflect rem-native opaque ranged restore once green.
- Halt cleanly as stub/env where the live daemon lacks this build; go green on
  the corrected daemon.

## Do NOT

- Do **not** add a daemon key store, key resolver, key config, or any key field
  to the proto. The daemon never holds or receives keys. (If a future deployment
  ever wants daemon-side decryption, that is a separate, opt-in `KeyResolver`
  work item — not this one.)
- Do **not** make the daemon seal or decrypt. Encryption stays client-side (rem
  CLI for standalone; sutradhara for the gRPC path).
- Do **not** change the RAO/REM-PARITY wire format or proto fields. This is a
  read-server semantics correction plus a comment fix.
- Do **not** remove the `file_id`-set member-scoped path, `ListFilesInObject`,
  `GetFile`, the `object_files` table, or the format-layer planner — they are
  correct and reused.

## DoD

- rem: empty-`file_id` opaque payload-range reads work and match what sutradhara
  sends; the dead encrypted-refusal branch removed; proto comment corrected;
  member-scoped + whole-payload paths preserved; tests above green (paste
  output); `target/release` rebuilt. Commit to `main` per `AGENTS.md`.
- system: `scenario_rao_archive` asserts rem-native opaque ranged restore for
  both plain and aead pools (or halts as stub/env where the daemon is absent);
  registry + INDEX updated.
- Report: what the opaque ranged path proves vs the d2 fallback, confirmation the
  daemon stays key-free, and any case not covered and why.
