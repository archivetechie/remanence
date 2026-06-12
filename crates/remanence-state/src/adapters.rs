//! Typed audit adapters for lower Remanence layers.
//!
//! Layer 4 stores audit records in one generic hash-chained log, while Layer 2
//! and Layer 3c expose their own domain-specific event vocabularies. This
//! module is the boundary between those worlds: it maps lower-layer events to
//! stable Layer 4 [`AuditEventRecord`] values without relying on lossy free-form
//! strings for event, operation, outcome, warning, recovery, or health tags.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration as StdDuration, SystemTime};

use ciborium::value::Value as CborValue;
use remanence_library::{
    AuditEvent as LibraryAuditEvent, AuditOp as LibraryAuditOp,
    AuditOutcome as LibraryAuditOutcome, RescanWarning as LibraryRescanWarning, SpaceKind,
};
use remanence_parity::{
    ParityAuditHook, RecoveryEvent as ParityRecoveryEvent, RecoveryOutcome, SidecarMetadataHealth,
    SidecarMetadataHealthEvent, StripePosition,
};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::audit::{
    AuditActor, AuditEvent, AuditEventRecord, AuditReceipt, AuditSink, AuditSubject, SourceLayer,
};
use crate::error::StateError;

/// Stable tag for the four Layer 2 library audit event phases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer2AuditEventTag {
    /// A CDB is about to be issued.
    Started,
    /// Public operation was refused before dispatch.
    Refused,
    /// A CDB returned.
    Finished,
    /// Reconciliation emitted a non-fatal warning.
    Warning,
}

impl Layer2AuditEventTag {
    /// Return the stable serialized tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Refused => "refused",
            Self::Finished => "finished",
            Self::Warning => "warning",
        }
    }
}

/// Stable tag for Layer 2 public operation kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer2AuditOpTag {
    /// MOVE MEDIUM.
    Move,
    /// Composed slot-to-drive load.
    Load,
    /// Composed drive-to-slot unload.
    Unload,
    /// Export to an import/export element.
    Export,
    /// Import from an import/export element.
    Import,
    /// INITIALIZE ELEMENT STATUS plus reconciliation.
    Rescan,
    /// PREVENT MEDIUM REMOVAL.
    LockRemoval,
    /// ALLOW MEDIUM REMOVAL.
    AllowRemoval,
    /// Drive-side UNLOAD.
    DriveUnload,
    /// Drive-side LOAD.
    DriveLoad,
    /// SSC REWIND.
    TapeRewind,
    /// SSC READ POSITION.
    TapeReadPosition,
    /// SSC LOCATE.
    TapeLocate,
    /// SSC SPACE.
    TapeSpace,
    /// SSC READ.
    TapeRead,
    /// SSC WRITE.
    TapeWrite,
    /// SSC WRITE FILEMARKS.
    TapeWriteFilemarks,
    /// Tape configuration read.
    TapeReadConfig,
    /// Tape configuration write.
    TapeWriteConfig,
}

impl Layer2AuditOpTag {
    /// Return the stable serialized tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Move => "move",
            Self::Load => "load",
            Self::Unload => "unload",
            Self::Export => "export",
            Self::Import => "import",
            Self::Rescan => "rescan",
            Self::LockRemoval => "lock_removal",
            Self::AllowRemoval => "allow_removal",
            Self::DriveUnload => "drive_unload",
            Self::DriveLoad => "drive_load",
            Self::TapeRewind => "tape_rewind",
            Self::TapeReadPosition => "tape_read_position",
            Self::TapeLocate => "tape_locate",
            Self::TapeSpace => "tape_space",
            Self::TapeRead => "tape_read",
            Self::TapeWrite => "tape_write",
            Self::TapeWriteFilemarks => "tape_write_filemarks",
            Self::TapeReadConfig => "tape_read_config",
            Self::TapeWriteConfig => "tape_write_config",
        }
    }

    /// Convert a Layer 2 operation into its stable tag.
    pub const fn from_operation(operation: &LibraryAuditOp) -> Self {
        match operation {
            LibraryAuditOp::Move { .. } => Self::Move,
            LibraryAuditOp::Load { .. } => Self::Load,
            LibraryAuditOp::Unload { .. } => Self::Unload,
            LibraryAuditOp::Export { .. } => Self::Export,
            LibraryAuditOp::Import { .. } => Self::Import,
            LibraryAuditOp::Rescan => Self::Rescan,
            LibraryAuditOp::LockRemoval => Self::LockRemoval,
            LibraryAuditOp::AllowRemoval => Self::AllowRemoval,
            LibraryAuditOp::DriveUnload { .. } => Self::DriveUnload,
            LibraryAuditOp::DriveLoad { .. } => Self::DriveLoad,
            LibraryAuditOp::TapeRewind { .. } => Self::TapeRewind,
            LibraryAuditOp::TapeReadPosition { .. } => Self::TapeReadPosition,
            LibraryAuditOp::TapeLocate { .. } => Self::TapeLocate,
            LibraryAuditOp::TapeSpace { .. } => Self::TapeSpace,
            LibraryAuditOp::TapeRead { .. } => Self::TapeRead,
            LibraryAuditOp::TapeWrite { .. } => Self::TapeWrite,
            LibraryAuditOp::TapeWriteFilemarks { .. } => Self::TapeWriteFilemarks,
            LibraryAuditOp::TapeReadConfig { .. } => Self::TapeReadConfig,
            LibraryAuditOp::TapeWriteConfig { .. } => Self::TapeWriteConfig,
        }
    }
}

