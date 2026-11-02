# Remanence Layer 5 — gRPC API protos

**Status: implemented, pre-stability contract.** `layer5.proto` is compiled by
`crates/remanence-api` and served by `rem-daemon`. It uses the
`remanence.api.v1` package namespace, but the project has not made a wire
stability promise for that namespace. Breaking changes remain possible while
the software is alpha and must be coordinated with known generated-binding
consumers.

## Scope

The file is the single source of truth for the Layer 5 wire contract. It
defines Daemon, LibraryService, Catalog, WriteSessionService,
ReadSessionService, ReadPlanService, and Audit services. The implementation
includes Unix-socket transport, optional mTLS TCP transport, role-based
authorization, catalog and audit streams, pool-targeted or pinned-tape write
sessions, whole-object and ranged reads, read planning, library inspection and
robotics, drive stewardship, alarms, and live status.

Some declared operations intentionally return `UNIMPLEMENTED`; the current
list belongs in [the status page](../docs/status.md), not in a second contract
copy here.

## Authoritative references

- [`layer5.proto`](layer5.proto) is authoritative for field numbers, presence,
  services, and RPC behavior.
- [REM-OBJECT](../specs/publication/rem-object-core-1-specification.md) and
  [REM-ENCRYPT](../specs/publication/rem-encrypt-1-specification.md) define the
  object and envelope bytes carried through the API.
- The tape implementation follows the
  [REM-PARITY revision in preparation](../specs/in-progress/rem-parity-1-specification.md).
  The copy in `specs/publication/` describes the `schema_major` 1 layout, which current software rejects.
- [Architecture](../docs/architecture-overview.md) and
  [configuration](../docs/reference-configuration.md) describe deployment and
  transport trust boundaries.

## Stable intent and evolving surfaces

| Stable intent | Still evolving or explicitly unsupported |
| --- | --- |
| Explicit library identity; no implicit single-library default | Exact wire-stability declaration for the `remanence.api.v1` namespace |
| REM-OBJECT identity, digests, and opaque body-format manifests | Write-session caller idempotency keys remain reserved; `FinalizeTape` and `CloseoutTape` require supported keys |
| Native object catalog plus additive native/foreign unit discovery | Persistent per-entry cache shape for foreign formats |
| Pool-targeted and pinned-tape write admission | Runtime pool-assignment mutation; pool membership is configuration/catalog policy |
| Server streaming with bounded producer channels | Write-session restart and the operations listed as unimplemented in `docs/status.md` |
| Explicit open/close/abort/checkpoint session lifecycle | Cross-tape transaction semantics |

Foreign-format metadata stays behind opaque adapter-owned bytes rather than a
wire `oneof`; the core API does not acquire BRU- or tar-specific locator types.
Multiple-library routing stays explicit because several libraries can expose
the same element addresses and must never be selected by convenience default.

## Generation

Rust generation runs from `crates/remanence-api/build.rs` through
`tonic-prost-build`; generated bindings are included with
`tonic::include_proto!`. The checked-in `.proto` is the source of truth, not
generated Rust.

Python bindings used by consumers are generated and maintained by those
consumers; this repository does not ship them.

## Compatibility policy

Before a wire-stability declaration, breaking changes require coordinated
consumer updates but no compatibility guarantee is implied by the package
suffix. Once the namespace is declared stable, changes are additive: fields
are deprecated rather than reused or removed, and new behavior is introduced
through new fields, RPCs, or services. The wire-presence census in CI prevents
new scalar fields from entering the contract without an explicit presence
decision.

## Readiness and finalization operations

`ResumeMediaReadiness` attaches to a running wait for the same operation id,
whether the daemon or a client starts it. A different holder keeps the busy
refusal. `WatchOperation` progress includes `started_by=daemon|client`, readiness
state and decoded sense. The daemon starts resumable fences after session-open
refusals and startup reconciliation, with a 9000 s timeout and 30 s steady polls.
Release-only states require operator release. An operator-cancelled wait stays
stopped until a client explicitly resumes it. Startup still reconciles that fence:
a ready drive clears it, an absent cartridge requires release, and a resumable
fence preserves the operator cancellation.

Shutdown stops readiness admission before the servers drain and ends every
running readiness wait, including the wait inside LoadDrive, as Cancelled with a
shutdown summary and its last durable readiness state. Its durable fence
keeps its resumable state without an operator cancellation, so startup
reconciliation and automatic discovery resume it on the next start. Discovery
wakes on durable changes and wait completion and keeps a 30 s safety sweep.
Any live operation owns its fence id. Newly eligible or changed records get one
5 s window of busy retries; notifications do not extend it. Failed waits and
non-busy start refusals wait for the sweep. Each distinct start refusal logs once.
A resume starts a wait with at least the daemon's 9000 s profile; a shorter
client deadline stops watching without shortening the drive wait.

