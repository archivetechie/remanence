//! Preserve a typed local write failure through the existing Status-based actor channel.
//!
//! Tonic serializes the same code/message/details as before; the error source is
//! process-local and lets the mounted adapter retain the original write cause.

use crate::PoolWriteError;
use std::{error::Error, fmt, sync::Arc};
use tonic::Status;

#[derive(Debug)]
pub struct ActorFailure {
    pub status: Status,
    pub pool_write_error: Option<Arc<PoolWriteError>>,
}

#[derive(Debug)]
struct WriteCause(Arc<PoolWriteError>);
impl fmt::Display for WriteCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl Error for WriteCause {}

impl ActorFailure {
    /// Legacy Display sinks must not expose this commit's process-local source.
    /// Preserve every wire field and leave any unrelated error source untouched.
    pub(crate) fn without_local_cause(status: Status) -> Status {
        if status
            .source()
            .is_some_and(|source| source.is::<WriteCause>())
        {
            Status::with_details_and_metadata(
                status.code(),
                status.message(),
                status.details().to_vec().into(),
                status.metadata().clone(),
            )
        } else {
            status
        }
    }
    pub(crate) fn preserve(original: &Status, mut replacement: Status) -> Status {
        if let Some(source) = original
            .source()
            .and_then(|s| s.downcast_ref::<WriteCause>())
        {
            replacement.set_source(Arc::new(WriteCause(source.0.clone())));
        }
        replacement
    }
    pub(crate) fn attach(mut status: Status, error: PoolWriteError) -> Status {
        status.set_source(Arc::new(WriteCause(Arc::new(error))));
        status
    }
}
impl From<Status> for ActorFailure {
    fn from(status: Status) -> Self {
        let pool_write_error = status
            .source()
            .and_then(|source| source.downcast_ref::<WriteCause>())
            .map(|cause| Arc::clone(&cause.0));
        Self {
            status,
            pool_write_error,
        }
    }
}
impl fmt::Display for ActorFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.status.message())
    }
}
impl Error for ActorFailure {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capacity_cause_survives_status_wrapping_without_wire_or_display_changes() {
        let selected = crate::pool_write::SelectedTape {
            standalone_endpoint: Some(remanence_library::DriveKey::Standalone {
                drive_serial: "local".into(),
            }),
            tape_uuid: [0; 16],
            block_size: 262144,
            parity_config: remanence_parity::ParityConfig::None,
            pool_id: "default".into(),
        };
        let status = super::super::restore::status_from_selected_write_error(
            &selected,
            PoolWriteError::SelectedTapeInsufficientCapacity {
                object_size: 31,
                raw_capacity: 17,
                used: 9,
            },
        );
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        let message = status.message().to_owned();
        let legacy = ActorFailure::without_local_cause(status.clone());
        assert!(legacy.source().is_none());
        assert_eq!(
            legacy.to_string(),
            Status::new(status.code(), status.message()).to_string()
        );
        let refusal = crate::mount_support::refusal(legacy);
        assert!(
            matches!(refusal, crate::MountError::Refused(ref text) if text == &Status::new(status.code(), status.message()).to_string())
        );
        let failure = ActorFailure::from(status.clone());
        assert_eq!(failure.to_string(), message);
        let wire = status.clone().into_http::<()>();
        assert_eq!(wire.headers()["grpc-status"], "9");
        let wrapped = ActorFailure::preserve(&status, Status::internal("host suffix failed"));
        let failure = ActorFailure::from(wrapped);
        assert!(matches!(
            failure.pool_write_error.as_deref(),
            Some(PoolWriteError::SelectedTapeInsufficientCapacity {
                object_size: 31,
                raw_capacity: 17,
                used: 9
            })
        ));
        assert_eq!(failure.to_string(), "host suffix failed");
    }
}
