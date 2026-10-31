//! Unwired step-2 per-tape authority format. Storage remains unwired; no tape I/O.
//!
//! The fixed little-endian prefix is CRC protected before lengths are trusted;
//! structural and catalog sections have independent CRC-64/XZ trailers. The
//! body limit is 64 MiB, with 44 bytes of framing. Integer-keyed, definite-length
//! deterministic CBOR preserves binary fields as byte strings and permits a
//! catalog item to cross arbitrarily many frames. Unlike JSON byte arrays this
//! is compact; unlike a generic CBOR DOM it needs no object-sized allocation.
//! Field keys are their zero-based declaration order in `wire.rs`. Unknown keys,
//! tags, versions, nonminimal integers and trailing bytes are refused.
//!
//! Groups start at Initialized or a continuation and close at an authoritative
//! record. Their SHA-256 covers each preceding frame's prefix fields (without
//! CRC), structural bytes and payload bytes, in order. The closing reference
//! binds first sequence, byte offset, frame/byte/member counts and this digest.
//! Provisional frames are fdatasynced before the media barrier; the closing
//! record and its fsync follow the barrier and remain the commit point. Staging
//! without provisional frames needs no data sync. A failed stage data sync rolls
//! back the provisional suffix before any media work, without an ambiguity fence.
//! EOF-short prefixes must agree with every visible field. EOF-short bodies with
//! internal zeros followed by data are torn: such a frame cannot close a group.
//! Only a successful close advances the public summary. Catalog callbacks must
//! therefore stage output until close; they must not publish provisional rows.
//! Frames and group digests do not bind a tape UUID: B3a must validate the
//! file header and its tape identity before folding these bytes.
//! Session tags and step-6 semantics are reserved, not accepted by the fold.

pub mod authority;
pub mod codec;
pub mod fold;
#[cfg(any(test, feature = "fixtures"))]
pub mod measure;
pub mod payload;
mod rows;
pub mod snapshot;
pub mod store;
mod terminal_source;
mod wire;

pub use remanence_parity::authority::{SealDueReason, TerminalFinalizationTrigger};
pub use remanence_parity::{
    CheckProvenance, CheckValue, CommitCursor, ObjectRecoveryRow, ParityScheme, TapeFileEntry,
};
use remanence_parity::{SidecarEpochDirectoryEntry, TerminalTailLayout};

/// Host frame version; independent of every on-tape format version.
pub const VERSION: u16 = 1;
/// Maximum sum of structural and payload bytes per frame.
pub const MAX_FRAME_BODY: usize = 64 * 1024 * 1024;
/// Fixed prefix bytes, including its CRC.
pub const PREFIX_LEN: usize = 28;
/// Two independent section CRC trailers.
pub const FRAME_OVERHEAD: usize = PREFIX_LEN + 16;

macro_rules! tags {
    ($($name:ident = $value:literal, $doc:literal;)+) => {
        /// Stable frame discriminator. Reserved tags are never emitted in step 2.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        #[repr(u16)]
        pub enum Tag { $(#[doc = $doc] $name = $value,)+ }
        impl TryFrom<u16> for Tag {
            type Error = Fault;
            fn try_from(value: u16) -> Result<Self, Fault> {
                match value { $($value => Ok(Self::$name),)+ _ => Err(Fault::UnknownTag(value)) }
            }
        }
    }
}
tags! {
    Initialized = 1, "Provisional BOT and geometry.";
    PayloadContinuation = 2, "Provisional catalog, file row or directory chunk.";
    Committed = 3, "Ordinary media barrier closing a group.";
    TerminalPrefix = 4, "Distinct sidecar/ParityMap prefix barrier.";
    ComponentWritten = 5, "One exact terminal component barrier.";
    Sealed = 6, "Completed replica C and immutable edition binding.";
    FinalizationDecided = 7, "Decision and exact immutable plan together.";
    FinalizationPrepared = 8, "Provisional manual request; removed at step 6.";
    FinalizationPreparationRetired = 9, "Retire provisional request; removed at step 6.";
    FinalizationRecoveryRequired = 10, "Compatibility recovery flag; removed at step 6.";
    FinalizationPlanned = 16, "Reserved for step 6.";
    ClosedOut = 17, "Reserved for step 3.";
    SessionOpened = 18, "Reserved for step 3.";
    SessionEnded = 19, "Reserved for step 3.";
    UnprotectedAccepted = 20, "Reserved for step 6.";
}

/// Provisional initialization; incarnation is always one, reinit uses a new UUID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Initialized {
    /// Tape UUID as binary.
    pub tape_uuid: [u8; 16],
    /// Must be one.
    pub incarnation: u64,
    /// First sequence of this group.
    pub group: u64,
    /// Fixed tape block size.
    pub block_size: u32,
    /// None for parity-off.
    pub scheme: Option<ParityScheme>,
    /// The one-block file-zero BOT row.
    pub bot: TapeFileEntry,
}

/// Constant-size reference to a contiguous group or immutable plan stream.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GroupRef {
    /// First provisional sequence, or the closing sequence for an empty group.
    pub first_sequence: u64,
    /// Byte offset of that frame.
    pub start_offset: u64,
    /// Number of preceding provisional frames.
    pub frame_count: u64,
    /// Structural plus payload byte count of those frames.
    pub byte_count: u64,
    /// Catalog member rows, summed over objects.
    pub member_count: u64,
    /// SHA-256 binding all provisional frames.
    pub digest: [u8; 32],
}

