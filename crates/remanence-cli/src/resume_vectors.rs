//! Portable resume recipes and a reference-only journal adapter. The adapter
//! frames claims without accepting or repairing them; production journal replay
//! and bounded resume validation decide whether those claims authorize append.
use crate::tape_image_vectors::{inputs, object, recipe, VectorImage, BLOCK};
use ciborium::value::Value as Cbor;
use remanence_chaos::{
    model::{DeviceRole, ModelTransport, Record, SharedVirtualWorld, VirtualTape, VirtualWorld},
    ChaosTransport, DeviceCtx, FaultEngine,
};
use remanence_library::DriveHandle;
use remanence_parity::*;
use serde_json::{json, Value};
use std::{
    fs::OpenOptions,
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
};

pub const EXPECTATIONS: &str = include_str!(
    "../../../fixtures/rem-parity-terminal-index-draft/tape-images/resume/expected-cases.json"
);

/// Preserve each complete JSON case's literal spelling, including multiline cases.
pub fn literal_cases() -> Vec<&'static str> {
    let end = EXPECTATIONS.rfind("\n  ]").unwrap();
    let starts: Vec<_> = EXPECTATIONS
        .match_indices("    {\"id\"")
        .map(|(offset, _)| offset + 4)
        .collect();
    starts
        .iter()
        .enumerate()
        .map(|(i, &start)| {
            let next = starts.get(i + 1).map_or(end, |offset| offset - 4);
            EXPECTATIONS[start..next].trim_end().trim_end_matches(',')
        })
        .collect()
}

/// The only mutations are the portable prefix mutations specified by D5.
pub fn portable_input(id: &str, vector: &VectorImage) -> Value {
    let mut prefix = vector.written.map.entries().to_vec();
    if id.starts_with("resume-non") {
        prefix.truncate(4);
    }
    match id {
        "resume-closed" | "resume-open" => {}
        "resume-w-greater-than-t" => prefix[1].block_count = 3,
        "resume-full-unprotected-epoch" => prefix.truncate(2),
        "resume-final-object-not-at-t" => prefix[3].first_parity_data_ordinal = Some(1),
        "resume-boundary-read" => prefix[3].block_count = 3,
        "resume-noncontiguous-sidecars" => prefix[3].protected_ordinal_start = Some(5),
        "resume-nonconsecutive-epochs" => prefix[3].epoch_id = Some(2),
        _ => panic!("unknown portable case {id}"),
    }
    let w = prefix
        .iter()
        .filter_map(|e| e.protected_ordinal_end_exclusive)
        .max()
        .unwrap_or(0);
    let t = prefix
        .iter()
        .filter_map(|e| e.first_parity_data_ordinal.map(|o| o + e.block_count))
        .max()
        .unwrap_or(0);
    let rows: Vec<_> = prefix.iter().map(|e| json!({
        "tape_file_number":e.tape_file_number, "block_count":e.block_count,
        "kind":format!("{:?}",e.kind), "first_parity_data_ordinal":e.first_parity_data_ordinal,
        "protected_ordinal_start":e.protected_ordinal_start,
        "protected_ordinal_end_exclusive":e.protected_ordinal_end_exclusive,"epoch_id":e.epoch_id
    })).collect();
    let name = if id.starts_with("resume-non") {
        "two-epoch"
    } else if matches!(
        id,
        "resume-open" | "resume-final-object-not-at-t" | "resume-boundary-read"
    ) {
        "unfinalized-open"
    } else {
        "unfinalized-closed"
    };
    let mut result = json!({"image":name,"committed_prefix":rows,"W":w,"T":t});
    if matches!(id, "resume-closed" | "resume-open") {
        let mut appended = inputs(name);
        appended.objects = vec![object(2, if id == "resume-open" { 2 } else { 1 })];
        result["append_object"] = recipe(&appended)["objects"][0].clone();
    }
    result
}

