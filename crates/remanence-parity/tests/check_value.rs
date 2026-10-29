//! Shared writer vectors pin the CRC composition and non-consuming snapshots.

use remanence_parity::{check_value, data_shard_crc64, CheckValue, CheckValueBuilder};
use serde::Deserialize;

#[derive(Deserialize)]
struct Vectors {
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    block_crcs: Vec<String>,
    expected: String,
    block_pattern: Option<BlockPattern>,
}

#[derive(Deserialize)]
struct BlockPattern {
    block_count: usize,
    block_size: usize,
    block_multiplier: usize,
    modulus: usize,
}

fn vectors() -> Vec<Case> {
    serde_json::from_str::<Vectors>(include_str!(
        "../../../fixtures/rem-writer-check-value/vectors.json"
    ))
    .expect("valid writer check vectors")
    .cases
}

fn hex(value: &str) -> u64 {
    assert_eq!(value.len(), 16);
    assert!(value
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    u64::from_str_radix(value, 16).expect("hexadecimal CRC")
}

#[test]
fn shared_vectors_match_including_derived_block_crcs() {
    let cases = vectors();
    assert_eq!(cases.len(), 4);
    for case in cases {
        let crcs: Vec<_> = case.block_crcs.iter().map(|s| hex(s)).collect();
        if let Some(pattern) = case.block_pattern {
            assert_eq!(pattern.block_count, 4);
            assert_eq!(pattern.block_size, 256 * 1024);
            assert_eq!(crcs.len(), pattern.block_count);
            for (i, expected) in crcs.iter().enumerate() {
                let block: Vec<_> = (0..pattern.block_size)
                    .map(|j| ((i * pattern.block_multiplier + j) % pattern.modulus) as u8)
                    .collect();
                assert_eq!(
                    data_shard_crc64(&block),
                    *expected,
                    "{} block {i}",
                    case.name
                );
            }
        }
        assert_eq!(
            check_value(crcs),
            CheckValue(hex(&case.expected)),
            "{}",
            case.name
        );
    }
    assert_eq!(check_value([]), CheckValue::EMPTY);
}

#[test]
fn incremental_snapshots_match_every_prefix_and_allow_more_pushes() {
    for case in vectors() {
        let crcs: Vec<_> = case.block_crcs.iter().map(|s| hex(s)).collect();
        let mut builder = CheckValueBuilder::new();
        assert_eq!(builder.value(), CheckValue::EMPTY);
        assert_eq!(CheckValueBuilder::default().value(), CheckValue::EMPTY);
        for (i, crc) in crcs.iter().enumerate() {
            builder.push(*crc);
            let expected = check_value(crcs[..=i].iter().copied());
            assert_eq!(builder.value(), expected, "{} prefix {}", case.name, i + 1);
            assert_eq!(
                builder.value(),
                expected,
                "snapshot leaves the register unchanged"
            );
        }
        assert_eq!(builder.value(), CheckValue(hex(&case.expected)));
    }
}
