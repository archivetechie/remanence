//! One-copy terminal tape-index replica framing.
//!
//! A replica is exactly one full header record, zero or more streamed payload
//! records, one full `BootstrapFooter` record, and a trailing filemark written
//! by the caller. A/B/C are separate tape files so every filemark and barrier
//! can advance durable five-component progress independently.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::diagnostic_text::{
    validate_write_timestamp, validate_writer_version, WRITER_VERSION_MAX_BYTES,
    WRITE_TIMESTAMP_MAX_BYTES,
};
use crate::error::ParityError;
use crate::sidecar::crc64_xz;
use crate::tape_index::{
    checked_tape_index_payload_byte_len, decode_tape_index_payload_map_entry_slot,
    decode_tape_index_payload_object_row_slot, encode_cbor_array_header, stream_tape_index_payload,
    validate_tape_index_payload_descriptor, TapeIndexPayloadCounts, TapeIndexPayloadDescriptor,
    TapeIndexPayloadFileKind, TapeIndexPayloadMapEntry, TapeIndexPayloadObjectRow,
    TapeIndexPayloadRecordSource, TapeIndexPayloadScope, TAPE_INDEX_PAYLOAD_OBJECT_ROW_SLOT_LEN,
    TAPE_INDEX_PAYLOAD_SLOT_PREFIX_LEN, TAPE_INDEX_PAYLOAD_STRUCTURAL_SLOT_LEN,
};
use crate::terminal_tail::{
    validate_terminal_index_block_size, validate_terminal_index_block_size_hint,
    TerminalTailComponentKind, TerminalTailComponentPlan, TerminalTailLayout,
    TerminalTailLayoutError, TERMINAL_INDEX_REPLICA_COUNT, TERMINAL_TAIL_COMPONENT_COUNT,
};

type HmacSha256 = Hmac<Sha256>;

/// Candidate terminal-replica schema version.
pub const TAPE_INDEX_REPLICA_SCHEMA_VERSION: u16 = 1;

/// Meaningful frame bytes in a full zero-padded header/footer record.
pub const TAPE_INDEX_REPLICA_FRAME_LEN: usize = 0x400;

/// CRC-64/XZ offset within the meaningful frame.
pub const TAPE_INDEX_REPLICA_CRC_OFFSET: usize = 0x3F8;

const TAPE_INDEX_REPLICA_HEADER_MAGIC_MESSAGE: &[u8] = b"REM\0TIREP\x01H";
const TAPE_INDEX_REPLICA_FOOTER_MAGIC_MESSAGE: &[u8] = b"REM\0TIREP\x01F";
const TAPE_INDEX_REPLICA_PAYLOAD_DOMAIN: &[u8] = b"REM-TAPE-INDEX-REPLICA-PAYLOAD-V1\0";
const TAPE_INDEX_EDITION_DIGEST_DOMAIN: &[u8] = b"REM-TAPE-INDEX-EDITION-V1\0";
const TAPE_INDEX_REPLICA_DESCRIPTOR_DOMAIN: &[u8] = b"REM-TAPE-INDEX-REPLICA-DESCRIPTOR-V1\0";
const TAPE_INDEX_REPLICA_HEADER_ROLE: u16 = 1;
const TAPE_INDEX_REPLICA_FOOTER_ROLE: u16 = 2;
const TAPE_INDEX_REPLICA_FLAG_FINAL: u32 = 1;

const MAGIC_OFFSET: usize = 0x000;
const SCHEMA_VERSION_OFFSET: usize = 0x008;
const ROLE_OFFSET: usize = 0x00A;
const FINALITY_FLAGS_OFFSET: usize = 0x00C;
const TAPE_UUID_OFFSET: usize = 0x010;
const EDITION_ID_OFFSET: usize = 0x020;
const EDITION_SEQUENCE_OFFSET: usize = 0x030;
const REPLICA_ORDINAL_OFFSET: usize = 0x038;
const REPLICA_COUNT_OFFSET: usize = 0x03A;
const PARTITION_OFFSET: usize = 0x03C;
const BLOCK_SIZE_OFFSET: usize = 0x040;
const COMPRESSION_OFFSET: usize = 0x044;
const COVERED_PREFIX_FILE_COUNT_OFFSET: usize = 0x048;
const TOTAL_DATA_ORDINALS_OFFSET: usize = 0x050;
const HIGHEST_PROTECTED_ORDINAL_OFFSET: usize = 0x058;
const STRUCTURAL_ENTRY_COUNT_OFFSET: usize = 0x060;
const OBJECT_ROW_COUNT_OFFSET: usize = 0x068;
const PAYLOAD_LEN_OFFSET: usize = 0x070;
const PAYLOAD_RECORD_COUNT_OFFSET: usize = 0x078;
const REPLICA_RECORD_COUNT_OFFSET: usize = 0x080;
const PLANNED_TAPE_FILE_OFFSET: usize = 0x088;
const PLANNED_START_LBA_OFFSET: usize = 0x090;
const FOOTER_BLOCK_OFFSET: usize = 0x098;
const EXPECTED_EOD_LBA_OFFSET: usize = 0x0A0;
const PAYLOAD_SHA256_OFFSET: usize = 0x0A8;
const CANONICAL_MAP_SHA256_OFFSET: usize = 0x0C8;
const EDITION_DIGEST_OFFSET: usize = 0x0E8;
const LAYOUT_DIGEST_OFFSET: usize = 0x108;
const DESCRIPTOR_DIGEST_OFFSET: usize = 0x128;
const TAIL_COMPONENTS_OFFSET: usize = 0x148;
const TAIL_COMPONENT_LEN: usize = 32;
const DIAGNOSTIC_LENGTHS_OFFSET: usize = 0x1F0;
const WRITER_VERSION_OFFSET: usize = 0x1F8;
const WRITE_TIMESTAMP_OFFSET: usize = 0x278;
const HEADER_SHA256_OFFSET: usize = 0x2B8;
const _: () = assert!(
    WRITER_VERSION_OFFSET + WRITER_VERSION_MAX_BYTES == WRITE_TIMESTAMP_OFFSET,
    "writer-version bound must exactly match its fixed frame slot"
);
const _: () = assert!(
    WRITE_TIMESTAMP_OFFSET + WRITE_TIMESTAMP_MAX_BYTES == HEADER_SHA256_OFFSET,
    "write-timestamp bound must exactly match its fixed frame slot"
);
const OBSERVED_TAPE_FILE_OFFSET: usize = 0x2D8;
const OBSERVED_START_LBA_OFFSET: usize = 0x2E0;
const OBSERVED_RECORD_COUNT_OFFSET: usize = 0x2E8;
const OBSERVED_FOOTER_LBA_OFFSET: usize = 0x2F0;
const BACKWARD_START_DELTA_OFFSET: usize = 0x2F8;
const FIXED_RESERVED_OFFSET: usize = 0x300;
const _: () = assert!(
    MAGIC_OFFSET == 0
        && MAGIC_OFFSET + 8 == SCHEMA_VERSION_OFFSET
        && SCHEMA_VERSION_OFFSET + size_of::<u16>() == ROLE_OFFSET
        && ROLE_OFFSET + size_of::<u16>() == FINALITY_FLAGS_OFFSET
        && FINALITY_FLAGS_OFFSET + size_of::<u32>() == TAPE_UUID_OFFSET
        && TAPE_UUID_OFFSET + 16 == EDITION_ID_OFFSET
        && EDITION_ID_OFFSET + 16 == EDITION_SEQUENCE_OFFSET
        && EDITION_SEQUENCE_OFFSET + size_of::<u64>() == REPLICA_ORDINAL_OFFSET
        && REPLICA_ORDINAL_OFFSET + size_of::<u16>() == REPLICA_COUNT_OFFSET
        && REPLICA_COUNT_OFFSET + size_of::<u16>() == PARTITION_OFFSET
        && PARTITION_OFFSET + size_of::<u32>() == BLOCK_SIZE_OFFSET
        && BLOCK_SIZE_OFFSET + size_of::<u32>() == COMPRESSION_OFFSET
        && COMPRESSION_OFFSET + size_of::<u32>() == COVERED_PREFIX_FILE_COUNT_OFFSET
        && COVERED_PREFIX_FILE_COUNT_OFFSET + size_of::<u64>() == TOTAL_DATA_ORDINALS_OFFSET
        && TOTAL_DATA_ORDINALS_OFFSET + size_of::<u64>() == HIGHEST_PROTECTED_ORDINAL_OFFSET
        && HIGHEST_PROTECTED_ORDINAL_OFFSET + size_of::<u64>() == STRUCTURAL_ENTRY_COUNT_OFFSET
        && STRUCTURAL_ENTRY_COUNT_OFFSET + size_of::<u64>() == OBJECT_ROW_COUNT_OFFSET
        && OBJECT_ROW_COUNT_OFFSET + size_of::<u64>() == PAYLOAD_LEN_OFFSET
        && PAYLOAD_LEN_OFFSET + size_of::<u64>() == PAYLOAD_RECORD_COUNT_OFFSET
        && PAYLOAD_RECORD_COUNT_OFFSET + size_of::<u64>() == REPLICA_RECORD_COUNT_OFFSET
        && REPLICA_RECORD_COUNT_OFFSET + size_of::<u64>() == PLANNED_TAPE_FILE_OFFSET
        && PLANNED_TAPE_FILE_OFFSET + size_of::<u64>() == PLANNED_START_LBA_OFFSET
        && PLANNED_START_LBA_OFFSET + size_of::<u64>() == FOOTER_BLOCK_OFFSET
        && FOOTER_BLOCK_OFFSET + size_of::<u64>() == EXPECTED_EOD_LBA_OFFSET
        && EXPECTED_EOD_LBA_OFFSET + size_of::<u64>() == PAYLOAD_SHA256_OFFSET
        && PAYLOAD_SHA256_OFFSET + 32 == CANONICAL_MAP_SHA256_OFFSET
        && CANONICAL_MAP_SHA256_OFFSET + 32 == EDITION_DIGEST_OFFSET
        && EDITION_DIGEST_OFFSET + 32 == LAYOUT_DIGEST_OFFSET
        && LAYOUT_DIGEST_OFFSET + 32 == DESCRIPTOR_DIGEST_OFFSET
        && DESCRIPTOR_DIGEST_OFFSET + 32 == TAIL_COMPONENTS_OFFSET
        && TAIL_COMPONENTS_OFFSET + TERMINAL_TAIL_COMPONENT_COUNT * TAIL_COMPONENT_LEN
            <= DIAGNOSTIC_LENGTHS_OFFSET
        && DIAGNOSTIC_LENGTHS_OFFSET + 4 <= WRITER_VERSION_OFFSET
        && WRITER_VERSION_OFFSET + WRITER_VERSION_MAX_BYTES == WRITE_TIMESTAMP_OFFSET
        && WRITE_TIMESTAMP_OFFSET + WRITE_TIMESTAMP_MAX_BYTES == HEADER_SHA256_OFFSET
        && HEADER_SHA256_OFFSET + 32 == OBSERVED_TAPE_FILE_OFFSET
        && OBSERVED_TAPE_FILE_OFFSET + size_of::<u64>() == OBSERVED_START_LBA_OFFSET
        && OBSERVED_START_LBA_OFFSET + size_of::<u64>() == OBSERVED_RECORD_COUNT_OFFSET
        && OBSERVED_RECORD_COUNT_OFFSET + size_of::<u64>() == OBSERVED_FOOTER_LBA_OFFSET
        && OBSERVED_FOOTER_LBA_OFFSET + size_of::<u64>() == BACKWARD_START_DELTA_OFFSET
        && BACKWARD_START_DELTA_OFFSET + size_of::<u64>() == FIXED_RESERVED_OFFSET
        && FIXED_RESERVED_OFFSET <= TAPE_INDEX_REPLICA_CRC_OFFSET
        && TAPE_INDEX_REPLICA_CRC_OFFSET + size_of::<u64>() == TAPE_INDEX_REPLICA_FRAME_LEN,
    "every fixed field, reserved region, and CRC must remain ordered inside the replica frame"
);

/// Exact counts that determine the streamed payload size.
pub type TapeIndexReplicaCounts = TapeIndexPayloadCounts;

/// Final pre-tail structural scope.
pub type TapeIndexReplicaScope = TapeIndexPayloadScope;

/// One structural map entry in the streamed payload.
pub type TapeIndexReplicaMapEntry = TapeIndexPayloadMapEntry;

/// Structural file kinds understood by the terminal replica payload.
pub type TapeIndexReplicaFileKind = TapeIndexPayloadFileKind;

/// One Object recovery row in the streamed payload.
pub type TapeIndexReplicaObjectRow = TapeIndexPayloadObjectRow;

/// Replayable hardened authority for a terminal edition.
pub trait TapeIndexReplicaRecordSource {
    /// Visit the complete canonical pre-tail structural map in order.
    fn visit_structural_entries(
        &mut self,
        visitor: &mut dyn FnMut(&TapeIndexReplicaMapEntry) -> Result<(), ParityError>,
    ) -> Result<(), ParityError>;

    /// Visit the complete canonical Object recovery row set in tape order.
    fn visit_object_rows(
        &mut self,
        visitor: &mut dyn FnMut(&TapeIndexReplicaObjectRow) -> Result<(), ParityError>,
    ) -> Result<(), ParityError>;
}

/// Streamed full-block payload source used for independent replica validation.
pub trait TapeIndexReplicaPayloadBlockSource {
    /// Visit all payload records between the already-read header and footer.
    fn visit_payload_blocks(
        &mut self,
        visitor: &mut dyn FnMut(&[u8]) -> Result<(), TapeIndexReplicaError>,
    ) -> Result<(), TapeIndexReplicaError>;
}

