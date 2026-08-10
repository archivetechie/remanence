//! Canonical fixed-slot payload codec for terminal tape-index replicas.
//!
//! This module owns only the replayable structural/Object payload shared by
//! terminal replicas. Replica header/footer framing and tape-file geometry live
//! in `tape_index_replica`; there is no independent outer payload container.

use std::collections::BTreeMap;
use std::io::Cursor;

use ciborium::value::Value as CborValue;
use sha2::{Digest, Sha256};

use crate::cbor::IntegerMapKeyTracker;
use crate::error::ParityError;
use crate::object_recovery::{validate_object_recovery_row_fields, ObjectRecoveryRepresentation};

/// Bytes reserved for each canonical structural map entry.
pub const TAPE_INDEX_PAYLOAD_STRUCTURAL_SLOT_LEN: u64 = 64;

/// Bytes reserved for each canonical Object recovery row.
pub const TAPE_INDEX_PAYLOAD_OBJECT_ROW_SLOT_LEN: u64 = 256;

/// Per-slot little-endian encoded-length prefix.
pub const TAPE_INDEX_PAYLOAD_SLOT_PREFIX_LEN: usize = 2;

/// Largest canonical structural entry accepted inside one slot.
pub const TAPE_INDEX_PAYLOAD_STRUCTURAL_ENTRY_MAX_LEN: usize =
    TAPE_INDEX_PAYLOAD_STRUCTURAL_SLOT_LEN as usize - TAPE_INDEX_PAYLOAD_SLOT_PREFIX_LEN;

/// Largest canonical Object recovery row accepted inside one slot.
pub const TAPE_INDEX_PAYLOAD_OBJECT_ROW_MAX_LEN: usize =
    TAPE_INDEX_PAYLOAD_OBJECT_ROW_SLOT_LEN as usize - TAPE_INDEX_PAYLOAD_SLOT_PREFIX_LEN;

/// Counts that determine the exact fixed-slot payload size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TapeIndexPayloadCounts {
    /// Structural map entries in the covered prefix.
    pub structural_entry_count: u64,
    /// Object recovery rows, exactly one for each Object map entry.
    pub object_row_count: u64,
}

/// Digest-bound structural scope covered by a terminal replica payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TapeIndexPayloadScope {
    /// Number of leading tape files in the embedded structural map.
    pub covered_prefix_tape_file_count: u64,
    /// Total Object-data ordinals in the covered prefix.
    pub total_data_ordinals: u64,
    /// Highest protected Object-data ordinal in the covered prefix.
    pub highest_protected_ordinal: u64,
}

/// Facts needed to validate and stream the canonical payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TapeIndexPayloadDescriptor {
    pub(crate) block_size: u32,
    pub(crate) scope: TapeIndexPayloadScope,
    pub(crate) counts: TapeIndexPayloadCounts,
}

/// Structural kind stored in the embedded map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TapeIndexPayloadFileKind {
    /// REM-OBJECT body tape file.
    Object,
    /// Parity sidecar control tape file.
    ParitySidecar,
    /// One-block Bootstrap control tape file.
    Bootstrap,
    /// External ParityMap control tape file.
    ParityMap,
    /// Complete terminal tape-index replica control tape file.
    TapeIndexReplica,
    /// Typed terminal index separation extent.
    IndexSeparationExtent,
}

impl TapeIndexPayloadFileKind {
    fn code(self) -> u64 {
        match self {
            Self::Object => 0,
            Self::ParitySidecar => 1,
            Self::Bootstrap => 2,
            Self::ParityMap => 3,
            Self::TapeIndexReplica => 4,
            Self::IndexSeparationExtent => 5,
        }
    }

    fn from_code(value: u64) -> Result<Self, ParityError> {
        match value {
            0 => Ok(Self::Object),
            1 => Ok(Self::ParitySidecar),
            2 => Ok(Self::Bootstrap),
            3 => Ok(Self::ParityMap),
            4 => Ok(Self::TapeIndexReplica),
            5 => Ok(Self::IndexSeparationExtent),
            _ => Err(payload_error(format!(
                "unsupported tape-index structural kind {value}"
            ))),
        }
    }
}

/// One structural map row streamed into the terminal replica payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapeIndexPayloadMapEntry {
    /// Dense filemark-delimited tape-file number from BOT.
    pub tape_file_number: u64,
    /// Structural kind.
    pub kind: TapeIndexPayloadFileKind,
    /// Fixed records before the trailing filemark.
    pub block_count: u64,
    /// First Object-data ordinal for Object entries.
    pub first_parity_data_ordinal: Option<u64>,
    /// First protected ordinal for sidecar entries.
    pub protected_ordinal_start: Option<u64>,
    /// End-exclusive protected ordinal for sidecar entries.
    pub protected_ordinal_end_exclusive: Option<u64>,
    /// Epoch identity for sidecar entries.
    pub epoch_id: Option<u64>,
}

/// One Object recovery row streamed into the terminal replica payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapeIndexPayloadObjectRow {
    /// Tape-file number of the matching Object map entry.
    pub tape_file_number: u64,
    /// Stored fixed-block count, which must match the map entry.
    pub stored_block_count: u64,
    /// Required 1–64-byte verbatim REM-OBJECT identifier.
    pub object_id: Vec<u8>,
    /// Existing representation-specific recovery anchors.
    pub representation: ObjectRecoveryRepresentation,
}

/// Replayable authority used to hash and emit each terminal replica payload.
pub(crate) trait TapeIndexPayloadRecordSource {
    fn visit_structural_entries(
        &mut self,
        visitor: &mut dyn FnMut(&TapeIndexPayloadMapEntry) -> Result<(), ParityError>,
    ) -> Result<(), ParityError>;

    fn visit_object_rows(
        &mut self,
        visitor: &mut dyn FnMut(&TapeIndexPayloadObjectRow) -> Result<(), ParityError>,
    ) -> Result<(), ParityError>;
}

