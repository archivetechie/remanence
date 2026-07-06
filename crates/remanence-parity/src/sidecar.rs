//! Parity-epoch sidecar tape-file codec for Layer 3c v0.4.4.
//!
//! A sidecar begins with a primary header/index copy, continues with raw
//! parity-shard blocks, repeats the header/index copy at the tail, and ends
//! with a footer locator. This module implements the fixed binary surface from
//! `docs/layer3c-design.md` §5.5 as tightened by
//! `docs/remanence-3c-implementation-addendum-v0.2.md`: little-endian fields,
//! HMAC-derived per-tape magic, CRC-64/XZ for every sidecar CRC, and index
//! packing that never splits a parity or data-CRC entry across block
//! boundaries.

use hmac::{Hmac, Mac};
pub use remanence_crc::{crc64_xz, CRC64_XZ_CHECK_VALUE};
use sha2::{Digest, Sha256};

use crate::error::ParityError;

type HmacSha256 = Hmac<Sha256>;

/// Sidecar schema version emitted and accepted by this codec.
pub const SIDECAR_SCHEMA_VERSION: u32 = 1;

/// Byte length of sidecar block 0's fixed header through the inline-index start.
pub const SIDECAR_HEADER_LEN: usize = 0xB8;

/// Byte offset of the `header_crc64` field in sidecar block 0.
pub const SIDECAR_HEADER_CRC_OFFSET: usize = 0xB0;

/// Sidecar footer schema version emitted and accepted by this codec.
pub const SIDECAR_FOOTER_VERSION: u16 = 1;

/// Byte length of the fixed footer fields including `footer_crc64`.
pub const SIDECAR_FOOTER_LEN: usize = 0x80;

/// Byte offset of the `footer_crc64` field in the footer block.
pub const SIDECAR_FOOTER_CRC_OFFSET: usize = 0x78;

/// Byte length of one packed parity-index entry.
pub const PARITY_INDEX_ENTRY_LEN: usize = 16;

/// Byte length of one packed data-shard CRC entry.
pub const DATA_CRC_ENTRY_LEN: usize = 8;

const SIDECAR_MAGIC_MESSAGE: &[u8] = b"REM\x00PAR\x01";
const SIDECAR_FOOTER_MAGIC_MESSAGE: &[u8] = b"REM\x00PARFOOT\x01";
const SIDECAR_METADATA_HASH_DOMAIN: &[u8] = b"remanence-sidecar-metadata-v1";

/// Header/index copy identity for replicated sidecar metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidecarCopyKind {
    /// Header/index copy at the beginning of the sidecar tape file.
    Primary,
    /// Header/index copy after the parity shard blocks.
    Tail,
}

impl SidecarCopyKind {
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
            _ => Err(sidecar_parse(format!(
                "unsupported sidecar copy kind: {value}"
            ))),
        }
    }
}

/// Writer-side description of the epoch whose parity sidecar is being encoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidecarDescriptor {
    /// Tape UUID used as the HMAC key for sidecar magic.
    pub tape_uuid: [u8; 16],
    /// Parity epoch identifier.
    pub epoch_id: u64,
    /// Data shards per stripe.
    pub k: u16,
    /// Parity shards per stripe.
    pub m: u16,
    /// Stripes per epoch.
    pub stripes_per_epoch: u32,
    /// Fixed tape block size in bytes.
    pub block_size: u32,
    /// First protected object-data ordinal in this sidecar's half-open range.
    pub protected_ordinal_start: u64,
    /// Exclusive end of this sidecar's protected object-data ordinal range.
    pub protected_ordinal_end_exclusive: u64,
}

/// Decoded sidecar block-0 header, including computed/stored CRC fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidecarHeader {
    /// HMAC-derived sidecar magic stored at offset `0x00`.
    pub magic: [u8; 8],
    /// Tape UUID stored at offset `0x08`.
    pub tape_uuid: [u8; 16],
    /// Parity epoch identifier.
    pub epoch_id: u64,
    /// Data shards per stripe.
    pub k: u16,
    /// Parity shards per stripe.
    pub m: u16,
    /// Stripes per epoch.
    pub stripes_per_epoch: u32,
    /// Fixed tape block size in bytes.
    pub block_size: u32,
    /// Sidecar schema version.
    pub schema_version: u32,
    /// First protected object-data ordinal in this sidecar's half-open range.
    pub protected_ordinal_start: u64,
    /// Exclusive end of this sidecar's protected object-data ordinal range.
    pub protected_ordinal_end_exclusive: u64,
    /// Padded RS data-shard width for the epoch, equal to `S * k`.
    pub logical_shard_count: u64,
    /// Real data-shard count, equal to `end_exclusive - start`.
    pub real_data_shard_count: u64,
    /// Number of raw parity shard blocks following the index, equal to `S * m`.
    pub parity_block_count: u32,
    /// Number of data-shard CRC entries, equal to `real_data_shard_count`.
    pub data_crc_count: u32,
    /// Number of header/index blocks, including block 0.
    pub shard_index_block_count: u32,
    /// Number of packed index-entry bytes stored after the header in block 0.
    pub inline_index_entry_bytes: u32,
    /// Total fixed-block count for the whole sidecar tape file, excluding the
    /// trailing filemark.
    pub sidecar_total_block_count: u64,
    /// Primary header/index copy start block; always 0 for v1.
    pub primary_header_start_block: u64,
    /// Tail header/index copy start block, equal to `H + P`.
    pub tail_header_start_block: u64,
    /// Footer locator block index, equal to `H + P + H`.
    pub footer_block_index: u64,
    /// Whether this decoded header block is the primary or tail copy.
    pub copy_kind: SidecarCopyKind,
    /// Copy generation, currently fixed at 0.
    pub copy_generation: u32,
    /// SHA-256 over deterministic sidecar metadata and index entries, excluding
    /// copy-local fields and CRC fields.
    pub canonical_metadata_hash: [u8; 32],
    /// CRC-64/XZ over bytes before the `header_crc64` field in block 0.
    pub header_crc64: u64,
    /// CRC-64/XZ over bytes `0..block_size-8` of block 0.
    pub block0_crc64: u64,
}

/// Decoded sidecar footer locator from the final block of a sidecar tape file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidecarFooter {
    /// HMAC-derived footer magic stored at offset `0x00`.
    pub magic: [u8; 8],
    /// Footer schema version.
    pub sidecar_footer_version: u16,
    /// Tape UUID stored in the footer.
    pub tape_uuid: [u8; 16],
    /// Parity epoch identifier.
    pub epoch_id: u64,
    /// First protected object-data ordinal in this sidecar's half-open range.
    pub protected_ordinal_start: u64,
    /// Exclusive end of this sidecar's protected object-data ordinal range.
    pub protected_ordinal_end_exclusive: u64,
    /// Number of header/index blocks in one metadata copy.
    pub sidecar_header_block_count: u32,
    /// Number of raw parity shard blocks.
    pub parity_shard_block_count: u32,
    /// Total fixed-block count for the sidecar tape file.
    pub sidecar_total_block_count: u64,
    /// Primary header/index start block; always 0 for v1.
    pub primary_header_start_block: u64,
    /// Tail header/index start block.
    pub tail_header_start_block: u64,
    /// Canonical metadata hash shared by the primary and tail copies.
    pub canonical_metadata_hash: [u8; 32],
    /// CRC-64/XZ over footer fields before this field.
    pub footer_crc64: u64,
}

/// One parity-shard index entry from the sidecar index stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParityShardIndexEntry {
    /// Stripe index within the epoch.
    pub stripe_index: u32,
    /// Parity shard index within the stripe.
    pub parity_index: u16,
    /// CRC-64/XZ of the full raw parity-shard block.
    pub parity_shard_crc64: u64,
}

/// The parsed or writer-supplied sidecar index stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidecarIndex {
    /// Parity-shard index entries in `(stripe_index, parity_index)` order.
    pub parity_entries: Vec<ParityShardIndexEntry>,
    /// CRC-64/XZ values for real object-data shards in ordinal order.
    pub data_shard_crc64s: Vec<u64>,
}

impl SidecarIndex {
    /// Construct a sidecar index from parity entries and data-shard CRCs.
    pub fn new(parity_entries: Vec<ParityShardIndexEntry>, data_shard_crc64s: Vec<u64>) -> Self {
        Self {
            parity_entries,
            data_shard_crc64s,
        }
    }
}

/// Encoded header/index blocks for a sidecar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedSidecarIndex {
    /// Header that was written into block 0.
    pub header: SidecarHeader,
    /// Header/index blocks only; raw parity shard blocks are not included.
    pub blocks: Vec<Vec<u8>>,
}

/// Encoded full parity-sidecar tape file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedSidecarTapeFile {
    /// Header that was written into block 0.
    pub header: SidecarHeader,
    /// Index stream written into the header/index blocks.
    pub index: SidecarIndex,
    /// Complete filemark-delimited sidecar blocks: primary header/index copy,
    /// raw parity shard blocks, tail header/index copy, then footer locator.
    pub blocks: Vec<Vec<u8>>,
}

/// Decoded sidecar header and index stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedSidecarIndex {
    /// Validated sidecar header.
    pub header: SidecarHeader,
    /// Validated sidecar index entries.
    pub index: SidecarIndex,
}

/// Decoded full parity-sidecar tape file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedSidecarTapeFile {
    /// Validated sidecar header.
    pub header: SidecarHeader,
    /// Validated sidecar index entries.
    pub index: SidecarIndex,
    /// Raw parity shard blocks in `(stripe_index, parity_index)` order.
    pub parity_shards: Vec<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryKind {
    Parity,
    Data,
}

#[derive(Clone, Copy, Debug)]
struct HeaderCounts {
    k: u16,
    m: u16,
    stripes_per_epoch: u32,
    protected_ordinal_start: u64,
    protected_ordinal_end_exclusive: u64,
    logical_shard_count: u64,
    real_data_shard_count: u64,
    parity_block_count: u32,
    data_crc_count: u32,
    shard_index_block_count: u32,
    sidecar_total_block_count: u64,
    primary_header_start_block: u64,
    tail_header_start_block: u64,
    footer_block_index: u64,
    copy_generation: u32,
}

/// Derive the 8-byte per-tape sidecar magic from the tape UUID.
///
/// Per v0.4.4 §5.5 this is
/// `HMAC-SHA256(key = tape_uuid, msg = b"REM\0PAR\1")[0..8]`.
pub fn derive_sidecar_magic(tape_uuid: &[u8; 16]) -> [u8; 8] {
    let mut mac = HmacSha256::new_from_slice(tape_uuid).expect("HMAC accepts any key length");
    mac.update(SIDECAR_MAGIC_MESSAGE);
    let result = mac.finalize().into_bytes();
    let mut magic = [0u8; 8];
    magic.copy_from_slice(&result[..8]);
    magic
}

/// Derive the 8-byte per-tape sidecar footer magic from the tape UUID.
pub fn derive_sidecar_footer_magic(tape_uuid: &[u8; 16]) -> [u8; 8] {
    let mut mac = HmacSha256::new_from_slice(tape_uuid).expect("HMAC accepts any key length");
    mac.update(SIDECAR_FOOTER_MAGIC_MESSAGE);
    let result = mac.finalize().into_bytes();
    let mut magic = [0u8; 8];
    magic.copy_from_slice(&result[..8]);
    magic
}

