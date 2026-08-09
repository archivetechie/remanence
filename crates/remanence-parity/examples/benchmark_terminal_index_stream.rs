//! Benchmark replayable terminal-index planning and three-replica emission.
//!
//! This executable synthesizes representative and high-count row authorities
//! one row at a time, retaining no structural or Object rows. It records
//! elapsed time, throughput, replay passes, emitted geometry, and Linux peak
//! RSS when `/proc/self/status` exposes `VmHWM`. Results are observational and
//! deliberately separate from pinned conformance fixtures.

use std::env;
use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use remanence_parity::{
    checked_tape_index_replica_layout, plan_tape_index_edition, plan_tape_index_replica,
    write_tape_index_replica, ObjectRecoveryRepresentation, ParityError,
    TapeIndexEditionDescriptor, TapeIndexReplicaCounts, TapeIndexReplicaFileKind,
    TapeIndexReplicaMapEntry, TapeIndexReplicaObjectRow, TapeIndexReplicaObservation,
    TapeIndexReplicaRecordSource, TapeIndexReplicaScope, TerminalTailLayout,
};

const DEFAULT_REPRESENTATIVE_ROWS: u64 = 10_000;
const DEFAULT_HIGH_COUNT_ROWS: u64 = 1_000_000;
const BLOCK_SIZE: u32 = 256 * 1024;
const COMPACT_GAP_RECORDS: u64 = 3;
const REPORT_SCHEMA: &str = "rem.parity.terminal-index-stream-benchmark.v1";

#[derive(Debug)]
struct Args {
    representative_rows: u64,
    high_count_rows: u64,
    report: Option<PathBuf>,
}

#[derive(Debug)]
struct SyntheticRecords {
    object_rows: u64,
    structural_passes: u64,
    object_passes: u64,
}

impl TapeIndexReplicaRecordSource for SyntheticRecords {
    fn visit_structural_entries(
        &mut self,
        visitor: &mut dyn FnMut(&TapeIndexReplicaMapEntry) -> Result<(), ParityError>,
    ) -> Result<(), ParityError> {
        self.structural_passes =
            self.structural_passes
                .checked_add(1)
                .ok_or(ParityError::Invariant(
                    "benchmark structural pass count overflows",
                ))?;
        visitor(&TapeIndexReplicaMapEntry {
            tape_file_number: 0,
            kind: TapeIndexReplicaFileKind::Bootstrap,
            block_count: 1,
            first_parity_data_ordinal: None,
            protected_ordinal_start: None,
            protected_ordinal_end_exclusive: None,
            epoch_id: None,
        })?;
        for ordinal in 0..self.object_rows {
            visitor(&TapeIndexReplicaMapEntry {
                tape_file_number: ordinal.checked_add(1).ok_or(ParityError::Invariant(
                    "benchmark tape-file number overflows",
                ))?,
                kind: TapeIndexReplicaFileKind::Object,
                block_count: 1,
                first_parity_data_ordinal: Some(ordinal),
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                epoch_id: None,
            })?;
        }
        Ok(())
    }

    fn visit_object_rows(
        &mut self,
        visitor: &mut dyn FnMut(&TapeIndexReplicaObjectRow) -> Result<(), ParityError>,
    ) -> Result<(), ParityError> {
        self.object_passes = self
            .object_passes
            .checked_add(1)
            .ok_or(ParityError::Invariant(
                "benchmark Object pass count overflows",
            ))?;
        for ordinal in 0..self.object_rows {
            visitor(&TapeIndexReplicaObjectRow {
                tape_file_number: ordinal.checked_add(1).ok_or(ParityError::Invariant(
                    "benchmark Object tape-file number overflows",
                ))?,
                stored_block_count: 1,
                object_id: b"benchmark-object".to_vec(),
                representation: ObjectRecoveryRepresentation::Plaintext {
                    manifest_first_chunk_lba: 0,
                    manifest_size_bytes: 1,
                    manifest_chunk_count: 1,
                    manifest_sha256: [0x51; 32],
                },
            })?;
        }
        Ok(())
    }
}

#[derive(Debug)]
struct ProfileReport {
    name: &'static str,
    object_rows: u64,
    structural_rows: u64,
    payload_bytes: u64,
    payload_records: u64,
    replica_records: u64,
    emitted_blocks: u64,
    emitted_bytes: u64,
    record_source_pass_count: u64,
    structural_passes: u64,
    object_passes: u64,
    retained_structural_rows: u64,
    retained_object_rows: u64,
    elapsed: Duration,
    throughput_mib_per_second: f64,
    row_visits_per_second: f64,
    peak_rss_bytes: Option<u64>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args()?;
    let representative = run_profile("representative", args.representative_rows, 0x31)?;
    let high_count = run_profile("high_count", args.high_count_rows, 0x41)?;
    let report = render_report(&[representative, high_count]);
    print!("{report}");
    if let Some(path) = args.report {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, report)?;
    }
    Ok(())
}

