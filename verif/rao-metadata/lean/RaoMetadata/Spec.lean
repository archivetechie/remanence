/- Specification theorems for the RAO metadata deterministic-CBOR writer core
   extraction (SPEC.md M1-M5).

   This file targets the Aeneas-generated definitions in `RaoMetadata.Funs`.
   It proves that valid metadata validates, encodes to the required v1 writer
   schema, and decodes back to the same metadata core. The Rust `drift_guard`
   test ties this proof-facing scalar extraction back to production
   `crates/remanence-aead/src/metadata.rs`.

   Scope: this proof models the required writer-schema core, not production
   `Vec<u8>` construction, UTF-8 text decoding, exact digest byte copying,
   extension-key skipping, recursive CBOR parsing, encryption, or hashing. -/
import RaoMetadata.Funs

open Aeneas Aeneas.Std Result

namespace RaoMetadata

def TagLen : Nat := 16
def ChunkGranularity : Nat := 512

def chunkCountSpec (metadata : MetadataCore) (chunkSize : Std.U64) : Nat :=
  metadata.plaintext_size.val / chunkSize.val

def tagBytesSpec (metadata : MetadataCore) (chunkSize : Std.U64) : Nat :=
  TagLen * chunkCountSpec metadata chunkSize

def MetadataCoreValid (metadata : MetadataCore) (chunkSize : Std.U64) : Prop :=
  chunkSize.val ≠ 0 ∧
  chunkSize.val % ChunkGranularity = 0 ∧
  metadata.plaintext_size.val ≠ 0 ∧
  metadata.plaintext_size.val % chunkSize.val = 0 ∧
  tagBytesSpec metadata chunkSize < 2 ^ 64 ∧
  metadata.plaintext_size.val + tagBytesSpec metadata chunkSize < 2 ^ 64

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

lemma u64_checked_mul_none_of_prod_ge (a b : Std.U64)
    (h : 2 ^ 64 ≤ a.val * b.val) :
    U64.checked_mul a b = none := by
  have hspec := U64.checked_mul_bv_spec a b
  cases hmul : U64.checked_mul a b with
  | none => rfl
  | some product =>
      simp [hmul, U64.max, U64.numBits] at hspec
      omega

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

theorem checked_add_rejects_overflow (a b : Std.U64)
    (h : 2 ^ 64 ≤ a.val + b.val) :
    checked_add a b = ok (.Err RaoMetadataError.InvalidMetadataField) := by
  have hadd := u64_checked_add_none_of_sum_ge a b h
  unfold checked_add
  simp [lift, hadd]

theorem checked_mul_rejects_overflow (a b : Std.U64)
    (h : 2 ^ 64 ≤ a.val * b.val) :
    checked_mul a b = ok (.Err RaoMetadataError.InvalidMetadataField) := by
  have hmul := u64_checked_mul_none_of_prod_ge a b h
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

lemma tag_len_val : CHACHA20POLY1305_TAG_LEN.val = TagLen := by
  simp [CHACHA20POLY1305_TAG_LEN, TagLen]

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
      ok (.Err RaoMetadataError.InvalidChunkSize) := by
  unfold validate_chunk_size
  simp

theorem validate_chunk_size_rejects_non_multiple (chunkSize : Std.U64)
    (hpos : chunkSize.val ≠ 0)
    (hgran : chunkSize.val % ChunkGranularity ≠ 0) :
    validate_chunk_size chunkSize =
      ok (.Err RaoMetadataError.InvalidChunkSize) := by
  have hChunkNe := u64_ne_zero_of_val_ne_zero chunkSize hpos
  have hGranNz : CHUNK_SIZE_GRANULARITY.val ≠ 0 := by
    simp [CHUNK_SIZE_GRANULARITY]
  rcases u64_rem_ok_val chunkSize CHUNK_SIZE_GRANULARITY hGranNz with
    ⟨rem, hrem, hremVal⟩
  have hremValNe : rem.val ≠ 0 := by
    intro hz
    apply hgran
    rw [hremVal, granularity_val] at hz
    simpa using hz
  unfold validate_chunk_size
  simp [hChunkNe, hrem, hremValNe]

