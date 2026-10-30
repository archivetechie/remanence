//! A blocking, in-process owner for one operator-mounted drive.
//!
//! Async callers run this owner on a blocking worker. Commands and receipts use
//! the same channel and session implementation as the daemon, without gRPC.

pub use crate::write_owner::ActorFailure;
use crate::{
    pb,
    write_owner::{self, DriveCommand},
    MountBackend, MountTarget, OperatorMount,
};
use remanence_library::{SgTransport, StaticAllowlist};
use remanence_state::{AuditActor, StateHandle};
use std::{
    future::Future,
    path::Path,
    sync::{Arc, Mutex},
    thread::JoinHandle,
};
use tokio::sync::{mpsc, oneshot};
use tonic::Status;
use uuid::Uuid;

type Result<T> = std::result::Result<T, ActorFailure>;

/// Owns a single mounted endpoint and its state until explicit finish (or Drop).
/// Calls block the calling thread and must not be made from async code.
///
/// Drop joins when there is no Tokio runtime on the thread, as in the synchronous
/// CLI used by every step-1 caller. On any runtime thread, including
/// `spawn_blocking`, Drop detaches cleanup; if the cleanup thread cannot be
/// spawned, it finishes in place. Cleanup releases the drive and both locks in
/// either case. Call [`Self::finish`] when the caller must know the drive and
/// locks are released before continuing.
pub struct MountedDrive {
    owner: Option<Owner>,
    client: MountedCommands,
}

