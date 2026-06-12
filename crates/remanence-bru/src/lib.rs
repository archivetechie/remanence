//! Read-only BRU/BRU-PE legacy archive reader for Remanence.
//!
//! BRU uses 2048-byte logical blocks with per-block checksums. On physical
//! tape those logical blocks may be grouped into larger tape records, so this
//! crate keeps logical BRU parsing separate from the input source. The first
//! implementation supports dump/byte streams and the [`ForeignTapeFormat`]
//! entry point over [`PhysicalTapeSource`].

use std::io::{self, Read, Seek, SeekFrom};

use remanence_format::{
    ArchiveEventSink, ArchiveGapCause, ArchiveGapRange, ArchiveReader, DamageRange, DamageStatus,
    EntryCatalogSink, EntryKind, FileDataSink, FileId, FileStreamReport, ForeignTapeFormat,
    FormatCapabilities, FormatDescriptor, FormatError, NormalizedEntry, ProbeConfidence,
    ProbeResult, ScanReport, SourceRequirement, StreamReport,
};
use remanence_library::{BlockSize, PhysicalReadOutcome, PhysicalTapeSource};

/// BRU logical block size in bytes.
pub const BRU_BLOCK_SIZE: usize = 2048;

const CHKSUM_OFFSET: usize = 0x080;
const CHKSUM_SIZE: usize = 8;
const CHKSUM_PLACEHOLDER: &[u8; CHKSUM_SIZE] = b"       0";

const MAGIC_OFFSET: usize = 0x0B0;
const MAGIC_SIZE: usize = 4;
const MAGIC_ARCHIVE_HEADER: u64 = 0x1234;
const MAGIC_FILE_HEADER: u64 = 0x2345;
const MAGIC_CONTINUATION: u64 = 0x3456;

const ARTIME_OFFSET: usize = 0x098;
const BUFSIZE_OFFSET: usize = 0x0A0;
const RELEASE_MINOR_OFFSET: usize = 0x0B8;
const RELEASE_MAJOR_OFFSET: usize = 0x0BC;
const VARIANT_OFFSET: usize = 0x0C0;
const ARCHIVE_ID_LOW_OFFSET: usize = 0x0D8;
const LABEL_OFFSET: usize = 0x1D0;
const LABEL_SIZE: usize = 80;

const FILE_PATH_OFFSET: usize = 0x000;
const FILE_PATH_SIZE: usize = 128;
const INLINE_DATA_LEN_OFFSET: usize = 0x0DC;
const INLINE_DATA_OFFSET: usize = 0x400;
const ST_MODE_OFFSET: usize = 0x180;
const ST_SIZE_OFFSET: usize = 0x1B8;
const SIZE_HIGH_OFFSET: usize = 0x278;

const CONTINUATION_DLEN_OFFSET: usize = 0x0DC;
const CONTINUATION_DATA_OFFSET: usize = 0x100;

const S_IFMT: u64 = 0xF000;
const S_IFDIR: u64 = 0x4000;
const S_IFREG: u64 = 0x8000;
const S_IFLNK: u64 = 0xA000;

const PHYSICAL_READ_BUFFER_BYTES: usize = 0x00FF_FFFF;

/// Parsed BRU archive header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BruArchiveHeader {
    /// Composite BRU archive id.
    pub archive_id: String,
    /// Archive label.
    pub label: String,
    /// BRU I/O buffer size in bytes.
    pub buffer_size_bytes: u64,
    /// Archive creation time as a Unix timestamp.
    pub artime: u64,
    /// BRU release major component.
    pub release_major: u64,
    /// BRU release minor component.
    pub release_minor: u64,
    /// BRU archive variant.
    pub variant: u64,
}

/// Parsed BRU file header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BruFileHeader {
    /// UTF-8 presentation path.
    pub path: String,
    /// Raw path bytes before UTF-8 conversion.
    pub raw_path: Vec<u8>,
    /// File size in bytes.
    pub size_bytes: u64,
    /// Entry kind.
    pub kind: EntryKind,
    /// Raw POSIX mode bits.
    pub mode: u64,
}

/// BRU format driver.
#[derive(Debug, Default, Clone, Copy)]
pub struct BruFormat;

impl BruFormat {
    /// Create a BRU reader from a byte stream, such as a `.bru` dump file.
    pub fn open_dump_reader<R: Read>(&self, reader: R) -> BruDumpReader<R> {
        BruDumpReader {
            inner: BruArchiveReader::new(ByteBruBlockSource::new(reader)),
        }
    }

    /// Probe a seekable byte stream, such as a `.bru` dump file.
    ///
    /// The stream position is restored before this returns, even when the
    /// source does not contain enough bytes for a full BRU block.
    pub fn probe_dump<R: Read + Seek>(&self, reader: &mut R) -> Result<ProbeResult, FormatError> {
        let start = reader
            .stream_position()
            .map_err(|err| io_to_format("reading BRU dump position", err))?;
        let mut block = [0; BRU_BLOCK_SIZE];
        let mut filled = 0usize;
        let mut read_error = None;
        while filled < BRU_BLOCK_SIZE {
            match reader.read(&mut block[filled..]) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(err) => {
                    read_error = Some(err);
                    break;
                }
            }
        }
        reader
            .seek(SeekFrom::Start(start))
            .map_err(|err| io_to_format("restoring BRU dump position", err))?;
        if let Some(err) = read_error {
            return Err(io_to_format("probing BRU dump block", err));
        }

        let confidence = if filled == BRU_BLOCK_SIZE {
            probe_block_confidence(&block)
        } else {
            ProbeConfidence::NoMatch
        };
        Ok(ProbeResult {
            format_id: self.id().to_string(),
            confidence,
            source_requirement: SourceRequirement::ByteStreamDump,
            adapter_state: Vec::new(),
        })
    }
}

impl FormatDescriptor for BruFormat {
    fn id(&self) -> &'static str {
        "remanence-bru"
    }

    fn version(&self) -> &'static str {
        "0.1"
    }

    fn source_requirement(&self) -> SourceRequirement {
        SourceRequirement::PhysicalTapeRecords
    }

    fn capabilities(&self) -> FormatCapabilities {
        FormatCapabilities {
            catalog_scan: true,
            sequential_restore: true,
            indexed_file_restore: false,
            range_read: false,
            write: false,
            verify: true,
            damage_events: true,
            metadata_preserving: false,
        }
    }
}