/// Checked result from fully decoding and validating one replica payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TapeIndexReplicaPayloadSummary {
    /// Number of decoded structural rows.
    pub structural_entry_count: u64,
    /// Number of decoded Object recovery rows.
    pub object_row_count: u64,
    /// Complete fixed-slot payload digest.
    pub payload_sha256: [u8; 32],
    /// Canonical structural-map digest.
    pub canonical_map_sha256: [u8; 32],
    /// Logical coordinate immediately after the complete pre-tail prefix.
    pub covered_prefix_end_lba: u64,
}

/// Checked record geometry for one replica tape file, excluding its filemark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TapeIndexReplicaLayout {
    /// Exact fixed-slot payload bytes.
    pub payload_len: u64,
    /// Payload-only full records following the header.
    pub payload_record_count: u64,
    /// Zero bytes at the end of the last payload record.
    pub payload_padding_bytes: u64,
    /// Footer record offset from the replica header.
    pub footer_block_offset: u64,
    /// Header + payload records + footer.
    pub replica_record_count: u64,
}

/// Common final edition facts shared by A, B, and C.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapeIndexEditionDescriptor {
    /// Tape identity.
    pub tape_uuid: [u8; 16],
    /// Opaque nonzero final edition identity.
    pub edition_id: [u8; 16],
    /// Monotonic final edition sequence.
    pub edition_sequence: u64,
    /// Final pre-tail map scope.
    pub scope: TapeIndexReplicaScope,
    /// Structural/Object row counts.
    pub counts: TapeIndexReplicaCounts,
    /// Fixed record size.
    pub block_size: u32,
    /// Must be false for the terminal-index profile.
    pub compression_enabled: bool,
    /// Bounded printable writer identity.
    pub writer_version: String,
    /// Bounded RFC3339 timestamp.
    pub write_timestamp: String,
    /// Complete planned A/gap/B/gap/C layout.
    pub terminal_layout: TerminalTailLayout,
}

/// Immutable common edition plan after one validation/hash replay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapeIndexEditionPlan {
    /// Common descriptor.
    pub descriptor: TapeIndexEditionDescriptor,
    /// Exact one-replica geometry.
    pub replica_layout: TapeIndexReplicaLayout,
    /// Domain-separated digest of every complete fixed slot.
    pub payload_sha256: [u8; 32],
    /// Existing canonical structural-map digest.
    pub canonical_map_sha256: [u8; 32],
    /// Common digest over the final scope and payload facts.
    pub edition_digest: [u8; 32],
    /// Digest of the complete planned five-file layout.
    pub layout_digest: [u8; 32],
}

/// One replica-local immutable plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapeIndexReplicaPlan {
    /// Common edition plan.
    pub edition: TapeIndexEditionPlan,
    /// One-based A/B/C ordinal.
    pub replica_ordinal: u16,
    /// Planned local component.
    pub component: TerminalTailComponentPlan,
    /// Digest binding this ordinal/location to the common edition/layout.
    pub descriptor_digest: [u8; 32],
}

/// Placement values measured locally before/footer emission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TapeIndexReplicaObservation {
    /// Dense tape-file number.
    pub tape_file_number: u64,
    /// Physical start LBA.
    pub start_lba: u64,
    /// Header + payload + footer records.
    pub record_count: u64,
}

/// Parsed header frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapeIndexReplicaHeader {
    /// Checked replica plan encoded by the header.
    pub plan: TapeIndexReplicaPlan,
    /// SHA-256 over the complete zero-padded header record.
    pub header_sha256: [u8; 32],
}

/// Parsed Object-count-independent local bootstrap/footer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapeIndexBootstrapFooter {
    /// Checked replica plan repeated by the footer.
    pub plan: TapeIndexReplicaPlan,
    /// Exact local header-record hash.
    pub header_sha256: [u8; 32],
    /// Locally observed placement.
    pub observation: TapeIndexReplicaObservation,
    /// Observed footer LBA.
    pub observed_footer_lba: u64,
    /// Backward distance from footer to header.
    pub backward_start_delta: u64,
}

/// Typed terminal-replica framing/planning failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TapeIndexReplicaError {
    /// Planned five-file layout is invalid.
    #[error("invalid terminal layout for tape index: {0}")]
    Layout(#[from] TerminalTailLayoutError),
    /// Payload source or canonical fixed-slot validation failed.
    #[error("terminal index payload authority failed: {message}")]
    Payload {
        /// Underlying bounded detail.
        message: String,
    },
    /// A final edition must contain the BOT Bootstrap structural row.
    #[error("terminal tape-index final prefix has zero structural rows")]
    EmptyPrefix,
    /// Hardware compression must be disabled.
    #[error("terminal tape index requires hardware compression disabled")]
    CompressionEnabled,
    /// Edition ID cannot be all zero.
    #[error("terminal tape-index edition ID is zero")]
    ZeroEditionId,
    /// Edition sequence cannot be zero.
    #[error("terminal tape-index edition sequence is zero")]
    ZeroEditionSequence,
    /// Replica ordinal is outside A/B/C.
    #[error("invalid tape-index replica ordinal {ordinal}; expected 1, 2, or 3")]
    InvalidReplicaOrdinal {
        /// Supplied ordinal.
        ordinal: u16,
    },
    /// Tail plan and final prefix disagree.
    #[error("terminal index {field} {actual}, expected {expected}")]
    PlanMismatch {
        /// Mismatched field.
        field: &'static str,
        /// Expected value.
        expected: u64,
        /// Actual value.
        actual: u64,
    },
    /// Checked arithmetic overflowed.
    #[error("terminal tape-index arithmetic overflow: {context}")]
    ArithmeticOverflow {
        /// Failed operation.
        context: &'static str,
    },
    /// Header/footer magic does not match the tape-domain role.
    #[error("tape-index replica {frame} magic mismatch")]
    MagicMismatch {
        /// Header or footer.
        frame: &'static str,
    },
    /// Schema version is unsupported.
    #[error("unsupported tape-index replica schema version {version}")]
    UnsupportedVersion {
        /// Encoded version.
        version: u16,
    },
    /// Encoded tape identity differs from the expected tape.
    #[error("tape-index replica tape UUID mismatch")]
    WrongTape,
    /// Full block or meaningful frame length is wrong.
    #[error("tape-index replica {frame} length {actual}, expected {expected}")]
    WrongLength {
        /// Header/footer/encoded.
        frame: &'static str,
        /// Required bytes.
        expected: usize,
        /// Actual bytes.
        actual: usize,
    },
    /// Full record length is not one of the closed profile sizes.
    #[error("tape-index replica {frame} has unsupported record length {actual}")]
    UnsupportedRecordLength {
        /// Header, footer, or payload record.
        frame: &'static str,
        /// Supplied bytes.
        actual: usize,
    },
    /// Frame CRC does not match.
    #[error("tape-index replica {frame} CRC mismatch")]
    CrcMismatch {
        /// Header or footer.
        frame: &'static str,
    },
    /// A semantic or cryptographic binding failed.
    #[error("tape-index replica {field} mismatch")]
    DigestMismatch {
        /// Binding name.
        field: &'static str,
    },
    /// Reserved bytes are nonzero.
    #[error("tape-index replica {field} has nonzero byte 0x{byte:02x} at offset {offset}")]
    ReservedNonzero {
        /// Reserved region.
        field: &'static str,
        /// Relative offset.
        offset: usize,
        /// Unexpected byte.
        byte: u8,
    },
    /// Observed placement differs from the precommitted plan.
    #[error("tape-index observed {field} {actual}, planned {planned}")]
    ObservationMismatch {
        /// Placement field.
        field: &'static str,
        /// Planned value.
        planned: u64,
        /// Observed value.
        actual: u64,
    },
    /// Writer callback failed after possible media motion.
    #[error("tape-index replica emission failed: {message}")]
    Emit {
        /// Underlying detail.
        message: String,
    },
}

/// Exact checked payload length, without collecting rows.
pub fn checked_tape_index_payload_len(
    counts: TapeIndexReplicaCounts,
) -> Result<u64, TapeIndexReplicaError> {
    checked_tape_index_payload_byte_len(counts).map_err(payload_error)
}

/// Exact full-header + payload-records + full-footer geometry.
pub fn checked_tape_index_replica_layout(
    block_size: u32,
    counts: TapeIndexReplicaCounts,
) -> Result<TapeIndexReplicaLayout, TapeIndexReplicaError> {
    validate_terminal_index_block_size(block_size)?;
    checked_tape_index_replica_layout_after_block_size(block_size, counts)
}

fn checked_tape_index_replica_layout_hinted(
    block_size: u32,
    counts: TapeIndexReplicaCounts,
) -> Result<TapeIndexReplicaLayout, TapeIndexReplicaError> {
    validate_terminal_index_block_size_hint(block_size)?;
    checked_tape_index_replica_layout_after_block_size(block_size, counts)
}

fn checked_tape_index_replica_layout_after_block_size(
    block_size: u32,
    counts: TapeIndexReplicaCounts,
) -> Result<TapeIndexReplicaLayout, TapeIndexReplicaError> {
    let payload_len = checked_tape_index_payload_len(counts)?;
    let block_size_u64 = u64::from(block_size);
    let payload_record_count = if payload_len == 0 {
        0
    } else {
        payload_len.checked_add(block_size_u64 - 1).ok_or(
            TapeIndexReplicaError::ArithmeticOverflow {
                context: "payload record ceiling division",
            },
        )? / block_size_u64
    };
    let payload_capacity = payload_record_count.checked_mul(block_size_u64).ok_or(
        TapeIndexReplicaError::ArithmeticOverflow {
            context: "payload record byte capacity",
        },
    )?;
    let payload_padding_bytes = payload_capacity.checked_sub(payload_len).ok_or(
        TapeIndexReplicaError::ArithmeticOverflow {
            context: "payload padding subtraction",
        },
    )?;
    let footer_block_offset =
        payload_record_count
            .checked_add(1)
            .ok_or(TapeIndexReplicaError::ArithmeticOverflow {
                context: "footer block offset",
            })?;
    let replica_record_count =
        footer_block_offset
            .checked_add(1)
            .ok_or(TapeIndexReplicaError::ArithmeticOverflow {
                context: "replica record count",
            })?;
    Ok(TapeIndexReplicaLayout {
        payload_len,
        payload_record_count,
        payload_padding_bytes,
        footer_block_offset,
        replica_record_count,
    })
}

/// Validate/hash one complete replayable final edition.
pub fn plan_tape_index_edition<S: TapeIndexReplicaRecordSource + ?Sized>(
    descriptor: TapeIndexEditionDescriptor,
    source: &mut S,
) -> Result<TapeIndexEditionPlan, TapeIndexReplicaError> {
    validate_edition_descriptor(&descriptor)?;
    let replica_layout =
        checked_tape_index_replica_layout(descriptor.block_size, descriptor.counts)?;
    validate_tail_scope(&descriptor, replica_layout)?;
    let payload_descriptor = payload_descriptor(&descriptor);
    let mut payload_hasher = Sha256::new();
    payload_hasher.update(TAPE_INDEX_REPLICA_PAYLOAD_DOMAIN);
    let mut source = PreTailRecordSource { inner: source };
    let summary = stream_tape_index_payload(&mut source, &payload_descriptor, |slot| {
        payload_hasher.update(slot);
        Ok(())
    })
    .map_err(payload_error)?;
    if summary.payload_len != replica_layout.payload_len {
        return Err(TapeIndexReplicaError::PlanMismatch {
            field: "payload_len",
            expected: replica_layout.payload_len,
            actual: summary.payload_len,
        });
    }
    let first_replica = descriptor.terminal_layout.replica(1)?;
    if summary.covered_prefix_end_lba != first_replica.planned_start_lba {
        return Err(TapeIndexReplicaError::PlanMismatch {
            field: "covered_prefix_end_lba",
            expected: first_replica.planned_start_lba,
            actual: summary.covered_prefix_end_lba,
        });
    }
    let payload_sha256 = payload_hasher.finalize().into();
    let layout_digest = descriptor.terminal_layout.digest()?;
    let edition_digest = edition_digest(
        &descriptor,
        replica_layout,
        payload_sha256,
        summary.canonical_map_sha256,
    );
    Ok(TapeIndexEditionPlan {
        descriptor,
        replica_layout,
        payload_sha256,
        canonical_map_sha256: summary.canonical_map_sha256,
        edition_digest,
        layout_digest,
    })
}

/// Bind one local A/B/C replica to a common edition plan.
pub fn plan_tape_index_replica(
    edition: TapeIndexEditionPlan,
    replica_ordinal: u16,
) -> Result<TapeIndexReplicaPlan, TapeIndexReplicaError> {
    validate_edition_plan(&edition)?;
    plan_tape_index_replica_after_validation(edition, replica_ordinal)
}

fn plan_tape_index_replica_hinted(
    edition: TapeIndexEditionPlan,
    replica_ordinal: u16,
) -> Result<TapeIndexReplicaPlan, TapeIndexReplicaError> {
    validate_edition_plan_hinted(&edition)?;
    plan_tape_index_replica_after_validation(edition, replica_ordinal)
}

fn plan_tape_index_replica_after_validation(
    edition: TapeIndexEditionPlan,
    replica_ordinal: u16,
) -> Result<TapeIndexReplicaPlan, TapeIndexReplicaError> {
    if !(1..=TERMINAL_INDEX_REPLICA_COUNT).contains(&replica_ordinal) {
        return Err(TapeIndexReplicaError::InvalidReplicaOrdinal {
            ordinal: replica_ordinal,
        });
    }
    let component = edition
        .descriptor
        .terminal_layout
        .replica(replica_ordinal)?;
    if component.record_count != edition.replica_layout.replica_record_count {
        return Err(TapeIndexReplicaError::PlanMismatch {
            field: "replica_record_count",
            expected: edition.replica_layout.replica_record_count,
            actual: component.record_count,
        });
    }
    let descriptor_digest = replica_descriptor_digest(&edition, replica_ordinal, component);
    Ok(TapeIndexReplicaPlan {
        edition,
        replica_ordinal,
        component,
        descriptor_digest,
    })
}

