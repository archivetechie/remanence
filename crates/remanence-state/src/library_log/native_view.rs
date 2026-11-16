//! Native evidence for library replay. Probe every tape log before replay starts;
//! retain immutable snapshots, not decoded histories. Missing/corrupt authority
//! contradicts references, while environmental read failures stop the operation.
//! The authority owner holds the state lease across probing, replay and refresh.
use super::{FoldError, Id, TapeAt, TapeLookup, TapeView};
use crate::tape_log::{
    codec::CodecError,
    snapshot::Snapshot,
    store::{Store, StoreError},
};
use std::{collections::BTreeMap, fs, io, path::Path};

/// A tape source the tolerant probe could not retain. Only offline rebuild refuses it.
#[derive(Clone)]
pub(crate) struct DroppedTapeLog {
    pub tape: Id,
    pub path: std::path::PathBuf,
    /// None denotes a marker with no corresponding tape log.
    pub error: Option<String>,
}

/// Effect of one authoritative snapshot replacement on the previously held prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuthoritativeChange {
    Unchanged,
    ChangedPrefix,
    Removed,
    ShorterPrefix,
    IdentityOnly,
}

/// A frozen set of probed native logs. A genuinely missing or corrupt log is
/// absent from the map and can never produce a pending (`NotYet`) reference.
#[derive(Clone)]
pub struct NativeTapeView {
    tapes: BTreeMap<Id, Snapshot>,
    mutations: u64,
    // Two ordered views of the latest change per tape, including removals.
    // Repeated refreshes replace the old entry rather than retain history.
    changed: BTreeMap<Id, u64>,
    changes: BTreeMap<u64, Id>,
    dropped: Vec<DroppedTapeLog>,
    replaced: BTreeMap<Id, std::collections::BTreeSet<(u64, u64)>>,
}
impl NativeTapeView {
    /// Number of tape entries inserted, replaced or removed since the probe.
    /// Probing establishes the initial view without counting a mutation.
    pub fn mutations(&self) -> u64 {
        self.mutations
    }

    /// Tapes changed after this count, once each, including removed tapes.
    /// Reads only memory and visits only the changes after the ordered seek;
    /// it never clones snapshots or scans unchanged tape entries.
    pub fn changed_since(&self, mutations: u64) -> Vec<Id> {
        use std::ops::Bound::{Excluded, Unbounded};
        self.changes
            .range((Excluded(mutations), Unbounded))
            .map(|(_, &tape)| tape)
            .collect()
    }

    /// Retain just the latest mutation for each tape in both lookup orders.
    fn changed_tape(&mut self, tape: Id) {
        self.mutations += 1;
        if let Some(previous) = self.changed.insert(tape, self.mutations) {
            self.changes.remove(&previous);
        }
        self.changes.insert(self.mutations, tape);
    }

    /// Replace evidence under the current exclusive native owner. Unlike an
    /// ordinary append refresh, this can represent a proved absent/repaired log.
    pub(crate) fn replace_authoritative(
        &mut self,
        snapshot: Snapshot,
    ) -> Result<AuthoritativeChange, StoreError> {
        let uuid = snapshot.header().tape_uuid;
        if let Some(prior) = self.tapes.get(&uuid) {
            if prior.file_identity()? != snapshot.file_identity()? {
                if let Some(identity) = prior.file_identity()? {
                    self.replaced.entry(uuid).or_default().insert(identity);
                }
            }
        }
        if let Some(identity) = snapshot.file_identity()? {
            if let Some(retired) = self.replaced.get_mut(&uuid) {
                retired.remove(&identity);
            }
        }
        // Keep replacement bookkeeping in its existing position: a failed
        // digest read leaves tapes unchanged, but may have retired an identity.
        let has_header = snapshot.header_digest()?.is_some();
        let change = match (self.tapes.get(&uuid), has_header) {
            (None, false) => AuthoritativeChange::Unchanged,
            (None, true) => AuthoritativeChange::ChangedPrefix,
            (Some(_), false) => AuthoritativeChange::Removed,
            (Some(prior), true)
                if snapshot.summary().closed_end < prior.summary().closed_end
                    || snapshot.summary().last_closed_sequence
                        < prior.summary().last_closed_sequence =>
            {
                AuthoritativeChange::ShorterPrefix
            }
            (Some(prior), true) => {
                let old_identity = prior.file_identity()?;
                if old_identity.is_some() && old_identity == snapshot.file_identity()? {
                    AuthoritativeChange::Unchanged
                } else if prior.projection_prefix_digest(prior.summary().closed_end)?
                    != snapshot.projection_prefix_digest(prior.summary().closed_end)?
                {
                    AuthoritativeChange::ChangedPrefix
                } else {
                    AuthoritativeChange::IdentityOnly
                }
            }
        };
        if has_header {
            self.tapes.insert(uuid, snapshot);
            self.changed_tape(uuid);
        } else if self.tapes.remove(&uuid).is_some() {
            self.changed_tape(uuid);
        }
        Ok(change)
    }
    /// Validate every native log before opening the library authority. The tape
    /// directory must already exist; failure to read it is never an empty view.
    /// `index_path` is passed to the native store for its existing volume policy;
    /// evidence reads neither consult nor write that derived projection.
    pub fn probe(directory: &Path, index_path: &Path) -> Result<Self, StoreError> {
        Self::probe_with(directory, |id| Store::open(directory, id, index_path))
    }

