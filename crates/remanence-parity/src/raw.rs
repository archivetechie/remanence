//! Raw physical-tape access traits for Layer 3c.
//!
//! The body-facing [`BlockSource`] and
//! [`BlockSink`] traits are intentionally
//! object-local. Bootstrap discovery, catalog-less scanning, and future
//! sidecar/filemark handling need physical tape operations instead: configure
//! fixed-block size, locate to a physical address, read records, and space
//! filemarks. This module is the v0.4.4 bridge for those operations.

#[cfg(target_os = "linux")]
use remanence_library::scsi::decode_sense;
use remanence_library::{
    BlockSink, BlockSize, BlockSource, DriveHandle, SpaceKind, TapeConfig, TapeIoError,
    TapePosition, WriteFilemarksOutcome, WriteOutcome, WriteUnpositionedOutcome,
};

use crate::error::ParityError;

/// Physical block-address hint used by raw tape operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalPositionHint {
    /// Physical logical block address from READ POSITION / LOCATE.
    pub lba: u64,
    /// Tape partition number. Remanence currently uses partition 0.
    pub partition: u32,
}

impl PhysicalPositionHint {
    /// Construct a partition-0 physical-position hint.
    pub const fn new(lba: u64) -> Self {
        Self { lba, partition: 0 }
    }
}

impl From<TapePosition> for PhysicalPositionHint {
    fn from(position: TapePosition) -> Self {
        Self {
            lba: position.lba,
            partition: position.partition,
        }
    }
}

/// Outcome of reading one physical tape record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawReadOutcome {
    /// A data record was read into the caller's buffer.
    Block {
        /// Number of bytes delivered by the drive.
        bytes: usize,
        /// Physical position immediately after the record.
        position_after: PhysicalPositionHint,
    },
    /// A filemark was encountered and consumed.
    Filemark {
        /// Physical position immediately after the filemark.
        position_after: PhysicalPositionHint,
    },
    /// End-of-data was encountered.
    EndOfData {
        /// Physical position at EOD.
        position_after: PhysicalPositionHint,
    },
}

impl RawReadOutcome {
    /// Physical position immediately after the raw read outcome.
    pub fn position_after(self) -> PhysicalPositionHint {
        match self {
            Self::Block { position_after, .. }
            | Self::Filemark { position_after }
            | Self::EndOfData { position_after } => position_after,
        }
    }
}

/// Outcome of writing one physical tape object through [`RawTapeSink`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawWriteOutcome {
    /// A fixed-size data block was written.
    WroteBlock {
        /// Physical position immediately after the block.
        position_after: PhysicalPositionHint,
        /// True when the drive reported programmable early warning.
        early_warning: bool,
        /// True when the drive reported hard end-of-medium.
        end_of_medium: bool,
    },
    /// A synchronous filemark durability barrier was written.
    WroteFilemark {
        /// Physical position immediately after the filemark.
        position_after: PhysicalPositionHint,
        /// True when the drive reported programmable early warning.
        early_warning: bool,
        /// True when the drive reported hard end-of-medium.
        end_of_medium: bool,
    },
}

impl RawWriteOutcome {
    /// Physical position immediately after the raw write.
    pub fn position_after(self) -> PhysicalPositionHint {
        match self {
            Self::WroteBlock { position_after, .. }
            | Self::WroteFilemark { position_after, .. } => position_after,
        }
    }

    /// Whether the raw write crossed the drive's early-warning point.
    pub fn early_warning(self) -> bool {
        match self {
            Self::WroteBlock { early_warning, .. } | Self::WroteFilemark { early_warning, .. } => {
                early_warning
            }
        }
    }

    /// Whether the raw write reached hard end-of-medium.
    pub fn end_of_medium(self) -> bool {
        match self {
            Self::WroteBlock { end_of_medium, .. } | Self::WroteFilemark { end_of_medium, .. } => {
                end_of_medium
            }
        }
    }
}

/// Outcome of spacing over physical filemarks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpaceFilemarksOutcome {
    /// Signed number of filemarks actually traversed.
    pub filemarks_spaced: i64,
    /// Physical position immediately after the SPACE command.
    pub position_after: PhysicalPositionHint,
    /// True when the operation stopped at end-of-data before the full count.
    pub hit_end_of_data: bool,
}

/// Geometry hints supplied to bootstrap discovery and scan reconstruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapeGeometryHint {
    /// Known fixed block size from catalog or operator configuration.
    pub configured_block_size: Option<u32>,
    /// Candidate fixed block sizes to try when the configured size is unknown.
    pub candidate_block_sizes: Vec<u32>,
    /// Physical positions to probe for bootstrap copies.
    pub probe_positions: Vec<PhysicalPositionHint>,
}

/// Raw physical-tape READ access used by Layer 3c.
pub trait RawTapeSource {
    /// Configure the drive for fixed-block reads at `block_size` bytes.
    fn configure_fixed_block_size(&mut self, block_size: u32) -> Result<(), ParityError>;

    /// Locate to a physical block-address hint.
    fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError>;

