# Layer 2b Design Feedback

Review target: `docs/layer2b-design.md` at commit `011ba80`.

Overall: the document has the right boundary: Layer 2b hangs state-changing
operations off `LibraryHandle`, keeps discovery read-only, and treats the
allowlist as mandatory. The main fixes needed before implementation are around
transport support, drive-handle testability, unresolved-drive safety, and
multi-step operation failure semantics.

## Findings

### 1. High: the transport layer cannot issue the CDBs this design depends on

The Layer 2b doc specifies MOVE MEDIUM, INITIALIZE ELEMENT STATUS,
PREVENT/ALLOW, and drive LOAD/UNLOAD (`docs/layer2b-design.md:18-23`,
`docs/layer2b-design.md:76-153`). Those are no-data state-changing commands.
The current transport API only exposes:

```rust
fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<usize, ScsiError>;
```

See `crates/remanence-library/src/transport.rs:17` and
`crates/remanence-scsi/src/sg_io.rs:15-16`.

That means the implementation plan jumps from "CDB build + execute" to tests
without specifying the required Layer 1/transport work. Layer 2b needs an
explicit transport expansion before `LibraryHandle::move_medium` can exist.

Suggested fix:

- Add a Layer 1 slice before `7.1` or as `7.0`: CDB builders for MOVE MEDIUM,
  INITIALIZE ELEMENT STATUS, PREVENT/ALLOW, and LOAD/UNLOAD in
  `remanence-scsi`.
- Extend `sg_io` with a no-data execution path using `SG_DXFER_NONE`.
- Extend `SgTransport` and `FixtureTransport`/`RecordingTransport` to support
  no-data CDBs and to record them for safety tests.
- Keep data-in RES/INQUIRY and no-data state-changing paths separate enough
  that discovery cannot accidentally send a state-changing opcode.

### 2. High: `DriveHandle` is not testable with the current handle design

`LibraryHandle::open_drive(...)` is specified as opening the drive's own
`/dev/sgN` (`docs/layer2b-design.md:163-179`, `docs/layer2b-design.md:403-405`).
But the existing `LibraryHandle` only owns one already-open changer transport.
There is no factory stored in the handle, and no `open_drive_with(...)` API is
specified.

As written, production code can call `LinuxSgTransport::open_rw`, but the
recorded-transport tests promised in `docs/layer2b-design.md:442-447` cannot
exercise drive open/revalidation without touching live `/dev/sg*`.

Suggested fix:

- Specify a testable drive-open path, for example
  `LibraryHandle::open_drive_with(bay, policy, transport_for)`.
- Or store a transport factory/provider in the handle created by
  `Library::open_with`, so drive opens use the same injection mechanism as
  changer opens.
- Add an acceptance test that `LibraryHandle::unload` can open a fake drive,
  revalidate VPD 0x80, issue `LOAD/UNLOAD`, and then move the cartridge via a
  fake changer, all without live devices.

### 3. High: drive-bay preflight can allow operations against unresolved drives

The MOVE MEDIUM validation says:

- source drive bay is valid when `installed.is_some()` and `loaded_tape.is_some()`
- destination drive bay is valid when `loaded_tape.is_none()`
- derived drive identity gets a policy check

See `docs/layer2b-design.md:95-100`.

This leaves a safety hole: a destination drive bay with `installed = None` and
`loaded_tape = None` passes the destination check, even though Layer 2a uses
`installed = None` to mean the drive identity could not be resolved. That is
exactly the case where we should refuse to load a tape into the bay.

The same issue exists for source classification: a bay with `loaded_tape =
Some(...)` but `installed = None` would likely be reported as `SourceEmpty`,
which is misleading. The problem is not that the source is empty; it is that the
drive bay is operationally unsafe.

Suggested fix:

- For any MOVE involving a drive bay, require `bay.installed.is_some()`.
- For drive-targeting operations, require `installed.sg_path.is_some()` if a
  later drive LOAD/UNLOAD is part of the operation.
- Add error variants such as `DriveBayUnresolved { addr }` and
  `DriveDeviceUnavailable { addr, serial }`.
