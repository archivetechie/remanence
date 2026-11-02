//! Dispatch evidence for one session, bounded to one in-memory file. Completed
//! files are spooled, not retained with the epoch. This is live evidence only:
//! SessionEnded makes it durable; a process crash still uses the crash suffix row.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

use remanence_library::transport::TransferOutcome;
use remanence_library::{MediaDispatchObserver, TapeIoError};
use remanence_scsi::ScsiError;
use remanence_state::tape_log::{TailFile, TailFilemark, TailIdentity};
use remanence_state::StateError;

#[derive(Clone, Debug)]
pub(crate) struct SessionTail(Arc<Mutex<Evidence>>);

#[derive(Debug)]
struct Evidence {
    spool: File,
    count: u64,
    next_file: u64,
    current: Option<TailFile>,
    next_object: Option<String>,
    block_size: u32,
    tape_uuid: [u8; 16],
    stopped: bool,
    spool_failed: bool,
}

fn invalid() -> TapeIoError {
    TapeIoError::InvalidRequest(ScsiError::InvalidInput("session tail evidence unavailable"))
}

impl SessionTail {
    pub(crate) fn new(
        tape_uuid: [u8; 16],
        block_size: u32,
        next_file: u64,
    ) -> std::io::Result<Self> {
        Ok(Self(Arc::new(Mutex::new(Evidence {
            spool: tempfile::tempfile()?,
            count: 0,
            next_file,
            current: None,
            next_object: None,
            block_size,
            tape_uuid,
            stopped: false,
            spool_failed: false,
        }))))
    }

    pub(crate) fn observe(
        &self,
        identity: TailIdentity,
        blocks: u64,
        filemark: TailFilemark,
    ) -> std::io::Result<()> {
        let next_file = self.0.lock().expect("session tail lock").next_file;
        self.observed(TailFile {
            tape_file_number: next_file,
            identity,
            known_blocks: blocks,
            unknown_blocks: 0,
            filemark,
        })
    }

    pub(crate) fn observed(&self, file: TailFile) -> std::io::Result<()> {
        let mut e = self.0.lock().expect("session tail lock");
        e.flush_current()?;
        if file.tape_file_number != e.next_file {
            return Err(std::io::Error::other("observed tail file sequence"));
        }
        e.next_file = e
            .next_file
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("observed tail file overflow"))?;
        e.current = Some(file);
        Ok(())
    }

    pub(crate) fn drive_failed(&self) -> bool {
        self.0.lock().expect("session tail lock").stopped
    }

    /// Set by object preparation before any of its blocks can be dispatched.
    pub(crate) fn object(&self, id: &str) -> Result<(), String> {
        let mut e = self.0.lock().map_err(|_| "session tail lock poisoned")?;
        if e.stopped || e.spool_failed || id.is_empty() || id.len() > 64 {
            return Err("session tail object identity unavailable".into());
        }
        e.next_object = Some(id.to_owned());
        Ok(())
    }

    /// Only a successful durable publication may consume dispatch evidence.
    pub(crate) fn committed(&self, next_file: u64) -> std::io::Result<()> {
        let mut e = self.0.lock().expect("session tail lock");
        e.spool.set_len(0)?;
        e.spool.rewind()?;
        e.count = 0;
        e.next_file = next_file;
        e.current = None;
        e.next_object = None;
        e.stopped = false;
        e.spool_failed = false;
        Ok(())
    }

    pub(crate) fn digest(&self) -> std::io::Result<[u8; 32]> {
        use sha2::{Digest, Sha256};
        let mut files = self.files()?;
        let mut hash = Sha256::new();
        let mut block = [0; 8192];
        loop {
            let n = files.spool.read(&mut block)?;
            if n == 0 {
                break;
            }
            hash.update(&block[..n]);
        }
        Ok(hash.finalize().into())
    }

    /// Freeze a streaming reader after detaching the observer from the drive.
    pub(crate) fn files(&self) -> std::io::Result<TailFiles> {
        let mut e = self.0.lock().expect("session tail lock");
        e.flush_current()?;
        let mut spool = e.spool.try_clone()?;
        spool.rewind()?;
        Ok(TailFiles {
            spool,
            remaining: e.count,
        })
    }
}

impl Evidence {
    fn flush_current(&mut self) -> std::io::Result<()> {
        if self.spool_failed {
            return Err(std::io::Error::other("session tail spool failed"));
        }
        if let Some(file) = self.current.as_ref() {
            let result = (|| {
                let next_count = self
                    .count
                    .checked_add(1)
                    .ok_or_else(|| std::io::Error::other("tail count overflow"))?;
                let offset = self.spool.seek(SeekFrom::End(0))?;
                if let Err(error) = write_file(&mut self.spool, file) {
                    // A partial append must never become another tail record.
                    self.spool.set_len(offset)?;
                    return Err(error);
                }
                self.count = next_count;
                Ok(())
            })();
            if result.is_err() {
                self.spool_failed = true;
            }
            result?;
            self.current = None;
        }
        Ok(())
    }
}

