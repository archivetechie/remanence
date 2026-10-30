# API reference

## Write sessions

`CheckpointSession` costs a synchronizing barrier and a durable commit record;
it need not end the open parity epoch.
A session close costs the final barrier and commit record, followed by any
remaining short epoch's full sidecar and its closeout barrier under the
recommended session policy.
`CHECKPOINTED` means the Object is durable; parity for its open epoch may follow.
The scope of `caller_object_id` for retry identity is the pool.
Session opens queue for a drive; clients accumulate ready Objects before
opening and stream the batch when admitted.

## Tape-I/O fences

The `LibraryService` service owns readiness quarantine and administrative fence
release. The `Daemon` service provides process health.
These additions extend `proto/layer5.proto` without changing existing field tags.

| RPC | Request | Reply | Authorization |
|---|---|---|---|
| `LibraryService.ReleaseTapeIoFence` | `ReleaseTapeIoFenceRequest`: `quarantine_id`, non-empty `acknowledgment` | `TapeIoFence` | Operation control, the same permission as `CancelOperation` |
| `Daemon.Health` | Empty | `HealthResponse`, including active fence count and records | Read |

`ReleaseTapeIoFence` returns `NOT_FOUND` for an unknown id and `INVALID_ARGUMENT`
for an empty or whitespace-only acknowledgment. A released id returns the
original record unchanged. Tape and drive endpoint fences use the same RPC.
The authenticated actor, quarantine id, scope, acknowledgment, and release time
are durable in the audit log before the active row clears. Startup or a repeated
release completes an interrupted projection without a second release event.
Normal tape and drive admission observes the new catalog state without restart.

`TapeIoFence` contains `fence_id`, `quarantine_id`, `tape_uuid`, optional `barcode`
and `endpoint_key`, `reason`, `state`, `created_at_utc`, `updated_at_utc`, optional
`release_ack`, and optional `evidence_json`. The released record's
`updated_at_utc` is the original audit release timestamp.

`HealthResponse`, `ListTapesResponse`, and `GetLiveStatusResponse` include
`active_tape_io_fence_count` and repeated `active_tape_io_fences`. The list
includes drive-only fences even when no tape matches a catalog query filter.
Live status refreshes fence records even when the inventory snapshot is cached.
Scoped fences leave the daemon available; an unresolved scope refuses startup.

On mTLS connections, authorization uses the certificate role and audit evidence
identifies the certificate fingerprint. Read-only roles cannot release a fence.