/// Stable tag for Layer 2 audit outcomes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer2AuditOutcomeTag {
    /// The CDB completed successfully.
    Success,
    /// The CDB returned CHECK CONDITION or transport failure.
    ScsiError,
    /// Layer 2 reported an unclassified failure.
    Other,
}

impl Layer2AuditOutcomeTag {
    /// Return the stable serialized tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::ScsiError => "scsi_error",
            Self::Other => "other",
        }
    }

    /// Convert a Layer 2 outcome into its stable tag.
    pub const fn from_outcome(outcome: &LibraryAuditOutcome) -> Self {
        match outcome {
            LibraryAuditOutcome::Success { .. } => Self::Success,
            LibraryAuditOutcome::ScsiError { .. } => Self::ScsiError,
            LibraryAuditOutcome::Other { .. } => Self::Other,
        }
    }
}

/// Stable tag for Layer 2 reconciliation warnings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer2RescanWarningTag {
    /// A drive bay now reports a different serial.
    DriveReplaced,
    /// A drive bay gained a resolved serial.
    DriveAppeared,
    /// A drive bay lost its resolved serial.
    DriveVanished,
    /// Library shape changed.
    ShapeMismatch,
}

impl Layer2RescanWarningTag {
    /// Return the stable serialized tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DriveReplaced => "drive_replaced",
            Self::DriveAppeared => "drive_appeared",
            Self::DriveVanished => "drive_vanished",
            Self::ShapeMismatch => "shape_mismatch",
        }
    }

    /// Convert a Layer 2 warning into its stable tag.
    pub const fn from_warning(warning: &LibraryRescanWarning) -> Self {
        match warning {
            LibraryRescanWarning::DriveReplaced { .. } => Self::DriveReplaced,
            LibraryRescanWarning::DriveAppeared { .. } => Self::DriveAppeared,
            LibraryRescanWarning::DriveVanished { .. } => Self::DriveVanished,
            LibraryRescanWarning::ShapeMismatch { .. } => Self::ShapeMismatch,
        }
    }
}

/// Stable tag for Layer 2 tape SPACE units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer2SpaceKindTag {
    /// Block spacing.
    Blocks,
    /// Filemark spacing.
    Filemarks,
    /// Sequential-filemark spacing.
    SequentialFilemarks,
    /// Space to end of data.
    EndOfData,
}

impl Layer2SpaceKindTag {
    /// Return the stable serialized tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Blocks => "blocks",
            Self::Filemarks => "filemarks",
            Self::SequentialFilemarks => "sequential_filemarks",
            Self::EndOfData => "end_of_data",
        }
    }

    /// Convert a Layer 2 space kind into its stable tag.
    pub const fn from_space_kind(kind: SpaceKind) -> Self {
        match kind {
            SpaceKind::Blocks => Self::Blocks,
            SpaceKind::Filemarks => Self::Filemarks,
            SpaceKind::SequentialFilemarks => Self::SequentialFilemarks,
            SpaceKind::EndOfData => Self::EndOfData,
        }
    }
}

/// Stable tag for Layer 3c audit event families.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer3cAuditEventTag {
    /// A sidecar recovery attempt completed.
    Recovery,
    /// Sidecar metadata redundancy health was observed.
    SidecarMetadataHealth,
}

impl Layer3cAuditEventTag {
    /// Return the stable serialized tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Recovery => "recovery",
            Self::SidecarMetadataHealth => "sidecar_metadata_health",
        }
    }
}

/// Stable tag for Layer 3c recovery outcomes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer3cRecoveryOutcomeTag {
    /// Sidecar reconstruction succeeded.
    Recovered,
    /// Sidecar reconstruction could not recover the block.
    Unrecoverable,
}

impl Layer3cRecoveryOutcomeTag {
    /// Return the stable serialized tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Recovered => "recovered",
            Self::Unrecoverable => "unrecoverable",
        }
    }

    /// Convert a Layer 3c recovery outcome into its stable tag.
    pub const fn from_outcome(outcome: &RecoveryOutcome) -> Self {
        match outcome {
            RecoveryOutcome::Recovered => Self::Recovered,
            RecoveryOutcome::Unrecoverable { .. } => Self::Unrecoverable,
        }
    }
}

