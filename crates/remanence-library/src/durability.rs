//! Local-volume durability policy shared by authority stores.
//! Verifies filesystem placement and the underlying disk's write-cache/FUA policy.

use std::fs;
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Failure to establish the existing journal durability policy.
#[derive(Debug, thiserror::Error)]
pub enum VolumeError {
    /// Filesystem inspection failed.
    #[error("journal I/O: {0}")]
    Io(#[from] std::io::Error),
    /// Filesystem or device policy rejected this placement.
    #[error(
        "journal volume rejected: {0} (must be a trusted local volume with honored fsync; §10.6)"
    )]
    UntrustedVolume(String),
}

/// Require a local block-backed volume whose filesystem and write-cache
/// behavior are suitable for an fsync-based recovery authority.
///
/// Tape-log creation uses this shared policy before publishing durable authority.
pub fn validate_trusted_journal_volume(path: &Path) -> Result<(), VolumeError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let canonical_parent = parent
        .canonicalize()
        .unwrap_or_else(|_| parent.to_path_buf());
    let fstype = filesystem_type_for_path(&canonical_parent);
    validate_journal_fstype(&canonical_parent, fstype.as_deref())?;
    validate_journal_write_cache(&canonical_parent)
}

fn validate_journal_fstype(
    canonical_parent: &Path,
    fstype: Option<&str>,
) -> Result<(), VolumeError> {
    let fstype = fstype.ok_or_else(|| {
        VolumeError::UntrustedVolume(format!(
            "cannot determine filesystem type for {}",
            canonical_parent.display()
        ))
    })?;
    if is_untrusted_journal_fstype(fstype) {
        return Err(VolumeError::UntrustedVolume(format!(
            "{} is on {fstype}",
            canonical_parent.display()
        )));
    }
    Ok(())
}

fn is_untrusted_journal_fstype(fstype: &str) -> bool {
    fstype == "fuse"
        || fstype == "fuseblk"
        || fstype.starts_with("fuse.")
        || matches!(
            fstype,
            "tmpfs" | "ramfs" | "nfs" | "nfs4" | "cifs" | "smbfs" | "9p" | "overlay" | "overlayfs"
        )
}

#[cfg(target_os = "linux")]
fn validate_journal_write_cache(canonical_parent: &Path) -> Result<(), VolumeError> {
    let metadata = fs::metadata(canonical_parent)?;
    let dev = metadata.dev();
    let major = nix::sys::stat::major(dev);
    let minor = nix::sys::stat::minor(dev);
    validate_journal_write_cache_for_dev(
        canonical_parent,
        major,
        minor,
        Path::new("/sys/dev/block"),
    )
}

#[cfg(not(target_os = "linux"))]
fn validate_journal_write_cache(canonical_parent: &Path) -> Result<(), VolumeError> {
    Err(VolumeError::UntrustedVolume(format!(
        "cannot verify write-cache flush behavior for {} on this platform",
        canonical_parent.display()
    )))
}

