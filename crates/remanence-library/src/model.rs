//! Value types — `Library`, `DriveBay`, `InstalledDrive`, etc.
//! See `docs/layer2-design.md` §3.
//!
//! Everything in this module is plain `Clone + PartialEq` data. No
//! interior mutability, no async, no lifetimes leaking. Snapshots are
//! returned by value and freely cloned through Layer 3+.

use std::collections::HashSet;
use std::path::PathBuf;

use remanence_scsi::{DeviceDesignator, Inquiry};

use crate::error::{DiscoveryWarning, LoadError};

/// One snapshot pass over the host: the libraries we successfully
/// enumerated, plus any non-fatal issues we noticed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryReport {
    /// Libraries enumerated, ordered by `serial` (lexicographic) for
    /// determinism.
    pub libraries: Vec<Library>,
    /// Non-fatal per-device or per-library issues observed.
    pub warnings: Vec<DiscoveryWarning>,
}

impl DiscoveryReport {
    /// Find a library by serial. Returns `None` if no library in this
    /// report has that serial — typically because the operator typo'd
    /// or because discovery happened to miss the library this pass.
    pub fn library(&self, serial: &str) -> Option<&Library> {
        self.libraries.iter().find(|l| l.serial == serial)
    }
}

/// One logical library — the operational unit of Remanence.
///
/// A "logical library" is one SCSI medium changer with its own drives,
/// slots, and import/export ports. On HPE MSL hardware this often
/// corresponds to a *partition* of a physical chassis; on simpler
/// libraries it's the whole box. From this crate's perspective the
/// distinction is irrelevant — see `docs/layer2-design.md` §3.2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Library {
    /// Stable, human-readable identity. Whatever the changer returns in
    /// its VPD 0x80 unit-serial field.
    pub serial: String,

    /// `/dev/sgN` path of the medium-changer device **observed at
    /// discovery time only.** Not durable across reboots, HBA rescans,
    /// or hot-plug. `Library::open()` must revalidate by INQUIRYing
    /// this path and comparing the returned VPD 0x80 against `serial`.
    pub changer_sg: PathBuf,

    /// Current sysfs attachment path observed at discovery time.
    /// Same caveat as `changer_sg` — kept for diagnostics, never
    /// trusted as identity.
    pub changer_sysfs: PathBuf,

    /// What the changer's standard INQUIRY reports.
    pub changer_inquiry: Inquiry,

    /// Optional chassis-level designator from VPD 0x83. Used for
    /// operator UX (highlight libraries that share a physical chassis);
    /// no operational logic depends on it.
    pub chassis_designator: Option<DeviceDesignator>,

    /// Element-address layout. v0.1 derives this from RES page headers;
    /// a MODE SENSE 1Dh cross-check is deferred (see design doc §10.2).
    pub layout: ElementLayout,

    /// Drive bays, ordered by element address. A bay is present whether
    /// or not a usable host attachment was found — see
    /// [`DriveBay::installed`].
    pub drive_bays: Vec<DriveBay>,

    /// Storage slots, ordered by element address.
    pub slots: Vec<Slot>,

    /// Import/export ports, ordered by element address. May be empty.
    pub ie_ports: Vec<IePort>,
}

/// Element-address ranges within a library.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElementLayout {
    /// Address of the medium transport (robot). Almost always 0.
    pub robot_address: u16,
    /// First Data Transfer Element (drive bay) address.
    pub drive_start: u16,
    /// Number of drive bays.
    pub drive_count: u16,
    /// First storage-slot address.
    pub slot_start: u16,
    /// Number of storage slots.
    pub slot_count: u16,
    /// First import/export port address.
    pub ie_start: u16,
    /// Number of import/export ports.
    pub ie_count: u16,
}

/// READ ELEMENT STATUS exception evidence for one changer element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElementException {
    /// Additional Sense Code from the element descriptor.
    pub asc: u8,
    /// Additional Sense Code Qualifier from the element descriptor.
    pub ascq: u8,
}

impl ElementException {
    fn from_res_element(el: &remanence_scsi::read_element_status::Element) -> Option<Self> {
        // Descriptor ASC/ASCQ is operator evidence only when the changer sets
        // EXCEPT; otherwise vendor/stale bytes stay at the low-level parser.
        el.except.then_some(Self {
            asc: el.asc,
            ascq: el.ascq,
        })
    }
}

