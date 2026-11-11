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

/// A frozen set of probed native logs. A genuinely missing or corrupt log is
/// absent from the map and can never produce a pending (`NotYet`) reference.
#[derive(Clone)]
pub struct NativeTapeView {
    tapes: BTreeMap<Id, Snapshot>,
    replaced: BTreeMap<Id, std::collections::BTreeSet<(u64, u64)>>,
}
impl NativeTapeView {
    /// Replace evidence under the current exclusive native owner. Unlike an
    /// ordinary append refresh, this can represent a proved absent/repaired log.
    pub(crate) fn replace_authoritative(&mut self, snapshot: Snapshot) -> Result<(), StoreError> {
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
        if snapshot.header_digest()?.is_some() {
            self.tapes.insert(uuid, snapshot);
        } else {
            self.tapes.remove(&uuid);
        }
        Ok(())
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
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "remtape") {
                continue;
            }
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
                Err(StoreError::Header | StoreError::Codec(CodecError::Recovery(_))) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(Self {
            tapes,
            replaced: BTreeMap::new(),
        })
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
        self.tapes.insert(snapshot.header().tape_uuid, snapshot);
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

#[cfg(test)]
#[path = "native_view_tests.rs"]
mod tests;
