/- Specification theorem for the RAO archive composition core (SPEC.md A1).

   This proof targets the Aeneas-generated definitions in `RaoArchive.Funs`.
   It proves that the proof-facing archive core validates, encodes into the
   frozen top-level header/metadata/manifest writer shape, and decodes back to
   the same archive core.

   Scope: this proof composes scalar/fixed-capacity RAO component fields and
   cross-component consistency checks. It does not model exact CBOR bytes,
   arbitrary `Vec`/`String` traversal, tar/pax records, hashing, encryption,
   allocation, or IO. -/
import RaoArchive.Funs

open Aeneas Aeneas.Std Result

namespace RaoArchive

/-- Archive-level round trip for the generated Rust extraction.

    For every archive accepted by `validate_archive_core`, the deterministic
    writer shape emitted by `encode_archive_core` is accepted by
    `decode_archive_core` and reconstructs the same archive. -/
theorem decode_encode_archive_core_round_trip
    (archive : ArchiveCore)
    (hvalid : validate_archive_core archive = ok (.Ok ())) :
    ∃ wire,
      encode_archive_core archive = ok (.Ok wire) ∧
      decode_archive_core wire = ok (.Ok archive) := by
  refine ⟨{
    header := {
      magic_rao1 := true,
      header_len := RAO_HEADER_LEN_U16,
      format_version := FORMAT_VERSION,
      suite_id := SUITE_ID_HKDF_SHA256_CHACHA20POLY1305,
      object_id := archive.header.object_id,
      chunk_size := archive.header.chunk_size,
      flags := 0#u32,
      key_id_nonzero := archive.header.key_id_nonzero,
      hkdf_salt_nonzero := archive.header.hkdf_salt_nonzero,
      metadata_frame_len := archive.header.metadata_frame_len,
      reserved_zero := true
    },
    metadata := {
      map_len := METADATA_REQUIRED_MAP_LEN,
      metadata_version := METADATA_VERSION,
      plaintext_size := archive.metadata.plaintext_size,
      digest_alg_sha256 := true,
      digest_byte_len := DIGEST_BYTE_LEN,
      digest := archive.metadata.digest,
      trailing_data := false
    },
    manifest := {
      root_map_len := MANIFEST_ROOT_MAP_LEN,
      object_id := archive.manifest.object_id,
      caller_object_id := archive.manifest.caller_object_id,
      chunk_size := archive.manifest.chunk_size,
      file_entries_len := MANIFEST_FILE_ENTRIES_LEN,
      schema_version := MANIFEST_SCHEMA_VERSION,
      object_metadata_empty := true,
      external_references_empty := true,
      nonempty_regular_path_id := archive.manifest.nonempty_regular_path_id,
      nonempty_regular_file_id := archive.manifest.nonempty_regular_file_id,
      nonempty_regular_size_bytes :=
        archive.manifest.nonempty_regular_size_bytes,
      nonempty_regular_sha256 := archive.manifest.nonempty_regular_sha256,
      empty_regular_path_id := archive.manifest.empty_regular_path_id,
      empty_regular_file_id := archive.manifest.empty_regular_file_id,
      hardlink_path_id := archive.manifest.hardlink_path_id,
      hardlink_file_id := archive.manifest.hardlink_file_id,
      hardlink_target_path_id := archive.manifest.hardlink_target_path_id,
      symlink_path_id := archive.manifest.symlink_path_id,
      symlink_file_id := archive.manifest.symlink_file_id,
      symlink_target_id := archive.manifest.symlink_target_id,
      directory_path_id := archive.manifest.directory_path_id,
      directory_file_id := archive.manifest.directory_file_id,
      trailing_data := false
    }
  }, ?_, ?_⟩
  · unfold encode_archive_core
    simp [hvalid, core.result.Result.Insts.CoreOpsTry.branch]
  · unfold decode_archive_core
    simp [hvalid, core.result.Result.Insts.CoreOpsTry.branch]

end RaoArchive
