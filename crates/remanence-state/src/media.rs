//! Cartridge evidence and durable endpoint identities shared by state and API.

use std::fmt;

/// Recorded LTO cartridge generation, independent of its shell label.
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
fn parse_label_generation(voltag: &str) -> Option<LtoGen> {
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

/// Who read the operator-facing label; neither source proves BOT identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LabelProvenance {
    /// Read by a medium changer scanner.
    Changer,
    /// Asserted by a person.
    Operator,
}
impl LabelProvenance {
    /// Stable SQLite/audit spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Changer => "changer",
            Self::Operator => "operator",
        }
    }
}
/// Origin of a cartridge generation observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationSource {
    /// Density measured by the drive.
    Measured,
    /// Pre-load hint read from a label suffix.
    Label,
    /// Explicit operator assertion.
    Operator,
}
impl GenerationSource {
    /// Stable SQLite/audit spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Measured => "measured",
            Self::Label => "label",
            Self::Operator => "operator",
        }
    }
}
impl LtoGen {
    /// Stable cartridge media code stored in the catalog.
    pub fn media_code(self) -> &'static str {
        match self {
            Self::Lto1 => "L1",
            Self::Lto2 => "L2",
            Self::Lto3 => "L3",
            Self::Lto4 => "L4",
            Self::Lto5 => "L5",
            Self::Lto6 => "L6",
            Self::Lto7 => "L7",
            Self::M8 => "M8",
            Self::Lto8 => "L8",
            Self::Lto9 => "L9",
        }
    }
    /// Parse an exact recorded media code (never a shell label).
    pub fn from_media_code(code: &str) -> Option<Self> {
        if code.len() != 2 {
            return None;
        }
        parse_label_generation(code)
    }
}
/// A generation observation before a catalog row exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaObservation {
    /// Observed cartridge generation.
    pub generation: LtoGen,
    /// Evidence that supplied the generation.
    pub source: GenerationSource,
}
impl MediaObservation {
    /// Pre-load selection hint; it is never a measured density.
    pub fn label_hint(label: &str) -> Option<Self> {
        Some(Self {
            generation: parse_label_generation(label)?,
            source: GenerationSource::Label,
        })
    }
    /// Validate post-load density against the hint. An explicit operator
    /// override permits disagreement, but measured density remains the result.
    pub fn measured(
        generation: LtoGen,
        hint: Option<Self>,
        operator_override: bool,
    ) -> Result<Self, String> {
        if let Some(hint) = hint {
            if hint.generation != generation {
                if !operator_override {
                    return Err(format!(
                        "generation hint {} disagrees with measured {}",
                        hint.generation, generation
                    ));
                }
                tracing::warn!(hint = %hint.generation, measured = %generation, "operator override permits density disagreement");
            }
        }
        Ok(Self {
            generation,
            source: GenerationSource::Measured,
        })
    }
}
/// Read recorded generation authority. A missing pair never falls back to a label.
pub fn tape_generation(tape: &crate::TapeRecord) -> Option<(LtoGen, GenerationSource)> {
    let generation = LtoGen::from_media_code(tape.cartridge_generation.as_deref()?)?;
    let source = match tape.generation_source.as_deref()? {
        "measured" => GenerationSource::Measured,
        "label" => GenerationSource::Label,
        "operator" => GenerationSource::Operator,
        _ => return None,
    };
    if let Some(hint) = tape
        .voltag
        .as_deref()
        .and_then(MediaObservation::label_hint)
    {
        if hint.generation != generation {
            tracing::warn!(label = ?tape.voltag, label_generation = %hint.generation, recorded_generation = %generation, "label suffix disagrees with recorded cartridge generation");
        }
    }
    Some((generation, source))
}