    fn probe_with(
        directory: &Path,
        open: impl Fn(Id) -> Result<Store, StoreError>,
    ) -> Result<Self, StoreError> {
        let mut tapes = BTreeMap::new();
        let mut dropped = Vec::new();
        let mut markers = BTreeMap::new();
        let mut log_paths = std::collections::BTreeSet::new();
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "ambiguous") {
                // Previously ignored marker names remain ignored unless they name a tape.
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    if let Ok(id) = uuid::Uuid::parse_str(stem) {
                        if stem == id.simple().to_string() {
                            markers.insert(*id.as_bytes(), path);
                        }
                    }
                }
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "remtape") {
                continue;
            }
            log_paths.insert(path.clone());
            let stem = path.file_stem().and_then(|s| s.to_str()).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid native tape filename")
            })?;
            let id = uuid::Uuid::parse_str(stem).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid native tape UUID filename",
                )
            })?;
            if stem != id.simple().to_string() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "noncanonical native tape UUID filename",
                )
                .into());
            }
            let id = *id.as_bytes();
            match open(id).and_then(|store| store.snapshot()) {
                Ok(snapshot) => {
                    tapes.insert(id, snapshot);
                }
                // These are format/identity failures proved by bytes, not I/O.
                Err(error @ (StoreError::Header | StoreError::Codec(CodecError::Recovery(_)))) => {
                    dropped.push(DroppedTapeLog {
                        tape: id,
                        path,
                        error: Some(error.to_string()),
                    });
                }
                Err(error) => return Err(error),
            }
        }
        for (tape, path) in markers {
            if !log_paths.contains(&path.with_extension("remtape")) {
                dropped.push(DroppedTapeLog {
                    tape,
                    path,
                    error: None,
                });
            }
        }
        dropped.sort_by_key(|entry| entry.tape);
        Ok(Self {
            tapes,
            dropped,
            replaced: BTreeMap::new(),
            mutations: 0,
            changed: BTreeMap::new(),
            changes: BTreeMap::new(),
        })
    }

    /// Probe omissions are evidence for the offline completeness gate, not a startup policy.
    pub(crate) fn dropped_logs(&self) -> &[DroppedTapeLog] {
        &self.dropped
    }

    /// Frozen native prefixes already probed at startup; no second path open.
    pub(crate) fn snapshots(&self) -> impl Iterator<Item = Snapshot> + '_ {
        self.tapes.values().cloned()
    }

    /// Frozen evidence for one tape; absence is distinct from a zero-length log.
    pub(crate) fn snapshot(&self, tape: Id) -> Option<&Snapshot> {
        self.tapes.get(&tape)
    }

    /// A replaced source cannot re-enter this process through a retained old
    /// snapshot, even when its former sequence is higher than the repaired log.
    pub(crate) fn accepts_snapshot(&self, snapshot: &Snapshot) -> Result<bool, StoreError> {
        Ok(snapshot.file_identity()?.is_none_or(|identity| {
            !self
                .replaced
                .get(&snapshot.header().tape_uuid)
                .is_some_and(|set| set.contains(&identity))
        }))
    }
    /// Replace one frozen prefix after a successful native completion, under
    /// the same tape admission ownership as the dependent library append. The
    /// caller must not invoke this after an ambiguous native completion.
    pub fn refresh(&mut self, snapshot: Snapshot) -> Result<(), StoreError> {
        if !self.accepts_snapshot(&snapshot)? {
            return Err(StoreError::Refused(
                "native snapshot belongs to a replaced repair source",
            ));
        }
        // A newly opened writer may have no published file yet. Its synthetic
        // spool header offset is not an authoritative empty prefix. A retained
        // pre-publication snapshot cannot erase a newer published source.
        if snapshot.file_identity()?.is_none() {
            return Ok(());
        }
        if self
            .tapes
            .get(&snapshot.header().tape_uuid)
            .is_some_and(|prior| {
                prior.summary().last_closed_sequence > snapshot.summary().last_closed_sequence
            })
        {
            return Ok(());
        }
        let tape = snapshot.header().tape_uuid;
        self.tapes.insert(tape, snapshot);
        self.changed_tape(tape);
        Ok(())
    }

    /// Discover a newly published tape without reopening any known tape owner.
    /// A new tape still exclusively held by its producer refuses safely before
    /// rebuild clears SQL; the producer will publish its snapshot on completion.
    pub(crate) fn discover_missing(
        &mut self,
        directory: &Path,
        index_path: &Path,
    ) -> Result<(), StoreError> {
        for path in crate::TapeAuthority::list_entries(directory)
            .map_err(|e| std::io::Error::other(e.to_string()))?
        {
            let id = crate::TapeAuthority::entry_tape_uuid(&path)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            if let std::collections::btree_map::Entry::Vacant(entry) = self.tapes.entry(id) {
                let snapshot = Store::open(directory, id, index_path)?.snapshot()?;
                entry.insert(snapshot);
                self.changed_tape(id);
            }
        }
        Ok(())
    }
}
impl TapeView for NativeTapeView {
    fn lookup(&self, at: &TapeAt) -> Result<TapeLookup, FoldError> {
        let Some(snapshot) = self.tapes.get(&at.tape.uuid) else {
            return Ok(TapeLookup::Contradiction);
        };
        // The immutable v2 header binds incarnation 1 even before Initialized.
        if at.tape.incarnation != 1 {
            return Ok(TapeLookup::Contradiction);
        }
        if at.sequence == 0 {
            return snapshot
                .header_digest()
                .map(|digest| {
                    digest.map_or(TapeLookup::Contradiction, |digest| {
                        TapeLookup::EmptyPrefix { digest }
                    })
                })
                .map_err(|error| FoldError::TapeUnavailable(error.to_string()));
        }
        match snapshot.record_evidence(at.sequence) {
            Ok(Some(evidence)) => Ok(TapeLookup::Present {
                kind: evidence.kind,
                digest: evidence.digest,
            }),
            Ok(None) => Ok(TapeLookup::Contradiction),
            Err(error) => Err(FoldError::TapeUnavailable(error.to_string())),
        }
    }
}

