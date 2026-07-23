//! Sidecar epoch directory and `parity_map` tape-file codec.
//!
//! The Layer 3c v0.4.4 implementation addendum v0.2 makes the sidecar
//! directory a root-of-trust input for catalog-less reconstruction. This
//! module owns that compact directory model plus the replicated
//! `parity_map` control tape-file format: primary header/payload copy, tail
//! copy, and footer locator.

use ciborium::value::Value as CborValue;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::error::ParityError;
use crate::sidecar::crc64_xz;

type HmacSha256 = Hmac<Sha256>;

/// Canonical format identifier stored in every parity-map payload.
pub const PARITY_MAP_FORMAT_ID: &str = "rem-parity-map-v1";

/// Parity-map header schema version emitted and accepted by this codec.
pub const PARITY_MAP_SCHEMA_VERSION: u16 = 1;

/// Parity-map footer schema version emitted and accepted by this codec.
pub const PARITY_MAP_FOOTER_VERSION: u16 = 1;

/// Byte length of the fixed parity-map header fields including CRC.
pub const PARITY_MAP_HEADER_LEN: usize = 0xB8;

/// Byte offset of the header CRC-64/XZ field.
pub const PARITY_MAP_HEADER_CRC_OFFSET: usize = 0xB0;

/// Byte length of the fixed parity-map footer fields including CRC.
pub const PARITY_MAP_FOOTER_LEN: usize = 0xB8;

/// Byte offset of the footer CRC-64/XZ field.
pub const PARITY_MAP_FOOTER_CRC_OFFSET: usize = 0xB0;

/// Directory flag: this sidecar protects a final partial epoch.
pub const SIDECAR_DIRECTORY_FLAG_FINAL_PARTIAL_EPOCH: u32 = 0x01;

/// Directory flag: the primary sidecar metadata copy was known good when written.
pub const SIDECAR_DIRECTORY_FLAG_PRIMARY_KNOWN_GOOD: u32 = 0x02;

/// Directory flag: the tail sidecar metadata copy was known good when written.
pub const SIDECAR_DIRECTORY_FLAG_TAIL_KNOWN_GOOD: u32 = 0x04;

const PARITY_MAP_MAGIC_MESSAGE: &[u8] = b"REM\x00PMAP\x01";
const KNOWN_DIRECTORY_FLAGS: u32 = SIDECAR_DIRECTORY_FLAG_FINAL_PARTIAL_EPOCH
    | SIDECAR_DIRECTORY_FLAG_PRIMARY_KNOWN_GOOD
    | SIDECAR_DIRECTORY_FLAG_TAIL_KNOWN_GOOD;

/// Structural sidecar-directory entry from addendum v0.2 §3.2.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidecarEpochDirectoryEntry {
    /// Filemark-delimited tape-file number of the parity sidecar.
    pub tape_file_number: u32,
    /// Parity epoch identifier recorded in the sidecar metadata.
    pub epoch_id: u64,
    /// First protected object-data ordinal.
    pub protected_ordinal_start: u64,
    /// End-exclusive protected object-data ordinal.
    pub protected_ordinal_end_exclusive: u64,
    /// Total sidecar tape-file blocks before the trailing filemark.
    pub sidecar_total_block_count: u64,
    /// Blocks in one sidecar header/index metadata copy.
    pub sidecar_header_block_count: u32,
    /// Raw parity-shard block count.
    pub parity_shard_block_count: u32,
    /// Canonical metadata hash shared by the primary/tail sidecar copies.
    pub canonical_metadata_hash: [u8; 32],
    /// Addendum-defined structural flags.
    pub flags: u32,
}

/// Compact structural list of parity sidecars for one map scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidecarEpochDirectory {
    /// Number of leading tape files described by this directory scope.
    pub directory_scope_tape_file_count: u32,
    /// Total object-data ordinals in the directory scope.
    pub directory_scope_total_data_ordinals: u64,
    /// Highest protected ordinal in the directory scope.
    pub directory_scope_highest_protected_ordinal: u64,
    /// True when this directory covers the final tape map.
    pub is_final_directory: bool,
    /// Structural parity-sidecar rows in ascending tape-file order.
    pub entries: Vec<SidecarEpochDirectoryEntry>,
}

impl SidecarEpochDirectory {
    /// Validate structural consistency that must hold before the directory is
    /// used as a root-of-trust input.
    pub fn validate(&self) -> Result<(), ParityError> {
        if self.directory_scope_highest_protected_ordinal > self.directory_scope_total_data_ordinals
        {
            return Err(directory_invalid(
                "directory highest protected ordinal exceeds total data ordinals",
            ));
        }

        let mut previous_tape_file_number = None;
        let mut expected_protected_ordinal_start = 0u64;
        for (expected_epoch_id, entry) in (0u64..).zip(&self.entries) {
            if entry.tape_file_number >= self.directory_scope_tape_file_count {
                return Err(directory_invalid(format!(
                    "directory sidecar tape file {} lies outside scope {}",
                    entry.tape_file_number, self.directory_scope_tape_file_count
                )));
            }
            if previous_tape_file_number.is_some_and(|previous| entry.tape_file_number <= previous)
            {
                return Err(directory_invalid(
                    "directory sidecar entries must be in ascending tape-file order",
                ));
            }
            if entry.protected_ordinal_end_exclusive <= entry.protected_ordinal_start {
                return Err(directory_invalid(format!(
                    "directory sidecar {} has an empty protected range",
                    entry.tape_file_number
                )));
            }
            if entry.protected_ordinal_start != expected_protected_ordinal_start {
                return Err(directory_invalid(format!(
                    "directory sidecar {} protected range starts at {}, expected contiguous start {expected_protected_ordinal_start}",
                    entry.tape_file_number, entry.protected_ordinal_start
                )));
            }
            if entry.epoch_id != expected_epoch_id {
                return Err(directory_invalid(format!(
                    "directory sidecar {} has epoch_id {}, expected {expected_epoch_id}",
                    entry.tape_file_number, entry.epoch_id
                )));
            }
            if entry.sidecar_total_block_count == 0
                || entry.sidecar_header_block_count == 0
                || entry.parity_shard_block_count == 0
            {
                return Err(directory_invalid(format!(
                    "directory sidecar {} has invalid block counts",
                    entry.tape_file_number
                )));
            }
            if entry.flags & !KNOWN_DIRECTORY_FLAGS != 0 {
                return Err(directory_invalid(format!(
                    "directory sidecar {} has unknown flags 0x{:08x}",
                    entry.tape_file_number,
                    entry.flags & !KNOWN_DIRECTORY_FLAGS
                )));
            }
            expected_protected_ordinal_start = entry.protected_ordinal_end_exclusive;
            previous_tape_file_number = Some(entry.tape_file_number);
        }

        if expected_protected_ordinal_start != self.directory_scope_highest_protected_ordinal {
            return Err(directory_invalid(format!(
                "directory highest protected ordinal {} does not match partition end {expected_protected_ordinal_start}",
                self.directory_scope_highest_protected_ordinal
            )));
        }

        Ok(())
    }

    /// Return the deterministic CBOR byte length for sizing inline bootstrap
    /// storage versus an external `parity_map` tape file.
    pub fn encoded_len(&self) -> Result<usize, ParityError> {
        let mut bytes = Vec::new();
        ciborium::into_writer(&encode_sidecar_epoch_directory_cbor(self)?, &mut bytes)
            .map_err(|err| parity_map_parse(format!("directory CBOR encode failed: {err}")))?;
        Ok(bytes.len())
    }
}

/// Bootstrap reference to an external `parity_map` tape file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParityMapReference {
    /// Filemark-delimited tape-file number of the referenced parity map.
    pub tape_file_number: u32,
    /// Fixed-block count in the parity-map tape file, excluding filemark.
    pub block_count: u64,
    /// Directory scope tape-file count.
    pub directory_scope_tape_file_count: u32,
    /// Directory scope total data ordinals.
    pub directory_scope_total_data_ordinals: u64,
    /// Directory scope highest protected ordinal.
    pub directory_scope_highest_protected_ordinal: u64,
    /// True when the referenced directory covers the final tape map.
    pub is_final_directory: bool,
    /// SHA-256 of the canonical parity-map payload bytes.
    pub parity_map_payload_sha256: [u8; 32],
    /// Canonical filemark-map digest for the referenced scope.
    pub canonical_map_digest: [u8; 32],
}

impl ParityMapReference {
    /// Validate structural fields before storing or trusting a bootstrap
    /// reference to an external parity-map control file.
    pub fn validate(&self) -> Result<(), ParityError> {
        if self.block_count == 0 {
            return Err(parity_map_parse(
                "parity-map reference block_count must be non-zero",
            ));
        }
        if self.directory_scope_tape_file_count == 0 {
            return Err(parity_map_parse(
                "parity-map reference scope must include at least one tape file",
            ));
        }
        if self.directory_scope_highest_protected_ordinal > self.directory_scope_total_data_ordinals
        {
            return Err(parity_map_parse(
                "parity-map reference highest protected ordinal exceeds total data ordinals",
            ));
        }
        Ok(())
    }
}

/// Canonical payload stored inside a `parity_map` tape file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParityMapPayload {
    /// Tape UUID this parity map belongs to.
    pub tape_uuid: [u8; 16],
    /// Writer-assigned parity-map sequence number.
    pub sequence: u32,
    /// Sidecar directory carried by this parity map.
    pub directory: SidecarEpochDirectory,
    /// Canonical filemark-map digest for this directory scope.
    pub canonical_map_digest: [u8; 32],
    /// Optional writer version string.
    pub writer_version: Option<String>,
    /// Optional write timestamp string.
    pub write_timestamp: Option<String>,
}

