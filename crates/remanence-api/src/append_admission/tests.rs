//! Exact-sense transport tests for append proof and its structural write gate.
use super::*;
use remanence_chaos::model::{DeviceRole, ModelTransport, Record, VirtualTape, VirtualWorld};
use remanence_library::{
    transport::TransferOutcome, BlockSink, DriveHandleSink, ScsiError, SgTransport, StaticAllowlist,
};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

const UUID: [u8; 16] = [0x42; 16];
const SIZE: u32 = 4096;
type World = Arc<Mutex<VirtualWorld>>;
type ProbeResponse = Arc<Mutex<Option<(Vec<u8>, u32)>>>;
struct ProbeTransport {
    inner: ModelTransport,
    response: ProbeResponse,
    fail_read_after: Arc<Mutex<Option<usize>>>,
    response_skip: Arc<Mutex<usize>>,
    omit_kernel_residual: Arc<Mutex<bool>>,
    position_fault: Arc<Mutex<Option<(u64, u32)>>>,
    reported_partition: Arc<Mutex<u32>>,
    commands: Arc<Mutex<Vec<u8>>>,
}
impl SgTransport for ProbeTransport {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        self.commands.lock().unwrap().push(cdb[0]);
        if cdb[0] == 0x08 {
            let mut countdown = self.fail_read_after.lock().unwrap();
            if let Some(remaining) = countdown.as_mut() {
                if *remaining == 0 {
                    *countdown = None;
                    return Err(ScsiError::CheckCondition {
                        sense: vec![0x70, 0, 4, 0, 0, 0, 0, 10, 0, 0, 0, 0, 0x44, 0, 0, 0, 0, 0],
                        bytes_transferred: 0,
                    });
                }
                *remaining -= 1;
            }
            let mut skip = self.response_skip.lock().unwrap();
            let response = if *skip > 0 {
                *skip -= 1;
                None
            } else {
                self.response.lock().unwrap().take()
            };
            if let Some((sense, bytes_transferred)) = response {
                return Err(ScsiError::CheckCondition {
                    sense,
                    bytes_transferred,
                });
            }
        }
        let mut result = self.inner.execute_in(cdb, buf);
        if cdb[0] == 0x08 && *self.omit_kernel_residual.lock().unwrap() {
            if let Err(ScsiError::CheckCondition {
                sense,
                bytes_transferred,
            }) = &mut result
            {
                if cdb[1] & 1 == 0
                    && sense.len() >= 7
                    && (sense[2] & 0x80 != 0 || sense[2] & 15 == 8)
                {
                    sense[0] |= 0x80;
                    sense[3..7]
                        .copy_from_slice(&(buf.len() as u32 - *bytes_transferred).to_be_bytes());
                }
                *bytes_transferred = buf.len() as u32;
            }
        }
        if cdb[0] == 0x34 && result.is_ok() {
            buf[4..8].copy_from_slice(&self.reported_partition.lock().unwrap().to_be_bytes());
            let commands = self.commands.lock().unwrap();
            if commands.len() >= 2 && commands[commands.len() - 2] == 0x08 {
                if let Some((lba, partition)) = *self.position_fault.lock().unwrap() {
                    buf[4..8].copy_from_slice(&partition.to_be_bytes());
                    buf[8..16].copy_from_slice(&lba.to_be_bytes());
                }
            }
        }
        result
    }
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        self.commands.lock().unwrap().push(cdb[0]);
        self.inner.execute_out(cdb, buf)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        self.commands.lock().unwrap().push(cdb[0]);
        self.inner.execute_none(cdb)
    }
}
struct Fixture {
    drive: DriveHandle,
    world: World,
    response: ProbeResponse,
    fail_read_after: Arc<Mutex<Option<usize>>>,
    response_skip: Arc<Mutex<usize>>,
    omit_kernel_residual: Arc<Mutex<bool>>,
    position_fault: Arc<Mutex<Option<(u64, u32)>>>,
    reported_partition: Arc<Mutex<u32>>,
    commands: Arc<Mutex<Vec<u8>>>,
}
fn fixture() -> Fixture {
    let bootstrap = crate::build_tape_bootstrap(
        UUID,
        SIZE,
        remanence_parity::ParityConfig::None,
        "2026-10-23T00:00:00Z",
        "test",
    );
    let mut block = vec![0; SIZE as usize];
    remanence_parity::bootstrap::write_bootstrap_block(&bootstrap, &mut block).unwrap();
    let mut tape = VirtualTape::empty(16 * 1024 * 1024, SIZE);
    tape.records = vec![Record::Block(block), Record::Filemark];
    let mut world = VirtualWorld::single_drive("APPEND", 0x100, "APPEND-DRIVE", 0x400, 0);
    world.put_tape_in_drive(0x100, "TAIL", None, tape);
    let world = Arc::new(Mutex::new(world));
    let response = Arc::new(Mutex::new(None));
    let commands = Arc::new(Mutex::new(Vec::new()));
    let fail_read_after = Arc::new(Mutex::new(None));
    let response_skip = Arc::new(Mutex::new(0));
    let omit_kernel_residual = Arc::new(Mutex::new(false));
    let position_fault = Arc::new(Mutex::new(None));
    let reported_partition = Arc::new(Mutex::new(0));
    let drive = DriveHandle::open_standalone_with_transport(
        Path::new("/dev/sg-append-test"),
        Box::new(ProbeTransport {
            inner: ModelTransport::new(world.clone(), DeviceRole::Drive { bay: 0x100 }),
            response: response.clone(),
            fail_read_after: fail_read_after.clone(),
            response_skip: response_skip.clone(),
            omit_kernel_residual: omit_kernel_residual.clone(),
            position_fault: position_fault.clone(),
            reported_partition: reported_partition.clone(),
            commands: commands.clone(),
        }),
        &StaticAllowlist::new(["APPEND-DRIVE"]),
    )
    .unwrap();
    Fixture {
        drive,
        world,
        response,
        fail_read_after,
        response_skip,
        omit_kernel_residual,
        position_fault,
        reported_partition,
        commands,
    }
}
fn checkpoint(drive: &mut DriveHandle) -> Result<AppendAdmission, AdmissionError> {
    let ctx = context(UUID, SIZE, 0, 2);
    admit_append_boundary(
        drive,
        AppendAuthority::Checkpoint { eod: ctx.boundary },
        ctx,
    )
}
fn writes(commands: &[u8]) -> usize {
    commands
        .iter()
        .filter(|&&op| op == 0x0a || op == 0x10)
        .count()
}
#[test]
fn clean_checkpoint_counts_commands_and_preserves_the_normal_write_bytes() {
    let mut f = fixture();
    f.commands.lock().unwrap().clear();
    let _admission = checkpoint(&mut f.drive).unwrap();
    // The writer's existing initial LOCATE reuses the admission's device proof.
    DriveHandleSink(&mut f.drive).locate(2).unwrap();
    let commands = f.commands.lock().unwrap().clone();
    assert_eq!(
        commands,
        [0x92, 0x34, 0x08, 0x34, 0x08, 0x92, 0x34, 0x08, 0x34]
    );
    assert_eq!(writes(&commands), 0);
    let block = vec![0xab; SIZE as usize];
    DriveHandleSink(&mut f.drive).write_block(&block).unwrap();
    assert_eq!(
        f.world.lock().unwrap().tapes["TAIL"].records[2],
        Record::Block(block)
    );
}
#[test]
fn both_production_adapters_refuse_missing_admission_without_dispatch() {
    use remanence_parity::RawTapeSink;
    let mut f = fixture();
    f.drive.locate(2).unwrap();
    assert!(DriveHandleSink(&mut f.drive)
        .write_block(&[1; 4096])
        .is_err());
    assert!(DriveHandleSink(&mut f.drive)
        .write_block_batch(&[1; 4096], SIZE)
        .is_err());
    assert!(DriveHandleSink(&mut f.drive).write_filemarks(1).is_err());
    assert!(DriveHandleSink(&mut f.drive)
        .write_filemarks_immediate(1)
        .is_err());
    assert!(remanence_parity::DriveHandleRawSink::new(&mut f.drive)
        .write_fixed_block(&[1; 4096])
        .is_err());
    assert!(remanence_parity::DriveHandleRawSink::new(&mut f.drive)
        .write_filemarks(1, false)
        .is_err());
    assert_eq!(writes(&f.commands.lock().unwrap()), 0);
    assert_eq!(f.drive.media_write_dispatch_count(), 0);
}
#[test]
fn admission_is_bound_to_position_and_identity_generation() {
    let mut f = fixture();
    let _admission = checkpoint(&mut f.drive).unwrap();
    f.drive.locate(0).unwrap();
    assert!(DriveHandleSink(&mut f.drive)
        .write_block(&[1; 4096])
        .is_err());
    f.drive.locate(2).unwrap();
    *f.response.lock().unwrap() = Some((vec![0x72, 6, 0x29, 0], 0));
    assert!(f.drive.read_block(&mut [0; 4096]).is_err());
    f.drive.locate(2).unwrap();
    assert!(DriveHandleSink(&mut f.drive).write_filemarks(1).is_err());
    assert_eq!(writes(&f.commands.lock().unwrap()), 0);
}
#[test]
fn only_exact_current_zero_transfer_blank_check_is_eod() {
    for (sense, bytes, allowed) in [
        (vec![0x72, 8, 0, 5], 0, true),
        (vec![0x72, 8, 0x50, 0], 0, false),
        (vec![0x73, 8, 0, 5], 0, false),
        (vec![0x72, 8, 0, 5], 1, false),
        (vec![0x72, 3, 0x0c, 0], 0, false),
        (vec![0x72, 6, 0x29, 0], 0, false),
    ] {
        let mut f = fixture();
        *f.response_skip.lock().unwrap() = 1; // Inject at C, after its predecessor proof.
        *f.response.lock().unwrap() = Some((sense.clone(), bytes));
        assert_eq!(
            checkpoint(&mut f.drive).is_ok(),
            allowed,
            "{sense:?}, bytes={bytes}"
        );
        assert_eq!(writes(&f.commands.lock().unwrap()), 0);
        if !allowed {
            assert!(DriveHandleSink(&mut f.drive).write_filemarks(1).is_err());
        }
    }
}
#[test]
fn torn_first_object_is_admitted_by_fresh_and_checkpoint() {
    for fresh in [false, true] {
        let mut f = fixture();
        f.world
            .lock()
            .unwrap()
            .tapes
            .get_mut("TAIL")
            .unwrap()
            .records
            .push(Record::Block(plain_blocks()[0].clone()));
        let _admission = if fresh {
            admit_append_boundary(
                &mut f.drive,
                AppendAuthority::Fresh,
                context(UUID, SIZE, 0, 0),
            )
            .unwrap()
        } else {
            checkpoint(&mut f.drive).unwrap()
        };
        assert_eq!(writes(&f.commands.lock().unwrap()), 0);
        assert_eq!(f.drive.position().unwrap().lba, if fresh { 0 } else { 2 });
        DriveHandleSink(&mut f.drive).write_filemarks(1).unwrap();
    }
}

