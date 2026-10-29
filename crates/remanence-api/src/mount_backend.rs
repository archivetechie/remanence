//! Shared cartridge mount boundary. Operator mounts own a host-wide device
//! lock; all object writes still enter the checkpointed, drive-bound writer.
use crate::{LtoGen, PoolWriteResult, TapeUuid, WriteObjectToPoolRequest};
pub use remanence_library::MountEndpoint;
use remanence_library::{
    AccessPolicy, DriveHandle, DriveHandleSource, LibraryHandle, MediaFamily, MediaReadiness,
    MediaReadinessWaitOptions, SgTransport, StaticAllowlist,
};
use remanence_state::{
    AuditActor, AuditEvent, AuditEventRecord, AuditSubject, CatalogIndex, GenerationSource,
    LabelProvenance, SourceLayer, StateHandle,
};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

/// Only an explicitly standalone-raised fence permits release without inventory settlement.
pub fn standalone_fence_releasable(fence: &remanence_state::TapeIoFenceRecord) -> bool {
    matches!(
        fence.endpoint,
        Some(remanence_library::DriveKey::Standalone { .. })
    )
}

/// Shared recovery advice for startup and standalone write admission.
pub(crate) fn fence_release_options(fence: &remanence_state::TapeIoFenceRecord) -> &'static str {
    if standalone_fence_releasable(fence) {
        "--ack TEXT"
    } else {
        "--after-settled-inventory --ack TEXT"
    }
}

/// Standalone reads report all fences; writes refuse only the loaded identity or endpoint.
/// Historical UUID-only fences never inherit the endpoint of today's cartridge.
pub fn standalone_fence_policy(
    index: &CatalogIndex,
    loaded_uuid: Option<TapeUuid>,
    endpoint: Option<&remanence_library::DriveKey>,
    writing: bool,
) -> Result<Vec<remanence_state::TapeIoFenceRecord>, MountError> {
    let fences = index.list_active_tape_io_fences().map_err(refusal)?;
    if writing {
        let identity_conflicts = if let Some(uuid) = loaded_uuid {
            let label = index
                .get_tape(&uuid)
                .map_err(refusal)?
                .and_then(|t| t.voltag);
            index
                .tape_io_admission_conflicts(&uuid, label.as_deref())
                .map_err(refusal)?
        } else {
            Vec::new()
        };
        if let Some(fence) = fences.iter().find(|f| {
            identity_conflicts
                .iter()
                .any(|conflict| conflict.quarantine_id == f.quarantine_id)
                || endpoint.is_some_and(|key| f.endpoint.as_ref() == Some(key))
        }) {
            let release_options = fence_release_options(fence);
            return Err(refusal(format!(
                "write blocked by active tape-I/O fence {}: {}; inspect `rem tape quarantine list`, then release with `rem tape quarantine release {} {release_options}`",
                fence.quarantine_id, fence.reason, fence.quarantine_id
            )));
        }
    }
    Ok(fences)
}

/// How an invocation addresses a cartridge; labels are location hints only.
pub enum MountTarget {
    Tape {
        tape_uuid: TapeUuid,
    },
    Element {
        library_serial: String,
        element: u16,
    },
    Device {
        path: PathBuf,
        expected_uuid: Option<TapeUuid>,
    },
}
/// A BOT-proven cartridge and the endpoint on which it was proved.
pub struct MountedTape {
    pub drive: DriveHandle,
    pub tape_uuid: TapeUuid,
    pub label: Option<(String, LabelProvenance)>,
    /// Unknown generation is valid for reads; write admission still requires it.
    pub generation: Option<(LtoGen, GenerationSource)>,
    pub endpoint: MountEndpoint,
    pub identity_generation: u64,
    _device_lock: Option<Arc<DeviceLock>>,
    reserved: bool,
}
impl Drop for MountedTape {
    fn drop(&mut self) {
        if let Err(error) = self.drive.release_medium_removal_guard() {
            tracing::error!(%error, "ALLOW failed; run tape release");
        }
        if self.reserved {
            if let Err(error) = self.drive.release_reservation() {
                tracing::error!(%error, "RELEASE failed");
            }
        }
    }
}
impl MountedTape {
    /// Ephemeral identities never authorize persistent mutations.
    pub fn writable(&self) -> bool {
        matches!(self.endpoint, MountEndpoint::Durable(_))
    }
}
/// Mount refusal, including safe classification of an unwritten cartridge.
#[derive(Debug, thiserror::Error)]
pub enum MountError {
    #[error("blank cartridge at {path}; initialize it explicitly before writing")]
    Blank { path: PathBuf },
    #[error("{0}")]
    Refused(String),
    /// A plaintext request would duplicate the newest encrypted version.
    #[error("would store plaintext copy of encrypted version {newest_version}")]
    PlaintextCopy { newest_version: String },
    /// Preserve typed tape failures until the command chooses continuation advice.
    #[error("{0}")]
    PoolWrite(Box<crate::PoolWriteError>),
    /// A durable terminal projection records hardware early warning.
    #[error("tape closed after hardware early warning; continue on the next tape")]
    EarlyWarning,
}
fn refusal(error: impl std::fmt::Display) -> MountError {
    MountError::Refused(error.to_string())
}
/// One cartridge per invocation. Release consumes the backend and its lock.
pub trait MountBackend {
    fn mount(&mut self, target: MountTarget) -> Result<MountedTape, MountError>;
    fn release(self, mounted: MountedTape);
}

/// The only filesystem boundary for device locks. Production supplies exactly
/// /run/lock/rem; tests inject a root without changing acquisition semantics.
type LockIo = dyn FnMut(&str, &Path) -> std::io::Result<PathBuf>;

/// Held exclusive flock on a device lock file, released explicitly on drop.
///
/// A flock belongs to the open file description, not to this descriptor. A
/// child forked by any other thread inherits a duplicate of that description
/// and keeps it until its exec closes it (`O_CLOEXEC`), so close(2) alone can
/// leave the drive locked after release. `LOCK_UN` on any duplicate releases it.
struct DeviceLock(File);
impl Drop for DeviceLock {
    fn drop(&mut self) {
        // SAFETY: flock borrows the live descriptor that the File owns. A
        // failed unlock leaves close(2) to release the lock as before.
        let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn lock_device(io: &mut LockIo, serial: &str) -> Result<DeviceLock, MountError> {
    if serial.is_empty() || serial == "." || serial == ".." || serial.contains(['/', '\\', '\0']) {
        return Err(refusal(
            "invalid drive serial for device lock; check VPD identity",
        ));
    }
    let root = Path::new("/run/lock/rem");
    let physical = io("create_dir_all", root).map_err(|e| {
        refusal(format!(
            "device lock {}: {e}; check directory permissions",
            root.display()
        ))
    })?;
    std::fs::create_dir_all(physical).map_err(|e| {
        refusal(format!(
            "device lock {}: {e}; create directory and fix permissions",
            root.display()
        ))
    })?;
    use std::os::unix::fs::OpenOptionsExt;
    let path = root.join(format!("{serial}.lock"));
    let physical = io("open", &path).map_err(|e| {
        refusal(format!(
            "device lock {}: {e}; check directory permissions",
            path.display()
        ))
    })?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&physical)
        .map_err(|e| {
            refusal(format!(
                "device lock {}: {e}; check directory permissions",
                path.display()
            ))
        })?;
    let metadata_error = |error| {
        refusal(format!(
            "device lock {}: {error}; check the lock file and directory permissions",
            path.display()
        ))
    };
    io("metadata", &path).map_err(metadata_error)?;
    if !file.metadata().map_err(metadata_error)?.is_file() {
        return Err(refusal(format!(
            "device lock {} is not a regular file; fix the lock directory",
            path.display()
        )));
    }
    io("lock_exclusive", &path).map_err(|e| {
        refusal(format!(
            "device lock {}: {e}; stop the other process and retry",
            path.display()
        ))
    })?;
    // SAFETY: flock borrows a live file descriptor; File owns its lifetime.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(refusal(format!(
            "device lock {}: {}; stop the other process and retry",
            path.display(),
            std::io::Error::last_os_error()
        )));
    }
    Ok(DeviceLock(file))
}

