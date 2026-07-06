/- Specification theorems for the parity-sidecar-layout extraction (SPEC.md S1-S5).

   Targets the Aeneas-generated definitions in `ParitySidecarLayout.Funs`.
   These theorems certify the proof-facing scalar layout and CRC-window
   arithmetic for sidecar header blocks, footer locator blocks, spill index
   blocks, and full sidecar tape-file block placement. They do not prove HMAC,
   SHA-256, CRC-64/XZ algebra, production slice copying, Vec allocation, tape
   IO, or Reed-Solomon recovery. The Rust drift guard ties this extraction back
   to `crates/remanence-parity/src/sidecar.rs`. -/
import ParitySidecarLayout.Funs

open Aeneas Aeneas.Std Result

set_option linter.unusedVariables false

namespace parity_sidecar_layout_verif

def byteRangeSpec (start end1 : Std.U64) : ByteRange :=
  { start, «end» := end1 }

def headerLayoutSpec (blockSize crcStart : Std.U64) : HeaderBlockLayout := {
  magic := byteRangeSpec 0#u64 8#u64,
  tape_uuid := byteRangeSpec 8#u64 24#u64,
  epoch_id := byteRangeSpec 24#u64 32#u64,
  k := byteRangeSpec 32#u64 34#u64,
  m := byteRangeSpec 34#u64 36#u64,
  stripes_per_epoch := byteRangeSpec 36#u64 40#u64,
  block_size := byteRangeSpec 40#u64 44#u64,
  schema_version := byteRangeSpec 44#u64 48#u64,
  protected_ordinal_start := byteRangeSpec 48#u64 56#u64,
  protected_ordinal_end_exclusive := byteRangeSpec 56#u64 64#u64,
  logical_shard_count := byteRangeSpec 64#u64 72#u64,
  real_data_shard_count := byteRangeSpec 72#u64 80#u64,
  parity_block_count := byteRangeSpec 80#u64 84#u64,
  data_crc_count := byteRangeSpec 84#u64 88#u64,
  shard_index_block_count := byteRangeSpec 88#u64 92#u64,
  inline_index_entry_bytes := byteRangeSpec 92#u64 96#u64,
  sidecar_total_block_count := byteRangeSpec 96#u64 104#u64,
  primary_header_start_block := byteRangeSpec 104#u64 112#u64,
  tail_header_start_block := byteRangeSpec 112#u64 120#u64,
  footer_block_index := byteRangeSpec 120#u64 128#u64,
  copy_kind := byteRangeSpec 128#u64 130#u64,
  copy_kind_reserved := byteRangeSpec 130#u64 132#u64,
  copy_generation := byteRangeSpec 132#u64 136#u64,
  canonical_metadata_hash := byteRangeSpec 136#u64 168#u64,
  header_reserved := byteRangeSpec 168#u64 SIDECAR_HEADER_CRC_OFFSET,
  header_crc_field := byteRangeSpec SIDECAR_HEADER_CRC_OFFSET SIDECAR_HEADER_LEN,
  inline_index_payload := byteRangeSpec SIDECAR_HEADER_LEN crcStart,
  header_crc_input := byteRangeSpec 0#u64 SIDECAR_HEADER_CRC_OFFSET,
  block0_crc_input := byteRangeSpec 0#u64 crcStart,
  block0_crc_field := byteRangeSpec crcStart blockSize
}

def footerLayoutSpec (blockSize : Std.U64) : FooterBlockLayout := {
  magic := byteRangeSpec 0#u64 8#u64,
  sidecar_footer_version := byteRangeSpec 8#u64 10#u64,
  reserved16 := byteRangeSpec 10#u64 12#u64,
  reserved32 := byteRangeSpec 12#u64 16#u64,
  tape_uuid := byteRangeSpec 16#u64 32#u64,
  epoch_id := byteRangeSpec 32#u64 40#u64,
  protected_ordinal_start := byteRangeSpec 40#u64 48#u64,
  protected_ordinal_end_exclusive := byteRangeSpec 48#u64 56#u64,
  sidecar_header_block_count := byteRangeSpec 56#u64 60#u64,
  parity_shard_block_count := byteRangeSpec 60#u64 64#u64,
  sidecar_total_block_count := byteRangeSpec 64#u64 72#u64,
  primary_header_start_block := byteRangeSpec 72#u64 80#u64,
  tail_header_start_block := byteRangeSpec 80#u64 88#u64,
  canonical_metadata_hash := byteRangeSpec 88#u64 SIDECAR_FOOTER_CRC_OFFSET,
  footer_crc_field := byteRangeSpec SIDECAR_FOOTER_CRC_OFFSET SIDECAR_FOOTER_LEN,
  footer_crc_input := byteRangeSpec 0#u64 SIDECAR_FOOTER_CRC_OFFSET,
  footer_padding := byteRangeSpec SIDECAR_FOOTER_LEN blockSize
}

