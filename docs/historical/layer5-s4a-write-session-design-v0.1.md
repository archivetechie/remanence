# Layer 5 S4 — Write Path Descoping + S4a (Single-Object Write Session) Design v0.1

Status: design decision + S4 descoping record. S1 (read-only catalog server) is
live and proven end-to-end (scenario D reconciles Sutradhara's catalog over gRPC;
locators match the CLI-write path across transports). This doc descopes the
**write path** (decomposition's S4 — `WriteSessionService` server) into sub-slices
and designs the anchor, **S4a**. Grounds in `proto/layer5.proto`
(`WriteSessionService`), `remanence-api` (`WriteSessionApi`, currently an
in-memory mock), and `remanence-api::pool_write` (`write_to_selected_tape`, the
shared write core the CLI break-glass `rem-debug archive write` already uses).

## Background — what exists

- **Proto contract** (`proto/layer5.proto`): `OpenWriteSession`, `AppendObject`
  (client-stream `Start`→`Chunk*`→`Finish` → returns `ObjectRecord`),
  `CheckpointSession`, `CloseWriteSession`, `AbortWriteSession`, `GetWriteSession`.
  Targets: `DriveTarget` / `TapeTarget` / `TapePoolTarget`. Sessions model rollover
  (`tape_sequence`/`current_tape_index`) and recovery (`ORPHANED`/`LOST`,
  `recover_session_id`).
- **Write core** (`pool_write.rs`): `write_to_selected_tape(&mut CatalogIndex,
  &mut dyn BlockSink, pool_cfg, request, selected)` writes one object to an already
  selected+mounted tape (rem-tar-v1 + parity, commit, locator) and returns
  `PoolWriteResult` → `to_proto()` → `pb::ObjectRecord`. `WriteObjectToPoolRequest`
  reads from a `source_path` file (two-pass: pass-1 hash builds the manifest,
  pass-2 streams the body to tape).
- **Gap**: `WriteSessionApi` is scaffolded against an in-memory store; `AppendObject`
  returns `unimplemented` ("append without a pool-targeted write session is not
  wired"). It is **not** connected to the real core, has no single-writer
  ownership, and buffers (Phase-1 review flag).

## S4 descoping (sub-slices)

- **S4a — Single-object write session over the daemon** *(anchor, this doc)*:
  `OpenWriteSession`(pool target → select+mount, pin to session) + `AppendObject`
  (stream → bounded private spool → real `write_to_selected_tape`) +
  `Close`/`Abort`/`GetWriteSession`, behind a **single writer thread**. Returns the
  canonical locator. End-to-end, harness-drivable.
- **S4b — Multi-object + durability**: `CheckpointSession`, idempotency-key dedup,
  `ORPHANED`/`LOST` recovery (`recover_session_id`), multi-object atomicity.
- **S4c — Async/OperationRef** (only if long writes need progress/cancel; proto
  `AppendObject` is inline, so this is a separate surface + needs S3).
- **S4d — Rollover + reservation**: `tape_sequence`/`current_tape_index`, per-pool
  tape reservation, reservation-rebuild-on-restart, multi-drive concurrency.
- **S4e — Capacity/eligibility hardening**: real capacity vs the `1_000_000`-block
  placeholder (`pool_write.rs:1463`), select-before-write no-spanning preflight
  (spec §1058-1063).

**Anchor = S4a.** Minimal coherent "write via the daemon"; shares the exact core
the CLI uses (so locators match across transports — the parity the read path
proved); establishes the single-writer foundation every later slice builds on.

## S4a architecture — the single writer thread

A `DriveHandle` borrows its `LibraryHandle` for its lifetime, and a session holds
that drive across multiple RPCs (Open→Append→Close). Storing `{LibraryHandle,
DriveHandle}` in a session struct would be **self-referential** (a borrow-checker
dead end). **Global single-writer dissolves this**: one dedicated **writer thread**
holds `library` + `drive` as **stack locals** for the session's duration.

- At daemon startup, spawn **one writer thread** owning the mutable
  `CatalogIndex`/`StateHandle` (`Send`, not `Sync` — moves onto the thread), the
  `DiscoveryReport`, and the allowlist policy. `ApiState` holds a
  `tokio::sync::mpsc::Sender<WriteCommand>` (Send+Sync → `ApiState` stays a valid
  tonic service).
- Thread loop (`blocking_recv`): idle until `Open` → `select_tape_in_pool`
  (catalog-only) → mount via the relocated `load_tape_by_uuid` → hold `library`
  + `drive` as **locals**, run an inner command loop (`AppendFinish`/`Close`/
  `Abort`/`Get`) for that one session → drop on Close/Abort. A second `Open` while
  active → `FAILED_PRECONDITION` (single-writer). `GetWriteSession` answered in
  either state.
- Each command carries a `tokio::sync::oneshot` reply. The synchronous tape write
  (`write_to_selected_tape`) runs **on this thread**, never the async executor.

**Channel choice (verified):** `tokio::sync::mpsc` (its `Sender` is `Send + Sync`,
keeping `ApiState` `Sync`; the writer thread consumes via `blocking_recv()`, no
runtime needed) + `tokio::sync::oneshot` replies. **Not** `std::sync::mpsc` (its
`Sender` Sync-ness is fragile and would risk making `ApiState` `!Sync`).

## Mount-bridge relocation (required)

`load_tape_by_uuid` (resolve voltag → load slot → open drive) currently lives in
`remanence-cli::pool_ops`. The owner thread in `remanence-api` cannot reach it, so
S4a **moves it down to `remanence-api`** (e.g. a `mount`/`write_owner` module). The
CLI's `archive write`/`read`/`verify` then call `remanence_api::load_tape_by_uuid`
— one shared mount bridge for both transports.

## Daemon now needs hardware

Unlike the S1 read-only daemon (catalog only, no hardware), the write daemon must
`discover()` libraries at startup and run with `cap_sys_rawio` + the `tape` group +
an `--allow <serial>` allowlist (as `rem-debug` does). The owner thread holds the
`DiscoveryReport` + policy; `OpenWriteSession` opens the `LibraryHandle` on demand.

## Streaming + bounded private spool

The async `AppendObject` handler (tonic executor) receives `Start`→`Chunk*`→
`Finish`, appending chunks to a **bounded, private spool** (file `create_new` in a
`0700` daemon-owned dir; size capped by `declared_size_bytes` or a config max —
exceed ⇒ `RESOURCE_EXHAUSTED`; never whole-object-in-memory). On `Finish` it sends
`AppendFinish{spool_path, expected_content_sha256}` to the writer thread and awaits
the `ObjectRecord`. The core's two-pass verify reads the spool; the spool is
unlinked after (success or abort). The spool is **required** (the core needs a
re-readable `source_path`); S4a's fix vs the Phase-1 flag is *bounded + private*,
not eliminating it.

## Session lifecycle

`Open(pool_target, mount_if_needed)` → `OPEN` + selected/mounted tape pinned.
`AppendObject` (one object: stream → spool → `write_to_selected_tape` →
`ObjectRecord`, inline). `Close` → `CLOSED`. `Abort` → discard spool, `ABORTED`.
`Get` → state. **Atomic unit = one object** (the core commits one object: rem-tar
+ parity + catalog projection + locator). Abort cleans the spool; a partial object
is the core's existing failure path (no new partial-commit semantics in S4a).

## `expected_content_sha256` — verify-before-tape (folded in)

`AppendObjectFinish.expected_content_sha256`, when supplied, is honored as a
**verification**: thread it as an optional field into `WriteObjectToPoolRequest`;
the core compares it to the pass-1 computed `content_sha256` **after prepare and
before the pass-2 tape write**, returning `PoolWriteError::ContentHashMismatch`
(→ `FAILED_PRECONDITION`) on mismatch, so a corrupt/mismatched object **never
reaches tape**. The CLI write path gets the same field (passes `None` today). This
banks an end-to-end integrity check immediately and exercises the proto field the
Sutradhara client will use.

### Future (consumer contract, not built in S4a)

A client-supplied **origin manifest** (camera-card offload, MHL/ASC-MHL style)
carried through ingest → Sutradhara lets a later slice *skip the compute pass*
(hash-on-receive + build the leading manifest from the supplied hash). Pinned
preconditions for the consumer side: the manifest must carry **SHA-256**
(Remanence's `content_sha256` is SHA-256-typed; xxhash/MD5 won't match), and for a
single-file object `content_sha256` == the raw file's SHA-256 (so the per-file
origin hash *is* the object hash). S4a does the verification only; the
skip-the-pass optimization and multi-file object-hash definition are out of scope.

## Locator parity (load-bearing)

`AppendObject` returns `PoolWriteResult.to_proto()` → `pb::ObjectRecord`, the **same
projection the CLI write path emits** (`object_id`/`content_sha256` hex; per-copy
`tape_uuid`/`tape_file_number`/`first_body_lba`/`pool_id`). A daemon-written object
and the catalog's later read/scrub agree by `locator_key` — the cross-transport
parity the read path validated. Shared core ⇒ holds by construction.

## Error taxonomy (gRPC Status)

`invalid_argument` (missing `session_id`; empty pool; unsupported target kind),
`failed_precondition` (a session already active; tape identity mismatch; not
writable; `expected_content_sha256` mismatch), `resource_exhausted` (spool size
cap; no eligible/writable tape in pool), `not_found` (unknown `session_id`),
`internal` (SCSI/IO/commit). `PoolWriteError`/`SelectTapeError` map to these — no
swallowing.

## Pinned contract for the Sutradhara write client (the consumer)

- **Open:** `OpenWriteSessionRequest{ pool_target: TapePoolTarget{ pool_id,
  library_uuid?, mount_if_needed=true }, body_format:"rem-tar-v1" }`.
  `drive_target`/`tape_target`/`recover_session_id` → `unimplemented` in S4a.
  Non-empty `idempotency_key` → `unimplemented` until S4b can replay the
  original operation instead of silently accepting an unenforced retry key.
- **Append:** `AppendObjectStart{ session_id, caller_object_id, caller_metadata,
  declared_size_bytes }` → `AppendObjectChunk{ session_id, data }*` →
  `AppendObjectFinish{ session_id, expected_content_sha256? }`. `body_format_manifest`
  ignored in S4a. In-object `archive_path` = `caller_metadata["path"]` if present,
  else a default from `caller_object_id`.
- **Returns:** `ObjectRecord` (canonical locator). **Close/Abort/Get** as above.
  `CheckpointSession` → `unimplemented` (S4b).
- **Concurrency:** at most one active write session daemon-wide; a concurrent
  `OpenWriteSession` returns `FAILED_PRECONDITION`. The client serializes its writes
  (or retries on that status).

## Scope

**IN (S4a):** the writer thread + `tokio::mpsc`/`oneshot` channel; relocate
`load_tape_by_uuid` to `remanence-api`; `Open`(pool)/`AppendObject`/`Close`/`Abort`/
`Get`; bounded private spool; real-core wiring; `expected_content_sha256`
verify-before-tape (core field); locator parity; error taxonomy; daemon hardware
bring-up (`discover` + register `WriteSessionService`). **OUT:** checkpoint /
multi-object atomicity / idempotency-dedup / resume (S4b); OperationRef/cancel
(S4c); rollover/reservation/multi-drive (S4d); real capacity + no-spanning preflight
(S4e); `drive`/`tape` targets; the client-supplied-origin-manifest optimization.

## Testing / acceptance criteria

1. **Unit:** writer-thread state machine (Open→Append→Close; reject second Open →
   `FAILED_PRECONDITION`; Abort cleans the spool); spool size-cap →
   `RESOURCE_EXHAUSTED`; `expected_content_sha256` mismatch → `FAILED_PRECONDITION`
   with **no tape write**.
2. **Integration (no hardware):** in-process daemon + a `VecBlockSink`-backed fake
   drive — Open(pool) → Append(one object) → `ObjectRecord` whose
   `object_id`/`content_sha256`/`tape_uuid` match what the catalog then returns by
   `locator_key`.
3. **Harness e2e (consumer side):** a "write via the Layer 5 daemon" scenario —
   Sutradhara write client opens a pool session, streams a file, gets a locator; a
   subsequent `Catalog.EnumerateObjects`/read finds it by the same `locator_key`;
   and the locator is byte-identical to what `rem-debug archive write` produces for
   the same input (cross-transport parity).
- Gates: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test`.

## §verification — Rust design verification

Verified against `cargo check -p remanence-api` + `cargo clippy -p remanence-api
--all-targets -- -D warnings` (both clean) on 2026-06-02, with a real-body skeleton
(`crates/remanence-api/src/write_owner_skeleton.rs`: `WriteCommand`, `spawn_writer`,
`writer_loop` with the nested-session borrow structure, the `mount<'a>` bridge stub,
and Send/Sync assertions). Removed after verification (design-only); the plan
recreates it. Checked against current HEAD (post S1).