pub(crate) struct MountedCommands {
    tx: mpsc::Sender<DriveCommand>,
    tape_uuid: crate::TapeUuid,
    endpoint: remanence_library::DriveKey,
    pub(crate) checkpoints_reconciled: bool,
    last_session: Option<pb::WriteSession>,
    session: Option<(Uuid, String)>,
    receipts: Vec<pb::ObjectRecord>,
}
struct Owner {
    tx: mpsc::Sender<DriveCommand>,
    thread: JoinHandle<()>,
    // Step-1 temporary deviation (Q1): retain StateHandle's state.lock AND the
    // owned device lock in the actor's MountedTape. Device-lock-only is step 5.
    state: StateHandle,
    // Retain the caller's actual changer guard until the mounted actor has joined.
    _library_guard: Option<remanence_library::ChangerHandle>,
}
impl Owner {
    fn finish(mut self, propagate_panic: bool) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        let sent = wait_for_cleanup(
            self.tx
                .send(DriveCommand::FinishMountedInvocation { reply }),
        );
        let result = if sent.is_ok() {
            wait_for_cleanup(rx)
                .map_err(|_| stopped())
                .and_then(|result| result.map_err(Into::into))
        } else {
            Err(stopped())
        };
        drop(self.tx);
        let joined = self.thread.join();
        let refresh = self
            .state
            .refresh_audit_append_cursor()
            .map_err(|e| ActorFailure::from(crate::status_from_state_error(e)));
        // state.lock is released only after join, including failure and panic paths.
        if let Err(panic) = joined {
            if propagate_panic {
                drop(self.state);
                std::panic::resume_unwind(panic);
            }
            return Err(Status::internal("mounted drive actor panicked").into());
        }
        result.and(refresh)
    }
}
fn stopped() -> ActorFailure {
    Status::unavailable("mounted drive actor stopped").into()
}
fn receive<T>(rx: oneshot::Receiver<std::result::Result<T, Status>>) -> Result<T> {
    wait_for_cleanup(rx)
        .map_err(|_| stopped())?
        .map_err(Into::into)
}
impl MountedDrive {
    /// Open and prove the actual cartridge while holding both invocation locks.
    pub fn open(
        mut state: StateHandle,
        path: &Path,
        policy: &StaticAllowlist,
        expected: Option<crate::TapeUuid>,
    ) -> Result<Self> {
        let mut mount = OperatorMount::open(path, policy, &mut state).map_err(mount_error)?;
        let mounted = mount
            .mount(MountTarget::Device {
                path: path.into(),
                expected_uuid: expected,
            })
            .map_err(mount_error)?;
        let audit = mount.context.audit_error.clone();
        mount.finish().map_err(mount_error)?;
        Self::start(state, mounted, policy.clone(), audit)
    }
    /// Inject the transport and lock namespace; identical ownership and admission.
    pub fn with_transport(
        state: StateHandle,
        path: &Path,
        policy: &StaticAllowlist,
        transport: Box<dyn SgTransport>,
        lock_root: &Path,
        expected: Option<crate::TapeUuid>,
    ) -> Result<Self> {
        Self::with_transport_and_reservation(
            state, path, policy, transport, lock_root, expected, false,
        )
    }
    /// Open the mounted endpoint and optionally reserve it before transferring
    /// the mount and the same owned StateHandle to the actor.
    pub fn with_transport_and_reservation(
        mut state: StateHandle,
        path: &Path,
        policy: &StaticAllowlist,
        transport: Box<dyn SgTransport>,
        lock_root: &Path,
        expected: Option<crate::TapeUuid>,
        reserve: bool,
    ) -> Result<Self> {
        let mut mount =
            OperatorMount::with_transport(path, policy, transport, &mut state, lock_root)
                .map_err(mount_error)?;
        let mut mounted = mount
            .mount(MountTarget::Device {
                path: path.into(),
                expected_uuid: expected,
            })
            .map_err(mount_error)?;
        if reserve {
            mount.reserve(&mut mounted).map_err(mount_error)?;
        }
        let audit = mount.context.audit_error.clone();
        mount.finish().map_err(mount_error)?;
        Self::start(state, mounted, policy.clone(), audit)
    }
    /// Own an already placed library drive, its real bay key, state and library guard.
    /// The serial device lock coordinates with standalone use of the same drive;
    /// bay sessions and calibration remain the daemon's responsibility.
    /// No load or changer command is issued here. The optional WOR plan is bound
    /// to this one caller before any actor write; daemon construction never reads it.
    pub fn from_placed_library(
        mut state: StateHandle,
        library: remanence_library::LibraryHandle,
        drive: remanence_library::DriveHandle,
        policy: StaticAllowlist,
        tape_uuid: crate::TapeUuid,
        caller_object_id: &str,
        lock_root: &Path,
    ) -> Result<Self> {
        let key = drive.endpoint().durable_key().ok_or_else(|| {
            ActorFailure::from(Status::failed_precondition(
                "library drive requires a durable bay",
            ))
        })?;
        let remanence_library::DriveKey::Bay {
            library_serial,
            bay,
        } = key
        else {
            return Err(
                Status::failed_precondition("placed library drive requires a library bay").into(),
            );
        };
        if library_serial != &library.library().serial {
            return Err(Status::failed_precondition(
                "drive does not belong to the retained library",
            )
            .into());
        }
        let serial = library
            .library()
            .drive_bays
            .iter()
            .find(|b| b.element_address == *bay)
            .and_then(|b| b.installed.as_ref())
            .map(|d| d.serial.clone())
            .ok_or_else(|| {
                ActorFailure::from(Status::failed_precondition(
                    "placed bay has no installed drive",
                ))
            })?;
        let fault = write_owner::direct_replay_fault::DirectReplayFaultPlan::from_env_for_object(
            tape_uuid,
            caller_object_id,
        )
        .map_err(|e| {
            let error = crate::PoolWriteError::InvalidInput(e);
            ActorFailure::from(ActorFailure::attach(
                Status::invalid_argument(error.to_string()),
                error,
            ))
        })?;
        let path = Path::new("library drive");
        let mut mount =
            OperatorMount::from_placed_library(path, drive, &mut state, &serial, lock_root)
                .map_err(mount_error)?;
        let mounted = mount
            .mount(MountTarget::Device {
                path: path.into(),
                expected_uuid: Some(tape_uuid),
            })
            .map_err(mount_error)?;
        let audit = mount.context.audit_error.clone();
        mount.finish().map_err(mount_error)?;
        let mut cfg = mounted_config(&state, &mounted, policy, audit)?;
        if let write_owner::ActorPlacement::Mounted(context) = &mut cfg.placement {
            context.direct_replay_fault = fault;
        }
        Self::start_configured(state, mounted, cfg, Some(library.into_changer()))
    }
    /// Write one object through the actor without standalone version policy.
    pub fn write_checkpointed(
        &mut self,
        request: crate::WriteObjectToPoolRequest,
    ) -> std::result::Result<crate::MountedWriteResult, crate::MountError> {
        let owner = self
            .owner
            .as_mut()
            .ok_or_else(|| crate::mount_support::refusal("mounted drive finished"))?;
        self.client
            .reconcile_checkpoints(&mut owner.state)
            .map_err(crate::MountError::Actor)?;
        write_checkpointed(&mut self.client, &mut owner.state, request)
    }
    pub(crate) fn start(
        state: StateHandle,
        mounted: crate::MountedTape,
        policy: StaticAllowlist,
        audit_error: Arc<Mutex<Option<String>>>,
    ) -> Result<Self> {
        let cfg = mounted_config(&state, &mounted, policy, audit_error)?;
        Self::start_configured(state, mounted, cfg, None)
    }
    fn start_configured(
        state: StateHandle,
        mounted: crate::MountedTape,
        cfg: write_owner::WriteOwnerConfig,
        library_guard: Option<remanence_library::ChangerHandle>,
    ) -> Result<Self> {
        let tape_uuid = mounted.tape_uuid;
        let endpoint = cfg.mounted().expect("mounted config").endpoint.clone();
        let (tx, thread) = write_owner::spawn_mounted_actor(mounted, cfg)
            .map_err(|e| ActorFailure::from(Status::internal(e.to_string())))?;
        Ok(Self {
            owner: Some(Owner {
                tx: tx.clone(),
                thread,
                state,
                _library_guard: library_guard,
            }),
            client: MountedCommands::new(tx, tape_uuid, endpoint),
        })
    }
    pub fn open_session(&mut self, pool: &str) -> Result<pb::WriteSession> {
        let owner = self.owner.as_mut().ok_or_else(stopped)?;
        self.client.open_session(&mut owner.state, pool)
    }
    pub fn append(&mut self, request: crate::WriteObjectToPoolRequest) -> Result<pb::ObjectRecord> {
        self.client
            .append_outcome(request)
            .map(|(record, _)| record)
    }
    pub fn checkpoint(&mut self) -> Result<pb::WriteSession> {
        self.client.checkpoint()
    }
    pub fn close_session(&mut self) -> Result<pb::WriteSession> {
        self.client.close_session()
    }
    pub fn abort_session(&mut self, reason: Option<String>) -> Result<pb::WriteSession> {
        self.client.abort_session(reason)
    }
    pub fn finalize(
        &self,
        request: crate::StandaloneFinalizeRequest,
    ) -> Result<remanence_state::TerminalFinalizationProjection> {
        self.client.finalize(request)
    }
    pub fn supersede_tail(
        &self,
        request: pb::SupersedeTailRequest,
    ) -> Result<pb::SupersedeTailResponse> {
        self.client.supersede_tail(request)
    }
    pub fn receipts(&self) -> &[pb::ObjectRecord] {
        self.client.receipts()
    }
    pub fn tape_uuid(&self) -> crate::TapeUuid {
        self.client.tape_uuid
    }
    pub fn supersede_tail_confirmed(
        &mut self,
        acknowledge: &str,
        confirm: impl FnOnce(&str, &[String]) -> std::result::Result<(), String>,
    ) -> std::result::Result<Vec<String>, crate::MountError> {
        self.standalone_write_admission()?;
        crate::standalone_write::supersede(
            &self.client,
            &mut self
                .owner
                .as_mut()
                .ok_or_else(|| crate::mount_support::refusal("mounted drive finished"))?
                .state,
            acknowledge,
            confirm,
        )
    }
    pub fn finalize_standalone(
        &mut self,
        request: crate::StandaloneFinalizeRequest,
    ) -> std::result::Result<remanence_state::TerminalFinalizationProjection, crate::MountError>
    {
        self.standalone_write_admission()?;
        self.finalize(request)
            .map_err(crate::standalone_write::legacy_status)
    }

