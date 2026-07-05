/- Specification theorems for the parity-mapping extraction (SPEC.md M1-M5).

   Targets the Aeneas-generated definitions in `ParityMapping.Funs`. The Lean
   checker accepting this file with no remaining placeholders is the success
   criterion; the generated file is trusted only through Aeneas plus Lean, and
   the Rust `drift_guard` test ties the extraction back to production
   `crates/remanence-parity/src/mapping.rs`. -/
import ParityMapping.Funs

open Aeneas Aeneas.Std Result

namespace parity_mapping_verif

/- Formal-proof scope:
   these theorems certify the extracted pure ordinal/stripe arithmetic for the
   parity mapping core: epoch size, row-major coordinate production, successful
   data-coordinate round trip, and rejection of invalid reverse coordinates.
   They do not prove the whole parity planner, storage I/O, or the Rust u32/u16
   casts in production `StripeAddress`; those are guarded separately by the
   extraction drift test and normal Rust tests. -/

lemma checked_mul_some_of_prod_lt (a b : Std.U64)
    (h : a.val * b.val < 2 ^ 64) :
    ∃ prod, U64.checked_mul a b = some prod ∧ prod.val = a.val * b.val := by
  have hspec := U64.checked_mul_bv_spec a b
  cases hmul : U64.checked_mul a b with
  | none =>
      simp [hmul, U64.max, U64.numBits] at hspec
      omega
  | some prod =>
      simp [hmul, U64.max, U64.numBits] at hspec
      exact ⟨prod, rfl, hspec.2.1⟩

lemma checked_add_some_of_sum_lt (a b : Std.U64)
    (h : a.val + b.val < 2 ^ 64) :
    ∃ sum, U64.checked_add a b = some sum ∧ sum.val = a.val + b.val := by
  have hspec := U64.checked_add_bv_spec a b
  cases hadd : U64.checked_add a b with
  | none =>
      simp [hadd, U64.max, U64.numBits] at hspec
      omega
  | some sum =>
      simp [hadd, U64.max, U64.numBits] at hspec
      exact ⟨sum, rfl, hspec.2.1⟩

lemma u64_div_ok_val (x y : Std.U64) (hy : y.val ≠ 0) :
    ∃ z, x / y = ok z ∧ z.val = x.val / y.val := by
  have hspec := U64.div_spec x (y := y) hy
  cases hdiv : x / y with
  | ok z =>
      simp [hdiv] at hspec
      exact ⟨z, rfl, hspec⟩
  | fail e =>
      simp [hdiv] at hspec
  | div =>
      simp [hdiv] at hspec

lemma u64_rem_ok_val (x y : Std.U64) (hy : y.val ≠ 0) :
    ∃ z, x % y = ok z ∧ z.val = x.val % y.val := by
  have hspec := U64.rem_spec x (y := y) hy
  cases hrem : x % y with
  | ok z =>
      simp [hrem] at hspec
      exact ⟨z, rfl, hspec⟩
  | fail e =>
      simp [hrem] at hspec
  | div =>
      simp [hrem] at hspec

/-- M1 -- under a positive, non-overflowing scheme, the extracted epoch-size
    helper returns exactly `stripes_per_neighborhood * data_blocks_per_stripe`. -/
theorem data_shards_per_epoch_spec (scheme : ParityScheme)
    (hS : 0 < scheme.stripes_per_neighborhood.val)
    (hk : 0 < scheme.data_blocks_per_stripe.val)
    (hprod :
      scheme.stripes_per_neighborhood.val * scheme.data_blocks_per_stripe.val < 2 ^ 64) :
    ∃ epochData,
      data_shards_per_epoch scheme = ok (.Ok epochData) ∧
      epochData.val =
        scheme.stripes_per_neighborhood.val * scheme.data_blocks_per_stripe.val := by
  rcases checked_mul_some_of_prod_lt scheme.stripes_per_neighborhood
      scheme.data_blocks_per_stripe hprod with ⟨epochData, hmul, hval⟩
  have hnonzero : epochData ≠ 0#u64 := by
    intro hz
    have hzv : epochData.val = 0 := by scalar_tac
    have hpos : 0 < epochData.val := by
      rw [hval]
      exact Nat.mul_pos hS hk
    omega
  refine ⟨epochData, ?_, hval⟩
  unfold data_shards_per_epoch
  simp [hmul, lift, hnonzero]