#[test]
fn terminal_admission_preserves_reconciliation_at_each_component_cut() {
    use remanence_parity::{
        DriveHandleRawSource, TerminalComponentReconcileEvidence as Evidence, TerminalTailProgress,
    };
    let vector = crate::tape_image_vectors::generate("a4-minimal").unwrap();
    let plan = vector.written.terminal_plan.as_ref().unwrap();
    let layout = &plan.edition.descriptor.terminal_layout;
    let mut records = Vec::new();
    for file in &vector.image.files {
        for (index, offset) in file.record_offsets.iter().enumerate() {
            let end = file
                .record_offsets
                .get(index + 1)
                .copied()
                .unwrap_or(file.bytes.len());
            records.push(Record::Block(file.bytes[*offset..end].to_vec()));
        }
        if file.filemark_record.is_some() {
            records.push(Record::Filemark);
        }
    }
    // Reconcile first, as the production terminal path does, then admit from
    // the checkpoint before the existing parity prefix rather than replica A.
    let prefix = vector.written.terminal_prefix.as_ref().unwrap();
    let mut f = fixture();
    {
        let mut world = f.world.lock().unwrap();
        let tape = world.tapes.get_mut("TAIL").unwrap();
        tape.block_size = plan.edition.descriptor.block_size;
        tape.records = records.clone();
    }
    assert_eq!(
        remanence_parity::reconcile_terminal_prefix(
            &mut DriveHandleRawSource::new(&mut f.drive),
            prefix,
            &plan.edition.descriptor.tape_uuid,
            plan.edition.descriptor.block_size,
            true
        ),
        remanence_parity::TerminalPrefixReconcileEvidence::Complete
    );
    let mut ctx = context(
        plan.edition.descriptor.tape_uuid,
        plan.edition.descriptor.block_size,
        0,
        prefix.start_lba,
    );
    ctx.terminal_prefix = Some(prefix.clone());
    ctx.write_position = f.drive.position().unwrap();
    let _admission =
        admit_append_boundary(&mut f.drive, AppendAuthority::Terminal { plan }, ctx).unwrap();
    assert_eq!(writes(&f.commands.lock().unwrap()), 0);

    // A sidecar with no owning terminal plan is foreign even when alone.
    let sidecar = vector
        .written
        .map
        .entries()
        .iter()
        .find(|e| e.kind == remanence_parity::TapeFileKind::ParitySidecar)
        .unwrap();
    let start = vector
        .written
        .map
        .physical_position(remanence_parity::TapeFilePosition {
            tape_file_number: sidecar.tape_file_number,
            block_within_file: 0,
        })
        .unwrap()
        .lba;
    let mut ctx = context(
        plan.edition.descriptor.tape_uuid,
        plan.edition.descriptor.block_size,
        0,
        start,
    );
    ctx.write_position = ctx.boundary;
    f.world
        .lock()
        .unwrap()
        .tapes
        .get_mut("TAIL")
        .unwrap()
        .records
        .truncate((start + sidecar.block_count + 1) as usize);
    let error = admit_append_boundary(
        &mut f.drive,
        AppendAuthority::Checkpoint { eod: ctx.boundary },
        ctx,
    )
    .unwrap_err();
    assert!(error.to_string().contains("terminal sidecar"), "{error}");

    let mut progress = TerminalTailProgress::BeforeReplicaA;
    for component in layout.components {
        for retained in [
            0,
            1,
            component.record_count - 1,
            component.record_count,
            component.record_count + 1,
        ] {
            let mut f = fixture();
            f.world
                .lock()
                .unwrap()
                .tapes
                .get_mut("TAIL")
                .unwrap()
                .block_size = plan.edition.descriptor.block_size;
            f.world
                .lock()
                .unwrap()
                .tapes
                .get_mut("TAIL")
                .unwrap()
                .records = records[..(component.planned_start_lba + retained) as usize].to_vec();
            let mut ctx = context(
                plan.edition.descriptor.tape_uuid,
                plan.edition.descriptor.block_size,
                0,
                layout.components[0].planned_start_lba,
            );
            ctx.write_position.lba = component.planned_start_lba;
            f.commands.lock().unwrap().clear();
            let _admission =
                admit_append_boundary(&mut f.drive, AppendAuthority::Terminal { plan }, ctx)
                    .unwrap();
            if progress == TerminalTailProgress::BeforeReplicaA && retained == 0 {
                assert_eq!(
                    *f.commands.lock().unwrap(),
                    [0x92, 0x34, 0x08, 0x34, 0x08, 0x92, 0x34, 0x08, 0x34]
                );
            }
            let evidence = remanence_parity::reconcile_terminal_tail_next(
                &mut DriveHandleRawSource::new(&mut f.drive),
                plan,
                progress,
                true,
            );
            let expected = if retained == 0 {
                Evidence::Absent
            } else if retained == component.record_count + 1 {
                Evidence::Complete
            } else {
                Evidence::TornRewritable
            };
            assert_eq!(evidence, expected, "{progress:?}, retained={retained}");
            assert_eq!(writes(&f.commands.lock().unwrap()), 0);
        }
        progress = progress.successor().unwrap();
    }
}

