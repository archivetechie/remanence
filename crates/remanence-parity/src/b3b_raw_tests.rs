//! Strict-sense regression coverage for raw parity boundary adapters.
use super::*;
/// Three-byte sense cannot establish current medium damage for parity recovery.
#[test]
fn b3b_raw_medium_damage_rejects_truncated_sense() {
    let error =
        remanence_library::TapeIoError::new(remanence_library::TapeIoErrorKind::CheckCondition(
            remanence_library::ScsiError::CheckCondition {
                sense: vec![0x70, 0, 3],
                bytes_transferred: 0,
            },
        ));
    assert!(!tape_error_is_current_medium_damage(&error));
}
/// A truncated zero-transfer FILEMARK cannot become a raw boundary.
#[test]
fn b3b_raw_boundary_rejects_truncated_filemark() {
    let error =
        remanence_library::TapeIoError::new(remanence_library::TapeIoErrorKind::CheckCondition(
            remanence_library::ScsiError::CheckCondition {
                sense: vec![0x70, 0, 0x80],
                bytes_transferred: 0,
            },
        ));
    assert!(classify_read_boundary(&error).is_none());
}

/// Boundary, erasure and retry consumers borrow the same normalized adapter error.
#[test]
fn b3b_parity_boundary_consumer_census() {
    use remanence_library::outcome::{census, RetryCause};
    let error =
        remanence_library::TapeIoError::new(remanence_library::TapeIoErrorKind::CheckCondition(
            remanence_library::ScsiError::CheckCondition {
                sense: vec![0x72, 3, 0x11, 0],
                bytes_transferred: 0,
            },
        ));
    let before = (census::total(), census::normalizations());
    for _ in 0..3 {
        assert!(tape_error_is_current_medium_damage(&error));
        assert!(classify_read_boundary(&error).is_none());
        assert_eq!(RetryCause::tape_io(&error), RetryCause::Media);
    }
    assert_eq!((census::total(), census::normalizations()), before);
}
