# Drive stewardship — catalog, auto-cleaning, live console — Design v0.2

**Status:** panel folded (2026-07-04), awaiting verify round.
**Panel 2026-07-04:** 68 findings (11 security / 17 failure-modes[codex] /
13 contract / 15 UX / 12 cost), 8 blockers; deduped to ~35 folds; folded
in one pass (this revision). Fold decisions recorded inline as **[fold]**
notes where the rationale isn't obvious.
**Problem source:** operator request 2026-07-04 — three gaps: no drive
history (the D2-era `devices` table never answered "tape fault or drive
fault?"), no automatic cleaning, no live combined library+drive view.
**Operator decisions (2026-07-04):** cleaning policy =
fence-after-session; catalog scope = observe-all/act-on-own; snapshot
retention = keep everything forever (event-driven collection makes this
tens of MB/year); foreign TapeAlert reading = **off by default**, config
opt-in (`foreign_tapealert`), documented as consuming clear-on-read
state.
**Precedent docs:** `tape-identity-lifecycle-design-v0.1.md`,
`chaos-phase-d-tapealert-design-v0.1.md`,
`layer5-phase3c-drive-status-busy-design-v0.1.md`,
`multidrive-concurrency-architecture-v0.1.md`.

---

## 1. Problem

1. **No drive records.** The catalog has no drives table
   (`MINIMUM_SCHEMA`, index.rs:4172-4337). `sessions` records
   `tape_uuid` + `drive_bay` (index.rs:4327-4336) but a bay is a
   position, not a device. When a restore misbehaves there is no data to
   ask: is this tape bad, or is this drive bad on every tape it touches?
2. **No cleaning automation.** TapeAlert flags 20/21/22 are parsed and
   named (log_sense.rs:91-93) but nothing consumes them; the CLN prefix
   is cosmetic only (`print_slot`, remanence-cli lib.rs:8754-8770).
   Dirty heads on an archival write path are a durability risk managed
   by hand.
3. **No live visibility.** `rem library --slots` is a one-shot
   direct-SCSI snapshot; `StreamLibraryEvents` is stubbed (S6c).

## 2. Design overview

Three stacked milestones (the copygrain shape): **DS-M1 drive catalog**
(foundation), **DS-M2 auto-cleaning** (first automated consumer),
**DS-M3 `rem top`** (read surface). Each is its own codex prompt set
with its own verification member.

Two design pillars, both panel-hardened:

- **Drives are things, bays are positions** — and serials are claims,
  not truths. Everything keys on a daemon-assigned surrogate
  `drive_uuid` (the `Library.library_uuid` pattern); the
  device-reported serial is an attribute.
- **Authoritative vs rebuildable.** The SQLite index is architecturally
  a rebuildable cache (proto:304-308; `startup_replay` →
  `rebuild_index_from_journals` runs on every daemon boot and
  `clear_rebuildable_tables` wipes+reprojects `sessions`). Drive
  history is host-observed and CANNOT be rebuilt from tape. Every new
  table in this design is declared **authoritative**: excluded from
  `clear_rebuildable_tables`, excluded from journal rebuild, and
  covered by a widened consent message on `rem-debug catalog reset`
  (which today deletes the whole DB file, state.rs:122). A rebuild
  regression test asserts drive history and cleaning state survive
  `rem rebuild-catalog-from-journals` and a daemon restart.

## 3. DS-M1 — drive catalog

### 3.1 Identity

- **Surrogate key.** `drive_uuid` assigned at first discovery. Mapping
  rule for subsequent discoveries: a drive maps to an existing
  `drive_uuid` when (serial, vendor, product) match and the serial is
  non-blank. Blank serial → provisional row, matched only
  transiently by bay, marked `actionable=0`. Two simultaneous drives
  presenting the same serial → both rows marked `actionable=0` +
  standing alarm `drive-serial-collision`; non-actionable drives are
  excluded from cleaning, lifecycle verbs, and correlation attribution
  until an operator disambiguates. **[fold]** VTL/emulated drives
  routinely report blank/duplicate serials; a serial PK would merge two
  drives' histories (codex + security, independently).
- **identity_source** stores the exact discovery enum —
  `DvcidInline | DvcidAndInquiry | Derived` (model.rs:154-165), no
  lossy collapse. Mutating RPCs on `Derived`-identity drives are
  refused unless the request carries `allow_derived_identity: true`
  (the RPC-surface equivalent of `--allow-derived`, which itself gates
  only the local direct-SCSI path and does NOT cover RPCs).
- Firmware revision is an attribute updated per observation; a change
  is a `firmware-changed` event, not a new identity.

### 3.2 Managed vs foreign

`drives.managed` ∈ {`rem`, `foreign`}, determined by a config list of
managed library serials (default: the libraries the daemon is
configured to drive). The boundary, hardened:

- **Read-only by construction, not discipline.** All foreign-device
  access goes through a wrapping transport that allowlists CDB opcodes:
  INQUIRY, TEST UNIT READY, READ ELEMENT STATUS, and LOG SENSE
  restricted to non-clear-on-read pages (error counter pages 02h/03h).
  LOG SENSE page 2Eh (TapeAlert) is **excluded from the allowlist
  unless `foreign_tapealert = true`** — reading it consumes clear-on-
  read state on a device rem doesn't own. Any other opcode through the
  foreign transport is a programming error that fails loudly.
- With `foreign_tapealert = off` (default): foreign drives get identity
  + cumulative error counters only; **no foreign cleaning advisories
  exist** (no `cleaning_due` is ever set on foreign drives — no stale
  monotonic state rem can never clear).
- With opt-in: a foreign flag-20/21 observation raises an **ackable**
  advisory alarm whose text states plainly: *"rem will NOT clean this
  drive; clean d2lib drive X manually"* — never a `cleaning_due`
  fence/act path.
- Action verbs (`clean`, `retire`, fence) hard-refuse on foreign drives
  naming the owning library. Every surface labels `[foreign: <lib>]`.
- Payoff unchanged: attribution for d2tape restores with zero d2tape
  changes; on d2tape retirement the drives flip to `managed=rem` with
  history intact.

*(Panel P1 resolved: passive polling was NOT risk-free as drafted —
TapeAlert is clear-on-read; hence the allowlist + opt-in above.)*

### 3.3 Schema

New **tables** are added to `MINIMUM_SCHEMA` (`create table if not
exists`, index.rs:4172); only new **columns** on existing tables use
`ensure_column` (index.rs:4121 — it does columns, nothing else).
All five new tables are authoritative (§2).

```sql
create table if not exists drives(
  drive_uuid blob primary key,
  serial text,                             -- device claim; may be ''
  identity_source text not null,  -- 'DvcidInline'|'DvcidAndInquiry'|'Derived'
  actionable integer not null default 1,   -- 0 = blank/collided serial
  vendor text, product text, firmware_rev text,
  managed text not null,                   -- 'rem' | 'foreign'
  state text not null default 'active',    -- 'active' | 'retired'
  cleaning_due text not null default 'none', -- 'none'|'periodic'|'now'
  fenced integer not null default 0,
  first_seen_utc text not null, last_seen_utc text not null,
  last_library_serial text, last_element_address integer,
  purchase_date text, warranty_until text,  -- ISO-8601, validated
  cost text,                                -- display-only string
  notes text,                               -- append-only timestamped lines
  retired_at_utc text, retire_reason text
);
create table if not exists drive_events(   -- observational only (§3.6)
  event_id integer primary key,
  drive_uuid blob not null references drives(drive_uuid),
  event_kind text not null, -- first-seen|bay-moved|firmware-changed|
                            -- alert-observed|reappeared|serial-collision
  at_utc text not null,
  library_serial text, element_address integer,
  tape_uuid blob, detail text               -- JSON, kind-specific
);
create table if not exists drive_health_snapshots(
  snapshot_id integer primary key,
  drive_uuid blob not null references drives(drive_uuid),
  at_utc text not null,
  trigger text not null,        -- 'session-close'|'alert'|'manual'
  session_id text,              -- correlation anchor when trigger=session-close
  tape_alert_flags text,        -- JSON array of active flag numbers
  write_errors_corrected integer, write_errors_uncorrected integer,
  read_errors_corrected integer, read_errors_uncorrected integer,
  raw_pages text,               -- JSON: everything the device served
  unique(session_id, trigger)   -- idempotent close-phase insert
);
create table if not exists clean_runs(     -- §4.3 durable state machine
  run_id text primary key,
  drive_uuid blob not null, library_serial text not null,
  cart_tape_uuid blob not null, cart_home_slot integer not null,
  phase text not null, -- 'fencing'|'selecting'|'moving-in'|'cleaning'|
                       -- 'moving-back'|'verifying'|'done'|
                       -- 'failed-retryable'|'needs-operator'
  trigger text not null,        -- 'auto-now'|'auto-periodic'|'manual'
  started_at_utc text not null, updated_at_utc text not null,
  detail text
);
create table if not exists alarms(         -- §4.4 subsystem
  alarm_id integer primary key,
  condition_key text not null unique, -- e.g. 'no-cln-cart:mainlib'
  kind text not null, severity text not null,
  state text not null,          -- 'open'|'acked'|'cleared'
  first_seen_utc text not null, last_seen_utc text not null,
  acked_by text, acked_at_utc text,
  detail text
);
-- ensure_column additions:
--   tapes.kind text not null default 'data'   ('data'|'cleaning')
--   tapes.cleaning_uses integer                (null for data tapes)
--   tapes.cleaning_state text                  (null|'ok'|'expired')  §4.1
--   sessions.drive_uuid blob                   (correlation join)
```

- **Retention: none (keep forever)** — operator decision; event-driven
  collection (§3.5) makes growth tens of MB/year. A rollup knob is
  deferred until size warrants.
- **The correlation join survives restarts.** `sessions` is wiped and
  reprojected from audit detail on every boot
  (`clear_rebuildable_tables` → `project_session_record`,
  index.rs:2969-3015). Therefore `drive_uuid` (and serial, for display)
  is **written into the `SessionOpened` audit detail** and
  `project_session_record` restores it, exactly as `drive_bay` is
  restored today (index.rs:3007). Without this the join this milestone
  exists for silently nulls after any restart (panel blocker).
- Old session rows and rows written by older binaries have null
  `drive_uuid`; correlation output labels these "unattributed
  (pre-stewardship)" rather than dropping them silently.
- `tapes.kind` backfill: `migrate()` gains a one-time
  `update tapes set kind='cleaning' where voltag glob 'CLN*'` (per
  configured prefixes) guarded by "no committed object_copies",
  mirroring the pool_id backfill (index.rs:4070-4087) — existing
  inventoried CLN carts must not stay `data`.

### 3.4 Events vs audit (single timeline, no double-write)

Lifecycle facts are **audit events** (additive `AuditEvent` variants):
`DriveRetired`, `DriveAnnotated`, `DriveCleaned`,
`CleaningCartridgeExpired`, `DriveFenced`, `DriveUnfenced` — the
cleaning actor writes them with a distinct actor component
(`System{component:"cleaning"}`) so autonomous robotics is forensically
attributable. `drive_events` holds only **observational** facts
(first-seen, bay-moved, firmware-changed, alert-observed, reappeared,
serial-collision). `rem drive history` merges both streams at read
time. **[fold]** Avoids double-writing the same fact to two stores
(cost lens); audit stays the tamper-evident record for everything
state-changing (security lens).

### 3.5 Collection — event-driven, ordered, off the hot path

- **Session close (managed drives):** an ordered close phase in the
  drive actor — LOG SENSE (TapeAlert + error counters) is read while
  the actor still owns the device, tagged with `session_id`,
  `tape_uuid`, `drive_uuid`; the DB insert is idempotent
  (`unique(session_id, trigger)`) and may complete asynchronously.
  Snapshot failure is logged and alarmed after N consecutive misses,
  never blocks session close.
- **On alert** and **manual** (`rem drive poll <serial>`).
- **Hourly liveness heartbeat:** `last_seen_utc` update only (TUR), no
  snapshot row. **[fold]** The drafted 15-min idle poll was the
  dominant row generator and added nothing — counters are static on an
  idle drive (cost lens).
- **Foreign drives:** error-counter pages only, via the allowlisted
  transport, every 60 min, timeout + exponential backoff on
  BUSY/reservation-conflict/unit-attention; failures mark data stale,
  never alarm-spam.
- **Inventory diffing** on discovery/refresh: upserts `drives`, emits
  observational events. A vanished drive is NOT auto-retired. A retired
  `drive_uuid` reappearing in inventory → `reappeared` event + standing
  alarm; the row **stays retired** and is excluded from everything;
  re-adopting that hardware is an explicit new registration (new
  `drive_uuid`, old history intact under the old one).

### 3.6 Lifecycle, RPCs, CLI

**Transport (contract fix):** drive lifecycle mutations are **daemon
RPCs**, NOT the offline path `rem tape retire` uses (local
catalog+audit mutation, lib.rs:2528) — the active-session refusal
requires live `DrivePool` knowledge only the daemon has. The v0.1
"same path as tape retire" claim is dropped.

**RPCs live on LibraryService** (Catalog service keeps its documented
invariant that every query is answerable from on-tape state alone,
proto:304-308; drives are not on-tape state): `ListDrives`, `GetDrive`,
`GetDriveHistory`, `AnnotateDrive`, `RetireDrive`, `CleanDrive`,
`ListAlarms`, `AckAlarm`, `GetLiveStatus`.

**AuthZ (new permission tier):** a new `AuthPermission::Lifecycle`
gates `RetireDrive` (and future destructive lifecycle RPCs). Mapping:
`AnnotateDrive` = Write; `CleanDrive`, `AckAlarm` = Robotics; reads =
Readonly. The unix-socket `System` role retains all; the mTLS default
`Readonly` role gets none of the mutations. `RetireDrive` additionally
requires an in-request acknowledgment field the server rejects without
(the CLI ack-flag moved server-side), plus `allow_derived_identity`
when applicable (§3.1).

```
rem drive list [--foreign] [--retired]        # labels [foreign: d2lib]
rem drive show <serial|uuid>    # identity, annotations, cleaning/fence
                                # state, last snapshot, merged history,
                                # per-tape session/error rollup
rem drive history <serial|uuid> [--events|--snapshots|--json]
rem drive alerts <serial|uuid>  # canonical TapeAlert read
rem drive annotate <serial|uuid> [--warranty-until YYYY-MM-DD]
        [--purchase-date YYYY-MM-DD] [--cost <text>]
        [--note <line> | --notes-set <text>]
rem drive clean <serial|uuid>
rem drive retire <serial|uuid> --reason <r> \
        --i-understand-fleet-removal-is-permanent
rem alarms [--all] | rem alarms ack <condition-key>
```

- **The dual correlation view (panel blocker):** `rem catalog tape
  <voltag>` gains a per-drive session/error rollup — the incident
  starts from a barcode ("is RMJ042 bad, or the drive?"), so one
  command must answer it from the tape side, exactly as
  `rem drive show` answers it from the drive side. All rollups render
  and accept **voltags**, never raw uuid hex.
- `rem tape alerts` becomes a deprecated alias of `rem drive alerts`
  (it always was a per-drive read keyed by `--bay`); help text
  cross-references.
- **Annotate semantics:** partial update — omitted flags leave fields
  unchanged; `--note` appends a timestamped line, `--notes-set`
  replaces; dates validated ISO-8601 at parse time; `cost` documented
  display-only. The command echoes the stored record.
- **Retire semantics (wording fixed):** history is PRESERVED; the flag
  names the real consequence (permanent fleet removal). Retire removes
  the drive from session scheduling, cleaning eligibility, and
  collection; it renders `retired` in all views; if the hardware is
  still in a bay it is shown with a warning banner. Refused while an
  active session holds the drive. Idempotent. JSON envelope
  `rem.drive.retire.v1` (the `rem.tape.retire.v1` convention,
  lib.rs:3840).

## 4. DS-M2 — auto-cleaning

### 4.1 Cleaning cartridges — classification hardened

- `tapes.kind='cleaning'` assigned at registration by voltag prefix,
  **corroborated** before the cart is ever auto-moved: the first clean
  using a cart confirms it behaves as a cleaning cart (drive accepts +
  runs cycle); until corroborated a cart is selectable but a
  non-cleaning behavior (drive treats it as data media) aborts the run,
  flips nothing, and raises an alarm.
- **Kind is one-way-guarded:** any kind flip on a tape with committed
  `object_copies` refuses and alarms — a misclassified data tape must
  never silently leave the durability floor NOR become an auto-move
  target (panel blocker). Manual override `rem tape set-kind
  --cleaning <voltag>` exists for non-CLN-barcoded carts (same
  committed-copies guard) — without it an inserted cart with an
  unexpected barcode is silently data and the no-cartridge alarm nags
  forever while the cart sits there.
- **Expiry does NOT use tape-retire** (panel blocker: retire frees the
  voltag, so the same physical expired cart would be re-adopted fresh
  with reset uses). Instead: `tapes.cleaning_state='expired'` —
  terminal for auto-selection, voltag stays bound, surfaced as a
  replace-cartridge alarm until the cart is exported; export clears the
  row's presence normally. `cleaning_uses` increments only on verified
  completed cleans; warn threshold default 45.
- **Query contract** (the "query edge" made concrete): `list_tapes`
  (index.rs:1362) gains a `kind` filter; data-facing callers —
  `ListTapes` RPC, pool eligibility (pool_write.rs:554), durability
  accounting, catalog views (library.rs:142) — default to
  `kind='data'`; the cleaning selector, `rem drive`/`rem top` surfaces
  opt in to `kind='cleaning'`. The prompt set enumerates every
  `list_tapes` call site and states its kind explicitly.
- Cart registration (IE insert + refresh, which is triggered on
  IE-port state change) emits a `cleaning-cartridge-registered` event
  with remaining-use estimate — the operator's "the system will now
  use this" confirmation.

### 4.2 Detection — persist on first observation (unchanged, scoped)

Any observation of flag 20/21 on a **managed** drive persists
`drives.cleaning_due` (monotonic: `periodic` never downgrades `now`),
cleared only by a verified clean. Detection points: session close,
on-alert, manual poll. Foreign drives: §3.2 (no `cleaning_due` unless
opt-in, and then advisory-alarm-only, ackable).
**Frequency cap [fold]:** per-drive minimum interval between auto-
cleans (default 12 h) and a weekly cap (default 4); exceeding the cap
raises `drive-cleaning-abnormal-frequency` **instead of** cleaning — a
drive begging for cleaning this often is itself failing, and the cap
bounds consumable burn (cost lens; doubles as a failing-drive signal).

### 4.3 The cleaning actor — durable, serialized, verified

**Durable state machine (panel blocker):** every run is a `clean_runs`
row updated at each phase transition. **Startup reconciliation:** on
daemon boot, before admitting sessions, any non-terminal run is
reconciled against READ ELEMENT STATUS — cart back in a slot → close
the run accordingly; cart still in the drive → resume at `moving-back`
(attempt return) or park in `needs-operator` with the bay fenced and an
alarm. No guessing about use-count/`cleaning_due`: they change only in
`verifying→done`, exactly once per run.

**Serialization:** at most one active run per drive (`clean_runs`
uniqueness on non-terminal phase per drive_uuid); the selected cart is
reserved (its tape_uuid held by the run) before MOVE MEDIUM; a manual
trigger while a run is active joins it (no-op with a message).

**Robotics choke-point gate (security fold):** the envelope — source
slot holds a confirmed `kind='cleaning'`, non-expired cart; destination
is a drive in the same managed library — is verified in ONE place, a
pre-move guard where the cleaning actor submits to the changer actor,
not smeared across selection code. The guard refuses anything else;
both auto moves are audited.

**Fencing is first-class (codex fold):** `drives.fenced` +
`clean_runs.phase` back a real admission check in mount/session
resolution (the current DrivePool reservation is a transient busy bit,
disarmed after session open — insufficient). Fence/unfence are audited
(`DriveFenced`/`DriveUnfenced`), exposed as drive status `FENCED`, and
persist across restarts (reconciliation decides release). Race test:
session-open racing fence must lose or complete, never interleave.

**Phases:** trigger → fence (policy: `now` = fence-after-session;
`periodic` = wait natural idle, no fence) → select+reserve cart → gate
→ MOVE MEDIUM slot→drive → drive runs cycle + auto-ejects → poll
element status for eject → MOVE MEDIUM drive→home slot → **verify** →
done.

**Verification before credit (codex fold):** eject alone is NOT
success (expired media, operator removal, failed unload all look like
"cart left the drive"). `verifying` requires: cart back in home slot,
AND cycle duration ≥ a floor (expired carts eject in seconds), AND no
flag 22 observed, AND a post-clean TapeAlert read on the managed drive
shows 20/21 no longer asserted (safe: rem is the only reader of managed
drives). Only then: clear `cleaning_due`, increment `cleaning_uses`,
`DriveCleaned` audit, unfence. Fast-eject or flag 22 →
`cleaning_state='expired'` on the cart, try the next cart, else alarm.

**Failure protocol (codex fold):** explicit terminal states.
`failed-retryable` (move failure with cart safely in a slot): one
retry, then alarm. `needs-operator` (cart stuck in drive, return move
failed, timeout at default 10 min): bay STAYS fenced, standing alarm
names the run, the cart, and the recovery step; no auto-retry loops.
The bay is never left fenced without an open alarm saying why.

### 4.4 Alarm subsystem (promoted from prose to a mechanism)

The `alarms` table (§3.3) is the single standing-condition store:
condition-keyed (dedup by `condition_key`), `open → acked → cleared`,
re-observation refreshes `last_seen_utc` (no new row, no re-spam; a
log line per state change only), restart-persistent, self-heal sets
`cleared` when the condition stops holding. `AckAlarm` is audited;
acked alarms stop re-alarming until cleared-then-reraised. Surfaces:
`rem alarms` (CLI, works headless — alarms must not live only inside
the TUI), `GetLiveStatus` (top's pinned band), daemon log. Conditions
in this design: `no-cln-cart`, `cleaning-needs-operator`,
`cln-cart-expired`, `drive-cleaning-abnormal-frequency`,
`retired-drive-reappeared`, `drive-serial-collision`,
`foreign-drive-wants-cleaning` (opt-in only), `snapshot-persist-failing`.

*(Panel P2 resolved: auto-clean default stays ON for managed libraries
— no lens objected; the frequency cap bounds the failure mode.)*

## 5. DS-M3 — `rem top`

### 5.1 Data path — enriched existing projection, polled

`GetLiveStatus` (LibraryService) **returns the existing shapes,
extended additively** — NOT a parallel projection (two projections of
drive/slot state would drift; contract + cost lenses):
`LibraryState` gains `managed`; `Drive` (proto:194-210) gains
`drive_uuid`, `cleaning_due`, `fenced`, `lifetime_read_bytes`,
`lifetime_write_bytes`, `counter_epoch`, `session_id`,
`active_alert_names`; `Drive.Status` gains additive values `CLEANING`,
`FENCED` (proto3 open enums: old consumers decode unknown ints and
must render them pass-through — renderer catch-all arm required;
one-line compat note in the proto). The response adds active
operations (existing `OperationRef`s), open alarms, and a snapshot
timestamp. Active-session bytes come from the existing
`WriteSession.bytes_committed` field — not re-counted.

**Counters (codex fold):** lifetime byte counters are keyed by
`drive_uuid` (NOT bay — a drive swap must not inherit another drive's
counters), with a `counter_epoch` that changes on daemon restart or
drive identity change; clients recompute their MB/s baseline whenever
the epoch changes or a counter decreases. Implementation: per-actor
`AtomicU64`s bumped in the existing append/read streaming paths.

**Serving cost:** the snapshot is served from daemon memory + cached
inventory — zero SCSI per poll; a server-side minimum poll interval
(250 ms) bounds abusive clients. **Foreign changer polling is lazy
[fold]:** READ ELEMENT STATUS against d2lib runs only while at least
one `GetLiveStatus` consumer is connected (60 s cadence), with UA/
conflict backoff; otherwise last-known inventory is served with its
age. *(Panel P3 resolved: lazy + backoff, not continuous.)*

### 5.2 TUI — minimum glanceable v1, deps confined

ratatui + crossterm land in **remanence-cli only**, behind a `tui`
cargo feature (default on, the `linux-udev` pattern); `rem-daemon`
never links them.

v1 layout (80×24-first, panel-hardened): a **pinned band** that never
scrolls — open alarms (all libraries, regardless of which is selected)
+ drive table (bay, serial, tape voltag, state, MB/s, badges) — above a
**collapsible/scrollable slot grid**, ops footer. Every state is
**glyph + text, color redundant** (colorblind/no-color-terminal safe).
Keys: `q` quit, `l` cycle library, `s` slot grid toggle, `?` help;
pause exists but renders a prominent `PAUSED` banner. Drive detail is
NOT re-implemented in the TUI — the status line points at
`rem drive show <serial>`. Deferred polish: pool coloring, sparklines,
in-TUI ack, event ticker, poll-rate keys.

`rem top --once --json` emits a **versioned envelope `rem.top.v1`**
(the CLI's stable-JSON convention, not raw proto dump) embedding the
enriched library/drive shapes — this is the TOPX scenario contract;
its field names are normative in the prompt set.

## 6. Config

```toml
[drives]
managed_libraries = []        # default: daemon-operated set
foreign_counter_poll = "60m"
foreign_tapealert  = false    # operator decision 2026-07-04
heartbeat          = "1h"

[cleaning]
auto               = true     # P2: default on, managed libraries only
voltag_prefixes    = ["CLN"]
use_warn           = 45
complete_timeout   = "10m"
min_interval       = "12h"    # per drive
weekly_cap         = 4        # per drive; exceed → alarm, not clean

[livestatus]
min_poll_interval  = "250ms"
foreign_changer_poll = "60s"  # only while a consumer is connected
```

## 7. Rollout matrix (codex fold)

- **Schema:** new tables via `MINIMUM_SCHEMA` (idempotent create),
  new columns via `ensure_column`, `tapes.kind` backfill in
  `migrate()` under `user_version` gate — safe on a live catalog.
- **Proto:** purely additive (new RPCs, new fields, new enum values).
  Old CLI + new daemon: unknown status ints render pass-through
  (catch-all arm). New CLI + old daemon: new RPCs fail UNIMPLEMENTED →
  CLI prints "daemon predates drive stewardship; upgrade rem-daemon".
  Compat tests for both directions ship in DS-M1.
- **Consumers:** sutradhara regenerates its `_proto` bindings (explicit
  step in the DS-M1 prompt set); the system harness pins `rem.top.v1`.

## 8. Verification members

- **DS-M1:** state-crate unit tests (migration + backfill, upserts,
  event append, snapshot idempotency key); **rebuild regression**
  (drive tables + `tapes.kind`/`cleaning_uses`/`cleaning_state` +
  `sessions.drive_uuid` survive `rebuild-catalog-from-journals` and a
  daemon restart — the SessionOpened audit-detail path); retire-flow
  tests incl. RPC ack/permission refusals and derived-identity
  refusal; proto compat tests; harness scenario **DRV**: archive +
  restore, then assert drives registered, sessions carry `drive_uuid`,
  both correlation views (`rem drive show` rollup, `rem catalog tape`
  rollup) answer, foreign d2lib drive present, labeled, and
  non-actionable.
- **DS-M2:** chaos extension — `VirtualDrive` dirty state (scenario-
  armed flag 20 after N ops; cleaning-cart load clears + auto-ejects
  after a realistic cycle time; armed-expired cart fast-ejects with
  flag 22). Hermetic tests: detect→fence→clean→verify→record; **crash-
  resume** (kill mid-`cleaning`, reconcile on boot); fence/session-open
  race; manual/auto join; frequency-cap alarm; no-cart alarm branch;
  expired-cart quarantine (voltag stays bound). Harness scenario
  **CLN** covers the green path + no-cart branch. QuadStor CLN
  emulation investigated for an `#[ignore]` smoke (O1).
- **DS-M3:** `GetLiveStatus` tests against the chaos model incl.
  counter-epoch reset semantics; ratatui `TestBackend` snapshot tests
  (80×24 pinned band, glyph+text states); harness scenario **TOPX**:
  poll `rem top --once --json` (`rem.top.v1`) during a large archive
  write — slot map present, drive BUSY, byte counters advancing
  between polls, alarms array well-formed.
- Design verification (skeleton compile, tape-lifecycle precedent)
  runs when prompts are cut, before codex dispatch.

## 9. Out of scope / deferred

Correlation analytics/reports (schema is the deliverable; console
later) · wear-aware drive selection (data model enables it; cost lens:
even wear extends fleet life) · MAM reading · `StreamLibraryEvents`
(S6c) · any d2tape change · system-ui surfaces (RPCs designed for it)
· snapshot rollup/retention knob (until size warrants) · TUI polish
list (§5.2) · predictive failure models.

## 10. Open questions

- **O1:** does QuadStor emulate cleaning cartridges / TapeAlert 20-22?
  (Determines whether DS-M2 gets a VTL smoke beyond chaos.)
- **O2:** which LOG SENSE pages do the virtual HH9/H7Q1 drives serve?
  (`raw_pages` absorbs whatever; counters nullable.)
- **O3:** real-hardware TapeAlert clear-on-read variance —
  persist-on-observe (§4.2) is safe under every variant; confirm on
  physical drives when they arrive.

Panel P1/P2/P3: resolved in §3.2 / §4.4 / §5.1 respectively.