/// One drive bay in a library. The bay is a structural slot in the
/// changer; the (replaceable) drive currently in the bay is described
/// by [`installed`](Self::installed) when known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveBay {
    /// SCSI element address for this bay.
    pub element_address: u16,
    /// True when the changer reports this element as accessible and
    /// exception-free in READ ELEMENT STATUS.
    pub accessible: bool,
    /// Optional READ ELEMENT STATUS exception ASC/ASCQ for this bay.
    pub exception: Option<ElementException>,
    /// Drive currently installed, with what we know about it. `None`
    /// if the changer reports the bay but no drive identity could be
    /// resolved (DVCID unavailable + no safe topology mapping, etc.).
    pub installed: Option<InstalledDrive>,
    /// True if the bay currently holds a cartridge (RES `FULL` flag).
    /// Distinguishes "empty bay" from "loaded bay with no readable
    /// barcode" — `loaded_tape.is_none()` alone does not. This is the
    /// canonical occupancy signal for state-changing operations.
    pub loaded: bool,
    /// Trimmed volume tag, when one was read. `None` when the bay is
    /// empty *or* when it's loaded with an un-barcoded cartridge.
    /// Consult `loaded` to disambiguate.
    pub loaded_tape: Option<String>,
    /// If the bay is loaded, the element address of the storage slot
    /// the cartridge came from (RES SVALID + source-address fields).
    /// `None` when SMC's SVALID bit was clear or when the cartridge
    /// arrived from a non-slot source.
    pub source_slot: Option<u16>,
}

/// The drive currently sitting in a [`DriveBay`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledDrive {
    /// Drive serial. Source of truth varies — see `identity_source`.
    pub serial: String,
    /// How we learned this serial. Operations against `Derived` drives
    /// require explicit policy opt-in (see [`AccessPolicy`]).
    pub identity_source: IdentitySource,
    /// Vendor (from drive INQUIRY when the matching `/dev/sgN` was reachable).
    pub vendor: Option<String>,
    /// Product (e.g., "Ultrium 9-SCSI").
    pub product: Option<String>,
    /// Firmware revision (4-char ASCII from INQUIRY).
    pub revision: Option<String>,
    /// `/dev/sgN` of the matching tape device, if found.
    pub sg_path: Option<PathBuf>,
    /// Sysfs attachment path observed at discovery time.
    pub sysfs_path: Option<PathBuf>,
}

/// Provenance of an [`InstalledDrive::serial`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentitySource {
    /// Returned inline by the changer in the RES DVCID block. Trustable.
    DvcidInline,
    /// DVCID inline AND independently matched against the drive's own
    /// VPD 0x80. The strongest signal we get from off-the-shelf SCSI.
    DvcidAndInquiry,
    /// Topology-derived (drives on same `host:channel` as the changer,
    /// sorted by SCSI ID, matched by ordinal to the RES drive-element
    /// addresses). Only safe on deployments where this vendor convention
    /// has been validated AND where no other logical library shares the
    /// channel. Discovery emits `DriveMappingDerived` warnings.
    Derived,
}

/// One storage slot in a library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    /// SCSI element address.
    pub element_address: u16,
    /// True when the changer reports this element as accessible and
    /// exception-free in READ ELEMENT STATUS.
    pub accessible: bool,
    /// Optional READ ELEMENT STATUS exception ASC/ASCQ for this slot.
    pub exception: Option<ElementException>,
    /// True if the slot has a cartridge present (RES `FULL` flag).
    /// Distinguishes "empty slot" from "full slot with no voltag" —
    /// `cartridge.is_none()` alone does not.
    pub full: bool,
    /// Trimmed volume tag, when one was read. `None` when the slot is
    /// empty *or* when it's full but the cartridge has no readable
    /// barcode (un-barcoded cartridge, scan in progress, malformed
    /// label). Consult `full` to disambiguate.
    pub cartridge: Option<String>,
}

