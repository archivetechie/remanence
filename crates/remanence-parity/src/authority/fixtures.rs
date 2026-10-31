//! Structural authority fixtures expressed as semantic rows and publication reports.
use super::{ShadowPublication, TapeReportPublisher, TapeRowAuthority};
use crate::{CommittedBundle, CommittedBundleKind};
use crate::{JournalError, ParityScheme};
use std::path::{Path, PathBuf};

/// Builds the selected structural backend without exposing an encoder to tests.
#[derive(Debug)]
pub struct RowAuthorityBuilder {
    location: PathBuf,
    tape_uuid: [u8; 16],
    block_size: u32,
    scheme: ParityScheme,
    reports: Vec<ShadowPublication>,
    claims: Vec<crate::CommittedBundle>,
    absent_block_counts: Vec<(usize, usize)>,
}
impl RowAuthorityBuilder {
    /// Select a directory and tape identity; the backend owns all file names.
    pub fn new(
        location: impl AsRef<Path>,
        tape_uuid: [u8; 16],
        block_size: u32,
        scheme: ParityScheme,
    ) -> Self {
        Self {
            location: TapeRowAuthority::location_in(location.as_ref(), tape_uuid),
            tape_uuid,
            block_size,
            scheme,
            reports: Vec::new(),
            claims: Vec::new(),
            absent_block_counts: Vec::new(),
        }
    }
    /// Add one semantic media publication in its original order.
    pub fn publication(mut self, report: ShadowPublication) -> Self {
        self.reports.push(report);
        self
    }
    /// Supply potentially contradictory semantic claims for negative replay tests.
    /// The selected codec records them without treating them as accepted authority.
    pub fn claims(mut self, claims: Vec<crate::CommittedBundle>) -> Self {
        self.claims = claims;
        self
    }
    /// Omit required block counts to exercise malformed legacy claims. Pairs
    /// identify the publication and file within it; replay must reject them.
    pub fn absent_block_counts(mut self, rows: Vec<(usize, usize)>) -> Self {
        self.absent_block_counts = rows;
        self
    }
    /// Build only the compile-time selected authority implementation.
    pub fn build(self) -> Result<TapeRowAuthority, JournalError> {
        self.build_mode(false)
    }
    /// Construct developer-vector claims using the production trusted-volume open.
    pub fn build_trusted(self) -> Result<TapeRowAuthority, JournalError> {
        self.build_mode(true)
    }
    fn build_mode(self, trusted: bool) -> Result<TapeRowAuthority, JournalError> {
        // A directory-based fixture creates its directory before the production
        // open checks that directory's filesystem, just like the former callers.
        if let Some(parent) = self.location.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let open = if trusted {
            |path: &Path, id, block, scheme| TapeRowAuthority::open(path, id, block, scheme)
        } else {
            |path: &Path, id, block, scheme| TapeRowAuthority::open_fixture(path, id, block, scheme)
        };
        let mut authority = open(
            &self.location,
            self.tape_uuid,
            self.block_size,
            self.scheme.clone(),
        )?;
        for report in self.reports {
            authority.publish_fixture(&report)?;
        }
        if !self.claims.is_empty() {
            authority.append_fixture_claims(&self.claims, &self.absent_block_counts)?;
            drop(authority);
            return open(&self.location, self.tape_uuid, self.block_size, self.scheme);
        }
        Ok(authority)
    }
}

/// Backend-independent faults used by negative authority fixtures.
#[derive(Clone, Copy, Debug)]
pub enum RowFixtureFault {
    /// Remove the first claimed file.
    MissingFirst,
    /// Repeat the first Object unchanged.
    DuplicateObject,
    /// Repeat the first Object with a conflicting block count.
    ConflictingObject,
    /// Contradict the protected watermark in the final barrier.
    ProtectedWatermark,
    /// Contradict the total watermark in the final barrier.
    TotalWatermark,
    /// Interrupt the final encoded publication.
    IncompleteFinal,
}
/// Inject one semantic contradiction through the selected fixture backend.
pub fn inject_row_fault(
    authority: &TapeRowAuthority,
    fault: RowFixtureFault,
) -> Result<(), JournalError> {
    authority.inject_fixture_fault(fault)
}

/// Retained fixture evidence that can be restored after a refusal check.
pub use super::legacy::HiddenRowAuthority;

/// Make structural authority unavailable without changing its saved contents.
pub fn hide_authority(location: impl AsRef<Path>) -> Result<HiddenRowAuthority, JournalError> {
    super::legacy::hide_fixture_rows(location.as_ref())
}