/// Physical removal recovery owns only the device lock and audit sink. It never
/// constructs an OperatorMount or gains write/session admission methods.
struct LockedAuditedDevice {
    drive: Option<DriveHandle>,
    lock: Option<Arc<DeviceLock>>,
    audit_error: Arc<Mutex<Option<String>>>,
    endpoint: MountEndpoint,
}
impl LockedAuditedDevice {
    fn open(
        mut drive: DriveHandle,
        state: &StateHandle,
        io: &mut LockIo,
    ) -> Result<Self, MountError> {
        let lock = match drive.endpoint() {
            MountEndpoint::Durable(remanence_library::DriveKey::Standalone { drive_serial }) => {
                Some(Arc::new(lock_device(io, drive_serial)?))
            }
            MountEndpoint::Ephemeral { .. } => None,
            _ => return Err(refusal("operator mount requires a standalone endpoint")),
        };
        let audit_error = Arc::new(Mutex::new(None));
        if drive.endpoint().durable_key().is_some() {
            let pending = Arc::clone(&audit_error);
            let audit_dir = state.paths().audit_dir.clone();
            let fsync = state.config().audit.fsync;
            let mut log =
                remanence_state::FileAuditLog::open(&audit_dir, fsync).map_err(refusal)?;
            let mut known_len = std::fs::metadata(log.segment_path())
                .map_err(refusal)?
                .len();
            drive.set_audit_hook(move |event| {
                use remanence_state::AuditSink;
                let result = (|| {
                    let len = std::fs::metadata(log.segment_path())
                        .map_err(|e| e.to_string())?
                        .len();
                    // Other appenders run synchronously under the invocation's
                    // state lock. Replay only when one advanced the chain.
                    if len != known_len {
                        log = remanence_state::FileAuditLog::open(&audit_dir, fsync)
                            .map_err(|e| e.to_string())?;
                    }
                    log.append(remanence_state::library_audit_event_record(event))
                        .map_err(|e| e.to_string())?;
                    known_len = std::fs::metadata(log.segment_path())
                        .map_err(|e| e.to_string())?
                        .len();
                    Ok::<_, String>(())
                })();
                if let Err(error) = result {
                    *pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
                }
            });
        }
        let endpoint = drive.endpoint().clone();
        Ok(Self {
            drive: Some(drive),
            lock,
            audit_error,
            endpoint,
        })
    }
    fn flush_audit(&self, state: &mut StateHandle) -> Result<(), MountError> {
        if matches!(self.endpoint, MountEndpoint::Ephemeral { .. }) {
            return Ok(());
        }
        state.refresh_audit_append_cursor().map_err(refusal)?;
        if let Some(error) = self
            .audit_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            return Err(refusal(format!("drive audit failed: {error}")));
        }
        Ok(())
    }
}

/// Standalone invocation state. A second mount cannot consume another cartridge.
pub struct OperatorMount<'a> {
    state: &'a mut StateHandle,
    context: LockedAuditedDevice,
    path: PathBuf,
    unusable: bool,
    checkpoints_reconciled: bool,
}
impl<'a> OperatorMount<'a> {
    /// Finish after a common read consumer has consumed/released MountedTape.
    /// Refresh the state append cursor and surface any drive-audit failure.
    pub fn finish(mut self) -> Result<(), MountError> {
        self.flush_audit()
    }

    /// Catalog view for the common mounted-object read, verify, and export paths.
    pub fn catalog_index(&mut self) -> &CatalogIndex {
        self.state.catalog_index()
    }

    pub fn device_lock_path(serial: &str) -> PathBuf {
        Path::new("/run/lock/rem").join(format!("{serial}.lock"))
    }
    /// Open under explicit device policy; caller must establish standalone ownership.
    pub fn open(
        path: &Path,
        policy: &StaticAllowlist,
        state: &'a mut StateHandle,
    ) -> Result<Self, MountError> {
        let drive = DriveHandle::open_standalone(path, policy).map_err(refusal)?;
        Self::from_drive(path, drive, state, &mut |_, logical| Ok(logical.into()))
    }
    /// Open with caller-supplied transport and lock namespace (production uses /run/lock/rem).
    pub fn with_transport(
        path: &Path,
        policy: &StaticAllowlist,
        transport: Box<dyn SgTransport>,
        state: &'a mut StateHandle,
        lock_root: &Path,
    ) -> Result<Self, MountError> {
        let drive = DriveHandle::open_standalone_with_transport(path, transport, policy)
            .map_err(refusal)?;
        let root = lock_root.to_owned();
        Self::from_drive(path, drive, state, &mut move |_, logical| {
            Ok(root.join(
                logical
                    .strip_prefix("/run/lock/rem")
                    .expect("device lock namespace"),
            ))
        })
    }
    /// Recover physical removal after a crash, without clearing any durable fence.
    /// The recovery owner is consumed here so no write can bypass startup admission.
    pub fn release_with_transport(
        path: &Path,
        policy: &StaticAllowlist,
        transport: Box<dyn SgTransport>,
        state: &'a mut StateHandle,
        lock_root: &Path,
    ) -> Result<(), MountError> {
        let drive = DriveHandle::open_standalone_with_transport(path, transport, policy)
            .map_err(refusal)?;
        if drive.endpoint().durable_key().is_none() {
            return Err(refusal(
                "ephemeral endpoint is read-only; release requires a VPD serial",
            ));
        }
        let root = lock_root.to_owned();
        let mut context = LockedAuditedDevice::open(drive, state, &mut move |_, logical| {
            Ok(root.join(
                logical
                    .strip_prefix("/run/lock/rem")
                    .expect("device lock namespace"),
            ))
        })?;
        let result = context
            .drive
            .as_mut()
            .expect("opened drive")
            .allow_medium_removal()
            .map_err(refusal);
        context.flush_audit(state)?;
        result
    }
    /// Compatibility integration seam delegating to the production IO boundary.
    pub fn with_transport_for_test(
        path: &Path,
        policy: &StaticAllowlist,
        transport: Box<dyn SgTransport>,
        state: &'a mut StateHandle,
        lock_root: &Path,
    ) -> Result<Self, MountError> {
        Self::with_transport(path, policy, transport, state, lock_root)
    }
    /// Inject every lock filesystem operation while retaining the production namespace.
    pub fn with_transport_and_lock_io_for_test(
        path: &Path,
        policy: &StaticAllowlist,
        transport: Box<dyn SgTransport>,
        state: &'a mut StateHandle,
        mut io: Box<LockIo>,
    ) -> Result<Self, MountError> {
        let drive = DriveHandle::open_standalone_with_transport(path, transport, policy)
            .map_err(refusal)?;
        Self::from_drive(path, drive, state, io.as_mut())
    }
    fn from_drive(
        path: &Path,
        drive: DriveHandle,
        state: &'a mut StateHandle,
        io: &mut LockIo,
    ) -> Result<Self, MountError> {
        let context = LockedAuditedDevice::open(drive, state, io)?;
        if let Some(endpoint) = context.endpoint.durable_key() {
            state.replay_audit_projection().map_err(refusal)?;
            standalone_fence_policy(state.catalog_index(), None, Some(endpoint), false)?;
            state
                .recover_sessions_after_restart(Some(endpoint), |state, session| {
                    if session.session_kind == "write" {
                        let tape_uuid = session
                            .tape_uuid
                            .as_deref()
                            .and_then(|b| b.try_into().ok())
                            .ok_or_else(|| {
                                remanence_state::StateError::IndexCorrupt(
                                    "unfinished write has no tape identity".into(),
                                )
                            })?;
                        raise_standalone_fence(
                            state,
                            tape_uuid,
                            session.drive_key.as_ref(),
                            "standalone_write_lost_by_restart",
                            "unfinished durable write session requires recovery",
                        )
                        .map_err(|e| remanence_state::StateError::IndexCorrupt(e.to_string()))?;
                    }
                    Ok(())
                })
                .map_err(refusal)?;
            standalone_fence_policy(state.catalog_index(), None, Some(endpoint), false)?;
        }
        Ok(Self {
            state,
            context,
            path: path.into(),
            unusable: false,
            checkpoints_reconciled: false,
        })
    }
    /// Prepare a BOT initialization decision, then run its write and provisioning
    /// under the same durable calibration/session guards as mounted writes.
    /// Preparation must only inspect/configure media; it returns the planned UUID.
    pub fn initialize<T, R>(
        &mut self,
        reserve: bool,
        prepare: impl FnOnce(&mut DriveHandle, &mut StateHandle) -> Result<(TapeUuid, T), String>,
        write_and_provision: impl FnOnce(&mut DriveHandle, &mut StateHandle, T) -> Result<R, String>,
    ) -> Result<R, MountError> {
        let mut drive = self
            .context
            .drive
            .take()
            .ok_or_else(|| refusal("one cartridge per invocation"))?;
        if drive.endpoint().durable_key().is_none() {
            return Err(refusal("ephemeral endpoint is read-only"));
        }
        let mut reserved = false;
        let result = (|| {
            if reserve {
                drive.reserve().map_err(refusal)?;
                reserved = true;
            }
            let (uuid, plan) = prepare(&mut drive, self.state).map_err(refusal)?;
            self.flush_audit()?;
            standalone_fence_policy(
                self.state.catalog_index(),
                Some(uuid),
                drive.endpoint().durable_key(),
                true,
            )?;
            let control = self.state.calibration_control().clone();
            crate::calibration::install_standalone_calibration_fence(&mut drive, &control, uuid)
                .map_err(refusal)?;
            let id = uuid::Uuid::new_v4();
            self.session(uuid, id, AuditEvent::SessionOpened)?;
            let before = drive.media_write_dispatch_count();
            let result = write_and_provision(&mut drive, self.state, plan).map_err(refusal);
            self.flush_audit()?;
            let uncertain = result.is_err() && drive.media_write_dispatch_count() != before;
            if uncertain {
                self.unusable = true;
                raise_standalone_fence(
                    self.state,
                    uuid,
                    self.context.endpoint.durable_key(),
                    "standalone_init_failed",
                    &result.as_ref().err().expect("failed write").to_string(),
                )?;
            }
            self.session(
                uuid,
                id,
                if uncertain {
                    AuditEvent::SessionOrphaned
                } else {
                    AuditEvent::SessionClosed
                },
            )?;
            result
        })();
        let allow = drive.allow_medium_removal().map_err(refusal);
        let release = if reserved {
            drive.release_reservation().map_err(refusal)
        } else {
            Ok(())
        };
        self.flush_audit()?;
        let value = result?;
        allow?;
        release?;
        Ok(value)
    }

