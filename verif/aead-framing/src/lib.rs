//! Verification extraction of the RAO AEAD framing arithmetic.
//!
//! This crate is a standalone, dependency-free model of the pure arithmetic in
//! `crates/remanence-aead/src/{stream,range,inspect}.rs`: chunk counts,
//! payload-frame lengths, stored-size rounding, ciphertext offsets, plaintext
//! range planning, and keyless inspection geometry. It deliberately excludes
//! encryption, hashing, CBOR, allocation, and byte I/O. The `drift_guard` test
//! pins the production formulas this extraction mirrors; if it fails, the
//! extraction and Lean proofs must be re-synced.

pub const RAO_HEADER_LEN: u64 = 128;
pub const RAO_FOOTER_LEN: u64 = 16;
pub const CHACHA20POLY1305_TAG_LEN: u64 = 16;
pub const CHUNK_SIZE_GRANULARITY: u64 = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AeadFrameError {
    InvalidChunkSize,
    InvalidMetadataField,
    SizeOverflow,
    PlaintextRangeOverflow,
    EmptyRangeStartsPastEnd,
    PlaintextRangePastEnd,
    UnexpectedEof,
    TrailingData,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RangePlan {
    pub plaintext_end: u64,
    pub first_chunk: u64,
    pub last_chunk: u64,
    pub fetched_chunk_count: u64,
    pub stored_range_start: u64,
    pub stored_range_len: u64,
    pub trim_start: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InspectGeometry {
    pub stride: u64,
    pub numerator: u64,
    pub chunk_count: u64,
    pub plaintext_size: u64,
    pub footer_offset: u64,
    pub expected_stored_size: u64,
    pub fill_len: u64,
}

pub fn checked_add(a: u64, b: u64) -> Result<u64, AeadFrameError> {
    match a.checked_add(b) {
        Some(sum) => Ok(sum),
        None => Err(AeadFrameError::SizeOverflow),
    }
}

pub fn checked_sub(a: u64, b: u64) -> Result<u64, AeadFrameError> {
    match a.checked_sub(b) {
        Some(diff) => Ok(diff),
        None => Err(AeadFrameError::SizeOverflow),
    }
}

pub fn checked_mul(a: u64, b: u64) -> Result<u64, AeadFrameError> {
    match a.checked_mul(b) {
        Some(product) => Ok(product),
        None => Err(AeadFrameError::SizeOverflow),
    }
}

pub fn validate_chunk_size(chunk_size: u64) -> Result<(), AeadFrameError> {
    if chunk_size == 0 || chunk_size % CHUNK_SIZE_GRANULARITY != 0 {
        return Err(AeadFrameError::InvalidChunkSize);
    }
    Ok(())
}

pub fn chunk_count(plaintext_size: u64, chunk_size: u64) -> Result<u64, AeadFrameError> {
    if chunk_size == 0 || plaintext_size == 0 || plaintext_size % chunk_size != 0 {
        return Err(AeadFrameError::InvalidMetadataField);
    }
    Ok(plaintext_size / chunk_size)
}

pub fn payload_frame_len(plaintext_size: u64, chunk_size: u64) -> Result<u64, AeadFrameError> {
    let chunks = chunk_count(plaintext_size, chunk_size)?;
    let tag_bytes = checked_mul(CHACHA20POLY1305_TAG_LEN, chunks)?;
    checked_add(plaintext_size, tag_bytes)
}

pub fn round_up(value: u64, multiple: u64) -> Result<u64, AeadFrameError> {
    if multiple == 0 {
        return Err(AeadFrameError::SizeOverflow);
    }
    let remainder = value % multiple;
    if remainder == 0 {
        Ok(value)
    } else {
        checked_add(value, multiple - remainder)
    }
}

pub fn stored_size_from_parts(
    chunk_size: u64,
    metadata_frame_len: u64,
    plaintext_size: u64,
) -> Result<u64, AeadFrameError> {
    stored_size_from_parts_with_prefix(
        RAO_HEADER_LEN,
        0,
        chunk_size,
        metadata_frame_len,
        plaintext_size,
    )
}

pub fn stored_size_from_parts_with_prefix(
    header_len: u64,
    key_frame_len: u64,
    chunk_size: u64,
    metadata_frame_len: u64,
    plaintext_size: u64,
) -> Result<u64, AeadFrameError> {
    let payload_len = payload_frame_len(plaintext_size, chunk_size)?;
    let prefix_len = checked_add(header_len, key_frame_len)?;
    let footer_base = checked_add(prefix_len, metadata_frame_len)?;
    let footer_payload_end = checked_add(footer_base, payload_len)?;
    let footer_end = checked_add(footer_payload_end, RAO_FOOTER_LEN)?;
    round_up(footer_end, chunk_size)
}

pub fn expected_stored_size(
    chunk_size: u64,
    metadata_frame_len: u64,
    plaintext_size: u64,
) -> Result<u64, AeadFrameError> {
    stored_size_from_parts(chunk_size, metadata_frame_len, plaintext_size)
}

pub fn cipher_offset(
    metadata_frame_len: u64,
    chunk_size: u64,
    block_index: u64,
) -> Result<u64, AeadFrameError> {
    cipher_offset_with_prefix(
        RAO_HEADER_LEN,
        0,
        metadata_frame_len,
        chunk_size,
        block_index,
    )
}

pub fn cipher_offset_with_prefix(
    header_len: u64,
    key_frame_len: u64,
    metadata_frame_len: u64,
    chunk_size: u64,
    block_index: u64,
) -> Result<u64, AeadFrameError> {
    let stride = checked_add(chunk_size, CHACHA20POLY1305_TAG_LEN)?;
    let prefix_len = checked_add(header_len, key_frame_len)?;
    let base = checked_add(prefix_len, metadata_frame_len)?;
    let payload_offset = checked_mul(block_index, stride)?;
    checked_add(base, payload_offset)
}

pub fn validate_range(start: u64, len: u64, plaintext_size: u64) -> Result<u64, AeadFrameError> {
    let end = match start.checked_add(len) {
        Some(end) => end,
        None => return Err(AeadFrameError::PlaintextRangeOverflow),
    };
    if len == 0 {
        if start > plaintext_size {
            return Err(AeadFrameError::EmptyRangeStartsPastEnd);
        }
        return Ok(end);
    }
    if end > plaintext_size {
        return Err(AeadFrameError::PlaintextRangePastEnd);
    }
    Ok(end)
}

pub fn nonempty_range_plan(
    metadata_frame_len: u64,
    chunk_size: u64,
    plaintext_size: u64,
    plaintext_start: u64,
    plaintext_len: u64,
) -> Result<RangePlan, AeadFrameError> {
    if chunk_size == 0 {
        return Err(AeadFrameError::InvalidChunkSize);
    }
    let plaintext_end = validate_range(plaintext_start, plaintext_len, plaintext_size)?;
    if plaintext_len == 0 {
        return Err(AeadFrameError::InvalidMetadataField);
    }

    let first_chunk = plaintext_start / chunk_size;
    let last_byte = checked_sub(plaintext_end, 1)?;
    let last_chunk = last_byte / chunk_size;
    let fetched_minus_one = checked_sub(last_chunk, first_chunk)?;
    let fetched_chunk_count = checked_add(fetched_minus_one, 1)?;
    let stored_chunk_len = checked_add(chunk_size, CHACHA20POLY1305_TAG_LEN)?;
    let stored_range_start = cipher_offset(metadata_frame_len, chunk_size, first_chunk)?;
    let last_chunk_start = cipher_offset(metadata_frame_len, chunk_size, last_chunk)?;
    let stored_range_end = checked_add(last_chunk_start, stored_chunk_len)?;
    let stored_range_len = checked_sub(stored_range_end, stored_range_start)?;
    let trim_start = plaintext_start % chunk_size;

    Ok(RangePlan {
        plaintext_end,
        first_chunk,
        last_chunk,
        fetched_chunk_count,
        stored_range_start,
        stored_range_len,
        trim_start,
    })
}

pub fn inspect_geometry(
    stored_size_bytes: u64,
    metadata_frame_len: u64,
    chunk_size: u64,
) -> Result<InspectGeometry, AeadFrameError> {
    inspect_geometry_with_prefix(
        RAO_HEADER_LEN,
        0,
        stored_size_bytes,
        metadata_frame_len,
        chunk_size,
    )
}

pub fn inspect_geometry_with_prefix(
    header_len: u64,
    key_frame_len: u64,
    stored_size_bytes: u64,
    metadata_frame_len: u64,
    chunk_size: u64,
) -> Result<InspectGeometry, AeadFrameError> {
    if chunk_size == 0 {
        return Err(AeadFrameError::InvalidChunkSize);
    }
    if stored_size_bytes % chunk_size != 0 {
        return Err(AeadFrameError::TrailingData);
    }
    let prefix_len = checked_add(header_len, key_frame_len)?;
    let minimum_size = checked_add(checked_add(prefix_len, RAO_FOOTER_LEN)?, metadata_frame_len)?;
    if stored_size_bytes < minimum_size {
        return Err(AeadFrameError::UnexpectedEof);
    }

    let stride = checked_add(chunk_size, CHACHA20POLY1305_TAG_LEN)?;
    let fixed_len = checked_add(prefix_len, RAO_FOOTER_LEN)?;
    let without_fixed = match stored_size_bytes.checked_sub(fixed_len) {
        Some(value) => value,
        None => return Err(AeadFrameError::UnexpectedEof),
    };
    let numerator = match without_fixed.checked_sub(metadata_frame_len) {
        Some(value) => value,
        None => return Err(AeadFrameError::UnexpectedEof),
    };
    let chunk_count = numerator / stride;
    if chunk_count == 0 {
        return Err(AeadFrameError::UnexpectedEof);
    }
    let plaintext_size = checked_mul(chunk_count, chunk_size)?;
    let payload_span = checked_mul(chunk_count, stride)?;
    let footer_base = checked_add(prefix_len, metadata_frame_len)?;
    let footer_offset = checked_add(footer_base, payload_span)?;
    let footer_end = checked_add(footer_offset, RAO_FOOTER_LEN)?;
    let expected_stored_size = round_up(footer_end, chunk_size)?;
    if expected_stored_size != stored_size_bytes {
        return Err(AeadFrameError::TrailingData);
    }
    let fill_len = checked_sub(expected_stored_size, footer_end)?;

    Ok(InspectGeometry {
        stride,
        numerator,
        chunk_count,
        plaintext_size,
        footer_offset,
        expected_stored_size,
        fill_len,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drift_guard() {
        let this_file = include_str!("lib.rs");
        let stream = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/remanence-aead/src/stream.rs"
        ))
        .expect("production stream.rs must be readable from verif/aead-framing");
        let range = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/remanence-aead/src/range.rs"
        ))
        .expect("production range.rs must be readable from verif/aead-framing");
        let inspect = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/remanence-aead/src/inspect.rs"
        ))
        .expect("production inspect.rs must be readable from verif/aead-framing");

        let stream_snippets: &[&str] = &[
            "if chunk == 0 || plaintext_size == 0 || plaintext_size % chunk != 0",
            "plaintext_size\n        .checked_add(\n            CHACHA20POLY1305_TAG_LEN\n                .checked_mul(chunks)",
            "stored_size_from_parts_with_key_frame(chunk_size, 0, metadata_frame_len, plaintext_size)",
            ".checked_add(u64::from(key_frame_len))\n        .and_then(|value| value.checked_add(metadata_frame_len))",
            "let remainder = value % multiple;\n    if remainder == 0 {\n        Ok(value)\n    } else {",
            "let stride = u64::from(chunk_size)\n        .checked_add(CHACHA20POLY1305_TAG_LEN)",
            "cipher_offset_with_key_frame(0, metadata_frame_len, chunk_size, b)",
            ".checked_add(u64::from(key_frame_len))\n        .and_then(|value| value.checked_add(metadata_frame_len))",
        ];
        for (i, snippet) in stream_snippets.iter().enumerate() {
            assert!(
                stream.contains(snippet),
                "stream snippet {i} changed; re-sync AEAD framing extraction"
            );
        }

        let range_snippets: &[&str] = &[
            "let plaintext_end = validate_range(plaintext_start, plaintext_len, metadata.plaintext_size)?;",
            "let first_chunk = plaintext_start / chunk_size;",
            "let last_chunk = (plaintext_end - 1) / chunk_size;",
            "let fetched_chunk_count = last_chunk\n        .checked_sub(first_chunk)\n        .and_then(|value| value.checked_add(1))",
            "let stored_chunk_len_u64 = chunk_size\n        .checked_add(CHACHA20POLY1305_TAG_LEN)",
            "let stored_range_start = cipher_offset_with_key_frame(\n        header.key_frame_len,\n        header.metadata_frame_len,\n        header.chunk_size,\n        first_chunk,",
            "let stored_range_end = last_chunk_start\n        .checked_add(stored_chunk_len_u64)",
            "let trim_start =\n        usize::try_from(plaintext_start % chunk_size)",
            "let end = start.checked_add(len).ok_or_else(||",
            "if len == 0 {\n        if start > plaintext_size {",
            "if end > plaintext_size {",
        ];
        for (i, snippet) in range_snippets.iter().enumerate() {
            assert!(
                range.contains(snippet),
                "range snippet {i} changed; re-sync AEAD framing extraction"
            );
        }

        let inspect_snippets: &[&str] = &[
            "if stored_size_bytes % u64::from(header.chunk_size) != 0",
            "let stride = u64::from(header.chunk_size)\n        .checked_add(16)",
            "let key_frame_len = u64::from(header.key_frame_len);",
            ".checked_sub(RAO_HEADER_LEN as u64)\n        .and_then(|value| value.checked_sub(key_frame_len))\n        .and_then(|value| value.checked_sub(RAO_FOOTER.len() as u64))",
            "let chunk_count = numerator / stride;",
            "if chunk_count == 0 {",
            "let plaintext_size = chunk_count\n        .checked_mul(u64::from(header.chunk_size))",
            "let footer_offset = (RAO_HEADER_LEN as u64)\n        .checked_add(key_frame_len)\n        .and_then(|value| value.checked_add(header.metadata_frame_len))",
            "let expected_size = round_up(\n        footer_offset\n            .checked_add(RAO_FOOTER.len() as u64)",
            "if expected_size != stored_size_bytes {",
        ];
        for (i, snippet) in inspect_snippets.iter().enumerate() {
            assert!(
                inspect.contains(snippet),
                "inspect snippet {i} changed; re-sync AEAD framing extraction"
            );
        }

        let extraction_snippets: &[&str] = &[
            "let chunks = chunk_count(plaintext_size, chunk_size)?;",
            "let prefix_len = checked_add(header_len, key_frame_len)?;",
            "let footer_base = checked_add(prefix_len, metadata_frame_len)?;",
            "let footer_payload_end = checked_add(footer_base, payload_len)?;",
            "let footer_end = checked_add(footer_payload_end, RAO_FOOTER_LEN)?;",
            "let stride = checked_add(chunk_size, CHACHA20POLY1305_TAG_LEN)?;",
            "let payload_offset = checked_mul(block_index, stride)?;",
            "let last_chunk = last_byte / chunk_size;",
            "let stored_range_end = checked_add(last_chunk_start, stored_chunk_len)?;",
            "let trim_start = plaintext_start % chunk_size;",
            "let numerator = match without_fixed.checked_sub(metadata_frame_len)",
            "let expected_stored_size = round_up(footer_end, chunk_size)?;",
        ];
        for (i, snippet) in extraction_snippets.iter().enumerate() {
            assert!(
                this_file.contains(snippet),
                "extraction snippet {i} missing from AEAD framing model"
            );
        }
    }

    #[test]
    fn sample_stream_geometry_matches_production_formula() {
        let chunk = 512;
        let metadata = 64;
        let plaintext = 1536;
        assert_eq!(chunk_count(plaintext, chunk).unwrap(), 3);
        assert_eq!(payload_frame_len(plaintext, chunk).unwrap(), 1584);
        assert_eq!(cipher_offset(metadata, chunk, 0).unwrap(), 192);
        assert_eq!(cipher_offset(metadata, chunk, 2).unwrap(), 1248);
        assert_eq!(
            stored_size_from_parts(chunk, metadata, plaintext).unwrap(),
            2048
        );
    }

    #[test]
    fn sample_range_plan_matches_production_test_case() {
        let plan = nonempty_range_plan(64, 512, 1536, 400, 700).unwrap();
        assert_eq!(plan.plaintext_end, 1100);
        assert_eq!(plan.first_chunk, 0);
        assert_eq!(plan.last_chunk, 2);
        assert_eq!(plan.fetched_chunk_count, 3);
        assert_eq!(plan.stored_range_start, 192);
        assert_eq!(plan.stored_range_len, 3 * (512 + 16));
        assert_eq!(plan.trim_start, 400);
    }

    #[test]
    fn inspect_geometry_accepts_exact_rounded_layout() {
        let geometry = inspect_geometry(2048, 64, 512).unwrap();
        assert_eq!(geometry.stride, 528);
        assert_eq!(geometry.chunk_count, 3);
        assert_eq!(geometry.plaintext_size, 1536);
        assert_eq!(geometry.footer_offset, 1776);
        assert_eq!(geometry.expected_stored_size, 2048);
        assert_eq!(geometry.fill_len, 256);
    }

    #[test]
    fn generic_prefix_geometry_covers_v1_and_v2_instances() {
        let v1 = inspect_geometry_with_prefix(128, 0, 2048, 64, 512).unwrap();
        assert_eq!(v1, inspect_geometry(2048, 64, 512).unwrap());

        let key_frame_len = 211;
        let v2_stored =
            stored_size_from_parts_with_prefix(128, key_frame_len, 512, 64, 1536).unwrap();
        let v2 = inspect_geometry_with_prefix(128, key_frame_len, v2_stored, 64, 512).unwrap();
        assert_eq!(v2.chunk_count, 3);
        assert_eq!(v2.footer_offset, 128 + key_frame_len + 64 + 3 * 528);
        assert_eq!(
            cipher_offset_with_prefix(128, key_frame_len, 64, 512, 2).unwrap(),
            128 + key_frame_len + 64 + 2 * 528
        );
    }
}
