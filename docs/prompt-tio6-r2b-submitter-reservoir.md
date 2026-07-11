# Prompt TIO-6 R2b — read submitter + reservoir (watermark stop-start) + ranged proof-frontier + diag (PIPELINE)

**Status:** pending. **Dispatch AFTER R2a lands** (it provides the `BlockRead` sub-trait,
the `ReadDelivery`/`SequencedHandoff`/`ProofFrontier` types, the funnel hardening, and the
error-precedence accumulator this stage wires up).
**Normative (read FIRST, binding — do NOT inline):** `docs/design-tape-io-read-pipeline-v0.1.md`
(**FROZEN v0.4**): §3.1 (read submitter hot loop), §3.2/§3.3 (reservoir pool, channels,
error-carrying delivery, decode+sender consumer), §4 in full (reservoir watermark stop-start,
§4.3 re-proof, §4.5 liveness, §4.6 RAM reservation, §4.8 config), §5.2 (staged-cancel), §5.5
(ranged proof-frontier), §8/§3.5 (diag), §9 (invariants + crash rows), §10 (rows below). Also
read `crates/remanence-api/src/read_core.rs` (BatchingBlockSource/refill/ChannelWriter — R1),
`crates/remanence-api/src/write_owner.rs` (the existing two-thread restore relay
`stream_with_staged_read_sender_diagnostics` / `drain_staged_read_sender`).

## Scope — the read pipeline, exactly per the frozen design
1. **Read submitter (§3.1):** the drive-actor thread becomes a hot READ submitter — pop a free
   reservoir buffer, recompute the clamp, call the R2a funnel unchanged, push
   `SequencedHandoff`/`ProofFrontier` into delivery. **Plan-bounded read-ahead:** the staged-next
   carries the BUFFER ONLY; the record count is recomputed as `min(batch, submitter_remaining)`
   before EVERY issue (never carried) — total records issued == plan exactly (full object, ranged
   incl. first-block offset, trailing partial). Staged READ is host memory only (never in the
   kernel) so cancel-on-prior-residual is pure control flow (§5.2). Exactly ONE SCSI data command
   in flight, ever (§9.1).
2. **Consumer retype (§3.3):** `BatchingBlockSource` → `HandoffBlockSource` whose `refill` is a
   channel `recv` — holds ONLY channels (no drive handle, per R2a `BlockRead`); decode-side
   `remaining` slaved to Σ `handoff.records_read` (never recomputed; Σ received ≠ Σ issued at close
   ⇒ fail-closed). Keep the decode→sender split, byte-sized channel. `HandoffBlockSource` validation
   (byte/record mismatch, filemark-early, zero-record) reproduces today's `refill` errors
   byte-for-byte.
3. **Reservoir + watermark stop-start (§4 — CORE):** watermark-controlled host-RAM reservoir; fill,
   park at high-water (stop issuing READs — drive parks), drain, resume at low-water. Reposition
   count bounded by construction. **§4.3 re-proof:** EVERY deliberate park→resume (any duration,
   incl. zero) requires a passing RP before the next READ (`GATED → RESUMING(rp-pending) → FILLING`
   precondition); the resume RP's `ProofFrontier` names the LAST COMPLETED command's seq/cursor
   (NO off-by-one — a fixture crediting the next command must FAIL); `T_REPROOF_INCIDENTAL` governs
   only incidental free-buffer waits. **NO IN_BAND regime, no park detector, no per-command RP** —
   removed by owner; single regime only.
4. **RAM reservation manager (§4.6):** a fixed `daemon.io_memory_ceiling`; an ATOMIC reservation
   shared by reservoir slabs + the 5b spool; reserve→alloc→mlock with permit rollback on
   alloc/mlock failure; Σ granted permits ≤ ceiling under a concurrent-growth RACE (two streams +
   spool); minimum-pool mlock failure ⇒ REFUSE-to-start (no swappable fallback); growth-step denial
   non-fatal (effective cap, warn once). tmpfs spool is ceiling-reserved but expressly swappable.