impl ForeignTapeFormat for BruFormat {
    fn probe(&self, source: &mut dyn PhysicalTapeSource) -> Result<ProbeResult, FormatError> {
        source.configure_block_size(BlockSize::Variable)?;
        let start = source.position()?;
        let mut buf = vec![0; PHYSICAL_READ_BUFFER_BYTES];
        let read_result = source.read_record(&mut buf);
        let restore_result = source.locate_physical(start);
        let outcome = read_result?;
        restore_result?;

        let confidence = match outcome {
            PhysicalReadOutcome::Data { bytes, .. } if bytes >= BRU_BLOCK_SIZE => {
                let block: &[u8; BRU_BLOCK_SIZE] = buf[..BRU_BLOCK_SIZE]
                    .try_into()
                    .map_err(|_| FormatError::Parse("BRU probe block size mismatch".to_string()))?;
                probe_block_confidence(block)
            }
            _ => ProbeConfidence::NoMatch,
        };
        Ok(ProbeResult {
            format_id: self.id().to_string(),
            confidence,
            source_requirement: SourceRequirement::PhysicalTapeRecords,
            adapter_state: Vec::new(),
        })
    }

    fn open_tape_reader<'a>(
        &self,
        source: &'a mut dyn PhysicalTapeSource,
        _probe: &ProbeResult,
    ) -> Result<Box<dyn ArchiveReader + 'a>, FormatError> {
        source.configure_block_size(BlockSize::Variable)?;
        Ok(Box::new(BruPhysicalReader {
            inner: BruArchiveReader::new(PhysicalBruBlockSource::new(source)),
        }))
    }
}

/// BRU reader over a byte stream.
#[derive(Debug)]
pub struct BruDumpReader<R: Read> {
    inner: BruArchiveReader<ByteBruBlockSource<R>>,
}

impl<R: Read> BruDumpReader<R> {
    /// Read and validate the archive header from the current stream position.
    ///
    /// This consumes the header block. Use a fresh reader for `scan` or
    /// `stream_all`, which expect to start at the archive header.
    pub fn read_archive_header(&mut self) -> Result<BruArchiveHeader, FormatError> {
        self.inner.read_archive_header()
    }
}

impl<R: Read> ArchiveReader for BruDumpReader<R> {
    fn scan(&mut self, sink: &mut dyn EntryCatalogSink) -> Result<ScanReport, FormatError> {
        self.inner.scan(sink)
    }

    fn stream_all(&mut self, sink: &mut dyn ArchiveEventSink) -> Result<StreamReport, FormatError> {
        self.inner.stream_all(sink)
    }

    fn stream_file(
        &mut self,
        _file_id: &FileId,
        _sink: &mut dyn FileDataSink,
    ) -> Result<FileStreamReport, FormatError> {
        Err(FormatError::unsupported(
            "BRU indexed file restore requires a scanned index",
        ))
    }
}

/// BRU reader over a physical tape stream.
pub struct BruPhysicalReader<'a> {
    inner: BruArchiveReader<PhysicalBruBlockSource<'a>>,
}

impl ArchiveReader for BruPhysicalReader<'_> {
    fn scan(&mut self, sink: &mut dyn EntryCatalogSink) -> Result<ScanReport, FormatError> {
        self.inner.scan(sink)
    }

    fn stream_all(&mut self, sink: &mut dyn ArchiveEventSink) -> Result<StreamReport, FormatError> {
        self.inner.stream_all(sink)
    }

    fn stream_file(
        &mut self,
        _file_id: &FileId,
        _sink: &mut dyn FileDataSink,
    ) -> Result<FileStreamReport, FormatError> {
        Err(FormatError::unsupported(
            "BRU indexed file restore requires a scanned index",
        ))
    }
}

#[derive(Debug)]
struct BruArchiveReader<S> {
    source: S,
}

impl<S: BruBlockSource> BruArchiveReader<S> {
    fn new(source: S) -> Self {
        Self { source }
    }

    fn read_archive_header(&mut self) -> Result<BruArchiveHeader, FormatError> {
        let block = self
            .source
            .next_block()?
            .ok_or_else(|| FormatError::Parse("missing BRU archive header".to_string()))?;
        if block.status != BruBlockStatus::Ok {
            return Err(FormatError::Parse(
                "BRU archive header checksum failed".to_string(),
            ));
        }
        parse_archive_header(&block.data)
    }

    fn scan(&mut self, sink: &mut dyn EntryCatalogSink) -> Result<ScanReport, FormatError> {
        let mut visitor = ScanVisitor {
            sink,
            report: ScanReport::default(),
        };
        walk_archive(&mut self.source, &mut visitor)?;
        Ok(visitor.report)
    }

    fn stream_all(&mut self, sink: &mut dyn ArchiveEventSink) -> Result<StreamReport, FormatError> {
        let mut visitor = StreamVisitor {
            sink,
            report: StreamReport::default(),
        };
        walk_archive(&mut self.source, &mut visitor)?;
        Ok(visitor.report)
    }
}

trait BruBlockSource {
    fn next_block(&mut self) -> Result<Option<BruBlock>, FormatError>;
}

#[derive(Debug)]
struct ByteBruBlockSource<R> {
    reader: R,
    offset: u64,
}

impl<R: Read> ByteBruBlockSource<R> {
    fn new(reader: R) -> Self {
        Self { reader, offset: 0 }
    }
}

impl<R: Read> BruBlockSource for ByteBruBlockSource<R> {
    fn next_block(&mut self) -> Result<Option<BruBlock>, FormatError> {
        let mut data = [0; BRU_BLOCK_SIZE];
        let mut filled = 0;
        while filled < BRU_BLOCK_SIZE {
            let read = self
                .reader
                .read(&mut data[filled..])
                .map_err(|err| io_to_format("reading BRU dump block", err))?;
            if read == 0 {
                if filled == 0 {
                    return Ok(None);
                }
                return Err(FormatError::TruncatedPayload);
            }
            filled += read;
        }
        let offset = self.offset;
        self.offset = self
            .offset
            .checked_add(BRU_BLOCK_SIZE as u64)
            .ok_or_else(|| FormatError::Parse("BRU source offset overflow".to_string()))?;
        Ok(Some(BruBlock::new(offset, data)))
    }
}

struct PhysicalBruBlockSource<'a> {
    source: &'a mut dyn PhysicalTapeSource,
    record: Vec<u8>,
    record_pos: usize,
    record_len: usize,
    logical_offset: u64,
}

