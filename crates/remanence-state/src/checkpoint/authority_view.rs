//! Old-format implementation of the shared read/write contracts. Frozen reads
//! retain the validated checkpoint boundary, never a borrow of the writer.
use super::*;
use remanence_parity::authority::{AuthoritySummary, TapeAuthoritySnapshot, TapeAuthorityView};
use remanence_parity::{CommitCursor, ObjectRecoveryRow, TapeFileEntry, TapeFileMapEntry};

/// Owned catalog/structural snapshot of the selected old authority prefix.
#[derive(Debug)]
pub struct AuthoritySnapshot {
    checkpoint: Option<CheckpointReplaySnapshot>,
    summary: AuthoritySummary,
    intent: Option<TerminalFinalizationIntent>,
    neutral_intent: Option<crate::authority::AuthorityFinalization>,
    parity_rows: Option<remanence_parity::FrozenFileRows>,
    prefix_durable: bool,
}

impl AuthoritySnapshot {
    fn capture(
        path: &Path,
        tape_uuid: [u8; 16],
        retained: Option<&dyn remanence_parity::authority::StructuralAuthority>,
    ) -> Result<Self, StateError> {
        let file = match File::open(path) {
            Ok(file) => Some(file),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(StateError::io_at("open frozen authority", path, error)),
        };
        let checkpoint = file
            .map(|file| {
                let replay_end = file
                    .metadata()
                    .map_err(|error| StateError::io_at("stat frozen authority", path, error))?
                    .len();
                Ok::<_, StateError>(CheckpointReplaySnapshot {
                    file,
                    path: path.to_path_buf(),
                    tape_uuid,
                    replay_end,
                    metrics: CheckpointBoundedReplayMetrics::default(),
                })
            })
            .transpose()?;
        let mut summary = AuthoritySummary {
            tape_uuid,
            checkpoint_ordinal: 0,
            object_count: 0,
            cursor: None,
            protected: 0,
            total: 0,
            next_file: 0,
            block_size: None,
            seal_due: None,
            sealed: false,
        };
        let mut tail = Vec::with_capacity(2);
        if let Some(snapshot) = &checkpoint {
            snapshot.visit_records(|record| {
                summary.checkpoint_ordinal = record.ordinal;
                summary.object_count = record.committed_object_count;
                summary.cursor = Some(CommitCursor {
                    partition: record.eod_partition,
                    lba: record.eod_lba,
                });
                summary.next_file = record.next_tape_file_number;
                summary.block_size = Some(record.block_size);
                summary.seal_due = record.seal_due;
                summary.sealed = record.sealed_after_write;
                if let Some(bundle) = record
                    .barrier_bundle
                    .as_ref()
                    .or_else(|| record.object_tape_file_bundles.last())
                {
                    summary.protected = bundle.highest_protected_ordinal;
                    summary.total = bundle.total_committed_ordinals;
                } else if let Some(object) = record.objects.last() {
                    summary.total = object.total_committed_ordinals;
                }
                if tail.len() == 2 {
                    tail.remove(0);
                }
                tail.push(record.clone());
                Ok(())
            })?;
        }
        let pending = read_terminal_finalization_intent(path, tape_uuid)?;
        let intent = pending.or_else(|| {
            tail.last()
                .and_then(|record| record.terminal_finalization.clone())
        });
        if let Some(intent) = &intent {
            intent.validate_for_checkpoint_prefix(finalization_base_record(&tail))?;
        }
        if !summary.sealed {
            if let Some(plan) = &intent {
                let count = completed_components(plan.progress);
                if count > 0 {
                    let last = plan.layout.components[count - 1];
                    summary.cursor = Some(CommitCursor {
                        partition: plan.layout.partition,
                        lba: last
                            .start_lba
                            .checked_add(last.record_count)
                            .and_then(|lba| lba.checked_add(1))
                            .ok_or_else(|| {
                                StateError::JournalReplayFailed("terminal cursor overflows".into())
                            })?,
                    });
                    summary.next_file = last.tape_file_number.checked_add(1).ok_or_else(|| {
                        StateError::JournalReplayFailed("terminal file number overflows".into())
                    })?;
                    if let Some(prefix) = &plan.terminal_prefix {
                        summary.protected = prefix.committed_bundle.highest_protected_ordinal;
                        summary.total = prefix.committed_bundle.total_committed_ordinals;
                    }
                }
            }
        }
        let neutral_intent = intent.as_ref().map(Into::into);
        let mut snapshot = Self {
            neutral_intent,
            checkpoint,
            summary,
            intent,
            parity_rows: None,
            prefix_durable: false,
        };
        // This is the old backend's agreement check, kept out of consumers.
        // An ordinary frozen view must describe precisely the same two prefixes.
        if tail.last().is_some_and(|record| record.scheme.is_some()) {
            let parity_path = crate::authority::structural_path(path, tape_uuid)
                .map_err(|error| StateError::JournalReplayFailed(error.to_string()))?;
            let rows = if let Some(retained) = retained {
                retained
                    .freeze()
                    .map_err(|error| StateError::JournalReplayFailed(error.to_string()))?
            } else {
                let reader = remanence_parity::TapeRowAuthority::open_shared_existing_for_replay(
                    parity_path,
                )
                .map_err(|error| StateError::JournalReplayFailed(error.to_string()))?;
                reader
                    .frozen_rows()
                    .map_err(|error| StateError::JournalReplayFailed(error.to_string()))?
            };
            if let Some(prefix) = snapshot.intent.as_ref().and_then(|intent| {
                (intent.progress == TerminalFinalizationProgress::BeforeReplicaA)
                    .then_some(intent.terminal_prefix.as_ref())
                    .flatten()
            }) {
                if rows.committed_entry_count() == prefix.tail_start_tape_file_number {
                    snapshot.prefix_durable = true;
                    snapshot.summary.cursor = Some(CommitCursor {
                        partition: 0,
                        lba: prefix.tail_start_lba,
                    });
                    snapshot.summary.next_file = prefix.tail_start_tape_file_number;
                    snapshot.summary.protected = prefix.committed_bundle.highest_protected_ordinal;
                    snapshot.summary.total = prefix.committed_bundle.total_committed_ordinals;
                }
            }
            if !snapshot.summary.sealed && snapshot.intent.is_none() {
                validate_replay_backed_parity_terminal_rows(
                    snapshot.checkpoint.as_ref().ok_or_else(|| {
                        StateError::JournalReplayFailed(
                            "missing checkpoint at frozen boundary".into(),
                        )
                    })?,
                    &rows,
                    ReplayTerminalPrefixMode::Absent,
                )?;
            } else {
                // Recovery has its own scoped one-transition-ahead validation.
                // A normal frozen view must never present a planned row as durable.
                snapshot.validate_terminal_agreement(
                    &rows,
                    tail.last().ok_or_else(|| {
                        StateError::JournalReplayFailed("missing commit at frozen boundary".into())
                    })?,
                )?;
            }
            snapshot.parity_rows = Some(rows);
        }
        if snapshot.summary.cursor.is_none() {
            if let Some(retained) = retained {
                let rows = retained
                    .freeze()
                    .map_err(|error| StateError::JournalReplayFailed(error.to_string()))?;
                if rows.committed_entry_count() != 0 {
                    return Err(StateError::JournalReplayFailed(
                        "parity journal has a committed prefix but checkpoint authority is empty"
                            .into(),
                    ));
                }
                snapshot.parity_rows = Some(rows);
            } else if path.parent().and_then(Path::parent).is_some() {
                let parity_path = crate::authority::structural_path(path, tape_uuid)
                    .map_err(|error| StateError::JournalReplayFailed(error.to_string()))?;
                match remanence_parity::TapeRowAuthority::open_shared_existing_for_replay(
                    parity_path,
                ) {
                    Ok(reader) => {
                        let rows = reader
                            .frozen_rows()
                            .map_err(|error| StateError::JournalReplayFailed(error.to_string()))?;
                        if rows.committed_entry_count() != 0 {
                            return Err(StateError::JournalReplayFailed(
                                "parity journal has a committed prefix but checkpoint authority is empty".into(),
                            ));
                        }
                    }
                    Err(remanence_parity::JournalError::Io(error))
                        if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(StateError::JournalReplayFailed(error.to_string())),
                }
            }
        }
        Ok(snapshot)
    }

