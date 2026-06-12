//! Bootstrap block — the canonical root of trust per tape.
//!
//! The bootstrap is the first block a reader finds on tape
//! mount. It tells the reader the parity scheme (so a
//! [`ObjectParitySource`](crate::ObjectParitySource) can be constructed),
//! the tape UUID (which derives the per-tape parity magic), and
//! the filemark-map digest that authenticates catalog-less
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

use crate::error::ParityError;
use crate::filemark_map::FilemarkMapDigest;
use crate::parity_map::{
    decode_parity_map_reference_cbor, decode_sidecar_epoch_directory_cbor,
    encode_parity_map_reference_cbor, encode_sidecar_epoch_directory_cbor, ParityMapReference,
    SidecarEpochDirectory,
};
use crate::raw::{PhysicalPositionHint, RawReadOutcome, RawTapeSource};
use crate::sidecar::crc64_xz;

/// Magic at byte 0 of every bootstrap block.
pub const BOOTSTRAP_MAGIC: [u8; 8] = *b"REM\x00BOO\x01";

/// Schema-major version this writer emits / this reader
/// accepts. Major bumps require an explicit migration plan
/// documented in `docs/layer3c-design.md`.
pub const BOOTSTRAP_SCHEMA_MAJOR: u16 = 1;

/// Schema-minor version this writer emits. Reader accepts
/// minors `<= BOOTSTRAP_SCHEMA_MINOR` written by older
/// versions, and minors `> BOOTSTRAP_SCHEMA_MINOR` written by
/// newer versions (forward-compatible). Minor bumps add fields
/// with sensible defaults.
pub const BOOTSTRAP_SCHEMA_MINOR: u16 = 2;

/// `flags` bit 0: this tape was written with `--parity none`
/// and contains no parity blocks. Readers see this bit and
/// bypass the parity source.
pub const FLAG_NO_PARITY: u32 = 1 << 0;

/// Byte offset of the bootstrap header CRC-64/XZ field.
pub const BOOTSTRAP_HEADER_CRC_OFFSET: usize = 0x2C;

/// Size of the fixed bootstrap header, through the header CRC field.
pub const BOOTSTRAP_HEADER_LEN: usize = 0x34;

const BOOTSTRAP_PAYLOAD_CRC_LEN: usize = 8;
const OBJECT_ROWS_KEY: u64 = 30;
const OBJECT_ROW_METADATA_FRAME_MIN_LEN: u64 = 17;
const OBJECT_ROW_METADATA_FRAME_MAX_LEN: u64 = 16 * 1024 * 1024;

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
    /// rem software version string that wrote this tape.
    pub written_by_version: String,
    /// RFC3339 timestamp of when this bootstrap copy was
    /// written.
    pub written_at: String,
    /// Bootstrap sequence number (0 at LBA 0; subsequent
    /// copies increment).
    pub sequence: u32,
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
    /// Inline sidecar epoch directory, when it fits in this bootstrap block.
    pub sidecar_epoch_directory: Option<SidecarEpochDirectory>,
    /// Reference to an external `parity_map` tape file carrying the directory.
    pub parity_map_reference: Option<ParityMapReference>,
    /// Optional RAO-binding per-object rows carried by checkpoint/final
    /// bootstraps. The parity layer treats object bytes as opaque; higher
    /// layers supply these rows when they have representation-specific anchors.
    pub object_rows: Vec<BootstrapObjectRow>,
}

/// One object row carried in a bootstrap payload's object directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootstrapObjectRow {
    /// Filemark-delimited tape-file number of the object copy.
    pub tape_file_number: u32,
    /// Number of fixed-size tape blocks occupied by the stored copy.
    pub stored_block_count: u64,
    /// Representation-specific recovery anchors for the copy.
    pub representation: BootstrapObjectRepresentation,
}

impl BootstrapObjectRow {
    /// Construct a plaintext RAO object row with manifest anchors.
    pub fn plaintext(
        tape_file_number: u32,
        stored_block_count: u64,
        manifest_first_chunk_lba: u64,
        manifest_size_bytes: u64,
        manifest_chunk_count: u64,
        manifest_sha256: [u8; 32],
    ) -> Self {
        Self {
            tape_file_number,
            stored_block_count,
            representation: BootstrapObjectRepresentation::Plaintext {
                manifest_first_chunk_lba,
                manifest_size_bytes,
                manifest_chunk_count,
                manifest_sha256,
            },
        }
    }

    /// Construct an encrypted RAO object row with envelope fields only.
    pub fn encrypted(
        tape_file_number: u32,
        stored_block_count: u64,
        key_id: [u8; 16],
        metadata_frame_len: u64,
    ) -> Self {
        Self {
            tape_file_number,
            stored_block_count,
            representation: BootstrapObjectRepresentation::Encrypted {
                key_id,
                metadata_frame_len,
            },
        }
    }
}

/// Representation-specific payload for one bootstrap object row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BootstrapObjectRepresentation {
    /// Plaintext RAO representation: bootstrap row carries manifest anchors.
    Plaintext {
        /// Object-local body LBA of the generated manifest.
        manifest_first_chunk_lba: u64,
        /// Manifest byte length.
        manifest_size_bytes: u64,
        /// Number of object-local chunks occupied by the manifest.
        manifest_chunk_count: u64,
        /// SHA-256 digest of the manifest CBOR bytes.
        manifest_sha256: [u8; 32],
    },
    /// Encrypted RAO representation: bootstrap row carries envelope fields
    /// only and deliberately omits plaintext manifest anchors.
    Encrypted {
        /// Opaque 16-byte key identifier from the RAO encrypted header.
        key_id: [u8; 16],
        /// RAO encrypted metadata frame length.
        metadata_frame_len: u64,
    },
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
    if !payload.no_parity_flag && payload.drive_compression {
        return Err(ParityError::DriveCompressionEnabled);
    }
    if payload.sidecar_epoch_directory.is_some() && payload.parity_map_reference.is_some() {
        return Err(ParityError::Invariant(
            "BootstrapPayload: sidecar_epoch_directory and parity_map_reference are mutually exclusive",
        ));
    }
    validate_bootstrap_object_rows(&payload.object_rows, Some(payload.block_size_bytes))?;

    let block_size = payload.block_size_bytes as usize;
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
    buf[36..40].copy_from_slice(&payload.sequence.to_be_bytes());
    buf[40..44].copy_from_slice(&payload_len_u32.to_le_bytes());
    // Header CRC covers bytes 0..0x2C, including cbor_payload_len.
    let crc_header = crc64_xz(&buf[0..BOOTSTRAP_HEADER_CRC_OFFSET]);
    buf[44..52].copy_from_slice(&crc_header.to_le_bytes());

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

    // Header CRC validates bytes 0..0x2C against bytes 0x2C..0x34.
    let stored_header_crc = u64::from_le_bytes(buf[44..52].try_into().unwrap());
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
    let sequence = u32::from_be_bytes(buf[36..40].try_into().unwrap());
    let payload_len = u32::from_le_bytes(buf[40..44].try_into().unwrap()) as usize;

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

    let decoded = decode_cbor_payload(cbor_bytes, no_parity, block_size_bytes)?;

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
        sidecar_epoch_directory: decoded.sidecar_epoch_directory,
        parity_map_reference: decoded.parity_map_reference,
        object_rows: decoded.object_rows,
    })
}