    /// Apply metadata changes only while the durable BOT proof remains valid.
    pub fn mutate_catalog<T>(
        &mut self,
        mounted: &MountedTape,
        operation: impl FnOnce(&mut StateHandle) -> Result<T, String>,
    ) -> Result<T, MountError> {
        self.require_durable_mount(mounted)?;
        standalone_fence_policy(
            self.state.catalog_index(),
            Some(mounted.tape_uuid),
            mounted.endpoint.durable_key(),
            true,
        )?;
        if mounted.identity_generation != mounted.drive.identity_generation() {
            return Err(refusal("media identity invalidated"));
        }
        self.flush_audit()?;
        operation(self.state).map_err(refusal)
    }

    fn flush_audit(&mut self) -> Result<(), MountError> {
        self.context.flush_audit(self.state)
    }
    fn require_durable_mount(&self, mounted: &MountedTape) -> Result<(), MountError> {
        if self.unusable {
            return Err(refusal("invocation is unusable after uncertain I/O; recover the tape and start a new invocation"));
        }
        if !mounted.writable() {
            return Err(refusal(
                "ephemeral endpoint is read-only; a VPD serial is required",
            ));
        }
        if mounted.endpoint != self.context.endpoint
            || mounted.drive.endpoint() != &self.context.endpoint
        {
            return Err(refusal(
                "mounted endpoint differs from the invocation's locked device",
            ));
        }
        Ok(())
    }
    /// Opt in to fabric reservation; never called implicitly by mount.
    pub fn reserve(&mut self, mounted: &mut MountedTape) -> Result<(), MountError> {
        self.require_durable_mount(mounted)?;
        mounted.drive.reserve().map_err(refusal)?;
        mounted.reserved = true;
        self.flush_audit()
    }
    fn session(
        &mut self,
        tape_uuid: TapeUuid,
        id: uuid::Uuid,
        event: AuditEvent,
    ) -> Result<(), MountError> {
        self.flush_audit()?;
        let key = self
            .context
            .endpoint
            .durable_key()
            .ok_or_else(|| refusal("ephemeral endpoint is read-only"))?;
        let detail = BTreeMap::from([
            ("session_kind".into(), ciborium::Value::Text("write".into())),
            (
                "tape_uuid".into(),
                ciborium::Value::Bytes(tape_uuid.to_vec()),
            ),
            ("endpoint_key".into(), ciborium::Value::Text(key.encode())),
        ]);
        // Session recovery evidence must reach stable storage before any write,
        // even when ordinary audit appends are configured without fsync.
        use remanence_state::AuditSink;
        let mut log = remanence_state::FileAuditLog::open(&self.state.paths().audit_dir, true)
            .map_err(refusal)?;
        log.append(AuditEventRecord {
            event,
            actor: AuditActor::local_user(),
            source_layer: SourceLayer::Layer5,
            session_id: Some(id),
            operation_id: None,
            idempotency_key: None,
            subject: AuditSubject {
                kind: "write".into(),
                id: Some(id.to_string()),
            },
            detail,
        })
        .map_err(refusal)?;
        self.state.refresh_audit_append_cursor().map_err(refusal)?;
        self.state.replay_audit_projection().map_err(refusal)?;
        Ok(())
    }
    /// Reconcile durable checkpoint authority before any standalone catalog decision.
    pub fn reconcile_checkpoints(&mut self) -> Result<(), MountError> {
        if !self.checkpoints_reconciled {
            crate::reconcile_checkpoint_journal_projections(self.state).map_err(refusal)?;
            self.checkpoints_reconciled = true;
        }
        Ok(())
    }