/// Encode one complete zero-padded header record.
pub fn encode_tape_index_replica_header(
    plan: &TapeIndexReplicaPlan,
) -> Result<Vec<u8>, TapeIndexReplicaError> {
    validate_replica_plan(plan)?;
    let mut block =
        vec![0u8; validate_terminal_index_block_size(plan.edition.descriptor.block_size,)?];
    write_replica_frame(
        &mut block,
        plan,
        derive_tape_index_replica_header_magic(&plan.edition.descriptor.tape_uuid),
        TAPE_INDEX_REPLICA_HEADER_ROLE,
    )?;
    write_crc(&mut block);
    Ok(block)
}

/// Encode one complete Object-count-independent footer record.
pub fn encode_tape_index_bootstrap_footer(
    plan: &TapeIndexReplicaPlan,
    header_sha256: [u8; 32],
    observation: TapeIndexReplicaObservation,
) -> Result<Vec<u8>, TapeIndexReplicaError> {
    validate_replica_plan(plan)?;
    validate_observation(plan, observation)?;
    let mut block =
        vec![0u8; validate_terminal_index_block_size(plan.edition.descriptor.block_size,)?];
    write_replica_frame(
        &mut block,
        plan,
        derive_tape_index_replica_footer_magic(&plan.edition.descriptor.tape_uuid),
        TAPE_INDEX_REPLICA_FOOTER_ROLE,
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
    let delta = observation.record_count.checked_sub(1).ok_or(
        TapeIndexReplicaError::ArithmeticOverflow {
            context: "footer backward delta",
        },
    )?;
    let footer_lba = observation.start_lba.checked_add(delta).ok_or(
        TapeIndexReplicaError::ArithmeticOverflow {
            context: "observed footer LBA",
        },
    )?;
    write_u64(&mut block, OBSERVED_FOOTER_LBA_OFFSET, footer_lba);
    write_u64(&mut block, BACKWARD_START_DELTA_OFFSET, delta);
    write_crc(&mut block);
    Ok(block)
}

/// Stream one replica only; caller owns its filemark, barrier, and journal step.
pub fn write_tape_index_replica<S, F>(
    plan: &TapeIndexReplicaPlan,
    observation: TapeIndexReplicaObservation,
    source: &mut S,
    mut emit_block: F,
) -> Result<(), TapeIndexReplicaError>
where
    S: TapeIndexReplicaRecordSource + ?Sized,
    F: FnMut(&[u8]) -> Result<(), ParityError>,
{
    validate_replica_plan(plan)?;
    validate_observation(plan, observation)?;
    let header = encode_tape_index_replica_header(plan)?;
    let header_sha256: [u8; 32] = Sha256::digest(&header).into();
    emit_block(&header).map_err(|error| TapeIndexReplicaError::Emit {
        message: error.to_string(),
    })?;

    let mut writer = ReplicaPayloadBlockWriter::new(
        plan.edition.descriptor.block_size,
        plan.edition.replica_layout.payload_record_count,
        plan.edition.replica_layout.payload_len,
        &mut emit_block,
    )?;
    let payload_descriptor = payload_descriptor(&plan.edition.descriptor);
    let mut payload_hasher = Sha256::new();
    payload_hasher.update(TAPE_INDEX_REPLICA_PAYLOAD_DOMAIN);
    let mut source = PreTailRecordSource { inner: source };
    let summary = stream_tape_index_payload(&mut source, &payload_descriptor, |slot| {
        payload_hasher.update(slot);
        writer.write_bytes(slot)
    })
    .map_err(payload_error)?;
    let emitted_payload_records = writer.finish()?;
    if emitted_payload_records != plan.edition.replica_layout.payload_record_count
        || summary.payload_len != plan.edition.replica_layout.payload_len
        || summary.canonical_map_sha256 != plan.edition.canonical_map_sha256
        || summary.covered_prefix_end_lba != plan.component_for_a()?.planned_start_lba
        || <[u8; 32]>::from(payload_hasher.finalize()) != plan.edition.payload_sha256
    {
        return Err(TapeIndexReplicaError::DigestMismatch {
            field: "replayed payload authority",
        });
    }

    let footer = encode_tape_index_bootstrap_footer(plan, header_sha256, observation)?;
    emit_block(&footer).map_err(|error| TapeIndexReplicaError::Emit {
        message: error.to_string(),
    })?;
    Ok(())
}

struct PreTailRecordSource<'a, S: TapeIndexReplicaRecordSource + ?Sized> {
    inner: &'a mut S,
}

impl<S: TapeIndexReplicaRecordSource + ?Sized> TapeIndexPayloadRecordSource
    for PreTailRecordSource<'_, S>
{
    fn visit_structural_entries(
        &mut self,
        visitor: &mut dyn FnMut(&TapeIndexPayloadMapEntry) -> Result<(), ParityError>,
    ) -> Result<(), ParityError> {
        let mut entry_count = 0u64;
        self.inner.visit_structural_entries(&mut |entry| {
            if entry_count == 0 && entry.kind != TapeIndexPayloadFileKind::Bootstrap {
                return Err(ParityError::TapeIndexReplica(
                    "terminal pre-A payload does not begin with the BOT Bootstrap".to_string(),
                ));
            }
            if matches!(
                entry.kind,
                TapeIndexPayloadFileKind::TapeIndexReplica
                    | TapeIndexPayloadFileKind::IndexSeparationExtent
            ) {
                return Err(ParityError::TapeIndexReplica(format!(
                    "terminal structural kind {:?} is forbidden in the pre-A payload",
                    entry.kind
                )));
            }
            visitor(entry)?;
            entry_count = entry_count.checked_add(1).ok_or_else(|| {
                ParityError::TapeIndexReplica(
                    "terminal pre-A structural count overflows u64".to_string(),
                )
            })?;
            Ok(())
        })?;
        if entry_count == 0 {
            return Err(ParityError::TapeIndexReplica(
                "terminal pre-A payload is empty; BOT Bootstrap is required".to_string(),
            ));
        }
        Ok(())
    }

    fn visit_object_rows(
        &mut self,
        visitor: &mut dyn FnMut(&TapeIndexPayloadObjectRow) -> Result<(), ParityError>,
    ) -> Result<(), ParityError> {
        self.inner.visit_object_rows(visitor)
    }
}

/// Parse one full header record after role-magic classification.
pub fn parse_tape_index_replica_header(
    block: &[u8],
    expected_tape_uuid: &[u8; 16],
) -> Result<TapeIndexReplicaHeader, TapeIndexReplicaError> {
    let expected_magic = derive_tape_index_replica_header_magic(expected_tape_uuid);
    if block.len() < SCHEMA_VERSION_OFFSET
        || block[MAGIC_OFFSET..SCHEMA_VERSION_OFFSET] != expected_magic
    {
        return Err(TapeIndexReplicaError::MagicMismatch { frame: "header" });
    }
    let plan = parse_replica_frame(
        block,
        expected_tape_uuid,
        "header",
        TAPE_INDEX_REPLICA_HEADER_ROLE,
    )?;
    Ok(TapeIndexReplicaHeader {
        plan,
        header_sha256: Sha256::digest(block).into(),
    })
}

/// Parse one full local BootstrapFooter record.
pub fn parse_tape_index_bootstrap_footer(
    block: &[u8],
    expected_tape_uuid: &[u8; 16],
) -> Result<TapeIndexBootstrapFooter, TapeIndexReplicaError> {
    let expected_magic = derive_tape_index_replica_footer_magic(expected_tape_uuid);
    if block.len() < SCHEMA_VERSION_OFFSET
        || block[MAGIC_OFFSET..SCHEMA_VERSION_OFFSET] != expected_magic
    {
        return Err(TapeIndexReplicaError::MagicMismatch { frame: "footer" });
    }
    let plan = parse_replica_frame(
        block,
        expected_tape_uuid,
        "footer",
        TAPE_INDEX_REPLICA_FOOTER_ROLE,
    )?;
    let mut header_sha256 = [0u8; 32];
    header_sha256.copy_from_slice(&block[HEADER_SHA256_OFFSET..HEADER_SHA256_OFFSET + 32]);
    let observation = TapeIndexReplicaObservation {
        tape_file_number: read_u64(block, OBSERVED_TAPE_FILE_OFFSET),
        start_lba: read_u64(block, OBSERVED_START_LBA_OFFSET),
        record_count: read_u64(block, OBSERVED_RECORD_COUNT_OFFSET),
    };
    validate_observation(&plan, observation)?;
    let observed_footer_lba = read_u64(block, OBSERVED_FOOTER_LBA_OFFSET);
    let backward_start_delta = read_u64(block, BACKWARD_START_DELTA_OFFSET);
    let expected_delta = observation.record_count - 1;
    if backward_start_delta != expected_delta {
        return Err(TapeIndexReplicaError::ObservationMismatch {
            field: "backward_start_delta",
            planned: expected_delta,
            actual: backward_start_delta,
        });
    }
    let expected_footer_lba = observation.start_lba.checked_add(expected_delta).ok_or(
        TapeIndexReplicaError::ArithmeticOverflow {
            context: "parsed footer LBA",
        },
    )?;
    if observed_footer_lba != expected_footer_lba {
        return Err(TapeIndexReplicaError::ObservationMismatch {
            field: "footer_lba",
            planned: expected_footer_lba,
            actual: observed_footer_lba,
        });
    }
    Ok(TapeIndexBootstrapFooter {
        plan,
        header_sha256,
        observation,
        observed_footer_lba,
        backward_start_delta,
    })
}

/// Bind a parsed footer to its exact local header and descriptor.
pub fn validate_tape_index_replica_pair(
    header: &TapeIndexReplicaHeader,
    footer: &TapeIndexBootstrapFooter,
) -> Result<(), TapeIndexReplicaError> {
    if header.plan != footer.plan {
        return Err(TapeIndexReplicaError::DigestMismatch {
            field: "header/footer descriptor",
        });
    }
    if header.header_sha256 != footer.header_sha256 {
        return Err(TapeIndexReplicaError::DigestMismatch {
            field: "local header hash",
        });
    }
    Ok(())
}

