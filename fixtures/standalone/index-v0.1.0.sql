-- Frozen schema extracted from tag v0.1.0, index.rs MINIMUM_SCHEMA and additive columns.
BEGIN TRANSACTION;
CREATE TABLE alarms(
  alarm_id integer primary key,
  condition_key text not null unique,
  kind text not null, severity text not null,
  state text not null,
  first_seen_utc text not null, last_seen_utc text not null,
  acked_by text, acked_at_utc text,
  detail text
);
CREATE TABLE catalog_units(
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
CREATE TABLE clean_runs(
  run_id text primary key,
  drive_uuid blob not null, library_serial text not null,
  cart_tape_uuid blob, cart_home_slot integer,
  phase text not null,
  trigger text not null,
  started_at_utc text not null, updated_at_utc text not null,
  detail text
);
CREATE TABLE drive_events(
  event_id integer primary key,
  drive_uuid blob not null references drives(drive_uuid),
  event_kind text not null,
  at_utc text not null,
  library_serial text, element_address integer,
  tape_uuid blob, detail text
);
CREATE TABLE drive_health_snapshots(
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
CREATE TABLE drives(
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
CREATE TABLE idempotency_keys(
  actor_fingerprint text not null,
  idempotency_key text not null,
  request_fingerprint blob not null,
  operation_id text not null,
  terminal_state text,
  response_fingerprint blob,
  updated_at_utc text not null,
  primary key(actor_fingerprint, idempotency_key)
);
CREATE TABLE ingested_sources(
  source_kind text not null,
  source_id text not null,
  offset_bytes integer not null,
  terminal_hash blob,
  updated_at_utc text not null,
  primary key(source_kind, source_id)
);
CREATE TABLE media_readiness_ops(
  operation_id text primary key,
  run_id text,
  library_serial text not null,
  changer_sg text,
  drive_element integer not null,
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
CREATE TABLE media_readiness_transitions(
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
CREATE TABLE object_copies(
  object_id text not null,
  tape_uuid blob not null,
  tape_file_number integer not null,
  first_body_lba integer not null default 0,
  first_parity_data_ordinal integer,
  protected_until_ordinal integer,
  status text not null,
  representation text not null default 'unknown',
  recipient_epoch_ids text,
  metadata_frame_len integer,
  plaintext_digest blob,
  plaintext_digest_algorithm text,
  stored_digest blob,
  stored_digest_algorithm text,
  pool_id text,
  primary key(object_id, tape_uuid, tape_file_number)
);
CREATE TABLE object_files(
  object_id text not null,
  file_id text not null,
  path text not null,
  size_bytes integer not null,
  file_sha256 blob not null,
  file_digest_algorithm text not null default 'sha256',
  first_chunk_lba integer,
  chunk_count integer not null,
  mtime text,
  executable integer,
  primary key(object_id, file_id)
);
CREATE TABLE objects(
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
CREATE TABLE operations(
  operation_id text primary key,
  operation_kind text not null,
  state text not null,
  session_id text,
  subject text,
  error_summary text,
  started_at_utc text not null,
  updated_at_utc text not null
);
CREATE TABLE schema_meta(
  key text primary key,
  value blob not null
);
CREATE TABLE sessions(
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
CREATE TABLE tape_files(
  tape_uuid blob not null,
  tape_file_number integer not null,
  kind text not null,
  block_count integer not null,
  physical_start_hint integer,
  object_id text,
  first_parity_data_ordinal integer,
  epoch_id integer,
  protected_ordinal_start integer,
  protected_ordinal_end_exclusive integer,
  canonical_metadata_hash blob,
  canonical_metadata_hash_algorithm text,
  bundle_uuid text,
  bundle_kind text,
  primary key(tape_uuid, tape_file_number)
);
CREATE TABLE tape_io_fences(
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
CREATE TABLE tape_pools(
  pool_id text primary key,
  display_name text,
  copy_class text,
  content_class text,
  created_at_utc text not null
);
CREATE TABLE tapes(
  tape_uuid blob primary key,
  voltag text,
  pool_id text,
  kind text not null default 'data',
  cleaning_uses integer,
  cleaning_state text,
  block_size integer,
  scheme_id text,
  data_blocks_per_stripe integer,
  parity_blocks_per_stripe integer,
  stripes_per_neighborhood integer,
  highest_protected_ordinal integer not null default 0,
  total_committed_ordinals integer not null default 0,
  last_committed_tape_file integer,
  written_extent_lba integer,
  state text not null,
  updated_at_utc text not null
);
CREATE TABLE wrap_maps(
  tape_uuid blob primary key,
  descriptors_json text not null,
  mapped_extent_lba integer not null,
  write_epoch integer not null,
  calibration_generation integer not null,
  harvested_at_utc text not null
);
CREATE UNIQUE INDEX tapes_voltag_unique
  on tapes(voltag)
  where voltag is not null;
CREATE INDEX objects_content_hash_idx
  on objects(content_hash)
  where content_hash is not null;
CREATE INDEX objects_caller_object_id_idx
  on objects(caller_object_id)
  where caller_object_id is not null;
CREATE INDEX object_files_path_idx
  on object_files(object_id, path);
CREATE INDEX catalog_units_tape_origin_idx
  on catalog_units(tape_uuid, origin_kind);
CREATE INDEX catalog_units_native_object_idx
  on catalog_units(native_object_id);
CREATE INDEX media_readiness_ops_active_idx
  on media_readiness_ops(library_serial, drive_element, state, updated_at_utc)
  where state not in ('ready', 'released')
    and coalesce(dirty_scope, 'drive+tape') <> 'none';
CREATE INDEX media_readiness_ops_barcode_idx
  on media_readiness_ops(library_serial, barcode)
  where barcode is not null;
CREATE INDEX media_readiness_transitions_op_idx
  on media_readiness_transitions(operation_id, transition_id);
CREATE INDEX tape_io_fences_active_tape_idx
  on tape_io_fences(tape_uuid, state, updated_at_utc)
  where state = 'active';
CREATE INDEX tape_io_fences_active_barcode_idx
  on tape_io_fences(barcode, state, updated_at_utc)
  where state = 'active' and barcode is not null;
CREATE UNIQUE INDEX clean_runs_one_active_per_drive
  on clean_runs(drive_uuid)
  where phase not in ('done','failed','needs-operator');
CREATE UNIQUE INDEX clean_runs_one_active_per_cart
  on clean_runs(cart_tape_uuid)
  where phase not in ('done','failed','needs-operator')
    and cart_tape_uuid is not null;
CREATE INDEX object_copies_pool_idx on object_copies(pool_id) where pool_id is not null;
CREATE INDEX objects_content_digest_idx on objects(content_hash_algorithm, content_hash) where content_hash is not null;
CREATE INDEX tapes_pool_idx on tapes(pool_id) where pool_id is not null;
INSERT INTO schema_meta(key, value) VALUES ('schema_version', X'3136');
COMMIT;
PRAGMA user_version=16;