fn parse_args() -> Result<Args, Box<dyn std::error::Error>> {
    let mut representative_rows = DEFAULT_REPRESENTATIVE_ROWS;
    let mut high_count_rows = DEFAULT_HIGH_COUNT_ROWS;
    let mut report = None;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--representative-rows" => {
                representative_rows = parse_positive_rows(
                    args.next()
                        .ok_or("--representative-rows requires a value")?,
                    "--representative-rows",
                )?;
            }
            "--high-count-rows" => {
                high_count_rows = parse_positive_rows(
                    args.next().ok_or("--high-count-rows requires a value")?,
                    "--high-count-rows",
                )?;
            }
            "--report" => {
                let value = args.next().ok_or("--report requires a path")?;
                if value.is_empty() {
                    return Err("--report path must not be empty".into());
                }
                report = Some(PathBuf::from(value));
            }
            "--help" | "-h" => {
                println!(
                    "Usage: benchmark_terminal_index_stream [--representative-rows N] \
                     [--high-count-rows N] [--report PATH]"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}").into()),
        }
    }
    if high_count_rows <= representative_rows {
        return Err("--high-count-rows must exceed --representative-rows".into());
    }
    Ok(Args {
        representative_rows,
        high_count_rows,
        report,
    })
}

fn parse_positive_rows(value: String, flag: &str) -> Result<u64, Box<dyn std::error::Error>> {
    let rows = value
        .parse::<u64>()
        .map_err(|error| format!("{flag} is not a u64: {error}"))?;
    if rows == 0 {
        return Err(format!("{flag} must be positive").into());
    }
    Ok(rows)
}

fn run_profile(
    name: &'static str,
    object_rows: u64,
    identity_byte: u8,
) -> Result<ProfileReport, Box<dyn std::error::Error>> {
    let structural_rows = object_rows
        .checked_add(1)
        .ok_or("benchmark structural row count overflows")?;
    let counts = TapeIndexReplicaCounts {
        structural_entry_count: structural_rows,
        object_row_count: object_rows,
    };
    let replica_layout = checked_tape_index_replica_layout(BLOCK_SIZE, counts)?;
    let prefix_end_lba = object_rows
        .checked_mul(2)
        .and_then(|value| value.checked_add(2))
        .ok_or("benchmark prefix end LBA overflows")?;
    let terminal_layout = TerminalTailLayout::new(
        0,
        BLOCK_SIZE,
        structural_rows,
        prefix_end_lba,
        replica_layout.replica_record_count,
        COMPACT_GAP_RECORDS,
    )?;
    let descriptor = TapeIndexEditionDescriptor {
        tape_uuid: [identity_byte; 16],
        edition_id: [identity_byte.wrapping_add(1); 16],
        edition_sequence: 1,
        scope: TapeIndexReplicaScope {
            covered_prefix_tape_file_count: structural_rows,
            total_data_ordinals: object_rows,
            highest_protected_ordinal: 0,
        },
        counts,
        block_size: BLOCK_SIZE,
        compression_enabled: false,
        writer_version: "terminal-index-stream-benchmark/1".to_string(),
        write_timestamp: "2026-08-09T00:00:00Z".to_string(),
        terminal_layout,
    };
    let mut source = SyntheticRecords {
        object_rows,
        structural_passes: 0,
        object_passes: 0,
    };
    let started = Instant::now();
    let edition = plan_tape_index_edition(descriptor, &mut source)?;
    let mut emitted_blocks = 0u64;
    let mut emitted_bytes = 0u64;
    for ordinal in 1..=3 {
        let plan = plan_tape_index_replica(edition.clone(), ordinal)?;
        let observation = TapeIndexReplicaObservation {
            tape_file_number: plan.component.planned_tape_file_number,
            start_lba: plan.component.planned_start_lba,
            record_count: plan.component.record_count,
        };
        write_tape_index_replica(&plan, observation, &mut source, |block| {
            black_box(block);
            emitted_blocks = emitted_blocks.checked_add(1).ok_or(ParityError::Invariant(
                "benchmark emitted block count overflows",
            ))?;
            emitted_bytes = emitted_bytes
                .checked_add(u64::try_from(block.len()).map_err(|_| {
                    ParityError::Invariant("benchmark block length does not fit u64")
                })?)
                .ok_or(ParityError::Invariant(
                    "benchmark emitted byte count overflows",
                ))?;
            Ok(())
        })?;
    }
    let elapsed = started.elapsed();
    if elapsed.is_zero() {
        return Err("benchmark clock did not advance".into());
    }
    let expected_blocks = edition
        .replica_layout
        .replica_record_count
        .checked_mul(3)
        .ok_or("benchmark expected block count overflows")?;
    if emitted_blocks != expected_blocks
        || source.structural_passes != source.object_passes
        || source.structural_passes != 4
    {
        return Err(format!(
            "benchmark replay mismatch: emitted {emitted_blocks}/{expected_blocks} blocks, structural/object passes {}/{}",
            source.structural_passes, source.object_passes
        )
        .into());
    }
    let seconds = elapsed.as_secs_f64();
    let throughput_mib_per_second = emitted_bytes as f64 / (1024.0 * 1024.0) / seconds;
    let row_visits = structural_rows
        .checked_add(object_rows)
        .and_then(|rows| rows.checked_mul(source.structural_passes))
        .ok_or("benchmark row-visit count overflows")?;
    Ok(ProfileReport {
        name,
        object_rows,
        structural_rows,
        payload_bytes: edition.replica_layout.payload_len,
        payload_records: edition.replica_layout.payload_record_count,
        replica_records: edition.replica_layout.replica_record_count,
        emitted_blocks,
        emitted_bytes,
        record_source_pass_count: source.structural_passes,
        structural_passes: source.structural_passes,
        object_passes: source.object_passes,
        retained_structural_rows: 0,
        retained_object_rows: 0,
        elapsed,
        throughput_mib_per_second,
        row_visits_per_second: row_visits as f64 / seconds,
        peak_rss_bytes: linux_peak_rss_bytes(),
    })
}

