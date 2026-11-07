//! Shared production ownership of typed library authority and its projection.
//! Producers call LibraryAuthority methods with a frozen native view. Durable
//! applications are delivered in fold order after all append/view locks drop;
//! a refused candidate still forwards AppliedBeforeRefusal. Storage failures
//! latch motion for this process, independently of later native session endings.
use crate::{
    authority_manifest::AuthorityManifest,
    library_log::{
        self as lib,
        native_view::NativeTapeView,
        store::{Clock, Origin, StoreError, UuidSource},
        AuthorityError, AuthorityReceipt, LibraryAuthority, TapeView,
    },
    projector::{
        apply::{ApplyError, ApplyPath, AuthorityRecord},
        thread::{ProjectionClient, ProjectionResult},
    },
    StateError, StateLockGuard, StatePaths,
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

/// Error text of the 32-L1 refusal of a tape-log tail repair. The text starts with the code.
pub const TAIL_REPAIR_DEFERRED: &str =
    "TailRepairDeferred: repairing a tape-log tail is deferred; nothing was repaired or changed";

/// Typed errors retain the distinction between semantic refusal and authority
/// storage failure needed by continuation-LOAD compensation.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// Library append refused or failed; inspect this cause before compensating.
    #[error(transparent)]
    Authority(#[from] AuthorityError),
    /// Durable authority exists but SQL catch-up failed.
    #[error(transparent)]
    Projection(#[from] ApplyError),
    /// Namespace, lease, worker or native evidence failure.
    #[error(transparent)]
    State(#[from] StateError),
    /// A prior storage failure forbids all subsequent motion in this process.
    #[error("library authority failed; motion is fenced until startup reconciliation")]
    MotionFenced,
    /// Both results matter: never hide an authority storage failure behind SQL.
    #[error("{authority}; projecting its applied prefix also failed: {projection}")]
    AuthorityAndProjection {
        /// Original typed append failure.
        authority: AuthorityError,
        /// Failure forwarding the durable applied prefix.
        projection: Box<RuntimeError>,
    },
}
/// Ruling 34's verification modes, one bit per check. A bit records that the check ran,
/// not what it found; the mask is nonzero and bits 7 to 31 are zero.
pub mod verification_modes {
    /// The five terminal components (replicas A, B and C, separations AB and BC) were validated.
    pub const TERMINAL_INDEX: u32 = 1 << 0;
    /// Every physical prefix row was read, and `verified_prefix_record_count` is present.
    pub const PHYSICAL_PREFIX: u32 = 1 << 1;
    /// The REM-PARITY 2.2 protected-content walk ran.
    pub const PROTECTED_CONTENT: u32 = 1 << 2;
    /// `replica_health` was evaluated: no replica entry is a placeholder.
    pub const REPLICAS: u32 = 1 << 3;
    /// `separation_health` was evaluated.
    pub const SEPARATIONS: u32 = 1 << 4;
    /// `measured_eod_lba` came from the drive.
    pub const EOD_MEASURED: u32 = 1 << 5;
    /// `verification_basis` is `bot_structural_recovery` and `recovery_inventory` is attached.
    pub const BOT_RECOVERY: u32 = 1 << 6;
}

/// What a verification's producer decides about its verdict. The runtime adds
/// the operation and the tape identity under its dispatch lease
/// (`LibraryRuntime::record_verification_verdict`); it records the native
/// reference it is given and never reads one itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerdictFacts {
    /// The tape's native reference as it stood when the verification ran, captured while
    /// the verification still held the tape (D-2b-3). `None` means the tape had no native
    /// reference then; a write after the capture does not change it (D-W-7).
    pub native_reference: Option<lib::TapeAt>,
    /// Terminal outcome, one of proto states 2, 3 and 4.
    pub outcome: lib::VerificationOutcome,
    /// The unprotected tail (`w < t`), when the verification's own result gives one.
    pub unprotected: Option<lib::Unprotected>,
    /// Checks that ran: the nonzero seven-bit mask of ruling 34.
    pub modes: u32,
    /// Digest of the verified terminal edition, absent for `RecoveryRequired`.
    pub edition_digest: Option<lib::Hash>,
}
/// What a recorded refusal (`LibraryRuntime::record_refusal`) is about, as the `Diagnostic`
/// record names it. `subject_kind` is the variant's lower-case name, and `subject_id` is the
/// hyphenated UUID of the thing named, when the request names one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusalSubject {
    /// A tape, when the request names one (`tape`).
    Tape(Option<lib::Id>),
    /// A drive (`drive`).
    Drive(lib::Id),
    /// An operation (`operation`).
    Operation(lib::Id),
    /// The library as a whole, with no identifier: `library` and no `subject_id`.
    Library,
}
impl RefusalSubject {
    fn fields(self) -> (&'static str, Option<lib::Id>) {
        match self {
            Self::Tape(id) => ("tape", id),
            Self::Drive(id) => ("drive", Some(id)),
            Self::Operation(id) => ("operation", Some(id)),
            Self::Library => ("library", None),
        }
    }
}
struct WallClock;
impl Clock for WallClock {
    fn now(&self) -> u64 {
        u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos()).unwrap_or(0)
    }
}
struct FreshUuids;
impl UuidSource for FreshUuids {
    fn next(&self, _: u64) -> lib::Id {
        *uuid::Uuid::new_v4().as_bytes()
    }
}

