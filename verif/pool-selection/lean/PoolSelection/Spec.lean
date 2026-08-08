/- Specification theorems for the pool-selection ranking extraction
   (SPEC.md P1-P7).

   Targets the Aeneas-generated definitions in `PoolSelection.Funs`. These
   theorems certify the pure ranking and admission kernels used by production:
   fit filtering, completion detection, leftover arithmetic, lexicographic key
   dominance, and capacity-admission branch order.
   The Rust `drift_guard` test ties this proof-facing model back to production
   `crates/remanence-api/src/pool_selection.rs`. -/
import PoolSelection.Funs

open Aeneas Aeneas.Std Result

namespace PoolSelection

/- Formal-proof scope:
   the theorems below certify the extracted arithmetic, pairwise ranking
   predicates, and pure admission disposition. They do not prove Rust iterator
   internals, `Vec`, tuple `min_by_key`, trait-object storage, catalog/runtime
   projection, or retry orchestration. Those are covered by normal Rust tests,
   the extraction drift guards, and caller/runtime obligations. -/

/-- P1a: a defined sufficient checked remainder is fitting. -/
theorem fits_some_true (candidate : TapeFitState)
    (projected_footprint remaining : Std.U64)
    (hsub : U64.checked_sub candidate.usable_bytes candidate.used_bytes =
      some remaining)
    (hfit : projected_footprint.val ≤ remaining.val) :
    fits candidate projected_footprint = ok true := by
  simp [fits, lift, hsub, hfit]

/-- P1b: a defined but insufficient checked remainder is not fitting. -/
theorem fits_some_false (candidate : TapeFitState)
    (projected_footprint remaining : Std.U64)
    (hsub : U64.checked_sub candidate.usable_bytes candidate.used_bytes =
      some remaining)
    (hshort : remaining.val < projected_footprint.val) :
    fits candidate projected_footprint = ok false := by
  simp [fits, lift, hsub, hshort]

/-- P1c: checked-subtraction failure cannot be reported as fitting. -/
theorem fits_none_spec (candidate : TapeFitState)
    (projected_footprint : Std.U64)
    (hsub : U64.checked_sub candidate.usable_bytes candidate.used_bytes = none) :
    fits candidate projected_footprint = ok false := by
  simp [fits, lift, hsub]

/-- P2a: reaching the low watermark after saturating addition completes. -/
theorem completes_tape_true (candidate : TapeFitState)
    (projected_footprint : Std.U64)
    (hreached : candidate.low_bytes.val ≤
      (core.num.U64.saturating_add candidate.used_bytes projected_footprint).val) :
    completes_tape candidate projected_footprint = ok true := by
  simp [completes_tape, lift, hreached]

/-- P2b: remaining below low after saturating addition does not complete. -/
theorem completes_tape_false (candidate : TapeFitState)
    (projected_footprint : Std.U64)
    (hbelow :
      (core.num.U64.saturating_add candidate.used_bytes projected_footprint).val <
        candidate.low_bytes.val) :
    completes_tape candidate projected_footprint = ok false := by
  simp [completes_tape, lift, hbelow]

/-- P3: leftover is exactly the two production saturating subtractions. -/
theorem leftover_after_write_spec (candidate : TapeFitState)
    (projected_footprint : Std.U64) :
    leftover_after_write candidate projected_footprint =
      ok (core.num.U64.saturating_sub
        (core.num.U64.saturating_sub candidate.usable_bytes candidate.used_bytes)
        projected_footprint) := by
  simp [leftover_after_write, lift]

/-- P7: an invalid low/high ordering is a terminal policy rejection. -/
theorem admission_rejects_low_above_high (input : CapacityAdmissionInput)
    (h : input.high_watermark_blocks.val < input.low_watermark_blocks.val) :
    capacity_admission_disposition input =
      ok AdmissionDisposition.RejectInvalidCapacityPolicy := by
  simp [capacity_admission_disposition, h]

