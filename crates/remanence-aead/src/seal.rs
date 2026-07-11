//! Whole-object RAO encrypted sealing.

use std::io::{Read, Write};

use sha2::{Digest, Sha256};

use crate::error::{RaoAeadError, Result};
use crate::header::{validate_chunk_size, RaoHeader, RAO_FOOTER};
use crate::kdf::{derive_keys, derive_salt, RootKey};
use crate::metadata::RaoMetadata;
use crate::stream::{
    encrypt_chunk, encrypt_metadata, finalize_sha256, stored_size_from_parts,
    stored_size_from_parts_with_key_frame, PlaintextStats,
};
use crate::wrap::{wrap_dek, DataEncryptionKey, RecipientPublicKey};

/// Inputs to the RAO sealer.
#[derive(Debug, Clone)]
pub struct SealOptions {
    /// Body block size and AEAD plaintext chunk size.
    pub chunk_size: u32,
    /// Opaque 16-byte key identifier.
    pub key_id: [u8; 16],
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
    /// Parsed v2 key frame, absent for v1 objects.
    pub key_frame: Option<crate::KeyFrame>,
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

/// Seal a canonical plaintext RAO object into the encrypted representation.
pub fn seal<R: Read, W: Write>(
    mut plaintext: R,
    mut output: W,
    root_key: &RootKey,
    options: &SealOptions,
) -> Result<SealReport> {
    validate_options(options)?;

    let metadata = RaoMetadata::new(
        options.plaintext_size,
        options.plaintext_digest,
        options.chunk_size,
    )?;
    let metadata_plaintext = metadata.to_cbor_bytes(options.chunk_size)?;
    let metadata_frame_len = u64::try_from(metadata_plaintext.len())
        .ok()
        .and_then(|len| len.checked_add(16))
        .ok_or(RaoAeadError::SizeOverflow)?;
    let object_id_field = crate::header::object_id_field(&options.object_id)?;
    let salt = derive_salt(
        root_key,
        &object_id_field,
        &options.plaintext_digest,
        &metadata_plaintext,
    )?;
    let header = RaoHeader::new(
        options.chunk_size,
        options.key_id,
        salt,
        metadata_frame_len,
        options.object_id.clone(),
    )?;
    let keys = derive_keys(root_key, &header.hkdf_salt, &header.header_hash()?)?;
    let metadata_frame = encrypt_metadata(&keys.metadata_key, &metadata_plaintext)?;
    if metadata_frame.len() as u64 != metadata_frame_len {
        return Err(RaoAeadError::MetadataFrameLengthInvalid);
    }

    let mut hashing_output = HashingWriter::new(&mut output);
    hashing_output.write_all(&header.serialize()?)?;
    hashing_output.write_all(&metadata_frame)?;

    let plaintext_stats = encrypt_payload(&mut plaintext, &mut hashing_output, options, &keys)?;
    if plaintext_stats.size != options.plaintext_size {
        return Err(RaoAeadError::PlaintextSizeMismatch);
    }
    if plaintext_stats.digest != options.plaintext_digest {
        return Err(RaoAeadError::PlaintextDigestMismatch);
    }
    ensure_eof(&mut plaintext)?;

    hashing_output.write_all(RAO_FOOTER)?;
    let stored_size_bytes = stored_size_from_parts(
        options.chunk_size,
        metadata_frame_len,
        options.plaintext_size,
    )?;
    let current_len = hashing_output.count;
    let fill_len = stored_size_bytes
        .checked_sub(current_len)
        .ok_or(RaoAeadError::SizeOverflow)?;
    write_zero_fill(&mut hashing_output, fill_len)?;
    let (_, stored_len, stored_digest) = hashing_output.finish();
    if stored_len != stored_size_bytes {
        return Err(RaoAeadError::SizeOverflow);
    }

    Ok(SealReport {
        header,
        key_frame: None,
        metadata_plaintext_len: metadata_plaintext.len() as u64,
        metadata_frame_len,
        stored_size_bytes,
        stored_size_blocks: stored_size_bytes / u64::from(options.chunk_size),
        stored_digest,
        plaintext: plaintext_stats,
    })
}

/// Inputs unique to a v2 envelope-mode seal.
#[derive(Debug, Clone)]
pub struct EnvelopeSealOptions {
    /// Common framing and plaintext facts; `key_id` must be zero.
    pub common: SealOptions,
    /// At least two distinct-custody recipient epochs in canonical slot order.
    pub recipients: Vec<RecipientPublicKey>,
}

/// Seal a canonical plaintext RAO object as a v2 HPKE envelope.
pub fn seal_envelope<R: Read, W: Write>(
    mut plaintext: R,
    mut output: W,
    options: &EnvelopeSealOptions,
) -> Result<SealReport> {
    validate_chunk_size(options.common.chunk_size)?;
    crate::header::object_id_field(&options.common.object_id)?;
    let chunk = u64::from(options.common.chunk_size);
    if options.common.key_id != [0; 16]
        || options.common.plaintext_size == 0
        || options.common.plaintext_size % chunk != 0
        || options.recipients.len() < 2
    {
        return Err(RaoAeadError::InvalidInput(
            "v2 envelope seal requires zero key_id, aligned plaintext, and at least two recipients"
                .to_string(),
        ));
    }
    if options
        .recipients
        .windows(2)
        .any(|pair| pair[0].recipient_epoch_id == pair[1].recipient_epoch_id)
    {
        return Err(RaoAeadError::InvalidInput(
            "recipient epochs must be distinct".to_string(),
        ));
    }

    let dek = DataEncryptionKey::generate()?;
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
    let salt = crate::kdf::derive_salt_v2(
        dek.as_bytes(),
        &object_id_field,
        &options.common.plaintext_digest,
        &metadata_plaintext,
    )?;
    let key_frame = wrap_dek(&dek, &options.common.object_id, &options.recipients)?;
    let key_frame_bytes = key_frame.serialize()?;
    let key_frame_len =
        u32::try_from(key_frame_bytes.len()).map_err(|_| RaoAeadError::InvalidKeyFrameLength)?;
    let header = RaoHeader::new_v2_envelope(
        options.common.chunk_size,
        salt,
        metadata_frame_len,
        options.common.object_id.clone(),
        key_frame_len,
    )?;
    let keys = crate::kdf::derive_keys_v2(
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
    let stored_size_bytes = stored_size_from_parts_with_key_frame(
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
        key_frame: Some(key_frame),
        metadata_plaintext_len: metadata_plaintext.len() as u64,
        metadata_frame_len,
        stored_size_bytes,
        stored_size_blocks: stored_size_bytes / u64::from(options.common.chunk_size),
        stored_digest,
        plaintext: plaintext_stats,
    })
}

/// Seal a v2 envelope into a newly allocated vector.
pub fn seal_envelope_to_vec(
    plaintext: &[u8],
    options: &EnvelopeSealOptions,
) -> Result<(Vec<u8>, SealReport)> {
    let mut out = Vec::new();
    let report = seal_envelope(plaintext, &mut out, options)?;
    Ok((out, report))
}

/// Seal into a newly allocated vector, for tests and file-object builders.
pub fn seal_to_vec(
    plaintext: &[u8],
    root_key: &RootKey,
    options: &SealOptions,
) -> Result<(Vec<u8>, SealReport)> {
    let mut out = Vec::new();
    let report = seal(plaintext, &mut out, root_key, options)?;
    Ok((out, report))
}

fn validate_options(options: &SealOptions) -> Result<()> {
    validate_chunk_size(options.chunk_size)?;
    if options.key_id == [0; 16] {
        return Err(RaoAeadError::InvalidKeyIdentifier);
    }
    crate::header::object_id_field(&options.object_id)?;
    let chunk = u64::from(options.chunk_size);
    if options.plaintext_size == 0 || options.plaintext_size % chunk != 0 {
        return Err(RaoAeadError::InvalidInput(
            "plaintext_size must be a positive multiple of chunk_size".to_string(),
        ));
    }
    Ok(())
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
    use crate::open_to_vec;
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
            key_id: [0x10; 16],
            object_id: object_id.to_string(),
            plaintext_size: plaintext.len() as u64,
            plaintext_digest,
        }
    }

    fn derived_key_tuple(root: &RootKey, report: &SealReport) -> ([u8; 32], [u8; 32], [u8; 32]) {
        let keys = derive_keys(
            root,
            &report.header.hkdf_salt,
            &report.header.header_hash().unwrap(),
        )
        .unwrap();
        (keys.object_secret, keys.metadata_key, keys.payload_key)
    }

    fn seal_with_salt_override(
        plaintext: &[u8],
        root: &RootKey,
        options: &SealOptions,
        salt: [u8; 16],
    ) -> Vec<u8> {
        let metadata = RaoMetadata::new(
            options.plaintext_size,
            options.plaintext_digest,
            options.chunk_size,
        )
        .unwrap();
        let metadata_plaintext = metadata.to_cbor_bytes(options.chunk_size).unwrap();
        let metadata_frame_len = metadata_plaintext.len() as u64 + 16;
        let header = RaoHeader::new(
            options.chunk_size,
            options.key_id,
            salt,
            metadata_frame_len,
            options.object_id.clone(),
        )
        .unwrap();
        let keys = derive_keys(root, &header.hkdf_salt, &header.header_hash().unwrap()).unwrap();
        let chunk_size = options.chunk_size as usize;
        assert_eq!(plaintext.len() as u64, options.plaintext_size);
        assert_eq!(plaintext.len() % chunk_size, 0);
        let chunk_count = options.plaintext_size / u64::from(options.chunk_size);
        let mut out = Vec::new();
        out.extend_from_slice(&header.serialize().unwrap());
        out.extend_from_slice(&encrypt_metadata(&keys.metadata_key, &metadata_plaintext).unwrap());
        for (index, chunk) in plaintext.chunks_exact(chunk_size).enumerate() {
            out.extend_from_slice(
                &encrypt_chunk(
                    &keys.payload_key,
                    index as u64,
                    index as u64 + 1 == chunk_count,
                    chunk,
                )
                .unwrap(),
            );
        }
        out.extend_from_slice(RAO_FOOTER);
        let stored_size = stored_size_from_parts(
            options.chunk_size,
            metadata_frame_len,
            options.plaintext_size,
        )
        .unwrap() as usize;
        out.resize(stored_size, 0);
        out
    }

    fn footer_offset_for(options: &SealOptions) -> usize {
        let metadata = RaoMetadata::new(
            options.plaintext_size,
            options.plaintext_digest,
            options.chunk_size,
        )
        .unwrap();
        let metadata_plaintext = metadata.to_cbor_bytes(options.chunk_size).unwrap();
        let metadata_frame_len = metadata_plaintext.len() + 16;
        let chunk_count = options.plaintext_size / u64::from(options.chunk_size);
        crate::header::RAO_HEADER_LEN
            + metadata_frame_len
            + (chunk_count as usize * (options.chunk_size as usize + 16))
    }

    fn assert_no_footer_at_derived_offset(output: &[u8], options: &SealOptions) {
        let footer_offset = footer_offset_for(options);
        assert!(
            output.len() <= footer_offset,
            "failed seal wrote beyond derived footer offset: len={}, footer_offset={footer_offset}",
            output.len()
        );
        assert_ne!(
            output.get(footer_offset..footer_offset + RAO_FOOTER.len()),
            Some(&RAO_FOOTER[..]),
            "failed seal wrote the completion footer"
        );
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
    fn seal_is_deterministic_and_opens() {
        let root = RootKey::new([0x11; 32]).unwrap();
        let plaintext = vec![0x5a; 1024];
        let options = options(&plaintext);
        let (first, first_report) = seal_to_vec(&plaintext, &root, &options).unwrap();
        let (second, second_report) = seal_to_vec(&plaintext, &root, &options).unwrap();
        assert_eq!(first, second);
        assert_eq!(first_report.stored_digest, second_report.stored_digest);
        assert_eq!(first.len() as u64, first_report.stored_size_bytes);
        assert_eq!(first.len() % options.chunk_size as usize, 0);

        let (opened, open_report) = open_to_vec(&first, &root).unwrap();
        assert_eq!(opened, plaintext);
        assert_eq!(
            open_report.metadata.plaintext_digest,
            options.plaintext_digest
        );
    }

    #[test]
    fn salt_derivation_conformance_covers_epoch_content_and_object_id() {
        let root = RootKey::new([0x11; 32]).unwrap();
        let plaintext = vec![0x5a; 1024];
        let same_options = options(&plaintext);
        let (first, first_report) = seal_to_vec(&plaintext, &root, &same_options).unwrap();
        let (second, second_report) = seal_to_vec(&plaintext, &root, &same_options).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            first_report.header.hkdf_salt,
            second_report.header.hkdf_salt
        );
        assert_eq!(
            derived_key_tuple(&root, &first_report),
            derived_key_tuple(&root, &second_report)
        );

        let mut changed_content = plaintext.clone();
        changed_content[0] ^= 0x01;
        let changed_content_options = options(&changed_content);
        let (content_variant, content_report) =
            seal_to_vec(&changed_content, &root, &changed_content_options).unwrap();
        assert_ne!(
            first_report.header.hkdf_salt,
            content_report.header.hkdf_salt
        );
        assert_ne!(
            derived_key_tuple(&root, &first_report),
            derived_key_tuple(&root, &content_report)
        );
        assert_ne!(first, content_variant);
        assert_ne!(first_report.stored_digest, content_report.stored_digest);

        let changed_object_id_options = options_with_object_id(&plaintext, "object-2");
        let (object_id_variant, object_id_report) =
            seal_to_vec(&plaintext, &root, &changed_object_id_options).unwrap();
        assert_ne!(
            first_report.header.hkdf_salt,
            object_id_report.header.hkdf_salt
        );
        assert_ne!(
            derived_key_tuple(&root, &first_report),
            derived_key_tuple(&root, &object_id_report)
        );
        assert_ne!(first, object_id_variant);
        assert_ne!(first_report.stored_digest, object_id_report.stored_digest);

        let mut non_derived_salt = [0x77; 16];
        if non_derived_salt == first_report.header.hkdf_salt {
            non_derived_salt[0] ^= 0x01;
        }
        let nonconformant =
            seal_with_salt_override(&plaintext, &root, &same_options, non_derived_salt);
        assert!(matches!(
            open_to_vec(&nonconformant, &root),
            Err(RaoAeadError::SaltDerivationMismatch)
        ));
    }

    #[test]
    fn failed_seal_paths_do_not_write_completion_footer() {
        let root = RootKey::new([0x11; 32]).unwrap();
        let plaintext = vec![0x5a; 1024];
        let options = options(&plaintext);

        let mut bad_digest_options = options.clone();
        bad_digest_options.plaintext_digest[0] ^= 0x01;
        let mut digest_mismatch_output = Vec::new();
        assert!(matches!(
            seal(
                plaintext.as_slice(),
                &mut digest_mismatch_output,
                &root,
                &bad_digest_options,
            ),
            Err(RaoAeadError::PlaintextDigestMismatch)
        ));
        assert_no_footer_at_derived_offset(&digest_mismatch_output, &bad_digest_options);

        let mut oversized_plaintext = plaintext.clone();
        oversized_plaintext.push(0x01);
        let mut size_mismatch_output = Vec::new();
        assert!(matches!(
            seal(
                oversized_plaintext.as_slice(),
                &mut size_mismatch_output,
                &root,
                &options,
            ),
            Err(RaoAeadError::PlaintextSizeMismatch)
        ));
        assert_no_footer_at_derived_offset(&size_mismatch_output, &options);

        let mut failing_output = FailAfterWriter::new(footer_offset_for(&options));
        assert!(matches!(
            seal(plaintext.as_slice(), &mut failing_output, &root, &options),
            Err(RaoAeadError::Io(_))
        ));
        assert_no_footer_at_derived_offset(&failing_output.bytes, &options);
    }

    #[test]
    fn seal_refuses_extra_plaintext_bytes() {
        let root = RootKey::new([0x11; 32]).unwrap();
        let mut plaintext = vec![0x5a; 1024];
        let options = options(&plaintext);
        plaintext.push(1);
        assert!(matches!(
            seal_to_vec(&plaintext, &root, &options),
            Err(RaoAeadError::PlaintextSizeMismatch)
        ));
    }
}