/// CRC-64/XZ for a raw parity shard block.
pub fn parity_shard_crc64(shard: &[u8]) -> u64 {
    crc64_xz(shard)
}

/// CRC-64/XZ for a fixed-size object-data block.
pub fn data_shard_crc64(block: &[u8]) -> u64 {
    crc64_xz(block)
}

/// Encode sidecar header/index blocks from a descriptor and index entries.
///
/// The returned blocks contain block 0 and any spilled index blocks. Raw
/// parity-shard blocks are intentionally not appended; the caller writes them
/// after these blocks using the same `(stripe_index, parity_index)` order.
pub fn encode_sidecar_index_blocks(
    descriptor: &SidecarDescriptor,
    index: &SidecarIndex,
) -> Result<EncodedSidecarIndex, ParityError> {
    let block_size = validate_descriptor(descriptor)?;
    validate_index_shape(descriptor, index)?;

    let (expected_h, expected_inline) = compute_index_layout(
        block_size,
        index.parity_entries.len(),
        index.data_shard_crc64s.len(),
    )?;
    let mut blocks = pack_index_blocks(block_size, index)?;
    let canonical_metadata_hash =
        compute_canonical_metadata_hash(descriptor, index, expected_h, expected_inline)?;

    let mut header = build_header(
        descriptor,
        expected_h,
        expected_inline,
        SidecarCopyKind::Primary,
        canonical_metadata_hash,
    )?;
    finalize_header_block(&mut header, &mut blocks[0]);

    Ok(EncodedSidecarIndex { header, blocks })
}

/// Encode a complete parity-sidecar tape file.
///
/// The returned `blocks` are ready to write as one filemark-delimited sidecar
/// tape file: primary header/index block(s), raw parity shard blocks, tail
/// header/index block(s), then the footer locator. Parity shard CRCs are
/// computed from the supplied shard bytes and stored in the index.
pub fn encode_sidecar_tape_file<B: AsRef<[u8]>>(
    descriptor: &SidecarDescriptor,
    parity_shards: &[B],
    data_shard_crc64s: Vec<u64>,
) -> Result<EncodedSidecarTapeFile, ParityError> {
    let block_size = validate_descriptor(descriptor)?;
    let expected_parity_shards = descriptor
        .stripes_per_epoch
        .checked_mul(descriptor.m as u32)
        .ok_or_else(|| sidecar_parse("sidecar parity block count overflows u32"))?
        as usize;
    if parity_shards.len() != expected_parity_shards {
        return Err(sidecar_parse(format!(
            "sidecar has {} parity shards, expected {expected_parity_shards}",
            parity_shards.len()
        )));
    }

    let mut parity_entries = Vec::with_capacity(expected_parity_shards);
    for (i, shard) in parity_shards.iter().enumerate() {
        let shard = shard.as_ref();
        if shard.len() != block_size {
            return Err(sidecar_parse(format!(
                "sidecar parity shard {i} length {} != block_size {block_size}",
                shard.len()
            )));
        }
        parity_entries.push(ParityShardIndexEntry {
            stripe_index: (i / descriptor.m as usize) as u32,
            parity_index: (i % descriptor.m as usize) as u16,
            parity_shard_crc64: parity_shard_crc64(shard),
        });
    }

    let index = SidecarIndex::new(parity_entries, data_shard_crc64s);
    let encoded_index = encode_sidecar_index_blocks(descriptor, &index)?;
    let mut blocks = encoded_index.blocks;
    blocks.reserve(parity_shards.len() + encoded_index.header.shard_index_block_count as usize + 1);
    for shard in parity_shards {
        blocks.push(shard.as_ref().to_vec());
    }
    let block_size = descriptor.block_size as usize;
    let mut tail_blocks = pack_index_blocks(block_size, &index)?;
    let mut tail_header = build_header(
        descriptor,
        encoded_index.header.shard_index_block_count,
        encoded_index.header.inline_index_entry_bytes,
        SidecarCopyKind::Tail,
        encoded_index.header.canonical_metadata_hash,
    )?;
    finalize_header_block(&mut tail_header, &mut tail_blocks[0]);
    blocks.extend(tail_blocks);
    blocks.push(encode_sidecar_footer_block(&encoded_index.header)?);

    Ok(EncodedSidecarTapeFile {
        header: encoded_index.header,
        index,
        blocks,
    })
}

/// Parse and validate a sidecar block-0 header.
///
/// This validates sidecar magic, tape UUID, schema version, structural counts,
/// header CRC, and block-0 CRC. Catalog-less scanners should use this before
/// classifying a tape file as a sidecar.
pub fn parse_sidecar_header_block(
    block0: &[u8],
    expected_tape_uuid: &[u8; 16],
) -> Result<SidecarHeader, ParityError> {
    if block0.len() < SIDECAR_HEADER_LEN + 8 {
        return Err(sidecar_parse(format!(
            "sidecar block0 too short: got {}, need at least {}",
            block0.len(),
            SIDECAR_HEADER_LEN + 8
        )));
    }

    let mut magic = [0u8; 8];
    magic.copy_from_slice(&block0[0x00..0x08]);
    let expected_magic = derive_sidecar_magic(expected_tape_uuid);
    if magic != expected_magic {
        return Err(sidecar_parse("sidecar magic mismatch"));
    }

    let mut tape_uuid = [0u8; 16];
    tape_uuid.copy_from_slice(&block0[0x08..0x18]);
    if &tape_uuid != expected_tape_uuid {
        return Err(sidecar_parse("sidecar tape UUID mismatch"));
    }

    let epoch_id = read_u64_le(block0, 0x18);
    let k = read_u16_le(block0, 0x20);
    let m = read_u16_le(block0, 0x22);
    let stripes_per_epoch = read_u32_le(block0, 0x24);
    let block_size = read_u32_le(block0, 0x28);
    let schema_version = read_u32_le(block0, 0x2C);
    let protected_ordinal_start = read_u64_le(block0, 0x30);
    let protected_ordinal_end_exclusive = read_u64_le(block0, 0x38);
    let logical_shard_count = read_u64_le(block0, 0x40);
    let real_data_shard_count = read_u64_le(block0, 0x48);
    let parity_block_count = read_u32_le(block0, 0x50);
    let data_crc_count = read_u32_le(block0, 0x54);
    let shard_index_block_count = read_u32_le(block0, 0x58);
    let inline_index_entry_bytes = read_u32_le(block0, 0x5C);
    let sidecar_total_block_count = read_u64_le(block0, 0x60);
    let primary_header_start_block = read_u64_le(block0, 0x68);
    let tail_header_start_block = read_u64_le(block0, 0x70);
    let footer_block_index = read_u64_le(block0, 0x78);
    let copy_kind = SidecarCopyKind::from_u16(read_u16_le(block0, 0x80))?;
    let copy_kind_reserved = read_u16_le(block0, 0x82);
    if copy_kind_reserved != 0 {
        return Err(sidecar_parse(format!(
            "sidecar copy-kind reserved field non-zero: 0x{copy_kind_reserved:04x}"
        )));
    }
    let copy_generation = read_u32_le(block0, 0x84);
    let mut canonical_metadata_hash = [0u8; 32];
    canonical_metadata_hash.copy_from_slice(&block0[0x88..0xA8]);
    let header_reserved = read_u64_le(block0, 0xA8);
    if header_reserved != 0 {
        return Err(sidecar_parse(format!(
            "sidecar header reserved field non-zero: 0x{header_reserved:016x}"
        )));
    }
    let header_crc64 = read_u64_le(block0, SIDECAR_HEADER_CRC_OFFSET);

    if schema_version != SIDECAR_SCHEMA_VERSION {
        return Err(sidecar_parse(format!(
            "unsupported sidecar schema version: got {schema_version}, accept {SIDECAR_SCHEMA_VERSION}"
        )));
    }
    let block_size_usize = usize::try_from(block_size)
        .map_err(|_| sidecar_parse(format!("sidecar block_size {block_size} overflows usize")))?;
    if block_size_usize != block0.len() {
        return Err(sidecar_parse(format!(
            "sidecar block_size {block_size} does not match block0 length {}",
            block0.len()
        )));
    }
    if block_size_usize < SIDECAR_HEADER_LEN + 8 {
        return Err(sidecar_parse(
            "sidecar block_size smaller than header plus trailing CRC",
        ));
    }

    let computed_header_crc64 = crc64_xz(&block0[..SIDECAR_HEADER_CRC_OFFSET]);
    if header_crc64 != computed_header_crc64 {
        return Err(sidecar_parse(format!(
            "sidecar header CRC mismatch: stored 0x{header_crc64:016x}, computed 0x{computed_header_crc64:016x}"
        )));
    }

    let crc_offset = block_size_usize - 8;
    let block0_crc64 = read_u64_le(block0, crc_offset);
    let computed_block0_crc64 = crc64_xz(&block0[..crc_offset]);
    if block0_crc64 != computed_block0_crc64 {
        return Err(sidecar_parse(format!(
            "sidecar block0 CRC mismatch: stored 0x{block0_crc64:016x}, computed 0x{computed_block0_crc64:016x}"
        )));
    }

    validate_header_counts(HeaderCounts {
        k,
        m,
        stripes_per_epoch,
        protected_ordinal_start,
        protected_ordinal_end_exclusive,
        logical_shard_count,
        real_data_shard_count,
        parity_block_count,
        data_crc_count,
        shard_index_block_count,
        sidecar_total_block_count,
        primary_header_start_block,
        tail_header_start_block,
        footer_block_index,
        copy_generation,
    })?;

    let (expected_h, expected_inline) = compute_index_layout(
        block_size_usize,
        parity_block_count as usize,
        data_crc_count as usize,
    )?;
    if shard_index_block_count != expected_h {
        return Err(sidecar_parse(format!(
            "sidecar shard_index_block_count {shard_index_block_count} != expected {expected_h}"
        )));
    }
    if inline_index_entry_bytes != expected_inline {
        return Err(sidecar_parse(format!(
            "sidecar inline_index_entry_bytes {inline_index_entry_bytes} != expected {expected_inline}"
        )));
    }

    let inline_end = SIDECAR_HEADER_LEN + inline_index_entry_bytes as usize;
    if inline_end > crc_offset {
        return Err(sidecar_parse(
            "sidecar inline index exceeds block0 capacity",
        ));
    }
    ensure_zero_filled(&block0[inline_end..crc_offset], "block0 index padding")?;

    Ok(SidecarHeader {
        magic,
        tape_uuid,
        epoch_id,
        k,
        m,
        stripes_per_epoch,
        block_size,
        schema_version,
        protected_ordinal_start,
        protected_ordinal_end_exclusive,
        logical_shard_count,
        real_data_shard_count,
        parity_block_count,
        data_crc_count,
        shard_index_block_count,
        inline_index_entry_bytes,
        sidecar_total_block_count,
        primary_header_start_block,
        tail_header_start_block,
        footer_block_index,
        copy_kind,
        copy_generation,
        canonical_metadata_hash,
        header_crc64,
        block0_crc64,
    })
}

