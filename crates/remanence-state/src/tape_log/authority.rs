//! Unwired B2 authority implementation over one per-tape log. The exclusive
//! owner retains the fold and lends weak structural capabilities; snapshots own
//! immutable inode prefixes. No production alias selects this implementation.
use super::{
    fold::TapeSummary,
    rows::{journal, Rows},
    snapshot::Snapshot,
    store::{GroupWriter, Header, Lease, Store, StoreError},
    wire::Wire,
    *,
};
use crate::authority::{
    AuthorityCommit, AuthorityFinalization, AuthorityPrefixPlan, CatalogPayload, ObjectPayload,
    TerminalFinalizationProgress as Progress,
};
use crate::{CheckpointBoundedReplayMetrics, StateError};
use remanence_parity::authority::*;
use remanence_parity::{FrozenFileRows, JournalError, TapeFileKind};
use std::{
    io::{self, Write},
    path::Path,
    sync::{Arc, Mutex, MutexGuard, Weak},
};

pub(super) fn invalid(message: impl Into<String>) -> StateError {
    StateError::JournalReplayFailed(message.into())
}
fn unsupported(operation: &'static str) -> StateError {
    StoreError::Volume(JournalError::UnsupportedOperation(operation)).into()
}
fn publication_error(error: StateError) -> JournalError {
    match error {
        StateError::TapeLog(error) => match *error {
            StoreError::Volume(error) => error,
            StoreError::Io(error) => JournalError::Io(error),
            StoreError::Header => JournalError::HeaderMismatch,
            StoreError::Locked(error) => {
                JournalError::Io(io::Error::from_raw_os_error(error as i32))
            }
            error @ (StoreError::Ambiguous(_)
            | StoreError::AfterDurable(_)
            | StoreError::Fenced) => JournalError::RecoveryRequired(error.to_string()),
            error => journal(error),
        },
        error => journal(error),
    }
}
fn progress(count: u8) -> Progress {
    [
        Progress::BeforeReplicaA,
        Progress::AfterReplicaA,
        Progress::AfterSeparationAb,
        Progress::AfterReplicaB,
        Progress::AfterSeparationBc,
        Progress::AfterReplicaC,
    ][usize::from(count)]
}
fn count(p: Progress) -> u8 {
    match p {
        Progress::BeforeReplicaA => 0,
        Progress::AfterReplicaA => 1,
        Progress::AfterSeparationAb => 2,
        Progress::AfterReplicaB => 3,
        Progress::AfterSeparationBc => 4,
        Progress::AfterReplicaC => 5,
    }
}
fn summary(s: &TapeSummary, uuid: [u8; 16]) -> AuthoritySummary {
    AuthoritySummary {
        tape_uuid: uuid,
        checkpoint_ordinal: s.last_closed_sequence,
        object_count: s.object_count,
        cursor: (!s.is_fresh()).then_some(s.cursor),
        protected: s.protected,
        total: s.total,
        next_file: s.next_file,
        block_size: (!s.is_fresh()).then_some(s.block_size),
        seal_due: s.seal_due,
        sealed: s.sealed,
    }
}
fn metrics(s: &Snapshot) -> CheckpointBoundedReplayMetrics {
    CheckpointBoundedReplayMetrics {
        replay_passes: s.replay_metrics().validation_passes,
        frame_count: s.summary.last_closed_sequence,
        peak_frame_payload_bytes: s.replay_metrics().peak_frame_buffer as u64,
        peak_live_record_count: 1,
        peak_live_object_rows: 1,
    }
}
fn bridge<T>(
    operation: impl FnOnce(&mut dyn FnMut(&T) -> Result<(), StoreError>) -> Result<(), StoreError>,
    visitor: &mut dyn FnMut(&T) -> Result<(), StateError>,
) -> Result<(), StateError> {
    let mut failure = None;
    let result = operation(&mut |item| {
        visitor(item).map_err(|error| {
            failure = Some(error);
            StoreError::Refused("visitor failed")
        })
    });
    if let Some(error) = failure {
        return Err(error);
    }
    result.map_err(Into::into)
}
/// Frozen validated authority, including only durable physical rows.
#[derive(Debug, Clone)]
pub struct AuthoritySnapshot {
    pub(super) raw: Snapshot,
    summary: AuthoritySummary,
    finalization: Option<AuthorityFinalization>,
}
impl AuthoritySnapshot {
    pub(super) fn new(raw: Snapshot) -> Result<Self, StateError> {
        let finalization = read_plan(&raw)?;
        Ok(Self {
            summary: summary(raw.summary(), raw.header().tape_uuid),
            raw,
            finalization,
        })
    }
}
impl TapeAuthoritySnapshot for AuthoritySnapshot {
    type Error = StateError;
    type Payload = CatalogPayload;
    type Finalization = AuthorityFinalization;
    fn summary(&self) -> &AuthoritySummary {
        &self.summary
    }
    fn finalization(&self) -> Option<&AuthorityFinalization> {
        self.finalization.as_ref()
    }
    fn into_file_rows(self) -> Option<FrozenFileRows> {
        self.raw.header.scheme.as_ref()?;
        Some(FrozenFileRows::new(Rows(self.raw)))
    }
    fn visit_file_rows(
        &self,
        visitor: &mut dyn FnMut(&TapeFileEntry) -> Result<(), StateError>,
    ) -> Result<(), StateError> {
        bridge(|v| self.raw.visit_file_rows(v), visitor)
    }
    fn visit_object_rows(
        &self,
        visitor: &mut dyn FnMut(&ObjectRecoveryRow) -> Result<(), StateError>,
    ) -> Result<(), StateError> {
        self.visit_file_rows(&mut |row| {
            if let Some(object) = &row.object_recovery_row {
                visitor(object)?;
            }
            Ok(())
        })
    }
    fn visit_payloads(
        &self,
        visitor: &mut AuthorityVisitor<'_, CatalogPayload, StateError>,
    ) -> Result<(), StateError> {
        bridge(|v| self.raw.visit_catalog(v), visitor)
    }
}
fn read_plan(raw: &Snapshot) -> Result<Option<AuthorityFinalization>, StateError> {
    let Some(state) = &raw.summary.finalization else {
        return Ok(None);
    };
    let mut files = Vec::new();
    let mut directory = Vec::new();
    let mut found = None;
    if let Some(mut decoder) = raw.decoder() {
        while let Some(frame) = decoder.next_row_frame().map_err(StoreError::from)? {
            if frame.sequence < state.reference.first_sequence {
                continue;
            }
            if frame.sequence > state.sequence {
                break;
            }
            match frame.record {
                Record::PayloadContinuation(Continuation {
                    chunk: Chunk::FileRow(row),
                    ..
                }) => files.push(row),
                Record::PayloadContinuation(Continuation {
                    chunk: Chunk::PlanDirectory(row),
                    ..
                }) => directory.push(row),
                Record::FinalizationPrepared(plan) | Record::FinalizationDecided(plan) => {
                    found = Some(*plan)
                }
                _ => {}
            }
        }
    }
    let plan = found.ok_or_else(|| invalid("missing finalization record"))?;
    let prefix = if raw.header.scheme.is_some() {
        let base = raw.before(state.reference.first_sequence)?;
        let protected = files
            .iter()
            .filter_map(|r| r.protected_ordinal_end_exclusive)
            .next_back()
            .unwrap_or(base.summary.protected);
        Some(AuthorityPrefixPlan {
            diagnostics: remanence_parity::ParityMapDiagnostics {
                writer_version: plan.writer_version.clone(),
                write_timestamp: plan.write_timestamp.clone(),
            },
            start_tape_file_number: base.summary.next_file,
            tail_start_tape_file_number: plan.layout.components[0].planned_tape_file_number,
            start_lba: base.summary.cursor.lba,
            tail_start_lba: plan.prefix_end.lba,
            parity_map_tape_file_number: files
                .iter()
                .find(|r| r.kind == TapeFileKind::ParityMap)
                .map(|r| r.tape_file_number),
            sidecar_directory_entries: directory,
            publication: FilePublication {
                purpose: FilePurpose::TerminalPrefix,
                files,
                protected,
                total: base.summary.total,
                cursor: None,
                check: None,
                seal_due: None,
            },
        })
    } else {
        None
    };
    Ok(Some(AuthorityFinalization {
        tape_uuid: raw.header.tape_uuid,
        trigger: plan.trigger,
        manual: plan.manual,
        progress: progress(state.components_written),
        recovery_required: state.recovery_required,
        edition_id: plan.edition_id,
        edition_sequence: plan.edition_sequence,
        edition_digest: plan.edition_digest,
        writer_version: plan.writer_version,
        write_timestamp: plan.write_timestamp,
        terminal_prefix: prefix,
        layout: plan.layout.try_into()?,
    }))
}
/// One-member-at-a-time semantic report producer. The Object metadata and file
/// bundles live in the ordinary report; the callback supplies only its members.
/// Each invocation receives the Object's zero-based index in that report.
pub type ReportMembers<'a> = dyn FnMut(
        usize,
        &mut dyn FnMut(&crate::NativeObjectFileProjectionInput) -> Result<(), StateError>,
    ) -> Result<(), StateError>
    + 'a;

