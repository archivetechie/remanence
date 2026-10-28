//! Bounded on-disk staging for one incoming append stream.
//!
//! A spool owns its temporary file until `finish` transfers responsibility to
//! the caller. Dropping an unfinished spool removes the partial file.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use uuid::Uuid;

pub(crate) struct Spool {
    file: std::fs::File,
    path: PathBuf,
    written: u64,
    cap: u64,
    keep: bool,
    hasher: Sha256,
}

impl Spool {
    pub(crate) fn create(dir: &Path, cap: u64) -> std::io::Result<Self> {
        let path = dir.join(format!("spool-{}.bin", Uuid::new_v4()));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|err| {
                std::io::Error::new(err.kind(), format!("open {}: {err}", path.display()))
            })?;
        Ok(Self {
            file,
            path,
            written: 0,
            cap,
            keep: false,
            hasher: Sha256::new(),
        })
    }

    pub(crate) fn write_chunk(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let next = self
            .written
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "spool size overflows u64")
            })?;
        if next > self.cap {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("spool size cap exceeded at {}", self.path.display()),
            ));
        }
        self.file.write_all(bytes).map_err(|err| {
            std::io::Error::new(err.kind(), format!("write {}: {err}", self.path.display()))
        })?;
        self.hasher.update(bytes);
        self.written = next;
        Ok(())
    }

    /// Return the digest of accepted chunks without rereading the spool.
    pub(crate) fn content_sha256(&self) -> [u8; 32] {
        self.hasher.clone().finalize().into()
    }

    pub(crate) fn finish(mut self) -> std::io::Result<PathBuf> {
        self.file.flush().map_err(|err| {
            std::io::Error::new(err.kind(), format!("flush {}: {err}", self.path.display()))
        })?;
        self.keep = true;
        Ok(self.path.clone())
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Spool {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
