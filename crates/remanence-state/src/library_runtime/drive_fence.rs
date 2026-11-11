//! Endpoint-scoped data admission reads typed drive fences under dispatch ownership.
//! The first active raise supplies the refusal; identity observations and SQL do not.
use super::{lib, LibraryRuntime, RefusalSubject, RuntimeError};

/// The earliest active drive obligation on a physical endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriveFenced {
    /// Endpoint named by the raise, even when no drive identity was observed.
    pub endpoint: lib::DriveKey,
    /// Durable library sequence of the active raise.
    pub fence: u64,
    /// Typed reason for the drive obligation.
    pub cause: lib::FenceCause,
}
impl std::fmt::Display for DriveFenced {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let endpoint = match &self.endpoint {
            lib::DriveKey::Bay(bay) => crate::DriveKey::new(bay.library_serial.as_str(), bay.bay),
            lib::DriveKey::Standalone(drive) => crate::DriveKey::Standalone {
                drive_serial: drive.drive_serial.as_str().to_owned(),
            },
        };
        write!(
            f,
            "DriveFenced: {} library:{} {:?}",
            endpoint.label(),
            self.fence,
            self.cause
        )
    }
}
impl std::error::Error for DriveFenced {}

/// Admission retains the distinction between a drive obligation and runtime failure.
#[derive(Debug, thiserror::Error)]
pub enum DriveDataRefusal {
    /// Active drive-scoped obligation.
    #[error(transparent)]
    Fenced(DriveFenced),
    /// Runtime motion fence takes precedence over endpoint fences.
    #[error(transparent)]
    Runtime(RuntimeError),
}
impl LibraryRuntime {
    /// Admit data work in the same dispatch order as fence publication.
    pub fn admit_drive_data(
        &self,
        endpoint: &crate::DriveKey,
        subject: RefusalSubject,
        actor: lib::AuditActor,
    ) -> Result<(), DriveDataRefusal> {
        if self.motion_fenced() {
            return Err(DriveDataRefusal::Runtime(RuntimeError::MotionFenced));
        }
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(refusal) = self.drive_fenced(endpoint) {
            self.record_refusal_locked(lib::RefusalCode::DriveFenced, subject, actor);
            return Err(DriveDataRefusal::Fenced(refusal));
        }
        Ok(())
    }

    /// Read a planning exclusion without recording a refused decision.
    pub fn drive_fenced(&self, endpoint: &crate::DriveKey) -> Option<DriveFenced> {
        let endpoint = super::motion::endpoint(endpoint);
        self.0
            .authority
            .active_drive_fences(&endpoint)
            .into_iter()
            .next()
            .map(|(fence, raise)| DriveFenced {
                endpoint,
                fence,
                cause: raise.cause,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{StateLockGuard, StatePaths};
    use std::sync::{atomic::Ordering, Arc};

    /// Exercise admission against real source records, restart, SQL loss and rebuild.
    #[test]
    fn drive_data_refuses_without_identity_after_restart_and_sql_rebuild() {
        let root = tempfile::tempdir().unwrap();
        let paths = StatePaths {
            config_path: root.path().join("config"),
            state_dir: root.path().join("state"),
            audit_dir: root.path().join("library"),
            journal_dir: root.path().join("native"),
            sqlite_path: root.path().join("catalog.sqlite"),
            tape_cache_dir: root.path().join("cache"),
            calibration_dir: root.path().join("calibration"),
        };
        let lease = Arc::new(StateLockGuard::acquire(&paths.state_dir).unwrap());
        let mut runtime = LibraryRuntime::open(&paths, true, lease.clone()).unwrap();
        let endpoint = crate::DriveKey::new("LIB", 1);
        let subject = RefusalSubject::Tape(Some([0x42; 16]));
        let actor = lib::AuditActor::User("operator".into());
        let before = runtime.test_source_records().len();
        runtime
            .admit_drive_data(&endpoint, subject, actor.clone())
            .unwrap();
        assert_eq!(runtime.test_source_records().len(), before);
        let mut sequences = Vec::new();
        for cause in [lib::FenceCause::Cleaning, lib::FenceCause::TransferError] {
            sequences.push(
                runtime
                    .append(|a, v| {
                        a.fence_raised(
                            lib::FenceRaised {
                                detail: lib::Bounded(vec![]),
                                kind: lib::FenceKind::DriveFault,
                                scope: lib::FenceScope::Drive(super::super::motion::endpoint(
                                    &endpoint,
                                )),
                                cause,
                                evidence: None,
                                at: None,
                                operation: None,
                            },
                            lib::AuditActor::System,
                            lib::SourceLayer::Layer4,
                            v,
                        )
                    })
                    .unwrap()
                    .sequence,
            );
        }
        for stage in 0..3 {
            if stage > 0 {
                drop(runtime);
                if stage == 2 {
                    for suffix in ["", "-wal", "-shm"] {
                        let path = std::path::PathBuf::from(format!(
                            "{}{suffix}",
                            paths.sqlite_path.display()
                        ));
                        if path.exists() {
                            std::fs::remove_file(path).unwrap();
                        }
                    }
                }
                runtime = LibraryRuntime::open(&paths, true, lease.clone()).unwrap();
                if stage == 2 {
                    runtime
                        .rebuild_projection(
                            &mut crate::projector::open(&paths.sqlite_path)
                                .unwrap()
                                .with_library_runtime(runtime.clone())
                                .write_handle(),
                        )
                        .unwrap();
                }
            }
            assert!(!runtime
                .test_source_records()
                .iter()
                .any(|r| matches!(r.body_view(), lib::BodyView::DriveObserved(_))));
            let before = runtime.test_source_records().len();
            let error = runtime
                .admit_drive_data(&endpoint, subject, actor.clone())
                .unwrap_err();
            let DriveDataRefusal::Fenced(ref refusal) = error else {
                panic!("{error}")
            };
            assert_eq!(refusal.fence, sequences[0]);
            assert_eq!(refusal.cause, lib::FenceCause::Cleaning);
            assert_eq!(refusal.endpoint, super::super::motion::endpoint(&endpoint));
            let text = error.to_string();
            assert!(text.starts_with("DriveFenced: ") && text.contains(&endpoint.label()));
            assert!(
                text.contains(&format!("library:{}", sequences[0])) && text.contains("Cleaning")
            );
            for forbidden in [
                "busy",
                "no free drive",
                "media_readiness_state=",
                "media-readiness fence",
            ] {
                assert!(!text.contains(forbidden));
            }
            assert_eq!(runtime.test_source_records().len(), before + 1);
            let records = super::super::refusal_tests::refusals(&runtime);
            assert_eq!(
                records.last().unwrap(),
                &(
                    12,
                    "DriveFenced".into(),
                    "tape".into(),
                    Some(uuid::Uuid::from_bytes([0x42; 16]).to_string()),
                    actor.clone(),
                    lib::SourceLayer::Layer5
                )
            );
            let before = runtime.test_source_records().len();
            assert!(runtime.drive_fenced(&endpoint).is_some());
            runtime
                .admit_drive_data(&crate::DriveKey::new("LIB", 2), subject, actor.clone())
                .unwrap();
            assert_eq!(runtime.test_source_records().len(), before);
        }
        runtime.0.motion_fenced.store(true, Ordering::Release);
        let before = runtime.test_source_records().len();
        assert!(matches!(
            runtime.admit_drive_data(&endpoint, subject, actor),
            Err(DriveDataRefusal::Runtime(RuntimeError::MotionFenced))
        ));
        assert_eq!(runtime.test_source_records().len(), before);
    }
}
