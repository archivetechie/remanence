//! Disconnected projector worker. Requests contain durable fold-issued records
//! or frozen tape snapshots. A response never changes authority, and retry uses
//! the same apply path. Readers open their own read-only WAL connections.
use super::{
    apply::{ApplyError, ApplyPath, AuthorityRecord, Watermarks},
    CatalogIndex, CatalogReader,
};
use crate::StateError;
use std::{
    path::{Path, PathBuf},
    sync::{mpsc, Arc},
};

/// Completed projection response, including its independently tracked progress.
#[derive(Debug)]
pub enum ProjectionResult {
    /// No fold dependencies remain; counts distinguish applications from non-applications.
    Applied(Watermarks),
    /// The fold still has pending records (or a native prefix remains incomplete).
    Pending(Watermarks),
    /// SQL failed; retry from durable authority remains safe.
    Failed {
        /// Projection error; durable authority remains unchanged.
        error: ApplyError,
        /// Last committed source watermarks, if SQL remains readable.
        watermarks: Option<Watermarks>,
    },
}
enum Request {
    Apply {
        source: AuthorityRecord,
        path: ApplyPath,
        reply: mpsc::Sender<ProjectionResult>,
    },
    Snapshot {
        snapshot: crate::library_log::LibrarySnapshot,
        view: Box<dyn crate::library_log::TapeView + Send>,
        path: ApplyPath,
        reply: mpsc::Sender<Result<LibraryRebuildReport, ApplyError>>,
    },
    Library {
        directory: PathBuf,
        uuid: crate::library_log::Id,
        view: Box<dyn crate::library_log::TapeView + Send>,
        path: ApplyPath,
        reply: mpsc::Sender<Result<LibraryRebuildReport, ApplyError>>,
    },
}
/// Cloneable Send + Sync channel capability. It does not expose a writer handle.
#[derive(Clone)]
pub struct ProjectionClient {
    worker: Arc<Worker>,
    path: PathBuf,
}
struct Worker {
    requests: Option<mpsc::SyncSender<Request>>,
    join: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.requests.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
impl ProjectionClient {
    /// Acquire B4a's registered writer and move its capability into a worker.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StateError> {
        let path = path.as_ref().to_path_buf();
        let mut writer = super::open(&path)?.write_handle();
        let (requests, queue) = mpsc::sync_channel::<Request>(64);
        let join = std::thread::Builder::new()
            .name("library-projector".into())
            .spawn(move || {
                while let Ok(request) = queue.recv() {
                    match request {
                        Request::Apply {
                            source,
                            path,
                            reply,
                        } => {
                            let result = match writer.apply(&source, path) {
                                Ok(marks) if marks.pending == 0 => ProjectionResult::Applied(marks),
                                Ok(marks) => ProjectionResult::Pending(marks),
                                Err(error) => ProjectionResult::Failed {
                                    watermarks: writer
                                        .with_core(|core| core.committed_watermarks(&source))
                                        .ok(),
                                    error,
                                },
                            };
                            let _ = reply.send(result);
                        }
                        Request::Snapshot {
                            snapshot,
                            view,
                            path,
                            reply,
                        } => {
                            let _ = reply.send(project_library_snapshot(
                                &mut writer,
                                &snapshot,
                                view.as_ref(),
                                path,
                            ));
                        }
                        Request::Library {
                            directory,
                            uuid,
                            view,
                            path,
                            reply,
                        } => {
                            let _ = reply.send(rebuild_library(
                                &mut writer,
                                &directory,
                                uuid,
                                view.as_ref(),
                                path,
                            ));
                        }
                    }
                }
            })
            .map_err(|e| StateError::io_at("start library projector", &path, e))?;
        Ok(Self {
            worker: Arc::new(Worker {
                requests: Some(requests),
                join: Some(join),
            }),
            path,
        })
    }
    /// Submit after releasing authority/tape locks. No catalog lock is held while waiting.
    pub fn project(
        &self,
        source: AuthorityRecord,
        path: ApplyPath,
    ) -> Result<ProjectionResult, StateError> {
        let (reply, answer) = mpsc::channel();
        self.worker
            .requests
            .as_ref()
            .expect("live worker")
            .send(Request::Apply {
                source,
                path,
                reply,
            })
            .map_err(|_| StateError::IndexCorrupt("projector worker stopped".into()))?;
        answer.recv().map_err(|_| {
            StateError::IndexCorrupt("projector worker stopped before response".into())
        })
    }
    /// Retry a durable library prefix against a frozen native view. This also
    /// publishes fold-selected pending scopes and per-tape contradiction status,
    /// including when no record applied. B4b-2 supplies its real frozen tape view.
    pub fn catch_up_library(
        &self,
        directory: PathBuf,
        uuid: crate::library_log::Id,
        view: Box<dyn crate::library_log::TapeView + Send>,
        path: ApplyPath,
    ) -> Result<LibraryRebuildReport, ApplyError> {
        let (reply, answer) = mpsc::channel();
        self.worker
            .requests
            .as_ref()
            .expect("live worker")
            .send(Request::Library {
                directory,
                uuid,
                view,
                path,
                reply,
            })
            .map_err(|_| StateError::IndexCorrupt("projector worker stopped".into()))?;
        answer.recv().map_err(|_| {
            StateError::IndexCorrupt("projector worker stopped before response".into())
        })?
    }
    /// Apply a live frozen prefix in the fold's order. Capture it with
    /// LibraryAuthority::snapshot before entering this channel wait.
    pub fn project_library(
        &self,
        snapshot: crate::library_log::LibrarySnapshot,
        view: Box<dyn crate::library_log::TapeView + Send>,
        path: ApplyPath,
    ) -> Result<LibraryRebuildReport, ApplyError> {
        let (reply, answer) = mpsc::channel();
        self.worker
            .requests
            .as_ref()
            .expect("live worker")
            .send(Request::Snapshot {
                snapshot,
                view,
                path,
                reply,
            })
            .map_err(|_| StateError::IndexCorrupt("projector worker stopped".into()))?;
        answer.recv().map_err(|_| {
            StateError::IndexCorrupt("projector worker stopped before response".into())
        })?
    }
    /// Hot-path reads use an independent read-only connection, never the worker mutex.
    pub fn reader(&self) -> Result<CatalogReader, StateError> {
        CatalogReader::open(&self.path)
    }
}
impl CatalogIndex {
    /// Single apply entry shared by live, startup and empty rebuild callers.
    pub fn apply(
        &mut self,
        source: &AuthorityRecord,
        path: ApplyPath,
    ) -> Result<Watermarks, ApplyError> {
        self.with_core(|core| Ok(core.apply_authority(source, path)))?
    }
}

