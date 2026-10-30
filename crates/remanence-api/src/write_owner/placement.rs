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
    pub identity_generation: u64,
    pub actor: AuditActor,
    pub audit_error: Arc<Mutex<Option<String>>>,
    pub audit_session: Arc<Mutex<Option<Uuid>>>,
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
            return Err(Status::failed_precondition(
                "session blocked by active tape-I/O fence",
            ));
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
        Ok(())
    }
    pub(super) fn mounted_ready(
        &self,
        index: &CatalogIndex,
        drive: &mut DriveHandle,
    ) -> Result<(), Status> {
        self.mounted_admission(index, drive)?;
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
