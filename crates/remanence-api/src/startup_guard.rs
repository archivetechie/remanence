//! Daemon startup configuration and durable-fence admission guards.

use remanence_state::{CatalogIndex, TapeIoConfig};
use tonic::Status;
use uuid::Uuid;

use crate::hex_encoding::bytes_to_hex;

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
    let tape_io_fences = index.list_active_tape_io_fences().map_err(|error| {
        Status::failed_precondition(format!(
            "startup blocked by active tape-I/O fence: unparseable scope: {error}"
        ))
    })?;
    for first in &tape_io_fences {
        let uuid = Uuid::from_slice(&first.tape_uuid);
        let malformed_uuid = !first.tape_uuid.is_empty() && uuid.is_err();
        let scoped = uuid.as_ref().is_ok_and(|id| !id.is_nil())
            || first
                .barcode
                .as_deref()
                .is_some_and(|b| !b.trim().is_empty())
            || first.endpoint.is_some();
        if scoped && !malformed_uuid {
            tracing::warn!(quarantine_id = %first.quarantine_id,
                tape_uuid = %bytes_to_hex(&first.tape_uuid), barcode = ?first.barcode,
                endpoint = ?first.endpoint, reason = %first.reason,
                release_command = %format!("rem tape quarantine release {} {}", first.quarantine_id, crate::mount_support::fence_release_options(first)),
                "active tape-I/O fence; scoped tape and drive remain refused");
            continue;
        }
        let release_options = crate::mount_support::fence_release_options(first);
        return Err(Status::failed_precondition(format!(
            "startup blocked by active tape-I/O fence {} tape_uuid={} barcode={} reason={}; {} active fence(s); release via `rem tape quarantine release {} {release_options}` before retrying",
            first.quarantine_id,
            Uuid::from_slice(first.tape_uuid.as_slice())
                .map(|uuid| uuid.to_string())
                .unwrap_or_else(|_| bytes_to_hex(first.tape_uuid.as_slice())),
            first.barcode.as_deref().unwrap_or("(unknown)"),
            first.reason,
            tape_io_fences.len(),
            first.quarantine_id,
        )));
    }
    Ok(())
}
