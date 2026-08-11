//! Layer 3c — tape parity (Reed-Solomon erasure coding) for
//! Remanence.
//!
//! Sits between Layer 3a (the SSC primitive set on `DriveHandle`)
//! and Layer 3b (the pluggable body-format layer). Wraps the
//! [`BlockSink`](remanence_library::BlockSink) /
//! [`BlockSource`](remanence_library::BlockSource) traits 3b
//! consumes; on a clean read the wrapper is transparent, and on
//! medium-error / servo-damage the wrapper reconstructs missing
//! blocks from parity.
//!
//! See `docs/layer3c-design.md` for the current sidecar-only design.
//!
//! ### Crate position
//!
//! ```text
//! remanence-cli ──┐
//!                 ├─→ remanence-parity ──┐
//!                 ├─→ remanence-format ──┴─→ remanence-library ──→ remanence-scsi
//! remanence-api ──┘
//! ```
//!
//! `remanence-parity` and `remanence-format` are **true
//! siblings**: both depend on `remanence-library` (for the
//! `BlockSink` / `BlockSource` traits + `TapeIoError`), neither
//! depends on the other. Composition happens at the daemon level
//! (Layer 5).

#![warn(missing_docs)]
#![warn(unsafe_op_in_unsafe_fn)]

pub mod bootstrap;
pub mod bot_recovery;
pub mod capacity;
mod cbor;
pub mod codec;
mod diagnostic_text;
mod durable;
pub mod error;
pub mod filemark_map;
pub mod index_separation;
pub mod journal;
pub mod mapping;
pub mod model;
pub mod object_recovery;
pub mod parity_map;
pub mod raw;
pub mod recovery;
mod replicated_control;
pub mod resume;
pub mod scan;
pub mod sidecar;
pub mod sink;
pub mod source;
pub mod tape_index;
pub mod tape_index_replica;
pub mod terminal_inventory;
pub mod terminal_tail;
pub mod terminal_writer;

