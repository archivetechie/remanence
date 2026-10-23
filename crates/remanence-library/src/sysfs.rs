//! Linux sysfs walker — enumerate `/dev/sg*` and resolve each to its
//! current SCSI sysfs attachment path.
//!
//! The walker returns attachments and any enumeration failures, so discovery
//! cannot mistake a partial host inventory for proof of standalone ownership.
//! It skips nodes whose kernel peripheral type is neither tape nor changer;
//! discovery classifies the rest with INQUIRY. Linux-only;
//! the rest of `remanence-library` compiles fine without this module.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{DiscoveryWarning, IoErrorKind};

/// One `/dev/sgN` discovered on the host, paired with the current
/// sysfs attachment path it resolves to. Both are observed at the
/// moment of enumeration — they are not stable identities. The
/// discovery loop in `discover()` re-INQUIRYs through this `sg_path`
/// to capture the (stable) VPD 0x80 serial that becomes the catalog
/// key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceAttachment {
    /// e.g., `/dev/sg7`.
    pub sg_path: PathBuf,
    /// e.g., `/sys/class/scsi_device/2:0:13:0`.
    pub sysfs_path: PathBuf,
}

/// Host attachments together with evidence that enumeration was incomplete.
#[derive(Debug, Default)]
pub struct DeviceEnumeration {
    /// SG nodes whose current sysfs attachments were resolved.
    pub devices: Vec<DeviceAttachment>,
    /// Failures that must be retained when deciding standalone ownership.
    pub warnings: Vec<DiscoveryWarning>,
}

/// Default sysfs walk against the live system. Returns resolved SG attachments
/// and warnings for directory entries or attachments that could not be read.
#[cfg(target_os = "linux")]
pub fn enumerate_sg_devices() -> Result<DeviceEnumeration, IoErrorKind> {
    enumerate_sg_devices_under(Path::new("/dev"), Path::new("/sys/class/scsi_generic"))
}

/// Same as [`enumerate_sg_devices`] but with configurable roots. The
/// test suite passes tempdirs to exercise the walker without touching
/// the live `/dev` and `/sys`.
///
/// `dev_root` is searched for `sg*` entries; `sys_root` is expected to
/// contain `sgN/device` symlinks that resolve to per-device directories
/// like `…/scsi_device/H:C:I:L/…`.
pub fn enumerate_sg_devices_under(
    dev_root: &Path,
    sys_root: &Path,
) -> Result<DeviceEnumeration, IoErrorKind> {
    let entries = fs::read_dir(dev_root).map_err(|e| IoErrorKind::from(&e))?;
    Ok(resolve_sg_devices(
        dev_root,
        sys_root,
        entries.map(|entry| entry.map(|entry| entry.path())),
    ))
}

/// Keep iteration and attachment failures alongside the nodes we can resolve.
fn resolve_sg_devices(
    dev_root: &Path,
    sys_root: &Path,
    entries: impl IntoIterator<Item = Result<PathBuf, std::io::Error>>,
) -> DeviceEnumeration {
    let mut out = DeviceEnumeration::default();
    for entry in entries {
        let path = match entry {
            Ok(e) => e,
            Err(e) => {
                out.warnings.push(DiscoveryWarning::DeviceUnreachable {
                    path: dev_root.to_path_buf(),
                    source: IoErrorKind::from(&e),
                });
                continue;
            }
        };
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        // Match `sgN` only (not `sgN+1` style names; not `sg_changer`
        // symlinks; not the bare `sg` directory if it ever exists).
        if !is_sg_node(name) {
            continue;
        }

        // Resolve the sysfs attachment via /sys/class/scsi_generic/sgN/device.
        // `fs::canonicalize` follows the symlink AND cleans up `..` segments,
        // so the resulting path is the actual `/sys/devices/.../H:C:I:L`
        // directory rather than the readlink-relative path with `..` left in.
        let link = sys_root.join(name).join("device");
        let resolved = match fs::canonicalize(&link) {
            Ok(p) => p,
            Err(e) => {
                out.warnings.push(DiscoveryWarning::DeviceUnreachable {
                    path,
                    source: IoErrorKind::from(&e),
                });
                continue;
            }
        };

        // The kernel publishes each device's INQUIRY peripheral type here.
        // Only tape drives (1) and changers (8) can bear on drive ownership,
        // so other known types are reported without opening their nodes;
        // an unreadable or invalid type keeps the open-and-INQUIRY probe.
        if let Some(peripheral_type) = peripheral_type(&link) {
            if !matches!(peripheral_type, 1 | 8) {
                out.warnings.push(DiscoveryWarning::NotTapeOrChanger {
                    path,
                    peripheral_type,
                });
                continue;
            }
        }

        out.devices.push(DeviceAttachment {
            sg_path: path,
            sysfs_path: resolved,
        });
    }

    // Lexicographic sort by sg_path for deterministic output.
    out.devices.sort_by(|a, b| a.sg_path.cmp(&b.sg_path));
    out
}