impl MediaDispatchObserver for SessionTail {
    fn dispatching(&mut self, cdb: &[u8], data: Option<&[u8]>) -> Result<(), TapeIoError> {
        if !matches!(cdb.first(), Some(0x0a | 0x10)) {
            return Ok(());
        }
        let mut e = self.0.lock().map_err(|_| invalid())?;
        let count = cdb
            .get(2..5)
            .ok_or_else(invalid)?
            .iter()
            .fold(0u64, |n, b| n * 256 + u64::from(*b));
        if e.stopped || e.spool_failed {
            return Err(invalid());
        }
        // WFM(0) settles buffered work but does not introduce a tail file.
        if cdb[0] == 0x10 && count == 0 {
            return Ok(());
        }
        if cdb[0] == 0x0a {
            let bytes = data.ok_or_else(invalid)?;
            let blocks = if cdb[1] & 1 == 1 { count } else { 1 };
            if blocks == 0
                || e.block_size == 0
                || bytes.len() as u64 != blocks * u64::from(e.block_size)
            {
                return Err(invalid());
            }
            if e.current
                .as_ref()
                .is_some_and(|f| f.filemark == TailFilemark::Present)
            {
                e.flush_current().map_err(|_| invalid())?;
            }
            if e.current.is_none() {
                let head = &bytes[..e.block_size as usize];
                let identity = if let Ok(header) =
                    remanence_parity::sidecar::parse_sidecar_header_block(head, &e.tape_uuid)
                {
                    TailIdentity::Sidecar(header.canonical_metadata_hash)
                } else {
                    TailIdentity::Object(e.next_object.take().ok_or_else(invalid)?)
                };
                e.current = Some(TailFile {
                    tape_file_number: e.next_file,
                    identity,
                    known_blocks: 0,
                    unknown_blocks: 0,
                    filemark: TailFilemark::Absent,
                });
            }
            let file = e.current.as_mut().ok_or_else(invalid)?;
            file.known_blocks.checked_add(blocks).ok_or_else(invalid)?;
            file.unknown_blocks = blocks;
        } else {
            if count != 1 || e.next_file == u64::MAX {
                return Err(invalid());
            }
            let file = e.current.as_mut().ok_or_else(invalid)?;
            if file.filemark != TailFilemark::Absent || file.unknown_blocks != 0 {
                return Err(invalid());
            }
            file.filemark = TailFilemark::Unknown;
        }
        Ok(())
    }

    fn settled(&mut self, cdb: &[u8], result: &Result<Option<TransferOutcome>, ScsiError>) {
        if !matches!(cdb.first(), Some(0x0a | 0x10)) {
            return;
        }
        let mut e = self.0.lock().expect("session tail lock");
        if cdb[0] == 0x10 && cdb[2..5] == [0, 0, 0] {
            if result.is_err() {
                e.stopped = true;
            }
            return;
        }
        let size = u64::from(e.block_size);
        let Some(file) = e.current.as_mut() else {
            e.stopped = true;
            return;
        };
        if cdb[0] == 0x0a {
            if let Ok(Some(outcome)) = result {
                let known = (u64::from(outcome.bytes_transferred) / size).min(file.unknown_blocks);
                file.known_blocks += known;
                file.unknown_blocks -= known;
            }
            e.stopped = file.unknown_blocks != 0 || result.is_err();
        } else if result.is_ok() {
            file.filemark = TailFilemark::Present;
            match e.next_file.checked_add(1) {
                Some(next) => e.next_file = next,
                None => e.stopped = true,
            }
        } else {
            e.stopped = true;
        }
    }
}

pub(crate) struct TailFiles {
    spool: File,
    remaining: u64,
}
impl Iterator for TailFiles {
    type Item = Result<TailFile, StateError>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        Some(
            read_file(&mut self.spool)
                .map_err(|e| StateError::JournalReplayFailed(format!("session tail spool: {e}"))),
        )
    }
}

fn write_file(w: &mut File, f: &TailFile) -> std::io::Result<()> {
    for n in [f.tape_file_number, f.known_blocks, f.unknown_blocks] {
        w.write_all(&n.to_le_bytes())?;
    }
    w.write_all(&[match f.filemark {
        TailFilemark::Present => 0,
        TailFilemark::Absent => 1,
        TailFilemark::Unknown => 2,
    }])?;
    match &f.identity {
        TailIdentity::Object(id) => {
            w.write_all(&[0])?;
            w.write_all(&(id.len() as u32).to_le_bytes())?;
            w.write_all(id.as_bytes())
        }
        TailIdentity::Sidecar(hash) => {
            w.write_all(&[1])?;
            w.write_all(hash)
        }
        TailIdentity::TornPrefix(hash) => {
            w.write_all(&[2])?;
            w.write_all(hash)
        }
    }
}

