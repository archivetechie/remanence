//! Resume content refusals retain their classification separately from I/O failures.
use crate::StateError;
use remanence_parity::ParityError;
/// Why the combined durable authorities refused a resume.
#[derive(Debug)]
pub enum AuthorityAgreementError {
    /// The commit records are missing, conflict, or are incomplete or
    /// ambiguous (REM-PARITY 3.4): reported as `ResumeAppend` with its detail.
    Records(ParityError),
    /// An operational failure reading the checkpoint journal, which keeps
    /// its own error, as `resume_record_result` does for the parity journal.
    State(StateError),
}

impl From<StateError> for AuthorityAgreementError {
    fn from(error: StateError) -> Self {
        match error {
            StateError::JournalReplayFailed(ref detail)
                if detail == "parity journal has a committed prefix but checkpoint authority is empty" =>
                Self::Records(ParityError::ResumeAppend(detail.clone())),
            StateError::JournalReplayFailed(_) => Self::Records(ParityError::ResumeAppend(error.to_string())),
            other => Self::State(other),
        }
    }
}