/// Read the kernel's peripheral device type for an sg attachment.
fn peripheral_type(device_link: &Path) -> Option<u8> {
    fs::read_to_string(device_link.join("type"))
        .ok()
        .and_then(|value| value.trim().parse().ok())
}

/// Match exactly `sg` followed by one or more ASCII digits.
fn is_sg_node(name: &str) -> bool {
    name.strip_prefix("sg")
        .filter(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
        .is_some()
}

// -------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    /// Build a tempdir layout mimicking the host:
    ///   <tmp>/dev/sgN          — empty files (we never read them in walker)
    ///   <tmp>/sys/class/scsi_generic/sgN/device → ../../../devices/.../H:C:I:L
    fn build_mock_host(devices: &[(&str, &str)]) -> tempfile::TempDir {
        let tmp = tempfile::Builder::new()
            .prefix("sysfs-test")
            .tempdir()
            .unwrap();
        let dev = tmp.path().join("dev");
        let sys = tmp.path().join("sys/class/scsi_generic");
        fs::create_dir_all(&dev).unwrap();
        fs::create_dir_all(&sys).unwrap();

        for (sg, hcil) in devices {
            // /dev/sgN
            fs::write(dev.join(sg), b"").unwrap();
            // /sys/class/scsi_generic/sgN/device -> ../../../devices/<hcil>
            let sg_dir = sys.join(sg);
            fs::create_dir(&sg_dir).unwrap();
            let target_rel = format!("../../../devices/{}", hcil);
            // Also create the real target so canonicalization works
            let target_abs = tmp.path().join("sys").join(format!("devices/{}", hcil));
            fs::create_dir_all(&target_abs).unwrap();
            symlink(&target_rel, sg_dir.join("device")).unwrap();
        }
        tmp
    }

    #[test]
    fn enumerates_and_sorts_sg_devices() {
        let tmp = build_mock_host(&[
            ("sg11", "host2/2:0:13:0"),
            ("sg7", "host1/1:0:8:0"),
            ("sg3", "host2/2:0:11:0"),
        ]);
        let dev_root = tmp.path().join("dev");
        let sys_root = tmp.path().join("sys/class/scsi_generic");
        let out = enumerate_sg_devices_under(&dev_root, &sys_root).unwrap();

        // Lexicographic sort puts sg11 between sg1- and sg2-; that's fine
        // for determinism. (We don't promise numeric sort.)
        let names: Vec<_> = out
            .devices
            .iter()
            .map(|d| {
                d.sg_path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, vec!["sg11", "sg3", "sg7"]);

        // Each entry resolved its sysfs path.
        assert!(out.warnings.is_empty());
        for d in &out.devices {
            assert!(d.sysfs_path.to_string_lossy().contains("devices/"));
        }
    }

    #[test]
    fn ignores_non_sg_dev_entries() {
        let tmp = build_mock_host(&[("sg0", "host0/0:0:0:0")]);
        let dev_root = tmp.path().join("dev");
        let sys_root = tmp.path().join("sys/class/scsi_generic");

        // Add noise that the walker should skip silently.
        fs::write(dev_root.join("null"), b"").unwrap();
        fs::write(dev_root.join("sga"), b"").unwrap(); // not sg<digits>
        fs::write(dev_root.join("sg"), b"").unwrap(); // bare "sg"
        fs::write(dev_root.join("sg_some_link"), b"").unwrap(); // sg_* is not sg<digits>

        let out = enumerate_sg_devices_under(&dev_root, &sys_root).unwrap();
        assert_eq!(out.devices.len(), 1);
        assert!(out.devices[0].sg_path.ends_with("sg0"));
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn reports_devices_without_sysfs_link() {
        // /dev/sg0 exists but its /sys/class/scsi_generic/sg0/device
        // symlink doesn't. The failure must survive into discovery because
        // this unprobed node could be a changer that owns a visible drive.
        let tmp = tempfile::Builder::new()
            .prefix("sysfs-test")
            .tempdir()
            .unwrap();
        let dev = tmp.path().join("dev");
        let sys = tmp.path().join("sys/class/scsi_generic");
        fs::create_dir_all(&dev).unwrap();
        fs::create_dir_all(sys.join("sg0")).unwrap();
        fs::write(dev.join("sg0"), b"").unwrap();
        // intentionally no symlink at sys/.../sg0/device

        let out = enumerate_sg_devices_under(&dev, &sys).unwrap();
        assert!(out.devices.is_empty());
        assert!(matches!(&out.warnings[..],
            [DiscoveryWarning::DeviceUnreachable { path, .. }] if path == &dev.join("sg0")
        ));
    }

    #[test]
    fn enumerate_returns_error_for_missing_dev_root() {
        let r = enumerate_sg_devices_under(Path::new("/no/such/dir-zzzzz"), Path::new("/sys"));
        assert!(r.is_err());
    }

    /// A failed directory entry may hide a changer, even with a usable drive.
    #[test]
    fn directory_entry_error_keeps_discovery_incomplete() {
        let tmp = build_mock_host(&[("sg0", "host0/0:0:0:0")]);
        let dev = tmp.path().join("dev");
        let sys = tmp.path().join("sys/class/scsi_generic");
        let enumeration = resolve_sg_devices(
            &dev,
            &sys,
            [
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "directory entry unreadable",
                )),
                Ok(dev.join("sg0")),
            ],
        );
        let report = crate::discover_enumerated_with(enumeration, |_| {
            Ok(crate::FixtureTransport::new().with_responses([
                include_bytes!("../../../fixtures/inquiry/drive1-lto9.bin").to_vec(),
                include_bytes!("../../../fixtures/vpd-80/drive1-lto9.bin").to_vec(),
            ]))
        })
        .unwrap();
        assert!(!report.complete);
        assert_eq!(report.unattached_drives.len(), 1);
        assert_eq!(
            report.unattached_drives[0].ownership,
            crate::DriveOwnership::Unknown
        );
        assert!(matches!(&report.incomplete_evidence[..],
            [DiscoveryWarning::DeviceUnreachable { path, source }]
                if path == &dev && source.kind == "PermissionDenied"
        ));
    }
    /// Exercise the live enumeration funnel with a disk that cannot be opened.
    #[test]
    fn disk_is_skipped_before_open_and_drive_is_standalone() {
        for peripheral_type in [0, 5, 13, 31, 255] {
            let tmp = build_mock_host(&[("sg0", "disk"), ("sg1", "drive")]);
            let dev = tmp.path().join("dev");
            let sys = tmp.path().join("sys/class/scsi_generic");
            fs::write(sys.join("sg0/device/type"), peripheral_type.to_string()).unwrap();
            fs::write(sys.join("sg1/device/type"), "1\n").unwrap();
            let enumeration = enumerate_sg_devices_under(&dev, &sys).unwrap();
            assert_eq!(enumeration.devices.len(), 1, "disk must not be enumerated");
            let mut opened = Vec::new();
            let report = crate::discover_enumerated_with(enumeration, |path| {
                opened.push(path.to_path_buf());
                if path == dev.join("sg0") {
                    return Err(IoErrorKind::from(&std::io::Error::from(
                        std::io::ErrorKind::PermissionDenied,
                    )));
                }
                Ok(drive_transport())
            })
            .unwrap();
            assert_eq!(opened, [dev.join("sg1")]);
            assert!(report.complete);
            assert!(report.incomplete_evidence.is_empty());
            assert_eq!(report.unattached_drives.len(), 1);
            assert_eq!(
                report.unattached_drives[0].ownership,
                crate::DriveOwnership::Standalone
            );
            assert!(report.warnings.iter().any(|warning| matches!(warning,
                DiscoveryWarning::NotTapeOrChanger { path, peripheral_type: t }
                if path == &dev.join("sg0") && *t == peripheral_type)));
        }
    }

    /// Permission failures for tape and changer nodes remain ownership evidence.
    #[test]
    fn tape_and_changer_open_denied_keep_ownership_unknown() {
        for peripheral_type in [1, 8] {
            let tmp = build_mock_host(&[("sg0", "unreachable"), ("sg1", "drive")]);
            let dev = tmp.path().join("dev");
            let sys = tmp.path().join("sys/class/scsi_generic");
            fs::write(sys.join("sg0/device/type"), peripheral_type.to_string()).unwrap();
            fs::write(sys.join("sg1/device/type"), "1").unwrap();
            let enumeration = enumerate_sg_devices_under(&dev, &sys).unwrap();
            let mut opened = Vec::new();
            let report = crate::discover_enumerated_with(enumeration, |path| {
                opened.push(path.to_path_buf());
                if path == dev.join("sg0") {
                    Err(IoErrorKind::from(&std::io::Error::from(
                        std::io::ErrorKind::PermissionDenied,
                    )))
                } else {
                    Ok(drive_transport())
                }
            })
            .unwrap();
            assert_eq!(opened, [dev.join("sg0"), dev.join("sg1")]);
            assert!(!report.complete);
            assert_eq!(
                report.unattached_drives[0].ownership,
                crate::DriveOwnership::Unknown
            );
            assert!(matches!(&report.incomplete_evidence[..],
                [DiscoveryWarning::DeviceUnreachable { path, source }]
                if path == &dev.join("sg0") && source.kind == "PermissionDenied"));
        }
    }

    /// Missing, unreadable and invalid kernel types retain open-and-INQUIRY.
    #[test]
    fn unknown_peripheral_type_is_opened() {
        for value in [
            None,
            Some("invalid"),
            Some("256"),
            Some("-1"),
            Some("directory"),
        ] {
            let tmp = build_mock_host(&[("sg0", "drive")]);
            let dev = tmp.path().join("dev");
            let sys = tmp.path().join("sys/class/scsi_generic");
            match value {
                Some("directory") => fs::create_dir(sys.join("sg0/device/type")).unwrap(),
                Some(value) => fs::write(sys.join("sg0/device/type"), value).unwrap(),
                None => (),
            }
            let enumeration = enumerate_sg_devices_under(&dev, &sys).unwrap();
            assert_eq!(enumeration.devices.len(), 1);
            let mut opened = Vec::new();
            let report = crate::discover_enumerated_with(enumeration, |path| {
                opened.push(path.to_path_buf());
                Ok(drive_transport())
            })
            .unwrap();
            assert_eq!(opened, [dev.join("sg0")]);
            assert!(report.complete);
            assert_eq!(report.unattached_drives.len(), 1);
        }
    }

    /// Type 8 reaches the changer probe through the same resolver as discover().
    #[test]
    fn changer_type_is_probed() {
        let tmp = build_mock_host(&[("sg0", "changer")]);
        let dev = tmp.path().join("dev");
        let sys = tmp.path().join("sys/class/scsi_generic");
        fs::write(sys.join("sg0/device/type"), "8\n").unwrap();
        let enumeration = resolve_sg_devices(&dev, &sys, [Ok(dev.join("sg0"))]);
        assert_eq!(enumeration.devices.len(), 1);
        let res = include_bytes!("../../../fixtures/element-status/quadstor-msl-g3.bin");
        let mut opened = Vec::new();
        let report = crate::discover_enumerated_with(enumeration, |path| {
            opened.push(path.to_path_buf());
            Ok(crate::FixtureTransport::new().with_responses([
                include_bytes!("../../../fixtures/inquiry/changer-msl-g3.bin").to_vec(),
                include_bytes!("../../../fixtures/vpd-80/changer-msl-g3.bin").to_vec(),
                vec![0x08, 0, 0, 0],
                res[..8].to_vec(),
                res.to_vec(),
            ]))
        })
        .unwrap();
        assert_eq!(opened, [dev.join("sg0")]);
        assert_eq!(report.libraries.len(), 1);
        assert_eq!(report.libraries[0].changer_sg, dev.join("sg0"));
        assert!(!report
            .warnings
            .iter()
            .any(|w| matches!(w, DiscoveryWarning::NotTapeOrChanger { .. })));
    }

    /// Canned standard INQUIRY and serial responses for a standalone tape drive.
    fn drive_transport() -> crate::FixtureTransport {
        crate::FixtureTransport::new().with_responses([
            include_bytes!("../../../fixtures/inquiry/drive1-lto9.bin").to_vec(),
            include_bytes!("../../../fixtures/vpd-80/drive1-lto9.bin").to_vec(),
        ])
    }
}
