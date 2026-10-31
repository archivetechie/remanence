//! Old authority import storage operations. The caller proves media identity
//! and position; this backend preserves regular-file checks, conflict refusal,
//! publication order and directory durability for the existing journal pair.
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// Opaque staged authority member. Only this backend names its stored files.
pub struct ImportPublication {
    source: PathBuf,
    destination: PathBuf,
}

impl super::TapeAuthority {
    /// Preserve the existing no-symlink regular-file import precondition.
    pub fn is_regular_import_source(&self) -> io::Result<bool> {
        Ok(std::fs::symlink_metadata(self.path())?.is_file())
    }
    /// Stage this authority's commit member for later ordered publication.
    pub fn stage_import_member(
        &self,
        staging_directory: &Path,
        destination_directory: &Path,
    ) -> io::Result<ImportPublication> {
        let filename = self
            .path()
            .file_name()
            .ok_or_else(|| io::Error::other("checkpoint path has no filename"))?;
        let source = staging_directory.join(filename);
        copy_import_authority(self.path(), &source)?;
        Ok(ImportPublication {
            source,
            destination: destination_directory.join(filename),
        })
    }
}

/// Stage an authority member without following a final-component symlink.
fn copy_import_authority(source: &Path, destination: &Path) -> io::Result<()> {
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(source)?;
    if !input.metadata()?.is_file() {
        return Err(io::Error::other("journal authority must be a regular file"));
    }
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    io::copy(&mut input, &mut output)?;
    output.sync_all()
}

fn same_authority(a: &Path, b: &Path) -> io::Result<bool> {
    fn digest(path: &Path) -> io::Result<[u8; 32]> {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(path)?;
        digest_reader(&mut file)
    }
    Ok(digest(a)? == digest(b)?)
}

fn digest_reader(reader: &mut impl Read) -> io::Result<[u8; 32]> {
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(digest.finalize().into()),
            Ok(count) => digest.update(&buffer[..count]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

/// Refuse every conflict before publishing any member, then link in the
/// caller's established order and sync each destination directory.
pub fn publish_import_authority(publications: &[ImportPublication]) -> io::Result<()> {
    for ImportPublication {
        source,
        destination,
    } in publications
    {
        if destination.exists() && !same_authority(source, destination)? {
            return Err(io::Error::other(format!(
                "local journal {} differs; refusing overwrite",
                destination.display()
            )));
        }
    }
    for ImportPublication {
        source,
        destination,
    } in publications
    {
        if !destination.exists() {
            std::fs::hard_link(source, destination)?;
            File::open(
                destination
                    .parent()
                    .expect("authority destination has a directory"),
            )?
            .sync_all()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn authority_import_hash_retries_interrupted_reads() {
        struct InterruptedOnce {
            interrupted: bool,
            bytes: io::Cursor<&'static [u8]>,
        }
        impl Read for InterruptedOnce {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(io::ErrorKind::Interrupted.into());
                }
                self.bytes.read(buffer)
            }
        }
        let mut reader = InterruptedOnce {
            interrupted: false,
            bytes: io::Cursor::new(b"authority"),
        };
        assert_eq!(
            digest_reader(&mut reader).unwrap(),
            <[u8; 32]>::from(Sha256::digest(b"authority"))
        );
    }

    #[test]
    fn authority_hashing_keeps_no_follow_open_policy() {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("a");
        let b = root.path().join("b");
        let link = root.path().join("link");
        std::fs::write(&a, b"authority").unwrap();
        std::fs::write(&b, b"authority").unwrap();
        assert!(same_authority(&a, &b).unwrap());
        std::os::unix::fs::symlink(&a, &link).unwrap();
        assert!(same_authority(&a, &link).is_err());
        std::fs::write(&b, b"changed").unwrap();
        assert!(!same_authority(&a, &b).unwrap());
    }
}