/// Checked facts produced while streaming a complete canonical payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TapeIndexPayloadSummary {
    pub(crate) payload_len: u64,
    pub(crate) canonical_map_sha256: [u8; 32],
    pub(crate) covered_prefix_end_lba: u64,
}

/// Stream and validate a canonical fixed-slot payload without collecting rows.
pub(crate) fn stream_tape_index_payload<S, F>(
    source: &mut S,
    descriptor: &TapeIndexPayloadDescriptor,
    mut emit_slot: F,
) -> Result<TapeIndexPayloadSummary, ParityError>
where
    S: TapeIndexPayloadRecordSource + ?Sized,
    F: FnMut(&[u8]) -> Result<(), ParityError>,
{
    validate_tape_index_payload_descriptor(descriptor)?;
    let mut map_hasher = Sha256::new();
    map_hasher.update(encode_cbor_array_header(
        descriptor.counts.structural_entry_count,
    ));
    let mut map_locator_hasher = Sha256::new();
    let mut row_locator_hasher = Sha256::new();
    let mut structural_count = 0u64;
    let mut object_map_count = 0u64;
    let mut row_count = 0u64;
    let mut expected_tape_file_number = 0u64;
    let mut expected_data_ordinal = 0u64;
    let mut expected_protected_ordinal = 0u64;
    let mut expected_epoch_id = 0u64;
    let mut covered_prefix_end_lba = 0u64;
    let mut final_parity_map_seen = false;
    let mut sidecar_seen = false;

    source.visit_structural_entries(&mut |entry| {
        validate_payload_map_entry(entry)?;
        if entry.tape_file_number != expected_tape_file_number {
            return Err(payload_error(format!(
                "structural entry {} is not dense expected tape file {expected_tape_file_number}",
                entry.tape_file_number
            )));
        }
        if structural_count == 0 {
            if entry.kind != TapeIndexPayloadFileKind::Bootstrap || entry.block_count != 1 {
                return Err(payload_error(
                    "terminal pre-A payload must begin with the one-block tape-file-0 BOT Bootstrap",
                ));
            }
        } else if entry.kind == TapeIndexPayloadFileKind::Bootstrap {
            return Err(payload_error(
                "terminal pre-A payload contains a Bootstrap outside tape file 0",
            ));
        }
        if final_parity_map_seen {
            return Err(payload_error(
                "terminal pre-A ParityMap must be the final structural entry",
            ));
        }
        if matches!(
            entry.kind,
            TapeIndexPayloadFileKind::TapeIndexReplica
                | TapeIndexPayloadFileKind::IndexSeparationExtent
        ) {
            return Err(payload_error(format!(
                "terminal structural kind {:?} is forbidden in the pre-A payload",
                entry.kind
            )));
        }
        if entry.kind == TapeIndexPayloadFileKind::ParityMap {
            final_parity_map_seen = true;
        }
        expected_tape_file_number = expected_tape_file_number
            .checked_add(1)
            .ok_or_else(|| payload_error("structural tape-file sequence overflows u64"))?;
        structural_count = structural_count
            .checked_add(1)
            .ok_or_else(|| payload_error("structural entry count overflows u64"))?;
        covered_prefix_end_lba = covered_prefix_end_lba
            .checked_add(entry.block_count)
            .and_then(|lba| lba.checked_add(1))
            .ok_or_else(|| {
                payload_error("covered-prefix physical extent plus filemark overflows u64")
            })?;
        match entry.kind {
            TapeIndexPayloadFileKind::Object => {
                let first = entry.first_parity_data_ordinal.ok_or_else(|| {
                    payload_error("Object structural entry is missing first data ordinal")
                })?;
                if first != expected_data_ordinal {
                    return Err(payload_error(format!(
                        "Object tape file {} begins at ordinal {first}, expected {expected_data_ordinal}",
                        entry.tape_file_number
                    )));
                }
                expected_data_ordinal = expected_data_ordinal
                    .checked_add(entry.block_count)
                    .ok_or_else(|| payload_error("Object data ordinal range overflows u64"))?;
                object_map_count = object_map_count
                    .checked_add(1)
                    .ok_or_else(|| payload_error("Object map count overflows u64"))?;
                update_locator_digest(
                    &mut map_locator_hasher,
                    entry.tape_file_number,
                    entry.block_count,
                );
            }
            TapeIndexPayloadFileKind::ParitySidecar => {
                sidecar_seen = true;
                let start = entry.protected_ordinal_start.ok_or_else(|| {
                    payload_error("sidecar structural entry is missing protected start")
                })?;
                let end = entry.protected_ordinal_end_exclusive.ok_or_else(|| {
                    payload_error("sidecar structural entry is missing protected end")
                })?;
                if start != expected_protected_ordinal {
                    return Err(payload_error(format!(
                        "sidecar tape file {} begins protection at {start}, expected {expected_protected_ordinal}",
                        entry.tape_file_number
                    )));
                }
                if entry.epoch_id != Some(expected_epoch_id) {
                    return Err(payload_error(format!(
                        "sidecar tape file {} has epoch {:?}, expected {expected_epoch_id}",
                        entry.tape_file_number, entry.epoch_id
                    )));
                }
                expected_protected_ordinal = end;
                expected_epoch_id = expected_epoch_id
                    .checked_add(1)
                    .ok_or_else(|| payload_error("sidecar epoch sequence overflows u64"))?;
            }
            TapeIndexPayloadFileKind::Bootstrap | TapeIndexPayloadFileKind::ParityMap => {}
            TapeIndexPayloadFileKind::TapeIndexReplica
            | TapeIndexPayloadFileKind::IndexSeparationExtent => unreachable!(
                "terminal structural kinds are rejected before payload emission"
            ),
        }
        let encoded = encode_payload_map_entry(entry)?;
        map_hasher.update(&encoded);
        let slot = encode_slot(
            &encoded,
            TAPE_INDEX_PAYLOAD_STRUCTURAL_SLOT_LEN,
            "structural entry",
        )?;
        emit_slot(&slot)
    })?;

    if structural_count == 0 {
        return Err(payload_error(
            "terminal pre-A payload is empty; BOT Bootstrap is required",
        ));
    }

    if structural_count != descriptor.counts.structural_entry_count {
        return Err(payload_error(format!(
            "source yielded {structural_count} structural entries, expected {}",
            descriptor.counts.structural_entry_count
        )));
    }
    if expected_data_ordinal != descriptor.scope.total_data_ordinals {
        return Err(payload_error(format!(
            "structural map has {expected_data_ordinal} data ordinals, scope declares {}",
            descriptor.scope.total_data_ordinals
        )));
    }
    if expected_protected_ordinal != descriptor.scope.highest_protected_ordinal {
        return Err(payload_error(format!(
            "structural map protects through {expected_protected_ordinal}, scope declares {}",
            descriptor.scope.highest_protected_ordinal
        )));
    }
    if sidecar_seen != final_parity_map_seen {
        return Err(payload_error(
            "terminal pre-A payload requires exactly one final ParityMap iff a ParitySidecar is present",
        ));
    }
    if sidecar_seen && expected_protected_ordinal != expected_data_ordinal {
        return Err(payload_error(
            "terminal parity closeout must protect every Object ordinal before replica A",
        ));
    }

    let mut previous_row_file_number = None;
    source.visit_object_rows(&mut |row| {
        validate_object_recovery_row_fields(
            row.stored_block_count,
            Some(&row.object_id),
            &row.representation,
            Some(descriptor.block_size),
        )
        .map_err(|error| payload_error(error.to_string()))?;
        if previous_row_file_number.is_some_and(|previous| row.tape_file_number <= previous) {
            return Err(payload_error(
                "Object recovery rows are not in strictly increasing tape-file order",
            ));
        }
        previous_row_file_number = Some(row.tape_file_number);
        row_count = row_count
            .checked_add(1)
            .ok_or_else(|| payload_error("Object row count overflows u64"))?;
        update_locator_digest(
            &mut row_locator_hasher,
            row.tape_file_number,
            row.stored_block_count,
        );
        let encoded = encode_payload_object_row(row)?;
        let slot = encode_slot(
            &encoded,
            TAPE_INDEX_PAYLOAD_OBJECT_ROW_SLOT_LEN,
            "Object recovery row",
        )?;
        emit_slot(&slot)
    })?;

    if row_count != descriptor.counts.object_row_count {
        return Err(payload_error(format!(
            "source yielded {row_count} Object rows, expected {}",
            descriptor.counts.object_row_count
        )));
    }
    // This bounded-memory exact-order comparison relies on SHA-256 collision
    // resistance. Counts are compared separately so no empty suffix is hidden.
    if object_map_count != row_count
        || map_locator_hasher.finalize() != row_locator_hasher.finalize()
    {
        return Err(payload_error(
            "embedded structural map and Object rows are not a bijection",
        ));
    }

    let payload_len = checked_tape_index_payload_byte_len(TapeIndexPayloadCounts {
        structural_entry_count: structural_count,
        object_row_count: row_count,
    })?;
    Ok(TapeIndexPayloadSummary {
        payload_len,
        canonical_map_sha256: map_hasher.finalize().into(),
        covered_prefix_end_lba,
    })
}