/// Header/payload copy identity for replicated parity-map metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParityMapCopyKind {
    /// Header/payload copy at the beginning of the tape file.
    Primary,
    /// Header/payload copy after the primary copy.
    Tail,
}

impl ParityMapCopyKind {
    fn to_u16(self) -> u16 {
        match self {
            Self::Primary => 1,
            Self::Tail => 2,
        }
    }

    fn from_u16(value: u16) -> Result<Self, ParityError> {
        match value {
            1 => Ok(Self::Primary),
            2 => Ok(Self::Tail),
            _ => Err(parity_map_parse(format!(
                "unsupported parity-map copy kind: {value}"
            ))),
        }
    }
}

/// Decoded fixed header from a parity-map copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParityMapHeader {
    /// HMAC-derived per-tape parity-map magic.
    pub magic: [u8; 8],
    /// Header schema version.
    pub schema_version: u16,
    /// Primary or tail copy.
    pub copy_kind: ParityMapCopyKind,
    /// Tape UUID.
    pub tape_uuid: [u8; 16],
    /// Parity-map sequence number.
    pub sequence: u32,
    /// Fixed tape block size.
    pub block_size: u32,
    /// Canonical payload byte length.
    pub payload_len: u64,
    /// SHA-256 over canonical payload bytes.
    pub payload_sha256: [u8; 32],
    /// Canonical map digest for the directory scope.
    pub canonical_map_digest: [u8; 32],
    /// Directory scope tape-file count.
    pub directory_scope_tape_file_count: u32,
    /// Directory scope total data ordinals.
    pub directory_scope_total_data_ordinals: u64,
    /// Directory scope highest protected ordinal.
    pub directory_scope_highest_protected_ordinal: u64,
    /// True when this parity map covers the final tape map.
    pub is_final_directory: bool,
    /// Blocks in one header/payload copy.
    pub copy_block_count: u64,
    /// Total parity-map tape-file blocks before the trailing filemark.
    pub parity_map_total_block_count: u64,
    /// Primary copy start block.
    pub primary_copy_start_block: u64,
    /// Tail copy start block.
    pub tail_copy_start_block: u64,
    /// Footer locator block index.
    pub footer_block_index: u64,
    /// CRC-64/XZ over the fixed header fields before this field.
    pub header_crc64: u64,
}

/// Decoded footer locator from the final parity-map block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParityMapFooter {
    /// HMAC-derived per-tape parity-map magic.
    pub magic: [u8; 8],
    /// Footer schema version.
    pub footer_version: u16,
    /// Tape UUID.
    pub tape_uuid: [u8; 16],
    /// Parity-map sequence number.
    pub sequence: u32,
    /// Fixed tape block size.
    pub block_size: u32,
    /// Canonical payload byte length.
    pub payload_len: u64,
    /// SHA-256 over canonical payload bytes.
    pub payload_sha256: [u8; 32],
    /// Canonical map digest for the directory scope.
    pub canonical_map_digest: [u8; 32],
    /// Directory scope tape-file count.
    pub directory_scope_tape_file_count: u32,
    /// Directory scope total data ordinals.
    pub directory_scope_total_data_ordinals: u64,
    /// Directory scope highest protected ordinal.
    pub directory_scope_highest_protected_ordinal: u64,
    /// True when this parity map covers the final tape map.
    pub is_final_directory: bool,
    /// Blocks in one header/payload copy.
    pub copy_block_count: u64,
    /// Total parity-map tape-file blocks before the trailing filemark.
    pub parity_map_total_block_count: u64,
    /// Primary copy start block.
    pub primary_copy_start_block: u64,
    /// Tail copy start block.
    pub tail_copy_start_block: u64,
    /// Footer locator block index.
    pub footer_block_index: u64,
    /// CRC-64/XZ over the fixed footer fields before this field.
    pub footer_crc64: u64,
}

/// Encoded full parity-map tape file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedParityMapTapeFile {
    /// Header written into the primary copy.
    pub header: ParityMapHeader,
    /// Canonical payload bytes.
    pub payload_bytes: Vec<u8>,
    /// Complete filemark-delimited parity-map blocks.
    pub blocks: Vec<Vec<u8>>,
}

/// Decoded full parity-map tape file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedParityMapTapeFile {
    /// Header from the copy selected by the parser.
    pub header: ParityMapHeader,
    /// Validated canonical payload.
    pub payload: ParityMapPayload,
    /// Canonical payload bytes.
    pub payload_bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ParityMapLayout {
    block_size: u32,
    payload_len: u64,
    payload_sha256: [u8; 32],
    copy_block_count: u64,
    parity_map_total_block_count: u64,
    primary_copy_start_block: u64,
    tail_copy_start_block: u64,
    footer_block_index: u64,
}

/// Derive the 8-byte per-tape parity-map magic.
pub fn derive_parity_map_magic(tape_uuid: &[u8; 16]) -> [u8; 8] {
    let mut mac = HmacSha256::new_from_slice(tape_uuid).expect("HMAC accepts any key length");
    mac.update(PARITY_MAP_MAGIC_MESSAGE);
    let result = mac.finalize().into_bytes();
    let mut magic = [0u8; 8];
    magic.copy_from_slice(&result[..8]);
    magic
}

/// Encode a complete replicated `parity_map` tape file.
pub fn encode_parity_map_tape_file(
    payload: &ParityMapPayload,
    block_size: u32,
) -> Result<EncodedParityMapTapeFile, ParityError> {
    payload.directory.validate()?;
    if payload.directory.directory_scope_tape_file_count == 0 {
        return Err(parity_map_parse(
            "parity-map directory scope must include at least one tape file",
        ));
    }
    if payload.directory.directory_scope_total_data_ordinals
        < payload.directory.directory_scope_highest_protected_ordinal
    {
        return Err(parity_map_parse(
            "parity-map directory total ordinals precede protection watermark",
        ));
    }
    let block_size_usize = validate_block_size(block_size)?;
    let payload_bytes = encode_parity_map_payload(payload)?;
    let payload_sha256 = sha256_array(&payload_bytes);
    let copy_bytes_len = PARITY_MAP_HEADER_LEN
        .checked_add(payload_bytes.len())
        .ok_or_else(|| parity_map_parse("parity-map copy length overflows"))?;
    let copy_block_count_usize = copy_bytes_len.div_ceil(block_size_usize);
    let copy_block_count = u64::try_from(copy_block_count_usize)
        .map_err(|_| parity_map_parse("parity-map copy block count overflows u64"))?;
    let tail_copy_start_block = copy_block_count;
    let footer_block_index = copy_block_count
        .checked_mul(2)
        .ok_or_else(|| parity_map_parse("parity-map footer block index overflows"))?;
    let total_block_count = footer_block_index
        .checked_add(1)
        .ok_or_else(|| parity_map_parse("parity-map total block count overflows"))?;

    let layout = ParityMapLayout {
        block_size,
        payload_len: payload_bytes.len() as u64,
        payload_sha256,
        copy_block_count,
        parity_map_total_block_count: total_block_count,
        primary_copy_start_block: 0,
        tail_copy_start_block,
        footer_block_index,
    };

    let primary_header = build_header(payload, ParityMapCopyKind::Primary, layout)?;
    let tail_header = build_header(payload, ParityMapCopyKind::Tail, layout)?;

    let mut blocks = Vec::with_capacity(
        usize::try_from(total_block_count)
            .map_err(|_| parity_map_parse("parity-map total block count overflows usize"))?,
    );
    blocks.extend(pack_copy_blocks(&primary_header, &payload_bytes)?);
    blocks.extend(pack_copy_blocks(&tail_header, &payload_bytes)?);
    blocks.push(encode_footer_block(payload, layout)?);

    Ok(EncodedParityMapTapeFile {
        header: primary_header,
        payload_bytes,
        blocks,
    })
}