- Keep the existing derived-identity check, but treat `installed = None` as a
  separate hard refusal, not as "empty" or "safe destination."

### 4. High: `load` and `unload` are not atomic, and `MoveError` cannot express partial failure

The goals call `library.unload(bay)` "drive UNLOAD + slot move, atomic at the
API level" (`docs/layer2b-design.md:24-30`). The API later returns only
`MoveError` for `LibraryHandle::unload` (`docs/layer2b-design.md:283-290`),
while `DriveHandle::unload` returns `DriveOpError`
(`docs/layer2b-design.md:190-199`, `docs/layer2b-design.md:245-251`).

This is not actually atomic at the hardware level:

- `unload`: drive UNLOAD can succeed, then changer MOVE can fail.
- `load`: changer MOVE can succeed, then drive LOAD can fail.
- `open_drive` can fail before either operation.

The current error vocabulary has no phase information and no place to carry
`OpenError` or `DriveOpError` from the drive side. It also does not say what
happens to the cached snapshot after a partial success.

Suggested fix:

- Replace the public composed operation return types with phase-aware errors,
  for example `LoadError` and `UnloadError`.
- Include variants like `OpenDrive(OpenError)`, `DriveUnload(DriveOpError)`,
  `Move(MoveError)`, and `PostMoveLoad(DriveOpError)`.
- For every partial-success path, explicitly say whether the snapshot is
  patched, marked dirty, or immediately refreshed.
- Avoid saying "atomic" unless the API provides an all-or-nothing guarantee.
  "Composed operation with phase-aware recovery" is more accurate.

### 5. High: refresh/rescan can lose drive host bindings

The rescan path says that after INITIALIZE ELEMENT STATUS, Layer 2b re-issues
RES and replaces `drive_bays`, `slots`, and `ie_ports` while keeping the
changer identity fields (`docs/layer2b-design.md:343-349`). The `refresh()`
path similarly re-reads element state via RES only
(`docs/layer2b-design.md:351-365`).

RES from the changer can provide DVCID drive serials, but it does not provide
the host-side tape `/dev/sgN`, sysfs path, or per-drive INQUIRY fields that
Layer 2a fills by probing tape devices and joining by VPD 0x80. If Layer 2b
blindly replaces `drive_bays` from RES, it can downgrade
`IdentitySource::DvcidAndInquiry` to `DvcidInline` and drop `sg_path`. After
that, `open_drive` may no longer work.

Suggested fix:

- Define a reconciliation step for `refresh` and `rescan`.
- Preserve existing `InstalledDrive.sg_path`, `sysfs_path`, vendor, product,
  revision, and `DvcidAndInquiry` when the serial still matches.
- If a RES refresh reports a drive serial not present in the old host-joined
  snapshot, either emit a warning/dirty state or require full `discover()`.
- If the operation needs a full tape-device join, say so explicitly and reuse
  the Layer 2a orchestration rather than only the changer RES path.

### 6. Medium: `open_rw` does not guarantee state-changing SG_IO permission

The doc says `Library::open` uses `LinuxSgTransport::open_rw`, "so TO_DEV CDBs
are accepted by the SG driver without a surprise EACCES"
(`docs/layer2b-design.md:61`). That solves file open mode, but not the Linux
SCSI command filter. The recent INSTALL/JOURNAL work found that READ ELEMENT
STATUS and other non-whitelisted commands need `CAP_SYS_RAWIO`; otherwise SG_IO
returns EPERM even when `/dev/sgN` opens successfully.

Layer 2b should not imply that `open_rw` is sufficient for MOVE MEDIUM or
PREVENT/ALLOW.

Suggested fix:

- Add a Layer 2b deployment prerequisite section: device group membership plus
  `CAP_SYS_RAWIO` for the CLI/daemon.
- Add an error-handling requirement: EPERM from state-changing SG_IO should
  produce an operator-facing hint similar to the existing discovery hint.
- Consider a harmless capability probe during handle acquisition, or accept
  first-operation failure but document the exact error path.

### 7. Medium: PREVENT auto-release via `Drop` is overstated