    /// Preserve standalone quarantine guidance before dispatching a terminal command.
    fn standalone_write_admission(&mut self) -> std::result::Result<(), crate::MountError> {
        let owner = self
            .owner
            .as_mut()
            .ok_or_else(|| crate::mount_support::refusal("mounted drive finished"))?;
        crate::mount_support::standalone_fence_policy(
            owner.state.catalog_index(),
            Some(self.client.tape_uuid),
            Some(&self.client.endpoint),
            true,
        )
        .map(|_| ())
    }

    pub fn catalog_index(&mut self) -> &remanence_state::CatalogIndex {
        self.owner
            .as_mut()
            .expect("unfinished mounted owner")
            .state
            .catalog_index()
    }
    pub fn reconcile_checkpoints(&mut self) -> Result<()> {
        self.client
            .reconcile_checkpoints(&mut self.owner.as_mut().ok_or_else(stopped)?.state)
    }
    /// Run frontend version policy before opening a session; a skip does no write work.
    pub fn put_path_version_with_policy(
        &mut self,
        request: crate::WriteObjectToPoolRequest,
        allow_plaintext_copy: bool,
    ) -> std::result::Result<
        std::result::Result<(crate::MountedWriteResult, &'static str), remanence_state::VersionRow>,
        crate::MountError,
    > {
        let owner = self
            .owner
            .as_mut()
            .ok_or_else(|| crate::mount_support::refusal("mounted drive finished"))?;
        put_version(
            &mut self.client,
            &mut owner.state,
            request,
            allow_plaintext_copy,
            || {},
        )
    }
    /// Close the epoch without unloading, cancel timers, join and release both locks.
    pub fn finish(&mut self) -> Result<()> {
        let Some(owner) = self.owner.take() else {
            return Ok(());
        };
        self.client.session = None;
        owner.finish(true)
    }
}
/// Joins cleanup when there is no Tokio runtime on the thread, as in the
/// synchronous CLI used by every step-1 caller. On any runtime thread, including
/// `spawn_blocking`, detaches cleanup; if the cleanup thread cannot be spawned,
/// finishes in place. Cleanup releases the drive and both locks in either case.
/// Call [`MountedDrive::finish`] when the caller must know the drive and locks
/// are released before continuing.
impl Drop for MountedDrive {
    fn drop(&mut self) {
        let Some(owner) = self.owner.take() else {
            return;
        };
        tracing::warn!("mounted owner dropped without explicit finish");
        if tokio::runtime::Handle::try_current().is_err() {
            if let Err(error) = owner.finish(false) {
                tracing::error!(%error, "mounted owner cleanup failed");
            }
            return;
        }
        // Never wait for drive I/O on a Tokio worker. The detached cleanup thread
        // owns both locks until finish has joined the actor. Explicit finish is
        // the caller's way to wait for completion and receive cleanup errors.
        defer_cleanup(owner, |cleanup| {
            std::thread::Builder::new()
                .name("mounted-owner-cleanup".into())
                .spawn(cleanup)
        });
    }
}

fn defer_cleanup(
    owner: Owner,
    spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<JoinHandle<()>>,
) {
    let pending = Arc::new(Mutex::new(Some(owner)));
    let worker = pending.clone();
    let spawned = spawn(Box::new(move || {
        let owner = worker.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(owner) = owner {
            if let Err(error) = owner.finish(false) {
                tracing::error!(%error, "mounted owner cleanup failed");
            }
        }
    }));
    if let Err(error) = spawned {
        tracing::error!(%error, "cannot spawn mounted cleanup; finishing in place");
        if let Some(owner) = pending.lock().unwrap_or_else(|e| e.into_inner()).take() {
            if let Err(error) = owner.finish(false) {
                tracing::error!(%error, "mounted owner cleanup failed");
            }
        }
    }
}

fn mount_error(error: crate::MountError) -> ActorFailure {
    match error {
        crate::MountError::PoolWrite(error) => {
            ActorFailure::attach(Status::failed_precondition(error.to_string()), *error).into()
        }
        other => Status::failed_precondition(other.to_string()).into(),
    }
}

#[cfg(test)]
mod tests;

fn pinned_error(error: crate::pool_write::PinnedTapeError) -> ActorFailure {
    let capacity = match &error {
        crate::pool_write::PinnedTapeError::NotWritable {
            reason:
                crate::pool_write::WritabilityError::InsufficientCapacity {
                    object_size,
                    raw_capacity,
                    used,
                },
            ..
        } => Some(crate::PoolWriteError::SelectedTapeInsufficientCapacity {
            object_size: *object_size,
            raw_capacity: *raw_capacity,
            used: *used,
        }),
        _ => None,
    };
    let status = write_owner::status_from_pinned_tape_error(error);
    match capacity {
        Some(cause) => ActorFailure::attach(status, cause).into(),
        None => status.into(),
    }
}

impl MountedCommands {
    fn reconcile_checkpoints(&mut self, state: &mut StateHandle) -> Result<()> {
        if !self.checkpoints_reconciled {
            crate::reconcile_checkpoint_journal_projections(state).map_err(ActorFailure::from)?;
            self.checkpoints_reconciled = true;
        }
        Ok(())
    }