impl<'a> PhysicalBruBlockSource<'a> {
    fn new(source: &'a mut dyn PhysicalTapeSource) -> Self {
        Self {
            source,
            record: vec![0; PHYSICAL_READ_BUFFER_BYTES],
            record_pos: 0,
            record_len: 0,
            logical_offset: 0,
        }
    }
}

impl BruBlockSource for PhysicalBruBlockSource<'_> {
    fn next_block(&mut self) -> Result<Option<BruBlock>, FormatError> {
        loop {
            if self.record_len.saturating_sub(self.record_pos) >= BRU_BLOCK_SIZE {
                let mut data = [0; BRU_BLOCK_SIZE];
                data.copy_from_slice(
                    &self.record[self.record_pos..self.record_pos + BRU_BLOCK_SIZE],
                );
                self.record_pos += BRU_BLOCK_SIZE;
                let offset = self.logical_offset;
                self.logical_offset = self
                    .logical_offset
                    .checked_add(BRU_BLOCK_SIZE as u64)
                    .ok_or_else(|| FormatError::Parse("BRU logical offset overflow".to_string()))?;
                return Ok(Some(BruBlock::new(offset, data)));
            }
            if self.record_pos != self.record_len {
                return Err(FormatError::Parse(format!(
                    "BRU physical record length {} is not a multiple of {}",
                    self.record_len, BRU_BLOCK_SIZE
                )));
            }

            match self.source.read_record(&mut self.record)? {
                PhysicalReadOutcome::Data { bytes: 0, .. } => {
                    return Err(FormatError::Parse(
                        "BRU physical data record is empty".to_string(),
                    ));
                }
                PhysicalReadOutcome::Data { bytes, .. } => {
                    self.record_pos = 0;
                    self.record_len = bytes;
                }
                PhysicalReadOutcome::Filemark { .. } | PhysicalReadOutcome::EndOfData { .. } => {
                    return Ok(None);
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BruBlockStatus {
    Ok,
    ChecksumFailed,
}

#[derive(Debug)]
struct BruBlock {
    offset: u64,
    data: [u8; BRU_BLOCK_SIZE],
    status: BruBlockStatus,
}

impl BruBlock {
    fn new(offset: u64, data: [u8; BRU_BLOCK_SIZE]) -> Self {
        let status = if verify_block(&data).is_ok() {
            BruBlockStatus::Ok
        } else {
            BruBlockStatus::ChecksumFailed
        };
        Self {
            offset,
            data,
            status,
        }
    }
}

trait BruVisitor {
    fn entry(&mut self, entry: &NormalizedEntry) -> Result<(), FormatError>;
    fn data(&mut self, file_offset: u64, bytes: &[u8]) -> Result<(), FormatError>;
    fn damage(&mut self, range: &DamageRange) -> Result<(), FormatError>;
    fn archive_gap(&mut self, range: &ArchiveGapRange) -> Result<(), FormatError>;
    fn end_entry(&mut self, entry: &NormalizedEntry) -> Result<(), FormatError>;
}

struct ScanVisitor<'a> {
    sink: &'a mut dyn EntryCatalogSink,
    report: ScanReport,
}

impl BruVisitor for ScanVisitor<'_> {
    fn entry(&mut self, entry: &NormalizedEntry) -> Result<(), FormatError> {
        self.report.entries += 1;
        self.sink.entry(entry)
    }

    fn data(&mut self, _file_offset: u64, _bytes: &[u8]) -> Result<(), FormatError> {
        Ok(())
    }

    fn damage(&mut self, range: &DamageRange) -> Result<(), FormatError> {
        self.report.damage_events += 1;
        self.sink.damage(range)
    }

    fn archive_gap(&mut self, range: &ArchiveGapRange) -> Result<(), FormatError> {
        self.report.archive_gaps += 1;
        self.sink.archive_gap(range)
    }

    fn end_entry(&mut self, _entry: &NormalizedEntry) -> Result<(), FormatError> {
        Ok(())
    }
}

struct StreamVisitor<'a> {
    sink: &'a mut dyn ArchiveEventSink,
    report: StreamReport,
}

impl BruVisitor for StreamVisitor<'_> {
    fn entry(&mut self, entry: &NormalizedEntry) -> Result<(), FormatError> {
        self.report.entries += 1;
        self.sink.begin_entry(entry)
    }

    fn data(&mut self, file_offset: u64, bytes: &[u8]) -> Result<(), FormatError> {
        self.report.bytes += bytes.len() as u64;
        self.sink.write_file_data(file_offset, bytes)
    }

    fn damage(&mut self, range: &DamageRange) -> Result<(), FormatError> {
        self.report.damage_events += 1;
        self.sink.report_damage(range)
    }

    fn archive_gap(&mut self, range: &ArchiveGapRange) -> Result<(), FormatError> {
        self.report.archive_gaps += 1;
        self.sink.report_archive_gap(range)
    }

    fn end_entry(&mut self, entry: &NormalizedEntry) -> Result<(), FormatError> {
        self.sink.end_entry(entry)
    }
}

fn walk_archive(
    source: &mut dyn BruBlockSource,
    visitor: &mut dyn BruVisitor,
) -> Result<(), FormatError> {
    let archive_block = source
        .next_block()?
        .ok_or_else(|| FormatError::Parse("missing BRU archive header".to_string()))?;
    if archive_block.status != BruBlockStatus::Ok {
        return Err(FormatError::Parse(
            "BRU archive header checksum failed".to_string(),
        ));
    }
    parse_archive_header(&archive_block.data)?;

    let mut sequence = 0u64;
    let mut pending_block: Option<BruBlock> = None;
    loop {
        let block = match pending_block.take() {
            Some(block) => Some(block),
            None => source.next_block()?,
        };
        let Some(block) = block else { break };
        let Some(block) = file_header_or_resync(source, visitor, block)? else {
            break;
        };
        sequence += 1;
        let header = parse_file_header(&block.data)?;
        let entry = normalized_entry(sequence, block.offset, &header);
        visitor.entry(&entry)?;
        pending_block = stream_entry_body(source, visitor, &entry, &header, &block)?;
        visitor.end_entry(&entry)?;
    }
    Ok(())
}

fn file_header_or_resync(
    source: &mut dyn BruBlockSource,
    visitor: &mut dyn BruVisitor,
    block: BruBlock,
) -> Result<Option<BruBlock>, FormatError> {
    if is_resync_file_header(&block) {
        Ok(Some(block))
    } else {
        visitor.archive_gap(&archive_gap_range(
            &block,
            ArchiveGapCause::UnrecognizedData,
        ))?;
        resync_to_file_header(source, visitor)
    }
}

