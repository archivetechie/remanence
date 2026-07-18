/- Specification theorems for the pool-selection ranking extraction
   (SPEC.md P1-P6).

   Targets the Aeneas-generated definitions in `PoolSelection.Funs`. These
   theorems certify the pure ranking kernel used by the production
   `CompleteOrFill` and `FillOldest` policies: fit filtering, completion
   detection, leftover arithmetic, and the lexicographic key dominance rules.
   The Rust `drift_guard` test ties this proof-facing model back to production
   `crates/remanence-api/src/pool_selection.rs`. -/
import PoolSelection.Funs

open Aeneas Aeneas.Std Result

namespace PoolSelection

/- Formal-proof scope:
   the theorems below certify the extracted arithmetic and pairwise ranking
   predicates. They do not prove Rust iterator internals, `Vec`, tuple
   `min_by_key`, trait-object storage, or catalog/runtime projection. Those are
   covered by normal Rust tests and the extraction drift guard. -/

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
