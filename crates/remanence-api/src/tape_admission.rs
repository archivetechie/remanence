//! Shared tape-admission payloads and counted refusals. SQL locates conflicts;
//! the library authority alone supplies their typed cause and contradiction state.
use remanence_state::{
    library_log as lib, library_runtime::RefusalSubject, CatalogIndex, TapeIoFenceRecord,
};

/// Recovery may pass only a nonempty set of this tape's live typed suffix fences.
/// SQL locates tape, barcode and endpoint conflicts; the authority validates each raise.
pub(crate) fn only_recoverable_suffix_fences(
    index: &CatalogIndex,
    runtime: Option<&remanence_state::library_runtime::LibraryRuntime>,
    tape_uuid: &crate::TapeUuid,
    barcode: Option<&str>,
    endpoint: Option<&remanence_state::DriveKey>,
) -> Result<bool, remanence_state::StateError> {
    let Some(runtime) = runtime else {
        return Ok(false);
    };
    let Some(identity) = runtime.tape_state(*tape_uuid) else {
        return Ok(false);
    };
    let mut fences = index.tape_io_admission_conflicts(tape_uuid, barcode)?;
    fences.extend(
        index
            .list_active_tape_io_fences()?
            .into_iter()
            .filter(|f| endpoint.is_some_and(|endpoint| f.endpoint.as_ref() == Some(endpoint))),
    );
    Ok(!fences.is_empty()
        && fences.into_iter().all(|fence| {
            fence
                .quarantine_id
                .strip_prefix("library:")
                .and_then(|n| n.parse::<u64>().ok())
                .and_then(|n| runtime.active_fence(n))
                .is_some_and(|raise| {
                    raise.scope == lib::FenceScope::Tape(identity.tape)
                        && raise.kind == lib::FenceKind::Write
                        && raise.cause == lib::FenceCause::SessionSuffixContradiction
                })
        }))
}

pub(crate) fn typed_cause(
    index: &CatalogIndex,
    fence: &TapeIoFenceRecord,
) -> Option<lib::FenceCause> {
    index
        .library_runtime()?
        .fence_for_quarantine_id(&fence.quarantine_id)
        .map(|raise| raise.cause)
}

/// Stable ordering preserves existing order within each cause rank and in fixtures.
pub(crate) fn order_conflicts(index: &CatalogIndex, conflicts: &mut [TapeIoFenceRecord]) {
    if index.library_runtime().is_none() {
        return;
    }
    conflicts.sort_by_key(|fence| match typed_cause(index, fence) {
        Some(lib::FenceCause::SessionSuffixContradiction) => 0,
        Some(lib::FenceCause::NativeLogBehindProjection) => 1,
        _ => 2,
    });
}

/// A skip has the same payload but does not record a refused request.
pub(crate) fn fence_payload(
    index: &CatalogIndex,
    tape: lib::Id,
    fence: &TapeIoFenceRecord,
    message: String,
    record: bool,
    actor: lib::AuditActor,
) -> String {
    if typed_cause(index, fence) == Some(lib::FenceCause::NativeLogBehindProjection) {
        if record {
            record_code(
                index,
                tape,
                lib::RefusalCode::TapeLogBehindProjection,
                actor,
            );
        }
        format!("TapeLogBehindProjection: {message}")
    } else {
        message
    }
}

pub(crate) fn contradiction_text(tape: lib::Id, action: &str) -> String {
    format!("ContradictedTapeAdmission: {action} refused: the library log excludes records that depend on tape {}'s log; restore the exact log or retire the tape", uuid::Uuid::from_bytes(tape))
}

/// Called only after the caller has chosen to refuse, with no dispatch lease held.
pub(crate) fn record_code(
    index: &CatalogIndex,
    tape: lib::Id,
    code: lib::RefusalCode,
    actor: lib::AuditActor,
) {
    if let Some(runtime) = index.library_runtime() {
        runtime.record_refusal(code, RefusalSubject::Tape(Some(tape)), actor);
    }
}

