//! Independent bounded structural cursors over a validated inode. A second
//! structural decoder finds each group's closer before any row is exposed.
use super::{
    codec::Decoder,
    snapshot::{PrefixReader, Snapshot},
    *,
};
use remanence_parity::authority::{CommittedFileRows, FileRowCursor};
use remanence_parity::{BoundedJournalReplayMetrics, JournalError};

pub(super) fn journal(error: impl std::fmt::Display) -> JournalError {
    JournalError::Codec(error.to_string())
}
impl Snapshot {
    pub(super) fn rows_cursor(&self) -> RowsCursor {
        RowsCursor {
            snapshot: self.clone(),
            decoder: self.decoder(),
            boundary: self.decoder(),
            plan: false,
            group_end: 0,
        }
    }
    pub(super) fn row_metrics(&self) -> BoundedJournalReplayMetrics {
        BoundedJournalReplayMetrics {
            validation_passes: self.replay_metrics().validation_passes,
            row_replay_passes: 2,
            journal_record_count: self.replay_metrics().frames,
            peak_record_payload_bytes: self.replay_metrics().peak_frame_buffer as u64,
            peak_live_entry_count: 2,
        }
    }
}
pub(super) struct RowsCursor {
    snapshot: Snapshot,
    decoder: Option<Decoder<PrefixReader>>,
    boundary: Option<Decoder<PrefixReader>>,
    plan: bool,
    group_end: u64,
}
impl std::fmt::Debug for RowsCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RowsCursor")
            .field("group_end", &self.group_end)
            .finish_non_exhaustive()
    }
}
impl FileRowCursor for RowsCursor {
    fn committed_entry_count(&self) -> u64 {
        self.snapshot.summary.next_file
    }
    fn highest_protected_ordinal(&self) -> u64 {
        self.snapshot.summary.protected
    }
    fn total_committed_ordinals(&self) -> u64 {
        self.snapshot.summary.total
    }
    fn metrics(&self) -> BoundedJournalReplayMetrics {
        self.snapshot.row_metrics()
    }
    fn next_entry(&mut self) -> Result<Option<TapeFileEntry>, JournalError> {
        let Some(decoder) = &mut self.decoder else {
            return Ok(None);
        };
        while let Some(frame) = decoder.next_row_frame().map_err(journal)? {
            if frame.offset >= self.group_end {
                while let Some(close) = self
                    .boundary
                    .as_mut()
                    .expect("same snapshot")
                    .next_row_frame()
                    .map_err(journal)?
                {
                    if !matches!(
                        close.record,
                        Record::Initialized(_) | Record::PayloadContinuation(_)
                    ) {
                        self.plan = matches!(
                            close.record,
                            Record::FinalizationPrepared(_) | Record::FinalizationDecided(_)
                        );
                        self.group_end = close.end;
                        break;
                    }
                }
            }
            if self.plan {
                continue;
            }
            match frame.record {
                Record::Initialized(init) => return Ok(Some(init.bot)),
                Record::PayloadContinuation(Continuation {
                    chunk: Chunk::FileRow(row),
                    ..
                }) => return Ok(Some(row)),
                _ => {}
            }
        }
        Ok(None)
    }
}
/// Only constructed for parity-enabled authority.
#[derive(Debug, Clone)]
pub(super) struct Rows(pub Snapshot);
impl CommittedFileRows for Rows {
    fn metrics(&self) -> BoundedJournalReplayMetrics {
        self.0.row_metrics()
    }
    fn committed_entry_count(&self) -> u64 {
        self.0.summary.next_file
    }
    fn highest_protected_ordinal(&self) -> u64 {
        self.0.summary.protected
    }
    fn total_committed_ordinals(&self) -> u64 {
        self.0.summary.total
    }
    fn tape_uuid(&self) -> [u8; 16] {
        self.0.header.tape_uuid
    }
    fn block_size(&self) -> u32 {
        self.0.header.block_size
    }
    fn scheme(&self) -> &ParityScheme {
        self.0.header.scheme.as_ref().expect("parity rows")
    }
    fn try_clone_rows(&self) -> Result<Box<dyn CommittedFileRows>, JournalError> {
        Ok(Box::new(self.clone()))
    }
    fn open_cursor(&self) -> Result<Box<dyn FileRowCursor>, JournalError> {
        Ok(Box::new(self.0.rows_cursor()))
    }
}