#[test]
fn fresh_admission_command_budget_is_explicit() {
    let mut f = fixture();
    f.commands.lock().unwrap().clear();
    let _admission = admit_append_boundary(
        &mut f.drive,
        AppendAuthority::Fresh,
        context(UUID, SIZE, 0, 0),
    )
    .unwrap();
    let commands = f.commands.lock().unwrap().clone();
    println!("fresh admission SCSI opcodes: {commands:02x?}");
    assert_eq!(
        commands,
        [0x01, 0x92, 0x34, 0x08, 0x08, 0x34, 0x08, 0x92, 0x34]
    );
    assert_eq!(writes(&commands), 0);
    assert_eq!(commands.iter().filter(|&&op| op == 0x08).count(), 3);
    assert_eq!(commands.last(), Some(&0x34));
}

fn plain_blocks() -> Vec<Vec<u8>> {
    let mut options = remanence_format::RemTarObjectOptions::new(
        "00000000-0000-4000-8000-000000000001",
        "append-test",
        "2026-10-23T00:00:00Z",
        "00000000-0000-4000-8000-000000000002",
    );
    options.chunk_size = SIZE as usize;
    let payload = vec![0x35; SIZE as usize * 8];
    let file = remanence_format::RemTarFile {
        path: "payload",
        file_id: "00000000-0000-4000-8000-000000000003",
        data: &payload,
        mtime: None,
        executable: Some(false),
    };
    let mut sink = remanence_library::VecBlockSink::new();
    remanence_format::write_rem_tar_object(&mut sink, &options, &[file]).unwrap();
    sink.blocks
}

