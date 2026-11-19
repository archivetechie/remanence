//! Local-volume durability policy shared by authority stores.
//! Verifies filesystem placement, write barriers and ZFS flush switches for fsync.

#[cfg(any(target_os = "linux", test))]
use std::fs;
use std::path::{Path, PathBuf};

/// Failure to establish the existing journal durability policy.
#[derive(Debug, thiserror::Error)]
pub enum VolumeError {
    /// Filesystem inspection failed.
    #[error("journal I/O: {0}")]
    Io(#[from] std::io::Error),
    /// Filesystem policy rejected this placement.
    #[error(
        "journal volume rejected: {0} (the journal needs a local ext3, ext4, XFS, btrfs, ZFS, f2fs or bcachefs filesystem with write barriers on)"
    )]
    UntrustedVolume(String),
}

/// A journal directory's reachable mount after the shared durability checks pass.
#[derive(Debug)]
pub struct JournalVolume {
    /// Supported filesystem type reported by mountinfo and checked against statfs.
    pub fstype: String,
    /// Mount point reached by pathname lookup.
    pub mount_point: PathBuf,
    /// Unescaped mount source (the dataset name on ZFS).
    pub source: String,
}

/// Require a local filesystem with write barriers suitable for an fsync-based
/// recovery authority.
///
/// Tape-log creation uses this shared policy before publishing durable authority.
pub fn validate_trusted_journal_volume(path: &Path) -> Result<(), VolumeError> {
    validate_trusted_journal_directory(path.parent().unwrap_or_else(|| Path::new(".")))
}

/// Require a journal directory to satisfy the shared filesystem durability policy.
pub fn validate_trusted_journal_directory(dir: &Path) -> Result<(), VolumeError> {
    inspect_journal_directory(dir).map(|_| ())
}

/// Inspect the reachable journal mount and require supported fsync durability.
/// This checks kernel-reported policy, not whether hardware honors its flushes.
pub fn inspect_journal_directory(dir: &Path) -> Result<JournalVolume, VolumeError> {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    #[cfg(target_os = "linux")]
    {
        let mountinfo = fs::read_to_string("/proc/self/mountinfo")
            .map_err(|_| unknown_journal_filesystem(&dir))?;
        inspect_journal_mountinfo(
            &dir,
            &mountinfo,
            |path| {
                nix::sys::statfs::statfs(path)
                    .map(|stat| stat.filesystem_type().0 as u64)
                    .map_err(std::io::Error::from)
            },
            Path::new("/sys/module/zfs/parameters"),
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(unknown_journal_filesystem(&dir))
    }
}

fn unknown_journal_filesystem(dir: &Path) -> VolumeError {
    VolumeError::UntrustedVolume(format!(
        "cannot determine filesystem type for {}",
        dir.display()
    ))
}

/// The only accepted filesystem names and their Linux statfs magics.
#[cfg(any(target_os = "linux", test))]
fn journal_filesystem_magic(fstype: &str) -> Option<u64> {
    match fstype {
        "ext3" | "ext4" => Some(0xEF53),
        "xfs" => Some(0x58465342),
        "btrfs" => Some(0x9123683E),
        "zfs" => Some(0x2FC12FC1),
        "f2fs" => Some(0xF2F52010),
        "bcachefs" => Some(0xCA451A4E),
        _ => None,
    }
}

/// Mount-tree identity and the fields needed by the journal policy.
#[cfg(any(target_os = "linux", test))]
struct JournalMount<'a> {
    id: u64,
    parent: u64,
    volume: JournalVolume,
    super_options: &'a str,
}

/// Preserve an empty mount source and the full super-options for journal checks.
#[cfg(any(target_os = "linux", test))]
fn parse_journal_mount(line: &str) -> Option<JournalMount<'_>> {
    let (left, right) = line.split_once(" - ")?;
    let mut left = left.split_whitespace();
    let id = left.next()?.parse().ok()?;
    let parent = left.next()?.parse().ok()?;
    let mount_point = PathBuf::from(unescape_mountinfo_path(left.nth(2)?));
    left.next()?; // The mandatory per-mount options must be present too.
    let (fstype, rest) = right.split_once(' ')?;
    let (source, super_options) = rest.split_once(' ')?;
    if fstype.is_empty() || super_options.is_empty() {
        return None;
    }
    let fstype = fstype.to_owned();
    let source = unescape_mountinfo_path(source);
    Some(JournalMount {
        id,
        parent,
        volume: JournalVolume {
            fstype,
            mount_point,
            source,
        },
        super_options,
    })
}

