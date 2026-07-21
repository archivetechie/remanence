//! Stripe mapping helpers.
//!
//! Layer 3c v0.4.4 maps object-data `ParityDataOrdinal` values to
//! epoch/stripe/data-index coordinates; parity shards live in sidecar tape
//! files and are addressed by the sidecar index. The active implementation no
//! longer exposes the v0.2 physical LBA ↔ stripe mapping because data and
//! parity do not share one inline block stream.
//!
//! The mappings are pure functions of the scheme parameters; no per-tape state
//! is needed beyond the filemark map that translates object blocks to ordinals.

use crate::error::ParityError;
use crate::model::{ParityScheme, StripeAddress, StripePosition};

/// Number of object-data shards in one v0.4.4 parity epoch: `S * k`.
pub fn data_shards_per_epoch(scheme: &ParityScheme) -> Result<u64, ParityError> {
    let count = u64::from(scheme.stripes_per_neighborhood)
        .checked_mul(u64::from(scheme.data_blocks_per_stripe))
        .ok_or(ParityError::Invariant("data shards per epoch overflows"))?;
    if count == 0 {
        return Err(ParityError::Invariant("data shards per epoch is zero"));
    }
    Ok(count)
}

/// Map a global `ParityDataOrdinal` to its epoch-local stripe coordinates.
///
/// [`StripeAddress::neighborhood`] carries the v0.4.4 parity epoch id.
#[cfg(test)]
pub(crate) fn ordinal_to_stripe(
    ordinal: u64,
    scheme: &ParityScheme,
) -> Result<StripeAddress, ParityError> {
    let s = u64::from(scheme.stripes_per_neighborhood);
    let epoch_data_shards = data_shards_per_epoch(scheme)?;
    let epoch = ordinal / epoch_data_shards;
    let ordinal_in_epoch = ordinal % epoch_data_shards;

    Ok(StripeAddress {
        neighborhood: epoch,
        stripe_index: (ordinal_in_epoch % s) as u32,
        position: StripePosition::Data {
            index: (ordinal_in_epoch / s) as u16,
        },
    })
}

/// Map an ordinal through an explicitly ranged parity epoch.
///
/// Epoch identifiers are monotonic labels, not ordinal arithmetic. Callers
/// must first select the sidecar whose descriptor range contains `ordinal`,
/// then pass that descriptor's epoch id, start, and real data-shard count here.
pub fn ordinal_to_stripe_in_epoch(
    ordinal: u64,
    epoch_id: u64,
    protected_ordinal_start: u64,
    real_data_shard_count: u64,
    scheme: &ParityScheme,
) -> Result<StripeAddress, ParityError> {
    let logical_data_shards = data_shards_per_epoch(scheme)?;
    if real_data_shard_count == 0 || real_data_shard_count > logical_data_shards {
        return Err(ParityError::Invariant(
            "explicit epoch real data-shard count is outside 1..=S*k",
        ));
    }
    let ordinal_in_epoch =
        ordinal
            .checked_sub(protected_ordinal_start)
            .ok_or(ParityError::Invariant(
                "ordinal precedes explicit epoch protected range",
            ))?;
    if ordinal_in_epoch >= real_data_shard_count {
        return Err(ParityError::Invariant(
            "ordinal lies outside explicit epoch protected range",
        ));
    }
    let stripes = u64::from(scheme.stripes_per_neighborhood);

    Ok(StripeAddress {
        neighborhood: epoch_id,
        stripe_index: (ordinal_in_epoch % stripes) as u32,
        position: StripePosition::Data {
            index: (ordinal_in_epoch / stripes) as u16,
        },
    })
}

/// Map epoch-local data coordinates back to an ordinal using the descriptor
/// range start rather than deriving the start from the epoch id.
///
/// The returned ordinal can be beyond the descriptor's real shard count; that
/// coordinate represents an implicit zero in a short epoch. Callers compare it
/// with the descriptor end before attempting an object-data read.
pub fn stripe_data_to_ordinal_in_epoch(
    addr: &StripeAddress,
    protected_ordinal_start: u64,
    scheme: &ParityScheme,
) -> Result<u64, ParityError> {
    let stripes = u64::from(scheme.stripes_per_neighborhood);
    if u64::from(addr.stripe_index) >= stripes {
        return Err(ParityError::Invariant("stripe_index outside scheme"));
    }
    let data_index = match addr.position {
        StripePosition::Data { index } => {
            if index >= scheme.data_blocks_per_stripe {
                return Err(ParityError::Invariant("data index outside scheme"));
            }
            u64::from(index)
        }
        StripePosition::Parity { .. } => {
            return Err(ParityError::Invariant("parity shard has no data ordinal"));
        }
    };

    protected_ordinal_start
        .checked_add(
            data_index
                .checked_mul(stripes)
                .and_then(|offset| offset.checked_add(u64::from(addr.stripe_index)))
                .ok_or(ParityError::Invariant("epoch-local data offset overflows"))?,
        )
        .ok_or(ParityError::Invariant("data ordinal overflows"))
}

