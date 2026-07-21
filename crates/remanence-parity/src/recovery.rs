//! Sidecar-aware recovery primitives for Layer 3c v0.4.4.
//!
//! The public helper in this module reconstructs one protected
//! `ParityDataOrdinal` from the authenticated filemark map, the epoch sidecar,
//! verified data peers, and verified parity shards. It is intentionally below
//! the future object-scoped `ObjectParitySource` surface: callers provide the
//! failed ordinal directly, and this code performs the core sidecar/CRC/RS work
//! that `ObjectParitySource::recover_block_at` will later drive from
//! `(tape_file_number, body_lba)`.

use std::collections::{BTreeMap, BTreeSet};

use crate::codec::ReedSolomonCodec;
use crate::durable::DurableBoundaryState;
use crate::error::ParityError;
use crate::filemark_map::{
    MapScope, ScopedFilemarkMap, TapeFileKind, TapeFileMapEntry, TapeFilePosition,
};
#[cfg(test)]
use crate::mapping::{data_shards_per_epoch, ordinal_to_stripe};
use crate::mapping::{ordinal_to_stripe_in_epoch, stripe_data_to_ordinal_in_epoch};
use crate::model::{ParityScheme, SidecarMetadataHealth, StripeAddress, StripePosition};
use crate::raw::{PhysicalPositionHint, RawReadOutcome, RawTapeSource};
use crate::sidecar::{
    data_shard_crc64, parity_shard_crc64, parse_sidecar_footer_block, parse_sidecar_header_block,
    parse_sidecar_index_blocks, DecodedSidecarIndex, ParityShardIndexEntry, SidecarCopyKind,
    SidecarFooter,
};

/// Result of reconstructing one protected object-data block from a sidecar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidecarRecoveryResult {
    /// Object-data ordinal requested by the caller.
    pub failed_ordinal: u64,
    /// Epoch/stripe/data-index coordinates for `failed_ordinal`.
    pub stripe: StripeAddress,
    /// Tape-file number of the sidecar used for this reconstruction.
    pub sidecar_tape_file_number: u32,
    /// Health of the replicated sidecar metadata copies observed while
    /// preparing this recovery.
    pub sidecar_metadata_health: SidecarMetadataHealth,
    /// Reconstructed fixed-size object-data block.
    pub recovered_block: Vec<u8>,
    /// Stripe members that were treated as erasures in this attempt.
    ///
    /// This always includes the failed data shard itself and also includes any
    /// clean-reading peer whose sidecar-recorded CRC did not match.
    pub lost_shards: Vec<StripePosition>,
}

/// One recovered block from a region-level sidecar recovery plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidecarRegionRecoveryBlock {
    /// Object-local body LBA requested by the caller.
    pub body_lba: u64,
    /// Recovery result for the corresponding object-data ordinal.
    pub result: SidecarRecoveryResult,
}

#[derive(Clone, Copy, Debug)]
struct RequestedDataShard {
    body_lba: u64,
    ordinal: u64,
    data_index: u16,
}

