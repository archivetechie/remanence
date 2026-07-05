/- Specification theorems for the parity-state extraction (SPEC.md T1–T5).

   Targets the Aeneas-generated definitions in `ParityState.Funs`. The Lean
   checker accepting this file with no remaining placeholders is the pilot's
   success criterion; proofs are searched by Leanstral but trusted only via
   `lake build`. -/
import ParityState.Funs

open Aeneas Aeneas.Std Result

namespace parity_state_verif

/- Formal-proof scope:
   the theorems below certify the extracted pure parity-state decision core
   against SPEC.md T1-T5. They do not prove the whole tape/catalog subsystem.
   If the mirrored Rust logic changes, update the extraction and rerun
   `lake build`; the Rust drift_guard is intended to catch stale proofs. -/

lemma checked_add_some_of_sum_lt (s c : Std.U64)
    (h : s.val + c.val < 2 ^ 64) :
    ∃ end1, U64.checked_add s c = some end1 ∧ end1.val = s.val + c.val := by
  have hspec := U64.checked_add_bv_spec s c
  cases hca : U64.checked_add s c with
  | none =>
      simp [hca, U64.max, U64.numBits] at hspec
      omega
  | some end1 =>
      simp [hca, U64.max, U64.numBits] at hspec
      exact ⟨end1, rfl, hspec.2.1⟩

lemma checked_add_none_iff_sum_ge (s c : Std.U64) :
    U64.checked_add s c = none ↔ 2 ^ 64 ≤ s.val + c.val := by
  constructor
  · intro hca
    have hspec := U64.checked_add_bv_spec s c
    simp [hca, U64.max, U64.numBits] at hspec
    omega
  · intro hge
    cases hca : U64.checked_add s c with
    | none => rfl
    | some end1 =>
        have hspec := U64.checked_add_bv_spec s c
        simp [hca, U64.max, U64.numBits] at hspec
        omega

lemma checked_add_some_val {s c end1 : Std.U64}
    (h : U64.checked_add s c = some end1) :
    end1.val = s.val + c.val := by
  have hspec := U64.checked_add_bv_spec s c
  simp [h, U64.max, U64.numBits] at hspec
  exact hspec.2.1

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
  have hc0 : c ≠ 0#u64 := by
    intro h
    scalar_tac
  rcases checked_add_some_of_sum_lt s c hno with ⟨end1, hadd, hend⟩
  unfold ObjectParityState.from_ordinals
  rw [hadd]
  simp [hc0]
  by_cases hprot : s.val + c.val ≤ W.val
  · have hend_le : end1.val ≤ W.val := by omega
    refine ⟨ObjectParityState.Protected, ?_, ?_, ?_, ?_⟩
    · simp [lift, hend_le]
    · constructor
      · intro _; exact hprot
      · intro _; rfl
    · constructor
      · intro h; cases h
      · intro h; omega
    · constructor
      · intro h; cases h
      · intro h; omega
  · have hnend_le : ¬ end1.val ≤ W.val := by omega
    simp [lift, hnend_le]
    by_cases hpending : W.val ≤ s.val
    · refine ⟨ObjectParityState.Pending, ?_, ?_, ?_, ?_⟩
      · simp [hpending]
      · constructor
        · intro h; cases h
        · intro h; omega
      · constructor
        · intro _; exact hpending
        · intro _; rfl
      · constructor
        · intro h; cases h
        · intro h; omega
    · have hpartial_left : s.val < W.val := Nat.lt_of_not_ge hpending
      have hpartial_right : W.val < s.val + c.val := Nat.lt_of_not_ge hprot
      refine ⟨ObjectParityState.Partial, ?_, ?_, ?_, ?_⟩
      · simp [hpending]
      · constructor
        · intro h; cases h
        · intro h; omega
      · constructor
        · intro h; cases h
        · intro h; omega
      · constructor
        · intro _; exact ⟨hpartial_left, hpartial_right⟩
        · intro _; rfl

/-- T2a — error completeness: `from_ordinals` returns `Err` exactly on an empty
    range or an overflowing one. -/
theorem from_ordinals_err_iff (s c W : Std.U64) :
    (∃ e, ObjectParityState.from_ordinals s c W = ok (.Err e)) ↔
      (c.val = 0 ∨ 2 ^ 64 ≤ s.val + c.val) := by
  constructor
  · intro h
    rcases h with ⟨e, he⟩
    unfold ObjectParityState.from_ordinals at he
    by_cases hc0 : c = 0#u64
    · left
      scalar_tac
    · right
      simp [hc0] at he
      cases hca : U64.checked_add s c with
      | none =>
          exact (checked_add_none_iff_sum_ge s c).mp hca
      | some end1 =>
          simp [hca, lift] at he
          by_cases hend : end1.val ≤ W.val
          · simp [hend] at he
          · simp [hend] at he
            by_cases hp : W.val ≤ s.val
            · simp [hp] at he
            · simp [hp] at he
  · intro h
    rcases h with (hc0 | hoverflow)
    · refine ⟨ParityError.ZeroDataBlocks, ?_⟩
      unfold ObjectParityState.from_ordinals
      have hcz : c = 0#u64 := by
        apply UScalar.eq_imp
        simpa using hc0
      simp [hcz]
    · refine ⟨ParityError.OrdinalRangeOverflow, ?_⟩
      unfold ObjectParityState.from_ordinals
      have hc0 : c ≠ 0#u64 := by
        intro hcz
        have hczv : c.val = 0 := by scalar_tac
        have hs_lt : s.val + c.val < 2 ^ 64 := by
          have hs := U64.lt_succ_max s
          omega
        omega
      have hca : U64.checked_add s c = none :=
        (checked_add_none_iff_sum_ge s c).mpr hoverflow
      simp [hc0, hca, lift]