fn encode_payload_map_entry(entry: &TapeIndexPayloadMapEntry) -> Result<Vec<u8>, ParityError> {
    encode_cbor_value(
        &CborValue::Array(vec![
            CborValue::Integer(entry.tape_file_number.into()),
            CborValue::Integer(entry.kind.code().into()),
            CborValue::Integer(entry.block_count.into()),
            optional_u64(entry.first_parity_data_ordinal),
            optional_u64(entry.protected_ordinal_start),
            optional_u64(entry.protected_ordinal_end_exclusive),
            optional_u64(entry.epoch_id),
        ]),
        "structural entry",
    )
}

fn encode_payload_object_row(row: &TapeIndexPayloadObjectRow) -> Result<Vec<u8>, ParityError> {
    let mut entries = vec![
        (
            CborValue::Integer(1.into()),
            CborValue::Integer(row.tape_file_number.into()),
        ),
        (
            CborValue::Integer(2.into()),
            CborValue::Text(match row.representation {
                ObjectRecoveryRepresentation::Plaintext { .. } => "plaintext".to_string(),
                ObjectRecoveryRepresentation::Encrypted { .. } => "encrypted".to_string(),
            }),
        ),
        (
            CborValue::Integer(3.into()),
            CborValue::Integer(row.stored_block_count.into()),
        ),
        (
            CborValue::Integer(4.into()),
            CborValue::Bytes(row.object_id.clone()),
        ),
    ];
    match &row.representation {
        ObjectRecoveryRepresentation::Plaintext {
            manifest_first_chunk_lba,
            manifest_size_bytes,
            manifest_chunk_count,
            manifest_sha256,
        } => entries.extend([
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
        ]),
        ObjectRecoveryRepresentation::Encrypted {
            recipient_epoch_ids,
            metadata_frame_len,
            key_frame_len,
        } => entries.extend([
            (
                CborValue::Integer(21.into()),
                CborValue::Integer((*metadata_frame_len).into()),
            ),
            (
                CborValue::Integer(22.into()),
                CborValue::Array(
                    recipient_epoch_ids
                        .iter()
                        .map(|epoch_id| CborValue::Bytes(epoch_id.to_vec()))
                        .collect(),
                ),
            ),
            (
                CborValue::Integer(23.into()),
                CborValue::Integer((*key_frame_len).into()),
            ),
        ]),
    }
    encode_cbor_value(&CborValue::Map(entries), "Object recovery row")
}

