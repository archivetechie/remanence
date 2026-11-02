//! Structural fixture inputs published through the native tape-log writer.
//! Synthetic catalog payloads supply identities for media-only parity tests.
use super::{
    TapeAuthority, TapeAuthorityLease, TapeAuthoritySnapshot, TapeAuthorityView,
    TapeAuthorityWriter,
};
use remanence_parity::{authority::*, *};
use std::path::{Path, PathBuf};

fn error(e: impl std::fmt::Display) -> JournalError {
    JournalError::Codec(e.to_string())
}

/// Native fixture builder for parity media tests and developer vectors.
#[derive(Debug)]
pub struct RowAuthorityBuilder {
    directory: PathBuf,
    uuid: [u8; 16],
    block: u32,
    scheme: ParityScheme,
    reports: Vec<ShadowPublication>,
    claims: Vec<FileRowsReport>,
    absent: Vec<(usize, usize)>,
}
impl RowAuthorityBuilder {
    /// Select a temporary directory and fixed media geometry.
    pub fn new(
        directory: impl AsRef<Path>,
        uuid: [u8; 16],
        block: u32,
        scheme: ParityScheme,
    ) -> Self {
        Self {
            directory: directory.as_ref().into(),
            uuid,
            block,
            scheme,
            reports: Vec::new(),
            claims: Vec::new(),
            absent: Vec::new(),
        }
    }
    /// Stage a complete fixture report.
    pub fn publication(mut self, report: ShadowPublication) -> Self {
        self.reports.push(report);
        self
    }
    /// Stage possibly invalid structural claims for native validation.
    pub fn claims(mut self, claims: Vec<FileRowsReport>) -> Self {
        self.claims = claims;
        self
    }
    /// Mark incomplete structural input before native publication.
    pub fn absent_block_counts(mut self, rows: Vec<(usize, usize)>) -> Self {
        self.absent = rows;
        self
    }
    /// Build a temporary native authority.
    pub fn build(self) -> Result<RowAuthority, JournalError> {
        self.build_mode(false)
    }
    /// Apply the production volume policy before constructing the fixture.
    pub fn build_trusted(self) -> Result<RowAuthority, JournalError> {
        self.build_mode(true)
    }
    fn build_mode(self, trusted: bool) -> Result<RowAuthority, JournalError> {
        std::fs::create_dir_all(&self.directory)?;
        if trusted {
            remanence_library::durability::validate_trusted_journal_volume(
                &self.directory.join("fixture"),
            )
            .map_err(JournalError::from)?;
        }
        if !self.absent.is_empty() {
            return Err(error("missing required block count"));
        }
        let authority = TapeAuthority::open_fixture(&self.directory, self.uuid).map_err(error)?;
        let owner = authority
            .acquire_exclusive_for_terminal_recovery()
            .map_err(|e| match e {
                crate::StateError::TapeLog(e) => match *e {
                    crate::tape_log::store::StoreError::Locked(e) => {
                        JournalError::Io(std::io::Error::from_raw_os_error(e as i32))
                    }
                    other => error(other),
                },
                other => error(other),
            })?;
        owner.open_structural(self.block, self.scheme.clone())?;
        let mut rows = RowAuthority {
            authority,
            uuid: self.uuid,
            owner: Some(owner),
            pending: Vec::new(),
            units: Vec::new(),
            block: self.block,
            scheme: self.scheme,
        };
        for report in self.reports {
            rows.publish_fixture(&report)?;
        }
        if !self.claims.is_empty() {
            rows.publish_fixture(&ShadowPublication {
                bundles: self.claims,
            })?;
        }
        Ok(rows)
    }
}

