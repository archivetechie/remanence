//! Pin the classifier's tape-scoped cause vocabulary to library-log v1.
use remanence_library::outcome::FenceCause as Device;
use remanence_state::library_log::FenceCause as Wire;

/// Exhaustive match makes an added device cause require an explicit wire mapping.
fn wire(cause: Device) -> Wire {
    match cause {
        Device::PartialBatch => Wire::PartialBatch,
        Device::TransferError => Wire::TransferError,
        Device::CloseoutCheckMismatch => Wire::CloseoutCheckMismatch,
        Device::SupersedeTailFailed => Wire::SupersedeTailFailed,
        Device::StandaloneWriteLostByRestart => Wire::StandaloneWriteLostByRestart,
        Device::AppendAdmissionRefused => Wire::AppendAdmissionRefused,
        Device::ResetUnitAttention => Wire::ResetUnitAttention,
        Device::PositionDrift => Wire::PositionDrift,
        Device::AuthorityAmbiguous => Wire::AuthorityAmbiguous,
        Device::SessionSuffixContradiction => Wire::SessionSuffixContradiction,
        Device::CloseoutReadContradiction => Wire::CloseoutReadContradiction,
    }
}
/// Names and numeric order match all eleven device causes in the frozen wire enum.
#[test]
fn b3b_fence_cause_matches_library_log() {
    let causes = [
        Device::PartialBatch,
        Device::TransferError,
        Device::CloseoutCheckMismatch,
        Device::SupersedeTailFailed,
        Device::StandaloneWriteLostByRestart,
        Device::AppendAdmissionRefused,
        Device::ResetUnitAttention,
        Device::PositionDrift,
        Device::AuthorityAmbiguous,
        Device::SessionSuffixContradiction,
        Device::CloseoutReadContradiction,
    ];
    for (code, cause) in causes.into_iter().enumerate() {
        let encoded = wire(cause);
        assert_eq!(cause as usize, code);
        assert_eq!(encoded as usize, code);
        assert_eq!(format!("{cause:?}"), format!("{encoded:?}"));
    }
}