#[test]
fn closed_admission_refuses_a_second_session_with_zero_dispatch() {
    let mut f = fixture();
    let admission = checkpoint(&mut f.drive).unwrap();
    DriveHandleSink(&mut f.drive).write_filemarks(1).unwrap();
    drop(admission);
    let before = f.drive.media_write_dispatch_count();
    assert!(DriveHandleSink(&mut f.drive).write_filemarks(1).is_err());
    use remanence_parity::RawTapeSink;
    assert!(remanence_parity::DriveHandleRawSink::new(&mut f.drive)
        .write_fixed_block(&[0; 4096])
        .is_err());
    assert_eq!(f.drive.media_write_dispatch_count(), before);
}

#[test]
fn corrupt_trailer_short_block_and_fragment_before_complete_are_refused() {
    let blocks = plain_blocks();
    let mut corrupt = blocks.clone();
    *corrupt.last_mut().unwrap().last_mut().unwrap() = 1;
    let mut short = blocks.clone();
    short.last_mut().unwrap().pop();
    for suffix in [
        corrupt
            .into_iter()
            .map(Record::Block)
            .chain([Record::Filemark])
            .collect::<Vec<_>>(),
        short
            .into_iter()
            .map(Record::Block)
            .chain([Record::Filemark])
            .collect(),
        std::iter::once(Record::Block(blocks[0].clone()))
            .chain([Record::Filemark])
            .chain(blocks.iter().cloned().map(Record::Block))
            .collect(),
    ] {
        let mut f = fixture();
        f.world
            .lock()
            .unwrap()
            .tapes
            .get_mut("TAIL")
            .unwrap()
            .records
            .extend(suffix);
        let error = checkpoint(&mut f.drive).unwrap_err();
        assert!(
            matches!(error, AdmissionError::TailBeyondKnownState { .. }),
            "{error}"
        );
        assert_eq!(writes(&f.commands.lock().unwrap()), 0);
    }
}

#[test]
fn encrypted_unsupported_version_and_geometry_are_not_truncation() {
    use sha2::{Digest, Sha256};
    let payload = vec![0x35; 8192];
    let key = remanence_aead::RecipientPrivateKey::new([1; 16], "test", [7; 32]).unwrap();
    let (sealed, _) = remanence_aead::seal_to_vec(
        &payload,
        &remanence_aead::EnvelopeSealOptions {
            allow_single_recipient: true,
            common: remanence_aead::SealOptions {
                chunk_size: SIZE,
                object_id: "test-object".into(),
                plaintext_size: payload.len() as u64,
                plaintext_digest: Sha256::digest(&payload).into(),
            },
            recipients: vec![key.public_key(0).unwrap()],
        },
    )
    .unwrap();
    let mut torn = fixture();
    torn.world
        .lock()
        .unwrap()
        .tapes
        .get_mut("TAIL")
        .unwrap()
        .records
        .push(Record::Block(sealed[..SIZE as usize].to_vec()));
    let admission = admit_append_boundary(
        &mut torn.drive,
        AppendAuthority::Fresh,
        context(UUID, SIZE, 0, 0),
    )
    .expect("supported envelope with a validated key frame and truncated first payload is torn");
    assert_eq!(admission.boundary().2, 0);
    assert_eq!(writes(&torn.commands.lock().unwrap()), 0);
    for geometry in [false, true] {
        let mut bytes = sealed.clone();
        if geometry {
            bytes[8..12].copy_from_slice(&(SIZE * 2).to_be_bytes());
        } else {
            bytes[6] = 99;
        }
        let mut f = fixture();
        f.world
            .lock()
            .unwrap()
            .tapes
            .get_mut("TAIL")
            .unwrap()
            .records
            .extend(
                bytes
                    .chunks(SIZE as usize)
                    .map(|b| Record::Block(b.to_vec()))
                    .chain([Record::Filemark]),
            );
        let error = checkpoint(&mut f.drive).unwrap_err();
        assert!(
            error.to_string().contains("unclassifiable at LBA 2"),
            "{error}"
        );
        assert_eq!(writes(&f.commands.lock().unwrap()), 0);
    }
}