/// Retained native writer used by structural fixtures.
#[derive(Debug)]
pub struct RowAuthority {
    authority: TapeAuthority,
    uuid: [u8; 16],
    owner: Option<TapeAuthorityLease>,
    pending: Vec<FileRowsReport>,
    units: Vec<FileRowsReport>,
    block: u32,
    scheme: ParityScheme,
}
impl RowAuthority {
    /// Native file location for opaque evidence operations.
    pub fn path(&self) -> &Path {
        self.authority.path()
    }
    fn owner(&self) -> Result<&TapeAuthorityLease, JournalError> {
        self.owner
            .as_ref()
            .ok_or_else(|| error("fixture writer released after corruption"))
    }
    fn owner_mut(&mut self) -> Result<&mut TapeAuthorityLease, JournalError> {
        self.owner
            .as_mut()
            .ok_or_else(|| error("fixture writer released after corruption"))
    }
    fn rows(&self) -> Result<StructuralCapability, JournalError> {
        self.owner()?
            .open_structural(self.block, self.scheme.clone())
    }
    /// Freeze the exact native prefix, refusing unclosed fixture claims.
    pub fn committed_snapshot_bounded(&self) -> Result<FrozenFileRows, JournalError> {
        if !self.pending.is_empty() {
            return Err(error("unclosed structural fixture claims"));
        }
        if self.owner.is_none() {
            let snapshot = self.authority.freeze().map_err(error)?;
            if snapshot.raw.tail() != crate::tape_log::codec::Tail::Complete {
                return Err(error("unclosed structural fixture claims"));
            }
            return snapshot
                .into_file_rows()
                .ok_or_else(|| error("missing parity rows"));
        }
        self.rows()?.freeze()
    }
    /// Enumerate the reports supplied during this fixture's lifetime.
    pub fn published_units_for_test(&self) -> Result<Vec<FileRowsReport>, JournalError> {
        Ok(self.units.clone())
    }
    /// Persist an immutable native plan before fixture terminal media motion.
    pub fn prepare_terminal(&mut self, prefix: &TerminalPrefixPlan) -> Result<(), JournalError> {
        let plan = super::AuthorityFinalization {
            tape_uuid: self.uuid,
            trigger: TerminalFinalizationTrigger::ReachedLowWatermark,
            manual: None,
            progress: super::TerminalFinalizationProgress::BeforeReplicaA,
            recovery_required: false,
            edition_id: [1; 16],
            edition_sequence: 1,
            edition_digest: [1; 32],
            writer_version: prefix.diagnostics.writer_version.clone(),
            write_timestamp: prefix.diagnostics.write_timestamp.clone(),
            terminal_prefix: Some(prefix.into()),
            layout: TerminalTailLayout::new(
                0,
                self.block,
                prefix.tail_start_tape_file_number,
                prefix.tail_start_lba,
                4,
                4096,
            )
            .map_err(error)?
            .try_into()
            .map_err(error)?,
        };
        self.owner_mut()?.begin_finalization(&plan).map_err(error)?;
        Ok(())
    }
    fn close_group(&mut self, protected: u64, total: u64) -> Result<(), JournalError> {
        if let Some(at) = self
            .pending
            .iter()
            .position(|r| r.kind == FileRowsKind::TerminalPrefix)
        {
            let suffix = self.pending.split_off(at);
            let [prefix] = suffix.as_slice() else {
                return Err(error("fixture terminal prefix must be the final report"));
            };
            if !self.pending.is_empty() {
                let ordinary = self.pending.last().unwrap();
                self.close_group(
                    ordinary.highest_protected_ordinal,
                    ordinary.total_committed_ordinals,
                )?;
            }
            let snapshot = self.rows()?.freeze()?;
            let mut plan = plan_checkpointed_terminal_index_close(
                &snapshot,
                ParityMapDiagnostics {
                    writer_version: "native-fixture".into(),
                    write_timestamp: "2026-01-01T00:00:00Z".into(),
                },
            )
            .map_err(error)?;
            // Preserve the supplied physical claim, including its metadata hash.
            // The native plan/group validator rejects contradictory coordinates.
            plan.committed_bundle = prefix.clone();
            plan.tail_start_tape_file_number = prefix
                .entries
                .last()
                .map(|r| {
                    r.tape_file_number
                        .checked_add(1)
                        .ok_or_else(|| error("fixture file number overflow"))
                })
                .transpose()?
                .unwrap_or(plan.start_tape_file_number);
            plan.tail_start_lba = prefix
                .entries
                .last()
                .map(|r| {
                    r.physical_start_hint
                        .unwrap_or(plan.start_lba)
                        .checked_add(r.block_count)
                        .and_then(|v| v.checked_add(1))
                        .ok_or_else(|| error("fixture extent overflow"))
                })
                .transpose()?
                .unwrap_or(plan.start_lba);
            if (protected, total)
                != (
                    prefix.highest_protected_ordinal,
                    prefix.total_committed_ordinals,
                )
            {
                return Err(error(
                    "fixture prefix counters differ from closing boundary",
                ));
            }
            self.prepare_terminal(&plan)?;
            self.owner_mut()?
                .publish_terminal_prefix(&FilePublication::try_from(prefix)?)?;
            return Ok(());
        }
        let summary = self.owner()?.authority_summary().map_err(error)?;
        let mut object_files = Vec::<FilePublication>::new();
        let mut barrier: Option<FilePublication> = None;
        for bundle in &self.pending {
            let publication = FilePublication::try_from(bundle)?;
            match publication.purpose {
                FilePurpose::Bootstrap => {
                    let expected = TapeFileEntry {
                        tape_file_number: 0,
                        kind: TapeFileKind::Bootstrap,
                        block_count: 1,
                        physical_start_hint: Some(0),
                        object_id: None,
                        first_parity_data_ordinal: None,
                        epoch_id: None,
                        protected_ordinal_start: None,
                        protected_ordinal_end_exclusive: None,
                        canonical_metadata_hash: None,
                        object_recovery_row: None,
                    };
                    let [row] = publication.files.as_slice() else {
                        return Err(error("fixture Bootstrap must have one row"));
                    };
                    let mut supplied = row.clone();
                    supplied.physical_start_hint.get_or_insert(0);
                    if supplied != expected || publication.protected != 0 || publication.total != 0
                    {
                        return Err(error(
                            "fixture Bootstrap differs from native initialization",
                        ));
                    }
                }
                FilePurpose::Object => object_files.push(publication),
                FilePurpose::Sidecars(_) => {
                    if let Some(last) = object_files.last_mut() {
                        last.files.extend(publication.files);
                        last.protected = publication.protected;
                        last.total = publication.total;
                    } else if let Some(prior) = &mut barrier {
                        prior.files.extend(publication.files);
                        prior.protected = publication.protected;
                        prior.total = publication.total;
                    } else {
                        barrier = Some(publication);
                    }
                }
                _ => return Err(error("terminal fixtures require a native immutable plan")),
            }
        }
        let mut cursor = summary.cursor.map_or(2, |p| p.lba);
        let mut payloads = Vec::new();
        for report in &mut object_files {
            for row in &mut report.files {
                row.physical_start_hint.get_or_insert(cursor);
                cursor = row
                    .physical_start_hint
                    .unwrap()
                    .checked_add(row.block_count)
                    .and_then(|n| n.checked_add(1))
                    .ok_or_else(|| error("fixture extent overflow"))?;
            }
            let row = &mut report.files[0];
            let object_id = row
                .object_id
                .clone()
                .unwrap_or_else(|| format!("fixture-{}", row.tape_file_number));
            let recovery = row.object_recovery_row.clone().unwrap_or_else(|| {
                ObjectRecoveryRow::plaintext(
                    row.tape_file_number,
                    row.block_count,
                    0,
                    1,
                    1,
                    [1; 32],
                )
                .with_object_id(object_id.as_bytes())
            });
            row.object_id = Some(object_id.clone());
            row.object_recovery_row = Some(recovery.clone());
            payloads.push(super::ObjectPayload {
                projection: crate::tape_log::payload::CommittedObjectProjection {
                    object: crate::NativeObjectProjectionInput {
                        object_id: object_id.clone(),
                        caller_object_id: None,
                        body_format: "rem-object-v1".into(),
                        logical_size_bytes: Some(1),
                        content_hash: None,
                        metadata_hash: None,
                        created_at_utc: None,
                    },
                    copy: crate::NativeObjectCopyProjectionInput {
                        object_id,
                        tape_uuid: summary.tape_uuid,
                        tape_file_number: row.tape_file_number,
                        first_body_lba: 0,
                        first_parity_data_ordinal: row.first_parity_data_ordinal,
                        protected_until_ordinal: Some(report.protected),
                        status: "committed".into(),
                        representation: "plaintext".into(),
                        recipient_epoch_ids: None,
                        metadata_frame_len: None,
                        plaintext_digest: None,
                        stored_digest: None,
                    },
                    block_size: self.block,
                    block_count: row.block_count,
                    fresh_tape: summary.checkpoint_ordinal == 0,
                    total_committed_ordinals: report.total,
                    object_recovery_row: recovery,
                },
                files: Vec::new(),
            });
        }
        if let Some(barrier) = &mut barrier {
            for row in &mut barrier.files {
                row.physical_start_hint.get_or_insert(cursor);
                cursor = row
                    .physical_start_hint
                    .unwrap()
                    .checked_add(row.block_count)
                    .and_then(|n| n.checked_add(1))
                    .ok_or_else(|| error("fixture extent overflow"))?;
            }
        }
        let next_file = object_files
            .iter()
            .flat_map(|p| p.files.iter())
            .chain(barrier.iter().flat_map(|p| p.files.iter()))
            .last()
            .map(|r| {
                r.tape_file_number
                    .checked_add(1)
                    .ok_or_else(|| error("fixture file number overflow"))
            })
            .transpose()?
            .unwrap_or(1);
        let report = super::AuthorityCommit {
            session_id: Some([9; 16]),
            committed_at_utc: "2026-11-02T00:00:00Z".into(),
            ordinal: summary
                .checkpoint_ordinal
                .checked_add(1)
                .ok_or_else(|| error("fixture ordinal overflow"))?,
            object_count: summary
                .object_count
                .checked_add(payloads.len() as u64)
                .ok_or_else(|| error("fixture object count overflow"))?,
            protected,
            total,
            cursor: CommitCursor {
                partition: 0,
                lba: cursor,
            },
            tape_uuid: summary.tape_uuid,
            operation_id: [1; 16],
            next_file,
            block_size: self.block,
            payloads,
            scheme: Some(self.scheme.clone()),
            object_files,
            barrier,
            finalization: None,
            sealed: false,
            seal_due: None,
            check: (total > protected).then_some((CheckValue(0), CheckProvenance::OriginalWrite)),
        };
        self.owner_mut()?.publish_report(&report).map_err(error)?;
        self.pending.clear();
        Ok(())
    }
}
impl TapeReportPublisher for RowAuthority {
    fn tape_uuid(&self) -> [u8; 16] {
        self.uuid
    }
    fn publish_fixture(&mut self, report: &ShadowPublication) -> Result<(), JournalError> {
        for unit in &report.bundles {
            if unit.kind == FileRowsKind::FixtureBoundary {
                self.close_group(
                    unit.highest_protected_ordinal,
                    unit.total_committed_ordinals,
                )?;
            } else {
                self.pending.push(unit.clone());
            }
            self.units.push(unit.clone());
        }
        Ok(())
    }
    fn publish_terminal_prefix(
        &mut self,
        p: &FilePublication,
    ) -> Result<PublicationReceipt, JournalError> {
        self.owner_mut()?.publish_terminal_prefix(p)
    }
    fn publish_terminal_component(
        &mut self,
        p: &FilePublication,
    ) -> Result<PublicationReceipt, JournalError> {
        self.owner_mut()?.publish_terminal_component(p)
    }
    fn load_committed(&self) -> Result<FileRowsState, JournalError> {
        let snapshot = self.rows()?.freeze()?;
        let mut cursor = snapshot.replay()?;
        let mut entries = Vec::new();
        while let Some(row) = cursor.next_entry()? {
            entries.push(row);
        }
        Ok(FileRowsState {
            entries,
            highest_protected_ordinal: snapshot.highest_protected_ordinal(),
            total_committed_ordinals: snapshot.total_committed_ordinals(),
            orphaned_bundles: self.pending.clone(),
        })
    }
    fn committed_snapshot_bounded_authority(&self) -> Result<FrozenFileRows, JournalError> {
        self.committed_snapshot_bounded()
    }
    fn session_resume_snapshot(&self) -> Result<Option<FrozenFileRows>, JournalError> {
        self.committed_snapshot_bounded().map(Some)
    }
}
impl StructuralAuthority for RowAuthority {
    fn fixture_publications(&self) -> Result<Vec<FilePublication>, JournalError> {
        self.units
            .iter()
            .filter(|b| b.kind != FileRowsKind::FixtureBoundary)
            .map(FilePublication::try_from)
            .collect()
    }
    fn freeze(&self) -> Result<FrozenFileRows, JournalError> {
        self.committed_snapshot_bounded()
    }
    fn freeze_before_terminal_prefix(
        &self,
        p: &FilePublication,
    ) -> Result<FrozenFileRows, JournalError> {
        self.rows()?.freeze_before_terminal_prefix(p)
    }
    fn freeze_terminal_prefix(&self) -> Result<FrozenFileRows, JournalError> {
        self.rows()?.freeze_terminal_prefix()
    }
    fn prefix_is_durable(&self, p: &FilePublication) -> Result<bool, JournalError> {
        self.rows()?.prefix_is_durable(p)
    }
    fn component_relation(
        &self,
        n: u8,
        p: &[FilePublication],
    ) -> Result<TerminalComponentAuthorityRelation, JournalError> {
        self.rows()?.component_relation(n, p)
    }
    fn pending_count(&self) -> usize {
        self.pending.len()
    }
    fn discard_suffix(&mut self, n: u64) -> Result<(), JournalError> {
        self.pending
            .retain(|p| p.entries.iter().all(|r| r.tape_file_number < n));
        Ok(())
    }
    fn discard_pending(&mut self, expected: &[FilePublication]) -> Result<(), JournalError> {
        if self
            .pending
            .iter()
            .map(FilePublication::try_from)
            .collect::<Result<Vec<_>, _>>()?
            != expected
        {
            return Err(error("pending fixture mismatch"));
        }
        self.pending.clear();
        Ok(())
    }
}

