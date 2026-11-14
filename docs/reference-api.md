# API reference

## Write sessions

`CheckpointSession` costs a synchronizing barrier and a durable commit record;
it need not end the open parity epoch.
A session close costs the final barrier and commit record, followed by any
remaining short epoch's full sidecar and its closeout barrier under the
recommended session policy.
`CHECKPOINTED` means the Object is durable; parity for its open epoch may follow.
The scope of `caller_object_id` for retry identity is the pool.
A session open does not wait for a drive. When no drive bay can take the tape
because the bays are reserved by other work, the daemon refuses the open with
`FAILED_PRECONDITION` and appends the holders to the message, for example
`no free drive bay available to load cartridge RMJ101L9; held by: drive <SERIAL> held by write session <UUID>`.
The daemon keeps no queue of waiting opens; the client retries the open later.

## Tape-I/O fences

The `LibraryService` service owns readiness quarantine and administrative fence
release. The `Daemon` service provides process health.
These additions extend `proto/layer5.proto` without changing existing field tags.

| RPC | Request | Reply | Authorization |
|---|---|---|---|
| `LibraryService.ReleaseTapeIoFence` | `ReleaseTapeIoFenceRequest`: `quarantine_id`, non-empty `acknowledgment` | `TapeIoFence` | Operation control, the same permission as `CancelOperation` |
| `Daemon.Health` | Empty | `HealthResponse`, including active fence count and records | Read |

`ReleaseTapeIoFence` refuses an empty or whitespace-only acknowledgment with
`INVALID_ARGUMENT` and `acknowledgment must not be empty`. Release requires a
`library:` tape-fence id. An id without a typed source identity is refused with
`FAILED_PRECONDITION` and `fence has no typed library source identity`; a
drive-scoped source is refused with `fence is not tape-scoped`. For a typed tape
id with no record, the RPC returns `NOT_FOUND`. A released id returns its
original record unchanged.

A tape-fence release appends `FenceReleased` to the library log with the source
fence reference and acknowledgment, under the authenticated actor. The record
is durable before the catalog row clears. Startup or a repeated release
completes an interrupted projection without a second release event. Tape
admission observes the released catalog row, subject to the normal identity,
readiness and append checks.

`TapeIoFence` contains `fence_id`, `quarantine_id`, `tape_uuid`, optional `barcode`
and `endpoint_key`, `reason`, `state`, `created_at_utc`, `updated_at_utc`, optional
`release_ack`, and optional `evidence_json`. The released record's
`updated_at_utc` is the original library-log release timestamp.

`HealthResponse`, `ListTapesResponse`, and `GetLiveStatusResponse` include
`active_tape_io_fence_count` and repeated `active_tape_io_fences`. The list
includes drive-only fences even when no tape matches a catalog query filter.
Live status refreshes fence records even when the inventory snapshot is cached.
Scoped fences leave the daemon available; an unresolved scope refuses startup.

On mTLS connections, authorization uses the certificate role and audit evidence
identifies the certificate fingerprint. Read-only roles cannot release a fence.

## Errors

Tape and drive failures can reach clients as gRPC `INTERNAL`,
`FAILED_PRECONDITION` or `UNAVAILABLE`. Such a status may carry the
`remanence-retry` metadata label: `transient`, `operator` or `media` describes
what kind of intervention helps. A status without the label gives no such
advice; the label is not promised for every failure or RPC. `DATA_LOSS` means
a digest or checksum failure.
