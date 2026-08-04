//! Structural cartridge geometry — design-read-ordering.md §6.3.
//!
//! Wrap boundaries come from the drive via the cached REOWP map, never
//! from this table. The table supplies only the structural constants that
//! are exact and format-defined — bands, wraps per band, channels, data
//! tracks — used to derive the band a wrap belongs to for the
//! second-order band-step cost term. `data_tracks` is stored
//! independently rather than computed, so the identity
//! `bands * wraps_per_band * channels == data_tracks` is a checkable fact
//! about each row rather than a construction.
//!
//! A lookup distinguishes three outcomes: a supported row, a recognised
//! but unsupported row carrying its reason, and no row at all.

/// One supported structural row of the §6.3 table.
///
/// `data_tracks` is stored, not derived; the §6.3 identity over the row
/// is asserted by tests. `source` records where the row's figures come
/// from and is itself asserted by tests, so a row cannot change without
/// its provenance changing with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StructuralRow {
    /// Cartridge generation, e.g. `"LTO-9"`.
    pub cartridge_generation: &'static str,
    /// Recording format, e.g. `"L9"`.
    pub recording_format: &'static str,
    /// Data bands across the width of the tape.
    pub bands: u32,
    /// Wraps in each band.
    pub wraps_per_band: u32,
    /// Total wraps, `bands * wraps_per_band`.
    pub wraps: u32,
    /// Concurrently written channels.
    pub channels: u32,
    /// Total data tracks. Stored independently of the identity.
    pub data_tracks: u32,
    /// Provenance of the row's figures.
    pub source: &'static str,
}

/// A recognised `(cartridge_generation, recording_format)` key that the
/// planner refuses to serve, with the reason it is refused.
///
/// Unsupported is a different answer from absent: the caller can tell
/// "we know this format and will not plan for it" apart from "we have
/// never heard of this key".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnsupportedRow {
    /// Cartridge generation component of the recognised key.
    pub cartridge_generation: &'static str,
    /// Recording format component of the recognised key.
    pub recording_format: &'static str,
    /// Why the key is refused.
    pub reason: &'static str,
    /// Provenance of the refusal.
    pub source: &'static str,
}

/// The supported structural table — design §6.3. LA and PA keep separate
/// rows to preserve the canonical geometry key even though their
/// structural constants are presently identical.
pub const STRUCTURAL_TABLE: [StructuralRow; 5] = [
    StructuralRow {
        cartridge_generation: "LTO-7",
        recording_format: "L7",
        bands: 4,
        wraps_per_band: 28,
        wraps: 112,
        channels: 32,
        data_tracks: 3584,
        source: "Track and channel counts as previously established from the HP LTO Ultrium \
                 Technical Reference Manual lineage (Volume 4); design-read-ordering.md \u{a7}6.3.",
    },
    StructuralRow {
        cartridge_generation: "LTO-8",
        recording_format: "L8",
        bands: 4,
        wraps_per_band: 52,
        wraps: 208,
        channels: 32,
        data_tracks: 6656,
        source: "Published LTO-8 specification tables and CERN EOSCTA operations notes; \
                 design-read-ordering.md \u{a7}6.3.",
    },
    StructuralRow {
        cartridge_generation: "LTO-9",
        recording_format: "L9",
        bands: 4,
        wraps_per_band: 70,
        wraps: 280,
        channels: 32,
        data_tracks: 8960,
        source: "HPE published LTO-9 figures; design-read-ordering.md \u{a7}6.3.",
    },
    StructuralRow {
        cartridge_generation: "LTO-10",
        recording_format: "LA",
        bands: 4,
        wraps_per_band: 118,
        wraps: 472,
        channels: 32,
        data_tracks: 15104,
        source: "Published LTO-10 structural figures; media code LA per IBM TS4300 \
                 'Bar code label' Table 2; design-read-ordering.md \u{a7}6.3.",
    },
    StructuralRow {
        cartridge_generation: "LTO-10",
        recording_format: "PA",
        bands: 4,
        wraps_per_band: 118,
        wraps: 472,
        channels: 32,
        data_tracks: 15104,
        source: "Published LTO-10 structural figures; media code PA per IBM TS4300 \
                 'Bar code label' Table 2; design-read-ordering.md \u{a7}6.3.",
    },
];

const M8_REASON: &str = "Conflicting published structural geometry: the LTO-7 track anchor \
     implies 5,376 tracks and the LTO-8 capacity anchor implies 4,992, and neither candidate \
     reconciles across both anchors; unresolvable without hardware.";