/// Stable tag for Layer 3c sidecar metadata health.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer3cSidecarMetadataHealthTag {
    /// Both replicated metadata copies were usable.
    BothCopiesUsable,
    /// The tail copy was lost.
    TailCopyLost,
    /// The primary header copy was lost.
    PrimaryHeaderLost,
}

impl Layer3cSidecarMetadataHealthTag {
    /// Return the stable serialized tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BothCopiesUsable => "both_copies_usable",
            Self::TailCopyLost => "tail_copy_lost",
            Self::PrimaryHeaderLost => "primary_header_lost",
        }
    }

    /// Convert Layer 3c metadata health into its stable tag.
    pub const fn from_health(health: SidecarMetadataHealth) -> Self {
        match health {
            SidecarMetadataHealth::BothCopiesUsable => Self::BothCopiesUsable,
            SidecarMetadataHealth::TailCopyLost => Self::TailCopyLost,
            SidecarMetadataHealth::PrimaryHeaderLost => Self::PrimaryHeaderLost,
        }
    }
}

/// Stable tag for a position inside a parity stripe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StripePositionTag {
    /// Data shard position.
    Data,
    /// Parity shard position.
    Parity,
}

impl StripePositionTag {
    /// Return the stable serialized tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Data => "data",
            Self::Parity => "parity",
        }
    }

    /// Convert a Layer 3c stripe position into its stable tag.
    pub const fn from_position(position: StripePosition) -> Self {
        match position {
            StripePosition::Data { .. } => Self::Data,
            StripePosition::Parity { .. } => Self::Parity,
        }
    }
}

/// Shared lower-layer adapter that appends into a Layer 4 audit sink.
///
/// Layer 2 and Layer 3c hooks cannot return an error, so this adapter records
/// the most recent append failure for the owner task to inspect. Callers that
/// need strict error handling should use the direct `append_*` functions.
pub struct SharedAuditAdapter<S: AuditSink + Send + ?Sized> {
    sink: Arc<Mutex<S>>,
    last_error: Arc<Mutex<Option<String>>>,
}

impl<S: AuditSink + Send + ?Sized> Clone for SharedAuditAdapter<S> {
    fn clone(&self) -> Self {
        Self {
            sink: Arc::clone(&self.sink),
            last_error: Arc::clone(&self.last_error),
        }
    }
}

impl<S: AuditSink + Send + 'static> SharedAuditAdapter<S> {
    /// Create a shared adapter that owns the supplied audit sink.
    pub fn new(sink: S) -> Self {
        Self::from_shared_sink(Arc::new(Mutex::new(sink)))
    }
}

impl<S: AuditSink + Send + ?Sized + 'static> SharedAuditAdapter<S> {
    /// Create a shared adapter around an existing synchronized sink.
    pub fn from_shared_sink(sink: Arc<Mutex<S>>) -> Self {
        Self {
            sink,
            last_error: Arc::new(Mutex::new(None)),
        }
    }

    /// Build a `LibraryHandle::set_audit_hook` closure for Layer 2 events.
    pub fn library_hook(&self) -> impl FnMut(&LibraryAuditEvent<'_>) + Send + 'static {
        let adapter = self.clone();
        move |event| adapter.append_record(library_audit_event_record(event))
    }

    /// Return and clear the most recent hook append failure.
    pub fn take_last_error(&self) -> Option<String> {
        match self.last_error.lock() {
            Ok(mut guard) => guard.take(),
            Err(err) => Some(format!("audit adapter error mutex poisoned: {err}")),
        }
    }

    fn append_record(&self, record: AuditEventRecord) {
        match self.sink.lock() {
            Ok(mut sink) => {
                if let Err(err) = sink.append(record) {
                    self.record_error(err.to_string());
                }
            }
            Err(err) => self.record_error(format!("audit sink mutex poisoned: {err}")),
        }
    }

    fn record_error(&self, error: String) {
        if let Ok(mut guard) = self.last_error.lock() {
            *guard = Some(error);
        }
    }
}

impl<S: AuditSink + Send + ?Sized + 'static> ParityAuditHook for SharedAuditAdapter<S> {
    fn on_recovery(&self, event: &ParityRecoveryEvent) {
        self.append_record(parity_recovery_audit_record(event));
    }

    fn on_sidecar_metadata_health(&self, event: &SidecarMetadataHealthEvent) {
        self.append_record(sidecar_metadata_health_audit_record(event));
    }
}