/// Classify a candidate block-0 as a parity sidecar header.
///
/// Catalog-less scanners can call this while walking tape files. `Ok(None)`
/// means the candidate does not have this tape's sidecar magic and should be
/// treated as a non-sidecar tape file. If the magic matches, this delegates to
/// [`parse_sidecar_header_block`] so CRC, UUID, schema, and count validation
/// still gate sidecar classification.
pub fn classify_sidecar_header_block(
    block0: &[u8],
    expected_tape_uuid: &[u8; 16],
) -> Result<Option<SidecarHeader>, ParityError> {
    let expected_magic = derive_sidecar_magic(expected_tape_uuid);
    if block0.len() < expected_magic.len() || block0[..expected_magic.len()] != expected_magic {
        return Ok(None);
    }

    let header = parse_sidecar_header_block(block0, expected_tape_uuid)?;
    if header.copy_kind != SidecarCopyKind::Primary {
        return Err(sidecar_parse(format!(
            "sidecar file starts with {:?} header copy, expected primary",
            header.copy_kind
        )));
    }
    Ok(Some(header))
}

/// Parse and validate the final sidecar footer locator block.
pub fn parse_sidecar_footer_block(
    footer_block: &[u8],
    expected_tape_uuid: &[u8; 16],
) -> Result<SidecarFooter, ParityError> {
    if footer_block.len() < SIDECAR_FOOTER_LEN {
        return Err(sidecar_parse(format!(
            "sidecar footer block too short: got {}, need at least {SIDECAR_FOOTER_LEN}",
            footer_block.len()
        )));
    }

    let mut magic = [0u8; 8];
    magic.copy_from_slice(&footer_block[0x00..0x08]);
    let expected_magic = derive_sidecar_footer_magic(expected_tape_uuid);
    if magic != expected_magic {
        return Err(sidecar_parse("sidecar footer magic mismatch"));
    }

    let sidecar_footer_version = read_u16_le(footer_block, 0x08);
    if sidecar_footer_version != SIDECAR_FOOTER_VERSION {
        return Err(sidecar_parse(format!(
            "unsupported sidecar footer version: got {sidecar_footer_version}, accept {SIDECAR_FOOTER_VERSION}"
        )));
    }
    let footer_reserved16 = read_u16_le(footer_block, 0x0A);
    let footer_reserved32 = read_u32_le(footer_block, 0x0C);
    if footer_reserved16 != 0 || footer_reserved32 != 0 {
        return Err(sidecar_parse("sidecar footer reserved fields are non-zero"));
    }

    let mut tape_uuid = [0u8; 16];
    tape_uuid.copy_from_slice(&footer_block[0x10..0x20]);
    if &tape_uuid != expected_tape_uuid {
        return Err(sidecar_parse("sidecar footer tape UUID mismatch"));
    }

    let epoch_id = read_u64_le(footer_block, 0x20);
    let protected_ordinal_start = read_u64_le(footer_block, 0x28);
    let protected_ordinal_end_exclusive = read_u64_le(footer_block, 0x30);
    let sidecar_header_block_count = read_u32_le(footer_block, 0x38);
    let parity_shard_block_count = read_u32_le(footer_block, 0x3C);
    let sidecar_total_block_count = read_u64_le(footer_block, 0x40);
    let primary_header_start_block = read_u64_le(footer_block, 0x48);
    let tail_header_start_block = read_u64_le(footer_block, 0x50);
    let mut canonical_metadata_hash = [0u8; 32];
    canonical_metadata_hash.copy_from_slice(&footer_block[0x58..0x78]);
    let footer_crc64 = read_u64_le(footer_block, SIDECAR_FOOTER_CRC_OFFSET);
    let computed = crc64_xz(&footer_block[..SIDECAR_FOOTER_CRC_OFFSET]);
    if footer_crc64 != computed {
        return Err(sidecar_parse(format!(
            "sidecar footer CRC mismatch: stored 0x{footer_crc64:016x}, computed 0x{computed:016x}"
        )));
    }
    ensure_zero_filled(
        &footer_block[SIDECAR_FOOTER_LEN..],
        "sidecar footer padding",
    )?;

    validate_sidecar_layout(
        sidecar_header_block_count,
        parity_shard_block_count,
        sidecar_total_block_count,
        primary_header_start_block,
        tail_header_start_block,
        sidecar_total_block_count
            .checked_sub(1)
            .ok_or_else(|| sidecar_parse("sidecar footer total block count is zero"))?,
    )?;

    Ok(SidecarFooter {
        magic,
        sidecar_footer_version,
        tape_uuid,
        epoch_id,
        protected_ordinal_start,
        protected_ordinal_end_exclusive,
        sidecar_header_block_count,
        parity_shard_block_count,
        sidecar_total_block_count,
        primary_header_start_block,
        tail_header_start_block,
        canonical_metadata_hash,
        footer_crc64,
    })
}

/// Parse and validate sidecar header/index blocks.
///
/// `blocks` must include at least the `H` header/index blocks named in block 0.
/// Extra blocks are ignored so callers can pass a prefix of a larger sidecar
/// file buffer that also contains raw parity shards.
pub fn parse_sidecar_index_blocks<B: AsRef<[u8]>>(
    blocks: &[B],
    expected_tape_uuid: &[u8; 16],
) -> Result<DecodedSidecarIndex, ParityError> {
    let block0 = blocks
        .first()
        .ok_or_else(|| sidecar_parse("sidecar has no header block"))?
        .as_ref();
    let header = parse_sidecar_header_block(block0, expected_tape_uuid)?;
    let block_size = header.block_size as usize;
    let h = header.shard_index_block_count as usize;
    if blocks.len() < h {
        return Err(sidecar_parse(format!(
            "sidecar has {} index blocks, header requires {h}",
            blocks.len()
        )));
    }

    for (i, block) in blocks.iter().take(h).enumerate() {
        let block = block.as_ref();
        if block.len() != block_size {
            return Err(sidecar_parse(format!(
                "sidecar index block {i} length {} != block_size {block_size}",
                block.len()
            )));
        }
        if i > 0 {
            validate_spill_block_crc(block, i)?;
        }
    }

    let index = parse_index_entries(&header, blocks)?;
    validate_index_order(&header, &index)?;
    let descriptor = SidecarDescriptor {
        tape_uuid: header.tape_uuid,
        epoch_id: header.epoch_id,
        k: header.k,
        m: header.m,
        stripes_per_epoch: header.stripes_per_epoch,
        block_size: header.block_size,
        protected_ordinal_start: header.protected_ordinal_start,
        protected_ordinal_end_exclusive: header.protected_ordinal_end_exclusive,
    };
    let expected_hash = compute_canonical_metadata_hash(
        &descriptor,
        &index,
        header.shard_index_block_count,
        header.inline_index_entry_bytes,
    )?;
    if header.canonical_metadata_hash != expected_hash {
        return Err(sidecar_parse("sidecar canonical metadata hash mismatch"));
    }
    Ok(DecodedSidecarIndex { header, index })
}

/// Parse and validate a complete parity-sidecar tape file.
///
/// `blocks` must be the exact filemark-delimited sidecar file: `H` primary
/// header/index blocks, `P` raw parity shard blocks, `H` tail header/index
/// blocks, and one footer locator block. This enforces the catalog
/// `block_count == 2 * H + P + 1` identity and validates every raw parity
/// shard against its index CRC.
pub fn parse_sidecar_tape_file<B: AsRef<[u8]>>(
    blocks: &[B],
    expected_tape_uuid: &[u8; 16],
) -> Result<DecodedSidecarTapeFile, ParityError> {
    let footer_block = blocks
        .last()
        .ok_or_else(|| sidecar_parse("sidecar has no footer block"))?
        .as_ref();
    let footer = parse_sidecar_footer_block(footer_block, expected_tape_uuid)?;
    let expected_blocks = usize::try_from(footer.sidecar_total_block_count)
        .map_err(|_| sidecar_parse("sidecar total block count overflows usize"))?;
    if blocks.len() != expected_blocks {
        return Err(sidecar_parse(format!(
            "sidecar tape file has {} blocks, expected exactly {expected_blocks}",
            blocks.len()
        )));
    }
    let h = usize::try_from(footer.sidecar_header_block_count)
        .map_err(|_| sidecar_parse("sidecar header block count overflows usize"))?;
    let p = usize::try_from(footer.parity_shard_block_count)
        .map_err(|_| sidecar_parse("sidecar parity block count overflows usize"))?;
    let tail_start = usize::try_from(footer.tail_header_start_block)
        .map_err(|_| sidecar_parse("sidecar tail header start overflows usize"))?;
    let tail_end = tail_start
        .checked_add(h)
        .ok_or_else(|| sidecar_parse("sidecar tail header range overflows usize"))?;
    if h == 0 || tail_end > blocks.len() - 1 {
        return Err(sidecar_parse("sidecar tail header range is outside file"));
    }

    let primary = parse_sidecar_index_copy(
        &blocks[..h],
        expected_tape_uuid,
        &footer,
        SidecarCopyKind::Primary,
    );
    let tail = parse_sidecar_index_copy(
        &blocks[tail_start..tail_end],
        expected_tape_uuid,
        &footer,
        SidecarCopyKind::Tail,
    );
    let decoded = match (primary, tail) {
        (Ok(primary), Ok(tail)) => {
            if !sidecar_header_metadata_matches(&primary.header, &tail.header)
                || primary.index != tail.index
            {
                return Err(sidecar_parse(
                    "sidecar primary and tail metadata copies disagree",
                ));
            }
            primary
        }
        (Ok(primary), Err(_)) => primary,
        (Err(_), Ok(tail)) => tail,
        (Err(primary_err), Err(tail_err)) => {
            return Err(sidecar_parse(format!(
                "no usable sidecar header/index copy: primary={primary_err}; tail={tail_err}"
            )));
        }
    };

    let block_size = decoded.header.block_size as usize;
    let mut parity_shards = Vec::with_capacity(p);
    for (i, entry) in decoded.index.parity_entries.iter().enumerate() {
        let block_index = h + i;
        let shard = blocks[block_index].as_ref();
        if shard.len() != block_size {
            return Err(sidecar_parse(format!(
                "sidecar parity shard {i} block length {} != block_size {block_size}",
                shard.len()
            )));
        }
        let computed = parity_shard_crc64(shard);
        if computed != entry.parity_shard_crc64 {
            return Err(sidecar_parse(format!(
                "sidecar parity shard {i} ({}, {}) CRC mismatch: stored 0x{:016x}, computed 0x{computed:016x}",
                entry.stripe_index, entry.parity_index, entry.parity_shard_crc64
            )));
        }
        parity_shards.push(shard.to_vec());
    }

    Ok(DecodedSidecarTapeFile {
        header: decoded.header,
        index: decoded.index,
        parity_shards,
    })
}