const PRE_REOWP_REASON: &str = "Predates READ END OF WRAP POSITION, so exact wrap boundaries \
     are unobtainable; the arithmetic fallback is measurably worse than not ordering \
     (design \u{a7}6.2).";

const PRE_REOWP_SOURCE: &str = "design-read-ordering.md \u{a7}\u{a7}6.2-6.3; IBM LTO SCSI \
     Reference GA32-0928-09 Table A.1 (REOWP listed for generations 7-10 only).";

/// Recognised-but-unsupported keys — design §6.3 and D4a.
///
/// The M8 row answers any lookup naming `M8` in either component. The
/// pre-REOWP generations answer their exact `(generation, format)` pairs.
pub const UNSUPPORTED_TABLE: [UnsupportedRow; 7] = [
    UnsupportedRow {
        cartridge_generation: "M8",
        recording_format: "M8",
        reason: M8_REASON,
        source: "design-read-ordering.md \u{a7}6.3.",
    },
    UnsupportedRow {
        cartridge_generation: "LTO-6",
        recording_format: "L6",
        reason: PRE_REOWP_REASON,
        source: PRE_REOWP_SOURCE,
    },
    UnsupportedRow {
        cartridge_generation: "LTO-5",
        recording_format: "L5",
        reason: PRE_REOWP_REASON,
        source: PRE_REOWP_SOURCE,
    },
    UnsupportedRow {
        cartridge_generation: "LTO-4",
        recording_format: "L4",
        reason: PRE_REOWP_REASON,
        source: PRE_REOWP_SOURCE,
    },
    UnsupportedRow {
        cartridge_generation: "LTO-3",
        recording_format: "L3",
        reason: PRE_REOWP_REASON,
        source: PRE_REOWP_SOURCE,
    },
    UnsupportedRow {
        cartridge_generation: "LTO-2",
        recording_format: "L2",
        reason: PRE_REOWP_REASON,
        source: PRE_REOWP_SOURCE,
    },
    UnsupportedRow {
        cartridge_generation: "LTO-1",
        recording_format: "L1",
        reason: PRE_REOWP_REASON,
        source: PRE_REOWP_SOURCE,
    },
];

/// Result of a structural-geometry lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeometryLookup {
    /// A supported row; planning may proceed.
    Supported(&'static StructuralRow),
    /// A recognised key the planner refuses, with the reason.
    Unsupported(&'static UnsupportedRow),
    /// No row for this key at all.
    Absent,
}

/// Look up the structural geometry for a `(generation, format)` pair.
///
/// Matching is exact and case-sensitive; the wire layer normalises before
/// calling. A pair naming `M8` in either component is recognised but
/// unsupported (design §6.3). Impossible cross-generation pairs (for
/// example `("LTO-7", "L8")`) are `Absent` — the wire layer decides what
/// status an impossible pair maps to.
pub fn lookup_geometry(cartridge_generation: &str, recording_format: &str) -> GeometryLookup {
    for row in &STRUCTURAL_TABLE {
        if row.cartridge_generation == cartridge_generation
            && row.recording_format == recording_format
        {
            return GeometryLookup::Supported(row);
        }
    }
    if cartridge_generation == "M8" || recording_format == "M8" {
        return GeometryLookup::Unsupported(&UNSUPPORTED_TABLE[0]);
    }
    for row in &UNSUPPORTED_TABLE[1..] {
        if row.cartridge_generation == cartridge_generation
            && row.recording_format == recording_format
        {
            return GeometryLookup::Unsupported(row);
        }
    }
    GeometryLookup::Absent
}

/// Physical order of logical bands across the width of the tape —
/// design §6.4. `BAND_LAYOUT[rank] == logical_band`: physical rank 0 is
/// occupied by logical band 3, rank 1 by band 1, rank 2 by band 0 and
/// rank 3 by band 2. No supported generation departs from it.
///
/// Layout and fill order are different facts: recording fills bands
/// numerically (0, 1, 2, 3); this constant only says where each band
/// sits physically.
pub const BAND_LAYOUT: [u32; 4] = [3, 1, 0, 2];

/// Physical rank of a logical band under [`BAND_LAYOUT`] — the
/// `index_of` lookup from design §6.4. `None` when the band is outside
/// the four-band layout.
pub fn band_rank(logical_band: u32) -> Option<u32> {
    BAND_LAYOUT
        .iter()
        .position(|&b| b == logical_band)
        .map(|i| i as u32)
}
