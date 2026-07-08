# Layer 5 S5a — Read Session (whole-object read over the daemon) Design v0.1

Status: design decision. S1 (catalog) + S4a (write) are live. S5a is the read
counterpart — stream an object's bytes off tape over the daemon, the production
sequel to the A.9/B.7 `rem-debug archive read`/`verify` break-glass path. Grounds
in `proto/layer5.proto` (`ReadSessionService`), `remanence-api` (`ReadSessionApi`,
an in-memory mock; `write_owner.rs`, the S4a drive owner), and the A.9 read core
(`stream_tape_object` + `CapturePayloadSink` in `remanence-cli/pool_ops`).

## Background

`ReadSessionService` (proto): `OpenReadSession`(drive/tape target) → `ReadObjectRange`
(ranged) / `ReadFile` → `stream BytesChunk`; `Close`/`Get`. `ReadSessionApi` today
reads from an in-memory `store.objects` mock ("object payload is not resident in
this api process") — never tape. The streaming response shape (`BytesChunkStream`
= `ReceiverStream`) already exists (used by `Catalog.EnumerateObjects`). S4a left a
single drive owner thread (`write_owner`, `WriteCommand`/`writer_loop`) that holds
the drive across a session.

## Architecture — generalize S4a's owner into a drive-session owner

The drive is the serialized resource (and Layer 2 enforces one open `DriveHandle`
per library — see *Concurrency* below). So S4a's writer thread becomes a
**drive-session owner** serving read *or* write, one active session at a time.
This is a rename + new match arms, not a rewrite of the write logic:

- `WriteCommand` → `SessionCommand`, gaining read arms: `OpenRead{tape_uuid, reply}`,
  `ReadFile{object_id, file_id, chunk_tx}`, `CloseRead{reply}`, `GetRead{reply}`.
- The owner's outer loop: `Open(write)` → existing write inner loop; `OpenRead` →
  mount + a **read** inner loop. A second `Open*` while active → `FAILED_PRECONDITION`.
- The read inner loop holds the mounted `DriveHandle` as a **stack local** across
  `blocking_recv` (the same pattern S4a verified), serving `ReadFile` calls.

## Read-core relocation + factoring