/// Parse and validate a parity-map header block.
pub fn parse_parity_map_header_block(
    block0: &[u8],
    expected_tape_uuid: &[u8; 16],
) -> Result<ParityMapHeader, ParityError> {
    if block0.len() < PARITY_MAP_HEADER_LEN {
        return Err(parity_map_parse(format!(
            "parity-map header block too short: got {}, need {PARITY_MAP_HEADER_LEN}",
            block0.len()
        )));
    }
    let mut magic = [0u8; 8];
    magic.copy_from_slice(&block0[0x00..0x08]);
    let expected_magic = derive_parity_map_magic(expected_tape_uuid);
    if magic != expected_magic {
        return Err(parity_map_parse("parity-map magic mismatch"));
    }

    let schema_version = read_u16_le(block0, 0x08);
    if schema_version != PARITY_MAP_SCHEMA_VERSION {
        return Err(parity_map_parse(format!(
            "unsupported parity-map schema version: got {schema_version}, accept {PARITY_MAP_SCHEMA_VERSION}"
        )));
    }
    let copy_kind = ParityMapCopyKind::from_u16(read_u16_le(block0, 0x0A))?;
    if read_u32_le(block0, 0x0C) != 0 {
        return Err(parity_map_parse(
            "parity-map header reserved field is non-zero",
        ));
    }

    let mut tape_uuid = [0u8; 16];
    tape_uuid.copy_from_slice(&block0[0x10..0x20]);
    if &tape_uuid != expected_tape_uuid {
        return Err(parity_map_parse("parity-map tape UUID mismatch"));
    }
    let sequence = read_u32_le(block0, 0x20);
    let block_size = read_u32_le(block0, 0x24);
    if usize::try_from(block_size).ok() != Some(block0.len()) {
        return Err(parity_map_parse(format!(
            "parity-map block_size {block_size} does not match block length {}",
            block0.len()
        )));
    }
    validate_block_size(block_size)?;
    let payload_len = read_u64_le(block0, 0x28);
    let mut payload_sha256 = [0u8; 32];
    payload_sha256.copy_from_slice(&block0[0x30..0x50]);
    let mut canonical_map_digest = [0u8; 32];
    canonical_map_digest.copy_from_slice(&block0[0x50..0x70]);
    let directory_scope_tape_file_count = read_u32_le(block0, 0x70);
    let directory_scope_total_data_ordinals = read_u64_le(block0, 0x74);
    let directory_scope_highest_protected_ordinal = read_u64_le(block0, 0x7C);
    let is_final_directory = match block0[0x84] {
        0 => false,
        1 => true,
        value => {
            return Err(parity_map_parse(format!(
                "parity-map is_final_directory byte is {value}, expected 0 or 1"
            )))
        }
    };
    if block0[0x85..0x88].iter().any(|byte| *byte != 0) {
        return Err(parity_map_parse(
            "parity-map header bool padding is non-zero",
        ));
    }
    let copy_block_count = read_u64_le(block0, 0x88);
    let parity_map_total_block_count = read_u64_le(block0, 0x90);
    let primary_copy_start_block = read_u64_le(block0, 0x98);
    let tail_copy_start_block = read_u64_le(block0, 0xA0);
    let footer_block_index = read_u64_le(block0, 0xA8);
    let header_crc64 = read_u64_le(block0, PARITY_MAP_HEADER_CRC_OFFSET);
    let computed = crc64_xz(&block0[..PARITY_MAP_HEADER_CRC_OFFSET]);
    if header_crc64 != computed {
        return Err(parity_map_parse(format!(
            "parity-map header CRC mismatch: stored 0x{header_crc64:016x}, computed 0x{computed:016x}"
        )));
    }
    validate_locator_counts(
        payload_len,
        block_size,
        copy_block_count,
        parity_map_total_block_count,
        primary_copy_start_block,
        tail_copy_start_block,
        footer_block_index,
    )?;

    Ok(ParityMapHeader {
        magic,
        schema_version,
        copy_kind,
        tape_uuid,
        sequence,
        block_size,
        payload_len,
        payload_sha256,
        canonical_map_digest,
        directory_scope_tape_file_count,
        directory_scope_total_data_ordinals,
        directory_scope_highest_protected_ordinal,
        is_final_directory,
        copy_block_count,
        parity_map_total_block_count,
        primary_copy_start_block,
        tail_copy_start_block,
        footer_block_index,
        header_crc64,
    })
}

/// Classify a candidate block as a parity-map header for this tape.
pub fn classify_parity_map_header_block(
    block0: &[u8],
    expected_tape_uuid: &[u8; 16],
) -> Result<Option<ParityMapHeader>, ParityError> {
    let expected_magic = derive_parity_map_magic(expected_tape_uuid);
    if block0.len() < expected_magic.len() || block0[0..8] != expected_magic {
        return Ok(None);
    }
    parse_parity_map_header_block(block0, expected_tape_uuid).map(Some)
}

/// Parse and validate a parity-map footer locator block.
pub fn parse_parity_map_footer_block(
    footer_block: &[u8],
    expected_tape_uuid: &[u8; 16],
) -> Result<ParityMapFooter, ParityError> {
    if footer_block.len() < PARITY_MAP_FOOTER_LEN {
        return Err(parity_map_parse(format!(
            "parity-map footer block too short: got {}, need {PARITY_MAP_FOOTER_LEN}",
            footer_block.len()
        )));
    }
    let mut magic = [0u8; 8];
    magic.copy_from_slice(&footer_block[0x00..0x08]);
    let expected_magic = derive_parity_map_magic(expected_tape_uuid);
    if magic != expected_magic {
        return Err(parity_map_parse("parity-map footer magic mismatch"));
    }
    let footer_version = read_u16_le(footer_block, 0x08);
    if footer_version != PARITY_MAP_FOOTER_VERSION {
        return Err(parity_map_parse(format!(
            "unsupported parity-map footer version: got {footer_version}, accept {PARITY_MAP_FOOTER_VERSION}"
        )));
    }
    if read_u16_le(footer_block, 0x0A) != 0 || read_u32_le(footer_block, 0x0C) != 0 {
        return Err(parity_map_parse(
            "parity-map footer reserved fields are non-zero",
        ));
    }
    let mut tape_uuid = [0u8; 16];
    tape_uuid.copy_from_slice(&footer_block[0x10..0x20]);
    if &tape_uuid != expected_tape_uuid {
        return Err(parity_map_parse("parity-map footer tape UUID mismatch"));
    }
    let sequence = read_u32_le(footer_block, 0x20);
    let block_size = read_u32_le(footer_block, 0x24);
    if usize::try_from(block_size).ok() != Some(footer_block.len()) {
        return Err(parity_map_parse(format!(
            "parity-map footer block_size {block_size} does not match block length {}",
            footer_block.len()
        )));
    }
    validate_block_size(block_size)?;
    let payload_len = read_u64_le(footer_block, 0x28);
    let mut payload_sha256 = [0u8; 32];
    payload_sha256.copy_from_slice(&footer_block[0x30..0x50]);
    let mut canonical_map_digest = [0u8; 32];
    canonical_map_digest.copy_from_slice(&footer_block[0x50..0x70]);
    let directory_scope_tape_file_count = read_u32_le(footer_block, 0x70);
    let directory_scope_total_data_ordinals = read_u64_le(footer_block, 0x74);
    let directory_scope_highest_protected_ordinal = read_u64_le(footer_block, 0x7C);
    let is_final_directory = match footer_block[0x84] {
        0 => false,
        1 => true,
        value => {
            return Err(parity_map_parse(format!(
                "parity-map footer is_final_directory byte is {value}, expected 0 or 1"
            )))
        }
    };
    if footer_block[0x85..0x88].iter().any(|byte| *byte != 0) {
        return Err(parity_map_parse(
            "parity-map footer bool padding is non-zero",
        ));
    }
    let copy_block_count = read_u64_le(footer_block, 0x88);
    let parity_map_total_block_count = read_u64_le(footer_block, 0x90);
    let primary_copy_start_block = read_u64_le(footer_block, 0x98);
    let tail_copy_start_block = read_u64_le(footer_block, 0xA0);
    let footer_block_index = read_u64_le(footer_block, 0xA8);
    let footer_crc64 = read_u64_le(footer_block, PARITY_MAP_FOOTER_CRC_OFFSET);
    let computed = crc64_xz(&footer_block[..PARITY_MAP_FOOTER_CRC_OFFSET]);
    if footer_crc64 != computed {
        return Err(parity_map_parse(format!(
            "parity-map footer CRC mismatch: stored 0x{footer_crc64:016x}, computed 0x{computed:016x}"
        )));
    }
    ensure_zero_filled(
        &footer_block[PARITY_MAP_FOOTER_LEN..],
        "parity-map footer padding",
    )?;
    validate_locator_counts(
        payload_len,
        block_size,
        copy_block_count,
        parity_map_total_block_count,
        primary_copy_start_block,
        tail_copy_start_block,
        footer_block_index,
    )?;

    Ok(ParityMapFooter {
        magic,
        footer_version,
        tape_uuid,
        sequence,
        block_size,
        payload_len,
        payload_sha256,
        canonical_map_digest,
        directory_scope_tape_file_count,
        directory_scope_total_data_ordinals,
        directory_scope_highest_protected_ordinal,
        is_final_directory,
        copy_block_count,
        parity_map_total_block_count,
        primary_copy_start_block,
        tail_copy_start_block,
        footer_block_index,
        footer_crc64,
    })
}

/// Parse and validate a complete replicated parity-map tape file.
pub fn parse_parity_map_tape_file(
    blocks: &[Vec<u8>],
    expected_tape_uuid: &[u8; 16],
) -> Result<DecodedParityMapTapeFile, ParityError> {
    let footer_block = blocks
        .last()
        .ok_or_else(|| parity_map_parse("parity-map tape file is empty"))?;
    let footer = parse_parity_map_footer_block(footer_block, expected_tape_uuid)?;
    let expected_blocks = usize::try_from(footer.parity_map_total_block_count)
        .map_err(|_| parity_map_parse("parity-map block count overflows usize"))?;
    if blocks.len() != expected_blocks {
        return Err(parity_map_parse(format!(
            "parity-map has {} blocks, footer expects {expected_blocks}",
            blocks.len()
        )));
    }

    let primary = parse_copy_at(blocks, &footer, ParityMapCopyKind::Primary);
    let tail = parse_copy_at(blocks, &footer, ParityMapCopyKind::Tail);
    select_parity_map_copy(primary, tail)
}

