// Shared advancing clock fixture for writer and persisted-intent tests.

pub(crate) fn advancing_test_identity() -> (WriterIdentity, Arc<std::sync::atomic::AtomicI64>) {
    use std::sync::atomic::{AtomicI64, Ordering};
    let calls = Arc::new(AtomicI64::new(0));
    let clock = Arc::clone(&calls);
    let identity = WriterIdentity::with_clock("remanence-advancing-test".into(), move || {
        OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(clock.fetch_add(1, Ordering::SeqCst))
    });
    (identity, calls)
}
