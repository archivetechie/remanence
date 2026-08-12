//! Drive-mode configuration shared by production write paths.
//!
//! A successful MODE SELECT is only an acknowledgement that the command was
//! accepted.  Write geometry becomes authoritative only after MODE SENSE
//! reports the requested fixed block size with hardware compression disabled.

use remanence_library::{BlockSize, DriveHandle, TapeConfig, TapeIoError};

/// Build the fixed-block, no-compression target while preserving media facts
/// and drive limits reported by the preceding MODE SENSE.
pub(crate) fn fixed_uncompressed_target(current: TapeConfig, block_size: u32) -> TapeConfig {
    TapeConfig {
        block_size: BlockSize::Fixed {
            size_bytes: block_size,
        },
        compression: false,
        max_block_size_bytes: current.max_block_size_bytes,
        write_protected: current.write_protected,
        worm: current.worm,
    }
}

/// Apply and read back the exact mode required before a Remanence media write.
///
/// The caller must perform its media-policy checks against `current` first.
/// This function issues no block write, filemark, or tape-motion command when
/// the readback disagrees; the mismatch remains a pre-write failure.
pub(crate) fn configure_fixed_uncompressed_write(
    drive: &mut DriveHandle,
    current: TapeConfig,
    block_size: u32,
) -> Result<(TapeConfig, TapeConfig), TapeIoError> {
    let target = fixed_uncompressed_target(current, block_size);
    drive.write_config(target)?;
    let observed = drive.read_config()?;
    verify_fixed_uncompressed_write(target, observed)?;
    Ok((target, observed))
}

fn verify_fixed_uncompressed_write(
    target: TapeConfig,
    observed: TapeConfig,
) -> Result<(), TapeIoError> {
    if observed.block_size != target.block_size || observed.compression {
        return Err(TapeIoError::OperationFailed(format!(
            "fixed uncompressed write-mode verification mismatch: expected block_size={:?} compression=false, got block_size={:?} compression={}",
            target.block_size, observed.block_size, observed.compression
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use remanence_chaos::model::{ModelTransport, VirtualTape, VirtualWorld};
    use remanence_library::{SgTransport, StaticAllowlist};

    use super::*;

    fn drive_ignoring_mode_select(
        block_size: u32,
        compression: bool,
    ) -> (DriveHandle, Arc<Mutex<VirtualWorld>>) {
        const BAY: u16 = 0x0100;
        const LIBRARY_SERIAL: &str = "LIB-MODE-READBACK";
        let mut tape = VirtualTape::empty(64 * 1024 * 1024, block_size);
        tape.compression = compression;
        tape.retain_block_size_on_mode_select = true;
        tape.retain_compression_on_mode_select = true;
        let mut world =
            VirtualWorld::single_drive(LIBRARY_SERIAL, BAY, "DRV-MODE-READBACK", 0x0400, 1);
        world.put_tape_in_drive(BAY, "MODE001L9", Some(0x0400), tape);
        let world = Arc::new(Mutex::new(world));
        let library = world.lock().expect("world lock").library_snapshot();
        let policy = StaticAllowlist::new([LIBRARY_SERIAL]);
        let factory_world = Arc::clone(&world);
        let mut library = library
            .open_with(&policy, move |path| {
                let role = factory_world
                    .lock()
                    .expect("world lock")
                    .role_for_path(path)
                    .expect("known model path");
                Ok::<_, remanence_library::IoErrorKind>(Box::new(ModelTransport::new(
                    Arc::clone(&factory_world),
                    role,
                )) as Box<dyn SgTransport>)
            })
            .expect("open model library");
        let drive = library.open_drive(BAY, &policy).expect("open model drive");
        (drive, world)
    }

    #[test]
    fn successful_mode_select_that_retains_compression_is_rejected_before_media_write() {
        let (mut drive, world) = drive_ignoring_mode_select(4096, true);
        let current = drive.read_config().expect("read current mode");

        let error = configure_fixed_uncompressed_write(&mut drive, current, 4096)
            .expect_err("retained compression must fail closed");

        assert!(error.to_string().contains("compression=true"), "{error}");
        let world = world.lock().expect("world lock");
        let tape = world.tapes.get("MODE001L9").expect("loaded tape");
        assert!(tape.records.is_empty());
        assert_eq!(tape.written_bytes, 0);
        assert!(!world
            .command_log
            .iter()
            .any(|command| matches!(command.opcode, 0x0a | 0x10)));
    }

    #[test]
    fn successful_mode_select_that_retains_wrong_block_size_is_rejected_before_media_write() {
        let (mut drive, world) = drive_ignoring_mode_select(8192, false);
        let current = drive.read_config().expect("read current mode");

        let error = configure_fixed_uncompressed_write(&mut drive, current, 4096)
            .expect_err("retained block size must fail closed");

        assert!(error.to_string().contains("size_bytes: 8192"), "{error}");
        let world = world.lock().expect("world lock");
        let tape = world.tapes.get("MODE001L9").expect("loaded tape");
        assert!(tape.records.is_empty());
        assert_eq!(tape.written_bytes, 0);
        assert!(!world
            .command_log
            .iter()
            .any(|command| matches!(command.opcode, 0x0a | 0x10)));
    }
}