/-- M2/M3 -- `ordinal_to_stripe` produces the row-major coordinates described
    in SPEC.md, and the produced stripe/data coordinates are in scheme bounds. -/
theorem ordinal_to_stripe_shape (ordinal : Std.U64) (scheme : ParityScheme)
    (hS : 0 < scheme.stripes_per_neighborhood.val)
    (hk : 0 < scheme.data_blocks_per_stripe.val)
    (hprod :
      scheme.stripes_per_neighborhood.val * scheme.data_blocks_per_stripe.val < 2 ^ 64) :
    ∃ addr dataIndex,
      ordinal_to_stripe ordinal scheme = ok (.Ok addr) ∧
      addr.neighborhood.val = ordinal.val /
        (scheme.stripes_per_neighborhood.val * scheme.data_blocks_per_stripe.val) ∧
      addr.stripe_index.val = (ordinal.val %
        (scheme.stripes_per_neighborhood.val * scheme.data_blocks_per_stripe.val)) %
        scheme.stripes_per_neighborhood.val ∧
      addr.position = StripePosition.Data dataIndex ∧
      dataIndex.val = (ordinal.val %
        (scheme.stripes_per_neighborhood.val * scheme.data_blocks_per_stripe.val)) /
        scheme.stripes_per_neighborhood.val ∧
      addr.stripe_index.val < scheme.stripes_per_neighborhood.val ∧
      dataIndex.val < scheme.data_blocks_per_stripe.val := by
  rcases data_shards_per_epoch_spec scheme hS hk hprod with
    ⟨epochData, hepoch, hepochVal⟩
  have hepochNz : epochData.val ≠ 0 := by
    have hpos : 0 < epochData.val := by
      rw [hepochVal]
      exact Nat.mul_pos hS hk
    omega
  rcases u64_div_ok_val ordinal epochData hepochNz with
    ⟨epoch, hdivEpoch, hEpochVal⟩
  rcases u64_rem_ok_val ordinal epochData hepochNz with
    ⟨ordinalInEpoch, hremEpoch, hOrdinalInEpochVal⟩
  have hSNz : scheme.stripes_per_neighborhood.val ≠ 0 := by omega
  rcases u64_rem_ok_val ordinalInEpoch scheme.stripes_per_neighborhood hSNz with
    ⟨stripe, hremStripe, hStripeVal⟩
  rcases u64_div_ok_val ordinalInEpoch scheme.stripes_per_neighborhood hSNz with
    ⟨dataIndex, hdivData, hDataVal⟩
  refine ⟨{
    neighborhood := epoch,
    stripe_index := stripe,
    position := StripePosition.Data dataIndex
  }, dataIndex, ?_, ?_, ?_, rfl, ?_, ?_, ?_⟩
  · unfold ordinal_to_stripe
    simp [hepoch, core.result.Result.Insts.CoreOpsTry.branch, hdivEpoch, hremEpoch,
      hremStripe, hdivData]
  · simpa [hepochVal] using hEpochVal
  · calc
      stripe.val = ordinalInEpoch.val % scheme.stripes_per_neighborhood.val := hStripeVal
      _ = (ordinal.val % (scheme.stripes_per_neighborhood.val *
            scheme.data_blocks_per_stripe.val)) % scheme.stripes_per_neighborhood.val := by
          simp [hOrdinalInEpochVal, hepochVal]
  · calc
      dataIndex.val =
          ordinalInEpoch.val / scheme.stripes_per_neighborhood.val := hDataVal
      _ = (ordinal.val % (scheme.stripes_per_neighborhood.val *
            scheme.data_blocks_per_stripe.val)) / scheme.stripes_per_neighborhood.val := by
          simp [hOrdinalInEpochVal, hepochVal]
  · rw [hStripeVal]
    exact Nat.mod_lt _ hS
  · rw [hDataVal, hOrdinalInEpochVal, hepochVal]
    exact Nat.div_lt_of_lt_mul (Nat.mod_lt _ (Nat.mul_pos hS hk))

/-- M4 -- converting any valid-scheme ordinal to a stripe address and back
    returns exactly the original ordinal. -/
