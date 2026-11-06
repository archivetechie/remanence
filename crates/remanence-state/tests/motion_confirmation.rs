//! B4b-3 writer confirmation checks: durable kind validation, refusal without
//! publication, and ordered inventory/readiness confirmation through the typed API.
use remanence_state::library_log::store::{Clock, Origin, StoreError, UuidSource};
use remanence_state::{library_log::*, StateLockGuard};
use std::sync::Arc;

struct Fixed;
impl Clock for Fixed {
    fn now(&self) -> u64 {
        1_793_664_000_000_000_000
    }
}
impl UuidSource for Fixed {
    fn next(&self, n: u64) -> Id {
        let mut id = [0x72; 16];
        id[8..].copy_from_slice(&n.to_be_bytes());
        id
    }
}
struct View;
impl TapeView for View {
    fn lookup(&self, _: &TapeAt) -> Result<TapeLookup, remanence_state::library_log::FoldError> {
        Ok(TapeLookup::NotYet)
    }
}
fn evidence() -> CompletionEvidence {
    CompletionEvidence {
        dispatch: Dispatch::Issued,
        completion: Completion::Known,
        scsi_status: Some(0),
        host_status: Some(0),
        driver_status: Some(0),
        sense: None,
        residual: None,
    }
}
fn reference(r: &AuthorityReceipt) -> DecisionRef {
    DecisionRef {
        sequence: r.receipt.sequence,
        record_uuid: r.receipt.record_uuid,
    }
}

/// An unresolved intent remains usable after refused evidence; a valid observation
/// may be older than the latest observation and survives an authority reopen.
#[test]
fn motion_confirmation_requires_durable_record_of_matching_kind() {
    // MOVE, UNLOAD and both LOAD confirmation paths; repeat drive motions
    // against standalone endpoints as well as library bays.
    for (readiness, motion_kind, standalone) in [
        (false, 2, false),
        (true, 0, false),
        (false, 0, false),
        (false, 1, false),
        (true, 0, true),
        (false, 0, true),
        (false, 1, true),
    ] {
        let root = tempfile::tempdir().unwrap();
        let lease = Arc::new(StateLockGuard::acquire(root.path()).unwrap());
        let open = || {
            LibraryAuthority::open(
                &root.path().join("library"),
                [0x72; 16],
                Origin {
                    host: "test".into(),
                    pid: 1,
                    build: "confirmations".into(),
                },
                Box::new(Fixed),
                Box::new(Fixed),
                false,
                &View,
                lease.clone(),
            )
            .unwrap()
        };
        let a = open();
        let endpoint = if standalone {
            DriveKey::Standalone(Standalone {
                drive_serial: "DRIVE".into(),
            })
        } else {
            DriveKey::Bay(Bay {
                library_serial: "LIB".into(),
                bay: 1,
            })
        };
        let intent = a
            .motion_intent(
                MotionIntent {
                    command: [1; 16],
                    parent_operation: None,
                    motion: match motion_kind {
                        0 => Motion::Load(Load {
                            endpoint: endpoint.clone(),
                            attempt: 1,
                        }),
                        1 => Motion::Unload(Unload {
                            endpoint: endpoint.clone(),
                        }),
                        _ => Motion::Move(Move {
                            library_serial: "LIB".into(),
                            source: 2,
                            destination: 1,
                        }),
                    },
                    tape: None,
                    inventory_generation: None,
                    readiness_generation: None,
                },
                AuditActor::System,
                SourceLayer::Layer4,
                &View,
            )
            .unwrap();
        let confirm = |sequence| {
            if readiness {
                Confirmation::Readiness(sequence)
            } else {
                Confirmation::Inventory(sequence)
            }
        };
        let mut outcome = MotionOutcome {
            intent: reference(&intent),
            result: MotionResult::Done,
            evidence: evidence(),
            confirmation: None,
        };
        // Zero, wrong kind, next-unwritten and far-future references must not append.
        for sequence in [
            0,
            intent.receipt.sequence,
            intent.receipt.sequence + 1,
            u64::MAX,
        ] {
            outcome.confirmation = Some(confirm(sequence));
            assert!(matches!(
                a.motion_outcome(
                    outcome.clone(),
                    AuditActor::System,
                    SourceLayer::Layer4,
                    &View
                ),
                Err(AuthorityError::Store(StoreError::Fold(
                    FoldError::Confirmation
                )))
            ));
        }
        let mut first = None;
        for generation in 1..=2 {
            let observed = if readiness {
                a.begin_readiness(
                    ReadinessObserved {
                        scope: ReadinessScope::Endpoint,
                        media: None,
                        operation: [0; 16],
                        endpoint: endpoint.clone(),
                        tape: None,
                        barcode: None,
                        phase: ReadinessPhase::Ready,
                        state: ReadinessState::Ready,
                        previous: None,
                        started_unix_ns: Fixed.now(),
                        limit_ms: 1000,
                        evidence: None,
                        load_intent: Some(reference(&intent)),
                        ready_generation: generation,
                        result: ReadinessResult::Ready,
                    },
                    AuditActor::System,
                    SourceLayer::Layer4,
                    &View,
                )
                .unwrap()
                .1
            } else {
                a.inventory_reconciled(
                    InventoryReconciled {
                        subject: if standalone {
                            InventorySubject::Standalone(Standalone {
                                drive_serial: "DRIVE".into(),
                            })
                        } else {
                            InventorySubject::Library("LIB".into())
                        },
                        observation_generation: generation,
                        resolves: Bounded(vec![]),
                        complete: true,
                        elements: Bounded(vec![]),
                    },
                    AuditActor::System,
                    SourceLayer::Layer4,
                    &View,
                )
                .unwrap()
            };
            if first.is_none() {
                assert_eq!(observed.receipt.sequence, intent.receipt.sequence + 1);
                first = Some(observed.receipt.sequence);
            }
        }
        drop(a);
        let a = open();
        outcome.confirmation = Some(confirm(first.unwrap()));
        let done = a
            .motion_outcome(outcome, AuditActor::System, SourceLayer::Layer4, &View)
            .unwrap();
        assert_eq!(done.receipt.sequence, intent.receipt.sequence + 3);
        assert_eq!(done.applied.len(), 1);
    }
}

