//! Family-neutral bounded log framing. Prefix CRC is checked before lengths
//! influence allocation. Full and row scans share one damaged-tail classifier.
//! EOF-short bodies with internal zeros followed by data remain torn suffixes;
//! complete contradictory bodies are refused. Group closure belongs to callers.

pub mod publication;

use remanence_crc::crc64_xz;
use std::io::{self, Read, Seek, SeekFrom, Write};

/// Maximum combined structural and payload length.
pub const MAX_FRAME_BODY: usize = 64 * 1024 * 1024;
/// Twenty prefix bytes and their CRC-64/XZ.
pub const PREFIX_LEN: usize = 28;
/// Prefix and both section trailers.
pub const FRAME_OVERHEAD: usize = PREFIX_LEN + 16;

/// Independently checksummed sections.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    /// Fixed fields.
    Prefix,
    /// Family structural bytes.
    Structural,
    /// Family payload bytes.
    Payload,
}

/// Byte-level fault, converted by a family to its existing diagnostic type.
#[derive(Debug, thiserror::Error)]
pub enum Fault {
    /// CRC mismatch.
    #[error("bad {0:?} CRC")]
    Crc(Section),
    /// Bytes after a zero-filled region.
    #[error("nonzero data after zeros at {offset}")]
    NonZeroAfterZeros {
        /// Absolute offset.
        offset: u64,
    },
    /// The combined body exceeds its bound.
    #[error("frame exceeds 64 MiB")]
    FrameTooLarge,
    /// Noncontiguous sequence.
    #[error("expected sequence {expected}, got {actual}")]
    Sequence {
        /// Expected sequence.
        expected: u64,
        /// Observed sequence.
        actual: u64,
    },
    /// Counter overflow.
    #[error("counter overflow")]
    Overflow,
    /// Invalid snapshot contract.
    #[error("invalid record: {0}")]
    Invalid(&'static str),
}

/// Family policy and schema. Validation is supplied by the caller, so distinct
/// log versions/tags cannot be accepted merely because the framing is identical.
///
/// Every accepted version MUST have a nonzero low byte (`version & 0xff != 0`).
/// The decoder uses a zero first byte to distinguish zero-filled tails from
/// EOF-short identities, before a full version can be validated. Versions such
/// as 256 violate this contract even when their complete prefix CRC is valid.
/// Families must reject them in both identity validators.
pub trait Family {
    /// Decoded structural value.
    type Record;
    /// Family fault, including shared byte faults.
    type Fault: From<Fault>;
    /// Family diagnostic, preserving byte offset/sequence and environmental I/O.
    type Error: From<io::Error>;
    /// Validate complete version and tag fields.
    fn validate(version: u16, tag: u16) -> Result<(), Self::Fault>;
    /// Validate the visible identity bytes of an EOF-short prefix.
    fn validate_short_identity(
        prefix: &[u8; PREFIX_LEN],
        present: usize,
    ) -> Result<(), Self::Fault>;
    /// Recognize a prefix during bounded corruption lookahead, not authorization.
    fn valid_identity(version: u16, tag: u16) -> bool;
    /// Decode the structural section, consuming it exactly.
    fn decode(tag: u16, structural: &[u8]) -> Result<Self::Record, Self::Fault>;
    /// Attach the family's diagnostic context without changing its messages.
    fn error(offset: u64, sequence: Option<u64>, fault: Self::Fault) -> Self::Error;
}
fn error<F: Family>(offset: u64, sequence: Option<u64>, fault: Fault) -> F::Error {
    F::error(offset, sequence, fault.into())
}

/// Bounded construction buffer; failed writes never extend it past the cap.
pub struct Limited {
    /// Encoded bytes so far.
    pub bytes: Vec<u8>,
    /// Maximum permitted size.
    pub limit: usize,
}
impl Write for Limited {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "frame exceeds bound",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
/// The exact twenty little-endian prefix bytes, excluding their CRC.
pub fn prefix_bytes(version: u16, tag: u16, s: u32, p: u32, sequence: u64) -> [u8; 20] {
    let mut bytes = [0; 20];
    bytes[..2].copy_from_slice(&version.to_le_bytes());
    bytes[2..4].copy_from_slice(&tag.to_le_bytes());
    bytes[4..8].copy_from_slice(&s.to_le_bytes());
    bytes[8..12].copy_from_slice(&p.to_le_bytes());
    bytes[12..20].copy_from_slice(&sequence.to_le_bytes());
    bytes
}
/// Write the common frame after the family has validated its structural value.
/// This does not publish or sync. Oversize sections are refused before any write.
pub fn encode_frame(
    w: &mut impl Write,
    version: u16,
    tag: u16,
    sequence: u64,
    structural: &[u8],
    payload: &[u8],
) -> io::Result<()> {
    if structural
        .len()
        .checked_add(payload.len())
        .is_none_or(|len| len > MAX_FRAME_BODY)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame exceeds bound",
        ));
    }
    let prefix = prefix_bytes(
        version,
        tag,
        structural.len() as u32,
        payload.len() as u32,
        sequence,
    );
    w.write_all(&prefix)?;
    w.write_all(&crc64_xz(&prefix).to_le_bytes())?;
    w.write_all(structural)?;
    w.write_all(&crc64_xz(structural).to_le_bytes())?;
    w.write_all(payload)?;
    w.write_all(&crc64_xz(payload).to_le_bytes())?;
    Ok(())
}
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
/// Permitted EOF-short or zero-filled append suffixes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TornKind {
    /// EOF interrupts the fixed frame prefix before its CRC can be checked.
    ShortPrefix,
    /// Integrity-checked prefix, short body or section trailer.
    ShortBody,
    /// Zero-filled suffix from a frame boundary or inside a damaged final body.
    ZeroFilled,
}
/// Validated frame; payload is borrowed from the decoder's one-frame buffer.
#[derive(Debug)]
pub struct Frame<'a, Record> {
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
    pub(crate) prefix: [u8; 20],
    pub(crate) structural: &'a [u8],
    pub(crate) payload_validated: bool,
}

