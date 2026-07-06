/- Specification theorems for the RAO manifest regular-file writer core
   extraction (SPEC.md M1-M5).

   This file targets the Aeneas-generated definitions in `RaoManifest.Funs`.
   It proves that a valid one-regular-file manifest validates, encodes to the
   required writer schema, and decodes back to the same manifest core. The Rust
   `drift_guard` test ties this proof-facing scalar extraction back to
   production `crates/remanence-format/src/{layout,manifest}.rs`.

   Scope: this proof models a small regular-file writer-schema core, not
   production CBOR bytes, `Vec`/`String`, UTF-8 decoding, tar/pax layout,
   hashing, xattrs, nonregular entries, global-pax cross-checking, or
   arbitrary-length manifest arrays. -/
import RaoManifest.Funs

open Aeneas Aeneas.Std Result

namespace RaoManifest

def ChunkGranularity : Nat := 512

def chunkCountSpec (sizeBytes : Std.U64) (chunkSize : Std.U64) : Nat :=
  if sizeBytes.val = 0 then
    0
  else
    (sizeBytes.val - 1) / chunkSize.val + 1

def RegularFileCoreValid (file : RegularFileCore) (chunkSize : Std.U64) : Prop :=
  chunkSize.val ≠ 0 ∧
  chunkSize.val % ChunkGranularity = 0 ∧
  file.path_id.val ≠ 0 ∧
  file.file_id.val ≠ 0 ∧
  file.executable_tag.val ≤ 2 ∧
  (file.size_bytes.val = 0 →
    file.first_chunk_lba_present = false ∧ file.first_chunk_lba.val = 0) ∧
  (file.size_bytes.val ≠ 0 → file.first_chunk_lba_present = true) ∧
  chunkCountSpec file.size_bytes chunkSize < 2 ^ 64

def ManifestCoreValid (manifest : ManifestCore) : Prop :=
  manifest.object_id.val ≠ 0 ∧
  RegularFileCoreValid manifest.file manifest.chunk_size

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

lemma checked_add_ok (a b : Std.U64) (h : a.val + b.val < 2 ^ 64) :
    ∃ sum, checked_add a b = ok (.Ok sum) ∧ sum.val = a.val + b.val := by
  rcases u64_checked_add_some_of_sum_lt a b h with ⟨sum, hadd, hval⟩
  refine ⟨sum, ?_, hval⟩
  unfold checked_add
  simp [lift, hadd]

theorem checked_add_rejects_overflow (a b : Std.U64)
    (h : 2 ^ 64 ≤ a.val + b.val) :
    checked_add a b = ok (.Err RaoManifestError.InvalidManifestField) := by
  have hadd := u64_checked_add_none_of_sum_ge a b h
  unfold checked_add
  simp [lift, hadd]

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

lemma granularity_val : CHUNK_SIZE_GRANULARITY.val = ChunkGranularity := by
  simp [CHUNK_SIZE_GRANULARITY, ChunkGranularity]

lemma executable_true_val : EXECUTABLE_TRUE.val = 2 := by
  simp [EXECUTABLE_TRUE]

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
      ok (.Err RaoManifestError.InvalidChunkSize) := by
  unfold validate_chunk_size
  simp

theorem validate_chunk_size_rejects_non_multiple (chunkSize : Std.U64)
    (hpos : chunkSize.val ≠ 0)
    (hgran : chunkSize.val % ChunkGranularity ≠ 0) :
    validate_chunk_size chunkSize =
      ok (.Err RaoManifestError.InvalidChunkSize) := by
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

