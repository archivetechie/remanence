//! Transactional, streaming projection of validated tape-log groups. The mark
//! and every catalog row advance in the same SQLite transaction. Frozen older
//! snapshots cannot undo a newer projection, and members never collect in RAM.
//!
//! A projection mark accelerates projection; it does not prove the rows below
//! it. Incremental projection validates the named closing frame and reads only
//! later groups, so a group whose rows are missing or wrong below the mark is
//! not re-examined here. Only a full projection rebuild compares every group
//! with SQLite and surfaces such a gap.
//!
//! Projection marks are derived cache positions, not fence authority. A mark
//! beyond the validated source refuses incremental projection; an explicit
//! source rebuild replaces the cache. Native ambiguity and received library
//! dependency floors remain independent admission and repair constraints.
use super::*;
use crate::authority::{CatalogPayload, TapeAuthoritySnapshot};
use crate::tape_log::store::StoreError;
use crate::tape_log::{authority::AuthoritySnapshot, Record};

/// Completion proof for one owned final commit. Pinning the file prevents an
/// inode from being reused while this in-memory marker survives.
#[derive(Debug)]
pub(super) struct ProjectedTape {
    _file: std::sync::Arc<std::fs::File>,
    identity: (u64, u64),
    end: u64,
    digest: [u8; 32],
}

impl ProjectedTape {
    /// Source bytes and SQL must still name the exact completed projection.
    fn matches(
        &self,
        snapshot: &crate::tape_log::snapshot::Snapshot,
        source: Option<&(SqliteU64, Vec<u8>)>,
    ) -> bool {
        matches!(snapshot.file_identity(), Ok(Some(identity)) if identity == self.identity)
            && self.end == snapshot.summary().closed_end
            && source.is_some_and(|(end, digest)| {
                end.get() == self.end && digest.as_slice() == self.digest.as_slice()
            })
    }
}

pub(super) const SCHEMA: &str = "create table if not exists tape_log_projection (
 tape_uuid blob primary key check(length(tape_uuid) = 16),
 incarnation integer not null check(incarnation = 1),
 projected_through_sequence blob not null check(length(projected_through_sequence) = 8),
 projected_through_offset blob not null check(length(projected_through_offset) = 8)
);
create table if not exists tape_log_object_commits (
 tape_uuid blob not null, tape_file_number blob not null,
 tape_log_sequence blob not null, tape_incarnation integer not null check(tape_incarnation = 1),
 primary key(tape_uuid, tape_file_number)
);";