    /// Space forward or backward over filemarks.
    fn space_filemarks(&mut self, count: i64) -> Result<SpaceFilemarksOutcome, ParityError>;

    /// Read one physical record at the current position.
    fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError>;

    /// Return the current physical tape position.
    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError>;
}

/// Raw physical-tape WRITE access used by Layer 3c.
///
/// This trait is intentionally below the body-facing
/// [`BlockSink`] surface: Layer 3c owns
/// physical object, sidecar, bootstrap, and filemark emission. In particular,
/// [`Self::write_filemarks`] carries both advisory delimiters and synchronous
/// zero-count durability barriers.
///
/// Implementations must preserve completion-unknown failures from the tape
/// transport as `ParityError::TapeIo(TapeIoError::Transport(_))`. Do not wrap
/// SG_IO timeouts, disconnects, or equivalent driver-level failures inside
/// string-only `ParityError` variants; Layer 5 relies on the transport variant
/// to observe the dirty-bit / rescan-required signal.
pub trait RawTapeSink {
    /// Append one fixed-size tape block at the current physical position.
    ///
    /// The caller must pass exactly the fixed block size configured for the
    /// tape session. Raw adapters may rely on the drive's fixed-block mode for
    /// enforcement; a size mismatch is a caller invariant violation, not an
    /// alternate variable-block write path.
    fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError>;

    /// Issue WRITE FILEMARKS with the supplied count and IMMED bit.
    ///
    /// Object/control delimiters use `(1, true)`. A checkpoint barrier uses
    /// `(0, false)`, whose successful completion proves all preceding writes
    /// reached the medium without adding another tape-file boundary.
    fn write_filemarks(&mut self, count: u32, immed: bool) -> Result<RawWriteOutcome, ParityError>;

    /// Return the current physical tape position.
    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError>;
}

/// Test/compatibility adapter from the legacy body-facing `BlockSink` to
/// [`RawTapeSink`].
///
/// Production write paths should prefer [`DriveHandleRawSink`]. This adapter
/// exists so in-memory fixtures can exercise the raw write API while the sink
/// migration from `BlockSink` to `RawTapeSink` is still underway.
pub struct BlockSinkRawTapeSink<'a> {
    inner: &'a mut dyn BlockSink,
}

impl<'a> BlockSinkRawTapeSink<'a> {
    /// Wrap a legacy block sink.
    pub fn new(inner: &'a mut dyn BlockSink) -> Self {
        Self { inner }
    }
}

impl RawTapeSink for BlockSinkRawTapeSink<'_> {
    fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
        Ok(raw_block_outcome(self.inner.write_block(buf)?))
    }

    fn write_filemarks(&mut self, count: u32, immed: bool) -> Result<RawWriteOutcome, ParityError> {
        if immed {
            self.inner.write_filemarks_immediate(count)?;
            Ok(RawWriteOutcome::WroteFilemark {
                position_after: self.inner.position()?.into(),
                early_warning: false,
                end_of_medium: false,
            })
        } else {
            Ok(raw_filemark_outcome(self.inner.write_filemarks(count)?))
        }
    }

    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        Ok(self.inner.position()?.into())
    }
}

/// Test/compatibility adapter from the legacy body-facing `BlockSource` to
/// [`RawTapeSource`].
///
/// This adapter is intended for in-memory tests and legacy callers only. It
/// cannot reconfigure a real drive's fixed block size because `BlockSource`
/// has no configuration hook; it records the requested block size only so
/// existing fixtures can exercise the raw API. Production tape access must use
/// [`DriveHandleRawSource`].
pub struct BlockSourceRawTapeSource<'a> {
    inner: &'a mut dyn BlockSource,
    cursor_hint: PhysicalPositionHint,
    configured_block_size: Option<u32>,
}

impl<'a> BlockSourceRawTapeSource<'a> {
    /// Wrap a legacy block source.
    pub fn new(inner: &'a mut dyn BlockSource) -> Self {
        Self {
            inner,
            cursor_hint: PhysicalPositionHint::new(0),
            configured_block_size: None,
        }
    }

    /// Last fixed block size requested through the raw trait.
    pub fn configured_block_size(&self) -> Option<u32> {
        self.configured_block_size
    }
}

impl RawTapeSource for BlockSourceRawTapeSource<'_> {
    fn configure_fixed_block_size(&mut self, block_size: u32) -> Result<(), ParityError> {
        if block_size == 0 {
            return Err(ParityError::Invariant("fixed block size is zero"));
        }
        self.configured_block_size = Some(block_size);
        Ok(())
    }

    fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
        let position = self.inner.locate(hint.lba)?;
        self.cursor_hint = position.into();
        Ok(())
    }

    fn space_filemarks(&mut self, count: i64) -> Result<SpaceFilemarksOutcome, ParityError> {
        let outcome = self.inner.space(count, SpaceKind::Filemarks)?;
        self.cursor_hint = outcome.position_after.into();
        Ok(SpaceFilemarksOutcome {
            filemarks_spaced: outcome.units_traversed,
            position_after: self.cursor_hint,
            hit_end_of_data: outcome.stopped_at_boundary,
        })
    }

    fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
        let bytes = self.inner.read_block(buf)?;
        self.cursor_hint.lba = self.cursor_hint.lba.saturating_add(1);
        Ok(RawReadOutcome::Block {
            bytes,
            position_after: self.cursor_hint,
        })
    }

    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        let position = self.inner.position()?;
        self.cursor_hint = position.into();
        Ok(self.cursor_hint)
    }
}

