//! Physical tape read primitives for foreign-format readers.
//!
//! [`BlockSource`] is the object-local read surface used by native Remanence
//! body formats. Legacy or foreign tape formats sometimes need to decode the
//! physical tape stream first: variable-size records, filemarks, and the
//! drive's current block-size mode. This module provides that read-side
//! boundary without depending on Layer 3c parity internals.

use crate::handle::tape_io::{BlockSize, SpaceKind, TapeConfig, TapeIoError, TapePosition};
use crate::handle::DriveHandle;
#[cfg(target_os = "linux")]
use crate::scsi::decode_sense;

/// Physical block-address hint used by foreign-format readers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalTapePosition {
    /// Logical block address reported by READ POSITION / accepted by LOCATE.
    pub lba: u64,
    /// Tape partition number. Remanence currently uses partition 0.
    pub partition: u32,
}

impl PhysicalTapePosition {
    /// Construct a partition-0 physical tape position.
    pub const fn new(lba: u64) -> Self {
        Self { lba, partition: 0 }
    }
}

impl From<TapePosition> for PhysicalTapePosition {
    fn from(position: TapePosition) -> Self {
        Self {
            lba: position.lba,
            partition: position.partition,
        }
    }
}

/// Outcome of reading one physical tape record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhysicalReadOutcome {
    /// A data record was read into the caller's buffer.
    Data {
        /// Number of bytes delivered by the drive.
        bytes: usize,
        /// Physical position immediately after the record.
        position_after: PhysicalTapePosition,
    },
    /// A filemark was encountered and consumed.
    Filemark {
        /// Physical position immediately after the filemark.
        position_after: PhysicalTapePosition,
    },
    /// End-of-data was encountered.
    EndOfData {
        /// Physical position at EOD.
        position_after: PhysicalTapePosition,
    },
}

/// Outcome of spacing over physical filemarks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalFilemarkSpace {
    /// Signed number of filemarks actually traversed.
    pub filemarks_spaced: i64,
    /// Physical position immediately after the SPACE command.
    pub position_after: PhysicalTapePosition,
    /// True when the operation stopped at end-of-data before the full count.
    pub hit_end_of_data: bool,
}

/// Read-side physical tape access for legacy and foreign format drivers.
pub trait PhysicalTapeSource {
    /// Configure the drive's tape block-size mode for subsequent reads.
    fn configure_block_size(&mut self, block_size: BlockSize) -> Result<(), TapeIoError>;

    /// Locate to a physical block-address hint.
    fn locate_physical(
        &mut self,
        position: PhysicalTapePosition,
    ) -> Result<PhysicalTapePosition, TapeIoError>;

    /// Space forward or backward over filemarks.
    fn space_filemarks(&mut self, count: i64) -> Result<PhysicalFilemarkSpace, TapeIoError>;

    /// Read one physical record at the current tape position.
    fn read_record(&mut self, buf: &mut [u8]) -> Result<PhysicalReadOutcome, TapeIoError>;

    /// Return the current physical tape position.
    fn position(&mut self) -> Result<PhysicalTapePosition, TapeIoError>;
}

/// Adapter that exposes [`DriveHandle`] as a [`PhysicalTapeSource`].
pub struct DriveHandlePhysicalSource<'a> {
    drive: &'a mut DriveHandle,
    cursor_hint: Option<PhysicalTapePosition>,
}

impl<'a> DriveHandlePhysicalSource<'a> {
    /// Wrap a drive handle for physical tape reads.
    pub fn new(drive: &'a mut DriveHandle) -> Self {
        Self {
            drive,
            cursor_hint: None,
        }
    }

    fn current_or_seed_position(&mut self) -> Result<PhysicalTapePosition, TapeIoError> {
        if let Some(position) = self.cursor_hint {
            return Ok(position);
        }
        let position = self.drive.position()?.into();
        self.cursor_hint = Some(position);
        Ok(position)
    }

    fn resync_position(&mut self) -> Result<PhysicalTapePosition, TapeIoError> {
        let position = self.drive.position()?.into();
        self.cursor_hint = Some(position);
        Ok(position)
    }

    fn advance_after_block(
        &mut self,
        position_before: PhysicalTapePosition,
    ) -> Result<PhysicalTapePosition, TapeIoError> {
        let position_after = PhysicalTapePosition {
            lba: position_before.lba.checked_add(1).ok_or_else(|| {
                TapeIoError::OperationFailed(
                    "physical tape position overflow after block read".to_string(),
                )
            })?,
            partition: position_before.partition,
        };
        self.cursor_hint = Some(position_after);
        Ok(position_after)
    }
}

