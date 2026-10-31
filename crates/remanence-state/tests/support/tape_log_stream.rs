//! Bounded generated catalog stream shared by the isolated allocation tests.
use remanence_parity::{SchemeId, TapeFileKind};
use remanence_state::tape_log::codec::{encode_frame, Decoder};
use remanence_state::tape_log::fold::GroupDigest;
use remanence_state::tape_log::payload::{encode_header, encode_member, CommittedObjectProjection};
use remanence_state::tape_log::*;
use remanence_state::{
    NativeObjectCopyProjectionInput, NativeObjectFileProjectionInput, NativeObjectProjectionInput,
};
use std::io::{self, Read};
const UUID: [u8; 16] = [7; 16];
const BLOCK: u32 = 262144;
fn scheme() -> ParityScheme {
    ParityScheme {
        id: SchemeId::new_static("rs-cauchy-gf256-v1"),
        data_blocks_per_stripe: 2,
        parity_blocks_per_stripe: 1,
        stripes_per_neighborhood: 4,
    }
}
fn plain_row(file: u64, kind: TapeFileKind, blocks: u64, lba: u64) -> TapeFileEntry {
    TapeFileEntry {
        tape_file_number: file,
        kind,
        block_count: blocks,
        physical_start_hint: Some(lba),
        object_id: None,
        first_parity_data_ordinal: None,
        epoch_id: None,
        protected_ordinal_start: None,
        protected_ordinal_end_exclusive: None,
        canonical_metadata_hash: None,
        object_recovery_row: None,
    }
}
fn init(parity: bool) -> Record {
    Record::Initialized(Initialized {
        tape_uuid: UUID,
        incarnation: 1,
        group: 1,
        block_size: BLOCK,
        scheme: parity.then(scheme),
        bot: plain_row(0, TapeFileKind::Bootstrap, 1, 0),
    })
}
fn object(file: u64, lba: u64, first: Option<u64>, blocks: u64) -> TapeFileEntry {
    let mut row = plain_row(file, TapeFileKind::Object, blocks, lba);
    row.object_id = Some("o".into());
    row.first_parity_data_ordinal = first;
    row.object_recovery_row = Some(
        ObjectRecoveryRow::plaintext(file, blocks, 0, 1, 1, [2; 32]).with_object_id(b"o".to_vec()),
    );
    row
}
fn header(row: &TapeFileEntry, total: u64) -> CommittedObjectProjection {
    CommittedObjectProjection {
        object: NativeObjectProjectionInput {
            object_id: "o".into(),
            caller_object_id: Some("caller".into()),
            body_format: "raw".into(),
            logical_size_bytes: Some(12),
            content_hash: Some(vec![3; 32]),
            metadata_hash: Some(vec![4; 32]),
            created_at_utc: Some("2026-10-31T00:00:00Z".into()),
        },
        copy: NativeObjectCopyProjectionInput {
            object_id: "o".into(),
            tape_uuid: UUID,
            tape_file_number: row.tape_file_number,
            first_body_lba: 0,
            first_parity_data_ordinal: row.first_parity_data_ordinal,
            protected_until_ordinal: Some(0),
            status: "committed".into(),
            representation: "plaintext".into(),
            recipient_epoch_ids: None,
            metadata_frame_len: None,
            plaintext_digest: Some(vec![5; 32]),
            stored_digest: Some(vec![6; 32]),
        },
        block_size: BLOCK,
        block_count: row.block_count,
        fresh_tape: true,
        total_committed_ordinals: total,
        object_recovery_row: row.object_recovery_row.clone().unwrap(),
    }
}
fn member() -> NativeObjectFileProjectionInput {
    NativeObjectFileProjectionInput {
        object_id: "o".into(),
        file_id: "f".into(),
        path: "path/λ/🦀".into(),
        size_bytes: 12,
        file_sha256: vec![9; 32],
        first_chunk_lba: Some(0),
        chunk_count: 1,
        mtime: Some("123.45".into()),
        executable: Some(true),
    }
}
fn commit(reference: GroupRef, next_file: u64, lba: u64, objects: u64) -> Committed {
    Committed {
        group: reference,
        cursor: CommitCursor { partition: 0, lba },
        protected: 0,
        total: 3,
        check: CheckValue::EMPTY,
        provenance: CheckProvenance::Empty,
        unverified_end: None,
        next_epoch: 0,
        next_file,
        object_count: objects,
        batch_id: [8; 16],
        session_id: Some([9; 16]),
        seal_due: None,
    }
}

