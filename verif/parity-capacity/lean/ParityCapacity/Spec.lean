/- Specification theorems for the parity-capacity extraction (SPEC.md C1-C4).

   Targets the Aeneas-generated definitions in `ParityCapacity.Funs`. The Lean
   checker accepting this file with no remaining local placeholders is the
   success criterion; the generated file is trusted only through Aeneas plus
   Lean, and the Rust `drift_guard` test ties the extraction back to production
   `crates/remanence-parity/src/capacity.rs`. -/
import ParityCapacity.Funs

open Aeneas Aeneas.Std Result

namespace parity_capacity_verif

/- Formal-proof scope:
   these theorems certify the extracted pure capacity arithmetic: exact
   sidecar/ParityMap closeout, terminal payload and five-file tail geometry,
   structural admission, automatic/manual close gate ordering, and fail-closed
   tape/spool/profile behavior. They do not prove the whole writer, catalog,
   tape device, or production error payload text; those remain covered by the
   extraction drift guard and normal Rust tests. -/

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

/- Terminal-triple C1-C4. -/

/-- C2: the extracted scalar packer computes the shipped 256 KiB profile
    exactly: three index blocks, with block zero filled through its usable
    payload after the 200-byte header and eight-byte trailing CRC. -/
theorem shipped_sidecar_index_capacity_layout :
    checked_sidecar_index_capacity_layout 262144#u64 2048#u64 65536#u64 =
      ok (.Ok {
        block_count := 3#u64,
        inline_entry_bytes := 261936#u64
      }) := by
  have hComputed :
      (match checked_sidecar_index_capacity_layout
          262144#u64 2048#u64 65536#u64 with
      | ok (.Ok layout) =>
          decide (layout.block_count.val = 3) &&
          decide (layout.inline_entry_bytes.val = 261936)
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
          have hInlineEq : layout.inline_entry_bytes = 261936#u64 := by
            apply UScalar.eq_imp
            simpa using hInline
          cases layout
          simp_all
      | Err error => simp at hComputed
  | fail error => simp at hComputed
  | div => simp at hComputed

/-- C2: the complete ParityMap payload bound is exactly `325 + 116*N`. -/
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

/-- C1/C2 boundary: thirty 4096-byte rows retain the minimum three-block
    external-ParityMap body, while row thirty-one crosses to five body blocks.
    The trailing filemark is included in both reported tape-file charges. -/
theorem parity_map_capacity_layout_4096_row_boundary :
    (match checked_parity_map_capacity_layout 4096#u64 30#u64 1#u64 with
      | ok (.Ok layout) =>
          decide (layout.payload_bound_bytes.val = 3805) &&
          decide (layout.blocks_before_filemark.val = 3) &&
          decide (layout.tape_file_blocks.val = 4)
      | _ => false) = true ∧
    (match checked_parity_map_capacity_layout 4096#u64 31#u64 1#u64 with
      | ok (.Ok layout) =>
          decide (layout.payload_bound_bytes.val = 3921) &&
          decide (layout.blocks_before_filemark.val = 5) &&
          decide (layout.tape_file_blocks.val = 6)
      | _ => false) = true := by
  native_decide

/-- C1 overflow boundary: one row beyond the largest representable
    `116*N` product is rejected rather than wrapping the map payload bound. -/
theorem parity_map_payload_row_overflow_fails_closed :
    (match parity_map_payload_len_upper_bound 159023655807840963#u64 with
      | ok (.Err CapacityError.ArithmeticOverflow) => true
      | _ => false) = true := by
  native_decide

/-- C2: the inline-directory bound is exactly `43 + 116*N`. -/
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

/-- C1: an Object-row count above the structural count is rejected before
    terminal payload arithmetic. -/
theorem terminal_payload_rejects_row_count_above_structure
    (structural objectRows : Std.U64)
    (h : structural.val < objectRows.val) :
    terminal_payload_bytes structural objectRows =
      ok (.Err CapacityError.ObjectRowsExceedStructuralEntries) := by
  simp [terminal_payload_bytes, h]

/-- C1: the accepted terminal payload is exactly `64*S + 256*R`; each
    multiplication and the final sum are checked rather than wrapped. -/
theorem terminal_payload_success (structural objectRows : Std.U64)
    (hRows : objectRows.val ≤ structural.val)
    (hStructural : structural.val * 64 < 2 ^ 64)
    (hObjects : objectRows.val * 256 < 2 ^ 64)
    (hTotal : structural.val * 64 + objectRows.val * 256 < 2 ^ 64) :
    ∃ payload,
      terminal_payload_bytes structural objectRows = ok (.Ok payload) ∧
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
  · simp [terminal_payload_bytes, terminal_structural_slot_bytes,
      terminal_object_row_slot_bytes, hnRows, hStructuralEval, hObjectEval,
      hPayloadEval, core.result.Result.Insts.CoreOpsTry.branch]
  · rw [hPayloadVal, hStructuralVal, hObjectVal]
    norm_num

/-- C1: a replica contains one header record, the rounded payload records, and
    one replica-local footer record. Its trailing filemark is deliberately not
    part of `recordsBeforeFilemark`. -/
theorem terminal_replica_layout_of_intermediates
    (blockSize structural objectRows payload blockMinusOne adjusted
      payloadRecords recordsBeforeFilemark : Std.U64)
    (hSupported : supported_terminal_block_size blockSize = ok true)
    (hPayload : terminal_payload_bytes structural objectRows = ok (.Ok payload))
    (hMinusOne : checked_sub blockSize 1#u64 = ok (.Ok blockMinusOne))
    (hAdjusted : checked_add payload blockMinusOne = ok (.Ok adjusted))
    (hRecords : adjusted / blockSize = ok payloadRecords)
    (hHeaderFooter : checked_add payloadRecords 2#u64 =
      ok (.Ok recordsBeforeFilemark)) :
    terminal_replica_layout blockSize structural objectRows =
      ok (.Ok (payload, payloadRecords, recordsBeforeFilemark)) := by
  simp [terminal_replica_layout, hSupported, hPayload, hMinusOne, hAdjusted,
    hRecords, hHeaderFooter, core.result.Result.Insts.CoreOpsTry.branch]

/-- C1: the physical charge of one replica is its header/payload/local-footer
    record count plus exactly one caller-supplied filemark charge. -/
theorem terminal_replica_filemark_charge
    (recordsBeforeFilemark filemark tapeFile : Std.U64)
    (h : checked_add recordsBeforeFilemark filemark = ok (.Ok tapeFile)) :
    tapeFile.val = recordsBeforeFilemark.val + filemark.val := by
  exact checked_add_result_value _ _ _ h

/-- C1: the fixed 1 GiB separation extent is exact at every supported block
    size. These are records before the separate gap filemark. -/
theorem one_gib_index_separation_records_exact :
    (match index_separation_records 262144#u64 1073741824#u64 with
      | ok (.Ok records) => decide (records.val = 4096)
      | _ => false) = true ∧
    (match index_separation_records 524288#u64 1073741824#u64 with
      | ok (.Ok records) => decide (records.val = 2048)
      | _ => false) = true ∧
    (match index_separation_records 1048576#u64 1073741824#u64 with
      | ok (.Ok records) => decide (records.val = 1024)
      | _ => false) = true := by
  native_decide

/-- C1: the complete terminal-tail multiplicities are exact: three identically
    charged replicas and two identically charged 1 GiB separation files. -/
theorem terminal_tail_multiplicities
    (replicaFile tripleReplicas gapFile doubleGaps tail : Std.U64)
    (hTriple : checked_mul 3#u64 replicaFile = ok (.Ok tripleReplicas))
    (hDouble : checked_mul 2#u64 gapFile = ok (.Ok doubleGaps))
    (hTail : checked_add tripleReplicas doubleGaps = ok (.Ok tail)) :
    tripleReplicas.val = 3 * replicaFile.val ∧
    doubleGaps.val = 2 * gapFile.val ∧
    tail.val = tripleReplicas.val + doubleGaps.val := by
  exact ⟨checked_mul_result_value _ _ _ hTriple,
    checked_mul_result_value _ _ _ hDouble,
    checked_add_result_value _ _ _ hTail⟩

/-- C1-C2: the checked control constructor assembles exactly one local
    footer and filemark per replica, three replicas, two separation files, the
    optional parity closeout, and safety. Both optional closeout branches are
    covered; the terminal triple and two gaps are unconditional. -/
theorem compute_terminal_control_terms_all_branches
    (input : TerminalTripleCloseInput) (sidecar : TerminalSidecarTerms)
    (projection : TerminalProjectionTerms)
    (replicaPayload replicaPayloadRecords replicaRecords replicaFile triple partialFile
      gapRecords gapFile doubleGaps mapBlocks mapFile parityClose tail
      close1 closeBound : Std.U64)
    (hReplica : terminal_replica_layout input.block_size_bytes
      projection.structural_entries_after_closeout projection.object_rows_after =
      ok (.Ok (replicaPayload, replicaPayloadRecords, replicaRecords)))
    (hReplicaFile : checked_add replicaRecords input.replica_filemark_blocks =
      ok (.Ok replicaFile))
    (hTriple : checked_mul 3#u64 replicaFile = ok (.Ok triple))
    (hGap : index_separation_records input.block_size_bytes
      input.gap_nominal_bytes = ok (.Ok gapRecords))
    (hGapFile : checked_add gapRecords input.gap_filemark_blocks =
      ok (.Ok gapFile))
    (hDouble : checked_mul 2#u64 gapFile = ok (.Ok doubleGaps))
    (hPartialFile : projection.final_partial_sidecar_needed = true →
      final_partial_sidecar_tape_file_blocks input = ok (.Ok partialFile))
    (hMapBlocks : projection.final_parity_map_needed = true →
      replicated_control_total_blocks input.block_size_bytes 200#u64
        projection.final_parity_map_payload_bound_bytes = ok (.Ok mapBlocks))
    (hMapFile : projection.final_parity_map_needed = true →
      checked_add mapBlocks input.parity_map_filemark_blocks = ok (.Ok mapFile))
    (hParityClose : checked_add
      (if projection.final_partial_sidecar_needed then partialFile
       else 0#u64)
      (if projection.final_parity_map_needed then mapFile else 0#u64) =
      ok (.Ok parityClose))
    (hTail : checked_add triple doubleGaps = ok (.Ok tail))
    (hClose1 : checked_add parityClose tail = ok (.Ok close1))
    (hCloseBound : checked_add close1 input.safety_margin_blocks =
      ok (.Ok closeBound)) :
    compute_terminal_control_terms input sidecar projection = ok (.Ok {
      final_partial_sidecar_blocks :=
        if projection.final_partial_sidecar_needed then partialFile
        else 0#u64,
      final_parity_map_blocks_before_filemark :=
        if projection.final_parity_map_needed then mapBlocks else 0#u64,
      final_parity_map_tape_file_blocks :=
        if projection.final_parity_map_needed then mapFile else 0#u64,
      replica_payload_bytes := replicaPayload,
      replica_payload_record_count := replicaPayloadRecords,
      replica_records_before_filemark := replicaRecords,
      replica_tape_file_blocks := replicaFile,
      triple_replica_blocks := triple,
      gap_records_before_filemark := gapRecords,
      gap_tape_file_blocks := gapFile,
      double_gap_blocks := doubleGaps,
      parity_closeout_charge_blocks := parityClose,
      terminal_tail_charge_blocks := tail,
      close_bound_blocks := closeBound
    }) := by
  cases hPartial : projection.final_partial_sidecar_needed <;>
    cases hMap : projection.final_parity_map_needed
  all_goals
    have hParityClose' := hParityClose
    simp [hPartial, hMap] at hParityClose'
  · simp [compute_terminal_control_terms, hReplica, hReplicaFile, hTriple,
      hGap, hGapFile, hDouble, hPartial, hMap, hParityClose', hTail,
      hClose1, hCloseBound, core.result.Result.Insts.CoreOpsTry.branch]
  · have hMapBlocks' := hMapBlocks hMap
    have hMapFile' := hMapFile hMap
    simp [compute_terminal_control_terms, hReplica, hReplicaFile, hTriple,
      hGap, hGapFile, hDouble, hPartial, hMap, parity_map_header_bytes,
      hMapBlocks', hMapFile', hParityClose', hTail, hClose1, hCloseBound,
      core.result.Result.Insts.CoreOpsTry.branch]
  · simp [compute_terminal_control_terms, hReplica, hReplicaFile, hTriple,
      hGap, hGapFile, hDouble, hPartial, hMap, hPartialFile hPartial,
      hParityClose', hTail,
      hClose1, hCloseBound, core.result.Result.Insts.CoreOpsTry.branch]
  · have hMapBlocks' := hMapBlocks hMap
    have hMapFile' := hMapFile hMap
    have hPartialFile' := hPartialFile hPartial
    simp [compute_terminal_control_terms, hReplica, hReplicaFile, hTriple,
      hGap, hGapFile, hDouble, hPartial, hMap, parity_map_header_bytes,
      hMapBlocks', hMapFile', hPartialFile', hParityClose', hTail, hClose1, hCloseBound,
      core.result.Result.Insts.CoreOpsTry.branch]

/-- C2 exact capacity policy rejects a physical remainder above C. -/
theorem validate_terminal_close_rejects_remaining_above_c
    (input : TerminalTripleCloseInput)
    (h : input.remaining_tape_blocks.val > input.capacity_basis_blocks.val) :
    validate_terminal_close_input input =
      ok (.Err CapacityError.CapacityPolicyInvalid) := by
  simp [validate_terminal_close_input, h]

/-- C2 exact capacity policy requires L < H. -/
theorem validate_terminal_close_rejects_low_not_below_high
    (input : TerminalTripleCloseInput)
    (hRemaining : ¬ input.remaining_tape_blocks.val > input.capacity_basis_blocks.val)
    (h : input.low_watermark_blocks.val ≥ input.high_watermark_blocks.val) :
    validate_terminal_close_input input =
      ok (.Err CapacityError.CapacityPolicyInvalid) := by
  simp [validate_terminal_close_input, hRemaining, h]

/-- C2 exact capacity policy requires H <= C. -/
theorem validate_terminal_close_rejects_high_above_c
    (input : TerminalTripleCloseInput)
    (hRemaining : ¬ input.remaining_tape_blocks.val > input.capacity_basis_blocks.val)
    (hLowHigh : ¬ input.low_watermark_blocks.val ≥ input.high_watermark_blocks.val)
    (h : input.high_watermark_blocks.val > input.capacity_basis_blocks.val) :
    validate_terminal_close_input input =
      ok (.Err CapacityError.CapacityPolicyInvalid) := by
  simp [validate_terminal_close_input, hRemaining, hLowHigh, h]

/-- C2 structural safety: a terminal close is rejected when no committed BOT
    Bootstrap is represented, after all earlier input checks have succeeded. -/
theorem validate_terminal_close_rejects_missing_bot
    (input : TerminalTripleCloseInput) (neighborhood recoveryRows : Std.U64)
    (hRemaining : ¬ input.remaining_tape_blocks.val > input.capacity_basis_blocks.val)
    (hLowHigh : ¬ input.low_watermark_blocks.val ≥ input.high_watermark_blocks.val)
    (hHighC : ¬ input.high_watermark_blocks.val > input.capacity_basis_blocks.val)
    (hSupported : supported_terminal_block_size input.block_size_bytes = ok true)
    (hPresence : (input.projected_object_present !=
      (input.projected_object_blocks != 0#u64)) = false)
    (hGap : input.gap_nominal_bytes = 1073741824#u64)
    (hData : input.data_shards_per_epoch ≠ 0#u64)
    (hParity : input.parity_shards_per_epoch ≠ 0#u64)
    (hNeighborhood : checked_add input.data_shards_per_epoch
      input.parity_shards_per_epoch = ok (.Ok neighborhood))
    (hNeighborhoodSafe : ¬ neighborhood.val > 4294967295)
    (hEpoch : ¬ input.current_epoch_fill_blocks.val ≥
      input.data_shards_per_epoch.val)
    (hObjectRows : ¬ input.object_rows_before_object.val >
      input.structural_entries_before_object.val)
    (hSidecarRows : ¬ input.sidecar_entries_before_object.val >
      input.structural_entries_before_object.val)
    (hRecovery : checked_add input.object_rows_before_object
      input.sidecar_entries_before_object = ok (.Ok recoveryRows))
    (hMissing : input.structural_entries_before_object = 0#u64) :
    validate_terminal_close_input input =
      ok (.Err CapacityError.MissingBotBootstrap) := by
  have hStructuralZero : input.structural_entries_before_object.val = 0 := by
    simp [hMissing]
  have hObjectZero : input.object_rows_before_object.val = 0 := by omega
  have hSidecarZero : input.sidecar_entries_before_object.val = 0 := by omega
  have hRecoveryZero : recoveryRows.val = 0 := by
    rw [checked_add_result_value _ _ _ hRecovery, hObjectZero, hSidecarZero]
  simp [validate_terminal_close_input, hRemaining, hLowHigh, hHighC, hSupported,
    hPresence, hGap, hData, hParity,
    hNeighborhood, hNeighborhoodSafe, hEpoch, hObjectZero, hSidecarZero,
    hRecovery, hRecoveryZero, hMissing,
    core.result.Result.Insts.CoreOpsTry.branch]

/-- C2 manual-close contract: `projected_object_present = false` is legal only
    with zero projected Object blocks. A nonzero Object cannot be hidden from
    admission by selecting the manual-close branch. -/
theorem validate_manual_close_rejects_hidden_object
    (input : TerminalTripleCloseInput)
    (hRemaining : ¬ input.remaining_tape_blocks.val > input.capacity_basis_blocks.val)
    (hLowHigh : ¬ input.low_watermark_blocks.val ≥ input.high_watermark_blocks.val)
    (hHighC : ¬ input.high_watermark_blocks.val > input.capacity_basis_blocks.val)
    (hSupported : supported_terminal_block_size input.block_size_bytes = ok true)
    (hManual : input.projected_object_present = false)
    (hObject : input.projected_object_blocks ≠ 0#u64) :
    validate_terminal_close_input input =
      ok (.Err CapacityError.ProjectedObjectPresenceMismatch) := by
  simp [validate_terminal_close_input, hRemaining, hLowHigh, hHighC,
    hSupported, hManual, hObject]

/-- C1 input safety: every terminal separation extent must be exactly 1 GiB.
    Any other size is rejected before sidecar/profile arithmetic or motion. -/
theorem validate_terminal_close_rejects_wrong_gap_extent
    (input : TerminalTripleCloseInput)
    (hRemaining : ¬ input.remaining_tape_blocks.val > input.capacity_basis_blocks.val)
    (hLowHigh : ¬ input.low_watermark_blocks.val ≥ input.high_watermark_blocks.val)
    (hHighC : ¬ input.high_watermark_blocks.val > input.capacity_basis_blocks.val)
    (hSupported : supported_terminal_block_size input.block_size_bytes = ok true)
    (hPresence : (input.projected_object_present !=
      (input.projected_object_blocks != 0#u64)) = false)
    (hGap : input.gap_nominal_bytes ≠ 1073741824#u64) :
    validate_terminal_close_input input =
      ok (.Err CapacityError.GapExtentSizeMismatch) := by
  simp [validate_terminal_close_input, hRemaining, hLowHigh, hHighC,
    hSupported, hPresence, hGap]

/-- C2 manual projection: with no projected Object, the prefix charge contains
    only already-pending/newly-completed sidecars, the Object row count is
    unchanged, and structural growth begins with those sidecars rather than a
    synthetic Object entry. -/
theorem compute_terminal_projection_manual_close
    (input : TerminalTripleCloseInput) (sidecar : TerminalSidecarTerms)
    (maximum projectedFill epochs remainder sidecarsEmitted sidecarBlocks
      prefixCharge objectRows sidecarAfterCommit sidecarAfter directoryBound
      payloadBound structuralBase structuralCommit structuralPartial
      structuralFinal : Std.U64)
    (hParity : input.parity_shards_per_epoch ≠ 0#u64)
    (hManual : input.projected_object_present = false)
    (hProjected : checked_add input.current_epoch_fill_blocks
      input.projected_object_blocks = ok (.Ok projectedFill))
    (hEpochs : projectedFill / input.data_shards_per_epoch = ok epochs)
    (hRemainder : projectedFill % input.data_shards_per_epoch = ok remainder)
    (hSidecars : checked_add input.pending_completed_sidecars epochs =
      ok (.Ok sidecarsEmitted))
    (hSidecarBlocks : checked_mul sidecarsEmitted sidecar.tape_file_blocks =
      ok (.Ok sidecarBlocks))
    (hPrefix : checked_add 0#u64 sidecarBlocks = ok (.Ok prefixCharge))
    (hObjectRows : checked_add input.object_rows_before_object 0#u64 =
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
    (hStructuralBase : checked_add input.structural_entries_before_object 0#u64 =
      ok (.Ok structuralBase))
    (hStructuralCommit : checked_add structuralBase sidecarsEmitted =
      ok (.Ok structuralCommit))
    (hStructuralPartial : checked_add structuralCommit
      (if remainder = 0#u64 then 0#u64 else 1#u64) =
      ok (.Ok structuralPartial))
    (hStructuralFinal : checked_add structuralPartial
      (if sidecarAfter = 0#u64 then 0#u64 else 1#u64) =
      ok (.Ok structuralFinal)) :
    compute_terminal_projection_terms input sidecar maximum = ok (.Ok {
      epochs_completed_by_object := epochs,
      final_partial_sidecar_needed := remainder != 0#u64,
      sidecars_emitted_by_commit := sidecarsEmitted,
      sidecar_blocks_emitted_by_commit := sidecarBlocks,
      object_tape_file_blocks := 0#u64,
      prefix_commit_charge_blocks := prefixCharge,
      object_rows_after := objectRows,
      sidecar_entries_after_closeout := sidecarAfter,
      maximum_sidecar_entries_for_capacity := maximum,
      structural_entries_after_closeout := structuralFinal,
      final_parity_map_needed := sidecarAfter != 0#u64,
      final_parity_map_directory_bound_bytes := directoryBound,
      final_parity_map_payload_bound_bytes := payloadBound
    }) ∧
    objectRows.val = input.object_rows_before_object.val ∧
    prefixCharge.val = sidecarBlocks.val := by
  have hnMaximum : ¬ maximum.val < input.sidecar_entries_before_object.val := by
    omega
  have hObjectRowsVal := checked_add_result_value _ _ _ hObjectRows
  have hPrefixVal := checked_add_result_value _ _ _ hPrefix
  refine ⟨?_, by simpa using hObjectRowsVal, by simpa using hPrefixVal⟩
  unfold compute_terminal_projection_terms
  by_cases hRem : remainder = 0#u64 <;>
    by_cases hMap : sidecarAfter = 0#u64
  all_goals
    have hSidecarAfter' := hSidecarAfter
    have hStructuralPartial' := hStructuralPartial
    have hStructuralFinal' := hStructuralFinal
    have hDirectory' := hDirectory
    have hPayload' := hPayload
    simp [hRem] at hSidecarAfter' hStructuralPartial'
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
    simp [hParity, hManual, hProjected, hEpochs, hRemainder, hSidecars,
      hSidecarBlocks, hPrefix, hObjectRows, hSidecarAfterCommit, hRem,
      hRemVal, hSidecarAfter', hnMaximum, hDirectory', hPayload', hMap,
      hMapVal, hStructuralBase, hStructuralCommit, hStructuralPartial',
      hStructuralFinal', core.result.Result.Insts.CoreOpsTry.branch]

/-- C2 profile failures are collapsed to the fail-closed public profile error;
    projection, capacity gates, and spool gates are not reached. -/
theorem evaluate_terminal_close_maps_profile_failure
    (input : TerminalTripleCloseInput) (sidecar : TerminalSidecarTerms)
    (profileError : CapacityError)
    (hParity : input.parity_shards_per_epoch ≠ 0#u64)
    (hValidate : validate_terminal_close_input input = ok (.Ok ()))
    (hSidecar : compute_terminal_sidecar_terms input = ok (.Ok sidecar))
    (hProfile : validate_capacity_derived_profile_bounds input
      (input.parity_shards_per_epoch != 0#u64) sidecar.tape_file_blocks =
      ok (.Err profileError)) :
    evaluate_terminal_close input =
      ok (.Err CapacityError.UnsafeCapacityProfile) := by
  simp [evaluate_terminal_close, hParity, hValidate, hSidecar, hProfile,
    core.result.Result.Insts.CoreOpsTry.branch]

/-- C2 arithmetic failure while combining the prefix commit and close bound
    fails closed before any tape or spool comparison. -/
theorem evaluate_terminal_close_rejects_required_tape_overflow
    (input : TerminalTripleCloseInput) (sidecar : TerminalSidecarTerms)
    (projection : TerminalProjectionTerms) (control : TerminalControlTerms)
    (maximum : Std.U64)
    (hParity : input.parity_shards_per_epoch ≠ 0#u64)
    (hValidate : validate_terminal_close_input input = ok (.Ok ()))
    (hSidecar : compute_terminal_sidecar_terms input = ok (.Ok sidecar))
    (hProfile : validate_capacity_derived_profile_bounds input
      (input.parity_shards_per_epoch != 0#u64) sidecar.tape_file_blocks =
      ok (.Ok maximum))
    (hProjection : compute_terminal_projection_terms input sidecar maximum =
      ok (.Ok projection))
    (hControl : compute_terminal_control_terms input sidecar projection =
      ok (.Ok control))
    (hOverflow : checked_add projection.prefix_commit_charge_blocks
      control.close_bound_blocks = ok (.Err CapacityError.ArithmeticOverflow)) :
    evaluate_terminal_close input =
      ok (.Err CapacityError.ArithmeticOverflow) := by
  simp [evaluate_terminal_close, hParity, hValidate, hSidecar, hProfile, hProjection,
    hControl, hOverflow, core.result.Result.Insts.CoreOpsTry.branch,
    core.result.Result.Insts.CoreOpsTryTraitFromResidualResultInfallible.from_residual]

/-- C2 checked multiplication of encoded sidecar records by block size also
    propagates arithmetic overflow rather than producing a capacity result. -/
theorem evaluate_terminal_close_rejects_sidecar_byte_overflow
    (input : TerminalTripleCloseInput) (sidecar : TerminalSidecarTerms)
    (projection : TerminalProjectionTerms) (control : TerminalControlTerms)
    (maximum required : Std.U64)
    (hParity : input.parity_shards_per_epoch ≠ 0#u64)
    (hValidate : validate_terminal_close_input input = ok (.Ok ()))
    (hSidecar : compute_terminal_sidecar_terms input = ok (.Ok sidecar))
    (hProfile : validate_capacity_derived_profile_bounds input
      (input.parity_shards_per_epoch != 0#u64) sidecar.tape_file_blocks =
      ok (.Ok maximum))
    (hProjection : compute_terminal_projection_terms input sidecar maximum =
      ok (.Ok projection))
    (hControl : compute_terminal_control_terms input sidecar projection =
      ok (.Ok control))
    (hRequired : checked_add projection.prefix_commit_charge_blocks
      control.close_bound_blocks = ok (.Ok required))
    (hAutomatic : input.projected_object_present = true)
    (hTapeFits : required.val ≤ input.remaining_tape_blocks.val)
    (hOverflow : checked_mul sidecar.blocks_before_filemark
      input.block_size_bytes = ok (.Err CapacityError.ArithmeticOverflow)) :
    evaluate_terminal_close input =
      ok (.Err CapacityError.ArithmeticOverflow) := by
  have hnTape : ¬ input.remaining_tape_blocks.val < required.val := by omega
  simp [evaluate_terminal_close, hParity, hValidate, hSidecar, hProfile, hProjection,
    hControl, hRequired, hAutomatic, hnTape, hOverflow,
    core.result.Result.Insts.CoreOpsTry.branch,
    core.result.Result.Insts.CoreOpsTryTraitFromResidualResultInfallible.from_residual]

/-- C2 checked multiplication of completed epochs by encoded sidecar bytes
    propagates overflow after the tape gates have succeeded. -/
theorem evaluate_terminal_close_rejects_completed_spool_overflow
    (input : TerminalTripleCloseInput) (sidecar : TerminalSidecarTerms)
    (projection : TerminalProjectionTerms) (control : TerminalControlTerms)
    (maximum required sidecarBytes : Std.U64)
    (hParity : input.parity_shards_per_epoch ≠ 0#u64)
    (hValidate : validate_terminal_close_input input = ok (.Ok ()))
    (hSidecar : compute_terminal_sidecar_terms input = ok (.Ok sidecar))
    (hProfile : validate_capacity_derived_profile_bounds input
      (input.parity_shards_per_epoch != 0#u64) sidecar.tape_file_blocks =
      ok (.Ok maximum))
    (hProjection : compute_terminal_projection_terms input sidecar maximum =
      ok (.Ok projection))
    (hControl : compute_terminal_control_terms input sidecar projection =
      ok (.Ok control))
    (hRequired : checked_add projection.prefix_commit_charge_blocks
      control.close_bound_blocks = ok (.Ok required))
    (hAutomatic : input.projected_object_present = true)
    (hTapeFits : required.val ≤ input.remaining_tape_blocks.val)
    (hSidecarBytes : checked_mul sidecar.blocks_before_filemark
      input.block_size_bytes = ok (.Ok sidecarBytes))
    (hOverflow : checked_mul projection.epochs_completed_by_object sidecarBytes =
      ok (.Err CapacityError.ArithmeticOverflow)) :
    evaluate_terminal_close input =
      ok (.Err CapacityError.ArithmeticOverflow) := by
  have hnTape : ¬ input.remaining_tape_blocks.val < required.val := by omega
  simp [evaluate_terminal_close, hParity, hValidate, hSidecar, hProfile, hProjection,
    hControl, hRequired, hAutomatic, hnTape, hSidecarBytes, hOverflow,
    core.result.Result.Insts.CoreOpsTry.branch,
    core.result.Result.Insts.CoreOpsTryTraitFromResidualResultInfallible.from_residual]

/-- C2 adding pending parity bytes to newly completed sidecar bytes is also a
    checked fail-closed operation. -/
theorem evaluate_terminal_close_rejects_required_spool_overflow
    (input : TerminalTripleCloseInput) (sidecar : TerminalSidecarTerms)
    (projection : TerminalProjectionTerms) (control : TerminalControlTerms)
    (maximum required sidecarBytes newSidecarBytes : Std.U64)
    (hParity : input.parity_shards_per_epoch ≠ 0#u64)
    (hValidate : validate_terminal_close_input input = ok (.Ok ()))
    (hSidecar : compute_terminal_sidecar_terms input = ok (.Ok sidecar))
    (hProfile : validate_capacity_derived_profile_bounds input
      (input.parity_shards_per_epoch != 0#u64) sidecar.tape_file_blocks =
      ok (.Ok maximum))
    (hProjection : compute_terminal_projection_terms input sidecar maximum =
      ok (.Ok projection))
    (hControl : compute_terminal_control_terms input sidecar projection =
      ok (.Ok control))
    (hRequired : checked_add projection.prefix_commit_charge_blocks
      control.close_bound_blocks = ok (.Ok required))
    (hAutomatic : input.projected_object_present = true)
    (hTapeFits : required.val ≤ input.remaining_tape_blocks.val)
    (hSidecarBytes : checked_mul sidecar.blocks_before_filemark
      input.block_size_bytes = ok (.Ok sidecarBytes))
    (hNewSidecarBytes : checked_mul projection.epochs_completed_by_object
      sidecarBytes = ok (.Ok newSidecarBytes))
    (hOverflow : checked_add input.pending_completed_epoch_parity_bytes
      newSidecarBytes = ok (.Err CapacityError.ArithmeticOverflow)) :
    evaluate_terminal_close input =
      ok (.Err CapacityError.ArithmeticOverflow) := by
  have hnTape : ¬ input.remaining_tape_blocks.val < required.val := by omega
  simp [evaluate_terminal_close, hParity, hValidate, hSidecar, hProfile, hProjection,
    hControl, hRequired, hAutomatic, hnTape, hSidecarBytes,
    hNewSidecarBytes, hOverflow, core.result.Result.Insts.CoreOpsTry.branch,
    core.result.Result.Insts.CoreOpsTryTraitFromResidualResultInfallible.from_residual]

/-- C2 automatic admission ordering: current-tape shortfall is the tape-capacity
    result; fresh-media impossibility is classified by orchestration using the
    same report rather than a second calculator. -/
theorem evaluate_terminal_close_automatic_tape_gate
    (input : TerminalTripleCloseInput) (sidecar : TerminalSidecarTerms)
    (projection : TerminalProjectionTerms) (control : TerminalControlTerms)
    (maximum required : Std.U64)
    (hParity : input.parity_shards_per_epoch ≠ 0#u64)
    (hValidate : validate_terminal_close_input input = ok (.Ok ()))
    (hSidecar : compute_terminal_sidecar_terms input = ok (.Ok sidecar))
    (hProfile : validate_capacity_derived_profile_bounds input
      (input.parity_shards_per_epoch != 0#u64) sidecar.tape_file_blocks =
      ok (.Ok maximum))
    (hProjection : compute_terminal_projection_terms input sidecar maximum =
      ok (.Ok projection))
    (hControl : compute_terminal_control_terms input sidecar projection =
      ok (.Ok control))
    (hRequired : checked_add projection.prefix_commit_charge_blocks
      control.close_bound_blocks = ok (.Ok required))
    (hAutomatic : input.projected_object_present = true)
    (hTapeShort : input.remaining_tape_blocks.val < required.val) :
    evaluate_terminal_close input =
      ok (.Err CapacityError.CapacityReserveExceededTape) := by
  simp [evaluate_terminal_close, hParity, hValidate, hSidecar, hProfile, hProjection,
    hControl, hRequired, hAutomatic, hTapeShort,
    core.result.Result.Insts.CoreOpsTry.branch]

/-- C2 manual close has no Object-specific empty-media gate. With
    `projected_object_present = false`, the same checked close bound proceeds
    directly to the current-tape comparison. -/
theorem evaluate_terminal_close_manual_tape_gate
    (input : TerminalTripleCloseInput) (sidecar : TerminalSidecarTerms)
    (projection : TerminalProjectionTerms) (control : TerminalControlTerms)
    (maximum required : Std.U64)
    (hParity : input.parity_shards_per_epoch ≠ 0#u64)
    (hValidate : validate_terminal_close_input input = ok (.Ok ()))
    (hSidecar : compute_terminal_sidecar_terms input = ok (.Ok sidecar))
    (hProfile : validate_capacity_derived_profile_bounds input
      (input.parity_shards_per_epoch != 0#u64) sidecar.tape_file_blocks =
      ok (.Ok maximum))
    (hProjection : compute_terminal_projection_terms input sidecar maximum =
      ok (.Ok projection))
    (hControl : compute_terminal_control_terms input sidecar projection =
      ok (.Ok control))
    (hRequired : checked_add projection.prefix_commit_charge_blocks
      control.close_bound_blocks = ok (.Ok required))
    (hManual : input.projected_object_present = false)
    (hTapeShort : input.remaining_tape_blocks.val < required.val) :
    evaluate_terminal_close input =
      ok (.Err CapacityError.CapacityReserveExceededTape) := by
  simp [evaluate_terminal_close, hParity, hValidate, hSidecar, hProfile, hProjection,
    hControl, hRequired, hManual, hTapeShort,
    core.result.Result.Insts.CoreOpsTry.branch]

/-- C2 spool failure is checked only after the automatic tape gate passes. -/
theorem evaluate_terminal_close_automatic_spool_gate
    (input : TerminalTripleCloseInput) (sidecar : TerminalSidecarTerms)
    (projection : TerminalProjectionTerms) (control : TerminalControlTerms)
    (maximum required sidecarBytes newSidecarBytes requiredSpool : Std.U64)
    (hParity : input.parity_shards_per_epoch ≠ 0#u64)
    (hValidate : validate_terminal_close_input input = ok (.Ok ()))
    (hSidecar : compute_terminal_sidecar_terms input = ok (.Ok sidecar))
    (hProfile : validate_capacity_derived_profile_bounds input
      (input.parity_shards_per_epoch != 0#u64) sidecar.tape_file_blocks =
      ok (.Ok maximum))
    (hProjection : compute_terminal_projection_terms input sidecar maximum =
      ok (.Ok projection))
    (hControl : compute_terminal_control_terms input sidecar projection =
      ok (.Ok control))
    (hRequired : checked_add projection.prefix_commit_charge_blocks
      control.close_bound_blocks = ok (.Ok required))
    (hAutomatic : input.projected_object_present = true)
    (hTapeFits : required.val ≤ input.remaining_tape_blocks.val)
    (hSidecarBytes : checked_mul sidecar.blocks_before_filemark
      input.block_size_bytes = ok (.Ok sidecarBytes))
    (hNewSidecarBytes : checked_mul projection.epochs_completed_by_object
      sidecarBytes = ok (.Ok newSidecarBytes))
    (hSpool : checked_add input.pending_completed_epoch_parity_bytes
      newSidecarBytes = ok (.Ok requiredSpool))
    (hSpoolShort : input.remaining_spool_bytes.val < requiredSpool.val) :
    evaluate_terminal_close input =
      ok (.Err CapacityError.CapacityReserveExceededSpool) := by
  have hnTape : ¬ input.remaining_tape_blocks.val < required.val := by omega
  simp [evaluate_terminal_close, hParity, hValidate, hSidecar, hProfile, hProjection,
    hControl, hRequired, hAutomatic, hnTape, hSidecarBytes,
    hNewSidecarBytes, hSpool, hSpoolShort,
    core.result.Result.Insts.CoreOpsTry.branch]

/-- C2 manual close retains the same spool gate after its current-tape
    bound succeeds; suppressing the projected Object suppresses no parity debt. -/
theorem evaluate_terminal_close_manual_spool_gate
    (input : TerminalTripleCloseInput) (sidecar : TerminalSidecarTerms)
    (projection : TerminalProjectionTerms) (control : TerminalControlTerms)
    (maximum required sidecarBytes newSidecarBytes requiredSpool : Std.U64)
    (hParity : input.parity_shards_per_epoch ≠ 0#u64)
    (hValidate : validate_terminal_close_input input = ok (.Ok ()))
    (hSidecar : compute_terminal_sidecar_terms input = ok (.Ok sidecar))
    (hProfile : validate_capacity_derived_profile_bounds input
      (input.parity_shards_per_epoch != 0#u64) sidecar.tape_file_blocks =
      ok (.Ok maximum))
    (hProjection : compute_terminal_projection_terms input sidecar maximum =
      ok (.Ok projection))
    (hControl : compute_terminal_control_terms input sidecar projection =
      ok (.Ok control))
    (hRequired : checked_add projection.prefix_commit_charge_blocks
      control.close_bound_blocks = ok (.Ok required))
    (hManual : input.projected_object_present = false)
    (hTapeFits : required.val ≤ input.remaining_tape_blocks.val)
    (hSidecarBytes : checked_mul sidecar.blocks_before_filemark
      input.block_size_bytes = ok (.Ok sidecarBytes))
    (hNewSidecarBytes : checked_mul projection.epochs_completed_by_object
      sidecarBytes = ok (.Ok newSidecarBytes))
    (hSpool : checked_add input.pending_completed_epoch_parity_bytes
      newSidecarBytes = ok (.Ok requiredSpool))
    (hSpoolShort : input.remaining_spool_bytes.val < requiredSpool.val) :
    evaluate_terminal_close input =
      ok (.Err CapacityError.CapacityReserveExceededSpool) := by
  have hnTape : ¬ input.remaining_tape_blocks.val < required.val := by omega
  simp [evaluate_terminal_close, hParity, hValidate, hSidecar, hProfile, hProjection,
    hControl, hRequired, hManual, hnTape, hSidecarBytes, hNewSidecarBytes,
    hSpool, hSpoolShort, core.result.Result.Insts.CoreOpsTry.branch]

/- Terminal authority and survivor-selection invariants (SPEC.md C3-C4). -/

/-- C3: without a successful component barrier, durable progress is unchanged. -/
theorem failed_barrier_preserves_terminal_progress
    (progress : TerminalTailProgress) :
    advance_terminal_progress progress false = ok progress := by
  simp [advance_terminal_progress]

/-- C3: a successful component transition never decreases the projected
    number of complete replicas. Gap transitions deliberately keep it equal. -/
theorem terminal_replica_projection_monotone
    (progress next : TerminalTailProgress) (before after : Std.U64)
    (hNext : advance_terminal_progress progress true = ok next)
    (hBefore : completed_terminal_replicas progress = ok before)
    (hAfter : completed_terminal_replicas next = ok after) :
    before.val ≤ after.val := by
  cases progress <;>
    simp [advance_terminal_progress] at hNext <;>
    subst next <;>
    simp [completed_terminal_replicas] at hBefore hAfter <;>
    subst before <;>
    subst after <;>
    norm_num

/-- C3: entering Finalizing permanently excludes later Object admission. -/
theorem no_object_admission_after_finalizing :
    object_admission_allowed true = ok false := by
  simp [object_admission_allowed]

/-- C3: ordinary sealed projection implies that replica C's barrier-proved
    state has been reached. -/
theorem sealed_implies_after_replica_c (progress : TerminalTailProgress)
    (hSealed : sealed_projection_allowed progress = ok true) :
    progress = TerminalTailProgress.AfterReplicaC := by
  cases progress <;>
    simp [sealed_projection_allowed] at hSealed ⊢

/-- C4: with agreeing survivors, selection is newest-first. -/
theorem terminal_replica_selection_prefers_c :
    (match select_terminal_replica true true true true with
      | ok TerminalReplicaSelection.ReplicaC => true
      | _ => false) = true := by
  native_decide

theorem terminal_replica_selection_prefers_b_over_a :
    (match select_terminal_replica true true false true with
      | ok TerminalReplicaSelection.ReplicaB => true
      | _ => false) = true := by
  native_decide

theorem terminal_replica_selection_uses_a_alone :
    (match select_terminal_replica true false false false with
      | ok TerminalReplicaSelection.ReplicaA => true
      | _ => false) = true := by
  native_decide

/-- C4: no valid terminal member requests the explicit BOT structural scan. -/
theorem no_terminal_replica_requires_full_bot_scan :
    (match select_terminal_replica false false false false with
      | ok TerminalReplicaSelection.FullBotScan => true
      | _ => false) = true := by
  native_decide

/-- C4: any disagreeing pair fails closed instead of silently choosing one. -/
theorem disagreeing_terminal_survivors_conflict :
    (match select_terminal_replica true true false false with
      | ok TerminalReplicaSelection.Conflict => true
      | _ => false) = true ∧
    (match select_terminal_replica true false true false with
      | ok TerminalReplicaSelection.Conflict => true
      | _ => false) = true ∧
    (match select_terminal_replica false true true false with
      | ok TerminalReplicaSelection.Conflict => true
      | _ => false) = true := by
  native_decide

end parity_capacity_verif
