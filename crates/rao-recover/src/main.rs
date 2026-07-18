//! Standalone, catalogless recovery for RAO recipient-envelope objects.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use remanence_aead::{open, KeyFrame, OpenReport, RaoHeader, RecipientPrivateKey, RAO_HEADER_LEN};
use remanence_format::{stream_rem_tar_object, FormatError, RemTarEntrySink, RemTarStreamEntry};
use remanence_library::FileBlockSource;
use remanence_stream::{restore_object_to_directory, FilesystemRestoreOptions};
use zeroize::Zeroize;

#[derive(Debug, Parser)]
#[command(
    name = "rao-recover",
    about = "Recover plaintext members from one RAO encrypted object without a catalog or daemon"
)]
struct Args {
    /// Complete stored RAO object bytes.
    #[arg(long, value_name = "PATH")]
    object: PathBuf,

    /// RAOP recipient private-key file for the envelope object.
    #[arg(long, value_name = "PATH")]
    private_key: PathBuf,

    /// Destination directory for recovered plaintext members.
    #[arg(long, value_name = "DIR")]
    out: PathBuf,

    /// Directory for temporary plaintext; defaults inside an existing --out,
    /// otherwise adjacent to --out.
    #[arg(long, value_name = "DIR")]
    staging_dir: Option<PathBuf>,

    /// Replace existing destination members.
    #[arg(long)]
    overwrite: bool,
}

fn main() -> ExitCode {
    match recover(&Args::parse()) {
        Ok(summary) => {
            println!(
                "recovered {} files ({} bytes) from RAO v{} object {}",
                summary.files_written,
                summary.bytes_written,
                summary.format_version,
                summary.object_id
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(1)
        }
    }
}

struct RecoverySummary {
    files_written: u64,
    bytes_written: u64,
    format_version: u8,
    object_id: String,
}

fn recover(args: &Args) -> Result<RecoverySummary, String> {
    let mut encrypted = File::open(&args.object)
        .map_err(|error| format!("open object {}: {error}", args.object.display()))?;
    let mut header_bytes = [0u8; RAO_HEADER_LEN];
    encrypted
        .read_exact(&mut header_bytes)
        .map_err(|error| format!("read object header: {error}"))?;
    let header =
        RaoHeader::parse(&header_bytes).map_err(|error| format!("parse object header: {error}"))?;
    let mut key_frame_bytes = vec![0u8; header.key_frame_len as usize];
    encrypted
        .read_exact(&mut key_frame_bytes)
        .map_err(|error| format!("read object key frame: {error}"))?;
    let key_frame = KeyFrame::parse(&key_frame_bytes)
        .map_err(|error| format!("parse object key frame: {error}"))?;
    encrypted
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("rewind encrypted object: {error}"))?;
    let staging_dir = args
        .staging_dir
        .as_deref()
        .unwrap_or_else(|| default_staging_dir(&args.out));
    let mut staged = SecurePlaintextStage::new_in(staging_dir)?;
    let opened = open_object(args, &mut encrypted, &key_frame, staged.as_file_mut())?;
    staged
        .as_file_mut()
        .sync_all()
        .map_err(|error| format!("sync authenticated plaintext staging file: {error}"))?;
    let chunk_size = usize::try_from(header.chunk_size)
        .map_err(|_| "RAO chunk size does not fit this host".to_string())?;
    let block_count = opened
        .metadata
        .plaintext_size
        .checked_div(chunk_size as u64)
        .ok_or_else(|| "plaintext block count division failed".to_string())?;

    let mut validation_source = FileBlockSource::open(staged.path(), chunk_size)
        .map_err(|error| format!("open authenticated plaintext stage: {error}"))?;
    let mut discard = DiscardEntrySink;
    let inner = stream_rem_tar_object(
        &mut validation_source,
        chunk_size,
        block_count,
        &mut discard,
    )
    .map_err(|error| format!("validate decrypted RAO members: {error}"))?;
    let inner_object_id = inner
        .global_pax
        .get("REMANENCE.object_id")
        .ok_or_else(|| "decrypted RAO is missing REMANENCE.object_id".to_string())?;
    if inner_object_id != &header.object_id {
        return Err("decrypted inner object_id does not match envelope header".to_string());
    }

    let mut restore_source = FileBlockSource::open(staged.path(), chunk_size)
        .map_err(|error| format!("reopen authenticated plaintext stage: {error}"))?;
    let restored = restore_object_to_directory(
        &mut restore_source,
        chunk_size,
        block_count,
        &args.out,
        FilesystemRestoreOptions {
            overwrite: args.overwrite,
            include_manifest: false,
        },
    )
    .map_err(|error| format!("restore plaintext members: {error}"))?;
    Ok(RecoverySummary {
        files_written: restored.files_written,
        bytes_written: restored.bytes_written,
        format_version: header.format_version,
        object_id: header.object_id,
    })
}

/// Chooses the target filesystem for authenticated plaintext staging.
fn default_staging_dir(out: &Path) -> &Path {
    if out.is_dir() {
        out
    } else {
        out.parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
    }
}

fn open_object<R: Read, W: std::io::Write>(
    args: &Args,
    encrypted: &mut R,
    key_frame: &KeyFrame,
    output: &mut W,
) -> Result<OpenReport, String> {
    let path = &args.private_key;
    let mut bytes = fs::read(path)
        .map_err(|error| format!("read recipient private key {}: {error}", path.display()))?;
    let parsed = RecipientPrivateKey::parse(&bytes);
    bytes.zeroize();
    let key = parsed
        .map_err(|error| format!("parse recipient private key {}: {error}", path.display()))?;
    if !key_frame
        .slots
        .iter()
        .any(|slot| slot.recipient_epoch_id == key.recipient_epoch_id)
    {
        let wanted = key_frame
            .slots
            .iter()
            .map(|slot| slot.epoch_label.as_str())
            .collect::<Vec<_>>()
            .join("/");
        return Err(format!(
            "object wants epoch {wanted}; you supplied {}",
            key.epoch_label
        ));
    }
    open(encrypted, output, &key).map_err(|error| format!("open RAO envelope: {error}"))
}