/-- P7: a high watermark above conservative capacity is also a terminal policy
    rejection, regardless of later arithmetic. -/
theorem admission_rejects_high_above_capacity (input : CapacityAdmissionInput)
    (h : input.capacity_blocks.val < input.high_watermark_blocks.val) :
    capacity_admission_disposition input =
      ok AdmissionDisposition.RejectInvalidCapacityPolicy := by
  simp [capacity_admission_disposition, h]

/-- P7: overflow while projecting the Object boundary fails closed to retry
    after a valid policy has been established. -/
theorem admission_retries_when_projected_boundary_overflows
    (input : CapacityAdmissionInput)
    (hLowHigh : input.low_watermark_blocks.val ≤ input.high_watermark_blocks.val)
    (hHighCapacity : input.high_watermark_blocks.val ≤ input.capacity_blocks.val)
    (hProjected : U64.checked_add input.current_used_blocks
      input.object_commit_charge_blocks = none) :
    capacity_admission_disposition input =
      ok AdmissionDisposition.FinalizePrefixAndRetry := by
  have hnLowHigh : ¬ input.high_watermark_blocks.val <
      input.low_watermark_blocks.val := by omega
  have hnHighCapacity : ¬ input.capacity_blocks.val <
      input.high_watermark_blocks.val := by omega
  simp [capacity_admission_disposition, lift, hProjected, hnLowHigh,
    hnHighCapacity]

/-- P7: a successfully projected boundary above H retries before attempting
    the close-bound addition. -/
theorem admission_retries_above_high
    (input : CapacityAdmissionInput) (projected : Std.U64)
    (hLowHigh : input.low_watermark_blocks.val ≤ input.high_watermark_blocks.val)
    (hHighCapacity : input.high_watermark_blocks.val ≤ input.capacity_blocks.val)
    (hProjected : U64.checked_add input.current_used_blocks
      input.object_commit_charge_blocks = some projected)
    (hOverHigh : input.high_watermark_blocks.val < projected.val) :
    capacity_admission_disposition input =
      ok AdmissionDisposition.FinalizePrefixAndRetry := by
  have hnLowHigh : ¬ input.high_watermark_blocks.val <
      input.low_watermark_blocks.val := by omega
  have hnHighCapacity : ¬ input.capacity_blocks.val <
      input.high_watermark_blocks.val := by omega
  simp [capacity_admission_disposition, lift, hProjected, hnLowHigh,
    hnHighCapacity, hOverHigh]

/-- P7: overflow while adding CloseBound retries before the below-low branch. -/
theorem admission_retries_when_close_boundary_overflows
    (input : CapacityAdmissionInput) (projected : Std.U64)
    (hLowHigh : input.low_watermark_blocks.val ≤ input.high_watermark_blocks.val)
    (hHighCapacity : input.high_watermark_blocks.val ≤ input.capacity_blocks.val)
    (hProjected : U64.checked_add input.current_used_blocks
      input.object_commit_charge_blocks = some projected)
    (hAtMostHigh : projected.val ≤ input.high_watermark_blocks.val)
    (hRequired : U64.checked_add projected input.close_bound_blocks = none) :
    capacity_admission_disposition input =
      ok AdmissionDisposition.FinalizePrefixAndRetry := by
  have hnLowHigh : ¬ input.high_watermark_blocks.val <
      input.low_watermark_blocks.val := by omega
  have hnHighCapacity : ¬ input.capacity_blocks.val <
      input.high_watermark_blocks.val := by omega
  have hnOverHigh : ¬ input.high_watermark_blocks.val < projected.val := by omega
  simp [capacity_admission_disposition, lift, hProjected, hRequired, hnLowHigh,
    hnHighCapacity, hnOverHigh]

