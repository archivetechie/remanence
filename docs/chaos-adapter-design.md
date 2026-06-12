# QuadStor Chaos Adapter Design

Status: sixth draft (Claude review — code-verified the `FixtureTransport` limit
and L1a/L1b split; de-risked `ModelTransport` via existing discovery fixtures +
proven injection seam; bounded its scope)
Date: 2026-06-09

## What changed in this revision (review summary)

The first draft put the single fault seam at Remanence's **parity-layer
`RawTapeSource`/`RawTapeSink`** (`remanence-parity/src/raw.rs`). That layer only
sees the data path (`read_record`, `write_fixed_block`, `write_filemark`,
`space_filemarks`, `locate_physical`, `position`) and returns `ParityError`. It
cannot see TEST UNIT READY, LOG SENSE/TapeAlert, MODE SENSE/SELECT, RESERVE,
READ ELEMENT STATUS, MOVE MEDIUM, INQUIRY, or discovery — which is where roughly
half the catalogue actually lives. Injecting there also means a synthetic fault
never travels through Remanence's *real* sense-parsing, dirty-state, retry, and
position logic — so the test proves less than it appears to.

This revision **moves the primary seam one layer down**, to the
`SgTransport` trait (`remanence-library/src/transport.rs`), which every SCSI
command — for both the **drive** and the **changer** — funnels through. Verified
against the code:

```rust
pub trait SgTransport: Send {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError>;
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError>;
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError>;
    fn set_timeout_for(&mut self, class: TimeoutClass) { ... }
}
```

`DriveHandle` holds `transport: Box<dyn SgTransport>`; the changer/library handle
holds the same boxed-dyn shape. `ScsiError::CheckCondition { sense: Vec<u8>,
bytes_transferred }` carries **raw sense bytes** (any fixed-format
SK/ASC/ASCQ), and `ScsiError::TransportError { status, host_status,
driver_status, info, sense }` carries kernel-style host/driver/transport
failures.

Review refinement: do **not** blindly use `TransportError` for every non-GOOD
SCSI status. Today Remanence maps `TransportError` to completion-unknown dirty
state. That is correct for DID_NO_CONNECT, timeouts, and real device loss, but
too strong for target-status cases such as RESERVATION CONFLICT (0x18), BUSY
(0x08), TASK SET FULL (0x28), and TASK ABORTED (0x40). The implementation should
either add a target-status variant (for example `ScsiError::TargetStatus`) or an
explicit no-dirty status mapping before using those statuses broadly. Until that
lands, status-only scenarios must say whether they intentionally dirty the
library snapshot.

Second refinement: `TransferOutcome { sense: Option<SenseInfo> }` exists, but
current Layer 3a code primarily handles EOM/ILI/filemark via fixed-format
CHECK CONDITION sense (`ScsiError::CheckCondition`), not via `Ok(...sense:
Some(...))`. `ChaosTransport` should synthesize the error shape that the current
caller actually consumes, then add tests before relying on the
`TransferOutcome::sense` path.

Third refinement: the existing `FixtureTransport` is a scripted response
replayer, not a virtual tape. It logs CDBs, returns queued `execute_in` buffers,
and treats `execute_none` / `execute_out` as successful no-ops. That is useful
for command-level and short-sequence tests, but it cannot remember a WRITE and
serve the same block back on READ. Full hermetic end-to-end coverage therefore
needs a new stateful `ModelTransport` that implements a small virtual
tape/changer model.

Net effect: a single `ChaosTransport<T: SgTransport>` wrapper is still the right
fault engine. It can run over scripted `FixtureTransport` tests first, over a new
stateful `ModelTransport` for the hermetic workhorse path, and over real
`LinuxSgTransport` for guarded QuadStor validation. The parity-layer `RawTape`
hook is demoted to an optional convenience (§Components 7), not the primary path.

Everything else in Codex's draft that was sound — the `qschaos` CLI, the harness
`chaos.py` seam, SQLite state, TOML scenarios, JSONL observability, guardrails,
the fidelity ladder — is kept and re-pointed at the transport seam.

## Purpose

Let the system harness and Remanence exercise the host-visible LTO/VTL fault
catalogue in `quadstor-chaos.md`, after the Stage-0 spike proved QuadStor's old
`sensectl`/`rwctl` ioctl path is not usable on this installation.

The adapter is a **behavioral fault layer**: it becomes the *device contract*
Remanence tests against. QuadStor stays as the optional real storage substrate;
it is no longer the fault-injection engine.

The marquee requirement is **MED-05**: a WRITE returns GOOD, and a later READ of
the same block returns mutated bytes with GOOD status. That must happen **below**
Remanence's parity/read logic so the Reed-Solomon layer actually sees bad data.
At the transport seam this is exactly an `execute_in` that calls the inner
transport and then mutates the returned buffer — the corruption rides up through
every real Remanence layer above it.

## Ground Truth (from `SPIKE-RESULT.md`)

- `mainlib` → `tl_id=0`; drive1 → `target_id=1`, `/dev/sg0`, `/dev/nst0`.
- Baseline QuadStor tape I/O works.
- ioctl 56 (`TLTARGIOCMODIFYSENSE`) → `EINVAL`; ioctl 55 reaches dispatch but
  `VDEVICE_RWCTL_ADD/DELETE` → `EPERM`. The removed daemon wrappers fail through
  the same lower path. **QuadStor native injection is dead** (vendor-confirmed
  abandoned).
- **A separate scratch VTL cannot be created**: `Failed to create VTL … Maximum
  VTL/Virtual Drives already configured` (license-limited). So real-tape (L2)
  runs must reuse `mainlib` under explicit guardrails. Hermetic coverage splits
  into **L1a** (`FixtureTransport` + `ChaosTransport`, scripted CDB tests) and
  **L1b** (`ModelTransport` + `ChaosTransport`, stateful virtual tape/changer);
  neither needs a spare VTL nor real tape.

This design uses QuadStor as a storage substrate where helpful, never as the
fault engine.

### Verified against Remanence code (2026-06-09)