pub use bootstrap::{
    discover_bootstrap, discover_bootstrap_with_block_size,
    discover_bootstrap_with_candidate_block_sizes, expected_bootstrap_positions, BootstrapPayload,
    ParitySchemeRecord, BOOTSTRAP_HEADER_CRC_OFFSET, BOOTSTRAP_HEADER_LEN,
    DEFAULT_BOOTSTRAP_CANDIDATE_BLOCK_SIZES,
};
pub use bot_recovery::{
    recover_terminal_inventory_from_bot, recover_terminal_inventory_from_bot_with_authority,
    BotObjectRecoveryAuthority, BotObjectRecoveryAuthorityRow, BotObjectRecoveryAuthorityScope,
    BotRecoveredObject, BotRecoveredObjectState, BotStructuralRecoveryError,
    BotStructuralRecoverySummary,
};
pub use capacity::{
    CapacityReserveCause, CapacityReserveRemedy, TerminalTripleCloseInput,
    TerminalTripleCloseReport, TerminalTripleObjectReservation,
};
pub use diagnostic_text::escape_member_name;
pub use error::ParityError;
pub use filemark_map::{
    sole_bot_filemark_map_digest, FilemarkMap, FilemarkMapBuilder, FilemarkMapDigest, MapScope,
    ScopedFilemarkMap, TapeFileKind, TapeFileMapEntry, TapeFilePosition,
};
pub use index_separation::{
    derive_index_separation_footer_magic, derive_index_separation_header_magic,
    encode_index_separation_footer, encode_index_separation_header, index_separation_records,
    parse_index_separation_footer, parse_index_separation_header, plan_index_separation,
    validate_index_separation_full, validate_index_separation_pair, write_index_separation,
    IndexSeparationDescriptor, IndexSeparationError, IndexSeparationFooter, IndexSeparationHeader,
    IndexSeparationInteriorBlockSource, IndexSeparationObservation, IndexSeparationPlan,
    DEFAULT_INDEX_SEPARATION_BYTES, INDEX_SEPARATION_CRC_OFFSET, INDEX_SEPARATION_FRAME_LEN,
    INDEX_SEPARATION_SCHEMA_VERSION,
};
pub use journal::{
    validate_committed_bundle_shape, validate_trusted_journal_volume, BoundedJournalReplayMetrics,
    CommittedBundle, CommittedBundleKind, CommittedBundleShapeError, CommittedState,
    FileTapeFileJournal, FileTapeFileJournalCommittedReplay, FileTapeFileJournalCommittedSnapshot,
    FileTapeFileJournalReader, JournalError, TapeFileEntry, TapeFileJournal,
    TerminalComponentAuthorityRelation,
};
pub use mapping::data_shards_per_epoch;
pub use model::{
    FinalGeometry, ObjectParityState, ObjectParityStateUpdateRange, ParityScheme, RecoveryEvent,
    RecoveryOutcome, SchemeId, SidecarMetadataHealth, SidecarMetadataHealthEvent, StripeAddress,
    StripePosition, TransportRetryEvent,
};
pub use object_recovery::{ObjectRecoveryRepresentation, ObjectRecoveryRow};
pub use parity_map::{
    classify_parity_map_header_block, derive_parity_map_magic, encode_parity_map_tape_file,
    parity_map_directory_len_upper_bound, parity_map_payload_len_upper_bound,
    parse_parity_map_footer_block, parse_parity_map_header_block, parse_parity_map_tape_file,
    parse_parity_map_tape_file_with_unreadable_blocks, DecodedParityMapTapeFile,
    EncodedParityMapTapeFile, ParityMapCopyKind, ParityMapFooter, ParityMapHeader,
    ParityMapPayload, ParityMapReference, SidecarEpochDirectory, SidecarEpochDirectoryEntry,
    PARITY_MAP_CBOR_DIRECTORY_ENTRY_UPPER_BOUND_BYTES,
    PARITY_MAP_CBOR_DIRECTORY_FIXED_UPPER_BOUND_BYTES, PARITY_MAP_CBOR_FIXED_UPPER_BOUND_BYTES,
    PARITY_MAP_FOOTER_CRC_OFFSET, PARITY_MAP_FOOTER_LEN, PARITY_MAP_FOOTER_VERSION,
    PARITY_MAP_FORMAT_ID, PARITY_MAP_HEADER_CRC_OFFSET, PARITY_MAP_HEADER_LEN,
    PARITY_MAP_SCHEMA_VERSION, SIDECAR_DIRECTORY_FLAG_FINAL_PARTIAL_EPOCH,
    SIDECAR_DIRECTORY_FLAG_PRIMARY_KNOWN_GOOD, SIDECAR_DIRECTORY_FLAG_TAIL_KNOWN_GOOD,
};
pub use raw::{
    BlockSinkRawTapeSink, BlockSourceRawTapeSource, DriveHandleRawSink, DriveHandleRawSource,
    ImageDirectoryRawSource, PhysicalPositionHint, RawReadOutcome, RawTapeSink, RawTapeSource,
    RawWriteOutcome, SpaceFilemarksOutcome, TapeGeometryHint,
};
pub use recovery::{
    recover_object_block_from_sidecar, recover_ordinal_from_sidecar, SidecarRecoveryResult,
};
pub use resume::{
    checked_bounded_resume_summary, checked_checkpointed_terminal_close_summary,
    close_checkpointed_terminal_index_prefix, emit_resume_rebuilt_sidecars_to_raw,
    plan_checkpointed_terminal_index_close, rebuild_open_epoch_from_bounded_summary,
    BoundedResumeSummary, CheckpointedTerminalCloseSummary, CheckpointedTerminalPrefixCloseResult,
    ResumeAppendPlan, ResumeAppendResult, ResumeLiveEpochState, ResumeOpenEpochRebuild,
    ResumeRebuiltSidecar, ResumeSidecarPlan,
};
pub use scan::{
    acquire_filemark_map, acquire_filemark_map_with_report, scan_reconstruct_filemark_map,
    scan_reconstruct_filemark_map_with_report, validate_scan_reconstruction_with_report,
    CatalogFilemarkMapInput, FilemarkMapScanResult, ParityMapContentConflict,
    ParityMapSelectionKey, ScanBootstrapCandidate, ScanDamageKind, ScanDamagedRegion,
    ScanOverlaySource, ScanTailTruncation, ScanTailTruncationKind, ScanWalkResult,
    UnattestedTapeFile,
};
pub use sidecar::{
    checked_sidecar_index_capacity_layout, classify_sidecar_header_block, crc64_xz,
    data_shard_crc64, derive_sidecar_footer_magic, derive_sidecar_magic,
    encode_sidecar_index_blocks, encode_sidecar_tape_file, parity_block_position,
    parity_shard_crc64, parse_sidecar_footer_block, parse_sidecar_header_block,
    parse_sidecar_index_blocks, parse_sidecar_tape_file, DecodedSidecarIndex,
    DecodedSidecarTapeFile, EncodedSidecarIndex, EncodedSidecarTapeFile, ParityShardIndexEntry,
    SidecarCopyKind, SidecarDescriptor, SidecarFooter, SidecarHeader, SidecarIndex,
    SidecarIndexCapacityLayout, CRC64_XZ_CHECK_VALUE, DATA_CRC_ENTRY_LEN, PARITY_INDEX_ENTRY_LEN,
    SIDECAR_FOOTER_CRC_OFFSET, SIDECAR_FOOTER_LEN, SIDECAR_FOOTER_VERSION,
    SIDECAR_HEADER_CRC_OFFSET, SIDECAR_HEADER_LEN, SIDECAR_SCHEMA_VERSION,
};
pub use sink::{
    reconcile_terminal_prefix, BoundedResumeWriterSeed, CheckpointResult, CloseReason,
    ObjectCloseResult, ObjectWriteSummary, ParitySink, ParitySinkSessionState, SidecarTapeFile,
    SidecarWriteSummary, TerminalPrefixCloseResult, TerminalPrefixPlan,
    TerminalPrefixReconcileEvidence, TerminalTripleCapacityRuntimeState,
};
pub use source::{
    BulkRecoveryPolicy, ObjectParitySource, OpenTrust, ParityAuditHook, RecoveredOrdinalBlock,
    RecoveredOrdinalRange, RecoveredRegion,
};
pub use tape_index_replica::{
    checked_tape_index_payload_len, checked_tape_index_replica_layout,
    derive_tape_index_replica_footer_magic, derive_tape_index_replica_header_magic,
    encode_tape_index_bootstrap_footer, encode_tape_index_replica_header,
    parse_tape_index_bootstrap_footer, parse_tape_index_replica_header, plan_tape_index_edition,
    plan_tape_index_replica, validate_tape_index_replica_pair, validate_tape_index_replica_payload,
    write_tape_index_replica, TapeIndexBootstrapFooter, TapeIndexEditionDescriptor,
    TapeIndexEditionPlan, TapeIndexReplicaCounts, TapeIndexReplicaError, TapeIndexReplicaFileKind,
    TapeIndexReplicaHeader, TapeIndexReplicaLayout, TapeIndexReplicaMapEntry,
    TapeIndexReplicaObjectRow, TapeIndexReplicaObservation, TapeIndexReplicaPayloadBlockSource,
    TapeIndexReplicaPayloadSummary, TapeIndexReplicaPlan, TapeIndexReplicaRecordSource,
    TapeIndexReplicaScope, TAPE_INDEX_REPLICA_CRC_OFFSET, TAPE_INDEX_REPLICA_FRAME_LEN,
    TAPE_INDEX_REPLICA_SCHEMA_VERSION,
};
pub use terminal_inventory::{
    read_terminal_index_inventory, read_terminal_index_inventory_streamed,
    read_terminal_index_inventory_summary, verify_terminal_index_full,
    verify_terminal_index_full_with_authority, BotStructuralRecoveryReason,
    BotStructuralRecoveryRequired, TerminalIndexRecoveryRequired, TerminalIndexVerification,
    TerminalIndexVerificationError, TerminalIndexVerificationOutcome, TerminalInventoryOutcome,
    TerminalInventoryReadError, TerminalInventorySelection, TerminalInventoryStreamEvent,
    TerminalReplicaEvidence, TerminalReplicaFailure, TerminalReplicaFailureKind,
    TerminalSeparationEvidence,
};
pub use terminal_tail::{
    validate_terminal_index_block_size, TerminalTailComponentKind, TerminalTailComponentPlan,
    TerminalTailLayout, TerminalTailLayoutError, TerminalTailProgress, TERMINAL_INDEX_BLOCK_SIZES,
    TERMINAL_INDEX_REPLICA_COUNT, TERMINAL_INDEX_SEPARATION_COUNT, TERMINAL_TAIL_COMPONENT_COUNT,
};
pub use terminal_writer::{
    reconcile_terminal_tail_next, terminal_component_bundle, write_terminal_tail,
    write_terminal_tail_step, TerminalComponentCommit, TerminalComponentReconcileEvidence,
    TerminalTailAuthority, TerminalTailRunOutcome, TerminalTailStepOutcome, TerminalTailWriteError,
    TerminalTripleWritePlan,
};

