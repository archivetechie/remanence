//! Acceptance tests for the §6.3 structural table and band layout.

use remanence_order::{
    band_rank, lookup_geometry, lookup_media_code, GeometryLookup, BAND_LAYOUT, MEDIA_CODE_TABLE,
    STRUCTURAL_TABLE, UNSUPPORTED_TABLE,
};

/// The §6.3 identity holds for every row: `bands * wraps_per_band *
/// channels == data_tracks`, with `data_tracks` stored independently.
///
/// This identity is **necessary but not sufficient** — any invented row
/// can be made internally self-consistent, which is exactly why it did
/// not catch a wrong M8 row. The provenance assertions below are the
/// other half of the check.
#[test]
fn table_identity_holds_for_every_row() {
    for row in &STRUCTURAL_TABLE {
        assert_eq!(
            row.bands * row.wraps_per_band * row.channels,
            row.data_tracks,
            "identity fails for ({}, {})",
            row.cartridge_generation,
            row.recording_format,
        );
        assert_eq!(
            row.bands * row.wraps_per_band,
            row.wraps,
            "wrap count inconsistent for ({}, {})",
            row.cartridge_generation,
            row.recording_format,
        );
    }
}

/// Every row's stored source string is asserted verbatim, so a row
/// cannot change without its provenance changing with it. The identity
/// alone cannot catch an invented-but-self-consistent row; this can.
#[test]
fn every_row_carries_its_expected_source() {
    let expected: [(&str, &str, u32, u32, u32, u32, &str); 5] = [
        (
            "LTO-7",
            "L7",
            28,
            112,
            32,
            3584,
            "Track and channel counts as previously established from the HP LTO Ultrium \
             Technical Reference Manual lineage (Volume 4); design-read-ordering.md \u{a7}6.3.",
        ),
        (
            "LTO-8",
            "L8",
            52,
            208,
            32,
            6656,
            "Published LTO-8 specification tables and CERN EOSCTA operations notes; \
             design-read-ordering.md \u{a7}6.3.",
        ),
        (
            "LTO-9",
            "L9",
            70,
            280,
            32,
            8960,
            "HPE published LTO-9 figures; design-read-ordering.md \u{a7}6.3.",
        ),
        (
            "LTO-10",
            "LA",
            118,
            472,
            32,
            15104,
            "Published LTO-10 structural figures; media code LA per IBM TS4300 \
             'Bar code label' Table 2; design-read-ordering.md \u{a7}6.3.",
        ),
        (
            "LTO-10",
            "PA",
            118,
            472,
            32,
            15104,
            "Published LTO-10 structural figures; media code PA per IBM TS4300 \
             'Bar code label' Table 2; design-read-ordering.md \u{a7}6.3.",
        ),
    ];
    assert_eq!(STRUCTURAL_TABLE.len(), expected.len());
    for (row, (gen, fmt, wpb, wraps, channels, tracks, source)) in
        STRUCTURAL_TABLE.iter().zip(expected.iter())
    {
        assert_eq!(row.cartridge_generation, *gen);
        assert_eq!(row.recording_format, *fmt);
        assert_eq!(row.bands, 4);
        assert_eq!(row.wraps_per_band, *wpb);
        assert_eq!(row.wraps, *wraps);
        assert_eq!(row.channels, *channels);
        assert_eq!(row.data_tracks, *tracks);
        assert_eq!(row.source, *source, "source drifted for ({gen}, {fmt})");
    }
}

/// Supported keys resolve to their rows; LA and PA are distinct keys
/// even though their structural constants are identical today.
#[test]
fn supported_lookups_resolve() {
    for (gen, fmt) in [
        ("LTO-7", "L7"),
        ("LTO-8", "L8"),
        ("LTO-9", "L9"),
        ("LTO-10", "LA"),
        ("LTO-10", "PA"),
    ] {
        match lookup_geometry(gen, fmt) {
            GeometryLookup::Supported(row) => {
                assert_eq!(row.cartridge_generation, gen);
                assert_eq!(row.recording_format, fmt);
            }
            other => panic!("({gen}, {fmt}) resolved to {other:?}, expected Supported"),
        }
    }
    let la = match lookup_geometry("LTO-10", "LA") {
        GeometryLookup::Supported(r) => r,
        _ => unreachable!(),
    };
    let pa = match lookup_geometry("LTO-10", "PA") {
        GeometryLookup::Supported(r) => r,
        _ => unreachable!(),
    };
    assert_eq!(la.recording_format, "LA");
    assert_eq!(pa.recording_format, "PA");
}

