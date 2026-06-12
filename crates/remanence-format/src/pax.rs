//! POSIX pax record encoding and the `REMANENCE.pad` alignment solver.

use std::collections::BTreeMap;

use crate::error::FormatError;
use crate::model::TAR_RECORD_SIZE;

const PAD_KEY: &str = "REMANENCE.pad";

/// Encode pax records in deterministic key order.
pub fn encode_pax_records(records: &BTreeMap<String, String>) -> Result<Vec<u8>, FormatError> {
    let mut out = Vec::new();
    for (key, value) in records {
        out.extend_from_slice(&encode_pax_record(key, value)?);
    }
    Ok(out)
}

/// Compute the byte length of one encoded pax record.
pub fn pax_record_len(key: &str, value_len: usize) -> Result<usize, FormatError> {
    validate_keyword(key)?;
    let base = key
        .len()
        .checked_add(value_len)
        .and_then(|n| n.checked_add(3))
        .ok_or_else(|| FormatError::layout("pax record length overflow"))?;
    let mut digits = decimal_digits(base);
    loop {
        let len = base
            .checked_add(digits)
            .ok_or_else(|| FormatError::layout("pax record length overflow"))?;
        let next = decimal_digits(len);
        if next == digits {
            return Ok(len);
        }
        digits = next;
    }
}