    fn validate_terminal_agreement(
        &self,
        rows: &remanence_parity::FrozenFileRows,
        last: &CheckpointJournalRecord,
    ) -> Result<(), StateError> {
        let mismatch = |detail: &str| {
            StateError::JournalReplayFailed(format!("frozen terminal authority mismatch: {detail}"))
        };
        if rows.tape_uuid() != self.summary.tape_uuid
            || rows.block_size() != last.block_size
            || Some(rows.scheme()) != last.scheme.as_ref()
            || rows.highest_protected_ordinal() != self.summary.protected
            || rows.total_committed_ordinals() != self.summary.total
            || rows.committed_entry_count() != self.summary.next_file
        {
            return Err(mismatch(
                "identity, geometry, counters or position disagree",
            ));
        }
        let mut cursor = rows
            .replay()
            .map_err(|error| mismatch(&error.to_string()))?;
        self.visit_file_rows(&mut |expected| {
            let actual = cursor
                .next_entry()
                .map_err(|error| mismatch(&error.to_string()))?
                .ok_or_else(|| mismatch("structural authority ends before committed plan"))?;
            let agrees = if expected.tape_file_number == 0 {
                actual.tape_file_number == 0
                    && actual.kind == remanence_parity::TapeFileKind::Bootstrap
                    && actual.block_count == 1
            } else {
                parity_resume_entries_match(&actual, expected)
            };
            if !agrees {
                return Err(mismatch("structural authority differs from committed plan"));
            }
            Ok(())
        })?;
        if cursor
            .next_entry()
            .map_err(|error| mismatch(&error.to_string()))?
            .is_some()
        {
            return Err(mismatch(
                "structural authority extends beyond committed plan",
            ));
        }
        Ok(())
    }

