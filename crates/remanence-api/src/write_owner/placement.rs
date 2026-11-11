//! Physical placement capabilities: a managed library or one operator-mounted cartridge.

use remanence_library::{DriveHandle, DriveKey, MediaFamily};
use remanence_state::{AuditActor, CatalogIndex};
use std::sync::{Arc, Mutex, RwLock};
use tonic::Status;
use uuid::Uuid;

#[derive(Clone)]
pub(crate) struct LibraryActorContext {
    pub serial: String,
    pub snapshot: Arc<RwLock<Arc<crate::LibrarySnapshot>>>,
}

#[derive(Clone)]
pub(crate) struct MountedDriveContext {
    pub endpoint: DriveKey,
    pub tape_uuid: crate::TapeUuid,
    pub barcode: Option<String>,
    /// Cartridge generation established by the mount, independently of catalog admission.
    pub cartridge_generation: Option<crate::LtoGen>,
    pub identity_generation: u64,
    pub actor: AuditActor,
    pub audit_error: Arc<Mutex<Option<String>>>,
    pub audit_session: Arc<Mutex<Option<Uuid>>>,
    pub direct_replay_fault: Option<super::direct_replay_fault::DirectReplayFaultPlan>,
}

#[derive(Clone)]
pub(crate) enum ActorPlacement {
    Library(LibraryActorContext),
    Mounted(MountedDriveContext),
}

impl super::WriteOwnerConfig {
    /// Library-only operations must never be dispatched by a mounted owner.
    pub(crate) fn library(&self) -> &LibraryActorContext {
        match &self.placement {
            ActorPlacement::Library(library) => library,
            ActorPlacement::Mounted(_) => panic!("library operation on mounted actor"),
        }
    }
    pub(crate) fn mounted(&self) -> Option<&MountedDriveContext> {
        match &self.placement {
            ActorPlacement::Mounted(mounted) => Some(mounted),
            ActorPlacement::Library(_) => None,
        }
    }
    pub(crate) fn endpoint(&self, bay: u16) -> DriveKey {
        match &self.placement {
            ActorPlacement::Library(library) => DriveKey::new(&library.serial, bay),
            ActorPlacement::Mounted(mounted) => mounted.endpoint.clone(),
        }
    }
    pub(crate) fn wire_bay(&self, bay: u16) -> Option<u16> {
        match self.endpoint(bay) {
            DriveKey::Bay { bay, .. } => Some(bay),
            DriveKey::Standalone { .. } => None,
        }
    }
    pub(super) fn mounted_audit_ready(&self) -> Result<(), Status> {
        if let Some(library) = self.authority.runtime() {
            library
                .ensure_motion_allowed()
                .map_err(|e| Status::internal(e.to_string()))?;
        }
        if let Some(mounted) = self.mounted() {
            if let Some(error) = mounted
                .audit_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
            {
                return Err(Status::internal(format!("drive audit failed: {error}")));
            }
        }
        Ok(())
    }
    pub(super) fn mounted_admission(
        &self,
        index: &CatalogIndex,
        drive: &DriveHandle,
    ) -> Result<(), Status> {
        self.mounted_admission_inner(index, drive, false)
    }
    fn mounted_admission_inner(
        &self,
        index: &CatalogIndex,
        drive: &DriveHandle,
        recovering_tail: bool,
    ) -> Result<(), Status> {
        if let Some(library) = self.authority.runtime() {
            library
                .ensure_motion_allowed()
                .map_err(|e| Status::failed_precondition(e.to_string()))?;
        }
        self.mounted_audit_ready()?;
        let mounted = self.mounted().expect("mounted readiness capability");
        if drive.endpoint().durable_key() != Some(&mounted.endpoint)
            || drive.identity_generation() != mounted.identity_generation
        {
            return Err(Status::failed_precondition(
                "mounted media identity invalidated",
            ));
        }
        if index
            .tape_io_is_fenced(
                Some(&mounted.tape_uuid),
                mounted.barcode.as_deref(),
                Some(&mounted.endpoint),
            )
            .map_err(crate::status_from_state_error)?
        {
            use remanence_state::library_log as lib;
            let allowed = if recovering_tail {
                if let Some(library) = self.authority.runtime() {
                    let mut fences = index
                        .tape_io_admission_conflicts(&mounted.tape_uuid, mounted.barcode.as_deref())
                        .map_err(crate::status_from_state_error)?;
                    fences.extend(
                        index
                            .list_active_tape_io_fences()
                            .map_err(crate::status_from_state_error)?
                            .into_iter()
                            .filter(|f| f.endpoint.as_ref() == Some(&mounted.endpoint)),
                    );
                    !fences.is_empty()
                        && fences.into_iter().all(|fence| {
                            fence
                                .quarantine_id
                                .strip_prefix("library:")
                                .and_then(|n| n.parse::<u64>().ok())
                                .and_then(|n| library.active_fence(n))
                                .is_some_and(|raise| {
                                    raise.scope
                                        == lib::FenceScope::Tape(lib::TapeRef {
                                            uuid: mounted.tape_uuid,
                                            incarnation: 1,
                                        })
                                        && raise.kind == lib::FenceKind::Write
                                        && raise.cause
                                            == lib::FenceCause::SessionSuffixContradiction
                                })
                        })
                } else {
                    false
                }
            } else {
                false
            };
            if !allowed {
                let mut conflicts = index
                    .tape_io_admission_conflicts(&mounted.tape_uuid, mounted.barcode.as_deref())
                    .map_err(crate::status_from_state_error)?;
                crate::tape_admission::order_conflicts(index, &mut conflicts);
                let message = "session blocked by active tape-I/O fence".to_owned();
                let message = if let Some(first) = conflicts.first() {
                    crate::tape_admission::fence_payload(
                        index,
                        mounted.tape_uuid,
                        first,
                        message,
                        true,
                        lib::AuditActor::System,
                    )
                } else {
                    message
                };
                return Err(Status::failed_precondition(message));
            }
        }
        if !recovering_tail {
            if let Some(message) = crate::tape_admission::contradicted_payload(
                index,
                mounted.tape_uuid,
                "mounted write",
                remanence_state::library_log::AuditActor::System,
            ) {
                return Err(Status::failed_precondition(message));
            }
        }
        if !index
            .media_readiness_admission_conflicts(
                &mounted.endpoint,
                None,
                mounted.barcode.as_deref(),
                false,
            )
            .map_err(crate::status_from_state_error)?
            .is_empty()
        {
            return Err(Status::failed_precondition(
                "mounted endpoint blocked by active media-readiness operation",
            ));
        }
        // The endpoint's own open or Unknown LOAD/UNLOAD refuses a write on it. The production
        // authority is always typed; only the unit-test fixture has none.
        crate::startup_media_readiness::ensure_endpoint_motion_resolved(
            self.authority.runtime(),
            "mounted write",
            &mounted.endpoint,
        )
    }
    pub(super) fn mounted_ready(
        &self,
        index: &CatalogIndex,
        drive: &mut DriveHandle,
    ) -> Result<(), Status> {
        self.mounted_ready_inner(index, drive, false)
    }
    /// Recovery preserves every admission gate except its exact suffix raises.
    pub(super) fn mounted_recovery_ready(
        &self,
        index: &CatalogIndex,
        drive: &mut DriveHandle,
    ) -> Result<(), Status> {
        self.mounted_ready_inner(index, drive, true)
    }
    fn mounted_ready_inner(
        &self,
        index: &CatalogIndex,
        drive: &mut DriveHandle,
        recovering_tail: bool,
    ) -> Result<(), Status> {
        self.mounted_admission_inner(index, drive, recovering_tail)?;
        let mounted = self.mounted().expect("mounted readiness capability");
        // An operator placement cannot authorize LOAD or a library readiness projection.
        let first = drive.probe_media_readiness(MediaFamily::Unknown);
        if first.is_ready() && drive.identity_generation() == mounted.identity_generation {
            return Ok(());
        }
        Err(Status::unavailable(format!(
            "operator-mounted drive is not ready: {:?}",
            first
        )))
    }
}

