//! Standalone writing frontend; one mounted actor owns each invocation.
use super::*;
use remanence_api::engine::drive_adapter::MountedDrive;

pub(super) fn dispatch(
    cli: &Cli,
    backend: &mut MountedDrive,
    output: &mut PutOutput<'_>,
) -> Result<Value, String> {
    let uuid = backend.tape_uuid();
    match &cli.command {
        Command::Put {
            paths,
            pool,
            recipient,
            allow_plaintext_copy,
            tape: guard,
        } => {
            backend.reconcile_checkpoints().map_err(err)?;
            let tape = backend
                .catalog_index()
                .get_tape(&uuid)
                .map_err(err)?
                .ok_or("loaded tape is not catalogued; use tape adopt")?;
            if let Some(guard) = guard {
                if !Uuid::parse_str(guard)
                    .map(|u| *u.as_bytes() == uuid)
                    .unwrap_or(false)
                    && tape.voltag.as_deref() != Some(guard.as_str())
                {
                    return Err(format!(
                        "requested tape {guard} differs from loaded tape {} ({})",
                        tape.voltag.as_deref().unwrap_or("unlabelled"),
                        Uuid::from_bytes(uuid)
                    ));
                }
            }
            let loaded = tape
                .pool_id
                .ok_or("loaded tape has no pool; use tape assign-pool")?;
            if let Some(p) = pool {
                if p != &loaded {
                    return Err(format!(
                        "requested pool {p} differs from loaded tape pool {loaded}"
                    ));
                }
            }
            let representation = if recipient.is_empty() {
                remanence_api::PoolWriteRepresentation::Plaintext
            } else {
                remanence_api::PoolWriteRepresentation::Encrypted {
                    recipients: crate::pool_ops::read_recipient_files(recipient)?,
                }
            };
            let mut objects = Vec::new();
            let mut failed = None;
            for p in paths {
                let archive = p
                    .file_name()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| p.clone());
                let result = collect_files(p, archive, &mut |source, archive_path, kind| {
                    if let Some(kind) = kind {
                        return output.receipt(
                            json!({"event":"skipped_unsupported","path":archive_path.to_string_lossy(),"kind":kind}),
                        );
                    }
                    let request = remanence_api::WriteObjectToPoolRequest {
                        pool_id: loaded.clone(),
                        caller_object_id: format!("put:{}", Uuid::new_v4()),
                        archive_path: archive_path.clone(),
                        source: remanence_api::WriteObjectSource::Path(source.into()),
                        expected_content_sha256: None,
                        expected_object_id: None,
                        representation: representation.clone(),
                        input_kind: remanence_api::WriteObjectInputKind::LogicalFile,
                    };
                    let render_written = |result: remanence_api::MountedWriteResult,
                                          reason: &str,
                                          output: &mut PutOutput<'_>,
                                          objects: &mut Vec<Value>|
                     -> Result<(), String> {
                        let mut locator = Vec::new();
                        crate::pool_ops::print_locator_json_with_recipients(
                            &result.object,
                            &loaded,
                            None,
                            &mut locator,
                        );
                        let locator: Value = serde_json::from_slice(&locator).map_err(err)?;
                        output.receipt(json!({"event":"written","reason":reason,"path":archive_path.to_string_lossy(),"object_id":Uuid::from_bytes(result.object.object_id),"tape_uuid":Uuid::from_bytes(uuid),"label":tape.voltag,"bytes":result.object.logical_size_bytes}))?;
                        objects.push(locator);
                        Ok(())
                    };
                    let outcome = match backend
                        .put_path_version_with_policy(request, *allow_plaintext_copy)
                    {
                        Err(remanence_api::MountError::Committed {
                            receipt,
                            reason,
                            failure,
                        }) => {
                            render_written(*receipt, reason, output, &mut objects)?;
                            Err(*failure)
                        }
                        outcome => outcome,
                    };
                    if let Err(remanence_api::MountError::PlaintextCopy { newest_version }) =
                        &outcome
                    {
                        return output.receipt(json!({"event":"refused","reason":"would_store_plaintext_copy_of_encrypted","path":archive_path.to_string_lossy(),"newest_version":newest_version}));
                    }
                    match outcome.map_err(|error| {
                        output.tape_continuation = tape_continuation(&error);
                        format!("{}: {error}", source.display())
                    })? {
                        Ok((result, reason)) => {
                            render_written(result, reason, output, &mut objects)?;
                        }
                        Err(version) => {
                            let (copy, label) = version
                                .copies
                                .first()
                                .ok_or("committed version has no copy")?;
                            output.receipt(json!({"event":"already_stored","path":archive_path.to_string_lossy(),"object_id":version.object_id,"tape_uuid":Uuid::from_slice(copy).map_err(err)?,"label":label}))?;
                        }
                    }
                    Ok(())
                });
                if let Err(failure) = result {
                    failed = Some(failure);
                    break;
                }
            }
            // Owner-end epoch closure can fail after all object receipts. Finish
            // before the summary so the invocation cannot report false success.
            if let Err(error) = backend.finish() {
                if failed.is_none() {
                    failed = Some((
                        paths.last().cloned().unwrap_or_else(|| PathBuf::from(".")),
                        error.to_string(),
                    ));
                }
            }
            output.finish(failed.as_ref(), &loaded)?;
            match failed {
                Some((path, error)) => Err(format!("{}: {error}", path.display())),
                None if output.receipts.iter().any(|r| r["event"] == "refused") => {
                    Err("one or more files refused".into())
                }
                None => Ok(
                    json!({"tape":Uuid::from_bytes(uuid),"pool":loaded,"objects":objects,"receipts":output.receipts,"summary":output.summary}),
                ),
            }
        }
        Command::Tape { command } => match command {
            Tape::SupersedeTail {
                acknowledge_tape,
                confirm_supersede,
            } => {
                let discarded = backend.supersede_tail_confirmed(acknowledge_tape, |expected, found| {
                        eprintln!("Tail to discard: {}", found.join(", "));
                        let confirmation = match confirm_supersede {
                            Some(value) => value.clone(),
                            None => {
                                use std::io::IsTerminal;
                                if !std::io::stdin().is_terminal() {
                                    return Err(format!("non-interactive supersede requires --confirm-supersede {expected:?}"));
                                }
                                eprint!("Type {expected:?} to discard this tail: ");
                                std::io::stderr().flush().map_err(err)?;
                                let mut line = String::new();
                                std::io::stdin().read_line(&mut line).map_err(err)?;
                                line.trim_end_matches(['\r', '\n']).to_owned()
                            }
                        };
                        if confirmation != expected { return Err("supersede confirmation did not match".into()); }
                        Ok(())
                    }).map_err(err)?;
                Ok(json!({"discarded_object_ids": discarded}))
            }
            Tape::Finalize {
                tape,
                acknowledge_tape,
                reason,
                idempotency_key,
                expected_pool,
            } => {
                let row = backend
                    .catalog_index()
                    .get_tape(&uuid)
                    .map_err(err)?
                    .ok_or("loaded tape is not catalogued")?;
                if Uuid::parse_str(acknowledge_tape)
                    .ok()
                    .map(|u| *u.as_bytes())
                    != Some(uuid)
                    && row.voltag.as_deref() != Some(acknowledge_tape.as_str())
                {
                    return Err("--acknowledge-tape does not match the loaded tape".into());
                }
                let result = backend
                    .finalize_standalone(remanence_api::StandaloneFinalizeRequest {
                        tape_uuid: *tape.unwrap_or(Uuid::from_bytes(uuid)).as_bytes(),
                        acknowledge_tape: uuid,
                        reason: reason
                            .clone()
                            .unwrap_or_else(|| DEFAULT_FINALIZE_REASON.into()),
                        idempotency_key: idempotency_key
                            .unwrap_or_else(|| default_finalize_key(uuid)),
                        expected_pool_id: expected_pool.clone().or(row.pool_id),
                    })
                    .map_err(err)?;
                Ok(
                    json!({"finalized": matches!(result.outcome, remanence_state::TerminalFinalizationOutcome::Finalized | remanence_state::TerminalFinalizationOutcome::FinalizedDegraded), "outcome": result.outcome.as_str(), "operation_id": result.operation_id, "completed_replicas": result.completed_replicas, "edition_digest": crate::bytes_to_hex(&result.edition_digest), "layout_digest": crate::bytes_to_hex(&result.layout_digest)}),
                )
            }
            _ => unreachable!("writing command routing"),
        },
        _ => unreachable!("writing command routing"),
    }
}
