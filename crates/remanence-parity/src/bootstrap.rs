//! Bootstrap block — the canonical root of trust per tape.
//!
//! The bootstrap is the first block a reader finds on tape
//! mount. It tells the reader the parity scheme (so a
//! [`ObjectParitySource`](crate::ObjectParitySource) can be constructed),
//! the tape UUID (which derives the per-tape parity magic), and
//! the filemark-map digest that validates catalog-less
//! reconstruction.
//!
//! On-tape layout per `docs/layer3c-design.md` v0.4.4 §5.6: a
//! fixed header with `cbor_payload_len` covered by the header
//! CRC-64/XZ, followed by a CBOR payload, a payload CRC-64/XZ,
//! and zero padding to fill one tape block.
//!
//! Discovery uses the raw tape adapter so candidate block-size probes are real
//! fixed-block reads rather than buffer-size changes.

use remanence_library::{scsi::decode_sense, TapeIoError};

use crate::cbor::IntegerMapKeyTracker;
use crate::diagnostic_text::{
    escape_member_name, log_ignored_diagnostic_text, validate_write_timestamp,
    validate_writer_version,
};
use crate::error::ParityError;
use crate::filemark_map::{sole_bot_filemark_map_digest, FilemarkMapDigest};
use crate::raw::{PhysicalPositionHint, RawReadOutcome, RawTapeSource};
use crate::sidecar::crc64_xz;

/// Magic at byte 0 of every bootstrap block.
pub const BOOTSTRAP_MAGIC: [u8; 8] = *b"REM\x00BOO\x01";

/// Schema-major version this writer emits / this reader
/// accepts. Major bumps require an explicit migration plan
/// documented in `docs/layer3c-design.md`.
pub const BOOTSTRAP_SCHEMA_MAJOR: u16 = 2;

/// Schema-minor version this writer emits. Reader accepts
/// minors `<= BOOTSTRAP_SCHEMA_MINOR` written by older
/// versions, and minors `> BOOTSTRAP_SCHEMA_MINOR` written by
/// newer versions (forward-compatible). Minor bumps add fields
/// with sensible defaults.
pub const BOOTSTRAP_SCHEMA_MINOR: u16 = 3;

/// `flags` bit 0: this tape was written with `--parity none`
/// and contains no parity blocks. Readers see this bit and
/// bypass the parity source.
pub const FLAG_NO_PARITY: u32 = 1 << 0;

/// Byte offset of the bootstrap header CRC-64/XZ field.
pub const BOOTSTRAP_HEADER_CRC_OFFSET: usize = 0x30;

/// Size of the fixed bootstrap header, through the header CRC field.
pub const BOOTSTRAP_HEADER_LEN: usize = 0x38;

const BOOTSTRAP_PAYLOAD_CRC_LEN: usize = 8;
const LEGACY_INLINE_DIRECTORY_KEY: i128 = 20;
const LEGACY_PARITY_MAP_REFERENCE_KEY: i128 = 21;
const LEGACY_OBJECT_ROWS_KEY: i128 = 30;

/// Decoded bootstrap-block payload.
///
/// `scheme` is `Option<...>` because the design (§5.6) says a
/// `FLAG_NO_PARITY` bootstrap may omit the scheme record
/// entirely: "all other fields except magic, schema version,
/// tape UUID, block size, sequence, and header CRC may be
/// absent." Codex idref=794a16ac caught the earlier always-Some
/// shape rejecting compliant minimal no-parity bootstraps.
///
/// Invariant: if `scheme` is `Some`, its `no_parity_flag`
/// matches the bootstrap header's `FLAG_NO_PARITY` bit; if
/// `scheme` is `None`, the header's flag MUST be set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootstrapPayload {
    /// Parity scheme this tape was written with. `None` for
    /// no-parity bootstraps that omit the record entirely.
    pub scheme: Option<ParitySchemeRecord>,
    /// True iff this tape was written with `--parity none`
    /// (mirrors the [`FLAG_NO_PARITY`] bit in the bootstrap
    /// header). When `scheme` is `None`, this must be `true`.
    /// When `scheme` is `Some`, this should match
    /// `scheme.as_ref().unwrap().no_parity_flag`.
    pub no_parity_flag: bool,
    /// Filemark-map digest carried by this bootstrap. It may be
    /// omitted only on minimal no-parity bootstraps.
    pub filemark_map_digest: Option<FilemarkMapDigest>,
    /// Tape UUID (16 bytes, UUIDv4).
    pub tape_uuid: [u8; 16],
    /// rem software version string that wrote this tape. Use
    /// [`Self::escaped_written_by_version`] for display.
    pub written_by_version: String,
    /// RFC3339 timestamp of when this bootstrap copy was
    /// written. Use [`Self::escaped_written_at`] for display.
    pub written_at: String,
    /// Bootstrap sequence number. Schema-major 2 permits only the sole BOT
    /// Bootstrap, so this is always zero.
    pub sequence: u64,
    /// Tape block size in bytes that the writer used. Pinned
    /// so future readers can verify continuity without
    /// MODE SENSE. Also the size the writer expects the
    /// destination buffer to be (see
    /// [`write_bootstrap_block`]).
    pub block_size_bytes: u32,
    /// Effective drive hardware compression mode recorded at write-session
    /// open. This must be `false` for parity-protected tapes; a compressed
    /// parity tape has non-authoritative physical geometry and is refused for
    /// Layer 3c recovery.
    pub drive_compression: bool,
}

impl BootstrapPayload {
    /// Return the writer version escaped for a human-readable output channel.
    pub fn escaped_written_by_version(&self) -> String {
        escape_member_name(self.written_by_version.as_bytes())
    }

    /// Return the write timestamp escaped for a human-readable output channel.
    pub fn escaped_written_at(&self) -> String {
        escape_member_name(self.written_at.as_bytes())
    }
}

/// Decoded parity-scheme record from a bootstrap payload. Distinguished from
/// the in-memory
/// [`crate::ParityScheme`] because the on-tape representation
/// is forever-stable and must not depend on Rust type-system
/// evolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParitySchemeRecord {
    /// Scheme ID (e.g. `"rs-cauchy-gf256-v1"`).
    pub id: String,
    /// `k` — data blocks per stripe.
    pub data_blocks_per_stripe: u16,
    /// `m` — parity blocks per stripe.
    pub parity_blocks_per_stripe: u16,
    /// `S` — stripes per neighborhood.
    pub stripes_per_neighborhood: u32,
    /// True if this tape was written with `--parity none` and
    /// has no parity blocks. This mirrors the header flag in
    /// memory; it is not encoded inside the scheme CBOR map.
    pub no_parity_flag: bool,
}

/// Serialize a `BootstrapPayload` into a tape block buffer.
///
/// **Buffer contract** (codex idref=794a16ac Low catch): `buf`
/// must be at least `payload.block_size_bytes` long — exactly
/// one tape block. The function writes the framed bootstrap
/// into the prefix `buf[0..written]` and **zero-fills the
/// padding** `buf[written..block_size_bytes]` so the caller
/// can hand `buf` to the tape transport without leaking stale
/// bytes from a reused scratch buffer.
///
/// Returns the framed length (header + CBOR payload + payload
/// CRC). The total bytes touched in `buf` is
/// `payload.block_size_bytes`.
pub fn write_bootstrap_block(
    payload: &BootstrapPayload,
    buf: &mut [u8],
) -> Result<usize, ParityError> {
    // Codex idref=99c40750 Low: enforce the documented
    // invariants on the no_parity_flag / scheme pair before
    // serializing, so the writer never emits a frame the
    // parser would reject.
    match (&payload.scheme, payload.no_parity_flag) {
        (None, false) => {
            return Err(ParityError::Invariant(
                "BootstrapPayload: scheme = None requires no_parity_flag = true",
            ));
        }
        (Some(s), no_parity) if s.no_parity_flag != no_parity => {
            return Err(ParityError::Invariant(
                "BootstrapPayload: scheme.no_parity_flag must equal payload.no_parity_flag",
            ));
        }
        _ => {}
    }
    if !payload.no_parity_flag && payload.filemark_map_digest.is_none() {
        return Err(ParityError::Invariant(
            "BootstrapPayload: parity bootstrap requires filemark_map_digest",
        ));
    }
    if let Some(digest) = payload.filemark_map_digest.as_ref() {
        validate_sole_bot_map_digest(digest).map_err(ParityError::Invariant)?;
    }
    if !payload.no_parity_flag && payload.drive_compression {
        return Err(ParityError::DriveCompressionEnabled);
    }
    if payload.sequence != 0 {
        return Err(ParityError::Invariant(
            "schema-major 2 permits only the sequence-0 BOT Bootstrap",
        ));
    }
    if payload.written_by_version.is_empty() {
        return Err(ParityError::Invariant(
            "BootstrapPayload: written_by_version key 3 is required for writers",
        ));
    }

    let block_size = usize::try_from(payload.block_size_bytes).map_err(|_| {
        ParityError::Invariant("payload.block_size_bytes does not fit the host address space")
    })?;
    if buf.len() < block_size {
        return Err(ParityError::Invariant(
            "bootstrap buffer shorter than payload.block_size_bytes",
        ));
    }
    if block_size < BOOTSTRAP_HEADER_LEN + BOOTSTRAP_PAYLOAD_CRC_LEN {
        return Err(ParityError::Invariant(
            "payload.block_size_bytes smaller than fixed-header + payload CRC",
        ));
    }

    // 1. CBOR-encode the payload first so we know cbor_len.
    let cbor_bytes = encode_cbor_payload(payload)?;

    let payload_len_u32: u32 = cbor_bytes
        .len()
        .try_into()
        .map_err(|_| ParityError::Invariant("CBOR payload >= 4 GiB"))?;

    let total_len = BOOTSTRAP_HEADER_LEN
        .checked_add(cbor_bytes.len())
        .and_then(|n| n.checked_add(BOOTSTRAP_PAYLOAD_CRC_LEN))
        .ok_or(ParityError::Invariant("bootstrap size overflow"))?;
    if total_len > block_size {
        return Err(ParityError::BootstrapPayloadTooLarge {
            framed_len: total_len,
            block_size,
        });
    }

    // 2. Write the fixed header into bytes 0..BOOTSTRAP_HEADER_LEN.
    let flags = if payload.no_parity_flag {
        FLAG_NO_PARITY
    } else {
        0
    };
    buf[0..8].copy_from_slice(&BOOTSTRAP_MAGIC);
    buf[8..10].copy_from_slice(&BOOTSTRAP_SCHEMA_MAJOR.to_be_bytes());
    buf[10..12].copy_from_slice(&BOOTSTRAP_SCHEMA_MINOR.to_be_bytes());
    buf[12..16].copy_from_slice(&flags.to_be_bytes());
    buf[16..32].copy_from_slice(&payload.tape_uuid);
    buf[32..36].copy_from_slice(&payload.block_size_bytes.to_be_bytes());
    buf[36..44].copy_from_slice(&payload.sequence.to_be_bytes());
    buf[44..48].copy_from_slice(&payload_len_u32.to_le_bytes());
    // Header CRC covers bytes 0..0x30, including cbor_payload_len.
    let crc_header = crc64_xz(&buf[0..BOOTSTRAP_HEADER_CRC_OFFSET]);
    buf[48..56].copy_from_slice(&crc_header.to_le_bytes());

    // 3. Append CBOR + payload CRC.
    let cbor_end = BOOTSTRAP_HEADER_LEN + cbor_bytes.len();
    buf[BOOTSTRAP_HEADER_LEN..cbor_end].copy_from_slice(&cbor_bytes);
    let crc_payload = crc64_xz(&cbor_bytes);
    buf[cbor_end..cbor_end + BOOTSTRAP_PAYLOAD_CRC_LEN].copy_from_slice(&crc_payload.to_le_bytes());
    let framed_end = cbor_end + BOOTSTRAP_PAYLOAD_CRC_LEN;

    // 4. Zero-fill the padding so a reused scratch buffer
    // doesn't leak stale bytes onto tape.
    buf[framed_end..block_size].iter_mut().for_each(|b| *b = 0);

    Ok(framed_end)
}

