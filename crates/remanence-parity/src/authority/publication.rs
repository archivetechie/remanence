//! Media publications and their durable outcomes, independent of host-log framing.
use super::{SealDueReason, TapeFileEntry};
use crate::{CheckProvenance, CheckValue, CommitCursor, TapeFileKind};

/// Physical event whose files became durable.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FilePurpose {
    /// The sole BOT file.
    Bootstrap,
    /// An Object and any sidecars emitted with it.
    Object,
    /// Sidecars, with the origin of the data they protect.
    Sidecars(SidecarProvenance),
    /// The planned prefix before the terminal edition.
    TerminalPrefix,
    /// One planned terminal replica or separation extent.
    TerminalComponent,
}

impl std::fmt::Debug for FilePurpose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Bootstrap => "BotBootstrap",
            Self::Object => "Object",
            Self::Sidecars(SidecarProvenance::Fresh) => "CheckpointSidecars",
            Self::Sidecars(SidecarProvenance::Reconstructed) => "ResumeSidecars",
            Self::TerminalPrefix => "TerminalPrefix",
            Self::TerminalComponent => "TerminalComponent",
        })
    }
}

/// Origin of sidecar data; independent of how a backend encodes publication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidecarProvenance {
    /// Data accepted by the current write session.
    Fresh,
    /// Data reconstructed from the previously committed tape prefix.
    Reconstructed,
}

/// Files and evidence proved by one physical operation. Missing evidence means
/// the active legacy format did not retain it; it must never be fabricated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilePublication {
    /// Physical operation, never a host-codec discriminator.
    pub purpose: FilePurpose,
    /// Files in physical order.
    pub files: Vec<TapeFileEntry>,
    /// Protected data ordinal W.
    pub protected: u64,
    /// Committed data ordinal T.
    pub total: u64,
    /// Barrier-proved position, when supplied by the writer.
    pub cursor: Option<CommitCursor>,
    /// Writer check and its source, absent in old stored records.
    pub check: Option<(CheckValue, CheckProvenance)>,
    /// Admission obligation observed at this boundary.
    pub seal_due: Option<SealDueReason>,
}

/// Outcome for one unit of host durability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublicationDisposition {
    /// Media report awaiting the actor's tape-log publication.
    Pending,
    /// This call appended and synchronized the unit.
    Appended,
    /// Exact matching authority was already durable.
    AlreadyPresent,
}

/// One semantic operation described by a successful publication receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedItem {
    /// Semantic content of the unit.
    pub publication: FilePublication,
    /// Whether this invocation appended the operation's rows.
    pub disposition: PublicationDisposition,
    /// Whether this invocation completed its durable authority. An orphan may
    /// have existing rows while this call completes durability.
    pub durability: PublicationDisposition,
}

/// Actual publication outcomes, including idempotent recovery.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PublicationReceipt {
    /// Published semantic operations in their established order.
    pub items: Vec<PublishedItem>,
}
impl PublicationReceipt {
    /// Pending physical evidence; this is never a durable client receipt.
    pub fn pending(publication: FilePublication) -> Self {
        Self {
            items: vec![PublishedItem {
                publication,
                disposition: PublicationDisposition::Pending,
                durability: PublicationDisposition::Pending,
            }],
        }
    }

    /// Receipt for an ordinary append.
    pub fn appended(publication: FilePublication) -> Self {
        Self {
            items: vec![PublishedItem {
                publication,
                disposition: PublicationDisposition::Appended,
                durability: PublicationDisposition::Appended,
            }],
        }
    }
}

/// One durable media commit and its referenced catalog payloads. This is a
/// semantic projection: no field selects a host record or bundle encoding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitPublication<P, F> {
    /// Monotonic authority generation.
    pub ordinal: u64,
    /// Cumulative Object count.
    pub object_count: u64,
    /// Protected data ordinal W at this commit.
    pub protected: u64,
    /// Total committed data ordinals T at this commit.
    pub total: u64,
    /// Proved append position.
    pub cursor: CommitCursor,
    /// Physical tape identity.
    pub tape_uuid: [u8; 16],
    /// Idempotent operation identity.
    pub operation_id: [u8; 16],
    /// Next physical file number.
    pub next_file: u64,
    /// Fixed record size.
    pub block_size: u32,
    /// Referenced catalog payloads, in Object order.
    pub payloads: Vec<P>,
    /// Parity geometry, absent on an unprotected tape.
    pub scheme: Option<crate::ParityScheme>,
    /// Physical operations associated with Objects, in the same order.
    pub object_files: Vec<FilePublication>,
    /// Files emitted at the barrier, if any.
    pub barrier: Option<FilePublication>,
    /// Completed terminal plan and progress.
    pub finalization: Option<F>,
    /// This commit proves sealed completion.
    pub sealed: bool,
    /// Deferred finalization obligation.
    pub seal_due: Option<SealDueReason>,
    /// Check evidence when retained by the active format.
    pub check: Option<(CheckValue, CheckProvenance)>,
}

/// Materialized semantic structural evidence for offline catalog projection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StructuralState {
    /// Durable physical rows in file order.
    pub files: Vec<TapeFileEntry>,
    /// Protected ordinal W.
    pub protected: u64,
    /// Committed ordinal T.
    pub total: u64,
    /// Physical operations not yet part of the durable checkpoint prefix.
    pub pending: Vec<FilePublication>,
}
impl StructuralState {
    /// Construct the structural map without consulting a host codec.
    pub fn filemark_map(&self) -> Result<crate::FilemarkMap, crate::ParityError> {
        crate::FilemarkMap::new(self.files.iter().map(TapeFileEntry::to_map_entry).collect())
    }
}

