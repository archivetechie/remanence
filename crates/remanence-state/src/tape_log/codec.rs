//! Streaming frame codec and byte-tail classification. A row scan deliberately
//! skips healthy non-final payloads; final/damaged frames share full classification.
//! It is not a payload trust-validation pass over the preceding stream.

use super::*;
use remanence_crc::crc64_xz;
use std::io::{self, Read, Seek, SeekFrom, Write};

/// End condition of a frame stream (group closure is checked by the fold).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tail {
    /// Every byte belongs to a complete frame.
    Complete,
    /// Non-authoritative append suffix.
    Torn {
        /// First absent byte/frame.
        offset: u64,
        /// Why the suffix is absent.
        kind: TornKind,
    },
}
/// Permitted absent suffixes; a short nonzero prefix is deliberately excluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TornKind {
    /// Integrity-checked prefix, short body or section trailer.
    ShortBody,
    /// Zero-filled suffix from a frame boundary or inside a damaged final body.
    ZeroFilled,
    /// Complete provisional records without a closing record.
    UnclosedGroup {
        /// Start of the zero-filled final frame, if it prevented group closure.
        zero_filled_at: Option<u64>,
    },
}
/// Read/encode failure distinguishes environmental I/O from corrupt authority.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    /// Bytes contradict the format or fold contract.
    #[error(transparent)]
    Recovery(#[from] TapeLogError),
    /// Candidate data does not satisfy the encode-side schema. No I/O failure.
    #[error("tape-log encode schema: {0}")]
    EncodeSchema(Fault),
    /// The transport failed; not evidence of corruption.
    #[error("tape-log I/O: {0}")]
    Io(#[from] io::Error),
}
impl CodecError {
    /// Whether the bytes require explicit recovery rather than absent-tail handling.
    pub fn recovery_required(&self) -> bool {
        matches!(self, Self::Recovery(_))
    }
}
fn error(offset: u64, sequence: Option<u64>, fault: Fault) -> CodecError {
    TapeLogError {
        offset,
        sequence,
        fault,
    }
    .into()
}

/// Validated frame; payload is borrowed from the decoder's one-frame buffer.
#[derive(Debug)]
pub struct Frame<'a> {
    /// Absolute byte offset.
    pub offset: u64,
    /// Exclusive byte end.
    pub end: u64,
    /// Contiguous frame sequence.
    pub sequence: u64,
    /// Decoded structural record.
    pub record: Record,
    /// Payload bytes (empty in a row scan, even when final-frame validation reads them).
    pub payload: &'a [u8],
    pub(super) prefix: [u8; 20],
    pub(super) structural: &'a [u8],
    pub(super) payload_validated: bool,
}

/// Encode one frame. This does not fsync, publish, or validate group semantics.
/// Structural encoding is limited while writing, before an oversized allocation.
pub fn encode_frame(
    w: &mut impl Write,
    sequence: u64,
    record: &Record,
    payload: &[u8],
) -> Result<(), CodecError> {
    if payload.len() > MAX_FRAME_BODY {
        return Err(CodecError::EncodeSchema(Fault::FrameTooLarge));
    }
    struct Limited {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl Write for Limited {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            if b.len() > self.limit - self.bytes.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "frame exceeds bound",
                ));
            }
            self.bytes.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut structural = Limited {
        bytes: Vec::new(),
        limit: MAX_FRAME_BODY - payload.len(),
    };
    wire::encode(record, &mut structural).map_err(|e| {
        if e.kind() == io::ErrorKind::InvalidInput {
            CodecError::EncodeSchema(Fault::FrameTooLarge)
        } else {
            CodecError::EncodeSchema(Fault::Invalid("CBOR encode schema"))
        }
    })?;
    let prefix = prefix_bytes(
        VERSION,
        record.tag(),
        structural.bytes.len() as u32,
        payload.len() as u32,
        sequence,
    );
    w.write_all(&prefix)?;
    w.write_all(&crc64_xz(&prefix).to_le_bytes())?;
    w.write_all(&structural.bytes)?;
    w.write_all(&crc64_xz(&structural.bytes).to_le_bytes())?;
    w.write_all(payload)?;
    w.write_all(&crc64_xz(payload).to_le_bytes())?;
    Ok(())
}
pub(super) fn prefix_bytes(version: u16, tag: Tag, s: u32, p: u32, sequence: u64) -> [u8; 20] {
    let mut bytes = [0; 20];
    bytes[..2].copy_from_slice(&version.to_le_bytes());
    bytes[2..4].copy_from_slice(&(tag as u16).to_le_bytes());
    bytes[4..8].copy_from_slice(&s.to_le_bytes());
    bytes[8..12].copy_from_slice(&p.to_le_bytes());
    bytes[12..20].copy_from_slice(&sequence.to_le_bytes());
    bytes
}

