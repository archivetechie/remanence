//! Neutral Object recovery rows shared by the journal and terminal index.

use ciborium::value::{Integer, Value as CborValue};
use serde::{Deserialize, Serialize};

use crate::cbor::IntegerMapKeyTracker;
use crate::error::ParityError;

const METADATA_FRAME_MIN_LEN: u64 = 17;
const METADATA_FRAME_MAX_LEN: u64 = 16 * 1024 * 1024;
const KEY_FRAME_MIN_LEN: u32 = 1191;
const KEY_FRAME_MAX_LEN: u32 = 16_384;
const MAX_RECIPIENTS: usize = 8;

/// One representation-aware Object recovery row.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ObjectRecoveryRow {
    /// Filemark-delimited tape-file number of the Object copy.
    pub tape_file_number: u64,
    /// Number of fixed-size tape blocks occupied by the stored copy.
    pub stored_block_count: u64,
    /// Verbatim 1–64-byte REM-OBJECT object identifier.
    pub object_id: Option<Vec<u8>>,
    /// Representation-specific recovery anchors for the copy.
    pub representation: ObjectRecoveryRepresentation,
}

impl ObjectRecoveryRow {
    /// Construct a plaintext REM-OBJECT recovery row.
    pub fn plaintext(
        tape_file_number: u64,
        stored_block_count: u64,
        manifest_first_chunk_lba: u64,
        manifest_size_bytes: u64,
        manifest_chunk_count: u64,
        manifest_sha256: [u8; 32],
    ) -> Self {
        Self {
            tape_file_number,
            stored_block_count,
            object_id: None,
            representation: ObjectRecoveryRepresentation::Plaintext {
                manifest_first_chunk_lba,
                manifest_size_bytes,
                manifest_chunk_count,
                manifest_sha256,
            },
        }
    }

    /// Construct an encrypted REM-OBJECT recovery row.
    pub fn encrypted(
        tape_file_number: u64,
        stored_block_count: u64,
        recipient_epoch_ids: Vec<[u8; 16]>,
        metadata_frame_len: u64,
        key_frame_len: u32,
    ) -> Self {
        Self {
            tape_file_number,
            stored_block_count,
            object_id: None,
            representation: ObjectRecoveryRepresentation::Encrypted {
                recipient_epoch_ids,
                metadata_frame_len,
                key_frame_len,
            },
        }
    }

    /// Bind the row to verbatim REM-OBJECT identifier bytes.
    pub fn with_object_id(mut self, object_id: impl Into<Vec<u8>>) -> Self {
        self.object_id = Some(object_id.into());
        self
    }
}

/// Representation-specific recovery anchors for one Object copy.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub enum ObjectRecoveryRepresentation {
    /// Plaintext REM-OBJECT representation with manifest anchors.
    Plaintext {
        /// Object-local body LBA of the generated manifest.
        manifest_first_chunk_lba: u64,
        /// Manifest byte length.
        manifest_size_bytes: u64,
        /// Number of Object-local chunks occupied by the manifest.
        manifest_chunk_count: u64,
        /// SHA-256 digest of the manifest CBOR bytes.
        manifest_sha256: [u8; 32],
    },
    /// Encrypted REM-OBJECT representation with envelope-visible anchors.
    Encrypted {
        /// Recipient epoch ids from the key-frame slots.
        recipient_epoch_ids: Vec<[u8; 16]>,
        /// REM-OBJECT encrypted metadata frame length.
        metadata_frame_len: u64,
        /// Serialized key-frame length.
        key_frame_len: u32,
    },
}

pub(crate) fn encode_object_recovery_row_cbor(
    row: &ObjectRecoveryRow,
) -> Result<CborValue, ParityError> {
    validate_object_recovery_row(row, None)?;
    let mut entries = vec![
        (
            CborValue::Integer(1.into()),
            CborValue::Integer(row.tape_file_number.into()),
        ),
        (
            CborValue::Integer(3.into()),
            CborValue::Integer(row.stored_block_count.into()),
        ),
    ];
    if let Some(object_id) = row.object_id.as_ref() {
        entries.push((
            CborValue::Integer(4.into()),
            CborValue::Bytes(object_id.clone()),
        ));
    }
    match &row.representation {
        ObjectRecoveryRepresentation::Plaintext {
            manifest_first_chunk_lba,
            manifest_size_bytes,
            manifest_chunk_count,
            manifest_sha256,
        } => {
            entries.insert(
                1,
                (
                    CborValue::Integer(2.into()),
                    CborValue::Text("plaintext".into()),
                ),
            );
            entries.extend([
                (
                    CborValue::Integer(10.into()),
                    CborValue::Integer((*manifest_first_chunk_lba).into()),
                ),
                (
                    CborValue::Integer(11.into()),
                    CborValue::Integer((*manifest_size_bytes).into()),
                ),
                (
                    CborValue::Integer(12.into()),
                    CborValue::Integer((*manifest_chunk_count).into()),
                ),
                (
                    CborValue::Integer(13.into()),
                    CborValue::Bytes(manifest_sha256.to_vec()),
                ),
            ]);
        }
        ObjectRecoveryRepresentation::Encrypted {
            recipient_epoch_ids,
            metadata_frame_len,
            key_frame_len,
        } => {
            entries.insert(
                1,
                (
                    CborValue::Integer(2.into()),
                    CborValue::Text("encrypted".into()),
                ),
            );
            entries.extend([
                (
                    CborValue::Integer(21.into()),
                    CborValue::Integer((*metadata_frame_len).into()),
                ),
                (
                    CborValue::Integer(22.into()),
                    CborValue::Array(
                        recipient_epoch_ids
                            .iter()
                            .map(|id| CborValue::Bytes(id.to_vec()))
                            .collect(),
                    ),
                ),
                (
                    CborValue::Integer(23.into()),
                    CborValue::Integer((*key_frame_len).into()),
                ),
            ]);
        }
    }
    Ok(CborValue::Map(entries))
}