fn linux_peak_rss_bytes() -> Option<u64> {
    let status = fs::read_to_string(Path::new("/proc/self/status")).ok()?;
    let kibibytes = status.lines().find_map(|line| {
        let value = line.strip_prefix("VmHWM:")?.trim();
        value.strip_suffix("kB")?.trim().parse::<u64>().ok()
    })?;
    kibibytes.checked_mul(1024)
}

fn render_report(profiles: &[ProfileReport]) -> String {
    let mut output = format!(
        "{{\n  \"schema\": \"{REPORT_SCHEMA}\",\n  \"profile_count\": {},\n  \"elapsed_scope\": \"edition_plan_plus_three_replica_emissions\",\n  \"throughput_basis\": \"padded_emitted_replica_bytes\",\n  \"peak_rss_scope\": \"process_high_water_mark_at_profile_end\",\n  \"profiles\": [\n",
        profiles.len()
    );
    for (index, profile) in profiles.iter().enumerate() {
        let peak_rss = profile
            .peak_rss_bytes
            .map_or_else(|| "null".to_string(), |value| value.to_string());
        output.push_str(&format!(
            "    {{\n      \"name\": \"{}\",\n      \"block_size_bytes\": {BLOCK_SIZE},\n      \"object_rows\": {},\n      \"structural_rows\": {},\n      \"payload_bytes\": {},\n      \"payload_records\": {},\n      \"replica_records\": {},\n      \"replica_count\": 3,\n      \"emitted_blocks\": {},\n      \"emitted_bytes\": {},\n      \"elapsed_seconds\": {:.6},\n      \"throughput_mib_per_second\": {:.3},\n      \"row_visits_per_second\": {:.3},\n      \"record_source_pass_count\": {},\n      \"structural_passes\": {},\n      \"object_passes\": {},\n      \"retained_structural_rows\": {},\n      \"retained_object_rows\": {},\n      \"peak_rss_bytes\": {peak_rss},\n      \"peak_rss_source\": {}\n    }}{}\n",
            profile.name,
            profile.object_rows,
            profile.structural_rows,
            profile.payload_bytes,
            profile.payload_records,
            profile.replica_records,
            profile.emitted_blocks,
            profile.emitted_bytes,
            profile.elapsed.as_secs_f64(),
            profile.throughput_mib_per_second,
            profile.row_visits_per_second,
            profile.record_source_pass_count,
            profile.structural_passes,
            profile.object_passes,
            profile.retained_structural_rows,
            profile.retained_object_rows,
            if profile.peak_rss_bytes.is_some() {
                "\"linux_proc_status_vm_hwm\""
            } else {
                "null"
            },
            if index + 1 == profiles.len() { "" } else { "," },
        ));
    }
    output.push_str("  ]\n}\n");
    output
}
