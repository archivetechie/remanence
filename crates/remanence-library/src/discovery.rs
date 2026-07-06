//! `discover()` — the read-only orchestration that walks `/dev/sg*`,
//! classifies each device, probes changers and tapes, and assembles a
//! [`DiscoveryReport`]. See `docs/layer2-design.md` §4 for the
//! algorithm; this is its implementation.

use std::path::{Path, PathBuf};

use remanence_scsi::{
    inquiry, read_element_status as res, vpd, DeviceIdentification, DeviceType, Inquiry, ScsiError,
    UnitSerial,
};

use crate::error::{DiscoveryError, DiscoveryWarning, IoErrorKind};
use crate::model::{DeviceCaptures, DiscoveryReport, IdentitySource, Library};
use crate::sysfs::DeviceAttachment;
use crate::transport::{SgTransport, TimeoutClass};

// CDB / buffer sizes for the read-only commands discovery issues.
// INQUIRY standard is at most 96 bytes; VPD pages can be larger.
const INQUIRY_BUF_LEN: usize = inquiry::ALLOC_LEN as usize;
const VPD_BUF_LEN: usize = vpd::ALLOC_LEN as usize;

/// Build a [`DiscoveryReport`] for the host, given an enumeration of
/// SG attachments and a way to open an [`SgTransport`] for each.
///
/// This is the testable form: `transport_for` is a closure that the
/// caller controls. Production code calls [`discover`] (Linux-only),
/// which passes the sysfs walker output and
/// [`LinuxSgTransport::open`](crate::transport::LinuxSgTransport::open).
/// Tests use [`FixtureTransport`](crate::transport::FixtureTransport)
/// to replay canned CDB responses without touching `/dev/sg*`.
pub fn discover_with<F, T>(
    devices: impl IntoIterator<Item = DeviceAttachment>,
    mut transport_for: F,
) -> Result<DiscoveryReport, DiscoveryError>
where
    F: FnMut(&Path) -> Result<T, IoErrorKind>,
    T: SgTransport,
{
    let devices: Vec<_> = devices.into_iter().collect();
    if devices.is_empty() {
        return Err(DiscoveryError::NoLibraries {
            warnings: Vec::new(),
        });
    }

    let mut warnings: Vec<DiscoveryWarning> = Vec::new();
    let mut libraries: Vec<Library> = Vec::new();
    // Tape devices we've successfully probed but not yet matched into
    // any library's drive bay.
    let mut tapes: Vec<TapeDevice> = Vec::new();

    for attach in devices {
        // -- open the transport ---------------------------------------
        let mut transport = match transport_for(&attach.sg_path) {
            Ok(t) => t,
            Err(io) => {
                warnings.push(DiscoveryWarning::DeviceUnreachable {
                    path: attach.sg_path.clone(),
                    source: io,
                });
                continue;
            }
        };

        // -- INQUIRY (standard) — classify ----------------------------
        let inq = match read_standard_inquiry(&mut transport) {
            Ok(i) => i,
            Err(e) => {
                warnings.push(scsi_warning(&attach.sg_path, "INQUIRY", e));
                continue;
            }
        };

        match inq.device_type {
            DeviceType::MediumChanger => {
                match probe_changer(&mut transport, &attach, inq, &mut warnings) {
                    Some(lib) => libraries.push(lib),
                    None => continue,
                }
            }
            DeviceType::SequentialAccess => {
                match probe_tape(&mut transport, attach, inq, &mut warnings) {
                    Some(tape) => tapes.push(tape),
                    None => continue,
                }
            }
            _ => { /* disk, enclosure, controller — silently skipped */ }
        }
    }

    // NoLibraries means "no medium-changer was successfully classified."
    // A host with only tape drives still counts as having no libraries —
    // Remanence requires a changer to be useful. Per the error model,
    // tapes alone are not enough. We hand the collected warnings to the
    // caller so the CLI can surface *why* every probe failed (e.g., a
    // string of `ScsiError ... EPERM` from a missing CAP_SYS_RAWIO).
    if libraries.is_empty() {
        return Err(DiscoveryError::NoLibraries { warnings });
    }

    // -- match tape devices into drive bays ---------------------------
    attach_tape_devices(&mut libraries, tapes, &mut warnings)?;

    // -- emit UnresolvedDrive warnings for bays without a /dev/sgN ----
    for lib in &libraries {
        for bay in &lib.drive_bays {
            if let Some(installed) = &bay.installed {
                if installed.sg_path.is_none() {
                    warnings.push(DiscoveryWarning::UnresolvedDrive {
                        library: lib.serial.clone(),
                        serial: installed.serial.clone(),
                        element_address: bay.element_address,
                    });
                }
            }
        }
    }

    // Deterministic ordering for human-readable output & snapshot tests.
    libraries.sort_by(|a, b| a.serial.cmp(&b.serial));

    Ok(DiscoveryReport {
        libraries,
        warnings,
    })
}

