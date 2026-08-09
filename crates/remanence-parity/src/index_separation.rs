//! Typed fixed-record separation extent between terminal index replicas.
//!
//! The extent's configured byte target includes its header and footer. Its
//! interior records are zero-filled only while hardware compression is
//! verified disabled. The trailing filemark is outside `total_records`; the
//! terminal-tail plan counts it once for logical positioning while the
//! capacity model applies its separate conservative charge.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::sidecar::crc64_xz;
use crate::terminal_tail::{
    validate_terminal_index_block_size, TerminalTailComponentKind, TerminalTailComponentPlan,
    TerminalTailLayout, TerminalTailLayoutError, TERMINAL_INDEX_SEPARATION_COUNT,
    TERMINAL_TAIL_COMPONENT_COUNT,
};

type HmacSha256 = Hmac<Sha256>;

/// Default physical extent target. Header and footer are included.
pub const DEFAULT_INDEX_SEPARATION_BYTES: u64 = 1 << 30;

/// Version of the typed separation frame.
pub const INDEX_SEPARATION_SCHEMA_VERSION: u16 = 1;

/// Meaningful bytes in the otherwise zero-padded full header/footer record.
pub const INDEX_SEPARATION_FRAME_LEN: usize = 0x200;

/// CRC-64/XZ offset within the meaningful frame.
pub const INDEX_SEPARATION_CRC_OFFSET: usize = 0x1F8;

const INDEX_SEPARATION_HEADER_MAGIC_MESSAGE: &[u8] = b"REM\0TISEP\x01H";
const INDEX_SEPARATION_FOOTER_MAGIC_MESSAGE: &[u8] = b"REM\0TISEP\x01F";
const INDEX_SEPARATION_DESCRIPTOR_DOMAIN: &[u8] = b"REM-INDEX-SEPARATION-DESCRIPTOR-V1\0";
const INDEX_SEPARATION_HEADER_ROLE: u16 = 1;
const INDEX_SEPARATION_FOOTER_ROLE: u16 = 2;
const INDEX_SEPARATION_FILL_ZERO: u32 = 0;

const TAIL_COMPONENTS_OFFSET: usize = 0x0E0;
const TAIL_COMPONENT_LEN: usize = 32;
const HEADER_SHA256_OFFSET: usize = 0x180;
const OBSERVED_TAPE_FILE_OFFSET: usize = 0x1A0;
const OBSERVED_START_LBA_OFFSET: usize = 0x1A8;
const OBSERVED_RECORD_COUNT_OFFSET: usize = 0x1B0;
const OBSERVED_FOOTER_LBA_OFFSET: usize = 0x1B8;
const BACKWARD_START_DELTA_OFFSET: usize = 0x1C0;
const FIXED_RESERVED_OFFSET: usize = 0x1C8;

/// Immutable gap metadata shared by its header and footer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexSeparationDescriptor {
    /// Tape identity.
    pub tape_uuid: [u8; 16],
    /// Final index edition shared by A, B, and C.
    pub edition_id: [u8; 16],
    /// Gap AB is 1 and gap BC is 2.
    pub gap_ordinal: u16,
    /// Fixed record size.
    pub block_size: u32,
    /// Configured total extent target, including header and footer.
    pub nominal_extent_bytes: u64,
    /// Total records including header and footer, excluding the filemark.
    pub total_records: u64,
    /// Must be false for the zero-filled separation profile.
    pub compression_enabled: bool,
    /// Complete planned terminal layout.
    pub terminal_layout: TerminalTailLayout,
}

/// Start/count values measured locally by the writer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexSeparationObservation {
    /// Dense tape-file number from the writer's physical cursor.
    pub tape_file_number: u64,
    /// Physical start LBA from the writer's barrier coordinate.
    pub start_lba: u64,
    /// Emitted records including header and footer.
    pub record_count: u64,
}

/// Fully checked immutable plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexSeparationPlan {
    /// Shared descriptor.
    pub descriptor: IndexSeparationDescriptor,
    /// Planned gap component.
    pub component: TerminalTailComponentPlan,
    /// Planned predecessor component.
    pub predecessor: TerminalTailComponentPlan,
    /// Planned successor component.
    pub successor: TerminalTailComponentPlan,
    /// Digest of the complete planned five-file layout.
    pub layout_digest: [u8; 32],
    /// Digest binding the gap descriptor to its neighbors and layout.
    pub descriptor_digest: [u8; 32],
}

/// Parsed header facts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexSeparationHeader {
    /// Checked plan encoded by the header.
    pub plan: IndexSeparationPlan,
    /// SHA-256 of the complete zero-padded header record.
    pub header_sha256: [u8; 32],
}

/// Parsed footer facts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexSeparationFooter {
    /// Checked plan repeated by the footer.
    pub plan: IndexSeparationPlan,
    /// Header-record digest carried by the footer.
    pub header_sha256: [u8; 32],
    /// Locally observed placement.
    pub observation: IndexSeparationObservation,
    /// Footer LBA derived from observed start/count.
    pub observed_footer_lba: u64,
    /// Backward distance from footer to header.
    pub backward_start_delta: u64,
}

/// Streamed source for the records between a gap header and footer.
pub trait IndexSeparationInteriorBlockSource {
    /// Visit every interior record in physical order.
    fn visit_interior_blocks(
        &mut self,
        visitor: &mut dyn FnMut(&[u8]) -> Result<(), IndexSeparationError>,
    ) -> Result<(), IndexSeparationError>;
}

