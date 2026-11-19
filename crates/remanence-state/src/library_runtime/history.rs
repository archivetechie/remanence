//! Runtime history captures authority limits, then visits records without runtime locks.
use super::*;
use std::ops::ControlFlow;

impl LibraryRuntime {
    /// Visit the captured library history, reporting standing or byte damage at its end.
    /// The visitor runs without dispatch, native-view or authority-log locks held.
    pub fn history(
        &self,
        visit: &mut dyn FnMut(&HistoryEntry<'_>) -> ControlFlow<()>,
    ) -> Result<HistoryEnd, RuntimeError> {
        let (snapshot, standing) = self.0.authority.history_snapshot()?;
        Ok(lib::history::walk(&snapshot, standing, visit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    fn runtime(root: &std::path::Path) -> LibraryRuntime {
        let paths = StatePaths {
            config_path: root.join("config"),
            state_dir: root.join("state"),
            audit_dir: root.join("library"),
            journal_dir: root.join("native"),
            sqlite_path: root.join("catalog.sqlite"),
            tape_cache_dir: root.join("cache"),
            calibration_dir: root.join("calibration"),
        };
        let lease = Arc::new(StateLockGuard::acquire(&paths.state_dir).unwrap());
        LibraryRuntime::open(&paths, true, lease).unwrap()
    }
    fn seed(runtime: &LibraryRuntime, n: usize) -> lib::Receipt {
        runtime
            .append(|authority, view| {
                authority.diagnostic(
                    lib::Diagnostic {
                        event: lib::DiagnosticEvent::RequestReceived,
                        subject_kind: "history-concurrency".into(),
                        subject_id: Some(n.to_string().as_str().into()),
                        detail: lib::Bounded(vec![]),
                    },
                    lib::AuditActor::System,
                    lib::SourceLayer::Layer4,
                    view,
                )
            })
            .unwrap()
    }

    /// S6: callbacks release every writer lock; the first walk retains its original limits.
    #[test]
    fn s6_captured_limits_and_last_record_break() {
        let root = tempfile::tempdir().unwrap();
        let runtime = runtime(root.path());
        let expected: Vec<_> = (0..3).map(|n| seed(&runtime, n).sequence).collect();
        assert_eq!(expected, vec![1, 2, 3]);
        let mut stopped = vec![];
        assert_eq!(
            runtime
                .history(&mut |entry| {
                    stopped.push(entry.head().sequence);
                    if stopped.len() == 3 {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    }
                })
                .unwrap(),
            HistoryEnd::Stopped
        );
        assert_eq!(stopped, expected);

        let writer = runtime.clone();
        let (start_tx, start_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let task = std::thread::spawn(move || {
            start_rx.recv_timeout(Duration::from_secs(120)).unwrap();
            for n in 3..203 {
                seed(&writer, n);
            }
            done_tx.send(()).unwrap();
        });
        let mut sequences = vec![];
        let mut completion = Ok(());
        let end = runtime
            .history(&mut |entry| {
                sequences.push(entry.head().sequence);
                let _: serde_json::Value = serde_json::from_str(&entry.detail_json()).unwrap();
                if sequences.len() == 1 {
                    start_tx.send(()).unwrap();
                    completion = done_rx.recv_timeout(Duration::from_secs(120));
                }
                ControlFlow::Continue(())
            })
            .unwrap();
        task.join().unwrap();
        completion.expect("writer must finish while the first visitor is running");
        assert_eq!(end, HistoryEnd::Clean);
        assert_eq!(sequences, expected);
        let mut after = vec![];
        assert_eq!(
            runtime
                .history(&mut |entry| {
                    after.push(entry.head().sequence);
                    let _: serde_json::Value = serde_json::from_str(&entry.detail_json()).unwrap();
                    ControlFlow::Continue(())
                })
                .unwrap(),
            HistoryEnd::Clean
        );
        assert_eq!(after, (1..=203).collect::<Vec<_>>());
    }
}
