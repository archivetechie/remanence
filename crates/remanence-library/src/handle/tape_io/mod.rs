//! Layer 3a — tape I/O methods on [`DriveHandle`](super::DriveHandle).
//!
//! See `docs/layer3a-design.md` for the full design. This module is
//! a **child** of `handle` (not a sibling), which lets it see the
//! private fields of `LibraryHandle` and `DriveHandle` — necessary
//! for the data-path methods (`rewind`, `locate`, `space`, `position`,
//! `read_block`, `write_block`, `write_filemarks`, `read_config`,
//! `write_config`) to issue CDBs through `DriveHandle::transport`,
//! emit audit events on the parent's hook, and flip the parent's
//! dirty bit via `DriveHandle`'s shared state on transport errors.
//!
//! Method bodies arrive incrementally across Step 9.4–9.7. Step 9.4
//! lands [`super::DriveHandle::rewind`] and
//! [`super::DriveHandle::position`] — the simplest pair (one no-data
//! CDB + one in-data CDB) that exercises the new transport plumbing
//! end-to-end. Subsequent steps add
//! `locate`/`space`/`read_block`/`write_block`/etc.

pub mod model;

use std::time::{Duration, Instant, SystemTime};

use remanence_scsi::{decode_sense, log_sense::TapeAlerts, ScsiError};
use thiserror::Error;

use super::{fire_audit, lock_drive_shared, DirtyCause};
use crate::error::{AuditEvent, AuditOp, AuditOutcome};
use crate::transport::TimeoutClass;

pub use model::{
    BlockSize, SpaceKind, SpaceResult, TapeConfig, TapePosition, WormMediaState,
    WriteFilemarksOutcome, WriteOutcome, WriteUnpositionedOutcome,
};

/// Errors a Layer 3a tape I/O operation can return. Preserves Layer
/// 2b's dirty-state vocabulary: `Transport` always marks the parent
/// `LibraryHandle` dirty with cause `CompletionUnknown` (via
/// `DriveHandle`'s shared dirty state); `CheckCondition` leaves it
/// clean (physical state is known).
#[derive(Debug, Error)]
pub enum TapeIoError {
    /// The drive returned CHECK CONDITION with sense data we could
    /// parse. Physical state is known; this is **not** a dirty
    /// signal. Caller should consult the sense bytes.
    #[error("drive rejected the command: {0}")]
    CheckCondition(ScsiError),

    /// The drive returned a target status such as BUSY or RESERVATION
    /// CONFLICT without CHECK CONDITION. The target did not accept the
    /// command, so this is not a completion-unknown dirty signal.
    #[error("drive returned target status: {0}")]
    UnexpectedStatus(ScsiError),

    /// The caller supplied a value Layer 3a rejects before issuing any
    /// CDB. No command reached the target, so this is not a CHECK
    /// CONDITION and does not imply device state changed.
    #[error("invalid tape I/O request: {0}")]
    InvalidRequest(ScsiError),

    /// SG_IO transport-level failure (timeout, kernel I/O error,
    /// driver-level disconnect). Completion is **unknown** —
    /// snapshot is marked dirty with cause `CompletionUnknown`.
    /// Caller should refresh / rescan before re-issuing.
    #[error("transport error (completion unknown): {0}")]
    Transport(ScsiError),

    /// MODE SENSE / MODE SELECT returned bytes we couldn't parse —
    /// the drive is reporting a mode page we don't recognise, or a
    /// malformed page. Distinct from `CheckCondition` because the
    /// CDB succeeded; the payload was wrong.
    #[error("malformed MODE response: {0}")]
    MalformedModeResponse(String),

    /// A non-MODE SCSI response parser found malformed bytes after a
    /// successful command. This is distinct from CHECK CONDITION: the
    /// target did not reject the command, but the returned payload was
    /// unusable.
    #[error("malformed SCSI response: {0}")]
    MalformedResponse(ScsiError),

    /// A tape-operation adapter failed above the SCSI command layer and
    /// needs to preserve an owned diagnostic string. This variant carries no
    /// sense data and is not a completion-unknown transport signal; true
    /// transport failures must use [`Self::Transport`].
    #[error("tape operation failed: {0}")]
    OperationFailed(String),

    /// WRITE buffer exceeded the drive's per-block limit. Surfaced
    /// from sense key 5 / ASC `0x24` / ASCQ `0x00` (INVALID FIELD
    /// IN CDB; IBM LTO SCSI Reference Annex B Table B.6) with the
    /// drive's `MAXIMUM BLOCK LENGTH LIMIT` value (see READ BLOCK
    /// LIMITS, §5.2.17.1 / Table 78). Caller should chunk smaller.
    #[error("write block exceeds drive limit: {requested} > {limit}")]
    BlockTooLarge {
        /// Buffer length the caller passed.
        requested: u32,
        /// Drive's per-block maximum. Sourced from READ BLOCK
        /// LIMITS at session open (`read_config()`); the
        /// `BlockTooLarge` error is synthesised by Layer 3a after
        /// observing INVALID FIELD IN CDB on the WRITE.
        limit: u32,
    },

    /// READ buffer was too small for the block on tape. The drive
    /// **consumed the block** (the head advanced past it) and set
    /// ILI in sense data. Per IBM LTO SCSI Reference §4.12.1 /
    /// Table 17, in variable-block mode the sense INFORMATION
    /// field carries `requested_length - actual_length` in two's-
    /// complement form (negative when the on-tape block was
    /// larger than the host buffer — the ILI-causing case for
    /// rem). Layer 3a's `read_block` computes
    /// `actual = provided as i64 - signed_information` and
    /// stuffs that into `actual` below. Caller must
    /// `space(-1, Blocks)` to back up before retrying with a
    /// larger buffer.
    #[error("read buffer too small for block: needed {actual}, got {provided}")]
    ReadBufferTooSmall {
        /// On-tape block size in bytes, decoded from the sense
        /// INFORMATION field via the formula above.
        actual: u32,
        /// Buffer length the caller supplied.
        provided: u32,
    },

    /// READ encountered and consumed a filemark boundary instead of
    /// returning data. The tape head is positioned after the filemark;
    /// callers that need data from the preceding tape file must space
    /// backward over the filemark before retrying.
    #[error("read encountered filemark")]
    FilemarkEncountered,

    /// The drive is not loaded with a tape. SCSI returns NOT READY;
    /// rem maps it to a distinct variant because the recovery action
    /// ("call Layer 2b's `load()`") is different from any other
    /// CHECK CONDITION.
    #[error("drive has no medium loaded")]
    NoMedium,

    /// Cartridge is write-protected (the tape's physical switch).
    #[error("medium is write-protected")]
    WriteProtected,

    /// Sense key `0x0D` — DATA PROTECT — drive refused write for a
    /// reason other than the WP switch (encryption mismatch, WORM
    /// violation, etc.). Caller reads sense for specifics.
    #[error("data protect: {0}")]
    DataProtect(ScsiError),
}

impl TapeIoError {
    /// True iff this error is "completion unknown" in the sense of
    /// `DirtyCause::CompletionUnknown`. Layer 3a method bodies use
    /// this to decide whether to flip the parent `LibraryHandle`'s
    /// shared dirty state. Only [`Self::Transport`] qualifies —
    /// `CheckCondition` and friends leave the snapshot clean because
    /// physical state is known.
    pub fn is_completion_unknown(&self) -> bool {
        matches!(self, Self::Transport(_))
    }
}