/// One import/export port (mailslot).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IePort {
    /// SCSI element address.
    pub element_address: u16,
    /// True when the changer reports this element as accessible and
    /// exception-free in READ ELEMENT STATUS.
    pub accessible: bool,
    /// Optional READ ELEMENT STATUS exception ASC/ASCQ for this port.
    pub exception: Option<ElementException>,
    /// True if the IE port currently holds a cartridge (RES `FULL`).
    pub full: bool,
    /// Trimmed volume tag, when one was read. See [`Slot::cartridge`]
    /// for the empty-vs-no-tag ambiguity.
    pub cartridge: Option<String>,
    /// `INENAB` flag from the RES descriptor — whether the port
    /// currently accepts imports.
    pub import_enabled: bool,
    /// `EXENAB` flag from the RES descriptor — whether the port
    /// currently accepts exports.
    pub export_enabled: bool,
}

/// Pure plan for getting a requested cartridge into a drive bay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadPlan {
    /// The requested barcode is already in the named drive bay.
    AlreadyLoaded {
        /// SCSI element address of the occupied drive bay.
        bay: u16,
    },
    /// Move the cartridge from the named slot into the named free drive bay.
    Load {
        /// SCSI element address of the source storage slot.
        slot: u16,
        /// SCSI element address of the destination drive bay.
        bay: u16,
    },
}

/// Resolve the pure library inventory decision for loading `voltag`.
///
/// This performs no SCSI I/O. A usable free drive bay is one whose `loaded`
/// flag is false and whose installed drive has a reachable `/dev/sgN`;
/// `loaded_tape == None` is not enough because a bay may hold a barcode-less
/// cartridge, and a DVCID-only unresolved bay cannot be commanded.
pub fn resolve_load_target(lib: &Library, voltag: &str) -> Result<LoadPlan, LoadError> {
    if let Some(bay) = lib
        .drive_bays
        .iter()
        .find(|bay| bay.loaded && bay.loaded_tape.as_deref() == Some(voltag))
    {
        return Ok(LoadPlan::AlreadyLoaded {
            bay: bay.element_address,
        });
    }

    let slot = lib
        .slots
        .iter()
        .find(|slot| slot.cartridge.as_deref() == Some(voltag))
        .ok_or(LoadError::NotInLibrary)?;
    let bay = lib
        .drive_bays
        .iter()
        .find(|bay| {
            !bay.loaded
                && bay
                    .installed
                    .as_ref()
                    .and_then(|drive| drive.sg_path.as_ref())
                    .is_some()
        })
        .ok_or(LoadError::NoFreeDrive)?;

    Ok(LoadPlan::Load {
        slot: slot.element_address,
        bay: bay.element_address,
    })
}

// ====================================================================
//  AccessPolicy — daemon-owned allowlist (spec v0.2 §8.2)
// ====================================================================

/// Hook for the daemon's library allowlist. Spec v0.2 §8.2 makes the
/// allowlist a defense-in-depth requirement: state-changing operations
/// must refuse for any library serial not on the operator-configured
/// list. Real daemon deployments back this with a config-file-driven
/// implementation; tests and CLI helpers can use [`StaticAllowlist`].
pub trait AccessPolicy {
    /// Is this library allowed for state-changing operations? Discovery
    /// itself never consults this — only `Library::open()` does.
    fn allows(&self, library_serial: &str) -> bool;

    /// Are operations against drives whose identity is
    /// [`IdentitySource::Derived`] permitted in this library? Default
    /// `false`. Set `true` only for libraries on which the topology
    /// convention has been operationally validated.
    fn allows_derived_drive_identity(&self, _library_serial: &str) -> bool {
        false
    }
}

/// Simple in-memory `AccessPolicy` driven by two `HashSet`s of library
/// serials. Used by tests, the `rem` CLI, and as a sensible default for
/// small deployments.
#[derive(Debug, Clone, Default)]
pub struct StaticAllowlist {
    allowed: HashSet<String>,
    allowed_with_derived: HashSet<String>,
}

impl StaticAllowlist {
    /// Build a policy allowing operations on the given library serials.
    /// No library in this set permits derived-identity drives by
    /// default; use [`with_derived_allowed`](Self::with_derived_allowed)
    /// to opt specific libraries into that.
    pub fn new<I, S>(library_serials: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            allowed: library_serials.into_iter().map(Into::into).collect(),
            allowed_with_derived: HashSet::new(),
        }
    }

    /// Mark a library serial as permitting derived-identity drives.
    /// Must already be in the main allowlist; ignored otherwise.
    pub fn with_derived_allowed<S: Into<String>>(mut self, library_serial: S) -> Self {
        let s = library_serial.into();
        if self.allowed.contains(&s) {
            self.allowed_with_derived.insert(s);
        }
        self
    }
}