/// Parse a tape block buffer into a `BootstrapPayload`. The
/// buffer must contain the full bootstrap block (header + CBOR
/// + payload CRC); any trailing zero padding is ignored.
///
/// Errors:
/// - [`ParityError::BootstrapParse`] if the magic doesn't match.
/// - [`ParityError::BootstrapParse`] on header / payload CRC
///   mismatch.
/// - [`ParityError::BootstrapParse`] on unsupported schema
///   version, malformed CBOR, or missing required fields.
pub fn parse_bootstrap_block(buf: &[u8]) -> Result<BootstrapPayload, ParityError> {
    if buf.len() < BOOTSTRAP_HEADER_LEN + BOOTSTRAP_PAYLOAD_CRC_LEN {
        return Err(ParityError::BootstrapParse(format!(
            "buffer too short: got {} bytes, need at least {}",
            buf.len(),
            BOOTSTRAP_HEADER_LEN + BOOTSTRAP_PAYLOAD_CRC_LEN
        )));
    }
    if buf[0..8] != BOOTSTRAP_MAGIC {
        return Err(ParityError::BootstrapParse(format!(
            "magic mismatch: got {:02x?}",
            &buf[0..8]
        )));
    }

    // Header CRC validates bytes 0..0x30 against bytes 0x30..0x38.
    let stored_header_crc = u64::from_le_bytes(buf[48..56].try_into().unwrap());
    let computed_header_crc = crc64_xz(&buf[0..BOOTSTRAP_HEADER_CRC_OFFSET]);
    if stored_header_crc != computed_header_crc {
        return Err(ParityError::BootstrapParse(format!(
            "header CRC mismatch: stored 0x{stored_header_crc:016x}, computed 0x{computed_header_crc:016x}"
        )));
    }

    let major = u16::from_be_bytes(buf[8..10].try_into().unwrap());
    let minor = u16::from_be_bytes(buf[10..12].try_into().unwrap());
    if major != BOOTSTRAP_SCHEMA_MAJOR {
        return Err(ParityError::BootstrapParse(format!(
            "unsupported bootstrap schema major version: got {major}, accept {BOOTSTRAP_SCHEMA_MAJOR}"
        )));
    }
    // Minor is forward-compatible — we accept higher minors but ignore
    // unknown fields when decoding.
    let _ = minor;

    let flags = u32::from_be_bytes(buf[12..16].try_into().unwrap());
    let no_parity = (flags & FLAG_NO_PARITY) != 0;
    let mut tape_uuid = [0u8; 16];
    tape_uuid.copy_from_slice(&buf[16..32]);
    let block_size_bytes = u32::from_be_bytes(buf[32..36].try_into().unwrap());
    let sequence = u64::from_be_bytes(buf[36..44].try_into().unwrap());
    if sequence != 0 {
        return Err(ParityError::BootstrapParse(
            "schema-major 2 permits only the sequence-0 BOT Bootstrap".into(),
        ));
    }
    let payload_len = usize::try_from(u32::from_le_bytes(buf[44..48].try_into().unwrap()))
        .map_err(|_| {
            ParityError::BootstrapParse("payload_len does not fit host address space".into())
        })?;

    let cbor_end = BOOTSTRAP_HEADER_LEN
        .checked_add(payload_len)
        .ok_or_else(|| ParityError::BootstrapParse("payload_len overflows".into()))?;
    let crc_end = cbor_end
        .checked_add(BOOTSTRAP_PAYLOAD_CRC_LEN)
        .ok_or_else(|| ParityError::BootstrapParse("payload+crc overflows".into()))?;
    if crc_end > buf.len() {
        return Err(ParityError::BootstrapParse(format!(
            "payload_len {payload_len} extends past buffer (need {crc_end}, got {})",
            buf.len()
        )));
    }

    let cbor_bytes = &buf[BOOTSTRAP_HEADER_LEN..cbor_end];
    let stored_payload_crc = u64::from_le_bytes(buf[cbor_end..crc_end].try_into().unwrap());
    let computed_payload_crc = crc64_xz(cbor_bytes);
    if stored_payload_crc != computed_payload_crc {
        return Err(ParityError::BootstrapParse(format!(
            "payload CRC mismatch: stored 0x{stored_payload_crc:016x}, computed 0x{computed_payload_crc:016x}"
        )));
    }

    let decoded = decode_cbor_payload(cbor_bytes, no_parity)?;

    // Codex idref=794a16ac Medium: scheme record is optional
    // only when FLAG_NO_PARITY is set. Reject a missing scheme
    // record on a parity-protected tape.
    if decoded.scheme_record.is_none() && !no_parity {
        return Err(ParityError::BootstrapParse(
            "CBOR payload missing scheme record (and FLAG_NO_PARITY not set)".into(),
        ));
    }
    if decoded.filemark_map_digest.is_none() && !no_parity {
        return Err(ParityError::BootstrapParse(
            "CBOR payload missing filemark map digest (and FLAG_NO_PARITY not set)".into(),
        ));
    }
    if let Some(digest) = decoded.filemark_map_digest.as_ref() {
        validate_sole_bot_map_digest(digest)
            .map_err(|message| ParityError::BootstrapParse(message.to_string()))?;
    }

    Ok(BootstrapPayload {
        scheme: decoded.scheme_record,
        no_parity_flag: no_parity,
        filemark_map_digest: decoded.filemark_map_digest,
        tape_uuid,
        written_by_version: decoded.written_by_version,
        written_at: decoded.written_at,
        sequence,
        block_size_bytes,
        drive_compression: decoded.drive_compression,
    })
}

fn validate_sole_bot_map_digest(digest: &FilemarkMapDigest) -> Result<(), &'static str> {
    let Ok(expected) = sole_bot_filemark_map_digest() else {
        return Err("could not derive the canonical sole-BOT filemark map digest");
    };
    if digest != &expected {
        return Err("filemark map digest must describe exactly the sole tape-file-0 BOT Bootstrap");
    }
    Ok(())
}

/// Cheap magic-only check used by the discovery scanner before
/// running the full parse. Returns true if `buf` starts with
/// the bootstrap magic.
pub fn has_bootstrap_magic(buf: &[u8]) -> bool {
    buf.len() >= BOOTSTRAP_MAGIC.len() && buf[0..BOOTSTRAP_MAGIC.len()] == BOOTSTRAP_MAGIC
}

/// Candidate fixed block sizes used when the caller has no catalog
/// or operator-provided block-size hint. The normal path should use
/// [`discover_bootstrap_with_block_size`].
pub const DEFAULT_BOOTSTRAP_CANDIDATE_BLOCK_SIZES: &[u32] = &[256 * 1024, 512 * 1024, 1024 * 1024];

/// Find the sole valid schema-major 2 Bootstrap block at LBA 0.
///
/// Used at tape-mount time before constructing a
/// [`ObjectParitySource`](crate::ObjectParitySource) — the source needs the
/// scheme, which only the bootstrap can provide.
///
/// The tape-size hint is accepted for caller symmetry but does not alter the
/// sole-BOT lookup.
pub fn discover_bootstrap(
    source: &mut dyn RawTapeSource,
    tape_total_blocks_hint: Option<u64>,
) -> Result<BootstrapPayload, ParityError> {
    discover_bootstrap_with_candidate_block_sizes(
        source,
        tape_total_blocks_hint,
        DEFAULT_BOOTSTRAP_CANDIDATE_BLOCK_SIZES,
    )
}

/// Discover a bootstrap when the tape's fixed block size is already
/// known from the catalog, operator config, or Layer 3a setup.
pub fn discover_bootstrap_with_block_size(
    source: &mut dyn RawTapeSource,
    _tape_total_blocks_hint: Option<u64>,
    block_size: u32,
) -> Result<BootstrapPayload, ParityError> {
    source.configure_fixed_block_size(block_size)?;
    match try_read_bootstrap_at(source, 0, block_size) {
        Ok(payload) => Ok(payload),
        Err(err) if bootstrap_probe_can_continue(&err) => {
            if matches!(err, ParityError::BootstrapParse(_)) {
                Err(err)
            } else {
                Err(ParityError::NoBootstrapFound)
            }
        }
        Err(err) => Err(err),
    }
}

/// Discover a bootstrap for a catalog-less tape whose block size is
/// unknown. Each candidate is a real fixed-block read size from the
/// caller's perspective; the first candidate whose block parses and
/// whose header records the same size wins.
pub fn discover_bootstrap_with_candidate_block_sizes(
    source: &mut dyn RawTapeSource,
    _tape_total_blocks_hint: Option<u64>,
    candidate_block_sizes: &[u32],
) -> Result<BootstrapPayload, ParityError> {
    for block_size in candidate_block_sizes {
        source.configure_fixed_block_size(*block_size)?;
        match try_read_bootstrap_at(source, 0, *block_size) {
            Ok(payload) => return Ok(payload),
            Err(err) if bootstrap_probe_can_continue(&err) => continue,
            Err(err) => return Err(err),
        }
    }
    Err(ParityError::NoBootstrapFound)
}

fn bootstrap_probe_can_continue(err: &ParityError) -> bool {
    matches!(
        err,
        ParityError::NoBootstrapAtPosition(_) | ParityError::BootstrapParse(_)
    )
}

/// Return the sole schema-major 2 Bootstrap position.
pub fn expected_bootstrap_positions(_tape_total_blocks_hint: Option<u64>) -> Vec<u64> {
    vec![0]
}