/// Convert a low-level [`ScsiError`] into a Layer 3a [`TapeIoError`].
/// Sense bytes are decoded to surface `NoMedium` / `WriteProtected` /
/// `DataProtect` as their own variants — everything else falls
/// through to [`TapeIoError::CheckCondition`],
/// [`TapeIoError::UnexpectedStatus`], or [`TapeIoError::Transport`].
pub(crate) fn map_scsi(err: ScsiError) -> TapeIoError {
    match err {
        #[cfg(target_os = "linux")]
        ScsiError::CheckCondition { ref sense, .. } => match decode_sense_key_asc(sense) {
            // NOT READY / MEDIUM NOT PRESENT — drive is loaded with no
            // tape. IBM LTO SCSI Reference Annex B Table B.3.
            Some((0x02, 0x3A, _)) => TapeIoError::NoMedium,
            // DATA PROTECT / WRITE PROTECTED — the cartridge's
            // physical write-protect switch is on. Annex B Table B.6.
            Some((0x07, 0x27, _)) => TapeIoError::WriteProtected,
            // DATA PROTECT — any other reason (encryption mismatch,
            // WORM violation, etc.).
            Some((0x07, _, _)) => TapeIoError::DataProtect(err),
            _ => TapeIoError::CheckCondition(err),
        },
        #[cfg(target_os = "linux")]
        ScsiError::UnexpectedStatus { .. } => TapeIoError::UnexpectedStatus(err),
        #[cfg(target_os = "linux")]
        ScsiError::TransportError { .. } => TapeIoError::Transport(err),
        #[cfg(target_os = "linux")]
        ScsiError::Io(_) => TapeIoError::Transport(err),
        ScsiError::Truncated { .. } | ScsiError::InvalidResponse { .. } => {
            TapeIoError::MalformedResponse(err)
        }
        ScsiError::InvalidInput(_) => TapeIoError::InvalidRequest(err),
    }
}

/// Extract sense key + ASC + ASCQ from fixed-format or descriptor-format
/// SCSI sense.
fn decode_sense_key_asc(sense: &[u8]) -> Option<(u8, u8, u8)> {
    decode_sense(sense).map(|decoded| (decoded.key, decoded.asc, decoded.ascq))
}

/// Build an [`AuditOutcome::ScsiError`] from a [`TapeIoError`],
/// pulling sense bytes out of the inner [`ScsiError`] when the
/// variant carries them. Mirrors `super::scsi_outcome` for the
/// Layer 3a error shape.
fn tape_outcome(err: &TapeIoError, dirty: bool) -> AuditOutcome {
    let sense = match err {
        TapeIoError::CheckCondition(inner)
        | TapeIoError::Transport(inner)
        | TapeIoError::DataProtect(inner) => extract_sense(inner),
        _ => None,
    };
    AuditOutcome::ScsiError {
        sense,
        summary: err.to_string(),
        dirty,
    }
}

fn extract_sense(err: &ScsiError) -> Option<Vec<u8>> {
    match err {
        #[cfg(target_os = "linux")]
        ScsiError::CheckCondition { sense, .. } => Some(sense.clone()),
        #[cfg(target_os = "linux")]
        ScsiError::TransportError { sense, .. } => Some(sense.clone()),
        _ => None,
    }
}

/// Parse the 32-byte READ POSITION long-form (service action 6)
/// response into a [`TapePosition`]. Per IBM LTO SCSI Reference
/// §5.2.22.3 / Table 99:
///
/// - Byte 0 flags: bit 7 `BOP`, bit 6 `EOP`, bit 0 `BPEW`
/// - Byte 1: reserved
/// - Bytes 4..8: Partition number (4-byte big-endian)
/// - Bytes 8..16: First Logical Object Position on Partition
///   (current LBA, big-endian u64)
///
/// Codex review 19:57 (idref=403679f2) caught the earlier byte-1
/// partition parse — the spec puts partition at bytes 4..8.
///
/// Short responses surface as [`TapeIoError::MalformedResponse`]:
/// the CDB completed without CHECK CONDITION, but the returned bytes
/// were unusable.
fn parse_read_position_long(buf: &[u8]) -> Result<TapePosition, TapeIoError> {
    const RPL_RESPONSE_LEN: usize = 32;
    const BOP_BIT: u8 = 1 << 7;
    const EOP_BIT: u8 = 1 << 6;
    const BPEW_BIT: u8 = 1 << 0;

    if buf.len() < RPL_RESPONSE_LEN {
        return Err(TapeIoError::MalformedResponse(ScsiError::Truncated {
            got: buf.len(),
            need: RPL_RESPONSE_LEN,
        }));
    }
    let flags = buf[0];
    let partition_bytes: [u8; 4] = buf[4..8]
        .try_into()
        .expect("32-byte slice contains 4 bytes at offset 4");
    let partition = u32::from_be_bytes(partition_bytes);
    let lba_bytes: [u8; 8] = buf[8..16]
        .try_into()
        .expect("32-byte slice contains 8 bytes at offset 8");
    let lba = u64::from_be_bytes(lba_bytes);

    Ok(TapePosition {
        lba,
        partition,
        beginning_of_partition: (flags & BOP_BIT) != 0,
        end_of_partition: (flags & EOP_BIT) != 0,
        block_position_end_of_warning: (flags & BPEW_BIT) != 0,
    })
}

impl super::DriveHandle {
    fn fire_tape_started(&self, operation: AuditOp, cdb: &[u8]) {
        fire_audit(
            &mut lock_drive_shared(&self.shared).audit_hook,
            &AuditEvent::Started {
                library_serial: &self.library_serial,
                operation,
                cdb,
                at: SystemTime::now(),
            },
        );
    }

    fn finish_tape_success(&self, operation: AuditOp, duration: Duration) {
        fire_audit(
            &mut lock_drive_shared(&self.shared).audit_hook,
            &AuditEvent::Finished {
                library_serial: &self.library_serial,
                operation,
                outcome: AuditOutcome::Success {
                    duration,
                    snapshot_patched: false,
                    dirty: false,
                },
                at: SystemTime::now(),
            },
        );
    }

    fn finish_tape_error(&mut self, operation: AuditOp, err: &TapeIoError) {
        let dirty = err.is_completion_unknown();
        if dirty {
            self.mark_position_unknown();
            lock_drive_shared(&self.shared)
                .dirty
                .mark(DirtyCause::CompletionUnknown);
        }
        let outcome = tape_outcome(err, dirty);
        fire_audit(
            &mut lock_drive_shared(&self.shared).audit_hook,
            &AuditEvent::Finished {
                library_serial: &self.library_serial,
                operation,
                outcome,
                at: SystemTime::now(),
            },
        );
    }

    /// Issue SSC `REWIND` (CDB `0x01`). The drive moves to BOT on
    /// partition 0; IMMED is **not** set so this call returns only
    /// after the head reaches BOT. Transport errors mark the parent
    /// `LibraryHandle` dirty with cause
    /// [`DirtyCause::CompletionUnknown`]; CHECK CONDITION (including
    /// the [`TapeIoError::NoMedium`] mapping) leaves the snapshot
    /// clean because the physical state is known.
    pub fn rewind(&mut self) -> Result<(), TapeIoError> {
        let op = AuditOp::TapeRewind {
            bay: self.bay_address,
        };
        let cdb = remanence_scsi::rewind::build_cdb();

        self.fire_tape_started(op, &cdb);

        self.transport.set_timeout_for(TimeoutClass::Rewind);
        let started = Instant::now();
        match self.transport.execute_none(&cdb) {
            Ok(()) => {
                self.position_known = true;
                self.finish_tape_success(op, started.elapsed());
                Ok(())
            }
            Err(e) => {
                let mapped = map_scsi(e);
                self.finish_tape_error(op, &mapped);
                Err(mapped)
            }
        }
    }

