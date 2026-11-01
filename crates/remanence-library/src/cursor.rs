//! Shared physical commit-cursor proof. File numbers remain authority metadata;
//! only the predecessor filemark and READ POSITION establish physical C.
use crate::{TapeIoError, TapePosition};

/// Internal cross-layer adapter for the one cursor proof algorithm.
#[doc(hidden)]
pub trait CursorSource {
    type Error: From<TapeIoError>;
    fn identity_generation(&self) -> u64 {
        0
    }
    fn locate(&mut self, partition: u32, lba: u64) -> Result<(), Self::Error>;
    fn read_filemark(&mut self, block: &mut [u8]) -> Result<bool, Self::Error>;
    fn position(&mut self) -> Result<TapePosition, Self::Error>;
}

/// Non-cloneable evidence, consumed before the exclusively borrowed drive moves.
#[doc(hidden)]
#[derive(Debug)]
pub struct CursorProof {
    pub(crate) position: TapePosition,
    pub(crate) identity_generation: u64,
}

impl CursorProof {
    /// Consume the physical evidence when installing an append admission.
    pub fn into_position(self) -> (TapePosition, u64) {
        (self.position, self.identity_generation)
    }
}

/// Prove a non-origin cursor by reading exactly its predecessor filemark, then
/// independently asking READ POSITION. EOD, data and short records never pass.
/// Origin must use the owner's existing fresh-media/bootstrap proof instead.
#[doc(hidden)]
pub fn prove_cursor<S: CursorSource + ?Sized>(
    source: &mut S,
    expected: TapePosition,
    block_size: u32,
) -> Result<CursorProof, S::Error> {
    let generation = source.identity_generation();
    let predecessor = expected
        .lba
        .checked_sub(1)
        .ok_or_else(|| TapeIoError::OperationFailed("origin requires bootstrap proof".into()))?;
    if block_size == 0 || block_size > crate::scsi::read_write::MAX_TRANSFER_LEN {
        return Err(TapeIoError::OperationFailed("invalid cursor proof block size".into()).into());
    }
    source.locate(expected.partition, predecessor)?;
    if !source.read_filemark(&mut vec![0; block_size as usize])? {
        return Err(TapeIoError::CursorPredecessorNotFilemark.into());
    }
    let actual = source.position()?;
    if actual.partition != expected.partition || actual.lba != expected.lba {
        return Err(TapeIoError::OperationFailed(format!(
            "commit cursor position mismatch: expected {expected:?}, got {actual:?}"
        ))
        .into());
    }
    if source.identity_generation() != generation {
        return Err(TapeIoError::IdentityInvalidated {
            proved: generation,
            current: source.identity_generation(),
        }
        .into());
    }
    Ok(CursorProof {
        position: actual,
        identity_generation: generation,
    })
}

impl CursorSource for crate::DriveHandle {
    type Error = TapeIoError;
    fn identity_generation(&self) -> u64 {
        self.identity_generation()
    }
    fn locate(&mut self, partition: u32, lba: u64) -> Result<(), TapeIoError> {
        let actual = self.locate(lba)?;
        if actual.partition != partition || actual.lba != lba {
            return Err(TapeIoError::OperationFailed(
                "cursor predecessor locate mismatch".into(),
            ));
        }
        Ok(())
    }
    fn read_filemark(&mut self, block: &mut [u8]) -> Result<bool, TapeIoError> {
        match self.read_block(block) {
            Err(TapeIoError::FilemarkEncountered) => Ok(true),
            Err(TapeIoError::ReadBufferTooSmall { .. }) => {
                Err(TapeIoError::CursorPredecessorNotFilemark)
            }
            Ok(_) => Ok(false),
            Err(error) => Err(error),
        }
    }
    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        self.position()
    }
}
