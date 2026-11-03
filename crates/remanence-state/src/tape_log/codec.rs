//! Streaming frame codec and byte-tail classification. A row scan deliberately
//! skips healthy non-final payloads; final/damaged frames share full classification.
//! It is not a payload trust-validation pass over the preceding stream.
//! EOF-short prefixes are torn only when every visible field agrees with the
//! format and next sequence. EOF-short bodies may contain zeros followed by data:
//! they cannot close a group, so internal zero bytes do not make them corruption.

use super::*;
use std::io::{self, Read, Seek, Write};

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

/// Validated frame with the tape family's decoded structural record.
pub type Frame<'a> = crate::log_frame::Frame<'a, Record>;

impl From<crate::log_frame::Fault> for Fault {
    fn from(fault: crate::log_frame::Fault) -> Self {
        use crate::log_frame::Fault as Raw;
        match fault {
            Raw::Crc(section) => Self::Crc(section),
            Raw::NonZeroAfterZeros { offset } => Self::NonZeroAfterZeros { offset },
            Raw::FrameTooLarge => Self::FrameTooLarge,
            Raw::Sequence { expected, actual } => Self::Sequence { expected, actual },
            Raw::Overflow => Self::Overflow,
            Raw::Invalid(reason) => Self::Invalid(reason),
        }
    }
}
struct TapeFamily;
impl crate::log_frame::Family for TapeFamily {
    type Record = Record;
    type Fault = Fault;
    type Error = CodecError;
    fn validate(version: u16, tag: u16) -> Result<(), Fault> {
        if version != VERSION {
            return Err(Fault::Version(version));
        }
        let tag = Tag::try_from(tag)?;
        if matches!(tag, Tag::FinalizationPlanned | Tag::UnprotectedAccepted) {
            return Err(Fault::ReservedTag(tag));
        }
        Ok(())
    }
    fn validate_short_identity(prefix: &[u8; PREFIX_LEN], n: usize) -> Result<(), Fault> {
        if prefix[..n.min(2)] != VERSION.to_le_bytes()[..n.min(2)] {
            return Err(Fault::Version(u16::from_le_bytes(
                prefix[..2].try_into().unwrap(),
            )));
        }
        if n > 2 {
            Self::validate(
                VERSION,
                u16::from_le_bytes(prefix[2..4].try_into().unwrap()),
            )?;
        }
        Ok(())
    }
    fn valid_identity(version: u16, tag: u16) -> bool {
        version == VERSION && Tag::try_from(tag).is_ok()
    }
    fn decode(tag: u16, structural: &[u8]) -> Result<Record, Fault> {
        wire::decode(Tag::try_from(tag)?, structural)
    }
    fn error(offset: u64, sequence: Option<u64>, fault: Fault) -> CodecError {
        error(offset, sequence, fault)
    }
}

/// Encode one tape record without publishing or validating group semantics.
pub fn encode_frame(
    w: &mut impl Write,
    sequence: u64,
    record: &Record,
    payload: &[u8],
) -> Result<(), CodecError> {
    if payload.len() > MAX_FRAME_BODY {
        return Err(CodecError::EncodeSchema(Fault::FrameTooLarge));
    }
    let mut structural = crate::log_frame::Limited {
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
    crate::log_frame::encode_frame(
        w,
        VERSION,
        record.tag() as u16,
        sequence,
        &structural.bytes,
        payload,
    )?;
    Ok(())
}
#[cfg(test)]
pub(super) fn prefix_bytes(version: u16, tag: Tag, s: u32, p: u32, sequence: u64) -> [u8; 20] {
    crate::log_frame::prefix_bytes(version, tag as u16, s, p, sequence)
}

/// Tape adapter around the shared bounded reader. Group closure is handled by Fold.
pub struct Decoder<R> {
    inner: crate::log_frame::Decoder<R, TapeFamily>,
    #[cfg(any(test, feature = "fixtures"))]
    measured: bool,
}
impl<R: Read> Decoder<R> {
    /// Start at sequence one and byte zero.
    pub fn new(reader: R) -> Self {
        Self::at(reader, 1, 0)
    }
    /// Start at a known frame boundary in an immutable snapshot.
    pub fn at(reader: R, sequence: u64, offset: u64) -> Self {
        Self {
            inner: crate::log_frame::Decoder::at(reader, sequence, offset),
            #[cfg(any(test, feature = "fixtures"))]
            measured: false,
        }
    }
    /// Byte-tail classification; the fold adds the unclosed-group case.
    pub fn tail(&self) -> Tail {
        use crate::log_frame::{Tail as RawTail, TornKind as RawKind};
        match self.inner.tail() {
            RawTail::Complete => Tail::Complete,
            RawTail::Torn { offset, kind } => Tail::Torn {
                offset,
                kind: match kind {
                    RawKind::ShortPrefix => TornKind::ShortPrefix,
                    RawKind::ShortBody => TornKind::ShortBody,
                    RawKind::ZeroFilled => TornKind::ZeroFilled,
                },
            },
        }
    }
    /// Actual retained allocation, for memory-bound tests.
    pub fn retained_buffer_bytes(&self) -> usize {
        self.inner.retained_buffer_bytes()
    }
    /// Decode and validate all three CRCs and the structural schema.
    pub fn next_frame(&mut self) -> Result<Option<Frame<'_>>, CodecError> {
        #[cfg(any(test, feature = "fixtures"))]
        self.measure_start(false);
        let frame = self.inner.next_frame()?;
        #[cfg(any(test, feature = "fixtures"))]
        if let Some(frame) = &frame {
            super::measure::frame(false, frame.end - frame.offset);
        }
        Ok(frame)
    }
    #[cfg(any(test, feature = "fixtures"))]
    fn measure_start(&mut self, row: bool) {
        if !self.measured {
            super::measure::start(row);
            self.measured = true;
        }
    }
}
impl<R: Read + Seek> Decoder<R> {
    /// Structural row scan over an immutable snapshot, not a payload trust pass.
    pub fn next_row_frame(&mut self) -> Result<Option<Frame<'_>>, CodecError> {
        #[cfg(any(test, feature = "fixtures"))]
        self.measure_start(true);
        let frame = self.inner.next_row_frame()?;
        #[cfg(any(test, feature = "fixtures"))]
        if let Some(frame) = &frame {
            super::measure::frame(true, frame.end - frame.offset);
        }
        Ok(frame)
    }
}
/// Pure frame-tail classifier over bytes. It validates every section, but use
/// fold::replay for semantic contradictions and well-formed unclosed groups.
pub fn classify_frames(bytes: &[u8]) -> Result<Tail, CodecError> {
    let mut decoder = Decoder::new(bytes);
    while decoder.next_frame()?.is_some() {}
    Ok(decoder.tail())
}