/-- T2b — totality: `from_ordinals` never diverges or panics (the outer Aeneas
    monad always returns `ok`). -/
theorem from_ordinals_total (s c W : Std.U64) :
    ∃ r, ObjectParityState.from_ordinals s c W = ok r := by
  unfold ObjectParityState.from_ordinals
  by_cases hc0 : c = 0#u64
  · refine ⟨.Err ParityError.ZeroDataBlocks, ?_⟩
    simp [hc0]
  · simp [hc0]
    cases hca : U64.checked_add s c with
    | none =>
        refine ⟨.Err ParityError.OrdinalRangeOverflow, ?_⟩
        simp [lift]
    | some end1 =>
        simp [lift]
        by_cases hle : end1.val ≤ W.val
        · refine ⟨.Ok ObjectParityState.Protected, ?_⟩
          simp [hle]
        · by_cases hp : W.val ≤ s.val
          · refine ⟨.Ok ObjectParityState.Pending, ?_⟩
            simp [hle, hp]
          · refine ⟨.Ok ObjectParityState.Partial, ?_⟩
            simp [hle, hp]

/-- T3 — watermark-advance predicate safety (the crown theorem): any object the
    recomputation predicate skips has provably the same parity state at the old
    and new watermarks. This mechanizes the "never misses an object whose
    summary state can change" claim the Layer-5 catalog transaction relies on. -/
theorem includes_object_safe (r : ObjectParityStateUpdateRange) (s c : Std.U64)
    (hw : r.old_highest_protected_ordinal.val ≤ r.new_highest_protected_ordinal.val) :
    r.includes_object s c = ok (.Ok false) →
    ObjectParityState.from_ordinals s c r.old_highest_protected_ordinal =
      ObjectParityState.from_ordinals s c r.new_highest_protected_ordinal := by
  intro h
  by_cases hc0 : c = 0#u64
  · unfold ObjectParityStateUpdateRange.includes_object at h
    simp [hc0] at h
  · unfold ObjectParityStateUpdateRange.includes_object at h
    simp [hc0] at h
    cases hca : U64.checked_add s c with
    | none =>
        simp [hca, lift] at h
    | some end1 =>
        have hend : end1.val = s.val + c.val := checked_add_some_val hca
        have hcpos : 0 < c.val := by scalar_tac
        simp [hca, lift] at h
        by_cases hsnew : s.val < r.new_highest_protected_ordinal.val
        · simp [ObjectParityStateUpdateRange.first_parity_data_ordinal_upper_exclusive,
                ObjectParityStateUpdateRange.ordinal_end_exclusive_lower_exclusive,
                hsnew] at h
          have hend_old : end1.val ≤ r.old_highest_protected_ordinal.val := h
          have hend_new : end1.val ≤ r.new_highest_protected_ordinal.val := by omega
          unfold ObjectParityState.from_ordinals
          simp [hc0, hca, lift, hend_old, hend_new]
        · have hnew_le_s : r.new_highest_protected_ordinal.val ≤ s.val := by omega
          have hold_le_s : r.old_highest_protected_ordinal.val ≤ s.val := by omega
          have hend_gt_s : s.val < end1.val := by omega
          have hnot_end_old : ¬ end1.val ≤ r.old_highest_protected_ordinal.val := by omega
          have hnot_end_new : ¬ end1.val ≤ r.new_highest_protected_ordinal.val := by omega
          unfold ObjectParityState.from_ordinals
          simp [hc0, hca, lift, hnot_end_old, hnot_end_new, hold_le_s, hnew_le_s]

/-- T4a — monotonicity: a fully protected object stays protected under any
    watermark advance. -/
