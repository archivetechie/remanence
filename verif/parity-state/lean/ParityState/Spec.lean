/- Specification theorems for the parity-state extraction (SPEC.md T1–T5).

   Targets the Aeneas-generated definitions in `ParityState.Funs`. The Lean
   checker accepting this file with no remaining placeholders is the pilot's
   success criterion; proofs are searched by Leanstral but trusted only via
   `lake build`. -/
import ParityState.Funs

open Aeneas Aeneas.Std Result

namespace parity_state_verif

/-- T1 — classification correctness (`docs/layer3c-design.md` §7.2.1 / §10.1):
    for a nonempty, non-overflowing ordinal range `[s, s+c)` and watermark `W`,
    `from_ordinals` succeeds and classifies exactly by the spec. -/
theorem from_ordinals_spec (s c W : Std.U64)
    (hc : 0 < c.val) (hno : s.val + c.val < 2 ^ 64) :
    ∃ st,
      ObjectParityState.from_ordinals s c W = ok (.Ok st) ∧
      (st = ObjectParityState.Protected ↔ s.val + c.val ≤ W.val) ∧
      (st = ObjectParityState.Pending ↔ W.val ≤ s.val) ∧
      (st = ObjectParityState.Partial ↔ s.val < W.val ∧ W.val < s.val + c.val) := by
  sorry

/-- T2a — error completeness: `from_ordinals` returns `Err` exactly on an empty
    range or an overflowing one. -/
theorem from_ordinals_err_iff (s c W : Std.U64) :
    (∃ e, ObjectParityState.from_ordinals s c W = ok (.Err e)) ↔
      (c.val = 0 ∨ 2 ^ 64 ≤ s.val + c.val) := by
  sorry

/-- T2b — totality: `from_ordinals` never diverges or panics (the outer Aeneas
    monad always returns `ok`). -/
theorem from_ordinals_total (s c W : Std.U64) :
    ∃ r, ObjectParityState.from_ordinals s c W = ok r := by
  sorry

/-- T3 — watermark-advance predicate safety (the crown theorem): any object the
    recomputation predicate skips has provably the same parity state at the old
    and new watermarks. This mechanizes the "never misses an object whose
    summary state can change" claim the Layer-5 catalog transaction relies on. -/
theorem includes_object_safe (r : ObjectParityStateUpdateRange) (s c : Std.U64)
    (hw : r.old_highest_protected_ordinal.val ≤ r.new_highest_protected_ordinal.val) :
    r.includes_object s c = ok (.Ok false) →
    ObjectParityState.from_ordinals s c r.old_highest_protected_ordinal =
      ObjectParityState.from_ordinals s c r.new_highest_protected_ordinal := by
  sorry

/-- T4a — monotonicity: a fully protected object stays protected under any
    watermark advance. -/
theorem state_monotone_protected (s c W W' : Std.U64) (hw : W.val ≤ W'.val) :
    ObjectParityState.from_ordinals s c W = ok (.Ok ObjectParityState.Protected) →
    ObjectParityState.from_ordinals s c W' = ok (.Ok ObjectParityState.Protected) := by
  sorry

/-- T4b — monotonicity, dual: an object still pending at the new watermark was
    pending at the old one. -/
theorem state_monotone_pending (s c W W' : Std.U64) (hw : W.val ≤ W'.val) :
    ObjectParityState.from_ordinals s c W' = ok (.Ok ObjectParityState.Pending) →
    ObjectParityState.from_ordinals s c W = ok (.Ok ObjectParityState.Pending) := by
  sorry

/-- T5a — recompute consistency: a recomputed state is exactly the state at the
    new watermark. -/
theorem recompute_object_sound (r : ObjectParityStateUpdateRange) (s c : Std.U64)
    (st : ObjectParityState) :
    r.recompute_object s c = ok (.Ok (some st)) →
    ObjectParityState.from_ordinals s c r.new_highest_protected_ordinal = ok (.Ok st) := by
  sorry

/-- T5b — recompute consistency: `none` is returned only when the predicate
    excluded the object. -/
theorem recompute_object_none (r : ObjectParityStateUpdateRange) (s c : Std.U64) :
    r.recompute_object s c = ok (.Ok none) →
    r.includes_object s c = ok (.Ok false) := by
  sorry

/-- Auxiliary — `from_watermark_advance` construction spec: rejects a backwards
    move, returns `none` on no movement, and otherwise returns exactly the
    ordered pair. -/
theorem from_watermark_advance_spec (wOld wNew : Std.U64) :
    ObjectParityStateUpdateRange.from_watermark_advance wOld wNew =
      (if wNew.val < wOld.val then
        ok (.Err ParityError.WatermarkMovedBackwards)
      else if wNew.val = wOld.val then
        ok (.Ok none)
      else
        ok (.Ok (some ⟨wOld, wNew⟩))) := by
  sorry

end parity_state_verif
