//! [`ParityError`] — Layer 3c's error vocabulary.
//!
//! Wraps [`remanence_library::TapeIoError`] from Layer 3a
//! (via `#[from]`); higher layers (format / Layer 5) wrap
//! `ParityError` in turn.

use remanence_library::TapeIoError;

use crate::capacity::CapacityReserveCause;
use crate::journal::JournalError;
use crate::model::StripeAddress;
use crate::raw::PhysicalPositionHint;

/// Errors a Layer 3c operation can return.
#[derive(Debug, thiserror::Error)]
pub enum ParityError {
    /// Underlying tape I/O failed (transport error, CHECK
    /// CONDITION, etc.) — propagated from the inner
    /// [`BlockSink`](remanence_library::BlockSink) /
    /// [`BlockSource`](remanence_library::BlockSource).
    #[error("tape I/O error: {0}")]
    TapeIo(#[from] TapeIoError),

    /// Caller-supplied [`ParityScheme`](crate::ParityScheme)
    /// failed validation (`m=0`, `k<2`, etc.) — see
    /// `docs/layer3c-design-v0.2.md` §11.3.
    #[error("invalid parity scheme: {0}")]
    InvalidScheme(String),

    /// Reed-Solomon library reported an internal error
    /// (typically a shape mismatch — should not happen if our
    /// stripe accounting is correct).
    #[error("Reed-Solomon error: {0}")]
    ReedSolomon(reed_solomon_erasure::Error),

    /// More than `m` blocks were missing from this stripe;
    /// reconstruction is impossible.
    #[error("stripe {stripe:?} unrecoverable: lost {lost_count} blocks (limit is {limit})")]
    Unrecoverable {
        /// Which stripe.
        stripe: StripeAddress,
        /// How many were missing.
        lost_count: u16,
        /// The scheme's tolerance.
        limit: u16,
    },

    /// Internal invariant violation. Should not occur in
    /// well-tested code; emitted at the boundary where a panic
    /// would otherwise be appropriate.
    #[error("invariant violation: {0}")]
    Invariant(&'static str),

    /// No bootstrap block could be found anywhere on the tape.
    /// The tape is effectively unreadable as a parity-protected
    /// volume — Layer 5 may opt to treat it as no-parity with
    /// an operator warning.
    #[error("bootstrap not found anywhere on tape")]
    NoBootstrapFound,

    /// Scanned the expected bootstrap region but found nothing
    /// valid at the given LBA. Discovery continues to the next
    /// expected position.
    #[error("no bootstrap found at expected position {0}")]
    NoBootstrapAtPosition(u64),

    /// CBOR / CRC / version error parsing a bootstrap block.
    #[error("bootstrap parse error: {0}")]
    BootstrapParse(String),

    /// A bootstrap payload serialized successfully but does not fit in one
    /// fixed-size bootstrap tape block.
    #[error(
        "bootstrap payload too large: framed length {framed_len} exceeds block size {block_size}"
    )]
    BootstrapPayloadTooLarge {
        /// Header + CBOR payload + payload CRC length.
        framed_len: usize,
        /// Fixed tape block size available for the bootstrap.
        block_size: usize,
    },

    /// CRC / version / shape error parsing a parity sidecar tape file.
    #[error("sidecar parse error: {0}")]
    SidecarParse(String),

