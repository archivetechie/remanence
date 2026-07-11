/- Specification theorems for the AEAD framing extraction (SPEC.md A1-A7).

   Targets the Aeneas-generated definitions in `AeadFraming.Funs`. The Lean
   checker accepting this file with no remaining local placeholders is the
   success criterion; the generated file is trusted only through Aeneas plus
   Lean, and the Rust `drift_guard` test ties the extraction back to production
   `crates/remanence-aead/src/{stream,range,inspect}.rs`. -/
import AeadFraming.Funs

open Aeneas Aeneas.Std Result

set_option linter.unusedSimpArgs false

namespace AeadFraming

/- Formal-proof scope for this extraction:
   this is the verification claim attached to the AEAD framing proof. These
   theorems certify the extracted pure RAO AEAD framing arithmetic:
   chunk-count validation, payload-frame length, stored-size rounding,
   ciphertext offsets, plaintext range validation, non-empty range planning,
   and keyless inspect geometry. They do not prove ChaCha20-Poly1305 security,
   HKDF, SHA-256, CBOR canonicalization, byte I/O, allocation behavior, or the
   production parser's prior header-validation path; those remain covered by
   crypto-library trust, the extraction drift guard, and normal Rust tests. -/

def HeaderLen : Nat := 128
def FooterLen : Nat := 16
def TagLen : Nat := 16
def ChunkGranularity : Nat := 512

/- V2 keeps the scalar header at 128 bytes and inserts a variable key frame.
   These generic definitions make the framing prefix explicit and quantify the
   arithmetic once over both lengths. -/
def prefixLenSpec (headerLen keyFrameLen : Nat) : Nat :=
  headerLen + keyFrameLen

def genericCipherOffsetSpec
    (headerLen keyFrameLen metadataFrameLen blockIndex stride : Nat) : Nat :=
  prefixLenSpec headerLen keyFrameLen + metadataFrameLen + blockIndex * stride

def genericFooterOffsetSpec
    (headerLen keyFrameLen metadataFrameLen chunkCount stride : Nat) : Nat :=
  prefixLenSpec headerLen keyFrameLen + metadataFrameLen + chunkCount * stride

def genericFooterEndSpec
    (headerLen keyFrameLen metadataFrameLen payloadFrameLen footerLen : Nat) : Nat :=
  prefixLenSpec headerLen keyFrameLen + metadataFrameLen + payloadFrameLen + footerLen

def genericStoredSizeSpec
    (headerLen keyFrameLen metadataFrameLen payloadFrameLen footerLen chunkSize : Nat) : Nat :=
  let footerEnd :=
    genericFooterEndSpec headerLen keyFrameLen metadataFrameLen payloadFrameLen footerLen
  if footerEnd % chunkSize = 0 then footerEnd else footerEnd + (chunkSize - footerEnd % chunkSize)

def genericInspectNumeratorSpec
    (headerLen keyFrameLen storedSize metadataFrameLen footerLen : Nat) : Nat :=
  storedSize - headerLen - keyFrameLen - footerLen - metadataFrameLen

theorem generic_prefix_geometry
    (headerLen keyFrameLen metadataFrameLen index stride : Nat) :
    genericCipherOffsetSpec headerLen keyFrameLen metadataFrameLen index stride =
      headerLen + keyFrameLen + metadataFrameLen + index * stride := by
  rfl

theorem generic_prefix_v1_instance
    (metadataFrameLen index stride : Nat) :
    genericCipherOffsetSpec 128 0 metadataFrameLen index stride =
      128 + metadataFrameLen + index * stride := by
  simp [genericCipherOffsetSpec, prefixLenSpec]

theorem generic_prefix_v2_instance
    (keyFrameLen metadataFrameLen index stride : Nat) :
    genericCipherOffsetSpec 128 keyFrameLen metadataFrameLen index stride =
      128 + keyFrameLen + metadataFrameLen + index * stride := by
  rfl

theorem generic_footer_uses_same_prefix
    (headerLen keyFrameLen metadataFrameLen chunkCount stride : Nat) :
    genericFooterOffsetSpec headerLen keyFrameLen metadataFrameLen chunkCount stride =
      genericCipherOffsetSpec headerLen keyFrameLen metadataFrameLen chunkCount stride := by
  rfl

theorem generic_footer_end_geometry
    (headerLen keyFrameLen metadataFrameLen payloadFrameLen footerLen : Nat) :
    genericFooterEndSpec headerLen keyFrameLen metadataFrameLen payloadFrameLen footerLen =
      headerLen + keyFrameLen + metadataFrameLen + payloadFrameLen + footerLen := by
  rfl

theorem generic_stored_size_v1_instance
    (metadataFrameLen payloadFrameLen footerLen chunkSize : Nat) :
    genericStoredSizeSpec 128 0 metadataFrameLen payloadFrameLen footerLen chunkSize =
      let footerEnd := 128 + metadataFrameLen + payloadFrameLen + footerLen
      if footerEnd % chunkSize = 0 then
        footerEnd
      else
        footerEnd + (chunkSize - footerEnd % chunkSize) := by
  simp [genericStoredSizeSpec, genericFooterEndSpec, prefixLenSpec]

theorem generic_stored_size_v2_uses_key_frame_prefix
    (keyFrameLen metadataFrameLen payloadFrameLen footerLen chunkSize : Nat) :
    genericStoredSizeSpec 128 keyFrameLen metadataFrameLen payloadFrameLen footerLen chunkSize =
      let footerEnd := 128 + keyFrameLen + metadataFrameLen + payloadFrameLen + footerLen
      if footerEnd % chunkSize = 0 then
        footerEnd
      else
        footerEnd + (chunkSize - footerEnd % chunkSize) := by
  rfl

theorem generic_inspect_numerator_geometry
    (headerLen keyFrameLen storedSize metadataFrameLen footerLen : Nat) :
    genericInspectNumeratorSpec
        headerLen keyFrameLen storedSize metadataFrameLen footerLen =
      storedSize - headerLen - keyFrameLen - footerLen - metadataFrameLen := by
  rfl

def strideSpec (chunkSize : Std.U64) : Nat :=
  chunkSize.val + TagLen

def chunkCountSpec (plaintextSize chunkSize : Std.U64) : Nat :=
  plaintextSize.val / chunkSize.val

def payloadFrameLenSpec (plaintextSize chunkSize : Std.U64) : Nat :=
  plaintextSize.val + TagLen * chunkCountSpec plaintextSize chunkSize

def roundUpSpec (value multiple : Std.U64) : Nat :=
  if value.val % multiple.val = 0 then
    value.val
  else
    value.val + (multiple.val - value.val % multiple.val)

def footerEndSpec (chunkSize metadataFrameLen plaintextSize : Std.U64) : Nat :=
  HeaderLen + metadataFrameLen.val +
    payloadFrameLenSpec plaintextSize chunkSize + FooterLen

def storedSizeSpec (chunkSize metadataFrameLen plaintextSize : Std.U64) : Nat :=
  let footerEnd := footerEndSpec chunkSize metadataFrameLen plaintextSize
  if footerEnd % chunkSize.val = 0 then
    footerEnd
  else
    footerEnd + (chunkSize.val - footerEnd % chunkSize.val)

def cipherOffsetNatSpec
    (metadataFrameLen chunkSize : Std.U64) (blockIndex : Nat) : Nat :=
  HeaderLen + metadataFrameLen.val + blockIndex * strideSpec chunkSize

