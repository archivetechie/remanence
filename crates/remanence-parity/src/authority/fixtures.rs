//! In-memory report collectors for pure media fixtures.
use super::{ShadowPublication, TapeReportPublisher};
use crate::{FileRowsKind, FileRowsReport, JournalError};
/// Implement publication for an in-memory fixture retaining semantic reports.
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
        bundles.push(crate::FileRowsReport {
            kind: crate::FileRowsKind::FixtureBoundary,
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

/// Expected input reports, including fixture-only commit boundaries, for a receipt.
pub fn published_fixture_reports(receipt: &super::PublicationReceipt) -> Vec<FileRowsReport> {
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
            result.push(FileRowsReport {
                kind: FileRowsKind::FixtureBoundary,
                entries: Vec::new(),
                highest_protected_ordinal: item.publication.protected,
                total_committed_ordinals: item.publication.total,
            });
        }
    }
    result
}