/// Opaque native bytes saved before a simulated loss of host publication.
pub struct SavedRows {
    path: PathBuf,
    bytes: Option<Vec<u8>>,
}
impl SavedRows {
    /// Restore only the temporary fixture authority, after its lease is dropped.
    pub fn restore(self) -> Result<(), JournalError> {
        if let Some(bytes) = self.bytes {
            std::fs::write(self.path, bytes)?;
        } else if self.path.exists() {
            std::fs::remove_file(self.path)?;
        }
        Ok(())
    }
}
/// Save the exact native authority bytes without exposing its encoder.
pub fn save_authority(authority: &RowAuthority) -> Result<SavedRows, JournalError> {
    Ok(SavedRows {
        path: authority.path().into(),
        bytes: match std::fs::read(authority.path()) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        },
    })
}
/// Native fixture evidence temporarily absent from its active path.
pub struct HiddenRowAuthority {
    original: PathBuf,
    saved: PathBuf,
}
impl HiddenRowAuthority {
    /// Restore exactly the saved bytes.
    pub fn restore(self) -> Result<(), JournalError> {
        std::fs::rename(self.saved, self.original)?;
        Ok(())
    }
}
/// Hide authority in a temporary fixture directory.
pub fn hide_authority(location: impl AsRef<Path>) -> Result<HiddenRowAuthority, JournalError> {
    let original = location.as_ref().to_path_buf();
    let saved = original.with_extension("saved");
    std::fs::rename(&original, &saved)?;
    Ok(HiddenRowAuthority { original, saved })
}

