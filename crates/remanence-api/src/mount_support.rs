//! Cartridge mount, device-lock, read, fence and journal-import support.

use crate::{LtoGen, TapeUuid};
pub use remanence_library::MountEndpoint;
use remanence_library::{
    AccessPolicy, DriveHandle, DriveHandleSource, LibraryHandle, MediaFamily, MediaReadiness,
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
pub(crate) struct LockedAuditedDevice {
    pub(crate) drive: Option<DriveHandle>,
    lock: Option<Arc<DeviceLock>>,
    pub(crate) audit_error: Arc<Mutex<Option<String>>>,
    pub(crate) endpoint: MountEndpoint,
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
pub(crate) fn raise_standalone_fence(
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
#[path = "mount_backend/tests.rs"]
mod tests;