/// Production raw-source adapter over Layer 3a's [`DriveHandle`].
pub struct DriveHandleRawSource<'a> {
    drive: &'a mut DriveHandle,
    cursor_hint: Option<PhysicalPositionHint>,
}

impl<'a> DriveHandleRawSource<'a> {
    /// Wrap a drive handle for raw Layer 3c reads.
    pub fn new(drive: &'a mut DriveHandle) -> Self {
        Self {
            drive,
            cursor_hint: None,
        }
    }

    fn current_or_seed_position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        if let Some(position) = self.cursor_hint {
            return Ok(position);
        }
        let position: PhysicalPositionHint = self.drive.position()?.into();
        self.cursor_hint = Some(position);
        Ok(position)
    }

    fn resync_position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        let position: PhysicalPositionHint = self.drive.position()?.into();
        self.cursor_hint = Some(position);
        Ok(position)
    }

    fn advance_after_block(
        &mut self,
        position_before: PhysicalPositionHint,
    ) -> Result<PhysicalPositionHint, ParityError> {
        let position_after = PhysicalPositionHint {
            lba: position_before
                .lba
                .checked_add(1)
                .ok_or(ParityError::Invariant(
                    "raw source physical position overflow after block read",
                ))?,
            partition: position_before.partition,
        };
        self.cursor_hint = Some(position_after);
        Ok(position_after)
    }
}

/// Production raw-sink adapter over Layer 3a's [`DriveHandle`].
pub struct DriveHandleRawSink<'a> {
    drive: &'a mut DriveHandle,
    cursor_hint: Option<PhysicalPositionHint>,
}

impl<'a> DriveHandleRawSink<'a> {
    /// Wrap a drive handle for raw Layer 3c writes.
    pub fn new(drive: &'a mut DriveHandle) -> Self {
        Self {
            drive,
            cursor_hint: None,
        }
    }

    /// Configure and verify the Layer 3c parity-write preconditions.
    ///
    /// v0.7.2 makes hardware compression a hard-false precondition for
    /// parity-protected writes because compression breaks the stable
    /// logical-block to physical-extent geometry that sidecar recovery relies
    /// on. This method must be called before the BOT bootstrap is written.
    pub fn configure_parity_write_session(&mut self, block_size: u32) -> Result<(), ParityError> {
        if block_size == 0 {
            return Err(ParityError::Invariant("fixed block size is zero"));
        }
        let current = self
            .drive
            .read_config()
            .map_err(|_| ParityError::DriveCompressionModeUnknown)?;
        let desired_block_size = BlockSize::Fixed {
            size_bytes: block_size,
        };
        self.drive.write_config(TapeConfig {
            block_size: desired_block_size,
            compression: false,
            max_block_size_bytes: current.max_block_size_bytes,
            write_protected: current.write_protected,
            worm: current.worm,
        })?;
        let verified = self
            .drive
            .read_config()
            .map_err(|_| ParityError::DriveCompressionModeUnknown)?;
        if verified.compression {
            return Err(ParityError::DriveCompressionEnabled);
        }
        if verified.block_size != desired_block_size {
            return Err(ParityError::SessionOpen(format!(
                "drive read back block size {:?}, expected {:?}",
                verified.block_size, desired_block_size
            )));
        }
        Ok(())
    }

    fn current_or_seed_position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        if let Some(position) = self.cursor_hint {
            return Ok(position);
        }
        let position: PhysicalPositionHint = self.drive.position()?.into();
        self.cursor_hint = Some(position);
        Ok(position)
    }
}

impl RawTapeSink for DriveHandleRawSink<'_> {
    fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
        let position_before = self.current_or_seed_position()?;
        let outcome = self.drive.write_block_unpositioned(buf)?;
        let position_after = PhysicalPositionHint {
            lba: position_before
                .lba
                .checked_add(1)
                .ok_or(ParityError::Invariant(
                    "raw sink physical position overflow after block write",
                ))?,
            partition: position_before.partition,
        };
        self.cursor_hint = Some(position_after);
        Ok(raw_unpositioned_block_outcome(outcome, position_after))
    }

    fn write_filemarks(&mut self, count: u32, immed: bool) -> Result<RawWriteOutcome, ParityError> {
        let outcome = if immed {
            let position_before = self.current_or_seed_position()?;
            self.drive.write_filemarks_immediate(count)?;
            RawWriteOutcome::WroteFilemark {
                position_after: PhysicalPositionHint {
                    lba: position_before.lba.checked_add(u64::from(count)).ok_or(
                        ParityError::Invariant(
                            "raw sink physical position overflow after immediate filemarks",
                        ),
                    )?,
                    partition: position_before.partition,
                },
                early_warning: false,
                end_of_medium: false,
            }
        } else {
            raw_filemark_outcome(self.drive.write_filemarks(count)?)
        };
        self.cursor_hint = Some(outcome.position_after());
        Ok(outcome)
    }

    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        let position = self.drive.position()?.into();
        self.cursor_hint = Some(position);
        Ok(position)
    }
}

