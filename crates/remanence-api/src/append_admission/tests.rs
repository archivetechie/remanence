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
            if let Some((sense, bytes_transferred)) = self.response.lock().unwrap().take() {
                return Err(ScsiError::CheckCondition {
                    sense,
                    bytes_transferred,
                });
            }
        }
        self.inner.execute_in(cdb, buf)
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
    let drive = DriveHandle::open_standalone_with_transport(
        Path::new("/dev/sg-append-test"),
        Box::new(ProbeTransport {
            inner: ModelTransport::new(world.clone(), DeviceRole::Drive { bay: 0x100 }),
            response: response.clone(),
            fail_read_after: fail_read_after.clone(),
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
    assert_eq!(commands, [0x92, 0x34, 0x08, 0x92, 0x34]);
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
                assert_eq!(*f.commands.lock().unwrap(), [0x92, 0x34, 0x08, 0x92, 0x34]);
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
    let journal = remanence_state::FileCheckpointJournal::open(root.path(), UUID).unwrap();
    let lease = journal.acquire_exclusive().unwrap();
    let mut f = fixture();
    let _admission = admit_append_boundary(
        &mut f.drive,
        AppendAuthority::Fresh,
        context(UUID, SIZE, 0, 0),
    )
    .unwrap();
    abandon_pending_append(&mut f.drive, &lease, None).unwrap();
    assert_eq!(f.drive.media_write_dispatch_count(), 0);
    f.drive.clear_append_admission();
    assert!(abandon_pending_append(&mut f.drive, &lease, None).is_err());
}