/// Convert one Layer 2 library audit hook event to a Layer 4 audit record.
pub fn library_audit_event_record(event: &LibraryAuditEvent<'_>) -> AuditEventRecord {
    match event {
        LibraryAuditEvent::Started {
            library_serial,
            operation,
            cdb,
            at,
        } => {
            let mut detail =
                library_detail(library_serial, Layer2AuditEventTag::Started, operation, *at);
            detail.insert("cdb".to_string(), CborValue::Bytes((*cdb).to_vec()));
            library_record(
                AuditEvent::OperationStarted,
                library_serial,
                detail,
                SourceLayer::Layer2,
            )
        }
        LibraryAuditEvent::Refused {
            library_serial,
            operation,
            reason,
            at,
        } => {
            let mut detail =
                library_detail(library_serial, Layer2AuditEventTag::Refused, operation, *at);
            text(&mut detail, "reason", *reason);
            library_record(
                AuditEvent::OperationFailed,
                library_serial,
                detail,
                SourceLayer::Layer2,
            )
        }
        LibraryAuditEvent::Finished {
            library_serial,
            operation,
            outcome,
            at,
        } => {
            let mut detail = library_detail(
                library_serial,
                Layer2AuditEventTag::Finished,
                operation,
                *at,
            );
            add_library_outcome_detail(&mut detail, outcome);
            library_record(
                library_outcome_audit_event(outcome),
                library_serial,
                detail,
                SourceLayer::Layer2,
            )
        }
        LibraryAuditEvent::Warning {
            library_serial,
            operation,
            warning,
            at,
        } => {
            let mut detail =
                library_detail(library_serial, Layer2AuditEventTag::Warning, operation, *at);
            add_rescan_warning_detail(&mut detail, warning);
            library_record(
                AuditEvent::HardwareWarning,
                library_serial,
                detail,
                SourceLayer::Layer2,
            )
        }
    }
}

/// Append one Layer 2 library audit hook event to a Layer 4 audit sink.
pub fn append_library_audit_event(
    sink: &mut dyn AuditSink,
    event: &LibraryAuditEvent<'_>,
) -> Result<AuditReceipt, StateError> {
    sink.append(library_audit_event_record(event))
}

/// Convert one Layer 3c sidecar recovery event to a Layer 4 audit record.
pub fn parity_recovery_audit_record(event: &ParityRecoveryEvent) -> AuditEventRecord {
    let mut detail = BTreeMap::new();
    text(
        &mut detail,
        "layer3c_event",
        Layer3cAuditEventTag::Recovery.as_str(),
    );
    stripe_address_detail(&mut detail, "stripe", event.stripe);
    detail.insert(
        "lost_blocks".to_string(),
        CborValue::Array(
            event
                .lost_blocks
                .iter()
                .copied()
                .map(stripe_position_cbor)
                .collect(),
        ),
    );
    text(
        &mut detail,
        "outcome",
        Layer3cRecoveryOutcomeTag::from_outcome(&event.outcome).as_str(),
    );
    if let RecoveryOutcome::Unrecoverable { lost_count } = event.outcome {
        uint(&mut detail, "lost_count", u64::from(lost_count));
    }
    uint(&mut detail, "at_lba_requested", event.at_lba_requested);
    uint(
        &mut detail,
        "tape_file_number",
        u64::from(event.at_requested.0),
    );
    uint(&mut detail, "body_lba", event.at_requested.1);

    AuditEventRecord {
        actor: AuditActor::System,
        source_layer: SourceLayer::Layer3c,
        operation_id: None,
        session_id: None,
        idempotency_key: None,
        event: AuditEvent::RecoveryEvent,
        subject: AuditSubject {
            kind: "tape_file".to_string(),
            id: Some(event.at_requested.0.to_string()),
        },
        detail,
    }
}

/// Append one Layer 3c sidecar recovery event to a Layer 4 audit sink.
pub fn append_parity_recovery_event(
    sink: &mut dyn AuditSink,
    event: &ParityRecoveryEvent,
) -> Result<AuditReceipt, StateError> {
    sink.append(parity_recovery_audit_record(event))
}

/// Convert one Layer 3c sidecar metadata health event to a Layer 4 audit record.
pub fn sidecar_metadata_health_audit_record(
    event: &SidecarMetadataHealthEvent,
) -> AuditEventRecord {
    let mut detail = BTreeMap::new();
    text(
        &mut detail,
        "layer3c_event",
        Layer3cAuditEventTag::SidecarMetadataHealth.as_str(),
    );
    uint(
        &mut detail,
        "sidecar_tape_file_number",
        u64::from(event.sidecar_tape_file_number),
    );
    uint(&mut detail, "epoch_id", event.epoch_id);
    text(
        &mut detail,
        "health",
        Layer3cSidecarMetadataHealthTag::from_health(event.health).as_str(),
    );

    AuditEventRecord {
        actor: AuditActor::System,
        source_layer: SourceLayer::Layer3c,
        operation_id: None,
        session_id: None,
        idempotency_key: None,
        event: if event.health.is_degraded() {
            AuditEvent::HardwareWarning
        } else {
            AuditEvent::RecoveryEvent
        },
        subject: AuditSubject {
            kind: "sidecar".to_string(),
            id: Some(event.sidecar_tape_file_number.to_string()),
        },
        detail,
    }
}

