//! Golden S15 cases enter the production parity-bootstrap owner. Its drive and
//! adapters construct each returned error and supply the operation context.
//! Admission rows use their real funnel and renderer, never the S15 text table.
use super::*;
use remanence_library::{transport::TransferOutcome, ScsiError};

#[derive(Clone, Copy, Debug)]
enum Fault {
    Sense(u8, u8, u8),
    Transport(&'static str),
    Status(u8),
    Cancelled,
    Invalid,
    HostCapacity(nix::errno::Errno),
    Short,
    FenceNotDurable,
    Ephemeral,
}
struct Faulting {
    inner: ModelTransport,
    fault: Fault,
    fired: Arc<Mutex<bool>>,
    classifications_at_fault: Arc<Mutex<Option<u64>>>,
}
impl SgTransport for Faulting {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        if matches!(self.fault, Fault::Ephemeral)
            && cdb[0] == 0x12
            && cdb[1] & 1 != 0
            && cdb[2] == 0x80
        {
            return Ok(TransferOutcome::clean(0));
        }
        self.inner.execute_in(cdb, buf)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        self.inner.execute_none(cdb)
    }
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        if cdb[0] != 10 || *self.fired.lock().unwrap() {
            return self.inner.execute_out(cdb, buf);
        }
        *self.fired.lock().unwrap() = true;
        *self.classifications_at_fault.lock().unwrap() =
            Some(remanence_library::outcome::census::total());
        match self.fault {
            Fault::Sense(key, asc, ascq) => Err(ScsiError::CheckCondition {
                sense: vec![
                    0x70, 0, key, 0, 0, 0, 0, 10, 0, 0, 0, 0, asc, ascq, 0, 0, 0, 0,
                ],
                bytes_transferred: 0,
            }),
            Fault::Transport(message) => Err(ScsiError::Io(std::io::Error::other(message))),
            Fault::Status(status) => Err(ScsiError::UnexpectedStatus { status }),
            Fault::Cancelled => Err(ScsiError::Cancelled),
            Fault::Invalid => Err(ScsiError::InvalidInput("invalid request")),
            Fault::HostCapacity(errno) => {
                Err(remanence_library::scsi::sg_io::classify_ioctl_error(errno))
            }
            Fault::FenceNotDurable | Fault::Ephemeral => panic!("gate refusal must precede WRITE"),
            Fault::Short => {
                let result = self.inner.execute_out(cdb, buf)?;
                Ok(TransferOutcome::clean(result.bytes_transferred - 1))
            }
        }
    }
}

/// Base text oracle, only applied to errors that actually entered S15.
fn legacy_reason(error: &str) -> &'static str {
    if error.contains("TailBeyondKnownState") || error.contains("append admission") {
        "append_admission_refused"
    } else if error.contains("partial fixed batch uncommittable") {
        "partial_batch"
    } else if error.contains("reset UNIT ATTENTION") {
        "reset_unit_attention"
    } else if error.contains("position drift") || error.contains("position mismatch") {
        "position_drift"
    } else {
        "transfer_error"
    }
}

/// Invoke production fresh-parity bootstrap, including admission, scope, error producer and persistence.
fn bootstrap(fault: Fault) -> (String, String) {
    bootstrap_with_fence_count(fault, 1)
}