/// Parse a replicated parity-map tape file whose individual blocks may be
/// unreadable.
///
/// The measured tape-file length locates both header/payload copies without
/// relying on the footer. A readable footer remains authoritative redundancy:
/// it must parse and agree with any accepted header. Payload blocks are never
/// spliced between copies; each copy is hash-checked independently.
pub fn parse_parity_map_tape_file_with_unreadable_blocks(
    blocks: &[Option<Vec<u8>>],
    expected_tape_uuid: &[u8; 16],
) -> Result<DecodedParityMapTapeFile, ParityError> {
    let measured_total = u64::try_from(blocks.len())
        .map_err(|_| parity_map_parse("parity-map measured block count overflows u64"))?;
    if measured_total < 3 || measured_total % 2 == 0 {
        return Err(parity_map_parse(format!(
            "parity-map measured block count {measured_total} cannot have layout 2M+1"
        )));
    }
    let measured_copy_block_count = (measured_total - 1) / 2;
    let footer = match blocks.last().and_then(Option::as_ref) {
        Some(footer_block) => {
            let footer = parse_parity_map_footer_block(footer_block, expected_tape_uuid)?;
            if footer.parity_map_total_block_count != measured_total {
                return Err(parity_map_parse(format!(
                    "parity-map has {measured_total} blocks, footer expects {}",
                    footer.parity_map_total_block_count
                )));
            }
            Some(footer)
        }
        None => None,
    };

    let primary = parse_available_copy_at(
        blocks,
        0,
        ParityMapCopyKind::Primary,
        expected_tape_uuid,
        measured_total,
        footer.as_ref(),
    );
    let tail_start = usize::try_from(measured_copy_block_count)
        .map_err(|_| parity_map_parse("parity-map tail copy start overflows usize"))?;
    let tail = parse_available_copy_at(
        blocks,
        tail_start,
        ParityMapCopyKind::Tail,
        expected_tape_uuid,
        measured_total,
        footer.as_ref(),
    );
    select_parity_map_copy(primary, tail)
}

fn select_parity_map_copy(
    primary: Result<DecodedParityMapTapeFile, ParityError>,
    tail: Result<DecodedParityMapTapeFile, ParityError>,
) -> Result<DecodedParityMapTapeFile, ParityError> {
    match (primary, tail) {
        (Ok(primary), Ok(tail)) => {
            if !parity_map_copies_agree(&primary, &tail) {
                return Err(parity_map_parse(
                    "parity-map primary and tail copies disagree",
                ));
            }
            Ok(primary)
        }
        (Ok(primary), Err(_)) => Ok(primary),
        (Err(_), Ok(tail)) => Ok(tail),
        (Err(primary_err), Err(tail_err)) => Err(parity_map_parse(format!(
            "both parity-map metadata copies failed: primary={primary_err}; tail={tail_err}"
        ))),
    }
}

pub(crate) fn encode_sidecar_epoch_directory_cbor(
    directory: &SidecarEpochDirectory,
) -> Result<CborValue, ParityError> {
    directory.validate()?;
    Ok(CborValue::Map(vec![
        (
            CborValue::Integer(1.into()),
            CborValue::Integer(directory.directory_scope_tape_file_count.into()),
        ),
        (
            CborValue::Integer(2.into()),
            CborValue::Integer(directory.directory_scope_total_data_ordinals.into()),
        ),
        (
            CborValue::Integer(3.into()),
            CborValue::Integer(directory.directory_scope_highest_protected_ordinal.into()),
        ),
        (
            CborValue::Integer(4.into()),
            CborValue::Bool(directory.is_final_directory),
        ),
        (
            CborValue::Integer(5.into()),
            CborValue::Array(
                directory
                    .entries
                    .iter()
                    .map(encode_sidecar_epoch_directory_entry_cbor)
                    .collect(),
            ),
        ),
    ]))
}

pub(crate) fn decode_sidecar_epoch_directory_cbor(
    value: CborValue,
) -> Result<SidecarEpochDirectory, ParityError> {
    let map = match value {
        CborValue::Map(map) => map,
        _ => return Err(parity_map_parse("sidecar epoch directory is not a map")),
    };
    let mut directory_scope_tape_file_count = None;
    let mut directory_scope_total_data_ordinals = None;
    let mut directory_scope_highest_protected_ordinal = None;
    let mut is_final_directory = None;
    let mut entries = None;
    for (key, value) in map {
        let key_i: i128 = match key {
            CborValue::Integer(i) => i.into(),
            _ => continue,
        };
        match (key_i, value) {
            (1, CborValue::Integer(i)) => {
                directory_scope_tape_file_count =
                    Some(cbor_int_to_u32(i, "directory_scope_tape_file_count")?)
            }
            (2, CborValue::Integer(i)) => {
                directory_scope_total_data_ordinals =
                    Some(cbor_int_to_u64(i, "directory_scope_total_data_ordinals")?)
            }
            (3, CborValue::Integer(i)) => {
                directory_scope_highest_protected_ordinal = Some(cbor_int_to_u64(
                    i,
                    "directory_scope_highest_protected_ordinal",
                )?)
            }
            (4, CborValue::Bool(value)) => is_final_directory = Some(value),
            (5, CborValue::Array(values)) => {
                let decoded = values
                    .into_iter()
                    .map(decode_sidecar_epoch_directory_entry_cbor)
                    .collect::<Result<Vec<_>, _>>()?;
                entries = Some(decoded);
            }
            _ => {}
        }
    }

    let directory = SidecarEpochDirectory {
        directory_scope_tape_file_count: directory_scope_tape_file_count.ok_or_else(|| {
            parity_map_parse("sidecar epoch directory missing scope tape-file count")
        })?,
        directory_scope_total_data_ordinals: directory_scope_total_data_ordinals.ok_or_else(
            || parity_map_parse("sidecar epoch directory missing total data ordinals"),
        )?,
        directory_scope_highest_protected_ordinal: directory_scope_highest_protected_ordinal
            .ok_or_else(|| {
                parity_map_parse("sidecar epoch directory missing highest protected ordinal")
            })?,
        is_final_directory: is_final_directory
            .ok_or_else(|| parity_map_parse("sidecar epoch directory missing final flag"))?,
        entries: entries
            .ok_or_else(|| parity_map_parse("sidecar epoch directory missing entries"))?,
    };
    directory.validate()?;
    Ok(directory)
}

pub(crate) fn encode_parity_map_reference_cbor(reference: &ParityMapReference) -> CborValue {
    CborValue::Map(vec![
        (
            CborValue::Integer(1.into()),
            CborValue::Integer(reference.tape_file_number.into()),
        ),
        (
            CborValue::Integer(2.into()),
            CborValue::Integer(reference.block_count.into()),
        ),
        (
            CborValue::Integer(3.into()),
            CborValue::Integer(reference.directory_scope_tape_file_count.into()),
        ),
        (
            CborValue::Integer(4.into()),
            CborValue::Integer(reference.directory_scope_total_data_ordinals.into()),
        ),
        (
            CborValue::Integer(5.into()),
            CborValue::Integer(reference.directory_scope_highest_protected_ordinal.into()),
        ),
        (
            CborValue::Integer(6.into()),
            CborValue::Bool(reference.is_final_directory),
        ),
        (
            CborValue::Integer(7.into()),
            CborValue::Bytes(reference.parity_map_payload_sha256.to_vec()),
        ),
        (
            CborValue::Integer(8.into()),
            CborValue::Bytes(reference.canonical_map_digest.to_vec()),
        ),
    ])
}

pub(crate) fn decode_parity_map_reference_cbor(
    value: CborValue,
) -> Result<ParityMapReference, ParityError> {
    let map = match value {
        CborValue::Map(map) => map,
        _ => return Err(parity_map_parse("parity-map reference is not a map")),
    };
    let mut tape_file_number = None;
    let mut block_count = None;
    let mut directory_scope_tape_file_count = None;
    let mut directory_scope_total_data_ordinals = None;
    let mut directory_scope_highest_protected_ordinal = None;
    let mut is_final_directory = None;
    let mut parity_map_payload_sha256 = None;
    let mut canonical_map_digest = None;

    for (key, value) in map {
        let key_i: i128 = match key {
            CborValue::Integer(i) => i.into(),
            _ => continue,
        };
        match (key_i, value) {
            (1, CborValue::Integer(i)) => {
                tape_file_number = Some(cbor_int_to_u32(i, "tape_file_number")?)
            }
            (2, CborValue::Integer(i)) => block_count = Some(cbor_int_to_u64(i, "block_count")?),
            (3, CborValue::Integer(i)) => {
                directory_scope_tape_file_count =
                    Some(cbor_int_to_u32(i, "directory_scope_tape_file_count")?)
            }
            (4, CborValue::Integer(i)) => {
                directory_scope_total_data_ordinals =
                    Some(cbor_int_to_u64(i, "directory_scope_total_data_ordinals")?)
            }
            (5, CborValue::Integer(i)) => {
                directory_scope_highest_protected_ordinal = Some(cbor_int_to_u64(
                    i,
                    "directory_scope_highest_protected_ordinal",
                )?)
            }
            (6, CborValue::Bool(value)) => is_final_directory = Some(value),
            (7, CborValue::Bytes(bytes)) => {
                parity_map_payload_sha256 = Some(bytes_to_32(bytes, "parity_map_payload_sha256")?)
            }
            (8, CborValue::Bytes(bytes)) => {
                canonical_map_digest = Some(bytes_to_32(bytes, "canonical_map_digest")?)
            }
            _ => {}
        }
    }

    let reference = ParityMapReference {
        tape_file_number: tape_file_number
            .ok_or_else(|| parity_map_parse("parity-map reference missing tape_file_number"))?,
        block_count: block_count
            .ok_or_else(|| parity_map_parse("parity-map reference missing block_count"))?,
        directory_scope_tape_file_count: directory_scope_tape_file_count.ok_or_else(|| {
            parity_map_parse("parity-map reference missing directory scope tape-file count")
        })?,
        directory_scope_total_data_ordinals: directory_scope_total_data_ordinals.ok_or_else(
            || parity_map_parse("parity-map reference missing directory total data ordinals"),
        )?,
        directory_scope_highest_protected_ordinal: directory_scope_highest_protected_ordinal
            .ok_or_else(|| {
                parity_map_parse("parity-map reference missing highest protected ordinal")
            })?,
        is_final_directory: is_final_directory
            .ok_or_else(|| parity_map_parse("parity-map reference missing final flag"))?,
        parity_map_payload_sha256: parity_map_payload_sha256
            .ok_or_else(|| parity_map_parse("parity-map reference missing payload sha256"))?,
        canonical_map_digest: canonical_map_digest
            .ok_or_else(|| parity_map_parse("parity-map reference missing map digest"))?,
    };
    reference.validate()?;
    Ok(reference)
}

