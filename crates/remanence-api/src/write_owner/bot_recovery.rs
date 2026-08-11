//! Production binding between BOT scanning and durable checkpoint authority.
//!
//! Foreign or catalogless tapes have no local checkpoint journal and retain
//! the parity layer's honest `Unknown` classifications. When a journal exists,
//! its exclusive bounded replay can recover exact Object identities without
//! consulting the rebuildable SQLite catalog.

use std::path::Path;

use remanence_parity::{
    recover_terminal_inventory_from_bot_controlled,
    recover_terminal_inventory_from_bot_with_authority_controlled, verify_terminal_index_full,
    verify_terminal_index_full_with_authority, BotRecoveredObject, BotStructuralRecoveryError,
    BotStructuralRecoveryEvent, BotStructuralRecoverySummary, RawTapeSource, ScanWalkControl,
    TerminalIndexVerificationError, TerminalIndexVerificationOutcome,
};
use remanence_state::CheckpointBotRecoveryAuthority;

/// Run checkpoint-assisted BOT recovery with bounded progress and cancellation.
pub(super) fn recover_terminal_inventory_with_checkpoint_authority_controlled<C, F>(
    source: &mut dyn RawTapeSource,
    checkpoint_journal_dir: &Path,
    tape_uuid: &[u8; 16],
    block_size: u32,
    visit_control: C,
    visit_object: F,
) -> Result<BotStructuralRecoverySummary, BotStructuralRecoveryError>
where
    C: FnMut(&BotStructuralRecoveryEvent) -> ScanWalkControl,
    F: FnMut(&BotRecoveredObject) -> Result<(), String>,
{
    let authority =
        CheckpointBotRecoveryAuthority::try_open(checkpoint_journal_dir, *tape_uuid, block_size)
            .map_err(|error| BotStructuralRecoveryError::ObjectAuthority {
                message: error.to_string(),
            })?;
    match authority {
        Some(mut authority) => recover_terminal_inventory_from_bot_with_authority_controlled(
            source,
            tape_uuid,
            block_size,
            &mut authority,
            visit_control,
            visit_object,
        ),
        None => recover_terminal_inventory_from_bot_controlled(
            source,
            tape_uuid,
            block_size,
            visit_control,
            visit_object,
        ),
    }
}

/// Run full terminal verification with checkpoint identity authority for its
/// all-replicas-invalid recovery outcome when that authority exists.
pub(super) fn verify_terminal_index_with_checkpoint_authority(
    source: &mut dyn RawTapeSource,
    checkpoint_journal_dir: &Path,
    tape_uuid: &[u8; 16],
    block_size: u32,
) -> Result<TerminalIndexVerificationOutcome, TerminalIndexVerificationError> {
    let authority =
        CheckpointBotRecoveryAuthority::try_open(checkpoint_journal_dir, *tape_uuid, block_size)
            .map_err(|error| TerminalIndexVerificationError::RecoveryAuthority {
                message: error.to_string(),
            })?;
    match authority {
        Some(mut authority) => {
            verify_terminal_index_full_with_authority(source, tape_uuid, block_size, &mut authority)
        }
        None => verify_terminal_index_full(source, tape_uuid, block_size),
    }
}
