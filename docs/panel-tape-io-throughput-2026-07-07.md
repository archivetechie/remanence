# Panel report — design-tape-io-throughput v0.1 → v0.2 (2026-07-07)

**Lenses (blind, parallel):** SCSI/SSC correctness (Opus), failure-modes/ops
(codex — independent model), cost/efficiency (Opus), general adversarial
(GLM 5.2 via OpenRouter, `ops/openrouter-review.sh`).
**Raw counts:** 2 blockers, 14 majors (11 unique after dedup), 9 minors,
4 nits. **Business-tagged questions for the owner: none** — all findings are
technical and were decided per `defer-technical-decide-business`.
**Disposition:** all accepted findings folded into
`design-tape-io-throughput-v0.1.md` (header bumped to v0.2) in one pass;
verify round follows.

## Blockers (both accepted)

1. **[SCSI] Read path never enters fixed-block mode.** `drive_prepare`'s
   MODE SELECT runs only on the write-owner path; the restore path opens the
   drive variable-mode, so FIXED=1 READ(6) batches would be rejected
   (ILLEGAL REQUEST) on every batched restore. Fold: read side gets a
   first-class mode-setup step, block size sourced from the tape's own
   bootstrap/catalog row, never a constant.
2. **[codex] Transport-unknown must not trigger follow-up data-path CDBs.**
   Draft said READ POSITION after "ANY non-clean outcome", contradicting the
   readiness design's completion-unknown rules. Fold: split decodable
   CHECK CONDITION/EW/EOM (drive responsive → RP is the arbiter) from
   transport-unknown (persist dirty fence, stop drive I/O, recovery path
   reopens and proves position).

## Majors (accepted; deduped)

3. **[SCSI+cost] The real per-command cap is the sg/HBA DMA limit, not Block
   Limits VPD** (which is per-block byte length — category error). Fold:
   `SG_SET_RESERVED_SIZE`/`max_sectors` handling, clamp batch to the
   achievable limit, log the effective batch at drive open (a silently
   clamped batch must be visible), deployment/runbook step to raise+verify.
4. **[SCSI] Deferred errors (sense response 0x71/0x73) must map to
   completion-unknown**, never to EW/residual accounting — buffered mode can
   surface batch K's destage failure on batch K+1. Durability is proven only
   by the blocking WRITE FILEMARKS at the layer-5 boundary; per-batch GOOD is
   not a media-commit guarantee. Current helpers (`is_fixed_format`,
   `write_eom_signal`) accept 0x71 — must be tightened.
5. **[SCSI] Residual source of truth pinned:** fixed-mode
   `records_transferred` comes from the sense INFORMATION field (records,
   VALID required, bounds-checked 0..=batch; out-of-range ⇒
   completion-unknown); for EW/EOM the post-event READ POSITION delta is the
   arbiter, not the residual. Never derive record counts from the SG_IO byte
   residual.
6. **[codex] Partial-batch ⇒ object uncommittable, explicitly:** any object
   data batch with `records_transferred != requested`, hard EOM, or
   undecodable residual writes no object-closing filemark and no journal
   record, and persists a tape fence.
7. **[codex] "Fence the tape" becomes a durable mechanism:** extend the
   existing quarantine store with a tape-I/O scope (keyed by tape UUID +
   barcode), enforced at selection, session open, and startup reconciliation,
   released only by explicit operator action. In-memory session poisoning is
   not admission control.
8. **[codex] Shared `PositionProof { lba, source }`** on every
   position-bearing outcome (write, filemark, locate/space); append-commit
   construction must be impossible from computed (unproven) positions —
   type-enforced, matching layer-5's post-filemark proof requirement.
9. **[codex] True backout mode:** `write_batch_blocks=1` is not today's
   path. Add an explicit legacy config that preserves variable-mode WRITE +
   per-block RP exactly as shipped, as the rollout/backout switch.
10. **[codex] L3 gets its own crash table** (kill after producer read; after
    batch write before cursor update; after tripwire mismatch before durable
    fence; after sink error with queued buffers; after filemark proof before
    journal) with chaos coverage for each row.
11. **[cost] Honest end-to-end framing + per-lever ladder:** transfer-phase
    75→~286 is ~1.6× end-to-end until spool and close are attacked
    (ladder: 75 → ~180 L1+L2 root-disk → ~223 +L3 → ~286 +tmpfs). Spool
    placement is co-primary, not tail-end; L1+L2 alone likely land under the
    ≥200 MB/s gate on root disk, so tmpfs (zero-code symlink or L4 config)
    must precede the physical checkpoint. A7 (spool elimination) and A1
    (lazy dismount; close ≈29% of post-fix wall) are the named next levers.
12. **[GLM] Filemarks consume one logical position** — the arithmetic
    tracker increments by 1 per filemark; `position_before` is pinned to the
    exact pre-batch LBA. (GLM's companion claim that 4 MiB batches cause
    shoe-shining was folded only as documentation: the drive's ~1 GiB buffer
    plus average-feed-above-floor is the anti-back-hitch mechanism, per the
    cost lens; default stays 16 with a bench sweep {8,16,32,64} next window.)

## Minors/nits (accepted)

- SILI=0 mandated on batch reads; interchange preconditions stated (every
  rem record is exactly block_size incl. zero-padded final body block; drive
  fixed block length must equal tape record length). [SCSI]
- Read filemark backstop specified: FILEMARK+VALID+INFORMATION residual
  decode is the load-bearing check for how many records were delivered. [SCSI]
- Position re-seed points enumerated: SPACE(EOD) before append, locate,
  space, rewind. [SCSI]
- RP long-form under buffered mode is position-consistency only, not
  durability — stated so nobody leans on the tripwire as a commit proof. [SCSI]
- Cross-version compatibility tests: old-code reads new-batch-written tapes
  and vice versa (stored-image based). [codex]
- tmpfs spool RAM budget: reconcile spool budget with free RAM when
  spool_dir is tmpfs; fail toward disk beyond the RAM budget. [cost]
- Fast spool documented as the *wear-safe* configuration (feed must exceed
  the ~100–112 MB/s LTO-9 speed-matching floor). [cost]
- §9 acceptance split by spool placement (root-disk ~234 vs tmpfs ~286
  expectations). [cost]
- Health-snapshot decoupling reworded — not a free win; same
  client-blocking-close question as IMMED=1 unload. [cost]
- Tripwire stays 1 GiB; clarified that a poisoned session invalidates the
  whole uncommitted prefix (forensic window ≠ restore-correctness window);
  optional wall-clock companion noted. [GLM cadence change rejected, cost]
- Concurrent spool creation already safe (UUID + create_new) — stated. [GLM]

## Rejected (with rationale)

- **GLM: WRITE(6) transfer length is 8-bit (255 blocks).** Factually wrong
  for SSC: the WRITE(6)/READ(6) SSC TRANSFER LENGTH field is 24-bit (the
  code's `MAX_SCSI_VARIABLE_WRITE_BYTES = 0x00FF_FFFF` reflects it). No
  WRITE(16) migration needed; 24-bit records ≈ 16M records per CDB.
- **GLM: default batch 64–128.** Ignores buffered mode; superseded by the
  sweep + documentation fold (finding 12).
- **GLM: tripwire 256 MiB.** Cost lens showed 1 GiB pays rent and the
  poisoned-prefix rule makes the window forensic-only; kept 1 GiB with the
  clarification.