/// Typed separation framing/validation failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum IndexSeparationError {
    /// Shared terminal layout is invalid.
    #[error("invalid terminal layout for separation: {0}")]
    Layout(#[from] TerminalTailLayoutError),
    /// Zero-filled separation cannot be honest with compression enabled.
    #[error("index separation requires hardware compression disabled")]
    CompressionEnabled,
    /// Physical interior source ended or failed before the planned extent.
    #[error("index separation physical source failed: {0}")]
    PhysicalSource(String),
    /// Gap ordinal is outside 1..=2.
    #[error("invalid separation ordinal {ordinal}; expected 1 or 2")]
    InvalidOrdinal {
        /// Supplied ordinal.
        ordinal: u16,
    },
    /// Final edition identity cannot be the all-zero sentinel.
    #[error("index separation edition ID is zero")]
    ZeroEditionId,
    /// Extent cannot hold its header and footer.
    #[error("separation record count {records} is below 2")]
    TooShort {
        /// Supplied count.
        records: u64,
    },
    /// Descriptor count differs from the planned component.
    #[error("separation record count {actual} differs from planned {planned}")]
    RecordCountMismatch {
        /// Planned records.
        planned: u64,
        /// Supplied or observed records.
        actual: u64,
    },
    /// The outer gap descriptor and committed terminal layout disagree on B.
    #[error("separation block size {actual} differs from terminal layout {planned}")]
    BlockSizeMismatch {
        /// Block size committed by the terminal layout.
        planned: u32,
        /// Block size supplied by the gap descriptor.
        actual: u32,
    },
    /// Frame magic does not match tape/domain/kind.
    #[error("index separation {frame} magic mismatch")]
    MagicMismatch {
        /// Header or footer.
        frame: &'static str,
    },
    /// Schema version is unsupported.
    #[error("unsupported index separation schema version {version}")]
    UnsupportedVersion {
        /// Encoded version.
        version: u16,
    },
    /// Encoded gap identity differs from expectation.
    #[error("index separation tape UUID mismatch")]
    WrongTape,
    /// Full record or meaningful frame has the wrong length.
    #[error("index separation {frame} length {actual}, expected block size {expected}")]
    WrongLength {
        /// Header or footer.
        frame: &'static str,
        /// Required bytes.
        expected: usize,
        /// Supplied bytes.
        actual: usize,
    },
    /// Full record length is not one of the closed profile sizes.
    #[error("index separation {frame} has unsupported record length {actual}")]
    UnsupportedRecordLength {
        /// Header, footer, or interior record.
        frame: &'static str,
        /// Supplied bytes.
        actual: usize,
    },
    /// A reserved byte is nonzero.
    #[error("index separation {field} has nonzero byte 0x{byte:02x} at relative offset {offset}")]
    ReservedNonzero {
        /// Reserved region.
        field: &'static str,
        /// Relative offset.
        offset: usize,
        /// Unexpected byte.
        byte: u8,
    },
    /// CRC does not match.
    #[error("index separation {frame} CRC mismatch")]
    CrcMismatch {
        /// Header or footer.
        frame: &'static str,
    },
    /// Layout or descriptor digest does not match recomputation.
    #[error("index separation {field} digest mismatch")]
    DigestMismatch {
        /// Digest name.
        field: &'static str,
    },
    /// Footer carried a header digest from another extent.
    #[error("index separation footer header digest mismatch")]
    HeaderDigestMismatch,
    /// Observed placement differs from the immutable plan.
    #[error("index separation observed {field} {actual}, planned {planned}")]
    ObservationMismatch {
        /// Placement field.
        field: &'static str,
        /// Planned value.
        planned: u64,
        /// Observed value.
        actual: u64,
    },
    /// Checked arithmetic overflowed.
    #[error("index separation arithmetic overflow: {context}")]
    ArithmeticOverflow {
        /// Failed arithmetic.
        context: &'static str,
    },
    /// Caller stopped or failed the streaming emitter.
    #[error("index separation emitter failed: {message}")]
    Emit {
        /// Emitter detail.
        message: String,
    },
}

/// Calculate records for an extent target. Header/footer are already included.
pub fn index_separation_records(
    block_size: u32,
    extent_bytes: u64,
) -> Result<u64, IndexSeparationError> {
    validate_terminal_index_block_size(block_size)?;
    let block_size = u64::from(block_size);
    let adjusted = extent_bytes.checked_add(block_size - 1).ok_or(
        IndexSeparationError::ArithmeticOverflow {
            context: "extent byte ceiling division",
        },
    )?;
    let records = adjusted / block_size;
    if records < 2 {
        return Err(IndexSeparationError::TooShort { records });
    }
    Ok(records)
}

/// Validate and bind one gap descriptor.
pub fn plan_index_separation(
    descriptor: IndexSeparationDescriptor,
) -> Result<IndexSeparationPlan, IndexSeparationError> {
    validate_terminal_index_block_size(descriptor.block_size)?;
    if descriptor.compression_enabled {
        return Err(IndexSeparationError::CompressionEnabled);
    }
    if descriptor.edition_id == [0; 16] {
        return Err(IndexSeparationError::ZeroEditionId);
    }
    if !(1..=TERMINAL_INDEX_SEPARATION_COUNT).contains(&descriptor.gap_ordinal) {
        return Err(IndexSeparationError::InvalidOrdinal {
            ordinal: descriptor.gap_ordinal,
        });
    }
    if descriptor.total_records < 2 {
        return Err(IndexSeparationError::TooShort {
            records: descriptor.total_records,
        });
    }
    let expected_records =
        index_separation_records(descriptor.block_size, descriptor.nominal_extent_bytes)?;
    if descriptor.total_records != expected_records {
        return Err(IndexSeparationError::RecordCountMismatch {
            planned: expected_records,
            actual: descriptor.total_records,
        });
    }
    descriptor.terminal_layout.validate()?;
    if descriptor.block_size != descriptor.terminal_layout.block_size {
        return Err(IndexSeparationError::BlockSizeMismatch {
            planned: descriptor.terminal_layout.block_size,
            actual: descriptor.block_size,
        });
    }
    let component = descriptor
        .terminal_layout
        .separation(descriptor.gap_ordinal)?;
    if descriptor.total_records != component.record_count {
        return Err(IndexSeparationError::RecordCountMismatch {
            planned: component.record_count,
            actual: descriptor.total_records,
        });
    }
    let component_index = if descriptor.gap_ordinal == 1 { 1 } else { 3 };
    let predecessor = descriptor.terminal_layout.components[component_index - 1];
    let successor = descriptor.terminal_layout.components[component_index + 1];
    let layout_digest = descriptor.terminal_layout.digest()?;
    let descriptor_digest = separation_descriptor_digest(
        &descriptor,
        component,
        predecessor,
        successor,
        layout_digest,
    );
    Ok(IndexSeparationPlan {
        descriptor,
        component,
        predecessor,
        successor,
        layout_digest,
        descriptor_digest,
    })
}