Every seam/type/behavior this design depends on was confirmed present at HEAD, so
the implementer need not re-litigate:

- `SgTransport` is `transport.rs` with `execute_in/none/out` + a defaulted
  `set_timeout_for(TimeoutClass)` (`transport.rs:207,235`). `DriveHandle.transport:
  Box<dyn SgTransport>`; the boxed-dyn forwards `set_timeout_for` (`:251`).
- `ScsiError::TransportError` is mapped to **completion-unknown dirty** state —
  proven by `DirtyCause::CompletionUnknown` (`handle/mod.rs:209`) and the test
  `map_scsi_transport_error_is_completion_unknown` (`tape_io/mod.rs:1978`). This
  is *why* status-only faults need a separate shape (below).
- Boundary signals are consumed from **fixed-format CHECK CONDITION** sense via
  `map_scsi`, `write_eom_signal`, `ili_signed_information`,
  `space_residual_if_early_stop` (`tape_io/mod.rs`); descriptor-format sense
  "falls through" (test `map_scsi_descriptor_sense_falls_through_to_check_condition`).
  So Phase B emits fixed-format first.
- Install points: drive via `LinuxSgTransport::open` (`discovery.rs:146`); changer
  via `Library::open` → `LinuxSgTransport::open_rw` (`handle/mod.rs:1665`).
- **No** high-level TapeAlert/LOG SENSE reader exists today (only unrelated
  `0x4D…` hex constants). Phase D must add a minimal LOG SENSE 0x2E caller before
  any TapeAlert scenario can be acceptance-tested end to end.

## Design Goals

- Exercise as many catalogue entries as possible as deterministic, host-visible
  behaviors, at the lowest seam that still tests real Remanence logic.
- Preserve catalogue timing and statefulness: per-tape persistence, per-drive
  dirtiness, first-load optimization, EOM thresholds, UA queues, reservation
  ownership, library element state, TapeAlert snapshots.
- Run the **same fault engine** against (a) scripted `FixtureTransport` tests,
  (b) a stateful in-memory `ModelTransport`, and (c) a real QuadStor-backed
  `LinuxSgTransport`.
- Make MED-05 and EOM visible inside Remanence's real read/write path.
- Synthesize unit attentions, CHECK CONDITION sense, and selected target-status
  faults **without** physically disrupting the shared `mainlib` host; reserve
  true transport errors such as DID_NO_CONNECT for completion-unknown cases,
  with real disruption available as an opt-in L3 escalation.
- Log enough command context to prove which fault fired, where, and why.

## Non-Goals

- Do not resurrect QuadStor's abandoned injection CLIs.
- Do not modify QuadStor kernel modules for the first implementation.
- Do not claim physical fidelity for environmental/firmware/magnetic failures —
  emulate their host-visible consequences (sense + TapeAlert + timing).
- Do not depend on a separate scratch VTL (the install is at its configured
  VTL/drive limit).
- Do not attempt to test **d2tape** with this adapter. d2tape drives the real
  Linux `st`/`mt`/`tar`/`dd` path, which a Remanence-internal transport wrapper
  never sees. d2tape coverage needs an L4 kernel/TCMU/SCSI-target shim and is out
  of scope here (see §Scope Boundaries).

## Architecture

```text
system scenarios
  -> harness/seams/chaos.py
      -> qschaos scenario/state/log CLI          (author/arm/clear/inspect)
      -> Remanence CLI/daemon with REM_CHAOS_*    (engine reads scenario+state)
      -> optional QuadStor control-plane / real transport disruption (L3)

Remanence command path (drive AND changer)
  DriveHandle / ChangerHandle / discovery
    -> Box<dyn SgTransport>
        -> ChaosTransport<Inner: SgTransport>     <-- PRIMARY SEAM
            -> fault engine + persistent state + JSONL log
            -> Inner transport:
                 FixtureTransport   (L1a scripted CDB tests)
                 ModelTransport      (L1b stateful virtual tape/changer; new)
                 LinuxSgTransport    (L2 real, mainlib /dev/sgN under guardrails)

QuadStor (optional, L2/L3 only)
  -> mainlib (no scratch VTL available)
  -> real load/unload/read/write substrate
  -> optional iSCSI/session/device bounce for real transport loss (L3)
```

The seam is split only where the *surface* is genuinely different:

- **Drive + changer SCSI** (the bulk): one `ChaosTransport` wrapping
  `Box<dyn SgTransport>`, installed for both the drive handle and the
  changer/library handle.
- **Scenario authoring, state, guardrails, observability**: the `qschaos` CLI +
  harness seam (Python).
- **Real target disappearance** (L3 only, opt-in): Linux/iSCSI/device control,
  for cases where we want the actual kernel `sg` error, not a synthesized one.

## Components

### 1. `ChaosTransport<Inner: SgTransport>` — the fault engine (PRIMARY)

A new Remanence-side module/crate (`remanence-chaos`) providing:

```rust
pub struct ChaosTransport<T: SgTransport> {
    inner: T,
    engine: FaultEngine,   // scenario + state + RNG + log, shared via Arc<Mutex<…>>
    ctx: DeviceCtx,        // vtl, tl_id/target_id, drive vs changer, /dev path, loaded barcode
}
impl<T: SgTransport> SgTransport for ChaosTransport<T> { … }
```

Model it on the existing `RecordingTransport<T>` (`transport.rs:476`) — same
wrap-a-`SgTransport`-and-observe shape, already proven to compose with both
`LinuxSgTransport` and `FixtureTransport`. `ChaosTransport` adds the
mutate/short-circuit behavior `RecordingTransport` deliberately omits.

For each `execute_*` call the engine:

1. **Decodes the CDB opcode** (and operands) to learn the command kind and, for
   data ops, the target block — tracking current LBA from LOCATE/READ POSITION/
   READ/WRITE so faults can be LBA-scoped without a real position.
2. **Resolves context**: tape UUID/barcode (from loaded state), drive, partition,
   op kind, object/filemark phase when known, initiator/I_T nexus.
