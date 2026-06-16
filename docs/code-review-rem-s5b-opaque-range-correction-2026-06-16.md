# Code review — rem S5b opaque-range correction, 2026-06-16

**Scope:** the implementation of `docs/prompt-rem-s5b-opaque-range-correction.md`
— commit `78df4ec` ("rem-api: correct S5b opaque range reads"), rem side; plus
the `~/system` harness flip in commit `094d1fe` ("system: verify rem-native RAO
archive restore").

**Reviewed against:** the correction prompt as normative (opaque,
payload-relative, key-free ranged reads), and the prior S5b review
(`code-review-rem-s5b-ranged-reads-2026-06-16.md`) whose F1–F4 this closes.

**Method:** read the RPC routing, the refactored drive-actor range path, the
proto comment, and all new tests line by line; traced the empty-`file_id` →
sole-payload-member resolution; confirmed the daemon takes no key dependency;
verified the harness flip removed the CLI cache hack and routes real gRPC.

**Gates (green):** `cargo fmt --check` clean; `clippy -p remanence-api
-p remanence-format -p remanence-cli --all-targets -D warnings` clean;
`cargo test -p remanence-api -p remanence-format -p remanence-cli` exit 0 /
0 failed (`remanence-api` lib: **167 passed**).

## Verdict

**Clean — faithful to the corrected contract, no findings.** The range path is
now an opaque, representation-agnostic, key-free payload-byte server that matches
what sutradhara actually calls, and the daemon takes no key dependency. The good
parts of the original S5b (member-scoped path, `object_files`, planner,
`ListFilesInObject`/`GetFile`) are preserved as the prompt required.

## Closed correctly

- **Core fix — empty `file_id` ranged reads** (`lib.rs:1366`): empty `file_id` +
  non-zero range now dispatches `ReadObjectRange` (was `InvalidArgument`); empty
  `file_id` + `0,0` routes to the whole-payload path; non-empty `file_id` keeps
  the member-scoped path. `resolve_object_file_for_range`
  (`write_owner.rs`) resolves empty `file_id` to the object's **sole**
  `object_files` row (`[file]` → ok; `0` or `>1` → clear `FailedPrecondition`).
  Matches the verified one-payload-member-per-object invariant.
- **Dead encrypted branch removed** (`write_owner.rs:1992` in the old revision):
  the `representation == ENCRYPTED → FailedPrecondition` and `!= PLAINTEXT`
  branches and their imports are gone. The range path no longer consults
  representation; a comment states the daemon serves opaque bytes and holds no
  keys. (The `copy` lookup stays — it still pins the object to the session tape.)
- **Proto comment corrected** (`layer5.proto:823`, `:866`): documents both modes
  (empty `file_id` = payload-relative; non-empty = member-relative), both-zero =
  whole, and "the daemon does not decrypt or interpret payload bytes; clients
  perform any decryption." No field/wire change. This is the cross-repo contract
  source of truth that originally misled the implementation — now unambiguous.
- **Testability refactor:** `stream_one_file_range` is split into
  `file_range_read_request` (pure catalog → request) and
  `stream_file_range_from_source` (`BlockSource` → bytes). This closes the prior
  review's F4 — the catalog→plan→stream path is now exercised against a real
  `VecBlockSource` with actual bytes, not only a mocked-drive dispatch.
- **F3 reframed and proven** (`encrypted_payload_is_served_opaque_and_decrypted_client_side`):
  a real `RAO1` envelope is stored as payload `S`, the daemon serves its byte
  ranges **with no key** (header `[0,64)` and whole payload byte-exact), and a
  **client-side** `read_encrypted_rao_file_range_to_vec` with the test key
  decrypts a member range. This is the end-to-end proof that the daemon needs no
  key material — the correct replacement for the old "encrypted → refusal" test.
- **Payload-relative real bytes** (`empty_file_id_ranges_are_payload_relative_real_bytes`):
  mid-slice, slice-to-EOF, empty-but-valid, and whole-payload all byte-exact vs
  `payload[..]` through the real block source — and proves offset 0 maps to the
  first byte of `S`, not the wrapping object's manifest.
- **Superset preserved** (`member_scoped_ranges_still_resolve_file_id`):
  non-empty `file_id` member reads still work.
- **Typed errors** (`invalid_payload_ranges_return_typed_status`): past-EOF,
  huge-offset, and reversed ranges return `InvalidArgument` (no panic).
- **RPC dispatch tests:** empty-`file_id` range → `ReadObjectRange`;
  empty-`file_id` `0,0` → `ReadFile` whole-payload.
- **Key-free build:** the only dependency added is a **dev-dependency**
  (`tempfile`); no crypto enters the daemon's runtime dependency set.
- **F2:** codex shipped a report (`report-rem-s5b-opaque-range-correction-2026-06-16.md`)
  and a journal entry.

## System half (`~/system` `094d1fe`)

The harness flip is real, not just claimed: `scenario_rao_archive.py` lost 116
lines — the `_CliRangeReadRemBackend` cache hack and the "rem gRPC ranged reads
are not available" note are gone; `read_range` now routes real gRPC for **both**
`s-rao-work` (plain) and `s-rao-offsite` (aead). `scenario-registry.md` RAS row
and `GAPBOARD.md` updated. codex's report states a clean-slate
(`make reset && make up`) pass on 2026-06-16. (Live re-run not repeated here;
hardware-dependent.)

## Nits (no action needed)

- `invalid_payload_ranges_return_typed_status` exercises the `u64::MAX-1 ..
  u64::MAX` case as a past-EOF rejection rather than literal addition overflow
  (the offsets exceed a 13-byte payload, so `validate_file_range` rejects on EOF
  first). The typed `InvalidArgument` assertion is still correct; only the
  comment's "arithmetic overflow" framing is slightly off.

## Net

The original S5b solved the proto's stated contract; this correction realigns it
to the deployed client and the key-off-the-tape-host architecture, makes the
daemon path unit-testable, and proves the encrypted case is served key-free with
client-side decryption. Remanence remains standalone (the CLI seals/opens with a
32-byte `--key-file`) while the daemon stays a key-free byte/payload-range
server. Ready.
