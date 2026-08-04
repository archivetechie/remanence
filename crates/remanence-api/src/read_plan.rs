//! The `PlanBatchRead` RPC — design-read-ordering.md §§6.3, 8.3, 9 and
//! 11; prompt P5.
//!
//! The handler resolves cartridge facts by the §6.3 matrix, fetches the
//! volume's cached wrap map through the P4 serve path, and calls the
//! pure `remanence-order` planning core. It owns every wire concern the
//! pure core deliberately does not: statuses, the §11 precedence order,
//! `written_extent_lba` validation, the `MAX_TARGETS` degraded
//! fallback, and the `google.rpc.BadRequest` details on malformed
//! requests.
//!
//! **The read surface is unchanged.** This RPC returns an ordering; the
//! caller then issues its own reads in that order, one per call, and
//! remanence never learns at read time that the calls are related. That
//! is what makes a plan advice rather than a contract.
//!
//! **Precedence is fixed** (§11): malformed-request validation, then
//! cartridge facts, then map validity and trust, then map coverage,
//! then the batch-size fallback, then the zero/one-target fast path.
//! Later stages never mask an earlier result. The malformed checks that
//! need `written_extent_lba` run as soon as that fact is known — it is
//! a request field, so when it is present and non-zero they run in the
//! malformed stage — and never later than map validity.
//!
//! **Unavailability is a normal, cacheable result**, not an RPC error.
//! `INVALID_ARGUMENT` is reserved for malformed requests and carries a
//! `google.rpc.BadRequest` naming the offending target by index —
//! `tag` is opaque bytes and may not be printable.
//!
//! **The handler never writes.** Consulting map validity allocates no
//! calibration generation: `calibration_generation = 0` means "no
//! calibration history" and is a valid cache key, reported as-is.

use prost::Message;
use remanence_order::{
    hop_ns, lookup_geometry, lookup_media_code, physical_position, plan, GeometryLookup, Objective,
    PlanInput, PositionError, ReadTarget as OrderReadTarget, StructuralRow, WrapMap, MAX_TARGETS,
    PUBLISHED_PRIORS, STRUCTURAL_TABLE, UNSUPPORTED_TABLE,
};
use remanence_state::{CalibrationControlStore, CatalogIndex};
use tonic::{Code, Request, Response, Status};

use crate::calibration::{
    media_code_of, servable_wrap_map, WrapMapServeOutcome, WrapMapServeRefusal,
};
use crate::{authorize_request, pb, pb_rpc, ApiState, AuthPermission};

/// Canonical type URL for the vendored `google.rpc.BadRequest` detail.
const BAD_REQUEST_TYPE_URL: &str = "type.googleapis.com/google.rpc.BadRequest";

/// The §6.4 high-dispersion caveat, stated on the response as the design
/// requires ("state in `detail` that the estimate is unreliable").
const DISPERSED_DETAIL: &str = "completed-wrap spans are highly dispersed; \
     the EOD-wrap estimate is unreliable for this volume";

/// Implementation of the Layer 5 read-plan service.
#[derive(Clone)]
pub struct ReadPlanApi {
    pub(crate) state: ApiState,
}

#[tonic::async_trait]
impl pb::read_plan_service_server::ReadPlanService for ReadPlanApi {
    async fn plan_batch_read(
        &self,
        request: Request<pb::PlanBatchReadRequest>,
    ) -> Result<Response<pb::PlanBatchReadResponse>, Status> {
        authorize_request(&request, AuthPermission::Read)?;
        let index = self.state.index()?;
        let store = self.state.calibration_store().clone();
        let message = request.into_inner();
        // The solver is CPU-bound at up to MAX_TARGETS targets; keep it
        // off the async executor.
        let response =
            tokio::task::spawn_blocking(move || plan_batch_read_response(&index, &store, message))
                .await
                .map_err(|err| Status::internal(format!("plan task failed: {err}")))??;
        Ok(Response::new(response))
    }
}

// ---------------------------------------------------------------------
//  Malformed-request errors: INVALID_ARGUMENT + google.rpc.BadRequest
// ---------------------------------------------------------------------

fn field_violation(
    field: impl Into<String>,
    description: impl Into<String>,
) -> pb_rpc::bad_request::FieldViolation {
    pb_rpc::bad_request::FieldViolation {
        field: field.into(),
        description: description.into(),
    }
}

/// Build the `INVALID_ARGUMENT` status carrying every collected
/// violation as a `google.rpc.BadRequest` in `grpc-status-details-bin`,
/// naming offending targets by index (`tag` may not be printable).
fn invalid_argument(violations: Vec<pb_rpc::bad_request::FieldViolation>) -> Status {
    debug_assert!(!violations.is_empty());
    let message = match violations.as_slice() {
        [only] => format!("{}: {}", only.field, only.description),
        many => format!(
            "{} request violations; first: {}: {}",
            many.len(),
            many[0].field,
            many[0].description
        ),
    };
    let bad_request = pb_rpc::BadRequest {
        field_violations: violations,
    };
    let details = pb_rpc::Status {
        code: Code::InvalidArgument as i32,
        message: message.clone(),
        details: vec![prost_types::Any {
            type_url: BAD_REQUEST_TYPE_URL.to_string(),
            value: bad_request.encode_to_vec(),
        }],
    };
    Status::with_details(
        Code::InvalidArgument,
        message,
        details.encode_to_vec().into(),
    )
}

// ---------------------------------------------------------------------
//  Cartridge-fact resolution — the §6.3 matrix
// ---------------------------------------------------------------------

/// What the §6.3 resolution matrix concluded.
enum FactOutcome {
    /// A supported structural row; planning may proceed.
    Supported(&'static StructuralRow),
    /// A recognised-but-unsupported key — a request-derived negative.
    UnsupportedFormat {
        /// Human-readable evidence, never parsed.
        detail: String,
    },
    /// No row resolves and none is derivable — a request-derived
    /// negative.
    UnknownFormat {
        /// Human-readable evidence, never parsed.
        detail: String,
    },
    /// A complete caller pair whose components are both individually
    /// recognised but whose combination does not exist — caller
    /// self-contradiction, malformed (§11).
    ImpossiblePair {
        /// Names the pair.
        detail: String,
    },
}

/// Outcome of resolving `(cartridge_generation, recording_format,
/// voltag)` as one pair through the canonical §6.3 matrix.
struct FactResolution {
    outcome: FactOutcome,
    /// The definite pair the resolution concluded, both halves —
    /// present for supported and recognised-unsupported keys.
    resolved_key: Option<(String, String)>,
    /// Diagnostic only (§9): the caller's asserted facts disagreed with
    /// the barcode. Changes neither plan nor status.
    format_disagreement: bool,
}

/// What the voltag's two-character suffix contributes as evidence.
enum SuffixEvidence {
    /// No readable suffix (voltag absent, too short, or not ASCII).
    Absent,
    /// A suffix in the canonical media-code table. `supported` is false
    /// exactly for `M8`.
    Recognised {
        code: String,
        generation: &'static str,
        format: &'static str,
        supported: bool,
        row: Option<&'static StructuralRow>,
        unsupported_reason: Option<&'static str>,
    },
    /// A suffix we have never heard of.
    Unrecognised { code: String },
}

fn suffix_evidence(voltag: &str) -> SuffixEvidence {
    let Some(code) = media_code_of(voltag) else {
        return SuffixEvidence::Absent;
    };
    let code = code.to_ascii_uppercase();
    match lookup_media_code(&code) {
        GeometryLookup::Supported(row) => SuffixEvidence::Recognised {
            code,
            generation: row.cartridge_generation,
            format: row.recording_format,
            supported: true,
            row: Some(row),
            unsupported_reason: None,
        },
        GeometryLookup::Unsupported(row) => SuffixEvidence::Recognised {
            code,
            generation: row.cartridge_generation,
            format: row.recording_format,
            supported: false,
            row: None,
            unsupported_reason: Some(row.reason),
        },
        GeometryLookup::Absent => SuffixEvidence::Unrecognised { code },
    }
}

/// Whether a generation string appears in the structural or unsupported
/// vocabulary — the "individually recognised" test that separates an
/// impossible pair (malformed) from an unknown one (a version-skew-safe
/// unavailable).
fn known_generation(generation: &str) -> bool {
    STRUCTURAL_TABLE
        .iter()
        .any(|row| row.cartridge_generation == generation)
        || UNSUPPORTED_TABLE
            .iter()
            .any(|row| row.cartridge_generation == generation)
}

fn known_format(format: &str) -> bool {
    STRUCTURAL_TABLE
        .iter()
        .any(|row| row.recording_format == format)
        || UNSUPPORTED_TABLE
            .iter()
            .any(|row| row.recording_format == format)
}

fn normalise_fact(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_ascii_uppercase())
    }
}

/// The reason string of the M8 unsupported row, via the single lookup
/// funnel rather than a table index.
fn m8_reason() -> &'static str {
    match lookup_geometry("M8", "M8") {
        GeometryLookup::Unsupported(row) => row.reason,
        _ => unreachable!("M8 is a recognised unsupported key by construction"),
    }
}

/// Resolve the cartridge facts as one pair — the complete §6.3 matrix.
///
/// A caller-supplied complete pair takes precedence over a recognised
/// barcode suffix; a lone field may only be completed by an agreeing
/// supported suffix; `M8` in either component is recognised but
/// unsupported, never an impossible pair.
fn resolve_cartridge_facts(generation_raw: &str, format_raw: &str, voltag: &str) -> FactResolution {
    let generation = normalise_fact(generation_raw);
    let format = normalise_fact(format_raw);
    let suffix = suffix_evidence(voltag);

    match (generation.as_deref(), format.as_deref()) {
        (Some(generation), Some(format)) => resolve_complete_pair(generation, format, &suffix),
        (None, None) => resolve_suffix_alone(&suffix),
        (Some(generation), None) => resolve_partial(PartialFact::Generation(generation), &suffix),
        (None, Some(format)) => resolve_partial(PartialFact::Format(format), &suffix),
    }
}

