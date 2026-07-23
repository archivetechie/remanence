//! Whole-object RAO encrypted sealing.

use std::io::{Read, Write};

use sha2::{Digest, Sha256};

use crate::error::{RaoAeadError, Result};
use crate::header::{validate_chunk_size, RaoHeader, RAO_FOOTER};
use crate::metadata::RaoMetadata;
use crate::stream::{
    encrypt_chunk, encrypt_metadata, finalize_sha256, stored_size_from_parts, PlaintextStats,
};
use crate::wrap::{wrap_dek, DataEncryptionKey, EphemeralRng, RecipientPublicKey};

/// Inputs to the RAO sealer.
#[derive(Debug, Clone)]
pub struct SealOptions {
    /// Body block size and AEAD plaintext chunk size.
    pub chunk_size: u32,
    /// Inner canonical object id.
    pub object_id: String,
    /// Expected canonical plaintext size.
    pub plaintext_size: u64,
    /// Expected SHA-256 of the canonical plaintext bytes.
    pub plaintext_digest: [u8; 32],
}

/// Report returned after successful sealing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealReport {
    /// Serialized header used for this object.
    pub header: RaoHeader,
    /// Parsed key frame.
    pub key_frame: crate::KeyFrame,
    /// Metadata plaintext size before the AEAD tag.
    pub metadata_plaintext_len: u64,
    /// Metadata frame size including the AEAD tag.
    pub metadata_frame_len: u64,
    /// Stored byte length after footer and fill.
    pub stored_size_bytes: u64,
    /// Stored block count.
    pub stored_size_blocks: u64,
    /// SHA-256 over the complete stored encrypted object.
    pub stored_digest: [u8; 32],
    /// Plaintext size and digest observed while sealing.
    pub plaintext: PlaintextStats,
}

/// Inputs unique to an envelope-mode seal.
#[derive(Debug, Clone)]
pub struct EnvelopeSealOptions {
    /// Common framing and plaintext facts.
    pub common: SealOptions,
    /// Permit exactly one recipient instead of the safe default of at least two.
    pub allow_single_recipient: bool,
    /// Distinct-custody recipient epochs in canonical slot order.
    pub recipients: Vec<RecipientPublicKey>,
}

/// Seal a canonical plaintext RAO object as an HPKE envelope.
pub fn seal<R: Read, W: Write>(
    plaintext: R,
    output: W,
    options: &EnvelopeSealOptions,
) -> Result<SealReport> {
    let dek = DataEncryptionKey::generate()?;
    let mut rng = EphemeralRng::from_os()?;
    seal_with_material(plaintext, output, options, &dek, &mut rng)
}

/// Seal a byte-identical envelope from fixed test-vector key material.
///
/// This entry point exists only to generate reproducible conformance test
/// vectors. Production callers must use [`seal`], which obtains the
/// DEK and HPKE randomness from operating-system entropy.
pub fn seal_deterministic_for_test_vectors<R: Read, W: Write>(
    plaintext: R,
    output: W,
    options: &EnvelopeSealOptions,
    dek: DataEncryptionKey,
    hpke_rng_seed: [u8; 32],
) -> Result<SealReport> {
    let mut rng = EphemeralRng::from_seed(&hpke_rng_seed);
    seal_with_material(plaintext, output, options, &dek, &mut rng)
}