    /// Issue READ POSITION long-form (service action 6, CDB `0x34`).
    /// Returns the drive's current tape position including BPEW. The
    /// long form fits the full 64-bit LBA — Layer 3a does not expose
    /// the short form. Read-only: never marks the snapshot dirty on
    /// success; transport errors do, same as any other Layer 3a
    /// method.
    pub fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        let op = AuditOp::TapeReadPosition {
            bay: self.bay_address,
        };
        let cdb = remanence_scsi::read_position::build_cdb_long();

        self.fire_tape_started(op, &cdb);

        let started = Instant::now();
        match self.read_position_inline_with_cdb(&cdb) {
            Ok(pos) => {
                self.finish_tape_success(op, started.elapsed());
                Ok(pos)
            }
            Err(mapped) => {
                self.finish_tape_error(op, &mapped);
                Err(mapped)
            }
        }
    }

    /// Query the drive's current TapeAlert flags via LOG SENSE page 0x2E.
    ///
    /// This is a read-only status command. Malformed LOG SENSE payloads surface
    /// as [`TapeIoError::MalformedResponse`] so callers can distinguish a
    /// successful CDB with unusable medium-sourced bytes from CHECK CONDITION or
    /// transport failure.
    pub fn read_tape_alerts(&mut self) -> Result<TapeAlerts, TapeIoError> {
        let op = AuditOp::TapeReadAlerts {
            bay: self.bay_address,
        };
        let cdb = remanence_scsi::log_sense::build_tape_alert_cdb(
            remanence_scsi::log_sense::TAPE_ALERT_RESPONSE_LEN,
        );
        self.fire_tape_started(op, &cdb);

        let started = Instant::now();
        self.transport.set_timeout_for(TimeoutClass::TapeStatus);
        let mut buf = [0u8; remanence_scsi::log_sense::TAPE_ALERT_RESPONSE_LEN as usize];
        match self.transport.execute_in(&cdb, &mut buf) {
            Ok(outcome) => {
                let bytes = (outcome.bytes_transferred as usize).min(buf.len());
                match remanence_scsi::log_sense::parse_response(&buf[..bytes]) {
                    Ok(alerts) => {
                        self.finish_tape_success(op, started.elapsed());
                        Ok(alerts)
                    }
                    Err(err) => {
                        let err = TapeIoError::MalformedResponse(err);
                        self.finish_tape_error(op, &err);
                        Err(err)
                    }
                }
            }
            Err(e) => {
                let mapped = map_scsi(e);
                self.finish_tape_error(op, &mapped);
                Err(mapped)
            }
        }
    }

    /// Issue LOCATE(16) (CDB `0x92`) — seek to `lba` in partition 0.
    /// Heads block until the seek completes. Follows with an inline
    /// READ POSITION so the caller learns where the head actually
    /// settled without a second round-trip. Transport errors flip
    /// the parent dirty bit with cause
    /// [`DirtyCause::CompletionUnknown`].
    pub fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
        let op = AuditOp::TapeLocate {
            bay: self.bay_address,
            lba,
        };
        let cdb = remanence_scsi::locate::build_cdb(lba);

        self.fire_tape_started(op, &cdb);

        self.transport.set_timeout_for(TimeoutClass::Positioning);
        let started = Instant::now();
        let result = self.transport.execute_none(&cdb);
        match result {
            Ok(()) => match self.read_position_inline() {
                Ok(pos) => {
                    self.finish_tape_success(op, started.elapsed());
                    Ok(pos)
                }
                Err(mapped) => {
                    self.finish_tape_error(op, &mapped);
                    Err(mapped)
                }
            },
            Err(e) => {
                let mapped = map_scsi(e);
                self.finish_tape_error(op, &mapped);
                Err(mapped)
            }
        }
    }

    /// Issue SPACE (`0x11` 6-byte or `0x91` 16-byte form) for
    /// relative motion. `count` is signed (negative is backward);
    /// `kind` selects motion type. The short form is used when
    /// `count` fits in 24-bit signed (±8 388 607), the long form
    /// otherwise — this is internal to `space()`.
    ///
    /// SPACE can stop short of the requested count when it hits a
    /// file mark, EOD, BOP, or EOM. SSC-5 surfaces that as CHECK
    /// CONDITION with VALID + INFORMATION carrying the residual; it
    /// is **not** a Layer 3a error — the operation succeeded with a
    /// boundary signal. [`SpaceResult::stopped_at_boundary`] is set
    /// and `units_traversed = count - residual`.
    ///
    /// Transport errors flip the parent dirty bit with
    /// [`DirtyCause::CompletionUnknown`], same as the other Layer 3a
    /// methods.
    pub fn space(&mut self, count: i64, kind: SpaceKind) -> Result<SpaceResult, TapeIoError> {
        use remanence_scsi::space::{self as sp, SpaceCode};

        // IBM LTO drives only implement CODE 0 (Blocks), 1
        // (Filemarks), and 3 (End of Data) per the IBM LTO SCSI
        // Reference SPACE table; CODE 2 (sequential filemarks) is
        // listed as reserved. Issuing it would yield INVALID FIELD
        // IN CDB at runtime on hardware — reject at the API
        // boundary so the failure is locatable. Codex 20:00 catch.
        let scsi_code = match kind {
            SpaceKind::Blocks => SpaceCode::Blocks,
            SpaceKind::Filemarks => SpaceCode::Filemarks,
            SpaceKind::EndOfData => SpaceCode::EndOfData,
            SpaceKind::SequentialFilemarks => {
                return Err(TapeIoError::InvalidRequest(ScsiError::InvalidInput(
                    "SpaceKind::SequentialFilemarks (CODE 2) is reserved on IBM LTO; \
                     issue separate Filemarks / Blocks space() calls instead",
                )));
            }
        };

        // SPACE(EndOfData) ignores `count`. For the other kinds, pick
        // SPACE(6) when the count fits in 24-bit signed, SPACE(16)
        // otherwise. The CDB-builder layer handles encoding; we hand
        // it a reference-sized slice via the matching arm.
        let op = AuditOp::TapeSpace {
            bay: self.bay_address,
            count,
            kind,
        };

        let cdb6_buf;
        let cdb16_buf;
        let cdb: &[u8] = if matches!(kind, SpaceKind::EndOfData) {
            // EndOfData with count=0 always fits SPACE(6); save 10
            // bytes on the wire.
            cdb6_buf = sp::build_cdb_6(scsi_code, 0);
            &cdb6_buf
        } else if sp::fits_in_space6(count) {
            cdb6_buf = sp::build_cdb_6(scsi_code, count as i32);
            &cdb6_buf
        } else {
            cdb16_buf = sp::build_cdb_16(scsi_code, count);
            &cdb16_buf
        };

        self.fire_tape_started(op, cdb);

        self.transport.set_timeout_for(TimeoutClass::Positioning);
        let started = Instant::now();
        let result = self.transport.execute_none(cdb);

        // Detect "spaced to boundary" early-stop. Per SSC-5 §6.10 the
        // drive raises CHECK CONDITION (key = NO SENSE 0 or BLANK
        // CHECK 8) with VALID + INFORMATION = residual count. That's
        // a successful operation with a stop signal, not an error.
        let (units_traversed, stopped_at_boundary) = match &result {
            Ok(()) => (
                // EndOfData drove all the way; the drive doesn't
                // report distance traversed. units_traversed is 0
                // for EndOfData; for the other kinds, full count.
                if matches!(kind, SpaceKind::EndOfData) {
                    0
                } else {
                    count
                },
                false,
            ),
            Err(ScsiError::CheckCondition { sense, .. }) => {
                if let Some(residual) = space_residual_if_early_stop(sense) {
                    (units_traversed_from_space_residual(count, residual), true)
                } else {
                    // Not an early-stop; will surface via the
                    // normal error path below.
                    (0, false)
                }
            }
            _ => (0, false),
        };

        match result {
            Ok(()) => match self.read_position_inline() {
                Ok(position_after) => {
                    self.finish_tape_success(op, started.elapsed());
                    Ok(SpaceResult {
                        units_traversed,
                        stopped_at_boundary,
                        position_after,
                    })
                }
                Err(mapped) => {
                    self.finish_tape_error(op, &mapped);
                    Err(mapped)
                }
            },
            Err(e) => {
                // CHECK CONDITION with VALID+residual = early-stop
                // boundary success. Read position and return
                // SpaceResult. Otherwise treat as a normal error.
                if stopped_at_boundary {
                    match self.read_position_inline() {
                        Ok(position_after) => {
                            self.finish_tape_success(op, started.elapsed());
                            return Ok(SpaceResult {
                                units_traversed,
                                stopped_at_boundary: true,
                                position_after,
                            });
                        }
                        Err(rp_err) => {
                            // The SPACE succeeded with a boundary
                            // signal, but the inline RP failed.
                            // That's an RP-side failure; preserve
                            // the dirty-flip semantics of RP.
                            self.finish_tape_error(op, &rp_err);
                            return Err(rp_err);
                        }
                    }
                }
                let mapped = map_scsi(e);
                self.finish_tape_error(op, &mapped);
                Err(mapped)
            }
        }
    }

    /// Issue READ(6) in variable-block mode (CDB `0x08`, FIXED=0,
    /// SILI=0). The drive returns one block of up to `buf.len()`
    /// bytes; the return value is the number of bytes actually
    /// delivered.
    ///
    /// Buffer sizing per IBM LTO SCSI Reference §4.12.1 / Table 17:
    /// in variable-block mode the drive raises CHECK CONDITION with
    /// the ILI bit + VALID + INFORMATION = `requested - actual` in
    /// two's-complement form whenever on-tape block size ≠ host
    /// buffer length. `read_block` distinguishes the two ILI cases:
    ///
    /// - INFORMATION ≥ 0 (block smaller than buffer): a normal
    ///   short read. Returns `Ok(actual)` where `actual = requested
    ///   - INFORMATION`.
    /// - INFORMATION < 0 (block larger than buffer): the drive
    ///   consumed the block (the head advanced past it). Returns
    ///   [`TapeIoError::ReadBufferTooSmall { actual, provided }`]
    ///   where `actual = (requested as i64) - signed_information`.
    ///   The caller MUST `space(-1, Blocks)` to back up before
    ///   retrying with a larger buffer.
    ///
    /// Transport errors flip the parent dirty bit; CHECK CONDITION
    /// without ILI (NOT READY, etc.) maps via the usual `map_scsi`.
    pub fn read_block(&mut self, buf: &mut [u8]) -> Result<usize, TapeIoError> {
        // Drive's 24-bit READ(6) transfer-length field caps at
        // MAX_TRANSFER_LEN (16 MiB - 1). Bail before the CDB build's
        // debug_assert can fire.
        let len_u32 = if buf.len() > remanence_scsi::read_write::MAX_TRANSFER_LEN as usize {
            return Err(TapeIoError::InvalidRequest(ScsiError::InvalidInput(
                "read_block: buffer exceeds READ(6) 24-bit transfer-length max",
            )));
        } else {
            buf.len() as u32
        };

        let op = AuditOp::TapeRead {
            bay: self.bay_address,
            len: len_u32,
        };
        let cdb = remanence_scsi::read_write::build_read_variable_cdb(len_u32);

        self.fire_tape_started(op, &cdb);

        self.transport.set_timeout_for(TimeoutClass::TapeIo);
        let started = Instant::now();
        let result = self.transport.execute_in(&cdb, buf);
        match result {
            Ok(outcome) => {
                let bytes = (outcome.bytes_transferred as usize).min(buf.len());
                self.finish_tape_success(op, started.elapsed());
                Ok(bytes)
            }
            Err(e) => {
                // Variable-block ILI handling. If the drive raised
                // CHECK CONDITION with ILI + VALID, the sense's
                // INFORMATION field tells us the on-tape vs host
                // buffer size delta.
                if let ScsiError::CheckCondition { ref sense, .. } = e {
                    if read_filemark_signal(sense) {
                        let mapped = TapeIoError::FilemarkEncountered;
                        self.finish_tape_error(op, &mapped);
                        return Err(mapped);
                    }
                    if let Some(signed_info) = ili_signed_information(sense) {
                        if signed_info >= 0 {
                            // Block smaller than buffer — normal short
                            // read. Use the sense INFORMATION field as the
                            // spec source, then clamp to the host buffer so
                            // malformed sense cannot report stale bytes.
                            let actual_usize = ((len_u32 as i64) - signed_info).max(0) as usize;
                            let actual_usize = actual_usize.min(buf.len());
                            self.finish_tape_success(op, started.elapsed());
                            return Ok(actual_usize);
                        } else {
                            // Block larger than buffer — drive
                            // consumed it; caller must space(-1)
                            // before retrying.
                            let actual_i64 = (len_u32 as i64) - signed_info;
                            let actual_u32 = actual_i64.clamp(0, u32::MAX as i64) as u32;
                            let mapped = TapeIoError::ReadBufferTooSmall {
                                actual: actual_u32,
                                provided: len_u32,
                            };
                            self.finish_tape_error(op, &mapped);
                            return Err(mapped);
                        }
                    }
                }
                let mapped = map_scsi(e);
                self.finish_tape_error(op, &mapped);
                Err(mapped)
            }
        }
    }

    /// Issue WRITE(6) in variable-block mode (CDB `0x0A`, FIXED=0).
    /// The drive writes exactly `buf.len()` bytes as one logical
    /// block. Returns a [`WriteOutcome`] carrying the bytes
    /// actually committed plus the post-write position (via inline
    /// READ POSITION), so the caller can record the LBA of the
    /// block it just wrote without a second round-trip.
    ///
    /// Near-EOM behaviour per IBM LTO SCSI Reference: when the
    /// drive crosses the early-warning point it raises CHECK
    /// CONDITION with NO SENSE (sense key 0), the EOM bit set,
    /// and VALID with INFORMATION carrying the residual. The
    /// block has been written; the drive is signalling that
    /// tape is filling up. `write_block` surfaces this as
    /// `Ok(WriteOutcome)` with `early_warning = true` rather
    /// than an error. Sense key 0x0D (VOLUME OVERFLOW)
    /// escalates to `end_of_medium = true`, meaning further
    /// writes will fail.
    ///
    /// Hard transport errors flip the parent dirty bit with cause
    /// [`DirtyCause::CompletionUnknown`]; CHECK CONDITION mappings
    /// (WriteProtected, DataProtect, NoMedium, BlockTooLarge) leave
    /// the snapshot clean.
    pub fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        self.ensure_position_known_for_write()?;
        let len_u32 = if buf.len() > remanence_scsi::read_write::MAX_TRANSFER_LEN as usize {
            return Err(TapeIoError::InvalidRequest(ScsiError::InvalidInput(
                "write_block: buffer exceeds WRITE(6) 24-bit transfer-length max",
            )));
        } else {
            buf.len() as u32
        };

        let op = AuditOp::TapeWrite {
            bay: self.bay_address,
            len: len_u32,
        };
        let cdb = remanence_scsi::read_write::build_write_variable_cdb(len_u32);

        self.fire_tape_started(op, &cdb);

        self.transport.set_timeout_for(TimeoutClass::TapeIo);
        let started = Instant::now();
        let result = self.transport.execute_out(&cdb, buf);

        match result {
            Ok(outcome) => {
                let bytes_written = outcome.bytes_transferred.min(len_u32);
                // Inline READ POSITION so the caller learns the LBA
                // of the block it just wrote.
                match self.read_position_inline() {
                    Ok(position_after) => {
                        self.finish_tape_success(op, started.elapsed());
                        Ok(WriteOutcome {
                            bytes_written,
                            early_warning: false,
                            end_of_medium: false,
                            position_after,
                        })
                    }
                    Err(rp_err) => {
                        self.finish_tape_error(op, &rp_err);
                        Err(rp_err)
                    }
                }
            }
            Err(ScsiError::CheckCondition {
                sense,
                bytes_transferred,
            }) => {
                // Near-EOM informational CHECK CONDITION. If the
                // drive set EOM/VALID + a "soft" key, this is
                // success-with-EW (and possibly hard EOM via
                // VOLUME OVERFLOW). Otherwise reconstruct the
                // CheckCondition and map normally.
                if let Some(signal) = write_eom_signal(&sense) {
                    let bytes_written = bytes_transferred.min(len_u32);
                    match self.read_position_inline() {
                        Ok(position_after) => {
                            self.finish_tape_success(op, started.elapsed());
                            return Ok(WriteOutcome {
                                bytes_written,
                                early_warning: signal.early_warning,
                                end_of_medium: signal.end_of_medium,
                                position_after,
                            });
                        }
                        Err(rp_err) => {
                            self.finish_tape_error(op, &rp_err);
                            return Err(rp_err);
                        }
                    }
                }
                // INVALID FIELD IN CDB on WRITE means the drive's
                // per-block limit was exceeded. If read_config()
                // has already learned that limit from READ BLOCK
                // LIMITS, surface the purpose-built variant.
                let key_asc = decode_sense_key_asc(&sense);
                let mapped = if matches!(key_asc, Some((0x05, 0x24, 0x00))) {
                    match self.max_write_block_size_bytes {
                        Some(limit) => TapeIoError::BlockTooLarge {
                            requested: len_u32,
                            limit,
                        },
                        None => TapeIoError::CheckCondition(ScsiError::CheckCondition {
                            sense,
                            bytes_transferred,
                        }),
                    }
                } else {
                    map_scsi(ScsiError::CheckCondition {
                        sense,
                        bytes_transferred,
                    })
                };
                self.finish_tape_error(op, &mapped);
                Err(mapped)
            }
            Err(e) => {
                let mapped = map_scsi(e);
                self.finish_tape_error(op, &mapped);
                Err(mapped)
            }
        }
    }

    /// Issue WRITE(6) in variable-block mode without an inline READ
    /// POSITION on clean success. This is for higher layers that
    /// already maintain a sequential physical cursor and need to avoid
    /// doubling the CDB count on fixed-block streaming paths. EW/EOM
    /// handling and all error mapping matches [`Self::write_block`].
    pub fn write_block_unpositioned(
        &mut self,
        buf: &[u8],
    ) -> Result<WriteUnpositionedOutcome, TapeIoError> {
        self.ensure_position_known_for_write()?;
        let len_u32 = if buf.len() > remanence_scsi::read_write::MAX_TRANSFER_LEN as usize {
            return Err(TapeIoError::InvalidRequest(ScsiError::InvalidInput(
                "write_block_unpositioned: buffer exceeds WRITE(6) 24-bit transfer-length max",
            )));
        } else {
            buf.len() as u32
        };

        let op = AuditOp::TapeWrite {
            bay: self.bay_address,
            len: len_u32,
        };
        let cdb = remanence_scsi::read_write::build_write_variable_cdb(len_u32);

        self.fire_tape_started(op, &cdb);

        self.transport.set_timeout_for(TimeoutClass::TapeIo);
        let started = Instant::now();
        let result = self.transport.execute_out(&cdb, buf);

        match result {
            Ok(outcome) => {
                let bytes_written = outcome.bytes_transferred.min(len_u32);
                self.finish_tape_success(op, started.elapsed());
                Ok(WriteUnpositionedOutcome {
                    bytes_written,
                    early_warning: false,
                    end_of_medium: false,
                })
            }
            Err(ScsiError::CheckCondition {
                sense,
                bytes_transferred,
            }) => {
                if let Some(signal) = write_eom_signal(&sense) {
                    let bytes_written = bytes_transferred.min(len_u32);
                    self.finish_tape_success(op, started.elapsed());
                    return Ok(WriteUnpositionedOutcome {
                        bytes_written,
                        early_warning: signal.early_warning,
                        end_of_medium: signal.end_of_medium,
                    });
                }

                let key_asc = decode_sense_key_asc(&sense);
                let mapped = if matches!(key_asc, Some((0x05, 0x24, 0x00))) {
                    match self.max_write_block_size_bytes {
                        Some(limit) => TapeIoError::BlockTooLarge {
                            requested: len_u32,
                            limit,
                        },
                        None => TapeIoError::CheckCondition(ScsiError::CheckCondition {
                            sense,
                            bytes_transferred,
                        }),
                    }
                } else {
                    map_scsi(ScsiError::CheckCondition {
                        sense,
                        bytes_transferred,
                    })
                };
                self.finish_tape_error(op, &mapped);
                Err(mapped)
            }
            Err(e) => {
                let mapped = map_scsi(e);
                self.finish_tape_error(op, &mapped);
                Err(mapped)
            }
        }
    }

    /// Issue WRITE FILEMARKS(6) (CDB `0x10`) with the given count.
    /// IMMED is **not** set — the call returns only after the marks
    /// are committed to media. Returns a
    /// [`WriteFilemarksOutcome`] carrying the post-mark position
    /// plus early-warning / end-of-medium flags.
    ///
    /// Near-EOM behaviour mirrors [`Self::write_block`]: per IBM
    /// LTO SCSI Reference §4.8, WRITE FILEMARKS can cross the
    /// programmable early-warning zone (PEWZ) and the drive
    /// raises CHECK CONDITION with NO SENSE + EOM bit at
    /// completion. The marks **are committed**; surfacing this as
    /// Err would lead callers to retry and double-write filemarks.
    /// `write_filemarks` therefore reuses the same EOM-signal
    /// helper as [`Self::write_block`] and returns success-with-EW;
    /// sense key
    /// `0x0D` (VOLUME OVERFLOW) escalates to `end_of_medium = true`.
    /// Codex 20:17 (idref=6e9b56d9 High) caught the earlier
    /// always-Err-on-CC behaviour.
    ///
    /// Transport errors flip the parent dirty bit with cause
    /// [`DirtyCause::CompletionUnknown`].
    pub fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.ensure_position_known_for_write()?;
        if count > remanence_scsi::write_filemarks::WRITE_FILEMARKS_6_MAX {
            return Err(TapeIoError::InvalidRequest(ScsiError::InvalidInput(
                "write_filemarks: count exceeds WRITE FILEMARKS(6) 24-bit max",
            )));
        }

        let op = AuditOp::TapeWriteFilemarks {
            bay: self.bay_address,
            count,
        };
        let cdb = remanence_scsi::write_filemarks::build_cdb_6(count);

        self.fire_tape_started(op, &cdb);

        self.transport.set_timeout_for(TimeoutClass::WriteFilemarks);
        let started = Instant::now();
        let result = self.transport.execute_none(&cdb);
        match result {
            Ok(()) => match self.read_position_inline() {
                Ok(pos) => {
                    self.finish_tape_success(op, started.elapsed());
                    Ok(WriteFilemarksOutcome {
                        early_warning: false,
                        end_of_medium: false,
                        position_after: pos,
                    })
                }
                Err(rp_err) => {
                    self.finish_tape_error(op, &rp_err);
                    Err(rp_err)
                }
            },
            Err(ScsiError::CheckCondition {
                sense,
                bytes_transferred,
            }) => {
                // Informational EW / EOM signal: the marks are
                // committed. Read position, return success.
                if let Some(signal) = write_eom_signal(&sense) {
                    match self.read_position_inline() {
                        Ok(position_after) => {
                            self.finish_tape_success(op, started.elapsed());
                            return Ok(WriteFilemarksOutcome {
                                early_warning: signal.early_warning,
                                end_of_medium: signal.end_of_medium,
                                position_after,
                            });
                        }
                        Err(rp_err) => {
                            self.finish_tape_error(op, &rp_err);
                            return Err(rp_err);
                        }
                    }
                }
                let mapped = map_scsi(ScsiError::CheckCondition {
                    sense,
                    bytes_transferred,
                });
                self.finish_tape_error(op, &mapped);
                Err(mapped)
            }
            Err(e) => {
                let mapped = map_scsi(e);
                self.finish_tape_error(op, &mapped);
                Err(mapped)
            }
        }
    }

    /// Read the drive's current block-size + compression
    /// configuration plus its reported maximum block size. Issues
    /// **two CDBs** at session open:
    ///
    /// 1. READ BLOCK LIMITS (CDB `0x05`) for
    ///    [`TapeConfig::max_block_size_bytes`]. MODE SENSE pages do
    ///    not carry the per-block size limit (codex 19:45 catch).
    /// 2. MODE SENSE(6) (CDB `0x1A`) for page `0x0F` (Data
    ///    Compression) with `DBD=0` so the response includes the
    ///    block descriptor — block size comes from the
    ///    descriptor's 3-byte BLOCK LENGTH field; compression
    ///    comes from the page's DCE bit.
    ///
    /// Malformed responses surface as
    /// [`TapeIoError::MalformedModeResponse`] (a CDB-succeeded /
    /// payload-wrong condition, distinct from CHECK CONDITION).
    pub fn read_config(&mut self) -> Result<TapeConfig, TapeIoError> {
        let op = AuditOp::TapeReadConfig {
            bay: self.bay_address,
        };
        let cdb_rbl = remanence_scsi::read_block_limits::build_cdb();
        self.fire_tape_started(op, &cdb_rbl);

        let started = Instant::now();

        // 1. READ BLOCK LIMITS.
        self.transport.set_timeout_for(TimeoutClass::TapeStatus);
        let mut rbl_buf = [0u8; 6];
        let max_block_size_bytes = match self.transport.execute_in(&cdb_rbl, &mut rbl_buf) {
            Ok(outcome) => {
                // Codex 20:22 (idref=01cf3e76 Medium): slice to
                // bytes_transferred so a short successful transfer
                // isn't accepted with fabricated trailing zeros
                // from the zero-initialised buffer — mirrors the
                // MODE SENSE path below.
                let bytes = (outcome.bytes_transferred as usize).min(rbl_buf.len());
                match remanence_scsi::read_block_limits::parse_response(&rbl_buf[..bytes]) {
                    Some(limits) => limits.max_block_length,
                    None => {
                        let err = TapeIoError::MalformedModeResponse(format!(
                            "READ BLOCK LIMITS response too short: got {bytes} bytes"
                        ));
                        self.finish_tape_error(op, &err);
                        return Err(err);
                    }
                }
            }
            Err(e) => {
                let mapped = map_scsi(e);
                self.finish_tape_error(op, &mapped);
                return Err(mapped);
            }
        };

        // 2. MODE SENSE(6) page 0x0F.
        let cdb_ms = remanence_scsi::mode::build_mode_sense6_cdb(
            remanence_scsi::mode::PageControl::Current,
            remanence_scsi::mode::PAGE_DATA_COMPRESSION,
            64,
        );
        self.transport.set_timeout_for(TimeoutClass::ModeConfig);
        let mut ms_buf = [0u8; 64];
        let mode = match self.transport.execute_in(&cdb_ms, &mut ms_buf) {
            Ok(outcome) => {
                let bytes = (outcome.bytes_transferred as usize).min(ms_buf.len());
                match parse_mode_sense_data_compression(&ms_buf[..bytes]) {
                    Ok(parsed) => parsed,
                    Err(err) => {
                        self.finish_tape_error(op, &err);
                        return Err(err);
                    }
                }
            }
            Err(e) => {
                let mapped = map_scsi(e);
                self.finish_tape_error(op, &mapped);
                return Err(mapped);
            }
        };

        self.finish_tape_success(op, started.elapsed());

        self.max_write_block_size_bytes = Some(max_block_size_bytes);

        Ok(TapeConfig {
            block_size: mode.block_size,
            compression: mode.compression,
            max_block_size_bytes,
            write_protected: mode.write_protected,
            worm: mode.worm,
        })
    }

    /// Write a new block-size + compression configuration via
    /// MODE SELECT(6) (CDB `0x15`) with `PF=1, SP=0` so the
    /// values are volatile (apply for this session only).
    /// `max_block_size_bytes` from `cfg` is ignored — the drive's
    /// own cap always wins and is returned through `read_config()`.
    pub fn write_config(&mut self, cfg: TapeConfig) -> Result<(), TapeIoError> {
        // Reject invalid block-size values BEFORE building the
        // CDB so the failure is locatable (and matches our public
        // contract — model.rs §4.2 says "size_bytes: zero is
        // invalid; multi-MiB values must fit in the drive's
        // documented maximum"). Codex 20:22 (idref=01cf3e76
        // Medium) caught the missing validation: the prior path
        // would silently truncate 0x0100_0000 to 0 (variable mode)
        // and accept Fixed{0}.
        if let BlockSize::Fixed { size_bytes } = cfg.block_size {
            if size_bytes == 0 {
                return Err(TapeIoError::InvalidRequest(ScsiError::InvalidInput(
                    "write_config: BlockSize::Fixed { size_bytes: 0 } is invalid; \
                     use BlockSize::Variable for variable-block mode",
                )));
            }
            if size_bytes > 0x00FF_FFFF {
                return Err(TapeIoError::InvalidRequest(ScsiError::InvalidInput(
                    "write_config: BlockSize::Fixed.size_bytes exceeds 24-bit \
                     block-descriptor field width (max 0x00FF_FFFF)",
                )));
            }
        }

        let op = AuditOp::TapeWriteConfig {
            bay: self.bay_address,
        };
        let param_list = build_compression_param_list(&cfg);
        let cdb = remanence_scsi::mode::build_mode_select6_cdb(
            /* pf */ true,
            /* save_pages */ false,
            param_list.len() as u8,
        );

        self.fire_tape_started(op, &cdb);

        self.transport.set_timeout_for(TimeoutClass::ModeConfig);
        let started = Instant::now();
        let result = self.transport.execute_out(&cdb, &param_list);
        match result {
            Ok(_) => {
                self.finish_tape_success(op, started.elapsed());
                Ok(())
            }
            Err(e) => {
                let mapped = map_scsi(e);
                self.finish_tape_error(op, &mapped);
                Err(mapped)
            }
        }
    }

    fn mark_position_unknown(&mut self) {
        self.position_known = false;
    }

    fn ensure_position_known_for_write(&self) -> Result<(), TapeIoError> {
        if self.position_known {
            return Ok(());
        }
        Err(TapeIoError::InvalidRequest(ScsiError::InvalidInput(
            "drive position is unknown after a completion-unknown transport error; \
             call position(), locate(), rewind(), or space() before writing",
        )))
    }

    /// Inline READ POSITION used by [`Self::locate`] and
    /// [`Self::space`] to fill `position_after`. Sets the TapeStatus
    /// timeout, issues the long-form CDB, parses the 32-byte
    /// response. Does **not** fire its own audit events — the
    /// wrapping operation's audit covers it.
    pub(crate) fn read_position_inline(&mut self) -> Result<TapePosition, TapeIoError> {
        let cdb = remanence_scsi::read_position::build_cdb_long();
        self.read_position_inline_with_cdb(&cdb)
    }

    fn read_position_inline_with_cdb(&mut self, cdb: &[u8]) -> Result<TapePosition, TapeIoError> {
        self.transport.set_timeout_for(TimeoutClass::TapeStatus);
        let mut buf = [0u8; 32];
        match self.transport.execute_in(cdb, &mut buf) {
            Ok(outcome) => {
                let bytes = (outcome.bytes_transferred as usize).min(buf.len());
                match parse_read_position_long(&buf[..bytes]) {
                    Ok(position) => {
                        self.position_known = true;
                        Ok(position)
                    }
                    Err(err) => {
                        self.mark_position_unknown();
                        Err(err)
                    }
                }
            }
            Err(e) => {
                self.mark_position_unknown();
                Err(map_scsi(e))
            }
        }
    }
}

