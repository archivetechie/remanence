-- Genuine schema-23 creation DDL reconstructed from index.rs and
-- tape_log/projection.rs at 1cc53692 (MINIMUM_SCHEMA plus migration additions).
-- Includes migrate_standalone_evidence's nullable readiness coordinates.
-- Kept independent of the current schema: tapes has no unprotected_since.

create table if not exists schema_meta(
  key text primary key,
  value blob not null
);

create table if not exists ingested_sources(
  source_kind text not null,
  source_id text not null,
  offset_bytes integer not null,
  terminal_hash blob,
  updated_at_utc text not null,
  primary key(source_kind, source_id)
);

create table if not exists tapes(
  tape_uuid blob primary key,
  voltag text,
  pool_id text,
  assignment_generation integer not null default 0,
  kind text not null default 'data',
  cleaning_uses integer,
  cleaning_state text,
  block_size integer,
  scheme_id text,
  data_blocks_per_stripe integer,
  parity_blocks_per_stripe integer,
  stripes_per_neighborhood integer,
  highest_protected_ordinal blob not null default X'0000000000000000'
    check(typeof(highest_protected_ordinal) = 'blob' and length(highest_protected_ordinal) = 8),
  total_committed_ordinals blob not null default X'0000000000000000'
    check(typeof(total_committed_ordinals) = 'blob' and length(total_committed_ordinals) = 8),
  last_committed_tape_file blob
    check(last_committed_tape_file is null or (typeof(last_committed_tape_file) = 'blob' and length(last_committed_tape_file) = 8)),
  written_extent_lba blob
    check(written_extent_lba is null or (typeof(written_extent_lba) = 'blob' and length(written_extent_lba) = 8)),
  finalization_progress text,
  finalization_trigger text,
  finalization_operation_id text,
  finalization_edition_digest blob,
  finalization_layout_digest blob,
  completed_replicas integer,
  finalization_outcome text,
  state text not null,
  updated_at_utc text not null
);

create unique index if not exists tapes_voltag_unique
  on tapes(voltag)
  where voltag is not null;

create table if not exists tape_pools(
  pool_id text primary key,
  display_name text,
  copy_class text,
  content_class text,
  created_at_utc text not null
);

create table if not exists tape_files(
  tape_uuid blob not null,
  tape_file_number blob not null
    check(typeof(tape_file_number) = 'blob' and length(tape_file_number) = 8),
  kind text not null,
  block_count blob not null
    check(typeof(block_count) = 'blob' and length(block_count) = 8),
  physical_start_hint blob
    check(physical_start_hint is null or (typeof(physical_start_hint) = 'blob' and length(physical_start_hint) = 8)),
  object_id text,
  first_parity_data_ordinal blob
    check(first_parity_data_ordinal is null or (typeof(first_parity_data_ordinal) = 'blob' and length(first_parity_data_ordinal) = 8)),
  epoch_id blob
    check(epoch_id is null or (typeof(epoch_id) = 'blob' and length(epoch_id) = 8)),
  protected_ordinal_start blob
    check(protected_ordinal_start is null or (typeof(protected_ordinal_start) = 'blob' and length(protected_ordinal_start) = 8)),
  protected_ordinal_end_exclusive blob
    check(protected_ordinal_end_exclusive is null or (typeof(protected_ordinal_end_exclusive) = 'blob' and length(protected_ordinal_end_exclusive) = 8)),
  canonical_metadata_hash blob,
  canonical_metadata_hash_algorithm text,
  bundle_uuid text,
  bundle_kind text,
  primary key(tape_uuid, tape_file_number)
);

create table if not exists objects(
  object_id text primary key,
  caller_object_id text,
  body_format text,
  logical_size_bytes integer,
  content_hash blob,
  content_hash_algorithm text,
  metadata_hash blob,
  metadata_hash_algorithm text,
  created_at_utc text not null
);

create index if not exists objects_content_hash_idx
  on objects(content_hash)
  where content_hash is not null;

create index if not exists objects_caller_object_id_idx
  on objects(caller_object_id)
  where caller_object_id is not null;

create table if not exists object_copies(
  object_id text not null,
  tape_uuid blob not null,
  tape_file_number blob not null
    check(typeof(tape_file_number) = 'blob' and length(tape_file_number) = 8),
  first_body_lba blob not null default X'0000000000000000'
    check(typeof(first_body_lba) = 'blob' and length(first_body_lba) = 8),
  first_parity_data_ordinal blob
    check(first_parity_data_ordinal is null or (typeof(first_parity_data_ordinal) = 'blob' and length(first_parity_data_ordinal) = 8)),
  protected_until_ordinal blob
    check(protected_until_ordinal is null or (typeof(protected_until_ordinal) = 'blob' and length(protected_until_ordinal) = 8)),
  status text not null,
  representation text not null default 'unknown',
  recipient_epoch_ids text,
  metadata_frame_len blob
    check(metadata_frame_len is null or (typeof(metadata_frame_len) = 'blob' and length(metadata_frame_len) = 8)),
  plaintext_digest blob,
  plaintext_digest_algorithm text,
  stored_digest blob,
  stored_digest_algorithm text,
  pool_id text,
  primary key(object_id, tape_uuid, tape_file_number)
);