/// Return a copy of `base_records` with a solved `REMANENCE.pad` value.
pub fn with_alignment_pad(
    offset: u64,
    chunk_size: usize,
    base_records: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, FormatError> {
    validate_chunk_size(chunk_size)?;
    if offset % TAR_RECORD_SIZE as u64 != 0 {
        return Err(FormatError::invalid(
            "pax alignment offset must be a multiple of 512",
        ));
    }
    if base_records.contains_key(PAD_KEY) {
        return Err(FormatError::invalid(
            "caller must not pre-populate REMANENCE.pad",
        ));
    }

    let base_len = pax_records_len(base_records)?;
    let search_min = round_up_usize(
        base_len
            .checked_add(pax_record_len(PAD_KEY, 0)?)
            .ok_or_else(|| FormatError::layout("pax body length overflow"))?,
        TAR_RECORD_SIZE,
    )?;
    let mut min_rounded = search_min;

    let chunk = chunk_size as u64;
    let residue = (chunk - ((offset + 1024) % chunk)) % chunk;
    while (min_rounded as u64) % chunk != residue {
        min_rounded = min_rounded
            .checked_add(TAR_RECORD_SIZE)
            .ok_or_else(|| FormatError::layout("pax alignment search overflow"))?;
    }

    let mut target = min_rounded;
    let max_target = search_min
        .checked_add(
            chunk_size
                .checked_mul(4)
                .ok_or_else(|| FormatError::layout("pax alignment search bound overflow"))?,
        )
        .ok_or_else(|| FormatError::layout("pax alignment search bound overflow"))?;

    while target <= max_target {
        let pad_len = max_pad_len_for_target(base_len, target)?;
        let body_len = base_len
            .checked_add(pax_record_len(PAD_KEY, pad_len)?)
            .ok_or_else(|| FormatError::layout("pax body length overflow"))?;
        if round_up_usize(body_len, TAR_RECORD_SIZE)? == target {
            let mut records = base_records.clone();
            records.insert(PAD_KEY.to_string(), " ".repeat(pad_len));
            debug_assert_eq!(encode_pax_records(&records)?.len(), body_len);
            return Ok(records);
        }

        target = target
            .checked_add(chunk_size)
            .ok_or_else(|| FormatError::layout("pax alignment target overflow"))?;
    }

    Err(FormatError::layout(
        "could not solve REMANENCE.pad alignment within search bound",
    ))
}

/// Round up `value` to `unit`.
pub fn round_up_usize(value: usize, unit: usize) -> Result<usize, FormatError> {
    if unit == 0 {
        return Err(FormatError::invalid("rounding unit must be non-zero"));
    }
    let remainder = value % unit;
    if remainder == 0 {
        return Ok(value);
    }
    value
        .checked_add(unit - remainder)
        .ok_or_else(|| FormatError::layout("round-up overflow"))
}

/// Return the tar padding needed after a byte payload.
pub fn tar_padding_len(size: u64) -> usize {
    let rem = (size as usize) % TAR_RECORD_SIZE;
    if rem == 0 {
        0
    } else {
        TAR_RECORD_SIZE - rem
    }
}

pub(crate) fn validate_chunk_size(chunk_size: usize) -> Result<(), FormatError> {
    if chunk_size == 0 {
        return Err(FormatError::invalid("chunk_size must be non-zero"));
    }
    if chunk_size % TAR_RECORD_SIZE != 0 {
        return Err(FormatError::invalid(
            "chunk_size must be a positive multiple of 512",
        ));
    }
    Ok(())
}

fn encode_pax_record(key: &str, value: &str) -> Result<Vec<u8>, FormatError> {
    validate_keyword(key)?;
    validate_value(value)?;

    let len = pax_record_len(key, value.len())?;
    let line = format!("{len} {key}={value}\n");
    if line.len() != len {
        return Err(FormatError::layout("pax record fixed-point mismatch"));
    }
    Ok(line.into_bytes())
}

fn pax_records_len(records: &BTreeMap<String, String>) -> Result<usize, FormatError> {
    let mut len = 0usize;
    for (key, value) in records {
        len = len
            .checked_add(pax_record_len(key, value.len())?)
            .ok_or_else(|| FormatError::layout("pax records length overflow"))?;
    }
    Ok(len)
}

fn max_pad_len_for_target(base_len: usize, target: usize) -> Result<usize, FormatError> {
    let mut lo = 0usize;
    let mut hi = target;
    let mut best = 0usize;
    while lo <= hi {
        let mid = lo + (hi - lo) / 2;
        let len = base_len
            .checked_add(pax_record_len(PAD_KEY, mid)?)
            .ok_or_else(|| FormatError::layout("pax body length overflow"))?;
        if len <= target {
            best = mid;
            lo = mid.saturating_add(1);
        } else if mid == 0 {
            break;
        } else {
            hi = mid - 1;
        }
    }
    Ok(best)
}

fn validate_keyword(key: &str) -> Result<(), FormatError> {
    if key.is_empty() {
        return Err(FormatError::invalid("pax keyword must not be empty"));
    }
    if key.contains('=') || key.contains('\n') || key.contains('\0') {
        return Err(FormatError::invalid("invalid character in pax keyword"));
    }
    if !key.is_ascii() {
        return Err(FormatError::invalid("pax keyword must be ASCII"));
    }
    Ok(())
}

fn validate_value(value: &str) -> Result<(), FormatError> {
    if value.bytes().any(|byte| byte < 0x20) {
        return Err(FormatError::invalid(
            "pax value must not contain ASCII control characters",
        ));
    }
    Ok(())
}

fn decimal_digits(mut value: usize) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pax_record_length_handles_digit_boundaries() {
        for value_len in [0usize, 1, 9, 10, 90, 99, 100, 990, 999, 1000, 9999] {
            let value = "x".repeat(value_len);
            let encoded = encode_pax_record("REMANENCE.test", &value).unwrap();
            assert_eq!(
                encoded.len(),
                pax_record_len("REMANENCE.test", value_len).unwrap()
            );
            assert!(encoded.starts_with(encoded.len().to_string().as_bytes()));
            assert!(encoded.ends_with(b"\n"));
        }
    }

    #[test]
    fn alignment_pad_solves_normative_equation() {
        let mut records = BTreeMap::new();
        records.insert("path".to_string(), "video.mov".to_string());
        records.insert("size".to_string(), "1024".to_string());
        records.insert("REMANENCE.file_id".to_string(), "file-1".to_string());
        records.insert("REMANENCE.file_sha256".to_string(), "00".repeat(32));
        records.insert("REMANENCE.chunk_count".to_string(), "1".to_string());
        records.insert("REMANENCE.compression".to_string(), "none".to_string());

        for offset in [0u64, 512, 4096, 65_536, 260_608] {
            let solved = with_alignment_pad(offset, 262_144, &records).unwrap();
            let body_len = encode_pax_records(&solved).unwrap().len();
            let rounded = round_up_usize(body_len, TAR_RECORD_SIZE).unwrap();
            assert_eq!((offset + 512 + rounded as u64 + 512) % 262_144, 0);
            assert!(solved.contains_key(PAD_KEY));
        }
    }

    #[test]
    fn alignment_pad_rejects_non_record_aligned_offset() {
        let records = BTreeMap::new();

        let err = with_alignment_pad(1, 262_144, &records).expect_err("unaligned offset");

        assert!(err.to_string().contains("offset must be a multiple of 512"));
    }

    #[test]
    fn pax_record_rejects_control_characters_in_value() {
        let err = encode_pax_record("path", "bad\nname").unwrap_err();
        assert!(err.to_string().contains("control characters"));
    }
}