/// The standard Linux entry point — walk `/dev/sg*` via sysfs, open
/// each through [`LinuxSgTransport`](crate::transport::LinuxSgTransport),
/// and build a [`DiscoveryReport`]. This is the name
/// `docs/layer2-design.md` §4 refers to; tests use [`discover_with`]
/// directly with a
/// [`FixtureTransport`](crate::transport::FixtureTransport).
///
/// Linux-only. Other OSes need to use [`discover_with`] with their own
/// transport (none are implemented today — v0.1 is Linux-only by the
/// spec).
#[cfg(target_os = "linux")]
pub fn discover() -> Result<DiscoveryReport, DiscoveryError> {
    use crate::sysfs;
    use crate::transport::LinuxSgTransport;

    let devices = sysfs::enumerate_sg_devices()
        .map_err(|cause| DiscoveryError::EnumerationDenied { cause })?;
    discover_with(devices, |path| {
        LinuxSgTransport::open(path).map_err(|e| IoErrorKind::from(&e))
    })
}

// ===================================================================
//  Internals — single-device probes
// ===================================================================

struct TapeDevice {
    sg_path: PathBuf,
    sysfs_path: PathBuf,
    inquiry: Inquiry,
    serial: String,
}

fn read_standard_inquiry<T: SgTransport>(t: &mut T) -> Result<Inquiry, ScsiError> {
    let cdb = inquiry::build_cdb(inquiry::ALLOC_LEN);
    let mut buf = vec![0u8; INQUIRY_BUF_LEN];
    let n = t.execute_in(&cdb, &mut buf)?.bytes_transferred as usize;
    Inquiry::parse(&buf[..n])
}

fn read_vpd_80<T: SgTransport>(t: &mut T) -> Result<String, ScsiError> {
    let cdb = inquiry::build_cdb_vpd(vpd::PAGE_UNIT_SERIAL, vpd::ALLOC_LEN);
    let mut buf = vec![0u8; VPD_BUF_LEN];
    let n = t.execute_in(&cdb, &mut buf)?.bytes_transferred as usize;
    Ok(UnitSerial::parse(&buf[..n])?.as_str().to_string())
}

fn read_vpd_83<T: SgTransport>(t: &mut T) -> Result<DeviceIdentification, ScsiError> {
    let cdb = inquiry::build_cdb_vpd(vpd::PAGE_DEVICE_ID, vpd::ALLOC_LEN);
    let mut buf = vec![0u8; VPD_BUF_LEN];
    let n = t.execute_in(&cdb, &mut buf)?.bytes_transferred as usize;
    DeviceIdentification::parse(&buf[..n])
}

/// Probe a medium-changer device end-to-end and return a [`Library`].
/// Returns `None` if the device couldn't be probed cleanly (a warning
/// is pushed onto `warnings` describing what happened).
fn probe_changer<T: SgTransport>(
    t: &mut T,
    attach: &DeviceAttachment,
    inq: Inquiry,
    warnings: &mut Vec<DiscoveryWarning>,
) -> Option<Library> {
    let serial = match read_vpd_80(t) {
        Ok(s) => s,
        Err(e) => {
            warnings.push(scsi_warning(&attach.sg_path, "INQUIRY VPD 0x80", e));
            return None;
        }
    };

    // VPD 0x83 is informational — failure is a warning, not fatal.
    let device_id = match read_vpd_83(t) {
        Ok(did) => Some(did),
        Err(e) => {
            warnings.push(scsi_warning(&attach.sg_path, "INQUIRY VPD 0x83", e));
            None
        }
    };

    let elements = probe_element_status(t, &attach.sg_path, &serial, warnings)?;

    let captures = DeviceCaptures {
        changer_inquiry: inq,
        unit_serial: serial,
        device_id,
        element_status: elements,
        changer_sg: attach.sg_path.clone(),
        changer_sysfs: attach.sysfs_path.clone(),
    };
    Some(Library::from_captures(captures))
}

