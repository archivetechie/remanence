//! Drive/changer actor pool for Layer 5 read and write sessions.
//!
//! Phase 3b reserves individual drive bays for sessions while keeping
//! reconcile and robotics pool-exclusive.

use std::time::Duration as StdDuration;

use tokio::sync::oneshot;
use tonic::Status;

mod bot_recovery;
mod cleaning;
mod readiness;
#[cfg(test)]
pub(crate) use tests::{
    fail_readiness_actor, fail_readiness_test_command, run_readiness_test_command,
};
mod reconcile;
mod restore;
mod robotics;
pub(crate) mod terminal_inventory;
pub(crate) use restore::{status_from_pinned_tape_error, status_from_select_tape_error};

pub(crate) use crate::write_admission::{
    validate_provisional_replay_guards, WriteAdmissionCoordinator, WriteAdmissionReservation,
};

pub(crate) use crate::append_spool::Spool;

pub(crate) use crate::drive_pool::{
    DriveKey, DrivePool, DrivePoolLifecycle, DriveReservation, ExclusiveGuard, MountedSession,
    ParkedCartridge, SeatedCartridge, TapeReservation,
};
use crate::pool_write::SelectedTape;
use crate::{pb, TapeUuid};

pub(crate) const SPOOL_MAX_BYTES: u64 = crate::APPEND_SPOOL_MAX_BYTES;
const LOAD_READY_TIMEOUT: StdDuration =
    StdDuration::from_secs(crate::readiness_wait::DEFAULT_READY_TIMEOUT_SECONDS as u64);
const LOAD_READY_POLL_INTERVAL: StdDuration =
    StdDuration::from_secs(crate::readiness_wait::DEFAULT_READY_POLL_SECONDS as u64);

pub(crate) mod direct_replay_fault;
mod mounted_commands;
mod placement;
pub(crate) use placement::{ActorPlacement, LibraryActorContext, MountedDriveContext};
mod local_failure;
pub use local_failure::ActorFailure;
mod actor_protocol;
mod timers;
pub(crate) use actor_protocol::*;

mod terminal_types;
pub(crate) use terminal_types::*;

#[derive(Debug)]
pub(crate) struct AppendFinishOutcome {
    pub(crate) record: pb::ObjectRecord,
    pub(crate) replay: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CheckpointTrigger {
    Explicit,
    /// Internal append barrier preserves receipts for the caller.
    Append,
    Timer,
    Shutdown,
}

#[derive(Debug)]
pub(crate) struct CheckpointActorReply {
    pub(crate) session: pb::WriteSession,
    pub(crate) committed_objects: Vec<pb::ObjectRecord>,
}

/// Internal append barriers report durability while preserving caller-owned receipts.
fn send_checkpoint_actor_reply(
    reply: oneshot::Sender<Result<CheckpointActorReply, Status>>,
    session: pb::WriteSession,
    committed_receipts: &mut Vec<pb::ObjectRecord>,
    trigger: CheckpointTrigger,
) {
    if trigger == CheckpointTrigger::Append {
        let _ = reply.send(Ok(CheckpointActorReply {
            session,
            committed_objects: committed_receipts.clone(),
        }));
        return;
    }
    let committed_objects = std::mem::take(committed_receipts);
    if let Err(Ok(unsent)) = reply.send(Ok(CheckpointActorReply {
        session,
        committed_objects,
    })) {
        *committed_receipts = unsent.committed_objects;
    }
}

/// Retain each durable object once until a caller checkpoint or close claims its copy.
///
/// A replay has no pending checkpoint batch because its copy was committed by an earlier
/// session. Append still reports that durable record to the current caller, whose batch contract
/// releases it only from `CheckpointSession`; coalescing by object id also prevents duplicate
/// receipts when the same replay arrives before that checkpoint.
fn retain_committed_receipt(
    committed_receipts: &mut Vec<pb::ObjectRecord>,
    record: &pb::ObjectRecord,
) {
    if !committed_receipts
        .iter()
        .any(|committed| committed.object_id == record.object_id)
    {
        committed_receipts.push(record.clone());
    }
}

#[derive(Debug)]
pub(crate) struct CloseWriteActorReply {
    pub(crate) session: pb::WriteSession,
    pub(crate) diagnostics: CloseWriteActorDiagnostics,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CloseWriteActorDiagnostics {
    /// Synchronous object-closing filemark time accumulated by append calls.
    pub(crate) filemark_write_drain: StdDuration,
    /// Catalog/journal commit time accumulated after those filemarks.
    pub(crate) catalog_journal_fsync: StdDuration,
    /// Close-time health snapshot and projection work.
    pub(crate) drive_snapshot: StdDuration,
    /// Always zero for lazy session close; later dismount diagnostics own rewind time.
    pub(crate) rewind: StdDuration,
    /// Always zero for lazy session close; later dismount diagnostics own SSC UNLOAD time.
    pub(crate) ssc_unload: StdDuration,
    /// SessionClosed audit append/fsync and SQLite projection time.
    pub(crate) session_audit_projection: StdDuration,
}

mod actor_runtime;
pub(crate) use actor_runtime::*;

mod checkpoint;

pub(crate) struct SessionOpenReadinessContext<'a> {
    action: &'static str,
    bay: u16,
    library_serial: &'a str,
    barcode: Option<&'a str>,
    source_slot: Option<u16>,
    drive_serial: Option<&'a str>,
    needs_drive_load: bool,
}

mod write_session;
pub(crate) use write_session::raise_tape_io_fence;

mod terminal_finalize;
pub(crate) use terminal_finalize::preflight_manual_finalize_with_prefix;
pub(crate) use terminal_finalize::*;

mod read_session;

#[cfg(test)]
pub(crate) mod tests;

pub(crate) use actor_runtime::spawn_mounted_actor;

mod tail_recovery;
pub(crate) use tail_recovery::supersede_mounted_tail;