theorem validate_metadata_core_success (metadata : MetadataCore)
    (chunkSize : Std.U64) (hvalid : MetadataCoreValid metadata chunkSize) :
    validate_metadata_core metadata chunkSize = ok (.Ok ()) := by
  rcases hvalid with
    ⟨hChunkNz, hChunkGran, hPlainNz, hAligned, hTagNo, hPayloadNo⟩
  have hChunk := validate_chunk_size_success chunkSize hChunkNz hChunkGran
  have hPlainNe := u64_ne_zero_of_val_ne_zero metadata.plaintext_size hPlainNz
  rcases u64_rem_ok_val metadata.plaintext_size chunkSize hChunkNz with
    ⟨rem, hrem, hremVal⟩
  have hremZero : rem = 0#u64 := by
    apply u64_eq_zero_of_val_zero
    rw [hremVal]
    exact hAligned
  rcases u64_div_ok_val metadata.plaintext_size chunkSize hChunkNz with
    ⟨chunks, hdiv, hchunksVal⟩
  have hTagInput : CHACHA20POLY1305_TAG_LEN.val * chunks.val < 2 ^ 64 := by
    rw [tag_len_val, hchunksVal]
    exact hTagNo
  rcases checked_mul_ok CHACHA20POLY1305_TAG_LEN chunks hTagInput with
    ⟨tagBytes, htagBytes, htagBytesVal⟩
  have hPayloadInput : metadata.plaintext_size.val + tagBytes.val < 2 ^ 64 := by
    rw [htagBytesVal, tag_len_val, hchunksVal]
    exact hPayloadNo
  rcases checked_add_ok metadata.plaintext_size tagBytes hPayloadInput with
    ⟨payloadLen, hpayloadLen, _hpayloadLenVal⟩
  unfold validate_metadata_core
  simp [hChunk, core.result.Result.Insts.CoreOpsTry.branch, hPlainNe, hrem,
    hremZero, hdiv, htagBytes, hpayloadLen]