def spillLayoutSpec (blockSize crcStart : Std.U64) : SpillBlockLayout := {
  index_payload := byteRangeSpec 0#u64 crcStart,
  trailing_crc_input := byteRangeSpec 0#u64 crcStart,
  trailing_crc_field := byteRangeSpec crcStart blockSize
}

def sidecarTapeLayoutSpec (h _p tailStart footerIndex total : Std.U64) :
    SidecarTapeFileLayout := {
  primary_header_copy := byteRangeSpec 0#u64 h,
  parity_shards := byteRangeSpec h tailStart,
  tail_header_copy := byteRangeSpec tailStart footerIndex,
  footer_block_index := footerIndex,
  sidecar_total_block_count := total
}

def rangeWithin (r : ByteRange) (limit : Std.U64) : Prop :=
  r.start.val ≤ r.«end».val ∧ r.«end».val ≤ limit.val

def headerLayoutWithin (layout : HeaderBlockLayout) (blockSize : Std.U64) : Prop :=
  rangeWithin layout.magic blockSize ∧
  rangeWithin layout.tape_uuid blockSize ∧
  rangeWithin layout.epoch_id blockSize ∧
  rangeWithin layout.k blockSize ∧
  rangeWithin layout.m blockSize ∧
  rangeWithin layout.stripes_per_epoch blockSize ∧
  rangeWithin layout.block_size blockSize ∧
  rangeWithin layout.schema_version blockSize ∧
  rangeWithin layout.protected_ordinal_start blockSize ∧
  rangeWithin layout.protected_ordinal_end_exclusive blockSize ∧
  rangeWithin layout.logical_shard_count blockSize ∧
  rangeWithin layout.real_data_shard_count blockSize ∧
  rangeWithin layout.parity_block_count blockSize ∧
  rangeWithin layout.data_crc_count blockSize ∧
  rangeWithin layout.shard_index_block_count blockSize ∧
  rangeWithin layout.inline_index_entry_bytes blockSize ∧
  rangeWithin layout.sidecar_total_block_count blockSize ∧
  rangeWithin layout.primary_header_start_block blockSize ∧
  rangeWithin layout.tail_header_start_block blockSize ∧
  rangeWithin layout.footer_block_index blockSize ∧
  rangeWithin layout.copy_kind blockSize ∧
  rangeWithin layout.copy_kind_reserved blockSize ∧
  rangeWithin layout.copy_generation blockSize ∧
  rangeWithin layout.canonical_metadata_hash blockSize ∧
  rangeWithin layout.header_reserved blockSize ∧
  rangeWithin layout.header_crc_field blockSize ∧
  rangeWithin layout.inline_index_payload blockSize ∧
  rangeWithin layout.header_crc_input blockSize ∧
  rangeWithin layout.block0_crc_input blockSize ∧
  rangeWithin layout.block0_crc_field blockSize

def footerLayoutWithin (layout : FooterBlockLayout) (blockSize : Std.U64) : Prop :=
  rangeWithin layout.magic blockSize ∧
  rangeWithin layout.sidecar_footer_version blockSize ∧
  rangeWithin layout.reserved16 blockSize ∧
  rangeWithin layout.reserved32 blockSize ∧
  rangeWithin layout.tape_uuid blockSize ∧
  rangeWithin layout.epoch_id blockSize ∧
  rangeWithin layout.protected_ordinal_start blockSize ∧
  rangeWithin layout.protected_ordinal_end_exclusive blockSize ∧
  rangeWithin layout.sidecar_header_block_count blockSize ∧
  rangeWithin layout.parity_shard_block_count blockSize ∧
  rangeWithin layout.sidecar_total_block_count blockSize ∧
  rangeWithin layout.primary_header_start_block blockSize ∧
  rangeWithin layout.tail_header_start_block blockSize ∧
  rangeWithin layout.canonical_metadata_hash blockSize ∧
  rangeWithin layout.footer_crc_field blockSize ∧
  rangeWithin layout.footer_crc_input blockSize ∧
  rangeWithin layout.footer_padding blockSize

