//! Streaming writer for `rao-v1` objects.

use std::collections::BTreeMap;
use std::io::Read;

use remanence_aead::{
    header::object_id_field, seal_to_vec, EnvelopeSealOptions, RecipientPublicKey, SealOptions,
    SealReport,
};
use remanence_library::{BlockSink, VecBlockSink};
use sha2::{Digest, Sha256};

use crate::error::FormatError;
use crate::layout::{file_pax_records, plan_one_file, plan_rem_tar_object, RemTarObjectLayout};
use crate::model::{
    RemTarEntryType, RemTarFile, RemTarFileSpec, RemTarFileStream, RemTarObjectOptions,
    MANIFEST_PATH, TAR_RECORD_SIZE,
};
use crate::pax::{encode_pax_records, round_up_usize, tar_padding_len, with_alignment_pad};
use crate::tar::{
    encode_header, encode_pax_backed_directory_header, encode_pax_backed_hardlink_header,
    encode_pax_backed_regular_header, encode_pax_backed_symlink_header, is_portable_ustar_linkname,
    TYPE_PAX_EXTENDED, TYPE_PAX_GLOBAL,
};

/// Report for writing an encrypted RAO object.
#[derive(Debug, Clone)]
pub struct EncryptedRaoWriteReport {
    /// Layout of the authenticated canonical plaintext RAO stream.
    pub plaintext_layout: RemTarObjectLayout,
    /// Envelope report for the stored encrypted bytes.
    pub envelope: SealReport,
}

/// Buffered body-block writer. It emits only full fixed-size body blocks until
/// [`Self::finish_after_tar_eof`] is called after tar EOF records.
#[must_use = "BodyBlockWriter buffers data and must be finished via finish_after_tar_eof()"]
pub struct BodyBlockWriter<'a, S: BlockSink + ?Sized> {
    sink: &'a mut S,
    block_size: usize,
    buffer: Vec<u8>,
    blocks_written: u64,
    finished: bool,
}

impl<'a, S: BlockSink + ?Sized> BodyBlockWriter<'a, S> {
    /// Construct a writer for fixed-size body blocks.
    pub fn new(sink: &'a mut S, block_size: usize) -> Result<Self, FormatError> {
        crate::pax::validate_chunk_size(block_size)?;
        Ok(Self {
            sink,
            block_size,
            buffer: Vec::with_capacity(block_size),
            blocks_written: 0,
            finished: false,
        })
    }

    /// Stream bytes into fixed-size tape blocks.
    pub fn write_all(&mut self, mut bytes: &[u8]) -> Result<(), FormatError> {
        if self.finished {
            return Err(FormatError::invalid(
                "cannot write after finish_after_tar_eof",
            ));
        }
        while !bytes.is_empty() {
            let available = self.block_size - self.buffer.len();
            let take = available.min(bytes.len());
            self.buffer.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.buffer.len() == self.block_size {
                self.flush_full_block()?;
            }
        }
        Ok(())
    }

    /// Number of full body blocks emitted so far.
    pub fn blocks_written(&self) -> u64 {
        self.blocks_written
    }

    /// Bytes currently buffered but not yet emitted as a full block.
    pub fn buffered_len(&self) -> usize {
        self.buffer.len()
    }

    /// Zero-fill and emit the final partial block. This must be called only
    /// after both tar EOF zero records have already been written.
    pub fn finish_after_tar_eof(&mut self) -> Result<u64, FormatError> {
        if self.finished {
            return Ok(self.blocks_written);
        }
        if !self.buffer.is_empty() {
            self.buffer.resize(self.block_size, 0);
            self.flush_full_block()?;
        }
        self.finished = true;
        Ok(self.blocks_written)
    }

    fn flush_full_block(&mut self) -> Result<(), FormatError> {
        debug_assert_eq!(self.buffer.len(), self.block_size);
        let outcome = self.sink.write_block(&self.buffer)?;
        let expected_bytes = self.buffer.len() as u64;
        if u64::from(outcome.bytes_written) != expected_bytes || outcome.end_of_medium {
            return Err(FormatError::IncompleteBlockWrite {
                expected_bytes,
                bytes_written: u64::from(outcome.bytes_written),
                early_warning: outcome.early_warning,
                end_of_medium: outcome.end_of_medium,
            });
        }
        self.buffer.clear();
        self.blocks_written += 1;
        Ok(())
    }
}