    /// Decide a standalone file version, then wrap the ordinary admitted writer.
    /// Preparation is reused so mutations during streaming fail writer validation.
    pub fn put_path_version(
        &mut self,
        mounted: &mut MountedTape,
        request: WriteObjectToPoolRequest,
    ) -> Result<Result<(PoolWriteResult, &'static str), remanence_state::VersionRow>, MountError>
    {
        self.put_path_version_with_policy(mounted, request, false)
    }

    /// Explicit plaintext downgrade policy around the same version and write funnel.
    pub fn put_path_version_with_policy(
        &mut self,
        mounted: &mut MountedTape,
        request: WriteObjectToPoolRequest,
        allow_plaintext_copy: bool,
    ) -> Result<Result<(PoolWriteResult, &'static str), remanence_state::VersionRow>, MountError>
    {
        self.put_path_version_with_preparation(mounted, request, allow_plaintext_copy, || {})
    }

    /// Keep the decision and preparation in one funnel; the hook models a source race.
    fn put_path_version_with_preparation(
        &mut self,
        mounted: &mut MountedTape,
        mut request: WriteObjectToPoolRequest,
        allow_plaintext_copy: bool,
        before_prepare: impl FnOnce(),
    ) -> Result<Result<(PoolWriteResult, &'static str), remanence_state::VersionRow>, MountError>
    {
        self.require_durable_mount(mounted)?;
        self.reconcile_checkpoints()?;
        standalone_fence_policy(
            self.state.catalog_index(),
            Some(mounted.tape_uuid),
            mounted.endpoint.durable_key(),
            true,
        )?;
        let path =
            remanence_stream::normalize_archive_path(&request.archive_path).map_err(refusal)?;
        request.archive_path = PathBuf::from(&path);
        if !allow_plaintext_copy
            && matches!(
                request.representation,
                crate::PoolWriteRepresentation::Plaintext
            )
        {
            if let Some(version) = self
                .state
                .catalog_index()
                .list_committed_versions_by_path_any_pool(&path)
                .map_err(refusal)?
                .first()
            {
                let record = self
                    .state
                    .catalog_index()
                    .get_native_object(&version.object_id)
                    .map_err(refusal)?
                    .ok_or_else(|| refusal("version object disappeared"))?;
                if record
                    .copies
                    .iter()
                    .any(|copy| copy.status == "committed" && copy.representation == "encrypted")
                {
                    return Err(MountError::PlaintextCopy {
                        newest_version: version.object_id.clone(),
                    });
                }
            }
        }
        let newest = self
            .state
            .catalog_index()
            .list_committed_versions_by_path(&request.pool_id, &path)
            .map_err(refusal)?
            .into_iter()
            .next();
        let representation_matches = if let Some(version) = &newest {
            let record = self
                .state
                .catalog_index()
                .get_native_object(&version.object_id)
                .map_err(refusal)?
                .ok_or_else(|| refusal("version object disappeared"))?;
            record.copies.iter().any(|copy| {
                copy.status == "committed"
                    && copy.pool_id.as_deref() == Some(request.pool_id.as_str())
                    && match &request.representation {
                        crate::PoolWriteRepresentation::Plaintext => {
                            copy.representation == "plaintext"
                        }
                        crate::PoolWriteRepresentation::Encrypted { recipients } => {
                            let requested: std::collections::BTreeSet<_> = recipients
                                .iter()
                                .map(|r| {
                                    r.recipient_epoch_id
                                        .iter()
                                        .map(|b| format!("{b:02x}"))
                                        .collect::<String>()
                                })
                                .collect();
                            copy.representation == "encrypted"
                                && copy.recipient_epoch_ids.as_ref().is_some_and(|ids| {
                                    ids.iter()
                                        .cloned()
                                        .collect::<std::collections::BTreeSet<_>>()
                                        == requested
                                })
                        }
                    }
            })
        } else {
            false
        };
        let reason = if newest.is_none() {
            "new"
        } else if !representation_matches {
            "representation_changed"
        } else {
            "content_changed"
        };
        if let Some(version) = &newest {
            if representation_matches
                && request.source.size_bytes().map_err(refusal)? == version.size_bytes
                && request.source.content_sha256().map_err(refusal)?.as_slice()
                    == version.file_sha256
            {
                return Ok(Err(version.clone()));
            }
        }
        let crate::WriteObjectSource::Path(source) = &request.source else {
            return Err(refusal("standalone put requires a regular file path"));
        };
        before_prepare();
        let prepared = remanence_stream::prepare_regular_file(
            source,
            &request.archive_path,
            uuid::Uuid::new_v4().to_string(),
        )
        .map_err(refusal)?;
        if let Some(version) = newest {
            if representation_matches
                && prepared.spec.size_bytes == version.size_bytes
                && prepared.spec.file_sha256.as_ref().map(|d| d.as_slice())
                    == Some(version.file_sha256.as_slice())
            {
                return Ok(Err(version));
            }
        }
        request.source =
            crate::WriteObjectSource::PreparedPath(crate::pool_write::PreparedPathSource(prepared));
        self.write_checkpointed(mounted, request)
            .map(|result| Ok((result, reason)))
    }

    /// Durable session and calibration fencing wrap the existing admission funnel.
    pub fn write_checkpointed(
        &mut self,
        mounted: &mut MountedTape,
        request: WriteObjectToPoolRequest,
    ) -> Result<PoolWriteResult, MountError> {
        self.require_durable_mount(mounted)?;
        self.reconcile_checkpoints()?;
        self.flush_audit()?;
        standalone_fence_policy(
            self.state.catalog_index(),
            Some(mounted.tape_uuid),
            mounted.endpoint.durable_key(),
            true,
        )?;
        let pool = self
            .state
            .config()
            .tape_pools
            .iter()
            .find(|p| p.id == request.pool_id)
            .cloned()
            .ok_or_else(|| refusal("unconfigured pool"))?;
        let loaded = self
            .state
            .catalog_index()
            .get_tape(&mounted.tape_uuid)
            .map_err(refusal)?
            .ok_or_else(|| refusal("mounted tape is not catalogued"))?;
        if loaded.pool_id.as_deref() != Some(request.pool_id.trim()) {
            return Err(refusal("requested pool must equal the loaded tape's pool"));
        }
        if let Some(replay) =
            crate::pool_write::replay_committed_pool_write_reconciled(self.state, &request)
                .map_err(|e| MountError::PoolWrite(Box::new(e)))?
        {
            return Ok(replay);
        }
        let journals = self.state.paths().journal_dir.join("checkpoints");
        let disposition = crate::pool_write::admit_pinned_tape_for_write_session(
            self.state.catalog_index(),
            mounted.tape_uuid,
            &request.pool_id,
            &pool,
            &journals,
        )
        .map_err(|error| match error {
            crate::pool_write::PinnedTapeError::NotWritable {
                reason:
                    crate::pool_write::WritabilityError::InsufficientCapacity {
                        object_size,
                        raw_capacity,
                        used,
                    },
                ..
            } => MountError::PoolWrite(Box::new(
                crate::PoolWriteError::SelectedTapeInsufficientCapacity {
                    object_size,
                    raw_capacity,
                    used,
                },
            )),
            crate::pool_write::PinnedTapeError::NotWritable { .. }
                if loaded
                    .terminal_finalization
                    .as_ref()
                    .is_some_and(|terminal| {
                        terminal.trigger
                            == remanence_state::TerminalFinalizationTrigger::HardwareEarlyWarning
                    }) =>
            {
                MountError::EarlyWarning
            }
            other => refusal(other),
        })?;
        let selected = match disposition {
            crate::pool_write::PinnedWriteDisposition::Writable(selected) => selected,
            _ => {
                return Err(MountError::PoolWrite(Box::new(
                    crate::PoolWriteError::TerminalCloseRequired {
                        detail: "tape requires terminal recovery before writing".into(),
                    },
                )))
            }
        };
        let id = uuid::Uuid::new_v4();
        self.session(mounted.tape_uuid, id, AuditEvent::SessionOpened)?;
        let before = mounted.drive.media_write_dispatch_count();
        let result = (|| {
            mounted.drive.test_unit_ready().map_err(refusal)?;
            mounted.drive.prevent_medium_removal().map_err(refusal)?;
            crate::pool_write::write_to_selected_drive_checkpointed_reconciled(
                self.state,
                &mut mounted.drive,
                request,
                selected,
            )
            .map_err(|e| MountError::PoolWrite(Box::new(e)))
        })();
        let uncertain = result.is_err() && mounted.drive.media_write_dispatch_count() != before;
        if uncertain {
            self.unusable = true;
            raise_standalone_fence(
                self.state,
                mounted.tape_uuid,
                mounted.endpoint.durable_key(),
                "standalone_write_failed",
                &result.as_ref().expect_err("failed write").to_string(),
            )?;
        }
        let closed = self.session(
            mounted.tape_uuid,
            id,
            if uncertain {
                AuditEvent::SessionOrphaned
            } else {
                AuditEvent::SessionClosed
            },
        );
        if closed.is_err() && mounted.drive.media_write_dispatch_count() != before {
            self.unusable = true;
        }
        closed?;
        if result.as_ref().is_ok_and(|r| !r.is_replay()) {
            mounted.identity_generation = mounted.drive.identity_generation();
        }
        result
    }
}
impl MountBackend for OperatorMount<'_> {
    fn mount(&mut self, target: MountTarget) -> Result<MountedTape, MountError> {
        let MountTarget::Device {
            path,
            expected_uuid,
        } = target
        else {
            return Err(refusal("operator mount requires a device target"));
        };
        if path != self.path {
            return Err(refusal("device target differs from opened device"));
        }
        let mut drive = self
            .context
            .drive
            .take()
            .ok_or_else(|| refusal("one cartridge per invocation; start a new invocation"))?;
        let result = prove_mount(&mut drive, self.state.catalog_index(), expected_uuid, &path);
        match result {
            Ok((tape_uuid, label, generation)) => {
                let proof_generation = drive.identity_generation();
                if drive.endpoint().durable_key().is_some() {
                    let control = self.state.calibration_control().clone();
                    if let Err(error) = crate::calibration::install_standalone_calibration_fence(
                        &mut drive, &control, tape_uuid,
                    ) {
                        let _ = drive.allow_medium_removal();
                        return Err(refusal(error));
                    }
                }
                if drive.identity_generation() != proof_generation {
                    let _ = drive.allow_medium_removal();
                    return Err(refusal("identity invalidated while binding calibration; remount and prove BOT again"));
                }
                if let Err(error) = self.flush_audit() {
                    let _ = drive.allow_medium_removal();
                    return Err(error);
                }
                Ok(MountedTape {
                    endpoint: drive.endpoint().clone(),
                    identity_generation: drive.identity_generation(),
                    drive,
                    tape_uuid,
                    label,
                    generation,
                    reserved: false,
                    _device_lock: self.context.lock.clone(),
                })
            }
            Err(error) => {
                let _ = drive.allow_medium_removal();
                self.flush_audit()?;
                Err(error)
            }
        }
    }
    fn release(self, mounted: MountedTape) {
        drop(mounted);
        if let Err(error) = self.finish() {
            tracing::error!(%error, "mount audit failed");
        }
    }
}

type MountProof = (
    TapeUuid,
    Option<(String, LabelProvenance)>,
    Option<(LtoGen, GenerationSource)>,
);
fn prove_mount(
    drive: &mut DriveHandle,
    index: &CatalogIndex,
    expected: Option<TapeUuid>,
    path: &Path,
) -> Result<MountProof, MountError> {
    let poll = drive
        .wait_for_media_readiness(
            MediaFamily::Unknown,
            None,
            MediaReadinessWaitOptions {
                wait: true,
                timeout: Duration::from_secs(9_000),
                poll_interval: Duration::from_secs(30),
            },
            || None,
            |_| Ok(()),
        )
        .map_err(refusal)?;
    if poll.readiness != MediaReadiness::Ready {
        return Err(refusal(format!(
            "{}; insert the cartridge and retry",
            poll.readiness
        )));
    }
    drive.prevent_medium_removal().map_err(refusal)?;
    drive.rewind().map_err(refusal)?;
    let config = drive.read_config().map_err(refusal)?;
    let classification = crate::classify_bot_from_source(&mut DriveHandleSource(drive));
    let tape_uuid = match classification.classification {
        crate::BotClassification::OursBootstrap { uuid, .. } => uuid,
        crate::BotClassification::BlankCheckEod => {
            return Err(MountError::Blank { path: path.into() })
        }
        other => return Err(refusal(format!("BOT classification refused: {other:?}"))),
    };
    if let Some(expected) = expected {
        drive.rewind().map_err(refusal)?;
        crate::verify_tape_identity(&mut DriveHandleSource(drive), &expected).map_err(refusal)?;
    }
    let tape = index.get_tape(&tape_uuid).map_err(refusal)?;
    let generation = tape.as_ref().and_then(crate::tape_generation).or_else(|| {
        config
            .density_code
            .and_then(crate::lto_generation_from_density)
            .map(|g| (g, GenerationSource::Measured))
    });
    let label = tape.and_then(|t| t.voltag.zip(t.voltag_source));
    Ok((tape_uuid, label, generation))
}

/// Library mount adapter; the existing barcode loader remains the load funnel.
pub struct ChangerMount<'a> {
    pub library: &'a mut LibraryHandle,
    pub policy: &'a dyn AccessPolicy,
    pub index: &'a CatalogIndex,
}
impl MountBackend for ChangerMount<'_> {
    fn mount(&mut self, target: MountTarget) -> Result<MountedTape, MountError> {
        if let MountTarget::Tape { tape_uuid } = target {
            let mut drive =
                crate::load_tape_by_uuid(self.index, self.library, self.policy, &tape_uuid)
                    .map_err(refusal)?;
            drive.rewind().map_err(refusal)?;
            crate::verify_tape_identity(&mut DriveHandleSource(&mut drive), &tape_uuid)
                .map_err(refusal)?;
            let tape = self.index.get_tape(&tape_uuid).map_err(refusal)?;
            return Ok(MountedTape {
                endpoint: drive.endpoint().clone(),
                identity_generation: drive.identity_generation(),
                drive,
                tape_uuid,
                generation: tape.as_ref().and_then(crate::tape_generation),
                label: tape.and_then(|t| t.voltag.zip(t.voltag_source)),
                reserved: false,
                _device_lock: None,
            });
        }
        let (mut drive, expected) = match target {
            MountTarget::Element {
                library_serial,
                element,
            } => {
                if self.library.library().serial != library_serial {
                    return Err(refusal("element belongs to a different library"));
                }
                let bay = if self
                    .library
                    .library()
                    .drive_bays
                    .iter()
                    .any(|b| b.element_address == element && b.loaded)
                {
                    element
                } else {
                    let bay = self
                        .library
                        .library()
                        .drive_bays
                        .iter()
                        .find(|b| !b.loaded && b.installed.is_some())
                        .ok_or_else(|| refusal("no free drive for element"))?
                        .element_address;
                    self.library
                        .load(element, bay, self.policy)
                        .map_err(refusal)?;
                    bay
                };
                (
                    self.library.open_drive(bay, self.policy).map_err(refusal)?,
                    None,
                )
            }
            _ => return Err(refusal("changer mount requires a tape or element target")),
        };
        match prove_mount(&mut drive, self.index, expected, Path::new("library drive")) {
            Ok((tape_uuid, label, generation)) => Ok(MountedTape {
                endpoint: drive.endpoint().clone(),
                identity_generation: drive.identity_generation(),
                drive,
                tape_uuid,
                label,
                generation,
                reserved: false,
                _device_lock: None,
            }),
            Err(error) => {
                let _ = drive.allow_medium_removal();
                Err(error)
            }
        }
    }
    fn release(self, mounted: MountedTape) {
        drop(mounted);
    }
}