/// Parse a MODE SENSE(6) response that asked for page `0x0F`
/// (Data Compression) with `DBD=0`. Layout per SPC-5 §6.10 + SSC:
///
/// - byte 0: Mode Data Length
/// - byte 1: Medium Type
/// - byte 2: Device-Specific Parameter (WP bit etc.)
/// - byte 3: Block Descriptor Length (BDL — typically 8 for tape)
/// - bytes 4..4+BDL: Block Descriptor; for tape:
///   - byte 4: Density Code
///   - bytes 5..8: Number of Blocks (3 BE)
///   - byte 8: Reserved
///   - bytes 9..12: BLOCK LENGTH (3 BE) — 0 = variable, else
///     fixed-block of that size
/// - bytes 4+BDL..: mode pages, starting with page 0x0F:
///   - byte 0: PS|SPF|PAGE_CODE (low 6 bits = 0x0F)
///   - byte 1: PAGE LENGTH (= 14)
///   - byte 2: DCE (bit 7) — compression-enabled
///   - ... remaining fields (algorithms etc.) ignored by Layer 3a
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModeSenseDataCompression {
    block_size: BlockSize,
    compression: bool,
    write_protected: bool,
    worm: WormMediaState,
    medium_type: u8,
}

fn parse_mode_sense_data_compression(buf: &[u8]) -> Result<ModeSenseDataCompression, TapeIoError> {
    if buf.len() < 4 {
        return Err(TapeIoError::MalformedModeResponse(format!(
            "response too short for parameter header: got {}",
            buf.len()
        )));
    }
    let medium_type = buf[1];
    let write_protected = (buf[2] & 0x80) != 0;
    let bdl = buf[3] as usize;
    if bdl != 8 {
        return Err(TapeIoError::MalformedModeResponse(format!(
            "expected 8-byte block descriptor, got BDL={bdl}"
        )));
    }
    if buf.len() < 4 + bdl {
        return Err(TapeIoError::MalformedModeResponse(format!(
            "response too short for block descriptor: got {} bytes, need {}",
            buf.len(),
            4 + bdl
        )));
    }
    // BLOCK LENGTH is a 3-byte big-endian field at offset 9..12.
    let block_length = ((buf[9] as u32) << 16) | ((buf[10] as u32) << 8) | (buf[11] as u32);
    let block_size = if block_length == 0 {
        BlockSize::Variable
    } else {
        BlockSize::Fixed {
            size_bytes: block_length,
        }
    };

    let pages_start = 4 + bdl;
    if buf.len() < pages_start + 2 {
        return Err(TapeIoError::MalformedModeResponse(format!(
            "response too short for mode page header: got {}, need {}",
            buf.len(),
            pages_start + 2
        )));
    }
    let page_code = buf[pages_start] & 0x3F;
    if page_code != remanence_scsi::mode::PAGE_DATA_COMPRESSION {
        return Err(TapeIoError::MalformedModeResponse(format!(
            "expected page 0x0F, got 0x{page_code:02X}"
        )));
    }
    let page_length = buf[pages_start + 1] as usize;
    let dce_offset = pages_start + 2;
    if buf.len() < dce_offset + 1 || page_length < 1 {
        return Err(TapeIoError::MalformedModeResponse(format!(
            "page 0x0F too short: length {page_length}, buf {}",
            buf.len()
        )));
    }
    let compression = (buf[dce_offset] & 0x80) != 0;

    Ok(ModeSenseDataCompression {
        block_size,
        compression,
        write_protected,
        worm: worm_state_from_medium_type(medium_type),
        medium_type,
    })
}