create table if not exists object_files(
  object_id text not null,
  file_id text not null,
  path text not null,
  size_bytes blob not null
    check(typeof(size_bytes) = 'blob' and length(size_bytes) = 8),
  file_sha256 blob not null,
  file_digest_algorithm text not null default 'sha256',
  first_chunk_lba blob
    check(first_chunk_lba is null or (typeof(first_chunk_lba) = 'blob' and length(first_chunk_lba) = 8)),
  chunk_count blob not null
    check(typeof(chunk_count) = 'blob' and length(chunk_count) = 8),
  mtime text,
  executable integer,
  primary key(object_id, file_id)
);

create index if not exists object_files_by_path_idx on object_files(path);

create index if not exists object_files_path_idx
  on object_files(object_id, path);

create table if not exists catalog_units(
  unit_id text primary key,
  tape_uuid blob not null,
  origin_kind text not null,
  format_id text not null,
  native_object_id text,
  scan_id text,
  source_kind text,
  source_id text,
  confidence text,
  entry_count integer,
  damage_event_count integer,
  last_scan_at_utc text,
  adapter_state blob not null default X'',
  created_at_utc text not null
);

create index if not exists catalog_units_tape_origin_idx
  on catalog_units(tape_uuid, origin_kind);

create index if not exists catalog_units_native_object_idx
  on catalog_units(native_object_id);

create table if not exists idempotency_keys(
  actor_fingerprint text not null,
  operation_kind text not null,
  idempotency_key text not null,
  request_fingerprint blob not null,
  operation_id text not null,
  terminal_state text,
  response_fingerprint blob,
  updated_at_utc text not null,
  primary key(actor_fingerprint, operation_kind, idempotency_key)
);

create table if not exists operations(
  operation_id text primary key,
  operation_kind text not null,
  state text not null,
  session_id text,
  subject text,
  error_summary text,
  started_at_utc text not null,
  updated_at_utc text not null
);

create table if not exists media_readiness_ops(
  operation_id text primary key,
  run_id text,
  library_serial text,
  changer_sg text,
  drive_element integer,
  drive_sg text,
  drive_serial text,
  barcode text,
  source_slot integer,
  media_generation integer,
  phase text not null,
  state text not null,
  dirty_scope text,
  started_at_utc text not null,
  updated_at_utc text not null,
  deadline_at_utc text,
  last_cdb_opcode integer,
  last_sense_raw text,
  last_sense_key integer,
  last_asc integer,
  last_ascq integer,
  last_host_status integer,
  last_driver_status integer,
  target_status integer,
  transport_class text,
  cancel_source text,
  signal text,
  evidence_path text,
  last_error_json text,
  quarantine_id text
);

create index if not exists media_readiness_ops_active_idx
  on media_readiness_ops(library_serial, drive_element, state, updated_at_utc)
  where state not in ('ready', 'released')
    and coalesce(dirty_scope, 'drive+tape') <> 'none';

create index if not exists media_readiness_ops_barcode_idx
  on media_readiness_ops(library_serial, barcode)
  where barcode is not null;

create table if not exists media_readiness_transitions(
  transition_id integer primary key,
  operation_id text not null references media_readiness_ops(operation_id),
  phase text,
  state text not null,
  dirty_scope text,
  at_utc text not null,
  last_cdb_opcode integer,
  last_sense_raw text,
  last_sense_key integer,
  last_asc integer,
  last_ascq integer,
  last_host_status integer,
  last_driver_status integer,
  target_status integer,
  transport_class text,
  cancel_source text,
  signal text,
  evidence_path text,
  last_error_json text,
  quarantine_id text
);

create index if not exists media_readiness_transitions_op_idx
  on media_readiness_transitions(operation_id, transition_id);

create table if not exists tape_io_fences(
  fence_id integer primary key,
  tape_uuid blob not null,
  barcode text,
  state text not null,
  reason text not null,
  evidence_json text,
  quarantine_id text not null unique,
  created_at_utc text not null,
  updated_at_utc text not null,
  release_ack text
);

create index if not exists tape_io_fences_active_tape_idx
  on tape_io_fences(tape_uuid, state, updated_at_utc)
  where state = 'active';