fn validate_descriptor(descriptor: &SidecarDescriptor) -> Result<usize, ParityError> {
    let block_size = usize::try_from(descriptor.block_size).map_err(|_| {
        sidecar_parse(format!(
            "sidecar block_size {} overflows usize",
            descriptor.block_size
        ))
    })?;
    if block_size < SIDECAR_HEADER_LEN + 8 {
        return Err(sidecar_parse(
            "sidecar block_size smaller than header plus trailing CRC",
        ));
    }
    validate_header_counts(HeaderCounts {
        k: descriptor.k,
        m: descriptor.m,
        stripes_per_epoch: descriptor.stripes_per_epoch,
        protected_ordinal_start: descriptor.protected_ordinal_start,
        protected_ordinal_end_exclusive: descriptor.protected_ordinal_end_exclusive,
        logical_shard_count: descriptor.stripes_per_epoch as u64 * descriptor.k as u64,
        real_data_shard_count: descriptor
            .protected_ordinal_end_exclusive
            .checked_sub(descriptor.protected_ordinal_start)
            .ok_or_else(|| sidecar_parse("sidecar protected ordinal range is inverted"))?,
        parity_block_count: descriptor
            .stripes_per_epoch
            .checked_mul(descriptor.m as u32)
            .ok_or_else(|| sidecar_parse("sidecar parity block count overflows u32"))?,
        data_crc_count: descriptor
            .protected_ordinal_end_exclusive
            .checked_sub(descriptor.protected_ordinal_start)
            .and_then(|d| u32::try_from(d).ok())
            .ok_or_else(|| sidecar_parse("sidecar data CRC count overflows u32"))?,
        shard_index_block_count: 1,
        sidecar_total_block_count: 0,
        primary_header_start_block: 0,
        tail_header_start_block: 0,
        footer_block_index: 0,
        copy_generation: 0,
    })?;
    Ok(block_size)
}

fn validate_header_counts(counts: HeaderCounts) -> Result<(), ParityError> {
    let HeaderCounts {
        k,
        m,
        stripes_per_epoch,
        protected_ordinal_start,
        protected_ordinal_end_exclusive,
        logical_shard_count,
        real_data_shard_count,
        parity_block_count,
        data_crc_count,
        shard_index_block_count,
        sidecar_total_block_count,
        primary_header_start_block,
        tail_header_start_block,
        footer_block_index,
        copy_generation,
    } = counts;
    if k == 0 || m == 0 || stripes_per_epoch == 0 {
        return Err(sidecar_parse("sidecar k, m, and S must all be non-zero"));
    }
    let range_len = protected_ordinal_end_exclusive
        .checked_sub(protected_ordinal_start)
        .ok_or_else(|| sidecar_parse("sidecar protected ordinal range is inverted"))?;
    if range_len == 0 {
        return Err(sidecar_parse("sidecar protects zero real ordinals"));
    }
    let expected_logical = stripes_per_epoch as u64 * k as u64;
    if logical_shard_count != expected_logical {
        return Err(sidecar_parse(format!(
            "sidecar logical_shard_count {logical_shard_count} != S*k {expected_logical}"
        )));
    }
    if real_data_shard_count != range_len {
        return Err(sidecar_parse(format!(
            "sidecar real_data_shard_count {real_data_shard_count} != range length {range_len}"
        )));
    }
    if real_data_shard_count > logical_shard_count {
        return Err(sidecar_parse(
            "sidecar real data count exceeds logical shard count",
        ));
    }
    let expected_parity = stripes_per_epoch
        .checked_mul(m as u32)
        .ok_or_else(|| sidecar_parse("sidecar parity block count overflows u32"))?;
    if parity_block_count != expected_parity {
        return Err(sidecar_parse(format!(
            "sidecar parity_block_count {parity_block_count} != S*m {expected_parity}"
        )));
    }
    let expected_data_crc_count: u32 = real_data_shard_count
        .try_into()
        .map_err(|_| sidecar_parse("sidecar real data count exceeds u32 data CRC count"))?;
    if data_crc_count != expected_data_crc_count {
        return Err(sidecar_parse(format!(
            "sidecar data_crc_count {data_crc_count} != real data count {expected_data_crc_count}"
        )));
    }
    if copy_generation != 0 {
        return Err(sidecar_parse(format!(
            "unsupported sidecar copy_generation: {copy_generation}"
        )));
    }
    if sidecar_total_block_count != 0 {
        validate_sidecar_layout(
            shard_index_block_count,
            parity_block_count,
            sidecar_total_block_count,
            primary_header_start_block,
            tail_header_start_block,
            footer_block_index,
        )?;
    }
    Ok(())
}

fn validate_sidecar_layout(
    shard_index_block_count: u32,
    parity_block_count: u32,
    sidecar_total_block_count: u64,
    primary_header_start_block: u64,
    tail_header_start_block: u64,
    footer_block_index: u64,
) -> Result<(), ParityError> {
    if shard_index_block_count == 0 {
        return Err(sidecar_parse("sidecar header block count is zero"));
    }
    if primary_header_start_block != 0 {
        return Err(sidecar_parse(format!(
            "sidecar primary_header_start_block {primary_header_start_block} != 0"
        )));
    }
    let expected_tail = u64::from(shard_index_block_count)
        .checked_add(u64::from(parity_block_count))
        .ok_or_else(|| sidecar_parse("sidecar tail header start overflows"))?;
    if tail_header_start_block != expected_tail {
        return Err(sidecar_parse(format!(
            "sidecar tail_header_start_block {tail_header_start_block} != expected {expected_tail}"
        )));
    }
    let expected_footer = expected_tail
        .checked_add(u64::from(shard_index_block_count))
        .ok_or_else(|| sidecar_parse("sidecar footer block index overflows"))?;
    if footer_block_index != expected_footer {
        return Err(sidecar_parse(format!(
            "sidecar footer_block_index {footer_block_index} != expected {expected_footer}"
        )));
    }
    let expected_total = expected_footer
        .checked_add(1)
        .ok_or_else(|| sidecar_parse("sidecar total block count overflows"))?;
    if sidecar_total_block_count != expected_total {
        return Err(sidecar_parse(format!(
            "sidecar_total_block_count {sidecar_total_block_count} != expected {expected_total}"
        )));
    }
    Ok(())
}

fn validate_index_shape(
    descriptor: &SidecarDescriptor,
    index: &SidecarIndex,
) -> Result<(), ParityError> {
    let expected_parity = descriptor
        .stripes_per_epoch
        .checked_mul(descriptor.m as u32)
        .ok_or_else(|| sidecar_parse("sidecar parity block count overflows u32"))?
        as usize;
    if index.parity_entries.len() != expected_parity {
        return Err(sidecar_parse(format!(
            "sidecar parity index has {} entries, expected {expected_parity}",
            index.parity_entries.len()
        )));
    }

    let expected_data = descriptor
        .protected_ordinal_end_exclusive
        .checked_sub(descriptor.protected_ordinal_start)
        .ok_or_else(|| sidecar_parse("sidecar protected ordinal range is inverted"))?
        as usize;
    if index.data_shard_crc64s.len() != expected_data {
        return Err(sidecar_parse(format!(
            "sidecar data CRC index has {} entries, expected {expected_data}",
            index.data_shard_crc64s.len()
        )));
    }

    for (i, entry) in index.parity_entries.iter().enumerate() {
        let expected_stripe = (i / descriptor.m as usize) as u32;
        let expected_parity = (i % descriptor.m as usize) as u16;
        if entry.stripe_index != expected_stripe || entry.parity_index != expected_parity {
            return Err(sidecar_parse(format!(
                "sidecar parity index entry {i} is ({}, {}), expected ({expected_stripe}, {expected_parity})",
                entry.stripe_index, entry.parity_index
            )));
        }
    }
    Ok(())
}

fn validate_index_order(header: &SidecarHeader, index: &SidecarIndex) -> Result<(), ParityError> {
    let descriptor = SidecarDescriptor {
        tape_uuid: header.tape_uuid,
        epoch_id: header.epoch_id,
        k: header.k,
        m: header.m,
        stripes_per_epoch: header.stripes_per_epoch,
        block_size: header.block_size,
        protected_ordinal_start: header.protected_ordinal_start,
        protected_ordinal_end_exclusive: header.protected_ordinal_end_exclusive,
    };
    validate_index_shape(&descriptor, index)
}

fn compute_canonical_metadata_hash(
    descriptor: &SidecarDescriptor,
    index: &SidecarIndex,
    shard_index_block_count: u32,
    inline_index_entry_bytes: u32,
) -> Result<[u8; 32], ParityError> {
    let real_data_shard_count = descriptor
        .protected_ordinal_end_exclusive
        .checked_sub(descriptor.protected_ordinal_start)
        .ok_or_else(|| sidecar_parse("sidecar protected ordinal range is inverted"))?;
    let parity_block_count = descriptor
        .stripes_per_epoch
        .checked_mul(descriptor.m as u32)
        .ok_or_else(|| sidecar_parse("sidecar parity block count overflows u32"))?;
    let logical_shard_count = descriptor.stripes_per_epoch as u64 * descriptor.k as u64;
    let tail_header_start_block = u64::from(shard_index_block_count)
        .checked_add(u64::from(parity_block_count))
        .ok_or_else(|| sidecar_parse("sidecar tail header start overflows"))?;
    let footer_block_index = tail_header_start_block
        .checked_add(u64::from(shard_index_block_count))
        .ok_or_else(|| sidecar_parse("sidecar footer block index overflows"))?;
    let sidecar_total_block_count = footer_block_index
        .checked_add(1)
        .ok_or_else(|| sidecar_parse("sidecar total block count overflows"))?;

    let mut hash = Sha256::new();
    hash.update(SIDECAR_METADATA_HASH_DOMAIN);
    hash.update(derive_sidecar_magic(&descriptor.tape_uuid));
    hash.update(descriptor.tape_uuid);
    hash.update(descriptor.epoch_id.to_le_bytes());
    hash.update(descriptor.k.to_le_bytes());
    hash.update(descriptor.m.to_le_bytes());
    hash.update(descriptor.stripes_per_epoch.to_le_bytes());
    hash.update(descriptor.block_size.to_le_bytes());
    hash.update(SIDECAR_SCHEMA_VERSION.to_le_bytes());
    hash.update(descriptor.protected_ordinal_start.to_le_bytes());
    hash.update(descriptor.protected_ordinal_end_exclusive.to_le_bytes());
    hash.update(logical_shard_count.to_le_bytes());
    hash.update(real_data_shard_count.to_le_bytes());
    hash.update(parity_block_count.to_le_bytes());
    let data_crc_count: u32 = real_data_shard_count
        .try_into()
        .map_err(|_| sidecar_parse("sidecar data CRC count overflows u32"))?;
    hash.update(data_crc_count.to_le_bytes());
    hash.update(shard_index_block_count.to_le_bytes());
    hash.update(inline_index_entry_bytes.to_le_bytes());
    hash.update(sidecar_total_block_count.to_le_bytes());
    hash.update(0u64.to_le_bytes());
    hash.update(tail_header_start_block.to_le_bytes());
    hash.update(footer_block_index.to_le_bytes());
    for entry in &index.parity_entries {
        hash.update(entry.stripe_index.to_le_bytes());
        hash.update(entry.parity_index.to_le_bytes());
        hash.update(0u16.to_le_bytes());
        hash.update(entry.parity_shard_crc64.to_le_bytes());
    }
    for crc in &index.data_shard_crc64s {
        hash.update(crc.to_le_bytes());
    }

    let digest = hash.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    Ok(out)
}