fn try_read_bootstrap_at(
    source: &mut dyn RawTapeSource,
    target_lba: u64,
    block_size: u32,
) -> Result<BootstrapPayload, ParityError> {
    if block_size == 0 {
        return Err(ParityError::Invariant("bootstrap block size is zero"));
    }
    let block_size = usize::try_from(block_size).map_err(|_| {
        ParityError::Invariant("bootstrap block size does not fit the host address space")
    })?;
    source.locate_physical(PhysicalPositionHint::new(target_lba))?;
    let mut buf = vec![0u8; block_size];
    for _ in 0..1 {
        match source.read_record(&mut buf) {
            Ok(RawReadOutcome::Block { bytes, .. }) if bytes != block_size => {
                return Err(ParityError::BootstrapParse(format!(
                    "short fixed-block bootstrap read: got {bytes} bytes, expected {block_size}"
                )));
            }
            Ok(RawReadOutcome::Block { .. }) => {
                if has_bootstrap_magic(&buf) {
                    // Magic hit — try to parse. If the parse
                    // succeeds, return immediately. If it fails
                    // (corrupted bootstrap or a user block that
                    // deliberately starts with the magic), keep
                    // scanning forward — the next block in the
                    // window might be a valid bootstrap.
                    match parse_bootstrap_block(&buf) {
                        Ok(bp) => {
                            let bootstrap_block_size = usize::try_from(bp.block_size_bytes)
                                .map_err(|_| {
                                    ParityError::BootstrapParse(
                                        "bootstrap block_size does not fit host address space"
                                            .into(),
                                    )
                                })?;
                            if bootstrap_block_size != block_size {
                                return Err(ParityError::BootstrapParse(format!(
                                    "bootstrap block_size {} does not match read size {block_size}",
                                    bp.block_size_bytes
                                )));
                            }
                            return Ok(bp);
                        }
                        Err(ParityError::DriveCompressionEnabled) => {
                            return Err(ParityError::DriveCompressionEnabled);
                        }
                        Err(_) => {}
                    }
                }
            }
            Ok(RawReadOutcome::Filemark { .. }) => continue,
            Ok(RawReadOutcome::EndOfData { .. }) => {
                return Err(ParityError::NoBootstrapAtPosition(target_lba));
            }
            Err(ParityError::TapeIo(remanence_library::TapeIoError::ReadBufferTooSmall {
                actual,
                provided,
            })) => {
                return Err(ParityError::BootstrapParse(format!(
                    "bootstrap block larger than candidate read size: actual {actual}, provided {provided}"
                )));
            }
            // Medium-error reads skip past the bad block and keep scanning.
            // Transport and other drive-state errors propagate; they do not
            // mean "this position has no bootstrap."
            Err(ParityError::TapeIo(err)) if bootstrap_read_error_can_continue(&err) => continue,
            Err(err) => return Err(err),
        }
    }
    Err(ParityError::NoBootstrapAtPosition(target_lba))
}

fn bootstrap_read_error_can_continue(err: &TapeIoError) -> bool {
    match err {
        TapeIoError::CheckCondition(remanence_library::scsi::ScsiError::CheckCondition {
            sense,
            ..
        }) => {
            // IBM LTO SCSI Reference GA32-0928-08 Annex B Table B.4 defines
            // sense key 3 as Medium Error. Fixed-format and descriptor-format
            // sense carry that key at different offsets, so use Layer 1's
            // shared decoder instead of duplicating the byte layout here.
            decode_sense(sense).is_some_and(|decoded| decoded.key == 0x03)
        }
        _ => false,
    }
}

// ====================================================================
// CBOR encode/decode — uses ciborium::Value to build the integer-
// keyed map shape the design doc pins (smaller than tstr keys and
// stable forever).
// ====================================================================

use ciborium::value::Value as CborValue;

fn encode_cbor_payload(payload: &BootstrapPayload) -> Result<Vec<u8>, ParityError> {
    let mut entries: Vec<(CborValue, CborValue)> = Vec::new();

    // Tag 1: scheme record. Omitted on no-parity bootstraps
    // per design §5.6 / codex idref=794a16ac.
    if let Some(scheme) = payload.scheme.as_ref() {
        let scheme_map = CborValue::Map(vec![
            (
                CborValue::Integer(1.into()),
                CborValue::Text(scheme.id.clone()),
            ),
            (
                CborValue::Integer(2.into()),
                CborValue::Integer(scheme.data_blocks_per_stripe.into()),
            ),
            (
                CborValue::Integer(3.into()),
                CborValue::Integer(scheme.parity_blocks_per_stripe.into()),
            ),
            (
                CborValue::Integer(4.into()),
                CborValue::Integer(scheme.stripes_per_neighborhood.into()),
            ),
        ]);
        entries.push((CborValue::Integer(1.into()), scheme_map));
    }

    // Tag 2: filemark-map digest. Omitted only for minimal no-parity
    // bootstraps.
    if let Some(digest) = payload.filemark_map_digest.as_ref() {
        entries.push((
            CborValue::Integer(2.into()),
            encode_filemark_map_digest(digest),
        ));
    }

    // Tag 3: software version. Omitted if empty.
    if !payload.written_by_version.is_empty() {
        validate_writer_version(&payload.written_by_version).map_err(|violated_bound| {
            ParityError::BootstrapParse(format!(
                "bootstrap payload key 3 violates {violated_bound}"
            ))
        })?;
        entries.push((
            CborValue::Integer(3.into()),
            CborValue::Text(payload.written_by_version.clone()),
        ));
    }

    // Tag 4: write timestamp. Omitted if empty.
    if !payload.written_at.is_empty() {
        validate_write_timestamp(&payload.written_at).map_err(|violated_bound| {
            ParityError::BootstrapParse(format!(
                "bootstrap payload key 4 violates {violated_bound}"
            ))
        })?;
        entries.push((
            CborValue::Integer(4.into()),
            CborValue::Text(payload.written_at.clone()),
        ));
    }

    // Tag 5: effective drive hardware compression mode. Parity-protected
    // writers must record `false`; readers refuse parity geometry if this is
    // ever `true`.
    entries.push((
        CborValue::Integer(5.into()),
        CborValue::Bool(payload.drive_compression),
    ));

    let payload_cbor = CborValue::Map(entries);
    let mut buf = Vec::new();
    ciborium::into_writer(&payload_cbor, &mut buf)
        .map_err(|e| ParityError::BootstrapParse(format!("CBOR encode failed: {e}")))?;
    Ok(buf)
}

#[derive(Debug)]
struct DecodedBootstrapCbor {
    scheme_record: Option<ParitySchemeRecord>,
    filemark_map_digest: Option<FilemarkMapDigest>,
    written_by_version: String,
    written_at: String,
    drive_compression: bool,
}

fn decode_cbor_payload(
    bytes: &[u8],
    no_parity_flag: bool,
) -> Result<DecodedBootstrapCbor, ParityError> {
    let value: CborValue = ciborium::from_reader(bytes)
        .map_err(|e| ParityError::BootstrapParse(format!("CBOR decode failed: {e}")))?;
    let map = match value {
        CborValue::Map(m) => m,
        _ => {
            return Err(ParityError::BootstrapParse(
                "CBOR payload root is not a map".into(),
            ))
        }
    };

    let mut scheme_record: Option<ParitySchemeRecord> = None;
    let mut filemark_map_digest: Option<FilemarkMapDigest> = None;
    let mut written_by_version = String::new();
    let mut written_at = String::new();
    let mut drive_compression = false;
    let mut key_order = IntegerMapKeyTracker::default();

    for (key, value) in map {
        let key_i = key_order
            .next(key, "bootstrap payload")
            .map_err(ParityError::BootstrapParse)?;
        match key_i {
            1 => {
                scheme_record = Some(decode_scheme_record(value, no_parity_flag)?);
            }
            2 => {
                filemark_map_digest = Some(decode_filemark_map_digest(value)?);
            }
            3 => {
                if let CborValue::Text(s) = value {
                    match validate_writer_version(&s) {
                        Ok(()) => written_by_version = s,
                        Err(violated_bound) => log_ignored_diagnostic_text(
                            "bootstrap payload",
                            3,
                            "writer_version",
                            violated_bound,
                        ),
                    }
                }
            }
            4 => {
                if let CborValue::Text(s) = value {
                    match validate_write_timestamp(&s) {
                        Ok(()) => written_at = s,
                        Err(violated_bound) => log_ignored_diagnostic_text(
                            "bootstrap payload",
                            4,
                            "write_timestamp",
                            violated_bound,
                        ),
                    }
                }
            }
            5 => match value {
                CborValue::Bool(compression) => {
                    drive_compression = compression;
                }
                _ => {
                    return Err(ParityError::BootstrapParse(
                        "drive_compression must be a bool".into(),
                    ))
                }
            },
            LEGACY_INLINE_DIRECTORY_KEY
            | LEGACY_PARITY_MAP_REFERENCE_KEY
            | LEGACY_OBJECT_ROWS_KEY => {
                return Err(ParityError::BootstrapParse(format!(
                    "schema-major 2 Bootstrap forbids legacy payload key {key_i}"
                )))
            }
            _ => {
                // Forward-compatible: ignore unknown integer
                // keys from newer minor versions.
            }
        }
    }

    if !no_parity_flag && drive_compression {
        return Err(ParityError::DriveCompressionEnabled);
    }

    Ok(DecodedBootstrapCbor {
        scheme_record,
        filemark_map_digest,
        written_by_version,
        written_at,
        drive_compression,
    })
}

fn encode_filemark_map_digest(digest: &FilemarkMapDigest) -> CborValue {
    CborValue::Map(vec![
        (
            CborValue::Integer(1.into()),
            CborValue::Bytes(digest.map_sha256.to_vec()),
        ),
        (
            CborValue::Integer(2.into()),
            CborValue::Integer(digest.tape_file_count.into()),
        ),
        (
            CborValue::Integer(3.into()),
            CborValue::Integer(digest.map_total_data_ordinals.into()),
        ),
        (
            CborValue::Integer(4.into()),
            CborValue::Integer(digest.highest_protected_ordinal.into()),
        ),
        (
            CborValue::Integer(5.into()),
            CborValue::Bool(digest.covers_complete_map),
        ),
    ])
}

