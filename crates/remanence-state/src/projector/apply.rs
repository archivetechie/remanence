//! Disconnected library projection entry point. Only fold-issued applications
//! reach this module. Source identity/digest and decision consumption commit in
//! the same transaction as the existing typed catalog projection cores.
use super::*;
use crate::library_log::{self as lib, authority::AppliedLibraryRecord};
use sha2::{Digest, Sha256};

/// Maximum native payload rows committed per tape projection chunk.
pub const TAPE_CHUNK_ROWS: usize = 512;

pub(super) const SCHEMA: &str = "
create table library_projection_identity (singleton integer primary key check(singleton=1), log_uuid blob not null);
create table library_projection_records (
 sequence blob primary key check(length(sequence)=8),
 record_uuid blob not null unique check(length(record_uuid)=16),
 source_digest blob not null check(length(source_digest)=32),
 tag integer not null, source_bytes blob not null);
create table library_projection_decisions (
 record_uuid blob primary key check(length(record_uuid)=16),
 sequence blob not null unique, source_digest blob not null);
create table library_projection_progress (singleton integer primary key check(singleton=1), received blob not null, applied blob not null, records integer not null);
create table library_fold_progress (singleton integer primary key check(singleton=1), received blob not null, non_applications integer not null, pending integer not null);
create table projection_sources (
 source text primary key, received blob not null, applied blob not null, pending integer not null);
create table library_wrap_bytes (tape_uuid blob primary key, descriptors blob not null);
create table library_health_keys (
 session_id blob not null, trigger integer not null, body_digest blob not null,
 primary key(session_id,trigger));
create table library_tape_status (
 tape_uuid blob primary key, incarnation blob not null, contradicted integer not null default 0,
 media_revision blob not null, medium_spec blob not null);
create table library_drive_endpoints (endpoint text primary key, drive_uuid blob not null unique);
create table library_drive_fences (sequence blob primary key, endpoint text not null, released integer not null default 0);
create table library_cleaning_runs (run_id text primary key, begin_sequence blob not null, body blob not null, cartridge blob, home_slot integer);
create table library_cleaning_homes (tape_uuid blob primary key, home_slot integer not null);
create table library_configuration (singleton integer primary key check(singleton=1), body blob not null);
create table library_motion (sequence blob primary key, operation blob, endpoint text, state text not null, body blob not null);
create table library_inventory (library_serial text primary key, generation blob not null, body blob not null);
create table library_recovery (tape_uuid blob primary key, retained_through blob not null, source_digest blob not null);
create table tape_projection_sources (tape_uuid blob primary key, received_sequence blob not null, byte_end blob not null, source_digest blob not null);
create table tape_projection_chunks (
 tape_uuid blob primary key, through_sequence blob not null, rows_committed blob not null);
";

/// Durable input to the single projection entry point.
#[derive(Clone)]
pub enum AuthorityRecord {
    /// Private proof constructed only after the library fold applies the record.
    Library(Box<AppliedLibraryRecord>),
    /// Frozen native tape authority; never a mutable filesystem path.
    Tape(Box<crate::tape_log::authority::AuthoritySnapshot>),
}
/// Invocation attribution for tests; every path calls the same implementation.
#[derive(Clone, Copy, Debug)]
pub enum ApplyPath {
    /// A durable live append.
    Live,
    /// Startup catch-up.
    Startup,
    /// A catalog created from empty SQL.
    Rebuild,
}
/// Projection outcome and source progress.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Watermarks {
    /// Highest received source sequence.
    pub received: u64,
    /// Highest contiguous projected sequence; a gap is never silently skipped.
    pub applied: u64,
    /// Number of positions actually projected, independent of the contiguous prefix.
    pub applied_records: u64,
    /// Fold decisions that produced no application (duplicates and exclusions).
    pub non_applications: u64,
    /// Number of actual fold pending records (native sources use remaining positions).
    pub pending: u64,
}
/// Apply distinguishes immutable contradictions from ordinary SQL failures.
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// Catalog I/O or typed-core failure. Authority is untouched.
    #[error(transparent)]
    Catalog(#[from] StateError),
    /// The same durable key arrived with different bytes.
    #[error("projection contradiction: {0}")]
    Contradiction(String),
    /// Lifetime session/trigger uniqueness is stricter than the fold's retained window.
    #[error("conflicting health body at library sequence {sequence}")]
    HealthConflict {
        /// Conflicting later record, whose SQL effect is refused.
        sequence: u64,
    },
}

/// Bind both the validated hash-chain position and canonical decoded source bytes.
/// Rebuild completeness and projection must agree on precisely this source identity.
pub(crate) fn library_source_digest(
    sequence: u64,
    record_hash: lib::Hash,
    record: &lib::Record,
) -> Result<[u8; 32], StateError> {
    let (encoded, _) = lib::codec::encode(sequence, [0; 32], record)
        .map_err(|e| StateError::IndexCorrupt(e.to_string()))?;
    let mut source_hash = Sha256::new();
    source_hash.update(record_hash);
    source_hash.update(&encoded);
    Ok(source_hash.finalize().into())
}

/// Catalog witnesses retained until a rebuild has proved its sources complete.
pub(crate) struct StoredLibraryRecord {
    pub sequence: u64,
    pub digest: [u8; 32],
    pub tag: u16,
    pub bytes: Vec<u8>,
}
/// All stored applications and the greatest received position, including unapplied records.
pub(crate) struct LibraryCatalogEvidence {
    pub rows: Vec<StoredLibraryRecord>,
    pub received: u64,
}
impl CatalogCore {
    /// Read only: no replay, projection progress update, or transaction is started here.
    pub(in crate::projector) fn library_catalog_evidence(
        &self,
    ) -> Result<LibraryCatalogEvidence, StateError> {
        let mut statement = self.conn.prepare(
            "select sequence,source_digest,tag,source_bytes from library_projection_records order by sequence"
        ).map_err(|e| sqlite_error("read library rebuild witnesses", e))?;
        let rows = statement
            .query_map([], |r| {
                Ok(StoredLibraryRecord {
                    sequence: r.get::<_, SqliteU64>(0)?.get(),
                    digest: r.get(1)?,
                    tag: r.get(2)?,
                    bytes: r.get(3)?,
                })
            })
            .map_err(|e| sqlite_error("read library rebuild witnesses", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| sqlite_error("read library rebuild witness", e))?;
        let received = self.library_catalog_received()?;
        Ok(LibraryCatalogEvidence { rows, received })
    }

    /// Greatest stored or received library sequence, without decoding record bytes.
    pub(in crate::projector) fn library_catalog_end(&self) -> Result<u64, StateError> {
        let stored = self
            .conn
            .query_row(
                "select sequence from library_projection_records order by sequence desc limit 1",
                [],
                |row| row.get::<_, SqliteU64>(0),
            )
            .optional()
            .map_err(|e| sqlite_error("read library catalog end", e))?
            .map_or(0, SqliteU64::get);
        Ok(stored.max(self.library_catalog_received()?))
    }

    /// Share all three received cursors between open admission and rebuild evidence.
    fn library_catalog_received(&self) -> Result<u64, StateError> {
        let mut received = 0;
        for sql in [
            "select received from projection_sources where source='library'",
            "select received from library_projection_progress where singleton=1",
            "select received from library_fold_progress where singleton=1",
        ] {
            let position = self
                .conn
                .query_row(sql, [], |r| r.get::<_, SqliteU64>(0))
                .optional()
                .map_err(|e| sqlite_error("read library rebuild received position", e))?;
            received = received.max(position.map_or(0, SqliteU64::get));
        }
        Ok(received)
    }
}

/// Roll back on both error and unwind; inner typed methods use nested savepoints.
struct RecordTransaction<'a> {
    core: &'a mut CatalogCore,
    committed: bool,
}
impl Drop for RecordTransaction<'_> {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self
                .core
                .conn
                .execute_batch("rollback to authority_record; release authority_record");
        }
    }
}
impl RecordTransaction<'_> {
    fn commit(mut self) -> Result<(), StateError> {
        self.core
            .conn
            .execute_batch("release authority_record")
            .map_err(|e| sqlite_error("commit authority projection", e))?;
        self.committed = true;
        Ok(())
    }
}