/// Fully validate one replica, including every payload slot and record padding.
///
/// Header/footer pairing is the fast structural check. This full check streams
/// the intervening records, emits decoded inventory rows to bounded visitors,
/// and proves the payload/map digests and complete pre-tail shape.
pub fn validate_tape_index_replica_payload<S, FE, FR>(
    header: &TapeIndexReplicaHeader,
    footer: &TapeIndexBootstrapFooter,
    source: &mut S,
    mut visit_entry: FE,
    mut visit_row: FR,
) -> Result<TapeIndexReplicaPayloadSummary, TapeIndexReplicaError>
where
    S: TapeIndexReplicaPayloadBlockSource + ?Sized,
    FE: FnMut(&TapeIndexReplicaMapEntry) -> Result<(), TapeIndexReplicaError>,
    FR: FnMut(&TapeIndexReplicaObjectRow) -> Result<(), TapeIndexReplicaError>,
{
    validate_tape_index_replica_pair(header, footer)?;
    let plan = &header.plan;
    let descriptor = &plan.edition.descriptor;
    let layout = plan.edition.replica_layout;
    let block_size = validate_terminal_index_block_size_hint(descriptor.block_size)?;

    let mut payload_hasher = Sha256::new();
    payload_hasher.update(TAPE_INDEX_REPLICA_PAYLOAD_DOMAIN);
    let mut map_hasher = Sha256::new();
    map_hasher.update(encode_cbor_array_header(
        descriptor.counts.structural_entry_count,
    ));
    let mut map_locator_hasher = Sha256::new();
    let mut row_locator_hasher = Sha256::new();
    let mut payload_record_count = 0u64;
    let mut payload_bytes = 0u64;
    let mut structural_count = 0u64;
    let mut object_map_count = 0u64;
    let mut row_count = 0u64;
    let mut expected_tape_file_number = 0u64;
    let mut expected_data_ordinal = 0u64;
    let mut expected_protected_ordinal = 0u64;
    let mut expected_epoch_id = 0u64;
    let mut covered_prefix_end_lba = 0u64;
    let mut final_parity_map_seen = false;
    let mut sidecar_seen = false;
    let mut previous_row_file_number = None;
    let mut slot_buffer = Vec::with_capacity(TAPE_INDEX_PAYLOAD_OBJECT_ROW_SLOT_LEN as usize);

    source.visit_payload_blocks(&mut |block| {
        if payload_record_count >= layout.payload_record_count {
            return Err(TapeIndexReplicaError::Payload {
                message: format!(
                    "payload source yielded more than {} records",
                    layout.payload_record_count
                ),
            });
        }
        if block.len() != block_size {
            return Err(TapeIndexReplicaError::WrongLength {
                frame: "payload record",
                expected: block_size,
                actual: block.len(),
            });
        }
        payload_record_count = payload_record_count.checked_add(1).ok_or(
            TapeIndexReplicaError::ArithmeticOverflow {
                context: "validated payload record count",
            },
        )?;
        let remaining = layout.payload_len.checked_sub(payload_bytes).ok_or(
            TapeIndexReplicaError::ArithmeticOverflow {
                context: "remaining payload bytes",
            },
        )?;
        let logical_len = usize::try_from(remaining.min(block_size as u64)).map_err(|_| {
            TapeIndexReplicaError::ArithmeticOverflow {
                context: "logical payload bytes in record",
            }
        })?;
        let mut offset = 0usize;
        while offset < logical_len {
            let structural = structural_count < descriptor.counts.structural_entry_count;
            let slot_len = usize::try_from(if structural {
                TAPE_INDEX_PAYLOAD_STRUCTURAL_SLOT_LEN
            } else {
                TAPE_INDEX_PAYLOAD_OBJECT_ROW_SLOT_LEN
            })
            .map_err(|_| TapeIndexReplicaError::ArithmeticOverflow {
                context: "payload slot length",
            })?;
            let needed = slot_len.checked_sub(slot_buffer.len()).ok_or(
                TapeIndexReplicaError::ArithmeticOverflow {
                    context: "remaining payload slot bytes",
                },
            )?;
            let take = needed.min(logical_len - offset);
            let take_end = offset.checked_add(take).ok_or(
                TapeIndexReplicaError::ArithmeticOverflow {
                    context: "payload fragment end",
                },
            )?;
            slot_buffer.extend_from_slice(&block[offset..take_end]);
            offset = take_end;
            if slot_buffer.len() != slot_len {
                continue;
            }
            let slot = slot_buffer.as_slice();
            payload_hasher.update(slot);
            if structural {
                let entry =
                    decode_tape_index_payload_map_entry_slot(slot).map_err(payload_error)?;
                if matches!(
                    entry.kind,
                    TapeIndexPayloadFileKind::TapeIndexReplica
                        | TapeIndexPayloadFileKind::IndexSeparationExtent
                ) {
                    return Err(TapeIndexReplicaError::Payload {
                        message: format!(
                            "terminal structural kind {:?} is forbidden in the pre-A payload",
                            entry.kind
                        ),
                    });
                }
                if entry.tape_file_number != expected_tape_file_number {
                    return Err(TapeIndexReplicaError::Payload {
                        message: format!(
                            "structural entry {} is not dense; expected {expected_tape_file_number}",
                            entry.tape_file_number
                        ),
                    });
                }
                if expected_tape_file_number == 0
                    && entry.kind != TapeIndexPayloadFileKind::Bootstrap
                {
                    return Err(TapeIndexReplicaError::Payload {
                        message: "terminal pre-A payload does not begin with the BOT Bootstrap"
                            .to_string(),
                    });
                }
                if expected_tape_file_number != 0
                    && entry.kind == TapeIndexPayloadFileKind::Bootstrap
                {
                    return Err(TapeIndexReplicaError::Payload {
                        message: "terminal pre-A payload contains a Bootstrap outside tape file 0"
                            .to_string(),
                    });
                }
                if final_parity_map_seen {
                    return Err(TapeIndexReplicaError::Payload {
                        message: "terminal pre-A ParityMap must be the final structural entry"
                            .to_string(),
                    });
                }
                if entry.kind == TapeIndexPayloadFileKind::ParityMap {
                    final_parity_map_seen = true;
                }
                expected_tape_file_number = expected_tape_file_number.checked_add(1).ok_or(
                    TapeIndexReplicaError::ArithmeticOverflow {
                        context: "structural tape-file sequence",
                    },
                )?;
                covered_prefix_end_lba = covered_prefix_end_lba
                    .checked_add(entry.block_count)
                    .and_then(|value| value.checked_add(1))
                    .ok_or(TapeIndexReplicaError::ArithmeticOverflow {
                        context: "covered prefix records plus filemarks",
                    })?;
                match entry.kind {
                    TapeIndexPayloadFileKind::Object => {
                        let first = entry.first_parity_data_ordinal.ok_or_else(|| {
                            TapeIndexReplicaError::Payload {
                                message: "Object structural row is missing its first data ordinal"
                                    .to_string(),
                            }
                        })?;
                        if first != expected_data_ordinal {
                            return Err(TapeIndexReplicaError::Payload {
                                message: format!(
                                    "Object file {} starts at ordinal {first}, expected {expected_data_ordinal}",
                                    entry.tape_file_number
                                ),
                            });
                        }
                        expected_data_ordinal = expected_data_ordinal
                            .checked_add(entry.block_count)
                            .ok_or(TapeIndexReplicaError::ArithmeticOverflow {
                                context: "Object data ordinal range",
                            })?;
                        object_map_count = object_map_count.checked_add(1).ok_or(
                            TapeIndexReplicaError::ArithmeticOverflow {
                                context: "Object structural count",
                            },
                        )?;
                        update_locator_digest(
                            &mut map_locator_hasher,
                            entry.tape_file_number,
                            entry.block_count,
                        );
                    }
                    TapeIndexPayloadFileKind::ParitySidecar => {
                        sidecar_seen = true;
                        let start = entry.protected_ordinal_start.ok_or_else(|| {
                            TapeIndexReplicaError::Payload {
                                message: "sidecar structural row is missing protected start"
                                    .to_string(),
                            }
                        })?;
                        let end = entry.protected_ordinal_end_exclusive.ok_or_else(|| {
                            TapeIndexReplicaError::Payload {
                                message: "sidecar structural row is missing protected end"
                                    .to_string(),
                            }
                        })?;
                        if start != expected_protected_ordinal
                            || entry.epoch_id != Some(expected_epoch_id)
                        {
                            return Err(TapeIndexReplicaError::Payload {
                                message: format!(
                                    "sidecar file {} range/epoch is not the next canonical value",
                                    entry.tape_file_number
                                ),
                            });
                        }
                        expected_protected_ordinal = end;
                        expected_epoch_id = expected_epoch_id.checked_add(1).ok_or(
                            TapeIndexReplicaError::ArithmeticOverflow {
                                context: "sidecar epoch sequence",
                            },
                        )?;
                    }
                    TapeIndexPayloadFileKind::Bootstrap
                    | TapeIndexPayloadFileKind::ParityMap => {}
                    TapeIndexPayloadFileKind::TapeIndexReplica
                    | TapeIndexPayloadFileKind::IndexSeparationExtent => unreachable!(
                        "terminal kinds are rejected before structural validation"
                    ),
                }
                let encoded_len = usize::from(u16::from_le_bytes(
                    slot[..TAPE_INDEX_PAYLOAD_SLOT_PREFIX_LEN]
                        .try_into()
                        .expect("decoded slot prefix is bounded"),
                ));
                map_hasher.update(
                    &slot[TAPE_INDEX_PAYLOAD_SLOT_PREFIX_LEN
                        ..TAPE_INDEX_PAYLOAD_SLOT_PREFIX_LEN + encoded_len],
                );
                visit_entry(&entry)?;
                structural_count = structural_count.checked_add(1).ok_or(
                    TapeIndexReplicaError::ArithmeticOverflow {
                        context: "decoded structural count",
                    },
                )?;
            } else {
                if row_count >= descriptor.counts.object_row_count {
                    return Err(TapeIndexReplicaError::Payload {
                        message: "payload contains bytes beyond declared fixed slots".to_string(),
                    });
                }
                let row = decode_tape_index_payload_object_row_slot(slot, descriptor.block_size)
                    .map_err(payload_error)?;
                if previous_row_file_number
                    .is_some_and(|previous| row.tape_file_number <= previous)
                {
                    return Err(TapeIndexReplicaError::Payload {
                        message: "Object recovery rows are not strictly increasing".to_string(),
                    });
                }
                previous_row_file_number = Some(row.tape_file_number);
                update_locator_digest(
                    &mut row_locator_hasher,
                    row.tape_file_number,
                    row.stored_block_count,
                );
                visit_row(&row)?;
                row_count = row_count.checked_add(1).ok_or(
                    TapeIndexReplicaError::ArithmeticOverflow {
                        context: "decoded Object row count",
                    },
                )?;
            }
            slot_buffer.clear();
        }
        if let Some((relative, byte)) = block[logical_len..]
            .iter()
            .copied()
            .enumerate()
            .find(|(_, byte)| *byte != 0)
        {
            return Err(TapeIndexReplicaError::ReservedNonzero {
                field: "payload record padding",
                offset: logical_len + relative,
                byte,
            });
        }
        payload_bytes = payload_bytes.checked_add(logical_len as u64).ok_or(
            TapeIndexReplicaError::ArithmeticOverflow {
                context: "validated payload bytes",
            },
        )?;
        Ok(())
    })?;

    if !slot_buffer.is_empty() {
        return Err(TapeIndexReplicaError::Payload {
            message: format!(
                "payload ended with {} bytes of an incomplete fixed slot",
                slot_buffer.len()
            ),
        });
    }
    if structural_count == 0 {
        return Err(TapeIndexReplicaError::Payload {
            message: "terminal pre-A payload is empty; BOT Bootstrap is required".to_string(),
        });
    }
    if sidecar_seen != final_parity_map_seen {
        return Err(TapeIndexReplicaError::Payload {
            message: "terminal pre-A payload requires exactly one final ParityMap iff a ParitySidecar is present".to_string(),
        });
    }
    if sidecar_seen && expected_protected_ordinal != expected_data_ordinal {
        return Err(TapeIndexReplicaError::Payload {
            message: "terminal parity closeout must protect every Object ordinal before replica A"
                .to_string(),
        });
    }

    for (field, expected, actual) in [
        (
            "payload record count",
            layout.payload_record_count,
            payload_record_count,
        ),
        ("payload length", layout.payload_len, payload_bytes),
        (
            "structural entry count",
            descriptor.counts.structural_entry_count,
            structural_count,
        ),
        (
            "Object row count",
            descriptor.counts.object_row_count,
            row_count,
        ),
        (
            "total data ordinals",
            descriptor.scope.total_data_ordinals,
            expected_data_ordinal,
        ),
        (
            "highest protected ordinal",
            descriptor.scope.highest_protected_ordinal,
            expected_protected_ordinal,
        ),
    ] {
        if actual != expected {
            return Err(TapeIndexReplicaError::PlanMismatch {
                field,
                expected,
                actual,
            });
        }
    }
    if object_map_count != row_count
        || map_locator_hasher.finalize() != row_locator_hasher.finalize()
    {
        return Err(TapeIndexReplicaError::Payload {
            message: "structural Object rows and recovery rows are not a bijection".to_string(),
        });
    }
    let payload_sha256: [u8; 32] = payload_hasher.finalize().into();
    let canonical_map_sha256: [u8; 32] = map_hasher.finalize().into();
    if payload_sha256 != plan.edition.payload_sha256 {
        return Err(TapeIndexReplicaError::DigestMismatch {
            field: "payload SHA-256",
        });
    }
    if canonical_map_sha256 != plan.edition.canonical_map_sha256 {
        return Err(TapeIndexReplicaError::DigestMismatch {
            field: "canonical map SHA-256",
        });
    }
    let planned_start = plan.component_for_a()?.planned_start_lba;
    if covered_prefix_end_lba != planned_start {
        return Err(TapeIndexReplicaError::PlanMismatch {
            field: "covered_prefix_end_lba",
            expected: planned_start,
            actual: covered_prefix_end_lba,
        });
    }
    Ok(TapeIndexReplicaPayloadSummary {
        structural_entry_count: structural_count,
        object_row_count: row_count,
        payload_sha256,
        canonical_map_sha256,
        covered_prefix_end_lba,
    })
}

/// Derive tape-domain header magic; classification only, not authentication.
pub fn derive_tape_index_replica_header_magic(tape_uuid: &[u8; 16]) -> [u8; 8] {
    derive_magic(tape_uuid, TAPE_INDEX_REPLICA_HEADER_MAGIC_MESSAGE)
}

/// Derive tape-domain footer magic; classification only, not authentication.
pub fn derive_tape_index_replica_footer_magic(tape_uuid: &[u8; 16]) -> [u8; 8] {
    derive_magic(tape_uuid, TAPE_INDEX_REPLICA_FOOTER_MAGIC_MESSAGE)
}

impl TapeIndexReplicaPlan {
    fn component_for_a(&self) -> Result<TerminalTailComponentPlan, TapeIndexReplicaError> {
        Ok(self.edition.descriptor.terminal_layout.replica(1)?)
    }
}

fn validate_edition_descriptor(
    descriptor: &TapeIndexEditionDescriptor,
) -> Result<(), TapeIndexReplicaError> {
    validate_terminal_index_block_size(descriptor.block_size)?;
    validate_edition_descriptor_after_block_size(descriptor)
}

fn validate_edition_descriptor_hinted(
    descriptor: &TapeIndexEditionDescriptor,
) -> Result<(), TapeIndexReplicaError> {
    validate_terminal_index_block_size_hint(descriptor.block_size)?;
    validate_edition_descriptor_after_block_size(descriptor)
}

fn validate_edition_descriptor_after_block_size(
    descriptor: &TapeIndexEditionDescriptor,
) -> Result<(), TapeIndexReplicaError> {
    descriptor.terminal_layout.validate()?;
    if descriptor.terminal_layout.block_size != descriptor.block_size {
        return Err(TapeIndexReplicaError::PlanMismatch {
            field: "terminal_layout.block_size",
            expected: u64::from(descriptor.block_size),
            actual: u64::from(descriptor.terminal_layout.block_size),
        });
    }
    if descriptor.compression_enabled {
        return Err(TapeIndexReplicaError::CompressionEnabled);
    }
    if descriptor.edition_id == [0; 16] {
        return Err(TapeIndexReplicaError::ZeroEditionId);
    }
    if descriptor.edition_sequence == 0 {
        return Err(TapeIndexReplicaError::ZeroEditionSequence);
    }
    validate_replica_scope_counts(descriptor.scope, descriptor.counts)?;
    validate_writer_version(&descriptor.writer_version).map_err(|bound| {
        TapeIndexReplicaError::Payload {
            message: format!("writer_version violates {bound}"),
        }
    })?;
    validate_write_timestamp(&descriptor.write_timestamp).map_err(|bound| {
        TapeIndexReplicaError::Payload {
            message: format!("write_timestamp violates {bound}"),
        }
    })?;
    validate_tape_index_payload_descriptor(&payload_descriptor(descriptor)).map_err(payload_error)
}