/// Unsupported is a different answer from absent, and each unsupported
/// row carries its reason. M8 is recognised through either component of
/// the key.
#[test]
fn unsupported_is_distinguishable_from_absent() {
    // M8 in either component: recognised, refused, with the
    // conflicting-geometry reason.
    for (gen, fmt) in [("M8", "M8"), ("LTO-7", "M8"), ("LTO-8", "M8"), ("M8", "")] {
        match lookup_geometry(gen, fmt) {
            GeometryLookup::Unsupported(row) => {
                assert!(
                    row.reason
                        .contains("Conflicting published structural geometry"),
                    "M8 reason missing for ({gen}, {fmt})"
                );
            }
            other => panic!("({gen}, {fmt}) resolved to {other:?}, expected Unsupported"),
        }
    }
    // Pre-REOWP generations: recognised, refused, with the REOWP reason.
    for (gen, fmt) in [
        ("LTO-6", "L6"),
        ("LTO-5", "L5"),
        ("LTO-4", "L4"),
        ("LTO-3", "L3"),
        ("LTO-2", "L2"),
        ("LTO-1", "L1"),
    ] {
        match lookup_geometry(gen, fmt) {
            GeometryLookup::Unsupported(row) => {
                assert!(
                    row.reason.contains("READ END OF WRAP POSITION"),
                    "pre-REOWP reason missing for ({gen}, {fmt})"
                );
            }
            other => panic!("({gen}, {fmt}) resolved to {other:?}, expected Unsupported"),
        }
    }
    // Unknown keys and impossible cross-generation pairs are Absent —
    // nobody invents a row for them.
    for (gen, fmt) in [
        ("LTO-11", "LB"),
        ("", ""),
        ("LTO-7", "L8"),
        ("LTO-9", "L7"),
        ("DDS-4", "D4"),
    ] {
        assert_eq!(
            lookup_geometry(gen, fmt),
            GeometryLookup::Absent,
            "({gen}, {fmt}) should be Absent"
        );
    }
    // Every unsupported row states a reason and a source.
    for row in &UNSUPPORTED_TABLE {
        assert!(!row.reason.is_empty());
        assert!(!row.source.is_empty());
    }
}

/// Every supported media code in the canonical §6.3 table resolves to
/// its stated geometry key; WORM codes normalise to the data medium's
/// structural row; `M8` is recognised but unsupported; an unrecognised
/// suffix is Absent.
#[test]
fn media_code_table_resolves_to_the_stated_geometry_keys() {
    let expectations = [
        ("L7", "LTO-7", "L7"),
        ("LX", "LTO-7", "L7"),
        ("L8", "LTO-8", "L8"),
        ("LY", "LTO-8", "L8"),
        ("L9", "LTO-9", "L9"),
        ("LZ", "LTO-9", "L9"),
        ("LA", "LTO-10", "LA"),
        ("LH", "LTO-10", "LA"),
        ("PA", "LTO-10", "PA"),
    ];
    for (code, gen, fmt) in expectations {
        match lookup_media_code(code) {
            GeometryLookup::Supported(row) => {
                assert_eq!(row.cartridge_generation, gen, "media code {code}");
                assert_eq!(row.recording_format, fmt, "media code {code}");
            }
            other => panic!("media code {code} resolved to {other:?}, expected Supported"),
        }
    }
    // M8 is recognised but unsupported — distinguishable from Absent.
    match lookup_media_code("M8") {
        GeometryLookup::Unsupported(row) => {
            assert_eq!(row.cartridge_generation, "M8");
        }
        other => panic!("M8 resolved to {other:?}, expected Unsupported"),
    }
    // Unrecognised suffixes are Absent, including case mismatches —
    // the caller normalises, this table does not guess.
    for code in ["LB", "l8", "XX", "", "L", "L8 "] {
        assert_eq!(
            lookup_media_code(code),
            GeometryLookup::Absent,
            "media code {code:?} should be Absent"
        );
    }
    // Every table row round-trips through lookup_media_code without
    // inventing a pair the structural tables cannot answer.
    for row in &MEDIA_CODE_TABLE {
        assert!(
            !matches!(lookup_media_code(row.media_code), GeometryLookup::Absent),
            "canonical media code {} must resolve",
            row.media_code
        );
    }
}

/// Band layout is `[3, 1, 0, 2]` and band 0 sits at physical rank 2.
/// Layout is a different fact from fill order; the fill-order half lives
/// in the mapping tests.
#[test]
fn band_layout_and_ranks() {
    assert_eq!(BAND_LAYOUT, [3, 1, 0, 2]);
    assert_eq!(band_rank(0), Some(2));
    assert_eq!(band_rank(1), Some(1));
    assert_eq!(band_rank(2), Some(3));
    assert_eq!(band_rank(3), Some(0));
    assert_eq!(band_rank(4), None);
}