/// Operator acknowledgements are checked before the shared finalization admission.
pub struct StandaloneFinalizeRequest {
    pub tape_uuid: TapeUuid,
    pub acknowledge_tape: TapeUuid,
    pub expected_pool_id: Option<String>,
    pub reason: String,
    pub idempotency_key: uuid::Uuid,
}
impl OperatorMount<'_> {
    /// Close a partially filled tape through the daemon's terminal safeguards.
    pub fn finalize(
        &mut self,
        mounted: &mut MountedTape,
        request: StandaloneFinalizeRequest,
    ) -> Result<remanence_state::TerminalFinalizationProjection, MountError> {
        self.require_durable_mount(mounted)?;
        if request.tape_uuid != mounted.tape_uuid || request.acknowledge_tape != mounted.tape_uuid {
            return Err(refusal(
                "--tape and --acknowledge-tape must match the mounted BOT UUID",
            ));
        }
        if request.reason.trim().is_empty()
            || request.idempotency_key.is_nil()
            || request
                .expected_pool_id
                .as_ref()
                .is_some_and(|p| p.is_empty() || p.trim() != p)
        {
            return Err(refusal(
                "finalize requires a reason, non-nil idempotency key and canonical expected pool",
            ));
        }
        standalone_fence_policy(
            self.state.catalog_index(),
            Some(mounted.tape_uuid),
            mounted.endpoint.durable_key(),
            true,
        )?;
        let actor = AuditActor::local_user();
        let actor_fingerprint = crate::catalog_conversion::audit_actor_fingerprint(&actor);
        let fingerprint = crate::catalog_conversion::manual_finalize_request_fingerprint(
            request.tape_uuid,
            request.expected_pool_id.as_deref(),
            &actor_fingerprint,
            request.reason.as_bytes(),
        );
        let pools = self.state.config().tape_pools.clone();
        let (tape, mut request) = crate::mount::prepare_manual_finalize_request(
            self.state.catalog_index(),
            crate::mount::ManualFinalizeTapeAdmission {
                candidate_operation_id: uuid::Uuid::new_v4(),
                actor,
                actor_fingerprint,
                idempotency_key: request.idempotency_key,
                request_fingerprint: fingerprint,
                tape_uuid: request.tape_uuid,
                expected_pool_id: request.expected_pool_id,
                reason: request.reason,
            },
            |id| {
                pools
                    .iter()
                    .find(|p| p.id == id)
                    .cloned()
                    .ok_or_else(|| tonic::Status::failed_precondition("unconfigured pool"))
            },
        )
        .map_err(refusal)?;
        self.flush_audit()?;
        let cfg = crate::write_owner::TerminalWriteConfig {
            checkpoint_journal_dir: self.state.paths().journal_dir.join("checkpoints"),
            audit_dir: self.state.paths().audit_dir.clone(),
            audit_fsync: self.state.config().audit.fsync,
            audit_append_lock: Arc::new(Mutex::new(())),
        };
        let session_id = uuid::Uuid::new_v4();
        self.session(mounted.tape_uuid, session_id, AuditEvent::SessionOpened)?;
        let result = (|| {
            let pending = remanence_state::FileCheckpointJournal::open(
                &cfg.checkpoint_journal_dir,
                request.tape_uuid,
            )
            .and_then(|journal| journal.terminal_finalization_intent())
            .map_err(crate::status_from_state_error)?;
            let (prefix, carried) = if pending.is_none()
                && tape.scheme_id.is_some()
                && tape.highest_protected_ordinal < tape.total_committed_ordinals
            {
                let (plan, state) = crate::write_owner::plan_unretained_epoch_finalization(
                    self.state.catalog_index(),
                    &cfg,
                    &mut mounted.drive,
                    &request,
                )?;
                (
                    Some(plan),
                    state.map(|state| (mounted.drive.identity_generation(), state)),
                )
            } else {
                (None, None)
            };
            if let Some(reply) = crate::write_owner::preflight_manual_finalize_with_prefix(
                self.state.catalog_index(),
                crate::write_owner::ManualFinalizePreflightConfig {
                    checkpoint_journal_dir: &cfg.checkpoint_journal_dir,
                    audit_dir: &cfg.audit_dir,
                    audit_fsync: cfg.audit_fsync,
                    audit_append_lock: &cfg.audit_append_lock,
                },
                tape.voltag.as_deref(),
                &mut request,
                prefix.as_ref(),
            )? {
                return Ok(reply);
            }
            crate::write_owner::finalize_mounted_tape(
                self.state.catalog_index(),
                &cfg,
                &mut mounted.drive,
                &request,
                tape.voltag.as_deref(),
                carried,
                |_, drive| {
                    drive
                        .test_unit_ready()
                        .map_err(|e| tonic::Status::unavailable(e.to_string()))?;
                    drive
                        .prevent_medium_removal()
                        .map_err(|e| tonic::Status::unavailable(e.to_string()))?;
                    crate::write_owner::prepare_drive_for_write(
                        drive,
                        &request.tape_uuid,
                        request.block_size,
                        request.candidate_operation_id,
                        crate::write_owner::WriteMediaPolicy::TerminalAppend,
                    )
                },
                |_, _| {},
            )
        })();
        // Terminal routines use their existing synchronized audit appender.
        self.state.refresh_audit_append_cursor().map_err(refusal)?;
        self.flush_audit()?;
        self.session(
            mounted.tape_uuid,
            session_id,
            if result.is_ok() {
                AuditEvent::SessionClosed
            } else {
                AuditEvent::SessionOrphaned
            },
        )?;
        result.map(|reply| reply.projection).map_err(refusal)
    }
}