pub(crate) fn contradicted_payload(
    index: &CatalogIndex,
    tape: lib::Id,
    action: &str,
    actor: lib::AuditActor,
) -> Option<String> {
    if !index.library_runtime()?.contradicted(tape) {
        return None;
    }
    record_code(
        index,
        tape,
        lib::RefusalCode::ContradictedTapeAdmission,
        actor,
    );
    Some(contradiction_text(tape, action))
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use remanence_state::{authority::TapeAuthorityView, StateHandle, TapePoolConfig};
    use std::sync::{Arc, Mutex};
    pub(crate) const T: lib::Id = [0x51; 16];
    pub(crate) const B: lib::Id = [0x52; 16];
    pub(crate) struct Fixture {
        pub root: crate::tests::TypedRoot,
        pub state: Option<StateHandle>,
        pub pool: TapePoolConfig,
        config: remanence_state::RemConfig,
    }
    impl Fixture {
        pub fn new() -> Self {
            // Initialize the StateHandle layout before adding this fixture's pool.
            let mut root = crate::tests::TypedRoot::open_with_tapes(&[([0x53; 16], "SETUP53L9")]);
            let pool = TapePoolConfig {
                id: "admission".into(),
                display_name: None,
                copy_class: None,
                content_class: None,
                scheme: None,
                selection_policy: Default::default(),
                watermark_low: 0.98,
                watermark_high: 1.0,
                capacity_cap_bytes: None,
                block_size_bytes: 262144,
                min_object_size_bytes: 0,
            };
            let mut config = root.config();
            config.tape_pools.push(pool.clone());
            root.release();
            let state =
                StateHandle::open_with_config(root.paths().clone(), config.clone()).unwrap();
            for id in [T, B] {
                state
                    .library_runtime()
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
                                pool: Some("admission".into()),
                                endpoint: None,
                                adoption: None,
                                pool_source: Some(lib::PoolSource::Operator(lib::Unit)),
                                operation: None,
                                replaced_bot: None,
                            },
                            lib::AuditActor::System,
                            lib::SourceLayer::Layer4,
                            v,
                        )
                    })
                    .unwrap();
            }
            Self {
                root,
                state: Some(state),
                pool,
                config,
            }
        }
        pub fn index(&self) -> CatalogIndex {
            self.state.as_ref().unwrap().write_handle()
        }
        /// Restart through the production owner after all caller-owned clones drop.
        pub fn reopen(&mut self) {
            drop(self.state.take());
            self.state = Some(
                StateHandle::open_with_config(self.root.paths().clone(), self.config.clone())
                    .unwrap(),
            );
        }
        /// Configured public ingress with a pool that needs no hardware at admission.
        pub fn api_state(&self) -> crate::ApiState {
            let mut api = crate::ApiState::new_with_config(
                self.state.as_ref().unwrap().projection_client(),
                &self.config,
            )
            .unwrap();
            let (changer, _commands) = tokio::sync::mpsc::channel(1);
            api.drive_pool = Some(crate::write_owner::DrivePool::new_for_library(
                "ADMISSION-LIBRARY",
                changer,
                Default::default(),
                Arc::new(Default::default()),
            ));
            api
        }
        pub fn raise(&self, tape: lib::Id, cause: lib::FenceCause) -> u64 {
            self.state
                .as_ref()
                .unwrap()
                .library_runtime()
                .append(|a, v| {
                    a.fence_raised(
                        lib::FenceRaised {
                            kind: lib::FenceKind::Write,
                            scope: lib::FenceScope::Tape(lib::TapeRef {
                                uuid: tape,
                                incarnation: 1,
                            }),
                            cause,
                            evidence: None,
                            at: None,
                            operation: None,
                            detail: lib::Bounded(vec![]),
                        },
                        lib::AuditActor::System,
                        lib::SourceLayer::Layer4,
                        v,
                    )
                })
                .unwrap()
                .sequence
        }
        /// Exclude a real received dependency on restart, then remove the safety
        /// fence through the test producer to isolate the independent D3 gate.
        pub fn contradict(&mut self, tape: lib::Id) {
            {
                let runtime = self.state.as_ref().unwrap().library_runtime();
                let authority = remanence_state::TapeAuthority::open(
                    self.root.paths().journal_dir.join("tapes"),
                    tape,
                    &self.root.paths().sqlite_path,
                )
                .unwrap();
                let mut owner = authority.acquire_exclusive().unwrap();
                owner
                    .publish_initialized(
                        262144,
                        None,
                        remanence_state::tape_log::CommitCursor {
                            partition: 0,
                            lba: 2,
                        },
                    )
                    .unwrap();
                let snapshot = owner.freeze().unwrap();
                runtime.refresh_native(&snapshot).unwrap();
                let at = runtime.native_reference(&snapshot).unwrap();
                runtime
                    .append(|a, v| {
                        a.verification_verdict(
                            lib::VerificationVerdict {
                                tape: at.tape,
                                at: Some(at),
                                outcome: lib::VerificationOutcome::VerifiedComplete,
                                operation: [0x61; 16],
                                damaged: lib::Bounded(vec![]),
                                unprotected: None,
                                modes: 1,
                                edition_digest: None,
                                progress: None,
                            },
                            lib::AuditActor::System,
                            lib::SourceLayer::Layer4,
                            v,
                        )
                    })
                    .unwrap();
            }
            drop(self.state.take());
            std::fs::remove_file(
                self.root
                    .paths()
                    .journal_dir
                    .join("tapes")
                    .join(format!("{}.remtape", uuid::Uuid::from_bytes(tape).simple())),
            )
            .unwrap();
            self.state = Some(
                StateHandle::open_with_config(self.root.paths().clone(), self.config.clone())
                    .unwrap(),
            );
            let runtime = self.state.as_ref().unwrap().library_runtime();
            for fence in self
                .index()
                .tape_io_admission_conflicts(&tape, None)
                .unwrap()
            {
                let n = fence
                    .quarantine_id
                    .strip_prefix("library:")
                    .unwrap()
                    .parse()
                    .unwrap();
                runtime
                    .append(|a, v| {
                        a.fence_released(
                            lib::FenceReleased {
                                fence: n,
                                evidence: "fixture isolates D3".into(),
                                forced: false,
                                detail: lib::Bounded(vec![]),
                            },
                            lib::AuditActor::System,
                            lib::SourceLayer::Layer4,
                            v,
                        )
                    })
                    .unwrap();
            }
            assert!(runtime.contradicted(tape));
            assert!(self.index().tape_contradicted(&tape).unwrap());
            assert!(self
                .index()
                .tape_io_admission_conflicts(&tape, None)
                .unwrap()
                .is_empty());
        }
        /// Call after dropping all caller-owned catalog/runtime/config clones.
        pub fn refusals(&mut self) -> Vec<lib::Diagnostic> {
            let uuid = self
                .state
                .as_ref()
                .unwrap()
                .library_runtime()
                .manifest()
                .library_uuid();
            drop(self.state.take());
            let paths = self.root.paths();
            let view = lib::native_view::NativeTapeView::probe(
                &paths.journal_dir.join("tapes"),
                &paths.sqlite_path,
            )
            .unwrap();
            let mut records = Vec::new();
            lib::store::replay_observed(&paths.audit_dir, uuid, &view, &mut |applied| {
                if let Some(d) = applied.record().diagnostic() {
                    if d.event == lib::DiagnosticEvent::RefusalRecorded {
                        records.push(d.clone());
                    }
                }
                Ok(())
            })
            .unwrap();
            records
        }
        pub fn config(
            &self,
            drive: &remanence_library::DriveHandle,
        ) -> crate::write_owner::WriteOwnerConfig {
            let state = self.state.as_ref().unwrap();
            let paths = self.root.paths();
            crate::write_owner::WriteOwnerConfig {
                authority: crate::write_owner::TerminalAuthority::Typed(state.library_runtime()),
                catalog: state.projection_client(),
                index_path: paths.sqlite_path.clone(),
                report: remanence_library::DiscoveryReport {
                    libraries: vec![],
                    unattached_drives: vec![],
                    warnings: vec![],
                    complete: true,
                    incomplete_evidence: vec![],
                },
                policy: remanence_library::StaticAllowlist::new(["ADMISSION-DRIVE"]),
                reservations: Arc::new(Default::default()),
                placement: crate::write_owner::ActorPlacement::Mounted(
                    crate::write_owner::MountedDriveContext {
                        endpoint: drive.endpoint().durable_key().unwrap().clone(),
                        tape_uuid: T,
                        barcode: None,
                        cartridge_generation: Some(crate::LtoGen::Lto9),
                        identity_generation: drive.identity_generation(),
                        actor: lib::AuditActor::System,
                        audit_error: Arc::new(Mutex::new(None)),
                        audit_session: Arc::new(Mutex::new(None)),
                        direct_replay_fault: None,
                    },
                ),
                snapshot_miss_alarm: 1,
                managed_library_serials: Arc::new(Default::default()),
                cleaning: Default::default(),
                tape_io: Default::default(),
                tape_pools: vec![self.pool.clone()],
                verification_route: remanence_parity::VerificationRoute::Walk,
                io_memory: crate::io_memory::IoMemoryReservation::new(64 * 1024 * 1024).unwrap(),
                write_admissions: Default::default(),
                checkpoint_journal_dir: paths.journal_dir.join("tapes"),
                checkpoint_max_bytes: remanence_state::DEFAULT_CHECKPOINT_MAX_BYTES,
                checkpoint_max_objects: remanence_state::DEFAULT_CHECKPOINT_MAX_OBJECTS,
                checkpoint_max_age_seconds: remanence_state::DEFAULT_CHECKPOINT_MAX_AGE_SECONDS,
                session_idle_seconds: 1800,
                lifecycle: None,
                changer_ready_wait: Default::default(),
                calibration_store: remanence_state::CalibrationControlStore::open(
                    &paths.calibration_dir,
                )
                .unwrap(),
            }
        }
    }
    pub(crate) fn drive() -> remanence_library::DriveHandle {
        let serial = b"ADMISSION-DRIVE";
        let mut vpd = vec![1, 0x80, 0, serial.len() as u8];
        vpd.extend_from_slice(serial);
        let transport = remanence_library::FixtureTransport::new().with_responses(vec![
            include_bytes!("../../../fixtures/inquiry/drive1-lto9.bin").to_vec(),
            vpd,
        ]);
        remanence_library::DriveHandle::open_standalone_with_transport(
            std::path::Path::new("/fixture/admission-drive"),
            Box::new(transport),
            &remanence_library::StaticAllowlist::new(["ADMISSION-DRIVE"]),
        )
        .unwrap()
    }
    pub(crate) fn assert_code(d: &lib::Diagnostic, tape: lib::Id, code: u64) {
        assert_eq!(d.subject_kind.as_str(), "tape");
        assert_eq!(
            d.subject_id.as_ref().unwrap().as_str(),
            uuid::Uuid::from_bytes(tape).to_string()
        );
        assert_eq!(
            d.detail,
            lib::Bounded(vec![
                lib::DiagnosticEntry("code".into(), lib::DiagnosticValue::Unsigned(code)),
                lib::DiagnosticEntry(
                    "name".into(),
                    lib::DiagnosticValue::Text(
                        if code == 1 {
                            "TapeLogBehindProjection"
                        } else {
                            "ContradictedTapeAdmission"
                        }
                        .into()
                    )
                )
            ])
        );
    }
}