/// Unresolved native key and the resource scopes it still holds.
#[derive(Clone, Debug)]
pub struct PendingProjection {
    /// Library sequence.
    pub sequence: u64,
    /// Missing native evidence, if any.
    pub key: Option<crate::library_log::TapeAt>,
    /// Fold-selected scopes, never reconstructed from SQL.
    pub scopes: std::collections::BTreeSet<crate::library_log::fold::Scope>,
}
/// A completed streaming rebuild's source progress and unresolved scopes.
#[derive(Clone, Debug)]
pub struct LibraryRebuildReport {
    /// Last validated library frame.
    pub received: u64,
    /// Highest contiguous fold decision (including exclusions).
    pub decided: u64,
    /// Native dependencies still pending.
    pub pending: Vec<PendingProjection>,
    /// Later health bodies rejected by the lifetime first-wins key.
    pub health_conflicts: Vec<u64>,
}
/// Stream a library log through the frozen fold into the same apply used live.
/// The caller supplies immutable tape evidence and has already opened fresh SQL
/// for an empty rebuild, or existing SQL for idempotent startup catch-up.
pub fn rebuild_library(
    writer: &mut CatalogIndex,
    directory: &Path,
    log_uuid: crate::library_log::Id,
    view: &dyn crate::library_log::TapeView,
    path: ApplyPath,
) -> Result<LibraryRebuildReport, ApplyError> {
    project_library_replay(writer, path, |applied| {
        crate::library_log::store::replay_observed(directory, log_uuid, view, applied)
    })
}
/// Stream a frozen live library prefix through the same fold and apply as restart.
pub fn project_library_snapshot(
    writer: &mut CatalogIndex,
    snapshot: &crate::library_log::LibrarySnapshot,
    view: &dyn crate::library_log::TapeView,
    path: ApplyPath,
) -> Result<LibraryRebuildReport, ApplyError> {
    project_library_replay(writer, path, |applied| snapshot.replay(view, applied))
}
fn project_library_replay(
    writer: &mut CatalogIndex,
    path: ApplyPath,
    replay: impl FnOnce(
        &mut dyn FnMut(
            crate::library_log::AppliedLibraryRecord,
        ) -> Result<(), crate::library_log::store::StoreError>,
    ) -> Result<
        crate::library_log::store::Replay,
        crate::library_log::store::StoreError,
    >,
) -> Result<LibraryRebuildReport, ApplyError> {
    let mut failure = None;
    let mut health_conflicts = Vec::new();
    let replay = replay(&mut |record| {
        let result = writer.apply(&AuthorityRecord::Library(Box::new(record)), path);
        if let Err(ApplyError::HealthConflict { sequence }) = result {
            health_conflicts.push(sequence);
            return Ok(());
        }
        result.map(|_| ()).map_err(|e| {
            failure = Some(e);
            crate::library_log::store::StoreError::Io(std::io::Error::other(
                "catalog projection failed",
            ))
        })
    })
    .map_err(|e| {
        failure
            .take()
            .unwrap_or_else(|| ApplyError::Catalog(StateError::IndexCorrupt(e.to_string())))
    })?;
    if let Some(refusal) = replay.refusal {
        return Err(ApplyError::Contradiction(format!(
            "library replay refused: {refusal:?}"
        )));
    }
    writer.with_core(|core| core.finish_library_replay(&replay.fold))?;
    Ok(LibraryRebuildReport {
        received: replay.last_sequence,
        decided: replay.fold.applied_through,
        pending: replay
            .fold
            .pending
            .iter()
            .map(|p| PendingProjection {
                sequence: p.sequence,
                key: p.key.clone(),
                scopes: p.scope.clone(),
            })
            .collect(),
        health_conflicts,
    })
}

#[cfg(any(test, feature = "fixtures"))]
impl CatalogIndex {
    /// Test hook: live/startup/rebuild entry counts for this shared writer.
    pub fn fixture_apply_counts(&self) -> Result<[u64; 3], StateError> {
        self.with_core(|core| Ok(core.apply_counts))
    }
}

#[cfg(test)]
impl CatalogIndex {
    pub(crate) fn normalized_authority_dump(
        &self,
    ) -> std::collections::BTreeMap<String, Vec<String>> {
        self.with_core(|core| core.normalized_authority_dump())
            .unwrap()
    }
    pub(crate) fn finish_observed_fold(&mut self, fold: &crate::library_log::LibraryFold) {
        self.with_core(|core| core.finish_library_replay(fold))
            .unwrap();
    }
}
