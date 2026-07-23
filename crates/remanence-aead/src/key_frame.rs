//! Canonical RAO wrapped-DEK key-frame codec.

use crate::error::{RaoAeadError, Result};
use crate::xwing::XWING_CIPHERTEXT_LEN;

/// Maximum RAO 2.0 key-frame size accepted from an object.
pub const RAO_KEY_FRAME_MAX_LEN: usize = 16_384;
/// Smallest RAO 2.0 one-slot X-Wing key frame (with an empty label).
pub const RAO_KEY_FRAME_MIN_LEN: usize = 5 + 1 + 16 + 1 + XWING_CIPHERTEXT_LEN + 48;
/// Maximum number of recipient slots in a key frame.
pub const RAO_KEY_FRAME_MAX_SLOTS: usize = 8;

const MAGIC: &[u8; 4] = b"RAOK";
const RECIPIENT_SLOT_FIXED_LEN: usize = 1 + 16 + 1 + XWING_CIPHERTEXT_LEN + 48;

/// One recipient's HPKE-wrapped data-encryption key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipientSlot {
    /// Canonical strictly increasing slot index.
    pub slot_index: u8,
    /// Recipient epoch identifier.
    pub recipient_epoch_id: [u8; 16],
    /// Printable-ASCII label used during recovery ceremonies.
    pub epoch_label: String,
    /// RFC 9180 X-Wing encapsulated key.
    pub enc: [u8; XWING_CIPHERTEXT_LEN],
    /// Wrapped 32-byte DEK plus a 16-byte AEAD tag.
    pub ciphertext: [u8; 48],
}

/// Canonically ordered RAO recipient slots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyFrame {
    /// Recipient slots in strictly ascending slot-index order.
    pub slots: Vec<RecipientSlot>,
}

impl KeyFrame {
    /// Construct a frame and enforce all canonical slot rules.
    pub fn new(slots: Vec<RecipientSlot>) -> Result<Self> {
        let frame = Self { slots };
        frame.validate()?;
        Ok(frame)
    }

    /// Parse a complete frame, rejecting truncation, non-canonical order, and trailing bytes.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if !(RAO_KEY_FRAME_MIN_LEN..=RAO_KEY_FRAME_MAX_LEN).contains(&bytes.len()) {
            return Err(RaoAeadError::InvalidKeyFrameLength);
        }
        if bytes.get(..4) != Some(MAGIC) {
            return Err(RaoAeadError::InvalidKeyFrame);
        }
        let count = bytes[4] as usize;
        if !(1..=RAO_KEY_FRAME_MAX_SLOTS).contains(&count) {
            return Err(RaoAeadError::InvalidKeyFrame);
        }
        let mut cursor = 5usize;
        let mut slots = Vec::with_capacity(count);
        for _ in 0..count {
            let slot_index = take(bytes, &mut cursor, 1)?[0];
            let recipient_epoch_id = take(bytes, &mut cursor, 16)?
                .try_into()
                .map_err(|_| RaoAeadError::InvalidKeyFrame)?;
            let label_len = take(bytes, &mut cursor, 1)?[0] as usize;
            if label_len > 32 {
                return Err(RaoAeadError::InvalidKeyFrame);
            }
            let label = take(bytes, &mut cursor, label_len)?;
            if !label.iter().all(|b| (0x20..=0x7e).contains(b)) {
                return Err(RaoAeadError::InvalidKeyFrame);
            }
            let epoch_label = std::str::from_utf8(label)
                .map_err(|_| RaoAeadError::InvalidKeyFrame)?
                .to_owned();
            let enc = take(bytes, &mut cursor, XWING_CIPHERTEXT_LEN)?
                .try_into()
                .map_err(|_| RaoAeadError::InvalidKeyFrame)?;
            let ciphertext = take(bytes, &mut cursor, 48)?
                .try_into()
                .map_err(|_| RaoAeadError::InvalidKeyFrame)?;
            slots.push(RecipientSlot {
                slot_index,
                recipient_epoch_id,
                epoch_label,
                enc,
                ciphertext,
            });
        }
        if cursor != bytes.len() {
            return Err(RaoAeadError::InvalidKeyFrame);
        }
        Self::new(slots)
    }

    /// Serialize this frame in its byte-exact canonical wire encoding.
    pub fn serialize(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let capacity = 5 + self
            .slots
            .iter()
            .map(|slot| RECIPIENT_SLOT_FIXED_LEN + slot.epoch_label.len())
            .sum::<usize>();
        if !(RAO_KEY_FRAME_MIN_LEN..=RAO_KEY_FRAME_MAX_LEN).contains(&capacity) {
            return Err(RaoAeadError::InvalidKeyFrameLength);
        }
        let mut out = Vec::with_capacity(capacity);
        out.extend_from_slice(MAGIC);
        out.push(self.slots.len() as u8);
        for slot in &self.slots {
            out.push(slot.slot_index);
            out.extend_from_slice(&slot.recipient_epoch_id);
            out.push(slot.epoch_label.len() as u8);
            out.extend_from_slice(slot.epoch_label.as_bytes());
            out.extend_from_slice(&slot.enc);
            out.extend_from_slice(&slot.ciphertext);
        }
        Ok(out)
    }

    fn validate(&self) -> Result<()> {
        if !(1..=RAO_KEY_FRAME_MAX_SLOTS).contains(&self.slots.len()) {
            return Err(RaoAeadError::InvalidKeyFrame);
        }
        let mut previous = None;
        for (index, slot) in self.slots.iter().enumerate() {
            if previous.is_some_and(|value| slot.slot_index <= value)
                || self.slots[..index]
                    .iter()
                    .any(|earlier| earlier.recipient_epoch_id == slot.recipient_epoch_id)
                || slot.epoch_label.len() > 32
                || !slot
                    .epoch_label
                    .as_bytes()
                    .iter()
                    .all(|b| (0x20..=0x7e).contains(b))
            {
                return Err(RaoAeadError::InvalidKeyFrame);
            }
            previous = Some(slot.slot_index);
        }
        Ok(())
    }
}

