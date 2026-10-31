//! Complete catalog data as a CBOR sequence: one object header, then member maps.
//! Encoding writes directly to any Write, including a chunking/spooling writer.
//! Validation accepts arbitrary chunk boundaries and retains only a schema stack,
//! a nine-byte CBOR head and one UTF-8 scalar. Even an individual text field may
//! exceed a frame. Projection consumers may spool the same validated bytes;
//! nothing in the replay path deserializes a whole object or member list.

use super::{
    codec::CodecError,
    wire::{self, Wire},
    Fault, ObjectRecoveryRow,
};
use crate::{
    NativeObjectCopyProjectionInput, NativeObjectFileProjectionInput, NativeObjectProjectionInput,
};
use std::io::{self, Write};

/// Complete checkpoint projection metadata, with members streamed separately.
/// Recovery fields remain structural as well, so row readers never need catalogs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommittedObjectProjection {
    /// Object identity, caller, body format, sizes, hashes and timestamp.
    pub object: NativeObjectProjectionInput,
    /// Copy identity, encryption, protection and digest metadata.
    pub copy: NativeObjectCopyProjectionInput,
    /// Fixed record size.
    pub block_size: u32,
    /// Stored object blocks.
    pub block_count: u64,
    /// Historical projection input, retained without synthesizing BOT authority.
    pub fresh_tape: bool,
    /// T after this object.
    pub total_committed_ordinals: u64,
    /// Full representation-aware recovery anchors.
    pub object_recovery_row: ObjectRecoveryRow,
}
/// Write one validated catalog header; rejection writes no bytes.
pub fn encode_header(
    w: &mut impl Write,
    header: &CommittedObjectProjection,
) -> Result<(), CodecError> {
    let bad = |why| CodecError::EncodeSchema(Fault::Invalid(why));
    for digest in [
        &header.object.content_hash,
        &header.object.metadata_hash,
        &header.copy.plaintext_digest,
        &header.copy.stored_digest,
    ] {
        if digest.as_ref().is_some_and(|value| value.len() != 32) {
            return Err(bad("catalog digest length"));
        }
    }
    if header.object.object_id != header.copy.object_id
        || header.object_recovery_row.object_id.as_deref()
            != Some(header.object.object_id.as_bytes())
        || header.object_recovery_row.tape_file_number != header.copy.tape_file_number
        || header.object_recovery_row.stored_block_count != header.block_count
    {
        return Err(bad("catalog header identity"));
    }
    if !remanence_parity::DEFAULT_BOOTSTRAP_CANDIDATE_BLOCK_SIZES.contains(&header.block_size) {
        return Err(bad("catalog block size"));
    }
    if header.copy.first_parity_data_ordinal.is_some_and(|first| {
        first.checked_add(header.block_count) != Some(header.total_committed_ordinals)
    }) {
        return Err(bad("catalog data ordinals"));
    }
    remanence_parity::object_recovery::validate_object_recovery_row_fields(
        header.block_count,
        header.object_recovery_row.object_id.as_deref(),
        &header.object_recovery_row.representation,
        Some(header.block_size),
    )
    .map_err(|_| bad("catalog recovery anchors"))?;
    // Validate binary recipient IDs and bounds before writing even the header.
    header
        .put(&mut io::sink())
        .map_err(|_| bad("catalog encode schema"))?;
    header.put(w).map_err(CodecError::Io)
}
/// Write one member bound to its header; rejection writes no bytes.
/// All binary hashes are CBOR byte strings.
pub fn encode_member(
    w: &mut impl Write,
    header: &CommittedObjectProjection,
    member: &NativeObjectFileProjectionInput,
) -> Result<(), CodecError> {
    if member.object_id != header.object.object_id {
        return Err(CodecError::EncodeSchema(Fault::Invalid(
            "catalog member identity",
        )));
    }
    if member.file_sha256.len() != 32 {
        return Err(CodecError::EncodeSchema(Fault::Invalid(
            "catalog digest length",
        )));
    }
    member.put(w).map_err(CodecError::Io)
}