impl CatalogCore {
    pub(in crate::projector) fn apply_authority(
        &mut self,
        source: &AuthorityRecord,
        _path: ApplyPath,
    ) -> Result<Watermarks, ApplyError> {
        #[cfg(any(test, feature = "fixtures"))]
        {
            self.apply_counts[match _path {
                ApplyPath::Live => 0,
                ApplyPath::Startup => 1,
                ApplyPath::Rebuild => 2,
            }] += 1;
        }
        match source {
            AuthorityRecord::Tape(snapshot) => {
                self.project_tape_log_bounded(snapshot)?;
                let sequence = snapshot.log_reference().0;
                Ok(Watermarks {
                    received: sequence,
                    applied: sequence,
                    applied_records: sequence,
                    non_applications: 0,
                    pending: 0,
                })
            }
            AuthorityRecord::Library(value) => {
                self.conn
                    .execute_batch("savepoint authority_record")
                    .map_err(|e| sqlite_error("begin authority projection", e))?;
                let guard = RecordTransaction {
                    core: self,
                    committed: false,
                };
                let identity: Option<Vec<u8>> = guard
                    .core
                    .conn
                    .query_row(
                        "select log_uuid from library_projection_identity where singleton=1",
                        [],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(|e| sqlite_error("read library source identity", e))?;
                if let Some(identity) = identity {
                    if identity != value.log_uuid {
                        return Err(ApplyError::Contradiction(
                            "library source UUID changed: the catalog was built from another library log; stop the service and run rem rebuild-catalog-from-journals".into(),
                        ));
                    }
                } else {
                    guard
                        .core
                        .conn
                        .execute(
                            "insert into library_projection_identity values(1,?1)",
                            [value.log_uuid.as_slice()],
                        )
                        .map_err(|e| sqlite_error("bind library source identity", e))?;
                }
                let (encoded, _) = lib::codec::encode(value.sequence, [0; 32], &value.record)
                    .map_err(|e| StateError::IndexCorrupt(e.to_string()))?;
                let digest =
                    library_source_digest(value.sequence, value.record_hash, &value.record)?;
                let previous: Option<Vec<u8>> = guard.core.conn.query_row(
                    "select source_digest from library_projection_records where sequence=?1 or record_uuid=?2",
                    params![SqliteU64::new(value.sequence), value.record.envelope.record_uuid.as_slice()], |r|r.get(0))
                    .optional().map_err(|e| sqlite_error("read library source digest", e))?;
                let inserted = previous.is_none();
                if let Some(previous) = previous {
                    if previous != digest {
                        return Err(ApplyError::Contradiction(format!(
                            "library sequence {}: the catalog holds a different record for this sequence or record identity than the library log; stop the service and run rem rebuild-catalog-from-journals",
                            value.sequence
                        )));
                    }
                    // A duplicate is a no-op even after prefix completion has
                    // recorded exclusions or dependencies beyond this record.
                    let marks = guard.core.committed_watermarks(source)?;
                    guard.commit()?;
                    return Ok(marks);
                } else {
                    guard.core.apply_library_body(value)?;
                    guard
                        .core
                        .conn
                        .execute(
                            "insert into library_projection_records values(?1,?2,?3,?4,?5)",
                            params![
                                SqliteU64::new(value.sequence),
                                value.record.envelope.record_uuid.as_slice(),
                                digest.as_slice(),
                                value.record.tag() as u16,
                                encoded
                            ],
                        )
                        .map_err(|e| sqlite_error("record library application", e))?;
                    guard
                        .core
                        .conn
                        .execute(
                            "insert into library_projection_decisions values(?1,?2,?3)",
                            params![
                                value.record.envelope.record_uuid.as_slice(),
                                SqliteU64::new(value.sequence),
                                digest.as_slice()
                            ],
                        )
                        .map_err(|e| sqlite_error("consume library decision", e))?;
                }
                record_fold_progress(&guard.core.conn, value.progress)?;
                let marks = library_watermarks(&guard.core.conn, value.sequence, inserted)?;
                guard.core.conn.execute("insert into projection_sources values('library',?1,?2,?3) on conflict(source) do update set received=excluded.received,applied=excluded.applied,pending=excluded.pending",
                    params![SqliteU64::new(marks.received),SqliteU64::new(marks.applied),marks.pending])
                    .map_err(|e| sqlite_error("record library watermarks", e))?;
                guard.commit()?;
                Ok(marks)
            }
        }
    }

    fn apply_library_body(&mut self, value: &AppliedLibraryRecord) -> Result<(), ApplyError> {
        let record = &value.record;
        let at = OffsetDateTime::from_unix_timestamp_nanos(i128::from(
            record.envelope.timestamp_unix_ns,
        ))
        .map_err(|e| StateError::IndexCorrupt(e.to_string()))?
        .format(&Rfc3339)
        .map_err(|e| StateError::IndexCorrupt(e.to_string()))?;
        let actor = actor_fingerprint(&record.envelope.actor);
        use lib::BodyView as Body;
        match record.body_view() {
            Body::Diagnostic(_)
            | Body::AcceptUnprotectedFinalization(_)
            | Body::VerificationProgress(_) => (),
            Body::AssignPool(v) => {
                project_assignment_row(
                    &self.conn,
                    v.tape.uuid,
                    v.change.expected,
                    v.change.next,
                    v.pool.as_ref().map(|x| x.0.as_str()),
                    &at,
                    false,
                )?;
                project_assignment_history(
                    &self.conn,
                    v.tape.uuid,
                    v.change.next,
                    v.pool.as_ref().map(|x| x.0.as_str()),
                    if v.source == lib::AssignmentSource::Operator {
                        "operator"
                    } else {
                        "rule"
                    },
                    &actor,
                    &at,
                )?;
            }
            Body::Retire(v) => {
                self.retire_tape(RetireTapeInput {
                    tape_uuid: v.tape.uuid,
                    reason: format!("{:?}", v.reason),
                })?;
                // A retired physical identity cannot serve its cached wrap map.
                // The control store also advances its eviction generation.
                self.delete_wrap_map(&v.tape.uuid)?;
                project_assignment_row(
                    &self.conn,
                    v.tape.uuid,
                    v.assignment_generation,
                    v.assignment_generation,
                    None,
                    &at,
                    false,
                )?;
            }
            Body::AlarmChanged(v) => match &v.action {
                lib::AlarmAction::Raise(a) => raise_alarm_tx(
                    &self.conn,
                    &v.condition_key.0,
                    &spelling(a.kind).replace('_', "-"),
                    &spelling(a.severity),
                    Some(&a.reason.0),
                    &at,
                )?,
                lib::AlarmAction::Refresh(a) => {
                    // UPDATE preserves state, acknowledgement time and actor.
                    self.conn.execute("update alarms set severity=?2,detail=?3,last_seen_utc=?4 where condition_key=?1", params![v.condition_key.0, spelling(a.severity), a.reason.0, at])
                        .map_err(|e| sqlite_error("refresh raised alarm", e))?;
                }
                lib::AlarmAction::Acknowledge(_) => {
                    project_alarm_state_core(&self.conn, &v.condition_key.0, "acked", &at, &actor)?;
                }
                lib::AlarmAction::Clear(_) => {
                    project_alarm_state_core(
                        &self.conn,
                        &v.condition_key.0,
                        "cleared",
                        &at,
                        &actor,
                    )?;
                }
            },
            Body::DriveDecision(v) => match &v.decision {
                lib::Stewardship::Retire(reason) => {
                    project_drive_lifecycle_core(
                        &self.conn,
                        &v.drive_uuid,
                        DriveLifecycleProjection::Retired(Some(reason.reason.0.trim().to_string())),
                        Some(&at),
                    )?;
                }
                lib::Stewardship::Reinstate(reason) => {
                    self.reinstate_drive(&v.drive_uuid, &reason.reason.0)?;
                }
                lib::Stewardship::Annotate(a) => {
                    project_drive_lifecycle_core(
                        &self.conn,
                        &v.drive_uuid,
                        DriveLifecycleProjection::Annotated {
                            purchase_date: text(&a.purchase_date),
                            warranty_until: text(&a.warranty_until),
                            cost: text(&a.cost),
                            notes: text(&a.notes),
                        },
                        Some(&at),
                    )?;
                }
            },
            Body::OperationRecorded(v) => {
                let (state, subject, error) = match &v.event {
                    lib::OperationEvent::Accepted(a) => ("queued", text(&a.subject), None),
                    lib::OperationEvent::Started(_) => ("running", None, None),
                    lib::OperationEvent::CancelRequested(_) => ("cancel_requested", None, None),
                    lib::OperationEvent::CancellationRejected(_) => ("running", None, None),
                    lib::OperationEvent::CancelledBeforeDispatch(_) => {
                        ("cancelled_before_dispatch", None, None)
                    }
                    lib::OperationEvent::CompletedAfterCancel(_) => {
                        ("completed_after_cancel", None, None)
                    }
                    lib::OperationEvent::Finished(_) => ("finished", None, None),
                    lib::OperationEvent::Failed(f) => ("failed", None, Some(f.summary.0.clone())),
                    lib::OperationEvent::CompletionUnknown(_) => ("completion_unknown", None, None),
                };
                project_operation_core(
                    &self.conn,
                    Uuid::from_bytes(v.operation),
                    &spelling(v.kind),
                    state,
                    None,
                    subject,
                    error,
                    &at,
                )?;
                if let lib::OperationEvent::Accepted(a) = &v.event {
                    if let Some(key) = a.idempotency_key {
                        upsert_idempotency_request(
                            &self.conn,
                            IdempotencyRequestProjection {
                                actor_fingerprint: &a.actor_fingerprint.0,
                                operation_kind: &spelling(v.kind),
                                idempotency_key: Uuid::from_bytes(key),
                                request_fingerprint: a.request_fingerprint.to_vec(),
                                operation_id: Uuid::from_bytes(v.operation),
                                updated_at_utc: &at,
                            },
                            IdempotencyProjectionMode::Live,
                        )?;
                    }
                }
                let response = match &v.event {
                    lib::OperationEvent::Finished(r)
                    | lib::OperationEvent::CompletedAfterCancel(r) => {
                        r.response_fingerprint.map(|h| h.to_vec())
                    }
                    _ => None,
                };
                if matches!(
                    v.event,
                    lib::OperationEvent::Finished(_)
                        | lib::OperationEvent::Failed(_)
                        | lib::OperationEvent::CompletedAfterCancel(_)
                        | lib::OperationEvent::CancelledBeforeDispatch(_)
                        | lib::OperationEvent::CompletionUnknown(_)
                ) {
                    let scopes = {
                        let mut stmt=self.conn.prepare("select actor_fingerprint,operation_kind,idempotency_key from idempotency_keys where operation_id=?1").map_err(|e|sqlite_error("read operation idempotency scopes",e))?;
                        let rows = stmt
                            .query_map([Uuid::from_bytes(v.operation).to_string()], |r| {
                                Ok((
                                    r.get::<_, String>(0)?,
                                    r.get::<_, String>(1)?,
                                    r.get::<_, String>(2)?,
                                ))
                            })
                            .map_err(|e| sqlite_error("query operation idempotency scopes", e))?;
                        rows.collect::<Result<Vec<_>, _>>()
                            .map_err(|e| sqlite_error("read operation idempotency scope", e))?
                    };
                    for (actor, kind, key) in scopes {
                        project_idempotency_terminal_core(
                            &self.conn,
                            state,
                            response.clone(),
                            &at,
                            &actor,
                            &kind,
                            Uuid::parse_str(&key)
                                .map_err(|e| StateError::IndexCorrupt(e.to_string()))?,
                            Uuid::from_bytes(v.operation),
                        )?;
                    }
                }
            }

            Body::SupersedeTail(_) => (), // Native SessionEnded consumes this decision.
            Body::Reprovision(v) => {
                let geometry = ProvisionTapeGeometry {
                    block_size: i64::from(v.medium.record_size),
                    scheme_id: v.parity_scheme.as_ref().map(|p| p.id.0.clone()),
                    data_blocks_per_stripe: v
                        .parity_scheme
                        .as_ref()
                        .map(|p| i64::from(p.data_blocks_per_stripe)),
                    parity_blocks_per_stripe: v
                        .parity_scheme
                        .as_ref()
                        .map(|p| i64::from(p.parity_blocks_per_stripe)),
                    stripes_per_neighborhood: v
                        .parity_scheme
                        .as_ref()
                        .map(|p| i64::from(p.stripes_per_neighborhood)),
                };
                if self.get_tape(&v.tape.uuid)?.is_none() {
                    insert_provisioned_tape_tx(
                        &self.conn,
                        v.tape.uuid,
                        (
                            v.label.as_ref().map_or("", |x| &x.0),
                            label_source(v.label_provenance),
                        ),
                        &geometry,
                        &at,
                    )?;
                }
                project_label_core(
                    &self.conn,
                    v.tape.uuid,
                    None,
                    v.label.as_ref().map(|l| l.0.as_str()),
                    v.label_provenance.map(|p| label_source(Some(p)).as_str()),
                    &at,
                )?;
                if let Some(pool) = &v.pool {
                    if self.get_tape_pool(&pool.0)?.is_none() {
                        upsert_tape_pool_projection_tx(&self.conn, &pool.0, None, None, None, &at)?;
                    }
                }
                project_assignment_row(
                    &self.conn,
                    v.tape.uuid,
                    0,
                    1,
                    v.pool.as_ref().map(|p| p.0.as_str()),
                    &at,
                    false,
                )?;
                if let Some(source) = crate::state::adoption_assignment_source(v) {
                    project_assignment_history(
                        &self.conn,
                        v.tape.uuid,
                        1,
                        v.pool.as_ref().map(|p| p.0.as_str()),
                        source,
                        &actor,
                        &at,
                    )?;
                }
                self.project_library_medium(
                    v.tape,
                    &v.medium,
                    1,
                    &record
                        .canonical_body()
                        .map_err(|e| StateError::IndexCorrupt(e.to_string()))?,
                    &at,
                )?;
                if let Some(adoption) = &v.adoption {
                    project_adopted_state_core(
                        &self.conn,
                        &v.tape.uuid,
                        match adoption.state {
                            lib::AdoptedState::Ready => "ready",
                            lib::AdoptedState::RecoveryRequired => "recovery_required",
                        },
                        true,
                    )?;
                    upsert_idempotency_request(
                        &self.conn,
                        IdempotencyRequestProjection {
                            actor_fingerprint: &actor,
                            operation_kind: "unknown",
                            idempotency_key: Uuid::from_bytes(adoption.idempotency_key),
                            request_fingerprint: adoption.request_fingerprint.to_vec(),
                            operation_id: Uuid::from_bytes(adoption.operation),
                            updated_at_utc: &at,
                        },
                        IdempotencyProjectionMode::Live,
                    )?;
                }
            }
            Body::Label(v) => project_label_core(
                &self.conn,
                v.tape.uuid,
                v.prior_holder.as_ref().map(|p| p.tape.uuid),
                v.label.as_ref().map(|l| l.0.as_str()),
                Some(label_source(Some(v.provenance)).as_str()),
                &at,
            )?,
            Body::MediumSpecSet(v) => self.project_library_medium(
                v.tape,
                &v.medium,
                v.change.next,
                &record
                    .canonical_body()
                    .map_err(|e| StateError::IndexCorrupt(e.to_string()))?,
                &at,
            )?,
            Body::VerificationVerdict(v) => project_verification_core(
                &self.conn,
                &v.tape.uuid,
                &at,
                // Ruling 34: the outcome is the state; damage and the unprotected tail are
                // evidence about it and no longer stand in for it.
                match v.outcome {
                    lib::VerificationOutcome::VerifiedComplete => 2,
                    lib::VerificationOutcome::VerifiedDegraded => 3,
                    lib::VerificationOutcome::RecoveryRequired => 4,
                },
                v.edition_digest.as_ref().map(|d| d.as_slice()),
            )?,
            Body::ManualFinalizationAccepted(v) => {
                upsert_idempotency_request(
                    &self.conn,
                    IdempotencyRequestProjection {
                        actor_fingerprint: &v.actor_fingerprint.0,
                        operation_kind: "finalize_tape",
                        idempotency_key: Uuid::from_bytes(v.idempotency_key),
                        request_fingerprint: v.request_fingerprint.to_vec(),
                        operation_id: Uuid::from_bytes(v.operation),
                        updated_at_utc: &at,
                    },
                    IdempotencyProjectionMode::Live,
                )?;
            }
            Body::ConfigurationAccepted(v) => {
                self.conn.execute("insert into library_configuration values(1,?1) on conflict(singleton) do update set body=excluded.body",[record.canonical_body().map_err(|e|StateError::IndexCorrupt(e.to_string()))?]).map_err(|e|sqlite_error("project accepted configuration",e))?;
                for pool in &v.definitions.pools.0 {
                    upsert_tape_pool_projection_tx(
                        &self.conn,
                        &pool.id.0,
                        pool.display_name.as_ref().map(|x| x.0.as_str()),
                        pool.copy_class.as_ref().map(|x| x.0.as_str()),
                        pool.content_class.as_ref().map(|x| x.0.as_str()),
                        &at,
                    )?;
                }
                let configured = v
                    .definitions
                    .pools
                    .0
                    .iter()
                    .map(|pool| &pool.id.0)
                    .collect::<HashSet<_>>();
                let existing = {
                    let mut statement = self
                        .conn
                        .prepare("select pool_id from tape_pools")
                        .map_err(|e| sqlite_error("read configured tape pools", e))?;
                    let pools = statement
                        .query_map([], |row| row.get::<_, String>(0))
                        .map_err(|e| sqlite_error("query configured tape pools", e))?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|e| sqlite_error("decode configured tape pools", e))?;
                    pools
                };
                for pool_id in existing {
                    if !configured.contains(&pool_id) {
                        self.conn
                            .execute("delete from tape_pools where pool_id = ?1", [&pool_id])
                            .map_err(|e| sqlite_error("remove unconfigured tape pool", e))?;
                    }
                }
            }
            Body::DriveObserved(v) => {
                let endpoint = v.endpoint.as_ref().map(endpoint);
                observe_drive_core(
                    &self.conn,
                    DriveObservationInput {
                        serial: v.serial.as_ref().map_or_else(String::new, |x| x.0.clone()),
                        identity_source: format!("{:?}", v.identity_source),
                        vendor: text(&v.vendor),
                        product: text(&v.product),
                        firmware_rev: text(&v.firmware_rev),
                        managed: spelling(v.managed),
                        library_serial: endpoint
                            .as_ref()
                            .and_then(|e| e.library_serial().map(str::to_owned)),
                        element_address: endpoint.as_ref().and_then(|e| e.bay().map(i64::from)),
                        observed_at_utc: Some(at),
                    },
                    false,
                    Some(v.drive_uuid),
                )?;
                self.conn
                    .execute(
                        "delete from library_drive_endpoints where drive_uuid=?1",
                        [v.drive_uuid.as_slice()],
                    )
                    .map_err(|e| sqlite_error("move observed drive endpoint", e))?;
                if let Some(endpoint) = endpoint {
                    self.conn.execute("insert into library_drive_endpoints values(?1,?2) on conflict(endpoint) do update set drive_uuid=excluded.drive_uuid",params![endpoint.encode(),v.drive_uuid.as_slice()]).map_err(|e|sqlite_error("project observed drive endpoint",e))?;
                    self.project_library_drive_fences(&endpoint)?;
                }
            }
            Body::DriveHealthObserved(v) => {
                let body = record
                    .canonical_body()
                    .map_err(|e| StateError::IndexCorrupt(e.to_string()))?;
                let digest = Sha256::digest(&body).to_vec();
                if let Some(session) = v.session_id {
                    let old:Option<Vec<u8>>=self.conn.query_row("select body_digest from library_health_keys where session_id=?1 and trigger=?2",params![session.as_slice(),v.trigger as u16],|r|r.get(0)).optional().map_err(|e|sqlite_error("read lifetime health key",e))?;
                    if let Some(old) = old {
                        if old != digest {
                            return Err(ApplyError::HealthConflict {
                                sequence: value.sequence,
                            });
                        }
                        return Ok(());
                    }
                    self.conn
                        .execute(
                            "insert into library_health_keys values(?1,?2,?3)",
                            params![session.as_slice(), v.trigger as u16, digest],
                        )
                        .map_err(|e| sqlite_error("consume lifetime health key", e))?;
                }
                // Match today's signed SQL projection. The full unsigned value
                // remains in the canonical source even when SQL cannot hold it.
                let counter = |v: Option<u64>| v.and_then(|n| i64::try_from(n).ok());
                let mut divergences = Vec::new();
                project_drive_health_core(
                    &self.conn,
                    DriveHealthSnapshotInput {
                        drive_uuid: v.drive_uuid.to_vec(),
                        trigger: spelling(v.trigger).replace('_', "-"),
                        session_id: v.session_id.map(|s| Uuid::from_bytes(s).to_string()),
                        tape_alert_flags: Some(tape_alert_json(v.tape_alert_flags)),
                        write_errors_corrected: counter(v.counters.write_corrected),
                        write_errors_uncorrected: counter(v.counters.write_uncorrected),
                        read_errors_corrected: counter(v.counters.read_corrected),
                        read_errors_uncorrected: counter(v.counters.read_uncorrected),
                        raw_pages: v.raw_pages.as_ref().map(|b| health_raw_pages(&b.0)),
                        at_utc: Some(at.clone()),
                    },
                    None,
                    false,
                    &mut divergences,
                )?;
                // TapeAlert cleaning requests and last-seen are projections of
                // this exact sampled source, never supplemental owner SQL.
                let due = if v.tape_alert_flags & (1 << 19) != 0 {
                    Some("now")
                } else if v.tape_alert_flags & (1 << 20) != 0 {
                    Some("periodic")
                } else {
                    None
                };
                self.conn.execute("update drives set cleaning_due=case when state='active' and managed='rem' and ?2 is not null then case when cleaning_due='now' or ?2='now' then 'now' else 'periodic' end else cleaning_due end,last_seen_utc=?3 where drive_uuid=?1 and state='active'",params![v.drive_uuid.as_slice(),due,at]).map_err(|e|sqlite_error("project sampled drive cleaning request",e))?;
                if !divergences.is_empty() {
                    return Err(ApplyError::HealthConflict {
                        sequence: value.sequence,
                    });
                }
            }
            Body::SessionObserved(v) => {
                let endpoint = v.endpoint.as_ref().map(endpoint);
                let drive_uuid = if v.previous.is_some() {
                    self.conn
                        .query_row(
                            "select drive_uuid from sessions where session_id=?1",
                            [Uuid::from_bytes(v.session_id).to_string()],
                            |row| row.get::<_, Option<Vec<u8>>>(0),
                        )
                        .optional()
                        .map_err(|e| sqlite_error("read derived session creation identity", e))?
                        .flatten()
                } else {
                    endpoint
                        .as_ref()
                        .map(|e| self.library_drive_at(e))
                        .transpose()?
                        .flatten()
                        .filter(|drive| drive.state == "active")
                        .map(|drive| drive.drive_uuid)
                };
                project_session_core(
                    &self.conn,
                    SessionProjection {
                        session_id: Uuid::from_bytes(v.session_id),
                        kind: &spelling(v.kind),
                        tape_uuid: v.tape.map(|t| t.uuid.to_vec()),
                        library_serial: endpoint
                            .as_ref()
                            .and_then(|e| e.library_serial().map(str::to_owned)),
                        drive_bay: endpoint.as_ref().and_then(|e| e.bay().map(i64::from)),
                        drive_uuid,
                        drive_serial: None,
                        state: match v.event {
                            lib::SessionEvent::Opened => "open",
                            lib::SessionEvent::Closed => "closed",
                            lib::SessionEvent::Orphaned => "orphaned",
                            lib::SessionEvent::LostByRestart => "lost_restart",
                        },
                        timestamp: &at,
                        endpoint: endpoint.map(|e| e.encode()),
                    },
                )?;
            }
            Body::WrapMapCached(v) => match &v.action {
                lib::WrapAction::Cache(cache) => {
                    // The frozen corpus includes opaque descriptor formats. Preserve all
                    // source bytes; only today's typed serialization is a usable cache.
                    self.conn.execute("insert into library_wrap_bytes values(?1,?2) on conflict(tape_uuid) do update set descriptors=excluded.descriptors",params![v.tape.uuid.as_slice(),cache.descriptors.0]).map_err(|e|sqlite_error("preserve opaque wrap descriptors",e))?;
                    if let Ok(descriptors) = serde_json::from_slice(&cache.descriptors.0) {
                        self.upsert_wrap_map(&WrapMapCacheRecord {
                            tape_uuid: v.tape.uuid,
                            descriptors,
                            mapped_extent_lba: cache.mapped_extent_lba,
                            write_epoch: cache.write_epoch,
                            calibration_generation: v.calibration_generation,
                            harvested_at_utc: at,
                        })?;
                    } else {
                        self.delete_wrap_map(&v.tape.uuid)?;
                    }
                }
                lib::WrapAction::Invalidate(_) => {
                    self.delete_wrap_map(&v.tape.uuid)?;
                    self.conn
                        .execute(
                            "delete from library_wrap_bytes where tape_uuid=?1",
                            [v.tape.uuid.as_slice()],
                        )
                        .map_err(|e| sqlite_error("invalidate opaque wrap descriptors", e))?;
                }
            },
            Body::CleaningRecorded(v) => {
                // An unbound run is still an applied fact. Keep its typed state
                // until a drive binding permits the existing clean_runs view.
                if !self.project_library_cleaning_binding(value, v, &at)? {
                    return Ok(());
                }
                match &v.action {
                    lib::CleaningAction::Register(a) => {
                        let tape = v.cartridge.ok_or_else(|| {
                            StateError::IndexCorrupt(
                                "cleaning registration has no cartridge".into(),
                            )
                        })?;
                        // Register creates a fresh inventory entity, including
                        // the collision with a retired data UUID. Use insertion
                        // defaults for every data-only field, not an inherited row.
                        self.conn
                            .execute(
                                "delete from tapes where tape_uuid=?1",
                                [tape.uuid.as_slice()],
                            )
                            .map_err(|e| {
                                sqlite_error(
                                    "replace retired data entity with cleaning inventory",
                                    e,
                                )
                            })?;
                        self.conn.execute(
                            "insert into tapes(tape_uuid,voltag,kind,cleaning_uses,cleaning_state,highest_protected_ordinal,total_committed_ordinals,last_committed_tape_file,state,updated_at_utc) values(?1,?2,'cleaning',0,'unverified',X'0000000000000000',X'0000000000000000',null,'ready',?3)",
                            params![tape.uuid.as_slice(), a.barcode.0, at],
                        ).map_err(|e| sqlite_error("project cleaning identity", e))?;
                        self.conn.execute("insert into library_cleaning_homes values(?1,?2) on conflict(tape_uuid) do update set home_slot=excluded.home_slot",params![tape.uuid.as_slice(),a.home_slot]).map_err(|e|sqlite_error("project cleaning home slot",e))?;
                        // Native data remains on disk as historical evidence;
                        // these caches describe the newly registered inventory.
                        for table in [
                            "tape_files",
                            "object_instances",
                            "catalog_units",
                            "wrap_maps",
                            "tape_log_object_commits",
                            "tape_log_projection",
                            "tape_projection_chunks",
                        ] {
                            self.conn
                                .execute(
                                    &format!("delete from {table} where tape_uuid=?1"),
                                    [tape.uuid.as_slice()],
                                )
                                .map_err(|e| {
                                    sqlite_error(
                                        "clear retired data projection after cleaning registration",
                                        e,
                                    )
                                })?;
                        }
                    }
                    lib::CleaningAction::Begin(_) => {
                        self.project_library_cleaning_selection(v)?;
                    }
                    lib::CleaningAction::Advance(a) => {
                        self.project_library_cleaning_selection(v)?;
                        self.advance_clean_run(
                            &v.run_id.0,
                            &spelling(a.phase).replace('_', "-"),
                            None,
                        )?;
                    }
                    lib::CleaningAction::Finish(a) => {
                        if a.verified_credit {
                            let run = self.get_clean_run(&v.run_id.0)?.ok_or_else(|| {
                                StateError::IndexCorrupt("clean finish has no run".into())
                            })?;
                            finalize_clean_run_core(
                                &self.conn,
                                &v.run_id.0,
                                &run.drive_uuid,
                                v.cartridge
                                    .as_ref()
                                    .map(|t| t.uuid.as_slice())
                                    .or(run.cart_tape_uuid.as_deref()),
                                None,
                                &at,
                                false,
                            )?;
                        } else {
                            self.terminalize_clean_run(
                                &v.run_id.0,
                                &spelling(a.result).replace('_', "-"),
                                None,
                            )?;
                        }
                    }
                    lib::CleaningAction::Reject(_) => {
                        if let Some(t) = v.cartridge {
                            self.set_tape_cleaning_state(&t.uuid, "rejected")?;
                        }
                    }
                    lib::CleaningAction::Expire(_) => {
                        if let Some(t) = v.cartridge {
                            self.set_tape_cleaning_state(&t.uuid, "expired")?;
                        }
                    }
                }
            }
            Body::ReadinessObserved(v) => {
                let drive_serial = if v.previous.is_some() {
                    self.media_readiness_operation(Uuid::from_bytes(v.operation))?
                        .and_then(|row| row.drive_serial)
                } else {
                    match &v.endpoint {
                        lib::DriveKey::Standalone(key) => {
                            Some(key.drive_serial.as_str().to_owned())
                        }
                        lib::DriveKey::Bay(_) => self
                            .library_drive_at(&endpoint(&v.endpoint))?
                            .filter(|drive| drive.state == "active")
                            .and_then(|drive| drive.serial),
                    }
                };
                self.record_media_readiness_operation(
                    None,
                    MediaReadinessOperationInput {
                        operation_id: Uuid::from_bytes(v.operation),
                        run_id: v.run.map(|run| Uuid::from_bytes(run).to_string()),
                        endpoint: endpoint(&v.endpoint),
                        changer_sg: None,
                        drive_sg: None,
                        drive_serial,
                        barcode: v.barcode.as_ref().map(|x| x.0.clone()),
                        source_slot: v.source_slot,
                        media_generation: match v.tape {
                            Some(t) => self.get_tape(&t.uuid)?.and_then(|t| {
                                crate::tape_generation(&t).map(|(g, _)| g.generation_number())
                            }),
                            None => match v.media {
                                Some(lib::MediumFamily::Lto(g)) => Some(g.generation_number()),
                                _ => None,
                            },
                        },
                        media_family: match v.tape {
                            Some(t) => self
                                .get_tape(&t.uuid)?
                                .and_then(|t| crate::tape_generation(&t).map(|(g, _)| g)),
                            None => match v.media {
                                Some(lib::MediumFamily::Lto(g)) => Some(g),
                                _ => None,
                            },
                        },
                        phase: spelling(v.phase),
                        state: spelling(v.state),
                        dirty_scope: if v.state == lib::ReadinessState::Ready {
                            None
                        } else {
                            Some(
                                match v.scope {
                                    lib::ReadinessScope::Endpoint => "drive+tape",
                                    lib::ReadinessScope::Library => "selected-library-snapshot",
                                }
                                .into(),
                            )
                        },
                        deadline_at_utc: v
                            .limit_ms
                            .map(|limit_ms| {
                                OffsetDateTime::from_unix_timestamp_nanos(
                                    i128::from(v.started_unix_ns)
                                        + i128::from(limit_ms) * 1_000_000,
                                )
                                .map_err(|e| StateError::IndexCorrupt(e.to_string()))?
                                .format(&Rfc3339)
                                .map_err(|e| StateError::IndexCorrupt(e.to_string()))
                            })
                            .transpose()?,
                        evidence_path: None,
                    },
                )?;
                self.conn.execute(
                    "update media_readiness_ops set started_at_utc = ?2 where operation_id = ?1",
                    params![Uuid::from_bytes(v.operation).to_string(),
                        OffsetDateTime::from_unix_timestamp_nanos(i128::from(v.started_unix_ns))
                            .map_err(|e| StateError::IndexCorrupt(e.to_string()))?
                            .format(&Rfc3339).map_err(|e| StateError::IndexCorrupt(e.to_string()))?],
                ).map_err(|e| sqlite_error("project readiness source start", e))?;
                self.library_readiness_transition(
                    v.operation,
                    &spelling(v.state),
                    v.evidence
                        .as_ref()
                        .and_then(|_| crate::library_runtime::readiness::phase_opcode(v.phase)),
                    v.evidence.as_ref(),
                    v.cancel_source.map(spelling),
                )?;
            }
            Body::FenceRaised(v) => match &v.scope {
                lib::FenceScope::Tape(tape) => {
                    let text_detail = |key: &str| {
                        if v.cause == lib::FenceCause::NativeLogBehindProjection {
                            return None;
                        }
                        v.detail.0.iter().find_map(|entry| match &entry.1 {
                            lib::DiagnosticValue::Text(text) if entry.0.as_str() == key => {
                                Some(text.0.clone())
                            }
                            _ => None,
                        })
                    };
                    project_fence_core(
                        &self.conn,
                        TapeIoFenceInput {
                            tape_uuid: tape.uuid,
                            barcode: text_detail("barcode"),
                            reason: spelling(v.cause),
                            evidence_json: text_detail("evidence_json"),
                        },
                        &format!("library:{}", value.sequence),
                        &at,
                        None,
                    )?;
                }
                lib::FenceScope::Drive(key) => {
                    let key = endpoint(key);
                    self.conn
                        .execute(
                            "insert into library_drive_fences(sequence,endpoint) values(?1,?2)",
                            params![SqliteU64::new(value.sequence), key.encode()],
                        )
                        .map_err(|e| sqlite_error("project drive fence source", e))?;
                    self.project_library_drive_fences(&key)?;
                }
                lib::FenceScope::Model { .. } => {
                    return Err(ApplyError::Contradiction("reserved model fence".into()))
                }
            },
            Body::FenceReleased(v) => {
                let raised = source_record(&self.conn, v.fence)?;
                if let lib::BodyView::FenceRaised(fence) = raised.body_view() {
                    match &fence.scope {
                        lib::FenceScope::Tape(_) => {
                            self.complete_tape_io_fence_release(
                                &format!("library:{}", v.fence),
                                &v.evidence.0,
                                &at,
                                Some(v.forced),
                            )?;
                        }
                        lib::FenceScope::Drive(key) => {
                            self.conn
                                .execute(
                                    "update library_drive_fences set released=1 where sequence=?1",
                                    [SqliteU64::new(v.fence)],
                                )
                                .map_err(|e| sqlite_error("release drive fence source", e))?;
                            self.project_library_drive_fences(&endpoint(key))?;
                        }
                        lib::FenceScope::Model { .. } => {
                            return Err(ApplyError::Contradiction("reserved model fence".into()))
                        }
                    }
                }
            }
            Body::MotionIntent(v) => {
                let key = match &v.motion {
                    lib::Motion::Load(a) => Some(endpoint(&a.endpoint)),
                    lib::Motion::Unload(a) => Some(endpoint(&a.endpoint)),
                    lib::Motion::Move(_) => None,
                };
                self.conn
                    .execute(
                        "insert into library_motion values(?1,?2,?3,'pending',?4)",
                        params![
                            SqliteU64::new(value.sequence),
                            v.parent_operation.map(|id| id.to_vec()),
                            key.as_ref().map(DriveKey::encode),
                            record
                                .canonical_body()
                                .map_err(|e| StateError::IndexCorrupt(e.to_string()))?
                        ],
                    )
                    .map_err(|e| sqlite_error("project motion intent", e))?;
                if matches!(v.motion, lib::Motion::Load(_)) {
                    // A LOAD is not a readiness operation. Only a row that a `ReadinessObserved`
                    // created (this arm is the one creator in a log-driven projection), whose
                    // head is open and not a fence, takes the LOAD's write-ahead transition. Any
                    // other LOAD, with no parent or a parent no readiness record names, projects
                    // into the motion table alone. The motion fold tracks its uncertainty.
                    let operation = v.parent_operation.unwrap_or(v.command);
                    if self
                        .media_readiness_operation(Uuid::from_bytes(operation))?
                        .is_some_and(|row| {
                            crate::library_runtime::readiness::load_may_advance_head(&row.state)
                        })
                    {
                        self.library_readiness_transition(
                            operation,
                            "pre_ready_loading",
                            Some(0x1b),
                            None,
                            None,
                        )?;
                    }
                }
            }
            Body::MotionOutcome(v) => {
                self.conn
                    .execute(
                        "update library_motion set state=?2 where sequence=?1",
                        params![SqliteU64::new(v.intent.sequence), spelling(v.result)],
                    )
                    .map_err(|e| sqlite_error("project motion outcome", e))?;
            }
            Body::InventoryReconciled(v) => {
                // The historical SQL column is named library_serial; namespace
                // its derived key so identical library/drive serials cannot collide.
                let subject = match &v.subject {
                    lib::InventorySubject::Library(serial) => format!("library:{}", serial.0),
                    lib::InventorySubject::Standalone(endpoint) => {
                        format!("standalone:{}", endpoint.drive_serial.0)
                    }
                };
                self.conn.execute("insert into library_inventory values(?1,?2,?3) on conflict(library_serial) do update set generation=excluded.generation,body=excluded.body",params![subject,SqliteU64::new(v.observation_generation),record.canonical_body().map_err(|e|StateError::IndexCorrupt(e.to_string()))?]).map_err(|e|sqlite_error("project inventory reconciliation",e))?;
                for reference in &v.resolves.0 {
                    self.conn
                        .execute(
                            "update library_motion set state='reconciled' where sequence=?1",
                            [SqliteU64::new(reference.sequence)],
                        )
                        .map_err(|e| sqlite_error("resolve projected motion", e))?;
                }
            }
            Body::RecoveryRecorded(v) => {
                let digest = match &v.action {
                    lib::RecoveryAction::RepairApplied(a) => a.archived_prefix_digest,
                    lib::RecoveryAction::AppendAuthorityRecovered(a) => a.source_digest,
                };
                self.conn.execute("insert into library_recovery values(?1,?2,?3) on conflict(tape_uuid) do update set retained_through=excluded.retained_through,source_digest=excluded.source_digest",params![v.at.tape.uuid.as_slice(),SqliteU64::new(v.at.sequence),digest.as_slice()]).map_err(|e|sqlite_error("project recovery evidence",e))?;
            }
        }
        Ok(())
    }
}
fn text(value: &Option<lib::Text>) -> Option<String> {
    value.as_ref().map(|s| s.0.clone())
}
fn spelling(value: impl std::fmt::Debug) -> String {
    let name = format!("{value:?}");
    let mut out = String::new();
    for (n, c) in name.chars().enumerate() {
        if c.is_uppercase() && n > 0 {
            out.push('_');
        }
        out.extend(c.to_lowercase());
    }
    out
}
/// Retain fold-owned counts independently of SQL applications. At equal received
/// positions resolve can only remove pending entries; stale proofs cannot undo it.
fn record_fold_progress(
    conn: &Connection,
    progress: lib::authority::FoldProgress,
) -> Result<bool, StateError> {
    let changed = conn.execute("insert into library_fold_progress values(1,?1,?2,?3) on conflict(singleton) do update set received=excluded.received,non_applications=excluded.non_applications,pending=excluded.pending where excluded.received > library_fold_progress.received or (excluded.received = library_fold_progress.received and excluded.pending <= library_fold_progress.pending)",
        params![SqliteU64::new(progress.received), progress.non_applications, progress.pending])
        .map_err(|e| sqlite_error("record fold progress", e))?;
    Ok(changed != 0)
}