/// §6.3 matrix, complete-pair rows: the pair is used, subject to its
/// supported/unsupported row; a recognised disagreeing suffix only sets
/// the diagnostic flag; an impossible pair of recognised components is
/// malformed, while a pair with an unrecognised component stays a
/// version-skew-safe unknown.
fn resolve_complete_pair(
    generation: &str,
    format: &str,
    suffix: &SuffixEvidence,
) -> FactResolution {
    let format_disagreement = match suffix {
        SuffixEvidence::Recognised {
            generation: suffix_generation,
            format: suffix_format,
            ..
        } => *suffix_generation != generation || *suffix_format != format,
        _ => false,
    };
    match lookup_geometry(generation, format) {
        GeometryLookup::Supported(row) => FactResolution {
            outcome: FactOutcome::Supported(row),
            resolved_key: Some((generation.to_string(), format.to_string())),
            format_disagreement,
        },
        GeometryLookup::Unsupported(row) => FactResolution {
            outcome: FactOutcome::UnsupportedFormat {
                detail: format!(
                    "cartridge facts ({generation}, {format}) name a recognised but \
                     unsupported format: {}",
                    row.reason
                ),
            },
            resolved_key: Some((generation.to_string(), format.to_string())),
            format_disagreement,
        },
        GeometryLookup::Absent => {
            if known_generation(generation) && known_format(format) {
                FactResolution {
                    outcome: FactOutcome::ImpossiblePair {
                        detail: format!(
                            "cartridge facts name the impossible pair ({generation}, {format}): \
                             both components are recognised but the combination does not exist"
                        ),
                    },
                    resolved_key: None,
                    format_disagreement,
                }
            } else {
                FactResolution {
                    outcome: FactOutcome::UnknownFormat {
                        detail: format!(
                            "no geometry row resolves for the pair ({generation}, {format})"
                        ),
                    },
                    resolved_key: None,
                    format_disagreement,
                }
            }
        }
    }
}

/// §6.3 matrix, neither-field rows: the barcode-default path.
fn resolve_suffix_alone(suffix: &SuffixEvidence) -> FactResolution {
    match suffix {
        SuffixEvidence::Absent => FactResolution {
            outcome: FactOutcome::UnknownFormat {
                detail: "neither cartridge generation nor recording format was supplied and \
                         the voltag carries no readable media-code suffix; there is no \
                         evidence from which to form a pair"
                    .to_string(),
            },
            resolved_key: None,
            format_disagreement: false,
        },
        SuffixEvidence::Unrecognised { code } => FactResolution {
            outcome: FactOutcome::UnknownFormat {
                detail: format!("unrecognised voltag media code {code:?}"),
            },
            resolved_key: None,
            format_disagreement: false,
        },
        SuffixEvidence::Recognised {
            code,
            generation,
            format,
            supported: false,
            unsupported_reason,
            ..
        } => FactResolution {
            outcome: FactOutcome::UnsupportedFormat {
                detail: format!(
                    "voltag media code {code} is recognised but unsupported: {}",
                    unsupported_reason.unwrap_or("no adjudicated geometry")
                ),
            },
            resolved_key: Some((generation.to_string(), format.to_string())),
            format_disagreement: false,
        },
        SuffixEvidence::Recognised {
            generation,
            format,
            supported: true,
            row,
            ..
        } => FactResolution {
            outcome: FactOutcome::Supported(row.expect("supported suffix carries its row")),
            resolved_key: Some((generation.to_string(), format.to_string())),
            format_disagreement: false,
        },
    }
}