/// Bounded streaming decoder. It retains at most F+16 encoded section/trailer bytes, plus
/// one schema-directed structural record (at most O(F)); no history is retained.
/// A row decoder needs Read+Seek; a full decoder works with Read alone.
pub struct Decoder<R> {
    reader: R,
    buffer: Vec<u8>,
    offset: u64,
    expected: u64,
    tail: Tail,
    halted: bool,
}
impl<R: Read> Decoder<R> {
    /// Start a complete frame stream, sequence one at byte zero.
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            buffer: Vec::new(),
            offset: 0,
            expected: 1,
            tail: Tail::Complete,
            halted: false,
        }
    }
    /// Start at a known frame boundary in an immutable snapshot. This validates
    /// sequence continuity from the supplied boundary, not the preceding bytes.
    pub fn at(reader: R, sequence: u64, offset: u64) -> Self {
        Self {
            reader,
            buffer: Vec::new(),
            offset,
            expected: sequence,
            tail: Tail::Complete,
            halted: false,
        }
    }
    /// Tail classification after next_frame returns None.
    pub fn tail(&self) -> Tail {
        self.tail
    }
    /// Actual encoded-buffer capacity, for memory-bound tests.
    pub fn retained_buffer_bytes(&self) -> usize {
        self.buffer.capacity()
    }
    /// Decode and validate all three CRCs. No catalog DOM is constructed.
    pub fn next_frame(&mut self) -> Result<Option<Frame<'_>>, CodecError> {
        self.read_frame(|_, _| Ok(()), true)
    }
    fn read_frame(
        &mut self,
        skip_payload: impl FnOnce(&mut R, usize) -> io::Result<()>,
        full: bool,
    ) -> Result<Option<Frame<'_>>, CodecError> {
        if self.halted {
            return Ok(None);
        }
        // Fail closed after any error; no scan-past on a second call.
        self.halted = true;
        let offset = self.offset;
        let mut prefix = [0; PREFIX_LEN];
        let n = read_some(&mut self.reader, &mut prefix)?;
        if n == 0 {
            return Ok(None);
        }
        if prefix[..n].iter().all(|b| *b == 0) {
            let suffix = scan_suffix(&mut self.reader, offset + n as u64)?;
            if suffix.has_frame {
                return Err(error(offset, None, Fault::Crc(Section::Prefix)));
            }
            if let Some(at) = suffix.first_nonzero {
                return Err(error(offset, None, Fault::NonZeroAfterZeros { offset: at }));
            }
            self.tail = Tail::Torn {
                offset,
                kind: TornKind::ZeroFilled,
            };
            return Ok(None);
        }
        if n < PREFIX_LEN
            || crc64_xz(&prefix[..20]) != u64::from_le_bytes(prefix[20..].try_into().unwrap())
        {
            let fallback = if n < PREFIX_LEN {
                Fault::ShortPrefix
            } else {
                Fault::Crc(Section::Prefix)
            };
            if prefix[0] == 0 {
                let suffix = scan_suffix(&mut self.reader, offset + n as u64)?;
                if !suffix.has_frame {
                    let at = prefix[..n].iter().position(|b| *b != 0).unwrap();
                    return Err(error(
                        offset,
                        None,
                        Fault::NonZeroAfterZeros {
                            offset: offset + at as u64,
                        },
                    ));
                }
            }
            return Err(error(offset, None, fallback));
        }
        let seq = u64::from_le_bytes(prefix[12..20].try_into().unwrap());
        let fail = |fault| error(offset, Some(seq), fault);
        let version = u16::from_le_bytes(prefix[..2].try_into().unwrap());
        if version != VERSION {
            return Err(fail(Fault::Version(version)));
        }
        let tag =
            Tag::try_from(u16::from_le_bytes(prefix[2..4].try_into().unwrap())).map_err(fail)?;
        if (tag as u16) >= 16 {
            return Err(fail(Fault::ReservedTag(tag)));
        }
        if seq != self.expected {
            return Err(fail(Fault::Sequence {
                expected: self.expected,
                actual: seq,
            }));
        }
        let s = u32::from_le_bytes(prefix[4..8].try_into().unwrap()) as usize;
        let p = u32::from_le_bytes(prefix[8..12].try_into().unwrap()) as usize;
        if s as u64 + p as u64 > MAX_FRAME_BODY as u64 {
            return Err(fail(Fault::FrameTooLarge));
        }
        self.buffer.clear();
        let needed = s + p + 16;
        let initial = if full { needed } else { s + 8 };
        self.buffer.reserve_exact(initial);
        self.buffer.resize(initial, 0);
        let mut n = read_some(&mut self.reader, &mut self.buffer)?;
        let structural_bad = n >= s + 8
            && crc64_xz(&self.buffer[..s])
                != u64::from_le_bytes(self.buffer[s..s + 8].try_into().unwrap());
        // A damaged row falls back to the same bounded full-frame classifier.
        // Healthy non-final rows retain the payload-skipping fast path.
        let inspect_payload = full || structural_bad || n < s + 8;
        if inspect_payload && !full && n == s + 8 {
            self.buffer.reserve_exact(needed - n);
            self.buffer.resize(needed, 0);
            n += read_some(&mut self.reader, &mut self.buffer[n..])?;
        }
        self.buffer.truncate(n);
        let short = n < if inspect_payload { needed } else { s + 8 };
        let payload_bad = inspect_payload
            && !short
            && crc64_xz(&self.buffer[s + 8..s + 8 + p])
                != u64::from_le_bytes(self.buffer[s + 8 + p..].try_into().unwrap());
        let bad = if structural_bad {
            Some(Section::Structural)
        } else if payload_bad {
            Some(Section::Payload)
        } else {
            None
        };
        if short || bad.is_some() {
            self.tail = Tail::Torn {
                offset,
                kind: self.classify_final_body(s, p, seq, bad, short)?,
            };
            return Ok(None);
        }
        let record = wire::decode(tag, &self.buffer[..s]).map_err(fail)?;
        if !inspect_payload {
            skip_payload(&mut self.reader, p)?;
            let mut trailer = [0; 8];
            if read_some(&mut self.reader, &mut trailer)? != 8 {
                return Err(fail(Fault::Invalid("row snapshot changed during scan")));
            }
        }
        self.offset = offset
            .checked_add((FRAME_OVERHEAD + s + p) as u64)
            .ok_or_else(|| fail(Fault::Overflow))?;
        self.expected = seq.checked_add(1).ok_or_else(|| fail(Fault::Overflow))?;
        self.halted = false;
        Ok(Some(Frame {
            offset,
            end: self.offset,
            sequence: seq,
            record,
            payload: if full {
                &self.buffer[s + 8..s + 8 + p]
            } else {
                &[]
            },
            prefix: prefix[..20].try_into().unwrap(),
            structural: &self.buffer[..s],
            payload_validated: full,
        }))
    }
    // The sole damaged/short-body classifier, used by full and row decoding.
    fn classify_final_body(
        &mut self,
        s: usize,
        p: usize,
        seq: u64,
        bad: Option<Section>,
        short: bool,
    ) -> Result<TornKind, CodecError> {
        let body = &self.buffer;
        let offset = self.offset;
        let fail = |fault| error(offset, Some(seq), fault);
        let suffix = scan_suffix(
            &mut self.reader,
            offset + PREFIX_LEN as u64 + body.len() as u64,
        )?;
        if suffix.has_frame {
            return Err(fail(Fault::Crc(bad.unwrap_or(Section::Structural))));
        }
        if body.first() == Some(&0) {
            if let Some(at) = body.iter().position(|b| *b != 0) {
                return Err(fail(Fault::NonZeroAfterZeros {
                    offset: offset + PREFIX_LEN as u64 + at as u64,
                }));
            }
        }
        let zero_start = body.iter().rposition(|b| *b != 0).map_or(0, |i| i + 1);
        // Position, not the last data value: the zero suffix must cover the
        // whole structural CRC, or reach payload data. The naturally zero
        // empty-payload CRC by itself is insufficient. Valid CRCs bypass this
        // classifier, including any legitimately zero-valued structural CRC.
        let zero_extension = zero_start < body.len()
            && (zero_start <= s || (p > 0 && body.len() > s + 8 && zero_start < s + 8 + p));
        if zero_extension {
            if let Some(at) = suffix.first_nonzero {
                return Err(fail(Fault::NonZeroAfterZeros { offset: at }));
            }
            return Ok(TornKind::ZeroFilled);
        }
        if let Some(section) = bad {
            return Err(fail(Fault::Crc(section)));
        }
        if short {
            // Bytes in a CRC-proved structural section are not tail evidence.
            // In the remaining incomplete section, an interrupted zero run is
            // ambiguous; conservatively require recovery rather than discard it.
            let start = if body.len() >= s + 8 { s + 8 } else { 0 };
            if let Some(zero) = body[start..].iter().position(|b| *b == 0) {
                if let Some(nonzero) = body[start + zero..].iter().position(|b| *b != 0) {
                    return Err(fail(Fault::NonZeroAfterZeros {
                        offset: offset + PREFIX_LEN as u64 + (start + zero + nonzero) as u64,
                    }));
                }
            }
            return Ok(TornKind::ShortBody);
        }
        unreachable!("classifier requires a short body or failed CRC")
    }
}
impl<R: Read + Seek> Decoder<R> {
    /// Skip payloads of healthy non-final frames in an immutable snapshot.
    /// Final or structurally damaged frames use the full decoder's classifier,
    /// which reads their payloads. Returned rows never certify payload integrity
    /// of the preceding stream and cannot be passed to the fold.
    pub fn next_row_frame(&mut self) -> Result<Option<Frame<'_>>, CodecError> {
        if self.halted {
            return Ok(None);
        }
        let position = self.reader.stream_position()?;
        let mut prefix = [0; PREFIX_LEN];
        let n = read_some(&mut self.reader, &mut prefix)?;
        let mut full = true;
        if n == PREFIX_LEN && valid_prefix(&prefix) {
            let s = u32::from_le_bytes(prefix[4..8].try_into().unwrap()) as u64;
            let p = u32::from_le_bytes(prefix[8..12].try_into().unwrap()) as u64;
            let next = position
                .checked_add(FRAME_OVERHEAD as u64 + s + p)
                .ok_or_else(|| error(self.offset, None, Fault::Overflow))?;
            self.reader.seek(SeekFrom::Start(next))?;
            let n = read_some(&mut self.reader, &mut prefix)?;
            full = n != PREFIX_LEN || !valid_prefix(&prefix);
        }
        self.reader.seek(SeekFrom::Start(position))?;
        let frame = self.read_frame(
            |r, n| {
                r.seek(SeekFrom::Current(n as i64))?;
                Ok(())
            },
            full,
        )?;
        Ok(frame.map(|mut frame| {
            frame.payload = &[];
            frame.payload_validated = false;
            frame
        }))
    }
}

