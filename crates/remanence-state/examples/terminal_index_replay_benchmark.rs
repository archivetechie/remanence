//! Reproducible bounded-replay benchmark for terminal tape-index authority.
//!
//! The benchmark builds a valid parity-off checkpoint journal, freezes the
//! exact committed prefix, and performs the same structural-row/Object-row
//! replay pair used for each terminal replica. It reports deterministic live
//! allocation bounds from the journal decoder alongside measured throughput.

use std::error::Error;
use std::hint::black_box;
use std::time::Instant;

use remanence_parity::TapeIndexReplicaRecordSource;
use remanence_state::{
    CheckpointJournalRecord, CheckpointObjectProjection, CheckpointObjectRecoveryRepresentation,
    CheckpointObjectRecoveryRow, CheckpointTerminalIndexRecordSource, FileCheckpointJournal,
    NativeObjectCopyProjectionInput, NativeObjectProjectionInput,
};
use serde_json::json;

const BLOCK_SIZE: u32 = 256 * 1024;
const OBJECT_BLOCKS: u64 = 3;
const DEFAULT_OBJECT_COUNT: u64 = 1_024;
const DEFAULT_REPLICA_PASSES: u64 = 3;
const SETUP_BATCH_SIZE: u64 = 128;

fn checked_arg(args: &[String], index: usize, default: u64, name: &str) -> Result<u64, String> {
    let value = match args.get(index) {
        Some(raw) => raw
            .parse::<u64>()
            .map_err(|error| format!("invalid {name} {raw:?}: {error}"))?,
        None => default,
    };
    if value == 0 {
        return Err(format!("{name} must be positive"));
    }
    Ok(value)
}