theorem state_monotone_protected (s c W W' : Std.U64) (hw : W.val ≤ W'.val) :
    ObjectParityState.from_ordinals s c W = ok (.Ok ObjectParityState.Protected) →
    ObjectParityState.from_ordinals s c W' = ok (.Ok ObjectParityState.Protected) := by
  intro h
  by_cases hc0 : c = 0#u64
  · unfold ObjectParityState.from_ordinals at h
    simp [hc0] at h
  · unfold ObjectParityState.from_ordinals at h ⊢
    simp [hc0] at h ⊢
    cases hca : U64.checked_add s c with
    | none =>
        simp [hca, lift] at h
    | some end1 =>
        simp [hca, lift] at h ⊢
        by_cases hendW : end1.val ≤ W.val
        · have hendW' : end1.val ≤ W'.val := by omega
          simp [hendW']
        · have hbad := h (by omega)
          by_cases hWs : W.val ≤ s.val
          · simp [hWs] at hbad
          · simp [hWs] at hbad

/-- T4b — monotonicity, dual: an object still pending at the new watermark was
    pending at the old one. -/
theorem state_monotone_pending (s c W W' : Std.U64) (hw : W.val ≤ W'.val) :
    ObjectParityState.from_ordinals s c W' = ok (.Ok ObjectParityState.Pending) →
    ObjectParityState.from_ordinals s c W = ok (.Ok ObjectParityState.Pending) := by
  intro h
  by_cases hc0 : c = 0#u64
  · unfold ObjectParityState.from_ordinals at h
    simp [hc0] at h
  · unfold ObjectParityState.from_ordinals at h ⊢
    simp [hc0] at h ⊢
    cases hca : U64.checked_add s c with
    | none =>
        simp [hca, lift] at h
    | some end1 =>
        simp [hca, lift] at h ⊢
        by_cases hendW' : end1.val ≤ W'.val
        · simp [hendW'] at h
        · simp [hendW'] at h
          by_cases hW's : W'.val ≤ s.val
          · have hWs : W.val ≤ s.val := by omega
            have hnot_endW : ¬ end1.val ≤ W.val := by omega
            simp [hnot_endW, hWs]
          · simp [hW's] at h

/-- T5a — recompute consistency: a recomputed state is exactly the state at the
    new watermark. -/
theorem recompute_object_sound (r : ObjectParityStateUpdateRange) (s c : Std.U64)
    (st : ObjectParityState) :
    r.recompute_object s c = ok (.Ok (some st)) →
    ObjectParityState.from_ordinals s c r.new_highest_protected_ordinal = ok (.Ok st) := by
  intro h
  unfold ObjectParityStateUpdateRange.recompute_object at h
  cases hincl : r.includes_object s c with
  | fail e =>
      simp [hincl] at h
  | div =>
      simp [hincl] at h
  | ok inc =>
      cases inc with
      | Err e =>
          simp [hincl, core.result.Result.Insts.CoreOpsTry.branch,
            core.result.Result.Insts.CoreOpsTryTraitFromResidualResultInfallible.from_residual] at h
      | Ok b =>
          cases b
          · simp [hincl, core.result.Result.Insts.CoreOpsTry.branch] at h
          · simp [hincl, core.result.Result.Insts.CoreOpsTry.branch] at h
            cases hfrom : ObjectParityState.from_ordinals s c r.new_highest_protected_ordinal with
            | fail e =>
                simp [hfrom] at h
            | div =>
                simp [hfrom] at h
            | ok out =>
                cases out with
                | Err e =>
                    simp [hfrom] at h
                | Ok st' =>
                    simp [hfrom] at h
                    cases h
                    rfl

/-- T5b — recompute consistency: `none` is returned only when the predicate
    excluded the object. -/
theorem recompute_object_none (r : ObjectParityStateUpdateRange) (s c : Std.U64) :
    r.recompute_object s c = ok (.Ok none) →
    r.includes_object s c = ok (.Ok false) := by
  intro h
  unfold ObjectParityStateUpdateRange.recompute_object at h
  cases hincl : r.includes_object s c with
  | fail e =>
      simp [hincl] at h
  | div =>
      simp [hincl] at h
  | ok inc =>
      cases inc with
      | Err e =>
          simp [hincl, core.result.Result.Insts.CoreOpsTry.branch,
            core.result.Result.Insts.CoreOpsTryTraitFromResidualResultInfallible.from_residual] at h
      | Ok b =>
          cases b
          · rfl
          · simp [hincl, core.result.Result.Insts.CoreOpsTry.branch] at h
            cases hfrom : ObjectParityState.from_ordinals s c r.new_highest_protected_ordinal with
            | fail e =>
                simp [hfrom] at h
            | div =>
                simp [hfrom] at h
            | ok out =>
                cases out with
                | Err e =>
                    simp [hfrom] at h
                | Ok st =>
                    simp [hfrom] at h

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
  unfold ObjectParityStateUpdateRange.from_watermark_advance
  by_cases hlt : wNew.val < wOld.val
  · simp [hlt]
  · simp [hlt]
    by_cases heq : wNew.val = wOld.val
    · have heq' : wNew = wOld := by
        apply UScalar.eq_imp
        exact heq
      simp [heq']
    · have hneq : wNew ≠ wOld := by
        intro h
        apply heq
        scalar_tac
      simp [heq, hneq]

end parity_state_verif
