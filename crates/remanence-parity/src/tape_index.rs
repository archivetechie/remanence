//! Candidate streamed tape-index snapshot framing and checked size model.
//!
//! This is the single pre-specification candidate used by the capacity proofs
//! and byte-draft review. A snapshot is one filemark-delimited control file:
//! two complete independently verifiable copies followed by one locator
//! footer. The payload is fixed-slot canonical CBOR, so its exact block count
//! depends only on two `u64` counts and can be calculated without collecting
//! the tape-wide row set.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use ciborium::value::Value as CborValue;

use crate::bootstrap::{validate_object_recovery_row_fields, BootstrapObjectRepresentation};
use crate::diagnostic_text::{
    validate_write_timestamp, validate_writer_version, WRITER_VERSION_MAX_BYTES,
    WRITE_TIMESTAMP_MAX_BYTES,
};
use crate::error::ParityError;
use crate::replicated_control::{
    checked_replicated_control_layout, validate_replicated_control_layout,
};
use crate::sidecar::crc64_xz;

type HmacSha256 = Hmac<Sha256>;

/// Candidate snapshot schema version.
pub const TAPE_INDEX_SNAPSHOT_SCHEMA_VERSION: u16 = 1;

/// Candidate footer schema version.
pub const TAPE_INDEX_SNAPSHOT_FOOTER_VERSION: u16 = 1;

/// Fixed header bytes at the start of each complete copy.
pub const TAPE_INDEX_SNAPSHOT_HEADER_LEN: usize = 0x200;

/// Header CRC-64/XZ offset; the CRC covers all earlier fixed bytes.
pub const TAPE_INDEX_SNAPSHOT_HEADER_CRC_OFFSET: usize = 0x1F8;

/// Fixed footer-locator bytes in the final tape block.
pub const TAPE_INDEX_SNAPSHOT_FOOTER_LEN: usize = 0x200;

/// Footer CRC-64/XZ offset; the CRC covers all earlier fixed bytes.
pub const TAPE_INDEX_SNAPSHOT_FOOTER_CRC_OFFSET: usize = 0x1F8;

/// Bytes reserved for each canonical structural map entry.
pub const TAPE_INDEX_STRUCTURAL_SLOT_LEN: u64 = 64;

/// Bytes reserved for each canonical Object recovery row.
pub const TAPE_INDEX_OBJECT_ROW_SLOT_LEN: u64 = 256;

/// Fixed record sizes accepted by the candidate snapshot wire.
pub const TAPE_INDEX_SNAPSHOT_BLOCK_SIZES: &[u32] = &[256 * 1024, 512 * 1024, 1024 * 1024];

/// Per-slot little-endian encoded-length prefix.
pub const TAPE_INDEX_SLOT_PREFIX_LEN: usize = 2;

/// Largest canonical structural entry accepted inside one slot.
pub const TAPE_INDEX_STRUCTURAL_ENTRY_MAX_LEN: usize =
    TAPE_INDEX_STRUCTURAL_SLOT_LEN as usize - TAPE_INDEX_SLOT_PREFIX_LEN;

/// Largest canonical Object recovery row accepted inside one slot.
pub const TAPE_INDEX_OBJECT_ROW_MAX_LEN: usize =
    TAPE_INDEX_OBJECT_ROW_SLOT_LEN as usize - TAPE_INDEX_SLOT_PREFIX_LEN;

const TAPE_INDEX_HEADER_MAGIC_MESSAGE: &[u8] = b"REM\x00TIDX\x01H";
const TAPE_INDEX_FOOTER_MAGIC_MESSAGE: &[u8] = b"REM\x00TIDX\x01F";
const TAPE_INDEX_FLAG_FINAL: u32 = 0x0000_0001;
const TAPE_INDEX_KNOWN_FLAGS: u32 = TAPE_INDEX_FLAG_FINAL;

const WRITER_VERSION_LEN_OFFSET: usize = 0xC8;
const WRITE_TIMESTAMP_LEN_OFFSET: usize = 0xCA;
const DIAGNOSTIC_RESERVED_OFFSET: usize = 0xCC;
const WRITER_VERSION_OFFSET: usize = 0xD0;
const WRITE_TIMESTAMP_OFFSET: usize = 0x150;
const FIXED_RESERVED_OFFSET: usize = 0x190;

/// Counts that determine the exact fixed-slot payload size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TapeIndexSnapshotCounts {
    /// Structural map entries in the covered prefix.
    pub structural_entry_count: u64,
    /// Object recovery rows, exactly one for each Object map entry.
    pub object_row_count: u64,
}

/// Authenticated structural scope covered by a snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TapeIndexSnapshotScope {
    /// Number of leading tape files in the embedded structural map.
    pub covered_prefix_tape_file_count: u64,
    /// Total Object-data ordinals in the covered prefix.
    pub total_data_ordinals: u64,
    /// Highest protected Object-data ordinal in the covered prefix.
    pub highest_protected_ordinal: u64,
}

/// Metadata repeated in both snapshot headers and the footer locator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapeIndexSnapshotDescriptor {
    /// Tape UUID that domains the snapshot magic.
    pub tape_uuid: [u8; 16],
    /// Monotonic snapshot edition on this tape.
    pub sequence: u64,
    /// Fixed tape-record size.
    pub block_size: u32,
    /// Authenticated prefix scope.
    pub scope: TapeIndexSnapshotScope,
    /// Exact structural and Object row counts.
    pub counts: TapeIndexSnapshotCounts,
    /// True when this edition is the terminal inventory.
    pub is_final: bool,
    /// Bounded printable writer identity.
    pub writer_version: String,
    /// Bounded RFC3339 write timestamp.
    pub write_timestamp: String,
}

/// Exact `copy 1 + copy 2 + footer` snapshot geometry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TapeIndexSnapshotLayout {
    /// Fixed-slot payload bytes in one copy.
    pub payload_len: u64,
    /// Blocks occupied by one complete header/payload copy.
    pub copy_block_count: u64,
    /// Total blocks before the trailing filemark.
    pub total_block_count: u64,
    /// Primary copy start block, always zero.
    pub primary_copy_start_block: u64,
    /// Tail copy start block.
    pub tail_copy_start_block: u64,
    /// Final footer-locator block index.
    pub footer_block_index: u64,
}

/// Primary or tail snapshot copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TapeIndexSnapshotCopyKind {
    /// Copy beginning at block zero.
    Primary,
    /// Complete second copy immediately following the primary copy.
    Tail,
}

impl TapeIndexSnapshotCopyKind {
    fn code(self) -> u16 {
        match self {
            Self::Primary => 1,
            Self::Tail => 2,
        }
    }

    fn from_code(value: u16) -> Result<Self, ParityError> {
        match value {
            1 => Ok(Self::Primary),
            2 => Ok(Self::Tail),
            _ => Err(snapshot_error(format!(
                "unsupported snapshot copy kind {value}"
            ))),
        }
    }
}

/// Candidate u64 structural kind stored in the embedded map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TapeIndexSnapshotFileKind {
    /// REM-OBJECT body tape file.
    Object,
    /// Parity sidecar control tape file.
    ParitySidecar,
    /// One-block Bootstrap control tape file.
    Bootstrap,
    /// External ParityMap control tape file.
    ParityMap,
    /// Earlier complete TapeIndexSnapshot control tape file.
    TapeIndexSnapshot,
}

impl TapeIndexSnapshotFileKind {
    fn code(self) -> u64 {
        match self {
            Self::Object => 0,
            Self::ParitySidecar => 1,
            Self::Bootstrap => 2,
            Self::ParityMap => 3,
            Self::TapeIndexSnapshot => 4,
        }
    }
}

/// One u64 structural map row streamed into the snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapeIndexSnapshotMapEntry {
    /// Dense filemark-delimited tape-file number from BOT.
    pub tape_file_number: u64,
    /// Structural kind.
    pub kind: TapeIndexSnapshotFileKind,
    /// Fixed records before the trailing filemark.
    pub block_count: u64,
    /// First Object-data ordinal for Object entries.
    pub first_parity_data_ordinal: Option<u64>,
    /// First protected ordinal for sidecar entries.
    pub protected_ordinal_start: Option<u64>,
    /// End-exclusive protected ordinal for sidecar entries.
    pub protected_ordinal_end_exclusive: Option<u64>,
    /// Epoch identity for sidecar entries.
    pub epoch_id: Option<u64>,
}

/// One u64 Object recovery row streamed into the snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapeIndexSnapshotObjectRow {
    /// Tape-file number of the matching Object map entry.
    pub tape_file_number: u64,
    /// Stored fixed-block count, which must match the map entry.
    pub stored_block_count: u64,
    /// Required 1–64-byte verbatim REM-OBJECT identifier.
    pub object_id: Vec<u8>,
    /// Existing representation-specific recovery anchors.
    pub representation: BootstrapObjectRepresentation,
}

/// Replayable authority source used for the pre-hash pass and both copies.
///
/// Implementations may stream from a hardened journal or another leased
/// authority. They must not collect all rows merely to satisfy this trait.
pub trait TapeIndexSnapshotRecordSource {
    /// Visit the complete canonical structural prefix in tape-file order.
    fn visit_structural_entries(
        &mut self,
        visitor: &mut dyn FnMut(&TapeIndexSnapshotMapEntry) -> Result<(), ParityError>,
    ) -> Result<(), ParityError>;

    /// Visit every Object recovery row in matching tape-file order.
    fn visit_object_rows(
        &mut self,
        visitor: &mut dyn FnMut(&TapeIndexSnapshotObjectRow) -> Result<(), ParityError>,
    ) -> Result<(), ParityError>;
}

/// Immutable result of the pre-hash/shape pass used for both emitted copies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapeIndexSnapshotPlan {
    /// Repeated snapshot metadata.
    pub descriptor: TapeIndexSnapshotDescriptor,
    /// Exact fixed-slot layout.
    pub layout: TapeIndexSnapshotLayout,
    /// SHA-256 over every fixed slot including zero slot padding.
    pub payload_sha256: [u8; 32],
    /// SHA-256 over the deterministic CBOR structural array projection.
    pub canonical_map_sha256: [u8; 32],
    /// Absolute LBA immediately following the complete covered prefix.
    pub absolute_start_lba: u64,
}