theorem ordinal_to_stripe_round_trip (ordinal : Std.U64) (scheme : ParityScheme)
    (hS : 0 < scheme.stripes_per_neighborhood.val)
    (hk : 0 < scheme.data_blocks_per_stripe.val)
    (hprod :
      scheme.stripes_per_neighborhood.val * scheme.data_blocks_per_stripe.val < 2 ^ 64) :
    ∃ addr,
      ordinal_to_stripe ordinal scheme = ok (.Ok addr) ∧
      stripe_data_to_ordinal addr scheme = ok (.Ok ordinal) := by
  rcases data_shards_per_epoch_spec scheme hS hk hprod with
    ⟨epochData, hepoch, hepochVal⟩
  have hepochNz : epochData.val ≠ 0 := by
    have hpos : 0 < epochData.val := by
      rw [hepochVal]
      exact Nat.mul_pos hS hk
    omega
  rcases u64_div_ok_val ordinal epochData hepochNz with
    ⟨epoch, hdivEpoch, hEpochVal⟩
  rcases u64_rem_ok_val ordinal epochData hepochNz with
    ⟨ordinalInEpoch, hremEpoch, hOrdinalInEpochVal⟩
  have hSNz : scheme.stripes_per_neighborhood.val ≠ 0 := by omega
  rcases u64_rem_ok_val ordinalInEpoch scheme.stripes_per_neighborhood hSNz with
    ⟨stripe, hremStripe, hStripeVal⟩
  rcases u64_div_ok_val ordinalInEpoch scheme.stripes_per_neighborhood hSNz with
    ⟨dataIndex, hdivData, hDataVal⟩
  have hStripeBound : stripe.val < scheme.stripes_per_neighborhood.val := by
    rw [hStripeVal]
    exact Nat.mod_lt _ hS
  have hDataBound : dataIndex.val < scheme.data_blocks_per_stripe.val := by
    rw [hDataVal, hOrdinalInEpochVal, hepochVal]
    exact Nat.div_lt_of_lt_mul (Nat.mod_lt _ (Nat.mul_pos hS hk))
  have hStripeNotGe : ¬ stripe >= scheme.stripes_per_neighborhood := by
    scalar_tac
  have hDataNotGe : ¬ dataIndex >= scheme.data_blocks_per_stripe := by
    scalar_tac
  have hn64 : ordinal.val < 2 ^ 64 := by
    have := U64.lt_succ_max ordinal
    omega
  have hBaseMulLt : epoch.val * epochData.val < 2 ^ 64 := by
    have hle := Nat.div_mul_le_self ordinal.val epochData.val
    rw [hEpochVal]
    omega
  rcases checked_mul_some_of_prod_lt epoch epochData hBaseMulLt with
    ⟨base, hmulBase, hBaseVal⟩
  have hOffsetMulLt :
      dataIndex.val * scheme.stripes_per_neighborhood.val < 2 ^ 64 := by
    have hle :=
      Nat.div_mul_le_self ordinalInEpoch.val scheme.stripes_per_neighborhood.val
    rw [hDataVal]
    have := U64.lt_succ_max ordinalInEpoch
    omega
  rcases checked_mul_some_of_prod_lt dataIndex scheme.stripes_per_neighborhood
      hOffsetMulLt with ⟨offset, hmulOffset, hOffsetVal⟩
  have hBaseAddLt : base.val + offset.val < 2 ^ 64 := by
    have hEpochDecomp := Nat.div_add_mod ordinal.val epochData.val
    rw [Nat.mul_comm epochData.val (ordinal.val / epochData.val)] at hEpochDecomp
    have hOffsetLe :=
      Nat.div_mul_le_self ordinalInEpoch.val scheme.stripes_per_neighborhood.val
    rw [hBaseVal, hOffsetVal, hEpochVal, hDataVal]
    omega
  rcases checked_add_some_of_sum_lt base offset hBaseAddLt with
    ⟨base1, haddBase, hBase1Val⟩
  have hFinalAddLt : base1.val + stripe.val < 2 ^ 64 := by
    rw [hBase1Val, hBaseVal, hOffsetVal, hEpochVal, hDataVal, hStripeVal,
      hOrdinalInEpochVal]
    have hEpochDecomp := Nat.div_add_mod ordinal.val epochData.val
    rw [Nat.mul_comm epochData.val (ordinal.val / epochData.val)] at hEpochDecomp
    have hStripeDecomp := Nat.div_add_mod (ordinal.val % epochData.val)
      scheme.stripes_per_neighborhood.val
    rw [Nat.mul_comm scheme.stripes_per_neighborhood.val
      ((ordinal.val % epochData.val) / scheme.stripes_per_neighborhood.val)] at hStripeDecomp
    omega
  rcases checked_add_some_of_sum_lt base1 stripe hFinalAddLt with
    ⟨ordinalBack, haddFinal, hOrdinalBackVal⟩
  have hOrdinalBackEq : ordinalBack = ordinal := by
    apply UScalar.eq_imp
    rw [hOrdinalBackVal, hBase1Val, hBaseVal, hOffsetVal, hEpochVal, hDataVal,
      hStripeVal, hOrdinalInEpochVal]
    have hEpochDecomp := Nat.div_add_mod ordinal.val epochData.val
    rw [Nat.mul_comm epochData.val (ordinal.val / epochData.val)] at hEpochDecomp
    have hStripeDecomp := Nat.div_add_mod (ordinal.val % epochData.val)
      scheme.stripes_per_neighborhood.val
    rw [Nat.mul_comm scheme.stripes_per_neighborhood.val
      ((ordinal.val % epochData.val) / scheme.stripes_per_neighborhood.val)] at hStripeDecomp
    omega
  refine ⟨{
    neighborhood := epoch,
    stripe_index := stripe,
    position := StripePosition.Data dataIndex
  }, ?_, ?_⟩
  · unfold ordinal_to_stripe
    simp [hepoch, core.result.Result.Insts.CoreOpsTry.branch, hdivEpoch, hremEpoch,
      hremStripe, hdivData]
  · unfold stripe_data_to_ordinal
    simp [lift, hStripeNotGe, hDataNotGe, hepoch,
      core.result.Result.Insts.CoreOpsTry.branch, hmulBase, hmulOffset, haddBase,
      haddFinal, hOrdinalBackEq]

