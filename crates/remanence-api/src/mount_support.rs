//! Cartridge mount, device-lock, read, fence and journal-import support.

use crate::{LtoGen, TapeUuid};
pub use remanence_library::MountEndpoint;
use remanence_library::{
    AccessPolicy, DriveHandle, DriveHandleSource, DriveKey, LibraryHandle, MediaFamily,
    MediaReadinessWaitOptions, SgTransport, StaticAllowlist,
};
use remanence_state::{CatalogIndex, GenerationSource, LabelProvenance, StateHandle};
use std::{
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

/// Shared recovery advice for startup, standalone write admission and tape init.
pub(crate) fn fence_release_options(fence: &remanence_state::TapeIoFenceRecord) -> &'static str {
    if fence.reason == "native_log_behind_projection" {
        "--after-settled-inventory --ack TEXT --force, after restoring the exact tape log or rebuilding the catalog (or retire the tape)"
    } else if standalone_fence_releasable(fence) {
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

/// Refuse standalone data commands against the shared authority before reservation.
/// Physical scan rescue exempts only cause 14; excluded dependencies still refuse.
pub fn standalone_data_admission(
    index: &CatalogIndex,
    tape: TapeUuid,
    rescue_scan: bool,
) -> Result<(), MountError> {
    use remanence_state::library_log as lib;
    if index.library_runtime().is_none() {
        return Err(refusal(
            "standalone admission requires shared LibraryAuthority",
        ));
    }
    if !rescue_scan {
        for fence in index.list_active_tape_io_fences().map_err(refusal)? {
            if fence.tape_uuid.as_slice() == tape
                && crate::tape_admission::typed_cause(index, &fence)
                    == Some(lib::FenceCause::NativeLogBehindProjection)
            {
                return Err(refusal(crate::tape_admission::fence_payload(
                    index,
                    tape,
                    &fence,
                    format!(
                        "standalone data command blocked by active tape-I/O fence {}: {}",
                        fence.quarantine_id, fence.reason
                    ),
                    true,
                    lib::AuditActor::local_user(),
                )));
            }
        }
    }
    if let Some(message) = crate::tape_admission::contradicted_payload(
        index,
        tape,
        "standalone data command",
        lib::AuditActor::local_user(),
    ) {
        return Err(refusal(message));
    }
    Ok(())
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
    /// Finish an actor-owned mount, reporting ALLOW/RELEASE failures before reply.
    pub(crate) fn finish(mut self) -> Result<(), MountError> {
        let removal = self.drive.finish_medium_removal_guard().map_err(refusal);
        let reservation = if std::mem::take(&mut self.reserved) {
            self.drive.release_reservation().map_err(refusal)
        } else {
            Ok(())
        };
        removal.and(reservation)
    }

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
    #[error("{0}")]
    Actor(crate::engine::drive_adapter::ActorFailure),
    /// The object checkpoint committed before later automatic work failed.
    #[error("{failure}")]
    Committed {
        receipt: Box<crate::MountedWriteResult>,
        reason: &'static str,
        failure: Box<MountError>,
    },
    /// A durable terminal projection records hardware early warning.
    #[error("tape closed after hardware early warning; continue on the next tape")]
    EarlyWarning,
}
pub(crate) fn refusal(error: impl std::fmt::Display) -> MountError {
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
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Err(refusal(format!(
                "drive {serial} is locked by a live writer or another process; not clearing its medium-removal lock (device lock {}: {error})",
                path.display()
            )));
        }
        return Err(refusal(format!("device lock {}: {error}", path.display())));
    }
    Ok(DeviceLock(file))
}