#[cfg(test)]
mod tests {
    //! Public daemon ingress must use the owner's typed refusal authority.
    use super::{lib, test_support::*};
    use crate::pb;
    use pb::library_service_server::LibraryService;
    use pb::write_session_service_server::WriteSessionService;
    use remanence_state::authority::TapeAuthorityView;
    use tonic::{Code, Request};

    /// Recovery checks every typed cause and fails closed without live identity.
    #[test]
    fn c0b3c_suffix_predicate_cause_matrix() {
        use lib::FenceCause::{
            NativeLogBehindProjection, SessionSuffixContradiction, TransferError,
        };
        let cases = [
            vec![],
            vec![SessionSuffixContradiction],
            vec![SessionSuffixContradiction, TransferError],
            vec![TransferError],
            vec![NativeLogBehindProjection],
        ];
        let mut actual = Vec::new();
        for causes in cases {
            let fixture = Fixture::new();
            let runtime = fixture.state.as_ref().unwrap().library_runtime();
            for cause in causes {
                fixture.raise(T, cause);
            }
            let index = fixture.index();
            let endpoint = remanence_state::DriveKey::new("SUFFIX", 0x100);
            actual.push(
                super::only_recoverable_suffix_fences(
                    &index,
                    Some(&runtime),
                    &T,
                    None,
                    Some(&endpoint),
                )
                .unwrap(),
            );
            assert!(!super::only_recoverable_suffix_fences(
                &index,
                None,
                &T,
                None,
                Some(&endpoint)
            )
            .unwrap());
            assert!(!super::only_recoverable_suffix_fences(
                &index,
                Some(&runtime),
                &[0xff; 16],
                None,
                Some(&endpoint)
            )
            .unwrap());
        }
        assert_eq!(actual, [false, true, false, false, false]);
    }