fn library_watermarks(
    conn: &Connection,
    sequence: u64,
    inserted: bool,
) -> Result<Watermarks, StateError> {
    let old: Option<(SqliteU64, SqliteU64, u64)> = conn
        .query_row(
            "select received,applied,records from library_projection_progress where singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(|e| sqlite_error("read library projection progress", e))?;
    let prefix_received: Option<SqliteU64> = conn
        .query_row(
            "select received from projection_sources where source='library'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| sqlite_error("read completed library source prefix", e))?;
    let fold_received: SqliteU64 = conn
        .query_row(
            "select received from library_fold_progress where singleton=1",
            [],
            |r| r.get(0),
        )
        .map_err(|e| sqlite_error("read fold received position", e))?;
    let received = sequence
        .max(prefix_received.map_or(0, SqliteU64::get))
        .max(fold_received.get());
    let (received, mut applied, count) = old
        .map_or((received, 0, u64::from(inserted)), |(r, a, n)| {
            (r.get().max(received), a.get(), n + u64::from(inserted))
        });
    let mut statement = conn
        .prepare(
            "select sequence from library_projection_records where sequence>?1 order by sequence",
        )
        .map_err(|e| sqlite_error("read library applied sequences", e))?;
    let rows = statement
        .query_map([SqliteU64::new(applied)], |r| r.get::<_, SqliteU64>(0))
        .map_err(|e| sqlite_error("query library applied sequences", e))?;
    for row in rows {
        let n = row
            .map_err(|e| sqlite_error("read library applied sequence", e))?
            .get();
        if n != applied + 1 {
            break;
        }
        applied = n;
    }
    conn.execute("insert into library_projection_progress values(1,?1,?2,?3) on conflict(singleton) do update set received=excluded.received,applied=excluded.applied,records=excluded.records",params![SqliteU64::new(received),SqliteU64::new(applied),count]).map_err(|e|sqlite_error("advance library projection progress",e))?;
    Ok(Watermarks {
        received,
        applied,
        applied_records: count,
        non_applications: conn
            .query_row(
                "select non_applications from library_fold_progress where singleton=1",
                [],
                |r| r.get(0),
            )
            .map_err(|e| sqlite_error("read fold non-application count", e))?,
        pending: conn
            .query_row(
                "select pending from library_fold_progress where singleton=1",
                [],
                |r| r.get(0),
            )
            .map_err(|e| sqlite_error("read fold pending count", e))?,
    })
}

fn label_source(value: Option<lib::LabelProvenance>) -> crate::LabelProvenance {
    if value == Some(lib::LabelProvenance::Operator) {
        crate::LabelProvenance::Operator
    } else {
        crate::LabelProvenance::Changer
    }
}
fn endpoint(value: &lib::DriveKey) -> crate::DriveKey {
    match value {
        lib::DriveKey::Bay(b) => crate::DriveKey::new(b.library_serial.0.clone(), b.bay),
        lib::DriveKey::Standalone(s) => crate::DriveKey::Standalone {
            drive_serial: s.drive_serial.0.clone(),
        },
    }
}
fn source_record(conn: &Connection, sequence: u64) -> Result<lib::Record, StateError> {
    let bytes: Vec<u8> = conn
        .query_row(
            "select source_bytes from library_projection_records where sequence=?1",
            [SqliteU64::new(sequence)],
            |r| r.get(0),
        )
        .map_err(|e| sqlite_error("read consumed library record", e))?;
    lib::codec::Decoder::at(bytes.as_slice(), sequence, 0)
        .next_frame()
        .map_err(|e| StateError::IndexCorrupt(e.to_string()))?
        .map(|f| f.record)
        .ok_or_else(|| StateError::IndexCorrupt("consumed library record absent".into()))
}
impl CatalogCore {
    fn project_library_medium(
        &mut self,
        tape: lib::TapeRef,
        medium: &lib::MediumSpec,
        revision: u64,
        bytes: &[u8],
        at: &str,
    ) -> Result<(), StateError> {
        if let lib::MediumFamily::Lto(generation) = medium.family {
            set_tape_generation_at(
                &self.conn,
                &tape.uuid,
                MediaObservation {
                    generation,
                    source: medium.source,
                },
                at,
            )?;
        } else {
            self.conn.execute("update tapes set cartridge_generation=null,generation_source=null where tape_uuid=?1", [tape.uuid.as_slice()])
                .map_err(|e| sqlite_error("project non-LTO family", e))?;
        }
        self.conn.execute("insert into library_tape_status(tape_uuid,incarnation,media_revision,medium_spec) values(?1,?2,?3,?4) on conflict(tape_uuid) do update set incarnation=excluded.incarnation,media_revision=excluded.media_revision,medium_spec=excluded.medium_spec",params![tape.uuid.as_slice(),SqliteU64::new(tape.incarnation),SqliteU64::new(revision),bytes]).map_err(|e|sqlite_error("project library medium capabilities",e))?;
        Ok(())
    }
}

impl CatalogCore {
    fn library_drive_at(&self, endpoint: &DriveKey) -> Result<Option<DriveRecord>, StateError> {
        let id: Option<Vec<u8>> = self
            .conn
            .query_row(
                "select drive_uuid from library_drive_endpoints where endpoint=?1",
                [endpoint.encode()],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| sqlite_error("read observed endpoint identity", e))?;
        let id = if id.is_none() {
            if let DriveKey::Standalone { drive_serial } = endpoint {
                let mut statement = self
                    .conn
                    .prepare(
                        "select drive_uuid from drives where serial=?1 and state='active' limit 2",
                    )
                    .map_err(|e| sqlite_error("find source-observed standalone drive", e))?;
                let ids = statement
                    .query_map([drive_serial], |row| row.get::<_, Vec<u8>>(0))
                    .map_err(|e| sqlite_error("query standalone drive identity", e))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| sqlite_error("read standalone drive identity", e))?;
                if ids.len() == 1 {
                    ids.into_iter().next()
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            id
        };
        id.map(|id| self.get_drive_by_uuid(&id))
            .transpose()
            .map(Option::flatten)
    }
}

impl CatalogCore {
    fn library_readiness_transition(
        &mut self,
        operation: lib::Id,
        state: &str,
        opcode: Option<u8>,
        evidence: Option<&lib::CompletionEvidence>,
        cancel_source: Option<String>,
    ) -> Result<(), StateError> {
        // The decoded sense is a view of the evidence's raw sense, read by the decoder the
        // producers classify a completion with, so it equals what they projected before.
        let decoded = evidence
            .and_then(|e| e.sense.as_ref())
            .and_then(|b| remanence_library::scsi::decode_usable_sense(&b.0));
        self.record_media_readiness_transition(
            None,
            MediaReadinessTransitionInput {
                completion_evidence: None,
                operation_id: Uuid::from_bytes(operation),
                phase: None,
                state: state.into(),
                dirty_scope: None,
                last_cdb_opcode: opcode,
                last_sense_raw: evidence
                    .and_then(|e| e.sense.as_ref())
                    .map(|b| b.0.iter().map(|x| format!("{x:02x}")).collect()),
                last_sense_key: decoded.map(|sense| sense.key),
                last_asc: decoded.map(|sense| sense.asc),
                last_ascq: decoded.map(|sense| sense.ascq),
                last_host_status: evidence.and_then(|e| e.host_status).map(i64::from),
                last_driver_status: evidence.and_then(|e| e.driver_status).map(i64::from),
                target_status: evidence.and_then(|e| e.scsi_status),
                transport_class: evidence
                    .filter(|e| e.completion == lib::Completion::Unknown)
                    .map(|_| "unknown".into()),
                cancel_source,
                signal: None,
                evidence_path: None,
                last_error_json: None,
                quarantine_id: matches!(
                    state,
                    "aborted_unknown"
                        | "timeout_unknown"
                        | "transport_unknown"
                        | "terminal_error"
                        | "reservation_conflict"
                )
                .then(|| format!("mrq-{}", Uuid::from_bytes(operation))),
            },
        )?;
        Ok(())
    }
}

impl CatalogCore {
    /// Ingestion bookkeeping comes from the fold after all its observed applies
    /// have committed; it does not make another readiness or ordering decision.
    pub(in crate::projector) fn finish_library_replay(
        &mut self,
        fold: &lib::LibraryFold,
    ) -> Result<(), StateError> {
        let tx = projection_transaction(&mut self.conn)
            .map_err(|e| sqlite_error("begin source progress", e))?;
        let received: Option<SqliteU64> = tx
            .query_row(
                "select received from projection_sources where source='library'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| sqlite_error("read source progress", e))?;
        if received.is_some_and(|n| n.get() > fold.received_through) {
            return tx
                .commit()
                .map_err(|e| sqlite_error("retain newer source progress", e));
        }

        if !record_fold_progress(&tx, lib::authority::FoldProgress::capture(fold))? {
            return tx
                .commit()
                .map_err(|e| sqlite_error("retain resolved fold progress", e));
        }

        // Finish the received prefix even when its tail produced no SQL
        // applications. Keep the application count and contiguous prefix intact.
        let marks = library_watermarks(&tx, fold.received_through, false)?;
        tx.execute("insert into projection_sources values('library',?1,?2,?3) on conflict(source) do update set received=excluded.received,applied=excluded.applied,pending=excluded.pending",params![SqliteU64::new(marks.received),SqliteU64::new(marks.applied),marks.pending]).map_err(|e|sqlite_error("finish source progress",e))?;
        for tape in fold.tapes.values() {
            tx.execute(
                "update library_tape_status set contradicted=?2 where tape_uuid=?1",
                params![tape.tape.uuid.as_slice(), fold.contradicted(tape.tape)],
            )
            .map_err(|e| sqlite_error("project contradicted tape status", e))?;
        }
        tx.commit()
            .map_err(|e| sqlite_error("commit source progress", e))
    }
}

impl CatalogCore {
    /// Retain unbound cleaning runs without inventing a drive identity. Once
    /// bound, materialize the existing typed run view using the original Begin.
    fn project_library_cleaning_binding(
        &mut self,
        value: &AppliedLibraryRecord,
        v: &lib::CleaningRecorded,
        at: &str,
    ) -> Result<bool, StateError> {
        if matches!(
            v.action,
            lib::CleaningAction::Register(_)
                | lib::CleaningAction::Expire(_)
                | lib::CleaningAction::Reject(_)
        ) {
            return Ok(true);
        }
        self.conn.execute(
            "insert into library_cleaning_runs values(?1,?2,?3,?4,?5) on conflict(run_id) do update set body=excluded.body,cartridge=coalesce(excluded.cartridge,library_cleaning_runs.cartridge),home_slot=coalesce(excluded.home_slot,library_cleaning_runs.home_slot)",
            params![v.run_id.0, SqliteU64::new(value.sequence), value.record.canonical_body().map_err(|e|StateError::IndexCorrupt(e.to_string()))?, v.cartridge.map(|t|t.uuid.to_vec()), match &v.action { lib::CleaningAction::Advance(a) => a.home_slot, _ => None }],
        ).map_err(|e|sqlite_error("retain cleaning run binding",e))?;
        if let (Some(tape), Some(home)) = (
            self.library_cleaning_run_cartridge(&v.run_id.0)?,
            self.library_cleaning_run_home(v)?,
        ) {
            self.conn.execute("insert into library_cleaning_homes values(?1,?2) on conflict(tape_uuid) do update set home_slot=excluded.home_slot", params![tape, home])
                .map_err(|e| sqlite_error("project selected cleaning home", e))?;
        }
        if self.get_clean_run(&v.run_id.0)?.is_some() {
            return Ok(true);
        }
        let Some(drive) = v.drive_uuid else {
            return Ok(false);
        };
        let (trigger, started) = if let lib::CleaningAction::Begin(begin) = &v.action {
            (spelling(begin.trigger), at.to_string())
        } else {
            let sequence = self
                .conn
                .query_row(
                    "select begin_sequence from library_cleaning_runs where run_id=?1",
                    [&v.run_id.0],
                    |r| r.get::<_, SqliteU64>(0),
                )
                .map_err(|e| sqlite_error("read cleaning begin sequence", e))?
                .get();
            let begin = source_record(&self.conn, sequence)?;
            let lib::BodyView::CleaningRecorded(lib::CleaningRecorded {
                action: lib::CleaningAction::Begin(begin_action),
                ..
            }) = begin.body_view()
            else {
                return Err(StateError::IndexCorrupt("cleaning run lacks Begin".into()));
            };
            let started = OffsetDateTime::from_unix_timestamp_nanos(i128::from(
                begin.envelope.timestamp_unix_ns,
            ))
            .map_err(|e| StateError::IndexCorrupt(e.to_string()))?
            .format(&Rfc3339)
            .map_err(|e| StateError::IndexCorrupt(e.to_string()))?;
            (spelling(begin_action.trigger), started)
        };
        let serial = self
            .get_drive_by_uuid(&drive)?
            .and_then(|d| d.last_library_serial)
            .unwrap_or_default();
        begin_clean_run_core(
            &self.conn,
            &v.run_id.0,
            &drive,
            &serial,
            &trigger,
            None,
            &started,
        )?;
        self.project_library_cleaning_selection(v)?;
        Ok(true)
    }

    fn project_library_drive_fences(&mut self, key: &DriveKey) -> Result<(), StateError> {
        let active:bool=self.conn.query_row("select exists(select 1 from library_drive_fences where endpoint=?1 and released=0)",[key.encode()],|r|r.get(0)).map_err(|e|sqlite_error("read remaining drive fences",e))?;
        if let Some(drive) = self.library_drive_at(key)? {
            self.set_drive_fenced(&drive.drive_uuid, active)?;
        }
        Ok(())
    }
    /// MovingIn fixes the return slot; an unselected run has no home.
    fn library_cleaning_run_home(
        &self,
        current: &lib::CleaningRecorded,
    ) -> Result<Option<u16>, StateError> {
        self.conn
            .query_row(
                "select home_slot from library_cleaning_runs where run_id=?1",
                [&current.run_id.0],
                |r| r.get(0),
            )
            .map_err(|e| sqlite_error("read cleaning run home", e))
    }
    /// Retain a selected cartridge through records that omit an unchanged binding.
    fn library_cleaning_run_cartridge(&self, run: &str) -> Result<Option<Vec<u8>>, StateError> {
        self.conn
            .query_row(
                "select cartridge from library_cleaning_runs where run_id=?1",
                [run],
                |r| r.get(0),
            )
            .map_err(|e| sqlite_error("read cleaning run cartridge", e))
    }
    fn project_library_cleaning_selection(
        &mut self,
        current: &lib::CleaningRecorded,
    ) -> Result<(), StateError> {
        if let (Some(tape), Some(home)) = (
            self.library_cleaning_run_cartridge(&current.run_id.0)?,
            self.library_cleaning_run_home(current)?,
        ) {
            self.select_clean_run_cart(&current.run_id.0, &tape, i64::from(home), None)?;
        }
        Ok(())
    }
}

impl CatalogCore {
    pub(in crate::projector) fn committed_watermarks(
        &self,
        source: &AuthorityRecord,
    ) -> Result<Watermarks, StateError> {
        match source {
            AuthorityRecord::Library(_) => self.conn.query_row(
                "select s.received,s.applied,s.pending,coalesce(p.records,0),coalesce(f.non_applications,0) from projection_sources s left join library_projection_progress p on p.singleton=1 left join library_fold_progress f on f.singleton=1 where s.source='library'", [],
                |r| Ok(Watermarks { received:r.get::<_,SqliteU64>(0)?.get(), applied:r.get::<_,SqliteU64>(1)?.get(), pending:r.get(2)?, applied_records:r.get(3)?, non_applications:r.get(4)? }))
                .optional().map_err(|e|sqlite_error("read committed library watermarks",e))
                .map(|m|m.unwrap_or(Watermarks {received:0,applied:0,pending:0,applied_records:0,non_applications:0})),
            AuthorityRecord::Tape(snapshot) => {
                let applied=self.projected_through_sequence(snapshot.raw.header().tape_uuid)?;
                Ok(Watermarks {received:snapshot.log_reference().0,applied,applied_records:applied,non_applications:0,pending:snapshot.log_reference().0.saturating_sub(applied)})
            }
        }
    }
}

#[cfg(test)]
impl CatalogCore {
    /// Compare semantic rows, excluding wall-clock projections and allocation IDs.
    pub(in crate::projector) fn normalized_authority_dump(
        &self,
    ) -> Result<std::collections::BTreeMap<String, Vec<String>>, StateError> {
        let mut result = std::collections::BTreeMap::new();
        let mut stmt=self.conn.prepare("select name from sqlite_master where type='table' and name not like 'sqlite_%' order by name").unwrap();
        let tables = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        for table in tables {
            let mut info = self
                .conn
                .prepare(&format!("pragma table_info(\"{table}\")"))
                .unwrap();
            let cols = info
                .query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            let cols: Vec<_> = cols
                .into_iter()
                .filter(|c| {
                    !c.ends_with("_utc")
                        && !c.ends_with("_at")
                        && !matches!(
                            c.as_str(),
                            "snapshot_id" | "event_id" | "transition_id" | "history_id"
                        )
                })
                .collect();
            if cols.is_empty() {
                continue;
            }
            let mut query = self
                .conn
                .prepare(&format!(
                    "select {} from \"{table}\"",
                    cols.iter()
                        .map(|c| format!("\"{c}\""))
                        .collect::<Vec<_>>()
                        .join(",")
                ))
                .unwrap();
            let mut rows = query
                .query_map([], |r| {
                    Ok((0..cols.len())
                        .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
                        .collect::<Vec<_>>()
                        .join("|"))
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            rows.sort();
            result.insert(table, rows);
        }
        Ok(result)
    }
}

/// TapeAlert flag numbers are 1..=64; library mask bit zero represents flag 1.
/// Keep the existing catalog's JSON array representation for current readers.
/// Legacy health producers emit JSON presence metadata, not transport bytes.
/// Preserve that presentation exactly; opaque page blobs retain hex display.
fn health_raw_pages(bytes: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        if serde_json::from_str::<serde_json::Value>(text).is_ok_and(|v| v.is_object()) {
            return text.to_owned();
        }
    }
    bytes.iter().map(|x| format!("{x:02x}")).collect()
}
fn tape_alert_json(mask: u64) -> String {
    let flags = (0..64)
        .filter(|bit| mask & (1u64 << bit) != 0)
        .map(|bit| (bit + 1).to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!("[{flags}]")
}