enum PartialFact<'a> {
    Generation(&'a str),
    Format(&'a str),
}

/// §6.3 matrix, exactly-one-field rows: an agreeing supported suffix
/// completes the pair; an absent or unrecognised suffix cannot; a
/// disagreeing suffix cannot safely complete a contradictory partial
/// pair; an M8 suffix — or a lone M8 field, which no completion could
/// make supported — is recognised but unsupported.
fn resolve_partial(supplied: PartialFact<'_>, suffix: &SuffixEvidence) -> FactResolution {
    let supplied_value = match &supplied {
        PartialFact::Generation(value) | PartialFact::Format(value) => *value,
    };
    if supplied_value == "M8" {
        return FactResolution {
            outcome: FactOutcome::UnsupportedFormat {
                detail: format!("M8 is recognised but unsupported: {}", m8_reason()),
            },
            resolved_key: Some(("M8".to_string(), "M8".to_string())),
            format_disagreement: false,
        };
    }
    match suffix {
        SuffixEvidence::Absent => FactResolution {
            outcome: FactOutcome::UnknownFormat {
                detail: "exactly one of cartridge generation and recording format was \
                         supplied and the voltag carries no readable media-code suffix; \
                         one field is not a geometry key"
                    .to_string(),
            },
            resolved_key: None,
            format_disagreement: false,
        },
        SuffixEvidence::Unrecognised { code } => FactResolution {
            outcome: FactOutcome::UnknownFormat {
                detail: format!(
                    "unrecognised voltag media code {code:?} cannot complete a partial pair"
                ),
            },
            resolved_key: None,
            format_disagreement: false,
        },
        SuffixEvidence::Recognised {
            code,
            generation,
            format,
            supported: false,
            unsupported_reason,
            ..
        } => FactResolution {
            outcome: FactOutcome::UnsupportedFormat {
                detail: format!(
                    "voltag media code {code} is recognised but unsupported, and a partial \
                     assertion cannot override it: {}",
                    unsupported_reason.unwrap_or("no adjudicated geometry")
                ),
            },
            resolved_key: Some((generation.to_string(), format.to_string())),
            format_disagreement: false,
        },
        SuffixEvidence::Recognised {
            code,
            generation,
            format,
            supported: true,
            row,
            ..
        } => {
            let agrees = match &supplied {
                PartialFact::Generation(value) => value == generation,
                PartialFact::Format(value) => value == format,
            };
            if agrees {
                FactResolution {
                    outcome: FactOutcome::Supported(row.expect("supported suffix carries its row")),
                    resolved_key: Some((generation.to_string(), format.to_string())),
                    format_disagreement: false,
                }
            } else {
                FactResolution {
                    outcome: FactOutcome::UnknownFormat {
                        detail: format!(
                            "the supplied {} disagrees with voltag media code {code} \
                             (canonical pair ({generation}, {format})); the suffix cannot \
                             safely complete a contradictory partial pair",
                            match &supplied {
                                PartialFact::Generation(value) =>
                                    format!("cartridge generation {value:?}"),
                                PartialFact::Format(value) => format!("recording format {value:?}"),
                            }
                        ),
                    },
                    resolved_key: None,
                    format_disagreement: true,
                }
            }
        }
    }
}

// ---------------------------------------------------------------------
//  Response assembly
// ---------------------------------------------------------------------

/// Everything an unavailable response still carries.
struct ResponseContext {
    resolved_key: Option<(String, String)>,
    format_disagreement: bool,
    calibration_generation: u64,
}

impl ResponseContext {
    fn bare() -> Self {
        ResponseContext {
            resolved_key: None,
            format_disagreement: false,
            calibration_generation: 0,
        }
    }
}

/// The single response funnel: every emitted response passes through
/// here, which is where the two unconditional invariants live —
/// `max_targets` on every response and the sentinel never emitted.
fn finish(response: pb::PlanBatchReadResponse) -> pb::PlanBatchReadResponse {
    debug_assert_ne!(
        response.status,
        pb::PlanStatus::Unspecified as i32,
        "PLAN_STATUS_UNSPECIFIED is a wire sentinel and is never emitted"
    );
    debug_assert_eq!(response.max_targets, MAX_TARGETS);
    response
}

fn resolved_key_pb(key: &Option<(String, String)>) -> Option<pb::ResolvedGeometryKey> {
    key.as_ref()
        .map(|(generation, format)| pb::ResolvedGeometryKey {
            cartridge_generation: generation.clone(),
            recording_format: format.clone(),
        })
}

/// An unavailable result: a normal response, empty hops, no estimates.
fn unavailable(
    status: pb::PlanStatus,
    detail: String,
    context: &ResponseContext,
) -> pb::PlanBatchReadResponse {
    finish(pb::PlanBatchReadResponse {
        status: status as i32,
        detail,
        hops: Vec::new(),
        estimated_total_ns: 0,
        cost_model_basis: pb::CostModelBasis::Priors as i32,
        resolved_key: resolved_key_pb(&context.resolved_key),
        format_disagreement: context.format_disagreement,
        max_targets: MAX_TARGETS,
        calibration_generation: context.calibration_generation,
        uses_estimated_eod_geometry: false,
    })
}

// ---------------------------------------------------------------------
//  The pipeline
// ---------------------------------------------------------------------

/// Serve one `PlanBatchRead`. Pure with respect to its inputs: reads
/// the wrap-map projection and the durable calibration row, writes
/// nothing, touches no drive.
pub(crate) fn plan_batch_read_response(
    index: &CatalogIndex,
    store: &CalibrationControlStore,
    request: pb::PlanBatchReadRequest,
) -> Result<pb::PlanBatchReadResponse, Status> {
    // -- Stage 1: malformed-request validation (§11, first). Every
    //    violation is collected so the BadRequest names them all.
    let mut violations = Vec::new();
    if request.tape_uuid.len() != 16 {
        violations.push(field_violation(
            "tape_uuid",
            format!(
                "must be exactly 16 bytes; got {} — no canonical per-volume map key can be \
                 selected",
                request.tape_uuid.len()
            ),
        ));
    }
    let objective = match pb::PlanObjective::try_from(request.objective) {
        Ok(pb::PlanObjective::ObjectiveUnspecified) | Ok(pb::PlanObjective::MinTotalTime) => {
            Objective::MinTotalTime
        }
        Ok(pb::PlanObjective::MinTimeToFirst) => Objective::MinTimeToFirst,
        Err(_) => {
            // Never coerced: across version skew a failed recall beats
            // a silently different ordering (§8.4).
            violations.push(field_violation(
                "objective",
                format!("unrecognised objective value {}", request.objective),
            ));
            Objective::MinTotalTime // placeholder; the request is rejected below
        }
    };
    for (target_index, target) in request.targets.iter().enumerate() {
        if target.partition != 0 {
            violations.push(field_violation(
                format!("targets[{target_index}].partition"),
                format!(
                    "partition {} is not partition zero; v1 plans only partition zero, and a \
                     wrap-wise partitioned volume's geometry is not the map's geometry",
                    target.partition
                ),
            ));
        }
        if target.end_block < target.start_block {
            violations.push(field_violation(
                format!("targets[{target_index}].end_block"),
                format!(
                    "end_block {} precedes start_block {}; the drive itself rejects this",
                    target.end_block, target.start_block
                ),
            ));
        }
    }
    for (field, position) in [
        ("start_position", request.start_position.as_ref()),
        ("end_position", request.end_position.as_ref()),
    ] {
        if let Some(position) = position {
            if position.partition != 0 {
                violations.push(field_violation(
                    format!("{field}.partition"),
                    format!(
                        "partition {} is not partition zero; v1 plans only partition zero",
                        position.partition
                    ),
                ));
            }
        }
    }
    // The extent-dependent malformed checks run as soon as the fact is
    // known (§11) — it is a request field, so a known (non-zero) extent
    // is checked here, before any cartridge-facts result; an unknown
    // extent leaves them to stage 2's UNAVAILABLE_UNKNOWN_EXTENT.
    let written_extent = request
        .cartridge
        .as_ref()
        .map(|facts| facts.written_extent_lba)
        .unwrap_or(0);
    if written_extent > 0 {
        for (target_index, target) in request.targets.iter().enumerate() {
            if target.start_block >= written_extent || target.end_block >= written_extent {
                violations.push(field_violation(
                    format!("targets[{target_index}]"),
                    format!(
                        "target [{}, {}] lies at or beyond the exclusive written extent \
                         {written_extent}",
                        target.start_block, target.end_block
                    ),
                ));
            }
        }
        for (field, position) in [
            ("start_position.block", request.start_position.as_ref()),
            ("end_position.block", request.end_position.as_ref()),
        ] {
            if let Some(position) = position {
                if position.block >= written_extent {
                    violations.push(field_violation(
                        field,
                        format!(
                            "block {} is at or beyond the exclusive written extent \
                             {written_extent}; written_extent_lba is exclusive, so the extent \
                             itself is EOD, not a written block",
                            position.block
                        ),
                    ));
                }
            }
        }
    }
    if !violations.is_empty() {
        return Err(invalid_argument(violations));
    }

    // -- Stage 2: cartridge facts (§11 table order: block size,
    //    compression, extent, format).
    let facts = request.cartridge.clone().unwrap_or_default();
    let mut context = ResponseContext::bare();
    if facts.block_size_bytes == 0 {
        return Ok(unavailable(
            pb::PlanStatus::UnavailableUnknownBlockSize,
            "block_size_bytes is absent or zero; the planner does not guess block sizes"
                .to_string(),
            &context,
        ));
    }
    match pb::CompressionState::try_from(facts.compression) {
        Ok(pb::CompressionState::CompressionDisabled) => {}
        Ok(pb::CompressionState::CompressionEnabled) => {
            return Ok(unavailable(
                pb::PlanStatus::UnavailableCompressionEnabled,
                "the volume was written with drive compression enabled; recorded lengths do \
                 not describe physical extent"
                    .to_string(),
                &context,
            ));
        }
        // Silence is its own state, never permission (§11); an
        // unrecognised value is equally not a declaration this version
        // can honour, and coercing it to either declared state would
        // assume what the design promises never to assume.
        Ok(pb::CompressionState::CompressionUnspecified) | Err(_) => {
            return Ok(unavailable(
                pb::PlanStatus::UnavailableUnknownCompression,
                "compression state was not declared; silence is its own state, not permission"
                    .to_string(),
                &context,
            ));
        }
    }
    if written_extent == 0 {
        return Ok(unavailable(
            pb::PlanStatus::UnavailableUnknownExtent,
            "written_extent_lba is absent or zero; the written extent bounds every position \
             check"
                .to_string(),
            &context,
        ));
    }
    let resolution = resolve_cartridge_facts(
        &facts.cartridge_generation,
        &facts.recording_format,
        &facts.voltag,
    );
    context.resolved_key = resolution.resolved_key;
    context.format_disagreement = resolution.format_disagreement;
    let geometry = match resolution.outcome {
        FactOutcome::Supported(row) => row,
        FactOutcome::UnsupportedFormat { detail } => {
            // Request-derived: the map was never consulted, so the
            // generation stays 0 and the negative is freely cacheable.
            return Ok(unavailable(
                pb::PlanStatus::UnavailableUnsupportedFormat,
                detail,
                &context,
            ));
        }
        FactOutcome::UnknownFormat { detail } => {
            return Ok(unavailable(
                pb::PlanStatus::UnavailableUnknownFormat,
                detail,
                &context,
            ));
        }
        FactOutcome::ImpossiblePair { detail } => {
            return Err(invalid_argument(vec![field_violation("cartridge", detail)]));
        }
    };

    // -- Stage 3: map validity and trust. Consult-only: the read path
    //    never writes to the durable store and never allocates a
    //    generation — 0 means "no calibration history" and is a valid
    //    cache key, reported as-is.
    let tape_uuid: [u8; 16] = request
        .tape_uuid
        .as_slice()
        .try_into()
        .expect("tape_uuid length was validated in stage 1");
    let serve = servable_wrap_map(index, store, tape_uuid)
        .map_err(|err| Status::internal(format!("calibration consult failed: {err}")))?;
    let map = match serve {
        WrapMapServeOutcome::NotServable {
            calibration_generation,
            refusal,
        } => {
            context.calibration_generation = calibration_generation;
            let (status, detail) = match refusal {
                WrapMapServeRefusal::UnsupportedFormat => (
                    // Calibration-derived even though it shares the
                    // unsupported-format wire status: it carries the
                    // volume's generation and is not freely cacheable.
                    pb::PlanStatus::UnavailableUnsupportedFormat,
                    "the volume's last load harvest recorded an unsupported format (a \
                     recognised-unsupported medium, or the drive rejected READ END OF WRAP \
                     POSITION); holds for this load",
                ),
                WrapMapServeRefusal::Untrusted => (
                    pb::PlanStatus::UnavailableUncalibrated,
                    "write_path_trust is OUT_OF_BAND_WRITE_POSSIBLE; planning is disabled \
                     until every modifying path is fenced again and a fresh load harvest \
                     succeeds",
                ),
                WrapMapServeRefusal::NotCalibrated => (
                    pb::PlanStatus::UnavailableUncalibrated,
                    "the volume is uncalibrated under the current write epoch; a fresh \
                     harvest at its next load recalibrates it",
                ),
                WrapMapServeRefusal::NoMap => (
                    pb::PlanStatus::UnavailableUncalibrated,
                    "no wrap map is cached for this volume (never harvested, or evicted); \
                     the next load harvest rebuilds it",
                ),
                WrapMapServeRefusal::InvalidEpochMismatch => (
                    pb::PlanStatus::UnavailableUncalibrated,
                    "the cached wrap map's write epoch does not match the volume's durable \
                     epoch; an invalid map is never served",
                ),
                WrapMapServeRefusal::CorruptMapRow => (
                    pb::PlanStatus::UnavailableUncalibrated,
                    "the cached wrap-map row no longer validates and is treated as absent; \
                     the next load harvest rewrites it",
                ),
            };
            return Ok(unavailable(status, detail.to_string(), &context));
        }
        WrapMapServeOutcome::Servable {
            map,
            write_epoch: _,
            calibration_generation,
        } => {
            context.calibration_generation = calibration_generation;
            map
        }
    };

    // Facts-versus-map consistency: a harvested map with more wraps
    // than the resolved geometry allows cannot be described by the
    // caller's facts. §11 has no explicit row for this contradiction;
    // it is answered as UNKNOWN_FORMAT (no row resolves *for this
    // volume*), carrying the generation because a re-harvest could
    // change the answer.
    if map.wrap_count() > geometry.wraps {
        return Ok(unavailable(
            pb::PlanStatus::UnavailableUnknownFormat,
            format!(
                "the volume's harvested wrap map reports {} wraps but the resolved geometry \
                 ({}, {}) allows only {}; the supplied cartridge facts cannot describe this \
                 volume",
                map.wrap_count(),
                geometry.cartridge_generation,
                geometry.recording_format,
                geometry.wraps
            ),
            &context,
        ));
    }

    // -- Stage 4: map coverage, against the exclusive
    //    `mapped_extent_lba` only, never a derived wrap start (§6.5).
    //    Everything here is already below `written_extent_lba`, so this
    //    is exactly the written-but-uncovered gap.
    let mapped_extent = map.mapped_extent_lba();
    for (target_index, target) in request.targets.iter().enumerate() {
        if target.start_block >= mapped_extent || target.end_block >= mapped_extent {
            return Ok(unavailable(
                pb::PlanStatus::UnavailableMapStale,
                format!(
                    "target {target_index} lies at or beyond the map's exclusive mapped extent \
                     {mapped_extent}: written, but outside this valid snapshot's coverage — \
                     a coverage gap, not an invalid map"
                ),
                &context,
            ));
        }
    }
    for (field, position) in [
        ("start_position", request.start_position.as_ref()),
        ("end_position", request.end_position.as_ref()),
    ] {
        if let Some(position) = position {
            if position.block >= mapped_extent {
                return Ok(unavailable(
                    pb::PlanStatus::UnavailableMapStale,
                    format!(
                        "{field}.block {} is at or beyond the map's exclusive mapped extent \
                         {mapped_extent}; the supplied position is outside map coverage",
                        position.block
                    ),
                    &context,
                ));
            }
        }
    }

    let start_block = request
        .start_position
        .as_ref()
        .map(|position| position.block);
    let end_block = request.end_position.as_ref().map(|position| position.block);

    // -- Stage 5: batch-size fallback. Above the drive's own UDS limit
    //    the solver is not invoked: ascending block order is returned
    //    and the estimates describe that order.
    if request.targets.len() > MAX_TARGETS as usize {
        return degraded_ascending(
            geometry,
            &map,
            &request.targets,
            start_block,
            end_block,
            &context,
        );
    }

    // -- Stage 6: the plan, zero/one-target fast path included (the
    //    pure core returns those unchanged without invoking the
    //    solver; a present end_position still contributes its terminal
    //    hop).
    let order_targets: Vec<OrderReadTarget> = request
        .targets
        .iter()
        .map(|target| OrderReadTarget {
            start_block: target.start_block,
            end_block: target.end_block,
        })
        .collect();
    let planned = plan(&PlanInput {
        geometry,
        map: &map,
        priors: &PUBLISHED_PRIORS,
        targets: &order_targets,
        objective,
        start_block,
        end_block,
    })
    .map_err(|err| {
        // Every PlanError cause was decided in an earlier stage
        // (validation, coverage, wrap-count consistency); reaching one
        // here is a defect, not a caller condition.
        Status::internal(format!("planning failed after validation: {err}"))
    })?;

    let hops = planned
        .hops
        .iter()
        .map(|hop| pb::PlannedHop {
            target: Some(request.targets[hop.target_index].clone()),
            estimated_locate_ns: hop.estimated_locate_ns.as_u64(),
        })
        .collect();
    Ok(finish(pb::PlanBatchReadResponse {
        status: pb::PlanStatus::Ok as i32,
        detail: if map.completed_spans_highly_dispersed() {
            DISPERSED_DETAIL.to_string()
        } else {
            String::new()
        },
        hops,
        estimated_total_ns: planned.estimated_total_ns.as_u64(),
        cost_model_basis: pb::CostModelBasis::Priors as i32,
        resolved_key: resolved_key_pb(&context.resolved_key),
        format_disagreement: context.format_disagreement,
        max_targets: MAX_TARGETS,
        calibration_generation: context.calibration_generation,
        uses_estimated_eod_geometry: planned.uses_estimated_eod_geometry,
    }))
}

/// The `DEGRADED_ASCENDING_FALLBACK` path: ascending block order, no
/// solver, per-hop estimates computed over the order actually returned
/// with the same cost primitives the planner uses.
fn degraded_ascending(
    geometry: &'static StructuralRow,
    map: &WrapMap,
    targets: &[pb::ReadTarget],
    start_block: Option<u64>,
    end_block: Option<u64>,
    context: &ResponseContext,
) -> Result<pb::PlanBatchReadResponse, Status> {
    let position = |block: u64| {
        physical_position(map, geometry, block).map_err(|err: PositionError| {
            // Coverage and wrap-count consistency were decided in
            // stages 3 and 4; reaching an error here is a defect.
            Status::internal(format!("position mapping failed after validation: {err:?}"))
        })
    };
    let mut order: Vec<usize> = (0..targets.len()).collect();
    order.sort_by_key(|&index| (targets[index].start_block, targets[index].end_block, index));

    let mut uses_estimated_eod = map.completed_spans_highly_dispersed();
    let (start_position, start_uses_eod) = position(start_block.unwrap_or(0))?;
    uses_estimated_eod |= start_uses_eod;
    let mut previous_end = start_position;
    let mut hops = Vec::with_capacity(order.len());
    let mut total: u128 = 0;
    for &index in &order {
        let (target_start, start_eod) = position(targets[index].start_block)?;
        let (target_end, end_eod) = position(targets[index].end_block)?;
        uses_estimated_eod |= start_eod | end_eod;
        let locate_ns = hop_ns(&PUBLISHED_PRIORS, &previous_end, &target_start).as_u64();
        total += u128::from(locate_ns);
        hops.push(pb::PlannedHop {
            target: Some(targets[index].clone()),
            estimated_locate_ns: locate_ns,
        });
        previous_end = target_end;
    }
    if let Some(end_block) = end_block {
        let (end_position, end_eod) = position(end_block)?;
        uses_estimated_eod |= end_eod;
        total += u128::from(hop_ns(&PUBLISHED_PRIORS, &previous_end, &end_position).as_u64());
    }

    let mut detail = format!(
        "batch of {} targets exceeds MAX_TARGETS ({MAX_TARGETS}); returning ascending block \
         order without the solver, with estimates describing that order",
        targets.len()
    );
    if map.completed_spans_highly_dispersed() {
        detail.push_str("; ");
        detail.push_str(DISPERSED_DETAIL);
    }
    Ok(finish(pb::PlanBatchReadResponse {
        status: pb::PlanStatus::DegradedAscendingFallback as i32,
        detail,
        hops,
        estimated_total_ns: u64::try_from(total).unwrap_or(u64::MAX),
        cost_model_basis: pb::CostModelBasis::Priors as i32,
        resolved_key: resolved_key_pb(&context.resolved_key),
        format_disagreement: context.format_disagreement,
        max_targets: MAX_TARGETS,
        calibration_generation: context.calibration_generation,
        uses_estimated_eod_geometry: uses_estimated_eod,
    }))
}

#[cfg(test)]
mod tests {
    //! Failure paths carry the weight (prompt-set rule): every
    //! non-sentinel status is reachable, every §11 row maps to the
    //! status it claims, and overlapping conditions resolve by the §11
    //! precedence. The `call` helper asserts the two unconditional
    //! response invariants — the sentinel is never emitted and
    //! `max_targets` is returned on every response, unavailable ones
    //! included — on every response every test receives.

    use prost::Message;
    use remanence_order::ReowpDescriptor;
    use remanence_state::{
        CalibrationControlStore, CatalogIndex, HarvestTransition, StoredWrapDescriptor,
        WrapMapCacheRecord, WritePathTrust,
    };

    use super::*;

    const TAPE: [u8; 16] = [0x5B; 16];

    /// The fixture volume: written extent 4000, mapped extent 3500 —
    /// blocks [3500, 4000) are written but outside map coverage.
    const WRITTEN_EXTENT: u64 = 4000;
    const MAPPED_EXTENT: u64 = 3500;

    struct World {
        _dir: tempfile::TempDir,
        index: CatalogIndex,
        store: CalibrationControlStore,
    }

    fn world() -> World {
        let dir = tempfile::Builder::new()
            .prefix("rem-api-read-plan")
            .tempdir()
            .expect("tempdir");
        let index = CatalogIndex::open(dir.path().join("index.sqlite")).expect("open index");
        let store =
            CalibrationControlStore::open(dir.path().join("calibration")).expect("open store");
        World {
            _dir: dir,
            index,
            store,
        }
    }

    /// Raw descriptors of the fixture map: three completed wraps of
    /// span 1000, EOD wrap 3 starting at 3000, EOD at 3500.
    fn fixture_descriptors() -> Vec<(u32, u64)> {
        vec![(0, 999), (1, 1999), (2, 2999), (3, MAPPED_EXTENT)]
    }

    /// Seed the calibrated wrap map exactly as the design's scenario
    /// seeding describes: matching write epoch and calibration
    /// generation in the durable store and the projection. Returns the
    /// generation.
    fn seed_calibrated(world: &mut World) -> u64 {
        let HarvestTransition::Calibrated {
            write_epoch,
            calibration_generation,
        } = world
            .store
            .record_harvest_success(TAPE, 0)
            .expect("harvest transition")
        else {
            panic!("fresh trusted volume calibrates");
        };
        world
            .index
            .upsert_wrap_map(&WrapMapCacheRecord {
                tape_uuid: TAPE,
                descriptors: fixture_descriptors()
                    .into_iter()
                    .map(|(wrap_number, end_loi)| StoredWrapDescriptor {
                        partition: 0,
                        wrap_number,
                        end_loi,
                    })
                    .collect(),
                mapped_extent_lba: MAPPED_EXTENT,
                write_epoch,
                calibration_generation,
                harvested_at_utc: "2026-08-04T00:00:00Z".to_string(),
            })
            .expect("seed map");
        calibration_generation
    }

    /// The same map, as the pure core sees it, for expected-estimate
    /// cross-checks.
    fn fixture_map() -> WrapMap {
        let descriptors: Vec<ReowpDescriptor> = fixture_descriptors()
            .into_iter()
            .map(|(wrap_number, end_loi)| ReowpDescriptor {
                partition: 0,
                wrap_number,
                end_loi,
            })
            .collect();
        WrapMap::from_descriptors(&descriptors).expect("fixture map is valid")
    }

    fn lto8_row() -> &'static StructuralRow {
        match lookup_geometry("LTO-8", "L8") {
            GeometryLookup::Supported(row) => row,
            other => panic!("LTO-8/L8 is supported, got {other:?}"),
        }
    }

    fn expected_hop_ns(from_block: u64, to_block: u64) -> u64 {
        let map = fixture_map();
        let geometry = lto8_row();
        let (from, _) = physical_position(&map, geometry, from_block).expect("from in coverage");
        let (to, _) = physical_position(&map, geometry, to_block).expect("to in coverage");
        hop_ns(&PUBLISHED_PRIORS, &from, &to).as_u64()
    }

    fn target(start_block: u64, end_block: u64) -> pb::ReadTarget {
        pb::ReadTarget {
            partition: 0,
            start_block,
            end_block,
            tag: format!("t-{start_block}").into_bytes(),
        }
    }

    fn facts() -> pb::CartridgeFacts {
        pb::CartridgeFacts {
            cartridge_generation: "LTO-8".to_string(),
            recording_format: "L8".to_string(),
            voltag: String::new(),
            block_size_bytes: 262_144,
            compression: pb::CompressionState::CompressionDisabled as i32,
            written_extent_lba: WRITTEN_EXTENT,
        }
    }

    fn request(targets: Vec<pb::ReadTarget>) -> pb::PlanBatchReadRequest {
        pb::PlanBatchReadRequest {
            cartridge: Some(facts()),
            targets,
            objective: pb::PlanObjective::ObjectiveUnspecified as i32,
            start_position: None,
            end_position: None,
            tape_uuid: TAPE.to_vec(),
        }
    }

    /// Every normal response funnels through here: the sentinel is
    /// never emitted, `max_targets` is on every response (unavailable
    /// ones included), and the basis is always PRIORS in v1.
    fn call(world: &World, request: pb::PlanBatchReadRequest) -> pb::PlanBatchReadResponse {
        let response = plan_batch_read_response(&world.index, &world.store, request)
            .expect("a normal response, not an RPC error");
        assert_ne!(
            response.status,
            pb::PlanStatus::Unspecified as i32,
            "PLAN_STATUS_UNSPECIFIED is never emitted"
        );
        assert_eq!(response.max_targets, MAX_TARGETS);
        assert_eq!(
            response.cost_model_basis,
            pb::CostModelBasis::Priors as i32,
            "v1 ships on fixed priors"
        );
        response
    }

    fn call_err(world: &World, request: pb::PlanBatchReadRequest) -> Status {
        plan_batch_read_response(&world.index, &world.store, request)
            .expect_err("expected an RPC error")
    }

    /// Decode the `google.rpc.BadRequest` out of the status details and
    /// return the violated field paths.
    fn bad_request_fields(status: &Status) -> Vec<String> {
        assert_eq!(status.code(), Code::InvalidArgument);
        let decoded =
            pb_rpc::Status::decode(status.details()).expect("grpc-status-details-bin decodes");
        let any = decoded
            .details
            .iter()
            .find(|any| any.type_url == BAD_REQUEST_TYPE_URL)
            .expect("a google.rpc.BadRequest detail is present");
        let bad_request =
            pb_rpc::BadRequest::decode(any.value.as_slice()).expect("BadRequest decodes");
        assert!(!bad_request.field_violations.is_empty());
        bad_request
            .field_violations
            .iter()
            .map(|violation| violation.field.clone())
            .collect()
    }

    fn status_of(response: &pb::PlanBatchReadResponse) -> pb::PlanStatus {
        pb::PlanStatus::try_from(response.status).expect("known status")
    }

    // =================================================================
    //  Stage 1 — malformed requests
    // =================================================================

    /// `tape_uuid` must be exactly 16 bytes: 15 and 17 both rejected
    /// (and zero), naming the field.
    #[test]
    fn tape_uuid_of_15_and_17_bytes_both_rejected() {
        let mut world = world();
        seed_calibrated(&mut world);
        for len in [15usize, 17, 0] {
            let mut req = request(vec![target(100, 200)]);
            req.tape_uuid = vec![0x5B; len];
            let status = call_err(&world, req);
            let fields = bad_request_fields(&status);
            assert_eq!(fields, vec!["tape_uuid".to_string()], "length {len}");
        }
    }

    /// An unrecognised objective is rejected, never coerced to the
    /// default: across version skew a failed recall beats a silently
    /// different ordering.
    #[test]
    fn unrecognised_objective_rejected_not_coerced() {
        let mut world = world();
        seed_calibrated(&mut world);
        let mut req = request(vec![target(100, 200)]);
        req.objective = 7;
        let status = call_err(&world, req);
        assert_eq!(bad_request_fields(&status), vec!["objective".to_string()]);

        // The two defined values still plan; UNSPECIFIED maps to
        // MIN_TOTAL_TIME rather than being rejected.
        for objective in [
            pb::PlanObjective::ObjectiveUnspecified,
            pb::PlanObjective::MinTotalTime,
            pb::PlanObjective::MinTimeToFirst,
        ] {
            let mut req = request(vec![target(100, 200)]);
            req.objective = objective as i32;
            let response = call(&world, req);
            assert_eq!(status_of(&response), pb::PlanStatus::Ok, "{objective:?}");
        }
    }

    /// `end_block` before `start_block` names the offending target by
    /// index — `tag` is opaque bytes and may not be printable.
    #[test]
    fn end_before_start_names_the_target_index() {
        let mut world = world();
        seed_calibrated(&mut world);
        let status = call_err(&world, request(vec![target(100, 200), target(500, 400)]));
        assert_eq!(
            bad_request_fields(&status),
            vec!["targets[1].end_block".to_string()]
        );
    }

    /// Non-zero partitions are rejected on targets and on both
    /// positions — remanence plans only partition zero.
    #[test]
    fn nonzero_partitions_rejected_everywhere() {
        let mut world = world();
        seed_calibrated(&mut world);

        let mut bad_target = target(100, 200);
        bad_target.partition = 1;
        let status = call_err(&world, request(vec![bad_target]));
        assert_eq!(
            bad_request_fields(&status),
            vec!["targets[0].partition".to_string()]
        );

        let mut req = request(vec![target(100, 200)]);
        req.start_position = Some(pb::StartPosition {
            partition: 2,
            block: 100,
        });
        let status = call_err(&world, req);
        assert_eq!(
            bad_request_fields(&status),
            vec!["start_position.partition".to_string()]
        );

        let mut req = request(vec![target(100, 200)]);
        req.end_position = Some(pb::StartPosition {
            partition: 3,
            block: 100,
        });
        let status = call_err(&world, req);
        assert_eq!(
            bad_request_fields(&status),
            vec!["end_position.partition".to_string()]
        );
    }

    /// A target at or beyond the exclusive written extent is malformed
    /// (§8.3/§11): the extent itself is EOD, not a written block.
    #[test]
    fn target_at_or_beyond_written_extent_rejected() {
        let mut world = world();
        seed_calibrated(&mut world);
        for end_block in [WRITTEN_EXTENT, WRITTEN_EXTENT + 100] {
            let status = call_err(&world, request(vec![target(100, end_block)]));
            assert_eq!(
                bad_request_fields(&status),
                vec!["targets[0]".to_string()],
                "end_block {end_block}"
            );
        }
    }

    /// Both supplied positions get the same written-extent rule, at the
    /// boundary and beyond it.
    #[test]
    fn positions_at_or_beyond_written_extent_rejected() {
        let mut world = world();
        seed_calibrated(&mut world);
        for block in [WRITTEN_EXTENT, WRITTEN_EXTENT + 1] {
            let mut req = request(vec![target(100, 200)]);
            req.start_position = Some(pb::StartPosition {
                partition: 0,
                block,
            });
            let status = call_err(&world, req);
            assert_eq!(
                bad_request_fields(&status),
                vec!["start_position.block".to_string()]
            );

            let mut req = request(vec![target(100, 200)]);
            req.end_position = Some(pb::StartPosition {
                partition: 0,
                block,
            });
            let status = call_err(&world, req);
            assert_eq!(
                bad_request_fields(&status),
                vec!["end_position.block".to_string()]
            );
        }
    }

    /// A request with several malformed conditions reports them all in
    /// one BadRequest.
    #[test]
    fn all_violations_reported_together() {
        let mut world = world();
        seed_calibrated(&mut world);
        let mut bad_target = target(300, 200);
        bad_target.partition = 5;
        let mut req = request(vec![bad_target]);
        req.tape_uuid = vec![0; 15];
        req.objective = 42;
        let status = call_err(&world, req);
        let fields = bad_request_fields(&status);
        assert_eq!(
            fields,
            vec![
                "tape_uuid".to_string(),
                "objective".to_string(),
                "targets[0].partition".to_string(),
                "targets[0].end_block".to_string(),
            ]
        );
    }

    // =================================================================
    //  Stage 2 — cartridge facts
    // =================================================================

    fn assert_unavailable_with_zero_generation(
        response: &pb::PlanBatchReadResponse,
        status: pb::PlanStatus,
    ) {
        assert_eq!(status_of(response), status);
        assert!(response.hops.is_empty(), "hops empty unless OK or DEGRADED");
        assert_eq!(response.estimated_total_ns, 0);
        assert_eq!(
            response.calibration_generation, 0,
            "request-derived negatives never consult the map"
        );
    }

    /// Block size absent (no cartridge message at all) or zero.
    #[test]
    fn unknown_block_size() {
        let mut world = world();
        seed_calibrated(&mut world);

        let mut req = request(vec![target(100, 200)]);
        req.cartridge = None;
        let response = call(&world, req);
        assert_unavailable_with_zero_generation(
            &response,
            pb::PlanStatus::UnavailableUnknownBlockSize,
        );

        let mut req = request(vec![target(100, 200)]);
        req.cartridge.as_mut().unwrap().block_size_bytes = 0;
        let response = call(&world, req);
        assert_unavailable_with_zero_generation(
            &response,
            pb::PlanStatus::UnavailableUnknownBlockSize,
        );
    }

    /// Compression is three-state: silence is its own status, never
    /// permission; enabled is its own status; only a declared-off
    /// volume plans.
    #[test]
    fn compression_is_three_state() {
        let mut world = world();
        seed_calibrated(&mut world);

        let mut req = request(vec![target(100, 200)]);
        req.cartridge.as_mut().unwrap().compression =
            pb::CompressionState::CompressionUnspecified as i32;
        let response = call(&world, req);
        assert_unavailable_with_zero_generation(
            &response,
            pb::PlanStatus::UnavailableUnknownCompression,
        );

        let mut req = request(vec![target(100, 200)]);
        req.cartridge.as_mut().unwrap().compression =
            pb::CompressionState::CompressionEnabled as i32;
        let response = call(&world, req);
        assert_unavailable_with_zero_generation(
            &response,
            pb::PlanStatus::UnavailableCompressionEnabled,
        );

        // An unrecognised enum value is not a declaration this version
        // can honour: unknown, never permission.
        let mut req = request(vec![target(100, 200)]);
        req.cartridge.as_mut().unwrap().compression = 9;
        let response = call(&world, req);
        assert_unavailable_with_zero_generation(
            &response,
            pb::PlanStatus::UnavailableUnknownCompression,
        );
    }

    /// `written_extent_lba` absent or zero.
    #[test]
    fn unknown_extent() {
        let mut world = world();
        seed_calibrated(&mut world);
        let mut req = request(vec![target(100, 200)]);
        req.cartridge.as_mut().unwrap().written_extent_lba = 0;
        let response = call(&world, req);
        assert_unavailable_with_zero_generation(
            &response,
            pb::PlanStatus::UnavailableUnknownExtent,
        );
    }

    /// The full §6.3 media-code matrix: every supported code resolves
    /// to its canonical geometry key and plans; `M8` is recognised but
    /// unsupported — not an impossible pair, not INVALID_ARGUMENT; an
    /// unrecognised code is unknown. A naive implementation refuses the
    /// WORM codes `LX`/`LY` as unknown; they are supported.
    #[test]
    fn full_media_code_matrix_via_voltag() {
        let mut world = world();
        seed_calibrated(&mut world);
        let supported = [
            ("L7", "LTO-7", "L7"),
            ("LX", "LTO-7", "L7"),
            ("L8", "LTO-8", "L8"),
            ("LY", "LTO-8", "L8"),
            ("L9", "LTO-9", "L9"),
            ("LZ", "LTO-9", "L9"),
            ("LA", "LTO-10", "LA"),
            ("LH", "LTO-10", "LA"),
            ("PA", "LTO-10", "PA"),
        ];
        for (code, generation, format) in supported {
            let mut req = request(vec![target(100, 200)]);
            let cartridge = req.cartridge.as_mut().unwrap();
            cartridge.cartridge_generation.clear();
            cartridge.recording_format.clear();
            cartridge.voltag = format!("ARC001{code}");
            let response = call(&world, req);
            assert_eq!(
                status_of(&response),
                pb::PlanStatus::Ok,
                "media code {code}"
            );
            let key = response.resolved_key.expect("resolved key, both halves");
            assert_eq!(key.cartridge_generation, generation, "media code {code}");
            assert_eq!(key.recording_format, format, "media code {code}");
            assert!(!response.format_disagreement);
        }

        // M8: recognised but unsupported — a normal unavailable result.
        let mut req = request(vec![target(100, 200)]);
        let cartridge = req.cartridge.as_mut().unwrap();
        cartridge.cartridge_generation.clear();
        cartridge.recording_format.clear();
        cartridge.voltag = "ARC001M8".to_string();
        let response = call(&world, req);
        assert_unavailable_with_zero_generation(
            &response,
            pb::PlanStatus::UnavailableUnsupportedFormat,
        );

        // An unrecognised code is unknown, distinguishable from
        // unsupported.
        let mut req = request(vec![target(100, 200)]);
        let cartridge = req.cartridge.as_mut().unwrap();
        cartridge.cartridge_generation.clear();
        cartridge.recording_format.clear();
        cartridge.voltag = "ARC001QQ".to_string();
        let response = call(&world, req);
        assert_unavailable_with_zero_generation(
            &response,
            pb::PlanStatus::UnavailableUnknownFormat,
        );
    }

    /// The §6.3 resolution matrix, row by row.
    #[test]
    fn cartridge_fact_resolution_matrix() {
        let mut world = world();
        seed_calibrated(&mut world);
        let with_facts = |generation: &str, format: &str, voltag: &str| {
            let mut req = request(vec![target(100, 200)]);
            let cartridge = req.cartridge.as_mut().unwrap();
            cartridge.cartridge_generation = generation.to_string();
            cartridge.recording_format = format.to_string();
            cartridge.voltag = voltag.to_string();
            req
        };

        // Neither field, absent barcode: no evidence at all.
        let response = call(&world, with_facts("", "", ""));
        assert_eq!(
            status_of(&response),
            pb::PlanStatus::UnavailableUnknownFormat
        );
        assert!(response.resolved_key.is_none());

        // Exactly one field, agreeing supported suffix (WORM
        // normalisation included): the suffix completes the pair.
        let response = call(&world, with_facts("LTO-8", "", "ARC001LY"));
        assert_eq!(status_of(&response), pb::PlanStatus::Ok);
        let key = response.resolved_key.expect("completed pair");
        assert_eq!(key.cartridge_generation, "LTO-8");
        assert_eq!(key.recording_format, "L8");
        assert!(!response.format_disagreement);

        // Exactly one field, absent barcode: one field is not a
        // geometry key.
        let response = call(&world, with_facts("LTO-8", "", ""));
        assert_eq!(
            status_of(&response),
            pb::PlanStatus::UnavailableUnknownFormat
        );

        // Exactly one field, recognised suffix that disagrees: the
        // suffix cannot complete a contradictory partial pair.
        let response = call(&world, with_facts("LTO-7", "", "ARC001L8"));
        assert_eq!(
            status_of(&response),
            pb::PlanStatus::UnavailableUnknownFormat
        );
        assert!(
            response.format_disagreement,
            "the disagreement is reported diagnostically"
        );

        // Exactly one field with an M8 suffix: a partial assertion
        // cannot override the recognised unsupported suffix.
        let response = call(&world, with_facts("LTO-8", "", "ARC001M8"));
        assert_eq!(
            status_of(&response),
            pb::PlanStatus::UnavailableUnsupportedFormat
        );

        // A lone M8 field: recognised, and no completion could make a
        // pair containing M8 supported.
        let response = call(&world, with_facts("", "M8", ""));
        assert_eq!(
            status_of(&response),
            pb::PlanStatus::UnavailableUnsupportedFormat
        );

        // Complete caller pair with an agreeing suffix.
        let response = call(&world, with_facts("LTO-8", "L8", "ARC001L8"));
        assert_eq!(status_of(&response), pb::PlanStatus::Ok);
        assert!(!response.format_disagreement);

        // Complete caller pair overrides a disagreeing barcode; the
        // disagreement is diagnostic only — same plan, same status.
        let response = call(&world, with_facts("LTO-8", "L8", "ARC001L9"));
        assert_eq!(status_of(&response), pb::PlanStatus::Ok);
        assert!(response.format_disagreement);
        let key = response.resolved_key.expect("caller pair wins");
        assert_eq!(key.cartridge_generation, "LTO-8");
        assert_eq!(key.recording_format, "L8");

        // Complete pair naming M8 in either component: recognised but
        // unsupported, NOT an impossible pair, NOT INVALID_ARGUMENT.
        for (generation, format) in [("M8", "M8"), ("LTO-8", "M8"), ("M8", "L8")] {
            let response = call(&world, with_facts(generation, format, ""));
            assert_eq!(
                status_of(&response),
                pb::PlanStatus::UnavailableUnsupportedFormat,
                "({generation}, {format})"
            );
        }

        // A pre-REOWP generation is a request-derived unsupported row.
        let response = call(&world, with_facts("LTO-5", "L5", ""));
        assert_eq!(
            status_of(&response),
            pb::PlanStatus::UnavailableUnsupportedFormat
        );
        assert_eq!(response.calibration_generation, 0);

        // Impossible pair: both components recognised, combination
        // nonexistent — malformed (§11), naming the pair.
        let status = call_err(&world, with_facts("LTO-7", "L8", ""));
        assert_eq!(bad_request_fields(&status), vec!["cartridge".to_string()]);

        // A pair with an unrecognised component is version-skew-safe
        // unknown, not malformed.
        let response = call(&world, with_facts("LTO-11", "LB", ""));
        assert_eq!(
            status_of(&response),
            pb::PlanStatus::UnavailableUnknownFormat
        );
    }

    // =================================================================
    //  Stage 3 — map validity and trust
    // =================================================================

    /// A volume with no calibration history: uncalibrated at
    /// generation 0 — a valid cache key — and the consult allocates
    /// nothing (the read path never writes to the durable store).
    #[test]
    fn uncalibrated_no_history_reports_generation_zero_and_allocates_nothing() {
        let world = world();
        let response = call(&world, request(vec![target(100, 200)]));
        assert_eq!(
            status_of(&response),
            pb::PlanStatus::UnavailableUncalibrated
        );
        assert_eq!(
            response.calibration_generation, 0,
            "no history is honestly generation 0"
        );
        assert_eq!(
            world.store.last_generation(),
            0,
            "consulting validity must not allocate a generation"
        );
        assert_eq!(world.store.row(TAPE).calibration_generation, 0);
    }

    /// A load-recorded unsupported format is calibration-derived: the
    /// same wire status as the request-derived case, but carrying the
    /// volume's non-zero generation.
    #[test]
    fn unsupported_format_from_load_harvest_carries_generation() {
        let world = world();
        let generation = world
            .store
            .record_unsupported_format(TAPE)
            .expect("record transition");
        let response = call(&world, request(vec![target(100, 200)]));
        assert_eq!(
            status_of(&response),
            pb::PlanStatus::UnavailableUnsupportedFormat
        );
        assert_eq!(response.calibration_generation, generation);
        assert!(generation > 0);
    }

    /// `OUT_OF_BAND_WRITE_POSSIBLE` disables planning even with a
    /// calibrated map in the projection.
    #[test]
    fn out_of_band_write_trust_disables_planning() {
        let mut world = world();
        seed_calibrated(&mut world);
        world
            .store
            .set_write_path_trust(TAPE, WritePathTrust::OutOfBandWritePossible)
            .expect("set trust");
        let response = call(&world, request(vec![target(100, 200)]));
        assert_eq!(
            status_of(&response),
            pb::PlanStatus::UnavailableUncalibrated
        );
    }

    /// An epoch-mismatched map row is invalid and never served.
    #[test]
    fn epoch_mismatched_map_is_never_served() {
        let mut world = world();
        seed_calibrated(&mut world);
        let mut cached = world
            .index
            .get_wrap_map(&TAPE)
            .expect("get")
            .expect("present");
        cached.write_epoch += 7;
        world.index.upsert_wrap_map(&cached).expect("re-stamp");
        let response = call(&world, request(vec![target(100, 200)]));
        assert_eq!(
            status_of(&response),
            pb::PlanStatus::UnavailableUncalibrated
        );
    }

    /// Eviction leaves the volume uncalibrated with a fresh generation.
    #[test]
    fn evicted_map_is_uncalibrated_under_a_fresh_generation() {
        let mut world = world();
        let seeded_generation = seed_calibrated(&mut world);
        assert!(world.index.delete_wrap_map(&TAPE).expect("evict"));
        let evicted_generation = world.store.record_map_evicted(TAPE).expect("record");
        let response = call(&world, request(vec![target(100, 200)]));
        assert_eq!(
            status_of(&response),
            pb::PlanStatus::UnavailableUncalibrated
        );
        assert_eq!(response.calibration_generation, evicted_generation);
        assert!(evicted_generation > seeded_generation);
    }

    /// Caller facts that cannot describe the harvested map: an LTO-7
    /// claim (112 wraps) against a 208-wrap map. Unspecified by the §11
    /// table; answered as UNKNOWN_FORMAT carrying the generation.
    #[test]
    fn facts_that_cannot_describe_the_map_are_unknown_format() {
        let mut world = world();
        let HarvestTransition::Calibrated {
            write_epoch,
            calibration_generation,
        } = world
            .store
            .record_harvest_success(TAPE, 0)
            .expect("transition")
        else {
            panic!("calibrates");
        };
        // A 208-wrap map (LTO-8-shaped): completed spans of 10, EOD
        // wrap written 5.
        let descriptors: Vec<StoredWrapDescriptor> = (0..208u32)
            .map(|wrap_number| StoredWrapDescriptor {
                partition: 0,
                wrap_number,
                end_loi: if wrap_number == 207 {
                    2075
                } else {
                    u64::from(wrap_number) * 10 + 9
                },
            })
            .collect();
        world
            .index
            .upsert_wrap_map(&WrapMapCacheRecord {
                tape_uuid: TAPE,
                descriptors,
                mapped_extent_lba: 2075,
                write_epoch,
                calibration_generation,
                harvested_at_utc: "2026-08-04T00:00:00Z".to_string(),
            })
            .expect("seed map");
        let mut req = request(vec![target(100, 200)]);
        let cartridge = req.cartridge.as_mut().unwrap();
        cartridge.cartridge_generation = "LTO-7".to_string();
        cartridge.recording_format = "L7".to_string();
        cartridge.written_extent_lba = 2075;
        let response = call(&world, req);
        assert_eq!(
            status_of(&response),
            pb::PlanStatus::UnavailableUnknownFormat
        );
        assert_eq!(response.calibration_generation, calibration_generation);
    }

    // =================================================================
    //  Stage 4 — map coverage
    // =================================================================

    /// A target in the written-but-unmapped gap is a coverage gap:
    /// `UNAVAILABLE_MAP_STALE`, generation attached, target named in
    /// the human detail.
    #[test]
    fn target_beyond_mapped_extent_is_map_stale() {
        let mut world = world();
        let generation = seed_calibrated(&mut world);
        // end_block at the exclusive mapped extent: written (< 4000),
        // uncovered (>= 3500).
        let response = call(&world, request(vec![target(100, 200), target(3400, 3500)]));
        assert_eq!(status_of(&response), pb::PlanStatus::UnavailableMapStale);
        assert_eq!(response.calibration_generation, generation);
        assert!(
            response.detail.contains("target 1"),
            "detail names the offending target index: {}",
            response.detail
        );
        assert!(response.hops.is_empty());
    }

    /// Supplied positions get the same coverage rule.
    #[test]
    fn positions_beyond_mapped_extent_are_map_stale() {
        let mut world = world();
        seed_calibrated(&mut world);
        let in_gap = MAPPED_EXTENT + 100; // written, uncovered

        let mut req = request(vec![target(100, 200)]);
        req.start_position = Some(pb::StartPosition {
            partition: 0,
            block: in_gap,
        });
        let response = call(&world, req);
        assert_eq!(status_of(&response), pb::PlanStatus::UnavailableMapStale);
        assert!(response.detail.contains("start_position"));

        let mut req = request(vec![target(100, 200)]);
        req.end_position = Some(pb::StartPosition {
            partition: 0,
            block: in_gap,
        });
        let response = call(&world, req);
        assert_eq!(status_of(&response), pb::PlanStatus::UnavailableMapStale);
        assert!(response.detail.contains("end_position"));
    }

    // =================================================================
    //  Stage 5 — batch-size fallback
    // =================================================================

    fn oversized_targets() -> Vec<pb::ReadTarget> {
        (0..=MAX_TARGETS as u64) // 2731 targets
            .map(|index| {
                let start = (index * 7919) % 3400;
                target(start, start + 50)
            })
            .collect()
    }

    /// Above `MAX_TARGETS` the solver is not invoked: ascending block
    /// order comes back as DEGRADED, and the estimates describe the
    /// ascending order actually returned — recomputed here hop by hop
    /// with the same cost primitives.
    #[test]
    fn oversized_batch_degrades_to_ascending_with_matching_estimates() {
        let mut world = world();
        let generation = seed_calibrated(&mut world);
        let targets = oversized_targets();
        let response = call(&world, request(targets.clone()));
        assert_eq!(
            status_of(&response),
            pb::PlanStatus::DegradedAscendingFallback
        );
        assert_eq!(response.hops.len(), targets.len());
        assert_eq!(response.calibration_generation, generation);

        // The order is ascending block order and a permutation of the
        // input (tags are echoed, so identity survives duplication of
        // block ranges).
        let mut previous_start = 0u64;
        let mut seen_tags: Vec<Vec<u8>> = Vec::with_capacity(response.hops.len());
        for hop in &response.hops {
            let hop_target = hop.target.as_ref().expect("target echoed");
            assert!(
                hop_target.start_block >= previous_start,
                "ascending block order"
            );
            previous_start = hop_target.start_block;
            seen_tags.push(hop_target.tag.clone());
        }
        seen_tags.sort();
        let mut expected_tags: Vec<Vec<u8>> = targets.iter().map(|t| t.tag.clone()).collect();
        expected_tags.sort();
        assert_eq!(seen_tags, expected_tags, "a permutation of the input");

        // Estimates describe the returned order: recompute from the
        // load point through every hop.
        let mut from_block = 0u64;
        let mut expected_total = 0u128;
        for hop in &response.hops {
            let hop_target = hop.target.as_ref().expect("target echoed");
            let expected = expected_hop_ns(from_block, hop_target.start_block);
            assert_eq!(hop.estimated_locate_ns, expected);
            expected_total += u128::from(expected);
            from_block = hop_target.end_block;
        }
        assert_eq!(
            u128::from(response.estimated_total_ns),
            expected_total,
            "no terminal hop without an end_position"
        );
    }

    // =================================================================
    //  §11 precedence — overlapping conditions
    // =================================================================

    /// Oversized AND unsupported: cartridge facts precede the
    /// batch-size fallback — exactly one right answer.
    #[test]
    fn precedence_unsupported_beats_oversized() {
        let mut world = world();
        seed_calibrated(&mut world);
        let mut req = request(oversized_targets());
        let cartridge = req.cartridge.as_mut().unwrap();
        cartridge.cartridge_generation.clear();
        cartridge.recording_format.clear();
        cartridge.voltag = "ARC001M8".to_string();
        let response = call(&world, req);
        assert_eq!(
            status_of(&response),
            pb::PlanStatus::UnavailableUnsupportedFormat
        );
    }

    /// Malformed AND unavailable: a malformed target outranks the
    /// cartridge-facts stage entirely.
    #[test]
    fn precedence_malformed_beats_cartridge_facts() {
        let mut world = world();
        seed_calibrated(&mut world);
        let mut req = request(vec![target(500, 400)]); // end < start
        req.cartridge.as_mut().unwrap().compression =
            pb::CompressionState::CompressionUnspecified as i32;
        let status = call_err(&world, req);
        assert_eq!(
            bad_request_fields(&status),
            vec!["targets[0].end_block".to_string()]
        );
    }

    /// The written-extent malformed check runs as soon as the fact is
    /// known — before any stage-2 unavailable, block size included.
    #[test]
    fn precedence_known_extent_violation_beats_unknown_block_size() {
        let mut world = world();
        seed_calibrated(&mut world);
        let mut req = request(vec![target(100, WRITTEN_EXTENT + 5)]);
        req.cartridge.as_mut().unwrap().block_size_bytes = 0;
        let status = call_err(&world, req);
        assert_eq!(bad_request_fields(&status), vec!["targets[0]".to_string()]);
    }

    /// Oversized AND out of coverage: coverage precedes the batch-size
    /// fallback.
    #[test]
    fn precedence_map_stale_beats_oversized() {
        let mut world = world();
        seed_calibrated(&mut world);
        let mut targets = oversized_targets();
        targets[17] = target(3400, MAPPED_EXTENT); // written, uncovered
        let response = call(&world, request(targets));
        assert_eq!(status_of(&response), pb::PlanStatus::UnavailableMapStale);
        assert!(response.hops.is_empty());
    }

    /// Oversized AND uncalibrated: map validity precedes the batch-size
    /// fallback (and coverage cannot even be judged without a map).
    #[test]
    fn precedence_uncalibrated_beats_oversized() {
        let world = world(); // nothing seeded
        let response = call(&world, request(oversized_targets()));
        assert_eq!(
            status_of(&response),
            pb::PlanStatus::UnavailableUncalibrated
        );
    }

    /// Unknown compression AND unsupported format: within the
    /// cartridge-facts stage the §11 table order holds (compression
    /// before format resolution).
    #[test]
    fn precedence_unknown_compression_beats_unsupported_format() {
        let mut world = world();
        seed_calibrated(&mut world);
        let mut req = request(vec![target(100, 200)]);
        let cartridge = req.cartridge.as_mut().unwrap();
        cartridge.compression = pb::CompressionState::CompressionUnspecified as i32;
        cartridge.cartridge_generation = "M8".to_string();
        cartridge.recording_format = "M8".to_string();
        let response = call(&world, req);
        assert_eq!(
            status_of(&response),
            pb::PlanStatus::UnavailableUnknownCompression
        );
    }

    // =================================================================
    //  Stage 6 — plans
    // =================================================================

    /// Zero targets: OK without the solver; with an end_position the
    /// total is exactly the start-to-end terminal cost.
    #[test]
    fn zero_targets_fast_path() {
        let mut world = world();
        seed_calibrated(&mut world);

        let response = call(&world, request(Vec::new()));
        assert_eq!(status_of(&response), pb::PlanStatus::Ok);
        assert!(response.hops.is_empty());
        assert_eq!(response.estimated_total_ns, 0);

        let mut req = request(Vec::new());
        req.end_position = Some(pb::StartPosition {
            partition: 0,
            block: 2500,
        });
        let response = call(&world, req);
        assert_eq!(status_of(&response), pb::PlanStatus::Ok);
        assert!(response.hops.is_empty());
        assert_eq!(
            response.estimated_total_ns,
            expected_hop_ns(0, 2500),
            "zero targets with an end returns only the start-to-end terminal cost"
        );
    }

    /// One target: OK, returned unchanged, estimate from the supplied
    /// start position; the echoed target keeps its opaque tag.
    #[test]
    fn one_target_fast_path() {
        let mut world = world();
        let generation = seed_calibrated(&mut world);
        let mut req = request(vec![target(2100, 2200)]);
        req.start_position = Some(pb::StartPosition {
            partition: 0,
            block: 500,
        });
        let response = call(&world, req);
        assert_eq!(status_of(&response), pb::PlanStatus::Ok);
        assert_eq!(response.hops.len(), 1);
        let hop = &response.hops[0];
        let hop_target = hop.target.as_ref().expect("echoed");
        assert_eq!(hop_target.start_block, 2100);
        assert_eq!(hop_target.tag, b"t-2100".to_vec());
        assert_eq!(hop.estimated_locate_ns, expected_hop_ns(500, 2100));
        assert_eq!(response.estimated_total_ns, expected_hop_ns(500, 2100));
        assert_eq!(response.calibration_generation, generation);
        let key = response.resolved_key.expect("both halves");
        assert_eq!(key.cartridge_generation, "LTO-8");
        assert_eq!(key.recording_format, "L8");
        assert!(!response.uses_estimated_eod_geometry);
    }

    /// A multi-target plan is a permutation with estimates describing
    /// the returned order, terminal hop included in the total when an
    /// end_position is supplied.
    #[test]
    fn plan_estimates_describe_the_returned_order() {
        let mut world = world();
        seed_calibrated(&mut world);
        let mut req = request(vec![
            target(3050, 3060), // EOD wrap
            target(100, 200),
            target(2100, 2200),
            target(1500, 1600),
        ]);
        req.end_position = Some(pb::StartPosition {
            partition: 0,
            block: 10,
        });
        let response = call(&world, req);
        assert_eq!(status_of(&response), pb::PlanStatus::Ok);
        assert_eq!(response.hops.len(), 4);
        assert!(
            response.uses_estimated_eod_geometry,
            "a target in the EOD wrap uses the estimated denominator"
        );

        let mut seen_tags: Vec<Vec<u8>> = response
            .hops
            .iter()
            .map(|hop| hop.target.as_ref().expect("echoed").tag.clone())
            .collect();
        seen_tags.sort();
        assert_eq!(
            seen_tags,
            vec![
                b"t-100".to_vec(),
                b"t-1500".to_vec(),
                b"t-2100".to_vec(),
                b"t-3050".to_vec()
            ],
            "a permutation of the input"
        );

        let mut from_block = 0u64;
        let mut expected_total = 0u128;
        for hop in &response.hops {
            let hop_target = hop.target.as_ref().expect("echoed");
            let expected = expected_hop_ns(from_block, hop_target.start_block);
            assert_eq!(hop.estimated_locate_ns, expected);
            expected_total += u128::from(expected);
            from_block = hop_target.end_block;
        }
        expected_total += u128::from(expected_hop_ns(from_block, 10));
        assert_eq!(
            u128::from(response.estimated_total_ns),
            expected_total,
            "the reported total includes the terminal hop"
        );
    }

    /// Highly dispersed completed spans: the flag is set even for
    /// targets on completed wraps, and the detail says the estimate is
    /// unreliable for this volume.
    #[test]
    fn dispersed_spans_set_the_flag_and_say_so() {
        let mut world = world();
        let HarvestTransition::Calibrated {
            write_epoch,
            calibration_generation,
        } = world
            .store
            .record_harvest_success(TAPE, 0)
            .expect("transition")
        else {
            panic!("calibrates");
        };
        // Completed spans 500, 1000, 1600: nMAD well above 0.05.
        world
            .index
            .upsert_wrap_map(&WrapMapCacheRecord {
                tape_uuid: TAPE,
                descriptors: vec![
                    StoredWrapDescriptor {
                        partition: 0,
                        wrap_number: 0,
                        end_loi: 499,
                    },
                    StoredWrapDescriptor {
                        partition: 0,
                        wrap_number: 1,
                        end_loi: 1499,
                    },
                    StoredWrapDescriptor {
                        partition: 0,
                        wrap_number: 2,
                        end_loi: 3099,
                    },
                    StoredWrapDescriptor {
                        partition: 0,
                        wrap_number: 3,
                        end_loi: 3600,
                    },
                ],
                mapped_extent_lba: 3600,
                write_epoch,
                calibration_generation,
                harvested_at_utc: "2026-08-04T00:00:00Z".to_string(),
            })
            .expect("seed dispersed map");
        let response = call(&world, request(vec![target(100, 200), target(700, 800)]));
        assert_eq!(status_of(&response), pb::PlanStatus::Ok);
        assert!(response.uses_estimated_eod_geometry);
        assert!(
            response.detail.contains("unreliable"),
            "detail states the estimate is unreliable: {}",
            response.detail
        );
    }

    /// No completed wrap (§11): the single-wrap volume still plans —
    /// the EOD wrap's own observed span is the denominator — and the
    /// response says an estimate was used.
    #[test]
    fn no_completed_wrap_plans_with_the_flag_set() {
        let mut world = world();
        let HarvestTransition::Calibrated {
            write_epoch,
            calibration_generation,
        } = world
            .store
            .record_harvest_success(TAPE, 0)
            .expect("transition")
        else {
            panic!("calibrates");
        };
        world
            .index
            .upsert_wrap_map(&WrapMapCacheRecord {
                tape_uuid: TAPE,
                descriptors: vec![StoredWrapDescriptor {
                    partition: 0,
                    wrap_number: 0,
                    end_loi: 900,
                }],
                mapped_extent_lba: 900,
                write_epoch,
                calibration_generation,
                harvested_at_utc: "2026-08-04T00:00:00Z".to_string(),
            })
            .expect("seed single-wrap map");
        let mut req = request(vec![target(100, 200), target(600, 700)]);
        req.cartridge.as_mut().unwrap().written_extent_lba = 900;
        let response = call(&world, req);
        assert_eq!(status_of(&response), pb::PlanStatus::Ok);
        assert_eq!(response.hops.len(), 2);
        assert!(
            response.uses_estimated_eod_geometry,
            "every target sits in the EOD wrap; its denominator is the observed span"
        );
    }

    /// The two objectives return different orders on the §8.4
    /// counterexample shape, and neither is coerced into the other.
    #[test]
    fn objectives_diverge_on_the_counterexample() {
        let mut world = world();
        seed_calibrated(&mut world);
        // Both targets on forward wrap 0; start at block 0. A begins
        // nearer but ends far; B is tiny and near.
        let targets = vec![target(1, 900), target(2, 2)];
        let order_under = |objective: pb::PlanObjective| {
            let mut req = request(targets.clone());
            req.objective = objective as i32;
            let response = call(&world, req);
            assert_eq!(status_of(&response), pb::PlanStatus::Ok);
            response
                .hops
                .iter()
                .map(|hop| hop.target.as_ref().expect("echoed").start_block)
                .collect::<Vec<_>>()
        };
        let total_time = order_under(pb::PlanObjective::MinTotalTime);
        let time_to_first = order_under(pb::PlanObjective::MinTimeToFirst);
        assert_eq!(time_to_first[0], 1, "MIN_TIME_TO_FIRST reaches A first");
        assert_ne!(
            total_time, time_to_first,
            "the objectives return different orders"
        );
    }

    // =================================================================
    //  The service surface itself
    // =================================================================

    /// Through ApiState and the generated tonic trait: the RPC is
    /// wired, authorizes a bare in-process request, and returns the
    /// same plan the core produces.
    #[tokio::test]
    async fn service_serves_plan_batch_read() {
        use pb::read_plan_service_server::ReadPlanService as _;

        let dir = tempfile::Builder::new()
            .prefix("rem-api-read-plan-svc")
            .tempdir()
            .expect("tempdir");
        let index = CatalogIndex::open(dir.path().join("index.sqlite")).expect("open index");
        // ApiState::new opens the calibration store beside the index;
        // seed through a handle onto the same directory.
        let store =
            CalibrationControlStore::open(dir.path().join("calibration")).expect("open store");
        let mut world = World {
            _dir: dir,
            index,
            store,
        };
        let generation = seed_calibrated(&mut world);
        let state = ApiState::new(
            CatalogIndex::open(world._dir.path().join("index.sqlite")).expect("reopen index"),
        );
        let service = state.read_plan_service();

        let response = service
            .plan_batch_read(Request::new(request(vec![target(100, 200)])))
            .await
            .expect("RPC succeeds")
            .into_inner();
        assert_eq!(status_of(&response), pb::PlanStatus::Ok);
        assert_eq!(response.max_targets, MAX_TARGETS);
        assert_eq!(response.calibration_generation, generation);

        // The malformed path surfaces as a real RPC error with the
        // BadRequest detail through the same surface.
        let mut bad = request(vec![target(100, 200)]);
        bad.tape_uuid = vec![0; 15];
        let status = service
            .plan_batch_read(Request::new(bad))
            .await
            .expect_err("malformed requests are RPC errors");
        assert_eq!(bad_request_fields(&status), vec!["tape_uuid".to_string()]);
    }
}
