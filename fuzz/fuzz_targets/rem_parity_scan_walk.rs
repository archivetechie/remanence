#![no_main]

//! Structured fuzz target for the REM-PARITY 1.0 catalog-less scan walk and
//! overlay/validation (§12; freeze criterion §18.3). The fuzz input is
//! decoded into a compact synthetic tape (kind/damage tuples rendered into an
//! in-memory raw tape), then the production scan entry runs over it. The
//! property is robustness only — no panic, no hang, no unbounded
//! allocation — never recovery success.

use libfuzzer_sys::fuzz_target;
use remanence_parity::{
    derive_parity_map_magic, derive_sidecar_magic, scan_reconstruct_filemark_map_with_report,
    PhysicalPositionHint, RawReadOutcome, RawTapeSource, SpaceFilemarksOutcome,
};
use remanence_parity::error::ParityError;
use remanence_library::TapeIoError;
use std::collections::BTreeSet;

const BLOCK_SIZE: u32 = 4096;
const MAX_RECORDS: usize = 1024;
const MAX_TAPE_FILES: usize = 64;
const BOOTSTRAP_MAGIC: [u8; 8] = [0x52, 0x45, 0x4D, 0x00, 0x42, 0x4F, 0x4F, 0x01];

enum Rec {
    Block(Vec<u8>),
    Filemark,
}

struct FuzzRawTape {
    records: Vec<Rec>,
    cursor: usize,
    unreadable: BTreeSet<usize>,
}

impl RawTapeSource for FuzzRawTape {
    fn configure_fixed_block_size(&mut self, block_size: u32) -> Result<(), ParityError> {
        if block_size == 0 {
            return Err(ParityError::Invariant("fuzz block size is zero"));
        }
        Ok(())
    }

    fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
        self.cursor = usize::try_from(hint.lba)
            .unwrap_or(usize::MAX)
            .min(self.records.len());
        Ok(())
    }

    fn space_filemarks(&mut self, count: i64) -> Result<SpaceFilemarksOutcome, ParityError> {
        if count < 0 {
            return Err(ParityError::Invariant("fuzz tape spaces forward only"));
        }
        let mut spaced = 0i64;
        while spaced < count {
            match self.records.get(self.cursor) {
                Some(Rec::Filemark) => {
                    self.cursor += 1;
                    spaced += 1;
                }
                Some(Rec::Block(_)) => self.cursor += 1,
                None => {
                    return Ok(SpaceFilemarksOutcome {
                        filemarks_spaced: spaced,
                        position_after: PhysicalPositionHint::new(self.cursor as u64),
                        hit_end_of_data: true,
                    });
                }
            }
        }
        Ok(SpaceFilemarksOutcome {
            filemarks_spaced: spaced,
            position_after: PhysicalPositionHint::new(self.cursor as u64),
            hit_end_of_data: false,
        })
    }

    fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
        if self.unreadable.contains(&self.cursor) {
            self.cursor += 1;
            return Err(ParityError::TapeIo(TapeIoError::OperationFailed(
                "simulated unreadable raw record".to_string(),
            )));
        }
        match self.records.get(self.cursor) {
            Some(Rec::Block(block)) => {
                let n = block.len().min(buf.len());
                buf[..n].copy_from_slice(&block[..n]);
                self.cursor += 1;
                Ok(RawReadOutcome::Block {
                    bytes: n,
                    position_after: PhysicalPositionHint::new(self.cursor as u64),
                })
            }
            Some(Rec::Filemark) => {
                self.cursor += 1;
                Ok(RawReadOutcome::Filemark {
                    position_after: PhysicalPositionHint::new(self.cursor as u64),
                })
            }
            None => Ok(RawReadOutcome::EndOfData {
                position_after: PhysicalPositionHint::new(self.cursor as u64),
            }),
        }
    }

    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        Ok(PhysicalPositionHint::new(self.cursor as u64))
    }
}

/// Decode fuzz bytes into a bounded synthetic tape. Layout: 16 bytes tape
/// uuid, then per-record tuples `[tag, seed]`. Tag low nibble selects the
/// record kind; magic-prefixed blocks steer the walk into the classifier
/// paths without hand-holding it to success.
fn build_tape(data: &[u8]) -> Option<(FuzzRawTape, [u8; 16])> {
    if data.len() < 18 {
        return None;
    }
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&data[..16]);
    let sidecar_magic = derive_sidecar_magic(&uuid);
    let map_magic = derive_parity_map_magic(&uuid);

    let mut records = Vec::new();
    let mut unreadable = BTreeSet::new();
    let mut tape_files = 0usize;
    let mut iter = data[16..].chunks_exact(2);
    for pair in &mut iter {
        if records.len() >= MAX_RECORDS || tape_files >= MAX_TAPE_FILES {
            break;
        }
        let (tag, seed) = (pair[0], pair[1]);
        match tag & 0x07 {
            0 => {
                records.push(Rec::Filemark);
                tape_files += 1;
            }
            1 => {
                unreadable.insert(records.len());
                records.push(Rec::Block(vec![seed; BLOCK_SIZE as usize]));
            }
            2 => records.push(Rec::Block(magic_block(&BOOTSTRAP_MAGIC, seed))),
            3 => records.push(Rec::Block(magic_block(&sidecar_magic, seed))),
            4 => records.push(Rec::Block(magic_block(&map_magic, seed))),
            5 => {
                // Short (non-full) record — drives the short-read paths.
                let len = usize::from(seed).max(1).min(BLOCK_SIZE as usize - 1);
                records.push(Rec::Block(vec![seed; len]));
            }
            _ => records.push(Rec::Block(vec![seed; BLOCK_SIZE as usize])),
        }
    }
    Some((
        FuzzRawTape {
            records,
            cursor: 0,
            unreadable,
        },
        uuid,
    ))
}

fn magic_block(magic: &[u8; 8], seed: u8) -> Vec<u8> {
    let mut block = vec![seed; BLOCK_SIZE as usize];
    block[..8].copy_from_slice(magic);
    block
}

fuzz_target!(|data: &[u8]| {
    if data.len() > 1 << 16 {
        return;
    }
    let Some((mut tape, uuid)) = build_tape(data) else {
        return;
    };
    let _ = scan_reconstruct_filemark_map_with_report(&mut tape, &uuid, BLOCK_SIZE);
});