fn resync_to_file_header(
    source: &mut dyn BruBlockSource,
    visitor: &mut dyn BruVisitor,
) -> Result<Option<BruBlock>, FormatError> {
    while let Some(block) = source.next_block()? {
        if is_resync_file_header(&block) {
            return Ok(Some(block));
        }
        visitor.archive_gap(&archive_gap_range(&block, ArchiveGapCause::Resync))?;
    }
    Ok(None)
}

fn is_resync_file_header(block: &BruBlock) -> bool {
    block.status == BruBlockStatus::Ok
        && matches!(read_magic(&block.data), Ok(MAGIC_FILE_HEADER))
        && parse_file_header(&block.data).is_ok()
}

fn stream_entry_body(
    source: &mut dyn BruBlockSource,
    visitor: &mut dyn BruVisitor,
    entry: &NormalizedEntry,
    header: &BruFileHeader,
    header_block: &BruBlock,
) -> Result<Option<BruBlock>, FormatError> {
    if header.kind != EntryKind::RegularFile || header.size_bytes == 0 {
        return Ok(None);
    }

    let mut position = 0u64;
    let inline_len = read_hex_u64(&header_block.data, INLINE_DATA_LEN_OFFSET, 4)?;
    if inline_len > 1024 {
        return Err(FormatError::Parse(format!(
            "invalid BRU inline_data_len={inline_len} at byte offset 0x{:x}",
            header_block.offset
        )));
    }
    if inline_len > header.size_bytes {
        return Err(FormatError::Parse(format!(
            "BRU inline_data_len={inline_len} exceeds file size {} for {}",
            header.size_bytes, header.path
        )));
    }
    if inline_len > 0 {
        let end = INLINE_DATA_OFFSET + inline_len as usize;
        if header_block.status != BruBlockStatus::Ok {
            visitor.damage(&damage_range(
                &entry.file_id,
                position,
                position + inline_len,
                DamageStatus::ChecksumFailed,
                Some(header_block.offset),
            ))?;
        }
        visitor.data(position, &header_block.data[INLINE_DATA_OFFSET..end])?;
        position += inline_len;
    }

    while position < header.size_bytes {
        let block = match source.next_block() {
            Ok(Some(block)) => block,
            Ok(None) | Err(FormatError::TruncatedPayload) => {
                visitor.damage(&damage_range(
                    &entry.file_id,
                    position,
                    header.size_bytes,
                    DamageStatus::Missing,
                    None,
                ))?;
                return Ok(None);
            }
            Err(err) => return Err(err),
        };
        let magic = match read_magic(&block.data) {
            Ok(magic) => magic,
            Err(_) => {
                return abort_entry_body_after_bad_continuation(
                    visitor, entry, header, position, block,
                );
            }
        };
        if magic != MAGIC_CONTINUATION {
            return abort_entry_body_after_bad_continuation(
                visitor, entry, header, position, block,
            );
        }
        let dlen = match read_hex_u64(&block.data, CONTINUATION_DLEN_OFFSET, 4) {
            Ok(dlen) => dlen,
            Err(_) => {
                return abort_entry_body_after_bad_continuation(
                    visitor, entry, header, position, block,
                );
            }
        };
        if dlen == 0 || dlen > 1792 {
            return abort_entry_body_after_bad_continuation(
                visitor, entry, header, position, block,
            );
        }
        let remaining = header.size_bytes - position;
        if dlen > remaining {
            return abort_entry_body_after_bad_continuation(
                visitor, entry, header, position, block,
            );
        }
        if block.status != BruBlockStatus::Ok {
            visitor.damage(&damage_range(
                &entry.file_id,
                position,
                position + dlen,
                DamageStatus::ChecksumFailed,
                Some(block.offset),
            ))?;
        }
        let end = CONTINUATION_DATA_OFFSET + dlen as usize;
        visitor.data(position, &block.data[CONTINUATION_DATA_OFFSET..end])?;
        position += dlen;
    }
    Ok(None)
}

fn abort_entry_body_after_bad_continuation(
    visitor: &mut dyn BruVisitor,
    entry: &NormalizedEntry,
    header: &BruFileHeader,
    position: u64,
    block: BruBlock,
) -> Result<Option<BruBlock>, FormatError> {
    if position < header.size_bytes {
        visitor.damage(&damage_range(
            &entry.file_id,
            position,
            header.size_bytes,
            DamageStatus::Missing,
            Some(block.offset),
        ))?;
    }
    if is_resync_file_header(&block) {
        return Ok(Some(block));
    }
    visitor.archive_gap(&archive_gap_range(
        &block,
        ArchiveGapCause::UnrecognizedData,
    ))?;
    Ok(None)
}

fn normalized_entry(sequence: u64, header_offset: u64, header: &BruFileHeader) -> NormalizedEntry {
    let mut adapter_state = Vec::with_capacity(16 + header.raw_path.len());
    adapter_state.extend_from_slice(&sequence.to_le_bytes());
    adapter_state.extend_from_slice(&header_offset.to_le_bytes());
    adapter_state.extend_from_slice(&header.raw_path);
    NormalizedEntry {
        file_id: FileId(format!("bru:{sequence}")),
        path: header.path.clone(),
        kind: header.kind,
        link_target: None,
        size_bytes: Some(header.size_bytes),
        adapter_state,
    }
}

fn damage_range(
    file_id: &FileId,
    start: u64,
    end: u64,
    status: DamageStatus,
    block_offset: Option<u64>,
) -> DamageRange {
    DamageRange {
        file_id: file_id.clone(),
        start,
        end,
        status,
        adapter_state: block_offset
            .map(|offset| offset.to_le_bytes().to_vec())
            .unwrap_or_default(),
    }
}

fn archive_gap_range(block: &BruBlock, cause: ArchiveGapCause) -> ArchiveGapRange {
    let mut adapter_state = Vec::with_capacity(16);
    adapter_state.extend_from_slice(&block.offset.to_le_bytes());
    if let Ok(magic) = read_magic(&block.data) {
        adapter_state.extend_from_slice(&magic.to_le_bytes());
    }
    ArchiveGapRange {
        source_start: block.offset,
        source_end: block.offset + BRU_BLOCK_SIZE as u64,
        cause,
        adapter_state,
    }
}