/// Physical removal recovery owns only the device lock and audit sink. It never
/// constructs an OperatorMount or gains write/session admission methods.
pub(crate) struct LockedAuditedDevice {
    pub(crate) drive: Option<DriveHandle>,
    lock: Option<Arc<DeviceLock>>,
    pub(crate) audit_error: Arc<Mutex<Option<String>>>,
    pub(crate) endpoint: MountEndpoint,
}
impl LockedAuditedDevice {
    fn open(drive: DriveHandle, state: &StateHandle, io: &mut LockIo) -> Result<Self, MountError> {
        let lock = match drive.endpoint() {
            MountEndpoint::Durable(remanence_library::DriveKey::Standalone { drive_serial }) => {
                Some(Arc::new(lock_device(io, drive_serial)?))
            }
            MountEndpoint::Ephemeral { .. } => None,
            _ => return Err(refusal("operator mount requires a standalone endpoint")),
        };
        Self::with_lock(drive, state, lock)
    }
    fn with_lock(
        mut drive: DriveHandle,
        state: &StateHandle,
        lock: Option<Arc<DeviceLock>>,
    ) -> Result<Self, MountError> {
        drive.set_motion_authority(Arc::new(state.library_runtime()));
        drive.reconcile_startup_motion().map_err(refusal)?;
        let audit_error = Arc::new(Mutex::new(None));
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
        state
            .library_runtime()
            .ensure_motion_allowed()
            .map_err(refusal)?;
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

/// Drive device locks live here, one file per VPD serial.
pub const DEVICE_LOCK_ROOT: &str = "/run/lock/rem";

/// Serial-device exclusion retained through removal recovery and return MOVE.
/// Authority ownership is installed on the library before the first UNLOAD.
pub(crate) struct UnloadRecoveryGuard {
    _device: DeviceLock,
}
impl std::fmt::Debug for UnloadRecoveryGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnloadRecoveryGuard")
            .finish_non_exhaustive()
    }
}
impl UnloadRecoveryGuard {
    fn acquire(drive: &mut DriveHandle, lock_root: &Path) -> Result<Self, MountError> {
        let root = lock_root.to_owned();
        let device = lock_device(
            &mut move |_, logical| {
                Ok(root.join(
                    logical
                        .strip_prefix(DEVICE_LOCK_ROOT)
                        .expect("device lock namespace"),
                ))
            },
            &drive.drive().serial,
        )?;
        Ok(Self { _device: device })
    }
    fn finish(&self, result: Result<(), MountError>) -> Result<(), MountError> {
        result
    }
}

/// Refuse a library serial that `config` does not list. Motion authority comes from one
/// state's configuration; installing it on a library that configuration does not own would
/// take that state's lock and write into its library log.
pub fn require_configured_library(
    config: &remanence_state::RemConfig,
    config_path: &Path,
    serial: &str,
) -> Result<(), MountError> {
    if config
        .libraries
        .iter()
        .any(|entry| serial == entry.serial.trim())
    {
        Ok(())
    } else {
        Err(MountError::Refused(format!(
            "library {serial} is not listed in {}",
            config_path.display()
        )))
    }
}

/// Install invocation authority under the state lease before any physical unload.
/// Existing daemon/mount owners already carry this capability. A direct caller
/// supplies its configuration; startup reconciliation occurs before admission.
pub fn bind_library_motion_authority(
    library: &mut LibraryHandle,
    config: Option<&Path>,
) -> Result<(), MountError> {
    let Some(path) = config else {
        return if library.has_motion_authority() {
            Ok(())
        } else {
            Err(refusal(
                "typed motion authority or explicit configuration is required before UNLOAD",
            ))
        };
    };
    let config = remanence_state::load_config(path).map_err(refusal)?;
    require_configured_library(&config, path, &library.library().serial)?;
    let paths = remanence_state::StatePaths::from_config(path, &config);
    let manifest = remanence_state::authority_manifest::AuthorityManifest::preflight(&paths)
        .map_err(refusal)?;
    if library.has_motion_authority() {
        let supplied = manifest
            .ok_or_else(|| refusal("supplied configuration has no bound motion authority"))?;
        let actual = library
            .motion_authority_identity()
            .ok_or_else(|| refusal("installed motion authority has no configuration binding"))?;
        if actual.library_uuid != supplied.library_uuid()
            || actual.library_root != supplied.library_root()
            || actual.tape_root != supplied.tape_root()
        {
            return Err(refusal(
                "installed motion authority differs from supplied configuration",
            ));
        }
        return Ok(());
    }
    let state =
        Arc::new(remanence_state::StateLockGuard::acquire(&paths.state_dir).map_err(refusal)?);
    state.ensure_no_catalog_reset().map_err(refusal)?;
    let runtime =
        remanence_state::library_runtime::LibraryRuntime::open(&paths, config.audit.fsync, state)
            .map_err(refusal)?;
    library.set_motion_authority(Arc::new(runtime));
    library.reconcile_startup_motion().map_err(refusal)
}