/// Plaintext staging file that is truncated before its directory entry is removed.
struct SecurePlaintextStage(tempfile::NamedTempFile);

impl SecurePlaintextStage {
    fn new_in(directory: &Path) -> Result<Self, String> {
        tempfile::Builder::new()
            .prefix(".rao-recover-plaintext.")
            .tempfile_in(directory)
            .map(Self)
            .map_err(|error| {
                format!(
                    "create secure plaintext staging file in {}: {error}",
                    directory.display()
                )
            })
    }

    fn as_file_mut(&mut self) -> &mut File {
        self.0.as_file_mut()
    }

    fn path(&self) -> &Path {
        self.0.path()
    }
}

impl Drop for SecurePlaintextStage {
    fn drop(&mut self) {
        // Best effort only: storage may retain old blocks, but no intact
        // plaintext staging file remains in the caller-selected directory.
        let _ = self.0.as_file_mut().set_len(0);
        let _ = self.0.as_file_mut().sync_all();
    }
}

struct DiscardEntrySink;

impl RemTarEntrySink for DiscardEntrySink {
    fn begin_file(&mut self, _entry: &RemTarStreamEntry) -> Result<(), FormatError> {
        Ok(())
    }

    fn write_file_data(&mut self, _bytes: &[u8]) -> Result<(), FormatError> {
        Ok(())
    }

    fn end_file(&mut self, _entry: &RemTarStreamEntry) -> Result<(), FormatError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remanence_aead::{seal_to_vec, EnvelopeSealOptions, SealOptions};
    use remanence_format::{write_rem_tar_object, RemTarFile, RemTarObjectOptions};
    use remanence_library::VecBlockSink;
    use sha2::{Digest, Sha256};

    #[test]
    fn recovers_encrypted_members_and_reports_epoch_mismatch() {
        let payload = b"recovery payload";
        let mut sink = VecBlockSink::new();
        let mut inner_options = RemTarObjectOptions::new(
            "recovery-object",
            "caller",
            "2026-07-11T00:00:00Z",
            "manifest",
        );
        inner_options.chunk_size = 512;
        write_rem_tar_object(
            &mut sink,
            &inner_options,
            &[RemTarFile {
                path: "member.txt",
                file_id: "member",
                data: payload,
                mtime: None,
                executable: None,
            }],
        )
        .unwrap();
        let plaintext = sink.blocks.concat();
        let digest: [u8; 32] = Sha256::digest(&plaintext).into();
        let common = SealOptions {
            chunk_size: 512,
            object_id: "recovery-object".to_string(),
            plaintext_size: plaintext.len() as u64,
            plaintext_digest: digest,
        };
        let safe = RecipientPrivateKey::new([1; 16], "safe-2026", [7; 32]).unwrap();
        let escrow = RecipientPrivateKey::new([2; 16], "escrow-2026", [8; 32]).unwrap();
        let (sealed, _) = seal_to_vec(
            &plaintext,
            &EnvelopeSealOptions {
                common,
                recipients: vec![safe.public_key(0).unwrap(), escrow.public_key(1).unwrap()],
            },
        )
        .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let object = temp.path().join("object.rao");
        let private_key = temp.path().join("safe.raop");
        let out = temp.path().join("out");
        fs::create_dir(&out).unwrap();
        fs::write(&object, sealed).unwrap();
        fs::write(&private_key, safe.serialize()).unwrap();
        let summary = recover(&Args {
            object: object.clone(),
            private_key,
            out: out.clone(),
            staging_dir: None,
            overwrite: false,
        })
        .unwrap();
        assert_eq!(summary.format_version, 2);
        assert_eq!(fs::read(out.join("member.txt")).unwrap(), payload);
        assert_eq!(fs::read_dir(&out).unwrap().count(), 1);

        let wrong = RecipientPrivateKey::new([3; 16], "wrong-2026", [9; 32]).unwrap();
        let wrong_path = temp.path().join("wrong.raop");
        fs::write(&wrong_path, wrong.serialize()).unwrap();
        let error = recover(&Args {
            object,
            private_key: wrong_path,
            out: temp.path().join("wrong-out"),
            staging_dir: None,
            overwrite: false,
        })
        .err()
        .unwrap();
        assert!(error.contains("object wants epoch safe-2026/escrow-2026"));
        assert!(error.contains("you supplied wrong-2026"));
    }

    #[test]
    fn default_staging_stays_on_existing_output_filesystem() {
        let temp = tempfile::tempdir().unwrap();
        let existing_out = temp.path().join("mounted-output");
        fs::create_dir(&existing_out).unwrap();

        assert_eq!(default_staging_dir(&existing_out), existing_out);
        let stage = SecurePlaintextStage::new_in(default_staging_dir(&existing_out)).unwrap();
        assert_eq!(stage.path().parent().unwrap(), existing_out);
    }

    #[test]
    fn default_staging_uses_parent_for_uncreated_output() {
        let temp = tempfile::tempdir().unwrap();
        let uncreated_out = temp.path().join("future-output");

        assert!(!uncreated_out.exists());
        assert_eq!(default_staging_dir(&uncreated_out), temp.path());
        let stage = SecurePlaintextStage::new_in(default_staging_dir(&uncreated_out)).unwrap();
        assert_eq!(stage.path().parent().unwrap(), temp.path());
    }
}
