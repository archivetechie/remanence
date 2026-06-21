# Code review — chaos Phase C (`ModelTransport` + L1b), 2026-06-21

**Scope:** the implementation of `docs/prompt-chaos-phase-c.md` /
`docs/chaos-phase-c-modeltransport-design-v0.1.md` — commit `e7e99b6`
("Implement chaos Phase C ModelTransport"). New
`crates/remanence-chaos/src/model.rs` (1,774 lines: `ModelTransport` +
`VirtualWorld` + L1b tests), a 7-line `lib.rs` wiring, an 8-line change to
`crates/remanence-parity/src/source.rs`, and Cargo/docs/report/journal.

**Method:** read the parity production change and the L1b test assertions
line by line; dispatched a parallel deep review of the `model.rs` CDB handlers
against the authoritative encoders in `remanence-scsi` and the real consumers in
`remanence-library`; independently verified the two flagged "High" items in
source.

**Gates (green):** `cargo fmt --check` clean; `cargo clippy -p remanence-chaos
--all-targets -D warnings` clean; `cargo test -p remanence-chaos` **19 passed /
0 failed**; `cargo test -p remanence-parity` **405 + 27 passed / 0 failed**
(the production change is safe across the full parity suite).

## Verdict

**Strong, faithful implementation — no Critical/High, no blockers.** The model
is byte-exact against every consumer parser checked (discovery, data path, sense
shapes, READ ELEMENT STATUS framing); no reachable panic from a short/edge CDB
(all CDB access is length-checked or `.get()`-based); no deadlock (one world lock
per handler, dropped before return; the inline READ POSITION re-locks after the
prior guard drops). The L1b suite drives the **real** parity + RAO-format stack
through `ChaosTransport<ModelTransport>` over a genuine `DriveHandle`, and the
assertions match the design's per-fault detection table exactly. No
`remanence-library` change, as designed.

The notable item is a small **production change to `remanence-parity`** made to
let the test reuse a convenience wrapper (L1 below) — benign and safe, but
avoidable. Everything else is Low/Nit or latent-for-future-phases.

## Findings

### L1 (Low) — production `remanence-parity` change to enable a test; avoidable, untested in the parity crate
`crates/remanence-parity/src/source.rs:1000-1007`. `ObjectParitySource::space`
now short-circuits `count == 0` to a no-op **before** the `kind != Blocks`
rejection, so `read_object_payload(…, tape_file_number = 0, …)` (which always
issues `space(0, Filemarks)`) composes over an object-local source.
- **Safe:** `ObjectParitySource::space` is called by **no production path**
  (the daemon read path and `raw.rs` operate on raw `DriveHandleSource`, not
  `ObjectParitySource`). The new branch returns an observably **identical**
  `SpaceResult` to the old `count==0` Blocks path (same `units_traversed`,
  `stopped_at_boundary`, `position_after`); it only drops incidental
  side-effects (read-cache clear + a redundant re-locate to the current LBA),
  which a zero-length space arguably should not do anyway. Full parity suite
  green.
- **But:** it's a production-crate edit in a phase whose stated ethos was
  "production builds unchanged," and it was **avoidable** —
  `stream_rem_tar_object_with_manifest_anchor` is `pub`-exported and the
  `ObjectParitySource` opens already positioned at object start, so the test
  could call the streamer directly and skip the `space(0)` entirely. The new
  branch also has no `remanence-parity` unit test (it's exercised only
  indirectly from `remanence-chaos`).
- **Recommendation (non-blocking):** prefer reverting source.rs and calling the
  streamer directly in the L1b read helper; *or*, if the team wants
  `space(0, _)` as a deliberate idempotent-no-op API generalization, keep it but
  add a `remanence-parity` unit test and a one-line doc comment. Either is fine;
  don't leave it as a silent test-driven prod change.

### L2 (Low, latent) — `written_bytes` not decremented on overwrite-truncate; EOM can over-count after rewind+rewrite
`crates/remanence-chaos/src/model.rs:423-428`. `write_6` truncates trailing
records when writing mid-tape (`records.truncate(position)`) but `written_bytes`
only ever `saturating_add`s. After a rewind + overwrite, `written_bytes` keeps
climbing, so the virtual EOM threshold could trip early. **Not exercised by
L1b** (the suite writes append-only after a fresh load; the only rewind is
followed by reads, not rewrites), so it doesn't affect Phase C. **Fix before**
any rewrite-after-rewind scenario (Phase D/E): recompute `written_bytes` from
surviving records on truncate, or subtract the truncated byte count.