impl CatalogCore {
    /// A mark survives when either its row or its source binding exists.
    pub(crate) fn tape_projection_marks(
        &self,
    ) -> Result<
        std::collections::BTreeMap<[u8; 16], crate::library_runtime::native_floor::MarkEvidence>,
        StateError,
    > {
        use crate::library_runtime::native_floor::MarkEvidence;
        let mut query = self.conn.prepare(
            "select ids.tape_uuid, m.projected_through_sequence, m.projected_through_offset, s.byte_end, s.source_digest
             from (select tape_uuid from tape_log_projection where incarnation=1
                   union select tape_uuid from tape_projection_sources) ids
             left join tape_log_projection m on m.tape_uuid=ids.tape_uuid and m.incarnation=1
             left join tape_projection_sources s on s.tape_uuid=ids.tape_uuid order by ids.tape_uuid"
        ).map_err(|e| sqlite_error("read tape projection evidence", e))?;
        let rows = query
            .query_map([], |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, Option<SqliteU64>>(1)?,
                    r.get::<_, Option<SqliteU64>>(2)?,
                    r.get::<_, Option<SqliteU64>>(3)?,
                    r.get::<_, Option<Vec<u8>>>(4)?,
                ))
            })
            .map_err(|e| sqlite_error("query tape projection evidence", e))?;
        rows.map(|row| {
            let (id, sequence, offset, end, digest) =
                row.map_err(|e| sqlite_error("decode tape projection evidence", e))?;
            let id = id
                .try_into()
                .map_err(|_| StateError::IndexCorrupt("invalid projection UUID".into()))?;
            let source = match (end, digest) {
                (None, None) => None,
                (Some(end), Some(digest)) => Some((
                    end.get(),
                    digest.try_into().map_err(|_| {
                        StateError::IndexCorrupt("invalid projection source digest".into())
                    })?,
                )),
                _ => {
                    return Err(StateError::IndexCorrupt(
                        "incomplete projection source evidence".into(),
                    ))
                }
            };
            Ok((
                id,
                MarkEvidence {
                    sequence: sequence.map_or(0, SqliteU64::get),
                    offset: offset.map_or(0, SqliteU64::get),
                    source,
                },
            ))
        })
        .collect()
    }
    /// One writer lease and outer transaction cover full replacement. Nested
    /// library/native apply uses savepoints, so any failed chunk retains the
    /// prior complete cache while the runtime keeps catch-up admission dirty.
    pub(crate) fn rebuild_typed_sources(
        &mut self,
        replay: impl FnOnce(
            &mut dyn FnMut(
                crate::library_log::AppliedLibraryRecord,
            ) -> Result<(), crate::library_log::store::StoreError>,
        ) -> Result<
            crate::library_log::store::Replay,
            crate::library_log::store::StoreError,
        >,
        snapshots: &[AuthoritySnapshot],
    ) -> Result<RebuildReport, apply::ApplyError> {
        self.conn
            .execute_batch("begin immediate")
            .map_err(|e| sqlite_error("begin atomic typed rebuild", e))?;
        let result = (|| {
            self.clear_typed_projection()?;
            let mut projection_error = None;
            let folded = replay(&mut |record| match self.apply_authority(
                &apply::AuthorityRecord::Library(Box::new(record)),
                apply::ApplyPath::Rebuild,
            ) {
                Ok(_) | Err(apply::ApplyError::HealthConflict { .. }) => Ok(()),
                Err(error) => {
                    projection_error = Some(error);
                    Err(std::io::Error::other("typed rebuild projection failed").into())
                }
            })
            .map_err(|error| {
                projection_error.take().unwrap_or_else(|| {
                    apply::ApplyError::Catalog(StateError::JournalReplayFailed(error.to_string()))
                })
            })?;
            if let Some(refusal) = folded.refusal {
                return Err(apply::ApplyError::Contradiction(format!(
                    "typed rebuild source refused: {refusal:?}"
                )));
            }
            self.finish_library_replay(&folded.fold)?;
            for snapshot in snapshots {
                self.apply_authority(
                    &apply::AuthorityRecord::Tape(Box::new(snapshot.clone())),
                    apply::ApplyPath::Rebuild,
                )?;
            }
            self.typed_rebuild_report(snapshots.iter().map(|s| s.log_reference().0).sum())
                .map_err(Into::into)
        })();
        match result {
            Ok(report) => {
                if let Err(error) = self.conn.execute_batch("commit") {
                    let _ = self.conn.execute_batch("rollback");
                    return Err(sqlite_error("commit atomic typed rebuild", error).into());
                }
                Ok(report)
            }
            Err(error) => {
                let _ = self.conn.execute_batch("rollback");
                Err(error)
            }
        }
    }
    /// Current cache counts after typed replay; no audit records were consumed.
    pub(crate) fn typed_replay_report(&self) -> Result<AuditReplayReport, StateError> {
        Ok(AuditReplayReport {
            audit_records_replayed: 0,
            operations_rebuilt: table_count(&self.conn, "operations")?,
            sessions_rebuilt: table_count(&self.conn, "sessions")?,
            idempotency_keys_rebuilt: table_count(&self.conn, "idempotency_keys")?,
        })
    }
    /// Drop every derived row before replaying the complete typed source set.
    /// Schema metadata is infrastructure; no catalog identity is preserved as
    /// fallback authority. Callers validate all source prefixes before this.
    pub(crate) fn clear_typed_projection(&mut self) -> Result<(), StateError> {
        self.forget_projection_markers();
        let tx = projection_transaction(&mut self.conn)
            .map_err(|e| sqlite_error("begin typed projection rebuild", e))?;
        tx.execute_batch("pragma defer_foreign_keys=on")
            .map_err(|e| sqlite_error("defer projection foreign keys", e))?;
        let names = {
            let mut query = tx.prepare("select name from sqlite_master where type='table' and name not like 'sqlite_%' and name <> 'schema_meta'")
                .map_err(|e| sqlite_error("list derived tables", e))?;
            let result = query
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|e| sqlite_error("query derived tables", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| sqlite_error("read derived table", e))?;
            result
        };
        for name in names {
            tx.execute(
                &format!("delete from \"{}\"", name.replace('"', "\"\"")),
                [],
            )
            .map_err(|e| sqlite_error("clear typed projection", e))?;
        }
        tx.commit()
            .map_err(|e| sqlite_error("commit empty typed projection", e))
    }

    /// Counts of the rebuilt derived rows, without fabricating audit replay.
    pub(crate) fn typed_rebuild_report(
        &self,
        native_records: u64,
    ) -> Result<RebuildReport, StateError> {
        Ok(RebuildReport {
            standing_note: None,
            tapes_rebuilt: table_count(&self.conn, "tapes")?,
            tape_files_rebuilt: table_count(&self.conn, "tape_files")?,
            object_instances_rebuilt: table_count(&self.conn, "object_instances")?,
            audit_records_replayed: 0,
            journal_records_replayed: native_records,
            preserved_fallbacks: Vec::new(),
            replay_divergences: Vec::new(),
        })
    }
    /// Catalog accelerator only; the log remains authority and must be validated.
    pub fn projected_through_sequence(&self, uuid: [u8; 16]) -> Result<u64, StateError> {
        Ok(projection_mark(&self.conn, uuid)?.map_or(0, |(sequence, _)| sequence))
    }
    /// Tape identities with acknowledged projection marks, including missing logs.
    pub fn projected_tape_uuids(&self) -> Result<Vec<[u8; 16]>, StateError> {
        let mut statement = self
            .conn
            .prepare("select tape_uuid from tape_log_projection order by tape_uuid")
            .map_err(|e| sqlite_error("list tape projection marks", e))?;
        let rows = statement
            .query_map([], |r| r.get::<_, Vec<u8>>(0))
            .map_err(|e| sqlite_error("read tape projection marks", e))?;
        rows.map(|row| {
            row.map_err(|e| sqlite_error("read tape projection UUID", e))
                .and_then(|v| {
                    v.try_into()
                        .map_err(|_| StateError::IndexCorrupt("invalid projection UUID".into()))
                })
        })
        .collect()
    }
    pub(super) fn join_object_log_reference(
        &self,
        instance: &mut NativeObjectInstanceRecord,
    ) -> Result<(), StateError> {
        instance.tape_log_sequence = self.conn.query_row("select tape_log_sequence from tape_log_object_commits where tape_uuid = ?1 and tape_file_number = ?2", params![&instance.tape_uuid, SqliteU64::new(instance.tape_file_number)], |row| row.get::<_, SqliteU64>(0)).optional().map_err(|e| sqlite_error("join object log reference", e))?.map(SqliteU64::get);
        instance.tape_incarnation = instance.tape_log_sequence.map(|_| 1);
        Ok(())
    }
    /// Project all new closed groups from one immutable, validated snapshot.
    pub fn project_tape_log(&mut self, snapshot: &AuthoritySnapshot) -> Result<(), StateError> {
        let tx = projection_transaction(&mut self.conn)
            .map_err(|e| sqlite_error("begin tape-log projection", e))?;
        project_snapshot(&tx, snapshot)?;
        tx.commit()
            .map_err(|e| sqlite_error("commit tape-log projection", e))
    }
}
impl CatalogCore {
    /// Projection-only rebuild: this entry point has no filesystem reset access.
    pub fn rebuild_tape_log_projection(
        &mut self,
        audit: &[AuditRecord],
        snapshots: &[AuthoritySnapshot],
    ) -> Result<RebuildReport, StateError> {
        let tx = projection_transaction(&mut self.conn)
            .map_err(|e| sqlite_error("begin projection-only rebuild", e))?;
        {
            let mut marks = tx
                .prepare("select tape_uuid, projected_through_sequence from tape_log_projection")
                .map_err(|e| sqlite_error("check rebuild projection floors", e))?;
            let rows = marks
                .query_map([], |r| {
                    Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, SqliteU64>(1)?.get()))
                })
                .map_err(|e| sqlite_error("read rebuild projection floors", e))?;
            for row in rows {
                let (uuid, projected) =
                    row.map_err(|e| sqlite_error("read rebuild projection floor", e))?;
                let available = snapshots
                    .iter()
                    .find(|s| s.raw.header().tape_uuid.as_slice() == uuid)
                    .map_or(0, |s| s.log_reference().0);
                if available < projected {
                    return Err(StateError::JournalReplayFailed(format!("projection rebuild refused: tape {} has sequence {available} below projected-through sequence {projected}; recovery required", hex_uuid(uuid.try_into().map_err(|_| StateError::IndexCorrupt("invalid projection UUID".into()))?))));
                }
            }
        }
        for snapshot in snapshots {
            validate_projected_history(&tx, snapshot)?;
        }
        let preserved = query_preserved_tape_rows_tx(&tx)?;
        clear_rebuildable_tables(&tx)?;
        restore_preserved_tape_rows_tx(&tx, &preserved)?;
        for record in audit {
            project_session_record(&tx, record)?;
            project_operation_record(&tx, record)?;
            project_idempotency_record(&tx, record, IdempotencyProjectionMode::Replay)?;
        }
        for snapshot in snapshots {
            project_snapshot(&tx, snapshot)?;
        }
        merge_preserved_tape_operator_columns_tx(&tx, &preserved)?;
        let mut warnings = Vec::new();
        for record in audit {
            project_catalog_evidence_record(&tx, record, &mut warnings)?;
        }
        tx.execute("update object_instances set status = 'missing' where tape_uuid in (select tape_uuid from tapes where state = 'retired')", []).map_err(|e| sqlite_error("rebuild retired copies", e))?;
        let report = RebuildReport {
            standing_note: None,
            tapes_rebuilt: snapshots.len() as u64,
            tape_files_rebuilt: table_count(&tx, "tape_files")?,
            object_instances_rebuilt: table_count(&tx, "object_instances")?,
            audit_records_replayed: audit.len() as u64,
            journal_records_replayed: snapshots
                .iter()
                .map(|s| s.raw.summary().last_closed_sequence)
                .sum(),
            preserved_fallbacks: Vec::new(),
            replay_divergences: warnings,
        };
        tx.commit()
            .map_err(|e| sqlite_error("commit projection-only rebuild", e))?;
        Ok(report)
    }
    /// A repair cannot erase accepted terminal obligations recorded in SQLite,
    /// including surviving idempotency/operation rows. Forensic recovery is a
    /// separate operator decision outside the step-2 repair/supersede paths.
    pub(crate) fn validate_repair_prefix(
        &self,
        snapshot: &AuthoritySnapshot,
    ) -> Result<(), StateError> {
        const REFUSAL: &str = "repair would discard a seal or accepted terminal finalization; a separate explicit forensic recovery path is required and is unavailable through repair-tail";
        let uuid = snapshot.raw.header().tape_uuid;
        let plan = snapshot.finalization().filter(|_| {
            snapshot
                .raw
                .summary()
                .finalization
                .as_ref()
                .is_some_and(|p| !p.prepared)
        });
        let manual = plan.and_then(|p| p.manual.as_ref());
        let operation = manual.map(|m| Uuid::from_bytes(m.operation_id).to_string());
        if let Some(tape) = self.get_tape(&uuid)? {
            if tape.state == "sealed" && !snapshot.summary().sealed {
                return Err(StoreError::Refused(REFUSAL).into());
            }
            if let Some(terminal) = tape.terminal_finalization {
                if !plan.is_some_and(|p| {
                    p.trigger == terminal.trigger
                        && p.edition_digest == terminal.edition_digest
                        && p.layout.layout_digest == terminal.layout_digest
                        && operation == terminal.operation_id.map(|id| id.to_string())
                        && terminal_progress_reaches(terminal.progress, p.progress)
                        && (!matches!(
                            terminal.outcome,
                            TerminalFinalizationOutcome::Finalized
                                | TerminalFinalizationOutcome::FinalizedDegraded
                        ) || snapshot.summary().sealed)
                }) {
                    return Err(StoreError::Refused(REFUSAL).into());
                }
            }
        }
        // Only this tape's finalize requests count: a request bound to another
        // tape, or whose operation names another subject, is not this tape's
        // obligation. A request of unknown owner is kept, conservatively. Only a
        // request cancelled before dispatch accepted nothing: a failed attempt
        // can sit on a plan already durable in the log and resumes under its key.
        let subjects = [
            format!("tape:{}", Uuid::from_bytes(uuid)),
            Uuid::from_bytes(uuid).to_string(),
            hex_uuid(uuid),
        ];
        let mut requests = self
            .conn
            .prepare(
                "select k.operation_id, k.actor_fingerprint, k.idempotency_key, k.request_fingerprint
                 from idempotency_keys k left join operations o on o.operation_id = k.operation_id
                 where k.operation_kind = 'finalize_tape'
                   and coalesce(k.terminal_state, '') != 'cancelled_before_dispatch'
                   and coalesce(o.state, '') != 'cancelled_before_dispatch'
                   and not exists (select 1 from tapes t where t.finalization_operation_id = k.operation_id and t.tape_uuid != ?1)
                   and (o.subject is null or o.subject in (?2, ?3, ?4))",
            )
            .map_err(|e| sqlite_error("inspect repair terminal requests", e))?;
        let rows = requests
            .query_map(
                params![uuid.as_slice(), subjects[0], subjects[1], subjects[2]],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Vec<u8>>(3)?,
                    ))
                },
            )
            .map_err(|e| sqlite_error("read repair terminal requests", e))?;
        for row in rows {
            let (op, actor, key, fingerprint) =
                row.map_err(|e| sqlite_error("read repair terminal request", e))?;
            if !manual.is_some_and(|m| {
                Some(&op) == operation.as_ref()
                    && actor == m.actor_fingerprint
                    && key == Uuid::from_bytes(m.idempotency_key).to_string()
                    && fingerprint == m.request_fingerprint
            }) {
                return Err(StoreError::Refused(REFUSAL).into());
            }
        }
        // Any accepted or still non-terminal finalize operation for this tape,
        // other than the retained plan's own, would be discarded by the repair.
        let cutoff: bool = self
            .conn
            .query_row(
                "select exists(select 1 from operations
                 where operation_kind = 'finalize_tape' and subject in (?1, ?2, ?3)
                   and operation_id is not ?4
                   and state != 'cancelled_before_dispatch')",
                params![subjects[0], subjects[1], subjects[2], operation],
                |r| r.get(0),
            )
            .map_err(|e| sqlite_error("inspect repair terminal operations", e))?;
        if cutoff {
            return Err(StoreError::Refused(REFUSAL).into());
        }
        Ok(())
    }
    /// Replace only a repaired tape's derived rows; authority was archived and
    /// remains physically fenced until the operator proves its append tail.
    ///
    /// This explicit rebuild is the one projection that may lower the written
    /// extent: it re-derives every row of the tape from the retained prefix
    /// alone. It does not change the tape's state. Ruling 32-L1 refuses tail
    /// repair before any effect, so no production path calls it until the library
    /// log records repairs. A full catalog rebuild likewise has no prior extent
    /// and derives it from each log.
    pub fn rebuild_repaired_tape_projection(
        &mut self,
        snapshot: &AuthoritySnapshot,
    ) -> Result<(), StateError> {
        self.validate_repair_prefix(snapshot)?;
        let uuid = snapshot.raw.header().tape_uuid;
        let tx = projection_transaction(&mut self.conn)
            .map_err(|e| sqlite_error("begin repaired tape projection", e))?;
        tx.execute_batch(
            "create temp table repaired_objects as select object_id from object_instances where 0",
        )
        .map_err(|e| sqlite_error("prepare repair object scope", e))?;
        tx.execute(
            "insert into repaired_objects select object_id from object_instances where tape_uuid = ?1",
            params![uuid.as_slice()],
        )
        .map_err(|e| sqlite_error("retain repair object scope", e))?;
        for table in [
            "catalog_units",
            "object_instances",
            "tape_files",
            "tape_log_projection",
            "tape_log_object_commits",
            "tape_projection_sources",
            "tape_projection_chunks",
        ] {
            tx.execute(
                &format!("delete from {table} where tape_uuid = ?1"),
                params![uuid.as_slice()],
            )
            .map_err(|e| sqlite_error("clear repaired tape projection", e))?;
        }
        tx.execute("update tapes set highest_protected_ordinal = ?2, total_committed_ordinals = ?2, last_committed_tape_file = null, written_extent_lba = null, finalization_progress = null, finalization_trigger = null, finalization_operation_id = null, finalization_edition_digest = null, finalization_layout_digest = null, finalization_outcome = null, completed_replicas = null, seal_due = null where tape_uuid = ?1", params![uuid.as_slice(), SqliteU64::new(0)]).map_err(|e| sqlite_error("reset repaired tape counters", e))?;
        project_snapshot(&tx, snapshot)?;
        tx.execute("delete from object_files where object_id in (select object_id from repaired_objects) and object_id not in (select object_id from object_instances)", []).map_err(|e| sqlite_error("remove repaired orphan members", e))?;
        tx.execute("delete from objects where object_id in (select object_id from repaired_objects) and object_id not in (select object_id from object_instances)", []).map_err(|e| sqlite_error("remove repaired orphan objects", e))?;
        tx.execute_batch("drop table repaired_objects")
            .map_err(|e| sqlite_error("clear repair object scope", e))?;
        tx.commit()
            .map_err(|e| sqlite_error("commit repaired tape projection", e))
    }
}

