# Codex prompt — DS-M1: drive catalog (remanence)

**Status:** pending.
**Normative:** Read `docs/contract-drive-stewardship.md` first — it is
normative for schema, vocabularies, proto, authz, envelopes, config.
Design rationale: `docs/drive-stewardship-design-v0.1.md` (v0.3,
frozen) §§2–3, 7. Definition of done: `AGENTS.md`.

## Step 0 — skeleton first

Land the API surface as compiling stubs (types, table DDL, proto
messages/RPCs returning UNIMPLEMENTED, clap verbs) and get
`cargo check --workspace --all-features` green BEFORE implementing.
This commit doubles as the design-verification gate; report anything
in the contract that does not fit the real code as a finding, do not
silently improvise.

## Scope

1. **Schema:** the five authoritative tables + four `ensure_column`
   additions + the config-free `CLN*` backfill + post-open
   configured-prefix reconciliation (contract §1). Exclude the five
   tables from `clear_rebuildable_tables`; widen `rem-debug catalog
   reset` consent text.
2. **Sessions correlation:** `drive_uuid`+`drive_serial` into
   `SessionOpened` audit detail; restore in `project_session_record`;
   stamp at session open from the bay→drive resolution.
3. **Identity:** `drive_uuid` surrogate assignment; (serial, vendor,
   product) matching rule; blank/collided serial ⇒ `actionable=0` +
   `drive-serial-collision` alarm + exclusion from attribution and
   mutation (design §3.1).
4. **Collection (event-driven):** ordered session-close snapshot phase
   in the drive actor (idempotent insert, async completion,
   `snapshot_miss_alarm`); on-alert; manual `rem drive poll`; hourly
   heartbeat (`last_seen_utc` only); inventory diffing → observational
   `drive_events`; retired-reappeared handling. **Foreign drives:**
   CDB-allowlisted transport wrapper (contract §6), error-counter
   pages only, 60 m cadence, backoff, stale-marking.
5. **Alarm subsystem:** `alarms` table semantics (dedup by
   condition_key, open→acked→cleared, re-observation refreshes
   `last_seen_utc`, self-heal clears, restart-persistent), `ListAlarms`
   / `AckAlarm` (audited), `rem alarms` + `rem alarms ack`.
6. **RPCs + authz:** the nine LibraryService RPCs (CleanDrive +
   GetLiveStatus land as UNIMPLEMENTED stubs — they are M2/M3);
   `AuthPermission::Lifecycle` + full role×RPC permission tests;
   derived-identity refusal; RetireDrive server-side ack field.
7. **CLI:** `rem drive list|show|history|alerts|annotate|retire|poll`,
   `rem alarms`, per design §3.6 semantics (annotate partial-update +
   append notes + ISO-8601 validation; retire wording + operational
   effect; voltag rendering everywhere). `rem catalog tape <voltag>`
   gains the per-drive rollup (the tape-anchored dual view).
   `rem tape alerts` becomes a deprecated alias of `rem drive alerts`.
   JSON envelopes per contract §7.
8. **Compat:** proto additive-evolution tests both directions
   (old-CLI/new-daemon pass-through; new-CLI/old-daemon UNIMPLEMENTED
   message); note in your report that sutradhara `_proto` regeneration
   is required (separate dispatch).

## Out of scope

Cleaning actor, clean_runs consumers, fencing behavior (M2 — but the
`fenced` column and `FENCED` status land now as inert), TUI/live
status serving (M3), any d2tape change, retention/pruning.

## Acceptance

`cargo fmt --check`; `cargo clippy --workspace --all-targets -- -D
warnings`; full test suite; plus, named: migration+backfill tests,
**rebuild regression** (authoritative tables + kind/uses/cleaning_state
+ sessions.drive_uuid survive `rebuild-catalog-from-journals` and a
daemon restart), snapshot idempotency, permission matrix, retire
refusals (active session / missing ack / derived identity), alarm
lifecycle (open→ack→clear→re-raise). Report per repo convention.
Post-implementation **diff gate** runs before this prompt is archived.

Verification member: harness scenario **DRV** —
`~/system/docs/prompt-drive-stewardship-scenarios.md` §DRV (cut in the
same set).
