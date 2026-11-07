//! Explicit tail recovery shared by the mounted and library drive actors.
use remanence_library::{DriveHandle, DriveHandleSource};
use remanence_state::authority::TapeAuthorityView;
use remanence_state::{AuditActor, SourceLayer};

/// Explicit recovery shared by standalone ownership and the library drive actor.
/// Confirmation, rescan, source publication, and mutation all retain the same exclusive lease.
#[allow(clippy::too_many_arguments)]
pub(crate) fn supersede_mounted_tail(
    index: &mut remanence_state::CatalogIndex,
    cfg: &crate::write_owner::TerminalWriteConfig,
    drive: &mut DriveHandle,
    tape_uuid: [u8; 16],
    identity_generation: u64,
    acknowledge: &str,
    actor: AuditActor,
    confirm: impl FnOnce(&str, &[String], u64) -> Result<(), String>,
) -> Result<Vec<String>, tonic::Status> {
    use crate::append_admission::{admit_append_boundary, context, AppendAuthority};
    use tonic::Status;
    let failure = |e: String| Status::failed_precondition(e);
    let library = cfg
        .authority
        .runtime()
        .ok_or_else(|| failure("shared library authority is required for tail recovery".into()))?;
    library
        .ensure_motion_allowed()
        .map_err(|e| failure(e.to_string()))?;
    library.catch_up().map_err(|e| failure(e.to_string()))?;
    let tape = index
        .get_tape(&tape_uuid)
        .map_err(crate::status_from_state_error)?
        .ok_or_else(|| failure("adopt the tape before superseding its tail".into()))?;
    if tape.cartridge_generation.is_none() {
        return Err(failure("tape generation is required".into()));
    }
    if tape.terminal_finalization.is_some() {
        return Err(failure("terminal finalization prevents tail discard; retry FinalizeTape with its original idempotency key. If rolled-back state cannot resume that intent, restore a newer copy of the state directory; supersede-tail cannot clear a stale terminal intent".into()));
    }
    if tape.kind != "data"
        || !matches!(tape.state.as_str(), "ready" | "recovery_required")
        || tape.terminal_finalization.is_some()
    {
        return Err(failure(
            "tape is sealed, retired, or requires terminal recovery".into(),
        ));
    }
    use remanence_state::library_log as lib;
    let identity = library
        .tape_state(tape_uuid)
        .ok_or_else(|| failure("tail recovery requires Reprovision authority".into()))?;
    let conflicts = index
        .tape_io_admission_conflicts(&tape_uuid, tape.voltag.as_deref())
        .map_err(crate::status_from_state_error)?;
    let mut suffix_fences = Vec::new();
    for fence in conflicts {
        // The projection supplies only a locator; the fold validates current
        // liveness, tape identity, kind and cause before any bypass or release.
        let sequence = fence
            .quarantine_id
            .strip_prefix("library:")
            .and_then(|n| n.parse::<u64>().ok());
        let allowed = sequence
            .and_then(|n| library.active_fence(n))
            .is_some_and(|raise| {
                raise.kind == lib::FenceKind::Write
                    && raise.scope == lib::FenceScope::Tape(identity.tape)
                    && raise.cause == lib::FenceCause::SessionSuffixContradiction
            });
        if !allowed {
            return Err(failure(
                "release the unrelated tape I/O fence before superseding".into(),
            ));
        }
        suffix_fences.push(sequence.unwrap());
    }
    let uuid = uuid::Uuid::from_bytes(tape_uuid).to_string();
    let label = tape.voltag.as_deref().unwrap_or(&uuid);
    if acknowledge != uuid && acknowledge != label {
        return Err(failure(
            "acknowledgment does not identify the loaded tape".into(),
        ));
    }
    let (size, parity) = crate::pool_write::selected_tape_geometry(
        &tape,
        tape.pool_id.as_deref().unwrap_or("(unpooled)"),
    )
    .map_err(|e| failure(e.to_string()))?;
    let journal =
        remanence_state::TapeAuthority::open(&cfg.checkpoint_journal_dir, tape_uuid, index.path())
            .map_err(crate::status_from_state_error)?;
    let terminal_guidance = "retry FinalizeTape with its original idempotency key. If rolled-back state cannot resume that intent, restore a newer copy of the state directory; supersede-tail cannot clear a stale terminal intent";
    if journal
        .pending_finalization()
        .map_err(|e| {
            failure(format!(
                "cannot establish terminal authority: {e}; {terminal_guidance}"
            ))
        })?
        .is_some()
    {
        return Err(failure(format!(
            "terminal finalization intent prevents tail discard; {terminal_guidance}"
        )));
    }
    // The log's own seal refuses too: SQLite may not have projected it yet,
    // and pending_finalization reports nothing for a sealed log.
    let sealed = "tape log is sealed; supersede-tail cannot make a sealed tape writable";
    if journal
        .authority_summary()
        .map_err(crate::status_from_state_error)?
        .sealed
    {
        return Err(failure(sealed.into()));
    }
    let mut lease = journal
        .acquire_for_tail_reconciliation()
        .map_err(crate::status_from_state_error)?;
    lease
        .set_projection_floor(
            index
                .projected_through_sequence(tape_uuid)
                .map_err(crate::status_from_state_error)?,
        )
        .map_err(crate::status_from_state_error)?;
    let repaired_tail = lease
        .repair_pending()
        .map_err(crate::status_from_state_error)?;
    if lease
        .pending_finalization()
        .map_err(crate::status_from_state_error)?
        .is_some()
    {
        return Err(failure(
            "terminal finalization intent prevents tail discard; retry FinalizeTape with its original idempotency key. If rolled-back state cannot resume that intent, restore a newer copy of the state directory; supersede-tail cannot clear a stale terminal intent".into(),
        ));
    }
    if lease
        .authority_summary()
        .map_err(crate::status_from_state_error)?
        .sealed
    {
        return Err(failure(sealed.into()));
    }
    let last = lease
        .latest_commit()
        .map_err(crate::status_from_state_error)?;
    let snapshot = lease.freeze().map_err(crate::status_from_state_error)?;
    let sequence = snapshot.log_reference().0;
    let make_context = || {
        let mut ctx = context(
            tape_uuid,
            size,
            last.as_ref().map_or(0, |r| r.cursor.partition),
            last.as_ref().map_or(0, |r| r.cursor.lba),
        );
        ctx.inspect_all = true;
        ctx
    };
    let authority = || {
        last.as_ref()
            .map_or(AppendAuthority::Fresh, |_| AppendAuthority::Session {
                snapshot: &snapshot,
                closeout: None,
            })
    };
    drive
        .bind_identity_generation(identity_generation)
        .map_err(|e| failure(e.to_string()))?;
    drive.rewind().map_err(|e| failure(e.to_string()))?;
    crate::verify_tape_identity(&mut DriveHandleSource(drive), &tape_uuid)
        .map_err(|e| failure(e.to_string()))?;
    let config = drive.read_config().map_err(|e| failure(e.to_string()))?;
    crate::pool_write::require_rewritable_object_media(config)
        .map_err(|e| failure(e.to_string()))?;
    crate::drive_mode::configure_fixed_uncompressed_write(drive, config, size)
        .map_err(|e| failure(e.to_string()))?;
    let prior_empty_recovery = library
        .empty_prefix_supersede(&snapshot)
        .map_err(|e| failure(e.to_string()))?
        .is_some_and(|ack| {
            ack.session_id.is_none() && ack.preview_digest.is_none() && ack.tail_file_count == 0
        });
    let (found, missing_filemark, already_recovered) =
        match admit_append_boundary(drive, authority(), make_context()) {
            Err(crate::append_admission::AdmissionError {
                kind:
                    crate::append_admission::AdmissionErrorKind::TailBeyondKnownState { found, .. },
                ..
            }) => (found, false, false),
            Err(crate::append_admission::AdmissionError {
                kind: crate::append_admission::AdmissionErrorKind::MissingBootstrapFilemark,
                ..
            }) => (Vec::new(), true, false),
            Err(e) => return Err(failure(e.to_string())),
            Ok(_) if repaired_tail => (Vec::new(), false, false),
            Ok(_) if !suffix_fences.is_empty() || prior_empty_recovery => (Vec::new(), false, true),
            Ok(_) => return Err(failure("no foreign tail requires superseding".into())),
        };
    confirm(&format!("SUPERSEDE {label}"), &found, sequence).map_err(failure)?;
    let mut ctx = make_context();
    ctx.confirmed_discard = Some(found.clone());
    ctx.allow_missing_bootstrap_filemark = missing_filemark;
    let mut admission =
        admit_append_boundary(drive, authority(), ctx).map_err(|e| failure(e.to_string()))?;
    drive
        .bind_identity_generation(identity_generation)
        .map_err(|e| failure(e.to_string()))?;
    if already_recovered {
        // Crash after a successful physical repair but before release: the
        // operator repeats acknowledgement and the physical proof above. No
        // second discard/SessionEnded is issued; only the still-active raises
        // are released through their original source identities.
        drive.clear_append_admission();
        if last.is_none() {
            // Admission restored BOT. Re-read the exact healthy Bootstrap and
            // delimiter, then publish their observed cursor before release.
            // This completes a post-effect crash without repeating any write.
            drive.rewind().map_err(|e| failure(e.to_string()))?;
            let mut block = vec![0; size as usize];
            let read = drive
                .read_block(&mut block)
                .map_err(|e| failure(e.to_string()))?;
            let bootstrap = remanence_parity::bootstrap::parse_bootstrap_block(&block[..read])
                .map_err(|e| failure(e.to_string()))?;
            let parity_matches = match (&parity, &bootstrap.scheme) {
                (remanence_parity::ParityConfig::None, None) => bootstrap.no_parity_flag,
                (remanence_parity::ParityConfig::Scheme(expected), Some(actual)) => {
                    !bootstrap.no_parity_flag
                        && !actual.no_parity_flag
                        && actual.id == expected.id.as_str()
                        && actual.data_blocks_per_stripe == expected.data_blocks_per_stripe
                        && actual.parity_blocks_per_stripe == expected.parity_blocks_per_stripe
                        && actual.stripes_per_neighborhood == expected.stripes_per_neighborhood
                }
                _ => false,
            };
            if bootstrap.tape_uuid != tape_uuid
                || bootstrap.block_size_bytes != size
                || bootstrap.drive_compression
                || !parity_matches
            {
                return Err(failure(
                    "recovered Bootstrap geometry differs from retained identity".into(),
                ));
            }
            match drive.read_block(&mut block) {
                Err(error)
                    if matches!(
                        error.kind,
                        remanence_library::TapeIoErrorKind::FilemarkEncountered
                    ) => {}
                other => {
                    return Err(failure(format!(
                        "recovered Bootstrap delimiter was not proved: {other:?}"
                    )))
                }
            }
            let position = drive.position().map_err(|e| failure(e.to_string()))?;
            if position.partition != 0 {
                return Err(failure(
                    "recovered Bootstrap is not in partition zero".into(),
                ));
            }
            lease
                .publish_initialized(
                    size,
                    match &parity {
                        remanence_parity::ParityConfig::None => None,
                        remanence_parity::ParityConfig::Scheme(scheme) => Some(scheme.clone()),
                    },
                    remanence_parity::CommitCursor {
                        partition: position.partition,
                        lba: position.lba,
                    },
                )
                .map_err(crate::status_from_state_error)?;
            index
                .project_tape_log(&lease.freeze().map_err(crate::status_from_state_error)?)
                .map_err(crate::status_from_state_error)?;
        }
        for fence in suffix_fences {
            library
                .append(|authority, view| {
                    authority.fence_released(
                        lib::FenceReleased {
                            detail: lib::Bounded(vec![]),
                            fence,
                            evidence:
                                "operator acknowledgment and repeated physical boundary proof"
                                    .into(),
                            forced: false,
                        },
                        actor.clone(),
                        SourceLayer::Layer5,
                        view,
                    )
                })
                .map_err(|e| failure(e.to_string()))?;
        }
        return Ok(found);
    }
    if sequence == 0 {
        lease
            .publish_empty_prefix(
                size,
                match &parity {
                    remanence_parity::ParityConfig::None => None,
                    remanence_parity::ParityConfig::Scheme(scheme) => Some(scheme.clone()),
                },
            )
            .map_err(crate::status_from_state_error)?;
    }
    let durable_snapshot = lease.freeze().map_err(crate::status_from_state_error)?;
    library
        .refresh_native(&durable_snapshot)
        .map_err(|e| failure(e.to_string()))?;
    let at = library
        .native_reference(&durable_snapshot)
        .map_err(|e| failure(e.to_string()))?;
    let preview_digest = admission
        .recovered_tail
        .as_ref()
        .map(|tail| tail.digest())
        .transpose()
        .map_err(|e| Status::internal(e.to_string()))?;
    let tail_file_count = admission
        .recovered_tail
        .as_ref()
        .map(|tail| tail.file_count())
        .transpose()
        .map_err(|e| Status::internal(e.to_string()))?
        .unwrap_or(0);
    let acknowledgment = library
        .append(|authority, view| {
            authority.supersede_tail(
                lib::SupersedeTail {
                    at,
                    operation: None,
                    session_id: snapshot
                        .tape_summary()
                        .open_session
                        .as_ref()
                        .map(|s| s.session_id),
                    preview_digest,
                    tail_file_count,
                },
                actor.clone(),
                SourceLayer::Layer5,
                view,
            )
        })
        .map_err(|e| failure(e.to_string()))?;
    #[cfg(test)]
    if take_ack_cut(library.manifest().library_uuid()) {
        drive.clear_append_admission();
        return Err(Status::unavailable(
            "injected cut after typed supersede acknowledgment",
        ));
    }
    let result = (|| {
        if let Some(open) = &snapshot.tape_summary().open_session {
            let tail = admission
                .recovered_tail
                .take()
                .ok_or_else(|| "missing crash tail proof".to_string())?;
            lease
                .end_session(
                    open.session_id,
                    super::restore::now_rfc3339().map_err(|e| e.to_string())?,
                    remanence_state::tape_log::SessionEndReason::CrashRecovered,
                    Some(acknowledgment.sequence),
                    tail.files().map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string())?;
        }
        // READ POSITION proves the retained boundary before the synchronous
        // recovery filemark advances it. Do not LOCATE merely to compare its
        // requested argument with itself.
        if repaired_tail {
            let expected = last
                .as_ref()
                .ok_or_else(|| "repaired tail has no retained commit".to_string())?
                .cursor;
            let observed = drive.position().map_err(|e| e.to_string())?;
            if observed.partition != expected.partition || observed.lba != expected.lba {
                return Err("physical repair cursor differs from retained authority".into());
            }
        }
        if last.is_none() {
            let bootstrap = crate::tape_init::build_tape_bootstrap(
                tape_uuid,
                size,
                parity.clone(),
                remanence_state::audit::writer_identity(env!("CARGO_PKG_VERSION"))
                    .written_at()
                    .map_err(|e| e.to_string())?,
                env!("CARGO_PKG_VERSION"),
            );
            crate::tape_init::write_tape_bootstrap(
                &mut remanence_library::DriveHandleSink(drive),
                &bootstrap,
            )
            .map_err(|e| e.to_string())?;
            // This recovery was authorized before dispatch by its exact
            // SupersedeTail. Publish initialization only after the synchronous
            // delimiter and real READ POSITION prove the new physical prefix.
            #[cfg(test)]
            if take_effect_cut(library.manifest().library_uuid()) {
                return Err("injected cut after physical recovery before Initialized".into());
            }
            let position = drive.position().map_err(|e| e.to_string())?;
            lease
                .publish_initialized(
                    size,
                    match &parity {
                        remanence_parity::ParityConfig::None => None,
                        remanence_parity::ParityConfig::Scheme(scheme) => Some(scheme.clone()),
                    },
                    remanence_parity::CommitCursor {
                        partition: position.partition,
                        lba: position.lba,
                    },
                )
                .map_err(|e| e.to_string())?;
        }
        if let remanence_parity::ParityConfig::Scheme(scheme) = &parity {
            let journal = lease
                .open_structural(size, scheme.clone())
                .map_err(|e| e.to_string())?;
            lease
                .retain_structural_publisher(journal)
                .map_err(|error| error.to_string())?;
            lease
                .with_structural(|lease, rows| {
                    lease.finish_structural_scope();
                    crate::append_admission::discard_reconciled_parity_suffix(
                        rows.ok_or_else(|| "missing retained structural capability".to_string())?,
                        lease,
                    )
                })
                .map_err(|error| error.to_string())??;
        }
        if repaired_tail {
            let expected = last
                .as_ref()
                .ok_or_else(|| "repaired tail has no retained commit".to_string())?
                .cursor;
            lease
                .reconcile_repaired_tail(expected)
                .map_err(|e| e.to_string())?;
        }
        Ok::<_, String>(())
    })();
    drive.clear_append_admission();
    if let Err(error) = result {
        #[cfg(test)]
        if error == "injected cut after physical recovery before Initialized" {
            let mut status = Status::unavailable(error);
            status
                .metadata_mut()
                .insert("remanence-test-crash-cut", "true".parse().unwrap());
            return Err(status);
        }
        crate::write_owner::raise_tape_io_fence(
            index,
            cfg,
            remanence_state::TapeIoFenceInput {
                tape_uuid,
                barcode: tape.voltag,
                reason: if lease.ensure_abandonment_allowed().is_err() {
                    "AuthorityAmbiguous"
                } else {
                    "supersede_tail_failed"
                }
                .into(),
                evidence_json: Some(serde_json::json!({"error":error}).to_string()),
            },
            drive
                .endpoint()
                .durable_key()
                .filter(|key| matches!(key, remanence_library::DriveKey::Standalone { .. })),
            None,
        )
        .map_err(crate::status_from_state_error)?;
        return Err(Status::unavailable(error));
    }
    // Projection consumes the exact held owner; reopening this tape path here
    // would contend with its lease and could borrow a stale recovery prefix.
    index
        .project_tape_log(&lease.freeze().map_err(crate::status_from_state_error)?)
        .map_err(crate::status_from_state_error)?;
    for fence in suffix_fences {
        library
            .append(|authority, view| {
                authority.fence_released(
                    lib::FenceReleased {
                        detail: lib::Bounded(vec![]),
                        fence,
                        evidence: "operator acknowledgment and successful physical tail recovery"
                            .into(),
                        forced: false,
                    },
                    actor.clone(),
                    SourceLayer::Layer5,
                    view,
                )
            })
            .map_err(|e| failure(e.to_string()))?;
    }
    Ok(found)
}