#[derive(Clone, Debug, Default)]
struct EpochRegionRequest {
    stripes: BTreeMap<u32, Vec<RequestedDataShard>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum BulkShardKey {
    Data {
        stripe_index: u32,
        data_index: u16,
    },
    Parity {
        stripe_index: u32,
        parity_index: u16,
    },
}

#[derive(Clone, Copy, Debug)]
enum BulkReadKind {
    Data { expected_crc64: u64 },
    Parity { expected_crc64: u64 },
}

#[derive(Clone, Copy, Debug)]
struct BulkReadPlanItem {
    key: BulkShardKey,
    physical: PhysicalPositionHint,
    kind: BulkReadKind,
}

type BulkWindowCache = BTreeMap<BulkShardKey, Option<Vec<u8>>>;

#[derive(Clone, Debug)]
struct SidecarIndexRead {
    index: DecodedSidecarIndex,
    metadata_health: SidecarMetadataHealth,
}

/// Recover a contiguous object-local region through an epoch-scoped sidecar plan.
///
/// Unlike repeated single-block recovery, this treats every requested data
/// shard as suspect, loads each affected epoch's sidecar metadata once, reads
/// each surviving peer at most once per recovery window in physical order, and
/// reconstructs all requested blocks for a stripe together.
#[allow(clippy::too_many_arguments)]
pub fn recover_object_region_from_sidecar(
    source: &mut dyn RawTapeSource,
    scoped_map: &ScopedFilemarkMap,
    scheme: &ParityScheme,
    tape_uuid: [u8; 16],
    block_size: u32,
    tape_file_number: u32,
    start_body_lba: u64,
    block_count: u64,
    max_stripes_per_window: u64,
) -> Result<Vec<SidecarRegionRecoveryBlock>, ParityError> {
    if max_stripes_per_window == 0 {
        return Err(ParityError::Invariant(
            "bulk recovery max_stripes_per_window is zero",
        ));
    }
    let read_boundary = DurableBoundaryState::from_scoped_map(scoped_map)?;
    let end_body_lba = start_body_lba
        .checked_add(block_count)
        .ok_or(ParityError::Invariant("bulk recovery range overflows"))?;
    let capacity = usize::try_from(block_count)
        .map_err(|_| ParityError::Invariant("bulk recovery block_count does not fit usize"))?;
    let mut epochs: BTreeMap<u64, EpochRegionRequest> = BTreeMap::new();
    let mut output_order = Vec::with_capacity(capacity);

    let mut body_lba = start_body_lba;
    while body_lba < end_body_lba {
        let position = TapeFilePosition {
            tape_file_number,
            block_within_file: body_lba,
        };
        let ordinal = scoped_map.map.ordinal_at(position)?.ok_or_else(|| {
            ParityError::FilemarkMapReconstruct(format!(
                "tape file {tape_file_number} is not an object tape file"
            ))
        })?;
        scoped_map.scope.recoverable(ordinal)?;
        ensure_inside_durable_recovery_boundary(
            scoped_map,
            &read_boundary,
            tape_file_number,
            ordinal,
        )?;
        let sidecar_entry = find_sidecar_covering_ordinal(scoped_map, ordinal)?;
        let stripe = stripe_for_sidecar_ordinal(sidecar_entry, ordinal, scheme)?;
        let StripePosition::Data { index } = stripe.position else {
            return Err(ParityError::Invariant(
                "ordinal_to_stripe returned a parity address",
            ));
        };
        let requested = RequestedDataShard {
            body_lba,
            ordinal,
            data_index: index,
        };
        epochs
            .entry(stripe.neighborhood)
            .or_default()
            .stripes
            .entry(stripe.stripe_index)
            .or_default()
            .push(requested);
        output_order.push(body_lba);
        body_lba = body_lba.checked_add(1).ok_or(ParityError::Invariant(
            "bulk recovery request scan overflows",
        ))?;
    }

    source.configure_fixed_block_size(block_size)?;
    let mut recovered_by_lba = BTreeMap::new();
    for (epoch_id, request) in epochs {
        recover_epoch_region_from_sidecar(
            source,
            scoped_map,
            &read_boundary,
            scheme,
            &tape_uuid,
            block_size,
            epoch_id,
            request,
            max_stripes_per_window,
            &mut recovered_by_lba,
        )?;
    }

    output_order
        .into_iter()
        .map(|body_lba| {
            let result = recovered_by_lba
                .remove(&body_lba)
                .ok_or(ParityError::Invariant(
                    "bulk recovery did not produce a requested block",
                ))?;
            Ok(SidecarRegionRecoveryBlock { body_lba, result })
        })
        .collect()
}

/// Recover one object-local body block using the v0.4.4 sidecar layout.
///
/// This is the object-addressed bridge that `ObjectParitySource::recover_block_at`
/// will call: it resolves `(tape_file_number, body_lba)` to a
/// `ParityDataOrdinal` through the authenticated filemark map, rejects
/// unvalidated-prefix recovery before any tape I/O, and then delegates to the
/// sidecar-aware ordinal recovery core.
pub fn recover_object_block_from_sidecar(
    source: &mut dyn RawTapeSource,
    scoped_map: &ScopedFilemarkMap,
    scheme: &ParityScheme,
    tape_uuid: [u8; 16],
    block_size: u32,
    tape_file_number: u32,
    body_lba: u64,
) -> Result<SidecarRecoveryResult, ParityError> {
    let read_boundary = DurableBoundaryState::from_scoped_map(scoped_map)?;
    let position = TapeFilePosition {
        tape_file_number,
        block_within_file: body_lba,
    };
    let failed_ordinal = scoped_map.map.ordinal_at(position)?.ok_or_else(|| {
        ParityError::FilemarkMapReconstruct(format!(
            "tape file {tape_file_number} is not an object tape file"
        ))
    })?;

    recover_ordinal_from_sidecar_inside_boundary(
        source,
        scoped_map,
        &read_boundary,
        scheme,
        tape_uuid,
        block_size,
        failed_ordinal,
    )
}

/// Recover one protected object-data ordinal using the v0.4.4 sidecar layout.
///
/// The caller must already have acquired a [`ScopedFilemarkMap`] from the
/// catalog or authoritative bootstrap. This function enforces the scoped
/// per-block recoverability check before touching tape:
///
/// - prefix scope: `failed_ordinal` must be inside the authenticated prefix;
/// - protection watermark: `failed_ordinal` must be below the highest
///   committed sidecar ordinal.
///
/// Data peers are verified against the sidecar data-CRC index before they are
/// supplied to Reed-Solomon reconstruction. CRC mismatches and peer read
/// failures are treated as additional erasures, never as trusted shards.
pub fn recover_ordinal_from_sidecar(
    source: &mut dyn RawTapeSource,
    scoped_map: &ScopedFilemarkMap,
    scheme: &ParityScheme,
    tape_uuid: [u8; 16],
    block_size: u32,
    failed_ordinal: u64,
) -> Result<SidecarRecoveryResult, ParityError> {
    let read_boundary = DurableBoundaryState::from_scoped_map(scoped_map)?;
    recover_ordinal_from_sidecar_inside_boundary(
        source,
        scoped_map,
        &read_boundary,
        scheme,
        tape_uuid,
        block_size,
        failed_ordinal,
    )
}

fn recover_ordinal_from_sidecar_inside_boundary(
    source: &mut dyn RawTapeSource,
    scoped_map: &ScopedFilemarkMap,
    read_boundary: &DurableBoundaryState,
    scheme: &ParityScheme,
    tape_uuid: [u8; 16],
    block_size: u32,
    failed_ordinal: u64,
) -> Result<SidecarRecoveryResult, ParityError> {
    scoped_map.scope.recoverable(failed_ordinal)?;
    let failed_position = scoped_map.map.position_for_ordinal(failed_ordinal)?;
    ensure_inside_durable_recovery_boundary(
        scoped_map,
        read_boundary,
        failed_position.tape_file_number,
        failed_ordinal,
    )?;

    let sidecar_entry = find_sidecar_covering_ordinal(scoped_map, failed_ordinal)?;
    let failed_stripe = stripe_for_sidecar_ordinal(sidecar_entry, failed_ordinal, scheme)?;
    let failed_data_index = match failed_stripe.position {
        StripePosition::Data { index } => index as usize,
        StripePosition::Parity { .. } => {
            return Err(ParityError::Invariant(
                "ordinal_to_stripe returned a parity address",
            ));
        }
    };
    let epoch_start = sidecar_range(sidecar_entry)?.0;
    ensure_inside_durable_recovery_boundary(
        scoped_map,
        read_boundary,
        sidecar_entry.tape_file_number,
        failed_ordinal,
    )?;

    source.configure_fixed_block_size(block_size)?;
    let sidecar =
        read_and_parse_sidecar_index(source, scoped_map, sidecar_entry, &tape_uuid, block_size)?;
    validate_sidecar_for_recovery(
        &sidecar.index,
        sidecar_entry,
        scheme,
        &failed_stripe,
        epoch_start,
        block_size,
    )?;

    let codec = ReedSolomonCodec::new(scheme)?;
    let k = codec.data_blocks();
    let m = codec.parity_blocks();
    let mut shards = vec![None; k + m];
    let mut attempted = vec![false; k + m];
    attempted[failed_data_index] = true;

    for data_index in 0..k {
        if data_index == failed_data_index {
            continue;
        }
        let position = StripePosition::Data {
            index: data_index as u16,
        };
        let ordinal = stripe_data_to_ordinal_in_epoch(
            &StripeAddress {
                neighborhood: failed_stripe.neighborhood,
                stripe_index: failed_stripe.stripe_index,
                position,
            },
            epoch_start,
            scheme,
        )?;
        attempted[data_index] = true;
        if ordinal >= sidecar.index.header.protected_ordinal_end_exclusive {
            shards[data_index] = Some(vec![0u8; block_size as usize]);
            continue;
        }

        if let Some(peer) = read_verified_data_peer(
            source,
            scoped_map,
            &sidecar.index,
            ordinal,
            epoch_start,
            block_size,
            read_boundary,
        )? {
            shards[data_index] = Some(peer);
        }
    }

    for parity_index in 0..m {
        let shard_index = k + parity_index;
        attempted[shard_index] = true;
        if let Some((entry_index, entry)) = sidecar
            .index
            .index
            .parity_entries
            .iter()
            .enumerate()
            .find(|(_, entry)| {
                entry.stripe_index == failed_stripe.stripe_index
                    && entry.parity_index == parity_index as u16
            })
        {
            shards[shard_index] = read_verified_parity_peer(
                source,
                scoped_map,
                sidecar_entry,
                sidecar.index.header.shard_index_block_count,
                entry_index,
                entry,
                block_size,
            )?;
        }
    }

    let surviving = shards.iter().filter(|shard| shard.is_some()).count();
    let lost_shards = lost_shards(&attempted, &shards, k);
    if surviving < k {
        return Err(ParityError::Unrecoverable {
            stripe: failed_stripe,
            lost_count: lost_shards.len() as u16,
            limit: m as u16,
        });
    }

    codec.reconstruct(&mut shards)?;
    let recovered = shards[failed_data_index]
        .take()
        .ok_or(ParityError::Invariant(
            "reconstruct did not fill failed shard",
        ))?;
    if recovered.len() != block_size as usize {
        return Err(ParityError::Invariant(
            "reconstructed shard length does not match fixed block size",
        ));
    }
    let expected_crc = data_crc_for_ordinal(&sidecar.index, failed_ordinal, epoch_start)?;
    let actual_crc = data_shard_crc64(&recovered);
    if actual_crc != expected_crc {
        return Err(ParityError::Unrecoverable {
            stripe: failed_stripe,
            lost_count: lost_shards.len() as u16,
            limit: m as u16,
        });
    }

    let failed_position = scoped_map
        .map
        .position_for_ordinal(failed_ordinal)
        .and_then(|position| scoped_map.map.physical_position(position))?;
    source.locate_physical(PhysicalPositionHint {
        lba: failed_position.lba.saturating_add(1),
        partition: failed_position.partition,
    })?;

    Ok(SidecarRecoveryResult {
        failed_ordinal,
        stripe: failed_stripe,
        sidecar_tape_file_number: sidecar_entry.tape_file_number,
        sidecar_metadata_health: sidecar.metadata_health,
        recovered_block: recovered,
        lost_shards,
    })
}

#[allow(clippy::too_many_arguments)]
fn recover_epoch_region_from_sidecar(
    source: &mut dyn RawTapeSource,
    scoped_map: &ScopedFilemarkMap,
    read_boundary: &DurableBoundaryState,
    scheme: &ParityScheme,
    tape_uuid: &[u8; 16],
    block_size: u32,
    epoch_id: u64,
    request: EpochRegionRequest,
    max_stripes_per_window: u64,
    recovered_by_lba: &mut BTreeMap<u64, SidecarRecoveryResult>,
) -> Result<(), ParityError> {
    let sidecar_entry = find_epoch_sidecar(scoped_map, epoch_id)?;
    let epoch_start = sidecar_range(sidecar_entry)?.0;
    ensure_inside_durable_recovery_boundary(
        scoped_map,
        read_boundary,
        sidecar_entry.tape_file_number,
        epoch_start,
    )?;
    let sidecar =
        read_and_parse_sidecar_index(source, scoped_map, sidecar_entry, tape_uuid, block_size)?;
    let representative_stripe = request
        .stripes
        .keys()
        .next()
        .copied()
        .ok_or(ParityError::Invariant("bulk recovery epoch has no stripes"))?;
    validate_sidecar_for_recovery(
        &sidecar.index,
        sidecar_entry,
        scheme,
        &StripeAddress {
            neighborhood: epoch_id,
            stripe_index: representative_stripe,
            position: StripePosition::Data { index: 0 },
        },
        epoch_start,
        block_size,
    )?;

    let window_size = usize::try_from(max_stripes_per_window).map_err(|_| {
        ParityError::Invariant("bulk recovery max_stripes_per_window does not fit usize")
    })?;
    if window_size == 0 {
        return Err(ParityError::Invariant(
            "bulk recovery max_stripes_per_window is zero",
        ));
    }
    let codec = ReedSolomonCodec::new(scheme)?;
    let stripes: Vec<(u32, Vec<RequestedDataShard>)> = request.stripes.into_iter().collect();
    for window in stripes.chunks(window_size) {
        let cache = read_bulk_window_peers(
            source,
            scoped_map,
            read_boundary,
            sidecar_entry,
            &sidecar.index,
            scheme,
            epoch_start,
            block_size,
            window,
        )?;
        for (stripe_index, requested) in window {
            recover_bulk_stripe_from_cache(
                &cache,
                &sidecar.index,
                sidecar_entry,
                &codec,
                scheme,
                sidecar.metadata_health,
                epoch_id,
                epoch_start,
                block_size,
                *stripe_index,
                requested,
                recovered_by_lba,
            )?;
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn read_bulk_window_peers(
    source: &mut dyn RawTapeSource,
    scoped_map: &ScopedFilemarkMap,
    read_boundary: &DurableBoundaryState,
    sidecar_entry: &TapeFileMapEntry,
    sidecar: &DecodedSidecarIndex,
    scheme: &ParityScheme,
    epoch_start: u64,
    block_size: u32,
    window: &[(u32, Vec<RequestedDataShard>)],
) -> Result<BulkWindowCache, ParityError> {
    let mut requested = BTreeMap::<u32, BTreeSet<u16>>::new();
    for (stripe_index, shards) in window {
        let entry = requested.entry(*stripe_index).or_default();
        for shard in shards {
            entry.insert(shard.data_index);
        }
    }

    let mut plan = Vec::new();
    for (stripe_index, requested_indexes) in &requested {
        for data_index in 0..scheme.data_blocks_per_stripe {
            if requested_indexes.contains(&data_index) {
                continue;
            }
            let ordinal = stripe_data_to_ordinal_in_epoch(
                &StripeAddress {
                    neighborhood: sidecar.header.epoch_id,
                    stripe_index: *stripe_index,
                    position: StripePosition::Data { index: data_index },
                },
                epoch_start,
                scheme,
            )?;
            if ordinal >= sidecar.header.protected_ordinal_end_exclusive {
                continue;
            }
            let position = scoped_map.map.position_for_ordinal(ordinal)?;
            if !read_boundary.contains_committed_tape_file(position.tape_file_number) {
                continue;
            }
            let expected_crc64 = data_crc_for_ordinal(sidecar, ordinal, epoch_start)?;
            plan.push(BulkReadPlanItem {
                key: BulkShardKey::Data {
                    stripe_index: *stripe_index,
                    data_index,
                },
                physical: scoped_map.map.physical_position(position)?,
                kind: BulkReadKind::Data { expected_crc64 },
            });
        }

        for parity_index in 0..scheme.parity_blocks_per_stripe {
            let parity_entry_index = parity_entry_index(sidecar, *stripe_index, parity_index)?;
            let block_within_file = u64::from(sidecar.header.shard_index_block_count)
                .checked_add(
                    u64::try_from(parity_entry_index)
                        .map_err(|_| ParityError::Invariant("parity entry index overflows u64"))?,
                )
                .ok_or(ParityError::Invariant(
                    "sidecar parity block offset overflows",
                ))?;
            let entry = sidecar
                .index
                .parity_entries
                .get(parity_entry_index)
                .ok_or(ParityError::Invariant("sidecar parity entry missing"))?;
            plan.push(BulkReadPlanItem {
                key: BulkShardKey::Parity {
                    stripe_index: *stripe_index,
                    parity_index,
                },
                physical: scoped_map.map.physical_position(TapeFilePosition {
                    tape_file_number: sidecar_entry.tape_file_number,
                    block_within_file,
                })?,
                kind: BulkReadKind::Parity {
                    expected_crc64: entry.parity_shard_crc64,
                },
            });
        }
    }

    plan.sort_by_key(|item| (item.physical.partition, item.physical.lba, item.key));
    let mut cache = BTreeMap::new();
    for item in plan {
        if cache.contains_key(&item.key) {
            continue;
        }
        let block = match read_required_block(source, item.physical, block_size) {
            Ok(block) => block,
            Err(_) => {
                cache.insert(item.key, None);
                continue;
            }
        };
        let valid = match item.kind {
            BulkReadKind::Data { expected_crc64 } => data_shard_crc64(&block) == expected_crc64,
            BulkReadKind::Parity { expected_crc64 } => parity_shard_crc64(&block) == expected_crc64,
        };
        cache.insert(item.key, valid.then_some(block));
    }
    Ok(cache)
}

#[allow(clippy::too_many_arguments)]
fn recover_bulk_stripe_from_cache(
    cache: &BulkWindowCache,
    sidecar: &DecodedSidecarIndex,
    sidecar_entry: &TapeFileMapEntry,
    codec: &ReedSolomonCodec,
    scheme: &ParityScheme,
    sidecar_metadata_health: SidecarMetadataHealth,
    epoch_id: u64,
    epoch_start: u64,
    block_size: u32,
    stripe_index: u32,
    requested: &[RequestedDataShard],
    recovered_by_lba: &mut BTreeMap<u64, SidecarRecoveryResult>,
) -> Result<(), ParityError> {
    let k = codec.data_blocks();
    let m = codec.parity_blocks();
    let mut shards = vec![None; k + m];
    let mut attempted = vec![false; k + m];
    let requested_by_index: BTreeMap<u16, RequestedDataShard> = requested
        .iter()
        .copied()
        .map(|request| (request.data_index, request))
        .collect();

    for data_index in 0..scheme.data_blocks_per_stripe {
        let shard_index = usize::from(data_index);
        attempted[shard_index] = true;
        if requested_by_index.contains_key(&data_index) {
            continue;
        }
        let ordinal = stripe_data_to_ordinal_in_epoch(
            &StripeAddress {
                neighborhood: epoch_id,
                stripe_index,
                position: StripePosition::Data { index: data_index },
            },
            epoch_start,
            scheme,
        )?;
        if ordinal >= sidecar.header.protected_ordinal_end_exclusive {
            shards[shard_index] = Some(vec![0u8; block_size as usize]);
            continue;
        }
        shards[shard_index] = cache
            .get(&BulkShardKey::Data {
                stripe_index,
                data_index,
            })
            .cloned()
            .flatten();
    }

    for parity_index in 0..scheme.parity_blocks_per_stripe {
        let shard_index = k + usize::from(parity_index);
        attempted[shard_index] = true;
        shards[shard_index] = cache
            .get(&BulkShardKey::Parity {
                stripe_index,
                parity_index,
            })
            .cloned()
            .flatten();
    }

    let surviving = shards.iter().filter(|shard| shard.is_some()).count();
    let first_requested_index = requested
        .first()
        .map(|request| request.data_index)
        .unwrap_or(0);
    let stripe = StripeAddress {
        neighborhood: epoch_id,
        stripe_index,
        position: StripePosition::Data {
            index: first_requested_index,
        },
    };
    let lost_shards = lost_shards(&attempted, &shards, k);
    if surviving < k {
        return Err(ParityError::Unrecoverable {
            stripe,
            lost_count: lost_shards.len() as u16,
            limit: m as u16,
        });
    }

    codec.reconstruct(&mut shards)?;
    for request in requested {
        let shard_index = usize::from(request.data_index);
        let recovered = shards
            .get(shard_index)
            .and_then(|shard| shard.as_ref())
            .ok_or(ParityError::Invariant(
                "bulk reconstruct did not fill requested shard",
            ))?
            .clone();
        if recovered.len() != block_size as usize {
            return Err(ParityError::Invariant(
                "bulk reconstructed shard length does not match fixed block size",
            ));
        }
        let expected_crc = data_crc_for_ordinal(sidecar, request.ordinal, epoch_start)?;
        if data_shard_crc64(&recovered) != expected_crc {
            return Err(ParityError::Unrecoverable {
                stripe: StripeAddress {
                    neighborhood: epoch_id,
                    stripe_index,
                    position: StripePosition::Data {
                        index: request.data_index,
                    },
                },
                lost_count: lost_shards.len() as u16,
                limit: m as u16,
            });
        }
        recovered_by_lba.insert(
            request.body_lba,
            SidecarRecoveryResult {
                failed_ordinal: request.ordinal,
                stripe: StripeAddress {
                    neighborhood: epoch_id,
                    stripe_index,
                    position: StripePosition::Data {
                        index: request.data_index,
                    },
                },
                sidecar_tape_file_number: sidecar_entry.tape_file_number,
                sidecar_metadata_health,
                recovered_block: recovered,
                lost_shards: lost_shards.clone(),
            },
        );
    }

    Ok(())
}

fn parity_entry_index(
    sidecar: &DecodedSidecarIndex,
    stripe_index: u32,
    parity_index: u16,
) -> Result<usize, ParityError> {
    let m = usize::from(sidecar.header.m);
    let stripe = usize::try_from(stripe_index)
        .map_err(|_| ParityError::Invariant("stripe index does not fit usize"))?;
    let parity = usize::from(parity_index);
    let index = stripe
        .checked_mul(m)
        .and_then(|base| base.checked_add(parity))
        .ok_or(ParityError::Invariant("parity entry index overflows"))?;
    let entry = sidecar
        .index
        .parity_entries
        .get(index)
        .ok_or(ParityError::Invariant("sidecar parity entry missing"))?;
    if entry.stripe_index != stripe_index || entry.parity_index != parity_index {
        return Err(ParityError::SidecarParse(
            "sidecar parity entry order does not match scheme".to_string(),
        ));
    }
    Ok(index)
}

fn outside_validated_prefix_error(scoped_map: &ScopedFilemarkMap, ordinal: u64) -> ParityError {
    match &scoped_map.scope {
        MapScope::Prefix {
            map_total_data_ordinals,
            ..
        } => ParityError::OutsideValidatedMapPrefix {
            ordinal,
            prefix_ordinals: *map_total_data_ordinals,
        },
        MapScope::Complete { .. } => ParityError::Invariant(
            "complete filemark map unexpectedly marked a tape file unvalidated",
        ),
    }
}

fn ensure_inside_durable_recovery_boundary(
    scoped_map: &ScopedFilemarkMap,
    read_boundary: &DurableBoundaryState,
    tape_file_number: u32,
    ordinal: u64,
) -> Result<(), ParityError> {
    if read_boundary.contains_committed_tape_file(tape_file_number) {
        Ok(())
    } else {
        Err(outside_validated_prefix_error(scoped_map, ordinal))
    }
}

fn find_epoch_sidecar(
    scoped_map: &ScopedFilemarkMap,
    epoch_id: u64,
) -> Result<&TapeFileMapEntry, ParityError> {
    scoped_map
        .map
        .entries()
        .iter()
        .find(|entry| entry.kind == TapeFileKind::ParitySidecar && entry.epoch_id == Some(epoch_id))
        .ok_or_else(|| {
            ParityError::FilemarkMapReconstruct(format!(
                "no parity sidecar entry for protected epoch {epoch_id}"
            ))
        })
}

fn find_sidecar_covering_ordinal(
    scoped_map: &ScopedFilemarkMap,
    ordinal: u64,
) -> Result<&TapeFileMapEntry, ParityError> {
    scoped_map
        .map
        .entries()
        .iter()
        .find(|entry| {
            entry.kind == TapeFileKind::ParitySidecar
                && matches!(
                    (entry.protected_ordinal_start, entry.protected_ordinal_end_exclusive),
                    (Some(start), Some(end)) if start <= ordinal && ordinal < end
                )
        })
        .ok_or_else(|| {
            ParityError::FilemarkMapReconstruct(format!(
                "no parity sidecar range contains protected ordinal {ordinal}"
            ))
        })
}

fn sidecar_range(entry: &TapeFileMapEntry) -> Result<(u64, u64, u64), ParityError> {
    match (
        entry.epoch_id,
        entry.protected_ordinal_start,
        entry.protected_ordinal_end_exclusive,
    ) {
        (Some(epoch_id), Some(start), Some(end)) if start < end => Ok((start, end, epoch_id)),
        _ => Err(ParityError::FilemarkMapReconstruct(format!(
            "parity sidecar tape file {} has incomplete epoch range metadata",
            entry.tape_file_number
        ))),
    }
}

pub(crate) fn stripe_for_sidecar_ordinal(
    entry: &TapeFileMapEntry,
    ordinal: u64,
    scheme: &ParityScheme,
) -> Result<StripeAddress, ParityError> {
    let (start, end, epoch_id) = sidecar_range(entry)?;
    let real_data_shard_count = end
        .checked_sub(start)
        .ok_or(ParityError::Invariant("sidecar protected range underflows"))?;
    ordinal_to_stripe_in_epoch(ordinal, epoch_id, start, real_data_shard_count, scheme)
}

fn read_and_parse_sidecar_index(
    source: &mut dyn RawTapeSource,
    scoped_map: &ScopedFilemarkMap,
    sidecar_entry: &TapeFileMapEntry,
    tape_uuid: &[u8; 16],
    block_size: u32,
) -> Result<SidecarIndexRead, ParityError> {
    if sidecar_entry.block_count == 0 {
        return Err(ParityError::SidecarParse(format!(
            "sidecar map entry {} has zero block_count",
            sidecar_entry.tape_file_number
        )));
    }
    let footer_block = match read_sidecar_block(
        source,
        scoped_map,
        sidecar_entry,
        sidecar_entry.block_count - 1,
        block_size,
    ) {
        Ok(block) => block,
        Err(_err) => {
            return read_primary_sidecar_index_without_footer(
                source,
                scoped_map,
                sidecar_entry,
                tape_uuid,
                block_size,
            )
        }
    };
    let footer = match parse_sidecar_footer_block(&footer_block, tape_uuid) {
        Ok(footer) => footer,
        Err(_err) => {
            return read_primary_sidecar_index_without_footer(
                source,
                scoped_map,
                sidecar_entry,
                tape_uuid,
                block_size,
            )
        }
    };
    if sidecar_entry.block_count != footer.sidecar_total_block_count {
        return Err(ParityError::SidecarParse(format!(
            "sidecar map block_count {} does not match footer total {}",
            sidecar_entry.block_count, footer.sidecar_total_block_count
        )));
    }

    let primary = read_sidecar_index_copy(
        source,
        scoped_map,
        sidecar_entry,
        tape_uuid,
        block_size,
        &footer,
        SidecarCopyKind::Primary,
    );
    let tail = read_sidecar_index_copy(
        source,
        scoped_map,
        sidecar_entry,
        tape_uuid,
        block_size,
        &footer,
        SidecarCopyKind::Tail,
    );

    match (primary, tail) {
        (Ok(primary), Ok(_tail)) => Ok(SidecarIndexRead {
            index: primary,
            metadata_health: SidecarMetadataHealth::BothCopiesUsable,
        }),
        (Ok(primary), Err(_tail_err)) => Ok(SidecarIndexRead {
            index: primary,
            metadata_health: SidecarMetadataHealth::TailCopyLost,
        }),
        (Err(_primary_err), Ok(tail)) => Ok(SidecarIndexRead {
            index: tail,
            metadata_health: SidecarMetadataHealth::PrimaryHeaderLost,
        }),
        (Err(_primary_err), Err(_tail_err)) => Err(ParityError::SidecarMetadataUnavailable {
            epoch_id: footer.epoch_id,
        }),
    }
}

fn read_primary_sidecar_index_without_footer(
    source: &mut dyn RawTapeSource,
    scoped_map: &ScopedFilemarkMap,
    sidecar_entry: &TapeFileMapEntry,
    tape_uuid: &[u8; 16],
    block_size: u32,
) -> Result<SidecarIndexRead, ParityError> {
    let block0 = read_sidecar_block(source, scoped_map, sidecar_entry, 0, block_size)
        .map_err(|_| sidecar_metadata_unavailable_from_map_entry(sidecar_entry))?;
    let header = parse_sidecar_header_block(&block0, tape_uuid)
        .map_err(|_| sidecar_metadata_unavailable_from_map_entry(sidecar_entry))?;
    if header.copy_kind != SidecarCopyKind::Primary {
        return Err(sidecar_metadata_unavailable_from_map_entry(sidecar_entry));
    }
    if sidecar_entry.block_count != header.sidecar_total_block_count {
        return Err(ParityError::SidecarParse(format!(
            "sidecar map block_count {} does not match primary header total {}",
            sidecar_entry.block_count, header.sidecar_total_block_count
        )));
    }

    let h = usize::try_from(header.shard_index_block_count)
        .map_err(|_| ParityError::Invariant("sidecar index block count overflows usize"))?;
    let mut blocks = Vec::with_capacity(h);
    blocks.push(block0);
    for block_within_file in 1..u64::from(header.shard_index_block_count) {
        blocks.push(read_sidecar_block(
            source,
            scoped_map,
            sidecar_entry,
            block_within_file,
            block_size,
        )?);
    }
    let decoded = parse_sidecar_index_blocks(&blocks, tape_uuid)?;
    if decoded.header.copy_kind != SidecarCopyKind::Primary {
        return Err(ParityError::SidecarParse(
            "sidecar primary metadata copy decoded as non-primary".into(),
        ));
    }
    if decoded.header.sidecar_total_block_count != sidecar_entry.block_count {
        return Err(ParityError::SidecarParse(format!(
            "sidecar map block_count {} does not match decoded primary total {}",
            sidecar_entry.block_count, decoded.header.sidecar_total_block_count
        )));
    }
    Ok(SidecarIndexRead {
        index: decoded,
        metadata_health: SidecarMetadataHealth::TailCopyLost,
    })
}

fn sidecar_metadata_unavailable_from_map_entry(sidecar_entry: &TapeFileMapEntry) -> ParityError {
    match sidecar_entry.epoch_id {
        Some(epoch_id) => ParityError::SidecarMetadataUnavailable { epoch_id },
        None => ParityError::SidecarParse(format!(
            "sidecar map entry {} is missing an epoch id",
            sidecar_entry.tape_file_number
        )),
    }
}

fn read_sidecar_index_copy(
    source: &mut dyn RawTapeSource,
    scoped_map: &ScopedFilemarkMap,
    sidecar_entry: &TapeFileMapEntry,
    tape_uuid: &[u8; 16],
    block_size: u32,
    footer: &SidecarFooter,
    expected_copy_kind: SidecarCopyKind,
) -> Result<DecodedSidecarIndex, ParityError> {
    let start_block = match expected_copy_kind {
        SidecarCopyKind::Primary => footer.primary_header_start_block,
        SidecarCopyKind::Tail => footer.tail_header_start_block,
    };
    let h = usize::try_from(footer.sidecar_header_block_count)
        .map_err(|_| ParityError::Invariant("sidecar index block count overflows usize"))?;
    let mut blocks = Vec::with_capacity(h);
    for offset in 0..u64::from(footer.sidecar_header_block_count) {
        let block_within_file = start_block
            .checked_add(offset)
            .ok_or(ParityError::Invariant(
                "sidecar index block offset overflows",
            ))?;
        blocks.push(read_sidecar_block(
            source,
            scoped_map,
            sidecar_entry,
            block_within_file,
            block_size,
        )?);
    }

    let decoded = parse_sidecar_index_blocks(&blocks, tape_uuid)?;
    if decoded.header.copy_kind != expected_copy_kind {
        return Err(ParityError::SidecarParse(format!(
            "sidecar {:?} copy decoded as {:?}",
            expected_copy_kind, decoded.header.copy_kind
        )));
    }
    if decoded.header.sidecar_total_block_count != footer.sidecar_total_block_count
        || decoded.header.shard_index_block_count != footer.sidecar_header_block_count
        || decoded.header.parity_block_count != footer.parity_shard_block_count
        || decoded.header.primary_header_start_block != footer.primary_header_start_block
        || decoded.header.tail_header_start_block != footer.tail_header_start_block
        || decoded.header.canonical_metadata_hash != footer.canonical_metadata_hash
    {
        return Err(ParityError::SidecarParse(
            "sidecar metadata copy does not match footer locator".into(),
        ));
    }
    Ok(decoded)
}

fn read_sidecar_block(
    source: &mut dyn RawTapeSource,
    scoped_map: &ScopedFilemarkMap,
    sidecar_entry: &TapeFileMapEntry,
    block_within_file: u64,
    block_size: u32,
) -> Result<Vec<u8>, ParityError> {
    let position = TapeFilePosition {
        tape_file_number: sidecar_entry.tape_file_number,
        block_within_file,
    };
    let physical = scoped_map.map.physical_position(position)?;
    read_required_block(source, physical, block_size).map_err(|err| match err {
        ParityError::TapeIo(_) => ParityError::SidecarParse(format!(
            "could not read sidecar tape_file {} block {block_within_file}",
            sidecar_entry.tape_file_number
        )),
        other => other,
    })
}

fn read_verified_parity_peer(
    source: &mut dyn RawTapeSource,
    scoped_map: &ScopedFilemarkMap,
    sidecar_entry: &TapeFileMapEntry,
    index_block_count: u32,
    parity_entry_index: usize,
    entry: &ParityShardIndexEntry,
    block_size: u32,
) -> Result<Option<Vec<u8>>, ParityError> {
    let parity_offset = u64::try_from(parity_entry_index)
        .map_err(|_| ParityError::Invariant("parity entry index overflows u64"))?;
    let block_within_file = u64::from(index_block_count)
        .checked_add(parity_offset)
        .ok_or(ParityError::Invariant(
            "sidecar parity block offset overflows",
        ))?;
    let position = TapeFilePosition {
        tape_file_number: sidecar_entry.tape_file_number,
        block_within_file,
    };
    let physical = scoped_map.map.physical_position(position)?;
    let block = match read_required_block(source, physical, block_size) {
        Ok(block) => block,
        Err(_) => return Ok(None),
    };
    if parity_shard_crc64(&block) != entry.parity_shard_crc64 {
        return Ok(None);
    }
    Ok(Some(block))
}

fn validate_sidecar_for_recovery(
    sidecar: &DecodedSidecarIndex,
    sidecar_entry: &TapeFileMapEntry,
    scheme: &ParityScheme,
    failed_stripe: &StripeAddress,
    epoch_start: u64,
    block_size: u32,
) -> Result<(), ParityError> {
    if sidecar.header.epoch_id != failed_stripe.neighborhood {
        return Err(ParityError::SidecarParse(format!(
            "sidecar epoch {} does not match failed epoch {}",
            sidecar.header.epoch_id, failed_stripe.neighborhood
        )));
    }
    if sidecar.header.protected_ordinal_start != epoch_start {
        return Err(ParityError::SidecarParse(format!(
            "sidecar starts at ordinal {}, expected epoch start {epoch_start}",
            sidecar.header.protected_ordinal_start
        )));
    }
    let descriptor_real_data_shards = sidecar
        .header
        .protected_ordinal_end_exclusive
        .checked_sub(sidecar.header.protected_ordinal_start)
        .ok_or(ParityError::Invariant("sidecar protected range underflows"))?;
    if sidecar.header.real_data_shard_count != descriptor_real_data_shards {
        return Err(ParityError::SidecarParse(format!(
            "sidecar real_data_shard_count {} does not match descriptor range length {descriptor_real_data_shards}",
            sidecar.header.real_data_shard_count
        )));
    }
    if sidecar_entry.protected_ordinal_start != Some(sidecar.header.protected_ordinal_start)
        || sidecar_entry.protected_ordinal_end_exclusive
            != Some(sidecar.header.protected_ordinal_end_exclusive)
    {
        return Err(ParityError::SidecarParse(
            "sidecar header protected range does not match filemark map entry".to_string(),
        ));
    }
    if sidecar.header.k != scheme.data_blocks_per_stripe
        || sidecar.header.m != scheme.parity_blocks_per_stripe
        || sidecar.header.stripes_per_epoch != scheme.stripes_per_neighborhood
        || sidecar.header.block_size != block_size
    {
        return Err(ParityError::SidecarParse(
            "sidecar scheme or block size does not match recovery scheme".to_string(),
        ));
    }
    Ok(())
}

fn read_verified_data_peer(
    source: &mut dyn RawTapeSource,
    scoped_map: &ScopedFilemarkMap,
    sidecar: &DecodedSidecarIndex,
    ordinal: u64,
    epoch_start: u64,
    block_size: u32,
    read_boundary: &DurableBoundaryState,
) -> Result<Option<Vec<u8>>, ParityError> {
    let expected_crc = data_crc_for_ordinal(sidecar, ordinal, epoch_start)?;
    let position = scoped_map.map.position_for_ordinal(ordinal)?;
    if !read_boundary.contains_committed_tape_file(position.tape_file_number) {
        return Ok(None);
    }
    let physical = scoped_map.map.physical_position(position)?;
    let block = match read_required_block(source, physical, block_size) {
        Ok(block) => block,
        Err(_) => return Ok(None),
    };
    if data_shard_crc64(&block) != expected_crc {
        return Ok(None);
    }
    Ok(Some(block))
}

fn data_crc_for_ordinal(
    sidecar: &DecodedSidecarIndex,
    ordinal: u64,
    epoch_start: u64,
) -> Result<u64, ParityError> {
    if ordinal < epoch_start || ordinal >= sidecar.header.protected_ordinal_end_exclusive {
        return Err(ParityError::Invariant(
            "requested ordinal is outside sidecar real-data range",
        ));
    }
    let index = usize::try_from(ordinal - epoch_start)
        .map_err(|_| ParityError::Invariant("sidecar data CRC index overflows usize"))?;
    sidecar
        .index
        .data_shard_crc64s
        .get(index)
        .copied()
        .ok_or(ParityError::Invariant("sidecar data CRC entry missing"))
}

fn read_required_block(
    source: &mut dyn RawTapeSource,
    position: PhysicalPositionHint,
    block_size: u32,
) -> Result<Vec<u8>, ParityError> {
    source.locate_physical(position)?;
    let mut block = vec![0u8; block_size as usize];
    match source.read_record(&mut block)? {
        RawReadOutcome::Block { bytes, .. } if bytes == block.len() => Ok(block),
        RawReadOutcome::Block { bytes, .. } => Err(ParityError::SidecarParse(format!(
            "short fixed-block read: got {bytes}, expected {}",
            block.len()
        ))),
        RawReadOutcome::Filemark { .. } => Err(ParityError::SidecarParse(
            "unexpected filemark while reading recovery block".to_string(),
        )),
        RawReadOutcome::EndOfData { .. } => Err(ParityError::SidecarParse(
            "unexpected EOD while reading recovery block".to_string(),
        )),
    }
}

fn lost_shards(attempted: &[bool], shards: &[Option<Vec<u8>>], k: usize) -> Vec<StripePosition> {
    (0..shards.len())
        .filter(|&idx| attempted[idx] && shards[idx].is_none())
        .map(|idx| {
            if idx < k {
                StripePosition::Data { index: idx as u16 }
            } else {
                StripePosition::Parity {
                    index: (idx - k) as u16,
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filemark_map::{FilemarkMap, TapeFileMapEntry, TapeFilePosition};
    use crate::model::SchemeId;
    use crate::raw::{RawTapeSink, RawWriteOutcome};
    use crate::resume::{
        emit_resume_rebuilt_sidecars_to_raw_without_journal,
        rebuild_legacy_forensic_open_epoch_from_committed_prefix,
    };
    use crate::sidecar::{data_shard_crc64, encode_sidecar_tape_file, SidecarDescriptor};

    const TAPE_UUID: [u8; 16] = [0x33; 16];
    const BLOCK_SIZE: u32 = 256;

    #[derive(Clone, Debug)]
    enum Record {
        Block(Vec<u8>),
        Filemark,
    }

    #[derive(Debug)]
    struct RawVec {
        records: Vec<Record>,
        cursor: usize,
        configured_block_size: Option<u32>,
        unreadable_lbas: Vec<usize>,
        read_lbas: Vec<usize>,
    }

    impl RawVec {
        fn new(records: Vec<Record>) -> Self {
            Self {
                records,
                cursor: 0,
                configured_block_size: None,
                unreadable_lbas: Vec::new(),
                read_lbas: Vec::new(),
            }
        }
    }

    impl RawTapeSource for RawVec {
        fn configure_fixed_block_size(&mut self, block_size: u32) -> Result<(), ParityError> {
            if block_size == 0 {
                return Err(ParityError::Invariant("test block size is zero"));
            }
            self.configured_block_size = Some(block_size);
            Ok(())
        }

        fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
            self.cursor = usize::try_from(hint.lba)
                .map_err(|_| ParityError::Invariant("test LBA overflows usize"))?;
            Ok(())
        }

        fn space_filemarks(
            &mut self,
            _count: i64,
        ) -> Result<crate::raw::SpaceFilemarksOutcome, ParityError> {
            Err(ParityError::Invariant("test does not use spacing"))
        }

        fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
            self.read_lbas.push(self.cursor);
            if self.unreadable_lbas.contains(&self.cursor) {
                return Err(ParityError::TapeIo(
                    remanence_library::TapeIoError::OperationFailed(format!(
                        "unreadable test LBA {}",
                        self.cursor
                    )),
                ));
            }
            let Some(record) = self.records.get(self.cursor) else {
                return Ok(RawReadOutcome::EndOfData {
                    position_after: PhysicalPositionHint::new(self.cursor as u64),
                });
            };
            match record {
                Record::Block(block) => {
                    let bytes = block.len();
                    buf[..bytes].copy_from_slice(block);
                    self.cursor += 1;
                    Ok(RawReadOutcome::Block {
                        bytes,
                        position_after: PhysicalPositionHint::new(self.cursor as u64),
                    })
                }
                Record::Filemark => {
                    self.cursor += 1;
                    Ok(RawReadOutcome::Filemark {
                        position_after: PhysicalPositionHint::new(self.cursor as u64),
                    })
                }
            }
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            Ok(PhysicalPositionHint::new(self.cursor as u64))
        }
    }

    #[derive(Debug)]
    struct AppendRawVecSink {
        records: Vec<Record>,
        cursor: usize,
    }

    impl AppendRawVecSink {
        fn at_append_position(records: Vec<Record>, append_position: PhysicalPositionHint) -> Self {
            assert_eq!(
                usize::try_from(append_position.lba).unwrap(),
                records.len(),
                "append sink starts after the last committed tape-file filemark"
            );
            Self {
                cursor: records.len(),
                records,
            }
        }

        fn into_source(self) -> RawVec {
            RawVec::new(self.records)
        }
    }

    impl RawTapeSink for AppendRawVecSink {
        fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
            if self.cursor != self.records.len() {
                return Err(ParityError::Invariant(
                    "append sink cursor is not at the physical append point",
                ));
            }
            self.records.push(Record::Block(buf.to_vec()));
            self.cursor += 1;
            Ok(RawWriteOutcome::WroteBlock {
                position_after: PhysicalPositionHint::new(self.cursor as u64),
                early_warning: false,
                end_of_medium: false,
            })
        }

        fn write_filemarks(
            &mut self,
            _count: u32,
            _immed: bool,
        ) -> Result<RawWriteOutcome, ParityError> {
            if self.cursor != self.records.len() {
                return Err(ParityError::Invariant(
                    "append sink cursor is not at the physical append point",
                ));
            }
            self.records.push(Record::Filemark);
            self.cursor += 1;
            Ok(RawWriteOutcome::WroteFilemark {
                position_after: PhysicalPositionHint::new(self.cursor as u64),
                early_warning: false,
                end_of_medium: false,
            })
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            Ok(PhysicalPositionHint::new(self.cursor as u64))
        }
    }

    fn scheme(k: u16, m: u16, stripes: u32) -> ParityScheme {
        ParityScheme {
            id: SchemeId::new_static("recovery-test"),
            data_blocks_per_stripe: k,
            parity_blocks_per_stripe: m,
            stripes_per_neighborhood: stripes,
        }
    }

    fn block(seed: u8) -> Vec<u8> {
        let mut block = vec![seed; BLOCK_SIZE as usize];
        block[0] = seed.wrapping_mul(17);
        block[1] = seed.wrapping_mul(31);
        block
    }

    fn sidecar_for_epoch_with_data_crcs(
        scheme: &ParityScheme,
        object_blocks: &[Vec<u8>],
        data_crcs: Vec<u64>,
    ) -> crate::sidecar::EncodedSidecarTapeFile {
        sidecar_for_epoch_at_with_data_crcs(scheme, 0, 0, object_blocks, data_crcs)
    }

    fn sidecar_for_epoch(
        scheme: &ParityScheme,
        object_blocks: &[Vec<u8>],
    ) -> crate::sidecar::EncodedSidecarTapeFile {
        sidecar_for_epoch_at(scheme, 0, 0, object_blocks)
    }

    fn sidecar_for_epoch_at(
        scheme: &ParityScheme,
        epoch_id: u64,
        protected_ordinal_start: u64,
        object_blocks: &[Vec<u8>],
    ) -> crate::sidecar::EncodedSidecarTapeFile {
        let data_crcs = object_blocks
            .iter()
            .map(|block| data_shard_crc64(block))
            .collect::<Vec<_>>();
        sidecar_for_epoch_at_with_data_crcs(
            scheme,
            epoch_id,
            protected_ordinal_start,
            object_blocks,
            data_crcs,
        )
    }

    fn sidecar_for_epoch_at_with_data_crcs(
        scheme: &ParityScheme,
        epoch_id: u64,
        protected_ordinal_start: u64,
        object_blocks: &[Vec<u8>],
        data_crcs: Vec<u64>,
    ) -> crate::sidecar::EncodedSidecarTapeFile {
        let logical_shards = usize::try_from(data_shards_per_epoch(scheme).unwrap()).unwrap();
        assert!(
            !object_blocks.is_empty(),
            "test sidecar helper requires at least one real data shard"
        );
        assert!(
            object_blocks.len() <= logical_shards,
            "test sidecar helper cannot protect more real shards than one epoch"
        );
        assert_eq!(
            data_crcs.len(),
            object_blocks.len(),
            "test sidecar helper needs one data CRC per real shard"
        );
        let codec = ReedSolomonCodec::new(scheme).unwrap();
        let zero_block = vec![0u8; BLOCK_SIZE as usize];
        let mut parity_shards = Vec::new();
        for stripe in 0..scheme.stripes_per_neighborhood as usize {
            let mut data = Vec::new();
            for row in 0..scheme.data_blocks_per_stripe as usize {
                let ordinal = row * scheme.stripes_per_neighborhood as usize + stripe;
                data.push(
                    object_blocks
                        .get(ordinal)
                        .cloned()
                        .unwrap_or_else(|| zero_block.clone()),
                );
            }
            parity_shards.extend(codec.encode(&data).unwrap());
        }

        let protected_ordinal_end_exclusive = protected_ordinal_start
            .checked_add(object_blocks.len() as u64)
            .unwrap();
        let descriptor = SidecarDescriptor {
            tape_uuid: TAPE_UUID,
            epoch_id,
            k: scheme.data_blocks_per_stripe,
            m: scheme.parity_blocks_per_stripe,
            stripes_per_epoch: scheme.stripes_per_neighborhood,
            block_size: BLOCK_SIZE,
            protected_ordinal_start,
            protected_ordinal_end_exclusive,
        };
        encode_sidecar_tape_file(&descriptor, &parity_shards, data_crcs).unwrap()
    }

    fn scoped_map(sidecar_blocks: u64, object_blocks: u64) -> ScopedFilemarkMap {
        let map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, object_blocks, 0),
            TapeFileMapEntry::parity_sidecar(2, sidecar_blocks, 0, 0, object_blocks),
        ])
        .unwrap();
        ScopedFilemarkMap::from_catalog(map, object_blocks)
    }

    fn scoped_two_object_map(
        sidecar_blocks: u64,
        first_object_blocks: u64,
        second_object_blocks: u64,
        highest_protected_ordinal: u64,
    ) -> ScopedFilemarkMap {
        let map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, first_object_blocks, 0),
            TapeFileMapEntry::object(2, second_object_blocks, first_object_blocks),
            TapeFileMapEntry::parity_sidecar(3, sidecar_blocks, 0, 0, highest_protected_ordinal),
        ])
        .unwrap();
        ScopedFilemarkMap::from_catalog(map, highest_protected_ordinal)
    }

    fn scoped_two_epoch_sidecar_map(
        first_sidecar_blocks: u64,
        second_sidecar_blocks: u64,
        object_blocks: u64,
        epoch_data_shards: u64,
    ) -> ScopedFilemarkMap {
        let map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, object_blocks, 0),
            TapeFileMapEntry::parity_sidecar(2, first_sidecar_blocks, 0, 0, epoch_data_shards),
            TapeFileMapEntry::parity_sidecar(
                3,
                second_sidecar_blocks,
                1,
                epoch_data_shards,
                object_blocks,
            ),
        ])
        .unwrap();
        ScopedFilemarkMap::from_catalog(map, object_blocks)
    }

    fn scoped_two_object_two_epoch_sidecar_map(
        first_sidecar_blocks: u64,
        second_sidecar_blocks: u64,
        epoch_data_shards: u64,
    ) -> ScopedFilemarkMap {
        let total_object_blocks = epoch_data_shards.checked_mul(2).unwrap();
        let map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, epoch_data_shards, 0),
            TapeFileMapEntry::parity_sidecar(2, first_sidecar_blocks, 0, 0, epoch_data_shards),
            TapeFileMapEntry::object(3, epoch_data_shards, epoch_data_shards),
            TapeFileMapEntry::parity_sidecar(
                4,
                second_sidecar_blocks,
                1,
                epoch_data_shards,
                total_object_blocks,
            ),
        ])
        .unwrap();
        ScopedFilemarkMap::from_catalog(map, total_object_blocks)
    }

    fn scoped_two_object_full_then_partial_sidecar_map(
        first_sidecar_blocks: u64,
        partial_sidecar_blocks: u64,
        epoch_data_shards: u64,
        partial_real_data_shards: u64,
    ) -> ScopedFilemarkMap {
        let total_object_blocks = epoch_data_shards
            .checked_add(partial_real_data_shards)
            .unwrap();
        let map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, epoch_data_shards, 0),
            TapeFileMapEntry::parity_sidecar(2, first_sidecar_blocks, 0, 0, epoch_data_shards),
            TapeFileMapEntry::object(3, partial_real_data_shards, epoch_data_shards),
            TapeFileMapEntry::parity_sidecar(
                4,
                partial_sidecar_blocks,
                1,
                epoch_data_shards,
                total_object_blocks,
            ),
        ])
        .unwrap();
        ScopedFilemarkMap::from_catalog(map, total_object_blocks)
    }

    fn scoped_three_object_three_epoch_sidecar_map(
        first_sidecar_blocks: u64,
        second_sidecar_blocks: u64,
        third_sidecar_blocks: u64,
        epoch_data_shards: u64,
    ) -> ScopedFilemarkMap {
        let second_epoch_start = epoch_data_shards;
        let third_epoch_start = epoch_data_shards.checked_mul(2).unwrap();
        let total_object_blocks = epoch_data_shards.checked_mul(3).unwrap();
        let map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, epoch_data_shards, 0),
            TapeFileMapEntry::parity_sidecar(2, first_sidecar_blocks, 0, 0, epoch_data_shards),
            TapeFileMapEntry::object(3, epoch_data_shards, second_epoch_start),
            TapeFileMapEntry::parity_sidecar(
                4,
                second_sidecar_blocks,
                1,
                second_epoch_start,
                third_epoch_start,
            ),
            TapeFileMapEntry::object(5, epoch_data_shards, third_epoch_start),
            TapeFileMapEntry::parity_sidecar(
                6,
                third_sidecar_blocks,
                2,
                third_epoch_start,
                total_object_blocks,
            ),
            TapeFileMapEntry::bootstrap(7, 1),
        ])
        .unwrap();
        ScopedFilemarkMap::from_catalog(map, total_object_blocks)
    }

    fn scoped_partial_object_map(
        sidecar_blocks: u64,
        object_blocks: u64,
        highest_protected_ordinal: u64,
    ) -> ScopedFilemarkMap {
        let map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, object_blocks, 0),
            TapeFileMapEntry::parity_sidecar(2, sidecar_blocks, 0, 0, highest_protected_ordinal),
        ])
        .unwrap();
        ScopedFilemarkMap::from_catalog(map, highest_protected_ordinal)
    }

    fn raw_tape(object_blocks: &[Vec<u8>], sidecar_blocks: &[Vec<u8>]) -> RawVec {
        let mut records = Vec::new();
        records.push(Record::Block(vec![0xB0; BLOCK_SIZE as usize]));
        records.push(Record::Filemark);
        for block in object_blocks {
            records.push(Record::Block(block.clone()));
        }
        records.push(Record::Filemark);
        for block in sidecar_blocks {
            records.push(Record::Block(block.clone()));
        }
        records.push(Record::Filemark);
        RawVec::new(records)
    }

    fn raw_tape_two_objects(
        first_object_blocks: &[Vec<u8>],
        second_object_blocks: &[Vec<u8>],
        sidecar_blocks: &[Vec<u8>],
    ) -> RawVec {
        let mut records = Vec::new();
        records.push(Record::Block(vec![0xB0; BLOCK_SIZE as usize]));
        records.push(Record::Filemark);
        for block in first_object_blocks {
            records.push(Record::Block(block.clone()));
        }
        records.push(Record::Filemark);
        for block in second_object_blocks {
            records.push(Record::Block(block.clone()));
        }
        records.push(Record::Filemark);
        for block in sidecar_blocks {
            records.push(Record::Block(block.clone()));
        }
        records.push(Record::Filemark);
        RawVec::new(records)
    }

    fn records_for_object_sidecar_then_object(
        first_object_blocks: &[Vec<u8>],
        first_sidecar_blocks: &[Vec<u8>],
        second_object_blocks: &[Vec<u8>],
    ) -> Vec<Record> {
        let mut records = Vec::new();
        records.push(Record::Block(vec![0xB0; BLOCK_SIZE as usize]));
        records.push(Record::Filemark);
        for block in first_object_blocks {
            records.push(Record::Block(block.clone()));
        }
        records.push(Record::Filemark);
        for block in first_sidecar_blocks {
            records.push(Record::Block(block.clone()));
        }
        records.push(Record::Filemark);
        for block in second_object_blocks {
            records.push(Record::Block(block.clone()));
        }
        records.push(Record::Filemark);
        records
    }

    fn raw_tape_two_object_two_epoch_sidecars(
        first_object_blocks: &[Vec<u8>],
        first_sidecar_blocks: &[Vec<u8>],
        second_object_blocks: &[Vec<u8>],
        second_sidecar_blocks: &[Vec<u8>],
    ) -> RawVec {
        let mut records = Vec::new();
        records.push(Record::Block(vec![0xB0; BLOCK_SIZE as usize]));
        records.push(Record::Filemark);
        for block in first_object_blocks {
            records.push(Record::Block(block.clone()));
        }
        records.push(Record::Filemark);
        for block in first_sidecar_blocks {
            records.push(Record::Block(block.clone()));
        }
        records.push(Record::Filemark);
        for block in second_object_blocks {
            records.push(Record::Block(block.clone()));
        }
        records.push(Record::Filemark);
        for block in second_sidecar_blocks {
            records.push(Record::Block(block.clone()));
        }
        records.push(Record::Filemark);
        RawVec::new(records)
    }

    fn raw_tape_three_object_three_epoch_sidecars(
        first_object_blocks: &[Vec<u8>],
        first_sidecar_blocks: &[Vec<u8>],
        second_object_blocks: &[Vec<u8>],
        second_sidecar_blocks: &[Vec<u8>],
        third_object_blocks: &[Vec<u8>],
        third_sidecar_blocks: &[Vec<u8>],
    ) -> RawVec {
        let mut records = Vec::new();
        records.push(Record::Block(vec![0xB0; BLOCK_SIZE as usize]));
        records.push(Record::Filemark);
        for block in first_object_blocks {
            records.push(Record::Block(block.clone()));
        }
        records.push(Record::Filemark);
        for block in first_sidecar_blocks {
            records.push(Record::Block(block.clone()));
        }
        records.push(Record::Filemark);
        for block in second_object_blocks {
            records.push(Record::Block(block.clone()));
        }
        records.push(Record::Filemark);
        for block in second_sidecar_blocks {
            records.push(Record::Block(block.clone()));
        }
        records.push(Record::Filemark);
        for block in third_object_blocks {
            records.push(Record::Block(block.clone()));
        }
        records.push(Record::Filemark);
        for block in third_sidecar_blocks {
            records.push(Record::Block(block.clone()));
        }
        records.push(Record::Filemark);
        records.push(Record::Block(vec![0xBF; BLOCK_SIZE as usize]));
        records.push(Record::Filemark);
        RawVec::new(records)
    }

    fn raw_tape_two_epoch_sidecars(
        object_blocks: &[Vec<u8>],
        first_sidecar_blocks: &[Vec<u8>],
        second_sidecar_blocks: &[Vec<u8>],
    ) -> RawVec {
        let mut records = Vec::new();
        records.push(Record::Block(vec![0xB0; BLOCK_SIZE as usize]));
        records.push(Record::Filemark);
        for block in object_blocks {
            records.push(Record::Block(block.clone()));
        }
        records.push(Record::Filemark);
        for block in first_sidecar_blocks {
            records.push(Record::Block(block.clone()));
        }
        records.push(Record::Filemark);
        for block in second_sidecar_blocks {
            records.push(Record::Block(block.clone()));
        }
        records.push(Record::Filemark);
        RawVec::new(records)
    }

    fn object_damage_lbas(
        scoped: &ScopedFilemarkMap,
        tape_file_number: u32,
        start_body_lba: u64,
        len: u64,
    ) -> Vec<usize> {
        (start_body_lba..start_body_lba + len)
            .map(|body_lba| {
                let physical = scoped
                    .map
                    .physical_position(TapeFilePosition {
                        tape_file_number,
                        block_within_file: body_lba,
                    })
                    .unwrap();
                usize::try_from(physical.lba).unwrap()
            })
            .collect()
    }

    fn object_lbas_for_ordinals(scoped: &ScopedFilemarkMap, ordinals: &[u64]) -> Vec<usize> {
        ordinals
            .iter()
            .map(|ordinal| {
                scoped
                    .map
                    .position_for_ordinal(*ordinal)
                    .and_then(|position| scoped.map.physical_position(position))
                    .map(|physical| usize::try_from(physical.lba).unwrap())
                    .unwrap()
            })
            .collect()
    }

    fn parity_lba_for_shard(
        scoped: &ScopedFilemarkMap,
        sidecar_tape_file_number: u32,
        sidecar: &crate::sidecar::EncodedSidecarTapeFile,
        stripe_index: u32,
        parity_index: u16,
    ) -> usize {
        let (entry_index, _) = sidecar
            .index
            .parity_entries
            .iter()
            .enumerate()
            .find(|(_, entry)| {
                entry.stripe_index == stripe_index && entry.parity_index == parity_index
            })
            .unwrap_or_else(|| {
                panic!(
                    "sidecar must contain parity shard stripe {stripe_index} parity {parity_index}"
                )
            });
        let block_within_file =
            u64::from(sidecar.header.shard_index_block_count) + u64::try_from(entry_index).unwrap();
        let physical = scoped
            .map
            .physical_position(TapeFilePosition {
                tape_file_number: sidecar_tape_file_number,
                block_within_file,
            })
            .unwrap();
        usize::try_from(physical.lba).unwrap()
    }

    #[test]
    fn object_block_recovery_maps_body_lba_across_filemarks() {
        let scheme = scheme(2, 1, 2);
        let object_blocks = vec![block(1), block(2), block(3), block(4)];
        let sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let scoped = scoped_two_object_map(sidecar.blocks.len() as u64, 2, 2, 4);
        let mut raw =
            raw_tape_two_objects(&object_blocks[..2], &object_blocks[2..], &sidecar.blocks);
        raw.unreadable_lbas.push(6); // tape_file 2, body_lba 1.

        let recovered = recover_object_block_from_sidecar(
            &mut raw, &scoped, &scheme, TAPE_UUID, BLOCK_SIZE, 2, 1,
        )
        .expect("object-addressed sidecar recovery succeeds");

        assert_eq!(recovered.failed_ordinal, 3);
        assert_eq!(recovered.recovered_block, object_blocks[3]);
        assert_eq!(recovered.sidecar_tape_file_number, 3);
        assert_eq!(raw.position().unwrap(), PhysicalPositionHint::new(7));
    }

    #[test]
    fn damage_spanning_object_filemark_recovers_adjacent_object_blocks() {
        let scheme = scheme(2, 1, 2);
        let object_blocks = vec![block(1), block(2), block(3), block(4)];
        let sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let scoped = scoped_two_object_map(sidecar.blocks.len() as u64, 2, 2, 4);
        let first_tail_lba = scoped
            .map
            .physical_position(TapeFilePosition {
                tape_file_number: 1,
                block_within_file: 1,
            })
            .unwrap()
            .lba;
        let second_head_lba = scoped
            .map
            .physical_position(TapeFilePosition {
                tape_file_number: 2,
                block_within_file: 0,
            })
            .unwrap()
            .lba;
        let damaged_lbas = vec![
            usize::try_from(first_tail_lba).unwrap(),
            usize::try_from(first_tail_lba + 1).unwrap(), // object-1 trailing filemark
            usize::try_from(second_head_lba).unwrap(),
        ];
        assert_eq!(damaged_lbas, vec![3, 4, 5]);

        for (tape_file_number, body_lba, expected_ordinal, expected_lost) in [
            (1, 1, 1, StripePosition::Data { index: 0 }),
            (2, 0, 2, StripePosition::Data { index: 1 }),
        ] {
            let mut raw =
                raw_tape_two_objects(&object_blocks[..2], &object_blocks[2..], &sidecar.blocks);
            raw.unreadable_lbas = damaged_lbas.clone();

            let recovered = recover_object_block_from_sidecar(
                &mut raw,
                &scoped,
                &scheme,
                TAPE_UUID,
                BLOCK_SIZE,
                tape_file_number,
                body_lba,
            )
            .unwrap_or_else(|err| {
                panic!(
                    "ordinal {expected_ordinal} should recover across object filemark damage: {err}"
                )
            });

            assert_eq!(recovered.failed_ordinal, expected_ordinal);
            assert_eq!(
                recovered.recovered_block, object_blocks[expected_ordinal as usize],
                "ordinal {expected_ordinal} recovered bytes"
            );
            assert_eq!(recovered.lost_shards, vec![expected_lost]);
        }
    }

    #[test]
    fn damage_spanning_object_into_primary_sidecar_header_uses_tail_copy() {
        let scheme = scheme(2, 1, 2);
        let object_blocks = vec![block(1), block(2), block(3), block(4)];
        let sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let object_tail_lba = scoped
            .map
            .physical_position(TapeFilePosition {
                tape_file_number: 1,
                block_within_file: 3,
            })
            .unwrap()
            .lba;
        let sidecar_header_lba = scoped
            .map
            .physical_position(TapeFilePosition {
                tape_file_number: 2,
                block_within_file: 0,
            })
            .unwrap()
            .lba;
        let damaged_lbas = vec![
            usize::try_from(object_tail_lba).unwrap(),
            usize::try_from(object_tail_lba + 1).unwrap(), // object trailing filemark
            usize::try_from(sidecar_header_lba).unwrap(),
        ];
        assert_eq!(damaged_lbas, vec![5, 6, 7]);
        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
        raw.unreadable_lbas = damaged_lbas;

        let recovered =
            recover_ordinal_from_sidecar(&mut raw, &scoped, &scheme, TAPE_UUID, BLOCK_SIZE, 3)
                .expect("tail sidecar metadata copy should survive primary header damage");
        assert_eq!(recovered.recovered_block, object_blocks[3]);
    }

    #[test]
    fn sidecar_body_damage_past_header_is_counted_as_missing_parity() {
        let scheme = scheme(3, 3, 1);
        let object_blocks = vec![block(51), block(52), block(53)];
        let sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let first_parity_block = u64::from(sidecar.header.shard_index_block_count);
        assert_eq!(
            sidecar.header.parity_block_count,
            u32::from(scheme.parity_blocks_per_stripe)
        );

        let header_lba = scoped
            .map
            .physical_position(TapeFilePosition {
                tape_file_number: 2,
                block_within_file: 0,
            })
            .unwrap()
            .lba;
        let damaged_lbas = (0..sidecar.header.parity_block_count)
            .map(|offset| {
                scoped
                    .map
                    .physical_position(TapeFilePosition {
                        tape_file_number: 2,
                        block_within_file: first_parity_block + u64::from(offset),
                    })
                    .unwrap()
                    .lba
            })
            .map(|lba| usize::try_from(lba).unwrap())
            .collect::<Vec<_>>();
        assert!(
            damaged_lbas[0] as u64 > header_lba,
            "damage must leave sidecar header and index readable"
        );
        assert!(
            damaged_lbas.windows(2).all(|pair| pair[1] == pair[0] + 1),
            "sidecar parity-shard body damage should be physically contiguous"
        );

        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
        raw.unreadable_lbas = damaged_lbas;

        let err =
            recover_ordinal_from_sidecar(&mut raw, &scoped, &scheme, TAPE_UUID, BLOCK_SIZE, 2)
                .expect_err("all parity-shard body blocks are unavailable");

        match err {
            ParityError::Unrecoverable {
                lost_count, limit, ..
            } => {
                assert_eq!(lost_count, scheme.parity_blocks_per_stripe + 1);
                assert_eq!(limit, scheme.parity_blocks_per_stripe);
            }
            other => panic!("expected missing parity shards to be unrecoverable, got {other:?}"),
        }
    }

    #[test]
    fn sidecar_index_damage_after_header_is_metadata_unavailable() {
        let scheme = scheme(3, 3, 1);
        let object_blocks = vec![block(61), block(62), block(63)];
        let sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        assert!(
            sidecar.header.shard_index_block_count > 1,
            "fixture must place sidecar index bytes in a post-header block"
        );

        let header_lba = scoped
            .map
            .physical_position(TapeFilePosition {
                tape_file_number: 2,
                block_within_file: 0,
            })
            .unwrap()
            .lba;
        let index_lba = scoped
            .map
            .physical_position(TapeFilePosition {
                tape_file_number: 2,
                block_within_file: 1,
            })
            .unwrap()
            .lba;
        let first_parity_lba = scoped
            .map
            .physical_position(TapeFilePosition {
                tape_file_number: 2,
                block_within_file: u64::from(sidecar.header.shard_index_block_count),
            })
            .unwrap()
            .lba;
        assert_eq!(index_lba, header_lba + 1);
        assert!(
            index_lba < first_parity_lba,
            "damage must target the sidecar index, not the parity-shard body"
        );

        let tail_index_lba = scoped
            .map
            .physical_position(TapeFilePosition {
                tape_file_number: 2,
                block_within_file: sidecar.header.tail_header_start_block + 1,
            })
            .unwrap()
            .lba;
        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
        raw.unreadable_lbas = vec![
            usize::try_from(index_lba).unwrap(),
            usize::try_from(tail_index_lba).unwrap(),
        ];

        let err =
            recover_ordinal_from_sidecar(&mut raw, &scoped, &scheme, TAPE_UUID, BLOCK_SIZE, 2)
                .expect_err(
                    "unreadable primary and tail sidecar index blocks should make the epoch opaque",
                );

        assert!(matches!(
            err,
            ParityError::SidecarMetadataUnavailable { epoch_id: 0 }
        ));
    }

    #[test]
    fn sidecar_later_index_spill_damage_is_metadata_unavailable() {
        let scheme = scheme(8, 4, 8);
        let object_blocks = (0..64)
            .map(|idx| block((idx + 71) as u8))
            .collect::<Vec<_>>();
        let sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        assert!(
            sidecar.header.shard_index_block_count > 2,
            "fixture must spill the sidecar index across multiple post-header blocks"
        );

        let final_index_block = u64::from(sidecar.header.shard_index_block_count - 1);
        let final_index_lba = scoped
            .map
            .physical_position(TapeFilePosition {
                tape_file_number: 2,
                block_within_file: final_index_block,
            })
            .unwrap()
            .lba;
        let first_parity_lba = scoped
            .map
            .physical_position(TapeFilePosition {
                tape_file_number: 2,
                block_within_file: u64::from(sidecar.header.shard_index_block_count),
            })
            .unwrap()
            .lba;
        assert!(
            final_index_lba < first_parity_lba,
            "damage must hit a spill index block before the parity-shard body"
        );

        let tail_final_index_lba = scoped
            .map
            .physical_position(TapeFilePosition {
                tape_file_number: 2,
                block_within_file: sidecar.header.tail_header_start_block + final_index_block,
            })
            .unwrap()
            .lba;
        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
        raw.unreadable_lbas = vec![
            usize::try_from(final_index_lba).unwrap(),
            usize::try_from(tail_final_index_lba).unwrap(),
        ];

        let err = recover_ordinal_from_sidecar(
            &mut raw, &scoped, &scheme, TAPE_UUID, BLOCK_SIZE, 9,
        )
        .expect_err(
            "unreadable primary and tail later sidecar index blocks should make the epoch opaque",
        );

        assert!(matches!(
            err,
            ParityError::SidecarMetadataUnavailable { epoch_id: 0 }
        ));
    }

    #[test]
    fn sidecar_primary_tail_and_footer_damage_is_metadata_unavailable() {
        let scheme = scheme(2, 1, 2);
        let object_blocks = vec![block(1), block(2), block(3), block(4)];
        let sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let mut sidecar_blocks = sidecar.blocks.clone();
        sidecar_blocks[0][0] ^= 0xFF;
        let tail_start = usize::try_from(sidecar.header.tail_header_start_block).unwrap();
        sidecar_blocks[tail_start][0] ^= 0xFF;
        let footer_index = sidecar_blocks
            .len()
            .checked_sub(1)
            .expect("sidecar has a footer block");
        sidecar_blocks[footer_index][0] ^= 0xFF;

        let scoped = scoped_map(sidecar_blocks.len() as u64, object_blocks.len() as u64);
        let mut raw = raw_tape(&object_blocks, &sidecar_blocks);
        let err =
            recover_ordinal_from_sidecar(&mut raw, &scoped, &scheme, TAPE_UUID, BLOCK_SIZE, 2)
                .expect_err(
                    "map-valid sidecar with all local metadata copies damaged is epoch-unavailable",
                );

        assert!(matches!(
            err,
            ParityError::SidecarMetadataUnavailable { epoch_id: 0 }
        ));
    }

    #[test]
    fn sidecar_footer_damage_falls_back_to_primary_header_copy() {
        let scheme = scheme(2, 1, 2);
        let object_blocks = vec![block(1), block(2), block(3), block(4)];
        let sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let mut sidecar_blocks = sidecar.blocks.clone();
        let footer_index = sidecar_blocks
            .len()
            .checked_sub(1)
            .expect("sidecar has a footer block");
        sidecar_blocks[footer_index][0] ^= 0xFF;

        let scoped = scoped_map(sidecar_blocks.len() as u64, object_blocks.len() as u64);
        let mut raw = raw_tape(&object_blocks, &sidecar_blocks);
        let recovered =
            recover_ordinal_from_sidecar(&mut raw, &scoped, &scheme, TAPE_UUID, BLOCK_SIZE, 2)
                .expect("footer loss with intact primary metadata remains recoverable");

        assert_eq!(recovered.recovered_block, object_blocks[2]);
        assert_eq!(
            recovered.sidecar_metadata_health,
            SidecarMetadataHealth::TailCopyLost
        );
    }

    #[test]
    fn object_block_recovery_uses_per_block_watermark_for_partial_object() {
        let scheme = scheme(2, 1, 1);
        let object_blocks = vec![block(1), block(2), block(3), block(4)];
        let protected_blocks = object_blocks[..2].to_vec();
        let sidecar = sidecar_for_epoch(&scheme, &protected_blocks);
        let scoped = scoped_partial_object_map(sidecar.blocks.len() as u64, 4, 2);
        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);

        let recovered = recover_object_block_from_sidecar(
            &mut raw, &scoped, &scheme, TAPE_UUID, BLOCK_SIZE, 1, 1,
        )
        .expect("early block below watermark recovers");

        assert_eq!(recovered.failed_ordinal, 1);
        assert_eq!(recovered.recovered_block, object_blocks[1]);

        let mut tail_raw = raw_tape(&object_blocks, &sidecar.blocks);
        let err = recover_object_block_from_sidecar(
            &mut tail_raw,
            &scoped,
            &scheme,
            TAPE_UUID,
            BLOCK_SIZE,
            1,
            2,
        )
        .expect_err("tail block at watermark has no sidecar yet");
        assert!(matches!(
            err,
            ParityError::UnrecoverablePendingEpoch {
                failed_ordinal: 2,
                watermark: 2
            }
        ));
        assert_eq!(tail_raw.configured_block_size, None);
        assert_eq!(tail_raw.position().unwrap(), PhysicalPositionHint::new(0));
    }

    #[test]
    fn object_committed_without_sidecar_is_pending_epoch_before_tape_io() {
        let scheme = scheme(2, 1, 2);
        let first_object_blocks = vec![block(1), block(2), block(3), block(4)];
        let second_object_blocks = 2;
        let sidecar = sidecar_for_epoch(&scheme, &first_object_blocks);
        let watermark = first_object_blocks.len() as u64;
        let map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, watermark, 0),
            TapeFileMapEntry::parity_sidecar(2, sidecar.blocks.len() as u64, 0, 0, watermark),
            TapeFileMapEntry::object(3, second_object_blocks, watermark),
        ])
        .expect("object-without-sidecar catalog state validates");
        let scoped = ScopedFilemarkMap::from_catalog(map, watermark);
        assert_eq!(
            scoped.map.position_for_ordinal(watermark + 1).unwrap(),
            TapeFilePosition {
                tape_file_number: 3,
                block_within_file: 1,
            }
        );
        let mut raw = RawVec::new(Vec::new());

        let err = recover_object_block_from_sidecar(
            &mut raw, &scoped, &scheme, TAPE_UUID, BLOCK_SIZE, 3, 1,
        )
        .expect_err("object committed without a sidecar remains unprotected");

        assert!(matches!(
            err,
            ParityError::UnrecoverablePendingEpoch {
                failed_ordinal: 5,
                watermark: 4
            }
        ));
        assert_eq!(raw.configured_block_size, None);
        assert_eq!(raw.position().unwrap(), PhysicalPositionHint::new(0));
    }

    #[test]
    fn resume_committed_sidecar_makes_object_without_sidecar_recoverable() {
        let scheme = scheme(2, 1, 2);
        let first_object_blocks = vec![block(1), block(2), block(3), block(4)];
        let second_object_blocks = vec![block(11), block(12), block(13), block(14)];
        let first_sidecar = sidecar_for_epoch(&scheme, &first_object_blocks);
        let first_watermark = first_object_blocks.len() as u64;
        let second_object_start = first_watermark;
        let second_object_end = second_object_start + second_object_blocks.len() as u64;
        assert_eq!(
            second_object_end - first_watermark,
            data_shards_per_epoch(&scheme).unwrap(),
            "the committed object tail must be one full rebuildable epoch"
        );
        let committed_map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, first_watermark, 0),
            TapeFileMapEntry::parity_sidecar(
                2,
                first_sidecar.blocks.len() as u64,
                0,
                0,
                first_watermark,
            ),
            TapeFileMapEntry::object(3, second_object_blocks.len() as u64, second_object_start),
        ])
        .expect("object-without-sidecar catalog state validates");
        let pending_scoped =
            ScopedFilemarkMap::from_catalog(committed_map.clone(), first_watermark);
        let failed_body_lba = 1;
        let failed_ordinal = second_object_start + failed_body_lba;
        let mut pending_raw = RawVec::new(Vec::new());

        let pending = recover_object_block_from_sidecar(
            &mut pending_raw,
            &pending_scoped,
            &scheme,
            TAPE_UUID,
            BLOCK_SIZE,
            3,
            failed_body_lba,
        )
        .expect_err("object committed without sidecar is pending before resume");

        assert!(matches!(
            pending,
            ParityError::UnrecoverablePendingEpoch {
                failed_ordinal: 5,
                watermark: 4
            }
        ));
        assert_eq!(failed_ordinal, 5);
        assert_eq!(pending_raw.configured_block_size, None);
        assert_eq!(
            pending_raw.position().unwrap(),
            PhysicalPositionHint::new(0),
            "pending epoch gate must fire before tape I/O"
        );

        let committed_records = records_for_object_sidecar_then_object(
            &first_object_blocks,
            &first_sidecar.blocks,
            &second_object_blocks,
        );
        let mut rebuild_source = RawVec::new(committed_records.clone());
        let rebuild = rebuild_legacy_forensic_open_epoch_from_committed_prefix(
            &mut rebuild_source,
            &committed_map,
            &scheme,
            TAPE_UUID,
            BLOCK_SIZE,
        )
        .expect("resume rebuild rereads the committed object tail");
        assert_eq!(rebuild.plan.append_after_tape_file_number, 3);
        assert_eq!(
            rebuild.plan.highest_protected_ordinal_before_rebuild,
            first_watermark
        );
        assert_eq!(
            rebuild.plan.highest_protected_ordinal_after_rebuild,
            second_object_end
        );
        assert_eq!(rebuild.rebuilt_sidecars.len(), 1);
        assert!(rebuild.live_epoch.is_none());

        let mut append_sink =
            AppendRawVecSink::at_append_position(committed_records, rebuild.plan.append_position);
        let mut commit_events = Vec::new();
        let resume_result = emit_resume_rebuilt_sidecars_to_raw_without_journal(
            &mut append_sink,
            rebuild.plan,
            &rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |sidecar| {
                commit_events.push((
                    sidecar.tape_file_number,
                    sidecar.filemark_outcome.position_after.lba,
                ));
                Ok(())
            },
        )
        .expect("resume-generated sidecar commits through the ordinary sidecar path");

        assert_eq!(resume_result.sidecars_emitted.len(), 1);
        let rebuilt_sidecar = &resume_result.sidecars_emitted[0];
        assert_eq!(rebuilt_sidecar.tape_file_number, 4);
        assert_eq!(rebuilt_sidecar.epoch_id, 1);
        assert_eq!(rebuilt_sidecar.protected_ordinal_start, second_object_start);
        assert_eq!(
            rebuilt_sidecar.protected_ordinal_end_exclusive,
            second_object_end
        );
        assert_eq!(resume_result.highest_protected_ordinal, second_object_end);
        assert_eq!(
            commit_events,
            vec![(
                rebuilt_sidecar.tape_file_number,
                rebuilt_sidecar.filemark_outcome.position_after.lba
            )],
            "catalog commit callback fires after the resume sidecar filemark barrier"
        );

        let recovered_map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, first_watermark, 0),
            TapeFileMapEntry::parity_sidecar(
                2,
                first_sidecar.blocks.len() as u64,
                0,
                0,
                first_watermark,
            ),
            TapeFileMapEntry::object(3, second_object_blocks.len() as u64, second_object_start),
            TapeFileMapEntry::parity_sidecar(
                rebuilt_sidecar.tape_file_number,
                rebuilt_sidecar.block_count,
                rebuilt_sidecar.epoch_id,
                rebuilt_sidecar.protected_ordinal_start,
                rebuilt_sidecar.protected_ordinal_end_exclusive,
            ),
        ])
        .expect("resume-committed sidecar extends the committed prefix");
        let recovered_scoped =
            ScopedFilemarkMap::from_catalog(recovered_map, resume_result.highest_protected_ordinal);
        let damaged_lba = recovered_scoped
            .map
            .physical_position(TapeFilePosition {
                tape_file_number: 3,
                block_within_file: failed_body_lba,
            })
            .unwrap()
            .lba;
        let mut recovery_raw = append_sink.into_source();
        recovery_raw.unreadable_lbas = vec![usize::try_from(damaged_lba).unwrap()];

        let recovered = recover_object_block_from_sidecar(
            &mut recovery_raw,
            &recovered_scoped,
            &scheme,
            TAPE_UUID,
            BLOCK_SIZE,
            3,
            failed_body_lba,
        )
        .expect("same object block recovers after the resume sidecar commits");

        assert_eq!(recovered.failed_ordinal, failed_ordinal);
        assert_eq!(
            recovered.recovered_block,
            second_object_blocks[failed_body_lba as usize]
        );
        assert_eq!(recovered.sidecar_tape_file_number, 4);
        assert_eq!(
            recovered.lost_shards,
            vec![StripePosition::Data { index: 0 }]
        );
        assert_eq!(
            recovery_raw.configured_block_size,
            Some(BLOCK_SIZE),
            "successful post-resume recovery performs tape I/O"
        );
    }

    #[test]
    fn final_partial_epoch_recovery_supplies_implicit_zero_peers() {
        let scheme = scheme(4, 3, 5);
        let epoch_data_shards = data_shards_per_epoch(&scheme).unwrap();
        let real_data_shards = 7u64;
        let object_blocks = (0..usize::try_from(real_data_shards).unwrap())
            .map(|idx| block((idx + 81) as u8))
            .collect::<Vec<_>>();
        let sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);

        assert_eq!(epoch_data_shards, 20);
        assert_eq!(sidecar.header.logical_shard_count, epoch_data_shards);
        assert_eq!(sidecar.header.real_data_shard_count, real_data_shards);
        assert_eq!(sidecar.header.data_crc_count, real_data_shards as u32);
        assert_eq!(
            sidecar.header.protected_ordinal_end_exclusive,
            real_data_shards
        );

        let failed_ordinal = 4;
        let failed_stripe = ordinal_to_stripe(failed_ordinal, &scheme).unwrap();
        assert_eq!(failed_stripe.stripe_index, 4);
        assert_eq!(failed_stripe.position, StripePosition::Data { index: 0 });
        for implicit_ordinal in [9, 14, 19] {
            let implicit = ordinal_to_stripe(implicit_ordinal, &scheme).unwrap();
            assert_eq!(implicit.neighborhood, failed_stripe.neighborhood);
            assert_eq!(implicit.stripe_index, failed_stripe.stripe_index);
            assert!(
                implicit_ordinal >= real_data_shards,
                "fixture must make stripe 4's non-failed peers implicit zeros"
            );
        }

        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
        raw.unreadable_lbas = object_lbas_for_ordinals(&scoped, &[failed_ordinal]);

        let recovered = recover_ordinal_from_sidecar(
            &mut raw,
            &scoped,
            &scheme,
            TAPE_UUID,
            BLOCK_SIZE,
            failed_ordinal,
        )
        .expect("final partial epoch real shard recovers using implicit zero peers");

        assert_eq!(
            recovered.recovered_block,
            object_blocks[usize::try_from(failed_ordinal).unwrap()]
        );
        assert_eq!(
            recovered.lost_shards,
            vec![StripePosition::Data { index: 0 }]
        );
        assert_eq!(recovered.sidecar_tape_file_number, 2);
    }

    #[test]
    fn final_partial_epoch_recovery_mixes_real_and_implicit_zero_peers() {
        let scheme = scheme(4, 3, 5);
        let epoch_data_shards = data_shards_per_epoch(&scheme).unwrap();
        let real_data_shards = 7u64;
        let object_blocks = (0..usize::try_from(real_data_shards).unwrap())
            .map(|idx| block((idx + 101) as u8))
            .collect::<Vec<_>>();
        let sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);

        assert_eq!(epoch_data_shards, 20);
        assert_eq!(sidecar.header.logical_shard_count, epoch_data_shards);
        assert_eq!(sidecar.header.real_data_shard_count, real_data_shards);
        assert_eq!(sidecar.header.data_crc_count, real_data_shards as u32);

        let failed_ordinal = 0;
        let real_peer_ordinal = 5;
        let failed_stripe = ordinal_to_stripe(failed_ordinal, &scheme).unwrap();
        let real_peer = ordinal_to_stripe(real_peer_ordinal, &scheme).unwrap();
        assert_eq!(failed_stripe.stripe_index, 0);
        assert_eq!(failed_stripe.position, StripePosition::Data { index: 0 });
        assert_eq!(real_peer.neighborhood, failed_stripe.neighborhood);
        assert_eq!(real_peer.stripe_index, failed_stripe.stripe_index);
        assert_eq!(real_peer.position, StripePosition::Data { index: 1 });
        assert!(
            real_peer_ordinal < real_data_shards,
            "fixture must keep one same-stripe peer as a real on-tape shard"
        );
        for implicit_ordinal in [10, 15] {
            let implicit = ordinal_to_stripe(implicit_ordinal, &scheme).unwrap();
            assert_eq!(implicit.neighborhood, failed_stripe.neighborhood);
            assert_eq!(implicit.stripe_index, failed_stripe.stripe_index);
            assert!(
                implicit_ordinal >= real_data_shards,
                "fixture must keep later same-stripe peers as implicit zeros"
            );
        }

        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
        raw.unreadable_lbas =
            object_lbas_for_ordinals(&scoped, &[failed_ordinal, real_peer_ordinal]);

        let recovered = recover_ordinal_from_sidecar(
            &mut raw,
            &scoped,
            &scheme,
            TAPE_UUID,
            BLOCK_SIZE,
            failed_ordinal,
        )
        .expect("final partial epoch recovers with real and implicit-zero peers");

        assert_eq!(
            recovered.recovered_block,
            object_blocks[usize::try_from(failed_ordinal).unwrap()]
        );
        assert_eq!(
            recovered.lost_shards,
            vec![
                StripePosition::Data { index: 0 },
                StripePosition::Data { index: 1 },
            ]
        );
        assert_eq!(recovered.sidecar_tape_file_number, 2);
    }

    #[test]
    fn final_partial_epoch_recovery_handles_boundary_real_shard_counts() {
        struct Case {
            name: &'static str,
            real_data_shards: u64,
            failed_ordinal: u64,
            damaged_ordinals: Vec<u64>,
            damaged_parity_indices: Vec<u16>,
            expected_lost: Vec<StripePosition>,
            implicit_same_stripe_ordinals: Vec<u64>,
            seed_base: u8,
        }

        let scheme = scheme(4, 3, 5);
        let epoch_data_shards = data_shards_per_epoch(&scheme).unwrap();
        let cases = vec![
            Case {
                name: "single real shard with damaged parity",
                real_data_shards: 1,
                failed_ordinal: 0,
                damaged_ordinals: vec![0],
                damaged_parity_indices: vec![1],
                expected_lost: vec![
                    StripePosition::Data { index: 0 },
                    StripePosition::Parity { index: 1 },
                ],
                implicit_same_stripe_ordinals: vec![5, 10, 15],
                seed_base: 131,
            },
            Case {
                name: "D equals k",
                real_data_shards: u64::from(scheme.data_blocks_per_stripe),
                failed_ordinal: 3,
                damaged_ordinals: vec![3],
                damaged_parity_indices: vec![],
                expected_lost: vec![StripePosition::Data { index: 0 }],
                implicit_same_stripe_ordinals: vec![8, 13, 18],
                seed_base: 151,
            },
            Case {
                name: "single implicit zero at epoch tail",
                real_data_shards: epoch_data_shards - 1,
                failed_ordinal: 14,
                damaged_ordinals: vec![4, 9, 14],
                damaged_parity_indices: vec![],
                expected_lost: vec![
                    StripePosition::Data { index: 0 },
                    StripePosition::Data { index: 1 },
                    StripePosition::Data { index: 2 },
                ],
                implicit_same_stripe_ordinals: vec![19],
                seed_base: 171,
            },
        ];

        for case in cases {
            let object_blocks = (0..usize::try_from(case.real_data_shards).unwrap())
                .map(|idx| block(case.seed_base.wrapping_add(idx as u8)))
                .collect::<Vec<_>>();
            let sidecar = sidecar_for_epoch(&scheme, &object_blocks);
            let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);

            assert_eq!(
                sidecar.header.logical_shard_count, epoch_data_shards,
                "{}",
                case.name
            );
            assert_eq!(
                sidecar.header.real_data_shard_count, case.real_data_shards,
                "{}",
                case.name
            );
            assert_eq!(
                sidecar.header.data_crc_count, case.real_data_shards as u32,
                "{}",
                case.name
            );
            assert!(
                case.failed_ordinal < case.real_data_shards,
                "{}: failed ordinal must be a real on-tape shard",
                case.name
            );
            assert!(
                case.real_data_shards < epoch_data_shards,
                "{}: fixture must be a final partial epoch",
                case.name
            );
            assert!(
                case.damaged_ordinals.contains(&case.failed_ordinal),
                "{}: fixture must include the requested failed shard in object damage",
                case.name
            );

            let failed_stripe = ordinal_to_stripe(case.failed_ordinal, &scheme).unwrap();
            for damaged_ordinal in &case.damaged_ordinals {
                assert!(
                    *damaged_ordinal < case.real_data_shards,
                    "{}: damaged object ordinal must be real",
                    case.name
                );
                let damaged = ordinal_to_stripe(*damaged_ordinal, &scheme).unwrap();
                assert_eq!(
                    damaged.neighborhood, failed_stripe.neighborhood,
                    "{}",
                    case.name
                );
                assert_eq!(
                    damaged.stripe_index, failed_stripe.stripe_index,
                    "{}",
                    case.name
                );
            }
            for implicit_ordinal in &case.implicit_same_stripe_ordinals {
                let implicit = ordinal_to_stripe(*implicit_ordinal, &scheme).unwrap();
                assert_eq!(
                    implicit.neighborhood, failed_stripe.neighborhood,
                    "{}",
                    case.name
                );
                assert_eq!(
                    implicit.stripe_index, failed_stripe.stripe_index,
                    "{}",
                    case.name
                );
                assert!(
                    *implicit_ordinal >= case.real_data_shards,
                    "{}: implicit peer must be outside the real shard range",
                    case.name
                );
            }

            let mut damaged_lbas = object_lbas_for_ordinals(&scoped, &case.damaged_ordinals);
            for parity_index in &case.damaged_parity_indices {
                assert!(
                    *parity_index < scheme.parity_blocks_per_stripe,
                    "{}: parity damage index must be inside the scheme",
                    case.name
                );
                damaged_lbas.push(parity_lba_for_shard(
                    &scoped,
                    2,
                    &sidecar,
                    failed_stripe.stripe_index,
                    *parity_index,
                ));
            }
            let mut unique_damaged_lbas = damaged_lbas.clone();
            unique_damaged_lbas.sort_unstable();
            unique_damaged_lbas.dedup();
            assert_eq!(
                unique_damaged_lbas.len(),
                damaged_lbas.len(),
                "{}: fixture damage must target distinct physical blocks",
                case.name
            );

            let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
            raw.unreadable_lbas = damaged_lbas;

            let recovered = recover_ordinal_from_sidecar(
                &mut raw,
                &scoped,
                &scheme,
                TAPE_UUID,
                BLOCK_SIZE,
                case.failed_ordinal,
            )
            .expect(case.name);

            assert_eq!(
                recovered.recovered_block,
                object_blocks[usize::try_from(case.failed_ordinal).unwrap()],
                "{}",
                case.name
            );
            assert_eq!(recovered.lost_shards, case.expected_lost, "{}", case.name);
            assert_eq!(recovered.sidecar_tape_file_number, 2, "{}", case.name);
        }
    }

    #[test]
    fn final_partial_epoch_after_full_epoch_recovers_with_epoch_local_damage() {
        let scheme = scheme(4, 3, 5);
        let epoch_data_shards = data_shards_per_epoch(&scheme).unwrap();
        let epoch_blocks = usize::try_from(epoch_data_shards).unwrap();
        let final_partial_real_shards = u64::from(scheme.data_blocks_per_stripe) + 1;
        let final_partial_blocks = usize::try_from(final_partial_real_shards).unwrap();
        let object_blocks = (0..epoch_blocks + final_partial_blocks)
            .map(|idx| block((idx + 191) as u8))
            .collect::<Vec<_>>();
        let first_sidecar = sidecar_for_epoch_at(&scheme, 0, 0, &object_blocks[..epoch_blocks]);
        let partial_sidecar = sidecar_for_epoch_at(
            &scheme,
            1,
            epoch_data_shards,
            &object_blocks[epoch_blocks..],
        );
        let scoped = scoped_two_epoch_sidecar_map(
            first_sidecar.blocks.len() as u64,
            partial_sidecar.blocks.len() as u64,
            object_blocks.len() as u64,
            epoch_data_shards,
        );

        assert_eq!(epoch_data_shards, 20);
        assert_eq!(
            final_partial_real_shards,
            u64::from(scheme.data_blocks_per_stripe) + 1,
            "fixture covers the D=k+1 final-partial boundary"
        );
        assert_eq!(partial_sidecar.header.epoch_id, 1);
        assert_eq!(
            partial_sidecar.header.protected_ordinal_start,
            epoch_data_shards
        );
        assert_eq!(
            partial_sidecar.header.protected_ordinal_end_exclusive,
            epoch_data_shards + final_partial_real_shards
        );
        assert_eq!(
            partial_sidecar.header.logical_shard_count,
            epoch_data_shards
        );
        assert_eq!(
            partial_sidecar.header.real_data_shard_count,
            final_partial_real_shards
        );
        assert_eq!(
            partial_sidecar.header.data_crc_count,
            final_partial_real_shards as u32
        );

        let epoch0_failed_ordinal = 5;
        let epoch0_peer_ordinal = 0;
        let partial_failed_ordinal = epoch_data_shards;
        let partial_other_stripe_noise = epoch_data_shards + final_partial_real_shards - 1;
        let epoch0_failed = ordinal_to_stripe(epoch0_failed_ordinal, &scheme).unwrap();
        let epoch0_peer = ordinal_to_stripe(epoch0_peer_ordinal, &scheme).unwrap();
        let partial_failed = ordinal_to_stripe(partial_failed_ordinal, &scheme).unwrap();
        let partial_noise = ordinal_to_stripe(partial_other_stripe_noise, &scheme).unwrap();
        assert_eq!(epoch0_failed.neighborhood, 0);
        assert_eq!(epoch0_failed.stripe_index, epoch0_peer.stripe_index);
        assert_eq!(epoch0_failed.position, StripePosition::Data { index: 1 });
        assert_eq!(epoch0_peer.position, StripePosition::Data { index: 0 });
        assert_eq!(partial_failed.neighborhood, 1);
        assert_eq!(partial_failed.stripe_index, 0);
        assert_eq!(partial_failed.position, StripePosition::Data { index: 0 });
        assert_eq!(partial_noise.neighborhood, 1);
        assert_ne!(
            partial_noise.stripe_index, partial_failed.stripe_index,
            "noise must be in the final partial epoch but outside the failed stripe"
        );
        for implicit_ordinal in [
            partial_failed_ordinal + u64::from(scheme.stripes_per_neighborhood),
            partial_failed_ordinal + 2 * u64::from(scheme.stripes_per_neighborhood),
            partial_failed_ordinal + 3 * u64::from(scheme.stripes_per_neighborhood),
        ] {
            let implicit = ordinal_to_stripe(implicit_ordinal, &scheme).unwrap();
            assert_eq!(implicit.neighborhood, partial_failed.neighborhood);
            assert_eq!(implicit.stripe_index, partial_failed.stripe_index);
            assert!(
                implicit_ordinal >= epoch_data_shards + final_partial_real_shards,
                "same-stripe final-partial peers after D=k+1 must be implicit zeros"
            );
        }

        let damaged_ordinals = vec![
            epoch0_failed_ordinal,
            epoch0_peer_ordinal,
            partial_failed_ordinal,
            partial_other_stripe_noise,
        ];
        let mut damaged_lbas = object_lbas_for_ordinals(&scoped, &damaged_ordinals);
        damaged_lbas.push(parity_lba_for_shard(
            &scoped,
            2,
            &first_sidecar,
            epoch0_failed.stripe_index,
            1,
        ));
        damaged_lbas.push(parity_lba_for_shard(
            &scoped,
            3,
            &partial_sidecar,
            partial_failed.stripe_index,
            2,
        ));
        damaged_lbas.push(parity_lba_for_shard(
            &scoped,
            3,
            &partial_sidecar,
            partial_noise.stripe_index,
            0,
        ));
        let mut unique_damaged_lbas = damaged_lbas.clone();
        unique_damaged_lbas.sort_unstable();
        unique_damaged_lbas.dedup();
        assert_eq!(
            unique_damaged_lbas.len(),
            damaged_lbas.len(),
            "fixture damage must target distinct physical blocks"
        );

        for (failed_ordinal, expected_sidecar_tape_file, expected_lost) in [
            (
                epoch0_failed_ordinal,
                2,
                vec![
                    StripePosition::Data { index: 0 },
                    StripePosition::Data { index: 1 },
                    StripePosition::Parity { index: 1 },
                ],
            ),
            (
                partial_failed_ordinal,
                3,
                vec![
                    StripePosition::Data { index: 0 },
                    StripePosition::Parity { index: 2 },
                ],
            ),
        ] {
            let failed_position = scoped.map.position_for_ordinal(failed_ordinal).unwrap();
            let mut raw = raw_tape_two_epoch_sidecars(
                &object_blocks,
                &first_sidecar.blocks,
                &partial_sidecar.blocks,
            );
            raw.unreadable_lbas = damaged_lbas.clone();

            let recovered = recover_object_block_from_sidecar(
                &mut raw,
                &scoped,
                &scheme,
                TAPE_UUID,
                BLOCK_SIZE,
                failed_position.tape_file_number,
                failed_position.block_within_file,
            )
            .unwrap_or_else(|err| {
                panic!(
                    "full-plus-final-partial epoch damage should recover ordinal {failed_ordinal}: {err}"
                )
            });

            assert_eq!(recovered.failed_ordinal, failed_ordinal);
            assert_eq!(
                recovered.recovered_block, object_blocks[failed_ordinal as usize],
                "ordinal {failed_ordinal} recovered bytes"
            );
            assert_eq!(
                recovered.sidecar_tape_file_number, expected_sidecar_tape_file,
                "ordinal {failed_ordinal} should recover from its own epoch sidecar"
            );
            assert_eq!(
                recovered.lost_shards, expected_lost,
                "ordinal {failed_ordinal} should only count its own epoch stripe damage"
            );
        }
    }

    #[test]
    fn final_partial_epoch_in_second_object_recovers_with_epoch_local_damage() {
        let scheme = scheme(4, 3, 5);
        let epoch_data_shards = data_shards_per_epoch(&scheme).unwrap();
        let epoch_blocks = usize::try_from(epoch_data_shards).unwrap();
        let final_partial_real_shards = u64::from(scheme.data_blocks_per_stripe) + 1;
        let final_partial_blocks = usize::try_from(final_partial_real_shards).unwrap();
        let object_blocks = (0..epoch_blocks + final_partial_blocks)
            .map(|idx| block((idx + 203) as u8))
            .collect::<Vec<_>>();
        let first_object_blocks = &object_blocks[..epoch_blocks];
        let partial_object_blocks = &object_blocks[epoch_blocks..];
        let first_sidecar = sidecar_for_epoch_at(&scheme, 0, 0, first_object_blocks);
        let partial_sidecar =
            sidecar_for_epoch_at(&scheme, 1, epoch_data_shards, partial_object_blocks);
        let scoped = scoped_two_object_full_then_partial_sidecar_map(
            first_sidecar.blocks.len() as u64,
            partial_sidecar.blocks.len() as u64,
            epoch_data_shards,
            final_partial_real_shards,
        );

        assert_eq!(epoch_data_shards, 20);
        assert_eq!(final_partial_real_shards, 5);
        assert_eq!(scoped.map.entries().len(), 5);
        assert_eq!(scoped.map.entries()[1].kind, TapeFileKind::Object);
        assert_eq!(scoped.map.entries()[2].kind, TapeFileKind::ParitySidecar);
        assert_eq!(scoped.map.entries()[3].kind, TapeFileKind::Object);
        assert_eq!(scoped.map.entries()[4].kind, TapeFileKind::ParitySidecar);
        assert_eq!(partial_sidecar.header.epoch_id, 1);
        assert_eq!(
            partial_sidecar.header.protected_ordinal_start,
            epoch_data_shards
        );
        assert_eq!(
            partial_sidecar.header.protected_ordinal_end_exclusive,
            epoch_data_shards + final_partial_real_shards
        );
        assert_eq!(
            partial_sidecar.header.logical_shard_count,
            epoch_data_shards
        );
        assert_eq!(
            partial_sidecar.header.real_data_shard_count,
            final_partial_real_shards
        );
        assert_eq!(
            partial_sidecar.header.data_crc_count,
            final_partial_real_shards as u32
        );

        let epoch0_tail_position = scoped
            .map
            .position_for_ordinal(epoch_data_shards - 1)
            .unwrap();
        let partial_head_position = scoped.map.position_for_ordinal(epoch_data_shards).unwrap();
        assert_eq!(epoch0_tail_position.tape_file_number, 1);
        assert_eq!(partial_head_position.tape_file_number, 3);
        let epoch0_tail_lba = scoped
            .map
            .physical_position(epoch0_tail_position)
            .unwrap()
            .lba;
        let partial_head_lba = scoped
            .map
            .physical_position(partial_head_position)
            .unwrap()
            .lba;
        assert!(
            partial_head_lba > epoch0_tail_lba + 1,
            "fixture must cross a filemark and sidecar between the full and partial object epochs"
        );

        let epoch0_failed_ordinal = 5;
        let epoch0_peer_ordinal = 0;
        let partial_failed_ordinal = epoch_data_shards;
        let partial_other_stripe_noise = epoch_data_shards + final_partial_real_shards - 1;
        let epoch0_failed = ordinal_to_stripe(epoch0_failed_ordinal, &scheme).unwrap();
        let epoch0_peer = ordinal_to_stripe(epoch0_peer_ordinal, &scheme).unwrap();
        let partial_failed = ordinal_to_stripe(partial_failed_ordinal, &scheme).unwrap();
        let partial_noise = ordinal_to_stripe(partial_other_stripe_noise, &scheme).unwrap();
        assert_eq!(epoch0_failed.neighborhood, 0);
        assert_eq!(epoch0_failed.stripe_index, epoch0_peer.stripe_index);
        assert_eq!(partial_failed.neighborhood, 1);
        assert_eq!(partial_failed.stripe_index, 0);
        assert_eq!(partial_failed.position, StripePosition::Data { index: 0 });
        assert_eq!(partial_noise.neighborhood, 1);
        assert_ne!(
            partial_noise.stripe_index, partial_failed.stripe_index,
            "partial-epoch noise must be outside the failed stripe"
        );
        for implicit_ordinal in [
            partial_failed_ordinal + u64::from(scheme.stripes_per_neighborhood),
            partial_failed_ordinal + 2 * u64::from(scheme.stripes_per_neighborhood),
            partial_failed_ordinal + 3 * u64::from(scheme.stripes_per_neighborhood),
        ] {
            let implicit = ordinal_to_stripe(implicit_ordinal, &scheme).unwrap();
            assert_eq!(implicit.neighborhood, partial_failed.neighborhood);
            assert_eq!(implicit.stripe_index, partial_failed.stripe_index);
            assert!(
                implicit_ordinal >= epoch_data_shards + final_partial_real_shards,
                "same-stripe peers beyond the D=k+1 final partial object are implicit zeros"
            );
        }

        let damaged_ordinals = vec![
            epoch0_failed_ordinal,
            epoch0_peer_ordinal,
            partial_failed_ordinal,
            partial_other_stripe_noise,
        ];
        let mut damaged_lbas = object_lbas_for_ordinals(&scoped, &damaged_ordinals);
        damaged_lbas.push(parity_lba_for_shard(
            &scoped,
            2,
            &first_sidecar,
            epoch0_failed.stripe_index,
            1,
        ));
        damaged_lbas.push(parity_lba_for_shard(
            &scoped,
            4,
            &partial_sidecar,
            partial_failed.stripe_index,
            2,
        ));
        damaged_lbas.push(parity_lba_for_shard(
            &scoped,
            4,
            &partial_sidecar,
            partial_noise.stripe_index,
            0,
        ));
        let mut unique_damaged_lbas = damaged_lbas.clone();
        unique_damaged_lbas.sort_unstable();
        unique_damaged_lbas.dedup();
        assert_eq!(
            unique_damaged_lbas.len(),
            damaged_lbas.len(),
            "fixture damage must target distinct physical blocks"
        );

        for (failed_ordinal, expected_sidecar_tape_file, expected_lost) in [
            (
                epoch0_failed_ordinal,
                2,
                vec![
                    StripePosition::Data { index: 0 },
                    StripePosition::Data { index: 1 },
                    StripePosition::Parity { index: 1 },
                ],
            ),
            (
                partial_failed_ordinal,
                4,
                vec![
                    StripePosition::Data { index: 0 },
                    StripePosition::Parity { index: 2 },
                ],
            ),
        ] {
            let failed_position = scoped.map.position_for_ordinal(failed_ordinal).unwrap();
            let mut raw = raw_tape_two_object_two_epoch_sidecars(
                first_object_blocks,
                &first_sidecar.blocks,
                partial_object_blocks,
                &partial_sidecar.blocks,
            );
            raw.unreadable_lbas = damaged_lbas.clone();

            let recovered = recover_object_block_from_sidecar(
                &mut raw,
                &scoped,
                &scheme,
                TAPE_UUID,
                BLOCK_SIZE,
                failed_position.tape_file_number,
                failed_position.block_within_file,
            )
            .unwrap_or_else(|err| {
                panic!(
                    "multi-object full-plus-final-partial damage should recover ordinal {failed_ordinal}: {err}"
                )
            });

            assert_eq!(recovered.failed_ordinal, failed_ordinal);
            assert_eq!(
                recovered.recovered_block, object_blocks[failed_ordinal as usize],
                "ordinal {failed_ordinal} recovered bytes"
            );
            assert_eq!(
                recovered.sidecar_tape_file_number, expected_sidecar_tape_file,
                "ordinal {failed_ordinal} should recover from its own object epoch sidecar"
            );
            assert_eq!(
                recovered.lost_shards, expected_lost,
                "ordinal {failed_ordinal} should only count its own epoch stripe damage"
            );
        }
    }

    /// Covers a single damage set spanning the full-object tail and the
    /// following final-partial object's real-data head in the B/O/S/O/S
    /// topology. Recovery must remain epoch-local even though the physical data
    /// LBAs cross a filemark plus the first epoch's sidecar cluster.
    #[test]
    fn full_then_partial_object_boundary_damage_recovers_per_epoch_sidecar() {
        let scheme = scheme(4, 3, 5);
        let epoch_data_shards = data_shards_per_epoch(&scheme).unwrap();
        let epoch_blocks = usize::try_from(epoch_data_shards).unwrap();
        let final_partial_real_shards = u64::from(scheme.data_blocks_per_stripe) + 1;
        let final_partial_blocks = usize::try_from(final_partial_real_shards).unwrap();
        let object_blocks = (0..epoch_blocks + final_partial_blocks)
            .map(|idx| block((idx + 217) as u8))
            .collect::<Vec<_>>();
        let first_object_blocks = &object_blocks[..epoch_blocks];
        let partial_object_blocks = &object_blocks[epoch_blocks..];
        let first_sidecar = sidecar_for_epoch_at(&scheme, 0, 0, first_object_blocks);
        let partial_sidecar =
            sidecar_for_epoch_at(&scheme, 1, epoch_data_shards, partial_object_blocks);
        let scoped = scoped_two_object_full_then_partial_sidecar_map(
            first_sidecar.blocks.len() as u64,
            partial_sidecar.blocks.len() as u64,
            epoch_data_shards,
            final_partial_real_shards,
        );

        let damage_start = epoch_data_shards - 2;
        let damage_end = epoch_data_shards + final_partial_real_shards;
        let damaged_ordinals = (damage_start..damage_end).collect::<Vec<_>>();
        assert_eq!(epoch_data_shards, 20);
        assert_eq!(final_partial_real_shards, 5);
        assert_eq!(damaged_ordinals, vec![18, 19, 20, 21, 22, 23, 24]);

        let full_tail_position = scoped
            .map
            .position_for_ordinal(epoch_data_shards - 1)
            .unwrap();
        let partial_head_position = scoped.map.position_for_ordinal(epoch_data_shards).unwrap();
        assert_eq!(full_tail_position.tape_file_number, 1);
        assert_eq!(partial_head_position.tape_file_number, 3);
        let full_tail_lba = scoped
            .map
            .physical_position(full_tail_position)
            .unwrap()
            .lba;
        let partial_head_lba = scoped
            .map
            .physical_position(partial_head_position)
            .unwrap()
            .lba;

        let damaged_lbas = object_lbas_for_ordinals(&scoped, &damaged_ordinals);
        let boundary_gaps = damaged_lbas
            .windows(2)
            .filter(|pair| pair[1] > pair[0] + 1)
            .collect::<Vec<_>>();
        assert_eq!(
            boundary_gaps.len(),
            1,
            "fixture must cross exactly one object boundary gap"
        );
        assert_eq!(boundary_gaps[0][0] as u64, full_tail_lba);
        assert_eq!(boundary_gaps[0][1] as u64, partial_head_lba);
        assert!(
            partial_head_lba > full_tail_lba + 1,
            "object-boundary damage must cross the filemark plus sidecar gap"
        );

        let mut recovered_full_tail = false;
        let mut recovered_partial_head = false;
        for failed_ordinal in damaged_ordinals {
            let failed_position = scoped.map.position_for_ordinal(failed_ordinal).unwrap();
            let mut raw = raw_tape_two_object_two_epoch_sidecars(
                first_object_blocks,
                &first_sidecar.blocks,
                partial_object_blocks,
                &partial_sidecar.blocks,
            );
            raw.unreadable_lbas = damaged_lbas.clone();

            let recovered = recover_object_block_from_sidecar(
                &mut raw,
                &scoped,
                &scheme,
                TAPE_UUID,
                BLOCK_SIZE,
                failed_position.tape_file_number,
                failed_position.block_within_file,
            )
            .unwrap_or_else(|err| {
                panic!(
                    "full-then-partial object-boundary damage should recover ordinal {failed_ordinal}: {err}"
                )
            });

            let failed_stripe = ordinal_to_stripe(failed_ordinal, &scheme).unwrap();
            let expected_lost = (damage_start..damage_end)
                .filter_map(|ordinal| {
                    let address = ordinal_to_stripe(ordinal, &scheme).unwrap();
                    (address.neighborhood == failed_stripe.neighborhood
                        && address.stripe_index == failed_stripe.stripe_index)
                        .then_some(address.position)
                })
                .collect::<Vec<_>>();
            let expected_sidecar_tape_file = if failed_ordinal < epoch_data_shards {
                recovered_full_tail = true;
                2
            } else {
                recovered_partial_head = true;
                4
            };

            assert_eq!(recovered.failed_ordinal, failed_ordinal);
            assert_eq!(
                recovered.recovered_block, object_blocks[failed_ordinal as usize],
                "object-boundary damage recovered ordinal {failed_ordinal}"
            );
            assert_eq!(
                recovered.sidecar_tape_file_number, expected_sidecar_tape_file,
                "ordinal {failed_ordinal} should recover from its own epoch sidecar"
            );
            assert_eq!(
                recovered.lost_shards, expected_lost,
                "object-boundary damage should only count losses from the failed epoch stripe"
            );
            assert!(
                recovered.lost_shards.len() <= scheme.parity_blocks_per_stripe as usize,
                "object-boundary damage should stay within the per-epoch stripe erasure limit"
            );
        }
        assert!(
            recovered_full_tail,
            "fixture must recover the full-object tail"
        );
        assert!(
            recovered_partial_head,
            "fixture must recover the final-partial object head"
        );
    }

    /// Extends the full-then-partial object-boundary fixture with sidecar body
    /// damage on both epoch sidecars. The sidecar headers and indexes remain
    /// readable, so recovery should count the damaged parity shards only for
    /// their own epoch/stripe while ignoring sidecar damage from the other
    /// side of the object boundary.
    #[test]
    fn full_then_partial_boundary_with_sidecar_noise_recovers_per_epoch_sidecar() {
        let scheme = scheme(4, 3, 5);
        let epoch_data_shards = data_shards_per_epoch(&scheme).unwrap();
        let epoch_blocks = usize::try_from(epoch_data_shards).unwrap();
        let final_partial_real_shards = u64::from(scheme.data_blocks_per_stripe) + 1;
        let final_partial_blocks = usize::try_from(final_partial_real_shards).unwrap();
        let object_blocks = (0..epoch_blocks + final_partial_blocks)
            .map(|idx| block((idx + 219) as u8))
            .collect::<Vec<_>>();
        let first_object_blocks = &object_blocks[..epoch_blocks];
        let partial_object_blocks = &object_blocks[epoch_blocks..];
        let first_sidecar = sidecar_for_epoch_at(&scheme, 0, 0, first_object_blocks);
        let partial_sidecar =
            sidecar_for_epoch_at(&scheme, 1, epoch_data_shards, partial_object_blocks);
        let scoped = scoped_two_object_full_then_partial_sidecar_map(
            first_sidecar.blocks.len() as u64,
            partial_sidecar.blocks.len() as u64,
            epoch_data_shards,
            final_partial_real_shards,
        );

        let damage_start = epoch_data_shards - 2;
        let damage_end = epoch_data_shards + final_partial_real_shards;
        let damaged_ordinals = (damage_start..damage_end).collect::<Vec<_>>();
        assert_eq!(damaged_ordinals, vec![18, 19, 20, 21, 22, 23, 24]);

        let full_tail_position = scoped
            .map
            .position_for_ordinal(epoch_data_shards - 1)
            .unwrap();
        let partial_head_position = scoped.map.position_for_ordinal(epoch_data_shards).unwrap();
        let partial_tail_position = scoped
            .map
            .position_for_ordinal(epoch_data_shards + final_partial_real_shards - 1)
            .unwrap();
        assert_eq!(full_tail_position.tape_file_number, 1);
        assert_eq!(partial_head_position.tape_file_number, 3);
        assert_eq!(partial_tail_position.tape_file_number, 3);

        let full_tail_lba = scoped
            .map
            .physical_position(full_tail_position)
            .unwrap()
            .lba;
        let partial_head_lba = scoped
            .map
            .physical_position(partial_head_position)
            .unwrap()
            .lba;
        let partial_tail_lba = scoped
            .map
            .physical_position(partial_tail_position)
            .unwrap()
            .lba;
        assert!(
            partial_head_lba > full_tail_lba + 1,
            "fixture must have a physical sidecar gap between object files"
        );

        let full_tail_address = ordinal_to_stripe(epoch_data_shards - 1, &scheme).unwrap();
        let partial_head_address = ordinal_to_stripe(epoch_data_shards, &scheme).unwrap();
        assert_eq!(full_tail_address.neighborhood, 0);
        assert_eq!(partial_head_address.neighborhood, 1);
        assert_eq!(partial_head_address.stripe_index, 0);
        assert_eq!(
            partial_head_address.position,
            StripePosition::Data { index: 0 }
        );

        let first_sidecar_parity = StripePosition::Parity { index: 1 };
        let partial_sidecar_parity = StripePosition::Parity { index: 2 };
        let first_sidecar_noise_lba = parity_lba_for_shard(
            &scoped,
            2,
            &first_sidecar,
            full_tail_address.stripe_index,
            1,
        );
        let partial_sidecar_noise_lba = parity_lba_for_shard(
            &scoped,
            4,
            &partial_sidecar,
            partial_head_address.stripe_index,
            2,
        );
        assert!(
            (first_sidecar_noise_lba as u64) > full_tail_lba
                && (first_sidecar_noise_lba as u64) < partial_head_lba,
            "first sidecar parity damage must sit in the boundary sidecar gap"
        );
        assert!(
            (partial_sidecar_noise_lba as u64) > partial_tail_lba,
            "partial sidecar parity damage must sit after the partial object data"
        );

        let mut damaged_lbas = object_lbas_for_ordinals(&scoped, &damaged_ordinals);
        damaged_lbas.push(first_sidecar_noise_lba);
        damaged_lbas.push(partial_sidecar_noise_lba);
        let mut unique_damaged_lbas = damaged_lbas.clone();
        unique_damaged_lbas.sort_unstable();
        unique_damaged_lbas.dedup();
        assert_eq!(
            unique_damaged_lbas.len(),
            damaged_lbas.len(),
            "object and sidecar damage must target distinct physical blocks"
        );

        let sidecar_noise = [
            (
                full_tail_address.neighborhood,
                full_tail_address.stripe_index,
                first_sidecar_parity,
            ),
            (
                partial_head_address.neighborhood,
                partial_head_address.stripe_index,
                partial_sidecar_parity,
            ),
        ];

        let mut saw_full_sidecar_noise = false;
        let mut saw_partial_sidecar_noise = false;
        for failed_ordinal in damaged_ordinals {
            let failed_position = scoped.map.position_for_ordinal(failed_ordinal).unwrap();
            let mut raw = raw_tape_two_object_two_epoch_sidecars(
                first_object_blocks,
                &first_sidecar.blocks,
                partial_object_blocks,
                &partial_sidecar.blocks,
            );
            raw.unreadable_lbas = damaged_lbas.clone();

            let recovered = recover_object_block_from_sidecar(
                &mut raw,
                &scoped,
                &scheme,
                TAPE_UUID,
                BLOCK_SIZE,
                failed_position.tape_file_number,
                failed_position.block_within_file,
            )
            .unwrap_or_else(|err| {
                panic!(
                    "full-then-partial boundary with sidecar noise should recover ordinal {failed_ordinal}: {err}"
                )
            });

            let failed_stripe = ordinal_to_stripe(failed_ordinal, &scheme).unwrap();
            let mut expected_lost = (damage_start..damage_end)
                .filter_map(|ordinal| {
                    let address = ordinal_to_stripe(ordinal, &scheme).unwrap();
                    (address.neighborhood == failed_stripe.neighborhood
                        && address.stripe_index == failed_stripe.stripe_index)
                        .then_some(address.position)
                })
                .collect::<Vec<_>>();
            for (neighborhood, stripe_index, position) in sidecar_noise {
                if neighborhood == failed_stripe.neighborhood
                    && stripe_index == failed_stripe.stripe_index
                {
                    expected_lost.push(position);
                }
            }
            if expected_lost.contains(&first_sidecar_parity) {
                saw_full_sidecar_noise = true;
            }
            if expected_lost.contains(&partial_sidecar_parity) {
                saw_partial_sidecar_noise = true;
            }
            let expected_sidecar_tape_file = if failed_ordinal < epoch_data_shards {
                2
            } else {
                4
            };

            assert_eq!(recovered.failed_ordinal, failed_ordinal);
            assert_eq!(
                recovered.recovered_block, object_blocks[failed_ordinal as usize],
                "boundary plus sidecar-noise damage recovered ordinal {failed_ordinal}"
            );
            assert_eq!(
                recovered.sidecar_tape_file_number, expected_sidecar_tape_file,
                "ordinal {failed_ordinal} should use its own epoch sidecar"
            );
            assert_eq!(
                recovered.lost_shards, expected_lost,
                "sidecar parity noise should only count for its own epoch stripe"
            );
            assert!(
                recovered.lost_shards.len() <= scheme.parity_blocks_per_stripe as usize,
                "sidecar noise should stay within the per-epoch stripe erasure limit"
            );
        }
        assert!(
            saw_full_sidecar_noise,
            "fixture must exercise first-epoch sidecar parity damage"
        );
        assert!(
            saw_partial_sidecar_noise,
            "fixture must exercise partial-epoch sidecar parity damage"
        );
    }

    /// Boundary-specific parity overload: if the failed boundary data shard
    /// loses every parity shard in its own sidecar, recovery must report only
    /// that epoch/stripe as over the RS limit. Damage in the adjacent epoch's
    /// sidecar is present in the same raw tape but must not be counted.
    #[test]
    fn full_then_partial_boundary_all_sidecar_parity_shards_stay_epoch_local_overload() {
        let scheme = scheme(4, 3, 5);
        assert_full_then_partial_boundary_sidecar_parity_overload(
            &scheme,
            u64::from(scheme.data_blocks_per_stripe) + 1,
            0,
            "partial-head-stripe-0",
        );
    }

    #[test]
    fn full_then_partial_boundary_sidecar_parity_overload_handles_nonzero_partial_stripe() {
        let scheme = scheme(4, 3, 5);
        assert_full_then_partial_boundary_sidecar_parity_overload(
            &scheme,
            u64::from(scheme.data_blocks_per_stripe) + 2,
            1,
            "partial-offset-1-stripe-1",
        );
    }

    fn assert_full_then_partial_boundary_sidecar_parity_overload(
        scheme: &ParityScheme,
        final_partial_real_shards: u64,
        partial_failed_offset: u64,
        case_name: &str,
    ) {
        let epoch_data_shards = data_shards_per_epoch(scheme).unwrap();
        let epoch_blocks = usize::try_from(epoch_data_shards).unwrap();
        assert!(final_partial_real_shards > partial_failed_offset);
        assert!(
            partial_failed_offset < u64::from(scheme.stripes_per_neighborhood),
            "{case_name}: partial failed offset should stay in data row 0"
        );
        let final_partial_blocks = usize::try_from(final_partial_real_shards).unwrap();
        let object_blocks = (0..epoch_blocks + final_partial_blocks)
            .map(|idx| block((idx + 191) as u8))
            .collect::<Vec<_>>();
        let first_object_blocks = &object_blocks[..epoch_blocks];
        let partial_object_blocks = &object_blocks[epoch_blocks..];
        let first_sidecar = sidecar_for_epoch_at(scheme, 0, 0, first_object_blocks);
        let partial_sidecar =
            sidecar_for_epoch_at(scheme, 1, epoch_data_shards, partial_object_blocks);
        let scoped = scoped_two_object_full_then_partial_sidecar_map(
            first_sidecar.blocks.len() as u64,
            partial_sidecar.blocks.len() as u64,
            epoch_data_shards,
            final_partial_real_shards,
        );

        let full_tail_ordinal = epoch_data_shards - 1;
        let partial_head_ordinal = epoch_data_shards;
        let partial_failed_ordinal = epoch_data_shards + partial_failed_offset;
        let full_tail_position = scoped.map.position_for_ordinal(full_tail_ordinal).unwrap();
        let partial_head_position = scoped
            .map
            .position_for_ordinal(partial_head_ordinal)
            .unwrap();
        let partial_failed_position = scoped
            .map
            .position_for_ordinal(partial_failed_ordinal)
            .unwrap();
        let partial_tail_position = scoped
            .map
            .position_for_ordinal(epoch_data_shards + final_partial_real_shards - 1)
            .unwrap();
        assert_eq!(full_tail_position.tape_file_number, 1);
        assert_eq!(partial_head_position.tape_file_number, 3);
        assert_eq!(partial_failed_position.tape_file_number, 3);
        assert_eq!(partial_tail_position.tape_file_number, 3);

        let full_tail_lba = scoped
            .map
            .physical_position(full_tail_position)
            .unwrap()
            .lba;
        let partial_head_lba = scoped
            .map
            .physical_position(partial_head_position)
            .unwrap()
            .lba;
        let partial_failed_lba = scoped
            .map
            .physical_position(partial_failed_position)
            .unwrap()
            .lba;
        let partial_tail_lba = scoped
            .map
            .physical_position(partial_tail_position)
            .unwrap()
            .lba;
        assert!(
            partial_head_lba > full_tail_lba + 1,
            "fixture must have a physical sidecar gap between object files"
        );
        assert!(
            partial_failed_lba >= partial_head_lba && partial_failed_lba <= partial_tail_lba,
            "{case_name}: failed partial ordinal must sit inside the partial object"
        );

        let full_tail_address = ordinal_to_stripe(full_tail_ordinal, scheme).unwrap();
        let partial_failed_address = ordinal_to_stripe(partial_failed_ordinal, scheme).unwrap();
        assert_eq!(full_tail_address.neighborhood, 0);
        assert_eq!(
            full_tail_address.stripe_index,
            scheme.stripes_per_neighborhood - 1
        );
        assert_eq!(
            full_tail_address.position,
            StripePosition::Data {
                index: scheme.data_blocks_per_stripe - 1
            }
        );
        assert_eq!(partial_failed_address.neighborhood, 1);
        assert_eq!(
            partial_failed_address.stripe_index, partial_failed_offset as u32,
            "{case_name}: partial failed ordinal should exercise the requested stripe"
        );
        assert_eq!(
            partial_failed_address.position,
            StripePosition::Data { index: 0 }
        );

        let mut damaged_lbas =
            object_lbas_for_ordinals(&scoped, &[full_tail_ordinal, partial_failed_ordinal]);
        for parity_index in 0..scheme.parity_blocks_per_stripe {
            let first_sidecar_lba = parity_lba_for_shard(
                &scoped,
                2,
                &first_sidecar,
                full_tail_address.stripe_index,
                parity_index,
            );
            assert!(
                (first_sidecar_lba as u64) > full_tail_lba
                    && (first_sidecar_lba as u64) < partial_head_lba,
                "first sidecar parity overload must sit in the boundary sidecar gap"
            );
            damaged_lbas.push(first_sidecar_lba);

            let partial_sidecar_lba = parity_lba_for_shard(
                &scoped,
                4,
                &partial_sidecar,
                partial_failed_address.stripe_index,
                parity_index,
            );
            assert!(
                (partial_sidecar_lba as u64) > partial_tail_lba,
                "partial sidecar parity overload must sit after the partial object data"
            );
            damaged_lbas.push(partial_sidecar_lba);
        }
        assert_eq!(
            damaged_lbas.len(),
            2 + usize::from(scheme.parity_blocks_per_stripe) * 2
        );
        let mut unique_damaged_lbas = damaged_lbas.clone();
        unique_damaged_lbas.sort_unstable();
        unique_damaged_lbas.dedup();
        assert_eq!(
            unique_damaged_lbas.len(),
            damaged_lbas.len(),
            "boundary object and sidecar overload damage must target distinct physical blocks"
        );

        for (failed_ordinal, expected_stripe) in [
            (full_tail_ordinal, full_tail_address),
            (partial_failed_ordinal, partial_failed_address),
        ] {
            let failed_position = scoped.map.position_for_ordinal(failed_ordinal).unwrap();
            let mut raw = raw_tape_two_object_two_epoch_sidecars(
                first_object_blocks,
                &first_sidecar.blocks,
                partial_object_blocks,
                &partial_sidecar.blocks,
            );
            raw.unreadable_lbas = damaged_lbas.clone();

            let err = recover_object_block_from_sidecar(
                &mut raw,
                &scoped,
                scheme,
                TAPE_UUID,
                BLOCK_SIZE,
                failed_position.tape_file_number,
                failed_position.block_within_file,
            )
            .expect_err("failed data plus every same-stripe sidecar parity shard exceeds m");

            match err {
                ParityError::Unrecoverable {
                    stripe,
                    lost_count,
                    limit,
                } => {
                    assert_eq!(
                        stripe, expected_stripe,
                        "{case_name}: boundary sidecar overload should stay pinned to the failed epoch stripe"
                    );
                    assert_eq!(
                        lost_count,
                        scheme.parity_blocks_per_stripe + 1,
                        "{case_name}: adjacent epoch sidecar damage must not inflate the failed stripe loss count"
                    );
                    assert_eq!(limit, scheme.parity_blocks_per_stripe);
                }
                other => {
                    panic!("expected boundary sidecar parity overload error, got {other:?}")
                }
            }
        }
    }

    #[test]
    fn object_block_recovery_rejects_unvalidated_suffix_before_tape_io() {
        let scheme = scheme(2, 1, 1);
        let map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, 2, 0),
            TapeFileMapEntry::object(2, 2, 2),
        ])
        .unwrap();
        let scoped = ScopedFilemarkMap {
            map,
            validated_prefix_tape_files: Some(2),
            scope: MapScope::Prefix {
                map_total_data_ordinals: 2,
                highest_protected_ordinal: 2,
            },
        };
        let mut raw = RawVec::new(Vec::new());

        let err = recover_object_block_from_sidecar(
            &mut raw, &scoped, &scheme, TAPE_UUID, BLOCK_SIZE, 2, 0,
        )
        .expect_err("unvalidated suffix recovery is refused");
        assert!(matches!(
            err,
            ParityError::OutsideValidatedMapPrefix {
                ordinal: 2,
                prefix_ordinals: 2
            }
        ));
        assert_eq!(raw.configured_block_size, None);
        assert_eq!(raw.position().unwrap(), PhysicalPositionHint::new(0));
    }

    #[test]
    fn recovery_rejects_sidecar_beyond_durable_prefix_before_tape_io() {
        let scheme = scheme(2, 1, 1);
        let object_blocks = vec![block(1), block(2)];
        let sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let protected = object_blocks.len() as u64;
        let map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, protected, 0),
            TapeFileMapEntry::parity_sidecar(2, sidecar.blocks.len() as u64, 0, 0, protected),
        ])
        .expect("map with suffix sidecar validates");
        let scoped = ScopedFilemarkMap {
            map,
            validated_prefix_tape_files: Some(2),
            scope: MapScope::Prefix {
                map_total_data_ordinals: protected,
                highest_protected_ordinal: protected,
            },
        };
        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);

        let err =
            recover_ordinal_from_sidecar(&mut raw, &scoped, &scheme, TAPE_UUID, BLOCK_SIZE, 0)
                .expect_err("unvalidated suffix sidecar is outside the durable boundary");

        assert!(matches!(
            err,
            ParityError::OutsideValidatedMapPrefix {
                ordinal: 0,
                prefix_ordinals: 2
            }
        ));
        assert_eq!(
            raw.configured_block_size, None,
            "durable-boundary prefix gate must fire before recovery tape I/O"
        );
        assert_eq!(raw.position().unwrap(), PhysicalPositionHint::new(0));
    }

    #[test]
    fn recovery_counts_unvalidated_prefix_peers_as_erasures_until_unrecoverable() {
        let scheme = scheme(3, 1, 1);
        let committed_object_blocks = vec![block(1)];
        let suffix_object_blocks = vec![block(2), block(3)];
        let mut all_object_blocks = committed_object_blocks.clone();
        all_object_blocks.extend(suffix_object_blocks.clone());
        let sidecar = sidecar_for_epoch(&scheme, &all_object_blocks);
        let protected = all_object_blocks.len() as u64;
        let map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, committed_object_blocks.len() as u64, 0),
            TapeFileMapEntry::parity_sidecar(2, sidecar.blocks.len() as u64, 0, 0, protected),
            TapeFileMapEntry::object(
                3,
                suffix_object_blocks.len() as u64,
                committed_object_blocks.len() as u64,
            ),
        ])
        .expect("map with forensic suffix peers validates");
        let scoped = ScopedFilemarkMap {
            map,
            validated_prefix_tape_files: Some(3),
            scope: MapScope::Prefix {
                map_total_data_ordinals: committed_object_blocks.len() as u64,
                highest_protected_ordinal: protected,
            },
        };
        let suffix_peer_lbas = object_lbas_for_ordinals(&scoped, &[1, 2]);
        let mut raw = RawVec::new(records_for_object_sidecar_then_object(
            &committed_object_blocks,
            &sidecar.blocks,
            &suffix_object_blocks,
        ));

        let err =
            recover_ordinal_from_sidecar(&mut raw, &scoped, &scheme, TAPE_UUID, BLOCK_SIZE, 0)
                .expect_err("unvalidated same-stripe peers must exhaust recovery budget");

        match err {
            ParityError::Unrecoverable {
                stripe,
                lost_count,
                limit,
            } => {
                assert_eq!(stripe, ordinal_to_stripe(0, &scheme).unwrap());
                assert_eq!(
                    lost_count, 3,
                    "failed shard plus two unvalidated same-stripe peers are erasures"
                );
                assert_eq!(limit, scheme.parity_blocks_per_stripe);
            }
            other => {
                panic!("expected durable-boundary peer-filter unrecoverable error, got {other:?}")
            }
        }
        assert_eq!(raw.configured_block_size, Some(BLOCK_SIZE));
        for suffix_lba in suffix_peer_lbas {
            assert!(
                !raw.read_lbas.contains(&suffix_lba),
                "unvalidated suffix peer LBA {suffix_lba} must be filtered before tape read"
            );
        }
    }

    #[test]
    fn recovery_recovers_when_unvalidated_prefix_peers_fit_erasure_budget() {
        let scheme = scheme(3, 2, 1);
        let committed_object_blocks = vec![block(1), block(2)];
        let suffix_object_blocks = vec![block(3)];
        let mut all_object_blocks = committed_object_blocks.clone();
        all_object_blocks.extend(suffix_object_blocks.clone());
        let sidecar = sidecar_for_epoch(&scheme, &all_object_blocks);
        let protected = all_object_blocks.len() as u64;
        let map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, committed_object_blocks.len() as u64, 0),
            TapeFileMapEntry::parity_sidecar(2, sidecar.blocks.len() as u64, 0, 0, protected),
            TapeFileMapEntry::object(
                3,
                suffix_object_blocks.len() as u64,
                committed_object_blocks.len() as u64,
            ),
        ])
        .expect("map with one forensic suffix peer validates");
        let scoped = ScopedFilemarkMap {
            map,
            validated_prefix_tape_files: Some(3),
            scope: MapScope::Prefix {
                map_total_data_ordinals: committed_object_blocks.len() as u64,
                highest_protected_ordinal: protected,
            },
        };
        let committed_peer_lba = object_lbas_for_ordinals(&scoped, &[1])
            .pop()
            .expect("committed peer ordinal has a physical LBA");
        let suffix_peer_lba = object_lbas_for_ordinals(&scoped, &[2])
            .pop()
            .expect("suffix peer ordinal has a physical LBA");
        let failed_lba = object_lbas_for_ordinals(&scoped, &[0])
            .pop()
            .expect("failed ordinal has a physical LBA");
        let mut raw = RawVec::new(records_for_object_sidecar_then_object(
            &committed_object_blocks,
            &sidecar.blocks,
            &suffix_object_blocks,
        ));

        let recovered =
            recover_ordinal_from_sidecar(&mut raw, &scoped, &scheme, TAPE_UUID, BLOCK_SIZE, 0)
                .expect("one unvalidated same-stripe peer fits the m=2 erasure budget");

        assert_eq!(recovered.recovered_block, all_object_blocks[0]);
        assert_eq!(recovered.sidecar_tape_file_number, 2);
        assert_eq!(
            recovered.lost_shards,
            vec![
                StripePosition::Data { index: 0 },
                StripePosition::Data { index: 2 },
            ],
            "failed shard plus the filtered suffix peer should be the only data erasures"
        );
        assert_eq!(raw.configured_block_size, Some(BLOCK_SIZE));
        assert!(
            raw.read_lbas.contains(&committed_peer_lba),
            "committed same-stripe peer LBA {committed_peer_lba} should be read"
        );
        assert!(
            !raw.read_lbas.contains(&suffix_peer_lba),
            "unvalidated suffix peer LBA {suffix_peer_lba} must be filtered before tape read"
        );
        assert_eq!(
            raw.position().unwrap(),
            PhysicalPositionHint::new((failed_lba + 1) as u64),
            "successful recovery repositions just after the failed data block"
        );
    }

    #[test]
    fn recovery_counts_committed_parity_peer_with_unvalidated_data_peer() {
        let scheme = scheme(3, 2, 1);
        let committed_object_blocks = vec![block(1), block(2)];
        let suffix_object_blocks = vec![block(3)];
        let mut all_object_blocks = committed_object_blocks.clone();
        all_object_blocks.extend(suffix_object_blocks.clone());
        let sidecar = sidecar_for_epoch(&scheme, &all_object_blocks);
        let protected = all_object_blocks.len() as u64;
        let map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, committed_object_blocks.len() as u64, 0),
            TapeFileMapEntry::parity_sidecar(2, sidecar.blocks.len() as u64, 0, 0, protected),
            TapeFileMapEntry::object(
                3,
                suffix_object_blocks.len() as u64,
                committed_object_blocks.len() as u64,
            ),
        ])
        .expect("map with one forensic suffix peer validates");
        let scoped = ScopedFilemarkMap {
            map,
            validated_prefix_tape_files: Some(3),
            scope: MapScope::Prefix {
                map_total_data_ordinals: committed_object_blocks.len() as u64,
                highest_protected_ordinal: protected,
            },
        };
        let committed_peer_lba = object_lbas_for_ordinals(&scoped, &[1])
            .pop()
            .expect("committed peer ordinal has a physical LBA");
        let suffix_peer_lba = object_lbas_for_ordinals(&scoped, &[2])
            .pop()
            .expect("suffix peer ordinal has a physical LBA");
        let parity0_lba = parity_lba_for_shard(&scoped, 2, &sidecar, 0, 0);
        let parity1_lba = parity_lba_for_shard(&scoped, 2, &sidecar, 0, 1);
        let mut raw = RawVec::new(records_for_object_sidecar_then_object(
            &committed_object_blocks,
            &sidecar.blocks,
            &suffix_object_blocks,
        ));
        raw.unreadable_lbas.push(parity0_lba);

        let err =
            recover_ordinal_from_sidecar(&mut raw, &scoped, &scheme, TAPE_UUID, BLOCK_SIZE, 0)
                .expect_err(
                    "failed data plus filtered suffix data plus one missing parity exceeds m=2",
                );

        match err {
            ParityError::Unrecoverable {
                stripe,
                lost_count,
                limit,
            } => {
                assert_eq!(stripe, ordinal_to_stripe(0, &scheme).unwrap());
                assert_eq!(
                    lost_count, 3,
                    "failed data, filtered suffix data, and damaged parity must all count"
                );
                assert_eq!(limit, scheme.parity_blocks_per_stripe);
            }
            other => panic!("expected combined data/parity erasure failure, got {other:?}"),
        }
        assert!(
            raw.read_lbas.contains(&committed_peer_lba),
            "committed same-stripe data peer LBA {committed_peer_lba} should be read"
        );
        assert!(
            !raw.read_lbas.contains(&suffix_peer_lba),
            "unvalidated suffix data peer LBA {suffix_peer_lba} must be filtered before tape read"
        );
        assert!(
            raw.read_lbas.contains(&parity0_lba),
            "damaged committed parity peer LBA {parity0_lba} should be read and counted"
        );
        assert!(
            raw.read_lbas.contains(&parity1_lba),
            "surviving committed parity peer LBA {parity1_lba} should be read"
        );
    }

    #[test]
    fn recovery_filters_unvalidated_prefix_peers_per_failed_stripe() {
        let scheme = scheme(3, 2, 2);
        let committed_object_blocks = vec![block(1), block(2), block(3), block(4)];
        let suffix_object_blocks = vec![block(5), block(6)];
        let mut all_object_blocks = committed_object_blocks.clone();
        all_object_blocks.extend(suffix_object_blocks.clone());
        let sidecar = sidecar_for_epoch(&scheme, &all_object_blocks);
        let protected = all_object_blocks.len() as u64;
        let map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, committed_object_blocks.len() as u64, 0),
            TapeFileMapEntry::parity_sidecar(2, sidecar.blocks.len() as u64, 0, 0, protected),
            TapeFileMapEntry::object(
                3,
                suffix_object_blocks.len() as u64,
                committed_object_blocks.len() as u64,
            ),
        ])
        .expect("map with multi-stripe forensic suffix peers validates");
        let scoped = ScopedFilemarkMap {
            map,
            validated_prefix_tape_files: Some(3),
            scope: MapScope::Prefix {
                map_total_data_ordinals: committed_object_blocks.len() as u64,
                highest_protected_ordinal: protected,
            },
        };
        let failed_ordinal = 1;
        let committed_same_stripe_peer_ordinal = 3;
        let suffix_other_stripe_peer_ordinal = 4;
        let suffix_same_stripe_peer_ordinal = 5;
        assert_eq!(
            ordinal_to_stripe(failed_ordinal, &scheme).unwrap(),
            StripeAddress {
                neighborhood: 0,
                stripe_index: 1,
                position: StripePosition::Data { index: 0 },
            }
        );
        assert_eq!(
            ordinal_to_stripe(committed_same_stripe_peer_ordinal, &scheme)
                .unwrap()
                .stripe_index,
            1,
            "committed peer must sit in the failed stripe"
        );
        assert_eq!(
            ordinal_to_stripe(suffix_same_stripe_peer_ordinal, &scheme)
                .unwrap()
                .stripe_index,
            1,
            "suffix peer must sit in the failed stripe"
        );
        assert_eq!(
            ordinal_to_stripe(suffix_other_stripe_peer_ordinal, &scheme)
                .unwrap()
                .stripe_index,
            0,
            "second suffix peer must exercise a different stripe"
        );
        let committed_peer_lba =
            object_lbas_for_ordinals(&scoped, &[committed_same_stripe_peer_ordinal])
                .pop()
                .expect("committed same-stripe peer has a physical LBA");
        let suffix_same_stripe_lba =
            object_lbas_for_ordinals(&scoped, &[suffix_same_stripe_peer_ordinal])
                .pop()
                .expect("suffix same-stripe peer has a physical LBA");
        let suffix_other_stripe_lba =
            object_lbas_for_ordinals(&scoped, &[suffix_other_stripe_peer_ordinal])
                .pop()
                .expect("suffix other-stripe peer has a physical LBA");
        let failed_lba = object_lbas_for_ordinals(&scoped, &[failed_ordinal])
            .pop()
            .expect("failed ordinal has a physical LBA");
        let mut raw = RawVec::new(records_for_object_sidecar_then_object(
            &committed_object_blocks,
            &sidecar.blocks,
            &suffix_object_blocks,
        ));

        let recovered = recover_ordinal_from_sidecar(
            &mut raw,
            &scoped,
            &scheme,
            TAPE_UUID,
            BLOCK_SIZE,
            failed_ordinal,
        )
        .expect("one unvalidated peer in the failed stripe fits the m=2 erasure budget");

        assert_eq!(
            recovered.recovered_block,
            all_object_blocks[failed_ordinal as usize]
        );
        assert_eq!(recovered.sidecar_tape_file_number, 2);
        assert_eq!(
            recovered.lost_shards,
            vec![
                StripePosition::Data { index: 0 },
                StripePosition::Data { index: 2 },
            ],
            "only the failed stripe should count the filtered suffix peer as an erasure"
        );
        assert!(
            raw.read_lbas.contains(&committed_peer_lba),
            "committed same-stripe peer LBA {committed_peer_lba} should be read"
        );
        assert!(
            !raw.read_lbas.contains(&suffix_same_stripe_lba),
            "unvalidated same-stripe suffix peer LBA {suffix_same_stripe_lba} must be filtered before tape read"
        );
        assert!(
            !raw.read_lbas.contains(&suffix_other_stripe_lba),
            "unvalidated other-stripe suffix peer LBA {suffix_other_stripe_lba} must not be read or counted"
        );
        assert_eq!(
            raw.position().unwrap(),
            PhysicalPositionHint::new((failed_lba + 1) as u64),
            "successful recovery repositions just after the failed data block"
        );
    }

    #[test]
    fn recovers_failed_data_ordinal_from_sidecar_parity() {
        let scheme = scheme(2, 1, 2);
        let object_blocks = vec![block(1), block(2), block(3), block(4)];
        let sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
        raw.unreadable_lbas.push(4); // ordinal 2 physical position.

        let recovered =
            recover_ordinal_from_sidecar(&mut raw, &scoped, &scheme, TAPE_UUID, BLOCK_SIZE, 2)
                .expect("sidecar recovery succeeds");

        assert_eq!(recovered.recovered_block, object_blocks[2]);
        assert_eq!(recovered.sidecar_tape_file_number, 2);
        assert_eq!(
            recovered.lost_shards,
            vec![StripePosition::Data { index: 1 }]
        );
        assert_eq!(raw.position().unwrap(), PhysicalPositionHint::new(5));
    }

    #[test]
    fn multiple_object_data_erasures_in_one_stripe_recover_up_to_m() {
        let scheme = scheme(4, 3, 5);
        let object_blocks = (0..20)
            .map(|idx| block((idx + 101) as u8))
            .collect::<Vec<_>>();
        let sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);

        for (lost_ordinals, expected_lost) in [
            (
                vec![0, 5],
                vec![
                    StripePosition::Data { index: 0 },
                    StripePosition::Data { index: 1 },
                ],
            ),
            (
                vec![0, 5, 10],
                vec![
                    StripePosition::Data { index: 0 },
                    StripePosition::Data { index: 1 },
                    StripePosition::Data { index: 2 },
                ],
            ),
        ] {
            let lost_addresses = lost_ordinals
                .iter()
                .map(|ordinal| ordinal_to_stripe(*ordinal, &scheme).unwrap())
                .collect::<Vec<_>>();
            assert!(
                lost_addresses
                    .iter()
                    .all(|address| address.stripe_index == 0),
                "fixture ordinals must target one stripe"
            );
            assert_eq!(
                lost_addresses
                    .iter()
                    .map(|address| address.position)
                    .collect::<Vec<_>>(),
                expected_lost
            );

            let damaged_lbas = object_lbas_for_ordinals(&scoped, &lost_ordinals);
            for failed_ordinal in &lost_ordinals {
                let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
                raw.unreadable_lbas = damaged_lbas.clone();

                let recovered = recover_ordinal_from_sidecar(
                    &mut raw,
                    &scoped,
                    &scheme,
                    TAPE_UUID,
                    BLOCK_SIZE,
                    *failed_ordinal,
                )
                .unwrap_or_else(|err| {
                    panic!(
                        "same-stripe object erasures {lost_ordinals:?} should recover ordinal {failed_ordinal}: {err}"
                    )
                });

                assert_eq!(
                    recovered.recovered_block, object_blocks[*failed_ordinal as usize],
                    "same-stripe erasures recovered ordinal {failed_ordinal}"
                );
                assert_eq!(
                    recovered.lost_shards, expected_lost,
                    "same-stripe erasures should report the same lost shards for ordinal {failed_ordinal}"
                );
            }
        }
    }

    #[test]
    fn non_contiguous_object_erasures_across_stripes_recover_per_failed_stripe() {
        let scheme = scheme(4, 3, 5);
        let object_blocks = (0..20)
            .map(|idx| block((idx + 121) as u8))
            .collect::<Vec<_>>();
        let sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let lost_ordinals = vec![0, 1, 5, 6];
        let damaged_lbas = object_lbas_for_ordinals(&scoped, &lost_ordinals);

        for failed_ordinal in &lost_ordinals {
            let failed_stripe = ordinal_to_stripe(*failed_ordinal, &scheme).unwrap();
            let expected_lost = lost_ordinals
                .iter()
                .filter_map(|ordinal| {
                    let address = ordinal_to_stripe(*ordinal, &scheme).unwrap();
                    (address.stripe_index == failed_stripe.stripe_index).then_some(address.position)
                })
                .collect::<Vec<_>>();
            assert_eq!(
                expected_lost,
                vec![
                    StripePosition::Data { index: 0 },
                    StripePosition::Data { index: 1 },
                ],
                "fixture must lose two data shards in the failed stripe"
            );

            let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
            raw.unreadable_lbas = damaged_lbas.clone();

            let recovered = recover_ordinal_from_sidecar(
                &mut raw,
                &scoped,
                &scheme,
                TAPE_UUID,
                BLOCK_SIZE,
                *failed_ordinal,
            )
            .unwrap_or_else(|err| {
                panic!(
                    "multi-stripe non-contiguous erasures {lost_ordinals:?} should recover ordinal {failed_ordinal}: {err}"
                )
            });

            assert_eq!(
                recovered.recovered_block, object_blocks[*failed_ordinal as usize],
                "multi-stripe erasures recovered ordinal {failed_ordinal}"
            );
            assert_eq!(
                recovered.lost_shards, expected_lost,
                "multi-stripe erasures should only report losses from the failed stripe"
            );
        }
    }

    #[test]
    fn same_stripe_data_and_parity_erasures_recover_at_rs_limit() {
        let scheme = scheme(4, 3, 5);
        let object_blocks = (0..20)
            .map(|idx| block((idx + 131) as u8))
            .collect::<Vec<_>>();
        let sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let lost_ordinals = vec![0, 5];
        let mut damaged_lbas = object_lbas_for_ordinals(&scoped, &lost_ordinals);
        assert_eq!(sidecar.index.parity_entries[0].stripe_index, 0);
        assert_eq!(sidecar.index.parity_entries[0].parity_index, 0);
        let parity_position = scoped
            .map
            .physical_position(TapeFilePosition {
                tape_file_number: 2,
                block_within_file: u64::from(sidecar.header.shard_index_block_count),
            })
            .unwrap();
        damaged_lbas.push(usize::try_from(parity_position.lba).unwrap());

        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
        raw.unreadable_lbas = damaged_lbas;

        let recovered =
            recover_ordinal_from_sidecar(&mut raw, &scoped, &scheme, TAPE_UUID, BLOCK_SIZE, 5)
                .expect("two same-stripe data erasures plus one parity erasure should recover");

        assert_eq!(recovered.recovered_block, object_blocks[5]);
        assert_eq!(
            recovered.lost_shards,
            vec![
                StripePosition::Data { index: 0 },
                StripePosition::Data { index: 1 },
                StripePosition::Parity { index: 0 },
            ]
        );
        assert_eq!(
            recovered.lost_shards.len(),
            scheme.parity_blocks_per_stripe as usize,
            "fixture should recover exactly at the RS erasure limit"
        );
    }

    #[test]
    fn data_and_non_first_parity_erasures_recover_at_rs_limit() {
        let scheme = scheme(4, 3, 5);
        let object_blocks = (0..20)
            .map(|idx| block((idx + 141) as u8))
            .collect::<Vec<_>>();
        let sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);

        for (lost_ordinals, stripe_index, parity_index, failed_ordinal, expected_lost) in [
            (
                vec![0, 5],
                0,
                1,
                5,
                vec![
                    StripePosition::Data { index: 0 },
                    StripePosition::Data { index: 1 },
                    StripePosition::Parity { index: 1 },
                ],
            ),
            (
                vec![3, 8],
                3,
                2,
                8,
                vec![
                    StripePosition::Data { index: 0 },
                    StripePosition::Data { index: 1 },
                    StripePosition::Parity { index: 2 },
                ],
            ),
        ] {
            let lost_addresses = lost_ordinals
                .iter()
                .map(|ordinal| ordinal_to_stripe(*ordinal, &scheme).unwrap())
                .collect::<Vec<_>>();
            assert!(
                lost_addresses
                    .iter()
                    .all(|address| address.stripe_index == stripe_index),
                "fixture ordinals must target stripe {stripe_index}"
            );
            assert_eq!(
                lost_addresses
                    .iter()
                    .map(|address| address.position)
                    .collect::<Vec<_>>(),
                vec![
                    StripePosition::Data { index: 0 },
                    StripePosition::Data { index: 1 },
                ],
                "fixture should lose two data shards in the target stripe"
            );

            let mut damaged_lbas = object_lbas_for_ordinals(&scoped, &lost_ordinals);
            damaged_lbas.push(parity_lba_for_shard(
                &scoped,
                2,
                &sidecar,
                stripe_index,
                parity_index,
            ));
            let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
            raw.unreadable_lbas = damaged_lbas;

            let recovered = recover_ordinal_from_sidecar(
                &mut raw,
                &scoped,
                &scheme,
                TAPE_UUID,
                BLOCK_SIZE,
                failed_ordinal,
            )
            .unwrap_or_else(|err| {
                panic!("data plus parity erasures should recover ordinal {failed_ordinal}: {err}")
            });

            assert_eq!(
                recovered.recovered_block, object_blocks[failed_ordinal as usize],
                "data plus parity erasures recovered ordinal {failed_ordinal}"
            );
            assert_eq!(
                recovered.lost_shards, expected_lost,
                "lost shards should include the selected non-first parity shard"
            );
            assert_eq!(
                recovered.lost_shards.len(),
                scheme.parity_blocks_per_stripe as usize,
                "fixture should recover exactly at the RS erasure limit"
            );
        }
    }

    #[test]
    fn same_stripe_data_and_parity_erasures_over_rs_limit_is_unrecoverable() {
        let scheme = scheme(4, 3, 5);
        let object_blocks = (0..20)
            .map(|idx| block((idx + 161) as u8))
            .collect::<Vec<_>>();
        let sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let lost_ordinals = vec![0, 5, 10];
        let expected_lost_data = lost_ordinals
            .iter()
            .map(|ordinal| ordinal_to_stripe(*ordinal, &scheme).unwrap())
            .collect::<Vec<_>>();
        assert!(
            expected_lost_data
                .iter()
                .all(|address| address.stripe_index == 0),
            "fixture ordinals must target stripe 0"
        );
        assert_eq!(
            expected_lost_data
                .iter()
                .map(|address| address.position)
                .collect::<Vec<_>>(),
            vec![
                StripePosition::Data { index: 0 },
                StripePosition::Data { index: 1 },
                StripePosition::Data { index: 2 },
            ],
            "fixture should lose three data shards in one stripe"
        );

        let mut damaged_lbas = object_lbas_for_ordinals(&scoped, &lost_ordinals);
        damaged_lbas.push(parity_lba_for_shard(&scoped, 2, &sidecar, 0, 2));
        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
        raw.unreadable_lbas = damaged_lbas;

        let err =
            recover_ordinal_from_sidecar(&mut raw, &scoped, &scheme, TAPE_UUID, BLOCK_SIZE, 10)
                .expect_err("three data erasures plus one parity erasure should exceed m");

        match err {
            ParityError::Unrecoverable {
                stripe,
                lost_count,
                limit,
            } => {
                assert_eq!(stripe, ordinal_to_stripe(10, &scheme).unwrap());
                assert_eq!(lost_count, scheme.parity_blocks_per_stripe + 1);
                assert_eq!(limit, scheme.parity_blocks_per_stripe);
            }
            other => panic!("expected mixed data/parity unrecoverable error, got {other:?}"),
        }
    }

    /// Exercises mixed data/parity erasures in two committed epochs at once.
    /// Recovery must route each failed ordinal to its own sidecar and ignore
    /// unrelated damage from the other epoch.
    #[test]
    fn data_and_parity_erasures_across_two_object_epochs_recover_per_epoch_sidecar() {
        let scheme = scheme(4, 3, 5);
        let epoch_data_shards = data_shards_per_epoch(&scheme).unwrap();
        let epoch_blocks = usize::try_from(epoch_data_shards).unwrap();
        let object_blocks = (0..epoch_blocks * 2)
            .map(|idx| block((idx + 211) as u8))
            .collect::<Vec<_>>();
        let first_sidecar = sidecar_for_epoch_at(&scheme, 0, 0, &object_blocks[..epoch_blocks]);
        let second_sidecar = sidecar_for_epoch_at(
            &scheme,
            1,
            epoch_data_shards,
            &object_blocks[epoch_blocks..],
        );
        let scoped = scoped_two_object_two_epoch_sidecar_map(
            first_sidecar.blocks.len() as u64,
            second_sidecar.blocks.len() as u64,
            epoch_data_shards,
        );

        let epoch0_lost_ordinals = vec![0, 5];
        let epoch1_lost_ordinals = vec![epoch_data_shards + 3, epoch_data_shards + 8];
        let mut damaged_lbas = object_lbas_for_ordinals(&scoped, &epoch0_lost_ordinals);
        damaged_lbas.extend(object_lbas_for_ordinals(&scoped, &epoch1_lost_ordinals));
        damaged_lbas.push(parity_lba_for_shard(&scoped, 2, &first_sidecar, 0, 1));
        damaged_lbas.push(parity_lba_for_shard(&scoped, 4, &second_sidecar, 3, 2));

        for (ordinals, expected_neighborhood, expected_stripe) in
            [(&epoch0_lost_ordinals, 0, 0), (&epoch1_lost_ordinals, 1, 3)]
        {
            let addresses = ordinals
                .iter()
                .map(|ordinal| ordinal_to_stripe(*ordinal, &scheme).unwrap())
                .collect::<Vec<_>>();
            assert!(
                addresses
                    .iter()
                    .all(|address| address.neighborhood == expected_neighborhood),
                "fixture ordinals must target epoch {expected_neighborhood}"
            );
            assert!(
                addresses
                    .iter()
                    .all(|address| address.stripe_index == expected_stripe),
                "fixture ordinals must target stripe {expected_stripe}"
            );
            assert_eq!(
                addresses
                    .iter()
                    .map(|address| address.position)
                    .collect::<Vec<_>>(),
                vec![
                    StripePosition::Data { index: 0 },
                    StripePosition::Data { index: 1 },
                ],
                "fixture should lose two data shards in the target stripe"
            );
        }

        for (failed_ordinal, expected_sidecar_tape_file, expected_lost) in [
            (
                5,
                2,
                vec![
                    StripePosition::Data { index: 0 },
                    StripePosition::Data { index: 1 },
                    StripePosition::Parity { index: 1 },
                ],
            ),
            (
                epoch_data_shards + 8,
                4,
                vec![
                    StripePosition::Data { index: 0 },
                    StripePosition::Data { index: 1 },
                    StripePosition::Parity { index: 2 },
                ],
            ),
        ] {
            let failed_position = scoped.map.position_for_ordinal(failed_ordinal).unwrap();
            let mut raw = raw_tape_two_object_two_epoch_sidecars(
                &object_blocks[..epoch_blocks],
                &first_sidecar.blocks,
                &object_blocks[epoch_blocks..],
                &second_sidecar.blocks,
            );
            raw.unreadable_lbas = damaged_lbas.clone();

            let recovered = recover_object_block_from_sidecar(
                &mut raw,
                &scoped,
                &scheme,
                TAPE_UUID,
                BLOCK_SIZE,
                failed_position.tape_file_number,
                failed_position.block_within_file,
            )
            .unwrap_or_else(|err| {
                panic!(
                    "mixed data/parity erasures across epochs should recover ordinal {failed_ordinal}: {err}"
                )
            });

            assert_eq!(recovered.failed_ordinal, failed_ordinal);
            assert_eq!(
                recovered.recovered_block, object_blocks[failed_ordinal as usize],
                "mixed data/parity erasures recovered ordinal {failed_ordinal}"
            );
            assert_eq!(
                recovered.sidecar_tape_file_number, expected_sidecar_tape_file,
                "ordinal {failed_ordinal} should recover from its own epoch sidecar"
            );
            assert_eq!(
                recovered.lost_shards, expected_lost,
                "recovery should only count mixed erasures from the failed epoch stripe"
            );
            assert_eq!(
                recovered.lost_shards.len(),
                scheme.parity_blocks_per_stripe as usize,
                "fixture should recover exactly at the RS erasure limit"
            );
        }
    }

    #[test]
    fn same_stripe_data_and_multiple_parity_erasures_recover_at_rs_limit() {
        let scheme = scheme(4, 3, 5);
        let object_blocks = (0..20)
            .map(|idx| block((idx + 221) as u8))
            .collect::<Vec<_>>();
        let sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let failed_ordinal = 5;
        let failed_address = ordinal_to_stripe(failed_ordinal, &scheme).unwrap();
        assert_eq!(failed_address.stripe_index, 0);
        assert_eq!(failed_address.position, StripePosition::Data { index: 1 });

        let mut damaged_lbas = object_lbas_for_ordinals(&scoped, &[failed_ordinal]);
        damaged_lbas.push(parity_lba_for_shard(&scoped, 2, &sidecar, 0, 0));
        damaged_lbas.push(parity_lba_for_shard(&scoped, 2, &sidecar, 0, 2));
        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
        raw.unreadable_lbas = damaged_lbas;

        let recovered = recover_ordinal_from_sidecar(
            &mut raw,
            &scoped,
            &scheme,
            TAPE_UUID,
            BLOCK_SIZE,
            failed_ordinal,
        )
        .expect("one data erasure plus two same-stripe parity erasures should recover");

        assert_eq!(
            recovered.recovered_block,
            object_blocks[failed_ordinal as usize]
        );
        assert_eq!(
            recovered.lost_shards,
            vec![
                StripePosition::Data { index: 1 },
                StripePosition::Parity { index: 0 },
                StripePosition::Parity { index: 2 },
            ]
        );
        assert_eq!(
            recovered.lost_shards.len(),
            scheme.parity_blocks_per_stripe as usize,
            "fixture should recover exactly at the RS erasure limit"
        );
    }

    #[test]
    fn all_same_stripe_parity_shards_lost_with_failed_data_is_unrecoverable() {
        let scheme = scheme(4, 3, 5);
        let object_blocks = (0..20)
            .map(|idx| block((idx + 226) as u8))
            .collect::<Vec<_>>();
        let sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let failed_ordinal = 5;
        let failed_address = ordinal_to_stripe(failed_ordinal, &scheme).unwrap();
        assert_eq!(failed_address.stripe_index, 0);
        assert_eq!(failed_address.position, StripePosition::Data { index: 1 });

        let mut damaged_lbas = object_lbas_for_ordinals(&scoped, &[failed_ordinal]);
        for parity_index in 0..scheme.parity_blocks_per_stripe {
            damaged_lbas.push(parity_lba_for_shard(
                &scoped,
                2,
                &sidecar,
                failed_address.stripe_index,
                parity_index,
            ));
        }
        let clean_peer_ordinals = [0, 10, 15];
        let clean_peer_addresses = clean_peer_ordinals
            .iter()
            .map(|ordinal| ordinal_to_stripe(*ordinal, &scheme).unwrap())
            .collect::<Vec<_>>();
        assert!(
            clean_peer_addresses
                .iter()
                .all(|address| address.stripe_index == failed_address.stripe_index),
            "fixture keeps every non-failed data peer in the failed stripe"
        );
        assert_eq!(
            clean_peer_addresses
                .iter()
                .map(|address| address.position)
                .collect::<Vec<_>>(),
            vec![
                StripePosition::Data { index: 0 },
                StripePosition::Data { index: 2 },
                StripePosition::Data { index: 3 },
            ],
            "fixture leaves all non-failed data peers readable"
        );

        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
        raw.unreadable_lbas = damaged_lbas;

        let err = recover_ordinal_from_sidecar(
            &mut raw,
            &scoped,
            &scheme,
            TAPE_UUID,
            BLOCK_SIZE,
            failed_ordinal,
        )
        .expect_err("failed data plus every same-stripe parity shard exceeds m");

        match err {
            ParityError::Unrecoverable {
                stripe,
                lost_count,
                limit,
            } => {
                assert_eq!(stripe, failed_address);
                assert_eq!(lost_count, scheme.parity_blocks_per_stripe + 1);
                assert_eq!(limit, scheme.parity_blocks_per_stripe);
            }
            other => panic!("expected parity-overload unrecoverable error, got {other:?}"),
        }
    }

    /// Pins parity-overload accounting to the failed last stripe even when
    /// another stripe's parity shards are unreadable in the same raw source.
    #[test]
    fn last_stripe_parity_overload_ignores_other_stripe_parity_damage() {
        let scheme = scheme(4, 3, 5);
        let object_blocks = (0..20)
            .map(|idx| block((idx + 229) as u8))
            .collect::<Vec<_>>();
        let sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let failed_ordinal = 9;
        let failed_address = ordinal_to_stripe(failed_ordinal, &scheme).unwrap();
        assert_eq!(
            failed_address.stripe_index,
            scheme.stripes_per_neighborhood - 1,
            "fixture should target the last stripe by index"
        );
        assert_eq!(failed_address.position, StripePosition::Data { index: 1 });

        let mut damaged_lbas = object_lbas_for_ordinals(&scoped, &[failed_ordinal]);
        for parity_index in 0..scheme.parity_blocks_per_stripe {
            damaged_lbas.push(parity_lba_for_shard(
                &scoped,
                2,
                &sidecar,
                failed_address.stripe_index,
                parity_index,
            ));
            damaged_lbas.push(parity_lba_for_shard(&scoped, 2, &sidecar, 0, parity_index));
        }
        assert_eq!(
            damaged_lbas.len(),
            1 + (2 * usize::from(scheme.parity_blocks_per_stripe)),
            "fixture should damage one data shard plus every parity shard in two stripes"
        );
        let mut unique_damaged_lbas = damaged_lbas.clone();
        unique_damaged_lbas.sort_unstable();
        unique_damaged_lbas.dedup();
        assert_eq!(
            unique_damaged_lbas.len(),
            damaged_lbas.len(),
            "fixture parity damage should target distinct physical blocks"
        );
        let clean_peer_ordinals = [4, 14, 19];
        let clean_peer_addresses = clean_peer_ordinals
            .iter()
            .map(|ordinal| ordinal_to_stripe(*ordinal, &scheme).unwrap())
            .collect::<Vec<_>>();
        assert!(
            clean_peer_addresses
                .iter()
                .all(|address| address.stripe_index == failed_address.stripe_index),
            "fixture keeps every non-failed data peer in the failed last stripe"
        );
        assert_eq!(
            clean_peer_addresses
                .iter()
                .map(|address| address.position)
                .collect::<Vec<_>>(),
            vec![
                StripePosition::Data { index: 0 },
                StripePosition::Data { index: 2 },
                StripePosition::Data { index: 3 },
            ],
            "fixture leaves all non-failed data peers readable"
        );

        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
        raw.unreadable_lbas = damaged_lbas;

        let err = recover_ordinal_from_sidecar(
            &mut raw,
            &scoped,
            &scheme,
            TAPE_UUID,
            BLOCK_SIZE,
            failed_ordinal,
        )
        .expect_err("last-stripe failed data plus every same-stripe parity shard exceeds m");

        match err {
            ParityError::Unrecoverable {
                stripe,
                lost_count,
                limit,
            } => {
                assert_eq!(stripe, failed_address);
                assert_eq!(
                    lost_count,
                    scheme.parity_blocks_per_stripe + 1,
                    "unrelated parity damage from another stripe must not inflate lost_count"
                );
                assert_eq!(limit, scheme.parity_blocks_per_stripe);
            }
            other => panic!("expected last-stripe parity-overload error, got {other:?}"),
        }
    }

    /// Pins middle-stripe parity-overload accounting, including raw sources
    /// that also contain unrelated data/parity damage in other stripes.
    #[test]
    fn middle_stripe_parity_overload_ignores_other_stripe_data_damage() {
        let scheme = scheme(4, 3, 5);
        let object_blocks = (0..20)
            .map(|idx| block((idx + 234) as u8))
            .collect::<Vec<_>>();
        let sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let failed_ordinal = 7;
        let failed_address = ordinal_to_stripe(failed_ordinal, &scheme).unwrap();
        assert_eq!(
            failed_address.stripe_index,
            scheme.stripes_per_neighborhood / 2,
            "fixture should target the middle stripe by index"
        );
        assert_eq!(failed_address.position, StripePosition::Data { index: 1 });

        let clean_peer_ordinals = [2, 12, 17];
        let clean_peer_addresses = clean_peer_ordinals
            .iter()
            .map(|ordinal| ordinal_to_stripe(*ordinal, &scheme).unwrap())
            .collect::<Vec<_>>();
        assert!(
            clean_peer_addresses
                .iter()
                .all(|address| address.stripe_index == failed_address.stripe_index),
            "fixture keeps every non-failed data peer in the failed middle stripe"
        );
        assert_eq!(
            clean_peer_addresses
                .iter()
                .map(|address| address.position)
                .collect::<Vec<_>>(),
            vec![
                StripePosition::Data { index: 0 },
                StripePosition::Data { index: 2 },
                StripePosition::Data { index: 3 },
            ],
            "fixture leaves all non-failed middle-stripe data peers readable"
        );

        for (case_name, noise_ordinals, noise_parity_stripes, expected_noise_stripes) in [
            ("clean-neighbor-stripes", &[][..], &[][..], &[][..]),
            (
                "lower-and-upper-data-noise",
                &[0, 5, 10, 15, 4, 9, 14, 19][..],
                &[][..],
                &[0, scheme.stripes_per_neighborhood - 1][..],
            ),
            (
                "far-lower-only-mixed-data-and-parity-noise",
                &[0, 5, 10, 15][..],
                &[0][..],
                &[0][..],
            ),
            (
                "adjacent-mixed-data-and-parity-noise",
                &[1, 6, 11, 16, 3, 8, 13, 18][..],
                &[1, 3][..],
                &[
                    failed_address.stripe_index - 1,
                    failed_address.stripe_index + 1,
                ][..],
            ),
            (
                "adjacent-one-sided-mixed-data-and-parity-noise",
                &[1, 6, 11, 16][..],
                &[failed_address.stripe_index - 1][..],
                &[failed_address.stripe_index - 1][..],
            ),
            (
                "adjacent-upper-only-mixed-data-and-parity-noise",
                &[3, 8, 13, 18][..],
                &[failed_address.stripe_index + 1][..],
                &[failed_address.stripe_index + 1][..],
            ),
        ] {
            let noise_addresses = noise_ordinals
                .iter()
                .map(|ordinal| ordinal_to_stripe(*ordinal, &scheme).unwrap())
                .collect::<Vec<_>>();
            assert!(
                noise_addresses
                    .iter()
                    .all(|address| address.stripe_index != failed_address.stripe_index),
                "{case_name} noise must not target the failed middle stripe"
            );
            assert!(
                noise_parity_stripes
                    .iter()
                    .all(|stripe_index| *stripe_index != failed_address.stripe_index),
                "{case_name} parity noise must not target the failed middle stripe"
            );

            if !expected_noise_stripes.is_empty() {
                let mut noise_stripes = noise_addresses
                    .iter()
                    .map(|address| address.stripe_index)
                    .chain(noise_parity_stripes.iter().copied())
                    .collect::<Vec<_>>();
                noise_stripes.sort_unstable();
                noise_stripes.dedup();
                assert_eq!(
                    noise_stripes, expected_noise_stripes,
                    "{case_name} should include the expected unrelated noise stripes"
                );
            }

            let mut damaged_lbas = object_lbas_for_ordinals(&scoped, &[failed_ordinal]);
            damaged_lbas.extend(object_lbas_for_ordinals(&scoped, noise_ordinals));
            for parity_index in 0..scheme.parity_blocks_per_stripe {
                damaged_lbas.push(parity_lba_for_shard(
                    &scoped,
                    2,
                    &sidecar,
                    failed_address.stripe_index,
                    parity_index,
                ));
                for stripe_index in noise_parity_stripes {
                    damaged_lbas.push(parity_lba_for_shard(
                        &scoped,
                        2,
                        &sidecar,
                        *stripe_index,
                        parity_index,
                    ));
                }
            }
            assert_eq!(
                damaged_lbas.len(),
                1 + noise_ordinals.len()
                    + (usize::from(scheme.parity_blocks_per_stripe)
                        * (1 + noise_parity_stripes.len())),
                "{case_name} should damage the failed shard, configured noise, and every failed-stripe parity shard"
            );
            let mut unique_damaged_lbas = damaged_lbas.clone();
            unique_damaged_lbas.sort_unstable();
            unique_damaged_lbas.dedup();
            assert_eq!(
                unique_damaged_lbas.len(),
                damaged_lbas.len(),
                "{case_name} damage should target distinct physical blocks"
            );

            let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
            raw.unreadable_lbas = damaged_lbas;

            let err = match recover_ordinal_from_sidecar(
                &mut raw,
                &scoped,
                &scheme,
                TAPE_UUID,
                BLOCK_SIZE,
                failed_ordinal,
            ) {
                Ok(_) => {
                    panic!("{case_name}: middle-stripe parity overload should be unrecoverable")
                }
                Err(err) => err,
            };

            match err {
                ParityError::Unrecoverable {
                    stripe,
                    lost_count,
                    limit,
                } => {
                    assert_eq!(stripe, failed_address, "{case_name}");
                    assert_eq!(
                        lost_count,
                        scheme.parity_blocks_per_stripe + 1,
                        "{case_name}: unrelated data/parity noise must not inflate lost_count"
                    );
                    assert_eq!(limit, scheme.parity_blocks_per_stripe, "{case_name}");
                }
                other => {
                    panic!(
                        "{case_name}: expected middle-stripe parity-overload error, got {other:?}"
                    )
                }
            }
        }
    }

    #[test]
    fn data_and_parity_erasures_across_multiple_stripes_recover_per_failed_stripe() {
        let scheme = scheme(4, 3, 5);
        let object_blocks = (0..20)
            .map(|idx| block((idx + 231) as u8))
            .collect::<Vec<_>>();
        let sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let lost_ordinals = vec![5, 7];
        let mut damaged_lbas = object_lbas_for_ordinals(&scoped, &lost_ordinals);
        damaged_lbas.push(parity_lba_for_shard(&scoped, 2, &sidecar, 0, 1));
        damaged_lbas.push(parity_lba_for_shard(&scoped, 2, &sidecar, 2, 2));
        damaged_lbas.push(parity_lba_for_shard(&scoped, 2, &sidecar, 4, 0));

        for (failed_ordinal, expected_stripe, expected_lost) in [
            (
                5,
                0,
                vec![
                    StripePosition::Data { index: 1 },
                    StripePosition::Parity { index: 1 },
                ],
            ),
            (
                7,
                2,
                vec![
                    StripePosition::Data { index: 1 },
                    StripePosition::Parity { index: 2 },
                ],
            ),
        ] {
            let failed_address = ordinal_to_stripe(failed_ordinal, &scheme).unwrap();
            assert_eq!(failed_address.stripe_index, expected_stripe);
            assert_eq!(failed_address.position, StripePosition::Data { index: 1 });

            let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
            raw.unreadable_lbas = damaged_lbas.clone();

            let recovered = recover_ordinal_from_sidecar(
                &mut raw,
                &scoped,
                &scheme,
                TAPE_UUID,
                BLOCK_SIZE,
                failed_ordinal,
            )
            .unwrap_or_else(|err| {
                panic!(
                    "multi-stripe data/parity erasures should recover ordinal {failed_ordinal}: {err}"
                )
            });

            assert_eq!(
                recovered.recovered_block, object_blocks[failed_ordinal as usize],
                "multi-stripe data/parity erasures recovered ordinal {failed_ordinal}"
            );
            assert_eq!(
                recovered.lost_shards, expected_lost,
                "recovery should count only erasures from the failed stripe"
            );
        }
    }

    #[test]
    fn corrupt_clean_reading_peer_is_erasure_not_poison() {
        let scheme = scheme(2, 2, 1);
        let object_blocks = vec![block(9), block(10)];
        let sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let mut corrupt_object_blocks = object_blocks.clone();
        corrupt_object_blocks[0][7] ^= 0xA5;
        let mut raw = raw_tape(&corrupt_object_blocks, &sidecar.blocks);
        raw.unreadable_lbas.push(3); // failed ordinal 1.

        let recovered =
            recover_ordinal_from_sidecar(&mut raw, &scoped, &scheme, TAPE_UUID, BLOCK_SIZE, 1)
                .expect("parity-only survivors recover the failed block");

        assert_eq!(recovered.recovered_block, object_blocks[1]);
        assert_eq!(
            recovered.lost_shards,
            vec![
                StripePosition::Data { index: 0 },
                StripePosition::Data { index: 1 },
            ]
        );
    }

    #[test]
    fn corrupt_clean_reading_parity_peer_is_erasure_not_poison() {
        let scheme = scheme(2, 2, 1);
        let object_blocks = vec![block(11), block(12)];
        let mut sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let index_blocks = sidecar.header.shard_index_block_count as usize;
        sidecar.blocks[index_blocks][5] ^= 0x5A; // Corrupt parity shard 0 only.
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
        raw.unreadable_lbas.push(3); // failed ordinal 1.

        let recovered =
            recover_ordinal_from_sidecar(&mut raw, &scoped, &scheme, TAPE_UUID, BLOCK_SIZE, 1)
                .expect("remaining parity shard plus data peer recover the failed block");

        assert_eq!(recovered.recovered_block, object_blocks[1]);
        assert_eq!(
            recovered.lost_shards,
            vec![
                StripePosition::Data { index: 1 },
                StripePosition::Parity { index: 0 },
            ]
        );
    }

    #[test]
    fn reconstructed_block_must_match_sidecar_data_crc() {
        let scheme = scheme(2, 1, 1);
        let object_blocks = vec![block(31), block(32)];
        let mut data_crcs = object_blocks
            .iter()
            .map(|block| data_shard_crc64(block))
            .collect::<Vec<_>>();
        data_crcs[1] ^= 0xA5A5_A5A5_A5A5_A5A5;
        let sidecar = sidecar_for_epoch_with_data_crcs(&scheme, &object_blocks, data_crcs);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
        raw.unreadable_lbas.push(3); // failed ordinal 1.

        let err =
            recover_ordinal_from_sidecar(&mut raw, &scoped, &scheme, TAPE_UUID, BLOCK_SIZE, 1)
                .expect_err("reconstructed bytes must still pass the sidecar data CRC");

        match err {
            ParityError::Unrecoverable {
                lost_count, limit, ..
            } => {
                assert_eq!(lost_count, 1);
                assert_eq!(limit, 1);
            }
            other => panic!("expected CRC-gated unrecoverable error, got {other:?}"),
        }
    }

    #[test]
    fn simultaneous_data_and_parity_peer_corruption_recovers_at_rs_limit() {
        let scheme = scheme(3, 3, 1);
        let object_blocks = vec![block(21), block(22), block(23)];
        let mut sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let index_blocks = sidecar.header.shard_index_block_count as usize;
        sidecar.blocks[index_blocks][9] ^= 0x6C; // Corrupt parity shard 0.
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let mut corrupt_object_blocks = object_blocks.clone();
        corrupt_object_blocks[0][11] ^= 0xD3; // Corrupt clean-reading data peer 0.
        let mut raw = raw_tape(&corrupt_object_blocks, &sidecar.blocks);
        raw.unreadable_lbas.push(4); // Failed ordinal 2.

        let recovered =
            recover_ordinal_from_sidecar(&mut raw, &scoped, &scheme, TAPE_UUID, BLOCK_SIZE, 2)
                .expect("failed data plus one data peer and one parity peer remains recoverable");

        assert_eq!(recovered.recovered_block, object_blocks[2]);
        assert_eq!(
            recovered.lost_shards,
            vec![
                StripePosition::Data { index: 0 },
                StripePosition::Data { index: 2 },
                StripePosition::Parity { index: 0 },
            ]
        );
    }

    #[test]
    fn contiguous_damage_up_to_m_times_s_recovers_every_affected_ordinal() {
        let scheme = scheme(4, 3, 5);
        let object_blocks = (0..20)
            .map(|idx| block((idx + 1) as u8))
            .collect::<Vec<_>>();
        let sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let damage_len = scheme.contiguous_damage_threshold();
        assert_eq!(damage_len, 15);
        let damaged_lbas = object_damage_lbas(&scoped, 1, 0, damage_len);

        for failed_ordinal in 0..damage_len {
            let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
            raw.unreadable_lbas = damaged_lbas.clone();

            let recovered = recover_ordinal_from_sidecar(
                &mut raw,
                &scoped,
                &scheme,
                TAPE_UUID,
                BLOCK_SIZE,
                failed_ordinal,
            )
            .unwrap_or_else(|err| {
                panic!(
                    "ordinal {failed_ordinal} within S*m contiguous damage should recover: {err}"
                )
            });

            assert_eq!(
                recovered.recovered_block, object_blocks[failed_ordinal as usize],
                "ordinal {failed_ordinal} recovered bytes"
            );
            assert_eq!(
                recovered.lost_shards.len(),
                scheme.parity_blocks_per_stripe as usize,
                "ordinal {failed_ordinal} should lose exactly m data peers"
            );
        }
    }

    #[test]
    fn contiguous_damage_length_50_recovers_when_within_threshold() {
        let scheme = scheme(2, 1, 50);
        let object_blocks = (0..100)
            .map(|idx| block((idx % 251 + 1) as u8))
            .collect::<Vec<_>>();
        let sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let damage_len = 50;
        assert_eq!(scheme.contiguous_damage_threshold(), damage_len);
        let damaged_lbas = object_damage_lbas(&scoped, 1, 0, damage_len);

        for failed_ordinal in 0..damage_len {
            let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
            raw.unreadable_lbas = damaged_lbas.clone();

            let recovered = recover_ordinal_from_sidecar(
                &mut raw,
                &scoped,
                &scheme,
                TAPE_UUID,
                BLOCK_SIZE,
                failed_ordinal,
            )
            .unwrap_or_else(|err| {
                panic!("ordinal {failed_ordinal} in length-50 damage run should recover: {err}")
            });

            assert_eq!(
                recovered.recovered_block, object_blocks[failed_ordinal as usize],
                "ordinal {failed_ordinal} recovered bytes"
            );
            assert_eq!(
                recovered.lost_shards,
                vec![StripePosition::Data { index: 0 }],
                "length-50 run with S=50 should lose one block per stripe"
            );
        }
    }

    #[test]
    fn contiguous_damage_required_length_matrix_matches_recovery_threshold() {
        let scheme = scheme(4, 3, 5);
        let object_blocks = (0..20)
            .map(|idx| block((idx + 91) as u8))
            .collect::<Vec<_>>();
        let sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let s = u64::from(scheme.stripes_per_neighborhood);
        let recoverable_cases = [
            (1, 1usize, "single block"),
            (s - 1, 1, "S-1"),
            (s, 1, "S"),
            (2 * s, 2, "2S"),
            (scheme.contiguous_damage_threshold(), 3, "mS"),
        ];

        for (damage_len, expected_lost, label) in recoverable_cases {
            let damaged_lbas = object_damage_lbas(&scoped, 1, 0, damage_len);
            for failed_ordinal in 0..damage_len {
                let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
                raw.unreadable_lbas = damaged_lbas.clone();

                let recovered = recover_ordinal_from_sidecar(
                    &mut raw,
                    &scoped,
                    &scheme,
                    TAPE_UUID,
                    BLOCK_SIZE,
                    failed_ordinal,
                )
                .unwrap_or_else(|err| {
                    panic!("{label} damage should recover ordinal {failed_ordinal}: {err}")
                });

                assert_eq!(
                    recovered.recovered_block, object_blocks[failed_ordinal as usize],
                    "{label} recovered ordinal {failed_ordinal}"
                );
                assert_eq!(
                    recovered.lost_shards.len(),
                    expected_lost,
                    "{label} should lose {expected_lost} shards in the failed stripe"
                );
            }
        }

        let too_long = scheme.contiguous_damage_threshold() + 1;
        let damaged_lbas = object_damage_lbas(&scoped, 1, 0, too_long);
        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
        raw.unreadable_lbas = damaged_lbas;

        let err =
            recover_ordinal_from_sidecar(&mut raw, &scoped, &scheme, TAPE_UUID, BLOCK_SIZE, 0)
                .expect_err("mS+1 damage should overload stripe 0");

        match err {
            ParityError::Unrecoverable {
                lost_count, limit, ..
            } => {
                assert_eq!(lost_count, scheme.parity_blocks_per_stripe + 1);
                assert_eq!(limit, scheme.parity_blocks_per_stripe);
            }
            other => panic!("expected mS+1 unrecoverable threshold error, got {other:?}"),
        }
    }

    #[test]
    fn contiguous_damage_threshold_is_start_invariant_within_and_across_epochs() {
        let scheme = scheme(4, 3, 5);
        let epoch_data_shards = data_shards_per_epoch(&scheme).unwrap();
        let epoch_blocks = usize::try_from(epoch_data_shards).unwrap();
        let object_blocks = (0..epoch_blocks * 2)
            .map(|idx| block((idx + 151) as u8))
            .collect::<Vec<_>>();
        let first_sidecar = sidecar_for_epoch_at(&scheme, 0, 0, &object_blocks[..epoch_blocks]);
        let second_sidecar = sidecar_for_epoch_at(
            &scheme,
            1,
            epoch_data_shards,
            &object_blocks[epoch_blocks..],
        );
        let scoped = scoped_two_epoch_sidecar_map(
            first_sidecar.blocks.len() as u64,
            second_sidecar.blocks.len() as u64,
            object_blocks.len() as u64,
            epoch_data_shards,
        );
        let threshold = scheme.contiguous_damage_threshold();
        assert_eq!(threshold, 15);

        for (start_body_lba, label) in [
            (2, "mid-stripe start inside epoch 0"),
            (epoch_data_shards - 1, "start at final block of epoch 0"),
        ] {
            let damaged_lbas = object_damage_lbas(&scoped, 1, start_body_lba, threshold);

            for failed_ordinal in start_body_lba..start_body_lba + threshold {
                let mut raw = raw_tape_two_epoch_sidecars(
                    &object_blocks,
                    &first_sidecar.blocks,
                    &second_sidecar.blocks,
                );
                raw.unreadable_lbas = damaged_lbas.clone();

                let recovered = recover_ordinal_from_sidecar(
                    &mut raw,
                    &scoped,
                    &scheme,
                    TAPE_UUID,
                    BLOCK_SIZE,
                    failed_ordinal,
                )
                .unwrap_or_else(|err| {
                    panic!(
                        "{label}: threshold-length damage should recover ordinal {failed_ordinal}: {err}"
                    )
                });

                let failed_stripe = ordinal_to_stripe(failed_ordinal, &scheme).unwrap();
                let expected_lost = (start_body_lba..start_body_lba + threshold)
                    .filter_map(|ordinal| {
                        let address = ordinal_to_stripe(ordinal, &scheme).unwrap();
                        (address.neighborhood == failed_stripe.neighborhood
                            && address.stripe_index == failed_stripe.stripe_index)
                            .then_some(address.position)
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    recovered.recovered_block, object_blocks[failed_ordinal as usize],
                    "{label}: ordinal {failed_ordinal} recovered bytes"
                );
                assert_eq!(
                    recovered.lost_shards, expected_lost,
                    "{label}: ordinal {failed_ordinal} should only count damaged data in its own epoch stripe"
                );
                assert!(
                    recovered.lost_shards.len() <= scheme.parity_blocks_per_stripe as usize,
                    "{label}: threshold damage must not exceed the parity limit in any epoch stripe"
                );
            }
        }

        let overloaded_start = 2;
        let too_long = threshold + 1;
        let damaged_lbas = object_damage_lbas(&scoped, 1, overloaded_start, too_long);
        let mut raw = raw_tape_two_epoch_sidecars(
            &object_blocks,
            &first_sidecar.blocks,
            &second_sidecar.blocks,
        );
        raw.unreadable_lbas = damaged_lbas;

        let err = recover_ordinal_from_sidecar(
            &mut raw,
            &scoped,
            &scheme,
            TAPE_UUID,
            BLOCK_SIZE,
            overloaded_start,
        )
        .expect_err("off-zero mS+1 damage should overload the starting stripe");

        match err {
            ParityError::Unrecoverable {
                lost_count, limit, ..
            } => {
                assert_eq!(lost_count, scheme.parity_blocks_per_stripe + 1);
                assert_eq!(limit, scheme.parity_blocks_per_stripe);
            }
            other => panic!("expected off-zero unrecoverable threshold error, got {other:?}"),
        }
    }

    /// Guards section 9.2's per-epoch stripe accounting: a globally over-threshold
    /// damage run can still recover when the epoch boundary keeps each stripe
    /// at or below the RS erasure limit.
    #[test]
    fn m_s_plus_one_damage_straddling_epoch_boundary_recovers_when_each_epoch_stripe_is_bounded() {
        let scheme = scheme(4, 3, 5);
        let epoch_data_shards = data_shards_per_epoch(&scheme).unwrap();
        let epoch_blocks = usize::try_from(epoch_data_shards).unwrap();
        let object_blocks = (0..epoch_blocks * 2)
            .map(|idx| block((idx + 181) as u8))
            .collect::<Vec<_>>();
        let first_sidecar = sidecar_for_epoch_at(&scheme, 0, 0, &object_blocks[..epoch_blocks]);
        let second_sidecar = sidecar_for_epoch_at(
            &scheme,
            1,
            epoch_data_shards,
            &object_blocks[epoch_blocks..],
        );
        let scoped = scoped_two_epoch_sidecar_map(
            first_sidecar.blocks.len() as u64,
            second_sidecar.blocks.len() as u64,
            object_blocks.len() as u64,
            epoch_data_shards,
        );
        let threshold = scheme.contiguous_damage_threshold();
        let damage_start = epoch_data_shards - 2;
        let damage_len = threshold + 1;
        assert_eq!(threshold, 15);
        assert_eq!(damage_start, 18);
        assert_eq!(damage_len, 16);

        let damaged_lbas = object_damage_lbas(&scoped, 1, damage_start, damage_len);
        for failed_ordinal in damage_start..damage_start + damage_len {
            let mut raw = raw_tape_two_epoch_sidecars(
                &object_blocks,
                &first_sidecar.blocks,
                &second_sidecar.blocks,
            );
            raw.unreadable_lbas = damaged_lbas.clone();

            let recovered = recover_ordinal_from_sidecar(
                &mut raw,
                &scoped,
                &scheme,
                TAPE_UUID,
                BLOCK_SIZE,
                failed_ordinal,
            )
            .unwrap_or_else(|err| {
                panic!("epoch-boundary mS+1 damage should recover ordinal {failed_ordinal}: {err}")
            });

            let failed_stripe = ordinal_to_stripe(failed_ordinal, &scheme).unwrap();
            let expected_lost = (damage_start..damage_start + damage_len)
                .filter_map(|ordinal| {
                    let address = ordinal_to_stripe(ordinal, &scheme).unwrap();
                    (address.neighborhood == failed_stripe.neighborhood
                        && address.stripe_index == failed_stripe.stripe_index)
                        .then_some(address.position)
                })
                .collect::<Vec<_>>();
            assert_eq!(
                recovered.recovered_block, object_blocks[failed_ordinal as usize],
                "epoch-boundary mS+1 damage recovered ordinal {failed_ordinal}"
            );
            assert_eq!(
                recovered.lost_shards, expected_lost,
                "epoch-boundary mS+1 damage should only count losses from the failed epoch stripe"
            );
            assert!(
                recovered.lost_shards.len() <= scheme.parity_blocks_per_stripe as usize,
                "epoch-boundary mS+1 damage should stay within the per-epoch stripe erasure limit"
            );
        }
    }

    /// Same per-epoch mS+1 invariant as the single-object fixture above, but
    /// with each epoch's data in its own object tape file and recovery routed
    /// through the object-local address surface.
    #[test]
    fn m_s_plus_one_damage_across_object_epoch_boundary_recovers_when_each_epoch_stripe_is_bounded()
    {
        let scheme = scheme(4, 3, 5);
        let epoch_data_shards = data_shards_per_epoch(&scheme).unwrap();
        let epoch_blocks = usize::try_from(epoch_data_shards).unwrap();
        let object_blocks = (0..epoch_blocks * 2)
            .map(|idx| block((idx + 201) as u8))
            .collect::<Vec<_>>();
        let first_sidecar = sidecar_for_epoch_at(&scheme, 0, 0, &object_blocks[..epoch_blocks]);
        let second_sidecar = sidecar_for_epoch_at(
            &scheme,
            1,
            epoch_data_shards,
            &object_blocks[epoch_blocks..],
        );
        let scoped = scoped_two_object_two_epoch_sidecar_map(
            first_sidecar.blocks.len() as u64,
            second_sidecar.blocks.len() as u64,
            epoch_data_shards,
        );
        let threshold = scheme.contiguous_damage_threshold();
        let damage_start = epoch_data_shards - 2;
        let damage_len = threshold + 1;
        assert_eq!(threshold, 15);
        assert_eq!(damage_start, 18);
        assert_eq!(damage_len, 16);

        let epoch0_tail_position = scoped
            .map
            .position_for_ordinal(epoch_data_shards - 1)
            .unwrap();
        let epoch1_head_position = scoped.map.position_for_ordinal(epoch_data_shards).unwrap();
        assert_eq!(epoch0_tail_position.tape_file_number, 1);
        assert_eq!(epoch1_head_position.tape_file_number, 3);

        let damaged_ordinals = (damage_start..damage_start + damage_len).collect::<Vec<_>>();
        let damaged_lbas = object_lbas_for_ordinals(&scoped, &damaged_ordinals);
        assert!(
            damaged_lbas.windows(2).any(|pair| pair[1] > pair[0] + 1),
            "fixture must cross from object tape file 1 to object tape file 3 with non-data records between"
        );

        for failed_ordinal in damaged_ordinals {
            let failed_position = scoped.map.position_for_ordinal(failed_ordinal).unwrap();
            let mut raw = raw_tape_two_object_two_epoch_sidecars(
                &object_blocks[..epoch_blocks],
                &first_sidecar.blocks,
                &object_blocks[epoch_blocks..],
                &second_sidecar.blocks,
            );
            raw.unreadable_lbas = damaged_lbas.clone();

            let recovered = recover_object_block_from_sidecar(
                &mut raw,
                &scoped,
                &scheme,
                TAPE_UUID,
                BLOCK_SIZE,
                failed_position.tape_file_number,
                failed_position.block_within_file,
            )
            .unwrap_or_else(|err| {
                panic!(
                    "multi-object epoch-boundary mS+1 damage should recover ordinal {failed_ordinal}: {err}"
                )
            });

            let failed_stripe = ordinal_to_stripe(failed_ordinal, &scheme).unwrap();
            let expected_lost = (damage_start..damage_start + damage_len)
                .filter_map(|ordinal| {
                    let address = ordinal_to_stripe(ordinal, &scheme).unwrap();
                    (address.neighborhood == failed_stripe.neighborhood
                        && address.stripe_index == failed_stripe.stripe_index)
                        .then_some(address.position)
                })
                .collect::<Vec<_>>();
            let expected_sidecar_tape_file = if failed_ordinal < epoch_data_shards {
                2
            } else {
                4
            };
            assert_eq!(recovered.failed_ordinal, failed_ordinal);
            assert_eq!(
                recovered.recovered_block, object_blocks[failed_ordinal as usize],
                "multi-object epoch-boundary damage recovered ordinal {failed_ordinal}"
            );
            assert_eq!(
                recovered.sidecar_tape_file_number, expected_sidecar_tape_file,
                "ordinal {failed_ordinal} should recover from its own epoch sidecar"
            );
            assert_eq!(
                recovered.lost_shards, expected_lost,
                "multi-object epoch-boundary mS+1 damage should only count losses from the failed epoch stripe"
            );
            assert!(
                recovered.lost_shards.len() <= scheme.parity_blocks_per_stripe as usize,
                "multi-object epoch-boundary damage should stay within the per-epoch stripe erasure limit"
            );
        }
    }

    /// Extends the epoch-boundary recovery gate to a longer chain:
    /// BOT bootstrap, three object/sidecar epoch pairs, and a final bootstrap.
    /// The global damage crosses two object/epoch boundaries, but each
    /// epoch-local stripe remains at or below the RS limit.
    #[test]
    fn damage_across_two_object_epoch_boundaries_recovers_per_epoch_sidecar() {
        let scheme = scheme(4, 4, 5);
        let epoch_data_shards = data_shards_per_epoch(&scheme).unwrap();
        let epoch_blocks = usize::try_from(epoch_data_shards).unwrap();
        let object_blocks = (0..epoch_blocks * 3)
            .map(|idx| block((idx + 31) as u8))
            .collect::<Vec<_>>();
        let first_sidecar = sidecar_for_epoch_at(&scheme, 0, 0, &object_blocks[..epoch_blocks]);
        let second_sidecar = sidecar_for_epoch_at(
            &scheme,
            1,
            epoch_data_shards,
            &object_blocks[epoch_blocks..epoch_blocks * 2],
        );
        let third_sidecar = sidecar_for_epoch_at(
            &scheme,
            2,
            epoch_data_shards * 2,
            &object_blocks[epoch_blocks * 2..],
        );
        let scoped = scoped_three_object_three_epoch_sidecar_map(
            first_sidecar.blocks.len() as u64,
            second_sidecar.blocks.len() as u64,
            third_sidecar.blocks.len() as u64,
            epoch_data_shards,
        );
        let threshold = scheme.contiguous_damage_threshold();
        let damage_start = epoch_data_shards - 2;
        let damage_len = epoch_data_shards + 4;
        assert_eq!(epoch_data_shards, 20);
        assert_eq!(threshold, 20);
        assert_eq!(damage_start, 18);
        assert_eq!(damage_len, 24);

        let entries = scoped.map.entries();
        assert_eq!(entries.len(), 8);
        assert_eq!(entries[0].kind, TapeFileKind::Bootstrap);
        assert_eq!(entries[7].kind, TapeFileKind::Bootstrap);
        for (ordinal, expected_tape_file) in [
            (epoch_data_shards - 1, 1),
            (epoch_data_shards, 3),
            (epoch_data_shards * 2 - 1, 3),
            (epoch_data_shards * 2, 5),
        ] {
            let position = scoped.map.position_for_ordinal(ordinal).unwrap();
            assert_eq!(
                position.tape_file_number, expected_tape_file,
                "ordinal {ordinal} should live in object tape file {expected_tape_file}"
            );
        }

        let damaged_ordinals = (damage_start..damage_start + damage_len).collect::<Vec<_>>();
        let damaged_lbas = object_lbas_for_ordinals(&scoped, &damaged_ordinals);
        let boundary_gap_count = damaged_lbas
            .windows(2)
            .filter(|pair| pair[1] > pair[0] + 1)
            .count();
        assert_eq!(
            boundary_gap_count, 2,
            "fixture must cross two object boundaries with sidecar/filemark records between"
        );

        let mut saw_middle_epoch_rs_limit = false;
        for failed_ordinal in damaged_ordinals {
            let failed_position = scoped.map.position_for_ordinal(failed_ordinal).unwrap();
            let mut raw = raw_tape_three_object_three_epoch_sidecars(
                &object_blocks[..epoch_blocks],
                &first_sidecar.blocks,
                &object_blocks[epoch_blocks..epoch_blocks * 2],
                &second_sidecar.blocks,
                &object_blocks[epoch_blocks * 2..],
                &third_sidecar.blocks,
            );
            raw.unreadable_lbas = damaged_lbas.clone();

            let recovered = recover_object_block_from_sidecar(
                &mut raw,
                &scoped,
                &scheme,
                TAPE_UUID,
                BLOCK_SIZE,
                failed_position.tape_file_number,
                failed_position.block_within_file,
            )
            .unwrap_or_else(|err| {
                panic!("three-epoch boundary damage should recover ordinal {failed_ordinal}: {err}")
            });

            let failed_stripe = ordinal_to_stripe(failed_ordinal, &scheme).unwrap();
            let expected_lost = (damage_start..damage_start + damage_len)
                .filter_map(|ordinal| {
                    let address = ordinal_to_stripe(ordinal, &scheme).unwrap();
                    (address.neighborhood == failed_stripe.neighborhood
                        && address.stripe_index == failed_stripe.stripe_index)
                        .then_some(address.position)
                })
                .collect::<Vec<_>>();
            let expected_sidecar_tape_file = match failed_stripe.neighborhood {
                0 => 2,
                1 => 4,
                2 => 6,
                other => panic!("unexpected epoch {other} in fixture"),
            };
            if failed_stripe.neighborhood == 1 {
                saw_middle_epoch_rs_limit = true;
                assert_eq!(
                    expected_lost.len(),
                    scheme.parity_blocks_per_stripe as usize,
                    "middle epoch should recover exactly at the RS erasure limit"
                );
            }

            assert_eq!(recovered.failed_ordinal, failed_ordinal);
            assert_eq!(
                recovered.recovered_block, object_blocks[failed_ordinal as usize],
                "three-epoch boundary damage recovered ordinal {failed_ordinal}"
            );
            assert_eq!(
                recovered.sidecar_tape_file_number, expected_sidecar_tape_file,
                "ordinal {failed_ordinal} should recover from its own epoch sidecar"
            );
            assert_eq!(
                recovered.lost_shards, expected_lost,
                "three-epoch boundary damage should only count losses from the failed epoch stripe"
            );
            assert!(
                recovered.lost_shards.len() <= scheme.parity_blocks_per_stripe as usize,
                "three-epoch boundary damage should stay within the per-epoch stripe erasure limit"
            );
        }

        assert!(
            saw_middle_epoch_rs_limit,
            "fixture must exercise a fully damaged middle epoch stripe at the RS limit"
        );
    }

    #[test]
    fn contiguous_damage_over_m_times_s_is_unrecoverable_for_overloaded_stripe() {
        let scheme = scheme(4, 3, 5);
        let object_blocks = (0..20)
            .map(|idx| block((idx + 41) as u8))
            .collect::<Vec<_>>();
        let sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let scoped = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64);
        let damage_len = scheme.contiguous_damage_threshold() + 1;
        let damaged_lbas = object_damage_lbas(&scoped, 1, 0, damage_len);
        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);
        raw.unreadable_lbas = damaged_lbas;

        let err =
            recover_ordinal_from_sidecar(&mut raw, &scoped, &scheme, TAPE_UUID, BLOCK_SIZE, 0)
                .expect_err("S*m+1 contiguous damage should overload stripe 0");

        match err {
            ParityError::Unrecoverable {
                lost_count, limit, ..
            } => {
                assert_eq!(lost_count, 4);
                assert_eq!(limit, scheme.parity_blocks_per_stripe);
            }
            other => panic!("expected unrecoverable overloaded stripe, got {other:?}"),
        }
    }

    #[test]
    fn pending_epoch_is_rejected_before_tape_reads() {
        let scheme = scheme(2, 1, 2);
        let object_blocks = vec![block(1), block(2), block(3), block(4)];
        let sidecar = sidecar_for_epoch(&scheme, &object_blocks);
        let map = scoped_map(sidecar.blocks.len() as u64, object_blocks.len() as u64).map;
        let scoped = ScopedFilemarkMap::from_catalog(map, 2);
        let mut raw = raw_tape(&object_blocks, &sidecar.blocks);

        let err =
            recover_ordinal_from_sidecar(&mut raw, &scoped, &scheme, TAPE_UUID, BLOCK_SIZE, 2)
                .expect_err("ordinal at watermark is pending");
        assert!(matches!(
            err,
            ParityError::UnrecoverablePendingEpoch {
                failed_ordinal: 2,
                watermark: 2
            }
        ));
        assert_eq!(raw.configured_block_size, None);
        assert_eq!(raw.position().unwrap(), PhysicalPositionHint::new(0));
    }
}