/-- P7: close-proof failure dominates the below-low remain-open branch. -/
theorem admission_retries_when_close_does_not_fit
    (input : CapacityAdmissionInput) (projected required : Std.U64)
    (hLowHigh : input.low_watermark_blocks.val ≤ input.high_watermark_blocks.val)
    (hHighCapacity : input.high_watermark_blocks.val ≤ input.capacity_blocks.val)
    (hProjected : U64.checked_add input.current_used_blocks
      input.object_commit_charge_blocks = some projected)
    (hAtMostHigh : projected.val ≤ input.high_watermark_blocks.val)
    (hRequired : U64.checked_add projected input.close_bound_blocks = some required)
    (hTooLarge : input.capacity_blocks.val < required.val) :
    capacity_admission_disposition input =
      ok AdmissionDisposition.FinalizePrefixAndRetry := by
  have hnLowHigh : ¬ input.high_watermark_blocks.val <
      input.low_watermark_blocks.val := by omega
  have hnHighCapacity : ¬ input.capacity_blocks.val <
      input.high_watermark_blocks.val := by omega
  have hnOverHigh : ¬ input.high_watermark_blocks.val < projected.val := by omega
  simp [capacity_admission_disposition, lift, hProjected, hRequired,
    hnLowHigh, hnHighCapacity, hnOverHigh, hTooLarge]

/-- P7: below low remains open after the high and close proofs succeed. -/
theorem admission_remains_open_below_low
    (input : CapacityAdmissionInput) (projected required : Std.U64)
    (hLowHigh : input.low_watermark_blocks.val ≤ input.high_watermark_blocks.val)
    (hHighCapacity : input.high_watermark_blocks.val ≤ input.capacity_blocks.val)
    (hProjected : U64.checked_add input.current_used_blocks
      input.object_commit_charge_blocks = some projected)
    (hAtMostHigh : projected.val ≤ input.high_watermark_blocks.val)
    (hRequired : U64.checked_add projected input.close_bound_blocks = some required)
    (hFits : required.val ≤ input.capacity_blocks.val)
    (hBelowLow : projected.val < input.low_watermark_blocks.val) :
    capacity_admission_disposition input =
      ok AdmissionDisposition.AdmitRemainOpen := by
  have hnLowHigh : ¬ input.high_watermark_blocks.val <
      input.low_watermark_blocks.val := by omega
  have hnHighCapacity : ¬ input.capacity_blocks.val <
      input.high_watermark_blocks.val := by omega
  have hnOverHigh : ¬ input.high_watermark_blocks.val < projected.val := by omega
  have hnOverCapacity : ¬ input.capacity_blocks.val < required.val := by omega
  simp [capacity_admission_disposition, lift, hProjected, hRequired,
    hnLowHigh, hnHighCapacity, hnOverHigh, hnOverCapacity, hBelowLow]

/-- P7: equality at low or any higher boundary through high admits and seals. -/
theorem admission_finalizes_in_closing_band
    (input : CapacityAdmissionInput) (projected required : Std.U64)
    (hLowHigh : input.low_watermark_blocks.val ≤ input.high_watermark_blocks.val)
    (hHighCapacity : input.high_watermark_blocks.val ≤ input.capacity_blocks.val)
    (hProjected : U64.checked_add input.current_used_blocks
      input.object_commit_charge_blocks = some projected)
    (hAtMostHigh : projected.val ≤ input.high_watermark_blocks.val)
    (hRequired : U64.checked_add projected input.close_bound_blocks = some required)
    (hFits : required.val ≤ input.capacity_blocks.val)
    (hAtLeastLow : input.low_watermark_blocks.val ≤ projected.val) :
    capacity_admission_disposition input =
      ok AdmissionDisposition.AdmitThenFinalize := by
  have hnLowHigh : ¬ input.high_watermark_blocks.val <
      input.low_watermark_blocks.val := by omega
  have hnHighCapacity : ¬ input.capacity_blocks.val <
      input.high_watermark_blocks.val := by omega
  have hnOverHigh : ¬ input.high_watermark_blocks.val < projected.val := by omega
  have hnOverCapacity : ¬ input.capacity_blocks.val < required.val := by omega
  have hnBelowLow : ¬ projected.val < input.low_watermark_blocks.val := by omega
  simp [capacity_admission_disposition, lift, hProjected, hRequired,
    hnLowHigh, hnHighCapacity, hnOverHigh, hnOverCapacity, hnBelowLow]