/// A frozen report boundary plus streaming payload access. No Object/member list
/// is decoded while enumerating reports; nested visits own independent cursors.
pub struct ReportView {
    summary: AuthorityCommit,
    commit: Committed,
    group: Snapshot,
}
impl ReportView {
    /// Structural admission fields, with catalog and physical bundles omitted.
    pub fn summary(&self) -> &AuthorityCommit {
        &self.summary
    }
    /// Exact closer, including check provenance and unverified range end.
    pub fn commit(&self) -> &Committed {
        &self.commit
    }
    /// Visit one catalog item at a time within this report's immutable group.
    pub fn visit_payloads(
        &self,
        visitor: &mut dyn FnMut(&CatalogPayload) -> Result<(), StateError>,
    ) -> Result<(), StateError> {
        bridge(|v| self.group.visit_catalog(v), visitor)
    }
    /// Stream the report's physical rows without decoding its member catalog.
    pub fn visit_file_rows(
        &self,
        visitor: &mut dyn FnMut(&TapeFileEntry) -> Result<(), StateError>,
    ) -> Result<(), StateError> {
        bridge(|v| self.group.visit_file_rows(v), visitor)
    }
}
fn stream_reports(
    raw: &Snapshot,
    visitor: &mut dyn FnMut(&ReportView) -> Result<(), StateError>,
) -> Result<(), StateError> {
    let Some(mut decoder) = raw.decoder() else {
        return Ok(());
    };
    let mut start = None;
    while let Some(frame) = decoder.next_row_frame().map_err(StoreError::from)? {
        let (sequence, offset) = *start.get_or_insert((frame.sequence, frame.offset));
        if matches!(
            frame.record,
            Record::Initialized(_) | Record::PayloadContinuation(_)
        ) {
            continue;
        }
        if let Record::Committed(c) = frame.record {
            visitor(&ReportView {
                summary: report_summary(&c, frame.sequence, &raw.header),
                commit: c,
                group: raw.group_snapshot(sequence, offset, frame.end),
            })?;
        }
        start = None;
    }
    Ok(())
}
/// Factory for an explicitly selected, unwired per-tape backend.
#[derive(Debug)]
pub struct TapeAuthority {
    store: Store,
}
impl TapeAuthority {
    /// Open an existing identity, or bind geometry at the first report.
    pub fn open(directory: impl AsRef<Path>, uuid: [u8; 16]) -> Result<Self, StateError> {
        Ok(Self {
            store: Store::open(directory, uuid)?,
        })
    }
    /// Open on an ordinary temporary filesystem for contract tests.
    #[cfg(any(test, feature = "fixtures"))]
    pub fn open_fixture(directory: impl AsRef<Path>, uuid: [u8; 16]) -> Result<Self, StateError> {
        Ok(Self {
            store: Store::open_fixture(directory, uuid)?,
        })
    }
    /// Enumerate per-tape entries in stable path order without decoding catalogs.
    pub fn list_entries(
        directory: impl AsRef<Path>,
    ) -> Result<Vec<std::path::PathBuf>, StateError> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(directory).map_err(StoreError::from)? {
            let path = entry.map_err(StoreError::from)?.path();
            if path.extension().is_some_and(|e| e == "remtape") {
                Self::entry_tape_uuid(&path)?;
                entries.push(path);
            }
        }
        entries.sort();
        Ok(entries)
    }
    /// Decode an enumerated entry's UUID; file content is checked during capture.
    pub fn entry_tape_uuid(path: &Path) -> Result<[u8; 16], StateError> {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| invalid("invalid tape-log filename"))?;
        if path
            .extension()
            .is_none_or(|extension| extension != "remtape")
            || stem.len() != 32
            || !stem
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(invalid("tape-log UUID must be lowercase simple form"));
        }
        uuid::Uuid::parse_str(stem)
            .map(|id| *id.as_bytes())
            .map_err(|_| invalid("invalid tape-log UUID"))
    }
    /// Check the active name without creating a log.
    pub fn exists(directory: impl AsRef<Path>, id: [u8; 16]) -> Result<bool, StateError> {
        directory
            .as_ref()
            .join(format!("{}.remtape", uuid::Uuid::from_bytes(id).simple()))
            .try_exists()
            .map_err(StoreError::from)
            .map_err(Into::into)
    }
    /// Acquire an ordinary writer; pending finalization requires recovery routing.
    pub fn acquire_exclusive(&self) -> Result<TapeAuthorityLease, StateError> {
        let lease = self.acquire_exclusive_for_terminal_recovery()?;
        let pending = {
            let core = lease.core()?;
            core.summary().finalization.is_some() && !core.summary().sealed
        };
        if pending {
            return Err(invalid("pending terminal finalization requires recovery"));
        }
        Ok(lease)
    }
    /// Retain the same exclusive lock for terminal recovery.
    pub fn acquire_exclusive_for_terminal_recovery(
        &self,
    ) -> Result<TapeAuthorityLease, StateError> {
        Ok(TapeAuthorityLease {
            uuid: self.store.header.tape_uuid,
            core: Arc::new(Mutex::new(self.store.acquire(false)?)),
            structural: None,
            scope: Scope::Idle,
        })
    }
}
#[derive(Debug, PartialEq)]
enum Scope {
    Idle,
    Lent,
    Retiring,
}
/// Exclusive authority owner. A borrowed structural handle cannot keep it alive.
#[derive(Debug)]
pub struct TapeAuthorityLease {
    uuid: [u8; 16],
    core: Arc<Mutex<Lease>>,
    structural: Option<StructuralCapability>,
    scope: Scope,
}
impl TapeAuthorityLease {
    fn core(&self) -> Result<MutexGuard<'_, Lease>, StateError> {
        self.core
            .lock()
            .map_err(|_| invalid("authority owner panicked"))
    }
    /// Publish a semantic report with a bounded member producer. Uses the same
    /// report-to-frame path and validation as B2's owned adapter, without building
    /// a member Vec. The callback must emit each Object's members in source order.
    pub fn publish_report_streaming(
        &mut self,
        report: &AuthorityCommit,
        members: &mut ReportMembers<'_>,
    ) -> Result<(), StateError> {
        publish_report(&mut *self.core()?, report, Some(members))
    }
    /// Publish final completion after all five proved components. The fold carries
    /// the seal obligation forward; the after seam runs only after durability.
    pub fn seal<F: FnOnce() -> Result<(), StateError>>(
        &mut self,
        after: F,
    ) -> Result<(), StateError> {
        let mut core = self.core()?;
        let state = core
            .summary()
            .finalization
            .as_ref()
            .ok_or_else(|| invalid("no finalization"))?;
        if !core.summary().sealed {
            let record = Record::Sealed {
                decision: state.sequence,
                cursor: core.summary().cursor,
                edition_digest: state.edition_digest,
            };
            let group = core.prepare()?;
            core.append(group.close(&record)?)?;
        }
        drop(core);
        after().map_err(|error| {
            StoreError::AfterDurable(Box::new(StoreError::Io(io::Error::other(error)))).into()
        })
    }
    /// Borrow the retained store for a streaming group. Continuations must be
    /// staged before media durability, then the closing record completed after it.
    /// This does not reacquire a lock or materialize a catalog member list.
    pub fn with_store<R>(
        &mut self,
        operation: impl FnOnce(&mut Lease) -> Result<R, StateError>,
    ) -> Result<R, StateError> {
        operation(&mut *self.core()?)
    }
    /// Identity bound to this lease.
    pub fn tape_uuid(&self) -> [u8; 16] {
        self.uuid
    }
    /// Bind the geometry and create a weak publication facet under this owner.
    pub fn open_structural(
        &self,
        block_size: u32,
        scheme: ParityScheme,
    ) -> Result<StructuralCapability, JournalError> {
        let mut core = self.core().map_err(journal)?;
        let uuid = core.header().tape_uuid;
        core.bind_header(Header {
            tape_uuid: uuid,
            block_size,
            scheme: Some(scheme),
        })
        .map_err(journal)?;
        Ok(Box::new(Structural {
            core: Arc::downgrade(&self.core),
            uuid,
        }))
    }
    /// Retain a capability for this tape; replacement during a loan is refused.
    pub fn retain_structural_publisher(
        &mut self,
        publisher: impl StructuralAuthority + 'static,
    ) -> Result<(), StateError> {
        if publisher.tape_uuid() != self.tape_uuid()
            || self.structural.is_some()
            || self.scope != Scope::Idle
        {
            return Err(invalid(
                "structural capability does not belong to this unbound writer",
            ));
        }
        self.structural = Some(Box::new(publisher));
        Ok(())
    }
    /// Lend the facet while retaining the exclusive authority owner.
    pub fn with_structural<R>(
        &mut self,
        operation: impl FnOnce(&mut Self, Option<&mut dyn StructuralAuthority>) -> R,
    ) -> Result<R, StateError> {
        self.with_structural_scope(|owner, slot| {
            operation(
                owner,
                slot.as_mut()
                    .map(|s| s.as_mut() as &mut dyn StructuralAuthority),
            )
        })
    }
    /// Panic-safe loan of the retained publication slot.
    pub fn with_structural_scope<R>(
        &mut self,
        operation: impl FnOnce(&mut Self, &mut Option<StructuralCapability>) -> R,
    ) -> Result<R, StateError> {
        if self.scope != Scope::Idle {
            return Err(invalid("nested structural publication loan"));
        }
        let mut slot = self.structural.take();
        self.scope = Scope::Lent;
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| operation(self, &mut slot)));
        let retire = self.scope == Scope::Retiring;
        self.scope = Scope::Idle;
        if !retire {
            self.structural = slot;
        }
        match result {
            Ok(v) => Ok(v),
            Err(p) => std::panic::resume_unwind(p),
        }
    }
    /// Retire the structural facet at the terminal operation boundary.
    pub fn finish_structural_scope(&mut self) {
        self.structural = None;
        if self.scope == Scope::Lent {
            self.scope = Scope::Retiring;
        }
    }
    /// Read the retained structural capability.
    pub fn structural(&self) -> Option<&dyn StructuralAuthority> {
        self.structural.as_deref()
    }
    /// Borrow the retained structural capability.
    pub fn structural_mut(&mut self) -> Option<&mut dyn StructuralAuthority> {
        self.structural
            .as_mut()
            .map(|s| s.as_mut() as &mut dyn StructuralAuthority)
    }
}
fn visit_reports(
    raw: &Snapshot,
    visitor: &mut AuthorityVisitor<'_, AuthorityCommit, StateError>,
) -> Result<CheckpointBoundedReplayMetrics, StateError> {
    let Some(mut decoder) = raw.decoder() else {
        return Ok(metrics(raw));
    };
    let mut measured = metrics(raw);
    measured.replay_passes += 1; // the boundary cursor opened above
    let mut start = None;
    let mut previous_protected = 0;
    while let Some(frame) = decoder.next_row_frame().map_err(StoreError::from)? {
        let (sequence, offset) = *start.get_or_insert((frame.sequence, frame.offset));
        if matches!(
            frame.record,
            Record::Initialized(_) | Record::PayloadContinuation(_)
        ) {
            continue;
        }
        if matches!(frame.record, Record::Sealed { .. }) {
            return Err(unsupported(
                "legacy sealed semantic projection; use the frozen summary and finalization status",
            ));
        }
        if let Record::Committed(c) = frame.record {
            let group = raw.group_snapshot(sequence, offset, frame.end);
            measured.replay_passes += 1; // this group's catalog cursor
            let mut payloads = Vec::<ObjectPayload>::new();
            group.visit_catalog(|event| {
                match event {
                    CatalogPayload::Object(p) => payloads.push(ObjectPayload {
                        projection: (**p).clone(),
                        files: Vec::new(),
                    }),
                    CatalogPayload::Member(m) => payloads
                        .last_mut()
                        .expect("catalog header")
                        .files
                        .push(m.clone()),
                    CatalogPayload::ObjectEnd => {}
                }
                Ok(())
            })?;
            let mut rows = Vec::new();
            measured.replay_passes += 2; // row boundary and row emitter cursors
            group.visit_file_rows(|row| {
                if row.kind != TapeFileKind::Bootstrap {
                    rows.push(row.clone())
                }
                Ok(())
            })?;
            let mut object_files = Vec::new();
            let mut barrier = None;
            if raw.header.scheme.is_some() {
                let mut position = 0;
                let mut protected = previous_protected;
                for payload in &payloads {
                    let mut files = Vec::new();
                    if position < rows.len() {
                        files.push(rows[position].clone());
                        position += 1;
                    }
                    let bound = payload.copy.protected_until_ordinal.unwrap_or(protected);
                    while position < rows.len()
                        && rows[position].kind == TapeFileKind::ParitySidecar
                        && rows[position]
                            .protected_ordinal_end_exclusive
                            .is_some_and(|end| end <= bound)
                    {
                        files.push(rows[position].clone());
                        position += 1;
                    }
                    protected = files
                        .iter()
                        .filter_map(|r| r.protected_ordinal_end_exclusive)
                        .next_back()
                        .unwrap_or(protected);
                    object_files.push(FilePublication {
                        purpose: FilePurpose::Object,
                        files,
                        protected,
                        total: payload.total_committed_ordinals,
                        cursor: None,
                        check: None,
                        seal_due: None,
                    });
                }
                if position < rows.len() {
                    barrier = Some(FilePublication {
                        purpose: FilePurpose::Sidecars(SidecarProvenance::Fresh),
                        files: rows[position..].to_vec(),
                        protected: c.protected,
                        total: c.total,
                        cursor: None,
                        check: None,
                        seal_due: None,
                    });
                }
            }
            measured.peak_live_object_rows =
                measured.peak_live_object_rows.max(payloads.len() as u64);
            previous_protected = c.protected;
            visitor(&AuthorityCommit {
                ordinal: frame.sequence,
                object_count: c.object_count,
                protected: c.protected,
                total: c.total,
                cursor: c.cursor,
                tape_uuid: raw.header.tape_uuid,
                operation_id: c.batch_id,
                next_file: c.next_file,
                block_size: raw.header.block_size,
                payloads,
                scheme: raw.header.scheme.clone(),
                object_files,
                barrier,
                finalization: None,
                sealed: false,
                seal_due: c.seal_due,
                check: (c.provenance != CheckProvenance::Empty).then_some((c.check, c.provenance)),
            })?;
        }
        start = None;
    }
    Ok(measured)
}
// B2's compatibility shape is retained, but latest admission needs only the
// last ordinary closer. Catalogs remain accessible through report/snapshot cursors.
fn latest_report(s: &TapeSummary, header: &Header) -> Result<Option<AuthorityCommit>, StateError> {
    if s.sealed {
        return Err(unsupported(
            "legacy sealed semantic projection; use the frozen summary and finalization status",
        ));
    }
    Ok(s.last_ordinary_record.as_ref().map(|c| {
        report_summary(
            c,
            s.last_ordinary_commit.expect("record has sequence"),
            header,
        )
    }))
}
fn report_summary(c: &Committed, sequence: u64, header: &Header) -> AuthorityCommit {
    AuthorityCommit {
        ordinal: sequence,
        object_count: c.object_count,
        protected: c.protected,
        total: c.total,
        cursor: c.cursor,
        tape_uuid: header.tape_uuid,
        operation_id: c.batch_id,
        next_file: c.next_file,
        block_size: header.block_size,
        payloads: Vec::new(),
        scheme: header.scheme.clone(),
        object_files: Vec::new(),
        barrier: None,
        finalization: None,
        sealed: false,
        seal_due: c.seal_due,
        check: (c.provenance != CheckProvenance::Empty).then_some((c.check, c.provenance)),
    }
}
trait Capture {
    fn capture(&self) -> Result<Snapshot, StateError>;
    fn latest_report_summary(&self) -> Result<Option<AuthorityCommit>, StateError> {
        let raw = self.capture()?;
        latest_report(&raw.summary, &raw.header)
    }
    fn small_summary(&self) -> Result<AuthoritySummary, StateError> {
        let raw = self.capture()?;
        Ok(summary(raw.summary(), raw.header.tape_uuid))
    }
}
impl Capture for TapeAuthority {
    fn capture(&self) -> Result<Snapshot, StateError> {
        Ok(self.store.snapshot()?)
    }
}
impl Capture for TapeAuthorityLease {
    fn latest_report_summary(&self) -> Result<Option<AuthorityCommit>, StateError> {
        let core = self.core()?;
        latest_report(core.summary(), core.header())
    }
    fn capture(&self) -> Result<Snapshot, StateError> {
        Ok(self.core()?.snapshot()?)
    }
    fn small_summary(&self) -> Result<AuthoritySummary, StateError> {
        let core = self.core()?;
        Ok(summary(core.summary(), core.header().tape_uuid))
    }
}
macro_rules! view {
    ($ty:ty) => {
        impl $ty {
            /// Stream frozen report boundaries; members are pulled through each
            /// ReportView, so neither commit history nor member lists are retained.
            pub fn visit_reports(
                &self,
                visitor: &mut dyn FnMut(&ReportView) -> Result<(), StateError>,
            ) -> Result<(), StateError> {
                stream_reports(&self.capture()?, visitor)
            }
        }
        impl TapeAuthorityView for $ty {
            type Error = StateError;
            type Commit = AuthorityCommit;
            type ReplayMetrics = CheckpointBoundedReplayMetrics;
            type Finalization = AuthorityFinalization;
            type TerminalPrefix = AuthorityPrefixPlan;
            type TerminalSource = super::terminal_source::TerminalSource;
            type Snapshot = AuthoritySnapshot;
            fn terminal_source(
                &self,
                structural: Option<&dyn StructuralAuthority>,
                prefix: TerminalPrefixMode<&AuthorityPrefixPlan>,
            ) -> Result<Self::TerminalSource, StateError> {
                let raw = self.capture()?;
                if structural.is_some_and(|s| s.tape_uuid() != raw.header.tape_uuid) {
                    return Err(invalid("foreign structural capability"));
                }
                super::terminal_source::TerminalSource::new(raw, prefix)
            }
            fn latest_commit(&self) -> Result<Option<AuthorityCommit>, StateError> {
                self.latest_report_summary()
            }
            fn visit_commits(
                &self,
                v: &mut AuthorityVisitor<'_, AuthorityCommit, StateError>,
            ) -> Result<CheckpointBoundedReplayMetrics, StateError> {
                visit_reports(&self.capture()?, v)
            }
            fn replay_commits(&mut self) -> Result<Vec<AuthorityCommit>, StateError> {
                let mut all = Vec::new();
                self.visit_commits(&mut |c| {
                    all.push(c.clone());
                    Ok(())
                })?;
                Ok(all)
            }
            fn pending_finalization(&self) -> Result<Option<AuthorityFinalization>, StateError> {
                let raw = self.capture()?;
                if raw.summary.sealed {
                    Ok(None)
                } else {
                    read_plan(&raw)
                }
            }
            fn authority_summary(&self) -> Result<AuthoritySummary, StateError> {
                self.small_summary()
            }
            fn validate_append_rows(&self, rows: &FrozenFileRows) -> Result<(), StateError> {
                validate_rows(&self.capture()?, rows)
            }
            fn freeze(&self) -> Result<AuthoritySnapshot, StateError> {
                AuthoritySnapshot::new(self.capture()?)
            }
            fn finalization_status(&self) -> Result<Option<AuthorityFinalization>, StateError> {
                read_plan(&self.capture()?)
            }
        }
    };
}
view!(TapeAuthority);
view!(TapeAuthorityLease);
fn validate_rows(raw: &Snapshot, rows: &FrozenFileRows) -> Result<(), StateError> {
    if raw.summary.finalization.is_some() || raw.summary.sealed {
        return Err(invalid(
            "terminal authority is not an ordinary append boundary",
        ));
    }
    if rows.tape_uuid() != raw.header.tape_uuid
        || rows.block_size() != raw.header.block_size
        || Some(rows.scheme()) != raw.header.scheme.as_ref()
        || rows.committed_entry_count() != raw.summary.next_file
        || rows.highest_protected_ordinal() != raw.summary.protected
        || rows.total_committed_ordinals() != raw.summary.total
    {
        return Err(invalid("structural authority disagrees with log"));
    }
    let mut actual = rows
        .replay()
        .map_err(journal)
        .map_err(|e| invalid(e.to_string()))?;
    bridge(|v| raw.visit_file_rows(v), &mut |row| {
        if actual
            .next_entry()
            .map_err(|e| invalid(e.to_string()))?
            .as_ref()
            != Some(row)
        {
            return Err(invalid("structural row differs"));
        }
        Ok(())
    })?;
    if actual
        .next_entry()
        .map_err(|e| invalid(e.to_string()))?
        .is_some()
    {
        return Err(invalid("extra structural row"));
    }
    Ok(())
}
fn push(group: &mut GroupWriter, chunk: Chunk, payload: &[u8]) -> Result<(), StoreError> {
    group.push(
        &Record::PayloadContinuation(Continuation {
            group: group.sequence(),
            chunk,
        }),
        payload,
    )
}
fn plain(file: u64, kind: TapeFileKind, blocks: u64, lba: u64) -> TapeFileEntry {
    TapeFileEntry {
        tape_file_number: file,
        kind,
        block_count: blocks,
        physical_start_hint: Some(lba),
        object_id: None,
        first_parity_data_ordinal: None,
        epoch_id: None,
        protected_ordinal_start: None,
        protected_ordinal_end_exclusive: None,
        canonical_metadata_hash: None,
        object_recovery_row: None,
    }
}
/// A bounded encoder that lets a single catalog field cross frame boundaries.
struct CatalogWriter<'a> {
    group: &'a mut GroupWriter,
    file: u64,
    ordinal: u64,
    bytes: Vec<u8>,
}
impl CatalogWriter<'_> {
    fn flush_frame(&mut self, last: bool) -> io::Result<()> {
        push(
            self.group,
            Chunk::Catalog {
                tape_file_number: self.file,
                ordinal: self.ordinal,
                last,
            },
            &self.bytes,
        )
        .map_err(io::Error::other)?;
        self.ordinal += 1;
        self.bytes.clear();
        Ok(())
    }
}
impl Write for CatalogWriter<'_> {
    fn write(&mut self, mut bytes: &[u8]) -> io::Result<usize> {
        let len = bytes.len();
        while !bytes.is_empty() {
            let n = bytes.len().min(65536 - self.bytes.len());
            self.bytes.extend_from_slice(&bytes[..n]);
            bytes = &bytes[n..];
            if self.bytes.len() == 65536 {
                self.flush_frame(false)?;
            }
        }
        Ok(len)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn publish_report(
    core: &mut Lease,
    report: &AuthorityCommit,
    mut streamed: Option<&mut ReportMembers<'_>>,
) -> Result<(), StateError> {
    if report.sealed || report.finalization.is_some() {
        return Err(unsupported("ordinary report with terminal completion"));
    }
    if report.ordinal == 0 || report.tape_uuid != core.header().tape_uuid {
        return Err(invalid("report identity"));
    }
    if (streamed.is_none() && report.check.is_some())
        || report
            .object_files
            .iter()
            .chain(report.barrier.iter())
            .any(|p| p.cursor.is_some() || p.check.is_some() || p.seal_due.is_some())
    {
        return Err(unsupported(
            "legacy report optional evidence; publish a native streaming group",
        ));
    }
    if report.scheme.is_some()
        && report
            .payloads
            .iter()
            .zip(&report.object_files)
            .any(|(object, files)| object.copy.protected_until_ordinal != Some(files.protected))
    {
        return Err(unsupported(
            "legacy Object bundle boundary not retained by its catalog projection",
        ));
    }
    core.bind_header(Header {
        tape_uuid: report.tape_uuid,
        block_size: report.block_size,
        scheme: report.scheme.clone(),
    })?;
    let s = core.summary();
    if report
        .barrier
        .as_ref()
        .is_some_and(|barrier| barrier.files.is_empty())
    {
        return Err(unsupported("empty legacy barrier bundle"));
    }
    if report.payloads.is_empty() {
        return Err(unsupported("empty legacy checkpoint report"));
    }
    let mut group = core.prepare()?;
    let mut lba = if s.is_fresh() { 2 } else { s.cursor.lba };
    if s.is_fresh() {
        group.push(
            &Record::Initialized(Initialized {
                tape_uuid: report.tape_uuid,
                incarnation: 1,
                group: group.sequence(),
                block_size: report.block_size,
                scheme: report.scheme.clone(),
                bot: plain(0, TapeFileKind::Bootstrap, 1, 0),
            }),
            &[],
        )?;
    }
    if report.scheme.is_some() {
        crate::authority::validation::validate_parity_barrier_bundles(report)
            .map_err(|e| invalid(e.to_string()))?;
    } else if !report.object_files.is_empty() || report.barrier.is_some() {
        return Err(invalid(
            "parity-off report contains structural publications",
        ));
    }
    if streamed.is_some() && report.payloads.iter().any(|p| !p.files.is_empty()) {
        return Err(invalid("streamed report also contains owned members"));
    }
    let mut members = 0u64;
    let mut next_epoch = s.next_epoch;
    for (index, payload) in report.payloads.iter().enumerate() {
        let rows = report.object_files.get(index);
        let row = if let Some(rows) = rows {
            rows.files
                .first()
                .ok_or_else(|| invalid("empty Object files"))?
                .clone()
        } else {
            let mut row = plain(
                payload.copy.tape_file_number,
                TapeFileKind::Object,
                payload.block_count,
                lba,
            );
            row.object_id = Some(payload.object.object_id.clone());
            row.object_recovery_row = Some(payload.object_recovery_row.clone());
            row
        };
        lba = row
            .physical_start_hint
            .ok_or_else(|| invalid("missing physical Object start"))?
            .checked_add(row.block_count)
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| invalid("Object end overflow"))?;
        push(&mut group, Chunk::FileRow(row), &[])?;
        let mut stream = CatalogWriter {
            group: &mut group,
            file: payload.copy.tape_file_number,
            ordinal: 0,
            bytes: Vec::with_capacity(65536),
        };
        payload
            .projection
            .put(&mut stream)
            .map_err(StoreError::from)?;
        let mut emit = |member: &crate::NativeObjectFileProjectionInput| -> Result<(), StateError> {
            super::payload::encode_member(&mut stream, &payload.projection, member)
                .map_err(StoreError::from)?;
            members = members
                .checked_add(1)
                .ok_or_else(|| invalid("member count overflow"))?;
            Ok(())
        };
        if let Some(source) = streamed.as_mut() {
            source(index, &mut emit)?;
        } else {
            for member in &payload.files {
                emit(member)?;
            }
        }
        stream.flush_frame(true).map_err(StoreError::from)?;
        if let Some(rows) = rows {
            for row in rows.files.iter().skip(1) {
                if let Some(epoch) = row.epoch_id {
                    next_epoch = epoch
                        .checked_add(1)
                        .ok_or_else(|| invalid("epoch overflow"))?;
                }
                push(&mut group, Chunk::FileRow(row.clone()), &[])?;
            }
        }
    }
    if let Some(barrier) = &report.barrier {
        for row in &barrier.files {
            if let Some(epoch) = row.epoch_id {
                next_epoch = epoch
                    .checked_add(1)
                    .ok_or_else(|| invalid("epoch overflow"))?;
            }
            push(&mut group, Chunk::FileRow(row.clone()), &[])?;
        }
    }
    // Streaming semantic reports carry explicit evidence. Legacy optional
    // evidence still has a typed refusal; an absent check cannot prove [W,T).
    if report.check.is_none() && report.scheme.is_some() && report.protected != report.total {
        return Err(unsupported(
            "uncovered legacy report without check evidence",
        ));
    }
    let (check, provenance) = report
        .check
        .unwrap_or((CheckValue::EMPTY, CheckProvenance::Empty));
    let c = Committed {
        group: group.reference(members),
        cursor: report.cursor,
        protected: report.protected,
        total: report.total,
        check,
        provenance,
        unverified_end: (provenance == CheckProvenance::UnverifiedRebuild).then_some(report.total),
        next_epoch,
        next_file: report.next_file,
        object_count: report.object_count,
        batch_id: report.operation_id,
        session_id: None,
        seal_due: report.seal_due,
    };
    core.append(group.close(&Record::Committed(c))?)?;
    Ok(())
}
fn append_plan(
    core: &mut Lease,
    plan: &AuthorityFinalization,
    prepared: bool,
) -> Result<(), StateError> {
    plan.validate_for_tape(core.header().tape_uuid)?;
    if plan.terminal_prefix.as_ref().is_some_and(|p| {
        p.publication.cursor.is_some()
            || p.publication.check.is_some()
            || p.publication.seal_due.is_some()
    }) {
        return Err(unsupported(
            "legacy prefix optional evidence; use a native plan group",
        ));
    }
    let mut group = core.prepare()?;
    if let Some(prefix) = &plan.terminal_prefix {
        for row in &prefix.publication.files {
            push(&mut group, Chunk::FileRow(row.clone()), &[])?;
        }
        for row in &prefix.sidecar_directory_entries {
            push(&mut group, Chunk::PlanDirectory(row.clone()), &[])?;
        }
    }
    let plan = Box::new(FinalizationPlan {
        group: group.reference(0),
        trigger: plan.trigger,
        manual: plan.manual.clone(),
        edition_id: plan.edition_id,
        edition_sequence: plan.edition_sequence,
        edition_digest: plan.edition_digest,
        writer_version: plan.writer_version.clone(),
        write_timestamp: plan.write_timestamp.clone(),
        layout: plan.layout.try_to_parity_layout()?,
        prefix_end: CommitCursor {
            partition: plan.layout.partition,
            lba: plan.layout.components[0].start_lba,
        },
    });
    let record = if prepared {
        Record::FinalizationPrepared(plan)
    } else {
        Record::FinalizationDecided(plan)
    };
    core.append(group.close(&record)?)?;
    Ok(())
}
fn accept(core: &mut Lease) -> Result<(), StateError> {
    if core
        .summary()
        .finalization
        .as_ref()
        .is_some_and(|p| p.prepared)
    {
        let plan = read_plan(&core.snapshot()?)?.ok_or_else(|| invalid("no plan"))?;
        append_plan(core, &plan, false)?;
    }
    Ok(())
}
fn component(
    plan: &AuthorityFinalization,
    index: usize,
    protected: u64,
    total: u64,
) -> Result<FilePublication, StateError> {
    let c = plan.layout.components[index];
    let (kind, hash) = match c.kind {
        crate::authority::TerminalFinalizationComponentKind::TapeIndexReplica => {
            (TapeFileKind::TapeIndexReplica, plan.edition_digest)
        }
        crate::authority::TerminalFinalizationComponentKind::IndexSeparationExtent => {
            let separation = remanence_parity::plan_index_separation(
                remanence_parity::IndexSeparationDescriptor {
                    tape_uuid: plan.tape_uuid,
                    edition_id: plan.edition_id,
                    gap_ordinal: c.ordinal,
                    block_size: plan.layout.block_size,
                    nominal_extent_bytes: remanence_parity::DEFAULT_INDEX_SEPARATION_BYTES,
                    total_records: c.record_count,
                    compression_enabled: false,
                    terminal_layout: plan.layout.try_to_parity_layout()?,
                },
            )
            .map_err(|error| invalid(error.to_string()))?;
            (
                TapeFileKind::IndexSeparationExtent,
                separation.descriptor_digest,
            )
        }
    };
    let mut row = plain(c.tape_file_number, kind, c.record_count, c.start_lba);
    row.canonical_metadata_hash = Some(hash);
    Ok(FilePublication {
        purpose: FilePurpose::TerminalComponent,
        files: vec![row],
        protected,
        total,
        cursor: None,
        check: None,
        seal_due: None,
    })
}

