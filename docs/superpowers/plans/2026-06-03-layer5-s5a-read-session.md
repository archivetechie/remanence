# Layer 5 S5a — Read Session (whole-object read) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `rem-daemon` stream an object's bytes off tape over `ReadSessionService` (`OpenReadSession → ReadFile → CloseReadSession`), reusing the A.9 read core, behind the (generalized) single drive-session owner.

**Architecture:** Generalize S4a's writer thread into a drive-session owner serving read *or* write (one active session). Relocate/factor the A.9 read-into-sink core `cli→remanence-api`; the owner streams the object's single payload entry via `CapturePayloadSink<ChannelWriter>`, where `ChannelWriter` `blocking_send`s `BytesChunk` over a `tokio::mpsc` and the async handler returns a `ReceiverStream`. One read path → daemon and CLI bytes/locators agree.

**Tech Stack:** Rust, `tonic 0.14`, `tokio` (`mpsc`/`oneshot`, `blocking_send`/`blocking_recv`), `remanence-api` (`write_owner`→session owner, `ReadSessionApi`, `pool_write`), `remanence-format` (`stream_rem_tar_object`/`CapturePayloadSink`), `remanence-library` (`DriveHandle`/`DriveHandleSource`), `remanence-cli` (A.9/B.7 re-point), `remanence-daemon`.

Spec: `docs/layer5-s5a-read-session-design-v0.1.md`. The `ChannelWriter` (sync `Write`→`blocking_send`), the read inner-loop borrow, and the channel Send/Sync were `cargo check` + clippy verified on 2026-06-03 (skeleton removed; this plan recreates it). Builds on committed S1 (`3e9ea08`) + S4a (`c1539d6`).

**Gates (before every commit):** `cargo fmt --all`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test -p <crate touched>`.

**Test reality (as for S4a):** the owner mounts a real drive on `OpenRead`, so the full read path is hardware-gated (manual harness e2e, Task 6). Hardware-free automated tests cover: the relocated read core (A.9/B.7 regression), `ChannelWriter` framing, `read_object_payload` over a `VecBlockSource` rem-tar fixture, and the no/closed-session validation.

---

## File Structure

- `crates/remanence-api/src/read_core.rs` (new) — relocated `CapturePayloadSink` + `read_object_payload` (Task 1).
- `crates/remanence-cli/src/pool_ops.rs` — A.9/B.7 call the relocated core (Task 1).
- `crates/remanence-api/src/write_owner.rs` — generalize to `SessionCommand` + read arms + read inner loop + `ChannelWriter` (Tasks 2, 3).
- `crates/remanence-api/src/lib.rs` — `write_tx`→`session_tx`, `with_write_owner`→`with_session_owner`; rewrite `ReadSessionApi` handlers (Task 4).
- `crates/remanence-daemon/src/lib.rs` — register `ReadSessionServiceServer` (Task 5).

---

### Task 1: Relocate + factor the read core (`cli → remanence-api`)

**Files:**
- Create: `crates/remanence-api/src/read_core.rs`
- Modify: `crates/remanence-api/src/lib.rs` (`mod read_core;` + `pub(crate) use`), `crates/remanence-cli/src/pool_ops.rs`

- [ ] **Step 1: Move `CapturePayloadSink` + add `read_object_payload`**

Cut `struct CapturePayloadSink<W: Write>` + its impls from `pool_ops.rs` into `crates/remanence-api/src/read_core.rs`. Add the shared "position + stream the single payload into a sink" fn (extracted from A.9's `stream_tape_object` body):
```rust
use remanence_format::{stream_rem_tar_object, FormatError, RemTarEntrySink};
use remanence_library::{BlockSource, SpaceKind};