impl RawTapeSource for DriveHandleRawSource<'_> {
    fn configure_fixed_block_size(&mut self, block_size: u32) -> Result<(), ParityError> {
        if block_size == 0 {
            return Err(ParityError::Invariant("fixed block size is zero"));
        }

        let current = self.drive.read_config()?;
        let desired = BlockSize::Fixed {
            size_bytes: block_size,
        };
        if current.block_size == desired {
            return Ok(());
        }

        self.drive.write_config(TapeConfig {
            block_size: desired,
            compression: current.compression,
            max_block_size_bytes: current.max_block_size_bytes,
            write_protected: current.write_protected,
            worm: current.worm,
        })?;
        Ok(())
    }

    fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
        let position = self.drive.locate(hint.lba)?;
        self.cursor_hint = Some(position.into());
        Ok(())
    }

    fn space_filemarks(&mut self, count: i64) -> Result<SpaceFilemarksOutcome, ParityError> {
        let outcome = self.drive.space(count, SpaceKind::Filemarks)?;
        let position_after = outcome.position_after.into();
        self.cursor_hint = Some(position_after);
        Ok(SpaceFilemarksOutcome {
            filemarks_spaced: outcome.units_traversed,
            position_after,
            hit_end_of_data: outcome.stopped_at_boundary,
        })
    }

    fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
        let position_before = self.current_or_seed_position()?;
        match self.drive.read_block(buf) {
            Ok(bytes) => {
                let position_after = self.advance_after_block(position_before)?;
                Ok(RawReadOutcome::Block {
                    bytes,
                    position_after,
                })
            }
            Err(err) => match classify_read_boundary(&err) {
                Some(RawReadBoundary::Filemark) => Ok(RawReadOutcome::Filemark {
                    position_after: self.resync_position()?,
                }),
                Some(RawReadBoundary::EndOfData) => Ok(RawReadOutcome::EndOfData {
                    position_after: self.resync_position()?,
                }),
                None => {
                    self.cursor_hint = None;
                    Err(err.into())
                }
            },
        }
    }

    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.resync_position()
    }
}

fn raw_block_outcome(outcome: WriteOutcome) -> RawWriteOutcome {
    RawWriteOutcome::WroteBlock {
        position_after: outcome.position_after.into(),
        early_warning: outcome.early_warning,
        end_of_medium: outcome.end_of_medium,
    }
}

fn raw_unpositioned_block_outcome(
    outcome: WriteUnpositionedOutcome,
    position_after: PhysicalPositionHint,
) -> RawWriteOutcome {
    RawWriteOutcome::WroteBlock {
        position_after,
        early_warning: outcome.early_warning,
        end_of_medium: outcome.end_of_medium,
    }
}