    fn visit_records(
        &self,
        visitor: impl FnMut(&CheckpointJournalRecord) -> Result<(), StateError>,
    ) -> Result<(), StateError> {
        let Some(snapshot) = &self.checkpoint else {
            return Ok(());
        };
        // File::try_clone shares the seek offset. Each public immutable pass
        // instead opens its own description, including nested visitor passes.
        let mut file = File::open(&snapshot.path).map_err(|error| {
            StateError::io_at("open frozen authority cursor", &snapshot.path, error)
        })?;
        let metadata = file.metadata().map_err(|error| {
            StateError::io_at("stat frozen authority cursor", &snapshot.path, error)
        })?;
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::MetadataExt;
            let original = snapshot.file.metadata().map_err(|error| {
                StateError::io_at("stat frozen authority inode", &snapshot.path, error)
            })?;
            if original.dev() != metadata.dev() || original.ino() != metadata.ino() {
                return Err(StateError::JournalReplayFailed(
                    "checkpoint path no longer names frozen authority".into(),
                ));
            }
        }
        if metadata.len() < snapshot.replay_end {
            return Err(StateError::JournalReplayFailed(
                "checkpoint was truncated before frozen authority".into(),
            ));
        }
        visit_checkpoint_records_bounded_to(
            &mut file,
            snapshot.tape_uuid,
            &snapshot.path,
            snapshot.replay_end,
            visitor,
        )?;
        Ok(())
    }

    /// Frozen cumulative counters and position.
    pub fn summary(&self) -> &AuthoritySummary {
        &self.summary
    }
    /// Frozen finalization plan and progress.
    pub fn finalization(&self) -> Option<&crate::authority::AuthorityFinalization> {
        self.neutral_intent.as_ref()
    }
}

impl TapeAuthoritySnapshot for AuthoritySnapshot {
    type Error = StateError;
    type Payload = crate::authority::CatalogPayload;
    type Finalization = crate::authority::AuthorityFinalization;
    fn summary(&self) -> &AuthoritySummary {
        &self.summary
    }
    fn finalization(&self) -> Option<&Self::Finalization> {
        self.neutral_intent.as_ref()
    }

    fn into_file_rows(self) -> Option<remanence_parity::FrozenFileRows> {
        self.parity_rows
    }

    fn visit_file_rows(
        &self,
        visitor: &mut dyn FnMut(&TapeFileEntry) -> Result<(), StateError>,
    ) -> Result<(), StateError> {
        if let Some(rows) = &self.parity_rows {
            let mut cursor = rows
                .replay()
                .map_err(|error| StateError::JournalReplayFailed(error.to_string()))?;
            while let Some(row) = cursor
                .next_entry()
                .map_err(|error| StateError::JournalReplayFailed(error.to_string()))?
            {
                visitor(&row)?;
            }
            return Ok(());
        }
        let Some(_checkpoint) = &self.checkpoint else {
            return Ok(());
        };
        if self.summary.cursor.is_none() {
            return Ok(());
        }
        let mut bot = TapeFileEntry::from_map_entry(TapeFileMapEntry::bootstrap(0, 1));
        bot.physical_start_hint = Some(0);
        visitor(&bot)?;
        self.visit_records(|record| {
            if let Some(completed) = &record.terminal_finalization {
                return visit_terminal_rows(completed, 5, visitor);
            }
            if record.scheme.is_some() {
                for row in record
                    .object_tape_file_bundles
                    .iter()
                    .chain(record.barrier_bundle.iter())
                    .flat_map(|bundle| &bundle.entries)
                {
                    visitor(row)?;
                }
            } else {
                let span = record
                    .objects
                    .iter()
                    .try_fold(0u64, |sum, object| {
                        sum.checked_add(object.block_count)?.checked_add(1)
                    })
                    .ok_or_else(|| {
                        StateError::JournalReplayFailed("checkpoint Object span overflows".into())
                    })?;
                let mut start = record.eod_lba.checked_sub(span).ok_or_else(|| {
                    StateError::JournalReplayFailed("checkpoint Object span exceeds EOD".into())
                })?;
                for object in &record.objects {
                    let first = object
                        .total_committed_ordinals
                        .checked_sub(object.block_count)
                        .ok_or_else(|| {
                            StateError::JournalReplayFailed(
                                "checkpoint Object ordinal underflow".into(),
                            )
                        })?;
                    let mut row = TapeFileEntry::from_map_entry(TapeFileMapEntry::object(
                        object.copy.tape_file_number,
                        object.block_count,
                        first,
                    ));
                    row.physical_start_hint = Some(start);
                    row.object_id = Some(object.object.object_id.clone());
                    row.object_recovery_row = Some(object.object_recovery_row.to_parity_row());
                    visitor(&row)?;
                    start += object.block_count + 1; // bounded by the checked span above
                }
            }
            Ok(())
        })?;
        if !self.summary.sealed {
            if let Some(intent) = &self.intent {
                let count = completed_components(intent.progress);
                if count > 0 || self.prefix_durable {
                    visit_terminal_rows(intent, count, visitor)?;
                }
            }
        }
        Ok(())
    }

