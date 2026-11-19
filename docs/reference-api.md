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
`FAILED_PRECONDITION` and a message containing
`fence has no typed library source identity`; a bay-scoped drive fence is
refused with a message containing `fence is not tape-scoped`. A standalone
drive hold is refused with `FAILED_PRECONDITION` and a message containing:

```text
fence library:<n> holds standalone drive <serial>; stop the daemon and run `rem tape quarantine release library:<n> --device PATH --allow-device <serial> --ack TEXT` as a standalone command
```

The hold refusal appends no release record. For a typed tape
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
advice; the label is not promised for every failure or RPC. Among other uses,
`DATA_LOSS` reports a file-digest, manifest-digest or tar-header checksum
mismatch on a verified whole-object read. An audit query also ends with
`DATA_LOSS` if its history walk encounters a damaged library-log segment.

A read-session `ReadObjectRange` or `ReadFile` fails with `INTERNAL` carrying
`remanence-finding: media-damage` when a READ of the object's recorded blocks
reports a medium error, an unrecovered read error, a recorded entity not found,
a blank region, or a record length different from the expected length (a current
incorrect-length indication, ILI). An error the drive reports, or a mismatch
the daemon finds, while positioning to the data is not so classified, nor is a
short transfer reported as good, a filemark, a transport fault, or a hardware
fault (sense key 4) other than an unrecovered read error or a recorded entity
not found. The same status carries `remanence-retry`: `media` when the drive
reports a medium or hardware error, `transient` otherwise (a blank region, or an
ILI alone); the finding's advice holds either way. It means this copy could
not be read here: read another copy. It does not by itself prove the medium is
bad; a dirty head in one drive can give the same answer.