/// One stream at a time, in physical file order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Chunk {
    /// A file row; Object must be followed by its catalog stream.
    FileRow(TapeFileEntry),
    /// CBOR catalog bytes in the payload section; chunks can split any item.
    Catalog {
        /// Physical Object file to which this stream belongs.
        tape_file_number: u64,
        /// Zero-based contiguous chunk number.
        ordinal: u64,
        /// Last chunk of this object's catalog stream.
        last: bool,
    },
    /// An immutable plan directory row, carried structurally.
    PlanDirectory(SidecarEpochDirectoryEntry),
}

/// Non-authoritative continuation envelope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Continuation {
    /// First provisional sequence of the owning group.
    pub group: u64,
    /// Stream discriminator and metadata.
    pub chunk: Chunk,
}

/// Complete media state at a barrier; file rows are in the named group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Committed {
    /// Exactly the preceding provisional frames.
    pub group: GroupRef,
    /// New append boundary.
    pub cursor: CommitCursor,
    /// Protected ordinal W.
    pub protected: u64,
    /// Committed ordinal T.
    pub total: u64,
    /// Check of [W,T), encoded as eight little-endian bytes.
    pub check: CheckValue,
    /// Provenance, carried through retained and rebuilt state.
    pub provenance: CheckProvenance,
    /// End of the rebuilt, still-unverified prefix. Present exactly while W is
    /// below that end; fresh writes do not extend it. Not a verification claim.
    pub unverified_end: Option<u64>,
    /// Next actual sidecar epoch ID.
    pub next_epoch: u64,
    /// Next physical file number.
    pub next_file: u64,
    /// Cumulative Object count.
    pub object_count: u64,
    /// Stable diagnostic batch identity.
    pub batch_id: [u8; 16],
    /// Diagnostic only; no step-3 session semantics.
    pub session_id: Option<[u8; 16]>,
    /// Obligation observed at this barrier.
    pub seal_due: Option<SealDueReason>,
}

/// Step-2 decision and immutable plan; the potentially large directory is a reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinalizationPlan {
    /// Streamed directory and prefix rows closed by this record.
    pub group: GroupRef,
    /// Admission trigger.
    pub trigger: TerminalFinalizationTrigger,
    /// Manual acceptance identity; automatic plans have none.
    pub manual: Option<remanence_parity::authority::ManualTerminalFinalizationIdentity>,
    /// Exact edition identity.
    pub edition_id: [u8; 16],
    /// Edition sequence.
    pub edition_sequence: u64,
    /// Canonical edition digest.
    pub edition_digest: [u8; 32],
    /// Writer diagnostic string.
    pub writer_version: String,
    /// RFC3339 timestamp.
    pub write_timestamp: String,
    /// Exact terminal tail.
    pub layout: TerminalTailLayout,
    /// End of the planned sidecar/map prefix; equals start of replica A.
    pub prefix_end: CommitCursor,
}

