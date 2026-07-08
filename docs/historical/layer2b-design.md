# Layer 2b Design: State-changing operations

**Status:** draft v0.2 (revised 2026-05-17 in response to `docs/layer2b-design-feedback.md`)
**Author:** owner (drafted with Claude)
**Companion to:** `docs/spec-v0.2.md`, `docs/layer2-design.md` (Layer 2a), `plan.txt`
**Crate:** `crates/remanence-library/` extended in place; CDB construction lives in `crates/remanence-scsi/` (Layer 1).

This document specifies the **state-changing side** of Layer 2 — the SCSI operations that physically rearrange cartridges, force the changer to rescan elements, and gate cartridge removal. It builds on Layer 2a's `Library` snapshot, `LibraryHandle`, and `AccessPolicy` types; every state-changing CDB goes through a `LibraryHandle` (or its drive analog `DriveHandle`), never through a plain `Library` value.

The Layer 2c udev watcher (event-driven re-discovery) remains a sibling concern with its own future design doc. Where this doc and spec v0.2 disagree, spec v0.2 wins.

---

## 1. Scope

### Goals

- Expose the minimal set of state-changing SCSI commands a tape archive actually needs, with a safe, typed Rust API:
  - **MOVE MEDIUM** (SMC-3 `0xA5`) — the primitive every cartridge move composes from.
  - **INITIALIZE ELEMENT STATUS** (SMC-3 `0x07`) — force the changer to rescan when the operator inserted a cartridge via the front panel.
  - **PREVENT / ALLOW MEDIUM REMOVAL** (`0x1E`) — lock the changer's import/export and front panel during sensitive operations.
  - **UNLOAD / LOAD** on tape drives (SSC `0x1B`) — needed before the changer can pluck a tape from a drive bay, and the polite explicit form when the changer puts one in.

- Compose those primitives into the operator-visible operations the daemon and CLI actually call:
  - `library.move_medium(src, dst)` — single-CDB move (changer-only).
  - `library.load(slot, bay)` — composed: changer MOVE *then* drive LOAD.
  - `library.unload(bay, destination)` — composed: drive UNLOAD *then* changer MOVE.
  - `library.export(slot)` — slot → first available IE port.
  - `library.import(slot)` — first occupied IE → slot.
  - `library.rescan()` — INITIALIZE ELEMENT STATUS + post-init re-RES with reconciliation.
  - `library.lock_removal()` / `allow_removal()`.

  Composed operations are **phase-aware**, not atomic. Each phase can fail independently, and the public error type names which phase failed and what the snapshot looks like afterwards. See §4.2 and §5.1.