impl OperatorMount<'_> {
    /// Import append authority only after replay validates the checkpoint and
    /// parity journals together. Publish parity first and checkpoint last so a
    /// crash can leave only an incomplete (therefore refused) authority pair.
    pub fn import_journals(
        &mut self,
        mounted: &mut MountedTape,
        from: &Path,
    ) -> Result<(), MountError> {
        self.require_durable_mount(mounted)?;
        standalone_fence_policy(
            self.state.catalog_index(),
            Some(mounted.tape_uuid),
            mounted.endpoint.durable_key(),
            true,
        )?;
        mounted.drive.rewind().map_err(refusal)?;
        crate::verify_tape_identity(
            &mut DriveHandleSource(&mut mounted.drive),
            &mounted.tape_uuid,
        )
        .map_err(refusal)?;
        let tape = self
            .state
            .catalog_index()
            .get_tape(&mounted.tape_uuid)
            .map_err(refusal)?
            .ok_or_else(|| refusal("adopt this tape before importing journals"))?;
        let (block_size, parity) = crate::pool_write::selected_tape_geometry(
            &tape,
            tape.pool_id.as_deref().unwrap_or("(unpooled)"),
        )
        .map_err(refusal)?;
        let source_dir = from.join("checkpoints");
        if !source_dir.is_dir() {
            return Err(refusal("import source has no checkpoints directory"));
        }
        let source = remanence_state::FileCheckpointJournal::open(source_dir, mounted.tape_uuid)
            .map_err(refusal)?;
        if !std::fs::symlink_metadata(source.path())
            .map_err(refusal)?
            .is_file()
        {
            return Err(refusal(
                "import checkpoint must be an existing regular file",
            ));
        }
        let _source_lease = source.acquire_exclusive().map_err(refusal)?;
        if _source_lease
            .terminal_finalization_intent()
            .map_err(refusal)?
            .is_some()
        {
            return Err(refusal("pending terminal intent requires complete state recovery, not append journal import"));
        }
        let journal_dir = self.state.paths().journal_dir.clone();
        let staging = tempfile::tempdir_in(&journal_dir).map_err(refusal)?;
        let checkpoints = staging.path().join("checkpoints");
        std::fs::create_dir(&checkpoints).map_err(refusal)?;
        let filename = source
            .path()
            .file_name()
            .ok_or_else(|| refusal("checkpoint path has no filename"))?;
        let staged_checkpoint = checkpoints.join(filename);
        copy_authority(source.path(), &staged_checkpoint)?;
        let staged = remanence_state::FileCheckpointJournal::open(&checkpoints, mounted.tape_uuid)
            .map_err(refusal)?;
        let lease = staged.acquire_exclusive().map_err(refusal)?;
        let last = lease
            .last_record_bounded()
            .map_err(refusal)?
            .ok_or_else(|| refusal("import requires a nonempty checkpoint journal"))?;
        let parity_for_projection = parity.clone();
        let mut publications = Vec::new();
        if let remanence_parity::ParityConfig::Scheme(scheme) = parity {
            let name = self
                .state
                .journal_path(mounted.tape_uuid)
                .file_name()
                .unwrap()
                .to_owned();
            let path = staging.path().join(&name);
            copy_authority(&from.join(&name), &path)?;
            let journal = remanence_parity::FileTapeFileJournal::open(
                &path,
                mounted.tape_uuid,
                block_size,
                scheme,
            )
            .map_err(refusal)?;
            let snapshot = journal.committed_snapshot_bounded().map_err(refusal)?;
            let summary =
                remanence_parity::checked_bounded_resume_summary(&snapshot).map_err(refusal)?;
            crate::pool_write::validate_parity_resume_checkpoint_authority(
                &lease,
                &journal,
                false,
                summary.committed_tape_file_count,
            )
            .map_err(|e| refusal(format!("import journal authority: {e:?}")))?;
            publications.push((path, journal_dir.join(name)));
        } else {
            remanence_state::CheckpointTerminalIndexRecordSource::new_replay_backed_no_parity(
                &lease,
            )
            .map_err(refusal)?;
        }
        let ctx = crate::append_admission::context(
            mounted.tape_uuid,
            block_size,
            last.eod_partition,
            last.eod_lba,
        );
        let _admission = crate::append_admission::admit_append_boundary(
            &mut mounted.drive,
            crate::append_admission::AppendAuthority::Checkpoint { eod: ctx.boundary },
            ctx,
        )
        .map_err(refusal)?;
        let destination = journal_dir.join("checkpoints");
        std::fs::create_dir_all(&destination).map_err(refusal)?;
        publications.push((staged_checkpoint, destination.join(filename)));
        // Conflicts are detected before publishing either member of the pair.
        for (source, dest) in &publications {
            if dest.exists() && !same_authority(source, dest)? {
                return Err(refusal(format!(
                    "local journal {} differs; refusing overwrite",
                    dest.display()
                )));
            }
        }
        for (source, dest) in publications {
            if !dest.exists() {
                std::fs::hard_link(source, &dest).map_err(refusal)?;
                File::open(dest.parent().unwrap())
                    .and_then(|f| f.sync_all())
                    .map_err(refusal)?;
            }
        }
        let parity_journal = match &parity_for_projection {
            remanence_parity::ParityConfig::None => None,
            remanence_parity::ParityConfig::Scheme(scheme) => Some(
                remanence_parity::FileTapeFileJournal::open(
                    self.state.journal_path(mounted.tape_uuid),
                    mounted.tape_uuid,
                    block_size,
                    scheme.clone(),
                )
                .map_err(refusal)?,
            ),
        };
        self.state
            .catalog_index()
            .recover_imported_append_authority(
                &lease,
                parity_journal.as_ref(),
                mounted.endpoint.durable_key().expect("durable mount"),
            )
            .map_err(refusal)?;
        drop(parity_journal);
        if let remanence_parity::ParityConfig::Scheme(scheme) = &parity_for_projection {
            match self
                .state
                .ingest_tape_journal(mounted.tape_uuid, block_size, scheme.clone())
                .map_err(refusal)?
            {
                remanence_state::TapeJournalIngestionOutcome::Pending(_) => {
                    return Err(refusal(
                        "imported parity journal is locked; retry import before appending",
                    ))
                }
                remanence_state::TapeJournalIngestionOutcome::Indexed(_) => {}
            }
        }
        self.state
            .catalog_index()
            .project_checkpoint_record(&last)
            .map_err(refusal)?;
        self.flush_audit()?;
        Ok(())
    }
}
/// Copy a regular authority file without following a final-component symlink.
fn copy_authority(source: &Path, dest: &Path) -> Result<(), MountError> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(source)
        .map_err(refusal)?;
    if !input.metadata().map_err(refusal)?.is_file() {
        return Err(refusal("journal authority must be a regular file"));
    }
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .map_err(refusal)?;
    std::io::copy(&mut input, &mut output).map_err(refusal)?;
    output.sync_all().map_err(refusal)
}
fn same_authority(a: &Path, b: &Path) -> Result<bool, MountError> {
    use std::os::unix::fs::OpenOptionsExt;
    let digest = |path: &Path| {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(refusal)?;
        crate::sha256_reader(&mut file).map_err(refusal)
    };
    Ok(digest(a)? == digest(b)?)
}

