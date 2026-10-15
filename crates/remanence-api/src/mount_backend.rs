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
    _device_lock: Option<Arc<File>>,
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
fn lock_device(io: &mut LockIo, serial: &str) -> Result<File, MountError> {
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
    Ok(file)
}

/// Standalone invocation state. A second mount cannot consume another cartridge.
pub struct OperatorMount<'a> {
    state: &'a mut StateHandle,
    drive: Option<DriveHandle>,
    path: PathBuf,
    _lock: Option<Arc<File>>,
    audit_error: Arc<Mutex<Option<String>>>,
    endpoint: MountEndpoint,
    unusable: bool,
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
    /// Transport and lock filesystem seam; uses the complete production adapter.
    pub fn with_transport_for_test(
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
        mut drive: DriveHandle,
        state: &'a mut StateHandle,
        io: &mut LockIo,
    ) -> Result<Self, MountError> {
        let lock = match drive.endpoint() {
            MountEndpoint::Durable(remanence_library::DriveKey::Standalone { drive_serial }) => {
                Some(Arc::new(lock_device(io, drive_serial)?))
            }
            MountEndpoint::Ephemeral { .. } => None,
            _ => return Err(refusal("operator mount requires a standalone endpoint")),
        };
        if let Some(endpoint) = drive.endpoint().durable_key() {
            state.replay_audit_projection().map_err(refusal)?;
            crate::startup_guard::reject_active_tape_io_fences_on_startup(state.catalog_index())
                .map_err(refusal)?;
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
            crate::startup_guard::reject_active_tape_io_fences_on_startup(state.catalog_index())
                .map_err(refusal)?;
        }
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
            state,
            drive: Some(drive),
            path: path.into(),
            _lock: lock,
            audit_error,
            endpoint,
            unusable: false,
        })
    }
    fn flush_audit(&mut self) -> Result<(), MountError> {
        if matches!(self.endpoint, MountEndpoint::Ephemeral { .. }) {
            return Ok(());
        }
        self.state.refresh_audit_append_cursor().map_err(refusal)?;
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
    fn require_durable_mount(&self, mounted: &MountedTape) -> Result<(), MountError> {
        if self.unusable {
            return Err(refusal("invocation is unusable after uncertain I/O; recover the tape and start a new invocation"));
        }
        if !mounted.writable() {
            return Err(refusal(
                "ephemeral endpoint is read-only; a VPD serial is required",
            ));
        }
        if mounted.endpoint != self.endpoint || mounted.drive.endpoint() != &self.endpoint {
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
        mounted: &MountedTape,
        id: uuid::Uuid,
        event: AuditEvent,
    ) -> Result<(), MountError> {
        self.flush_audit()?;
        let key = mounted
            .endpoint
            .durable_key()
            .ok_or_else(|| refusal("ephemeral endpoint is read-only"))?;
        let detail = BTreeMap::from([
            ("session_kind".into(), ciborium::Value::Text("write".into())),
            (
                "tape_uuid".into(),
                ciborium::Value::Bytes(mounted.tape_uuid.to_vec()),
            ),
            ("endpoint_key".into(), ciborium::Value::Text(key.encode())),
        ]);
        self.state
            .audit()
            .append(AuditEventRecord {
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
        self.state.replay_audit_projection().map_err(refusal)?;
        Ok(())
    }
    /// Durable session and calibration fencing wrap the existing admission funnel.
    pub fn write_checkpointed(
        &mut self,
        mounted: &mut MountedTape,
        request: WriteObjectToPoolRequest,
    ) -> Result<PoolWriteResult, MountError> {
        self.require_durable_mount(mounted)?;
        self.flush_audit()?;
        crate::startup_guard::reject_active_tape_io_fences_on_startup(self.state.catalog_index())
            .map_err(refusal)?;
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
            crate::replay_committed_pool_write_from_state(self.state, &request).map_err(refusal)?
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
        .map_err(refusal)?;
        let selected = match disposition {
            crate::pool_write::PinnedWriteDisposition::Writable(selected) => selected,
            _ => return Err(refusal("tape requires terminal recovery before writing")),
        };
        let id = uuid::Uuid::new_v4();
        self.session(mounted, id, AuditEvent::SessionOpened)?;
        let before = mounted.drive.media_write_dispatch_count();
        let result = (|| {
            mounted.drive.test_unit_ready().map_err(refusal)?;
            mounted.drive.prevent_medium_removal().map_err(refusal)?;
            crate::write_to_selected_drive_checkpointed(
                self.state,
                &mut mounted.drive,
                request,
                selected,
            )
            .map_err(refusal)
        })();
        let uncertain = result.is_err() && mounted.drive.media_write_dispatch_count() != before;
        if uncertain {
            self.unusable = true;
            raise_standalone_fence(
                self.state,
                mounted.tape_uuid,
                mounted.endpoint.durable_key(),
                "standalone_write_failed",
                &result.as_ref().unwrap_err().to_string(),
            )?;
        }
        let closed = self.session(
            mounted,
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
                    _device_lock: self._lock.clone(),
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
            "media not ready: {:?}; insert the cartridge and retry",
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
        crate::startup_guard::reject_active_tape_io_fences_on_startup(self.state.catalog_index())
            .map_err(refusal)?;
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
        self.session(mounted, session_id, AuditEvent::SessionOpened)?;
        let result = (|| {
            if let Some(reply) = crate::write_owner::preflight_manual_finalize_tape(
                self.state.catalog_index(),
                crate::write_owner::ManualFinalizePreflightConfig {
                    checkpoint_journal_dir: &cfg.checkpoint_journal_dir,
                    audit_dir: &cfg.audit_dir,
                    audit_fsync: cfg.audit_fsync,
                    audit_append_lock: &cfg.audit_append_lock,
                },
                tape.voltag.as_deref(),
                &mut request,
            )? {
                return Ok(reply);
            }
            crate::write_owner::finalize_mounted_tape(
                self.state.catalog_index(),
                &cfg,
                &mut mounted.drive,
                &request,
                tape.voltag.as_deref(),
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
            mounted,
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

/// Persist uncertainty in both the index and its existing audit replay funnel.
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
    let fence = state
        .catalog_index()
        .record_tape_io_fence_at_endpoint(
            remanence_state::TapeIoFenceInput {
                tape_uuid,
                barcode,
                reason: reason.into(),
                evidence_json: Some(serde_json::json!({"error": detail}).to_string()),
            },
            endpoint,
        )
        .map_err(refusal)?;
    let cfg = crate::write_owner::TerminalWriteConfig {
        checkpoint_journal_dir: state.paths().journal_dir.join("checkpoints"),
        audit_dir: state.paths().audit_dir.clone(),
        audit_fsync: state.config().audit.fsync,
        audit_append_lock: Arc::new(Mutex::new(())),
    };
    crate::write_owner::append_tape_io_fence_evidence(
        state.catalog_index(),
        &cfg,
        &fence,
        AuditEvent::TapeIoFenceRaised,
    )
    .map_err(refusal)?;
    state.refresh_audit_append_cursor().map_err(refusal)
}

#[cfg(test)]
mod tests;