/// Native digest reads and path freshness share the runtime's frozen view.
/// Only forced release asks for freshness; detection reads the captured prefix.
pub(crate) struct NativePrefixDigests<'a> {
    pub view: &'a NativeTapeView,
    pub root: &'a Path,
}
impl crate::library_runtime::native_floor::PrefixDigestSource for NativePrefixDigests<'_> {
    fn prefix_digest(&self, tape: Id, end: u64) -> Result<super::Hash, StoreError> {
        self.view
            .snapshot(tape)
            .ok_or(StoreError::Refused("native log absent"))?
            .projection_prefix_digest(end)
    }
    fn source_unchanged(&self, tape: Id) -> Result<bool, StoreError> {
        use std::os::unix::fs::MetadataExt;
        let snapshot = self
            .view
            .snapshot(tape)
            .ok_or(StoreError::Refused("native log absent"))?;
        let metadata = fs::metadata(
            self.root
                .join(format!("{}.remtape", uuid::Uuid::from_bytes(tape).simple())),
        )?;
        Ok(
            snapshot.file_identity()? == Some((metadata.dev(), metadata.ino()))
                && metadata.len() == snapshot.captured_end(),
        )
    }
}

#[cfg(test)]
#[path = "native_view_tests.rs"]
mod tests;

#[cfg(test)]
mod replacement_report_tests {
    use super::*;
    use crate::authority::TapeAuthorityView;
    use crate::tape_log::CommitCursor;
    const ID: Id = [0xd2; 16];