fn receipt(publication: &FilePublication, existing: bool) -> PublicationReceipt {
    let d = if existing {
        PublicationDisposition::AlreadyPresent
    } else {
        PublicationDisposition::Appended
    };
    PublicationReceipt {
        items: vec![PublishedItem {
            publication: publication.clone(),
            disposition: d,
            durability: d,
        }],
    }
}
fn terminal(
    core: &mut Lease,
    publication: &FilePublication,
    prefix: bool,
) -> Result<PublicationReceipt, StateError> {
    let expected = if prefix {
        FilePurpose::TerminalPrefix
    } else {
        FilePurpose::TerminalComponent
    };
    if publication.purpose != expected {
        return Err(invalid(if prefix {
            "invalid terminal-prefix/checkpoint transition"
        } else {
            "invalid terminal-component/checkpoint transition"
        }));
    }
    let plan = read_plan(&core.snapshot()?)?
        .ok_or_else(|| invalid("terminal record before first Committed or missing plan"))?;
    let state = core
        .summary()
        .finalization
        .as_ref()
        .ok_or_else(|| invalid("missing plan"))?;
    let (index, already) = if prefix {
        let expected = plan
            .terminal_prefix
            .as_ref()
            .ok_or_else(|| invalid("parity-off has no terminal prefix"))?;
        if publication != &expected.publication {
            return Err(invalid("terminal prefix differs from plan"));
        }
        (0, state.prefix_written)
    } else {
        let index = plan
            .layout
            .components
            .iter()
            .position(|c| {
                publication
                    .files
                    .first()
                    .is_some_and(|r| r.tape_file_number == c.tape_file_number)
            })
            .ok_or_else(|| invalid("component outside plan"))?;
        if publication != &component(&plan, index, core.summary().protected, core.summary().total)?
        {
            return Err(invalid("terminal component differs from plan"));
        }
        (index, index < usize::from(state.components_written))
    };
    if already {
        return Ok(receipt(publication, true));
    }
    accept(core)?;
    let s = core.summary();
    let decision = s.finalization.as_ref().expect("accepted").sequence;
    let mut group = core.prepare()?;
    let mut next_epoch = s.next_epoch;
    for row in &publication.files {
        if let Some(epoch) = row.epoch_id {
            next_epoch = epoch
                .checked_add(1)
                .ok_or_else(|| invalid("epoch overflow"))?;
        }
        push(&mut group, Chunk::FileRow(row.clone()), &[])?;
    }
    let (cursor, next_file) = publication
        .files
        .last()
        .map_or((s.cursor, s.next_file), |r| {
            (
                CommitCursor {
                    partition: s.cursor.partition,
                    lba: r.physical_start_hint.unwrap_or(0) + r.block_count + 1,
                },
                r.tape_file_number + 1,
            )
        });
    let (check, provenance, unverified_end) =
        if prefix && publication.protected == publication.total {
            (CheckValue::EMPTY, CheckProvenance::Empty, None)
        } else {
            (s.check, s.provenance, s.unverified_end)
        };
    let commit = Committed {
        group: group.reference(0),
        cursor,
        protected: publication.protected,
        total: publication.total,
        check,
        provenance,
        unverified_end,
        next_epoch,
        next_file,
        object_count: s.object_count,
        batch_id: plan.edition_id,
        session_id: None,
        seal_due: s.seal_due,
    };
    let record = if prefix {
        Record::TerminalPrefix { decision, commit }
    } else {
        Record::ComponentWritten {
            decision,
            component: index as u8,
            commit,
        }
    };
    core.append(group.close(&record)?)?;
    Ok(receipt(publication, false))
}
/// Bounded recovery authority; callers stream rows rather than owning history.
#[derive(Debug)]
pub struct Recovery {
    /// Frozen durable prefix.
    pub snapshot: AuthoritySnapshot,
    /// Pending plan, absent after Sealed.
    pub finalization: Option<AuthorityFinalization>,
}
impl TapeAuthorityWriter for TapeAuthorityLease {
    type Report = AuthorityCommit;
    type Progress = Progress;
    type Recovery = Recovery;
    fn publish_commit(&mut self, p: &FilePublication) -> Result<PublicationReceipt, JournalError> {
        publish_physical(&mut *self.core().map_err(journal)?, p)
    }
    fn publish_terminal_prefix(
        &mut self,
        p: &FilePublication,
    ) -> Result<PublicationReceipt, JournalError> {
        terminal(&mut *self.core().map_err(publication_error)?, p, true).map_err(publication_error)
    }
    fn publish_terminal_component(
        &mut self,
        p: &FilePublication,
    ) -> Result<PublicationReceipt, JournalError> {
        terminal(&mut *self.core().map_err(publication_error)?, p, false).map_err(publication_error)
    }
    fn recover_finalization(&mut self) -> Result<Recovery, StateError> {
        let snapshot = self.freeze()?;
        if snapshot.finalization.is_none() {
            return Err(invalid("no terminal finalization to recover"));
        }
        let finalization = if snapshot.summary.sealed {
            None
        } else {
            snapshot.finalization.clone()
        };
        Ok(Recovery {
            snapshot,
            finalization,
        })
    }
    fn publish_report(&mut self, r: &AuthorityCommit) -> Result<(), StateError> {
        publish_report(&mut *self.core()?, r, None)
    }
    fn publish_reports(&mut self, r: &[AuthorityCommit]) -> Result<(), StateError> {
        match r {
            [] => Ok(()),
            [one] => self.publish_report(one),
            _ => Err(unsupported("legacy multi-report transaction")),
        }
    }
    fn publish_sealed_reports<F: FnOnce() -> Result<(), StateError>>(
        &mut self,
        _reports: &[AuthorityCommit],
        _after: F,
    ) -> Result<(), StateError> {
        // Sealed is a terminal decision binding, not another legacy checkpoint.
        // Its format has no checkpoint batch identity or structural bundle list.
        Err(unsupported(
            "legacy sealed checkpoint report; use seal after ComponentWritten",
        ))
    }
    fn lease_tape_uuid(&self) -> [u8; 16] {
        self.tape_uuid()
    }
    fn begin_finalization(
        &mut self,
        plan: &AuthorityFinalization,
    ) -> Result<AuthorityFinalization, StateError> {
        let mut core = self.core()?;
        if let Some(existing) = read_plan(&core.snapshot()?)? {
            if existing == *plan {
                return Ok(existing);
            }
            return Err(invalid("immutable plan differs"));
        }
        if plan.progress != Progress::BeforeReplicaA || plan.recovery_required {
            return Err(invalid("new plan has progress"));
        }
        append_plan(&mut core, plan, plan.manual.is_some())?;
        Ok(plan.clone())
    }
    fn advance_finalization(
        &mut self,
        expected: Progress,
        next: Progress,
    ) -> Result<AuthorityFinalization, StateError> {
        if expected.successor() != Some(next) {
            return Err(invalid("invalid finalization successor"));
        }
        let mut core = self.core()?;
        let plan = read_plan(&core.snapshot()?)?.ok_or_else(|| invalid("no plan"))?;
        if plan.progress == next {
            return Ok(plan);
        }
        if plan.progress != expected {
            return Err(invalid("finalization progress differs"));
        }
        let publication = component(
            &plan,
            usize::from(count(expected)),
            core.summary().protected,
            core.summary().total,
        )?;
        terminal(&mut core, &publication, false)?;
        read_plan(&core.snapshot()?)?.ok_or_else(|| invalid("missing updated plan"))
    }
    fn require_finalization_recovery(&mut self) -> Result<AuthorityFinalization, StateError> {
        let mut core = self.core()?;
        let state = core
            .summary()
            .finalization
            .as_ref()
            .ok_or_else(|| invalid("no plan"))?;
        if !state.recovery_required {
            let record = Record::FinalizationRecoveryRequired {
                plan: state.sequence,
            };
            let group = core.prepare()?;
            core.append(group.close(&record)?)?;
        }
        read_plan(&core.snapshot()?)?.ok_or_else(|| invalid("no plan"))
    }
    fn retire_preparation(
        &mut self,
        expected: &AuthorityFinalization,
        catalog: bool,
        binding: bool,
    ) -> Result<(), StateError> {
        let mut core = self.core()?;
        if catalog
            || binding
            || expected.manual.is_none()
            || expected.progress != Progress::BeforeReplicaA
            || expected.recovery_required
            || read_plan(&core.snapshot()?)?.as_ref() != Some(expected)
        {
            return Err(invalid("manual retirement guard refused"));
        }
        let prepared = core
            .summary()
            .finalization
            .as_ref()
            .ok_or_else(|| invalid("no preparation"))?
            .sequence;
        let group = core.prepare()?;
        core.append(group.close(&Record::FinalizationPreparationRetired { prepared })?)?;
        Ok(())
    }
}
fn publish_physical(
    _core: &mut Lease,
    p: &FilePublication,
) -> Result<PublicationReceipt, JournalError> {
    match p.purpose {
        FilePurpose::TerminalPrefix | FilePurpose::TerminalComponent => Err(JournalError::Codec(
            "terminal publication requires its explicit operation".into(),
        )),
        FilePurpose::Checkpoint => Err(JournalError::UnsupportedOperation(
            "empty Checkpoint watermark; B3b removes sink commits",
        )),
        _ => Err(JournalError::UnsupportedOperation(
            "separate physical publication without catalog; use a complete report",
        )),
    }
}
#[derive(Debug)]
struct Structural {
    core: Weak<Mutex<Lease>>,
    uuid: [u8; 16],
}
impl Structural {
    fn with<R>(
        &self,
        operation: impl FnOnce(&mut Lease) -> Result<R, JournalError>,
    ) -> Result<R, JournalError> {
        let core = self
            .core
            .upgrade()
            .ok_or_else(|| journal("authority lease ended"))?;
        let mut core = core
            .lock()
            .map_err(|_| journal("authority owner panicked"))?;
        operation(&mut core)
    }
    fn snapshot(&self) -> Result<Snapshot, JournalError> {
        self.with(|c| c.snapshot().map_err(journal))
    }
}
impl TapeReportPublisher for Structural {
    fn tape_uuid(&self) -> [u8; 16] {
        self.uuid
    }
    fn publish_commit(&mut self, p: &FilePublication) -> Result<PublicationReceipt, JournalError> {
        self.with(|c| publish_physical(c, p))
    }
    fn publish_terminal_prefix(
        &mut self,
        p: &FilePublication,
    ) -> Result<PublicationReceipt, JournalError> {
        self.with(|c| terminal(c, p, true).map_err(publication_error))
    }
    fn publish_terminal_component(
        &mut self,
        p: &FilePublication,
    ) -> Result<PublicationReceipt, JournalError> {
        self.with(|c| terminal(c, p, false).map_err(publication_error))
    }
    fn committed_snapshot_bounded_authority(&self) -> Result<FrozenFileRows, JournalError> {
        self.freeze()
    }
    fn session_terminal_resume_snapshot(
        &self,
        p: &FilePublication,
    ) -> Result<Option<FrozenFileRows>, JournalError> {
        self.freeze_before_terminal_prefix(p).map(Some)
    }
}
impl StructuralAuthority for Structural {
    #[cfg(any(test, feature = "fixtures"))]
    fn fixture_publications(&self) -> Result<Vec<FilePublication>, JournalError> {
        Err(JournalError::UnsupportedOperation(
            "dual-journal fixture units",
        ))
    }
    fn freeze(&self) -> Result<FrozenFileRows, JournalError> {
        let raw = self.snapshot()?;
        if raw.header.scheme.is_none() {
            return Err(journal("parity-off structural cursor"));
        }
        Ok(FrozenFileRows::new(Rows(raw)))
    }
    fn freeze_before_terminal_prefix(
        &self,
        p: &FilePublication,
    ) -> Result<FrozenFileRows, JournalError> {
        let raw = self.snapshot()?;
        let plan = read_plan(&raw)
            .map_err(journal)?
            .ok_or_else(|| journal("missing plan"))?;
        if plan.terminal_prefix.as_ref().map(|p| &p.publication) != Some(p) {
            return Err(journal("prefix differs"));
        }
        let start = raw
            .summary
            .finalization
            .as_ref()
            .expect("plan")
            .reference
            .first_sequence;
        Ok(FrozenFileRows::new(Rows(
            raw.before(start).map_err(journal)?,
        )))
    }
    fn freeze_terminal_prefix(&self) -> Result<FrozenFileRows, JournalError> {
        let raw = self.snapshot()?;
        if !raw
            .summary
            .finalization
            .as_ref()
            .is_some_and(|p| p.prefix_written && p.components_written == 0)
        {
            return Err(journal("not at durable terminal prefix"));
        }
        Ok(FrozenFileRows::new(Rows(raw)))
    }
    fn prefix_is_durable(&self, p: &FilePublication) -> Result<bool, JournalError> {
        let raw = self.snapshot()?;
        let plan = read_plan(&raw).map_err(journal)?;
        Ok(plan
            .as_ref()
            .and_then(|p| p.terminal_prefix.as_ref())
            .is_some_and(|plan| &plan.publication == p)
            && raw
                .summary
                .finalization
                .as_ref()
                .is_some_and(|p| p.prefix_written))
    }
    fn component_relation(
        &self,
        completed: u8,
        expected: &[FilePublication],
    ) -> Result<remanence_parity::TerminalComponentAuthorityRelation, JournalError> {
        let raw = self.snapshot()?;
        let plan = read_plan(&raw)
            .map_err(journal)?
            .ok_or_else(|| journal("missing plan"))?;
        if count(plan.progress) != completed || expected.len() != 5 {
            return Err(journal("component authority differs"));
        }
        for (index, publication) in expected.iter().enumerate() {
            if publication
                != &component(&plan, index, raw.summary.protected, raw.summary.total)
                    .map_err(publication_error)?
            {
                return Err(journal("component authority differs"));
            }
        }
        Ok(remanence_parity::TerminalComponentAuthorityRelation::Aligned)
    }
    fn pending_count(&self) -> usize {
        0
    }
    fn discard_suffix(&mut self, next_file: u64) -> Result<(), JournalError> {
        self.with(|c| {
            if next_file == c.summary().next_file {
                Ok(())
            } else {
                Err(journal("cannot discard durable authority"))
            }
        })
    }
    fn discard_pending(&mut self, expected: &[FilePublication]) -> Result<(), JournalError> {
        if expected.is_empty() {
            Ok(())
        } else {
            Err(journal("one-log authority has no pending physical units"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::tests::{finalization_intent, record, second_record};
    #[test]
    fn new_authority_seeded_structural_contract() {
        let dir = tempfile::tempdir().unwrap();
        let id = [0x39; 16];
        let (record, plan, _) = crate::authority::contract_tests::structural_fixture(id);
        let plan: AuthorityFinalization = (&plan).into();
        let prefix = plan.terminal_prefix.as_ref().unwrap().publication.clone();
        let authority = TapeAuthority::open_fixture(dir.path(), id).unwrap();
        let mut writer = authority.acquire_exclusive().unwrap();
        writer.publish_report(&(&record).into()).unwrap();
        writer.begin_finalization(&plan).unwrap();
        crate::authority::contract_tests::structural_writer_contract(&mut writer, prefix);
    }
    #[test]
    fn new_authority_snapshot_agreement_contract() {
        let dir = tempfile::tempdir().unwrap();
        let id = [0x39; 16];
        let (record, plan, _) = crate::authority::contract_tests::structural_fixture(id);
        let authority = TapeAuthority::open_fixture(dir.path(), id).unwrap();
        let mut writer = authority.acquire_exclusive().unwrap();
        writer.publish_report(&(&record).into()).unwrap();
        crate::authority::contract_tests::snapshot_agreement_contract(&mut writer, (&plan).into());
    }
    #[test]
    fn new_authority_view_writer_contract() {
        let dir = tempfile::tempdir().unwrap();
        let id = [0x31; 16];
        let authority = TapeAuthority::open_fixture(dir.path(), id).unwrap();
        let mut writer = authority.acquire_exclusive().unwrap();
        crate::authority::contract_tests::view_writer_contract(
            &mut writer,
            (&record(id)).into(),
            (&second_record(id)).into(),
            |sequence| match sequence {
                4 => 1,
                7 => 2,
                other => panic!("unexpected closing sequence: {other}"),
            },
        );
    }
    #[test]
    fn new_authority_finalization_contract() {
        let dir = tempfile::tempdir().unwrap();
        let id = [0x32; 16];
        let authority = TapeAuthority::open_fixture(dir.path(), id).unwrap();
        let mut writer = authority.acquire_exclusive().unwrap();
        writer.publish_report(&(&record(id)).into()).unwrap();
        crate::authority::contract_tests::finalization_contract(
            &mut writer,
            (&finalization_intent(id)).into(),
        );
    }
    #[test]
    fn new_authority_refuses_terminal_first() {
        let dir = tempfile::tempdir().unwrap();
        let id = [0x32; 16];
        let authority = TapeAuthority::open_fixture(dir.path(), id).unwrap();
        let mut writer = authority.acquire_exclusive().unwrap();
        let p = FilePublication {
            purpose: FilePurpose::TerminalPrefix,
            files: Vec::new(),
            protected: 0,
            total: 0,
            cursor: None,
            check: None,
            seal_due: None,
        };
        assert!(writer.publish_terminal_prefix(&p).is_err());
        assert!(writer.authority_summary().unwrap().cursor.is_none());
    }
}

#[cfg(test)]
#[path = "authority_tests.rs"]
mod additional_tests;