fn encode_sidecar_footer_block(header: &SidecarHeader) -> Result<Vec<u8>, ParityError> {
    let block_size = usize::try_from(header.block_size)
        .map_err(|_| sidecar_parse("sidecar block_size overflows usize"))?;
    if block_size < SIDECAR_FOOTER_LEN {
        return Err(sidecar_parse(
            "sidecar block_size smaller than footer locator",
        ));
    }

    let mut block = vec![0u8; block_size];
    block[0x00..0x08].copy_from_slice(&derive_sidecar_footer_magic(&header.tape_uuid));
    block[0x08..0x0A].copy_from_slice(&SIDECAR_FOOTER_VERSION.to_le_bytes());
    block[0x0A..0x0C].copy_from_slice(&0u16.to_le_bytes());
    block[0x0C..0x10].copy_from_slice(&0u32.to_le_bytes());
    block[0x10..0x20].copy_from_slice(&header.tape_uuid);
    block[0x20..0x28].copy_from_slice(&header.epoch_id.to_le_bytes());
    block[0x28..0x30].copy_from_slice(&header.protected_ordinal_start.to_le_bytes());
    block[0x30..0x38].copy_from_slice(&header.protected_ordinal_end_exclusive.to_le_bytes());
    block[0x38..0x3C].copy_from_slice(&header.shard_index_block_count.to_le_bytes());
    block[0x3C..0x40].copy_from_slice(&header.parity_block_count.to_le_bytes());
    block[0x40..0x48].copy_from_slice(&header.sidecar_total_block_count.to_le_bytes());
    block[0x48..0x50].copy_from_slice(&header.primary_header_start_block.to_le_bytes());
    block[0x50..0x58].copy_from_slice(&header.tail_header_start_block.to_le_bytes());
    block[0x58..0x78].copy_from_slice(&header.canonical_metadata_hash);
    let crc = crc64_xz(&block[..SIDECAR_FOOTER_CRC_OFFSET]);
    block[SIDECAR_FOOTER_CRC_OFFSET..SIDECAR_FOOTER_CRC_OFFSET + 8]
        .copy_from_slice(&crc.to_le_bytes());
    Ok(block)
}

fn parse_sidecar_index_copy<B: AsRef<[u8]>>(
    blocks: &[B],
    expected_tape_uuid: &[u8; 16],
    footer: &SidecarFooter,
    expected_copy_kind: SidecarCopyKind,
) -> Result<DecodedSidecarIndex, ParityError> {
    let decoded = parse_sidecar_index_blocks(blocks, expected_tape_uuid)?;
    if decoded.header.copy_kind != expected_copy_kind {
        return Err(sidecar_parse(format!(
            "sidecar {:?} copy decoded as {:?}",
            expected_copy_kind, decoded.header.copy_kind
        )));
    }
    validate_header_matches_footer(&decoded.header, footer)?;
    Ok(decoded)
}

fn validate_header_matches_footer(
    header: &SidecarHeader,
    footer: &SidecarFooter,
) -> Result<(), ParityError> {
    if header.tape_uuid != footer.tape_uuid
        || header.epoch_id != footer.epoch_id
        || header.protected_ordinal_start != footer.protected_ordinal_start
        || header.protected_ordinal_end_exclusive != footer.protected_ordinal_end_exclusive
        || header.shard_index_block_count != footer.sidecar_header_block_count
        || header.parity_block_count != footer.parity_shard_block_count
        || header.sidecar_total_block_count != footer.sidecar_total_block_count
        || header.primary_header_start_block != footer.primary_header_start_block
        || header.tail_header_start_block != footer.tail_header_start_block
        || header.canonical_metadata_hash != footer.canonical_metadata_hash
    {
        return Err(sidecar_parse(
            "sidecar header/index copy does not match footer locator",
        ));
    }
    Ok(())
}

fn sidecar_header_metadata_matches(left: &SidecarHeader, right: &SidecarHeader) -> bool {
    left.magic == right.magic
        && left.tape_uuid == right.tape_uuid
        && left.epoch_id == right.epoch_id
        && left.k == right.k
        && left.m == right.m
        && left.stripes_per_epoch == right.stripes_per_epoch
        && left.block_size == right.block_size
        && left.schema_version == right.schema_version
        && left.protected_ordinal_start == right.protected_ordinal_start
        && left.protected_ordinal_end_exclusive == right.protected_ordinal_end_exclusive
        && left.logical_shard_count == right.logical_shard_count
        && left.real_data_shard_count == right.real_data_shard_count
        && left.parity_block_count == right.parity_block_count
        && left.data_crc_count == right.data_crc_count
        && left.shard_index_block_count == right.shard_index_block_count
        && left.inline_index_entry_bytes == right.inline_index_entry_bytes
        && left.sidecar_total_block_count == right.sidecar_total_block_count
        && left.primary_header_start_block == right.primary_header_start_block
        && left.tail_header_start_block == right.tail_header_start_block
        && left.footer_block_index == right.footer_block_index
        && left.copy_generation == right.copy_generation
        && left.canonical_metadata_hash == right.canonical_metadata_hash
}

fn build_header(
    descriptor: &SidecarDescriptor,
    shard_index_block_count: u32,
    inline_index_entry_bytes: u32,
    copy_kind: SidecarCopyKind,
    canonical_metadata_hash: [u8; 32],
) -> Result<SidecarHeader, ParityError> {
    let real_data_shard_count = descriptor
        .protected_ordinal_end_exclusive
        .checked_sub(descriptor.protected_ordinal_start)
        .ok_or_else(|| sidecar_parse("sidecar protected ordinal range is inverted"))?;
    let parity_block_count = descriptor
        .stripes_per_epoch
        .checked_mul(descriptor.m as u32)
        .ok_or_else(|| sidecar_parse("sidecar parity block count overflows u32"))?;
    let data_crc_count: u32 = real_data_shard_count
        .try_into()
        .map_err(|_| sidecar_parse("sidecar data CRC count overflows u32"))?;
    let tail_header_start_block = u64::from(shard_index_block_count)
        .checked_add(u64::from(parity_block_count))
        .ok_or_else(|| sidecar_parse("sidecar tail header start overflows"))?;
    let footer_block_index = tail_header_start_block
        .checked_add(u64::from(shard_index_block_count))
        .ok_or_else(|| sidecar_parse("sidecar footer block index overflows"))?;
    let sidecar_total_block_count = footer_block_index
        .checked_add(1)
        .ok_or_else(|| sidecar_parse("sidecar total block count overflows"))?;
    Ok(SidecarHeader {
        magic: derive_sidecar_magic(&descriptor.tape_uuid),
        tape_uuid: descriptor.tape_uuid,
        epoch_id: descriptor.epoch_id,
        k: descriptor.k,
        m: descriptor.m,
        stripes_per_epoch: descriptor.stripes_per_epoch,
        block_size: descriptor.block_size,
        schema_version: SIDECAR_SCHEMA_VERSION,
        protected_ordinal_start: descriptor.protected_ordinal_start,
        protected_ordinal_end_exclusive: descriptor.protected_ordinal_end_exclusive,
        logical_shard_count: descriptor.stripes_per_epoch as u64 * descriptor.k as u64,
        real_data_shard_count,
        parity_block_count,
        data_crc_count,
        shard_index_block_count,
        inline_index_entry_bytes,
        sidecar_total_block_count,
        primary_header_start_block: 0,
        tail_header_start_block,
        footer_block_index,
        copy_kind,
        copy_generation: 0,
        canonical_metadata_hash,
        header_crc64: 0,
        block0_crc64: 0,
    })
}

fn pack_index_blocks(block_size: usize, index: &SidecarIndex) -> Result<Vec<Vec<u8>>, ParityError> {
    let entry_kinds = entry_kinds(index.parity_entries.len(), index.data_shard_crc64s.len());
    let mut blocks = vec![vec![0u8; block_size]];
    let mut block_index = 0usize;
    let mut offset = SIDECAR_HEADER_LEN;
    let limit = block_size - 8;
    let mut parity_i = 0usize;
    let mut data_i = 0usize;

    for kind in entry_kinds {
        let len = entry_len(kind);
        if offset + len > limit {
            if offset == block_start(block_index) {
                return Err(sidecar_parse(format!(
                    "sidecar block_size {block_size} cannot hold a {len}-byte index entry"
                )));
            }
            if block_index > 0 {
                write_trailing_crc(&mut blocks[block_index]);
            }
            blocks.push(vec![0u8; block_size]);
            block_index += 1;
            offset = 0;
        }

        match kind {
            EntryKind::Parity => {
                write_parity_entry(
                    &index.parity_entries[parity_i],
                    &mut blocks[block_index],
                    offset,
                );
                parity_i += 1;
            }
            EntryKind::Data => {
                blocks[block_index][offset..offset + 8]
                    .copy_from_slice(&index.data_shard_crc64s[data_i].to_le_bytes());
                data_i += 1;
            }
        }
        offset += len;
    }

    for block in blocks.iter_mut().skip(1) {
        write_trailing_crc(block);
    }
    Ok(blocks)
}

fn parse_index_entries<B: AsRef<[u8]>>(
    header: &SidecarHeader,
    blocks: &[B],
) -> Result<SidecarIndex, ParityError> {
    let block_size = header.block_size as usize;
    let crc_offset = block_size - 8;
    let h = header.shard_index_block_count as usize;
    let inline_end = SIDECAR_HEADER_LEN + header.inline_index_entry_bytes as usize;
    let entry_kinds = entry_kinds(
        header.parity_block_count as usize,
        header.data_crc_count as usize,
    );

    let mut block_index = 0usize;
    let mut offset = SIDECAR_HEADER_LEN;
    let mut limit = inline_end;
    let mut parity_entries = Vec::with_capacity(header.parity_block_count as usize);
    let mut data_shard_crc64s = Vec::with_capacity(header.data_crc_count as usize);

    for kind in entry_kinds {
        let len = entry_len(kind);
        if offset + len > limit {
            ensure_zero_filled(
                &blocks[block_index].as_ref()[offset..crc_offset],
                "sidecar index padding",
            )?;
            block_index += 1;
            if block_index >= h {
                return Err(sidecar_parse(
                    "sidecar index entries exceed declared index blocks",
                ));
            }
            offset = 0;
            limit = crc_offset;
        }

        let block = blocks[block_index].as_ref();
        match kind {
            EntryKind::Parity => {
                let stripe_index = read_u32_le(block, offset);
                let parity_index = read_u16_le(block, offset + 4);
                let reserved = read_u16_le(block, offset + 6);
                if reserved != 0 {
                    return Err(sidecar_parse(format!(
                        "sidecar parity index reserved field non-zero: 0x{reserved:04x}"
                    )));
                }
                let parity_shard_crc64 = read_u64_le(block, offset + 8);
                parity_entries.push(ParityShardIndexEntry {
                    stripe_index,
                    parity_index,
                    parity_shard_crc64,
                });
            }
            EntryKind::Data => {
                data_shard_crc64s.push(read_u64_le(block, offset));
            }
        }
        offset += len;
    }

    ensure_zero_filled(
        &blocks[block_index].as_ref()[offset..crc_offset],
        "sidecar index padding",
    )?;
    for block in blocks.iter().take(h).skip(block_index + 1) {
        ensure_zero_filled(&block.as_ref()[..crc_offset], "sidecar unused index block")?;
    }

    Ok(SidecarIndex {
        parity_entries,
        data_shard_crc64s,
    })
}

