//! Read-only tape throughput measurements with forward filemark crossings.
//! Timings use host command completions; no fallback seek or record retry occurs.

use super::*;
#[cfg(target_os = "linux")]
use remanence_library::SpaceKind;
#[cfg(target_os = "linux")]
use std::time::Instant;

#[derive(Args, Debug)]
pub(super) struct TapeReadRateArgs {
    #[command(flatten)]
    pub(super) address: TapeBotProbeArgs,
    /// Zero-based tape file number (file 0 contains the Bootstrap).
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(0..=i32::MAX as i64))]
    from_file: u32,
    /// Number of tape files to measure.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    files: u32,
    /// Read records through filemarks, or space over files.
    #[arg(long, value_enum)]
    mode: Mode,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Read,
    Space,
}

impl TapeReadRateArgs {
    pub(super) fn validate_before_discovery(&self) -> Result<(), String> {
        self.address.validate_before_discovery()?;
        if !matches!(
            parse_tape_init_target(&self.address.target)?,
            TapeInitTarget::Voltag(_)
        ) {
            return Err("tape read-rate requires one exact barcode".into());
        }
        self.from_file
            .checked_add(self.files)
            .ok_or("tape file range overflows")?;
        Ok(())
    }
}

/// Acquire only the state lock; opening a state handle would also open journals.
pub(super) fn run(args: &TapeReadRateArgs, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let result = (|| {
        let config =
            remanence_state::load_config(&args.address.config).map_err(|e| e.to_string())?;
        let paths = remanence_state::StatePaths::from_config(&args.address.config, &config);
        let _lock = remanence_state::StateLockGuard::acquire(&paths.state_dir)
            .map_err(|e| e.to_string())?;
        run_locked(args, &config)
    })();
    write_report(result, out, err)
}

/// Measurement outcomes exit successfully; command and cleanup errors do not.
fn write_report(
    result: Result<Value, String>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let (value, code) = match result {
        Ok(value) => {
            let code = if value.get("cleanup_error").is_some() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            };
            (value, code)
        }
        Err(detail) => {
            let _ = writeln!(err, "error: {detail}");
            (
                json!({"schema": "rem.tape.read-rate.v1", "ok": false, "outcome": "command_error", "detail": detail}),
                ExitCode::FAILURE,
            )
        }
    };
    let _ = writeln!(out, "{value}");
    code
}

#[cfg(not(target_os = "linux"))]
fn run_locked(
    _args: &TapeReadRateArgs,
    _config: &remanence_state::RemConfig,
) -> Result<Value, String> {
    Err("tape read-rate requires Linux SG_IO access".into())
}

#[cfg(target_os = "linux")]
fn run_locked(
    args: &TapeReadRateArgs,
    config: &remanence_state::RemConfig,
) -> Result<Value, String> {
    let report = fresh_bot_identity_discovery()?;
    let (candidate, library) = resolve_bot_identity_command(
        &report,
        config,
        Some(&args.address.library),
        &args.address.target,
    )?;
    let generation = candidate
        .voltag
        .as_deref()
        .and_then(remanence_api::MediaObservation::label_hint)
        .ok_or("barcode has no known LTO generation suffix")?
        .generation;
    let bay = select_read_compatible_drive(library, &candidate, generation)?;
    let mut measured = None;
    let result = with_bot_identity_drive(
        library,
        &configured_library_policy(config),
        candidate,
        bay,
        args.address.expected_home_slot,
        |drive, identity| {
            with_temporary_bot_read_config(drive, |drive| {
                let classification =
                    remanence_api::probe_bot_identity_from_source(&mut DriveHandleSource(drive));
                let remanence_api::BotIdentityClassification::ValidRemBootstrap { payload, .. } =
                    classification
                else {
                    return Err(format!(
                        "read-rate requires a valid Bootstrap: {classification:?}"
                    ));
                };
                let mut mode = drive.bot_read_config().map_err(|e| e.to_string())?;
                mode.block_size = BlockSize::Fixed {
                    size_bytes: payload.block_size_bytes,
                };
                drive.bot_write_config(mode).map_err(|e| e.to_string())?;
                let checked = drive.bot_read_config().map_err(|e| e.to_string())?;
                if checked.block_size != mode.block_size || checked.compression != mode.compression
                {
                    return Err("drive does not retain the tape read configuration".into());
                }
                let mut value = measure(
                    drive,
                    payload.block_size_bytes,
                    args.from_file,
                    args.files,
                    args.mode,
                );
                value["library_serial"] = json!(identity.library_serial);
                value["library_revision"] = json!(identity.library_revision);
                value["barcode"] = json!(identity.voltag);
                value["source_slot"] = json!(identity.home_slot);
                value["drive_element"] = json!(identity.drive_element);
                value["drive_serial"] = json!(identity.drive_serial);
                value["tape_uuid"] = json!(Uuid::from_bytes(payload.tape_uuid));
                measured = Some(value.clone());
                Ok(value)
            })
        },
    );
    preserve_measurement_on_cleanup_error(result, measured)
}