/// Compute the BRU checksum for one logical block.
pub fn bru_checksum(block: &[u8; BRU_BLOCK_SIZE]) -> u32 {
    let mut sums = [0u32; 4];
    for (index, byte) in block.iter().enumerate() {
        let value = if (CHKSUM_OFFSET..CHKSUM_OFFSET + CHKSUM_SIZE).contains(&index) {
            CHKSUM_PLACEHOLDER[index - CHKSUM_OFFSET]
        } else {
            *byte
        };
        sums[index % 4] = sums[index % 4].wrapping_add(value as u32);
    }
    ((sums[0] & 0xff) << 24) | ((sums[1] & 0xff) << 16) | ((sums[2] & 0xff) << 8) | (sums[3] & 0xff)
}

/// Extract the stored BRU checksum from one logical block.
pub fn stored_checksum(block: &[u8; BRU_BLOCK_SIZE]) -> Result<u32, FormatError> {
    Ok(read_hex_u64(block, CHKSUM_OFFSET, CHKSUM_SIZE)? as u32)
}

fn verify_block(block: &[u8; BRU_BLOCK_SIZE]) -> Result<(), FormatError> {
    let stored = stored_checksum(block)?;
    let computed = bru_checksum(block);
    if stored == computed {
        Ok(())
    } else {
        Err(FormatError::Parse(format!(
            "BRU checksum mismatch: stored 0x{stored:08x}, computed 0x{computed:08x}"
        )))
    }
}

fn probe_block_confidence(block: &[u8; BRU_BLOCK_SIZE]) -> ProbeConfidence {
    match read_magic(block) {
        Ok(MAGIC_ARCHIVE_HEADER) if verify_block(block).is_ok() => ProbeConfidence::Certain,
        Ok(MAGIC_ARCHIVE_HEADER) => ProbeConfidence::Probable,
        Ok(_) | Err(_) => ProbeConfidence::NoMatch,
    }
}

fn parse_archive_header(block: &[u8; BRU_BLOCK_SIZE]) -> Result<BruArchiveHeader, FormatError> {
    let magic = read_magic(block)?;
    if magic != MAGIC_ARCHIVE_HEADER {
        return Err(FormatError::Parse(format!(
            "not a BRU archive header: magic=0x{magic:04x}"
        )));
    }
    let artime = read_hex_u64(block, ARTIME_OFFSET, 8)?;
    let archive_id_low = read_hex_u64(block, ARCHIVE_ID_LOW_OFFSET, 4)?;
    Ok(BruArchiveHeader {
        archive_id: format!("{artime:08x}{archive_id_low:04x}"),
        label: read_ascii(block, LABEL_OFFSET, LABEL_SIZE).0,
        buffer_size_bytes: read_hex_u64(block, BUFSIZE_OFFSET, 8)?,
        artime,
        release_major: read_hex_u64(block, RELEASE_MAJOR_OFFSET, 4)?,
        release_minor: read_hex_u64(block, RELEASE_MINOR_OFFSET, 4)?,
        variant: read_hex_u64(block, VARIANT_OFFSET, 4)?,
    })
}

fn parse_file_header(block: &[u8; BRU_BLOCK_SIZE]) -> Result<BruFileHeader, FormatError> {
    let magic = read_magic(block)?;
    if magic != MAGIC_FILE_HEADER {
        return Err(FormatError::Parse(format!(
            "not a BRU file header: magic=0x{magic:04x}"
        )));
    }
    let (path, raw_path) = read_ascii(block, FILE_PATH_OFFSET, FILE_PATH_SIZE);
    let mode = read_hex_u64(block, ST_MODE_OFFSET, 8)?;
    let kind = match mode & S_IFMT {
        S_IFDIR => EntryKind::Directory,
        S_IFREG => EntryKind::RegularFile,
        S_IFLNK => EntryKind::Symlink,
        _ => EntryKind::Special,
    };
    let size_low = read_hex_u64(block, ST_SIZE_OFFSET, 8)?;
    let size_high = read_hex_u64(block, SIZE_HIGH_OFFSET, 8)?;
    Ok(BruFileHeader {
        path,
        raw_path,
        size_bytes: (size_high << 32) | size_low,
        kind,
        mode,
    })
}

fn read_magic(block: &[u8; BRU_BLOCK_SIZE]) -> Result<u64, FormatError> {
    read_hex_u64(block, MAGIC_OFFSET, MAGIC_SIZE)
}

fn read_hex_u64(
    block: &[u8; BRU_BLOCK_SIZE],
    offset: usize,
    size: usize,
) -> Result<u64, FormatError> {
    let raw = block
        .get(offset..offset + size)
        .ok_or_else(|| FormatError::Parse("BRU field offset out of range".to_string()))?;
    let text: String = raw
        .iter()
        .map(|byte| if *byte == 0 { b' ' } else { *byte })
        .map(char::from)
        .collect();
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(trimmed, 16).map_err(|err| {
        FormatError::Parse(format!(
            "invalid BRU hex field at 0x{offset:x} ({trimmed:?}): {err}"
        ))
    })
}

fn read_ascii(block: &[u8; BRU_BLOCK_SIZE], offset: usize, size: usize) -> (String, Vec<u8>) {
    let mut raw = block[offset..offset + size].to_vec();
    if let Some(nul) = raw.iter().position(|byte| *byte == 0) {
        raw.truncate(nul);
    }
    let text = String::from_utf8_lossy(&raw).into_owned();
    (text, raw)
}