pub(crate) fn decode_object_recovery_row_cbor(
    value: CborValue,
    block_size_bytes: Option<u32>,
) -> Result<ObjectRecoveryRow, ParityError> {
    let CborValue::Map(map) = value else {
        return Err(row_error("Object recovery row is not a map"));
    };
    let mut tape_file_number = None;
    let mut representation = None;
    let mut stored_block_count = None;
    let mut object_id = None;
    let mut manifest_first_chunk_lba = None;
    let mut manifest_size_bytes = None;
    let mut manifest_chunk_count = None;
    let mut manifest_sha256 = None;
    let mut recipient_epoch_ids = None;
    let mut metadata_frame_len = None;
    let mut key_frame_len = None;
    let mut key_order = IntegerMapKeyTracker::default();
    for (key, value) in map {
        let key = key_order
            .next(key, "Object recovery row")
            .map_err(row_error)?;
        match (key, value) {
            (1, CborValue::Integer(value)) => {
                tape_file_number = Some(int_to_u64(value, "tape_file_number")?)
            }
            (2, CborValue::Text(value)) => representation = Some(value),
            (3, CborValue::Integer(value)) => {
                stored_block_count = Some(int_to_u64(value, "stored_block_count")?)
            }
            (4, CborValue::Bytes(value)) => object_id = Some(value),
            (10, CborValue::Integer(value)) => {
                manifest_first_chunk_lba = Some(int_to_u64(value, "manifest_first_chunk_lba")?)
            }
            (11, CborValue::Integer(value)) => {
                manifest_size_bytes = Some(int_to_u64(value, "manifest_size_bytes")?)
            }
            (12, CborValue::Integer(value)) => {
                manifest_chunk_count = Some(int_to_u64(value, "manifest_chunk_count")?)
            }
            (13, CborValue::Bytes(value)) => {
                manifest_sha256 = Some(value.try_into().map_err(|value: Vec<u8>| {
                    row_error(format!(
                        "manifest_sha256 has length {}, expected 32",
                        value.len()
                    ))
                })?)
            }
            (21, CborValue::Integer(value)) => {
                metadata_frame_len = Some(int_to_u64(value, "metadata_frame_len")?)
            }
            (22, CborValue::Array(values)) => {
                let mut ids = Vec::with_capacity(values.len());
                for value in values {
                    let CborValue::Bytes(value) = value else {
                        return Err(row_error("recipient epoch id is not bytes"));
                    };
                    ids.push(value.try_into().map_err(|value: Vec<u8>| {
                        row_error(format!(
                            "recipient epoch id has length {}, expected 16",
                            value.len()
                        ))
                    })?);
                }
                recipient_epoch_ids = Some(ids);
            }
            (23, CborValue::Integer(value)) => {
                key_frame_len = Some(int_to_u32(value, "key_frame_len")?)
            }
            _ => {}
        }
    }
    let tape_file_number = tape_file_number
        .ok_or_else(|| row_error("Object recovery row missing tape_file_number"))?;
    let stored_block_count = stored_block_count
        .ok_or_else(|| row_error("Object recovery row missing stored_block_count"))?;
    let representation =
        representation.ok_or_else(|| row_error("Object recovery row missing representation"))?;
    let mut row = match representation.as_str() {
        "plaintext" => {
            if recipient_epoch_ids.is_some()
                || metadata_frame_len.is_some()
                || key_frame_len.is_some()
            {
                return Err(row_error(
                    "plaintext Object recovery row carries encrypted fields",
                ));
            }
            ObjectRecoveryRow::plaintext(
                tape_file_number,
                stored_block_count,
                manifest_first_chunk_lba
                    .ok_or_else(|| row_error("plaintext row missing manifest_first_chunk_lba"))?,
                manifest_size_bytes
                    .ok_or_else(|| row_error("plaintext row missing manifest_size_bytes"))?,
                manifest_chunk_count
                    .ok_or_else(|| row_error("plaintext row missing manifest_chunk_count"))?,
                manifest_sha256
                    .ok_or_else(|| row_error("plaintext row missing manifest_sha256"))?,
            )
        }
        "encrypted" => {
            if manifest_first_chunk_lba.is_some()
                || manifest_size_bytes.is_some()
                || manifest_chunk_count.is_some()
                || manifest_sha256.is_some()
            {
                return Err(row_error(
                    "encrypted Object recovery row carries plaintext fields",
                ));
            }
            ObjectRecoveryRow::encrypted(
                tape_file_number,
                stored_block_count,
                recipient_epoch_ids
                    .ok_or_else(|| row_error("encrypted row missing recipient_epoch_ids"))?,
                metadata_frame_len
                    .ok_or_else(|| row_error("encrypted row missing metadata_frame_len"))?,
                key_frame_len.ok_or_else(|| row_error("encrypted row missing key_frame_len"))?,
            )
        }
        other => {
            return Err(row_error(format!(
                "unsupported Object recovery representation {other}"
            )))
        }
    };
    row.object_id = object_id;
    validate_object_recovery_row(&row, block_size_bytes)?;
    Ok(row)
}

