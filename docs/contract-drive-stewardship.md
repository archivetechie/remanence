# Contract — drive stewardship (normative)

**Status:** frozen 2026-07-04 (design `drive-stewardship-design-v0.1.md`
v0.3, panel + verify r1 + re-verify clean). This file is the single
source for every shared artifact across the DS-M1/M2/M3 prompt set.
Prompts reference it; NEVER inline a copy into a prompt. On conflict,
this contract wins over prose in the design doc.

## 1. Schema (remanence-state)

New TABLES go into `MINIMUM_SCHEMA` (`create table if not exists`);
new COLUMNS on existing tables use `ensure_column` under the
`user_version` gate. DDL exactly as design §3.3:

- `drives` (pk `drive_uuid blob`; `serial` may be '' — never a key),
  `drive_events`, `drive_health_snapshots`
  (`unique(session_id, trigger)`), `clean_runs` (+ partial unique
  indexes `clean_runs_one_active_per_drive`,
  `clean_runs_one_active_per_cart`; cart fields nullable until
  selection), `alarms` (`condition_key` unique).
- `ensure_column`: `tapes.kind` (default `'data'`),
  `tapes.cleaning_uses`, `tapes.cleaning_state`,
  `sessions.drive_uuid`.
- `migrate()` backfill (config-free, one-time, user_version-gated):
  `update tapes set kind='cleaning' where voltag glob 'CLN*'` guarded
  by no committed `object_copies`. Configured extra prefixes: post-open
  config-aware reconciliation pass, same guard.

**Authoritative tables (non-rebuildable):** `drives`, `drive_events`,
`drive_health_snapshots`, `clean_runs`, `alarms` — MUST be excluded
from `clear_rebuildable_tables` and survive
`rebuild-catalog-from-journals`; `rem-debug catalog reset` consent
text must name drive history in its blast radius.

**Sessions reprojection:** `SessionOpened` audit detail carries
`drive_uuid` + `drive_serial`; `project_session_record` restores
`sessions.drive_uuid` (the `drive_bay` pattern, index.rs:3007). Null
`drive_uuid` renders as "unattributed (pre-stewardship)".

## 2. Vocabularies (exact strings)

- `drives.managed`: `rem` | `foreign`
- `drives.state`: `active` | `retired`
- `drives.cleaning_due`: `none` | `periodic` | `now` (monotonic:
  `periodic` never downgrades `now`; cleared only by verified clean)
- `drives.identity_source`: `DvcidInline` | `DvcidAndInquiry` |
  `Derived` (exact `IdentitySource` variant names, model.rs:154)
- `tapes.kind`: `data` | `cleaning`
- `tapes.cleaning_state`: null | `unverified` | `ok` | `expired` |
  `rejected`
- `drive_events.event_kind` (observational ONLY): `first-seen` |
  `bay-moved` | `firmware-changed` | `alert-observed` | `reappeared` |
  `serial-collision`
- `clean_runs.phase`: `fencing` | `selecting` | `moving-in` |
  `cleaning` | `moving-back` | `verifying` | terminal: `done` |
  `failed` | `needs-operator`
- `clean_runs.trigger`: `auto-now` | `auto-periodic` | `manual`
- `alarms.condition_key` kinds: `no-cln-cart`,
  `cleaning-needs-operator`, `cln-cart-expired`,
  `cart-not-cleaning-behavior`, `kind-flip-refused`,
  `drive-cleaning-abnormal-frequency`, `retired-drive-reappeared`,
  `drive-serial-collision`, `foreign-drive-wants-cleaning`,
  `snapshot-persist-failing` — instance keys are
  `<kind>:<scope>` (e.g. `no-cln-cart:mainlib`,
  `drive-serial-collision:<serial>`)
- `alarms.state`: `open` | `acked` | `cleared`

## 3. Audit (additive `AuditEvent` variants)

`DriveRetired`, `DriveAnnotated`, `DriveCleaned`,
`CleaningCartridgeExpired`, `CleaningCartridgeRegistered`,
`DriveFenced`, `DriveUnfenced`, `AlarmAcked`.
`AuditActor::System` stays a unit variant; autonomous-cleaning events
carry `component: "cleaning"` inside the event detail payload.

## 4. Proto (layer5.proto — purely additive; never reuse/renumber fields)