/// Encode v4 reference journal frames, including contradictory claims. This is
/// deliberately not `commit_bundle`: a writer rejects W > T before persisting
/// it, whereas a resume executor must exercise replay of the claimed records.
/// Nonportable sidecar hashes come from the unchanged source image's metadata.
pub fn adapt(
    input: &Value,
    vector: &VectorImage,
    path: &Path,
) -> Result<FileTapeFileJournal, ParityError> {
    let journal = FileTapeFileJournal::open(
        path,
        vector.written.inputs.tape_uuid,
        BLOCK,
        vector.written.inputs.scheme.clone(),
    )?;
    let mut file = OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(JournalError::from)?;
    let uint = |n: u64| Cbor::Integer(n.into());
    let optional = |v: &Value| v.as_u64().map_or(Cbor::Null, &uint);
    let mut w = 0;
    let mut t = 0;
    let mut lba = 0;
    for row in input["committed_prefix"].as_array().unwrap() {
        let f = row["tape_file_number"].as_u64().unwrap();
        if let Some(physical) = row["physical_start_override"].as_u64() {
            lba = physical;
        }
        let (kind, bundle) = match row["kind"].as_str().unwrap() {
            "Bootstrap" => (2, 1),
            "Object" => (0, 0),
            "ParitySidecar" => (1, 3),
            other => panic!("unsupported resume row {other}"),
        };
        if let Some(first) = row["first_parity_data_ordinal"].as_u64() {
            t = t.max(first + row["block_count"].as_u64().unwrap());
        }
        if let Some(end) = row["protected_ordinal_end_exclusive"].as_u64() {
            w = w.max(end);
        }
        let hash = vector
            .written
            .sidecars
            .iter()
            .find(|s| s.tape_file_number == f)
            .map_or(Cbor::Null, |s| {
                Cbor::Bytes(s.canonical_metadata_hash.to_vec())
            });
        let entry = Cbor::Map(vec![
            (uint(1), uint(f)),
            (uint(2), uint(kind)),
            (uint(3), optional(&row["block_count"])),
            (uint(4), uint(lba)),
            (uint(5), Cbor::Null),
            (uint(6), optional(&row["first_parity_data_ordinal"])),
            (uint(7), optional(&row["epoch_id"])),
            (uint(8), optional(&row["protected_ordinal_start"])),
            (uint(9), optional(&row["protected_ordinal_end_exclusive"])),
            (uint(10), hash),
            (uint(11), Cbor::Null),
        ]);
        frame(&mut file, bundle, vec![entry], w, t)?;
        // Supplemental hostile commit claims retain measured starts, avoiding
        // arithmetic on their block counts inside the reference adapter.
        if row["physical_start_override"].is_null() {
            lba += row["block_count"].as_u64().unwrap() + 1;
        }
    }
    frame(
        &mut file,
        4,
        vec![],
        input["W"].as_u64().unwrap(),
        input["T"].as_u64().unwrap(),
    )?;
    file.sync_all().map_err(JournalError::from)?;
    drop(file);
    drop(journal);
    resume_record_result(FileTapeFileJournal::open(
        path,
        vector.written.inputs.tape_uuid,
        BLOCK,
        vector.written.inputs.scheme.clone(),
    ))
}

fn frame(
    file: &mut std::fs::File,
    kind: u64,
    entries: Vec<Cbor>,
    w: u64,
    t: u64,
) -> Result<(), ParityError> {
    let uint = |n: u64| Cbor::Integer(n.into());
    let value = Cbor::Map(vec![
        (uint(1), uint(kind)),
        (uint(2), Cbor::Array(entries)),
        (uint(3), uint(w)),
        (uint(4), uint(t)),
    ]);
    let mut bytes = Vec::new();
    ciborium::into_writer(&value, &mut bytes).map_err(|e| JournalError::Codec(e.to_string()))?;
    file.write_all(&u32::try_from(bytes.len()).unwrap().to_le_bytes())
        .map_err(JournalError::from)?;
    file.write_all(&bytes).map_err(JournalError::from)?;
    file.write_all(&crc64_xz(&bytes).to_le_bytes())
        .map_err(JournalError::from)?;
    Ok(())
}