    /// Independent native owner, allowing real file identities and byte digests.
    fn initialized(block_size: u32) -> (tempfile::TempDir, Snapshot) {
        let root = tempfile::tempdir().unwrap();
        let authority = crate::TapeAuthority::open_fixture(root.path(), ID).unwrap();
        let mut owner = authority.acquire_exclusive().unwrap();
        owner
            .publish_initialized(
                block_size,
                None,
                CommitCursor {
                    partition: 0,
                    lba: 2,
                },
            )
            .unwrap();
        (root, owner.freeze().unwrap().raw)
    }

    /// Same number of valid frames, with different session identity lengths.
    fn session_prefix(serial: &str) -> (tempfile::TempDir, Snapshot) {
        let root = tempfile::tempdir().unwrap();
        let authority = crate::TapeAuthority::open_fixture(root.path(), ID).unwrap();
        let mut owner = authority.acquire_exclusive().unwrap();
        owner
            .publish_initialized(
                262144,
                None,
                CommitCursor {
                    partition: 0,
                    lba: 2,
                },
            )
            .unwrap();
        owner
            .open_session(
                [7; 16],
                "2026-11-11T00:00:00Z".into(),
                crate::tape_log::SessionDrive {
                    serial: serial.into(),
                    vendor: None,
                    product: None,
                    endpoint: crate::library_log::DriveKey::Standalone(
                        crate::library_log::Standalone {
                            drive_serial: serial.into(),
                        },
                    ),
                    drive_uuid: None,
                },
            )
            .unwrap();
        (root, owner.freeze().unwrap().raw)
    }

    /// Pin every ordered report branch, especially shortening before identity
    /// or prefix hashing and a changed prefix with the same closed length.
    #[test]
    fn authoritative_replacement_reports_each_prefix_change() {
        use AuthoritativeChange::*;
        let root = tempfile::tempdir().unwrap();
        let authority = crate::TapeAuthority::open_fixture(root.path(), ID).unwrap();
        let mut owner = authority.acquire_exclusive().unwrap();
        let absent = owner.freeze().unwrap().raw;
        let mut view = NativeTapeView {
            tapes: BTreeMap::new(),
            dropped: Vec::new(),
            replaced: BTreeMap::new(),
            mutations: 0,
            changed: BTreeMap::new(),
            changes: BTreeMap::new(),
        };
        assert_eq!(
            view.replace_authoritative(absent.clone()).unwrap(),
            Unchanged
        );
        let empty = owner.publish_empty_prefix(262144, None).unwrap();
        assert_eq!(
            view.replace_authoritative(empty.clone()).unwrap(),
            ChangedPrefix
        );
        owner
            .publish_initialized(
                262144,
                None,
                CommitCursor {
                    partition: 0,
                    lba: 2,
                },
            )
            .unwrap();
        let grown = owner.freeze().unwrap().raw;
        assert!(grown.summary().closed_end > empty.summary().closed_end);
        assert_eq!(
            grown.file_identity().unwrap(),
            empty.file_identity().unwrap()
        );
        assert_eq!(
            view.replace_authoritative(grown.clone()).unwrap(),
            Unchanged,
            "same file grown"
        );
        assert_eq!(
            view.replace_authoritative(empty).unwrap(),
            ShorterPrefix,
            "shorter despite same identity"
        );
        assert_eq!(
            view.replace_authoritative(grown.clone()).unwrap(),
            Unchanged
        );
        let (_same_root, same) = initialized(262144);
        assert_ne!(
            same.file_identity().unwrap(),
            grown.file_identity().unwrap()
        );
        assert_eq!(view.replace_authoritative(same).unwrap(), IdentityOnly);
        let (_changed_root, changed) = initialized(524288);
        assert_eq!(changed.summary().closed_end, grown.summary().closed_end);
        assert_eq!(
            view.replace_authoritative(changed).unwrap(),
            ChangedPrefix,
            "changed-prefix report"
        );
        let (_long_root, long) = session_prefix("LONG-DRIVE-SERIAL");
        view.replace_authoritative(long.clone()).unwrap();
        let (_short_root, short) = session_prefix("DRV");
        assert_eq!(
            short.summary().last_closed_sequence,
            long.summary().last_closed_sequence
        );
        assert!(short.summary().closed_end < long.summary().closed_end);
        assert_eq!(
            view.replace_authoritative(short).unwrap(),
            ShorterPrefix,
            "shorter by bytes alone"
        );
        assert_eq!(view.replace_authoritative(absent.clone()).unwrap(), Removed);
        assert!(!view.tapes.contains_key(&ID));
        assert_eq!(view.replace_authoritative(absent).unwrap(), Unchanged);
    }
}