/// Follow only reachable children; a covering ancestor hides its older subtree.
#[cfg(any(target_os = "linux", test))]
fn reachable_journal_mount<'a>(
    dir: &Path,
    mounts: &'a [JournalMount<'_>],
) -> Option<&'a JournalMount<'a>> {
    let mut current = mounts.iter().rfind(|mount| {
        mount.volume.mount_point == Path::new("/")
            && (mount.parent == mount.id || !mounts.iter().any(|m| m.id == mount.parent))
    })?;
    if !dir.starts_with(&current.volume.mount_point) {
        return None;
    }
    // A valid mount tree cannot revisit a node. Bound the walk so malformed
    // cyclic input also fails closed instead of hanging.
    for _ in 0..mounts.len() {
        let child = mounts
            .iter()
            .rev()
            .filter(|mount| {
                mount.parent == current.id
                    && mount.id != current.id
                    && dir.starts_with(&mount.volume.mount_point)
            })
            .min_by_key(|mount| mount.volume.mount_point.components().count());
        match child {
            Some(child) => current = child,
            None => return Some(current),
        }
    }
    None
}

/// Host-independent funnel: callers supply mountinfo, statfs and ZFS parameters.
#[cfg(any(target_os = "linux", test))]
fn inspect_journal_mountinfo(
    dir: &Path,
    mountinfo: &str,
    statfs: impl Fn(&Path) -> std::io::Result<u64>,
    zfs_parameters: &Path,
) -> Result<JournalVolume, VolumeError> {
    let mounts: Vec<_> = mountinfo.lines().filter_map(parse_journal_mount).collect();
    let mount =
        reachable_journal_mount(dir, &mounts).ok_or_else(|| unknown_journal_filesystem(dir))?;
    let volume = &mount.volume;
    let magic = journal_filesystem_magic(&volume.fstype).ok_or_else(|| {
        VolumeError::UntrustedVolume(format!(
            "{} is on {}, which is not a supported journal filesystem",
            dir.display(),
            volume.fstype
        ))
    })?;
    let actual_magic = statfs(dir)?;
    if actual_magic != magic {
        return Err(VolumeError::UntrustedVolume(format!(
            "{}: mountinfo reports {} at {} but statfs reports magic 0x{actual_magic:X}",
            dir.display(),
            volume.fstype,
            volume.mount_point.display()
        )));
    }
    for token in mount.super_options.split(',') {
        if matches!(
            token,
            "nobarrier"
                | "barrier=0"
                | "fsync_mode=nobarrier"
                | "checkpoint=disable"
                | "journal_flush_disabled"
        ) || token.starts_with("checkpoint=disable:")
        {
            return Err(VolumeError::UntrustedVolume(format!(
                "{}: {} at {} disables journal durability with {token}",
                dir.display(),
                volume.fstype,
                volume.mount_point.display()
            )));
        }
    }
    if volume.fstype == "zfs" {
        for name in ["zfs_nocacheflush", "zil_nocacheflush"] {
            let path = zfs_parameters.join(name);
            let value = match fs::read_to_string(&path) {
                Ok(value) => value,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            let value = value.trim();
            if value != "0" {
                return Err(VolumeError::UntrustedVolume(format!(
                    "{}: {}={value} disables ZFS cache flushes",
                    dir.display(),
                    path.display()
                )));
            }
        }
    }
    Ok(JournalVolume {
        fstype: volume.fstype.clone(),
        mount_point: volume.mount_point.clone(),
        source: volume.source.clone(),
    })
}

#[cfg(any(target_os = "linux", test))]
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

    /// File and directory entry points apply the same policy to the log directory.
    #[test]
    fn trusted_journal_directory_matches_volume_parent() {
        let root = tempfile::tempdir().unwrap();
        for dir in [root.path().to_path_buf(), root.path().join("missing")] {
            let volume = validate_trusted_journal_volume(&dir.join("x"));
            let directory = validate_trusted_journal_directory(&dir);
            if dir != root.path() {
                let volume_error = volume.as_ref().expect_err("missing directory must fail");
                let directory_error = directory.as_ref().expect_err("missing directory must fail");
                assert_eq!(
                    std::mem::discriminant(volume_error),
                    std::mem::discriminant(directory_error)
                );
            }
            assert_eq!(
                volume.map_err(|e| e.to_string()),
                directory.map_err(|e| e.to_string())
            );
        }
        #[cfg(target_os = "linux")]
        {
            let volume = inspect_journal_directory(root.path()).unwrap();
            assert!(journal_filesystem_magic(&volume.fstype).is_some());
            assert!(!volume.mount_point.as_os_str().is_empty());
            assert!(!volume.source.is_empty());
        }
        #[cfg(not(target_os = "linux"))]
        assert!(inspect_journal_directory(root.path())
            .unwrap_err()
            .to_string()
            .contains("cannot determine filesystem type"));
    }

    fn mount(
        id: u64,
        parent: u64,
        point: &str,
        fstype: &str,
        source: &str,
        options: &str,
    ) -> String {
        format!("{id} {parent} 8:1 / {point} rw shared:1 - {fstype} {source} {options}\n")
    }

    fn root_mount() -> String {
        mount(1, 0, "/", "ext4", "/fixture/disk", "rw")
    }

    fn inspect(text: &str, path: &str, magic: u64) -> Result<JournalVolume, VolumeError> {
        let params = tempfile::tempdir().unwrap();
        inspect_journal_mountinfo(Path::new(path), text, |_| Ok(magic), params.path())
    }

    fn refusal(result: Result<JournalVolume, VolumeError>) -> String {
        match result {
            Err(VolumeError::UntrustedVolume(detail)) => detail,
            other => panic!("expected UntrustedVolume, got {other:?}"),
        }
    }

    #[test]
    fn trusted_volume_policy_accepts_only_supported_filesystems() {
        for (fstype, magic) in [
            ("ext3", 0xEF53),
            ("ext4", 0xEF53),
            ("xfs", 0x58465342),
            ("btrfs", 0x9123683E),
            ("zfs", 0x2FC12FC1),
            ("f2fs", 0xF2F52010),
            ("bcachefs", 0xCA451A4E),
        ] {
            let text = root_mount() + &mount(2, 1, "/journal", fstype, "pool/journal", "rw");
            let volume = inspect(&text, "/journal/log", magic).unwrap();
            assert_eq!(volume.fstype, fstype);
            assert_eq!(volume.mount_point, Path::new("/journal"));
            assert_eq!(volume.source, "pool/journal");
        }
    }

    #[test]
    fn journal_allow_list_refuses_unsupported_filesystems() {
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
            "ext2",
            "vfat",
            "fuse.sshfs",
            "virtiofs",
        ] {
            let text = mount(1, 0, "/", fstype, "fixture", "rw");
            let detail = refusal(inspect(&text, "/journal", 0x01021994));
            assert_eq!(
                detail,
                format!("/journal is on {fstype}, which is not a supported journal filesystem")
            );
        }
    }

    #[test]
    fn journal_mount_lookup_requires_covering_root_and_skips_malformed_lines() {
        for text in [
            String::new(),
            mount(2, 1, "/other", "ext4", "disk", "rw"),
            "bad mountinfo\n1 0 - ext4\n".into(),
        ] {
            assert_eq!(
                refusal(inspect(&text, "/journal", 0xEF53)),
                "cannot determine filesystem type for /journal"
            );
        }
        for parent in [0, 1] {
            let text = mount(1, parent, "/", "ext4", "disk", "rw")
                + "broken\n2 1 8:1 / /journal rw - tmpfs\n";
            assert_eq!(
                inspect(&text, "/journal", 0xEF53).unwrap().mount_point,
                Path::new("/")
            );
        }
        assert!(refusal(inspect(&root_mount(), "relative", 0xEF53))
            .contains("cannot determine filesystem type"));
    }

    /// All three covering cases must refuse for the reachable mount's own policy.
    #[test]
    fn journal_mount_lookup_rejects_covered_mounts() {
        let base = root_mount() + &mount(2, 1, "/mnt", "ext4", "lower", "rw");
        let cases = [
            (
                "tie",
                base.clone() + &mount(3, 2, "/mnt", "tmpfs", "upper", "rw"),
                0x01021994,
                "is on tmpfs, which is not a supported journal filesystem",
            ),
            (
                "covered nested",
                base + &mount(3, 2, "/mnt/sub", "ext4", "hidden", "rw")
                    + &mount(4, 2, "/mnt", "ext4", "upper", "rw,nobarrier"),
                0xEF53,
                "nobarrier",
            ),
            (
                "covered ancestor",
                root_mount()
                    + &mount(2, 1, "/mnt/sub", "ext4", "hidden", "rw")
                    + &mount(3, 1, "/mnt", "ext4", "cover", "rw,nobarrier"),
                0xEF53,
                "nobarrier",
            ),
        ];
        let mut failures = Vec::new();
        for (name, text, magic, expected) in cases {
            let result = inspect(&text, "/mnt/sub/log", magic);
            if !matches!(&result, Err(VolumeError::UntrustedVolume(detail)) if detail.contains(expected))
            {
                failures.push(format!("{name}: expected {expected}, got {result:?}"));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn journal_mount_lookup_follows_stacks_and_reachable_children() {
        let text = root_mount()
            + &mount(2, 1, "/mnt", "ext4", "bottom", "rw")
            + &mount(3, 2, "/mnt", "ext4", "middle", "rw")
            + &mount(4, 3, "/mnt", "xfs", "top", "rw");
        assert_eq!(
            inspect(&text, "/mnt/log", 0x58465342).unwrap().source,
            "top"
        );
        let text = text + &mount(5, 4, "/mnt/sub", "btrfs", "deep", "rw");
        let volume = inspect(&text, "/mnt/sub/log", 0x9123683E).unwrap();
        assert_eq!(volume.source, "deep");
        assert_eq!(volume.mount_point, Path::new("/mnt/sub"));
        // Component-wise matching must not mistake /mnt for /mnt-other.
        assert_eq!(
            inspect(&text, "/mnt-other/log", 0xEF53)
                .unwrap()
                .mount_point,
            Path::new("/")
        );
        let siblings = root_mount()
            + &mount(2, 1, "/mnt", "ext4", "older", "rw")
            + &mount(3, 1, "/mnt", "xfs", "later", "rw");
        assert_eq!(
            inspect(&siblings, "/mnt/log", 0x58465342).unwrap().source,
            "later"
        );
        let roots = root_mount() + &mount(9, 9, "/", "xfs", "last-root", "rw");
        assert_eq!(
            inspect(&roots, "/log", 0x58465342).unwrap().source,
            "last-root"
        );
    }

    /// An empty source is valid mountinfo and must still reach the allow-list.
    #[test]
    fn journal_empty_source_mount_is_parsed_and_refused() {
        let line = mount(2, 1, "/mnt", "tmpfs", "", "rw");
        let parsed = parse_journal_mount(line.lines().next().unwrap()).unwrap();
        assert_eq!(parsed.volume.source, "");
        assert_eq!(parsed.volume.fstype, "tmpfs");
        assert_eq!(parsed.super_options, "rw");
        let text = root_mount() + &line;
        assert_eq!(
            refusal(inspect(&text, "/mnt/log", 0x01021994)),
            "/mnt/log is on tmpfs, which is not a supported journal filesystem"
        );
    }

    /// A valid empty-source parent must not orphan a child's barrier policy.
    #[test]
    fn journal_empty_source_parent_preserves_child_refusal() {
        let text = root_mount()
            + &mount(2, 1, "/mnt", "tmpfs", "", "rw")
            + &mount(3, 2, "/mnt/journal", "ext4", "child", "rw,nobarrier");
        assert_eq!(
            refusal(inspect(&text, "/mnt/journal/log", 0xEF53)),
            "/mnt/journal/log: ext4 at /mnt/journal disables journal durability with nobarrier"
        );
    }

    /// A space inside a security option must not hide a later barrier token.
    #[test]
    fn journal_security_option_space_preserves_barrier_refusal() {
        let text = mount(1, 0, "/", "ext4", "disk", "rw,context=\"a b\",nobarrier");
        assert_eq!(
            refusal(inspect(&text, "/journal", 0xEF53)),
            "/journal: ext4 at / disables journal durability with nobarrier"
        );
    }

    /// Equal component depths select the later sibling despite a trailing slash.
    #[test]
    fn journal_mount_lookup_trailing_slash_tie_reaches_later_mount() {
        let text = root_mount()
            + &mount(2, 1, "/mnt", "ext4", "older", "rw")
            + &mount(3, 1, "/mnt/", "ext4", "later", "rw");
        let volume = inspect(&text, "/mnt/log", 0xEF53).unwrap();
        assert_eq!(volume.source, "later");
        assert_eq!(volume.mount_point, Path::new("/mnt/"));
    }

    #[test]
    fn journal_statfs_disagreement_is_refused() {
        let detail = refusal(inspect(&root_mount(), "/journal", 0x01021994));
        assert!(detail.contains("/journal"));
        assert!(
            detail.contains("mountinfo reports ext4 at / but statfs reports magic 0x1021994"),
            "{detail}"
        );
    }

    #[test]
    fn journal_statfs_error_is_io() {
        let params = tempfile::tempdir().unwrap();
        let result = inspect_journal_mountinfo(
            Path::new("/journal"),
            &root_mount(),
            |_| Err(std::io::ErrorKind::PermissionDenied.into()),
            params.path(),
        );
        assert!(
            matches!(result, Err(VolumeError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied)
        );
    }

    /// Collect each unsafe option's outcome so one mutation check witnesses them all.
    #[test]
    fn journal_barrier_options_refuse_disabled_flushes() {
        let mut failures = Vec::new();
        for (fstype, magic, token) in [
            ("ext4", 0xEF53, "nobarrier"),
            ("ext4", 0xEF53, "barrier=0"),
            ("f2fs", 0xF2F52010, "fsync_mode=nobarrier"),
            ("f2fs", 0xF2F52010, "checkpoint=disable"),
            ("f2fs", 0xF2F52010, "checkpoint=disable:10%"),
            ("bcachefs", 0xCA451A4E, "journal_flush_disabled"),
        ] {
            let text = mount(
                1,
                0,
                "/",
                fstype,
                "fixture",
                &format!("rw,{token},relatime"),
            );
            let result = inspect(&text, "/journal", magic);
            if !matches!(&result, Err(VolumeError::UntrustedVolume(detail)) if detail.contains(token) && detail.contains("/journal"))
            {
                failures.push(format!(
                    "{token}: expected refusal naming token and directory, got {result:?}"
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn journal_barrier_options_accept_safe_tokens() {
        for (fstype, magic, options) in [
            ("ext4", 0xEF53, "rw,barrier=1,data=writeback,commit=30"),
            ("f2fs", 0xF2F52010, "rw,fsync_mode=posix,checkpoint=enable"),
            ("f2fs", 0xF2F52010, "rw,fsync_mode=strict"),
            ("bcachefs", 0xCA451A4E, "rw"),
            // Only whole tokens (and the prescribed checkpoint prefix) are refused.
            ("ext4", 0xEF53, "rw,notnobarrier,barrier=01,nobarrier_extra"),
        ] {
            inspect(
                &mount(1, 0, "/", fstype, "fixture", options),
                "/journal",
                magic,
            )
            .unwrap();
        }
    }

    #[test]
    fn journal_zfs_parameters_require_zero_or_absence() {
        let params = tempfile::tempdir().unwrap();
        let text = mount(1, 0, "/", "zfs", "pool/journal", "rw");
        let check = || {
            inspect_journal_mountinfo(
                Path::new("/journal"),
                &text,
                |_| Ok(0x2FC12FC1),
                params.path(),
            )
        };
        check().unwrap();
        for name in ["zfs_nocacheflush", "zil_nocacheflush"] {
            for value in ["1\n", "unexpected", ""] {
                fs::write(params.path().join(name), value).unwrap();
                let detail = refusal(check());
                assert!(detail.contains("/journal"));
                assert!(
                    detail.contains(&format!("{name}={}", value.trim())),
                    "{detail}"
                );
            }
            fs::write(params.path().join(name), " 0\n").unwrap();
            check().unwrap();
        }
        // Both switches now exist and are zero.
        check().unwrap();
        fs::remove_file(params.path().join("zil_nocacheflush")).unwrap();
        check().unwrap();
    }

    #[test]
    fn journal_zfs_parameter_read_error_is_io_and_other_types_ignore_parameters() {
        let params = tempfile::tempdir().unwrap();
        // A directory is an unreadable parameter file even when tests run as root.
        fs::create_dir(params.path().join("zfs_nocacheflush")).unwrap();
        let text = mount(1, 0, "/", "zfs", "pool/journal", "rw");
        assert!(matches!(
            inspect_journal_mountinfo(
                Path::new("/journal"),
                &text,
                |_| Ok(0x2FC12FC1),
                params.path()
            ),
            Err(VolumeError::Io(_))
        ));
        inspect_journal_mountinfo(
            Path::new("/journal"),
            &root_mount(),
            |_| Ok(0xEF53),
            params.path(),
        )
        .unwrap();
    }

    #[test]
    fn journal_mount_lookup_unescapes_mount_point_and_source() {
        let text = root_mount()
            + &mount(
                2,
                1,
                "/mnt/my\\040journal",
                "ext4",
                "disk\\040name\\134suffix",
                "rw",
            );
        let volume = inspect(&text, "/mnt/my journal/log", 0xEF53).unwrap();
        assert_eq!(volume.mount_point, Path::new("/mnt/my journal"));
        assert_eq!(volume.source, "disk name\\suffix");
    }
}