/// Cheap magic-only check used by the discovery scanner before
/// running the full parse. Returns true if `buf` starts with
/// the bootstrap magic.
pub fn has_bootstrap_magic(buf: &[u8]) -> bool {
    buf.len() >= BOOTSTRAP_MAGIC.len() && buf[0..BOOTSTRAP_MAGIC.len()] == BOOTSTRAP_MAGIC
}

/// Max blocks to scan forward from each candidate position
/// looking for bootstrap magic. Picked to comfortably exceed
/// typical inter-bootstrap object spacing (~1 GiB ≈ 1024 blocks
/// at 1 MiB).
pub const MAX_BOOTSTRAP_SCAN_BLOCKS: u32 = 1024;

/// Candidate fixed block sizes used when the caller has no catalog
/// or operator-provided block-size hint. The normal path should use
/// [`discover_bootstrap_with_block_size`].
pub const DEFAULT_BOOTSTRAP_CANDIDATE_BLOCK_SIZES: &[u32] = &[256 * 1024, 512 * 1024, 1024 * 1024];

/// Find a valid bootstrap block on the tape. Per design §8.1:
/// the writer always places copy 0 at LBA 0; subsequent copies
/// land at writer-policy LBAs that the design recommends near
/// ~5%, ~10%, ... of tape capacity. The reader tries each
/// expected position in order and scans forward up to
/// [`MAX_BOOTSTRAP_SCAN_BLOCKS`] looking for bootstrap magic.
///
/// Used at tape-mount time before constructing a
/// [`ObjectParitySource`](crate::ObjectParitySource) — the source needs the
/// scheme, which only the bootstrap can provide.
///
/// `tape_total_blocks_hint` lets the scanner compute fractional
/// positions. `None` skips the fractional fallbacks and only
/// checks LBA 0; that's the common case for healthy tapes and
/// the cheapest path.
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
    tape_total_blocks_hint: Option<u64>,
    block_size: u32,
) -> Result<BootstrapPayload, ParityError> {
    source.configure_fixed_block_size(block_size)?;
    let mut first_parse_error = None;
    for pos in expected_bootstrap_positions(tape_total_blocks_hint) {
        match try_read_bootstrap_at(source, pos, block_size) {
            Ok(bp) => return Ok(bp),
            Err(err) => {
                if bootstrap_probe_can_continue(&err) {
                    if first_parse_error.is_none() && matches!(err, ParityError::BootstrapParse(_))
                    {
                        first_parse_error = Some(err);
                    }
                    continue;
                }
                return Err(err);
            }
        }
    }
    Err(first_parse_error.unwrap_or(ParityError::NoBootstrapFound))
}

/// Discover the authoritative bootstrap copy when the fixed block size is
/// already known.
///
/// Layer 3c v0.4.4 §8.1 deliberately separates "first valid bootstrap" from
/// "authoritative bootstrap": the BOT copy is enough to learn the scheme and
/// block size, but a later bootstrap may carry a wider filemark-map digest.
/// This helper probes every expected bootstrap region at `block_size`, accepts
/// only fully parsed bootstrap blocks, and returns the copy with the widest
/// map scope: final map first, otherwise highest sequence, otherwise largest
/// mapped ordinal count.
pub fn discover_authoritative_bootstrap_with_block_size(
    source: &mut dyn RawTapeSource,
    tape_total_blocks_hint: Option<u64>,
    block_size: u32,
) -> Result<BootstrapPayload, ParityError> {
    source.configure_fixed_block_size(block_size)?;
    let mut best: Option<BootstrapPayload> = None;
    let mut first_parse_error = None;
    for pos in expected_bootstrap_positions(tape_total_blocks_hint) {
        match try_read_bootstrap_at(source, pos, block_size) {
            Ok(bp) => {
                best = Some(match best {
                    None => bp,
                    Some(prev) => choose_wider_map_scope(prev, bp),
                });
            }
            Err(err) => {
                if bootstrap_probe_can_continue(&err) {
                    if first_parse_error.is_none() && matches!(err, ParityError::BootstrapParse(_))
                    {
                        first_parse_error = Some(err);
                    }
                    continue;
                }
                return Err(err);
            }
        }
    }
    match best {
        Some(payload) => Ok(payload),
        None => Err(first_parse_error.unwrap_or(ParityError::NoBootstrapFound)),
    }
}

/// Discover the authoritative bootstrap copy when the fixed block size may be
/// unknown.
///
/// The first valid bootstrap found through the normal candidate-size fallback
/// supplies the block size; then the tape is rescanned at that exact size to
/// select the highest-scope bootstrap copy for filemark-map validation.
pub fn discover_authoritative_bootstrap(
    source: &mut dyn RawTapeSource,
    tape_total_blocks_hint: Option<u64>,
) -> Result<BootstrapPayload, ParityError> {
    let first_valid = discover_bootstrap(source, tape_total_blocks_hint)?;
    discover_authoritative_bootstrap_with_block_size(
        source,
        tape_total_blocks_hint,
        first_valid.block_size_bytes,
    )
}

