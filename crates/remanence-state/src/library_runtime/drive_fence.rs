//! Endpoint-scoped data admission reads typed drive fences under dispatch ownership.
//! The first active raise supplies the refusal; identity observations and SQL do not.
use super::{lib, LibraryRuntime, RefusalSubject, RuntimeError};
use std::sync::atomic::Ordering;

#[cfg(any(test, feature = "fixtures"))]
thread_local! {
    /// Fail one hold publication on this thread without the store-failure latch.
    static FAIL_NEXT_STANDALONE_HOLD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// An active standalone drive hold, rendered from its authoritative raise.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StandaloneDriveHold {
    /// Durable library sequence identifying the hold.
    pub fence: u64,
    /// Serial of the standalone endpoint named by the raise.
    pub drive_serial: String,
    /// Typed reason retained from the raise, independent of release eligibility.
    pub cause: lib::FenceCause,
    /// Raise details in their original order, with text unwrapped and other values in Debug form.
    pub detail: Vec<(String, String)>,
}

/// Render a scoped raise without losing its diagnostic entry order or scalar type.
fn standalone_hold(
    fence: u64,
    drive_serial: &str,
    raise: &lib::FenceRaised,
) -> StandaloneDriveHold {
    StandaloneDriveHold {
        fence,
        drive_serial: drive_serial.to_owned(),
        cause: raise.cause,
        detail: raise
            .detail
            .0
            .iter()
            .map(|lib::DiagnosticEntry(key, value)| {
                let value = match value {
                    lib::DiagnosticValue::Text(text) => text.as_str().to_owned(),
                    value => format!("{value:?}"),
                };
                (key.as_str().to_owned(), value)
            })
            .collect(),
    }
}

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
        )?;
        if let lib::DriveKey::Standalone(drive) = &self.endpoint {
            write!(
                f,
                "; release the hold with `rem tape quarantine release library:{} --device PATH --allow-device {} --ack TEXT`",
                self.fence,
                drive.drive_serial.as_str()
            )?;
        }
        Ok(())
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
    /// List active standalone holds in sequence order under dispatch ownership.
    pub fn standalone_drive_holds(&self) -> Result<Vec<StandaloneDriveHold>, RuntimeError> {
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        if self.0.projection_dirty.load(Ordering::Acquire) {
            self.catch_up_locked()?;
        }
        let sequences = self.0.projector.reader()?.active_drive_fence_sequences()?;
        Ok(sequences
            .into_iter()
            .filter_map(|sequence| {
                let raise = self.0.authority.active_fence(sequence)?;
                match &raise.scope {
                    lib::FenceScope::Drive(lib::DriveKey::Standalone(drive)) => Some(
                        standalone_hold(sequence, drive.drive_serial.as_str(), &raise),
                    ),
                    _ => None,
                }
            })
            .collect())
    }

    /// Acknowledge one standalone hold by scope, preserving independent tape fences.
    pub fn release_standalone_drive_hold(
        &self,
        id: &str,
        acknowledgment: &str,
        actor: lib::AuditActor,
    ) -> Result<Option<StandaloneDriveHold>, RuntimeError> {
        if acknowledgment.trim().is_empty() {
            return Err(crate::StateError::ConfigInvalid(
                "acknowledgment must not be empty".into(),
            )
            .into());
        }
        let Some(sequence) = id
            .strip_prefix("library:")
            .and_then(|s| s.parse::<u64>().ok())
        else {
            return Ok(None);
        };
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        let Some(raise) = self.0.authority.active_fence(sequence) else {
            return Ok(None);
        };
        let lib::FenceScope::Drive(lib::DriveKey::Standalone(drive)) = &raise.scope else {
            return Ok(None);
        };
        let hold = standalone_hold(sequence, drive.drive_serial.as_str(), &raise);
        self.append_locked(|a, v| {
            a.fence_released(
                lib::FenceReleased {
                    fence: sequence,
                    evidence: acknowledgment.into(),
                    forced: false,
                    detail: lib::Bounded(vec![]),
                },
                actor,
                lib::SourceLayer::Layer4,
                v,
            )
        })?;
        Ok(Some(hold))
    }

    /// Hold a standalone drive after write uncertainty, independently of its tape fence.
    /// The fold decides whether publication succeeded, including after an append error.
    pub fn raise_standalone_drive_hold(
        &self,
        endpoint: &crate::DriveKey,
        tape: lib::Id,
        cause: lib::FenceCause,
        detail: lib::Bounded<lib::DiagnosticEntry, 256>,
    ) -> Result<(), RuntimeError> {
        if !matches!(endpoint, crate::DriveKey::Standalone { .. }) {
            return Ok(());
        }
        let endpoint = super::motion::endpoint(endpoint);
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        if !self.0.authority.active_drive_fences(&endpoint).is_empty() {
            return Ok(());
        }
        let result = self.append_locked(|authority, view| {
            #[cfg(any(test, feature = "fixtures"))]
            if FAIL_NEXT_STANDALONE_HOLD.with(|fail| fail.replace(false)) {
                return Err(super::AuthorityError::Store(super::StoreError::Fold(
                    lib::FoldError::InvalidSchema,
                )));
            }
            authority.fence_raised(
                lib::FenceRaised {
                    detail,
                    kind: lib::FenceKind::DriveFault,
                    scope: lib::FenceScope::Drive(endpoint.clone()),
                    cause,
                    evidence: None,
                    at: None,
                    operation: None,
                },
                lib::AuditActor::System,
                lib::SourceLayer::Layer5,
                view,
            )
        });
        if let Err(error) = result {
            if !self.0.authority.active_drive_fences(&endpoint).is_empty() {
                tracing::warn!(%error, ?endpoint, "standalone drive hold recorded despite append error");
                return Ok(());
            }
            self.fence_motion_after_evidence_failure();
            self.record_refusal_locked(
                lib::RefusalCode::StandaloneFenceUnrecorded,
                RefusalSubject::Tape(Some(tape)),
                lib::AuditActor::local_user(),
            );
            return Err(error);
        }
        Ok(())
    }

    /// Fail the next standalone hold append on this thread; an already held drive does not consume it.
    #[cfg(any(test, feature = "fixtures"))]
    pub fn fail_next_standalone_hold_for_test(&self) {
        FAIL_NEXT_STANDALONE_HOLD.with(|fail| fail.set(true));
    }

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

    /// Open a source-backed runtime without requiring observed drive identities.
    fn hold_fixture() -> (
        tempfile::TempDir,
        StatePaths,
        Arc<StateLockGuard>,
        LibraryRuntime,
    ) {
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
        let runtime = LibraryRuntime::open(&paths, true, lease.clone()).unwrap();
        (root, paths, lease, runtime)
    }

    /// One drive hold retains the first cause and detail; bays and repeated raises add nothing.
    #[test]
    fn standalone_hold_records_once_with_typed_scope() {
        let (_root, _paths, _lease, runtime) = hold_fixture();
        let endpoint = crate::DriveKey::Standalone {
            drive_serial: "HOLD-A".into(),
        };
        let detail = lib::Bounded(vec![lib::DiagnosticEntry(
            "evidence_json".into(),
            lib::DiagnosticValue::Text("E".into()),
        )]);
        let before = runtime.test_source_records().len();
        runtime
            .raise_standalone_drive_hold(
                &endpoint,
                [0x42; 16],
                lib::FenceCause::TransferError,
                detail.clone(),
            )
            .unwrap();
        let records = runtime.test_source_records();
        assert_eq!(records.len(), before + 1);
        let record = records.last().unwrap();
        let lib::BodyView::FenceRaised(raised) = record.body_view() else {
            panic!("expected FenceRaised")
        };
        assert_eq!(
            runtime.active_fence(runtime.standalone_drive_holds().unwrap()[0].fence),
            Some(raised.clone())
        );
        assert_eq!(raised.kind, lib::FenceKind::DriveFault);
        assert_eq!(
            raised.scope,
            lib::FenceScope::Drive(super::super::motion::endpoint(&endpoint))
        );
        assert_eq!(raised.cause, lib::FenceCause::TransferError);
        assert_eq!(raised.detail, detail);
        assert_eq!(raised.evidence, None);
        assert_eq!(raised.at, None);
        assert_eq!(raised.operation, None);
        assert_eq!(record.envelope.actor, lib::AuditActor::System);
        assert_eq!(record.envelope.source_layer, lib::SourceLayer::Layer5);
        assert_eq!(
            runtime.drive_fenced(&endpoint).unwrap(),
            DriveFenced {
                endpoint: super::super::motion::endpoint(&endpoint),
                fence: runtime.standalone_drive_holds().unwrap()[0].fence,
                cause: lib::FenceCause::TransferError,
            }
        );
        runtime
            .raise_standalone_drive_hold(
                &endpoint,
                [0x42; 16],
                lib::FenceCause::PartialBatch,
                lib::Bounded(vec![]),
            )
            .unwrap();
        assert_eq!(
            runtime.test_source_records().len(),
            before + 1,
            "second call must retain the existing hold"
        );
        runtime
            .raise_standalone_drive_hold(
                &crate::DriveKey::new("LIB", 1),
                [0x42; 16],
                lib::FenceCause::PartialBatch,
                lib::Bounded(vec![]),
            )
            .unwrap();
        assert_eq!(runtime.test_source_records().len(), before + 1);
        assert!(!runtime.motion_fenced());
    }

    /// A missing hold latches and counts once; the one-shot seam permits the next publication.
    #[test]
    fn standalone_hold_failure_latches_and_counts_once() {
        let (_root, _paths, _lease, runtime) = hold_fixture();
        let endpoint = crate::DriveKey::Standalone {
            drive_serial: "HOLD-B".into(),
        };
        let tape = [0x42; 16];
        let before = runtime.test_source_records().len();
        runtime.fail_next_standalone_hold_for_test();
        runtime
            .raise_standalone_drive_hold(
                &endpoint,
                tape,
                lib::FenceCause::TransferError,
                lib::Bounded(vec![]),
            )
            .unwrap_err();
        assert!(runtime.motion_fenced(), "unrecorded hold must latch motion");
        assert!(runtime.drive_fenced(&endpoint).is_none());
        let records = runtime.test_source_records();
        assert_eq!(records.len(), before + 1);
        assert!(matches!(
            records.last().unwrap().body_view(),
            lib::BodyView::Diagnostic(_)
        ));
        assert_eq!(
            super::super::refusal_tests::refusals(&runtime),
            vec![(
                5,
                "StandaloneFenceUnrecorded".into(),
                "tape".into(),
                Some(uuid::Uuid::from_bytes(tape).to_string()),
                lib::AuditActor::local_user(),
                lib::SourceLayer::Layer5,
            )]
        );
        runtime
            .raise_standalone_drive_hold(
                &endpoint,
                tape,
                lib::FenceCause::TransferError,
                lib::Bounded(vec![]),
            )
            .unwrap();
        assert_eq!(runtime.test_source_records().len(), before + 2);
        assert!(runtime.drive_fenced(&endpoint).is_some());
    }

    /// Seed the same typed raise that a drive producer publishes.
    fn raise_drive(
        runtime: &LibraryRuntime,
        endpoint: &crate::DriveKey,
        cause: lib::FenceCause,
        detail: Vec<lib::DiagnosticEntry>,
    ) -> u64 {
        runtime
            .append(|a, v| {
                a.fence_raised(
                    lib::FenceRaised {
                        detail: lib::Bounded(detail),
                        kind: lib::FenceKind::DriveFault,
                        scope: lib::FenceScope::Drive(super::super::motion::endpoint(endpoint)),
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
            .sequence
    }

    /// Tape release refuses a standalone hold with a route naming its drive, before append.
    #[test]
    fn hold_tape_release_names_standalone_route() {
        let (_root, _paths, _lease, runtime) = hold_fixture();
        let endpoint = crate::DriveKey::Standalone {
            drive_serial: "HOLD-1".into(),
        };
        let sequence = raise_drive(
            &runtime,
            &endpoint,
            lib::FenceCause::SessionSuffixContradiction,
            vec![],
        );
        let before = runtime.test_source_records().len();
        let error = runtime
            .release_tape_fence(
                &format!("library:{sequence}"),
                "drive checked",
                false,
                lib::AuditActor::local_user(),
            )
            .unwrap_err();
        let expected = format!("fence library:{sequence} holds standalone drive HOLD-1; stop the daemon and run `rem tape quarantine release library:{sequence} --device PATH --allow-device HOLD-1 --ack TEXT` as a standalone command");
        assert!(error.to_string().contains(&expected), "{error}");
        assert_eq!(runtime.test_source_records().len(), before);
        assert!(runtime.active_fence(sequence).is_some());
    }

    /// Standalone admission adds actionable hold advice; bay text stays exact.
    #[test]
    fn hold_admission_names_release_and_preserves_bay_text() {
        let (_root, _paths, _lease, runtime) = hold_fixture();
        let standalone = crate::DriveKey::Standalone {
            drive_serial: "HOLD-1".into(),
        };
        let bay = crate::DriveKey::new("LIB", 1);
        let a = raise_drive(
            &runtime,
            &standalone,
            lib::FenceCause::SessionSuffixContradiction,
            vec![],
        );
        let b = raise_drive(&runtime, &bay, lib::FenceCause::Cleaning, vec![]);
        for (endpoint, expected) in [
            (&standalone, format!("DriveFenced: drive HOLD-1 library:{a} SessionSuffixContradiction; release the hold with `rem tape quarantine release library:{a} --device PATH --allow-device HOLD-1 --ack TEXT`")),
            (&bay, format!("DriveFenced: LIB drive bay 0x0001 library:{b} Cleaning")),
        ] {
            let error = runtime.admit_drive_data(endpoint, RefusalSubject::Tape(None), lib::AuditActor::local_user()).unwrap_err();
            assert_eq!(error.to_string(), expected);
        }
    }

    /// List only standalone scopes and release exactly one hold, durably across rebuilds.
    #[test]
    fn standalone_holds_list_release_and_replay() {
        let (_root, paths, lease, mut runtime) = hold_fixture();
        let first = crate::DriveKey::Standalone {
            drive_serial: "HOLD-1".into(),
        };
        let bay = crate::DriveKey::new("LIB", 1);
        let second = crate::DriveKey::Standalone {
            drive_serial: "HOLD-2".into(),
        };
        let a = raise_drive(
            &runtime,
            &first,
            lib::FenceCause::SessionSuffixContradiction,
            vec![
                lib::DiagnosticEntry("barcode".into(), lib::DiagnosticValue::Text("FIRST".into())),
                lib::DiagnosticEntry("slot".into(), lib::DiagnosticValue::Unsigned(7)),
            ],
        );
        let b = raise_drive(&runtime, &bay, lib::FenceCause::Cleaning, vec![]);
        let c = raise_drive(&runtime, &second, lib::FenceCause::TransferError, vec![]);
        let expected_a = StandaloneDriveHold {
            fence: a,
            drive_serial: "HOLD-1".into(),
            cause: lib::FenceCause::SessionSuffixContradiction,
            detail: vec![
                ("barcode".into(), "FIRST".into()),
                ("slot".into(), "Unsigned(7)".into()),
            ],
        };
        let expected_c = StandaloneDriveHold {
            fence: c,
            drive_serial: "HOLD-2".into(),
            cause: lib::FenceCause::TransferError,
            detail: vec![],
        };
        assert_eq!(
            runtime.standalone_drive_holds().unwrap(),
            vec![expected_a.clone(), expected_c.clone()]
        );
        let before = runtime.test_source_records().len();
        for id in [format!("library:{b}"), "tioq-1".into(), "library:x".into()] {
            assert_eq!(
                runtime
                    .release_standalone_drive_hold(
                        &id,
                        "drive checked",
                        lib::AuditActor::local_user()
                    )
                    .unwrap(),
                None
            );
            assert_eq!(runtime.test_source_records().len(), before);
        }
        let id = format!("library:{a}");
        assert!(runtime
            .release_standalone_drive_hold(&id, " \t", lib::AuditActor::local_user())
            .is_err());
        assert_eq!(runtime.test_source_records().len(), before);
        assert_eq!(
            runtime
                .release_standalone_drive_hold(&id, "drive checked", lib::AuditActor::local_user())
                .unwrap(),
            Some(expected_a)
        );
        let records = runtime.test_source_records();
        assert_eq!(records.len(), before + 1);
        let record = records.last().unwrap();
        let lib::BodyView::FenceReleased(released) = record.body_view() else {
            panic!("expected FenceReleased")
        };
        assert_eq!(released.fence, a);
        assert_eq!(released.evidence.as_str(), "drive checked");
        assert!(!released.forced);
        assert!(released.detail.0.is_empty());
        assert_eq!(record.envelope.actor, lib::AuditActor::local_user());
        assert_eq!(record.envelope.source_layer, lib::SourceLayer::Layer4);
        assert!(runtime.drive_fenced(&first).is_none());
        assert_eq!(
            runtime.standalone_drive_holds().unwrap(),
            vec![expected_c.clone()]
        );
        assert_eq!(
            runtime
                .release_standalone_drive_hold(&id, "drive checked", lib::AuditActor::local_user())
                .unwrap(),
            None
        );
        assert_eq!(runtime.test_source_records().len(), before + 1);
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
            assert_eq!(
                runtime.standalone_drive_holds().unwrap(),
                vec![expected_c.clone()]
            );
        }
    }

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