/// Position to `tape_file_number` and stream the object's payload blocks into `sink`.
/// Caller has already mounted + positioned the drive at BOT (this spaces forward).
pub(crate) fn read_object_payload(
    source: &mut dyn BlockSource,
    block_size: usize,
    block_count: u64,
    tape_file_number: u32,
    sink: &mut dyn RemTarEntrySink,
) -> Result<(), FormatError> {
    source
        .space(i64::from(tape_file_number), SpaceKind::Filemarks)
        .map_err(|e| FormatError::parse(format!("space to tape file {tape_file_number}: {e}")))?;
    stream_rem_tar_object(source, block_size, block_count, sink)?;
    Ok(())
}
```
(Match the exact positioning A.9 uses — confirm `space` vs `locate(first_body_lba)` against the current `stream_tape_object`; reproduce it. Keep `CapturePayloadSink` `pub(crate)`.)

- [ ] **Step 2: Re-point A.9/B.7**

In `pool_ops.rs`, replace the moved `CapturePayloadSink` + the inline position/stream in `stream_tape_object` with `use remanence_api::read_core::{CapturePayloadSink, read_object_payload};` and a call to `read_object_payload(&mut source, plan.block_size_bytes as usize, plan.block_count, loc.tape_file_number, &mut sink)`. Behaviour unchanged.

- [ ] **Step 3: Verify regression**

Run: `cargo test -p remanence-api && cargo test -p remanence-cli`
Expected: PASS — A.9 (`archive read`) + B.7 (`archive verify`) tests still green with the relocated core.

- [ ] **Step 4: Commit**
```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
git add crates/remanence-api/src/read_core.rs crates/remanence-api/src/lib.rs crates/remanence-cli/src/pool_ops.rs
git commit -m "Relocate read core (CapturePayloadSink + read_object_payload) cli -> api (S5a)"
```

---

### Task 2: `ChannelWriter`

**Files:**
- Modify: `crates/remanence-api/src/read_core.rs`

- [ ] **Step 1: Write the failing test**
```rust
#[tokio::test]
async fn channel_writer_frames_and_streams() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<crate::pb::BytesChunk, tonic::Status>>(8);
    let writer_tx = tx.clone();
    let handle = tokio::task::spawn_blocking(move || {
        use std::io::Write as _;
        let mut w = super::read_core::ChannelWriter::new(writer_tx);
        w.write_all(b"hello").unwrap();
        w.finish().unwrap(); // sends a final is_last=true chunk
    });
    let mut got = Vec::new();
    let mut saw_last = false;
    while let Some(item) = rx.recv().await {
        let chunk = item.unwrap();
        got.extend_from_slice(&chunk.data);
        saw_last |= chunk.is_last;
    }
    handle.await.unwrap();
    assert_eq!(got, b"hello");
    assert!(saw_last, "stream must end with an is_last chunk");
}
```

- [ ] **Step 2: Run → fail** (`ChannelWriter` doesn't exist).
Run: `cargo test -p remanence-api channel_writer_frames_and_streams` → FAIL.

- [ ] **Step 3: Implement** (verified pattern):
```rust
use tokio::sync::mpsc;
use tonic::Status;
use crate::pb;

pub(crate) struct ChannelWriter {
    tx: mpsc::Sender<Result<pb::BytesChunk, Status>>,
}

impl ChannelWriter {
    pub(crate) fn new(tx: mpsc::Sender<Result<pb::BytesChunk, Status>>) -> Self {
        Self { tx }
    }
    /// Send the terminal `is_last=true` chunk.
    pub(crate) fn finish(self) -> std::io::Result<()> {
        self.tx
            .blocking_send(Ok(pb::BytesChunk { data: Vec::new(), is_last: true }))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "read stream closed"))
    }
}

impl std::io::Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.tx
            .blocking_send(Ok(pb::BytesChunk { data: buf.to_vec(), is_last: false }))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "read stream closed"))?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}
```

- [ ] **Step 4: Run → pass; commit**
```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
git add crates/remanence-api/src/read_core.rs
git commit -m "Add ChannelWriter: sync Write -> BytesChunk blocking_send (S5a)"
```

---

### Task 3: Generalize the owner — `SessionCommand` + read inner loop

**Files:**
- Modify: `crates/remanence-api/src/write_owner.rs`, `crates/remanence-api/src/lib.rs` (references)

- [ ] **Step 1: Rename `WriteCommand` → `SessionCommand`; add read arms**

Rename the enum and add:
```rust
    OpenRead {
        tape_uuid: [u8; 16],
        reply: oneshot::Sender<Result<pb::ReadSession, Status>>,
    },
    ReadFile {
        object_id: String,
        file_id: Vec<u8>,
        chunk_tx: tokio::sync::mpsc::Sender<Result<pb::BytesChunk, Status>>,
    },
    CloseRead {
        reply: oneshot::Sender<Result<pb::ReadSession, Status>>,
    },
    GetRead {
        reply: oneshot::Sender<Result<pb::ReadSession, Status>>,
    },