/// Pin the same production route for legacy fences and proven host refusals.
fn bootstrap_with_fence_count(fault: Fault, expected_fences: usize) -> (String, String) {
    const BLOCK_SIZE: u32 = 256 * 1024;

    let typed = crate::tests::TypedRoot::open_with_tapes(&[([0x46; 16], "PARITY1L9")]);
    let mut index = typed.catalog().write_handle();
    let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
        "LIB-FRESH-PARITY-FENCE",
        0x0100,
        "DRV-FRESH-PARITY-FENCE",
        0x0400,
        1,
    )));
    world.lock().expect("world lock").put_tape_in_drive(
        0x0100,
        "PARITY001",
        Some(0x0400),
        VirtualTape::empty(1024 * 1024, BLOCK_SIZE),
    );
    let library_model = world.lock().expect("world lock").library_snapshot();
    let policy = remanence_library::StaticAllowlist::new([library_model.serial.as_str()]);
    let factory_world = Arc::clone(&world);
    let fired = Arc::new(Mutex::new(false));
    let capture = fired.clone();
    let classifications_at_fault = Arc::new(Mutex::new(None));
    let capture_classifications = classifications_at_fault.clone();
    let mut library = library_model
        .open_with(&policy, move |path| {
            let role = factory_world
                .lock()
                .expect("world lock")
                .role_for_path(path)
                .expect("known model path");
            let model = ModelTransport::new(Arc::clone(&factory_world), role);
            let transport: Box<dyn SgTransport> =
                if path.to_string_lossy().contains("/sg-chaos-drive-") {
                    Box::new(Faulting {
                        inner: model,
                        fault,
                        fired: capture.clone(),
                        classifications_at_fault: capture_classifications.clone(),
                    })
                } else {
                    Box::new(model)
                };
            Ok::<_, remanence_library::IoErrorKind>(transport)
        })
        .expect("open model library");
    let snapshot = library_snapshot_cell(library.library().clone());
    let cfg = test_typed_write_owner_config(&typed, &library, snapshot);
    let tape_uuid = [0x46; 16];
    let scheme = remanence_parity::default_scheme_for_block_size(BLOCK_SIZE);
    let selected = SelectedTape {
        standalone_endpoint: None,
        pool_id: "fresh-parity-fence-test".to_string(),
        tape_uuid,
        block_size: BLOCK_SIZE,
        parity_config: ParityConfig::Scheme(scheme.clone()),
    };

    let bootstrap = crate::build_tape_bootstrap(
        tape_uuid,
        BLOCK_SIZE,
        ParityConfig::Scheme(scheme.clone()),
        "2026-10-23T00:00:00Z",
        "test",
    );
    let mut block = vec![0; BLOCK_SIZE as usize];
    write_bootstrap_block(&bootstrap, &mut block).unwrap();
    world
        .lock()
        .unwrap()
        .tapes
        .get_mut("PARITY001")
        .unwrap()
        .records = vec![Record::Block(block), Record::Filemark];
    let mut drive = if matches!(fault, Fault::Ephemeral) {
        let path = Path::new("/fixture/fix2-ephemeral");
        DriveHandle::open_standalone_with_transport(
            path,
            Box::new(Faulting {
                inner: ModelTransport::new(
                    world.clone(),
                    remanence_chaos::model::DeviceRole::Drive { bay: 0x0100 },
                ),
                fault,
                fired: fired.clone(),
                classifications_at_fault: classifications_at_fault.clone(),
            }),
            &remanence_library::StaticAllowlist::new(["/fixture/fix2-ephemeral"])
                .with_unserialized_device_allowed(path),
        )
        .unwrap()
    } else {
        library.open_drive(0x0100, &policy).expect("open drive")
    };
    let _admission = crate::admit_initialized_tape(&mut drive, tape_uuid, BLOCK_SIZE).unwrap();
    if matches!(fault, Fault::FenceNotDurable) {
        struct Refuse;
        impl remanence_library::MediaWriteFence for Refuse {
            fn fence_media_write(&mut self) -> Result<(), remanence_library::MediaWriteFenceError> {
                Err(remanence_library::MediaWriteFenceError::new(
                    "fix2 durable transaction failed",
                ))
            }
        }
        drive.install_media_write_fence(Box::new(Refuse));
    }
    let status = {
        let checkpoint_journal = remanence_state::authority::fixtures::AuthorityBuilder::new(
            &cfg.checkpoint_journal_dir,
            tape_uuid,
        )
        .build()
        .expect("open fresh checkpoint journal");
        let mut checkpoint_lease = checkpoint_journal
            .acquire_exclusive()
            .expect("acquire fresh checkpoint lease");
        let authority = validate_parity_actor_authority(&selected, &mut checkpoint_lease)
            .expect("validate fresh parity authority");
        match open_parity_actor_session(
            &mut index,
            &mut drive,
            &cfg,
            &selected,
            &[],
            authority,
            &mut checkpoint_lease,
            None,
        ) {
            Ok(_) => panic!("short bootstrap completion must fail closed"),
            Err(status) => status,
        }
    };

    let attempted = !matches!(fault, Fault::FenceNotDurable | Fault::Ephemeral);
    let host_refused = matches!(fault, Fault::HostCapacity(_));
    let dispatched = attempted && !host_refused;
    assert_eq!(
        *fired.lock().unwrap(),
        attempted,
        "WRITE transport call: {fault:?}"
    );
    assert_eq!(drive.media_write_dispatch_count(), u64::from(dispatched));
    if host_refused {
        assert!(status.message().contains("no command issued"), "{status}");
        assert_eq!(
            remanence_library::outcome::census::total()
                - classifications_at_fault
                    .lock()
                    .unwrap()
                    .expect("WRITE attempted"),
            1,
            "host refusal must reach S15 without a second abandoned-write classification"
        );
    }
    if !attempted {
        assert!(
            status
                .message()
                .contains(if matches!(fault, Fault::FenceNotDurable) {
                    "durable write-epoch"
                } else {
                    "ephemeral"
                }),
            "{status}"
        );
    }
    let fences = index.list_active_tape_io_fences().unwrap();
    assert_eq!(
        fences.len(),
        expected_fences,
        "{fault:?}: {status}; {fences:?}"
    );
    (
        status.message().to_string(),
        fences
            .first()
            .map(|fence| fence.reason.clone())
            .unwrap_or_default(),
    )
}