impl PhysicalTapeSource for DriveHandlePhysicalSource<'_> {
    fn configure_block_size(&mut self, block_size: BlockSize) -> Result<(), TapeIoError> {
        let current = self.drive.read_config()?;
        if current.block_size == block_size {
            return Ok(());
        }
        self.drive.write_config(TapeConfig {
            block_size,
            compression: current.compression,
            max_block_size_bytes: current.max_block_size_bytes,
            write_protected: current.write_protected,
            worm: current.worm,
        })
    }

    fn locate_physical(
        &mut self,
        position: PhysicalTapePosition,
    ) -> Result<PhysicalTapePosition, TapeIoError> {
        if position.partition != 0 {
            return Err(TapeIoError::OperationFailed(format!(
                "physical locate only supports partition 0, got {}",
                position.partition
            )));
        }
        let position = self.drive.locate(position.lba)?.into();
        self.cursor_hint = Some(position);
        Ok(position)
    }

    fn space_filemarks(&mut self, count: i64) -> Result<PhysicalFilemarkSpace, TapeIoError> {
        let outcome = self.drive.space(count, SpaceKind::Filemarks)?;
        let position_after = outcome.position_after.into();
        self.cursor_hint = Some(position_after);
        Ok(PhysicalFilemarkSpace {
            filemarks_spaced: outcome.units_traversed,
            position_after,
            hit_end_of_data: outcome.stopped_at_boundary,
        })
    }

    fn read_record(&mut self, buf: &mut [u8]) -> Result<PhysicalReadOutcome, TapeIoError> {
        let position_before = self.current_or_seed_position()?;
        match self.drive.read_block(buf) {
            Ok(bytes) => Ok(PhysicalReadOutcome::Data {
                bytes,
                position_after: self.advance_after_block(position_before)?,
            }),
            Err(err) => match classify_read_boundary(&err) {
                Some(PhysicalReadBoundary::Filemark) => Ok(PhysicalReadOutcome::Filemark {
                    position_after: self.resync_position()?,
                }),
                Some(PhysicalReadBoundary::EndOfData) => Ok(PhysicalReadOutcome::EndOfData {
                    position_after: self.resync_position()?,
                }),
                None => {
                    self.cursor_hint = None;
                    Err(err)
                }
            },
        }
    }

    fn position(&mut self) -> Result<PhysicalTapePosition, TapeIoError> {
        self.resync_position()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhysicalReadBoundary {
    Filemark,
    EndOfData,
}

fn classify_read_boundary(err: &TapeIoError) -> Option<PhysicalReadBoundary> {
    if matches!(err, TapeIoError::FilemarkEncountered) {
        return Some(PhysicalReadBoundary::Filemark);
    }

    #[cfg(target_os = "linux")]
    {
        let TapeIoError::CheckCondition(crate::scsi::ScsiError::CheckCondition { sense, .. }) = err
        else {
            return None;
        };
        let decoded = decode_sense(sense)?;

        if decoded.filemark
            && decoded.valid
            && decoded.key == 0x00
            && decoded.asc == 0x00
            && decoded.ascq == 0x01
        {
            return Some(PhysicalReadBoundary::Filemark);
        }
        if decoded.key == 0x08 && decoded.asc == 0x00 && decoded.ascq == 0x05 {
            return Some(PhysicalReadBoundary::EndOfData);
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = err;
        None
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn check_condition(sense: Vec<u8>) -> TapeIoError {
        TapeIoError::CheckCondition(crate::scsi::ScsiError::CheckCondition {
            sense,
            bytes_transferred: 0,
        })
    }

    #[test]
    fn classifies_fixed_sense_filemark_boundary() {
        let mut sense = vec![0; 18];
        sense[0] = 0xF0;
        sense[2] = 0x80;
        sense[13] = 0x01;

        assert_eq!(
            classify_read_boundary(&check_condition(sense)),
            Some(PhysicalReadBoundary::Filemark)
        );
    }

    #[test]
    fn classifies_structured_filemark_boundary() {
        assert_eq!(
            classify_read_boundary(&TapeIoError::FilemarkEncountered),
            Some(PhysicalReadBoundary::Filemark)
        );
    }

    #[test]
    fn classifies_fixed_sense_end_of_data_boundary() {
        let mut sense = vec![0; 18];
        sense[0] = 0x70;
        sense[2] = 0x08;
        sense[13] = 0x05;

        assert_eq!(
            classify_read_boundary(&check_condition(sense)),
            Some(PhysicalReadBoundary::EndOfData)
        );
    }
}
