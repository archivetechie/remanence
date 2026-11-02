//! Field leg A: checkpoint a prefix, hold an incomplete append at actual daemon
//! dispatch, then verify exact-tape closeout and retained receipts. No device I/O
//! is available here; all media commands go through the scoped daemon.
use super::*;
use remanence_state::tape_log::{codec::Decoder, fold::Fold, store::Header};
use std::io::{Seek, Write};

#[derive(Parser, Debug)]
pub(super) struct Args {
    #[arg(long, value_parser = ["prepare", "finish", "live-control", "snapshot", "cancel"])]
    phase: String,
    #[arg(long)]
    tape: Uuid,
    #[arg(long, value_parser = ["AOX032L9", "AOX036L9", "AOX038L9", "AOX058L9"])]
    barcode: String,
    #[arg(long)]
    pool: String,
    #[arg(long)]
    evidence: PathBuf,
    #[arg(long)]
    gate: PathBuf,
    #[arg(long)]
    log: PathBuf,
    #[arg(long)]
    work: PathBuf,
    #[arg(long, value_parser = ["kill-mid-write", "abort-mid-object", "failed-append", "measure-full-epoch"])]
    mode: String,
    #[arg(long, default_value_t = 4 * 1024 * 1024)]
    seed_bytes: u64,
}
fn save(path: &Path, value: &Value) -> AppResult<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    writeln!(f, "{value}")?;
    f.sync_all()?;
    Ok(())
}
fn read_json(path: &Path) -> AppResult<Value> {
    serde_json::from_slice(&std::fs::read(path)?).map_err(|e| AppError::new(e.to_string()))
}
fn native(path: &Path, tape: Uuid) -> AppResult<Value> {
    let mut f = std::fs::File::open(path)?;
    Header::decode(&mut f, *tape.as_bytes()).map_err(|e| AppError::new(e.to_string()))?;
    let offset = f.stream_position()?;
    let mut decoder = Decoder::at(f, 1, offset);
    let mut fold = Fold::new();
    let mut records = Vec::new();
    while let Some(frame) = decoder
        .next_frame()
        .map_err(|e| AppError::new(e.to_string()))?
    {
        fold.push(&frame)
            .map_err(|e| AppError::new(e.to_string()))?;
        let tail = match &frame.record {
            remanence_state::tape_log::Record::PayloadContinuation(c) => match &c.chunk {
                remanence_state::tape_log::Chunk::TailFile(f) => Some(json!({
                    "file":f.tape_file_number, "known_blocks":f.known_blocks, "unknown_blocks":f.unknown_blocks,
                    "filemark":format!("{:?}",f.filemark),
                    "identity":match &f.identity {
                        remanence_state::tape_log::TailIdentity::Object(id) => json!({"object_id":id}),
                        remanence_state::tape_log::TailIdentity::TornPrefix(hash) => json!({"torn_sha256":bytes_to_hex(hash)}),
                        remanence_state::tape_log::TailIdentity::Sidecar(hash) => json!({"sidecar_sha256":bytes_to_hex(hash)}),
                    }
                })),
                _ => None,
            },
            _ => None,
        };
        let file_row = match &frame.record {
            remanence_state::tape_log::Record::PayloadContinuation(c) => match &c.chunk {
                remanence_state::tape_log::Chunk::FileRow(row) => {
                    Some(serde_json::to_value(row).map_err(|e| AppError::new(e.to_string()))?)
                }
                _ => None,
            },
            _ => None,
        };
        records.push(
            json!({"sequence":frame.sequence, "record":format!("{:?}",frame.record), "tail":tail, "file_row":file_row}),
        );
    }
    let s = fold.summary();
    records.retain(|r| r["sequence"].as_u64().unwrap() <= s.last_closed_sequence);
    Ok(
        json!({"schema":"rem.field.native.v1", "tape_uuid":tape.to_string(),
        "sequence":s.last_closed_sequence, "C":{"partition":s.cursor.partition,"lba":s.cursor.lba},
        "W":s.protected, "T":s.total, "check":s.check.0,
        "block_size":s.block_size, "epoch_blocks":s.epoch_blocks(), "geometry":format!("{:?}",s.scheme),
        "open_session":s.open_session.as_ref().map(|v| Uuid::from_bytes(v.session_id).to_string()),
        "end":format!("{:?}",s.last_session_end), "sidecar":format!("{:?}",s.last_closeout_record),
        "closed_out":s.last_closeout_record.as_ref().map(|c| json!({"from_w":c.from_w,"to_t":c.to_t,"check_before":c.check_before.0,"epoch_id":c.epoch_id,"sidecar_file_number":c.sidecar_file_number})),
        "barrier_count":s.barrier_count, "records":records}),
    )
}
async fn wait_file(path: &Path, timeout: Duration) -> AppResult<()> {
    tokio::time::timeout(timeout, async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| AppError::new(format!("gate timed out: {}", path.display())))
}
fn receipt(record: &pb::ObjectRecord, pool: &str) -> Value {
    write_result_json(
        record,
        pool,
        record.logical_size_bytes.unwrap_or(0),
        0.0,
        PhaseTimings {
            open_ms: None,
            transfer_ms: None,
            close_ms: None,
        },
    )
}
pub(super) async fn run(endpoint: &str, args: Args) -> AppResult<()> {
    if args.seed_bytes == 0 {
        return Err(AppError::new("seed must be nonempty"));
    }
    if args.phase == "snapshot" {
        return print_json_line(native(&args.log, args.tape)?);
    }
    let channel = connect_daemon(endpoint).await?;
    if args.phase == "cancel" {
        let session = read_json(&args.evidence.join("session.json"))?;
        let id = parse_uuid_or_hex(session["session_id"].as_str().unwrap_or(""))?;
        let mut client = pb::write_session_service_client::WriteSessionServiceClient::new(channel);
        client
            .abort_write_session(pb::AbortWriteSessionRequest {
                session_id: id.to_vec(),
                reason: Some("field teardown".into()),
                idempotency_key: None,
            })
            .await?;
        return Ok(());
    }
    let mut catalog = pb::catalog_client::CatalogClient::new(channel.clone());
    let tapes = catalog
        .list_tapes(pb::ListTapesRequest::default())
        .await?
        .into_inner();
    let tape = tapes
        .tapes
        .iter()
        .find(|t| t.tape_uuid == args.tape.as_bytes())
        .ok_or_else(|| AppError::new("exact tape absent"))?;
    if tape.voltag.as_deref() != Some(&args.barcode) || tape.pool_id.as_deref() != Some(&args.pool)
    {
        return Err(AppError::new("barcode/UUID/pool mismatch"));
    }
    match args.phase.as_str() {
        "prepare" | "live-control" => prepare(channel, &args).await,
        "finish" => finish(endpoint, channel, &args).await,
        _ => unreachable!(),
    }
}
async fn prepare(channel: Channel, args: &Args) -> AppResult<()> {
    let mut client = pb::write_session_service_client::WriteSessionServiceClient::new(channel);
    let opened = client
        .open_write_session(pb::OpenWriteSessionRequest {
            target: Some(pb::open_write_session_request::Target::TapeTarget(
                pb::TapeTarget {
                    tape_uuid: args.tape.as_bytes().to_vec(),
                    mount_if_needed: true,
                    required_pool_id: args.pool.clone(),
                    allow_unpooled: false,
                },
            )),
            body_format: Some("rem-object-v1".into()),
            ..Default::default()
        })
        .await?
        .into_inner();
    let session = opened.session_id;
    save(
        &args.evidence.join("session.json"),
        &json!({"session_id":uuid_bytes_to_text(&session)?, "tape_uuid":args.tape.to_string()}),
    )?;
    let result = prepare_open(&mut client, &session, args).await;
    if result.is_err() {
        // Release a held dispatch before asking the actor to tear down.
        let _ = std::fs::write(args.gate.join("release"), b"cleanup");
        abort_write_session(&mut client, &session).await;
    }
    result?;
    save(
        &args.evidence.join("prepared.json"),
        &json!({"complete":true}),
    )
}
async fn prepare_open(
    client: &mut pb::write_session_service_client::WriteSessionServiceClient<Channel>,
    session: &[u8],
    args: &Args,
) -> AppResult<()> {
    let seed_bytes = if args.mode == "measure-full-epoch" {
        let initial = native(&args.log, args.tape)?;
        initial["epoch_blocks"]
            .as_u64()
            .and_then(|n| n.checked_mul(initial["block_size"].as_u64()?))
            .and_then(|n| n.checked_sub(4 * 1024 * 1024))
            .filter(|n| *n > 0)
            .ok_or_else(|| AppError::new("full-epoch geometry unavailable"))?
    } else {
        args.seed_bytes
    };
    let seed = append_object(
        client,
        session,
        AppendInput::generated(
            1,
            seed_bytes,
            DEFAULT_CHUNK_BYTES,
            format!("field-seed-{}", Uuid::new_v4()),
        ),
    )
    .await?;
    let checkpoint = client
        .checkpoint_session(pb::CheckpointSessionRequest {
            session_id: session.to_vec(),
            idempotency_key: None,
        })
        .await?
        .into_inner();
    let committed = checkpoint
        .committed_objects
        .iter()
        .find(|o| o.object_id == seed.object_id)
        .ok_or_else(|| AppError::new("checkpoint did not return seed receipt"))?;
    save(
        &args.evidence.join("receipt.json"),
        &receipt(committed, &args.pool),
    )?;
    let mut before = native(&args.log, args.tape)?;
    before["trace_offset"] = json!(std::fs::metadata(args.evidence.join("commands.jsonl"))?.len());
    if before["W"].as_u64() >= before["T"].as_u64()
        || before["open_session"] != uuid_bytes_to_text(session)?
    {
        return Err(AppError::new(
            "prefix is not an uncovered checkpoint in the same open session",
        ));
    }
    save(&args.evidence.join("before.json"), &before)?;
    if args.phase == "live-control" {
        close_write_session(client, session).await?;
        return Ok(());
    }
    let nonce = Uuid::new_v4();
    let action = if args.mode == "failed-append" {
        "fail"
    } else {
        "hold"
    };
    save(
        &args.gate.join("arm.json"),
        &json!({"schema":"rem.field.arm.v1", "tape_uuid":args.tape.to_string(),
        "barcode":args.barcode,"nonce":nonce.to_string(),"session_id":uuid_bytes_to_text(session)?,"action":action}),
    )?;
    // Declared replayable source with a start digest qualifies for overlap.
    // A withheld Finish prevents this object from receiving a commit.
    let size = 2 * 1024 * 1024 * 1024u64;
    let mut hasher = Sha256::new();
    let mut data = vec![0u8; DEFAULT_CHUNK_BYTES];
    for offset in (0..size).step_by(data.len()) {
        fill_generated_payload_chunk(2, offset, &mut data);
        hasher.update(&data);
    }
    let digest = hasher.finalize().to_vec();
    let (tx, rx) = tokio::sync::mpsc::channel(2);
    tx.send(pb::AppendObjectMessage {
        payload: Some(pb::append_object_message::Payload::Start(
            pb::AppendObjectStart {
                session_id: session.to_vec(),
                caller_object_id: format!("field-abandoned-{nonce}"),
                caller_metadata: HashMap::from([("path".into(), "abandoned.bin".into())]),
                declared_size_bytes: Some(size),
                expected_content_sha256: Some(digest.clone()),
                expected_content_digest: Some(pb::Digest {
                    algorithm: "sha256".into(),
                    value: digest,
                }),
                source_replay_capability: pb::SourceReplayCapability::ReplayFromStart as i32,
                ..Default::default()
            },
        )),
    })
    .await
    .map_err(|e| AppError::new(e.to_string()))?;
    let mut append_client = client.clone();
    let mut append =
        tokio::spawn(async move { append_client.append_object(ReceiverStream::new(rx)).await });
    let session_copy = session.to_vec();
    let sender = tokio::spawn(async move {
        let mut hash = Sha256::new();
        send_generated_chunks(&tx, &session_copy, 2, size, DEFAULT_CHUNK_BYTES, &mut hash).await?;
        // Keep the stream open without Finish, even if transport buffers drain.
        std::future::pending::<()>().await;
        drop(tx);
        Ok::<(), AppError>(())
    });
    let result = async {
        wait_file(&args.gate.join("dispatch.json"),Duration::from_secs(120)).await?;
        let proof = read_json(&args.gate.join("dispatch.json"))?;
        if proof["nonce"] != nonce.to_string() || proof["tape_uuid"] != args.tape.to_string() || proof["bytes_completed"].as_u64().unwrap_or(0) == 0 {
            return Err(AppError::new("dispatch proof identity or byte count differs"));
        }
        save(&args.evidence.join("fault.json"),&proof)?;
        print_json_line(json!({"gate":"data-dispatched", "evidence":args.evidence}))?;
        if args.mode == "kill-mid-write" { std::future::pending::<()>().await; }
        if args.mode == "failed-append" {
            std::fs::write(args.gate.join("release"), b"fail")?;
            // Further queued data meets the one-shot refusal after the proved write.
            let response = tokio::time::timeout(Duration::from_secs(60), &mut append).await
                .map_err(|_| AppError::new("failed append did not settle"))?
                .map_err(|e| AppError::new(e.to_string()))?;
            if response.is_ok() { return Err(AppError::new("abandoned append returned a receipt")); }
            save(&args.evidence.join("append-error.json"),&json!({"error":response.unwrap_err().to_string()}))?;
        } else {
            append.abort();
            let _ = (&mut append).await;
            wait_file(&args.gate.join("cancelled.json"), Duration::from_secs(30)).await?;
            let cancelled = read_json(&args.gate.join("cancelled.json"))?;
            if cancelled["nonce"] != nonce.to_string() || cancelled["session_id"] != uuid_bytes_to_text(session)? || cancelled["tape_uuid"] != args.tape.to_string() {
                return Err(AppError::new("stream cancellation identity differs"));
            }
            save(&args.evidence.join("stream-cancelled.json"), &cancelled)?;
            std::fs::write(args.gate.join("release"), b"abort")?;
        }
        let aborted = client.abort_write_session(pb::AbortWriteSessionRequest {session_id:session.to_vec(),idempotency_key:None,reason:Some("field leg A".into())}).await?.into_inner();
        if aborted.checkpointed_objects.iter().any(|r| r.caller_object_id.as_deref().is_some_and(|v| v.starts_with("field-abandoned-"))) {
            return Err(AppError::new("abandoned receipt leaked"));
        }
        save(&args.evidence.join("abort.json"),&json!({"state":aborted.state,"receipts":aborted.checkpointed_objects.iter().map(|r|receipt(r,&args.pool)).collect::<Vec<_>>()}))?;
        Ok(())
    }.await;
    sender.abort();
    append.abort();
    result
}
fn validate_preview(
    tape: Uuid,
    before: &Value,
    proof: &Value,
    preview: &pb::SupersedeTailResponse,
) -> AppResult<()> {
    let expected = proof["object_id"]
        .as_str()
        .filter(|v| Uuid::parse_str(v).is_ok());
    if before["tape_uuid"] != tape.to_string()
        || proof["tape_uuid"] != tape.to_string()
        || expected.is_none()
        || preview.found != vec![expected.unwrap_or("").to_string()]
        || before["sequence"].as_u64() != Some(preview.tape_log_sequence)
        || preview.required_confirmation.is_empty()
    {
        return Err(AppError::new(
            "unexpected crash UUID, tail or sequence; acknowledgement refused",
        ));
    }
    Ok(())
}
async fn finish(endpoint: &str, channel: Channel, args: &Args) -> AppResult<()> {
    let mut catalog = pb::catalog_client::CatalogClient::new(channel.clone());
    if args.mode == "kill-mid-write" {
        let preview = catalog
            .supersede_tail(pb::SupersedeTailRequest {
                tape_uuid: args.tape.as_bytes().to_vec(),
                ..Default::default()
            })
            .await;
        let preview = match preview {
            Ok(response) => Some(response.into_inner()),
            Err(status)
                if status.code() == tonic::Code::FailedPrecondition
                    && status
                        .message()
                        .contains("no foreign tail requires superseding") =>
            {
                save(
                    &args.evidence.join("crash-preview.json"),
                    &json!({"tape_uuid":args.tape.to_string(),"requires_ack":false,"status":status.to_string()}),
                )?;
                None
            }
            Err(status) => return Err(status.into()),
        };
        if let Some(preview) = preview {
            save(
                &args.evidence.join("crash-preview.json"),
                &json!({"tape_uuid":args.tape.to_string(),"sequence":preview.tape_log_sequence,"found":preview.found,"confirmation":preview.required_confirmation}),
            )?;
            if !preview.found.is_empty() {
                let proof = read_json(&args.evidence.join("fault.json"))?;
                let before = read_json(&args.evidence.join("before.json"))?;
                validate_preview(args.tape, &before, &proof, &preview)?;
                let ack = catalog
                    .supersede_tail(pb::SupersedeTailRequest {
                        tape_uuid: args.tape.as_bytes().to_vec(),
                        ack_tape_uuid: args.tape.as_bytes().to_vec(),
                        confirm_supersede: Some(preview.required_confirmation),
                        expected_found: preview.found,
                        observed_tape_log_sequence: Some(preview.tape_log_sequence),
                    })
                    .await?
                    .into_inner();
                save(
                    &args.evidence.join("crash-ack.json"),
                    &json!({"completed":ack.completed,"sequence":ack.tape_log_sequence,"found":ack.found}),
                )?;
            }
        }
    }
    let key = Uuid::new_v4();
    save(
        &args.evidence.join("closeout-key.json"),
        &json!({"tape_uuid":args.tape.to_string(),"idempotency_key":key.to_string()}),
    )?;
    let operation = catalog
        .closeout_tape(pb::CloseoutTapeRequest {
            tape_uuid: args.tape.as_bytes().to_vec(),
            expected_pool_id: Some(args.pool.clone()),
            idempotency_key: Some(pb::IdempotencyKey {
                value: key.as_bytes().to_vec(),
            }),
        })
        .await?
        .into_inner();
    let mut watch = pb::daemon_client::DaemonClient::new(channel)
        .watch_operation(pb::GetOperationRequest {
            operation_id: operation.operation_id,
        })
        .await?
        .into_inner();
    let mut updates = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(args.evidence.join("closeout.jsonl"))?;
    let mut succeeded = false;
    while let Some(status) = watch.message().await? {
        writeln!(
            updates,
            "{}",
            json!({"operation_id":uuid_bytes_to_text(&status.operation_id)?,"state":status.state,"progress":status.progress,"error":status.error_summary})
        )?;
        updates.sync_data()?;
        if status.state == pb::OperationState::Succeeded as i32 {
            succeeded = true;
        }
        if status.state == pb::OperationState::Failed as i32 {
            return Err(AppError::new("closeout failed; see watch"));
        }
    }
    if !succeeded {
        return Err(AppError::new("closeout watch ended without success"));
    }
    let after = native(&args.log, args.tape)?;
    if after["W"] != after["T"] || after["open_session"].is_string() {
        return Err(AppError::new("closeout did not discharge session debt"));
    }
    save(&args.evidence.join("after.json"), &after)?;
    let mut objects = catalog
        .enumerate_objects(pb::EnumerateObjectsRequest {
            scope: Some(pb::enumerate_objects_request::Scope::All(())),
            reconcile_from_tape: false,
        })
        .await?
        .into_inner();
    let mut rows = Vec::new();
    while let Some(object) = objects.message().await? {
        rows.push(receipt(&object, &args.pool));
    }
    save(
        &args.evidence.join("objects-after.json"),
        &json!({"data":{"objects":rows}}),
    )?;
    let receipt = read_json(&args.evidence.join("receipt.json"))?;
    std::fs::create_dir_all(&args.work)?;
    let restored = args.work.join("restored.bin");
    read_command(
        endpoint,
        ReadArgs {
            object: json!({"object_id":receipt["object_id"],"tape_uuid":args.tape.to_string(),"pool_id":args.pool}).to_string(),
            out: restored.clone(),
            offset: 0,
            length: None,
            chunk_bytes: DEFAULT_CHUNK_BYTES_U32,
        },
        true,
    )
    .await?;
    let digest = bytes_to_hex(&hash_file_before_start(&restored).await?);
    if receipt["content_sha256"] != digest {
        return Err(AppError::new("restored prefix hash differs"));
    }
    save(
        &args.evidence.join("verified.json"),
        &json!({"object_id":receipt["object_id"],"sha256":digest,"bytes":std::fs::metadata(&restored)?.len()}),
    )?;
    std::fs::remove_file(restored)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Feed actual native v2 bytes into the same printer used in the field.
    /// Exported files are shell-test fixtures, not hand-authored CLI responses.
    #[test]
    fn native_printer_and_generated_field_fixtures() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../fixtures/tape-log/parity-object-session-close.remtape");
        let id = Uuid::from_bytes([0xa2; 16]);
        let after = native(&fixture, id).unwrap();
        assert_eq!(after["W"], after["T"]);
        assert_eq!(after["closed_out"]["from_w"], 0);
        assert_eq!(after["closed_out"]["to_t"], 3);
        assert_eq!(after["sequence"], 8);
        let bytes = std::fs::read(&fixture).unwrap();
        let mut cursor = std::io::Cursor::new(bytes.as_slice());
        Header::decode(&mut cursor, *id.as_bytes()).unwrap();
        let mut decoder = Decoder::at(&bytes[cursor.position() as usize..], 1, cursor.position());
        let mut end = None;
        while let Some(frame) = decoder.next_frame().unwrap() {
            if frame.sequence == 5 {
                end = Some(frame.end as usize);
            }
        }
        let dir = std::env::temp_dir().join(format!("field-native-{}", Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let log = dir.join("prefix.remtape");
        std::fs::write(&log, &bytes[..end.unwrap()]).unwrap();
        let before = native(&log, id).unwrap();
        assert_eq!(before["W"], 0);
        assert_eq!(before["T"], 3);
        assert!(before["open_session"].is_string());
        assert!(native(&log, Uuid::new_v4()).is_err());
        let record = pb::ObjectRecord {
            object_id: vec![1; 16],
            caller_object_id: Some("field-seed-model".into()),
            logical_size_bytes: Some(64),
            content_sha256: Some(vec![2; 32]),
            ..Default::default()
        };
        let printed = receipt(&record, "fieldtest-a");
        assert_eq!(printed["object_id"], Uuid::from_bytes([1; 16]).to_string());
        if let Ok(export) = std::env::var("FIELD_FIXTURE_EXPORT_DIR") {
            let export = Path::new(&export);
            std::fs::create_dir_all(export).unwrap();
            for (name, value) in [
                ("session-before.json", before),
                ("session-after.json", after),
                ("session-receipt.json", printed),
            ] {
                std::fs::write(export.join(name), format!("{value}\n")).unwrap();
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn crash_ack_requires_exact_identity_sequence_and_found_ids() {
        let tape = Uuid::from_bytes([3; 16]);
        let object = Uuid::from_bytes([4; 16]).to_string();
        let before = json!({"tape_uuid":tape.to_string(),"sequence":5});
        let proof = json!({"tape_uuid":tape.to_string(),"object_id":object});
        let mut preview = pb::SupersedeTailResponse {
            tape_log_sequence: 5,
            found: vec![object],
            required_confirmation: "confirmation from real preview".into(),
            ..Default::default()
        };
        assert!(validate_preview(tape, &before, &proof, &preview).is_ok());
        preview.tape_log_sequence += 1;
        assert!(validate_preview(tape, &before, &proof, &preview).is_err());
        preview.tape_log_sequence = 5;
        preview.found.push(Uuid::new_v4().to_string());
        assert!(validate_preview(tape, &before, &proof, &preview).is_err());
        preview.found.pop();
        assert!(validate_preview(Uuid::new_v4(), &before, &proof, &preview).is_err());
    }
    #[tokio::test]
    async fn gate_timeout_is_a_failure_and_never_a_media_claim() {
        let path = std::env::temp_dir().join(Uuid::new_v4().to_string());
        assert!(wait_file(&path, Duration::from_millis(5)).await.is_err());
    }
}