fn encode_sidecar_epoch_directory_entry_cbor(entry: &SidecarEpochDirectoryEntry) -> CborValue {
    CborValue::Map(vec![
        (
            CborValue::Integer(1.into()),
            CborValue::Integer(entry.tape_file_number.into()),
        ),
        (
            CborValue::Integer(2.into()),
            CborValue::Integer(entry.epoch_id.into()),
        ),
        (
            CborValue::Integer(3.into()),
            CborValue::Integer(entry.protected_ordinal_start.into()),
        ),
        (
            CborValue::Integer(4.into()),
            CborValue::Integer(entry.protected_ordinal_end_exclusive.into()),
        ),
        (
            CborValue::Integer(5.into()),
            CborValue::Integer(entry.sidecar_total_block_count.into()),
        ),
        (
            CborValue::Integer(6.into()),
            CborValue::Integer(entry.sidecar_header_block_count.into()),
        ),
        (
            CborValue::Integer(7.into()),
            CborValue::Integer(entry.parity_shard_block_count.into()),
        ),
        (
            CborValue::Integer(8.into()),
            CborValue::Bytes(entry.canonical_metadata_hash.to_vec()),
        ),
        (
            CborValue::Integer(9.into()),
            CborValue::Integer(entry.flags.into()),
        ),
    ])
}

fn decode_sidecar_epoch_directory_entry_cbor(
    value: CborValue,
) -> Result<SidecarEpochDirectoryEntry, ParityError> {
    let map = match value {
        CborValue::Map(map) => map,
        _ => return Err(parity_map_parse("sidecar directory entry is not a map")),
    };
    let mut tape_file_number = None;
    let mut epoch_id = None;
    let mut protected_ordinal_start = None;
    let mut protected_ordinal_end_exclusive = None;
    let mut sidecar_total_block_count = None;
    let mut sidecar_header_block_count = None;
    let mut parity_shard_block_count = None;
    let mut canonical_metadata_hash = None;
    let mut flags = None;
    for (key, value) in map {
        let key_i: i128 = match key {
            CborValue::Integer(i) => i.into(),
            _ => continue,
        };
        match (key_i, value) {
            (1, CborValue::Integer(i)) => {
                tape_file_number = Some(cbor_int_to_u32(i, "tape_file_number")?)
            }
            (2, CborValue::Integer(i)) => epoch_id = Some(cbor_int_to_u64(i, "epoch_id")?),
            (3, CborValue::Integer(i)) => {
                protected_ordinal_start = Some(cbor_int_to_u64(i, "protected_ordinal_start")?)
            }
            (4, CborValue::Integer(i)) => {
                protected_ordinal_end_exclusive =
                    Some(cbor_int_to_u64(i, "protected_ordinal_end_exclusive")?)
            }
            (5, CborValue::Integer(i)) => {
                sidecar_total_block_count = Some(cbor_int_to_u64(i, "sidecar_total_block_count")?)
            }
            (6, CborValue::Integer(i)) => {
                sidecar_header_block_count = Some(cbor_int_to_u32(i, "sidecar_header_block_count")?)
            }
            (7, CborValue::Integer(i)) => {
                parity_shard_block_count = Some(cbor_int_to_u32(i, "parity_shard_block_count")?)
            }
            (8, CborValue::Bytes(bytes)) => {
                canonical_metadata_hash = Some(bytes_to_32(bytes, "canonical_metadata_hash")?)
            }
            (9, CborValue::Integer(i)) => flags = Some(cbor_int_to_u32(i, "flags")?),
            _ => {}
        }
    }

    Ok(SidecarEpochDirectoryEntry {
        tape_file_number: tape_file_number
            .ok_or_else(|| parity_map_parse("directory entry missing tape_file_number"))?,
        epoch_id: epoch_id.ok_or_else(|| parity_map_parse("directory entry missing epoch_id"))?,
        protected_ordinal_start: protected_ordinal_start
            .ok_or_else(|| parity_map_parse("directory entry missing protected_ordinal_start"))?,
        protected_ordinal_end_exclusive: protected_ordinal_end_exclusive.ok_or_else(|| {
            parity_map_parse("directory entry missing protected_ordinal_end_exclusive")
        })?,
        sidecar_total_block_count: sidecar_total_block_count
            .ok_or_else(|| parity_map_parse("directory entry missing sidecar_total_block_count"))?,
        sidecar_header_block_count: sidecar_header_block_count.ok_or_else(|| {
            parity_map_parse("directory entry missing sidecar_header_block_count")
        })?,
        parity_shard_block_count: parity_shard_block_count
            .ok_or_else(|| parity_map_parse("directory entry missing parity_shard_block_count"))?,
        canonical_metadata_hash: canonical_metadata_hash
            .ok_or_else(|| parity_map_parse("directory entry missing canonical_metadata_hash"))?,
        flags: flags.ok_or_else(|| parity_map_parse("directory entry missing flags"))?,
    })
}

fn encode_parity_map_payload(payload: &ParityMapPayload) -> Result<Vec<u8>, ParityError> {
    let mut entries = vec![
        (
            CborValue::Integer(1.into()),
            CborValue::Text(PARITY_MAP_FORMAT_ID.to_string()),
        ),
        (
            CborValue::Integer(2.into()),
            CborValue::Bytes(payload.tape_uuid.to_vec()),
        ),
        (
            CborValue::Integer(3.into()),
            CborValue::Integer(payload.sequence.into()),
        ),
        (
            CborValue::Integer(4.into()),
            encode_sidecar_epoch_directory_cbor(&payload.directory)?,
        ),
        (
            CborValue::Integer(5.into()),
            CborValue::Bytes(payload.canonical_map_digest.to_vec()),
        ),
    ];
    if let Some(version) = payload.writer_version.as_ref() {
        entries.push((
            CborValue::Integer(6.into()),
            CborValue::Text(version.clone()),
        ));
    }
    if let Some(timestamp) = payload.write_timestamp.as_ref() {
        entries.push((
            CborValue::Integer(7.into()),
            CborValue::Text(timestamp.clone()),
        ));
    }

    let mut bytes = Vec::new();
    ciborium::into_writer(&CborValue::Map(entries), &mut bytes)
        .map_err(|err| parity_map_parse(format!("parity-map payload CBOR encode failed: {err}")))?;
    Ok(bytes)
}

fn decode_parity_map_payload(bytes: &[u8]) -> Result<ParityMapPayload, ParityError> {
    let value: CborValue = ciborium::from_reader(bytes)
        .map_err(|err| parity_map_parse(format!("parity-map payload CBOR decode failed: {err}")))?;
    let map = match value {
        CborValue::Map(map) => map,
        _ => return Err(parity_map_parse("parity-map payload root is not a map")),
    };
    let mut format_id = None;
    let mut tape_uuid = None;
    let mut sequence = None;
    let mut directory = None;
    let mut canonical_map_digest = None;
    let mut writer_version = None;
    let mut write_timestamp = None;
    for (key, value) in map {
        let key_i: i128 = match key {
            CborValue::Integer(i) => i.into(),
            _ => continue,
        };
        match (key_i, value) {
            (1, CborValue::Text(value)) => format_id = Some(value),
            (2, CborValue::Bytes(bytes)) => tape_uuid = Some(bytes_to_16(bytes, "tape_uuid")?),
            (3, CborValue::Integer(i)) => sequence = Some(cbor_int_to_u32(i, "sequence")?),
            (4, value) => directory = Some(decode_sidecar_epoch_directory_cbor(value)?),
            (5, CborValue::Bytes(bytes)) => {
                canonical_map_digest = Some(bytes_to_32(bytes, "canonical_map_digest")?)
            }
            (6, CborValue::Text(value)) => writer_version = Some(value),
            (7, CborValue::Text(value)) => write_timestamp = Some(value),
            _ => {}
        }
    }

    let format_id =
        format_id.ok_or_else(|| parity_map_parse("parity-map payload missing format_id"))?;
    if format_id != PARITY_MAP_FORMAT_ID {
        return Err(parity_map_parse(format!(
            "unsupported parity-map payload format_id {format_id:?}"
        )));
    }

    Ok(ParityMapPayload {
        tape_uuid: tape_uuid
            .ok_or_else(|| parity_map_parse("parity-map payload missing tape_uuid"))?,
        sequence: sequence
            .ok_or_else(|| parity_map_parse("parity-map payload missing sequence"))?,
        directory: directory
            .ok_or_else(|| parity_map_parse("parity-map payload missing sidecar directory"))?,
        canonical_map_digest: canonical_map_digest
            .ok_or_else(|| parity_map_parse("parity-map payload missing map digest"))?,
        writer_version,
        write_timestamp,
    })
}

