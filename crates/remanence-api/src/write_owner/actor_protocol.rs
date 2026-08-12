//! Commands and public request/response types for drive and changer actors.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use remanence_library::{MediaFamily, MediaReadinessWaitOptions};
use remanence_state::{DriveHealthSnapshotRecord, TapePoolConfig};
use tokio::sync::{mpsc, oneshot};
use tonic::Status;
use uuid::Uuid;

use super::{
    AppendFinishOutcome, CheckpointActorReply, CheckpointTrigger, CloseWriteActorReply,
    DriveReservation, ManualFinalizeTapeActorReply, ManualFinalizeTapeActorRequest, SelectedTape,
};
use crate::pb;

/// Session-independent coordinates used to position a newly minted read
/// session at a catalogued file boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReadResumeTarget {
    pub(crate) tape_uuid: [u8; 16],
    pub(crate) object_id: String,
    pub(crate) file_id: String,
    pub(crate) file_boundary_byte_offset: u64,
    pub(crate) expected_position_lba: Option<u64>,
    pub(crate) prior_daemon_epoch: Option<u64>,
}

/// Robotics work to perform after the owner opens and refreshes the library.
pub(crate) enum RoboticsAction {
    Refresh,
    Move {
        src: u16,
        dst: u16,
    },
    Load {
        slot: u16,
        bay: u16,
        wait_ready: bool,
    },
    Unload {
        bay: u16,
        destination: Option<u16>,
    },
    Clean {
        drive_uuid: Vec<u8>,
        trigger: String,
    },
}

pub(crate) enum ChangerCommand {
    Move {
        src: u16,
        dst: u16,
        reply: oneshot::Sender<Result<(), Status>>,
    },
    #[expect(dead_code, reason = "Phase 3a command shape includes explicit refresh")]
    Refresh {
        reply: oneshot::Sender<Result<(), Status>>,
    },
    Reconcile {
        tape_uuid: [u8; 16],
        handle: crate::operations::OperationHandle,
    },
    Robotics {
        library_serial: String,
        action: RoboticsAction,
        handle: crate::operations::OperationHandle,
    },
}

pub(crate) enum DriveCommand {
    WaitReady {
        operation_id: Uuid,
        family: MediaFamily,
        options: MediaReadinessWaitOptions,
        handle: crate::operations::OperationHandle,
        reservation: DriveReservation,
    },
    OpenWrite {
        pool_cfg: TapePoolConfig,
        selected: SelectedTape,
        target_kind: pb::write_session::TargetKind,
        needs_drive_load: bool,
        library_serial: String,
        barcode: Option<String>,
        source_slot: Option<u16>,
        drive_uuid: Option<Vec<u8>>,
        drive_serial: Option<String>,
        reply: oneshot::Sender<Result<pb::WriteSession, Status>>,
    },
    OpenRead {
        tape_uuid: [u8; 16],
        needs_drive_load: bool,
        library_serial: String,
        barcode: Option<String>,
        source_slot: Option<u16>,
        drive_uuid: Option<Vec<u8>>,
        drive_serial: Option<String>,
        resume_target: Option<ReadResumeTarget>,
        daemon_epoch: u64,
        reply: oneshot::Sender<Result<pb::ReadSession, Status>>,
    },
    TapeInventory {
        tape_uuid: [u8; 16],
        needs_drive_load: bool,
        library_serial: String,
        barcode: Option<String>,
        source_slot: Option<u16>,
        drive_serial: Option<String>,
        stream_tx: mpsc::Sender<Result<pb::TapeInventoryStreamItem, Status>>,
        reply: oneshot::Sender<Result<(), Status>>,
    },
    VerifyTapeIndex {
        tape_uuid: [u8; 16],
        needs_drive_load: bool,
        library_serial: String,
        barcode: Option<String>,
        source_slot: Option<u16>,
        drive_serial: Option<String>,
        reply: oneshot::Sender<Result<pb::TapeIndexVerification, Status>>,
    },
    FinalizeTape {
        request: ManualFinalizeTapeActorRequest,
        needs_drive_load: bool,
        library_serial: String,
        barcode: Option<String>,
        source_slot: Option<u16>,
        drive_uuid: Option<Vec<u8>>,
        drive_serial: Option<String>,
        reply: oneshot::Sender<Result<ManualFinalizeTapeActorReply, Status>>,
    },
    Unload {
        reply: oneshot::Sender<Result<StdDuration, Status>>,
    },
    PollHealth {
        drive_uuid: Vec<u8>,
        trigger: &'static str,
        session_id: Option<Uuid>,
        tape_uuid: Option<[u8; 16]>,
        reply: oneshot::Sender<Result<DriveHealthSnapshotRecord, Status>>,
    },
    Heartbeat {
        drive_uuid: Vec<u8>,
        reply: oneshot::Sender<Result<(), Status>>,
    },
    AppendFinish {
        session_id: Uuid,
        source: crate::WriteObjectSource,
        archive_path: PathBuf,
        caller_object_id: String,
        expected_content_sha256: Option<[u8; 32]>,
        expected_object_id: Option<[u8; 16]>,
        input_kind: crate::WriteObjectInputKind,
        live_write_counter: Option<Arc<crate::DriveByteCounters>>,
        reply: oneshot::Sender<Result<AppendFinishOutcome, Status>>,
    },
    Checkpoint {
        session_id: Uuid,
        trigger: CheckpointTrigger,
        expected_batch_id: Option<Uuid>,
        reply: Option<oneshot::Sender<Result<CheckpointActorReply, Status>>>,
    },
    TimerIdleClose {
        session_id: Uuid,
        checkpoint_batch_id: Uuid,
    },
    Close {
        session_id: Uuid,
        reply: oneshot::Sender<Result<CloseWriteActorReply, Status>>,
    },
    Abort {
        session_id: Uuid,
        /// The caller's stated reason, when it gave one. Absent is an abort
        /// with no explanation -- common from a client that is itself dying --
        /// and is recorded as the absence of the audit key, not as "".
        reason: Option<String>,
        reply: oneshot::Sender<Result<CloseWriteActorReply, Status>>,
    },
    Get {
        session_id: Uuid,
        reply: oneshot::Sender<Result<pb::WriteSession, Status>>,
    },
    ReadFile {
        session_id: Uuid,
        object_id: String,
        file_id: Vec<u8>,
        stream_chunk_bytes: u32,
        chunk_tx: crate::read_core::ReadStreamSender,
    },
    ReadObjectRange {
        session_id: Uuid,
        object_id: String,
        file_id: String,
        start_byte: u64,
        end_byte: u64,
        stream_chunk_bytes: u32,
        chunk_tx: crate::read_core::ReadStreamSender,
    },
    CloseRead {
        session_id: Uuid,
        reply: oneshot::Sender<Result<pb::ReadSession, Status>>,
    },
    GetRead {
        session_id: Uuid,
        reply: oneshot::Sender<Result<pb::ReadSession, Status>>,
    },
}