fn checkpoint_record(
    tape_uuid: [u8; 16],
    zero_based_index: u64,
) -> Result<CheckpointJournalRecord, String> {
    let ordinal = zero_based_index
        .checked_add(1)
        .ok_or_else(|| "checkpoint ordinal overflow".to_string())?;
    let object_tape_file_number = ordinal;
    let next_tape_file_number = ordinal
        .checked_add(1)
        .ok_or_else(|| "next tape-file number overflow".to_string())?;
    let total_committed_ordinals = ordinal
        .checked_mul(OBJECT_BLOCKS)
        .ok_or_else(|| "Object ordinal count overflow".to_string())?;
    let eod_lba = ordinal
        .checked_mul(OBJECT_BLOCKS.checked_add(1).expect("constant fits"))
        .and_then(|object_prefix| object_prefix.checked_add(2))
        .ok_or_else(|| "checkpoint EOD LBA overflow".to_string())?;
    let object_uuid = uuid::Uuid::from_u128(u128::from(ordinal));
    let object_id = object_uuid.to_string();

    Ok(CheckpointJournalRecord {
        ordinal,
        committed_object_count: ordinal,
        eod_partition: 0,
        eod_lba,
        tape_uuid,
        batch_id: u128::from(ordinal).to_be_bytes(),
        next_tape_file_number,
        block_size: BLOCK_SIZE,
        objects: vec![CheckpointObjectProjection {
            object: NativeObjectProjectionInput {
                object_id: object_id.clone(),
                caller_object_id: Some(format!("terminal-benchmark-{ordinal}")),
                body_format: "rem-object-v1".to_string(),
                logical_size_bytes: Some(1),
                content_hash: Some(vec![0x11; 32]),
                metadata_hash: Some(vec![0x22; 32]),
                created_at_utc: Some("2026-08-09T00:00:00Z".to_string()),
            },
            files: Vec::new(),
            copy: NativeObjectCopyProjectionInput {
                object_id: object_id.clone(),
                tape_uuid,
                tape_file_number: object_tape_file_number,
                first_body_lba: 0,
                first_parity_data_ordinal: None,
                protected_until_ordinal: None,
                status: "committed".to_string(),
                representation: "plaintext".to_string(),
                recipient_epoch_ids: None,
                metadata_frame_len: None,
                plaintext_digest: Some(vec![0x33; 32]),
                stored_digest: Some(vec![0x33; 32]),
            },
            block_size: BLOCK_SIZE,
            block_count: OBJECT_BLOCKS,
            fresh_tape: ordinal == 1,
            total_committed_ordinals,
            object_recovery_row: CheckpointObjectRecoveryRow {
                tape_file_number: object_tape_file_number,
                stored_block_count: OBJECT_BLOCKS,
                object_id: object_id.into_bytes(),
                representation: CheckpointObjectRecoveryRepresentation::Plaintext {
                    manifest_first_chunk_lba: 1,
                    manifest_size_bytes: 1,
                    manifest_chunk_count: 1,
                    manifest_sha256: [0x44; 32],
                },
            },
        }],
        scheme: None,
        object_tape_file_bundles: Vec::new(),
        barrier_bundle: None,
        terminal_finalization: None,
        sealed_after_write: false,
    })
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let object_count = checked_arg(&args, 0, DEFAULT_OBJECT_COUNT, "object count")?;
    let replica_passes = checked_arg(&args, 1, DEFAULT_REPLICA_PASSES, "replica pass count")?;
    if args.len() > 2 {
        return Err("usage: terminal_index_replay_benchmark [OBJECTS] [REPLICA_PASSES]".into());
    }

    let directory = tempfile::tempdir()?;
    let tape_uuid = [0xB7; 16];
    let journal = FileCheckpointJournal::open(directory.path(), tape_uuid)?;
    let mut lease = journal.acquire_exclusive()?;
    let mut next_index = 0u64;
    while next_index < object_count {
        let batch_end = next_index
            .checked_add(SETUP_BATCH_SIZE)
            .ok_or("benchmark batch boundary overflow")?
            .min(object_count);
        let mut batch = Vec::with_capacity(usize::try_from(batch_end - next_index)?);
        for zero_based_index in next_index..batch_end {
            batch.push(checkpoint_record(tape_uuid, zero_based_index)?);
        }
        lease.append_batch(&batch)?;
        next_index = batch_end;
    }

    let setup_started = Instant::now();
    let mut source = CheckpointTerminalIndexRecordSource::new_replay_backed_no_parity(&lease)?;
    let setup_elapsed = setup_started.elapsed();
    let summary = source.summary();
    let metrics = source
        .replay_metrics()
        .ok_or("replay-backed source did not expose replay metrics")?;

    let mut checksum = 0u64;
    let replay_started = Instant::now();
    for _ in 0..replica_passes {
        TapeIndexReplicaRecordSource::visit_structural_entries(&mut source, &mut |entry| {
            checksum ^= entry.tape_file_number.rotate_left(7) ^ entry.block_count;
            Ok(())
        })?;
        TapeIndexReplicaRecordSource::visit_object_rows(&mut source, &mut |row| {
            checksum ^= row.tape_file_number.rotate_left(13) ^ row.stored_block_count;
            Ok(())
        })?;
    }
    black_box(checksum);
    let replay_elapsed = replay_started.elapsed();

    let emitted_rows_per_replica = summary
        .counts
        .structural_entry_count
        .checked_add(summary.counts.object_row_count)
        .ok_or("emitted row count overflow")?;
    let emitted_rows = emitted_rows_per_replica
        .checked_mul(replica_passes)
        .ok_or("total emitted row count overflow")?;
    let canonical_bytes_per_replica = summary
        .counts
        .structural_entry_count
        .checked_mul(64)
        .and_then(|value| {
            summary
                .counts
                .object_row_count
                .checked_mul(256)
                .and_then(|rows| value.checked_add(rows))
        })
        .ok_or("canonical byte count overflow")?;
    let canonical_bytes = canonical_bytes_per_replica
        .checked_mul(replica_passes)
        .ok_or("total canonical byte count overflow")?;
    let replay_seconds = replay_elapsed.as_secs_f64();
    let setup_authority_passes = metrics
        .checkpoint
        .replay_passes
        .checked_add(1)
        .ok_or("setup pass count overflow")?;
    let timed_authority_passes = replica_passes
        .checked_mul(2)
        .ok_or("timed pass count overflow")?;

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "schema": "rem.terminal-index-replay-benchmark.v1",
            "object_count": object_count.to_string(),
            "structural_entry_count": summary.counts.structural_entry_count.to_string(),
            "object_row_count": summary.counts.object_row_count.to_string(),
            "replica_passes": replica_passes.to_string(),
            "journal_setup_batch_size": SETUP_BATCH_SIZE.to_string(),
            "setup_authority_passes": setup_authority_passes.to_string(),
            "timed_authority_passes": timed_authority_passes.to_string(),
            "total_authority_passes": setup_authority_passes
                .checked_add(timed_authority_passes)
                .ok_or("total pass count overflow")?
                .to_string(),
            "checkpoint_frames": metrics.checkpoint.frame_count.to_string(),
            "peak_frame_payload_bytes": metrics.checkpoint.peak_frame_payload_bytes.to_string(),
            "peak_live_checkpoint_records": metrics.checkpoint.peak_live_record_count.to_string(),
            "peak_live_object_rows": metrics.checkpoint.peak_live_object_rows.to_string(),
            "setup_seconds": setup_elapsed.as_secs_f64(),
            "replay_seconds": replay_seconds,
            "emitted_rows": emitted_rows.to_string(),
            "rows_per_second": if replay_seconds > 0.0 {
                emitted_rows as f64 / replay_seconds
            } else {
                0.0
            },
            "canonical_mib_per_second": if replay_seconds > 0.0 {
                canonical_bytes as f64 / (1024.0 * 1024.0) / replay_seconds
            } else {
                0.0
            },
            "checksum": checksum.to_string(),
        }))?
    );
    Ok(())
}