#[test]
fn fresh_missing_delimiter_restores_position_and_requires_explicit_supersede() {
    let mut f = fixture();
    f.world
        .lock()
        .unwrap()
        .tapes
        .get_mut("TAIL")
        .unwrap()
        .records
        .truncate(1);
    assert!(matches!(
        admit_append_boundary(
            &mut f.drive,
            AppendAuthority::Fresh,
            context(UUID, SIZE, 0, 0)
        ),
        Err(AdmissionError::MissingBootstrapFilemark)
    ));
    assert_eq!(f.drive.position().unwrap().lba, 0);
    assert_eq!(f.drive.media_write_dispatch_count(), 0);
    let mut ctx = context(UUID, SIZE, 0, 0);
    ctx.allow_missing_bootstrap_filemark = true;
    let _admission = admit_append_boundary(&mut f.drive, AppendAuthority::Fresh, ctx).unwrap();
    assert_eq!(f.drive.position().unwrap().lba, 0);
}

#[test]
fn large_torn_candidate_requires_no_temporary_directory() {
    const CHILD: &str = "REM_APPEND_SCAN_NO_TEMP_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "append_admission::tests::large_torn_candidate_requires_no_temporary_directory",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("TMPDIR", "/nonexistent-rem-append-scan-temp")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    struct CutSink(remanence_library::VecBlockSink);
    impl BlockSink for CutSink {
        fn write_block(
            &mut self,
            buf: &[u8],
        ) -> Result<remanence_library::WriteOutcome, TapeIoError> {
            if self.0.blocks.len() == 2 {
                return Err(TapeIoError::OperationFailed(
                    "intentional fixture cut".into(),
                ));
            }
            self.0.write_block(buf)
        }
        fn write_filemarks(
            &mut self,
            n: u32,
        ) -> Result<remanence_library::WriteFilemarksOutcome, TapeIoError> {
            self.0.write_filemarks(n)
        }
        fn position(&mut self) -> Result<TapePosition, TapeIoError> {
            self.0.position()
        }
    }
    let mut options = remanence_format::RemTarObjectOptions::new(
        "large-candidate",
        "large",
        "2026-10-23T00:00:00Z",
        "manifest",
    );
    options.chunk_size = SIZE as usize;
    let spec = remanence_format::RemTarFileSpec::new("large", "large-file", 4u64 << 30, [0; 32]);
    let mut payload = std::io::repeat(0);
    let mut input = [remanence_format::RemTarFileStream {
        spec,
        reader: &mut payload,
    }];
    let mut cut = CutSink(remanence_library::VecBlockSink::new());
    let error = remanence_format::write_rem_tar_object_from_readers(&mut cut, &options, &mut input)
        .unwrap_err();
    assert!(
        error.to_string().contains("intentional fixture cut"),
        "{error}"
    );
    let mut f = fixture();
    {
        let mut world = f.world.lock().unwrap();
        let tape = world.tapes.get_mut("TAIL").unwrap();
        tape.records
            .extend(cut.0.blocks.into_iter().map(Record::Block));
        tape.records
            .extend(std::iter::repeat_n(Record::ZeroBlock(SIZE), 16384));
    }
    let _admission = checkpoint(&mut f.drive).unwrap();
    assert_eq!(f.drive.position().unwrap().lba, 2);
    assert_eq!(f.drive.media_write_dispatch_count(), 0);
}

#[test]
fn transient_error_in_candidate_validation_does_not_classify_a_foreign_tail() {
    let mut f = fixture();
    let blocks = plain_blocks();
    // The measuring walk and physical EOD succeed. Fail the first
    // read through the tape-backed validator, not the initial measurement.
    *f.fail_read_after.lock().unwrap() = Some(blocks.len() + 2);
    f.world
        .lock()
        .unwrap()
        .tapes
        .get_mut("TAIL")
        .unwrap()
        .records
        .extend(
            blocks
                .into_iter()
                .map(Record::Block)
                .chain([Record::Filemark]),
        );
    f.drive.locate(2).unwrap();
    let err = crate::catalogless_restore::scan_append_suffix(
        &mut f.drive,
        &UUID,
        SIZE,
        2,
        None,
        None,
        false,
    )
    .unwrap_err();
    assert!(matches!(err, AdmissionError::Io(_)));
    assert_eq!(writes(&f.commands.lock().unwrap()), 0);
}