fn seal_with_material<R, W, G>(
    mut plaintext: R,
    mut output: W,
    options: &EnvelopeSealOptions,
    dek: &DataEncryptionKey,
    rng: &mut G,
) -> Result<SealReport>
where
    R: Read,
    W: Write,
    G: rand_core::CryptoRng + rand_core::RngCore,
{
    validate_chunk_size(options.common.chunk_size)?;
    crate::header::object_id_field(&options.common.object_id)?;
    let chunk = u64::from(options.common.chunk_size);
    if options.common.plaintext_size == 0 || options.common.plaintext_size % chunk != 0 {
        return Err(RaoAeadError::InvalidInput(
            "envelope seal requires non-empty, chunk-aligned plaintext".to_string(),
        ));
    }
    match options.recipients.len() {
        0 => {
            return Err(RaoAeadError::InvalidInput(
                "envelope seal requires at least one recipient".to_string(),
            ))
        }
        1 if !options.allow_single_recipient => return Err(RaoAeadError::InvalidInput(
            "single-recipient envelope seal requires the allow_single_recipient safety override"
                .to_string(),
        )),
        _ => {}
    }
    if options
        .recipients
        .iter()
        .enumerate()
        .any(|(index, recipient)| {
            options.recipients[..index]
                .iter()
                .any(|earlier| earlier.recipient_epoch_id == recipient.recipient_epoch_id)
        })
    {
        return Err(RaoAeadError::InvalidInput(
            "recipient epochs must be distinct".to_string(),
        ));
    }

    let metadata = RaoMetadata::new(
        options.common.plaintext_size,
        options.common.plaintext_digest,
        options.common.chunk_size,
    )?;
    let metadata_plaintext = metadata.to_cbor_bytes(options.common.chunk_size)?;
    let metadata_frame_len = (metadata_plaintext.len() as u64)
        .checked_add(16)
        .ok_or(RaoAeadError::SizeOverflow)?;
    let object_id_field = crate::header::object_id_field(&options.common.object_id)?;
    let salt = crate::kdf::derive_salt(
        dek.as_bytes(),
        &object_id_field,
        &options.common.plaintext_digest,
        &metadata_plaintext,
    )?;
    let key_frame = wrap_dek(dek, &options.common.object_id, &options.recipients, rng)?;
    let key_frame_bytes = key_frame.serialize()?;
    let key_frame_len =
        u32::try_from(key_frame_bytes.len()).map_err(|_| RaoAeadError::InvalidKeyFrameLength)?;
    let header = RaoHeader::new_envelope(
        options.common.chunk_size,
        salt,
        metadata_frame_len,
        options.common.object_id.clone(),
        key_frame_len,
    )?;
    let keys = crate::kdf::derive_keys(
        dek.as_bytes(),
        &header.hkdf_salt,
        &header.header_hash_with_key_frame(&key_frame_bytes)?,
    )?;
    let metadata_frame = encrypt_metadata(&keys.metadata_key, &metadata_plaintext)?;

    let mut hashing_output = HashingWriter::new(&mut output);
    hashing_output.write_all(&header.serialize()?)?;
    hashing_output.write_all(&key_frame_bytes)?;
    hashing_output.write_all(&metadata_frame)?;
    let plaintext_stats =
        encrypt_payload(&mut plaintext, &mut hashing_output, &options.common, &keys)?;
    if plaintext_stats.size != options.common.plaintext_size {
        return Err(RaoAeadError::PlaintextSizeMismatch);
    }
    if plaintext_stats.digest != options.common.plaintext_digest {
        return Err(RaoAeadError::PlaintextDigestMismatch);
    }
    ensure_eof(&mut plaintext)?;
    hashing_output.write_all(RAO_FOOTER)?;
    let stored_size_bytes = stored_size_from_parts(
        options.common.chunk_size,
        key_frame_len,
        metadata_frame_len,
        options.common.plaintext_size,
    )?;
    let fill_len = stored_size_bytes
        .checked_sub(hashing_output.count)
        .ok_or(RaoAeadError::SizeOverflow)?;
    write_zero_fill(&mut hashing_output, fill_len)?;
    let (_, stored_len, stored_digest) = hashing_output.finish();
    if stored_len != stored_size_bytes {
        return Err(RaoAeadError::SizeOverflow);
    }
    Ok(SealReport {
        header,
        key_frame,
        metadata_plaintext_len: metadata_plaintext.len() as u64,
        metadata_frame_len,
        stored_size_bytes,
        stored_size_blocks: stored_size_bytes / u64::from(options.common.chunk_size),
        stored_digest,
        plaintext: plaintext_stats,
    })
}

/// Seal an envelope into a newly allocated vector.
pub fn seal_to_vec(
    plaintext: &[u8],
    options: &EnvelopeSealOptions,
) -> Result<(Vec<u8>, SealReport)> {
    let mut out = Vec::new();
    let report = seal(plaintext, &mut out, options)?;
    Ok((out, report))
}

