/- Specification theorems for the RAO header scalar-layout extraction
   (SPEC.md H1-H5).

   This is the first incremental RAO format-correctness proof. It targets the
   Aeneas-generated definitions in `RaoHeader.Funs` and proves that the
   proof-facing header core validates, serializes to canonical frozen fields,
   and parses back to the same core. The Rust `drift_guard` test ties this
   scalar extraction back to production `crates/remanence-aead/src/header.rs`.

   Scope: this proof does not model the exact 128-byte array, object-id UTF-8
   reconstruction, SHA-256 header hash, allocation, encryption, or CBOR. -/
import RaoHeader.Funs

open Aeneas Aeneas.Std Result

namespace RaoHeader

def HeaderCoreValid (header : HeaderCore) : Prop :=
  header.chunk_size.val ≠ 0 ∧
  header.chunk_size.val % 512 = 0 ∧
  header.key_id_nonzero = true ∧
  header.hkdf_salt_nonzero = true ∧
  17 ≤ header.metadata_frame_len.val ∧
  header.metadata_frame_len.val ≤ 16777216 ∧
  header.object_id_field_valid = true

lemma u32_rem_ok_val (x y : Std.U32) (hy : y.val ≠ 0) :
    ∃ z, x % y = ok z ∧ z.val = x.val % y.val := by
  have hspec := U32.rem_spec x (y := y) hy
  cases hrem : x % y with
  | ok z =>
      simp [hrem] at hspec
      exact ⟨z, rfl, hspec⟩
  | fail e =>
      simp [hrem] at hspec
  | div =>
      simp [hrem] at hspec

lemma u32_eq_zero_of_val_zero (x : Std.U32) (h : x.val = 0) : x = 0#u32 := by
  apply UScalar.eq_imp
  simpa using h

lemma u32_ne_zero_of_val_ne_zero (x : Std.U32) (h : x.val ≠ 0) :
    x ≠ 0#u32 := by
  intro hx
  apply h
  rw [hx]
  rfl

theorem validate_chunk_size_success (chunkSize : Std.U32)
    (hnz : chunkSize.val ≠ 0)
    (hmultiple : chunkSize.val % 512 = 0) :
    validate_chunk_size chunkSize = ok (.Ok ()) := by
  rcases u32_rem_ok_val chunkSize 512#u32 (by decide) with ⟨rem, hrem, hremVal⟩
  have hchunk_ne : chunkSize ≠ 0#u32 := u32_ne_zero_of_val_ne_zero chunkSize hnz
  have hrem_zero : rem = 0#u32 := by
    apply u32_eq_zero_of_val_zero
    rw [hremVal]
    simpa using hmultiple
  unfold validate_chunk_size CHUNK_SIZE_GRANULARITY
  simp [hchunk_ne, hrem, hrem_zero]

theorem validate_chunk_size_rejects_zero :
    validate_chunk_size 0#u32 = ok (.Err RaoHeaderError.InvalidChunkSize) := by
  unfold validate_chunk_size
  simp

theorem validate_chunk_size_rejects_non_multiple (chunkSize : Std.U32)
    (hnz : chunkSize.val ≠ 0)
    (hnonmultiple : chunkSize.val % 512 ≠ 0) :
    validate_chunk_size chunkSize = ok (.Err RaoHeaderError.InvalidChunkSize) := by
  rcases u32_rem_ok_val chunkSize 512#u32 (by decide) with ⟨rem, hrem, hremVal⟩
  have hchunk_ne : chunkSize ≠ 0#u32 := u32_ne_zero_of_val_ne_zero chunkSize hnz
  have hrem_val_ne_zero : rem.val ≠ 0 := by
    intro hzv
    apply hnonmultiple
    rw [hremVal] at hzv
    simpa using hzv
  unfold validate_chunk_size CHUNK_SIZE_GRANULARITY
  simp [hchunk_ne, hrem, hrem_val_ne_zero]