impl AccessPolicy for StaticAllowlist {
    fn allows(&self, library_serial: &str) -> bool {
        self.allowed.contains(library_serial)
    }

    fn allows_derived_drive_identity(&self, library_serial: &str) -> bool {
        self.allowed_with_derived.contains(library_serial)
    }
}

// ====================================================================
//  Library::from_captures — pure assembly from already-parsed inputs
// ====================================================================

/// Already-parsed SCSI captures for one library, ready to be turned
/// into a [`Library`] value. Separates "what came off the SCSI wire"
/// from "where in the host we got it" — `discover()` (later, §7.5)
/// builds this from live I/O; the tests build it from `include_bytes!`
/// fixtures.
#[derive(Debug, Clone)]
pub struct DeviceCaptures {
    /// What the changer's standard INQUIRY returned.
    pub changer_inquiry: Inquiry,
    /// VPD page 0x80 (unit serial) — trimmed string. Stored as `String`
    /// rather than `UnitSerial<'_>` because the live discovery path
    /// can't produce a `'static` borrow without leaking the response
    /// buffer; `from_captures` only ever needs the trimmed identifier.
    pub unit_serial: String,
    /// VPD page 0x83 (device identification) — supplies the optional
    /// chassis designator. `None` if the device didn't return one.
    pub device_id: Option<remanence_scsi::DeviceIdentification>,
    /// Fully-parsed READ ELEMENT STATUS response.
    pub element_status: remanence_scsi::ElementStatusData,
    /// `/dev/sgN` of the changer device.
    pub changer_sg: PathBuf,
    /// Sysfs attachment of the changer.
    pub changer_sysfs: PathBuf,
}

impl Library {
    /// Build a `Library` from already-parsed SCSI captures. Pure logic;
    /// performs no I/O. The orchestration in §7.5 of the design doc
    /// composes the live SCSI calls + the sysfs walker + this function.
    ///
    /// The drive bays returned by this function are RES-only: each
    /// carries the DVCID-derived serial in `installed.serial` when the
    /// RES response included one, but no `sg_path` or vendor/product
    /// fields. The §7.5 orchestrator fills those in after walking the
    /// host's tape devices.
    pub fn from_captures(captures: DeviceCaptures) -> Self {
        use remanence_scsi::ElementType;

        // Derive element layout from RES page-header observations.
        let mut layout = ElementLayout {
            robot_address: 0,
            drive_start: 0,
            drive_count: 0,
            slot_start: 0,
            slot_count: 0,
            ie_start: 0,
            ie_count: 0,
        };

        let mut drive_bays = Vec::new();
        let mut slots = Vec::new();
        let mut ie_ports = Vec::new();

        for el in &captures.element_status.elements {
            match el.element_type {
                ElementType::MediumTransport => {
                    layout.robot_address = el.address;
                }
                ElementType::DataTransfer => {
                    if layout.drive_count == 0 {
                        layout.drive_start = el.address;
                    }
                    layout.drive_count += 1;
                    drive_bays.push(DriveBay {
                        element_address: el.address,
                        accessible: el.access && !el.except,
                        exception: ElementException::from_res_element(el),
                        installed: el.drive_serial.as_ref().map(|s| InstalledDrive {
                            serial: s.clone(),
                            identity_source: IdentitySource::DvcidInline,
                            vendor: None,
                            product: None,
                            revision: None,
                            sg_path: None,
                            sysfs_path: None,
                        }),
                        loaded: el.full,
                        loaded_tape: el.primary_voltag.clone(),
                        source_slot: el.source_address,
                    });
                }
                ElementType::Storage => {
                    if layout.slot_count == 0 {
                        layout.slot_start = el.address;
                    }
                    layout.slot_count += 1;
                    slots.push(Slot {
                        element_address: el.address,
                        accessible: el.access && !el.except,
                        exception: ElementException::from_res_element(el),
                        full: el.full,
                        cartridge: el.primary_voltag.clone(),
                    });
                }
                ElementType::ImportExport => {
                    if layout.ie_count == 0 {
                        layout.ie_start = el.address;
                    }
                    layout.ie_count += 1;
                    ie_ports.push(IePort {
                        element_address: el.address,
                        accessible: el.access && !el.except,
                        exception: ElementException::from_res_element(el),
                        full: el.full,
                        cartridge: el.primary_voltag.clone(),
                        import_enabled: el.import_enabled,
                        export_enabled: el.export_enabled,
                    });
                }
                ElementType::Other(_) => {}
            }
        }

        // Pick the best chassis designator if the device returned one
        // and it's a recognised "addressed-LU or target-device" form.
        let chassis_designator = captures
            .device_id
            .as_ref()
            .and_then(|did| did.preferred_chassis().cloned());

        Self {
            serial: captures.unit_serial,
            changer_sg: captures.changer_sg,
            changer_sysfs: captures.changer_sysfs,
            changer_inquiry: captures.changer_inquiry,
            chassis_designator,
            layout,
            drive_bays,
            slots,
            ie_ports,
        }
    }
}