- Keep every Layer 2a safety property *load-bearing*, not aspirational, and add the operation-level safety properties spec v0.2 §8.2 implies:
  - State-changing CDBs only reach the kernel through a `LibraryHandle` or `DriveHandle`.
  - **Preflight validation** against the snapshot (refuse moves to/from unknown addresses, refuse moves out of empty sources, refuse moves into full destinations, refuse drive-bay ops where the bay's `installed` is `None` or `installed.sg_path` is `None` for drive-touching ops).
  - The kernel response is the final source of truth — preflight is for ergonomics and fast refusal, not for ground truth. A stale snapshot must still surface a typed error, not a silent corruption.
  - **Capability mismatches surface as targeted hints**, just like discovery does for EPERM. Operators should never have to strace `rem move` to learn that `setcap cap_sys_rawio+ep` was missing.

- Keep the snapshot model coherent across state changes: after a successful move, the cached `Library` value the handle holds reflects the new state without a full re-RES on every operation; partial failures mark the snapshot *dirty* so callers can `refresh()`.

### Non-goals (for this doc)

- **On-tape data.** READ, WRITE, READ POSITION, LOCATE — Layer 3's job (`remanence-tape`). Layer 2b stops at the cartridge level (UNLOAD/LOAD lifecycle, bay loading/unloading) and does not interpret tape contents.
- **Asynchronous / queued operations.** The daemon serializes per-library moves behind a mutex; Layer 2b exposes synchronous functions. (`move_medium` blocks until the changer responds; typical real-hardware move on an MSL3040 is 8–20 seconds.)
- **EXCHANGE MEDIUM** (`0xA6`). Atomic swap is rarely used and complicates address validation; defer until a concrete user appears.
- **Drive cleaning automation.** Auto-detect "drive wants cleaning" and dispatch the cleaning tape. The bay's `loaded_tape` voltag tells the operator a cleaning tape is in; running it is operator-initiated for v0.1.
- **POSITION TO ELEMENT, REQUEST VOLUME ELEMENT ADDRESS.** Diagnostic CDBs we don't need yet.
- **Cross-library transactions.** A move can't span two `LibraryHandle`s; each library is operated independently.

---

## 2. Background — what Layer 2a already gives us

By the time Layer 2b runs, the caller has:

- A read-only `DiscoveryReport` from `discover()` with the full topology, identity, and cartridge state of every reachable library.
- An `AccessPolicy` — the explicit allowlist of library serials state-changing ops may target. The daemon constructs this from its own config; the `rem` CLI builds a `StaticAllowlist` inline from `--allow <serial>` / `--allow-derived <serial>` global flags (see §7.9).
- A `LibraryHandle` returned by `library.open(policy)`, which already:
  - Refused the open unless the library's serial is on the allowlist (`OpenError::NotAllowed`).
  - Refused the open if any drive bay's `installed.identity_source` is `Derived` and the policy hasn't opted in (`OpenError::DerivedIdentityNotOptedIn`).
  - Opened the changer's `/dev/sgN` **read/write** via `LinuxSgTransport::open_rw`, so TO_DEV CDBs are accepted by the SG driver without a surprise `EACCES`. (Note: `open_rw` is necessary but not sufficient — see §2.1 on capabilities.)
  - Re-issued standard INQUIRY + VPD 0x80 and confirmed the device behind the cached `/dev/sgN` is still the same changer (`OpenError::IdentityChanged` otherwise).

This means **every state-changing CDB starts from a handle whose identity was just freshly revalidated**, against a snapshot whose contents the operator/policy has already vetted. We don't redo the heavy lifting per operation. We do defend against three things the handle alone can't:

1. The snapshot itself drifting between handle acquisition and the CDB being issued (operator inserted a tape via the front panel mid-session; another process moved a cartridge — rare but possible with shared changers like the partitioned MSL3040).
2. The caller asking for an operation that's structurally impossible against the snapshot (move from an empty slot, move to a full slot, address that doesn't exist in this library, drive bay whose identity we couldn't resolve at discovery).
3. The caller asking for an operation whose effect targets a drive whose identity we have low confidence in (derived identity, unresolved `sg_path`).

### 2.1 Deployment prerequisites — capabilities and groups

`Library::open(policy)` succeeding is necessary but not sufficient to issue state-changing CDBs. The Linux SG_IO kernel filter that affected discovery (see `INSTALL.md` "Host privileges") applies to *every* opcode Layer 2b uses:

- **MOVE MEDIUM (`0xA5`)** is not on the kernel whitelist.
- **INITIALIZE ELEMENT STATUS (`0x07`)** is not on the kernel whitelist.
- **PREVENT/ALLOW MEDIUM REMOVAL (`0x1E`)** is not on the kernel whitelist.
- **SSC LOAD/UNLOAD (`0x1B`)** is not on the kernel whitelist.

Without `CAP_SYS_RAWIO`, every one of these opcodes returns SG_IO `EPERM`. The handle has already opened the device R/W and revalidated identity by then, so the failure surfaces *as a SCSI error inside the operation*, not at handle acquisition.

**Layer 2b therefore requires the same two-step host setup as discovery:**

1. Operator user in the `tape` group (gates `open()` on `/dev/sgN`).
2. `CAP_SYS_RAWIO` on the binary — `setcap cap_sys_rawio+ep` on dev binaries, `AmbientCapabilities=CAP_SYS_RAWIO` on the production systemd unit.

The CLI and daemon should detect EPERM coming out of state-changing CDBs and surface the same `setcap` hint discovery does for `NoLibraries`. See §6 safety property 9.

### 2.2 Transport prerequisites — no-data CDBs

The transport API today (`SgTransport::execute_in`) handles only `SG_DXFER_FROM_DEV` — INQUIRY, RES, VPD pages. Every Layer 2b primitive is a **no-data CDB**: MOVE MEDIUM, INITIALIZE ELEMENT STATUS, PREVENT/ALLOW, and SSC LOAD/UNLOAD all carry the operation in their CDB bytes alone, with no data phase in either direction.

Layer 2b therefore lands a small Layer 1 expansion first (§7.0 below). It adds:

- `sg_io::execute_none(file, cdb, timeout_ms) -> Result<(), ScsiError>` — sends an `SG_DXFER_NONE` request through the kernel.
- `SgTransport::execute_none(cdb) -> Result<(), ScsiError>` — the trait method state-changing ops route through. `FixtureTransport` and `RecordingTransport` get the same method so safety tests still capture every CDB issued (regardless of direction).
- CDB builders for the four primitives as top-level modules in `remanence-scsi` (`move_medium`, `initialize_element_status`, `prevent_allow`, `load_unload`), matching the existing layout of `inquiry` and `read_element_status`.

**Discovery deliberately does not use `execute_none`** and Layer 2b deliberately does not issue data-in CDBs other than for `refresh()` and `rescan()`'s post-init RES. This is the structural reason a discovery pass is mechanically incapable of emitting a state-changing opcode — the data-direction split mirrors the safety boundary.

---

## 3. Operation set, with rationale

The first three sections cover the SMC-3 (changer) operations. The fourth covers the SSC (drive) side.

### 3.1 MOVE MEDIUM (`0xA5`)

The primitive every cartridge motion composes from. SMC-3 §6.10. CDB shape:

```text
byte 0   : 0xA5
byte 1   : reserved
byte 2..3: medium transport address (the robot — typically 0x0000)
byte 4..5: source element address
byte 6..7: destination element address
byte 8..9: reserved
byte 10  : flags (bit 0: INVERT)
byte 11  : control
```

We always use the library's robot address from `library.layout.robot_address` (almost always 0). INVERT (flip the cartridge during the move) is a two-sided-media feature that doesn't apply to LTO; we always set it to 0.

**Preflight validation list** (every item must hold; failure produces the corresponding `MoveError` variant):

| Check | Variant on failure |
|---|---|
| `src != dst` | `SameElement` |
| `src` and `dst` are each present in the snapshot (drive bay, slot, or IE port) | `AddressUnknown` |
| For source drive bay: `installed.is_some()` | `DriveBayUnresolved` |
| For destination drive bay: `installed.is_some()` | `DriveBayUnresolved` |
| For either side that is a drive bay, if the bay's `installed.identity_source` is `Derived`: policy allows it | `DerivedDriveBay` |
| Source is full (slot/IE: `full == true`; drive bay: `loaded == true`) | `SourceEmpty` |
| Destination is empty (slot/IE: `full == false`; drive bay: `loaded == false`) | `DestinationFull` |

Note the deliberate ordering: `DriveBayUnresolved` is checked **before** `SourceEmpty` / `DestinationFull`. A bay with `installed = None` is operationally unsafe regardless of what `loaded` says — the snapshot might be wrong about emptiness too — so we refuse on the identity gap rather than mislabelling the failure as "empty".

Occupancy is tracked by a dedicated `bool` (`Slot::full`, `IePort::full`, `DriveBay::loaded`) on every element kind. The cartridge tag (`Slot::cartridge`, `IePort::cartridge`, `DriveBay::loaded_tape`) carries the volume tag *when readable* and is `None` for an empty element *or* a full element with an un-barcoded cartridge. The two fields are independent: a `Slot { full: true, cartridge: None }` moved into a drive bay must produce `DriveBay { loaded: true, loaded_tape: None, … }` so downstream operations correctly recognise the bay as holding a cartridge.

**Outcomes:**

- On success, the handle's snapshot is patched (see §5.1).
- On CHECK CONDITION, the snapshot stays untouched and the typed error carries the sense bytes verbatim. Common cases worth noting: `02/0408` ("logical unit not ready, in process of becoming ready"), `05/3B0E` ("medium destination element full"), `05/3B0D` ("medium source element empty").
- On EPERM, the operator-facing hint about `CAP_SYS_RAWIO` fires (see §6 property 9).

### 3.2 INITIALIZE ELEMENT STATUS (`0x07`)

"Robot, please re-scan every slot and tell me what's actually there." Necessary when the operator inserted a cartridge via the front panel, or when the snapshot is suspect for any reason. SMC-3 §6.4.

**No range:** we issue the no-range form (`0x07`), not INITIALIZE ELEMENT STATUS WITH RANGE (`0x37`). The latter is for large libraries where a full rescan is slow; on our hardware (40-slot QuadStor, 40-slot MSL3040 partition) the full rescan is single-digit seconds and not worth the complexity. Revisit if/when we touch a 280-slot stack.

```text
byte 0 : 0x07
byte 1 : reserved
byte 2..4: reserved
byte 5 : control
```

**Outcomes:**

- On success, follow up with a post-init RES + reconciliation against the existing snapshot (§5.2). On reconciliation refusal, return `RescanError::SnapshotMismatch`.
- On CHECK CONDITION, bubble as `RescanError::ScsiError`.
- This is a state-changing CDB (it moves the robot — the picker arm scans every slot), so it's gated by the same handle/policy machinery as MOVE MEDIUM.

### 3.3 PREVENT / ALLOW MEDIUM REMOVAL (`0x1E`)

CDB byte 4 bit 0 = prevent (1) or allow (0). When prevent is set, the front-panel eject button and any operator-initiated mailslot eject are refused by the changer. Used for two scenarios:

- A multi-step operation that involves several MOVE MEDIUMs in sequence. Lock during the sequence, unlock when done.
- A daemon-level "session" abstraction (Layer 4+) where a long-running write job wants exclusive access for its duration.

**Best-effort cleanup, not a guarantee.** The library doesn't expose `Drop`-based PREVENT release as a *guarantee*. `Drop` doesn't run on `SIGKILL`, on aborts, on host crashes, or on power loss. What we do provide:

- `lock_removal()` returns a `RemovalLockGuard` whose `Drop` performs best-effort ALLOW. The guard is a hint to the linear scope of locking; it is not a promise of atomicity.
- The guard also has an explicit `release(self) -> Result<(), ScsiError>` for callers that want the failure surfaced.
- The audit hook (§6 property 7) records every lock/unlock transition, including failed ALLOWs, so an operator can tell from logs that a lock was left asserted.
- **Operational recovery** when a process dies while holding the lock: `rem unlock <library>`, or in the worst case a power-cycle / front-panel recovery. Document this in `INSTALL.md` alongside the host-privileges section.

### 3.4 SSC LOAD / UNLOAD (`0x1B`)

The SSC LOAD/UNLOAD CDB sent to a tape drive's `/dev/sgN`. UNLOAD is required before the changer can pluck a tape from a drive bay — the drive holds the cartridge mechanically until told to release it. LOAD is the polite explicit form for after the changer puts a cartridge in the bay; modern LTO drives load automatically on insert, but the explicit CDB is what we issue.

```text
byte 0 : 0x1B
byte 1 : 0   (immediate=0; we wait for completion)
byte 2..3: reserved
byte 4 : flags (bit 0: LOAD; bit 1: RETEN; bit 2: EOT; bit 3: HOLD)
byte 5 : control
```

For unload: byte 4 = `0x00`. For load: byte 4 = `0x01`. Both are no-data CDBs (use `execute_none`).

LOAD/UNLOAD live on `DriveHandle`, not `LibraryHandle`, because they talk to the drive's own `/dev/sgN`.

---

## 4. Domain model additions

Layer 2a's types stay as-is. Layer 2b adds:

### 4.1 `DriveHandle` — and how it's testable

Parallel to `LibraryHandle` but for a single drive. Acquired through the library handle, since opening a drive requires knowing which bay it sits in and what serial we expect.

**Testability:** the existing `LibraryHandle` is constructed by `Library::open_with(policy, transport_for)`, which consumes the `transport_for` closure exactly once (to open the changer). Drive opens need the same injection mechanism. Two viable shapes:

- *Stored factory*: `LibraryHandle` retains the transport factory (e.g. `Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>>`) passed to `open_with`. Drive opens reuse it. Production callers get `lib_handle.open_drive(bay, policy)` with no ceremony; tests construct the factory once and let it serve both changer and drive.
- *Per-call injection*: `LibraryHandle::open_drive_with(bay, policy, transport_for)`. Production gets a `open_drive(bay, policy)` convenience that passes `LinuxSgTransport::open_rw`. Tests pass their own factory.

We pick the **stored factory** approach. Two reasons:

1. The factory's lifetime obviously matches the handle's. Production callers and tests both call `lib_handle.open_drive(bay, policy)` the same way; no per-call ceremony at the test boundary.
2. Layer 2c's udev watcher will want to refresh drive `sg_path`s after hot-plug events, which means re-opening drives mid-session. A stored factory matches that future need.

```rust
impl LibraryHandle {
    /// Open the drive in `bay_address` for state-changing operations.
    /// Performs the same three-stage gate as Library::open:
    /// policy (the drive's library is on the allowlist AND the drive's
    /// identity_source is acceptable), transport open (R/W via the
    /// library's stored factory), and identity revalidation (standard
    /// INQUIRY confirming SequentialAccess + VPD 0x80 matching the
    /// recorded installed.serial).
    pub fn open_drive(
        &mut self,
        bay_address: u16,
        policy: &dyn AccessPolicy,
    ) -> Result<DriveHandle<'_>, OpenError>;
}

pub struct DriveHandle<'a> {
    drive: InstalledDrive,           // snapshot at the time of open
    library_serial: String,           // for audit trails
    transport: Box<dyn SgTransport>,  // open R/W to the drive's /dev/sgN
    _lib: std::marker::PhantomData<&'a mut LibraryHandle>,  // tied to the library handle's lifetime
}

impl DriveHandle<'_> {
    /// Issue SSC UNLOAD (0x1B byte 4 = 0).
    pub fn unload(&mut self) -> Result<(), DriveOpError>;

    /// Issue SSC LOAD (0x1B byte 4 = 1).
    pub fn load(&mut self) -> Result<(), DriveOpError>;

    pub fn library_serial(&self) -> &str { &self.library_serial }
    pub fn drive(&self) -> &InstalledDrive { &self.drive }
}
```

The `'a` lifetime ties a `DriveHandle` to its parent `LibraryHandle`; two drives in the same library can't be open simultaneously through this surface. That's a deliberate constraint matching the underlying changer-serializes-moves reality.

### 4.2 Error types

```rust
/// Errors from `LibraryHandle::move_medium` and other single-CDB
/// changer operations.
#[derive(Debug, Error)]
pub enum MoveError {
    /// The given element address isn't part of this library's snapshot.
    #[error("element address 0x{addr:04x} is not part of library {library:?}")]
    AddressUnknown { library: String, addr: u16 },

    /// Source slot/IE/drive bay has no cartridge.
    #[error("source element 0x{addr:04x} is empty")]
    SourceEmpty { addr: u16 },

    /// Destination is already occupied.
    #[error("destination element 0x{addr:04x} is full")]
    DestinationFull { addr: u16 },

    /// src == dst.
    #[error("source and destination are the same element 0x{addr:04x}")]
    SameElement { addr: u16 },

    /// A drive bay involved in the move has `installed = None` — Layer 2a
    /// couldn't resolve the drive's identity at discovery time. We refuse
    /// to operate on it regardless of `loaded_tape`. Re-discover after
    /// fixing the host (cap, drivers, hot-plug) or use `rescan()`.
    #[error("drive bay 0x{addr:04x} has unresolved identity — refuse to operate")]
    DriveBayUnresolved { addr: u16 },

    /// A drive bay involved in the move has `installed.is_some()` but
    /// `installed.sg_path.is_none()` — we know the drive serial but no
    /// `/dev/sgN` was bound to it at discovery. Only matters for ops
    /// that also need to talk to the drive's own SG node (composed
    /// `load`/`unload`).
    #[error("drive bay 0x{addr:04x} (serial {serial:?}) has no /dev/sgN bound — drive-side ops impossible")]
    DriveBayMissingDevice { addr: u16, serial: String },

    /// A drive bay's identity is `Derived` and the policy hasn't opted
    /// in. Duplicates the open-time check because operations can be
    /// more granular than handle acquisition.
    #[error("drive bay 0x{addr:04x} has derived identity ({serial:?}) and the policy does not allow it")]
    DerivedDriveBay { addr: u16, serial: String },

    /// SCSI changer returned CHECK CONDITION or some other error. Sense
    /// bytes (when available) are preserved verbatim inside ScsiError.
    #[error("SCSI error during move: {0}")]
    ScsiError(#[from] ScsiError),
}

/// Errors from `DriveHandle::unload` / `load`.
#[derive(Debug, Error)]
pub enum DriveOpError {
    #[error("SCSI error on drive: {0}")]
    ScsiError(#[from] ScsiError),
}

/// Errors from the composed `LibraryHandle::load`. Tells the caller
/// which phase failed and what the snapshot looks like afterwards.
#[derive(Debug, Error)]
pub enum LoadError {
    /// The MOVE MEDIUM phase failed. No drive operation attempted.
    /// Snapshot unchanged.
    #[error("changer MOVE phase failed: {0}")]
    Move(MoveError),

    /// MOVE MEDIUM succeeded, but opening the drive at its new bay
    /// failed (e.g., identity mismatch after the move, cap missing).
    /// Snapshot is *patched* — the cartridge is in the bay now.
    /// Snapshot is marked dirty; caller should `refresh()` and retry
    /// the drive LOAD via `open_drive(...).load()`.
    #[error("MOVE succeeded but drive open failed: {0}")]
    OpenDrive(OpenError),

    /// MOVE succeeded, drive opened, but `SSC LOAD` returned an error.
    /// Snapshot is *patched* — cartridge in bay. Snapshot marked
    /// dirty; recovery is operator-driven.
    #[error("MOVE succeeded but drive LOAD failed: {0}")]
    DriveLoad(DriveOpError),
}

/// Errors from the composed `LibraryHandle::unload`.
#[derive(Debug, Error)]
pub enum UnloadError {
    /// Opening the drive for UNLOAD failed. No MOVE attempted.
    /// Snapshot unchanged.
    #[error("drive open failed: {0}")]
    OpenDrive(OpenError),

    /// Drive UNLOAD CDB failed. No MOVE attempted. Snapshot unchanged.
    /// Operator may retry directly via `open_drive(...).unload()`.
    #[error("drive UNLOAD failed: {0}")]
    DriveUnload(DriveOpError),

    /// Drive UNLOAD succeeded, but the changer MOVE failed. The
    /// cartridge is still in the bay (the drive released it
    /// mechanically but it physically hasn't moved). Snapshot is
    /// **unchanged** because the drive bay still holds the cartridge
    /// from the snapshot's perspective. The operator can retry the
    /// MOVE phase via `move_medium(bay, dst)` directly — UNLOAD is
    /// idempotent at the drive level.
    #[error("drive UNLOAD succeeded but changer MOVE failed: {0}")]
    Move(MoveError),
}

#[derive(Debug, Error)]
pub enum RescanError {
    #[error("SCSI error during INITIALIZE ELEMENT STATUS: {0}")]
    ScsiError(ScsiError),

    /// The post-init RES disagreed with the existing snapshot on
    /// structural shape (drive_count differs, layout addresses moved).
    /// Caller should re-run `discover()` from scratch.
    #[error("post-init shape disagrees with prior snapshot: {0}")]
    SnapshotMismatch(String),
}
```

There is no `MoveError::CrossLibrary` — a `LibraryHandle` carries one library's snapshot; addresses from another library hit `AddressUnknown` and that's the right reading.

### 4.3 `LibraryHandle` API surface

```rust
impl LibraryHandle {
    /// Single-CDB move. See §3.1 for the validation list.
    pub fn move_medium(&mut self,
                       src: u16,
                       dst: u16,
                       policy: &dyn AccessPolicy) -> Result<(), MoveError>;

    /// Composed: MOVE from slot to bay, then SSC LOAD on the drive.
    /// Argument order: cartridge moves *from* `slot` *to* `bay`.
    pub fn load(&mut self, slot: u16, bay: u16, policy: &dyn AccessPolicy)
        -> Result<(), LoadError>;

    /// Composed: SSC UNLOAD on the drive, then MOVE bay → destination.
    /// `destination` is the slot to move the cartridge to. If `None`,
    /// uses the bay's `source_slot` (the slot the cartridge originally
    /// came from, recorded by Layer 2a from RES SVALID/source-address).
    /// If `source_slot` is also `None`, returns `MoveError::SourceEmpty`
    /// inside `UnloadError::Move`.
    pub fn unload(&mut self,
                  bay: u16,
                  destination: Option<u16>,
                  policy: &dyn AccessPolicy) -> Result<(), UnloadError>;

    /// Composed: move cartridge from `slot` to the first available IE
    /// port. Returns `MoveError::DestinationFull` if all IE ports
    /// already hold a cartridge.
    pub fn export(&mut self, slot: u16) -> Result<(), MoveError>;

    /// Composed: move cartridge from the first occupied IE port into
    /// `slot`. Returns `MoveError::SourceEmpty` if all IE ports are
    /// empty.
    pub fn import(&mut self, slot: u16) -> Result<(), MoveError>;

    /// INITIALIZE ELEMENT STATUS + post-init re-RES + reconciliation.
    /// See §5.2.
    pub fn rescan(&mut self) -> Result<(), RescanError>;

    /// Re-RES without forcing INIT. Cheap path. Preserves
    /// `InstalledDrive.sg_path` etc. via reconciliation. See §5.3.
    pub fn refresh(&mut self) -> Result<(), ScsiError>;

    /// PREVENT MEDIUM REMOVAL. Returns a guard whose Drop attempts
    /// best-effort ALLOW. Idempotent: a second `lock_removal()` while
    /// already holding a guard is allowed and returns a guard whose
    /// Drop is a no-op.
    pub fn lock_removal(&mut self) -> Result<RemovalLockGuard<'_>, ScsiError>;

    /// Explicit ALLOW MEDIUM REMOVAL, surfacing failure to the caller.
    /// Equivalent to calling `RemovalLockGuard::release(...)`.
    pub fn allow_removal(&mut self) -> Result<(), ScsiError>;

    /// Read-only access to the cached snapshot. Marked dirty after
    /// partial-success operations; callers that need a guaranteed-
    /// fresh view should `refresh()` before reading.
    pub fn library(&self) -> &Library;

    /// True if the snapshot has been marked dirty by a partial-success
    /// operation since the last `refresh()` / `rescan()`.
    pub fn is_dirty(&self) -> bool;

    /// Install an audit hook. See §6 property 7.
    pub fn set_audit_hook(&mut self,
                          hook: Box<dyn FnMut(&AuditEvent) + Send + 'static>);
}

pub struct RemovalLockGuard<'a> { /* … */ }

impl RemovalLockGuard<'_> {
    /// Explicit ALLOW, surfacing failure. After this, Drop is a no-op.
    pub fn release(self) -> Result<(), ScsiError>;
}

impl Drop for RemovalLockGuard<'_> {
    fn drop(&mut self) {
        // Best-effort ALLOW. Failure is logged via the audit hook;
        // the operator's recovery is `rem unlock <library>`.
    }
}
```

---

## 5. Snapshot refresh model

The cached `Library` snapshot becomes stale the moment a state-changing CDB returns. We pick the cheapest correct refresh per operation:

### 5.1 MOVE MEDIUM patch rules

After a successful single-CDB `move_medium(src, dst)`:

- Locate `src` and `dst` in the snapshot (both already exist per preflight).
- Read `src`'s cartridge state (occupancy flag + voltag, if any). Clear `src`:
  - Slot/IE: `full = false`, `cartridge = None`.
  - DriveBay: `loaded = false`, `loaded_tape = None`, `source_slot = None`.
- Write the state to `dst`:
  - Slot/IE: `full = true`, `cartridge = <tag-or-None>`.
  - DriveBay: `loaded = true`, `loaded_tape = <tag-or-None>`. The cartridge tag is carried forward as-is — `None` is preserved, so a full-but-unbarcoded slot moved into a bay yields `loaded = true, loaded_tape = None` and downstream ops correctly see the bay as occupied.
  - **`source_slot`** semantics: set `dst.source_slot = Some(src)` *only if `src` is a Storage slot*. For IE-port or drive-bay sources, set `source_slot = None`. (This matches RES SVALID semantics: the changer reports source-address only when the cartridge came from a storage slot, since that's the "natural home" relationship.)
- Patch is in-process; no I/O. For a guaranteed-fresh view, call `refresh()`.

**Partial-failure handling for composed ops** (see §4.2 for the error types):

| Operation | Phase that failed | Snapshot effect | Dirty? | Cause |
|---|---|---|---|---|
| `load(slot, bay)` | MOVE — CHECK CONDITION | unchanged | no | — |
| `load(slot, bay)` | MOVE — transport / timeout (completion unknown) | unchanged | **yes** | `CompletionUnknown` |
| `load(slot, bay)` | drive open, or drive LOAD CHECK CONDITION | MOVE patch applied, LOAD didn't run | **yes** | `PartialFailure` |
| `load(slot, bay)` | drive LOAD — transport / timeout | MOVE patch applied; drive may have loaded | **yes** | `CompletionUnknown` |
| `unload(bay, dst)` | drive open, or drive UNLOAD CHECK CONDITION | unchanged | no | — |
| `unload(bay, dst)` | drive UNLOAD — transport / timeout | unchanged | **yes** | `CompletionUnknown` |
| `unload(bay, dst)` | MOVE (after UNLOAD ok) — CHECK CONDITION | unchanged (cartridge still in bay) | no | — |
| `unload(bay, dst)` | MOVE (after UNLOAD ok) — transport / timeout | unchanged | **yes** | `CompletionUnknown` |
| `move_medium(src, dst)` | CDB returned CHECK CONDITION | unchanged | no | — |
| `move_medium(src, dst)` | CDB returned transport error / driver timeout | unchanged (but completion unknown) | **yes** | `CompletionUnknown` |
| `move_medium(src, dst)` | succeeded, IE-port endpoint | snapshot patched | **yes** | `VendorSemantics` |
| `rescan()` | INIT, RES, or reconciliation | see §5.2 | depends | `CompletionUnknown` when dirty |

The completion-unknown rows all rely on the same `completion_unknown(&ScsiError)` predicate (§6.10) — transport-level failures and raw ioctl errors leave physical state ambiguous, so the snapshot is marked dirty even when no patch was applied. CHECK CONDITION is the explicit-rejection case where the device received the CDB, declined to execute it, and returned sense; physical state is unchanged and the snapshot stays clean.

The `is_dirty()` accessor (§4.3) lets the daemon decide whether to read the cached snapshot or re-RES before reporting back to a UI client. Alongside it, `dirty_cause() -> Option<DirtyCause>` categorises *why* the snapshot is dirty so an operator-facing surface can pick the right wording without inferring from error-summary strings. The three causes are operationally distinct:

- **`PartialFailure`** — a composed op (`load` / `unload` / `export` / `import`) had an earlier CDB succeed and a later CDB fail. Snapshot patch from the earlier phase was applied; the later phase never ran.
- **`VendorSemantics`** — a single CDB *succeeded*, but the post-state depends on vendor flavor (the IE-port case: HPE parks visibly, QuadStor vaults). The snapshot's IE-full / slot-full bits can't be trusted without re-reading.
- **`CompletionUnknown`** — a state-changing CDB *failed* with a completion-ambiguous transport error (driver timeout, bus reset, host adapter reset). The robot or drive may have actually executed the operation even though we didn't get a clean status back. Also covers `rescan`'s post-INIT failures and `refresh`'s shape-mismatch outcome — both leave the snapshot stale via a different mechanism. See §6.10 for the transport-level shape.

The invariant is `is_dirty() == true ⟺ dirty_cause().is_some()`; the two flip together via a single internal helper inside the handle.

The CLI does **not** auto-refresh — after every dispatched op it consults `handle.dirty_cause()` and, when `Some(_)`, prints a recovery hint (`rem library <serial> --slots` + `rem rescan <serial> --allow <serial>`) with wording matching the cause so the operator can decide whether to inspect or rebuild the cache before retrying.

### 5.2 INITIALIZE ELEMENT STATUS — reconciliation

The whole element state is invalidated by definition (the changer's internal state was re-derived). Naively, we'd just take the post-init RES and overwrite `drive_bays`, `slots`, `ie_ports`. But that drops everything Layer 2a's tape-device join added — `installed.sg_path`, `sysfs_path`, vendor/product/revision, and the `DvcidAndInquiry` identity source. After such a downgrade, `open_drive` may not work for bays that previously did.

The fix is **reconciliation**, not replacement. After the post-init RES:

1. Build the post-RES `drive_bays / slots / ie_ports` collections in the usual way.
2. For each post-RES `DriveBay`:
   - Find the pre-RES bay with the same `element_address`.
   - If both have an `installed` and the serials match: copy `sg_path`, `sysfs_path`, `vendor`, `product`, `revision`, and `identity_source` from the pre-RES `InstalledDrive` into the post-RES one. The new bay keeps its post-RES `loaded_tape` / `source_slot` (those *did* potentially change).
   - If the pre-RES had an `installed` whose serial doesn't match the post-RES one: drop the pre-RES host-side data; the bay holds a different drive now (operator hot-swapped the drive, or firmware reset reassigned). `identity_source = DvcidInline`. Emit `RescanWarning::DriveReplaced { addr, old_serial, new_serial }`.
   - If post-RES has an `installed` but pre-RES didn't: this is a new drive identity in a previously-unresolved bay (firmware glitch resolved itself, or initial DVCID was partial). `identity_source = DvcidInline`, `sg_path = None`. Emit `RescanWarning::DriveAppeared { addr, serial }` — operator can rerun full `discover()` if they want the tape-device join.
   - If post-RES has no `installed` but pre-RES did: the bay's drive vanished from the changer's view. Emit `RescanWarning::DriveVanished { addr, old_serial }`.
3. If the post-RES `drive_count`, `slot_count`, or `ie_count` differs from the pre-RES `ElementLayout`, return `RescanError::SnapshotMismatch(…)` — the library shape changed (partition reconfiguration, firmware update), and the handle is no longer trustworthy. Caller must re-`discover()`.
4. After successful reconciliation, the snapshot is clean (`is_dirty() == false`).

The reconciliation warnings flow into the audit log via the audit hook (see §6 property 7), not into a return-value list — `rescan()` is expected to be invoked from the daemon and the operator-facing surface is "ok / mismatch / scsi error."

### 5.3 `refresh()` — element state only, no INIT

Cheap path. Re-RES against the existing transport, parse, run the **same reconciliation** as §5.2 against the existing snapshot. Returns `ScsiError` only — no `SnapshotMismatch` variant because refresh is the path callers use when they're *not* expecting structural change. If the shape does change unexpectedly, fire an `AuditEvent::Warning { warning: RescanWarning::ShapeMismatch { summary }, ... }` and return success with `is_dirty()` true; the daemon decides whether to escalate. Bay-level reconciliation observations (DriveReplaced / Appeared / Vanished) also fire as `AuditEvent::Warning` events on the success path.

---

## 6. Safety contracts

Layer 2b inherits Layer 2a's five safety properties (which is the point of the handle scaffolding). It adds five operation-level ones:

6. **Preflight against the snapshot.** Every state-changing op validates `src`/`dst`/full-empty/drive-bay identity/derived-identity-policy against the cached snapshot before any CDB goes out. Mistyped addresses fail without I/O; the kernel logs see nothing.

7. **Audit hook records intent *and* outcome — per CDB.** Granularity is **per state-changing CDB**, not per public operation. Composed ops (`load`, `unload`, `export`, `import`) emit one `Started` / `Finished` pair per CDB they issue, every event carrying the same `operation: AuditOp` context so the audit log can correlate per-CDB events back to one operator-level request. Read-only `refresh()` doesn't fire `Started`/`Finished`, but it *does* fire `Warning` events for reconciliation observations — operators want to know about a hot-swapped drive regardless of which entry point surfaced it.

   Modelled as a flat 4-variant enum so every event has exactly the fields its kind needs (no overloaded `Option<...>` fields, no surprise unwraps in hook code):

   ```rust
   pub enum AuditEvent<'a> {
       /// Preflight succeeded; the CDB is about to go out.
       Started {
           library_serial: &'a str,
           operation: AuditOp,    // composed-op context
           cdb: &'a [u8],         // the primitive's bytes — cdb[0] is the opcode
           at: std::time::SystemTime,
       },
       /// Preflight refused at the *public op* level. No CDB ever issued.
       /// Single event for the whole public op; no Started/Finished follow.
       Refused {
           library_serial: &'a str,
           operation: AuditOp,
           reason: &'static str,  // e.g. "DriveBayUnresolved"
           at: std::time::SystemTime,
       },
       /// CDB returned. `outcome` carries success or failure detail.
       Finished {
           library_serial: &'a str,
           operation: AuditOp,
           outcome: AuditOutcome,
           at: std::time::SystemTime,
       },
       /// Reconciliation produced a per-bay observation. Fires between
       /// `Started` and `Finished` for `rescan()`, and standalone for
       /// `refresh()` (which doesn't fire Started/Finished itself).
       Warning {
           library_serial: &'a str,
           operation: AuditOp,      // Rescan in both cases
           warning: RescanWarning,
           at: std::time::SystemTime,
       },
   }

   pub enum AuditOutcome {
       Success { duration: std::time::Duration, snapshot_patched: bool, dirty: bool },
       ScsiError { sense: Option<Vec<u8>>, summary: String, dirty: bool },
       Other { summary: String },
   }
   ```

   The `dirty` field on `ScsiError` matches `Success`'s — it tells audit consumers whether the failure left the cached snapshot in a state that no longer matches reality. Set to `true` when the CDB is state-changing and the failure mode leaves *completion ambiguous* (transport-level error / driver timeout — the CDB may have actually executed on the device side without us getting a clean status back). `false` for CHECK CONDITION (device explicitly rejected the CDB, physical state unchanged) and for non-state-changing ops where dirtiness doesn't apply (PREVENT/ALLOW).

   For a successful `move_medium`: one `Started` + one `Finished`. For a successful `load(slot, bay)`: `Started{cdb=0xA5…}` + `Finished{Success}` + `Started{cdb=0x1B…}` + `Finished{Success}`, every event carrying `operation = AuditOp::Load { slot, bay }`. For a preflight-refused op: a single `Refused` event with the variant name of the `MoveError` as the `reason` tag (static strings so the audit log has a low-cardinality, stable filter key). For a `rescan()` that observes a hot-swapped drive: `Started{cdb=0x07…}` + `Warning{DriveReplaced{…}}` + `Finished{Success}`. For a `refresh()` against the same hot-swapped drive: just `Warning{DriveReplaced{…}}` — no Started/Finished, because refresh is read-only.

   `AuditOp` variants whose source/destination is *resolved at runtime* — `Unload::dst` (from the bay's `source_slot`), `Export::ie` / `Import::ie` (first available / occupied IE port) — use `Option<u16>`. `None` only appears in `Refused` events for the case where preflight refused before resolution completed.

   Default hook is a no-op. The daemon installs a hook writing to systemd journal + an append-only file in `/var/log/remanence/`.

8. **PREVENT MEDIUM REMOVAL is best-effort released.** `lock_removal()` returns a `RemovalLockGuard` whose `Drop` attempts ALLOW; `Drop` doesn't run for SIGKILL / abort / power loss. Daemon paths use the guard *and* call `allow_removal()` explicitly in the success path, so the recovery on guard-Drop failure is detectable from logs. Operational recovery on stranded lock: `rem unlock <library>` or a power cycle.

9. **EPERM surfaces the same hint discovery does.** When a state-changing CDB returns SG_IO `EPERM`, Layer 2b detects it and the CLI prints the `setcap cap_sys_rawio+ep` hint, pointing at `INSTALL.md`'s "Host privileges" section. The daemon's systemd unit uses `AmbientCapabilities=CAP_SYS_RAWIO`, so this is a CLI/dev-host concern in practice; daemon production never hits it.

10. **Op-class SG_IO timeouts.** A single global timeout is wrong in both directions: too tight for slow ops (MSL3040 MOVE is 8–20 s, INIT can take minutes), wastefully loose for fast ones (INQUIRY is sub-100 ms). Each call site sets a `TimeoutClass` on the transport before its CDB:

    | Class | Used by | Window |
    |---|---|---|
    | `Inquiry` | INQUIRY / VPD (discovery, identity revalidation) | 5 s |
    | `PreventAllow` | PREVENT / ALLOW MEDIUM REMOVAL | 5 s |
    | `ReadElementStatus` | READ ELEMENT STATUS (discovery, post-INIT) | 60 s |
    | `Move` | MOVE MEDIUM (`move_medium`, and the MOVE phases of `load` / `unload` / `export` / `import`) | 120 s |
    | `InitElementStatus` | INITIALIZE ELEMENT STATUS (`rescan`) | 600 s |
    | `LoadUnload` | SSC LOAD / UNLOAD (`DriveHandle::load` / `unload`) | 600 s |

    Numbers are conservative upper bounds — a healthy real-hardware op completes well inside the window, but the kernel won't tear it down prematurely if the robot stalls and recovers. `READ ELEMENT STATUS` is special: every caller (Layer 2a discovery, identity revalidation, post-INIT reconcile in `rescan`, the `refresh` fast path) routes through the shared `issue_res` helper, which sets `TimeoutClass::ReadElementStatus` on the transport itself. That way the long window applies whether the call comes from cold discovery or from a Layer 2b handle that previously set a different class. If the SG_IO timeout *does* fire, the CDB surfaces as a `ScsiError::TransportError` with `driver_status = 0x06` (SG_ERR_DRIVER_TIMEOUT). That's a **completion-unknown** failure: the robot or drive may have actually executed the operation even though we didn't get a clean status back. The handle marks the snapshot dirty with `DirtyCause::CompletionUnknown` (§5.1) and the audit `Finished { outcome: ScsiError { dirty: true, .. } }` carries the same signal — so an audit-replay can reconstruct that the snapshot is untrustworthy from this point. The CLI prints the `CompletionUnknown`-flavor recovery hint ("the operation failed with a transport-level error; the device may have actually executed it even though the host didn't get a clean status back").

    The same `completion_unknown` predicate also covers `ScsiError::Io` (raw ioctl failure — rare, safer to treat as ambiguous than to claim clean rollback). It does **not** apply to `CheckCondition` (device explicitly rejected the CDB, physical state unchanged) or to pre-flight parse errors.

dwara2 coexistence still the prime constraint. The mechanism doesn't change: dwara2's library serial isn't on Remanence's allowlist, so `Library::open()` refuses it, no `LibraryHandle` is ever produced for it, no state-changing CDB reaches its `/dev/sgN`. A test enforces this end-to-end: an excluded library + a `move_medium` call → `OpenError::NotAllowed` returned without any TO_DEV CDB observed on any transport.

---

## 7. Implementation plan

Sliced into chunks. Each chunk lands as its own commit with passing tests.

### 7.0 (Layer 1 prerequisite) — no-data CDBs and the four builders

Lands in `crates/remanence-scsi/`, not the library crate:

- `sg_io::execute_none(&File, &[u8], u32) -> Result<(), ScsiError>` — `SG_DXFER_NONE` form of the existing `execute_in`. Compile-time struct assertions cover both paths.
- CDB builders: `move_medium::build_cdb(robot, src, dst, invert)`, `initialize_element_status::build_cdb()`, `prevent_allow::build_cdb(prevent: bool)`, `load_unload::build_cdb(load: bool)`. Each in its own module under `remanence-scsi/src/`. Tests assert byte-for-byte against SMC-3 / SSC examples.
- `SgTransport::execute_none(&[u8]) -> Result<(), ScsiError>` — trait extension.
- `LinuxSgTransport`, `FixtureTransport` get the implementation.
- A new `RecordingTransport` (in `remanence-library`'s `transport.rs`) wraps any `SgTransport` and tees every CDB into a shared log; replaces the inline test wrapper from Layer 2a's "no state-changing CDBs in discovery" test.

### 7.1 Layer 2b error vocabulary

`MoveError`, `DriveOpError`, `LoadError`, `UnloadError`, `RescanError`, `AuditEvent` types in `error.rs`. No behavior change yet; compile-checks the rest of the plan.

### 7.2 Snapshot patcher

Pure function `apply_move(library: &mut Library, src: u16, dst: u16) -> Result<MovePatch, MoveError>`. `MovePatch` records what changed; used by both the success path and the (eventual) audit hook. Source-slot semantics per §5.1.

### 7.3 `LibraryHandle::move_medium`

Wires `apply_move` + audit hook + `execute_none(build_cdb(...))`. Fixture-transport tests assert the CDB bytes and the snapshot delta.

### 7.4 Reconciliation logic + `refresh()`

Pure function `reconcile(old: &Library, new_element_status: ElementStatusData) -> (Library, Vec<RescanWarning>)`. Tested in isolation with synthetic snapshots. Then wire `refresh()`.

### 7.5 `rescan()`

INIT CDB + post-init RES + `reconcile()`. Fixture-transport tests cover all four cases from §5.2 (serial match, serial replaced, drive appeared, drive vanished), plus the layout-mismatch refusal.

### 7.6 `DriveHandle` + drive-side ops

Store the transport factory on `LibraryHandle`. Implement `open_drive` with the three-stage gate. `DriveHandle::unload` / `load` via `execute_none`. Tests assert identity revalidation refuses a wrong-serial response, and that the CDBs match SSC.

### 7.7 Composed `load` / `unload` / `export` / `import`

Pure composition over §7.3 + §7.6. Tests cover every partial-failure path from §5.1's table.

### 7.8 `RemovalLockGuard` + `lock_removal` / `allow_removal`

Guard with `Drop` doing best-effort ALLOW. Tests assert idempotency and Drop behavior.

### 7.9 CLI subcommands

Explicit-flag form (avoiding the positional `<src> <dst>` footgun):

```text
rem move    <library> --src 0x0400 --dst 0x0100
rem load    <library> --slot 0x0400 --bay 0x0100
rem unload  <library> --bay 0x0100 [--dest 0x0400]
rem export  <library> --slot 0x0400
rem import  <library> --slot 0x0400
rem rescan  <library>
rem lock    <library>
rem unlock  <library>
```

Each subcommand opens the library through `discover()` → `library.open(policy)` and dispatches. Policy is built inline from two global flags rather than a config file:

- `--allow <serial>` — every state-changing subcommand requires the target library's serial to appear on this list. May be repeated for multi-library invocations.
- `--allow-derived <serial>` — subset of `--allow`; opts a library into accepting `IdentitySource::Derived` drive bays (default-denied, see §6 property 8).

A pre-discovery allowlist gate in `run()` short-circuits before any SG_IO call: if the target library isn't on `--allow`, the CLI prints `error: library "<serial>" not on the --allow list — state-changing ops are refused` and exits 1, *without* probing `/dev/sg*`. This protects against the operator-fat-fingered-the-serial case, and is the reason the CLI's own recovery hints embed `--allow <serial>` in suggested follow-up commands.

EPERM on any state-changing CDB triggers the same `setcap` hint discovery already has; the hint substitutes the running binary's path via `std::env::current_exe()` so the suggested `setcap cap_sys_rawio+ep <path>` lands on the right file even when copy-pasted from an interactive shell.

### 7.10 Live test on akash

Move a cartridge in QuadStor's `mainlib` (`rem load 7CBAD9CF74 --slot 0x0400 --bay 0x0100`), verify with `rem libraries --slots`, unload it back. First true write-side smoke.

---

## 8. Testing strategy

Three tiers, paralleling Layer 2a's §8:

1. **Fixture-driven unit tests.** Snapshot patcher (§7.2), reconciliation (§7.4), CDB builders (§7.0), error mapping — all pure-function logic, tested with synthetic snapshots and `include_bytes!` fixtures.

2. **Recorded-transport tests** via `RecordingTransport` (§7.0). For every public state-changing op:
   - The exact CDB bytes match SMC-3 / SSC.
   - The order: preflight → audit `Started` → CDB → audit `Finished` → snapshot patch.
   - The snapshot before/after matches the expected delta.
   - On a canned CHECK CONDITION, no snapshot patch.
   - On EPERM, the operator hint fires.
   - Partial-failure paths produce the right phase-aware error and `is_dirty()` state.

   The safety pin from Layer 2a remains: a library *not* on the policy allowlist results in zero TO_DEV CDBs observed across the entire test, even when the test calls `move_medium`.

3. **Live integration against akash's QuadStor.** Move slot→bay, verify slot empty / bay loaded, move back, verify reversed. Behind `REMANENCE_LIVE_QUADSTOR=1` so hosts without `/dev/sg4` don't fail CI.

We deliberately don't gate Layer 2b's release on a production MSL3040 live test. QuadStor pass + recorded-transport coverage of corner cases is enough; the next datamover access window adds fixture-test deltas.

---

## 9. Open questions

1. **CHECK CONDITION sense classification.** A handful of sense codes are operationally distinguishable: `04/0408` ("becoming ready"), `05/3B0E` ("destination full"), `05/3B0D` ("source empty"), `06/2A01` ("mode parameters changed"). Worth a dedicated enum, or fold into `ScsiError` and let the daemon pattern-match on sense bytes? Lean: enum once we hit the third operational-need case.

2. **Per-operation policy granularity.** Today `AccessPolicy::allows(library_serial)` is binary. Should there be `allows_state_changing_op(library, op_kind)` so the policy can permit `rescan` but refuse `move_medium`? Operationally yes for read-mostly archival cases, but no concrete demand yet. Defer.

3. **Concurrent moves on a shared chassis (partitioned MSL3040).** Two `LibraryHandle`s for the same physical chassis, one per partition — can MOVE MEDIUMs run concurrently? The chassis robot is shared, so SCSI-level the changers serialize. Worth confirming with a live test; document the result.

4. **Audit log format details.** systemd journal vs append-only file vs both? Daemon concern, but `AuditEvent`'s shape matters here. Strawman above; revisit during daemon design.

5. **`refresh()` on EPERM.** A daemon that lost `CAP_SYS_RAWIO` between handle acquisition and a `refresh()` call gets EPERM on RES. Should this invalidate the handle? Lean toward yes — the daemon should detect and re-open from scratch. Implementation detail for §7.4.

---

## 10. Out of scope (revisited explicitly)

- **Tape data I/O.** READ, WRITE, READ POSITION, LOCATE, SPACE, etc. → Layer 3.
- **Catalog persistence.** Where the daemon stores "we put cartridge X into slot Y at time T" → Layer 4.
- **HTTP / event API.** Remote operators driving moves over network → Layer 5.
- **EXCHANGE MEDIUM (`0xA6`).** Atomic move-and-replace. Defer.
- **MODE SELECT.** Element address reassignment, library config writes. Out of scope.
- **Multi-host transactional moves.** Out of scope.
- **Drive cleaning automation.** Operator decides when.
- **udev integration.** Layer 2c.

---

## Appendix A — Worked example (mainlib on akash)

The first live Layer 2b run, post-implementation:

```text
$ rem libraries
7CBAD9CF74  HP MSL G3 Series  /dev/sg4  (4 drives, 40 slots [10 loaded], 4 IE)

$ rem load 7CBAD9CF74 --slot 0x0400 --bay 0x0100 --allow 7CBAD9CF74
ok: loaded slot 0x0400 → bay 0x0100

$ rem unload 7CBAD9CF74 --bay 0x0100 --allow 7CBAD9CF74
ok: unloaded bay 0x0100 → recorded source slot
```

CDB sequence (with their audit `Started` log entries preceding each):

1. `0xA5 00 00 00 04 00 01 00 00 00 00 00` — MOVE MEDIUM, robot 0, src 0x0400, dst 0x0100
2. `0x1B 00 00 00 01 00` — SSC LOAD on `/dev/sg0`
3. `0x1B 00 00 00 00 00` — SSC UNLOAD on `/dev/sg0`
4. `0xA5 00 00 00 01 00 04 00 00 00 00 00` — MOVE MEDIUM, robot 0, src 0x0100, dst 0x0400

After step 1, the handle's snapshot has bay `0x0100`'s `loaded_tape = Some("RMN001L9")` and `source_slot = Some(0x0400)`. After step 4, both are `None` and slot `0x0400` is `full = true` again. Net change on the physical library: zero. Net change on the operator's confidence in the daemon: large.

If step 2 had failed (drive open refused or LOAD CDB error), step 1's snapshot patch would still apply (`loaded_tape = Some("RMN001L9")`) and `is_dirty()` would return `true`; the CLI prints the **partial-failure** flavor of the recovery hint:

```text
warning: the operation partially succeeded — an earlier phase
         changed library state before the later phase failed.
         Inspect or recover before retrying:
             rem library 7CBAD9CF74 --slots                    # see current state
             rem rescan  7CBAD9CF74 --allow 7CBAD9CF74         # force re-derive
```

If step 4 had failed after step 3 succeeded, the snapshot would stay unchanged (`bay.loaded_tape` still `Some(...)`) and the operator can complete the move manually with `rem move 7CBAD9CF74 --src 0x0100 --dst 0x0400 --allow 7CBAD9CF74`.

**IE-port flow on QuadStor** (the appendix example that exposed §7.10's vendor-semantics finding):

```text
$ rem export 7CBAD9CF74 --slot 0x0400 --allow 7CBAD9CF74
ok: export issued for slot 0x0400

warning: the operation touched an IE port. Post-move state
         depends on vendor semantics (some libraries vault the
         cartridge rather than park it in the IE element).
         Confirm physical state before relying on it:
             rem library 7CBAD9CF74 --slots                    # see current state
             rem rescan  7CBAD9CF74 --allow 7CBAD9CF74         # force re-derive
```

QuadStor vaulted the cartridge to a hidden pool rather than parking it in element `0x1000`; HPE firmware would have parked it visibly in the IE element. The CLI doesn't try to guess — it dispatched the MOVE successfully, flagged the snapshot as `is_dirty()` because an IE endpoint was involved, and printed the vendor-semantics hint. Operator decides whether to follow up with `rem library --slots` (cheap, just re-reads the cached snapshot) or `rem rescan` (re-derives via `INITIALIZE ELEMENT STATUS` and reconciles).

---