    /// Endpoint conflicts participate even when their tape and barcode do not match.
    #[test]
    fn c0b3c_suffix_predicate_includes_other_tape_endpoint_fences() {
        let mut actual = Vec::new();
        for cause in [
            lib::FenceCause::TransferError,
            lib::FenceCause::SessionSuffixContradiction,
        ] {
            let fixture = Fixture::new();
            let runtime = fixture.state.as_ref().unwrap().library_runtime();
            fixture.raise(T, lib::FenceCause::SessionSuffixContradiction);
            let other = fixture.raise(B, cause);
            let endpoint = remanence_state::DriveKey::new("SUFFIX", 0x100);
            // Typed tape raises have no endpoint column. Attach the legacy locator
            // to this real typed raise to exercise the union with endpoint conflicts.
            let conn = rusqlite::Connection::open(&fixture.root.paths().sqlite_path).unwrap();
            assert_eq!(
                conn.execute(
                    "UPDATE tape_io_fences SET endpoint_key = ?1 WHERE quarantine_id = ?2",
                    rusqlite::params![endpoint.encode(), format!("library:{other}")]
                )
                .unwrap(),
                1
            );
            let index = fixture.index();
            assert_eq!(
                index.tape_io_admission_conflicts(&T, None).unwrap().len(),
                1
            );
            assert!(
                super::only_recoverable_suffix_fences(&index, Some(&runtime), &T, None, None)
                    .unwrap()
            );
            actual.push(
                super::only_recoverable_suffix_fences(
                    &index,
                    Some(&runtime),
                    &T,
                    None,
                    Some(&endpoint),
                )
                .unwrap(),
            );
        }
        assert_eq!(actual, [false, false]);
    }