/// READ ELEMENT STATUS probe with the full DVCID fallback ladder from
/// `docs/layer2-design.md` §4.2.1.
///
/// **Primary call** (element_type=0, DVCID=1, CurData=1) uses the
/// two-phase allocation pattern: 8-byte header to learn `byte_count`,
/// then a sized second call. Falls back to a 1 MiB allocation if the
/// 8-byte probe is rejected. This is what works on HPE MSL3040
/// firmware 3350 and QuadStor.
///
/// **If the primary call leaves any DataTransfer bay without a drive
/// serial** — including the *partial* case where some bays have inline
/// identity and others don't — retry drives-only (element_type=4) with
/// CurData=1 then CurData=0. Each successful rung *gap-fills* primary:
/// for every bay where primary's DataTransfer element has no
/// drive_serial, we copy the serial from drives-only if it has one.
/// Slot, IE, transport, and already-resolved drive elements are left
/// untouched. The ladder stops as soon as every bay is resolved.
///
/// **If the ladder is exhausted with any bay still unresolved**, emit
/// [`DiscoveryWarning::DriveMappingUnavailable`] for the library. The
/// returned `ElementStatusData` keeps its (possibly partial)
/// DataTransfer page; `Library::from_captures` builds
/// `DriveBay { installed: None }` for the unresolved bays and the
/// daemon refuses state-changing operations on them. The warning fires
/// for *partial* failure as well as total — this is a regression-safe
/// design: a one-off firmware glitch dropping one drive's DVCID won't
/// silently shrink the operator's view of the library.
fn probe_element_status<T: SgTransport>(
    t: &mut T,
    path: &Path,
    library_serial: &str,
    warnings: &mut Vec<DiscoveryWarning>,
) -> Option<res::ElementStatusData> {
    // -- Primary call -------------------------------------------------
    let mut primary = match issue_res(t, 0, true, true) {
        Ok(data) => data,
        Err(e) => {
            warnings.push(scsi_warning(path, "READ ELEMENT STATUS", e));
            return None;
        }
    };

    if all_drives_resolved(&primary) {
        return Some(primary);
    }

    // -- Fallback ladder ----------------------------------------------
    // Drives-only, DVCID=1, with both CurData polarities. Ladder rung
    // failures are *expected* on most firmware and silently consumed;
    // each successful rung gap-fills `primary` in place. The loop
    // exits as soon as every DataTransfer bay has a serial.
    for (curdata, _label) in [
        (true, "drives-only DVCID+CurData=1"),
        (false, "drives-only DVCID+CurData=0"),
    ] {
        if let Ok(drives) = issue_res(t, 4, true, curdata) {
            fill_missing_drive_serials(&mut primary, &drives);
            if all_drives_resolved(&primary) {
                return Some(primary);
            }
        }
    }

    // -- Ladder exhausted with at least one bay unresolved ------------
    // `IdentitySource::Derived` is deliberately not produced here yet.
    // Topology-derived drive identities need an operator-validated bay
    // convention per logical library; until that rung exists, unresolved
    // bays remain uncommandable and surface as a warning.
    warnings.push(DiscoveryWarning::DriveMappingUnavailable {
        library: library_serial.to_string(),
    });
    Some(primary)
}

/// Issue a single READ ELEMENT STATUS using the two-phase allocation
/// pattern. Returns the parsed response or the first SCSI/parse error.
/// `pub(crate)` so `LibraryHandle::refresh` (Layer 2b §7.4) can reuse
/// the same probe pattern.
pub(crate) fn issue_res<T: SgTransport + ?Sized>(
    t: &mut T,
    element_type: u8,
    dvcid: bool,
    curdata: bool,
) -> Result<res::ElementStatusData, ScsiError> {
    // RES on a real library can take ~1 s per ~50 elements; on a
    // partitioned MSL3040 with hundreds of elements this would
    // blow past the 5 s default. Set the longer window once here
    // — applies to *both* the probe (learn_byte_count) and the
    // full read below, so any caller (discovery, Layer 2b
    // refresh / rescan post-INIT, identity revalidation) gets the
    // appropriate timeout without having to remember.
    t.set_timeout_for(TimeoutClass::ReadElementStatus);
    let alloc_len = learn_byte_count(t, element_type, dvcid, curdata)
        // Phase-1 rejected — single big call. 1 MiB comfortably covers
        // a 7-module 280-slot MSL3040 with DVCID (worst case ≈30 KiB).
        .unwrap_or(1024 * 1024);
    let cdb = res::build_cdb(
        element_type,
        /* starting_addr */ 0,
        /* num_elements  */ res::FULL_NUM_ELEMENTS,
        /* voltag        */ true,
        dvcid,
        curdata,
        alloc_len,
    );
    let mut buf = vec![0u8; alloc_len as usize];
    let n = t.execute_in(&cdb, &mut buf)?.bytes_transferred as usize;
    res::parse(&buf[..n])
}

/// Phase-1: ask for just the 8-byte header to learn `byte_count`.
/// `issue_res` has already set `TimeoutClass::ReadElementStatus`
/// on the transport, so this small probe inherits the long window
/// — fine, because the firmware-side cost is the scan, not the
/// allocation length we asked for.
fn learn_byte_count<T: SgTransport + ?Sized>(
    t: &mut T,
    element_type: u8,
    dvcid: bool,
    curdata: bool,
) -> Option<u32> {
    let cdb = res::build_cdb(
        element_type,
        0,
        res::FULL_NUM_ELEMENTS,
        true,
        dvcid,
        curdata,
        res::PROBE_ALLOC_LEN,
    );
    let mut buf = vec![0u8; res::PROBE_ALLOC_LEN as usize];
    let n = t.execute_in(&cdb, &mut buf).ok()?.bytes_transferred as usize;
    if n < 8 {
        return None;
    }
    let bc = ((buf[5] as u32) << 16) | ((buf[6] as u32) << 8) | (buf[7] as u32);
    // Exact header + reported payload length; never less than 64 because
    // some firmware reports byte_count=0 when the page has no elements,
    // but the response still needs room for the 8-byte header.
    //
    // Clamped to the CDB's 24-bit allocation-length ceiling: `bc` is a
    // device-supplied 24-bit value, so `bc + 8` can exceed what the CDB
    // field can express, and `res::build_cdb` asserts on oversize
    // allocation lengths. A hostile or buggy byte count must degrade to
    // a truncated parse (probe warning), never panic the process.
    Some((bc + 8).clamp(64, res::MAX_ALLOC_LEN))
}