/// The production golden rows preserve every base fence; D1 changes only collision reasons.
#[test]
fn b3b_golden_base_display_fence_table() {
    let rows = [
        (
            "write fence not durable",
            Fault::FenceNotDurable,
            "transfer_error",
            "transfer_error",
        ),
        (
            "ephemeral read only",
            Fault::Ephemeral,
            "transfer_error",
            "transfer_error",
        ),
        (
            "medium",
            Fault::Sense(3, 0x0c, 0),
            "transfer_error",
            "transfer_error",
        ),
        (
            "hardware",
            Fault::Sense(4, 0x44, 0),
            "transfer_error",
            "transfer_error",
        ),
        (
            "definite 05h",
            Fault::Sense(5, 0x24, 0),
            "transfer_error",
            "transfer_error",
        ),
        (
            "data protect",
            Fault::Sense(7, 0x27, 0),
            "transfer_error",
            "transfer_error",
        ),
        (
            "not ready",
            Fault::Sense(2, 0x04, 1),
            "transfer_error",
            "transfer_error",
        ),
        (
            "no medium",
            Fault::Sense(2, 0x3a, 0),
            "transfer_error",
            "transfer_error",
        ),
        (
            "reset",
            Fault::Sense(6, 0x29, 0),
            "reset_unit_attention",
            "reset_unit_attention",
        ),
        (
            "short bootstrap",
            Fault::Short,
            "partial_batch",
            "partial_batch",
        ),
        ("busy", Fault::Status(8), "transfer_error", "transfer_error"),
        (
            "cancelled",
            Fault::Cancelled,
            "transfer_error",
            "transfer_error",
        ),
        (
            "invalid input",
            Fault::Invalid,
            "transfer_error",
            "transfer_error",
        ),
        (
            "transport",
            Fault::Transport("link lost"),
            "transfer_error",
            "transfer_error",
        ),
        (
            "transport / TailBeyondKnownState",
            Fault::Transport("TailBeyondKnownState"),
            "append_admission_refused",
            "transfer_error",
        ),
        (
            "transport / append admission",
            Fault::Transport("append admission"),
            "append_admission_refused",
            "transfer_error",
        ),
        (
            "transport / partial",
            Fault::Transport("partial fixed batch uncommittable"),
            "partial_batch",
            "transfer_error",
        ),
        (
            "transport / reset",
            Fault::Transport("reset UNIT ATTENTION"),
            "reset_unit_attention",
            "transfer_error",
        ),
        (
            "transport / drift",
            Fault::Transport("position drift"),
            "position_drift",
            "transfer_error",
        ),
        (
            "transport / mismatch",
            Fault::Transport("position mismatch"),
            "position_drift",
            "transfer_error",
        ),
    ];
    let mut changed = 0;
    for (name, fault, old, expected) in rows {
        let (message, actual) = bootstrap(fault);
        assert_eq!(legacy_reason(&message), old, "{name}: {message}");
        assert_eq!(actual, expected, "{name}: {message}");
        changed += usize::from(old != expected);
        eprintln!("golden S15: {name}: {old} -> {actual}");
    }
    assert_eq!(changed, 6);
    eprintln!("golden S15: 20 production rows, 6 D1 reason changes, 0 removed fences");
}

/// S3: pre-execution-looking command failures cannot remove the enclosing S15 fence.
#[test]
fn b3b_fix1_golden_s15_preserves_base_fences() {
    for fault in [
        Fault::Cancelled,
        Fault::Invalid,
        Fault::Sense(5, 0x24, 0),
        Fault::Sense(7, 0x27, 0),
    ] {
        assert_eq!(bootstrap(fault).1, "transfer_error");
    }
}

/// S4: all admission variants originate in the actual physical admission funnel.
#[test]
fn b3b_fix1_golden_admission_uses_its_production_route() {
    crate::append_admission::tests::golden_production_admission_rows();
}

/// An attempted bootstrap WRITE preserves the base fence even when its durability gate refuses it.
#[test]
fn b3b_fix2_bootstrap_write_fence_not_durable() {
    assert_eq!(bootstrap(Fault::FenceNotDurable).1, "transfer_error");
}

/// An ephemeral bootstrap attempt reaches the production read-only gate without dispatching a CDB.
#[test]
fn b3b_fix2_bootstrap_ephemeral_read_only() {
    assert_eq!(bootstrap(Fault::Ephemeral).1, "transfer_error");
}

/// A proven pre-dispatch capacity refusal survives the S15 attempt guard without a fence.
#[test]
fn ioctl_capacity_parity_bootstrap_refusals_raise_no_failure_fence() {
    for errno in [nix::errno::Errno::ENOMEM, nix::errno::Errno::EINVAL] {
        let (_, reason) = bootstrap_with_fence_count(Fault::HostCapacity(errno), 0);
        assert!(reason.is_empty());
    }
}
