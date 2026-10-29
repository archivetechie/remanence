# Tape-I/O fence API

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