/// Load the unchanged source bytes, retaining command evidence for refusals.
pub fn model(
    vector: &VectorImage,
    input: &Value,
) -> (DriveHandle, SharedVirtualWorld, FaultEngine) {
    let mut tape = VirtualTape::empty(64 * 1024 * 1024, BLOCK);
    for file in &vector.image.files {
        tape.records.extend(
            file.bytes
                .chunks_exact(BLOCK as usize)
                .map(|b| Record::Block(b.to_vec())),
        );
        if file.filemark_record.is_some() {
            tape.records.push(Record::Filemark);
        }
    }
    tape.written_bytes = vector
        .image
        .files
        .iter()
        .map(|f| f.bytes.len() as u64)
        .sum();
    let append: u64 = input["committed_prefix"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["block_count"].as_u64().unwrap() + 1)
        .sum();
    let lbas: Vec<_> = if input["append_object"].is_null() {
        (append..=vector.image.eod_record as u64 + 16).collect()
    } else {
        vec![]
    };
    let engine = FaultEngine::for_read_medium_errors(lbas).unwrap();
    let mut world = VirtualWorld::single_drive("RESUME-LIB", 0x100, "RESUME-DRV", 0x400, 1);
    world.put_tape_in_drive(0x100, "RESUME001", None, tape);
    let world = Arc::new(Mutex::new(world));
    let transport = ChaosTransport::new(
        ModelTransport::new(Arc::clone(&world), DeviceRole::Drive { bay: 0x100 }),
        engine.clone(),
        DeviceCtx::new().with_backend("model"),
    );
    let drive = DriveHandle::open_standalone_with_transport(
        Path::new("/dev/sg-resume-model"),
        Box::new(transport),
    )
    .unwrap();
    world.lock().unwrap().command_log.clear();
    (drive, world, engine)
}

/// Record actual read addresses while delegating every operation to the drive.
struct ReadTrace<'a> {
    raw: DriveHandleRawSource<'a>,
    lbas: Vec<u64>,
}
impl RawTapeSource for ReadTrace<'_> {
    fn locate_end_of_data(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.raw.locate_end_of_data()
    }
    fn space_filemarks(&mut self, count: i64) -> Result<SpaceFilemarksOutcome, ParityError> {
        self.raw.space_filemarks(count)
    }
    fn configure_fixed_block_size(&mut self, size: u32) -> Result<(), ParityError> {
        self.raw.configure_fixed_block_size(size)
    }
    fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
        self.raw.locate_physical(hint)
    }
    fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
        self.lbas.push(self.raw.position()?.lba);
        self.raw.read_record(buf)
    }
    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.raw.position()
    }
}

/// Write the recorded Object through ordinary format, capacity and parity APIs.
/// Both the library executor and the Layer-5 session test use this write path.
pub fn append_object(
    sink: &mut ParitySink<'_>,
    world: &SharedVirtualWorld,
    input: &Value,
    vector: &VectorImage,
) -> Result<(TapeIndexReplicaObjectRow, Vec<SidecarWriteSummary>), String> {
    use remanence_format::{
        plan_rem_tar_object, write_rem_tar_object_from_readers, RemTarObjectOptions,
    };
    let record = &input["append_object"];
    assert_eq!(
        record["files"],
        json!([]),
        "resume Objects have no payload files"
    );
    let options = &record["options"];
    let mut opts = RemTarObjectOptions::new(
        options["object_id"].as_str().unwrap(),
        options["caller_object_id"].as_str().unwrap(),
        options["write_timestamp"].as_str().unwrap(),
        options["manifest_file_id"].as_str().unwrap(),
    );
    opts.chunk_size = options["chunk_size"].as_u64().unwrap() as usize;
    assert_eq!(options["metadata_preservation"], "archival");
    assert_eq!(options["extensions"], json!({}));
    assert_eq!(
        options["encryption"],
        serde_json::to_value(&opts.encryption).unwrap()
    );
    let layout = plan_rem_tar_object(&opts, &[]).map_err(|e| e.to_string())?;
    let runtime = sink
        .terminal_triple_capacity_runtime_state()
        .map_err(|e| e.to_string())?;
    let reserve = crate::tape_image::capacity_input(
        &vector.written.inputs.scheme,
        BLOCK,
        layout.projected_size_blocks,
        runtime,
        vector.written.inputs.capacity_bytes,
    )?
    .reserve_object()
    .map_err(|e| e.to_string())?;
    let opened = sink
        .begin_object_with_terminal_triple_reservation(reserve)
        .map_err(|e| e.to_string())?;
    let layout =
        write_rem_tar_object_from_readers(sink, &opts, &mut []).map_err(|e| e.to_string())?;
    let closed = sink.finish_object().map_err(|e| e.to_string())?;
    let image = world.lock().unwrap().tapes["RESUME001"].export_image();
    let row = TapeIndexReplicaObjectRow {
        tape_file_number: opened.0,
        stored_block_count: {
            let bytes = image.files[opened.0 as usize].bytes.len();
            assert_eq!(
                bytes % BLOCK as usize,
                0,
                "appended Object is not whole blocks"
            );
            (bytes / BLOCK as usize) as u64
        },
        object_id: opts.object_id.as_bytes().to_vec(),
        representation: ObjectRecoveryRepresentation::Plaintext {
            manifest_first_chunk_lba: layout.manifest.first_chunk_lba.unwrap().0,
            manifest_size_bytes: layout.manifest.size_bytes,
            manifest_chunk_count: layout.manifest.chunk_count,
            manifest_sha256: layout.manifest_sha256,
        },
    };
    let mut sidecars = closed.sidecars_emitted;
    sidecars.extend(
        sink.checkpoint()
            .map_err(|e| e.to_string())?
            .sidecars_emitted,
    );
    Ok((row, sidecars))
}