```
Rename `spawn_writer`→`spawn_session_owner`, `writer_loop`→`session_loop`, `WriteOwnerConfig` stays. Update `lib.rs` references (`write_tx` type, `with_write_owner` body) — Task 4 renames those.

- [ ] **Step 2: Add the read open handler + read inner loop**

In the owner's outer loop, add an `OpenRead` arm calling `handle_open_read`, and add (mirroring `handle_open`):
```rust
fn handle_open_read(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    rx: &mut mpsc::Receiver<SessionCommand>,
    tape_uuid: [u8; 16],
    reply: oneshot::Sender<Result<pb::ReadSession, Status>>,
) {
    // Resolve a library hosting the tape + mount it (load_tape_by_uuid).
    let library_serial = match cfg.report.first_library_serial() { /* or from catalog */ };
    let lib = match cfg.report.library(&library_serial) { Some(l) => l, None => { reply…not_found; return } };
    let mut library = match lib.open(&cfg.policy) { Ok(h)=>h, Err(e)=>{ reply…internal; return } };
    let mut drive = match crate::load_tape_by_uuid(index, &mut library, &cfg.policy, &tape_uuid) {
        Ok(d) => d, Err(e) => { reply…internal; return }
    };
    let _ = reply.send(Ok(read_session_proto(&tape_uuid, pb::read_session::State::Open)));

    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            SessionCommand::ReadFile { object_id, file_id, chunk_tx } => {
                let result = stream_one_object(index, &mut drive, &tape_uuid, &object_id, chunk_tx.clone());
                if let Err(status) = result { let _ = chunk_tx.blocking_send(Err(status)); }
            }
            SessionCommand::CloseRead { reply } => {
                let _ = reply.send(Ok(read_session_proto(&tape_uuid, pb::read_session::State::Closed))); break;
            }
            SessionCommand::GetRead { reply } => {
                let _ = reply.send(Ok(read_session_proto(&tape_uuid, pb::read_session::State::Open)));
            }
            // any Open* while active → already-active rejection
            SessionCommand::Open { reply, .. } => { let _ = reply.send(Err(Status::failed_precondition("session already active"))); }
            SessionCommand::OpenRead { reply, .. } => { let _ = reply.send(Err(Status::failed_precondition("session already active"))); }
            SessionCommand::AppendFinish { reply, spool_path, .. } => { let _ = std::fs::remove_file(spool_path); let _ = reply.send(Err(Status::failed_precondition("active session is read-only"))); }
            SessionCommand::Close { reply, .. } | SessionCommand::Abort { reply, .. } | SessionCommand::Get { reply, .. } => { let _ = reply.send(Err(Status::failed_precondition("active session is a read session"))); }
        }
    }
}