pub(crate) fn validate_object_recovery_row(
    row: &ObjectRecoveryRow,
    block_size_bytes: Option<u32>,
) -> Result<(), ParityError> {
    validate_object_recovery_row_fields(
        row.stored_block_count,
        row.object_id.as_deref(),
        &row.representation,
        block_size_bytes,
    )
}

pub(crate) fn validate_object_recovery_row_fields(
    stored_block_count: u64,
    object_id: Option<&[u8]>,
    representation: &ObjectRecoveryRepresentation,
    block_size_bytes: Option<u32>,
) -> Result<(), ParityError> {
    if stored_block_count == 0 {
        return Err(row_error(
            "Object recovery row stored_block_count must be positive",
        ));
    }
    if let Some(object_id) = object_id {
        if !(1..=64).contains(&object_id.len()) || object_id.contains(&0) {
            return Err(row_error(
                "Object recovery row object_id must contain 1..=64 non-NUL bytes",
            ));
        }
    }
    match representation {
        ObjectRecoveryRepresentation::Plaintext {
            manifest_first_chunk_lba,
            manifest_size_bytes,
            manifest_chunk_count,
            ..
        } => {
            if *manifest_size_bytes == 0 || *manifest_chunk_count == 0 {
                return Err(row_error("plaintext manifest size/count must be positive"));
            }
            let end = manifest_first_chunk_lba
                .checked_add(*manifest_chunk_count)
                .ok_or_else(|| row_error("plaintext manifest chunk range overflows"))?;
            if end > stored_block_count {
                return Err(row_error(
                    "plaintext manifest chunk range exceeds stored block count",
                ));
            }
            if let Some(block_size_bytes) = block_size_bytes {
                let capacity = manifest_chunk_count
                    .checked_mul(u64::from(block_size_bytes))
                    .ok_or_else(|| row_error("plaintext manifest byte capacity overflows"))?;
                if *manifest_size_bytes > capacity {
                    return Err(row_error(
                        "plaintext manifest size exceeds manifest chunk capacity",
                    ));
                }
            }
        }
        ObjectRecoveryRepresentation::Encrypted {
            recipient_epoch_ids,
            metadata_frame_len,
            key_frame_len,
        } => {
            if recipient_epoch_ids.is_empty()
                || recipient_epoch_ids.len() > MAX_RECIPIENTS
                || recipient_epoch_ids
                    .iter()
                    .any(|id| id.iter().all(|byte| *byte == 0))
                || recipient_epoch_ids
                    .iter()
                    .enumerate()
                    .any(|(index, id)| recipient_epoch_ids[..index].contains(id))
            {
                return Err(row_error(
                    "encrypted recipient_epoch_ids must contain 1..=8 distinct nonzero ids",
                ));
            }
            if !(METADATA_FRAME_MIN_LEN..=METADATA_FRAME_MAX_LEN).contains(metadata_frame_len) {
                return Err(row_error(
                    "encrypted metadata_frame_len is outside REM-OBJECT bounds",
                ));
            }
            if !(KEY_FRAME_MIN_LEN..=KEY_FRAME_MAX_LEN).contains(key_frame_len) {
                return Err(row_error(
                    "encrypted key_frame_len is outside REM-OBJECT bounds",
                ));
            }
        }
    }
    Ok(())
}

fn int_to_u64(value: Integer, field: &str) -> Result<u64, ParityError> {
    u64::try_from(value).map_err(|_| row_error(format!("{field} is not a nonnegative u64")))
}

fn int_to_u32(value: Integer, field: &str) -> Result<u32, ParityError> {
    u32::try_from(value).map_err(|_| row_error(format!("{field} is not a nonnegative u32")))
}

fn row_error(message: impl Into<String>) -> ParityError {
    ParityError::TapeIndexReplica(message.into())
}