pub(crate) fn decode_tape_index_payload_map_entry_slot(
    slot: &[u8],
) -> Result<TapeIndexPayloadMapEntry, ParityError> {
    let value = decode_canonical_slot(
        slot,
        TAPE_INDEX_PAYLOAD_STRUCTURAL_SLOT_LEN,
        "structural entry",
    )?;
    let CborValue::Array(values) = value else {
        return Err(payload_error("structural entry CBOR is not an array"));
    };
    let values: [CborValue; 7] = values.try_into().map_err(|values: Vec<CborValue>| {
        payload_error(format!(
            "structural entry has {} fields, expected 7",
            values.len()
        ))
    })?;
    let [tape_file_number, kind, block_count, first_data, protected_start, protected_end, epoch_id] =
        values;
    let entry = TapeIndexPayloadMapEntry {
        tape_file_number: cbor_u64(tape_file_number, "tape_file_number")?,
        kind: TapeIndexPayloadFileKind::from_code(cbor_u64(kind, "kind")?)?,
        block_count: cbor_u64(block_count, "block_count")?,
        first_parity_data_ordinal: cbor_optional_u64(first_data, "first_parity_data_ordinal")?,
        protected_ordinal_start: cbor_optional_u64(protected_start, "protected_ordinal_start")?,
        protected_ordinal_end_exclusive: cbor_optional_u64(
            protected_end,
            "protected_ordinal_end_exclusive",
        )?,
        epoch_id: cbor_optional_u64(epoch_id, "epoch_id")?,
    };
    validate_payload_map_entry(&entry)?;
    let encoded = encode_payload_map_entry(&entry)?;
    validate_slot_reencoding(slot, &encoded, "structural entry")?;
    Ok(entry)
}

pub(crate) fn decode_tape_index_payload_object_row_slot(
    slot: &[u8],
    block_size: u32,
) -> Result<TapeIndexPayloadObjectRow, ParityError> {
    let value = decode_canonical_slot(
        slot,
        TAPE_INDEX_PAYLOAD_OBJECT_ROW_SLOT_LEN,
        "Object recovery row",
    )?;
    validate_canonical_cbor_shape(&value, "Object recovery row")?;
    let canonical = encode_cbor_value(&value, "Object recovery row")?;
    validate_slot_reencoding(slot, &canonical, "Object recovery row")?;
    let CborValue::Map(entries) = value else {
        return Err(payload_error("Object recovery row CBOR is not a map"));
    };
    let mut fields = BTreeMap::new();
    let mut key_order = IntegerMapKeyTracker::default();
    for (key, value) in entries {
        let key = key_order
            .next(key, "tape-index Object recovery row")
            .map_err(payload_error)?;
        if fields.insert(key, value).is_some() {
            return Err(payload_error(format!(
                "Object recovery row contains duplicate key {key}"
            )));
        }
    }

    let representation = match required_field(&mut fields, 2, "representation")? {
        CborValue::Text(value) => value,
        _ => {
            return Err(payload_error(
                "Object recovery row representation is not text",
            ))
        }
    };
    let (forbidden_keys, forbidden_representation): (&[i128], &str) = match representation.as_str()
    {
        "plaintext" => (&[21, 22, 23], "encrypted"),
        "encrypted" => (&[10, 11, 12, 13], "plaintext"),
        other => {
            return Err(payload_error(format!(
                "unsupported Object recovery row representation {other:?}"
            )))
        }
    };
    if let Some(key) = forbidden_keys.iter().find(|key| fields.contains_key(key)) {
        return Err(payload_error(format!(
            "{representation} Object recovery row carries {forbidden_representation} field key {key}"
        )));
    }

    let tape_file_number = cbor_u64(
        required_field(&mut fields, 1, "tape_file_number")?,
        "tape_file_number",
    )?;
    let stored_block_count = cbor_u64(
        required_field(&mut fields, 3, "stored_block_count")?,
        "stored_block_count",
    )?;
    let object_id = match required_field(&mut fields, 4, "object_id")? {
        CborValue::Bytes(bytes) => bytes,
        _ => return Err(payload_error("Object recovery row object_id is not bytes")),
    };
    let representation = if representation == "plaintext" {
        let manifest_sha256 = match required_field(&mut fields, 13, "manifest_sha256")? {
            CborValue::Bytes(bytes) => bytes.try_into().map_err(|bytes: Vec<u8>| {
                payload_error(format!(
                    "Object recovery row manifest_sha256 has length {}, expected 32",
                    bytes.len()
                ))
            })?,
            _ => {
                return Err(payload_error(
                    "Object recovery row manifest_sha256 is not bytes",
                ))
            }
        };
        ObjectRecoveryRepresentation::Plaintext {
            manifest_first_chunk_lba: cbor_u64(
                required_field(&mut fields, 10, "manifest_first_chunk_lba")?,
                "manifest_first_chunk_lba",
            )?,
            manifest_size_bytes: cbor_u64(
                required_field(&mut fields, 11, "manifest_size_bytes")?,
                "manifest_size_bytes",
            )?,
            manifest_chunk_count: cbor_u64(
                required_field(&mut fields, 12, "manifest_chunk_count")?,
                "manifest_chunk_count",
            )?,
            manifest_sha256,
        }
    } else {
        let recipient_epoch_ids = match required_field(&mut fields, 22, "recipient_epoch_ids")? {
            CborValue::Array(values) => values
                .into_iter()
                .map(|value| match value {
                    CborValue::Bytes(bytes) => bytes.try_into().map_err(|bytes: Vec<u8>| {
                        payload_error(format!(
                            "recipient epoch ID has length {}, expected 16",
                            bytes.len()
                        ))
                    }),
                    _ => Err(payload_error("recipient epoch ID is not bytes")),
                })
                .collect::<Result<Vec<[u8; 16]>, _>>()?,
            _ => return Err(payload_error("recipient_epoch_ids is not an array")),
        };
        let key_frame_len = cbor_u64(
            required_field(&mut fields, 23, "key_frame_len")?,
            "key_frame_len",
        )?;
        ObjectRecoveryRepresentation::Encrypted {
            recipient_epoch_ids,
            metadata_frame_len: cbor_u64(
                required_field(&mut fields, 21, "metadata_frame_len")?,
                "metadata_frame_len",
            )?,
            key_frame_len: u32::try_from(key_frame_len)
                .map_err(|_| payload_error("key_frame_len exceeds u32"))?,
        }
    };
    let row = TapeIndexPayloadObjectRow {
        tape_file_number,
        stored_block_count,
        object_id,
        representation,
    };
    validate_object_recovery_row_fields(
        row.stored_block_count,
        Some(&row.object_id),
        &row.representation,
        Some(block_size),
    )
    .map_err(|error| payload_error(error.to_string()))?;
    Ok(row)
}

