# Codex prompt — DS-M3: `rem top` live console (remanence)

**Status:** pending (dispatch after DS-M2 lands).
**Normative:** Read `docs/contract-drive-stewardship.md` first.
Design rationale: `docs/drive-stewardship-design-v0.1.md` (v0.3,
frozen) §5. Definition of done: `AGENTS.md`. Step 0 skeleton-first as
in the M1 prompt.

## Scope

1. **Counters:** per-drive `AtomicU64` lifetime read/write byte
   counters bumped in the existing append/read streaming paths, keyed
   by `drive_uuid` (NEVER bay), with `counter_epoch` semantics
   (contract §4). Active-session bytes come from existing
   `WriteSession.bytes_committed` — do not re-count.
2. **`GetLiveStatus`:** implement per contract §4 —
   `GetLiveStatusResponse` composing enriched `LibraryState`/`Drive`
   (+ `CLEANING`/`FENCED` status values, catch-all renderer arms),
   operations, alarms, `snapshot_at_utc`, `daemon_epoch`. Served from
   daemon memory + cached inventory — zero SCSI per poll;
   `min_poll_interval` enforcement. **Lazy foreign changer polling**
   via `foreign_poll_lease` recency window, UA/conflict backoff,
   stale-with-age serving.
3. **TUI:** `rem top` in `remanence-cli` behind cargo feature `tui`
   (default on; contract §9 — `rem-daemon` must not link ratatui or
   crossterm). v1 layout per design §5.2: pinned band (all-library
   alarms + drive table: bay, serial, tape voltag, state, MB/s,
   badges) over a collapsible/scrollable slot grid + ops footer;
   80×24-first; every state glyph + text (color redundant, never
   load-bearing); keys `q`/`l`/`s`/`?`, pause with prominent `PAUSED`
   banner; drive detail POINTS AT `rem drive show`, not re-implemented.
   Deferred (do NOT build): pool coloring, sparklines, in-TUI ack,
   event ticker, poll-rate keys.
4. **`rem top --once --json`:** emits `rem.top.v1` (contract §7);
   daemon-unreachable prints the banner pointing at `rem library` and
   exits 1 (correction: 2 is reserved for library-not-found per the CLI
   exit convention; uniform op-error = 1).

## Out of scope

`StreamLibraryEvents` (stays S6c); any new SCSI polling of managed
devices for display purposes; TUI polish list above; system-ui.

## Acceptance

fmt/clippy/-D warnings/full suite; plus, named: `GetLiveStatus`
against the chaos model (drive states, alarms, counter values);
counter-epoch rebaseline semantics (restart ⇒ epoch change; MB/s
never negative); lease-driven foreign polling on/off; ratatui
`TestBackend` snapshots (80×24 pinned band, glyph+text states, PAUSED
banner); feature matrix build (`--no-default-features` daemon +
CLI-without-tui compile). Diff gate before archive.

Verification member: harness scenario **TOPX** —
`~/system/docs/prompt-drive-stewardship-scenarios.md` §TOPX.
