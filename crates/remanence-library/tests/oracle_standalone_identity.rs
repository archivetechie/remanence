//! P1 oracles for C3 discovery and policy-gated standalone identity.
//! Hardware input is replayed from the HP Ultrium 7 capture; no device is opened.
//! Missing endpoint/policy APIs are intentional compile failures until P3.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use remanence_library::{
    discover_with, DeviceAttachment, DriveHandle, FixtureTransport, IdentitySource, MountEndpoint,
    RecordingTransport, StaticAllowlist,
};

const DRIVE: &str = "/dev/sg-oracle-lto7";
const INQUIRY: &[u8] = include_bytes!("../../../fixtures/standalone/drive-inquiry.bin");
const SERIAL: &[u8] = include_bytes!("../../../fixtures/standalone/drive-vpd80.bin");
const CHANGER: &[u8] = include_bytes!("../../../fixtures/standalone/changer-inquiry.bin");

fn attachment(path: &str) -> DeviceAttachment {
    DeviceAttachment {
        sg_path: path.into(),
        sysfs_path: PathBuf::from("/sys/devices/oracle").join(path.rsplit('/').next().unwrap()),
    }
}

// Discovery's factory and SgTransport are the production seams. This wrapper
// fails INQUIRY or READ ELEMENT STATUS explicitly, never by script exhaustion.
struct DiscoveryTransport {
    fixture: FixtureTransport,
    fail_inquiry: bool,
    fail_element_status: bool,
    element_status_failures: Arc<AtomicUsize>,
}
impl remanence_library::SgTransport for DiscoveryTransport {
    fn execute_in(
        &mut self,
        cdb: &[u8],
        buf: &mut [u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        if self.fail_inquiry || (self.fail_element_status && cdb[0] == 0xb8) {
            if self.fail_inquiry {
                assert_eq!(cdb[0], 0x12);
            } else {
                self.element_status_failures.fetch_add(1, Ordering::SeqCst);
            }
            return Err(remanence_library::ScsiError::TransportError {
                status: 0,
                host_status: 3,
                driver_status: 0,
                info: 1,
                sense: Vec::new(),
            });
        }
        self.fixture.execute_in(cdb, buf)
    }
    fn execute_none(&mut self, _: &[u8]) -> Result<(), remanence_library::ScsiError> {
        panic!("discovery must be read-only")
    }
    fn execute_out(
        &mut self,
        _: &[u8],
        _: &[u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        panic!("discovery must be read-only")
    }
}

#[test]
fn c3_discovery_drives_only_and_incomplete_evidence() {
    const SUSPECT: &str = "/dev/sg-oracle-changer";
    for failure in [
        "none",
        "vpd",
        "open",
        "inquiry",
        "element-status",
        "unresolved-bays",
        "unresolved-device",
    ] {
        let incomplete = failure != "none";
        let element_status_failures = Arc::new(AtomicUsize::new(0));
        let mut devices = vec![attachment(DRIVE)];
        if incomplete {
            devices.push(attachment(SUSPECT));
        }
        let report = discover_with(devices, |path| {
            if path == Path::new(SUSPECT) && failure == "open" {
                return Err(remanence_library::IoErrorKind {
                    kind: "PermissionDenied",
                    message: "oracle device open denied".into(),
                    raw_os_error: Some(13),
                });
            }
            let replies = if path == Path::new(DRIVE) {
                vec![INQUIRY.to_vec(), SERIAL.to_vec()]
            } else if failure == "element-status" {
                // INQUIRY and both VPD pages succeed before the explicit RES fault.
                vec![
                    include_bytes!("../../../fixtures/inquiry/changer-msl-g3.bin").to_vec(),
                    include_bytes!("../../../fixtures/vpd-80/changer-msl-g3.bin").to_vec(),
                    vec![8, 0x83, 0, 0],
                ]
            } else if failure.starts_with("unresolved-") {
                // Real captures cover missing DVCID and identified bays whose
                // devices are absent. Supply all RES ladder replies explicitly;
                // incomplete evidence must never depend on script exhaustion.
                let res: &[u8] = if failure == "unresolved-bays" {
                    include_bytes!("../../../fixtures/element-status/quadstor-msl-g3.bin")
                } else {
                    include_bytes!("../../../fixtures/element-status/quadstor-msl-g3-dvcid.bin")
                };
                let mut replies = vec![
                    include_bytes!("../../../fixtures/inquiry/changer-msl-g3.bin").to_vec(),
                    include_bytes!("../../../fixtures/vpd-80/changer-msl-g3.bin").to_vec(),
                    vec![8, 0x83, 0, 0],
                ];
                for _ in 0..3 {
                    replies.extend([res[..8].to_vec(), res.to_vec()]);
                }
                replies
            } else {
                vec![CHANGER.to_vec(), Vec::new(), Vec::new()]
            };
            Ok(DiscoveryTransport {
                fixture: FixtureTransport::new().with_responses(replies),
                fail_inquiry: path == Path::new(SUSPECT) && failure == "inquiry",
                fail_element_status: path == Path::new(SUSPECT) && failure == "element-status",
                element_status_failures: Arc::clone(&element_status_failures),
            })
        })
        .expect("C3: retain discovered drives even when another attachment fails");
        if failure == "element-status" {
            assert!(
                element_status_failures.load(Ordering::SeqCst) > 0,
                "must reach the RES fault"
            );
        }
        assert_eq!(report.complete, !incomplete, "{failure}");
        assert_eq!(report.unattached_drives.len(), 1, "{failure}");
        let drive = &report.unattached_drives[0];
        assert_eq!(drive.sg_path, Path::new(DRIVE));
        assert_eq!(drive.sysfs_path, attachment(DRIVE).sysfs_path);
        assert_eq!(drive.serial.as_deref(), Some("8031BDC7EF"));
        assert_eq!(drive.inquiry.vendor_str(), "HP");
        assert_eq!(drive.inquiry.product_str(), "Ultrium 7-SCSI");
        assert_eq!(
            format!("{:?}", drive.ownership),
            if incomplete { "Unknown" } else { "Standalone" },
            "{failure}"
        );
        assert_eq!(
            report.incomplete_evidence.is_empty(),
            !incomplete,
            "{failure}"
        );
        if failure.starts_with("unresolved-") {
            assert_eq!(report.libraries.len(), 1);
            assert!(report.libraries[0].drive_bays.iter().any(|b| {
                if failure == "unresolved-bays" {
                    b.installed.is_none()
                } else {
                    b.installed
                        .as_ref()
                        .is_some_and(|drive| drive.sg_path.is_none())
                }
            }));
            let evidence = format!("{:?}", report.incomplete_evidence);
            assert!(evidence.contains(&report.libraries[0].serial), "{evidence}");
            assert!(
                evidence.contains("DriveMappingUnavailable")
                    || evidence.contains("UnresolvedDrive"),
                "{evidence}"
            );
        } else {
            assert!(report.libraries.is_empty());
            if incomplete {
                let evidence = format!("{:?}", report.incomplete_evidence);
                assert!(evidence.contains(SUSPECT), "{failure}: {evidence}");
                let kind = match failure {
                    "open" => "DeviceUnreachable",
                    "inquiry" => "INQUIRY",
                    "vpd" => "0x80",
                    "element-status" => "READ ELEMENT STATUS",
                    _ => unreachable!(),
                };
                assert!(evidence.contains(kind), "{failure}: {evidence}");
            }
        }
    }
}

#[test]
fn c3_open_requires_serial_or_explicit_ephemeral_allowance() {
    let _production_open: fn(
        &Path,
        &StaticAllowlist,
    ) -> Result<DriveHandle, remanence_library::OpenError> = DriveHandle::open_standalone;
    let policy = StaticAllowlist::new([DRIVE]);
    let transport = || FixtureTransport::new().with_responses([INQUIRY.to_vec(), Vec::new()]);
    let (denied_transport, denied_log) = RecordingTransport::new(transport());
    let refusal = DriveHandle::open_standalone_with_transport(
        Path::new(DRIVE),
        Box::new(denied_transport),
        &policy,
    );
    let error = match refusal {
        Ok(_) => panic!("C3: missing VPD 0x80 must not become a durable path identity"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("serial") || error.contains("0x80"),
        "{error}"
    );
    assert!(denied_log.borrow().iter().all(|cdb| cdb[0] == 0x12));

    let policy = policy.with_unserialized_device_allowed(Path::new(DRIVE));
    assert!(policy.allows_unserialized_device(Path::new(DRIVE)));
    let drive = DriveHandle::open_standalone_with_transport(
        Path::new(DRIVE),
        Box::new(transport()),
        &policy,
    )
    .expect("C3: explicit read-only allowance admits an unserialized device");
    assert!(
        matches!(drive.endpoint(), MountEndpoint::Ephemeral { sg_path } if sg_path == Path::new(DRIVE))
    );
    assert_eq!(drive.drive().identity_source, IdentitySource::Standalone);
    assert_eq!(drive.drive().vendor.as_deref(), Some("HP"));
    assert_eq!(drive.drive().product.as_deref(), Some("Ultrium 7-SCSI"));
    assert_eq!(drive.drive().revision.as_deref(), Some("Q387"));
}