def spillLayoutWithin (layout : SpillBlockLayout) (blockSize : Std.U64) : Prop :=
  rangeWithin layout.index_payload blockSize ∧
  rangeWithin layout.trailing_crc_input blockSize ∧
  rangeWithin layout.trailing_crc_field blockSize

def sidecarTapeLayoutWithin (layout : SidecarTapeFileLayout) (total : Std.U64) : Prop :=
  rangeWithin layout.primary_header_copy total ∧
  rangeWithin layout.parity_shards total ∧
  rangeWithin layout.tail_header_copy total ∧
  layout.footer_block_index.val < total.val ∧
  layout.footer_block_index.val + 1 = total.val ∧
  layout.sidecar_total_block_count = total

lemma headerLayoutSpec_within (blockSize crcStart : Std.U64)
    (hBlock : 192 ≤ blockSize.val)
    (hCrcStart : crcStart.val = blockSize.val - 8) :
    headerLayoutWithin (headerLayoutSpec blockSize crcStart) blockSize := by
  unfold headerLayoutWithin headerLayoutSpec rangeWithin byteRangeSpec
  unfold SIDECAR_HEADER_CRC_OFFSET SIDECAR_HEADER_LEN
  simp
  omega

lemma footerLayoutSpec_within (blockSize : Std.U64)
    (hBlock : 128 ≤ blockSize.val) :
    footerLayoutWithin (footerLayoutSpec blockSize) blockSize := by
  unfold footerLayoutWithin footerLayoutSpec rangeWithin byteRangeSpec
  unfold SIDECAR_FOOTER_CRC_OFFSET SIDECAR_FOOTER_LEN
  simp
  omega

lemma spillLayoutSpec_within (blockSize crcStart : Std.U64)
    (hBlock : 8 ≤ blockSize.val)
    (hCrcStart : crcStart.val = blockSize.val - 8) :
    spillLayoutWithin (spillLayoutSpec blockSize crcStart) blockSize := by
  unfold spillLayoutWithin spillLayoutSpec rangeWithin byteRangeSpec
  simp
  omega

lemma sidecarTapeLayoutSpec_within
    (h p tailStart footerIndex total : Std.U64)
    (hPositive : 0 < h.val)
    (hTailVal : tailStart.val = h.val + p.val)
    (hFooterVal : footerIndex.val = h.val + p.val + h.val)
    (hTotalVal : total.val = h.val + p.val + h.val + 1) :
    sidecarTapeLayoutWithin
      (sidecarTapeLayoutSpec h p tailStart footerIndex total) total := by
  unfold sidecarTapeLayoutWithin sidecarTapeLayoutSpec rangeWithin byteRangeSpec
  simp [hTailVal, hFooterVal, hTotalVal]
  omega

lemma checked_add_ok (a b : Std.U64) (h : a.val + b.val < 2 ^ 64) :
    ∃ sum, checked_add a b = ok (.Ok sum) ∧ sum.val = a.val + b.val := by
  have hspec := U64.checked_add_bv_spec a b
  cases hadd : U64.checked_add a b with
  | none =>
      simp [hadd, U64.max, U64.numBits] at hspec
      omega
  | some sum =>
      refine ⟨sum, ?_, ?_⟩
      · unfold checked_add
        simp [hadd, lift]
      · simp [hadd, U64.max, U64.numBits] at hspec
        exact hspec.2.1

lemma checked_sub_ok (a b : Std.U64) (h : b.val ≤ a.val) :
    ∃ diff, checked_sub a b = ok (.Ok diff) ∧ diff.val = a.val - b.val := by
  have hspec := U64.checked_sub_bv_spec a b
  cases hsub : U64.checked_sub a b with
  | none =>
      simp [hsub] at hspec
      omega
  | some diff =>
      refine ⟨diff, ?_, ?_⟩
      · unfold checked_sub
        simp [hsub, lift]
      · simp [hsub] at hspec
        exact hspec.2.1

