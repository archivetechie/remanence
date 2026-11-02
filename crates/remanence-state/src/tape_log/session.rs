//! Native v2 session metadata and bounded references to streamed abandoned files.
//! Production writers publish these under the exclusive tape lease; the fold
//! binds every Object commit and named ending to its session.
use super::{CheckValue, CommitCursor, GroupRef};

/// A proved physical append boundary, including the dense file number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionCursor {
    /// SCSI partition.
    pub partition: u32,
    /// Physical logical block address.
    pub lba: u64,
    /// Next tape file.
    pub next_file: u64,
}
/// Durable start of an exclusive session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionOpened {
    /// Nonzero session UUID.
    pub session_id: [u8; 16],
    /// Tape incarnation, always one.
    pub incarnation: u64,
    /// Last media record sequence (required, including bootstrap).
    pub base_media_sequence: u64,
    /// Proved append boundary.
    pub cursor: SessionCursor,
    /// RFC3339 UTC time.
    pub opened_at_utc: String,
}
/// Why the session ended; wire discriminants follow this declaration order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEndReason {
    /// Completed close.
    CleanClose,
    /// Explicit abort.
    Abort,
    /// Append failed.
    FailedAppend,
    /// Client progress deadline expired.
    ClientStall,
    /// Drive command failed.
    DriveError,
    /// Graceful daemon shutdown.
    GracefulShutdown,
    /// Client disappeared.
    ClientDisconnected,
    /// Recovery proved a crashed session's suffix.
    CrashRecovered,
    /// Post-reply closeout failed.
    CloseoutFailed,
}
/// Observed outcome of the filemark command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TailFilemark {
    /// Accepted filemark, possibly immediate; synchronization is recorded separately.
    Present,
    /// No filemark was written.
    Absent,
    /// Filemark completion is unknown.
    Unknown,
}
/// Tail file kind and its format-native identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TailIdentity {
    /// Object id, the same bytes represented as UTF-8 by the catalog.
    Object(String),
    /// Canonical sidecar metadata SHA-256.
    Sidecar([u8; 32]),
    /// Hash of the exact observed blocks of a crash-torn final file. This is
    /// never a substitute for a live writer's known Object/sidecar identity.
    TornPrefix([u8; 32]),
}
/// One streamed file after the committed cursor; never retained in TapeSummary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TailFile {
    /// Dense physical file number.
    pub tape_file_number: u64,
    /// Kind and identity.
    pub identity: TailIdentity,
    /// Accepted blocks (GOOD completion does not imply synchronization).
    pub known_blocks: u64,
    /// Requested blocks whose completion is unknown.
    pub unknown_blocks: u64,
    /// Filemark outcome.
    pub filemark: TailFilemark,
    /// Prefix blocks proved present by synchronization or recovery readback.
    pub synchronized_blocks: u64,
    /// Closing filemark proved present by synchronization or recovery readback.
    pub synchronized_filemark: bool,
}
/// End metadata closing the preceding TailFile continuation stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionEnded {
    /// Session being ended.
    pub session_id: [u8; 16],
    /// Tape incarnation.
    pub incarnation: u64,
    /// Current media sequence, including commits within the session.
    pub base_media_sequence: u64,
    /// Current committed boundary; tail starts here.
    pub cursor: SessionCursor,
    /// RFC3339 UTC time.
    pub ended_at_utc: String,
    /// Ending cause.
    pub reason: SessionEndReason,
    /// Bound continuation group; empty for an empty tail.
    pub tail: GroupRef,
    /// Number of TailFile chunks in that group.
    pub tail_file_count: u64,
    /// Optional operator acknowledgement audit reference for crash recovery.
    pub crash_ack_audit_sequence: Option<u64>,
    /// Physical lower bound on the suffix, relative to cursor.lba.
    pub synchronized_tail_blocks: u64,
    /// Physical upper bound, including uncertain command completion.
    pub possible_tail_blocks: u64,
    /// Exact proved EOD recorded before reusing an ended session's tail.
    pub proved_eod: Option<u64>,
}
/// Barrier proof for precisely the current uncovered range and one sidecar row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClosedOut {
    /// Group containing exactly one sidecar structural row.
    pub group: GroupRef,
    /// Tape incarnation.
    pub incarnation: u64,
    /// Nonzero operation UUID.
    pub operation_id: [u8; 16],
    /// Session responsible for closeout, if any.
    pub session_id: Option<[u8; 16]>,
    /// Current media record sequence.
    pub base_media_sequence: u64,
    /// Exact current W.
    pub from_w: u64,
    /// Exact current T.
    pub to_t: u64,
    /// Current next epoch ID.
    pub epoch_id: u64,
    /// Current next physical file, occupied by the sidecar.
    pub sidecar_file_number: u64,
    /// Original-write check of the range before closeout.
    pub check_before: CheckValue,
    /// Physical cursor after the sidecar filemark.
    pub cursor_after: CommitCursor,
    /// Next file after the sidecar.
    pub next_file: u64,
    /// Next epoch after the sidecar.
    pub next_epoch: u64,
    /// RFC3339 UTC completion time.
    pub completed_at_utc: String,
}