/// Decoded fixed header from one snapshot copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapeIndexSnapshotHeader {
    /// HMAC-derived per-tape header magic.
    pub magic: [u8; 8],
    /// Candidate schema version.
    pub schema_version: u16,
    /// Primary or tail copy discriminator.
    pub copy_kind: TapeIndexSnapshotCopyKind,
    /// Repeated snapshot descriptor.
    pub descriptor: TapeIndexSnapshotDescriptor,
    /// Exact fixed-slot layout.
    pub layout: TapeIndexSnapshotLayout,
    /// Payload digest.
    pub payload_sha256: [u8; 32],
    /// Embedded canonical map digest.
    pub canonical_map_sha256: [u8; 32],
    /// CRC-64/XZ over all earlier fixed header bytes.
    pub header_crc64: u64,
}

/// Decoded final footer locator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapeIndexSnapshotFooter {
    /// HMAC-derived per-tape footer magic.
    pub magic: [u8; 8],
    /// Candidate footer version.
    pub footer_version: u16,
    /// Repeated snapshot descriptor.
    pub descriptor: TapeIndexSnapshotDescriptor,
    /// Exact fixed-slot layout.
    pub layout: TapeIndexSnapshotLayout,
    /// Payload digest.
    pub payload_sha256: [u8; 32],
    /// Embedded canonical map digest.
    pub canonical_map_sha256: [u8; 32],
    /// CRC-64/XZ over all earlier footer bytes.
    pub footer_crc64: u64,
}

/// Complete terminal-bootstrap locator for one snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapeIndexSnapshotReference {
    /// Device partition containing the snapshot.
    pub partition: u32,
    /// Partition-independent absolute start LBA.
    pub absolute_start_lba: u64,
    /// Filemark-delimited tape-file number.
    pub tape_file_number: u64,
    /// Total snapshot blocks before its trailing filemark.
    pub block_count: u64,
    /// Snapshot sequence.
    pub sequence: u64,
    /// Snapshot schema version.
    pub schema_version: u16,
    /// Covered structural scope.
    pub scope: TapeIndexSnapshotScope,
    /// Structural and Object row counts.
    pub counts: TapeIndexSnapshotCounts,
    /// Fixed-slot payload length.
    pub payload_len: u64,
    /// Payload digest.
    pub payload_sha256: [u8; 32],
    /// Embedded canonical map digest.
    pub canonical_map_sha256: [u8; 32],
    /// True for the final inventory.
    pub is_final: bool,
}

/// Calculate the exact fixed-slot snapshot geometry with checked `u64`
/// arithmetic and no row allocation.
pub fn tape_index_snapshot_layout(
    block_size: u32,
    counts: TapeIndexSnapshotCounts,
) -> Result<TapeIndexSnapshotLayout, ParityError> {
    validate_snapshot_block_size(block_size)?;
    let payload_len = checked_snapshot_payload_len(counts)?;
    let shared = checked_replicated_control_layout(
        u64::from(block_size),
        u64::try_from(TAPE_INDEX_SNAPSHOT_HEADER_LEN)
            .map_err(|_| snapshot_error("snapshot header length overflows u64"))?,
        payload_len,
        "tape-index snapshot",
    )?;
    Ok(TapeIndexSnapshotLayout {
        payload_len,
        copy_block_count: shared.copy_block_count,
        total_block_count: shared.total_block_count,
        primary_copy_start_block: shared.primary_copy_start_block,
        tail_copy_start_block: shared.tail_copy_start_block,
        footer_block_index: shared.footer_block_index,
    })
}

/// Derive the HMAC-domain header magic for one tape.
pub fn derive_tape_index_snapshot_header_magic(tape_uuid: &[u8; 16]) -> [u8; 8] {
    derive_snapshot_magic(tape_uuid, TAPE_INDEX_HEADER_MAGIC_MESSAGE)
}

/// Derive the distinct HMAC-domain footer magic for one tape.
pub fn derive_tape_index_snapshot_footer_magic(tape_uuid: &[u8; 16]) -> [u8; 8] {
    derive_snapshot_magic(tape_uuid, TAPE_INDEX_FOOTER_MAGIC_MESSAGE)
}

fn derive_snapshot_magic(tape_uuid: &[u8; 16], message: &[u8]) -> [u8; 8] {
    let mut mac = HmacSha256::new_from_slice(tape_uuid).expect("HMAC accepts any key length");
    mac.update(message);
    let bytes = mac.finalize().into_bytes();
    let mut magic = [0u8; 8];
    magic.copy_from_slice(&bytes[..8]);
    magic
}

/// Validate and pre-hash one replay of the complete source without collecting
/// its rows.
pub fn plan_tape_index_snapshot<S: TapeIndexSnapshotRecordSource>(
    descriptor: TapeIndexSnapshotDescriptor,
    source: &mut S,
) -> Result<TapeIndexSnapshotPlan, ParityError> {
    validate_snapshot_descriptor(&descriptor)?;
    let layout = tape_index_snapshot_layout(descriptor.block_size, descriptor.counts)?;
    let summary = stream_snapshot_payload(source, &descriptor, |_| Ok(()))?;
    if summary.payload_len != layout.payload_len {
        return Err(snapshot_error(format!(
            "streamed payload length {} does not match layout {}",
            summary.payload_len, layout.payload_len
        )));
    }
    Ok(TapeIndexSnapshotPlan {
        descriptor,
        layout,
        payload_sha256: summary.payload_sha256,
        canonical_map_sha256: summary.canonical_map_sha256,
        absolute_start_lba: summary.absolute_start_lba,
    })
}

/// Stream both complete copies and the footer, retaining only one tape block
/// plus one bounded slot in memory.
///
/// The source is replayed for each copy and revalidated against the plan. A
/// changed authority therefore fails the write rather than producing a
/// committed snapshot. Because tape writes are not transactional, blocks from
/// the mismatched copy may already be on media; no footer is emitted, so the
/// incomplete file remains torn-tail evidence rather than usable authority.
pub fn write_tape_index_snapshot<S, F>(
    plan: &TapeIndexSnapshotPlan,
    source: &mut S,
    mut emit_block: F,
) -> Result<(), ParityError>
where
    S: TapeIndexSnapshotRecordSource,
    F: FnMut(&[u8]) -> Result<(), ParityError>,
{
    validate_snapshot_plan(plan)?;
    let mut emitted_total = 0u64;
    for copy_kind in [
        TapeIndexSnapshotCopyKind::Primary,
        TapeIndexSnapshotCopyKind::Tail,
    ] {
        let header = encode_tape_index_snapshot_header(plan, copy_kind)?;
        let mut writer = SnapshotCopyBlockWriter::new(
            plan.descriptor.block_size,
            &mut emit_block,
            plan.layout.copy_block_count,
        )?;
        writer.write_bytes(&header)?;
        let summary =
            stream_snapshot_payload(source, &plan.descriptor, |slot| writer.write_bytes(slot))?;
        validate_replayed_summary(plan, &summary)?;
        let emitted = writer.finish()?;
        emitted_total = emitted_total
            .checked_add(emitted)
            .ok_or_else(|| snapshot_error("emitted snapshot block count overflows u64"))?;
    }

    let footer = encode_tape_index_snapshot_footer_block(plan)?;
    emit_block(&footer)?;
    emitted_total = emitted_total
        .checked_add(1)
        .ok_or_else(|| snapshot_error("emitted snapshot total overflows u64"))?;
    if emitted_total != plan.layout.total_block_count {
        return Err(snapshot_error(format!(
            "emitted {emitted_total} blocks, planned {}",
            plan.layout.total_block_count
        )));
    }
    Ok(())
}

/// Encode the fixed 512-byte header for one planned copy.
///
/// This is deliberately not a complete tape block: the canonical writer puts
/// payload bytes immediately after it in the first block of each copy.
pub fn encode_tape_index_snapshot_header(
    plan: &TapeIndexSnapshotPlan,
    copy_kind: TapeIndexSnapshotCopyKind,
) -> Result<[u8; TAPE_INDEX_SNAPSHOT_HEADER_LEN], ParityError> {
    validate_snapshot_plan(plan)?;
    let mut header = [0u8; TAPE_INDEX_SNAPSHOT_HEADER_LEN];
    header[0x00..0x08].copy_from_slice(&derive_tape_index_snapshot_header_magic(
        &plan.descriptor.tape_uuid,
    ));
    header[0x08..0x0A].copy_from_slice(&TAPE_INDEX_SNAPSHOT_SCHEMA_VERSION.to_le_bytes());
    header[0x0A..0x0C].copy_from_slice(&copy_kind.code().to_le_bytes());
    write_common_frame_fields(&mut header, plan)?;
    let crc = crc64_xz(&header[..TAPE_INDEX_SNAPSHOT_HEADER_CRC_OFFSET]);
    header[TAPE_INDEX_SNAPSHOT_HEADER_CRC_OFFSET..TAPE_INDEX_SNAPSHOT_HEADER_LEN]
        .copy_from_slice(&crc.to_le_bytes());
    Ok(header)
}

/// Encode the final one-block footer locator.
pub fn encode_tape_index_snapshot_footer_block(
    plan: &TapeIndexSnapshotPlan,
) -> Result<Vec<u8>, ParityError> {
    validate_snapshot_plan(plan)?;
    let block_size = validate_snapshot_block_size(plan.descriptor.block_size)?;
    let mut block = vec![0u8; block_size];
    block[0x00..0x08].copy_from_slice(&derive_tape_index_snapshot_footer_magic(
        &plan.descriptor.tape_uuid,
    ));
    block[0x08..0x0A].copy_from_slice(&TAPE_INDEX_SNAPSHOT_FOOTER_VERSION.to_le_bytes());
    write_common_frame_fields(&mut block, plan)?;
    let crc = crc64_xz(&block[..TAPE_INDEX_SNAPSHOT_FOOTER_CRC_OFFSET]);
    block[TAPE_INDEX_SNAPSHOT_FOOTER_CRC_OFFSET..TAPE_INDEX_SNAPSHOT_FOOTER_LEN]
        .copy_from_slice(&crc.to_le_bytes());
    Ok(block)
}