fn compute_index_layout(
    block_size: usize,
    parity_count: usize,
    data_count: usize,
) -> Result<(u32, u32), ParityError> {
    if block_size < SIDECAR_HEADER_LEN + 8 {
        return Err(sidecar_parse(
            "sidecar block_size smaller than header plus trailing CRC",
        ));
    }
    let limit = block_size - 8;
    let mut block_index = 0usize;
    let mut offset = SIDECAR_HEADER_LEN;
    let mut inline = 0usize;

    for kind in entry_kinds(parity_count, data_count) {
        let len = entry_len(kind);
        if len > limit {
            return Err(sidecar_parse(format!(
                "sidecar block_size {block_size} cannot hold a {len}-byte index entry"
            )));
        }
        if offset + len > limit {
            if offset == block_start(block_index) {
                return Err(sidecar_parse(format!(
                    "sidecar block_size {block_size} cannot hold a {len}-byte index entry"
                )));
            }
            if block_index == 0 {
                inline = offset - SIDECAR_HEADER_LEN;
            }
            block_index += 1;
            offset = 0;
        }
        offset += len;
    }
    if block_index == 0 {
        inline = offset - SIDECAR_HEADER_LEN;
    }

    let h = u32::try_from(block_index + 1)
        .map_err(|_| sidecar_parse("sidecar index block count overflows u32"))?;
    let inline = u32::try_from(inline)
        .map_err(|_| sidecar_parse("sidecar inline index byte count overflows u32"))?;
    Ok((h, inline))
}

fn entry_kinds(parity_count: usize, data_count: usize) -> impl Iterator<Item = EntryKind> {
    std::iter::repeat_n(EntryKind::Parity, parity_count)
        .chain(std::iter::repeat_n(EntryKind::Data, data_count))
}

fn entry_len(kind: EntryKind) -> usize {
    match kind {
        EntryKind::Parity => PARITY_INDEX_ENTRY_LEN,
        EntryKind::Data => DATA_CRC_ENTRY_LEN,
    }
}

fn block_start(block_index: usize) -> usize {
    if block_index == 0 {
        SIDECAR_HEADER_LEN
    } else {
        0
    }
}

fn write_header_without_crc(header: &SidecarHeader, block0: &mut [u8]) {
    block0[0x00..0x08].copy_from_slice(&header.magic);
    block0[0x08..0x18].copy_from_slice(&header.tape_uuid);
    block0[0x18..0x20].copy_from_slice(&header.epoch_id.to_le_bytes());
    block0[0x20..0x22].copy_from_slice(&header.k.to_le_bytes());
    block0[0x22..0x24].copy_from_slice(&header.m.to_le_bytes());
    block0[0x24..0x28].copy_from_slice(&header.stripes_per_epoch.to_le_bytes());
    block0[0x28..0x2C].copy_from_slice(&header.block_size.to_le_bytes());
    block0[0x2C..0x30].copy_from_slice(&header.schema_version.to_le_bytes());
    block0[0x30..0x38].copy_from_slice(&header.protected_ordinal_start.to_le_bytes());
    block0[0x38..0x40].copy_from_slice(&header.protected_ordinal_end_exclusive.to_le_bytes());
    block0[0x40..0x48].copy_from_slice(&header.logical_shard_count.to_le_bytes());
    block0[0x48..0x50].copy_from_slice(&header.real_data_shard_count.to_le_bytes());
    block0[0x50..0x54].copy_from_slice(&header.parity_block_count.to_le_bytes());
    block0[0x54..0x58].copy_from_slice(&header.data_crc_count.to_le_bytes());
    block0[0x58..0x5C].copy_from_slice(&header.shard_index_block_count.to_le_bytes());
    block0[0x5C..0x60].copy_from_slice(&header.inline_index_entry_bytes.to_le_bytes());
    block0[0x60..0x68].copy_from_slice(&header.sidecar_total_block_count.to_le_bytes());
    block0[0x68..0x70].copy_from_slice(&header.primary_header_start_block.to_le_bytes());
    block0[0x70..0x78].copy_from_slice(&header.tail_header_start_block.to_le_bytes());
    block0[0x78..0x80].copy_from_slice(&header.footer_block_index.to_le_bytes());
    block0[0x80..0x82].copy_from_slice(&header.copy_kind.to_u16().to_le_bytes());
    block0[0x82..0x84].copy_from_slice(&0u16.to_le_bytes());
    block0[0x84..0x88].copy_from_slice(&header.copy_generation.to_le_bytes());
    block0[0x88..0xA8].copy_from_slice(&header.canonical_metadata_hash);
    block0[0xA8..0xB0].copy_from_slice(&0u64.to_le_bytes());
}

fn finalize_header_block(header: &mut SidecarHeader, block0: &mut [u8]) {
    block0[..SIDECAR_HEADER_LEN].fill(0);
    write_header_without_crc(header, block0);
    header.header_crc64 = crc64_xz(&block0[..SIDECAR_HEADER_CRC_OFFSET]);
    block0[SIDECAR_HEADER_CRC_OFFSET..SIDECAR_HEADER_CRC_OFFSET + 8]
        .copy_from_slice(&header.header_crc64.to_le_bytes());

    let crc_offset = block0.len() - 8;
    header.block0_crc64 = crc64_xz(&block0[..crc_offset]);
    block0[crc_offset..].copy_from_slice(&header.block0_crc64.to_le_bytes());
}

fn write_parity_entry(entry: &ParityShardIndexEntry, block: &mut [u8], offset: usize) {
    block[offset..offset + 4].copy_from_slice(&entry.stripe_index.to_le_bytes());
    block[offset + 4..offset + 6].copy_from_slice(&entry.parity_index.to_le_bytes());
    block[offset + 6..offset + 8].copy_from_slice(&0u16.to_le_bytes());
    block[offset + 8..offset + 16].copy_from_slice(&entry.parity_shard_crc64.to_le_bytes());
}

fn write_trailing_crc(block: &mut [u8]) {
    let crc_offset = block.len() - 8;
    let crc = crc64_xz(&block[..crc_offset]);
    block[crc_offset..].copy_from_slice(&crc.to_le_bytes());
}

fn validate_spill_block_crc(block: &[u8], block_index: usize) -> Result<(), ParityError> {
    let crc_offset = block.len() - 8;
    let stored = read_u64_le(block, crc_offset);
    let computed = crc64_xz(&block[..crc_offset]);
    if stored != computed {
        return Err(sidecar_parse(format!(
            "sidecar index block {block_index} CRC mismatch: stored 0x{stored:016x}, computed 0x{computed:016x}"
        )));
    }
    Ok(())
}

