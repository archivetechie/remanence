//! Real source-pair replay tests for the native crash-acknowledgement join.
use super::*;
use crate::authority::{TapeAuthorityView, TapeAuthorityWriter};
use sha2::{Digest, Sha256};

#[test]
fn native_crash_ack_checks_type_tape_session_and_preview_before_projection() {
    for fault in [
        "valid-empty",
        "valid-file",
        "valid-reused",
        "kind",
        "tape",
        "session",
        "preview",
        "missing",
    ] {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let paths = StatePaths {
            config_path: root.join("config"),
            state_dir: root.join("state"),
            audit_dir: root.join("library"),
            journal_dir: root.join("native"),
            sqlite_path: root.join("catalog.sqlite"),
            tape_cache_dir: root.join("cache"),
            calibration_dir: root.join("calibration"),
        };
        let lease = Arc::new(StateLockGuard::acquire(&paths.state_dir).unwrap());
        let runtime = LibraryRuntime::open(&paths, true, lease.clone()).unwrap();
        let mut owners = Vec::new();
        let mut provision_sequence = 0;
        for id in [[0x41; 16], [0x42; 16]] {
            provision_sequence = runtime
                .append(|a, v| {
                    a.reprovision(
                        lib::Reprovision {
                            tape: lib::TapeRef {
                                uuid: id,
                                incarnation: 1,
                            },
                            predecessor: None,
                            mode: lib::ProvisionMode::Initialize,
                            label: None,
                            label_provenance: None,
                            parity_scheme: None,
                            medium: lib::MediumSpec {
                                family: lib::MediumFamily::Lto(lib::LtoGen::Lto9),
                                usable_logical_bytes: 18_000_000_000_000,
                                record_size: 262144,
                                rerecord: true,
                                lifecycle_ops: true,
                                source: lib::GenerationSource::Operator,
                            },
                            pool: None,
                            endpoint: None,
                            adoption: None,
                        },
                        lib::AuditActor::System,
                        lib::SourceLayer::Layer4,
                        v,
                    )
                })
                .unwrap()
                .sequence;
            let mut owner = crate::TapeAuthority::open_fixture(paths.journal_dir.join("tapes"), id)
                .unwrap()
                .acquire_exclusive()
                .unwrap();
            owner
                .publish_report(&(&crate::checkpoint::tests::record(id)).into())
                .unwrap();
            runtime.refresh_native(&owner.freeze().unwrap()).unwrap();
            owners.push(owner);
        }
        let session = owners[0]
            .freeze()
            .unwrap()
            .tape_summary()
            .open_session
            .as_ref()
            .unwrap()
            .session_id;
        let tails = if matches!(fault, "valid-file" | "valid-reused") {
            vec![crate::tape_log::TailFile {
                tape_file_number: 2,
                identity: crate::tape_log::TailIdentity::Object("observed-object".into()),
                known_blocks: 1,
                unknown_blocks: 0,
                filemark: crate::tape_log::TailFilemark::Present,
                synchronized_blocks: 1,
                synchronized_filemark: true,
            }]
        } else {
            vec![]
        };
        let mut bytes = Vec::new();
        for tail in &tails {
            lib::session_ack::write_preview_file(&mut bytes, tail).unwrap();
        }
        let digest = Sha256::digest(&bytes).into();
        let at = runtime
            .native_reference(&owners[usize::from(fault == "tape")].freeze().unwrap())
            .unwrap();
        let ack = runtime
            .append(|a, v| {
                a.supersede_tail(
                    lib::SupersedeTail {
                        at,
                        operation: None,
                        session_id: Some(if fault == "session" {
                            [0x52; 16]
                        } else {
                            session
                        }),
                        preview_digest: Some(if fault == "preview" {
                            [0xa5; 32]
                        } else {
                            digest
                        }),
                        tail_file_count: tails.len() as u64,
                    },
                    lib::AuditActor::System,
                    lib::SourceLayer::Layer5,
                    v,
                )
            })
            .unwrap();
        let earlier = owners[0].freeze().unwrap();
        let sequence = match fault {
            "kind" => provision_sequence,
            "missing" => ack.sequence + 99,
            _ => ack.sequence,
        };
        if fault == "valid-empty" {
            let mut index = crate::projector::open(&paths.sqlite_path)
                .unwrap()
                .write_handle();
            runtime
                .project_native(&mut index, &owners[0].freeze().unwrap(), ApplyPath::Live)
                .unwrap();
            runtime.0.ack_frames_checked.store(0, Ordering::Relaxed);
            runtime
                .project_native(&mut index, &owners[0].freeze().unwrap(), ApplyPath::Live)
                .unwrap();
            assert_eq!(
                runtime.0.ack_frames_checked.load(Ordering::Relaxed),
                0,
                "unchanged prefix must read no frames"
            );
        }
        owners[0]
            .end_session(
                session,
                "2026-11-06T00:01:00Z".into(),
                crate::tape_log::SessionEndReason::CrashRecovered,
                Some(sequence),
                tails.clone().into_iter().map(Ok),
            )
            .unwrap();
        runtime
            .refresh_native(&owners[0].freeze().unwrap())
            .unwrap();
        if fault == "valid-empty" {
            let mut index = crate::projector::open(&paths.sqlite_path)
                .unwrap()
                .write_handle();
            runtime
                .project_native(&mut index, &owners[0].freeze().unwrap(), ApplyPath::Live)
                .unwrap();
            assert_eq!(
                runtime.0.ack_frames_checked.load(Ordering::Relaxed),
                1,
                "only the new ending, never the other tape's history, is visited"
            );
            runtime
                .project_native(&mut index, &earlier, ApplyPath::Live)
                .unwrap();
            assert_eq!(runtime.0.ack_frames_checked.load(Ordering::Relaxed), 1);
            assert!(!runtime.motion_fenced());
        }
        let valid = fault.starts_with("valid");
        let result = runtime.catch_up();
        assert_eq!(result.is_ok(), valid, "{fault}: {result:?}");
        if matches!(fault, "valid-file" | "valid-reused") {
            owners[0].record_session_tail_eod(8).unwrap();
            let mut index = crate::projector::open(&paths.sqlite_path)
                .unwrap()
                .write_handle();
            runtime
                .project_native(&mut index, &owners[0].freeze().unwrap(), ApplyPath::Live)
                .unwrap();
            if fault == "valid-reused" {
                owners[0]
                    .open_session(session, "2026-11-06T00:02:00Z".into())
                    .unwrap();
                owners[0]
                    .end_session(
                        session,
                        "2026-11-06T00:03:00Z".into(),
                        crate::tape_log::SessionEndReason::CrashRecovered,
                        Some(sequence),
                        tails.into_iter().map(Ok),
                    )
                    .unwrap();
                runtime
                    .project_native(&mut index, &owners[0].freeze().unwrap(), ApplyPath::Live)
                    .unwrap();
            }
            runtime.catch_up().unwrap();
        }
        drop(owners);
        drop(runtime);
        let reopened = LibraryRuntime::open(&paths, true, lease);
        assert_eq!(reopened.is_ok(), valid, "reopen {fault}: {reopened:?}");
    }
}

