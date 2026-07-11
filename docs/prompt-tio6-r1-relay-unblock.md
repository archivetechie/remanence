# Prompt TIO-6 R1 — restore relay unblock (the 13.28 MB/s plumbing fix)

**Status:** pending. **Normative:** `docs/design-tape-io-read-pipeline-v0.1.md`
§6 (the 13.28 pathology ladder) + §6.4 (stage R1). This is the SMALL, INDEPENDENT
stage — the relay latency bug, NOT the read submitter (that is R2, in panel).
Server-side (remanence) only; client-side flag ownership is deferred (design §12 Q3).

## Root cause (verified against the code)
`crates/remanence-api/src/read_core.rs::send_chunk` (~:458) does `tx.try_send`
on a bounded tokio mpsc and, on `Full`, `thread::sleep(≤ READ_SEND_RETRY_DELAY
= 10 ms)` (:26) then retries. Under a bursty consumer this locks the writer into
~10 ms quanta → 6.5–13 MB/s, bracketing the field restore number (13.28).

## Scope
1. **Replace the poll with a real block.** Swap `try_send` + `thread::sleep` for
   tokio mpsc **`blocking_send`** (valid from the sync `ChannelWriter` context).
   Preserve the existing liveness deadline as a **watchdog** around the blocking
   send, NOT as a poll quantum — a genuinely closed receiver must still fail with
   `BrokenPipe` and a stalled one with `TimedOut` at the deadline, exact same
   error types/semantics as today. Add a **stall-time counter** (total wall blocked
   on a full channel) surfaced in the read diag (design §3.5) so the fix is
   measurable.
2. **Chunk size.** Raise the server default chunk size (used when the client
   requests 0) from 64 KiB to **256 KiB** (design §12 Q3 starting point; the
   physical leg-2 sweep finalizes it). Size the delivery channel in **bytes**, not
   message count, so ring memory stays bounded regardless of chunk size.
3. **Explicit h2 windows.** Set explicit HTTP/2 flow-control windows on the daemon
   server — `initial_stream_window_size` / `initial_connection_window_size`
   ≈ **4 MiB** (design §12 Q3), or enable tonic adaptive windows. Currently unset
   (tonic default 65,535 B independently caps ~13 MB/s at ~5 ms RTT). Document the
   chosen values AND the matching client-side requirement in
   `docs/reference-configuration.md`.
4. (Opportunistic — only if it falls out naturally) drop the `StagedReadWriter`
   `to_vec` copy via refcounted slices. Copies are not the dominant term; SKIP if
   it adds any risk.

## Constraints (binding)
- **Do NOT touch the read safety funnel (TIO-5b):** typed handoff, ILI/reset-UA/
  recovered handling, `valid_bytes` truncation — untouched. This is purely the
  relay/transport layer.
- **No read submitter / read-ahead here** (that is R2). Reads stay synchronous
  batched-refill; R1 only removes the relay stall.
- The **READ SCSI command stream must stay byte-identical** — this changes
  transport framing, not tape commands (assert against the golden fixture if the
  read-stream fixture covers this path).

## Tests (design §10)
- `blocking_send` replaces the poll: a slow-drain harness shows the writer's
  effective rate tracks the drain rate, NOT a 10 ms quantum; a CLOSED receiver
  still yields `BrokenPipe`; a stalled one still yields `TimedOut` at the deadline.
- stall-time counter increments under a full channel, is ~zero under a fast drain.
- server default chunk size is 256 KiB when the client requests 0; the channel
  byte-budget bounds memory across chunk sizes.
- h2 window values are applied (assert the server builder config) and recorded in
  `reference-configuration.md`.

## Definition of done (AGENTS.md applies)
`cargo test --workspace` green, `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings`, socket tests run (sandbox limits
reported, never `#[ignore]`d). Summary lists each change → its test.
**Verification member:** the sender-stall counter + a local slow-drain repro
demonstrating the writer no longer quantizes; physical confirmation is fieldtest
**leg-0** (re-decompose the July restore against the healthy relay) at the next
MSL3040 window, BEFORE the R2 submitter lands.
