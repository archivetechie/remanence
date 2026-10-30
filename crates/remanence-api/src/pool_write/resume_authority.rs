//! Combined parity/checkpoint authority validation at the resume boundary.

use remanence_parity::{FileTapeFileJournal, ParityError};

/// Why the combined durable authorities refused a resume.
#[derive(Debug)]
pub(crate) enum ResumeAuthorityRefusal {
    /// The commit records are missing, conflict, or are incomplete or
    /// ambiguous (REM-PARITY 3.4): reported as `ResumeAppend` with its detail.
    Records(ParityError),
    /// An operational failure reading the checkpoint journal, which keeps
    /// its own error, as `resume_record_result` does for the parity journal.
    State(remanence_state::StateError),
}

/// Validate the combined durable authorities only at the resume boundary.
/// Missing or conflicting claims retain their diagnostic as ResumeAppend;
/// operational failures propagate unchanged.
pub(crate) fn validate_parity_resume_checkpoint_authority(
    checkpoint: &remanence_state::FileCheckpointJournalLease,
    parity: &FileTapeFileJournal,
    checkpoint_is_empty: bool,
    committed_tape_file_count: u64,
) -> Result<(), ResumeAuthorityRefusal> {
    if checkpoint_is_empty {
        if committed_tape_file_count != 0 {
            return Err(ResumeAuthorityRefusal::Records(ParityError::ResumeAppend(
                "parity journal has a committed prefix but checkpoint authority is empty".into(),
            )));
        }
    } else {
        remanence_state::CheckpointTerminalIndexRecordSource::new_replay_backed(checkpoint, parity)
            .map_err(|error| match error {
                remanence_state::StateError::JournalReplayFailed(_) => {
                    ResumeAuthorityRefusal::Records(ParityError::ResumeAppend(error.to_string()))
                }
                other => ResumeAuthorityRefusal::State(other),
            })?;
    }
    Ok(())
}