fn ensure_zero_filled(bytes: &[u8], label: &str) -> Result<(), ParityError> {
    if let Some(pos) = bytes.iter().position(|b| *b != 0) {
        return Err(sidecar_parse(format!(
            "{label} contains non-zero byte at offset {pos}"
        )));
    }
    Ok(())
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

fn sidecar_parse(message: impl Into<String>) -> ParityError {
    ParityError::SidecarParse(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_uuid() -> [u8; 16] {
        [
            0x10, 0x32, 0x54, 0x76, 0x98, 0xBA, 0xDC, 0xFE, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB,
            0xCD, 0xEF,
        ]
    }

    fn descriptor(block_size: u32) -> SidecarDescriptor {
        SidecarDescriptor {
            tape_uuid: sample_uuid(),
            epoch_id: 7,
            k: 4,
            m: 2,
            stripes_per_epoch: 3,
            block_size,
            protected_ordinal_start: 30,
            protected_ordinal_end_exclusive: 42,
        }
    }

    fn parity_shards_for(desc: &SidecarDescriptor) -> Vec<Vec<u8>> {
        let mut shards = Vec::new();
        for stripe in 0..desc.stripes_per_epoch {
            for parity in 0..desc.m {
                let mut shard = vec![0u8; desc.block_size as usize];
                shard[0] = stripe as u8;
                shard[1] = parity as u8;
                let last = shard.len() - 1;
                shard[last] = 0x5A;
                shards.push(shard);
            }
        }
        shards
    }

    fn data_crc64s_for(desc: &SidecarDescriptor) -> Vec<u64> {
        let data_count =
            (desc.protected_ordinal_end_exclusive - desc.protected_ordinal_start) as usize;
        (0..data_count)
            .map(|i| {
                let mut block = vec![0u8; desc.block_size as usize];
                block[0] = i as u8;
                let last = block.len() - 1;
                block[last] = 0xA5;
                data_shard_crc64(&block)
            })
            .collect()
    }

    fn index_for(desc: &SidecarDescriptor) -> SidecarIndex {
        let parity_entries = parity_shards_for(desc)
            .iter()
            .enumerate()
            .map(|(i, shard)| ParityShardIndexEntry {
                stripe_index: (i / desc.m as usize) as u32,
                parity_index: (i % desc.m as usize) as u16,
                parity_shard_crc64: parity_shard_crc64(shard),
            })
            .collect();

        SidecarIndex::new(parity_entries, data_crc64s_for(desc))
    }

    fn descriptor_for_scheme(
        block_size: u32,
        scheme: &crate::ParityScheme,
        epoch_id: u64,
    ) -> SidecarDescriptor {
        let protected_ordinal_start = 0;
        let protected_ordinal_end_exclusive =
            u64::from(scheme.data_blocks_per_stripe) * u64::from(scheme.stripes_per_neighborhood);
        SidecarDescriptor {
            tape_uuid: sample_uuid(),
            epoch_id,
            k: scheme.data_blocks_per_stripe,
            m: scheme.parity_blocks_per_stripe,
            stripes_per_epoch: scheme.stripes_per_neighborhood,
            block_size,
            protected_ordinal_start,
            protected_ordinal_end_exclusive,
        }
    }

    fn metadata_only_index_for(desc: &SidecarDescriptor) -> SidecarIndex {
        let parity_entries = (0..desc.stripes_per_epoch)
            .flat_map(|stripe_index| {
                (0..desc.m).map(move |parity_index| ParityShardIndexEntry {
                    stripe_index,
                    parity_index,
                    parity_shard_crc64: 0xA5A5_0000_0000_0000
                        ^ ((stripe_index as u64) << 16)
                        ^ parity_index as u64,
                })
            })
            .collect::<Vec<_>>();
        let data_shard_crc64s = (desc.protected_ordinal_start
            ..desc.protected_ordinal_end_exclusive)
            .map(|ordinal| 0x5A5A_0000_0000_0000 ^ ordinal)
            .collect::<Vec<_>>();

        SidecarIndex::new(parity_entries, data_shard_crc64s)
    }

    #[test]
    fn crc64_xz_matches_normative_check_value() {
        assert_eq!(crc64_xz(b"123456789"), CRC64_XZ_CHECK_VALUE);
    }

    #[test]
    fn crc64_xz_testing_plan_vectors_and_little_endian_packing_are_stable() {
        assert_eq!(crc64_xz(b""), 0x0000_0000_0000_0000);
        assert_eq!(crc64_xz(&[0x00]), 0x1fad_a173_6467_3f59);
        assert_eq!(crc64_xz(&[0xff]), 0xff00_0000_0000_0000);

        let all_zero = vec![0x00; 256 * 1024];
        let all_ff = vec![0xff; 256 * 1024];
        let mut patterned = (0..256 * 1024)
            .map(|i| ((i * 37 + i / 251 * 17 + 0x5a) & 0xff) as u8)
            .collect::<Vec<_>>();

        assert_eq!(crc64_xz(&all_zero), 0x261b_df3d_2998_38fc);
        assert_eq!(crc64_xz(&all_ff), 0x5543_3dd0_f389_08ba);
        assert_eq!(crc64_xz(&patterned), 0x5fc8_b8c7_ab4b_d3ef);

        let original = crc64_xz(&patterned);
        patterned[12_345] ^= 0x01;
        assert_eq!(crc64_xz(&patterned), 0xf070_7c60_4a2b_85b2);
        assert_ne!(crc64_xz(&patterned), original);

        assert_eq!(
            CRC64_XZ_CHECK_VALUE.to_le_bytes(),
            [0xfa, 0x39, 0x19, 0xdf, 0xbb, 0xc9, 0x5d, 0x99]
        );
    }

    #[test]
    fn sidecar_magic_is_hmac_derived() {
        let uuid = sample_uuid();
        let mut mac = HmacSha256::new_from_slice(&uuid).unwrap();
        mac.update(SIDECAR_MAGIC_MESSAGE);
        let result = mac.finalize().into_bytes();
        assert_eq!(derive_sidecar_magic(&uuid), result[..8]);
    }

    #[test]
    fn sidecar_header_and_index_roundtrip() {
        let desc = descriptor(256);
        let index = index_for(&desc);

        let encoded = encode_sidecar_index_blocks(&desc, &index).expect("encode sidecar");
        assert_eq!(encoded.header.magic, derive_sidecar_magic(&desc.tape_uuid));
        assert_eq!(encoded.header.schema_version, SIDECAR_SCHEMA_VERSION);
        assert_eq!(encoded.header.logical_shard_count, 12);
        assert_eq!(encoded.header.real_data_shard_count, 12);
        assert_eq!(encoded.header.parity_block_count, 6);
        assert_eq!(encoded.header.data_crc_count, 12);
        assert_eq!(encoded.header.shard_index_block_count, 2);
        assert_eq!(encoded.header.inline_index_entry_bytes, 64);
        assert_eq!(encoded.header.sidecar_total_block_count, 11);
        assert_eq!(encoded.header.primary_header_start_block, 0);
        assert_eq!(encoded.header.tail_header_start_block, 8);
        assert_eq!(encoded.header.footer_block_index, 10);
        assert_eq!(encoded.header.copy_kind, SidecarCopyKind::Primary);

        let decoded =
            parse_sidecar_index_blocks(&encoded.blocks, &desc.tape_uuid).expect("parse sidecar");
        assert_eq!(decoded.header, encoded.header);
        assert_eq!(decoded.index, index);
    }

    #[test]
    fn full_sidecar_tape_file_roundtrip_validates_parity_shards() {
        let desc = descriptor(256);
        let parity_shards = parity_shards_for(&desc);
        let data_crcs = data_crc64s_for(&desc);

        let encoded = encode_sidecar_tape_file(&desc, &parity_shards, data_crcs)
            .expect("encode full sidecar");
        let expected_blocks = encoded.header.sidecar_total_block_count;
        assert_eq!(encoded.blocks.len(), expected_blocks as usize);
        let first_parity_block = encoded.header.shard_index_block_count as usize;
        assert_eq!(encoded.blocks[first_parity_block], parity_shards[0]);
        assert_eq!(
            encoded.index.parity_entries[0].parity_shard_crc64,
            parity_shard_crc64(&parity_shards[0])
        );

        let decoded =
            parse_sidecar_tape_file(&encoded.blocks, &desc.tape_uuid).expect("parse sidecar");
        assert_eq!(decoded.header, encoded.header);
        assert_eq!(decoded.index, encoded.index);
        assert_eq!(decoded.parity_shards, parity_shards);
    }

    #[test]
    fn full_sidecar_tape_file_replicates_tail_copy_and_footer_locator() {
        let desc = descriptor(256);
        let parity_shards = parity_shards_for(&desc);
        let data_crcs = data_crc64s_for(&desc);

        let encoded = encode_sidecar_tape_file(&desc, &parity_shards, data_crcs)
            .expect("encode full sidecar");
        let h = encoded.header.shard_index_block_count as usize;
        let p = encoded.header.parity_block_count as usize;
        let tail_start = encoded.header.tail_header_start_block as usize;
        let footer_index = encoded.header.footer_block_index as usize;

        assert_eq!(tail_start, h + p);
        assert_eq!(footer_index, h + p + h);
        assert_eq!(encoded.blocks.len(), footer_index + 1);

        let tail = parse_sidecar_index_blocks(
            &encoded.blocks[tail_start..tail_start + h],
            &desc.tape_uuid,
        )
        .expect("tail copy parses");
        assert_eq!(tail.header.copy_kind, SidecarCopyKind::Tail);
        assert!(sidecar_header_metadata_matches(
            &encoded.header,
            &tail.header
        ));
        assert_eq!(tail.index, encoded.index);

        let footer = parse_sidecar_footer_block(&encoded.blocks[footer_index], &desc.tape_uuid)
            .expect("footer parses");
        assert_eq!(footer.sidecar_footer_version, SIDECAR_FOOTER_VERSION);
        assert_eq!(
            footer.canonical_metadata_hash,
            encoded.header.canonical_metadata_hash
        );
        assert_eq!(
            footer.sidecar_total_block_count,
            encoded.header.sidecar_total_block_count
        );
    }

    #[test]
    fn parser_uses_tail_copy_when_primary_header_is_damaged() {
        let desc = descriptor(256);
        let parity_shards = parity_shards_for(&desc);
        let data_crcs = data_crc64s_for(&desc);
        let mut encoded = encode_sidecar_tape_file(&desc, &parity_shards, data_crcs)
            .expect("encode full sidecar");
        encoded.blocks[0][0] ^= 0x01;

        let decoded =
            parse_sidecar_tape_file(&encoded.blocks, &desc.tape_uuid).expect("tail copy recovers");
        assert_eq!(decoded.header.copy_kind, SidecarCopyKind::Tail);
        assert_eq!(decoded.index, encoded.index);
        assert_eq!(decoded.parity_shards, parity_shards);
    }

    #[test]
    fn parser_rejects_footer_metadata_hash_mismatch() {
        let desc = descriptor(256);
        let parity_shards = parity_shards_for(&desc);
        let data_crcs = data_crc64s_for(&desc);
        let mut encoded = encode_sidecar_tape_file(&desc, &parity_shards, data_crcs)
            .expect("encode full sidecar");
        let footer_index = encoded.header.footer_block_index as usize;
        encoded.blocks[footer_index][0x58] ^= 0x01;
        let crc = crc64_xz(&encoded.blocks[footer_index][..SIDECAR_FOOTER_CRC_OFFSET]);
        encoded.blocks[footer_index][SIDECAR_FOOTER_CRC_OFFSET..SIDECAR_FOOTER_CRC_OFFSET + 8]
            .copy_from_slice(&crc.to_le_bytes());

        let err = parse_sidecar_tape_file(&encoded.blocks, &desc.tape_uuid).unwrap_err();
        match err {
            ParityError::SidecarParse(msg) => assert!(msg.contains("footer locator"), "{msg}"),
            other => panic!("expected sidecar parse error, got {other:?}"),
        }
    }

    #[test]
    fn sidecar_header_offsets_are_little_endian() {
        let desc = descriptor(512);
        let index = index_for(&desc);
        let encoded = encode_sidecar_index_blocks(&desc, &index).expect("encode sidecar");
        let block0 = &encoded.blocks[0];

        assert_eq!(&block0[0x18..0x20], &desc.epoch_id.to_le_bytes());
        assert_eq!(&block0[0x20..0x22], &desc.k.to_le_bytes());
        assert_eq!(&block0[0x22..0x24], &desc.m.to_le_bytes());
        assert_eq!(&block0[0x24..0x28], &desc.stripes_per_epoch.to_le_bytes());
        assert_eq!(&block0[0x28..0x2C], &desc.block_size.to_le_bytes());
        assert_eq!(
            &block0[0x30..0x38],
            &desc.protected_ordinal_start.to_le_bytes()
        );
        assert_eq!(
            &block0[0x38..0x40],
            &desc.protected_ordinal_end_exclusive.to_le_bytes()
        );
    }

    #[test]
    fn block0_crc_covers_inline_index_entries() {
        let desc = descriptor(512);
        let index = index_for(&desc);
        let mut encoded = encode_sidecar_index_blocks(&desc, &index).expect("encode sidecar");
        encoded.blocks[0][SIDECAR_HEADER_LEN] ^= 0x01;

        let err = parse_sidecar_header_block(&encoded.blocks[0], &desc.tape_uuid).unwrap_err();
        match err {
            ParityError::SidecarParse(msg) => assert!(msg.contains("block0 CRC"), "{msg}"),
            other => panic!("expected sidecar parse error, got {other:?}"),
        }
    }

    #[test]
    fn header_crc_covers_fixed_header() {
        let desc = descriptor(512);
        let index = index_for(&desc);
        let mut encoded = encode_sidecar_index_blocks(&desc, &index).expect("encode sidecar");
        encoded.blocks[0][0x30] ^= 0x01;
        let crc_offset = encoded.blocks[0].len() - 8;
        let block0_crc = crc64_xz(&encoded.blocks[0][..crc_offset]);
        encoded.blocks[0][crc_offset..].copy_from_slice(&block0_crc.to_le_bytes());

        let err = parse_sidecar_header_block(&encoded.blocks[0], &desc.tape_uuid).unwrap_err();
        match err {
            ParityError::SidecarParse(msg) => assert!(msg.contains("header CRC"), "{msg}"),
            other => panic!("expected sidecar parse error, got {other:?}"),
        }
    }

    #[test]
    fn spill_index_block_crc_is_validated() {
        let desc = descriptor(256);
        let index = index_for(&desc);
        let mut encoded = encode_sidecar_index_blocks(&desc, &index).expect("encode sidecar");
        assert!(encoded.blocks.len() > 1);
        encoded.blocks[1][0] ^= 0x01;

        let err = parse_sidecar_index_blocks(&encoded.blocks, &desc.tape_uuid).unwrap_err();
        match err {
            ParityError::SidecarParse(msg) => assert!(msg.contains("index block 1 CRC"), "{msg}"),
            other => panic!("expected sidecar parse error, got {other:?}"),
        }
    }

    #[test]
    fn parser_rejects_wrong_tape_magic_before_classifying_sidecar() {
        let desc = descriptor(512);
        let index = index_for(&desc);
        let encoded = encode_sidecar_index_blocks(&desc, &index).expect("encode sidecar");
        let mut other_uuid = desc.tape_uuid;
        other_uuid[0] ^= 0x01;

        let err = parse_sidecar_header_block(&encoded.blocks[0], &other_uuid).unwrap_err();
        match err {
            ParityError::SidecarParse(msg) => assert!(msg.contains("magic"), "{msg}"),
            other => panic!("expected sidecar parse error, got {other:?}"),
        }
    }

    #[test]
    fn classifier_returns_none_for_non_sidecar_magic() {
        let uuid = sample_uuid();
        let block0 = vec![0xA5; 512];

        let classified =
            classify_sidecar_header_block(&block0, &uuid).expect("classify non-sidecar block");
        assert_eq!(classified, None);
    }

    #[test]
    fn classifier_errors_for_matching_magic_with_invalid_header() {
        let desc = descriptor(512);
        let mut block0 = vec![0u8; desc.block_size as usize];
        block0[0x00..0x08].copy_from_slice(&derive_sidecar_magic(&desc.tape_uuid));
        block0[0x08..0x18].copy_from_slice(&desc.tape_uuid);
        block0[0x80..0x82].copy_from_slice(&SidecarCopyKind::Primary.to_u16().to_le_bytes());

        let err = classify_sidecar_header_block(&block0, &desc.tape_uuid)
            .expect_err("matching sidecar magic must force strict validation");
        match err {
            ParityError::SidecarParse(msg) => {
                assert!(msg.contains("unsupported sidecar schema version"), "{msg}")
            }
            other => panic!("expected sidecar parse error, got {other:?}"),
        }
    }

    #[test]
    fn parser_rejects_nonzero_parity_index_reserved_field() {
        let desc = descriptor(512);
        let index = index_for(&desc);
        let mut encoded = encode_sidecar_index_blocks(&desc, &index).expect("encode sidecar");
        let reserved_offset = SIDECAR_HEADER_LEN + 6;
        encoded.blocks[0][reserved_offset] = 0x01;
        let crc_offset = encoded.blocks[0].len() - 8;
        let block0_crc = crc64_xz(&encoded.blocks[0][..crc_offset]);
        encoded.blocks[0][crc_offset..].copy_from_slice(&block0_crc.to_le_bytes());

        let err = parse_sidecar_index_blocks(&encoded.blocks, &desc.tape_uuid).unwrap_err();
        match err {
            ParityError::SidecarParse(msg) => assert!(msg.contains("reserved"), "{msg}"),
            other => panic!("expected sidecar parse error, got {other:?}"),
        }
    }

    #[test]
    fn multi_block_index_spill_preserves_entry_boundaries() {
        let mut desc = descriptor(256);
        desc.k = 5;
        desc.m = 2;
        desc.stripes_per_epoch = 2;
        desc.protected_ordinal_start = 0;
        desc.protected_ordinal_end_exclusive = 10;
        let index = index_for(&desc);

        let encoded = encode_sidecar_index_blocks(&desc, &index).expect("encode sidecar");
        assert!(encoded.blocks.len() > 1);
        assert_eq!(encoded.header.inline_index_entry_bytes % 8, 0);
        assert_eq!(
            encoded.header.shard_index_block_count as usize,
            encoded.blocks.len()
        );

        let decoded =
            parse_sidecar_index_blocks(&encoded.blocks, &desc.tape_uuid).expect("parse sidecar");
        assert_eq!(decoded.index.parity_entries, index.parity_entries);
        assert_eq!(decoded.index.data_shard_crc64s, index.data_shard_crc64s);
    }

    #[test]
    fn default_geometry_index_layout_matches_design_sizing() {
        let block_size = 256 * 1024;
        let scheme = crate::default_scheme_for_block_size(block_size);
        let desc = descriptor_for_scheme(block_size, &scheme, 42);
        let index = metadata_only_index_for(&desc);

        let encoded =
            encode_sidecar_index_blocks(&desc, &index).expect("encode default sidecar index");
        assert_eq!(encoded.header.k, 128);
        assert_eq!(encoded.header.m, 4);
        assert_eq!(encoded.header.stripes_per_epoch, 512);
        assert_eq!(encoded.header.logical_shard_count, 65_536);
        assert_eq!(encoded.header.real_data_shard_count, 65_536);
        assert_eq!(encoded.header.parity_block_count, 2_048);
        assert_eq!(encoded.header.data_crc_count, 65_536);
        assert_eq!(
            encoded.header.shard_index_block_count, 3,
            "default 256 KiB geometry should spill the ~544 KiB index into H=3 blocks"
        );

        let block0_index_capacity = block_size as usize - 8 - SIDECAR_HEADER_LEN;
        assert_eq!(
            encoded.header.inline_index_entry_bytes as usize,
            block0_index_capacity
        );
        assert_eq!(
            encoded.header.inline_index_entry_bytes % DATA_CRC_ENTRY_LEN as u32,
            0
        );
        assert_eq!(encoded.blocks.len(), 3);

        let parity_index_bytes = 2_048 * PARITY_INDEX_ENTRY_LEN;
        let inline_data_crc_count = (encoded.header.inline_index_entry_bytes as usize
            - parity_index_bytes)
            / DATA_CRC_ENTRY_LEN;
        let first_spill_data_crc_count = (block_size as usize - 8) / DATA_CRC_ENTRY_LEN;
        let final_spill_data_crc_count =
            65_536 - inline_data_crc_count - first_spill_data_crc_count;
        assert_eq!(inline_data_crc_count, 28_648);
        assert_eq!(first_spill_data_crc_count, 32_767);
        assert_eq!(final_spill_data_crc_count, 4_121);

        let final_payload_bytes = final_spill_data_crc_count * DATA_CRC_ENTRY_LEN;
        assert!(
            encoded.blocks[2][final_payload_bytes..block_size as usize - 8]
                .iter()
                .all(|byte| *byte == 0),
            "unused bytes in the final spill block must be zero-filled under the trailing CRC"
        );

        let decoded = parse_sidecar_index_blocks(&encoded.blocks, &desc.tape_uuid)
            .expect("parse default sidecar index");
        assert_eq!(decoded.header, encoded.header);
        assert_eq!(decoded.index, index);
    }

    #[test]
    fn scaled_scheme_index_layouts_match_design_sizing() {
        struct Case {
            name: &'static str,
            block_size: u32,
            scheme: crate::ParityScheme,
            expected_k: u16,
            expected_m: u16,
            expected_stripes: u32,
            expected_data_crcs: u64,
            expected_parity_entries: u32,
            expected_index_blocks: u32,
            expected_inline_index_bytes: usize,
        }

        let cases = [
            Case {
                name: "default 512 KiB",
                block_size: 512 * 1024,
                scheme: crate::default_scheme_for_block_size(512 * 1024),
                expected_k: 128,
                expected_m: 4,
                expected_stripes: 256,
                expected_data_crcs: 32_768,
                expected_parity_entries: 1_024,
                expected_index_blocks: 1,
                expected_inline_index_bytes: 278_528,
            },
            Case {
                name: "default 1 MiB",
                block_size: 1024 * 1024,
                scheme: crate::default_scheme_for_block_size(1024 * 1024),
                expected_k: 128,
                expected_m: 4,
                expected_stripes: 128,
                expected_data_crcs: 16_384,
                expected_parity_entries: 512,
                expected_index_blocks: 1,
                expected_inline_index_bytes: 139_264,
            },
            Case {
                name: "conservative 256 KiB",
                block_size: 256 * 1024,
                scheme: crate::conservative_scheme_for_block_size(256 * 1024),
                expected_k: 64,
                expected_m: 6,
                expected_stripes: 256,
                expected_data_crcs: 16_384,
                expected_parity_entries: 1_536,
                expected_index_blocks: 1,
                expected_inline_index_bytes: 155_648,
            },
        ];

        for (case_index, case) in cases.into_iter().enumerate() {
            let desc = descriptor_for_scheme(case.block_size, &case.scheme, case_index as u64);
            let index = metadata_only_index_for(&desc);
            let encoded = encode_sidecar_index_blocks(&desc, &index)
                .unwrap_or_else(|err| panic!("encode {} sidecar index: {err}", case.name));

            assert_eq!(encoded.header.k, case.expected_k, "{}", case.name);
            assert_eq!(encoded.header.m, case.expected_m, "{}", case.name);
            assert_eq!(
                encoded.header.stripes_per_epoch, case.expected_stripes,
                "{}",
                case.name
            );
            assert_eq!(
                encoded.header.logical_shard_count, case.expected_data_crcs,
                "{}",
                case.name
            );
            assert_eq!(
                encoded.header.real_data_shard_count, case.expected_data_crcs,
                "{}",
                case.name
            );
            assert_eq!(
                encoded.header.parity_block_count, case.expected_parity_entries,
                "{}",
                case.name
            );
            assert_eq!(
                encoded.header.data_crc_count, case.expected_data_crcs as u32,
                "{}",
                case.name
            );
            assert_eq!(
                encoded.header.shard_index_block_count, case.expected_index_blocks,
                "{}",
                case.name
            );

            let expected_index_bytes = case.expected_parity_entries as usize
                * PARITY_INDEX_ENTRY_LEN
                + case.expected_data_crcs as usize * DATA_CRC_ENTRY_LEN;
            assert_eq!(
                expected_index_bytes, case.expected_inline_index_bytes,
                "{}",
                case.name
            );
            assert_eq!(
                encoded.header.inline_index_entry_bytes as usize, case.expected_inline_index_bytes,
                "{}",
                case.name
            );
            assert_eq!(
                encoded.blocks.len(),
                case.expected_index_blocks as usize,
                "{}",
                case.name
            );

            let block_size = case.block_size as usize;
            assert!(
                encoded.blocks[0]
                    [SIDECAR_HEADER_LEN + case.expected_inline_index_bytes..block_size - 8]
                    .iter()
                    .all(|byte| *byte == 0),
                "{} leaves deterministic zero-fill after the final inline entry",
                case.name
            );

            let decoded = parse_sidecar_index_blocks(&encoded.blocks, &desc.tape_uuid)
                .unwrap_or_else(|err| panic!("parse {} sidecar index: {err}", case.name));
            assert_eq!(decoded.header, encoded.header, "{}", case.name);
            assert_eq!(decoded.index, index, "{}", case.name);
        }
    }

    #[test]
    fn parser_rejects_corrupt_raw_parity_shard() {
        let desc = descriptor(256);
        let parity_shards = parity_shards_for(&desc);
        let data_crcs = data_crc64s_for(&desc);
        let mut encoded = encode_sidecar_tape_file(&desc, &parity_shards, data_crcs)
            .expect("encode full sidecar");
        let first_parity_block = encoded.header.shard_index_block_count as usize;
        encoded.blocks[first_parity_block][0] ^= 0x01;

        let err = parse_sidecar_tape_file(&encoded.blocks, &desc.tape_uuid).unwrap_err();
        match err {
            ParityError::SidecarParse(msg) => {
                assert!(msg.contains("parity shard 0"), "{msg}");
                assert!(msg.contains("CRC mismatch"), "{msg}");
            }
            other => panic!("expected sidecar parse error, got {other:?}"),
        }
    }

    #[test]
    fn parser_rejects_sidecar_file_block_count_mismatch() {
        let desc = descriptor(256);
        let parity_shards = parity_shards_for(&desc);
        let data_crcs = data_crc64s_for(&desc);
        let mut encoded = encode_sidecar_tape_file(&desc, &parity_shards, data_crcs)
            .expect("encode full sidecar");
        let footer = encoded.blocks.pop().expect("encoded sidecar has footer");
        encoded.blocks.push(vec![0u8; desc.block_size as usize]);
        encoded.blocks.push(footer);

        let err = parse_sidecar_tape_file(&encoded.blocks, &desc.tape_uuid).unwrap_err();
        match err {
            ParityError::SidecarParse(msg) => {
                assert!(msg.contains("expected exactly"), "{msg}");
            }
            other => panic!("expected sidecar parse error, got {other:?}"),
        }
    }
}
