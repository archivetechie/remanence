//! Bounded terminal edition source. It scans physical rows directly and never
//! materializes commit or Object history. Only the immutable prefix plan is owned.
use super::{authority::invalid, snapshot::Snapshot};
use crate::{
    authority::{AuthorityFinalization, AuthorityPrefixPlan},
    StateError,
};
use remanence_parity::{
    authority::{
        FileRowCursor, TerminalAuthoritySource, TerminalPrefixMode, TerminalSourceSummary,
    },
    *,
};

/// Owned terminal source, independent of every authority lease.
#[derive(Debug)]
pub struct TerminalSource {
    raw: Snapshot,
    prefix: Option<AuthorityPrefixPlan>,
    summary: TerminalSourceSummary,
}
impl TerminalSource {
    pub(super) fn new(
        mut raw: Snapshot,
        mode: TerminalPrefixMode<&AuthorityPrefixPlan>,
    ) -> Result<Self, StateError> {
        let prefix = match mode {
            TerminalPrefixMode::Absent => {
                if let Some(state) = &raw.summary.finalization {
                    if raw.header.scheme.is_some() {
                        return Err(invalid("terminal plan requires explicit prefix mode"));
                    }
                    raw = raw.before(state.reference.first_sequence)?;
                }
                None
            }
            TerminalPrefixMode::Planned(p) | TerminalPrefixMode::Durable(p) => {
                p.validate()?;
                let durable = matches!(mode, TerminalPrefixMode::Durable(_));
                if let Some(state) = &raw.summary.finalization {
                    if durable && !state.prefix_written {
                        return Err(invalid("planned prefix is not durable"));
                    }
                    let plan = super::authority::AuthoritySnapshot::new(raw.clone())?;
                    use remanence_parity::authority::TapeAuthoritySnapshot;
                    if plan.finalization().and_then(|p| p.terminal_prefix.as_ref()) != Some(p) {
                        return Err(invalid("prefix differs from immutable plan"));
                    }
                    raw = raw.before(state.reference.first_sequence)?;
                } else if durable {
                    return Err(invalid("no durable prefix"));
                }
                if raw.summary.next_file != p.start_tape_file_number
                    || raw.summary.cursor.lba != p.start_lba
                {
                    return Err(invalid("prefix base differs"));
                }
                if p.publication.total != raw.summary.total
                    || p.publication.protected != raw.summary.total
                {
                    return Err(invalid("prefix coverage differs from committed data"));
                }
                let mut protected = raw.summary.protected;
                let mut epoch = raw.summary.next_epoch;
                for row in &p.publication.files {
                    if row.kind == TapeFileKind::ParitySidecar {
                        if row.epoch_id != Some(epoch)
                            || row.protected_ordinal_start != Some(protected)
                        {
                            return Err(invalid("prefix sidecar discontinuity"));
                        }
                        protected = row
                            .protected_ordinal_end_exclusive
                            .ok_or_else(|| invalid("prefix sidecar has no end"))?;
                        epoch = epoch
                            .checked_add(1)
                            .ok_or_else(|| invalid("prefix epoch overflow"))?;
                    }
                }
                if protected != p.publication.protected {
                    return Err(invalid("prefix rows do not prove coverage"));
                }
                let mut directory = p.sidecar_directory_entries.iter();
                let mut check_sidecar = |row: &TapeFileEntry| -> Result<(), StateError> {
                    if row.kind == TapeFileKind::ParitySidecar {
                        let entry = directory
                            .next()
                            .ok_or_else(|| invalid("prefix directory omits sidecar"))?;
                        if entry.tape_file_number != row.tape_file_number
                            || Some(entry.epoch_id) != row.epoch_id
                            || Some(entry.protected_ordinal_start) != row.protected_ordinal_start
                            || Some(entry.protected_ordinal_end_exclusive)
                                != row.protected_ordinal_end_exclusive
                            || entry.sidecar_total_block_count != row.block_count
                            || Some(entry.canonical_metadata_hash) != row.canonical_metadata_hash
                        {
                            return Err(invalid("prefix directory differs from durable sidecar"));
                        }
                    }
                    Ok(())
                };
                let mut cursor = raw.rows_cursor();
                while let Some(row) = cursor.next_entry().map_err(|e| invalid(e.to_string()))? {
                    check_sidecar(&row)?;
                }
                for row in &p.publication.files {
                    check_sidecar(row)?;
                }
                if directory.next().is_some() {
                    return Err(invalid("prefix directory adds unproved sidecar"));
                }
                Some(p.clone())
            }
        };
        let files = prefix
            .as_ref()
            .map_or(raw.summary.next_file, |p| p.tail_start_tape_file_number);
        let total = prefix
            .as_ref()
            .map_or(raw.summary.total, |p| p.publication.total);
        let protected = prefix
            .as_ref()
            .map_or(raw.summary.protected, |p| p.publication.protected);
        let summary = TerminalSourceSummary {
            scope: TapeIndexReplicaScope {
                covered_prefix_tape_file_count: files,
                total_data_ordinals: total,
                highest_protected_ordinal: protected,
            },
            counts: TapeIndexReplicaCounts {
                structural_entry_count: files,
                object_row_count: raw.summary.object_count,
            },
        };
        Ok(Self {
            raw,
            prefix,
            summary,
        })
    }
    fn visit_rows(
        &self,
        visitor: &mut dyn FnMut(&TapeFileEntry) -> Result<(), ParityError>,
    ) -> Result<(), ParityError> {
        let mut cursor = self.raw.rows_cursor();
        while let Some(row) = cursor
            .next_entry()
            .map_err(|e| ParityError::ImplementationLimit {
                context: "tape-log terminal source",
                detail: e.to_string(),
            })?
        {
            visitor(&row)?;
        }
        if let Some(prefix) = &self.prefix {
            for row in &prefix.publication.files {
                visitor(row)?;
            }
        }
        Ok(())
    }
}
impl TapeIndexReplicaRecordSource for TerminalSource {
    fn visit_structural_entries(
        &mut self,
        v: &mut dyn FnMut(&TapeIndexReplicaMapEntry) -> Result<(), ParityError>,
    ) -> Result<(), ParityError> {
        let mut ordinal = 0u64;
        self.visit_rows(&mut |row| {
            let mut entry = crate::checkpoint::terminal_map_entry_from_committed(row)?;
            if row.kind == TapeFileKind::Object && entry.first_parity_data_ordinal.is_none() {
                entry.first_parity_data_ordinal = Some(ordinal);
                ordinal = ordinal
                    .checked_add(row.block_count)
                    .ok_or(ParityError::Invariant("terminal data ordinal overflow"))?;
            }
            v(&entry)
        })
    }
    fn visit_object_rows(
        &mut self,
        v: &mut dyn FnMut(&TapeIndexReplicaObjectRow) -> Result<(), ParityError>,
    ) -> Result<(), ParityError> {
        self.visit_rows(&mut |row| {
            if let Some(object) = &row.object_recovery_row {
                v(&crate::checkpoint::terminal_object_row_from_recovery(
                    object,
                )?)?;
            }
            Ok(())
        })
    }
}
impl TerminalAuthoritySource<AuthorityFinalization> for TerminalSource {
    type Error = StateError;
    type ReplayMetrics = crate::TerminalIndexAuthorityReplayMetrics;
    fn summary(&self) -> TerminalSourceSummary {
        self.summary
    }
    fn replay_metrics(&self) -> Option<Self::ReplayMetrics> {
        Some(crate::TerminalIndexAuthorityReplayMetrics {
            checkpoint: crate::CheckpointBoundedReplayMetrics {
                replay_passes: self.raw.replay_metrics().validation_passes,
                frame_count: self.raw.replay_metrics().frames,
                peak_frame_payload_bytes: self.raw.replay_metrics().peak_frame_buffer as u64,
                peak_live_record_count: 1,
                peak_live_object_rows: 1,
            },
            parity: None,
        })
    }
    fn reconstruct_final_plan(
        &mut self,
        intent: &AuthorityFinalization,
    ) -> Result<TerminalTripleWritePlan, StateError> {
        intent.validate_for_tape(self.raw.header.tape_uuid)?;
        let plan = assemble_terminal_plan(
            intent.tape_uuid,
            intent.layout.block_size,
            false,
            intent.edition_sequence,
            self.summary.scope,
            self.summary.counts,
            self,
            intent.layout.try_to_parity_layout()?,
            ParityMapDiagnostics {
                writer_version: intent.writer_version.clone(),
                write_timestamp: intent.write_timestamp.clone(),
            },
            intent.edition_id,
            DEFAULT_INDEX_SEPARATION_BYTES,
        )
        .map_err(|e| invalid(e.to_string()))?;
        if plan.edition.edition_digest != intent.edition_digest
            || plan.edition.layout_digest != intent.layout.layout_digest
        {
            return Err(invalid("terminal edition digest mismatch"));
        }
        Ok(plan)
    }
}