### Investigated, not a defect — `drive_source_slots` on changer unload
The parallel review flagged a possible stale `drive_source_slots[bay]` (stale
SVALID on an emptied drive). **Verified false:** `take_from_element`
(`model.rs:864`) sets `drive_source_slots.insert(bay, None)` on bay-drain, and
the drive descriptor reads `drive_source_slots.get(&bay).flatten()` with `full =
barcode.is_some()` (`model.rs:246-251`, `descriptor_with_voltag`), so an emptied
bay correctly emits `full=false`, no SVALID. Unload is handled correctly.

### Nits (no action)
- `vpd80_response` byte 0 is hardcoded `0x00` (peripheral type) rather than
  echoing the device type; the `0x80`-page parser doesn't check it, so harmless.
- EOD-on-read is modeled as a plain BLANK CHECK `CheckCondition` (not a
  distinguished EOD signal); `read_block` never special-cases it, and L1b never
  reads past EOD, so harmless. Worth a code comment.
- READ POSITION never sets BPEW/EOP; EOM early-warning is signaled via sense
  (which is what `write_block` consumes), so the EW test passes. Set BPEW if a
  future test asserts position-based early-warning.

## Verified conformant (evidence retained)

- **Discovery/open byte-exact:** INQUIRY (real captured fixtures, device type
  drives `open_drive`'s `SequentialAccess` / changer `MediumChanger` checks),
  VPD 0x80 (4-byte header + world serial → matches `revalidate_serial`), READ
  BLOCK LIMITS (max at bytes 1-3 BE), MODE SENSE(6) p0x0F (BDL=8, block length
  9-11, DCE byte14 bit7, WP byte2 bit7, medium type → `NotWorm`) — all parse in
  the real `read_config`.
- **Data path + inline READ POSITION contract:** WRITE/READ(6) variable
  (byte-count transfer length), WRITE FILEMARKS, SPACE(6/16), LOCATE(16), READ
  POSITION long (LBA bytes 8-15 BE). Every WRITE/LOCATE/SPACE/WRITE-FILEMARKS
  updates `self.position`; the separate inline `execute_in(0x34)` the real
  `DriveHandle` issues afterward reads back the updated position. Record-oriented
  model is internally coherent (write appends, read returns+advances, filemark
  read advances past the mark).
- **Boundary sense byte-exact:** `fixed_sense` (resp code 0x70, VALID bit7 when
  INFORMATION present, byte2 = key|flags, signed i32 INFORMATION at 3-6, addl-len
  24, ASC/ASCQ at 12/13). EOM-on-write (EOM bit, key 0x00 early-warning) parses
  via `write_eom_signal`; FILEMARK-on-read (FILEMARK bit, key 0, ASC/ASCQ 00/01,
  ILI clear) via `read_filemark_signal`.
- **READ ELEMENT STATUS strict framing:** 8-byte header (first addr,
  num_elements, byte_count u24), per-type pages (PVOLTAG, desc_len, page byte
  count an exact multiple), 36-byte voltag block, DVCID for drives. `num_elements`
  == descriptor count and `byte_count` exact → satisfies the strict parser.
  Two-phase header-probe safe.
- **MOVE MEDIUM** mirrors `ops::apply_planned_move`: moves barcode src→dst, sets
  `drive_bays[bay]` + `drive_source_slots[bay]` on a drive destination, clears on
  drain.
- **L1b assertions match the design's detection table:** MED-05 →
  `FormatError::FileDigestMismatch{path=="payload.bin"}` (or `ManifestDigestMismatch`)
  at the **digest layer** + JSONL event (seed, lba, mutation offset 32/len 64);
  EOM → `early_warning=true, end_of_medium=false`; MED-01 ≤ m →
  `RecoveryOutcome::Recovered`; MED-01 > m → `ParityError::Unrecoverable` +
  `RecoveryOutcome::Unrecoverable{lost_count:2}`; combined MED-01+MED-05-peer →
  `Unrecoverable{lost_count:2}` (sidecar CRC-64 guard); changer coupling →
  loaded barcode bound, `read_back != original`. Plus a faithful chaos-disabled
  round trip. 19 tests.

## Net

Phase C is complete and correct for its L1b scope: a real Remanence write→read
workflow runs hermetically through a stateful fake SCSI device, and the
MED-05/EOM/RS-recovery properties are proven at the architecturally-honest
layers (digest for silent corruption, RS for erasures). Two small cleanups are
worth doing — the `remanence-parity` `space(0)` change (L1: revert-to-streamer
or test+doc it) and the latent `written_bytes` accounting (L2, before Phase
D/E) — neither blocks the phase. Ready, with those follow-ups noted for codex.