`StartTapeVerification` waits on readiness fences, including fences on another
drive that block robotics, for one 15-minute budget from the first refusal.
Progress reports `phase=readiness`, `readiness_operation_id`, `state`, `sense`
and `sense_name`. Cancellation stops verification within one 250 ms poll and
leaves the readiness wait running. A failed or shutdown-cancelled readiness wait
with a resumable record keeps the observer waiting. Release-only state, operator
cancellation or deadline expiry includes the original refusal, readiness state
and error summary. Unary `VerifyTapeIndex`
returns the immediate refusal; its client controls the wait budget.

`FinalizeTape` uses the same readiness wait. Its operation represents the
attempt; `TapeFinalization` represents durable intent. If the worker stops,
`GetOperation` reports FAILED with a durable error summary, and the projection
stays FINALIZING with that summary in `detail`. FAILED persists until a same-key
retry or startup recovery relaunches the worker. A retry publishes RUNNING before
`FinalizeTape` returns. After a restart, recovery relaunches the worker under the
same operation id and it reports RUNNING.

## Caller object identity

A `caller_object_id` names an object within one pool.
`GetObjectRequest.pool_id` and `FindObjectCopiesRequest.pool_id` select that
pool's committed object and copies. An absent or empty pool selector succeeds
only when one pool contains the caller id; several pools return
`FAILED_PRECONDITION` naming those pools. No match returns `NOT_FOUND`.
Repeated writes with the same pool, caller id and content replay the committed
receipt; conflicting content is refused.

## Session-owned epoch cutover

Regenerate consumer bindings from this revision before deployment. `Catalog.CloseoutTape`
accepts `CloseoutTapeRequest`: required UUID `tape_uuid = 1`, optional
`expected_pool_id = 2`, and required message `IdempotencyKey idempotency_key = 3`.
It returns `OperationRef`; follow `WatchOperation` to completion. A tape with no
closeout due succeeds without media work. Repeating a key rejoins its operation.

`SupersedeTailRequest.observed_tape_log_sequence = 5` is an optional uint64 on the
wire and mandatory on confirmation. Preview returns the required value in
`SupersedeTailResponse.tape_log_sequence = 4`. A stale value refuses with
`TailPreviewStale`. `GetWriteSession` retains the ended session's durable receipts;
there is no `SessionEndedDetails` status extension.

## Closeout obligation surfaces

Regenerate consumer bindings (including sutradhara `_proto`) for `Tape` tags
23–28 and `Drive` tags 33–38. The four debt fields are presence-carrying:
optional `uint64 unprotected_bytes`, optional Timestamp `unprotected_since`,
optional `bool closeout_due`, and optional `bool closeout_overdue`.
`closeout_authority_error` (27/37) and `closeout_retry_reason` (28/38) are
optional strings. These fields are unreleased; no compatibility form is retained.

Present zero bytes and false flags are determined values. An unreadable native
log leaves all four debt fields absent and sets `closeout_authority_error`.
`ListTapes` and `GetLiveStatus` retain the other tapes and all other status data;
`GetTape` for the affected tape returns its error status. Vacant drives have
absent debt fields and no authority error. For a readable tape,
`unprotected_since` is absent exactly when unprotected bytes are zero.
An accepted finalization plan may own the uncovered range: `closeout_due` is
then false while bytes, age and `closeout_overdue` remain visible.

`GetLiveStatusResponse.closeout_debt = 9` contains catalog `Tape` entries for
all uncovered ranges, pending seal triggers and unknown authorities, including
unseated and unavailable tapes and tapes outside the daemon's configured scope.
An empty list means no known debt or unknown authority among catalog tapes.
Media maintenance operates only within configured library and drive ownership;
retrying deferred SQL projection requires no media access.

CLI catalog/live/top JSON uses decimal strings for present byte counts,
RFC3339 UTC for present timestamps, booleans for present flags, and `null` for
absent values. Both optional strings are strings or `null`. Lists print every
entry before reporting unreadable authority as a nonzero exit status;
interactive top keeps running and shows `DEBT?` and the per-tape error.
`closeout_retry_reason` retains the worker's last refusal. Busy, no-drive and
memory refusals use capped backoff; media protection, fences and identity
refusals wait for fence release, inventory change or operator closeout.

Status observations use the last durable fold during a staged checkpoint.
Owners publish a separate summary mirror; observers retain no writer lease.
Unowned summaries are cached by device/inode, length, mtime and ctime (including
nanoseconds). Unchanged polls do no full replay; a changed log is revalidated.
The cache stores one bounded summary per observed log, and only caches a replay
if the log's change token remains stable through validation. Owner acquisition
and registration are serialized with observation; ambiguity and poison still fail.
There is at most one worker attempt per library, with memory reserved only after
a drive is secured and before eviction or other media work. Pinned writes,
CloseoutTape and FinalizeTape wait for an in-flight worker hold.

Existing open-epoch and seal-trigger fields retain their meanings. There is no
additional session receipt message or status extension; poll `GetWriteSession`.
