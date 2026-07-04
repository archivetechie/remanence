# Drive stewardship — catalog, auto-cleaning, live console — Design v0.1

**Status:** draft for panel review (2026-07-04).
**Problem source:** operator request 2026-07-04 — three gaps: no drive
history (the D2-era `devices` table never answered "tape fault or drive
fault?"), no automatic cleaning, no live combined library+drive view
(`mtx status` + `mt status` era tooling).
**Operator decisions (2026-07-04):** cleaning policy = fence-after-session
(option chosen over idle-only and advisory-only); catalog scope =
observe-all/act-on-own approved in direction, with the foreign-drive
boundary explicitly flagged for panel examination (see §3.2).
**Precedent docs:** `tape-identity-lifecycle-design-v0.1.md` (retire
pattern this design mirrors), `chaos-phase-d-tapealert-design-v0.1.md`
(TapeAlert reader; its deferred "Phase D2" wear counters land here),
`layer5-phase3c-drive-status-busy-design-v0.1.md` (drive status
projection `rem top` reads).

---

## 1. Problem

1. **No drive records.** The catalog has no drives table
   (`MINIMUM_SCHEMA`, index.rs:4172-4337 — tapes, sessions, objects…
   nothing for drives). `sessions` records `tape_uuid` + `drive_bay`
   (index.rs:4327-4336) but a bay is a position, not a device — swap
   drives between bays and history silently re-attributes. When a
   restore misbehaves there is no data to ask: is this tape bad, or is
   this drive bad on every tape it touches?
2. **No cleaning automation.** TapeAlert flags 20/21/22 ("clean now",
   "clean periodic", "expired cleaning cartridge") are parsed and named
   (log_sense.rs:91-93) and surfaced by `rem tape alerts`, but nothing
   consumes them. The CLN barcode prefix is a cosmetic display suffix
   only (`print_slot`, remanence-cli lib.rs:8754-8770). Dirty heads on
   an archival write path are a durability risk being managed by hand.
3. **No live visibility.** `rem library --slots` is a one-shot direct-
   SCSI snapshot; no busy state, no throughput, no combined view.
   `StreamLibraryEvents` is stubbed unimplemented (library.rs:433-439,
   roadmap slice S6c).

## 2. Design overview

Three stacked milestones under one design (the copygrain CG-M1/2/3
shape): **DS-M1 drive catalog** is the foundation; **DS-M2
auto-cleaning** is the first automated consumer writing events into it;
**DS-M3 `rem top`** is the read surface over everything. Each milestone
is its own codex prompt set with its own verification member.

Identity rule for the whole design: **drives are things, bays are
positions.** Everything keys on drive serial; bay/element address is an
observation that changes over time and is recorded per event.

## 3. DS-M1 — drive catalog

### 3.1 Identity

Serial comes from discovery's `InstalledDrive` (model.rs:141-150),
which already carries `identity_source` (Reported vs Derived). The
catalog stores the source alongside the serial; drives with Derived
identity are labeled as such and action verbs against them keep the
existing `AccessPolicy` opt-in semantics (`--allow-derived`). Firmware
revision is captured per observation; a revision change is an event,
not an identity change.

### 3.2 Managed vs foreign (the scope decision)

`drives.managed` ∈ {`rem`, `foreign`}. A drive is `rem` when it lives
in a library the daemon operates; everything else discovered on the box
(today: d2lib's Ultrium-7 used by legacy d2tape) is `foreign`.
Determination: a config list of managed library serials, defaulting to
the libraries the daemon is configured to drive — not hardcoded, since
two VTLs exist and enumeration must filter (CLAUDE.md).

The boundary:

- **Foreign drives are observed, never acted on.** Identity
  registration + passive health snapshots (LOG SENSE is read-only and
  never touches tape position — safe against a drive mid-d2tape
  restore, with a short timeout and back-off on BUSY/reservation
  sense). No robotics, no cleaning, no fencing — `rem drive clean` on a
  foreign drive refuses with a message naming the owning library.
- **Labeling, not blindness, handles the confusion risk.** Every CLI
  and `rem top` surface renders `[foreign: d2lib]` on foreign drives.
- **Payoff:** tape-vs-drive attribution covers d2tape restores (the
  original D2-era pain) at the health-counter level with zero d2tape
  code changes; when d2tape retires, its drives flip to `managed=rem`
  with history intact — exactly what repurposing the Ultrium-7s wants.

**Panel question P1:** attack this boundary. Specifically: (a) is
passive LOG SENSE against a foreign drive genuinely risk-free under
QuadStor and on real hardware; (b) does observing foreign TapeAlert
state (read-and-clear, §4.2) consume signal a future d2tape tool would
want; (c) is `managed` a config property or a per-drive override?

### 3.3 Schema (remanence-state, `ensure_column`/user_version migration)

```sql
create table drives(
  drive_serial text primary key,
  identity_source text not null,          -- 'reported' | 'derived'
  vendor text, product text, firmware_rev text,   -- latest observed
  managed text not null,                  -- 'rem' | 'foreign'
  state text not null default 'active',   -- 'active' | 'retired'
  cleaning_due text not null default 'none', -- 'none'|'periodic'|'now'
  first_seen_utc text not null, last_seen_utc text not null,
  last_library_serial text, last_element_address integer,
  -- operator annotations (rem drive annotate)
  purchase_date text, warranty_until text, cost text, notes text,
  retired_at_utc text, retire_reason text
);
create table drive_events(       -- append-only
  event_id integer primary key,
  drive_serial text not null references drives(drive_serial),
  event_kind text not null,   -- first-seen|bay-moved|firmware-changed|
                              -- alert|cleaned|cleaning-failed|fenced|
                              -- unfenced|annotated|retired|reappeared
  at_utc text not null,
  library_serial text, element_address integer,
  tape_uuid blob,             -- tape involved, when relevant
  detail text                 -- JSON, kind-specific
);
create table drive_health_snapshots(
  snapshot_id integer primary key,
  drive_serial text not null references drives(drive_serial),
  at_utc text not null,
  trigger text not null,      -- 'interval'|'session-close'|'alert'
  tape_alert_flags text,      -- JSON array of active flag numbers
  write_errors_corrected integer, write_errors_uncorrected integer,
  read_errors_corrected integer, read_errors_uncorrected integer,
  raw_pages text              -- JSON: whatever pages the device served
);
-- sessions: add drive_serial (nullable; stamped at open from the
-- DrivePool bay→serial map). tape_uuid × drive_serial × state is THE
-- correlation join this whole milestone exists for.
```

Error counters come from LOG SENSE pages 02h/03h (write/read error
counter pages). VTLs serve few pages; every counter column is nullable
and `raw_pages` keeps everything the device returned, so real-hardware
richness needs no schema change.

### 3.4 Collector

A daemon background task (sibling to the existing actors):

- **Managed drives:** snapshot at every session close (the high-value
  moment — the tape, the bytes, the error deltas are all in hand) plus
  an idle-interval poll (default 15 min). Never polls a bay with an
  active session; goes through the bay actor so SCSI access stays
  single-owner.
- **Foreign drives:** passive snapshot on a slower interval (default
  60 min), direct sg open, read-only, timeout + back-off; failures mark
  the snapshot stale rather than alarm.
- **Inventory diffing:** each discovery/refresh upserts `drives`
  (first-seen, bay-moved, firmware-changed, reappeared-after-retire
  events). A drive vanishing from inventory is NOT auto-retired —
  retirement is explicit and terminal, mirroring tapes. A retired
  serial reappearing raises a `reappeared` event + standing alarm.

### 3.5 Lifecycle + CLI

`rem drive retire <serial> --reason … --i-understand-history-is-final`
mirrors `rem tape retire` (terminal, idempotent, audited, refuses while
the drive has an active session; history rows are preserved forever).
Audit events (additive `AuditEvent` variants, same pattern the tape
lifecycle used): `DriveRetired`, `DriveAnnotated`, `DriveCleaned`,
`CleaningCartridgeExpired`.

```
rem drive list [--foreign] [--retired]     # labels [foreign: d2lib]
rem drive show <serial>       # identity, annotations, cleaning_due,
                              # last snapshot, recent events, per-tape
                              # session/error rollup (the correlation view)
rem drive history <serial> [--events|--snapshots] [--json]
rem drive annotate <serial> --warranty-until … --purchase-date … \
                            --cost … --notes …
rem drive retire <serial> --reason … --i-understand-history-is-final
rem drive clean <serial>      # M2, manual trigger of the same actor
```

Transport mirrors the existing tape verbs (daemon RPCs for reads;
lifecycle mutation takes the same path `rem tape retire` takes today).
New RPCs on the catalog service: `ListDrives`, `GetDrive`,
`GetDriveHistory`, `AnnotateDrive`, `RetireDrive` — designed so the
web console can consume them later without rework.

## 4. DS-M2 — auto-cleaning

### 4.1 Cleaning cartridges are first-class catalog citizens

`tapes.kind` ∈ {`data` (default), `cleaning`} via `ensure_column`.
Classification at inventory/registration time by voltag prefix `CLN`
(config-extensible). Cleaning tapes are excluded from pool derivation
(`derive_tape_pool_from_voltag`, config.rs:339), tape-init, write/read
session eligibility, and durability accounting — everywhere the catalog
reasons about data tapes, `kind='cleaning'` is filtered at the query
edge, not by callers remembering to check.

New columns for cleaning tapes: `cleaning_uses integer` (incremented
per completed clean; LTO Ultrium cleaning carts are good for ~50 uses)
and warn threshold in config (default 45). Expiry — via TapeAlert 22
or use-count exhaustion — retires the tape through the existing
lifecycle (`state='retired'`, reason `cleaning-expired`) and raises a
replace-cartridge alarm.

### 4.2 Detection — persist on first observation

TapeAlert pages are **read-and-clear** on real drives (behavior varies
by flag and vendor; the LTO SCSI reference is explicit that a read may
clear). Therefore the design never treats a live flag read as
re-checkable state: any observation of flag 20/21 immediately persists
`drives.cleaning_due = 'now'|'periodic'` (monotonic: `periodic` never
downgrades `now`), plus an `alert` event. `cleaning_due` is cleared
only by a successful clean. Detection points:

- session close (managed drives — `read_tape_alerts` already exists,
  tape_io/mod.rs:389; it becomes part of the close path),
- the M1 idle poll,
- foreign drives: observation sets `cleaning_due` + a standing
  advisory alarm ("d2lib drive X wants cleaning") — never an action.

### 4.3 The cleaning actor

Executes inside the daemon through the existing single-owner machinery
(changer actor + bay reservation in `DrivePool`, write_owner.rs:143):

1. **Trigger.** `cleaning_due='now'` → fence the bay (no new sessions;
   current session runs to completion — the operator-chosen
   fence-after-session policy). `cleaning_due='periodic'` → no fence;
   wait for natural idle. Manual `rem drive clean` follows the `now`
   path.
2. **Select cartridge.** Non-expired `kind='cleaning'` tape in a slot
   of the same library. None available → standing alarm
   `cleaning-required-no-cartridge`; the drive stays in service for
   data (loud degradation, not a halt); re-alarm on interval.
3. **Clean.** MOVE MEDIUM slot→drive. The drive runs its cleaning
   cycle autonomously and auto-ejects (typically 1–3 min). Completion
   detection: poll element status until the drive bay reports the
   cartridge ejected/absent; timeout default 10 min.
4. **Return + record.** MOVE MEDIUM drive→home slot; clear
   `cleaning_due`; `cleaned` event + `DriveCleaned` audit +
   `cleaning_uses` increment; unfence.
5. **Failures.** Flag 22 mid-clean → retire that cartridge
   (§4.1), try another, else alarm. Move/timeout failures → recovery
   per the chaos phase E changer-fault patterns; alarm; never leave the
   bay fenced without a standing alarm saying why.

Safety envelope for autonomous robotics (new ground — the daemon moves
media without an operator command): only `kind='cleaning'` cartridges
are ever auto-moved; only slot↔drive within one managed library; every
move audited; config kill-switch `cleaning.auto = true|false`
(recommended default **on** for managed libraries — that is the
feature — panel to challenge, **P2**).

## 5. DS-M3 — `rem top`

### 5.1 Data path: poll a snapshot, don't stream (v1)

New RPC `GetLiveStatus` on LibraryService — a cheap snapshot served
from daemon in-memory state (DrivePool status, active operations,
cached inventory, alarms). The TUI polls at 1–2 Hz. Rationale: for a
screen refreshing at eye speed, polling is drastically simpler than
finishing `StreamLibraryEvents`, and S6c can later feed the same UI
without layout changes. Snapshot carries:

- per library (managed + foreign): serial, model, managed flag,
  `last_inventory_at`, slot array (address, voltag, kind, pool,
  occupancy), IE ports;
- per drive: serial, bay, vendor/product/firmware, managed, status,
  loaded tape (voltag + uuid), active session (id, kind, bytes so
  far), **cumulative read/write byte counters**, `cleaning_due`,
  active TapeAlert names, last-snapshot age;
- active operations (kind, state, progress) and standing alarms.

`Drive.Status` (layer5.proto:194-210) gains additive enum values
`CLEANING` and `FENCED`.

**Throughput:** per-bay `AtomicU64` read/write byte counters bumped in
the existing append/read streaming paths, exposed cumulatively in the
snapshot; the client computes MB/s from deltas between its own polls
(stateless daemon, no sampling thread). The existing
`remanence_write_diag` tracing lines (diagnostics.rs) are untouched.

**Foreign library inventory:** the daemon passively polls the foreign
changer (READ ELEMENT STATUS, read-only, default 60 s) so d2lib's slot
map appears labeled `[foreign]`. Concurrent element-status reads
against a changer another initiator is driving are read-only; BUSY →
serve stale data with its age, never alarm-spam. **Panel question P3:**
sanity of passive foreign-changer polling vs leaving d2lib static
(inventory only at explicit refresh).

### 5.2 TUI

`rem top` (new `RemCommand`), ratatui + crossterm (new deps,
remanence-cli only). Daemon client only — no direct-SCSI live mode by
design (the daemon owns devices; a second SCSI poller would fight it).
Daemon unreachable → banner suggesting `rem library` (the existing
one-shot direct path).

Layout: header = daemon health/version, alarm count; per library: drive
table (bay, serial, tape, colored state, MB/s, session, alert/cleaning
badges) over a compact slot grid (barcode cells, `C` = cleaning tape,
IE ports, pool coloring); footer = active operations + recent drive
events ticker. Keys: `q` quit, `l` cycle library, `d` drive detail
(annotations, warranty, history sparkline), `s` slot grid toggle, `p`
pause, `+`/`-` poll rate.

`rem top --once --json` prints one `GetLiveStatus` snapshot and exits —
the scripting/harness hook, and the verification surface for scenarios.

## 6. Config (one `[drives]`/`[cleaning]` block, all defaults sane)

managed_libraries (default: daemon-operated set) · poll intervals
(managed 15 m / foreign 60 m / foreign changer 60 s) · cleaning.auto
(on) · cleaning.voltag_prefixes ([CLN]) · cleaning.use_warn (45) ·
cleaning.complete_timeout (10 m).

## 7. Verification members (one per milestone, per working pattern)

- **DS-M1:** state-crate unit tests (migration, upserts, event append,
  session `drive_serial` stamping through the ModelTransport session
  paths); retire-flow tests mirroring the tape-retire suite; harness
  scenario **DRV**: run an archive+restore, assert drives registered,
  session rows carry serials, `rem drive show` rollup non-empty,
  foreign d2lib drive present and labeled.
- **DS-M2:** chaos extension — `VirtualDrive` dirty state: scenario-
  armed flag 20 after N operations; loading a `kind='cleaning'` cart
  clears it and auto-ejects; an armed-expired cart raises flag 22.
  Hermetic tests for the full detect→fence→clean→record loop; harness
  scenario **CLN**: arm dirty drive, run archive, assert cleaned event,
  use-count bump, `cleaning_due` cleared, and the no-cartridge alarm
  branch. Investigate QuadStor CLN emulation for an `#[ignore]` smoke
  (open question O1 — chaos covers the logic regardless).
- **DS-M3:** `GetLiveStatus` tests against the chaos model; ratatui
  `TestBackend` snapshot tests for layout; harness scenario **TOPX**:
  poll `rem top --once --json` during a large archive write, assert
  slot map, drive BUSY state, and byte counters advancing between two
  polls.

Design verification (skeleton compile per the tape-lifecycle
precedent) runs when prompts are cut, before codex dispatch.

## 8. Out of scope / deferred

- Correlation analytics & reports (the schema is the deliverable;
  queries/console views come later; console P3 read-models can adopt
  the drive RPCs).
- Wear-aware drive selection (pick least-worn drive for a job) —
  natural follow-up the data model enables; cost lens: even wear
  extends fleet life.
- MAM (cartridge memory) reading; `StreamLibraryEvents` (stays S6c);
  any d2tape code change; system-ui surfaces; predictive failure
  models.

## 9. Open questions

- **O1:** does QuadStor emulate cleaning cartridges / TapeAlert 20-22?
  (Determines whether M2 gets a VTL smoke beyond chaos.)
- **O2:** which LOG SENSE pages do the virtual HH9/H7Q1 drives actually
  serve? (`raw_pages` absorbs whatever; nullable columns already assume
  "few".)
- **O3:** real-hardware TapeAlert clear-on-read behavior varies by
  vendor — persist-on-observe (§4.2) is designed to be safe under
  every variant; confirm on physical drives when they arrive.

Panel: P1 (§3.2 foreign boundary), P2 (§4.3 auto-clean default on),
P3 (§5.1 passive foreign-changer polling).
