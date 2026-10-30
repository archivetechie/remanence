//! Temporary borrowed-mount forwards; removed with the compatibility API in commit 6.
use crate::mount_support::{
    refusal, standalone_fence_policy, MountError, MountedTape, OperatorMount,
};
use crate::standalone_write::{legacy_status, supersede, MountedWriteResult};
use crate::{StandaloneFinalizeRequest, WriteObjectToPoolRequest};

impl OperatorMount<'_> {
    /// Temporary forwarding for borrowed-mount callers, removed with this module.
    /// The scope retains both original locks and always joins the same actor loop.
    fn with_actor<T>(
        &mut self,
        mounted: &mut MountedTape,
        operation: impl FnOnce(
            &mut crate::engine::drive_adapter::MountedCommands,
            &mut remanence_state::StateHandle,
        ) -> Result<T, MountError>,
    ) -> Result<T, MountError> {
        use crate::engine::drive_adapter::{mounted_config, wait_for_cleanup, MountedCommands};
        self.require_durable_mount(mounted)?;
        self.reconcile_checkpoints()?;
        standalone_fence_policy(
            self.state.catalog_index(),
            Some(mounted.tape_uuid),
            mounted.endpoint.durable_key(),
            true,
        )?;
        let cfg = mounted_config(
            self.state,
            mounted,
            remanence_library::StaticAllowlist::new(Vec::<String>::new()),
            self.context.audit_error.clone(),
        )
        .map_err(MountError::Actor)?;
        let tape_uuid = mounted.tape_uuid;
        let endpoint = cfg.mounted().expect("mounted config").endpoint.clone();
        let before = mounted.drive.media_write_dispatch_count();
        let result = std::thread::scope(|scope| {
            let (tx, rx) = tokio::sync::mpsc::channel(16);
            let actor_tx = tx.clone();
            let drive = &mut mounted.drive;
            let thread = scope.spawn(move || {
                crate::write_owner::run_borrowed_mounted_actor(drive, cfg, actor_tx, rx)
            });
            let mut client = MountedCommands::new(tx.clone(), tape_uuid, endpoint);
            client.checkpoints_reconciled = true; // The borrowed invocation reconciles once above.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                operation(&mut client, self.state)
            }));
            let (reply, rx) = tokio::sync::oneshot::channel();
            let finish = wait_for_cleanup(
                tx.send(crate::write_owner::DriveCommand::FinishMountedInvocation { reply }),
            )
            .map_err(refusal)
            .and_then(|()| {
                wait_for_cleanup(rx)
                    .map_err(refusal)?
                    .map_err(|e| MountError::Actor(e.into()))
            });
            let joined = thread.join();
            match result {
                Err(panic) => std::panic::resume_unwind(panic),
                Ok(result) => {
                    if let Err(panic) = joined {
                        std::panic::resume_unwind(panic);
                    }
                    let value = result?;
                    finish?;
                    Ok(value)
                }
            }
        });
        self.unusable |= result.is_err() && mounted.drive.media_write_dispatch_count() != before;
        self.state.refresh_audit_append_cursor().map_err(refusal)?;
        mounted.identity_generation = mounted.drive.identity_generation();
        result
    }
    pub fn write_checkpointed(
        &mut self,
        mounted: &mut MountedTape,
        request: WriteObjectToPoolRequest,
    ) -> Result<MountedWriteResult, MountError> {
        self.with_actor(mounted, |client, state| {
            crate::engine::drive_adapter::write_checkpointed(client, state, request)
        })
    }
    pub fn put_path_version(
        &mut self,
        mounted: &mut MountedTape,
        request: WriteObjectToPoolRequest,
    ) -> Result<Result<(MountedWriteResult, &'static str), remanence_state::VersionRow>, MountError>
    {
        self.put_path_version_with_policy(mounted, request, false)
    }
    pub fn put_path_version_with_policy(
        &mut self,
        mounted: &mut MountedTape,
        request: WriteObjectToPoolRequest,
        allow_plaintext_copy: bool,
    ) -> Result<Result<(MountedWriteResult, &'static str), remanence_state::VersionRow>, MountError>
    {
        self.put_path_version_with_preparation(mounted, request, allow_plaintext_copy, || {})
    }
    pub(crate) fn put_path_version_with_preparation(
        &mut self,
        mounted: &mut MountedTape,
        request: WriteObjectToPoolRequest,
        allow_plaintext_copy: bool,
        before_prepare: impl FnOnce(),
    ) -> Result<Result<(MountedWriteResult, &'static str), remanence_state::VersionRow>, MountError>
    {
        self.with_actor(mounted, |client, state| {
            crate::engine::drive_adapter::put_version(
                client,
                state,
                request,
                allow_plaintext_copy,
                before_prepare,
            )
        })
    }
    pub fn finalize(
        &mut self,
        mounted: &mut MountedTape,
        request: StandaloneFinalizeRequest,
    ) -> Result<remanence_state::TerminalFinalizationProjection, MountError> {
        self.with_actor(mounted, |client, _| {
            client.finalize(request).map_err(legacy_status)
        })
    }
    pub fn supersede_tail(
        &mut self,
        mounted: &mut MountedTape,
        acknowledge: &str,
        confirm: impl FnOnce(&str, &[String]) -> Result<(), String>,
    ) -> Result<Vec<String>, MountError> {
        self.with_actor(mounted, |client, state| {
            supersede(client, state, acknowledge, confirm)
        })
    }
}
