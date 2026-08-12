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
mod reconcile;
mod restore;
mod robotics;
mod terminal_inventory;
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
const LOAD_READY_TIMEOUT: StdDuration = StdDuration::from_secs(9_000);
const LOAD_READY_POLL_INTERVAL: StdDuration = StdDuration::from_secs(30);

mod actor_protocol;
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
    Timer,
    Shutdown,
}

#[derive(Debug)]
pub(crate) struct CheckpointActorReply {
    pub(crate) session: pb::WriteSession,
    pub(crate) committed_objects: Vec<pb::ObjectRecord>,
}

fn send_checkpoint_actor_reply(
    reply: oneshot::Sender<Result<CheckpointActorReply, Status>>,
    session: pb::WriteSession,
    committed_receipts: &mut Vec<pb::ObjectRecord>,
) {
    let committed_objects = std::mem::take(committed_receipts);
    if let Err(Ok(unsent)) = reply.send(Ok(CheckpointActorReply {
        session,
        committed_objects,
    })) {
        *committed_receipts = unsent.committed_objects;
    }
}

/// Retain a catalog-replayed object until an explicit checkpoint can claim its durable copy.
///
/// A replay has no pending checkpoint batch because its copy was committed by an earlier
/// session. Append still reports that durable record to the current caller, whose batch contract
/// releases it only from `CheckpointSession`; coalescing by object id also prevents duplicate
/// receipts when the same replay arrives before that checkpoint.
fn retain_replayed_committed_receipt(
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

mod terminal_finalize;
pub(crate) use terminal_finalize::*;

mod read_session;

#[cfg(test)]
mod tests;