    pub(crate) fn tape_uuid(&self) -> crate::TapeUuid {
        self.tape_uuid
    }
    pub(crate) fn new(
        tx: mpsc::Sender<DriveCommand>,
        tape_uuid: crate::TapeUuid,
        endpoint: remanence_library::DriveKey,
    ) -> Self {
        Self {
            tx,
            tape_uuid,
            endpoint,
            session: None,
            checkpoints_reconciled: false,
            last_session: None,
            receipts: vec![],
        }
    }
    pub(crate) fn send(&self, command: DriveCommand) -> Result<()> {
        wait_for_cleanup(self.tx.send(command)).map_err(|_| stopped())
    }
    /// The actor can close an idle session independently. Query its ordered
    /// mailbox before using our cached ID; already claimed receipts stay owned.
    fn refresh_session(&mut self) -> Result<()> {
        let Some((session_id, _)) = self.session.as_ref() else {
            return Ok(());
        };
        let (reply, rx) = oneshot::channel();
        self.send(DriveCommand::Get {
            session_id: *session_id,
            reply,
        })?;
        match receive(rx) {
            Ok(_) => Ok(()),
            Err(error) if error.status.code() == tonic::Code::NotFound => {
                self.session = None;
                if let Some(last) = self.last_session.as_mut() {
                    last.state = pb::write_session::State::WriteSessionStateClosed as i32;
                }
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
    /// Admit the pinned cartridge; no selection, changer or load is available.
    pub fn open_session(
        &mut self,
        state: &mut StateHandle,
        pool_id: &str,
    ) -> Result<pb::WriteSession> {
        self.refresh_session()?;
        if self.session.is_some() {
            return Err(Status::failed_precondition("write session already open").into());
        }
        self.reconcile_checkpoints(state)?;
        state
            .refresh_audit_append_cursor()
            .map_err(|e| ActorFailure::from(crate::status_from_state_error(e)))?;
        let pool_cfg = state
            .config()
            .tape_pools
            .iter()
            .find(|p| p.id == pool_id)
            .cloned()
            .ok_or_else(|| ActorFailure::from(Status::invalid_argument("unconfigured pool")))?;
        let journals = state.paths().journal_dir.join("checkpoints");
        let (selected, host_only_recovery) =
            match crate::pool_write::admit_pinned_tape_for_write_session(
                state.catalog_index(),
                self.tape_uuid,
                pool_id,
                &pool_cfg,
                &journals,
            )
            .map_err(pinned_error)?
            {
                crate::pool_write::PinnedWriteDisposition::Writable(selected) => (selected, false),
                crate::pool_write::PinnedWriteDisposition::HostOnlyTerminalRecovery(selected) => {
                    (selected, true)
                }
            };
        let (reply, rx) = oneshot::channel();
        self.send(DriveCommand::OpenMountedWrite {
            host_only_recovery,
            pool_cfg,
            selected,
            reply,
        })?;
        let session = receive(rx)?;
        let id = Uuid::from_slice(&session.session_id)
            .map_err(|e| ActorFailure::from(Status::internal(e.to_string())))?;
        self.last_session = Some(session.clone());
        self.session = Some((id, pool_id.into()));
        Ok(session)
    }
    /// Append a caller-owned logical file and publish only a durable receipt.
    pub fn append_outcome(
        &mut self,
        mut request: crate::WriteObjectToPoolRequest,
    ) -> Result<(pb::ObjectRecord, bool)> {
        self.refresh_session()?;
        let (session_id, pool) = self
            .session
            .as_ref()
            .ok_or_else(|| ActorFailure::from(Status::failed_precondition("no write session")))?;
        if pool != &request.pool_id {
            return Err(Status::failed_precondition("request pool differs from session").into());
        }
        request.source = match request.source {
            crate::WriteObjectSource::Path(path) => {
                crate::WriteObjectSource::PreparedPath(crate::pool_write::PreparedPathSource(
                    remanence_stream::prepare_regular_file(
                        &path,
                        &request.archive_path,
                        Uuid::new_v4().to_string(),
                    )
                    .map_err(|e| ActorFailure::from(Status::invalid_argument(e.to_string())))?,
                ))
            }
            source @ crate::WriteObjectSource::PreparedPath(_) => source,
            _ => {
                return Err(
                    Status::invalid_argument("mounted append requires a caller-owned file").into(),
                )
            }
        };
        let (reply, rx) = oneshot::channel();
        self.send(DriveCommand::AppendFinish {
            session_id: *session_id,
            source: request.source,
            archive_path: request.archive_path,
            caller_object_id: request.caller_object_id,
            expected_content_sha256: request.expected_content_sha256,
            expected_object_id: request.expected_object_id,
            input_kind: request.input_kind,
            representation: request.representation,
            live_write_counter: None,
            reply,
        })?;
        let appended = match receive(rx) {
            Ok(appended) => appended,
            Err(error) => {
                // Automatic terminal work may fail after the object's checkpoint.
                // Claim any durable receipt before returning that later failure.
                let _ = self.checkpoint();
                return Err(error);
            }
        };
        // Even an automatic barrier retains its receipts until this checkpoint.
        self.checkpoint()?;
        let record = self
            .receipts
            .iter()
            .find(|r| r.object_id == appended.record.object_id)
            .cloned();
        record
            .map(|record| (record, appended.replay))
            .ok_or_else(|| {
                ActorFailure::from(Status::internal("append produced no durable receipt"))
            })
    }
    /// Claim all committed receipts, including earlier automatic barriers.
    pub fn checkpoint(&mut self) -> Result<pb::WriteSession> {
        self.refresh_session()?;
        let session_id = self
            .session
            .as_ref()
            .ok_or_else(|| ActorFailure::from(Status::failed_precondition("no write session")))?
            .0;
        let (reply, rx) = oneshot::channel();
        self.send(DriveCommand::Checkpoint {
            session_id,
            trigger: write_owner::CheckpointTrigger::Explicit,
            expected_batch_id: None,
            reply: Some(reply),
        })?;
        let result = receive(rx)?;
        for record in result.committed_objects {
            if !self
                .receipts
                .iter()
                .any(|r| r.object_id == record.object_id)
            {
                self.receipts.push(record);
            }
        }
        self.last_session = Some(result.session.clone());
        Ok(result.session)
    }
    /// End the session and release its checkpoint/admission leases; retain the mount.
    pub fn close_session(&mut self) -> Result<pb::WriteSession> {
        self.refresh_session()?;
        if self.session.is_none() {
            return self.last_session.clone().ok_or_else(|| {
                ActorFailure::from(Status::failed_precondition("no write session"))
            });
        }
        self.checkpoint()?;
        let session_id = self
            .session
            .as_ref()
            .expect("checkpoint requires session")
            .0;
        let (reply, rx) = oneshot::channel();
        self.send(DriveCommand::Close { session_id, reply })?;
        let result = receive(rx)?;
        self.session = None;
        self.last_session = Some(result.session.clone());
        Ok(result.session)
    }
    /// Abort the active session using the actor's existing recovery disposition.
    pub fn abort_session(&mut self, reason: Option<String>) -> Result<pb::WriteSession> {
        self.refresh_session()?;
        let session_id = self
            .session
            .as_ref()
            .ok_or_else(|| ActorFailure::from(Status::failed_precondition("no write session")))?
            .0;
        let (reply, rx) = oneshot::channel();
        self.send(DriveCommand::Abort {
            session_id,
            reason,
            reply,
        })?;
        let result = receive(rx)?;
        self.session = None;
        self.last_session = Some(result.session.clone());
        Ok(result.session)
    }
    /// Explicit terminal recovery/finalization on the mounted cartridge.
    pub fn finalize(
        &self,
        request: crate::StandaloneFinalizeRequest,
    ) -> Result<remanence_state::TerminalFinalizationProjection> {
        let (reply, rx) = oneshot::channel();
        self.send(DriveCommand::FinalizeMounted { request, reply })?;
        receive(rx)
    }
    /// Preview or explicitly confirm recovery of a complete uncommitted tail.
    pub fn supersede_tail(
        &self,
        request: pb::SupersedeTailRequest,
    ) -> Result<pb::SupersedeTailResponse> {
        let (reply, rx) = oneshot::channel();
        self.send(DriveCommand::SupersedeMounted { request, reply })?;
        receive(rx)
    }
    /// Durable successes survive subsequent failures and remain available to render.
    pub fn receipts(&self) -> &[pb::ObjectRecord] {
        &self.receipts
    }
}

pub(crate) fn mounted_config(
    state: &StateHandle,
    mounted: &crate::MountedTape,
    policy: StaticAllowlist,
    audit_error: Arc<Mutex<Option<String>>>,
) -> Result<write_owner::WriteOwnerConfig> {
    let endpoint = mounted.endpoint.durable_key().cloned().ok_or_else(|| {
        ActorFailure::from(Status::failed_precondition(
            "ephemeral endpoint is read-only",
        ))
    })?;
    let c = state.config();
    Ok(write_owner::WriteOwnerConfig {
        index_path: state.paths().sqlite_path.clone(),
        report: remanence_library::DiscoveryReport {
            unattached_drives: vec![],
            complete: true,
            incomplete_evidence: vec![],
            libraries: vec![],
            warnings: vec![],
        },
        policy,
        audit_dir: state.paths().audit_dir.clone(),
        audit_fsync: c.audit.fsync,
        audit_append_lock: Arc::new(Mutex::new(())),
        reservations: Arc::new(Default::default()),
        placement: write_owner::ActorPlacement::Mounted(write_owner::MountedDriveContext {
            endpoint,
            tape_uuid: mounted.tape_uuid,
            barcode: mounted.label.as_ref().map(|(label, _)| label.clone()),
            cartridge_generation: mounted.generation.map(|(generation, _)| generation),
            identity_generation: mounted.identity_generation,
            actor: AuditActor::local_user(),
            audit_error,
            audit_session: Arc::new(Mutex::new(None)),
            direct_replay_fault: None,
        }),
        snapshot_miss_alarm: c.drives.snapshot_miss_alarm,
        managed_library_serials: Arc::new(Default::default()),
        cleaning: c.cleaning.clone(),
        tape_io: c.tape_io.clone(),
        tape_pools: c.tape_pools.clone(),
        verification_route: c.verification.route,
        io_memory: crate::io_memory::IoMemoryReservation::new(c.daemon.io_memory_ceiling)
            .map_err(|e| ActorFailure::from(Status::invalid_argument(e)))?,
        write_admissions: Default::default(),
        checkpoint_journal_dir: state.paths().journal_dir.join("checkpoints"),
        checkpoint_max_bytes: c.daemon.checkpoint_max_bytes,
        checkpoint_max_objects: c.daemon.checkpoint_max_objects,
        checkpoint_max_age_seconds: c.daemon.checkpoint_max_age_seconds,
        session_idle_seconds: c.daemon.default_idle_timeout_seconds,
        lifecycle: None,
        calibration_store: state.calibration_control().clone(),
    })
}

pub(crate) fn put_version(
    client: &mut MountedCommands,
    state: &mut StateHandle,
    request: crate::WriteObjectToPoolRequest,
    allow_plaintext_copy: bool,
    before_prepare: impl FnOnce(),
) -> std::result::Result<
    std::result::Result<(crate::MountedWriteResult, &'static str), remanence_state::VersionRow>,
    crate::MountError,
> {
    client
        .reconcile_checkpoints(state)
        .map_err(crate::MountError::Actor)?;
    crate::mount_support::standalone_fence_policy(
        state.catalog_index(),
        Some(client.tape_uuid),
        Some(&client.endpoint),
        true,
    )?;
    match crate::standalone_write::prepare_version(
        state.catalog_index(),
        request,
        allow_plaintext_copy,
        before_prepare,
    )? {
        Err(version) => Ok(Err(version)),
        Ok((request, reason)) => {
            let pool = request.pool_id.clone();
            let caller = request.caller_object_id.clone();
            let previously_claimed = client.receipts.len();
            match write_checkpointed(client, state, request) {
                Ok(receipt) => Ok(Ok((receipt, reason))),
                Err(crate::MountError::Committed {
                    receipt, failure, ..
                }) => Err(crate::MountError::Committed {
                    receipt,
                    reason,
                    failure,
                }),
                Err(failure) => {
                    if let Some(record) = client.receipts[previously_claimed..]
                        .iter()
                        .find(|r| r.caller_object_id.as_deref() == Some(caller.as_str()))
                    {
                        let receipt = crate::standalone_write::receipt(
                            state.catalog_index(),
                            record.clone(),
                            &pool,
                            false,
                        )?;
                        Err(crate::MountError::Committed {
                            receipt: Box::new(receipt),
                            reason,
                            failure: Box::new(failure),
                        })
                    } else {
                        Err(failure)
                    }
                }
            }
        }
    }
}

pub(crate) fn write_checkpointed(
    client: &mut MountedCommands,
    state: &mut StateHandle,
    request: crate::WriteObjectToPoolRequest,
) -> std::result::Result<crate::MountedWriteResult, crate::MountError> {
    use crate::mount_support::{refusal, MountError};
    client.refresh_session().map_err(MountError::Actor)?;
    let loaded = state
        .catalog_index()
        .get_tape(&client.tape_uuid)
        .map_err(refusal)?
        .ok_or_else(|| refusal("mounted tape is not catalogued"))?;
    if loaded.pool_id.as_deref() != Some(request.pool_id.trim()) {
        return Err(refusal("requested pool must equal the loaded tape's pool"));
    }
    if let Some(replay) = crate::pool_write::replay_committed_pool_write_reconciled(state, &request)
        .map_err(|e| MountError::PoolWrite(Box::new(e)))?
    {
        return Ok(crate::MountedWriteResult::replayed(replay.object));
    }
    let pool = request.pool_id.clone();
    if client.session.is_none() {
        client.open_session(state, &pool).map_err(|error| {
            if loaded
                .terminal_finalization
                .as_ref()
                .is_some_and(|terminal| {
                    terminal.trigger
                        == remanence_state::TerminalFinalizationTrigger::HardwareEarlyWarning
                })
            {
                MountError::EarlyWarning
            } else {
                MountError::Actor(error)
            }
        })?;
    }
    let caller = request.caller_object_id.clone();
    let (record, replay) = match client.append_outcome(request) {
        Ok(outcome) => outcome,
        Err(error) => {
            // A terminal suffix or context renewal may fail after CP publication.
            // The pre-append replay guard proved this caller had no prior object;
            // the invocation's state lock excludes another catalog writer.
            if let Ok(Some(native)) = state
                .catalog_index()
                .get_native_object_by_pool_and_caller_object_id(&pool, &caller)
            {
                if native.copies.iter().any(|copy| {
                    copy.status == "committed"
                        && copy.pool_id.as_deref() == Some(pool.as_str())
                        && copy.tape_uuid == client.tape_uuid
                }) {
                    if let Ok(mut record) =
                        crate::catalog_conversion::object_record_to_proto(native)
                    {
                        if let Some(info) = record.append_commit_info.as_mut() {
                            info.durability = pb::AppendDurability::Checkpointed as i32;
                        }
                        if !client
                            .receipts
                            .iter()
                            .any(|old| old.object_id == record.object_id)
                        {
                            client.receipts.push(record.clone());
                        }
                        let receipt = crate::standalone_write::receipt(
                            state.catalog_index(),
                            record,
                            &pool,
                            false,
                        )?;
                        return Err(MountError::Committed {
                            receipt: Box::new(receipt),
                            reason: "new",
                            failure: Box::new(MountError::Actor(error)),
                        });
                    }
                }
            }
            return Err(MountError::Actor(error));
        }
    };
    crate::standalone_write::receipt(state.catalog_index(), record, &pool, replay)
}

/// Poll only the actor's channel completion, including when thread creation fails
/// on an async worker. This exceptional fallback must not panic at Tokio's
/// blocking_recv guard or abandon the actor, device lock, ALLOW or RELEASE.
pub(crate) fn wait_for_cleanup<F: std::future::Future>(future: F) -> F::Output {
    struct WakeThread(std::thread::Thread);
    impl std::task::Wake for WakeThread {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = std::task::Waker::from(Arc::new(WakeThread(std::thread::current())));
    let mut context = std::task::Context::from_waker(&waker);
    let mut future = std::pin::pin!(tokio::task::unconstrained(future));
    loop {
        match future.as_mut().poll(&mut context) {
            std::task::Poll::Ready(value) => return value,
            std::task::Poll::Pending => std::thread::park(),
        }
    }
}
