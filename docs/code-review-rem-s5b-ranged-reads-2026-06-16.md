# Code review — rem S5b daemon ranged reads, 2026-06-16

**Scope:** the implementation of `docs/prompt-rem-s5b-ranged-reads.md` — single
commit `339d2b5` ("implement daemon ranged object reads"), ~1,137 insertions
across `remanence-api` (`lib.rs`, `mount.rs`, `pool_write.rs`, `read_core.rs`,
`write_owner.rs`), `remanence-format` (`pfr.rs`, `lib.rs`), and
`remanence-state` (`index.rs`, `lib.rs`).

**Reviewed against:** the prompt (work items 1–5, DoD) and
`design-rem-archive-object-format.md` §1.2 req 3 (PFR by closed-form offset
arithmetic) as normative.

**Method:** read the new format-layer planner, the read-core range streamer, the
drive-actor handler, the RPC guard replacement, and the catalog accessors +
schema line by line; traced the production write→catalog→read path; verified the
0-byte and non-regular edge cases against the new insert guard.

**Gates (all green):** `cargo fmt --check` clean; `clippy -p remanence-api
-p remanence-format -p remanence-cli --all-targets -D warnings` clean (exit 0);
`cargo test -p remanence-api -p remanence-format -p remanence-cli` **0 failed**
(format 14 vectors + lib suites, api, cli all pass).

## Verdict

**The plaintext ranged-read path is complete, correct, and clean — no
correctness defects in what shipped.** The one significant item is a **scope
gap, not a bug: the aead/encrypted ranged path is deliberately not implemented**
and is gated with a clear `FailedPrecondition`. That is defensible (see F1) but
diverges from the DoD, which asked for a passing **aead** range test and which
the `~/system` `s-rao-offsite` flip depends on. Codex also shipped **no report
and no journal entry**, so the deferral is currently undocumented.

What landed and is right:
- The RPC guard at `lib.rs:1326` is replaced correctly: empty `file_id` + both
  bytes zero keeps S5a whole-object behavior; empty `file_id` + non-zero range →
  `InvalidArgument` (not `Unimplemented`); a `file_id`-scoped request routes to
  the new ranged dispatch. `read_file` with a `file_id` now does a whole-**file**
  extract via the range path (range `[0,0)` → `[0,size)`), which is the
  file-scoped read the prompt wanted wired.
- Offset math lives in the format layer as required: `plan_plaintext_rao_file_range`
  reuses `validate_file_range` (EOF/overflow/empty-range), uses checked
  arithmetic throughout, and the server hand-rolls none of it.
- `read_core::read_plaintext_file_range` positions identically to the proven
  whole-object path (`source.space(tape_file_number, Filemarks)` after
  `verify_loaded_tape_identity`), then skips `first_body_lba` blocks and writes
  only the covering bytes. Proven byte-exact end-to-end by a real
  write→read→slice round-trip test (`payload[400..1100]`).
- Catalog plumbing is real and production-wired (see F-cleared notes): codex
  added the missing `object_files` table + accessors and the commit path
  populates it.

---

## F1 (Significant — scope/DoD) — aead ranged reads not implemented; gated, but undocumented and not DoD-complete

`write_owner.rs:1992`. For an encrypted copy, `stream_one_file_range` returns
`Status::failed_precondition("encrypted ranged reads require daemon key
material; no key resolver is configured")`. The plaintext path is fully built;
the aead path is a clean refusal.

**Why this is defensible, not a defect:** the daemon has **no read-side key
resolution at all**. `RootKey` appears in non-test daemon code only in
`pool_write.rs` (the seal/write path). The whole-object read path
(`stream_one_object`) streams raw on-tape bytes through the *plaintext*
rem-tar streamer (`stream_rem_tar_object_with_manifest_anchor`) with no
decryption — so the daemon cannot serve encrypted **whole-object** reads today
either. The prompt's step 4 ("resolve the key the same way the whole-object
keyed read resolves it") referenced a capability that does not exist. Building
it is a separate feature (key-epoch lookup + `RootKey` retrieval on the read
side), not the "wire the proven PFR" task this prompt scoped. Refusing with
`FailedPrecondition` rather than streaming ciphertext/garbage is the correct
"never silent" behavior (design §A1.2).

**But:** (a) the DoD explicitly requires "passing S5b range tests (plain **and**
aead)" — the aead half is not delivered; (b) the `~/system` shared-contract flip
wants `s-rao-offsite` (rao-aead-v1) served by rem ranged reads — unreachable
until daemon key resolution lands; (c) it is undocumented (F2).

**Recommendation:** accept the plaintext-only slice, but split daemon read-side
key resolution into its own work item (it unblocks both aead whole-object reads
*and* aead ranged reads), and record the deferral. This is a prompt-scoping
miss as much as an implementation one.

