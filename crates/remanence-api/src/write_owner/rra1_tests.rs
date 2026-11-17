//! Reset-recovery caller regressions using the durability model and native authority.
use super::*;
use remanence_chaos::model::DeviceRole;
use remanence_library::{BlockSize, TapeIoErrorKind};

/// Remember fixed mode, consume the reset on this handle, and start command observation.
fn arm_reset(drive: &mut DriveHandle, world: &Arc<Mutex<VirtualWorld>>) -> usize {
    let config = drive.read_config().expect("read fixed mode before reset");
    assert_eq!(config.block_size, BlockSize::Fixed { size_bytes: BLOCK });
    println!("A1 block size: {:?}", config.block_size);
    {
        let mut world = world.lock().unwrap();
        let state = world.drive_states.get_mut(&BAY).unwrap();
        state.reset_unit_attention = true;
        state.position_unknown = true;
    }
    let error = drive
        .position()
        .expect_err("consume the reset on the closeout handle");
    assert!(matches!(
        error.kind,
        TapeIoErrorKind::StateInvalidatingReset(_)
    ));
    println!("A3 error kind: {:?}", error.kind);
    world.lock().unwrap().command_log.len()
}

/// Observe only commands issued to the drive used by maintenance closeout.
fn drive_opcodes(world: &Arc<Mutex<VirtualWorld>>, start: usize) -> Vec<u8> {
    world.lock().unwrap().command_log[start..]
        .iter()
        .filter(|command| command.role == DeviceRole::Drive { bay: BAY })
        .map(|command| command.opcode)
        .collect()
}

/// Exercise the production closeout route with its real reservation and a no-op ready callback.
fn closeout(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive: &mut DriveHandle,
) -> Result<(), Status> {
    let permit = cfg
        .io_memory
        .reserve_closeout(&scheme(), BLOCK, 1, TAPE)
        .unwrap();
    let progress =
        crate::closeout_progress::CloseoutProgress::new(Uuid::new_v4(), TAPE, None).unwrap();
    super::super::super::closeout::handle(
        index,
        cfg,
        drive,
        super::super::super::closeout::CloseoutWork {
            tape_uuid: TAPE,
            expected_pool: Some(POOL),
            progress: &progress,
            permit,
        },
        |_, _| Ok(()),
    )
}

/// A READ POSITION medium error during reset recovery uses the media retry budget.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn rra1_reset_position_medium_error_is_media() {
    reset_position_failure_is_media((0x03, 0x11, 0x00)).await;
}

/// A READ POSITION hardware error during reset recovery uses the same media retry budget.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn rra1_reset_position_hardware_error_is_media() {
    reset_position_failure_is_media((0x04, 0x44, 0x00)).await;
}

/// Each sense case owns a fresh tape, authority, model world, and reset-invalidated handle.
#[cfg(target_os = "linux")]
async fn reset_position_failure_is_media(sense: (u8, u8, u8)) {
    let (typed, root) = typed_root();
    let mut index = typed.catalog().write_handle();
    let world = setup(&root);
    typed_due_tape(&typed, &root, world.clone()).await;
    let mut library = open_model_library(world.clone());
    let cfg = typed_config(&typed, &library);
    let mut drive = library.open_drive(BAY, &cfg.policy).unwrap();
    let start = arm_reset(&mut drive, &world);
    world
        .lock()
        .unwrap()
        .drive_states
        .get_mut(&BAY)
        .unwrap()
        .next_read_position_sense = Some(sense);
    let result = closeout(&mut index, &cfg, &mut drive);
    assert_eq!(drive_opcodes(&world, start), [0x05, 0x1a, 0x34]);
    let error = result.expect_err("reset recovery READ POSITION must fail");
    assert_eq!(error.metadata().get("remanence-retry").unwrap(), "media");
}

/// A mode mismatch refuses both attempts without a position command, tape fence, or media write.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn rra1_reset_block_size_mismatch_remains_transient() {
    let (typed, root) = typed_root();
    let mut index = typed.catalog().write_handle();
    let world = setup(&root);
    typed_due_tape(&typed, &root, world.clone()).await;
    let mut library = open_model_library(world.clone());
    let cfg = typed_config(&typed, &library);
    let mut drive = library.open_drive(BAY, &cfg.policy).unwrap();
    arm_reset(&mut drive, &world);
    world
        .lock()
        .unwrap()
        .tapes
        .get_mut(BARCODE)
        .unwrap()
        .block_size = BLOCK / 2;
    let dispatches = drive.media_write_dispatch_count();
    for _ in 0..2 {
        let start = world.lock().unwrap().command_log.len();
        let result = closeout(&mut index, &cfg, &mut drive);
        assert_eq!(drive_opcodes(&world, start), [0x05, 0x1a]);
        let error = result.expect_err("reset recovery must reject the changed fixed block size");
        assert_eq!(
            error.metadata().get("remanence-retry").unwrap(),
            "transient"
        );
    }
    assert!(index
        .tape_io_admission_conflicts(&TAPE, Some(BARCODE))
        .unwrap()
        .is_empty());
    assert_eq!(drive.media_write_dispatch_count(), dispatches);
}

/// An unarmed handle starts write preparation with REWIND, without a reset position probe.
#[cfg(target_os = "linux")]
#[test]
fn rra1_unarmed_write_preparation_starts_with_rewind() {
    let root = tempfile::tempdir().unwrap();
    let world = setup(root.path());
    let mut library = open_model_library(world.clone());
    let cfg = config(root.path(), &library);
    let mut drive = library.open_drive(BAY, &cfg.policy).unwrap();
    let start = world.lock().unwrap().command_log.len();
    let result = prepare_drive_for_write(
        &mut drive,
        &TAPE,
        BLOCK,
        Uuid::new_v4(),
        WriteMediaPolicy::RewritableObject,
    );
    assert_eq!(drive_opcodes(&world, start).first(), Some(&0x01));
    result.expect("prepare the clean durability fixture");
}