    /// Reach admission through the same TapeTarget RPC as a daemon client.
    async fn open_tape(api: &crate::ApiState, tape: lib::Id) -> tonic::Status {
        api.write_session_service()
            .open_write_session(Request::new(pb::OpenWriteSessionRequest {
                target: Some(pb::open_write_session_request::Target::TapeTarget(
                    pb::TapeTarget {
                        tape_uuid: tape.to_vec(),
                        mount_if_needed: true,
                        required_pool_id: "admission".into(),
                        allow_unpooled: false,
                    },
                )),
                body_format: Some("rem-object-v1".into()),
                idempotency_key: None,
                recover_session_id: None,
            }))
            .await
            .expect_err("admission must refuse before mounting")
    }

    /// A forced release remembers restoration, but cannot authorize a later loss.
    #[tokio::test]
    async fn deleted_log_after_forced_release_is_refused_and_recorded() {
        let mut f = Fixture::new();
        let path = f
            .root
            .paths()
            .journal_dir
            .join("tapes")
            .join(format!("{}.remtape", uuid::Uuid::from_bytes(T).simple()));
        let short = {
            let runtime = f.state.as_ref().unwrap().library_runtime();
            let authority = remanence_state::TapeAuthority::open(
                f.root.paths().journal_dir.join("tapes"),
                T,
                &f.root.paths().sqlite_path,
            )
            .unwrap();
            let mut owner = authority.acquire_exclusive().unwrap();
            owner
                .publish_initialized(
                    262144,
                    None,
                    remanence_state::tape_log::CommitCursor {
                        partition: 0,
                        lba: 2,
                    },
                )
                .unwrap();
            let short = std::fs::read(&path).unwrap();
            owner
                .open_session(
                    T,
                    "2026-11-11T00:00:00Z".into(),
                    remanence_state::tape_log::SessionDrive {
                        serial: "DRV-TEST".into(),
                        vendor: None,
                        product: None,
                        endpoint: remanence_state::library_log::DriveKey::Standalone(
                            remanence_state::library_log::Standalone {
                                drive_serial: "DRV-TEST".into(),
                            },
                        ),
                        drive_uuid: None,
                    },
                )
                .unwrap();
            owner
                .end_session(
                    T,
                    "2026-11-11T00:00:00Z".into(),
                    remanence_state::tape_log::SessionEndReason::CleanClose,
                    None,
                    std::iter::empty(),
                )
                .unwrap();
            runtime.refresh_native(&owner.freeze().unwrap()).unwrap();
            runtime.catch_up().unwrap();
            short
        };
        let full = std::fs::read(&path).unwrap();
        drop(f.state.take());
        std::fs::write(&path, short).unwrap();
        f.reopen();
        let first = f.index().tape_io_admission_conflicts(&T, None).unwrap();
        assert_eq!(first.len(), 1);
        let first_id = first[0].quarantine_id.clone();
        drop(f.state.take());
        std::fs::write(&path, full).unwrap();
        f.reopen();
        let api = f.api_state();
        let released = api
            .library_service()
            .release_tape_io_fence(Request::new(pb::ReleaseTapeIoFenceRequest {
                quarantine_id: first_id.clone(),
                acknowledgment: "operator restored exact log".into(),
                force: true,
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(released.state, "released");
        drop(api);
        drop(f.state.take());
        std::fs::remove_file(&path).unwrap();
        f.reopen();
        let api = f.api_state();
        let conflicts = f.index().tape_io_admission_conflicts(&T, None).unwrap();
        assert_eq!(conflicts.len(), 1, "deletion must raise the second fence");
        let fence = &conflicts[0];
        assert_ne!(fence.quarantine_id, first_id);
        assert_eq!(fence.state, "active");
        assert_eq!(
            f.state
                .as_ref()
                .unwrap()
                .library_runtime()
                .fence_for_quarantine_id(&fence.quarantine_id)
                .unwrap()
                .cause,
            lib::FenceCause::NativeLogBehindProjection
        );
        drop(api);
        let tape_id = uuid::Uuid::from_bytes(T).to_string();
        let before = f
            .refusals()
            .into_iter()
            .filter(|d| d.subject_id.as_ref().is_some_and(|s| s.as_str() == tape_id))
            .count();
        f.reopen();
        let api = f.api_state();
        let error = open_tape(&api, T).await;
        assert_eq!(error.code(), Code::FailedPrecondition);
        assert!(error.message().contains(&fence.quarantine_id), "{error}");
        assert!(api
            .drive_pool
            .as_ref()
            .unwrap()
            .sessions_by_drive()
            .values()
            .all(|(_, session)| session.tape_uuid != T));
        drop(api);
        let records: Vec<_> = f
            .refusals()
            .into_iter()
            .filter(|d| d.subject_id.as_ref().is_some_and(|s| s.as_str() == tape_id))
            .collect();
        assert_eq!(
            (
                error.message().contains("TapeLogBehindProjection: "),
                records.len()
            ),
            (true, before + 1),
            "prefix and refusal count: {error}"
        );
        assert_code(records.last().unwrap(), T, 1);
    }

    /// An unfenced contradicted tape keeps its refusal and records code 2.
    #[tokio::test]
    async fn contradicted_tape_ingress_is_refused_and_recorded() {
        let mut f = Fixture::new();
        f.contradict(B);
        let api = f.api_state();
        let error = open_tape(&api, B).await;
        assert_eq!(error.code(), Code::FailedPrecondition);
        assert_eq!(
            error.message(),
            format!(
                "tape {} is not writable: {}",
                uuid::Uuid::from_bytes(B),
                crate::WritabilityError::ContradictedTape
            )
        );
        drop(api);
        let records = f.refusals();
        assert_eq!(records.len(), 1);
        assert_code(&records[0], B, 2);
    }
}