/// Structural record. Catalog bytes live separately, never in this enum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
    /// Provisional initialization.
    Initialized(Initialized),
    /// Provisional stream frame.
    PayloadContinuation(Continuation),
    /// Ordinary barrier.
    Committed(Committed),
    /// Terminal prefix barrier, binding the decision sequence.
    TerminalPrefix {
        /// Decision reference.
        decision: u64,
        /// Barrier evidence.
        commit: Committed,
    },
    /// One terminal component, indexed 0..5 in the immutable plan.
    ComponentWritten {
        /// Decision reference.
        decision: u64,
        /// Zero-based component.
        component: u8,
        /// Barrier evidence.
        commit: Committed,
    },
    /// Final C and completed plan binding.
    Sealed {
        /// Decision reference.
        decision: u64,
        /// Exact final cursor.
        cursor: CommitCursor,
        /// Edition digest.
        edition_digest: [u8; 32],
    },
    /// Combined automatic/accepted manual decision and plan.
    FinalizationDecided(Box<FinalizationPlan>),
    /// Provisional manual plan; removed at step 6.
    FinalizationPrepared(Box<FinalizationPlan>),
    /// Retirement of a provisional plan; removed at step 6.
    FinalizationPreparationRetired {
        /// Prepared record sequence.
        prepared: u64,
    },
    /// Existing recovery flag; removed at step 6.
    FinalizationRecoveryRequired {
        /// Decision or preparation sequence.
        plan: u64,
    },
}

/// Location and typed cause of refused authority. Every format/fold fault requires recovery.
#[derive(Debug, thiserror::Error)]
#[error("tape log recovery required at byte {offset}, sequence {sequence:?}: {fault}")]
pub struct TapeLogError {
    /// Absolute frame offset.
    pub offset: u64,
    /// Sequence if the prefix was validated.
    pub sequence: Option<u64>,
    /// Machine-readable failure.
    pub fault: Fault,
}

/// Format and grammar refusal causes.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Fault {
    /// Short nonzero prefix cannot establish integrity.
    #[error("incomplete prefix")]
    ShortPrefix,
    /// Garbage after an apparent zero-filled suffix.
    #[error("non-zero byte after zeros at byte {offset}")]
    NonZeroAfterZeros {
        /// Absolute offset of the first non-zero byte following the zero region.
        offset: u64,
    },
    /// CRC mismatch in a particular section.
    #[error("bad {0:?} CRC")]
    Crc(Section),
    /// Unsupported host version.
    #[error("unsupported version {0}")]
    Version(u16),
    /// Unknown tag.
    #[error("unknown tag {0}")]
    UnknownTag(u16),
    /// Reserved tag must not appear in step 2.
    #[error("reserved tag {0:?}")]
    ReservedTag(Tag),
    /// Length exceeds the frame budget.
    #[error("frame exceeds 64 MiB")]
    FrameTooLarge,
    /// Sequence is not contiguous, including regression.
    #[error("expected sequence {expected}, got {actual}")]
    Sequence {
        /// Expected value.
        expected: u64,
        /// Observed value.
        actual: u64,
    },
    /// Initialization cannot repeat.
    #[error("second Initialized")]
    SecondInitialized,
    /// Terminal authority needs an ordinary committed prefix.
    #[error("terminal record before first Committed")]
    TerminalFirst,
    /// Closing reference does not match preceding frames.
    #[error("missing or mismatched continuation group")]
    GroupMismatch,
    /// Typed invariant with stable diagnostic context.
    #[error("invalid record: {0}")]
    Invalid(&'static str),
    /// Checked counter or coordinate overflow.
    #[error("counter overflow")]
    Overflow,
}

/// Independently checksummed regions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    /// Fixed prefix.
    Prefix,
    /// Structural CBOR.
    Structural,
    /// Catalog bytes.
    Payload,
}

/// Fence reason type only: publication wiring belongs to B3b.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TapeLogFenceReason {
    /// Append/fsync outcome could have changed durable authority.
    AuthorityAmbiguous,
}

impl Record {
    /// Stable on-disk discriminator.
    pub fn tag(&self) -> Tag {
        match self {
            Self::Initialized(_) => Tag::Initialized,
            Self::PayloadContinuation(_) => Tag::PayloadContinuation,
            Self::Committed(_) => Tag::Committed,
            Self::TerminalPrefix { .. } => Tag::TerminalPrefix,
            Self::ComponentWritten { .. } => Tag::ComponentWritten,
            Self::Sealed { .. } => Tag::Sealed,
            Self::FinalizationDecided(_) => Tag::FinalizationDecided,
            Self::FinalizationPrepared(_) => Tag::FinalizationPrepared,
            Self::FinalizationPreparationRetired { .. } => Tag::FinalizationPreparationRetired,
            Self::FinalizationRecoveryRequired { .. } => Tag::FinalizationRecoveryRequired,
        }
    }
}

#[cfg(test)]
mod tests;