#[cfg(target_os = "linux")]
fn validate_journal_write_cache_for_dev(
    canonical_parent: &Path,
    major: u64,
    minor: u64,
    sys_dev_block: &Path,
) -> Result<(), VolumeError> {
    const PLACEMENT_HINT: &str = "virtual or stacked filesystems such as btrfs may expose anonymous devices without queue/write_cache, so place journals on a trusted local block-backed volume or add explicit operator support";
    // sys_dev_block/<major>:<minor> is a symlink into the sysfs devices tree.
    // Resolve it so a partition can be recognised and walked up to its disk.
    let dev_link = sys_dev_block.join(format!("{major}:{minor}"));
    let device_dir = dev_link.canonicalize().map_err(|err| {
        VolumeError::UntrustedVolume(format!(
            "cannot resolve {} for {} (device {major}:{minor}; {PLACEMENT_HINT}): {err}",
            dev_link.display(),
            canonical_parent.display()
        ))
    })?;
    let device_name = sysfs_node_name(&device_dir);
    // A partition directory carries a `partition` file and has no `queue/`:
    // the request queue, and with it the write-cache mode the partition's
    // flushes reach, belongs to the whole disk one level up. Anything not
    // recognisably a partition must own its queue, so an unexpected layout
    // fails closed on the read below.
    let (queue_owner, consulted) = if device_dir.join("partition").is_file() {
        let disk_dir = device_dir.parent().ok_or_else(|| {
            VolumeError::UntrustedVolume(format!(
                "{} is on partition {} (device {major}:{minor}) with no parent disk in sysfs",
                canonical_parent.display(),
                device_dir.display()
            ))
        })?;
        let consulted = format!(
            "device {major}:{minor}, partition {device_name} of disk {}",
            sysfs_node_name(disk_dir)
        );
        (disk_dir.to_path_buf(), consulted)
    } else {
        let consulted = format!("device {major}:{minor}, {device_name}");
        (device_dir, consulted)
    };
    let queue = queue_owner.join("queue");
    let write_cache_path = queue.join("write_cache");
    let write_cache = fs::read_to_string(&write_cache_path)
        .map_err(|err| {
            VolumeError::UntrustedVolume(format!(
                "cannot read {} for {} ({consulted}; {PLACEMENT_HINT}): {err}",
                write_cache_path.display(),
                canonical_parent.display()
            ))
        })?
        .trim()
        .to_string();
    match write_cache.as_str() {
        "write through" => Ok(()),
        "write back" => {
            // Conservative v0.7.2 policy: require FUA for write-back journal
            // volumes even though some devices also honor fsync through flush.
            // Operators who hit this rejection need an explicit allowlist before
            // this commit point can be relaxed.
            let fua_path = queue.join("fua");
            let fua = fs::read_to_string(&fua_path)
                .map_err(|err| {
                    VolumeError::UntrustedVolume(format!(
                        "{} reports write back cache ({consulted}) but {} is unavailable: {err}",
                        canonical_parent.display(),
                        fua_path.display()
                    ))
                })?
                .trim()
                .to_string();
            if fua == "1" {
                Ok(())
            } else {
                Err(VolumeError::UntrustedVolume(format!(
                    "{} reports write back cache without FUA support ({consulted}; {}={fua})",
                    canonical_parent.display(),
                    fua_path.display()
                )))
            }
        }
        other => Err(VolumeError::UntrustedVolume(format!(
            "{} has unsupported write-cache mode {other:?} ({consulted}; {})",
            canonical_parent.display(),
            write_cache_path.display()
        ))),
    }
}

/// Last component of a sysfs device directory (`sda`, `sda1`, `nvme0n1p2`).
#[cfg(target_os = "linux")]
fn sysfs_node_name(dir: &Path) -> String {
    dir.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.display().to_string())
}

fn filesystem_type_for_path(path: &Path) -> Option<String> {
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").ok()?;
    let mut best: Option<(usize, String)> = None;
    for line in mountinfo.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let mut left_fields = left.split_whitespace();
        let Some(mount_point) = left_fields.nth(4) else {
            continue;
        };
        let Some(fstype) = right.split_whitespace().next().map(str::to_string) else {
            continue;
        };
        let mount_path = PathBuf::from(unescape_mountinfo_path(mount_point));
        if path.starts_with(&mount_path) {
            let len = mount_path.as_os_str().len();
            if best.as_ref().is_none_or(|(best_len, _)| len > *best_len) {
                best = Some((len, fstype));
            }
        }
    }
    best.map(|(_, fstype)| fstype)
}

