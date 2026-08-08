/- Specification theorems for the parity-capacity extraction (SPEC.md C1-C5).

   Targets the Aeneas-generated definitions in `ParityCapacity.Funs`. The Lean
   checker accepting this file with no remaining local placeholders is the
   success criterion; the generated file is trusted only through Aeneas plus
   Lean, and the Rust `drift_guard` test ties the extraction back to production
   `crates/remanence-parity/src/capacity.rs`. -/
import ParityCapacity.Funs

open Aeneas Aeneas.Std Result

namespace parity_capacity_verif

/- Formal-proof scope:
   these theorems certify the extracted pure object-start capacity arithmetic:
   sidecar/bootstrap sizing, epoch completion and final-partial-sidecar
   detection, tape/spool reserve formulas, and the empty-tape/current-tape/spool
   gate ordering. They do not prove the whole writer, catalog, tape device, or
   production error payload text; those remain covered by the extraction drift
   guard and normal Rust tests. -/

lemma u64_checked_add_some_of_sum_lt (a b : Std.U64)
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

lemma u64_checked_mul_some_of_prod_lt (a b : Std.U64)
    (h : a.val * b.val < 2 ^ 64) :
    ∃ product, U64.checked_mul a b = some product ∧
      product.val = a.val * b.val := by
  have hspec := U64.checked_mul_bv_spec a b
  cases hmul : U64.checked_mul a b with
  | none =>
      simp [hmul, U64.max, U64.numBits] at hspec
      omega
  | some product =>
      simp [hmul, U64.max, U64.numBits] at hspec
      exact ⟨product, rfl, hspec.2.1⟩

lemma checked_add_ok (a b : Std.U64) (h : a.val + b.val < 2 ^ 64) :
    ∃ sum, checked_add a b = ok (.Ok sum) ∧ sum.val = a.val + b.val := by
  rcases u64_checked_add_some_of_sum_lt a b h with ⟨sum, hadd, hval⟩
  refine ⟨sum, ?_, hval⟩
  unfold checked_add
  simp [lift, hadd]

lemma checked_mul_ok (a b : Std.U64) (h : a.val * b.val < 2 ^ 64) :
    ∃ product, checked_mul a b = ok (.Ok product) ∧
      product.val = a.val * b.val := by
  rcases u64_checked_mul_some_of_prod_lt a b h with ⟨product, hmul, hval⟩
  refine ⟨product, ?_, hval⟩
  unfold checked_mul
  simp [lift, hmul]

lemma checked_add_result_value (a b sum : Std.U64)
    (h : checked_add a b = ok (.Ok sum)) :
    sum.val = a.val + b.val := by
  have hspec := U64.checked_add_bv_spec a b
  unfold checked_add at h
  cases hadd : U64.checked_add a b with
  | none => simp [lift, hadd] at h
  | some actual =>
      have hEq : actual = sum := by
        simp [lift, hadd] at h
        exact h
      simp [hadd] at hspec
      rw [← hEq]
      exact hspec.2.1

lemma checked_mul_result_value (a b product : Std.U64)
    (h : checked_mul a b = ok (.Ok product)) :
    product.val = a.val * b.val := by
  have hspec := U64.checked_mul_bv_spec a b
  unfold checked_mul at h
  cases hmul : U64.checked_mul a b with
  | none => simp [lift, hmul] at h
  | some actual =>
      have hEq : actual = product := by
        simp [lift, hmul] at h
        exact h
      simp [hmul] at hspec
      rw [← hEq]
      exact hspec.2.1

lemma checked_sub_underflow (a b : Std.U64) (h : a.val < b.val) :
    checked_sub a b = ok (.Err CapacityError.ArithmeticOverflow) := by
  have hspec := U64.checked_sub_bv_spec a b
  unfold checked_sub
  cases hsub : U64.checked_sub a b with
  | none => simp [lift]
  | some difference =>
      simp [hsub] at hspec
      omega

lemma checked_sub_ok (a b : Std.U64) (h : b.val ≤ a.val) :
    ∃ difference,
      checked_sub a b = ok (.Ok difference) ∧
      difference.val = a.val - b.val := by
  have hspec := U64.checked_sub_bv_spec a b
  unfold checked_sub
  cases hsub : U64.checked_sub a b with
  | none =>
      simp [hsub] at hspec
      omega
  | some difference =>
      simp [hsub] at hspec
      refine ⟨difference, ?_, hspec.2.1⟩
      simp [lift]

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

def sidecarIndexProduct (input : CapacityReserveInput) : Nat :=
  2 * input.sidecar_index_block_count.val

def sidecarMetadataBlocks (input : CapacityReserveInput) : Nat :=
  sidecarIndexProduct input + 1

def sidecarPlusParityBlocks (input : CapacityReserveInput) : Nat :=
  sidecarMetadataBlocks input + input.parity_shards_per_epoch.val

def sidecarTapeFileBlocksSpec (input : CapacityReserveInput) : Nat :=
  sidecarPlusParityBlocks input + input.sidecar_filemark_blocks.val

def bootstrapTapeFileBlocksSpec (input : CapacityReserveInput) : Nat :=
  1 + input.bootstrap_filemark_blocks.val

def projectedEpochFill (input : CapacityReserveInput) : Nat :=
  input.current_epoch_fill_blocks.val + input.projected_object_blocks.val

def epochsCompletedByObjectSpec (input : CapacityReserveInput) : Nat :=
  projectedEpochFill input / input.data_shards_per_epoch.val

def finalPartialSidecarNeededSpec (input : CapacityReserveInput) : Bool :=
  projectedEpochFill input % input.data_shards_per_epoch.val != 0

def finalPartialSidecarBlocksSpec (input : CapacityReserveInput) : Nat :=
  if projectedEpochFill input % input.data_shards_per_epoch.val = 0 then
    0
  else
    sidecarTapeFileBlocksSpec input

def pendingSidecarBlocksSpec (input : CapacityReserveInput) : Nat :=
  input.pending_completed_sidecars.val * sidecarTapeFileBlocksSpec input

def completedByObjectSidecarBlocksSpec (input : CapacityReserveInput) : Nat :=
  epochsCompletedByObjectSpec input * sidecarTapeFileBlocksSpec input

def remainingBootstrapBlocksSpec (input : CapacityReserveInput) : Nat :=
  input.remaining_bootstrap_count.val * bootstrapTapeFileBlocksSpec input

def reserveStep1Spec (input : CapacityReserveInput) : Nat :=
  input.object_filemark_blocks.val + pendingSidecarBlocksSpec input

def reserveStep2Spec (input : CapacityReserveInput) : Nat :=
  reserveStep1Spec input + completedByObjectSidecarBlocksSpec input

def reserveStep3Spec (input : CapacityReserveInput) : Nat :=
  reserveStep2Spec input + finalPartialSidecarBlocksSpec input

def reserveStep4Spec (input : CapacityReserveInput) : Nat :=
  reserveStep3Spec input + remainingBootstrapBlocksSpec input

def reserveAfterObjectBlocksSpec (input : CapacityReserveInput) : Nat :=
  reserveStep4Spec input + input.safety_margin_blocks.val

def requiredTapeBlocksSpec (input : CapacityReserveInput) : Nat :=
  input.projected_object_blocks.val + reserveAfterObjectBlocksSpec input

def sidecarTapeFileBytesSpec (input : CapacityReserveInput) : Nat :=
  sidecarTapeFileBlocksSpec input * input.block_size_bytes.val

def completedByObjectSpoolBytesSpec (input : CapacityReserveInput) : Nat :=
  epochsCompletedByObjectSpec input * sidecarTapeFileBytesSpec input

def requiredSpoolBytesSpec (input : CapacityReserveInput) : Nat :=
  input.pending_completed_epoch_parity_bytes.val +
    completedByObjectSpoolBytesSpec input

def TapeReserveNoOverflow (input : CapacityReserveInput) : Prop :=
  sidecarIndexProduct input < 2 ^ 64 ∧
  sidecarMetadataBlocks input < 2 ^ 64 ∧
  sidecarPlusParityBlocks input < 2 ^ 64 ∧
  sidecarTapeFileBlocksSpec input < 2 ^ 64 ∧
  bootstrapTapeFileBlocksSpec input < 2 ^ 64 ∧
  projectedEpochFill input < 2 ^ 64 ∧
  pendingSidecarBlocksSpec input < 2 ^ 64 ∧
  completedByObjectSidecarBlocksSpec input < 2 ^ 64 ∧
  remainingBootstrapBlocksSpec input < 2 ^ 64 ∧
  reserveStep1Spec input < 2 ^ 64 ∧
  reserveStep2Spec input < 2 ^ 64 ∧
  reserveStep3Spec input < 2 ^ 64 ∧
  reserveStep4Spec input < 2 ^ 64 ∧
  reserveAfterObjectBlocksSpec input < 2 ^ 64 ∧
  requiredTapeBlocksSpec input < 2 ^ 64

def SpoolReserveNoOverflow (input : CapacityReserveInput) : Prop :=
  sidecarTapeFileBytesSpec input < 2 ^ 64 ∧
  completedByObjectSpoolBytesSpec input < 2 ^ 64 ∧
  requiredSpoolBytesSpec input < 2 ^ 64

def sidecarTapeFileBytesFrom (input : CapacityReserveInput)
    (sidecarBlocks : Std.U64) : Nat :=
  sidecarBlocks.val * input.block_size_bytes.val

def completedByObjectSpoolBytesFrom (input : CapacityReserveInput)
    (epochs sidecarBlocks : Std.U64) : Nat :=
  epochs.val * sidecarTapeFileBytesFrom input sidecarBlocks

def requiredSpoolBytesFrom (input : CapacityReserveInput)
    (epochs sidecarBlocks : Std.U64) : Nat :=
  input.pending_completed_epoch_parity_bytes.val +
    completedByObjectSpoolBytesFrom input epochs sidecarBlocks

def SpoolReserveNoOverflowFrom (input : CapacityReserveInput)
    (epochs sidecarBlocks : Std.U64) : Prop :=
  sidecarTapeFileBytesFrom input sidecarBlocks < 2 ^ 64 ∧
  completedByObjectSpoolBytesFrom input epochs sidecarBlocks < 2 ^ 64 ∧
  requiredSpoolBytesFrom input epochs sidecarBlocks < 2 ^ 64

theorem compute_spool_reserve_success (input : CapacityReserveInput)
    (epochs sidecarBlocks : Std.U64)
    (hno : SpoolReserveNoOverflowFrom input epochs sidecarBlocks) :
    ∃ required,
      compute_spool_reserve input epochs sidecarBlocks = ok (.Ok required) ∧
      required.val = requiredSpoolBytesFrom input epochs sidecarBlocks := by
  rcases hno with ⟨hBytes, hCompleted, hRequired⟩
  rcases checked_mul_ok sidecarBlocks input.block_size_bytes hBytes with
    ⟨sidecarBytes, hSidecarBytes, hSidecarBytesVal⟩
  have hCompletedInput : epochs.val * sidecarBytes.val < 2 ^ 64 := by
    rw [hSidecarBytesVal]
    exact hCompleted
  rcases checked_mul_ok epochs sidecarBytes hCompletedInput with
    ⟨completedBytes, hCompletedBytes, hCompletedBytesVal⟩
  have hRequiredInput : input.pending_completed_epoch_parity_bytes.val +
      completedBytes.val < 2 ^ 64 := by
    rw [hCompletedBytesVal, hSidecarBytesVal]
    exact hRequired
  rcases checked_add_ok input.pending_completed_epoch_parity_bytes completedBytes
      hRequiredInput with ⟨required, hRequiredBytes, hRequiredVal⟩
  refine ⟨required, ?_, ?_⟩
  · unfold compute_spool_reserve
    simp [hSidecarBytes, core.result.Result.Insts.CoreOpsTry.branch,
      hCompletedBytes, hRequiredBytes]
  · rw [hRequiredVal, hCompletedBytesVal, hSidecarBytesVal]
    rfl