/// The public repair entry point enforces received dependency floors even when
/// SQL is empty, then publishes a genuinely shorter frozen native prefix.
#[test]
fn native_repair_uses_received_reference_floor_and_replaces_cached_prefix() {
    for referenced in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.toml");
        std::fs::write(
            &config,
            format!(
                r#"[daemon]
state_dir="{0}"
default_idle_timeout_seconds=1800
[journal]
dir="{0}/journals"
require_trusted_volume=false
[audit]
dir="{0}/audit"
[index]
sqlite_path="{0}/index.sqlite"
[cache]
tape_catalog_dir="{0}/cache"
"#,
                root.path().display()
            ),
        )
        .unwrap();
        let mut state = crate::StateHandle::open_from_config_file(&config).unwrap();
        let id = [0x71; 16];
        state
            .provision_tape_with_media(
                crate::ProvisionTapeInput {
                    tape_uuid: id,
                    voltag: "ACM901L9".into(),
                    block_size: 262144,
                    parity: remanence_parity::ParityConfig::None,
                    force: false,
                },
                crate::LabelProvenance::Operator,
                crate::MediaObservation::label_hint("ACM901L9"),
            )
            .unwrap();
        let runtime = state.catalog().library_runtime().unwrap();
        let native =
            crate::TapeAuthority::open_fixture(state.paths().journal_dir.join("tapes"), id)
                .unwrap();
        let mut owner = native.acquire_exclusive().unwrap();
        owner
            .publish_report(&(&crate::checkpoint::tests::record(id)).into())
            .unwrap();
        let frozen = owner.freeze().unwrap();
        runtime.refresh_native(&frozen).unwrap();
        let at = runtime.native_reference(&frozen).unwrap();
        assert!(at.sequence > 1);
        if referenced {
            runtime
                .append(|a, v| {
                    a.supersede_tail(
                        lib::SupersedeTail {
                            at: at.clone(),
                            operation: Some([0xe7; 16]),
                            session_id: None,
                            preview_digest: None,
                            tail_file_count: 0,
                        },
                        lib::AuditActor::System,
                        lib::SourceLayer::Layer5,
                        v,
                    )
                })
                .unwrap();
            struct NotYet;
            impl lib::TapeView for NotYet {
                fn lookup(&self, _: &lib::TapeAt) -> Result<lib::TapeLookup, lib::FoldError> {
                    Ok(lib::TapeLookup::NotYet)
                }
            }
            let replay = runtime
                .0
                .authority
                .snapshot()
                .unwrap()
                .replay(&NotYet, &mut |_| Ok(()))
                .unwrap();
            assert!(!replay.fold.pending.is_empty());
            assert_eq!(
                replay.fold.highest_reference(at.tape),
                Some(at.sequence),
                "the received floor also survives a pending disposition"
            );
        }
        drop(owner);
        drop(native);
        let path = state
            .paths()
            .journal_dir
            .join("tapes")
            .join(format!("{}.remtape", uuid::Uuid::from_bytes(id).simple()));
        if !referenced {
            use std::io::{Seek, Write};
            let mut offset = None;
            frozen
                .raw
                .visit_frames(|frame| {
                    if frame.sequence == 2 {
                        offset = Some(frame.offset);
                    }
                    Ok(())
                })
                .unwrap();
            let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.seek(std::io::SeekFrom::Start(offset.unwrap()))
                .unwrap();
            file.write_all(&[0xff]).unwrap();
            file.sync_all().unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        let result = state.repair_tape_log_tail(id, 1, true);
        if referenced {
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("RepairBelowReference"));
            assert_eq!(std::fs::read(&path).unwrap(), before);
        } else {
            result.unwrap();
            assert_eq!(
                runtime
                    .current_native_reference(id)
                    .unwrap()
                    .unwrap()
                    .sequence,
                1
            );
            assert!(runtime.refresh_native(&frozen).is_err());
            let mut index = state.write_handle();
            assert!(runtime
                .project_native(&mut index, &frozen, ApplyPath::Live)
                .unwrap_err()
                .to_string()
                .contains("replaced repair source"));
            state.rebuild_projection_from_tape_logs().unwrap();
            assert_eq!(
                runtime
                    .current_native_reference(id)
                    .unwrap()
                    .unwrap()
                    .sequence,
                1
            );
        }
    }
}