theorem chunk_count_core_success (sizeBytes chunkSize : Std.U64)
    (hChunkNz : chunkSize.val ≠ 0)
    (hChunkGran : chunkSize.val % ChunkGranularity = 0)
    (hNoOverflow : chunkCountSpec sizeBytes chunkSize < 2 ^ 64) :
    ∃ count,
      chunk_count_core sizeBytes chunkSize = ok (.Ok count) ∧
      count.val = chunkCountSpec sizeBytes chunkSize := by
  have hChunk := validate_chunk_size_success chunkSize hChunkNz hChunkGran
  by_cases hSizeZero : sizeBytes.val = 0
  · have hSizeEq : sizeBytes = 0#u64 :=
      u64_eq_zero_of_val_zero sizeBytes hSizeZero
    refine ⟨0#u64, ?_, ?_⟩
    · unfold chunk_count_core
      simp [hChunk, core.result.Result.Insts.CoreOpsTry.branch, hSizeEq]
    · simp [chunkCountSpec, hSizeZero]
  · have hSizeNe : sizeBytes ≠ 0#u64 :=
      u64_ne_zero_of_val_ne_zero sizeBytes hSizeZero
    have hOneLe : (1#u64).val ≤ sizeBytes.val := by
      norm_num
      omega
    rcases u64_sub_ok_val sizeBytes 1#u64 hOneLe with
      ⟨minusOne, hsub, hminusVal⟩
    rcases u64_div_ok_val minusOne chunkSize hChunkNz with
      ⟨chunksMinusOne, hdiv, hchunksVal⟩
    have hAddInput : chunksMinusOne.val + (1#u64).val < 2 ^ 64 := by
      rw [hchunksVal, hminusVal]
      simp
      simpa [chunkCountSpec, hSizeZero] using hNoOverflow
    rcases checked_add_ok chunksMinusOne 1#u64 hAddInput with
      ⟨count, hcount, hcountVal⟩
    refine ⟨count, ?_, ?_⟩
    · unfold chunk_count_core
      simp [hChunk, core.result.Result.Insts.CoreOpsTry.branch, hSizeNe, hsub,
        hdiv, hcount]
    · rw [hcountVal, hchunksVal, hminusVal]
      simp [chunkCountSpec, hSizeZero]

theorem validate_regular_file_core_success (file : RegularFileCore)
    (chunkSize : Std.U64) (hvalid : RegularFileCoreValid file chunkSize) :
    validate_regular_file_core file chunkSize = ok (.Ok ()) := by
  rcases hvalid with
    ⟨hChunkNz, hChunkGran, hPathNz, hFileNz, hExec, hZeroShape,
      hNonzeroShape, hCountNoOverflow⟩
  have hChunk := validate_chunk_size_success chunkSize hChunkNz hChunkGran
  have hPathNe := u64_ne_zero_of_val_ne_zero file.path_id hPathNz
  have hFileNe := u64_ne_zero_of_val_ne_zero file.file_id hFileNz
  have hExecNotGt : ¬ file.executable_tag > EXECUTABLE_TRUE := by
    intro hgt
    have hv : EXECUTABLE_TRUE.val < file.executable_tag.val := by scalar_tac
    rw [executable_true_val] at hv
    omega
  rcases chunk_count_core_success file.size_bytes chunkSize hChunkNz hChunkGran
      hCountNoOverflow with
    ⟨count, hcount, _hcountVal⟩
  by_cases hSizeZero : file.size_bytes.val = 0
  · have hSizeEq : file.size_bytes = 0#u64 :=
      u64_eq_zero_of_val_zero file.size_bytes hSizeZero
    rcases hZeroShape hSizeZero with ⟨hPresent, hFirstZeroVal⟩
    have hFirstEq : file.first_chunk_lba = 0#u64 :=
      u64_eq_zero_of_val_zero file.first_chunk_lba hFirstZeroVal
    have hcountZero : chunk_count_core 0#u64 chunkSize = ok (.Ok count) := by
      simpa [hSizeEq] using hcount
    unfold validate_regular_file_core
    simp [hChunk, core.result.Result.Insts.CoreOpsTry.branch, hPathNe, hFileNe,
      hExecNotGt, hcountZero, hSizeEq, hPresent, hFirstEq]
  · have hSizeNe : file.size_bytes ≠ 0#u64 :=
      u64_ne_zero_of_val_ne_zero file.size_bytes hSizeZero
    have hPresent := hNonzeroShape hSizeZero
    unfold validate_regular_file_core
    simp [hChunk, core.result.Result.Insts.CoreOpsTry.branch, hPathNe, hFileNe,
      hExecNotGt, hcount, hSizeNe, hPresent]

theorem validate_regular_file_core_rejects_zero_path
    (file : RegularFileCore) (chunkSize : Std.U64)
    (hChunkNz : chunkSize.val ≠ 0)
    (hChunkGran : chunkSize.val % ChunkGranularity = 0)
    (hPath : file.path_id = 0#u64) :
    validate_regular_file_core file chunkSize =
      ok (.Err RaoManifestError.InvalidManifestField) := by
  have hChunk := validate_chunk_size_success chunkSize hChunkNz hChunkGran
  unfold validate_regular_file_core
  simp [hChunk, core.result.Result.Insts.CoreOpsTry.branch, hPath]

theorem validate_regular_file_core_rejects_bad_executable
    (file : RegularFileCore) (chunkSize : Std.U64)
    (hChunkNz : chunkSize.val ≠ 0)
    (hChunkGran : chunkSize.val % ChunkGranularity = 0)
    (hPathNz : file.path_id.val ≠ 0)
    (hFileNz : file.file_id.val ≠ 0)
    (hExec : 2 < file.executable_tag.val) :
    validate_regular_file_core file chunkSize =
      ok (.Err RaoManifestError.InvalidManifestField) := by
  have hChunk := validate_chunk_size_success chunkSize hChunkNz hChunkGran
  have hPathNe := u64_ne_zero_of_val_ne_zero file.path_id hPathNz
  have hFileNe := u64_ne_zero_of_val_ne_zero file.file_id hFileNz
  have hgt : file.executable_tag > EXECUTABLE_TRUE := by
    have hgt2 : file.executable_tag > 2#u8 := by scalar_tac
    simpa [EXECUTABLE_TRUE] using hgt2
  unfold validate_regular_file_core
  simp [hChunk, core.result.Result.Insts.CoreOpsTry.branch, hPathNe, hFileNe,
    hgt]

theorem validate_regular_file_core_rejects_zero_size_with_lba
    (file : RegularFileCore) (chunkSize : Std.U64)
    (hvalid : RegularFileCoreValid
      { file with first_chunk_lba_present := false, first_chunk_lba := 0#u64 }
      chunkSize)
    (hSize : file.size_bytes.val = 0)
    (hPresent : file.first_chunk_lba_present = true) :
    validate_regular_file_core file chunkSize =
      ok (.Err RaoManifestError.InvalidManifestField) := by
  rcases hvalid with
    ⟨hChunkNz, hChunkGran, hPathNz, hFileNz, hExec, _hZeroShape,
      _hNonzeroShape, hCountNoOverflow⟩
  have hChunk := validate_chunk_size_success chunkSize hChunkNz hChunkGran
  have hPathNz' : file.path_id.val ≠ 0 := by simpa using hPathNz
  have hFileNz' : file.file_id.val ≠ 0 := by simpa using hFileNz
  have hExec' : file.executable_tag.val ≤ 2 := by simpa using hExec
  have hPathNe := u64_ne_zero_of_val_ne_zero file.path_id hPathNz'
  have hFileNe := u64_ne_zero_of_val_ne_zero file.file_id hFileNz'
  have hExecNotGt : ¬ file.executable_tag > EXECUTABLE_TRUE := by
    intro hgt
    have hv : EXECUTABLE_TRUE.val < file.executable_tag.val := by scalar_tac
    rw [executable_true_val] at hv
    omega
  have hSizeEq : file.size_bytes = 0#u64 :=
    u64_eq_zero_of_val_zero file.size_bytes hSize
  have hCountNoOverflow' :
      chunkCountSpec file.size_bytes chunkSize < 2 ^ 64 := by
    simp [chunkCountSpec, hSize] at hCountNoOverflow ⊢
  rcases chunk_count_core_success file.size_bytes chunkSize hChunkNz hChunkGran
      hCountNoOverflow' with
    ⟨count, hcount, _⟩
  have hcountZero : chunk_count_core 0#u64 chunkSize = ok (.Ok count) := by
    simpa [hSizeEq] using hcount
  unfold validate_regular_file_core
  simp [hChunk, core.result.Result.Insts.CoreOpsTry.branch, hPathNe, hFileNe,
    hExecNotGt, hcountZero, hSizeEq, hPresent]

theorem validate_manifest_core_success (manifest : ManifestCore)
    (hvalid : ManifestCoreValid manifest) :
    validate_manifest_core manifest = ok (.Ok ()) := by
  rcases hvalid with ⟨hObjectNz, hFileValid⟩
  rcases hFileValid with
    ⟨hChunkNz, hChunkGran, hPathNz, hFileNz, hExec, hZeroShape,
      hNonzeroShape, hCountNoOverflow⟩
  have hChunk := validate_chunk_size_success manifest.chunk_size hChunkNz hChunkGran
  have hObjectNe := u64_ne_zero_of_val_ne_zero manifest.object_id hObjectNz
  have hFileValidate := validate_regular_file_core_success manifest.file
    manifest.chunk_size
    ⟨hChunkNz, hChunkGran, hPathNz, hFileNz, hExec, hZeroShape,
      hNonzeroShape, hCountNoOverflow⟩
  unfold validate_manifest_core
  simp [hChunk, core.result.Result.Insts.CoreOpsTry.branch, hObjectNe,
    hFileValidate]

theorem encode_regular_file_core_emits_writer_schema (file : RegularFileCore)
    (chunkSize : Std.U64) (hvalid : RegularFileCoreValid file chunkSize) :
    ∃ wire,
      encode_regular_file_core file chunkSize = ok (.Ok wire) ∧
      wire.map_len = FILE_ENTRY_REGULAR_MAP_LEN ∧
      wire.key_path = FILE_KEY_PATH ∧
      wire.path_id = file.path_id ∧
      wire.key_file_id = FILE_KEY_FILE_ID ∧
      wire.file_id = file.file_id ∧
      wire.key_executable = FILE_KEY_EXECUTABLE ∧
      wire.executable_tag = file.executable_tag ∧
      wire.key_size_bytes = FILE_KEY_SIZE_BYTES ∧
      wire.size_bytes = file.size_bytes ∧
      wire.key_chunk_count = FILE_KEY_CHUNK_COUNT ∧
      wire.chunk_count.val = chunkCountSpec file.size_bytes chunkSize ∧
      wire.key_file_sha256 = FILE_KEY_FILE_SHA256 ∧
      wire.file_sha256_len = DIGEST_BYTE_LEN ∧
      wire.file_sha256 = file.file_sha256 ∧
      wire.key_first_chunk_lba = FILE_KEY_FIRST_CHUNK_LBA ∧
      wire.first_chunk_lba_is_null = (¬ file.first_chunk_lba_present) ∧
      wire.first_chunk_lba = file.first_chunk_lba ∧
      wire.key_metadata_preservation_data =
        FILE_KEY_METADATA_PRESERVATION_DATA ∧
      wire.metadata_preservation_data_empty = true := by
  rcases hvalid with
    ⟨hChunkNz, hChunkGran, hPathNz, hFileNz, hExec, hZeroShape,
      hNonzeroShape, hCountNoOverflow⟩
  have hvalid' : RegularFileCoreValid file chunkSize :=
    ⟨hChunkNz, hChunkGran, hPathNz, hFileNz, hExec, hZeroShape,
      hNonzeroShape, hCountNoOverflow⟩
  have hvalidate := validate_regular_file_core_success file chunkSize hvalid'
  rcases chunk_count_core_success file.size_bytes chunkSize hChunkNz hChunkGran
      hCountNoOverflow with
    ⟨count, hcount, hcountVal⟩
  unfold encode_regular_file_core
  simp [hvalidate, core.result.Result.Insts.CoreOpsTry.branch, hcount,
    hcountVal]

theorem encode_manifest_core_emits_writer_schema (manifest : ManifestCore)
    (hvalid : ManifestCoreValid manifest) :
    ∃ wire,
      encode_manifest_core manifest = ok (.Ok wire) ∧
      wire.root_map_len = ROOT_MAP_LEN ∧
      wire.key_object_id = ROOT_KEY_OBJECT_ID ∧
      wire.object_id = manifest.object_id ∧
      wire.key_chunk_size = ROOT_KEY_CHUNK_SIZE ∧
      wire.chunk_size = manifest.chunk_size ∧
      wire.key_file_entries = ROOT_KEY_FILE_ENTRIES ∧
      wire.file_entries_len = FILE_ENTRIES_LEN_ONE ∧
      wire.file.map_len = FILE_ENTRY_REGULAR_MAP_LEN ∧
      wire.key_schema_version = ROOT_KEY_SCHEMA_VERSION ∧
      wire.schema_version = SCHEMA_VERSION ∧
      wire.key_object_metadata = ROOT_KEY_OBJECT_METADATA ∧
      wire.object_metadata_empty = true ∧
      wire.key_caller_object_id = ROOT_KEY_CALLER_OBJECT_ID ∧
      wire.caller_object_id = manifest.caller_object_id ∧
      wire.key_external_references = ROOT_KEY_EXTERNAL_REFERENCES ∧
      wire.external_references_empty = true ∧
      wire.trailing_data = false := by
  rcases hvalid with ⟨hObjectNz, hFileValid⟩
  have hvalid' : ManifestCoreValid manifest := ⟨hObjectNz, hFileValid⟩
  have hvalidate := validate_manifest_core_success manifest hvalid'
  rcases encode_regular_file_core_emits_writer_schema manifest.file
      manifest.chunk_size hFileValid with
    ⟨fileWire, hfileEncode, hfileMap, _⟩
  unfold encode_manifest_core
  simp [hvalidate, core.result.Result.Insts.CoreOpsTry.branch, hfileEncode,
    hfileMap]

theorem decode_encode_regular_file_core_round_trip (file : RegularFileCore)
    (chunkSize : Std.U64) (hvalid : RegularFileCoreValid file chunkSize) :
    ∃ wire,
      encode_regular_file_core file chunkSize = ok (.Ok wire) ∧
      decode_regular_file_core wire chunkSize = ok (.Ok file) := by
  rcases hvalid with
    ⟨hChunkNz, hChunkGran, hPathNz, hFileNz, hExec, hZeroShape,
      hNonzeroShape, hCountNoOverflow⟩
  have hvalid' : RegularFileCoreValid file chunkSize :=
    ⟨hChunkNz, hChunkGran, hPathNz, hFileNz, hExec, hZeroShape,
      hNonzeroShape, hCountNoOverflow⟩
  have hvalidate := validate_regular_file_core_success file chunkSize hvalid'
  rcases chunk_count_core_success file.size_bytes chunkSize hChunkNz hChunkGran
      hCountNoOverflow with
    ⟨count, hcount, hcountVal⟩
  refine ⟨{
    map_len := FILE_ENTRY_REGULAR_MAP_LEN,
    key_path := FILE_KEY_PATH,
    path_id := file.path_id,
    key_file_id := FILE_KEY_FILE_ID,
    file_id := file.file_id,
    key_executable := FILE_KEY_EXECUTABLE,
    executable_tag := file.executable_tag,
    key_size_bytes := FILE_KEY_SIZE_BYTES,
    size_bytes := file.size_bytes,
    key_chunk_count := FILE_KEY_CHUNK_COUNT,
    chunk_count := count,
    key_file_sha256 := FILE_KEY_FILE_SHA256,
    file_sha256_len := DIGEST_BYTE_LEN,
    file_sha256 := file.file_sha256,
    key_first_chunk_lba := FILE_KEY_FIRST_CHUNK_LBA,
    first_chunk_lba_is_null := (¬ file.first_chunk_lba_present),
    first_chunk_lba := file.first_chunk_lba,
    key_metadata_preservation_data := FILE_KEY_METADATA_PRESERVATION_DATA,
    metadata_preservation_data_empty := true
  }, ?_, ?_⟩
  · unfold encode_regular_file_core
    simp [hvalidate, core.result.Result.Insts.CoreOpsTry.branch, hcount]
  · by_cases hSizeZero : file.size_bytes.val = 0
    · have hSizeEq : file.size_bytes = 0#u64 :=
        u64_eq_zero_of_val_zero file.size_bytes hSizeZero
      rcases hZeroShape hSizeZero with ⟨hPresent, hFirstZeroVal⟩
      have hFirstEq : file.first_chunk_lba = 0#u64 :=
        u64_eq_zero_of_val_zero file.first_chunk_lba hFirstZeroVal
      have hcountZero : chunk_count_core 0#u64 chunkSize = ok (.Ok count) := by
        simpa [hSizeEq] using hcount
      have hfileZero :
          {
            path_id := file.path_id,
            file_id := file.file_id,
            size_bytes := 0#u64,
            file_sha256 := file.file_sha256,
            first_chunk_lba_present := false,
            first_chunk_lba := 0#u64,
            executable_tag := file.executable_tag
          } = file := by
        cases file
        simp_all
      have hvalidateZero :
          validate_regular_file_core
            {
              path_id := file.path_id,
              file_id := file.file_id,
              size_bytes := 0#u64,
              file_sha256 := file.file_sha256,
              first_chunk_lba_present := false,
              first_chunk_lba := 0#u64,
              executable_tag := file.executable_tag
            } chunkSize = ok (.Ok ()) := by
        rw [hfileZero]
        exact hvalidate
      unfold decode_regular_file_core
      simp [hcountZero, hcountVal, hSizeEq, hPresent, hfileZero, hvalidate,
        core.result.Result.Insts.CoreOpsTry.branch]
    · have hSizeNe : file.size_bytes ≠ 0#u64 :=
        u64_ne_zero_of_val_ne_zero file.size_bytes hSizeZero
      have hPresent := hNonzeroShape hSizeZero
      have hfilePresent :
          {
            path_id := file.path_id,
            file_id := file.file_id,
            size_bytes := file.size_bytes,
            file_sha256 := file.file_sha256,
            first_chunk_lba_present := true,
            first_chunk_lba := file.first_chunk_lba,
            executable_tag := file.executable_tag
          } = file := by
        cases file
        simp_all
      have hvalidatePresent :
          validate_regular_file_core
            {
              path_id := file.path_id,
              file_id := file.file_id,
              size_bytes := file.size_bytes,
              file_sha256 := file.file_sha256,
              first_chunk_lba_present := true,
              first_chunk_lba := file.first_chunk_lba,
              executable_tag := file.executable_tag
            } chunkSize = ok (.Ok ()) := by
        rw [hfilePresent]
        exact hvalidate
      unfold decode_regular_file_core
      simp [hcount, hcountVal, hSizeNe, hPresent, hfilePresent,
        hvalidate,
        core.result.Result.Insts.CoreOpsTry.branch]

theorem decode_encode_manifest_core_round_trip (manifest : ManifestCore)
    (hvalid : ManifestCoreValid manifest) :
    ∃ wire,
      encode_manifest_core manifest = ok (.Ok wire) ∧
      decode_manifest_core wire manifest.chunk_size = ok (.Ok manifest) := by
  rcases hvalid with ⟨hObjectNz, hFileValid⟩
  have hvalid' : ManifestCoreValid manifest := ⟨hObjectNz, hFileValid⟩
  have hvalidate := validate_manifest_core_success manifest hvalid'
  rcases decode_encode_regular_file_core_round_trip manifest.file
      manifest.chunk_size hFileValid with
    ⟨fileWire, hfileEncode, hfileDecode⟩
  refine ⟨{
    root_map_len := ROOT_MAP_LEN,
    key_object_id := ROOT_KEY_OBJECT_ID,
    object_id := manifest.object_id,
    key_chunk_size := ROOT_KEY_CHUNK_SIZE,
    chunk_size := manifest.chunk_size,
    key_file_entries := ROOT_KEY_FILE_ENTRIES,
    file_entries_len := FILE_ENTRIES_LEN_ONE,
    file := fileWire,
    key_schema_version := ROOT_KEY_SCHEMA_VERSION,
    schema_version := SCHEMA_VERSION,
    key_object_metadata := ROOT_KEY_OBJECT_METADATA,
    object_metadata_empty := true,
    key_caller_object_id := ROOT_KEY_CALLER_OBJECT_ID,
    caller_object_id := manifest.caller_object_id,
    key_external_references := ROOT_KEY_EXTERNAL_REFERENCES,
    external_references_empty := true,
    trailing_data := false
  }, ?_, ?_⟩
  · unfold encode_manifest_core
    simp [hvalidate, core.result.Result.Insts.CoreOpsTry.branch, hfileEncode]
  · unfold decode_manifest_core
    simp [hfileDecode, hvalidate, core.result.Result.Insts.CoreOpsTry.branch]

theorem decode_manifest_core_rejects_trailing_data
    (wire : ManifestWireCore) (readerChunkSize : Std.U64)
    (h : wire.trailing_data = true) :
    decode_manifest_core wire readerChunkSize =
      ok (.Err RaoManifestError.InvalidCborEncoding) := by
  unfold decode_manifest_core
  simp [h]

theorem decode_manifest_core_rejects_missing_required_shape
    (wire : ManifestWireCore) (readerChunkSize : Std.U64)
    (htrail : wire.trailing_data = false)
    (hlen : wire.root_map_len ≠ ROOT_MAP_LEN) :
    decode_manifest_core wire readerChunkSize =
      ok (.Err RaoManifestError.MissingRequiredManifestField) := by
  unfold decode_manifest_core
  simp [htrail, hlen]

theorem decode_manifest_core_rejects_wrong_root_key
    (wire : ManifestWireCore) (readerChunkSize : Std.U64)
    (htrail : wire.trailing_data = false)
    (hlen : wire.root_map_len = ROOT_MAP_LEN)
    (hkey : wire.key_object_id ≠ ROOT_KEY_OBJECT_ID) :
    decode_manifest_core wire readerChunkSize =
      ok (.Err RaoManifestError.InvalidCborEncoding) := by
  unfold decode_manifest_core
  simp [htrail, hlen, hkey]

theorem decode_manifest_core_rejects_wrong_schema_version
    (wire : ManifestWireCore) (readerChunkSize : Std.U64)
    (htrail : wire.trailing_data = false)
    (hlen : wire.root_map_len = ROOT_MAP_LEN)
    (hkey0 : wire.key_object_id = ROOT_KEY_OBJECT_ID)
    (hkey1 : wire.key_chunk_size = ROOT_KEY_CHUNK_SIZE)
    (hkey2 : wire.key_file_entries = ROOT_KEY_FILE_ENTRIES)
    (hkey3 : wire.key_schema_version = ROOT_KEY_SCHEMA_VERSION)
    (hkey4 : wire.key_object_metadata = ROOT_KEY_OBJECT_METADATA)
    (hkey5 : wire.key_caller_object_id = ROOT_KEY_CALLER_OBJECT_ID)
    (hkey6 : wire.key_external_references = ROOT_KEY_EXTERNAL_REFERENCES)
    (hversion : wire.schema_version ≠ SCHEMA_VERSION) :
    decode_manifest_core wire readerChunkSize =
      ok (.Err RaoManifestError.InvalidManifestField) := by
  unfold decode_manifest_core
  simp [htrail, hlen, hkey0, hkey1, hkey2, hkey3, hkey4, hkey5, hkey6,
    hversion]

theorem decode_manifest_core_rejects_chunk_size_mismatch
    (wire : ManifestWireCore) (readerChunkSize : Std.U64)
    (htrail : wire.trailing_data = false)
    (hlen : wire.root_map_len = ROOT_MAP_LEN)
    (hkey0 : wire.key_object_id = ROOT_KEY_OBJECT_ID)
    (hkey1 : wire.key_chunk_size = ROOT_KEY_CHUNK_SIZE)
    (hkey2 : wire.key_file_entries = ROOT_KEY_FILE_ENTRIES)
    (hkey3 : wire.key_schema_version = ROOT_KEY_SCHEMA_VERSION)
    (hkey4 : wire.key_object_metadata = ROOT_KEY_OBJECT_METADATA)
    (hkey5 : wire.key_caller_object_id = ROOT_KEY_CALLER_OBJECT_ID)
    (hkey6 : wire.key_external_references = ROOT_KEY_EXTERNAL_REFERENCES)
    (hversion : wire.schema_version = SCHEMA_VERSION)
    (hchunk : wire.chunk_size ≠ readerChunkSize) :
    decode_manifest_core wire readerChunkSize =
      ok (.Err RaoManifestError.InvalidManifestField) := by
  unfold decode_manifest_core
  simp [htrail, hlen, hkey0, hkey1, hkey2, hkey3, hkey4, hkey5, hkey6,
    hversion, hchunk]

theorem decode_manifest_core_rejects_wrong_file_count
    (wire : ManifestWireCore) (readerChunkSize : Std.U64)
    (htrail : wire.trailing_data = false)
    (hlen : wire.root_map_len = ROOT_MAP_LEN)
    (hkey0 : wire.key_object_id = ROOT_KEY_OBJECT_ID)
    (hkey1 : wire.key_chunk_size = ROOT_KEY_CHUNK_SIZE)
    (hkey2 : wire.key_file_entries = ROOT_KEY_FILE_ENTRIES)
    (hkey3 : wire.key_schema_version = ROOT_KEY_SCHEMA_VERSION)
    (hkey4 : wire.key_object_metadata = ROOT_KEY_OBJECT_METADATA)
    (hkey5 : wire.key_caller_object_id = ROOT_KEY_CALLER_OBJECT_ID)
    (hkey6 : wire.key_external_references = ROOT_KEY_EXTERNAL_REFERENCES)
    (hversion : wire.schema_version = SCHEMA_VERSION)
    (hchunk : wire.chunk_size = readerChunkSize)
    (hcount : wire.file_entries_len ≠ FILE_ENTRIES_LEN_ONE) :
    decode_manifest_core wire readerChunkSize =
      ok (.Err RaoManifestError.InvalidManifestField) := by
  unfold decode_manifest_core
  simp [htrail, hlen, hkey0, hkey1, hkey2, hkey3, hkey4, hkey5, hkey6,
    hversion, hchunk, hcount]

theorem decode_regular_file_core_rejects_wrong_file_shape
    (wire : RegularFileWireCore) (chunkSize : Std.U64)
    (hlen : wire.map_len ≠ FILE_ENTRY_REGULAR_MAP_LEN) :
    decode_regular_file_core wire chunkSize =
      ok (.Err RaoManifestError.MissingRequiredManifestField) := by
  unfold decode_regular_file_core
  simp [hlen]

theorem decode_regular_file_core_rejects_wrong_digest_len
    (wire : RegularFileWireCore) (chunkSize : Std.U64)
    (hlen : wire.map_len = FILE_ENTRY_REGULAR_MAP_LEN)
    (hkey0 : wire.key_path = FILE_KEY_PATH)
    (hkey1 : wire.key_file_id = FILE_KEY_FILE_ID)
    (hkey2 : wire.key_executable = FILE_KEY_EXECUTABLE)
    (hkey3 : wire.key_size_bytes = FILE_KEY_SIZE_BYTES)
    (hkey4 : wire.key_chunk_count = FILE_KEY_CHUNK_COUNT)
    (hkey5 : wire.key_file_sha256 = FILE_KEY_FILE_SHA256)
    (hkey6 : wire.key_first_chunk_lba = FILE_KEY_FIRST_CHUNK_LBA)
    (hkey7 : wire.key_metadata_preservation_data =
      FILE_KEY_METADATA_PRESERVATION_DATA)
    (hdigest : wire.file_sha256_len ≠ DIGEST_BYTE_LEN) :
    decode_regular_file_core wire chunkSize =
      ok (.Err RaoManifestError.InvalidManifestField) := by
  unfold decode_regular_file_core
  simp [hlen, hkey0, hkey1, hkey2, hkey3, hkey4, hkey5, hkey6, hkey7, hdigest]

theorem decode_regular_file_core_rejects_wrong_chunk_count
    (wire : RegularFileWireCore) (chunkSize expected : Std.U64)
    (hshape :
      wire.map_len = FILE_ENTRY_REGULAR_MAP_LEN ∧
      wire.key_path = FILE_KEY_PATH ∧
      wire.key_file_id = FILE_KEY_FILE_ID ∧
      wire.key_executable = FILE_KEY_EXECUTABLE ∧
      wire.key_size_bytes = FILE_KEY_SIZE_BYTES ∧
      wire.key_chunk_count = FILE_KEY_CHUNK_COUNT ∧
      wire.key_file_sha256 = FILE_KEY_FILE_SHA256 ∧
      wire.key_first_chunk_lba = FILE_KEY_FIRST_CHUNK_LBA ∧
      wire.key_metadata_preservation_data =
        FILE_KEY_METADATA_PRESERVATION_DATA ∧
      wire.file_sha256_len = DIGEST_BYTE_LEN ∧
      wire.metadata_preservation_data_empty = true)
    (hcount : chunk_count_core wire.size_bytes chunkSize = ok (.Ok expected))
    (hbad : wire.chunk_count ≠ expected) :
    decode_regular_file_core wire chunkSize =
      ok (.Err RaoManifestError.InvalidManifestField) := by
  rcases hshape with
    ⟨hlen, hkey0, hkey1, hkey2, hkey3, hkey4, hkey5, hkey6, hkey7,
      hdigest, hmetadata⟩
  have hbadVal : wire.chunk_count.val ≠ expected.val := by
    intro hv
    apply hbad
    apply UScalar.eq_imp
    exact hv
  unfold decode_regular_file_core
  simp [hlen, hkey0, hkey1, hkey2, hkey3, hkey4, hkey5, hkey6, hkey7,
    hdigest, hmetadata, hcount, core.result.Result.Insts.CoreOpsTry.branch,
    hbadVal]

theorem decode_regular_file_core_rejects_zero_size_nonnull_lba
    (wire : RegularFileWireCore) (chunkSize expected : Std.U64)
    (hshape :
      wire.map_len = FILE_ENTRY_REGULAR_MAP_LEN ∧
      wire.key_path = FILE_KEY_PATH ∧
      wire.key_file_id = FILE_KEY_FILE_ID ∧
      wire.key_executable = FILE_KEY_EXECUTABLE ∧
      wire.key_size_bytes = FILE_KEY_SIZE_BYTES ∧
      wire.key_chunk_count = FILE_KEY_CHUNK_COUNT ∧
      wire.key_file_sha256 = FILE_KEY_FILE_SHA256 ∧
      wire.key_first_chunk_lba = FILE_KEY_FIRST_CHUNK_LBA ∧
      wire.key_metadata_preservation_data =
        FILE_KEY_METADATA_PRESERVATION_DATA ∧
      wire.file_sha256_len = DIGEST_BYTE_LEN ∧
      wire.metadata_preservation_data_empty = true)
    (hcount : chunk_count_core wire.size_bytes chunkSize = ok (.Ok expected))
    (hcountEq : wire.chunk_count = expected)
    (hsize : wire.size_bytes = 0#u64)
    (hlba : wire.first_chunk_lba_is_null = false) :
    decode_regular_file_core wire chunkSize =
      ok (.Err RaoManifestError.InvalidManifestField) := by
  rcases hshape with
    ⟨hlen, hkey0, hkey1, hkey2, hkey3, hkey4, hkey5, hkey6, hkey7,
      hdigest, hmetadata⟩
  have hcountZero : chunk_count_core 0#u64 chunkSize = ok (.Ok expected) := by
    simpa [hsize] using hcount
  unfold decode_regular_file_core
  simp [hlen, hkey0, hkey1, hkey2, hkey3, hkey4, hkey5, hkey6, hkey7,
    hdigest, hmetadata, hcountZero, core.result.Result.Insts.CoreOpsTry.branch,
    hcountEq, hsize, hlba]

end RaoManifest