/// Semantic contradictions encoded only with the native frame encoder.
#[derive(Clone, Copy, Debug)]
pub enum RowFixtureFault {
    /// Remove the initializing Bootstrap proof.
    MissingFirst,
    /// Repeat one Object file number.
    DuplicateObject,
    /// Contradict an Object extent.
    ConflictingObject,
    /// Inject a claimed file extent and closing total into native frames.
    FileExtent {
        /// Structural file whose extent is changed.
        tape_file_number: u64,
        /// Claimed extent, also retained in its recovery row.
        block_count: u64,
        /// Claimed closing total.
        total: u64,
    },
    /// Contradict W at the barrier.
    ProtectedWatermark,
    /// Contradict T at the barrier.
    TotalWatermark,
    /// Tear the closing frame.
    IncompleteFinal,
}
/// Mutate native fixture frames and release the writer so the next read validates disk.
pub fn inject_row_fault(
    authority: &mut RowAuthority,
    fault: RowFixtureFault,
) -> Result<(), JournalError> {
    use crate::tape_log::{
        codec::{encode_frame, Decoder},
        store::Header,
        Chunk, Record,
    };
    let bytes = std::fs::read(authority.path())?;
    let mut input = std::io::Cursor::new(&bytes);
    Header::decode(&mut input, authority.uuid).map_err(error)?;
    let start = input.position();
    let mut decoder = Decoder::at(input, 1, start);
    let mut frames = Vec::new();
    while let Some(frame) = decoder.next_frame().map_err(error)? {
        frames.push((frame.record, frame.payload.to_vec()));
    }
    match fault {
        RowFixtureFault::MissingFirst => {
            frames.remove(0);
        }
        RowFixtureFault::DuplicateObject | RowFixtureFault::ConflictingObject => {
            let index=frames.iter().position(|(record,_)|matches!(record,Record::PayloadContinuation(c) if matches!(&c.chunk,Chunk::FileRow(row) if row.kind==TapeFileKind::Object))).ok_or_else(||error("fixture has no Object"))?;
            if matches!(fault, RowFixtureFault::DuplicateObject) {
                frames.insert(index + 1, frames[index].clone());
            } else if let Record::PayloadContinuation(c) = &mut frames[index].0 {
                if let Chunk::FileRow(row) = &mut c.chunk {
                    row.block_count += 1;
                }
            }
        }
        RowFixtureFault::FileExtent {
            tape_file_number,
            block_count,
            total,
        } => {
            let row = frames
                .iter_mut()
                .find_map(|(record, _)| match record {
                    Record::PayloadContinuation(c) => match &mut c.chunk {
                        Chunk::FileRow(row) if row.tape_file_number == tape_file_number => {
                            Some(row)
                        }
                        _ => None,
                    },
                    _ => None,
                })
                .ok_or_else(|| error("fixture has no matching file"))?;
            row.block_count = block_count;
            if let Some(recovery) = row.object_recovery_row.as_mut() {
                recovery.stored_block_count = block_count;
            }
            let commit = frames
                .iter_mut()
                .rev()
                .find_map(|(record, _)| match record {
                    Record::Committed(commit) => Some(commit),
                    _ => None,
                })
                .ok_or_else(|| error("fixture has no commit"))?;
            commit.total = total;
        }
        RowFixtureFault::ProtectedWatermark | RowFixtureFault::TotalWatermark => {
            let commit = frames
                .iter_mut()
                .rev()
                .find_map(|(record, _)| {
                    if let Record::Committed(c) = record {
                        Some(c)
                    } else {
                        None
                    }
                })
                .ok_or_else(|| error("fixture has no commit"))?;
            if matches!(fault, RowFixtureFault::ProtectedWatermark) {
                commit.protected += 1;
            } else {
                commit.total += 1;
            }
        }
        RowFixtureFault::IncompleteFinal => {}
    }
    let mut damaged = bytes[..start as usize].to_vec();
    for (i, (record, payload)) in frames.iter().enumerate() {
        encode_frame(&mut damaged, i as u64 + 1, record, payload).map_err(error)?;
    }
    if matches!(fault, RowFixtureFault::IncompleteFinal) {
        damaged.truncate(damaged.len() - 1);
    }
    authority.owner.take();
    std::fs::write(authority.path(), damaged)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Native initialization must not silently replace a contradictory BOT claim.
    #[test]
    fn native_fixture_preserves_bootstrap_claim_refusal() {
        let uuid = [0x39; 16];
        let record = crate::checkpoint::tests::partial_epoch_parity_record(uuid);
        let scheme = record.scheme.clone().unwrap();
        let mut claims = record.object_tape_file_bundles.clone();
        claims.push(record.barrier_bundle.clone().unwrap());
        claims.push(FileRowsReport {
            kind: FileRowsKind::FixtureBoundary,
            entries: Vec::new(),
            highest_protected_ordinal: 3,
            total_committed_ordinals: 3,
        });
        let control = tempfile::tempdir().unwrap();
        let authority =
            RowAuthorityBuilder::new(control.path(), uuid, record.block_size, scheme.clone())
                .claims(claims.clone())
                .build()
                .unwrap();
        let bootstrap = authority
            .committed_snapshot_bounded()
            .unwrap()
            .replay()
            .unwrap()
            .next_entry()
            .unwrap()
            .unwrap();
        claims.insert(
            0,
            FileRowsReport {
                kind: FileRowsKind::BotBootstrap,
                entries: vec![bootstrap],
                highest_protected_ordinal: 0,
                total_committed_ordinals: 0,
            },
        );
        let explicit = tempfile::tempdir().unwrap();
        RowAuthorityBuilder::new(explicit.path(), uuid, record.block_size, scheme.clone())
            .claims(claims.clone())
            .build()
            .unwrap();
        claims[0].entries[0].block_count = 2;
        let hostile = tempfile::tempdir().unwrap();
        let error = RowAuthorityBuilder::new(hostile.path(), uuid, record.block_size, scheme)
            .claims(claims)
            .build()
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("Bootstrap differs from native initialization"));
    }
}