create index if not exists tape_io_fences_active_barcode_idx
  on tape_io_fences(barcode, state, updated_at_utc)
  where state = 'active' and barcode is not null;

create table if not exists wrap_maps(
  tape_uuid blob primary key,
  descriptors_json text not null,
  mapped_extent_lba integer not null,
  write_epoch integer not null,
  calibration_generation integer not null,
  harvested_at_utc text not null
);

create table if not exists sessions(
  session_id text primary key,
  session_kind text not null,
  tape_uuid blob,
  library_serial text,
  drive_bay integer,
  drive_uuid blob,
  state text not null,
  opened_at_utc text not null,
  updated_at_utc text not null
);

create table if not exists drives(
  drive_uuid blob primary key,
  serial text,
  identity_source text not null,
  actionable integer not null default 1,
  vendor text, product text, firmware_rev text,
  managed text not null,
  state text not null default 'active',
  cleaning_due text not null default 'none',
  fenced integer not null default 0,
  first_seen_utc text not null, last_seen_utc text not null,
  last_library_serial text, last_element_address integer,
  purchase_date text, warranty_until text,
  cost text,
  notes text,
  retired_at_utc text, retire_reason text
);

create table if not exists drive_events(
  event_id integer primary key,
  drive_uuid blob not null references drives(drive_uuid),
  event_kind text not null,
  at_utc text not null,
  library_serial text, element_address integer,
  tape_uuid blob, detail text
);

create table if not exists drive_health_snapshots(
  snapshot_id integer primary key,
  drive_uuid blob not null references drives(drive_uuid),
  at_utc text not null,
  trigger text not null,
  session_id text,
  tape_alert_flags text,
  write_errors_corrected integer, write_errors_uncorrected integer,
  read_errors_corrected integer, read_errors_uncorrected integer,
  raw_pages text,
  unique(session_id, trigger)
);

create table if not exists clean_runs(
  run_id text primary key,
  drive_uuid blob not null, library_serial text not null,
  cart_tape_uuid blob, cart_home_slot integer,
  phase text not null,
  trigger text not null,
  started_at_utc text not null, updated_at_utc text not null,
  detail text
);

create unique index if not exists clean_runs_one_active_per_drive
  on clean_runs(drive_uuid)
  where phase not in ('done','failed','needs-operator');

create unique index if not exists clean_runs_one_active_per_cart
  on clean_runs(cart_tape_uuid)
  where phase not in ('done','failed','needs-operator')
    and cart_tape_uuid is not null;

create table if not exists alarms(
  alarm_id integer primary key,
  condition_key text not null unique,
  kind text not null, severity text not null,
  state text not null,
  first_seen_utc text not null, last_seen_utc text not null,
  acked_by text, acked_at_utc text,
  detail text
);

create table if not exists tape_log_projection (
 tape_uuid blob primary key check(length(tape_uuid) = 16),
 incarnation integer not null check(incarnation = 1),
 projected_through_sequence blob not null check(length(projected_through_sequence) = 8),
 projected_through_offset blob not null check(length(projected_through_offset) = 8)
);
create table if not exists tape_log_object_commits (
 tape_uuid blob not null, tape_file_number blob not null,
 tape_log_sequence blob not null, tape_incarnation integer not null check(tape_incarnation = 1),
 primary key(tape_uuid, tape_file_number)
);

create table if not exists tape_audit_cursors(
 tape_uuid blob not null, field text not null, audit_sequence blob not null,
 record_uuid text not null, primary key(tape_uuid, field));
 create table if not exists tape_assignment_replay_baselines(
 tape_uuid blob primary key not null, assignment_generation integer not null);

create index if not exists object_copies_pool_idx
         on object_copies(pool_id)
         where pool_id is not null;

create index if not exists objects_content_digest_idx
         on objects(content_hash_algorithm, content_hash)
         where content_hash is not null;

create unique index if not exists tapes_voltag_unique
         on tapes(voltag)
         where voltag is not null;

create index if not exists tapes_pool_idx
         on tapes(pool_id)
         where pool_id is not null;

alter table tapes add column seal_due text;

alter table tapes add column last_verified_at_utc text;

alter table tapes add column last_verification_state integer;

alter table tapes add column last_verified_edition_digest blob;

alter table tapes add column voltag_source text default 'changer' check(voltag_source in ('changer','operator') and (voltag is null or voltag_source is not null));

alter table tapes add column cartridge_generation text;

alter table tapes add column generation_source text check(generation_source in ('measured','label','operator'));

alter table tape_io_fences add column endpoint_key text;

create table if not exists tape_pool_assignments(
        tape_uuid blob not null, seq integer not null, pool_id text,
        source text not null check(source in ('rule','operator')),
        actor text not null, at_utc text not null, assignment_generation integer not null,
        primary key(tape_uuid, seq));

