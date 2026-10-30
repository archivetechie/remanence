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
    path::Path,
    sync::{Arc, Mutex},
    thread::JoinHandle,
};
use tokio::sync::{mpsc, oneshot};
use tonic::Status;
use uuid::Uuid;

type Result<T> = std::result::Result<T, ActorFailure>;

/// Owns a single mounted endpoint and its state until explicit finish (or Drop).
pub struct MountedDrive {
    owner: Option<Owner>,
    tape_uuid: crate::TapeUuid,
    session: Option<(Uuid, String)>,
    receipts: Vec<pb::ObjectRecord>,
}
struct Owner {
    tx: mpsc::Sender<DriveCommand>,
    thread: JoinHandle<()>,
    // Step-1 temporary deviation (Q1): retain StateHandle's state.lock AND the
    // owned device lock in the actor's MountedTape. Device-lock-only is step 5.
    state: StateHandle,
}
impl Owner {
    fn finish(mut self) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        let sent = self
            .tx
            .blocking_send(DriveCommand::FinishMountedInvocation { reply });
        let result = if sent.is_ok() {
            receive(rx)
        } else {
            Err(stopped())
        };
        drop(self.tx);
        let joined = self
            .thread
            .join()
            .map_err(|_| ActorFailure::from(Status::internal("mounted drive actor panicked")));
        let refresh = self
            .state
            .refresh_audit_append_cursor()
            .map_err(|e| ActorFailure::from(crate::status_from_state_error(e)));
        // state.lock is released only after join, including failure and panic paths.
        result.and(joined).and(refresh)
    }
}
fn stopped() -> ActorFailure {
    Status::unavailable("mounted drive actor stopped").into()
}
fn receive<T>(rx: oneshot::Receiver<std::result::Result<T, Status>>) -> Result<T> {
    rx.blocking_recv()
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
        mut state: StateHandle,
        path: &Path,
        policy: &StaticAllowlist,
        transport: Box<dyn SgTransport>,
        lock_root: &Path,
        expected: Option<crate::TapeUuid>,
    ) -> Result<Self> {
        let mut mount =
            OperatorMount::with_transport(path, policy, transport, &mut state, lock_root)
                .map_err(mount_error)?;
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
    fn start(
        state: StateHandle,
        mounted: crate::MountedTape,
        policy: StaticAllowlist,
        audit_error: Arc<Mutex<Option<String>>>,
    ) -> Result<Self> {
        let endpoint = mounted.endpoint.durable_key().cloned().ok_or_else(|| {
            ActorFailure::from(Status::failed_precondition(
                "ephemeral endpoint is read-only",
            ))
        })?;
        let c = state.config();
        let cfg = write_owner::WriteOwnerConfig {
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
                identity_generation: mounted.identity_generation,
                actor: AuditActor::local_user(),
                audit_error,
                audit_session: Arc::new(Mutex::new(None)),
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
        };
        let tape_uuid = mounted.tape_uuid;
        let (tx, thread) = write_owner::spawn_mounted_actor(mounted, cfg)
            .map_err(|e| ActorFailure::from(Status::internal(e.to_string())))?;
        Ok(Self {
            owner: Some(Owner { tx, thread, state }),
            tape_uuid,
            session: None,
            receipts: vec![],
        })
    }
    fn send(&self, command: DriveCommand) -> Result<()> {
        self.owner
            .as_ref()
            .ok_or_else(stopped)?
            .tx
            .blocking_send(command)
            .map_err(|_| stopped())
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
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
    /// Admit the pinned cartridge; no selection, changer or load is available.
    pub fn open_session(&mut self, pool_id: &str) -> Result<pb::WriteSession> {
        self.refresh_session()?;
        if self.session.is_some() {
            return Err(Status::failed_precondition("write session already open").into());
        }
        let owner = self.owner.as_mut().ok_or_else(stopped)?;
        crate::reconcile_checkpoint_journal_projections(&mut owner.state)
            .map_err(ActorFailure::from)?;
        owner
            .state
            .refresh_audit_append_cursor()
            .map_err(|e| ActorFailure::from(crate::status_from_state_error(e)))?;
        let pool_cfg = owner
            .state
            .config()
            .tape_pools
            .iter()
            .find(|p| p.id == pool_id)
            .cloned()
            .ok_or_else(|| ActorFailure::from(Status::invalid_argument("unconfigured pool")))?;
        let journals = owner.state.paths().journal_dir.join("checkpoints");
        let (selected, host_only_recovery) =
            match crate::pool_write::admit_pinned_tape_for_write_session(
                owner.state.catalog_index(),
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
        self.session = Some((id, pool_id.into()));
        Ok(session)
    }
    /// Append a caller-owned logical file and publish only a durable receipt.
    pub fn append(
        &mut self,
        mut request: crate::WriteObjectToPoolRequest,
    ) -> Result<pb::ObjectRecord> {
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
        record.ok_or_else(|| {
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
        Ok(result.session)
    }
    /// End the session and release its checkpoint/admission leases; retain the mount.
    pub fn close_session(&mut self) -> Result<pb::WriteSession> {
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
    /// Close the epoch without unloading, cancel timers, join and release both locks.
    pub fn finish(&mut self) -> Result<()> {
        let Some(owner) = self.owner.take() else {
            return Ok(());
        };
        self.session = None;
        owner.finish()
    }
}
impl Drop for MountedDrive {
    fn drop(&mut self) {
        let Some(owner) = self.owner.take() else {
            return;
        };
        tracing::warn!("mounted owner dropped without explicit finish");
        if tokio::runtime::Handle::try_current().is_err() {
            if let Err(error) = owner.finish() {
                tracing::error!(%error, "mounted owner cleanup failed");
            }
            return;
        }
        // Never wait for drive I/O on a Tokio worker. The detached cleanup thread
        // owns both locks until finish has joined the actor. Explicit finish is
        // the caller's way to wait for completion and receive cleanup errors.
        let pending = Arc::new(Mutex::new(Some(owner)));
        let worker = pending.clone();
        let spawned = std::thread::Builder::new()
            .name("mounted-owner-cleanup".into())
            .spawn(move || {
                let owner = worker.lock().unwrap_or_else(|e| e.into_inner()).take();
                if let Some(owner) = owner {
                    if let Err(error) = owner.finish() {
                        tracing::error!(%error, "mounted owner cleanup failed");
                    }
                }
            });
        if let Err(error) = spawned {
            // Resource exhaustion cannot justify blocking this runtime worker or
            // releasing state.lock while the drive thread still owns the device.
            // Preserve ownership until process exit; report this exceptional leak.
            std::mem::forget(pending);
            tracing::error!(%error, "cannot spawn mounted cleanup; locks retained until process exit");
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