/// Encode a complete zero-padded header record.
pub fn encode_index_separation_header(
    plan: &IndexSeparationPlan,
) -> Result<Vec<u8>, IndexSeparationError> {
    validate_plan(plan)?;
    let mut block = vec![0u8; validate_terminal_index_block_size(plan.descriptor.block_size)?];
    write_plan_frame(
        &mut block,
        plan,
        derive_index_separation_header_magic(&plan.descriptor.tape_uuid),
        INDEX_SEPARATION_HEADER_ROLE,
    )?;
    write_crc(&mut block);
    Ok(block)
}

/// Encode a complete zero-padded footer record with local observations.
pub fn encode_index_separation_footer(
    plan: &IndexSeparationPlan,
    header_sha256: [u8; 32],
    observation: IndexSeparationObservation,
) -> Result<Vec<u8>, IndexSeparationError> {
    validate_plan(plan)?;
    validate_observation(plan, observation)?;
    let mut block = vec![0u8; validate_terminal_index_block_size(plan.descriptor.block_size)?];
    write_plan_frame(
        &mut block,
        plan,
        derive_index_separation_footer_magic(&plan.descriptor.tape_uuid),
        INDEX_SEPARATION_FOOTER_ROLE,
    )?;
    block[HEADER_SHA256_OFFSET..HEADER_SHA256_OFFSET + 32].copy_from_slice(&header_sha256);
    write_u64(
        &mut block,
        OBSERVED_TAPE_FILE_OFFSET,
        observation.tape_file_number,
    );
    write_u64(&mut block, OBSERVED_START_LBA_OFFSET, observation.start_lba);
    write_u64(
        &mut block,
        OBSERVED_RECORD_COUNT_OFFSET,
        observation.record_count,
    );
    let footer_delta =
        observation
            .record_count
            .checked_sub(1)
            .ok_or(IndexSeparationError::TooShort {
                records: observation.record_count,
            })?;
    let footer_lba = observation.start_lba.checked_add(footer_delta).ok_or(
        IndexSeparationError::ArithmeticOverflow {
            context: "observed footer LBA",
        },
    )?;
    write_u64(&mut block, OBSERVED_FOOTER_LBA_OFFSET, footer_lba);
    write_u64(&mut block, BACKWARD_START_DELTA_OFFSET, footer_delta);
    write_crc(&mut block);
    Ok(block)
}

/// Stream exactly one typed extent without allocating its one-GiB body.
pub fn write_index_separation<F>(
    plan: &IndexSeparationPlan,
    observation: IndexSeparationObservation,
    mut emit_block: F,
) -> Result<(), IndexSeparationError>
where
    F: FnMut(&[u8]) -> Result<(), String>,
{
    validate_plan(plan)?;
    validate_observation(plan, observation)?;
    let header = encode_index_separation_header(plan)?;
    let header_sha256: [u8; 32] = Sha256::digest(&header).into();
    emit_block(&header).map_err(|message| IndexSeparationError::Emit { message })?;

    let zero = vec![0u8; validate_terminal_index_block_size(plan.descriptor.block_size)?];
    for _ in 0..plan.descriptor.total_records - 2 {
        emit_block(&zero).map_err(|message| IndexSeparationError::Emit { message })?;
    }

    let footer = encode_index_separation_footer(plan, header_sha256, observation)?;
    emit_block(&footer).map_err(|message| IndexSeparationError::Emit { message })?;
    Ok(())
}

/// Parse and validate a header record.
pub fn parse_index_separation_header(
    block: &[u8],
    expected_tape_uuid: &[u8; 16],
) -> Result<IndexSeparationHeader, IndexSeparationError> {
    let expected_magic = derive_index_separation_header_magic(expected_tape_uuid);
    if block.len() < 8 || block[..8] != expected_magic {
        return Err(IndexSeparationError::MagicMismatch { frame: "header" });
    }
    let plan = parse_plan_frame(
        block,
        expected_tape_uuid,
        "header",
        INDEX_SEPARATION_HEADER_ROLE,
    )?;
    Ok(IndexSeparationHeader {
        plan,
        header_sha256: Sha256::digest(block).into(),
    })
}

/// Parse and validate a footer record, including planned/observed equality.
pub fn parse_index_separation_footer(
    block: &[u8],
    expected_tape_uuid: &[u8; 16],
) -> Result<IndexSeparationFooter, IndexSeparationError> {
    let expected_magic = derive_index_separation_footer_magic(expected_tape_uuid);
    if block.len() < 8 || block[..8] != expected_magic {
        return Err(IndexSeparationError::MagicMismatch { frame: "footer" });
    }
    let plan = parse_plan_frame(
        block,
        expected_tape_uuid,
        "footer",
        INDEX_SEPARATION_FOOTER_ROLE,
    )?;
    let mut header_sha256 = [0u8; 32];
    header_sha256.copy_from_slice(&block[HEADER_SHA256_OFFSET..HEADER_SHA256_OFFSET + 32]);
    let observation = IndexSeparationObservation {
        tape_file_number: read_u64(block, OBSERVED_TAPE_FILE_OFFSET),
        start_lba: read_u64(block, OBSERVED_START_LBA_OFFSET),
        record_count: read_u64(block, OBSERVED_RECORD_COUNT_OFFSET),
    };
    validate_observation(&plan, observation)?;
    let observed_footer_lba = read_u64(block, OBSERVED_FOOTER_LBA_OFFSET);
    let backward_start_delta = read_u64(block, BACKWARD_START_DELTA_OFFSET);
    let expected_delta = observation.record_count - 1;
    if backward_start_delta != expected_delta {
        return Err(IndexSeparationError::ObservationMismatch {
            field: "backward_start_delta",
            planned: expected_delta,
            actual: backward_start_delta,
        });
    }
    let expected_footer_lba = observation.start_lba.checked_add(expected_delta).ok_or(
        IndexSeparationError::ArithmeticOverflow {
            context: "parsed footer LBA",
        },
    )?;
    if observed_footer_lba != expected_footer_lba {
        return Err(IndexSeparationError::ObservationMismatch {
            field: "footer_lba",
            planned: expected_footer_lba,
            actual: observed_footer_lba,
        });
    }
    Ok(IndexSeparationFooter {
        plan,
        header_sha256,
        observation,
        observed_footer_lba,
        backward_start_delta,
    })
}

