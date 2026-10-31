//! Combined parity/checkpoint authority validation at the resume boundary.

pub(crate) use remanence_state::authority::AuthorityAgreementError as ResumeAuthorityRefusal;

/// Validate the combined durable authorities only at the resume boundary.
/// Missing or conflicting claims retain their diagnostic as ResumeAppend;
/// operational failures propagate unchanged.
pub(crate) fn validate_parity_resume_checkpoint_authority(
    checkpoint: &remanence_state::TapeAuthorityLease,
    parity: &dyn remanence_parity::authority::StructuralAuthority,
    checkpoint_is_empty: bool,
    committed_tape_file_count: u64,
) -> Result<(), ResumeAuthorityRefusal> {
    checkpoint.validate_resume_rows(parity, checkpoint_is_empty, committed_tape_file_count)
}