Five-category result:
1. **Module privacy** — the owner thread + `WriteSessionApi` live with `ApiState`/
   `pool_write` in `remanence-api`. **Finding:** `load_tape_by_uuid` must move from
   `remanence-cli` to `remanence-api` (the owner thread can't reach a cli symbol).
   After relocation, all access is intra-crate or `pub`. Pass.
2. **`!Send` in threading** — verified: `CatalogIndex` (Send/!Sync),
   `DiscoveryReport`, `StaticAllowlist` move onto the writer thread; `WriteCommand`
   is Send; `tokio::mpsc::Sender<WriteCommand>` is Send+Sync (so `ApiState` stays a
   tonic service); the writer uses `blocking_recv()` and the synchronous tape write
   runs on that thread. Pass.
3. **Reactor timing** — gRPC handlers async on tonic's runtime; the writer is a
   plain `std::thread` (no reactor); no off-runtime fd construction. Pass.
4. **Borrowed-handle plumbing** — verified: `DriveHandle<'a>` borrowing a local
   `LibraryHandle` is held across the per-session `blocking_recv` loop, alongside
   `&mut CatalogIndex`, with no self-referential struct (global single-writer makes
   the stack-local pattern possible). Pass.
5. **Trait/method visibility traps** — `pb`/`ApiState`/service types reachable; the
   mount-bridge relocation resolves the one cross-crate gap. Pass.