/// Per-library test cut: parallel fixtures cannot consume another runtime's cut.
#[cfg(test)]
fn ack_cuts() -> &'static std::sync::Mutex<std::collections::HashSet<[u8; 16]>> {
    static CUTS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<[u8; 16]>>> =
        std::sync::OnceLock::new();
    CUTS.get_or_init(Default::default)
}
#[cfg(test)]
pub(crate) fn cut_after_ack(library: [u8; 16]) {
    ack_cuts().lock().unwrap().insert(library);
}
#[cfg(test)]
fn take_ack_cut(library: [u8; 16]) -> bool {
    ack_cuts().lock().unwrap().remove(&library)
}

/// Test cut after the actual Bootstrap delimiter but before native publication.
#[cfg(test)]
fn effect_cuts() -> &'static std::sync::Mutex<std::collections::HashSet<[u8; 16]>> {
    static CUTS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<[u8; 16]>>> =
        std::sync::OnceLock::new();
    CUTS.get_or_init(Default::default)
}
#[cfg(test)]
pub(crate) fn cut_after_recovery_effect(library: [u8; 16]) {
    effect_cuts().lock().unwrap().insert(library);
}
#[cfg(test)]
fn take_effect_cut(library: [u8; 16]) -> bool {
    effect_cuts().lock().unwrap().remove(&library)
}