/-- M5a -- reverse mapping rejects a stripe index outside the scheme before
    consulting the address position. -/
theorem stripe_data_to_ordinal_rejects_bad_stripe
    (neighborhood stripe dataIndex : Std.U64) (scheme : ParityScheme)
    (hStripe : scheme.stripes_per_neighborhood.val ≤ stripe.val) :
    stripe_data_to_ordinal {
      neighborhood := neighborhood,
      stripe_index := stripe,
      position := StripePosition.Data dataIndex
    } scheme = ok (.Err MappingError.StripeIndexOutsideScheme) := by
  have hStripeGe : stripe >= scheme.stripes_per_neighborhood := by
    scalar_tac
  unfold stripe_data_to_ordinal
  simp [hStripeGe]

/-- M5b -- reverse mapping rejects an out-of-bounds data position after the
    stripe index has passed its bounds check. -/
theorem stripe_data_to_ordinal_rejects_bad_data_index
    (neighborhood stripe dataIndex : Std.U64) (scheme : ParityScheme)
    (hStripe : stripe.val < scheme.stripes_per_neighborhood.val)
    (hData : scheme.data_blocks_per_stripe.val ≤ dataIndex.val) :
    stripe_data_to_ordinal {
      neighborhood := neighborhood,
      stripe_index := stripe,
      position := StripePosition.Data dataIndex
    } scheme = ok (.Err MappingError.DataIndexOutsideScheme) := by
  have hStripeNotGe : ¬ stripe >= scheme.stripes_per_neighborhood := by
    scalar_tac
  have hDataGe : dataIndex >= scheme.data_blocks_per_stripe := by
    scalar_tac
  unfold stripe_data_to_ordinal
  simp [hStripeNotGe, hDataGe]

/-- M5c -- reverse mapping rejects parity-shard positions because they have no
    corresponding object-data ordinal. -/
theorem stripe_data_to_ordinal_rejects_parity_position
    (neighborhood stripe parityIndex : Std.U64) (scheme : ParityScheme)
    (hStripe : stripe.val < scheme.stripes_per_neighborhood.val) :
    stripe_data_to_ordinal {
      neighborhood := neighborhood,
      stripe_index := stripe,
      position := StripePosition.Parity parityIndex
    } scheme = ok (.Err MappingError.ParityShardHasNoDataOrdinal) := by
  have hStripeNotGe : ¬ stripe >= scheme.stripes_per_neighborhood := by
    scalar_tac
  unfold stripe_data_to_ordinal
  simp [hStripeNotGe]

end parity_mapping_verif