fn projection_mark(conn: &Connection, uuid: [u8; 16]) -> Result<Option<(u64, u64)>, StateError> {
    conn.query_row("select projected_through_sequence, projected_through_offset from tape_log_projection where tape_uuid = ?1 and incarnation = 1", params![uuid.as_slice()], |row| {
        Ok((row.get::<_, SqliteU64>(0)?.get(), row.get::<_, SqliteU64>(1)?.get()))
    }).optional().map_err(|e| sqlite_error("read tape-log projection mark", e))
}
fn project_snapshot(tx: &Connection, snapshot: &AuthoritySnapshot) -> Result<(), StateError> {
    project_snapshot_rows(tx, snapshot, false, &mut |_, _, _| Ok(false))
}
/// The callback commits a completed bounded row prefix, and can skip a durable
/// prefix when restarting a partially projected native group.
fn project_snapshot_rows(
    tx: &Connection,
    snapshot: &AuthoritySnapshot,
    historical: bool,
    before_row: &mut dyn FnMut(&Connection, u64, u64) -> Result<bool, StateError>,
) -> Result<(), StateError> {
    let raw = &snapshot.raw;
    let uuid = raw.header().tape_uuid;
    // A retired data prefix remains evidence for library replay, but Register
    // gives its barcode UUID a new inventory-only cleaning entity. That old
    // native prefix must not restore data geometry or terminal state.
    let inventory_only: bool = tx
        .query_row(
            "select exists(select 1 from library_cleaning_homes where tape_uuid=?1)",
            [uuid.as_slice()],
            |r| r.get(0),
        )
        .map_err(|e| sqlite_error("check registered cleaning projection", e))?;
    if inventory_only {
        return Ok(());
    }
    let mark = projection_mark(tx, uuid)?;
    let (through, offset) = mark.unwrap_or((0, 0));
    if through > raw.summary().last_closed_sequence {
        return Err(StoreError::Refused(crate::tape_log::store::BELOW_PROJECTION_MARK).into());
    }
    let Some(mut decoder) = raw.projection_decoder(through, offset)? else {
        return Ok(());
    };
    let updated_at = now_utc()?;
    let mut start = None;
    while let Some(frame) = decoder.next_row_frame().map_err(StoreError::from)? {
        let (sequence, begin) = *start.get_or_insert((frame.sequence, frame.offset));
        if matches!(frame.record, Record::PayloadContinuation(_)) {
            continue;
        }
        let mut row_number = 0;
        let group = raw.group_snapshot(sequence, begin, frame.end);
        // Write-session authority exists only in the native tape source. Apply
        // it on live catch-up as well as rebuild; the ending preserves the
        // open's drive fields through project_session_core's coalescing upsert.
        // Bay coordinates stay in the native record: leaving both SQL bay
        // columns NULL prevents the legacy trigger from deriving an endpoint.
        // Native recovery remains tape-owned.
        let session = match &frame.record {
            Record::SessionOpened(v) => Some((
                v.session_id,
                "open",
                v.opened_at_utc.as_str(),
                Some(&v.drive),
            )),
            Record::SessionEnded(v) => {
                Some((v.session_id, "closed", v.ended_at_utc.as_str(), None))
            }
            _ => None,
        };
        if let Some((session, state, timestamp, drive)) = session {
            project_session_core(
                tx,
                SessionProjection {
                    session_id: Uuid::from_bytes(session),
                    kind: "write",
                    tape_uuid: Some(uuid.to_vec()),
                    library_serial: None,
                    drive_bay: None,
                    drive_uuid: drive.and_then(|v| v.drive_uuid).map(|v| v.to_vec()),
                    drive_serial: drive.map(|v| v.serial.clone()),
                    state,
                    timestamp,
                    endpoint: None,
                },
            )?;
        }
        if let Record::Initialized(init) = &frame.record {
            insert_tape_file(tx, uuid, &init.bot)?;
            project_tape_written_extent_tx(tx, uuid, 2)?;
            tx.execute(
                "update tapes set last_committed_tape_file = ?2 where tape_uuid = ?1",
                params![uuid.as_slice(), SqliteU64::new(0)],
            )
            .map_err(|e| sqlite_error("project bootstrap boundary", e))?;
        }
        if let Record::ClosedOut(close) = &frame.record {
            let mut failure = None;
            group
                .visit_file_rows(|row| {
                    row_number += 1;
                    match before_row(tx, frame.sequence, row_number) {
                        Ok(true) => return Ok(()),
                        Ok(false) => (),
                        Err(error) => {
                            failure = Some(error);
                            return Err(StoreError::Refused("chunk checkpoint failed"));
                        }
                    }
                    insert_tape_file(tx, uuid, row).map_err(|e| {
                        failure = Some(e);
                        StoreError::Refused("closeout row projection failed")
                    })
                })
                .map_err(|e| failure.take().unwrap_or_else(|| e.into()))?;
            tx.execute("update tapes set highest_protected_ordinal = ?2, last_committed_tape_file = ?3 where tape_uuid = ?1",
                params![uuid.as_slice(), SqliteU64::new(close.to_t), SqliteU64::new(close.next_file - 1)])
                .map_err(|e| sqlite_error("project closeout range", e))?;
            project_tape_written_extent_tx(tx, uuid, close.cursor_after.lba)?;
        }

        if let Record::Committed(commit)
        | Record::TerminalPrefix { commit, .. }
        | Record::ComponentWritten { commit, .. } = &frame.record
        {
            let input = TapeJournalIndexInput {
                tape_uuid: uuid,
                block_size: raw.header().block_size,
                scheme: raw.header().scheme.clone(),
                journal_offset_bytes: frame.end,
            };
            let bundle = FilePublication {
                purpose: FilePurpose::Object,
                files: Vec::new(),
                protected: commit.protected,
                total: commit.total,
                cursor: None,
                check: None,
                seal_due: None,
            };
            project_committed_tape_file_bundle_tx(tx, &input, &bundle, &updated_at)?;
            let mut failure = None;
            group
                .visit_file_rows(|row| {
                    row_number += 1;
                    match before_row(tx, frame.sequence, row_number) {
                        Ok(true) => return Ok(()),
                        Ok(false) => (),
                        Err(error) => {
                            failure = Some(error);
                            return Err(StoreError::Refused("chunk checkpoint failed"));
                        }
                    }
                    insert_tape_file(tx, uuid, row).map_err(|error| {
                        failure = Some(error);
                        StoreError::Refused("tape-file projection failed")
                    })
                })
                .map_err(|e| failure.take().unwrap_or_else(|| e.into()))?;
            group
                .visit_catalog(|event| {
                    row_number+=1;
                    match before_row(tx,frame.sequence,row_number) {
                        Ok(true)=>return Ok(()), Ok(false)=>(), Err(error)=> { failure=Some(error); return Err(StoreError::Refused("chunk checkpoint failed")); }
                    }
                    let result = match event {
                        CatalogPayload::Object(p) => {
                            validate_object_metadata(
                                tx,
                                &p.object,
                                uuid,
                                frame.sequence,
                                commit.batch_id,
                            )
                            .map_err(|e| {
                                failure = Some(e);
                                StoreError::Refused("object metadata conflict")
                            })?;
                            tx.execute(
                                "insert into tape_log_object_commits values (?1, ?2, ?3, 1) on conflict(tape_uuid, tape_file_number) do nothing",
                                params![
                                    uuid.as_slice(),
                                    SqliteU64::new(p.instance.tape_file_number),
                                    SqliteU64::new(frame.sequence)
                                ],
                            )
                            .map_err(|e| {
                                failure = Some(sqlite_error("project object log reference", e));
                                StoreError::Refused("object log reference failed")
                            })?;
                            let same: bool = tx.query_row(
                                "select tape_log_sequence = ?3 and tape_incarnation = 1 from tape_log_object_commits where tape_uuid = ?1 and tape_file_number = ?2",
                                params![uuid.as_slice(), SqliteU64::new(p.instance.tape_file_number), SqliteU64::new(frame.sequence)], |r| r.get(0)
                            ).map_err(|e| { failure = Some(sqlite_error("compare object log reference", e)); StoreError::Refused("object log reference failed") })?;
                            if !same { return Err(StoreError::Refused("object log reference conflicts with existing projection")); }
                            upsert_native_object_projection_tx(
                                tx,
                                &p.object,
                                Some(&[]),
                                std::slice::from_ref(&p.instance),
                                p.object.created_at_utc.as_deref().unwrap_or(&updated_at),
                            )
                        }
                        CatalogPayload::Member(member) => {
                            insert_native_object_file_projection_tx(tx, member)
                        }
                        CatalogPayload::ObjectEnd => Ok(()),
                    };
                    result.map_err(|error| {
                        failure = Some(error);
                        StoreError::Refused("catalog payload projection failed")
                    })
                })
                .map_err(|e| failure.take().unwrap_or_else(|| e.into()))?;
            tx.execute(
                "update tapes set last_committed_tape_file = ?2 where tape_uuid = ?1",
                params![
                    uuid.as_slice(),
                    commit.next_file.checked_sub(1).map(SqliteU64::new)
                ],
            )
            .map_err(|e| sqlite_error("project tape-log file boundary", e))?;
            project_tape_written_extent_tx(tx, uuid, commit.cursor.lba)?;
            project_checkpoint_tape_seal_tx(tx, uuid, false, commit.seal_due, &updated_at)?;
        }
        row_number += 1;
        before_row(tx, frame.sequence, row_number)?;
        tx.execute("insert into tape_log_projection values (?1, 1, ?2, ?3) on conflict(tape_uuid) do update set projected_through_sequence = excluded.projected_through_sequence, projected_through_offset = excluded.projected_through_offset", params![uuid.as_slice(), SqliteU64::new(frame.sequence), SqliteU64::new(frame.offset)])
            .map_err(|e| sqlite_error("advance tape-log projection mark", e))?;
        start = None;
    }
    tx.execute(
        "update tapes set unprotected_since = ?2 where tape_uuid = ?1",
        params![uuid.as_slice(), raw.summary().unprotected_since.as_deref()],
    )
    .map_err(|e| sqlite_error("project uncovered range age", e))?;
    if raw
        .summary()
        .finalization
        .as_ref()
        .is_some_and(|p| !p.prepared)
    {
        if let Some(plan) = snapshot.finalization() {
            if let Some(manual) = &plan.manual {
                let registration = upsert_idempotency_request(
                    tx,
                    IdempotencyRequestProjection {
                        actor_fingerprint: &manual.actor_fingerprint,
                        operation_kind: &manual.operation_kind,
                        idempotency_key: Uuid::from_bytes(manual.idempotency_key),
                        request_fingerprint: manual.request_fingerprint.to_vec(),
                        operation_id: Uuid::from_bytes(manual.operation_id),
                        updated_at_utc: &updated_at,
                    },
                    IdempotencyProjectionMode::Live,
                )?;
                if registration.operation_id != Uuid::from_bytes(manual.operation_id) {
                    return Err(StateError::IdempotencyConflict(
                        "tape-log decision conflicts with the catalog idempotency binding".into(),
                    ));
                }
            }
            project_terminal_log_progress_core(
                tx,
                &TerminalFinalizationProjectionInput {
                    tape_uuid: uuid,
                    trigger: plan.trigger,
                    operation_id: plan
                        .manual
                        .as_ref()
                        .map(|m| Uuid::from_bytes(m.operation_id)),
                    progress: plan.progress,
                    edition_digest: plan.edition_digest,
                    layout_digest: plan.layout.layout_digest,
                    outcome: if raw.summary().sealed {
                        TerminalFinalizationOutcome::Finalized
                    } else if plan.recovery_required {
                        TerminalFinalizationOutcome::RecoveryRequired
                    } else {
                        TerminalFinalizationOutcome::InProgress
                    },
                    updated_at_utc: None,
                },
                &updated_at,
                historical,
            )?;
        }
    }
    let summary = raw.summary();
    if summary.last_closed_sequence != 0 {
        // The written extent is physical: cutting the log (a tail repair) does
        // not shrink what is on tape, and capacity accounting reads it. So the
        // reconcile only raises it, like every other extent projection.
        tx.execute("update tapes set highest_protected_ordinal = ?2, total_committed_ordinals = ?3, last_committed_tape_file = ?4, written_extent_lba = case when written_extent_lba is null or written_extent_lba < ?5 then ?5 else written_extent_lba end where tape_uuid = ?1 and (highest_protected_ordinal is not ?2 or total_committed_ordinals is not ?3 or last_committed_tape_file is not ?4 or written_extent_lba is null or written_extent_lba < ?5)", params![uuid.as_slice(), SqliteU64::new(summary.protected), SqliteU64::new(summary.total), summary.next_file.checked_sub(1).map(SqliteU64::new), SqliteU64::new(summary.cursor.lba)]).map_err(|e| sqlite_error("reconcile tape-log summary projection", e))?;
    }
    tx.execute("update object_instances set status = 'missing' where tape_uuid = ?1 and exists(select 1 from tapes where tape_uuid = ?1 and state = 'retired')", params![uuid.as_slice()])
        .map_err(|e| sqlite_error("preserve retired copy status", e))?;
    Ok(())
}