impl FilePublication {
    /// Validate the physical file grammar without consulting a host log.
    pub fn validate(&self) -> Result<(), PublicationShapeError> {
        validate_file_publication(self).map(|_| ())
    }
}
/// Structural grammar violation in one committed tape-file bundle.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind:?} committed bundle: {detail}")]
pub struct PublicationShapeError {
    kind: FilePurpose,
    detail: String,
}

impl PublicationShapeError {
    fn new(kind: FilePurpose, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }
}

/// Validate the current-wire entry grammar for one committed bundle.
///
/// Object bundles contain their Object first followed only by completed
/// sidecars. The sole BOT Bootstrap, checkpoint sidecars, restart sidecars,
/// and terminal components each use distinct exact grammars.
fn validate_file_publication(
    bundle: &FilePublication,
) -> Result<Option<&TapeFileEntry>, PublicationShapeError> {
    validate_dense_entry_numbers(bundle)?;
    match bundle.purpose {
        FilePurpose::Object => {
            let Some((object, tail)) = bundle.files.split_first() else {
                return Err(PublicationShapeError::new(
                    bundle.purpose,
                    "must contain an Object entry",
                ));
            };
            if object.kind != TapeFileKind::Object {
                return Err(PublicationShapeError::new(
                    bundle.purpose,
                    format!("must start with Object, got {:?}", object.kind),
                ));
            }
            validate_sidecar_prefix(bundle.purpose, tail)?;
            Ok(None)
        }
        FilePurpose::Bootstrap => {
            let [entry] = bundle.files.as_slice() else {
                return Err(PublicationShapeError::new(
                    bundle.purpose,
                    "must contain exactly one Bootstrap",
                ));
            };
            if entry.tape_file_number != 0
                || entry.kind != TapeFileKind::Bootstrap
                || entry.block_count != 1
            {
                return Err(PublicationShapeError::new(
                    bundle.purpose,
                    "must contain the one-block tape-file-0 BOT Bootstrap",
                ));
            }
            Ok(Some(entry))
        }
        FilePurpose::TerminalPrefix => {
            validate_terminal_prefix(bundle.purpose, &bundle.files)?;
            Ok(None)
        }
        FilePurpose::TerminalComponent => {
            if !validate_terminal_control_component(bundle.purpose, &bundle.files)? {
                return Err(PublicationShapeError::new(
                    bundle.purpose,
                    "must contain exactly one terminal index or separation component",
                ));
            }
            Ok(None)
        }
        FilePurpose::Sidecars(_) => {
            if bundle.files.is_empty() {
                return Err(PublicationShapeError::new(
                    bundle.purpose,
                    "must contain at least one ParitySidecar",
                ));
            }
            validate_sidecar_prefix(bundle.purpose, &bundle.files)?;
            Ok(None)
        }
    }
}

fn validate_terminal_prefix(
    kind: FilePurpose,
    entries: &[TapeFileEntry],
) -> Result<(), PublicationShapeError> {
    let sidecars = match entries.split_last() {
        Some((last, sidecars)) if last.kind == TapeFileKind::ParityMap => sidecars,
        _ => entries,
    };
    validate_sidecar_prefix(kind, sidecars)
}

fn validate_terminal_control_component(
    kind: FilePurpose,
    entries: &[TapeFileEntry],
) -> Result<bool, PublicationShapeError> {
    let [entry] = entries else {
        return Ok(false);
    };
    if !matches!(
        entry.kind,
        TapeFileKind::TapeIndexReplica | TapeFileKind::IndexSeparationExtent
    ) {
        return Ok(false);
    }
    if entry.block_count == 0 {
        return Err(PublicationShapeError::new(
            kind,
            format!(
                "terminal {:?} at tape file {} has zero blocks",
                entry.kind, entry.tape_file_number
            ),
        ));
    }
    if entry.object_id.is_some()
        || entry.first_parity_data_ordinal.is_some()
        || entry.epoch_id.is_some()
        || entry.protected_ordinal_start.is_some()
        || entry.protected_ordinal_end_exclusive.is_some()
        || entry.object_recovery_row.is_some()
    {
        return Err(PublicationShapeError::new(
            kind,
            format!(
                "terminal {:?} at tape file {} has invalid kind-specific fields",
                entry.kind, entry.tape_file_number
            ),
        ));
    }
    Ok(true)
}

fn validate_dense_entry_numbers(bundle: &FilePublication) -> Result<(), PublicationShapeError> {
    for pair in bundle.files.windows(2) {
        let expected = pair[0].tape_file_number.checked_add(1).ok_or_else(|| {
            PublicationShapeError::new(bundle.purpose, "tape-file number overflows u64")
        })?;
        if pair[1].tape_file_number != expected {
            return Err(PublicationShapeError::new(
                bundle.purpose,
                format!(
                    "entries are not dense: expected tape file {expected}, got {}",
                    pair[1].tape_file_number
                ),
            ));
        }
    }
    Ok(())
}

fn validate_sidecar_prefix(
    kind: FilePurpose,
    entries: &[TapeFileEntry],
) -> Result<(), PublicationShapeError> {
    if let Some(entry) = entries
        .iter()
        .find(|entry| entry.kind != TapeFileKind::ParitySidecar)
    {
        return Err(PublicationShapeError::new(
            kind,
            format!(
                "unexpected {:?} entry at tape file {}",
                entry.kind, entry.tape_file_number
            ),
        ));
    }
    Ok(())
}