/// True iff every DataTransfer element in `rsd` has an inline drive
/// serial. False if there are no DataTransfer elements at all — a
/// library with zero drives is degenerate and the ladder doesn't help.
fn all_drives_resolved(rsd: &res::ElementStatusData) -> bool {
    let mut saw_drive = false;
    for el in &rsd.elements {
        if matches!(el.element_type, res::ElementType::DataTransfer) {
            saw_drive = true;
            if el.drive_serial.is_none() {
                return false;
            }
        }
    }
    saw_drive
}

/// Gap-fill `primary`'s DataTransfer elements from `drives_only`:
/// where primary's bay has `drive_serial: None`, copy the serial from
/// the matching (by element_address) bay in `drives_only` if it has
/// one. Bays that already have a serial in primary are left untouched;
/// non-DataTransfer elements are left untouched.
fn fill_missing_drive_serials(
    primary: &mut res::ElementStatusData,
    drives_only: &res::ElementStatusData,
) {
    for el in primary.elements.iter_mut() {
        if !matches!(el.element_type, res::ElementType::DataTransfer) {
            continue;
        }
        if el.drive_serial.is_some() {
            continue;
        }
        if let Some(donor) = drives_only.elements.iter().find(|d| {
            matches!(d.element_type, res::ElementType::DataTransfer) && d.address == el.address
        }) {
            if donor.drive_serial.is_some() {
                el.drive_serial = donor.drive_serial.clone();
            }
        }
    }
}

fn probe_tape<T: SgTransport>(
    t: &mut T,
    attach: DeviceAttachment,
    inquiry: Inquiry,
    warnings: &mut Vec<DiscoveryWarning>,
) -> Option<TapeDevice> {
    let serial = match read_vpd_80(t) {
        Ok(s) => s,
        Err(e) => {
            warnings.push(scsi_warning(&attach.sg_path, "INQUIRY VPD 0x80", e));
            return None;
        }
    };
    Some(TapeDevice {
        sg_path: attach.sg_path,
        sysfs_path: attach.sysfs_path,
        inquiry,
        serial,
    })
}

/// Walk each tape device and fill in `InstalledDrive` fields on the
/// matching drive bay. Tape devices whose serial doesn't match any
/// bay become `UnclaimedTape` warnings. A serial matching more than
/// one bay becomes `DriveSerialAmbiguous`; affected bays remain
/// unbound rather than aborting the whole host pass.
fn attach_tape_devices(
    libraries: &mut [Library],
    tapes: Vec<TapeDevice>,
    warnings: &mut Vec<DiscoveryWarning>,
) -> Result<(), DiscoveryError> {
    for tape in tapes {
        let mut matches: Vec<(usize, usize)> = Vec::new();
        for (li, lib) in libraries.iter().enumerate() {
            for (bi, bay) in lib.drive_bays.iter().enumerate() {
                if let Some(inst) = &bay.installed {
                    if inst.serial == tape.serial {
                        matches.push((li, bi));
                    }
                }
            }
        }

        match matches.len() {
            0 => {
                warnings.push(DiscoveryWarning::UnclaimedTape {
                    sg_path: tape.sg_path,
                    serial: tape.serial,
                });
            }
            1 => {
                let (li, bi) = matches[0];
                let bay = &mut libraries[li].drive_bays[bi];
                if let Some(installed) = &mut bay.installed {
                    installed.sg_path = Some(tape.sg_path);
                    installed.sysfs_path = Some(tape.sysfs_path);
                    installed.vendor = Some(tape.inquiry.vendor_str().to_string());
                    installed.product = Some(tape.inquiry.product_str().to_string());
                    installed.revision = Some(tape.inquiry.revision_str().to_string());
                    // DvcidInline + corroborating VPD 0x80 → strongest signal.
                    installed.identity_source = IdentitySource::DvcidAndInquiry;
                }
            }
            _ => {
                let claimants = matches
                    .iter()
                    .map(|(li, bi)| {
                        format!(
                            "{}:0x{:04x}",
                            libraries[*li].serial, libraries[*li].drive_bays[*bi].element_address
                        )
                    })
                    .collect();
                warnings.push(DiscoveryWarning::DriveSerialAmbiguous {
                    sg_path: tape.sg_path,
                    serial: tape.serial,
                    claimants,
                });
            }
        }
    }
    Ok(())
}

fn scsi_warning(path: &Path, command: &'static str, e: ScsiError) -> DiscoveryWarning {
    DiscoveryWarning::ScsiError {
        path: path.to_path_buf(),
        command,
        summary: format!("{e}"),
    }
}