fn build_header(
    payload: &ParityMapPayload,
    copy_kind: ParityMapCopyKind,
    layout: ParityMapLayout,
) -> Result<ParityMapHeader, ParityError> {
    validate_locator_counts(
        layout.payload_len,
        layout.block_size,
        layout.copy_block_count,
        layout.parity_map_total_block_count,
        layout.primary_copy_start_block,
        layout.tail_copy_start_block,
        layout.footer_block_index,
    )?;
    Ok(ParityMapHeader {
        magic: derive_parity_map_magic(&payload.tape_uuid),
        schema_version: PARITY_MAP_SCHEMA_VERSION,
        copy_kind,
        tape_uuid: payload.tape_uuid,
        sequence: payload.sequence,
        block_size: layout.block_size,
        payload_len: layout.payload_len,
        payload_sha256: layout.payload_sha256,
        canonical_map_digest: payload.canonical_map_digest,
        directory_scope_tape_file_count: payload.directory.directory_scope_tape_file_count,
        directory_scope_total_data_ordinals: payload.directory.directory_scope_total_data_ordinals,
        directory_scope_highest_protected_ordinal: payload
            .directory
            .directory_scope_highest_protected_ordinal,
        is_final_directory: payload.directory.is_final_directory,
        copy_block_count: layout.copy_block_count,
        parity_map_total_block_count: layout.parity_map_total_block_count,
        primary_copy_start_block: layout.primary_copy_start_block,
        tail_copy_start_block: layout.tail_copy_start_block,
        footer_block_index: layout.footer_block_index,
        header_crc64: 0,
    })
}

fn pack_copy_blocks(
    header: &ParityMapHeader,
    payload_bytes: &[u8],
) -> Result<Vec<Vec<u8>>, ParityError> {
    let block_size = validate_block_size(header.block_size)?;
    let block_count = usize::try_from(header.copy_block_count)
        .map_err(|_| parity_map_parse("copy block count overflows usize"))?;
    let mut blocks = vec![vec![0u8; block_size]; block_count];
    write_header_fields(header, &mut blocks[0])?;
    write_bytes_at(&mut blocks, PARITY_MAP_HEADER_LEN, payload_bytes)?;
    Ok(blocks)
}

fn encode_footer_block(
    payload: &ParityMapPayload,
    layout: ParityMapLayout,
) -> Result<Vec<u8>, ParityError> {
    validate_locator_counts(
        layout.payload_len,
        layout.block_size,
        layout.copy_block_count,
        layout.parity_map_total_block_count,
        layout.primary_copy_start_block,
        layout.tail_copy_start_block,
        layout.footer_block_index,
    )?;
    let block_size_usize = validate_block_size(layout.block_size)?;
    let mut block = vec![0u8; block_size_usize];
    block[0x00..0x08].copy_from_slice(&derive_parity_map_magic(&payload.tape_uuid));
    block[0x08..0x0A].copy_from_slice(&PARITY_MAP_FOOTER_VERSION.to_le_bytes());
    block[0x0A..0x0C].copy_from_slice(&0u16.to_le_bytes());
    block[0x0C..0x10].copy_from_slice(&0u32.to_le_bytes());
    block[0x10..0x20].copy_from_slice(&payload.tape_uuid);
    block[0x20..0x24].copy_from_slice(&payload.sequence.to_le_bytes());
    block[0x24..0x28].copy_from_slice(&layout.block_size.to_le_bytes());
    block[0x28..0x30].copy_from_slice(&layout.payload_len.to_le_bytes());
    block[0x30..0x50].copy_from_slice(&layout.payload_sha256);
    block[0x50..0x70].copy_from_slice(&payload.canonical_map_digest);
    block[0x70..0x74].copy_from_slice(
        &payload
            .directory
            .directory_scope_tape_file_count
            .to_le_bytes(),
    );
    block[0x74..0x7C].copy_from_slice(
        &payload
            .directory
            .directory_scope_total_data_ordinals
            .to_le_bytes(),
    );
    block[0x7C..0x84].copy_from_slice(
        &payload
            .directory
            .directory_scope_highest_protected_ordinal
            .to_le_bytes(),
    );
    block[0x84] = u8::from(payload.directory.is_final_directory);
    block[0x88..0x90].copy_from_slice(&layout.copy_block_count.to_le_bytes());
    block[0x90..0x98].copy_from_slice(&layout.parity_map_total_block_count.to_le_bytes());
    block[0x98..0xA0].copy_from_slice(&layout.primary_copy_start_block.to_le_bytes());
    block[0xA0..0xA8].copy_from_slice(&layout.tail_copy_start_block.to_le_bytes());
    block[0xA8..0xB0].copy_from_slice(&layout.footer_block_index.to_le_bytes());
    let crc = crc64_xz(&block[..PARITY_MAP_FOOTER_CRC_OFFSET]);
    block[PARITY_MAP_FOOTER_CRC_OFFSET..PARITY_MAP_FOOTER_CRC_OFFSET + 8]
        .copy_from_slice(&crc.to_le_bytes());
    Ok(block)
}

fn write_header_fields(header: &ParityMapHeader, block: &mut [u8]) -> Result<(), ParityError> {
    if block.len() != usize::try_from(header.block_size).unwrap_or(0) {
        return Err(parity_map_parse(
            "parity-map header output block has wrong length",
        ));
    }
    block[0x00..0x08].copy_from_slice(&header.magic);
    block[0x08..0x0A].copy_from_slice(&header.schema_version.to_le_bytes());
    block[0x0A..0x0C].copy_from_slice(&header.copy_kind.to_u16().to_le_bytes());
    block[0x0C..0x10].copy_from_slice(&0u32.to_le_bytes());
    block[0x10..0x20].copy_from_slice(&header.tape_uuid);
    block[0x20..0x24].copy_from_slice(&header.sequence.to_le_bytes());
    block[0x24..0x28].copy_from_slice(&header.block_size.to_le_bytes());
    block[0x28..0x30].copy_from_slice(&header.payload_len.to_le_bytes());
    block[0x30..0x50].copy_from_slice(&header.payload_sha256);
    block[0x50..0x70].copy_from_slice(&header.canonical_map_digest);
    block[0x70..0x74].copy_from_slice(&header.directory_scope_tape_file_count.to_le_bytes());
    block[0x74..0x7C].copy_from_slice(&header.directory_scope_total_data_ordinals.to_le_bytes());
    block[0x7C..0x84].copy_from_slice(
        &header
            .directory_scope_highest_protected_ordinal
            .to_le_bytes(),
    );
    block[0x84] = u8::from(header.is_final_directory);
    block[0x88..0x90].copy_from_slice(&header.copy_block_count.to_le_bytes());
    block[0x90..0x98].copy_from_slice(&header.parity_map_total_block_count.to_le_bytes());
    block[0x98..0xA0].copy_from_slice(&header.primary_copy_start_block.to_le_bytes());
    block[0xA0..0xA8].copy_from_slice(&header.tail_copy_start_block.to_le_bytes());
    block[0xA8..0xB0].copy_from_slice(&header.footer_block_index.to_le_bytes());
    let crc = crc64_xz(&block[..PARITY_MAP_HEADER_CRC_OFFSET]);
    block[PARITY_MAP_HEADER_CRC_OFFSET..PARITY_MAP_HEADER_CRC_OFFSET + 8]
        .copy_from_slice(&crc.to_le_bytes());
    Ok(())
}

fn parse_copy_at(
    blocks: &[Vec<u8>],
    footer: &ParityMapFooter,
    expected_kind: ParityMapCopyKind,
) -> Result<DecodedParityMapTapeFile, ParityError> {
    let start = match expected_kind {
        ParityMapCopyKind::Primary => footer.primary_copy_start_block,
        ParityMapCopyKind::Tail => footer.tail_copy_start_block,
    };
    let start = usize::try_from(start)
        .map_err(|_| parity_map_parse("parity-map copy start overflows usize"))?;
    let header_block = blocks
        .get(start)
        .ok_or_else(|| parity_map_parse("parity-map copy start lies outside tape file"))?;
    let header = parse_parity_map_header_block(header_block, &footer.tape_uuid)?;
    if header.copy_kind != expected_kind {
        return Err(parity_map_parse(format!(
            "parity-map copy kind {:?} does not match expected {:?}",
            header.copy_kind, expected_kind
        )));
    }
    validate_header_matches_footer(&header, footer)?;
    decode_copy(blocks, start, header)
}

fn parse_available_copy_at(
    blocks: &[Option<Vec<u8>>],
    start: usize,
    expected_kind: ParityMapCopyKind,
    expected_tape_uuid: &[u8; 16],
    measured_total: u64,
    footer: Option<&ParityMapFooter>,
) -> Result<DecodedParityMapTapeFile, ParityError> {
    let header_block = blocks.get(start).and_then(Option::as_ref).ok_or_else(|| {
        parity_map_parse(format!("parity-map {expected_kind:?} header unreadable"))
    })?;
    let header = parse_parity_map_header_block(header_block, expected_tape_uuid)?;
    if header.copy_kind != expected_kind {
        return Err(parity_map_parse(format!(
            "parity-map copy kind {:?} does not match expected {:?}",
            header.copy_kind, expected_kind
        )));
    }
    if header.parity_map_total_block_count != measured_total {
        return Err(parity_map_parse(format!(
            "parity-map has {measured_total} blocks, {expected_kind:?} header expects {}",
            header.parity_map_total_block_count
        )));
    }
    if let Some(footer) = footer {
        validate_header_matches_footer(&header, footer)?;
    }

    let copy_block_count = usize::try_from(header.copy_block_count)
        .map_err(|_| parity_map_parse("parity-map copy block count overflows usize"))?;
    let end = start
        .checked_add(copy_block_count)
        .ok_or_else(|| parity_map_parse("parity-map copy end overflows"))?;
    let available = blocks
        .get(start..end)
        .ok_or_else(|| parity_map_parse("parity-map copy range outside tape file"))?;
    let mut copy_blocks = Vec::with_capacity(copy_block_count);
    for (offset, block) in available.iter().enumerate() {
        let block = block.as_ref().ok_or_else(|| {
            parity_map_parse(format!(
                "parity-map {expected_kind:?} payload block {} unreadable",
                start + offset
            ))
        })?;
        copy_blocks.push(block.clone());
    }
    decode_copy(&copy_blocks, 0, header)
}