/// Validate that a parsed header and footer belong to the same extent.
pub fn validate_index_separation_pair(
    header: &IndexSeparationHeader,
    footer: &IndexSeparationFooter,
) -> Result<(), IndexSeparationError> {
    if header.plan != footer.plan {
        return Err(IndexSeparationError::DigestMismatch {
            field: "header/footer descriptor",
        });
    }
    if header.header_sha256 != footer.header_sha256 {
        return Err(IndexSeparationError::HeaderDigestMismatch);
    }
    Ok(())
}

/// Fully validate a gap by streaming and checking every interior byte is zero.
///
/// Pair validation alone is the fast structural check used while locating a
/// replica. Scrub/full-verify callers use this function to detect damage in the
/// deliberately typed physical separation region.
pub fn validate_index_separation_full<S: IndexSeparationInteriorBlockSource + ?Sized>(
    header: &IndexSeparationHeader,
    footer: &IndexSeparationFooter,
    source: &mut S,
) -> Result<u64, IndexSeparationError> {
    validate_index_separation_pair(header, footer)?;
    let expected_records = header.plan.descriptor.total_records.checked_sub(2).ok_or(
        IndexSeparationError::TooShort {
            records: header.plan.descriptor.total_records,
        },
    )?;
    let block_size = validate_terminal_index_block_size(header.plan.descriptor.block_size)?;
    let mut observed_records = 0u64;
    source.visit_interior_blocks(&mut |block| {
        if observed_records >= expected_records {
            let actual = observed_records.checked_add(1).ok_or(
                IndexSeparationError::ArithmeticOverflow {
                    context: "observed separation interior record count",
                },
            )?;
            return Err(IndexSeparationError::RecordCountMismatch {
                planned: expected_records,
                actual,
            });
        }
        if block.len() != block_size {
            return Err(IndexSeparationError::WrongLength {
                frame: "interior record",
                expected: block_size,
                actual: block.len(),
            });
        }
        if let Some((offset, byte)) = block
            .iter()
            .copied()
            .enumerate()
            .find(|(_, byte)| *byte != 0)
        {
            return Err(IndexSeparationError::ReservedNonzero {
                field: "interior zero fill",
                offset,
                byte,
            });
        }
        observed_records =
            observed_records
                .checked_add(1)
                .ok_or(IndexSeparationError::ArithmeticOverflow {
                    context: "validated interior record count",
                })?;
        Ok(())
    })?;
    if observed_records != expected_records {
        return Err(IndexSeparationError::RecordCountMismatch {
            planned: expected_records,
            actual: observed_records,
        });
    }
    Ok(observed_records)
}

/// Derive the tape-domain header magic.
pub fn derive_index_separation_header_magic(tape_uuid: &[u8; 16]) -> [u8; 8] {
    derive_magic(tape_uuid, INDEX_SEPARATION_HEADER_MAGIC_MESSAGE)
}

/// Derive the tape-domain footer magic.
pub fn derive_index_separation_footer_magic(tape_uuid: &[u8; 16]) -> [u8; 8] {
    derive_magic(tape_uuid, INDEX_SEPARATION_FOOTER_MAGIC_MESSAGE)
}

fn validate_plan(plan: &IndexSeparationPlan) -> Result<(), IndexSeparationError> {
    if *plan != plan_index_separation(plan.descriptor)? {
        return Err(IndexSeparationError::DigestMismatch {
            field: "immutable plan",
        });
    }
    Ok(())
}

fn validate_observation(
    plan: &IndexSeparationPlan,
    observation: IndexSeparationObservation,
) -> Result<(), IndexSeparationError> {
    for (field, planned, actual) in [
        (
            "tape_file_number",
            plan.component.planned_tape_file_number,
            observation.tape_file_number,
        ),
        (
            "start_lba",
            plan.component.planned_start_lba,
            observation.start_lba,
        ),
        (
            "record_count",
            plan.component.record_count,
            observation.record_count,
        ),
    ] {
        if planned != actual {
            return Err(IndexSeparationError::ObservationMismatch {
                field,
                planned,
                actual,
            });
        }
    }
    Ok(())
}