fn worm_state_from_medium_type(medium_type: u8) -> WormMediaState {
    match medium_type {
        0x00 => WormMediaState::Unknown,
        0x01 | 0x3C | 0x4C | 0x5C | 0x6C | 0x7C | 0x8C | 0x9C => WormMediaState::Worm,
        0x38 | 0x48 | 0x58 | 0x68 | 0x78 | 0x88 | 0x98 => WormMediaState::NotWorm,
        _ => WormMediaState::Unknown,
    }
}

/// Build a MODE SELECT(6) parameter list carrying the 4-byte
/// mode parameter header, an 8-byte block descriptor, and page
/// 0x0F (Data Compression). `cfg.max_block_size_bytes` is ignored
/// — the drive's own cap always wins.
///
/// **Important non-changeable field rules per IBM Table 345**
/// (codex 20:22 idref=01cf3e76 High): DCC is non-changeable 1,
/// DDE is non-changeable 1, and the compression / decompression
/// algorithm fields must be `0x00000001`. Sending any other value
/// for these non-changeable fields makes the drive return CHECK
/// CONDITION 5/2600 (INVALID FIELD IN PARAMETER LIST) and refuse
/// to apply any of the requested changes — including changes to
/// the unrelated DCE bit. Layer 3a only toggles DCE per
/// `cfg.compression` and pins the rest to their accepted values.
fn build_compression_param_list(cfg: &TapeConfig) -> Vec<u8> {
    let mut buf = Vec::with_capacity(28);

    // 4-byte mode parameter header. On MODE SELECT, byte 0 (Mode
    // Data Length) must be 0 per SPC; the drive recomputes it.
    //
    // Byte 2 is the **Device-Specific Parameter** on sequential-
    // access devices (IBM Table 330, §4.7.1.1) — bits 4..6 carry
    // BUFFERED MODE (default 001b, changeable shared field) and
    // bits 0..3 carry SPEED. IBM 4.7.1.1 + Table 14: changed
    // changeable fields update the drive's current value, so a
    // byte-2-of-0 payload would silently demote BUFFERED MODE
    // from 001b to 000b across every write_config() call,
    // changing write-completion semantics + throughput for any
    // initiator sharing the drive. We emit 0x10 = BUFFERED MODE
    // 001b, SPEED 0 — the IBM-documented default — so
    // write_config() touches only block size + compression as
    // intended. Codex 20:38 (idref=7714c4dc Medium) catch.
    buf.extend_from_slice(&[
        0,    // Mode Data Length (reserved on SELECT)
        0,    // Medium Type
        0x10, // Device-Specific Parameter: BUFFERED MODE=001b, SPEED=0
        8,    // Block Descriptor Length
    ]);

    // 8-byte block descriptor. Density Code 0 = retain current
    // density; Number of Blocks 0 = apply to all blocks; Block
    // Length 0 = variable-block, else fixed-block of size_bytes.
    // size_bytes is API-validated to fit in 24-bit unsigned at
    // the write_config entry, so the high byte is always 0.
    let block_length: u32 = match cfg.block_size {
        BlockSize::Variable => 0,
        BlockSize::Fixed { size_bytes } => size_bytes,
    };
    let bl_bytes = block_length.to_be_bytes();
    buf.extend_from_slice(&[
        0, // Density Code (0 = retain)
        0,
        0,
        0, // Number of Blocks = 0
        0, // Reserved
        bl_bytes[1],
        bl_bytes[2],
        bl_bytes[3], // Block Length (24-bit BE)
    ]);

    // 16-byte page 0x0F (Data Compression). DCE per
    // cfg.compression; DCC = 1 and DDE = 1 always (non-changeable
    // per IBM Table 345); algorithms always 1 (also non-changeable).
    // Codex 20:22 catch — earlier version sent DCC=0, DDE=0, and
    // algorithms=0 in the compression-off path, which would
    // surface as 5/2600 on real IBM drives.
    let byte_2 = if cfg.compression { 0xC0 } else { 0x40 }; // DCE | DCC=1
    let byte_3 = 0x80; // DDE = 1
    let algo: u32 = 1;
    let algo_bytes = algo.to_be_bytes();
    buf.extend_from_slice(&[
        remanence_scsi::mode::PAGE_DATA_COMPRESSION, // PS=0, SPF=0, page code
        14,                                          // PAGE LENGTH (n-2)
        byte_2,                                      // DCE | DCC=1 | reserved
        byte_3,                                      // DDE=1 | RED | reserved
        algo_bytes[0],
        algo_bytes[1],
        algo_bytes[2],
        algo_bytes[3], // COMPRESSION ALGORITHM = 1
        algo_bytes[0],
        algo_bytes[1],
        algo_bytes[2],
        algo_bytes[3], // DECOMPRESSION ALGORITHM
        0,
        0,
        0,
        0, // reserved
    ]);

    buf
}