fn decode_copy(
    blocks: &[Vec<u8>],
    start: usize,
    header: ParityMapHeader,
) -> Result<DecodedParityMapTapeFile, ParityError> {
    let payload_bytes = read_payload_from_copy(blocks, start, &header)?;
    if sha256_array(&payload_bytes) != header.payload_sha256 {
        return Err(parity_map_parse("parity-map payload sha256 mismatch"));
    }
    let payload = decode_parity_map_payload(&payload_bytes)?;
    validate_payload_matches_header(&payload, &header)?;
    Ok(DecodedParityMapTapeFile {
        header,
        payload,
        payload_bytes,
    })
}

fn read_payload_from_copy(
    blocks: &[Vec<u8>],
    start: usize,
    header: &ParityMapHeader,
) -> Result<Vec<u8>, ParityError> {
    let block_size = validate_block_size(header.block_size)?;
    let copy_block_count = usize::try_from(header.copy_block_count)
        .map_err(|_| parity_map_parse("parity-map copy block count overflows usize"))?;
    let end = start
        .checked_add(copy_block_count)
        .ok_or_else(|| parity_map_parse("parity-map copy end overflows"))?;
    let copy_blocks = blocks
        .get(start..end)
        .ok_or_else(|| parity_map_parse("parity-map copy range outside tape file"))?;
    if copy_blocks.iter().any(|block| block.len() != block_size) {
        return Err(parity_map_parse(
            "parity-map copy contains a block with the wrong size",
        ));
    }
    let payload_len = usize::try_from(header.payload_len)
        .map_err(|_| parity_map_parse("parity-map payload length overflows usize"))?;
    let copy_capacity = copy_block_count
        .checked_mul(block_size)
        .ok_or_else(|| parity_map_parse("parity-map copy capacity overflows"))?;
    let payload_end = PARITY_MAP_HEADER_LEN
        .checked_add(payload_len)
        .ok_or_else(|| parity_map_parse("parity-map payload end overflows"))?;
    if payload_end > copy_capacity {
        return Err(parity_map_parse("parity-map payload exceeds copy blocks"));
    }
    let mut payload = Vec::with_capacity(payload_len);
    let mut remaining = payload_len;
    let mut offset = PARITY_MAP_HEADER_LEN;
    while remaining > 0 {
        let block_index = offset / block_size;
        let within = offset % block_size;
        let take = remaining.min(block_size - within);
        payload.extend_from_slice(&copy_blocks[block_index][within..within + take]);
        offset += take;
        remaining -= take;
    }
    ensure_zero_tail(copy_blocks, payload_end, copy_capacity, block_size)?;
    Ok(payload)
}

fn validate_header_matches_footer(
    header: &ParityMapHeader,
    footer: &ParityMapFooter,
) -> Result<(), ParityError> {
    if header.magic != footer.magic
        || header.tape_uuid != footer.tape_uuid
        || header.sequence != footer.sequence
        || header.block_size != footer.block_size
        || header.payload_len != footer.payload_len
        || header.payload_sha256 != footer.payload_sha256
        || header.canonical_map_digest != footer.canonical_map_digest
        || header.directory_scope_tape_file_count != footer.directory_scope_tape_file_count
        || header.directory_scope_total_data_ordinals != footer.directory_scope_total_data_ordinals
        || header.directory_scope_highest_protected_ordinal
            != footer.directory_scope_highest_protected_ordinal
        || header.is_final_directory != footer.is_final_directory
        || header.copy_block_count != footer.copy_block_count
        || header.parity_map_total_block_count != footer.parity_map_total_block_count
        || header.primary_copy_start_block != footer.primary_copy_start_block
        || header.tail_copy_start_block != footer.tail_copy_start_block
        || header.footer_block_index != footer.footer_block_index
    {
        return Err(parity_map_parse(
            "parity-map header copy does not match footer locator",
        ));
    }
    Ok(())
}

fn validate_payload_matches_header(
    payload: &ParityMapPayload,
    header: &ParityMapHeader,
) -> Result<(), ParityError> {
    if payload.tape_uuid != header.tape_uuid
        || payload.sequence != header.sequence
        || payload.canonical_map_digest != header.canonical_map_digest
        || payload.directory.directory_scope_tape_file_count
            != header.directory_scope_tape_file_count
        || payload.directory.directory_scope_total_data_ordinals
            != header.directory_scope_total_data_ordinals
        || payload.directory.directory_scope_highest_protected_ordinal
            != header.directory_scope_highest_protected_ordinal
        || payload.directory.is_final_directory != header.is_final_directory
    {
        return Err(parity_map_parse(
            "parity-map payload does not match its header",
        ));
    }
    payload.directory.validate()
}

fn parity_map_copies_agree(
    left: &DecodedParityMapTapeFile,
    right: &DecodedParityMapTapeFile,
) -> bool {
    left.payload == right.payload
        && left.payload_bytes == right.payload_bytes
        && left.header.tape_uuid == right.header.tape_uuid
        && left.header.sequence == right.header.sequence
        && left.header.block_size == right.header.block_size
        && left.header.payload_len == right.header.payload_len
        && left.header.payload_sha256 == right.header.payload_sha256
        && left.header.canonical_map_digest == right.header.canonical_map_digest
        && left.header.directory_scope_tape_file_count
            == right.header.directory_scope_tape_file_count
        && left.header.directory_scope_total_data_ordinals
            == right.header.directory_scope_total_data_ordinals
        && left.header.directory_scope_highest_protected_ordinal
            == right.header.directory_scope_highest_protected_ordinal
        && left.header.is_final_directory == right.header.is_final_directory
        && left.header.copy_block_count == right.header.copy_block_count
        && left.header.parity_map_total_block_count == right.header.parity_map_total_block_count
        && left.header.primary_copy_start_block == right.header.primary_copy_start_block
        && left.header.tail_copy_start_block == right.header.tail_copy_start_block
        && left.header.footer_block_index == right.header.footer_block_index
}

fn validate_locator_counts(
    payload_len: u64,
    block_size: u32,
    copy_block_count: u64,
    total_block_count: u64,
    primary_copy_start_block: u64,
    tail_copy_start_block: u64,
    footer_block_index: u64,
) -> Result<(), ParityError> {
    let block_size = u64::try_from(validate_block_size(block_size)?)
        .map_err(|_| parity_map_parse("parity-map block size overflows u64"))?;
    let required_copy_bytes = u64::try_from(PARITY_MAP_HEADER_LEN)
        .map_err(|_| parity_map_parse("header length overflows u64"))?
        .checked_add(payload_len)
        .ok_or_else(|| parity_map_parse("parity-map required copy bytes overflow"))?;
    let expected_copy_blocks = required_copy_bytes.div_ceil(block_size);
    if copy_block_count != expected_copy_blocks {
        return Err(parity_map_parse(format!(
            "parity-map copy_block_count {copy_block_count} != expected {expected_copy_blocks}"
        )));
    }
    if primary_copy_start_block != 0 {
        return Err(parity_map_parse(
            "parity-map primary copy must start at block 0",
        ));
    }
    if tail_copy_start_block != copy_block_count {
        return Err(parity_map_parse(format!(
            "parity-map tail copy starts at {tail_copy_start_block}, expected {copy_block_count}"
        )));
    }
    let expected_footer = copy_block_count
        .checked_mul(2)
        .ok_or_else(|| parity_map_parse("parity-map expected footer index overflows"))?;
    if footer_block_index != expected_footer {
        return Err(parity_map_parse(format!(
            "parity-map footer index {footer_block_index} != expected {expected_footer}"
        )));
    }
    let expected_total = expected_footer
        .checked_add(1)
        .ok_or_else(|| parity_map_parse("parity-map expected total overflows"))?;
    if total_block_count != expected_total {
        return Err(parity_map_parse(format!(
            "parity-map total block count {total_block_count} != expected {expected_total}"
        )));
    }
    Ok(())
}

fn validate_block_size(block_size: u32) -> Result<usize, ParityError> {
    let block_size = usize::try_from(block_size)
        .map_err(|_| parity_map_parse("parity-map block size overflows usize"))?;
    if block_size < PARITY_MAP_HEADER_LEN || block_size < PARITY_MAP_FOOTER_LEN {
        return Err(parity_map_parse(
            "parity-map block size is smaller than fixed header/footer",
        ));
    }
    Ok(block_size)
}

fn write_bytes_at(
    blocks: &mut [Vec<u8>],
    mut offset: usize,
    mut bytes: &[u8],
) -> Result<(), ParityError> {
    let block_size = blocks
        .first()
        .map(Vec::len)
        .ok_or_else(|| parity_map_parse("parity-map copy has no blocks"))?;
    while !bytes.is_empty() {
        let block_index = offset / block_size;
        let within = offset % block_size;
        let block = blocks
            .get_mut(block_index)
            .ok_or_else(|| parity_map_parse("parity-map write offset exceeds copy blocks"))?;
        let take = bytes.len().min(block_size - within);
        block[within..within + take].copy_from_slice(&bytes[..take]);
        offset += take;
        bytes = &bytes[take..];
    }
    Ok(())
}