// ====================================================================
// Configuration helpers
// ====================================================================

/// Caller's intent for parity on a write session: a specific
/// scheme, or `None` (write a no-parity tape per §11.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParityConfig {
    /// Use this scheme. The bootstrap will record it.
    Scheme(ParityScheme),
    /// Write the tape with no parity (FLAG_NO_PARITY bootstrap).
    /// Sidecar recovery is bypassed on read for these tapes.
    None,
}

/// Parse a CLI-style parity argument into a [`ParityConfig`].
///
/// Accepted forms (matches `docs/layer3c-design.md` §11.4):
///
/// - `"default"` → [`default_scheme`] at
///   [`DEFAULT_SCHEME_BLOCK_SIZE_BYTES`]
/// - `"conservative"` → [`conservative_scheme`] at
///   [`DEFAULT_SCHEME_BLOCK_SIZE_BYTES`]
/// - `"none"` → [`ParityConfig::None`]
/// - `"custom:k,m,S"` → custom scheme with the given k/m/S
///   integers (using [`SCHEME_ID_RS_CAUCHY_GF256_V1`]); rejects
///   invalid values via [`ParityScheme::validate`].
pub fn parse_parity_arg(arg: &str) -> Result<ParityConfig, ParityError> {
    let trimmed = arg.trim();
    match trimmed {
        "default" => Ok(ParityConfig::Scheme(default_scheme())),
        "conservative" => Ok(ParityConfig::Scheme(conservative_scheme())),
        "none" => Ok(ParityConfig::None),
        s if s.starts_with("custom:") => {
            let payload = &s["custom:".len()..];
            let parts: Vec<&str> = payload.split(',').collect();
            if parts.len() != 3 {
                return Err(ParityError::InvalidScheme(format!(
                    "custom:k,m,S expected; got {parts:?}"
                )));
            }
            let k: u16 = parts[0].trim().parse().map_err(|_| {
                ParityError::InvalidScheme(format!("custom: bad k = {:?}", parts[0]))
            })?;
            let m: u16 = parts[1].trim().parse().map_err(|_| {
                ParityError::InvalidScheme(format!("custom: bad m = {:?}", parts[1]))
            })?;
            let stripes: u32 = parts[2].trim().parse().map_err(|_| {
                ParityError::InvalidScheme(format!("custom: bad S = {:?}", parts[2]))
            })?;
            let scheme = ParityScheme {
                id: SchemeId::new_static(SCHEME_ID_RS_CAUCHY_GF256_V1),
                data_blocks_per_stripe: k,
                parity_blocks_per_stripe: m,
                stripes_per_neighborhood: stripes,
            };
            scheme.validate()?;
            Ok(ParityConfig::Scheme(scheme))
        }
        _ => Err(ParityError::InvalidScheme(format!(
            "unrecognised --parity value: {arg:?}; expected default, conservative, none, or custom:k,m,S"
        ))),
    }
}