/// Write a complete `rao-v1` archive body to `sink`.
pub fn write_rem_tar_object<S: BlockSink + ?Sized>(
    sink: &mut S,
    options: &RemTarObjectOptions,
    files: &[RemTarFile<'_>],
) -> Result<RemTarObjectLayout, FormatError> {
    let specs: Vec<RemTarFileSpec> = files.iter().map(file_to_spec).collect();
    let layout = plan_rem_tar_object(options, &specs)?;
    let mut writer = BodyBlockWriter::new(sink, options.chunk_size)?;

    write_global_header(&mut writer, options, &layout.schema_version)?;
    for (file, spec) in files.iter().zip(specs.iter()) {
        write_file_entry_from_bytes(&mut writer, options.chunk_size, spec, file.data, false)?;
    }

    let manifest_spec = RemTarFileSpec {
        entry_type: RemTarEntryType::Regular,
        path: MANIFEST_PATH.to_string(),
        file_id: options.manifest_file_id.clone(),
        size_bytes: layout.manifest_cbor.len() as u64,
        file_sha256: Some(layout.manifest_sha256),
        link_target: None,
        xattrs: Default::default(),
        extensions: Default::default(),
        mtime: None,
        executable: Some(false),
    };
    write_file_entry_from_bytes(
        &mut writer,
        options.chunk_size,
        &manifest_spec,
        &layout.manifest_cbor,
        true,
    )?;

    writer.write_all(&[0u8; TAR_RECORD_SIZE])?;
    writer.write_all(&[0u8; TAR_RECORD_SIZE])?;
    let blocks_written = writer.finish_after_tar_eof()?;
    if blocks_written != layout.projected_size_blocks {
        return Err(FormatError::layout(format!(
            "writer emitted {blocks_written} blocks, layout projected {}",
            layout.projected_size_blocks
        )));
    }
    Ok(layout)
}

/// Write a complete `rao-v1` archive body from streaming file sources.
///
/// Each input supplies precomputed metadata and a [`Read`] implementation.
/// This lets callers perform the required size/hash pass before tape admission,
/// then stream file bytes through Layer 3b without materializing full payloads
/// in memory. The writer consumes exactly `spec.size_bytes` bytes from each
/// reader and verifies the observed SHA-256 against `spec.file_sha256`.
pub fn write_rem_tar_object_from_readers<S: BlockSink + ?Sized>(
    sink: &mut S,
    options: &RemTarObjectOptions,
    files: &mut [RemTarFileStream<'_>],
) -> Result<RemTarObjectLayout, FormatError> {
    let specs: Vec<RemTarFileSpec> = files.iter().map(|file| file.spec.clone()).collect();
    let layout = plan_rem_tar_object(options, &specs)?;
    let mut writer = BodyBlockWriter::new(sink, options.chunk_size)?;

    write_global_header(&mut writer, options, &layout.schema_version)?;
    for file in files.iter_mut() {
        write_file_entry_from_reader(
            &mut writer,
            options.chunk_size,
            &file.spec,
            file.reader,
            false,
        )?;
    }

    let manifest_spec = RemTarFileSpec {
        entry_type: RemTarEntryType::Regular,
        path: MANIFEST_PATH.to_string(),
        file_id: options.manifest_file_id.clone(),
        size_bytes: layout.manifest_cbor.len() as u64,
        file_sha256: Some(layout.manifest_sha256),
        link_target: None,
        xattrs: Default::default(),
        extensions: Default::default(),
        mtime: None,
        executable: Some(false),
    };
    write_file_entry_from_bytes(
        &mut writer,
        options.chunk_size,
        &manifest_spec,
        &layout.manifest_cbor,
        true,
    )?;

    writer.write_all(&[0u8; TAR_RECORD_SIZE])?;
    writer.write_all(&[0u8; TAR_RECORD_SIZE])?;
    let blocks_written = writer.finish_after_tar_eof()?;
    if blocks_written != layout.projected_size_blocks {
        return Err(FormatError::layout(format!(
            "writer emitted {blocks_written} blocks, layout projected {}",
            layout.projected_size_blocks
        )));
    }
    Ok(layout)
}

/// Write a complete recipient-envelope RAO object to `sink`.
///
/// Canonical archive construction remains in this crate while all encrypted
/// framing and cryptography are delegated to `remanence-aead`.
pub fn write_encrypted_rao_object<S: BlockSink + ?Sized>(
    sink: &mut S,
    options: &RemTarObjectOptions,
    files: &[RemTarFile<'_>],
    recipients: &[RecipientPublicKey],
) -> Result<EncryptedRaoWriteReport, FormatError> {
    let chunk_size = validate_recipient_envelope_preconditions(options)?;
    let mut plaintext_sink = VecBlockSink::new();
    let plaintext_layout = write_rem_tar_object(&mut plaintext_sink, options, files)?;
    let plaintext = flatten_blocks(plaintext_sink.blocks, options.chunk_size)?;
    seal_recipient_envelope(
        sink,
        options,
        recipients,
        chunk_size,
        plaintext_layout,
        plaintext,
    )
}

/// Write a complete recipient-envelope RAO object from streaming sources.
pub fn write_encrypted_rao_object_from_readers<S: BlockSink + ?Sized>(
    sink: &mut S,
    options: &RemTarObjectOptions,
    files: &mut [RemTarFileStream<'_>],
    recipients: &[RecipientPublicKey],
) -> Result<EncryptedRaoWriteReport, FormatError> {
    let chunk_size = validate_recipient_envelope_preconditions(options)?;
    let mut plaintext_sink = VecBlockSink::new();
    let plaintext_layout = write_rem_tar_object_from_readers(&mut plaintext_sink, options, files)?;
    let plaintext = flatten_blocks(plaintext_sink.blocks, options.chunk_size)?;
    seal_recipient_envelope(
        sink,
        options,
        recipients,
        chunk_size,
        plaintext_layout,
        plaintext,
    )
}

fn seal_recipient_envelope<S: BlockSink + ?Sized>(
    sink: &mut S,
    options: &RemTarObjectOptions,
    recipients: &[RecipientPublicKey],
    chunk_size: u32,
    plaintext_layout: RemTarObjectLayout,
    plaintext: Vec<u8>,
) -> Result<EncryptedRaoWriteReport, FormatError> {
    if plaintext.len() as u64 != plaintext_layout.total_size_bytes {
        return Err(FormatError::layout(format!(
            "plaintext byte length {} does not match layout {}",
            plaintext.len(),
            plaintext_layout.total_size_bytes
        )));
    }
    let plaintext_digest = sha256_array(&plaintext);
    let seal_options = EnvelopeSealOptions {
        allow_single_recipient: false,
        common: SealOptions {
            chunk_size,
            object_id: options.object_id.clone(),
            plaintext_size: plaintext.len() as u64,
            plaintext_digest,
        },
        recipients: recipients.to_vec(),
    };
    let (sealed, envelope) = seal_to_vec(&plaintext, &seal_options)?;
    let written_blocks = write_fixed_blocks(sink, options.chunk_size, &sealed)?;
    if written_blocks != envelope.stored_size_blocks {
        return Err(FormatError::layout(format!(
            "encrypted blocks written {written_blocks} does not match envelope {}",
            envelope.stored_size_blocks
        )));
    }
    Ok(EncryptedRaoWriteReport {
        plaintext_layout,
        envelope,
    })
}

fn validate_recipient_envelope_preconditions(
    options: &RemTarObjectOptions,
) -> Result<u32, FormatError> {
    crate::pax::validate_chunk_size(options.chunk_size)?;
    object_id_field(&options.object_id)?;
    u32::try_from(options.chunk_size)
        .map_err(|_| FormatError::invalid("chunk_size does not fit RAO header uint32"))
}

fn write_global_header<S: BlockSink + ?Sized>(
    writer: &mut BodyBlockWriter<'_, S>,
    options: &RemTarObjectOptions,
    schema_version: &str,
) -> Result<(), FormatError> {
    let body = encode_pax_records(&crate::layout::global_pax_records(options, schema_version))?;
    writer.write_all(&encode_header(
        "GlobalHead.0/PaxHeaders/remanence",
        body.len() as u64,
        TYPE_PAX_GLOBAL,
        0o644,
    )?)?;
    writer.write_all(&body)?;
    writer.write_all(&vec![
        0u8;
        round_up_usize(body.len(), TAR_RECORD_SIZE)?
            - body.len()
    ])?;
    Ok(())
}

fn flatten_blocks(blocks: Vec<Vec<u8>>, chunk_size: usize) -> Result<Vec<u8>, FormatError> {
    let total_len = blocks
        .len()
        .checked_mul(chunk_size)
        .ok_or_else(|| FormatError::layout("plaintext block byte count overflow"))?;
    let mut out = Vec::new();
    out.try_reserve_exact(total_len)
        .map_err(|_| FormatError::layout("plaintext object too large to materialize"))?;
    for block in blocks {
        if block.len() != chunk_size {
            return Err(FormatError::layout(format!(
                "plaintext block has length {}, expected {chunk_size}",
                block.len()
            )));
        }
        out.extend_from_slice(&block);
    }
    Ok(out)
}

fn write_fixed_blocks<S: BlockSink + ?Sized>(
    sink: &mut S,
    chunk_size: usize,
    bytes: &[u8],
) -> Result<u64, FormatError> {
    crate::pax::validate_chunk_size(chunk_size)?;
    if bytes.is_empty() || bytes.len() % chunk_size != 0 {
        return Err(FormatError::layout(
            "encrypted object bytes are not a positive multiple of chunk_size",
        ));
    }
    let mut blocks = 0u64;
    for block in bytes.chunks_exact(chunk_size) {
        let outcome = sink.write_block(block)?;
        let expected_bytes = block.len() as u64;
        if u64::from(outcome.bytes_written) != expected_bytes || outcome.end_of_medium {
            return Err(FormatError::IncompleteBlockWrite {
                expected_bytes,
                bytes_written: u64::from(outcome.bytes_written),
                early_warning: outcome.early_warning,
                end_of_medium: outcome.end_of_medium,
            });
        }
        blocks = blocks
            .checked_add(1)
            .ok_or_else(|| FormatError::layout("encrypted block count overflow"))?;
    }
    Ok(blocks)
}

fn sha256_array(bytes: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

fn write_file_entry_from_bytes<S: BlockSink + ?Sized>(
    writer: &mut BodyBlockWriter<'_, S>,
    chunk_size: usize,
    spec: &RemTarFileSpec,
    data: &[u8],
    is_manifest: bool,
) -> Result<(), FormatError> {
    if data.len() as u64 != spec.size_bytes {
        return Err(FormatError::invalid(format!(
            "data length for {} does not match spec",
            spec.path
        )));
    }
    write_file_entry_header(writer, chunk_size, spec, is_manifest)?;
    writer.write_all(data)?;
    write_file_padding(writer, spec.size_bytes)?;
    Ok(())
}

fn write_file_entry_from_reader<S: BlockSink + ?Sized>(
    writer: &mut BodyBlockWriter<'_, S>,
    chunk_size: usize,
    spec: &RemTarFileSpec,
    reader: &mut dyn Read,
    is_manifest: bool,
) -> Result<(), FormatError> {
    write_file_entry_header(writer, chunk_size, spec, is_manifest)?;
    let observed_hash = stream_file_payload(writer, chunk_size, spec, reader)?;
    if let Some(expected_hash) = spec.file_sha256 {
        if observed_hash != expected_hash {
            return Err(FormatError::invalid(format!(
                "streamed data hash for {} does not match spec",
                spec.path
            )));
        }
    } else if spec.entry_type == RemTarEntryType::Regular {
        return Err(FormatError::invalid(format!(
            "regular file {} is missing file_sha256",
            spec.path
        )));
    }
    write_file_padding(writer, spec.size_bytes)?;
    Ok(())
}

fn write_file_entry_header<S: BlockSink + ?Sized>(
    writer: &mut BodyBlockWriter<'_, S>,
    chunk_size: usize,
    spec: &RemTarFileSpec,
    is_manifest: bool,
) -> Result<(), FormatError> {
    let offset = writer
        .blocks_written
        .checked_mul(chunk_size as u64)
        .and_then(|base| base.checked_add(writer.buffer.len() as u64))
        .ok_or_else(|| FormatError::layout("writer byte offset overflow"))?;
    let base_records = file_pax_records(spec, chunk_size, is_manifest)?;
    let records = if spec.size_bytes == 0 {
        base_records
    } else {
        with_alignment_pad(offset, chunk_size, &base_records)?
    };
    let (_layout, _next_offset) =
        plan_one_file(chunk_size, offset, spec, is_manifest, Some(&records))?;
    write_pax_and_regular_header(writer, spec, &records, is_manifest)?;
    Ok(())
}

fn write_file_padding<S: BlockSink + ?Sized>(
    writer: &mut BodyBlockWriter<'_, S>,
    size_bytes: u64,
) -> Result<(), FormatError> {
    let padding = tar_padding_len(size_bytes);
    if padding > 0 {
        writer.write_all(&vec![0u8; padding])?;
    }
    Ok(())
}

fn write_pax_and_regular_header<S: BlockSink + ?Sized>(
    writer: &mut BodyBlockWriter<'_, S>,
    spec: &RemTarFileSpec,
    records: &BTreeMap<String, String>,
    is_manifest: bool,
) -> Result<(), FormatError> {
    let body = encode_pax_records(records)?;
    let pax_name = if is_manifest {
        "PaxHeaders.0/_remanence_manifest"
    } else {
        "PaxHeaders.0/remanence_file"
    };
    writer.write_all(&encode_header(
        pax_name,
        body.len() as u64,
        TYPE_PAX_EXTENDED,
        0o644,
    )?)?;
    writer.write_all(&body)?;
    writer.write_all(&vec![
        0u8;
        round_up_usize(body.len(), TAR_RECORD_SIZE)?
            - body.len()
    ])?;
    let entry_header = match spec.entry_type {
        RemTarEntryType::Regular => encode_pax_backed_regular_header(
            &spec.path,
            spec.size_bytes,
            if spec.executable == Some(true) {
                0o755
            } else {
                0o644
            },
        )?,
        RemTarEntryType::Hardlink => {
            let target = spec
                .link_target
                .as_deref()
                .ok_or_else(|| FormatError::invalid("hardlink entry missing link target"))?;
            encode_pax_backed_hardlink_header(
                &spec.path,
                target,
                !is_portable_ustar_linkname(target),
            )?
        }
        RemTarEntryType::Symlink => {
            let target = spec
                .link_target
                .as_deref()
                .ok_or_else(|| FormatError::invalid("symlink entry missing link target"))?;
            encode_pax_backed_symlink_header(
                &spec.path,
                target,
                !is_portable_ustar_linkname(target),
            )?
        }
        RemTarEntryType::Directory => encode_pax_backed_directory_header(&spec.path)?,
    };
    writer.write_all(&entry_header)?;
    Ok(())
}

fn stream_file_payload<S: BlockSink + ?Sized>(
    writer: &mut BodyBlockWriter<'_, S>,
    chunk_size: usize,
    spec: &RemTarFileSpec,
    reader: &mut dyn Read,
) -> Result<[u8; 32], FormatError> {
    let mut remaining = spec.size_bytes;
    let scratch_len = chunk_size.clamp(8 * 1024, 1024 * 1024);
    let mut scratch = vec![0u8; scratch_len];
    let mut hasher = Sha256::new();

    while remaining > 0 {
        let limit = scratch
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        let read = reader
            .read(&mut scratch[..limit])
            .map_err(|err| FormatError::source_io(format!("reading {}", spec.path), err))?;
        if read == 0 {
            return Err(FormatError::invalid(format!(
                "stream for {} ended before {} bytes were read",
                spec.path, spec.size_bytes
            )));
        }
        writer.write_all(&scratch[..read])?;
        hasher.update(&scratch[..read]);
        remaining -= read as u64;
    }

    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    Ok(out)
}

fn file_to_spec(file: &RemTarFile<'_>) -> RemTarFileSpec {
    let digest = Sha256::digest(file.data);
    let mut file_sha256 = [0u8; 32];
    file_sha256.copy_from_slice(&digest);
    RemTarFileSpec {
        entry_type: RemTarEntryType::Regular,
        path: file.path.to_string(),
        file_id: file.file_id.to_string(),
        size_bytes: file.data.len() as u64,
        file_sha256: Some(file_sha256),
        link_target: None,
        xattrs: Default::default(),
        extensions: Default::default(),
        mtime: file.mtime.map(str::to_string),
        executable: file.executable,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{self, Cursor, Read};
    use std::process::Command;

    use remanence_aead::RecipientPrivateKey;
    use remanence_library::{
        BlockSink, TapeIoError, TapePosition, VecBlockSink, VecBlockSource, WriteFilemarksOutcome,
        WriteOutcome,
    };

    use super::*;
    use crate::model::{RemTarCborValue, RemTarObjectOptions};

    fn options(chunk_size: usize) -> RemTarObjectOptions {
        let mut opts = RemTarObjectOptions::new(
            "33333333-3333-3333-3333-333333333333",
            "caller-writer",
            "2026-05-27T22:00:00+05:30",
            "44444444-4444-4444-4444-444444444444",
        );
        opts.chunk_size = chunk_size;
        opts
    }

    fn recipient_public_keys() -> Vec<remanence_aead::RecipientPublicKey> {
        let primary = RecipientPrivateKey::new([0x31; 16], "primary-2026", [0x41; 32]).unwrap();
        let recovery = RecipientPrivateKey::new([0x32; 16], "recovery-2026", [0x42; 32]).unwrap();
        vec![
            primary.public_key(0).unwrap(),
            recovery.public_key(1).unwrap(),
        ]
    }

    fn test_position(lba: u64) -> TapePosition {
        TapePosition {
            lba,
            partition: 0,
            beginning_of_partition: false,
            end_of_partition: false,
            block_position_end_of_warning: false,
        }
    }

    struct OutcomeSink {
        outcome: WriteOutcome,
        writes: u64,
    }

    impl OutcomeSink {
        fn new(outcome: WriteOutcome) -> Self {
            Self { outcome, writes: 0 }
        }
    }

    impl BlockSink for OutcomeSink {
        fn write_block(&mut self, _buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
            self.writes += 1;
            Ok(self.outcome)
        }

        fn write_filemarks(&mut self, _count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
            unreachable!("BodyBlockWriter does not write filemarks")
        }

        fn position(&mut self) -> Result<TapePosition, TapeIoError> {
            Ok(test_position(self.writes))
        }
    }

    #[test]
    fn body_block_writer_emits_only_full_blocks() {
        let mut sink = VecBlockSink::new();
        let mut writer = BodyBlockWriter::new(&mut sink, 1024).unwrap();
        writer.write_all(&vec![1u8; 1536]).unwrap();
        assert_eq!(writer.blocks_written(), 1);
        assert_eq!(writer.buffered_len(), 512);
        writer.write_all(&[2u8; 100]).unwrap();
        assert_eq!(writer.blocks_written(), 1);
        assert_eq!(writer.buffered_len(), 612);
        let count = writer.finish_after_tar_eof().unwrap();
        assert_eq!(count, 2);
        drop(writer);
        assert_eq!(sink.blocks.len(), 2);
        assert_eq!(sink.blocks[0].len(), 1024);
        assert_eq!(sink.blocks[1].len(), 1024);
    }

    #[test]
    fn body_block_writer_rejects_short_write_outcome() {
        let mut sink = OutcomeSink::new(WriteOutcome::from_device_position(
            512,
            true,
            false,
            test_position(1),
        ));
        let mut writer = BodyBlockWriter::new(&mut sink, 1024).unwrap();

        let err = writer.write_all(&vec![1u8; 1024]).expect_err("short write");

        match err {
            FormatError::IncompleteBlockWrite {
                expected_bytes,
                bytes_written,
                early_warning,
                end_of_medium,
            } => {
                assert_eq!(expected_bytes, 1024);
                assert_eq!(bytes_written, 512);
                assert!(early_warning);
                assert!(!end_of_medium);
            }
            other => panic!("expected IncompleteBlockWrite, got {other:?}"),
        }
    }

    #[test]
    fn body_block_writer_rejects_end_of_medium_outcome() {
        let mut sink = OutcomeSink::new(WriteOutcome::from_device_position(
            1024,
            false,
            true,
            test_position(1),
        ));
        let mut writer = BodyBlockWriter::new(&mut sink, 1024).unwrap();

        let err = writer.write_all(&vec![1u8; 1024]).expect_err("hard EOM");

        assert!(matches!(
            err,
            FormatError::IncompleteBlockWrite {
                bytes_written: 1024,
                end_of_medium: true,
                ..
            }
        ));
    }

    #[test]
    fn body_block_writer_accepts_full_write_with_early_warning() {
        let mut sink = OutcomeSink::new(WriteOutcome::from_device_position(
            1024,
            true,
            false,
            test_position(1),
        ));
        let mut writer = BodyBlockWriter::new(&mut sink, 1024).unwrap();

        writer.write_all(&vec![1u8; 1024]).expect("full write");

        assert_eq!(writer.blocks_written(), 1);
    }

    #[test]
    fn writer_projection_matches_emitted_blocks_and_data_alignment() {
        let opts = options(4096);
        let files = [
            RemTarFile {
                path: "a.txt",
                file_id: "file-a",
                data: b"hello",
                mtime: Some("0"),
                executable: Some(false),
            },
            RemTarFile {
                path: "b.bin",
                file_id: "file-b",
                data: &[0x5Au8; 5000],
                mtime: None,
                executable: Some(true),
            },
        ];
        let mut sink = VecBlockSink::new();
        let layout = write_rem_tar_object(&mut sink, &opts, &files).unwrap();
        assert_eq!(sink.blocks.len() as u64, layout.projected_size_blocks);
        assert_eq!(
            sink.blocks.len() as u64 * opts.chunk_size as u64,
            layout.total_size_bytes
        );
        for block in &sink.blocks {
            assert_eq!(block.len(), opts.chunk_size);
        }
        for file in layout.files.iter().chain(std::iter::once(&layout.manifest)) {
            assert_eq!(file.data_offset % opts.chunk_size as u64, 0);
        }
    }

    #[test]
    fn streaming_writer_matches_in_memory_writer_with_chunked_sources() {
        let opts = options(4096);
        let first = b"hello through a tiny reader".to_vec();
        let second = vec![0x5Au8; 5000];
        let files = [
            RemTarFile {
                path: "a.txt",
                file_id: "file-a",
                data: &first,
                mtime: Some("0"),
                executable: Some(false),
            },
            RemTarFile {
                path: "b.bin",
                file_id: "file-b",
                data: &second,
                mtime: None,
                executable: Some(true),
            },
        ];

        let mut expected_sink = VecBlockSink::new();
        let expected_layout = write_rem_tar_object(&mut expected_sink, &opts, &files).unwrap();

        let mut first_reader = ChunkedReader::new(&first, 3);
        let mut second_reader = ChunkedReader::new(&second, 17);
        let mut streams = [
            RemTarFileStream::new(
                file_spec("a.txt", "file-a", &first, Some("0"), Some(false)),
                &mut first_reader,
            ),
            RemTarFileStream::new(
                file_spec("b.bin", "file-b", &second, None, Some(true)),
                &mut second_reader,
            ),
        ];
        let mut streaming_sink = VecBlockSink::new();
        let streaming_layout =
            write_rem_tar_object_from_readers(&mut streaming_sink, &opts, &mut streams).unwrap();

        assert_eq!(
            streaming_layout.projected_size_blocks,
            expected_layout.projected_size_blocks
        );
        assert_eq!(
            streaming_layout.total_size_bytes,
            expected_layout.total_size_bytes
        );
        assert_eq!(streaming_sink.blocks, expected_sink.blocks);
    }

    #[test]
    fn streaming_writer_round_trips_symlinks_and_empty_directories() {
        let opts = options(4096);
        let long_target = format!("../{}", "nested-target/".repeat(10));
        let primary_data = b"primary hardlink bytes".to_vec();
        let mut primary_reader = Cursor::new(primary_data.as_slice());
        let mut hardlink_reader = io::empty();
        let mut short_reader = io::empty();
        let mut long_reader = io::empty();
        let mut dir_reader = io::empty();
        let mut streams = [
            RemTarFileStream::new(
                file_spec(
                    "target.mov",
                    "file-target",
                    &primary_data,
                    None,
                    Some(false),
                ),
                &mut primary_reader,
            ),
            RemTarFileStream::new(
                RemTarFileSpec::hardlink("links/copy.mov", "hardlink-copy", "target.mov"),
                &mut hardlink_reader,
            ),
            RemTarFileStream::new(
                RemTarFileSpec::symlink("links/latest", "link-short", "../target.mov"),
                &mut short_reader,
            ),
            RemTarFileStream::new(
                RemTarFileSpec::symlink("links/long", "link-long", long_target.clone()),
                &mut long_reader,
            ),
            RemTarFileStream::new(
                RemTarFileSpec::directory("empty/", "dir-empty"),
                &mut dir_reader,
            ),
        ];
        let mut sink = VecBlockSink::new();

        let layout = write_rem_tar_object_from_readers(&mut sink, &opts, &mut streams).unwrap();
        let mut source = VecBlockSource::new(sink.blocks);
        let read =
            crate::read_rem_tar_object(&mut source, opts.chunk_size, layout.projected_size_blocks)
                .unwrap();

        let primary = read.entry("target.mov").unwrap();
        assert_eq!(primary.entry_type, RemTarEntryType::Regular);
        assert_eq!(primary.data, primary_data);

        let hardlink = read.entry("links/copy.mov").unwrap();
        assert_eq!(hardlink.entry_type, RemTarEntryType::Hardlink);
        assert_eq!(hardlink.link_target.as_deref(), Some("target.mov"));
        assert_eq!(hardlink.size_bytes, 0);
        assert!(hardlink.data.is_empty());
        assert_eq!(hardlink.first_chunk_lba, None);

        let short = read.entry("links/latest").unwrap();
        assert_eq!(short.entry_type, RemTarEntryType::Symlink);
        assert_eq!(short.link_target.as_deref(), Some("../target.mov"));
        assert_eq!(short.size_bytes, 0);
        assert!(short.data.is_empty());
        assert_eq!(short.first_chunk_lba, None);

        let long = read.entry("links/long").unwrap();
        assert_eq!(long.entry_type, RemTarEntryType::Symlink);
        assert_eq!(long.link_target.as_deref(), Some(long_target.as_str()));
        assert!(long.pax_records.contains_key("linkpath"));

        let directory = read.entry("empty/").unwrap();
        assert_eq!(directory.entry_type, RemTarEntryType::Directory);
        assert_eq!(directory.link_target, None);
        assert_eq!(directory.size_bytes, 0);
        assert!(directory.data.is_empty());
    }

    #[test]
    fn streaming_writer_round_trips_xattrs_and_bumps_schema_version() {
        let opts = options(4096);
        let data = b"xattr payload".to_vec();
        let mut spec = file_spec("tagged.txt", "file-tagged", &data, None, Some(false));
        spec.xattrs
            .insert("user.comment".to_string(), b"blue".to_vec());
        spec.xattrs
            .insert("user.remanence.color".to_string(), vec![0x01, 0x02, 0xff]);
        let mut reader = Cursor::new(data.as_slice());
        let mut streams = [RemTarFileStream::new(spec, &mut reader)];
        let mut sink = VecBlockSink::new();

        let layout = write_rem_tar_object_from_readers(&mut sink, &opts, &mut streams).unwrap();
        assert_eq!(layout.schema_version, crate::model::SCHEMA_VERSION_XATTRS);

        let mut source = VecBlockSource::new(sink.blocks);
        let read =
            crate::read_rem_tar_object(&mut source, opts.chunk_size, layout.projected_size_blocks)
                .unwrap();

        assert_eq!(
            read.global_pax
                .get("REMANENCE.schema_version")
                .map(String::as_str),
            Some(crate::model::SCHEMA_VERSION_XATTRS)
        );
        let entry = read.entry("tagged.txt").unwrap();
        assert_eq!(
            entry.xattrs.get("user.comment").map(Vec::as_slice),
            Some(&b"blue"[..])
        );
        assert_eq!(
            entry.xattrs.get("user.remanence.color").map(Vec::as_slice),
            Some(&[0x01, 0x02, 0xff][..])
        );
    }

    #[test]
    fn streaming_writer_round_trips_entry_and_object_extensions() {
        let mut opts = options(4096);
        opts.extensions.insert(
            "org.example.object".to_string(),
            RemTarCborValue::Text("object-value".to_string()),
        );
        let data = b"extension payload".to_vec();
        let mut spec = file_spec("extended.txt", "file-extended", &data, None, Some(false));
        spec.extensions.insert(
            "org.example.entry".to_string(),
            RemTarCborValue::Array(vec![
                RemTarCborValue::Bytes(vec![0, 0xff]),
                RemTarCborValue::Bool(true),
            ]),
        );
        let mut reader = Cursor::new(data.as_slice());
        let mut streams = [RemTarFileStream::new(spec, &mut reader)];
        let mut sink = VecBlockSink::new();

        let layout = write_rem_tar_object_from_readers(&mut sink, &opts, &mut streams).unwrap();
        assert_eq!(layout.schema_version, crate::model::SCHEMA_VERSION);
        let mut source = VecBlockSource::new(sink.blocks);
        let read =
            crate::read_rem_tar_object(&mut source, opts.chunk_size, layout.projected_size_blocks)
                .unwrap();

        assert_eq!(
            read.object_extensions["org.example.object"],
            RemTarCborValue::Text("object-value".to_string())
        );
        assert_eq!(
            read.entry("extended.txt").unwrap().extensions["org.example.entry"],
            RemTarCborValue::Array(vec![
                RemTarCborValue::Bytes(vec![0, 0xff]),
                RemTarCborValue::Bool(true),
            ])
        );
    }

    #[test]
    fn streaming_writer_rejects_short_source() {
        let opts = options(4096);
        let expected = b"abcdefghij".to_vec();
        let actual = b"abcde".to_vec();
        let mut reader = ChunkedReader::new(&actual, 2);
        let mut streams = [RemTarFileStream::new(
            file_spec("short.txt", "file-short", &expected, None, Some(false)),
            &mut reader,
        )];
        let mut sink = VecBlockSink::new();

        let err = write_rem_tar_object_from_readers(&mut sink, &opts, &mut streams).unwrap_err();

        assert!(err.to_string().contains("ended before"), "{err}");
    }

    #[test]
    fn streaming_writer_rejects_hash_mismatch() {
        let opts = options(4096);
        let expected = b"expected".to_vec();
        let actual = b"actual!!".to_vec();
        assert_eq!(expected.len(), actual.len());
        let mut reader = ChunkedReader::new(&actual, 4);
        let mut streams = [RemTarFileStream::new(
            file_spec("changed.txt", "file-changed", &expected, None, Some(false)),
            &mut reader,
        )];
        let mut sink = VecBlockSink::new();

        let err = write_rem_tar_object_from_readers(&mut sink, &opts, &mut streams).unwrap_err();

        assert!(err.to_string().contains("hash"), "{err}");
    }

    #[test]
    fn encrypted_writer_rejects_overlong_object_id_before_plaintext_work() {
        let mut opts = options(4096);
        opts.object_id = "x".repeat(65);
        let recipients = recipient_public_keys();
        let files = [RemTarFile {
            path: "payload.bin",
            file_id: "file-payload",
            data: b"payload bytes",
            mtime: None,
            executable: Some(false),
        }];
        let mut sink = VecBlockSink::new();

        let err = write_encrypted_rao_object(&mut sink, &opts, &files, &recipients).unwrap_err();

        assert!(matches!(
            err,
            FormatError::Aead(remanence_aead::RaoAeadError::InvalidObjectIdField)
        ));
        assert!(
            sink.blocks.is_empty(),
            "invalid encrypted object_id must fail before stored writes"
        );
        assert!(
            sink.filemarks.is_empty(),
            "encrypted writer should not write filemarks"
        );

        let mut unreadable = PanicReader;
        let mut streams = [RemTarFileStream::new(
            file_spec(
                "streamed.bin",
                "file-streamed",
                b"streamed payload",
                None,
                Some(false),
            ),
            &mut unreadable,
        )];
        let mut streaming_sink = VecBlockSink::new();

        let err = write_encrypted_rao_object_from_readers(
            &mut streaming_sink,
            &opts,
            &mut streams,
            &recipients,
        )
        .unwrap_err();

        assert!(matches!(
            err,
            FormatError::Aead(remanence_aead::RaoAeadError::InvalidObjectIdField)
        ));
        assert!(
            streaming_sink.blocks.is_empty(),
            "streaming invalid encrypted object_id must fail before stored writes"
        );
        assert!(
            streaming_sink.filemarks.is_empty(),
            "encrypted streaming writer should not write filemarks"
        );
    }

    #[test]
    fn emitted_archive_is_readable_by_python_tarfile() {
        let opts = options(4096);
        let files = [
            RemTarFile {
                path: "a.txt",
                file_id: "file-a",
                data: b"hello",
                mtime: Some("0"),
                executable: Some(false),
            },
            RemTarFile {
                path: "dir/b.bin",
                file_id: "file-b",
                data: &[0x5Au8; 5000],
                mtime: None,
                executable: Some(true),
            },
            RemTarFile {
                path: "vidéo/clip.txt",
                file_id: "file-c",
                data: b"utf8 path",
                mtime: None,
                executable: Some(false),
            },
        ];
        let mut sink = VecBlockSink::new();
        write_rem_tar_object(&mut sink, &opts, &files).unwrap();
        let archive: Vec<u8> = sink.blocks.into_iter().flatten().collect();
        let path = std::env::temp_dir().join(format!(
            "remanence-format-tarfile-{}-{}.tar",
            std::process::id(),
            archive.len()
        ));
        fs::write(&path, archive).unwrap();
        let script = r#"
import sys, tarfile
with tarfile.open(sys.argv[1], "r:*") as tf:
    names = tf.getnames()
    assert names == ["a.txt", "dir/b.bin", "vidéo/clip.txt", "_remanence/manifest.cbor"], names
    assert tf.extractfile("a.txt").read() == b"hello"
    assert tf.extractfile("dir/b.bin").read() == bytes([0x5A]) * 5000
    assert tf.extractfile("vidéo/clip.txt").read() == b"utf8 path"
    assert len(tf.extractfile("_remanence/manifest.cbor").read()) > 0
"#;
        let status = Command::new("python3")
            .arg("-c")
            .arg(script)
            .arg(&path)
            .status()
            .unwrap();
        let _ = fs::remove_file(&path);
        assert!(status.success());
    }

    fn file_spec(
        path: &str,
        file_id: &str,
        data: &[u8],
        mtime: Option<&str>,
        executable: Option<bool>,
    ) -> RemTarFileSpec {
        let digest = Sha256::digest(data);
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&digest);
        RemTarFileSpec {
            entry_type: RemTarEntryType::Regular,
            path: path.to_string(),
            file_id: file_id.to_string(),
            size_bytes: data.len() as u64,
            file_sha256: Some(hash),
            link_target: None,
            xattrs: Default::default(),
            extensions: Default::default(),
            mtime: mtime.map(str::to_string),
            executable,
        }
    }

    struct ChunkedReader<'a> {
        data: &'a [u8],
        max_chunk: usize,
    }

    impl<'a> ChunkedReader<'a> {
        fn new(data: &'a [u8], max_chunk: usize) -> Self {
            Self { data, max_chunk }
        }
    }

    impl Read for ChunkedReader<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.data.is_empty() {
                return Ok(0);
            }
            let len = self.data.len().min(buf.len()).min(self.max_chunk);
            buf[..len].copy_from_slice(&self.data[..len]);
            self.data = &self.data[len..];
            Ok(len)
        }
    }

    struct PanicReader;

    impl Read for PanicReader {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            panic!("encrypted envelope precondition failure must happen before source reads")
        }
    }
}
