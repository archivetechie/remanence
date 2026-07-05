//! Verification extraction of the v0.4.4 parity ordinal/stripe mapping logic.
//!
//! This crate is a standalone, dependency-free model of
//! `crates/remanence-parity/src/mapping.rs`'s pure arithmetic. It keeps the
//! production formulas in Aeneas-friendly shape and uses `u64` for all exposed
//! coordinates so the proof can focus on the arithmetic rather than Rust cast
//! details. The `drift_guard` test pins the production formulas this extraction
//! mirrors; if it fails, the extraction and Lean proofs must be re-synced.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MappingError {
    DataShardsPerEpochOverflow,
    DataShardsPerEpochZero,
    StripeIndexOutsideScheme,
    DataIndexOutsideScheme,
    ParityShardHasNoDataOrdinal,
    DataOrdinalOverflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StripePosition {
    Data { index: u64 },
    Parity { index: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StripeAddress {
    pub neighborhood: u64,
    pub stripe_index: u64,
    pub position: StripePosition,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParityScheme {
    pub data_blocks_per_stripe: u64,
    pub parity_blocks_per_stripe: u64,
    pub stripes_per_neighborhood: u64,
}

pub fn data_shards_per_epoch(scheme: ParityScheme) -> Result<u64, MappingError> {
    let count = match scheme
        .stripes_per_neighborhood
        .checked_mul(scheme.data_blocks_per_stripe)
    {
        Some(count) => count,
        None => return Err(MappingError::DataShardsPerEpochOverflow),
    };
    if count == 0 {
        return Err(MappingError::DataShardsPerEpochZero);
    }
    Ok(count)
}

pub fn ordinal_to_stripe(
    ordinal: u64,
    scheme: ParityScheme,
) -> Result<StripeAddress, MappingError> {
    let s = scheme.stripes_per_neighborhood;
    let epoch_data_shards = data_shards_per_epoch(scheme)?;
    let epoch = ordinal / epoch_data_shards;
    let ordinal_in_epoch = ordinal % epoch_data_shards;

    Ok(StripeAddress {
        neighborhood: epoch,
        stripe_index: ordinal_in_epoch % s,
        position: StripePosition::Data {
            index: ordinal_in_epoch / s,
        },
    })
}

pub fn stripe_data_to_ordinal(
    addr: StripeAddress,
    scheme: ParityScheme,
) -> Result<u64, MappingError> {
    let s = scheme.stripes_per_neighborhood;
    if addr.stripe_index >= s {
        return Err(MappingError::StripeIndexOutsideScheme);
    }

    let data_index = match addr.position {
        StripePosition::Data { index } => {
            if index >= scheme.data_blocks_per_stripe {
                return Err(MappingError::DataIndexOutsideScheme);
            }
            index
        }
        StripePosition::Parity { .. } => {
            return Err(MappingError::ParityShardHasNoDataOrdinal);
        }
    };

    let epoch_data_shards = data_shards_per_epoch(scheme)?;
    let epoch_base = match addr.neighborhood.checked_mul(epoch_data_shards) {
        Some(base) => base,
        None => return Err(MappingError::DataOrdinalOverflow),
    };
    let data_offset = match data_index.checked_mul(s) {
        Some(offset) => offset,
        None => return Err(MappingError::DataOrdinalOverflow),
    };
    let base = match epoch_base.checked_add(data_offset) {
        Some(base) => base,
        None => return Err(MappingError::DataOrdinalOverflow),
    };
    match base.checked_add(addr.stripe_index) {
        Some(ordinal) => Ok(ordinal),
        None => Err(MappingError::DataOrdinalOverflow),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small_scheme() -> ParityScheme {
        ParityScheme {
            data_blocks_per_stripe: 4,
            parity_blocks_per_stripe: 2,
            stripes_per_neighborhood: 3,
        }
    }

    #[test]
    fn drift_guard() {
        let this_file = include_str!("lib.rs");
        let original = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/remanence-parity/src/mapping.rs"
        ))
        .expect("original mapping.rs must be readable from verif/parity-mapping");

        let snippets: &[&str] = &[
            "let count = u64::from(scheme.stripes_per_neighborhood)\n        .checked_mul(u64::from(scheme.data_blocks_per_stripe))",
            "let epoch = ordinal / epoch_data_shards;\n    let ordinal_in_epoch = ordinal % epoch_data_shards;",
            "stripe_index: (ordinal_in_epoch % s) as u32,",
            "index: (ordinal_in_epoch / s) as u16,",
            "if u64::from(addr.stripe_index) >= s {",
            "if index >= scheme.data_blocks_per_stripe {",
            ".and_then(|base| base.checked_add(data_index.checked_mul(s)?))\n        .and_then(|base| base.checked_add(u64::from(addr.stripe_index)))",
        ];
        for (i, snippet) in snippets.iter().enumerate() {
            assert!(
                original.contains(snippet),
                "snippet {i} no longer in remanence-parity mapping.rs -- original \
                 changed; re-sync this extraction and its Lean proofs"
            );
        }

        let extraction_snippets: &[&str] = &[
            "let epoch = ordinal / epoch_data_shards;\n    let ordinal_in_epoch = ordinal % epoch_data_shards;",
            "stripe_index: ordinal_in_epoch % s,",
            "index: ordinal_in_epoch / s,",
            "let epoch_base = match addr.neighborhood.checked_mul(epoch_data_shards)",
            "let data_offset = match data_index.checked_mul(s)",
            "let base = match epoch_base.checked_add(data_offset)",
            "match base.checked_add(addr.stripe_index)",
        ];
        for (i, snippet) in extraction_snippets.iter().enumerate() {
            assert!(
                this_file.contains(snippet),
                "extraction snippet {i} missing from verif mapping model"
            );
        }
    }

    #[test]
    fn mapping_round_trips_across_epochs() {
        let scheme = small_scheme();
        let epoch_data_shards = data_shards_per_epoch(scheme).unwrap();
        for ordinal in 0..(3 * epoch_data_shards) {
            let addr = ordinal_to_stripe(ordinal, scheme).unwrap();
            let back = stripe_data_to_ordinal(addr, scheme).unwrap();
            assert_eq!(back, ordinal);
        }
    }

    #[test]
    fn mapping_rejects_bad_addresses() {
        let scheme = small_scheme();
        assert_eq!(
            stripe_data_to_ordinal(
                StripeAddress {
                    neighborhood: 0,
                    stripe_index: scheme.stripes_per_neighborhood,
                    position: StripePosition::Data { index: 0 },
                },
                scheme,
            )
            .unwrap_err(),
            MappingError::StripeIndexOutsideScheme
        );
        assert_eq!(
            stripe_data_to_ordinal(
                StripeAddress {
                    neighborhood: 0,
                    stripe_index: 0,
                    position: StripePosition::Data {
                        index: scheme.data_blocks_per_stripe,
                    },
                },
                scheme,
            )
            .unwrap_err(),
            MappingError::DataIndexOutsideScheme
        );
        assert_eq!(
            stripe_data_to_ordinal(
                StripeAddress {
                    neighborhood: 0,
                    stripe_index: 0,
                    position: StripePosition::Parity { index: 0 },
                },
                scheme,
            )
            .unwrap_err(),
            MappingError::ParityShardHasNoDataOrdinal
        );
    }
}