theorem validate_metadata_core_rejects_zero_plaintext
    (metadata : MetadataCore) (chunkSize : Std.U64)
    (hChunkNz : chunkSize.val ≠ 0)
    (hChunkGran : chunkSize.val % ChunkGranularity = 0)
    (hPlain : metadata.plaintext_size = 0#u64) :
    validate_metadata_core metadata chunkSize =
      ok (.Err RaoMetadataError.InvalidMetadataField) := by
  have hChunk := validate_chunk_size_success chunkSize hChunkNz hChunkGran
  unfold validate_metadata_core
  simp [hChunk, core.result.Result.Insts.CoreOpsTry.branch, hPlain]

theorem validate_metadata_core_rejects_unaligned_plaintext
    (metadata : MetadataCore) (chunkSize : Std.U64)
    (hChunkNz : chunkSize.val ≠ 0)
    (hChunkGran : chunkSize.val % ChunkGranularity = 0)
    (hPlainNz : metadata.plaintext_size.val ≠ 0)
    (hUnaligned : metadata.plaintext_size.val % chunkSize.val ≠ 0) :
    validate_metadata_core metadata chunkSize =
      ok (.Err RaoMetadataError.InvalidMetadataField) := by
  have hChunk := validate_chunk_size_success chunkSize hChunkNz hChunkGran
  have hPlainNe := u64_ne_zero_of_val_ne_zero metadata.plaintext_size hPlainNz
  rcases u64_rem_ok_val metadata.plaintext_size chunkSize hChunkNz with
    ⟨rem, hrem, hremVal⟩
  have hremValNe : rem.val ≠ 0 := by
    intro hz
    apply hUnaligned
    rw [hremVal] at hz
    simpa using hz
  unfold validate_metadata_core
  simp [hChunk, core.result.Result.Insts.CoreOpsTry.branch, hPlainNe, hrem,
    hremValNe]

theorem encode_metadata_core_emits_writer_schema (metadata : MetadataCore)
    (chunkSize : Std.U64) (hvalid : MetadataCoreValid metadata chunkSize) :
    ∃ wire,
      encode_metadata_core metadata chunkSize = ok (.Ok wire) ∧
      wire.map_len = REQUIRED_MAP_LEN ∧
      wire.key_metadata_version = KEY_METADATA_VERSION ∧
      wire.metadata_version = METADATA_VERSION ∧
      wire.key_plaintext_size = KEY_PLAINTEXT_SIZE ∧
      wire.plaintext_size = metadata.plaintext_size ∧
      wire.key_plaintext_digest_alg = KEY_PLAINTEXT_DIGEST_ALG ∧
      wire.digest_alg_sha256 = true ∧
      wire.key_plaintext_digest = KEY_PLAINTEXT_DIGEST ∧
      wire.digest_byte_len = DIGEST_BYTE_LEN ∧
      wire.digest = metadata.digest ∧
      wire.trailing_data = false := by
  have hvalidate := validate_metadata_core_success metadata chunkSize hvalid
  unfold encode_metadata_core
  simp [hvalidate, core.result.Result.Insts.CoreOpsTry.branch]

theorem decode_encode_metadata_core_round_trip (metadata : MetadataCore)
    (chunkSize : Std.U64) (hvalid : MetadataCoreValid metadata chunkSize) :
    ∃ wire,
      encode_metadata_core metadata chunkSize = ok (.Ok wire) ∧
      decode_metadata_core wire chunkSize = ok (.Ok metadata) := by
  rcases encode_metadata_core_emits_writer_schema metadata chunkSize hvalid with
    ⟨wire, hencode, hmap, hkey0, hversion, hkey1, hplain, hkey2, halg, hkey3,
      hdigestLen, hdigest, htrail⟩
  have hvalidate := validate_metadata_core_success metadata chunkSize hvalid
  refine ⟨wire, hencode, ?_⟩
  unfold decode_metadata_core
  simp [htrail, hmap, hkey0, hkey1, hkey2, hkey3, hversion, halg, hdigestLen,
    hplain, hdigest, hvalidate, core.result.Result.Insts.CoreOpsTry.branch]

theorem decode_metadata_core_rejects_trailing_data
    (wire : MetadataCborCore) (chunkSize : Std.U64)
    (h : wire.trailing_data = true) :
    decode_metadata_core wire chunkSize =
      ok (.Err RaoMetadataError.InvalidCborEncoding) := by
  unfold decode_metadata_core
  simp [h]

theorem decode_metadata_core_rejects_missing_required_shape
    (wire : MetadataCborCore) (chunkSize : Std.U64)
    (htrail : wire.trailing_data = false)
    (hlen : wire.map_len ≠ REQUIRED_MAP_LEN) :
    decode_metadata_core wire chunkSize =
      ok (.Err RaoMetadataError.MissingRequiredMetadataField) := by
  unfold decode_metadata_core
  simp [htrail, hlen]

theorem decode_metadata_core_rejects_wrong_required_key
    (wire : MetadataCborCore) (chunkSize : Std.U64)
    (htrail : wire.trailing_data = false)
    (hlen : wire.map_len = REQUIRED_MAP_LEN)
    (hkey : wire.key_metadata_version ≠ KEY_METADATA_VERSION) :
    decode_metadata_core wire chunkSize =
      ok (.Err RaoMetadataError.InvalidCborEncoding) := by
  unfold decode_metadata_core
  simp [htrail, hlen, hkey]

theorem decode_metadata_core_rejects_wrong_version
    (wire : MetadataCborCore) (chunkSize : Std.U64)
    (htrail : wire.trailing_data = false)
    (hlen : wire.map_len = REQUIRED_MAP_LEN)
    (hkey0 : wire.key_metadata_version = KEY_METADATA_VERSION)
    (hkey1 : wire.key_plaintext_size = KEY_PLAINTEXT_SIZE)
    (hkey2 : wire.key_plaintext_digest_alg = KEY_PLAINTEXT_DIGEST_ALG)
    (hkey3 : wire.key_plaintext_digest = KEY_PLAINTEXT_DIGEST)
    (hversion : wire.metadata_version ≠ METADATA_VERSION) :
    decode_metadata_core wire chunkSize =
      ok (.Err RaoMetadataError.InvalidMetadataField) := by
  unfold decode_metadata_core
  simp [htrail, hlen, hkey0, hkey1, hkey2, hkey3, hversion]

theorem decode_metadata_core_rejects_wrong_digest_algorithm
    (wire : MetadataCborCore) (chunkSize : Std.U64)
    (htrail : wire.trailing_data = false)
    (hlen : wire.map_len = REQUIRED_MAP_LEN)
    (hkey0 : wire.key_metadata_version = KEY_METADATA_VERSION)
    (hkey1 : wire.key_plaintext_size = KEY_PLAINTEXT_SIZE)
    (hkey2 : wire.key_plaintext_digest_alg = KEY_PLAINTEXT_DIGEST_ALG)
    (hkey3 : wire.key_plaintext_digest = KEY_PLAINTEXT_DIGEST)
    (hversion : wire.metadata_version = METADATA_VERSION)
    (halg : wire.digest_alg_sha256 = false) :
    decode_metadata_core wire chunkSize =
      ok (.Err RaoMetadataError.InvalidMetadataField) := by
  unfold decode_metadata_core
  simp [htrail, hlen, hkey0, hkey1, hkey2, hkey3, hversion, halg]

theorem decode_metadata_core_rejects_wrong_digest_len
    (wire : MetadataCborCore) (chunkSize : Std.U64)
    (htrail : wire.trailing_data = false)
    (hlen : wire.map_len = REQUIRED_MAP_LEN)
    (hkey0 : wire.key_metadata_version = KEY_METADATA_VERSION)
    (hkey1 : wire.key_plaintext_size = KEY_PLAINTEXT_SIZE)
    (hkey2 : wire.key_plaintext_digest_alg = KEY_PLAINTEXT_DIGEST_ALG)
    (hkey3 : wire.key_plaintext_digest = KEY_PLAINTEXT_DIGEST)
    (hversion : wire.metadata_version = METADATA_VERSION)
    (halg : wire.digest_alg_sha256 = true)
    (hdigestLen : wire.digest_byte_len ≠ DIGEST_BYTE_LEN) :
    decode_metadata_core wire chunkSize =
      ok (.Err RaoMetadataError.InvalidMetadataField) := by
  unfold decode_metadata_core
  simp [htrail, hlen, hkey0, hkey1, hkey2, hkey3, hversion, halg, hdigestLen]

end RaoMetadata
