# Codex prompt — rem S5b: byte-ranged `ReadObjectRange` over the daemon read session

> Design by Claude + the owner; implementation by codex. **Primary repo: `~/remanence`**
> (the gRPC read server). A small **shared contract** section below covers the
> `~/system` harness flip that consumes it. Read `CLAUDE.md` + `AGENTS.md` in each
> repo first. **Source of truth:** `~/system/docs/design-ingest-v2-rao-archive.md`
> (Part B restore) and `~/remanence/docs/design-rem-archive-object-format.md` §1.2
> req 3 (PFR by closed-form offset arithmetic).

## The gap (one sentence)
The Layer-5 gRPC `ReadSessionService.ReadObjectRange` RPC **returns
`unimplemented` for any non-zero byte range** — so the daemon read path (which
sutradhara's remanence backend and the harness use) can only read whole files,
and live per-asset partial restore from the rem copies falls back to the d2 shelf.

Evidence:
- `crates/remanence-api/src/lib.rs:1326` —
  `if request.start_byte != 0 || request.end_byte != 0 { return Err(Status::unimplemented("ranged reads are S5b")); }`
- `crates/remanence-api/src/lib.rs:5419` —
  `read_object_range_rejects_partial_range_in_s5a()` asserts the current
  (to-be-replaced) `Unimplemented` behavior.
- `~/system/scenarios/scenario_rao_archive.py:176` — the harness caches the
  just-written bytes for sutradhara's verifier because "rem gRPC ranged reads are
  not available for write-session locators yet"; restore by member name uses the
  d2 fallback (`scenario-registry.md` RAS: "rem ranged reads still bypassed during
  verification").

## What already exists — BUILD ON IT, do not rebuild
The ranged-read machinery is **proven end-to-end in the CLI**; S5b is wiring it
behind the daemon read session, not inventing it.

- **Format-layer PFR** — `crates/remanence-format/src/pfr.rs`:
  `read_encrypted_rao_file_range_to_vec(encrypted, root_key, first_chunk_lba,
  file_size_bytes, range_start, range_len) -> EncryptedRaoFileRange`, with
  `validate_file_range` (range past EOF / overflow / empty-range rules) and the
  plaintext path via `remanence_aead::open_plaintext_range_to_vec`. Exported from
  `remanence-format/src/lib.rs:36`.
- **Per-file offset metadata** in the manifest/catalog —
  `crates/remanence-format/src/model.rs:274` `first_chunk_lba: Option<BodyLba>`,
  `data_offset`, `pax_header_offset`; the catalog already persists
  `first_chunk_lba` per file (`crates/remanence-api/src/pool_write.rs:2092`,
  `lib.rs:3814`).
- **Proven CLI caller** — `crates/remanence-cli/src/lib.rs:5836` already does a
  real keyed/keyless blob single-file **ranged** extract using
  `read_encrypted_rao_file_range_to_vec` + `idx_entry.first_chunk_lba` /
  `blob.first_chunk_lba`. This is the reference for correctness.
- **Read core + drive actor** — `crates/remanence-api/src/read_core.rs`
  (`read_object_payload`, `CapturePayloadSink`) streams the whole payload via
  `stream_rem_tar_object_with_manifest_anchor`; `mount::read_file`
  (`crates/remanence-api/src/mount.rs:539`) dispatches a `DriveCommand::ReadFile`
  to the drive actor that owns the mounted tape. S5a (whole-file) flows through
  here today.
- **Proto contract is final, do not change it** — `proto/layer5.proto:863`
  `ReadObjectRangeRequest { session_id, object_id, file_id, start_byte, end_byte,
  stream_chunk_bytes }`, half-open `[start_byte, end_byte)`, both-zero = whole
  file (`proto/layer5.proto:867`). sutradhara already calls this RPC; the harness
  seam already has `read_range`.

## The work (rem, `~/remanence`)
Implement S5b: honor `[start_byte, end_byte)` in the server read path by
resolving `(object_id, file_id, range)` → `first_chunk_lba` + payload offset →
the existing format-layer PFR, streamed back as `BytesChunk`.

1. **Remove the guard** at `lib.rs:1326-1328`. When `file_id` is empty, keep
   today's object-level whole-payload behavior (S5a) for the both-zero range; a
   non-zero range with an empty `file_id` is still an error (a range is meaningful
   only **within one file** — return `InvalidArgument`, not `Unimplemented`).
2. **Resolve the file row** for `(object_id, file_id)` from the catalog: its
   `first_chunk_lba`, `size_bytes`, and the object's tape placement
   (`tape_file_number` / BodyLba origin) — the same rows the CLI ranged path and
   `pool_write` already read. Validate the requested range against `size_bytes`
   via the format layer's `validate_file_range` semantics (reuse, don't
   re-derive). Out-of-range → `InvalidArgument`/`OutOfRange` with a clear message;
   do not panic, do not silently clamp.
3. **Add a ranged drive-actor command** alongside `DriveCommand::ReadFile` (e.g.
   `ReadObjectRange { …, first_chunk_lba, range_start, range_len }`) and a
   `read_core` entry point parallel to `read_object_payload` that positions to the
   object tape file and feeds the existing format-layer range opener
   (`read_encrypted_rao_file_range_to_vec` for aead pools; the plaintext range
   opener for rao-plain). Stream the resulting bytes through the existing
   `BytesChunk` channel honoring `stream_chunk_bytes`. Reuse the format crate for
   the actual byte math — the server must not hand-roll offset arithmetic.
4. **Keys**: the aead path needs the `RootKey` (key epoch) the object was sealed
   under, resolved the same way the whole-object keyed read resolves it. Plain
   pools take the plaintext range opener. A range request for a keyed object with
   no available key → a clear `FailedPrecondition`, never partial/garbage bytes
   (design "never silent" §A1.2).
5. **Tests** (this is the DoD gate, not optional):
   - Replace `read_object_range_rejects_partial_range_in_s5a` with an S5b test
     that a partial range now **succeeds** and returns the exact expected bytes,
     byte-comparing against the same fixture the CLI ranged test uses — for both a
     **plain** and an **aead** object.
   - Range edge cases: `[0,0)` whole-file unchanged; a mid-file slice; a slice to
     EOF; an empty-but-valid range; past-EOF and overflow → typed error (no
     panic); non-zero range with empty `file_id` → `InvalidArgument`.
   - A daemon-level read-session round trip (open session → `ReadObjectRange` with
     a real range → bytes) if the existing read-session test harness supports it;
     otherwise an API-level test exercising `dispatch` + drive actor.
   - `cargo fmt --check`, `clippy -p remanence-api -D warnings`,
     `cargo test -p remanence-api -p remanence-format -p remanence-cli` all green;
     paste the counts.

Do **not** change the RAO/REM-PARITY wire format or the proto. This is a
read-server addition that reuses the format/catalog layers.

## Shared contract (system harness, `~/system`)
Once S5b lands and a daemon build exposes it, flip the harness off the fallback:
- `scenarios/scenario_rao_archive.py` — drop the `read_range` cache hack + the
  "rem gRPC ranged reads are not available …" note (lines ~174-205); route
  `read_range` through the real rem gRPC ranged read for `s-rao-work`
  (rao-plain-v1) and `s-rao-offsite` (rao-aead-v1) locators, and assert a
  **per-asset PFR restore pulls one member's bytes out of the rem copy directly**
  (not via d2). Keep the d2 shelf as the *designed* preference-fallback path, but
  the primary rem copies must now serve ranged restore.
- `docs/scenario-registry.md` RAS row — change "rem ranged reads still bypassed
  during verification" to reflect rem-native ranged restore once green.
- This half **halts cleanly as stub/env** where the live daemon lacks S5b (so it
  can land before the rem build ships), then goes green on the S5b daemon.

## Caveats
- **Clean-slate only:** scenario outcomes count from `make reset && make up`
  (`~/system/CLAUDE.md`).
- Keep all inputs well-formed/benign (the rem archive hardening Highs from the
  2026-06-15 review are closed — see that doc's status banner — but this prompt is
  not the place to re-test hostile ingest).
- If your runtime hits `bwrap: loopback: Failed RTM_NEWADDR`, **stop and report
  it** — do not flail.

## DoD
- rem: S5b implemented, the S5a-rejects-partial test replaced by passing S5b
  range tests (plain + aead), full `remanence-api`/`-format`/`-cli` suites green
  (paste output). Commit to `main` per `AGENTS.md`.
- system: `scenario_rao_archive` asserts rem-native ranged restore (or halts as
  stub/env where the S5b daemon is absent); registry + INDEX updated.
- Report: what the ranged path now proves vs the d2 fallback, and any range/key
  case you could not cover and why.