/// Sync uncertainty evidence before projecting its tape and endpoint fence.
fn raise_standalone_fence(
    state: &mut StateHandle,
    tape_uuid: TapeUuid,
    endpoint: Option<&remanence_library::DriveKey>,
    reason: &str,
    detail: &str,
) -> Result<(), MountError> {
    let barcode = state
        .catalog_index()
        .get_tape(&tape_uuid)
        .map_err(refusal)?
        .and_then(|t| t.voltag);
    let audit_dir = state.paths().audit_dir.clone();
    let fence = remanence_state::fence::raise(
        state.catalog_index(),
        &audit_dir,
        remanence_state::TapeIoFenceInput {
            tape_uuid,
            barcode,
            reason: reason.into(),
            evidence_json: Some(serde_json::json!({"error": detail}).to_string()),
        },
        endpoint,
        remanence_state::AuditActor::System,
    )
    .map_err(refusal)?;
    let release_options = fence_release_options(&fence);
    eprintln!("standalone tape-I/O fence {}; inspect `rem tape quarantine list`; release with `rem tape quarantine release {} {release_options}`", fence.quarantine_id, fence.quarantine_id);
    state.refresh_audit_append_cursor().map_err(refusal)
}

#[cfg(test)]
mod tests;

impl OperatorMount<'_> {
    /// Discard an explicitly acknowledged suffix through the shared drive-owner path.
    pub fn supersede_tail(
        &mut self,
        mounted: &mut MountedTape,
        acknowledge: &str,
        confirm: impl FnOnce(&str, &[String]) -> Result<(), String>,
    ) -> Result<Vec<String>, MountError> {
        self.require_durable_mount(mounted)?;
        standalone_fence_policy(
            self.state.catalog_index(),
            Some(mounted.tape_uuid),
            mounted.endpoint.durable_key(),
            true,
        )?;
        if mounted.generation.is_none() {
            return Err(refusal(
                "tape generation is required before superseding a tail",
            ));
        }
        self.flush_audit()?;
        let cfg = crate::write_owner::TerminalWriteConfig {
            checkpoint_journal_dir: self.state.paths().journal_dir.join("checkpoints"),
            audit_dir: self.state.paths().audit_dir.clone(),
            audit_fsync: true,
            audit_append_lock: Arc::new(Mutex::new(())),
        };
        let session_id = uuid::Uuid::new_v4();
        self.session(mounted.tape_uuid, session_id, AuditEvent::SessionOpened)?;
        let before = mounted.drive.media_write_dispatch_count();
        let result = supersede_mounted_tail(
            self.state.catalog_index(),
            &cfg,
            &mut mounted.drive,
            mounted.tape_uuid,
            mounted.identity_generation,
            acknowledge,
            AuditActor::local_user(),
            confirm,
        );
        let uncertain = result.is_err() && before != mounted.drive.media_write_dispatch_count();
        self.unusable |= uncertain;
        self.session(
            mounted.tape_uuid,
            session_id,
            if uncertain {
                AuditEvent::SessionOrphaned
            } else {
                AuditEvent::SessionClosed
            },
        )?;
        self.state.refresh_audit_append_cursor().map_err(refusal)?;
        result.map_err(refusal)
    }
}