#[test]
fn truncated_global_header_without_closing_filemark_is_torn() {
    let mut head = plain_blocks()[0].clone();
    head[124..136].copy_from_slice(b"00000100000\0");
    head[148..156].fill(b' ');
    let checksum: u32 = head[..512].iter().map(|b| u32::from(*b)).sum();
    head[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
    let mut f = fixture();
    f.world
        .lock()
        .unwrap()
        .tapes
        .get_mut("TAIL")
        .unwrap()
        .records
        .push(Record::Block(head));
    let _admission = checkpoint(&mut f.drive).unwrap();
    assert_eq!(writes(&f.commands.lock().unwrap()), 0);
}

/// Every open candidate cut is physical truncation, including the header and trailer.
#[test]
fn all_plaintext_cuts_are_torn_but_closed_corruption_and_complete_prefix_refuse() {
    let blocks = plain_blocks();
    for cut in 1..=blocks.len() {
        let mut f = fixture();
        f.world
            .lock()
            .unwrap()
            .tapes
            .get_mut("TAIL")
            .unwrap()
            .records
            .extend(blocks[..cut].iter().cloned().map(Record::Block));
        let _admission = checkpoint(&mut f.drive).unwrap();
        assert_eq!(f.drive.media_write_dispatch_count(), 0);
    }
    // Partial physical records cut inside the first header and final trailer.
    for candidate in [vec![blocks[0][..128].to_vec()], {
        let mut trailer_cut = blocks.clone();
        let last = trailer_cut.last_mut().unwrap();
        last.truncate(last.len() - 256);
        trailer_cut
    }] {
        let mut f = fixture();
        f.world
            .lock()
            .unwrap()
            .tapes
            .get_mut("TAIL")
            .unwrap()
            .records
            .extend(candidate.into_iter().map(Record::Block));
        let _admission = checkpoint(&mut f.drive).unwrap();
        assert_eq!(f.drive.media_write_dispatch_count(), 0);
    }
    let mut f = fixture();
    f.world
        .lock()
        .unwrap()
        .tapes
        .get_mut("TAIL")
        .unwrap()
        .records
        .extend(
            blocks
                .iter()
                .cloned()
                .map(Record::Block)
                .chain([Record::Filemark, Record::Block(blocks[0].clone())]),
        );
    assert!(matches!(
        checkpoint(&mut f.drive),
        Err(AdmissionError::TailBeyondKnownState { .. })
    ));
}

#[test]
fn encrypted_multiblock_cuts_are_torn_and_closed_corrupt_footer_refuses() {
    use sha2::{Digest, Sha256};
    let payload = vec![0x35; SIZE as usize * 8];
    let key = remanence_aead::RecipientPrivateKey::new([1; 16], "test", [7; 32]).unwrap();
    let (sealed, _) = remanence_aead::seal_to_vec(
        &payload,
        &remanence_aead::EnvelopeSealOptions {
            allow_single_recipient: true,
            common: remanence_aead::SealOptions {
                chunk_size: SIZE,
                object_id: "encrypted-cut".into(),
                plaintext_size: payload.len() as u64,
                plaintext_digest: Sha256::digest(&payload).into(),
            },
            recipients: vec![key.public_key(0).unwrap()],
        },
    )
    .unwrap();
    for cut in [2 * SIZE as usize, 4 * SIZE as usize + 123] {
        let mut f = fixture();
        f.world
            .lock()
            .unwrap()
            .tapes
            .get_mut("TAIL")
            .unwrap()
            .records
            .extend(
                sealed[..cut]
                    .chunks(SIZE as usize)
                    .map(|b| Record::Block(b.to_vec())),
            );
        let _admission = checkpoint(&mut f.drive).unwrap();
    }
    let mut corrupt = sealed;
    *corrupt.last_mut().unwrap() ^= 1;
    let mut f = fixture();
    f.world
        .lock()
        .unwrap()
        .tapes
        .get_mut("TAIL")
        .unwrap()
        .records
        .extend(
            corrupt
                .chunks(SIZE as usize)
                .map(|b| Record::Block(b.to_vec()))
                .chain([Record::Filemark]),
        );
    assert!(matches!(
        checkpoint(&mut f.drive),
        Err(AdmissionError::TailBeyondKnownState { .. })
    ));
}

#[test]
fn unused_fresh_session_abandonment_dispatches_nothing() {
    let root = tempfile::tempdir().unwrap();
    let journal = remanence_state::authority::fixtures::AuthorityBuilder::new(root.path(), UUID)
        .build()
        .unwrap();
    let mut lease = journal.acquire_exclusive().unwrap();
    let mut f = fixture();
    let _admission = admit_append_boundary(
        &mut f.drive,
        AppendAuthority::Fresh,
        context(UUID, SIZE, 0, 0),
    )
    .unwrap();
    abandon_pending_append(&mut f.drive, &mut lease).unwrap();
    assert_eq!(f.drive.media_write_dispatch_count(), 0);
    f.drive.clear_append_admission();
    assert!(abandon_pending_append(&mut f.drive, &mut lease).is_err());
}

/// All physical append entry adapters must refuse a bad predecessor or a bad
/// post-filemark device report, even while a previously good admission is held.
#[tokio::test]
async fn cursor_refusal_matrix_before_any_write() {
    use remanence_parity::{DriveHandleRawSource, PhysicalPositionHint, RawTapeSource};
    let vector = crate::tape_image_vectors::generate("a4-minimal").unwrap();
    let plan = vector.written.terminal_plan.as_ref().unwrap();
    for omitted in [false, true] {
        for route in ["admission", "terminal", "checkpoint", "parity", "overlap"] {
            for fault in ["data", "missing", "short", "io", "lba", "partition"] {
                let mut f = fixture();
                *f.omit_kernel_residual.lock().unwrap() = omitted;
                let _old_admission = checkpoint(&mut f.drive).unwrap();
                // Configure a real fixed-mode writer before sink repositioning.
                let mut config = f.drive.read_config().unwrap();
                config.block_size = remanence_library::BlockSize::Fixed { size_bytes: SIZE };
                f.drive.write_config(config).unwrap();
                match fault {
                    "data" | "short" => {
                        f.world
                            .lock()
                            .unwrap()
                            .tapes
                            .get_mut("TAIL")
                            .unwrap()
                            .records[1] =
                            Record::Block(vec![
                                0x57;
                                if fault == "data" { SIZE as usize } else { 7 }
                            ]);
                    }
                    "missing" => f
                        .world
                        .lock()
                        .unwrap()
                        .tapes
                        .get_mut("TAIL")
                        .unwrap()
                        .records
                        .truncate(1),
                    "io" => {
                        *f.fail_read_after.lock().unwrap() =
                            Some(usize::from(route == "checkpoint"))
                    }
                    "lba" => *f.position_fault.lock().unwrap() = Some((3, 0)),
                    "partition" => *f.position_fault.lock().unwrap() = Some((2, 1)),
                    _ => unreachable!(),
                }
                f.commands.lock().unwrap().clear();
                let boundary = context(UUID, SIZE, 0, 2).boundary;
                let refused = match route {
                    "admission" => checkpoint(&mut f.drive).is_err(),
                    "terminal" => admit_append_boundary(
                        &mut f.drive,
                        AppendAuthority::Terminal { plan },
                        context(UUID, SIZE, 0, 2),
                    )
                    .is_err(),
                    "checkpoint" => {
                        let record = remanence_state::authority::AuthorityCommit {
                            ordinal: 1,
                            object_count: 0,
                            protected: 0,
                            total: 0,
                            cursor: remanence_state::tape_log::CommitCursor {
                                partition: 0,
                                lba: 2,
                            },
                            tape_uuid: UUID,
                            operation_id: [1; 16],
                            next_file: 1,
                            block_size: SIZE,
                            payloads: vec![],
                            scheme: None,
                            object_files: vec![],
                            barrier: None,
                            finalization: None,
                            sealed: false,
                            seal_due: None,
                            check: None,
                        };
                        crate::pool_write::reprove_checkpoint_boundary(&mut f.drive, &record)
                            .is_err()
                    }
                    "parity" => DriveHandleRawSource::new(&mut f.drive)
                        .prove_cursor(PhysicalPositionHint::new(2), SIZE)
                        .is_err(),
                    "overlap" => {
                        let capacity = crate::append_ring::APPEND_RING_SLAB_BYTES as u64 * 2;
                        let manager = crate::io_memory::IoMemoryReservation::new(capacity).unwrap();
                        let (mut producer, _consumer, control) =
                            crate::append_ring::create_append_ring(
                                &manager, capacity, 50, 25, capacity,
                            )
                            .unwrap();
                        producer
                            .push(&vec![0; crate::append_ring::APPEND_RING_SLAB_BYTES])
                            .await
                            .unwrap();
                        // An explicit locate revokes the old proof even at the same C.
                        f.drive.locate(boundary.lba).unwrap();
                        let mut sink = DriveHandleSink(&mut f.drive);
                        let mut overlap = crate::pool_write::overlap::OverlapBlockSink {
                            inner: &mut sink,
                            control,
                            expected_initial_lba: 2,
                            expected_next_lba: 2,
                            initial_position_proved: false,
                            write_started: false,
                            low_water_events: 0,
                        };
                        overlap.locate(2).is_err()
                    }
                    _ => unreachable!(),
                };
                assert!(refused, "{route}/{fault}/omitted={omitted}");
                assert!(
                    DriveHandleSink(&mut f.drive)
                        .write_block(&[1; SIZE as usize])
                        .is_err(),
                    "failed proof left a usable capability: {route}/{fault}"
                );
                assert_eq!(writes(&f.commands.lock().unwrap()), 0, "{route}/{fault}");
            }
        }
    }
}

/// A late locate is dangerous even after the session's first write, and even
/// when it returns to the exact same LBA. Only another proof restores permission.
#[test]
fn cursor_capability_is_revoked_by_later_locate_and_read() {
    for omitted in [false, true] {
        let mut f = fixture();
        *f.omit_kernel_residual.lock().unwrap() = omitted;
        let _admission = checkpoint(&mut f.drive).unwrap();
        DriveHandleSink(&mut f.drive)
            .write_block(&[0x55; SIZE as usize])
            .unwrap();
        DriveHandleSink(&mut f.drive).write_filemarks(1).unwrap();
        f.drive
            .advance_append_commit(context(UUID, SIZE, 0, 4).boundary)
            .unwrap();
        let before = writes(&f.commands.lock().unwrap());
        f.drive.locate(4).unwrap();
        assert!(DriveHandleSink(&mut f.drive).write_filemarks(1).is_err());
        assert_eq!(writes(&f.commands.lock().unwrap()), before);
        let cursor = context(UUID, SIZE, 0, 4).boundary;
        f.drive
            .prove_cursor(cursor, SIZE, f.drive.identity_generation())
            .unwrap();
        assert!(f
            .drive
            .read_block(&mut [0; SIZE as usize])
            .unwrap_err()
            .is_physical_eod());
        assert!(DriveHandleSink(&mut f.drive).write_filemarks(1).is_err());
        f.drive
            .prove_cursor(cursor, SIZE, f.drive.identity_generation())
            .unwrap();
        DriveHandleSink(&mut f.drive).write_filemarks(1).unwrap();
        assert_eq!(writes(&f.commands.lock().unwrap()), before + 1);
    }
}

/// Publication advances C for every commit; old and future filemark boundaries
/// cannot re-arm either entry point, even after the first write was dispatched.
#[test]
fn cursor_rearming_tracks_each_committed_position() {
    let mut f = fixture();
    let _admission = checkpoint(&mut f.drive).unwrap();
    let mut config = f.drive.read_config().unwrap();
    config.block_size = remanence_library::BlockSize::Fixed { size_bytes: SIZE };
    f.drive.write_config(config).unwrap();
    for c in [4, 6, 8] {
        DriveHandleSink(&mut f.drive)
            .write_block(&[0x55; SIZE as usize])
            .unwrap();
        DriveHandleSink(&mut f.drive).write_filemarks(1).unwrap();
        let boundary = context(UUID, SIZE, 0, c).boundary;
        f.drive.advance_append_commit(boundary).unwrap();
        let commands = f.commands.lock().unwrap().len();
        for stale in [2, c - 2, c + 2] {
            assert!(f.drive.locate_admitted_append(stale).is_err());
            assert!(f
                .drive
                .prove_cursor(
                    context(UUID, SIZE, 0, stale).boundary,
                    SIZE,
                    f.drive.identity_generation(),
                )
                .is_err());
            assert!(DriveHandleSink(&mut f.drive).write_filemarks(1).is_err());
        }
        assert_eq!(f.commands.lock().unwrap().len(), commands);
        assert!(f
            .drive
            .advance_append_commit(context(UUID, SIZE, 0, c - 2).boundary)
            .is_err());
        assert_eq!(f.drive.locate_admitted_append(c).unwrap().lba, c);
        f.drive.locate(c).unwrap();
        f.drive
            .prove_cursor(boundary, SIZE, f.drive.identity_generation())
            .unwrap();
        f.drive.check_append_admission().unwrap();
    }
}

/// A synthetic nonzero partition report exercises the admitted locate arguments
/// without any physical device or partition-changing commands.
#[test]
fn admitted_locate_uses_the_admitted_partition() {
    let mut f = fixture();
    *f.reported_partition.lock().unwrap() = 3;
    let ctx = context(UUID, SIZE, 3, 2);
    let _admission = admit_append_boundary(
        &mut f.drive,
        AppendAuthority::Checkpoint { eod: ctx.boundary },
        ctx,
    )
    .unwrap();
    let mut config = f.drive.read_config().unwrap();
    config.block_size = remanence_library::BlockSize::Fixed { size_bytes: SIZE };
    f.drive.write_config(config).unwrap();
    f.drive.locate(0).unwrap();
    assert_eq!(f.drive.locate_admitted_append(2).unwrap().partition, 3);
    f.drive.check_append_admission().unwrap();
}

/// Resume wrappers must re-arm the owning drive, rather than performing an
/// independent head-moving proof that leaves its adapter gate revoked.
#[test]
fn resume_source_wrappers_rearm_the_owning_drive() {
    use remanence_parity::{
        raw::CancellableRawSource, BlockSourceRawTapeSource, DriveHandleRawSource,
        PhysicalPositionHint, RawTapeSource,
    };
    let mut f = fixture();
    let _admission = checkpoint(&mut f.drive).unwrap();
    f.drive.locate(0).unwrap();
    CancellableRawSource(DriveHandleRawSource::new(&mut f.drive))
        .prove_cursor(PhysicalPositionHint::new(2), SIZE)
        .unwrap();
    f.drive.check_append_admission().unwrap();
    f.drive.locate(0).unwrap();
    BlockSourceRawTapeSource::new(&mut DriveHandleSource(&mut f.drive))
        .prove_cursor(PhysicalPositionHint::new(2), SIZE)
        .unwrap();
    f.drive.check_append_admission().unwrap();
    assert_eq!(writes(&f.commands.lock().unwrap()), 0);
}

/// A filemark needs no valid residual when the actual read transfers zero bytes.
/// Every other key, ILI and any data transfer must remain an error.
#[test]
fn filemark_read_sense_and_transfer_matrix() {
    for valid in [false, true] {
        for key in 0..16 {
            for ili in [false, true] {
                for bytes in [0, 1] {
                    let mut f = fixture();
                    let mut sense = vec![0; 18];
                    sense[0] = 0x70 | if valid { 0x80 } else { 0 };
                    sense[2] = 0x80 | key | if ili { 0x20 } else { 0 };
                    sense[7] = 10;
                    sense[13] = 1;
                    *f.response.lock().unwrap() = Some((sense, bytes));
                    let result = f.drive.read_block(&mut [0; SIZE as usize]);
                    assert_eq!(
                        matches!(result, Err(TapeIoError::FilemarkEncountered)),
                        matches!(key, 0 | 1) && !ili && bytes == 0,
                        "valid={valid}, key={key}, ili={ili}, bytes={bytes}: {result:?}"
                    );
                }
            }
        }
    }
}