/// Bounded streaming decoder. It retains at most F+16 encoded section/trailer bytes, plus
/// one schema-directed structural record (at most O(F)); no history is retained.
/// A row decoder needs Read+Seek; a full decoder works with Read alone.
pub struct Decoder<R, F: Family> {
    reader: R,
    family: std::marker::PhantomData<F>,
    buffer: Vec<u8>,
    offset: u64,
    expected: u64,
    tail: Tail,
    halted: bool,
}
impl<R: Read, F: Family> Decoder<R, F> {
    /// Start a complete frame stream, sequence one at byte zero.
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            family: std::marker::PhantomData,
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
            family: std::marker::PhantomData,
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
    pub fn next_frame(&mut self) -> Result<Option<Frame<'_, F::Record>>, F::Error> {
        self.read_frame(|_, _| Ok(()), true)
    }
    fn read_frame(
        &mut self,
        skip_payload: impl FnOnce(&mut R, usize) -> io::Result<()>,
        full: bool,
    ) -> Result<Option<Frame<'_, F::Record>>, F::Error> {
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
            let suffix = scan_suffix::<F>(&mut self.reader, offset + n as u64)?;
            if suffix.has_frame {
                return Err(error::<F>(offset, None, Fault::Crc(Section::Prefix)));
            }
            if let Some(at) = suffix.first_nonzero {
                return Err(error::<F>(
                    offset,
                    None,
                    Fault::NonZeroAfterZeros { offset: at },
                ));
            }
            self.tail = Tail::Torn {
                offset,
                kind: TornKind::ZeroFilled,
            };
            return Ok(None);
        }
        if n < PREFIX_LEN {
            // These families cannot start with zero. Preserve the refusal for
            // garbage following a zero-filled frame boundary, even at short EOF.
            if prefix[0] == 0 {
                let at = prefix[..n].iter().position(|b| *b != 0).unwrap();
                return Err(error::<F>(
                    offset,
                    None,
                    Fault::NonZeroAfterZeros {
                        offset: offset + at as u64,
                    },
                ));
            }
            validate_short_prefix::<F>(&prefix, n, self.expected)
                .map_err(|fault| F::error(offset, None, fault))?;
            self.tail = Tail::Torn {
                offset,
                kind: TornKind::ShortPrefix,
            };
            return Ok(None);
        }
        if crc64_xz(&prefix[..20]) != u64::from_le_bytes(prefix[20..].try_into().unwrap()) {
            let fallback = Fault::Crc(Section::Prefix);
            if prefix[0] == 0 {
                let suffix = scan_suffix::<F>(&mut self.reader, offset + n as u64)?;
                if !suffix.has_frame {
                    let at = prefix[..n].iter().position(|b| *b != 0).unwrap();
                    return Err(error::<F>(
                        offset,
                        None,
                        Fault::NonZeroAfterZeros {
                            offset: offset + at as u64,
                        },
                    ));
                }
            }
            return Err(error::<F>(offset, None, fallback));
        }
        let seq = u64::from_le_bytes(prefix[12..20].try_into().unwrap());
        let fail = |fault| error::<F>(offset, Some(seq), fault);
        let version = u16::from_le_bytes(prefix[..2].try_into().unwrap());
        let tag = u16::from_le_bytes(prefix[2..4].try_into().unwrap());
        F::validate(version, tag).map_err(|fault| F::error(offset, Some(seq), fault))?;
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
        self.buffer.resize(n, 0);
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
        let record = F::decode(tag, &self.buffer[..s])
            .map_err(|fault| F::error(offset, Some(seq), fault))?;
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
    ) -> Result<TornKind, F::Error> {
        let body = &self.buffer;
        let offset = self.offset;
        let fail = |fault| error::<F>(offset, Some(seq), fault);
        let suffix = scan_suffix::<F>(
            &mut self.reader,
            offset + PREFIX_LEN as u64 + body.len() as u64,
        )?;
        if suffix.has_frame {
            return Err(fail(Fault::Crc(bad.unwrap_or(Section::Structural))));
        }
        if !short && body.first() == Some(&0) {
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
            // A valid prefix bounds an EOF-short section. Internal zero bytes
            // are ordinary CBOR/payload data, not evidence of a filled hole.
            return Ok(TornKind::ShortBody);
        }
        unreachable!("classifier requires a short body or failed CRC")
    }
}
impl<R: Read + Seek, F: Family> Decoder<R, F> {
    /// Skip payloads of healthy non-final frames in an immutable snapshot.
    /// Final or structurally damaged frames use the full decoder's classifier,
    /// which reads their payloads. Returned rows never certify payload integrity
    /// of the preceding stream and cannot be passed to the fold.
    pub fn next_row_frame(&mut self) -> Result<Option<Frame<'_, F::Record>>, F::Error> {
        if self.halted {
            return Ok(None);
        }
        let position = self.reader.stream_position()?;
        let mut prefix = [0; PREFIX_LEN];
        let n = read_some(&mut self.reader, &mut prefix)?;
        let mut full = true;
        if n == PREFIX_LEN && valid_prefix::<F>(&prefix) {
            let s = u32::from_le_bytes(prefix[4..8].try_into().unwrap()) as u64;
            let p = u32::from_le_bytes(prefix[8..12].try_into().unwrap()) as u64;
            let next = position
                .checked_add(FRAME_OVERHEAD as u64 + s + p)
                .ok_or_else(|| error::<F>(self.offset, None, Fault::Overflow))?;
            self.reader.seek(SeekFrom::Start(next))?;
            let n = read_some(&mut self.reader, &mut prefix)?;
            full = n != PREFIX_LEN || !valid_prefix::<F>(&prefix);
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

// Unread bytes in the decoder's prefix buffer are zero. For lengths they give
// a lower bound; for fixed fields compare only bytes actually present.
fn validate_short_prefix<F: Family>(
    prefix: &[u8; PREFIX_LEN],
    n: usize,
    expected: u64,
) -> Result<(), F::Fault> {
    F::validate_short_identity(prefix, n)?;
    let structural = u32::from_le_bytes(prefix[4..8].try_into().unwrap()) as u64;
    let payload = u32::from_le_bytes(prefix[8..12].try_into().unwrap()) as u64;
    if structural + payload > MAX_FRAME_BODY as u64 {
        return Err(Fault::FrameTooLarge.into());
    }
    if n > 12 && prefix[12..n.min(20)] != expected.to_le_bytes()[..n.min(20) - 12] {
        return Err(Fault::Sequence {
            expected,
            actual: u64::from_le_bytes(prefix[12..20].try_into().unwrap()),
        }
        .into());
    }
    Ok(())
}
fn valid_prefix<F: Family>(bytes: &[u8]) -> bool {
    bytes.len() == PREFIX_LEN
        && crc64_xz(&bytes[..20]) == u64::from_le_bytes(bytes[20..].try_into().unwrap())
        && F::valid_identity(
            u16::from_le_bytes(bytes[..2].try_into().unwrap()),
            u16::from_le_bytes(bytes[2..4].try_into().unwrap()),
        )
        && u64::from(u32::from_le_bytes(bytes[4..8].try_into().unwrap()))
            + u64::from(u32::from_le_bytes(bytes[8..12].try_into().unwrap()))
            <= MAX_FRAME_BODY as u64
}
#[derive(Default)]
struct Suffix {
    first_nonzero: Option<u64>,
    has_frame: bool,
}
// Error paths only: bounded lookahead distinguishes a final zero region from
// corruption followed by another frame. A valid prefix is sufficient to refuse
// tail treatment; it never authorizes any bytes or resumes decoding past damage.
fn scan_suffix<F: Family>(reader: &mut impl Read, mut offset: u64) -> io::Result<Suffix> {
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
        result.has_frame |= scratch[..kept + n]
            .windows(PREFIX_LEN)
            .any(valid_prefix::<F>);
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

/// Copy bounded, already preflighted frames with one whole-frame write each.
/// A short kernel write may still tear; EOF-short prefixes/bodies handle that.
///
/// B1's one intentional behaviour change: recheck the private spool's prefix CRC
/// before trusting its lengths. A corrupt preflighted spool is refused; the
/// caller's already-fenced lease reports Ambiguous and keeps its sticky marker.
/// Before the extraction, plausible lengths alone allowed the frame to be copied.
pub fn copy_frames(
    input: &mut impl Read,
    output: &mut impl Write,
    mut left: u64,
    mut copied: impl FnMut(u64),
) -> io::Result<()> {
    let mut bytes = Vec::new();
    while left > 0 {
        let mut prefix = [0; PREFIX_LEN];
        input.read_exact(&mut prefix)?;
        if crc64_xz(&prefix[..20]) != u64::from_le_bytes(prefix[20..].try_into().unwrap()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid preflighted frame prefix CRC",
            ));
        }
        let body = u64::from(u32::from_le_bytes(prefix[4..8].try_into().unwrap()))
            + u64::from(u32::from_le_bytes(prefix[8..12].try_into().unwrap()));
        let len = FRAME_OVERHEAD as u64 + body;
        if body > MAX_FRAME_BODY as u64 || len > left {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid preflighted frame size",
            ));
        }
        bytes.resize(len as usize, 0);
        bytes[..PREFIX_LEN].copy_from_slice(&prefix);
        input.read_exact(&mut bytes[PREFIX_LEN..])?;
        output.write_all(&bytes)?;
        copied(len);
        left -= len;
    }
    Ok(())
}