A.9's `stream_tape_object` does *mount + read*. A session mounts once at
`OpenReadSession` and reads per `ReadFile` on the open drive, so the inner
"**read one object's payload into a sink**" core is factored out and moved
`remanence-cli/pool_ops → remanence-api`: resolve the object's copy on the pinned
tape (`find_native_object_copies` → the copy whose `tape_uuid` matches the session;
`list_tape_files` → `block_count`) → `space`/`locate` → `stream_rem_tar_object`
into a sink. A.9/B.7 (`run_archive_read`/`verify`) become *mount + that core*; S5's
`ReadFile` = *that core on the session's drive*. `CapturePayloadSink<W: Write>`
moves too (it's already generic over the writer). One shared read path → bytes and
locators agree across CLI and daemon (the parity the read path proved).

## Streaming bytes back — `ChannelWriter`

`ReadFile`/`ReadObjectRange` return `stream BytesChunk`. The owner thread streams
via the existing `CapturePayloadSink<W>` with **`W = ChannelWriter`** — a
`std::io::Write` adapter that frames bytes into `pb::BytesChunk{data, is_last}` and
**`blocking_send`s** them over a `tokio::mpsc::Sender<Result<BytesChunk, Status>>`
(backpressure). The async handler returns `ReceiverStream` from the matching
receiver (the `EnumerateObjects` pattern). No new sink type — A.9's sink reused
with a channel writer. A final empty `is_last=true` chunk closes the stream;
mid-stream failures surface as `Err(Status)` items.

## Session lifecycle + read path

`OpenReadSession(tape_target{tape_uuid, mount_if_needed=true})` → owner
`load_tape_by_uuid` + identity-verify → `ReadSession{OPEN, tape_uuid}`.
`ReadFile(session_id, object_id, file_id)` → owner resolves the object's copy on
the pinned tape, positions, and streams the **single non-manifest payload entry**
(as A.9 does) as `BytesChunk`. `ReadObjectRange` with `[start=0,end=0)` = whole
file (same path). `Close` → `CLOSED`. `Get` → state. Object not on the pinned tape
→ `FAILED_PRECONDITION`; unknown object → `NOT_FOUND`.

## Error taxonomy (gRPC Status)

`invalid_argument` (missing session_id/object_id), `failed_precondition` (no/closed
session; a session already active; object not on the pinned tape; tape identity
mismatch; non-whole-file range — S5b), `not_found` (unknown session; object absent
from catalog), `internal` (SCSI/format), `unimplemented` (`drive_target`, arbitrary
ranges, multi-file `file_id`). Mid-stream errors are `Err(Status)` stream items.

## Pinned contract for the Sutradhara read client

- **Open:** `OpenReadSession{ tape_target: TapeTarget{ tape_uuid, mount_if_needed=true } }`
  → `ReadSession`. `drive_target` / `recover` → `unimplemented`. `idempotency_key`
  accepted, not deduplicated (later).
- **Read:** `ReadFile{ session_id, object_id, file_id (empty = sole payload),
  stream_chunk_bytes }` → `stream BytesChunk`. `ReadObjectRange` with
  `start_byte=end_byte=0` = whole file; any other range → `unimplemented` (S5b).
- **Close/Get** as above. One active session at a time (read **or** write);
  concurrent `Open*` → `FAILED_PRECONDITION`.

## Scope

**IN (S5a):** generalize the owner to a drive-session owner (read+write); relocate/
factor the read-into-sink core + `CapturePayloadSink` `cli→api`; `ChannelWriter`;
`OpenReadSession`/`ReadFile`/`ReadObjectRange`(whole)/`Close`/`Get` wired to the
owner, replacing the in-memory mock; register `ReadSessionService` on the daemon
(confirm it isn't already). **OUT:** ranged/partial reads (S5b); multi-drive
parallelism (Layer 2 workstream + S5c); `drive_target`; idempotency-dedup;
multi-file `file_id` selection.

## Concurrency note (Layer 2 constraint)

`DriveHandle<'a>` exclusively `&mut`-borrows its `LibraryHandle`
(`docs/layer2b-design.md:251`: "two drives in the same library can't be open
simultaneously through this surface"), so the daemon can hold **one** open drive
at a time per library today. One active session (read or write) is therefore both
the S5a design and the current Layer 2 reality. **Multi-drive parallel read/write**
(the desired end state — the MSL3040 has several drives, and only the changer robot
must serialize) requires a **Layer 2 redesign** (independent per-drive handles +
serialized changer) → then Layer 5 per-drive owners. Tracked as a new roadmap item;
out of S5a.

## Testing / acceptance criteria

1. **Unit:** `ChannelWriter` framing (chunks + final `is_last`); the read-into-sink
   core over a `VecBlockSource` rem-tar fixture → streamed bytes equal the original;
   object-not-on-pinned-tape → `FAILED_PRECONDITION`.
2. **Regression:** the relocated read core keeps A.9/B.7 green.
3. **Harness e2e:** "read via the Layer 5 daemon" — write an object (S4a daemon
   write or `rem-debug archive write`), `OpenReadSession` on its tape, `ReadFile`,
   assert the streamed bytes are **bit-equal** to the original and match
   `rem-debug archive read`; a concurrent second `Open*` → `FAILED_PRECONDITION`.
- Gates: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test`.

## §verification — Rust design verification

Verified against `cargo check -p remanence-api` + `cargo clippy -p remanence-api
--all-targets -- -D warnings` (both clean) on 2026-06-03, with a real-body skeleton
(`crates/remanence-api/src/read_owner_skeleton.rs`: `ChannelWriter` impl `Write` +
`blocking_send`; a `ChunkSink` stand-in for `CapturePayloadSink<ChannelWriter>` over
`stream_rem_tar_object`; the read inner loop holding `DriveHandle` as a stack local;
`SessionCommand` read arms + Send/Sync asserts). Removed after (design-only); the
plan recreates it. Checked against current HEAD (post S1/S4a).

Five-category result:
1. **Module privacy** — owner generalizes `write_owner` (same crate); the read core
   + `CapturePayloadSink` relocate `cli→api` (like S4a's mount bridge). After
   relocation, intra-crate/`pub`. Pass.
2. **`!Send` in threading** — verified: `SessionCommand` (carrying `chunk_tx`) is
   Send; `tokio::mpsc::Sender<Result<BytesChunk,Status>>` is Send+Sync;
   `ChannelWriter` is Send. Pass.
3. **Reactor timing** — `blocking_send`/`blocking_recv` run on the plain
   `std::thread` owner (they panic inside a runtime); the async handler only returns
   the `ReceiverStream`. Pass.
4. **Borrowed-handle plumbing** — verified: `DriveHandle` held as a stack local
   across the read `blocking_recv` loop, `DriveHandleSource(&mut drive)` + `&mut
   index` coexist, no self-referential struct (single-session owner). Pass.
5. **Trait/method visibility traps** — `pb::{BytesChunk, ReadSession}`,
   `stream_rem_tar_object`, `DriveHandleSource`, `RemTarEntrySink` reachable; the
   read-core relocation resolves the one cross-crate gap. Pass.