/// Try normal UNLOAD after authority admission. Only 05/53/02 permits
/// serial-device-locked removal recovery; its guard spans the return MOVE.
pub fn unload_library_bay(
    library: &mut LibraryHandle,
    bay: u16,
    destination: Option<u16>,
    policy: &dyn remanence_library::AccessPolicy,
    config: Option<&Path>,
    lock_root: &Path,
) -> Result<UnloadOutcome, MountError> {
    unload_library_bay_with_admission(
        library,
        bay,
        destination,
        policy,
        config,
        lock_root,
        &mut || Ok(()),
    )
}

/// Keep the owner's physical-dispatch check across removal-lock recovery retries.
pub(crate) fn unload_library_bay_with_admission(
    library: &mut LibraryHandle,
    bay: u16,
    destination: Option<u16>,
    policy: &dyn remanence_library::AccessPolicy,
    config: Option<&Path>,
    lock_root: &Path,
    admit: &mut dyn FnMut() -> Result<(), String>,
) -> Result<UnloadOutcome, MountError> {
    bind_library_motion_authority(library, config)?;
    match library.unload_with_admission(bay, destination, policy, admit) {
        Ok(()) => return Ok(UnloadOutcome::Unloaded),
        Err(remanence_library::UnloadError::DriveUnload(ref error))
            if error.is_medium_removal_prevented() => {}
        Err(error) => return Err(refusal(error)),
    }
    let mut drive = library.open_drive(bay, policy).map_err(|error| {
        refusal(format!(
            "drive refused UNLOAD (medium removal prevented); opening it for recovery failed: {error}"
        ))
    })?;
    let serial = drive.drive().serial.clone();
    let guard = UnloadRecoveryGuard::acquire(&mut drive, lock_root)?;
    let result = library
        .unload_recovering_removal_with_admission(bay, destination, policy, admit)
        .map_err(refusal);
    guard.finish(result)?;
    tracing::warn!(
        bay = format_args!("0x{bay:04x}"),
        %serial,
        "cleared a stranded medium-removal lock under the device lock and unloaded"
    );
    Ok(UnloadOutcome::ClearedStrandedPrevent)
}

/// Whether an unload had to clear a medium-removal lock left by a writer that died holding it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnloadOutcome {
    Unloaded,
    ClearedStrandedPrevent,
}

/// A daemon drive unload failure: the plain UNLOAD's own error keeps its type so callers can
/// tell an inventory refusal from a transport failure.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DriveUnloadError {
    #[error("drive UNLOAD failed: {0}")]
    Drive(remanence_library::DriveOpError),
    #[error("{0}")]
    Recovery(MountError),
}

/// The daemon has already reserved this bay; the serial flock additionally
/// excludes external processes. Return its guard for the caller's changer MOVE.
pub(crate) fn unload_drive(
    drive: &mut DriveHandle,
    lock_root: &Path,
    admit: &mut dyn FnMut() -> Result<(), String>,
) -> Result<Option<UnloadRecoveryGuard>, DriveUnloadError> {
    match drive.unload_with_admission(admit) {
        Ok(()) => return Ok(None),
        Err(error) if error.is_medium_removal_prevented() => {}
        Err(error) => return Err(DriveUnloadError::Drive(error)),
    }
    let serial = drive.drive().serial.clone();
    let guard =
        UnloadRecoveryGuard::acquire(drive, lock_root).map_err(DriveUnloadError::Recovery)?;
    guard
        .finish((|| {
            drive.allow_medium_removal().map_err(refusal)?;
            drive.unload_with_admission(admit).map_err(refusal)
        })())
        .map_err(DriveUnloadError::Recovery)?;
    tracing::warn!(
        %serial,
        "cleared a stranded medium-removal lock under the device lock and unloaded"
    );
    Ok(Some(guard))
}

/// Standalone invocation state. A second mount cannot consume another cartridge.
pub struct OperatorMount<'a> {
    pub(crate) state: &'a mut StateHandle,
    pub(crate) context: LockedAuditedDevice,
    path: PathBuf,
    pub(crate) unusable: bool,
    pub(crate) checkpoints_reconciled: bool,
}
impl<'a> OperatorMount<'a> {
    /// Finish after a common read consumer has consumed/released MountedTape.
    /// Refresh the state append cursor and surface any drive-audit failure.
    pub fn finish(mut self) -> Result<(), MountError> {
        self.flush_audit()
    }