/// Informational EOM signal extracted from CHECK CONDITION sense
/// after a WRITE near end-of-medium. `early_warning` mirrors the
/// EOM bit in sense byte 2 (the drive is past the EW point);
/// `end_of_medium` is set when sense key is `VOLUME OVERFLOW`
/// (0x0D) — the drive will refuse further writes.
struct WriteEomSignal {
    early_warning: bool,
    end_of_medium: bool,
}

/// Decode CHECK CONDITION sense for an informational EOM/EW
/// signal during WRITE. Returns `Some(signal)` when the sense
/// indicates "write completed but tape is filling up"; returns
/// `None` for real errors (caller maps via `map_scsi`).
///
/// Per IBM LTO SCSI Reference: near-EOM WRITEs raise CHECK
/// CONDITION with sense byte 2 EOM bit set and sense key in
/// {0 NO_SENSE — approaching EOM (EW), 0x0D VOLUME_OVERFLOW —
/// tape full}. The block has been committed.
fn write_eom_signal(sense: &[u8]) -> Option<WriteEomSignal> {
    let decoded = decode_sense(sense)?;
    if !decoded.is_fixed_format() || !decoded.eom {
        return None;
    }
    match decoded.key {
        0x00 => Some(WriteEomSignal {
            early_warning: true,
            end_of_medium: false,
        }),
        0x0D => Some(WriteEomSignal {
            early_warning: true,
            end_of_medium: true,
        }),
        _ => None,
    }
}