fn decode_canonical_slot(
    slot: &[u8],
    expected_slot_len: u64,
    label: &str,
) -> Result<CborValue, ParityError> {
    let expected_slot_len = usize::try_from(expected_slot_len)
        .map_err(|_| payload_error(format!("{label} slot length overflows usize")))?;
    if slot.len() != expected_slot_len {
        return Err(payload_error(format!(
            "{label} slot length {}, expected {expected_slot_len}",
            slot.len()
        )));
    }
    let encoded_len = usize::from(u16::from_le_bytes(
        slot[..TAPE_INDEX_PAYLOAD_SLOT_PREFIX_LEN]
            .try_into()
            .expect("slot prefix is bounded"),
    ));
    if encoded_len == 0 || encoded_len > slot.len() - TAPE_INDEX_PAYLOAD_SLOT_PREFIX_LEN {
        return Err(payload_error(format!(
            "{label} encoded length {encoded_len} is outside slot capacity {}",
            slot.len() - TAPE_INDEX_PAYLOAD_SLOT_PREFIX_LEN
        )));
    }
    let encoded_end = TAPE_INDEX_PAYLOAD_SLOT_PREFIX_LEN + encoded_len;
    if let Some((offset, byte)) = slot[encoded_end..]
        .iter()
        .copied()
        .enumerate()
        .find(|(_, byte)| *byte != 0)
    {
        return Err(payload_error(format!(
            "{label} slot padding has nonzero byte 0x{byte:02x} at offset {}",
            encoded_end + offset
        )));
    }
    let encoded = &slot[TAPE_INDEX_PAYLOAD_SLOT_PREFIX_LEN..encoded_end];
    let mut reader = Cursor::new(encoded);
    let value: CborValue = ciborium::from_reader(&mut reader)
        .map_err(|error| payload_error(format!("{label} CBOR decode failed: {error}")))?;
    if reader.position() != encoded_len as u64 {
        return Err(payload_error(format!(
            "{label} CBOR consumed {} of {encoded_len} encoded bytes",
            reader.position()
        )));
    }
    Ok(value)
}

fn validate_slot_reencoding(slot: &[u8], canonical: &[u8], label: &str) -> Result<(), ParityError> {
    let encoded_len = usize::from(u16::from_le_bytes(
        slot[..TAPE_INDEX_PAYLOAD_SLOT_PREFIX_LEN]
            .try_into()
            .expect("slot prefix is bounded"),
    ));
    let encoded_end = TAPE_INDEX_PAYLOAD_SLOT_PREFIX_LEN + encoded_len;
    if canonical != &slot[TAPE_INDEX_PAYLOAD_SLOT_PREFIX_LEN..encoded_end] {
        return Err(payload_error(format!(
            "{label} CBOR is not the deterministic canonical encoding"
        )));
    }
    Ok(())
}

fn validate_canonical_cbor_shape(value: &CborValue, label: &str) -> Result<(), ParityError> {
    match value {
        CborValue::Integer(_)
        | CborValue::Bytes(_)
        | CborValue::Text(_)
        | CborValue::Bool(_)
        | CborValue::Null => Ok(()),
        CborValue::Array(values) => {
            for value in values {
                validate_canonical_cbor_shape(value, label)?;
            }
            Ok(())
        }
        CborValue::Map(entries) => {
            let mut key_order = IntegerMapKeyTracker::default();
            for (key, value) in entries {
                key_order.next(key.clone(), label).map_err(payload_error)?;
                validate_canonical_cbor_shape(value, label)?;
            }
            Ok(())
        }
        CborValue::Float(_) => Err(payload_error(format!(
            "{label} contains a forbidden CBOR float"
        ))),
        CborValue::Tag(_, _) => Err(payload_error(format!(
            "{label} contains a forbidden CBOR tag"
        ))),
        _ => Err(payload_error(format!(
            "{label} contains an unsupported CBOR value"
        ))),
    }
}

fn cbor_u64(value: CborValue, field: &str) -> Result<u64, ParityError> {
    let CborValue::Integer(value) = value else {
        return Err(payload_error(format!("{field} is not a CBOR integer")));
    };
    u64::try_from(i128::from(value)).map_err(|_| payload_error(format!("{field} is outside u64")))
}

fn cbor_optional_u64(value: CborValue, field: &str) -> Result<Option<u64>, ParityError> {
    match value {
        CborValue::Null => Ok(None),
        value => cbor_u64(value, field).map(Some),
    }
}

fn required_field(
    fields: &mut BTreeMap<i128, CborValue>,
    key: i128,
    field: &str,
) -> Result<CborValue, ParityError> {
    fields
        .remove(&key)
        .ok_or_else(|| payload_error(format!("Object recovery row is missing {field}")))
}

fn encode_cbor_value(value: &CborValue, label: &str) -> Result<Vec<u8>, ParityError> {
    let mut bytes = Vec::new();
    ciborium::into_writer(value, &mut bytes)
        .map_err(|error| payload_error(format!("{label} CBOR encode failed: {error}")))?;
    Ok(bytes)
}

