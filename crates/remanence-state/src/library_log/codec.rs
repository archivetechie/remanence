//! Library segment identity and chained frames over the shared streaming decoder.
use super::*;
use crate::log_frame::{self, Family};
use remanence_crc::crc64_xz;
use sha2::{Digest, Sha256};
use std::io::{self, Read};
/// Library frame and header version.
pub const VERSION: u16 = 1;
const _: () = assert!(VERSION & 0xff != 0);
/// Immutable segment header size.
pub const HEADER_LEN: u64 = 84;
const MAGIC: &[u8; 8] = b"REMLIB\0\x01";
/// Byte-level refusal with exact frame location supplied by the shared decoder.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Fault {
    /// Common frame validation.
    #[error("{0}")]
    Frame(#[from] log_frame::Fault),
    /// Unsupported version.
    #[error("Version({0})")]
    Version(u16),
    /// Unallocated tag.
    #[error("UnknownTag({0})")]
    UnknownTag(u16),
    /// Invalid deterministic structural data.
    #[error("NoncanonicalCbor")]
    Cbor,
    /// Payload must contain exactly the chain hash.
    #[error("PayloadLength")]
    PayloadLength,
    /// A record does not extend the chain.
    #[error("RecordHash")]
    RecordHash,
    /// An invalid or short header.
    #[error("Header")]
    Header,
    /// Wrong authority identity.
    #[error("LogUuid")]
    LogUuid,
    /// Segment name does not match its header.
    #[error("SegmentName")]
    SegmentName,
    /// Discontinuous segment start.
    #[error("SegmentSequence")]
    SegmentSequence,
    /// Previous hash does not bind the preceding segment.
    #[error("PreviousSegmentHash")]
    PreviousSegmentHash,
    /// Atomic publication cannot produce an empty segment.
    #[error("EmptySegment")]
    EmptySegment,
    /// An interior segment cannot have an absent tail.
    #[error("InteriorTorn")]
    InteriorTorn,
}
/// Environmental or byte-level decoding failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Environmental I/O.
    #[error("library I/O: {0}")]
    Io(#[from] io::Error),
    /// Exact damaged prefix boundary.
    #[error("at {offset}, sequence {sequence:?}: {fault}")]
    Recovery {
        /// Frame start.
        offset: u64,
        /// Sequence when the prefix was validated.
        sequence: Option<u64>,
        /// Exact fault.
        fault: Fault,
    },
}
/// Library adapter for the family-neutral frame engine.
pub struct LibraryFamily;
impl Family for LibraryFamily {
    type Record = Record;
    type Fault = Fault;
    type Error = Error;
    fn validate(version: u16, tag: u16) -> Result<(), Fault> {
        if version != VERSION {
            return Err(Fault::Version(version));
        }
        if !matches!(tag, 1..=26 | 32) {
            return Err(Fault::UnknownTag(tag));
        }
        Ok(())
    }
    fn validate_short_identity(p: &[u8; 28], n: usize) -> Result<(), Fault> {
        if p[..n.min(2)] != VERSION.to_le_bytes()[..n.min(2)] {
            return Err(Fault::Version(u16::from_le_bytes([p[0], p[1]])));
        }
        if n > 2 {
            Self::validate(VERSION, u16::from_le_bytes([p[2], p[3]]))?;
        }
        Ok(())
    }
    fn valid_identity(version: u16, tag: u16) -> bool {
        Self::validate(version, tag).is_ok()
    }
    fn decode(tag: u16, structural: &[u8]) -> Result<Record, Fault> {
        wire::decode(tag, structural).map_err(|_| Fault::Cbor)
    }
    fn error(offset: u64, sequence: Option<u64>, fault: Fault) -> Error {
        Error::Recovery {
            offset,
            sequence,
            fault,
        }
    }
}
/// Bounded one-frame decoder.
pub type Decoder<R> = log_frame::Decoder<R, LibraryFamily>;
/// A segment's immutable chain anchor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    /// Expected manifest identity.
    pub log_uuid: Id,
    /// Next contiguous global sequence.
    pub first_sequence: u64,
    /// Hash at the preceding segment's end.
    pub previous_segment_hash: Hash,
    /// Valid UTC calendar date; never orders authority.
    pub date: [u8; 10],
}
impl Header {
    /// Encode the immutable header with CRC-64/XZ.
    pub fn encode(&self) -> Result<[u8; 84], Fault> {
        if self.first_sequence == 0 || !valid_date(&self.date) {
            return Err(Fault::Header);
        }
        let mut b = [0; 84];
        b[..8].copy_from_slice(MAGIC);
        b[8..10].copy_from_slice(&VERSION.to_le_bytes());
        b[10..26].copy_from_slice(&self.log_uuid);
        b[26..34].copy_from_slice(&self.first_sequence.to_le_bytes());
        b[34..66].copy_from_slice(&self.previous_segment_hash);
        b[66..76].copy_from_slice(&self.date);
        let crc = crc64_xz(&b[..76]);
        b[76..].copy_from_slice(&crc.to_le_bytes());
        Ok(b)
    }
    /// Validate before accepting any record in a segment.
    pub fn decode(r: &mut impl Read, uuid: Id) -> Result<Self, Fault> {
        let mut b = [0; 84];
        r.read_exact(&mut b).map_err(|_| Fault::Header)?;
        if &b[..8] != MAGIC
            || u16::from_le_bytes(b[8..10].try_into().unwrap()) != VERSION
            || crc64_xz(&b[..76]) != u64::from_le_bytes(b[76..].try_into().unwrap())
        {
            return Err(Fault::Header);
        }
        let h = Self {
            log_uuid: b[10..26].try_into().unwrap(),
            first_sequence: u64::from_le_bytes(b[26..34].try_into().unwrap()),
            previous_segment_hash: b[34..66].try_into().unwrap(),
            date: b[66..76].try_into().unwrap(),
        };
        if h.log_uuid != uuid {
            return Err(Fault::LogUuid);
        }
        h.encode()?;
        Ok(h)
    }
    /// Canonical path basename.
    pub fn name(&self) -> String {
        format!(
            "{:020}-{}.remlib",
            self.first_sequence,
            String::from_utf8_lossy(&self.date)
        )
    }
}
fn valid_date(d: &[u8; 10]) -> bool {
    if d[4] != b'-'
        || d[7] != b'-'
        || d.iter()
            .enumerate()
            .any(|(i, b)| i != 4 && i != 7 && !b.is_ascii_digit())
    {
        return false;
    }
    let number = |s: &[u8]| s.iter().fold(0u16, |a, b| a * 10 + u16::from(b - b'0'));
    let year = number(&d[..4]);
    let month = number(&d[5..7]);
    let day = number(&d[8..]);
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => 0,
    };
    year > 0 && day > 0 && day <= days
}
/// Hash canonical definitions without envelope or frame metadata.
pub fn configuration_digest(definitions: &Definitions) -> io::Result<Hash> {
    use crate::log_frame::cbor::Wire;
    let mut b = Vec::new();
    definitions.put(&mut b)?;
    Ok(Sha256::digest(b).into())
}
/// Encode one candidate, before any publication. The hash occupies the full payload.
pub fn encode(sequence: u64, previous: Hash, record: &Record) -> io::Result<(Vec<u8>, Hash)> {
    let mut s = log_frame::Limited {
        bytes: Vec::new(),
        limit: log_frame::MAX_FRAME_BODY - 32,
    };
    wire::encode(record, &mut s)?;
    let prefix = log_frame::prefix_bytes(
        VERSION,
        record.body.tag() as u16,
        s.bytes.len() as u32,
        32,
        sequence,
    );
    let hash = chain_hash(previous, &prefix, &s.bytes);
    let mut out = Vec::new();
    log_frame::encode_frame(
        &mut out,
        VERSION,
        record.body.tag() as u16,
        sequence,
        &s.bytes,
        &hash,
    )?;
    Ok((out, hash))
}
fn chain_hash(previous: Hash, prefix: &[u8; 20], structural: &[u8]) -> Hash {
    let mut h = Sha256::new();
    h.update(previous);
    h.update(prefix);
    h.update(structural);
    h.finalize().into()
}
/// Refuse wrong payload lengths before hashing, then verify chain continuity.
pub fn verify(frame: &log_frame::Frame<'_, Record>, previous: Hash) -> Result<Hash, Fault> {
    if frame.payload.len() != 32 {
        return Err(Fault::PayloadLength);
    }
    let hash = chain_hash(previous, &frame.prefix, frame.structural);
    if frame.payload != hash {
        return Err(Fault::RecordHash);
    }
    Ok(hash)
}