def cipherOffsetSpec (metadataFrameLen chunkSize blockIndex : Std.U64) : Nat :=
  cipherOffsetNatSpec metadataFrameLen chunkSize blockIndex.val

def rangeEndSpec (start len : Std.U64) : Nat :=
  start.val + len.val

def lastByteSpec (start len : Std.U64) : Nat :=
  rangeEndSpec start len - 1

def firstChunkSpec (chunkSize start : Std.U64) : Nat :=
  start.val / chunkSize.val

def lastChunkSpec (chunkSize start len : Std.U64) : Nat :=
  lastByteSpec start len / chunkSize.val

def fetchedChunkCountSpec (chunkSize start len : Std.U64) : Nat :=
  lastChunkSpec chunkSize start len - firstChunkSpec chunkSize start + 1

def storedRangeEndSpec (metadataFrameLen chunkSize start len : Std.U64) : Nat :=
  cipherOffsetNatSpec metadataFrameLen chunkSize
    (lastChunkSpec chunkSize start len) +
    strideSpec chunkSize

def storedRangeLenSpec (metadataFrameLen chunkSize start len : Std.U64) : Nat :=
  storedRangeEndSpec metadataFrameLen chunkSize start len -
    cipherOffsetNatSpec metadataFrameLen chunkSize
      (firstChunkSpec chunkSize start)

def trimStartSpec (chunkSize start : Std.U64) : Nat :=
  start.val % chunkSize.val

def PayloadNoOverflow (plaintextSize chunkSize : Std.U64) : Prop :=
  TagLen * chunkCountSpec plaintextSize chunkSize < 2 ^ 64 ∧
  payloadFrameLenSpec plaintextSize chunkSize < 2 ^ 64

def StoredSizeNoOverflow
    (chunkSize metadataFrameLen plaintextSize : Std.U64) : Prop :=
  PayloadNoOverflow plaintextSize chunkSize ∧
  HeaderLen + metadataFrameLen.val < 2 ^ 64 ∧
  HeaderLen + metadataFrameLen.val +
    payloadFrameLenSpec plaintextSize chunkSize < 2 ^ 64 ∧
  footerEndSpec chunkSize metadataFrameLen plaintextSize < 2 ^ 64 ∧
  storedSizeSpec chunkSize metadataFrameLen plaintextSize < 2 ^ 64

def CipherOffsetNoOverflow
    (metadataFrameLen chunkSize blockIndex : Std.U64) : Prop :=
  strideSpec chunkSize < 2 ^ 64 ∧
  HeaderLen + metadataFrameLen.val < 2 ^ 64 ∧
  blockIndex.val * strideSpec chunkSize < 2 ^ 64 ∧
  cipherOffsetSpec metadataFrameLen chunkSize blockIndex < 2 ^ 64

def RangePlanNoOverflow
    (metadataFrameLen chunkSize plaintextSize start len : Std.U64) : Prop :=
  chunkSize.val ≠ 0 ∧
  len.val ≠ 0 ∧
  rangeEndSpec start len < 2 ^ 64 ∧
  rangeEndSpec start len ≤ plaintextSize.val ∧
  firstChunkSpec chunkSize start ≤ lastChunkSpec chunkSize start len ∧
  firstChunkSpec chunkSize start < 2 ^ 64 ∧
  lastChunkSpec chunkSize start len < 2 ^ 64 ∧
  fetchedChunkCountSpec chunkSize start len < 2 ^ 64 ∧
  strideSpec chunkSize < 2 ^ 64 ∧
  cipherOffsetNatSpec metadataFrameLen chunkSize
    (firstChunkSpec chunkSize start) < 2 ^ 64 ∧
  cipherOffsetNatSpec metadataFrameLen chunkSize
    (lastChunkSpec chunkSize start len) < 2 ^ 64 ∧
  storedRangeEndSpec metadataFrameLen chunkSize start len < 2 ^ 64 ∧
  storedRangeLenSpec metadataFrameLen chunkSize start len < 2 ^ 64

def inspectNumeratorSpec (storedSizeBytes metadataFrameLen : Std.U64) : Nat :=
  storedSizeBytes.val - (HeaderLen + FooterLen) - metadataFrameLen.val

def inspectChunkCountSpec
    (storedSizeBytes metadataFrameLen chunkSize : Std.U64) : Nat :=
  inspectNumeratorSpec storedSizeBytes metadataFrameLen / strideSpec chunkSize

def inspectPlaintextSizeSpec
    (storedSizeBytes metadataFrameLen chunkSize : Std.U64) : Nat :=
  inspectChunkCountSpec storedSizeBytes metadataFrameLen chunkSize * chunkSize.val

def inspectFooterOffsetSpec
    (storedSizeBytes metadataFrameLen chunkSize : Std.U64) : Nat :=
  HeaderLen + metadataFrameLen.val +
    inspectChunkCountSpec storedSizeBytes metadataFrameLen chunkSize *
      strideSpec chunkSize

def inspectExpectedSizeSpec
    (storedSizeBytes metadataFrameLen chunkSize : Std.U64) : Nat :=
  let footerEnd := inspectFooterOffsetSpec storedSizeBytes metadataFrameLen chunkSize +
    FooterLen
  if footerEnd % chunkSize.val = 0 then
    footerEnd
  else
    footerEnd + (chunkSize.val - footerEnd % chunkSize.val)

def inspectFillLenSpec
    (storedSizeBytes metadataFrameLen chunkSize : Std.U64) : Nat :=
  inspectExpectedSizeSpec storedSizeBytes metadataFrameLen chunkSize -
    (inspectFooterOffsetSpec storedSizeBytes metadataFrameLen chunkSize + FooterLen)

def InspectGeometryNoOverflow
    (storedSizeBytes metadataFrameLen chunkSize : Std.U64) : Prop :=
  chunkSize.val ≠ 0 ∧
  storedSizeBytes.val % chunkSize.val = 0 ∧
  HeaderLen + FooterLen + metadataFrameLen.val ≤ storedSizeBytes.val ∧
  strideSpec chunkSize < 2 ^ 64 ∧
  inspectNumeratorSpec storedSizeBytes metadataFrameLen < 2 ^ 64 ∧
  inspectChunkCountSpec storedSizeBytes metadataFrameLen chunkSize ≠ 0 ∧
  inspectPlaintextSizeSpec storedSizeBytes metadataFrameLen chunkSize < 2 ^ 64 ∧
  inspectChunkCountSpec storedSizeBytes metadataFrameLen chunkSize *
    strideSpec chunkSize < 2 ^ 64 ∧
  HeaderLen + metadataFrameLen.val < 2 ^ 64 ∧
  inspectFooterOffsetSpec storedSizeBytes metadataFrameLen chunkSize < 2 ^ 64 ∧
  inspectFooterOffsetSpec storedSizeBytes metadataFrameLen chunkSize +
    FooterLen < 2 ^ 64 ∧
  inspectExpectedSizeSpec storedSizeBytes metadataFrameLen chunkSize < 2 ^ 64 ∧
  inspectExpectedSizeSpec storedSizeBytes metadataFrameLen chunkSize =
    storedSizeBytes.val ∧
  inspectFillLenSpec storedSizeBytes metadataFrameLen chunkSize < 2 ^ 64

lemma u64_eq_zero_of_val_zero (x : Std.U64) (h : x.val = 0) : x = 0#u64 := by
  apply UScalar.eq_imp
  simpa using h