fn validate_replica_scope_counts(
    scope: TapeIndexReplicaScope,
    counts: TapeIndexReplicaCounts,
) -> Result<(), TapeIndexReplicaError> {
    if counts.structural_entry_count == 0 {
        return Err(TapeIndexReplicaError::EmptyPrefix);
    }
    if counts.object_row_count > counts.structural_entry_count {
        return Err(TapeIndexReplicaError::Payload {
            message: format!(
                "Object row count {} exceeds structural entry count {}",
                counts.object_row_count, counts.structural_entry_count
            ),
        });
    }
    if scope.covered_prefix_tape_file_count != counts.structural_entry_count {
        return Err(TapeIndexReplicaError::PlanMismatch {
            field: "covered_prefix_tape_file_count",
            expected: counts.structural_entry_count,
            actual: scope.covered_prefix_tape_file_count,
        });
    }
    if scope.highest_protected_ordinal > scope.total_data_ordinals {
        return Err(TapeIndexReplicaError::Payload {
            message: "highest protected ordinal exceeds total data ordinals".to_string(),
        });
    }
    Ok(())
}

fn validate_tail_scope(
    descriptor: &TapeIndexEditionDescriptor,
    layout: TapeIndexReplicaLayout,
) -> Result<(), TapeIndexReplicaError> {
    let first_replica = descriptor.terminal_layout.replica(1)?;
    if descriptor.scope.covered_prefix_tape_file_count != first_replica.planned_tape_file_number {
        return Err(TapeIndexReplicaError::PlanMismatch {
            field: "covered_prefix_tape_file_count",
            expected: first_replica.planned_tape_file_number,
            actual: descriptor.scope.covered_prefix_tape_file_count,
        });
    }
    for ordinal in 1..=TERMINAL_INDEX_REPLICA_COUNT {
        let component = descriptor.terminal_layout.replica(ordinal)?;
        if component.record_count != layout.replica_record_count {
            return Err(TapeIndexReplicaError::PlanMismatch {
                field: "terminal replica record count",
                expected: layout.replica_record_count,
                actual: component.record_count,
            });
        }
    }
    Ok(())
}

fn validate_edition_plan(plan: &TapeIndexEditionPlan) -> Result<(), TapeIndexReplicaError> {
    validate_edition_descriptor(&plan.descriptor)?;
    let expected_layout =
        checked_tape_index_replica_layout(plan.descriptor.block_size, plan.descriptor.counts)?;
    if expected_layout != plan.replica_layout {
        return Err(TapeIndexReplicaError::DigestMismatch {
            field: "replica layout",
        });
    }
    validate_tail_scope(&plan.descriptor, plan.replica_layout)?;
    if plan.layout_digest != plan.descriptor.terminal_layout.digest()? {
        return Err(TapeIndexReplicaError::DigestMismatch {
            field: "terminal layout digest",
        });
    }
    if plan.edition_digest
        != edition_digest(
            &plan.descriptor,
            plan.replica_layout,
            plan.payload_sha256,
            plan.canonical_map_sha256,
        )
    {
        return Err(TapeIndexReplicaError::DigestMismatch {
            field: "edition digest",
        });
    }
    Ok(())
}

fn validate_edition_plan_hinted(plan: &TapeIndexEditionPlan) -> Result<(), TapeIndexReplicaError> {
    validate_edition_descriptor_hinted(&plan.descriptor)?;
    let expected_layout = checked_tape_index_replica_layout_hinted(
        plan.descriptor.block_size,
        plan.descriptor.counts,
    )?;
    if expected_layout != plan.replica_layout {
        return Err(TapeIndexReplicaError::DigestMismatch {
            field: "replica layout",
        });
    }
    validate_tail_scope(&plan.descriptor, plan.replica_layout)?;
    if plan.layout_digest != plan.descriptor.terminal_layout.digest()? {
        return Err(TapeIndexReplicaError::DigestMismatch {
            field: "terminal layout digest",
        });
    }
    if plan.edition_digest
        != edition_digest(
            &plan.descriptor,
            plan.replica_layout,
            plan.payload_sha256,
            plan.canonical_map_sha256,
        )
    {
        return Err(TapeIndexReplicaError::DigestMismatch {
            field: "edition digest",
        });
    }
    Ok(())
}

fn validate_replica_plan(plan: &TapeIndexReplicaPlan) -> Result<(), TapeIndexReplicaError> {
    let expected = plan_tape_index_replica(plan.edition.clone(), plan.replica_ordinal)?;
    if *plan != expected {
        return Err(TapeIndexReplicaError::DigestMismatch {
            field: "replica immutable plan",
        });
    }
    Ok(())
}

fn validate_observation(
    plan: &TapeIndexReplicaPlan,
    observation: TapeIndexReplicaObservation,
) -> Result<(), TapeIndexReplicaError> {
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
            return Err(TapeIndexReplicaError::ObservationMismatch {
                field,
                planned,
                actual,
            });
        }
    }
    Ok(())
}

fn payload_descriptor(descriptor: &TapeIndexEditionDescriptor) -> TapeIndexPayloadDescriptor {
    TapeIndexPayloadDescriptor {
        block_size: descriptor.block_size,
        scope: descriptor.scope,
        counts: descriptor.counts,
    }
}

fn edition_digest(
    descriptor: &TapeIndexEditionDescriptor,
    layout: TapeIndexReplicaLayout,
    payload_sha256: [u8; 32],
    canonical_map_sha256: [u8; 32],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(TAPE_INDEX_EDITION_DIGEST_DOMAIN);
    hasher.update(TAPE_INDEX_REPLICA_SCHEMA_VERSION.to_le_bytes());
    hasher.update(descriptor.tape_uuid);
    hasher.update(descriptor.edition_id);
    hasher.update(descriptor.edition_sequence.to_le_bytes());
    hasher.update(descriptor.terminal_layout.partition.to_le_bytes());
    hasher.update(descriptor.block_size.to_le_bytes());
    hasher.update(0u32.to_le_bytes()); // compression disabled
    hasher.update(
        descriptor
            .scope
            .covered_prefix_tape_file_count
            .to_le_bytes(),
    );
    hasher.update(descriptor.scope.total_data_ordinals.to_le_bytes());
    hasher.update(descriptor.scope.highest_protected_ordinal.to_le_bytes());
    hasher.update(descriptor.counts.structural_entry_count.to_le_bytes());
    hasher.update(descriptor.counts.object_row_count.to_le_bytes());
    hasher.update(layout.payload_len.to_le_bytes());
    hasher.update(layout.payload_record_count.to_le_bytes());
    hasher.update(payload_sha256);
    hasher.update(canonical_map_sha256);
    update_len_bytes(&mut hasher, descriptor.writer_version.as_bytes());
    update_len_bytes(&mut hasher, descriptor.write_timestamp.as_bytes());
    hasher.finalize().into()
}

fn replica_descriptor_digest(
    edition: &TapeIndexEditionPlan,
    replica_ordinal: u16,
    component: TerminalTailComponentPlan,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(TAPE_INDEX_REPLICA_DESCRIPTOR_DOMAIN);
    hasher.update(edition.edition_digest);
    hasher.update(edition.layout_digest);
    hasher.update(replica_ordinal.to_le_bytes());
    hasher.update(TERMINAL_INDEX_REPLICA_COUNT.to_le_bytes());
    update_component_digest(&mut hasher, component);
    hasher.update(edition.replica_layout.footer_block_offset.to_le_bytes());
    hasher.finalize().into()
}

fn write_replica_frame(
    block: &mut [u8],
    plan: &TapeIndexReplicaPlan,
    magic: [u8; 8],
    role: u16,
) -> Result<(), TapeIndexReplicaError> {
    let descriptor = &plan.edition.descriptor;
    let expected_len = validate_terminal_index_block_size_hint(descriptor.block_size)?;
    if block.len() != expected_len {
        return Err(TapeIndexReplicaError::WrongLength {
            frame: "encoded",
            expected: expected_len,
            actual: block.len(),
        });
    }
    block[MAGIC_OFFSET..MAGIC_OFFSET + 8].copy_from_slice(&magic);
    block[SCHEMA_VERSION_OFFSET..SCHEMA_VERSION_OFFSET + size_of::<u16>()]
        .copy_from_slice(&TAPE_INDEX_REPLICA_SCHEMA_VERSION.to_le_bytes());
    block[ROLE_OFFSET..ROLE_OFFSET + size_of::<u16>()].copy_from_slice(&role.to_le_bytes());
    block[FINALITY_FLAGS_OFFSET..FINALITY_FLAGS_OFFSET + size_of::<u32>()]
        .copy_from_slice(&TAPE_INDEX_REPLICA_FLAG_FINAL.to_le_bytes());
    block[TAPE_UUID_OFFSET..TAPE_UUID_OFFSET + 16].copy_from_slice(&descriptor.tape_uuid);
    block[EDITION_ID_OFFSET..EDITION_ID_OFFSET + 16].copy_from_slice(&descriptor.edition_id);
    write_u64(block, EDITION_SEQUENCE_OFFSET, descriptor.edition_sequence);
    block[REPLICA_ORDINAL_OFFSET..REPLICA_ORDINAL_OFFSET + size_of::<u16>()]
        .copy_from_slice(&plan.replica_ordinal.to_le_bytes());
    block[REPLICA_COUNT_OFFSET..REPLICA_COUNT_OFFSET + size_of::<u16>()]
        .copy_from_slice(&TERMINAL_INDEX_REPLICA_COUNT.to_le_bytes());
    block[PARTITION_OFFSET..PARTITION_OFFSET + size_of::<u32>()]
        .copy_from_slice(&descriptor.terminal_layout.partition.to_le_bytes());
    block[BLOCK_SIZE_OFFSET..BLOCK_SIZE_OFFSET + size_of::<u32>()]
        .copy_from_slice(&descriptor.block_size.to_le_bytes());
    write_u64(
        block,
        COVERED_PREFIX_FILE_COUNT_OFFSET,
        descriptor.scope.covered_prefix_tape_file_count,
    );
    write_u64(
        block,
        TOTAL_DATA_ORDINALS_OFFSET,
        descriptor.scope.total_data_ordinals,
    );
    write_u64(
        block,
        HIGHEST_PROTECTED_ORDINAL_OFFSET,
        descriptor.scope.highest_protected_ordinal,
    );
    write_u64(
        block,
        STRUCTURAL_ENTRY_COUNT_OFFSET,
        descriptor.counts.structural_entry_count,
    );
    write_u64(
        block,
        OBJECT_ROW_COUNT_OFFSET,
        descriptor.counts.object_row_count,
    );
    write_u64(
        block,
        PAYLOAD_LEN_OFFSET,
        plan.edition.replica_layout.payload_len,
    );
    write_u64(
        block,
        PAYLOAD_RECORD_COUNT_OFFSET,
        plan.edition.replica_layout.payload_record_count,
    );
    write_u64(
        block,
        REPLICA_RECORD_COUNT_OFFSET,
        plan.edition.replica_layout.replica_record_count,
    );
    write_u64(
        block,
        PLANNED_TAPE_FILE_OFFSET,
        plan.component.planned_tape_file_number,
    );
    write_u64(
        block,
        PLANNED_START_LBA_OFFSET,
        plan.component.planned_start_lba,
    );
    write_u64(
        block,
        FOOTER_BLOCK_OFFSET,
        plan.edition.replica_layout.footer_block_offset,
    );
    write_u64(
        block,
        EXPECTED_EOD_LBA_OFFSET,
        descriptor.terminal_layout.expected_eod_lba,
    );
    block[PAYLOAD_SHA256_OFFSET..PAYLOAD_SHA256_OFFSET + 32]
        .copy_from_slice(&plan.edition.payload_sha256);
    block[CANONICAL_MAP_SHA256_OFFSET..CANONICAL_MAP_SHA256_OFFSET + 32]
        .copy_from_slice(&plan.edition.canonical_map_sha256);
    block[EDITION_DIGEST_OFFSET..EDITION_DIGEST_OFFSET + 32]
        .copy_from_slice(&plan.edition.edition_digest);
    block[LAYOUT_DIGEST_OFFSET..LAYOUT_DIGEST_OFFSET + 32]
        .copy_from_slice(&plan.edition.layout_digest);
    block[DESCRIPTOR_DIGEST_OFFSET..DESCRIPTOR_DIGEST_OFFSET + 32]
        .copy_from_slice(&plan.descriptor_digest);
    for (index, component) in descriptor
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
    let writer_len = u16::try_from(descriptor.writer_version.len()).map_err(|_| {
        TapeIndexReplicaError::Payload {
            message: "writer_version length exceeds u16".to_string(),
        }
    })?;
    let timestamp_len = u16::try_from(descriptor.write_timestamp.len()).map_err(|_| {
        TapeIndexReplicaError::Payload {
            message: "write_timestamp length exceeds u16".to_string(),
        }
    })?;
    block[DIAGNOSTIC_LENGTHS_OFFSET..DIAGNOSTIC_LENGTHS_OFFSET + 2]
        .copy_from_slice(&writer_len.to_le_bytes());
    block[DIAGNOSTIC_LENGTHS_OFFSET + 2..DIAGNOSTIC_LENGTHS_OFFSET + 4]
        .copy_from_slice(&timestamp_len.to_le_bytes());
    block[WRITER_VERSION_OFFSET..WRITER_VERSION_OFFSET + descriptor.writer_version.len()]
        .copy_from_slice(descriptor.writer_version.as_bytes());
    block[WRITE_TIMESTAMP_OFFSET..WRITE_TIMESTAMP_OFFSET + descriptor.write_timestamp.len()]
        .copy_from_slice(descriptor.write_timestamp.as_bytes());
    Ok(())
}