/// Keep measured evidence when restoring configuration or parking fails.
#[cfg(target_os = "linux")]
fn preserve_measurement_on_cleanup_error(
    result: Result<Value, String>,
    measured: Option<Value>,
) -> Result<Value, String> {
    match (result, measured) {
        (Err(error), Some(mut value)) => {
            value["ok"] = json!(false);
            value["cleanup_error"] = json!(error);
            Ok(value)
        }
        (result, _) => result,
    }
}

#[cfg(target_os = "linux")]
use remanence_library::verification_read_completion as completion;

#[cfg(target_os = "linux")]
/// Median command latency, or no value when no commands were timed.
fn median_seconds(samples: &mut [f64]) -> Option<f64> {
    samples.sort_by(f64::total_cmp);
    if samples.is_empty() {
        None
    } else {
        Some((samples[(samples.len() - 1) / 2] + samples[samples.len() / 2]) / 2.0)
    }
}

/// Measure commands on a mounted fixed-block drive. Setup time is separate from I/O time.
#[cfg(target_os = "linux")]
fn measure(drive: &mut DriveHandle, size: u32, from: u32, files: u32, mode: Mode) -> Value {
    let setup = Instant::now();
    let mut rows: Vec<Value> = Vec::new();
    let mut gaps: Vec<f64> = Vec::new();
    let mut total_records = 0u64;
    let mut setup_seconds = 0.0;
    let mut run_start = None;
    let mut outcome = "complete";
    let mut detail = None;
    let batch = (drive.sg_reserved_size_bytes() / size.max(1))
        .clamp(1, drive.requested_read_batch_blocks().max(1));
    let mut lookahead = Value::Null;
    let result = (|| -> Result<(), String> {
        if size == 0 {
            return Err("zero tape block size".into());
        }
        drive.locate(0).map_err(|e| e.to_string())?;
        let start = drive
            .space(i64::from(from), SpaceKind::Filemarks)
            .map_err(|e| e.to_string())?;
        let mut position = start.position_after;
        if start.units_traversed != i64::from(from) || start.stopped_at_boundary {
            outcome = "end_of_data";
            setup_seconds = setup.elapsed().as_secs_f64();
            return Ok(());
        }
        // SPACE includes READ POSITION; its returned position is device evidence.
        setup_seconds = setup.elapsed().as_secs_f64();
        run_start = Some(Instant::now());
        let mut buffer = vec![0; size as usize * batch as usize];
        let mut previous_last: Option<Instant> = None;
        for offset in 0..files {
            let file_start = Instant::now();
            let before = position;
            let mut records = 0u64;
            let mut last = None;
            let mut file_outcome = "filemark";
            let mut file_detail = None;
            let mut observed = None;
            let mut read_seconds = Vec::new();
            let mut filemark_read_seconds = None;
            if mode == Mode::Space {
                match drive.space(1, SpaceKind::Filemarks) {
                    Ok(spaced) => {
                        position = spaced.position_after;
                        observed = Some(position);
                        if spaced.units_traversed != 1 || spaced.stopped_at_boundary {
                            file_outcome = "end_of_data";
                        }
                        if position.partition != before.partition {
                            return Err("SPACE changes partition".into());
                        }
                        let delta = position
                            .lba
                            .checked_sub(before.lba)
                            .ok_or("SPACE position moves backward")?;
                        // Logical object positions include the consumed filemark.
                        records = delta
                            .checked_sub(u64::from(spaced.units_traversed == 1))
                            .ok_or("SPACE position omits filemark")?;
                    }
                    Err(error) => {
                        file_outcome = completion(&error, size, batch).1;
                        file_detail = Some(error.to_string());
                    }
                }
            } else {
                loop {
                    // The first record completes alone, giving a first-block timestamp.
                    let requested = if records == 0 { 1 } else { batch };
                    let read_started = Instant::now();
                    let read = drive.read_block_batch_for_verification(
                        &mut buffer,
                        size,
                        requested,
                        requested,
                    );
                    let completed = Instant::now();
                    let seconds = completed.duration_since(read_started).as_secs_f64();
                    read_seconds.push(seconds);
                    let (count, end) = match &read {
                        Ok(read) => (read.records_read, "complete"),
                        Err(error) => completion(error, size, requested),
                    };
                    if count > 0 {
                        if records == 0 {
                            if let Some(previous) = previous_last.take() {
                                let gap = completed.duration_since(previous).as_secs_f64();
                                gaps.push(gap);
                                if let Some(row) = rows.last_mut() {
                                    row["boundary_gap_seconds"] = json!(gap);
                                }
                            }
                        }
                        records += u64::from(count);
                        last = Some(completed);
                    }
                    if end == "complete" {
                        continue;
                    }
                    file_outcome = end;
                    if end == "filemark" {
                        filemark_read_seconds = Some(seconds);
                    }
                    if end != "filemark" {
                        file_detail = read.err().map(|e| e.to_string());
                    }
                    // Every filemark is followed by READ POSITION before the next READ.
                    match drive.position() {
                        Ok(after) => {
                            position = after;
                            observed = Some(after);
                            if end == "filemark"
                                && (after.partition != before.partition
                                    || after.lba.checked_sub(before.lba) != Some(records + 1))
                            {
                                file_outcome = "position_mismatch";
                            }
                        }
                        Err(error) => {
                            if end == "filemark" {
                                file_outcome = "position_error";
                                file_detail = Some(error.to_string());
                            } else {
                                file_detail = Some(format!(
                                    "{}; READ POSITION: {error}",
                                    file_detail.unwrap_or_default()
                                ));
                            }
                        }
                    }
                    break;
                }
                // Empty files have no last-block timestamp and no measurable gap.
                previous_last = last;
            }
            let seconds = file_start.elapsed().as_secs_f64();
            let bytes = records * u64::from(size);
            total_records += records;
            rows.push(json!({
                "tape_file": u64::from(from) + u64::from(offset), "records": records.to_string(), "bytes": bytes.to_string(),
                "elapsed_seconds": seconds, "mb_per_second": if mode == Mode::Read && seconds > 0.0 { Some(bytes as f64 / seconds / 1e6) } else { None },
                "position_delta_records": observed.and_then(|p| p.lba.checked_sub(before.lba)).map(|n| n.to_string()),
                "seconds_per_gib": if mode == Mode::Space && bytes > 0 { Some(seconds * 1073741824.0 / bytes as f64) } else { None },
                "position_after": observed.map(|p| p.lba.to_string()), "boundary_gap_seconds": null,
                "filemark_read_seconds": filemark_read_seconds,
                "median_batch_read_seconds": median_seconds(&mut read_seconds),
                "outcome": file_outcome, "detail": file_detail,
            }));
            if file_outcome != "filemark" {
                outcome = file_outcome;
                detail = file_detail;
                break;
            }
        }
        if mode == Mode::Read && outcome == "complete" {
            let read = drive.read_block_batch_for_verification(&mut buffer, size, 1, 1);
            let completed = Instant::now();
            let (count, end) = match &read {
                Ok(read) => (read.records_read, "complete"),
                Err(error) => completion(error, size, 1),
            };
            if count == 1 {
                if let Some(previous) = previous_last {
                    let gap = completed.duration_since(previous).as_secs_f64();
                    gaps.push(gap);
                    if let Some(row) = rows.last_mut() {
                        row["boundary_gap_seconds"] = json!(gap);
                    }
                }
            }
            lookahead = json!({ "records": count, "outcome": end, "detail": read.as_ref().err().map(ToString::to_string) });
            if end == "filemark" {
                match drive.position() {
                    Ok(after) => lookahead["position_after"] = json!(after.lba.to_string()),
                    Err(error) => lookahead["position_error"] = json!(error.to_string()),
                }
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        outcome = "io_error";
        detail = Some(error);
    }
    let elapsed_seconds = run_start
        .map(|start| start.elapsed().as_secs_f64())
        .unwrap_or(0.0);
    if run_start.is_none() {
        setup_seconds = setup.elapsed().as_secs_f64();
    }
    let median = median_seconds(&mut gaps);
    let bytes = total_records * u64::from(size);
    json!({
        "schema": "rem.tape.read-rate.v1", "ok": outcome == "complete", "outcome": outcome, "detail": detail,
        "mode": if mode == Mode::Read { "read" } else { "space" }, "from_file": from, "files_requested": files,
        "block_size_bytes": size, "batch_records": batch, "setup_seconds": setup_seconds,
        "timing_basis": "host command completion; filemark_read_seconds times the READ returning filemark; median_batch_read_seconds includes all file READs, including the initial single-record READ and terminal READ; gap includes READ POSITION and a single first-block READ; final lookahead is excluded from byte totals and measured outcome",
        "files": rows, "lookahead": lookahead, "totals": { "files_completed": rows.iter().filter(|row| row["outcome"] == "filemark").count(), "records": total_records.to_string(), "bytes": bytes.to_string(), "elapsed_seconds": elapsed_seconds,
            "mb_per_second": if mode == Mode::Read && elapsed_seconds > 0.0 { Some(bytes as f64 / elapsed_seconds / 1e6) } else { None },
            "seconds_per_gib": if mode == Mode::Space && bytes > 0 { Some(elapsed_seconds * 1073741824.0 / bytes as f64) } else { None },
            "boundary_gaps": gaps.len(), "median_boundary_gap_seconds": median, "max_boundary_gap_seconds": gaps.last() }
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use remanence_chaos::model::{
        DeviceRole, ModelTransport, Record, SharedVirtualWorld, VirtualTape, VirtualWorld,
    };
    use remanence_library::transport::TransferOutcome;
    use remanence_library::{decode_scsi_sense, ScsiError, SgTransport, TimeoutClass};
    use std::sync::{Arc, Mutex};

    /// Keep actual positioning and READ semantics in the virtual drive; inject one read fault.
    struct ReadFault {
        inner: ModelTransport,
        world: SharedVirtualWorld,
        reads: usize,
        fault: Option<u8>,
        omit_kernel_residual: bool,
        position_mismatch: bool,
        awaiting_filemark_position: bool,
        fixed_reads_only: bool,
    }
    impl SgTransport for ReadFault {
        fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
            let after_filemark = self.awaiting_filemark_position;
            if after_filemark {
                assert_eq!(cdb[0], 0x34, "filemark must be followed by READ POSITION");
                self.awaiting_filemark_position = false;
            }
            if [0x08, 0x34].contains(&cdb[0]) {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            if cdb[0] == 0x08 {
                if self.fixed_reads_only {
                    assert_eq!(cdb[1], 1, "READ uses fixed mode without SILI");
                }
                self.reads += 1;
                if self.reads == 2 {
                    if let Some(flags) = self.fault {
                        self.world.lock().unwrap().command_log.push(
                            remanence_chaos::model::ModelCommandRecord {
                                role: DeviceRole::Drive { bay: 0x100 },
                                opcode: 0x08,
                                timeout_class: TimeoutClass::TapeIo,
                            },
                        );
                        let mut sense = vec![0; 18];
                        sense[0] = 0xf0;
                        sense[2] = flags;
                        sense[7] = 10;
                        sense[12] = if flags == 3 { 0x11 } else { 0 };
                        return Err(ScsiError::CheckCondition {
                            sense,
                            bytes_transferred: 0,
                        });
                    }
                }
            }
            let mut result = self.inner.execute_in(cdb, buf);
            if cdb[0] == 0x08 {
                if let Err(ScsiError::CheckCondition {
                    sense,
                    bytes_transferred,
                }) = &mut result
                {
                    if decode_scsi_sense(sense).unwrap().filemark {
                        self.awaiting_filemark_position = true;
                        // Model a stop whose cost is inside the terminal READ.
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    if self.omit_kernel_residual && cdb[1] & 1 != 0 {
                        *bytes_transferred = buf.len() as u32;
                    }
                }
            }
            if after_filemark && self.position_mismatch && result.is_ok() {
                buf[15] ^= 1; // Corrupt the long-form logical object position.
            }
            result
        }
        fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
            self.inner.execute_none(cdb)
        }
        fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
            assert!(
                [0x15, 0x55].contains(&cdb[0]),
                "only MODE SELECT may send data"
            );
            self.inner.execute_out(cdb, buf)
        }
        fn set_timeout_for(&mut self, class: TimeoutClass) {
            self.inner.set_timeout_for(class);
        }
    }

    fn drive(fault: Option<u8>, empty: bool) -> (DriveHandle, SharedVirtualWorld) {
        drive_with_options(fault, empty, false, false)
    }

    fn drive_with_options(
        fault: Option<u8>,
        empty: bool,
        omit_kernel_residual: bool,
        position_mismatch: bool,
    ) -> (DriveHandle, SharedVirtualWorld) {
        drive_with_policy(fault, empty, omit_kernel_residual, position_mismatch, true)
    }

    fn drive_with_policy(
        fault: Option<u8>,
        empty: bool,
        omit_kernel_residual: bool,
        position_mismatch: bool,
        fixed_reads_only: bool,
    ) -> (DriveHandle, SharedVirtualWorld) {
        let mut world = VirtualWorld::single_drive("TESTLIB", 0x100, "TESTDRIVE", 0x400, 1);
        let mut tape = VirtualTape::empty(1 << 30, 262144);
        tape.records.push(Record::ZeroBlock(262144));
        tape.records.push(Record::Filemark);
        for count in [if empty { 0 } else { 19 }, 5, 1] {
            tape.records
                .extend((0..count).map(|_| Record::ZeroBlock(262144)));
            tape.records.push(Record::Filemark);
        }
        world.put_tape_in_drive(0x100, "TESTL9", Some(0x400), tape);
        let world = Arc::new(Mutex::new(world));
        let transport = ReadFault {
            inner: ModelTransport::new(world.clone(), DeviceRole::Drive { bay: 0x100 }),
            world: world.clone(),
            reads: 0,
            fault,
            omit_kernel_residual,
            position_mismatch,
            awaiting_filemark_position: false,
            fixed_reads_only,
        };
        let mut drive = DriveHandle::open_standalone_with_transport(
            Path::new("/virtual/read-rate"),
            Box::new(transport),
            &StaticAllowlist::new(["TESTDRIVE"]),
        )
        .unwrap();
        let config = drive.read_config().unwrap();
        drive.write_config(config).unwrap();
        world.lock().unwrap().command_log.clear();
        (drive, world)
    }

    /// The same no-residual transport mode exercises file measurement and crediting.
    #[test]
    fn forward_file_reader_credits_no_residual_partial_batch() {
        for omit in [false, true] {
            let (mut walk, walk_world) = drive_with_policy(None, false, omit, false, false);
            let (mut forward, world) = drive_with_policy(None, false, omit, false, true);
            let mut bot = vec![0; 262144];
            remanence_parity::bootstrap::write_bootstrap_block(
                &remanence_parity::BootstrapPayload {
                    tape_uuid: [0; 16],
                    sequence: 0,
                    block_size_bytes: 262144,
                    scheme: None,
                    no_parity_flag: true,
                    filemark_map_digest: None,
                    drive_compression: false,
                    written_by_version: "file-reader-test".into(),
                    written_at: String::new(),
                },
                &mut bot,
            )
            .unwrap();
            for world in [&walk_world, &world] {
                world
                    .lock()
                    .unwrap()
                    .tapes
                    .get_mut("TESTL9")
                    .unwrap()
                    .records[0] = Record::Block(bot.clone());
            }
            let walk = remanence_parity::scan_reconstruct_filemark_map_with_report(
                &mut remanence_parity::DriveHandleRawSource::new(&mut walk),
                &[0; 16],
                262144,
            )
            .unwrap();
            let forward = remanence_parity::scan::forward_file_scan_for_test(
                &mut remanence_parity::DriveHandleRawSource::new(&mut forward),
                &[0; 16],
                262144,
                remanence_parity::ScanMode::Standard,
            )
            .unwrap();
            assert_eq!(walk, forward);
            assert_eq!(
                forward
                    .map
                    .entries()
                    .iter()
                    .map(|e| e.block_count)
                    .collect::<Vec<_>>(),
                [1, 19, 5, 1]
            );
            let world = world.lock().unwrap();
            let count = |opcode| {
                world
                    .command_log
                    .iter()
                    .filter(|c| c.opcode == opcode)
                    .count()
            };
            assert_eq!(count(0x11), 0, "healthy file reader issues no SPACE");
            assert_eq!(count(0x92), 1, "only the initial BOT LOCATE");
            assert_eq!(
                count(0x34),
                5,
                "initial LOCATE report plus one report per filemark"
            );
        }
    }

    /// A boundary without agreeing transfer evidence cannot contribute bytes.
    #[test]
    fn read_rate_reports_unconfirmed_boundary_without_credit() {
        let (mut drive, _) = drive(Some(0x80), false);
        let value = measure(&mut drive, 262144, 1, 1, Mode::Read);
        assert_eq!(value["outcome"], "unconfirmed_boundary");
        assert_eq!(value["files"][0]["outcome"], "unconfirmed_boundary");
        assert_eq!(value["totals"]["bytes"], "262144");
        assert_eq!(value["totals"]["files_completed"], 0);
        assert_eq!(value["ok"], false);
    }

    #[test]
    fn read_rate_crosses_two_filemarks_without_repositioning() {
        for omit_kernel_residual in [false, true] {
            let (mut drive, world) = drive_with_options(None, false, omit_kernel_residual, false);
            let value = measure(&mut drive, 262144, 1, 2, Mode::Read);
            println!("READ_RATE_EXAMPLE={value}");
            assert_eq!(value["outcome"], "complete");
            assert_eq!(value["files"][0]["records"], "19");
            assert_eq!(value["files"][1]["records"], "5");
            assert_eq!(value["totals"]["bytes"], (24u64 * 262144).to_string());
            assert_eq!(value["totals"]["boundary_gaps"], 2);
            for row in value["files"].as_array().unwrap() {
                assert!(row["filemark_read_seconds"].as_f64().unwrap() >= 0.010);
                assert!(row["median_batch_read_seconds"].as_f64().unwrap() >= 0.002);
            }
            let gaps: Vec<_> = value["files"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["boundary_gap_seconds"].as_f64().unwrap())
                .collect();
            assert!(gaps.iter().all(|gap| *gap >= 0.004));
            assert_eq!(
                value["totals"]["median_boundary_gap_seconds"],
                (gaps[0] + gaps[1]) / 2.0
            );
            assert_eq!(
                value["totals"]["max_boundary_gap_seconds"],
                gaps[0].max(gaps[1])
            );
            let ops: Vec<_> = world
                .lock()
                .unwrap()
                .command_log
                .iter()
                .map(|c| c.opcode)
                .collect();
            assert_eq!(ops.iter().filter(|&&c| c == 0x92).count(), 1);
            assert_eq!(ops.iter().filter(|&&c| c == 0x11).count(), 1);
            assert_eq!(ops.iter().filter(|&&c| c == 0x08).count(), 6);
            let first_read = ops.iter().position(|&op| op == 0x08).unwrap();
            assert_eq!(
                &ops[first_read..],
                &[0x08, 0x08, 0x08, 0x34, 0x08, 0x08, 0x34, 0x08],
                "exactly one READ POSITION after each filemark"
            );
            assert!(!ops.iter().any(|c| [0x0a, 0x10, 0x19, 0x04].contains(c)));
        }
    }

    #[test]
    fn read_rate_space_reports_object_delta_and_data_bytes() {
        let (mut drive, world) = drive(None, false);
        let value = measure(&mut drive, 262144, 1, 2, Mode::Space);
        assert_eq!(value["outcome"], "complete");
        for (row, records) in value["files"].as_array().unwrap().iter().zip([19u64, 5]) {
            assert_eq!(row["position_delta_records"], (records + 1).to_string());
            assert_eq!(row["records"], records.to_string());
            assert!(row["filemark_read_seconds"].is_null());
            assert!(row["median_batch_read_seconds"].is_null());
            let seconds = row["elapsed_seconds"].as_f64().unwrap();
            assert_eq!(
                row["seconds_per_gib"],
                seconds * 1073741824.0 / (records * 262144) as f64
            );
        }
        assert!(!world
            .lock()
            .unwrap()
            .command_log
            .iter()
            .any(|c| c.opcode == 0x08));
        let later = measure(&mut drive, 262144, 2, 1, Mode::Space);
        assert_eq!(later["files"][0]["records"], "5");
        let ops: Vec<_> = world
            .lock()
            .unwrap()
            .command_log
            .iter()
            .map(|c| c.opcode)
            .collect();
        assert!(!ops.iter().any(|c| [0x0a, 0x10, 0x19, 0x04].contains(c)));
        for (index, &op) in ops.iter().enumerate() {
            if op == 0x11 {
                assert_eq!(ops[index + 1], 0x34);
                assert_ne!(ops.get(index + 2), Some(&0x34));
            }
        }
    }

    #[test]
    fn read_rate_reports_medium_wrong_length_and_recovered_errors_without_retry() {
        for (flags, expected) in [
            (3, "medium_error"),
            (0x20, "wrong_length"),
            (1, "recovered_error"),
        ] {
            let (mut drive, world) = drive(Some(flags), false);
            let value = measure(&mut drive, 262144, 1, 2, Mode::Read);
            assert_eq!(value["outcome"], expected);
            assert_eq!(value["files"].as_array().unwrap().len(), 1);
            assert_eq!(value["files"][0]["records"], "1");
            assert_eq!(value["totals"]["boundary_gaps"], 0);
            assert!(value["detail"].is_string());
            assert_eq!(
                write_report(Ok(value), &mut Vec::new(), &mut Vec::new()),
                ExitCode::SUCCESS
            );
            assert_eq!(
                world
                    .lock()
                    .unwrap()
                    .command_log
                    .iter()
                    .filter(|c| c.opcode == 0x08)
                    .count(),
                2
            );
        }
    }

    #[test]
    fn read_rate_handles_empty_files_and_end_of_data() {
        for omit_kernel_residual in [false, true] {
            let (mut drive, _) = drive_with_options(None, true, omit_kernel_residual, false);
            let value = measure(&mut drive, 262144, 1, 4, Mode::Read);
            assert_eq!(value["outcome"], "end_of_data");
            assert_eq!(value["files"][0]["records"], "0");
            assert!(value["files"][0]["boundary_gap_seconds"].is_null());
            assert_eq!(value["totals"]["boundary_gaps"], 1);
            assert_eq!(value["totals"]["records"], "6");
            assert_eq!(
                write_report(Ok(value), &mut Vec::new(), &mut Vec::new()),
                ExitCode::SUCCESS
            );
            let value = measure(&mut drive, 262144, 1, 4, Mode::Space);
            assert_eq!(value["outcome"], "end_of_data");
            assert!(value["files"][0]["seconds_per_gib"].is_null());
        }
    }

    #[test]
    fn read_rate_lookahead_end_of_data_keeps_completed_measurement() {
        for omit_kernel_residual in [false, true] {
            let (mut drive, _) = drive_with_options(None, false, omit_kernel_residual, false);
            let value = measure(&mut drive, 262144, 1, 3, Mode::Read);
            assert_eq!(value["outcome"], "complete");
            assert_eq!(value["ok"], true);
            assert!(value["detail"].is_null());
            assert_eq!(value["lookahead"]["outcome"], "end_of_data");
            assert_eq!(value["lookahead"]["records"], 0);
            assert_eq!(value["totals"]["files_completed"], 3);
            assert_eq!(value["totals"]["records"], "25");
            assert!(value["files"][2]["boundary_gap_seconds"].is_null());
            assert_eq!(
                write_report(Ok(value), &mut Vec::new(), &mut Vec::new()),
                ExitCode::SUCCESS
            );
        }
    }

    #[test]
    fn read_rate_end_of_data_credits_partial_batch_from_sense() {
        for omit_kernel_residual in [false, true] {
            let (mut drive, world) = drive_with_options(None, false, omit_kernel_residual, false);
            // Leave two blocks of the second file without a terminating filemark.
            world
                .lock()
                .unwrap()
                .tapes
                .get_mut("TESTL9")
                .unwrap()
                .records
                .truncate(24);
            let value = measure(&mut drive, 262144, 2, 1, Mode::Read);
            assert_eq!(value["outcome"], "end_of_data");
            assert_eq!(value["ok"], false);
            assert_eq!(value["files"][0]["records"], "2");
            assert_eq!(value["totals"]["bytes"], (2 * 262144).to_string());
            assert_eq!(value["totals"]["files_completed"], 0);
            assert!(value["files"][0]["filemark_read_seconds"].is_null());
        }
    }

    #[test]
    fn read_rate_lookahead_filemark_has_one_position_read() {
        for omit_kernel_residual in [false, true] {
            let (mut drive, world) = drive_with_options(None, true, omit_kernel_residual, false);
            let value = measure(&mut drive, 262144, 0, 1, Mode::Read);
            assert_eq!(value["outcome"], "complete");
            assert_eq!(value["lookahead"]["outcome"], "filemark");
            assert_eq!(value["lookahead"]["position_after"], "3");
            let ops: Vec<_> = world
                .lock()
                .unwrap()
                .command_log
                .iter()
                .map(|c| c.opcode)
                .collect();
            let first_read = ops.iter().position(|&op| op == 0x08).unwrap();
            assert_eq!(&ops[first_read..], &[0x08, 0x08, 0x34, 0x08, 0x34]);
        }
    }

    #[test]
    fn read_rate_position_mismatch_stops_without_retry() {
        let (mut drive, world) = drive_with_options(None, false, true, true);
        let value = measure(&mut drive, 262144, 1, 2, Mode::Read);
        assert_eq!(value["outcome"], "position_mismatch");
        assert_eq!(value["ok"], false);
        assert_eq!(value["files"].as_array().unwrap().len(), 1);
        assert_eq!(value["files"][0]["records"], "19");
        assert!(value["lookahead"].is_null());
        let ops: Vec<_> = world
            .lock()
            .unwrap()
            .command_log
            .iter()
            .map(|c| c.opcode)
            .collect();
        assert_eq!(ops.iter().filter(|&&op| op == 0x92).count(), 1);
        assert_eq!(ops.iter().filter(|&&op| op == 0x08).count(), 3);
        assert_eq!(ops.last(), Some(&0x34));
    }

    #[test]
    fn read_rate_cleanup_failure_preserves_counts_and_exits_nonzero() {
        let (mut drive, _) = drive(None, false);
        let mut measured = None;
        struct RestoreFailure<'a>(&'a mut DriveHandle, usize);
        impl BotDriveConfigOps for RestoreFailure<'_> {
            fn bot_read_config(&mut self) -> Result<TapeConfig, String> {
                self.0.bot_read_config()
            }
            fn bot_write_config(&mut self, config: TapeConfig) -> Result<(), String> {
                self.1 += 1;
                if self.1 == 2 {
                    Err("injected restore failure".into())
                } else {
                    self.0.bot_write_config(config)
                }
            }
        }
        let result = with_temporary_bot_read_config(&mut RestoreFailure(&mut drive, 0), |drive| {
            let value = measure(drive.0, 262144, 1, 2, Mode::Space);
            measured = Some(value.clone());
            Ok(value)
        });
        let result = preserve_measurement_on_cleanup_error(result, measured);
        let mut out = Vec::new();
        assert_eq!(
            write_report(result, &mut out, &mut Vec::new()),
            ExitCode::FAILURE
        );
        let value: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["outcome"], "complete");
        assert_eq!(value["ok"], false);
        assert_eq!(value["totals"]["records"], "24");
        assert!(value["cleanup_error"]
            .as_str()
            .unwrap()
            .contains("injected restore failure"));
    }

    #[test]
    fn read_rate_parser_authorization_and_state_lock_refusal() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config.toml");
        std::fs::write(
            &config,
            format!(
                r#"
[daemon]
state_dir = "{0}"
default_idle_timeout_seconds = 1800
read_only = false
[journal]
dir = "{0}/journals"
require_trusted_volume = false
[audit]
dir = "{0}/audit"
fsync = true
[index]
sqlite_path = "{0}/index.sqlite"
[cache]
tape_catalog_dir = "{0}/cache"
"#,
                temp.path().display()
            ),
        )
        .unwrap();
        let argv = [
            "rem-debug",
            "tape",
            "read-rate",
            "TESTL9",
            "--config",
            config.to_str().unwrap(),
            "--library",
            "TESTLIB",
            "--expected-home-slot",
            "1024",
            "--json",
            "--files",
            "2",
            "--mode",
            "read",
        ];
        let cli: ParsedCli = DebugCli::try_parse_from(argv).unwrap().into();
        assert_eq!(state_changing_target(&cli.command), Some("TESTLIB"));
        let Command::Tape {
            command: TapeCommand::ReadRate(args),
        } = cli.command
        else {
            panic!("wrong command")
        };
        args.validate_before_discovery().unwrap();
        let _lock = remanence_state::StateLockGuard::acquire(temp.path()).unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut authorized = argv.to_vec();
        authorized.extend(["--allow", "TESTLIB"]);
        assert_eq!(
            run_with_mode(
                DebugCli::try_parse_from(authorized).unwrap().into(),
                CliMode::Debug,
                || panic!("held state lock must refuse before discovery"),
                &mut io::empty(),
                &mut out,
                &mut err,
                &ForeignFormatRegistry::new()
            ),
            ExitCode::FAILURE
        );
        let value: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["outcome"], "command_error");
        assert!(
            value["detail"].as_str().unwrap().contains("state lock"),
            "{value}"
        );
        assert_eq!(
            std::fs::read_dir(temp.path()).unwrap().count(),
            2,
            "no catalog or journal opens"
        );
        out.clear();
        err.clear();
        assert_eq!(
            run_with_mode(
                DebugCli::try_parse_from(argv).unwrap().into(),
                CliMode::Debug,
                || panic!("missing allow must refuse before discovery"),
                &mut io::empty(),
                &mut out,
                &mut err,
                &ForeignFormatRegistry::new()
            ),
            ExitCode::FAILURE
        );
        assert!(String::from_utf8(err)
            .unwrap()
            .contains("not on the --allow list"));
        for omitted in [
            "--json",
            "--files",
            "--mode",
            "--library",
            "--expected-home-slot",
        ] {
            let mut bad = argv.to_vec();
            let index = bad.iter().position(|a| *a == omitted).unwrap();
            bad.remove(index);
            if omitted != "--json" {
                bad.remove(index);
            }
            assert!(DebugCli::try_parse_from(bad).is_err(), "requires {omitted}");
        }
    }
}