3. **Pre-call decision** — may short-circuit the inner call and return a
   synthesized result:
   - `Err(ScsiError::CheckCondition { sense, bytes_transferred })` with any
     SK/ASC/ASCQ. Phase B should emit fixed-format 70h/71h first because
     Remanence's current sense helpers consume fixed-format sense; descriptor
     72h/73h is future coverage.
   - For EOM/ILI/filemark boundary signals, prefer the current Remanence shape:
     fixed-format CHECK CONDITION sense with VALID/FM/EOM/ILI/INFORMATION set as
     needed. Only use `Ok(TransferOutcome { sense: Some(...) })` after a test
     proves the caller path consumes that field for the targeted operation.
   - `Err(ScsiError::TransportError { status, host_status, … })` only for
     completion-unknown transport/driver failures such as DID_NO_CONNECT or a
     synthetic timeout. Target-status scenarios such as RESERVATION CONFLICT
     (0x18), BUSY (0x08), TASK SET FULL (0x28), and TASK ABORTED (0x40) require
     the target-status error-shape decision above before they are considered
     high-fidelity.
   - A synthesized response **buffer** for INQUIRY-/LOG SENSE-/MODE SENSE-/READ
     ELEMENT STATUS-class reads (e.g. a TapeAlert page — see §TapeAlert).
   - A **delay** (becoming-ready / audit / robotic-move latency) before returning.
4. **Pass-through** to `inner` when no pre-call fault fires.
5. **Post-call mutation** — e.g. MED-05: after a successful `execute_in` READ,
   mutate the seed-selected byte range in `buf` and return the original success.
6. **State update + one JSONL event** (active fault id, op, LBA, status, sense,
   mutation summary, seed, state delta).

This is the **first-class path for MED-05**, every sense/TapeAlert fault, and
target-status faults once the status error shape is settled. The synthesized
`ScsiError` or response buffer flows up through Remanence's real sense decoder,
dirty-state machine, retry policy, and parity layer exactly as a real drive's
would.

**Installation point.** `LinuxSgTransport::open(path)` is constructed in
`discovery.rs`; `Library::open()` uses `LinuxSgTransport::open_rw(path)`;
`LibraryHandle::open_drive()` reuses the handle's stored transport factory.
Wrap the returned `Box<dyn SgTransport>` in `ChaosTransport` when
`REM_CHAOS_ENABLED=1`; otherwise return it untouched so production behavior is
byte-identical. The wrapper must forward `set_timeout_for` to the inner
transport before every real call. Prefer a small shared transport-factory helper
over scattered conditional wrappers, so discovery, changer open, and drive open
cannot drift.

### 2. `ModelTransport` — stateful hermetic device model (new)

A new in-memory or SQLite-backed `SgTransport` implementation used under
`ChaosTransport` for L1b. This is not the existing `FixtureTransport`.
`FixtureTransport` remains a scripted CDB-response fixture; `ModelTransport`
owns device state.

Minimum model:

- Drive state: loaded tape, current logical block, block size, filemarks,
  partition, BOT/EOD/EOM position, ready/not-ready windows, compression-neutral
  byte accounting.
- Tape state: barcode, generation, WORM/write-protect flags, virtual capacity,
  written blocks, corrupt ranges, mount counters, optimization flag, TapeAlert
  flags.
- Changer state: slots, drives, mailslot/door state, element addresses, barcode
  strings, inaccessible/empty/full flags.
- CDB handlers: INQUIRY, TEST UNIT READY, REQUEST SENSE, READ, WRITE, WRITE
  FILEMARKS, SPACE, LOCATE, READ POSITION, MODE SENSE/SELECT, LOG SENSE, LOAD/
  UNLOAD, READ ELEMENT STATUS, MOVE MEDIUM.

This makes Remanence perform real write/read and library workflows without
QuadStor: a WRITE updates the virtual tape, a READ returns the prior bytes, MOVE
MEDIUM updates the loaded-tape association, and LOG SENSE can expose TapeAlert.
`ChaosTransport` then injects the same faults against this model that it later
injects against `LinuxSgTransport`.

Build it deliberately small. The first version only needs fixed-block tape data,
filemarks, virtual capacity/EOM, loaded-barcode coupling, and enough changer
state to load a tape into a drive. Add encryption, reservations, and detailed
library states only when the catalogue row under test needs them.

**The hidden cost is discovery, and it is already solved.** Remanence's
`read_config()` / discovery issues INQUIRY, READ BLOCK LIMITS, and MODE SENSE
before any data op; `ModelTransport` must return byte-correct responses to those
or the device never opens — and that is where naive hand-fabrication burns days.
Two existing assets remove it:
1. **Reuse Remanence's own discovery fixtures.** The handle test suite already
   constructs working `DriveHandle`/`LibraryHandle`s over
   `Box::new(FixtureTransport::new().with_responses(r))` in ~20 places
   (`handle/mod.rs:1870, 2075, 3271, …`); each `r` is a correct discovery byte
   sequence. `ModelTransport` serves those same bytes for the static read-config
   phase and only adds *dynamic* state (data blocks, position, filemarks,
   loaded-barcode, capacity/EOM) on top.
2. **Optionally golden-capture from `mainlib`.** For higher fidelity, record real
   INQUIRY / READ BLOCK LIMITS / MODE SENSE / READ ELEMENT STATUS from `mainlib`
   once and replay them as `ModelTransport`'s static responses.

**Injection seam is already proven.** `ModelTransport` plugs into the exact
transport-factory closure those tests use to inject `FixtureTransport`, so it
bypasses sysfs `/dev/sg` discovery with no new hook — the same factory that
`ChaosTransport` wraps.

**Scope bound (important).** `ModelTransport` is a test double sized to what each
catalogue row needs, **not** a conformant SSC/SMC target. If it starts drifting
toward "reimplement QuadStor," stop — that is what L2 (`mainlib`) is for. Division
of labor: `ModelTransport` buys hermetic determinism for *logic*; `mainlib` L2
buys real-drive *fidelity*. Each catalogue row is proven at the lowest tier that
demonstrates what it claims.

### 3. `qschaos` CLI

