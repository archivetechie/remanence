//! S3 exercises the real runtime's pre-intent fence gate and exact cleaning capability.
use super::*;
use crate::{StateLockGuard, StatePaths};
use std::sync::Arc;

struct Fixture {
    runtime: LibraryRuntime,
    key: crate::DriveKey,
    operation: lib::Id,
    capability: MaintenanceCapability,
    _root: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
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
        let runtime = LibraryRuntime::open(
            &paths,
            true,
            Arc::new(StateLockGuard::acquire(&paths.state_dir).unwrap()),
        )
        .unwrap();
        let key = crate::DriveKey::new("LIB", 0x100);
        runtime
            .append(|a, v| {
                a.drive_observed(
                    lib::DriveObserved {
                        drive_uuid: [1; 16],
                        serial: Some("DRV".into()),
                        identity_source: lib::IdentitySource::DvcidAndInquiry,
                        endpoint: Some(endpoint(&key)),
                        managed: lib::Managed::Rem,
                        inventory_generation: 1,
                        vendor: None,
                        product: None,
                        firmware_rev: None,
                    },
                    lib::AuditActor::System,
                    lib::SourceLayer::Layer4,
                    v,
                )
            })
            .unwrap();
        let operation = runtime
            .start_owned_operation(lib::OperationKind::CleanDrive, lib::AuditActor::System)
            .unwrap();
        let cartridge = runtime
            .register_cleaning_cartridge("CLN001L9", 0x400)
            .unwrap();
        runtime
            .record_cleaning_action(
                "run",
                Some([1; 16]),
                None,
                lib::CleaningAction::Begin(lib::Begin {
                    trigger: lib::CleanTrigger::Now,
                }),
                lib::AuditActor::System,
            )
            .unwrap();
        runtime
            .record_cleaning_action(
                "run",
                None,
                Some(cartridge),
                lib::CleaningAction::Advance(lib::Advance {
                    phase: lib::CleanPhase::MovingIn,
                    home_slot: Some(0x400),
                }),
                lib::AuditActor::System,
            )
            .unwrap();
        let fence = raise(&runtime, &key, Some(operation));
        Self {
            runtime,
            key,
            operation,
            capability: MaintenanceCapability {
                fence,
                run: "run".into(),
                cartridge: cartridge.uuid,
            },
            _root: root,
        }
    }
    fn context(&self) -> MotionContext {
        MotionContext {
            parent_operation: Some(self.operation),
            maintenance: Some(self.capability.clone()),
            ..Default::default()
        }
    }
    fn move_in(&self) -> MotionCommand {
        MotionCommand::Move {
            library_serial: "LIB".into(),
            source: 0x400,
            destination: 0x100,
        }
    }
    fn refuse(&self, command: MotionCommand, context: MotionContext) {
        let records = self.runtime.test_source_records();
        let count = records
            .iter()
            .filter(|r| r.motion_intent().is_some())
            .count();
        let operation = context.parent_operation.unwrap_or(self.operation);
        let budget = self.runtime.readiness_load_attempts(operation).unwrap();
        let before = refusals(&self.runtime);
        let error = self.runtime.intent(&command, &context).unwrap_err();
        assert_eq!(error.kind, MotionAuthorityFailure::SemanticRefusal);
        assert!(error.message.starts_with("DriveFenced: "), "{error}");
        assert!(!self.runtime.motion_fenced());
        assert_eq!(
            self.runtime
                .test_source_records()
                .iter()
                .filter(|r| r.motion_intent().is_some())
                .count(),
            count
        );
        assert_eq!(
            self.runtime.readiness_load_attempts(operation).unwrap(),
            budget
        );
        assert_eq!(refusals(&self.runtime), before + 1);
        assert_eq!(self.runtime.test_source_records().len(), records.len() + 1);
    }
}
fn raise(runtime: &LibraryRuntime, key: &crate::DriveKey, operation: Option<lib::Id>) -> u64 {
    runtime
        .append(|a, v| {
            a.fence_raised(
                lib::FenceRaised {
                    detail: lib::Bounded(vec![]),
                    kind: lib::FenceKind::DriveFault,
                    scope: lib::FenceScope::Drive(endpoint(key)),
                    cause: lib::FenceCause::Cleaning,
                    evidence: None,
                    at: None,
                    operation,
                },
                lib::AuditActor::System,
                lib::SourceLayer::Layer4,
                v,
            )
        })
        .unwrap()
        .sequence
}
fn refusals(runtime: &LibraryRuntime) -> usize {
    runtime
        .test_source_records()
        .iter()
        .filter_map(|r| r.diagnostic())
        .filter(|d| {
            d.event == lib::DiagnosticEvent::RefusalRecorded
                && d.detail
                    .0
                    .iter()
                    .any(|e| e.0.as_str() == "code" && e.1 == lib::DiagnosticValue::Unsigned(12))
        })
        .count()
}
/// Plain and readiness LOAD/MOVE refuse before intent/budget; outgoing movement remains possible.
#[test]
fn drive_fence_backstop_precedes_intent_and_readiness_budget() {
    for mode in 0..3 {
        let f = Fixture::new();
        let mut ctx = MotionContext::default();
        let command = if mode == 1 {
            f.move_in()
        } else {
            MotionCommand::Load(f.key.clone())
        };
        if mode == 2 {
            let op = f
                .runtime
                .start_owned_operation(
                    lib::OperationKind::MediaReadinessWait,
                    lib::AuditActor::System,
                )
                .unwrap();
            f.runtime
                .begin_owned_readiness(&crate::MediaReadinessOperationInput {
                    operation_id: uuid::Uuid::from_bytes(op),
                    run_id: None,
                    endpoint: f.key.clone(),
                    changer_sg: None,
                    drive_sg: None,
                    drive_serial: None,
                    barcode: None,
                    source_slot: None,
                    media_generation: None,
                    media_family: None,
                    phase: "readiness_poll".into(),
                    state: "planned".into(),
                    dirty_scope: Some("drive+tape".into()),
                    deadline_at_utc: None,
                    evidence_path: None,
                })
                .unwrap();
            ctx.parent_operation = Some(op);
            ctx.load_confirmation = LoadConfirmation::Readiness;
        }
        f.refuse(command, ctx);
    }
    for outgoing in [false, true] {
        let f = Fixture::new();
        let command = if outgoing {
            MotionCommand::Move {
                library_serial: "LIB".into(),
                source: 0x100,
                destination: 0x400,
            }
        } else {
            MotionCommand::Unload(f.key.clone())
        };
        f.runtime
            .intent(&command, &MotionContext::default())
            .unwrap();
        assert_eq!(refusals(&f.runtime), 0);
    }
}
/// Only the exact run/cartridge/owned raise/operation and MOVE home authorize maintenance.
#[test]
fn drive_fence_backstop_validates_each_maintenance_capability_binding() {
    for moving in [false, true] {
        let f = Fixture::new();
        f.runtime
            .intent(
                &if moving {
                    f.move_in()
                } else {
                    MotionCommand::Load(f.key.clone())
                },
                &f.context(),
            )
            .unwrap();
        assert_eq!(refusals(&f.runtime), 0);
    }
    for defect in 0..7 {
        let f = Fixture::new();
        let mut ctx = f.context();
        let mut command = f.move_in();
        match defect {
            0 => ctx.parent_operation = None,
            1 => {
                ctx.parent_operation = Some(
                    f.runtime
                        .start_owned_operation(
                            lib::OperationKind::CleanDrive,
                            lib::AuditActor::System,
                        )
                        .unwrap(),
                )
            }
            2 => ctx.maintenance.as_mut().unwrap().run = "other-run".into(),
            3 => ctx.maintenance.as_mut().unwrap().cartridge = [99; 16],
            4 => {
                command = MotionCommand::Move {
                    library_serial: "LIB".into(),
                    source: 0x401,
                    destination: 0x100,
                }
            }
            5 => {
                raise(&f.runtime, &f.key, None);
            }
            6 => {
                raise(&f.runtime, &f.key, None);
                f.runtime
                    .append(|a, v| {
                        a.fence_released(
                            lib::FenceReleased {
                                detail: lib::Bounded(vec![]),
                                fence: f.capability.fence,
                                evidence: "owned raise released; foreign obligation remains".into(),
                                forced: false,
                            },
                            lib::AuditActor::System,
                            lib::SourceLayer::Layer4,
                            v,
                        )
                    })
                    .unwrap();
            }
            _ => unreachable!(),
        }
        f.refuse(command, ctx);
    }
}