theorem loaded_key_loaded (candidate : TapeFitState)
    (h : candidate.already_loaded = true) :
    loaded_key candidate = ok 0#u8 := by
  simp [loaded_key, h]

theorem loaded_key_unloaded (candidate : TapeFitState)
    (h : candidate.already_loaded = false) :
    loaded_key candidate = ok 1#u8 := by
  simp [loaded_key, h]


/-- P4a: on the completing tier, lower leftover dominates every later
    tie-breaker. -/
theorem completing_rank_lower_leftover_wins
    (left right : TapeFitState) (projected_footprint left_leftover right_leftover : Std.U64)
    (hleft : leftover_after_write left projected_footprint = ok left_leftover)
    (hright : leftover_after_write right projected_footprint = ok right_leftover)
    (hlt : left_leftover.val < right_leftover.val) :
    complete_or_fill_completing_precedes_or_ties left right projected_footprint =
      ok true := by
  unfold complete_or_fill_completing_precedes_or_ties
  simp [hleft, hright, hlt]

theorem completing_rank_higher_leftover_loses
    (left right : TapeFitState) (projected_footprint left_leftover right_leftover : Std.U64)
    (hleft : leftover_after_write left projected_footprint = ok left_leftover)
    (hright : leftover_after_write right projected_footprint = ok right_leftover)
    (hlt : right_leftover.val < left_leftover.val) :
    complete_or_fill_completing_precedes_or_ties left right projected_footprint =
      ok false := by
  unfold complete_or_fill_completing_precedes_or_ties
  have hle : right_leftover.val ≤ left_leftover.val := Nat.le_of_lt hlt
  simp [hleft, hright, hlt, hle]

/-- P4b: when leftover ties, already-loaded wins in the completing tier. -/
theorem completing_rank_loaded_wins_after_leftover_tie
    (left right : TapeFitState) (projected_footprint leftover : Std.U64)
    (hleft : leftover_after_write left projected_footprint = ok leftover)
    (hright : leftover_after_write right projected_footprint = ok leftover)
    (hloaded : left.already_loaded = true)
    (hunloaded : right.already_loaded = false) :
    complete_or_fill_completing_precedes_or_ties left right projected_footprint =
      ok true := by
  unfold complete_or_fill_completing_precedes_or_ties
  simp [hleft, hright, loaded_key, hloaded, hunloaded]

/-- P4c: after leftover and loaded-state ties, lower barcode wins in the
    completing tier. -/
theorem completing_rank_barcode_wins_after_loaded_tie
    (left right : TapeFitState) (projected_footprint leftover : Std.U64)
    (loaded : Bool)
    (hleft : leftover_after_write left projected_footprint = ok leftover)
    (hright : leftover_after_write right projected_footprint = ok leftover)
    (hleft_loaded : left.already_loaded = loaded)
    (hright_loaded : right.already_loaded = loaded)
    (hbarcode : left.barcode_order.val < right.barcode_order.val) :
    complete_or_fill_completing_precedes_or_ties left right projected_footprint =
      ok true := by
  unfold complete_or_fill_completing_precedes_or_ties
  cases loaded <;> simp [hleft, hright, hleft_loaded, hright_loaded,
    loaded_key, hbarcode]

/-- P4d: after earlier completing-tier keys tie, lower/equal UUID is the final
    deterministic tie-break. -/
theorem completing_rank_uuid_breaks_final_tie
    (left right : TapeFitState) (projected_footprint leftover : Std.U64)
    (loaded : Bool)
    (hleft : leftover_after_write left projected_footprint = ok leftover)
    (hright : leftover_after_write right projected_footprint = ok leftover)
    (hleft_loaded : left.already_loaded = loaded)
    (hright_loaded : right.already_loaded = loaded)
    (hbarcode : left.barcode_order = right.barcode_order)
    (huuid : left.tape_uuid.val ≤ right.tape_uuid.val) :
    complete_or_fill_completing_precedes_or_ties left right projected_footprint =
      ok true := by
  unfold complete_or_fill_completing_precedes_or_ties
  cases loaded <;> simp [hleft, hright, hleft_loaded, hright_loaded,
    hbarcode, loaded_key, huuid]