fn io_to_format(context: impl Into<String>, source: io::Error) -> FormatError {
    FormatError::SourceIo {
        context: context.into(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use remanence_library::{PhysicalFilemarkSpace, PhysicalTapePosition, TapeIoError};

    #[derive(Default)]
    struct CatalogSink {
        entries: Vec<NormalizedEntry>,
        damages: Vec<DamageRange>,
        archive_gaps: Vec<ArchiveGapRange>,
    }

    impl EntryCatalogSink for CatalogSink {
        fn entry(&mut self, entry: &NormalizedEntry) -> Result<(), FormatError> {
            self.entries.push(entry.clone());
            Ok(())
        }

        fn damage(&mut self, range: &DamageRange) -> Result<(), FormatError> {
            self.damages.push(range.clone());
            Ok(())
        }

        fn archive_gap(&mut self, range: &ArchiveGapRange) -> Result<(), FormatError> {
            self.archive_gaps.push(range.clone());
            Ok(())
        }
    }

    #[derive(Default)]
    struct EventSink {
        entries: Vec<NormalizedEntry>,
        data: Vec<u8>,
        data_ranges: Vec<(u64, u64)>,
        damages: Vec<DamageRange>,
        archive_gaps: Vec<ArchiveGapRange>,
        ended: Vec<FileId>,
    }

    impl ArchiveEventSink for EventSink {
        fn begin_entry(&mut self, entry: &NormalizedEntry) -> Result<(), FormatError> {
            self.entries.push(entry.clone());
            Ok(())
        }

        fn write_file_data(&mut self, file_offset: u64, bytes: &[u8]) -> Result<(), FormatError> {
            self.data_ranges
                .push((file_offset, file_offset + bytes.len() as u64));
            self.data.extend_from_slice(bytes);
            Ok(())
        }

        fn report_damage(&mut self, range: &DamageRange) -> Result<(), FormatError> {
            self.damages.push(range.clone());
            Ok(())
        }

        fn report_archive_gap(&mut self, range: &ArchiveGapRange) -> Result<(), FormatError> {
            self.archive_gaps.push(range.clone());
            Ok(())
        }

        fn end_entry(&mut self, entry: &NormalizedEntry) -> Result<(), FormatError> {
            self.ended.push(entry.file_id.clone());
            Ok(())
        }
    }

    #[test]
    fn checksum_vectors_match_bru_spec() {
        assert_eq!(bru_checksum(&[0; BRU_BLOCK_SIZE]), 0x40404050);
        assert_eq!(bru_checksum(&[0xFF; BRU_BLOCK_SIZE]), 0x42424252);
    }

    #[test]
    fn reads_archive_header_from_dump_stream() {
        let archive = archive_block();
        let mut reader = BruFormat.open_dump_reader(std::io::Cursor::new(archive));

        let header = reader.read_archive_header().unwrap();

        assert_eq!(header.archive_id, "4de47d2661a8");
        assert_eq!(header.label, "TEST");
        assert_eq!(header.buffer_size_bytes, 1024 * 1024);
    }

    #[test]
    fn streams_file_payload_and_damage_events() {
        let mut continuation = continuation_block(b"cde");
        continuation[CONTINUATION_DATA_OFFSET] ^= 0x40;
        let dump = [
            archive_block().as_slice(),
            file_header_block("file.bin", 5, b"ab").as_slice(),
            continuation.as_slice(),
        ]
        .concat();
        let mut reader = BruFormat.open_dump_reader(std::io::Cursor::new(dump));
        let mut sink = EventSink::default();

        let report = reader.stream_all(&mut sink).unwrap();

        assert_eq!(report.entries, 1);
        assert_eq!(report.bytes, 5);
        assert_eq!(sink.data, b"ab#de");
        assert_eq!(sink.data_ranges, vec![(0, 2), (2, 5)]);
        assert_eq!(sink.damages.len(), 1);
        assert_eq!(sink.damages[0].start, 2);
        assert_eq!(sink.damages[0].end, 5);
        assert_eq!(sink.damages[0].status, DamageStatus::ChecksumFailed);
    }

    #[test]
    fn streams_missing_damage_when_payload_truncates() {
        let dump = [
            archive_block().as_slice(),
            file_header_block("file.bin", 5, b"ab").as_slice(),
        ]
        .concat();
        let mut reader = BruFormat.open_dump_reader(std::io::Cursor::new(dump));
        let mut sink = EventSink::default();

        let report = reader.stream_all(&mut sink).unwrap();

        assert_eq!(report.entries, 1);
        assert_eq!(report.bytes, 2);
        assert_eq!(report.damage_events, 1);
        assert_eq!(sink.data, b"ab");
        assert_eq!(sink.data_ranges, vec![(0, 2)]);
        assert_eq!(sink.damages.len(), 1);
        assert_eq!(sink.damages[0].start, 2);
        assert_eq!(sink.damages[0].end, 5);
        assert_eq!(sink.damages[0].status, DamageStatus::Missing);
        assert_eq!(sink.ended, vec![FileId("bru:1".to_string())]);
    }

    #[test]
    fn scan_reports_entries_without_materializing_payloads() {
        let dump = [
            archive_block().as_slice(),
            file_header_block("dir", 0, b"").as_slice(),
            file_header_block("file.bin", 3, b"abc").as_slice(),
        ]
        .concat();
        let mut reader = BruFormat.open_dump_reader(std::io::Cursor::new(dump));
        let mut sink = CatalogSink::default();

        let report = reader.scan(&mut sink).unwrap();

        assert_eq!(report.entries, 2);
        assert_eq!(sink.entries[0].path, "dir");
        assert_eq!(sink.entries[0].kind, EntryKind::Directory);
        assert_eq!(sink.entries[1].path, "file.bin");
        assert_eq!(sink.entries[1].size_bytes, Some(3));
    }

    #[test]
    fn probe_recognizes_archive_header_and_restores_position() {
        let mut source = FakePhysicalSource::new(vec![archive_block().to_vec()]);

        let probe = BruFormat.probe(&mut source).unwrap();

        assert_eq!(probe.confidence, ProbeConfidence::Certain);
        assert_eq!(
            probe.source_requirement,
            SourceRequirement::PhysicalTapeRecords
        );
        assert_eq!(source.configured, vec![BlockSize::Variable]);
        assert_eq!(source.position, PhysicalTapePosition::new(0));
        assert_eq!(source.next_record, 0);
        assert_eq!(source.locates, vec![PhysicalTapePosition::new(0)]);
    }

    #[test]
    fn probe_returns_no_match_for_non_hex_magic_and_restores_position() {
        let mut block = [0; BRU_BLOCK_SIZE];
        block[MAGIC_OFFSET..MAGIC_OFFSET + MAGIC_SIZE].copy_from_slice(b"nope");
        let mut source = FakePhysicalSource::new(vec![block.to_vec()]);

        let probe = BruFormat.probe(&mut source).unwrap();

        assert_eq!(probe.confidence, ProbeConfidence::NoMatch);
        assert_eq!(source.configured, vec![BlockSize::Variable]);
        assert_eq!(source.position, PhysicalTapePosition::new(0));
        assert_eq!(source.next_record, 0);
        assert_eq!(source.locates, vec![PhysicalTapePosition::new(0)]);
    }

    #[test]
    fn probe_dump_recognizes_archive_header_and_restores_position() {
        let mut dump = archive_block().to_vec();
        dump.extend_from_slice(&file_header_block("file.bin", 3, b"abc"));
        let mut reader = std::io::Cursor::new(dump);
        reader.set_position(0);

        let probe = BruFormat.probe_dump(&mut reader).unwrap();

        assert_eq!(probe.confidence, ProbeConfidence::Certain);
        assert_eq!(probe.source_requirement, SourceRequirement::ByteStreamDump);
        assert_eq!(reader.position(), 0);
    }

    #[test]
    fn probe_dump_returns_no_match_for_short_stream_and_restores_position() {
        let mut reader = std::io::Cursor::new(vec![0; BRU_BLOCK_SIZE - 1]);
        reader.set_position(17);

        let probe = BruFormat.probe_dump(&mut reader).unwrap();

        assert_eq!(probe.confidence, ProbeConfidence::NoMatch);
        assert_eq!(probe.source_requirement, SourceRequirement::ByteStreamDump);
        assert_eq!(reader.position(), 17);
    }

    #[test]
    fn stream_resyncs_after_unattributed_top_level_block() {
        let dump = [
            archive_block().as_slice(),
            file_header_block("first.bin", 3, b"one").as_slice(),
            unrecognized_block().as_slice(),
            file_header_block("second.bin", 3, b"two").as_slice(),
        ]
        .concat();
        let mut reader = BruFormat.open_dump_reader(std::io::Cursor::new(dump));
        let mut sink = EventSink::default();

        let report = reader.stream_all(&mut sink).unwrap();

        assert_eq!(report.entries, 2);
        assert_eq!(report.bytes, 6);
        assert_eq!(report.archive_gaps, 1);
        assert_eq!(sink.data, b"onetwo");
        assert_eq!(sink.entries[0].path, "first.bin");
        assert_eq!(sink.entries[1].path, "second.bin");
        assert_eq!(sink.archive_gaps.len(), 1);
        assert_eq!(sink.archive_gaps[0].source_start, 2 * BRU_BLOCK_SIZE as u64);
        assert_eq!(sink.archive_gaps[0].source_end, 3 * BRU_BLOCK_SIZE as u64);
        assert_eq!(
            sink.archive_gaps[0].cause,
            ArchiveGapCause::UnrecognizedData
        );
    }

    #[test]
    fn stream_resyncs_after_invalid_top_level_file_header() {
        let mut bad_header = file_header_block("bad.bin", 0, b"");
        bad_header[ST_MODE_OFFSET] = b'z';
        let bad_header = finalize_block(bad_header);
        let dump = [
            archive_block().as_slice(),
            bad_header.as_slice(),
            file_header_block("after-bad.bin", 4, b"data").as_slice(),
        ]
        .concat();
        let mut reader = BruFormat.open_dump_reader(std::io::Cursor::new(dump));
        let mut sink = EventSink::default();

        let report = reader.stream_all(&mut sink).unwrap();

        assert_eq!(report.entries, 1);
        assert_eq!(report.archive_gaps, 1);
        assert_eq!(sink.entries[0].path, "after-bad.bin");
        assert_eq!(sink.data, b"data");
        assert_eq!(sink.archive_gaps[0].source_start, BRU_BLOCK_SIZE as u64);
    }

    #[test]
    fn stream_marks_missing_and_resyncs_after_bad_continuation_magic() {
        let dump = [
            archive_block().as_slice(),
            file_header_block("first.bin", 5, b"ab").as_slice(),
            unrecognized_block().as_slice(),
            file_header_block("second.bin", 3, b"two").as_slice(),
        ]
        .concat();
        let mut reader = BruFormat.open_dump_reader(std::io::Cursor::new(dump));
        let mut sink = EventSink::default();

        let report = reader.stream_all(&mut sink).unwrap();

        assert_eq!(report.entries, 2);
        assert_eq!(report.bytes, 5);
        assert_eq!(report.damage_events, 1);
        assert_eq!(report.archive_gaps, 1);
        assert_eq!(sink.data, b"abtwo");
        assert_eq!(sink.damages[0].start, 2);
        assert_eq!(sink.damages[0].end, 5);
        assert_eq!(sink.damages[0].status, DamageStatus::Missing);
        assert_eq!(sink.entries[1].path, "second.bin");
    }

    #[test]
    fn stream_marks_missing_and_keeps_next_header_when_continuation_is_replaced_by_header() {
        let dump = [
            archive_block().as_slice(),
            file_header_block("first.bin", 5, b"ab").as_slice(),
            file_header_block("second.bin", 3, b"two").as_slice(),
        ]
        .concat();
        let mut reader = BruFormat.open_dump_reader(std::io::Cursor::new(dump));
        let mut sink = EventSink::default();

        let report = reader.stream_all(&mut sink).unwrap();

        assert_eq!(report.entries, 2);
        assert_eq!(report.bytes, 5);
        assert_eq!(report.damage_events, 1);
        assert_eq!(report.archive_gaps, 0);
        assert_eq!(sink.data, b"abtwo");
        assert_eq!(sink.damages[0].status, DamageStatus::Missing);
        assert_eq!(sink.entries[1].path, "second.bin");
    }

    #[test]
    fn scan_resyncs_after_unattributed_top_level_block() {
        let dump = [
            archive_block().as_slice(),
            unrecognized_block().as_slice(),
            file_header_block("after-gap.bin", 4, b"data").as_slice(),
        ]
        .concat();
        let mut reader = BruFormat.open_dump_reader(std::io::Cursor::new(dump));
        let mut sink = CatalogSink::default();

        let report = reader.scan(&mut sink).unwrap();

        assert_eq!(report.entries, 1);
        assert_eq!(report.archive_gaps, 1);
        assert_eq!(sink.entries[0].path, "after-gap.bin");
        assert_eq!(sink.archive_gaps.len(), 1);
        assert_eq!(sink.archive_gaps[0].source_start, BRU_BLOCK_SIZE as u64);
    }

    #[test]
    fn physical_reader_splits_large_records_into_bru_blocks() {
        let record = [
            archive_block().as_slice(),
            file_header_block("file.bin", 3, b"abc").as_slice(),
        ]
        .concat();
        let mut source = FakePhysicalSource::new(vec![record]);
        let mut sink = EventSink::default();
        let probe = ProbeResult {
            format_id: BruFormat.id().to_string(),
            confidence: ProbeConfidence::Certain,
            source_requirement: SourceRequirement::PhysicalTapeRecords,
            adapter_state: Vec::new(),
        };

        let report = {
            let mut reader = BruFormat.open_tape_reader(&mut source, &probe).unwrap();
            reader.stream_all(&mut sink).unwrap()
        };

        assert_eq!(source.configured, vec![BlockSize::Variable]);
        assert_eq!(report.entries, 1);
        assert_eq!(report.bytes, 3);
        assert_eq!(sink.data, b"abc");
    }

    struct FakePhysicalSource {
        records: Vec<Vec<u8>>,
        next_record: usize,
        position: PhysicalTapePosition,
        configured: Vec<BlockSize>,
        locates: Vec<PhysicalTapePosition>,
    }

    impl FakePhysicalSource {
        fn new(records: Vec<Vec<u8>>) -> Self {
            Self {
                records,
                next_record: 0,
                position: PhysicalTapePosition::new(0),
                configured: Vec::new(),
                locates: Vec::new(),
            }
        }
    }

    impl PhysicalTapeSource for FakePhysicalSource {
        fn configure_block_size(&mut self, block_size: BlockSize) -> Result<(), TapeIoError> {
            self.configured.push(block_size);
            Ok(())
        }

        fn locate_physical(
            &mut self,
            position: PhysicalTapePosition,
        ) -> Result<PhysicalTapePosition, TapeIoError> {
            let index = usize::try_from(position.lba).map_err(|_| {
                TapeIoError::OperationFailed(format!(
                    "test position {} does not fit usize",
                    position.lba
                ))
            })?;
            if index > self.records.len() {
                return Err(TapeIoError::OperationFailed(format!(
                    "test position {} is beyond {} records",
                    index,
                    self.records.len()
                )));
            }
            self.locates.push(position);
            self.next_record = index;
            self.position = position;
            Ok(self.position)
        }

        fn space_filemarks(&mut self, _count: i64) -> Result<PhysicalFilemarkSpace, TapeIoError> {
            Ok(PhysicalFilemarkSpace {
                filemarks_spaced: 0,
                position_after: self.position,
                hit_end_of_data: false,
            })
        }

        fn read_record(&mut self, buf: &mut [u8]) -> Result<PhysicalReadOutcome, TapeIoError> {
            let Some(record) = self.records.get(self.next_record) else {
                return Ok(PhysicalReadOutcome::EndOfData {
                    position_after: self.position,
                });
            };
            let actual = record.len();
            if actual > buf.len() {
                self.next_record += 1;
                self.position = PhysicalTapePosition::new(self.next_record as u64);
                return Err(TapeIoError::ReadBufferTooSmall {
                    actual: actual as u32,
                    provided: buf.len() as u32,
                });
            }
            buf[..actual].copy_from_slice(record);
            self.next_record += 1;
            self.position = PhysicalTapePosition::new(self.next_record as u64);
            Ok(PhysicalReadOutcome::Data {
                bytes: actual,
                position_after: self.position,
            })
        }

        fn position(&mut self) -> Result<PhysicalTapePosition, TapeIoError> {
            Ok(self.position)
        }
    }

    fn put_ascii(block: &mut [u8; BRU_BLOCK_SIZE], offset: usize, text: &str) {
        block[offset..offset + text.len()].copy_from_slice(text.as_bytes());
    }

    fn put_hex(block: &mut [u8; BRU_BLOCK_SIZE], offset: usize, size: usize, value: u64) {
        put_ascii(block, offset, &format!("{value:0size$x}"));
    }

    fn finalize_block(mut block: [u8; BRU_BLOCK_SIZE]) -> [u8; BRU_BLOCK_SIZE] {
        let checksum = bru_checksum(&block);
        put_ascii(&mut block, CHKSUM_OFFSET, &format!("{checksum:08x}"));
        block
    }

    fn archive_block() -> [u8; BRU_BLOCK_SIZE] {
        let mut block = [0; BRU_BLOCK_SIZE];
        put_hex(&mut block, MAGIC_OFFSET, MAGIC_SIZE, MAGIC_ARCHIVE_HEADER);
        put_hex(&mut block, ARTIME_OFFSET, 8, 0x4DE47D26);
        put_hex(&mut block, BUFSIZE_OFFSET, 8, 1024 * 1024);
        put_hex(&mut block, RELEASE_MINOR_OFFSET, 4, 17);
        put_hex(&mut block, RELEASE_MAJOR_OFFSET, 4, 1);
        put_hex(&mut block, VARIANT_OFFSET, 4, 0);
        put_hex(&mut block, ARCHIVE_ID_LOW_OFFSET, 4, 0x61A8);
        put_ascii(&mut block, LABEL_OFFSET, "TEST");
        finalize_block(block)
    }

    fn unrecognized_block() -> [u8; BRU_BLOCK_SIZE] {
        let mut block = [0; BRU_BLOCK_SIZE];
        put_hex(&mut block, MAGIC_OFFSET, MAGIC_SIZE, 0x9999);
        finalize_block(block)
    }

    fn file_header_block(path: &str, size: u64, inline: &[u8]) -> [u8; BRU_BLOCK_SIZE] {
        let mut block = [0; BRU_BLOCK_SIZE];
        put_ascii(&mut block, FILE_PATH_OFFSET, path);
        put_hex(&mut block, MAGIC_OFFSET, MAGIC_SIZE, MAGIC_FILE_HEADER);
        put_hex(&mut block, INLINE_DATA_LEN_OFFSET, 4, inline.len() as u64);
        let mode = if path == "dir" {
            S_IFDIR | 0o755
        } else {
            S_IFREG | 0o644
        };
        put_hex(&mut block, ST_MODE_OFFSET, 8, mode);
        put_hex(&mut block, ST_SIZE_OFFSET, 8, size);
        block[INLINE_DATA_OFFSET..INLINE_DATA_OFFSET + inline.len()].copy_from_slice(inline);
        finalize_block(block)
    }

    fn continuation_block(data: &[u8]) -> [u8; BRU_BLOCK_SIZE] {
        let mut block = [0; BRU_BLOCK_SIZE];
        put_hex(&mut block, MAGIC_OFFSET, MAGIC_SIZE, MAGIC_CONTINUATION);
        put_hex(&mut block, CONTINUATION_DLEN_OFFSET, 4, data.len() as u64);
        block[CONTINUATION_DATA_OFFSET..CONTINUATION_DATA_OFFSET + data.len()]
            .copy_from_slice(data);
        finalize_block(block)
    }
}