fn encode_slot(bytes: &[u8], slot_len: u64, label: &str) -> Result<Vec<u8>, ParityError> {
    let slot_len = usize::try_from(slot_len)
        .map_err(|_| payload_error(format!("{label} slot length overflows usize")))?;
    let capacity = slot_len
        .checked_sub(TAPE_INDEX_PAYLOAD_SLOT_PREFIX_LEN)
        .ok_or_else(|| payload_error(format!("{label} slot is shorter than its prefix")))?;
    if bytes.len() > capacity {
        return Err(payload_error(format!(
            "{label} encoded length {} exceeds slot capacity {capacity}",
            bytes.len()
        )));
    }
    let encoded_len = u16::try_from(bytes.len())
        .map_err(|_| payload_error(format!("{label} encoded length exceeds u16")))?;
    let mut slot = vec![0u8; slot_len];
    slot[..TAPE_INDEX_PAYLOAD_SLOT_PREFIX_LEN].copy_from_slice(&encoded_len.to_le_bytes());
    slot[TAPE_INDEX_PAYLOAD_SLOT_PREFIX_LEN..TAPE_INDEX_PAYLOAD_SLOT_PREFIX_LEN + bytes.len()]
        .copy_from_slice(bytes);
    Ok(slot)
}

fn optional_u64(value: Option<u64>) -> CborValue {
    value.map_or(CborValue::Null, |value| CborValue::Integer(value.into()))
}

pub(crate) fn encode_cbor_array_header(len: u64) -> Vec<u8> {
    encode_cbor_major_argument(4, len)
}

fn encode_cbor_major_argument(major: u8, argument: u64) -> Vec<u8> {
    let initial = major << 5;
    match argument {
        0..=23 => vec![initial | argument as u8],
        24..=0xff => vec![initial | 24, argument as u8],
        0x100..=0xffff => {
            let mut bytes = vec![initial | 25];
            bytes.extend_from_slice(&(argument as u16).to_be_bytes());
            bytes
        }
        0x1_0000..=0xffff_ffff => {
            let mut bytes = vec![initial | 26];
            bytes.extend_from_slice(&(argument as u32).to_be_bytes());
            bytes
        }
        _ => {
            let mut bytes = vec![initial | 27];
            bytes.extend_from_slice(&argument.to_be_bytes());
            bytes
        }
    }
}

fn validate_payload_map_entry(entry: &TapeIndexPayloadMapEntry) -> Result<(), ParityError> {
    if entry.block_count == 0 {
        return Err(payload_error(format!(
            "tape file {} has zero block count",
            entry.tape_file_number
        )));
    }
    let no_object = entry.first_parity_data_ordinal.is_none();
    let no_sidecar = entry.protected_ordinal_start.is_none()
        && entry.protected_ordinal_end_exclusive.is_none()
        && entry.epoch_id.is_none();
    match entry.kind {
        TapeIndexPayloadFileKind::Object if !no_object && no_sidecar => Ok(()),
        TapeIndexPayloadFileKind::ParitySidecar => {
            let (Some(start), Some(end), Some(_)) = (
                entry.protected_ordinal_start,
                entry.protected_ordinal_end_exclusive,
                entry.epoch_id,
            ) else {
                return Err(payload_error(format!(
                    "sidecar tape file {} is missing range or epoch",
                    entry.tape_file_number
                )));
            };
            if no_object && end > start {
                Ok(())
            } else {
                Err(payload_error(format!(
                    "sidecar tape file {} has invalid kind fields",
                    entry.tape_file_number
                )))
            }
        }
        TapeIndexPayloadFileKind::Bootstrap
            if entry.block_count == 1 && no_object && no_sidecar =>
        {
            Ok(())
        }
        TapeIndexPayloadFileKind::ParityMap
        | TapeIndexPayloadFileKind::TapeIndexReplica
        | TapeIndexPayloadFileKind::IndexSeparationExtent
            if no_object && no_sidecar =>
        {
            Ok(())
        }
        _ => Err(payload_error(format!(
            "tape file {} has fields inconsistent with {:?}",
            entry.tape_file_number, entry.kind
        ))),
    }
}

fn validate_payload_counts(counts: TapeIndexPayloadCounts) -> Result<(), ParityError> {
    if counts.object_row_count > counts.structural_entry_count {
        return Err(payload_error(format!(
            "Object row count {} exceeds structural entry count {}",
            counts.object_row_count, counts.structural_entry_count
        )));
    }
    Ok(())
}

pub(crate) fn checked_tape_index_payload_byte_len(
    counts: TapeIndexPayloadCounts,
) -> Result<u64, ParityError> {
    validate_payload_counts(counts)?;
    let structural_bytes = counts
        .structural_entry_count
        .checked_mul(TAPE_INDEX_PAYLOAD_STRUCTURAL_SLOT_LEN)
        .ok_or_else(|| payload_error("structural slot byte count overflows u64"))?;
    let row_bytes = counts
        .object_row_count
        .checked_mul(TAPE_INDEX_PAYLOAD_OBJECT_ROW_SLOT_LEN)
        .ok_or_else(|| payload_error("Object-row slot byte count overflows u64"))?;
    structural_bytes
        .checked_add(row_bytes)
        .ok_or_else(|| payload_error("tape-index payload length overflows u64"))
}

pub(crate) fn validate_tape_index_payload_descriptor(
    descriptor: &TapeIndexPayloadDescriptor,
) -> Result<(), ParityError> {
    validate_payload_counts(descriptor.counts)?;
    if descriptor.scope.covered_prefix_tape_file_count != descriptor.counts.structural_entry_count {
        return Err(payload_error(format!(
            "covered prefix count {} differs from structural entry count {}",
            descriptor.scope.covered_prefix_tape_file_count,
            descriptor.counts.structural_entry_count
        )));
    }
    if descriptor.scope.highest_protected_ordinal > descriptor.scope.total_data_ordinals {
        return Err(payload_error(
            "highest protected ordinal exceeds total data ordinals",
        ));
    }
    Ok(())
}

fn update_locator_digest(hasher: &mut Sha256, tape_file_number: u64, block_count: u64) {
    hasher.update(tape_file_number.to_le_bytes());
    hasher.update(block_count.to_le_bytes());
}

