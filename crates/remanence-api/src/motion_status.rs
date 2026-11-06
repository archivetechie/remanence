//! Preserve the command gate's closed authority-failure class across actor Status
//! replies. Only an actual healthy continuation-LOAD refusal permits return
//! motion during shutdown; display strings never supply that proof.
use remanence_library::motion::{MotionAuthorityError, MotionAuthorityFailure};
use tonic::Status;
const CLASS: &str = "remanence-motion-authority";
const CONTINUATION: &str = "remanence-continuation-load";
pub(crate) fn status(error: &MotionAuthorityError, continuation: bool) -> Status {
    let mut status = Status::failed_precondition(error.to_string());
    let class = match error.kind {
        MotionAuthorityFailure::SemanticRefusal => "semantic",
        MotionAuthorityFailure::StoreFailure => "store",
        MotionAuthorityFailure::ProjectionFailure => "projection",
        MotionAuthorityFailure::EvidenceFailure => "evidence",
        MotionAuthorityFailure::UnresolvedMotion => "unresolved",
    };
    status
        .metadata_mut()
        .insert(CLASS, class.parse().expect("closed ASCII class"));
    if continuation {
        status
            .metadata_mut()
            .insert(CONTINUATION, "1".parse().unwrap());
    }
    status
}
pub(crate) fn classified(status: &Status) -> Option<MotionAuthorityFailure> {
    Some(match status.metadata().get(CLASS)?.to_str().ok()? {
        "semantic" => MotionAuthorityFailure::SemanticRefusal,
        "store" => MotionAuthorityFailure::StoreFailure,
        "projection" => MotionAuthorityFailure::ProjectionFailure,
        "evidence" => MotionAuthorityFailure::EvidenceFailure,
        "unresolved" => MotionAuthorityFailure::UnresolvedMotion,
        _ => return None,
    })
}
pub(crate) fn continuation_return(status: &Status) -> bool {
    classified(status) == Some(MotionAuthorityFailure::SemanticRefusal)
        && status
            .metadata()
            .get(CONTINUATION)
            .is_some_and(|v| v == "1")
}