/// Generates a log without retaining previous frames or a member vector.
/// Member IDs are distinct; only the current encoded member is reused.
pub struct MillionMemberStream {
    frame: Vec<u8>,
    position: usize,
    pub offset: u64,
    seq: u64,
    stage: u8,
    group: GroupDigest,
    member_bytes: Vec<u8>,
    id_offset: usize,
    members: u64,
    pub emitted: u64,
    chunk: u64,
    pub max_retained: usize,
}
impl MillionMemberStream {
    pub fn new(members: u64) -> Self {
        let mut m = member();
        m.file_id = "0000000000000000".into();
        m.path = "p".into();
        m.mtime = None;
        m.executable = None;
        let mut member_bytes = Vec::new();
        encode_member(&mut member_bytes, &header(&object(1, 2, None, 3), 3), &m).unwrap();
        let id_offset = member_bytes
            .windows(16)
            .position(|w| w == b"0000000000000000")
            .unwrap();
        Self {
            frame: Vec::new(),
            position: 0,
            offset: 0,
            seq: 0,
            stage: 0,
            group: GroupDigest::new(1, 0),
            member_bytes,
            id_offset,
            members,
            emitted: 0,
            chunk: 0,
            max_retained: 0,
        }
    }
    fn generate(&mut self) -> bool {
        self.offset += self.frame.len() as u64;
        self.frame.clear();
        self.position = 0;
        let mut payload = Vec::new();
        let record = match self.stage {
            0 => {
                self.stage = 1;
                init(false)
            }
            1 => {
                self.stage = 2;
                Record::PayloadContinuation(Continuation {
                    group: 1,
                    chunk: Chunk::FileRow(object(1, 2, None, 3)),
                })
            }
            2 => {
                if self.chunk == 0 {
                    encode_header(&mut payload, &header(&object(1, 2, None, 3), 3)).unwrap();
                }
                while self.emitted < self.members
                    && payload.len() + self.member_bytes.len() <= 16 * 1024
                {
                    let mut n = self.emitted;
                    for byte in self.member_bytes[self.id_offset..self.id_offset + 16]
                        .iter_mut()
                        .rev()
                    {
                        *byte = b"0123456789abcdef"[(n & 15) as usize];
                        n >>= 4;
                    }
                    payload.extend_from_slice(&self.member_bytes);
                    self.emitted += 1;
                }
                let ordinal = self.chunk;
                self.chunk += 1;
                let last = self.emitted == self.members;
                if last {
                    self.stage = 3;
                }
                Record::PayloadContinuation(Continuation {
                    group: 1,
                    chunk: Chunk::Catalog {
                        tape_file_number: 1,
                        ordinal,
                        last,
                    },
                })
            }
            3 => {
                self.stage = 4;
                Record::Committed(commit(self.group.reference(self.members), 2, 6, 1))
            }
            _ => return false,
        };
        self.seq += 1;
        encode_frame(&mut self.frame, self.seq, &record, &payload).unwrap();
        if self.stage != 4 {
            let mut decoder = Decoder::at(self.frame.as_slice(), self.seq, self.offset);
            let frame = decoder.next_frame().unwrap().unwrap();
            self.group.push(&frame).unwrap();
        }
        self.max_retained = self
            .max_retained
            .max(self.frame.capacity() + payload.capacity() + self.member_bytes.capacity());
        assert!(
            self.max_retained < 512 * 1024,
            "generator retained {}",
            self.max_retained
        );
        true
    }
}
impl Read for MillionMemberStream {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.position == self.frame.len() && !self.generate() {
            return Ok(0);
        }
        let n = out.len().min(self.frame.len() - self.position);
        out[..n].copy_from_slice(&self.frame[self.position..self.position + n]);
        self.position += n;
        Ok(n)
    }
}

/// One semantic Object report, without materializing its member list.
#[allow(dead_code)]
pub fn semantic_report() -> remanence_state::authority::AuthorityCommit {
    remanence_state::authority::AuthorityCommit {
        ordinal: 1,
        object_count: 1,
        protected: 0,
        total: 3,
        cursor: CommitCursor {
            partition: 0,
            lba: 6,
        },
        tape_uuid: UUID,
        operation_id: [8; 16],
        next_file: 2,
        block_size: BLOCK,
        payloads: vec![remanence_state::authority::ObjectPayload {
            projection: header(&object(1, 2, None, 3), 3),
            files: Vec::new(),
        }],
        scheme: None,
        object_files: Vec::new(),
        barrier: None,
        finalization: None,
        sealed: false,
        seal_due: None,
        check: None,
    }
}
/// Exactly the member used by MillionMemberStream, generated independently.
#[allow(dead_code)]
pub fn semantic_member(index: u64) -> NativeObjectFileProjectionInput {
    let mut m = member();
    m.file_id = format!("{index:016x}");
    m.path = "p".into();
    m.mtime = None;
    m.executable = None;
    m
}