On **LibraryService** (NOT Catalog — Catalog's invariant is
"answerable from on-tape state alone"):
`ListDrives`, `GetDrive`, `GetDriveHistory`, `AnnotateDrive`,
`RetireDrive`, `CleanDrive`, `ListAlarms`, `AckAlarm`,
`GetLiveStatus`.

- `Drive` message gains: `drive_uuid`, `cleaning_due`, `fenced`,
  `lifetime_read_bytes`, `lifetime_write_bytes`, `counter_epoch`,
  `session_id`, `active_alert_names` (next free field numbers).
- `Drive.Status` gains additive values `CLEANING`, `FENCED`; every
  renderer needs a catch-all arm (proto3 open enums).
- `LibraryState` gains `managed`.
- New messages: `Alarm` (mirrors the alarms row),
  `GetLiveStatusResponse { repeated LibraryState libraries, repeated
  OperationRef operations, repeated Alarm alarms, string
  snapshot_at_utc, uint64 daemon_epoch }` — composes existing shapes;
  NO parallel drive/slot messages.
- `RetireDrive` request: `drive_uuid`, `reason`,
  `i_understand_fleet_removal_is_permanent: bool` (server rejects
  false), `allow_derived_identity: bool`.
- Byte counters keyed by drive (`drive_uuid`), never bay;
  `counter_epoch` changes on daemon restart or identity change;
  clients rebaseline on epoch change or counter decrease.

## 5. AuthZ

New `AuthPermission::Lifecycle`. Mapping (tests required for every
role × mutating RPC): `RetireDrive` = Lifecycle (unix `System` role
ONLY; `Operator`/`Orchestrator`/`Readonly` denied; future `Admin` may
join); `AnnotateDrive` = Write; `CleanDrive`, `AckAlarm` = Robotics;
all reads = Readonly. Mutations on `identity_source='Derived'` rows
refuse without `allow_derived_identity: true`.

## 6. Safety invariants (enforced, not conventions)

- **Robotics choke-point gate** (one place, pre-move): source slot
  holds `kind='cleaning'` cart with `cleaning_state ∈
  ('unverified','ok')`; destination drive is in the same managed
  library. Refuse otherwise. Both cleaning moves audited.
- **Foreign transport CDB allowlist** (enforced at the transport
  wrapper): INQUIRY, TEST UNIT READY, READ ELEMENT STATUS, LOG SENSE
  excluding page 2Eh; page 2Eh admitted only when
  `foreign_tapealert = true`. Any other opcode = loud error.
- Foreign drives NEVER get `cleaning_due` (opt-in raises ackable
  advisory alarms only) and are never fenced/cleaned/retired-by-rem.
- **Kind-flip guard:** any `tapes.kind` change on a tape with
  committed `object_copies` refuses + raises `kind-flip-refused`.
- `cleaning_uses` increments and `cleaning_due` clears ONLY in
  `verifying → done` (post-clean verification per design §4.3).
- Retired `drive_uuid`s are excluded from mount resolution, cleaning,
  and collection; reappearance alarms, never reactivates.

## 7. CLI JSON envelopes (stable, versioned)

- `rem.drive.retire.v1` (the `rem.tape.retire.v1` pattern,
  lib.rs:3840); `rem.drive.list.v1`, `rem.drive.show.v1`,
  `rem.alarms.v1` for the read verbs.
- `rem.top.v1`: emitted by `rem top --once --json`; embeds the
  enriched library/drive shapes of `GetLiveStatusResponse` plus
  `snapshot_at_utc`, `daemon_epoch`. Field names in this envelope are
  the TOPX scenario contract — additive evolution only.

## 8. Config keys (defaults normative, design §6)

`[drives]` `managed_libraries=[]` (empty ⇒ daemon-operated set),
`foreign_counter_poll="60m"`, `foreign_tapealert=false`,
`heartbeat="1h"`, `snapshot_miss_alarm=3`.
`[cleaning]` `auto=true`, `voltag_prefixes=["CLN"]`, `use_warn=45`,
`complete_timeout="10m"`, `min_cycle_duration="60s"`,
`min_interval="12h"`, `weekly_cap=4`.
`[livestatus]` `min_poll_interval="250ms"`,
`foreign_changer_poll="60s"`, `foreign_poll_lease="5m"`.

## 9. Dependency rule

ratatui + crossterm: `remanence-cli` only, behind cargo feature `tui`
(default on). `rem-daemon` and every other crate must not link them.