/// Append one Layer 3c sidecar metadata health event to a Layer 4 audit sink.
pub fn append_sidecar_metadata_health_event(
    sink: &mut dyn AuditSink,
    event: &SidecarMetadataHealthEvent,
) -> Result<AuditReceipt, StateError> {
    sink.append(sidecar_metadata_health_audit_record(event))
}

fn library_record(
    event: AuditEvent,
    library_serial: &str,
    detail: BTreeMap<String, CborValue>,
    source_layer: SourceLayer,
) -> AuditEventRecord {
    AuditEventRecord {
        actor: AuditActor::System,
        source_layer,
        operation_id: None,
        session_id: None,
        idempotency_key: None,
        event,
        subject: AuditSubject {
            kind: "library".to_string(),
            id: Some(library_serial.to_string()),
        },
        detail,
    }
}

fn library_detail(
    library_serial: &str,
    tag: Layer2AuditEventTag,
    operation: &LibraryAuditOp,
    at: SystemTime,
) -> BTreeMap<String, CborValue> {
    let mut detail = BTreeMap::new();
    text(&mut detail, "layer2_event", tag.as_str());
    text(&mut detail, "library_serial", library_serial);
    text(&mut detail, "event_at_utc", system_time_to_rfc3339(at));
    text(
        &mut detail,
        "operation",
        Layer2AuditOpTag::from_operation(operation).as_str(),
    );
    add_library_operation_detail(&mut detail, operation);
    detail
}

fn add_library_operation_detail(
    detail: &mut BTreeMap<String, CborValue>,
    operation: &LibraryAuditOp,
) {
    match *operation {
        LibraryAuditOp::Move { src, dst } => {
            uint(detail, "source_element", u64::from(src));
            uint(detail, "destination_element", u64::from(dst));
        }
        LibraryAuditOp::Load { slot, bay } => {
            uint(detail, "slot_element", u64::from(slot));
            uint(detail, "bay_element", u64::from(bay));
        }
        LibraryAuditOp::Unload { bay, dst } => {
            uint(detail, "bay_element", u64::from(bay));
            optional_u16(detail, "destination_element", dst);
        }
        LibraryAuditOp::Export { slot, ie } => {
            uint(detail, "slot_element", u64::from(slot));
            optional_u16(detail, "ie_element", ie);
        }
        LibraryAuditOp::Import { ie, slot } => {
            optional_u16(detail, "ie_element", ie);
            uint(detail, "slot_element", u64::from(slot));
        }
        LibraryAuditOp::Rescan | LibraryAuditOp::LockRemoval | LibraryAuditOp::AllowRemoval => {}
        LibraryAuditOp::DriveUnload { bay }
        | LibraryAuditOp::DriveLoad { bay }
        | LibraryAuditOp::TapeRewind { bay }
        | LibraryAuditOp::TapeReadPosition { bay }
        | LibraryAuditOp::TapeReadConfig { bay }
        | LibraryAuditOp::TapeWriteConfig { bay } => {
            uint(detail, "bay_element", u64::from(bay));
        }
        LibraryAuditOp::TapeLocate { bay, lba } => {
            uint(detail, "bay_element", u64::from(bay));
            uint(detail, "lba", lba);
        }
        LibraryAuditOp::TapeSpace { bay, count, kind } => {
            uint(detail, "bay_element", u64::from(bay));
            int(detail, "count", count);
            text(
                detail,
                "space_kind",
                Layer2SpaceKindTag::from_space_kind(kind).as_str(),
            );
        }
        LibraryAuditOp::TapeRead { bay, len } | LibraryAuditOp::TapeWrite { bay, len } => {
            uint(detail, "bay_element", u64::from(bay));
            uint(detail, "len", u64::from(len));
        }
        LibraryAuditOp::TapeWriteFilemarks { bay, count } => {
            uint(detail, "bay_element", u64::from(bay));
            uint(detail, "count", u64::from(count));
        }
    }
}

fn add_library_outcome_detail(
    detail: &mut BTreeMap<String, CborValue>,
    outcome: &LibraryAuditOutcome,
) {
    text(
        detail,
        "outcome",
        Layer2AuditOutcomeTag::from_outcome(outcome).as_str(),
    );
    match outcome {
        LibraryAuditOutcome::Success {
            duration,
            snapshot_patched,
            dirty,
        } => {
            uint(detail, "duration_millis", duration_millis(*duration));
            bool_value(detail, "snapshot_patched", *snapshot_patched);
            bool_value(detail, "dirty", *dirty);
        }
        LibraryAuditOutcome::ScsiError {
            sense,
            summary,
            dirty,
        } => {
            optional_bytes(detail, "sense", sense.as_deref());
            text(detail, "summary", summary);
            bool_value(detail, "dirty", *dirty);
        }
        LibraryAuditOutcome::Other { summary } => {
            text(detail, "summary", summary);
        }
    }
}