fn parse_plan_frame(
    block: &[u8],
    expected_tape_uuid: &[u8; 16],
    frame: &'static str,
    expected_role: u16,
) -> Result<IndexSeparationPlan, IndexSeparationError> {
    if block.len() < INDEX_SEPARATION_FRAME_LEN {
        return Err(IndexSeparationError::WrongLength {
            frame,
            expected: INDEX_SEPARATION_FRAME_LEN,
            actual: block.len(),
        });
    }
    let physical_block_size = u32::try_from(block.len())
        .ok()
        .and_then(|size| validate_terminal_index_block_size(size).ok())
        .ok_or(IndexSeparationError::UnsupportedRecordLength {
            frame,
            actual: block.len(),
        })?;
    let stored_crc = read_u64(block, INDEX_SEPARATION_CRC_OFFSET);
    if stored_crc != crc64_xz(&block[..INDEX_SEPARATION_CRC_OFFSET]) {
        return Err(IndexSeparationError::CrcMismatch { frame });
    }
    let version = read_u16(block, 0x08);
    if version != INDEX_SEPARATION_SCHEMA_VERSION {
        return Err(IndexSeparationError::UnsupportedVersion { version });
    }
    if read_u16(block, 0x0A) != expected_role {
        return Err(IndexSeparationError::DigestMismatch {
            field: "frame role",
        });
    }
    if read_u32(block, 0x0C) != 0 {
        return Err(IndexSeparationError::DigestMismatch { field: "flags" });
    }
    let mut tape_uuid = [0u8; 16];
    tape_uuid.copy_from_slice(&block[0x10..0x20]);
    if &tape_uuid != expected_tape_uuid {
        return Err(IndexSeparationError::WrongTape);
    }
    let mut edition_id = [0u8; 16];
    edition_id.copy_from_slice(&block[0x20..0x30]);
    let gap_ordinal = read_u16(block, 0x30);
    if read_u16(block, 0x32) != TERMINAL_INDEX_SEPARATION_COUNT {
        return Err(IndexSeparationError::DigestMismatch {
            field: "separation count",
        });
    }
    let partition = read_u32(block, 0x34);
    let block_size = read_u32(block, 0x38);
    if read_u32(block, 0x3C) != 0 {
        return Err(IndexSeparationError::CompressionEnabled);
    }
    let expected_len = validate_terminal_index_block_size(block_size)?;
    if block_size != physical_block_size as u32 {
        return Err(IndexSeparationError::WrongLength {
            frame,
            expected: expected_len,
            actual: block.len(),
        });
    }
    let nominal_extent_bytes = read_u64(block, 0x50);
    let total_records = read_u64(block, 0x58);
    if edition_id == [0; 16] {
        return Err(IndexSeparationError::ZeroEditionId);
    }
    if !(1..=TERMINAL_INDEX_SEPARATION_COUNT).contains(&gap_ordinal) {
        return Err(IndexSeparationError::InvalidOrdinal {
            ordinal: gap_ordinal,
        });
    }
    if partition != 0 {
        return Err(IndexSeparationError::Layout(
            TerminalTailLayoutError::UnsupportedPartition { partition },
        ));
    }
    if total_records < 2 {
        return Err(IndexSeparationError::TooShort {
            records: total_records,
        });
    }
    match expected_role {
        INDEX_SEPARATION_HEADER_ROLE => ensure_zero(
            &block[HEADER_SHA256_OFFSET..INDEX_SEPARATION_CRC_OFFSET],
            "header reserved fields",
        )?,
        INDEX_SEPARATION_FOOTER_ROLE => ensure_zero(
            &block[FIXED_RESERVED_OFFSET..INDEX_SEPARATION_CRC_OFFSET],
            "footer reserved fields",
        )?,
        _ => {
            return Err(IndexSeparationError::DigestMismatch {
                field: "parser role",
            })
        }
    }
    ensure_zero(&block[INDEX_SEPARATION_FRAME_LEN..], "full record padding")?;
    let footer_offset = total_records
        .checked_sub(1)
        .ok_or(IndexSeparationError::TooShort {
            records: total_records,
        })?;
    let interior_records = total_records
        .checked_sub(2)
        .ok_or(IndexSeparationError::TooShort {
            records: total_records,
        })?;
    if read_u64(block, 0x60) != footer_offset
        || read_u64(block, 0x68) != interior_records
        || read_u32(block, 0x74) != INDEX_SEPARATION_FILL_ZERO
    {
        return Err(IndexSeparationError::DigestMismatch {
            field: "separation geometry",
        });
    }
    let mut stored_layout_digest = [0u8; 32];
    stored_layout_digest.copy_from_slice(&block[0x98..0xB8]);
    let mut stored_descriptor_digest = [0u8; 32];
    stored_descriptor_digest.copy_from_slice(&block[0xB8..0xD8]);
    let mut components = [TerminalTailComponentPlan {
        kind: TerminalTailComponentKind::TapeIndexReplica,
        ordinal: 1,
        planned_tape_file_number: 0,
        planned_start_lba: 0,
        record_count: 2,
    }; TERMINAL_TAIL_COMPONENT_COUNT];
    for (index, component) in components.iter_mut().enumerate() {
        *component = read_component(block, TAIL_COMPONENTS_OFFSET + index * TAIL_COMPONENT_LEN)?;
    }
    let terminal_layout = TerminalTailLayout {
        partition,
        block_size,
        components,
        expected_eod_lba: read_u64(block, 0xD8),
    };
    let descriptor = IndexSeparationDescriptor {
        tape_uuid,
        edition_id,
        gap_ordinal,
        block_size,
        nominal_extent_bytes,
        total_records,
        compression_enabled: false,
        terminal_layout,
    };
    let plan = plan_index_separation(descriptor)?;
    if plan.component.planned_tape_file_number != read_u64(block, 0x40)
        || plan.component.planned_start_lba != read_u64(block, 0x48)
        || plan.predecessor.ordinal != read_u16(block, 0x70)
        || plan.successor.ordinal != read_u16(block, 0x72)
        || plan.predecessor.planned_tape_file_number != read_u64(block, 0x78)
        || plan.successor.planned_tape_file_number != read_u64(block, 0x80)
        || plan.predecessor.planned_start_lba != read_u64(block, 0x88)
        || plan.successor.planned_start_lba != read_u64(block, 0x90)
    {
        return Err(IndexSeparationError::DigestMismatch {
            field: "planned component/neighbors",
        });
    }
    if plan.layout_digest != stored_layout_digest {
        return Err(IndexSeparationError::DigestMismatch { field: "layout" });
    }
    if plan.descriptor_digest != stored_descriptor_digest {
        return Err(IndexSeparationError::DigestMismatch {
            field: "descriptor",
        });
    }
    Ok(plan)
}

fn write_plan_frame(
    block: &mut [u8],
    plan: &IndexSeparationPlan,
    magic: [u8; 8],
    role: u16,
) -> Result<(), IndexSeparationError> {
    if block.len() != validate_terminal_index_block_size(plan.descriptor.block_size)? {
        return Err(IndexSeparationError::WrongLength {
            frame: "encoded",
            expected: plan.descriptor.block_size as usize,
            actual: block.len(),
        });
    }
    block[..8].copy_from_slice(&magic);
    block[0x08..0x0A].copy_from_slice(&INDEX_SEPARATION_SCHEMA_VERSION.to_le_bytes());
    block[0x0A..0x0C].copy_from_slice(&role.to_le_bytes());
    block[0x10..0x20].copy_from_slice(&plan.descriptor.tape_uuid);
    block[0x20..0x30].copy_from_slice(&plan.descriptor.edition_id);
    block[0x30..0x32].copy_from_slice(&plan.descriptor.gap_ordinal.to_le_bytes());
    block[0x32..0x34].copy_from_slice(&TERMINAL_INDEX_SEPARATION_COUNT.to_le_bytes());
    block[0x34..0x38].copy_from_slice(&plan.descriptor.terminal_layout.partition.to_le_bytes());
    block[0x38..0x3C].copy_from_slice(&plan.descriptor.block_size.to_le_bytes());
    write_u64(block, 0x40, plan.component.planned_tape_file_number);
    write_u64(block, 0x48, plan.component.planned_start_lba);
    write_u64(block, 0x50, plan.descriptor.nominal_extent_bytes);
    write_u64(block, 0x58, plan.descriptor.total_records);
    write_u64(block, 0x60, plan.descriptor.total_records - 1);
    write_u64(block, 0x68, plan.descriptor.total_records - 2);
    block[0x70..0x72].copy_from_slice(&plan.predecessor.ordinal.to_le_bytes());
    block[0x72..0x74].copy_from_slice(&plan.successor.ordinal.to_le_bytes());
    block[0x74..0x78].copy_from_slice(&INDEX_SEPARATION_FILL_ZERO.to_le_bytes());
    write_u64(block, 0x78, plan.predecessor.planned_tape_file_number);
    write_u64(block, 0x80, plan.successor.planned_tape_file_number);
    write_u64(block, 0x88, plan.predecessor.planned_start_lba);
    write_u64(block, 0x90, plan.successor.planned_start_lba);
    block[0x98..0xB8].copy_from_slice(&plan.layout_digest);
    block[0xB8..0xD8].copy_from_slice(&plan.descriptor_digest);
    write_u64(
        block,
        0xD8,
        plan.descriptor.terminal_layout.expected_eod_lba,
    );
    for (index, component) in plan
        .descriptor
        .terminal_layout
        .components
        .into_iter()
        .enumerate()
    {
        write_component(
            block,
            TAIL_COMPONENTS_OFFSET + index * TAIL_COMPONENT_LEN,
            component,
        );
    }
    Ok(())
}

