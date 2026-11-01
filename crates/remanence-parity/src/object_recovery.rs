//! Neutral Object recovery rows shared by the journal and terminal index.

use serde::{Deserialize, Serialize};

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

/// Validate Object recovery fields, including checked manifest range and capacity.
#[doc(hidden)]
pub fn validate_object_recovery_row_fields(
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
                // The capacity `count x B` is only compared, so it is compared
                // exactly and is never too large (REM-PARITY 2.4, 10.3).
                let capacity = u128::from(*manifest_chunk_count) * u128::from(block_size_bytes);
                if u128::from(*manifest_size_bytes) > capacity {
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

fn row_error(message: impl Into<String>) -> ParityError {
    ParityError::TapeIndexReplica(message.into())
}

#[cfg(test)]
mod exact_capacity_tests {
    use super::*;

    fn plaintext(count: u64, size: u64) -> ObjectRecoveryRepresentation {
        ObjectRecoveryRepresentation::Plaintext {
            manifest_first_chunk_lba: 0,
            manifest_size_bytes: size,
            manifest_chunk_count: count,
            manifest_sha256: [1; 32],
        }
    }

    /// REM-PARITY 2.4 and 10.3: the manifest capacity `count × B` is only
    /// compared, so it is compared exactly and never rejected for its size.
    #[test]
    fn manifest_capacity_is_compared_exactly() {
        let block = 256 * 1024;
        validate_object_recovery_row_fields(
            1 << 46,
            Some(b"unit"),
            &plaintext(1 << 46, 1),
            Some(block),
        )
        .expect("1 <= 2^46 x 2^18 = 2^64");
        validate_object_recovery_row_fields(
            1 << 46,
            Some(b"unit"),
            &plaintext(1 << 46, u64::MAX),
            Some(block),
        )
        .expect("2^64 − 1 <= 2^64");
        let error = validate_object_recovery_row_fields(
            1,
            Some(b"unit"),
            &plaintext(1, u64::from(block) + 1),
            Some(block),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("exceeds manifest chunk capacity"),
            "{error}"
        );
    }
}