/// Reverse [`ordinal_to_stripe`] for a data shard.
///
/// Parity shards are intentionally rejected because they are addressed inside
/// the sidecar index by `(epoch, stripe_index, parity_index)`, not by the
/// object-data ordinal stream.
#[cfg(test)]
pub(crate) fn stripe_data_to_ordinal(
    addr: &StripeAddress,
    scheme: &ParityScheme,
) -> Result<u64, ParityError> {
    let s = u64::from(scheme.stripes_per_neighborhood);
    if u64::from(addr.stripe_index) >= s {
        return Err(ParityError::Invariant("stripe_index outside scheme"));
    }

    let data_index = match addr.position {
        StripePosition::Data { index } => {
            if index >= scheme.data_blocks_per_stripe {
                return Err(ParityError::Invariant("data index outside scheme"));
            }
            u64::from(index)
        }
        StripePosition::Parity { .. } => {
            return Err(ParityError::Invariant("parity shard has no data ordinal"));
        }
    };

    let epoch_data_shards = data_shards_per_epoch(scheme)?;
    addr.neighborhood
        .checked_mul(epoch_data_shards)
        .and_then(|base| base.checked_add(data_index.checked_mul(s)?))
        .and_then(|base| base.checked_add(u64::from(addr.stripe_index)))
        .ok_or(ParityError::Invariant("data ordinal overflows"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SchemeId;

    fn small_scheme() -> ParityScheme {
        // k=4, m=2, S=3 → 3 × (4+2) = 18 blocks/neighborhood.
        // Small enough to enumerate the full mapping in tests.
        ParityScheme {
            id: SchemeId::new_static("test"),
            data_blocks_per_stripe: 4,
            parity_blocks_per_stripe: 2,
            stripes_per_neighborhood: 3,
        }
    }

    #[test]
    fn ordinal_to_stripe_matches_v044_row_major_epoch_mapping() {
        let s = small_scheme();
        let cases = [
            (0, 0, 0, StripePosition::Data { index: 0 }),
            (1, 0, 1, StripePosition::Data { index: 0 }),
            (2, 0, 2, StripePosition::Data { index: 0 }),
            (3, 0, 0, StripePosition::Data { index: 1 }),
            (11, 0, 2, StripePosition::Data { index: 3 }),
            (12, 1, 0, StripePosition::Data { index: 0 }),
            (23, 1, 2, StripePosition::Data { index: 3 }),
        ];

        for (ordinal, epoch, stripe_index, position) in cases {
            let addr = ordinal_to_stripe(ordinal, &s).expect("ordinal maps");
            assert_eq!(addr.neighborhood, epoch, "ordinal {ordinal}");
            assert_eq!(addr.stripe_index, stripe_index, "ordinal {ordinal}");
            assert_eq!(addr.position, position, "ordinal {ordinal}");
        }
    }

    #[test]
    fn ordinal_mapping_round_trips_across_epochs() {
        let s = small_scheme();
        for ordinal in 0..(3 * data_shards_per_epoch(&s).unwrap()) {
            let addr = ordinal_to_stripe(ordinal, &s).expect("ordinal maps");
            let back = stripe_data_to_ordinal(&addr, &s).expect("stripe maps back");
            assert_eq!(back, ordinal, "ordinal {ordinal}");
        }
    }

    #[test]
    fn ordinal_mapping_hits_every_epoch_local_data_coordinate_once() {
        let s = small_scheme();
        let epoch_data_shards = data_shards_per_epoch(&s).unwrap();

        for epoch in 0..3 {
            let mut seen = vec![
                vec![false; s.data_blocks_per_stripe as usize];
                s.stripes_per_neighborhood as usize
            ];
            let base = epoch * epoch_data_shards;

            for offset in 0..epoch_data_shards {
                let ordinal = base + offset;
                let addr = ordinal_to_stripe(ordinal, &s).expect("ordinal maps");
                assert_eq!(addr.neighborhood, epoch, "ordinal {ordinal}");

                let StripePosition::Data { index } = addr.position else {
                    panic!("ordinal {ordinal} mapped to a parity shard");
                };
                let slot = &mut seen[addr.stripe_index as usize][index as usize];
                assert!(
                    !*slot,
                    "duplicate coordinate for ordinal {ordinal}: {addr:?}"
                );
                *slot = true;
            }

            for stripe_index in 0..s.stripes_per_neighborhood {
                for data_index in 0..s.data_blocks_per_stripe {
                    assert!(
                        seen[stripe_index as usize][data_index as usize],
                        "epoch {epoch} missed stripe {stripe_index} data {data_index}"
                    );
                }
            }
        }
    }

    #[test]
    fn consecutive_ordinals_follow_row_major_stripe_then_data_index_pattern() {
        let s = small_scheme();
        let epoch_data_shards = data_shards_per_epoch(&s).unwrap();
        let epoch = 2;
        let base = epoch * epoch_data_shards;

        for offset in 0..epoch_data_shards {
            let ordinal = base + offset;
            let addr = ordinal_to_stripe(ordinal, &s).expect("ordinal maps");
            assert_eq!(addr.neighborhood, epoch, "ordinal {ordinal}");
            assert_eq!(
                addr.stripe_index,
                (offset % u64::from(s.stripes_per_neighborhood)) as u32,
                "consecutive ordinals must walk stripe_index first"
            );
            assert_eq!(
                addr.position,
                StripePosition::Data {
                    index: (offset / u64::from(s.stripes_per_neighborhood)) as u16,
                },
                "data index must advance only after all stripes in the row are assigned"
            );
        }

        let next_epoch =
            ordinal_to_stripe(base + epoch_data_shards, &s).expect("next epoch first ordinal maps");
        assert_eq!(next_epoch.neighborhood, epoch + 1);
        assert_eq!(next_epoch.stripe_index, 0);
        assert_eq!(next_epoch.position, StripePosition::Data { index: 0 });
    }

    #[test]
    fn final_partial_epoch_boundary_counts_follow_row_major_mapping() {
        let s = small_scheme();
        let epoch_data_shards = data_shards_per_epoch(&s).unwrap();
        let epoch = 5;
        let epoch_base = epoch * epoch_data_shards;
        let boundary_counts = [
            0,
            1,
            u64::from(s.data_blocks_per_stripe - 1),
            u64::from(s.data_blocks_per_stripe),
            u64::from(s.data_blocks_per_stripe + 1),
            epoch_data_shards - 1,
        ];

        for real_shards_in_final_epoch in boundary_counts {
            for offset in 0..real_shards_in_final_epoch {
                let ordinal = epoch_base + offset;
                let addr = ordinal_to_stripe(ordinal, &s).expect("ordinal maps");
                assert_eq!(addr.neighborhood, epoch, "ordinal {ordinal}");
                assert_eq!(
                    addr.stripe_index,
                    (offset % u64::from(s.stripes_per_neighborhood)) as u32,
                    "ordinal {ordinal}"
                );
                assert_eq!(
                    addr.position,
                    StripePosition::Data {
                        index: (offset / u64::from(s.stripes_per_neighborhood)) as u16,
                    },
                    "ordinal {ordinal}"
                );
                assert_eq!(
                    stripe_data_to_ordinal(&addr, &s).expect("stripe maps back"),
                    ordinal,
                    "ordinal {ordinal}"
                );
            }

            if real_shards_in_final_epoch < epoch_data_shards {
                let first_implicit_zero = epoch_base + real_shards_in_final_epoch;
                let addr =
                    ordinal_to_stripe(first_implicit_zero, &s).expect("implicit-zero slot maps");
                assert_eq!(addr.neighborhood, epoch);
                assert_eq!(
                    addr.stripe_index,
                    (real_shards_in_final_epoch % u64::from(s.stripes_per_neighborhood)) as u32
                );
                assert_eq!(
                    addr.position,
                    StripePosition::Data {
                        index: (real_shards_in_final_epoch / u64::from(s.stripes_per_neighborhood))
                            as u16,
                    }
                );
            }
        }
    }

    #[test]
    fn stripe_data_to_ordinal_rejects_non_data_and_out_of_range_addresses() {
        let s = small_scheme();

        let parity = StripeAddress {
            neighborhood: 0,
            stripe_index: 0,
            position: StripePosition::Parity { index: 0 },
        };
        let err = stripe_data_to_ordinal(&parity, &s).unwrap_err();
        assert!(format!("{err}").contains("parity shard"), "{err}");

        let bad_stripe = StripeAddress {
            neighborhood: 0,
            stripe_index: s.stripes_per_neighborhood,
            position: StripePosition::Data { index: 0 },
        };
        let err = stripe_data_to_ordinal(&bad_stripe, &s).unwrap_err();
        assert!(format!("{err}").contains("stripe_index"), "{err}");

        let bad_data_index = StripeAddress {
            neighborhood: 0,
            stripe_index: 0,
            position: StripePosition::Data {
                index: s.data_blocks_per_stripe,
            },
        };
        let err = stripe_data_to_ordinal(&bad_data_index, &s).unwrap_err();
        assert!(format!("{err}").contains("data index"), "{err}");
    }
}