/// Parse and validate one candidate snapshot header block.
pub fn parse_tape_index_snapshot_header_block(
    block: &[u8],
    expected_tape_uuid: &[u8; 16],
) -> Result<TapeIndexSnapshotHeader, ParityError> {
    require_fixed_frame_len(block, TAPE_INDEX_SNAPSHOT_HEADER_LEN, "header")?;
    let expected_magic = derive_tape_index_snapshot_header_magic(expected_tape_uuid);
    if block[..8] != expected_magic {
        return Err(snapshot_error("snapshot header magic mismatch"));
    }
    let schema_version = read_u16_le(block, 0x08);
    if schema_version != TAPE_INDEX_SNAPSHOT_SCHEMA_VERSION {
        return Err(snapshot_error(format!(
            "unsupported snapshot schema version {schema_version}"
        )));
    }
    let copy_kind = TapeIndexSnapshotCopyKind::from_code(read_u16_le(block, 0x0A))?;
    ensure_zero(&block[0x0C..0x10], "snapshot header reserved field")?;
    let header_crc64 = read_u64_le(block, TAPE_INDEX_SNAPSHOT_HEADER_CRC_OFFSET);
    let computed = crc64_xz(&block[..TAPE_INDEX_SNAPSHOT_HEADER_CRC_OFFSET]);
    if header_crc64 != computed {
        return Err(snapshot_error(format!(
            "snapshot header CRC mismatch: stored 0x{header_crc64:016x}, computed 0x{computed:016x}"
        )));
    }
    let common = parse_common_frame_fields(block, expected_tape_uuid)?;
    Ok(TapeIndexSnapshotHeader {
        magic: expected_magic,
        schema_version,
        copy_kind,
        descriptor: common.descriptor,
        layout: common.layout,
        payload_sha256: common.payload_sha256,
        canonical_map_sha256: common.canonical_map_sha256,
        header_crc64,
    })
}

/// Return `None` when a block is outside this tape's snapshot-header domain.
pub fn classify_tape_index_snapshot_header_block(
    block: &[u8],
    expected_tape_uuid: &[u8; 16],
) -> Result<Option<TapeIndexSnapshotHeader>, ParityError> {
    let magic = derive_tape_index_snapshot_header_magic(expected_tape_uuid);
    if block.len() < magic.len() || block[..8] != magic {
        return Ok(None);
    }
    parse_tape_index_snapshot_header_block(block, expected_tape_uuid).map(Some)
}

/// Parse and validate the final candidate footer locator.
pub fn parse_tape_index_snapshot_footer_block(
    block: &[u8],
    expected_tape_uuid: &[u8; 16],
) -> Result<TapeIndexSnapshotFooter, ParityError> {
    require_fixed_frame_len(block, TAPE_INDEX_SNAPSHOT_FOOTER_LEN, "footer")?;
    let expected_magic = derive_tape_index_snapshot_footer_magic(expected_tape_uuid);
    if block[..8] != expected_magic {
        return Err(snapshot_error("snapshot footer magic mismatch"));
    }
    let footer_version = read_u16_le(block, 0x08);
    if footer_version != TAPE_INDEX_SNAPSHOT_FOOTER_VERSION {
        return Err(snapshot_error(format!(
            "unsupported snapshot footer version {footer_version}"
        )));
    }
    if read_u16_le(block, 0x0A) != 0 {
        return Err(snapshot_error(
            "snapshot footer copy-kind field is non-zero",
        ));
    }
    ensure_zero(&block[0x0C..0x10], "snapshot footer reserved field")?;
    let footer_crc64 = read_u64_le(block, TAPE_INDEX_SNAPSHOT_FOOTER_CRC_OFFSET);
    let computed = crc64_xz(&block[..TAPE_INDEX_SNAPSHOT_FOOTER_CRC_OFFSET]);
    if footer_crc64 != computed {
        return Err(snapshot_error(format!(
            "snapshot footer CRC mismatch: stored 0x{footer_crc64:016x}, computed 0x{computed:016x}"
        )));
    }
    ensure_zero(
        &block[TAPE_INDEX_SNAPSHOT_FOOTER_LEN..],
        "snapshot footer block padding",
    )?;
    let common = parse_common_frame_fields(block, expected_tape_uuid)?;
    Ok(TapeIndexSnapshotFooter {
        magic: expected_magic,
        footer_version,
        descriptor: common.descriptor,
        layout: common.layout,
        payload_sha256: common.payload_sha256,
        canonical_map_sha256: common.canonical_map_sha256,
        footer_crc64,
    })
}