fn read_filemark_signal(sense: &[u8]) -> bool {
    let Some(decoded) = decode_sense(sense) else {
        return false;
    };
    decoded.is_fixed_format()
        && decoded.valid
        && decoded.filemark
        && !decoded.ili
        && decoded.key == 0x00
        && decoded.asc == 0x00
        && decoded.ascq == 0x01
}

/// Extract the signed INFORMATION field from a CHECK CONDITION
/// where the drive set the ILI (Incorrect Length Indicator) bit
/// per IBM LTO SCSI Reference §4.12.1 / Table 17. Returns
/// `Some(signed_info)` when:
///
/// - response code is fixed-format (0x70 or 0x71)
/// - VALID bit (byte 0 bit 7) is set — INFORMATION is meaningful
/// - ILI bit (byte 2 bit 5) is set
///
/// The returned value is `requested - actual` in two's-complement;
/// positive means the on-tape block was smaller than the host
/// buffer, negative means it was larger.
fn ili_signed_information(sense: &[u8]) -> Option<i64> {
    let decoded = decode_sense(sense)?;
    if !decoded.is_fixed_format() || !decoded.valid || !decoded.ili {
        return None;
    }
    let info_bytes: [u8; 4] = sense.get(3..7)?.try_into().ok()?;
    Some(i32::from_be_bytes(info_bytes) as i64)
}