lemma u64_ne_zero_of_val_ne_zero (x : Std.U64) (h : x.val ≠ 0) :
    x ≠ 0#u64 := by
  intro hx
  apply h
  rw [hx]
  rfl

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

lemma u64_checked_add_none_of_sum_ge (a b : Std.U64)
    (h : 2 ^ 64 ≤ a.val + b.val) :
    U64.checked_add a b = none := by
  have hspec := U64.checked_add_bv_spec a b
  cases hadd : U64.checked_add a b with
  | none => rfl
  | some sum =>
      simp [hadd, U64.max, U64.numBits] at hspec
      omega

lemma u64_checked_sub_some_of_le (a b : Std.U64)
    (h : b.val ≤ a.val) :
    ∃ diff, U64.checked_sub a b = some diff ∧ diff.val = a.val - b.val := by
  have hspec := U64.checked_sub_bv_spec a b
  cases hsub : U64.checked_sub a b with
  | none =>
      simp [hsub] at hspec
      omega
  | some diff =>
      simp [hsub] at hspec
      exact ⟨diff, rfl, hspec.2.1⟩

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

lemma checked_sub_ok (a b : Std.U64) (h : b.val ≤ a.val) :
    ∃ diff, checked_sub a b = ok (.Ok diff) ∧ diff.val = a.val - b.val := by
  rcases u64_checked_sub_some_of_le a b h with ⟨diff, hsub, hval⟩
  refine ⟨diff, ?_, hval⟩
  unfold checked_sub
  simp [lift, hsub]

lemma checked_mul_ok (a b : Std.U64) (h : a.val * b.val < 2 ^ 64) :
    ∃ product, checked_mul a b = ok (.Ok product) ∧
      product.val = a.val * b.val := by
  rcases u64_checked_mul_some_of_prod_lt a b h with ⟨product, hmul, hval⟩
  refine ⟨product, ?_, hval⟩
  unfold checked_mul
  simp [lift, hmul]

lemma u64_add_ok_val (x y : Std.U64) (h : x.val + y.val < 2 ^ 64) :
    ∃ z, x + y = ok z ∧ z.val = x.val + y.val := by
  have hmax : x.val + y.val ≤ U64.max := by
    simp [U64.max, U64.numBits]
    omega
  have hspec := U64.add_spec (x := x) (y := y) hmax
  cases hadd : x + y with
  | ok z =>
      simp [hadd] at hspec
      exact ⟨z, rfl, hspec⟩
  | fail e =>
      simp [hadd] at hspec
  | div =>
      simp [hadd] at hspec

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

lemma u64_sub_ok_val (x y : Std.U64) (h : y.val ≤ x.val) :
    ∃ z, x - y = ok z ∧ z.val = x.val - y.val := by
  have hspec := U64.sub_spec (x := x) (y := y) h
  cases hsub : x - y with
  | ok z =>
      simp [hsub] at hspec
      exact ⟨z, rfl, hspec.1⟩
  | fail e =>
      simp [hsub] at hspec
  | div =>
      simp [hsub] at hspec

lemma tag_len_val : CHACHA20POLY1305_TAG_LEN.val = TagLen := by
  simp [CHACHA20POLY1305_TAG_LEN, TagLen]

lemma header_len_val : RAO_HEADER_LEN.val = HeaderLen := by
  simp [RAO_HEADER_LEN, HeaderLen]

lemma footer_len_val : RAO_FOOTER_LEN.val = FooterLen := by
  simp [RAO_FOOTER_LEN, FooterLen]

lemma granularity_val : CHUNK_SIZE_GRANULARITY.val = ChunkGranularity := by
  simp [CHUNK_SIZE_GRANULARITY, ChunkGranularity]

theorem validate_chunk_size_success (chunkSize : Std.U64)
    (hpos : chunkSize.val ≠ 0)
    (hgran : chunkSize.val % ChunkGranularity = 0) :
    validate_chunk_size chunkSize = ok (.Ok ()) := by
  have hChunkNe := u64_ne_zero_of_val_ne_zero chunkSize hpos
  have hGranNz : CHUNK_SIZE_GRANULARITY.val ≠ 0 := by
    simp [CHUNK_SIZE_GRANULARITY]
  rcases u64_rem_ok_val chunkSize CHUNK_SIZE_GRANULARITY hGranNz with
    ⟨rem, hrem, hremVal⟩
  have hremZero : rem = 0#u64 := by
    apply u64_eq_zero_of_val_zero
    rw [hremVal, granularity_val]
    exact hgran
  unfold validate_chunk_size
  simp [hChunkNe, hrem, hremZero]

theorem validate_chunk_size_rejects_zero :
    validate_chunk_size 0#u64 =
      ok (.Err AeadFrameError.InvalidChunkSize) := by
  unfold validate_chunk_size
  simp

theorem chunk_count_success (plaintextSize chunkSize : Std.U64)
    (hChunk : chunkSize.val ≠ 0)
    (hPlain : plaintextSize.val ≠ 0)
    (hAligned : plaintextSize.val % chunkSize.val = 0) :
    ∃ chunks,
      chunk_count plaintextSize chunkSize = ok (.Ok chunks) ∧
      chunks.val = chunkCountSpec plaintextSize chunkSize := by
  have hChunkNe := u64_ne_zero_of_val_ne_zero chunkSize hChunk
  have hPlainNe := u64_ne_zero_of_val_ne_zero plaintextSize hPlain
  rcases u64_rem_ok_val plaintextSize chunkSize hChunk with
    ⟨rem, hrem, hremVal⟩
  have hremZero : rem = 0#u64 := by
    apply u64_eq_zero_of_val_zero
    rw [hremVal]
    exact hAligned
  rcases u64_div_ok_val plaintextSize chunkSize hChunk with
    ⟨chunks, hdiv, hdivVal⟩
  refine ⟨chunks, ?_, ?_⟩
  · unfold chunk_count
    simp [hChunkNe, hPlainNe, hrem, hremZero, hdiv]
  · rw [hdivVal]
    rfl

theorem chunk_count_rejects_zero_chunk (plaintextSize : Std.U64) :
    chunk_count plaintextSize 0#u64 =
      ok (.Err AeadFrameError.InvalidMetadataField) := by
  unfold chunk_count
  simp

theorem chunk_count_rejects_zero_plaintext (chunkSize : Std.U64)
    (hChunk : chunkSize.val ≠ 0) :
    chunk_count 0#u64 chunkSize =
      ok (.Err AeadFrameError.InvalidMetadataField) := by
  have hChunkNe := u64_ne_zero_of_val_ne_zero chunkSize hChunk
  unfold chunk_count
  simp [hChunkNe]