/// Explicit recovery shared by standalone ownership and the library drive actor.
/// Confirmation, rescan, audit, and mutation all retain the same exclusive lease.
#[allow(clippy::too_many_arguments)]
pub(crate) fn supersede_mounted_tail(
    index: &mut remanence_state::CatalogIndex,
    cfg: &crate::write_owner::TerminalWriteConfig,
    drive: &mut DriveHandle,
    tape_uuid: [u8; 16],
    identity_generation: u64,
    acknowledge: &str,
    actor: AuditActor,
    confirm: impl FnOnce(&str, &[String]) -> Result<(), String>,
) -> Result<Vec<String>, tonic::Status> {
    use crate::append_admission::{
        admit_append_boundary, context, AdmissionError, AppendAuthority,
    };
    use tonic::Status;
    let failure = |e: String| Status::failed_precondition(e);
    let tape = index
        .get_tape(&tape_uuid)
        .map_err(crate::status_from_state_error)?
        .ok_or_else(|| failure("adopt the tape before superseding its tail".into()))?;
    if tape.cartridge_generation.is_none() {
        return Err(failure("tape generation is required".into()));
    }
    if tape.terminal_finalization.is_some() {
        return Err(failure("terminal finalization prevents tail discard; retry FinalizeTape with its original idempotency key. If rolled-back state cannot resume that intent, restore a newer copy of the state directory; supersede-tail cannot clear a stale terminal intent".into()));
    }
    if tape.kind != "data"
        || !matches!(tape.state.as_str(), "ready" | "recovery_required")
        || tape.terminal_finalization.is_some()
    {
        return Err(failure(
            "tape is sealed, retired, or requires terminal recovery".into(),
        ));
    }
    if !index
        .tape_io_admission_conflicts(&tape_uuid, tape.voltag.as_deref())
        .map_err(crate::status_from_state_error)?
        .is_empty()
    {
        return Err(failure(
            "release the tape I/O fence before superseding".into(),
        ));
    }
    let uuid = uuid::Uuid::from_bytes(tape_uuid).to_string();
    let label = tape.voltag.as_deref().unwrap_or(&uuid);
    if acknowledge != uuid && acknowledge != label {
        return Err(failure(
            "acknowledgment does not identify the loaded tape".into(),
        ));
    }
    let (size, parity) = crate::pool_write::selected_tape_geometry(
        &tape,
        tape.pool_id.as_deref().unwrap_or("(unpooled)"),
    )
    .map_err(|e| failure(e.to_string()))?;
    let journal =
        remanence_state::FileCheckpointJournal::open(&cfg.checkpoint_journal_dir, tape_uuid)
            .map_err(crate::status_from_state_error)?;
    let terminal_guidance = "retry FinalizeTape with its original idempotency key. If rolled-back state cannot resume that intent, restore a newer copy of the state directory; supersede-tail cannot clear a stale terminal intent";
    if journal
        .terminal_finalization_intent()
        .map_err(|e| {
            failure(format!(
                "cannot establish terminal authority: {e}; {terminal_guidance}"
            ))
        })?
        .is_some()
    {
        return Err(failure(format!(
            "terminal finalization intent prevents tail discard; {terminal_guidance}"
        )));
    }
    let lease = journal
        .acquire_exclusive()
        .map_err(crate::status_from_state_error)?;
    if lease
        .terminal_finalization_intent()
        .map_err(crate::status_from_state_error)?
        .is_some()
    {
        return Err(failure(
            "terminal finalization intent prevents tail discard; retry FinalizeTape with its original idempotency key. If rolled-back state cannot resume that intent, restore a newer copy of the state directory; supersede-tail cannot clear a stale terminal intent".into(),
        ));
    }
    let last = lease
        .last_record_bounded()
        .map_err(crate::status_from_state_error)?;
    let make_context = || {
        let mut ctx = context(
            tape_uuid,
            size,
            last.as_ref().map_or(0, |r| r.eod_partition),
            last.as_ref().map_or(0, |r| r.eod_lba),
        );
        ctx.inspect_all = true;
        ctx
    };
    let authority = || {
        last.as_ref()
            .map_or(AppendAuthority::Fresh, |_| AppendAuthority::Checkpoint {
                eod: make_context().boundary,
            })
    };
    drive
        .bind_identity_generation(identity_generation)
        .map_err(|e| failure(e.to_string()))?;
    drive.rewind().map_err(|e| failure(e.to_string()))?;
    crate::verify_tape_identity(&mut DriveHandleSource(drive), &tape_uuid)
        .map_err(|e| failure(e.to_string()))?;
    let config = drive.read_config().map_err(|e| failure(e.to_string()))?;
    crate::pool_write::require_rewritable_object_media(config)
        .map_err(|e| failure(e.to_string()))?;
    crate::drive_mode::configure_fixed_uncompressed_write(drive, config, size)
        .map_err(|e| failure(e.to_string()))?;
    let (found, object_ids, missing_filemark) =
        match admit_append_boundary(drive, authority(), make_context()) {
            Err(AdmissionError::TailBeyondKnownState {
                found, object_ids, ..
            }) => (found, object_ids, false),
            Err(AdmissionError::MissingBootstrapFilemark) => (Vec::new(), Vec::new(), true),
            Err(e) => return Err(failure(e.to_string())),
            Ok(_) => return Err(failure("no foreign tail requires superseding".into())),
        };
    confirm(&format!("SUPERSEDE {label}"), &found).map_err(failure)?;
    let mut ctx = make_context();
    ctx.confirmed_discard = Some(found.clone());
    ctx.allow_missing_bootstrap_filemark = missing_filemark;
    let _admission =
        admit_append_boundary(drive, authority(), ctx).map_err(|e| failure(e.to_string()))?;
    drive
        .bind_identity_generation(identity_generation)
        .map_err(|e| failure(e.to_string()))?;
    crate::audit_projection::append_and_project_audit(
        index,
        &cfg.audit_dir,
        true,
        &cfg.audit_append_lock,
        crate::audit_projection::ProjectedAuditInput {
            actor: actor.clone(),
            source_layer: SourceLayer::Layer5,
            operation_id: None,
            session_id: None,
            idempotency_key: None,
            event: AuditEvent::RecoveryEvent,
            subject_kind: "tape",
            subject_id: Some(uuid.clone()),
            detail: BTreeMap::from([
                (
                    "action".into(),
                    ciborium::Value::Text("supersede_tail".into()),
                ),
                (
                    "discarded_object_ids".into(),
                    ciborium::Value::Array(
                        object_ids.into_iter().map(ciborium::Value::Text).collect(),
                    ),
                ),
                (
                    "found".into(),
                    ciborium::Value::Array(
                        found.iter().cloned().map(ciborium::Value::Text).collect(),
                    ),
                ),
                (
                    "expected_eod_lba".into(),
                    ciborium::Value::Integer(make_context().boundary.lba.into()),
                ),
            ]),
        },
    )?;
    let result = (|| {
        let mut sink = remanence_library::DriveHandleSink(drive);
        if last.is_none() {
            let bootstrap = crate::pool_write::build_tape_bootstrap(
                tape_uuid,
                size,
                parity.clone(),
                remanence_state::audit::writer_identity(env!("CARGO_PKG_VERSION"))
                    .written_at()
                    .map_err(|e| e.to_string())?,
                env!("CARGO_PKG_VERSION"),
            );
            crate::pool_write::write_tape_bootstrap(&mut sink, &bootstrap)
                .map_err(|e| e.to_string())?;
        } else {
            remanence_library::BlockSink::write_filemarks(&mut sink, 1)
                .map_err(|e| e.to_string())?;
        }
        if let remanence_parity::ParityConfig::Scheme(scheme) = &parity {
            let dir = cfg
                .checkpoint_journal_dir
                .parent()
                .ok_or("missing parity journal directory")?;
            let path = dir.join(format!("{}.remjournal", crate::bytes_to_hex(&tape_uuid)));
            let mut journal =
                remanence_parity::FileTapeFileJournal::open(path, tape_uuid, size, scheme.clone())
                    .map_err(|e| e.to_string())?;
            crate::append_admission::discard_reconciled_parity_suffix(&mut journal, &lease)?;
        }
        Ok::<_, String>(())
    })();
    drive.clear_append_admission();
    if let Err(error) = result {
        crate::write_owner::raise_tape_io_fence(
            index,
            cfg,
            remanence_state::TapeIoFenceInput {
                tape_uuid,
                barcode: tape.voltag,
                reason: "supersede_tail_failed".into(),
                evidence_json: Some(serde_json::json!({"error":error}).to_string()),
            },
            drive
                .endpoint()
                .durable_key()
                .filter(|key| matches!(key, remanence_library::DriveKey::Standalone { .. })),
        )
        .map_err(crate::status_from_state_error)?;
        return Err(Status::unavailable(error));
    }
    index
        .set_append_recovery_required(tape_uuid, false)
        .map_err(|e| {
            Status::failed_precondition(format!(
                "tail discard completed, but clearing tape recovery state failed: {e}"
            ))
        })?;
    Ok(found)
}