create trigger if not exists tapes_generation_pair_insert before insert on tapes
        when (new.cartridge_generation is null) <> (new.generation_source is null)
        begin select raise(abort, 'generation and provenance must be recorded together'); end;
        create trigger if not exists tapes_generation_pair_update before update of cartridge_generation, generation_source on tapes
        when (new.cartridge_generation is null) <> (new.generation_source is null)
        begin select raise(abort, 'generation and provenance must be recorded together'); end;

alter table media_readiness_ops add column endpoint_key text;

create trigger if not exists media_readiness_ops_endpoint_insert after insert on media_readiness_ops
            when new.endpoint_key is null and new.library_serial is not null and new.drive_element is not null
            begin update media_readiness_ops set endpoint_key = json_object('kind','bay','library_serial',new.library_serial,'bay',new.drive_element) where rowid = new.rowid; end;
            drop trigger if exists media_readiness_ops_endpoint_update;
            create trigger media_readiness_ops_endpoint_update after update of library_serial, drive_element on media_readiness_ops
            when (new.library_serial is not null and new.drive_element is not null)
              or (new.endpoint_key is old.endpoint_key and json_extract(new.endpoint_key, '$.kind') = 'bay')
            begin update media_readiness_ops set endpoint_key = case
                when new.library_serial is not null and new.drive_element is not null
                then json_object('kind','bay','library_serial',new.library_serial,'bay',new.drive_element)
                else null end where rowid = new.rowid; end;
            create index if not exists media_readiness_ops_endpoint_idx on media_readiness_ops(endpoint_key);

alter table sessions add column endpoint_key text;

create trigger if not exists sessions_endpoint_insert after insert on sessions
            when new.endpoint_key is null and new.library_serial is not null and new.drive_bay is not null
            begin update sessions set endpoint_key = json_object('kind','bay','library_serial',new.library_serial,'bay',new.drive_bay) where rowid = new.rowid; end;
            drop trigger if exists sessions_endpoint_update;
            create trigger sessions_endpoint_update after update of library_serial, drive_bay on sessions
            when (new.library_serial is not null and new.drive_bay is not null)
              or (new.endpoint_key is old.endpoint_key and json_extract(new.endpoint_key, '$.kind') = 'bay')
            begin update sessions set endpoint_key = case
                when new.library_serial is not null and new.drive_bay is not null
                then json_object('kind','bay','library_serial',new.library_serial,'bay',new.drive_bay)
                else null end where rowid = new.rowid; end;
            create index if not exists sessions_endpoint_idx on sessions(endpoint_key);

alter table drives add column endpoint_key text;

create trigger if not exists drives_endpoint_insert after insert on drives
            when new.endpoint_key is null and new.last_library_serial is not null and new.last_element_address is not null
            begin update drives set endpoint_key = json_object('kind','bay','library_serial',new.last_library_serial,'bay',new.last_element_address) where rowid = new.rowid; end;
            drop trigger if exists drives_endpoint_update;
            create trigger drives_endpoint_update after update of last_library_serial, last_element_address on drives
            when (new.last_library_serial is not null and new.last_element_address is not null)
              or (new.endpoint_key is old.endpoint_key and json_extract(new.endpoint_key, '$.kind') = 'bay')
            begin update drives set endpoint_key = case
                when new.last_library_serial is not null and new.last_element_address is not null
                then json_object('kind','bay','library_serial',new.last_library_serial,'bay',new.last_element_address)
                else null end where rowid = new.rowid; end;
            create index if not exists drives_endpoint_idx on drives(endpoint_key);

alter table drive_events add column endpoint_key text;

create trigger if not exists drive_events_endpoint_insert after insert on drive_events
            when new.endpoint_key is null and new.library_serial is not null and new.element_address is not null
            begin update drive_events set endpoint_key = json_object('kind','bay','library_serial',new.library_serial,'bay',new.element_address) where rowid = new.rowid; end;
            drop trigger if exists drive_events_endpoint_update;
            create trigger drive_events_endpoint_update after update of library_serial, element_address on drive_events
            when (new.library_serial is not null and new.element_address is not null)
              or (new.endpoint_key is old.endpoint_key and json_extract(new.endpoint_key, '$.kind') = 'bay')
            begin update drive_events set endpoint_key = case
                when new.library_serial is not null and new.element_address is not null
                then json_object('kind','bay','library_serial',new.library_serial,'bay',new.element_address)
                else null end where rowid = new.rowid; end;
            create index if not exists drive_events_endpoint_idx on drive_events(endpoint_key);

pragma user_version = 23;
insert into schema_meta(key, value) values('schema_version', X'3233');
