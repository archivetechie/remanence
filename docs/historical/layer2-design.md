# Layer 2a Design: Discovery and Topology

**Status:** draft v0.2 (revised 2026-05-17 in response to `docs/layer2-design-feedback.md`)
**Author:** owner (drafted with Claude)
**Companion to:** `docs/spec-v0.2.md` and `plan.txt`
**Crate:** `crates/remanence-library/`

This document specifies the **read-only discovery side** (Layer 2a) of Remanence's Layer 2: how the daemon and CLI learn what libraries, drives, slots, and cartridges are present on a host, and what stable identities they should be keyed by. **State-changing library operations** (MOVE MEDIUM, INITIALIZE ELEMENT STATUS, mailslot import/export, etc.) and **udev-driven re-discovery** are sibling concerns (Layer 2b and Layer 2c respectively); both will land in follow-up docs. Where this doc and spec v0.2 disagree, spec v0.2 wins.

---

## 1. Scope

### Goals

- Given a Linux host with one or more tape libraries attached, produce a complete, accurate **snapshot** of:
  - Each *logical library* reachable on the host. (Whether two libraries happen to live in the same physical chassis as HPE-style partitions, or are physically separate boxes, is incidental and not surfaced as model state — see spec v0.2 §6.5.)
  - For every library: its element layout (robot/drive/slot/IE address ranges), its drive bays with the serial numbers of the drives actually installed, its slots and IE ports with their current cartridge tags.
  - The `/dev/sgN` path each device is reachable on, matched by stable identity (serial) rather than enumeration order.