fn parse_replica_frame(
    block: &[u8],
    expected_tape_uuid: &[u8; 16],
    frame: &'static str,
    expected_role: u16,
) -> Result<TapeIndexReplicaPlan, TapeIndexReplicaError> {
    if block.len() < TAPE_INDEX_REPLICA_FRAME_LEN {
        return Err(TapeIndexReplicaError::WrongLength {
            frame,
            expected: TAPE_INDEX_REPLICA_FRAME_LEN,
            actual: block.len(),
        });
    }
    let physical_block_size = u32::try_from(block.len())
        .ok()
        .and_then(|size| validate_terminal_index_block_size_hint(size).ok())
        .ok_or(TapeIndexReplicaError::UnsupportedRecordLength {
            frame,
            actual: block.len(),
        })?;
    if read_u64(block, TAPE_INDEX_REPLICA_CRC_OFFSET)
        != crc64_xz(&block[..TAPE_INDEX_REPLICA_CRC_OFFSET])
    {
        return Err(TapeIndexReplicaError::CrcMismatch { frame });
    }
    let version = read_u16(block, SCHEMA_VERSION_OFFSET);
    if version != TAPE_INDEX_REPLICA_SCHEMA_VERSION {
        return Err(TapeIndexReplicaError::UnsupportedVersion { version });
    }
    if read_u16(block, ROLE_OFFSET) != expected_role
        || read_u32(block, FINALITY_FLAGS_OFFSET) != TAPE_INDEX_REPLICA_FLAG_FINAL
    {
        return Err(TapeIndexReplicaError::DigestMismatch {
            field: "role/finality flags",
        });
    }
    let mut tape_uuid = [0u8; 16];
    tape_uuid.copy_from_slice(&block[TAPE_UUID_OFFSET..TAPE_UUID_OFFSET + 16]);
    if &tape_uuid != expected_tape_uuid {
        return Err(TapeIndexReplicaError::WrongTape);
    }
    let mut edition_id = [0u8; 16];
    edition_id.copy_from_slice(&block[EDITION_ID_OFFSET..EDITION_ID_OFFSET + 16]);
    if edition_id == [0; 16] {
        return Err(TapeIndexReplicaError::ZeroEditionId);
    }
    let edition_sequence = read_u64(block, EDITION_SEQUENCE_OFFSET);
    if edition_sequence == 0 {
        return Err(TapeIndexReplicaError::ZeroEditionSequence);
    }
    let replica_ordinal = read_u16(block, REPLICA_ORDINAL_OFFSET);
    if !(1..=TERMINAL_INDEX_REPLICA_COUNT).contains(&replica_ordinal) {
        return Err(TapeIndexReplicaError::InvalidReplicaOrdinal {
            ordinal: replica_ordinal,
        });
    }
    if read_u16(block, REPLICA_COUNT_OFFSET) != TERMINAL_INDEX_REPLICA_COUNT {
        return Err(TapeIndexReplicaError::DigestMismatch {
            field: "replica count",
        });
    }
    let partition = read_u32(block, PARTITION_OFFSET);
    if partition != 0 {
        return Err(TerminalTailLayoutError::UnsupportedPartition { partition }.into());
    }
    let block_size = read_u32(block, BLOCK_SIZE_OFFSET);
    if block_size != physical_block_size as u32 {
        return Err(TapeIndexReplicaError::WrongLength {
            frame,
            expected: validate_terminal_index_block_size_hint(block_size)?,
            actual: block.len(),
        });
    }
    if read_u32(block, COMPRESSION_OFFSET) != 0 {
        return Err(TapeIndexReplicaError::CompressionEnabled);
    }
    let scope = TapeIndexReplicaScope {
        covered_prefix_tape_file_count: read_u64(block, COVERED_PREFIX_FILE_COUNT_OFFSET),
        total_data_ordinals: read_u64(block, TOTAL_DATA_ORDINALS_OFFSET),
        highest_protected_ordinal: read_u64(block, HIGHEST_PROTECTED_ORDINAL_OFFSET),
    };
    let counts = TapeIndexReplicaCounts {
        structural_entry_count: read_u64(block, STRUCTURAL_ENTRY_COUNT_OFFSET),
        object_row_count: read_u64(block, OBJECT_ROW_COUNT_OFFSET),
    };
    validate_replica_scope_counts(scope, counts)?;
    let writer_len = usize::from(read_u16(block, DIAGNOSTIC_LENGTHS_OFFSET));
    let timestamp_len = usize::from(read_u16(block, DIAGNOSTIC_LENGTHS_OFFSET + 2));
    if writer_len > WRITER_VERSION_MAX_BYTES || timestamp_len > WRITE_TIMESTAMP_MAX_BYTES {
        return Err(TapeIndexReplicaError::Payload {
            message: "diagnostic string length exceeds fixed slot".to_string(),
        });
    }
    match expected_role {
        TAPE_INDEX_REPLICA_HEADER_ROLE => ensure_zero(
            &block[HEADER_SHA256_OFFSET..TAPE_INDEX_REPLICA_CRC_OFFSET],
            "header footer-only fields",
        )?,
        TAPE_INDEX_REPLICA_FOOTER_ROLE => ensure_zero(
            &block[FIXED_RESERVED_OFFSET..TAPE_INDEX_REPLICA_CRC_OFFSET],
            "footer reserved fields",
        )?,
        _ => {
            return Err(TapeIndexReplicaError::DigestMismatch {
                field: "parser role",
            })
        }
    }
    ensure_zero(
        &block[TAPE_INDEX_REPLICA_FRAME_LEN..],
        "full record padding",
    )?;
    ensure_zero(
        &block[TAIL_COMPONENTS_OFFSET + TERMINAL_TAIL_COMPONENT_COUNT * TAIL_COMPONENT_LEN
            ..DIAGNOSTIC_LENGTHS_OFFSET],
        "pre-diagnostic reserved fields",
    )?;
    ensure_zero(
        &block[DIAGNOSTIC_LENGTHS_OFFSET + 4..WRITER_VERSION_OFFSET],
        "diagnostic reserved fields",
    )?;
    let writer_end = WRITER_VERSION_OFFSET + writer_len;
    let timestamp_end = WRITE_TIMESTAMP_OFFSET + timestamp_len;
    ensure_zero(
        &block[writer_end..WRITE_TIMESTAMP_OFFSET],
        "writer_version padding",
    )?;
    ensure_zero(
        &block[timestamp_end..HEADER_SHA256_OFFSET],
        "write_timestamp padding",
    )?;
    let mut payload_sha256 = [0u8; 32];
    payload_sha256.copy_from_slice(&block[PAYLOAD_SHA256_OFFSET..PAYLOAD_SHA256_OFFSET + 32]);
    let mut canonical_map_sha256 = [0u8; 32];
    canonical_map_sha256
        .copy_from_slice(&block[CANONICAL_MAP_SHA256_OFFSET..CANONICAL_MAP_SHA256_OFFSET + 32]);
    let mut stored_edition_digest = [0u8; 32];
    stored_edition_digest
        .copy_from_slice(&block[EDITION_DIGEST_OFFSET..EDITION_DIGEST_OFFSET + 32]);
    let mut stored_layout_digest = [0u8; 32];
    stored_layout_digest.copy_from_slice(&block[LAYOUT_DIGEST_OFFSET..LAYOUT_DIGEST_OFFSET + 32]);
    let mut stored_descriptor_digest = [0u8; 32];
    stored_descriptor_digest
        .copy_from_slice(&block[DESCRIPTOR_DIGEST_OFFSET..DESCRIPTOR_DIGEST_OFFSET + 32]);
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
    let writer_version = std::str::from_utf8(&block[WRITER_VERSION_OFFSET..writer_end])
        .map_err(|_| TapeIndexReplicaError::Payload {
            message: "writer_version is not UTF-8".to_string(),
        })?
        .to_string();
    let write_timestamp = std::str::from_utf8(&block[WRITE_TIMESTAMP_OFFSET..timestamp_end])
        .map_err(|_| TapeIndexReplicaError::Payload {
            message: "write_timestamp is not UTF-8".to_string(),
        })?
        .to_string();
    let terminal_layout = TerminalTailLayout {
        partition,
        block_size,
        components,
        expected_eod_lba: read_u64(block, EXPECTED_EOD_LBA_OFFSET),
    };
    let descriptor = TapeIndexEditionDescriptor {
        tape_uuid,
        edition_id,
        edition_sequence,
        scope,
        counts,
        block_size,
        compression_enabled: false,
        writer_version,
        write_timestamp,
        terminal_layout,
    };
    let replica_layout = checked_tape_index_replica_layout_hinted(block_size, counts)?;
    if replica_layout.payload_len != read_u64(block, PAYLOAD_LEN_OFFSET)
        || replica_layout.payload_record_count != read_u64(block, PAYLOAD_RECORD_COUNT_OFFSET)
        || replica_layout.replica_record_count != read_u64(block, REPLICA_RECORD_COUNT_OFFSET)
        || replica_layout.footer_block_offset != read_u64(block, FOOTER_BLOCK_OFFSET)
    {
        return Err(TapeIndexReplicaError::DigestMismatch {
            field: "replica geometry",
        });
    }
    let edition_digest = edition_digest(
        &descriptor,
        replica_layout,
        payload_sha256,
        canonical_map_sha256,
    );
    let layout_digest = descriptor.terminal_layout.digest()?;
    if edition_digest != stored_edition_digest || layout_digest != stored_layout_digest {
        return Err(TapeIndexReplicaError::DigestMismatch {
            field: "edition/layout digest",
        });
    }
    let edition = TapeIndexEditionPlan {
        descriptor,
        replica_layout,
        payload_sha256,
        canonical_map_sha256,
        edition_digest,
        layout_digest,
    };
    let plan = plan_tape_index_replica_hinted(edition, replica_ordinal)?;
    if plan.component.planned_tape_file_number != read_u64(block, PLANNED_TAPE_FILE_OFFSET)
        || plan.component.planned_start_lba != read_u64(block, PLANNED_START_LBA_OFFSET)
        || plan.descriptor_digest != stored_descriptor_digest
    {
        return Err(TapeIndexReplicaError::DigestMismatch {
            field: "local replica descriptor",
        });
    }
    Ok(plan)
}

struct ReplicaPayloadBlockWriter<'a, F>
where
    F: FnMut(&[u8]) -> Result<(), ParityError>,
{
    emit: &'a mut F,
    block: Vec<u8>,
    used: usize,
    emitted: u64,
    expected: u64,
    accepted_bytes: u64,
    expected_bytes: u64,
}