/// A crash before outcome and an explicitly Unknown outcome both retain endpoint
/// uncertainty across reopen. Reconciliation consumes only the named endpoint,
/// and SQL loss/replay twice preserves the same derived inventory and motion.
#[test]
fn standalone_reconciliation_survives_reopen_and_sql_loss() {
    use remanence_state::projector::{self, apply::ApplyPath};
    use rusqlite::{Connection, OpenFlags};
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("library");
    let lease = Arc::new(StateLockGuard::acquire(root.path()).unwrap());
    let open = || {
        LibraryAuthority::open(
            &dir,
            [0x72; 16],
            Origin {
                host: "test".into(),
                pid: 1,
                build: "standalone".into(),
            },
            Box::new(Fixed),
            Box::new(Fixed),
            false,
            &View,
            lease.clone(),
        )
        .unwrap()
    };
    let a = open();
    let endpoint = |serial: &str| {
        DriveKey::Standalone(Standalone {
            drive_serial: serial.into(),
        })
    };
    let intent = |command, serial: &str, load| {
        a.motion_intent(
            MotionIntent {
                command: [command; 16],
                parent_operation: None,
                motion: if load {
                    Motion::Load(Load {
                        endpoint: endpoint(serial),
                        attempt: 1,
                    })
                } else {
                    Motion::Unload(Unload {
                        endpoint: endpoint(serial),
                    })
                },
                tape: None,
                inventory_generation: None,
                readiness_generation: None,
            },
            AuditActor::System,
            SourceLayer::Layer4,
            &View,
        )
        .unwrap()
    };
    let load = intent(1, "SAME", true);
    let unload = intent(2, "OTHER", false);
    a.motion_outcome(
        MotionOutcome {
            intent: reference(&unload),
            result: MotionResult::Unknown,
            evidence: evidence(),
            confirmation: None,
        },
        AuditActor::System,
        SourceLayer::Layer4,
        &View,
    )
    .unwrap();
    drop(a);
    let replay = || remanence_state::library_log::store::replay(&dir, [0x72; 16], &View).unwrap();
    for serial in ["SAME", "OTHER"] {
        assert!(replay()
            .fold
            .inventory_unknown(&fold::Scope::Endpoint(endpoint(serial))));
    }
    let a = open();
    let observe = |subject, generation, resolves| InventoryReconciled {
        subject,
        observation_generation: generation,
        resolves: Bounded(resolves),
        complete: true,
        elements: Bounded(vec![]),
    };
    let append =
        |value| a.inventory_reconciled(value, AuditActor::System, SourceLayer::Layer4, &View);
    let standalone = |serial: &str| {
        InventorySubject::Standalone(Standalone {
            drive_serial: serial.into(),
        })
    };
    assert!(matches!(
        append(observe(standalone("OTHER"), 1, vec![reference(&load)])),
        Err(AuthorityError::Store(StoreError::Fold(
            FoldError::InventoryScope
        )))
    ));
    // A library sharing the drive serial is a distinct generation namespace.
    append(observe(InventorySubject::Library("SAME".into()), 9, vec![])).unwrap();
    append(observe(standalone("SAME"), 1, vec![reference(&load)])).unwrap();
    drop(a);
    assert!(!replay()
        .fold
        .inventory_unknown(&fold::Scope::Endpoint(endpoint("SAME"))));
    assert!(replay()
        .fold
        .inventory_unknown(&fold::Scope::Endpoint(endpoint("OTHER"))));
    let a = open();
    let append =
        |value| a.inventory_reconciled(value, AuditActor::System, SourceLayer::Layer4, &View);
    assert!(matches!(
        append(observe(standalone("SAME"), 1, vec![])),
        Err(AuthorityError::Store(StoreError::Fold(
            FoldError::InventoryGeneration
        )))
    ));
    append(observe(standalone("OTHER"), 1, vec![reference(&unload)])).unwrap();
    append(observe(standalone("SAME"), 2, vec![])).unwrap();
    drop(a);
    drop(open());
    for serial in ["SAME", "OTHER"] {
        assert!(!replay()
            .fold
            .inventory_unknown(&fold::Scope::Endpoint(endpoint(serial))));
    }
    let path = root.path().join("rebuilt.sqlite");
    let read = || {
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let rows = conn.prepare("select library_serial,generation,body from library_inventory order by library_serial")
            .unwrap().query_map([], |r| Ok((r.get::<_,String>(0)?,r.get::<_,Vec<u8>>(1)?,r.get::<_,Vec<u8>>(2)?)))
            .unwrap().collect::<Result<Vec<_>,_>>().unwrap();
        let open: u64 = conn
            .query_row(
                "select count(*) from library_motion where state in ('open','unknown')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(open, 0);
        rows
    };
    let mut first = None;
    for _ in 0..2 {
        let mut sql = projector::open(&path).unwrap().write_handle();
        for _ in 0..2 {
            projector::thread::rebuild_library(
                &mut sql,
                &dir,
                [0x72; 16],
                &View,
                ApplyPath::Rebuild,
            )
            .unwrap();
        }
        let rows = read();
        assert_eq!(
            rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
            vec!["library:SAME", "standalone:OTHER", "standalone:SAME"]
        );
        assert_eq!(rows[2].1, 2_u64.to_be_bytes());
        if let Some(expected) = &first {
            assert_eq!(&rows, expected);
        } else {
            first = Some(rows);
        }
        drop(sql);
        for suffix in ["", "-wal", "-shm"] {
            let file = std::path::PathBuf::from(format!("{}{suffix}", path.display()));
            if file.exists() {
                std::fs::remove_file(file).unwrap();
            }
        }
    }
}