fn decode_filemark_map_digest(value: CborValue) -> Result<FilemarkMapDigest, ParityError> {
    let map = match value {
        CborValue::Map(m) => m,
        _ => {
            return Err(ParityError::BootstrapParse(
                "filemark map digest is not a map".into(),
            ))
        }
    };

    let mut map_sha256: Option<[u8; 32]> = None;
    let mut tape_file_count: Option<u64> = None;
    let mut map_total_data_ordinals: Option<u64> = None;
    let mut highest_protected_ordinal: Option<u64> = None;
    let mut covers_complete_map: Option<bool> = None;
    let mut key_order = IntegerMapKeyTracker::default();

    for (key, value) in map {
        let key_i = key_order
            .next(key, "filemark map digest")
            .map_err(ParityError::BootstrapParse)?;
        match (key_i, value) {
            (1, CborValue::Bytes(bytes)) => {
                map_sha256 = Some(bytes.try_into().map_err(|bytes: Vec<u8>| {
                    ParityError::BootstrapParse(format!(
                        "filemark map digest sha256 has length {}, expected 32",
                        bytes.len()
                    ))
                })?);
            }
            (2, CborValue::Integer(i)) => tape_file_count = Some(int_to_u64(i, "tape_file_count")?),
            (3, CborValue::Integer(i)) => {
                map_total_data_ordinals = Some(int_to_u64(i, "map_total_data_ordinals")?)
            }
            (4, CborValue::Integer(i)) => {
                highest_protected_ordinal = Some(int_to_u64(i, "highest_protected_ordinal")?)
            }
            (5, CborValue::Bool(v)) => covers_complete_map = Some(v),
            _ => {}
        }
    }

    Ok(FilemarkMapDigest {
        map_sha256: map_sha256.ok_or_else(|| {
            ParityError::BootstrapParse("filemark map digest missing sha256".into())
        })?,
        tape_file_count: tape_file_count.ok_or_else(|| {
            ParityError::BootstrapParse("filemark map digest missing tape_file_count".into())
        })?,
        map_total_data_ordinals: map_total_data_ordinals.ok_or_else(|| {
            ParityError::BootstrapParse(
                "filemark map digest missing map_total_data_ordinals".into(),
            )
        })?,
        highest_protected_ordinal: highest_protected_ordinal.ok_or_else(|| {
            ParityError::BootstrapParse(
                "filemark map digest missing highest_protected_ordinal".into(),
            )
        })?,
        covers_complete_map: covers_complete_map.ok_or_else(|| {
            ParityError::BootstrapParse("filemark map digest missing completeness flag".into())
        })?,
    })
}

fn decode_scheme_record(
    value: CborValue,
    no_parity_flag: bool,
) -> Result<ParitySchemeRecord, ParityError> {
    let map = match value {
        CborValue::Map(m) => m,
        _ => {
            return Err(ParityError::BootstrapParse(
                "scheme record is not a map".into(),
            ))
        }
    };

    let mut id: Option<String> = None;
    let mut k: Option<u16> = None;
    let mut m: Option<u16> = None;
    let mut s: Option<u32> = None;
    let mut key_order = IntegerMapKeyTracker::default();

    for (key, value) in map {
        let key_i = key_order
            .next(key, "scheme record")
            .map_err(ParityError::BootstrapParse)?;
        match (key_i, value) {
            (1, CborValue::Text(t)) => id = Some(t),
            (2, CborValue::Integer(i)) => k = Some(int_to_u16(i, "data_blocks_per_stripe")?),
            (3, CborValue::Integer(i)) => m = Some(int_to_u16(i, "parity_blocks_per_stripe")?),
            (4, CborValue::Integer(i)) => s = Some(int_to_u32(i, "stripes_per_neighborhood")?),
            _ => {}
        }
    }

    Ok(ParitySchemeRecord {
        id: id.ok_or_else(|| ParityError::BootstrapParse("scheme record missing id".into()))?,
        data_blocks_per_stripe: k
            .ok_or_else(|| ParityError::BootstrapParse("scheme record missing k".into()))?,
        parity_blocks_per_stripe: m
            .ok_or_else(|| ParityError::BootstrapParse("scheme record missing m".into()))?,
        stripes_per_neighborhood: s
            .ok_or_else(|| ParityError::BootstrapParse("scheme record missing S".into()))?,
        no_parity_flag,
    })
}

fn int_to_u16(i: ciborium::value::Integer, field: &str) -> Result<u16, ParityError> {
    let v: i128 = i.into();
    u16::try_from(v)
        .map_err(|_| ParityError::BootstrapParse(format!("{field}: value {v} out of u16 range")))
}

fn int_to_u32(i: ciborium::value::Integer, field: &str) -> Result<u32, ParityError> {
    let v: i128 = i.into();
    u32::try_from(v)
        .map_err(|_| ParityError::BootstrapParse(format!("{field}: value {v} out of u32 range")))
}

fn int_to_u64(i: ciborium::value::Integer, field: &str) -> Result<u64, ParityError> {
    let v: i128 = i.into();
    u64::try_from(v)
        .map_err(|_| ParityError::BootstrapParse(format!("{field}: value {v} out of u64 range")))
}

