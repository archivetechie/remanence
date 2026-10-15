//! Shared streaming digest; callers retain ownership of file-open policy.
use sha2::{Digest, Sha256};
use std::io::{self, Read};

/// Hash a reader in bounded memory. Path handling, including secure no-follow
/// opens for imported authority, remains at the caller's filesystem boundary.
pub fn sha256_reader(reader: &mut (impl Read + ?Sized)) -> io::Result<[u8; 32]> {
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(hash.finalize().into()),
            Ok(n) => hash.update(&buffer[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}