fn ensure_zero_tail(
    copy_blocks: &[Vec<u8>],
    payload_end: usize,
    copy_capacity: usize,
    block_size: usize,
) -> Result<(), ParityError> {
    if payload_end >= copy_capacity {
        return Ok(());
    }
    let mut offset = payload_end;
    while offset < copy_capacity {
        let block_index = offset / block_size;
        let within = offset % block_size;
        let take = (copy_capacity - offset).min(block_size - within);
        ensure_zero_filled(
            &copy_blocks[block_index][within..within + take],
            "parity-map copy padding",
        )?;
        offset += take;
    }
    Ok(())
}

fn ensure_zero_filled(bytes: &[u8], label: &str) -> Result<(), ParityError> {
    if let Some((index, byte)) = bytes
        .iter()
        .copied()
        .enumerate()
        .find(|(_, byte)| *byte != 0)
    {
        return Err(parity_map_parse(format!(
            "{label} non-zero at offset {index}: 0x{byte:02x}"
        )));
    }
    Ok(())
}

fn sha256_array(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn bytes_to_16(bytes: Vec<u8>, field: &str) -> Result<[u8; 16], ParityError> {
    bytes.try_into().map_err(|bytes: Vec<u8>| {
        parity_map_parse(format!("{field} has length {}, expected 16", bytes.len()))
    })
}

fn bytes_to_32(bytes: Vec<u8>, field: &str) -> Result<[u8; 32], ParityError> {
    bytes.try_into().map_err(|bytes: Vec<u8>| {
        parity_map_parse(format!("{field} has length {}, expected 32", bytes.len()))
    })
}

fn cbor_int_to_u32(i: ciborium::value::Integer, field: &str) -> Result<u32, ParityError> {
    let value: i128 = i.into();
    u32::try_from(value)
        .map_err(|_| parity_map_parse(format!("{field}: value {value} out of u32 range")))
}

fn cbor_int_to_u64(i: ciborium::value::Integer, field: &str) -> Result<u64, ParityError> {
    let value: i128 = i.into();
    u64::try_from(value)
        .map_err(|_| parity_map_parse(format!("{field}: value {value} out of u64 range")))
}

fn read_u16_le(buf: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(buf[offset..offset + 2].try_into().unwrap())
}

fn read_u32_le(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap())
}

fn read_u64_le(buf: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(buf[offset..offset + 8].try_into().unwrap())
}

fn parity_map_parse(message: impl Into<String>) -> ParityError {
    ParityError::ParityMapParse(message.into())
}

fn directory_invalid(message: impl Into<String>) -> ParityError {
    ParityError::DirectoryInvalid(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAPE_UUID: [u8; 16] = [0x5A; 16];
    const BLOCK_SIZE: u32 = 256;

    fn sample_directory() -> SidecarEpochDirectory {
        SidecarEpochDirectory {
            directory_scope_tape_file_count: 5,
            directory_scope_total_data_ordinals: 4,
            directory_scope_highest_protected_ordinal: 4,
            is_final_directory: true,
            entries: vec![
                SidecarEpochDirectoryEntry {
                    tape_file_number: 2,
                    epoch_id: 0,
                    protected_ordinal_start: 0,
                    protected_ordinal_end_exclusive: 2,
                    sidecar_total_block_count: 5,
                    sidecar_header_block_count: 2,
                    parity_shard_block_count: 1,
                    canonical_metadata_hash: [0x11; 32],
                    flags: SIDECAR_DIRECTORY_FLAG_PRIMARY_KNOWN_GOOD
                        | SIDECAR_DIRECTORY_FLAG_TAIL_KNOWN_GOOD,
                },
                SidecarEpochDirectoryEntry {
                    tape_file_number: 4,
                    epoch_id: 1,
                    protected_ordinal_start: 2,
                    protected_ordinal_end_exclusive: 4,
                    sidecar_total_block_count: 5,
                    sidecar_header_block_count: 2,
                    parity_shard_block_count: 1,
                    canonical_metadata_hash: [0x22; 32],
                    flags: SIDECAR_DIRECTORY_FLAG_FINAL_PARTIAL_EPOCH
                        | SIDECAR_DIRECTORY_FLAG_PRIMARY_KNOWN_GOOD
                        | SIDECAR_DIRECTORY_FLAG_TAIL_KNOWN_GOOD,
                },
            ],
        }
    }

    fn sample_payload() -> ParityMapPayload {
        ParityMapPayload {
            tape_uuid: TAPE_UUID,
            sequence: 7,
            directory: sample_directory(),
            canonical_map_digest: [0xA5; 32],
            writer_version: Some("test-writer".to_string()),
            write_timestamp: Some("2026-05-23T10:00:00Z".to_string()),
        }
    }

    fn assert_directory_invalid(directory: &SidecarEpochDirectory) {
        let err = directory.validate().expect_err("directory must be invalid");
        assert!(
            matches!(err, ParityError::DirectoryInvalid(_)),
            "expected DirectoryInvalid, got {err:?}"
        );
    }

    #[test]
    fn sidecar_epoch_directory_cbor_round_trips() {
        let directory = sample_directory();
        let cbor = encode_sidecar_epoch_directory_cbor(&directory).unwrap();
        let decoded = decode_sidecar_epoch_directory_cbor(cbor).unwrap();

        assert_eq!(decoded, directory);
        assert!(directory.encoded_len().unwrap() > 0);
    }

    #[test]
    fn parity_map_tape_file_replicates_tail_copy_and_footer() {
        let payload = sample_payload();
        let encoded = encode_parity_map_tape_file(&payload, BLOCK_SIZE).unwrap();

        let copy_blocks = encoded.header.copy_block_count as usize;
        assert_eq!(encoded.header.primary_copy_start_block, 0);
        assert_eq!(encoded.header.tail_copy_start_block as usize, copy_blocks);
        assert_eq!(encoded.header.footer_block_index as usize, copy_blocks * 2);
        assert_eq!(
            encoded.blocks.len(),
            encoded.header.parity_map_total_block_count as usize
        );

        let footer =
            parse_parity_map_footer_block(encoded.blocks.last().unwrap(), &TAPE_UUID).unwrap();
        assert_eq!(footer.payload_sha256, sha256_array(&encoded.payload_bytes));

        let decoded = parse_parity_map_tape_file(&encoded.blocks, &TAPE_UUID).unwrap();
        assert_eq!(decoded.payload, payload);
        assert_eq!(decoded.header.copy_kind, ParityMapCopyKind::Primary);
    }

    #[test]
    fn parity_map_parser_uses_tail_when_primary_header_is_damaged() {
        let payload = sample_payload();
        let mut encoded = encode_parity_map_tape_file(&payload, BLOCK_SIZE).unwrap();
        encoded.blocks[0][PARITY_MAP_HEADER_CRC_OFFSET] ^= 0x55;

        let decoded = parse_parity_map_tape_file(&encoded.blocks, &TAPE_UUID).unwrap();

        assert_eq!(decoded.payload, payload);
        assert_eq!(decoded.header.copy_kind, ParityMapCopyKind::Tail);
    }

    #[test]
    fn parity_map_parser_rejects_payload_hash_mismatch() {
        let payload = sample_payload();
        let mut encoded = encode_parity_map_tape_file(&payload, BLOCK_SIZE).unwrap();
        let tail_start = encoded.header.tail_copy_start_block as usize;
        encoded.blocks[0][PARITY_MAP_HEADER_LEN] ^= 0x01;
        encoded.blocks[tail_start][PARITY_MAP_HEADER_LEN] ^= 0x01;

        let err = parse_parity_map_tape_file(&encoded.blocks, &TAPE_UUID).unwrap_err();

        assert!(
            matches!(err, ParityError::ParityMapParse(message) if message.contains("both parity-map metadata copies failed"))
        );
    }

    #[test]
    fn directory_rejects_unknown_flags() {
        let mut directory = sample_directory();
        directory.entries[0].flags |= 0x8000_0000;

        let err = directory.validate().unwrap_err();

        assert!(
            matches!(err, ParityError::DirectoryInvalid(message) if message.contains("unknown flags"))
        );
    }

    #[test]
    fn directory_rejects_overlapping_protected_ranges() {
        let mut directory = sample_directory();
        directory.entries[1].protected_ordinal_start = 1;

        assert_directory_invalid(&directory);
    }

    #[test]
    fn directory_rejects_gap_between_protected_ranges() {
        let mut directory = sample_directory();
        directory.entries[1].protected_ordinal_start = 3;

        assert_directory_invalid(&directory);
    }

    #[test]
    fn directory_rejects_duplicate_epoch_id() {
        let mut directory = sample_directory();
        directory.entries[1].epoch_id = 0;

        assert_directory_invalid(&directory);
    }

    #[test]
    fn directory_rejects_nonzero_first_protected_start() {
        let mut directory = sample_directory();
        directory.entries[0].protected_ordinal_start = 1;

        assert_directory_invalid(&directory);
    }

    #[test]
    fn directory_rejects_epoch_ids_not_starting_at_zero() {
        let mut directory = sample_directory();
        directory.entries[0].epoch_id = 1;
        directory.entries[1].epoch_id = 2;

        assert_directory_invalid(&directory);
    }

    #[test]
    fn directory_allows_unprotected_tail_after_partition_end() {
        let mut directory = sample_directory();
        directory.directory_scope_total_data_ordinals = 5;

        directory
            .validate()
            .expect("[0, W) partition remains valid when W < T");
    }

    #[test]
    fn empty_directory_requires_zero_highest_protected_ordinal() {
        let mut directory = sample_directory();
        directory.entries.clear();
        directory.directory_scope_highest_protected_ordinal = 0;
        directory
            .validate()
            .expect("empty [0, 0) partition is valid");

        directory.directory_scope_highest_protected_ordinal = 1;
        assert_directory_invalid(&directory);
    }
}