/// Extract the signed residual count from a CHECK CONDITION that
/// SSC-5 §6.10 calls "spaced to boundary". Returns `Some(residual)`
/// when sense indicates a successful early-stop:
///
/// - response code is fixed-format (0x70 or 0x71)
/// - VALID bit (byte 0 bit 7) is set — INFORMATION is meaningful
/// - sense key is `NO SENSE` (0) or `BLANK CHECK` (8) — anything
///   else is a real failure
///
/// The returned residual is `count - units_actually_moved`; the
/// caller computes traversed = `requested - residual`. Sign matches
/// the request: backward SPACE returns a negative residual when it
/// stops short.
fn space_residual_if_early_stop(sense: &[u8]) -> Option<i64> {
    let decoded = decode_sense(sense)?;
    if !decoded.is_fixed_format() || !decoded.valid {
        return None;
    }
    // SSC-5 stops at boundary report sense key 0 (NO SENSE) or 8
    // (BLANK CHECK, when SPACE on Blocks crosses EOD). Anything
    // else is a hard error and the caller treats it via map_scsi.
    if decoded.key != 0x00 && decoded.key != 0x08 {
        return None;
    }
    let info_bytes: [u8; 4] = sense.get(3..7)?.try_into().ok()?;
    let residual = i32::from_be_bytes(info_bytes) as i64;
    Some(residual)
}

fn units_traversed_from_space_residual(count: i64, residual: i64) -> i64 {
    let traversed = count.saturating_sub(residual);
    if count >= 0 {
        traversed.clamp(0, count)
    } else {
        traversed.clamp(count, 0)
    }
}

#[cfg(test)]
mod tests;