    /// Catalog view for the common mounted-object read, verify, and export paths.
    pub fn catalog_index(&mut self) -> &CatalogIndex {
        self.state.catalog()
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
        Self::from_context(path, context, state)
    }
    /// Adopt an already placed bay without reopening or relabelling the drive.
    pub(crate) fn from_placed_library(
        path: &Path,
        drive: DriveHandle,
        state: &'a mut StateHandle,
        drive_serial: &str,
        lock_root: &Path,
    ) -> Result<Self, MountError> {
        let root = lock_root.to_owned();
        let lock = lock_device(
            &mut move |_, logical| {
                Ok(root.join(
                    logical
                        .strip_prefix(DEVICE_LOCK_ROOT)
                        .expect("device lock namespace"),
                ))
            },
            drive_serial,
        )?;
        let context = LockedAuditedDevice::with_lock(drive, state, Some(Arc::new(lock)))?;
        Self::from_context(path, context, state)
    }
    fn from_context(
        path: &Path,
        context: LockedAuditedDevice,
        state: &'a mut StateHandle,
    ) -> Result<Self, MountError> {
        if let Some(endpoint) = context.endpoint.durable_key() {
            state.replay_audit_projection().map_err(refusal)?;
            if matches!(endpoint, DriveKey::Standalone { .. }) {
                standalone_fence_policy(state.catalog(), None, Some(endpoint), false)?;
                // C8-G2 / DEC-2: only library-log sessions at this endpoint
                // are recovered here; native writes await their tape's next open.
                state
                    .recover_sessions_after_restart(Some(endpoint), |_, _| Ok(()))
                    .map_err(refusal)?;
                standalone_fence_policy(state.catalog(), None, Some(endpoint), false)?;
            } else if let Some(session) = state
                .catalog()
                .non_terminal_sessions()
                .map_err(refusal)?
                .into_iter()
                .find(|session| session.drive_key.as_ref() == Some(endpoint))
            {
                // Bay sessions belong to daemon recovery. Never orphan them or
                // raise standalone restart fences from a placed-library caller.
                return Err(refusal(format!(
                    "library endpoint {} has open session {}; run daemon recovery before archive write",
                    endpoint.encode(), session.session_id
                )));
            }
        }
        Ok(Self {
            state,
            context,
            path: path.into(),
            unusable: false,
            checkpoints_reconciled: false,
        })
    }
    /// Apply metadata changes only while the durable BOT proof remains valid.
    pub fn mutate_catalog<T>(
        &mut self,
        mounted: &MountedTape,
        operation: impl FnOnce(&mut StateHandle) -> Result<T, String>,
    ) -> Result<T, MountError> {
        self.require_durable_mount(mounted)?;
        standalone_fence_policy(
            self.state.catalog(),
            Some(mounted.tape_uuid),
            mounted.endpoint.durable_key(),
            true,
        )?;
        if let Some(key @ remanence_library::DriveKey::Standalone { .. }) =
            mounted.endpoint.durable_key()
        {
            self.state
                .library_runtime()
                .admit_drive_data(
                    key,
                    remanence_state::library_runtime::RefusalSubject::Tape(Some(mounted.tape_uuid)),
                    remanence_state::AuditActor::local_user(),
                )
                .map_err(|e| refusal(e.to_string()))?;
        }
        if mounted.identity_generation != mounted.drive.identity_generation() {
            return Err(refusal("media identity invalidated"));
        }
        self.flush_audit()?;
        operation(self.state).map_err(refusal)
    }