/-- P5a: on the non-completing fill tier, already-loaded wins first. -/
theorem complete_or_fill_fill_loaded_wins
    (left right : TapeFitState)
    (hloaded : left.already_loaded = true)
    (hunloaded : right.already_loaded = false) :
    complete_or_fill_fill_precedes_or_ties left right = ok true := by
  unfold complete_or_fill_fill_precedes_or_ties
  simp [loaded_key, hloaded, hunloaded]

/-- P5b: after loaded-state ties, lower barcode wins in the `CompleteOrFill`
    fill tier. -/
theorem complete_or_fill_fill_barcode_wins_after_loaded_tie
    (left right : TapeFitState)
    (loaded : Bool)
    (hleft_loaded : left.already_loaded = loaded)
    (hright_loaded : right.already_loaded = loaded)
    (hbarcode : left.barcode_order.val < right.barcode_order.val) :
    complete_or_fill_fill_precedes_or_ties left right = ok true := by
  unfold complete_or_fill_fill_precedes_or_ties
  cases loaded <;> simp [hleft_loaded, hright_loaded, loaded_key, hbarcode]

/-- P5c: after loaded-state and barcode ties, UUID is the final deterministic
    `CompleteOrFill` fill-tier tie-break. -/
theorem complete_or_fill_fill_uuid_breaks_final_tie
    (left right : TapeFitState)
    (loaded : Bool)
    (hleft_loaded : left.already_loaded = loaded)
    (hright_loaded : right.already_loaded = loaded)
    (hbarcode : left.barcode_order = right.barcode_order)
    (huuid : left.tape_uuid.val ≤ right.tape_uuid.val) :
    complete_or_fill_fill_precedes_or_ties left right = ok true := by
  unfold complete_or_fill_fill_precedes_or_ties
  cases loaded <;> simp [hleft_loaded, hright_loaded, hbarcode, loaded_key,
    huuid]

/-- P6a: `FillOldest` ranks by barcode before loaded-state. -/
theorem fill_oldest_barcode_wins_first
    (left right : TapeFitState)
    (hbarcode : left.barcode_order.val < right.barcode_order.val) :
    fill_oldest_precedes_or_ties left right = ok true := by
  unfold fill_oldest_precedes_or_ties
  simp [hbarcode]

/-- P6b: after barcode ties, `FillOldest` prefers already-loaded tapes. -/
theorem fill_oldest_loaded_wins_after_barcode_tie
    (left right : TapeFitState)
    (hbarcode : left.barcode_order = right.barcode_order)
    (hloaded : left.already_loaded = true)
    (hunloaded : right.already_loaded = false) :
    fill_oldest_precedes_or_ties left right = ok true := by
  unfold fill_oldest_precedes_or_ties
  simp [hbarcode, loaded_key, hloaded, hunloaded]

/-- P6c: after barcode and loaded-state ties, UUID is the final deterministic
    `FillOldest` tie-break. -/
theorem fill_oldest_uuid_breaks_final_tie
    (left right : TapeFitState)
    (hbarcode : left.barcode_order = right.barcode_order)
    (loaded : Bool)
    (hleft_loaded : left.already_loaded = loaded)
    (hright_loaded : right.already_loaded = loaded)
    (huuid : left.tape_uuid.val ≤ right.tape_uuid.val) :
    fill_oldest_precedes_or_ties left right = ok true := by
  unfold fill_oldest_precedes_or_ties
  cases loaded <;> simp [hbarcode, hleft_loaded, hright_loaded, loaded_key,
    huuid]

end PoolSelection