/// Actual bytes and observations; no expected values enter execution.
pub struct Resumed {
    pub image: remanence_chaos::model::ExportedTapeImage,
    pub actual: Value,
}

/// Generate the positive tape through the bounded library handoff. The closed
/// case is additionally executed through Layer 5 in write_owner's unit tests.
pub fn execute_positive(input: &Value, vector: &VectorImage) -> Result<Resumed, String> {
    let temp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let mut journal =
        adapt(input, vector, &temp.path().join("resume.journal")).map_err(|e| e.to_string())?;
    let snapshot =
        resume_record_result(journal.committed_snapshot_bounded()).map_err(|e| e.to_string())?;
    let summary = checked_bounded_resume_summary(&snapshot).map_err(|e| e.to_string())?;
    let append = summary.append_position.lba;
    let (mut drive, world, _) = model(vector, input);
    let mut trace = ReadTrace {
        raw: DriveHandleRawSource::new(&mut drive),
        lbas: Vec::new(),
    };
    let rebuild = rebuild_open_epoch_from_bounded_summary(
        &mut trace,
        &summary,
        &vector.written.inputs.scheme,
        vector.written.inputs.tape_uuid,
        BLOCK,
    )
    .map_err(|e| e.to_string())?;
    let reads = trace.lbas;
    let live = rebuild.live_epoch;
    let ordinals: Vec<_> = live
        .as_ref()
        .map(|epoch| (epoch.protected_ordinal_start..epoch.next_data_ordinal).collect())
        .unwrap_or_default();
    let result = rebuild
        .plan
        .complete(Vec::new())
        .map_err(|e| e.to_string())?;
    let mut raw = DriveHandleRawSink::new(&mut drive);
    raw.configure_parity_write_session(BLOCK)
        .map_err(|e| e.to_string())?;
    let mut sink = ParitySink::new_sidecar_only_from_bounded_resume(
        &mut raw,
        &mut journal,
        vector.written.inputs.scheme.clone(),
        vector.written.inputs.tape_uuid,
        BLOCK,
        BoundedResumeWriterSeed {
            committed_prefix_snapshot: snapshot,
            committed_prefix_summary: summary,
            resume_result: &result,
            live_epoch: live,
        },
        vector.written.inputs.writer_identity.clone(),
    )
    .map_err(|e| e.to_string())?;
    let (row, sidecars) = append_object(&mut sink, &world, input, vector)?;
    let _state = sink.into_session_state().map_err(|e| e.to_string())?;
    let image = world.lock().unwrap().tapes["RESUME001"].export_image();
    let stored_bytes = image.files[row.tape_file_number as usize].bytes.len();
    assert_eq!(
        stored_bytes % BLOCK as usize,
        0,
        "appended Object is not whole blocks"
    );
    let stored_block_count = (stored_bytes / BLOCK as usize) as u64;
    assert_eq!(stored_block_count, row.stored_block_count);
    for (index, _) in input["committed_prefix"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        assert_eq!(
            image.files[index].bytes, vector.image.files[index].bytes,
            "resume altered committed bytes"
        );
        assert_eq!(
            image.files[index].filemark_record,
            vector.image.files[index].filemark_record
        );
    }
    let mut actual = json!({"accepted":true,"append_lba":append,"appended_object_tape_file":row.tape_file_number,"appended_object_first_lba":image.files[row.tape_file_number as usize].start_record,
        "reread_ordinals":ordinals,"reread_lbas":reads,"appended_object_recovery_row":{"tape_file_number":row.tape_file_number,"stored_block_count":stored_block_count}});
    if input["W"] == input["T"] {
        let scan = scan_reconstruct_filemark_map_with_report(
            &mut DriveHandleRawSource::new(&mut drive),
            &vector.written.inputs.tape_uuid,
            BLOCK,
        )
        .map_err(|e| e.to_string())?;
        let committed_state = journal.load_committed().map_err(|e| e.to_string())?;
        let committed = committed_state.filemark_map().map_err(|e| e.to_string())?;
        assert_eq!(scan.map, committed, "resumed committed prefix must scan");
        actual["torn_records_superseded"] = json!(
            image.files[row.tape_file_number as usize].start_record as u64 == append
                && image.files[row.tape_file_number as usize]
                    .bytes
                    .chunks_exact(BLOCK as usize)
                    .all(|b| b != vec![0xa5; BLOCK as usize])
        );
        actual["open_epoch_ordinals"] = json!((committed_state.highest_protected_ordinal
            ..committed_state.total_committed_ordinals)
            .collect::<Vec<_>>());
    } else {
        let sidecar = sidecars.last().ok_or("no completed sidecar")?;
        let comparison = uninterrupted()?;
        actual["sidecar"] = json!({"tape_file":sidecar.tape_file_number,"first_lba":sidecar.physical_start_lba,"epoch_id":sidecar.epoch_id,"protected_ordinal_start":sidecar.protected_ordinal_start,"protected_ordinal_end_exclusive":sidecar.protected_ordinal_end_exclusive,"total_blocks":sidecar.block_count});
        actual["sidecar_equals_uninterrupted"] = json!(
            image.files[sidecar.tape_file_number as usize].bytes == comparison.files[5].bytes
        );
    }
    Ok(Resumed { image, actual })
}