theorem compute_tape_reserve_success (input : CapacityReserveInput)
    (hBlock : 0 < input.block_size_bytes.val)
    (hData : 0 < input.data_shards_per_epoch.val)
    (hFill : input.current_epoch_fill_blocks.val < input.data_shards_per_epoch.val)
    (hno : TapeReserveNoOverflow input) :
    ∃ report,
      compute_tape_reserve input = ok (.Ok report) ∧
      report.epochs_completed_by_object.val = epochsCompletedByObjectSpec input ∧
      report.final_partial_sidecar_needed = finalPartialSidecarNeededSpec input ∧
      report.sidecar_tape_file_blocks.val = sidecarTapeFileBlocksSpec input ∧
      report.bootstrap_tape_file_blocks.val = bootstrapTapeFileBlocksSpec input ∧
      report.reserve_after_object_blocks.val = reserveAfterObjectBlocksSpec input ∧
      report.required_tape_blocks.val = requiredTapeBlocksSpec input := by
  rcases hno with
    ⟨hIdxProd, hMeta, hPlusParity, hSidecar, hBootstrap, hProjected,
      hPending, hCompleted, hRemainingBoot, hStep1, hStep2, hStep3,
      hStep4, hReserve, hRequiredTape⟩
  have hBlockNe : input.block_size_bytes ≠ 0#u64 := by
    intro hz
    have hzv : input.block_size_bytes.val = 0 := by scalar_tac
    omega
  have hDataNe : input.data_shards_per_epoch ≠ 0#u64 := by
    intro hz
    have hzv : input.data_shards_per_epoch.val = 0 := by scalar_tac
    omega
  have hFillNotGe : ¬ input.current_epoch_fill_blocks >= input.data_shards_per_epoch := by
    scalar_tac
  rcases checked_mul_ok 2#u64 input.sidecar_index_block_count
      (by simpa [sidecarIndexProduct] using hIdxProd) with
    ⟨idxProduct, hIdxProduct, hIdxProductVal⟩
  have hIdxProductSpecVal : idxProduct.val = sidecarIndexProduct input := by
    simpa [sidecarIndexProduct] using hIdxProductVal
  rcases checked_add_ok idxProduct 1#u64 (by rw [hIdxProductSpecVal]; exact hMeta) with
    ⟨sidecarMetadata, hSidecarMetadata, hSidecarMetadataVal⟩
  have hSidecarMetadataSpecVal :
      sidecarMetadata.val = sidecarMetadataBlocks input := by
    rw [hSidecarMetadataVal, hIdxProductSpecVal]
    rfl
  rcases checked_add_ok sidecarMetadata input.parity_shards_per_epoch
      (by rw [hSidecarMetadataSpecVal]; exact hPlusParity) with
    ⟨sidecarPlusParity, hSidecarPlusParity, hSidecarPlusParityVal⟩
  have hSidecarPlusParitySpecVal :
      sidecarPlusParity.val = sidecarPlusParityBlocks input := by
    rw [hSidecarPlusParityVal, hSidecarMetadataSpecVal]
    rfl
  rcases checked_add_ok sidecarPlusParity input.sidecar_filemark_blocks
      (by rw [hSidecarPlusParitySpecVal]; exact hSidecar) with
    ⟨sidecarBlocks, hSidecarBlocks, hSidecarBlocksVal⟩
  have hSidecarBlocksSpecVal :
      sidecarBlocks.val = sidecarTapeFileBlocksSpec input := by
    rw [hSidecarBlocksVal, hSidecarPlusParitySpecVal]
    rfl
  have hBootstrapCount : block_count_per_bootstrap = ok 1#u64 := by
    unfold block_count_per_bootstrap
    simp
  rcases checked_add_ok 1#u64 input.bootstrap_filemark_blocks hBootstrap with
    ⟨bootstrapBlocks, hBootstrapBlocks, hBootstrapBlocksVal⟩
  have hBootstrapBlocksSpecVal :
      bootstrapBlocks.val = bootstrapTapeFileBlocksSpec input := by
    simpa [bootstrapTapeFileBlocksSpec] using hBootstrapBlocksVal
  rcases checked_add_ok input.current_epoch_fill_blocks input.projected_object_blocks
      hProjected with ⟨projectedFill, hProjectedFill, hProjectedFillVal⟩
  have hProjectedFillSpecVal : projectedFill.val = projectedEpochFill input := by
    simpa [projectedEpochFill] using hProjectedFillVal
  have hDataNz : input.data_shards_per_epoch.val ≠ 0 := by omega
  rcases u64_div_ok_val projectedFill input.data_shards_per_epoch hDataNz with
    ⟨epochs, hEpochs, hEpochsVal⟩
  have hEpochsSpecVal : epochs.val = epochsCompletedByObjectSpec input := by
    rw [hEpochsVal, hProjectedFillSpecVal]
    rfl
  rcases u64_rem_ok_val projectedFill input.data_shards_per_epoch hDataNz with
    ⟨remainder, hRemainder, hRemainderVal⟩
  let finalBlocks : Std.U64 := if remainder = 0#u64 then 0#u64 else sidecarBlocks
  have hFinalBlocksVal : finalBlocks.val = finalPartialSidecarBlocksSpec input := by
    unfold finalBlocks
    by_cases hRemZero : remainder = 0#u64
    · have hNatRemZero :
          projectedEpochFill input % input.data_shards_per_epoch.val = 0 := by
        rw [← hProjectedFillSpecVal, ← hRemainderVal]
        scalar_tac
      simp [hRemZero, finalPartialSidecarBlocksSpec, hNatRemZero]
    · have hNatRemNotZero :
          projectedEpochFill input % input.data_shards_per_epoch.val ≠ 0 := by
        intro hzero
        apply hRemZero
        apply UScalar.eq_imp
        rw [hRemainderVal, hProjectedFillSpecVal]
        simpa using hzero
      simp [hRemZero, finalPartialSidecarBlocksSpec, hNatRemNotZero,
        hSidecarBlocksSpecVal]
  rcases checked_mul_ok input.pending_completed_sidecars sidecarBlocks
      (by rw [hSidecarBlocksSpecVal]; exact hPending) with
    ⟨pendingBlocks, hPendingBlocks, hPendingBlocksVal⟩
  have hPendingBlocksSpecVal : pendingBlocks.val = pendingSidecarBlocksSpec input := by
    rw [hPendingBlocksVal, hSidecarBlocksSpecVal]
    rfl
  rcases checked_mul_ok epochs sidecarBlocks
      (by rw [hEpochsSpecVal, hSidecarBlocksSpecVal]; exact hCompleted) with
    ⟨completedBlocks, hCompletedBlocks, hCompletedBlocksVal⟩
  have hCompletedBlocksSpecVal :
      completedBlocks.val = completedByObjectSidecarBlocksSpec input := by
    rw [hCompletedBlocksVal, hEpochsSpecVal, hSidecarBlocksSpecVal]
    rfl
  rcases checked_mul_ok input.remaining_bootstrap_count bootstrapBlocks
      (by rw [hBootstrapBlocksSpecVal]; exact hRemainingBoot) with
    ⟨remainingBootBlocks, hRemainingBootBlocks, hRemainingBootBlocksVal⟩
  have hRemainingBootBlocksSpecVal :
      remainingBootBlocks.val = remainingBootstrapBlocksSpec input := by
    rw [hRemainingBootBlocksVal, hBootstrapBlocksSpecVal]
    rfl
  rcases checked_add_ok input.object_filemark_blocks pendingBlocks
      (by rw [hPendingBlocksSpecVal]; exact hStep1) with
    ⟨reserve1, hReserve1, hReserve1Val⟩
  have hReserve1SpecVal : reserve1.val = reserveStep1Spec input := by
    rw [hReserve1Val, hPendingBlocksSpecVal]
    rfl
  rcases checked_add_ok reserve1 completedBlocks
      (by rw [hReserve1SpecVal, hCompletedBlocksSpecVal]; exact hStep2) with
    ⟨reserve2, hReserve2, hReserve2Val⟩
  have hReserve2SpecVal : reserve2.val = reserveStep2Spec input := by
    rw [hReserve2Val, hReserve1SpecVal, hCompletedBlocksSpecVal]
    rfl
  rcases checked_add_ok reserve2 finalBlocks
      (by rw [hReserve2SpecVal, hFinalBlocksVal]; exact hStep3) with
    ⟨reserve3, hReserve3, hReserve3Val⟩
  have hReserve3SpecVal : reserve3.val = reserveStep3Spec input := by
    rw [hReserve3Val, hReserve2SpecVal, hFinalBlocksVal]
    rfl
  rcases checked_add_ok reserve3 remainingBootBlocks
      (by rw [hReserve3SpecVal, hRemainingBootBlocksSpecVal]; exact hStep4) with
    ⟨reserve4, hReserve4, hReserve4Val⟩
  have hReserve4SpecVal : reserve4.val = reserveStep4Spec input := by
    rw [hReserve4Val, hReserve3SpecVal, hRemainingBootBlocksSpecVal]
    rfl
  rcases checked_add_ok reserve4 input.safety_margin_blocks
      (by rw [hReserve4SpecVal]; exact hReserve) with
    ⟨reserveAfter, hReserveAfter, hReserveAfterVal⟩
  have hReserveAfterSpecVal :
      reserveAfter.val = reserveAfterObjectBlocksSpec input := by
    rw [hReserveAfterVal, hReserve4SpecVal]
    rfl
  rcases checked_add_ok input.projected_object_blocks reserveAfter
      (by rw [hReserveAfterSpecVal]; exact hRequiredTape) with
    ⟨requiredTape, hRequiredTapeBlocks, hRequiredTapeVal⟩
  have hRequiredTapeSpecVal :
      requiredTape.val = requiredTapeBlocksSpec input := by
    rw [hRequiredTapeVal, hReserveAfterSpecVal]
    rfl
  refine ⟨{
    epochs_completed_by_object := epochs,
    final_partial_sidecar_needed := remainder != 0#u64,
    sidecar_tape_file_blocks := sidecarBlocks,
    bootstrap_tape_file_blocks := bootstrapBlocks,
    reserve_after_object_blocks := reserveAfter,
    required_tape_blocks := requiredTape
  }, ?_, hEpochsSpecVal, ?_, hSidecarBlocksSpecVal, hBootstrapBlocksSpecVal,
     hReserveAfterSpecVal, hRequiredTapeSpecVal⟩
  · unfold compute_tape_reserve
    by_cases hRemZero : remainder = 0#u64
    · have hReserve3' : checked_add reserve2 0#u64 = ok (.Ok reserve3) := by
        simpa [finalBlocks, hRemZero] using hReserve3
      simp [hBlockNe, hDataNe, hFillNotGe, hIdxProduct,
        core.result.Result.Insts.CoreOpsTry.branch, hSidecarMetadata,
        hSidecarPlusParity, hSidecarBlocks, hBootstrapCount, hBootstrapBlocks,
        hProjectedFill, hEpochs, hRemainder, hRemZero, hPendingBlocks,
        hCompletedBlocks, hRemainingBootBlocks, hReserve1, hReserve2,
        hReserve3', hReserve4, hReserveAfter, hRequiredTapeBlocks]
    · have hRemValNotZero : ¬ remainder.val = 0 := by
        intro hv
        apply hRemZero
        apply UScalar.eq_imp
        simpa using hv
      have hReserve3' : checked_add reserve2 sidecarBlocks = ok (.Ok reserve3) := by
        simpa [finalBlocks, hRemZero] using hReserve3
      simp [hBlockNe, hDataNe, hFillNotGe, hIdxProduct,
        core.result.Result.Insts.CoreOpsTry.branch, hSidecarMetadata,
        hSidecarPlusParity, hSidecarBlocks, hBootstrapCount, hBootstrapBlocks,
        hProjectedFill, hEpochs, hRemainder, hRemValNotZero, hPendingBlocks,
        hCompletedBlocks, hRemainingBootBlocks, hReserve1, hReserve2,
        hReserve3', hReserve4, hReserveAfter, hRequiredTapeBlocks]
  · by_cases hRemZero : remainder = 0#u64
    · have hNatRemZero :
          projectedEpochFill input % input.data_shards_per_epoch.val = 0 := by
        rw [← hProjectedFillSpecVal, ← hRemainderVal]
        scalar_tac
      have hLeft : (remainder != 0#u64) = false := by simp [hRemZero]
      have hRight : finalPartialSidecarNeededSpec input = false := by
        simp [finalPartialSidecarNeededSpec, hNatRemZero]
      rw [hLeft, hRight]
    · have hNatRemNotZero :
          projectedEpochFill input % input.data_shards_per_epoch.val ≠ 0 := by
        intro hzero
        apply hRemZero
        apply UScalar.eq_imp
        rw [hRemainderVal, hProjectedFillSpecVal]
        simpa using hzero
      have hLeft : (remainder != 0#u64) = true := by simp [hRemZero]
      have hRight : finalPartialSidecarNeededSpec input = true := by
        simp [finalPartialSidecarNeededSpec, hNatRemNotZero]
      rw [hLeft, hRight]

/-- Invariant guard: zero block size is rejected before any reserve arithmetic. -/
theorem compute_tape_reserve_rejects_zero_block_size
    (input : CapacityReserveInput) (h : input.block_size_bytes = 0#u64) :
    compute_tape_reserve input = ok (.Err CapacityError.BlockSizeZero) := by
  unfold compute_tape_reserve
  simp [h]

/-- Invariant guard: zero epoch size is rejected after the block-size check. -/
theorem compute_tape_reserve_rejects_zero_data_shards
    (input : CapacityReserveInput)
    (hBlock : input.block_size_bytes ≠ 0#u64)
    (hData : input.data_shards_per_epoch = 0#u64) :
    compute_tape_reserve input = ok (.Err CapacityError.DataShardsPerEpochZero) := by
  unfold compute_tape_reserve
  simp [hBlock, hData]

/-- Invariant guard: an already-full open epoch is rejected before arithmetic. -/
theorem compute_tape_reserve_rejects_epoch_fill_outside_open_epoch
    (input : CapacityReserveInput)
    (hBlock : input.block_size_bytes ≠ 0#u64)
    (hData : input.data_shards_per_epoch ≠ 0#u64)
    (hFill : input.current_epoch_fill_blocks.val ≥ input.data_shards_per_epoch.val) :
    compute_tape_reserve input =
      ok (.Err CapacityError.CurrentEpochFillOutsideOpenEpoch) := by
  have hGe : input.current_epoch_fill_blocks >= input.data_shards_per_epoch := by
    scalar_tac
  unfold compute_tape_reserve
  simp [hBlock, hData, hGe]

/-- C1-C5 success path: when all reserve arithmetic fits and all capacity gates
    have enough space, `evaluate` returns a report matching the spec formulas. -/
theorem evaluate_success_spec (input : CapacityReserveInput)
    (hBlock : 0 < input.block_size_bytes.val)
    (hData : 0 < input.data_shards_per_epoch.val)
    (hFill : input.current_epoch_fill_blocks.val < input.data_shards_per_epoch.val)
    (hTapeNo : TapeReserveNoOverflow input)
    (hSpoolNo : SpoolReserveNoOverflow input)
    (hEmptyFits : requiredTapeBlocksSpec input ≤ input.empty_tape_usable_blocks.val)
    (hCurrentFits : requiredTapeBlocksSpec input ≤ input.remaining_tape_blocks.val)
    (hSpoolFits : requiredSpoolBytesSpec input ≤ input.remaining_spool_bytes.val) :
    ∃ report,
      evaluate input = ok (.Ok report) ∧
      report.epochs_completed_by_object.val = epochsCompletedByObjectSpec input ∧
      report.final_partial_sidecar_needed = finalPartialSidecarNeededSpec input ∧
      report.sidecar_tape_file_blocks.val = sidecarTapeFileBlocksSpec input ∧
      report.bootstrap_tape_file_blocks.val = bootstrapTapeFileBlocksSpec input ∧
      report.reserve_after_object_blocks.val = reserveAfterObjectBlocksSpec input ∧
      report.required_tape_blocks.val = requiredTapeBlocksSpec input ∧
      report.required_spool_bytes.val = requiredSpoolBytesSpec input := by
  rcases compute_tape_reserve_success input hBlock hData hFill hTapeNo with
    ⟨tape, hTapeEval, hEpochs, hFinal, hSidecar, hBootstrap, hReserve,
      hRequiredTape⟩
  have hEmptyNotLtNat :
      ¬ input.empty_tape_usable_blocks.val < tape.required_tape_blocks.val := by
    omega
  have hCurrentNotLtNat :
      ¬ input.remaining_tape_blocks.val < tape.required_tape_blocks.val := by
    omega
  have hSpoolFrom : SpoolReserveNoOverflowFrom input
      tape.epochs_completed_by_object tape.sidecar_tape_file_blocks := by
    rcases hSpoolNo with ⟨hBytes, hCompleted, hRequired⟩
    refine ⟨?_, ?_, ?_⟩
    · rw [sidecarTapeFileBytesFrom, hSidecar]
      exact hBytes
    · rw [completedByObjectSpoolBytesFrom, sidecarTapeFileBytesFrom,
        hEpochs, hSidecar]
      exact hCompleted
    · rw [requiredSpoolBytesFrom, completedByObjectSpoolBytesFrom,
        sidecarTapeFileBytesFrom, hEpochs, hSidecar]
      exact hRequired
  rcases compute_spool_reserve_success input tape.epochs_completed_by_object
      tape.sidecar_tape_file_blocks hSpoolFrom with
    ⟨requiredSpool, hSpoolEval, hRequiredSpool⟩
  have hRequiredSpoolSpec : requiredSpool.val = requiredSpoolBytesSpec input := by
    rw [hRequiredSpool, requiredSpoolBytesFrom, completedByObjectSpoolBytesFrom,
      sidecarTapeFileBytesFrom, hEpochs, hSidecar]
    rfl
  have hSpoolNotLtNat :
      ¬ input.remaining_spool_bytes.val < requiredSpool.val := by
    omega
  refine ⟨{
    epochs_completed_by_object := tape.epochs_completed_by_object,
    final_partial_sidecar_needed := tape.final_partial_sidecar_needed,
    sidecar_tape_file_blocks := tape.sidecar_tape_file_blocks,
    bootstrap_tape_file_blocks := tape.bootstrap_tape_file_blocks,
    reserve_after_object_blocks := tape.reserve_after_object_blocks,
    required_tape_blocks := tape.required_tape_blocks,
    required_spool_bytes := requiredSpool
  }, ?_, hEpochs, hFinal, hSidecar, hBootstrap, hReserve, hRequiredTape,
     hRequiredSpoolSpec⟩
  unfold evaluate
  simp [hTapeEval, core.result.Result.Insts.CoreOpsTry.branch, hEmptyNotLtNat,
    hCurrentNotLtNat, hSpoolEval, hSpoolNotLtNat]

/-- C5a -- the empty-tape object-size gate fires before the current-tape gate. -/
theorem evaluate_object_too_large_gate (input : CapacityReserveInput)
    (hBlock : 0 < input.block_size_bytes.val)
    (hData : 0 < input.data_shards_per_epoch.val)
    (hFill : input.current_epoch_fill_blocks.val < input.data_shards_per_epoch.val)
    (hTapeNo : TapeReserveNoOverflow input)
    (hEmptyShort : input.empty_tape_usable_blocks.val < requiredTapeBlocksSpec input) :
    evaluate input = ok (.Err CapacityError.ObjectTooLargeForEmptyTape) := by
  rcases compute_tape_reserve_success input hBlock hData hFill hTapeNo with
    ⟨tape, hTapeEval, _hEpochs, _hFinal, _hSidecar, _hBootstrap, _hReserve,
      hRequiredTape⟩
  have hEmptyLtNat :
      input.empty_tape_usable_blocks.val < tape.required_tape_blocks.val := by
    omega
  unfold evaluate
  simp [hTapeEval, core.result.Result.Insts.CoreOpsTry.branch, hEmptyLtNat]

/-- C5b -- current-tape capacity is checked after the empty-tape feasibility
    gate and before any local spool check. -/
theorem evaluate_tape_capacity_gate (input : CapacityReserveInput)
    (hBlock : 0 < input.block_size_bytes.val)
    (hData : 0 < input.data_shards_per_epoch.val)
    (hFill : input.current_epoch_fill_blocks.val < input.data_shards_per_epoch.val)
    (hTapeNo : TapeReserveNoOverflow input)
    (hEmptyFits : requiredTapeBlocksSpec input ≤ input.empty_tape_usable_blocks.val)
    (hCurrentShort : input.remaining_tape_blocks.val < requiredTapeBlocksSpec input) :
    evaluate input = ok (.Err CapacityError.CapacityReserveExceededTape) := by
  rcases compute_tape_reserve_success input hBlock hData hFill hTapeNo with
    ⟨tape, hTapeEval, _hEpochs, _hFinal, _hSidecar, _hBootstrap, _hReserve,
      hRequiredTape⟩
  have hEmptyNotLtNat :
      ¬ input.empty_tape_usable_blocks.val < tape.required_tape_blocks.val := by
    omega
  have hCurrentLtNat :
      input.remaining_tape_blocks.val < tape.required_tape_blocks.val := by
    omega
  unfold evaluate
  simp [hTapeEval, core.result.Result.Insts.CoreOpsTry.branch, hEmptyNotLtNat,
    hCurrentLtNat]

/-- C5c -- after both tape gates pass, local spool capacity is the binding gate
    exactly when the remaining spool is below the required spool formula. -/
theorem evaluate_spool_capacity_gate (input : CapacityReserveInput)
    (hBlock : 0 < input.block_size_bytes.val)
    (hData : 0 < input.data_shards_per_epoch.val)
    (hFill : input.current_epoch_fill_blocks.val < input.data_shards_per_epoch.val)
    (hTapeNo : TapeReserveNoOverflow input)
    (hSpoolNo : SpoolReserveNoOverflow input)
    (hEmptyFits : requiredTapeBlocksSpec input ≤ input.empty_tape_usable_blocks.val)
    (hCurrentFits : requiredTapeBlocksSpec input ≤ input.remaining_tape_blocks.val)
    (hSpoolShort : input.remaining_spool_bytes.val < requiredSpoolBytesSpec input) :
    evaluate input = ok (.Err CapacityError.CapacityReserveExceededSpool) := by
  rcases compute_tape_reserve_success input hBlock hData hFill hTapeNo with
    ⟨tape, hTapeEval, hEpochs, _hFinal, hSidecar, _hBootstrap, _hReserve,
      hRequiredTape⟩
  have hEmptyNotLtNat :
      ¬ input.empty_tape_usable_blocks.val < tape.required_tape_blocks.val := by
    omega
  have hCurrentNotLtNat :
      ¬ input.remaining_tape_blocks.val < tape.required_tape_blocks.val := by
    omega
  have hSpoolFrom : SpoolReserveNoOverflowFrom input
      tape.epochs_completed_by_object tape.sidecar_tape_file_blocks := by
    rcases hSpoolNo with ⟨hBytes, hCompleted, hRequired⟩
    refine ⟨?_, ?_, ?_⟩
    · rw [sidecarTapeFileBytesFrom, hSidecar]
      exact hBytes
    · rw [completedByObjectSpoolBytesFrom, sidecarTapeFileBytesFrom,
        hEpochs, hSidecar]
      exact hCompleted
    · rw [requiredSpoolBytesFrom, completedByObjectSpoolBytesFrom,
        sidecarTapeFileBytesFrom, hEpochs, hSidecar]
      exact hRequired
  rcases compute_spool_reserve_success input tape.epochs_completed_by_object
      tape.sidecar_tape_file_blocks hSpoolFrom with
    ⟨requiredSpool, hSpoolEval, hRequiredSpool⟩
  have hRequiredSpoolSpec : requiredSpool.val = requiredSpoolBytesSpec input := by
    rw [hRequiredSpool, requiredSpoolBytesFrom, completedByObjectSpoolBytesFrom,
      sidecarTapeFileBytesFrom, hEpochs, hSidecar]
    rfl
  have hSpoolLtNat : input.remaining_spool_bytes.val < requiredSpool.val := by
    omega
  unfold evaluate
  simp [hTapeEval, core.result.Result.Insts.CoreOpsTry.branch, hEmptyNotLtNat,
    hCurrentNotLtNat, hSpoolEval, hSpoolLtNat]

/- Snapshot-aware C6-C10 candidate. -/

/-- C6: the extracted scalar packer computes the shipped 256 KiB profile
    exactly: three index blocks, with block zero filled through its usable
    payload after the 184-byte header and eight-byte trailing CRC. -/
theorem shipped_sidecar_index_capacity_layout :
    checked_sidecar_index_capacity_layout 262144#u64 2048#u64 65536#u64 =
      ok (.Ok {
        block_count := 3#u64,
        inline_entry_bytes := 261952#u64
      }) := by
  have hComputed :
      (match checked_sidecar_index_capacity_layout
          262144#u64 2048#u64 65536#u64 with
      | ok (.Ok layout) =>
          decide (layout.block_count.val = 3) &&
          decide (layout.inline_entry_bytes.val = 261952)
      | _ => false) = true := by
    native_decide
  generalize hResult : checked_sidecar_index_capacity_layout
      262144#u64 2048#u64 65536#u64 = result at hComputed ⊢
  cases result with
  | ok inner =>
      cases inner with
      | Ok layout =>
          simp only [Bool.and_eq_true, decide_eq_true_eq] at hComputed
          rcases hComputed with ⟨hBlocks, hInline⟩
          have hBlockEq : layout.block_count = 3#u64 := by
            apply UScalar.eq_imp
            simpa using hBlocks
          have hInlineEq : layout.inline_entry_bytes = 261952#u64 := by
            apply UScalar.eq_imp
            simpa using hInline
          cases layout
          simp_all
      | Err error => simp at hComputed
  | fail error => simp at hComputed
  | div => simp at hComputed

/-- C7: the complete ParityMap payload bound is exactly `325 + 116*N`. -/
theorem parity_map_payload_bound_success (entryCount : Std.U64)
    (hRows : entryCount.val * 116 < 2 ^ 64)
    (hTotal : 325 + entryCount.val * 116 < 2 ^ 64) :
    ∃ bound,
      parity_map_payload_len_upper_bound entryCount = ok (.Ok bound) ∧
      bound.val = 325 + entryCount.val * 116 := by
  rcases checked_mul_ok entryCount 116#u64 hRows with
    ⟨rows, hRowsEval, hRowsVal⟩
  have hTotal' : 325#u64.val + rows.val < 2 ^ 64 := by
    rw [hRowsVal]
    exact hTotal
  rcases checked_add_ok 325#u64 rows hTotal' with
    ⟨bound, hBoundEval, hBoundVal⟩
  refine ⟨bound, ?_, ?_⟩
  · simp [parity_map_payload_len_upper_bound,
      parity_map_directory_entry_bound_bytes, parity_map_fixed_bound_bytes,
      hRowsEval, hBoundEval, core.result.Result.Insts.CoreOpsTry.branch]
  · rw [hBoundVal, hRowsVal]
    norm_num

/-- C7: the inline-directory bound is exactly `43 + 116*N`. -/
theorem parity_map_directory_bound_success (entryCount : Std.U64)
    (hRows : entryCount.val * 116 < 2 ^ 64)
    (hTotal : 43 + entryCount.val * 116 < 2 ^ 64) :
    ∃ bound,
      parity_map_directory_len_upper_bound entryCount = ok (.Ok bound) ∧
      bound.val = 43 + entryCount.val * 116 := by
  rcases checked_mul_ok entryCount 116#u64 hRows with
    ⟨rows, hRowsEval, hRowsVal⟩
  have hTotal' : 43#u64.val + rows.val < 2 ^ 64 := by
    rw [hRowsVal]
    exact hTotal
  rcases checked_add_ok 43#u64 rows hTotal' with
    ⟨bound, hBoundEval, hBoundVal⟩
  refine ⟨bound, ?_, ?_⟩
  · simp [parity_map_directory_len_upper_bound,
      parity_map_directory_entry_bound_bytes,
      parity_map_directory_fixed_bound_bytes, hRowsEval, hBoundEval,
      core.result.Result.Insts.CoreOpsTry.branch]
  · rw [hBoundVal, hRowsVal]
    norm_num

/-- C8: an Object-row count above the structural count is rejected before
    payload arithmetic. -/
theorem snapshot_payload_rejects_row_count_above_structure
    (structural objectRows : Std.U64)
    (h : structural.val < objectRows.val) :
    snapshot_payload_bytes structural objectRows =
      ok (.Err CapacityError.ObjectRowsExceedStructuralEntries) := by
  simp [snapshot_payload_bytes, h]

/-- C8: accepted fixed-slot payload length is exactly `64*T + 256*O`. -/
theorem snapshot_payload_success (structural objectRows : Std.U64)
    (hRows : objectRows.val ≤ structural.val)
    (hStructural : structural.val * 64 < 2 ^ 64)
    (hObjects : objectRows.val * 256 < 2 ^ 64)
    (hTotal : structural.val * 64 + objectRows.val * 256 < 2 ^ 64) :
    ∃ payload,
      snapshot_payload_bytes structural objectRows = ok (.Ok payload) ∧
      payload.val = structural.val * 64 + objectRows.val * 256 := by
  rcases checked_mul_ok structural 64#u64 hStructural with
    ⟨structuralBytes, hStructuralEval, hStructuralVal⟩
  rcases checked_mul_ok objectRows 256#u64 hObjects with
    ⟨objectBytes, hObjectEval, hObjectVal⟩
  have hTotal' : structuralBytes.val + objectBytes.val < 2 ^ 64 := by
    rw [hStructuralVal, hObjectVal]
    exact hTotal
  rcases checked_add_ok structuralBytes objectBytes hTotal' with
    ⟨payload, hPayloadEval, hPayloadVal⟩
  have hnRows : ¬ structural.val < objectRows.val := by omega
  refine ⟨payload, ?_, ?_⟩
  · simp [snapshot_payload_bytes, snapshot_structural_slot_bytes,
      snapshot_object_row_slot_bytes, hnRows, hStructuralEval, hObjectEval,
      hPayloadEval, core.result.Result.Insts.CoreOpsTry.branch]
  · rw [hPayloadVal, hStructuralVal, hObjectVal]
    norm_num

/-- C8: replicated controls use one checked rounded copy count, two complete
    copies, and one footer locator. -/
theorem replicated_control_total_blocks_of_intermediates
    (blockSize header payload copyBytes quotient remainder rounding copyBlocks
      doubled total : Std.U64)
    (hBlock : blockSize ≠ 0#u64)
    (hHeader : header.val ≤ blockSize.val)
    (hCopyBytes : checked_add header payload = ok (.Ok copyBytes))
    (hQuotient : copyBytes / blockSize = ok quotient)
    (hRemainder : copyBytes % blockSize = ok remainder)
    (hRounding : (if remainder = 0#u64 then ok 0#u64 else ok 1#u64) =
      ok rounding)
    (hCopyBlocks : checked_add quotient rounding = ok (.Ok copyBlocks))
    (hDoubled : checked_mul 2#u64 copyBlocks = ok (.Ok doubled))
    (hTotal : checked_add doubled 1#u64 = ok (.Ok total)) :
    replicated_control_total_blocks blockSize header payload = ok (.Ok total) := by
  have hnHeader : ¬ blockSize.val < header.val := by omega
  simp [replicated_control_total_blocks, hBlock, hnHeader, hCopyBytes,
    hQuotient, hRemainder, hRounding, hCopyBlocks, hDoubled, hTotal,
    core.result.Result.Insts.CoreOpsTry.branch]

/-- C8: successful checked intermediates have the advertised rounded numeric
    meaning, not merely the same control-flow shape. -/
theorem replicated_control_total_blocks_numeric
    (blockSize header payload copyBytes quotient remainder rounding copyBlocks
      doubled total : Std.U64)
    (hBlock : blockSize ≠ 0#u64)
    (hBlockVal : blockSize.val ≠ 0)
    (hHeader : header.val ≤ blockSize.val)
    (hCopyBound : header.val + payload.val < 2 ^ 64)
    (hCopyBytes : checked_add header payload = ok (.Ok copyBytes))
    (hQuotient : copyBytes / blockSize = ok quotient)
    (hRemainder : copyBytes % blockSize = ok remainder)
    (hRounding : (if remainder = 0#u64 then ok 0#u64 else ok 1#u64) =
      ok rounding)
    (hCopyBlocksBound : quotient.val + rounding.val < 2 ^ 64)
    (hCopyBlocks : checked_add quotient rounding = ok (.Ok copyBlocks))
    (hDoubledBound : 2 * copyBlocks.val < 2 ^ 64)
    (hDoubled : checked_mul 2#u64 copyBlocks = ok (.Ok doubled))
    (hTotalBound : doubled.val + 1 < 2 ^ 64)
    (hTotal : checked_add doubled 1#u64 = ok (.Ok total)) :
    replicated_control_total_blocks blockSize header payload = ok (.Ok total) ∧
      total.val =
        2 * ((header.val + payload.val) / blockSize.val +
          if (header.val + payload.val) % blockSize.val = 0 then 0 else 1) + 1 := by
  have hEval := replicated_control_total_blocks_of_intermediates
    blockSize header payload copyBytes quotient remainder rounding copyBlocks
    doubled total hBlock hHeader hCopyBytes hQuotient hRemainder hRounding
    hCopyBlocks hDoubled hTotal
  rcases checked_add_ok header payload hCopyBound with
    ⟨copyBytes', hCopyBytes', hCopyVal'⟩
  have hCopyEq : copyBytes' = copyBytes := by
    rw [hCopyBytes] at hCopyBytes'
    symm
    simpa using hCopyBytes'
  have hCopyVal : copyBytes.val = header.val + payload.val := by
    rw [← hCopyEq]
    exact hCopyVal'
  have hQuotientVal : quotient.val = copyBytes.val / blockSize.val := by
    have hspec := U64.div_spec copyBytes (y := blockSize) hBlockVal
    simpa [hQuotient] using hspec
  have hRemainderVal : remainder.val = copyBytes.val % blockSize.val := by
    have hspec := U64.rem_spec copyBytes (y := blockSize) hBlockVal
    simpa [hRemainder] using hspec
  rcases checked_add_ok quotient rounding hCopyBlocksBound with
    ⟨copyBlocks', hCopyBlocks', hCopyBlocksVal'⟩
  have hCopyBlocksEq : copyBlocks' = copyBlocks := by
    rw [hCopyBlocks] at hCopyBlocks'
    symm
    simpa using hCopyBlocks'
  have hCopyBlocksVal : copyBlocks.val = quotient.val + rounding.val := by
    rw [← hCopyBlocksEq]
    exact hCopyBlocksVal'
  rcases checked_mul_ok 2#u64 copyBlocks hDoubledBound with
    ⟨doubled', hDoubled', hDoubledVal'⟩
  have hDoubledEq : doubled' = doubled := by
    rw [hDoubled] at hDoubled'
    symm
    simpa using hDoubled'
  have hDoubledVal : doubled.val = 2 * copyBlocks.val := by
    rw [← hDoubledEq]
    simpa using hDoubledVal'
  rcases checked_add_ok doubled 1#u64 hTotalBound with
    ⟨total', hTotal', hTotalVal'⟩
  have hTotalEq : total' = total := by
    rw [hTotal] at hTotal'
    symm
    simpa using hTotal'
  have hTotalVal : total.val = doubled.val + 1 := by
    rw [← hTotalEq]
    simpa using hTotalVal'
  refine ⟨hEval, ?_⟩
  by_cases hRem : remainder = 0#u64
  · have hRoundingEq : rounding = 0#u64 := by
      simpa [hRem] using hRounding.symm
    have hNatRem : (header.val + payload.val) % blockSize.val = 0 := by
      rw [← hCopyVal, ← hRemainderVal]
      scalar_tac
    simp [hTotalVal, hDoubledVal, hCopyBlocksVal, hQuotientVal,
      hCopyVal, hRoundingEq, hNatRem]
  · have hRoundingEq : rounding = 1#u64 := by
      simpa [hRem] using hRounding.symm
    have hNatRem : (header.val + payload.val) % blockSize.val ≠ 0 := by
      intro hz
      apply hRem
      apply UScalar.eq_imp
      rw [hRemainderVal, hCopyVal]
      simpa using hz
    simp [hTotalVal, hDoubledVal, hCopyBlocksVal, hQuotientVal,
      hCopyVal, hRoundingEq, hNatRem]

/-- C8 overflow propagation: header-plus-payload overflow fails closed. -/
theorem replicated_control_rejects_copy_byte_overflow
    (blockSize header payload : Std.U64)
    (hBlock : blockSize ≠ 0#u64)
    (hHeader : header.val ≤ blockSize.val)
    (hOverflow : checked_add header payload =
      ok (.Err CapacityError.ArithmeticOverflow)) :
    replicated_control_total_blocks blockSize header payload =
      ok (.Err CapacityError.ArithmeticOverflow) := by
  have hnHeader : ¬ blockSize.val < header.val := by omega
  simp [replicated_control_total_blocks, hBlock, hnHeader, hOverflow,
    core.result.Result.Insts.CoreOpsTry.branch,
    core.result.Result.Insts.CoreOpsTryTraitFromResidualResultInfallible.from_residual]

/-- C8 overflow propagation: doubling the rounded copy count fails closed. -/
theorem replicated_control_rejects_doubled_overflow
    (blockSize header payload copyBytes quotient remainder rounding copyBlocks : Std.U64)
    (hBlock : blockSize ≠ 0#u64)
    (hHeader : header.val ≤ blockSize.val)
    (hCopyBytes : checked_add header payload = ok (.Ok copyBytes))
    (hQuotient : copyBytes / blockSize = ok quotient)
    (hRemainder : copyBytes % blockSize = ok remainder)
    (hRounding : (if remainder = 0#u64 then ok 0#u64 else ok 1#u64) =
      ok rounding)
    (hCopyBlocks : checked_add quotient rounding = ok (.Ok copyBlocks))
    (hOverflow : checked_mul 2#u64 copyBlocks =
      ok (.Err CapacityError.ArithmeticOverflow)) :
    replicated_control_total_blocks blockSize header payload =
      ok (.Err CapacityError.ArithmeticOverflow) := by
  have hnHeader : ¬ blockSize.val < header.val := by omega
  simp [replicated_control_total_blocks, hBlock, hnHeader, hCopyBytes,
    hQuotient, hRemainder, hRounding, hCopyBlocks, hOverflow,
    core.result.Result.Insts.CoreOpsTry.branch,
    core.result.Result.Insts.CoreOpsTryTraitFromResidualResultInfallible.from_residual]

/-- C8 overflow propagation: adding the footer locator fails closed. -/
theorem replicated_control_rejects_total_overflow
    (blockSize header payload copyBytes quotient remainder rounding copyBlocks
      doubled : Std.U64)
    (hBlock : blockSize ≠ 0#u64)
    (hHeader : header.val ≤ blockSize.val)
    (hCopyBytes : checked_add header payload = ok (.Ok copyBytes))
    (hQuotient : copyBytes / blockSize = ok quotient)
    (hRemainder : copyBytes % blockSize = ok remainder)
    (hRounding : (if remainder = 0#u64 then ok 0#u64 else ok 1#u64) =
      ok rounding)
    (hCopyBlocks : checked_add quotient rounding = ok (.Ok copyBlocks))
    (hDoubled : checked_mul 2#u64 copyBlocks = ok (.Ok doubled))
    (hOverflow : checked_add doubled 1#u64 =
      ok (.Err CapacityError.ArithmeticOverflow)) :
    replicated_control_total_blocks blockSize header payload =
      ok (.Err CapacityError.ArithmeticOverflow) := by
  have hnHeader : ¬ blockSize.val < header.val := by omega
  simp [replicated_control_total_blocks, hBlock, hnHeader, hCopyBytes,
    hQuotient, hRemainder, hRounding, hCopyBlocks, hDoubled, hOverflow,
    core.result.Result.Insts.CoreOpsTry.branch]

/-- C6: the sidecar report contains both replicated index copies, the parity
    body, footer, and separate filemark when all checked intermediates succeed. -/
theorem compute_snapshot_sidecar_terms_of_intermediates
    (input : SnapshotCloseInput) (layout : SidecarIndexCapacityLayout)
    (replicated plusParity beforeFilemark tapeFile : Std.U64)
    (hLayout : checked_sidecar_index_capacity_layout input.block_size_bytes
      input.parity_shards_per_epoch input.data_shards_per_epoch = ok (.Ok layout))
    (hReplicated : checked_mul 2#u64 layout.block_count = ok (.Ok replicated))
    (hPlusParity : checked_add replicated input.parity_shards_per_epoch =
      ok (.Ok plusParity))
    (hFooter : checked_add plusParity 1#u64 = ok (.Ok beforeFilemark))
    (hFilemark : checked_add beforeFilemark input.sidecar_filemark_blocks =
      ok (.Ok tapeFile)) :
    compute_snapshot_sidecar_terms input = ok (.Ok {
      index_block_count := layout.block_count,
      blocks_before_filemark := beforeFilemark,
      tape_file_blocks := tapeFile
    }) := by
  simp [compute_snapshot_sidecar_terms, hLayout, hReplicated, hPlusParity,
    hFooter, hFilemark, core.result.Result.Insts.CoreOpsTry.branch]

/-- C9: the extracted projection computes every post-Object and post-closeout
    count from checked arithmetic. The final ParityMap decision is conservative:
    every nonempty sidecar directory reserves one external map. -/
theorem compute_snapshot_projection_terms_spec
    (input : SnapshotCloseInput) (sidecar : SnapshotSidecarTerms)
    (maximum projectedFill epochs remainder sidecarsEmitted sidecarBlocks
      objectTape objectCommit objectRows sidecarAfterCommit sidecarAfter
      directoryBound payloadBound structural1 structural2 structural3
      structuralFinal : Std.U64)
    (hData : input.data_shards_per_epoch.val ≠ 0)
    (hProjected : checked_add input.current_epoch_fill_blocks
      input.projected_object_blocks = ok (.Ok projectedFill))
    (hEpochs : projectedFill / input.data_shards_per_epoch = ok epochs)
    (hRemainder : projectedFill % input.data_shards_per_epoch = ok remainder)
    (hSidecars : checked_add input.pending_completed_sidecars epochs =
      ok (.Ok sidecarsEmitted))
    (hSidecarBlocks : checked_mul sidecarsEmitted sidecar.tape_file_blocks =
      ok (.Ok sidecarBlocks))
    (hObjectTape : checked_add input.projected_object_blocks
      input.object_filemark_blocks = ok (.Ok objectTape))
    (hObjectCommit : checked_add objectTape sidecarBlocks = ok (.Ok objectCommit))
    (hObjectRows : checked_add input.object_rows_before_object 1#u64 =
      ok (.Ok objectRows))
    (hSidecarAfterCommit : checked_add input.sidecar_entries_before_object
      sidecarsEmitted = ok (.Ok sidecarAfterCommit))
    (hSidecarAfter : checked_add sidecarAfterCommit
      (if remainder = 0#u64 then 0#u64 else 1#u64) = ok (.Ok sidecarAfter))
    (hExistingWithinMaximum : input.sidecar_entries_before_object.val ≤ maximum.val)
    (hDirectory : parity_map_directory_len_upper_bound sidecarAfter =
      ok (.Ok directoryBound))
    (hPayload : parity_map_payload_len_upper_bound sidecarAfter =
      ok (.Ok payloadBound))
    (hStructural1 : checked_add input.structural_entries_before_object 1#u64 =
      ok (.Ok structural1))
    (hStructural2 : checked_add structural1 sidecarsEmitted =
      ok (.Ok structural2))
    (hStructural3 : checked_add structural2
      (if remainder = 0#u64 then 0#u64 else 1#u64) = ok (.Ok structural3))
    (hStructuralFinal : checked_add structural3
      (if sidecarAfter = 0#u64 then 0#u64 else 1#u64) =
      ok (.Ok structuralFinal)) :
    compute_snapshot_projection_terms input sidecar maximum = ok (.Ok {
      epochs_completed_by_object := epochs,
      final_partial_sidecar_needed := remainder != 0#u64,
      sidecars_emitted_by_commit := sidecarsEmitted,
      sidecar_blocks_emitted_by_commit := sidecarBlocks,
      object_tape_file_blocks := objectTape,
      object_commit_charge_blocks := objectCommit,
      object_rows_after := objectRows,
      sidecar_entries_after_closeout := sidecarAfter,
      maximum_sidecar_entries_for_capacity := maximum,
      structural_entries_after_closeout := structuralFinal,
      final_parity_map_needed := sidecarAfter != 0#u64,
      final_parity_map_directory_bound_bytes := directoryBound,
      final_parity_map_payload_bound_bytes := payloadBound
    }) ∧
      epochs.val = projectedFill.val / input.data_shards_per_epoch.val ∧
      remainder.val = projectedFill.val % input.data_shards_per_epoch.val ∧
      sidecarsEmitted.val = input.pending_completed_sidecars.val + epochs.val ∧
      sidecarBlocks.val = sidecarsEmitted.val * sidecar.tape_file_blocks.val ∧
      objectTape.val = input.projected_object_blocks.val +
        input.object_filemark_blocks.val ∧
      objectCommit.val = objectTape.val + sidecarBlocks.val ∧
      objectRows.val = input.object_rows_before_object.val + 1 ∧
      sidecarAfter.val = input.sidecar_entries_before_object.val +
        sidecarsEmitted.val + (if remainder = 0#u64 then 0 else 1) ∧
      structuralFinal.val = input.structural_entries_before_object.val + 1 +
        sidecarsEmitted.val + (if remainder = 0#u64 then 0 else 1) +
        (if sidecarAfter = 0#u64 then 0 else 1) := by
  have hnMaximum : ¬ maximum.val < input.sidecar_entries_before_object.val := by
    omega
  have hEpochsVal : epochs.val = projectedFill.val /
      input.data_shards_per_epoch.val := by
    have hspec := U64.div_spec projectedFill
      (y := input.data_shards_per_epoch) hData
    simpa [hEpochs] using hspec
  have hRemainderVal : remainder.val = projectedFill.val %
      input.data_shards_per_epoch.val := by
    have hspec := U64.rem_spec projectedFill
      (y := input.data_shards_per_epoch) hData
    simpa [hRemainder] using hspec
  have hSidecarsVal := checked_add_result_value _ _ _ hSidecars
  have hSidecarBlocksVal := checked_mul_result_value _ _ _ hSidecarBlocks
  have hObjectTapeVal := checked_add_result_value _ _ _ hObjectTape
  have hObjectCommitVal := checked_add_result_value _ _ _ hObjectCommit
  have hObjectRowsVal := checked_add_result_value _ _ _ hObjectRows
  have hSidecarAfterCommitVal :=
    checked_add_result_value _ _ _ hSidecarAfterCommit
  have hSidecarAfterVal := checked_add_result_value _ _ _ hSidecarAfter
  have hStructural1Val := checked_add_result_value _ _ _ hStructural1
  have hStructural2Val := checked_add_result_value _ _ _ hStructural2
  have hStructural3Val := checked_add_result_value _ _ _ hStructural3
  have hStructuralFinalVal := checked_add_result_value _ _ _ hStructuralFinal
  have hSidecarFormula : sidecarAfter.val =
      input.sidecar_entries_before_object.val + sidecarsEmitted.val +
        (if remainder = 0#u64 then 0 else 1) := by
    rw [hSidecarAfterVal, hSidecarAfterCommitVal]
    by_cases hRem : remainder = 0#u64 <;> simp [hRem]
  have hStructuralFormula : structuralFinal.val =
      input.structural_entries_before_object.val + 1 + sidecarsEmitted.val +
        (if remainder = 0#u64 then 0 else 1) +
        (if sidecarAfter = 0#u64 then 0 else 1) := by
    rw [hStructuralFinalVal, hStructural3Val, hStructural2Val, hStructural1Val]
    by_cases hRem : remainder = 0#u64 <;>
      by_cases hMap : sidecarAfter = 0#u64 <;> simp [hRem, hMap]
  refine ⟨?_, hEpochsVal, hRemainderVal, hSidecarsVal, hSidecarBlocksVal,
    hObjectTapeVal, hObjectCommitVal, hObjectRowsVal, hSidecarFormula,
    hStructuralFormula⟩
  unfold compute_snapshot_projection_terms
  by_cases hRem : remainder = 0#u64 <;>
    by_cases hMap : sidecarAfter = 0#u64
  all_goals
    have hSidecarAfter' := hSidecarAfter
    have hStructural3' := hStructural3
    have hStructuralFinal' := hStructuralFinal
    have hDirectory' := hDirectory
    have hPayload' := hPayload
    simp [hRem] at hSidecarAfter' hStructural3'
    simp [hMap] at hStructuralFinal'
    simp [hMap] at hDirectory' hPayload'
    have hRemVal : (remainder.val = 0) = (remainder = 0#u64) := by
      apply propext
      constructor
      · intro h
        apply UScalar.eq_imp
        simpa using h
      · intro h
        simp [h]
    have hMapVal : (sidecarAfter.val = 0) = (sidecarAfter = 0#u64) := by
      apply propext
      constructor
      · intro h
        apply UScalar.eq_imp
        simpa using h
      · intro h
        simp [h]
    simp [hProjected, hEpochs, hRemainder, hSidecars, hSidecarBlocks,
      hObjectTape, hObjectCommit, hObjectRows, hSidecarAfterCommit, hRem,
      hRemVal, hSidecarAfter', hnMaximum, hDirectory', hPayload', hMap, hMapVal,
      hStructural1,
      hStructural2, hStructural3', hStructuralFinal',
      core.result.Result.Insts.CoreOpsTry.branch]

/-- C8-C10: the worst-case close branch (partial sidecar plus external
    ParityMap) includes each replicated control, every filemark, Bootstrap, and
    safety exactly once. -/
theorem compute_snapshot_control_terms_external_partial
    (input : SnapshotCloseInput) (sidecar : SnapshotSidecarTerms)
    (projection : SnapshotProjectionTerms)
    (snapshotPayload snapshotBlocks snapshotFile mapBlocks mapFile bootstrap
      close1 close2 close3 closeBound : Std.U64)
    (hPartial : projection.final_partial_sidecar_needed = true)
    (hMapNeeded : projection.final_parity_map_needed = true)
    (hSnapshotPayload : snapshot_payload_bytes
      projection.structural_entries_after_closeout projection.object_rows_after =
      ok (.Ok snapshotPayload))
    (hSnapshotBlocks : replicated_control_total_blocks input.block_size_bytes
      512#u64 snapshotPayload = ok (.Ok snapshotBlocks))
    (hSnapshotFile : checked_add snapshotBlocks input.snapshot_filemark_blocks =
      ok (.Ok snapshotFile))
    (hMapBlocks : replicated_control_total_blocks input.block_size_bytes
      184#u64 projection.final_parity_map_payload_bound_bytes = ok (.Ok mapBlocks))
    (hMapFile : checked_add mapBlocks input.parity_map_filemark_blocks =
      ok (.Ok mapFile))
    (hBootstrap : checked_add 1#u64 input.bootstrap_filemark_blocks =
      ok (.Ok bootstrap))
    (hClose1 : checked_add sidecar.tape_file_blocks mapFile = ok (.Ok close1))
    (hClose2 : checked_add close1 snapshotFile = ok (.Ok close2))
    (hClose3 : checked_add close2 bootstrap = ok (.Ok close3))
    (hCloseBound : checked_add close3 input.safety_margin_blocks =
      ok (.Ok closeBound)) :
    compute_snapshot_control_terms input sidecar projection = ok (.Ok {
      final_partial_sidecar_blocks := sidecar.tape_file_blocks,
      final_parity_map_blocks_before_filemark := mapBlocks,
      final_parity_map_tape_file_blocks := mapFile,
      snapshot_payload_bytes := snapshotPayload,
      snapshot_blocks_before_filemark := snapshotBlocks,
      snapshot_tape_file_blocks := snapshotFile,
      final_bootstrap_tape_file_blocks := bootstrap,
      close_bound_blocks := closeBound
    }) := by
  simp [compute_snapshot_control_terms, hPartial, hMapNeeded,
    snapshot_header_bytes, parity_map_header_bytes, block_count_per_bootstrap,
    hSnapshotPayload, hSnapshotBlocks, hSnapshotFile, hMapBlocks, hMapFile,
    hBootstrap, hClose1, hClose2, hClose3, hCloseBound,
    core.result.Result.Insts.CoreOpsTry.branch]

/-- C10: one theorem covers all four partial-sidecar / external-ParityMap
    branches. Conditional terms are zero exactly when their file is absent, and
    every present filemark is included once in the checked close sum. -/
theorem compute_snapshot_control_terms_all_branches
    (input : SnapshotCloseInput) (sidecar : SnapshotSidecarTerms)
    (projection : SnapshotProjectionTerms)
    (snapshotPayload snapshotBlocks snapshotFile mapBlocks mapFile bootstrap
      close1 close2 close3 closeBound : Std.U64)
    (hSnapshotPayload : snapshot_payload_bytes
      projection.structural_entries_after_closeout projection.object_rows_after =
      ok (.Ok snapshotPayload))
    (hSnapshotBlocks : replicated_control_total_blocks input.block_size_bytes
      512#u64 snapshotPayload = ok (.Ok snapshotBlocks))
    (hSnapshotFile : checked_add snapshotBlocks input.snapshot_filemark_blocks =
      ok (.Ok snapshotFile))
    (hMapBlocks : projection.final_parity_map_needed = true →
      replicated_control_total_blocks input.block_size_bytes 184#u64
        projection.final_parity_map_payload_bound_bytes = ok (.Ok mapBlocks))
    (hMapFile : projection.final_parity_map_needed = true →
      checked_add mapBlocks input.parity_map_filemark_blocks = ok (.Ok mapFile))
    (hBootstrap : checked_add 1#u64 input.bootstrap_filemark_blocks =
      ok (.Ok bootstrap))
    (hClose1 : checked_add
      (if projection.final_partial_sidecar_needed then sidecar.tape_file_blocks
       else 0#u64)
      (if projection.final_parity_map_needed then mapFile else 0#u64) =
      ok (.Ok close1))
    (hClose2 : checked_add close1 snapshotFile = ok (.Ok close2))
    (hClose3 : checked_add close2 bootstrap = ok (.Ok close3))
    (hCloseBound : checked_add close3 input.safety_margin_blocks =
      ok (.Ok closeBound)) :
    compute_snapshot_control_terms input sidecar projection = ok (.Ok {
      final_partial_sidecar_blocks :=
        if projection.final_partial_sidecar_needed then sidecar.tape_file_blocks
        else 0#u64,
      final_parity_map_blocks_before_filemark :=
        if projection.final_parity_map_needed then mapBlocks else 0#u64,
      final_parity_map_tape_file_blocks :=
        if projection.final_parity_map_needed then mapFile else 0#u64,
      snapshot_payload_bytes := snapshotPayload,
      snapshot_blocks_before_filemark := snapshotBlocks,
      snapshot_tape_file_blocks := snapshotFile,
      final_bootstrap_tape_file_blocks := bootstrap,
      close_bound_blocks := closeBound
    }) := by
  cases hPartial : projection.final_partial_sidecar_needed <;>
    cases hMap : projection.final_parity_map_needed
  all_goals
    have hClose1' := hClose1
    simp [hPartial, hMap] at hClose1'
  · simp [compute_snapshot_control_terms, hPartial, hMap,
      snapshot_header_bytes, block_count_per_bootstrap, hSnapshotPayload,
      hSnapshotBlocks, hSnapshotFile, hBootstrap, hClose1', hClose2, hClose3,
      hCloseBound, core.result.Result.Insts.CoreOpsTry.branch]
  · have hMapBlocks' := hMapBlocks hMap
    have hMapFile' := hMapFile hMap
    simp [compute_snapshot_control_terms, hPartial, hMap,
      snapshot_header_bytes, parity_map_header_bytes, block_count_per_bootstrap,
      hSnapshotPayload, hSnapshotBlocks, hSnapshotFile, hMapBlocks', hMapFile',
      hBootstrap, hClose1', hClose2, hClose3, hCloseBound,
      core.result.Result.Insts.CoreOpsTry.branch]
  · simp [compute_snapshot_control_terms, hPartial, hMap,
      snapshot_header_bytes, block_count_per_bootstrap, hSnapshotPayload,
      hSnapshotBlocks, hSnapshotFile, hBootstrap, hClose1', hClose2, hClose3,
      hCloseBound, core.result.Result.Insts.CoreOpsTry.branch]
  · have hMapBlocks' := hMapBlocks hMap
    have hMapFile' := hMapFile hMap
    simp [compute_snapshot_control_terms, hPartial, hMap,
      snapshot_header_bytes, parity_map_header_bytes, block_count_per_bootstrap,
      hSnapshotPayload, hSnapshotBlocks, hSnapshotFile, hMapBlocks', hMapFile',
      hBootstrap, hClose1', hClose2, hClose3, hCloseBound,
      core.result.Result.Insts.CoreOpsTry.branch]

/-- Input safety: Object rows and sidecar rows are disjoint recovery rows, so
    their checked sum may not exceed the structural prefix count. -/
theorem validate_snapshot_close_rejects_combined_recovery_rows
    (input : SnapshotCloseInput) (neighborhood recoveryRows : Std.U64)
    (hSupported : supported_snapshot_block_size input.block_size_bytes = ok true)
    (hData : input.data_shards_per_epoch ≠ 0#u64)
    (hParity : input.parity_shards_per_epoch ≠ 0#u64)
    (hNeighborhood : checked_add input.data_shards_per_epoch
      input.parity_shards_per_epoch = ok (.Ok neighborhood))
    (hNeighborhoodFits : neighborhood.val ≤ 4294967295)
    (hEpochOpen : input.current_epoch_fill_blocks.val <
      input.data_shards_per_epoch.val)
    (hObjectRows : input.object_rows_before_object.val ≤
      input.structural_entries_before_object.val)
    (hSidecarRows : input.sidecar_entries_before_object.val ≤
      input.structural_entries_before_object.val)
    (hRecoveryRows : checked_add input.object_rows_before_object
      input.sidecar_entries_before_object = ok (.Ok recoveryRows))
    (hTooMany : input.structural_entries_before_object.val < recoveryRows.val) :
    validate_snapshot_close_input input =
      ok (.Err CapacityError.RecoveryRowsExceedStructuralEntries) := by
  have hnNeighborhood : ¬ 4294967295 < neighborhood.val := by omega
  have hnEpochClosed : ¬ input.data_shards_per_epoch.val ≤
      input.current_epoch_fill_blocks.val := by omega
  have hnObjectRows : ¬ input.structural_entries_before_object.val <
      input.object_rows_before_object.val := by omega
  have hnSidecarRows : ¬ input.structural_entries_before_object.val <
      input.sidecar_entries_before_object.val := by omega
  simp [validate_snapshot_close_input, hSupported, hData, hParity,
    hNeighborhood, hnNeighborhood, hnEpochClosed, hnObjectRows, hnSidecarRows,
    hRecoveryRows, hTooMany, core.result.Result.Insts.CoreOpsTry.branch]

/-- Input safety: even internally consistent recovery rows cannot describe more
    structural entries than the empty-media physical capacity. -/
theorem validate_snapshot_close_rejects_structural_entries_above_capacity
    (input : SnapshotCloseInput) (neighborhood recoveryRows : Std.U64)
    (hSupported : supported_snapshot_block_size input.block_size_bytes = ok true)
    (hData : input.data_shards_per_epoch ≠ 0#u64)
    (hParity : input.parity_shards_per_epoch ≠ 0#u64)
    (hNeighborhood : checked_add input.data_shards_per_epoch
      input.parity_shards_per_epoch = ok (.Ok neighborhood))
    (hNeighborhoodFits : neighborhood.val ≤ 4294967295)
    (hEpochOpen : input.current_epoch_fill_blocks.val <
      input.data_shards_per_epoch.val)
    (hObjectRows : input.object_rows_before_object.val ≤
      input.structural_entries_before_object.val)
    (hSidecarRows : input.sidecar_entries_before_object.val ≤
      input.structural_entries_before_object.val)
    (hRecoveryRows : checked_add input.object_rows_before_object
      input.sidecar_entries_before_object = ok (.Ok recoveryRows))
    (hRecoveryFits : recoveryRows.val ≤ input.structural_entries_before_object.val)
    (hStructuralTooLarge : input.empty_tape_usable_blocks.val <
      input.structural_entries_before_object.val) :
    validate_snapshot_close_input input =
      ok (.Err CapacityError.StructuralEntriesExceedCapacity) := by
  have hnNeighborhood : ¬ 4294967295 < neighborhood.val := by omega
  have hnEpochClosed : ¬ input.data_shards_per_epoch.val ≤
      input.current_epoch_fill_blocks.val := by omega
  have hnObjectRows : ¬ input.structural_entries_before_object.val <
      input.object_rows_before_object.val := by omega
  have hnSidecarRows : ¬ input.structural_entries_before_object.val <
      input.sidecar_entries_before_object.val := by omega
  have hnRecoveryRows : ¬ input.structural_entries_before_object.val <
      recoveryRows.val := by omega
  simp [validate_snapshot_close_input, hSupported, hData, hParity,
    hNeighborhood, hnNeighborhood, hnEpochClosed, hnObjectRows, hnSidecarRows,
    hRecoveryRows, hnRecoveryRows, hStructuralTooLarge,
    core.result.Result.Insts.CoreOpsTry.branch]

/-- The structural-count ceiling is an input-validation rejection: evaluation
    returns it before sidecar sizing, profile validation, or Object projection. -/
theorem evaluate_snapshot_close_rejects_structural_entries_before_projection
    (input : SnapshotCloseInput)
    (hValidate : validate_snapshot_close_input input =
      ok (.Err CapacityError.StructuralEntriesExceedCapacity)) :
    evaluate_snapshot_close input =
      ok (.Err CapacityError.StructuralEntriesExceedCapacity) := by
  simp [evaluate_snapshot_close, hValidate,
    core.result.Result.Insts.CoreOpsTry.branch,
    core.result.Result.Insts.CoreOpsTryTraitFromResidualResultInfallible.from_residual]

/-- Stage 0 safety: any capacity-derived worst-profile failure is mapped to the
    terminal unsafe-profile error before projection arithmetic is entered. -/
theorem evaluate_snapshot_close_maps_profile_failure
    (input : SnapshotCloseInput) (sidecar : SnapshotSidecarTerms)
    (profileError : CapacityError)
    (hValidate : validate_snapshot_close_input input = ok (.Ok ()))
    (hSidecar : compute_snapshot_sidecar_terms input = ok (.Ok sidecar))
    (hProfile : validate_capacity_derived_profile_bounds input sidecar.tape_file_blocks =
      ok (.Err profileError)) :
    evaluate_snapshot_close input = ok (.Err CapacityError.UnsafeCapacityProfile) := by
  simp [evaluate_snapshot_close, hValidate, hSidecar, hProfile,
    core.result.Result.Insts.CoreOpsTry.branch]

/-- A high watermark above the capacity basis makes `C-H` underflow. The
    evaluator classifies that profile error as terminally unsafe before Object
    projection. -/
theorem evaluate_snapshot_close_maps_high_above_capacity_to_unsafe_profile
    (input : SnapshotCloseInput) (sidecar : SnapshotSidecarTerms)
    (hValidate : validate_snapshot_close_input input = ok (.Ok ()))
    (hSidecar : compute_snapshot_sidecar_terms input = ok (.Ok sidecar))
    (hHighAboveCapacity : input.empty_tape_usable_blocks.val <
      input.high_watermark_blocks.val) :
    evaluate_snapshot_close input =
      ok (.Err CapacityError.UnsafeCapacityProfile) := by
  have hBudget := checked_sub_underflow input.empty_tape_usable_blocks
    input.high_watermark_blocks hHighAboveCapacity
  have hProfile :
      validate_capacity_derived_profile_bounds input sidecar.tape_file_blocks =
        ok (.Err CapacityError.ArithmeticOverflow) := by
    simp [validate_capacity_derived_profile_bounds, hBudget,
      core.result.Result.Insts.CoreOpsTry.branch,
      core.result.Result.Insts.CoreOpsTryTraitFromResidualResultInfallible.from_residual]
  exact evaluate_snapshot_close_maps_profile_failure input sidecar
    CapacityError.ArithmeticOverflow hValidate hSidecar hProfile

/-- A complete sidecar that cannot fit on empty media is a terminal unsafe
    profile, not an Object-specific empty-tape rejection. -/
theorem evaluate_snapshot_close_maps_physical_sidecar_failure_to_unsafe_profile
    (input : SnapshotCloseInput) (sidecar : SnapshotSidecarTerms)
    (closeBudget : Std.U64)
    (hValidate : validate_snapshot_close_input input = ok (.Ok ()))
    (hSidecar : compute_snapshot_sidecar_terms input = ok (.Ok sidecar))
    (hCloseBudget : checked_sub input.empty_tape_usable_blocks
      input.high_watermark_blocks = ok (.Ok closeBudget))
    (hSidecarTooLarge : input.empty_tape_usable_blocks.val <
      sidecar.tape_file_blocks.val) :
    evaluate_snapshot_close input =
      ok (.Err CapacityError.UnsafeCapacityProfile) := by
  have hProfile :
      validate_capacity_derived_profile_bounds input sidecar.tape_file_blocks =
        ok (.Err CapacityError.CapacityProfileCloseExceedsCapacity) := by
    simp [validate_capacity_derived_profile_bounds, hCloseBudget,
      hSidecarTooLarge, core.result.Result.Insts.CoreOpsTry.branch]
  exact evaluate_snapshot_close_maps_profile_failure input sidecar
    CapacityError.CapacityProfileCloseExceedsCapacity hValidate hSidecar hProfile

/-- A checked worst-case close containing one maximum complete sidecar, the
    maximum ParityMap, maximum snapshot, final bootstrap, and safety allowance
    is also a profile check. If that bundle exceeds the checked `C-H` closeout budget,
    evaluation reports `UnsafeCapacityProfile`, never `ObjectTooLargeForEmptyTape`. -/
theorem evaluate_snapshot_close_maps_worst_close_failure_to_unsafe_profile
    (input : SnapshotCloseInput) (sidecar : SnapshotSidecarTerms)
    (closeBudget minimumBase minimumSidecar maximum directoryBound payloadBound mapBlocks
      mapFile snapshotPayload snapshotBlocks snapshotFile bootstrap close1 close2
      close3 worstClose : Std.U64)
    (hValidate : validate_snapshot_close_input input = ok (.Ok ()))
    (hSidecar : compute_snapshot_sidecar_terms input = ok (.Ok sidecar))
    (hCloseBudget : checked_sub input.empty_tape_usable_blocks
      input.high_watermark_blocks = ok (.Ok closeBudget))
    (hSidecarFits : sidecar.tape_file_blocks.val ≤
      input.empty_tape_usable_blocks.val)
    (hMinimumBase : checked_add input.parity_shards_per_epoch 3#u64 =
      ok (.Ok minimumBase))
    (hMinimumSidecar : checked_add minimumBase input.sidecar_filemark_blocks =
      ok (.Ok minimumSidecar))
    (hMaximum : input.empty_tape_usable_blocks / minimumSidecar = ok maximum)
    (hDirectory : parity_map_directory_len_upper_bound maximum =
      ok (.Ok directoryBound))
    (hPayload : parity_map_payload_len_upper_bound maximum = ok (.Ok payloadBound))
    (hMapBlocks : replicated_control_total_blocks input.block_size_bytes 184#u64
      payloadBound = ok (.Ok mapBlocks))
    (hMaximumNonzero : maximum ≠ 0#u64)
    (hMapFile : checked_add mapBlocks input.parity_map_filemark_blocks =
      ok (.Ok mapFile))
    (hSnapshotPayload : snapshot_payload_bytes input.empty_tape_usable_blocks
      input.empty_tape_usable_blocks = ok (.Ok snapshotPayload))
    (hSnapshotBlocks : replicated_control_total_blocks input.block_size_bytes
      512#u64 snapshotPayload = ok (.Ok snapshotBlocks))
    (hSnapshotFile : checked_add snapshotBlocks input.snapshot_filemark_blocks =
      ok (.Ok snapshotFile))
    (hBootstrap : checked_add 1#u64 input.bootstrap_filemark_blocks =
      ok (.Ok bootstrap))
    (hClose1 : checked_add sidecar.tape_file_blocks mapFile = ok (.Ok close1))
    (hClose2 : checked_add close1 snapshotFile = ok (.Ok close2))
    (hClose3 : checked_add close2 bootstrap = ok (.Ok close3))
    (hWorstClose : checked_add close3 input.safety_margin_blocks =
      ok (.Ok worstClose))
    (hWorstCloseTooLarge : closeBudget.val < worstClose.val) :
    evaluate_snapshot_close input =
      ok (.Err CapacityError.UnsafeCapacityProfile) := by
  have hnSidecarTooLarge : ¬ input.empty_tape_usable_blocks.val <
      sidecar.tape_file_blocks.val := by omega
  have hProfile :
      validate_capacity_derived_profile_bounds input sidecar.tape_file_blocks =
        ok (.Err CapacityError.CapacityProfileCloseExceedsCapacity) := by
    simp [validate_capacity_derived_profile_bounds, hCloseBudget, hnSidecarTooLarge,
      hMinimumBase, hMinimumSidecar, hMaximum, hDirectory, hPayload,
      parity_map_header_bytes, hMapBlocks, hMaximumNonzero, hMapFile,
      hSnapshotPayload, snapshot_header_bytes, hSnapshotBlocks, hSnapshotFile,
      block_count_per_bootstrap, hBootstrap, hClose1, hClose2, hClose3,
      hWorstClose, hWorstCloseTooLarge,
      core.result.Result.Insts.CoreOpsTry.branch]
  exact evaluate_snapshot_close_maps_profile_failure input sidecar
    CapacityError.CapacityProfileCloseExceedsCapacity hValidate hSidecar hProfile

/-- The reserve-budget comparison is non-strict: a fully checked worst close
    equal to `C-H` succeeds through profile validation and returns the physical
    sidecar-directory ceiling. -/
theorem validate_capacity_profile_accepts_worst_close_equal_to_budget
    (input : SnapshotCloseInput) (maximumCompleteSidecar : Std.U64)
    (closeBudget minimumBase minimumSidecar maximum directoryBound payloadBound
      mapBlocks mapFile snapshotPayload snapshotBlocks snapshotFile bootstrap
      close1 close2 close3 worstClose : Std.U64)
    (hCloseBudget : checked_sub input.empty_tape_usable_blocks
      input.high_watermark_blocks = ok (.Ok closeBudget))
    (hSidecarFits : maximumCompleteSidecar.val ≤
      input.empty_tape_usable_blocks.val)
    (hMinimumBase : checked_add input.parity_shards_per_epoch 3#u64 =
      ok (.Ok minimumBase))
    (hMinimumSidecar : checked_add minimumBase input.sidecar_filemark_blocks =
      ok (.Ok minimumSidecar))
    (hMaximum : input.empty_tape_usable_blocks / minimumSidecar = ok maximum)
    (hDirectory : parity_map_directory_len_upper_bound maximum =
      ok (.Ok directoryBound))
    (hPayload : parity_map_payload_len_upper_bound maximum = ok (.Ok payloadBound))
    (hMapBlocks : replicated_control_total_blocks input.block_size_bytes 184#u64
      payloadBound = ok (.Ok mapBlocks))
    (hMaximumNonzero : maximum ≠ 0#u64)
    (hMapFile : checked_add mapBlocks input.parity_map_filemark_blocks =
      ok (.Ok mapFile))
    (hSnapshotPayload : snapshot_payload_bytes input.empty_tape_usable_blocks
      input.empty_tape_usable_blocks = ok (.Ok snapshotPayload))
    (hSnapshotBlocks : replicated_control_total_blocks input.block_size_bytes
      512#u64 snapshotPayload = ok (.Ok snapshotBlocks))
    (hSnapshotFile : checked_add snapshotBlocks input.snapshot_filemark_blocks =
      ok (.Ok snapshotFile))
    (hBootstrap : checked_add 1#u64 input.bootstrap_filemark_blocks =
      ok (.Ok bootstrap))
    (hClose1 : checked_add maximumCompleteSidecar mapFile = ok (.Ok close1))
    (hClose2 : checked_add close1 snapshotFile = ok (.Ok close2))
    (hClose3 : checked_add close2 bootstrap = ok (.Ok close3))
    (hWorstClose : checked_add close3 input.safety_margin_blocks =
      ok (.Ok worstClose))
    (hEqualBudget : worstClose.val = closeBudget.val) :
    validate_capacity_derived_profile_bounds input maximumCompleteSidecar =
      ok (.Ok maximum) := by
  have hnSidecarTooLarge : ¬ input.empty_tape_usable_blocks.val <
      maximumCompleteSidecar.val := by omega
  have hnWorstCloseTooLarge : ¬ closeBudget.val < worstClose.val := by omega
  simp [validate_capacity_derived_profile_bounds, hCloseBudget,
    hnSidecarTooLarge, hMinimumBase, hMinimumSidecar, hMaximum, hDirectory,
    hPayload, parity_map_header_bytes, hMapBlocks, hMaximumNonzero, hMapFile,
    hSnapshotPayload, snapshot_header_bytes, hSnapshotBlocks, hSnapshotFile,
    block_count_per_bootstrap, hBootstrap, hClose1, hClose2, hClose3,
    hWorstClose, hnWorstCloseTooLarge,
    core.result.Result.Insts.CoreOpsTry.branch]

/-- C10 success: non-strict capacity and spool checks allow equality. The
    returned required terms are exactly the checked Object-plus-close and spool
    computations. -/
theorem evaluate_snapshot_close_success
    (input : SnapshotCloseInput) (sidecar : SnapshotSidecarTerms)
    (projection : SnapshotProjectionTerms) (control : SnapshotControlTerms)
    (maximum required sidecarBytes newSidecarBytes requiredSpool : Std.U64)
    (hValidate : validate_snapshot_close_input input = ok (.Ok ()))
    (hSidecar : compute_snapshot_sidecar_terms input = ok (.Ok sidecar))
    (hProfile : validate_capacity_derived_profile_bounds input sidecar.tape_file_blocks =
      ok (.Ok maximum))
    (hProjection : compute_snapshot_projection_terms input sidecar maximum =
      ok (.Ok projection))
    (hControl : compute_snapshot_control_terms input sidecar projection =
      ok (.Ok control))
    (hRequired : checked_add projection.object_commit_charge_blocks
      control.close_bound_blocks = ok (.Ok required))
    (hEmptyFits : required.val ≤ input.empty_tape_usable_blocks.val)
    (hCurrentFits : required.val ≤ input.remaining_tape_blocks.val)
    (hSidecarBytes : checked_mul sidecar.blocks_before_filemark
      input.block_size_bytes = ok (.Ok sidecarBytes))
    (hNewSidecarBytes : checked_mul projection.epochs_completed_by_object
      sidecarBytes = ok (.Ok newSidecarBytes))
    (hSpool : checked_add input.pending_completed_epoch_parity_bytes
      newSidecarBytes = ok (.Ok requiredSpool))
    (hSpoolFits : requiredSpool.val ≤ input.remaining_spool_bytes.val) :
    ∃ report,
      evaluate_snapshot_close input = ok (.Ok report) ∧
      report.required_tape_blocks = required ∧
      report.required_spool_bytes = requiredSpool ∧
      report.object_commit_charge_blocks = projection.object_commit_charge_blocks ∧
      report.close_bound_blocks = control.close_bound_blocks ∧
      report.final_parity_map_needed = projection.final_parity_map_needed := by
  have hnEmpty : ¬ input.empty_tape_usable_blocks.val < required.val := by omega
  have hnCurrent : ¬ input.remaining_tape_blocks.val < required.val := by omega
  have hnSpool : ¬ input.remaining_spool_bytes.val < requiredSpool.val := by omega
  refine ⟨{
    epochs_completed_by_object := projection.epochs_completed_by_object,
    final_partial_sidecar_needed := projection.final_partial_sidecar_needed,
    sidecar_index_block_count := sidecar.index_block_count,
    sidecar_blocks_before_filemark := sidecar.blocks_before_filemark,
    sidecar_tape_file_blocks := sidecar.tape_file_blocks,
    sidecars_emitted_by_commit := projection.sidecars_emitted_by_commit,
    sidecar_blocks_emitted_by_commit := projection.sidecar_blocks_emitted_by_commit,
    object_tape_file_blocks := projection.object_tape_file_blocks,
    object_commit_charge_blocks := projection.object_commit_charge_blocks,
    object_rows_after := projection.object_rows_after,
    sidecar_entries_after_closeout := projection.sidecar_entries_after_closeout,
    maximum_sidecar_entries_for_capacity :=
      projection.maximum_sidecar_entries_for_capacity,
    structural_entries_after_closeout := projection.structural_entries_after_closeout,
    final_partial_sidecar_blocks := control.final_partial_sidecar_blocks,
    final_parity_map_needed := projection.final_parity_map_needed,
    final_parity_map_directory_bound_bytes :=
      projection.final_parity_map_directory_bound_bytes,
    final_parity_map_payload_bound_bytes :=
      projection.final_parity_map_payload_bound_bytes,
    final_parity_map_blocks_before_filemark :=
      control.final_parity_map_blocks_before_filemark,
    final_parity_map_tape_file_blocks := control.final_parity_map_tape_file_blocks,
    snapshot_payload_bytes := control.snapshot_payload_bytes,
    snapshot_blocks_before_filemark := control.snapshot_blocks_before_filemark,
    snapshot_tape_file_blocks := control.snapshot_tape_file_blocks,
    final_bootstrap_tape_file_blocks := control.final_bootstrap_tape_file_blocks,
    safety_margin_blocks := input.safety_margin_blocks,
    close_bound_blocks := control.close_bound_blocks,
    required_tape_blocks := required,
    required_spool_bytes := requiredSpool
  }, ?_, rfl, rfl, rfl, rfl, rfl⟩
  simp [evaluate_snapshot_close, hValidate, hSidecar, hProfile, hProjection,
    hControl, hRequired, hnEmpty, hnCurrent, hSidecarBytes, hNewSidecarBytes,
    hSpool, hnSpool, core.result.Result.Insts.CoreOpsTry.branch]

/-- C10 overflow propagation: Object commit plus close-bound overflow fails
    closed before either tape-capacity comparison. -/
theorem evaluate_snapshot_close_rejects_required_tape_overflow
    (input : SnapshotCloseInput) (sidecar : SnapshotSidecarTerms)
    (projection : SnapshotProjectionTerms) (control : SnapshotControlTerms)
    (maximum : Std.U64)
    (hValidate : validate_snapshot_close_input input = ok (.Ok ()))
    (hSidecar : compute_snapshot_sidecar_terms input = ok (.Ok sidecar))
    (hProfile : validate_capacity_derived_profile_bounds input sidecar.tape_file_blocks =
      ok (.Ok maximum))
    (hProjection : compute_snapshot_projection_terms input sidecar maximum =
      ok (.Ok projection))
    (hControl : compute_snapshot_control_terms input sidecar projection =
      ok (.Ok control))
    (hOverflow : checked_add projection.object_commit_charge_blocks
      control.close_bound_blocks = ok (.Err CapacityError.ArithmeticOverflow)) :
    evaluate_snapshot_close input = ok (.Err CapacityError.ArithmeticOverflow) := by
  simp [evaluate_snapshot_close, hValidate, hSidecar, hProfile, hProjection,
    hControl, hOverflow, core.result.Result.Insts.CoreOpsTry.branch,
    core.result.Result.Insts.CoreOpsTryTraitFromResidualResultInfallible.from_residual]

/-- C10 overflow propagation: encoded sidecar byte-count overflow fails closed
    after both tape checks succeed. -/
theorem evaluate_snapshot_close_rejects_sidecar_byte_overflow
    (input : SnapshotCloseInput) (sidecar : SnapshotSidecarTerms)
    (projection : SnapshotProjectionTerms) (control : SnapshotControlTerms)
    (maximum required : Std.U64)
    (hValidate : validate_snapshot_close_input input = ok (.Ok ()))
    (hSidecar : compute_snapshot_sidecar_terms input = ok (.Ok sidecar))
    (hProfile : validate_capacity_derived_profile_bounds input sidecar.tape_file_blocks =
      ok (.Ok maximum))
    (hProjection : compute_snapshot_projection_terms input sidecar maximum =
      ok (.Ok projection))
    (hControl : compute_snapshot_control_terms input sidecar projection =
      ok (.Ok control))
    (hRequired : checked_add projection.object_commit_charge_blocks
      control.close_bound_blocks = ok (.Ok required))
    (hEmptyFits : required.val ≤ input.empty_tape_usable_blocks.val)
    (hCurrentFits : required.val ≤ input.remaining_tape_blocks.val)
    (hOverflow : checked_mul sidecar.blocks_before_filemark
      input.block_size_bytes = ok (.Err CapacityError.ArithmeticOverflow)) :
    evaluate_snapshot_close input = ok (.Err CapacityError.ArithmeticOverflow) := by
  have hnEmpty : ¬ input.empty_tape_usable_blocks.val < required.val := by omega
  have hnCurrent : ¬ input.remaining_tape_blocks.val < required.val := by omega
  simp [evaluate_snapshot_close, hValidate, hSidecar, hProfile, hProjection,
    hControl, hRequired, hnEmpty, hnCurrent, hOverflow,
    core.result.Result.Insts.CoreOpsTry.branch,
    core.result.Result.Insts.CoreOpsTryTraitFromResidualResultInfallible.from_residual]

/-- C10 overflow propagation: completed-sidecar spool multiplication overflow
    fails closed. -/
theorem evaluate_snapshot_close_rejects_completed_spool_overflow
    (input : SnapshotCloseInput) (sidecar : SnapshotSidecarTerms)
    (projection : SnapshotProjectionTerms) (control : SnapshotControlTerms)
    (maximum required sidecarBytes : Std.U64)
    (hValidate : validate_snapshot_close_input input = ok (.Ok ()))
    (hSidecar : compute_snapshot_sidecar_terms input = ok (.Ok sidecar))
    (hProfile : validate_capacity_derived_profile_bounds input sidecar.tape_file_blocks =
      ok (.Ok maximum))
    (hProjection : compute_snapshot_projection_terms input sidecar maximum =
      ok (.Ok projection))
    (hControl : compute_snapshot_control_terms input sidecar projection =
      ok (.Ok control))
    (hRequired : checked_add projection.object_commit_charge_blocks
      control.close_bound_blocks = ok (.Ok required))
    (hEmptyFits : required.val ≤ input.empty_tape_usable_blocks.val)
    (hCurrentFits : required.val ≤ input.remaining_tape_blocks.val)
    (hSidecarBytes : checked_mul sidecar.blocks_before_filemark
      input.block_size_bytes = ok (.Ok sidecarBytes))
    (hOverflow : checked_mul projection.epochs_completed_by_object sidecarBytes =
      ok (.Err CapacityError.ArithmeticOverflow)) :
    evaluate_snapshot_close input = ok (.Err CapacityError.ArithmeticOverflow) := by
  have hnEmpty : ¬ input.empty_tape_usable_blocks.val < required.val := by omega
  have hnCurrent : ¬ input.remaining_tape_blocks.val < required.val := by omega
  simp [evaluate_snapshot_close, hValidate, hSidecar, hProfile, hProjection,
    hControl, hRequired, hnEmpty, hnCurrent, hSidecarBytes, hOverflow,
    core.result.Result.Insts.CoreOpsTry.branch,
    core.result.Result.Insts.CoreOpsTryTraitFromResidualResultInfallible.from_residual]

/-- C10 overflow propagation: pending plus newly completed spool bytes overflow
    fails closed. -/
theorem evaluate_snapshot_close_rejects_required_spool_overflow
    (input : SnapshotCloseInput) (sidecar : SnapshotSidecarTerms)
    (projection : SnapshotProjectionTerms) (control : SnapshotControlTerms)
    (maximum required sidecarBytes newSidecarBytes : Std.U64)
    (hValidate : validate_snapshot_close_input input = ok (.Ok ()))
    (hSidecar : compute_snapshot_sidecar_terms input = ok (.Ok sidecar))
    (hProfile : validate_capacity_derived_profile_bounds input sidecar.tape_file_blocks =
      ok (.Ok maximum))
    (hProjection : compute_snapshot_projection_terms input sidecar maximum =
      ok (.Ok projection))
    (hControl : compute_snapshot_control_terms input sidecar projection =
      ok (.Ok control))
    (hRequired : checked_add projection.object_commit_charge_blocks
      control.close_bound_blocks = ok (.Ok required))
    (hEmptyFits : required.val ≤ input.empty_tape_usable_blocks.val)
    (hCurrentFits : required.val ≤ input.remaining_tape_blocks.val)
    (hSidecarBytes : checked_mul sidecar.blocks_before_filemark
      input.block_size_bytes = ok (.Ok sidecarBytes))
    (hNewSidecarBytes : checked_mul projection.epochs_completed_by_object
      sidecarBytes = ok (.Ok newSidecarBytes))
    (hOverflow : checked_add input.pending_completed_epoch_parity_bytes
      newSidecarBytes = ok (.Err CapacityError.ArithmeticOverflow)) :
    evaluate_snapshot_close input = ok (.Err CapacityError.ArithmeticOverflow) := by
  have hnEmpty : ¬ input.empty_tape_usable_blocks.val < required.val := by omega
  have hnCurrent : ¬ input.remaining_tape_blocks.val < required.val := by omega
  simp [evaluate_snapshot_close, hValidate, hSidecar, hProfile, hProjection,
    hControl, hRequired, hnEmpty, hnCurrent, hSidecarBytes, hNewSidecarBytes,
    hOverflow, core.result.Result.Insts.CoreOpsTry.branch,
    core.result.Result.Insts.CoreOpsTryTraitFromResidualResultInfallible.from_residual]

/-- C10a: empty-media infeasibility dominates current-tape and spool gates. -/
theorem evaluate_snapshot_close_empty_tape_gate
    (input : SnapshotCloseInput) (sidecar : SnapshotSidecarTerms)
    (projection : SnapshotProjectionTerms) (control : SnapshotControlTerms)
    (maximum required : Std.U64)
    (hValidate : validate_snapshot_close_input input = ok (.Ok ()))
    (hSidecar : compute_snapshot_sidecar_terms input = ok (.Ok sidecar))
    (hProfile : validate_capacity_derived_profile_bounds input sidecar.tape_file_blocks =
      ok (.Ok maximum))
    (hProjection : compute_snapshot_projection_terms input sidecar maximum =
      ok (.Ok projection))
    (hControl : compute_snapshot_control_terms input sidecar projection =
      ok (.Ok control))
    (hRequired : checked_add projection.object_commit_charge_blocks
      control.close_bound_blocks = ok (.Ok required))
    (hShort : input.empty_tape_usable_blocks.val < required.val) :
    evaluate_snapshot_close input =
      ok (.Err CapacityError.ObjectTooLargeForEmptyTape) := by
  simp [evaluate_snapshot_close, hValidate, hSidecar, hProfile, hProjection, hControl,
    hRequired, hShort, core.result.Result.Insts.CoreOpsTry.branch]

/-- C10b: current-tape shortfall is the retry gate after empty-media fit. -/
theorem evaluate_snapshot_close_current_tape_gate
    (input : SnapshotCloseInput) (sidecar : SnapshotSidecarTerms)
    (projection : SnapshotProjectionTerms) (control : SnapshotControlTerms)
    (maximum required : Std.U64)
    (hValidate : validate_snapshot_close_input input = ok (.Ok ()))
    (hSidecar : compute_snapshot_sidecar_terms input = ok (.Ok sidecar))
    (hProfile : validate_capacity_derived_profile_bounds input sidecar.tape_file_blocks =
      ok (.Ok maximum))
    (hProjection : compute_snapshot_projection_terms input sidecar maximum =
      ok (.Ok projection))
    (hControl : compute_snapshot_control_terms input sidecar projection =
      ok (.Ok control))
    (hRequired : checked_add projection.object_commit_charge_blocks
      control.close_bound_blocks = ok (.Ok required))
    (hEmptyFits : required.val ≤ input.empty_tape_usable_blocks.val)
    (hCurrentShort : input.remaining_tape_blocks.val < required.val) :
    evaluate_snapshot_close input =
      ok (.Err CapacityError.CapacityReserveExceededTape) := by
  have hnEmpty : ¬ input.empty_tape_usable_blocks.val < required.val := by omega
  simp [evaluate_snapshot_close, hValidate, hSidecar, hProfile, hProjection, hControl,
    hRequired, hnEmpty, hCurrentShort,
    core.result.Result.Insts.CoreOpsTry.branch]

/-- C10c: spool exhaustion remains a distinct remedy after both tape gates. -/
theorem evaluate_snapshot_close_spool_gate
    (input : SnapshotCloseInput) (sidecar : SnapshotSidecarTerms)
    (projection : SnapshotProjectionTerms) (control : SnapshotControlTerms)
    (maximum required sidecarBytes newSidecarBytes requiredSpool : Std.U64)
    (hValidate : validate_snapshot_close_input input = ok (.Ok ()))
    (hSidecar : compute_snapshot_sidecar_terms input = ok (.Ok sidecar))
    (hProfile : validate_capacity_derived_profile_bounds input sidecar.tape_file_blocks =
      ok (.Ok maximum))
    (hProjection : compute_snapshot_projection_terms input sidecar maximum =
      ok (.Ok projection))
    (hControl : compute_snapshot_control_terms input sidecar projection =
      ok (.Ok control))
    (hRequired : checked_add projection.object_commit_charge_blocks
      control.close_bound_blocks = ok (.Ok required))
    (hEmptyFits : required.val ≤ input.empty_tape_usable_blocks.val)
    (hCurrentFits : required.val ≤ input.remaining_tape_blocks.val)
    (hSidecarBytes : checked_mul sidecar.blocks_before_filemark
      input.block_size_bytes = ok (.Ok sidecarBytes))
    (hNewSidecarBytes : checked_mul projection.epochs_completed_by_object
      sidecarBytes = ok (.Ok newSidecarBytes))
    (hSpool : checked_add input.pending_completed_epoch_parity_bytes
      newSidecarBytes = ok (.Ok requiredSpool))
    (hSpoolShort : input.remaining_spool_bytes.val < requiredSpool.val) :
    evaluate_snapshot_close input =
      ok (.Err CapacityError.CapacityReserveExceededSpool) := by
  have hnEmpty : ¬ input.empty_tape_usable_blocks.val < required.val := by omega
  have hnCurrent : ¬ input.remaining_tape_blocks.val < required.val := by omega
  simp [evaluate_snapshot_close, hValidate, hSidecar, hProfile, hProjection, hControl,
    hRequired, hnEmpty, hnCurrent, hSidecarBytes, hNewSidecarBytes, hSpool,
    hSpoolShort, core.result.Result.Insts.CoreOpsTry.branch]

end parity_capacity_verif