theorem payload_frame_len_success (plaintextSize chunkSize : Std.U64)
    (hChunk : chunkSize.val ≠ 0)
    (hPlain : plaintextSize.val ≠ 0)
    (hAligned : plaintextSize.val % chunkSize.val = 0)
    (hno : PayloadNoOverflow plaintextSize chunkSize) :
    ∃ payloadLen,
      payload_frame_len plaintextSize chunkSize = ok (.Ok payloadLen) ∧
      payloadLen.val = payloadFrameLenSpec plaintextSize chunkSize := by
  rcases hno with ⟨hTagMul, hPayload⟩
  rcases chunk_count_success plaintextSize chunkSize hChunk hPlain hAligned with
    ⟨chunks, hchunks, hchunksVal⟩
  have hTagMulInput : CHACHA20POLY1305_TAG_LEN.val * chunks.val < 2 ^ 64 := by
    rw [tag_len_val, hchunksVal]
    exact hTagMul
  rcases checked_mul_ok CHACHA20POLY1305_TAG_LEN chunks hTagMulInput with
    ⟨tagBytes, htagBytes, htagBytesVal⟩
  have hPayloadInput : plaintextSize.val + tagBytes.val < 2 ^ 64 := by
    rw [htagBytesVal, tag_len_val, hchunksVal]
    exact hPayload
  rcases checked_add_ok plaintextSize tagBytes hPayloadInput with
    ⟨payloadLen, hpayloadLen, hpayloadLenVal⟩
  refine ⟨payloadLen, ?_, ?_⟩
  · unfold payload_frame_len
    simp [hchunks, core.result.Result.Insts.CoreOpsTry.branch, htagBytes,
      hpayloadLen]
  · rw [hpayloadLenVal, htagBytesVal, tag_len_val, hchunksVal]
    rfl

theorem round_up_success (value multiple : Std.U64)
    (hMultiple : multiple.val ≠ 0)
    (hNo : roundUpSpec value multiple < 2 ^ 64) :
    ∃ rounded,
      round_up value multiple = ok (.Ok rounded) ∧
      rounded.val = roundUpSpec value multiple := by
  have hMultipleNe := u64_ne_zero_of_val_ne_zero multiple hMultiple
  rcases u64_rem_ok_val value multiple hMultiple with ⟨rem, hrem, hremVal⟩
  unfold round_up
  by_cases hAligned : value.val % multiple.val = 0
  · have hremZero : rem = 0#u64 := by
      apply u64_eq_zero_of_val_zero
      rw [hremVal]
      exact hAligned
    refine ⟨value, ?_, ?_⟩
    · simp [hMultipleNe, hrem, hremZero]
    · unfold roundUpSpec
      simp [hAligned]
  · have hremNe : rem ≠ 0#u64 := by
      intro hz
      have hzv : rem.val = 0 := by rw [hz]; rfl
      apply hAligned
      rw [hremVal] at hzv
      exact hzv
    have hRemLt : rem.val < multiple.val := by
      rw [hremVal]
      exact Nat.mod_lt _ (Nat.pos_of_ne_zero hMultiple)
    rcases u64_sub_ok_val multiple rem (Nat.le_of_lt hRemLt) with
      ⟨diff, hdiff, hdiffVal⟩
    have hAddInput : value.val + diff.val < 2 ^ 64 := by
      rw [hdiffVal, hremVal]
      unfold roundUpSpec at hNo
      simpa [hAligned] using hNo
    rcases checked_add_ok value diff hAddInput with
      ⟨rounded, hrounded, hroundedVal⟩
    refine ⟨rounded, ?_, ?_⟩
    · simp [hMultipleNe, hrem, hremNe, hdiff, hrounded]
    · rw [hroundedVal, hdiffVal, hremVal]
      unfold roundUpSpec
      simp [hAligned]

theorem round_up_rejects_zero_multiple (value : Std.U64) :
    round_up value 0#u64 = ok (.Err AeadFrameError.SizeOverflow) := by
  unfold round_up
  simp

theorem cipher_offset_success
    (metadataFrameLen chunkSize blockIndex : Std.U64)
    (hno : CipherOffsetNoOverflow metadataFrameLen chunkSize blockIndex) :
    ∃ offset,
      cipher_offset metadataFrameLen chunkSize blockIndex = ok (.Ok offset) ∧
      offset.val = cipherOffsetSpec metadataFrameLen chunkSize blockIndex := by
  rcases hno with ⟨hStride, hHeaderMeta, hPayloadOffset, hOffset⟩
  have hStrideInput : chunkSize.val + CHACHA20POLY1305_TAG_LEN.val < 2 ^ 64 := by
    rw [tag_len_val]
    exact hStride
  rcases checked_add_ok chunkSize CHACHA20POLY1305_TAG_LEN hStrideInput with
    ⟨stride, hstride, hstrideVal⟩
  have hHeaderInput : RAO_HEADER_LEN.val + metadataFrameLen.val < 2 ^ 64 := by
    rw [header_len_val]
    exact hHeaderMeta
  rcases checked_add_ok RAO_HEADER_LEN metadataFrameLen hHeaderInput with
    ⟨base, hbase, hbaseVal⟩
  have hPayloadInput : blockIndex.val * stride.val < 2 ^ 64 := by
    rw [hstrideVal, tag_len_val]
    exact hPayloadOffset
  rcases checked_mul_ok blockIndex stride hPayloadInput with
    ⟨payloadOffset, hpayloadOffset, hpayloadOffsetVal⟩
  have hOffsetInput : base.val + payloadOffset.val < 2 ^ 64 := by
    rw [hbaseVal, hpayloadOffsetVal, header_len_val, hstrideVal, tag_len_val]
    exact hOffset
  rcases checked_add_ok base payloadOffset hOffsetInput with
    ⟨offset, hoffset, hoffsetVal⟩
  refine ⟨offset, ?_, ?_⟩
  · unfold cipher_offset
    simp [hstride, hbase, core.result.Result.Insts.CoreOpsTry.branch,
      hpayloadOffset, hoffset]
  · rw [hoffsetVal, hbaseVal, hpayloadOffsetVal, hstrideVal, header_len_val,
      tag_len_val]
    rfl

theorem cipher_offset_zero_block_is_payload_base
    (metadataFrameLen chunkSize : Std.U64)
    (hStride : strideSpec chunkSize < 2 ^ 64)
    (hHeaderMeta : HeaderLen + metadataFrameLen.val < 2 ^ 64) :
    ∃ offset,
      cipher_offset metadataFrameLen chunkSize 0#u64 = ok (.Ok offset) ∧
      offset.val = HeaderLen + metadataFrameLen.val := by
  have hno : CipherOffsetNoOverflow metadataFrameLen chunkSize 0#u64 := by
    refine ⟨hStride, hHeaderMeta, ?_, ?_⟩
    · simp
    · unfold cipherOffsetSpec cipherOffsetNatSpec
      simpa using hHeaderMeta
  rcases cipher_offset_success metadataFrameLen chunkSize 0#u64 hno with
    ⟨offset, hoffset, hoffsetVal⟩
  refine ⟨offset, hoffset, ?_⟩
  rw [hoffsetVal]
  unfold cipherOffsetSpec cipherOffsetNatSpec
  simp

theorem validate_range_success (start len plaintextSize : Std.U64)
    (hEnd : rangeEndSpec start len < 2 ^ 64)
    (hValid :
      if len.val = 0 then start.val ≤ plaintextSize.val
      else rangeEndSpec start len ≤ plaintextSize.val) :
    ∃ endValue,
      validate_range start len plaintextSize = ok (.Ok endValue) ∧
      endValue.val = rangeEndSpec start len := by
  rcases u64_checked_add_some_of_sum_lt start len hEnd with
    ⟨endValue, hadd, hendValue⟩
  refine ⟨endValue, ?_, hendValue⟩
  unfold validate_range
  by_cases hLenZero : len.val = 0
  · have hLenEq := u64_eq_zero_of_val_zero len hLenZero
    have hadd0 : U64.checked_add start 0#u64 = some endValue := by
      simpa [hLenEq] using hadd
    have hStartLe : start.val ≤ plaintextSize.val := by
      simpa [hLenZero] using hValid
    have hStartNotPast : ¬ start > plaintextSize := by
      scalar_tac
    simp [lift, hadd0, hLenEq, hStartNotPast]
  · have hLenNe := u64_ne_zero_of_val_ne_zero len hLenZero
    have hEndLe : endValue.val ≤ plaintextSize.val := by
      have h := hValid
      simp [hLenZero, rangeEndSpec] at h
      omega
    have hEndNotPast : ¬ endValue > plaintextSize := by
      scalar_tac
    simp [lift, hadd, hLenNe, hEndNotPast, hEndLe]