#[derive(Clone, Copy, Debug)]
enum Schema {
    BoundRecovery,
    Null,
    Uint,
    Exact(u64),
    Bool,
    Bytes(Option<u64>),
    Text,
    Id,
    Tape,
    Map(&'static [Schema]),
    Optional(&'static Schema),
    Recipients,
}
use Schema::*;
static OBJECT: &[Schema] = &[
    Id,
    Optional(&Text),
    Text,
    Optional(&Uint),
    Optional(&Bytes(Some(32))),
    Optional(&Bytes(Some(32))),
    Optional(&Text),
];
static COPY: &[Schema] = &[
    Id,
    Tape,
    Uint,
    Uint,
    Optional(&Uint),
    Optional(&Uint),
    Text,
    Text,
    Optional(&Recipients),
    Optional(&Uint),
    Optional(&Bytes(Some(32))),
    Optional(&Bytes(Some(32))),
];
static HEADER: &[Schema] = &[
    Map(OBJECT),
    Map(COPY),
    Uint,
    Uint,
    Bool,
    Uint,
    BoundRecovery,
];
static MEMBER: &[Schema] = &[
    Id,
    Text,
    Text,
    Uint,
    Bytes(Some(32)),
    Optional(&Uint),
    Uint,
    Optional(&Text),
    Optional(&Bool),
];

/// Constant-memory catalog validator, bound to one structural Object identity.
/// Counts decoded member maps, not a caller-supplied count in a frame envelope.
#[derive(Debug)]
pub struct CatalogValidator {
    stack: Vec<Schema>,
    head: [u8; 9],
    head_len: usize,
    head_need: usize,
    remaining: u64,
    text: bool,
    utf8: [u8; 4],
    utf8_len: usize,
    utf8_need: usize,
    identity: Option<u8>,
    identity_pos: usize,
    object_id: String,
    recovery: Vec<u8>,
    file: u64,
    blocks: u64,
    block_size: u32,
    total: u64,
    first_ordinal: Option<u64>,
    tape_uuid: [u8; 16],
    header_done: bool,
    members: u64,
    active_item: bool,
}
impl CatalogValidator {
    /// The envelope's physical row supplies the expected object and tape identity.
    pub fn new(
        row: &super::TapeFileEntry,
        tape_uuid: [u8; 16],
        block_size: u32,
        total: u64,
    ) -> Result<Self, Fault> {
        let object_id = row
            .object_id
            .clone()
            .ok_or(Fault::Invalid("catalog Object identity"))?;
        let mut recovery = Vec::new();
        row.object_recovery_row
            .as_ref()
            .ok_or(Fault::Invalid("catalog recovery identity"))?
            .put(&mut recovery)
            .map_err(|_| Fault::Invalid("recovery encoding"))?;
        Ok(Self {
            recovery,
            file: row.tape_file_number,
            blocks: row.block_count,
            block_size,
            total,
            first_ordinal: row.first_parity_data_ordinal,
            stack: vec![Map(HEADER)],
            head: [0; 9],
            head_len: 0,
            head_need: 1,
            remaining: 0,
            text: false,
            utf8: [0; 4],
            utf8_len: 0,
            utf8_need: 0,
            identity: None,
            identity_pos: 0,
            object_id,
            tape_uuid,
            header_done: false,
            members: 0,
            active_item: true,
        })
    }
    /// Number of fully decoded member maps so far.
    pub fn members(&self) -> u64 {
        self.members
    }
    /// Heap retained, independent of bytes/members supplied (object ID <=64 bytes).
    pub fn retained_bytes(&self) -> usize {
        self.stack.capacity() * std::mem::size_of::<Schema>()
            + self.object_id.capacity()
            + self.recovery.capacity()
    }
    /// Validate another contiguous slice. Fields and UTF-8 scalars may cross frames.
    pub fn feed(&mut self, mut bytes: &[u8]) -> Result<(), Fault> {
        while !bytes.is_empty() {
            if self.remaining > 0 {
                let n = self.remaining.min(bytes.len() as u64) as usize;
                let chunk = &bytes[..n];
                if let Some(tape) = self.identity {
                    let expected = match tape {
                        0 => self.object_id.as_bytes(),
                        1 => self.tape_uuid.as_slice(),
                        _ => self.recovery.as_slice(),
                    };
                    if expected.get(self.identity_pos..self.identity_pos + n) != Some(chunk) {
                        return Err(Fault::Invalid("catalog identity mismatch"));
                    }
                    self.identity_pos += n;
                }
                if self.text {
                    for &b in chunk {
                        self.utf8_byte(b)?;
                    }
                }
                self.remaining -= n as u64;
                bytes = &bytes[n..];
                if self.remaining == 0 && self.utf8_len != 0 {
                    return Err(Fault::Invalid("incomplete UTF-8 scalar"));
                }
                self.finish_item()?;
                continue;
            }
            if !self.active_item {
                self.stack.push(Map(MEMBER));
                self.active_item = true;
            }
            let b = bytes[0];
            bytes = &bytes[1..];
            self.head[self.head_len] = b;
            self.head_len += 1;
            if self.head_len == 1 {
                self.head_need = match b & 31 {
                    0..=23 => 1,
                    24 => 2,
                    25 => 3,
                    26 => 5,
                    27 => 9,
                    _ => return Err(Fault::Invalid("indefinite/reserved CBOR")),
                };
            }
            if self.head_len == self.head_need {
                let (major, n) = wire::read_head(&mut &self.head[..self.head_len])
                    .map_err(|_| Fault::Invalid("nonminimal CBOR"))?;
                self.head_len = 0;
                let schema = self
                    .stack
                    .pop()
                    .ok_or(Fault::Invalid("catalog schema stack"))?;
                self.accept(schema, major, n)?;
                self.finish_item()?;
            }
        }
        Ok(())
    }
    /// A complete header and zero or more complete members, no partial token.
    pub fn finish(&self) -> Result<u64, Fault> {
        if !self.header_done || self.active_item || self.head_len != 0 || self.remaining != 0 {
            return Err(Fault::Invalid("incomplete catalog item"));
        }
        Ok(self.members)
    }
    fn finish_item(&mut self) -> Result<(), Fault> {
        if self.active_item && self.stack.is_empty() && self.remaining == 0 && self.head_len == 0 {
            if self.header_done {
                self.members = self.members.checked_add(1).ok_or(Fault::Overflow)?;
            } else {
                self.header_done = true;
            }
            self.active_item = false;
        }
        Ok(())
    }
    fn accept(&mut self, schema: Schema, m: u8, n: u64) -> Result<(), Fault> {
        let bad = Fault::Invalid("catalog CBOR schema");
        match schema {
            BoundRecovery => {
                // The full recovery row has already been structurally validated.
                // Compare its canonical bytes without decoding a second copy.
                let mut encoded = Vec::new();
                wire::head(&mut encoded, m, n).map_err(|_| bad)?;
                if !self.recovery.starts_with(&encoded) {
                    return Err(Fault::Invalid("catalog recovery mismatch"));
                }
                self.identity = Some(2);
                self.identity_pos = encoded.len();
                self.remaining = (self.recovery.len() - encoded.len()) as u64;
                self.text = false;
            }
            Null => {
                if (m, n) != (7, 22) {
                    return Err(bad);
                }
            }
            Optional(inner) => {
                if (m, n) != (7, 22) {
                    self.accept(*inner, m, n)?;
                }
            }
            Uint => {
                if m != 0 {
                    return Err(bad);
                }
            }
            Exact(expected) => {
                if (m, n) != (0, expected) {
                    return Err(bad);
                }
            }
            Bool => {
                if m != 7 || !matches!(n, 20 | 21) {
                    return Err(bad);
                }
            }
            Map(fields) => {
                if m != 5 || n != fields.len() as u64 {
                    return Err(bad);
                }
                for (i, field) in fields.iter().enumerate().rev() {
                    let schema = if std::ptr::eq(fields, HEADER) {
                        match i {
                            2 => Exact(self.block_size as u64),
                            3 => Exact(self.blocks),
                            5 => Exact(self.total),
                            _ => *field,
                        }
                    } else if std::ptr::eq(fields, COPY) {
                        match i {
                            2 => Exact(self.file),
                            4 => self.first_ordinal.map_or(Null, Exact),
                            _ => *field,
                        }
                    } else {
                        *field
                    };
                    self.stack.push(schema);
                    self.stack.push(Exact(i as u64));
                }
            }
            Recipients => {
                if m != 4 || n > 8 {
                    return Err(bad);
                }
                for _ in 0..n {
                    self.stack.push(Bytes(Some(16)));
                }
            }
            Bytes(_) | Text | Id | Tape => {
                let is_text = matches!(schema, Text | Id);
                if m != if is_text { 3 } else { 2 } {
                    return Err(bad);
                }
                if let Bytes(Some(len)) = schema {
                    if len != n {
                        return Err(bad);
                    }
                }
                self.identity = match schema {
                    Id => Some(0),
                    Tape => Some(1),
                    _ => None,
                };
                if let Some(tape) = self.identity {
                    let len = if tape == 1 { 16 } else { self.object_id.len() };
                    if n != len as u64 {
                        return Err(Fault::Invalid("catalog identity length"));
                    }
                }
                self.identity_pos = 0;
                self.remaining = n;
                self.text = is_text;
            }
        }
        if self.stack.len() > 64 {
            return Err(Fault::Invalid("catalog nesting"));
        }
        Ok(())
    }
    fn utf8_byte(&mut self, b: u8) -> Result<(), Fault> {
        if self.utf8_len == 0 {
            self.utf8_need = match b {
                0..=127 => return Ok(()),
                0xc2..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf4 => 4,
                _ => return Err(Fault::Invalid("UTF-8")),
            };
        }
        self.utf8[self.utf8_len] = b;
        self.utf8_len += 1;
        if self.utf8_len == self.utf8_need {
            std::str::from_utf8(&self.utf8[..self.utf8_len])
                .map_err(|_| Fault::Invalid("UTF-8"))?;
            self.utf8_len = 0;
        }
        Ok(())
    }
}