fn take<'a>(bytes: &'a [u8], cursor: &mut usize, len: usize) -> Result<&'a [u8]> {
    let end = cursor
        .checked_add(len)
        .ok_or(RaoAeadError::InvalidKeyFrame)?;
    let value = bytes
        .get(*cursor..end)
        .ok_or(RaoAeadError::InvalidKeyFrame)?;
    *cursor = end;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(index: u8, label: &str) -> RecipientSlot {
        RecipientSlot {
            slot_index: index,
            recipient_epoch_id: [index; 16],
            epoch_label: label.to_owned(),
            enc: [index.wrapping_add(1); XWING_CIPHERTEXT_LEN],
            ciphertext: [index.wrapping_add(2); 48],
        }
    }

    #[test]
    fn byte_exact_round_trip() {
        assert_eq!(RAO_KEY_FRAME_MIN_LEN, 1191);
        assert_eq!(RAO_KEY_FRAME_MAX_LEN, 16_384);
        let frame = KeyFrame::new(vec![slot(0, "safe-2026"), slot(7, "escrow")]).unwrap();
        let bytes = frame.serialize().unwrap();
        assert_eq!(
            bytes.len(),
            5 + 2 * RECIPIENT_SLOT_FIXED_LEN + "safe-2026".len() + "escrow".len()
        );
        assert_eq!(&bytes[..5], b"RAOK\x02");
        assert_eq!(bytes[5], 0);
        assert_eq!(bytes[22], 9);
        assert_eq!(&bytes[23..32], b"safe-2026");
        assert_eq!(KeyFrame::parse(&bytes).unwrap(), frame);
        assert_eq!(frame.serialize().unwrap(), bytes);
    }

    #[test]
    fn accepts_valid_eight_slot_xwing_frame_within_bounds() {
        let frame = KeyFrame::new(
            (0..RAO_KEY_FRAME_MAX_SLOTS)
                .map(|index| slot(index as u8, &format!("recipient-{index}")))
                .collect(),
        )
        .unwrap();
        let encoded = frame.serialize().unwrap();
        assert!((RAO_KEY_FRAME_MIN_LEN..=RAO_KEY_FRAME_MAX_LEN).contains(&encoded.len()));
        assert_eq!(KeyFrame::parse(&encoded).unwrap(), frame);
    }

    #[test]
    fn rejects_truncation_order_duplicates_trailing_and_out_of_bounds_lengths() {
        assert!(KeyFrame::new(vec![slot(1, "a"), slot(1, "b")]).is_err());
        assert!(KeyFrame::new(vec![slot(2, "a"), slot(1, "b")]).is_err());
        let mut duplicate_epoch = slot(2, "b");
        duplicate_epoch.recipient_epoch_id = slot(1, "a").recipient_epoch_id;
        assert!(KeyFrame::new(vec![slot(1, "a"), duplicate_epoch]).is_err());
        let bytes = KeyFrame::new(vec![slot(0, "a")])
            .unwrap()
            .serialize()
            .unwrap();
        assert!(KeyFrame::parse(&bytes[..bytes.len() - 1]).is_err());
        let mut trailing = bytes;
        trailing.push(0);
        assert!(KeyFrame::parse(&trailing).is_err());
        assert!(matches!(
            KeyFrame::parse(&vec![0; RAO_KEY_FRAME_MIN_LEN - 1]),
            Err(RaoAeadError::InvalidKeyFrameLength)
        ));
        assert!(matches!(
            KeyFrame::parse(&vec![0; RAO_KEY_FRAME_MAX_LEN + 1]),
            Err(RaoAeadError::InvalidKeyFrameLength)
        ));
    }
}