struct NativeIdentityView(NativeTapeView);
impl lib::authority::FinalizationView for NativeIdentityView {
    fn committed_instance_count(&self, tape: lib::TapeRef) -> u64 {
        self.0
            .snapshots()
            .find(|s| s.header().tape_uuid == tape.uuid)
            .map_or(0, |s| s.summary().object_count)
    }
    fn finalization_blocks_change(&self, tape: lib::TapeRef) -> bool {
        self.0
            .snapshots()
            .find(|s| s.header().tape_uuid == tape.uuid)
            .is_some_and(|s| {
                let summary = s.summary();
                summary.seal_due.is_some() || summary.finalization.is_some() && !summary.sealed
            })
    }
}

/// Cloneable process-lifetime authority capability. All peers share one writer,
/// native view and sticky motion fence; it does not open an audit writer.
#[derive(Clone)]
pub struct LibraryRuntime(Arc<Inner>);
struct Inner {
    authority: LibraryAuthority,
    view: Mutex<NativeTapeView>,
    // This dispatch lock orders append receipts and projection submissions; it
    // is not a tape lock or LibraryAuthority's append lock.
    dispatch: Mutex<()>,
    projector: ProjectionClient,
    manifest: AuthorityManifest,
    motion_fenced: AtomicBool,
    projection_dirty: AtomicBool,
    #[cfg(any(test, feature = "fixtures"))]
    source_projection_lag: Mutex<Vec<std::time::Duration>>,
    #[cfg(test)]
    native_projection_waiters: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    native_projection_high_water: std::sync::atomic::AtomicUsize,
    ack_progress: Mutex<std::collections::BTreeMap<lib::Id, (u64, u64)>>,
    #[cfg(test)]
    ack_frames_checked: std::sync::atomic::AtomicUsize,
}
impl std::fmt::Debug for LibraryRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LibraryRuntime")
            .field("manifest", &self.0.manifest)
            .field("motion_fenced", &self.motion_fenced())
            .finish_non_exhaustive()
    }
}
fn store_failure(error: &AuthorityError) -> bool {
    match error {
        AuthorityError::AppliedBeforeRefusal { error, .. } => store_failure(error),
        AuthorityError::Store(
            StoreError::Fold(_)
            | StoreError::FinalizationInProgress(_)
            | StoreError::TapeHoldsCommittedInstances { .. },
        ) => false,
        AuthorityError::Store(_) => true,
        _ => false,
    }
}
impl LibraryRuntime {
    /// Admit the bound namespaces under an existing state lease, probe every
    /// native log before library replay, then start the projector. No SQL or
    /// AuditEvent is used to manufacture library identities.
    pub fn open(
        paths: &StatePaths,
        fsync_diagnostics: bool,
        lease: Arc<StateLockGuard>,
    ) -> Result<Self, RuntimeError> {
        let manifest = AuthorityManifest::admit(paths, &lease)?;
        Self::open_bound_manifest(paths, fsync_diagnostics, lease, manifest)
    }