    pub(crate) fn flush_audit(&mut self) -> Result<(), MountError> {
        self.context.flush_audit(self.state)
    }
    pub(crate) fn require_durable_mount(&self, mounted: &MountedTape) -> Result<(), MountError> {
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
        let result = prove_mount(&mut drive, self.state.catalog(), expected_uuid, &path);
        match result {
            Ok((tape_uuid, label, generation)) => {
                let proof_generation = drive.identity_generation();
                if matches!(
                    drive.endpoint().durable_key(),
                    Some(DriveKey::Standalone { .. })
                ) {
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
    if poll.readiness
        != remanence_library::MediaReadiness::new(remanence_library::MediaReadinessKind::Ready)
    {
        return Err(refusal(format!(
            "{}; insert the cartridge and retry",
            poll.readiness
        )));
    }
    drive.prevent_medium_removal().map_err(refusal)?;
    drive.rewind().map_err(refusal)?;
    let config = drive.read_config().map_err(refusal)?;
    let library_bay = matches!(drive.endpoint().durable_key(), Some(DriveKey::Bay { .. }));
    let tape_uuid = if library_bay {
        // The pool caller has already selected an exact UUID. Use the same
        // identity verifier/error as the direct writer, not standalone BOT classification.
        expected.ok_or_else(|| refusal("placed library mount requires a selected tape UUID"))?
    } else {
        let classification = crate::classify_bot_from_source(&mut DriveHandleSource(drive));
        match classification.classification {
            crate::BotClassification::OursBootstrap { uuid, .. } => uuid,
            crate::BotClassification::BlankCheckEod => {
                return Err(MountError::Blank { path: path.into() })
            }
            other => return Err(refusal(format!("BOT classification refused: {other:?}"))),
        }
    };
    if let Some(expected) = expected {
        drive.rewind().map_err(refusal)?;
        crate::verify_tape_identity(&mut DriveHandleSource(drive), &expected).map_err(|error| {
            if library_bay {
                MountError::PoolWrite(Box::new(error.into()))
            } else {
                refusal(error)
            }
        })?;
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
        if let Some(authority) = self.index.library_runtime() {
            self.library.set_motion_authority(Arc::new(authority));
        }
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

impl OperatorMount<'_> {
    /// Ruling 32-L1: the library log does not yet record an authority import, so
    /// import refuses before it checks, stages, publishes or writes anything. The
    /// CLI refuses earlier, before it enumerates or opens a device; this is the
    /// second line of defence and is unreachable from any production caller. Each
    /// refusal is counted in the library log as one `RefusalRecorded` naming the
    /// mounted tape, and is the only record it appends.
    pub fn import_journals(
        &mut self,
        mounted: &mut MountedTape,
        _from: &Path,
    ) -> Result<(), MountError> {
        self.state.library_runtime().record_refusal(
            remanence_state::library_log::RefusalCode::JournalImportDeferred,
            remanence_state::library_runtime::RefusalSubject::Tape(Some(mounted.tape_uuid)),
            remanence_state::AuditActor::local_user(),
        );
        Err(refusal(JOURNAL_IMPORT_DEFERRED))
    }
}
/// Error text of the 32-L1 refusal of a journal import. The text starts with the code.
pub const JOURNAL_IMPORT_DEFERRED: &str =
    "JournalImportDeferred: importing checkpoint journals is deferred; nothing was imported or changed";
/// Ruling 29 records initialization uncertainty as a typed standalone drive hold.
pub(crate) fn raise_standalone_fence(
    state: &mut StateHandle,
    tape_uuid: TapeUuid,
    endpoint: Option<&remanence_library::DriveKey>,
    reason: &str,
    detail: &str,
) -> Result<(), MountError> {
    // Ruling 29 supersedes C8-G10: the planned tape is detail on a DriveFault,
    // not a tape fence for an identity that initialization has not provisioned.
    let runtime = state.library_runtime();
    let error = if let Some(key @ remanence_library::DriveKey::Standalone { .. }) = endpoint {
        if let Some(cause) = crate::write_owner::write_fence_cause(reason) {
            match runtime.raise_standalone_drive_hold(
                key,
                tape_uuid,
                cause,
                crate::append_admission::tape_fence_detail(
                    None,
                    Some(
                        &serde_json::json!({
                            "error": detail,
                            "tape_uuid": uuid::Uuid::from_bytes(tape_uuid),
                        })
                        .to_string(),
                    ),
                ),
            ) {
                Ok(()) => return Ok(()),
                Err(error) => error.to_string(),
            }
        } else {
            runtime.fence_motion_after_evidence_failure();
            format!("unrepresented write fence cause: {reason}")
        }
    } else {
        runtime.fence_motion_after_evidence_failure();
        "no standalone drive to hold".into()
    };
    Err(refusal(format!(
        "cannot durably record standalone write uncertainty for {} at {:?} ({reason}): {detail}; the drive hold was not recorded: {error}; motion is fenced and recovery is required",
        uuid::Uuid::from_bytes(tape_uuid), endpoint
    )))
}

#[cfg(test)]
mod tests;

impl OperatorMount<'_> {
    /// Reconcile durable checkpoint authority before any standalone catalog decision.
    pub fn reconcile_checkpoints(&mut self) -> Result<(), MountError> {
        if !self.checkpoints_reconciled {
            crate::reconcile_checkpoint_journal_projections(self.state).map_err(refusal)?;
            self.checkpoints_reconciled = true;
        }
        Ok(())
    }
}

#[cfg(test)]
mod unload_tests;

#[cfg(test)]
mod l1b_admission_tests {
    use super::*;
    use crate::tape_admission::test_support::{assert_code, Fixture, B, T};
    use remanence_state::library_log as lib;

    #[test]
    fn l1b_standalone_authority_and_scan_boundary() {
        for (contradicted, scan, expected) in [
            (false, false, Some(1)),
            (false, true, None),
            (true, false, Some(2)),
            (true, true, Some(2)),
        ] {
            let mut f = Fixture::new();
            if contradicted {
                f.contradict(T);
            } else {
                f.raise(T, lib::FenceCause::NativeLogBehindProjection);
            }
            let index = f.index();
            let result = standalone_data_admission(&index, T, scan);
            if let Some(code) = expected {
                let text = result.unwrap_err().to_string();
                assert!(
                    text.starts_with(if code == 1 {
                        "TapeLogBehindProjection: "
                    } else {
                        "ContradictedTapeAdmission: "
                    }),
                    "{text}"
                );
            } else {
                result.unwrap();
            }
            // A different loaded identity does not inherit this tape's fence.
            standalone_data_admission(&index, B, false).unwrap();
            drop(index);
            let records = f.refusals();
            assert_eq!(records.len(), usize::from(expected.is_some()));
            if let Some(code) = expected {
                assert_code(&records[0], T, code);
            }
        }
        let f = Fixture::new();
        let catalog = remanence_state::projector::open(&f.root.paths().sqlite_path).unwrap();
        let index = catalog.write_handle();
        assert_eq!(
            standalone_data_admission(&index, T, true)
                .unwrap_err()
                .to_string(),
            "standalone admission requires shared LibraryAuthority"
        );
    }

    #[test]
    fn l1b_cause14_release_advice_at_all_consumers() {
        use std::io::{Read, Seek};
        for cause in [
            lib::FenceCause::NativeLogBehindProjection,
            lib::FenceCause::TransferError,
        ] {
            let f = Fixture::new();
            f.raise(T, cause);
            let index = f.index();
            let fences = index.list_active_tape_io_fences().unwrap();
            let expected = if cause == lib::FenceCause::NativeLogBehindProjection {
                "--after-settled-inventory --ack TEXT --force, after restoring the exact tape log or rebuilding the catalog (or retire the tape)"
            } else {
                "--after-settled-inventory --ack TEXT"
            };
            assert_eq!(fence_release_options(&fences[0]), expected);
            let write = standalone_fence_policy(&index, Some(T), None, true)
                .unwrap_err()
                .to_string();
            assert!(
                write.contains(&format!("{} {expected}`", fences[0].quarantine_id)),
                "{write}"
            );
            let init = crate::tape_init::tape_init_fence_refusal(&fences, None, Some(&T)).unwrap();
            assert!(
                init.contains(&format!("{} {expected}`", fences[0].quarantine_id)),
                "{init}"
            );
            let log = tempfile::tempfile().unwrap();
            let mut reader = log.try_clone().unwrap();
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(move || log.try_clone().unwrap())
                .finish();
            crate::tests::ensure_log_capture_anchor();
            tracing::subscriber::with_default(subscriber, || {
                crate::startup_guard::reject_active_tape_io_fences_on_startup(&index).unwrap()
            });
            reader.rewind().unwrap();
            let mut text = String::new();
            reader.read_to_string(&mut text).unwrap();
            assert!(
                text.contains(&format!("{} {expected}", fences[0].quarantine_id)),
                "{text}"
            );
            let mut standalone = fences[0].clone();
            standalone.endpoint = Some(remanence_library::DriveKey::Standalone {
                drive_serial: "ADVICE".into(),
            });
            assert_eq!(
                fence_release_options(&standalone),
                if cause == lib::FenceCause::NativeLogBehindProjection {
                    expected
                } else {
                    "--ack TEXT"
                }
            );
        }
    }
}