fn stream_one_object(
    index: &mut CatalogIndex,
    drive: &mut remanence_library::DriveHandle<'_>,
    tape_uuid: &[u8; 16],
    object_id: &str,
    chunk_tx: tokio::sync::mpsc::Sender<Result<pb::BytesChunk, Status>>,
) -> Result<(), Status> {
    // Resolve the object's copy on THIS tape + its block_count/block_size.
    let copies = index.find_native_object_copies(object_id).map_err(|e| Status::internal(e.to_string()))?;
    let copy = copies.iter().find(|c| c.tape_uuid.as_slice() == tape_uuid.as_slice())
        .ok_or_else(|| Status::failed_precondition("object is not on the tape pinned by this read session"))?;
    let tape_files = index.list_tape_files(tape_uuid).map_err(|e| Status::internal(e.to_string()))?;
    let tf = tape_files.iter().find(|f| f.tape_file_number == copy.tape_file_number && f.object_id.as_deref() == Some(object_id))
        .ok_or_else(|| Status::not_found("object tape file not in catalog"))?;
    let block_size = index.get_tape(tape_uuid).map_err(|e| Status::internal(e.to_string()))?
        .and_then(|t| t.block_size).ok_or_else(|| Status::internal("tape block size unknown"))?;
    drive.rewind().map_err(|e| Status::internal(format!("rewind: {e}")))?;
    let mut source = DriveHandleSource(drive);
    let writer = crate::read_core::ChannelWriter::new(chunk_tx);
    let mut sink = crate::read_core::CapturePayloadSink::new(writer);
    crate::read_core::read_object_payload(&mut source, block_size as usize, tf.block_count, copy.tape_file_number, &mut sink)
        .map_err(|e| Status::internal(format!("read object: {e}")))?;
    sink.finish().map_err(|e| Status::internal(e))?;  // flushes + sends final is_last chunk
    Ok(())
}
```
Notes: `read_session_proto` mirrors S4a's `*_session_proto` helpers. Confirm `CapturePayloadSink::finish` returns after flushing `ChannelWriter` (so the `Write::flush` is a no-op and `ChannelWriter::finish` sends the terminal chunk — fold the terminal send into `CapturePayloadSink::finish` or call `ChannelWriter::finish` explicitly; pick one and keep it consistent). Confirm `find_native_object_copies`/`list_tape_files`/`get_tape` field names (per A.9's `plan_from_records`).

- [ ] **Step 3: Build + bounds test**
```rust
#[test]
fn session_command_bounds_hold() {
    fn assert_send<T: Send>() {}
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send::<super::write_owner::SessionCommand>();
    assert_send_sync::<tokio::sync::mpsc::Sender<Result<pb::BytesChunk, tonic::Status>>>();
}
```
Run: `cargo test -p remanence-api session_command_bounds_hold` → PASS.

- [ ] **Step 4: Commit**
```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
git add crates/remanence-api/src/write_owner.rs crates/remanence-api/src/lib.rs
git commit -m "Generalize drive owner to SessionCommand + read inner loop (S5a)"
```

---

### Task 4: Wire `ReadSessionApi` to the owner

**Files:**
- Modify: `crates/remanence-api/src/lib.rs`

- [ ] **Step 1: Rename the owner field/constructor**

`write_tx: Option<…WriteCommand…>` → `session_tx: Option<tokio::sync::mpsc::Sender<crate::write_owner::SessionCommand>>`; `with_write_owner` → `with_session_owner` (same body, `spawn_session_owner`). Update `WriteSessionApi`'s `dispatch_session` + `append_object` to use `session_tx` and the renamed `SessionCommand` variants. Add `read_session_service(&self) -> ReadSessionApi` if not present (it is — keep).

- [ ] **Step 2: Rewrite the read handlers to dispatch**

`open_read_session`: resolve `tape_uuid` from `select_read_target` (keep), then dispatch `OpenRead{tape_uuid, reply}` to `session_tx` via a oneshot (mirror `dispatch_session`); drop the in-memory `read_sessions` insert. `close_read_session`/`get_read_session`: dispatch `CloseRead`/`GetRead`. `read_file` / `read_object_range`:
```rust
async fn read_file(&self, request: Request<pb::ReadFileRequest>) -> Result<Response<Self::ReadFileStream>, Status> {
    let req = request.into_inner();
    let tx = self.state.session_tx.as_ref().ok_or_else(|| Status::unavailable("read-only/no session owner"))?;
    let object_id = decode_object_id(&req.object_id)?;
    let (chunk_tx, chunk_rx) = tokio::sync::mpsc::channel::<Result<pb::BytesChunk, Status>>(16);
    tx.send(crate::write_owner::SessionCommand::ReadFile { object_id, file_id: req.file_id, chunk_tx })
        .await.map_err(|_| Status::internal("session owner unavailable"))?;
    Ok(Response::new(Box::pin(tokio_stream::wrappers::ReceiverStream::new(chunk_rx))))
}
```
`read_object_range`: reject a non-whole range (`start_byte != 0 || end_byte != 0` → `Status::unimplemented("ranged reads are S5b")`), else identical to `read_file`. Remove `read_session_object_bytes`/`frame_object_bytes`/the in-memory `objects` mock used only by reads (leave them if other code uses them — grep first).

- [ ] **Step 3: Build**
Run: `cargo build -p remanence-api` → compiles (read handlers dispatch; mock read path gone).

- [ ] **Step 4: Commit**
```bash
cargo fmt --all && cargo clippy -p remanence-api --all-targets -- -D warnings
git add crates/remanence-api/src/lib.rs
git commit -m "Wire ReadSessionApi to the drive-session owner; drop in-memory read mock (S5a)"
```

---

### Task 5: Register `ReadSessionService` on the daemon

**Files:**
- Modify: `crates/remanence-daemon/src/lib.rs`

- [ ] **Step 1: Add the service**

In `serve`, after the `WriteSessionServiceServer` registration:
```rust
        .add_service(pb::read_session_service_server::ReadSessionServiceServer::new(
            state.read_session_service(),
        ))