/// Same three Objects, one builder invocation, no interruption or torn records.
pub fn uninterrupted() -> Result<remanence_chaos::model::ExportedTapeImage, String> {
    let mut source = inputs("unfinalized-open");
    source.objects.push(object(2, 2));
    source.stop = crate::tape_image::TapeImageStop::CommittedPrefix {
        torn_records: 0,
        torn_record_fill: 0xa5,
    };
    Ok(crate::tape_image_vectors::write_model(source)?.1)
}

/// Only keys in the frozen expected object decide a result.
pub fn assert_expected(id: &str, expected: &Value, actual: &Value) {
    let agrees = expected
        .as_object()
        .unwrap()
        .iter()
        .all(|(key, value)| actual.get(key) == Some(value));
    println!(
        "{} {id}: {actual}",
        if agrees { "PASS" } else { "DISAGREEMENT" }
    );
    assert!(agrees, "{id}: expected {expected}; actual {actual}");
}

pub const README: &str = "# Resume review candidates\n\nThe frozen specification-authored expected-cases.json is copied verbatim. Per-case expected.json retains each literal JSON object. Never regenerate expectations from reference behavior. Inputs are the image name, the mutated Section 7.1 committed prefix, W and T, and complete appended Object inputs where applicable. Kinds use their names and absent fields are null.\n\nMANIFEST.tsv pins every resumed tape file and ALL using the parent manifest's convention. Image bytes are regenerated, not checked in. resume-open-uninterrupted is the same three Objects written by write_tape_image in one session, stopped at CommittedPrefix with zero torn records. The generator's --check checks all resume descriptors, digests and the file set.\n\nThe reference adapter frames v4 off-tape journal records from these claims, then uses FileTapeFileJournal replay and checked_bounded_resume_summary. It does not prevalidate or repair negative inputs. Sidecar metadata hashes are taken from the source image; they are reference journal details, not portable authority. The closed case enters write_owner::checkpoint::open_parity_actor_session and resumes its ordinary ParitySink session; the open case uses rebuild_open_epoch_from_bounded_summary and new_sidecar_only_from_bounded_resume. Both append with ordinary capacity reservation, REM-OBJECT writing, finish_object and checkpoint. No test-only committed-prefix planner is used.\n\nRun cargo test -p remanence-cli -p remanence-api --lib resume_vectors -- --nocapture for PASS, INFORMATIVE and DISAGREEMENT lines. Informative annotations never decide a pass. Pinned errors assert the frozen expectations; record-content failures are classified at the production Resumer boundary.\n\n## Commit-record reporting layer\n\nresume-commit-records is not portable: the journal format is implementation-defined. Existing reference coverage is in remanence-parity's journal::tests::shared_bundle_validator_rejects_ambiguous_or_non_dense_shapes, journal::tests::journal_cbor_rejects_duplicate_and_non_integer_keys, journal::tests::file_journal_preserves_torn_trailing_record_and_fails_closed, journal::tests::file_journal_rejects_header_mismatch, journal::tests::bounded_committed_replay_matches_flattened_authority_and_rejects_orphans, and resume::tests::bounded_resume_fails_closed_on_bot_orphan_and_epoch_discontinuity. These do not establish all four required ResumeAppend outcomes before tape positioning.\n\nThe added reference executor tests are resume_vectors::tests::resume_vectors_commit_records_missing (missing Bootstrap record), resume_vectors::tests::resume_vectors_commit_records_conflicting (two different claims for one Object file), resume_vectors::tests::resume_vectors_commit_records_incomplete (torn checkpoint checksum), and resume_vectors::tests::resume_vectors_commit_records_ambiguous (duplicate Object authority). Each runs journal replay and bounded validation, asserts an empty chaos command log before refusal, and asserts the frozen ResumeAppend error. They are shared with the Layer-5 test build. resume_vectors::tests::resume_vectors_commit_records_checkpoint_w_conflict and resume_vectors_commit_records_checkpoint_t_conflict give a checkpoint marker whose W or T disagrees with the preceding bundle, which reaches the watermark validator rather than the density check. The Layer-5 tests resume_vectors_cross_record_conflict and resume_vectors_empty_checkpoint_authority cover the cross-record checks (the checkpoint journal against the parity journal, and a committed prefix with no checkpoint authority), which report ResumeAppend through one shared helper.\n\nResolved by REM-PARITY §14 step 2 and §3.4: resume-w-greater-than-t and incomplete commit records are ResumeAppend refusals. The shared production resume_record_result boundary preserves journal diagnostics and leaves operational failures unchanged. Both are refused before tape reads or writes; the executor uses that same boundary.\n";