fn raw_filemark_outcome(outcome: WriteFilemarksOutcome) -> RawWriteOutcome {
    RawWriteOutcome::WroteFilemark {
        position_after: outcome.position_after.into(),
        early_warning: outcome.early_warning,
        end_of_medium: outcome.end_of_medium,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawReadBoundary {
    Filemark,
    EndOfData,
}

fn classify_read_boundary(err: &TapeIoError) -> Option<RawReadBoundary> {
    #[cfg(target_os = "linux")]
    {
        let TapeIoError::CheckCondition(remanence_library::scsi::ScsiError::CheckCondition {
            sense,
            ..
        }) = err
        else {
            return None;
        };
        let decoded = decode_sense(sense)?;

        // IBM LTO SCSI Reference GA32-0928-08 §4.12.1: a READ that
        // encounters a filemark returns CHECK CONDITION with FILEMARK +
        // VALID set and associated sense 0/0001. The position is after the
        // filemark, so the raw adapter reports a consumed boundary.
        if decoded.filemark
            && decoded.valid
            && decoded.key == 0x00
            && decoded.asc == 0x00
            && decoded.ascq == 0x01
        {
            return Some(RawReadBoundary::Filemark);
        }

        // IBM Annex B Table B.9 / Layer 3a fixture convention: READ at EOD
        // reports BLANK CHECK with ASC/ASCQ 00/05.
        if decoded.key == 0x08 && decoded.asc == 0x00 && decoded.ascq == 0x05 {
            return Some(RawReadBoundary::EndOfData);
        }

        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = err;
        None
    }
}

#[cfg(test)]
mod compat_tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    use remanence_library::{
        DriveBay, ElementLayout, FixtureTransport, IdentitySource, IePort, InstalledDrive,
        IoErrorKind, Library, RecordingLog, RecordingTransport, SgTransport, Slot, StaticAllowlist,
        VecBlockSink, VecBlockSource, VecBlockSourceCall,
    };

    #[test]
    fn block_sink_adapter_writes_single_filemark_boundary() {
        let mut sink = VecBlockSink::new();

        let (block_outcome, filemark_outcome, position) = {
            let mut raw = BlockSinkRawTapeSink::new(&mut sink);
            let block_outcome = raw
                .write_fixed_block(&[0xAB; 4])
                .expect("block write succeeds");
            let filemark_outcome = raw.write_filemarks(1, false).expect("filemark succeeds");
            let position = raw.position().expect("position succeeds");
            (block_outcome, filemark_outcome, position)
        };

        assert_eq!(
            block_outcome,
            RawWriteOutcome::WroteBlock {
                position_after: PhysicalPositionHint::new(1),
                early_warning: false,
                end_of_medium: false,
            }
        );
        assert_eq!(
            filemark_outcome,
            RawWriteOutcome::WroteFilemark {
                position_after: PhysicalPositionHint::new(2),
                early_warning: false,
                end_of_medium: false,
            }
        );
        assert_eq!(position, PhysicalPositionHint::new(2));
        assert_eq!(sink.blocks, vec![vec![0xAB; 4]]);
        assert_eq!(sink.filemarks, vec![1]);
        assert_eq!(sink.next_lba(), 2);
    }

    fn vpd80_response(serial: &str) -> Vec<u8> {
        let bytes = serial.as_bytes();
        let mut response = vec![0x08u8, 0x80, 0x00, bytes.len() as u8];
        response.extend_from_slice(bytes);
        response
    }

    fn changer_inquiry_response() -> Vec<u8> {
        include_bytes!("../../../fixtures/inquiry/changer-msl-g3.bin").to_vec()
    }

    fn lto9_inquiry_response() -> Vec<u8> {
        include_bytes!("../../../fixtures/inquiry/drive1-lto9.bin").to_vec()
    }

    fn read_position_long_response(flags: u8, partition: u32, lba: u64) -> Vec<u8> {
        let mut response = vec![0u8; 32];
        response[0] = flags;
        response[4..8].copy_from_slice(&partition.to_be_bytes());
        response[8..16].copy_from_slice(&lba.to_be_bytes());
        response
    }

    fn rbl_response(max_block_length: u32, min_block_length: u16) -> Vec<u8> {
        let mut buf = vec![0u8; 6];
        let max = max_block_length.to_be_bytes();
        buf[1] = max[1];
        buf[2] = max[2];
        buf[3] = max[3];
        let min = min_block_length.to_be_bytes();
        buf[4] = min[0];
        buf[5] = min[1];
        buf
    }

    fn mode_sense_response(block_length: u32, dce: bool) -> Vec<u8> {
        let mut buf = vec![0u8; 28];
        buf[0] = 27;
        buf[3] = 8;
        let bl = block_length.to_be_bytes();
        buf[9] = bl[1];
        buf[10] = bl[2];
        buf[11] = bl[3];
        buf[12] = remanence_library::scsi::mode::PAGE_DATA_COMPRESSION;
        buf[13] = 14;
        buf[14] = if dce { 0x80 } else { 0x00 };
        buf
    }

    fn open_drive_test_lib(library_serial: &str) -> Library {
        Library {
            serial: library_serial.to_string(),
            changer_sg: PathBuf::from("/dev/sg-mock"),
            changer_sysfs: PathBuf::from("/sys/class/scsi_device/mock"),
            changer_inquiry: remanence_library::scsi::Inquiry::parse(include_bytes!(
                "../../../fixtures/inquiry/changer-msl-g3.bin"
            ))
            .unwrap(),
            chassis_designator: None,
            layout: ElementLayout {
                robot_address: 0,
                drive_start: 0x0100,
                drive_count: 1,
                slot_start: 0x0400,
                slot_count: 1,
                ie_start: 0,
                ie_count: 0,
            },
            drive_bays: vec![DriveBay {
                element_address: 0x0100,
                accessible: true,
                exception: None,
                installed: Some(InstalledDrive {
                    serial: "DRV_A".into(),
                    identity_source: IdentitySource::DvcidAndInquiry,
                    vendor: None,
                    product: None,
                    revision: None,
                    sg_path: Some(PathBuf::from("/dev/sg-drive-mock")),
                    sysfs_path: None,
                }),
                loaded: false,
                loaded_tape: None,
                source_slot: None,
            }],
            slots: vec![Slot {
                element_address: 0x0400,
                accessible: true,
                exception: None,
                full: true,
                cartridge: Some("TAPE_A".into()),
            }],
            ie_ports: Vec::<IePort>::new(),
        }
    }

    #[allow(clippy::type_complexity)]
    fn multi_recording_factory(
        scripts: Vec<(PathBuf, Vec<Vec<u8>>)>,
    ) -> (
        Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>>,
        RecordingLog,
    ) {
        let log: RecordingLog = RecordingLog::new();
        let log_cl = log.clone();
        let mut bag: HashMap<PathBuf, FixtureTransport> = scripts
            .into_iter()
            .map(|(path, responses)| (path, FixtureTransport::new().with_responses(responses)))
            .collect();
        let factory: Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>> =
            Box::new(move |path: &Path| {
                let inner = bag.remove(path).ok_or_else(|| IoErrorKind {
                    kind: "NotFound",
                    message: format!("no fixture transport seeded for {path:?}"),
                    raw_os_error: None,
                })?;
                let wrapped = RecordingTransport::with_log(inner, log_cl.clone());
                Ok(Box::new(wrapped) as Box<dyn SgTransport>)
            });
        (factory, log)
    }

    #[test]
    fn parity_write_session_configures_and_verifies_compression_off() {
        let lib = open_drive_test_lib("LIB_RAW_CFG01");
        let policy = StaticAllowlist::new(["LIB_RAW_CFG01"]);
        let block_size = 262_144;
        let (factory, log) = multi_recording_factory(vec![
            (
                PathBuf::from("/dev/sg-mock"),
                vec![changer_inquiry_response(), vpd80_response("LIB_RAW_CFG01")],
            ),
            (
                PathBuf::from("/dev/sg-drive-mock"),
                vec![
                    lto9_inquiry_response(),
                    vpd80_response("DRV_A"),
                    rbl_response(0x80_0000, 1),
                    mode_sense_response(0, true),
                    rbl_response(0x80_0000, 1),
                    mode_sense_response(block_size, false),
                ],
            ),
        ]);
        let mut handle = lib.open_with(&policy, factory).expect("library opens");

        {
            let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
            let mut raw = DriveHandleRawSink::new(&mut drive);
            raw.configure_parity_write_session(block_size)
                .expect("parity session configures compression-off and fixed block size");
        }

        let config_cdbs = log
            .borrow()
            .iter()
            .filter(|cdb| matches!(cdb[0], 0x05 | 0x15 | 0x1A))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            config_cdbs.iter().map(|cdb| cdb[0]).collect::<Vec<_>>(),
            vec![0x05, 0x1A, 0x15, 0x05, 0x1A],
            "configure_parity_write_session must read, select, then read back: {config_cdbs:?}"
        );
        assert_eq!(
            config_cdbs[1],
            remanence_library::scsi::mode::build_mode_sense6_cdb(
                remanence_library::scsi::mode::PageControl::Current,
                remanence_library::scsi::mode::PAGE_DATA_COMPRESSION,
                64,
            )
        );
        assert_eq!(
            config_cdbs[2].as_slice(),
            &[0x15, 0x10, 0x00, 0x00, 28, 0x00],
            "MODE SELECT(6) must use PF=1, SP=0, and the 28-byte compression parameter list"
        );
        assert_eq!(config_cdbs[4], config_cdbs[1]);
        assert!(!handle.is_dirty());
    }

    #[test]
    fn parity_write_session_rejects_compression_enabled_readback() {
        let lib = open_drive_test_lib("LIB_RAW_CFG02");
        let policy = StaticAllowlist::new(["LIB_RAW_CFG02"]);
        let block_size = 262_144;
        let (factory, log) = multi_recording_factory(vec![
            (
                PathBuf::from("/dev/sg-mock"),
                vec![changer_inquiry_response(), vpd80_response("LIB_RAW_CFG02")],
            ),
            (
                PathBuf::from("/dev/sg-drive-mock"),
                vec![
                    lto9_inquiry_response(),
                    vpd80_response("DRV_A"),
                    rbl_response(0x80_0000, 1),
                    mode_sense_response(0, true),
                    rbl_response(0x80_0000, 1),
                    mode_sense_response(block_size, true),
                ],
            ),
        ]);
        let mut handle = lib.open_with(&policy, factory).expect("library opens");

        let err = {
            let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
            let mut raw = DriveHandleRawSink::new(&mut drive);
            raw.configure_parity_write_session(block_size)
                .expect_err("compression-enabled read-back must be refused")
        };

        assert!(matches!(err, ParityError::DriveCompressionEnabled));
        assert!(
            log.borrow().iter().any(|cdb| cdb[0] == 0x15),
            "the adapter should attempt MODE SELECT before rejecting the read-back"
        );
        assert!(!handle.is_dirty());
    }

    #[test]
    fn parity_write_session_maps_config_readback_failure_to_unknown() {
        let lib = open_drive_test_lib("LIB_RAW_CFG03");
        let policy = StaticAllowlist::new(["LIB_RAW_CFG03"]);
        let block_size = 262_144;
        let mut malformed_verified = mode_sense_response(block_size, false);
        malformed_verified[3] = 0;
        let (factory, log) = multi_recording_factory(vec![
            (
                PathBuf::from("/dev/sg-mock"),
                vec![changer_inquiry_response(), vpd80_response("LIB_RAW_CFG03")],
            ),
            (
                PathBuf::from("/dev/sg-drive-mock"),
                vec![
                    lto9_inquiry_response(),
                    vpd80_response("DRV_A"),
                    rbl_response(0x80_0000, 1),
                    mode_sense_response(0, false),
                    rbl_response(0x80_0000, 1),
                    malformed_verified,
                ],
            ),
        ]);
        let mut handle = lib.open_with(&policy, factory).expect("library opens");

        let err = {
            let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
            let mut raw = DriveHandleRawSink::new(&mut drive);
            raw.configure_parity_write_session(block_size)
                .expect_err("malformed read-back must fail closed")
        };

        assert!(matches!(err, ParityError::DriveCompressionModeUnknown));
        assert!(
            log.borrow().iter().any(|cdb| cdb[0] == 0x15),
            "read-back failure happens after the configuration attempt"
        );
        assert!(!handle.is_dirty());
    }

    #[test]
    fn drive_handle_raw_sink_filemark_uses_count_one_immed_clear_cdb() {
        let lib = open_drive_test_lib("LIB_RAW_WF01");
        let policy = StaticAllowlist::new(["LIB_RAW_WF01"]);
        let (factory, log) = multi_recording_factory(vec![
            (
                PathBuf::from("/dev/sg-mock"),
                vec![changer_inquiry_response(), vpd80_response("LIB_RAW_WF01")],
            ),
            (
                PathBuf::from("/dev/sg-drive-mock"),
                vec![
                    lto9_inquiry_response(),
                    vpd80_response("DRV_A"),
                    read_position_long_response(0, 0, 7_000),
                ],
            ),
        ]);
        let mut handle = lib.open_with(&policy, factory).expect("library opens");

        let outcome = {
            let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
            let mut raw = DriveHandleRawSink::new(&mut drive);
            raw.write_filemarks(1, false)
                .expect("raw sync filemark succeeds")
        };

        assert_eq!(outcome.position_after(), PhysicalPositionHint::new(7_000));
        assert!(!outcome.early_warning());
        assert!(!outcome.end_of_medium());

        let write_filemarks_cdbs: Vec<Vec<u8>> = log
            .borrow()
            .iter()
            .filter(|cdb| cdb[0] == 0x10)
            .cloned()
            .collect();
        assert_eq!(
            write_filemarks_cdbs.len(),
            1,
            "expected exactly one WRITE FILEMARKS CDB: {write_filemarks_cdbs:?}"
        );
        assert_eq!(
            write_filemarks_cdbs[0].as_slice(),
            &[0x10, 0x00, 0x00, 0x00, 0x01, 0x00],
            "RawTapeSink filemark barrier must keep IMMED clear and write exactly one mark"
        );
    }

    #[test]
    fn drive_handle_raw_sink_seeds_position_once_for_sequential_block_writes() {
        let lib = open_drive_test_lib("LIB_RAW_WR01");
        let policy = StaticAllowlist::new(["LIB_RAW_WR01"]);
        let (factory, log) = multi_recording_factory(vec![
            (
                PathBuf::from("/dev/sg-mock"),
                vec![changer_inquiry_response(), vpd80_response("LIB_RAW_WR01")],
            ),
            (
                PathBuf::from("/dev/sg-drive-mock"),
                vec![
                    lto9_inquiry_response(),
                    vpd80_response("DRV_A"),
                    read_position_long_response(0, 0, 10),
                ],
            ),
        ]);
        let mut handle = lib.open_with(&policy, factory).expect("library opens");

        let (first, second) = {
            let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
            let mut raw = DriveHandleRawSink::new(&mut drive);
            let first = raw
                .write_fixed_block(&[0xAA; 1024])
                .expect("first raw block succeeds");
            let second = raw
                .write_fixed_block(&[0xBB; 1024])
                .expect("second raw block succeeds");
            (first, second)
        };

        assert_eq!(first.position_after(), PhysicalPositionHint::new(11));
        assert_eq!(second.position_after(), PhysicalPositionHint::new(12));

        let log = log.borrow();
        let read_position_count = log.iter().filter(|cdb| cdb[0] == 0x34).count();
        let write_count = log.iter().filter(|cdb| cdb[0] == 0x0A).count();
        assert_eq!(
            read_position_count, 1,
            "raw fixed-block writes should seed position once, not after every block"
        );
        assert_eq!(write_count, 2);
        assert!(!handle.is_dirty());
    }

    #[test]
    fn drive_handle_raw_source_seeds_position_once_for_sequential_block_reads() {
        let lib = open_drive_test_lib("LIB_RAW_RD01");
        let policy = StaticAllowlist::new(["LIB_RAW_RD01"]);
        let (factory, log) = multi_recording_factory(vec![
            (
                PathBuf::from("/dev/sg-mock"),
                vec![changer_inquiry_response(), vpd80_response("LIB_RAW_RD01")],
            ),
            (
                PathBuf::from("/dev/sg-drive-mock"),
                vec![
                    lto9_inquiry_response(),
                    vpd80_response("DRV_A"),
                    read_position_long_response(0, 0, 20),
                    vec![0x11; 1024],
                    vec![0x22; 1024],
                ],
            ),
        ]);
        let mut handle = lib.open_with(&policy, factory).expect("library opens");

        let (first, second) = {
            let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
            let mut raw = DriveHandleRawSource::new(&mut drive);
            let mut buf = vec![0u8; 1024];
            let first = raw.read_record(&mut buf).expect("first raw block succeeds");
            assert_eq!(&buf[..4], &[0x11; 4]);
            let second = raw
                .read_record(&mut buf)
                .expect("second raw block succeeds");
            assert_eq!(&buf[..4], &[0x22; 4]);
            (first, second)
        };

        assert_eq!(first.position_after(), PhysicalPositionHint::new(21));
        assert_eq!(second.position_after(), PhysicalPositionHint::new(22));

        let log = log.borrow();
        let read_position_count = log.iter().filter(|cdb| cdb[0] == 0x34).count();
        let read_count = log.iter().filter(|cdb| cdb[0] == 0x08).count();
        assert_eq!(
            read_position_count, 1,
            "raw fixed-block reads should seed position once, not after every block"
        );
        assert_eq!(read_count, 2);
        assert!(!handle.is_dirty());
    }

    #[derive(Default)]
    struct DurabilityMockRawTapeSink {
        position: u64,
        pending_file_started: bool,
        barrier_ready: bool,
        committed_positions: Vec<PhysicalPositionHint>,
    }

    impl DurabilityMockRawTapeSink {
        fn commit_catalog_row(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            if !self.barrier_ready {
                return Err(ParityError::Invariant(
                    "catalog commit attempted before synchronous filemark barrier",
                ));
            }
            let position = PhysicalPositionHint::new(self.position);
            self.committed_positions.push(position);
            self.pending_file_started = false;
            self.barrier_ready = false;
            Ok(position)
        }
    }

    impl RawTapeSink for DurabilityMockRawTapeSink {
        fn write_fixed_block(&mut self, _buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
            self.position += 1;
            self.pending_file_started = true;
            self.barrier_ready = false;
            Ok(RawWriteOutcome::WroteBlock {
                position_after: PhysicalPositionHint::new(self.position),
                early_warning: false,
                end_of_medium: false,
            })
        }

        fn write_filemarks(
            &mut self,
            _count: u32,
            _immed: bool,
        ) -> Result<RawWriteOutcome, ParityError> {
            if !self.pending_file_started {
                return Err(ParityError::Invariant(
                    "filemark barrier requested before a tape file was written",
                ));
            }
            self.position += 1;
            self.barrier_ready = true;
            Ok(RawWriteOutcome::WroteFilemark {
                position_after: PhysicalPositionHint::new(self.position),
                early_warning: false,
                end_of_medium: false,
            })
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            Ok(PhysicalPositionHint::new(self.position))
        }
    }

    #[test]
    fn catalog_commit_gate_requires_synchronous_filemark_barrier() {
        let mut raw = DurabilityMockRawTapeSink::default();
        raw.write_fixed_block(&[0xCD; 4]).expect("block write");

        let err = raw
            .commit_catalog_row()
            .expect_err("catalog commit before filemark must fail");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("synchronous filemark barrier"));
            }
            other => panic!("expected invariant error, got {other:?}"),
        }

        let barrier = raw.write_filemarks(1, false).expect("sync filemark");
        assert_eq!(barrier.position_after(), PhysicalPositionHint::new(2));
        let committed = raw
            .commit_catalog_row()
            .expect("catalog commit after barrier succeeds");
        assert_eq!(committed, PhysicalPositionHint::new(2));
        assert_eq!(raw.committed_positions, vec![PhysicalPositionHint::new(2)]);
    }

    #[test]
    fn block_source_adapter_spaces_filemarks_through_legacy_source() {
        let mut source = VecBlockSource::new(vec![vec![0u8]; 3]);

        let outcome = {
            let mut raw = BlockSourceRawTapeSource::new(&mut source);
            raw.space_filemarks(5).expect("space filemarks")
        };

        assert_eq!(
            outcome,
            SpaceFilemarksOutcome {
                filemarks_spaced: 3,
                position_after: PhysicalPositionHint::new(3),
                hit_end_of_data: true,
            }
        );
        assert_eq!(source.cursor(), 3);
        assert_eq!(
            source.calls,
            vec![VecBlockSourceCall::Space {
                count: 5,
                kind: SpaceKind::Filemarks,
            }]
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn fixed_sense(byte2: u8, asc: u8, ascq: u8) -> Vec<u8> {
        let mut sense = vec![0u8; 18];
        sense[0] = 0x80 | 0x70;
        sense[2] = byte2;
        sense[12] = asc;
        sense[13] = ascq;
        sense
    }

    fn check_condition(sense: Vec<u8>) -> TapeIoError {
        TapeIoError::CheckCondition(remanence_library::scsi::ScsiError::CheckCondition {
            sense,
            bytes_transferred: 0,
        })
    }

    #[test]
    fn classify_read_filemark_boundary() {
        let err = check_condition(fixed_sense(0x80, 0x00, 0x01));
        assert_eq!(
            classify_read_boundary(&err),
            Some(RawReadBoundary::Filemark)
        );
    }

    #[test]
    fn classify_read_end_of_data_boundary() {
        let err = check_condition(fixed_sense(0x08, 0x00, 0x05));
        assert_eq!(
            classify_read_boundary(&err),
            Some(RawReadBoundary::EndOfData)
        );
    }

    #[test]
    fn classify_read_boundary_rejects_medium_error() {
        let err = check_condition(fixed_sense(0x03, 0x11, 0x00));
        assert_eq!(classify_read_boundary(&err), None);
    }
}
