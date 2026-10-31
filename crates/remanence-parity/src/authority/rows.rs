//! Owned structural cursors that hide the storage implementation from parity readers.
use crate::{JournalError, TapeFileEntry};

/// An independent bounded structural cursor.
pub trait FileRowCursor: std::fmt::Debug + Send {
    /// Frozen committed entry count.
    fn committed_entry_count(&self) -> u64;
    /// Frozen highest protected ordinal.
    fn highest_protected_ordinal(&self) -> u64;
    /// Frozen total committed ordinals.
    fn total_committed_ordinals(&self) -> u64;
    /// Next row in physical order, or end of the frozen prefix.
    fn next_entry(&mut self) -> Result<Option<TapeFileEntry>, JournalError>;
    /// Bounded replay measurements.
    fn metrics(&self) -> crate::BoundedJournalReplayMetrics;
}
/// Frozen structural source; implementations must not observe later appends.
pub trait CommittedFileRows: std::fmt::Debug + Send + Sync {
    /// Frozen metrics metadata.
    fn metrics(&self) -> crate::BoundedJournalReplayMetrics;
    /// Frozen committed entry count metadata.
    fn committed_entry_count(&self) -> u64;
    /// Frozen highest protected ordinal metadata.
    fn highest_protected_ordinal(&self) -> u64;
    /// Frozen total committed ordinals metadata.
    fn total_committed_ordinals(&self) -> u64;
    /// Frozen tape uuid metadata.
    fn tape_uuid(&self) -> [u8; 16];
    /// Frozen block size metadata.
    fn block_size(&self) -> u32;
    /// Frozen scheme metadata.
    fn scheme(&self) -> &crate::ParityScheme;
    /// Retain the same immutable boundary.
    fn try_clone_rows(&self) -> Result<Box<dyn CommittedFileRows>, JournalError>;
    /// Start an independent bounded cursor.
    fn open_cursor(&self) -> Result<Box<dyn FileRowCursor>, JournalError>;
}
/// Storage-independent owned frozen rows.
#[derive(Debug)]
pub struct FrozenFileRows(Box<dyn CommittedFileRows>);
impl FrozenFileRows {
    /// Bind a validated frozen source. No storage selection occurs at runtime.
    pub fn new(source: impl CommittedFileRows + 'static) -> Self {
        Self(Box::new(source))
    }
    /// Retain the identical frozen prefix.
    pub fn try_clone(&self) -> Result<Self, JournalError> {
        self.0.try_clone_rows().map(Self)
    }
    /// Start a fresh structural pass.
    pub fn replay(&self) -> Result<Box<dyn FileRowCursor>, JournalError> {
        self.0.open_cursor()
    }
    /// Frozen metrics metadata.
    pub fn metrics(&self) -> crate::BoundedJournalReplayMetrics {
        self.0.metrics()
    }
    /// Frozen committed entry count metadata.
    pub fn committed_entry_count(&self) -> u64 {
        self.0.committed_entry_count()
    }
    /// Frozen highest protected ordinal metadata.
    pub fn highest_protected_ordinal(&self) -> u64 {
        self.0.highest_protected_ordinal()
    }
    /// Frozen total committed ordinals metadata.
    pub fn total_committed_ordinals(&self) -> u64 {
        self.0.total_committed_ordinals()
    }
    /// Frozen tape uuid metadata.
    pub fn tape_uuid(&self) -> [u8; 16] {
        self.0.tape_uuid()
    }
    /// Frozen block size metadata.
    pub fn block_size(&self) -> u32 {
        self.0.block_size()
    }
    /// Frozen scheme metadata.
    pub fn scheme(&self) -> &crate::ParityScheme {
        self.0.scheme()
    }
}