/// Canonical scheme ID for the initial Reed-Solomon Cauchy
/// GF(2⁸) scheme. New parameter ranges or algorithm changes get
/// a new ID; this one is forever-stable on tape.
pub const SCHEME_ID_RS_CAUCHY_GF256_V1: &str = "rs-cauchy-gf256-v1";

const MIB: u64 = 1024 * 1024;

/// Fixed block size used by the compatibility [`default_scheme`] and
/// [`conservative_scheme`] helpers.
///
/// New writer integrations that already know the body format's fixed block
/// size should call [`default_scheme_for_block_size`] or
/// [`conservative_scheme_for_block_size`] directly.
pub const DEFAULT_SCHEME_BLOCK_SIZE_BYTES: u32 = 256 * 1024;

/// Default parity scheme for a known tape block size.
///
/// Layer 3c v0.4.4 makes `S` block-size-aware so the contiguous-loss
/// tolerance `S * m * block_size` stays near 512 MiB. At rem-object-v1's
/// 256 KiB default this yields RS(128, 4), `S = 512`; at 1 MiB it yields
/// `S = 128`, matching the legacy geometry.
pub fn default_scheme_for_block_size(block_size_bytes: u32) -> ParityScheme {
    ParityScheme {
        id: SchemeId::new_static(SCHEME_ID_RS_CAUCHY_GF256_V1),
        data_blocks_per_stripe: 128,
        parity_blocks_per_stripe: 4,
        stripes_per_neighborhood: stripes_for_tolerance(block_size_bytes, 512 * MIB, 4),
    }
}

/// Conservative parity scheme for a known tape block size.
///
/// This keeps the contiguous-loss tolerance near 384 MiB with RS(64, 6)
/// and about 9.4% parity overhead. At 256 KiB it yields `S = 256`.
pub fn conservative_scheme_for_block_size(block_size_bytes: u32) -> ParityScheme {
    ParityScheme {
        id: SchemeId::new_static(SCHEME_ID_RS_CAUCHY_GF256_V1),
        data_blocks_per_stripe: 64,
        parity_blocks_per_stripe: 6,
        stripes_per_neighborhood: stripes_for_tolerance(block_size_bytes, 384 * MIB, 6),
    }
}

