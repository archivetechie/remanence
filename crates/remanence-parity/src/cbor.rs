//! Shared validation for deterministic integer-keyed CBOR maps.

use std::collections::BTreeSet;

use ciborium::value::Value as CborValue;

/// Tracks duplicate keys and RFC 8949 deterministic key order within one map.
#[derive(Debug, Default)]
pub(crate) struct IntegerMapKeyTracker {
    previous: Option<(i128, Vec<u8>)>,
    seen: BTreeSet<i128>,
}

impl IntegerMapKeyTracker {
    /// Validates and returns the next integer key in one CBOR map.
    pub(crate) fn next(&mut self, key: CborValue, context: &str) -> Result<i128, String> {
        let CborValue::Integer(key) = key else {
            return Err(format!("{context} contains non-integer CBOR map key"));
        };
        let key: i128 = key.into();
        if !self.seen.insert(key) {
            return Err(format!("{context} contains duplicate CBOR map key {key}"));
        }

        let encoded = deterministic_integer_encoding(key);
        if let Some((previous, previous_encoded)) = self.previous.as_ref() {
            if (encoded.len(), encoded.as_slice())
                <= (previous_encoded.len(), previous_encoded.as_slice())
            {
                return Err(format!(
                    "{context} CBOR map key {key} is not in deterministic order after key {previous}"
                ));
            }
        }
        self.previous = Some((key, encoded));
        Ok(key)
    }
}

/// Returns the shortest RFC 8949 encoding of one CBOR integer map key.
fn deterministic_integer_encoding(value: i128) -> Vec<u8> {
    let (major, argument) = if value >= 0 {
        (
            0_u8,
            u64::try_from(value).expect("ciborium integer must fit the CBOR uint range"),
        )
    } else {
        (
            1_u8,
            u64::try_from(-1 - value).expect("ciborium integer must fit the CBOR nint range"),
        )
    };
    let initial = major << 5;
    match argument {
        0..=23 => vec![initial | argument as u8],
        24..=0xff => vec![initial | 24, argument as u8],
        0x100..=0xffff => {
            let mut encoded = vec![initial | 25];
            encoded.extend_from_slice(&(argument as u16).to_be_bytes());
            encoded
        }
        0x1_0000..=0xffff_ffff => {
            let mut encoded = vec![initial | 26];
            encoded.extend_from_slice(&(argument as u32).to_be_bytes());
            encoded
        }
        _ => {
            let mut encoded = vec![initial | 27];
            encoded.extend_from_slice(&argument.to_be_bytes());
            encoded
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_orders_keys_by_encoded_length_then_bytes() {
        let mut tracker = IntegerMapKeyTracker::default();
        for key in [23, -1, 24, -25] {
            tracker
                .next(CborValue::Integer(key.into()), "test map")
                .expect("encoded-key order accepts mixed integer keys");
        }

        let mut numeric_order = IntegerMapKeyTracker::default();
        numeric_order
            .next(CborValue::Integer((-1).into()), "test map")
            .expect("first key accepts");
        let error = numeric_order
            .next(CborValue::Integer(0.into()), "test map")
            .expect_err("numeric order differs from deterministic encoding order");
        assert!(error.contains("key 0"), "{error}");
        assert!(error.contains("after key -1"), "{error}");
    }
}