Lives in this repo; operator + harness entrypoint. Owns scenario parsing/
validation, state DB init/migration, guardrails, QuadStor helper calls
(`scripts/qsvtl_resolve.py`, `vtconfig`, `mtx`, `mt`, future iSCSI controls),
and JSONL log inspection. It does **not** mutate bytes Remanence reads — that is
`ChaosTransport`'s job.

```text
qschaos scenario validate <scenario.toml>
qschaos scenario arm <scenario.toml> --state <state.db>
qschaos scenario clear --state <state.db>
qschaos scenario status --state <state.db> --json
qschaos log tail --state <state.db>
qschaos library snapshot --vtl mainlib --json
qschaos transport bounce --drive drive1 --mode iscsi-session   # L3 opt-in
```

### 4. Harness seam: `chaos.py`

Add beside `harness/seams/rem.py`. Seam paths:

```text
chaos.scenario.arm    chaos.scenario.clear    chaos.scenario.status
chaos.library.snapshot    chaos.transport.bounce
```

Flow:

```python
chaos.scenario.arm("scenarios/med05.toml")
locator = rem.tape.write_object(path, pool)
result  = rem.tape.verify_object(locator, expected_sha256)
events  = chaos.scenario.status()
```

When a scenario is armed the seam passes these into Remanence CLI/daemon calls
(and into the `rem-daemon` service env for daemon paths):

```text
REM_CHAOS_ENABLED=1
REM_CHAOS_SCENARIO=<scenario_id>
REM_CHAOS_STATE=/var/lib/replica/chaos/state.db
```

### 5. State store (SQLite)

Same schema as Codex's draft, keyed by logical identity (tape/drive/initiator),
not Linux device names. Add an explicit **persistence-scope** enum on every
fault and on corrupt ranges:

```text
scope ∈ { transient, until_reset, drive_session, initiator,
          tape, tape_until_optimized, drive, global }
```

Tables (unchanged from draft, plus `scope`):
`scenarios, faults(+scope), tapes, drives, library_elements, initiators,
corrupt_ranges(+scope), events`.

### 6. Scenario format (TOML)

Selector/trigger/action lowered into DB rows. Examples:

```toml
# MED-05 silent corruption — mutate on read-back, GOOD status.
id = "med05-rs-parity-basic"
seed = "2026-06-09-med05-001"
[[fault]]
catalogue_id = "MED-05"
target  = { tape = "RMN002L9", drive = "drive1" }
trigger = { op = "read", lba = 1234 }                       # CDB opcode 08h
action  = { status = "good", mutate = { mode = "xor", offset = 8192, length = 64 } }
scope   = "tape"
```

```toml
# RDY-01 first-load LTO-9 optimization, time-scaled for CI.
id = "rdy01-first-load"
seed = "2026-06-09-rdy01"
time_scale = 0.01
[[fault]]
catalogue_id = "RDY-01"
target  = { tape = "RMN003L9", drive = "drive1" }
trigger = { op = "test_unit_ready", when = "first_mount_unoptimized" }   # 00h
action  = { not_ready = { sk = "02", asc = "04", ascq = "01", duration_seconds = 2400 } }
scope   = "tape_until_optimized"
```

```toml
# EOM-01 early-warning on write — informational sense, EOM bit, residual.
id = "eom-ew-on-write"
seed = "2026-06-09-eom"
[[fault]]
catalogue_id = "EOM-01"
target  = { tape = "RMN004L9" }
trigger = { op = "write", lba_at_least = 2048 }             # 0Ah
action  = { check_condition = { rc = "70", sk = "00", asc = "00", ascq = "02", eom = true, information = "residual", bytes_transferred = "partial" } }
scope   = "tape"
```

```toml
# RES-01 reservation conflict — synthetic target status, no real reservation.
id = "res01-conflict"
[[fault]]
catalogue_id = "RES-01"
target  = { drive = "drive1", initiator = "host-B" }
trigger = { op = "any" }
action  = { target_status = { status = "0x18", dirty = false } }  # requires target-status error shape
scope   = "until_reset"
```

```toml
# LBP-01 guard check failure — drive detects bad CRC on host-written block.
id = "lbp01-guard-failure"
[[fault]]
catalogue_id = "LBP-01"
target  = { tape = "RMN006L9" }
trigger = { op = "write", when = "lbp_enabled" }
action  = { check_condition = { rc = "70", sk = "0B", asc = "10", ascq = "01" } }
scope   = "transient"
```

```toml
# MED-07 media-life via synthesized TapeAlert (LOG SENSE 2Eh).
id = "med07-media-life"
[[fault]]
catalogue_id = "MED-07"
target  = { tape = "RMN005L9" }
trigger = { op = "log_sense", page = "0x2e" }                # 4Dh
action  = { tape_alert = [7, 19] }                           # flags set in the page
scope   = "tape"
```

Scenarios may list **multiple ordered faults** to model cascades
(§Cascade modeling).

### 7. Optional parity-layer `RawTape` hook (secondary)

Keep a thin optional `ChaosRawTapeSource/Sink` only for faults that are far
easier to express in physical-block terms than in CDB terms (rare). Default off.
Everything in the catalogue is expressible at the transport seam; this exists so
we are not *forced* down to CDB encoding for a one-off.

### 8. Optional real transport disruption (L3)

For end-to-end realism (actual kernel `sg` ENXIO/EIO, real UA on relogin), the
CLI can bounce the iSCSI session / offline the device. Opt-in, guarded, and only
needed when a synthetic completion-unknown `TransportError` is not enough. Most
BUS-* cases are covered synthetically at L1a/L1b after the target-status
distinction is implemented.

## Fault engine details

### CDB dispatch (opcodes the engine recognizes)
`00` TEST UNIT READY · `03` REQUEST SENSE · `08` READ · `0A` WRITE · `10` WRITE
FILEMARKS · `11` SPACE · `12` INQUIRY · `1A`/`5A` MODE SENSE · `15`/`55` MODE
SELECT · `16`/`17` RESERVE/RELEASE · `5E`/`5F` PERSISTENT RESERVE IN/OUT · `1B`
LOAD/UNLOAD · `2B`/`92` LOCATE · `34` READ POSITION · `4D` LOG SENSE · `A2`/`B5`
SECURITY PROTOCOL IN/OUT (encryption) · `B8` READ ELEMENT STATUS · `A5` MOVE
MEDIUM. Unknown opcodes pass through untouched.