/// Full rebuild alone compares historical catalog evidence. Missing rows are
/// repairable; contradicting rows are refused. Temporary key tables keep the
/// extra-member check bounded even for multi-million-member Objects.
fn validate_projected_history(
    tx: &Connection,
    snapshot: &AuthoritySnapshot,
) -> Result<(), StateError> {
    let uuid = snapshot.raw.header().tape_uuid;
    tx.execute_batch("create temp table rebuild_members(object_id text, file_id text, primary key(object_id,file_id)); create temp table rebuild_objects(object_id text primary key);")
        .map_err(|e| sqlite_error("prepare rebuild comparison", e))?;
    let conflict = |kind: &str| {
        StateError::JournalReplayFailed(format!(
            "projection rebuild: tape {} {kind} conflicts with SQLite",
            hex_uuid(uuid)
        ))
    };
    let mut failure = None;
    snapshot.raw.visit_file_rows(|entry| {
        let result = (|| -> Result<(), StateError> {
            let expected = checkpoint_tape_file_view(uuid, entry)?;
            let actual = tx.query_row("select tape_uuid, tape_file_number, kind, block_count, physical_start_hint, object_id, first_parity_data_ordinal, epoch_id, protected_ordinal_start, protected_ordinal_end_exclusive, canonical_metadata_hash, canonical_metadata_hash_algorithm, bundle_uuid, bundle_kind from tape_files where tape_uuid = ?1 and tape_file_number = ?2", params![uuid.as_slice(), SqliteU64::new(entry.tape_file_number)], checkpoint_tape_file_view_from_row).optional().map_err(|e| sqlite_error("compare rebuild tape file", e))?;
            if actual.is_some_and(|a| checkpoint_tape_file_conflicts(&expected, &a)) { return Err(conflict("tape file")); }
            Ok(())
        })();
        result.map_err(|e| { failure = Some(e); StoreError::Refused("rebuild file comparison failed") })
    }).map_err(|e| failure.take().unwrap_or_else(|| e.into()))?;
    if let Some(mut decoder) = snapshot.raw.decoder() {
        let mut start = None;
        while let Some(frame) = decoder.next_row_frame().map_err(StoreError::from)? {
            let (sequence, begin) = *start.get_or_insert((frame.sequence, frame.offset));
            if matches!(frame.record, Record::PayloadContinuation(_)) {
                continue;
            }
            let group = snapshot.raw.group_snapshot(sequence, begin, frame.end);
            start = None;
            let commit = match &frame.record {
                Record::Committed(c)
                | Record::TerminalPrefix { commit: c, .. }
                | Record::ComponentWritten { commit: c, .. } => c,
                _ => continue,
            };
            group.visit_catalog(|event| {
        let result = (|| -> Result<(), StateError> {
            match event {
                CatalogPayload::Object(p) => {
                    validate_object_metadata(tx, &p.object, uuid, frame.sequence, commit.batch_id)?;
                    tx.execute("insert or ignore into rebuild_objects values (?1)", params![p.object.object_id]).map_err(|e| sqlite_error("retain rebuild object key", e))?;
                    let mut expected = checkpoint_object_instance_view(&p.instance)?;
                    let actual = tx.query_row("select object_id, tape_uuid, tape_file_number, first_body_lba, first_parity_data_ordinal, protected_until_ordinal, status, representation, recipient_epoch_ids, metadata_frame_len, plaintext_digest, plaintext_digest_algorithm, stored_digest, stored_digest_algorithm from object_instances where tape_uuid = ?1 and tape_file_number = ?2", params![uuid.as_slice(), SqliteU64::new(p.instance.tape_file_number)], checkpoint_object_instance_view_from_row).optional().map_err(|e| sqlite_error("compare rebuild copy", e))?;
                    if let Some(a) = actual {
                        let retired: bool = tx.query_row("select exists(select 1 from tapes where tape_uuid = ?1 and state = 'retired')", params![uuid.as_slice()], |r| r.get(0)).map_err(|e| sqlite_error("read rebuild retired status", e))?;
                        if retired && a.status == "missing" { expected.status = a.status.clone(); }
                        if checkpoint_object_instance_conflicts(&expected, &a) { return Err(conflict("Object copy")); }
                    }
                }
                CatalogPayload::Member(member) => {
                    let expected = checkpoint_object_file_view(member)?;
                    let actual = tx.query_row("select object_id, file_id, path, size_bytes, file_sha256, file_digest_algorithm, first_chunk_lba, chunk_count, mtime, executable from object_files where object_id = ?1 and file_id = ?2", params![member.object_id, member.file_id], |r| Ok(CatalogObjectFileView { object_id:r.get(0)?, file_id:r.get(1)?, path:r.get(2)?, size_bytes:r.get(3)?, file_sha256:r.get(4)?, file_digest_algorithm:r.get(5)?, first_chunk_lba:r.get(6)?, chunk_count:r.get(7)?, mtime:r.get(8)?, executable:r.get(9)? })).optional().map_err(|e| sqlite_error("compare rebuild member", e))?;
                    if actual.is_some_and(|a| a != expected) { return Err(conflict("Object member")); }
                    tx.execute("insert or ignore into rebuild_members values (?1, ?2)", params![member.object_id, member.file_id]).map_err(|e| sqlite_error("retain rebuild member key", e))?;
                }
                CatalogPayload::ObjectEnd => (),
            }
            Ok(())
        })();
        result.map_err(|e| { failure = Some(e); StoreError::Refused("rebuild catalog comparison failed") })
    }).map_err(|e| failure.take().unwrap_or_else(|| e.into()))?;
        }
    }
    let extra: bool = tx.query_row("select exists(select 1 from object_files f join rebuild_objects o on o.object_id = f.object_id where not exists(select 1 from rebuild_members m where m.object_id = f.object_id and m.file_id = f.file_id))", [], |r| r.get(0)).map_err(|e| sqlite_error("compare extra rebuild members", e))?;
    if extra {
        return Err(conflict("extra Object member"));
    }
    tx.execute_batch("drop table rebuild_members; drop table rebuild_objects;")
        .map_err(|e| sqlite_error("finish rebuild comparison", e))?;
    Ok(())
}