    /// A map-valid parity sidecar has no usable primary or tail metadata copy,
    /// so only that epoch is unavailable for Layer 3c recovery.
    #[error(
        "sidecar metadata unavailable for epoch {epoch_id} (both header copies failed); only this epoch is parity-unavailable"
    )]
    SidecarMetadataUnavailable {
        /// Parity epoch whose sidecar metadata is unavailable.
        epoch_id: u64,
    },

    /// CRC / version / shape error parsing a parity-map tape file.
    #[error("parity-map parse error: {0}")]
    ParityMapParse(String),

    /// The bootstrap recorded a parity scheme the running reader
    /// doesn't support.
    #[error("parity scheme mismatch: bootstrap says {tape}, reader expects {expected}")]
    SchemeMismatch {
        /// Scheme ID on tape.
        tape: String,
        /// Scheme ID the reader expected.
        expected: String,
    },

    /// A catalog-less scan reconstructed a filemark map whose
    /// canonical projection does not match the selected bootstrap's
    /// digest.
    #[error(
        "reconstructed filemark map does not match bootstrap digest (walk truncation position: {truncation_position:?})"
    )]
    FilemarkMapDigestMismatch {
        /// First structurally incomplete tail position, when validation was
        /// attempted over a walk that terminated at a truncation signature.
        truncation_position: Option<PhysicalPositionHint>,
    },

    /// Filemark-map reconstruction or structural validation failed
    /// before digest comparison.
    #[error("filemark map could not be reconstructed: {0}")]
    FilemarkMapReconstruct(String),

    /// A recovery request is outside the tape-file prefix that an
    /// intermediate bootstrap authenticated.
    #[error("ordinal {ordinal} is outside validated map prefix ending at {prefix_ordinals}")]
    OutsideValidatedMapPrefix {
        /// Requested parity data ordinal.
        ordinal: u64,
        /// First ordinal outside the validated prefix.
        prefix_ordinals: u64,
    },

    /// A recovery request names object data whose epoch has not yet
    /// had a committed parity sidecar emitted.
    #[error("ordinal {failed_ordinal} is above parity protection watermark {watermark}")]
    UnrecoverablePendingEpoch {
        /// Requested parity data ordinal.
        failed_ordinal: u64,
        /// Highest protected ordinal from the catalog or bootstrap
        /// digest scope.
        watermark: u64,
    },

    /// Starting a new object would exceed the tape or local-spool reserve
    /// required by Layer 3c v0.4.4 §7.5.
    #[error("starting object would exceed required reserve: {cause:?}")]
    CapacityReserveExceeded {
        /// Which reserve failed; remedies differ for tape and spool.
        cause: CapacityReserveCause,
        /// Projected object body blocks supplied to `begin_object`.
        projected_object_blocks: u64,
        /// Remaining usable tape blocks. Present only for tape-capacity
        /// failures.
        remaining_blocks: Option<u64>,
        /// Required non-object reserve blocks. Present only for tape-capacity
        /// failures; callers add `projected_object_blocks` for the full need.
        reserve_blocks: Option<u64>,
        /// Remaining local parity spool bytes. Present only for spool-capacity
        /// failures.
        remaining_spool_bytes: Option<u64>,
        /// Required local parity spool bytes. Present only for spool-capacity
        /// failures.
        required_spool_bytes: Option<u64>,
    },

    /// The object cannot fit on an empty tape even before considering the
    /// current tape's remaining capacity. RAO objects do not span tapes;
    /// callers must split the object upstream.
    #[error("object too large for an empty tape: {projected_object_blocks} body blocks, empty tape holds {empty_tape_usable_blocks} usable blocks, reserve {required_reserve_blocks} blocks; split upstream")]
    ObjectTooLargeForEmptyTape {
        /// Projected object body blocks supplied to `begin_object`.
        projected_object_blocks: u64,
        /// Usable blocks on a freshly loaded empty tape under this session's
        /// capacity policy.
        empty_tape_usable_blocks: u64,
        /// Non-object reserve blocks needed after admitting the object.
        required_reserve_blocks: u64,
    },

    /// A bulk recovery plan would exceed the operator-configured recovery
    /// memory budget from Layer 3c §9.3 / addendum v0.2 §6.5.
    #[error(
        "bulk recovery plan needs {needed_bytes} bytes but the cap is {max_recovery_cache_bytes} bytes; allow_windowed_recovery={allow_windowed_recovery} (§9.3 -- enable windowed recovery or raise the cap)"
    )]
    RecoveryPlanExceedsMemoryBudget {
        /// Estimated peak bytes needed by the recovery plan.
        needed_bytes: u64,
        /// Operator-configured hard cache budget.
        max_recovery_cache_bytes: u64,
        /// Whether the caller allowed multi-window recovery.
        allow_windowed_recovery: bool,
    },

    /// Restart/append planning failed before a new write session could
    /// safely accept object data.
    #[error("resume append error: {0}")]
    ResumeAppend(String),

    /// Durable tape-file journal failed while committing or replaying a
    /// bundle.
    #[error("tape-file journal error: {0}")]
    Journal(#[from] JournalError),

    /// Write session failed to open before any BOT bootstrap could be written.
    #[error("write session cannot open: {0}")]
    SessionOpen(String),

    /// LTO hardware compression is enabled for a parity-protected write
    /// session.
    #[error("LTO hardware compression is enabled; parity-protected writes require it disabled")]
    DriveCompressionEnabled,

    /// Layer 3a could not read back the drive's effective compression mode.
    #[error("could not verify the drive's effective compression mode")]
    DriveCompressionModeUnknown,
}