    /// The reset owner supplies its receipt-pinned manifest while the startup
    /// fence is still held. This cannot be called by external producers.
    pub(crate) fn open_bound_manifest(
        paths: &StatePaths,
        fsync_diagnostics: bool,
        lease: Arc<StateLockGuard>,
        manifest: AuthorityManifest,
    ) -> Result<Self, RuntimeError> {
        let view = NativeTapeView::probe(manifest.tape_root(), &paths.sqlite_path)
            .map_err(|e| StateError::JournalReplayFailed(e.to_string()))?;
        let host = std::fs::read_to_string("/etc/machine-id")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "unknown-host".into());
        let origin = Origin {
            host: host.trim().into(),
            pid: std::process::id(),
            build: env!("CARGO_PKG_VERSION").into(),
        };
        let authority = LibraryAuthority::open_bound(
            manifest.library_root(),
            manifest.library_uuid(),
            origin,
            Box::new(WallClock),
            Box::new(FreshUuids),
            fsync_diagnostics,
            &view,
            lease,
        )?;
        let snapshot = authority.snapshot()?;
        lib::session_ack::validate(&view, |applied| snapshot.replay(&view, applied))?;
        let projector = ProjectionClient::open(&paths.sqlite_path)?;
        projector.project_library(snapshot, Box::new(view.clone()), ApplyPath::Startup)?;
        // Library identities/assignments precede native sessions and terminal
        // state. Every native prefix was validated before either replay starts.
        for native in view.snapshots() {
            let snapshot = crate::tape_log::authority::AuthoritySnapshot::new(native)?;
            if let ProjectionResult::Failed { error, .. } = projector.project(
                AuthorityRecord::Tape(Box::new(snapshot)),
                ApplyPath::Startup,
            )? {
                return Err(error.into());
            }
        }
        let ack_progress = view
            .snapshots()
            .map(|s| {
                (
                    s.header().tape_uuid,
                    (s.summary().last_closed_sequence, s.summary().closed_end),
                )
            })
            .collect();
        Ok(Self(Arc::new(Inner {
            #[cfg(test)]
            native_projection_waiters: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            native_projection_high_water: std::sync::atomic::AtomicUsize::new(0),
            ack_progress: Mutex::new(ack_progress),
            #[cfg(test)]
            ack_frames_checked: std::sync::atomic::AtomicUsize::new(0),
            authority,
            view: Mutex::new(view),
            dispatch: Mutex::new(()),
            projector,
            manifest,
            motion_fenced: AtomicBool::new(false),
            projection_dirty: AtomicBool::new(false),
            #[cfg(any(test, feature = "fixtures"))]
            source_projection_lag: Mutex::new(Vec::new()),
        })))
    }

    /// Execute one typed writer call and forward its applications even when its
    /// candidate is refused. Never hold the native view or append lock while
    /// waiting for SQL. The dispatch lock preserves concurrent producer order.
    pub fn append(
        &self,
        write: impl FnOnce(&LibraryAuthority, &dyn TapeView) -> Result<AuthorityReceipt, AuthorityError>,
    ) -> Result<lib::Receipt, RuntimeError> {
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        self.append_locked(write)
    }

    /// Offline identity producers already own the exclusive state lease. Their
    /// finalization guard freezes native evidence under the same retained tape
    /// admission leases as the identity append. Dispatch alone is insufficient.
    pub(crate) fn append_identity(
        &self,
        owners: &[&crate::TapeAuthorityLease],
        write: impl FnOnce(
            &LibraryAuthority,
            &dyn TapeView,
            &dyn lib::authority::FinalizationView,
        ) -> Result<AuthorityReceipt, AuthorityError>,
    ) -> Result<lib::Receipt, RuntimeError> {
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        use crate::authority::TapeAuthorityView;
        let view = {
            let mut view = self.0.view.lock().unwrap_or_else(|e| e.into_inner());
            for owner in owners {
                view.replace_authoritative(owner.freeze()?.raw)
                    .map_err(|e| StateError::JournalReplayFailed(e.to_string()))?;
            }
            view.clone()
        };
        self.append_locked(|a, v| write(a, v, &NativeIdentityView(view)))
    }

    fn append_locked(
        &self,
        write: impl FnOnce(&LibraryAuthority, &dyn TapeView) -> Result<AuthorityReceipt, AuthorityError>,
    ) -> Result<lib::Receipt, RuntimeError> {
        if self.0.projection_dirty.load(Ordering::Acquire) {
            self.catch_up_locked()?;
        }
        let result = {
            let view = self.0.view.lock().unwrap_or_else(|e| e.into_inner());
            write(&self.0.authority, &*view)
        };
        #[cfg(any(test, feature = "fixtures"))]
        let durable_return = std::time::Instant::now();
        if result.as_ref().err().is_some_and(store_failure) {
            self.0.motion_fenced.store(true, Ordering::Release);
        }
        let applied = match &result {
            Ok(receipt) => receipt.applied.as_slice(),
            Err(AuthorityError::AppliedBeforeRefusal { applied, .. }) => applied.as_slice(),
            Err(_) => &[],
        };
        // Dirty before channel submission: any failed or lost response requires
        // replay of the durable prefix before the next producer can append.
        self.0.projection_dirty.store(true, Ordering::Release);
        let projection = (|| -> Result<(), RuntimeError> {
            for record in applied {
                match self.0.projector.project(
                    AuthorityRecord::Library(Box::new(record.clone())),
                    ApplyPath::Live,
                )? {
                    ProjectionResult::Applied(_) | ProjectionResult::Pending(_) => (),
                    ProjectionResult::Failed { error, .. } => return Err(error.into()),
                }
            }
            // No application may mean exclusion/contradiction, or resolution
            // without a new application. Publish the fold's status as well.
            if applied.is_empty() && result.is_ok() {
                self.catch_up_locked()?;
            }
            Ok(())
        })();
        #[cfg(any(test, feature = "fixtures"))]
        if result.is_ok() && !applied.is_empty() && projection.is_ok() {
            self.0
                .source_projection_lag
                .lock()
                .unwrap()
                .push(durable_return.elapsed());
        }
        match (result, projection) {
            (Ok(receipt), Ok(())) => {
                self.0.projection_dirty.store(false, Ordering::Release);
                Ok(receipt.receipt)
            }
            (Err(error), Ok(())) => {
                self.0.projection_dirty.store(false, Ordering::Release);
                Err(error.into())
            }
            (Ok(_), Err(error)) => Err(error),
            (Err(authority), Err(projection)) => Err(RuntimeError::AuthorityAndProjection {
                authority,
                projection: Box::new(projection),
            }),
        }
    }

    fn catch_up_locked(&self) -> Result<(), RuntimeError> {
        let snapshot = self.0.authority.snapshot()?;
        let view = self
            .0
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        lib::session_ack::validate(&view, |applied| snapshot.replay(&view, applied))?;
        *self
            .0
            .ack_progress
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = view
            .snapshots()
            .map(|s| {
                (
                    s.header().tape_uuid,
                    (s.summary().last_closed_sequence, s.summary().closed_end),
                )
            })
            .collect();
        self.0
            .projector
            .project_library(snapshot, Box::new(view.clone()), ApplyPath::Live)?;
        for native in view.snapshots() {
            let snapshot = crate::tape_log::authority::AuthoritySnapshot::new(native)?;
            if let ProjectionResult::Failed { error, .. } = self
                .0
                .projector
                .project(AuthorityRecord::Tape(Box::new(snapshot)), ApplyPath::Live)?
            {
                return Err(error.into());
            }
        }
        self.0.projection_dirty.store(false, Ordering::Release);
        Ok(())
    }

    /// Recover a failed projection response from authority before admission.
    pub fn catch_up(&self) -> Result<(), RuntimeError> {
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        self.catch_up_locked()
    }

    /// Rebuild from the shared writer's immutable prefixes under dispatch
    /// ownership. No native path is reopened while its owner may hold a lease.
    pub(crate) fn rebuild_projection(
        &self,
        index: &mut crate::CatalogIndex,
    ) -> Result<crate::RebuildReport, RuntimeError> {
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        let snapshot = self.0.authority.snapshot()?;
        let view = {
            let mut view = self.0.view.lock().unwrap_or_else(|e| e.into_inner());
            view.discover_missing(self.0.manifest.tape_root(), index.path())
                .map_err(|e| StateError::JournalReplayFailed(e.to_string()))?;
            view.clone()
        };
        lib::session_ack::validate(&view, |applied| snapshot.replay(&view, applied))?;
        *self
            .0
            .ack_progress
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = view
            .snapshots()
            .map(|s| {
                (
                    s.header().tape_uuid,
                    (s.summary().last_closed_sequence, s.summary().closed_end),
                )
            })
            .collect();
        let natives = view
            .snapshots()
            .map(crate::tape_log::authority::AuthoritySnapshot::new)
            .collect::<Result<Vec<_>, _>>()?;
        self.0.projection_dirty.store(true, Ordering::Release);
        let report =
            index.rebuild_typed_sources(|applied| snapshot.replay(&view, applied), &natives)?;
        self.0.projection_dirty.store(false, Ordering::Release);
        Ok(report)
    }

    /// Ruling 32-L1: the library log does not yet record a tail repair, so native
    /// repair refuses before it reads, replays, archives or writes anything. The
    /// CLI refuses earlier, before it opens state; this is the second line of
    /// defence and is unreachable from any production caller.
    pub(crate) fn repair_native(
        &self,
        _index: &mut crate::CatalogIndex,
        _tape: lib::TapeRef,
        _through: u64,
        _rebuild: bool,
    ) -> Result<crate::tape_log::store::RepairEvidence, RuntimeError> {
        Err(
            StateError::from(crate::tape_log::store::StoreError::Refused(
                TAIL_REPAIR_DEFERRED,
            ))
            .into(),
        )
    }

    /// Motion admission must check this before any CDB, including compensation.
    pub fn ensure_motion_allowed(&self) -> Result<(), RuntimeError> {
        if self.motion_fenced() {
            return Err(RuntimeError::MotionFenced);
        }
        if self.0.projection_dirty.load(Ordering::Acquire) {
            self.catch_up()?;
        }
        Ok(())
    }
    /// Process health/status can expose a storage failure even when no further
    /// alarm can be appended to the fenced log.
    pub fn motion_fenced(&self) -> bool {
        self.0.motion_fenced.load(Ordering::Acquire)
    }
    /// Stop this process after observed write uncertainty cannot be represented
    /// by the available durable source schema. This is health state, not a
    /// substitute for the missing durable quarantine evidence.
    pub fn fence_motion_after_evidence_failure(&self) {
        self.0.motion_fenced.store(true, Ordering::Release);
    }

    /// Release an operator-addressed tape fence through its source identity.
    /// Suffix contradictions require the separate physical recovery producer.
    pub fn release_tape_fence(
        &self,
        id: &str,
        acknowledgment: &str,
        actor: lib::AuditActor,
    ) -> Result<Option<crate::TapeIoFenceRecord>, RuntimeError> {
        if acknowledgment.trim().is_empty() {
            return Err(
                StateError::ConfigInvalid("acknowledgment must not be empty".into()).into(),
            );
        }
        let sequence = id
            .strip_prefix("library:")
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| {
                StateError::ConfigInvalid("fence has no typed library source identity".into())
            })?;
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(fence) = self.0.authority.active_fence(sequence) {
            if !matches!(fence.scope, lib::FenceScope::Tape(_)) {
                return Err(StateError::ConfigInvalid("fence is not tape-scoped".into()).into());
            }
            if fence.cause == lib::FenceCause::SessionSuffixContradiction {
                return Err(StateError::ConfigInvalid(
                    "suffix fence requires explicit physical tail recovery".into(),
                )
                .into());
            }
            self.append_locked(|a, v| {
                a.fence_released(
                    lib::FenceReleased {
                        detail: lib::Bounded(vec![]),
                        fence: sequence,
                        evidence: acknowledgment.into(),
                        forced: false,
                    },
                    actor,
                    lib::SourceLayer::Layer4,
                    v,
                )
            })?;
        } else {
            self.catch_up_locked()?;
        }
        Ok(self
            .0
            .projector
            .reader()?
            .tape_io_fence_by_quarantine_id(id)?)
    }
    /// Refresh from the caller's already-held native lease after durable close.
    /// This does not reopen the tape path or acquire a second native lock.
    pub fn refresh_native(
        &self,
        snapshot: &crate::tape_log::authority::AuthoritySnapshot,
    ) -> Result<(), RuntimeError> {
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        self.0
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .refresh(snapshot.raw.clone())
            .map_err(StateError::from)?;
        Ok(())
    }
    /// Serialize durable native evidence and its projection with a concurrent
    /// rebuild. The same dispatch lease prevents a stale-view rebuild between
    /// SQL completion and publication of the new frozen prefix.
    pub(crate) fn project_native(
        &self,
        index: &mut crate::CatalogIndex,
        snapshot: &crate::tape_log::authority::AuthoritySnapshot,
        path: ApplyPath,
    ) -> Result<crate::projector::apply::Watermarks, ApplyError> {
        self.project_native_before_apply(index, snapshot, path, || ())
    }

    /// Test boundary after freezing evidence while dispatch still excludes a
    /// rebuild; production passes a no-op observer.
    pub(crate) fn project_native_before_apply(
        &self,
        index: &mut crate::CatalogIndex,
        snapshot: &crate::tape_log::authority::AuthoritySnapshot,
        path: ApplyPath,
        before_apply: impl FnOnce(),
    ) -> Result<crate::projector::apply::Watermarks, ApplyError> {
        // Fixture measurement of requests waiting for native dispatch; the
        // current projection is excluded once its lock has been acquired.
        #[cfg(test)]
        {
            let waiting = self
                .0
                .native_projection_waiters
                .fetch_add(1, Ordering::SeqCst)
                + 1;
            self.0
                .native_projection_high_water
                .fetch_max(waiting, Ordering::SeqCst);
        }
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(test)]
        self.0
            .native_projection_waiters
            .fetch_sub(1, Ordering::SeqCst);
        {
            let mut view = self.0.view.lock().unwrap_or_else(|e| e.into_inner());
            if !view
                .accepts_snapshot(&snapshot.raw)
                .map_err(|e| ApplyError::Catalog(StateError::from(e)))?
            {
                return Err(ApplyError::Catalog(StateError::JournalReplayFailed(
                    "native snapshot belongs to a replaced repair source".into(),
                )));
            }
            view.refresh(snapshot.raw.clone())
                .map_err(|e| ApplyError::Catalog(StateError::from(e)))?;
        }
        before_apply();
        let view = self
            .0
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let source = self
            .0
            .authority
            .snapshot()
            .map_err(|e| ApplyError::Catalog(StateError::JournalReplayFailed(e.to_string())))?;
        let prior = self
            .0
            .ack_progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&snapshot.raw.header().tape_uuid)
            .copied();
        let already_validated = prior
            .is_some_and(|(through, _)| through >= snapshot.raw.summary().last_closed_sequence);
        let checked = if already_validated {
            0
        } else {
            match lib::session_ack::validate_new(snapshot.raw.clone(), prior, |applied| {
                source.replay(&view, applied)
            }) {
                Ok(checked) => checked,
                Err(error) => {
                    self.0.motion_fenced.store(true, Ordering::Release);
                    return Err(ApplyError::Catalog(error));
                }
            }
        };
        #[cfg(test)]
        self.0
            .ack_frames_checked
            .fetch_add(checked, Ordering::Relaxed);
        #[cfg(not(test))]
        let _ = checked;
        if !already_validated {
            self.0
                .ack_progress
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    snapshot.raw.header().tape_uuid,
                    (
                        snapshot.raw.summary().last_closed_sequence,
                        snapshot.raw.summary().closed_end,
                    ),
                );
        }
        if self.0.projection_dirty.load(Ordering::Acquire) {
            self.catch_up_locked()
                .map_err(|e| ApplyError::Catalog(StateError::JournalReplayFailed(e.to_string())))?;
        }
        self.0.projection_dirty.store(true, Ordering::Release);
        let current = if prior
            .is_some_and(|(through, _)| through > snapshot.raw.summary().last_closed_sequence)
        {
            view.snapshots()
                .find(|s| s.header().tape_uuid == snapshot.raw.header().tape_uuid)
                .map(crate::tape_log::authority::AuthoritySnapshot::new)
                .transpose()
                .map_err(ApplyError::Catalog)?
                .unwrap_or_else(|| snapshot.clone())
        } else {
            snapshot.clone()
        };
        let result = index.apply_frozen(&AuthorityRecord::Tape(Box::new(current)), path)?;
        self.0.projection_dirty.store(false, Ordering::Release);
        Ok(result)
    }
    /// Exact dependency reference to a durable closing native record, or the
    /// published immutable header for the SupersedeTail-only empty prefix.
    pub fn native_reference(
        &self,
        snapshot: &crate::tape_log::authority::AuthoritySnapshot,
    ) -> Result<lib::TapeAt, RuntimeError> {
        let (sequence, incarnation) = snapshot.log_reference();
        let digest = if sequence == 0 {
            snapshot
                .raw
                .header_digest()
                .map_err(|e| StateError::JournalReplayFailed(e.to_string()))?
        } else {
            snapshot
                .raw
                .record_evidence(sequence)
                .map_err(|e| StateError::JournalReplayFailed(e.to_string()))?
                .map(|e| e.digest)
        }
        .ok_or_else(|| {
            StateError::JournalReplayFailed(
                "native dependency is not a durable closing prefix".into(),
            )
        })?;
        Ok(lib::TapeAt {
            tape: lib::TapeRef {
                uuid: snapshot.raw.header().tape_uuid,
                incarnation,
            },
            sequence,
            record_digest: digest,
        })
    }
    /// Latest frozen native dependency, refreshed by the native owner while its
    /// tape lease remains held. Empty headers cannot support a write-fence at.
    pub fn current_native_reference(
        &self,
        tape: lib::Id,
    ) -> Result<Option<lib::TapeAt>, RuntimeError> {
        let snapshot = self
            .0
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshots()
            .find(|snapshot| snapshot.header().tape_uuid == tape);
        let Some(snapshot) = snapshot else {
            return Ok(None);
        };
        if snapshot.summary().last_closed_sequence == 0 {
            return Ok(None);
        }
        let snapshot = crate::tape_log::authority::AuthoritySnapshot::new(snapshot)?;
        self.native_reference(&snapshot).map(Some)
    }

    /// Last accepted source configuration for idempotent reset publication.
    pub(crate) fn configuration(&self) -> Option<lib::ConfigurationAccepted> {
        self.0.authority.configuration()
    }
    #[cfg(test)]
    pub(crate) fn test_source_records(&self) -> Vec<lib::Record> {
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        let view = self.0.view.lock().unwrap_or_else(|e| e.into_inner());
        let mut records = Vec::new();
        self.0
            .authority
            .snapshot()
            .unwrap()
            .replay(&*view, &mut |record| {
                records.push(record.record().clone());
                Ok(())
            })
            .unwrap();
        records
    }
    /// Recover an interrupted header-only repair from its existing applied
    /// acknowledgment. An unpublished native factory is never such evidence.
    pub fn empty_prefix_supersede(
        &self,
        snapshot: &crate::tape_log::authority::AuthoritySnapshot,
    ) -> Result<Option<lib::SupersedeTail>, RuntimeError> {
        if snapshot.log_reference().0 != 0 {
            return Ok(None);
        }
        let Some(record_digest) = snapshot
            .raw
            .header_digest()
            .map_err(|e| StateError::JournalReplayFailed(e.to_string()))?
        else {
            return Ok(None);
        };
        self.supersede_at(lib::TapeAt {
            tape: lib::TapeRef {
                uuid: snapshot.raw.header().tape_uuid,
                incarnation: 1,
            },
            sequence: 0,
            record_digest,
        })
    }
    /// Inspect the applied acknowledgement for an exact native recovery prefix.
    /// This never admits a different suffix or an ordinary write session.
    pub fn supersede_at(
        &self,
        at: lib::TapeAt,
    ) -> Result<Option<lib::SupersedeTail>, RuntimeError> {
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        let view = self
            .0
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut found = None;
        let replay = self
            .0
            .authority
            .snapshot()?
            .replay(&view, &mut |applied| {
                if let lib::BodyView::SupersedeTail(value) = applied.record().body_view() {
                    if value.at == at {
                        found = Some(value.clone());
                    }
                }
                Ok(())
            })
            .map_err(AuthorityError::Store)?;
        if replay.refusal.is_some() {
            return Err(StateError::JournalReplayFailed(
                "recovery acknowledgement source is unresolved".into(),
            )
            .into());
        }
        Ok(found)
    }
    /// Authority identity used by provisioning and exact fence targeting.
    pub fn tape_state(&self, uuid: lib::Id) -> Option<lib::fold::TapeState> {
        self.0.authority.tape_state(uuid)
    }
    /// Resolve a shell label from current authenticated fold state, never SQL.
    pub fn tape_for_label(&self, label: &str) -> Result<Option<lib::Id>, RuntimeError> {
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        let view = self.0.view.lock().unwrap_or_else(|e| e.into_inner());
        let replay = self
            .0
            .authority
            .snapshot()?
            .replay(&*view, &mut |_| Ok(()))
            .map_err(AuthorityError::Store)?;
        if replay.refusal.is_some() || !replay.fold.pending.is_empty() {
            return Err(StateError::JournalReplayFailed(
                "initialization requires resolved source".into(),
            )
            .into());
        }
        Ok(replay
            .fold
            .tapes
            .values()
            .find(|v| !v.retired && v.label.as_ref().is_some_and(|v| v.as_str() == label))
            .map(|v| v.tape.uuid))
    }
    /// Exact source identity and cause of active tape raises.
    pub fn active_fence(&self, sequence: u64) -> Option<lib::FenceRaised> {
        self.0.authority.active_fence(sequence)
    }
    /// Fixture-only live source-to-projection acknowledgement timings and
    /// queued Apply request high-water mark. Caller provides a quiescent cut.
    #[cfg(any(test, feature = "fixtures"))]
    pub fn take_projection_measurement(&self) -> (usize, Vec<std::time::Duration>) {
        self.0.projector.take_measurement()
    }
    /// Fixture-only wall time from a successful durable library writer return
    /// through acknowledgement of all its applied records. Excludes fsync time.
    #[cfg(any(test, feature = "fixtures"))]
    pub fn take_source_projection_lag(&self) -> Vec<std::time::Duration> {
        std::mem::take(&mut *self.0.source_projection_lag.lock().unwrap())
    }
    /// Open an independent read-only connection to the derived catalog.
    pub fn reader(&self) -> Result<crate::CatalogReader, StateError> {
        self.0.projector.reader()
    }
    /// Bound manifest, useful for reset receipts and runtime identity.
    pub fn manifest(&self) -> &AuthorityManifest {
        &self.0.manifest
    }
}

mod motion;

mod adoption;
mod cleaning;
mod drive_observation;
mod peer_observations;
pub(crate) mod readiness;
#[cfg(test)]
pub(crate) mod refusal_tests;
pub(crate) mod restart;
#[cfg(test)]
mod session_ack_tests;
pub mod terminal;
#[cfg(test)]
mod verdict_tests;

#[cfg(test)]
mod measurements;