fn validate_object_metadata(
    tx: &Connection,
    object: &NativeObjectProjectionInput,
    uuid: [u8; 16],
    sequence: u64,
    batch: [u8; 16],
) -> Result<(), StateError> {
    let actual: Option<CatalogObjectView> = tx.query_row(
        "select caller_object_id, body_format, logical_size_bytes, content_hash, content_hash_algorithm, metadata_hash, metadata_hash_algorithm, created_at_utc from objects where object_id = ?1", params![object.object_id],
        |r| Ok(CatalogObjectView { caller_object_id: r.get(0)?, body_format: r.get(1)?, logical_size_bytes: r.get(2)?, content_hash: r.get(3)?, content_hash_algorithm: r.get(4)?, metadata_hash: r.get(5)?, metadata_hash_algorithm: r.get(6)?, created_at_utc: Some(r.get(7)?) })).optional().map_err(|e| sqlite_error("validate projected object metadata", e))?;
    if let Some(actual) = actual {
        let mut expected = checkpoint_object_view(object)?;
        if expected.created_at_utc.is_none() {
            expected.created_at_utc = actual.created_at_utc.clone();
        }
        if checkpoint_object_conflicts(&expected, &actual) {
            return Err(StateError::JournalReplayFailed(format!("tape-log record tape={} sequence={} batch={} component=object {} conflicts with SQLite: log={expected:?}; sqlite={actual:?}", hex_uuid(uuid), sequence, hex_uuid(batch), object.object_id)));
        }
    }
    Ok(())
}

type ProjectedTerminalIdentity = (String, String, Option<String>, Vec<u8>, Vec<u8>, String);

// Earlier native groups may precede a directly projected acceptance/progress
// row. Preserve a matching later view while still rejecting identity changes.
fn project_terminal_log_progress_core(
    tx: &Connection,
    input: &TerminalFinalizationProjectionInput,
    updated: &str,
    historical: bool,
) -> Result<(), StateError> {
    let existing: Option<ProjectedTerminalIdentity> = tx.query_row(
        "select finalization_progress, finalization_trigger, finalization_operation_id, finalization_edition_digest, finalization_layout_digest, finalization_outcome from tapes where tape_uuid = ?1 and finalization_progress is not null",
        params![input.tape_uuid.as_slice()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))).optional().map_err(|e| sqlite_error("read projected terminal identity", e))?;
    if let Some((progress, trigger, operation, edition, layout, outcome)) = existing {
        if trigger != terminal_finalization_trigger_name(input.trigger)
            || operation != input.operation_id.map(|id| id.to_string())
            || edition != input.edition_digest
            || layout != input.layout_digest
        {
            return Err(StateError::JournalReplayFailed(
                "terminal finalization identity changed in tape-log projection".into(),
            ));
        }
        let progress = parse_terminal_finalization_progress(&progress)?;
        if (progress != input.progress && terminal_progress_reaches(input.progress, progress))
            || (progress == input.progress
                && input.outcome == TerminalFinalizationOutcome::InProgress
                && matches!(
                    outcome.as_str(),
                    "recovery_required" | "finalized" | "finalized_degraded"
                ))
        {
            return Ok(());
        }
    }
    project_terminal_finalization_core(tx, input, updated, historical)
}