fn read_file(r: &mut File) -> std::io::Result<TailFile> {
    let mut n = || {
        let mut b = [0; 8];
        r.read_exact(&mut b).map(|()| u64::from_le_bytes(b))
    };
    let (tape_file_number, known_blocks, unknown_blocks) = (n()?, n()?, n()?);
    let mut tags = [0; 2];
    r.read_exact(&mut tags)?;
    let bad = || std::io::Error::other("invalid session tail spool");
    let filemark = match tags[0] {
        0 => TailFilemark::Present,
        1 => TailFilemark::Absent,
        2 => TailFilemark::Unknown,
        _ => return Err(bad()),
    };
    let identity = match tags[1] {
        0 => {
            let mut b = [0; 4];
            r.read_exact(&mut b)?;
            let len = u32::from_le_bytes(b) as usize;
            if len == 0 || len > 64 {
                return Err(bad());
            }
            let mut id = vec![0; len];
            r.read_exact(&mut id)?;
            TailIdentity::Object(String::from_utf8(id).map_err(|_| bad())?)
        }
        1 => {
            let mut hash = [0; 32];
            r.read_exact(&mut hash)?;
            TailIdentity::Sidecar(hash)
        }
        2 => {
            let mut hash = [0; 32];
            r.read_exact(&mut hash)?;
            TailIdentity::TornPrefix(hash)
        }
        _ => return Err(bad()),
    };
    Ok(TailFile {
        tape_file_number,
        identity,
        known_blocks,
        unknown_blocks,
        filemark,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_files_stream_and_unknown_last_command_stops_writes() {
        let mut tail = SessionTail::new([1; 16], 512, 4).unwrap();
        for n in 0..1000 {
            tail.object(&format!("object-{n}")).unwrap();
            tail.dispatching(&[0x0a, 1, 0, 0, 2, 0], Some(&[0; 1024]))
                .unwrap();
            tail.settled(
                &[0x0a, 1, 0, 0, 2, 0],
                &Ok(Some(TransferOutcome::clean(1024))),
            );
            tail.dispatching(&[0x10, 1, 0, 0, 1, 0], None).unwrap();
            tail.settled(&[0x10, 1, 0, 0, 1, 0], &Ok(None));
            assert!(tail.0.lock().unwrap().current.is_some());
        }
        tail.object("last").unwrap();
        tail.dispatching(&[0x0a, 1, 0, 0, 3, 0], Some(&[0; 1536]))
            .unwrap();
        tail.settled(
            &[0x0a, 1, 0, 0, 3, 0],
            &Ok(Some(TransferOutcome::clean(512))),
        );
        assert!(tail.dispatching(&[0x10, 1, 0, 0, 1, 0], None).is_err());
        let mut files = tail.files().unwrap();
        for n in 0..1000 {
            let file = files.next().unwrap().unwrap();
            assert_eq!(file.tape_file_number, 4 + n);
            assert_eq!(file.identity, TailIdentity::Object(format!("object-{n}")));
            assert_eq!(
                (file.known_blocks, file.unknown_blocks, file.filemark),
                (2, 0, TailFilemark::Present)
            );
        }
        let file = files.next().unwrap().unwrap();
        assert_eq!(
            (file.known_blocks, file.unknown_blocks, file.filemark),
            (1, 2, TailFilemark::Absent)
        );
        assert!(files.next().is_none());
    }

    #[test]
    fn lost_write_and_filemark_responses_remain_unknown_and_barriers_add_no_file() {
        for mark in [false, true] {
            let mut tail = SessionTail::new([1; 16], 512, 1).unwrap();
            tail.object("object").unwrap();
            let write = [0x0a, 1, 0, 0, 1, 0];
            tail.dispatching(&write, Some(&[0; 512])).unwrap();
            let lost = Err(ScsiError::InvalidInput("completed but response lost"));
            if mark {
                tail.settled(&write, &Ok(Some(TransferOutcome::clean(512))));
                tail.dispatching(&[0x10, 1, 0, 0, 1, 0], None).unwrap();
                tail.settled(&[0x10, 1, 0, 0, 1, 0], &lost);
            } else {
                tail.settled(&write, &lost);
            }
            assert!(tail.dispatching(&[0x10, 0, 0, 0, 0, 0], None).is_err());
            let files = tail
                .files()
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(files.len(), 1);
            assert_eq!(
                (files[0].known_blocks, files[0].unknown_blocks),
                if mark { (1, 0) } else { (0, 1) }
            );
            assert_eq!(
                files[0].filemark,
                if mark {
                    TailFilemark::Unknown
                } else {
                    TailFilemark::Absent
                }
            );
            tail.committed(2).unwrap();
            assert_eq!(tail.files().unwrap().count(), 0);
        }
    }
}