fn encrypt_payload<R: Read, W: Write>(
    plaintext: &mut R,
    output: &mut W,
    options: &SealOptions,
    keys: &crate::kdf::DerivedKeys,
) -> Result<PlaintextStats> {
    let chunk_size =
        usize::try_from(options.chunk_size).map_err(|_| RaoAeadError::InvalidChunkSize)?;
    let chunk_count = options.plaintext_size / u64::from(options.chunk_size);
    let mut hasher = Sha256::new();
    let mut count = 0u64;
    let mut buf = vec![0u8; chunk_size];

    for index in 0..chunk_count {
        read_exact(plaintext, &mut buf)?;
        hasher.update(&buf);
        count = count
            .checked_add(u64::from(options.chunk_size))
            .ok_or(RaoAeadError::PlaintextSizeMismatch)?;
        let final_chunk = index + 1 == chunk_count;
        let encrypted = encrypt_chunk(&keys.payload_key, index, final_chunk, &buf)?;
        output.write_all(&encrypted)?;
    }

    Ok(PlaintextStats {
        size: count,
        digest: finalize_sha256(hasher),
    })
}

fn ensure_eof<R: Read>(reader: &mut R) -> Result<()> {
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => return Ok(()),
            Ok(_) => return Err(RaoAeadError::PlaintextSizeMismatch),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(RaoAeadError::Io(err)),
        }
    }
}

fn read_exact<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<()> {
    reader
        .read_exact(buf)
        .map_err(crate::error::map_read_exact_error)
}

fn write_zero_fill<W: Write>(writer: &mut W, fill_len: u64) -> Result<()> {
    let mut remaining = fill_len;
    let zeros = [0u8; 8192];
    while remaining > 0 {
        let take = remaining.min(zeros.len() as u64) as usize;
        writer.write_all(&zeros[..take])?;
        remaining -= take as u64;
    }
    Ok(())
}

struct HashingWriter<W> {
    inner: W,
    hasher: Sha256,
    count: u64,
}