/// Open existing fixture rows without applying production filesystem policy.
pub fn open_shared(
    path: impl AsRef<Path>,
    tape_uuid: [u8; 16],
    block_size: u32,
    scheme: ParityScheme,
) -> Result<super::TapeRowReader, JournalError> {
    super::legacy::open_shared_fixture(path.as_ref(), tape_uuid, block_size, scheme)
}

/// Implement publication for an in-memory fixture retaining legacy claims.
#[macro_export]
macro_rules! fixture_publication_methods {
    () => {
        fn publish_commit(
            &mut self,
            publication: &$crate::authority::FilePublication,
        ) -> Result<$crate::authority::PublicationReceipt, $crate::JournalError> {
            $crate::authority::fixtures::publish(self, publication, false)
        }
        fn publish_terminal_prefix(
            &mut self,
            publication: &$crate::authority::FilePublication,
        ) -> Result<$crate::authority::PublicationReceipt, $crate::JournalError> {
            $crate::authority::fixtures::publish_terminal(
                self,
                publication,
                $crate::authority::FilePurpose::TerminalPrefix,
            )
        }
        fn publish_terminal_component(
            &mut self,
            publication: &$crate::authority::FilePublication,
        ) -> Result<$crate::authority::PublicationReceipt, $crate::JournalError> {
            $crate::authority::fixtures::publish_terminal(
                self,
                publication,
                $crate::authority::FilePurpose::TerminalComponent,
            )
        }
    };
}
#[doc(hidden)]
pub fn publish_terminal<P: TapeReportPublisher + ?Sized>(
    publisher: &mut P,
    publication: &super::FilePublication,
    expected: super::FilePurpose,
) -> Result<super::PublicationReceipt, JournalError> {
    super::validate_terminal_publication(publication, expected)?;
    publish(publisher, publication, true)
}
#[doc(hidden)]
pub fn publish<P: TapeReportPublisher + ?Sized>(
    publisher: &mut P,
    publication: &super::FilePublication,
    terminal: bool,
) -> Result<super::PublicationReceipt, JournalError> {
    let mut bundles = vec![publication.into()];
    if terminal {
        bundles.push(crate::CommittedBundle {
            kind: crate::CommittedBundleKind::CheckpointedThrough,
            entries: Vec::new(),
            highest_protected_ordinal: publication.protected,
            total_committed_ordinals: publication.total,
        });
    } else {
        super::refuse_terminal_commit(publication)?;
    }
    publisher.publish_fixture(&ShadowPublication { bundles })?;
    Ok(super::PublicationReceipt::appended(publication.clone()))
}

/// Isolated old-codec tests may name a byte file explicitly.
#[doc(hidden)]
pub struct LegacyCodecFixture;
impl LegacyCodecFixture {
    /// Bypass only filesystem policy for an isolated framing/codec fixture.
    pub fn at_path(
        path: impl AsRef<Path>,
        tape_uuid: [u8; 16],
        block_size: u32,
        scheme: ParityScheme,
    ) -> RowAuthorityBuilder {
        let mut fixture = RowAuthorityBuilder::new(Path::new("."), tape_uuid, block_size, scheme);
        fixture.location = path.as_ref().to_path_buf();
        fixture
    }
}

/// Decode the legacy append sequence claimed by a semantic receipt for disk comparisons.
/// Only codec-facing tests need to know that terminal durability has its own record.
pub fn appended_legacy_units(receipt: &super::PublicationReceipt) -> Vec<CommittedBundle> {
    let mut result = Vec::new();
    for item in &receipt.items {
        if item.disposition == super::PublicationDisposition::Appended {
            result.push((&item.publication).into());
        }
        if matches!(
            item.publication.purpose,
            super::FilePurpose::TerminalPrefix | super::FilePurpose::TerminalComponent
        ) && item.durability == super::PublicationDisposition::Appended
        {
            result.push(CommittedBundle {
                kind: CommittedBundleKind::CheckpointedThrough,
                entries: Vec::new(),
                highest_protected_ordinal: item.publication.protected,
                total_committed_ordinals: item.publication.total,
            });
        }
    }
    result
}

/// Opaque saved authority used to model loss of host publication after media I/O.
pub struct SavedRows {
    location: PathBuf,
    bytes: Vec<u8>,
}
impl SavedRows {
    /// Restore the saved fixture after dropping its writer capability.
    pub fn restore(self) -> Result<(), JournalError> {
        std::fs::write(self.location, self.bytes)?;
        Ok(())
    }
}
/// Save fixture authority without exposing its encoding or path to the test.
pub fn save_authority(authority: &TapeRowAuthority) -> Result<SavedRows, JournalError> {
    Ok(SavedRows {
        location: authority.path().to_path_buf(),
        bytes: std::fs::read(authority.path())?,
    })
}
