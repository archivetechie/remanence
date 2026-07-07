# Tape Init Flow & Media Compatibility v0.1

Status: design. Specifies the `rem tape init` orchestration and the supporting
media-compatibility primitive. Companion to:
- `docs/pool-membership-design-v0.1.md` — barcode→pool derivation + the
  uuid/barcode identity model and `relabel` (the identity rules here defer to it).
- Phase-2 `2a-2` (`system/docs/phase2-2a2-design.md`) — provides the state-layer
  primitives init orchestrates (`provision_tape`, `verify_tape_identity`,
  `build_tape_bootstrap`/`write_tape_bootstrap`, `lto_generation_from_voltag`,
  `raw_capacity_bytes`).
- `docs/spec-v0.4.md` §11.4.2 (tape pools), §3 (library/topology), §8.6 (bootstrap).

## 1. Scope

The state-layer write/identity primitives already exist (2a-2). What is missing is
the **orchestration above them**: read the barcode, derive the pool, decide whether
it is even *safe* to write, choose a compatible drive, write the BOT identity,
provision the catalog row, and report. This doc specifies that orchestration plus
one reusable primitive it depends on — the LTO read/write compatibility table.

It does **not** redesign identity/membership (see the membership doc) or the
selection policy (see `pool-tape-selection`).

## 2. What `rem tape init` does

Given a target tape (by slot or already-loaded drive), init:

1. Reads the **barcode (voltag)** and **media generation** from the library
   (element status), without loading.
2. Derives the **pool** from the barcode prefix (membership doc) and validates it.
3. Chooses a **write-compatible free drive** (compat table §6) and, if needed,
   loads the tape.
4. Reads **BOT** and classifies the tape: blank / ours / foreign (§5, format
   sniffing §7).
5. Decides: fresh-init / idempotent no-op / require-`--force` / **refuse**.
6. On a go: generates a `tape_uuid` (fresh tapes), writes the BOT bootstrap,
   `provision_tape`s the catalog `ready` row, reports, and **journals** the action.

## 3. Governing principle

**Init writes a bootstrap at BOT, destroying whatever is there. Therefore init
refuses anything not *provably safe* to write.** Every check below exists to make
"is this safe?" decidable before a single byte is written. The default on any
ambiguity is **refuse**, not proceed.

## 4. The validation gauntlet

Run fail-fast, cheapest first, **no tape writes until Phase 5**. Each check is
classified **fail** (hard stop), **force** (overridable by the scoped `--force`,
§8), or **warn** (proceed, note it).

### Phase 1 — identity & pool (no load needed)
| Check | Class | Failure |
|---|---|---|
| Tape present in the target slot/drive and element-status readable | fail | "tape not found / unreadable" |
| **Barcode (voltag) present** | **fail** | "no barcode; cannot init" |
| Voltag suffix → LTO generation parses (`lto_generation_from_voltag`) | fail | "unknown media generation from voltag" |
| Voltag prefix → exactly one pool (membership rules) | **fail** | "barcode prefix not mapped to a pool" |
| Matched `pool_id` is defined in `[[tape_pools]]` | fail | config error (also caught at config load) |
| Barcode not `retired`, and not already `assigned` to a different uuid | fail | barcode-lifecycle violation (membership doc) |

### Phase 2 — media compatibility (no load needed)
| Check | Class | Failure |
|---|---|---|
| The tape generation is **write-compatible** with at least one available drive (compat table §6) | fail | "no drive can write LTO-N media" |
| The drive chosen to load it into is write-compatible with the tape gen | fail | (drive selection consumes the table) |
| Pool's expected media class == this generation, if the pool implies one | warn | mismatch surfaced |

