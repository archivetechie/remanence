//! Daemon startup configuration and durable-fence admission guards.

use remanence_state::{CatalogIndex, TapeIoConfig};
use tonic::Status;
use uuid::Uuid;

use crate::hex_encoding::bytes_to_hex;
use crate::startup_media_readiness::status_from_state_error;

pub(crate) fn tape_io_runtime_config(
    config: &TapeIoConfig,
) -> remanence_library::TapeIoRuntimeConfig {
    remanence_library::TapeIoRuntimeConfig {
        staging_ring_buffers: config.staging_ring_buffers,
        write_batch_blocks: config.write_batch_blocks,
        read_batch_blocks: config.read_batch_blocks,
        position_check_bytes: config.position_check_bytes,
    }
}

pub(crate) fn reject_active_tape_io_fences_on_startup(index: &CatalogIndex) -> Result<(), Status> {
    let tape_io_fences = index
        .list_active_tape_io_fences()
        .map_err(status_from_state_error)?;
    if let Some(first) = tape_io_fences.first() {
        return Err(Status::failed_precondition(format!(
            "startup blocked by active tape-I/O fence {} tape_uuid={} barcode={} reason={}; release via `rem tape quarantine release {}` before retrying",
            first.quarantine_id,
            Uuid::from_slice(first.tape_uuid.as_slice())
                .map(|uuid| uuid.to_string())
                .unwrap_or_else(|_| bytes_to_hex(first.tape_uuid.as_slice())),
            first.barcode.as_deref().unwrap_or("(unknown)"),
            first.reason,
            first.quarantine_id,
        )));
    }
    Ok(())
}