/// Default parity scheme for new rem-object-v1 tapes.
///
/// Compatibility helper for callers that do not yet pass block size through
/// the configuration path. Uses [`DEFAULT_SCHEME_BLOCK_SIZE_BYTES`]; callers
/// with an explicit block size should use [`default_scheme_for_block_size`].
pub fn default_scheme() -> ParityScheme {
    default_scheme_for_block_size(DEFAULT_SCHEME_BLOCK_SIZE_BYTES)
}

/// Conservative parity scheme for new rem-object-v1 tapes.
///
/// Compatibility helper for callers that do not yet pass block size through
/// the configuration path. Uses [`DEFAULT_SCHEME_BLOCK_SIZE_BYTES`]; callers
/// with an explicit block size should use
/// [`conservative_scheme_for_block_size`].
pub fn conservative_scheme() -> ParityScheme {
    conservative_scheme_for_block_size(DEFAULT_SCHEME_BLOCK_SIZE_BYTES)
}

fn stripes_for_tolerance(
    block_size_bytes: u32,
    target_loss_bytes: u64,
    parity_blocks_per_stripe: u16,
) -> u32 {
    let denom = u64::from(block_size_bytes) * u64::from(parity_blocks_per_stripe);
    if denom == 0 {
        return 1;
    }
    let stripes = target_loss_bytes.div_ceil(denom).max(1);
    u32::try_from(stripes).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod parse_arg_tests {
    use super::*;

    #[test]
    fn parse_default() {
        let cfg = parse_parity_arg("default").unwrap();
        match cfg {
            ParityConfig::Scheme(s) => assert_eq!(s, default_scheme()),
            ParityConfig::None => panic!("expected Scheme"),
        }
    }

    #[test]
    fn parse_conservative() {
        let cfg = parse_parity_arg("conservative").unwrap();
        match cfg {
            ParityConfig::Scheme(s) => assert_eq!(s, conservative_scheme()),
            ParityConfig::None => panic!("expected Scheme"),
        }
    }

    #[test]
    fn parse_none() {
        match parse_parity_arg("none").unwrap() {
            ParityConfig::None => {}
            ParityConfig::Scheme(_) => panic!("expected None"),
        }
    }

    #[test]
    fn parse_custom_valid() {
        let cfg = parse_parity_arg("custom:8,2,4").unwrap();
        match cfg {
            ParityConfig::Scheme(s) => {
                assert_eq!(s.data_blocks_per_stripe, 8);
                assert_eq!(s.parity_blocks_per_stripe, 2);
                assert_eq!(s.stripes_per_neighborhood, 4);
            }
            ParityConfig::None => panic!("expected Scheme"),
        }
    }

    #[test]
    fn parse_custom_validates_scheme() {
        // m > k → InvalidScheme from validate.
        let err = parse_parity_arg("custom:4,5,1").unwrap_err();
        assert!(matches!(err, ParityError::InvalidScheme(_)));
    }

    #[test]
    fn parse_custom_rejects_bad_count_of_fields() {
        let err = parse_parity_arg("custom:4,5").unwrap_err();
        assert!(matches!(err, ParityError::InvalidScheme(_)));
    }

    #[test]
    fn parse_custom_rejects_non_numeric_field() {
        let err = parse_parity_arg("custom:k,5,1").unwrap_err();
        assert!(matches!(err, ParityError::InvalidScheme(_)));
    }

    #[test]
    fn block_size_aware_default_scales_to_tolerance_floor() {
        assert_eq!(
            default_scheme_for_block_size(256 * 1024).stripes_per_neighborhood,
            512
        );
        assert_eq!(
            default_scheme_for_block_size(512 * 1024).stripes_per_neighborhood,
            256
        );
        assert_eq!(
            default_scheme_for_block_size(1024 * 1024).stripes_per_neighborhood,
            128
        );
        assert_eq!(
            default_scheme_for_block_size(3 * 1024 * 1024).stripes_per_neighborhood,
            43,
            "ceiling division must not fall below the configured tolerance floor"
        );
    }

    #[test]
    fn block_size_aware_conservative_scales_to_tolerance_floor() {
        assert_eq!(
            conservative_scheme_for_block_size(256 * 1024).stripes_per_neighborhood,
            256
        );
        assert_eq!(
            conservative_scheme_for_block_size(512 * 1024).stripes_per_neighborhood,
            128
        );
        assert_eq!(
            conservative_scheme_for_block_size(1024 * 1024).stripes_per_neighborhood,
            64
        );
    }

    #[test]
    fn parse_unrecognised_value() {
        let err = parse_parity_arg("garbage").unwrap_err();
        assert!(matches!(err, ParityError::InvalidScheme(_)));
    }
}