### Phase 3 — existing-content safety (requires load + BOT read)
Read BOT and classify (format sniffing §7):
| Tape state | Class | Behaviour |
|---|---|---|
| **BLANK CHECK at BOT** (drive reports EOD immediately) **and** barcode not already `assigned` to another uuid | — | proceed → fresh init |
| BLANK CHECK at BOT **but** barcode already `assigned` to a different uuid | **fail** | media-swap anomaly — refuse (a fresh uuid here would orphan the assigned uuid's copies) |
| **Read error at BOT** (Medium/Hardware Error) | **fail** | media state unknown — refuse (fail-closed) |
| Ours: BOT uuid matches a catalog row, **same** geometry, catalog-unwritten **and** no physical data past the bootstrap | — | **idempotent no-op** |
| Ours: BOT uuid matches, but **physical data exists past the bootstrap** (interrupted/uncommitted write) | **fail** | not a no-op — resume/recovery candidate, or scoped `--force` to re-provision; never a silent overwrite |
| Ours: matches a row, **different** geometry/uuid, no physical data past bootstrap | force | re-provision a blank (scoped `--force`) |
| Ours: BOT uuid present but **no** catalog row | **fail** (in init) | not a silent init side-effect — adopt only via explicit rebuild (§5) |
| Ours but **has committed copies** (`last_committed_tape_file` set) | **fail** | refuse — would clobber data; not plain-`--force` |
| **Foreign**: a known legacy signature (BRU, legacy tar) or any non-Remanence data at BOT | **fail** | refuse — name the format; override only via `--clobber-data` (§8) |
| Barcode↔uuid anomaly (see membership doc identity table) | **fail** | refuse + surface |
| Committed copies whose snapshot pool ≠ derived pool | **fail** | `TapePoolAssignmentConflict` (`spec:1828`) |

### Phase 4 — config & hardware safety
| Check | Class | Failure |
|---|---|---|
| Pool watermark-band invariant holds for this capacity (`validate_tape_pool_capacity_invariant`) | warn→fail | catches a pool misconfigured for this media at init time |
| Cartridge not write-protected (WP tab) | fail | "tape is write-protected" — needs the WP bit surfaced (see §12 Q5) |
| Not a *used* WORM cartridge (WORM already initialized) | fail | "WORM tape already initialized" — detect via INQUIRY/Mode Sense, report clearly (not a generic error) |
| Tape is in a **Remanence-owned** library/partition (not the dwara2 LTO-7 partition) | fail | Layer-2 ownership/allowlist |
| Not concurrently being init'd (lock / idempotency key) | serialize | — |

### Phase 5 — execute & record
1. Write the BOT bootstrap (`build_tape_bootstrap` → `write_tape_bootstrap`). On a
   hardware error mid-write, do **not** leave a half-state — record
   completion-unknown and surface for operator inspection.
2. `provision_tape` → catalog `ready` row (geometry, voltag, uuid, derived pool
   membership projected from the barcode).
3. Report (uuid, voltag, pool, generation, capacity, geometry, action taken) and
   **append a journal entry** (sysadmin-documentation discipline).

## 5. Tape identity (defer to membership doc)

The uuid/barcode model, the re-init match-by-uuid table, anomaly detection, and the
`relabel` damaged-label path are specified in `docs/pool-membership-design-v0.1.md`
and are not duplicated here. Init consumes them: it matches by the BOT `tape_uuid`,
generates one only for genuinely blank tapes (BLANK CHECK *and* an unassigned
barcode), and treats any barcode-vs-catalog divergence not backed by a recorded
`relabel` as a refuse-and-surface anomaly.

**Rebuild-from-tape is explicit, not a silent init path.** Adopting a tape whose
BOT uuid has no catalog row (e.g. after catalog loss) happens only via an explicit
`rem tape rebuild` / `--rebuild`, never as a side-effect of `init` — so init cannot
silently absorb a foreign or crafted bootstrap's claimed identity.

## 6. LTO media compatibility table (reusable primitive)

A `(tape_gen, drive_gen) → { readable, writable }` matrix. **Not init-specific** —
it gates every mount and **feeds drive selection** (write-compatible drives for
writes/init; read-compatible for reads). It lives in the media/library layer next
to `lto_generation_from_voltag` / `raw_capacity_bytes` (which already parse
generation and already model `M8`).

The rule is **not uniform** — LTO-8 broke the historical "read 2 back / write 1
back," so a formula is wrong; it must be an explicit table:

| Drive | Reads | Writes |
|---|---|---|
| LTO-5 | 5, 4, 3 | 5, 4 |
| LTO-6 | 6, 5, 4 | 6, 5 |
| LTO-7 | 7, 6, 5 | 7, 6 |
| **LTO-8** | 8, 7, **M8** | 8, 7, **M8** |
| **LTO-9** | 9, 8 | 9, 8 |

Notes:
- `M8` = LTO-7 media initialized as Type-M for LTO-8 drives; `LtoGen::M8` already
  exists. An L7 cartridge can be *either* L7 or M8 depending on initialization —
  the on-shell barcode suffix and/or first-write decide; treat M8 as its own
  generation in the table.
- LTO-8 drives do **not** read LTO-6; LTO-9 drives do **not** read LTO-7; and
  LTO-9 drives do **not** read `M8` (it is LTO-7 media), only standard LTO-8/9.
  This is the load-bearing reason for an explicit table over a formula.
- The check runs **before load** (tape gen from slot element-status, drive gen from
  inventory), so an incompatible mount is refused without spinning the robot.

API shape (illustrative):
```rust
pub fn can_read(drive: LtoGen, tape: LtoGen) -> bool;
pub fn can_write(drive: LtoGen, tape: LtoGen) -> bool;
```
Drive selection (init and read/write paths) filters candidate drives through these
before considering occupancy/loaded-ness.

### Consumers: write, read, and the readability audit

The table gates drive selection on **both** paths, per operation:

- **Write / init** — a tape is write-eligible only if some available drive
  satisfies `can_write(drive_gen, tape_gen)`. In a library whose drives have
  outrun a tape's generation, that tape is simply not write-eligible — it becomes
  read-only automatically, and new writes flow to current-generation media with no
  special "prefer newest" rule. Mixed-generation pools are therefore fine (see
  `docs/pool-membership-design-v0.1.md`): generation is handled at drive selection,
  not by fragmenting pools.
- **Read / restore** — drive selection filters candidates through
  `can_read(drive_gen, tape_gen)`, and the restore path must distinguish two
  failure modes rather than collapsing them into "no free drive":
  - a read-compatible drive **exists but is busy** → **wait/block** for it;
  - **no** read-compatible drive exists in the library → a **specific hard error**
    ("object is on LTO-N media; no read-compatible drive present").

  **Deferred — flagged for the drive/mount design.** This consumption lives in the
  Layer-2a/2b drive/mount selection (`resolve_load_target`), which is not yet
  wired. The table here is the primitive it will consume; until then the restore
  path is not generation-aware.

### Generation skew & the readability audit

Because drives read back only one or two generations (LTO-9 cannot read LTO-7 at
all), an archive can drift into **silent unreadability**: as drives are upgraded or
fail, older media still in the catalog can become unrecoverable once the last
read-compatible drive is gone. A restore-time error is *too late* — the window to
migrate has already closed.

Remanence must therefore **proactively audit readability**, not merely fail at
restore: for every media generation present in the catalog, is there at least one
read-compatible drive in the owning library (ideally more than one, healthy)? When
the answer trends to "no" (or "one aging drive"), **alarm** — a
*migrate-before-unreadable* signal, the same durability discipline applied to
*future* readability rather than present integrity.

**Deferred — flagged for the scrub/health design.** It fits alongside the integrity
scrub as an availability/readability dimension keyed on the compat table + live
drive inventory; it is not part of the init slice.

## 7. Format detection (sniff)

"Blank / ours / foreign" is decided by composing per-format detectors from the
format registry, not by ad-hoc byte checks in init:
```rust
// each format crate provides one:
fn sniff(bot_bytes: &[u8]) -> Option<FormatId>;
```
- `rem-tar-v1` / the bootstrap parser → "ours" (then verify uuid/geometry).
- `remanence-tar-legacy`, `remanence-bru` → name the foreign format for a clear
  refusal ("this is a BRU tape").
- **Blank is decided by the drive, not by reading zeros.** A tape is blank only if
  a read at BOT returns the SCSI `BLANK CHECK`/EOD condition (the library already
  synthesizes this — `block_io.rs:433 synth_blank_check_eod_sense`). Reading *any*
  data at BOT — even all-zero blocks — means the tape is **not** blank: classify it
  foreign and refuse. (A zero preamble or an unknown format that doesn't start at
  byte 0 must never be mistaken for blank.)
- A **read error** (Medium/Hardware Error) at BOT → **refuse** (fail-closed): media
  state is unknown, so the write path is blocked.

This keeps detection co-located with each format's definition; init just asks the
registry.

## 8. Modes

- **`--dry-run`** — run Phases 1–4 and report what init *would* do or *why it would
  refuse*, **writing nothing**. First-class: it lets an operator validate a whole
  magazine before committing a byte, which is the main guard against batch
  mislabeling.
- **Batch** (a magazine / slot range) — per-tape gauntlet, summary table; choose
  stop-on-first-error vs continue-and-report.
- **`--force`** — scoped to the Phase-3 "ours, unwritten (catalog *and* physically
  clean past the bootstrap), geometry/uuid change" case *only*.