/// Discover a bootstrap for a catalog-less tape whose block size is
/// unknown. Each candidate is a real fixed-block read size from the
/// caller's perspective; the first candidate whose block parses and
/// whose header records the same size wins.
pub fn discover_bootstrap_with_candidate_block_sizes(
    source: &mut dyn RawTapeSource,
    tape_total_blocks_hint: Option<u64>,
    candidate_block_sizes: &[u32],
) -> Result<BootstrapPayload, ParityError> {
    for block_size in candidate_block_sizes {
        source.configure_fixed_block_size(*block_size)?;
        for pos in expected_bootstrap_positions(tape_total_blocks_hint) {
            match try_read_bootstrap_at(source, pos, *block_size) {
                Ok(bp) => return Ok(bp),
                Err(err) if bootstrap_probe_can_continue(&err) => continue,
                Err(err) => return Err(err),
            }
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

/// Compute the sequence of LBAs the discovery scanner will try.
/// LBA 0 always first (§7.3 invariant); fractional positions
/// follow if a tape-size hint is supplied.
pub fn expected_bootstrap_positions(tape_total_blocks_hint: Option<u64>) -> Vec<u64> {
    let mut positions = vec![0u64];
    if let Some(total) = tape_total_blocks_hint {
        // Per §7.3 default policy: bootstrap copies land at
        // ~5% intervals. Try every 5% mark; the scan window
        // tolerates jitter caused by writer-policy object
        // alignment.
        for pct in 1u64..=19 {
            let target = total * (pct * 5) / 100;
            if target > 0 && target < total {
                positions.push(target);
            }
        }
    }
    positions
}

fn choose_wider_map_scope(a: BootstrapPayload, b: BootstrapPayload) -> BootstrapPayload {
    if bootstrap_scope_key(&b) > bootstrap_scope_key(&a) {
        b
    } else {
        a
    }
}

fn bootstrap_scope_key(payload: &BootstrapPayload) -> (bool, u32, u64) {
    let digest = payload.filemark_map_digest.as_ref();
    (
        digest.map(|d| d.is_final_map).unwrap_or(false),
        payload.sequence,
        digest.map(|d| d.map_total_data_ordinals).unwrap_or(0),
    )
}

fn try_read_bootstrap_at(
    source: &mut dyn RawTapeSource,
    target_lba: u64,
    block_size: u32,
) -> Result<BootstrapPayload, ParityError> {
    if block_size == 0 {
        return Err(ParityError::Invariant("bootstrap block size is zero"));
    }
    let block_size = block_size as usize;
    source.locate_physical(PhysicalPositionHint::new(target_lba))?;
    let mut buf = vec![0u8; block_size];
    for _ in 0..MAX_BOOTSTRAP_SCAN_BLOCKS {
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
                            if bp.block_size_bytes as usize != block_size {
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
        entries.push((
            CborValue::Integer(3.into()),
            CborValue::Text(payload.written_by_version.clone()),
        ));
    }

    // Tag 4: write timestamp. Omitted if empty.
    if !payload.written_at.is_empty() {
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

    // Tag 20: inline sidecar epoch directory, when the directory fits in the
    // bootstrap block. Tag 21 is used instead when an external parity_map
    // control file carries the directory.
    if let Some(directory) = payload.sidecar_epoch_directory.as_ref() {
        entries.push((
            CborValue::Integer(20.into()),
            encode_sidecar_epoch_directory_cbor(directory)?,
        ));
    }

    // Tag 21: external parity_map reference.
    if let Some(reference) = payload.parity_map_reference.as_ref() {
        reference.validate()?;
        entries.push((
            CborValue::Integer(21.into()),
            encode_parity_map_reference_cbor(reference),
        ));
    }

    // Tag 30: RAO-binding object rows. Older readers ignore this unknown key;
    // schema-minor 2 readers use it for catalog-less object recovery hints.
    if !payload.object_rows.is_empty() {
        let rows = payload
            .object_rows
            .iter()
            .map(encode_bootstrap_object_row_cbor)
            .collect::<Result<Vec<_>, _>>()?;
        entries.push((
            CborValue::Integer(OBJECT_ROWS_KEY.into()),
            CborValue::Array(rows),
        ));
    }

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
    sidecar_epoch_directory: Option<SidecarEpochDirectory>,
    parity_map_reference: Option<ParityMapReference>,
    object_rows: Vec<BootstrapObjectRow>,
}

fn decode_cbor_payload(
    bytes: &[u8],
    no_parity_flag: bool,
    block_size_bytes: u32,
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
    let mut sidecar_epoch_directory: Option<SidecarEpochDirectory> = None;
    let mut parity_map_reference: Option<ParityMapReference> = None;
    let mut object_rows = Vec::new();

    for (key, value) in map {
        let key_i = match key {
            CborValue::Integer(i) => i,
            _ => continue, // ignore unknown / future tstr keys
        };
        let key_i: i128 = key_i.into();
        match key_i {
            1 => {
                scheme_record = Some(decode_scheme_record(value, no_parity_flag)?);
            }
            2 => {
                filemark_map_digest = Some(decode_filemark_map_digest(value)?);
            }
            3 => {
                if let CborValue::Text(s) = value {
                    written_by_version = s;
                }
            }
            4 => {
                if let CborValue::Text(s) = value {
                    written_at = s;
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
            20 => {
                sidecar_epoch_directory = Some(decode_sidecar_epoch_directory_cbor(value)?);
            }
            21 => {
                parity_map_reference = Some(decode_parity_map_reference_cbor(value)?);
            }
            key if key == i128::from(OBJECT_ROWS_KEY) => {
                object_rows = decode_bootstrap_object_rows_cbor(value, Some(block_size_bytes))?;
            }
            _ => {
                // Forward-compatible: ignore unknown integer
                // keys from newer minor versions.
            }
        }
    }

    if sidecar_epoch_directory.is_some() && parity_map_reference.is_some() {
        return Err(ParityError::BootstrapParse(
            "CBOR payload carries both inline sidecar directory and parity_map reference".into(),
        ));
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
        sidecar_epoch_directory,
        parity_map_reference,
        object_rows,
    })
}

pub(crate) fn encode_bootstrap_object_row_cbor(
    row: &BootstrapObjectRow,
) -> Result<CborValue, ParityError> {
    validate_bootstrap_object_row(row, None)?;
    let mut entries = vec![
        (
            CborValue::Integer(1.into()),
            CborValue::Integer(row.tape_file_number.into()),
        ),
        (
            CborValue::Integer(3.into()),
            CborValue::Integer(row.stored_block_count.into()),
        ),
    ];
    match &row.representation {
        BootstrapObjectRepresentation::Plaintext {
            manifest_first_chunk_lba,
            manifest_size_bytes,
            manifest_chunk_count,
            manifest_sha256,
        } => {
            entries.insert(
                1,
                (
                    CborValue::Integer(2.into()),
                    CborValue::Text("plaintext".to_string()),
                ),
            );
            entries.extend([
                (
                    CborValue::Integer(10.into()),
                    CborValue::Integer((*manifest_first_chunk_lba).into()),
                ),
                (
                    CborValue::Integer(11.into()),
                    CborValue::Integer((*manifest_size_bytes).into()),
                ),
                (
                    CborValue::Integer(12.into()),
                    CborValue::Integer((*manifest_chunk_count).into()),
                ),
                (
                    CborValue::Integer(13.into()),
                    CborValue::Bytes(manifest_sha256.to_vec()),
                ),
            ]);
        }
        BootstrapObjectRepresentation::Encrypted {
            key_id,
            metadata_frame_len,
        } => {
            entries.insert(
                1,
                (
                    CborValue::Integer(2.into()),
                    CborValue::Text("encrypted".to_string()),
                ),
            );
            entries.extend([
                (
                    CborValue::Integer(20.into()),
                    CborValue::Bytes(key_id.to_vec()),
                ),
                (
                    CborValue::Integer(21.into()),
                    CborValue::Integer((*metadata_frame_len).into()),
                ),
            ]);
        }
    }
    Ok(CborValue::Map(entries))
}

pub(crate) fn decode_bootstrap_object_row_cbor(
    value: CborValue,
    block_size_bytes: Option<u32>,
) -> Result<BootstrapObjectRow, ParityError> {
    let map = match value {
        CborValue::Map(map) => map,
        _ => {
            return Err(ParityError::BootstrapParse(
                "bootstrap object row is not a map".into(),
            ))
        }
    };

    let mut tape_file_number = None;
    let mut representation = None;
    let mut stored_block_count = None;
    let mut manifest_first_chunk_lba = None;
    let mut manifest_size_bytes = None;
    let mut manifest_chunk_count = None;
    let mut manifest_sha256 = None;
    let mut key_id = None;
    let mut metadata_frame_len = None;
    let mut seen_integer_keys = std::collections::BTreeSet::new();

    for (key, value) in map {
        let key_i: i128 = match key {
            CborValue::Integer(i) => i.into(),
            _ => continue,
        };
        if !seen_integer_keys.insert(key_i) {
            return Err(ParityError::BootstrapParse(format!(
                "duplicate object row key {key_i}"
            )));
        }
        match (key_i, value) {
            (1, CborValue::Integer(i)) => {
                tape_file_number = Some(int_to_u32(i, "object_row.tape_file_number")?)
            }
            (2, CborValue::Text(value)) => representation = Some(value),
            (3, CborValue::Integer(i)) => {
                stored_block_count = Some(int_to_u64(i, "object_row.stored_block_count")?)
            }
            (10, CborValue::Integer(i)) => {
                manifest_first_chunk_lba =
                    Some(int_to_u64(i, "object_row.manifest_first_chunk_lba")?)
            }
            (11, CborValue::Integer(i)) => {
                manifest_size_bytes = Some(int_to_u64(i, "object_row.manifest_size_bytes")?)
            }
            (12, CborValue::Integer(i)) => {
                manifest_chunk_count = Some(int_to_u64(i, "object_row.manifest_chunk_count")?)
            }
            (13, CborValue::Bytes(bytes)) => {
                manifest_sha256 = Some(bytes.try_into().map_err(|bytes: Vec<u8>| {
                    ParityError::BootstrapParse(format!(
                        "object_row.manifest_sha256 has length {}, expected 32",
                        bytes.len()
                    ))
                })?)
            }
            (20, CborValue::Bytes(bytes)) => {
                key_id = Some(bytes.try_into().map_err(|bytes: Vec<u8>| {
                    ParityError::BootstrapParse(format!(
                        "object_row.key_id has length {}, expected 16",
                        bytes.len()
                    ))
                })?)
            }
            (21, CborValue::Integer(i)) => {
                metadata_frame_len = Some(int_to_u64(i, "object_row.metadata_frame_len")?)
            }
            _ => {}
        }
    }

    let tape_file_number = tape_file_number
        .ok_or_else(|| ParityError::BootstrapParse("object row missing tape_file_number".into()))?;
    let stored_block_count = stored_block_count.ok_or_else(|| {
        ParityError::BootstrapParse("object row missing stored_block_count".into())
    })?;
    let representation = representation
        .ok_or_else(|| ParityError::BootstrapParse("object row missing representation".into()))?;
    let row = match representation.as_str() {
        "plaintext" => {
            if key_id.is_some() || metadata_frame_len.is_some() {
                return Err(ParityError::BootstrapParse(
                    "plaintext object row carries encrypted envelope fields".into(),
                ));
            }
            BootstrapObjectRow::plaintext(
                tape_file_number,
                stored_block_count,
                manifest_first_chunk_lba.ok_or_else(|| {
                    ParityError::BootstrapParse(
                        "plaintext object row missing manifest_first_chunk_lba".into(),
                    )
                })?,
                manifest_size_bytes.ok_or_else(|| {
                    ParityError::BootstrapParse(
                        "plaintext object row missing manifest_size_bytes".into(),
                    )
                })?,
                manifest_chunk_count.ok_or_else(|| {
                    ParityError::BootstrapParse(
                        "plaintext object row missing manifest_chunk_count".into(),
                    )
                })?,
                manifest_sha256.ok_or_else(|| {
                    ParityError::BootstrapParse(
                        "plaintext object row missing manifest_sha256".into(),
                    )
                })?,
            )
        }
        "encrypted" => {
            if manifest_first_chunk_lba.is_some()
                || manifest_size_bytes.is_some()
                || manifest_chunk_count.is_some()
                || manifest_sha256.is_some()
            {
                return Err(ParityError::BootstrapParse(
                    "encrypted object row carries plaintext manifest anchors".into(),
                ));
            }
            BootstrapObjectRow::encrypted(
                tape_file_number,
                stored_block_count,
                key_id.ok_or_else(|| {
                    ParityError::BootstrapParse("encrypted object row missing key_id".into())
                })?,
                metadata_frame_len.ok_or_else(|| {
                    ParityError::BootstrapParse(
                        "encrypted object row missing metadata_frame_len".into(),
                    )
                })?,
            )
        }
        other => {
            return Err(ParityError::BootstrapParse(format!(
                "unsupported object row representation {other}"
            )))
        }
    };
    validate_bootstrap_object_row(&row, block_size_bytes)?;
    Ok(row)
}

fn decode_bootstrap_object_rows_cbor(
    value: CborValue,
    block_size_bytes: Option<u32>,
) -> Result<Vec<BootstrapObjectRow>, ParityError> {
    let rows = match value {
        CborValue::Array(rows) => rows,
        _ => {
            return Err(ParityError::BootstrapParse(
                "bootstrap object rows are not an array".into(),
            ))
        }
    };
    let rows = rows
        .into_iter()
        .map(|row| decode_bootstrap_object_row_cbor(row, block_size_bytes))
        .collect::<Result<Vec<_>, _>>()?;
    validate_bootstrap_object_rows(&rows, block_size_bytes)?;
    Ok(rows)
}

pub(crate) fn validate_bootstrap_object_rows(
    rows: &[BootstrapObjectRow],
    block_size_bytes: Option<u32>,
) -> Result<(), ParityError> {
    let mut previous_tape_file_number = None;
    for row in rows {
        validate_bootstrap_object_row(row, block_size_bytes)?;
        if let Some(previous) = previous_tape_file_number {
            if row.tape_file_number <= previous {
                return Err(ParityError::BootstrapParse(
                    "bootstrap object rows must be in strictly increasing tape-file order".into(),
                ));
            }
        }
        previous_tape_file_number = Some(row.tape_file_number);
    }
    Ok(())
}

pub(crate) fn validate_bootstrap_object_row(
    row: &BootstrapObjectRow,
    block_size_bytes: Option<u32>,
) -> Result<(), ParityError> {
    if row.stored_block_count == 0 {
        return Err(ParityError::BootstrapParse(
            "object row stored_block_count must be positive".into(),
        ));
    }
    match &row.representation {
        BootstrapObjectRepresentation::Plaintext {
            manifest_first_chunk_lba,
            manifest_size_bytes,
            manifest_chunk_count,
            ..
        } => {
            if *manifest_size_bytes == 0 || *manifest_chunk_count == 0 {
                return Err(ParityError::BootstrapParse(
                    "plaintext object row manifest size/count must be positive".into(),
                ));
            }
            let manifest_end = manifest_first_chunk_lba
                .checked_add(*manifest_chunk_count)
                .ok_or_else(|| {
                    ParityError::BootstrapParse("plaintext manifest chunk range overflows".into())
                })?;
            if manifest_end > row.stored_block_count {
                return Err(ParityError::BootstrapParse(
                    "plaintext manifest chunk range exceeds stored block count".into(),
                ));
            }
            if let Some(block_size_bytes) = block_size_bytes {
                let manifest_capacity = manifest_chunk_count
                    .checked_mul(u64::from(block_size_bytes))
                    .ok_or_else(|| {
                        ParityError::BootstrapParse(
                            "plaintext manifest byte capacity overflows".into(),
                        )
                    })?;
                if *manifest_size_bytes > manifest_capacity {
                    return Err(ParityError::BootstrapParse(
                        "plaintext manifest size exceeds manifest chunk capacity".into(),
                    ));
                }
            }
        }
        BootstrapObjectRepresentation::Encrypted {
            key_id,
            metadata_frame_len,
        } => {
            if key_id.iter().all(|byte| *byte == 0) {
                return Err(ParityError::BootstrapParse(
                    "encrypted object row key_id must be nonzero".into(),
                ));
            }
            if !(OBJECT_ROW_METADATA_FRAME_MIN_LEN..=OBJECT_ROW_METADATA_FRAME_MAX_LEN)
                .contains(metadata_frame_len)
            {
                return Err(ParityError::BootstrapParse(
                    "encrypted object row metadata_frame_len is outside RAO bounds".into(),
                ));
            }
        }
    }
    Ok(())
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
            CborValue::Bool(digest.is_final_map),
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
    let mut tape_file_count: Option<u32> = None;
    let mut map_total_data_ordinals: Option<u64> = None;
    let mut highest_protected_ordinal: Option<u64> = None;
    let mut is_final_map: Option<bool> = None;

    for (key, value) in map {
        let key_i: i128 = match key {
            CborValue::Integer(i) => i.into(),
            _ => continue,
        };
        match (key_i, value) {
            (1, CborValue::Bytes(bytes)) => {
                map_sha256 = Some(bytes.try_into().map_err(|bytes: Vec<u8>| {
                    ParityError::BootstrapParse(format!(
                        "filemark map digest sha256 has length {}, expected 32",
                        bytes.len()
                    ))
                })?);
            }
            (2, CborValue::Integer(i)) => tape_file_count = Some(int_to_u32(i, "tape_file_count")?),
            (3, CborValue::Integer(i)) => {
                map_total_data_ordinals = Some(int_to_u64(i, "map_total_data_ordinals")?)
            }
            (4, CborValue::Integer(i)) => {
                highest_protected_ordinal = Some(int_to_u64(i, "highest_protected_ordinal")?)
            }
            (5, CborValue::Bool(v)) => is_final_map = Some(v),
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
        is_final_map: is_final_map.ok_or_else(|| {
            ParityError::BootstrapParse("filemark map digest missing is_final_map".into())
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

    for (key, value) in map {
        let key_i: i128 = match key {
            CborValue::Integer(i) => i.into(),
            _ => continue,
        };
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
    use super::*;
    use crate::parity_map::{
        ParityMapReference, SidecarEpochDirectory, SidecarEpochDirectoryEntry,
        SIDECAR_DIRECTORY_FLAG_PRIMARY_KNOWN_GOOD, SIDECAR_DIRECTORY_FLAG_TAIL_KNOWN_GOOD,
    };

    fn sample_digest() -> FilemarkMapDigest {
        FilemarkMapDigest {
            map_sha256: [0xA5; 32],
            tape_file_count: 3,
            map_total_data_ordinals: 128,
            highest_protected_ordinal: 64,
            is_final_map: false,
        }
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
            sidecar_epoch_directory: None,
            parity_map_reference: None,
            object_rows: Vec::new(),
        }
    }

    fn sample_sidecar_directory() -> SidecarEpochDirectory {
        SidecarEpochDirectory {
            directory_scope_tape_file_count: 4,
            directory_scope_total_data_ordinals: 3,
            directory_scope_highest_protected_ordinal: 3,
            is_final_directory: true,
            entries: vec![SidecarEpochDirectoryEntry {
                tape_file_number: 2,
                epoch_id: 0,
                protected_ordinal_start: 0,
                protected_ordinal_end_exclusive: 3,
                sidecar_total_block_count: 9,
                sidecar_header_block_count: 2,
                parity_shard_block_count: 4,
                canonical_metadata_hash: [0x33; 32],
                flags: SIDECAR_DIRECTORY_FLAG_PRIMARY_KNOWN_GOOD
                    | SIDECAR_DIRECTORY_FLAG_TAIL_KNOWN_GOOD,
            }],
        }
    }

    fn encode_bootstrap_block_unchecked_for_test(payload: &BootstrapPayload) -> Vec<u8> {
        let block_size = payload.block_size_bytes as usize;
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
        buf[36..40].copy_from_slice(&payload.sequence.to_be_bytes());
        buf[40..44].copy_from_slice(&payload_len_u32.to_le_bytes());
        let crc_header = crc64_xz(&buf[0..BOOTSTRAP_HEADER_CRC_OFFSET]);
        buf[44..52].copy_from_slice(&crc_header.to_le_bytes());

        let cbor_end = BOOTSTRAP_HEADER_LEN + cbor_bytes.len();
        buf[BOOTSTRAP_HEADER_LEN..cbor_end].copy_from_slice(&cbor_bytes);
        let crc_payload = crc64_xz(&cbor_bytes);
        buf[cbor_end..cbor_end + BOOTSTRAP_PAYLOAD_CRC_LEN]
            .copy_from_slice(&crc_payload.to_le_bytes());
        buf
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
    fn roundtrip_payload_with_inline_sidecar_directory() {
        let mut payload = sample_payload();
        payload.sidecar_epoch_directory = Some(sample_sidecar_directory());
        let mut buf = vec![0u8; 1_048_576];

        write_bootstrap_block(&payload, &mut buf).expect("write ok");
        let parsed = parse_bootstrap_block(&buf).expect("parse ok");

        assert_eq!(parsed, payload);
        assert!(parsed.sidecar_epoch_directory.is_some());
        assert!(parsed.parity_map_reference.is_none());
    }

    #[test]
    fn roundtrip_payload_with_parity_map_reference() {
        let mut payload = sample_payload();
        payload.parity_map_reference = Some(ParityMapReference {
            tape_file_number: 4,
            block_count: 7,
            directory_scope_tape_file_count: 5,
            directory_scope_total_data_ordinals: 3,
            directory_scope_highest_protected_ordinal: 3,
            is_final_directory: true,
            parity_map_payload_sha256: [0x44; 32],
            canonical_map_digest: [0x55; 32],
        });
        let mut buf = vec![0u8; 1_048_576];

        write_bootstrap_block(&payload, &mut buf).expect("write ok");
        let parsed = parse_bootstrap_block(&buf).expect("parse ok");

        assert_eq!(parsed, payload);
        assert!(parsed.sidecar_epoch_directory.is_none());
        assert!(parsed.parity_map_reference.is_some());
    }

    #[test]
    fn roundtrip_payload_with_object_rows() {
        let mut payload = sample_payload();
        payload.object_rows = vec![
            BootstrapObjectRow::plaintext(1, 8, 6, 1234, 1, [0xA1; 32]),
            BootstrapObjectRow::encrypted(3, 11, [0x24; 16], 66),
        ];
        let mut buf = vec![0xCC; payload.block_size_bytes as usize];

        write_bootstrap_block(&payload, &mut buf).expect("write ok");
        let parsed = parse_bootstrap_block(&buf).expect("parse ok");

        assert_eq!(parsed.object_rows, payload.object_rows);
        assert_eq!(parsed, payload);
    }

    #[test]
    fn encrypted_object_row_rejects_plaintext_manifest_anchors() {
        let row = CborValue::Map(vec![
            (CborValue::Integer(1.into()), CborValue::Integer(1.into())),
            (
                CborValue::Integer(2.into()),
                CborValue::Text("encrypted".to_string()),
            ),
            (CborValue::Integer(3.into()), CborValue::Integer(4.into())),
            (CborValue::Integer(10.into()), CborValue::Integer(2.into())),
            (
                CborValue::Integer(20.into()),
                CborValue::Bytes(vec![0x24; 16]),
            ),
            (CborValue::Integer(21.into()), CborValue::Integer(66.into())),
        ]);

        let err = decode_bootstrap_object_row_cbor(row, Some(4096)).unwrap_err();
        assert!(
            err.to_string().contains("plaintext manifest anchors"),
            "{err}"
        );
    }

    #[test]
    fn object_row_decoder_rejects_duplicate_keys() {
        let row = CborValue::Map(vec![
            (CborValue::Integer(1.into()), CborValue::Integer(1.into())),
            (CborValue::Integer(1.into()), CborValue::Integer(2.into())),
            (
                CborValue::Integer(2.into()),
                CborValue::Text("encrypted".to_string()),
            ),
            (CborValue::Integer(3.into()), CborValue::Integer(4.into())),
            (
                CborValue::Integer(20.into()),
                CborValue::Bytes(vec![0x24; 16]),
            ),
            (CborValue::Integer(21.into()), CborValue::Integer(66.into())),
        ]);

        let err = decode_bootstrap_object_row_cbor(row, Some(4096)).unwrap_err();

        assert!(
            err.to_string().contains("duplicate object row key"),
            "{err}"
        );
    }

    #[test]
    fn writer_rejects_unsorted_object_rows() {
        let mut payload = sample_payload();
        payload.object_rows = vec![
            BootstrapObjectRow::encrypted(3, 11, [0x24; 16], 66),
            BootstrapObjectRow::plaintext(1, 8, 6, 1234, 1, [0xA1; 32]),
        ];
        let mut buf = vec![0; payload.block_size_bytes as usize];

        let err = write_bootstrap_block(&payload, &mut buf).unwrap_err();
        assert!(err.to_string().contains("strictly increasing"), "{err}");
    }

    #[test]
    fn writer_rejects_both_inline_directory_and_parity_map_reference() {
        let mut payload = sample_payload();
        payload.sidecar_epoch_directory = Some(sample_sidecar_directory());
        payload.parity_map_reference = Some(ParityMapReference {
            tape_file_number: 4,
            block_count: 7,
            directory_scope_tape_file_count: 5,
            directory_scope_total_data_ordinals: 3,
            directory_scope_highest_protected_ordinal: 3,
            is_final_directory: true,
            parity_map_payload_sha256: [0x44; 32],
            canonical_map_digest: [0x55; 32],
        });
        let mut buf = vec![0u8; 1_048_576];

        let err = write_bootstrap_block(&payload, &mut buf).unwrap_err();

        assert!(matches!(
            err,
            ParityError::Invariant(message) if message.contains("mutually exclusive")
        ));
    }

    #[test]
    fn header_offsets_and_crc64_ranges_match_v044_table() {
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
            u32::from_be_bytes(buf[0x24..0x28].try_into().unwrap()),
            payload.sequence
        );

        let payload_len = u32::from_le_bytes(buf[0x28..0x2C].try_into().unwrap()) as usize;
        let stored_header_crc = u64::from_le_bytes(buf[0x2C..0x34].try_into().unwrap());
        assert_eq!(stored_header_crc, crc64_xz(&buf[0x00..0x2C]));

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
        buf[44..52].copy_from_slice(&new_crc.to_le_bytes());
        let err = parse_bootstrap_block(&buf[..]).unwrap_err();
        match err {
            ParityError::BootstrapParse(msg) => {
                assert!(msg.contains("missing scheme record"), "{msg}");
            }
            other => panic!("expected BootstrapParse, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_filemark_map_digest() {
        let mut payload = sample_payload();
        payload.filemark_map_digest = Some(FilemarkMapDigest {
            is_final_map: true,
            ..sample_digest()
        });
        let mut buf = vec![0u8; 1_048_576];
        write_bootstrap_block(&payload, &mut buf).expect("write ok");
        let parsed = parse_bootstrap_block(&buf[..]).expect("parse ok");
        assert_eq!(parsed.filemark_map_digest, payload.filemark_map_digest);
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
        buf[40] ^= 0x80; // cbor_payload_len is covered by crc64_header.
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
        buf[44..52].copy_from_slice(&new_crc.to_le_bytes());
        let err = parse_bootstrap_block(&buf[..]).unwrap_err();
        match err {
            ParityError::BootstrapParse(msg) => assert!(msg.contains("major")),
            other => panic!("expected BootstrapParse, got {other:?}"),
        }
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
        buf[44..52].copy_from_slice(&new_crc.to_le_bytes());
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

        fn space_filemarks(&mut self, count: i64) -> Result<SpaceFilemarksOutcome, ParityError> {
            Ok(SpaceFilemarksOutcome {
                filemarks_spaced: count,
                position_after: PhysicalPositionHint::new(self.cursor),
                hit_end_of_data: false,
            })
        }

        fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
            let lba = self.cursor;
            let Some(block) = self.blocks.get(lba as usize) else {
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
                        actual: block.len() as u32,
                        provided: buf.len() as u32,
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

    struct MediumErrorThenBootstrapRawSource {
        bootstrap: Vec<u8>,
        cursor: u64,
        returned_medium_error: bool,
        error: fn() -> TapeIoError,
    }

    impl MediumErrorThenBootstrapRawSource {
        fn new(bootstrap: Vec<u8>) -> Self {
            Self::with_error(bootstrap, medium_error)
        }

        fn with_error(bootstrap: Vec<u8>, error: fn() -> TapeIoError) -> Self {
            Self {
                bootstrap,
                cursor: 0,
                returned_medium_error: false,
                error,
            }
        }
    }

    impl RawTapeSource for MediumErrorThenBootstrapRawSource {
        fn configure_fixed_block_size(&mut self, block_size: u32) -> Result<(), ParityError> {
            if block_size == 0 {
                return Err(ParityError::Invariant("fixed block size is zero"));
            }
            Ok(())
        }

        fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
            self.cursor = hint.lba;
            Ok(())
        }

        fn space_filemarks(&mut self, _count: i64) -> Result<SpaceFilemarksOutcome, ParityError> {
            unreachable!("bootstrap discovery does not space filemarks")
        }

        fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
            if !self.returned_medium_error {
                self.returned_medium_error = true;
                self.cursor = self.cursor.saturating_add(1);
                return Err((self.error)().into());
            }
            buf[..self.bootstrap.len()].copy_from_slice(&self.bootstrap);
            self.cursor = self.cursor.saturating_add(1);
            Ok(RawReadOutcome::Block {
                bytes: self.bootstrap.len(),
                position_after: PhysicalPositionHint::new(self.cursor),
            })
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            Ok(PhysicalPositionHint::new(self.cursor))
        }
    }

    fn medium_error() -> TapeIoError {
        let mut sense = vec![0u8; 18];
        sense[0] = 0x70;
        sense[2] = 0x03;
        sense[7] = 10;
        sense[12] = 0x11;
        TapeIoError::CheckCondition(remanence_library::scsi::ScsiError::CheckCondition {
            sense,
            bytes_transferred: 0,
        })
    }

    fn descriptor_medium_error() -> TapeIoError {
        let mut sense = vec![0u8; 8];
        sense[0] = 0x72;
        sense[1] = 0x03;
        sense[2] = 0x11;
        TapeIoError::CheckCondition(remanence_library::scsi::ScsiError::CheckCondition {
            sense,
            bytes_transferred: 0,
        })
    }

    fn payload_with_scope(
        sequence: u32,
        block_size_bytes: u32,
        map_total_data_ordinals: u64,
        highest_protected_ordinal: u64,
        is_final_map: bool,
    ) -> BootstrapPayload {
        let mut payload = sample_payload();
        payload.sequence = sequence;
        payload.block_size_bytes = block_size_bytes;
        payload.filemark_map_digest = Some(FilemarkMapDigest {
            map_sha256: [sequence as u8; 32],
            tape_file_count: sequence + 1,
            map_total_data_ordinals,
            highest_protected_ordinal,
            is_final_map,
        });
        payload.object_rows = Vec::new();
        payload
    }

    fn encode_payload_block(payload: &BootstrapPayload) -> Vec<u8> {
        let mut buf = vec![0u8; payload.block_size_bytes as usize];
        write_bootstrap_block(payload, &mut buf).expect("bootstrap encodes");
        buf
    }

    #[test]
    fn expected_positions_starts_at_zero() {
        let p = expected_bootstrap_positions(None);
        assert_eq!(p[0], 0);
        assert_eq!(p.len(), 1);
    }

    #[test]
    fn expected_positions_with_hint_includes_fractional_marks() {
        let p = expected_bootstrap_positions(Some(1000));
        // LBA 0 plus ~5% intervals: 50, 100, 150, ..., 950.
        assert_eq!(p[0], 0);
        assert!(p.contains(&50));
        assert!(p.contains(&500));
        assert!(p.contains(&950));
        // No position equals the total (would be past-EOD).
        assert!(!p.contains(&1000));
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
        let mut buf = vec![0u8; payload.block_size_bytes as usize];
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
    fn configured_block_size_discovery_continues_past_bad_copy() {
        const BLOCK_SIZE: u32 = 1024;
        let payload = payload_with_scope(1, BLOCK_SIZE, 8, 8, false);

        let mut blocks = vec![vec![0xCC; BLOCK_SIZE as usize]; 101];
        blocks[0] = vec![0xAA; (BLOCK_SIZE / 2) as usize];
        blocks[50] = encode_payload_block(&payload);
        let mut source = RecordingRawSource::new(blocks);

        let parsed = discover_bootstrap_with_block_size(&mut source, Some(1000), BLOCK_SIZE)
            .expect("bad first copy must not abort configured-size discovery");

        assert_eq!(parsed, payload);
        assert!(
            source.calls.contains(&RecordingRawSourceCall::Locate(50)),
            "discovery should probe the later bootstrap region"
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
    fn discover_skips_medium_error_read_and_finds_later_bootstrap() {
        let payload = sample_payload();
        let block = encode_payload_block(&payload);
        let mut source = MediumErrorThenBootstrapRawSource::new(block);

        let parsed = discover_bootstrap_with_candidate_block_sizes(
            &mut source,
            None,
            &[payload.block_size_bytes],
        )
        .expect("medium error block is skipped and later bootstrap is parsed");

        assert_eq!(parsed, payload);
        assert_eq!(source.cursor, 2);
    }

    #[test]
    fn discover_skips_descriptor_format_medium_error_read() {
        let payload = sample_payload();
        let block = encode_payload_block(&payload);
        let mut source =
            MediumErrorThenBootstrapRawSource::with_error(block, descriptor_medium_error);

        let parsed = discover_bootstrap_with_candidate_block_sizes(
            &mut source,
            None,
            &[payload.block_size_bytes],
        )
        .expect("descriptor-format medium error block is skipped");

        assert_eq!(parsed, payload);
        assert_eq!(source.cursor, 2);
    }

    #[test]
    fn authoritative_discovery_prefers_final_map_over_first_valid_bot_copy() {
        const BLOCK_SIZE: u32 = 1024;
        let bot = payload_with_scope(0, BLOCK_SIZE, 0, 0, false);
        let final_copy = payload_with_scope(2, BLOCK_SIZE, 8, 8, true);

        let mut blocks = vec![vec![0xCC; BLOCK_SIZE as usize]; 101];
        blocks[0] = encode_payload_block(&bot);
        blocks[50] = vec![0xAA; (BLOCK_SIZE / 2) as usize];
        blocks[100] = encode_payload_block(&final_copy);

        let mut first_src = RecordingRawSource::new(blocks.clone());
        let first = discover_bootstrap_with_block_size(&mut first_src, Some(1000), BLOCK_SIZE)
            .expect("first valid bootstrap discovers BOT copy");
        assert_eq!(first.sequence, 0);
        assert!(!first.filemark_map_digest.unwrap().is_final_map);

        let mut authoritative_src = RecordingRawSource::new(blocks);
        let authoritative = discover_authoritative_bootstrap_with_block_size(
            &mut authoritative_src,
            Some(1000),
            BLOCK_SIZE,
        )
        .expect("authoritative discovery scans all expected regions");

        assert_eq!(authoritative, final_copy);
        assert!(authoritative.filemark_map_digest.unwrap().is_final_map);
    }

    #[test]
    fn bootstrap_scope_selection_uses_final_then_sequence_then_ordinal_count() {
        const BLOCK_SIZE: u32 = 1024;

        let nonfinal_higher_sequence = payload_with_scope(9, BLOCK_SIZE, 128, 64, false);
        let final_lower_sequence = payload_with_scope(1, BLOCK_SIZE, 32, 32, true);
        assert_eq!(
            choose_wider_map_scope(nonfinal_higher_sequence, final_lower_sequence.clone()),
            final_lower_sequence
        );

        let older_larger_prefix = payload_with_scope(2, BLOCK_SIZE, 128, 64, false);
        let newer_smaller_prefix = payload_with_scope(3, BLOCK_SIZE, 16, 16, false);
        assert_eq!(
            choose_wider_map_scope(older_larger_prefix, newer_smaller_prefix.clone()),
            newer_smaller_prefix
        );

        let tie_smaller_prefix = payload_with_scope(4, BLOCK_SIZE, 16, 16, false);
        let tie_larger_prefix = payload_with_scope(4, BLOCK_SIZE, 32, 32, false);
        assert_eq!(
            choose_wider_map_scope(tie_smaller_prefix, tie_larger_prefix.clone()),
            tie_larger_prefix
        );
    }

    #[test]
    fn discover_with_wrong_configured_size_reports_short_fixed_block_read() {
        let mut payload = sample_payload();
        payload.block_size_bytes = 256 * 1024;
        let mut buf = vec![0u8; payload.block_size_bytes as usize];
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
    fn discover_falls_back_to_fractional_position_when_lba0_corrupt() {
        // Set up a tape with a corrupt LBA 0 (garbage) and a
        // valid bootstrap at LBA 50 (5% of 1000).
        let payload = sample_payload();
        let mut bootstrap_buf = vec![0u8; 1_048_576];
        write_bootstrap_block(&payload, &mut bootstrap_buf).expect("write ok");
        let mut blocks: Vec<Vec<u8>> = (0..1000)
            .map(|i| {
                if i == 50 {
                    bootstrap_buf.clone()
                } else {
                    vec![0xCCu8; 1_048_576] // garbage
                }
            })
            .collect();
        // Make sure LBA 0 is garbage (the loop above did, but
        // explicit for clarity).
        blocks[0] = vec![0xCCu8; 1_048_576];
        let mut src = VecBlockSource::new(blocks);
        let parsed = {
            let mut raw = BlockSourceRawTapeSource::new(&mut src);
            discover_bootstrap(&mut raw, Some(1000)).expect("discover ok")
        };
        assert_eq!(parsed, payload);
    }

    #[test]
    fn discover_keeps_scanning_window_past_a_bad_magic_hit() {
        // A block at LBA 0 starts with the bootstrap magic but
        // is otherwise garbage (parse-fails). The valid bootstrap
        // lives a few blocks later in the same scan window. The
        // scanner should keep scanning forward through magic hits
        // whose parse fails, not abandon the window.
        let payload = sample_payload();
        let mut good = vec![0u8; 1_048_576];
        write_bootstrap_block(&payload, &mut good).expect("write ok");

        let mut bad_magic = vec![0xFFu8; 1_048_576];
        bad_magic[..BOOTSTRAP_MAGIC.len()].copy_from_slice(&BOOTSTRAP_MAGIC);
        // The CRC slot won't match, so parse_bootstrap_block
        // returns BootstrapParse / CrcHeaderMismatch.

        let blocks = vec![bad_magic.clone(), bad_magic.clone(), good];
        let mut src = VecBlockSource::new(blocks);
        let parsed = {
            let mut raw = BlockSourceRawTapeSource::new(&mut src);
            discover_bootstrap(&mut raw, None).expect("scanner walks past bad magic")
        };
        assert_eq!(parsed, payload);
    }

    #[test]
    fn unknown_cbor_fields_are_ignored_on_decode() {
        // Forward-compat: a future writer adds a field at tag 99.
        // Today's reader should ignore it cleanly.
        let payload = sample_payload();
        let mut buf = vec![0u8; 1_048_576];
        write_bootstrap_block(&payload, &mut buf).expect("write ok");

        // Decode CBOR, add a field, re-encode, re-frame.
        let cbor_len = u32::from_le_bytes(buf[40..44].try_into().unwrap()) as usize;
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
        let new_len: u32 = new_cbor.len() as u32;
        // Rewrite buf with the extended CBOR.
        buf[40..44].copy_from_slice(&new_len.to_le_bytes());
        let new_crc_header = crc64_xz(&buf[0..BOOTSTRAP_HEADER_CRC_OFFSET]);
        buf[44..52].copy_from_slice(&new_crc_header.to_le_bytes());
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