    fn visit_object_rows(
        &self,
        visitor: &mut dyn FnMut(&ObjectRecoveryRow) -> Result<(), StateError>,
    ) -> Result<(), StateError> {
        if self.checkpoint.is_some() {
            self.visit_records(|record| {
                for object in &record.objects {
                    visitor(&object.object_recovery_row.to_parity_row())?;
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    fn visit_payloads(
        &self,
        visitor: &mut remanence_parity::authority::AuthorityVisitor<'_, Self::Payload, StateError>,
    ) -> Result<(), StateError> {
        use crate::authority::CatalogPayload;
        if self.checkpoint.is_some() {
            self.visit_records(|record| {
                for object in &record.objects {
                    visitor(&CatalogPayload::Object(Box::new(object.into())))?;
                    for member in &object.files {
                        visitor(&CatalogPayload::Member(member.clone()))?;
                    }
                    visitor(&CatalogPayload::ObjectEnd)?;
                }
                Ok(())
            })?;
        }
        Ok(())
    }
}

fn completed_components(progress: TerminalFinalizationProgress) -> usize {
    match progress {
        TerminalFinalizationProgress::BeforeReplicaA => 0,
        TerminalFinalizationProgress::AfterReplicaA => 1,
        TerminalFinalizationProgress::AfterSeparationAb => 2,
        TerminalFinalizationProgress::AfterReplicaB => 3,
        TerminalFinalizationProgress::AfterSeparationBc => 4,
        TerminalFinalizationProgress::AfterReplicaC => 5,
    }
}

fn visit_terminal_rows(
    intent: &TerminalFinalizationIntent,
    count: usize,
    visitor: &mut dyn FnMut(&TapeFileEntry) -> Result<(), StateError>,
) -> Result<(), StateError> {
    if let Some(prefix) = &intent.terminal_prefix {
        for row in &prefix.committed_bundle.entries {
            visitor(row)?;
        }
    }
    let layout = intent.layout.try_to_parity_layout()?;
    for component in intent.layout.components.iter().take(count) {
        let (kind, hash) = match component.kind {
            TerminalFinalizationComponentKind::TapeIndexReplica => (
                remanence_parity::TapeFileKind::TapeIndexReplica,
                intent.edition_digest,
            ),
            TerminalFinalizationComponentKind::IndexSeparationExtent => {
                let plan = remanence_parity::plan_index_separation(
                    remanence_parity::IndexSeparationDescriptor {
                        tape_uuid: intent.tape_uuid,
                        edition_id: intent.edition_id,
                        gap_ordinal: component.ordinal,
                        block_size: intent.layout.block_size,
                        nominal_extent_bytes: remanence_parity::DEFAULT_INDEX_SEPARATION_BYTES,
                        total_records: component.record_count,
                        compression_enabled: false,
                        terminal_layout: layout,
                    },
                )
                .map_err(|error| StateError::JournalReplayFailed(error.to_string()))?;
                (
                    remanence_parity::TapeFileKind::IndexSeparationExtent,
                    plan.descriptor_digest,
                )
            }
        };
        visitor(&TapeFileEntry {
            tape_file_number: component.tape_file_number,
            kind,
            block_count: component.record_count,
            physical_start_hint: Some(component.start_lba),
            canonical_metadata_hash: Some(hash),
            object_id: None,
            first_parity_data_ordinal: None,
            epoch_id: None,
            protected_ordinal_start: None,
            protected_ordinal_end_exclusive: None,
            object_recovery_row: None,
        })?;
    }
    Ok(())
}

impl FileCheckpointJournalLease {
    pub(crate) fn freeze_with_structural(
        &self,
        retained: Option<&dyn remanence_parity::authority::StructuralAuthority>,
    ) -> Result<AuthoritySnapshot, StateError> {
        AuthoritySnapshot::capture(&self.path, self.tape_uuid, retained)
    }
}

impl TapeAuthorityView for FileCheckpointJournalLease {
    type TerminalPrefix = crate::authority::AuthorityPrefixPlan;
    type TerminalSource = CheckpointTerminalIndexRecordSource<'static>;
    fn terminal_source(
        &self,
        structural: Option<&dyn remanence_parity::authority::StructuralAuthority>,
        prefix: remanence_parity::authority::TerminalPrefixMode<&Self::TerminalPrefix>,
    ) -> Result<Self::TerminalSource, StateError> {
        use remanence_parity::authority::TerminalPrefixMode;
        match (structural, prefix) {
            (None, TerminalPrefixMode::Absent) => {
                CheckpointTerminalIndexRecordSource::new_replay_backed_no_parity(self)
            }
            (Some(rows), TerminalPrefixMode::Absent) => {
                CheckpointTerminalIndexRecordSource::new_replay_backed(self, rows)
            }
            (Some(rows), TerminalPrefixMode::Planned(plan)) => {
                CheckpointTerminalIndexRecordSource::new_replay_backed_with_planned_terminal_prefix(
                    self,
                    rows,
                    &plan.into(),
                )
            }
            (Some(rows), TerminalPrefixMode::Durable(plan)) => {
                CheckpointTerminalIndexRecordSource::new_replay_backed_after_terminal_prefix(
                    self,
                    rows,
                    &plan.into(),
                )
            }
            _ => Err(StateError::JournalReplayFailed(
                "terminal prefix requires structural authority".into(),
            )),
        }
    }

    type Error = StateError;
    type Commit = crate::authority::AuthorityCommit;
    type ReplayMetrics = CheckpointBoundedReplayMetrics;
    fn latest_commit(&self) -> Result<Option<Self::Commit>, StateError> {
        self.last_record_bounded()
            .map(|record| record.as_ref().map(Into::into))
    }
    fn visit_commits(
        &self,
        visitor: &mut remanence_parity::authority::AuthorityVisitor<'_, Self::Commit, StateError>,
    ) -> Result<Self::ReplayMetrics, StateError> {
        self.for_each_record_bounded(|record| visitor(&record.into()))
    }
    fn replay_commits(&mut self) -> Result<Vec<Self::Commit>, StateError> {
        self.replay()
            .map(|records| records.iter().map(Into::into).collect())
    }
    fn pending_finalization(&self) -> Result<Option<Self::Finalization>, StateError> {
        self.terminal_finalization_intent()
            .map(|intent| intent.as_ref().map(Into::into))
    }
    type Finalization = crate::authority::AuthorityFinalization;
    type Snapshot = AuthoritySnapshot;
    fn authority_summary(&self) -> Result<AuthoritySummary, StateError> {
        Ok(self.freeze()?.summary)
    }
    fn validate_append_rows(
        &self,
        rows: &remanence_parity::FrozenFileRows,
    ) -> Result<(), StateError> {
        let checkpoint = self.bounded_replay_snapshot()?;
        let empty = checkpoint.visit_records(|_| Ok(()))?.last.is_none();
        if empty {
            if rows.committed_entry_count() == 0 {
                return Ok(());
            }
            return Err(StateError::JournalReplayFailed(
                "parity journal has a committed prefix but checkpoint authority is empty".into(),
            ));
        }
        validate_replay_backed_parity_terminal_rows(
            &checkpoint,
            rows,
            ReplayTerminalPrefixMode::Absent,
        )
        .map(|_| ())
    }
    fn freeze(&self) -> Result<AuthoritySnapshot, StateError> {
        AuthoritySnapshot::capture(&self.path, self.tape_uuid, None)
    }
    fn finalization_status(&self) -> Result<Option<Self::Finalization>, StateError> {
        Ok(self.freeze()?.neutral_intent)
    }
}

impl TapeAuthorityView for FileCheckpointJournal {
    type TerminalPrefix = crate::authority::AuthorityPrefixPlan;
    type TerminalSource = CheckpointTerminalIndexRecordSource<'static>;
    fn terminal_source(
        &self,
        structural: Option<&dyn remanence_parity::authority::StructuralAuthority>,
        prefix: remanence_parity::authority::TerminalPrefixMode<&Self::TerminalPrefix>,
    ) -> Result<Self::TerminalSource, StateError> {
        self.acquire_exclusive_for_terminal_recovery()?
            .terminal_source(structural, prefix)
    }

    type Error = StateError;
    type Commit = crate::authority::AuthorityCommit;
    type ReplayMetrics = CheckpointBoundedReplayMetrics;
    fn latest_commit(&self) -> Result<Option<Self::Commit>, StateError> {
        self.last().map(|record| record.as_ref().map(Into::into))
    }
    fn visit_commits(
        &self,
        visitor: &mut remanence_parity::authority::AuthorityVisitor<'_, Self::Commit, StateError>,
    ) -> Result<Self::ReplayMetrics, StateError> {
        let _lock = acquire_checkpoint_lock(
            &self.path,
            FlockArg::LockSharedNonblock,
            "lock checkpoint journal for replay",
        )?;
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                enforce_terminal_intent_for_replay(&self.path, self.tape_uuid, &[], true)?;
                return Ok(CheckpointBoundedReplayMetrics::default());
            }
            Err(error) => {
                return Err(StateError::io_at(
                    "open checkpoint journal",
                    &self.path,
                    error,
                ))
            }
        };
        let mut tail = Vec::with_capacity(2);
        let replay =
            visit_checkpoint_records_bounded(&mut file, self.tape_uuid, &self.path, |record| {
                if tail.len() == 2 {
                    tail.remove(0);
                }
                tail.push(record.clone());
                visitor(&record.into())
            })?;
        enforce_terminal_intent_for_replay(&self.path, self.tape_uuid, &tail, true)?;
        Ok(replay.metrics)
    }
    fn replay_commits(&mut self) -> Result<Vec<Self::Commit>, StateError> {
        self.replay()
            .map(|records| records.iter().map(Into::into).collect())
    }
    fn pending_finalization(&self) -> Result<Option<Self::Finalization>, StateError> {
        self.terminal_finalization_intent()
            .map(|intent| intent.as_ref().map(Into::into))
    }
    type Finalization = crate::authority::AuthorityFinalization;
    type Snapshot = AuthoritySnapshot;
    fn authority_summary(&self) -> Result<AuthoritySummary, StateError> {
        Ok(self.freeze()?.summary)
    }
    fn validate_append_rows(
        &self,
        rows: &remanence_parity::FrozenFileRows,
    ) -> Result<(), StateError> {
        self.acquire_exclusive_for_terminal_recovery()?
            .validate_append_rows(rows)
    }
    fn freeze(&self) -> Result<AuthoritySnapshot, StateError> {
        let _lock = acquire_checkpoint_lock(
            &self.path,
            FlockArg::LockSharedNonblock,
            "freeze tape authority",
        )?;
        AuthoritySnapshot::capture(&self.path, self.tape_uuid, None)
    }
    fn finalization_status(&self) -> Result<Option<Self::Finalization>, StateError> {
        Ok(self.freeze()?.neutral_intent)
    }
}

// A shared factory handle can perform an owned replay without mutating the
// authority. Only the local reference is mutable to satisfy the cursor facet.
impl TapeAuthorityView for &FileCheckpointJournal {
    type TerminalPrefix = crate::authority::AuthorityPrefixPlan;
    type TerminalSource = CheckpointTerminalIndexRecordSource<'static>;
    fn terminal_source(
        &self,
        structural: Option<&dyn remanence_parity::authority::StructuralAuthority>,
        prefix: remanence_parity::authority::TerminalPrefixMode<&Self::TerminalPrefix>,
    ) -> Result<Self::TerminalSource, StateError> {
        (**self).terminal_source(structural, prefix)
    }

    type Error = StateError;
    type Commit = crate::authority::AuthorityCommit;
    type ReplayMetrics = CheckpointBoundedReplayMetrics;
    type Finalization = crate::authority::AuthorityFinalization;
    type Snapshot = AuthoritySnapshot;
    fn latest_commit(&self) -> Result<Option<Self::Commit>, StateError> {
        (**self).latest_commit()
    }
    fn visit_commits(
        &self,
        visitor: &mut remanence_parity::authority::AuthorityVisitor<'_, Self::Commit, StateError>,
    ) -> Result<Self::ReplayMetrics, StateError> {
        (**self).visit_commits(visitor)
    }
    fn replay_commits(&mut self) -> Result<Vec<Self::Commit>, StateError> {
        FileCheckpointJournal::replay(self).map(|records| records.iter().map(Into::into).collect())
    }
    fn pending_finalization(&self) -> Result<Option<Self::Finalization>, StateError> {
        (**self).pending_finalization()
    }
    fn authority_summary(&self) -> Result<AuthoritySummary, StateError> {
        (**self).authority_summary()
    }
    fn validate_append_rows(
        &self,
        rows: &remanence_parity::FrozenFileRows,
    ) -> Result<(), StateError> {
        (**self).validate_append_rows(rows)
    }
    fn freeze(&self) -> Result<AuthoritySnapshot, StateError> {
        (**self).freeze()
    }
    fn finalization_status(&self) -> Result<Option<Self::Finalization>, StateError> {
        (**self).finalization_status()
    }
}

impl remanence_parity::authority::TerminalAuthoritySource<crate::authority::AuthorityFinalization>
    for CheckpointTerminalIndexRecordSource<'static>
{
    type Error = StateError;
    type ReplayMetrics = TerminalIndexAuthorityReplayMetrics;
    fn summary(&self) -> TerminalIndexAuthoritySummary {
        self.summary()
    }
    fn replay_metrics(&self) -> Option<Self::ReplayMetrics> {
        self.replay_metrics()
    }
    fn reconstruct_final_plan(
        &mut self,
        plan: &crate::authority::AuthorityFinalization,
    ) -> Result<remanence_parity::TerminalTripleWritePlan, StateError> {
        self.reconstruct_final_plan(&plan.into())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{finalization_intent, record, second_record};
    use super::*;
    use crate::authority::TapeAuthorityWriter;
    use crate::authority::{legacy::TapeAuthorityLease, legacy_fixtures::AuthorityBuilder};

    use crate::authority::contract_tests::{finalization_contract, view_writer_contract};
    #[test]
    fn old_authority_seeded_structural_contract() {
        let dir = tempfile::tempdir().unwrap();
        let id = [0x39; 16];
        let (record, plan, rows) = crate::authority::contract_tests::structural_fixture(id);
        let prefix = crate::authority::AuthorityFinalization::from(&plan)
            .terminal_prefix
            .unwrap()
            .publication;
        let mut writer = AuthorityBuilder::new(dir.path().join("checkpoints"), id)
            .commit(record)
            .rows(rows)
            .plan(plan)
            .build_lease()
            .unwrap();
        crate::authority::contract_tests::structural_writer_contract(&mut writer, prefix);
    }
    #[test]
    fn old_authority_snapshot_agreement_contract() {
        let dir = tempfile::tempdir().unwrap();
        let id = [0x39; 16];
        let (record, plan, rows) = crate::authority::contract_tests::structural_fixture(id);
        let mut writer = AuthorityBuilder::new(dir.path().join("checkpoints"), id)
            .commit(record)
            .rows(rows)
            .build_lease()
            .unwrap();
        crate::authority::contract_tests::snapshot_agreement_contract(&mut writer, (&plan).into());
    }
    #[test]
    fn old_authority_accepts_terminal_first() {
        let directory = tempfile::tempdir().unwrap();
        let mut writer = AuthorityBuilder::new(directory.path().join("checkpoints"), [0x39; 16])
            .geometry(256 * 1024, remanence_parity::default_scheme())
            .build_lease()
            .unwrap();
        use remanence_parity::authority::{FilePublication, FilePurpose, PublicationDisposition};
        let prefix = FilePublication {
            purpose: FilePurpose::TerminalPrefix,
            files: Vec::new(),
            protected: 0,
            total: 0,
            cursor: None,
            check: None,
            seal_due: None,
        };
        let receipt = writer.publish_terminal_prefix(&prefix).unwrap();
        assert_eq!(receipt.items[0].publication, prefix);
        assert_eq!(
            receipt.items[0].disposition,
            PublicationDisposition::Appended
        );
        assert_eq!(
            writer.publish_terminal_prefix(&prefix).unwrap().items[0].disposition,
            PublicationDisposition::AlreadyPresent
        );
    }

    #[test]
    fn old_authority_view_writer_summary_cursors_and_errors_contract() {
        let dir = tempfile::tempdir().unwrap();
        let id = [0x31; 16];
        let authority = AuthorityBuilder::new(dir.path(), id).build().unwrap();
        let mut lease = authority.acquire_exclusive().unwrap();
        view_writer_contract(
            &mut lease,
            (&record(id)).into(),
            (&second_record(id)).into(),
            |ordinal| ordinal,
        );
    }

    #[test]
    fn old_authority_latest_commit_preserves_full_payload_and_physical_bundles() {
        let dir = tempfile::tempdir().unwrap();
        let id = [0x3a; 16];
        let authority = AuthorityBuilder::new(dir.path(), id).build().unwrap();
        let mut writer = authority.acquire_exclusive().unwrap();
        let mut report: crate::authority::AuthorityCommit =
            (&super::super::tests::partial_epoch_parity_record(id)).into();
        let object_id = report.payloads[0].projection.object.object_id.clone();
        report.payloads[0]
            .files
            .push(crate::NativeObjectFileProjectionInput {
                object_id,
                file_id: "member".into(),
                path: "payload.txt".into(),
                size_bytes: 1,
                file_sha256: vec![0x77; 32],
                first_chunk_lba: Some(0),
                chunk_count: 1,
                mtime: Some("1.25".into()),
                executable: Some(true),
            });
        assert!(!report.object_files.is_empty());
        assert!(report.barrier.is_some());
        writer.publish_report(&report).unwrap();
        assert_eq!(writer.latest_commit().unwrap(), Some(report.clone()));
        drop(writer);
        assert_eq!(authority.latest_commit().unwrap(), Some(report));
    }

    #[test]
    fn old_authority_finalization_plan_progress_and_errors_contract() {
        let dir = tempfile::tempdir().unwrap();
        let id = [0x32; 16];
        let authority = AuthorityBuilder::new(dir.path(), id)
            .commit(record(id))
            .build()
            .unwrap();
        let mut lease: TapeAuthorityLease = authority.acquire_exclusive().unwrap();
        finalization_contract(&mut lease, (&finalization_intent(id)).into());
    }
    #[test]
    fn old_authority_parity_agreement_and_frozen_rows_contract() {
        use remanence_parity::authority::ShadowPublication;
        use remanence_parity::{CommittedBundle, CommittedBundleKind, TapeFileMapEntry};
        for mismatch in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let id = [0x35; 16];
            let commit = super::super::tests::partial_epoch_parity_record(id);
            let mut reports = vec![CommittedBundle {
                kind: CommittedBundleKind::BotBootstrap,
                entries: vec![TapeFileEntry::from_map_entry(TapeFileMapEntry::bootstrap(
                    0, 1,
                ))],
                highest_protected_ordinal: 0,
                total_committed_ordinals: 0,
            }];
            reports.extend(commit.object_tape_file_bundles.clone());
            reports.push(commit.barrier_bundle.clone().unwrap());
            reports.push(CommittedBundle {
                kind: CommittedBundleKind::CheckpointedThrough,
                entries: Vec::new(),
                highest_protected_ordinal: 3,
                total_committed_ordinals: 3,
            });
            if mismatch {
                reports[1].entries[0].physical_start_hint = Some(999);
            }
            let authority = AuthorityBuilder::new(dir.path().join("checkpoints"), id)
                .commit(commit.clone())
                .rows(ShadowPublication { bundles: reports })
                .build()
                .unwrap();
            let snapshot = authority.freeze();
            if mismatch {
                assert!(
                    snapshot.is_err(),
                    "two-prefix disagreement must fail inside the adapter"
                );
            } else {
                let snapshot = snapshot.unwrap();
                assert_eq!(
                    (snapshot.summary().protected, snapshot.summary().total),
                    (3, 3)
                );
                let mut visited = Vec::new();
                snapshot
                    .visit_file_rows(&mut |row| {
                        visited.push(row.clone());
                        Ok(())
                    })
                    .unwrap();
                assert_eq!(visited.len(), 3);
                assert_eq!(visited[1], commit.object_tape_file_bundles[0].entries[0]);
                let mut plan = finalization_intent(id);
                let prefix = super::super::tests::terminal_prefix_plan_for_parity_record(&commit);
                plan.layout = TerminalFinalizationLayout::try_from(
                    remanence_parity::TerminalTailLayout::new(
                        0,
                        commit.block_size,
                        prefix.tail_start_tape_file_number,
                        prefix.tail_start_lba,
                        4,
                        4096,
                    )
                    .unwrap(),
                )
                .unwrap();
                plan.terminal_prefix = Some(prefix);
                let mut lease = authority.acquire_exclusive().unwrap();
                lease.begin_finalization(&(&plan).into()).unwrap();
                assert_eq!(
                    lease.freeze().unwrap().finalization(),
                    Some(&(&plan).into())
                );
                lease
                    .advance_finalization(
                        TerminalFinalizationProgress::BeforeReplicaA,
                        TerminalFinalizationProgress::AfterReplicaA,
                    )
                    .unwrap();
                assert!(
                    lease.freeze().is_err(),
                    "a plan cannot invent a durable component row"
                );
                drop(lease);
                let prefix = plan.terminal_prefix.as_ref().unwrap();
                let dir2 = tempfile::tempdir().unwrap();
                let mut prefix_reports = vec![CommittedBundle {
                    kind: CommittedBundleKind::BotBootstrap,
                    entries: vec![TapeFileEntry::from_map_entry(TapeFileMapEntry::bootstrap(
                        0, 1,
                    ))],
                    highest_protected_ordinal: 0,
                    total_committed_ordinals: 0,
                }];
                prefix_reports.extend(commit.object_tape_file_bundles.clone());
                prefix_reports.push(commit.barrier_bundle.clone().unwrap());
                let watermark = CommittedBundle {
                    kind: CommittedBundleKind::CheckpointedThrough,
                    entries: Vec::new(),
                    highest_protected_ordinal: 3,
                    total_committed_ordinals: 3,
                };
                prefix_reports.push(watermark.clone());
                prefix_reports.push(prefix.committed_bundle.clone());
                prefix_reports.push(watermark);
                let authority = AuthorityBuilder::new(dir2.path().join("checkpoints"), id)
                    .commit(commit)
                    .rows(ShadowPublication {
                        bundles: prefix_reports,
                    })
                    .plan(plan.clone())
                    .build()
                    .unwrap();
                let prefix_snapshot = authority.freeze().unwrap();
                assert_eq!(
                    prefix_snapshot.summary().next_file,
                    prefix.tail_start_tape_file_number
                );
                assert_eq!(
                    prefix_snapshot.summary().cursor.unwrap().lba,
                    prefix.tail_start_lba
                );
                let mut count = 0;
                prefix_snapshot
                    .visit_file_rows(&mut |_| {
                        count += 1;
                        Ok(())
                    })
                    .unwrap();
                assert_eq!(count, prefix.tail_start_tape_file_number);
            }
        }
    }
    #[test]
    fn old_authority_combined_writer_freezes_and_retains_loan_on_unwind() {
        use remanence_parity::{CommittedBundle, CommittedBundleKind};
        let dir = tempfile::tempdir().unwrap();
        let id = [0x3c; 16];
        let commit = super::super::tests::partial_epoch_parity_record(id);
        let mut bundles = vec![CommittedBundle {
            kind: CommittedBundleKind::BotBootstrap,
            entries: vec![TapeFileEntry::from_map_entry(TapeFileMapEntry::bootstrap(
                0, 1,
            ))],
            highest_protected_ordinal: 0,
            total_committed_ordinals: 0,
        }];
        bundles.extend(commit.object_tape_file_bundles.clone());
        bundles.push(commit.barrier_bundle.clone().unwrap());
        bundles.push(CommittedBundle {
            kind: CommittedBundleKind::CheckpointedThrough,
            entries: Vec::new(),
            highest_protected_ordinal: 3,
            total_committed_ordinals: 3,
        });
        let mut writer = AuthorityBuilder::new(dir.path().join("checkpoints"), id)
            .commit(commit)
            .rows(remanence_parity::authority::ShadowPublication { bundles })
            .build_lease()
            .unwrap();
        let frozen = writer
            .freeze()
            .expect("freeze must use the already-exclusive structural capability");
        assert_eq!(frozen.summary().next_file, 3);
        let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            writer
                .with_structural(|owner, rows| {
                    assert_eq!(rows.unwrap().tape_uuid(), owner.tape_uuid());
                    assert!(owner.structural().is_none());
                    assert!(owner
                        .freeze()
                        .unwrap_err()
                        .to_string()
                        .contains("during a legacy publication"));
                    panic!("interrupt the old publication loan");
                })
                .unwrap();
        }));
        assert!(interrupted.is_err());
        assert!(writer.structural().is_some());
        assert_eq!(writer.freeze().unwrap().summary(), frozen.summary());
    }