// ====================================================================
// Tests
// ====================================================================

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;
    use std::process::Command;

    use super::*;
    use crate::parity_map::parse_parity_map_tape_file;

    const PUBLICATION_POSITIVE_PREFIX: &str = "rem-parity-1/positive/";

    fn publication_archive_path() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../specs/publication/remanence-test-vectors.tar")
    }

    fn publication_archive_members() -> Vec<String> {
        let output = Command::new("tar")
            .arg("-tf")
            .arg(publication_archive_path())
            .output()
            .expect("tar is required to list the pinned publication archive");
        assert!(
            output.status.success(),
            "tar must list the pinned publication archive: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("publication archive member names are UTF-8")
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn read_publication_archive_member(member: &str) -> Vec<u8> {
        let output = Command::new("tar")
            .arg("-xOf")
            .arg(publication_archive_path())
            .arg(member)
            .output()
            .expect("tar is required to read the pinned publication archive");
        assert!(
            output.status.success(),
            "tar must read pinned publication member {member}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn sample_digest() -> FilemarkMapDigest {
        sole_bot_filemark_map_digest().expect("canonical sole-BOT digest")
    }

    /// Convert a test fixture's on-tape block size at the host allocation boundary.
    fn test_block_size(block_size_bytes: u32) -> usize {
        usize::try_from(block_size_bytes).expect("test block size fits host address space")
    }

    fn sample_payload() -> BootstrapPayload {
        BootstrapPayload {
            scheme: Some(ParitySchemeRecord {
                id: "rs-cauchy-gf256-v1".to_string(),
                data_blocks_per_stripe: 128,
                parity_blocks_per_stripe: 4,
                stripes_per_neighborhood: 128,
                no_parity_flag: false,
            }),
            no_parity_flag: false,
            filemark_map_digest: Some(sample_digest()),
            tape_uuid: [
                0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09,
                0x0A, 0x0B,
            ],
            written_by_version: "0.0.1".to_string(),
            written_at: "2026-05-18T12:00:00Z".to_string(),
            sequence: 0,
            block_size_bytes: 1_048_576,
            drive_compression: false,
        }
    }

    fn encode_bootstrap_block_unchecked_for_test(payload: &BootstrapPayload) -> Vec<u8> {
        let block_size = test_block_size(payload.block_size_bytes);
        let cbor_bytes = encode_cbor_payload(payload).expect("payload CBOR encodes");
        let payload_len_u32: u32 = cbor_bytes.len().try_into().expect("payload length fits");
        let total_len = BOOTSTRAP_HEADER_LEN + cbor_bytes.len() + BOOTSTRAP_PAYLOAD_CRC_LEN;
        assert!(total_len <= block_size);

        let mut buf = vec![0u8; block_size];
        let flags = if payload.no_parity_flag {
            FLAG_NO_PARITY
        } else {
            0
        };
        buf[0..8].copy_from_slice(&BOOTSTRAP_MAGIC);
        buf[8..10].copy_from_slice(&BOOTSTRAP_SCHEMA_MAJOR.to_be_bytes());
        buf[10..12].copy_from_slice(&BOOTSTRAP_SCHEMA_MINOR.to_be_bytes());
        buf[12..16].copy_from_slice(&flags.to_be_bytes());
        buf[16..32].copy_from_slice(&payload.tape_uuid);
        buf[32..36].copy_from_slice(&payload.block_size_bytes.to_be_bytes());
        buf[36..44].copy_from_slice(&payload.sequence.to_be_bytes());
        buf[44..48].copy_from_slice(&payload_len_u32.to_le_bytes());
        let crc_header = crc64_xz(&buf[0..BOOTSTRAP_HEADER_CRC_OFFSET]);
        buf[48..56].copy_from_slice(&crc_header.to_le_bytes());

        let cbor_end = BOOTSTRAP_HEADER_LEN + cbor_bytes.len();
        buf[BOOTSTRAP_HEADER_LEN..cbor_end].copy_from_slice(&cbor_bytes);
        let crc_payload = crc64_xz(&cbor_bytes);
        buf[cbor_end..cbor_end + BOOTSTRAP_PAYLOAD_CRC_LEN]
            .copy_from_slice(&crc_payload.to_le_bytes());
        buf
    }

    fn transpose_first_two_cbor_map_entries(value: CborValue) -> CborValue {
        let CborValue::Map(mut entries) = value else {
            panic!("test value must be a CBOR map");
        };
        assert!(entries.len() >= 2, "test map needs two entries");
        entries.swap(0, 1);
        CborValue::Map(entries)
    }

    fn append_unknown_cbor_map_key(value: CborValue) -> CborValue {
        let CborValue::Map(mut entries) = value else {
            panic!("test value must be a CBOR map");
        };
        entries.push((CborValue::Integer(99.into()), CborValue::Null));
        CborValue::Map(entries)
    }

    fn decode_bootstrap_payload_value(
        value: &CborValue,
    ) -> Result<DecodedBootstrapCbor, ParityError> {
        let mut bytes = Vec::new();
        ciborium::into_writer(value, &mut bytes).expect("test CBOR value encodes");
        decode_cbor_payload(&bytes, false)
    }

    fn replace_bootstrap_text_key(block: &mut [u8], key: i128, replacement: &str) {
        let old_cbor_len = usize::try_from(u32::from_le_bytes(block[44..48].try_into().unwrap()))
            .expect("CBOR length fits host address space");
        let old_cbor = &block[BOOTSTRAP_HEADER_LEN..BOOTSTRAP_HEADER_LEN + old_cbor_len];
        let mut value: CborValue = ciborium::from_reader(old_cbor).expect("bootstrap CBOR decodes");
        let CborValue::Map(entries) = &mut value else {
            panic!("bootstrap CBOR must be a map");
        };
        let entry = entries
            .iter_mut()
            .find(|(candidate, _)| {
                matches!(candidate, CborValue::Integer(value) if i128::from(*value) == key)
            })
            .unwrap_or_else(|| panic!("bootstrap CBOR must contain key {key}"));
        entry.1 = CborValue::Text(replacement.to_string());

        let mut new_cbor = Vec::new();
        ciborium::into_writer(&value, &mut new_cbor).expect("modified bootstrap CBOR encodes");
        let new_cbor_len: u32 = new_cbor.len().try_into().expect("CBOR length fits u32");
        let cbor_end = BOOTSTRAP_HEADER_LEN + new_cbor.len();
        let crc_end = cbor_end + BOOTSTRAP_PAYLOAD_CRC_LEN;
        assert!(crc_end <= block.len(), "modified bootstrap fits block");

        block[44..48].copy_from_slice(&new_cbor_len.to_le_bytes());
        let header_crc = crc64_xz(&block[..BOOTSTRAP_HEADER_CRC_OFFSET]);
        block[48..56].copy_from_slice(&header_crc.to_le_bytes());
        block[BOOTSTRAP_HEADER_LEN..cbor_end].copy_from_slice(&new_cbor);
        let payload_crc = crc64_xz(&new_cbor);
        block[cbor_end..crc_end].copy_from_slice(&payload_crc.to_le_bytes());
        block[crc_end..].fill(0);
    }

    fn assert_bootstrap_key_order_error(error: ParityError, key: i128, previous: i128) {
        let ParityError::BootstrapParse(message) = error else {
            panic!("expected bootstrap parse error, got {error:?}");
        };
        assert!(message.contains(&format!("key {key}")), "{message}");
        assert!(
            message.contains(&format!("after key {previous}")),
            "{message}"
        );
    }

    #[test]
    fn roundtrip_default_payload() {
        // Sample payload's block_size_bytes is 1 MiB; the
        // writer now insists buf.len() >= block_size_bytes.
        let mut buf = vec![0u8; 1_048_576];
        let payload = sample_payload();
        let written = write_bootstrap_block(&payload, &mut buf).expect("write ok");
        assert!(written > BOOTSTRAP_HEADER_LEN);
        let parsed = parse_bootstrap_block(&buf[..]).expect("parse ok");
        assert_eq!(parsed, payload);
    }

    #[test]
    fn bootstrap_writer_rejects_invalid_diagnostic_text() {
        for invalid_version in ["v".repeat(129), "writer\x1b[2J".to_string()] {
            let mut payload = sample_payload();
            payload.written_by_version = invalid_version;
            let mut block = vec![0u8; test_block_size(payload.block_size_bytes)];
            let error = write_bootstrap_block(&payload, &mut block)
                .expect_err("invalid bootstrap key 3 must not be written");
            assert!(
                matches!(&error, ParityError::BootstrapParse(message) if message.contains("key 3")),
                "unexpected error: {error:?}"
            );
        }

        for invalid_timestamp in ["x".repeat(65), "2026-01-01\x1b".to_string()] {
            let mut payload = sample_payload();
            payload.written_at = invalid_timestamp;
            let mut block = vec![0u8; test_block_size(payload.block_size_bytes)];
            let error = write_bootstrap_block(&payload, &mut block)
                .expect_err("invalid bootstrap key 4 must not be written");
            assert!(
                matches!(&error, ParityError::BootstrapParse(message) if message.contains("key 4")),
                "unexpected error: {error:?}"
            );
        }
    }

    #[test]
    fn bootstrap_writer_requires_version_but_reader_tolerates_its_absence() {
        let mut payload = sample_payload();
        payload.written_by_version.clear();
        let mut block = vec![0u8; test_block_size(payload.block_size_bytes)];
        let error = write_bootstrap_block(&payload, &mut block)
            .expect_err("writer must emit required key 3");
        assert!(matches!(
            error,
            ParityError::Invariant(message) if message.contains("written_by_version key 3")
        ));

        let block = encode_bootstrap_block_unchecked_for_test(&payload);
        let parsed = parse_bootstrap_block(&block)
            .expect("reader remains tolerant of an absent diagnostic key 3");
        assert!(parsed.written_by_version.is_empty());
    }

    #[test]
    fn bootstrap_reader_treats_invalid_writer_version_as_absent() {
        let payload = sample_payload();
        let mut block = vec![0u8; test_block_size(payload.block_size_bytes)];
        write_bootstrap_block(&payload, &mut block).expect("valid bootstrap writes");
        replace_bootstrap_text_key(&mut block, 3, "writer\x1b[2J");

        let decoded = parse_bootstrap_block(&block)
            .expect("invalid diagnostic key 3 must not invalidate bootstrap");
        let mut expected = payload;
        expected.written_by_version.clear();
        assert_eq!(decoded, expected);
    }

    #[test]
    fn bootstrap_reader_treats_invalid_write_timestamp_as_absent() {
        let payload = sample_payload();
        let mut block = vec![0u8; test_block_size(payload.block_size_bytes)];
        write_bootstrap_block(&payload, &mut block).expect("valid bootstrap writes");
        replace_bootstrap_text_key(&mut block, 4, "not a timestamp");

        let decoded = parse_bootstrap_block(&block)
            .expect("invalid diagnostic key 4 must not invalidate bootstrap");
        let mut expected = payload;
        expected.written_at.clear();
        assert_eq!(decoded, expected);
    }

    #[test]
    fn bootstrap_diagnostic_rendering_escapes_terminal_controls() {
        let mut payload = sample_payload();
        payload.written_by_version = "writer\x1b[2J".to_string();

        let rendered = payload.escaped_written_by_version();
        assert_eq!(rendered, "writer\\x1b[2J");
        assert!(!rendered.as_bytes().contains(&0x1b));
    }

    #[test]
    fn legacy_published_narrow_frames_fail_closed() {
        let members = publication_archive_members();
        let positive_vectors = members
            .iter()
            .filter_map(|member| {
                member
                    .strip_prefix(PUBLICATION_POSITIVE_PREFIX)
                    .and_then(|relative| relative.split('/').next())
                    .filter(|name| !name.is_empty())
                    .map(str::to_string)
            })
            .collect::<BTreeSet<_>>();
        assert!(
            !positive_vectors.is_empty(),
            "publication archive must contain positive REM-PARITY vectors"
        );

        let bootstrap_members = members
            .iter()
            .filter(|member| {
                member.starts_with(PUBLICATION_POSITIVE_PREFIX) && member.ends_with("bootstrap.bin")
            })
            .collect::<Vec<_>>();
        let mut vector_geometry = BTreeMap::new();
        for member in bootstrap_members {
            let bytes = read_publication_archive_member(member);
            let vector = member
                .strip_prefix(PUBLICATION_POSITIVE_PREFIX)
                .and_then(|relative| relative.split('/').next())
                .expect("positive bootstrap member has a vector directory");
            let tape_uuid: [u8; 16] = bytes[16..32].try_into().expect("legacy UUID field exists");
            let block_size = u32::from_be_bytes(
                bytes[32..36]
                    .try_into()
                    .expect("legacy block-size field exists"),
            );
            vector_geometry.insert(vector.to_string(), (tape_uuid, block_size));
            let error = parse_bootstrap_block(&bytes)
                .expect_err("legacy narrow bootstrap authority must fail closed");
            assert!(matches!(&error, ParityError::BootstrapParse(_)), "{error}");
        }
        assert_eq!(
            vector_geometry.keys().cloned().collect::<BTreeSet<_>>(),
            positive_vectors,
            "every legacy positive vector must contain a rejected narrow bootstrap"
        );

        let parity_map_members = members
            .iter()
            .filter(|member| {
                member.starts_with(PUBLICATION_POSITIVE_PREFIX)
                    && member.ends_with("parity-map.bin")
            })
            .collect::<Vec<_>>();
        assert!(
            !parity_map_members.is_empty(),
            "publication archive must exercise a parity-map CBOR payload"
        );
        for member in parity_map_members {
            let vector = member
                .strip_prefix(PUBLICATION_POSITIVE_PREFIX)
                .and_then(|relative| relative.split('/').next())
                .expect("positive parity-map member has a vector directory");
            let (tape_uuid, block_size) = vector_geometry
                .get(vector)
                .expect("parity-map vector has decoded bootstrap geometry");
            let bytes = read_publication_archive_member(member);
            let block_size = usize::try_from(*block_size).expect("block size fits usize");
            assert_eq!(
                bytes.len() % block_size,
                0,
                "pinned {member} contains whole fixed blocks"
            );
            let blocks = bytes
                .chunks_exact(block_size)
                .map(<[u8]>::to_vec)
                .collect::<Vec<_>>();
            let error = parse_parity_map_tape_file(&blocks, tape_uuid)
                .expect_err("legacy narrow parity-map authority must fail closed");
            assert!(error.to_string().contains("footer version"), "{error}");
        }
    }

    #[test]
    fn bootstrap_payload_map_enforces_order_and_ignores_ordered_unknown_key() {
        let canonical_bytes = encode_cbor_payload(&sample_payload()).expect("payload encodes");
        let canonical: CborValue =
            ciborium::from_reader(canonical_bytes.as_slice()).expect("payload value decodes");

        decode_bootstrap_payload_value(&canonical).expect("canonical payload map decodes");

        let transposed = transpose_first_two_cbor_map_entries(canonical.clone());
        let error = decode_bootstrap_payload_value(&transposed)
            .expect_err("transposed payload keys must reject");
        assert_bootstrap_key_order_error(error, 1, 2);

        let extended = append_unknown_cbor_map_key(canonical);
        decode_bootstrap_payload_value(&extended).expect("ordered unknown payload key is ignored");
    }

    #[test]
    fn scheme_record_map_enforces_order_and_ignores_ordered_unknown_key() {
        let canonical = CborValue::Map(vec![
            (
                CborValue::Integer(1.into()),
                CborValue::Text("rs-cauchy-gf256-v1".to_string()),
            ),
            (CborValue::Integer(2.into()), CborValue::Integer(2.into())),
            (CborValue::Integer(3.into()), CborValue::Integer(1.into())),
            (CborValue::Integer(4.into()), CborValue::Integer(2.into())),
        ]);

        decode_scheme_record(canonical.clone(), false).expect("canonical scheme map decodes");

        let transposed = transpose_first_two_cbor_map_entries(canonical.clone());
        let error = decode_scheme_record(transposed, false)
            .expect_err("transposed scheme keys must reject");
        assert_bootstrap_key_order_error(error, 1, 2);

        let extended = append_unknown_cbor_map_key(canonical);
        decode_scheme_record(extended, false).expect("ordered unknown scheme key is ignored");
    }

    #[test]
    fn filemark_digest_map_enforces_order_and_ignores_ordered_unknown_key() {
        let digest = sample_payload()
            .filemark_map_digest
            .expect("sample payload has a digest");
        let canonical = encode_filemark_map_digest(&digest);

        assert_eq!(
            decode_filemark_map_digest(canonical.clone()).expect("canonical digest map decodes"),
            digest
        );

        let transposed = transpose_first_two_cbor_map_entries(canonical.clone());
        let error =
            decode_filemark_map_digest(transposed).expect_err("transposed digest keys must reject");
        assert_bootstrap_key_order_error(error, 1, 2);

        let extended = append_unknown_cbor_map_key(canonical);
        assert_eq!(
            decode_filemark_map_digest(extended).expect("ordered unknown digest key is ignored"),
            digest
        );
    }

    #[test]
    fn filemark_digest_tape_file_count_preserves_the_full_u64_range() {
        for tape_file_count in [u64::from(u32::MAX) + 1, u64::MAX] {
            let digest = FilemarkMapDigest {
                tape_file_count,
                ..sample_digest()
            };
            let mut cbor = Vec::new();
            ciborium::into_writer(&encode_filemark_map_digest(&digest), &mut cbor)
                .expect("digest CBOR encodes");
            let cbor_value: CborValue =
                ciborium::from_reader(cbor.as_slice()).expect("digest CBOR decodes");

            assert_eq!(
                decode_filemark_map_digest(cbor_value).expect("u64 tape-file count decodes"),
                digest
            );
        }
    }

    #[test]
    fn filemark_digest_rejects_negative_tape_file_count() {
        let mut digest = encode_filemark_map_digest(&sample_digest());
        let CborValue::Map(entries) = &mut digest else {
            panic!("filemark digest encodes as a map");
        };
        let tape_file_count = entries
            .iter_mut()
            .find(|(key, _)| matches!(key, CborValue::Integer(value) if i128::from(*value) == 2))
            .expect("digest has tape-file-count key");
        tape_file_count.1 = CborValue::Integer((-1_i64).into());
        let mut cbor = Vec::new();
        ciborium::into_writer(&digest, &mut cbor).expect("negative digest CBOR encodes");
        let cbor_value: CborValue =
            ciborium::from_reader(cbor.as_slice()).expect("negative digest CBOR decodes");

        let error = decode_filemark_map_digest(cbor_value)
            .expect_err("negative tape-file count must be rejected");
        assert!(
            matches!(error, ParityError::BootstrapParse(ref message) if message.contains("tape_file_count") && message.contains("out of u64 range")),
            "{error}"
        );
    }

    #[test]
    fn roundtrip_records_drive_compression_false() {
        let payload = sample_payload();
        let mut buf = vec![0u8; 1_048_576];

        write_bootstrap_block(&payload, &mut buf).expect("write ok");
        let parsed = parse_bootstrap_block(&buf).expect("parse ok");

        assert!(!parsed.drive_compression);
    }

    #[test]
    fn writer_rejects_parity_payload_with_drive_compression_enabled() {
        let mut payload = sample_payload();
        payload.drive_compression = true;
        let mut buf = vec![0u8; 1_048_576];

        let err = write_bootstrap_block(&payload, &mut buf).unwrap_err();

        assert!(matches!(err, ParityError::DriveCompressionEnabled));
    }

    #[test]
    fn parser_rejects_parity_bootstrap_that_records_drive_compression_enabled() {
        let mut payload = sample_payload();
        payload.drive_compression = true;
        let block = encode_bootstrap_block_unchecked_for_test(&payload);

        let err = parse_bootstrap_block(&block).unwrap_err();

        assert!(matches!(err, ParityError::DriveCompressionEnabled));
    }

    #[test]
    fn no_parity_bootstrap_may_record_drive_compression_enabled() {
        let mut payload = sample_payload();
        payload.no_parity_flag = true;
        payload.scheme = None;
        payload.filemark_map_digest = None;
        payload.drive_compression = true;
        let mut buf = vec![0u8; 1_048_576];

        write_bootstrap_block(&payload, &mut buf).expect("no-parity bootstrap writes");
        let parsed = parse_bootstrap_block(&buf).expect("no-parity bootstrap parses");

        assert!(parsed.no_parity_flag);
        assert!(parsed.drive_compression);
    }

    #[test]
    fn schema_major_two_rejects_legacy_extended_payload_keys() {
        for key in [20, 21, 30] {
            let encoded = encode_cbor_payload(&sample_payload()).expect("encode base payload");
            let mut value: CborValue =
                ciborium::from_reader(encoded.as_slice()).expect("decode payload value");
            let CborValue::Map(ref mut entries) = value else {
                panic!("payload map");
            };
            entries.push((CborValue::Integer(key.into()), CborValue::Null));
            let mut bytes = Vec::new();
            ciborium::into_writer(&value, &mut bytes).expect("encode forbidden-key payload");
            let error =
                decode_cbor_payload(&bytes, false).expect_err("legacy key must fail closed");
            assert!(error
                .to_string()
                .contains(&format!("forbids legacy payload key {key}")));
        }
    }

    #[test]
    fn schema_major_two_writer_rejects_nonzero_sequence() {
        let mut payload = sample_payload();
        payload.sequence = 1;
        let mut block = vec![0; 1_048_576];
        let error = write_bootstrap_block(&payload, &mut block).expect_err("non-BOT copy rejected");
        assert!(error.to_string().contains("sequence-0 BOT Bootstrap"));
    }

    #[test]
    fn header_offsets_and_crc64_ranges_match_v2_table() {
        let mut buf = vec![0u8; 1_048_576];
        let payload = sample_payload();
        let written = write_bootstrap_block(&payload, &mut buf).expect("write ok");

        assert_eq!(&buf[0x00..0x08], &BOOTSTRAP_MAGIC);
        assert_eq!(
            u16::from_be_bytes(buf[0x08..0x0A].try_into().unwrap()),
            BOOTSTRAP_SCHEMA_MAJOR
        );
        assert_eq!(
            u16::from_be_bytes(buf[0x0A..0x0C].try_into().unwrap()),
            BOOTSTRAP_SCHEMA_MINOR
        );
        assert_eq!(u32::from_be_bytes(buf[0x0C..0x10].try_into().unwrap()), 0);
        assert_eq!(&buf[0x10..0x20], &payload.tape_uuid);
        assert_eq!(
            u32::from_be_bytes(buf[0x20..0x24].try_into().unwrap()),
            payload.block_size_bytes
        );
        assert_eq!(
            u64::from_be_bytes(buf[0x24..0x2C].try_into().unwrap()),
            payload.sequence
        );

        let payload_len = usize::try_from(u32::from_le_bytes(buf[0x2C..0x30].try_into().unwrap()))
            .expect("CBOR length fits host address space");
        let stored_header_crc = u64::from_le_bytes(buf[0x30..0x38].try_into().unwrap());
        assert_eq!(stored_header_crc, crc64_xz(&buf[0x00..0x30]));

        let payload_start = BOOTSTRAP_HEADER_LEN;
        let payload_end = payload_start + payload_len;
        assert_eq!(
            u64::from_le_bytes(buf[payload_end..payload_end + 8].try_into().unwrap()),
            crc64_xz(&buf[payload_start..payload_end])
        );
        assert_eq!(written, payload_end + BOOTSTRAP_PAYLOAD_CRC_LEN);
    }

    #[test]
    fn roundtrip_padded_buffer_still_parses() {
        // Real tape blocks are 1 MiB; the parser must tolerate
        // the trailing zeros without confusing them for CBOR.
        let payload = sample_payload();
        let mut buf = vec![0u8; 1_048_576];
        write_bootstrap_block(&payload, &mut buf).expect("write ok");
        let parsed = parse_bootstrap_block(&buf[..]).expect("parse ok");
        assert_eq!(parsed, payload);
    }

    #[test]
    fn roundtrip_no_parity_flag_with_scheme_record() {
        // Bootstrap declares no_parity but still carries the
        // scheme record (informational). Both must round-trip.
        let mut payload = sample_payload();
        payload.no_parity_flag = true;
        payload.scheme.as_mut().unwrap().no_parity_flag = true;
        let mut buf = vec![0u8; 1_048_576];
        write_bootstrap_block(&payload, &mut buf).expect("write ok");
        let parsed = parse_bootstrap_block(&buf[..]).expect("parse ok");
        assert!(parsed.no_parity_flag);
        assert!(parsed.scheme.as_ref().unwrap().no_parity_flag);
    }

    #[test]
    fn roundtrip_no_parity_bootstrap_without_scheme_record() {
        // Codex idref=794a16ac Medium: a no-parity bootstrap
        // may omit the scheme record entirely per design §5.6.
        let mut payload = sample_payload();
        payload.no_parity_flag = true;
        payload.scheme = None;
        payload.filemark_map_digest = None;
        let mut buf = vec![0u8; 1_048_576];
        write_bootstrap_block(&payload, &mut buf).expect("write ok");
        let parsed = parse_bootstrap_block(&buf[..]).expect("parse ok");
        assert!(parsed.no_parity_flag);
        assert!(parsed.scheme.is_none());
        assert!(parsed.filemark_map_digest.is_none());
    }

    #[test]
    fn writer_rejects_scheme_none_without_no_parity_flag() {
        // Codex idref=99c40750 Low: the writer must enforce
        // the BootstrapPayload invariants — emitting a
        // scheme=None payload with no_parity_flag=false would
        // produce a frame the parser rejects.
        let mut payload = sample_payload();
        payload.scheme = None;
        // no_parity_flag stays false → writer must refuse.
        let mut buf = vec![0u8; 1_048_576];
        let err = write_bootstrap_block(&payload, &mut buf).unwrap_err();
        match err {
            ParityError::Invariant(msg) => {
                assert!(msg.contains("no_parity_flag = true"), "{msg}");
            }
            other => panic!("expected Invariant, got {other:?}"),
        }
    }

    #[test]
    fn writer_rejects_mismatched_no_parity_flags() {
        // scheme.no_parity_flag must match payload.no_parity_flag.
        let mut payload = sample_payload();
        payload.no_parity_flag = false;
        payload.scheme.as_mut().unwrap().no_parity_flag = true;
        let mut buf = vec![0u8; 1_048_576];
        let err = write_bootstrap_block(&payload, &mut buf).unwrap_err();
        match err {
            ParityError::Invariant(msg) => {
                assert!(msg.contains("scheme.no_parity_flag"), "{msg}");
            }
            other => panic!("expected Invariant, got {other:?}"),
        }
    }

    #[test]
    fn writer_rejects_parity_bootstrap_without_map_digest() {
        let mut payload = sample_payload();
        payload.filemark_map_digest = None;
        let mut buf = vec![0u8; 1_048_576];
        let err = write_bootstrap_block(&payload, &mut buf).unwrap_err();
        match err {
            ParityError::Invariant(msg) => {
                assert!(msg.contains("filemark_map_digest"), "{msg}");
            }
            other => panic!("expected Invariant, got {other:?}"),
        }
    }

    #[test]
    fn parser_rejects_intentionally_invalid_frame_without_scheme_and_no_parity_flag() {
        // The writer enforces the invariant now (above), but we
        // also want defense-in-depth at parse time. Build an
        // invalid frame by hand and confirm parse rejects.
        // Construct payload that would (with the invariant
        // check disabled) produce a no-scheme + flag=false
        // frame: write a "minimal no-parity" frame, then
        // recompute the header CRC after clearing the
        // FLAG_NO_PARITY bit.
        let mut payload = sample_payload();
        payload.scheme = None;
        payload.no_parity_flag = true;
        payload.filemark_map_digest = None;
        let mut buf = vec![0u8; 1_048_576];
        write_bootstrap_block(&payload, &mut buf).expect("write minimal no-parity frame");
        // Now clear the FLAG_NO_PARITY bit in the header and
        // recompute the header CRC. The parse will reject
        // because the CBOR has no scheme record but the flag
        // is now false.
        buf[12..16].copy_from_slice(&0u32.to_be_bytes());
        let new_crc = crc64_xz(&buf[0..BOOTSTRAP_HEADER_CRC_OFFSET]);
        buf[48..56].copy_from_slice(&new_crc.to_le_bytes());
        let err = parse_bootstrap_block(&buf[..]).unwrap_err();
        match err {
            ParityError::BootstrapParse(msg) => {
                assert!(msg.contains("missing scheme record"), "{msg}");
            }
            other => panic!("expected BootstrapParse, got {other:?}"),
        }
    }

    #[test]
    fn writer_and_parser_reject_noncanonical_bot_map_digest() {
        let mut wrong_complete = sample_digest();
        wrong_complete.covers_complete_map = true;
        let mut wrong_count = sample_digest();
        wrong_count.tape_file_count = 2;
        let mut wrong_hash = sample_digest();
        wrong_hash.map_sha256[0] ^= 0x80;

        for invalid in [wrong_complete, wrong_count, wrong_hash] {
            let mut payload = sample_payload();
            payload.filemark_map_digest = Some(invalid);
            let mut buf = vec![0u8; test_block_size(payload.block_size_bytes)];
            let write_error = write_bootstrap_block(&payload, &mut buf)
                .expect_err("writer must reject noncanonical BOT key 2");
            assert!(write_error.to_string().contains("sole tape-file-0 BOT"));

            let block = encode_bootstrap_block_unchecked_for_test(&payload);
            let parse_error = parse_bootstrap_block(&block)
                .expect_err("parser must reject noncanonical BOT key 2");
            assert!(parse_error.to_string().contains("sole tape-file-0 BOT"));
        }
    }

    #[test]
    fn write_rejects_buffer_smaller_than_block_size() {
        // Codex idref=794a16ac Low: writer must reject buffers
        // shorter than block_size_bytes, not just shorter than
        // the framed payload.
        let payload = sample_payload(); // block_size_bytes = 1 MiB
        let mut tiny = vec![0u8; 1024]; // way smaller than 1 MiB
        let err = write_bootstrap_block(&payload, &mut tiny).unwrap_err();
        match err {
            ParityError::Invariant(msg) => {
                assert!(msg.contains("block_size_bytes"), "{msg}");
            }
            other => panic!("expected Invariant, got {other:?}"),
        }
    }

    #[test]
    fn write_rejects_payload_exceeding_block_size_with_typed_error() {
        let mut payload = sample_payload();
        payload.block_size_bytes = 64;
        let mut buf = vec![0u8; 64];

        let err = write_bootstrap_block(&payload, &mut buf).unwrap_err();

        match err {
            ParityError::BootstrapPayloadTooLarge {
                framed_len,
                block_size,
            } => {
                assert_eq!(block_size, 64);
                assert!(framed_len > block_size);
            }
            other => panic!("expected BootstrapPayloadTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn write_zero_fills_padding_so_stale_bytes_dont_leak() {
        // Codex idref=794a16ac Low: a reused scratch buffer
        // carrying nonzero bytes must end up zero in the
        // padding region after a write.
        let payload = sample_payload(); // block_size_bytes = 1 MiB
        let mut buf = vec![0xFFu8; 1_048_576];
        let written = write_bootstrap_block(&payload, &mut buf).expect("write ok");
        // Padding region must be zero.
        for (i, &b) in buf[written..].iter().enumerate() {
            assert_eq!(b, 0, "padding byte at offset {} is 0x{b:02x}", written + i);
        }
    }

    #[test]
    fn magic_mismatch_rejected() {
        let payload = sample_payload();
        let mut buf = vec![0u8; 1_048_576];
        write_bootstrap_block(&payload, &mut buf).expect("write ok");
        buf[0] = 0xFF; // corrupt magic
        let err = parse_bootstrap_block(&buf[..]).unwrap_err();
        match err {
            ParityError::BootstrapParse(msg) => assert!(msg.contains("magic")),
            other => panic!("expected BootstrapParse, got {other:?}"),
        }
    }

    #[test]
    fn header_crc_mismatch_rejected() {
        let payload = sample_payload();
        let mut buf = vec![0u8; 1_048_576];
        write_bootstrap_block(&payload, &mut buf).expect("write ok");
        buf[16] ^= 0xFF; // flip a UUID byte → header CRC mismatch
        let err = parse_bootstrap_block(&buf[..]).unwrap_err();
        match err {
            ParityError::BootstrapParse(msg) => assert!(msg.contains("header CRC")),
            other => panic!("expected BootstrapParse, got {other:?}"),
        }
    }

    #[test]
    fn corrupt_payload_length_is_caught_by_header_crc() {
        let payload = sample_payload();
        let mut buf = vec![0u8; 1_048_576];
        write_bootstrap_block(&payload, &mut buf).expect("write ok");
        buf[44] ^= 0x80; // cbor_payload_len is covered by crc64_header.
        let err = parse_bootstrap_block(&buf[..]).unwrap_err();
        match err {
            ParityError::BootstrapParse(msg) => assert!(msg.contains("header CRC"), "{msg}"),
            other => panic!("expected BootstrapParse, got {other:?}"),
        }
    }

    #[test]
    fn payload_crc_mismatch_rejected() {
        let payload = sample_payload();
        let mut buf = vec![0u8; 1_048_576];
        write_bootstrap_block(&payload, &mut buf).expect("write ok");
        // Flip a byte deep in the CBOR payload (after the
        // header). Don't touch the last 4 bytes of CBOR or the
        // CRC tail.
        buf[BOOTSTRAP_HEADER_LEN + 5] ^= 0xFF;
        let err = parse_bootstrap_block(&buf[..]).unwrap_err();
        match err {
            ParityError::BootstrapParse(msg) => assert!(msg.contains("payload CRC")),
            other => panic!("expected BootstrapParse, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_major_version_rejected() {
        let payload = sample_payload();
        let mut buf = vec![0u8; 1_048_576];
        write_bootstrap_block(&payload, &mut buf).expect("write ok");
        // Bump the major to 99.
        buf[8..10].copy_from_slice(&99u16.to_be_bytes());
        // Recompute header CRC so we hit the version check, not
        // the CRC check.
        let new_crc = crc64_xz(&buf[0..BOOTSTRAP_HEADER_CRC_OFFSET]);
        buf[48..56].copy_from_slice(&new_crc.to_le_bytes());
        let err = parse_bootstrap_block(&buf[..]).unwrap_err();
        match err {
            ParityError::BootstrapParse(msg) => assert!(msg.contains("major")),
            other => panic!("expected BootstrapParse, got {other:?}"),
        }
    }

    #[test]
    fn legacy_narrow_major_version_rejected() {
        let payload = sample_payload();
        let mut buf = vec![0u8; 1_048_576];
        write_bootstrap_block(&payload, &mut buf).expect("write ok");
        buf[8..10].copy_from_slice(&1_u16.to_be_bytes());
        let new_crc = crc64_xz(&buf[0..BOOTSTRAP_HEADER_CRC_OFFSET]);
        buf[48..56].copy_from_slice(&new_crc.to_le_bytes());

        let error = parse_bootstrap_block(&buf).expect_err("major 1 authority must fail closed");
        assert!(
            error.to_string().contains("schema major version"),
            "{error}"
        );
    }

    #[test]
    fn forward_compatible_minor_version_accepted() {
        // Writer at minor=0; reader simulating an older bootstrap
        // with minor=99 must still parse it.
        let payload = sample_payload();
        let mut buf = vec![0u8; 1_048_576];
        write_bootstrap_block(&payload, &mut buf).expect("write ok");
        buf[10..12].copy_from_slice(&99u16.to_be_bytes());
        let new_crc = crc64_xz(&buf[0..BOOTSTRAP_HEADER_CRC_OFFSET]);
        buf[48..56].copy_from_slice(&new_crc.to_le_bytes());
        let parsed = parse_bootstrap_block(&buf[..]).expect("forward-compat minor");
        assert_eq!(parsed, payload);
    }

    #[test]
    fn has_bootstrap_magic_quick_check() {
        let mut buf = vec![0u8; 16];
        buf[0..8].copy_from_slice(&BOOTSTRAP_MAGIC);
        assert!(has_bootstrap_magic(&buf));
        buf[0] = 0xFF;
        assert!(!has_bootstrap_magic(&buf));
        assert!(!has_bootstrap_magic(&[]));
        assert!(!has_bootstrap_magic(&buf[0..3]));
    }

    #[test]
    fn buffer_too_small_for_block_size_returns_error() {
        let payload = sample_payload();
        let mut tiny = vec![0u8; 10];
        let err = write_bootstrap_block(&payload, &mut tiny).unwrap_err();
        assert!(matches!(err, ParityError::Invariant(_)));
    }

    use crate::raw::{
        BlockSourceRawTapeSource, PhysicalPositionHint, RawReadOutcome, RawTapeSource,
        SpaceFilemarksOutcome,
    };
    use remanence_library::{TapeIoError, VecBlockSource};

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum RecordingRawSourceCall {
        Configure(u32),
        Locate(u64),
        ReadRecord {
            lba: u64,
            requested: usize,
            returned: usize,
        },
    }

    struct RecordingRawSource {
        blocks: Vec<Vec<u8>>,
        cursor: u64,
        calls: Vec<RecordingRawSourceCall>,
    }

    impl RecordingRawSource {
        fn new(blocks: Vec<Vec<u8>>) -> Self {
            Self {
                blocks,
                cursor: 0,
                calls: Vec::new(),
            }
        }
    }

    impl RawTapeSource for RecordingRawSource {
        fn configure_fixed_block_size(&mut self, block_size: u32) -> Result<(), ParityError> {
            if block_size == 0 {
                return Err(ParityError::Invariant("fixed block size is zero"));
            }
            self.calls
                .push(RecordingRawSourceCall::Configure(block_size));
            Ok(())
        }

        fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
            self.calls.push(RecordingRawSourceCall::Locate(hint.lba));
            self.cursor = hint.lba;
            Ok(())
        }

        fn locate_end_of_data(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            self.cursor = u64::try_from(self.blocks.len())
                .map_err(|_| ParityError::Invariant("test block count exceeds u64"))?;
            Ok(PhysicalPositionHint::new(self.cursor))
        }

        fn space_filemarks(&mut self, count: i64) -> Result<SpaceFilemarksOutcome, ParityError> {
            Ok(SpaceFilemarksOutcome {
                filemarks_spaced: count,
                position_after: PhysicalPositionHint::new(self.cursor),
                hit_end_of_data: false,
            })
        }

        fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
            let lba = self.cursor;
            let Some(block) = usize::try_from(lba)
                .ok()
                .and_then(|index| self.blocks.get(index))
            else {
                self.calls.push(RecordingRawSourceCall::ReadRecord {
                    lba,
                    requested: buf.len(),
                    returned: 0,
                });
                return Ok(RawReadOutcome::EndOfData {
                    position_after: PhysicalPositionHint::new(lba),
                });
            };

            if block.len() > buf.len() {
                self.cursor = self.cursor.saturating_add(1);
                self.calls.push(RecordingRawSourceCall::ReadRecord {
                    lba,
                    requested: buf.len(),
                    returned: 0,
                });
                return Err(ParityError::TapeIo(
                    remanence_library::TapeIoError::ReadBufferTooSmall {
                        actual: u32::try_from(block.len()).map_err(|_| {
                            ParityError::Invariant("test block length exceeds u32 host boundary")
                        })?,
                        provided: u32::try_from(buf.len()).map_err(|_| {
                            ParityError::Invariant("test buffer length exceeds u32 host boundary")
                        })?,
                    },
                ));
            }

            let returned = block.len();
            buf[..returned].copy_from_slice(block);
            self.cursor = self.cursor.saturating_add(1);
            self.calls.push(RecordingRawSourceCall::ReadRecord {
                lba,
                requested: buf.len(),
                returned,
            });
            Ok(RawReadOutcome::Block {
                bytes: returned,
                position_after: PhysicalPositionHint::new(self.cursor),
            })
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            Ok(PhysicalPositionHint::new(self.cursor))
        }
    }

    struct FailingLocateRawSource;

    impl RawTapeSource for FailingLocateRawSource {
        fn configure_fixed_block_size(&mut self, block_size: u32) -> Result<(), ParityError> {
            if block_size == 0 {
                return Err(ParityError::Invariant("fixed block size is zero"));
            }
            Ok(())
        }

        fn locate_physical(&mut self, _hint: PhysicalPositionHint) -> Result<(), ParityError> {
            Err(TapeIoError::OperationFailed("synthetic locate failure".into()).into())
        }

        fn locate_end_of_data(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            Err(TapeIoError::OperationFailed("synthetic EOD locate failure".into()).into())
        }

        fn space_filemarks(&mut self, _count: i64) -> Result<SpaceFilemarksOutcome, ParityError> {
            unreachable!("bootstrap discovery does not space filemarks")
        }

        fn read_record(&mut self, _buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
            unreachable!("locate failure prevents reads")
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            unreachable!("bootstrap discovery does not query position after locate failure")
        }
    }

    struct FailingReadRawSource;

    impl RawTapeSource for FailingReadRawSource {
        fn configure_fixed_block_size(&mut self, block_size: u32) -> Result<(), ParityError> {
            if block_size == 0 {
                return Err(ParityError::Invariant("fixed block size is zero"));
            }
            Ok(())
        }

        fn locate_physical(&mut self, _hint: PhysicalPositionHint) -> Result<(), ParityError> {
            Ok(())
        }

        fn locate_end_of_data(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            Ok(PhysicalPositionHint::new(0))
        }

        fn space_filemarks(&mut self, _count: i64) -> Result<SpaceFilemarksOutcome, ParityError> {
            unreachable!("bootstrap discovery does not space filemarks")
        }

        fn read_record(&mut self, _buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
            Err(TapeIoError::OperationFailed("synthetic read failure".into()).into())
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            unreachable!("bootstrap discovery does not query position after read failure")
        }
    }

    #[test]
    fn expected_positions_starts_at_zero() {
        let p = expected_bootstrap_positions(None);
        assert_eq!(p[0], 0);
        assert_eq!(p.len(), 1);
    }

    #[test]
    fn expected_positions_with_hint_still_names_only_bot() {
        let p = expected_bootstrap_positions(Some(1000));
        assert_eq!(p, vec![0]);
    }

    #[test]
    fn discover_finds_bootstrap_at_lba_zero() {
        let payload = sample_payload();
        let mut buf = vec![0u8; 1_048_576];
        write_bootstrap_block(&payload, &mut buf).expect("write ok");
        let blocks = vec![buf];
        let mut src = VecBlockSource::new(blocks);
        let parsed = {
            let mut raw = BlockSourceRawTapeSource::new(&mut src);
            discover_bootstrap(&mut raw, None).expect("discover ok")
        };
        assert_eq!(parsed, payload);
    }

    #[test]
    fn discover_refuses_parity_bootstrap_with_drive_compression_enabled() {
        let mut payload = sample_payload();
        payload.drive_compression = true;
        let block = encode_bootstrap_block_unchecked_for_test(&payload);
        let mut source = RecordingRawSource::new(vec![block]);

        let err = discover_bootstrap_with_block_size(&mut source, None, payload.block_size_bytes)
            .expect_err("compressed parity bootstrap must stop discovery");

        assert!(matches!(err, ParityError::DriveCompressionEnabled));
    }

    #[test]
    fn discover_candidate_fallback_finds_256k_bootstrap_after_wrong_size() {
        let mut payload = sample_payload();
        payload.block_size_bytes = 256 * 1024;
        let mut buf = vec![0u8; test_block_size(payload.block_size_bytes)];
        write_bootstrap_block(&payload, &mut buf).expect("write ok");
        let mut src = RecordingRawSource::new(vec![buf]);

        let parsed = discover_bootstrap_with_candidate_block_sizes(
            &mut src,
            None,
            &[512 * 1024, 256 * 1024],
        )
        .expect("fallback discovers 256 KiB bootstrap");

        assert_eq!(parsed, payload);
        assert_eq!(
            src.calls,
            vec![
                RecordingRawSourceCall::Configure(512 * 1024),
                RecordingRawSourceCall::Locate(0),
                RecordingRawSourceCall::ReadRecord {
                    lba: 0,
                    requested: 512 * 1024,
                    returned: 256 * 1024,
                },
                RecordingRawSourceCall::Configure(256 * 1024),
                RecordingRawSourceCall::Locate(0),
                RecordingRawSourceCall::ReadRecord {
                    lba: 0,
                    requested: 256 * 1024,
                    returned: 256 * 1024,
                },
            ]
        );
    }

    #[test]
    fn discover_candidate_fallback_propagates_raw_source_errors() {
        let mut source = FailingLocateRawSource;

        let err = discover_bootstrap_with_candidate_block_sizes(&mut source, None, &[512 * 1024])
            .expect_err("raw-source failures must not be masked as missing bootstrap");

        match err {
            ParityError::TapeIo(TapeIoError::OperationFailed(message)) => {
                assert!(message.contains("synthetic locate failure"), "{message}");
            }
            other => panic!("expected raw source error, got {other:?}"),
        }
    }

    #[test]
    fn discover_candidate_fallback_propagates_read_errors_that_are_not_medium_errors() {
        let mut source = FailingReadRawSource;

        let err = discover_bootstrap_with_candidate_block_sizes(&mut source, None, &[512 * 1024])
            .expect_err("non-medium read failures must not be masked as missing bootstrap");

        match err {
            ParityError::TapeIo(TapeIoError::OperationFailed(message)) => {
                assert!(message.contains("synthetic read failure"), "{message}");
            }
            other => panic!("expected raw read error, got {other:?}"),
        }
    }

    #[test]
    fn discover_with_wrong_configured_size_reports_short_fixed_block_read() {
        let mut payload = sample_payload();
        payload.block_size_bytes = 256 * 1024;
        let mut buf = vec![0u8; test_block_size(payload.block_size_bytes)];
        write_bootstrap_block(&payload, &mut buf).expect("write ok");
        let mut src = VecBlockSource::new(vec![buf]);

        let err = {
            let mut raw = BlockSourceRawTapeSource::new(&mut src);
            let err = discover_bootstrap_with_block_size(&mut raw, None, 512 * 1024).unwrap_err();
            assert_eq!(raw.configured_block_size(), Some(512 * 1024));
            err
        };

        match err {
            ParityError::BootstrapParse(msg) => {
                assert!(msg.contains("short fixed-block bootstrap read"), "{msg}");
            }
            other => panic!("expected BootstrapParse, got {other:?}"),
        }
    }

    #[test]
    fn discover_returns_no_bootstrap_found_on_empty_tape() {
        let mut src = RecordingRawSource::new(vec![]);
        let err = discover_bootstrap(&mut src, None).unwrap_err();
        assert!(matches!(err, ParityError::NoBootstrapFound));
    }

    #[test]
    fn unknown_cbor_fields_are_ignored_on_decode() {
        // Forward-compat: a future writer adds a field at tag 99.
        // Today's reader should ignore it cleanly.
        let payload = sample_payload();
        let mut buf = vec![0u8; 1_048_576];
        write_bootstrap_block(&payload, &mut buf).expect("write ok");

        // Decode CBOR, add a field, re-encode, re-frame.
        let cbor_len = usize::try_from(u32::from_le_bytes(buf[44..48].try_into().unwrap()))
            .expect("CBOR length fits host address space");
        let cbor_bytes = &buf[BOOTSTRAP_HEADER_LEN..BOOTSTRAP_HEADER_LEN + cbor_len];
        let mut value: CborValue = ciborium::from_reader(cbor_bytes).expect("decode");
        if let CborValue::Map(ref mut m) = value {
            m.push((
                CborValue::Integer(99.into()),
                CborValue::Text("future field".into()),
            ));
        }
        let mut new_cbor = Vec::new();
        ciborium::into_writer(&value, &mut new_cbor).expect("re-encode");
        let new_len: u32 = new_cbor.len().try_into().expect("CBOR length fits u32");
        // Rewrite buf with the extended CBOR.
        buf[44..48].copy_from_slice(&new_len.to_le_bytes());
        let new_crc_header = crc64_xz(&buf[0..BOOTSTRAP_HEADER_CRC_OFFSET]);
        buf[48..56].copy_from_slice(&new_crc_header.to_le_bytes());
        // payload area starts right after header.
        let cbor_end = BOOTSTRAP_HEADER_LEN + new_cbor.len();
        buf[BOOTSTRAP_HEADER_LEN..cbor_end].copy_from_slice(&new_cbor);
        let crc_payload = crc64_xz(&new_cbor);
        buf[cbor_end..cbor_end + BOOTSTRAP_PAYLOAD_CRC_LEN]
            .copy_from_slice(&crc_payload.to_le_bytes());

        let parsed = parse_bootstrap_block(&buf[..]).expect("future-field parse ok");
        assert_eq!(parsed, payload);
    }
}