fn unescape_mountinfo_path(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_policy_path(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("remanence-{name}-{stamp}"))
    }

    #[test]
    fn trusted_volume_policy_fails_closed_and_rejects_virtual_filesystems() {
        let path = Path::new("/journal-dir");
        let err = validate_journal_fstype(path, None)
            .expect_err("unknown filesystem type should fail closed");
        assert!(matches!(err, VolumeError::UntrustedVolume(_)));

        for fstype in [
            "tmpfs",
            "ramfs",
            "nfs",
            "nfs4",
            "cifs",
            "smbfs",
            "9p",
            "overlay",
            "overlayfs",
            "fuse",
            "fuseblk",
            "fuse.s3fs",
        ] {
            let err = validate_journal_fstype(path, Some(fstype))
                .expect_err("untrusted filesystem should reject");
            assert!(matches!(err, VolumeError::UntrustedVolume(_)));
        }

        validate_journal_fstype(path, Some("ext4")).expect("ordinary local fs passes");
        validate_journal_fstype(path, Some("xfs")).expect("ordinary local fs passes");
    }

    /// SCSI/SATA disk `sda` as the kernel places it under `/sys/devices`.
    #[cfg(target_os = "linux")]
    const MOCK_SDA: &str = "pci0000:00/0000:00:17.0/ata1/host0/target0:0:0/0:0:0:0/block/sda";
    /// NVMe namespace `nvme0n1` as the kernel places it under `/sys/devices`.
    #[cfg(target_os = "linux")]
    const MOCK_NVME0N1: &str = "pci0000:00/0000:00:03.1/0000:0a:00.0/nvme/nvme0/nvme0n1";
    /// eMMC user area `mmcblk0` as the kernel places it under `/sys/devices`.
    #[cfg(target_os = "linux")]
    const MOCK_MMCBLK0: &str = "platform/fe310000.mmc/mmc_host/mmc0/mmc0:0001/block/mmcblk0";

    /// A fake sysfs laid out the way the kernel lays it out:
    /// `dev/block/<major>:<minor>` is a relative symlink into `devices/`, a
    /// partition directory sits inside its disk's directory, carries a
    /// `partition` file and has no `queue/`, and only the whole disk owns
    /// `queue/write_cache` and `queue/fua`.
    #[cfg(target_os = "linux")]
    struct MockSysfs {
        root: PathBuf,
    }

    #[cfg(target_os = "linux")]
    impl MockSysfs {
        fn new(name: &str) -> Self {
            let root = temp_policy_path(name).with_extension("sysfs");
            fs::create_dir_all(root.join("dev").join("block")).expect("mock sysfs dev/block");
            Self { root }
        }

        fn sys_dev_block(&self) -> PathBuf {
            self.root.join("dev").join("block")
        }

        /// Create `devices/<device_path>` and link `dev/block/<dev>` to it.
        fn block_device(&self, dev: &str, device_path: &str) -> PathBuf {
            let dir = self.root.join("devices").join(device_path);
            fs::create_dir_all(&dir).expect("mock device dir");
            std::os::unix::fs::symlink(
                Path::new("../../devices").join(device_path),
                self.sys_dev_block().join(dev),
            )
            .expect("mock dev/block symlink");
            dir
        }

        /// A whole disk: a block device that owns an (initially empty) queue.
        fn disk(&self, dev: &str, device_path: &str) -> PathBuf {
            let dir = self.block_device(dev, device_path);
            fs::create_dir_all(dir.join("queue")).expect("mock disk queue dir");
            dir
        }

        /// A partition of the disk at `disk_path`: a `partition` file, no queue.
        fn partition(&self, dev: &str, disk_path: &str, name: &str, number: u32) -> PathBuf {
            let dir = self.block_device(dev, &format!("{disk_path}/{name}"));
            fs::write(dir.join("partition"), format!("{number}\n")).expect("mock partition file");
            dir
        }

        fn check(&self, major: u64, minor: u64) -> Result<(), VolumeError> {
            validate_journal_write_cache_for_dev(
                Path::new("/journal-dir"),
                major,
                minor,
                &self.sys_dev_block(),
            )
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for MockSysfs {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// Set a disk's `queue/write_cache`, and write or remove its `queue/fua`.
    #[cfg(target_os = "linux")]
    fn set_mock_queue(disk_dir: &Path, write_cache: &str, fua: Option<&str>) {
        let queue = disk_dir.join("queue");
        fs::create_dir_all(&queue).expect("mock queue dir");
        fs::write(queue.join("write_cache"), format!("{write_cache}\n"))
            .expect("write mock write_cache");
        match fua {
            Some(fua) => fs::write(queue.join("fua"), format!("{fua}\n")).expect("write mock fua"),
            None => {
                let _ = fs::remove_file(queue.join("fua"));
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn untrusted_message(result: Result<(), VolumeError>, why: &str) -> String {
        match result {
            Err(VolumeError::UntrustedVolume(message)) => message,
            other => panic!("{why}: expected UntrustedVolume, got {other:?}"),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn trusted_volume_policy_checks_write_cache_and_fua() {
        // 8:1 is sda1, a partition. As in real sysfs, sda1 has no queue/; the
        // write-cache mode it inherits is sda's.
        let sysfs = MockSysfs::new("mock-sysfs");
        let disk = sysfs.disk("8:0", MOCK_SDA);
        sysfs.partition("8:1", MOCK_SDA, "sda1", 1);

        let message = untrusted_message(
            sysfs.check(259, 0),
            "missing sysfs device is rejected with a useful hint",
        );
        assert!(message.contains("device 259:0"), "{message}");
        assert!(message.contains("btrfs"), "{message}");
        assert!(message.contains("trusted local block-backed"), "{message}");

        set_mock_queue(&disk, "write through", None);
        sysfs.check(8, 1).expect("write-through cache is trusted");

        set_mock_queue(&disk, "write back", None);
        untrusted_message(
            sysfs.check(8, 1),
            "write-back cache without FUA is rejected",
        );

        set_mock_queue(&disk, "write back", Some("0"));
        untrusted_message(sysfs.check(8, 1), "write-back cache with FUA=0 is rejected");

        set_mock_queue(&disk, "write back", Some("1"));
        sysfs
            .check(8, 1)
            .expect("write-back cache with FUA=1 is trusted");

        set_mock_queue(&disk, "mystery", Some("1"));
        untrusted_message(sysfs.check(8, 1), "unknown write-cache modes fail closed");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn trusted_volume_policy_reads_partition_cache_from_parent_disk() {
        let sysfs = MockSysfs::new("sysfs-partition-write-through");
        let disk = sysfs.disk("8:0", MOCK_SDA);
        let partition = sysfs.partition("8:1", MOCK_SDA, "sda1", 1);
        set_mock_queue(&disk, "write through", None);
        assert!(
            !partition.join("queue").exists(),
            "a partition has no queue/"
        );

        sysfs
            .check(8, 1)
            .expect("a partition of a write-through disk is trusted");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn trusted_volume_policy_partition_on_write_back_disk_requires_fua() {
        let sysfs = MockSysfs::new("sysfs-partition-write-back");
        let disk = sysfs.disk("8:0", MOCK_SDA);
        sysfs.partition("8:1", MOCK_SDA, "sda1", 1);

        set_mock_queue(&disk, "write back", Some("1"));
        sysfs
            .check(8, 1)
            .expect("a partition of a write-back disk with FUA is trusted");

        set_mock_queue(&disk, "write back", Some("0"));
        let message = untrusted_message(
            sysfs.check(8, 1),
            "a partition of a write-back disk without FUA is rejected",
        );
        assert!(message.contains("sda/queue/fua"), "{message}");
        assert!(message.contains("partition sda1 of disk sda"), "{message}");

        set_mock_queue(&disk, "write back", None);
        let message = untrusted_message(
            sysfs.check(8, 1),
            "a partition of a write-back disk with no fua file is rejected",
        );
        assert!(message.contains("sda/queue/fua"), "{message}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn trusted_volume_policy_whole_disk_reads_its_own_queue() {
        let sysfs = MockSysfs::new("sysfs-whole-disk");
        let disk = sysfs.disk("8:0", MOCK_SDA);
        sysfs.partition("8:1", MOCK_SDA, "sda1", 1);

        set_mock_queue(&disk, "write through", None);
        sysfs
            .check(8, 0)
            .expect("a write-through whole disk is trusted");

        set_mock_queue(&disk, "write back", Some("1"));
        sysfs
            .check(8, 0)
            .expect("a write-back whole disk with FUA is trusted");

        set_mock_queue(&disk, "write back", Some("0"));
        let message = untrusted_message(
            sysfs.check(8, 0),
            "a write-back whole disk without FUA is rejected",
        );
        assert!(message.contains("sda/queue/fua"), "{message}");
        assert!(!message.contains("partition"), "{message}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn trusted_volume_policy_resolves_nvme_partition_to_namespace() {
        let sysfs = MockSysfs::new("sysfs-nvme-partition");
        let namespace = sysfs.disk("259:0", MOCK_NVME0N1);
        sysfs.partition("259:1", MOCK_NVME0N1, "nvme0n1p1", 1);
        let partition = sysfs.partition("259:2", MOCK_NVME0N1, "nvme0n1p2", 2);
        assert!(
            !partition.join("queue").exists(),
            "a partition has no queue/"
        );

        set_mock_queue(&namespace, "write back", Some("1"));
        sysfs
            .check(259, 2)
            .expect("a partition of a write-back NVMe namespace with FUA is trusted");

        set_mock_queue(&namespace, "write back", Some("0"));
        let message = untrusted_message(
            sysfs.check(259, 2),
            "a partition of a write-back NVMe namespace without FUA is rejected",
        );
        assert!(message.contains("nvme0n1/queue/fua"), "{message}");
        assert!(
            message.contains("partition nvme0n1p2 of disk nvme0n1"),
            "{message}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn trusted_volume_policy_nested_device_with_own_queue_reads_its_own_queue() {
        // An eMMC boot area such as mmcblk0boot0 sits inside its disk's
        // directory like a partition, but it is a separate block device: it
        // owns a queue and carries no `partition` file.
        let sysfs = MockSysfs::new("sysfs-nested-device");
        let disk = sysfs.disk("179:0", MOCK_MMCBLK0);
        let boot = sysfs.disk("179:8", &format!("{MOCK_MMCBLK0}/mmcblk0boot0"));
        assert!(
            !boot.join("partition").exists(),
            "a nested device is not a partition"
        );

        set_mock_queue(&disk, "write through", None);
        set_mock_queue(&boot, "write back", Some("0"));
        let message = untrusted_message(
            sysfs.check(179, 8),
            "the nested device's own write-back queue without FUA is rejected",
        );
        assert!(message.contains("mmcblk0boot0/queue/fua"), "{message}");
        assert!(message.contains("device 179:8, mmcblk0boot0"), "{message}");
        assert!(!message.contains("partition"), "{message}");

        set_mock_queue(&disk, "write back", Some("0"));
        set_mock_queue(&boot, "write through", None);
        sysfs
            .check(179, 8)
            .expect("the nested device's own write-through queue is trusted");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn trusted_volume_policy_rejects_partition_whose_disk_has_no_queue() {
        let sysfs = MockSysfs::new("sysfs-partition-no-parent-queue");
        sysfs.block_device("8:0", MOCK_SDA);
        sysfs.partition("8:1", MOCK_SDA, "sda1", 1);

        let message = untrusted_message(
            sysfs.check(8, 1),
            "a partition whose disk exposes no queue fails closed",
        );
        assert!(message.contains("sda/queue/write_cache"), "{message}");
        assert!(message.contains("device 8:1"), "{message}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn trusted_volume_policy_rejects_anonymous_and_queueless_devices() {
        let sysfs = MockSysfs::new("sysfs-anonymous");
        let disk = sysfs.disk("8:0", MOCK_SDA);
        set_mock_queue(&disk, "write through", None);
        sysfs.block_device("252:9", "virtual/block/stacked0");

        // Anonymous devices (major 0: btrfs subvolumes, overlay, tmpfs) have no
        // /sys/dev/block entry at all.
        let message = untrusted_message(
            sysfs.check(0, 45),
            "an anonymous device has no sysfs entry and is rejected",
        );
        assert!(message.contains("device 0:45"), "{message}");
        assert!(message.contains("btrfs"), "{message}");

        let message = untrusted_message(
            sysfs.check(252, 9),
            "a virtual block device without a queue is rejected",
        );
        assert!(message.contains("device 252:9"), "{message}");
        assert!(message.contains("stacked0/queue/write_cache"), "{message}");
    }
}
