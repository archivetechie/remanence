//! Bounded checkpoint-journal authority for beginning-of-tape recovery.
//!
//! A recovery owner holds the journal's exclusive lease while the parity layer
//! scans physical tape and replays Object identity rows. SQLite is deliberately
//! absent: the fsynced checkpoint journal is the durable source of truth.

use std::path::Path;

use remanence_parity::{
    BotObjectRecoveryAuthority, BotObjectRecoveryAuthorityRow, BotObjectRecoveryAuthorityScope,
    BotStructuralRecoveryError,
};

use super::{TapeAuthority, TapeAuthorityLease, TapeAuthorityView};
use crate::StateError;
use remanence_parity::ObjectRecoveryRow;

/// Frozen checkpoint authority retained across a complete BOT recovery pass.
#[derive(Debug)]
pub struct BotRecoveryAuthority {
    lease: TapeAuthorityLease,
    tape_uuid: [u8; 16],
    block_size: u32,
}

impl BotRecoveryAuthority {
    /// Open existing checkpoint authority without creating a journal for a
    /// foreign or catalogless tape.
    pub fn try_open(
        dir: impl AsRef<Path>,
        tape_uuid: [u8; 16],
        block_size: u32,
    ) -> Result<Option<Self>, StateError> {
        let dir = dir.as_ref();
        if !TapeAuthority::exists(dir, tape_uuid)? {
            return Ok(None);
        }
        let journal = TapeAuthority::open(dir, tape_uuid)?;
        let lease = match journal.acquire_exclusive() {
            Ok(lease) => lease,
            Err(ordinary_error) => {
                if journal.pending_finalization()?.is_none() {
                    return Err(ordinary_error);
                }
                journal.acquire_exclusive_for_terminal_recovery()?
            }
        };
        Ok(Some(Self {
            lease,
            tape_uuid,
            block_size,
        }))
    }
}

impl BotObjectRecoveryAuthority for BotRecoveryAuthority {
    fn visit_object_rows(
        &mut self,
        visitor: &mut dyn FnMut(
            &BotObjectRecoveryAuthorityRow,
        ) -> Result<(), BotStructuralRecoveryError>,
    ) -> Result<BotObjectRecoveryAuthorityScope, BotStructuralRecoveryError> {
        let mut visitor_error = None;
        let mut covered_prefix_tape_file_count = 1u64;
        let mut final_committed_object_count = 0u64;
        let mut emitted_object_count = 0u64;
        let replay = self.lease.visit_commits(&mut |record| {
            if record.block_size != self.block_size {
                return Err(StateError::JournalReplayFailed(format!(
                    "BOT recovery checkpoint block size {} differs from requested {}",
                    record.block_size, self.block_size
                )));
            }
            if record.sealed {
                return Ok(());
            }
            for object in &record.payloads {
                let row = authority_row(&object.object_recovery_row)?;
                if let Err(error) = visitor(&row) {
                    visitor_error = Some(error);
                    return Err(StateError::JournalReplayFailed(
                        "BOT recovery authority visitor rejected a row".to_string(),
                    ));
                }
                emitted_object_count = emitted_object_count.checked_add(1).ok_or_else(|| {
                    StateError::JournalReplayFailed(
                        "BOT recovery emitted Object count overflows u64".to_string(),
                    )
                })?;
            }
            covered_prefix_tape_file_count = record.next_file;
            final_committed_object_count = record.object_count;
            Ok(())
        });
        if let Some(error) = visitor_error {
            return Err(error);
        }
        replay.map_err(|error| BotStructuralRecoveryError::ObjectAuthority {
            message: error.to_string(),
        })?;
        if emitted_object_count != final_committed_object_count {
            return Err(BotStructuralRecoveryError::ObjectAuthority {
                message: format!(
                    "checkpoint replay emitted {emitted_object_count} Object rows but final ordinary authority reports {final_committed_object_count}"
                ),
            });
        }
        Ok(BotObjectRecoveryAuthorityScope {
            tape_uuid: self.tape_uuid,
            block_size: self.block_size,
            covered_prefix_tape_file_count,
            object_row_count: emitted_object_count,
        })
    }
}

fn authority_row(row: &ObjectRecoveryRow) -> Result<BotObjectRecoveryAuthorityRow, StateError> {
    let object_id = row.object_id.as_deref().unwrap_or_default();
    if object_id.is_empty() || object_id.len() > 64 || object_id.contains(&0) {
        return Err(StateError::JournalReplayFailed(format!(
            "BOT recovery Object identifier at tape file {} is not 1..=64 non-NUL bytes",
            row.tape_file_number
        )));
    }
    Ok(BotObjectRecoveryAuthorityRow {
        tape_file_number: row.tape_file_number,
        stored_block_count: row.stored_block_count,
        object_id: object_id.to_vec(),
    })
}