fn valid_prefix(bytes: &[u8]) -> bool {
    bytes.len() == PREFIX_LEN
        && bytes[..2] == VERSION.to_le_bytes()
        && Tag::try_from(u16::from_le_bytes(bytes[2..4].try_into().unwrap())).is_ok()
        && u64::from(u32::from_le_bytes(bytes[4..8].try_into().unwrap()))
            + u64::from(u32::from_le_bytes(bytes[8..12].try_into().unwrap()))
            <= MAX_FRAME_BODY as u64
        && crc64_xz(&bytes[..20]) == u64::from_le_bytes(bytes[20..].try_into().unwrap())
}
#[derive(Default)]
struct Suffix {
    first_nonzero: Option<u64>,
    has_frame: bool,
}
// Error paths only: bounded lookahead distinguishes a final zero region from
// corruption followed by another frame. A valid prefix is sufficient to refuse
// tail treatment; it never authorizes any bytes or resumes decoding past damage.
fn scan_suffix(reader: &mut impl Read, mut offset: u64) -> io::Result<Suffix> {
    let mut result = Suffix::default();
    let mut scratch = [0; 8192 + PREFIX_LEN - 1];
    let mut kept = 0;
    loop {
        let n = read_some(reader, &mut scratch[kept..])?;
        if result.first_nonzero.is_none() {
            result.first_nonzero = scratch[kept..kept + n]
                .iter()
                .position(|b| *b != 0)
                .map(|i| offset + i as u64);
        }
        result.has_frame |= scratch[..kept + n].windows(PREFIX_LEN).any(valid_prefix);
        if result.has_frame || n < scratch.len() - kept {
            return Ok(result);
        }
        offset = offset
            .checked_add(n as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "offset overflow"))?;
        let total = kept + n;
        kept = PREFIX_LEN - 1;
        scratch.copy_within(total - kept..total, 0);
    }
}

fn read_some(r: &mut impl Read, mut b: &mut [u8]) -> io::Result<usize> {
    let initial = b.len();
    while !b.is_empty() {
        match r.read(b) {
            Ok(0) => break,
            Ok(n) => b = &mut b[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(initial - b.len())
}

/// Pure frame-tail classifier over bytes. It validates every section, but use
/// fold::replay for semantic contradictions and well-formed unclosed groups.
pub fn classify_frames(bytes: &[u8]) -> Result<Tail, CodecError> {
    let mut decoder = Decoder::new(bytes);
    while decoder.next_frame()?.is_some() {}
    Ok(decoder.tail())
}
