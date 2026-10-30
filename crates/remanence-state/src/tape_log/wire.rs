//! Small schema-directed CBOR codec: no generic Value tree and no member vectors.

use super::*;
use crate::checkpoint::ManualTerminalFinalizationIdentity;
use crate::{
    NativeObjectCopyProjectionInput, NativeObjectFileProjectionInput, NativeObjectProjectionInput,
};
use remanence_parity::{
    ObjectRecoveryRepresentation, SchemeId, TapeFileKind, TerminalTailComponentKind,
    TerminalTailComponentPlan,
};
use std::io::{self, Read, Write};

pub(super) fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "noncanonical tape-log CBOR or invalid schema",
    )
}
pub(super) trait Wire: Sized {
    fn put(&self, w: &mut dyn Write) -> io::Result<()>;
    fn get(r: &mut dyn Read) -> io::Result<Self>;
}
pub(super) fn head(w: &mut dyn Write, major: u8, n: u64) -> io::Result<()> {
    let base = major << 5;
    match n {
        0..=23 => w.write_all(&[base | n as u8]),
        24..=255 => w.write_all(&[base | 24, n as u8]),
        256..=65535 => {
            w.write_all(&[base | 25])?;
            w.write_all(&(n as u16).to_be_bytes())
        }
        65536..=4294967295 => {
            w.write_all(&[base | 26])?;
            w.write_all(&(n as u32).to_be_bytes())
        }
        _ => {
            w.write_all(&[base | 27])?;
            w.write_all(&n.to_be_bytes())
        }
    }
}
pub(super) fn read_head(r: &mut dyn Read) -> io::Result<(u8, u64)> {
    let mut b = [0];
    r.read_exact(&mut b)?;
    let a = b[0] & 31;
    let n = match a {
        0..=23 => a as u64,
        24..=27 => {
            let len = 1usize << (a - 24);
            let mut bytes = [0; 8];
            r.read_exact(&mut bytes[8 - len..])?;
            let n = u64::from_be_bytes(bytes);
            let min = [24, 256, 65536, 4294967296][(a - 24) as usize];
            if n < min {
                return Err(invalid());
            }
            n
        }
        _ => return Err(invalid()),
    };
    Ok((b[0] >> 5, n))
}
fn expect(r: &mut dyn Read, major: u8) -> io::Result<u64> {
    let (m, n) = read_head(r)?;
    if m != major {
        return Err(invalid());
    }
    Ok(n)
}
macro_rules! uint {
    ($($ty:ty),+) => {$ (impl Wire for $ty {
        fn put(&self,w:&mut dyn Write)->io::Result<()> { head(w,0,*self as u64) }
        fn get(r:&mut dyn Read)->io::Result<Self> { expect(r,0)?.try_into().map_err(|_|invalid()) }
    })+};
}
uint!(u8, u16, u32, u64);
impl Wire for bool {
    fn put(&self, w: &mut dyn Write) -> io::Result<()> {
        w.write_all(&[if *self { 0xf5 } else { 0xf4 }])
    }
    fn get(r: &mut dyn Read) -> io::Result<Self> {
        match read_head(r)? {
            (7, 20) => Ok(false),
            (7, 21) => Ok(true),
            _ => Err(invalid()),
        }
    }
}
impl<T: Wire> Wire for Option<T> {
    fn put(&self, w: &mut dyn Write) -> io::Result<()> {
        match self {
            Some(v) => v.put(w),
            None => w.write_all(&[0xf6]),
        }
    }
    fn get(r: &mut dyn Read) -> io::Result<Self> {
        let mut b = [0];
        r.read_exact(&mut b)?;
        if b[0] == 0xf6 {
            Ok(None)
        } else {
            T::get(&mut b.as_slice().chain(r)).map(Some)
        }
    }
}
impl<const N: usize> Wire for [u8; N] {
    fn put(&self, w: &mut dyn Write) -> io::Result<()> {
        head(w, 2, N as u64)?;
        w.write_all(self)
    }
    fn get(r: &mut dyn Read) -> io::Result<Self> {
        if expect(r, 2)? != N as u64 {
            return Err(invalid());
        }
        let mut b = [0; N];
        r.read_exact(&mut b)?;
        Ok(b)
    }
}
impl Wire for Vec<u8> {
    fn put(&self, w: &mut dyn Write) -> io::Result<()> {
        head(w, 2, self.len() as u64)?;
        w.write_all(self)
    }
    fn get(r: &mut dyn Read) -> io::Result<Self> {
        bytes(r, 2)
    }
}
fn bytes(r: &mut dyn Read, major: u8) -> io::Result<Vec<u8>> {
    let len = expect(r, major)?;
    if len > MAX_FRAME_BODY as u64 {
        return Err(invalid());
    }
    // Grow only from actual bytes, not a hostile declared length.
    let mut out = Vec::new();
    let read = r.take(len).read_to_end(&mut out)?;
    if read as u64 != len {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(out)
}
impl Wire for String {
    fn put(&self, w: &mut dyn Write) -> io::Result<()> {
        head(w, 3, self.len() as u64)?;
        w.write_all(self.as_bytes())
    }
    fn get(r: &mut dyn Read) -> io::Result<Self> {
        String::from_utf8(bytes(r, 3)?).map_err(|_| invalid())
    }
}
// Catalog's existing API uses lowercase hexadecimal recipient IDs; the wire
// representation is still an array of binary UUID byte strings.
impl Wire for Vec<String> {
    fn put(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.len() > 8 {
            return Err(invalid());
        }
        head(w, 4, self.len() as u64)?;
        for id in self {
            if id.len() != 32 {
                return Err(invalid());
            }
            let mut bytes = [0; 16];
            for (out, pair) in bytes.iter_mut().zip(id.as_bytes().chunks_exact(2)) {
                let digit = |b: u8| match b {
                    b'0'..=b'9' => Ok(b - b'0'),
                    b'a'..=b'f' => Ok(b - b'a' + 10),
                    _ => Err(invalid()),
                };
                *out = digit(pair[0])? * 16 + digit(pair[1])?;
            }
            bytes.put(w)?;
        }
        Ok(())
    }
    fn get(r: &mut dyn Read) -> io::Result<Self> {
        let n = expect(r, 4)?;
        if n > 8 {
            return Err(invalid());
        }
        (0..n)
            .map(|_| {
                let bytes = <[u8; 16]>::get(r)?;
                Ok(bytes
                    .iter()
                    .flat_map(|b| {
                        [
                            b"0123456789abcdef"[(b >> 4) as usize] as char,
                            b"0123456789abcdef"[(b & 15) as usize] as char,
                        ]
                    })
                    .collect())
            })
            .collect()
    }
}
impl Wire for Vec<[u8; 16]> {
    fn put(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.len() > 8 {
            return Err(invalid());
        }
        head(w, 4, self.len() as u64)?;
        for s in self {
            s.put(w)?;
        }
        Ok(())
    }
    fn get(r: &mut dyn Read) -> io::Result<Self> {
        let n = expect(r, 4)?;
        if n > 8 {
            return Err(invalid());
        }
        (0..n).map(|_| <[u8; 16]>::get(r)).collect()
    }
}
impl Wire for CheckValue {
    fn put(&self, w: &mut dyn Write) -> io::Result<()> {
        self.0.to_le_bytes().put(w)
    }
    fn get(r: &mut dyn Read) -> io::Result<Self> {
        Ok(Self(u64::from_le_bytes(<[u8; 8]>::get(r)?)))
    }
}
impl Wire for SchemeId {
    fn put(&self, w: &mut dyn Write) -> io::Result<()> {
        self.as_str().to_owned().put(w)
    }
    fn get(r: &mut dyn Read) -> io::Result<Self> {
        Ok(Self::new_owned(String::get(r)?))
    }
}
macro_rules! enumeration {
    ($ty:ty; $($n:literal=>$variant:ident),+ $(,)?) => { impl Wire for $ty {
        fn put(&self,w:&mut dyn Write)->io::Result<()> { let n:u64=match self {$(Self::$variant=>$n),+}; n.put(w) }
        fn get(r:&mut dyn Read)->io::Result<Self> { match u64::get(r)? {$($n=>Ok(Self::$variant),)+_=>Err(invalid())} }
    }};
}
enumeration!(CheckProvenance; 0=>Empty,1=>OriginalWrite,2=>UnverifiedRebuild);
enumeration!(TapeFileKind; 0=>Object,1=>ParitySidecar,2=>Bootstrap,3=>ParityMap,4=>TapeIndexReplica,5=>IndexSeparationExtent);
enumeration!(TerminalTailComponentKind; 4=>TapeIndexReplica,5=>IndexSeparationExtent);
enumeration!(SealDueReason; 0=>HardwareEarlyWarning,1=>ReachedLowWatermark);
enumeration!(TerminalFinalizationTrigger; 0=>ReachedLowWatermark,1=>HardwareEarlyWarning,2=>OperatorCloseOut,3=>PoolCloseOut,4=>NoPendingObjectFits);

// Mandatory maps, keys in declaration order. A duplicate, unknown or reordered
// key is an error, even if a generic CBOR deserializer would accept it.
macro_rules! fields {
    ($ty:ty; $($field:ident),+ $(,)?) => { impl Wire for $ty {
        fn put(&self,w:&mut dyn Write)->io::Result<()> {
            let keys=[$(stringify!($field)),+]; head(w,5,keys.len() as u64)?;
            let mut key=0u64;
            $(key.put(w)?; self.$field.put(w)?; key+=1;)+ let _=key; Ok(())
        }
        fn get(r:&mut dyn Read)->io::Result<Self> {
            let keys=[$(stringify!($field)),+]; if expect(r,5)? != keys.len() as u64 {return Err(invalid());}
            let mut key=0u64;
            $(if u64::get(r)? != key { return Err(invalid()); } let $field=Wire::get(r)?; key+=1;)+ let _=key;
            Ok(Self {$($field),+})
        }
    }};
}
fields!(CommitCursor; partition,lba);
fields!(ParityScheme; id,data_blocks_per_stripe,parity_blocks_per_stripe,stripes_per_neighborhood);
fields!(TapeFileEntry; tape_file_number,kind,block_count,physical_start_hint,object_id,first_parity_data_ordinal,epoch_id,protected_ordinal_start,protected_ordinal_end_exclusive,canonical_metadata_hash,object_recovery_row);
fields!(ObjectRecoveryRow; tape_file_number,stored_block_count,object_id,representation);
fields!(Initialized; tape_uuid,incarnation,group,block_size,scheme,bot);
fields!(GroupRef; first_sequence,start_offset,frame_count,byte_count,member_count,digest);
fields!(Continuation; group,chunk);
fields!(Committed; group,cursor,protected,total,check,provenance,unverified_end,next_epoch,next_file,object_count,batch_id,session_id,seal_due);
fields!(SidecarEpochDirectoryEntry; tape_file_number,epoch_id,protected_ordinal_start,protected_ordinal_end_exclusive,sidecar_total_block_count,sidecar_header_block_count,parity_shard_block_count,canonical_metadata_hash,flags);
fields!(TerminalTailComponentPlan; kind,ordinal,planned_tape_file_number,planned_start_lba,record_count);
fields!(TerminalTailLayout; partition,block_size,components,expected_eod_lba);
fields!(ManualTerminalFinalizationIdentity; operation_id,operation_kind,actor_fingerprint,idempotency_key,request_fingerprint,assigned_pool_id,expected_pool_id,assignment_generation,reason);
fields!(FinalizationPlan; group,trigger,manual,edition_id,edition_sequence,edition_digest,writer_version,write_timestamp,layout,prefix_end);
fields!(NativeObjectProjectionInput; object_id,caller_object_id,body_format,logical_size_bytes,content_hash,metadata_hash,created_at_utc);
fields!(NativeObjectCopyProjectionInput; object_id,tape_uuid,tape_file_number,first_body_lba,first_parity_data_ordinal,protected_until_ordinal,status,representation,recipient_epoch_ids,metadata_frame_len,plaintext_digest,stored_digest);
fields!(NativeObjectFileProjectionInput; object_id,file_id,path,size_bytes,file_sha256,first_chunk_lba,chunk_count,mtime,executable);
fields!(payload::CommittedObjectProjection; object,copy,block_size,block_count,fresh_tape,total_committed_ordinals,object_recovery_row);
impl Wire for [TerminalTailComponentPlan; 5] {
    fn put(&self, w: &mut dyn Write) -> io::Result<()> {
        head(w, 4, 5)?;
        for c in self {
            c.put(w)?;
        }
        Ok(())
    }
    fn get(r: &mut dyn Read) -> io::Result<Self> {
        if expect(r, 4)? != 5 {
            return Err(invalid());
        }
        Ok([
            Wire::get(r)?,
            Wire::get(r)?,
            Wire::get(r)?,
            Wire::get(r)?,
            Wire::get(r)?,
        ])
    }
}
impl Wire for ObjectRecoveryRepresentation {
    fn put(&self, w: &mut dyn Write) -> io::Result<()> {
        match self {
            Self::Plaintext {
                manifest_first_chunk_lba,
                manifest_size_bytes,
                manifest_chunk_count,
                manifest_sha256,
            } => {
                head(w, 4, 5)?;
                0u8.put(w)?;
                manifest_first_chunk_lba.put(w)?;
                manifest_size_bytes.put(w)?;
                manifest_chunk_count.put(w)?;
                manifest_sha256.put(w)
            }
            Self::Encrypted {
                recipient_epoch_ids,
                metadata_frame_len,
                key_frame_len,
            } => {
                head(w, 4, 4)?;
                1u8.put(w)?;
                recipient_epoch_ids.put(w)?;
                metadata_frame_len.put(w)?;
                key_frame_len.put(w)
            }
        }
    }
    fn get(r: &mut dyn Read) -> io::Result<Self> {
        let n = expect(r, 4)?;
        match (n, u8::get(r)?) {
            (5, 0) => Ok(Self::Plaintext {
                manifest_first_chunk_lba: Wire::get(r)?,
                manifest_size_bytes: Wire::get(r)?,
                manifest_chunk_count: Wire::get(r)?,
                manifest_sha256: Wire::get(r)?,
            }),
            (4, 1) => Ok(Self::Encrypted {
                recipient_epoch_ids: Wire::get(r)?,
                metadata_frame_len: Wire::get(r)?,
                key_frame_len: Wire::get(r)?,
            }),
            _ => Err(invalid()),
        }
    }
}
impl Wire for Chunk {
    fn put(&self, w: &mut dyn Write) -> io::Result<()> {
        match self {
            Self::FileRow(row) => {
                head(w, 4, 2)?;
                0u8.put(w)?;
                row.put(w)
            }
            Self::Catalog {
                tape_file_number,
                ordinal,
                last,
            } => {
                head(w, 4, 4)?;
                1u8.put(w)?;
                tape_file_number.put(w)?;
                ordinal.put(w)?;
                last.put(w)
            }
            Self::PlanDirectory(row) => {
                head(w, 4, 2)?;
                2u8.put(w)?;
                row.put(w)
            }
        }
    }
    fn get(r: &mut dyn Read) -> io::Result<Self> {
        let n = expect(r, 4)?;
        match (n, u8::get(r)?) {
            (2, 0) => Ok(Self::FileRow(Wire::get(r)?)),
            (4, 1) => Ok(Self::Catalog {
                tape_file_number: Wire::get(r)?,
                ordinal: Wire::get(r)?,
                last: Wire::get(r)?,
            }),
            (2, 2) => Ok(Self::PlanDirectory(Wire::get(r)?)),
            _ => Err(invalid()),
        }
    }
}
pub(super) fn encode(record: &Record, w: &mut dyn Write) -> io::Result<()> {
    match record {
        Record::Initialized(v) => v.put(w),
        Record::PayloadContinuation(v) => v.put(w),
        Record::Committed(v) => v.put(w),
        Record::FinalizationDecided(v) | Record::FinalizationPrepared(v) => v.put(w),
        Record::TerminalPrefix { decision, commit } => {
            head(w, 4, 2)?;
            decision.put(w)?;
            commit.put(w)
        }
        Record::ComponentWritten {
            decision,
            component,
            commit,
        } => {
            head(w, 4, 3)?;
            decision.put(w)?;
            component.put(w)?;
            commit.put(w)
        }
        Record::Sealed {
            decision,
            cursor,
            edition_digest,
        } => {
            head(w, 4, 3)?;
            decision.put(w)?;
            cursor.put(w)?;
            edition_digest.put(w)
        }
        Record::FinalizationPreparationRetired { prepared } => prepared.put(w),
        Record::FinalizationRecoveryRequired { plan } => plan.put(w),
    }
}
pub(super) fn decode(tag: Tag, bytes: &[u8]) -> Result<Record, Fault> {
    let mut r = bytes;
    let result = (|| -> io::Result<Record> {
        Ok(match tag {
            Tag::Initialized => Record::Initialized(Wire::get(&mut r)?),
            Tag::PayloadContinuation => Record::PayloadContinuation(Wire::get(&mut r)?),
            Tag::Committed => Record::Committed(Wire::get(&mut r)?),
            Tag::FinalizationDecided => Record::FinalizationDecided(Box::new(Wire::get(&mut r)?)),
            Tag::FinalizationPrepared => Record::FinalizationPrepared(Box::new(Wire::get(&mut r)?)),
            Tag::TerminalPrefix => {
                if expect(&mut r, 4)? != 2 {
                    return Err(invalid());
                }
                Record::TerminalPrefix {
                    decision: Wire::get(&mut r)?,
                    commit: Wire::get(&mut r)?,
                }
            }
            Tag::ComponentWritten => {
                if expect(&mut r, 4)? != 3 {
                    return Err(invalid());
                }
                Record::ComponentWritten {
                    decision: Wire::get(&mut r)?,
                    component: Wire::get(&mut r)?,
                    commit: Wire::get(&mut r)?,
                }
            }
            Tag::Sealed => {
                if expect(&mut r, 4)? != 3 {
                    return Err(invalid());
                }
                Record::Sealed {
                    decision: Wire::get(&mut r)?,
                    cursor: Wire::get(&mut r)?,
                    edition_digest: Wire::get(&mut r)?,
                }
            }
            Tag::FinalizationPreparationRetired => Record::FinalizationPreparationRetired {
                prepared: Wire::get(&mut r)?,
            },
            Tag::FinalizationRecoveryRequired => Record::FinalizationRecoveryRequired {
                plan: Wire::get(&mut r)?,
            },
            _ => return Err(invalid()),
        })
    })();
    let record = result.map_err(|_| Fault::Invalid("CBOR schema"))?;
    if !r.is_empty() {
        return Err(Fault::Invalid("trailing structural bytes"));
    }
    Ok(record)
}