fn add_rescan_warning_detail(
    detail: &mut BTreeMap<String, CborValue>,
    warning: &LibraryRescanWarning,
) {
    text(
        detail,
        "warning",
        Layer2RescanWarningTag::from_warning(warning).as_str(),
    );
    match warning {
        LibraryRescanWarning::DriveReplaced {
            addr,
            old_serial,
            new_serial,
        } => {
            uint(detail, "addr_element", u64::from(*addr));
            text(detail, "old_serial", old_serial);
            text(detail, "new_serial", new_serial);
        }
        LibraryRescanWarning::DriveAppeared { addr, serial } => {
            uint(detail, "addr_element", u64::from(*addr));
            text(detail, "serial", serial);
        }
        LibraryRescanWarning::DriveVanished { addr, old_serial } => {
            uint(detail, "addr_element", u64::from(*addr));
            text(detail, "old_serial", old_serial);
        }
        LibraryRescanWarning::ShapeMismatch { summary } => {
            text(detail, "summary", summary);
        }
    }
}

fn library_outcome_audit_event(outcome: &LibraryAuditOutcome) -> AuditEvent {
    match outcome {
        LibraryAuditOutcome::Success { .. } => AuditEvent::OperationFinished,
        LibraryAuditOutcome::ScsiError { dirty, .. } if *dirty => AuditEvent::CompletionUnknown,
        LibraryAuditOutcome::ScsiError { .. } | LibraryAuditOutcome::Other { .. } => {
            AuditEvent::OperationFailed
        }
    }
}

fn stripe_address_detail(
    detail: &mut BTreeMap<String, CborValue>,
    prefix: &str,
    address: remanence_parity::StripeAddress,
) {
    uint(
        detail,
        &format!("{prefix}_neighborhood"),
        address.neighborhood,
    );
    uint(
        detail,
        &format!("{prefix}_index"),
        u64::from(address.stripe_index),
    );
    text(
        detail,
        &format!("{prefix}_position"),
        StripePositionTag::from_position(address.position).as_str(),
    );
    uint(
        detail,
        &format!("{prefix}_position_index"),
        u64::from(stripe_position_index(address.position)),
    );
}

fn stripe_position_cbor(position: StripePosition) -> CborValue {
    cbor_map(vec![
        (
            "position",
            CborValue::Text(
                StripePositionTag::from_position(position)
                    .as_str()
                    .to_string(),
            ),
        ),
        (
            "index",
            CborValue::Integer(u64::from(stripe_position_index(position)).into()),
        ),
    ])
}

fn stripe_position_index(position: StripePosition) -> u16 {
    match position {
        StripePosition::Data { index } | StripePosition::Parity { index } => index,
    }
}

fn system_time_to_rfc3339(at: SystemTime) -> String {
    let at: OffsetDateTime = at.into();
    at.format(&Rfc3339)
        .unwrap_or_else(|err| format!("timestamp_format_error:{err}"))
}