// =====================================================================
//  Tests — fixture-driven, no /dev/sg* required
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DriveBay, ElementLayout, InstalledDrive};
    use crate::transport::FixtureTransport;
    use std::collections::HashMap;

    // Build the full set of canned bytes a single QuadStor /dev/sg*
    // would return during discovery: standard INQUIRY, VPD 0x80,
    // VPD 0x83, then the two RES calls (8-byte probe, then full read).

    fn quadstor_changer_responses() -> Vec<Vec<u8>> {
        let std_inq: &[u8] = include_bytes!("../../../fixtures/inquiry/changer-msl-g3.bin");
        let vpd_80: &[u8] = include_bytes!("../../../fixtures/vpd-80/changer-msl-g3.bin");
        // We don't have an in-tree QuadStor VPD 0x83 fixture — for the
        // test it's enough that VPD 0x83 fails gracefully (the warning
        // path is exercised). Push a malformed page-mismatch response.
        let vpd_83_dummy = vec![0x08, 0x00, 0x00, 0x00];
        // RES with DVCID+CurData (full library, all element types).
        let res_full: &[u8] =
            include_bytes!("../../../fixtures/element-status/quadstor-msl-g3.bin");
        // Phase-1 probe sees just the 8-byte header.
        let res_header = res_full[..8].to_vec();
        vec![
            std_inq.to_vec(),
            vpd_80.to_vec(),
            vpd_83_dummy,
            res_header,
            res_full.to_vec(),
        ]
    }

    fn lto9_drive_responses(unit_serial: &[u8]) -> Vec<Vec<u8>> {
        let std_inq: &[u8] = include_bytes!("../../../fixtures/inquiry/drive1-lto9.bin");
        vec![std_inq.to_vec(), unit_serial.to_vec()]
    }

    fn attachment(sg: &str) -> DeviceAttachment {
        DeviceAttachment {
            sg_path: PathBuf::from(sg),
            sysfs_path: PathBuf::from(format!("/sys/class/scsi_device/mock{}", sg)),
        }
    }

    fn test_changer_inquiry() -> Inquiry {
        Inquiry::parse(include_bytes!(
            "../../../fixtures/inquiry/changer-msl-g3.bin"
        ))
        .unwrap()
    }

    fn test_drive_inquiry() -> Inquiry {
        Inquiry::parse(include_bytes!("../../../fixtures/inquiry/drive1-lto9.bin")).unwrap()
    }

    fn library_with_drive_serials(serial: &str, bays: &[(u16, &str)]) -> Library {
        Library {
            serial: serial.to_string(),
            changer_sg: PathBuf::from(format!("/dev/changer-{serial}")),
            changer_sysfs: PathBuf::from(format!("/sys/changer/{serial}")),
            changer_inquiry: test_changer_inquiry(),
            chassis_designator: None,
            layout: ElementLayout {
                robot_address: 0,
                drive_start: bays.first().map(|(addr, _)| *addr).unwrap_or(0),
                drive_count: bays.len() as u16,
                slot_start: 0,
                slot_count: 0,
                ie_start: 0,
                ie_count: 0,
            },
            drive_bays: bays
                .iter()
                .map(|(element_address, drive_serial)| DriveBay {
                    element_address: *element_address,
                    accessible: true,
                    exception: None,
                    installed: Some(InstalledDrive {
                        serial: (*drive_serial).to_string(),
                        identity_source: IdentitySource::DvcidInline,
                        vendor: None,
                        product: None,
                        revision: None,
                        sg_path: None,
                        sysfs_path: None,
                    }),
                    loaded: false,
                    loaded_tape: None,
                    source_slot: None,
                })
                .collect(),
            slots: Vec::new(),
            ie_ports: Vec::new(),
        }
    }

    /// Build a transport-factory closure that hands out scripted
    /// `FixtureTransport`s for the named paths.
    fn make_factory(
        scripts: HashMap<PathBuf, Vec<Vec<u8>>>,
    ) -> impl FnMut(&Path) -> Result<FixtureTransport, IoErrorKind> {
        let mut scripts = scripts;
        move |path: &Path| {
            scripts
                .remove(path)
                .map(|s| FixtureTransport::new().with_responses(s))
                .ok_or_else(|| IoErrorKind {
                    kind: "NotFound",
                    message: format!("no fixture transport for {path:?}"),
                    raw_os_error: None,
                })
        }
    }

    #[test]
    fn discover_empty_host_returns_no_libraries() {
        let r = discover_with(
            Vec::<DeviceAttachment>::new(),
            |_| -> Result<FixtureTransport, _> { unreachable!() },
        );
        assert!(matches!(r, Err(DiscoveryError::NoLibraries { .. })));
    }

    #[test]
    fn quadstor_no_dvcid_walks_ladder_and_emits_drive_mapping_unavailable() {
        // The in-tree QuadStor RES fixture was captured *without* the
        // CurData bit so it has no DVCID identifier descriptors. This
        // test exercises the negative path: full DVCID ladder
        // (drives-only retries) consumed, emits
        // DriveMappingUnavailable, and the four real drive devices
        // become UnclaimedTape because no bay can claim them.
        let drives = [
            (
                "/dev/sg0",
                &[
                    0x01u8, 0x80, 0x00, 0x0a, b'1', b'1', b'A', b'1', b'D', b'5', b'7', b'A', b'D',
                    b'0',
                ] as &[u8],
            ),
            (
                "/dev/sg1",
                &[
                    0x01, 0x80, 0x00, 0x0a, b'6', b'D', b'7', b'1', b'F', b'B', b'6', b'F', b'E',
                    b'6',
                ],
            ),
            (
                "/dev/sg2",
                &[
                    0x01, 0x80, 0x00, 0x0a, b'2', b'F', b'E', b'B', b'2', b'3', b'D', b'4', b'1',
                    b'A',
                ],
            ),
            (
                "/dev/sg3",
                &[
                    0x01, 0x80, 0x00, 0x0a, b'7', b'9', b'B', b'0', b'7', b'D', b'9', b'D', b'0',
                    b'0',
                ],
            ),
        ];

        let mut scripts = HashMap::new();
        scripts.insert(PathBuf::from("/dev/sg4"), quadstor_changer_responses());
        for (path, vpd80) in &drives {
            scripts.insert(PathBuf::from(*path), lto9_drive_responses(vpd80));
        }

        let attachments: Vec<_> = scripts
            .keys()
            .cloned()
            .map(|p| DeviceAttachment {
                sysfs_path: PathBuf::from(format!(
                    "/sys/class/scsi_device/mock{}",
                    p.file_name().unwrap().to_string_lossy()
                )),
                sg_path: p,
            })
            .collect();

        let report =
            discover_with(attachments, make_factory(scripts)).expect("discovery should succeed");

        assert_eq!(report.libraries.len(), 1);
        let lib = &report.libraries[0];
        assert_eq!(lib.serial, "7CBAD9CF74");
        assert_eq!(lib.drive_bays.len(), 4);

        for bay in &lib.drive_bays {
            assert!(
                bay.installed.is_none(),
                "bay {bay:?} has installed despite no DVCID"
            );
        }
        let unclaimed: usize = report
            .warnings
            .iter()
            .filter(|w| matches!(w, DiscoveryWarning::UnclaimedTape { .. }))
            .count();
        assert_eq!(
            unclaimed, 4,
            "all 4 tape devices unclaimed when RES is no-DVCID"
        );

        // The ladder exhausted → exactly one DriveMappingUnavailable
        // for this library.
        let mapping_unavailable: Vec<&DiscoveryWarning> = report
            .warnings
            .iter()
            .filter(|w| matches!(w, DiscoveryWarning::DriveMappingUnavailable { .. }))
            .collect();
        assert_eq!(mapping_unavailable.len(), 1);
        match mapping_unavailable[0] {
            DiscoveryWarning::DriveMappingUnavailable { library } => {
                assert_eq!(library, "7CBAD9CF74");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn discovers_real_msl3040_full_dvcid_with_drive_join_by_serial() {
        // The positive path: real MSL3040 full-DVCID RES capture has
        // both drive serials inline (8031BDC7D1 + 8031BDC7DB), so the
        // primary probe succeeds and the ladder is skipped. Two LTO-9
        // tape devices with matching VPD 0x80 serials are present on
        // the host, and discovery should bind them into the bays with
        // identity_source = DvcidAndInquiry.
        let changer_std_inq: &[u8] =
            include_bytes!("../../../fixtures/inquiry/real/changer-msl3040.bin");
        let changer_vpd_80: &[u8] =
            include_bytes!("../../../fixtures/vpd-80/real/changer-msl3040.bin");
        let changer_vpd_83:  &[u8] = include_bytes!("../../../fixtures/real-hardware/remanence-fixtures-datamover-20260516T172906Z/inquiry/vpd-83/changer1.bin");
        let res_full: &[u8] =
            include_bytes!("../../../fixtures/element-status/real-msl3040-full-dvcid.bin");
        let res_header = res_full[..8].to_vec();
        let changer_responses: Vec<Vec<u8>> = vec![
            changer_std_inq.to_vec(),
            changer_vpd_80.to_vec(),
            changer_vpd_83.to_vec(),
            res_header,
            res_full.to_vec(),
        ];

        let drive_a_vpd80 = &[
            0x01u8, 0x80, 0x00, 0x0a, b'8', b'0', b'3', b'1', b'B', b'D', b'C', b'7', b'D', b'1',
        ];
        let drive_b_vpd80 = &[
            0x01u8, 0x80, 0x00, 0x0a, b'8', b'0', b'3', b'1', b'B', b'D', b'C', b'7', b'D', b'B',
        ];

        let mut scripts = HashMap::new();
        scripts.insert(PathBuf::from("/dev/sg7"), changer_responses);
        scripts.insert(
            PathBuf::from("/dev/sg0"),
            lto9_drive_responses(drive_a_vpd80),
        );
        scripts.insert(
            PathBuf::from("/dev/sg1"),
            lto9_drive_responses(drive_b_vpd80),
        );

        let attachments: Vec<_> = scripts
            .keys()
            .cloned()
            .map(|p| DeviceAttachment {
                sysfs_path: PathBuf::from(format!(
                    "/sys/class/scsi_device/mock{}",
                    p.file_name().unwrap().to_string_lossy()
                )),
                sg_path: p,
            })
            .collect();

        let report =
            discover_with(attachments, make_factory(scripts)).expect("discovery should succeed");

        assert_eq!(report.libraries.len(), 1);
        let lib = &report.libraries[0];
        assert_eq!(lib.serial, "DEC418146K_LL02");
        assert_eq!(lib.drive_bays.len(), 2);
        assert_eq!(lib.slots.len(), 40);

        // Both bays bound to their tape device, upgraded to
        // DvcidAndInquiry, with the per-drive INQUIRY fields populated.
        let serials_expected: std::collections::HashSet<&str> =
            ["8031BDC7D1", "8031BDC7DB"].into_iter().collect();
        let mut bound_sg_paths: Vec<&Path> = Vec::new();
        for bay in &lib.drive_bays {
            let installed = bay.installed.as_ref().expect("bay should be installed");
            assert!(serials_expected.contains(installed.serial.as_str()));
            assert!(matches!(
                installed.identity_source,
                IdentitySource::DvcidAndInquiry
            ));
            let sg = installed
                .sg_path
                .as_deref()
                .expect("sg_path should be bound");
            bound_sg_paths.push(sg);
            assert!(installed.vendor.is_some());
            assert!(installed.product.is_some());
            assert!(installed.revision.is_some());
        }
        bound_sg_paths.sort();
        assert_eq!(
            bound_sg_paths,
            vec![Path::new("/dev/sg0"), Path::new("/dev/sg1")]
        );

        // No UnclaimedTape, no DriveMappingUnavailable when the primary
        // RES has serials inline.
        for w in &report.warnings {
            assert!(!matches!(w, DiscoveryWarning::UnclaimedTape { .. }));
            assert!(!matches!(
                w,
                DiscoveryWarning::DriveMappingUnavailable { .. }
            ));
            assert!(!matches!(w, DiscoveryWarning::UnresolvedDrive { .. }));
        }
    }

    #[test]
    fn no_libraries_when_host_has_only_tapes() {
        // A host with one tape device and no changer should be a
        // NoLibraries error, not Ok(libraries: []). Per the error
        // model, "no libraries" means none classifiable as a tape
        // library — a tape drive alone is not a library.
        let drive_vpd80 = &[
            0x01u8, 0x80, 0x00, 0x0a, b'8', b'0', b'3', b'1', b'B', b'D', b'C', b'7', b'D', b'1',
        ];
        let mut scripts = HashMap::new();
        scripts.insert(PathBuf::from("/dev/sg0"), lto9_drive_responses(drive_vpd80));
        let attachments = vec![attachment("/dev/sg0")];
        let r = discover_with(attachments, make_factory(scripts));
        assert!(matches!(r, Err(DiscoveryError::NoLibraries { .. })));
    }

    #[test]
    fn ambiguous_tape_serial_warns_and_keeps_other_drive_bindings() {
        let mut libraries = vec![
            library_with_drive_serials("LIB_A", &[(0x0100, "DUP"), (0x0101, "DUP")]),
            library_with_drive_serials("LIB_B", &[(0x0200, "OK")]),
        ];
        let tapes = vec![
            TapeDevice {
                sg_path: PathBuf::from("/dev/sg-dup"),
                sysfs_path: PathBuf::from("/sys/tape/dup"),
                inquiry: test_drive_inquiry(),
                serial: "DUP".to_string(),
            },
            TapeDevice {
                sg_path: PathBuf::from("/dev/sg-ok"),
                sysfs_path: PathBuf::from("/sys/tape/ok"),
                inquiry: test_drive_inquiry(),
                serial: "OK".to_string(),
            },
        ];
        let mut warnings = Vec::new();

        attach_tape_devices(&mut libraries, tapes, &mut warnings)
            .expect("ambiguous tape serial should be a warning, not fatal");

        assert!(libraries[0].drive_bays[0]
            .installed
            .as_ref()
            .unwrap()
            .sg_path
            .is_none());
        assert!(libraries[0].drive_bays[1]
            .installed
            .as_ref()
            .unwrap()
            .sg_path
            .is_none());
        assert_eq!(
            libraries[1].drive_bays[0]
                .installed
                .as_ref()
                .unwrap()
                .sg_path
                .as_deref(),
            Some(Path::new("/dev/sg-ok"))
        );

        assert_eq!(warnings.len(), 1);
        match &warnings[0] {
            DiscoveryWarning::DriveSerialAmbiguous {
                sg_path,
                serial,
                claimants,
            } => {
                assert_eq!(sg_path, Path::new("/dev/sg-dup"));
                assert_eq!(serial, "DUP");
                assert_eq!(
                    claimants,
                    &vec!["LIB_A:0x0100".to_string(), "LIB_A:0x0101".to_string()]
                );
            }
            other => panic!("unexpected warning: {other:?}"),
        }
    }

    #[test]
    fn hostile_res_byte_count_degrades_to_error_not_panic() {
        // The phase-1 probe header reports the device-chosen byte count
        // in a 24-bit field. A hostile/buggy maximum (0xFFFFFF) makes
        // `bc + 8` exceed the CDB allocation-length ceiling, which the
        // builder asserts on; learn_byte_count must clamp so the probe
        // degrades to an ordinary error, never a panic.
        let mut t = FixtureTransport::new();
        // 8-byte RES header: first element 0, count 0, reserved,
        // byte count = 0xFFFFFF. No further responses seeded: the
        // follow-up full read fails cleanly.
        t.push_response([0u8, 0, 0, 0, 0, 0xFF, 0xFF, 0xFF]);

        let result = issue_res(&mut t, 0, true, true);

        assert!(result.is_err(), "under-seeded full read must error");
        // The full-read CDB (second logged) carries the clamped
        // allocation length in bytes 7..10 — the 24-bit ceiling, not a
        // wrapped or oversize value.
        assert_eq!(t.cdb_log.len(), 2);
        assert_eq!(&t.cdb_log[1][7..10], &[0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn no_state_changing_cdbs_during_discovery() {
        // Capture every CDB that discovery issues against a single
        // QuadStor /dev/sg4 changer probe, and assert none of them are
        // state-changing opcodes (the spec v0.2 §8.2 / layer2-design
        // §6 safety requirement).
        use crate::transport::RecordingLog;

        let mut scripts = HashMap::new();
        scripts.insert(PathBuf::from("/dev/sg4"), quadstor_changer_responses());

        // Shared log across all (here: just one) wrapped transports.
        let log: RecordingLog = RecordingLog::new();
        let log_cl = log.clone();
        let mut inner_factory = make_factory(scripts);
        let factory = move |path: &Path| {
            let inner = inner_factory(path)?;
            Ok(crate::transport::RecordingTransport::with_log(
                inner,
                log_cl.clone(),
            ))
        };

        let _report =
            discover_with(vec![attachment("/dev/sg4")], factory).expect("discovery succeeds");

        // The opcodes discovery should be allowed to issue. Anything
        // else is a bug.
        const READ_ONLY: &[u8] = &[
            0x12, // INQUIRY (standard + VPD)
            0xb8, // READ ELEMENT STATUS
            0x1a, 0x5a, // MODE SENSE (6/10) — not used today, will be in future
            0x4d, // LOG SENSE — not used today
            0x00, // TEST UNIT READY — not used today
            0x03, // REQUEST SENSE — not used today
        ];
        for cdb in log.borrow().iter() {
            let opcode = cdb[0];
            assert!(
                READ_ONLY.contains(&opcode),
                "discovery issued non-read-only CDB opcode 0x{opcode:02x}: {cdb:02x?}"
            );
        }
    }

    // -- Partial DVCID gap-fill / resolution checks -------------------

    /// Build a minimal DataTransfer-only ElementStatusData with the
    /// given (address, serial) tuples. Caller controls which drives
    /// have an inline serial and which don't.
    fn synthetic_drive_status(drives: &[(u16, Option<&str>)]) -> res::ElementStatusData {
        use remanence_scsi::read_element_status::{Element, ElementStatusData, ElementType};
        let elements = drives
            .iter()
            .map(|(addr, serial)| Element {
                element_type: ElementType::DataTransfer,
                address: *addr,
                full: false,
                impexp: false,
                except: false,
                asc: 0,
                ascq: 0,
                access: true,
                export_enabled: false,
                import_enabled: false,
                source_address: None,
                primary_voltag: None,
                drive_serial: serial.map(|s| s.to_string()),
            })
            .collect::<Vec<_>>();
        let num = elements.len() as u16;
        ElementStatusData {
            first_element_address: drives.first().map(|(a, _)| *a).unwrap_or(0),
            num_elements: num,
            elements,
        }
    }

    #[test]
    fn all_drives_resolved_returns_true_only_when_every_bay_has_serial() {
        assert!(all_drives_resolved(&synthetic_drive_status(&[
            (1, Some("AAA")),
            (2, Some("BBB")),
        ])));
        assert!(!all_drives_resolved(&synthetic_drive_status(&[
            (1, Some("AAA")),
            (2, None),
        ])));
        // A library with zero drives is degenerate — the ladder
        // doesn't help, but the property "every drive has a serial"
        // is trivially true. We deliberately return *false* here so
        // discovery doesn't skip the ladder on a zero-drive primary
        // response (it shouldn't fire the ladder anyway in that case,
        // but defensive default).
        assert!(!all_drives_resolved(&synthetic_drive_status(&[])));
    }

    #[test]
    fn fill_missing_drive_serials_gapfills_only_unresolved_bays() {
        // Primary has bay 1 resolved, bay 2 unresolved. Drives-only
        // has both. After gap-fill, primary's bay 1 is unchanged and
        // bay 2 picks up the drives-only serial.
        let mut primary = synthetic_drive_status(&[(1, Some("ORIGINAL_1")), (2, None)]);
        let drives_only = synthetic_drive_status(&[
            (1, Some("OTHER_1")), // would clobber primary if we weren't careful
            (2, Some("FRESH_2")),
        ]);
        fill_missing_drive_serials(&mut primary, &drives_only);
        assert_eq!(
            primary.elements[0].drive_serial.as_deref(),
            Some("ORIGINAL_1")
        );
        assert_eq!(primary.elements[1].drive_serial.as_deref(), Some("FRESH_2"));
    }

    #[test]
    fn fill_missing_drive_serials_leaves_bay_unresolved_when_drives_only_also_lacks_it() {
        let mut primary = synthetic_drive_status(&[(1, None)]);
        let drives_only = synthetic_drive_status(&[(1, None)]);
        fill_missing_drive_serials(&mut primary, &drives_only);
        assert!(primary.elements[0].drive_serial.is_none());
        assert!(!all_drives_resolved(&primary));
    }
}