// -------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn test_inquiry() -> Inquiry {
        Inquiry {
            device_type: remanence_scsi::DeviceType::MediumChanger,
            peripheral_qualifier: 0,
            removable: true,
            version: 7,
            response_data_format: 2,
            additional_length: 31,
            vendor: *b"TEST    ",
            product: *b"LIBRARY         ",
            revision: *b"0001",
        }
    }

    fn test_library(drive_bays: Vec<DriveBay>, slots: Vec<Slot>) -> Library {
        Library {
            serial: "LIB-TEST".to_string(),
            changer_sg: PathBuf::from("/dev/sg-test"),
            changer_sysfs: PathBuf::from("/sys/test"),
            changer_inquiry: test_inquiry(),
            chassis_designator: None,
            layout: ElementLayout {
                robot_address: 0,
                drive_start: 0x100,
                drive_count: drive_bays.len() as u16,
                slot_start: 0x400,
                slot_count: slots.len() as u16,
                ie_start: 0,
                ie_count: 0,
            },
            drive_bays,
            slots,
            ie_ports: Vec::new(),
        }
    }

    fn drive_bay(element_address: u16, loaded: bool, loaded_tape: Option<&str>) -> DriveBay {
        DriveBay {
            element_address,
            accessible: true,
            exception: None,
            installed: Some(InstalledDrive {
                serial: format!("DRV-{element_address:04X}"),
                identity_source: IdentitySource::DvcidAndInquiry,
                vendor: Some("TEST".to_string()),
                product: Some("DRIVE".to_string()),
                revision: Some("0001".to_string()),
                sg_path: Some(PathBuf::from(format!("/dev/sg-{element_address:04X}"))),
                sysfs_path: Some(PathBuf::from(format!("/sys/test/{element_address:04X}"))),
            }),
            loaded,
            loaded_tape: loaded_tape.map(str::to_string),
            source_slot: None,
        }
    }

    fn unresolved_drive_bay(
        element_address: u16,
        loaded: bool,
        loaded_tape: Option<&str>,
    ) -> DriveBay {
        DriveBay {
            element_address,
            accessible: true,
            exception: None,
            installed: None,
            loaded,
            loaded_tape: loaded_tape.map(str::to_string),
            source_slot: None,
        }
    }

    fn dvcid_only_drive_bay(
        element_address: u16,
        loaded: bool,
        loaded_tape: Option<&str>,
    ) -> DriveBay {
        DriveBay {
            element_address,
            accessible: true,
            exception: None,
            installed: Some(InstalledDrive {
                serial: format!("DRV-{element_address:04X}"),
                identity_source: IdentitySource::DvcidInline,
                vendor: None,
                product: None,
                revision: None,
                sg_path: None,
                sysfs_path: None,
            }),
            loaded,
            loaded_tape: loaded_tape.map(str::to_string),
            source_slot: None,
        }
    }

    fn slot(element_address: u16, full: bool, cartridge: Option<&str>) -> Slot {
        Slot {
            element_address,
            accessible: true,
            exception: None,
            full,
            cartridge: cartridge.map(str::to_string),
        }
    }

    fn res_element(
        element_type: remanence_scsi::read_element_status::ElementType,
        address: u16,
        except: bool,
        asc: u8,
        ascq: u8,
    ) -> remanence_scsi::read_element_status::Element {
        remanence_scsi::read_element_status::Element {
            element_type,
            address,
            full: true,
            impexp: false,
            except,
            asc,
            ascq,
            access: true,
            export_enabled: matches!(
                element_type,
                remanence_scsi::read_element_status::ElementType::ImportExport
            ),
            import_enabled: matches!(
                element_type,
                remanence_scsi::read_element_status::ElementType::ImportExport
            ),
            source_address: None,
            primary_voltag: Some(format!("TAPE{address:04X}")),
            drive_serial: matches!(
                element_type,
                remanence_scsi::read_element_status::ElementType::DataTransfer
            )
            .then(|| format!("DRV{address:04X}")),
        }
    }

    fn captures_from_elements(
        elements: Vec<remanence_scsi::read_element_status::Element>,
    ) -> DeviceCaptures {
        DeviceCaptures {
            changer_inquiry: test_inquiry(),
            unit_serial: "LIB-TEST".to_string(),
            device_id: None,
            element_status: remanence_scsi::read_element_status::ElementStatusData {
                first_element_address: elements
                    .iter()
                    .map(|element| element.address)
                    .min()
                    .unwrap_or(0),
                num_elements: elements.len() as u16,
                elements,
            },
            changer_sg: PathBuf::from("/dev/sg-test"),
            changer_sysfs: PathBuf::from("/sys/test"),
        }
    }

    #[test]
    fn from_captures_retains_exception_evidence_for_each_element_class() {
        use remanence_scsi::read_element_status::ElementType;

        let lib = Library::from_captures(captures_from_elements(vec![
            res_element(ElementType::DataTransfer, 0x0001, true, 0x04, 0x01),
            res_element(ElementType::Storage, 0x03e9, true, 0x3b, 0x12),
            res_element(ElementType::ImportExport, 0x0010, true, 0x28, 0x00),
        ]));

        assert_eq!(
            lib.drive_bays[0].exception,
            Some(ElementException {
                asc: 0x04,
                ascq: 0x01
            })
        );
        assert!(!lib.drive_bays[0].accessible);
        assert_eq!(
            lib.slots[0].exception,
            Some(ElementException {
                asc: 0x3b,
                ascq: 0x12
            })
        );
        assert!(!lib.slots[0].accessible);
        assert_eq!(
            lib.ie_ports[0].exception,
            Some(ElementException {
                asc: 0x28,
                ascq: 0x00
            })
        );
        assert!(!lib.ie_ports[0].accessible);
    }

    #[test]
    fn from_captures_drops_descriptor_sense_when_except_is_clear() {
        use remanence_scsi::read_element_status::ElementType;

        let lib = Library::from_captures(captures_from_elements(vec![res_element(
            ElementType::Storage,
            0x03e9,
            false,
            0x3b,
            0x12,
        )]));

        assert_eq!(lib.slots[0].exception, None);
        assert!(lib.slots[0].accessible);
    }

    #[test]
    fn from_captures_retains_zero_zero_exception_when_except_is_set() {
        use remanence_scsi::read_element_status::ElementType;

        let lib = Library::from_captures(captures_from_elements(vec![res_element(
            ElementType::Storage,
            0x03e9,
            true,
            0x00,
            0x00,
        )]));

        assert_eq!(
            lib.slots[0].exception,
            Some(ElementException {
                asc: 0x00,
                ascq: 0x00
            })
        );
        assert!(!lib.slots[0].accessible);
    }

    #[test]
    fn resolve_load_target_returns_already_loaded_drive_bay() {
        let lib = test_library(
            vec![
                drive_bay(0x100, true, Some("RMN001L9")),
                drive_bay(0x101, false, None),
            ],
            vec![slot(0x400, true, Some("RMN002L9"))],
        );

        let plan = resolve_load_target(&lib, "RMN001L9").expect("already loaded");

        assert_eq!(plan, LoadPlan::AlreadyLoaded { bay: 0x100 });
    }

    #[test]
    fn resolve_load_target_loads_from_slot_to_free_bay() {
        let lib = test_library(
            vec![
                drive_bay(0x100, true, Some("RMN000L9")),
                drive_bay(0x101, false, None),
            ],
            vec![slot(0x400, true, Some("RMN002L9"))],
        );

        let plan = resolve_load_target(&lib, "RMN002L9").expect("load from slot");

        assert_eq!(
            plan,
            LoadPlan::Load {
                slot: 0x400,
                bay: 0x101
            }
        );
    }

    #[test]
    fn resolve_load_target_reports_not_in_library() {
        let lib = test_library(
            vec![drive_bay(0x100, false, None)],
            vec![slot(0x400, true, Some("RMN002L9"))],
        );

        let err = resolve_load_target(&lib, "RMN999L9").expect_err("missing tape");

        assert!(matches!(err, LoadError::NotInLibrary), "{err}");
    }

    #[test]
    fn resolve_load_target_treats_barcode_less_loaded_bay_as_not_free() {
        let lib = test_library(
            vec![
                drive_bay(0x100, true, None),
                drive_bay(0x101, true, Some("RMN000L9")),
            ],
            vec![slot(0x400, true, Some("RMN003L9"))],
        );

        let err = resolve_load_target(&lib, "RMN003L9").expect_err("no free drive");

        assert!(matches!(err, LoadError::NoFreeDrive), "{err}");
    }

    #[test]
    fn resolve_load_target_skips_uncommandable_free_bays() {
        let lib = test_library(
            vec![
                unresolved_drive_bay(0x100, false, None),
                dvcid_only_drive_bay(0x101, false, None),
                drive_bay(0x102, false, None),
            ],
            vec![slot(0x400, true, Some("RMN004L9"))],
        );

        let plan = resolve_load_target(&lib, "RMN004L9").expect("usable free bay");

        assert_eq!(
            plan,
            LoadPlan::Load {
                slot: 0x400,
                bay: 0x102
            }
        );
    }

    #[test]
    fn resolve_load_target_reports_no_free_drive_when_all_free_bays_are_uncommandable() {
        let lib = test_library(
            vec![
                unresolved_drive_bay(0x100, false, None),
                dvcid_only_drive_bay(0x101, false, None),
            ],
            vec![slot(0x400, true, Some("RMN005L9"))],
        );

        let err = resolve_load_target(&lib, "RMN005L9").expect_err("no usable free bay");

        assert!(matches!(err, LoadError::NoFreeDrive), "{err}");
    }

    #[test]
    fn resolve_load_target_ignores_stale_loaded_tape_when_bay_not_loaded() {
        let lib = test_library(
            vec![drive_bay(0x100, false, Some("RMN004L9"))],
            vec![slot(0x400, true, Some("RMN004L9"))],
        );

        let plan = resolve_load_target(&lib, "RMN004L9").expect("load from slot");

        assert_eq!(
            plan,
            LoadPlan::Load {
                slot: 0x400,
                bay: 0x100
            }
        );
    }

    /// Build a DeviceCaptures from the QuadStor DVCID fixture — only
    /// element-status is exercised here; INQUIRY/VPD are stubs since the
    /// from_captures logic only reads serial + designator.
    fn quadstor_dvcid_captures() -> DeviceCaptures {
        const RES: &[u8] =
            include_bytes!("../../../fixtures/element-status/quadstor-msl-g3-dvcid.bin");
        const INQ: &[u8] = include_bytes!("../../../fixtures/inquiry/changer-msl-g3.bin");
        const VPD80: &[u8] = include_bytes!("../../../fixtures/vpd-80/changer-msl-g3.bin");
        DeviceCaptures {
            changer_inquiry: remanence_scsi::Inquiry::parse(INQ).unwrap(),
            unit_serial: remanence_scsi::UnitSerial::parse(VPD80)
                .unwrap()
                .as_str()
                .to_string(),
            device_id: None, // QuadStor VPD 0x83 fixture not staged in tree
            element_status: remanence_scsi::read_element_status::parse(RES).unwrap(),
            changer_sg: PathBuf::from("/dev/sg4"),
            changer_sysfs: PathBuf::from("/sys/class/scsi_device/10:0:0:0"),
        }
    }

    #[test]
    fn library_from_quadstor_dvcid_fixture() {
        // The DVCID fixture is element_type=4 only — 4 data-transfer
        // elements, no slots/IE/robot. Useful to pin the DriveBay
        // mapping in isolation.
        let lib = Library::from_captures(quadstor_dvcid_captures());
        assert_eq!(lib.serial, "7CBAD9CF74");
        assert_eq!(lib.drive_bays.len(), 4);
        assert_eq!(lib.layout.drive_count, 4);
        assert_eq!(lib.layout.slot_count, 0);
        assert_eq!(lib.layout.ie_count, 0);

        // Each bay should carry an installed drive with a DvcidInline
        // serial; no sg_path/vendor yet (that's the §7.5 step).
        for bay in &lib.drive_bays {
            let installed = bay.installed.as_ref().expect("dvcid-inline");
            assert!(matches!(
                installed.identity_source,
                IdentitySource::DvcidInline
            ));
            assert!(installed.sg_path.is_none());
            assert!(installed.vendor.is_none());
            assert_eq!(installed.serial.len(), 10); // LTO drive serials are 10 chars
        }
    }

    #[test]
    fn real_msl3040_full_dvcid_round_trip() {
        const RES: &[u8] =
            include_bytes!("../../../fixtures/element-status/real-msl3040-full-dvcid.bin");
        const INQ: &[u8] = include_bytes!("../../../fixtures/inquiry/real/changer-msl3040.bin");
        const VPD80: &[u8] = include_bytes!("../../../fixtures/vpd-80/real/changer-msl3040.bin");
        const VPD83: &[u8] = include_bytes!(
            "../../../fixtures/real-hardware/remanence-fixtures-datamover-20260516T172906Z/inquiry/vpd-83/changer1.bin"
        );
        let captures = DeviceCaptures {
            changer_inquiry: remanence_scsi::Inquiry::parse(INQ).unwrap(),
            unit_serial: remanence_scsi::UnitSerial::parse(VPD80)
                .unwrap()
                .as_str()
                .to_string(),
            device_id: Some(remanence_scsi::DeviceIdentification::parse(VPD83).unwrap()),
            element_status: remanence_scsi::read_element_status::parse(RES).unwrap(),
            changer_sg: PathBuf::from("/dev/sg7"),
            changer_sysfs: PathBuf::from("/sys/class/scsi_device/2:0:13:0"),
        };
        let lib = Library::from_captures(captures);

        assert_eq!(lib.serial, "DEC418146K_LL02");
        // 43 elements: 1 robot + 2 drives + 40 slots
        assert_eq!(lib.drive_bays.len(), 2);
        assert_eq!(lib.slots.len(), 40);
        assert!(lib.ie_ports.is_empty());

        // Chassis designator surfaced as NAA.
        let chassis = lib.chassis_designator.as_ref().expect("chassis designator");
        assert_eq!(chassis.as_naa(), Some(0x5001_4380_31bd_c7d4));

        // First drive: full, with the recorded voltag and source slot.
        let d1 = lib
            .drive_bays
            .iter()
            .find(|b| b.element_address == 1)
            .unwrap();
        assert!(d1.installed.is_some());
        assert_eq!(d1.installed.as_ref().unwrap().serial, "8031BDC7D1");
        assert_eq!(d1.loaded_tape.as_deref(), Some("S30002L9"));
        assert_eq!(d1.source_slot, Some(0x040a));

        // First slot: storage element 1 with the cleaning cartridge.
        let s1 = lib
            .slots
            .iter()
            .find(|s| s.element_address == 0x03e9)
            .unwrap();
        assert_eq!(s1.cartridge.as_deref(), Some("CLNU01L9"));
        assert!(s1.full, "the slot is full");

        // Storage element 3 (0x03eb) — empty in this capture per mtx.
        let s3 = lib
            .slots
            .iter()
            .find(|s| s.element_address == 0x03eb)
            .unwrap();
        assert!(!s3.full, "the slot is empty");
        assert!(s3.cartridge.is_none());
    }

    #[test]
    fn static_allowlist_round_trips() {
        let p =
            StaticAllowlist::new(["DEC418146K_LL02", "WTF12345"]).with_derived_allowed("WTF12345");
        assert!(p.allows("DEC418146K_LL02"));
        assert!(p.allows("WTF12345"));
        assert!(!p.allows("DEC418146K_LL03")); // not on the list
        assert!(!p.allows_derived_drive_identity("DEC418146K_LL02"));
        assert!(p.allows_derived_drive_identity("WTF12345"));
    }
}