- **`--clobber-data`** — the separate, visibly-dangerous override for the
  data-clobber refusals (committed copies / foreign content / uncommitted physical
  data). Requires per-tape interactive confirmation and is **rejected** in
  `--dry-run` and batch mode. Plain `--force` never overrides a data-clobber refusal.

## 9. VTL-agnostic

Init runs the standard SCSI/library path (element status for barcode+gen, the drive
for BOT read/write). It works against the QuadStor VTL transparently — Remanence
never branches on "is this a VTL." The only non-hardware substrate is the in-memory
fakes (`VecBlockSink`, in-memory `Library`) used by **unit tests**, not a runtime
mode. The compat/identity/pool logic is pure and unit-testable without hardware;
the hardware I/O sits behind the Layer 2/3a library/drive abstraction.

## 10. Reuse vs new surface

**Reused (2a-2 / existing):** `provision_tape`, `verify_tape_identity`,
`build_tape_bootstrap`/`write_tape_bootstrap`, `lto_generation_from_voltag`,
`raw_capacity_bytes`, `validate_tape_pool_capacity_invariant`, element-status
discovery (Layer 2a), `DriveBay.loaded_tape`.

**New:**
- LTO compat table (`can_read`/`can_write`) in the media layer.
- Format `sniff()` per format crate + a registry compose.
- Barcode→pool derivation + `[[tape_pool_rules]]` config (membership doc).
- Barcode lifecycle (`available`/`assigned`/`retired`) enforcement.
- The init orchestration + its phased error type, and the `rem tape init`
  (and `--dry-run`/batch) CLI surface.