The doc says a `Drop` impl issues ALLOW if PREVENT was ever issued, "so a
panicking session doesn't strand the library" (`docs/layer2b-design.md:135`) and
again frames this as an auto-release safety property
(`docs/layer2b-design.md:377`).

This should be narrowed. `Drop` does not run for process abort, SIGKILL, host
crash, power loss, or some panic configurations. It also cannot return an error
if ALLOW fails.

Suggested fix:

- Specify this as a best-effort cleanup, not a guarantee.
- Prefer a `RemovalLockGuard` returned by `lock_removal()` whose Drop performs
  best-effort ALLOW.
- Require explicit `allow_removal()` in daemon normal paths.
- Audit both failed ALLOW attempts and lock state transitions.
- Document the operational recovery: run `rem unlock <library>` or power-cycle /
  front-panel recovery if a process dies while locked.

### 8. Medium: audit hook records intent but not outcome

The audit hook fires after preflight and before the CDB is issued
(`docs/layer2b-design.md:375`). That records intent, but spec v0.2's defense
model wants auditability of state changes, not only attempted changes.

Suggested fix:

- Define two audit events, or one event with a result update:
  `OperationStarted` and `OperationFinished`.
- Include outcome, error summary, duration, and whether the snapshot was
  patched/refreshed/marked dirty.
- Include an audit event for preflight refusal if the daemon wants to detect
  repeated malicious or mistaken requests.

### 9. Medium: `load` argument order is inconsistent

The Rust API uses `load(slot, bay)` (`docs/layer2b-design.md:25-27`,
`docs/layer2b-design.md:278-281`), but the CLI plan uses
`rem load <library> <bay> <slot>` (`docs/layer2b-design.md:417-420`) and the
worked example follows bay-then-slot (`docs/layer2b-design.md:499-500`).

This is a footgun for tests and operator UX.

Suggested fix:

- Pick one order everywhere.
- The safer CLI shape is probably explicit flags:
  `rem load <library> --slot 0x0400 --bay 0x0100`.
- If positional args stay, make the CLI and Rust API use the same order and add
  examples next to the command synopsis.

### 10. Medium: the implementation plan omits where SCSI command builders live

Spec v0.2 defines Layer 1 as the pure SCSI command-construction/parsing layer.
Layer 2b's testing section mentions "CDB builders"
(`docs/layer2b-design.md:440`), but the implementation plan starts with Layer
2b error types and patching (`docs/layer2b-design.md:387-397`) without saying
that MOVE MEDIUM, INIT ELEMENT STATUS, PREVENT/ALLOW, and LOAD/UNLOAD CDB
builders first need to land in `remanence-scsi`.

Suggested fix:

- Add a first implementation slice for Layer 1 builders and SG no-data
  execution.
- Keep Layer 2b responsible for policy, validation, snapshot patching, and
  error mapping, not raw CDB byte layout.

### 11. Low: `source_slot` semantics need clarification for non-slot sources

The MOVE patch rules say that when the destination is a drive bay, set
`source_slot = Some(src)` so a later unload knows where to put the cartridge
back (`docs/layer2b-design.md:333-340`). That is fine for slot-to-drive loads,
but `move_medium` is generic and can also move from an IE port or potentially
from another drive bay.

Suggested fix:

- Either restrict drive destinations in composed `load()` to storage-slot
  sources only, and document that generic `move_medium` does not promise
  source-slot semantics.
- Or rename the model field in a later migration to `source_element_address`
  if non-storage sources are valid.

## Suggested Edit Order

1. Add the transport/Layer 1 prerequisite slice.
2. Tighten drive-bay preflight around `installed = None`, missing `sg_path`, and
   derived identity.
3. Replace composed `load`/`unload` "atomic" wording with phase-aware errors and
   recovery behavior.
4. Define refresh/rescan reconciliation so drive host bindings survive.
5. Add CAP_SYS_RAWIO and PREVENT/ALLOW cleanup caveats.
6. Fix CLI/API argument order and audit outcome semantics.

Once those are addressed, the doc should be implementation-ready.