### Drive↔changer loaded-tape coupling (do not overlook)
Per-tape faults are *armed* by barcode (`target.tape = "RMN002L9"`), but the
**drive** transport sees only CDBs to `/dev/sgN` — it has no tape identity of its
own. Only the **changer** transport sees `MOVE MEDIUM` (which slot → which drive).
So the changer `ChaosTransport` must, on a successful load, write the
`drive_id → loaded barcode` association into shared state, and the drive
`ChaosTransport` must read it to resolve `target.tape` on every data op. Without
this handoff, per-tape drive-path faults (MED-*, EOM-*, CMP-*) can't bind to a
tape. On L2 this can be cross-checked against `qsvtl_resolve.py` / the loaded
barcode; on L1b the model's MOVE MEDIUM handler updates the same association
(and L1a scripted tests may seed it directly). The two `ChaosTransport`
instances therefore share one `FaultEngine`/state DB, not independent copies.

### Sense synthesis
Build a 70h/71h/72h/73h sense buffer from `(response_code, sk, asc, ascq,
fm/eom/ili, information)`; return via `ScsiError::CheckCondition`. Phase B
should build fixed-format 70h/71h first because `map_scsi`,
`ili_signed_information`, `space_residual_if_early_stop`, and
`write_eom_signal` currently parse fixed-format sense. Response code 71h gives
**deferred** sense (HOST-01 "GOOD then later failure"). Descriptor-format 72h/73h
support is lower priority until a real Remanence caller needs it.

### Target-status synthesis
Do not conflate target status with host/driver transport failure. Current
`ScsiError::TransportError` marks Remanence state dirty. That is correct for
completion-unknown failures, but not necessarily for BUSY, RESERVATION CONFLICT,
TASK SET FULL, or TASK ABORTED. Phase B should either add a dedicated target
status variant to `ScsiError`, or document and test the conservative dirty
semantics for each status-only fault before marking it covered.