theorem validate_range_rejects_overflow (start len plaintextSize : Std.U64)
    (hOverflow : 2 ^ 64 ≤ rangeEndSpec start len) :
    validate_range start len plaintextSize =
      ok (.Err AeadFrameError.PlaintextRangeOverflow) := by
  have hadd := u64_checked_add_none_of_sum_ge start len hOverflow
  unfold validate_range
  simp [lift, hadd]

theorem validate_range_rejects_empty_past_end (start plaintextSize : Std.U64)
    (hPast : plaintextSize.val < start.val) :
    validate_range start 0#u64 plaintextSize =
      ok (.Err AeadFrameError.EmptyRangeStartsPastEnd) := by
  have hAdd : start.val + (0#u64 : Std.U64).val < 2 ^ 64 := by
    have := U64.lt_succ_max start
    simpa using this
  rcases u64_checked_add_some_of_sum_lt start 0#u64 hAdd with
    ⟨endValue, hadd, hendValue⟩
  have hStartPast : start > plaintextSize := by
    scalar_tac
  unfold validate_range
  simp [lift, hadd, hStartPast]

theorem validate_range_rejects_nonempty_past_end
    (start len plaintextSize : Std.U64)
    (hLen : len.val ≠ 0)
    (hEnd : rangeEndSpec start len < 2 ^ 64)
    (hPast : plaintextSize.val < rangeEndSpec start len) :
    validate_range start len plaintextSize =
      ok (.Err AeadFrameError.PlaintextRangePastEnd) := by
  rcases u64_checked_add_some_of_sum_lt start len hEnd with
    ⟨endValue, hadd, hendValue⟩
  have hLenNe := u64_ne_zero_of_val_ne_zero len hLen
  have hEndPastVal : plaintextSize.val < endValue.val := by
    rw [hendValue]
    simpa [rangeEndSpec] using hPast
  have hEndPast : endValue > plaintextSize := by
    scalar_tac
  unfold validate_range
  simp [lift, hadd, hLenNe, hEndPast, hEndPastVal]

theorem stored_size_from_parts_success
    (chunkSize metadataFrameLen plaintextSize : Std.U64)
    (hChunk : chunkSize.val ≠ 0)
    (hPlain : plaintextSize.val ≠ 0)
    (hAligned : plaintextSize.val % chunkSize.val = 0)
    (hno : StoredSizeNoOverflow chunkSize metadataFrameLen plaintextSize) :
    ∃ storedSize,
      stored_size_from_parts chunkSize metadataFrameLen plaintextSize =
        ok (.Ok storedSize) ∧
      storedSize.val = storedSizeSpec chunkSize metadataFrameLen plaintextSize := by
  rcases hno with
    ⟨hPayloadNo, hHeaderMetaNo, hHeaderPayloadNo, hFooterEndNo, hStoredNo⟩
  rcases payload_frame_len_success plaintextSize chunkSize hChunk hPlain hAligned
      hPayloadNo with ⟨payloadLen, hpayloadLen, hpayloadLenVal⟩
  have hHeaderMetaInput : RAO_HEADER_LEN.val + metadataFrameLen.val < 2 ^ 64 := by
    rw [header_len_val]
    exact hHeaderMetaNo
  rcases checked_add_ok RAO_HEADER_LEN metadataFrameLen hHeaderMetaInput with
    ⟨headerMeta, hheaderMeta, hheaderMetaVal⟩
  have hHeaderPayloadInput : headerMeta.val + payloadLen.val < 2 ^ 64 := by
    rw [hheaderMetaVal, hpayloadLenVal, header_len_val]
    exact hHeaderPayloadNo
  rcases checked_add_ok headerMeta payloadLen hHeaderPayloadInput with
    ⟨headerPayload, hheaderPayload, hheaderPayloadVal⟩
  have hFooterEndInput : headerPayload.val + RAO_FOOTER_LEN.val < 2 ^ 64 := by
    rw [hheaderPayloadVal, hheaderMetaVal, hpayloadLenVal, header_len_val,
      footer_len_val]
    exact hFooterEndNo
  rcases checked_add_ok headerPayload RAO_FOOTER_LEN hFooterEndInput with
    ⟨footerEnd, hfooterEnd, hfooterEndVal⟩
  have hFooterEndSpec :
      footerEnd.val = footerEndSpec chunkSize metadataFrameLen plaintextSize := by
    rw [hfooterEndVal, hheaderPayloadVal, hheaderMetaVal, hpayloadLenVal,
      header_len_val, footer_len_val]
    rfl
  have hRoundNo : roundUpSpec footerEnd chunkSize < 2 ^ 64 := by
    unfold roundUpSpec
    rw [hFooterEndSpec]
    change storedSizeSpec chunkSize metadataFrameLen plaintextSize < 2 ^ 64
    exact hStoredNo
  rcases round_up_success footerEnd chunkSize hChunk hRoundNo with
    ⟨storedSize, hstoredSize, hstoredSizeVal⟩
  have hRoundSpec :
      roundUpSpec footerEnd chunkSize =
        storedSizeSpec chunkSize metadataFrameLen plaintextSize := by
    unfold storedSizeSpec roundUpSpec
    rw [hFooterEndSpec]
  refine ⟨storedSize, ?_, ?_⟩
  · unfold stored_size_from_parts
    simp [hpayloadLen, core.result.Result.Insts.CoreOpsTry.branch, hheaderMeta,
      hheaderPayload, hfooterEnd, hstoredSize]
  · rw [hstoredSizeVal, hRoundSpec]

theorem expected_stored_size_success
    (chunkSize metadataFrameLen plaintextSize : Std.U64)
    (hChunk : chunkSize.val ≠ 0)
    (hPlain : plaintextSize.val ≠ 0)
    (hAligned : plaintextSize.val % chunkSize.val = 0)
    (hno : StoredSizeNoOverflow chunkSize metadataFrameLen plaintextSize) :
    ∃ storedSize,
      expected_stored_size chunkSize metadataFrameLen plaintextSize =
        ok (.Ok storedSize) ∧
      storedSize.val = storedSizeSpec chunkSize metadataFrameLen plaintextSize := by
  rcases stored_size_from_parts_success chunkSize metadataFrameLen plaintextSize
      hChunk hPlain hAligned hno with ⟨storedSize, hstoredSize, hstoredSizeVal⟩
  refine ⟨storedSize, ?_, hstoredSizeVal⟩
  unfold expected_stored_size
  exact hstoredSize

theorem nonempty_range_plan_rejects_zero_chunk
    (metadataFrameLen plaintextSize start len : Std.U64) :
    nonempty_range_plan metadataFrameLen 0#u64 plaintextSize start len =
      ok (.Err AeadFrameError.InvalidChunkSize) := by
  unfold nonempty_range_plan
  simp

theorem nonempty_range_plan_rejects_empty_len
    (metadataFrameLen chunkSize plaintextSize start : Std.U64)
    (hChunk : chunkSize.val ≠ 0)
    (hStart : start.val ≤ plaintextSize.val) :
    nonempty_range_plan metadataFrameLen chunkSize plaintextSize start 0#u64 =
      ok (.Err AeadFrameError.InvalidMetadataField) := by
  have hChunkNe := u64_ne_zero_of_val_ne_zero chunkSize hChunk
  have hEnd : rangeEndSpec start 0#u64 < 2 ^ 64 := by
    have hlt := U64.lt_succ_max start
    simpa [rangeEndSpec] using hlt
  have hValid :
      if (0#u64 : Std.U64).val = 0 then start.val ≤ plaintextSize.val
      else rangeEndSpec start 0#u64 ≤ plaintextSize.val := by
    simp [hStart]
  rcases validate_range_success start 0#u64 plaintextSize hEnd hValid with
    ⟨endValue, hvalidate, _⟩
  unfold nonempty_range_plan
  simp [hChunkNe, hvalidate, core.result.Result.Insts.CoreOpsTry.branch]

theorem nonempty_range_plan_success
    (metadataFrameLen chunkSize plaintextSize start len : Std.U64)
    (hno : RangePlanNoOverflow metadataFrameLen chunkSize plaintextSize start len) :
    ∃ plan,
      nonempty_range_plan metadataFrameLen chunkSize plaintextSize start len =
        ok (.Ok plan) ∧
      plan.plaintext_end.val = rangeEndSpec start len ∧
      plan.first_chunk.val = firstChunkSpec chunkSize start ∧
      plan.last_chunk.val = lastChunkSpec chunkSize start len ∧
      plan.fetched_chunk_count.val = fetchedChunkCountSpec chunkSize start len ∧
      plan.stored_range_start.val =
        cipherOffsetNatSpec metadataFrameLen chunkSize
          (firstChunkSpec chunkSize start) ∧
      plan.stored_range_len.val =
        storedRangeLenSpec metadataFrameLen chunkSize start len ∧
      plan.trim_start.val = trimStartSpec chunkSize start := by
  rcases hno with
    ⟨hChunk, hLen, hEnd, hValid, hFirstLeLast, hFirstLt, hLastLt,
      hFetchedLt, hStrideLt, hCipherFirstLt, hCipherLastLt, hStoredEndLt,
      hStoredLenLt⟩
  have hChunkNe := u64_ne_zero_of_val_ne_zero chunkSize hChunk
  have hLenNe := u64_ne_zero_of_val_ne_zero len hLen
  have hRangeValid :
      if len.val = 0 then start.val ≤ plaintextSize.val
      else rangeEndSpec start len ≤ plaintextSize.val := by
    simp [hLen, hValid]
  rcases validate_range_success start len plaintextSize hEnd hRangeValid with
    ⟨endValue, hvalidate, hendValue⟩
  rcases u64_div_ok_val start chunkSize hChunk with
    ⟨firstChunk, hfirstChunk, hfirstChunkVal⟩
  have hEndValueGeOne : (1#u64 : Std.U64).val ≤ endValue.val := by
    rw [hendValue]
    have hLenPos : 0 < len.val := Nat.pos_of_ne_zero hLen
    unfold rangeEndSpec
    norm_num
    nlinarith [Nat.zero_le start.val, hLenPos]
  rcases checked_sub_ok endValue 1#u64 hEndValueGeOne with
    ⟨lastByte, hlastByte, hlastByteVal⟩
  have hlastByteSpec : lastByte.val = lastByteSpec start len := by
    rw [hlastByteVal, hendValue]
    rfl
  rcases u64_div_ok_val lastByte chunkSize hChunk with
    ⟨lastChunk, hlastChunk, hlastChunkValRaw⟩
  have hlastChunkVal : lastChunk.val = lastChunkSpec chunkSize start len := by
    rw [hlastChunkValRaw, hlastByteSpec]
    rfl
  have hFirstLeLastU64 : firstChunk.val ≤ lastChunk.val := by
    rw [hfirstChunkVal, hlastChunkVal]
    exact hFirstLeLast
  rcases checked_sub_ok lastChunk firstChunk hFirstLeLastU64 with
    ⟨fetchedMinusOne, hfetchedMinusOne, hfetchedMinusOneVal⟩
  have hFetchedInput : fetchedMinusOne.val + (1#u64 : Std.U64).val < 2 ^ 64 := by
    rw [hfetchedMinusOneVal, hlastChunkVal, hfirstChunkVal]
    simpa [fetchedChunkCountSpec] using hFetchedLt
  rcases checked_add_ok fetchedMinusOne 1#u64 hFetchedInput with
    ⟨fetchedCount, hfetchedCount, hfetchedCountVal⟩
  have hFetchedCountSpec :
      fetchedCount.val = fetchedChunkCountSpec chunkSize start len := by
    rw [hfetchedCountVal, hfetchedMinusOneVal, hlastChunkVal, hfirstChunkVal]
    rfl
  have hStrideInput : chunkSize.val + CHACHA20POLY1305_TAG_LEN.val < 2 ^ 64 := by
    rw [tag_len_val]
    exact hStrideLt
  rcases checked_add_ok chunkSize CHACHA20POLY1305_TAG_LEN hStrideInput with
    ⟨stride, hstride, hstrideVal⟩
  have hHeaderMetaLt :
      HeaderLen + metadataFrameLen.val < 2 ^ 64 := by
    unfold cipherOffsetNatSpec at hCipherFirstLt
    omega
  have hCipherFirstNo :
      CipherOffsetNoOverflow metadataFrameLen chunkSize firstChunk := by
    refine ⟨hStrideLt, hHeaderMetaLt, ?_, ?_⟩
    · rw [hfirstChunkVal]
      have h : HeaderLen + metadataFrameLen.val +
          (start.val / chunkSize.val) * strideSpec chunkSize < 2 ^ 64 := by
        simpa [cipherOffsetNatSpec, firstChunkSpec] using hCipherFirstLt
      omega
    · unfold cipherOffsetSpec cipherOffsetNatSpec
      rw [hfirstChunkVal]
      simpa [cipherOffsetNatSpec, firstChunkSpec] using hCipherFirstLt
  rcases cipher_offset_success metadataFrameLen chunkSize firstChunk
      hCipherFirstNo with ⟨storedStart, hstoredStart, hstoredStartVal⟩
  have hstoredStartSpec :
      storedStart.val =
        cipherOffsetNatSpec metadataFrameLen chunkSize
          (firstChunkSpec chunkSize start) := by
    rw [hstoredStartVal]
    unfold cipherOffsetSpec cipherOffsetNatSpec
    rw [hfirstChunkVal]
    simp [firstChunkSpec]
  have hHeaderMetaLtLast :
      HeaderLen + metadataFrameLen.val < 2 ^ 64 := by
    unfold cipherOffsetNatSpec at hCipherLastLt
    omega
  have hCipherLastNo :
      CipherOffsetNoOverflow metadataFrameLen chunkSize lastChunk := by
    refine ⟨hStrideLt, hHeaderMetaLtLast, ?_, ?_⟩
    · rw [hlastChunkVal]
      have h : HeaderLen + metadataFrameLen.val +
          lastChunkSpec chunkSize start len * strideSpec chunkSize < 2 ^ 64 := by
        simpa [cipherOffsetNatSpec] using hCipherLastLt
      omega
    · unfold cipherOffsetSpec cipherOffsetNatSpec
      rw [hlastChunkVal]
      simpa [cipherOffsetNatSpec] using hCipherLastLt
  rcases cipher_offset_success metadataFrameLen chunkSize lastChunk
      hCipherLastNo with ⟨lastStart, hlastStart, hlastStartVal⟩
  have hlastStartSpec :
      lastStart.val =
        cipherOffsetNatSpec metadataFrameLen chunkSize
          (lastChunkSpec chunkSize start len) := by
    rw [hlastStartVal]
    unfold cipherOffsetSpec cipherOffsetNatSpec
    rw [hlastChunkVal]
  have hStoredEndInput : lastStart.val + stride.val < 2 ^ 64 := by
    rw [hlastStartSpec, hstrideVal, tag_len_val]
    simpa [storedRangeEndSpec] using hStoredEndLt
  rcases checked_add_ok lastStart stride hStoredEndInput with
    ⟨storedEnd, hstoredEnd, hstoredEndVal⟩
  have hStoredStartLeEnd : storedStart.val ≤ storedEnd.val := by
    rw [hstoredStartSpec, hstoredEndVal, hlastStartSpec, hstrideVal,
      tag_len_val]
    unfold cipherOffsetNatSpec
    have hMulLe :
        firstChunkSpec chunkSize start * strideSpec chunkSize ≤
          lastChunkSpec chunkSize start len * strideSpec chunkSize :=
      Nat.mul_le_mul_right _ hFirstLeLast
    omega
  rcases checked_sub_ok storedEnd storedStart hStoredStartLeEnd with
    ⟨storedLen, hstoredLen, hstoredLenVal⟩
  have hStoredLenSpec :
      storedLen.val = storedRangeLenSpec metadataFrameLen chunkSize start len := by
    rw [hstoredLenVal, hstoredEndVal, hstoredStartSpec, hlastStartSpec,
      hstrideVal, tag_len_val]
    unfold storedRangeLenSpec storedRangeEndSpec
    rfl
  rcases u64_rem_ok_val start chunkSize hChunk with
    ⟨trimStart, htrimStart, htrimStartVal⟩
  refine ⟨{
    plaintext_end := endValue,
    first_chunk := firstChunk,
    last_chunk := lastChunk,
    fetched_chunk_count := fetchedCount,
    stored_range_start := storedStart,
    stored_range_len := storedLen,
    trim_start := trimStart
  }, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩
  · unfold nonempty_range_plan
    simp [hChunkNe, hvalidate, core.result.Result.Insts.CoreOpsTry.branch,
      hLenNe, hfirstChunk, hlastByte, hlastChunk, hfetchedMinusOne,
      hfetchedCount, hstride, hstoredStart, hlastStart, hstoredEnd,
      hstoredLen, htrimStart]
  · exact hendValue
  · exact hfirstChunkVal
  · exact hlastChunkVal
  · exact hFetchedCountSpec
  · exact hstoredStartSpec
  · exact hStoredLenSpec
  · rw [htrimStartVal]
    rfl

theorem inspect_geometry_success
    (storedSizeBytes metadataFrameLen chunkSize : Std.U64)
    (hno : InspectGeometryNoOverflow storedSizeBytes metadataFrameLen chunkSize) :
    ∃ geometry,
      inspect_geometry storedSizeBytes metadataFrameLen chunkSize =
        ok (.Ok geometry) ∧
      geometry.stride.val = strideSpec chunkSize ∧
      geometry.numerator.val =
        inspectNumeratorSpec storedSizeBytes metadataFrameLen ∧
      geometry.chunk_count.val =
        inspectChunkCountSpec storedSizeBytes metadataFrameLen chunkSize ∧
      geometry.plaintext_size.val =
        inspectPlaintextSizeSpec storedSizeBytes metadataFrameLen chunkSize ∧
      geometry.footer_offset.val =
        inspectFooterOffsetSpec storedSizeBytes metadataFrameLen chunkSize ∧
      geometry.expected_stored_size.val =
        inspectExpectedSizeSpec storedSizeBytes metadataFrameLen chunkSize ∧
      geometry.fill_len.val =
        inspectFillLenSpec storedSizeBytes metadataFrameLen chunkSize := by
  rcases hno with
    ⟨hChunk, hStoredMod, hMinimumLe, hStrideLt, hNumeratorLt,
      hChunkCountNz, hPlaintextLt, hChunkCountStrideLt, hHeaderMetaLt,
      hFooterOffLt, hFooterOffFooterLt, hExpectedLt, hExpectedEq, hFillLt⟩
  have hChunkNe := u64_ne_zero_of_val_ne_zero chunkSize hChunk
  rcases u64_rem_ok_val storedSizeBytes chunkSize hChunk with
    ⟨storedRem, hstoredRem, hstoredRemVal⟩
  have hstoredRemZero : storedRem = 0#u64 := by
    apply u64_eq_zero_of_val_zero
    rw [hstoredRemVal]
    exact hStoredMod
  have hHeaderFooterInput :
      RAO_HEADER_LEN.val + RAO_FOOTER_LEN.val < 2 ^ 64 := by
    rw [header_len_val, footer_len_val]
    norm_num [HeaderLen, FooterLen]
  rcases checked_add_ok RAO_HEADER_LEN RAO_FOOTER_LEN hHeaderFooterInput with
    ⟨headerFooter, hheaderFooter, hheaderFooterVal⟩
  have hHeaderFooterMetaInput :
      headerFooter.val + metadataFrameLen.val < 2 ^ 64 := by
    rw [hheaderFooterVal, header_len_val, footer_len_val]
    have hStoredLt := U64.lt_succ_max storedSizeBytes
    omega
  rcases checked_add_ok headerFooter metadataFrameLen
      hHeaderFooterMetaInput with
    ⟨minimumSize, hminimumSize, hminimumSizeVal⟩
  have hStoredNotLtMinVal : ¬ storedSizeBytes.val < minimumSize.val := by
    rw [hminimumSizeVal, hheaderFooterVal, header_len_val, footer_len_val]
    omega
  have hStoredNotLtMin : ¬ storedSizeBytes < minimumSize := by
    intro hlt
    apply hStoredNotLtMinVal
    scalar_tac
  have hStrideInput : chunkSize.val + CHACHA20POLY1305_TAG_LEN.val < 2 ^ 64 := by
    rw [tag_len_val]
    exact hStrideLt
  rcases checked_add_ok chunkSize CHACHA20POLY1305_TAG_LEN hStrideInput with
    ⟨stride, hstride, hstrideVal⟩
  have hHeaderFooterScalarInput :
      RAO_HEADER_LEN.val + RAO_FOOTER_LEN.val < 2 ^ 64 := hHeaderFooterInput
  rcases u64_add_ok_val RAO_HEADER_LEN RAO_FOOTER_LEN
      hHeaderFooterScalarInput with
    ⟨headerFooterScalar, hheaderFooterScalar, hheaderFooterScalarVal⟩
  have hHeaderFooterLeStored :
      headerFooterScalar.val ≤ storedSizeBytes.val := by
    rw [hheaderFooterScalarVal, header_len_val, footer_len_val]
    omega
  rcases u64_checked_sub_some_of_le storedSizeBytes headerFooterScalar
      hHeaderFooterLeStored with
    ⟨withoutFixed, hwithoutFixed, hwithoutFixedVal⟩
  have hMetadataLeWithoutFixed : metadataFrameLen.val ≤ withoutFixed.val := by
    rw [hwithoutFixedVal, hheaderFooterScalarVal, header_len_val,
      footer_len_val]
    omega
  rcases u64_checked_sub_some_of_le withoutFixed metadataFrameLen
      hMetadataLeWithoutFixed with
    ⟨numerator, hnumerator, hnumeratorVal⟩
  have hnumeratorSpec :
      numerator.val = inspectNumeratorSpec storedSizeBytes metadataFrameLen := by
    rw [hnumeratorVal, hwithoutFixedVal, hheaderFooterScalarVal, header_len_val,
      footer_len_val]
    unfold inspectNumeratorSpec
    rfl
  have hStrideNz : stride.val ≠ 0 := by
    rw [hstrideVal, tag_len_val]
    have hChunkNonneg : 0 ≤ chunkSize.val := Nat.zero_le _
    norm_num [TagLen]
  rcases u64_div_ok_val numerator stride hStrideNz with
    ⟨chunkCount, hchunkCount, hchunkCountValRaw⟩
  have hchunkCountVal :
      chunkCount.val =
        inspectChunkCountSpec storedSizeBytes metadataFrameLen chunkSize := by
    rw [hchunkCountValRaw, hnumeratorSpec, hstrideVal, tag_len_val]
    rfl
  have hChunkCountNe : chunkCount ≠ 0#u64 := by
    apply u64_ne_zero_of_val_ne_zero
    rw [hchunkCountVal]
    exact hChunkCountNz
  have hPlaintextInput : chunkCount.val * chunkSize.val < 2 ^ 64 := by
    rw [hchunkCountVal]
    exact hPlaintextLt
  rcases checked_mul_ok chunkCount chunkSize hPlaintextInput with
    ⟨plaintextSize, hplaintextSize, hplaintextSizeVal⟩
  have hPlaintextSpec :
      plaintextSize.val =
        inspectPlaintextSizeSpec storedSizeBytes metadataFrameLen chunkSize := by
    rw [hplaintextSizeVal, hchunkCountVal]
    rfl
  have hChunkCountStrideInput : chunkCount.val * stride.val < 2 ^ 64 := by
    rw [hchunkCountVal, hstrideVal, tag_len_val]
    exact hChunkCountStrideLt
  rcases checked_mul_ok chunkCount stride hChunkCountStrideInput with
    ⟨payloadSpan, hpayloadSpan, hpayloadSpanVal⟩
  have hpayloadSpanSpec :
      payloadSpan.val =
        inspectChunkCountSpec storedSizeBytes metadataFrameLen chunkSize *
          strideSpec chunkSize := by
    rw [hpayloadSpanVal, hchunkCountVal, hstrideVal, tag_len_val]
    rfl
  have hHeaderMetaInput : RAO_HEADER_LEN.val + metadataFrameLen.val < 2 ^ 64 := by
    rw [header_len_val]
    exact hHeaderMetaLt
  rcases checked_add_ok RAO_HEADER_LEN metadataFrameLen hHeaderMetaInput with
    ⟨headerMeta, hheaderMeta, hheaderMetaVal⟩
  have hFooterOffsetInput : headerMeta.val + payloadSpan.val < 2 ^ 64 := by
    rw [hheaderMetaVal, hpayloadSpanSpec, header_len_val]
    exact hFooterOffLt
  rcases checked_add_ok headerMeta payloadSpan hFooterOffsetInput with
    ⟨footerOffset, hfooterOffset, hfooterOffsetVal⟩
  have hfooterOffsetSpec :
      footerOffset.val =
        inspectFooterOffsetSpec storedSizeBytes metadataFrameLen chunkSize := by
    rw [hfooterOffsetVal, hheaderMetaVal, hpayloadSpanSpec, header_len_val]
    rfl
  have hFooterEndInput : footerOffset.val + RAO_FOOTER_LEN.val < 2 ^ 64 := by
    rw [hfooterOffsetSpec, footer_len_val]
    exact hFooterOffFooterLt
  rcases checked_add_ok footerOffset RAO_FOOTER_LEN hFooterEndInput with
    ⟨footerEnd, hfooterEnd, hfooterEndVal⟩
  have hfooterEndSpec :
      footerEnd.val =
        inspectFooterOffsetSpec storedSizeBytes metadataFrameLen chunkSize +
          FooterLen := by
    rw [hfooterEndVal, hfooterOffsetSpec, footer_len_val]
  have hRoundNo : roundUpSpec footerEnd chunkSize < 2 ^ 64 := by
    unfold roundUpSpec
    rw [hfooterEndSpec]
    change inspectExpectedSizeSpec storedSizeBytes metadataFrameLen chunkSize <
      2 ^ 64
    exact hExpectedLt
  rcases round_up_success footerEnd chunkSize hChunk hRoundNo with
    ⟨expectedSize, hexpectedSize, hexpectedSizeVal⟩
  have hexpectedSpec :
      expectedSize.val =
        inspectExpectedSizeSpec storedSizeBytes metadataFrameLen chunkSize := by
    rw [hexpectedSizeVal]
    unfold roundUpSpec inspectExpectedSizeSpec
    rw [hfooterEndSpec]
  have hExpectedEqU64 : expectedSize = storedSizeBytes := by
    apply UScalar.eq_imp
    rw [hexpectedSpec]
    exact hExpectedEq
  have hExpectedValEq : expectedSize.val = storedSizeBytes.val := by
    rw [hexpectedSpec, hExpectedEq]
  have hExpectedNe : ¬ expectedSize != storedSizeBytes := by
    simp [hExpectedEqU64]
  have hFooterEndLeExpected : footerEnd.val ≤ expectedSize.val := by
    rw [hexpectedSpec, hfooterEndSpec]
    unfold inspectExpectedSizeSpec
    by_cases hrem :
        (inspectFooterOffsetSpec storedSizeBytes metadataFrameLen chunkSize +
          FooterLen) % chunkSize.val = 0
    · simp [hrem]
    · simp [hrem]
  rcases checked_sub_ok expectedSize footerEnd hFooterEndLeExpected with
    ⟨fillLen, hfillLen, hfillLenVal⟩
  have hfillLenSpec :
      fillLen.val = inspectFillLenSpec storedSizeBytes metadataFrameLen chunkSize := by
    rw [hfillLenVal, hexpectedSpec, hfooterEndSpec]
    unfold inspectFillLenSpec
    rfl
  refine ⟨{
    stride := stride,
    numerator := numerator,
    chunk_count := chunkCount,
    plaintext_size := plaintextSize,
    footer_offset := footerOffset,
    expected_stored_size := expectedSize,
    fill_len := fillLen
  }, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩
  · unfold inspect_geometry
    simp [hChunkNe, hstoredRem, hstoredRemZero, hheaderFooter,
      core.result.Result.Insts.CoreOpsTry.branch, hminimumSize,
      hStoredNotLtMin, hStoredNotLtMinVal, hstride, hheaderFooterScalar, lift, hwithoutFixed,
      hnumerator, hchunkCount, hChunkCountNe, hplaintextSize, hpayloadSpan,
      hheaderMeta, hfooterOffset, hfooterEnd, hexpectedSize, hExpectedNe,
      hExpectedValEq, hfillLen]
  · rw [hstrideVal, tag_len_val]
    rfl
  · exact hnumeratorSpec
  · exact hchunkCountVal
  · exact hPlaintextSpec
  · exact hfooterOffsetSpec
  · exact hexpectedSpec
  · exact hfillLenSpec

end AeadFraming