    #[test]
    fn old_authority_empty_checkpoint_refuses_nonempty_structural_authority() {
        use remanence_parity::{CommittedBundle, CommittedBundleKind};
        let dir = tempfile::tempdir().unwrap();
        let id = [0x3b; 16];
        let rows = remanence_parity::authority::ShadowPublication {
            bundles: vec![
                CommittedBundle {
                    kind: CommittedBundleKind::BotBootstrap,
                    entries: vec![TapeFileEntry::from_map_entry(TapeFileMapEntry::bootstrap(
                        0, 1,
                    ))],
                    highest_protected_ordinal: 0,
                    total_committed_ordinals: 0,
                },
                CommittedBundle {
                    kind: CommittedBundleKind::CheckpointedThrough,
                    entries: Vec::new(),
                    highest_protected_ordinal: 0,
                    total_committed_ordinals: 0,
                },
            ],
        };
        let authority = AuthorityBuilder::new(dir.path().join("checkpoints"), id)
            .geometry(256 * 1024, remanence_parity::default_scheme())
            .rows(rows)
            .build()
            .unwrap();
        assert!(authority
            .freeze()
            .unwrap_err()
            .to_string()
            .contains("checkpoint authority is empty"));
    }

    #[test]
    fn old_authority_sealed_snapshot_retains_plan_and_all_terminal_rows() {
        let dir = tempfile::tempdir().unwrap();
        let id = [0x39; 16];
        let base = record(id);
        let plan = finalization_intent(id);
        let authority = AuthorityBuilder::new(dir.path(), id)
            .commit(base.clone())
            .plan(plan.clone())
            .build()
            .unwrap();
        let mut lease = authority.acquire_exclusive_for_terminal_recovery().unwrap();
        let mut progress = TerminalFinalizationProgress::BeforeReplicaA;
        while let Some(next) = progress.successor() {
            lease.advance_finalization(progress, next).unwrap();
            progress = next;
        }
        let completed: TerminalFinalizationIntent =
            (&lease.finalization_status().unwrap().unwrap()).into();
        let sealed = super::super::tests::structured_terminal_record(&base, completed.clone());
        lease
            .publish_sealed_reports(&[(&sealed).into()], || Ok(()))
            .unwrap();
        let snapshot = lease.freeze().unwrap();
        assert!(snapshot.summary().sealed);
        assert_eq!(snapshot.finalization(), Some(&(&completed).into()));
        assert_eq!(
            snapshot.summary().cursor.unwrap().lba,
            completed.layout.expected_eod_lba
        );
        let mut rows = Vec::new();
        snapshot
            .visit_file_rows(&mut |row| {
                rows.push(row.clone());
                Ok(())
            })
            .unwrap();
        assert_eq!(rows.len(), 7);
        assert_eq!(
            rows.last().unwrap().tape_file_number + 1,
            snapshot.summary().next_file
        );
        for (row, component) in rows[2..].iter().zip(plan.layout.components) {
            assert_eq!(row.tape_file_number, component.tape_file_number);
            assert_eq!(row.block_count, component.record_count);
            assert_eq!(row.physical_start_hint, Some(component.start_lba));
        }
    }
}