fn duration_millis(duration: StdDuration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn text(detail: &mut BTreeMap<String, CborValue>, key: &str, value: impl Into<String>) {
    detail.insert(key.to_string(), CborValue::Text(value.into()));
}

fn uint(detail: &mut BTreeMap<String, CborValue>, key: &str, value: u64) {
    detail.insert(key.to_string(), CborValue::Integer(value.into()));
}

fn int(detail: &mut BTreeMap<String, CborValue>, key: &str, value: i64) {
    detail.insert(key.to_string(), CborValue::Integer(value.into()));
}

fn bool_value(detail: &mut BTreeMap<String, CborValue>, key: &str, value: bool) {
    detail.insert(key.to_string(), CborValue::Bool(value));
}

fn optional_u16(detail: &mut BTreeMap<String, CborValue>, key: &str, value: Option<u16>) {
    detail.insert(
        key.to_string(),
        value
            .map(|value| CborValue::Integer(u64::from(value).into()))
            .unwrap_or(CborValue::Null),
    );
}

fn optional_bytes(detail: &mut BTreeMap<String, CborValue>, key: &str, value: Option<&[u8]>) {
    detail.insert(
        key.to_string(),
        value
            .map(|value| CborValue::Bytes(value.to_vec()))
            .unwrap_or(CborValue::Null),
    );
}

fn cbor_map(fields: Vec<(&str, CborValue)>) -> CborValue {
    let mut entries: Vec<_> = fields
        .into_iter()
        .map(|(key, value)| (CborValue::Text(key.to_string()), value))
        .collect();
    entries.sort_by(|(left, _), (right, _)| text_key(left).cmp(text_key(right)));
    CborValue::Map(entries)
}

fn text_key(value: &CborValue) -> &str {
    match value {
        CborValue::Text(text) => text,
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remanence_parity::{RecoveryEvent, StripeAddress};

    #[derive(Default)]
    struct RecordingSink {
        records: Vec<AuditEventRecord>,
    }

    impl AuditSink for RecordingSink {
        fn append(&mut self, event: AuditEventRecord) -> Result<AuditReceipt, StateError> {
            self.records.push(event);
            Ok(AuditReceipt {
                sequence: self.records.len() as u64,
                record_uuid: uuid::Uuid::nil(),
                record_hash: [self.records.len() as u8; 32],
                fsync_completed: true,
            })
        }
    }

    fn text_detail(record: &AuditEventRecord, key: &str) -> String {
        match record.detail.get(key).expect("detail key") {
            CborValue::Text(value) => value.clone(),
            other => panic!("expected text for {key}, got {other:?}"),
        }
    }

    fn int_detail(record: &AuditEventRecord, key: &str) -> i128 {
        match record.detail.get(key).expect("detail key") {
            CborValue::Integer(value) => i128::from(*value),
            other => panic!("expected integer for {key}, got {other:?}"),
        }
    }

    fn bool_detail(record: &AuditEventRecord, key: &str) -> bool {
        match record.detail.get(key).expect("detail key") {
            CborValue::Bool(value) => *value,
            other => panic!("expected bool for {key}, got {other:?}"),
        }
    }

    #[test]
    fn layer2_started_event_preserves_operation_and_cdb_tags() {
        let event = LibraryAuditEvent::Started {
            library_serial: "lib-001",
            operation: LibraryAuditOp::TapeSpace {
                bay: 7,
                count: -3,
                kind: SpaceKind::Filemarks,
            },
            cdb: &[0x11, 0x22, 0x33],
            at: SystemTime::UNIX_EPOCH,
        };

        let record = library_audit_event_record(&event);

        assert_eq!(record.source_layer, SourceLayer::Layer2);
        assert_eq!(record.event, AuditEvent::OperationStarted);
        assert_eq!(record.subject.kind, "library");
        assert_eq!(record.subject.id.as_deref(), Some("lib-001"));
        assert_eq!(text_detail(&record, "layer2_event"), "started");
        assert_eq!(text_detail(&record, "operation"), "tape_space");
        assert_eq!(text_detail(&record, "space_kind"), "filemarks");
        assert_eq!(int_detail(&record, "bay_element"), 7);
        assert_eq!(int_detail(&record, "count"), -3);
        assert_eq!(
            record.detail.get("cdb"),
            Some(&CborValue::Bytes(vec![0x11, 0x22, 0x33]))
        );
    }

    #[test]
    fn layer2_dirty_scsi_error_maps_to_completion_unknown() {
        let event = LibraryAuditEvent::Finished {
            library_serial: "lib-001",
            operation: LibraryAuditOp::Move { src: 1, dst: 2 },
            outcome: LibraryAuditOutcome::ScsiError {
                sense: Some(vec![0x70, 0x00, 0x05]),
                summary: "check condition".to_string(),
                dirty: true,
            },
            at: SystemTime::UNIX_EPOCH,
        };

        let record = library_audit_event_record(&event);

        assert_eq!(record.event, AuditEvent::CompletionUnknown);
        assert_eq!(text_detail(&record, "layer2_event"), "finished");
        assert_eq!(text_detail(&record, "outcome"), "scsi_error");
        assert_eq!(text_detail(&record, "summary"), "check condition");
        assert!(bool_detail(&record, "dirty"));
        assert_eq!(
            record.detail.get("sense"),
            Some(&CborValue::Bytes(vec![0x70, 0x00, 0x05]))
        );
    }

    #[test]
    fn layer2_reconciliation_warning_maps_to_hardware_warning() {
        let event = LibraryAuditEvent::Warning {
            library_serial: "lib-001",
            operation: LibraryAuditOp::Rescan,
            warning: LibraryRescanWarning::DriveReplaced {
                addr: 0x100,
                old_serial: "old".to_string(),
                new_serial: "new".to_string(),
            },
            at: SystemTime::UNIX_EPOCH,
        };

        let record = library_audit_event_record(&event);

        assert_eq!(record.event, AuditEvent::HardwareWarning);
        assert_eq!(text_detail(&record, "warning"), "drive_replaced");
        assert_eq!(int_detail(&record, "addr_element"), 0x100);
        assert_eq!(text_detail(&record, "old_serial"), "old");
        assert_eq!(text_detail(&record, "new_serial"), "new");
    }

    #[test]
    fn layer3c_recovery_event_preserves_stripe_and_outcome_tags() {
        let event = RecoveryEvent {
            stripe: StripeAddress {
                neighborhood: 5,
                stripe_index: 9,
                position: StripePosition::Data { index: 1 },
            },
            lost_blocks: vec![
                StripePosition::Data { index: 1 },
                StripePosition::Parity { index: 0 },
            ],
            outcome: RecoveryOutcome::Unrecoverable { lost_count: 2 },
            at_lba_requested: 42,
            at_requested: (11, 42),
        };

        let record = parity_recovery_audit_record(&event);

        assert_eq!(record.source_layer, SourceLayer::Layer3c);
        assert_eq!(record.event, AuditEvent::RecoveryEvent);
        assert_eq!(record.subject.id.as_deref(), Some("11"));
        assert_eq!(text_detail(&record, "layer3c_event"), "recovery");
        assert_eq!(text_detail(&record, "outcome"), "unrecoverable");
        assert_eq!(int_detail(&record, "lost_count"), 2);
        assert_eq!(int_detail(&record, "stripe_neighborhood"), 5);
        assert_eq!(int_detail(&record, "stripe_index"), 9);
        assert_eq!(text_detail(&record, "stripe_position"), "data");
        assert_eq!(int_detail(&record, "stripe_position_index"), 1);
        assert_eq!(int_detail(&record, "tape_file_number"), 11);
        assert_eq!(int_detail(&record, "body_lba"), 42);
        match record.detail.get("lost_blocks").expect("lost blocks") {
            CborValue::Array(blocks) => assert_eq!(blocks.len(), 2),
            other => panic!("expected lost block array, got {other:?}"),
        }
    }

    #[test]
    fn layer3c_sidecar_metadata_loss_maps_to_hardware_warning() {
        let event = SidecarMetadataHealthEvent {
            sidecar_tape_file_number: 12,
            epoch_id: 34,
            health: SidecarMetadataHealth::TailCopyLost,
        };

        let record = sidecar_metadata_health_audit_record(&event);

        assert_eq!(record.source_layer, SourceLayer::Layer3c);
        assert_eq!(record.event, AuditEvent::HardwareWarning);
        assert_eq!(record.subject.kind, "sidecar");
        assert_eq!(record.subject.id.as_deref(), Some("12"));
        assert_eq!(
            text_detail(&record, "layer3c_event"),
            "sidecar_metadata_health"
        );
        assert_eq!(text_detail(&record, "health"), "tail_copy_lost");
        assert_eq!(int_detail(&record, "epoch_id"), 34);
    }

    #[test]
    fn direct_append_helper_appends_library_event() {
        let mut sink = RecordingSink::default();
        let event = LibraryAuditEvent::Refused {
            library_serial: "lib-001",
            operation: LibraryAuditOp::Export {
                slot: 0x400,
                ie: None,
            },
            reason: "NoIePort",
            at: SystemTime::UNIX_EPOCH,
        };

        let receipt = append_library_audit_event(&mut sink, &event).expect("append");

        assert_eq!(receipt.sequence, 1);
        assert_eq!(sink.records.len(), 1);
        assert_eq!(sink.records[0].event, AuditEvent::OperationFailed);
        assert_eq!(text_detail(&sink.records[0], "reason"), "NoIePort");
        assert_eq!(
            sink.records[0].detail.get("ie_element"),
            Some(&CborValue::Null)
        );
    }

    #[test]
    fn shared_adapter_feeds_library_and_parity_hooks() {
        let adapter = SharedAuditAdapter::new(RecordingSink::default());
        let event = LibraryAuditEvent::Warning {
            library_serial: "lib-001",
            operation: LibraryAuditOp::Rescan,
            warning: LibraryRescanWarning::DriveAppeared {
                addr: 0x100,
                serial: "drive-001".to_string(),
            },
            at: SystemTime::UNIX_EPOCH,
        };
        let recovery = RecoveryEvent {
            stripe: StripeAddress {
                neighborhood: 1,
                stripe_index: 2,
                position: StripePosition::Data { index: 0 },
            },
            lost_blocks: vec![StripePosition::Data { index: 0 }],
            outcome: RecoveryOutcome::Recovered,
            at_lba_requested: 3,
            at_requested: (4, 3),
        };

        let mut library_hook = adapter.library_hook();
        library_hook(&event);
        ParityAuditHook::on_recovery(&adapter, &recovery);

        assert_eq!(adapter.take_last_error(), None);
        let sink = adapter.sink.lock().expect("sink lock");
        assert_eq!(sink.records.len(), 2);
        assert_eq!(sink.records[0].source_layer, SourceLayer::Layer2);
        assert_eq!(sink.records[0].event, AuditEvent::HardwareWarning);
        assert_eq!(sink.records[1].source_layer, SourceLayer::Layer3c);
        assert_eq!(sink.records[1].event, AuditEvent::RecoveryEvent);
    }
}