/// Fixture projection uses actual closed native groups and the same comparison
/// and projection functions as a full rebuild. Only this tape's acceleration
/// mark is discarded, allowing tests to model a lost or partial SQLite cache.
#[cfg(any(test, feature = "fixtures"))]
impl CatalogCore {
    /// Publish a semantic fixture to a native log, then reconstruct its catalog.
    pub fn project_checkpoint_record(
        &mut self,
        record: &crate::CommitFixture,
        path: &Path,
    ) -> Result<(), StateError> {
        use crate::authority::{TapeAuthority, TapeAuthorityView, TapeAuthorityWriter};
        let directory = path.with_extension("fixture-logs");
        std::fs::create_dir_all(&directory).map_err(StoreError::Io)?;
        let authority = TapeAuthority::open_fixture(directory, record.tape_uuid)?;
        let mut writer = if authority.pending_finalization()?.is_some() {
            authority.acquire_exclusive_for_terminal_recovery()?
        } else {
            authority.acquire_exclusive()?
        };
        if writer
            .latest_commit()?
            .is_none_or(|c| c.ordinal < record.ordinal)
        {
            if record.sealed_after_write {
                let mut intent = record.terminal_finalization.clone().ok_or_else(|| {
                    StateError::JournalReplayFailed(
                        "fixture seal requires finalization plan".into(),
                    )
                })?;
                intent.progress = TerminalFinalizationProgress::BeforeReplicaA;
                writer.begin_finalization(&(&intent).into())?;
                if let Some(prefix) = &intent.terminal_prefix {
                    writer
                        .publish_terminal_prefix(&(&prefix.committed_bundle).try_into().map_err(
                            |e: remanence_parity::JournalError| {
                                StateError::JournalReplayFailed(e.to_string())
                            },
                        )?)
                        .map_err(|e| StateError::JournalReplayFailed(e.to_string()))?;
                }
                for (before, after) in [
                    (
                        TerminalFinalizationProgress::BeforeReplicaA,
                        TerminalFinalizationProgress::AfterReplicaA,
                    ),
                    (
                        TerminalFinalizationProgress::AfterReplicaA,
                        TerminalFinalizationProgress::AfterSeparationAb,
                    ),
                    (
                        TerminalFinalizationProgress::AfterSeparationAb,
                        TerminalFinalizationProgress::AfterReplicaB,
                    ),
                    (
                        TerminalFinalizationProgress::AfterReplicaB,
                        TerminalFinalizationProgress::AfterSeparationBc,
                    ),
                    (
                        TerminalFinalizationProgress::AfterSeparationBc,
                        TerminalFinalizationProgress::AfterReplicaC,
                    ),
                ] {
                    writer.advance_finalization(before, after)?;
                }
                writer.publish_sealed_reports(&[record.into()], || Ok(()))?;
            } else {
                writer.publish_report(&record.into())?;
            }
        }
        let snapshot = writer.freeze()?;
        let tx = projection_transaction(&mut self.conn)
            .map_err(|e| sqlite_error("begin native fixture reconstruction", e))?;
        validate_projected_history(&tx, &snapshot)?;
        tx.execute(
            "delete from tape_log_projection where tape_uuid = ?1",
            params![record.tape_uuid.as_slice()],
        )
        .map_err(|e| sqlite_error("discard fixture projection mark", e))?;
        project_snapshot(&tx, &snapshot)?;
        tx.commit()
            .map_err(|e| sqlite_error("commit native fixture reconstruction", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authority::{TapeAuthority, TapeAuthorityView, TapeAuthorityWriter};
    use crate::checkpoint::tests::{record, second_record};

    /// Audit refresh cannot erase native session rows behind an already advanced
    /// tape watermark. Live catch-up and a fresh rebuild consume the same facts.
    #[test]
    fn native_sessions_survive_audit_refresh_and_idempotent_catch_up() {
        let root = tempfile::tempdir().unwrap();
        let id = [0xa1; 16];
        std::fs::write(
            root.path()
                .join(format!("{}.remtape", Uuid::from_bytes(id).simple())),
            include_bytes!("../../../../fixtures/tape-log/session-tail-synchronized.remtape"),
        )
        .unwrap();
        let authority = TapeAuthority::open_fixture(root.path(), id).unwrap();
        let mut lease = authority.acquire_exclusive().unwrap();
        lease
            .open_session(
                [0xda; 16],
                "2026-11-05T00:00:00Z".into(),
                crate::tape_log::SessionDrive {
                    serial: "DRV-TEST".into(),
                    vendor: None,
                    product: None,
                    endpoint: crate::library_log::DriveKey::Standalone(
                        crate::library_log::Standalone {
                            drive_serial: "DRV-TEST".into(),
                        },
                    ),
                    drive_uuid: None,
                },
            )
            .unwrap();
        let snapshot = lease.freeze().unwrap();
        let mut expected = std::collections::BTreeMap::new();
        snapshot
            .raw
            .visit_frames(|frame| {
                match &frame.record {
                    Record::SessionOpened(v) => {
                        expected.insert(Uuid::from_bytes(v.session_id), "open");
                    }
                    Record::SessionEnded(v) => {
                        expected.insert(Uuid::from_bytes(v.session_id), "closed");
                    }
                    _ => {}
                }
                Ok(())
            })
            .unwrap();
        assert!(expected.values().any(|state| *state == "closed"));
        assert_eq!(expected[&Uuid::from_bytes([0xda; 16])], "open");
        let mut index = crate::projector::open(root.path().join("index.sqlite"))
            .unwrap()
            .write_handle();
        index.project_tape_log(&snapshot).unwrap();
        for _ in 0..2 {
            index.replay_audit_records(&[]).unwrap();
            index.project_tape_log(&snapshot).unwrap();
            for (id, state) in &expected {
                assert_eq!(index.session_state(*id).unwrap().as_deref(), Some(*state));
            }
            assert_eq!(
                index.projected_through_sequence(id).unwrap(),
                snapshot.log_reference().0
            );
        }
        let mut rebuilt = crate::projector::open(root.path().join("rebuilt.sqlite"))
            .unwrap()
            .write_handle();
        rebuilt
            .rebuild_tape_log_projection(&[], &[snapshot])
            .unwrap();
        for (id, state) in expected {
            assert_eq!(rebuilt.session_state(id).unwrap().as_deref(), Some(state));
        }
    }

    #[test]
    fn projection_is_atomic_idempotent_and_frozen_order_cannot_regress() {
        let root = tempfile::tempdir().unwrap();
        let id = [0x67; 16];
        let authority = TapeAuthority::open_fixture(root.path(), id).unwrap();
        let mut lease = authority.acquire_exclusive().unwrap();
        lease.publish_report(&(&record(id)).into()).unwrap();
        let older = lease.freeze().unwrap();
        lease.publish_report(&(&second_record(id)).into()).unwrap();
        let newer = lease.freeze().unwrap();
        let mut index = crate::projector::open(root.path().join("index.sqlite"))
            .map(|catalog| catalog.write_handle())
            .unwrap();
        Connection::open(index.path()).unwrap().execute_batch("create trigger fail_mark before insert on tape_log_projection begin select raise(abort, 'projection failure'); end;").unwrap();
        assert!(index.project_tape_log(&newer).is_err());
        assert_eq!(index.projected_through_sequence(id).unwrap(), 0);
        assert_eq!(
            Connection::open(index.path())
                .unwrap()
                .query_row("select count(*) from object_instances", [], |r| r
                    .get::<_, u64>(0))
                .unwrap(),
            0
        );
        Connection::open(index.path())
            .unwrap()
            .execute_batch("drop trigger fail_mark")
            .unwrap();
        index.project_tape_log(&newer).unwrap();
        assert!(index.project_tape_log(&older).is_err());
        index.project_tape_log(&newer).unwrap();
        assert_eq!(
            index.projected_through_sequence(id).unwrap(),
            newer.raw.summary().last_closed_sequence
        );
        assert_eq!(
            Connection::open(index.path())
                .unwrap()
                .query_row("select count(*) from object_instances", [], |r| r
                    .get::<_, u64>(0))
                .unwrap(),
            2
        );
        let instances = index
            .find_native_object_instances(&record(id).objects[0].object.object_id)
            .unwrap();
        assert_eq!(
            instances[0].tape_log_sequence,
            Some(older.raw.summary().last_closed_sequence)
        );
        assert_eq!(instances[0].tape_incarnation, Some(1));
    }

    thread_local! { static SQL_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
    #[test]
    fn incremental_projection_decodes_only_new_payloads_and_constant_sql_per_barrier() {
        let root = tempfile::tempdir().unwrap();
        let id = [0x66; 16];
        let authority = TapeAuthority::open_fixture(root.path(), id).unwrap();
        let mut writer = authority.acquire_exclusive().unwrap();
        let mut index = crate::projector::open(root.path().join("index.sqlite"))
            .map(|catalog| catalog.write_handle())
            .unwrap();
        index
            .with_core(|core| {
                core.conn
                    .trace_v2(
                        rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
                        Some(|_: rusqlite::trace::TraceEvent<'_>| {
                            SQL_COUNT.set(SQL_COUNT.get() + 1)
                        }),
                    )
                    .expect("register SQL statement trace");
                Ok(())
            })
            .unwrap();
        let mut baseline = None;
        for n in 1..=80u64 {
            let mut r = record(id);
            r.ordinal = n;
            r.committed_object_count = n;
            r.eod_lba = 2 + n * 4;
            r.next_tape_file_number = n + 1;
            r.batch_id = Uuid::from_u128(n as u128).into_bytes();
            let object = Uuid::from_u128(1000 + n as u128).to_string();
            r.objects[0].object.object_id = object.clone();
            r.objects[0].instance.object_id = object.clone();
            r.objects[0].instance.tape_file_number = n;
            r.objects[0].object_recovery_row.object_id = object.into_bytes();
            r.objects[0].object_recovery_row.tape_file_number = n;
            r.objects[0].fresh_tape = n == 1;
            r.objects[0].total_committed_ordinals = n * 3;
            writer.publish_report(&(&r).into()).unwrap();
            let snapshot = writer.freeze().unwrap();
            crate::tape_log::measure::reset();
            SQL_COUNT.set(0);
            index.project_tape_log(&snapshot).unwrap();
            let count = crate::tape_log::measure::counts().catalog_payloads;
            assert_eq!(count, 1, "barrier {n} decoded history");
            let statements = SQL_COUNT.get();
            assert!(
                statements > 0,
                "barrier {n} must trace the catalog connection"
            );
            if n == 2 {
                eprintln!("SQL statements per incremental barrier: {statements}");
                baseline = Some(statements);
            }
            if n > 2 {
                assert_eq!(
                    Some(statements),
                    baseline,
                    "barrier {n} executed history SQL"
                );
            }
            crate::tape_log::measure::reset();
            index.project_tape_log(&snapshot).unwrap();
            assert_eq!(crate::tape_log::measure::counts().catalog_payloads, 0);
        }
    }
    #[test]
    fn projection_mark_validates_the_named_closing_frame() {
        let root = tempfile::tempdir().unwrap();
        let id = [0x65; 16];
        let authority = TapeAuthority::open_fixture(root.path(), id).unwrap();
        let mut writer = authority.acquire_exclusive().unwrap();
        writer.publish_report(&(&record(id)).into()).unwrap();
        let snapshot = writer.freeze().unwrap();
        let mut index = crate::projector::open(root.path().join("index.sqlite"))
            .map(|catalog| catalog.write_handle())
            .unwrap();
        index.project_tape_log(&snapshot).unwrap();
        let (_, offset) = projection_mark(&Connection::open(index.path()).unwrap(), id)
            .unwrap()
            .unwrap();
        Connection::open(index.path())
            .unwrap()
            .execute(
                "update tape_log_projection set projected_through_offset = ?1",
                params![SqliteU64::new(offset + 1)],
            )
            .unwrap();
        let error = index.project_tape_log(&snapshot).unwrap_err();
        assert!(
            matches!(&error, StateError::TapeLog(e) if e.is_invalid_projection_mark() && !e.is_below_projection()),
            "{error}"
        );
    }
    #[test]
    fn object_log_join_reprojection_is_idempotent_and_refuses_a_different_sequence() {
        let root = tempfile::tempdir().unwrap();
        let id = [0x64; 16];
        let authority = TapeAuthority::open_fixture(root.path(), id).unwrap();
        let mut writer = authority.acquire_exclusive().unwrap();
        writer.publish_report(&(&record(id)).into()).unwrap();
        let snapshot = writer.freeze().unwrap();
        let mut index = crate::projector::open(root.path().join("index.sqlite"))
            .map(|catalog| catalog.write_handle())
            .unwrap();
        index.project_tape_log(&snapshot).unwrap();
        Connection::open(index.path())
            .unwrap()
            .execute("delete from tape_log_projection", [])
            .unwrap();
        index.project_tape_log(&snapshot).unwrap();
        Connection::open(index.path())
            .unwrap()
            .execute("delete from tape_log_projection", [])
            .unwrap();
        Connection::open(index.path())
            .unwrap()
            .execute(
                "update tape_log_object_commits set tape_log_sequence = ?1",
                params![SqliteU64::new(99)],
            )
            .unwrap();
        assert!(index
            .project_tape_log(&snapshot)
            .unwrap_err()
            .to_string()
            .contains("object log reference conflicts"));
    }
    #[test]
    fn full_rebuild_compares_object_file_instance_and_member_evidence() {
        for update in [
            "update objects set body_format = 'contradiction'",
            "update tape_files set block_count = X'0000000000000063' where tape_file_number = X'0000000000000001'",
            "update object_instances set first_body_lba = X'0000000000000063'",
            "update object_files set path = 'contradiction'",
        ] {
            let root = tempfile::tempdir().unwrap(); let id = [0x63; 16];
            let authority = TapeAuthority::open_fixture(root.path(), id).unwrap();
            let mut r = record(id);
            let object_id = r.objects[0].object.object_id.clone();
            r.objects[0].files.push(NativeObjectFileProjectionInput {
                object_id, file_id: "member".into(), path: "member.txt".into(),
                size_bytes: 0, file_sha256: vec![0x32; 32], first_chunk_lba: None, chunk_count: 0, mtime: None, executable: None,
            });
            let mut writer = authority.acquire_exclusive().unwrap(); writer.publish_report(&(&r).into()).unwrap();
            let snapshot = writer.freeze().unwrap();
            let mut index = crate::projector::open(root.path().join("index.sqlite")).map(|catalog| catalog.write_handle()).unwrap();
            index.project_tape_log(&snapshot).unwrap();
            Connection::open(index.path()).unwrap().execute_batch(update).unwrap();
            assert!(index.rebuild_tape_log_projection(&[], &[snapshot]).unwrap_err().to_string().contains("conflicts with SQLite"), "{update}");
        }
    }
    #[test]
    fn repair_protects_manual_idempotency_and_operation_rows_even_without_tape_columns() {
        let root = tempfile::tempdir().unwrap();
        let id = [0x62; 16];
        let authority = TapeAuthority::open_fixture(root.path(), id).unwrap();
        let mut writer = authority.acquire_exclusive().unwrap();
        writer.publish_report(&(&record(id)).into()).unwrap();
        let before = writer.freeze().unwrap();
        let intent: crate::authority::AuthorityFinalization =
            (&crate::checkpoint::tests::finalization_intent(id)).into();
        writer.begin_finalization(&intent).unwrap();
        writer.accept_preparation().unwrap();
        let mut index = crate::projector::open(root.path().join("index.sqlite"))
            .map(|catalog| catalog.write_handle())
            .unwrap();
        index.project_tape_log(&writer.freeze().unwrap()).unwrap();
        Connection::open(index.path()).unwrap().execute_batch("update tapes set finalization_progress=null, finalization_trigger=null, finalization_operation_id=null, finalization_edition_digest=null, finalization_layout_digest=null, finalization_outcome=null, completed_replicas=null, state='ready'").unwrap();
        assert!(index
            .validate_repair_prefix(&before)
            .unwrap_err()
            .to_string()
            .contains("separate explicit forensic recovery path"));
        Connection::open(index.path())
            .unwrap()
            .execute("delete from idempotency_keys", [])
            .unwrap();
        Connection::open(index.path()).unwrap().execute("insert into operations(operation_id,operation_kind,state,subject,started_at_utc,updated_at_utc) values ('accepted-operation','finalize_tape','running',?1,'now','now')", params![Uuid::from_bytes(id).to_string()]).unwrap();
        assert!(index
            .validate_repair_prefix(&before)
            .unwrap_err()
            .to_string()
            .contains("separate explicit forensic recovery path"));
    }
    #[test]
    fn repair_scope_ignores_other_tapes_and_finalize_requests_that_accepted_nothing() {
        let root = tempfile::tempdir().unwrap();
        let id = [0x61; 16];
        let authority = TapeAuthority::open_fixture(root.path(), id).unwrap();
        let mut writer = authority.acquire_exclusive().unwrap();
        writer.publish_report(&(&record(id)).into()).unwrap();
        let before = writer.freeze().unwrap();
        let mut index = crate::projector::open(root.path().join("index.sqlite"))
            .map(|catalog| catalog.write_handle())
            .unwrap();
        index.project_tape_log(&before).unwrap();
        index.validate_repair_prefix(&before).unwrap();
        let this = format!("tape:{}", Uuid::from_bytes(id));
        let other = format!("tape:{}", Uuid::from_bytes([0x60; 16]));
        let operation = |index: &CatalogIndex, op: &str, state: &str, subject: &str| {
            Connection::open(index.path()).unwrap().execute("insert into operations(operation_id,operation_kind,state,subject,started_at_utc,updated_at_utc) values (?1,'finalize_tape',?2,?3,'now','now')", params![op, state, subject]).unwrap();
        };
        let key = |index: &CatalogIndex, op: &str, terminal: Option<&str>| {
            Connection::open(index.path()).unwrap().execute("insert into idempotency_keys(actor_fingerprint,operation_kind,idempotency_key,request_fingerprint,operation_id,terminal_state,response_fingerprint,updated_at_utc) values ('actor','finalize_tape',?1,X'00',?1,?2,null,'now')", params![op, terminal]).unwrap();
        };
        let refused = |index: &CatalogIndex| {
            index
                .validate_repair_prefix(&before)
                .unwrap_err()
                .to_string()
                .contains("separate explicit forensic recovery path")
        };
        // An orphan request for another tape is that tape's obligation.
        operation(&index, "other-tape", "running", &other);
        key(&index, "other-tape", None);
        index.validate_repair_prefix(&before).unwrap();
        // A request for this tape cancelled before dispatch accepted nothing.
        operation(&index, "cancelled", "cancelled_before_dispatch", &this);
        key(&index, "cancelled", Some("cancelled_before_dispatch"));
        index.validate_repair_prefix(&before).unwrap();
        // A failed attempt may sit on a plan already durable in the log and
        // resumes under its key, so it refuses like a non-terminal or accepted
        // operation.
        for state in ["failed", "running", "finished", "completion_unknown"] {
            operation(&index, "pending", state, &this);
            assert!(refused(&index), "{state}");
            Connection::open(index.path())
                .unwrap()
                .execute("delete from operations where operation_id = 'pending'", [])
                .unwrap();
        }
        // So does a live request of unknown owner.
        key(&index, "unknown-owner", None);
        assert!(refused(&index));
    }
    #[test]
    fn reconcile_after_tail_repair_keeps_the_higher_physical_extent() {
        let root = tempfile::tempdir().unwrap();
        let id = [0x5f; 16];
        let authority = TapeAuthority::open_fixture(root.path(), id).unwrap();
        let mut lease = authority.acquire_exclusive().unwrap();
        lease.publish_report(&(&record(id)).into()).unwrap();
        let older = lease.freeze().unwrap();
        lease.publish_report(&(&second_record(id)).into()).unwrap();
        drop(lease);
        let mut index = crate::projector::open(root.path().join("index.sqlite"))
            .map(|catalog| catalog.write_handle())
            .unwrap();
        index.project_tape_log(&older).unwrap();
        let extent = |index: &CatalogIndex| {
            Connection::open(index.path())
                .unwrap()
                .query_row(
                    "select written_extent_lba from tapes where tape_uuid = ?1",
                    params![id.as_slice()],
                    |r| r.get::<_, SqliteU64>(0),
                )
                .unwrap()
                .get()
        };
        let retained = older.raw.summary().cursor.lba;
        assert_eq!(extent(&index), retained);
        // The second group reached tape before its log tail was damaged.
        let physical = second_record(id).eod_lba.max(retained + 1);
        Connection::open(index.path())
            .unwrap()
            .execute(
                "update tapes set written_extent_lba = ?2 where tape_uuid = ?1",
                params![id.as_slice(), SqliteU64::new(physical)],
            )
            .unwrap();
        let store = crate::tape_log::store::Store::open_fixture(root.path(), id).unwrap();
        let mut bytes = std::fs::read(store.path()).unwrap();
        bytes[older.raw.summary().closed_end as usize + 28] ^= 0x80;
        std::fs::write(store.path(), &bytes).unwrap();
        let sequence = older.raw.summary().last_closed_sequence;
        store.repair_tail(sequence, sequence).unwrap();
        let repaired = authority.freeze().unwrap();
        assert_eq!(repaired.raw.summary().cursor.lba, retained);
        index.project_tape_log(&repaired).unwrap();
        assert_eq!(
            extent(&index),
            physical,
            "a reconcile never lowers the extent"
        );
        // The explicit repaired-tape rebuild re-derives it from the prefix.
        index.rebuild_repaired_tape_projection(&repaired).unwrap();
        assert_eq!(extent(&index), retained);
    }
    #[test]
    fn repair_below_projection_rebuilds_only_the_selected_tape() {
        let root = tempfile::tempdir().unwrap();
        let id = [0x68; 16];
        let authority = TapeAuthority::open_fixture(root.path(), id).unwrap();
        let mut lease = authority.acquire_exclusive().unwrap();
        lease.publish_report(&(&record(id)).into()).unwrap();
        let older = lease.freeze().unwrap();
        lease.publish_report(&(&second_record(id)).into()).unwrap();
        let newer = lease.freeze().unwrap();
        let mut index = crate::projector::open(root.path().join("index.sqlite"))
            .map(|catalog| catalog.write_handle())
            .unwrap();
        index.project_tape_log(&newer).unwrap();
        let other_id = [0x69; 16];
        let other = TapeAuthority::open_fixture(root.path(), other_id).unwrap();
        let mut other_report = record(other_id);
        let other_object = uuid::Uuid::from_bytes([0x96; 16]).to_string();
        other_report.objects[0].object.object_id = other_object.clone();
        other_report.objects[0].instance.object_id = other_object.clone();
        other_report.objects[0].object_recovery_row.object_id = other_object.as_bytes().to_vec();
        let mut other_lease = other.acquire_exclusive().unwrap();
        other_lease.publish_report(&(&other_report).into()).unwrap();
        index
            .project_tape_log(&other_lease.freeze().unwrap())
            .unwrap();
        let other_mark = index.projected_through_sequence(other_id).unwrap();
        drop(other_lease);
        drop(lease);
        let store = crate::tape_log::store::Store::open_fixture(root.path(), id).unwrap();
        assert!(matches!(
            store.repair_tail(
                older.raw.summary().last_closed_sequence,
                newer.raw.summary().last_closed_sequence
            ),
            Err(StoreError::BelowProjection { .. })
        ));
        let mut bytes = std::fs::read(store.path()).unwrap();
        bytes[older.raw.summary().closed_end as usize + 28] ^= 0x80;
        std::fs::write(store.path(), &bytes).unwrap();
        let evidence = store
            .repair_tail(older.raw.summary().last_closed_sequence, 0)
            .unwrap();
        assert_eq!(std::fs::read(evidence.archive).unwrap(), bytes);
        index
            .rebuild_repaired_tape_projection(&authority.freeze().unwrap())
            .unwrap();
        assert_eq!(
            index.projected_through_sequence(id).unwrap(),
            older.raw.summary().last_closed_sequence
        );
        assert_eq!(
            Connection::open(index.path())
                .unwrap()
                .query_row("select count(*) from object_instances", [], |r| r
                    .get::<_, u64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            index.projected_through_sequence(other_id).unwrap(),
            other_mark
        );
        assert!(index.get_native_object(&other_object).unwrap().is_some());
        assert!(index
            .get_native_object(&second_record(id).objects[0].object.object_id)
            .unwrap()
            .is_none());
        assert!(
            authority.acquire_exclusive().is_err(),
            "physical repair fence survives projection repair"
        );
    }
}

impl CatalogCore {
    /// A direct catalog writer invalidates completion, but leaves source hashes.
    pub(in crate::projector) fn forget_projection_marker(&mut self, tape: [u8; 16]) {
        self.projected_tapes.remove(&tape);
    }

    /// A rebuild or a new state lease cannot trust earlier process-local commits.
    pub(in crate::projector) fn forget_projection_markers(&mut self) {
        self.projected_tapes.clear();
    }

    /// Commit bounded chunks only when this projection owns the outer transaction.
    /// Nested projections leave atomicity to their caller.
    pub(super) fn project_tape_log_bounded(
        &mut self,
        snapshot: &AuthoritySnapshot,
    ) -> Result<(), StateError> {
        // A newly opened native writer has a synthetic spool header offset but
        // no published authority. It contributes no rows or projection mark.
        // Published header-only sequence zero still has a real header digest.
        if snapshot.raw.summary().last_closed_sequence == 0
            && snapshot.raw.header_digest()?.is_none()
        {
            return Ok(());
        }
        let uuid = snapshot.raw.header().tape_uuid;
        let prior: Option<(SqliteU64, Vec<u8>)> = self
            .conn
            .query_row(
                "select byte_end,source_digest from tape_projection_sources where tape_uuid=?1",
                [uuid.as_slice()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| {
                self.forget_projection_marker(uuid);
                sqlite_error("read tape source digest", e)
            })?;
        let resume:Option<(SqliteU64,SqliteU64)>=self.conn.query_row("select through_sequence,rows_committed from tape_projection_chunks where tape_uuid=?1",[uuid.as_slice()],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(|e| { self.forget_projection_marker(uuid); sqlite_error("read tape chunk resume", e) })?;
        if resume.is_none()
            && self
                .projected_tapes
                .get(&uuid)
                .is_some_and(|marker| marker.matches(&snapshot.raw, prior.as_ref()))
            && projection_mark(&self.conn, uuid)
                .inspect_err(|_| {
                    self.forget_projection_marker(uuid);
                })?
                .is_some_and(|(sequence, offset)| {
                    sequence == snapshot.raw.summary().last_closed_sequence
                        && matches!(
                            snapshot.raw.projection_decoder(sequence, offset),
                            Ok(Some(_))
                        )
                })
        {
            #[cfg(any(test, feature = "fixtures"))]
            {
                self.tape_projection_skips += 1;
            }
            return Ok(());
        }
        self.forget_projection_marker(uuid);
        if let Some((end, digest)) = prior {
            if snapshot.raw.projection_prefix_digest(end.get())?.as_slice() != digest {
                return Err(StateError::IndexCorrupt(
                    "tape projection source digest changed".into(),
                ));
            }
        }
        let end = snapshot.raw.summary().closed_end;
        let digest = snapshot.raw.projection_prefix_digest(end)?;
        let tx = projection_transaction(&mut self.conn)
            .map_err(|e| sqlite_error("begin bounded tape projection", e))?;
        let owns_transaction = matches!(tx, ProjectionTransaction::Transaction(_));
        tx.execute("insert into tape_projection_sources values(?1,?2,?3,?4) on conflict(tape_uuid) do update set received_sequence=excluded.received_sequence,byte_end=excluded.byte_end,source_digest=excluded.source_digest",params![uuid.as_slice(),SqliteU64::new(snapshot.log_reference().0),SqliteU64::new(end),digest.as_slice()]).map_err(|e|sqlite_error("bind tape projection source",e))?;
        let mut count = 0;
        let mut last = None;
        project_snapshot_rows(&tx, snapshot, true, &mut |conn, sequence, row| {
            if owns_transaction && count == super::apply::TAPE_CHUNK_ROWS {
                let (sequence, row) = last.expect("nonempty chunk");
                conn.execute("insert into tape_projection_chunks values(?1,?2,?3) on conflict(tape_uuid) do update set through_sequence=excluded.through_sequence,rows_committed=excluded.rows_committed",params![uuid.as_slice(),SqliteU64::new(sequence),SqliteU64::new(row)]).map_err(|e|sqlite_error("checkpoint tape chunk",e))?;
                conn.execute_batch("commit; begin immediate")
                    .map_err(|e| sqlite_error("commit bounded tape chunk", e))?;
                count = 0;
            }
            if resume.is_some_and(|(s, r)| s.get() == sequence && row <= r.get()) {
                return Ok(true);
            }
            count += 1;
            last = Some((sequence, row));
            Ok(false)
        })?;
        tx.execute(
            "delete from tape_projection_chunks where tape_uuid=?1",
            [uuid.as_slice()],
        )
        .map_err(|e| sqlite_error("finish tape chunk projection", e))?;
        tx.commit()
            .map_err(|e| sqlite_error("commit final tape chunk", e))?;
        if owns_transaction {
            if let (Some(file), Ok(Some(identity))) =
                (snapshot.raw.projection_file(), snapshot.raw.file_identity())
            {
                self.projected_tapes.insert(
                    uuid,
                    ProjectedTape {
                        _file: file,
                        identity,
                        end,
                        digest,
                    },
                );
            }
        }
        Ok(())
    }
    /// Original full projection, with byte-zero digests and no in-memory marker.
    #[cfg(test)]
    pub(in crate::projector) fn project_tape_log_bounded_oracle(
        &mut self,
        snapshot: &AuthoritySnapshot,
    ) -> Result<(), StateError> {
        // A newly opened native writer has a synthetic spool header offset but
        // no published authority. It contributes no rows or projection mark.
        // Published header-only sequence zero still has a real header digest.
        if snapshot.raw.summary().last_closed_sequence == 0
            && snapshot.raw.header_digest()?.is_none()
        {
            return Ok(());
        }
        let uuid = snapshot.raw.header().tape_uuid;
        let prior: Option<(SqliteU64, Vec<u8>)> = self
            .conn
            .query_row(
                "select byte_end,source_digest from tape_projection_sources where tape_uuid=?1",
                [uuid.as_slice()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| sqlite_error("read tape source digest", e))?;
        if let Some((end, digest)) = prior {
            if snapshot
                .raw
                .projection_prefix_digest_oracle(end.get())?
                .as_slice()
                != digest
            {
                return Err(StateError::IndexCorrupt(
                    "tape projection source digest changed".into(),
                ));
            }
        }
        let end = snapshot.raw.summary().closed_end;
        let digest = snapshot.raw.projection_prefix_digest_oracle(end)?;
        let resume:Option<(SqliteU64,SqliteU64)>=self.conn.query_row("select through_sequence,rows_committed from tape_projection_chunks where tape_uuid=?1",[uuid.as_slice()],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(|e|sqlite_error("read tape chunk resume",e))?;
        let tx = projection_transaction(&mut self.conn)
            .map_err(|e| sqlite_error("begin bounded tape projection", e))?;
        let owns_transaction = matches!(tx, ProjectionTransaction::Transaction(_));
        tx.execute("insert into tape_projection_sources values(?1,?2,?3,?4) on conflict(tape_uuid) do update set received_sequence=excluded.received_sequence,byte_end=excluded.byte_end,source_digest=excluded.source_digest",params![uuid.as_slice(),SqliteU64::new(snapshot.log_reference().0),SqliteU64::new(end),digest.as_slice()]).map_err(|e|sqlite_error("bind tape projection source",e))?;
        let mut count = 0;
        let mut last = None;
        project_snapshot_rows(&tx, snapshot, true, &mut |conn, sequence, row| {
            if owns_transaction && count == super::apply::TAPE_CHUNK_ROWS {
                let (sequence, row) = last.expect("nonempty chunk");
                conn.execute("insert into tape_projection_chunks values(?1,?2,?3) on conflict(tape_uuid) do update set through_sequence=excluded.through_sequence,rows_committed=excluded.rows_committed",params![uuid.as_slice(),SqliteU64::new(sequence),SqliteU64::new(row)]).map_err(|e|sqlite_error("checkpoint tape chunk",e))?;
                conn.execute_batch("commit; begin immediate")
                    .map_err(|e| sqlite_error("commit bounded tape chunk", e))?;
                count = 0;
            }
            if resume.is_some_and(|(s, r)| s.get() == sequence && row <= r.get()) {
                return Ok(true);
            }
            count += 1;
            last = Some((sequence, row));
            Ok(false)
        })?;
        tx.execute(
            "delete from tape_projection_chunks where tape_uuid=?1",
            [uuid.as_slice()],
        )
        .map_err(|e| sqlite_error("finish tape chunk projection", e))?;
        tx.commit()
            .map_err(|e| sqlite_error("commit final tape chunk", e))
    }
}

#[cfg(test)]
mod b4b1_tests {
    use super::*;
    use crate::{
        authority::{TapeAuthority, TapeAuthorityView, TapeAuthorityWriter},
        projector::{
            apply::{ApplyPath, AuthorityRecord},
            thread::{ProjectionClient, ProjectionResult},
            CatalogReader,
        },
    };
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };
    fn large_snapshot(root: &std::path::Path, rows: usize) -> AuthoritySnapshot {
        let id = [0x74; 16];
        let authority = TapeAuthority::open_fixture(root, id).unwrap();
        let mut writer = authority.acquire_exclusive().unwrap();
        let mut record = crate::checkpoint::tests::record(id);
        let object = record.objects[0].object.object_id.clone();
        record.objects[0].files = (0..rows)
            .map(|n| NativeObjectFileProjectionInput {
                object_id: object.clone(),
                file_id: format!("file-{n:06}"),
                path: format!("member/{n:06}"),
                size_bytes: 1,
                file_sha256: vec![0x31; 32],
                first_chunk_lba: Some(0),
                chunk_count: 1,
                mtime: None,
                executable: None,
            })
            .collect();
        writer.publish_report(&(&record).into()).unwrap();
        writer.freeze().unwrap()
    }
    /// A partial native object resumes from its row watermark after SQL fails.
    #[test]
    fn authority_tape_chunk_failure_resumes_without_losing_or_duplicating_members() {
        let root = tempfile::tempdir().unwrap();
        let snapshot = large_snapshot(root.path(), 2000);
        let mut writer = crate::projector::open(root.path().join("catalog.sqlite"))
            .unwrap()
            .write_handle();
        let inspect = Connection::open(writer.path()).unwrap();
        inspect.execute_batch("create trigger stop_chunk before insert on object_files when (select count(*) from object_files)>=700 begin select raise(abort,'injected projection failure'); end;").unwrap();
        let source = AuthorityRecord::Tape(Box::new(snapshot));
        assert!(writer.apply(&source, ApplyPath::Live).is_err());
        let count: u64 = inspect
            .query_row("select count(*) from object_files", [], |r| r.get(0))
            .unwrap();
        assert!(count > 0 && count < 700, "committed chunk rows={count}");
        inspect.execute_batch("drop trigger stop_chunk").unwrap();
        writer.apply(&source, ApplyPath::Startup).unwrap();
        writer.apply(&source, ApplyPath::Startup).unwrap();
        assert_eq!(
            inspect
                .query_row("select count(*) from object_files", [], |r| r
                    .get::<_, u64>(0))
                .unwrap(),
            2000
        );
        assert_eq!(
            inspect
                .query_row("select count(*) from tape_projection_chunks", [], |r| r
                    .get::<_, u64>(0))
                .unwrap(),
            0
        );
    }
    /// A hot read never queues behind the writer during a 100k-member projection.
    #[test]
    fn authority_reader_latency_during_100k_row_tape_projection() {
        let root = tempfile::tempdir().unwrap();
        let snapshot = large_snapshot(root.path(), 100_000);
        let client = ProjectionClient::open(root.path().join("catalog.sqlite")).unwrap();
        let reader: CatalogReader = client.reader().unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            done_tx
                .send(client.project(
                    AuthorityRecord::Tape(Box::new(snapshot)),
                    ApplyPath::Rebuild,
                ))
                .unwrap();
        });
        let mut worst = Duration::ZERO;
        let mut reads = 0;
        let response = loop {
            let start = Instant::now();
            reader.get_tape(&[0x74; 16]).unwrap();
            worst = worst.max(start.elapsed());
            reads += 1;
            match done_rx.try_recv() {
                Ok(result) => break result,
                Err(mpsc::TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(1)),
                Err(error) => panic!("worker disappeared: {error}"),
            }
        };
        worker.join().unwrap();
        assert!(matches!(response.unwrap(), ProjectionResult::Applied(_)));
        eprintln!("B4b1 R4: 100000 members, chunk={} rows, {reads} concurrent reads; worst={worst:?}; bound=1000ms",super::super::apply::TAPE_CHUNK_ROWS);
        assert!(reads > 1);
        assert!(worst < Duration::from_secs(1), "reader stalled: {worst:?}");
        assert_eq!(
            reader
                .list_native_object_files(&Uuid::from_bytes([0x51; 16]).to_string())
                .unwrap()
                .len(),
            100_000
        );
    }
}