## 11. Implementation notes

- **Pure-logic first, hardware behind the abstraction.** The compat table, pool
  derivation, identity matching, format-sniff composition, and the gauntlet
  *decision* logic are pure and fully unit-testable. The barcode/gen read and the
  BOT read/write go through the Layer 2/3a library/drive handles; where those are
  not yet wired, init runs end-to-end on the in-memory `Library`/`BlockSink` fakes
  and the real-hardware path lands as that abstraction is completed. Be explicit in
  the PR about what is hardware-deferred.
- **Borrowed-handle caution** (rust-design-verification cat. 4): init may need a
  drive handle (write BOT) and library state (element status) together — factor the
  borrow so it split-borrows cleanly rather than taking two `&mut` of the same
  parent. Verify with a compiling skeleton before locking signatures.
- **Error type**: a single `TapeInitError` enum mapping each gauntlet failure to a
  distinct variant (and to a stable gRPC status), so `--dry-run`/batch can report
  per-tape reasons (cf. the `NoWritableTapes`-reasons issue — don't swallow them).
- Gates as always: `fmt` / `clippy -D warnings` / `check` / `test --workspace`.

## 12. Open questions / decisions

1. **Barcode convention** (membership-doc precondition) — confirm the deployment scheme
   has prefix room + per-pool spare ranges. Gates real config, not the code shape.
2. **M8 detection** — *resolved:* distinguish L7 vs M8 by the **voltag suffix**
   (`…L7` vs `…M8`) as the authority for intended format (an L8 drive only writes
   M8 when directed). The suffix drives the capacity + compat lookup.
3. **Phase-4 invariant on mismatch** — warn vs fail when a pool's watermark band is
   wrong for the just-init'd media's capacity.
4. **Foreign-data conservatism** — *resolved:* blank is the SCSI `BLANK CHECK`
   condition; any readable data at BOT (incl. zeros) without our signature →
   refuse. No deeper-than-BOT scan is needed because BLANK CHECK reflects true
   EOD-at-BOT; uncommitted-data detection uses physical position past the bootstrap.
5. **Write-protect / WORM surface** — Phase-4 WP and WORM checks depend on extending
   the mode-sense parser to surface the **WP bit (header byte 2)**:
   `parse_mode_sense_data_compression` (`tape_io/mod.rs:1494`) currently returns only
   block size + compression. WORM detection via INQUIRY/Mode Sense. Until that lands,
   the WP/WORM checks partially defer (Phase 4 fails *open* on these two only, with a
   logged warning — never fails *closed-incorrectly*).
6. **Batch ergonomics** — stop-on-first vs continue-and-summarize as the default.