fn separation_descriptor_digest(
    descriptor: &IndexSeparationDescriptor,
    component: TerminalTailComponentPlan,
    predecessor: TerminalTailComponentPlan,
    successor: TerminalTailComponentPlan,
    layout_digest: [u8; 32],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(INDEX_SEPARATION_DESCRIPTOR_DOMAIN);
    hasher.update(descriptor.tape_uuid);
    hasher.update(descriptor.edition_id);
    hasher.update(descriptor.gap_ordinal.to_le_bytes());
    hasher.update(TERMINAL_INDEX_SEPARATION_COUNT.to_le_bytes());
    hasher.update(descriptor.terminal_layout.partition.to_le_bytes());
    hasher.update(descriptor.block_size.to_le_bytes());
    hasher.update(descriptor.nominal_extent_bytes.to_le_bytes());
    hasher.update(descriptor.total_records.to_le_bytes());
    update_component_digest(&mut hasher, component);
    update_component_digest(&mut hasher, predecessor);
    update_component_digest(&mut hasher, successor);
    hasher.update(layout_digest);
    hasher.finalize().into()
}

fn derive_magic(tape_uuid: &[u8; 16], message: &[u8]) -> [u8; 8] {
    let mut mac = HmacSha256::new_from_slice(tape_uuid).expect("HMAC accepts any key length");
    mac.update(message);
    let bytes = mac.finalize().into_bytes();
    bytes[..8].try_into().expect("eight-byte slice")
}

fn update_component_digest(hasher: &mut Sha256, component: TerminalTailComponentPlan) {
    hasher.update(component.kind.code().to_le_bytes());
    hasher.update(component.ordinal.to_le_bytes());
    hasher.update(1u32.to_le_bytes());
    hasher.update(component.planned_tape_file_number.to_le_bytes());
    hasher.update(component.planned_start_lba.to_le_bytes());
    hasher.update(component.record_count.to_le_bytes());
}

fn write_component(block: &mut [u8], offset: usize, component: TerminalTailComponentPlan) {
    block[offset..offset + 2].copy_from_slice(&component.kind.code().to_le_bytes());
    block[offset + 2..offset + 4].copy_from_slice(&component.ordinal.to_le_bytes());
    block[offset + 4..offset + 8].copy_from_slice(&1u32.to_le_bytes());
    write_u64(block, offset + 8, component.planned_tape_file_number);
    write_u64(block, offset + 16, component.planned_start_lba);
    write_u64(block, offset + 24, component.record_count);
}

fn read_component(
    block: &[u8],
    offset: usize,
) -> Result<TerminalTailComponentPlan, IndexSeparationError> {
    let kind = match read_u16(block, offset) {
        4 => TerminalTailComponentKind::TapeIndexReplica,
        5 => TerminalTailComponentKind::IndexSeparationExtent,
        _ => {
            return Err(IndexSeparationError::DigestMismatch {
                field: "component kind",
            });
        }
    };
    if read_u32(block, offset + 4) != 1 {
        return Err(IndexSeparationError::DigestMismatch {
            field: "component filemark",
        });
    }
    Ok(TerminalTailComponentPlan {
        kind,
        ordinal: read_u16(block, offset + 2),
        planned_tape_file_number: read_u64(block, offset + 8),
        planned_start_lba: read_u64(block, offset + 16),
        record_count: read_u64(block, offset + 24),
    })
}

fn write_crc(block: &mut [u8]) {
    let crc = crc64_xz(&block[..INDEX_SEPARATION_CRC_OFFSET]);
    write_u64(block, INDEX_SEPARATION_CRC_OFFSET, crc);
}

fn ensure_zero(bytes: &[u8], field: &'static str) -> Result<(), IndexSeparationError> {
    if let Some((offset, byte)) = bytes
        .iter()
        .copied()
        .enumerate()
        .find(|(_, byte)| *byte != 0)
    {
        return Err(IndexSeparationError::ReservedNonzero {
            field,
            offset,
            byte,
        });
    }
    Ok(())
}

