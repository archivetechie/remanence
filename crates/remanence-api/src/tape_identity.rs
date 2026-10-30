//! BOT identity proof shared by mounted reads and append admission.

use crate::pool_write::{uuid_text, TapeIdentityError};
use remanence_library::BlockSource;
use remanence_parity::bootstrap::parse_bootstrap_block;

const VERIFY_BOOTSTRAP_READ_BYTES: usize = 1024 * 1024;

/// Verify that the block at BOT is a bootstrap for the expected tape UUID.
///
/// The helper uses only the generic [`BlockSource`] surface so tests can run
/// against [`remanence_library::VecBlockSource`]. It leaves the source
/// positioned immediately after the bootstrap block on success.
pub fn verify_tape_identity(
    source: &mut dyn BlockSource,
    expected_tape_uuid: &[u8; 16],
) -> Result<(), TapeIdentityError> {
    source.locate(0).map_err(TapeIdentityError::Io)?;
    let mut block = vec![0u8; VERIFY_BOOTSTRAP_READ_BYTES];
    let read = source
        .read_block(&mut block)
        .map_err(TapeIdentityError::Io)?;
    let payload = parse_bootstrap_block(&block[..read])
        .map_err(|err| TapeIdentityError::AbsentBootstrap(err.to_string()))?;
    if &payload.tape_uuid != expected_tape_uuid {
        return Err(TapeIdentityError::Mismatch {
            expected: uuid_text(*expected_tape_uuid),
            actual: uuid_text(payload.tape_uuid),
        });
    }
    Ok(())
}