impl TapeIndexSnapshotPlan {
    /// Construct the complete terminal-bootstrap reference for this plan.
    pub fn reference(&self, partition: u32) -> TapeIndexSnapshotReference {
        TapeIndexSnapshotReference {
            partition,
            absolute_start_lba: self.absolute_start_lba,
            tape_file_number: self.descriptor.scope.covered_prefix_tape_file_count,
            block_count: self.layout.total_block_count,
            sequence: self.descriptor.sequence,
            schema_version: TAPE_INDEX_SNAPSHOT_SCHEMA_VERSION,
            scope: self.descriptor.scope,
            counts: self.descriptor.counts,
            payload_len: self.layout.payload_len,
            payload_sha256: self.payload_sha256,
            canonical_map_sha256: self.canonical_map_sha256,
            is_final: self.descriptor.is_final,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PayloadSummary {
    payload_len: u64,
    payload_sha256: [u8; 32],
    canonical_map_sha256: [u8; 32],
    absolute_start_lba: u64,
}

fn stream_snapshot_payload<S, F>(
    source: &mut S,
    descriptor: &TapeIndexSnapshotDescriptor,
    mut emit_slot: F,
) -> Result<PayloadSummary, ParityError>
where
    S: TapeIndexSnapshotRecordSource,
    F: FnMut(&[u8]) -> Result<(), ParityError>,
{
    let mut payload_hasher = Sha256::new();
    let mut map_hasher = Sha256::new();
    map_hasher.update(cbor_array_header(descriptor.counts.structural_entry_count));
    let mut map_locator_hasher = Sha256::new();
    let mut row_locator_hasher = Sha256::new();
    let mut structural_count = 0u64;
    let mut object_map_count = 0u64;
    let mut row_count = 0u64;
    let mut expected_tape_file_number = 0u64;
    let mut expected_data_ordinal = 0u64;
    let mut expected_protected_ordinal = 0u64;
    let mut expected_epoch_id = 0u64;
    let mut absolute_start_lba = 0u64;

    source.visit_structural_entries(&mut |entry| {
        validate_snapshot_map_entry(entry)?;
        if entry.tape_file_number != expected_tape_file_number {
            return Err(snapshot_error(format!(
                "structural entry {} is not dense expected tape file {expected_tape_file_number}",
                entry.tape_file_number
            )));
        }
        expected_tape_file_number = expected_tape_file_number
            .checked_add(1)
            .ok_or_else(|| snapshot_error("structural tape-file sequence overflows u64"))?;
        structural_count = structural_count
            .checked_add(1)
            .ok_or_else(|| snapshot_error("structural entry count overflows u64"))?;
        absolute_start_lba = absolute_start_lba
            .checked_add(entry.block_count)
            .and_then(|lba| lba.checked_add(1))
            .ok_or_else(|| {
                snapshot_error("covered-prefix physical extent plus filemark overflows u64")
            })?;
        match entry.kind {
            TapeIndexSnapshotFileKind::Object => {
                let first = entry.first_parity_data_ordinal.ok_or_else(|| {
                    snapshot_error("Object structural entry is missing first data ordinal")
                })?;
                if first != expected_data_ordinal {
                    return Err(snapshot_error(format!(
                        "Object tape file {} begins at ordinal {first}, expected {expected_data_ordinal}",
                        entry.tape_file_number
                    )));
                }
                expected_data_ordinal = expected_data_ordinal
                    .checked_add(entry.block_count)
                    .ok_or_else(|| snapshot_error("Object data ordinal range overflows u64"))?;
                object_map_count = object_map_count
                    .checked_add(1)
                    .ok_or_else(|| snapshot_error("Object map count overflows u64"))?;
                update_locator_digest(
                    &mut map_locator_hasher,
                    entry.tape_file_number,
                    entry.block_count,
                );
            }
            TapeIndexSnapshotFileKind::ParitySidecar => {
                let start = entry.protected_ordinal_start.ok_or_else(|| {
                    snapshot_error("sidecar structural entry is missing protected start")
                })?;
                let end = entry.protected_ordinal_end_exclusive.ok_or_else(|| {
                    snapshot_error("sidecar structural entry is missing protected end")
                })?;
                if start != expected_protected_ordinal {
                    return Err(snapshot_error(format!(
                        "sidecar tape file {} begins protection at {start}, expected {expected_protected_ordinal}",
                        entry.tape_file_number
                    )));
                }
                if entry.epoch_id != Some(expected_epoch_id) {
                    return Err(snapshot_error(format!(
                        "sidecar tape file {} has epoch {:?}, expected {expected_epoch_id}",
                        entry.tape_file_number, entry.epoch_id
                    )));
                }
                expected_protected_ordinal = end;
                expected_epoch_id = expected_epoch_id
                    .checked_add(1)
                    .ok_or_else(|| snapshot_error("sidecar epoch sequence overflows u64"))?;
            }
            TapeIndexSnapshotFileKind::Bootstrap
            | TapeIndexSnapshotFileKind::ParityMap
            | TapeIndexSnapshotFileKind::TapeIndexSnapshot => {}
        }
        let encoded = encode_snapshot_map_entry(entry)?;
        map_hasher.update(&encoded);
        let slot = encode_slot(
            &encoded,
            TAPE_INDEX_STRUCTURAL_SLOT_LEN,
            "structural entry",
        )?;
        payload_hasher.update(&slot);
        emit_slot(&slot)
    })?;

    if structural_count != descriptor.counts.structural_entry_count {
        return Err(snapshot_error(format!(
            "source yielded {structural_count} structural entries, expected {}",
            descriptor.counts.structural_entry_count
        )));
    }
    if expected_data_ordinal != descriptor.scope.total_data_ordinals {
        return Err(snapshot_error(format!(
            "structural map has {expected_data_ordinal} data ordinals, scope declares {}",
            descriptor.scope.total_data_ordinals
        )));
    }
    if expected_protected_ordinal != descriptor.scope.highest_protected_ordinal {
        return Err(snapshot_error(format!(
            "structural map protects through {expected_protected_ordinal}, scope declares {}",
            descriptor.scope.highest_protected_ordinal
        )));
    }

    let mut previous_row_file_number = None;
    source.visit_object_rows(&mut |row| {
        validate_object_recovery_row_fields(
            row.stored_block_count,
            Some(&row.object_id),
            &row.representation,
            Some(descriptor.block_size),
        )
        .map_err(|error| snapshot_error(error.to_string()))?;
        if previous_row_file_number.is_some_and(|previous| row.tape_file_number <= previous) {
            return Err(snapshot_error(
                "Object recovery rows are not in strictly increasing tape-file order",
            ));
        }
        previous_row_file_number = Some(row.tape_file_number);
        row_count = row_count
            .checked_add(1)
            .ok_or_else(|| snapshot_error("Object row count overflows u64"))?;
        update_locator_digest(
            &mut row_locator_hasher,
            row.tape_file_number,
            row.stored_block_count,
        );
        let encoded = encode_snapshot_object_row(row)?;
        let slot = encode_slot(
            &encoded,
            TAPE_INDEX_OBJECT_ROW_SLOT_LEN,
            "Object recovery row",
        )?;
        payload_hasher.update(&slot);
        emit_slot(&slot)
    })?;

    if row_count != descriptor.counts.object_row_count {
        return Err(snapshot_error(format!(
            "source yielded {row_count} Object rows, expected {}",
            descriptor.counts.object_row_count
        )));
    }
    // This bounded-memory exact-order comparison relies on SHA-256 collision
    // resistance: both sides hash the same sequence of little-endian
    // `(tape_file_number, stored_block_count)` pairs. Counts are compared
    // separately so an empty suffix cannot be hidden.
    if object_map_count != row_count
        || map_locator_hasher.finalize() != row_locator_hasher.finalize()
    {
        return Err(snapshot_error(
            "embedded structural map and Object rows are not a bijection",
        ));
    }

    let payload_len = checked_snapshot_payload_len(TapeIndexSnapshotCounts {
        structural_entry_count: structural_count,
        object_row_count: row_count,
    })?;
    Ok(PayloadSummary {
        payload_len,
        payload_sha256: payload_hasher.finalize().into(),
        canonical_map_sha256: map_hasher.finalize().into(),
        absolute_start_lba,
    })
}

fn encode_snapshot_map_entry(entry: &TapeIndexSnapshotMapEntry) -> Result<Vec<u8>, ParityError> {
    encode_cbor_value(
        &CborValue::Array(vec![
            CborValue::Integer(entry.tape_file_number.into()),
            CborValue::Integer(entry.kind.code().into()),
            CborValue::Integer(entry.block_count.into()),
            optional_u64(entry.first_parity_data_ordinal),
            optional_u64(entry.protected_ordinal_start),
            optional_u64(entry.protected_ordinal_end_exclusive),
            optional_u64(entry.epoch_id),
        ]),
        "structural entry",
    )
}

fn encode_snapshot_object_row(row: &TapeIndexSnapshotObjectRow) -> Result<Vec<u8>, ParityError> {
    let mut entries = vec![
        (
            CborValue::Integer(1.into()),
            CborValue::Integer(row.tape_file_number.into()),
        ),
        (
            CborValue::Integer(2.into()),
            CborValue::Text(match row.representation {
                BootstrapObjectRepresentation::Plaintext { .. } => "plaintext".to_string(),
                BootstrapObjectRepresentation::Encrypted { .. } => "encrypted".to_string(),
            }),
        ),
        (
            CborValue::Integer(3.into()),
            CborValue::Integer(row.stored_block_count.into()),
        ),
        (
            CborValue::Integer(4.into()),
            CborValue::Bytes(row.object_id.clone()),
        ),
    ];
    match &row.representation {
        BootstrapObjectRepresentation::Plaintext {
            manifest_first_chunk_lba,
            manifest_size_bytes,
            manifest_chunk_count,
            manifest_sha256,
        } => {
            entries.extend([
                (
                    CborValue::Integer(10.into()),
                    CborValue::Integer((*manifest_first_chunk_lba).into()),
                ),
                (
                    CborValue::Integer(11.into()),
                    CborValue::Integer((*manifest_size_bytes).into()),
                ),
                (
                    CborValue::Integer(12.into()),
                    CborValue::Integer((*manifest_chunk_count).into()),
                ),
                (
                    CborValue::Integer(13.into()),
                    CborValue::Bytes(manifest_sha256.to_vec()),
                ),
            ]);
        }
        BootstrapObjectRepresentation::Encrypted {
            recipient_epoch_ids,
            metadata_frame_len,
            key_frame_len,
        } => {
            entries.extend([
                (
                    CborValue::Integer(21.into()),
                    CborValue::Integer((*metadata_frame_len).into()),
                ),
                (
                    CborValue::Integer(22.into()),
                    CborValue::Array(
                        recipient_epoch_ids
                            .iter()
                            .map(|epoch_id| CborValue::Bytes(epoch_id.to_vec()))
                            .collect(),
                    ),
                ),
                (
                    CborValue::Integer(23.into()),
                    CborValue::Integer((*key_frame_len).into()),
                ),
            ]);
        }
    }
    encode_cbor_value(&CborValue::Map(entries), "Object recovery row")
}

fn encode_cbor_value(value: &CborValue, label: &str) -> Result<Vec<u8>, ParityError> {
    let mut bytes = Vec::new();
    ciborium::into_writer(value, &mut bytes)
        .map_err(|error| snapshot_error(format!("{label} CBOR encode failed: {error}")))?;
    Ok(bytes)
}

fn encode_slot(bytes: &[u8], slot_len: u64, label: &str) -> Result<Vec<u8>, ParityError> {
    let slot_len = usize::try_from(slot_len)
        .map_err(|_| snapshot_error(format!("{label} slot length overflows usize")))?;
    let capacity = slot_len
        .checked_sub(TAPE_INDEX_SLOT_PREFIX_LEN)
        .ok_or_else(|| snapshot_error(format!("{label} slot is shorter than its prefix")))?;
    if bytes.len() > capacity {
        return Err(snapshot_error(format!(
            "{label} encoded length {} exceeds slot capacity {capacity}",
            bytes.len()
        )));
    }
    let encoded_len = u16::try_from(bytes.len())
        .map_err(|_| snapshot_error(format!("{label} encoded length exceeds u16")))?;
    let mut slot = vec![0u8; slot_len];
    slot[..2].copy_from_slice(&encoded_len.to_le_bytes());
    slot[2..2 + bytes.len()].copy_from_slice(bytes);
    Ok(slot)
}

fn optional_u64(value: Option<u64>) -> CborValue {
    value.map_or(CborValue::Null, |value| CborValue::Integer(value.into()))
}

fn cbor_array_header(len: u64) -> Vec<u8> {
    encode_cbor_major_argument(4, len)
}

fn encode_cbor_major_argument(major: u8, argument: u64) -> Vec<u8> {
    let initial = major << 5;
    match argument {
        0..=23 => vec![initial | argument as u8],
        24..=0xff => vec![initial | 24, argument as u8],
        0x100..=0xffff => {
            let mut bytes = vec![initial | 25];
            bytes.extend_from_slice(&(argument as u16).to_be_bytes());
            bytes
        }
        0x1_0000..=0xffff_ffff => {
            let mut bytes = vec![initial | 26];
            bytes.extend_from_slice(&(argument as u32).to_be_bytes());
            bytes
        }
        _ => {
            let mut bytes = vec![initial | 27];
            bytes.extend_from_slice(&argument.to_be_bytes());
            bytes
        }
    }
}

fn validate_snapshot_map_entry(entry: &TapeIndexSnapshotMapEntry) -> Result<(), ParityError> {
    if entry.block_count == 0 {
        return Err(snapshot_error(format!(
            "tape file {} has zero block count",
            entry.tape_file_number
        )));
    }
    let no_object = entry.first_parity_data_ordinal.is_none();
    let no_sidecar = entry.protected_ordinal_start.is_none()
        && entry.protected_ordinal_end_exclusive.is_none()
        && entry.epoch_id.is_none();
    match entry.kind {
        TapeIndexSnapshotFileKind::Object if !no_object && no_sidecar => Ok(()),
        TapeIndexSnapshotFileKind::ParitySidecar => {
            let (Some(start), Some(end), Some(_)) = (
                entry.protected_ordinal_start,
                entry.protected_ordinal_end_exclusive,
                entry.epoch_id,
            ) else {
                return Err(snapshot_error(format!(
                    "sidecar tape file {} is missing range or epoch",
                    entry.tape_file_number
                )));
            };
            if no_object && end > start {
                Ok(())
            } else {
                Err(snapshot_error(format!(
                    "sidecar tape file {} has invalid kind fields",
                    entry.tape_file_number
                )))
            }
        }
        TapeIndexSnapshotFileKind::Bootstrap
            if entry.block_count == 1 && no_object && no_sidecar =>
        {
            Ok(())
        }
        TapeIndexSnapshotFileKind::ParityMap | TapeIndexSnapshotFileKind::TapeIndexSnapshot
            if no_object && no_sidecar =>
        {
            Ok(())
        }
        _ => Err(snapshot_error(format!(
            "tape file {} has fields inconsistent with {:?}",
            entry.tape_file_number, entry.kind
        ))),
    }
}

fn validate_snapshot_counts(counts: TapeIndexSnapshotCounts) -> Result<(), ParityError> {
    if counts.object_row_count > counts.structural_entry_count {
        return Err(snapshot_error(format!(
            "Object row count {} exceeds structural entry count {}",
            counts.object_row_count, counts.structural_entry_count
        )));
    }
    Ok(())
}

fn checked_snapshot_payload_len(counts: TapeIndexSnapshotCounts) -> Result<u64, ParityError> {
    validate_snapshot_counts(counts)?;
    let structural_bytes = counts
        .structural_entry_count
        .checked_mul(TAPE_INDEX_STRUCTURAL_SLOT_LEN)
        .ok_or_else(|| snapshot_error("structural slot byte count overflows u64"))?;
    let row_bytes = counts
        .object_row_count
        .checked_mul(TAPE_INDEX_OBJECT_ROW_SLOT_LEN)
        .ok_or_else(|| snapshot_error("Object-row slot byte count overflows u64"))?;
    structural_bytes
        .checked_add(row_bytes)
        .ok_or_else(|| snapshot_error("snapshot payload length overflows u64"))
}

fn validate_snapshot_descriptor(
    descriptor: &TapeIndexSnapshotDescriptor,
) -> Result<(), ParityError> {
    validate_snapshot_counts(descriptor.counts)?;
    validate_snapshot_block_size(descriptor.block_size)?;
    if descriptor.scope.covered_prefix_tape_file_count != descriptor.counts.structural_entry_count {
        return Err(snapshot_error(format!(
            "covered prefix count {} differs from structural entry count {}",
            descriptor.scope.covered_prefix_tape_file_count,
            descriptor.counts.structural_entry_count
        )));
    }
    if descriptor.scope.highest_protected_ordinal > descriptor.scope.total_data_ordinals {
        return Err(snapshot_error(
            "highest protected ordinal exceeds total data ordinals",
        ));
    }
    validate_writer_version(&descriptor.writer_version)
        .map_err(|bound| snapshot_error(format!("snapshot writer_version violates {bound}")))?;
    validate_write_timestamp(&descriptor.write_timestamp)
        .map_err(|bound| snapshot_error(format!("snapshot write_timestamp violates {bound}")))?;
    Ok(())
}

fn validate_snapshot_plan(plan: &TapeIndexSnapshotPlan) -> Result<(), ParityError> {
    validate_snapshot_descriptor(&plan.descriptor)?;
    let expected = tape_index_snapshot_layout(plan.descriptor.block_size, plan.descriptor.counts)?;
    if plan.layout != expected {
        return Err(snapshot_error(
            "snapshot plan layout does not match descriptor",
        ));
    }
    Ok(())
}

fn validate_replayed_summary(
    plan: &TapeIndexSnapshotPlan,
    summary: &PayloadSummary,
) -> Result<(), ParityError> {
    if summary.payload_len != plan.layout.payload_len
        || summary.payload_sha256 != plan.payload_sha256
        || summary.canonical_map_sha256 != plan.canonical_map_sha256
        || summary.absolute_start_lba != plan.absolute_start_lba
    {
        return Err(snapshot_error(
            "replayed snapshot authority differs from the immutable plan",
        ));
    }
    Ok(())
}

fn update_locator_digest(hasher: &mut Sha256, tape_file_number: u64, block_count: u64) {
    hasher.update(tape_file_number.to_le_bytes());
    hasher.update(block_count.to_le_bytes());
}

fn validate_snapshot_block_size(block_size: u32) -> Result<usize, ParityError> {
    if !TAPE_INDEX_SNAPSHOT_BLOCK_SIZES.contains(&block_size) {
        return Err(snapshot_error(format!(
            "unsupported snapshot block size {block_size}; expected 262144, 524288, or 1048576 bytes"
        )));
    }
    let block_size = usize::try_from(block_size)
        .map_err(|_| snapshot_error("snapshot block size overflows usize"))?;
    Ok(block_size)
}

fn write_common_frame_fields(
    block: &mut [u8],
    plan: &TapeIndexSnapshotPlan,
) -> Result<(), ParityError> {
    validate_snapshot_block_size(plan.descriptor.block_size)?;
    if block.len() < TAPE_INDEX_SNAPSHOT_HEADER_LEN {
        return Err(snapshot_error(
            "snapshot common frame is shorter than the fixed header",
        ));
    }
    let descriptor = &plan.descriptor;
    block[0x10..0x20].copy_from_slice(&descriptor.tape_uuid);
    block[0x20..0x28].copy_from_slice(&descriptor.sequence.to_le_bytes());
    block[0x28..0x2C].copy_from_slice(&descriptor.block_size.to_le_bytes());
    let flags = if descriptor.is_final {
        TAPE_INDEX_FLAG_FINAL
    } else {
        0
    };
    block[0x2C..0x30].copy_from_slice(&flags.to_le_bytes());
    block[0x30..0x38].copy_from_slice(
        &descriptor
            .scope
            .covered_prefix_tape_file_count
            .to_le_bytes(),
    );
    block[0x38..0x40].copy_from_slice(&descriptor.scope.total_data_ordinals.to_le_bytes());
    block[0x40..0x48].copy_from_slice(&descriptor.scope.highest_protected_ordinal.to_le_bytes());
    block[0x48..0x50].copy_from_slice(&descriptor.counts.structural_entry_count.to_le_bytes());
    block[0x50..0x58].copy_from_slice(&descriptor.counts.object_row_count.to_le_bytes());
    block[0x58..0x60].copy_from_slice(&plan.layout.payload_len.to_le_bytes());
    block[0x60..0x80].copy_from_slice(&plan.payload_sha256);
    block[0x80..0xA0].copy_from_slice(&plan.canonical_map_sha256);
    block[0xA0..0xA8].copy_from_slice(&plan.layout.copy_block_count.to_le_bytes());
    block[0xA8..0xB0].copy_from_slice(&plan.layout.total_block_count.to_le_bytes());
    block[0xB0..0xB8].copy_from_slice(&plan.layout.primary_copy_start_block.to_le_bytes());
    block[0xB8..0xC0].copy_from_slice(&plan.layout.tail_copy_start_block.to_le_bytes());
    block[0xC0..0xC8].copy_from_slice(&plan.layout.footer_block_index.to_le_bytes());

    let writer_len = u16::try_from(descriptor.writer_version.len())
        .map_err(|_| snapshot_error("snapshot writer_version length exceeds u16"))?;
    let timestamp_len = u16::try_from(descriptor.write_timestamp.len())
        .map_err(|_| snapshot_error("snapshot write_timestamp length exceeds u16"))?;
    block[WRITER_VERSION_LEN_OFFSET..WRITE_TIMESTAMP_LEN_OFFSET]
        .copy_from_slice(&writer_len.to_le_bytes());
    block[WRITE_TIMESTAMP_LEN_OFFSET..DIAGNOSTIC_RESERVED_OFFSET]
        .copy_from_slice(&timestamp_len.to_le_bytes());
    block[WRITER_VERSION_OFFSET..WRITER_VERSION_OFFSET + descriptor.writer_version.len()]
        .copy_from_slice(descriptor.writer_version.as_bytes());
    block[WRITE_TIMESTAMP_OFFSET..WRITE_TIMESTAMP_OFFSET + descriptor.write_timestamp.len()]
        .copy_from_slice(descriptor.write_timestamp.as_bytes());
    Ok(())
}

#[derive(Debug)]
struct CommonFrameFields {
    descriptor: TapeIndexSnapshotDescriptor,
    layout: TapeIndexSnapshotLayout,
    payload_sha256: [u8; 32],
    canonical_map_sha256: [u8; 32],
}

fn parse_common_frame_fields(
    block: &[u8],
    expected_tape_uuid: &[u8; 16],
) -> Result<CommonFrameFields, ParityError> {
    let mut tape_uuid = [0u8; 16];
    tape_uuid.copy_from_slice(&block[0x10..0x20]);
    if &tape_uuid != expected_tape_uuid {
        return Err(snapshot_error("snapshot tape UUID mismatch"));
    }
    let block_size = read_u32_le(block, 0x28);
    if usize::try_from(block_size).ok() != Some(block.len()) {
        return Err(snapshot_error(format!(
            "snapshot block size {block_size} does not match block length {}",
            block.len()
        )));
    }
    validate_snapshot_block_size(block_size)?;
    let flags = read_u32_le(block, 0x2C);
    if flags & !TAPE_INDEX_KNOWN_FLAGS != 0 {
        return Err(snapshot_error(format!(
            "snapshot frame has unknown flags 0x{:08x}",
            flags & !TAPE_INDEX_KNOWN_FLAGS
        )));
    }
    let scope = TapeIndexSnapshotScope {
        covered_prefix_tape_file_count: read_u64_le(block, 0x30),
        total_data_ordinals: read_u64_le(block, 0x38),
        highest_protected_ordinal: read_u64_le(block, 0x40),
    };
    let counts = TapeIndexSnapshotCounts {
        structural_entry_count: read_u64_le(block, 0x48),
        object_row_count: read_u64_le(block, 0x50),
    };
    let payload_len = read_u64_le(block, 0x58);
    let mut payload_sha256 = [0u8; 32];
    payload_sha256.copy_from_slice(&block[0x60..0x80]);
    let mut canonical_map_sha256 = [0u8; 32];
    canonical_map_sha256.copy_from_slice(&block[0x80..0xA0]);
    let layout = TapeIndexSnapshotLayout {
        payload_len,
        copy_block_count: read_u64_le(block, 0xA0),
        total_block_count: read_u64_le(block, 0xA8),
        primary_copy_start_block: read_u64_le(block, 0xB0),
        tail_copy_start_block: read_u64_le(block, 0xB8),
        footer_block_index: read_u64_le(block, 0xC0),
    };
    ensure_zero(
        &block[DIAGNOSTIC_RESERVED_OFFSET..WRITER_VERSION_OFFSET],
        "snapshot diagnostic reserved field",
    )?;
    let writer_len = usize::from(read_u16_le(block, WRITER_VERSION_LEN_OFFSET));
    let timestamp_len = usize::from(read_u16_le(block, WRITE_TIMESTAMP_LEN_OFFSET));
    if writer_len > WRITER_VERSION_MAX_BYTES || timestamp_len > WRITE_TIMESTAMP_MAX_BYTES {
        return Err(snapshot_error(
            "snapshot diagnostic string length exceeds fixed slot",
        ));
    }
    let writer_end = WRITER_VERSION_OFFSET
        .checked_add(writer_len)
        .ok_or_else(|| snapshot_error("snapshot writer slot end overflows"))?;
    let timestamp_end = WRITE_TIMESTAMP_OFFSET
        .checked_add(timestamp_len)
        .ok_or_else(|| snapshot_error("snapshot timestamp slot end overflows"))?;
    ensure_zero(
        &block[writer_end..WRITE_TIMESTAMP_OFFSET],
        "snapshot writer slot padding",
    )?;
    ensure_zero(
        &block[timestamp_end..FIXED_RESERVED_OFFSET],
        "snapshot timestamp slot padding",
    )?;
    ensure_zero(
        &block[FIXED_RESERVED_OFFSET..TAPE_INDEX_SNAPSHOT_HEADER_CRC_OFFSET],
        "snapshot fixed reserved bytes",
    )?;
    let writer_version = std::str::from_utf8(&block[WRITER_VERSION_OFFSET..writer_end])
        .map_err(|_| snapshot_error("snapshot writer_version is not UTF-8"))?
        .to_string();
    let write_timestamp = std::str::from_utf8(&block[WRITE_TIMESTAMP_OFFSET..timestamp_end])
        .map_err(|_| snapshot_error("snapshot write_timestamp is not UTF-8"))?
        .to_string();
    let descriptor = TapeIndexSnapshotDescriptor {
        tape_uuid,
        sequence: read_u64_le(block, 0x20),
        block_size,
        scope,
        counts,
        is_final: flags & TAPE_INDEX_FLAG_FINAL != 0,
        writer_version,
        write_timestamp,
    };
    validate_snapshot_descriptor(&descriptor)?;
    let expected_layout = tape_index_snapshot_layout(block_size, counts)?;
    validate_replicated_control_layout(
        u64::from(block_size),
        u64::try_from(TAPE_INDEX_SNAPSHOT_HEADER_LEN)
            .map_err(|_| snapshot_error("snapshot header length overflows u64"))?,
        payload_len,
        layout.copy_block_count,
        layout.total_block_count,
        layout.primary_copy_start_block,
        layout.tail_copy_start_block,
        layout.footer_block_index,
        "tape-index snapshot",
    )?;
    if layout != expected_layout {
        return Err(snapshot_error(
            "snapshot locator payload/count geometry is inconsistent",
        ));
    }
    Ok(CommonFrameFields {
        descriptor,
        layout,
        payload_sha256,
        canonical_map_sha256,
    })
}

struct SnapshotCopyBlockWriter<'a, F>
where
    F: FnMut(&[u8]) -> Result<(), ParityError>,
{
    emit_block: &'a mut F,
    block: Vec<u8>,
    used: usize,
    emitted: u64,
    expected_blocks: u64,
}

impl<'a, F> SnapshotCopyBlockWriter<'a, F>
where
    F: FnMut(&[u8]) -> Result<(), ParityError>,
{
    fn new(
        block_size: u32,
        emit_block: &'a mut F,
        expected_blocks: u64,
    ) -> Result<Self, ParityError> {
        let block_size = validate_snapshot_block_size(block_size)?;
        if expected_blocks == 0 {
            return Err(snapshot_error(
                "snapshot copy must contain at least one block",
            ));
        }
        Ok(Self {
            emit_block,
            block: vec![0u8; block_size],
            used: 0,
            emitted: 0,
            expected_blocks,
        })
    }

    fn write_bytes(&mut self, mut bytes: &[u8]) -> Result<(), ParityError> {
        while !bytes.is_empty() {
            let available = self.block.len() - self.used;
            let take = available.min(bytes.len());
            self.block[self.used..self.used + take].copy_from_slice(&bytes[..take]);
            self.used += take;
            bytes = &bytes[take..];
            if self.used == self.block.len() {
                self.flush_block()?;
            }
        }
        Ok(())
    }

    fn flush_block(&mut self) -> Result<(), ParityError> {
        (self.emit_block)(&self.block)?;
        self.emitted = self
            .emitted
            .checked_add(1)
            .ok_or_else(|| snapshot_error("snapshot emitted block count overflows u64"))?;
        self.block.fill(0);
        self.used = 0;
        Ok(())
    }

    fn finish(mut self) -> Result<u64, ParityError> {
        if self.used != 0 {
            self.flush_block()?;
        }
        if self.emitted != self.expected_blocks {
            return Err(snapshot_error(format!(
                "snapshot copy emitted {} blocks, expected {}",
                self.emitted, self.expected_blocks
            )));
        }
        Ok(self.emitted)
    }
}

fn require_fixed_frame_len(block: &[u8], fixed_len: usize, label: &str) -> Result<(), ParityError> {
    if block.len() < fixed_len {
        return Err(snapshot_error(format!(
            "snapshot {label} block has {} bytes, needs at least {fixed_len}",
            block.len()
        )));
    }
    Ok(())
}

fn ensure_zero(bytes: &[u8], label: &str) -> Result<(), ParityError> {
    if let Some((offset, byte)) = bytes
        .iter()
        .copied()
        .enumerate()
        .find(|(_, byte)| *byte != 0)
    {
        return Err(snapshot_error(format!(
            "{label} is non-zero at offset {offset}: 0x{byte:02x}"
        )));
    }
    Ok(())
}

fn read_u16_le(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn read_u32_le(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_u64_le(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn snapshot_error(message: impl Into<String>) -> ParityError {
    ParityError::TapeIndexSnapshot(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAPE_UUID: [u8; 16] = [0x5A; 16];
    const BLOCK_SIZE: u32 = 256 * 1024;

    #[derive(Clone)]
    struct VecRecordSource {
        entries: Vec<TapeIndexSnapshotMapEntry>,
        rows: Vec<TapeIndexSnapshotObjectRow>,
        map_passes: usize,
        row_passes: usize,
    }

    impl TapeIndexSnapshotRecordSource for VecRecordSource {
        fn visit_structural_entries(
            &mut self,
            visitor: &mut dyn FnMut(&TapeIndexSnapshotMapEntry) -> Result<(), ParityError>,
        ) -> Result<(), ParityError> {
            self.map_passes += 1;
            for entry in &self.entries {
                visitor(entry)?;
            }
            Ok(())
        }

        fn visit_object_rows(
            &mut self,
            visitor: &mut dyn FnMut(&TapeIndexSnapshotObjectRow) -> Result<(), ParityError>,
        ) -> Result<(), ParityError> {
            self.row_passes += 1;
            for row in &self.rows {
                visitor(row)?;
            }
            Ok(())
        }
    }

    fn sample_entries() -> Vec<TapeIndexSnapshotMapEntry> {
        vec![
            TapeIndexSnapshotMapEntry {
                tape_file_number: 0,
                kind: TapeIndexSnapshotFileKind::Bootstrap,
                block_count: 1,
                first_parity_data_ordinal: None,
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                epoch_id: None,
            },
            TapeIndexSnapshotMapEntry {
                tape_file_number: 1,
                kind: TapeIndexSnapshotFileKind::Object,
                block_count: 2,
                first_parity_data_ordinal: Some(0),
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                epoch_id: None,
            },
            TapeIndexSnapshotMapEntry {
                tape_file_number: 2,
                kind: TapeIndexSnapshotFileKind::ParitySidecar,
                block_count: 5,
                first_parity_data_ordinal: None,
                protected_ordinal_start: Some(0),
                protected_ordinal_end_exclusive: Some(2),
                epoch_id: Some(0),
            },
            TapeIndexSnapshotMapEntry {
                tape_file_number: 3,
                kind: TapeIndexSnapshotFileKind::ParityMap,
                block_count: 3,
                first_parity_data_ordinal: None,
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                epoch_id: None,
            },
            TapeIndexSnapshotMapEntry {
                tape_file_number: 4,
                kind: TapeIndexSnapshotFileKind::TapeIndexSnapshot,
                block_count: 5,
                first_parity_data_ordinal: None,
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                epoch_id: None,
            },
        ]
    }

    fn sample_rows() -> Vec<TapeIndexSnapshotObjectRow> {
        vec![TapeIndexSnapshotObjectRow {
            tape_file_number: 1,
            stored_block_count: 2,
            object_id: b"object-1".to_vec(),
            representation: BootstrapObjectRepresentation::Plaintext {
                manifest_first_chunk_lba: 0,
                manifest_size_bytes: 1,
                manifest_chunk_count: 1,
                manifest_sha256: [0xA5; 32],
            },
        }]
    }

    fn sample_descriptor() -> TapeIndexSnapshotDescriptor {
        TapeIndexSnapshotDescriptor {
            tape_uuid: TAPE_UUID,
            sequence: 9,
            block_size: BLOCK_SIZE,
            scope: TapeIndexSnapshotScope {
                covered_prefix_tape_file_count: 5,
                total_data_ordinals: 2,
                highest_protected_ordinal: 2,
            },
            counts: TapeIndexSnapshotCounts {
                structural_entry_count: 5,
                object_row_count: 1,
            },
            is_final: true,
            writer_version: "test-writer".to_string(),
            write_timestamp: "2026-08-09T00:00:00Z".to_string(),
        }
    }

    fn sample_source() -> VecRecordSource {
        VecRecordSource {
            entries: sample_entries(),
            rows: sample_rows(),
            map_passes: 0,
            row_passes: 0,
        }
    }

    #[test]
    fn checked_layout_is_exact_for_supported_block_sizes() {
        let counts = TapeIndexSnapshotCounts {
            structural_entry_count: 5,
            object_row_count: 1,
        };
        for block_size in [256 * 1024, 512 * 1024, 1024 * 1024] {
            let layout = tape_index_snapshot_layout(block_size, counts).unwrap();
            assert_eq!(layout.payload_len, 5 * 64 + 256);
            assert_eq!(layout.copy_block_count, 1);
            assert_eq!(layout.total_block_count, 3);
            assert_eq!(layout.footer_block_index, 2);
        }

        for hostile in [0, 512, u32::MAX] {
            let error = tape_index_snapshot_layout(hostile, counts).unwrap_err();
            assert!(error
                .to_string()
                .contains("unsupported snapshot block size"));
        }
    }

    #[test]
    fn checked_layout_rejects_counts_and_all_payload_overflows() {
        let bad_relation = tape_index_snapshot_layout(
            BLOCK_SIZE,
            TapeIndexSnapshotCounts {
                structural_entry_count: 0,
                object_row_count: 1,
            },
        )
        .unwrap_err();
        assert!(bad_relation.to_string().contains("exceeds structural"));

        let structural_overflow = tape_index_snapshot_layout(
            BLOCK_SIZE,
            TapeIndexSnapshotCounts {
                structural_entry_count: u64::MAX,
                object_row_count: 0,
            },
        )
        .unwrap_err();
        assert!(structural_overflow.to_string().contains("structural slot"));

        let row_count = u64::MAX / TAPE_INDEX_OBJECT_ROW_SLOT_LEN + 1;
        let row_overflow = tape_index_snapshot_layout(
            BLOCK_SIZE,
            TapeIndexSnapshotCounts {
                structural_entry_count: u64::MAX / TAPE_INDEX_STRUCTURAL_SLOT_LEN,
                object_row_count: row_count,
            },
        )
        .unwrap_err();
        assert!(row_overflow.to_string().contains("Object-row slot"));

        let addition_overflow = tape_index_snapshot_layout(
            BLOCK_SIZE,
            TapeIndexSnapshotCounts {
                structural_entry_count: u64::MAX / TAPE_INDEX_STRUCTURAL_SLOT_LEN,
                object_row_count: 1,
            },
        )
        .unwrap_err();
        assert!(addition_overflow.to_string().contains("payload length"));
    }

    #[test]
    fn worst_case_canonical_records_fit_fixed_slots() {
        let map_entries = [
            TapeIndexSnapshotMapEntry {
                tape_file_number: u64::MAX,
                kind: TapeIndexSnapshotFileKind::Object,
                block_count: u64::MAX,
                first_parity_data_ordinal: Some(u64::MAX),
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                epoch_id: None,
            },
            TapeIndexSnapshotMapEntry {
                tape_file_number: u64::MAX,
                kind: TapeIndexSnapshotFileKind::ParitySidecar,
                block_count: u64::MAX,
                first_parity_data_ordinal: None,
                protected_ordinal_start: Some(u64::MAX - 1),
                protected_ordinal_end_exclusive: Some(u64::MAX),
                epoch_id: Some(u64::MAX),
            },
            TapeIndexSnapshotMapEntry {
                tape_file_number: u64::MAX,
                kind: TapeIndexSnapshotFileKind::Bootstrap,
                block_count: 1,
                first_parity_data_ordinal: None,
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                epoch_id: None,
            },
            TapeIndexSnapshotMapEntry {
                tape_file_number: u64::MAX,
                kind: TapeIndexSnapshotFileKind::ParityMap,
                block_count: u64::MAX,
                first_parity_data_ordinal: None,
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                epoch_id: None,
            },
            TapeIndexSnapshotMapEntry {
                tape_file_number: u64::MAX,
                kind: TapeIndexSnapshotFileKind::TapeIndexSnapshot,
                block_count: u64::MAX,
                first_parity_data_ordinal: None,
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                epoch_id: None,
            },
        ];
        let expected_map_lengths = [32, 48, 16, 24, 24];
        for (entry, expected_len) in map_entries.iter().zip(expected_map_lengths) {
            validate_snapshot_map_entry(entry).unwrap();
            let map_bytes = encode_snapshot_map_entry(entry).unwrap();
            assert_eq!(map_bytes.len(), expected_len);
            assert!(map_bytes.len() <= TAPE_INDEX_STRUCTURAL_ENTRY_MAX_LEN);
            encode_slot(
                &map_bytes,
                TAPE_INDEX_STRUCTURAL_SLOT_LEN,
                "structural entry",
            )
            .unwrap();
        }

        let plaintext_row = TapeIndexSnapshotObjectRow {
            tape_file_number: u64::MAX,
            stored_block_count: u64::MAX,
            object_id: vec![0xFF; 64],
            representation: BootstrapObjectRepresentation::Plaintext {
                manifest_first_chunk_lba: 0x1_0000_0000,
                manifest_size_bytes: 0x1_0000_0000,
                manifest_chunk_count: 0x1_0000_0000,
                manifest_sha256: [0xFF; 32],
            },
        };
        validate_object_recovery_row_fields(
            plaintext_row.stored_block_count,
            Some(&plaintext_row.object_id),
            &plaintext_row.representation,
            Some(BLOCK_SIZE),
        )
        .unwrap();
        let plaintext_bytes = encode_snapshot_object_row(&plaintext_row).unwrap();
        assert_eq!(plaintext_bytes.len(), 164);
        assert!(plaintext_bytes.len() <= TAPE_INDEX_OBJECT_ROW_MAX_LEN);

        let encrypted_row = TapeIndexSnapshotObjectRow {
            tape_file_number: u64::MAX,
            stored_block_count: u64::MAX,
            object_id: vec![0xFF; 64],
            representation: BootstrapObjectRepresentation::Encrypted {
                recipient_epoch_ids: (1u8..=8).map(|value| [value; 16]).collect(),
                metadata_frame_len: 16 * 1024 * 1024,
                key_frame_len: 16_384,
            },
        };
        validate_object_recovery_row_fields(
            encrypted_row.stored_block_count,
            Some(&encrypted_row.object_id),
            &encrypted_row.representation,
            Some(BLOCK_SIZE),
        )
        .unwrap();
        let encrypted_bytes = encode_snapshot_object_row(&encrypted_row).unwrap();
        assert_eq!(encrypted_bytes.len(), 247);
        assert!(encrypted_bytes.len() <= TAPE_INDEX_OBJECT_ROW_MAX_LEN);
        for row_bytes in [&plaintext_bytes, &encrypted_bytes] {
            encode_slot(
                row_bytes,
                TAPE_INDEX_OBJECT_ROW_SLOT_LEN,
                "Object recovery row",
            )
            .unwrap();
        }

        let over = vec![0u8; TAPE_INDEX_OBJECT_ROW_MAX_LEN + 1];
        let error =
            encode_slot(&over, TAPE_INDEX_OBJECT_ROW_SLOT_LEN, "Object recovery row").unwrap_err();
        assert!(error.to_string().contains("exceeds slot capacity"));
    }

    #[test]
    fn plan_and_writer_replay_without_collecting_the_full_snapshot() {
        let mut source = sample_source();
        let plan = plan_tape_index_snapshot(sample_descriptor(), &mut source).unwrap();
        assert_eq!(source.map_passes, 1);
        assert_eq!(source.row_passes, 1);
        assert_eq!(plan.layout.payload_len, 576);
        assert_eq!(plan.layout.copy_block_count, 1);
        assert_eq!(plan.layout.total_block_count, 3);
        assert_eq!(plan.absolute_start_lba, 21);

        let mut blocks = Vec::new();
        write_tape_index_snapshot(&plan, &mut source, |block| {
            blocks.push(block.to_vec());
            Ok(())
        })
        .unwrap();
        assert_eq!(source.map_passes, 3);
        assert_eq!(source.row_passes, 3);
        assert_eq!(blocks.len(), 3);
        assert!(blocks
            .iter()
            .all(|block| block.len() == BLOCK_SIZE as usize));

        let primary = parse_tape_index_snapshot_header_block(&blocks[0], &TAPE_UUID).unwrap();
        assert_eq!(primary.copy_kind, TapeIndexSnapshotCopyKind::Primary);
        assert_eq!(primary.descriptor, plan.descriptor);
        assert_eq!(primary.payload_sha256, plan.payload_sha256);

        let tail_start = usize::try_from(plan.layout.tail_copy_start_block).unwrap();
        let tail = parse_tape_index_snapshot_header_block(&blocks[tail_start], &TAPE_UUID).unwrap();
        assert_eq!(tail.copy_kind, TapeIndexSnapshotCopyKind::Tail);
        let footer =
            parse_tape_index_snapshot_footer_block(blocks.last().unwrap(), &TAPE_UUID).unwrap();
        assert_eq!(footer.layout, plan.layout);
        assert_eq!(footer.canonical_map_sha256, plan.canonical_map_sha256);

        let copy_bytes =
            |start: usize| blocks[start..start + plan.layout.copy_block_count as usize].concat();
        let primary_bytes = copy_bytes(0);
        let tail_bytes = copy_bytes(tail_start);
        let payload_end = TAPE_INDEX_SNAPSHOT_HEADER_LEN + plan.layout.payload_len as usize;
        assert_eq!(
            &primary_bytes[TAPE_INDEX_SNAPSHOT_HEADER_LEN..payload_end],
            &tail_bytes[TAPE_INDEX_SNAPSHOT_HEADER_LEN..payload_end]
        );
        assert!(primary_bytes[payload_end..].iter().all(|byte| *byte == 0));
        assert!(tail_bytes[payload_end..].iter().all(|byte| *byte == 0));

        let first_slot =
            &primary_bytes[TAPE_INDEX_SNAPSHOT_HEADER_LEN..TAPE_INDEX_SNAPSHOT_HEADER_LEN + 64];
        let encoded_len = usize::from(u16::from_le_bytes(first_slot[..2].try_into().unwrap()));
        assert!(first_slot[2 + encoded_len..].iter().all(|byte| *byte == 0));

        let reference = plan.reference(0);
        assert_eq!(reference.block_count, plan.layout.total_block_count);
        assert_eq!(reference.payload_len, plan.layout.payload_len);
        assert_eq!(reference.tape_file_number, 5);
        assert_eq!(reference.absolute_start_lba, 21);
    }

    struct MutatingReplaySource {
        entries: Vec<TapeIndexSnapshotMapEntry>,
        rows: Vec<TapeIndexSnapshotObjectRow>,
        row_passes: usize,
        mutate_row_pass: usize,
    }

    impl TapeIndexSnapshotRecordSource for MutatingReplaySource {
        fn visit_structural_entries(
            &mut self,
            visitor: &mut dyn FnMut(&TapeIndexSnapshotMapEntry) -> Result<(), ParityError>,
        ) -> Result<(), ParityError> {
            for entry in &self.entries {
                visitor(entry)?;
            }
            Ok(())
        }

        fn visit_object_rows(
            &mut self,
            visitor: &mut dyn FnMut(&TapeIndexSnapshotObjectRow) -> Result<(), ParityError>,
        ) -> Result<(), ParityError> {
            self.row_passes += 1;
            for row in &self.rows {
                if self.row_passes == self.mutate_row_pass {
                    let mut changed = row.clone();
                    changed.object_id = b"object-2".to_vec();
                    visitor(&changed)?;
                } else {
                    visitor(row)?;
                }
            }
            Ok(())
        }
    }

    #[test]
    fn replay_mutation_leaves_no_authoritative_footer() {
        for mutate_row_pass in [2, 3] {
            let mut source = MutatingReplaySource {
                entries: sample_entries(),
                rows: sample_rows(),
                row_passes: 0,
                mutate_row_pass,
            };
            let plan = plan_tape_index_snapshot(sample_descriptor(), &mut source).unwrap();
            let footer_magic = derive_tape_index_snapshot_footer_magic(&TAPE_UUID);
            let mut blocks = Vec::new();
            let error = write_tape_index_snapshot(&plan, &mut source, |block| {
                blocks.push(block.to_vec());
                Ok(())
            })
            .unwrap_err();
            assert!(error.to_string().contains("immutable plan"));
            assert!(blocks.len() < plan.layout.total_block_count as usize);
            assert!(blocks.iter().all(|block| block[..8] != footer_magic));
        }
    }

    #[test]
    fn emitter_failure_at_each_block_boundary_never_commits_a_footer() {
        let mut planning_source = sample_source();
        let plan = plan_tape_index_snapshot(sample_descriptor(), &mut planning_source).unwrap();
        let footer_magic = derive_tape_index_snapshot_footer_magic(&TAPE_UUID);

        for fail_at in 0..=plan.layout.footer_block_index {
            let mut source = sample_source();
            let mut calls = 0u64;
            let mut accepted = Vec::new();
            let error = write_tape_index_snapshot(&plan, &mut source, |block| {
                let block_index = calls;
                calls += 1;
                if block_index == fail_at {
                    return Err(snapshot_error("injected emitter failure"));
                }
                accepted.push(block.to_vec());
                Ok(())
            })
            .unwrap_err();
            assert!(error.to_string().contains("injected emitter failure"));
            assert_eq!(calls, fail_at + 1);
            assert_eq!(accepted.len(), fail_at as usize);
            assert!(accepted.iter().all(|block| block[..8] != footer_magic));
        }
    }

    #[test]
    fn distinct_footer_domain_classifies_damaged_head_as_control_evidence() {
        let mut source = sample_source();
        let plan = plan_tape_index_snapshot(sample_descriptor(), &mut source).unwrap();
        let mut header =
            encode_tape_index_snapshot_header(&plan, TapeIndexSnapshotCopyKind::Primary).unwrap();
        let footer = encode_tape_index_snapshot_footer_block(&plan).unwrap();
        header[0] ^= 1;

        assert!(
            classify_tape_index_snapshot_header_block(&header, &TAPE_UUID)
                .unwrap()
                .is_none()
        );
        let parsed_footer = parse_tape_index_snapshot_footer_block(&footer, &TAPE_UUID).unwrap();
        assert_eq!(parsed_footer.descriptor.sequence, plan.descriptor.sequence);
        assert!(
            classify_tape_index_snapshot_header_block(&footer, &TAPE_UUID)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn parser_rejects_hostile_counts_before_any_payload_allocation() {
        let mut source = sample_source();
        let plan = plan_tape_index_snapshot(sample_descriptor(), &mut source).unwrap();
        let header =
            encode_tape_index_snapshot_header(&plan, TapeIndexSnapshotCopyKind::Primary).unwrap();
        let mut block = vec![0u8; BLOCK_SIZE as usize];
        block[..TAPE_INDEX_SNAPSHOT_HEADER_LEN].copy_from_slice(&header);
        block[0x30..0x38].copy_from_slice(&u64::MAX.to_le_bytes());
        block[0x48..0x50].copy_from_slice(&u64::MAX.to_le_bytes());
        let crc = crc64_xz(&block[..TAPE_INDEX_SNAPSHOT_HEADER_CRC_OFFSET]);
        block[TAPE_INDEX_SNAPSHOT_HEADER_CRC_OFFSET..TAPE_INDEX_SNAPSHOT_HEADER_LEN]
            .copy_from_slice(&crc.to_le_bytes());

        let error = parse_tape_index_snapshot_header_block(&block, &TAPE_UUID).unwrap_err();
        assert!(error.to_string().contains("structural slot"));
    }

    #[test]
    fn plan_rejects_map_row_mismatch_and_wrong_scope() {
        let mut mismatch = sample_source();
        mismatch.rows[0].stored_block_count = 1;
        let error = plan_tape_index_snapshot(sample_descriptor(), &mut mismatch).unwrap_err();
        assert!(error.to_string().contains("not a bijection"));

        let mut descriptor = sample_descriptor();
        descriptor.scope.covered_prefix_tape_file_count = 4;
        let error = plan_tape_index_snapshot(descriptor, &mut sample_source()).unwrap_err();
        assert!(error.to_string().contains("covered prefix count"));
    }

    #[test]
    fn plan_rejects_covered_prefix_physical_extent_overflow() {
        let mut source = VecRecordSource {
            entries: vec![
                TapeIndexSnapshotMapEntry {
                    tape_file_number: 0,
                    kind: TapeIndexSnapshotFileKind::Bootstrap,
                    block_count: 1,
                    first_parity_data_ordinal: None,
                    protected_ordinal_start: None,
                    protected_ordinal_end_exclusive: None,
                    epoch_id: None,
                },
                TapeIndexSnapshotMapEntry {
                    tape_file_number: 1,
                    kind: TapeIndexSnapshotFileKind::ParityMap,
                    block_count: u64::MAX,
                    first_parity_data_ordinal: None,
                    protected_ordinal_start: None,
                    protected_ordinal_end_exclusive: None,
                    epoch_id: None,
                },
            ],
            rows: Vec::new(),
            map_passes: 0,
            row_passes: 0,
        };
        let descriptor = TapeIndexSnapshotDescriptor {
            tape_uuid: TAPE_UUID,
            sequence: 1,
            block_size: BLOCK_SIZE,
            scope: TapeIndexSnapshotScope {
                covered_prefix_tape_file_count: 2,
                total_data_ordinals: 0,
                highest_protected_ordinal: 0,
            },
            counts: TapeIndexSnapshotCounts {
                structural_entry_count: 2,
                object_row_count: 0,
            },
            is_final: false,
            writer_version: "overflow-test".to_string(),
            write_timestamp: "2026-08-09T00:00:00Z".to_string(),
        };
        let error = plan_tape_index_snapshot(descriptor, &mut source).unwrap_err();
        assert!(error.to_string().contains("physical extent plus filemark"));
    }

    struct SyntheticSource {
        count: u64,
    }

    impl TapeIndexSnapshotRecordSource for SyntheticSource {
        fn visit_structural_entries(
            &mut self,
            visitor: &mut dyn FnMut(&TapeIndexSnapshotMapEntry) -> Result<(), ParityError>,
        ) -> Result<(), ParityError> {
            for number in 0..self.count {
                visitor(&TapeIndexSnapshotMapEntry {
                    tape_file_number: number,
                    kind: TapeIndexSnapshotFileKind::Object,
                    block_count: 1,
                    first_parity_data_ordinal: Some(number),
                    protected_ordinal_start: None,
                    protected_ordinal_end_exclusive: None,
                    epoch_id: None,
                })?;
            }
            Ok(())
        }

        fn visit_object_rows(
            &mut self,
            visitor: &mut dyn FnMut(&TapeIndexSnapshotObjectRow) -> Result<(), ParityError>,
        ) -> Result<(), ParityError> {
            for number in 0..self.count {
                visitor(&TapeIndexSnapshotObjectRow {
                    tape_file_number: number,
                    stored_block_count: 1,
                    object_id: b"synthetic".to_vec(),
                    representation: BootstrapObjectRepresentation::Plaintext {
                        manifest_first_chunk_lba: 0,
                        manifest_size_bytes: 1,
                        manifest_chunk_count: 1,
                        manifest_sha256: [0x11; 32],
                    },
                })?;
            }
            Ok(())
        }
    }

    #[test]
    fn synthetic_streaming_plan_keeps_source_constant_sized() {
        const COUNT: u64 = 10_000;
        assert_eq!(std::mem::size_of::<SyntheticSource>(), 8);
        let descriptor = TapeIndexSnapshotDescriptor {
            tape_uuid: TAPE_UUID,
            sequence: 1,
            block_size: 256 * 1024,
            scope: TapeIndexSnapshotScope {
                covered_prefix_tape_file_count: COUNT,
                total_data_ordinals: COUNT,
                highest_protected_ordinal: 0,
            },
            counts: TapeIndexSnapshotCounts {
                structural_entry_count: COUNT,
                object_row_count: COUNT,
            },
            is_final: false,
            writer_version: "stream-test".to_string(),
            write_timestamp: "2026-08-09T00:00:00Z".to_string(),
        };
        let plan = plan_tape_index_snapshot(descriptor, &mut SyntheticSource { count: COUNT })
            .expect("generated rows stream without collection");
        assert_eq!(
            plan.layout.payload_len,
            COUNT * (TAPE_INDEX_STRUCTURAL_SLOT_LEN + TAPE_INDEX_OBJECT_ROW_SLOT_LEN)
        );
    }
}