- Surface these as plain Rust value types (`Library`, `Drive`, …), pure data, cheap to clone and pass around.
- Be **safe by default**: a discovery pass on a shared host (e.g. the production `datamover` where one of the MSL3040's logical libraries is owned by `dwara2`) issues only read-only SCSI commands and does not touch any library's medium.
- Be **fast**: a full discovery on a typical host (one or two libraries, a dozen `/dev/sg*` devices) completes in well under a second on real hardware.
- Be **portable**: parser code stays Linux-independent; only the transport layer (`SG_IO` ioctl) and the sysfs walker are Linux-specific. Tests can run anywhere fixtures can be loaded.

### Non-goals (for this doc)

- **State-changing operations.** MOVE MEDIUM, INIT ELEMENT STATUS, mailslot mapping, etc. live in Layer 2b (`remanence-library::ops`, to follow). Discovery returns *views* only.
- **Event-driven re-discovery.** udev integration (rescan on hot-add) is layered on top of discovery later; see *Refresh model*. Discovery itself is a one-shot pure function.
- **Multi-host discovery.** Remote libraries (iSCSI, NDMP) and multi-host SAN topologies are deferred. This layer talks to libraries via local `/dev/sg*` only.
- **Tape contents.** What's on a tape (manifests, file lists) is Layer 3+ (`remanence-tape`). Layer 2 stops at the cartridge tag visible to the library's barcode reader.
- **Operations that touch libraries Remanence does not own.** Coexistence with `dwara2` on the LTO-7 logical library is a hard requirement; see *Safety & coexistence*.

---

## 2. Background — what the real captures taught us

The design choices below are all grounded in fixtures already in the repo (`fixtures/` and `fixtures/real-hardware/`). The non-obvious lessons:

1. **The MSL3040 in production is one chassis partitioned into two logical libraries.** It is not a dual-chassis stack and the drives are not dual-attached SAS. Each logical library appears as its own `/dev/sgN` medium changer with its own VPD 0x80 serial (HPE form: `<chassis_id>_LL<NN>`). Remanence sees two libraries; the operator knows they live in one box. (See out-of-tree memory `project_msl3040_partitioning.md`.)
2. **SCSI bus topology does not map cleanly to logical-library membership.** On `datamover`, each physical HBA cable carries drives from both libraries. The "drives on the same host:channel as the changer" heuristic that `mtx` uses would associate drives with the wrong library. Discovery must derive bay→drive identity from SCSI semantics, not bus topology.
3. **`READ ELEMENT STATUS` with DVCID=1 and CurData=1 returns drive serials inline.** This is the documented HPE behavior (HPE 20-STG-TAPESCSIREF-ED5 §READ ELEMENT STATUS / Data Transfer Element status page) and is verified working on real MSL3040 firmware 3350 and on QuadStor's emulator. Discovery still probes the alternate CurData polarity as a compatibility fallback, but the primary contract is the DVCID+CurData request.
4. **Variable-length serials are normal.** MSL3040 chassis serials are 15-char; LTO drive serials are 10-char. The parsers already handle this through `page_length`.
5. **`HP` vs `HPE` vendor strings.** LTO-7 drives identify as `HP`; LTO-9 and the chassis itself identify as `HPE`. Tests must accommodate both.
6. **PVOLTAG block is 36 bytes, not 32.** 32-byte ASCII identifier + 4-byte reserved/sequence number per SMC-3 Table 47. The parser bug this exposed is fixed.

---

## 3. Domain model

All types are plain `Clone + PartialEq` value structs. No interior mutability, no async, no lifetimes leaking into the public API. Snapshots are returned by value; callers re-call `discover()` when they need fresh state.

### 3.1 `Library` — one logical library

A logical library is the operational unit: one SCSI medium changer with its own drives, slots, and import/export ports. Whether several libraries happen to live in one physical chassis (HPE-style partitioning) is incidental; Remanence treats each one independently.

```rust
pub struct Library {
    /// Stable, human-readable identity. Whatever the changer returns in
    /// its VPD 0x80 unit-serial field. On HPE MSL partitioned libraries
    /// the form is `<chassis-id>_LL<NN>` (e.g. `DEC418146K_LL02`); on
    /// other vendors it can be anything. Used as the primary key for
    /// selecting which library to operate on.
    pub serial: String,

    /// /dev/sgN path of the medium-changer device **observed at
    /// discovery time only.** Not durable — Linux can re-enumerate sg
    /// nodes across reboots, HBA rescans, hot-plug. Operations must
    /// revalidate (open + INQUIRY VPD 0x80) and confirm the serial
    /// still matches before issuing commands; see §5.2.
    pub changer_sg: PathBuf,

    /// Current sysfs attachment path observed at discovery time (e.g.
    /// `/sys/class/scsi_device/2:0:13:0`). Same caveat as `changer_sg`
    /// — `host:channel:id:lun` shifts when cabling or HBA enumeration
    /// changes. Recorded for diagnostics; never trusted as identity.
    pub changer_sysfs: PathBuf,

    /// What the changer's standard INQUIRY reports: vendor / product /
    /// revision / device_type. Re-exported from remanence-scsi.
    pub changer_inquiry: remanence_scsi::Inquiry,

    /// Optional chassis-level designator from VPD 0x83. Captured for
    /// diagnostics and operator UX (a CLI can highlight libraries that
    /// share a designator so the operator sees "these came from the
    /// same physical chassis"). No operational logic depends on it.
    ///
    /// Kept as an opaque normalized form rather than a raw `u64`,
    /// because VPD 0x83 designators are variable-shape and
    /// vendor-dependent — NAA-5 is the HPE form, but other vendors may
    /// use EUI-64, SCSI-name-string, or vendor-specific blobs.
    pub chassis_designator: Option<DeviceDesignator>,

    /// Element-address layout. v0.1 of this layer derives the layout
    /// from RES page headers (the byte counts implicitly carry per-type
    /// counts); MODE SENSE 1Dh cross-check is deferred — see §7.5.
    pub layout: ElementLayout,

    /// Drive bays in this library, ordered by element address. A bay
    /// is present whether or not a usable host attachment was found
    /// (see `DriveBay::installed`).
    pub drive_bays: Vec<DriveBay>,

    /// Storage slots, ordered by element address.
    pub slots: Vec<Slot>,

    /// Import/export ports, ordered by element address. May be empty
    /// (the MSL3040 on datamover reports 0 IE ports for both of its
    /// logical libraries).
    pub ie_ports: Vec<IePort>,
}

/// Opaque VPD 0x83 designator. Carries enough type information to be
/// compared meaningfully and rendered cleanly; renders as hex in the
/// CLI by default.
pub struct DeviceDesignator {
    pub designator_type: DesignatorType,   // NAA-5, EUI-64, T10VendorId, ...
    pub raw: Vec<u8>,                       // the bytes as the device returned them
}
```

### 3.2 Why a flat list of `Library`s, not a nested chassis model

Earlier drafts of this design grouped `Library`s under a `Chassis` parent type. Per spec v0.2 §3.1 and §6.5 we deliberately don't model chassis as a first-class concept: there are no operations that cross logical-library boundaries, drives belong to exactly one library, and shared-chassis relationships are operator concerns rather than runtime state. `chassis_designator` (above) is kept only because a CLI can use it to highlight "these libraries share a box" — it never gates a code path.

### 3.3 `ElementLayout`

```rust
pub struct ElementLayout {
    pub robot_address: u16,    // usually 0
    pub drive_start: u16,
    pub drive_count: u16,
    pub slot_start: u16,
    pub slot_count: u16,
    pub ie_start: u16,
    pub ie_count: u16,
}
```

In v0.1 this is derived from the RES page-header byte counts. A later revision may add MODE SENSE 1Dh as a canonical cross-check (see open question §10.2). If MODE SENSE eventually disagrees with RES, the implementation prefers the RES-observed values (because they're what the descriptors we actually parsed report) and raises a `LayoutMismatch` warning.

### 3.4 `DriveBay` and `InstalledDrive` — separating the bay from its current occupant

The bay is a structural feature of the library; the drive is the (replaceable) hardware currently sitting in the bay. Splitting the two lets discovery return a complete library topology even when host-side drive matching is incomplete or unsafe.

```rust
pub struct DriveBay {
    /// SCSI element address for this bay within the changer's address
    /// space. Stable for the lifetime of the library's partitioning
    /// configuration.
    pub element_address: u16,

    /// Drive currently installed in this bay, with what we know about
    /// it. `None` if the changer reports the bay but no drive identity
    /// could be resolved (DVCID off and no safe topology mapping, or
    /// the host-side /dev/sgN is unreadable). A `Some(_)` with
    /// `sg_path = None` is fine — the bay's drive serial is known but
    /// no host attachment was found.
    pub installed: Option<InstalledDrive>,

    /// Tape currently loaded into this drive's bay (the cartridge's
    /// voltag from the RES descriptor), or None if the bay is empty.
    pub loaded_tape: Option<String>,

    /// If loaded, the element address of the storage slot from which
    /// the tape was moved (`source_address` in SMC-3 terms). Useful for
    /// reasoning about what an unload would put back.
    pub source_slot: Option<u16>,
}

pub struct InstalledDrive {
    /// The serial number reported by the drive currently in this bay.
    /// Authoritative source is the RES DVCID block; when DVCID is
    /// available the result is cross-checked against the drive's own
    /// INQUIRY VPD 0x80 and a mismatch fails discovery.
    pub serial: String,

    /// Confidence in this serial — see §4.2.1. Only `Authoritative`
    /// values are safe for state-changing operations on a shared-HBA
    /// deployment; `Derived` values must be opted-in explicitly.
    pub identity_source: IdentitySource,

    /// Vendor / product / revision from the drive's standard INQUIRY,
    /// when a matching /dev/sgN was reachable.
    pub vendor:   Option<String>,
    pub product:  Option<String>,
    pub revision: Option<String>,

    /// /dev/sgN of THIS drive (whose INQUIRY VPD 0x80 matched `serial`).
    /// `None` when host-side matching was incomplete or refused (e.g.,
    /// DVCID unavailable on a multi-library-per-HBA topology).
    pub sg_path:    Option<PathBuf>,
    pub sysfs_path: Option<PathBuf>,
}

pub enum IdentitySource {
    /// Returned inline by the changer in the RES DVCID block. Trustable.
    DvcidInline,
    /// Cross-confirmed: DVCID inline AND independently matched by VPD 0x80
    /// on the corresponding /dev/sgN.
    DvcidAndInquiry,
    /// Topology-derived (drives on same host:channel as the changer,
    /// sorted by SCSI ID, matched by ordinal to the RES drive-element
    /// addresses). Only safe on deployments where this vendor convention
    /// has been validated AND where no multiple libraries share an HBA.
    /// Discovery emits `DriveMappingDerived` warnings for these.
    Derived,
}
```

### 3.5 `Slot` and `IePort`

```rust
pub struct Slot {
    pub element_address: u16,
    /// Trimmed volume tag of the cartridge in this slot, or None if empty.
    /// See "Barcode normalization" below.
    pub cartridge: Option<String>,
}

pub struct IePort {
    pub element_address: u16,
    pub cartridge: Option<String>,
    /// Whether this port accepts imports right now (RES `inenab` flag).
    pub import_enabled: bool,
    /// Whether this port accepts exports right now (RES `exenab` flag).
    pub export_enabled: bool,
}
```

**Barcode normalization.** The 32-byte primary VOLTAG identifier from RES is interpreted as ASCII, stripped of trailing spaces and NULs (matching what we already do in `remanence_scsi::read_element_status::trim_voltag`), and surfaced as a Rust `String`. If the resulting string is empty, `cartridge` is `None`. Non-ASCII tags (which would be a hardware misconfiguration) yield an empty string and a `MalformedVoltag` warning; the raw bytes are still available on the underlying `Element` for forensic inspection. Cleaning cartridges are not flagged in the model — the CLI may pattern-match the volume tag (HPE convention: `CLN…`) for the `(cleaning)` hint shown in §5.3, but discovery itself does not classify.

### 3.6 Identity rules at a glance

| What | Identity | Stability | Source |
|-|-|-|-|
| Library | `library.serial` | Stable until partitioning is reconfigured at the chassis | Changer's VPD 0x80 |
| Drive | `installed.serial` + `IdentitySource` | Moves with the drive | RES DVCID inline (authoritative); VPD 0x80 of `/dev/sgN` (corroborating) |
| Drive bay | `(library.serial, element_address)` | Stable across drive swaps | RES element address |
| Tape (cartridge) | `voltag` | Globally unique within a deployment by operator convention; duplicates across libraries are a warning | RES descriptor / library barcode reader |
| Tape location (where the cartridge is now) | `{ library_serial, kind: Slot(addr) \| Drive(serial) \| ImportExport(addr) \| Exported }` | Changes with every move | Derived from RES |
| Chassis (informational) | `library.chassis_designator` | Stable per physical chassis | VPD 0x83 |
| `/dev/sgN`, SCSI ID, sysfs path | (current attachment hint) | **NOT stable** | Linux enumeration |

A consequence worth being explicit about: **the catalog (Layer 4) keys cartridge/manifest records off `voltag` alone, not `(library.serial, voltag)`.** A tape that is physically exported from one library and imported into another remains the same tape; its location updates, its identity does not.

The runtime maps `voltag` to a current location for every operation; operations re-resolve before sending CDBs.

---

## 4. Discovery algorithm

`pub fn discover() -> Result<DiscoveryReport, DiscoveryError>` is the entry point. It does the following, in order:

### 4.1 Enumerate `/dev/sg*`

Glob `/dev/sg*` and `stat` each. For every present device:
1. `INQUIRY` (standard, EVPD=0).
2. Classify by peripheral device type:
   - `0x01` — sequential access → tape drive candidate
   - `0x08` — medium changer → library candidate
   - anything else (disk, enclosure, controller, …) → skip silently
3. Record the device's sysfs path (`/sys/class/scsi_device/<H>:<C>:<I>:<L>`), looked up via `readlink /sys/class/scsi_generic/sgN/device`.

### 4.2 Per-changer: probe the library

For each medium-changer device:

1. **`INQUIRY VPD 0x80`** → `library.serial` (e.g. `DEC418146K_LL02`).
2. **`INQUIRY VPD 0x83`** → parse descriptors, normalize one for `library.chassis_designator` (informational). Preference order: NAA > EUI-64 > SCSI Name String > T10 Vendor ID. Unrecognized designator types are kept as opaque bytes.
3. **`READ ELEMENT STATUS`** with VOLTAG=1, DVCID=1, CurData=1, element_type=0 (all):
   - **Allocation length and element-count strategy** (revised from the earlier 0x0100 cap, which would truncate a fully-populated MSL3040 stack):
     - First call: alloc_len = 8 (just the 8-byte header), num_elements = 0xFFFF. The target returns the header without the body; from `byte_count_of_report` we learn the exact size the descriptors need.
     - Second call: alloc_len = `byte_count + 8`, num_elements = 0xFFFF. The target sends the full descriptor stream.
     - If the target refuses the 8-byte probe (some firmware insists on a body-sized buffer), retry once with alloc_len = 1 MiB and num_elements = 0xFFFF — this comfortably covers a 7-module 280-slot MSL3040 plus drives, IE ports, and DVCID-inflated descriptors (worst case ≈ 25 KiB).
   - Parse the response into the existing `ElementStatusData`.
   - For each `DataTransfer` element, populate a `DriveBay`. If the descriptor carries a DVCID identifier, populate `installed` with `IdentitySource::DvcidInline` (to be upgraded to `DvcidAndInquiry` in §4.3); if not, leave `installed = None` for now and handle in §4.2.1.
   - For `Storage` elements, populate `Slot`.
   - For `ImportExport` elements, populate `IePort`.
   - For `MediumTransport`, ignore (we only record `layout.robot_address`).

#### 4.2.1 DVCID fallback ladder — and what to do when no safe answer exists

DVCID+CurData is verified on HPE firmware 3350 and on QuadStor, but other libraries we haven't tested might still omit the identifier block, honor only `element_type=4`, or invert the gating. The fallback ladder, on a per-changer basis:

- **Retry A**: re-issue RES with `element_type=4, VOLTAG=1, DVCID=1, CurData=1` (drives-only).
- **Retry B**: same shape, `CurData=0` (some firmware inverts).
- **Vendor-specific paths** (e.g., HPE-specific INQUIRY pages or LOG SENSE pages that report drive serials per bay): on the table for future vendor support but not specified here.

When none of the above yields DVCID identifiers, the discovery does **not** silently substitute a topology-derived mapping. Instead it considers two cases:

- **Safe topology** — the changer's sysfs `host:channel` carries exactly the expected number of tape devices and no *other* logical library on the same host has its own changer (so the mapping is unambiguous). The discovery emits `IdentitySource::Derived` per drive and a `DriveMappingDerived { library, method: "host:channel ordinal" }` warning. State-changing handle acquisition for any such library requires the operator to explicitly opt in to derived mappings (see §5.2).
- **Unsafe topology** — the changer shares `host:channel` with another logical library's drives (the production MSL3040 partitioning case). The discovery refuses to assign drives to bays at all and emits `DriveMappingUnavailable { library }`. `DriveBay::installed` stays `None` for the affected bays; the rest of the library topology (slots, IE ports, robot, voltags) is still returned.

This is a deliberate "loud partial discovery is better than wrong-looking full discovery" policy. The catalog and any operation that needs a drive serial will see `None` and refuse to act; the operator sees a clear warning rather than a plausible-looking misassignment.

### 4.3 Match drive identities to `/dev/sgN`, with cross-check

For every tape-classified device that wasn't skipped in 4.1:

1. **`INQUIRY VPD 0x80`** → tape serial.
2. Lookup this serial across all libraries' drive bays. If exactly one bay's `installed.serial` matches:
   - Set `installed.sg_path` and `installed.sysfs_path` to this tape device's paths.
   - Fill in `installed.vendor / product / revision` from the device's standard INQUIRY.
   - Upgrade `identity_source` from `DvcidInline` to `DvcidAndInquiry`.
3. If a tape device's serial doesn't match any library's bay → emit `DiscoveryWarning::UnclaimedTape { sg_path, serial }`. Not fatal.
4. If a tape device's serial matches more than one library's bay → emit `DiscoveryError::SerialAmbiguous`. Structurally impossible unless firmware is misbehaving.
5. After the loop, any bay whose `installed = Some(_)` with `sg_path = None` keeps that state: we know what drive should be there but couldn't find its host-side device. Emits `DiscoveryWarning::UnresolvedDrive` so operations on that bay can refuse cleanly.

### 4.4 Return

A `DiscoveryReport` containing a `Vec<Library>` ordered by `library.serial` (lexicographic), plus a per-device warning list. Determinism matters for human-readable output and snapshot-based tests.

### 4.5 Error model

```rust
/// Fatal errors: discovery couldn't produce a meaningful report at all.
pub enum DiscoveryError {
    /// Could not enumerate /dev/sg* at all — likely wrong privileges.
    EnumerationDenied { cause: IoErrorKind },

    /// The host has no /dev/sg* devices, or none classifiable as a tape
    /// library after INQUIRY.
    NoLibraries,

    /// A drive's serial appeared in more than one library's RES response.
    /// Structurally impossible unless firmware is misbehaving or libraries
    /// overlap in their drive-element address ranges.
    SerialAmbiguous { serial: String, libraries: Vec<String> },
}

/// Non-fatal per-device or per-library issues observed during the pass.
/// Returned alongside `Vec<Library>` in `DiscoveryReport`, never via
/// `Err`. Programmatic callers consume this list to gate operations.
pub enum DiscoveryWarning {
    /// Could not open or read a /dev/sg* that the host advertised.
    DeviceUnreachable { path: PathBuf, kind: IoErrorKind, message: String },

    /// A SCSI command on a specific device returned an error.
    ScsiError { path: PathBuf, command: &'static str, summary: String },

    /// DVCID+CurData didn't yield identifiers; topology was safely
    /// derivable so identity_source = Derived. Operations against
    /// affected drives require explicit opt-in.
    DriveMappingDerived { library: String, method: &'static str },

    /// DVCID failed AND the host:channel topology can't safely
    /// disambiguate (e.g., multiple logical libraries share the
    /// channel). Affected bays have `installed = None`.
    DriveMappingUnavailable { library: String },

    /// A tape device's serial didn't match any library's drive bay.
    UnclaimedTape { sg_path: PathBuf, serial: String },

    /// A library reported a drive bay with a known serial, but no
    /// /dev/sgN with that serial was reachable.
    UnresolvedDrive { library: String, serial: String, element_address: u16 },

    /// MODE SENSE 1Dh (if attempted) returned a layout that disagreed
    /// with what the RES page headers report. RES wins; this is for
    /// the operator's awareness.
    LayoutMismatch { library: String },

    /// A slot or IE port returned a voltag that wasn't trimmable ASCII.
    MalformedVoltag { library: String, element_address: u16 },
}
```

Best-effort principles:
- A failing single device does not abort the whole discovery — it produces a `DiscoveryWarning` and processing continues.
- Library-with-no-resolvable-drives is fine — we return the library with bays in `installed = None` state and the operator (or a higher-layer policy check) decides what to do.
- The boundary is precise: `DiscoveryError` is for "the whole pass is unusable" (no devices, wrong privileges, structural impossibility). Everything else is a warning.

---

## 5. Public API

The crate exposes one primary entry point and the value types from §3.

```rust
// remanence-library

pub use remanence_scsi as scsi;     // re-export for convenience

pub mod model;                       // Library, Drive, Slot, IePort, …

pub mod discovery;
pub use discovery::{discover, DiscoveryError, DiscoveryReport, DiscoveryWarning};

pub mod handle;
pub use handle::LibraryHandle;
```

### 5.1 `discover()` — the only entry point we publish in v0.1

```rust
pub fn discover() -> Result<DiscoveryReport, DiscoveryError>;

pub struct DiscoveryReport {
    /// Libraries successfully enumerated.
    pub libraries: Vec<Library>,
    /// Non-fatal issues observed during this discovery pass — e.g., a
    /// permission-denied on one /dev/sgN, a library whose drive serials
    /// had to use a fallback path, a tape device whose serial didn't
    /// match any library's drives. Programmatic callers can react
    /// (raise alerts, refuse to operate, etc.) without rereading log
    /// output.
    pub warnings: Vec<DiscoveryWarning>,
}

pub enum DiscoveryWarning {
    /// Could not open a /dev/sgN that lsscsi advertised.
    DeviceUnreadable { path: PathBuf, source: std::io::Error },
    /// A library's DVCID-RES path failed; fell back to per-drive VPD 0x80.
    DvcidFallbackUsed { library: String },
    /// A tape device's serial didn't match any library's drive list.
    UnclaimedTape { sg_path: PathBuf, serial: String },
    /// A library had configured drives that no /dev/sgN claimed.
    UnresolvedDrive { library: String, serial: String, element_address: u16 },
}
```

Read-only. Idempotent. No internal state. Caller manages refresh cadence. `Err(DiscoveryError)` is reserved for "discovery couldn't run meaningfully at all" — wrong privileges, no `/dev/sg*` whatsoever, ambiguous serials structurally impossible to resolve. Anything that's a per-device hiccup goes in `warnings` instead.

### 5.2 `LibraryHandle` — policy-gated, identity-revalidated handle for state-changing operations (Layer 2b)

A `LibraryHandle` is the only way state-changing CDBs reach a library. Acquiring one is **not** as simple as "open the cached `/dev/sgN`" — that path is stale at the moment discovery returns. Acquisition does three things, in order, and any failure surfaces a typed error rather than a `LibraryHandle`:

1. **Policy check** against an `AccessPolicy` the daemon's configuration owns (see §5.2.1). Refuses for any library serial not on the allowlist.
2. **Open** `library.changer_sg` with read/write access (or read-only when the caller wants a query-only handle).
3. **Re-identify**: issue standard INQUIRY + VPD 0x80 and verify the returned VPD 0x80 serial **exactly equals** `library.serial`. If it doesn't, the cached path now points at a different device — fail with `OpenError::IdentityChanged`.

The drive analog (used by future Layer 2b read/write operations) does the same dance with the bay's `installed.sg_path` and the recorded `installed.serial`.

```rust
impl DiscoveryReport {
    /// Borrow a Library by its serial. Read-only.
    pub fn library(&self, serial: &str) -> Option<&Library>;
}

impl Library {
    /// Acquire a handle for state-changing operations. Performs the
    /// policy check, opens the changer's /dev/sgN, and revalidates
    /// the library's identity via VPD 0x80 before returning.
    pub fn open(&self, policy: &dyn AccessPolicy) -> Result<LibraryHandle, OpenError>;
}

pub enum OpenError {
    /// Caller policy refused this library's serial.
    NotAllowed { serial: String },
    /// The cached /dev/sgN can't be opened.
    DeviceUnavailable { path: PathBuf, source: std::io::ErrorKind },
    /// The device at the cached path is not the library we discovered —
    /// kernel re-enumeration, hot-plug, or cable churn since discovery.
    IdentityChanged {
        path: PathBuf,
        expected: String,
        actual: Option<String>,   // None if the new device didn't respond to INQUIRY
    },
    /// Drive bays that depend on derived identity require explicit opt-in.
    /// (Issued by the analogous drive-handle path; included here for
    /// symmetry.)
    DerivedIdentityNotOptedIn { serial: String },
}

pub struct LibraryHandle {
    /// Snapshot at the time of open — re-validated before this value is
    /// constructed.
    pub library: Library,
    // Plus an internal File handle for the changer's /dev/sgN.
    // State-changing methods land in Layer 2b.
}
```

#### 5.2.1 `AccessPolicy` — daemon-owned allowlist as a hard requirement

Spec v0.2 §8.2 makes the library allowlist a defense-in-depth requirement, not a future soft rule. The allowlist is daemon-owned and surfaced into Layer 2 as a trait the caller has to provide:

```rust
pub trait AccessPolicy {
    /// Is this library allowed for state-changing operations?
    fn allows(&self, library_serial: &str) -> bool;

    /// Are operations against drives whose identity is `Derived` permitted
    /// for this library? Default false. Set true only on libraries for
    /// which the topology-derive convention has been operationally
    /// validated. (We deliberately do NOT have this enabled for the
    /// MSL3040 setup described in this doc.)
    fn allows_derived_drive_identity(&self, library_serial: &str) -> bool { false }
}

/// A convenience policy for `rem` subcommands and tests. Real daemon
/// deployments use a config-file-driven implementation.
pub struct StaticAllowlist {
    allowed: HashSet<String>,
    allowed_with_derived: HashSet<String>,
}
```

Discovery (a read-only operation) is **not** policy-gated — `discover()` always returns the full topology so the operator can see what's reachable. Only `Library::open(...)` and its drive equivalent are gated. This is what spec v0.2 §8.2's "Library allowlist" promises: **discovery surfaces everything, action requires opt-in.**

Why a handle type at all if Layer 2 is read-only today? Three reasons:
1. It bakes in both safety properties *now*: state-changing operations cannot be called on a plain `Library` value, and identity revalidation cannot be skipped. The dwara2 coexistence concern is enforced by the type system.
2. It pre-defines the surface area Layer 2b will hang operations from, so the API doesn't break when MOVE MEDIUM lands.
3. It surfaces `IdentityChanged` early — operators see "discovery is stale; re-run" rather than a write going to the wrong device.

### 5.3 What the CLI looks like (preview)

The `rem` binary (or its precursor as an example in this crate) will offer:

```
$ rem libraries                        # list everything discover() found
DEC418146K_LL02 — HPE MSL3040 D.00  /dev/sg7   (2 LTO-9 drives, 40 slots, 0 IE)
DEC418146K_LL03 — HPE MSL3040 D.00  /dev/sg11  (2 LTO-7 drives, 40 slots, 0 IE)

$ rem library DEC418146K_LL02          # focused view
Library DEC418146K_LL02
  Changer:  HPE MSL3040 D.00  /dev/sg7  (sysfs /sys/class/scsi_device/2:0:13:0)
  Chassis:  0x5001438031bdc7d4   (also shared with DEC418146K_LL03)
  Drives:
    [0x0001] HPE Ultrium 9-SCSI (HH90) /dev/sg4   serial 8031BDC7D1
    [0x0002] HPE Ultrium 9-SCSI (S2S1) /dev/sg2   serial 8031BDC7DB
  Slots:    40 (12 loaded, 28 empty)
  IE:       (none configured)

$ rem library DEC418146K_LL02 --slots
[0x03e9] full   CLNU01L9       (cleaning)
[0x03ea] full   S20001L9
[0x03eb] empty
…
```

`rem libraries` is the safe default that the operator can run on any host without risk. `rem library <serial>` requires the operator to spell the library out, which is the explicit opt-in. The `Chassis:` line on the focused view surfaces the shared-WWN information for operator awareness, without elevating chassis to a structural concept.

---

## 6. Safety & coexistence

The single hardest constraint on this layer: `dwara2` is in production on the LTO-7 logical library. Discovery must not interfere with its operation, even accidentally.

Hard rules enforced by code (all five are now non-negotiable):

1. **Discovery issues only read-only SCSI commands.** INQUIRY (with and without VPD), MODE SENSE, READ ELEMENT STATUS. No MODE SELECT, no INITIALIZE ELEMENT STATUS, no MOVE MEDIUM, no LOG SELECT. Asserted by the recorded-transport tests (§8 tier 2): the test harness rejects on first sight of a state-changing CDB opcode emitted during a discovery pass.
2. **No targeting by `/dev/sgN`.** Operators name libraries by serial; the runtime resolves the serial to a current `/dev/sgN`. A future operator who mistypes `sg11` instead of `sg7` cannot accidentally write to the LTO-7 library's changer.
3. **No "discover-and-act."** `discover()` returns data; the only way to do anything with state is through a `LibraryHandle` obtained via explicit `Library::open(policy)`. There is no helper that goes `find_library_with_n_empty_slots().move(…)`.
4. **Library allowlist (required, daemon-owned).** Per spec v0.2 §8.2: the daemon configuration carries an explicit list of library serials it is allowed to issue state-changing commands against. The `AccessPolicy` trait (§5.2.1) is the implementation. `Library::open(policy)` refuses any library not on the allowlist. There is no v0.1 "soft rule" stance — this requirement is in from the start.
5. **Identity revalidation at handle acquisition.** Every `Library::open(policy)` reissues INQUIRY VPD 0x80 against the cached `changer_sg` and refuses if the returned serial does not match. The same revalidation applies to drive handles before any read/write. This converts `/dev/sgN` from a trusted identity into a current attachment hint.

Soft rules (helpful, not load-bearing):
6. The CLI prints a banner when it sees a library not on its allowlist, identifying it explicitly. This makes the "not your library" state visible at the same moment the operator might be tempted to target it.
7. The CLI shows `IdentitySource` per drive bay in the focused-library view (§5.3) so the operator sees which drives are authoritative vs derived.

---

## 7. Implementation plan

In commit-sized chunks. Each ends with green tests.

### 7.1 (Prereq) VPD 0x83 parser in `remanence-scsi`

40-line parser, similar shape to `UnitSerial`. Surfaces a `DeviceIdentification` with a list of descriptors typed by designator type (NAA, T10 vendor, relative target port). Tests against the changer fixtures we already have.

### 7.2 The new crate skeleton

Add `crates/remanence-library/` to the workspace. `Cargo.toml`, `lib.rs`, a `model.rs` containing the value types from §3. No I/O yet. Compiles.

### 7.3 Pure parsing of a discovery snapshot

Implement `Library::from_captures(...)` taking already-parsed INQUIRY + VPD 0x80 + RES inputs and producing a `Library`. No `/dev/sg*` access. Tested entirely against the captured fixtures: feed the parsers' outputs through this and assert the resulting `Library` shape against expected values (drive count, drive serials, slot count, voltag of cleaning slot, etc.) for both QuadStor and the real MSL3040 (both logical libraries).

### 7.4 sysfs walker

`crate::sysfs` — enumerates `/dev/sg*`, resolves each to a sysfs path, returns a list. Linux-only behind `#[cfg(target_os = "linux")]`. Tested against a small mock of `/sys/class/scsi_generic/` written into a tempdir.

### 7.5 The orchestration glue

`crate::discovery` — composes §7.3 and §7.4 into the real `discover()` function. v0.1 of this layer **does not** issue MODE SENSE 1Dh; `ElementLayout` is derived from the RES page-header byte counts. The MODE SENSE cross-check is open question §10.2 and lands later. The first integration test runs `discover()` against akash's QuadStor and asserts: one library (`mainlib` from `setup.sh`), 4 drive bays each with `InstalledDrive` populated, 40 slots, 4 IE ports, drive serials match what the `topology` example already prints.

### 7.6 `LibraryHandle` + `AccessPolicy` (the safety scaffold, not yet state-changing)

Implements the full policy-check + identity-revalidation path from §5.2 even though there are no state-changing methods yet — the test harness in 7.5 already exercises:
- `OpenError::NotAllowed` when a policy refuses the library's serial.
- `OpenError::IdentityChanged` when a craft-test substitutes a different sg device after discovery.
- `OpenError::DerivedIdentityNotOptedIn` when a drive's `IdentitySource` is `Derived` and the policy hasn't opted into derived mappings.

### 7.7 `rem` CLI binary

Ship a real binary, not an example. Layout actually used:

- **New crate `crates/remanence-cli/`** with one binary, `rem`. Owned by Layer 2, depends only on `remanence-library`.
- **Subcommands** `libraries` (alias `libs`), `library <serial>` (alias `lib`), and `library <serial> --slots` per §5.3.
- **Read-only.** Discovery + formatting only. Layer 2b's mutating subcommands (`rem move`, `rem load`, `rem export`) hang off `Library::open(policy)` and will arrive with the allowlist plumbing in their own design doc.
- **`examples/topology.rs` stays.** It's a Layer 1 SCSI demonstration tool that doesn't even need Layer 2 to be useful — kept for low-level debugging. The original "move topology.rs's job into rem" intent is fulfilled at the user-visible level (operators run `rem`, not `topology`) without deleting the diagnostic tool.
- **Fixture mode deferred.** The originally-sketched `--from-fixture-tree=fixtures/real-hardware/...` flag is deferred: `remanence-library`'s recorded-transport tests already cover the same ground (the orchestration is driven through the same `discover_with` entry point the daemon will use). When/if an operator workflow needs to walk fixtures interactively from outside the test harness, the flag can be added then.

Smoke-test against akash's QuadStor closes out Layer 2a: `cargo run -p remanence-cli --bin rem -- libraries` should produce a single library with 4 drives and 40 slots, matching `scripts/quadstor/setup.sh`'s provision.

---

## 8. Testing strategy

Three tiers, in order of strictness and cost:

1. **Fixture-driven parser tests.** Every transformation from byte buffer to `Library` value is testable by feeding in `include_bytes!` fixtures. Captured corpus is rich: two QuadStor RES variants, two MSL3040 RES variants (with and without DVCID), VPD 0x80/0x83 for every device. New tests assert end-state `Library` values, not just byte parsing.

2. **Recorded-transport tests.** A fake `Transport` trait that, given a CDB, returns bytes from a directory of captures keyed by (device, command). Lets us run the full `discover()` orchestration in a pure-Rust unit test against the real-hardware fixtures. Bonus: same fake doubles as the assertion mechanism for *what commands were issued* (e.g., "discovery issued zero MOVE MEDIUM commands"). Builds the safety-rule check into the test harness.

3. **Live integration against QuadStor.** One test that runs `discover()` on akash and asserts the shape against what `scripts/quadstor/setup.sh` provisions (1 library `mainlib`, 4 drives, 40 slots, …). Only runs when the env var `REMANENCE_LIVE_QUADSTOR=1` is set, so CI on machines without a /dev/sg4 doesn't get false failures.

For the production MSL3040: no live integration testing from Remanence itself; the next datamover capture window can run `scripts/capture-msl3040.sh` and the captured fixtures get added to the recorded-transport test corpus.

---

## 9. Refresh model — and the Layer 2c udev watcher

Discovery (Layer 2a, this doc) is a one-shot function. Callers re-invoke it when they need fresh state — for example, after a `MOVE MEDIUM` operation, after a tape is inserted via the IE port, or on a periodic cadence (every 5 minutes for a UI dashboard).

The event-driven re-discovery that spec v0.2 §6.3 calls for is the responsibility of **Layer 2c**, a separate (but adjacent) document:

- `remanence-library::watch` (Layer 2c) — wraps udev's netlink socket, filters for `subsystem == "scsi_generic"` **and** `subsystem == "scsi_tape"` (per spec v0.2 §6.3, both are observed because hot-plug events for a drive can arrive on either subsystem first), and emits a stream of events (`DeviceAdded`, `DeviceRemoved`, `DeviceChanged`).
- The daemon subscribes to that stream and triggers a fresh `discover()` whenever something changes. The `DiscoveryReport` it returns becomes input to the daemon's state machine, never the source of truth itself.
- This also handles the case where the LTO-7 library disappears (because dwara2 power-cycled the chassis or unloaded a magazine) without affecting Remanence's state on the LTO-9 library — udev events scope per-device, and Remanence reacts only to those that touch its allowlisted libraries.

Keeping the watcher in a separate doc lets Layer 2a stay focused on the pure-function discovery primitive that the rest of the daemon composes around.

---

## 10. Open questions

1. **VPD 0x83 NAA — what's the WWN convention across vendors?** We've verified HPE/IEEE Company ID 0x1438. IBM and Quantum libraries may use different encodings; deferring until we have second-vendor fixtures.
2. **Should `discover()` ever attempt to MODE SENSE page 1Dh?** Required if we want the canonical Element Address Assignment. Currently deferred — page-header-derived layout is sufficient and avoids the extra command per library.
3. **What's the right behavior when a library exists but no `/dev/sgN` matched any of its drive serials?** Likely "return the library with empty `drives` and a per-library warning." Currently a known gap — implementation can come down on either side.
4. **`Library` cloning cost.** A library with 40 slots is ~3 KB. Cheap, but if Layer 4 (the catalog) ends up holding thousands of historical snapshots, we may want to intern voltags. Not a v0.1 problem.
5. **Drive-bay address ↔ physical bay number translation.** SCSI element addresses are vendor-defined; the MSL3040's drive-bay-1 = element 0x0001 by convention but not by spec. We surface only the element address. If the operator wants to know "physical drive bay 2," that's an OCP / front-panel concept we deliberately don't try to render.

---

## 11. Out of scope (revisited explicitly)

- **State-changing operations** (Layer 2b): `MOVE MEDIUM`, `INITIALIZE ELEMENT STATUS`, `EXCHANGE MEDIUM`, IE port mapping, drive load/unload. Will be designed in a sibling doc.
- **On-tape format / manifest read & write** (Layer 3).
- **Catalog persistence** (Layer 4).
- **HTTP API & event stream** (Layer 5).
- **Cross-host discovery, iSCSI, NDMP, FC topology.**
- **Auth, mTLS, access control.** Discovery runs with whatever permissions the caller has; the daemon will inherit those.
- **Operator UX for library allowlisting** (mentioned in §6 as a soft future).

These each get their own design doc when their layer's turn comes.

---

## Appendix A — Worked example (datamover, today)

Input: `discover()` on `datamover`, where `lsscsi -g` shows 4 tape drives and 2 medium changers (plus enclosure and HBA noise).

Output, in pseudo-JSON:

```json
{
  "libraries": [
    {
      "serial": "DEC418146K_LL02",
      "changer_sg": "/dev/sg7",
      "changer_sysfs": "/sys/class/scsi_device/2:0:13:0",
      "chassis_wwn": "0x5001438031bdc7d4",
      "layout": { "drive_start": 1, "drive_count": 2, "slot_start": 1001, "slot_count": 40, "ie_start": 0, "ie_count": 0 },
      "drives": [
        { "element_address": 1, "serial": "8031BDC7D1", "vendor": "HPE", "product": "Ultrium 9-SCSI", "revision": "R3G3", "sg_path": "/dev/sg4", "loaded_tape": "S30002L9", "source_slot": 1034 },
        { "element_address": 2, "serial": "8031BDC7DB", "vendor": "HPE", "product": "Ultrium 9-SCSI", "revision": "S2S1", "sg_path": "/dev/sg2", "loaded_tape": null, "source_slot": null }
      ],
      "slots": [ /* 40 entries */ ],
      "ie_ports": []
    },
    {
      "serial": "DEC418146K_LL03",
      "changer_sg": "/dev/sg11",
      "chassis_wwn": "0x5001438031bdc7d4",
      "drives": [
        { "element_address": 1, "serial": "8031BDC7E5", "vendor": "HP", "product": "Ultrium 7-SCSI", "revision": "S2T1", "sg_path": "/dev/sg8", "loaded_tape": null },
        { "element_address": 2, "serial": "8031BDC7EF", "vendor": "HP", "product": "Ultrium 7-SCSI", "revision": "Q387", "sg_path": "/dev/sg3", "loaded_tape": "E10046L7", "source_slot": 1033 }
      ]
      /* ... */
    }
  ],
  "warnings": []
}
```

The two libraries happen to share `chassis_wwn` — operator UX can surface this as "these live in the same box" but Remanence's logic treats them as completely independent. `_LL03` (which dwara2 owns) is **discovered and reported, but no operation against it is exposed by the CLI without an explicit `rem library DEC418146K_LL03 …` invocation**. Visibility is fine; action requires opt-in.

---

*End of design v0.1. Comments and corrections welcome — please annotate inline rather than rewriting.*
