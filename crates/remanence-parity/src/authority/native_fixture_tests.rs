//! Unit-test bridge to the native state fixture backend. Cargo builds this unit
//! test crate separately from the parity library used by state; only semantic
//! values cross that boundary. All persistent bytes belong to the native log.
use super::*;
use host::authority::{StructuralAuthority as _, TapeReportPublisher as _};
use remanence_parity as host;
use remanence_state::authority::row_fixtures as native;
use serde::{de::DeserializeOwned, Serialize};
use std::path::Path;

fn value<A: Serialize, B: DeserializeOwned>(a: &A) -> B {
    serde_json::from_value(serde_json::to_value(a).unwrap()).unwrap()
}
fn error(e: host::JournalError) -> JournalError {
    match e {
        host::JournalError::Io(e) => JournalError::Io(e),
        other => JournalError::Codec(other.to_string()),
    }
}
fn publication(p: &FilePublication) -> host::authority::FilePublication {
    let b: host::FileRowsReport = value(&crate::FileRowsReport::from(p));
    host::authority::FilePublication::try_from(&b).unwrap()
}
fn local_publication(p: &host::authority::FilePublication) -> FilePublication {
    let b: crate::FileRowsReport = value(&host::FileRowsReport::from(p));
    FilePublication::try_from(&b).unwrap()
}
fn disposition(p: host::authority::PublicationDisposition) -> PublicationDisposition {
    match p {
        host::authority::PublicationDisposition::Pending => PublicationDisposition::Pending,
        host::authority::PublicationDisposition::Appended => PublicationDisposition::Appended,
        host::authority::PublicationDisposition::AlreadyPresent => {
            PublicationDisposition::AlreadyPresent
        }
    }
}
fn receipt(r: host::authority::PublicationReceipt) -> PublicationReceipt {
    PublicationReceipt {
        items: r
            .items
            .into_iter()
            .map(|i| PublishedItem {
                publication: local_publication(&i.publication),
                disposition: disposition(i.disposition),
                durability: disposition(i.durability),
            })
            .collect(),
    }
}
#[derive(Debug)]
pub(crate) struct RowAuthorityBuilder(native::RowAuthorityBuilder, crate::ParityScheme);
impl RowAuthorityBuilder {
    pub(crate) fn new(
        path: impl AsRef<Path>,
        uuid: [u8; 16],
        block: u32,
        scheme: crate::ParityScheme,
    ) -> Self {
        Self(
            native::RowAuthorityBuilder::new(path, uuid, block, value(&scheme)),
            scheme,
        )
    }
    pub(crate) fn build(self) -> Result<RowAuthority, JournalError> {
        self.0
            .build()
            .map(|rows| RowAuthority(rows, self.1))
            .map_err(error)
    }
}
#[derive(Debug)]
pub(crate) struct RowAuthority(pub(crate) native::RowAuthority, crate::ParityScheme);
impl RowAuthority {
    pub(crate) fn prepare_terminal(
        &mut self,
        p: &crate::TerminalPrefixPlan,
    ) -> Result<(), JournalError> {
        let native = host::TerminalPrefixPlan {
            diagnostics: host::ParityMapDiagnostics {
                writer_version: p.diagnostics.writer_version.clone(),
                write_timestamp: p.diagnostics.write_timestamp.clone(),
            },
            start_tape_file_number: p.start_tape_file_number,
            tail_start_tape_file_number: p.tail_start_tape_file_number,
            start_lba: p.start_lba,
            tail_start_lba: p.tail_start_lba,
            parity_map_tape_file_number: p.parity_map_tape_file_number,
            committed_bundle: value(&p.committed_bundle),
            sidecar_directory_entries: p
                .sidecar_directory_entries
                .iter()
                .map(|r| host::SidecarEpochDirectoryEntry {
                    tape_file_number: r.tape_file_number,
                    epoch_id: r.epoch_id,
                    protected_ordinal_start: r.protected_ordinal_start,
                    protected_ordinal_end_exclusive: r.protected_ordinal_end_exclusive,
                    sidecar_total_block_count: r.sidecar_total_block_count,
                    sidecar_header_block_count: r.sidecar_header_block_count,
                    parity_shard_block_count: r.parity_shard_block_count,
                    canonical_metadata_hash: r.canonical_metadata_hash,
                    flags: r.flags,
                })
                .collect(),
        };
        self.0.prepare_terminal(&native).map_err(error)
    }
    pub(crate) fn committed_snapshot_bounded(&self) -> Result<FrozenFileRows, JournalError> {
        self.0
            .committed_snapshot_bounded()
            .map(|rows| FrozenFileRows::new(Rows(rows, self.1.clone())))
            .map_err(error)
    }
    pub(crate) fn published_units_for_test(
        &self,
    ) -> Result<Vec<crate::FileRowsReport>, JournalError> {
        self.0
            .published_units_for_test()
            .map(|v| value(&v))
            .map_err(error)
    }
}
impl TapeReportPublisher for RowAuthority {
    fn tape_uuid(&self) -> [u8; 16] {
        self.0.tape_uuid()
    }
    fn publish_fixture(&mut self, p: &ShadowPublication) -> Result<(), JournalError> {
        self.0
            .publish_fixture(&host::authority::ShadowPublication {
                bundles: value(&p.bundles),
            })
            .map_err(error)
    }
    fn publish_commit(&mut self, p: &FilePublication) -> Result<PublicationReceipt, JournalError> {
        self.0
            .publish_commit(&publication(p))
            .map(receipt)
            .map_err(error)
    }
    fn publish_terminal_prefix(
        &mut self,
        p: &FilePublication,
    ) -> Result<PublicationReceipt, JournalError> {
        self.0
            .publish_terminal_prefix(&publication(p))
            .map(receipt)
            .map_err(error)
    }
    fn publish_terminal_component(
        &mut self,
        p: &FilePublication,
    ) -> Result<PublicationReceipt, JournalError> {
        self.0
            .publish_terminal_component(&publication(p))
            .map(receipt)
            .map_err(error)
    }
    fn load_committed(&self) -> Result<crate::FileRowsState, JournalError> {
        self.0
            .load_committed()
            .map(|s| crate::FileRowsState {
                entries: value(&s.entries),
                highest_protected_ordinal: s.highest_protected_ordinal,
                total_committed_ordinals: s.total_committed_ordinals,
                orphaned_bundles: value(&s.orphaned_bundles),
            })
            .map_err(error)
    }
    fn session_resume_snapshot(&self) -> Result<Option<FrozenFileRows>, JournalError> {
        self.committed_snapshot_bounded().map(Some)
    }
    fn committed_snapshot_bounded_authority(&self) -> Result<FrozenFileRows, JournalError> {
        self.committed_snapshot_bounded()
    }
}
impl StructuralAuthority for RowAuthority {
    fn fixture_publications(&self) -> Result<Vec<FilePublication>, JournalError> {
        self.0
            .fixture_publications()
            .map(|p| p.iter().map(local_publication).collect())
            .map_err(error)
    }
    fn freeze(&self) -> Result<FrozenFileRows, JournalError> {
        self.committed_snapshot_bounded()
    }
    fn freeze_before_terminal_prefix(
        &self,
        p: &FilePublication,
    ) -> Result<FrozenFileRows, JournalError> {
        self.0
            .freeze_before_terminal_prefix(&publication(p))
            .map(|r| FrozenFileRows::new(Rows(r, self.1.clone())))
            .map_err(error)
    }
    fn freeze_terminal_prefix(&self) -> Result<FrozenFileRows, JournalError> {
        self.0
            .freeze_terminal_prefix()
            .map(|r| FrozenFileRows::new(Rows(r, self.1.clone())))
            .map_err(error)
    }
    fn prefix_is_durable(&self, p: &FilePublication) -> Result<bool, JournalError> {
        self.0.prefix_is_durable(&publication(p)).map_err(error)
    }
    fn component_relation(
        &self,
        n: u8,
        p: &[FilePublication],
    ) -> Result<crate::TerminalComponentAuthorityRelation, JournalError> {
        self.0
            .component_relation(n, &p.iter().map(publication).collect::<Vec<_>>())
            .map(|_| crate::TerminalComponentAuthorityRelation::Aligned)
            .map_err(error)
    }
    fn pending_count(&self) -> usize {
        self.0.pending_count()
    }
    fn discard_suffix(&mut self, n: u64) -> Result<(), JournalError> {
        self.0.discard_suffix(n).map_err(error)
    }
    fn discard_pending(&mut self, p: &[FilePublication]) -> Result<(), JournalError> {
        self.0
            .discard_pending(&p.iter().map(publication).collect::<Vec<_>>())
            .map_err(error)
    }
}
fn metrics(m: host::BoundedJournalReplayMetrics) -> crate::BoundedJournalReplayMetrics {
    crate::BoundedJournalReplayMetrics {
        validation_passes: m.validation_passes,
        row_replay_passes: m.row_replay_passes,
        journal_record_count: m.journal_record_count,
        peak_record_payload_bytes: m.peak_record_payload_bytes,
        peak_live_entry_count: m.peak_live_entry_count,
    }
}
#[derive(Debug)]
struct Rows(host::FrozenFileRows, crate::ParityScheme);
impl CommittedFileRows for Rows {
    fn metrics(&self) -> crate::BoundedJournalReplayMetrics {
        metrics(self.0.metrics())
    }
    fn committed_entry_count(&self) -> u64 {
        self.0.committed_entry_count()
    }
    fn highest_protected_ordinal(&self) -> u64 {
        self.0.highest_protected_ordinal()
    }
    fn total_committed_ordinals(&self) -> u64 {
        self.0.total_committed_ordinals()
    }
    fn tape_uuid(&self) -> [u8; 16] {
        self.0.tape_uuid()
    }
    fn block_size(&self) -> u32 {
        self.0.block_size()
    }
    fn scheme(&self) -> &crate::ParityScheme {
        &self.1
    }
    fn try_clone_rows(&self) -> Result<Box<dyn CommittedFileRows>, JournalError> {
        self.0
            .try_clone()
            .map(|r| Box::new(Rows(r, self.1.clone())) as _)
            .map_err(error)
    }
    fn open_cursor(&self) -> Result<Box<dyn FileRowCursor>, JournalError> {
        self.0
            .replay()
            .map(|r| Box::new(Cursor(r)) as _)
            .map_err(error)
    }
}
#[derive(Debug)]
struct Cursor(Box<dyn host::authority::FileRowCursor>);
impl FileRowCursor for Cursor {
    fn committed_entry_count(&self) -> u64 {
        self.0.committed_entry_count()
    }
    fn highest_protected_ordinal(&self) -> u64 {
        self.0.highest_protected_ordinal()
    }
    fn total_committed_ordinals(&self) -> u64 {
        self.0.total_committed_ordinals()
    }
    fn next_entry(&mut self) -> Result<Option<crate::TapeFileEntry>, JournalError> {
        self.0
            .next_entry()
            .map(|r| r.as_ref().map(value))
            .map_err(error)
    }
    fn metrics(&self) -> crate::BoundedJournalReplayMetrics {
        metrics(self.0.metrics())
    }
}
pub(crate) fn save_authority(authority: &RowAuthority) -> Result<native::SavedRows, JournalError> {
    native::save_authority(&authority.0).map_err(error)
}