5. **Liveness (§4.5):** slow-but-alive consumer parks INDEFINITELY (drive never moves); DEAD/
   half-open peer detected via receiver-drop + the h2 keepalive PING (server+client 30 s/20 s,
   `keep_alive_while_idle`) within the §4.5 bound ⇒ teardown/poison-drain, drive never moves.
6. **Ranged proof-frontier (§5.5):** ranged restore rides the SAME pipeline; the hash-less
   deliver-ahead frontier advances only on in-seq-order `Device` evidence; bytes past
   `proven_frontier` withheld until proof lands; withheld bytes counted as reservoir occupancy and
   gate the submitter; **proof-on-demand RP before park drains the withheld queue (NO deadlock)**;
   proof failure ⇒ every withheld handoff discarded; cadence clamp ≤ half effective reservoir;
   full-object exemption. Rework `read_plaintext_file_range` onto the pipeline (one path).
7. **Diag (§8/§3.5):** per-phase decomposition, `feed_gap_us` = gap − free_wait − park, cross-thread
   counters atomic or post-join; the dual-sided below-streaming-rate / reposition-rate Drishti signal.
8. **Channel sizing (frozen v0.4 doc-minor):** size free/delivery channels at window-creation for
   the effective max slab count + fixed proof-only slots; assert `allocated ≤ capacity` (bounded
   channels don't grow); account every buffer across free/delivery/in-flight/decode/pending-proof.

## Binding invariants
Exactly ONE SCSI command in flight; wrap the R2a funnel (never fork); decode thread reaches NO
drive method (by type); plan-bounded (count recomputed every issue); RAM reservation can never
OOM or swap the reservoir; every deliberate park re-proves position; proof off-by-one prohibited;
NO mode switches (one read path; backout = git revert + previous binary).

## Tests (§10 — the reservoir/ranged/consumer/RAM/crash rows)
one-in-flight under three-thread scope; staged-intent cancel matrix; reservoir park/resume +
occupancy-overshoot ≤ one command; EVERY park→resume re-proof (incl. zero-duration) + resume-RP
attribution (off-by-one fixture FAILS); RAM concurrent-growth-race ≤ ceiling + rollback + refuse-
to-start; slow-alive parked-indefinitely-no-abort; dead/half-open-while-parked teardown drive-never-
moves; ranged proof-frontier + no-deadlock + proof-failure-discards; plan-bounded exact issue;
HandoffBlockSource validation parity; slow-consumer blocks/parks-never-spins; consumer-death poison
ordering; chaos kill rows (incl. kill-while-parked); construction assertion. **Scenario:** extend the
restore scenario's `covers` with `rem.tape.read_pipeline`; full `~/system` suite green from clean slate.

## Definition of done (AGENTS.md)
`cargo test --workspace` green, `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
socket tests run (sandbox limits reported, never `#[ignore]`d). Summary lists files touched, each
§10 row → test name, confirms one-in-flight + plan-bounded + park-re-proof + no-OOM/swap. Physical
acceptance = §10 legs 0–6 at the next MSL3040 window (target 300 MB/s at leg 1).

## R2a diff-gate follow-ups (fold into R2b — minor)
- **Ord-order guard (§5.6):** the terminal precedence relies on `enum ReadTerminalPriority`
  declaration order + derived `Ord`. Add a one-line comment (`// declaration order defines
  rank — ScsiRoot first`) AND a direct unit assert `assert!(ScsiRoot < Decode && Decode <
  Sender)` so a future variant reorder can't silently invert precedence with green types.
- **compile_fail explicitness (§3.3):** the `HandoffBlockSource` compile_fail doctest may also
  `use remanence_library::BlockSource;` and still fail — make the "decode source cannot reach a
  drive method" guarantee maximally explicit.