impl<'a, F> ReplicaPayloadBlockWriter<'a, F>
where
    F: FnMut(&[u8]) -> Result<(), ParityError>,
{
    fn new(
        block_size: u32,
        expected: u64,
        expected_bytes: u64,
        emit: &'a mut F,
    ) -> Result<Self, TapeIndexReplicaError> {
        Ok(Self {
            emit,
            block: vec![0; validate_terminal_index_block_size(block_size)?],
            used: 0,
            emitted: 0,
            expected,
            accepted_bytes: 0,
            expected_bytes,
        })
    }

    fn write_bytes(&mut self, mut bytes: &[u8]) -> Result<(), ParityError> {
        let incoming = u64::try_from(bytes.len()).map_err(|_| {
            ParityError::TapeIndexReplica("replica payload chunk length overflows u64".to_string())
        })?;
        let accepted =
            self.accepted_bytes
                .checked_add(incoming)
                .ok_or(ParityError::TapeIndexReplica(
                    "replica accepted payload bytes overflow u64".to_string(),
                ))?;
        if accepted > self.expected_bytes {
            return Err(ParityError::TapeIndexReplica(format!(
                "replica replay produced payload bytes beyond planned {}",
                self.expected_bytes
            )));
        }
        self.accepted_bytes = accepted;
        while !bytes.is_empty() {
            let take = (self.block.len() - self.used).min(bytes.len());
            self.block[self.used..self.used + take].copy_from_slice(&bytes[..take]);
            self.used += take;
            bytes = &bytes[take..];
            if self.used == self.block.len() {
                self.flush()?;
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), ParityError> {
        if self.emitted >= self.expected {
            return Err(ParityError::TapeIndexReplica(format!(
                "replica replay attempted payload record beyond planned {}",
                self.expected
            )));
        }
        (self.emit)(&self.block)?;
        self.emitted = self
            .emitted
            .checked_add(1)
            .ok_or(ParityError::TapeIndexReplica(
                "replica emitted block count overflows u64".to_string(),
            ))?;
        self.block.fill(0);
        self.used = 0;
        Ok(())
    }

    fn finish(mut self) -> Result<u64, TapeIndexReplicaError> {
        if self.accepted_bytes != self.expected_bytes {
            return Err(TapeIndexReplicaError::PlanMismatch {
                field: "accepted payload bytes",
                expected: self.expected_bytes,
                actual: self.accepted_bytes,
            });
        }
        if self.used != 0 {
            self.flush().map_err(payload_error)?;
        }
        if self.emitted != self.expected {
            return Err(TapeIndexReplicaError::PlanMismatch {
                field: "emitted payload records",
                expected: self.expected,
                actual: self.emitted,
            });
        }
        Ok(self.emitted)
    }
}

fn update_len_bytes(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn update_locator_digest(hasher: &mut Sha256, tape_file_number: u64, block_count: u64) {
    hasher.update(tape_file_number.to_le_bytes());
    hasher.update(block_count.to_le_bytes());
}

fn derive_magic(tape_uuid: &[u8; 16], message: &[u8]) -> [u8; 8] {
    let mut mac = HmacSha256::new_from_slice(tape_uuid).expect("HMAC accepts any key length");
    mac.update(message);
    mac.finalize().into_bytes()[..8]
        .try_into()
        .expect("eight-byte prefix")
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
) -> Result<TerminalTailComponentPlan, TapeIndexReplicaError> {
    let kind = match read_u16(block, offset) {
        4 => TerminalTailComponentKind::TapeIndexReplica,
        5 => TerminalTailComponentKind::IndexSeparationExtent,
        _ => {
            return Err(TapeIndexReplicaError::DigestMismatch {
                field: "terminal component kind",
            });
        }
    };
    if read_u32(block, offset + 4) != 1 {
        return Err(TapeIndexReplicaError::DigestMismatch {
            field: "terminal component filemark count",
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
    let crc = crc64_xz(&block[..TAPE_INDEX_REPLICA_CRC_OFFSET]);
    write_u64(block, TAPE_INDEX_REPLICA_CRC_OFFSET, crc);
}

fn ensure_zero(bytes: &[u8], field: &'static str) -> Result<(), TapeIndexReplicaError> {
    if let Some((offset, byte)) = bytes
        .iter()
        .copied()
        .enumerate()
        .find(|(_, byte)| *byte != 0)
    {
        return Err(TapeIndexReplicaError::ReservedNonzero {
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

#[cfg(test)]
fn write_u16(block: &mut [u8], offset: usize, value: u16) {
    block[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
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

fn payload_error(error: ParityError) -> TapeIndexReplicaError {
    TapeIndexReplicaError::Payload {
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_recovery::ObjectRecoveryRepresentation;

    #[derive(Clone)]
    struct VecSource {
        entries: Vec<TapeIndexReplicaMapEntry>,
        rows: Vec<TapeIndexReplicaObjectRow>,
    }

    impl TapeIndexReplicaRecordSource for VecSource {
        fn visit_structural_entries(
            &mut self,
            visitor: &mut dyn FnMut(&TapeIndexReplicaMapEntry) -> Result<(), ParityError>,
        ) -> Result<(), ParityError> {
            for entry in &self.entries {
                visitor(entry)?;
            }
            Ok(())
        }

        fn visit_object_rows(
            &mut self,
            visitor: &mut dyn FnMut(&TapeIndexReplicaObjectRow) -> Result<(), ParityError>,
        ) -> Result<(), ParityError> {
            for row in &self.rows {
                visitor(row)?;
            }
            Ok(())
        }
    }

    struct PayloadBlocks(Vec<Vec<u8>>);

    impl TapeIndexReplicaPayloadBlockSource for PayloadBlocks {
        fn visit_payload_blocks(
            &mut self,
            visitor: &mut dyn FnMut(&[u8]) -> Result<(), TapeIndexReplicaError>,
        ) -> Result<(), TapeIndexReplicaError> {
            for block in &self.0 {
                visitor(block)?;
            }
            Ok(())
        }
    }

    fn source() -> VecSource {
        VecSource {
            entries: vec![
                TapeIndexReplicaMapEntry {
                    tape_file_number: 0,
                    kind: TapeIndexReplicaFileKind::Bootstrap,
                    block_count: 1,
                    first_parity_data_ordinal: None,
                    protected_ordinal_start: None,
                    protected_ordinal_end_exclusive: None,
                    epoch_id: None,
                },
                TapeIndexReplicaMapEntry {
                    tape_file_number: 1,
                    kind: TapeIndexReplicaFileKind::Object,
                    block_count: 1,
                    first_parity_data_ordinal: Some(0),
                    protected_ordinal_start: None,
                    protected_ordinal_end_exclusive: None,
                    epoch_id: None,
                },
            ],
            rows: vec![TapeIndexReplicaObjectRow {
                tape_file_number: 1,
                stored_block_count: 1,
                object_id: b"object-1".to_vec(),
                representation: ObjectRecoveryRepresentation::Plaintext {
                    manifest_first_chunk_lba: 0,
                    manifest_size_bytes: 1,
                    manifest_chunk_count: 1,
                    manifest_sha256: [0x33; 32],
                },
            }],
        }
    }

    fn edition_plan(block_size: u32) -> TapeIndexEditionPlan {
        let counts = TapeIndexReplicaCounts {
            structural_entry_count: 2,
            object_row_count: 1,
        };
        let replica = checked_tape_index_replica_layout(block_size, counts).unwrap();
        let gap =
            crate::index_separation_records(block_size, crate::DEFAULT_INDEX_SEPARATION_BYTES)
                .unwrap();
        let tail = TerminalTailLayout::new(0, block_size, 2, 4, replica.replica_record_count, gap)
            .unwrap();
        plan_tape_index_edition(
            TapeIndexEditionDescriptor {
                tape_uuid: [0x11; 16],
                edition_id: [0x22; 16],
                edition_sequence: 1,
                scope: TapeIndexReplicaScope {
                    covered_prefix_tape_file_count: 2,
                    total_data_ordinals: 1,
                    highest_protected_ordinal: 0,
                },
                counts,
                block_size,
                compression_enabled: false,
                writer_version: "remanence-test".to_string(),
                write_timestamp: "2026-08-09T00:00:00Z".to_string(),
                terminal_layout: tail,
            },
            &mut source(),
        )
        .unwrap()
    }

    fn written_replica(block_size: u32) -> (TapeIndexReplicaPlan, Vec<Vec<u8>>) {
        let plan = plan_tape_index_replica(edition_plan(block_size), 3).unwrap();
        let observation = TapeIndexReplicaObservation {
            tape_file_number: plan.component.planned_tape_file_number,
            start_lba: plan.component.planned_start_lba,
            record_count: plan.component.record_count,
        };
        let mut blocks = Vec::new();
        write_tape_index_replica(&plan, observation, &mut source(), |block| {
            blocks.push(block.to_vec());
            Ok(())
        })
        .unwrap();
        (plan, blocks)
    }

    #[test]
    fn full_header_is_not_hidden_inside_payload_geometry() {
        let exact = checked_tape_index_replica_layout(
            256 * 1024,
            TapeIndexReplicaCounts {
                structural_entry_count: 4_096,
                object_row_count: 0,
            },
        )
        .unwrap();
        assert_eq!(exact.payload_len, 256 * 1024);
        assert_eq!(exact.payload_record_count, 1);
        assert_eq!(exact.replica_record_count, 3);

        let crossed = checked_tape_index_replica_layout(
            256 * 1024,
            TapeIndexReplicaCounts {
                structural_entry_count: 4_097,
                object_row_count: 0,
            },
        )
        .unwrap();
        assert_eq!(crossed.payload_record_count, 2);
        assert_eq!(crossed.replica_record_count, 4);
    }

    #[test]
    fn hinted_frame_decoder_rejects_non_terminal_4096_geometry() {
        assert!(matches!(
            checked_tape_index_replica_layout_hinted(
                4096,
                TapeIndexReplicaCounts {
                    structural_entry_count: 2,
                    object_row_count: 1,
                },
            ),
            Err(TapeIndexReplicaError::Layout(
                TerminalTailLayoutError::UnsupportedBlockSize { block_size: 4096 }
            ))
        ));
    }

    #[test]
    fn edition_rejects_empty_or_non_bootstrap_prefix() {
        let block_size = 256 * 1024;
        let counts = TapeIndexReplicaCounts {
            structural_entry_count: 0,
            object_row_count: 0,
        };
        let layout = checked_tape_index_replica_layout(block_size, counts).unwrap();
        assert_eq!(layout.payload_len, 0);
        assert_eq!(layout.payload_record_count, 0);
        assert_eq!(layout.replica_record_count, 2);
        let gap =
            crate::index_separation_records(block_size, crate::DEFAULT_INDEX_SEPARATION_BYTES)
                .unwrap();
        let error = plan_tape_index_edition(
            TapeIndexEditionDescriptor {
                tape_uuid: [0x11; 16],
                edition_id: [0x22; 16],
                edition_sequence: 1,
                scope: TapeIndexReplicaScope {
                    covered_prefix_tape_file_count: 0,
                    total_data_ordinals: 0,
                    highest_protected_ordinal: 0,
                },
                counts,
                block_size,
                compression_enabled: false,
                writer_version: "remanence-test".to_string(),
                write_timestamp: "2026-08-09T00:00:00Z".to_string(),
                terminal_layout: TerminalTailLayout::new(0, block_size, 0, 0, 2, gap).unwrap(),
            },
            &mut VecSource {
                entries: Vec::new(),
                rows: Vec::new(),
            },
        )
        .unwrap_err();
        assert!(matches!(error, TapeIndexReplicaError::EmptyPrefix));

        let descriptor = TapeIndexEditionDescriptor {
            tape_uuid: [0x11; 16],
            edition_id: [0x22; 16],
            edition_sequence: 1,
            scope: TapeIndexReplicaScope {
                covered_prefix_tape_file_count: 0,
                total_data_ordinals: 0,
                highest_protected_ordinal: 0,
            },
            counts,
            block_size,
            compression_enabled: false,
            writer_version: "remanence-test".to_string(),
            write_timestamp: "2026-08-09T00:00:00Z".to_string(),
            terminal_layout: TerminalTailLayout::new(0, block_size, 0, 0, 2, gap).unwrap(),
        };
        let replica_layout = checked_tape_index_replica_layout(block_size, counts).unwrap();
        let payload_sha256 = [0x33; 32];
        let canonical_map_sha256 = [0x44; 32];
        let layout_digest = descriptor.terminal_layout.digest().unwrap();
        let edition_digest = edition_digest(
            &descriptor,
            replica_layout,
            payload_sha256,
            canonical_map_sha256,
        );
        let edition = TapeIndexEditionPlan {
            descriptor,
            replica_layout,
            payload_sha256,
            canonical_map_sha256,
            edition_digest,
            layout_digest,
        };
        let component = edition.descriptor.terminal_layout.replica(1).unwrap();
        let malicious = TapeIndexReplicaPlan {
            descriptor_digest: replica_descriptor_digest(&edition, 1, component),
            edition,
            replica_ordinal: 1,
            component,
        };
        let mut block = vec![0; block_size as usize];
        write_replica_frame(
            &mut block,
            &malicious,
            derive_tape_index_replica_header_magic(&malicious.edition.descriptor.tape_uuid),
            TAPE_INDEX_REPLICA_HEADER_ROLE,
        )
        .unwrap();
        write_crc(&mut block);
        assert!(matches!(
            parse_tape_index_replica_header(&block, &[0x11; 16]),
            Err(TapeIndexReplicaError::EmptyPrefix)
        ));

        let descriptor = edition_plan(block_size).descriptor;
        let mut non_bootstrap = source();
        non_bootstrap.entries[0].kind = TapeIndexReplicaFileKind::ParityMap;
        let error = plan_tape_index_edition(descriptor, &mut non_bootstrap).unwrap_err();
        assert!(matches!(error, TapeIndexReplicaError::Payload { .. }));
    }

    #[test]
    fn replica_parser_validates_scalars_before_reserved_bytes() {
        let (_, blocks) = written_replica(256 * 1024);
        let header = &blocks[0];

        let mut bad_count = header.clone();
        bad_count[0x3A..0x3C].copy_from_slice(&2u16.to_le_bytes());
        bad_count[FIXED_RESERVED_OFFSET] = 1;
        write_crc(&mut bad_count);
        assert!(matches!(
            parse_tape_index_replica_header(&bad_count, &[0x11; 16]),
            Err(TapeIndexReplicaError::DigestMismatch {
                field: "replica count"
            })
        ));

        let mut bad_ordinal = header.clone();
        bad_ordinal[0x38..0x3A].copy_from_slice(&0u16.to_le_bytes());
        bad_ordinal[FIXED_RESERVED_OFFSET] = 1;
        write_crc(&mut bad_ordinal);
        assert!(matches!(
            parse_tape_index_replica_header(&bad_ordinal, &[0x11; 16]),
            Err(TapeIndexReplicaError::InvalidReplicaOrdinal { ordinal: 0 })
        ));

        let mut bad_partition = header.clone();
        bad_partition[0x3C..0x40].copy_from_slice(&1u32.to_le_bytes());
        bad_partition[FIXED_RESERVED_OFFSET] = 1;
        write_crc(&mut bad_partition);
        assert!(matches!(
            parse_tape_index_replica_header(&bad_partition, &[0x11; 16]),
            Err(TapeIndexReplicaError::Layout(
                TerminalTailLayoutError::UnsupportedPartition { partition: 1 }
            ))
        ));
    }

    #[test]
    fn writes_parses_and_fully_validates_every_block_size() {
        for block_size in [256 * 1024, 512 * 1024, 1024 * 1024] {
            let (plan, blocks) = written_replica(block_size);
            assert_eq!(blocks.len() as u64, plan.component.record_count);
            let header = parse_tape_index_replica_header(&blocks[0], &[0x11; 16]).unwrap();
            let footer =
                parse_tape_index_bootstrap_footer(blocks.last().unwrap(), &[0x11; 16]).unwrap();
            let mut entries = Vec::new();
            let mut rows = Vec::new();
            let summary = validate_tape_index_replica_payload(
                &header,
                &footer,
                &mut PayloadBlocks(blocks[1..blocks.len() - 1].to_vec()),
                |entry| {
                    entries.push(entry.clone());
                    Ok(())
                },
                |row| {
                    rows.push(row.clone());
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(entries, source().entries);
            assert_eq!(rows, source().rows);
            assert_eq!(summary.payload_sha256, plan.edition.payload_sha256);
            assert_eq!(footer.plan.replica_ordinal, 3);
        }
    }

    #[test]
    fn local_header_substitution_and_wrong_observation_fail() {
        let edition = edition_plan(256 * 1024);
        let a = plan_tape_index_replica(edition.clone(), 1).unwrap();
        let b = plan_tape_index_replica(edition, 2).unwrap();
        let a_header = encode_tape_index_replica_header(&a).unwrap();
        let a_parsed = parse_tape_index_replica_header(&a_header, &[0x11; 16]).unwrap();
        let b_observation = TapeIndexReplicaObservation {
            tape_file_number: b.component.planned_tape_file_number,
            start_lba: b.component.planned_start_lba,
            record_count: b.component.record_count,
        };
        let b_footer =
            encode_tape_index_bootstrap_footer(&b, a_parsed.header_sha256, b_observation).unwrap();
        let b_parsed = parse_tape_index_bootstrap_footer(&b_footer, &[0x11; 16]).unwrap();
        assert!(validate_tape_index_replica_pair(&a_parsed, &b_parsed).is_err());

        let mut emitted = false;
        let error = write_tape_index_replica(
            &a,
            TapeIndexReplicaObservation {
                tape_file_number: a.component.planned_tape_file_number,
                start_lba: a.component.planned_start_lba + 1,
                record_count: a.component.record_count,
            },
            &mut source(),
            |_| {
                emitted = true;
                Ok(())
            },
        )
        .unwrap_err();
        assert!(matches!(
            error,
            TapeIndexReplicaError::ObservationMismatch {
                field: "start_lba",
                ..
            }
        ));
        assert!(!emitted);
    }

    #[test]
    fn full_validator_rejects_slot_and_record_padding_corruption() {
        let (_, blocks) = written_replica(256 * 1024);
        let header = parse_tape_index_replica_header(&blocks[0], &[0x11; 16]).unwrap();
        let footer =
            parse_tape_index_bootstrap_footer(blocks.last().unwrap(), &[0x11; 16]).unwrap();

        let mut slot_corrupt = blocks[1].clone();
        slot_corrupt[63] = 1;
        let error = validate_tape_index_replica_payload(
            &header,
            &footer,
            &mut PayloadBlocks(vec![slot_corrupt]),
            |_| Ok(()),
            |_| Ok(()),
        )
        .unwrap_err();
        assert!(matches!(error, TapeIndexReplicaError::Payload { .. }));

        let mut record_padding_corrupt = blocks[1].clone();
        *record_padding_corrupt.last_mut().unwrap() = 1;
        let error = validate_tape_index_replica_payload(
            &header,
            &footer,
            &mut PayloadBlocks(vec![record_padding_corrupt]),
            |_| Ok(()),
            |_| Ok(()),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            TapeIndexReplicaError::ReservedNonzero {
                field: "payload record padding",
                ..
            }
        ));
    }

    #[test]
    fn full_validator_rejects_sidecar_without_final_parity_map() {
        let (_, blocks) = written_replica(256 * 1024);
        let header = parse_tape_index_replica_header(&blocks[0], &[0x11; 16]).unwrap();
        let footer =
            parse_tape_index_bootstrap_footer(blocks.last().unwrap(), &[0x11; 16]).unwrap();

        // Replace the valid Object structural slot with a canonical sidecar
        // slot, leaving no final ParityMap. The semantic refusal precedes the
        // expected digest mismatch from this hostile payload mutation.
        let value = ciborium::value::Value::Array(vec![
            ciborium::value::Value::Integer(1.into()),
            ciborium::value::Value::Integer(1.into()),
            ciborium::value::Value::Integer(1.into()),
            ciborium::value::Value::Null,
            ciborium::value::Value::Integer(0.into()),
            ciborium::value::Value::Integer(1.into()),
            ciborium::value::Value::Integer(0.into()),
        ]);
        let mut encoded = Vec::new();
        ciborium::into_writer(&value, &mut encoded).unwrap();
        let mut sidecar_slot = vec![0; TAPE_INDEX_PAYLOAD_STRUCTURAL_SLOT_LEN as usize];
        sidecar_slot[..TAPE_INDEX_PAYLOAD_SLOT_PREFIX_LEN]
            .copy_from_slice(&(encoded.len() as u16).to_le_bytes());
        sidecar_slot[TAPE_INDEX_PAYLOAD_SLOT_PREFIX_LEN
            ..TAPE_INDEX_PAYLOAD_SLOT_PREFIX_LEN + encoded.len()]
            .copy_from_slice(&encoded);
        let mut payload = blocks[1].clone();
        let object_slot_start = TAPE_INDEX_PAYLOAD_STRUCTURAL_SLOT_LEN as usize;
        payload[object_slot_start..object_slot_start + sidecar_slot.len()]
            .copy_from_slice(&sidecar_slot);

        let error = validate_tape_index_replica_payload(
            &header,
            &footer,
            &mut PayloadBlocks(vec![payload]),
            |_| Ok(()),
            |_| Ok(()),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("exactly one final ParityMap iff"),
            "{error}"
        );
    }

    #[test]
    fn changed_replay_cannot_emit_past_planned_payload_or_footer() {
        let entries = vec![TapeIndexReplicaMapEntry {
            tape_file_number: 0,
            kind: TapeIndexReplicaFileKind::Bootstrap,
            block_count: 1,
            first_parity_data_ordinal: None,
            protected_ordinal_start: None,
            protected_ordinal_end_exclusive: None,
            epoch_id: None,
        }];
        let exact_source = VecSource {
            entries: entries.clone(),
            rows: Vec::new(),
        };
        let counts = TapeIndexReplicaCounts {
            structural_entry_count: 1,
            object_row_count: 0,
        };
        let replica = checked_tape_index_replica_layout(256 * 1024, counts).unwrap();
        assert_eq!(replica.payload_record_count, 1);
        let gap =
            crate::index_separation_records(256 * 1024, crate::DEFAULT_INDEX_SEPARATION_BYTES)
                .unwrap();
        let tail = TerminalTailLayout::new(0, 256 * 1024, 1, 2, 3, gap).unwrap();
        let edition = plan_tape_index_edition(
            TapeIndexEditionDescriptor {
                tape_uuid: [0x11; 16],
                edition_id: [0x22; 16],
                edition_sequence: 1,
                scope: TapeIndexReplicaScope {
                    covered_prefix_tape_file_count: 1,
                    total_data_ordinals: 0,
                    highest_protected_ordinal: 0,
                },
                counts,
                block_size: 256 * 1024,
                compression_enabled: false,
                writer_version: "remanence-test".to_string(),
                write_timestamp: "2026-08-09T00:00:00Z".to_string(),
                terminal_layout: tail,
            },
            &mut exact_source.clone(),
        )
        .unwrap();
        let plan = plan_tape_index_replica(edition, 1).unwrap();
        let mut replay = exact_source;
        replay.entries.push(TapeIndexReplicaMapEntry {
            tape_file_number: 1,
            kind: TapeIndexReplicaFileKind::Object,
            block_count: 1,
            first_parity_data_ordinal: Some(0),
            protected_ordinal_start: None,
            protected_ordinal_end_exclusive: None,
            epoch_id: None,
        });
        let observation = TapeIndexReplicaObservation {
            tape_file_number: plan.component.planned_tape_file_number,
            start_lba: plan.component.planned_start_lba,
            record_count: plan.component.record_count,
        };
        let mut emitted = Vec::new();
        let error = write_tape_index_replica(&plan, observation, &mut replay, |block| {
            emitted.push(block.to_vec());
            Ok(())
        })
        .unwrap_err();
        assert!(matches!(error, TapeIndexReplicaError::Payload { .. }));
        assert_eq!(
            emitted.len(),
            1,
            "changed replay is rejected after the header and before payload/footer emission"
        );
    }

    #[test]
    fn full_validator_carries_object_slots_across_record_boundaries() {
        let entries = (0..4_095)
            .map(|tape_file_number| TapeIndexReplicaMapEntry {
                tape_file_number,
                kind: if tape_file_number == 0 {
                    TapeIndexReplicaFileKind::Bootstrap
                } else {
                    TapeIndexReplicaFileKind::Object
                },
                block_count: 1,
                first_parity_data_ordinal: tape_file_number.checked_sub(1),
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                epoch_id: None,
            })
            .collect::<Vec<_>>();
        let rows = (1..4_095)
            .map(|tape_file_number| TapeIndexReplicaObjectRow {
                tape_file_number,
                stored_block_count: 1,
                object_id: format!("boundary-object-{tape_file_number}").into_bytes(),
                representation: ObjectRecoveryRepresentation::Plaintext {
                    manifest_first_chunk_lba: 0,
                    manifest_size_bytes: 1,
                    manifest_chunk_count: 1,
                    manifest_sha256: [0x44; 32],
                },
            })
            .collect::<Vec<_>>();
        let exact_source = VecSource {
            entries,
            rows: rows.clone(),
        };
        let counts = TapeIndexReplicaCounts {
            structural_entry_count: 4_095,
            object_row_count: 4_094,
        };
        let replica = checked_tape_index_replica_layout(256 * 1024, counts).unwrap();
        assert!(replica.payload_record_count > 1);
        let gap =
            crate::index_separation_records(256 * 1024, crate::DEFAULT_INDEX_SEPARATION_BYTES)
                .unwrap();
        let edition = plan_tape_index_edition(
            TapeIndexEditionDescriptor {
                tape_uuid: [0x11; 16],
                edition_id: [0x22; 16],
                edition_sequence: 1,
                scope: TapeIndexReplicaScope {
                    covered_prefix_tape_file_count: 4_095,
                    total_data_ordinals: 4_094,
                    highest_protected_ordinal: 0,
                },
                counts,
                block_size: 256 * 1024,
                compression_enabled: false,
                writer_version: "remanence-test".to_string(),
                write_timestamp: "2026-08-09T00:00:00Z".to_string(),
                terminal_layout: TerminalTailLayout::new(
                    0,
                    256 * 1024,
                    4_095,
                    8_190,
                    replica.replica_record_count,
                    gap,
                )
                .unwrap(),
            },
            &mut exact_source.clone(),
        )
        .unwrap();
        let plan = plan_tape_index_replica(edition, 1).unwrap();
        let observation = TapeIndexReplicaObservation {
            tape_file_number: plan.component.planned_tape_file_number,
            start_lba: plan.component.planned_start_lba,
            record_count: plan.component.record_count,
        };
        let mut blocks = Vec::new();
        write_tape_index_replica(&plan, observation, &mut exact_source.clone(), |block| {
            blocks.push(block.to_vec());
            Ok(())
        })
        .unwrap();
        let header = parse_tape_index_replica_header(&blocks[0], &[0x11; 16]).unwrap();
        let footer =
            parse_tape_index_bootstrap_footer(blocks.last().unwrap(), &[0x11; 16]).unwrap();
        let mut decoded_rows = Vec::new();
        validate_tape_index_replica_payload(
            &header,
            &footer,
            &mut PayloadBlocks(blocks[1..blocks.len() - 1].to_vec()),
            |_| Ok(()),
            |decoded| {
                decoded_rows.push(decoded.clone());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(decoded_rows, rows);
    }

    #[test]
    fn pre_tail_payload_rejects_terminal_structural_kinds() {
        for kind in [
            TapeIndexReplicaFileKind::TapeIndexReplica,
            TapeIndexReplicaFileKind::IndexSeparationExtent,
        ] {
            let mut changed = source();
            changed.entries[0].kind = kind;
            let error = plan_tape_index_edition(
                TapeIndexEditionDescriptor {
                    tape_uuid: [0x11; 16],
                    edition_id: [0x22; 16],
                    edition_sequence: 1,
                    scope: TapeIndexReplicaScope {
                        covered_prefix_tape_file_count: 2,
                        total_data_ordinals: 1,
                        highest_protected_ordinal: 0,
                    },
                    counts: TapeIndexReplicaCounts {
                        structural_entry_count: 2,
                        object_row_count: 1,
                    },
                    block_size: 256 * 1024,
                    compression_enabled: false,
                    writer_version: "remanence-test".to_string(),
                    write_timestamp: "2026-08-09T00:00:00Z".to_string(),
                    terminal_layout: TerminalTailLayout::new(
                        0,
                        256 * 1024,
                        2,
                        4,
                        3,
                        crate::index_separation_records(
                            256 * 1024,
                            crate::DEFAULT_INDEX_SEPARATION_BYTES,
                        )
                        .unwrap(),
                    )
                    .unwrap(),
                },
                &mut changed,
            )
            .unwrap_err();
            assert!(matches!(error, TapeIndexReplicaError::Payload { .. }));
        }
    }

    #[test]
    fn parser_rejects_hostile_frames_in_normative_order() {
        let plan = plan_tape_index_replica(edition_plan(256 * 1024), 1).unwrap();
        let valid = encode_tape_index_replica_header(&plan).unwrap();

        let error = parse_tape_index_replica_header(&valid[..8], &[0x11; 16]).unwrap_err();
        assert!(matches!(error, TapeIndexReplicaError::WrongLength { .. }));

        let mut corrupt = valid.clone();
        corrupt[0x08] ^= 1;
        assert!(matches!(
            parse_tape_index_replica_header(&corrupt, &[0x11; 16]),
            Err(TapeIndexReplicaError::CrcMismatch { .. })
        ));

        write_u16(&mut corrupt, 0x08, TAPE_INDEX_REPLICA_SCHEMA_VERSION + 1);
        write_crc(&mut corrupt);
        assert!(matches!(
            parse_tape_index_replica_header(&corrupt, &[0x11; 16]),
            Err(TapeIndexReplicaError::UnsupportedVersion { .. })
        ));

        let mut corrupt = valid.clone();
        corrupt[0x0A] = 9;
        write_crc(&mut corrupt);
        assert!(matches!(
            parse_tape_index_replica_header(&corrupt, &[0x11; 16]),
            Err(TapeIndexReplicaError::DigestMismatch {
                field: "role/finality flags"
            })
        ));

        let mut corrupt = valid.clone();
        corrupt[0x10] ^= 1;
        write_crc(&mut corrupt);
        assert_eq!(
            parse_tape_index_replica_header(&corrupt, &[0x11; 16]),
            Err(TapeIndexReplicaError::WrongTape)
        );

        let mut corrupt = valid.clone();
        corrupt[FIXED_RESERVED_OFFSET] = 1;
        write_crc(&mut corrupt);
        assert!(matches!(
            parse_tape_index_replica_header(&corrupt, &[0x11; 16]),
            Err(TapeIndexReplicaError::ReservedNonzero { .. })
        ));

        let mut corrupt = valid;
        corrupt[TAPE_INDEX_REPLICA_FRAME_LEN] = 1;
        assert!(matches!(
            parse_tape_index_replica_header(&corrupt, &[0x11; 16]),
            Err(TapeIndexReplicaError::ReservedNonzero { .. })
        ));
    }
}