#[cfg(test)]
mod admission_tests {
    use crate::tape_admission::test_support::{assert_code, drive, Fixture, T};
    use remanence_state::library_log as lib;

    /// Mounted admission preserves tail-recovery's independent gate and codes
    /// only the first ordinary admission refusal, after typed cause ordering.
    #[test]
    fn l1b_mounted_admission_payload_precedence_and_count() {
        for case in ["native", "ordinary", "suffix", "contradicted", "recovery"] {
            let mut f = Fixture::new();
            let expected = match case {
                "native" => {
                    f.raise(T, lib::FenceCause::NativeLogBehindProjection);
                    f.raise(T, lib::FenceCause::TransferError);
                    Some(1)
                }
                "ordinary" => {
                    f.raise(T, lib::FenceCause::TransferError);
                    None
                }
                "suffix" => {
                    f.raise(T, lib::FenceCause::SessionSuffixContradiction);
                    f.raise(T, lib::FenceCause::NativeLogBehindProjection);
                    None
                }
                _ => {
                    f.contradict(T);
                    if case == "recovery" {
                        None
                    } else {
                        Some(2)
                    }
                }
            };
            let drive = drive();
            let cfg = f.config(&drive);
            let index = f.index();
            let result = cfg.mounted_admission_inner(&index, &drive, case == "recovery");
            if case == "recovery" {
                result.unwrap();
            } else {
                let error = result.unwrap_err();
                match expected {
                    Some(1) => assert_eq!(
                        error.message(),
                        "TapeLogBehindProjection: session blocked by active tape-I/O fence"
                    ),
                    Some(2) => assert!(error
                        .message()
                        .starts_with("ContradictedTapeAdmission: mounted write refused:")),
                    _ => assert_eq!(error.message(), "session blocked by active tape-I/O fence"),
                }
            }
            drop(index);
            drop(cfg);
            drop(drive);
            let records = f.refusals();
            assert_eq!(records.len(), usize::from(expected.is_some()));
            if let Some(code) = expected {
                assert_code(&records[0], T, code);
            }
        }
    }
}
