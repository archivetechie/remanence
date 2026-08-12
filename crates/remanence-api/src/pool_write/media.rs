//! LTO media generations, compatibility, capacity, and write preconditions.
//!
//! The compatibility tables are explicit because newer LTO generations do
//! not follow one universal read-back or write-back formula.

use std::fmt;

use remanence_state::{TapePoolConfig, TapeRecord};

use super::capacity::{
    missing_geometry, tape_block_size, tape_physical_used_bytes, validate_scheme_columns,
};
use super::model::WritabilityError;

const LTO_RAW_CAPACITY_BYTES: &[(LtoGen, u64)] = &[
    (LtoGen::Lto1, 100_000_000_000),
    (LtoGen::Lto2, 200_000_000_000),
    (LtoGen::Lto3, 400_000_000_000),
    (LtoGen::Lto4, 800_000_000_000),
    (LtoGen::Lto5, 1_500_000_000_000),
    (LtoGen::Lto6, 2_500_000_000_000),
    (LtoGen::Lto7, 6_000_000_000_000),
    (LtoGen::M8, 9_000_000_000_000),
    (LtoGen::Lto8, 12_000_000_000_000),
    (LtoGen::Lto9, 18_000_000_000_000),
];

/// LTO cartridge generation parsed from a barcode media-type suffix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LtoGen {
    /// LTO-1 native media.
    Lto1,
    /// LTO-2 native media.
    Lto2,
    /// LTO-3 native media.
    Lto3,
    /// LTO-4 native media.
    Lto4,
    /// LTO-5 native media.
    Lto5,
    /// LTO-6 native media.
    Lto6,
    /// LTO-7 native media.
    Lto7,
    /// LTO-7 Type-M initialized media.
    M8,
    /// LTO-8 native media.
    Lto8,
    /// LTO-9 native media.
    Lto9,
}

impl LtoGen {
    /// Numeric LTO generation, with Type-M represented as LTO-8 media class.
    pub fn generation_number(self) -> u8 {
        match self {
            Self::Lto1 => 1,
            Self::Lto2 => 2,
            Self::Lto3 => 3,
            Self::Lto4 => 4,
            Self::Lto5 => 5,
            Self::Lto6 => 6,
            Self::Lto7 => 7,
            Self::M8 | Self::Lto8 => 8,
            Self::Lto9 => 9,
        }
    }
}

impl fmt::Display for LtoGen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Lto1 => "LTO-1",
            Self::Lto2 => "LTO-2",
            Self::Lto3 => "LTO-3",
            Self::Lto4 => "LTO-4",
            Self::Lto5 => "LTO-5",
            Self::Lto6 => "LTO-6",
            Self::Lto7 => "LTO-7",
            Self::M8 => "LTO-7 Type-M",
            Self::Lto8 => "LTO-8",
            Self::Lto9 => "LTO-9",
        };
        f.write_str(label)
    }
}

/// Parse an LTO generation from the barcode media-type suffix.
pub fn lto_generation_from_voltag(voltag: &str) -> Option<LtoGen> {
    let trimmed = voltag.trim();
    if !trimmed.is_ascii() {
        return None;
    }
    let suffix_start = trimmed.len().checked_sub(2)?;
    let suffix = trimmed[suffix_start..].to_ascii_uppercase();
    match suffix.as_str() {
        "L1" => Some(LtoGen::Lto1),
        "L2" => Some(LtoGen::Lto2),
        "L3" => Some(LtoGen::Lto3),
        "L4" => Some(LtoGen::Lto4),
        "L5" => Some(LtoGen::Lto5),
        "L6" => Some(LtoGen::Lto6),
        "L7" => Some(LtoGen::Lto7),
        "M8" => Some(LtoGen::M8),
        "L8" => Some(LtoGen::Lto8),
        "L9" | "LZ" => Some(LtoGen::Lto9),
        _ => None,
    }
}

/// Parse a drive LTO generation from common INQUIRY product strings.
pub fn lto_generation_from_drive_product(product: &str) -> Option<LtoGen> {
    let product = product.to_ascii_uppercase();
    for (needle, generation) in [
        ("LTO-9", LtoGen::Lto9),
        ("LTO9", LtoGen::Lto9),
        ("ULTRIUM 9", LtoGen::Lto9),
        ("LTO-8", LtoGen::Lto8),
        ("LTO8", LtoGen::Lto8),
        ("ULTRIUM 8", LtoGen::Lto8),
        ("LTO-7", LtoGen::Lto7),
        ("LTO7", LtoGen::Lto7),
        ("ULTRIUM 7", LtoGen::Lto7),
        ("LTO-6", LtoGen::Lto6),
        ("LTO6", LtoGen::Lto6),
        ("ULTRIUM 6", LtoGen::Lto6),
        ("LTO-5", LtoGen::Lto5),
        ("LTO5", LtoGen::Lto5),
        ("ULTRIUM 5", LtoGen::Lto5),
        ("LTO-4", LtoGen::Lto4),
        ("LTO4", LtoGen::Lto4),
        ("ULTRIUM 4", LtoGen::Lto4),
        ("LTO-3", LtoGen::Lto3),
        ("LTO3", LtoGen::Lto3),
        ("ULTRIUM 3", LtoGen::Lto3),
        ("LTO-2", LtoGen::Lto2),
        ("LTO2", LtoGen::Lto2),
        ("ULTRIUM 2", LtoGen::Lto2),
        ("LTO-1", LtoGen::Lto1),
        ("LTO1", LtoGen::Lto1),
        ("ULTRIUM 1", LtoGen::Lto1),
    ] {
        if product.contains(needle) {
            return Some(generation);
        }
    }
    None
}

