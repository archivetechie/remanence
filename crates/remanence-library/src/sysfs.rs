//! Linux sysfs walker — enumerate `/dev/sg*` and resolve each to its
//! current SCSI sysfs attachment path.
//!
//! The walker is intentionally minimal: it returns a flat
//! `Vec<DeviceAttachment>` and does not classify devices (that's
//! discovery's job, since classification requires INQUIRY). Linux-only;
//! the rest of `remanence-library` compiles fine without this module.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::IoErrorKind;

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

/// Default sysfs walk against the live system. Returns one entry per
/// existing `/dev/sg*` whose sysfs symlink could be resolved.
#[cfg(target_os = "linux")]
pub fn enumerate_sg_devices() -> Result<Vec<DeviceAttachment>, IoErrorKind> {
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
) -> Result<Vec<DeviceAttachment>, IoErrorKind> {
    let entries = fs::read_dir(dev_root).map_err(|e| IoErrorKind::from(&e))?;

    let mut out = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
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
            Err(_) => continue, // unreadable / not present; the device is unusable
        };

        out.push(DeviceAttachment {
            sg_path: path,
            sysfs_path: resolved,
        });
    }

    // Lexicographic sort by sg_path for deterministic output.
    out.sort_by(|a, b| a.sg_path.cmp(&b.sg_path));
    Ok(out)
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
        for d in &out {
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
        assert_eq!(out.len(), 1);
        assert!(out[0].sg_path.ends_with("sg0"));
    }

    #[test]
    fn skips_devices_without_sysfs_link() {
        // /dev/sg0 exists but its /sys/class/scsi_generic/sg0/device
        // symlink doesn't. The walker drops it (the device is unusable
        // — we couldn't even resolve where on the SCSI bus it sits).
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
        assert!(out.is_empty());
    }

    #[test]
    fn enumerate_returns_error_for_missing_dev_root() {
        let r = enumerate_sg_devices_under(Path::new("/no/such/dir-zzzzz"), Path::new("/sys"));
        assert!(r.is_err());
    }
}
