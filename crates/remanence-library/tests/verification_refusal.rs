//! Verification READ refusals return typed errors without dispatching to the tape.

use remanence_library::transport::{FixtureTransport, RecordingLog, RecordingTransport};
use remanence_library::{DriveHandle, StaticAllowlist, TapeIoErrorKind};
use std::path::Path;
use std::sync::{atomic::AtomicBool, Arc, OnceLock};
use std::time::{Duration, Instant};

/// Open the review's standalone fixture and retain its dispatched CDB log.
fn drive() -> (DriveHandle, RecordingLog) {
    let fixture = FixtureTransport::new().with_responses([
        include_bytes!("../../../fixtures/inquiry/drive1-lto9.bin").to_vec(),
        vec![8, 0x80, 0, 3, b'W', b'B', b'R'],
    ]);
    let (transport, log) = RecordingTransport::new(fixture);
    let drive = DriveHandle::open_standalone_with_transport(
        Path::new("/fixture/wbr"),
        Box::new(transport),
        &StaticAllowlist::new(["/fixture/wbr"]),
    )
    .unwrap();
    (drive, log)
}

/// Repeated cancellation keeps the handle's command outcome boundaries usable.
#[test]
fn verification_cancel_before_single_read_returns_error() {
    let (mut drive, log) = drive();
    let context = drive.verification_context();
    let _verification = context.verification();
    let _control = context.control(Arc::new(AtomicBool::new(true)), Arc::new(|_, _, _| {}));

    for _ in 0..2 {
        let error = drive.read_block(&mut [0; 512]).unwrap_err();
        assert!(matches!(error.kind, TapeIoErrorKind::Cancelled));
    }
    assert!(log.borrow().iter().all(|cdb| cdb.first() != Some(&0x08)));
}

/// An expired shutdown deadline refuses verification before a READ is issued.
#[test]
fn verification_deadline_before_single_read_returns_error() {
    let (mut drive, log) = drive();
    let context = drive.verification_context();
    let _verification = context.verification();
    let deadline = Arc::new(OnceLock::new());
    deadline
        .set(Instant::now() - Duration::from_secs(1))
        .unwrap();
    drive.set_command_deadline(Some(deadline));

    let error = drive.read_block(&mut [0; 512]).unwrap_err();
    assert!(matches!(error.kind, TapeIoErrorKind::InvalidRequest(_)));
    assert!(log.borrow().iter().all(|cdb| cdb.first() != Some(&0x08)));
}