fn payload_error(message: impl Into<String>) -> ParityError {
    ParityError::TapeIndexReplica(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Entries(Vec<TapeIndexPayloadMapEntry>);

    impl TapeIndexPayloadRecordSource for Entries {
        fn visit_structural_entries(
            &mut self,
            visitor: &mut dyn FnMut(&TapeIndexPayloadMapEntry) -> Result<(), ParityError>,
        ) -> Result<(), ParityError> {
            for entry in &self.0 {
                visitor(entry)?;
            }
            Ok(())
        }

        fn visit_object_rows(
            &mut self,
            _visitor: &mut dyn FnMut(&TapeIndexPayloadObjectRow) -> Result<(), ParityError>,
        ) -> Result<(), ParityError> {
            Ok(())
        }
    }

    fn structural_entry(
        tape_file_number: u64,
        kind: TapeIndexPayloadFileKind,
    ) -> TapeIndexPayloadMapEntry {
        TapeIndexPayloadMapEntry {
            tape_file_number,
            kind,
            block_count: 1,
            first_parity_data_ordinal: (kind == TapeIndexPayloadFileKind::Object).then_some(0),
            protected_ordinal_start: None,
            protected_ordinal_end_exclusive: None,
            epoch_id: None,
        }
    }

    fn stream_entries(entries: Vec<TapeIndexPayloadMapEntry>) -> Result<(), ParityError> {
        let count = entries.len() as u64;
        let total_data_ordinals = entries
            .iter()
            .filter(|entry| entry.kind == TapeIndexPayloadFileKind::Object)
            .map(|entry| entry.block_count)
            .sum();
        let descriptor = TapeIndexPayloadDescriptor {
            block_size: 4096,
            scope: TapeIndexPayloadScope {
                covered_prefix_tape_file_count: count,
                total_data_ordinals,
                highest_protected_ordinal: entries
                    .iter()
                    .filter_map(|entry| entry.protected_ordinal_end_exclusive)
                    .max()
                    .unwrap_or(0),
            },
            counts: TapeIndexPayloadCounts {
                structural_entry_count: count,
                object_row_count: 0,
            },
        };
        stream_tape_index_payload(&mut Entries(entries), &descriptor, |_| Ok(())).map(|_| ())
    }

    fn sidecar_entry(tape_file_number: u64, start: u64, end: u64) -> TapeIndexPayloadMapEntry {
        TapeIndexPayloadMapEntry {
            tape_file_number,
            kind: TapeIndexPayloadFileKind::ParitySidecar,
            block_count: 1,
            first_parity_data_ordinal: None,
            protected_ordinal_start: Some(start),
            protected_ordinal_end_exclusive: Some(end),
            epoch_id: Some(0),
        }
    }

    fn plaintext_object_row() -> TapeIndexPayloadObjectRow {
        TapeIndexPayloadObjectRow {
            tape_file_number: 1,
            stored_block_count: 8,
            object_id: b"object-1".to_vec(),
            representation: ObjectRecoveryRepresentation::Plaintext {
                manifest_first_chunk_lba: 1,
                manifest_size_bytes: 100,
                manifest_chunk_count: 1,
                manifest_sha256: [0x5a; 32],
            },
        }
    }

    fn encrypted_object_row() -> TapeIndexPayloadObjectRow {
        TapeIndexPayloadObjectRow {
            tape_file_number: 1,
            stored_block_count: 8,
            object_id: b"object-1".to_vec(),
            representation: ObjectRecoveryRepresentation::Encrypted {
                recipient_epoch_ids: vec![[0x6b; 16]],
                metadata_frame_len: 4_096,
                key_frame_len: 1_191,
            },
        }
    }

    fn with_object_row_field(value: CborValue, key: i128, field: CborValue) -> CborValue {
        let CborValue::Map(mut entries) = value else {
            panic!("encoded Object row must be a map");
        };
        entries.push((
            CborValue::Integer(key.try_into().expect("test key fits CBOR integer")),
            field,
        ));
        entries.sort_by(|(left, _), (right, _)| {
            let left = encode_cbor_value(left, "test map key").expect("left key encodes");
            let right = encode_cbor_value(right, "test map key").expect("right key encodes");
            left.len().cmp(&right.len()).then_with(|| left.cmp(&right))
        });
        CborValue::Map(entries)
    }

    fn encoded_object_row_value(row: &TapeIndexPayloadObjectRow) -> CborValue {
        let encoded = encode_payload_object_row(row).expect("Object row encodes");
        ciborium::from_reader(encoded.as_slice()).expect("Object row value decodes")
    }

    fn object_row_slot(value: &CborValue) -> Vec<u8> {
        let encoded = encode_cbor_value(value, "test Object row").expect("Object row encodes");
        encode_slot(
            &encoded,
            TAPE_INDEX_PAYLOAD_OBJECT_ROW_SLOT_LEN,
            "Object recovery row",
        )
        .expect("Object row fits its slot")
    }

    #[test]
    fn payload_length_is_checked_without_outer_replica_geometry() {
        assert_eq!(
            checked_tape_index_payload_byte_len(TapeIndexPayloadCounts {
                structural_entry_count: 3,
                object_row_count: 2,
            })
            .unwrap(),
            3 * TAPE_INDEX_PAYLOAD_STRUCTURAL_SLOT_LEN + 2 * TAPE_INDEX_PAYLOAD_OBJECT_ROW_SLOT_LEN
        );
        assert!(checked_tape_index_payload_byte_len(TapeIndexPayloadCounts {
            structural_entry_count: 0,
            object_row_count: 1,
        })
        .is_err());
        assert!(checked_tape_index_payload_byte_len(TapeIndexPayloadCounts {
            structural_entry_count: u64::MAX,
            object_row_count: 0,
        })
        .is_err());
    }

    #[test]
    fn structural_slot_round_trips_canonically() {
        let entry = TapeIndexPayloadMapEntry {
            tape_file_number: 1,
            kind: TapeIndexPayloadFileKind::Object,
            block_count: 7,
            first_parity_data_ordinal: Some(0),
            protected_ordinal_start: None,
            protected_ordinal_end_exclusive: None,
            epoch_id: None,
        };
        let encoded = encode_payload_map_entry(&entry).unwrap();
        let slot = encode_slot(
            &encoded,
            TAPE_INDEX_PAYLOAD_STRUCTURAL_SLOT_LEN,
            "structural entry",
        )
        .unwrap();
        assert_eq!(
            decode_tape_index_payload_map_entry_slot(&slot).unwrap(),
            entry
        );
    }

    #[test]
    fn object_row_ignores_canonical_unknown_integer_keys() {
        let row = plaintext_object_row();
        let extended = with_object_row_field(
            encoded_object_row_value(&row),
            -1,
            CborValue::Map(vec![
                (CborValue::Integer(1.into()), CborValue::Integer(7.into())),
                (CborValue::Integer(2.into()), CborValue::Bool(true)),
            ]),
        );
        let extended = with_object_row_field(extended, 24, CborValue::Bytes(vec![0xaa, 0xbb]));

        assert_eq!(
            decode_tape_index_payload_object_row_slot(&object_row_slot(&extended), 256 * 1024)
                .expect("canonical unknown integer fields are ignored"),
            row
        );
    }

    #[test]
    fn object_row_rejects_fields_assigned_to_the_other_representation() {
        let cases = [
            (plaintext_object_row(), 21, "encrypted field key 21"),
            (encrypted_object_row(), 10, "plaintext field key 10"),
        ];
        for (row, key, expected) in cases {
            let extended =
                with_object_row_field(encoded_object_row_value(&row), key, CborValue::Null);
            let error =
                decode_tape_index_payload_object_row_slot(&object_row_slot(&extended), 256 * 1024)
                    .expect_err(
                        "assigned cross-representation field must reject regardless of type",
                    );
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    #[test]
    fn object_row_checks_canonical_encoding_of_unknown_fields() {
        let row = plaintext_object_row();
        let extended = with_object_row_field(
            encoded_object_row_value(&row),
            24,
            CborValue::Integer(0.into()),
        );
        let mut encoded = encode_cbor_value(&extended, "test Object row").unwrap();
        assert_eq!(encoded.last(), Some(&0), "unknown value is the final byte");
        encoded.pop();
        encoded.extend_from_slice(&[0x18, 0x00]);
        let slot = encode_slot(
            &encoded,
            TAPE_INDEX_PAYLOAD_OBJECT_ROW_SLOT_LEN,
            "Object recovery row",
        )
        .unwrap();

        let error = decode_tape_index_payload_object_row_slot(&slot, 256 * 1024)
            .expect_err("non-shortest unknown value must reject");
        assert!(
            error
                .to_string()
                .contains("not the deterministic canonical encoding"),
            "{error}"
        );
    }

    #[test]
    fn object_row_checks_deterministic_order_inside_unknown_maps() {
        let row = plaintext_object_row();
        let extended = with_object_row_field(
            encoded_object_row_value(&row),
            24,
            CborValue::Map(vec![
                (CborValue::Integer(2.into()), CborValue::Bool(true)),
                (CborValue::Integer(1.into()), CborValue::Bool(false)),
            ]),
        );

        let error =
            decode_tape_index_payload_object_row_slot(&object_row_slot(&extended), 256 * 1024)
                .expect_err("noncanonical nested extension map must reject");
        assert!(
            error.to_string().contains("not in deterministic order"),
            "{error}"
        );
    }

    #[test]
    fn pre_a_payload_rejects_later_bootstrap_terminal_rows_and_nonfinal_parity_map() {
        let cases = [
            (
                vec![
                    structural_entry(0, TapeIndexPayloadFileKind::Bootstrap),
                    structural_entry(1, TapeIndexPayloadFileKind::Bootstrap),
                ],
                "Bootstrap outside tape file 0",
            ),
            (
                vec![
                    structural_entry(0, TapeIndexPayloadFileKind::Bootstrap),
                    structural_entry(1, TapeIndexPayloadFileKind::TapeIndexReplica),
                ],
                "forbidden in the pre-A payload",
            ),
            (
                vec![
                    structural_entry(0, TapeIndexPayloadFileKind::Bootstrap),
                    structural_entry(1, TapeIndexPayloadFileKind::ParityMap),
                    structural_entry(2, TapeIndexPayloadFileKind::Object),
                ],
                "ParityMap must be the final structural entry",
            ),
        ];
        for (entries, expected) in cases {
            let error = stream_entries(entries).expect_err("hostile pre-A shape must fail");
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    #[test]
    fn pre_a_payload_requires_final_parity_map_iff_sidecars_and_full_protection() {
        let cases = [
            (
                vec![
                    structural_entry(0, TapeIndexPayloadFileKind::Bootstrap),
                    structural_entry(1, TapeIndexPayloadFileKind::Object),
                    sidecar_entry(2, 0, 1),
                ],
                "exactly one final ParityMap iff",
            ),
            (
                vec![
                    structural_entry(0, TapeIndexPayloadFileKind::Bootstrap),
                    structural_entry(1, TapeIndexPayloadFileKind::ParityMap),
                ],
                "exactly one final ParityMap iff",
            ),
            (
                vec![
                    structural_entry(0, TapeIndexPayloadFileKind::Bootstrap),
                    TapeIndexPayloadMapEntry {
                        block_count: 2,
                        ..structural_entry(1, TapeIndexPayloadFileKind::Object)
                    },
                    sidecar_entry(2, 0, 1),
                    structural_entry(3, TapeIndexPayloadFileKind::ParityMap),
                ],
                "protect every Object ordinal",
            ),
        ];
        for (entries, expected) in cases {
            let error = stream_entries(entries).expect_err("hostile parity closeout must fail");
            assert!(error.to_string().contains(expected), "{error}");
        }
    }
}
