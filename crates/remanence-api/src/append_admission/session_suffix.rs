//! Bounded native-session suffix proofs. Named tails constrain physical command
//! completion; crash-only TornPrefix identities additionally bind every byte.
use super::AdmissionError;
use remanence_library::{DriveHandle, TapeIoError};
use remanence_state::tape_log::{TailFile, TailFilemark, TailIdentity};
use sha2::{Digest, Sha256};

fn contradiction(detail: &str) -> AdmissionError {
    AdmissionError::Unproved(format!("SessionSuffixContradiction: {detail}"))
}

pub(super) fn eod(drive: &mut DriveHandle, size: u32) -> Result<(), AdmissionError> {
    let mut block = vec![0; size as usize];
    match drive.read_block(&mut block) {
        Err(e) if e.is_physical_eod() => Ok(()),
        Err(TapeIoError::FilemarkEncountered) | Ok(_) => {
            Err(contradiction("unexpected file after named suffix"))
        }
        Err(e) => Err(e.into()),
    }
}

/// An interrupted own closeout must be a byte-for-byte prefix of the checked
/// deterministic encoding, with at most its one closing filemark, then EOD.
pub(super) fn own_closeout(
    drive: &mut DriveHandle,
    blocks: &[Vec<u8>],
    size: u32,
) -> Result<(u64, bool, [u8; 32]), AdmissionError> {
    let mut block = vec![0; size as usize];
    let mut digest = Sha256::new();
    let mut count = 0;
    for expected in blocks {
        match drive.read_block(&mut block) {
            Err(e) if e.is_physical_eod() => return Ok((count, false, digest.finalize().into())),
            Ok(n) if n == expected.len() && block[..n] == expected[..] => {
                digest.update(&block);
                count += 1;
            }
            Ok(_) | Err(TapeIoError::FilemarkEncountered) => {
                return Err(contradiction(
                    "closeout sidecar differs from checked encoding",
                ))
            }
            Err(e) => return Err(e.into()),
        }
    }
    match drive.read_block(&mut block) {
        Err(e) if e.is_physical_eod() => Ok((count, false, digest.finalize().into())),
        Err(TapeIoError::FilemarkEncountered) => {
            eod(drive, size)?;
            Ok((count, true, digest.finalize().into()))
        }
        Ok(_) => Err(contradiction("closeout sidecar exceeds encoded length")),
        Err(e) => Err(e.into()),
    }
}

pub(super) fn named_file(
    drive: &mut DriveHandle,
    uuid: &[u8; 16],
    size: u32,
    file: &TailFile,
) -> Result<bool, AdmissionError> {
    let start = drive.position()?.lba;
    let mut block = vec![0; size as usize];
    let mut first = None;
    let mut digest = Sha256::new();
    let max = file
        .known_blocks
        .checked_add(file.unknown_blocks)
        .ok_or_else(|| contradiction("tail block overflow"))?;
    let mut count = 0;
    let mark = loop {
        match drive.read_block(&mut block) {
            Ok(n) if n == size as usize && count < max => {
                if first.is_none() {
                    first = Some(block.clone());
                }
                digest.update(&block);
                count += 1;
            }
            Ok(_) => return Err(contradiction("tail block count or geometry differs")),
            Err(TapeIoError::FilemarkEncountered) => break true,
            Err(e) if e.is_physical_eod() => break false,
            Err(e) => return Err(e.into()),
        }
    };
    if count < file.synchronized_blocks
        || (file.synchronized_filemark && !mark)
        || (mark && count != file.known_blocks)
        || (file.filemark == TailFilemark::Absent && mark)
    {
        return Err(contradiction(
            "tail completion differs from recorded command outcomes",
        ));
    }
    let end = drive.position()?.lba;
    match (&file.identity, first) {
        (TailIdentity::TornPrefix(expected), _) => {
            if count != file.known_blocks
                || mark
                || <[u8; 32]>::from(digest.finalize()) != *expected
            {
                return Err(contradiction(
                    "crash torn-prefix digest or boundary differs",
                ));
            }
        }
        (_, None) if file.synchronized_blocks == 0 && !mark => (),
        (TailIdentity::Sidecar(expected), Some(first)) => {
            let header = remanence_parity::sidecar::parse_sidecar_header_block(&first, uuid)
                .map_err(|_| contradiction("named sidecar header is unrecognizable"))?;
            if header.canonical_metadata_hash != *expected {
                return Err(contradiction("named sidecar identity differs"));
            }
        }
        (TailIdentity::Object(expected), Some(first)) => {
            if first.starts_with(b"REMO") {
                let h = remanence_aead::RemObjectHeader::parse(
                    first[..128]
                        .try_into()
                        .map_err(|_| contradiction("short object header"))?,
                )
                .map_err(|_| contradiction("invalid object envelope header"))?;
                if h.object_id != *expected || h.chunk_size != size {
                    return Err(contradiction("named object identity differs"));
                }
            } else {
                crate::catalogless_restore::prove_named_plain_identity(
                    drive, start, count, size, expected, !mark,
                )?;
                drive.locate(end)?;
            }
        }
        _ => return Err(contradiction("empty named file")),
    }
    Ok(mark)
}