fn write_u64(block: &mut [u8], offset: usize, value: u64) {
    block[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn read_u16(block: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(block[offset..offset + 2].try_into().expect("bounded frame"))
}

fn read_u32(block: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(block[offset..offset + 4].try_into().expect("bounded frame"))
}

fn read_u64(block: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(block[offset..offset + 8].try_into().expect("bounded frame"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct InteriorBlocks(Vec<Vec<u8>>);

    impl IndexSeparationInteriorBlockSource for InteriorBlocks {
        fn visit_interior_blocks(
            &mut self,
            visitor: &mut dyn FnMut(&[u8]) -> Result<(), IndexSeparationError>,
        ) -> Result<(), IndexSeparationError> {
            for block in &self.0 {
                visitor(block)?;
            }
            Ok(())
        }
    }

    fn sample_plan(block_size: u32, ordinal: u16) -> IndexSeparationPlan {
        let gap_records = index_separation_records(block_size, DEFAULT_INDEX_SEPARATION_BYTES)
            .expect("default gap geometry");
        let layout = TerminalTailLayout::new(0, block_size, 20, 1_000, 7, gap_records).unwrap();
        plan_index_separation(IndexSeparationDescriptor {
            tape_uuid: [0x11; 16],
            edition_id: [0x22; 16],
            gap_ordinal: ordinal,
            block_size,
            nominal_extent_bytes: DEFAULT_INDEX_SEPARATION_BYTES,
            total_records: gap_records,
            compression_enabled: false,
            terminal_layout: layout,
        })
        .unwrap()
    }

    #[test]
    fn gap_geometry_includes_header_and_footer_for_every_block_size() {
        for (block_size, expected) in [
            (256 * 1024, 4_096),
            (512 * 1024, 2_048),
            (1024 * 1024, 1_024),
        ] {
            assert_eq!(
                index_separation_records(block_size, DEFAULT_INDEX_SEPARATION_BYTES).unwrap(),
                expected
            );
        }
        assert_eq!(
            index_separation_records(256 * 1024, 256 * 1024 + 1).unwrap(),
            2
        );
    }

    #[test]
    fn header_footer_round_trip_and_bind_full_plan() {
        let plan = sample_plan(256 * 1024, 1);
        let header_block = encode_index_separation_header(&plan).unwrap();
        let observation = IndexSeparationObservation {
            tape_file_number: plan.component.planned_tape_file_number,
            start_lba: plan.component.planned_start_lba,
            record_count: plan.component.record_count,
        };
        let header_sha: [u8; 32] = Sha256::digest(&header_block).into();
        let footer_block = encode_index_separation_footer(&plan, header_sha, observation).unwrap();

        let header = parse_index_separation_header(&header_block, &[0x11; 16]).unwrap();
        let footer = parse_index_separation_footer(&footer_block, &[0x11; 16]).unwrap();
        validate_index_separation_pair(&header, &footer).unwrap();
        assert_eq!(header.plan, plan);
        assert_eq!(footer.observation, observation);
    }

    #[test]
    fn streaming_extent_uses_one_reusable_interior_block() {
        let plan = sample_plan(1024 * 1024, 2);
        let observation = IndexSeparationObservation {
            tape_file_number: plan.component.planned_tape_file_number,
            start_lba: plan.component.planned_start_lba,
            record_count: plan.component.record_count,
        };
        let mut calls = 0u64;
        let mut nonzero_calls = Vec::new();
        write_index_separation(&plan, observation, |block| {
            if block.iter().any(|byte| *byte != 0) {
                nonzero_calls.push(calls);
            }
            calls += 1;
            Ok(())
        })
        .unwrap();

        assert_eq!(calls, plan.descriptor.total_records);
        assert_eq!(nonzero_calls, vec![0, calls - 1]);
    }

    #[test]
    fn compression_and_observation_mismatch_fail_before_emission() {
        let mut descriptor = sample_plan(256 * 1024, 1).descriptor;
        descriptor.compression_enabled = true;
        assert_eq!(
            plan_index_separation(descriptor),
            Err(IndexSeparationError::CompressionEnabled)
        );

        let plan = sample_plan(256 * 1024, 1);
        let mut emitted = false;
        let error = write_index_separation(
            &plan,
            IndexSeparationObservation {
                tape_file_number: plan.component.planned_tape_file_number,
                start_lba: plan.component.planned_start_lba + 1,
                record_count: plan.component.record_count,
            },
            |_| {
                emitted = true;
                Ok(())
            },
        )
        .unwrap_err();
        assert!(matches!(
            error,
            IndexSeparationError::ObservationMismatch {
                field: "start_lba",
                ..
            }
        ));
        assert!(!emitted);
    }

    #[test]
    fn descriptor_block_size_must_match_committed_layout() {
        let mut descriptor = sample_plan(512 * 1024, 1).descriptor;
        descriptor.terminal_layout =
            TerminalTailLayout::new(0, 256 * 1024, 20, 1_000, 7, descriptor.total_records).unwrap();

        assert_eq!(
            plan_index_separation(descriptor),
            Err(IndexSeparationError::BlockSizeMismatch {
                planned: 256 * 1024,
                actual: 512 * 1024,
            })
        );
    }

    #[test]
    fn footer_rejects_header_substitution_and_torn_reserved_bytes() {
        let plan = sample_plan(256 * 1024, 1);
        let header_block = encode_index_separation_header(&plan).unwrap();
        let observation = IndexSeparationObservation {
            tape_file_number: plan.component.planned_tape_file_number,
            start_lba: plan.component.planned_start_lba,
            record_count: plan.component.record_count,
        };
        let footer_block = encode_index_separation_footer(&plan, [0xAA; 32], observation).unwrap();
        let header = parse_index_separation_header(&header_block, &[0x11; 16]).unwrap();
        let footer = parse_index_separation_footer(&footer_block, &[0x11; 16]).unwrap();
        assert_eq!(
            validate_index_separation_pair(&header, &footer),
            Err(IndexSeparationError::HeaderDigestMismatch)
        );

        let mut corrupt = footer_block;
        corrupt[FIXED_RESERVED_OFFSET] = 1;
        write_crc(&mut corrupt);
        assert!(matches!(
            parse_index_separation_footer(&corrupt, &[0x11; 16]),
            Err(IndexSeparationError::ReservedNonzero { .. })
        ));
    }

    #[test]
    fn full_verify_distinguishes_zero_interior_from_fast_pair_check() {
        let block_size = 1024 * 1024;
        let nominal_extent_bytes = u64::from(block_size) * 3;
        let total_records = index_separation_records(block_size, nominal_extent_bytes).unwrap();
        let plan = plan_index_separation(IndexSeparationDescriptor {
            tape_uuid: [0x11; 16],
            edition_id: [0x22; 16],
            gap_ordinal: 1,
            block_size,
            nominal_extent_bytes,
            total_records,
            compression_enabled: false,
            terminal_layout: TerminalTailLayout::new(0, block_size, 20, 1_000, 7, total_records)
                .unwrap(),
        })
        .unwrap();
        let header_block = encode_index_separation_header(&plan).unwrap();
        let observation = IndexSeparationObservation {
            tape_file_number: plan.component.planned_tape_file_number,
            start_lba: plan.component.planned_start_lba,
            record_count: plan.component.record_count,
        };
        let footer_block = encode_index_separation_footer(
            &plan,
            Sha256::digest(&header_block).into(),
            observation,
        )
        .unwrap();
        let header = parse_index_separation_header(&header_block, &[0x11; 16]).unwrap();
        let footer = parse_index_separation_footer(&footer_block, &[0x11; 16]).unwrap();
        validate_index_separation_pair(&header, &footer).unwrap();

        let interior_count = usize::try_from(plan.descriptor.total_records - 2).unwrap();
        let zeros = vec![vec![0; block_size as usize]; interior_count];
        assert_eq!(
            validate_index_separation_full(&header, &footer, &mut InteriorBlocks(zeros.clone()))
                .unwrap(),
            plan.descriptor.total_records - 2
        );

        let mut corrupt = zeros;
        corrupt[0][17] = 1;
        assert!(matches!(
            validate_index_separation_full(&header, &footer, &mut InteriorBlocks(corrupt)),
            Err(IndexSeparationError::ReservedNonzero {
                field: "interior zero fill",
                ..
            })
        ));
        validate_index_separation_pair(&header, &footer).unwrap();
    }

    #[test]
    fn zero_edition_and_hostile_frame_fail_closed() {
        let mut descriptor = sample_plan(256 * 1024, 1).descriptor;
        descriptor.edition_id = [0; 16];
        assert_eq!(
            plan_index_separation(descriptor),
            Err(IndexSeparationError::ZeroEditionId)
        );

        let plan = sample_plan(256 * 1024, 1);
        let valid = encode_index_separation_header(&plan).unwrap();
        assert!(matches!(
            parse_index_separation_header(&valid[..8], &[0x11; 16]),
            Err(IndexSeparationError::WrongLength { .. })
        ));

        let mut corrupt = valid.clone();
        corrupt[0x08] ^= 1;
        assert!(matches!(
            parse_index_separation_header(&corrupt, &[0x11; 16]),
            Err(IndexSeparationError::CrcMismatch { .. })
        ));

        let mut corrupt = valid.clone();
        corrupt[0x0A] = 9;
        write_crc(&mut corrupt);
        assert!(matches!(
            parse_index_separation_header(&corrupt, &[0x11; 16]),
            Err(IndexSeparationError::DigestMismatch {
                field: "frame role"
            })
        ));

        let mut corrupt = valid;
        corrupt[INDEX_SEPARATION_FRAME_LEN] = 1;
        assert!(matches!(
            parse_index_separation_header(&corrupt, &[0x11; 16]),
            Err(IndexSeparationError::ReservedNonzero { .. })
        ));
    }

    #[test]
    fn scalar_corruption_precedes_reserved_padding_corruption() {
        let plan = sample_plan(256 * 1024, 1);
        let valid = encode_index_separation_header(&plan).unwrap();

        let mut zero_edition = valid.clone();
        zero_edition[0x20..0x30].fill(0);
        zero_edition[INDEX_SEPARATION_FRAME_LEN] = 1;
        write_crc(&mut zero_edition);
        assert_eq!(
            parse_index_separation_header(&zero_edition, &[0x11; 16]),
            Err(IndexSeparationError::ZeroEditionId)
        );

        let mut bad_ordinal = valid.clone();
        bad_ordinal[0x30..0x32].copy_from_slice(&0u16.to_le_bytes());
        bad_ordinal[INDEX_SEPARATION_FRAME_LEN] = 1;
        write_crc(&mut bad_ordinal);
        assert_eq!(
            parse_index_separation_header(&bad_ordinal, &[0x11; 16]),
            Err(IndexSeparationError::InvalidOrdinal { ordinal: 0 })
        );

        let mut bad_partition = valid.clone();
        bad_partition[0x34..0x38].copy_from_slice(&1u32.to_le_bytes());
        bad_partition[INDEX_SEPARATION_FRAME_LEN] = 1;
        write_crc(&mut bad_partition);
        assert_eq!(
            parse_index_separation_header(&bad_partition, &[0x11; 16]),
            Err(IndexSeparationError::Layout(
                TerminalTailLayoutError::UnsupportedPartition { partition: 1 }
            ))
        );

        let mut short_count = valid;
        write_u64(&mut short_count, 0x58, 1);
        short_count[INDEX_SEPARATION_FRAME_LEN] = 1;
        write_crc(&mut short_count);
        assert_eq!(
            parse_index_separation_header(&short_count, &[0x11; 16]),
            Err(IndexSeparationError::TooShort { records: 1 })
        );
    }

    #[test]
    fn zero_interior_extent_reports_the_first_extra_record_exactly() {
        let block_size = 256 * 1024;
        let layout = TerminalTailLayout::new(0, block_size, 20, 1_000, 7, 2).unwrap();
        let plan = plan_index_separation(IndexSeparationDescriptor {
            tape_uuid: [0x11; 16],
            edition_id: [0x22; 16],
            gap_ordinal: 1,
            block_size,
            nominal_extent_bytes: u64::from(block_size) * 2,
            total_records: 2,
            compression_enabled: false,
            terminal_layout: layout,
        })
        .unwrap();
        let header_block = encode_index_separation_header(&plan).unwrap();
        let observation = IndexSeparationObservation {
            tape_file_number: plan.component.planned_tape_file_number,
            start_lba: plan.component.planned_start_lba,
            record_count: plan.component.record_count,
        };
        let footer_block = encode_index_separation_footer(
            &plan,
            Sha256::digest(&header_block).into(),
            observation,
        )
        .unwrap();
        let header = parse_index_separation_header(&header_block, &[0x11; 16]).unwrap();
        let footer = parse_index_separation_footer(&footer_block, &[0x11; 16]).unwrap();
        let error = validate_index_separation_full(
            &header,
            &footer,
            &mut InteriorBlocks(vec![vec![0; block_size as usize]]),
        )
        .expect_err("zero-interior extent cannot accept an extra record");
        assert!(matches!(
            error,
            IndexSeparationError::RecordCountMismatch {
                planned: 0,
                actual: 1
            }
        ));
    }
}