```

- [ ] **Step 2: Build + commit**
Run: `cargo build -p remanence-daemon` → compiles.
```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
git add crates/remanence-daemon/src/lib.rs
git commit -m "rem-daemon: serve ReadSessionService (S5a)"
```

---

### Task 6: Full verification + manual harness e2e

- [ ] **Step 1: Full-workspace gates**
```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```
Expected: all pass (new: `channel_writer_frames_and_streams`, `session_command_bounds_hold`, a `read_object_payload`-over-`VecBlockSource` test; A.9/B.7 regression green).

- [ ] **Step 2: `read_object_payload` unit test (hardware-free)**

Add a test that builds an in-memory rem-tar object (reuse a `remanence-format` fixture/helper — grep its tests for how they construct one) over a `VecBlockSource`, runs `read_object_payload` into a `CapturePayloadSink<Vec<u8>>` (or a capturing `Write`), and asserts the extracted payload equals the original file bytes. (`tape_file_number = 0` for a BOT-relative fixture.)

- [ ] **Step 3: Manual hardware e2e (akash QuadStor fixture)**
```bash
cargo build --release -p remanence-daemon
sudo setcap cap_sys_rawio+ep target/release/rem-daemon
target/release/rem-daemon --config /var/lib/replica/rem/config.toml --allow 7CBAD9CF74 &
```
Write an object (S4a daemon write, or `rem-debug archive write --pool scenario-a --file <F>`); then via a gRPC client: `OpenReadSession{tape_target{tape_uuid=<that tape>}}` → `ReadFile{object_id=<that object>}` → collect `BytesChunk`s. Verify: (a) the bytes are bit-equal to `<F>` and to `rem-debug archive read` output (cross-transport parity); (b) a second concurrent `OpenReadSession`/`OpenWriteSession` returns `FAILED_PRECONDITION`.

- [ ] **Step 4: Record the result in the journal.**

---

## Self-Review

**Spec coverage** (against `docs/layer5-s5a-read-session-design-v0.1.md`):
- Generalize owner → SessionCommand + read inner loop → Task 3. ✓
- Relocate/factor read core cli→api → Task 1. ✓
- ChannelWriter streaming → Task 2 + Task 3 (`stream_one_object`). ✓
- OpenReadSession/ReadFile/ReadObjectRange(whole)/Close/Get wired, mock dropped → Task 4. ✓
- Register ReadSessionService → Task 5. ✓
- Object-not-on-pinned-tape → `failed_precondition`; ranged → `unimplemented` → Tasks 3, 4. ✓
- Locator/byte parity via shared core → Tasks 1, 6. ✓
- OUT (ranged S5b, multi-drive, drive_target, idempotency, multi-file) → not implemented, by design. ✓

**Placeholder scan:** the `handle_open_read` library-resolution lines (`first_library_serial`/reply arms) are sketched with `…` and a "resolve a library hosting the tape" note — that's the one spot the implementer fleshes out (the others are full code). Flag: the daemon must learn *which* library hosts the tape (config has the serial; for the single-library fixture use `config.libraries[0].serial`, generalize later). Not a logic placeholder elsewhere.

**Type consistency:** `SessionCommand` variants identical across Tasks 3 (def), 4 (dispatch). `ChannelWriter::{new, finish}` + `Write` consistent Tasks 2, 3. `read_object_payload(source, block_size: usize, block_count: u64, tape_file_number: u32, sink)` consistent Tasks 1, 3. `session_tx` rename consistent Tasks 3, 4. `CapturePayloadSink` (relocated) used in Tasks 1, 3.

**Verified:** `ChannelWriter` (sync `Write`→`blocking_send`), the read inner-loop borrow (`DriveHandle` stack-local + `&mut index` + `DriveHandleSource`), and `mpsc::Sender<Result<BytesChunk,Status>>` Send+Sync were cargo-check + clippy clean on 2026-06-03. `ApiState.write_tx`, `WriteSessionApi::dispatch_session`, `ReadSessionApi`, the daemon registrations, and `BytesChunkStream`/`ReceiverStream` confirmed at current HEAD.
