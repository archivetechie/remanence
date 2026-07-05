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

end parity_capacity_verif