/// Return whether an LTO drive generation can read a cartridge generation.
///
/// This is an explicit media compatibility table, not the historical
/// "read two generations back" formula. LTO-8 and LTO-9 intentionally break
/// that formula, and Type-M (`M8`) is modeled as its own media generation.
pub fn can_read(drive: LtoGen, tape: LtoGen) -> bool {
    match drive {
        LtoGen::Lto5 => matches!(tape, LtoGen::Lto5 | LtoGen::Lto4 | LtoGen::Lto3),
        LtoGen::Lto6 => matches!(tape, LtoGen::Lto6 | LtoGen::Lto5 | LtoGen::Lto4),
        LtoGen::Lto7 => matches!(tape, LtoGen::Lto7 | LtoGen::Lto6 | LtoGen::Lto5),
        LtoGen::Lto8 => matches!(tape, LtoGen::Lto8 | LtoGen::Lto7 | LtoGen::M8),
        LtoGen::Lto9 => matches!(tape, LtoGen::Lto9 | LtoGen::Lto8),
        LtoGen::Lto1 | LtoGen::Lto2 | LtoGen::Lto3 | LtoGen::Lto4 | LtoGen::M8 => false,
    }
}

/// Return whether an LTO drive generation can write a cartridge generation.
///
/// The table mirrors the authoritative init-flow design and is kept separate
/// from [`can_read`] because read and write compatibility differ.
pub fn can_write(drive: LtoGen, tape: LtoGen) -> bool {
    match drive {
        LtoGen::Lto5 => matches!(tape, LtoGen::Lto5 | LtoGen::Lto4),
        LtoGen::Lto6 => matches!(tape, LtoGen::Lto6 | LtoGen::Lto5),
        LtoGen::Lto7 => matches!(tape, LtoGen::Lto7 | LtoGen::Lto6),
        LtoGen::Lto8 => matches!(tape, LtoGen::Lto8 | LtoGen::Lto7 | LtoGen::M8),
        LtoGen::Lto9 => matches!(tape, LtoGen::Lto9 | LtoGen::Lto8),
        LtoGen::Lto1 | LtoGen::Lto2 | LtoGen::Lto3 | LtoGen::Lto4 | LtoGen::M8 => false,
    }
}

/// Native/raw cartridge capacity in bytes for one LTO generation.
pub fn raw_capacity_bytes(generation: LtoGen) -> u64 {
    LTO_RAW_CAPACITY_BYTES
        .iter()
        .find_map(|(candidate, bytes)| (*candidate == generation).then_some(*bytes))
        .expect("all LTO generations have a raw capacity entry")
}

/// Check that a catalog tape row is a hard-valid target for `object_size`.
pub fn check_writability_preconditions(
    tape: &TapeRecord,
    object_size: u64,
) -> Result<(), WritabilityError> {
    if tape.state != "ready" {
        return Err(WritabilityError::NotReady {
            state: tape.state.clone(),
        });
    }
    let block_size = tape
        .block_size
        .ok_or_else(|| missing_geometry("block_size is null"))?;
    if block_size == 0 {
        return Err(missing_geometry("block_size is zero"));
    }
    validate_scheme_columns(tape)?;
    if tape.total_committed_ordinals > 0 && tape.scheme_id.is_some() {
        return Err(WritabilityError::ParityAppendUnsupported {
            total_committed_ordinals: tape.total_committed_ordinals,
        });
    }
    let voltag = tape
        .voltag
        .as_deref()
        .ok_or_else(|| missing_geometry("voltag is null"))?;
    let generation = lto_generation_from_voltag(voltag)
        .ok_or_else(|| missing_geometry("voltag does not end in a known LTO suffix"))?;
    let raw_capacity = raw_capacity_bytes(generation);
    let used = tape_physical_used_bytes(tape, block_size)?;
    if used > raw_capacity || object_size > raw_capacity - used {
        return Err(WritabilityError::InsufficientCapacity {
            object_size,
            raw_capacity,
            used,
        });
    }
    Ok(())
}

pub(super) fn check_pool_block_size_precondition(
    tape: &TapeRecord,
    pool_cfg: &TapePoolConfig,
) -> Result<(), WritabilityError> {
    let tape_block_size = tape_block_size(tape)?;
    if tape_block_size != pool_cfg.block_size_bytes {
        return Err(WritabilityError::BlockSizeMismatch {
            tape_block_size,
            pool_block_size: pool_cfg.block_size_bytes,
        });
    }
    Ok(())
}