### TapeAlert provider
On `LOG SENSE` page `0x2E`, synthesize the 64-flag page from the tape/drive
alert state (set bits for the scenario's flags). This is the single mechanism
behind every alert-driven family (MED-07/10/11, CLN-*, ENV-*, HW-03, FW-*).
The catalogue's flag->hex table is the source of truth.

Current gap: Remanence does not appear to expose a high-level TapeAlert/LOG
SENSE reader today. `ChaosTransport` can synthesize the page as soon as a caller
issues CDB 0x4D, but Phase D must add a minimal Remanence API/CLI path that
actually asks for LOG SENSE page 0x2E before TapeAlert scenarios can be
acceptance-tested end to end.

### Read-buffer mutation (MED-05/06)
Deterministic: `mutation = f(seed, tape_id, lba, offset)`; same seed reproduces
byte-identical corruption. MED-06 arms only after a mount-cycle counter
increments (write looked clean; read-back fails after reload).

### Determinism
Seeded RNG keyed on `(scenario_id, tape_id, drive_id)` per the catalogue, so a
seed yields a byte-identical fault timeline across runs. **Caveat for L1b↔L2
parity:** MED-05 keys on `(seed, tape_id, lba, offset)`, so identical corruption
across L1b (model) and L2 (real `mainlib`) holds *only if the write produces the
same LBA sequence* in both. That requires a fixed block size and
compression-neutral payload (the catalogue's Gap-8 warning) — otherwise the same
seed lands the flip at a different physical block on real tape. State the block
size + compressibility assumption in any cross-fidelity scenario.

### Persistence scopes
Enforce the `scope` enum (§4) on every fault/range: e.g. RDY-01 is
`tape_until_optimized`, MED-05 ranges are `tape` (survive everything), RES-01 is
`until_reset`, BUS-01 UA is `initiator` (one per nexus, cleared after first
command), KMS reachability is `global`.

### Cascade modeling
A scenario's ordered fault list can express the catalogue's cross-layer cascades
(e.g. SAS reset → `TransportError(DID_NO_CONNECT)` on the in-flight command →
UA `06/29/00` on the next command from that initiator → `BUSY 0x08` during a
recovery window → success). The engine advances cascade state per command.

### Hardware compression & virtual capacity
To address the catalogue's finding that hardware compression causes
non-deterministic physical EOM arrival (Gap #8), the fault engine/model tracks
uncompressed written bytes and LBA natively. EOM thresholds (EOM-01, EOM-02) are
evaluated against a configured virtual capacity for the tape, abstracting away
the unpredictability of SLDC compression while maintaining deterministic
boundary transitions for the test harness.

## Fidelity ladder

```text
L0  in-memory Remanence unit-test fake
L1a ChaosTransport over FixtureTransport — scripted CDB tests, CI, no media state
L1b ChaosTransport over ModelTransport — stateful virtual tape/changer, CI   <-- workhorse
L2  ChaosTransport over LinuxSgTransport — real QuadStor mainlib (guarded)
L3  real Linux transport/device disruption (iSCSI bounce, device offline)
L4  kernel/TCMU/scsi_debug/patched-target shim — exact kernel-facing SCSI
    (only needed for tools that bypass Remanence, e.g. d2tape — out of scope)
```

First target: **L1a for the command-level skeleton**, then **L1b for nearly the
whole catalogue** once `ModelTransport` exists. Use L2 for the marquee data-path
and library-backed paths against `mainlib`, and L3 for a couple of real transport
cases. Do not block on L4.

## Coverage matrix (the point of this revision)

Seam key: **T** = ChaosTransport (CDB), **TA** = TapeAlert page via T,
**ST** = state model surfaced via T, **H** = harness/control-plane, **L3** =
real transport disruption (opt-in). Fidelity = best hermetic level reachable.
Rows marked `L1` mean `L1b` for stateful end-to-end workflows; simple one-command
sense/status cases can also run at `L1a` with scripted `FixtureTransport`.

| Catalogue | Seam | Mechanism | Fidelity | Notes |
|---|---|---|---|---|
| MED-01 unrecoverable read | T | CHECK COND SK3/11/00 on READ at LBA | L1 | |
| MED-02 unrecoverable write | T | CHECK COND SK3/0C/00 on WRITE (deferred via 71h) | L1 | |
| MED-03 write append/position | T | SK3/50/xx on WRITE-after-SPACE/LOCATE | L1 | |
| MED-04 recorded entity/EOD not found | T | SK3/14/00\|03 on SPACE/LOCATE out of range | L1 | |
| **MED-05 silent corruption** | T | mutate `execute_in` buffer, GOOD status | L1 | marquee; deterministic |
| MED-06 latent error on read-back | T+ST | arm after mount-cycle increment | L1 | |
| MED-07 media life/nearing | TA+ST | TapeAlert 7/19 via LOG SENSE; GB/mount counters | L1 | |
| MED-08 not-data-grade/unsupported | T | SK3/5h 30/01 | L1 | |
| MED-09 blank tape/EOD at BOT | T | SK8 00/05; 0-byte read | L1 | |
| MED-10 CM/directory corruption | TA+ST | TapeAlert 15/18/51 + degraded search state | L1 | |
| MED-11 system-area r/w failure | TA+T | TapeAlert 52/53; load-time Not Ready | L1 | |
| EOM-01 early-warning on write | T | fixed-format CC with EOM bit + residual; maps to success-with-EW | L1 | |
| EOM-02 PEWZ | T | fixed-format CC with EOM bit, 00/07, earlier threshold | L1 | |
| EOM-03 physical overflow | T | SK0Dh 00/02 hard cap | L1 | |
| EOM-04 EOM on read at EW | T | EOM bit on READ | L1 | low priority |
| LBP-01 guard check failed | T | SKB/3 10/01 | L1 | addresses gap #7 |
| LBP-02 protection method error | T | SK5 10/05 | L1 | addresses gap #7 |
| CMP-01 wrong-gen cannot write | T+ST | SK7 30/05 per cartridge generation tag | L1 | |
| CMP-02 wrong-gen cannot read | T+ST | SK3 30/02 | L1 | |
| CMP-03 cleaning cart as data | T+ST | SK2 30/03 | L1 | |
| CMP-04 M8/Type-M confusion | T+ST | 30/00\|02 | L1 | |
| CMP-05 WORM overwrite | T+ST | SK7 30/0C; per-cart WORM flag | L1 | |
| CMP-06 WORM integrity | T+ST | SK7/3 30/0D | L1 | |
| CMP-07 write protected | T+ST | SK7 27/00; per-cart WP flag | L1 | |
| CMP-08 cannot format | T | 30/06 | L1 | |
| CMP-09 first-load write restriction | T+ST | SK7 30/05 on first-load of old media | L1 | LTO-7+ specific quirk |
| RDY-01 LTO-9 optimization | T+ST | becoming-ready SK2 04/01 + delay; one-time flag | L1 | time-scaled |
| RDY-02 generic not-ready | T | SK2 04/00..07 on TUR/open | L1 | |
| RDY-03 load/thread failure | T | SK2 thread/unthread failure | L1 | |
| RDY-04 unload failure | T | SK4 53/01 | L1 | |
| RDY-05 calibration (non-9) | T | SK2 04/01 short | L1 | |
| RES-01 reservation conflict | T+ST | target status 0x18 by initiator | L1* | needs target-status error shape or tested dirty semantics |
| RES-02 stale reservation | T+ST | target status 0x18 until PREEMPT modeled | L1* | same |
| BUS-01 UA after reset/power-on | T+ST | SK6 29/xx, one per initiator | L1 | |
| BUS-02 mode/inventory-changed UA | T | SK6 28/00, 2A/01, 3F/0E | L1 | |
| BUS-03 BUSY / TASK SET FULL | T | target status 0x08 / 0x28 in recovery window | L1* | needs target-status error shape or tested dirty semantics |
| BUS-04 aborted / parity | T | target status 0x40 / SKB 47/00 | L1* | same |
| BUS-05 DID_NO_CONNECT / target lost | T / L3 | `TransportError` host_status DID_NO_CONNECT (synthetic); real bounce optional | L1/L3 | |
| BUS-06 illegal request/invalid field | T | SK5 24/00, 26/00, 20/00, 25/00 | L1 | |
| ENC-01 LU access not authorized | T+ST | SK2 74/71; `kms`/key state | L1 | |
| ENC-02 incorrect/unknown key | T+ST | SK3/B 74/03,01,04 | L1 | |
| ENC-03 KMS unreachable | T+ST | SK5/2 74/61–64,6E; global `kms_reachable` | L1 | |
| ENC-04 encryption policy violation | TA+T | TapeAlert 61; SK7/5 74/0D,21 | L1 | |
| ENC-05 unencrypted/mode mismatch | T | 74/02, 74/09 | L1 | |
| CLN-01 clean now | TA+ST | TapeAlert 20; 00/17; dirtiness counter | L1 | degrade-before-fail |
| CLN-02 clean periodic | TA | TapeAlert 21 | L1 | |
| CLN-03 expired cleaning cart | TA+ST | TapeAlert 22; cleaner use-count | L1 | |
| CLN-04 invalid cleaning cart | T | SK5 30/0A,03 | L1 | |
| CLN-05 cleaning failure/too-soon | T | 30/07 | L1 | |
| LIB-01 MOVE source empty | T(changer)+ST | SK5 3B/0E on MOVE MEDIUM; element state | L1 | |
| LIB-02 MOVE dest full | T(changer)+ST | SK5 3B/0D | L1 | |
| LIB-03 inventory-changed UA | T(changer) | SK6 28/00 post-move | L1 | |
| LIB-04 target reset during move | T(changer) | SK6 29/02 | L1 | |
| LIB-05 barcode unreadable | ST+T | empty/garbled VolumeTag in READ ELEMENT STATUS | L1 | |
| LIB-06 gripper/picker failure | T(changer) | SK4 15/01, 40–44 | L1 | |
| LIB-07 accessor/teach failure | T(changer)+ST | SK4 hardware error; library offline | L1 | |
| LIB-08 door/magazine open | T(changer) | SK2 04/18 | L1 | (QuadStor's old `sensectl` example) |
| LIB-09 inaccessible slot (MSL3040) | ST | element marked inaccessible | L1 | vendor realism |
| LIB-12 mailslot accessed | T(changer)+ST | door-open / inventory UA | L1 | addresses gap #12 |
| LIB-10 re-inventory/audit window | T(changer)+delay | changer Not Ready + latency | L1 | |
| LIB-11 medium not present | T | SK2 3A/00 | L1 | |
| ENV-01 over-temperature | TA+T | TapeAlert 36 then Not Ready/drop | L1 | |
| ENV-02 cooling fan | TA | TapeAlert 26 | L1 | |
| ENV-03 voltage/brown-out | TA+T | TapeAlert 37; SK4 | L1 | |
| ENV-04 power loss w/ tape | TA+ST | lost-stats alert; mid-position + audit + UA | L1 | |
| ENV-05 post-power audit window | T+delay | UA blast + Not Ready | L1 | |
| FW-01 firmware download fail | TA | TapeAlert 34; SK5/4 | L1 | |
| FW-02 firmware mismatch | ST | inconsistent media-status until 2nd load | L1 | |
| HW-01 hardware A (reset) | TA+T | TapeAlert 30; SK4 44/00 | L1 | |
| HW-02 hardware B (POST) | TA+T | TapeAlert 31; drive dead | L1 | |
| HW-03 predictive failure | TA+T | TapeAlert 38; SK1/0 5D/00 | L1 | |
| HW-04 microcode panic/forced eject | TA+T | TapeAlert 58/16; SK6 29/04 | L1 | |
| HOST-01 deferred write error | T | response code 71h/73h on next WRITE/WRITE FILEMARKS | L1 | |
| HOST-02 ILI / block-size mismatch | T | fixed-format CC with ILI + residual on READ | L1 | |
| HOST-03 two-filemark EOD | T | FILEMARK/EOD outcomes on READ/SPACE | L1 | tests rem's read engine |
| HOST-04 auto-rewind device confusion | — | **N/A for Remanence** (uses sg, not `/dev/st0`) | — | harness lint only |
| HOST-05 UA auto-consumed by st | — | **N/A for Remanence** (st-driver behavior) | — | relevant to d2tape only |
| HOST-06 ioctl errors (MT*) | — | **N/A for Remanence** (st `MTIO` ioctls; rem uses sg) | — | relevant to d2tape only |

Summary: roughly 60 entries are reachable at **L1b hermetic** once the stateful
model, target-status error shape, and LOG SENSE caller are in place. A smaller
command-level subset is immediately reachable at L1a. Three HOST-* entries are
`st`-driver-specific and **do not apply** to Remanence's sg path (they matter
only if we later cover d2tape via an L4 shim).

## Observability

One JSONL event per intercepted command (Codex's field list, kept):
`ts, scenario_id, fault_id, catalogue_id, operation(+cdb_opcode), tape_id/barcode,
drive_id, backend, lba_before, lba_after, requested_bytes, returned_bytes,
inner_called, status, sense(response_code,sk,asc,ascq,fm,eom,ili,information),
tape_alert, mutation_summary, state_delta, seed`. For L2 add
`vtl, tl_id, target_id, sg_path, nst_path, loaded_barcode`. Scenario acceptance
asserts on the event log, not just process exit status. Cross-check against
QuadStor `CmdDebug` / `ExtendedLogging` / `DebugTapePosition` on L2.

## Guardrails

No scratch VTL exists, so L2 runs reuse `mainlib` — guardrails matter more, not
less:

```text
--library mainlib                      requires --allow-active-library
overwrite/rewind/unload/bounce faults  require  --destructive
real transport bounce (L3)             requires --allow-device-disruption
scenario arm with a non-test barcode   requires --allow-barcode
ChaosTransport over LinuxSgTransport   requires REM_CHAOS_ALLOW_REAL=1
```

L1a/L1b need none of these — they touch no device. Keep the chaos adapter
incapable of issuing commands to production drives by default (device
allow-list); snapshot VTL state before destructive L2 runs.

## Implementation plan

### Phase A — scenario parser, state, CLI skeleton
`qschaos validate/arm/clear/status`; SQLite schema (+scope); JSONL writer; two
scenarios (MED-05, EOM-01); unit tests for parsing + deterministic trigger
selection.

### Phase B — `ChaosTransport` + scripted command-level tests (L1a)
`ChaosTransport<T: SgTransport>` with CDB dispatch, sense synthesis, transport-
error synthesis, state updates, JSONL events, and read-buffer mutation. Install
over `FixtureTransport` for small scripted tests. Land MED-05-on-a-seeded-READ,
MED-01, EOM-01, RDY-02, BUS-01, and HOST-01 first. Decide the target-status
error shape before claiming RES-01/BUS-03 coverage. Keep production
byte-identical when `REM_CHAOS_ENABLED` is unset. L1a tests assert that the
right CDB triggers the right error/mutation/log event; they do not claim a full
write/read object workflow.

### Phase C — `ModelTransport` stateful virtual tape/changer (L1b)
Build the minimal virtual device model needed for end-to-end Remanence workflows:
fixed-block WRITE/READ, filemarks, SPACE/LOCATE/READ POSITION, virtual capacity
and EOM, loaded-barcode coupling through MOVE MEDIUM, and READ ELEMENT STATUS.
Run a Remanence write/read scenario entirely through `ChaosTransport` over
`ModelTransport`; assert GOOD-status-bad-bytes is detected by parity and that EOM
maps through the real fixed-format sense path.

### Phase D — TapeAlert provider + stateful catalogue models
Add a minimal Remanence LOG SENSE/TapeAlert query path, then synthesize LOG
SENSE 0x2E; per-tape/drive counters (dirtiness, GB written, mount count,
optimized flag, cleaner use-count); RDY-01 first-load optimization with time
scaling; CLN/MED-07/ENV/HW/FW alert families.

### Phase E — changer transport + library faults
Install `ChaosTransport` on the changer/library handle; MOVE MEDIUM + READ
ELEMENT STATUS faults (LIB-01..11), inventory/door UA, audit-window latency,
element shadow state.

### Phase F — harness seam + QuadStor L2 + optional L3
`harness/seams/chaos.py`, `bindings.toml` rows; one end-to-end scenario (write,
arm MED-05 on read, assert parity reaction). Wire `ChaosTransport` over
`LinuxSgTransport` against `mainlib` under guardrails. Add one opt-in L3
disruption (iSCSI bounce / device offline) for BUS-05.

## Acceptance criteria

First useful cut (L1a, hermetic scripted):
- `qschaos scenario validate` catches malformed scenarios.
- `qschaos scenario arm` persists state visible to Remanence.
- Chaos disabled ⇒ `ChaosTransport` forwards calls unchanged; CDB log,
  transferred byte counts, and buffers match the unwrapped fixture.
- A scripted READ through `ChaosTransport<FixtureTransport>` triggers MED-05:
  the returned buffer is mutated while status remains GOOD, and the JSONL event
  records the seed, LBA, byte range, and mutation summary.
- EOM-01 surfaces the EOM bit + residual through fixed-format CHECK CONDITION
  and Remanence maps it to success-with-EW.
- A sense fault (e.g. RDY-02 04/01) flows through Remanence's real
  sense/dirty-state path, not a pre-baked error.
- Target-status faults either use a new no-dirty target-status error path or
  have explicit tests documenting the conservative dirty behavior.
- JSONL log shows active fault, CDB opcode, LBA, seed, mutation summary.

First hermetic end-to-end cut (L1b, `ModelTransport`):
- Remanence writes fixed-block data to `ModelTransport`, reads it back, and gets
  the original bytes when chaos is disabled.
- MED-05 mutates bytes inside a later `execute_in` READ while returning GOOD.
  Assert the corruption was **detected** (parity-mismatch counter / event-log
  entry), not merely that recovery succeeded — otherwise a parity layer that
  silently reconstructs is indistinguishable from one that never saw the flip.
- Corrupt **beyond** the parity tolerance and confirm the read fails loudly
  rather than returning bad data as good.
- A virtual-capacity EOM scenario reaches the same fixed-format CHECK CONDITION
  path Remanence uses with real drives.
- MOVE MEDIUM updates the drive→barcode association used by per-tape drive-path
  faults.

First QuadStor-backed cut (L2, `mainlib`, guarded):
- A write/read scenario runs through `mainlib` with `ChaosTransport` over
  `LinuxSgTransport`.
- MED-05 injected against a real QuadStor-backed read; same seed ⇒ same LBA +
  byte offsets as L1b under the stated fixed-block/compression-neutral
  assumptions.
- TapeAlert page synthesized on a real LOG SENSE round-trip after the
  Remanence LOG SENSE caller exists.
- Cleanup leaves the drive unloaded / documented pre-run state.

## Scope boundaries (honest)

- **Remanence (sg path): near-total after prerequisites.** Roughly 60/70
  entries are reachable at L1b once `ModelTransport`, target-status handling,
  and LOG SENSE querying are added; the rest are L2/L3 or out of scope.
- **d2tape: not covered.** It uses the kernel `st`/`mt`/`tar`/`dd` path; a
  Remanence-internal transport wrapper never sees those commands. Covering
  d2tape needs an L4 shim (TCMU / `scsi_debug` / patched VTL target) so the
  kernel itself emits the fault. Deliberately out of scope here.
- **HOST-04/05/06: N/A for Remanence** — `st`-driver/`MTIO`-ioctl behaviors that
  Remanence's raw-sg path doesn't traverse. Keep as harness lints; revisit only
  with the d2tape/L4 effort.
- **Multi-initiator faults are single-nexus simulations.** `ChaosTransport`
  wraps one transport = one I_T nexus, so RES-01/02 and the per-initiator UA
  (BUS-01) cannot reproduce a *real* second-initiator race. `target.initiator =
  "host-B"` is scenario config that tells the engine *when* to emit `0x18`/UA, to
  test Remanence's **reaction** to contention — not a real concurrent host. A
  genuine two-host race needs a second real initiator against `mainlib` (L3+),
  which is out of scope here.
- **True physical phenomena** (PHY signal integrity, magnetic bit-rot, mechanical
  wear): out of scope — we emulate host-observable signatures only, per the
  catalogue's own boundary.
- **Sense-tuple ground truth**: the catalogue's SK/ASC/ASCQ are T10/vendor-doc
  values; validate against the real LTO-9 SCSI Reference (IBM GA32-0928) on L2,
  per the catalogue Caveats.

## Open questions

- Exact factory seam to install `ChaosTransport`: inside `discovery.rs` open fns,
  in `LibraryHandle::open_drive`, or a dedicated transport-factory hook gated by
  `REM_CHAOS_ENABLED`? (Prefer the narrowest point that covers both drive and
  changer opens.)
- `ModelTransport` ownership: put it in `remanence-library` test utilities,
  a new `remanence-chaos` crate, or this adapter repo as an integration-test
  helper? Prefer the place that lets Remanence CLI/daemon tests use it without
  enabling chaos in production builds.
- Target-status error shape: add `ScsiError::TargetStatus`, add a no-dirty
  mapping around current `TransportError`, or intentionally accept dirty
  semantics for some status-only faults?
- LBA tracking: reconstruct position purely from observed LOCATE/READ POSITION/
  READ/WRITE in `ChaosTransport`, or read it from shared state the harness seeds?
- State location: `/var/lib/replica/chaos` (harness-owned) vs Remanence config
  dir (daemon+CLI share naturally)?
- RDY-01 timing: real sleeps × `time_scale`, or a virtual clock the harness
  drives?
- For L3, safest real mechanism on this host: iSCSI logout/login, SCSI device
  delete/rescan, or QuadStor service bounce? (Only `mainlib` exists — pick the
  least collateral option.)