/// Read the emitted portable input; missing fixtures are a hard test failure.
pub fn recorded_input(id: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/rem-parity-terminal-index-draft/tape-images/resume")
        .join(id)
        .join("inputs.json");
    serde_json::from_slice(&std::fs::read(path).expect("generated resume input must exist"))
        .expect("valid portable resume input")
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tape_image_vectors::generate;

    /// Replay malformed off-tape authority through the same adapter and bounded
    /// validator, proving refusal before even positioning the chaos drive.
    fn commit_record_refusal(kind: &str) {
        let vector = generate("unfinalized-closed").unwrap();
        let input = recorded_input("resume-closed");
        let (mut drive, world, _) = model(&vector, &input);
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("resume.journal");
        let result = (|| -> Result<(), ParityError> {
            let journal = adapt(&input, &vector, &path)?;
            // Mutate reference record frames, preserving every unchanged claim.
            // Ambiguity is an exact duplicate frame, including its physical hint.
            let bytes = std::fs::read(&path).unwrap();
            let header_len = 41 + vector.written.inputs.scheme.id.as_str().len() + 8;
            let mut cursor = header_len;
            let mut frames = Vec::new();
            while cursor < bytes.len() {
                let size =
                    u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap()) as usize;
                let end = cursor + size + 12;
                frames.push(bytes[cursor..end].to_vec());
                cursor = end;
            }
            assert_eq!(frames.len(), 4, "Bootstrap, Object, sidecar, checkpoint");
            match kind {
                "missing" => {
                    frames.remove(0);
                }
                "ambiguous" => frames.insert(2, frames[1].clone()),
                "conflicting" => {
                    let mut value: Cbor =
                        ciborium::from_reader(&frames[1][4..frames[1].len() - 8]).unwrap();
                    let Cbor::Map(bundle) = &mut value else {
                        panic!("bundle map")
                    };
                    let Cbor::Array(entries) = &mut bundle[1].1 else {
                        panic!("bundle entries")
                    };
                    let Cbor::Map(row) = &mut entries[0] else {
                        panic!("Object row")
                    };
                    assert_eq!(row[2].0, Cbor::Integer(3.into()));
                    row[2].1 = Cbor::Integer(3.into());
                    let mut payload = Vec::new();
                    ciborium::into_writer(&value, &mut payload).unwrap();
                    let mut frame = u32::try_from(payload.len()).unwrap().to_le_bytes().to_vec();
                    frame.extend_from_slice(&payload);
                    frame.extend_from_slice(&crc64_xz(&payload).to_le_bytes());
                    frames.insert(2, frame);
                }
                "checkpoint-w" | "checkpoint-t" => {
                    // Keep the dense tape-file entries unchanged. Only the marker's
                    // watermark contradicts its preceding bundle.
                    let frame = frames.last_mut().unwrap();
                    let mut value: Cbor =
                        ciborium::from_reader(&frame[4..frame.len() - 8]).unwrap();
                    let Cbor::Map(bundle) = &mut value else {
                        panic!("checkpoint bundle map")
                    };
                    let key = if kind == "checkpoint-w" { 3 } else { 4 };
                    let (_, watermark) = bundle
                        .iter_mut()
                        .find(|(k, _)| *k == Cbor::Integer(key.into()))
                        .unwrap();
                    *watermark = Cbor::Integer(if key == 3 { 3.into() } else { 5.into() });
                    let mut payload = Vec::new();
                    ciborium::into_writer(&value, &mut payload).unwrap();
                    *frame = u32::try_from(payload.len()).unwrap().to_le_bytes().to_vec();
                    frame.extend_from_slice(&payload);
                    frame.extend_from_slice(&crc64_xz(&payload).to_le_bytes());
                }
                "incomplete" => {
                    frames.last_mut().unwrap().pop().unwrap();
                }
                _ => panic!("unknown commit-record mutation"),
            }
            let mut mutated = bytes[..header_len].to_vec();
            mutated.extend(frames.into_iter().flatten());
            std::fs::write(&path, mutated).unwrap();
            let snapshot = resume_record_result(journal.committed_snapshot_bounded())?;
            let summary = checked_bounded_resume_summary(&snapshot)?;
            rebuild_open_epoch_from_bounded_summary(
                &mut DriveHandleRawSource::new(&mut drive),
                &summary,
                &vector.written.inputs.scheme,
                vector.written.inputs.tape_uuid,
                BLOCK,
            )?;
            Ok(())
        })();
        assert!(
            world.lock().unwrap().command_log.is_empty(),
            "commit-record refusal positioned or wrote tape"
        );
        let error = result.expect_err("bad commit records must refuse");
        if kind.starts_with("checkpoint-") {
            assert!(
                error
                    .to_string()
                    .contains("must equal preceding journal state (4/4)"),
                "{error}"
            );
            assert!(!error.to_string().contains("not dense"), "{error}");
        }
        let expected: Value = serde_json::from_str(EXPECTATIONS).unwrap();
        let case = expected["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == "resume-commit-records")
            .unwrap();
        assert_eq!(case["expected"]["error"], "ResumeAppend");
        let agrees = matches!(error, ParityError::ResumeAppend(_));
        println!(
            "{} resume-commit-records/{kind}: {error:?}; before_positioning_or_writing=true",
            if agrees { "PASS" } else { "DISAGREEMENT" }
        );
        assert!(
            agrees,
            "resume-commit-records/{kind}: expected ResumeAppend; actual {error:?}"
        );
    }

    #[test]
    fn resume_vectors_commit_records_checkpoint_w_conflict() {
        commit_record_refusal("checkpoint-w");
    }
    #[test]
    fn resume_vectors_commit_records_checkpoint_t_conflict() {
        commit_record_refusal("checkpoint-t");
    }

    #[test]
    fn resume_vectors_commit_records_missing() {
        commit_record_refusal("missing");
    }
    #[test]
    fn resume_vectors_commit_records_conflicting() {
        commit_record_refusal("conflicting");
    }
    #[test]
    fn resume_vectors_commit_records_incomplete() {
        commit_record_refusal("incomplete");
    }
    #[test]
    fn resume_vectors_commit_records_ambiguous() {
        commit_record_refusal("ambiguous");
    }

    #[test]
    fn resume_vectors_open() {
        let source: Value = serde_json::from_str(EXPECTATIONS).unwrap();
        let case = source["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == "resume-open")
            .unwrap();
        let vector = generate("unfinalized-open").unwrap();
        let input = recorded_input("resume-open");
        let result = execute_positive(&input, &vector).unwrap();
        assert_expected("resume-open", &case["expected"], &result.actual);
        println!("INFORMATIVE resume-open: {}", case["informative"]);
    }

    #[test]
    fn resume_vectors_portable_refusals() {
        let source: Value = serde_json::from_str(EXPECTATIONS).unwrap();
        let mut disagreements = Vec::new();
        for case in source["cases"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["expected"]["accepted"] == false && c["portable"] != false)
        {
            let id = case["id"].as_str().unwrap();
            let vector = generate(case["image"].as_str().unwrap()).unwrap();
            let input = recorded_input(id);
            let temp = tempfile::tempdir().unwrap();
            let (mut drive, world, engine) = model(&vector, &input);
            let mut trace = ReadTrace {
                raw: DriveHandleRawSource::new(&mut drive),
                lbas: Vec::new(),
            };
            let actual = (|| -> Result<(), ParityError> {
                let journal = adapt(&input, &vector, &temp.path().join("resume.journal"))?;
                let snapshot = resume_record_result(journal.committed_snapshot_bounded())?;
                let summary = checked_bounded_resume_summary(&snapshot)?;
                rebuild_open_epoch_from_bounded_summary(
                    &mut trace,
                    &summary,
                    &vector.written.inputs.scheme,
                    vector.written.inputs.tape_uuid,
                    BLOCK,
                )?;
                Ok(())
            })();
            let no_write = world
                .lock()
                .unwrap()
                .command_log
                .iter()
                .all(|c| !matches!(c.opcode, 0x0a | 0x10 | 0x8a | 0x80));
            assert!(no_write, "{id}: wrote after a refusal");
            if case["expected"]["before_any_read_or_write"] == true {
                assert!(
                    world
                        .lock()
                        .unwrap()
                        .command_log
                        .iter()
                        .all(|c| !matches!(c.opcode, 0x08 | 0x88)),
                    "{id}: unexpected read"
                );
                assert!(engine.observed_medium_error_lbas().is_empty());
            }
            if case["expected"]["fatal"] == true {
                let last_file = input["committed_prefix"]
                    .as_array()
                    .unwrap()
                    .last()
                    .unwrap()["tape_file_number"]
                    .as_u64()
                    .unwrap() as usize;
                assert_eq!(
                    trace.lbas.last().copied(),
                    vector.image.files[last_file]
                        .filemark_record
                        .map(|n| n as u64),
                    "fatal refusal must reach the boundary read"
                );
            }
            let error = actual.expect_err("negative must refuse");
            let agrees = case["expected"]["error"].is_null()
                || matches!(error, ParityError::ResumeAppend(_));
            println!(
                "{} {id}: {error:?}; no_write={no_write}; reread_lbas={:?}",
                if agrees { "PASS" } else { "DISAGREEMENT" },
                trace.lbas
            );
            if let Some(info) = case["informative"].as_str() {
                println!("INFORMATIVE {id}: {info}; reference={error:?}");
            }
            if !agrees {
                disagreements.push(format!(
                    "{id}: pinned ResumeAppend; reference returned {error:?}"
                ));
            }
        }
        assert!(disagreements.is_empty(), "{}", disagreements.join("\n"));
    }
}