impl<W> HashingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            count: 0,
        }
    }

    fn finish(self) -> (W, u64, [u8; 32]) {
        (self.inner, self.count, finalize_sha256(self.hasher))
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        let written = buf
            .get(..n)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "short write"))?;
        self.hasher.update(written);
        self.count = self
            .count
            .checked_add(n as u64)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "size overflow"))?;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{inspect_bytes, open_plaintext_range_to_vec, open_to_vec, RecipientPrivateKey};
    use std::io;

    fn options(plaintext: &[u8]) -> SealOptions {
        options_with_object_id(plaintext, "object-1")
    }

    fn options_with_object_id(plaintext: &[u8], object_id: &str) -> SealOptions {
        let digest = Sha256::digest(plaintext);
        let mut plaintext_digest = [0u8; 32];
        plaintext_digest.copy_from_slice(&digest);
        SealOptions {
            chunk_size: 512,
            object_id: object_id.to_string(),
            plaintext_size: plaintext.len() as u64,
            plaintext_digest,
        }
    }

    fn assert_no_footer(output: &[u8]) {
        assert!(
            !output
                .windows(RAO_FOOTER.len())
                .any(|window| window == RAO_FOOTER),
            "failed seal wrote the completion footer"
        );
    }

    fn envelope_options(common: SealOptions) -> EnvelopeSealOptions {
        let primary = RecipientPrivateKey::new([1; 16], "primary", [7; 32]).unwrap();
        let recovery = RecipientPrivateKey::new([2; 16], "recovery", [8; 32]).unwrap();
        EnvelopeSealOptions {
            common,
            allow_single_recipient: false,
            recipients: vec![
                primary.public_key(0).unwrap(),
                recovery.public_key(1).unwrap(),
            ],
        }
    }

    struct FailAfterWriter {
        bytes: Vec<u8>,
        fail_after: usize,
    }

    impl FailAfterWriter {
        fn new(fail_after: usize) -> Self {
            Self {
                bytes: Vec::new(),
                fail_after,
            }
        }
    }

    impl io::Write for FailAfterWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.bytes.len() >= self.fail_after {
                return Err(io::Error::other("injected write failure"));
            }
            let remaining = self.fail_after - self.bytes.len();
            let to_write = remaining.min(buf.len());
            self.bytes.extend_from_slice(&buf[..to_write]);
            Ok(to_write)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn single_recipient_is_rejected_by_default() {
        let plaintext = vec![0x5a; 512];
        let recipient = RecipientPrivateKey::new([1; 16], "primary", [7; 32]).unwrap();
        let options = EnvelopeSealOptions {
            common: options(&plaintext),
            allow_single_recipient: false,
            recipients: vec![recipient.public_key(0).unwrap()],
        };

        let error = seal_to_vec(&plaintext, &options).unwrap_err();
        assert!(matches!(error, RaoAeadError::InvalidInput(_)));
        assert_eq!(
            error.to_string(),
            "invalid RAO sealing input: single-recipient envelope seal requires the \
             allow_single_recipient safety override"
        );
    }

    #[test]
    fn single_recipient_override_seals_and_round_trips() {
        let plaintext = vec![0x5a; 512];
        let recipient = RecipientPrivateKey::new([1; 16], "primary", [7; 32]).unwrap();
        let options = EnvelopeSealOptions {
            common: options(&plaintext),
            allow_single_recipient: true,
            recipients: vec![recipient.public_key(0).unwrap()],
        };

        let (sealed, sealed_report) = seal_to_vec(&plaintext, &options).unwrap();
        assert_eq!(sealed_report.key_frame.slots.len(), 1);
        let (opened, open_report) = open_to_vec(&sealed, &recipient).unwrap();
        assert_eq!(open_report.key_frame.slots.len(), 1);
        assert_eq!(opened, plaintext);
    }

    #[test]
    fn two_recipients_seal_by_default() {
        let plaintext = vec![0x5a; 512];
        let options = envelope_options(options(&plaintext));

        let (_, report) = seal_to_vec(&plaintext, &options).unwrap();
        assert_eq!(report.key_frame.slots.len(), 2);
    }

    #[test]
    fn envelope_seal_open_range_and_inspect() {
        let plaintext: Vec<u8> = (0..1536).map(|index| (index % 251) as u8).collect();
        let common = options(&plaintext);
        let safe = RecipientPrivateKey::new([0x31; 16], "safe-2026", [7; 32]).unwrap();
        let escrow = RecipientPrivateKey::new([0x32; 16], "escrow-2026", [8; 32]).unwrap();
        let options = EnvelopeSealOptions {
            common,
            allow_single_recipient: false,
            recipients: vec![safe.public_key(0).unwrap(), escrow.public_key(1).unwrap()],
        };

        let (sealed, sealed_report) = seal_to_vec(&plaintext, &options).unwrap();
        assert_eq!(sealed_report.header.format_version, 2);
        assert_eq!(sealed_report.key_frame.slots.len(), 2);

        let inspected = inspect_bytes(&sealed).unwrap();
        assert_eq!(inspected.header, sealed_report.header);
        assert_eq!(inspected.key_frame, sealed_report.key_frame);
        assert_eq!(inspected.chunk_count, 3);
        assert_eq!(inspected.stored_size_bytes, sealed.len() as u64);

        let (opened_safe, safe_report) = open_to_vec(&sealed, &safe).unwrap();
        let (opened_escrow, _) = open_to_vec(&sealed, &escrow).unwrap();
        assert_eq!(opened_safe, plaintext);
        assert_eq!(opened_escrow, plaintext);
        assert_eq!(safe_report.header, sealed_report.header);

        let (range, range_report) = open_plaintext_range_to_vec(&sealed, &safe, 400, 700).unwrap();
        assert_eq!(range, plaintext[400..1100]);
        assert_eq!(range_report.first_chunk, Some(0));
        assert_eq!(range_report.chunk_count, 3);

        let mut unrequested_chunk_corrupt = sealed.clone();
        let third_chunk = crate::cipher_offset(
            sealed_report.header.key_frame_len,
            sealed_report.header.metadata_frame_len,
            sealed_report.header.chunk_size,
            2,
        )
        .unwrap() as usize;
        unrequested_chunk_corrupt[third_chunk] ^= 0x80;
        assert!(matches!(
            open_to_vec(&unrequested_chunk_corrupt, &safe),
            Err(RaoAeadError::AeadAuthenticationFailed)
        ));
        let (first_chunk_only, partial_report) =
            open_plaintext_range_to_vec(&unrequested_chunk_corrupt, &safe, 0, 512).unwrap();
        assert_eq!(first_chunk_only, plaintext[..512]);
        assert_eq!(partial_report.chunk_count, 1);

        let wrong_epoch = RecipientPrivateKey::new([0x33; 16], "wrong", [9; 32]).unwrap();
        assert!(matches!(
            open_to_vec(&sealed, &wrong_epoch),
            Err(RaoAeadError::RecipientEpochMismatch)
        ));

        let mut malformed_encapsulation = sealed.clone();
        let first_enc = crate::RAO_HEADER_LEN + 5 + 1 + 16 + 1 + "safe-2026".len();
        malformed_encapsulation[first_enc..first_enc + 32].fill(0);
        assert!(matches!(
            open_to_vec(&malformed_encapsulation, &safe),
            Err(RaoAeadError::HpkeFailed)
        ));

        let mut changed_label = sealed;
        let first_label = crate::RAO_HEADER_LEN + 5 + 1 + 16 + 1;
        changed_label[first_label] = b'S';
        assert!(matches!(
            open_to_vec(&changed_label, &safe),
            Err(RaoAeadError::AeadAuthenticationFailed)
        ));
    }

    #[test]
    fn envelope_rejects_nonadjacent_duplicate_epoch() {
        let plaintext = vec![0x5a; 512];
        let common = options(&plaintext);
        let first = RecipientPrivateKey::new([1; 16], "safe", [7; 32]).unwrap();
        let second = RecipientPrivateKey::new([2; 16], "escrow", [8; 32]).unwrap();
        let duplicate = RecipientPrivateKey::new([1; 16], "safe-copy", [9; 32]).unwrap();
        let options = EnvelopeSealOptions {
            common,
            allow_single_recipient: false,
            recipients: vec![
                first.public_key(0).unwrap(),
                second.public_key(1).unwrap(),
                duplicate.public_key(2).unwrap(),
            ],
        };
        assert!(matches!(
            seal_to_vec(&plaintext, &options),
            Err(RaoAeadError::InvalidInput(_))
        ));
    }

    #[test]
    fn deterministic_seal_matches_checked_in_expectation() {
        let plaintext = vec![0x5a; 512];
        let common = options_with_object_id(&plaintext, "deterministic-vector");
        let primary = RecipientPrivateKey::new([0x11; 16], "primary", [0x31; 32]).unwrap();
        let recovery = RecipientPrivateKey::new([0x22; 16], "recovery", [0x32; 32]).unwrap();
        let options = EnvelopeSealOptions {
            common,
            allow_single_recipient: false,
            recipients: vec![
                primary.public_key(0).unwrap(),
                recovery.public_key(1).unwrap(),
            ],
        };
        let mut sealed = Vec::new();
        seal_deterministic_for_test_vectors(
            &plaintext[..],
            &mut sealed,
            &options,
            DataEncryptionKey::from_bytes([0x44; 32]),
            [0x55; 32],
        )
        .unwrap();
        let expected_hex = include_str!("../testdata/deterministic-seal.hex").trim();
        let encoded = sealed
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(encoded, expected_hex);
        let (opened, _) = open_to_vec(&sealed, &primary).unwrap();
        assert_eq!(opened, plaintext);
    }

    #[test]
    fn failed_seal_paths_do_not_write_completion_footer() {
        let plaintext = vec![0x5a; 1024];
        let options = envelope_options(options(&plaintext));

        let mut bad_digest_options = options.clone();
        bad_digest_options.common.plaintext_digest[0] ^= 0x01;
        let mut digest_mismatch_output = Vec::new();
        assert!(matches!(
            seal(
                plaintext.as_slice(),
                &mut digest_mismatch_output,
                &bad_digest_options,
            ),
            Err(RaoAeadError::PlaintextDigestMismatch)
        ));
        assert_no_footer(&digest_mismatch_output);

        let mut oversized_plaintext = plaintext.clone();
        oversized_plaintext.push(0x01);
        let mut size_mismatch_output = Vec::new();
        assert!(matches!(
            seal(
                oversized_plaintext.as_slice(),
                &mut size_mismatch_output,
                &options,
            ),
            Err(RaoAeadError::PlaintextSizeMismatch)
        ));
        assert_no_footer(&size_mismatch_output);

        let mut failing_output = FailAfterWriter::new(256);
        assert!(matches!(
            seal(plaintext.as_slice(), &mut failing_output, &options),
            Err(RaoAeadError::Io(_))
        ));
        assert_no_footer(&failing_output.bytes);
    }

    #[test]
    fn seal_refuses_extra_plaintext_bytes() {
        let mut plaintext = vec![0x5a; 1024];
        let options = envelope_options(options(&plaintext));
        plaintext.push(1);
        assert!(matches!(
            seal_to_vec(&plaintext, &options),
            Err(RaoAeadError::PlaintextSizeMismatch)
        ));
    }
}