theorem header_block_layout_success (blockSize : Std.U64)
    (hBlock : 192 ≤ blockSize.val) :
    ∃ crcStart layout,
      header_block_layout blockSize = ok (.Ok layout) ∧
      crcStart.val = blockSize.val - 8 ∧
      layout = headerLayoutSpec blockSize crcStart := by
  have hNotLt : ¬ blockSize < MIN_HEADER_BLOCK_SIZE := by
    unfold MIN_HEADER_BLOCK_SIZE
    scalar_tac
  rcases checked_sub_ok blockSize TRAILING_CRC_LEN (by
      unfold TRAILING_CRC_LEN
      scalar_tac) with ⟨crcStart, hSub, hCrcStart⟩
  refine ⟨crcStart, headerLayoutSpec blockSize crcStart, ?_, ?_, rfl⟩
  · unfold header_block_layout headerLayoutSpec byteRangeSpec range
    simp [hNotLt, hSub, core.result.Result.Insts.CoreOpsTry.branch]
  · unfold TRAILING_CRC_LEN at hCrcStart
    simpa using hCrcStart

theorem header_block_layout_rejects_small (blockSize : Std.U64)
    (hSmall : blockSize.val < 192) :
    header_block_layout blockSize = ok (.Err LayoutError.BlockTooSmall) := by
  have hLt : blockSize < MIN_HEADER_BLOCK_SIZE := by
    unfold MIN_HEADER_BLOCK_SIZE
    scalar_tac
  unfold header_block_layout
  simp [hLt]

theorem header_block_layout_ranges_within (blockSize : Std.U64)
    (hBlock : 192 ≤ blockSize.val) :
    ∃ (crcStart : Std.U64) (layout : HeaderBlockLayout),
      header_block_layout blockSize = ok (.Ok layout) ∧
      headerLayoutWithin layout blockSize ∧
      crcStart.val = blockSize.val - 8 := by
  rcases header_block_layout_success blockSize hBlock with
    ⟨crcStart, layout, hOk, hCrcStart, hLayout⟩
  refine ⟨crcStart, layout, hOk, ?_, hCrcStart⟩
  rw [hLayout]
  exact headerLayoutSpec_within blockSize crcStart hBlock hCrcStart

theorem footer_block_layout_success (blockSize : Std.U64)
    (hBlock : 128 ≤ blockSize.val) :
    footer_block_layout blockSize = ok (.Ok (footerLayoutSpec blockSize)) := by
  have hNotLt : ¬ blockSize < SIDECAR_FOOTER_LEN := by
    unfold SIDECAR_FOOTER_LEN
    scalar_tac
  unfold footer_block_layout footerLayoutSpec byteRangeSpec range
  simp [hNotLt]

theorem footer_block_layout_rejects_small (blockSize : Std.U64)
    (hSmall : blockSize.val < 128) :
    footer_block_layout blockSize = ok (.Err LayoutError.BlockTooSmall) := by
  have hLt : blockSize < SIDECAR_FOOTER_LEN := by
    unfold SIDECAR_FOOTER_LEN
    scalar_tac
  unfold footer_block_layout
  simp [hLt]

theorem footer_block_layout_ranges_within (blockSize : Std.U64)
    (hBlock : 128 ≤ blockSize.val) :
    footer_block_layout blockSize = ok (.Ok (footerLayoutSpec blockSize)) ∧
      footerLayoutWithin (footerLayoutSpec blockSize) blockSize := by
  exact ⟨footer_block_layout_success blockSize hBlock,
    footerLayoutSpec_within blockSize hBlock⟩

theorem spill_block_layout_success (blockSize : Std.U64)
    (hBlock : 8 ≤ blockSize.val) :
    ∃ crcStart layout,
      spill_block_layout blockSize = ok (.Ok layout) ∧
      crcStart.val = blockSize.val - 8 ∧
      layout = spillLayoutSpec blockSize crcStart := by
  have hNotLt : ¬ blockSize < TRAILING_CRC_LEN := by
    unfold TRAILING_CRC_LEN
    scalar_tac
  rcases checked_sub_ok blockSize TRAILING_CRC_LEN (by
      unfold TRAILING_CRC_LEN
      scalar_tac) with ⟨crcStart, hSub, hCrcStart⟩
  refine ⟨crcStart, spillLayoutSpec blockSize crcStart, ?_, ?_, rfl⟩
  · unfold spill_block_layout spillLayoutSpec byteRangeSpec range
    simp [hNotLt, hSub, core.result.Result.Insts.CoreOpsTry.branch]
  · unfold TRAILING_CRC_LEN at hCrcStart
    simpa using hCrcStart

