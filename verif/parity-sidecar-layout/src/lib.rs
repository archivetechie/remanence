//! Verification extraction of the Remanence parity sidecar binary layout.
//!
//! This crate is a standalone, dependency-free model of the fixed byte offsets,
//! footer locator layout, sidecar tape-file block placement, and CRC input
//! windows in `crates/remanence-parity/src/sidecar.rs`. It intentionally omits
//! HMAC, SHA-256, CRC algebra, allocation, slice copying, tape IO, and
//! Reed-Solomon recovery; those remain outside this proof target. The
//! `drift_guard` test pins the production snippets this extraction mirrors.

pub const SIDECAR_HEADER_LEN: u64 = 0xB8;
pub const SIDECAR_HEADER_CRC_OFFSET: u64 = 0xB0;
pub const SIDECAR_FOOTER_LEN: u64 = 0x80;
pub const SIDECAR_FOOTER_CRC_OFFSET: u64 = 0x78;
pub const PARITY_INDEX_ENTRY_LEN: u64 = 16;
pub const DATA_CRC_ENTRY_LEN: u64 = 8;
pub const TRAILING_CRC_LEN: u64 = 8;

pub const MIN_HEADER_BLOCK_SIZE: u64 = 0xC0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutError {
    BlockTooSmall,
    HeaderBlockCountZero,
    ArithmeticOverflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeaderBlockLayout {
    pub magic: ByteRange,
    pub tape_uuid: ByteRange,
    pub epoch_id: ByteRange,
    pub k: ByteRange,
    pub m: ByteRange,
    pub stripes_per_epoch: ByteRange,
    pub block_size: ByteRange,
    pub schema_version: ByteRange,
    pub protected_ordinal_start: ByteRange,
    pub protected_ordinal_end_exclusive: ByteRange,
    pub logical_shard_count: ByteRange,
    pub real_data_shard_count: ByteRange,
    pub parity_block_count: ByteRange,
    pub data_crc_count: ByteRange,
    pub shard_index_block_count: ByteRange,
    pub inline_index_entry_bytes: ByteRange,
    pub sidecar_total_block_count: ByteRange,
    pub primary_header_start_block: ByteRange,
    pub tail_header_start_block: ByteRange,
    pub footer_block_index: ByteRange,
    pub copy_kind: ByteRange,
    pub copy_kind_reserved: ByteRange,
    pub copy_generation: ByteRange,
    pub canonical_metadata_hash: ByteRange,
    pub header_reserved: ByteRange,
    pub header_crc_field: ByteRange,
    pub inline_index_payload: ByteRange,
    pub header_crc_input: ByteRange,
    pub block0_crc_input: ByteRange,
    pub block0_crc_field: ByteRange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FooterBlockLayout {
    pub magic: ByteRange,
    pub sidecar_footer_version: ByteRange,
    pub reserved16: ByteRange,
    pub reserved32: ByteRange,
    pub tape_uuid: ByteRange,
    pub epoch_id: ByteRange,
    pub protected_ordinal_start: ByteRange,
    pub protected_ordinal_end_exclusive: ByteRange,
    pub sidecar_header_block_count: ByteRange,
    pub parity_shard_block_count: ByteRange,
    pub sidecar_total_block_count: ByteRange,
    pub primary_header_start_block: ByteRange,
    pub tail_header_start_block: ByteRange,
    pub canonical_metadata_hash: ByteRange,
    pub footer_crc_field: ByteRange,
    pub footer_crc_input: ByteRange,
    pub footer_padding: ByteRange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpillBlockLayout {
    pub index_payload: ByteRange,
    pub trailing_crc_input: ByteRange,
    pub trailing_crc_field: ByteRange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SidecarTapeFileLayout {
    pub primary_header_copy: ByteRange,
    pub parity_shards: ByteRange,
    pub tail_header_copy: ByteRange,
    pub footer_block_index: u64,
    pub sidecar_total_block_count: u64,
}

pub fn range(start: u64, end: u64) -> ByteRange {
    ByteRange { start, end }
}

pub fn checked_add(a: u64, b: u64) -> Result<u64, LayoutError> {
    match a.checked_add(b) {
        Some(sum) => Ok(sum),
        None => Err(LayoutError::ArithmeticOverflow),
    }
}

pub fn checked_sub(a: u64, b: u64) -> Result<u64, LayoutError> {
    match a.checked_sub(b) {
        Some(diff) => Ok(diff),
        None => Err(LayoutError::ArithmeticOverflow),
    }
}

pub fn header_block_layout(block_size: u64) -> Result<HeaderBlockLayout, LayoutError> {
    if block_size < MIN_HEADER_BLOCK_SIZE {
        return Err(LayoutError::BlockTooSmall);
    }

    let block_crc_start = checked_sub(block_size, TRAILING_CRC_LEN)?;

    Ok(HeaderBlockLayout {
        magic: range(0x00, 0x08),
        tape_uuid: range(0x08, 0x18),
        epoch_id: range(0x18, 0x20),
        k: range(0x20, 0x22),
        m: range(0x22, 0x24),
        stripes_per_epoch: range(0x24, 0x28),
        block_size: range(0x28, 0x2C),
        schema_version: range(0x2C, 0x30),
        protected_ordinal_start: range(0x30, 0x38),
        protected_ordinal_end_exclusive: range(0x38, 0x40),
        logical_shard_count: range(0x40, 0x48),
        real_data_shard_count: range(0x48, 0x50),
        parity_block_count: range(0x50, 0x54),
        data_crc_count: range(0x54, 0x58),
        shard_index_block_count: range(0x58, 0x5C),
        inline_index_entry_bytes: range(0x5C, 0x60),
        sidecar_total_block_count: range(0x60, 0x68),
        primary_header_start_block: range(0x68, 0x70),
        tail_header_start_block: range(0x70, 0x78),
        footer_block_index: range(0x78, 0x80),
        copy_kind: range(0x80, 0x82),
        copy_kind_reserved: range(0x82, 0x84),
        copy_generation: range(0x84, 0x88),
        canonical_metadata_hash: range(0x88, 0xA8),
        header_reserved: range(0xA8, SIDECAR_HEADER_CRC_OFFSET),
        header_crc_field: range(SIDECAR_HEADER_CRC_OFFSET, SIDECAR_HEADER_LEN),
        inline_index_payload: range(SIDECAR_HEADER_LEN, block_crc_start),
        header_crc_input: range(0, SIDECAR_HEADER_CRC_OFFSET),
        block0_crc_input: range(0, block_crc_start),
        block0_crc_field: range(block_crc_start, block_size),
    })
}

pub fn footer_block_layout(block_size: u64) -> Result<FooterBlockLayout, LayoutError> {
    if block_size < SIDECAR_FOOTER_LEN {
        return Err(LayoutError::BlockTooSmall);
    }

    Ok(FooterBlockLayout {
        magic: range(0x00, 0x08),
        sidecar_footer_version: range(0x08, 0x0A),
        reserved16: range(0x0A, 0x0C),
        reserved32: range(0x0C, 0x10),
        tape_uuid: range(0x10, 0x20),
        epoch_id: range(0x20, 0x28),
        protected_ordinal_start: range(0x28, 0x30),
        protected_ordinal_end_exclusive: range(0x30, 0x38),
        sidecar_header_block_count: range(0x38, 0x3C),
        parity_shard_block_count: range(0x3C, 0x40),
        sidecar_total_block_count: range(0x40, 0x48),
        primary_header_start_block: range(0x48, 0x50),
        tail_header_start_block: range(0x50, 0x58),
        canonical_metadata_hash: range(0x58, SIDECAR_FOOTER_CRC_OFFSET),
        footer_crc_field: range(SIDECAR_FOOTER_CRC_OFFSET, SIDECAR_FOOTER_LEN),
        footer_crc_input: range(0, SIDECAR_FOOTER_CRC_OFFSET),
        footer_padding: range(SIDECAR_FOOTER_LEN, block_size),
    })
}

pub fn spill_block_layout(block_size: u64) -> Result<SpillBlockLayout, LayoutError> {
    if block_size < TRAILING_CRC_LEN {
        return Err(LayoutError::BlockTooSmall);
    }

    let crc_start = checked_sub(block_size, TRAILING_CRC_LEN)?;
    Ok(SpillBlockLayout {
        index_payload: range(0, crc_start),
        trailing_crc_input: range(0, crc_start),
        trailing_crc_field: range(crc_start, block_size),
    })
}

pub fn sidecar_tape_file_layout(
    shard_index_block_count: u64,
    parity_block_count: u64,
) -> Result<SidecarTapeFileLayout, LayoutError> {
    if shard_index_block_count == 0 {
        return Err(LayoutError::HeaderBlockCountZero);
    }

    let primary_start = 0;
    let primary_end = shard_index_block_count;
    let parity_start = primary_end;
    let parity_end = checked_add(parity_start, parity_block_count)?;
    let tail_start = parity_end;
    let tail_end = checked_add(tail_start, shard_index_block_count)?;
    let footer_index = tail_end;
    let total = checked_add(footer_index, 1)?;

    Ok(SidecarTapeFileLayout {
        primary_header_copy: range(primary_start, primary_end),
        parity_shards: range(parity_start, parity_end),
        tail_header_copy: range(tail_start, tail_end),
        footer_block_index: footer_index,
        sidecar_total_block_count: total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drift_guard() {
        let this_file = include_str!("lib.rs");
        let original = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/remanence-parity/src/sidecar.rs"
        ))
        .expect("original sidecar.rs must be readable from verif/parity-sidecar-layout");

        let snippets: &[&str] = &[
            "pub const SIDECAR_HEADER_LEN: usize = 0xB8;",
            "pub const SIDECAR_HEADER_CRC_OFFSET: usize = 0xB0;",
            "pub const SIDECAR_FOOTER_LEN: usize = 0x80;",
            "pub const SIDECAR_FOOTER_CRC_OFFSET: usize = 0x78;",
            "block0[0x00..0x08].copy_from_slice(&header.magic);",
            "block0[0x08..0x18].copy_from_slice(&header.tape_uuid);",
            "block0[0x18..0x20].copy_from_slice(&header.epoch_id.to_le_bytes());",
            "block0[0x20..0x22].copy_from_slice(&header.k.to_le_bytes());",
            "block0[0x22..0x24].copy_from_slice(&header.m.to_le_bytes());",
            "block0[0x24..0x28].copy_from_slice(&header.stripes_per_epoch.to_le_bytes());",
            "block0[0x28..0x2C].copy_from_slice(&header.block_size.to_le_bytes());",
            "block0[0x2C..0x30].copy_from_slice(&header.schema_version.to_le_bytes());",
            "block0[0x30..0x38].copy_from_slice(&header.protected_ordinal_start.to_le_bytes());",
            "block0[0x38..0x40].copy_from_slice(&header.protected_ordinal_end_exclusive.to_le_bytes());",
            "block0[0x40..0x48].copy_from_slice(&header.logical_shard_count.to_le_bytes());",
            "block0[0x48..0x50].copy_from_slice(&header.real_data_shard_count.to_le_bytes());",
            "block0[0x50..0x54].copy_from_slice(&header.parity_block_count.to_le_bytes());",
            "block0[0x54..0x58].copy_from_slice(&header.data_crc_count.to_le_bytes());",
            "block0[0x58..0x5C].copy_from_slice(&header.shard_index_block_count.to_le_bytes());",
            "block0[0x5C..0x60].copy_from_slice(&header.inline_index_entry_bytes.to_le_bytes());",
            "block0[0x60..0x68].copy_from_slice(&header.sidecar_total_block_count.to_le_bytes());",
            "block0[0x68..0x70].copy_from_slice(&header.primary_header_start_block.to_le_bytes());",
            "block0[0x70..0x78].copy_from_slice(&header.tail_header_start_block.to_le_bytes());",
            "block0[0x78..0x80].copy_from_slice(&header.footer_block_index.to_le_bytes());",
            "block0[0x80..0x82].copy_from_slice(&header.copy_kind.to_u16().to_le_bytes());",
            "block0[0x82..0x84].copy_from_slice(&0u16.to_le_bytes());",
            "block0[0x84..0x88].copy_from_slice(&header.copy_generation.to_le_bytes());",
            "block0[0x88..0xA8].copy_from_slice(&header.canonical_metadata_hash);",
            "block0[0xA8..0xB0].copy_from_slice(&0u64.to_le_bytes());",
            "header.header_crc64 = crc64_xz(&block0[..SIDECAR_HEADER_CRC_OFFSET]);",
            "let crc_offset = block0.len() - 8;\n    header.block0_crc64 = crc64_xz(&block0[..crc_offset]);",
            "if block0.len() < SIDECAR_HEADER_LEN + 8 {",
            "magic.copy_from_slice(&block0[0x00..0x08]);",
            "tape_uuid.copy_from_slice(&block0[0x08..0x18]);",
            "let epoch_id = read_u64_le(block0, 0x18);",
            "let k = read_u16_le(block0, 0x20);",
            "let m = read_u16_le(block0, 0x22);",
            "let stripes_per_epoch = read_u32_le(block0, 0x24);",
            "let block_size = read_u32_le(block0, 0x28);",
            "let schema_version = read_u32_le(block0, 0x2C);",
            "let protected_ordinal_start = read_u64_le(block0, 0x30);",
            "let protected_ordinal_end_exclusive = read_u64_le(block0, 0x38);",
            "let logical_shard_count = read_u64_le(block0, 0x40);",
            "let real_data_shard_count = read_u64_le(block0, 0x48);",
            "let parity_block_count = read_u32_le(block0, 0x50);",
            "let data_crc_count = read_u32_le(block0, 0x54);",
            "let shard_index_block_count = read_u32_le(block0, 0x58);",
            "let inline_index_entry_bytes = read_u32_le(block0, 0x5C);",
            "let sidecar_total_block_count = read_u64_le(block0, 0x60);",
            "let primary_header_start_block = read_u64_le(block0, 0x68);",
            "let tail_header_start_block = read_u64_le(block0, 0x70);",
            "let footer_block_index = read_u64_le(block0, 0x78);",
            "let copy_kind = SidecarCopyKind::from_u16(read_u16_le(block0, 0x80))?;",
            "let copy_kind_reserved = read_u16_le(block0, 0x82);",
            "let copy_generation = read_u32_le(block0, 0x84);",
            "canonical_metadata_hash.copy_from_slice(&block0[0x88..0xA8]);",
            "let header_reserved = read_u64_le(block0, 0xA8);",
            "let header_crc64 = read_u64_le(block0, SIDECAR_HEADER_CRC_OFFSET);",
            "let computed_header_crc64 = crc64_xz(&block0[..SIDECAR_HEADER_CRC_OFFSET]);",
            "let block0_crc64 = read_u64_le(block0, crc_offset);",
            "let computed_block0_crc64 = crc64_xz(&block0[..crc_offset]);",
            "let inline_end = SIDECAR_HEADER_LEN + inline_index_entry_bytes as usize;",
            "block[0x00..0x08].copy_from_slice(&derive_sidecar_footer_magic(&header.tape_uuid));",
            "block[0x08..0x0A].copy_from_slice(&SIDECAR_FOOTER_VERSION.to_le_bytes());",
            "block[0x0A..0x0C].copy_from_slice(&0u16.to_le_bytes());",
            "block[0x0C..0x10].copy_from_slice(&0u32.to_le_bytes());",
            "block[0x10..0x20].copy_from_slice(&header.tape_uuid);",
            "block[0x20..0x28].copy_from_slice(&header.epoch_id.to_le_bytes());",
            "block[0x28..0x30].copy_from_slice(&header.protected_ordinal_start.to_le_bytes());",
            "block[0x30..0x38].copy_from_slice(&header.protected_ordinal_end_exclusive.to_le_bytes());",
            "block[0x38..0x3C].copy_from_slice(&header.shard_index_block_count.to_le_bytes());",
            "block[0x3C..0x40].copy_from_slice(&header.parity_block_count.to_le_bytes());",
            "block[0x40..0x48].copy_from_slice(&header.sidecar_total_block_count.to_le_bytes());",
            "block[0x48..0x50].copy_from_slice(&header.primary_header_start_block.to_le_bytes());",
            "block[0x50..0x58].copy_from_slice(&header.tail_header_start_block.to_le_bytes());",
            "block[0x58..0x78].copy_from_slice(&header.canonical_metadata_hash);",
            "let crc = crc64_xz(&block[..SIDECAR_FOOTER_CRC_OFFSET]);",
            "if footer_block.len() < SIDECAR_FOOTER_LEN {",
            "magic.copy_from_slice(&footer_block[0x00..0x08]);",
            "let sidecar_footer_version = read_u16_le(footer_block, 0x08);",
            "let footer_reserved16 = read_u16_le(footer_block, 0x0A);",
            "let footer_reserved32 = read_u32_le(footer_block, 0x0C);",
            "tape_uuid.copy_from_slice(&footer_block[0x10..0x20]);",
            "let epoch_id = read_u64_le(footer_block, 0x20);",
            "let protected_ordinal_start = read_u64_le(footer_block, 0x28);",
            "let protected_ordinal_end_exclusive = read_u64_le(footer_block, 0x30);",
            "let sidecar_header_block_count = read_u32_le(footer_block, 0x38);",
            "let parity_shard_block_count = read_u32_le(footer_block, 0x3C);",
            "let sidecar_total_block_count = read_u64_le(footer_block, 0x40);",
            "let primary_header_start_block = read_u64_le(footer_block, 0x48);",
            "let tail_header_start_block = read_u64_le(footer_block, 0x50);",
            "canonical_metadata_hash.copy_from_slice(&footer_block[0x58..0x78]);",
            "let footer_crc64 = read_u64_le(footer_block, SIDECAR_FOOTER_CRC_OFFSET);",
            "let computed = crc64_xz(&footer_block[..SIDECAR_FOOTER_CRC_OFFSET]);",
            "let expected_tail = u64::from(shard_index_block_count)\n        .checked_add(u64::from(parity_block_count))",
            "let expected_footer = expected_tail\n        .checked_add(u64::from(shard_index_block_count))",
            "let expected_total = expected_footer\n        .checked_add(1)",
        ];
        for (i, snippet) in snippets.iter().enumerate() {
            assert!(
                original.contains(snippet),
                "snippet {i} no longer in remanence-parity sidecar.rs -- original \
                 changed; re-sync this extraction and its Lean proofs"
            );
        }

        let extraction_snippets: &[&str] = &[
            "pub fn header_block_layout(block_size: u64) -> Result<HeaderBlockLayout, LayoutError>",
            "pub fn footer_block_layout(block_size: u64) -> Result<FooterBlockLayout, LayoutError>",
            "pub fn spill_block_layout(block_size: u64) -> Result<SpillBlockLayout, LayoutError>",
            "pub fn sidecar_tape_file_layout(",
            "header_crc_input: range(0, SIDECAR_HEADER_CRC_OFFSET)",
            "block0_crc_input: range(0, block_crc_start)",
            "footer_crc_input: range(0, SIDECAR_FOOTER_CRC_OFFSET)",
            "let total = checked_add(footer_index, 1)?;",
        ];
        for (i, snippet) in extraction_snippets.iter().enumerate() {
            assert!(
                this_file.contains(snippet),
                "extraction snippet {i} missing from verif sidecar-layout model"
            );
        }
    }

    #[test]
    fn header_layout_ranges_match_sidecar_constants() {
        let layout = header_block_layout(0x200).expect("valid header block size");
        assert_eq!(layout.magic, range(0x00, 0x08));
        assert_eq!(layout.canonical_metadata_hash, range(0x88, 0xA8));
        assert_eq!(layout.header_crc_input, range(0, SIDECAR_HEADER_CRC_OFFSET));
        assert_eq!(
            layout.header_crc_field,
            range(SIDECAR_HEADER_CRC_OFFSET, SIDECAR_HEADER_LEN)
        );
        assert_eq!(
            layout.inline_index_payload,
            range(SIDECAR_HEADER_LEN, 0x1F8)
        );
        assert_eq!(layout.block0_crc_input, range(0, 0x1F8));
        assert_eq!(layout.block0_crc_field, range(0x1F8, 0x200));
    }

    #[test]
    fn footer_layout_ranges_match_sidecar_constants() {
        let layout = footer_block_layout(0x200).expect("valid footer block size");
        assert_eq!(layout.magic, range(0x00, 0x08));
        assert_eq!(layout.canonical_metadata_hash, range(0x58, 0x78));
        assert_eq!(layout.footer_crc_input, range(0, SIDECAR_FOOTER_CRC_OFFSET));
        assert_eq!(
            layout.footer_crc_field,
            range(SIDECAR_FOOTER_CRC_OFFSET, SIDECAR_FOOTER_LEN)
        );
        assert_eq!(layout.footer_padding, range(SIDECAR_FOOTER_LEN, 0x200));
    }

    #[test]
    fn sidecar_file_layout_matches_header_parity_tail_footer_formula() {
        let layout = sidecar_tape_file_layout(3, 2048).expect("valid file layout");
        assert_eq!(layout.primary_header_copy, range(0, 3));
        assert_eq!(layout.parity_shards, range(3, 2051));
        assert_eq!(layout.tail_header_copy, range(2051, 2054));
        assert_eq!(layout.footer_block_index, 2054);
        assert_eq!(layout.sidecar_total_block_count, 2055);
    }
}