theorem validate_metadata_frame_len_success (metadataFrameLen : Std.U64)
    (hmin : 17 ≤ metadataFrameLen.val)
    (hmax : metadataFrameLen.val ≤ 16777216) :
    validate_metadata_frame_len metadataFrameLen = ok (.Ok ()) := by
  have hnot_min : ¬ metadataFrameLen < 17#u64 := by
    intro hlt
    have hv : metadataFrameLen.val < (17#u64).val := by scalar_tac
    norm_num at hv
    omega
  have hnot_max : ¬ metadataFrameLen > 16777216#u64 := by
    intro hgt
    have hv : (16777216#u64).val < metadataFrameLen.val := by scalar_tac
    norm_num at hv
    omega
  unfold validate_metadata_frame_len RAO_METADATA_FRAME_MIN_LEN
    RAO_MAX_METADATA_FRAME_LEN
  simp [hnot_min, hnot_max]

theorem validate_metadata_frame_len_rejects_too_small (metadataFrameLen : Std.U64)
    (hsmall : metadataFrameLen.val < 17) :
    validate_metadata_frame_len metadataFrameLen =
      ok (.Err RaoHeaderError.MetadataFrameLengthInvalid) := by
  have hlt : metadataFrameLen < 17#u64 := by scalar_tac
  unfold validate_metadata_frame_len RAO_METADATA_FRAME_MIN_LEN
  simp [hlt]

theorem validate_metadata_frame_len_rejects_too_large (metadataFrameLen : Std.U64)
    (hlarge : 16777216 < metadataFrameLen.val) :
    validate_metadata_frame_len metadataFrameLen =
      ok (.Err RaoHeaderError.MetadataFrameLengthInvalid) := by
  have hnot_min : ¬ metadataFrameLen < 17#u64 := by
    intro hlt
    have hv : metadataFrameLen.val < (17#u64).val := by scalar_tac
    norm_num at hv
    omega
  have hgt : metadataFrameLen > 16777216#u64 := by scalar_tac
  unfold validate_metadata_frame_len RAO_METADATA_FRAME_MIN_LEN
    RAO_MAX_METADATA_FRAME_LEN
  simp [hnot_min, hgt]

theorem validate_header_core_success (header : HeaderCore)
    (hvalid : HeaderCoreValid header) :
    validate_header_core header = ok (.Ok ()) := by
  rcases hvalid with
    ⟨hchunkNz, hchunkMultiple, hkey, hsalt, hmetaMin, hmetaMax, hobj⟩
  have hchunk := validate_chunk_size_success header.chunk_size hchunkNz hchunkMultiple
  have hmeta :=
    validate_metadata_frame_len_success header.metadata_frame_len hmetaMin hmetaMax
  unfold validate_header_core
  simp [hchunk, core.result.Result.Insts.CoreOpsTry.branch, hkey, hsalt,
    hmeta, hobj]

theorem validate_header_core_rejects_zero_key (header : HeaderCore)
    (hchunkNz : header.chunk_size.val ≠ 0)
    (hchunkMultiple : header.chunk_size.val % 512 = 0)
    (hkey : header.key_id_nonzero = false) :
    validate_header_core header =
      ok (.Err RaoHeaderError.InvalidKeyIdentifier) := by
  have hchunk := validate_chunk_size_success header.chunk_size hchunkNz hchunkMultiple
  unfold validate_header_core
  simp [hchunk, core.result.Result.Insts.CoreOpsTry.branch, hkey]

theorem validate_header_core_rejects_zero_salt (header : HeaderCore)
    (hchunkNz : header.chunk_size.val ≠ 0)
    (hchunkMultiple : header.chunk_size.val % 512 = 0)
    (hkey : header.key_id_nonzero = true)
    (hsalt : header.hkdf_salt_nonzero = false) :
    validate_header_core header = ok (.Err RaoHeaderError.InvalidSalt) := by
  have hchunk := validate_chunk_size_success header.chunk_size hchunkNz hchunkMultiple
  unfold validate_header_core
  simp [hchunk, core.result.Result.Insts.CoreOpsTry.branch, hkey, hsalt]

theorem validate_header_core_rejects_bad_object_id (header : HeaderCore)
    (hchunkNz : header.chunk_size.val ≠ 0)
    (hchunkMultiple : header.chunk_size.val % 512 = 0)
    (hkey : header.key_id_nonzero = true)
    (hsalt : header.hkdf_salt_nonzero = true)
    (hmetaMin : 17 ≤ header.metadata_frame_len.val)
    (hmetaMax : header.metadata_frame_len.val ≤ 16777216)
    (hobj : header.object_id_field_valid = false) :
    validate_header_core header =
      ok (.Err RaoHeaderError.InvalidObjectIdField) := by
  have hchunk := validate_chunk_size_success header.chunk_size hchunkNz hchunkMultiple
  have hmeta :=
    validate_metadata_frame_len_success header.metadata_frame_len hmetaMin hmetaMax
  unfold validate_header_core
  simp [hchunk, core.result.Result.Insts.CoreOpsTry.branch, hkey, hsalt,
    hmeta, hobj]

theorem serialize_header_core_emits_frozen_fields (header : HeaderCore)
    (hvalid : HeaderCoreValid header) :
    ∃ wire,
      serialize_header_core header = ok (.Ok wire) ∧
      wire.magic_rao1 = true ∧
      wire.header_len = RAO_HEADER_LEN_U16 ∧
      wire.format_version = FORMAT_VERSION ∧
      wire.suite_id = SUITE_ID_HKDF_SHA256_CHACHA20POLY1305 ∧
      wire.flags = 0#u32 ∧
      wire.reserved_0x38_0x40_zero = true ∧
      wire.chunk_size = header.chunk_size ∧
      wire.metadata_frame_len = header.metadata_frame_len ∧
      wire.key_id_nonzero = header.key_id_nonzero ∧
      wire.hkdf_salt_nonzero = header.hkdf_salt_nonzero ∧
      wire.object_id_field_valid = header.object_id_field_valid := by
  have hvalidate := validate_header_core_success header hvalid
  unfold serialize_header_core
  simp [hvalidate, core.result.Result.Insts.CoreOpsTry.branch]

theorem parse_serialize_header_core_round_trip (header : HeaderCore)
    (hvalid : HeaderCoreValid header) :
    ∃ wire,
      serialize_header_core header = ok (.Ok wire) ∧
      parse_header_core wire = ok (.Ok header) := by
  rcases serialize_header_core_emits_frozen_fields header hvalid with
    ⟨wire, hserialize, hmagic, hlen, hversion, hsuite, hflags, hreserved,
      hchunk, hmeta, hkey, hsalt, hobj⟩
  have hvalidate := validate_header_core_success header hvalid
  refine ⟨wire, hserialize, ?_⟩
  unfold parse_header_core
  simp [hmagic, hlen, hversion, hsuite, hflags, hreserved, hchunk, hmeta, hkey,
    hsalt, hobj, hvalidate, core.result.Result.Insts.CoreOpsTry.branch]

theorem parse_header_core_rejects_bad_magic (wire : HeaderWire)
    (h : wire.magic_rao1 = false) :
    parse_header_core wire = ok (.Err RaoHeaderError.InvalidMagicBytes) := by
  unfold parse_header_core
  simp [h]

theorem parse_header_core_rejects_bad_header_len (wire : HeaderWire)
    (hmagic : wire.magic_rao1 = true)
    (hlen : wire.header_len ≠ RAO_HEADER_LEN_U16) :
    parse_header_core wire = ok (.Err RaoHeaderError.InvalidHeaderLength) := by
  unfold parse_header_core
  simp [hmagic, hlen]

theorem parse_header_core_rejects_bad_version (wire : HeaderWire)
    (hmagic : wire.magic_rao1 = true)
    (hlen : wire.header_len = RAO_HEADER_LEN_U16)
    (hversion : wire.format_version ≠ FORMAT_VERSION) :
    parse_header_core wire = ok (.Err RaoHeaderError.UnsupportedFormatVersion) := by
  unfold parse_header_core
  simp [hmagic, hlen, hversion]

theorem parse_header_core_rejects_bad_suite (wire : HeaderWire)
    (hmagic : wire.magic_rao1 = true)
    (hlen : wire.header_len = RAO_HEADER_LEN_U16)
    (hversion : wire.format_version = FORMAT_VERSION)
    (hsuite : wire.suite_id ≠ SUITE_ID_HKDF_SHA256_CHACHA20POLY1305) :
    parse_header_core wire = ok (.Err RaoHeaderError.InvalidSuite) := by
  unfold parse_header_core
  simp [hmagic, hlen, hversion, hsuite]

theorem parse_header_core_rejects_nonzero_flags (wire : HeaderWire)
    (hmagic : wire.magic_rao1 = true)
    (hlen : wire.header_len = RAO_HEADER_LEN_U16)
    (hversion : wire.format_version = FORMAT_VERSION)
    (hsuite : wire.suite_id = SUITE_ID_HKDF_SHA256_CHACHA20POLY1305)
    (hflags : wire.flags ≠ 0#u32) :
    parse_header_core wire = ok (.Err RaoHeaderError.ReservedBytesNotZero) := by
  unfold parse_header_core
  simp [hmagic, hlen, hversion, hsuite, hflags]

theorem parse_header_core_rejects_nonzero_reserved_tail (wire : HeaderWire)
    (hmagic : wire.magic_rao1 = true)
    (hlen : wire.header_len = RAO_HEADER_LEN_U16)
    (hversion : wire.format_version = FORMAT_VERSION)
    (hsuite : wire.suite_id = SUITE_ID_HKDF_SHA256_CHACHA20POLY1305)
    (hflags : wire.flags = 0#u32)
    (hreserved : wire.reserved_0x38_0x40_zero = false) :
    parse_header_core wire = ok (.Err RaoHeaderError.ReservedBytesNotZero) := by
  unfold parse_header_core
  simp [hmagic, hlen, hversion, hsuite, hflags, hreserved]

end RaoHeader