theorem spill_block_layout_ranges_within (blockSize : Std.U64)
    (hBlock : 8 ≤ blockSize.val) :
    ∃ (crcStart : Std.U64) (layout : SpillBlockLayout),
      spill_block_layout blockSize = ok (.Ok layout) ∧
      spillLayoutWithin layout blockSize ∧
      crcStart.val = blockSize.val - 8 := by
  rcases spill_block_layout_success blockSize hBlock with
    ⟨crcStart, layout, hOk, hCrcStart, hLayout⟩
  refine ⟨crcStart, layout, hOk, ?_, hCrcStart⟩
  rw [hLayout]
  exact spillLayoutSpec_within blockSize crcStart hBlock hCrcStart

theorem sidecar_tape_file_layout_success (h p : Std.U64)
    (hPositive : 0 < h.val)
    (hTail : h.val + p.val < 2 ^ 64)
    (hFooter : h.val + p.val + h.val < 2 ^ 64)
    (hTotal : h.val + p.val + h.val + 1 < 2 ^ 64) :
    ∃ tailStart footerIndex total layout,
      sidecar_tape_file_layout h p = ok (.Ok layout) ∧
      tailStart.val = h.val + p.val ∧
      footerIndex.val = h.val + p.val + h.val ∧
      total.val = h.val + p.val + h.val + 1 ∧
      layout = sidecarTapeLayoutSpec h p tailStart footerIndex total := by
  have hNe : h ≠ 0#u64 := by
    intro hz
    have hzv : h.val = 0 := by scalar_tac
    omega
  rcases checked_add_ok h p hTail with ⟨tailStart, hAddTail, hTailVal⟩
  have hFooterInput : tailStart.val + h.val < 2 ^ 64 := by
    rw [hTailVal]
    exact hFooter
  rcases checked_add_ok tailStart h hFooterInput with
    ⟨footerIndex, hAddFooter, hFooterValRaw⟩
  have hFooterVal : footerIndex.val = h.val + p.val + h.val := by
    rw [hFooterValRaw, hTailVal]
  have hTotalInput : footerIndex.val + 1 < 2 ^ 64 := by
    rw [hFooterVal]
    exact hTotal
  rcases checked_add_ok footerIndex 1#u64 hTotalInput with
    ⟨total, hAddTotal, hTotalValRaw⟩
  have hTotalVal : total.val = h.val + p.val + h.val + 1 := by
    rw [hTotalValRaw, hFooterVal]
    rfl
  refine ⟨tailStart, footerIndex, total,
    sidecarTapeLayoutSpec h p tailStart footerIndex total, ?_, hTailVal,
    hFooterVal, hTotalVal, rfl⟩
  unfold sidecar_tape_file_layout sidecarTapeLayoutSpec byteRangeSpec range
  simp [hNe, hAddTail, hAddFooter, hAddTotal,
    core.result.Result.Insts.CoreOpsTry.branch]

theorem sidecar_tape_file_layout_ranges_within (h p : Std.U64)
    (hPositive : 0 < h.val)
    (hTail : h.val + p.val < 2 ^ 64)
    (hFooter : h.val + p.val + h.val < 2 ^ 64)
    (hTotal : h.val + p.val + h.val + 1 < 2 ^ 64) :
    ∃ (total : Std.U64) (layout : SidecarTapeFileLayout),
      sidecar_tape_file_layout h p = ok (.Ok layout) ∧
      sidecarTapeLayoutWithin layout total := by
  rcases sidecar_tape_file_layout_success h p hPositive hTail hFooter hTotal with
    ⟨tailStart, footerIndex, total, layout, hOk, hTailVal, hFooterVal,
      hTotalVal, hLayout⟩
  refine ⟨total, layout, hOk, ?_⟩
  rw [hLayout]
  exact sidecarTapeLayoutSpec_within h p tailStart footerIndex total
    hPositive hTailVal hFooterVal hTotalVal

theorem sidecar_tape_file_layout_rejects_zero_header (p : Std.U64) :
    sidecar_tape_file_layout 0#u64 p =
      ok (.Err LayoutError.HeaderBlockCountZero) := by
  unfold sidecar_tape_file_layout
  simp

end parity_sidecar_layout_verif