## F2 (Low) — no report and no journal entry

The prompt's DoD requires a report ("what the ranged path now proves vs the d2
fallback, and any range/key case you could not cover and why") and the project
convention is a dated `JOURNAL` entry per session. Neither was produced, so the
aead deferral (F1) is a silent divergence from the work order. **Fix:** journal
the slice and state the aead gap + reason explicitly.

## F3 (Low) — the encrypted→`FailedPrecondition` branch is untested

Given the new standing rule "a test never silently passes" (commit `acdcf79`)
and design §A1.2 "never silent," the clean refusal of an encrypted ranged read
should be asserted by a test. Nothing currently proves it; a future refactor
could regress it into silently streaming ciphertext as if plaintext.
`stream_one_file_range` can't be unit-tested without a `DriveHandle`, but the
representation gate (the `copy.representation == ENCRYPTED` check) could be
lifted into a small pure function and tested, or a daemon-level test added.
**Fix:** add a test that an encrypted copy's ranged read fails cleanly.

## F4 (Low) — no daemon-level real-bytes range test; matches the pre-existing S5a gap

Byte-exactness is proven only at the `read_core`/format layer. The RPC-level
test (`read_object_range_dispatches_file_scoped_range_to_drive_actor`) mocks the
drive actor with a hand-fed `drive_task`, so the real `stream_one_file_range`
(catalog resolve → plan → position → stream) is exercised by no test. This is
the **same** limitation as S5a (`stream_one_object` has no direct test either —
the test infra dispatches through a mocked drive), so it is not a regression and
is within the prompt's accepted fallback ("API-level test exercising dispatch +
drive actor"). Noted so it isn't mistaken for full daemon-path coverage. The
prompt's named edge cases land as: mid-file slice ✓ (read_core), empty-but-valid
range ✓ (planner), past-EOF ✓ (planner), non-zero range + empty `file_id` →
`InvalidArgument` ✓ (RPC). **Not covered:** slice-ending-exactly-at-EOF, an
arithmetic-overflow range, and a whole-file-via-range byte assertion.

## Nits

- `object_files_path_idx (object_id, path)` is created, but `get_file` by path
  does `list_native_object_files().into_iter().find(path == …)` — a full scan
  that ignores the index. Either query `where object_id=? and path=?` (uses the
  index) or drop the index. Also: `path` is not unique per object (PK is
  `(object_id, file_id)`); first-match is fine for RAO objects but worth a note.
- `requested_file_range` validates `end >= start` but defers EOF validation to
  the format layer, which runs after `write_config` has touched the drive. A
  past-EOF range therefore configures the drive before erroring — harmless (no
  tape motion), not worth reordering.

## Verified conformant (concerns investigated and cleared)

- **Production population:** `commit_pool_write` (`pool_write.rs:1348`) builds
  `file_projections` from `write_report.catalog.files` and passes them; the only
  other caller (`lib.rs:3260`) is a test helper. `object_files` is populated on
  the real write path, so `get_native_object_file` resolves for committed
  objects — the feature is functional outside tests.
- **New table, not a duplicate:** `object_files` did not exist before this
  commit (`git show 339d2b5~1` has no match). The prompt's claim that per-file
  `first_chunk_lba` was "already persisted" was inaccurate; codex correctly
  added the persistence rather than double-writing an existing table.
- **No commit-breaking insert guard:** the `object_files` insert requires a
  32-byte `file_sha256`. `FileCatalogProjection.file_sha256` is `[u8; 32]`
  (always present) on the daemon write path, so regular-file commits can't trip
  it. A 0-byte file projects as `first_chunk_lba=None, chunk_count=0`
  (`layout.rs:274`), matching the guard's `size==0` branch exactly — empty-file
  commits are safe. Non-regular entries (hardlinks/symlinks/dirs) are a
  CLI-archive concept and don't reach this single-file daemon write path.
- **Positioning:** the range path's filemark+block spacing matches the proven
  `read_object_payload` origin; `first_chunk_lba` is object-local (block 0 =
  start of the object tape file), confirmed by the in-memory round-trip test.
- **Value-add:** `ListFilesInObject` and `GetFile` (previously
  `unimplemented`) are now implemented on the shared accessors and tested
  (`catalog_lists_and_fetches_files_in_native_object`).

## Out of scope for this commit (rem repo only)

The `~/system` harness flip (drop the `read_range` cache hack in
`scenario_rao_archive.py`, assert rem-native ranged restore, update
`scenario-registry.md`) is not in this commit. The prompt allows it to land
separately and "halt cleanly as stub/env" until the S5b daemon ships; the aead
half of that flip is additionally blocked by F1.
